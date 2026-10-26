//! Jobs: a durable queue and ledger on top of topics.
//!
//! No dispatcher and no background workers: an interactive orchestrator claims READY
//! jobs and fans them out. Trust ceiling: one shared token and self-declared
//! identities, so `owns` is a lexical scheduling guard (not a filesystem permission or
//! boundary: paths are compared as text and symlinks are never resolved here),
//! `capability` is a routing label (only `live-infra` is refused), and heterogeneous
//! verification catches blind spots rather than enforcing a security boundary.
//! Upgrade path for all three: per-actor tokens.
//!
//! Concurrency: every route that reads-then-writes job state, or changes a job
//! topic's status, takes ONE global `pg_advisory_xact_lock` as its first statement
//! (create, claim, complete, verify, reclaim, cancel, retry, patch, amend, PATCH
//! topic, delete topic and the lifecycle sweep). Job write volume is tiny, and the
//! lock removes the overlapping-claim and half-cycle races without extra calls.
//! Lock order: advisory lock first, then row locks, everywhere.
//!
//! State x route table (anything not listed is 409):
//!
//! | state    | claim        | complete                | verify                       | reclaim | cancel | retry | amend | patch      |
//! |----------|--------------|-------------------------|------------------------------|---------|--------|-------|-------|------------|
//! | open     | -> claimed   |                         |                              |         | yes    |       | yes   | all fields |
//! | claimed  |              | -> done / failed        |                              | -> open | yes    |       | yes   | priority   |
//! | done     |              | idempotent repeat only  | pass -> verified, fail -> claimed (round+1) | -> open | yes |  | yes   | priority   |
//! | verified |              |                         |                              |         |        |       | yes   |            |
//! | failed   |              |                         |                              |         | yes    | -> open | yes |            |
//! | cancelled|              |                         |                              |         |        | -> open | yes |            |

use std::collections::{BTreeMap, HashMap, HashSet};

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, PgConnection};
use subtle::ConstantTimeEq;

use crate::config::JobsConfig;
use crate::db::{
    add_topic_event, clean_part, required_text, resolve_identity, resolve_topic_id,
    upsert_identity, valid_topic_name,
};
use crate::error::{ApiError, ApiResult};
use crate::extract::{ApiJson, ApiPath, ApiQuery};
use crate::messages::fetch_message;
use crate::models::{AgentIdent, Message, Ts};
use crate::redact::Redactions;
use crate::state::AppState;
use crate::util::{clamp_limit, sha256_hex, truncate_chars};

/// The one global advisory-lock key for all job state ("HIVEJOBS" in ASCII).
pub const JOBS_LOCK_KEY: i64 = 0x4849_5645_4a4f_4253;

const MAX_SPEC_CHARS: usize = 100_000;
const MAX_BATCH: usize = 50;
const STATUSES: [&str; 6] = ["open", "claimed", "done", "verified", "failed", "cancelled"];
const LOCKFILES: [&str; 4] = [
    "Cargo.lock",
    "package-lock.json",
    "pnpm-lock.yaml",
    "yarn.lock",
];

/// Take the global jobs lock. Must be the first statement of the transaction.
pub async fn lock_jobs(conn: &mut PgConnection) -> ApiResult<()> {
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(JOBS_LOCK_KEY)
        .execute(conn)
        .await?;
    Ok(())
}

// ---------------------------------------------------------------- owns

/// Normalise and validate one `owns` entry. Rules: absolute; `//`, `/./` and trailing `/`
/// collapse; `..`, `~` and globs are rejected; a component equal to `.git` is rejected; a
/// last component equal to a lockfile name is rejected; the path must lie STRICTLY BELOW one
/// of the configured roots (so a root itself cannot be owned). With no roots configured,
/// every entry is rejected (fail closed).
///
/// LEXICAL ONLY: the server cannot see the workers' filesystem, so it never resolves symlinks.
/// Containment and overlap (`owns_overlap`) compare path text, which makes `owns` a scheduling
/// guard, not a filesystem boundary: a symlink can alias two entries or point outside a root.
/// `hive-mcp` canonicalises existing ancestors on the posting machine before it sends an entry.
pub fn normalise_owns(raw: &str, roots: &[String]) -> Result<String, String> {
    let raw = raw.trim();
    if roots.is_empty() {
        return Err(
            "this server accepts no owns entries: set HIVE_JOB_OWNS_ROOTS to the \
                    directories agents may claim work under"
                .into(),
        );
    }
    if raw.chars().count() > 500 {
        return Err("owns entries are limited to 500 characters".into());
    }
    if !raw.starts_with('/') {
        return Err(format!("owns entry '{raw}' must be an absolute path"));
    }
    if raw.contains('\0') || raw.contains('\\') {
        return Err("owns entries must not contain NUL or backslash".into());
    }
    let mut comps: Vec<&str> = Vec::new();
    for c in raw.split('/') {
        if c.is_empty() || c == "." {
            continue;
        }
        if c == ".." {
            return Err(format!("owns entry '{raw}' must not contain '..'"));
        }
        if c.starts_with('~') {
            return Err(format!("owns entry '{raw}' must not use '~'"));
        }
        if c.contains(['*', '?', '[', ']', '{', '}']) {
            return Err(format!("owns entry '{raw}' must not contain globs"));
        }
        if c == ".git" {
            return Err("owns entries must not include .git (the coordinator owns it)".into());
        }
        comps.push(c);
    }
    let Some(last) = comps.last() else {
        return Err("owns entry must name a path below an allowed root".into());
    };
    if LOCKFILES.contains(last) {
        return Err(format!(
            "owns entries must not be lockfiles ({last}); the coordinator owns shared files"
        ));
    }
    let below_root = roots.iter().any(|r| {
        let rc: Vec<&str> = r.split('/').filter(|c| !c.is_empty()).collect();
        comps.len() > rc.len() && comps.iter().zip(rc.iter()).all(|(a, b)| a == b)
    });
    if !below_root {
        return Err(format!(
            "owns entry '{raw}' must lie strictly below one of: {}",
            roots.join(", ")
        ));
    }
    Ok(format!("/{}", comps.join("/")))
}

fn normalise_owns_list(items: Vec<String>, cfg: &JobsConfig) -> ApiResult<Vec<String>> {
    if items.len() > 64 {
        return Err(ApiError::validation("at most 64 owns entries per job"));
    }
    let mut out: Vec<String> = Vec::new();
    for raw in items {
        let n = normalise_owns(&raw, &cfg.owns_roots).map_err(ApiError::Validation)?;
        if !out.contains(&n) {
            out.push(n);
        }
    }
    Ok(out)
}

/// Whole-component overlap: equal, or one is an ancestor of the other.
pub fn owns_overlap(a: &str, b: &str) -> bool {
    let ca: Vec<&str> = a.split('/').filter(|c| !c.is_empty()).collect();
    let cb: Vec<&str> = b.split('/').filter(|c| !c.is_empty()).collect();
    let n = ca.len().min(cb.len());
    ca[..n] == cb[..n]
}

fn any_overlap(a: &[String], b: &[String]) -> bool {
    a.iter().any(|x| b.iter().any(|y| owns_overlap(x, y)))
}

// ---------------------------------------------------------------- graph

struct GNode {
    id: i64,
    name: String,
    status: String,
    security: bool,
    priority: i32,
    owns: Vec<String>,
}

/// Every job and dependency edge, loaded whole under the global lock.
/// CEILING: fine for tens to a few hundred jobs (swarms of ~14); it is a full table read per
/// call, so thousands of jobs would want an indexed per-job query. Upgrade path: query only
/// the open/claimed/done jobs and the edges of the candidates.
struct Graph {
    nodes: BTreeMap<i64, GNode>,
    deps: HashMap<i64, Vec<i64>>,
}

