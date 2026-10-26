//! Proposals, key/value cursors and gardener run bookkeeping.

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;

use crate::db::required_text;
use crate::error::{ApiError, ApiResult};
use crate::extract::{ApiJson, ApiPath, ApiQuery};
use crate::models::{GardenerRun, KvEntry, Proposal};
use crate::redact::Redactions;
use crate::state::AppState;
use crate::util::truncate_chars;

const PROPOSAL_COLS: &str = "id, kind, title, body, target, evidence_finding_ids, fingerprint, \
     status, decided_by, decided_at, created_at";
const PROPOSAL_KINDS: [&str; 4] = ["memory", "skill", "gotcha-note", "sentinel-signature"];
const PROPOSAL_STATUSES: [&str; 5] = ["proposed", "approved", "rejected", "applied", "expired"];

fn proposal_transition_allowed(from: &str, to: &str) -> bool {
    matches!(
        (from, to),
        ("proposed", "approved" | "rejected" | "expired")
            | ("approved", "applied" | "rejected" | "expired")
    )
}

#[derive(Deserialize)]
pub struct ProposalListQuery {
    status: Option<String>,
}

pub async fn list_proposals(
    State(st): State<AppState>,
    ApiQuery(q): ApiQuery<ProposalListQuery>,
) -> ApiResult<Json<Vec<Proposal>>> {
    if let Some(s) = &q.status {
        if !PROPOSAL_STATUSES.contains(&s.as_str()) {
            return Err(ApiError::validation("unknown proposal status"));
        }
    }
    let sql = format!(
        "SELECT {PROPOSAL_COLS} FROM proposals WHERE ($1::text IS NULL OR status = $1) \
         ORDER BY id DESC LIMIT 500"
    );
    Ok(Json(
        sqlx::query_as::<_, Proposal>(&sql)
            .bind(q.status)
            .fetch_all(&st.pool)
            .await?,
    ))
}

#[derive(Deserialize)]
pub struct ProposalCreate {
    kind: String,
    title: String,
    body: String,
    target: Option<String>,
    #[serde(default)]
    evidence_finding_ids: Vec<i64>,
    fingerprint: String,
}

pub async fn create_proposal(
    State(st): State<AppState>,
    ApiJson(p): ApiJson<ProposalCreate>,
) -> ApiResult<(StatusCode, Json<Proposal>)> {
    if !PROPOSAL_KINDS.contains(&p.kind.as_str()) {
        return Err(ApiError::validation("unknown proposal kind"));
    }
    let title = required_text("title", &p.title, 300)?;
    let body = required_text("body", &p.body, 50_000)?;
    let fingerprint = required_text("fingerprint", &p.fingerprint, 200)?;
    let mut red = Redactions::new();
    let title = red.text("title", title);
    let body = red.text("body", body);
    let fingerprint = red.text("fingerprint", fingerprint);
    let target = red
        .opt("target", p.target)
        .map(|t| truncate_chars(t.trim(), 300));
    red.log_only();
    let sql = format!(
        "INSERT INTO proposals (kind, title, body, target, evidence_finding_ids, fingerprint) \
         VALUES ($1,$2,$3,$4,$5,$6) RETURNING {PROPOSAL_COLS}"
    );
    // A known fingerprint surfaces as 409 through the unique-violation mapping.
    let row = sqlx::query_as::<_, Proposal>(&sql)
        .bind(&p.kind)
        .bind(&title)
        .bind(&body)
        .bind(&target)
        .bind(&p.evidence_finding_ids)
        .bind(&fingerprint)
        .fetch_one(&st.pool)
        .await?;
    Ok((StatusCode::CREATED, Json(row)))
}

#[derive(Deserialize)]
pub struct ProposalPatch {
    status: String,
    actor: String,
}

pub async fn patch_proposal(
    State(st): State<AppState>,
    ApiPath(id): ApiPath<i64>,
    ApiJson(p): ApiJson<ProposalPatch>,
) -> ApiResult<Json<Proposal>> {
    let actor = required_text("actor", &p.actor, 120)?;
    let mut red = Redactions::new();
    let actor = red.text("actor", actor);
    red.log_only();
    if !PROPOSAL_STATUSES.contains(&p.status.as_str()) {
        return Err(ApiError::validation("unknown proposal status"));
    }
    let mut tx = st.pool.begin().await?;
    let from: String = sqlx::query_scalar("SELECT status FROM proposals WHERE id = $1 FOR UPDATE")
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| ApiError::not_found("proposal not found"))?;
    if !proposal_transition_allowed(&from, &p.status) {
        return Err(ApiError::conflict(format!(
            "illegal proposal transition {from} -> {}",
            p.status
        )));
    }
    let sql = format!(
        "UPDATE proposals SET status = $2, decided_by = $3, decided_at = now() \
         WHERE id = $1 RETURNING {PROPOSAL_COLS}"
    );
    let row = sqlx::query_as::<_, Proposal>(&sql)
        .bind(id)
        .bind(&p.status)
        .bind(&actor)
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Json(row))
}

