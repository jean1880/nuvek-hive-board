//! Job tools (hive-board v0.3.0 and later): argument types, client-side validation and
//! request builders. Pure functions, so request shapes are unit-testable without a server. The
//! server re-validates everything and owns the real rules (READY, overlap, nonces, rounds); these
//! checks only give agents a fast, specific error. Outgoing text is redacted centrally in
//! `client::Hive::call`.

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};

type Check<T> = Result<T, String>;

/// `live-infra` is refused (there are no per-actor tokens yet), here and on the server.
const CAPABILITIES: [&str; 2] = ["read-only", "local-build"];
const TIERS: [&str; 3] = ["fast", "standard", "deep"];
const MAX_BATCH: usize = 50;
const LOCKFILES: [&str; 4] = [
    "Cargo.lock",
    "package-lock.json",
    "pnpm-lock.yaml",
    "yarn.lock",
];

#[derive(Deserialize, JsonSchema)]
pub struct JobSpec {
    /// Job slug, also its topic name: ^[a-z][a-z0-9-]{1,79}$
    pub name: String,
    /// Human-readable title
    pub title: String,
    /// SELF-CONTAINED brief for the worker: goal, files, constraints, how to verify. Immutable; change it later with job_admin amend
    pub spec: String,
    /// read-only (default) or local-build. live-infra is not allowed
    pub capability: Option<String>,
    /// Lower runs first (default 100)
    pub priority: Option<i32>,
    /// true for security-sensitive work: verification then needs a different HARNESS than the worker's, and dependents wait for 'verified'
    pub security: Option<bool>,
    /// e.g. build, review, research (for routing metrics)
    pub task_kind: Option<String>,
    /// ABSOLUTE files or directories this job may edit, no '..', '~' or globs, not lockfiles or .git. Symlinks are resolved on this machine before posting. REQUIRED (non-empty) for local-build. Overlapping owns never run at once
    pub owns: Option<Vec<String>>,
    /// Jobs that must finish first: names of jobs in this same batch, or ids/slugs of existing jobs
    pub depends_on: Option<Vec<String>>,
    /// Suggested worker tier: fast, standard or deep
    pub tier: Option<String>,
    /// Suggested worker agent type, e.g. 'rust-reviewer'
    pub agent_type: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct JobPostArgs {
    /// The wave: 1-50 jobs created together, all-or-nothing, dependencies resolved by name
    pub jobs: Vec<JobSpec>,
    /// Name of the poster (the orchestrator)
    pub author: String,
    /// YOUR agent type (defaults to HIVE_AGENT_TYPE, then 'unknown')
    pub agent_type: Option<String>,
    /// YOUR model (defaults to HIVE_MODEL, then 'unknown')
    pub model: Option<String>,
    /// YOUR harness (defaults to HIVE_HARNESS, then 'unknown')
    pub harness: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct JobListArgs {
    /// One job by id or slug (returns its full state, readiness and conflicts)
    pub job: Option<String>,
    /// open, claimed, done, verified, failed, cancelled
    pub status: Option<String>,
    /// true = only READY jobs; false = only jobs that are not ready
    pub ready: Option<bool>,
    /// true = only BLOCKED jobs (a dependency failed or was cancelled)
    pub blocked: Option<bool>,
    /// Jobs claimed by this claimant name (restart recovery: pass your own name)
    pub claimed_by: Option<String>,
    /// Max rows (default 50)
    pub limit: Option<u32>,
}

#[derive(Deserialize, JsonSchema)]
pub struct JobClaimArgs {
    /// Orchestrator name claiming the batch
    pub claimant: String,
    /// Batch size 1-10 (default 1)
    pub limit: Option<u32>,
    /// Override a known false-positive owns overlap (recorded on the board)
    pub force_overlap: Option<bool>,
    /// YOUR agent type (defaults to HIVE_AGENT_TYPE, then 'unknown')
    pub agent_type: Option<String>,
    /// YOUR model (defaults to HIVE_MODEL, then 'unknown')
    pub model: Option<String>,
    /// YOUR harness (defaults to HIVE_HARNESS, then 'unknown')
    pub harness: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct JobCompleteArgs {
    /// Job id or slug
    pub job: String,
    /// The nonce job_claim returned for this job (returned once; keep it)
    pub nonce: String,
    /// The current round: job_claim returns it, and a failed verify bumps it (the new round comes back in the job_verify response; echo that one)
    pub round: i64,
    /// done or failed
    pub status: String,
    /// What was produced, or why it failed
    pub result: String,
    /// Who is posting the result (the orchestrator)
    pub author: String,
    /// The WORKER's real agent type, e.g. 'rust-craftsman' (required, not 'unknown')
    pub worker_agent_type: String,
    /// The WORKER's real model, e.g. 'sonnet' (required)
    pub worker_model: String,
    /// The WORKER's real harness, e.g. 'claude' (required)
    pub worker_harness: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct JobVerifyArgs {
    /// Job id or slug (must be in status done)
    pub job: String,
    /// pass or fail
    pub verdict: String,
    /// What the verifier checked and found
    pub note: String,
    /// Who is recording the verdict (the orchestrator)
    pub author: String,
    /// The VERIFIER's real agent type, e.g. 'rust-reviewer' (required, not 'unknown')
    pub verifier_agent_type: String,
    /// The VERIFIER's real model (required). The server requires the verifier's identity triple (agent_type, model, harness) to differ from the worker's
    pub verifier_model: String,
    /// The VERIFIER's real harness (required). For security jobs the harness itself must differ from the worker's
    pub verifier_harness: String,
    /// YOUR (the caller's) agent type, used for your author alias; NOT the reviewer's (defaults to HIVE_AGENT_TYPE, then 'unknown')
    pub agent_type: Option<String>,
    /// YOUR (the caller's) model (defaults to HIVE_MODEL, then 'unknown')
    pub model: Option<String>,
    /// YOUR (the caller's) harness (defaults to HIVE_HARNESS, then 'unknown')
    pub harness: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct JobAdminArgs {
    /// amend | reclaim | cancel | retry | reprioritise
    pub op: String,
    /// Job id or slug
    pub job: String,
    /// Who is acting
    pub actor: String,
    /// amend: the clarification or correction (the spec itself is immutable)
    pub text: Option<String>,
    /// reclaim/cancel: why
    pub reason: Option<String>,
    /// reprioritise: new priority (lower runs first)
    pub priority: Option<i32>,
    /// reprioritise (open jobs only): replacement dependencies, ids or slugs of existing jobs
    pub depends_on: Option<Vec<String>>,
    /// reprioritise (open jobs only): replacement ABSOLUTE owns paths (same rules as job_post)
    pub owns: Option<Vec<String>>,
    /// amend: YOUR agent type (defaults to HIVE_AGENT_TYPE, then 'unknown')
    pub agent_type: Option<String>,
    /// amend: YOUR model (defaults to HIVE_MODEL, then 'unknown')
    pub model: Option<String>,
    /// amend: YOUR harness (defaults to HIVE_HARNESS, then 'unknown')
    pub harness: Option<String>,
}

/// A built request: method, path under `/api`, query pairs and JSON body.
#[derive(Debug)]
pub struct Req {
    pub method: &'static str,
    pub path: String,
    pub query: Vec<(&'static str, String)>,
    pub body: Option<Value>,
}

fn one_of(field: &str, v: &str, allowed: &[&str]) -> Check<()> {
    if allowed.contains(&v) {
        Ok(())
    } else {
        Err(format!(
            "invalid {field} '{v}': expected one of {}",
            allowed.join(", ")
        ))
    }
}

fn opt_one_of(field: &str, v: &Option<String>, allowed: &[&str]) -> Check<()> {
    v.as_deref().map_or(Ok(()), |v| one_of(field, v, allowed))
}

fn put<T: serde::Serialize>(o: &mut Map<String, Value>, k: &str, v: &Option<T>) {
    if let Some(v) = v {
        if let Ok(j) = serde_json::to_value(v) {
            o.insert(k.to_string(), j);
        }
    }
}

/// Topic/job slug: ^[a-z][a-z0-9-]{1,79}$.
pub fn is_slug(name: &str) -> bool {
    let n = name.chars().count();
    let mut cs = name.chars();
    cs.next().is_some_and(|c| c.is_ascii_lowercase())
        && name
            .chars()
            .skip(1)
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && (2..=80).contains(&n)
}

fn is_id(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_digit())
}

/// Normalise one `owns` entry and reject what the server rejects: relative paths, `..`, `~`,
/// globs, NUL, a `.git` component, a lockfile as the last component, and anything with fewer
/// than 3 components. (The server additionally requires the path to lie strictly below its
/// HIVE_JOB_OWNS_ROOTS, which the client cannot know.) Returns the normalised path.
pub fn normalise_owns(p: &str) -> Check<String> {
    let p = p.trim();
    if !p.starts_with('/') {
        return Err(format!(
            "invalid owns entry '{p}': must be an absolute path"
        ));
    }
    if p.contains('\0')
        || p.split('/').any(|c| c.starts_with('~'))
        || p.contains(['*', '?', '[', ']', '{', '}'])
    {
        return Err(format!(
            "invalid owns entry '{p}': components starting with '~', globs and control characters are not allowed"
        ));
    }
    let parts: Vec<&str> = p
        .split('/')
        .filter(|c| !c.is_empty() && *c != ".")
        .collect();
    if parts.contains(&"..") {
        return Err(format!(
            "invalid owns entry '{p}': '..' components are not allowed"
        ));
    }
    if parts.contains(&".git") {
        return Err(format!(
            "invalid owns entry '{p}': .git is shared state the coordinator owns"
        ));
    }
    if parts.last().is_some_and(|l| LOCKFILES.contains(l)) {
        return Err(format!(
            "invalid owns entry '{p}': lockfiles are shared; the coordinator owns them"
        ));
    }
    if parts.len() < 3 {
        return Err(format!(
            "invalid owns entry '{p}': too broad (need at least 3 path components, e.g. /srv/repos/app)"
        ));
    }
    Ok(format!("/{}", parts.join("/")))
}

/// Resolve symlinks in an `owns` entry on THIS machine, where the workers run, before the server
/// sees it. The server's containment and overlap checks are lexical (it cannot see this
/// filesystem), so `/srv/repos/alias/src` and `/srv/repos/app/src` would never overlap there
/// even when `alias` is a link to `app`, and a link out of the allowed roots would pass.
///
/// The longest EXISTING ancestor is canonicalised (`std::fs::canonicalize` resolves every
/// symlink in it), the not-yet-existing rest is appended, and the result goes through
/// [`normalise_owns`] again, so a link into `.git`, onto a lockfile or up to a short path is
/// refused like the literal path would be. Only a missing component is climbed past; a path that
/// cannot be inspected or resolved (permission denied, not a directory, any other error) is
/// refused rather than sent unresolved.
///
/// Ceiling: this is still a scheduling guard, not a sandbox. A link created after the job is
/// posted, or on another machine, is not seen.
pub fn resolve_owns(lexical: &str) -> Check<String> {
    let lexical = normalise_owns(lexical)?;
    let full = std::path::Path::new(&lexical);
    let mut rest: Vec<&std::ffi::OsStr> = Vec::new();
    let mut base = full;
    // Climb only past components that do not exist yet. Any other error (permission denied, a
    // file used as a directory, I/O) means the path cannot be checked, so the entry is refused:
    // fail closed. `symlink_metadata` sees a dangling link as existing, so canonicalize reports it.
    loop {
        match std::fs::symlink_metadata(base) {
            Ok(_) => break,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let (Some(parent), Some(name)) = (base.parent(), base.file_name()) else {
                    break;
                };
                rest.push(name);
                base = parent;
            }
            Err(e) => {
                return Err(format!(
                    "invalid owns entry '{lexical}': cannot inspect {}: {e}",
                    base.display()
                ));
            }
        }
    }
    let mut real = std::fs::canonicalize(base).map_err(|e| {
        format!(
            "invalid owns entry '{lexical}': cannot resolve {}: {e}",
            base.display()
        )
    })?;
    for name in rest.iter().rev() {
        real.push(name);
    }
    let real = real.to_str().ok_or_else(|| {
        format!("invalid owns entry '{lexical}': resolves to a path that is not UTF-8")
    })?;
    normalise_owns(real).map_err(|e| {
        if real == lexical {
            e
        } else {
            format!("{e} (resolved from '{lexical}' through a symlink)")
        }
    })
}

fn normalise_all(owns: &[String]) -> Check<Vec<String>> {
    owns.iter().map(|p| resolve_owns(p)).collect()
}

fn dep_ref(d: &str) -> Check<()> {
    if is_slug(d) || is_id(d) {
        Ok(())
    } else {
        Err(format!(
            "invalid dependency '{d}': expected a job slug or numeric id"
        ))
    }
}

/// Reject cycles among in-batch dependencies (the server also checks, under its lock).
fn check_batch_cycles(jobs: &[JobSpec]) -> Check<()> {
    let names: HashSet<&str> = jobs.iter().map(|j| j.name.as_str()).collect();
    let mut indeg: HashMap<&str, usize> = names.iter().map(|n| (*n, 0)).collect();
    let mut out: HashMap<&str, Vec<&str>> = HashMap::new();
    for j in jobs {
        for d in j.depends_on.iter().flatten() {
            if d == &j.name {
                return Err(format!("job '{}' depends on itself", j.name));
            }
            if names.contains(d.as_str()) {
                *indeg.entry(j.name.as_str()).or_default() += 1;
                out.entry(d.as_str()).or_default().push(j.name.as_str());
            }
        }
    }
    let mut queue: Vec<&str> = indeg
        .iter()
        .filter(|(_, n)| **n == 0)
        .map(|(k, _)| *k)
        .collect();
    let mut seen = 0;
    while let Some(n) = queue.pop() {
        seen += 1;
        for m in out.get(n).into_iter().flatten() {
            if let Some(c) = indeg.get_mut(m) {
                *c -= 1;
                if *c == 0 {
                    queue.push(m);
                }
            }
        }
    }
    if seen == names.len() {
        Ok(())
    } else {
        Err("dependency cycle among the jobs in this batch".into())
    }
}

fn job_json(j: &JobSpec) -> Check<Value> {
    if !is_slug(&j.name) {
        return Err(format!(
            "invalid job name '{}': use 2-80 characters matching ^[a-z][a-z0-9-]{{1,79}}$",
            j.name
        ));
    }
    let cap = j.capability.as_deref().unwrap_or("read-only");
    if cap == "live-infra" {
        return Err(format!(
            "job '{}': live-infra jobs cannot be created; do that work yourself",
            j.name
        ));
    }
    one_of("capability", cap, &CAPABILITIES)?;
    opt_one_of("tier", &j.tier, &TIERS)?;
    if j.spec.trim().is_empty() {
        return Err(format!(
            "job '{}': spec must not be empty (it is the worker's whole brief)",
            j.name
        ));
    }
    for d in j.depends_on.iter().flatten() {
        dep_ref(d).map_err(|e| format!("job '{}': {e}", j.name))?;
    }
    let owns = normalise_all(j.owns.as_deref().unwrap_or(&[]))
        .map_err(|e| format!("job '{}': {e}", j.name))?;
    if cap == "local-build" && owns.is_empty() {
        return Err(format!(
            "job '{}': local-build jobs must declare owns (absolute paths it may edit)",
            j.name
        ));
    }
    let mut o = Map::new();
    o.insert("name".into(), json!(j.name));
    o.insert("title".into(), json!(j.title));
    o.insert("spec".into(), json!(j.spec));
    o.insert("capability".into(), json!(cap));
    put(&mut o, "priority", &j.priority);
    put(&mut o, "security", &j.security);
    put(&mut o, "task_kind", &j.task_kind);
    if !owns.is_empty() {
        o.insert("owns".into(), json!(owns));
    }
    put(&mut o, "depends_on", &j.depends_on);
    put(&mut o, "tier", &j.tier);
    put(&mut o, "agent_type", &j.agent_type);
    Ok(Value::Object(o))
}

pub fn post_req(a: &JobPostArgs, agent: Value) -> Check<Req> {
    if a.jobs.is_empty() || a.jobs.len() > MAX_BATCH {
        return Err(format!("jobs must hold 1-{MAX_BATCH} entries"));
    }
    let mut seen = HashSet::new();
    for j in &a.jobs {
        if !seen.insert(j.name.as_str()) {
            return Err(format!("duplicate job name '{}' in the batch", j.name));
        }
    }
    let jobs = a.jobs.iter().map(job_json).collect::<Check<Vec<_>>>()?;
    check_batch_cycles(&a.jobs)?;
    Ok(Req {
        method: "POST",
        path: "/api/jobs".into(),
        query: vec![],
        body: Some(json!({"jobs": jobs, "author": a.author, "agent": agent})),
    })
}

pub fn list_req(a: &JobListArgs) -> Check<Req> {
    if let Some(j) = &a.job {
        return Ok(Req {
            method: "GET",
            path: format!("/api/jobs/{}", crate::client::seg(j)),
            query: vec![],
            body: None,
        });
    }
    opt_one_of(
        "status",
        &a.status,
        &["open", "claimed", "done", "verified", "failed", "cancelled"],
    )?;
    let mut q = Vec::new();
    if let Some(s) = &a.status {
        q.push(("status", s.clone()));
    }
    if let Some(r) = a.ready {
        q.push(("ready", r.to_string()));
    }
    if let Some(b) = a.blocked {
        q.push(("blocked", b.to_string()));
    }
    if let Some(c) = &a.claimed_by {
        q.push(("claimed_by", c.clone()));
    }
    q.push(("limit", a.limit.unwrap_or(50).to_string()));
    Ok(Req {
        method: "GET",
        path: "/api/jobs".into(),
        query: q,
        body: None,
    })
}

pub fn claim_req(a: &JobClaimArgs, agent: Value) -> Check<Req> {
    let limit = a.limit.unwrap_or(1);
    if !(1..=10).contains(&limit) {
        return Err(format!("invalid limit {limit}: expected 1-10"));
    }
    let mut b = Map::new();
    b.insert("claimant".into(), json!(a.claimant));
    b.insert("limit".into(), json!(limit));
    put(&mut b, "force_overlap", &a.force_overlap);
    b.insert("agent".into(), agent);
    Ok(Req {
        method: "POST",
        path: "/api/jobs/claim".into(),
        query: vec![],
        body: Some(Value::Object(b)),
    })
}

/// A real, explicit identity: every field non-blank and not the placeholder `unknown`.
pub fn require_identity(who: &str, t: &str, m: &str, h: &str) -> Check<Value> {
    for (field, v) in [("agent_type", t), ("model", m), ("harness", h)] {
        let v = v.trim();
        if v.is_empty() || v.eq_ignore_ascii_case("unknown") {
            return Err(format!(
                "{who} {field} is required and must be the real value, not blank or 'unknown'"
            ));
        }
    }
    Ok(json!({"agent_type": t.trim(), "model": m.trim(), "harness": h.trim()}))
}

fn need_nonce(n: &str) -> Check<()> {
    if n.trim().is_empty() {
        Err("nonce is required: use the one job_claim returned for this job".into())
    } else {
        Ok(())
    }
}

pub fn complete_req(a: &JobCompleteArgs) -> Check<Req> {
    one_of("status", &a.status, &["done", "failed"])?;
    need_nonce(&a.nonce)?;
    let worker = require_identity(
        "worker",
        &a.worker_agent_type,
        &a.worker_model,
        &a.worker_harness,
    )?;
    Ok(Req {
        method: "POST",
        path: format!("/api/jobs/{}/complete", crate::client::seg(&a.job)),
        query: vec![],
        body: Some(
            json!({"nonce": a.nonce, "round": a.round, "status": a.status,
                          "worker": worker, "result": a.result, "author": a.author}),
        ),
    })
}

/// `agent` is the CALLER's identity (the orchestrator recording the verdict): the server resolves
/// the author's alias from it, first write wins, so it must never be the reviewer's identity.
pub fn verify_req(a: &JobVerifyArgs, agent: Value) -> Check<Req> {
    one_of("verdict", &a.verdict, &["pass", "fail"])?;
    let verifier = require_identity(
        "verifier",
        &a.verifier_agent_type,
        &a.verifier_model,
        &a.verifier_harness,
    )?;
    Ok(Req {
        method: "POST",
        path: format!("/api/jobs/{}/verify", crate::client::seg(&a.job)),
        query: vec![],
        body: Some(
            json!({"verdict": a.verdict, "note": a.note, "author": a.author,
                          "verifier": verifier, "agent": agent}),
        ),
    })
}

pub fn admin_req(a: &JobAdminArgs, agent: Value) -> Check<Req> {
    one_of(
        "op",
        &a.op,
        &["amend", "reclaim", "cancel", "retry", "reprioritise"],
    )?;
    let base = format!("/api/jobs/{}", crate::client::seg(&a.job));
    let req = |method, suffix: &str, body| Req {
        method,
        path: format!("{base}{suffix}"),
        query: vec![],
        body: Some(body),
    };
    match a.op.as_str() {
        "amend" => {
            let text = a.text.as_deref().filter(|t| !t.trim().is_empty());
            let text = text.ok_or("amend needs non-empty text")?;
            Ok(req(
                "POST",
                "/amend",
                json!({"author": a.actor, "agent": agent, "text": text}),
            ))
        }
        "reclaim" | "cancel" => {
            let mut b = Map::new();
            b.insert("actor".into(), json!(a.actor));
            put(&mut b, "reason", &a.reason);
            Ok(req("POST", &format!("/{}", a.op), Value::Object(b)))
        }
        "retry" => Ok(req("POST", "/retry", json!({"actor": a.actor}))),
        _ => {
            if a.priority.is_none() && a.depends_on.is_none() && a.owns.is_none() {
                return Err("reprioritise needs priority, depends_on or owns".into());
            }
            for d in a.depends_on.iter().flatten() {
                dep_ref(d)?;
            }
            let mut b = Map::new();
            put(&mut b, "priority", &a.priority);
            put(&mut b, "depends_on", &a.depends_on);
            if let Some(o) = &a.owns {
                b.insert("owns".into(), json!(normalise_all(o)?));
            }
            b.insert("actor".into(), json!(a.actor));
            Ok(req("PATCH", "", Value::Object(b)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent() -> Value {
        json!({"agent_type": "t", "model": "m", "harness": "h"})
    }

    fn spec(name: &str) -> JobSpec {
        JobSpec {
            name: name.into(),
            title: "T".into(),
            spec: "do it".into(),
            capability: None,
            priority: None,
            security: None,
            task_kind: None,
            owns: None,
            depends_on: None,
            tier: None,
            agent_type: None,
        }
    }

    fn post(jobs: Vec<JobSpec>) -> JobPostArgs {
        JobPostArgs {
            jobs,
            author: "orch".into(),
            agent_type: None,
            model: None,
            harness: None,
        }
    }

    #[test]
    fn owns_normalised_and_validated() {
        assert_eq!(
            normalise_owns("/srv/x//repo/./src/").unwrap(),
            "/srv/x/repo/src"
        );
        for bad in [
            "rel/path/x",
            "/srv/x/../y/z",
            "/srv/~x/r",
            "/srv/x/*/r",
            "/srv/x/r/.git/hooks",
            "/srv/x/repo/Cargo.lock",
            "/srv/x/ui/yarn.lock",
            "/",
            "/srv",
            "/srv/x",
            "",
        ] {
            assert!(normalise_owns(bad).is_err(), "{bad}");
        }
        // Exact component match: not a substring check.
        assert!(normalise_owns("/srv/x/my.github/r").is_ok());
        assert!(normalise_owns("/srv/x/repo/Cargo.lock.d").is_ok());
        assert!(normalise_owns("/srv/x/..data/r").is_ok());
        // Only a component STARTING with '~' is rejected; a '~' elsewhere is a legal filename.
        assert!(normalise_owns("/srv/x/backup~/r").is_ok());
        assert!(normalise_owns("/srv/x/~root/r").is_err());
        // Surrounding whitespace is trimmed before validation.
        assert_eq!(normalise_owns("  /srv/x/repo \n").unwrap(), "/srv/x/repo");
    }

    /// A private, canonical temp directory (removed on drop); never under $HOME.
    struct Scratch(std::path::PathBuf);
    impl Scratch {
        fn new(tag: &str) -> Self {
            let mut b = [0u8; 8];
            getrandom::fill(&mut b).unwrap();
            let name = format!(
                "hive-mcp-{tag}-{}",
                b.iter().map(|x| format!("{x:02x}")).collect::<String>()
            );
            let p = std::env::temp_dir().join(name);
            std::fs::create_dir_all(&p).unwrap();
            Self(std::fs::canonicalize(&p).unwrap())
        }
        fn path(&self, rel: &str) -> String {
            format!("{}/{rel}", self.0.display())
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn owns_resolves_symlink_aliases_to_one_real_path() {
        let s = Scratch::new("alias");
        std::fs::create_dir_all(s.path("work/app/src")).unwrap();
        std::os::unix::fs::symlink(s.path("work/app"), s.path("work/alias")).unwrap();
        let real = resolve_owns(&s.path("work/app/src")).unwrap();
        let via_link = resolve_owns(&s.path("work/alias/src")).unwrap();
        assert_eq!(
            via_link, real,
            "an alias resolves to the real path, so overlap is seen"
        );
        // A path that does not exist yet keeps its tail below the resolved ancestor.
        assert_eq!(
            resolve_owns(&s.path("work/alias/new/file.rs")).unwrap(),
            s.path("work/app/new/file.rs")
        );
        // The batch builder sends the resolved form.
        let got = normalise_all(&[s.path("work/alias/src")]).unwrap();
        assert_eq!(got, vec![real]);
    }

    #[test]
    fn owns_that_cannot_be_inspected_is_refused_not_climbed() {
        use std::os::unix::fs::PermissionsExt;
        let s = Scratch::new("denied");
        // A file used as a directory: ENOTDIR, never treated as "not created yet".
        std::fs::create_dir_all(s.path("work")).unwrap();
        std::fs::write(s.path("work/file"), "").unwrap();
        let e = resolve_owns(&s.path("work/file/sub/x")).unwrap_err();
        assert!(e.contains("cannot inspect"), "{e}");

        std::fs::create_dir_all(s.path("work/locked/inner")).unwrap();
        std::fs::set_permissions(
            s.path("work/locked"),
            std::fs::Permissions::from_mode(0o000),
        )
        .unwrap();
        // Root reads through mode 000, so the case cannot be produced: skip it then.
        let root = std::fs::read_dir(s.path("work/locked")).is_ok();
        let got = resolve_owns(&s.path("work/locked/inner/x"));
        std::fs::set_permissions(
            s.path("work/locked"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        if root {
            eprintln!("skipped the permission-denied case: running as root");
            return;
        }
        let e = got.unwrap_err();
        assert!(
            e.contains("cannot inspect") && e.contains("ermission"),
            "{e}"
        );
    }

    #[test]
    fn owns_symlink_escapes_are_resolved_or_refused() {
        let s = Scratch::new("escape");
        std::fs::create_dir_all(s.path("work/app/.git/hooks")).unwrap();
        std::fs::create_dir_all(s.path("outside/target")).unwrap();
        std::fs::write(s.path("work/app/Cargo.lock"), "").unwrap();
        // A link out of the work tree: the server sees the real target, not the alias, so its
        // root check applies to where the writes would really land.
        std::os::unix::fs::symlink(s.path("outside"), s.path("work/out")).unwrap();
        assert_eq!(
            resolve_owns(&s.path("work/out/target")).unwrap(),
            s.path("outside/target")
        );
        // Links onto shared state are refused like the literal path.
        std::os::unix::fs::symlink(s.path("work/app/.git"), s.path("work/hooks-link")).unwrap();
        let e = resolve_owns(&s.path("work/hooks-link/hooks")).unwrap_err();
        assert!(e.contains(".git") && e.contains("symlink"), "{e}");
        std::os::unix::fs::symlink(s.path("work/app/Cargo.lock"), s.path("work/lock")).unwrap();
        assert!(resolve_owns(&s.path("work/lock"))
            .unwrap_err()
            .contains("lockfile"));
        // A link up to a short path is too broad once resolved.
        std::os::unix::fs::symlink("/", s.path("work/root")).unwrap();
        assert!(resolve_owns(&s.path("work/root/etc"))
            .unwrap_err()
            .contains("too broad"));
        // The lexical rules still run first.
        assert!(resolve_owns(&s.path("work/../outside")).is_err());
    }

    #[test]
    fn post_builds_a_batch_with_normalised_owns() {
        let mut a = spec("build-core");
        a.capability = Some("local-build".into());
        a.owns = Some(vec!["/srv/x/repo//core/".into()]);
        a.security = Some(true);
        let mut b = spec("review-core");
        b.depends_on = Some(vec!["build-core".into(), "12".into()]);
        let r = post_req(&post(vec![a, b]), agent()).unwrap();
        assert_eq!((r.method, r.path.as_str()), ("POST", "/api/jobs"));
        let body = r.body.unwrap();
        assert_eq!(body["author"], "orch");
        assert_eq!(body["jobs"][0]["owns"], json!(["/srv/x/repo/core"]));
        assert_eq!(body["jobs"][0]["security"], true);
        assert_eq!(body["jobs"][1]["capability"], "read-only");
        assert_eq!(body["jobs"][1]["depends_on"], json!(["build-core", "12"]));
        assert_eq!(body["agent"]["model"], "m");
    }

    #[test]
    fn post_rejects_bad_batches() {
        let err = |jobs| post_req(&post(jobs), agent()).unwrap_err();
        assert!(err(vec![]).contains("1-50"));
        assert!(err(vec![spec("Bad_Name")]).contains("invalid job name"));
        assert!(err(vec![spec("aa"), spec("aa")]).contains("duplicate"));
        let mut live = spec("live-job");
        live.capability = Some("live-infra".into());
        assert!(err(vec![live]).contains("live-infra"));
        let mut lb = spec("no-owns");
        lb.capability = Some("local-build".into());
        assert!(err(vec![lb]).contains("must declare owns"));
        let mut empty = spec("empty-spec");
        empty.spec = " ".into();
        assert!(err(vec![empty]).contains("spec"));
        let mut d = spec("bad-dep");
        d.depends_on = Some(vec!["Not A Slug".into()]);
        assert!(err(vec![d]).contains("dependency"));
        let mut t = spec("bad-tier");
        t.tier = Some("huge".into());
        assert!(err(vec![t]).contains("tier"));
    }

    #[test]
    fn batch_cycles_and_self_deps_are_rejected() {
        let mut a = spec("job-a");
        a.depends_on = Some(vec!["job-b".into()]);
        let mut b = spec("job-b");
        b.depends_on = Some(vec!["job-a".into()]);
        assert!(post_req(&post(vec![a, b]), agent())
            .unwrap_err()
            .contains("cycle"));
        let mut s = spec("job-s");
        s.depends_on = Some(vec!["job-s".into()]);
        assert!(post_req(&post(vec![s]), agent())
            .unwrap_err()
            .contains("itself"));
        // A chain is fine, and a dependency outside the batch is not a cycle.
        let mut x = spec("job-x");
        x.depends_on = Some(vec!["job-y".into(), "existing-job".into()]);
        let y = spec("job-y");
        assert!(post_req(&post(vec![x, y]), agent()).is_ok());
    }

    #[test]
    fn claim_limit_bounded_and_force_overlap_sent() {
        let mk = |limit, force| JobClaimArgs {
            claimant: "o".into(),
            limit,
            force_overlap: force,
            agent_type: None,
            model: None,
            harness: None,
        };
        let b = claim_req(&mk(None, Some(true)), agent())
            .unwrap()
            .body
            .unwrap();
        assert_eq!(
            (b["limit"].clone(), b["force_overlap"].clone()),
            (json!(1), json!(true))
        );
        assert!(claim_req(&mk(Some(0), None), agent()).is_err());
        assert!(claim_req(&mk(Some(11), None), agent()).is_err());
        assert!(claim_req(&mk(Some(10), None), agent())
            .unwrap()
            .body
            .unwrap()
            .get("force_overlap")
            .is_none());
    }

    fn complete(status: &str, nonce: &str, wmodel: &str) -> JobCompleteArgs {
        JobCompleteArgs {
            job: "my-job".into(),
            nonce: nonce.into(),
            round: 1,
            status: status.into(),
            result: "r".into(),
            author: "orch".into(),
            worker_agent_type: "rust-craftsman".into(),
            worker_model: wmodel.into(),
            worker_harness: "claude".into(),
        }
    }

    #[test]
    fn complete_requires_nonce_status_and_real_worker_identity() {
        let r = complete_req(&complete("done", "abc", "sonnet")).unwrap();
        assert_eq!(r.path, "/api/jobs/my-job/complete");
        let b = r.body.unwrap();
        assert_eq!(
            (b["round"].clone(), b["worker"]["model"].clone()),
            (json!(1), json!("sonnet"))
        );
        assert!(complete_req(&complete("verified", "abc", "sonnet")).is_err());
        assert!(complete_req(&complete("done", " ", "sonnet")).is_err());
        assert!(complete_req(&complete("done", "abc", "Unknown")).is_err());
        assert!(complete_req(&complete("done", "abc", " ")).is_err());
    }

    #[test]
    fn verify_requires_verdict_and_explicit_verifier() {
        let mk = |v: &str, h: &str| JobVerifyArgs {
            job: "my-job".into(),
            verdict: v.into(),
            note: "n".into(),
            author: "orch".into(),
            verifier_agent_type: "rust-reviewer".into(),
            verifier_model: "gemini".into(),
            verifier_harness: h.into(),
            agent_type: None,
            model: None,
            harness: None,
        };
        let r = verify_req(&mk("pass", "agy"), agent()).unwrap();
        assert_eq!(r.path, "/api/jobs/my-job/verify");
        let b = r.body.unwrap();
        assert_eq!(b["verifier"]["harness"], "agy");
        // `agent` is the CALLER's identity (author alias), never the reviewer's.
        assert_eq!(b["agent"], agent());
        assert_ne!(b["agent"], b["verifier"]);
        assert!(verify_req(&mk("maybe", "agy"), agent()).is_err());
        assert!(verify_req(&mk("pass", ""), agent()).is_err());
    }

    fn admin(op: &str) -> JobAdminArgs {
        JobAdminArgs {
            op: op.into(),
            job: "my-job".into(),
            actor: "orch".into(),
            text: None,
            reason: None,
            priority: None,
            depends_on: None,
            owns: None,
            agent_type: None,
            model: None,
            harness: None,
        }
    }

    #[test]
    fn admin_ops_map_to_routes() {
        let mut a = admin("amend");
        assert!(admin_req(&a, agent()).is_err());
        a.text = Some("also touch x".into());
        let r = admin_req(&a, agent()).unwrap();
        assert_eq!(
            (r.method, r.path.as_str()),
            ("POST", "/api/jobs/my-job/amend")
        );
        assert_eq!(r.body.unwrap()["author"], "orch");

        let mut c = admin("cancel");
        c.reason = Some("obsolete".into());
        let r = admin_req(&c, agent()).unwrap();
        assert_eq!(r.path, "/api/jobs/my-job/cancel");
        assert_eq!(r.body.unwrap()["reason"], "obsolete");

        let r = admin_req(&admin("reclaim"), agent()).unwrap();
        assert_eq!(r.path, "/api/jobs/my-job/reclaim");
        assert!(r.body.unwrap().get("reason").is_none());
        assert_eq!(
            admin_req(&admin("retry"), agent()).unwrap().path,
            "/api/jobs/my-job/retry"
        );
        assert!(admin_req(&admin("explode"), agent()).is_err());
    }

    #[test]
    fn reprioritise_is_a_patch_and_validates_owns() {
        assert!(admin_req(&admin("reprioritise"), agent()).is_err());
        let mut p = admin("reprioritise");
        p.priority = Some(1);
        p.owns = Some(vec!["/srv/x/repo//a/".into()]);
        let r = admin_req(&p, agent()).unwrap();
        assert_eq!((r.method, r.path.as_str()), ("PATCH", "/api/jobs/my-job"));
        let b = r.body.unwrap();
        assert_eq!(b["owns"], json!(["/srv/x/repo/a"]));
        assert_eq!(b["priority"], 1);
        p.owns = Some(vec!["/home".into()]);
        assert!(admin_req(&p, agent()).is_err());
    }

    #[test]
    fn list_builds_filters_or_single_get() {
        let mk = |job: Option<&str>, status: Option<&str>| JobListArgs {
            job: job.map(str::to_string),
            status: status.map(str::to_string),
            ready: Some(true),
            blocked: None,
            claimed_by: Some("orch".into()),
            limit: None,
        };
        let r = list_req(&mk(None, Some("open"))).unwrap();
        assert_eq!(r.path, "/api/jobs");
        assert!(r.query.contains(&("ready", "true".to_string())));
        assert!(r.query.contains(&("claimed_by", "orch".to_string())));
        assert!(r.query.contains(&("limit", "50".to_string())));
        assert!(list_req(&mk(None, Some("zzz"))).is_err());
        let one = list_req(&mk(Some("a b"), None)).unwrap();
        assert_eq!(one.path, "/api/jobs/a%20b");
        assert!(one.query.is_empty());
    }
}