async fn load_graph(conn: &mut PgConnection) -> Result<Graph, sqlx::Error> {
    let rows: Vec<(i64, String, String, bool, i32, Vec<String>)> = sqlx::query_as(
        "SELECT j.topic_id, t.name, j.status, j.security, j.priority, j.owns \
           FROM jobs j JOIN topics t ON t.id = j.topic_id",
    )
    .fetch_all(&mut *conn)
    .await?;
    let nodes = rows
        .into_iter()
        .map(|(id, name, status, security, priority, owns)| {
            (
                id,
                GNode {
                    id,
                    name,
                    status,
                    security,
                    priority,
                    owns,
                },
            )
        })
        .collect();
    let edges: Vec<(i64, i64)> = sqlx::query_as("SELECT job_id, depends_on FROM job_deps")
        .fetch_all(&mut *conn)
        .await?;
    let mut deps: HashMap<i64, Vec<i64>> = HashMap::new();
    for (j, d) in edges {
        deps.entry(j).or_default().push(d);
    }
    for v in deps.values_mut() {
        v.sort_unstable();
    }
    Ok(Graph { nodes, deps })
}

#[derive(Default)]
struct Analysis {
    ready: bool,
    blocked: bool,
    blocked_reason: Option<String>,
    blocked_by: Vec<i64>,
    conflicts_with: Vec<i64>,
}

fn dep_satisfied(n: &GNode) -> bool {
    n.status == "verified" || (n.status == "done" && !n.security)
}

/// READY = open AND every dependency done/verified (a `security` dependency must be
/// verified) AND no owns overlap with a claimed OR done-but-unverified job.
/// BLOCKED = a dependency is failed or cancelled.
fn analyse(g: &Graph, id: i64) -> Analysis {
    let Some(me) = g.nodes.get(&id) else {
        return Analysis::default();
    };
    if me.status != "open" {
        return Analysis::default();
    }
    let mut a = Analysis::default();
    for d in g.deps.get(&id).map(Vec::as_slice).unwrap_or(&[]) {
        match g.nodes.get(d) {
            Some(dn) if dep_satisfied(dn) => {}
            Some(dn) => {
                a.blocked_by.push(*d);
                if matches!(dn.status.as_str(), "failed" | "cancelled") && !a.blocked {
                    a.blocked = true;
                    a.blocked_reason = Some(format!("dependency '{}' is {}", dn.name, dn.status));
                }
            }
            None => a.blocked_by.push(*d),
        }
    }
    for other in g.nodes.values() {
        if other.id != id
            && matches!(other.status.as_str(), "claimed" | "done")
            && any_overlap(&me.owns, &other.owns)
        {
            a.conflicts_with.push(other.id);
        }
    }
    a.ready = a.blocked_by.is_empty() && !a.blocked && a.conflicts_with.is_empty();
    a
}

/// Kahn's algorithm over job -> dependency edges.
fn has_cycle(g: &Graph) -> bool {
    let mut indeg: HashMap<i64, usize> = g.nodes.keys().map(|k| (*k, 0)).collect();
    let mut rev: HashMap<i64, Vec<i64>> = HashMap::new();
    for (j, ds) in &g.deps {
        for d in ds {
            *indeg.entry(*j).or_insert(0) += 1;
            rev.entry(*d).or_default().push(*j);
        }
    }
    let mut stack: Vec<i64> = indeg
        .iter()
        .filter(|(_, n)| **n == 0)
        .map(|(k, _)| *k)
        .collect();
    let mut seen = 0usize;
    while let Some(n) = stack.pop() {
        seen += 1;
        for m in rev.get(&n).map(Vec::as_slice).unwrap_or(&[]) {
            if let Some(c) = indeg.get_mut(m) {
                *c -= 1;
                if *c == 0 {
                    stack.push(*m);
                }
            }
        }
    }
    seen != indeg.len()
}

// ---------------------------------------------------------------- rows and views