fn valid_key(k: &str) -> ApiResult<()> {
    if k.is_empty() || k.chars().count() > 200 {
        return Err(ApiError::validation("key must be 1-200 characters"));
    }
    Ok(())
}

pub async fn kv_get(
    State(st): State<AppState>,
    ApiPath(k): ApiPath<String>,
) -> ApiResult<Json<KvEntry>> {
    valid_key(&k)?;
    sqlx::query_as::<_, KvEntry>("SELECT k, v, updated_at FROM kv WHERE k = $1")
        .bind(&k)
        .fetch_optional(&st.pool)
        .await?
        .map(Json)
        .ok_or_else(|| ApiError::not_found("key not found"))
}

#[derive(Deserialize)]
pub struct KvPut {
    v: serde_json::Value,
}

pub async fn kv_put(
    State(st): State<AppState>,
    ApiPath(k): ApiPath<String>,
    ApiJson(p): ApiJson<KvPut>,
) -> ApiResult<Json<KvEntry>> {
    valid_key(&k)?;
    // `idem:*` records back the Idempotency-Key replays of the job routes; a token holder
    // must not be able to overwrite them through the generic key/value route.
    if k.starts_with("idem:") {
        return Err(ApiError::validation(
            "keys starting with 'idem:' are reserved",
        ));
    }
    let mut red = Redactions::new();
    let v = red.json("v", p.v);
    red.log_only();
    let row = sqlx::query_as::<_, KvEntry>(
        "INSERT INTO kv (k, v) VALUES ($1,$2) \
         ON CONFLICT (k) DO UPDATE SET v = EXCLUDED.v, updated_at = now() \
         RETURNING k, v, updated_at",
    )
    .bind(&k)
    .bind(&v)
    .fetch_one(&st.pool)
    .await?;
    Ok(Json(row))
}

const RUN_COLS: &str = "id, started_at, finished_at, ok, stage, topics_swept, topics_distilled, \
     findings_written, quarantined, notes";

#[derive(Deserialize, Default)]
pub struct RunCreate {
    stage: Option<String>,
}

pub async fn create_run(
    State(st): State<AppState>,
    body: axum::body::Bytes,
) -> ApiResult<(StatusCode, Json<GardenerRun>)> {
    let p: RunCreate = if body.iter().all(u8::is_ascii_whitespace) {
        RunCreate::default()
    } else {
        serde_json::from_slice(&body).map_err(|_| ApiError::validation("invalid JSON body"))?
    };
    let mut red = Redactions::new();
    let stage = red
        .opt("stage", p.stage)
        .map(|s| truncate_chars(s.trim(), 80));
    red.log_only();
    let sql = format!("INSERT INTO gardener_runs (stage) VALUES ($1) RETURNING {RUN_COLS}");
    let row = sqlx::query_as::<_, GardenerRun>(&sql)
        .bind(stage)
        .fetch_one(&st.pool)
        .await?;
    Ok((StatusCode::CREATED, Json(row)))
}

#[derive(Deserialize)]
pub struct RunPatch {
    ok: Option<bool>,
    stage: Option<String>,
    finished: Option<bool>,
    topics_swept: Option<i32>,
    topics_distilled: Option<i32>,
    findings_written: Option<i32>,
    quarantined: Option<i32>,
    notes: Option<String>,
}

pub async fn patch_run(
    State(st): State<AppState>,
    ApiPath(id): ApiPath<i64>,
    ApiJson(p): ApiJson<RunPatch>,
) -> ApiResult<Json<GardenerRun>> {
    // `ok` or `finished` closes the run.
    let finish = p.ok.is_some() || p.finished == Some(true);
    let mut red = Redactions::new();
    let stage = red
        .opt("stage", p.stage)
        .map(|s| truncate_chars(s.trim(), 80));
    let notes = red
        .opt("notes", p.notes)
        .map(|n| truncate_chars(&n, 10_000));
    red.log_only();
    let sql = format!(
        "UPDATE gardener_runs SET ok = coalesce($2, ok), stage = coalesce($3, stage), \
           topics_swept = coalesce($4, topics_swept), topics_distilled = coalesce($5, topics_distilled), \
           findings_written = coalesce($6, findings_written), quarantined = coalesce($7, quarantined), \
           notes = coalesce($8, notes), \
           finished_at = CASE WHEN $9 THEN coalesce(finished_at, now()) ELSE finished_at END \
         WHERE id = $1 RETURNING {RUN_COLS}"
    );
    sqlx::query_as::<_, GardenerRun>(&sql)
        .bind(id)
        .bind(p.ok)
        .bind(stage)
        .bind(p.topics_swept)
        .bind(p.topics_distilled)
        .bind(p.findings_written)
        .bind(p.quarantined)
        .bind(notes)
        .bind(finish)
        .fetch_optional(&st.pool)
        .await?
        .map(Json)
        .ok_or_else(|| ApiError::not_found("run not found"))
}