#[derive(Debug, Clone, Serialize, FromRow)]
pub struct Job {
    pub id: i64,
    pub topic_id: i64,
    pub topic_name: String,
    pub title: String,
    pub spec: String,
    pub priority: i32,
    pub capability: String,
    pub security: bool,
    pub task_kind: Option<String>,
    pub owns: Vec<String>,
    pub tier: Option<String>,
    pub agent_type: Option<String>,
    pub status: String,
    pub claimed_by: Option<String>,
    pub claimed_at: Option<Ts>,
    pub round: i32,
    pub verify_fails: i32,
    pub worker_identity: Option<serde_json::Value>,
    pub result_message_id: Option<i64>,
    pub verdict_message_id: Option<i64>,
    pub completed_at: Option<Ts>,
    pub verified_at: Option<Ts>,
    pub ready_at: Ts,
    pub attempts: i32,
    pub created_by: String,
    pub created_at: Ts,
    pub updated_at: Ts,
    pub claim_age_s: Option<f64>,
    pub stale: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct JobView {
    #[serde(flatten)]
    pub job: Job,
    pub depends_on: Vec<i64>,
    pub ready: bool,
    pub blocked: bool,
    pub blocked_reason: Option<String>,
    pub blocked_by: Vec<i64>,
    pub conflicts_with: Vec<i64>,
}

/// Job columns. Deliberately no nonce and no nonce hash: neither is ever readable.
fn job_cols(stale_h: i64) -> String {
    let h = stale_h.clamp(1, 8760);
    format!(
        "j.topic_id AS id, j.topic_id, t.name AS topic_name, t.title, j.spec, j.priority, \
         j.capability, j.security, j.task_kind, j.owns, j.tier, j.agent_type, j.status, \
         j.claimed_by, j.claimed_at, j.round, j.verify_fails, \
         (SELECT jsonb_build_object('agent_type', i.agent_type, 'model', i.model, 'harness', i.harness) \
            FROM identities i WHERE i.id = j.worker_identity_id) AS worker_identity, \
         j.result_message_id, j.verdict_message_id, j.completed_at, j.verified_at, j.ready_at, \
         j.attempts, j.created_by, j.created_at, j.updated_at, \
         CASE WHEN j.status = 'claimed' THEN extract(epoch FROM now() - j.claimed_at)::float8 END AS claim_age_s, \
         coalesce((j.status = 'claimed' AND j.claimed_at < now() - make_interval(hours => {h}) \
                   AND t.last_activity_at < now() - interval '1 hour') \
               OR (j.status = 'done' AND j.completed_at < now() - make_interval(hours => {h})), \
                  false) AS stale"
    )
}

const JOB_FROM: &str = "jobs j JOIN topics t ON t.id = j.topic_id";

async fn fetch_job(conn: &mut PgConnection, cfg: &JobsConfig, id: i64) -> ApiResult<Job> {
    let sql = format!(
        "SELECT {} FROM {JOB_FROM} WHERE j.topic_id = $1",
        job_cols(cfg.stale_h)
    );
    sqlx::query_as::<_, Job>(&sql)
        .bind(id)
        .fetch_optional(conn)
        .await?
        .ok_or_else(|| ApiError::not_found("job not found"))
}

fn make_view(g: &Graph, job: Job) -> JobView {
    let a = analyse(g, job.id);
    JobView {
        depends_on: g.deps.get(&job.id).cloned().unwrap_or_default(),
        ready: a.ready,
        blocked: a.blocked,
        blocked_reason: a.blocked_reason,
        blocked_by: a.blocked_by,
        conflicts_with: a.conflicts_with,
        job,
    }
}

async fn view_of(conn: &mut PgConnection, cfg: &JobsConfig, id: i64) -> ApiResult<JobView> {
    let job = fetch_job(conn, cfg, id).await?;
    let g = load_graph(conn).await?;
    Ok(make_view(&g, job))
}

/// Resolve `{id}` (topic slug or numeric id) to a job's topic id.
async fn job_id(conn: &mut PgConnection, t: &str) -> ApiResult<i64> {
    let id = resolve_topic_id(conn, t).await?;
    let exists: Option<i64> = sqlx::query_scalar("SELECT topic_id FROM jobs WHERE topic_id = $1")
        .bind(id)
        .fetch_optional(conn)
        .await?;
    exists.ok_or_else(|| ApiError::not_found("not a job topic"))
}

#[derive(FromRow)]
struct Core {
    status: String,
    round: i32,
    claim_nonce_hash: Option<String>,
    worker_identity_id: Option<i64>,
    security: bool,
    attempts: i32,
    verify_fails: i32,
}

async fn core(conn: &mut PgConnection, id: i64) -> ApiResult<Core> {
    sqlx::query_as::<_, Core>(
        "SELECT status, round, claim_nonce_hash, worker_identity_id, security, attempts, verify_fails \
           FROM jobs WHERE topic_id = $1 FOR UPDATE",
    )
    .bind(id)
    .fetch_optional(conn)
    .await?
    .ok_or_else(|| ApiError::not_found("job not found"))
}

/// A worker or verifier identity must be real: a blank or "unknown" field would collapse
/// to `unknown/unknown/unknown`, which trivially "differs" from any worker and defeats the
/// heterogeneous-verification rule.
fn strict_identity(who: &str, a: &AgentIdent) -> ApiResult<()> {
    for (field, v) in [
        ("agent_type", &a.agent_type),
        ("model", &a.model),
        ("harness", &a.harness),
    ] {
        let t = v.trim();
        if t.is_empty() || t.eq_ignore_ascii_case("unknown") {
            return Err(ApiError::validation(format!(
                "{who}.{field} must be a real value (not blank or \"unknown\")"
            )));
        }
    }
    Ok(())
}

fn state_conflict(status: &str, route: &str) -> ApiError {
    ApiError::conflict(format!(
        "job is {status}; {route} is not allowed in that state"
    ))
}

fn nonce_matches(hash: &Option<String>, nonce: &Option<String>) -> bool {
    match (hash, nonce) {
        (Some(h), Some(n)) => bool::from(sha256_hex(n.trim()).as_bytes().ct_eq(h.as_bytes())),
        _ => false,
    }
}

async fn touch_topic(conn: &mut PgConnection, tid: i64) -> ApiResult<()> {
    sqlx::query("UPDATE topics SET last_activity_at = now(), updated_at = now() WHERE id = $1")
        .bind(tid)
        .execute(conn)
        .await?;
    Ok(())
}

/// `retry` and `reclaim` reopen a resolved/archived/quiet job topic to `active`.
async fn reopen_topic(conn: &mut PgConnection, tid: i64, actor: &str) -> ApiResult<()> {
    let from: String = sqlx::query_scalar("SELECT status FROM topics WHERE id = $1 FOR UPDATE")
        .bind(tid)
        .fetch_one(&mut *conn)
        .await?;
    if matches!(from.as_str(), "quiet" | "resolved" | "archived") {
        sqlx::query(
            "UPDATE topics SET status = 'active', outcome = NULL, last_activity_at = now(), \
               updated_at = now() WHERE id = $1",
        )
        .bind(tid)
        .execute(&mut *conn)
        .await?;
        add_topic_event(
            conn,
            tid,
            "reopened",
            actor,
            serde_json::json!({"from": from, "by": "job"}),
        )
        .await
    } else {
        touch_topic(conn, tid).await
    }
}

/// Result, amendment and verdict messages. A `verdict` also creates the finding row
/// (as any typed post does) carrying `about_identity_id`, the identity being judged.
async fn post_job_message(
    conn: &mut PgConnection,
    tid: i64,
    author: &str,
    identity_id: Option<i64>,
    kind: &str,
    content: &str,
    about_identity_id: Option<i64>,
) -> ApiResult<i64> {
    let mid: i64 = sqlx::query_scalar(
        "INSERT INTO messages (topic_id, author, identity_id, content, kind) \
         VALUES ($1,$2,$3,$4,$5) RETURNING id",
    )
    .bind(tid)
    .bind(author)
    .bind(identity_id)
    .bind(content)
    .bind(kind)
    .fetch_one(&mut *conn)
    .await?;
    if kind == "verdict" {
        sqlx::query(
            "INSERT INTO findings (topic_id, message_id, identity_id, kind, claim, provenance, \
               content_hash, about_identity_id) \
             VALUES ($1,$2,$3,'verdict',$4,'agent',$5,$6) \
             ON CONFLICT (topic_id, content_hash) DO UPDATE \
               SET reinforce_count = findings.reinforce_count + 1, updated_at = now()",
        )
        .bind(tid)
        .bind(mid)
        .bind(identity_id)
        .bind(content)
        .bind(crate::util::content_hash(content))
        .bind(about_identity_id)
        .execute(&mut *conn)
        .await?;
    }
    Ok(mid)
}

async fn identity_triple(conn: &mut PgConnection, id: i64) -> ApiResult<(String, String, String)> {
    Ok(
        sqlx::query_as("SELECT agent_type, model, harness FROM identities WHERE id = $1")
            .bind(id)
            .fetch_one(conn)
            .await?,
    )
}

fn idem_header(headers: &HeaderMap) -> ApiResult<Option<String>> {
    match headers.get("idempotency-key") {
        None => Ok(None),
        Some(v) => {
            let s = v
                .to_str()
                .map_err(|_| ApiError::validation("invalid Idempotency-Key"))?
                .trim()
                .to_string();
            if s.is_empty() || s.chars().count() > 200 {
                return Err(ApiError::validation(
                    "Idempotency-Key must be 1-200 characters",
                ));
            }
            Ok(Some(s))
        }
    }
}

async fn idem_get(conn: &mut PgConnection, key: &str) -> ApiResult<Option<Vec<i64>>> {
    let v: Option<serde_json::Value> = sqlx::query_scalar("SELECT v FROM kv WHERE k = $1")
        .bind(key)
        .fetch_optional(conn)
        .await?;
    Ok(v.and_then(|v| serde_json::from_value::<Vec<i64>>(v).ok()))
}

async fn idem_put(conn: &mut PgConnection, key: &str, ids: &[i64]) -> ApiResult<()> {
    sqlx::query(
        "INSERT INTO kv (k, v) VALUES ($1,$2) \
         ON CONFLICT (k) DO UPDATE SET v = EXCLUDED.v, updated_at = now()",
    )
    .bind(key)
    .bind(serde_json::json!(ids))
    .execute(conn)
    .await?;
    Ok(())
}

// ---------------------------------------------------------------- create

#[derive(Deserialize)]
pub struct NewJob {
    name: String,
    title: String,
    spec: String,
    priority: Option<i32>,
    capability: String,
    security: Option<bool>,
    task_kind: Option<String>,
    owns: Option<Vec<String>>,
    depends_on: Option<Vec<String>>,
    tier: Option<String>,
    agent_type: Option<String>,
}

#[derive(Deserialize)]
pub struct CreatePayload {
    jobs: Vec<NewJob>,
    author: String,
    agent: Option<AgentIdent>,
}

fn check_tier(t: &Option<String>) -> ApiResult<()> {
    if let Some(t) = t {
        if !["fast", "standard", "deep"].contains(&t.as_str()) {
            return Err(ApiError::validation("tier must be fast, standard or deep"));
        }
    }
    Ok(())
}

pub async fn create(
    State(st): State<AppState>,
    headers: HeaderMap,
    ApiJson(p): ApiJson<CreatePayload>,
) -> ApiResult<(StatusCode, Json<Vec<JobView>>)> {
    if p.jobs.is_empty() || p.jobs.len() > MAX_BATCH {
        return Err(ApiError::validation(format!(
            "jobs must contain 1 to {MAX_BATCH} entries"
        )));
    }
    let idem = idem_header(&headers)?;
    let author = required_text("author", &p.author, 120)?;
    let mut red = Redactions::new();
    let author = red.text("author", author);
    let agent = red.agent(p.agent);

    // Validate everything that needs no database first.
    struct Clean {
        name: String,
        title: String,
        spec: String,
        priority: i32,
        capability: String,
        security: bool,
        task_kind: Option<String>,
        owns: Vec<String>,
        deps: Vec<String>,
        tier: Option<String>,
        agent_type: Option<String>,
    }
    let mut cleaned: Vec<Clean> = Vec::new();
    let mut names: HashSet<String> = HashSet::new();
    for j in p.jobs {
        let name = j.name.trim().to_string();
        if !valid_topic_name(&name) {
            return Err(ApiError::validation(
                "job name must match ^[a-z][a-z0-9-]{1,79}$",
            ));
        }
        if crate::db::reserved_topic_name(&name) {
            return Err(ApiError::validation(format!(
                "the name '{name}' is reserved"
            )));
        }
        if !names.insert(name.clone()) {
            return Err(ApiError::validation(format!(
                "duplicate job name '{name}' in the batch"
            )));
        }
        let title = red.text("title", required_text("title", &j.title, 300)?);
        let spec = red.text("spec", required_text("spec", &j.spec, MAX_SPEC_CHARS)?);
        match j.capability.as_str() {
            "read-only" | "local-build" => {}
            "live-infra" => return Err(ApiError::validation(
                "live-infra jobs are not supported: they cannot be created, claimed or completed",
            )),
            _ => {
                return Err(ApiError::validation(
                    "capability must be read-only or local-build",
                ))
            }
        }
        check_tier(&j.tier)?;
        let owns = normalise_owns_list(j.owns.unwrap_or_default(), &st.jobs)?;
        if j.capability == "local-build" && owns.is_empty() {
            return Err(ApiError::validation(
                "local-build jobs must declare a non-empty owns",
            ));
        }
        let priority = j.priority.unwrap_or(100);
        if priority.abs() > 1_000_000 {
            return Err(ApiError::validation("priority must be within +/-1000000"));
        }
        let task_kind = red
            .opt("task_kind", j.task_kind)
            .map(|k| truncate_chars(k.trim(), 80))
            .filter(|k| !k.is_empty());
        let agent_type = red
            .opt("agent_type", j.agent_type)
            .map(|k| truncate_chars(k.trim(), 80))
            .filter(|k| !k.is_empty());
        let mut deps = Vec::new();
        for d in j.depends_on.unwrap_or_default() {
            let d = d.trim().to_string();
            if d.is_empty() {
                return Err(ApiError::validation("depends_on entries must not be empty"));
            }
            if d == name {
                return Err(ApiError::validation("a job cannot depend on itself"));
            }
            if !deps.contains(&d) {
                deps.push(d);
            }
        }
        cleaned.push(Clean {
            name,
            title,
            spec,
            priority,
            capability: j.capability,
            security: j.security.unwrap_or(false),
            task_kind,
            owns,
            deps,
            tier: j.tier,
            agent_type,
        });
    }

    let mut tx = st.pool.begin().await?;
    lock_jobs(&mut tx).await?;

    let idem_key = idem.map(|k| format!("idem:jobs-create:{k}"));
    if let Some(k) = &idem_key {
        if let Some(ids) = idem_get(&mut tx, k).await? {
            let g = load_graph(&mut tx).await?;
            let mut out = Vec::new();
            for id in ids {
                let job = fetch_job(&mut tx, &st.jobs, id).await?;
                out.push(make_view(&g, job));
            }
            return Ok((StatusCode::OK, Json(out)));
        }
    }

    // Insert topics and job rows, remembering ids by name.
    let mut ids: HashMap<String, i64> = HashMap::new();
    let mut order: Vec<i64> = Vec::new();
    for c in &cleaned {
        let exists: Option<i64> = sqlx::query_scalar("SELECT id FROM topics WHERE name = $1")
            .bind(&c.name)
            .fetch_optional(&mut *tx)
            .await?;
        if exists.is_some() {
            return Err(ApiError::conflict(format!(
                "topic '{}' already exists",
                c.name
            )));
        }
        let tid: i64 = sqlx::query_scalar(
            "INSERT INTO topics (name, title, description, created_by, kind) \
             VALUES ($1,$2,$3,$4,'job') RETURNING id",
        )
        .bind(&c.name)
        .bind(&c.title)
        .bind(truncate_chars(&c.spec, 500))
        .bind(&author)
        .fetch_one(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO jobs (topic_id, spec, priority, capability, security, task_kind, owns, \
               tier, agent_type, created_by) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)",
        )
        .bind(tid)
        .bind(&c.spec)
        .bind(c.priority)
        .bind(&c.capability)
        .bind(c.security)
        .bind(&c.task_kind)
        .bind(&c.owns)
        .bind(&c.tier)
        .bind(&c.agent_type)
        .bind(&author)
        .execute(&mut *tx)
        .await?;
        resolve_identity(&mut tx, &author, agent.as_ref(), Some(tid)).await?;
        add_topic_event(
            &mut tx,
            tid,
            "created",
            &author,
            serde_json::json!({"kind": "job"}),
        )
        .await?;
        add_topic_event(
            &mut tx,
            tid,
            "job-created",
            &author,
            serde_json::json!({"priority": c.priority, "capability": c.capability,
                               "security": c.security, "owns": c.owns}),
        )
        .await?;
        red.record(&mut tx, Some(tid), &author).await?;
        ids.insert(c.name.clone(), tid);
        order.push(tid);
    }
    // Dependencies: in-batch names first, then existing jobs by slug or id.
    for c in &cleaned {
        let Some(&me) = ids.get(&c.name) else {
            continue;
        };
        for d in &c.deps {
            let dep = match ids.get(d) {
                Some(&id) => id,
                None => job_id(&mut tx, d).await.map_err(|e| match e {
                    ApiError::NotFound(_) => {
                        ApiError::validation(format!("unknown dependency '{d}'"))
                    }
                    other => other,
                })?,
            };
            sqlx::query("INSERT INTO job_deps (job_id, depends_on) VALUES ($1,$2)")
                .bind(me)
                .bind(dep)
                .execute(&mut *tx)
                .await?;
        }
    }
    let g = load_graph(&mut tx).await?;
    if has_cycle(&g) {
        return Err(ApiError::conflict("dependency cycle"));
    }
    if let Some(k) = &idem_key {
        idem_put(&mut tx, k, &order).await?;
    }
    let mut out = Vec::new();
    for id in &order {
        let job = fetch_job(&mut tx, &st.jobs, *id).await?;
        out.push(make_view(&g, job));
    }
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(out)))
}

// ---------------------------------------------------------------- reads

#[derive(Deserialize)]
pub struct ListQuery {
    status: Option<String>,
    ready: Option<bool>,
    blocked: Option<bool>,
    claimed_by: Option<String>,
    job: Option<String>,
    limit: Option<i64>,
}

pub async fn list(
    State(st): State<AppState>,
    ApiQuery(q): ApiQuery<ListQuery>,
) -> ApiResult<Json<Vec<JobView>>> {
    if let Some(s) = &q.status {
        if !STATUSES.contains(&s.as_str()) {
            return Err(ApiError::validation("unknown job status"));
        }
    }
    let mut conn = st.pool.acquire().await?;
    let only = match &q.job {
        Some(t) => Some(job_id(&mut conn, t).await?),
        None => None,
    };
    let sql = format!(
        "SELECT {} FROM {JOB_FROM} \
          WHERE ($1::text IS NULL OR j.status = $1) AND ($2::text IS NULL OR j.claimed_by = $2) \
            AND ($3::bigint IS NULL OR j.topic_id = $3) \
          ORDER BY j.priority ASC, j.topic_id ASC",
        job_cols(st.jobs.stale_h)
    );
    let jobs = sqlx::query_as::<_, Job>(&sql)
        .bind(&q.status)
        .bind(&q.claimed_by)
        .bind(only)
        .fetch_all(&mut *conn)
        .await?;
    let g = load_graph(&mut conn).await?;
    let limit = clamp_limit(q.limit, 100, 500) as usize;
    let out: Vec<JobView> = jobs
        .into_iter()
        .map(|j| make_view(&g, j))
        .filter(|v| q.ready.is_none_or(|r| v.ready == r))
        .filter(|v| q.blocked.is_none_or(|b| v.blocked == b))
        .take(limit)
        .collect();
    Ok(Json(out))
}

pub async fn get(
    State(st): State<AppState>,
    ApiPath(t): ApiPath<String>,
) -> ApiResult<Json<JobView>> {
    let mut conn = st.pool.acquire().await?;
    let id = job_id(&mut conn, &t).await?;
    Ok(Json(view_of(&mut conn, &st.jobs, id).await?))
}

// ---------------------------------------------------------------- claim

#[derive(Deserialize)]
pub struct ClaimPayload {
    claimant: String,
    agent: Option<AgentIdent>,
    limit: Option<i64>,
    force_overlap: Option<bool>,
}

#[derive(Serialize)]
pub struct ClaimedJob {
    job: JobView,
    /// Returned exactly once, here. Only its sha256 is stored. `null` on an idempotent replay.
    nonce: Option<String>,
    replayed: bool,
}

pub async fn claim(
    State(st): State<AppState>,
    headers: HeaderMap,
    ApiJson(p): ApiJson<ClaimPayload>,
) -> ApiResult<Json<Vec<ClaimedJob>>> {
    let idem = idem_header(&headers)?;
    let claimant = required_text("claimant", &p.claimant, 120)?;
    let limit = clamp_limit(p.limit, 1, 10) as usize;
    let force = p.force_overlap.unwrap_or(false);
    let mut red = Redactions::new();
    let claimant = red.text("claimant", claimant);
    let agent = red.agent(p.agent);

    let mut tx = st.pool.begin().await?;
    lock_jobs(&mut tx).await?;
    let idem_key = idem.map(|k| format!("idem:jobs-claim:{k}"));
    if let Some(k) = &idem_key {
        if let Some(ids) = idem_get(&mut tx, k).await? {
            // The nonce was returned once and is not stored: a replay cannot show it again.
            let g = load_graph(&mut tx).await?;
            let mut out = Vec::new();
            for id in ids {
                let job = fetch_job(&mut tx, &st.jobs, id).await?;
                out.push(ClaimedJob {
                    job: make_view(&g, job),
                    nonce: None,
                    replayed: true,
                });
            }
            return Ok(Json(out));
        }
    }

    let mut g = load_graph(&mut tx).await?;
    let mut candidates: Vec<(i32, i64)> = g
        .nodes
        .values()
        .filter(|n| n.status == "open")
        .map(|n| (n.priority, n.id))
        .collect();
    candidates.sort_unstable();

    let mut picked: Vec<(i64, Vec<i64>)> = Vec::new();
    for (_, id) in candidates {
        if picked.len() >= limit {
            break;
        }
        let a = analyse(&g, id);
        if a.blocked || !a.blocked_by.is_empty() {
            continue;
        }
        if !a.conflicts_with.is_empty() && !force {
            continue;
        }
        // Later candidates must see this one as claimed (in-batch overlap re-check).
        if let Some(n) = g.nodes.get_mut(&id) {
            n.status = "claimed".into();
        }
        picked.push((id, a.conflicts_with));
    }

    resolve_identity(&mut tx, &claimant, agent.as_ref(), None).await?;
    let mut out = Vec::new();
    let mut claimed_ids = Vec::new();
    for (id, forced) in &picked {
        let nonce: String = sqlx::query_scalar(
            "SELECT replace(gen_random_uuid()::text || gen_random_uuid()::text, '-', '')",
        )
        .fetch_one(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE jobs SET status = 'claimed', claimed_by = $2, claimed_at = now(), \
               claim_nonce_hash = $3, round = 1, attempts = attempts + 1, updated_at = now() \
             WHERE topic_id = $1",
        )
        .bind(id)
        .bind(&claimant)
        .bind(sha256_hex(&nonce))
        .execute(&mut *tx)
        .await?;
        add_topic_event(
            &mut tx,
            *id,
            "job-claimed",
            &claimant,
            serde_json::json!({"round": 1}),
        )
        .await?;
        for other in forced {
            add_topic_event(
                &mut tx,
                *id,
                "owns-overlap-forced",
                &claimant,
                serde_json::json!({"job_id": id, "conflicts_with_job_id": other}),
            )
            .await?;
        }
        touch_topic(&mut tx, *id).await?;
        red.record(&mut tx, Some(*id), &claimant).await?;
        claimed_ids.push(*id);
        let job = fetch_job(&mut tx, &st.jobs, *id).await?;
        out.push((job, nonce));
    }
    if let Some(k) = &idem_key {
        idem_put(&mut tx, k, &claimed_ids).await?;
    }
    tx.commit().await?;
    Ok(Json(
        out.into_iter()
            .map(|(job, nonce)| ClaimedJob {
                job: make_view(&g, job),
                nonce: Some(nonce),
                replayed: false,
            })
            .collect(),
    ))
}

// ---------------------------------------------------------------- complete

#[derive(Deserialize)]
pub struct CompletePayload {
    nonce: Option<String>,
    round: Option<i32>,
    status: String,
    worker: Option<AgentIdent>,
    result: String,
    author: String,
}

pub async fn complete(
    State(st): State<AppState>,
    ApiPath(t): ApiPath<String>,
    ApiJson(p): ApiJson<CompletePayload>,
) -> ApiResult<Json<JobView>> {
    if !["done", "failed"].contains(&p.status.as_str()) {
        return Err(ApiError::validation("status must be done or failed"));
    }
    let author = required_text("author", &p.author, 120)?;
    let result = required_text("result", &p.result, MAX_SPEC_CHARS)?;
    let Some(worker) = p.worker else {
        return Err(ApiError::validation("worker identity is required"));
    };
    strict_identity("worker", &worker)?;
    let mut red = Redactions::new();
    let author = red.text("author", author);
    let result = red.text("result", result);
    let worker = red.agent(Some(worker));

    let mut tx = st.pool.begin().await?;
    lock_jobs(&mut tx).await?;
    let id = job_id(&mut tx, &t).await?;
    let c = core(&mut tx, id).await?;

    match c.status.as_str() {
        "claimed" => {
            if !nonce_matches(&c.claim_nonce_hash, &p.nonce) {
                return Err(ApiError::conflict("stale or missing claim nonce"));
            }
            if p.round != Some(c.round) {
                return Err(ApiError::conflict(format!(
                    "stale round: the job is at round {}",
                    c.round
                )));
            }
        }
        "done" | "failed" => {
            // Idempotent repeat only: same nonce, same round, same status.
            if nonce_matches(&c.claim_nonce_hash, &p.nonce)
                && p.round == Some(c.round)
                && p.status == c.status
            {
                return Ok(Json(view_of(&mut tx, &st.jobs, id).await?));
            }
            return Err(ApiError::conflict(format!(
                "job is already {}; only an identical repeat is accepted",
                c.status
            )));
        }
        other => return Err(state_conflict(other, "complete")),
    }

    let w = worker.unwrap_or(AgentIdent {
        agent_type: "unknown".into(),
        model: "unknown".into(),
        harness: "unknown".into(),
    });
    let worker_id = upsert_identity(
        &mut tx,
        &clean_part(&w.agent_type),
        &clean_part(&w.model),
        &clean_part(&w.harness),
    )
    .await?;
    let mid = post_job_message(
        &mut tx,
        id,
        &author,
        Some(worker_id),
        "result",
        &result,
        None,
    )
    .await?;
    sqlx::query(
        "UPDATE jobs SET status = $2, worker_identity_id = $3, result_message_id = $4, \
           completed_at = now(), updated_at = now() WHERE topic_id = $1",
    )
    .bind(id)
    .bind(&p.status)
    .bind(worker_id)
    .bind(mid)
    .execute(&mut *tx)
    .await?;
    add_topic_event(
        &mut tx,
        id,
        if p.status == "done" {
            "job-done"
        } else {
            "job-failed"
        },
        &author,
        serde_json::json!({"round": c.round, "worker_identity_id": worker_id}),
    )
    .await?;
    touch_topic(&mut tx, id).await?;
    red.record(&mut tx, Some(id), &author).await?;
    let v = view_of(&mut tx, &st.jobs, id).await?;
    tx.commit().await?;
    Ok(Json(v))
}

// ---------------------------------------------------------------- verify

#[derive(Deserialize)]
pub struct VerifyPayload {
    verdict: String,
    note: Option<String>,
    author: String,
    verifier: Option<AgentIdent>,
    agent: Option<AgentIdent>,
}

pub async fn verify(
    State(st): State<AppState>,
    ApiPath(t): ApiPath<String>,
    ApiJson(p): ApiJson<VerifyPayload>,
) -> ApiResult<Json<JobView>> {
    if !["pass", "fail"].contains(&p.verdict.as_str()) {
        return Err(ApiError::validation("verdict must be pass or fail"));
    }
    let author = required_text("author", &p.author, 120)?;
    let Some(verifier) = p.verifier else {
        return Err(ApiError::validation(
            "verifier identity is required (the reviewer's real agent_type, model, harness)",
        ));
    };
    strict_identity("verifier", &verifier)?;
    let mut red = Redactions::new();
    let author = red.text("author", author);
    let note = red.opt("note", p.note);
    let verifier = red.agent(Some(verifier));
    let agent = red.agent(p.agent);
    let v = verifier.unwrap_or(AgentIdent {
        agent_type: "unknown".into(),
        model: "unknown".into(),
        harness: "unknown".into(),
    });

    let mut tx = st.pool.begin().await?;
    lock_jobs(&mut tx).await?;
    let id = job_id(&mut tx, &t).await?;
    let c = core(&mut tx, id).await?;
    if c.status != "done" {
        return Err(state_conflict(&c.status, "verify"));
    }
    let Some(worker_id) = c.worker_identity_id else {
        return Err(ApiError::conflict("job has no recorded worker identity"));
    };
    let verifier_id = upsert_identity(
        &mut tx,
        &clean_part(&v.agent_type),
        &clean_part(&v.model),
        &clean_part(&v.harness),
    )
    .await?;
    if verifier_id == worker_id {
        return Err(ApiError::conflict(
            "the verifier must be a different identity from the worker",
        ));
    }
    if c.security {
        let (_, _, worker_harness) = identity_triple(&mut tx, worker_id).await?;
        if worker_harness == clean_part(&v.harness) {
            return Err(ApiError::conflict(
                "security jobs need a verifier on a different harness than the worker",
            ));
        }
    }
    // Register the author alias (first write wins); the verdict itself is the verifier's.
    resolve_identity(&mut tx, &author, agent.as_ref(), Some(id)).await?;

    let pass = p.verdict == "pass";
    let mut content = format!(
        "Verdict (attempt {}, round {}): {}",
        c.attempts,
        c.round,
        if pass { "PASS" } else { "FAIL" }
    );
    if let Some(n) = note.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
        content.push_str(" - ");
        content.push_str(n);
    }
    let mid = post_job_message(
        &mut tx,
        id,
        &author,
        Some(verifier_id),
        "verdict",
        &content,
        Some(worker_id),
    )
    .await?;
    if pass {
        sqlx::query(
            "UPDATE jobs SET status = 'verified', verified_at = now(), verdict_message_id = $2, \
               updated_at = now() WHERE topic_id = $1",
        )
        .bind(id)
        .bind(mid)
        .execute(&mut *tx)
        .await?;
        add_topic_event(
            &mut tx,
            id,
            "job-verified",
            &author,
            serde_json::json!({"verifier_identity_id": verifier_id, "round": c.round,
                               "first_pass": c.verify_fails == 0}),
        )
        .await?;
    } else {
        sqlx::query(
            "UPDATE jobs SET status = 'claimed', round = round + 1, verify_fails = verify_fails + 1, \
               completed_at = NULL, verdict_message_id = $2, updated_at = now() WHERE topic_id = $1",
        )
        .bind(id)
        .bind(mid)
        .execute(&mut *tx)
        .await?;
        add_topic_event(
            &mut tx,
            id,
            "job-verify-failed",
            &author,
            serde_json::json!({"verifier_identity_id": verifier_id, "round": c.round + 1}),
        )
        .await?;
        // Dependents that may already have started on this job's output get a note.
        let g = load_graph(&mut tx).await?;
        let name = g.nodes.get(&id).map(|n| n.name.clone()).unwrap_or_default();
        let dependents: Vec<i64> = g
            .deps
            .iter()
            .filter(|(_, ds)| ds.contains(&id))
            .map(|(j, _)| *j)
            .collect();
        for dep in dependents {
            add_topic_event(
                &mut tx,
                dep,
                "dependency-reopened",
                &author,
                serde_json::json!({"dependency": name, "dependency_id": id}),
            )
            .await?;
        }
    }
    touch_topic(&mut tx, id).await?;
    red.record(&mut tx, Some(id), &author).await?;
    let view = view_of(&mut tx, &st.jobs, id).await?;
    tx.commit().await?;
    Ok(Json(view))
}

// ---------------------------------------------------------------- reclaim / cancel / retry

#[derive(Deserialize)]
pub struct ActorReason {
    actor: String,
    reason: Option<String>,
}

pub async fn reclaim(
    State(st): State<AppState>,
    ApiPath(t): ApiPath<String>,
    ApiJson(p): ApiJson<ActorReason>,
) -> ApiResult<Json<JobView>> {
    let actor = required_text("actor", &p.actor, 120)?;
    let mut red = Redactions::new();
    let actor = red.text("actor", actor);
    let reason = red.opt("reason", p.reason);
    let mut tx = st.pool.begin().await?;
    lock_jobs(&mut tx).await?;
    let id = job_id(&mut tx, &t).await?;
    let c = core(&mut tx, id).await?;
    if !matches!(c.status.as_str(), "claimed" | "done") {
        return Err(state_conflict(&c.status, "reclaim"));
    }
    reset_to_open(&mut tx, id).await?;
    add_topic_event(
        &mut tx,
        id,
        "job-reclaimed",
        &actor,
        serde_json::json!({"from": c.status, "reason": reason}),
    )
    .await?;
    reopen_topic(&mut tx, id, &actor).await?;
    red.record(&mut tx, Some(id), &actor).await?;
    let v = view_of(&mut tx, &st.jobs, id).await?;
    tx.commit().await?;
    Ok(Json(v))
}

/// Back to `open`: claim cleared, nonce invalidated, round reset, worker forgotten.
async fn reset_to_open(conn: &mut PgConnection, id: i64) -> ApiResult<()> {
    sqlx::query(
        "UPDATE jobs SET status = 'open', claimed_by = NULL, claimed_at = NULL, \
           claim_nonce_hash = NULL, round = 0, worker_identity_id = NULL, completed_at = NULL, \
           verified_at = NULL, ready_at = now(), updated_at = now() WHERE topic_id = $1",
    )
    .bind(id)
    .execute(conn)
    .await?;
    Ok(())
}

pub async fn cancel(
    State(st): State<AppState>,
    ApiPath(t): ApiPath<String>,
    ApiJson(p): ApiJson<ActorReason>,
) -> ApiResult<Json<JobView>> {
    let actor = required_text("actor", &p.actor, 120)?;
    let mut red = Redactions::new();
    let actor = red.text("actor", actor);
    let reason = red.opt("reason", p.reason);
    let mut tx = st.pool.begin().await?;
    lock_jobs(&mut tx).await?;
    let id = job_id(&mut tx, &t).await?;
    let c = core(&mut tx, id).await?;
    if !matches!(c.status.as_str(), "open" | "claimed" | "done" | "failed") {
        return Err(state_conflict(&c.status, "cancel"));
    }
    sqlx::query(
        "UPDATE jobs SET status = 'cancelled', claimed_by = NULL, claimed_at = NULL, \
           claim_nonce_hash = NULL, updated_at = now() WHERE topic_id = $1",
    )
    .bind(id)
    .execute(&mut *tx)
    .await?;
    add_topic_event(
        &mut tx,
        id,
        "job-cancelled",
        &actor,
        serde_json::json!({"from": c.status, "reason": reason}),
    )
    .await?;
    touch_topic(&mut tx, id).await?;
    red.record(&mut tx, Some(id), &actor).await?;
    let v = view_of(&mut tx, &st.jobs, id).await?;
    tx.commit().await?;
    Ok(Json(v))
}

#[derive(Deserialize)]
pub struct ActorOnly {
    actor: String,
}

pub async fn retry(
    State(st): State<AppState>,
    ApiPath(t): ApiPath<String>,
    ApiJson(p): ApiJson<ActorOnly>,
) -> ApiResult<Json<JobView>> {
    let actor = required_text("actor", &p.actor, 120)?;
    let mut red = Redactions::new();
    let actor = red.text("actor", actor);
    let mut tx = st.pool.begin().await?;
    lock_jobs(&mut tx).await?;
    let id = job_id(&mut tx, &t).await?;
    let c = core(&mut tx, id).await?;
    if !matches!(c.status.as_str(), "failed" | "cancelled") {
        return Err(state_conflict(&c.status, "retry"));
    }
    reset_to_open(&mut tx, id).await?;
    // Re-check the dependency graph under the lock.
    let g = load_graph(&mut tx).await?;
    if has_cycle(&g) {
        return Err(ApiError::conflict("dependency cycle"));
    }
    add_topic_event(
        &mut tx,
        id,
        "job-retried",
        &actor,
        serde_json::json!({"from": c.status}),
    )
    .await?;
    reopen_topic(&mut tx, id, &actor).await?;
    red.record(&mut tx, Some(id), &actor).await?;
    let v = view_of(&mut tx, &st.jobs, id).await?;
    tx.commit().await?;
    Ok(Json(v))
}

// ---------------------------------------------------------------- amend / patch

#[derive(Deserialize)]
pub struct AmendPayload {
    author: String,
    agent: Option<AgentIdent>,
    text: String,
}

pub async fn amend(
    State(st): State<AppState>,
    ApiPath(t): ApiPath<String>,
    ApiJson(p): ApiJson<AmendPayload>,
) -> ApiResult<(StatusCode, Json<Message>)> {
    let author = required_text("author", &p.author, 120)?;
    let text = required_text("text", &p.text, MAX_SPEC_CHARS)?;
    let mut red = Redactions::new();
    let author = red.text("author", author);
    let text = red.text("text", text);
    let agent = red.agent(p.agent);
    let mut tx = st.pool.begin().await?;
    lock_jobs(&mut tx).await?;
    let id = job_id(&mut tx, &t).await?;
    let identity_id = resolve_identity(&mut tx, &author, agent.as_ref(), Some(id)).await?;
    let mid = post_job_message(
        &mut tx,
        id,
        &author,
        Some(identity_id),
        "amendment",
        &text,
        None,
    )
    .await?;
    add_topic_event(
        &mut tx,
        id,
        "job-amended",
        &author,
        serde_json::json!({"message_id": mid}),
    )
    .await?;
    touch_topic(&mut tx, id).await?;
    red.record(&mut tx, Some(id), &author).await?;
    let m = fetch_message(&mut tx, mid).await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(m)))
}

#[derive(Deserialize)]
pub struct PatchPayload {
    priority: Option<i32>,
    depends_on: Option<Vec<String>>,
    owns: Option<Vec<String>>,
    actor: String,
}

pub async fn patch(
    State(st): State<AppState>,
    ApiPath(t): ApiPath<String>,
    ApiJson(p): ApiJson<PatchPayload>,
) -> ApiResult<Json<JobView>> {
    let actor = required_text("actor", &p.actor, 120)?;
    if p.priority.is_none() && p.depends_on.is_none() && p.owns.is_none() {
        return Err(ApiError::validation(
            "nothing to change: give priority, depends_on or owns",
        ));
    }
    if p.priority.is_some_and(|v| v.abs() > 1_000_000) {
        return Err(ApiError::validation("priority must be within +/-1000000"));
    }
    let mut red = Redactions::new();
    let actor = red.text("actor", actor);
    let owns = match p.owns {
        Some(items) => Some(normalise_owns_list(items, &st.jobs)?),
        None => None,
    };

    let mut tx = st.pool.begin().await?;
    lock_jobs(&mut tx).await?;
    let id = job_id(&mut tx, &t).await?;
    let c = core(&mut tx, id).await?;
    let depends_changed = p.depends_on.is_some();
    let structural = depends_changed || owns.is_some();
    match (c.status.as_str(), structural) {
        ("open", _) => {}
        ("claimed" | "done", false) => {}
        ("claimed" | "done", true) => {
            return Err(ApiError::conflict(format!(
                "job is {}; only priority can be changed",
                c.status
            )))
        }
        (other, _) => return Err(state_conflict(other, "patch")),
    }
    if let Some(pr) = p.priority {
        sqlx::query("UPDATE jobs SET priority = $2, updated_at = now() WHERE topic_id = $1")
            .bind(id)
            .bind(pr)
            .execute(&mut *tx)
            .await?;
    }
    if let Some(o) = &owns {
        let cap: String = sqlx::query_scalar("SELECT capability FROM jobs WHERE topic_id = $1")
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
        if cap == "local-build" && o.is_empty() {
            return Err(ApiError::validation(
                "local-build jobs must declare a non-empty owns",
            ));
        }
        sqlx::query("UPDATE jobs SET owns = $2, updated_at = now() WHERE topic_id = $1")
            .bind(id)
            .bind(o)
            .execute(&mut *tx)
            .await?;
    }
    if let Some(deps) = p.depends_on {
        let mut ids: Vec<i64> = Vec::new();
        for d in deps {
            let d = d.trim().to_string();
            let dep = job_id(&mut tx, &d).await.map_err(|e| match e {
                ApiError::NotFound(_) => ApiError::validation(format!("unknown dependency '{d}'")),
                other => other,
            })?;
            if dep == id {
                return Err(ApiError::validation("a job cannot depend on itself"));
            }
            if !ids.contains(&dep) {
                ids.push(dep);
            }
        }
        sqlx::query("DELETE FROM job_deps WHERE job_id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        for dep in &ids {
            sqlx::query("INSERT INTO job_deps (job_id, depends_on) VALUES ($1,$2)")
                .bind(id)
                .bind(dep)
                .execute(&mut *tx)
                .await?;
        }
        let g = load_graph(&mut tx).await?;
        if has_cycle(&g) {
            return Err(ApiError::conflict("dependency cycle"));
        }
    }
    add_topic_event(
        &mut tx,
        id,
        "job-patched",
        &actor,
        serde_json::json!({"priority": p.priority, "owns": owns, "depends_changed": depends_changed}),
    )
    .await?;
    touch_topic(&mut tx, id).await?;
    red.record(&mut tx, Some(id), &actor).await?;
    let v = view_of(&mut tx, &st.jobs, id).await?;
    tx.commit().await?;
    Ok(Json(v))
}

// ---------------------------------------------------------------- summary (metrics, stats)

pub struct JobSummary {
    pub by_status: BTreeMap<String, i64>,
    pub ready: i64,
    pub blocked: i64,
    pub stale: i64,
    pub max_claim_age_s: f64,
}

pub async fn summary(conn: &mut PgConnection, cfg: &JobsConfig) -> Result<JobSummary, sqlx::Error> {
    let g = load_graph(conn).await?;
    let mut by_status: BTreeMap<String, i64> =
        STATUSES.iter().map(|s| (s.to_string(), 0)).collect();
    let (mut ready, mut blocked) = (0, 0);
    for n in g.nodes.values() {
        *by_status.entry(n.status.clone()).or_insert(0) += 1;
        let a = analyse(&g, n.id);
        ready += i64::from(a.ready);
        blocked += i64::from(a.blocked);
    }
    let h = cfg.stale_h.clamp(1, 8760);
    let sql = format!(
        "SELECT count(*) FILTER (WHERE (j.status = 'claimed' \
                  AND j.claimed_at < now() - make_interval(hours => {h}) \
                  AND t.last_activity_at < now() - interval '1 hour') \
                  OR (j.status = 'done' AND j.completed_at < now() - make_interval(hours => {h}))), \
                coalesce(max(extract(epoch FROM now() - j.claimed_at)) \
                  FILTER (WHERE j.status = 'claimed'), 0)::float8 \
           FROM {JOB_FROM}"
    );
    let (stale, max_age): (i64, f64) = sqlx::query_as(&sql).fetch_one(conn).await?;
    Ok(JobSummary {
        by_status,
        ready,
        blocked,
        stale,
        max_claim_age_s: max_age,
    })
}

/// Used by the topic routes: does this topic have a job in a state that guards its lifecycle?
pub async fn topic_has_active_job(conn: &mut PgConnection, tid: i64) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM jobs WHERE topic_id = $1 AND status IN ('open','claimed','done'))",
    )
    .bind(tid)
    .fetch_one(conn)
    .await
}

/// Names of jobs that depend on `tid` (blocks deleting it).
pub async fn dependents_of(conn: &mut PgConnection, tid: i64) -> Result<Vec<String>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT t.name FROM job_deps d JOIN topics t ON t.id = d.job_id \
          WHERE d.depends_on = $1 ORDER BY t.name",
    )
    .bind(tid)
    .fetch_all(conn)
    .await
}

/// Is this topic a job topic? (cheap check used before taking the jobs lock)
pub async fn is_job_topic(conn: &mut PgConnection, tid: i64) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM jobs WHERE topic_id = $1)")
        .bind(tid)
        .fetch_one(conn)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roots() -> Vec<String> {
        vec![
            "/srv/example/scripts".to_string(),
            "/srv/example/git".to_string(),
        ]
    }

    #[test]
    fn owns_normalisation_and_rejections() {
        let r = roots();
        assert_eq!(
            normalise_owns("/srv/example/git//hive-board/./src/", &r).as_deref(),
            Ok("/srv/example/git/hive-board/src")
        );
        // Allowed: a repo dir, a crate dir, `.github` (not `.git`), a lockfile look-alike.
        for ok in [
            "/srv/example/git/hive-board",
            "/srv/example/scripts/crates/hive-mcp",
            "/srv/example/git/hive-board/.github",
            "/srv/example/git/hive-board/Cargo.lock.bak",
        ] {
            assert!(normalise_owns(ok, &r).is_ok(), "{ok}");
        }
        // Rejected: relative, `..`, `~`, glob, roots themselves and their parents,
        // `.git`, lockfiles, outside the roots.
        for bad in [
            "relative/path",
            "/srv/example/git/../etc",
            "~/git/hive-board",
            "/srv/example/git/hive-*",
            "/srv/example/scripts",
            "/srv/example/git",
            "/srv/example",
            "/srv",
            "/",
            "/srv/example/git/hive-board/.git",
            "/srv/example/git/hive-board/.git/hooks",
            "/srv/example/git/hive-board/Cargo.lock",
            "/srv/example/git/hive-board/frontend/package-lock.json",
            "/srv/example/git/hive-board/pnpm-lock.yaml",
            "/srv/example/git/hive-board/yarn.lock",
            "/etc/passwd",
            "/srv/example/gitx/repo",
        ] {
            assert!(normalise_owns(bad, &r).is_err(), "{bad}");
        }
    }

    #[test]
    fn no_roots_rejects_every_owns_entry() {
        for raw in ["/srv/example/git/repo", "/a/b/c", "/"] {
            let e = normalise_owns(raw, &[]).unwrap_err();
            assert!(e.contains("HIVE_JOB_OWNS_ROOTS"), "{e}");
        }
    }

    #[test]
    fn overlap_is_by_whole_component() {
        assert!(owns_overlap("/a/b", "/a/b"));
        assert!(owns_overlap("/a/b", "/a/b/c"));
        assert!(owns_overlap("/a/b/c", "/a/b"));
        assert!(!owns_overlap("/a/b", "/a/bc"));
        assert!(!owns_overlap("/a/bc", "/a/b"));
        assert!(!owns_overlap("/a/b", "/a/c"));
    }

    #[test]
    fn cycle_detection() {
        let node = |id| GNode {
            id,
            name: format!("n{id}"),
            status: "open".into(),
            security: false,
            priority: 100,
            owns: vec![],
        };
        let mut g = Graph {
            nodes: [(1, node(1)), (2, node(2)), (3, node(3))]
                .into_iter()
                .collect(),
            deps: HashMap::new(),
        };
        g.deps.insert(2, vec![1]);
        g.deps.insert(3, vec![2]);
        assert!(!has_cycle(&g));
        g.deps.insert(1, vec![3]);
        assert!(has_cycle(&g));
    }
}
