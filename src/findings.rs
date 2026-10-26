//! Finding routes: list, detail, guarded status transitions, gardener batch upsert.

use axum::extract::State;
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::db::{required_text, resolve_topic_id, FINDING_KINDS, SEVERITIES};
use crate::error::{ApiError, ApiResult};
use crate::extract::{ApiJson, ApiPath, ApiQuery};
use crate::models::{Finding, FindingDetail, FindingEvent, Ts, FINDING_COLS};
use crate::redact::Redactions;
use crate::state::AppState;
use crate::util::{clamp_limit, clean_list, content_hash, truncate_chars};

pub const FINDING_STATUSES: [&str; 7] = [
    "open",
    "confirmed",
    "fixed",
    "false_positive",
    "wontfix",
    "superseded",
    "expired",
];
const PROVENANCES: [&str; 5] = ["agent", "orchestrator", "gardener", "human", "external"];
const ACTOR_CLASSES: [&str; 4] = ["orchestrator", "human", "gardener", "system"];
/// Outcomes that feed the quality score: only a human or the orchestrator may judge.
const JUDGING: [&str; 3] = ["confirmed", "fixed", "false_positive"];

/// Legal status transitions (any state may be reopened to `open`).
pub fn finding_transition_allowed(from: &str, to: &str) -> bool {
    if from == to {
        return false;
    }
    if to == "open" {
        return true;
    }
    match from {
        "open" => FINDING_STATUSES.contains(&to),
        "confirmed" => matches!(to, "fixed" | "wontfix" | "superseded"),
        _ => false,
    }
}

#[derive(Deserialize)]
pub struct ListQuery {
    status: Option<String>,
    severity: Option<String>,
    kind: Option<String>,
    subject: Option<String>,
    topic: Option<String>,
    after: Option<i64>,
    limit: Option<i64>,
    updated_since: Option<Ts>,
}

pub async fn list(
    State(st): State<AppState>,
    ApiQuery(q): ApiQuery<ListQuery>,
) -> ApiResult<Json<Vec<Finding>>> {
    let mut conn = st.pool.acquire().await?;
    let tid = match &q.topic {
        Some(t) => Some(resolve_topic_id(&mut conn, t).await?),
        None => None,
    };
    let sql = format!(
        "SELECT {FINDING_COLS} FROM findings f JOIN topics t ON t.id = f.topic_id \
         WHERE ($1::text IS NULL OR f.status = $1) AND ($2::text IS NULL OR f.severity = $2) \
           AND ($3::text IS NULL OR f.kind = $3) AND ($4::text IS NULL OR $4 = ANY(f.subjects)) \
           AND ($5::bigint IS NULL OR f.topic_id = $5) AND f.id > $6 \
           AND ($7::timestamptz IS NULL OR f.updated_at >= $7) \
         ORDER BY f.id ASC LIMIT $8"
    );
    let rows = sqlx::query_as::<_, Finding>(&sql)
        .bind(q.status)
        .bind(q.severity)
        .bind(q.kind)
        .bind(q.subject)
        .bind(tid)
        .bind(q.after.unwrap_or(0))
        .bind(q.updated_since)
        .bind(clamp_limit(q.limit, 100, 500))
        .fetch_all(&mut *conn)
        .await?;
    Ok(Json(rows))
}

async fn fetch_finding(conn: &mut sqlx::PgConnection, id: i64) -> ApiResult<Finding> {
    let sql = format!(
        "SELECT {FINDING_COLS} FROM findings f JOIN topics t ON t.id = f.topic_id WHERE f.id = $1"
    );
    sqlx::query_as::<_, Finding>(&sql)
        .bind(id)
        .fetch_optional(conn)
        .await?
        .ok_or_else(|| ApiError::not_found("finding not found"))
}

pub async fn get(
    State(st): State<AppState>,
    ApiPath(id): ApiPath<i64>,
) -> ApiResult<Json<FindingDetail>> {
    let mut conn = st.pool.acquire().await?;
    let finding = fetch_finding(&mut conn, id).await?;
    let events = sqlx::query_as::<_, FindingEvent>(
        "SELECT id, finding_id, from_status, to_status, actor, actor_class, note, at \
         FROM finding_events WHERE finding_id = $1 ORDER BY id ASC",
    )
    .bind(id)
    .fetch_all(&mut *conn)
    .await?;
    Ok(Json(FindingDetail { finding, events }))
}

#[derive(Deserialize)]
pub struct PatchPayload {
    status: Option<String>,
    note: Option<String>,
    actor: String,
    actor_class: String,
    duplicate_of: Option<i64>,
    resolved_subjects: Option<serde_json::Value>,
    unresolved_subjects: Option<Vec<String>>,
    graph_synced: Option<bool>,
}

pub async fn patch(
    State(st): State<AppState>,
    ApiPath(id): ApiPath<i64>,
    ApiJson(p): ApiJson<PatchPayload>,
) -> ApiResult<Json<Finding>> {
    let actor = required_text("actor", &p.actor, 120)?;
    if !ACTOR_CLASSES.contains(&p.actor_class.as_str()) {
        return Err(ApiError::validation("unknown actor_class"));
    }
    if let Some(s) = &p.status {
        if !FINDING_STATUSES.contains(&s.as_str()) {
            return Err(ApiError::validation("unknown finding status"));
        }
    }
    if let Some(v) = &p.resolved_subjects {
        if !v.is_array() {
            return Err(ApiError::validation(
                "resolved_subjects must be a JSON array",
            ));
        }
    }
    let mut red = Redactions::new();
    let actor = red.text("actor", actor);
    let p = PatchPayload {
        note: red.opt("note", p.note),
        resolved_subjects: p
            .resolved_subjects
            .map(|v| red.json("resolved_subjects", v)),
        unresolved_subjects: p
            .unresolved_subjects
            .map(|u| red.list("unresolved_subjects", u)),
        ..p
    };
    let unresolved = p.unresolved_subjects.map(|u| clean_list(Some(u), 64, 120));

    let mut tx = st.pool.begin().await?;
    let (from, topic_id): (String, i64) =
        sqlx::query_as("SELECT status, topic_id FROM findings WHERE id = $1 FOR UPDATE")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(|| ApiError::not_found("finding not found"))?;
    red.record(&mut tx, Some(topic_id), &actor).await?;

    if let Some(to) = &p.status {
        if p.actor_class == "gardener"
            && (JUDGING.contains(&to.as_str()) || JUDGING.contains(&from.as_str()))
        {
            return Err(ApiError::conflict(
                "the gardener may not set, or undo, confirmed, fixed or false_positive",
            ));
        }
        if !finding_transition_allowed(&from, to) {
            return Err(ApiError::conflict(format!(
                "illegal finding transition {from} -> {to}"
            )));
        }
        sqlx::query(
            "UPDATE findings SET status = $2, status_changed_at = now(), \
               status_changed_by = $3, updated_at = now() WHERE id = $1",
        )
        .bind(id)
        .bind(to)
        .bind(&actor)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO finding_events (finding_id, from_status, to_status, actor, actor_class, note) \
             VALUES ($1,$2,$3,$4,$5,$6)",
        )
        .bind(id)
        .bind(&from)
        .bind(to)
        .bind(&actor)
        .bind(&p.actor_class)
        .bind(p.note.as_deref().map(|n| truncate_chars(n.trim(), 2000)))
        .execute(&mut *tx)
        .await?;
    }

    if let Some(dup) = p.duplicate_of {
        if dup == id {
            return Err(ApiError::validation("a finding cannot duplicate itself"));
        }
        sqlx::query("UPDATE findings SET duplicate_of = $2 WHERE id = $1")
            .bind(id)
            .bind(dup)
            .execute(&mut *tx)
            .await?;
    }
    if let Some(rs) = &p.resolved_subjects {
        sqlx::query("UPDATE findings SET resolved_subjects = $2 WHERE id = $1")
            .bind(id)
            .bind(rs)
            .execute(&mut *tx)
            .await?;
    }
    if let Some(u) = &unresolved {
        sqlx::query("UPDATE findings SET unresolved_subjects = $2 WHERE id = $1")
            .bind(id)
            .bind(u)
            .execute(&mut *tx)
            .await?;
    }
    if p.graph_synced == Some(true) {
        sqlx::query("UPDATE findings SET graph_synced_at = now() WHERE id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
    }
    // Any PATCH bumps updated_at: incremental consumers (the gardener among them) key on it.
    sqlx::query("UPDATE findings SET updated_at = now() WHERE id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;

    let f = fetch_finding(&mut tx, id).await?;
    tx.commit().await?;
    Ok(Json(f))
}

#[derive(Deserialize)]
pub struct BatchItem {
    kind: String,
    severity: Option<String>,
    claim: String,
    evidence: Option<String>,
    subjects: Option<Vec<String>>,
    task_kind: Option<String>,
    confidence: Option<f32>,
    provenance: String,
    message_id: Option<i64>,
    resolved_subjects: Option<serde_json::Value>,
    unresolved_subjects: Option<Vec<String>>,
}

#[derive(Deserialize)]
pub struct BatchPayload {
    findings: Vec<BatchItem>,
}

#[derive(Serialize)]
pub struct BatchResult {
    created: Vec<i64>,
    reinforced: Vec<i64>,
}

pub async fn batch_upsert(
    State(st): State<AppState>,
    ApiPath(t): ApiPath<String>,
    ApiJson(p): ApiJson<BatchPayload>,
) -> ApiResult<Json<BatchResult>> {
    if p.findings.len() > 200 {
        return Err(ApiError::validation("at most 200 findings per batch"));
    }
    let mut tx = st.pool.begin().await?;
    let tid = resolve_topic_id(&mut tx, &t).await?;
    let mut red = Redactions::new();
    let mut out = BatchResult {
        created: Vec::new(),
        reinforced: Vec::new(),
    };
    for it in p.findings {
        if !FINDING_KINDS.contains(&it.kind.as_str()) {
            return Err(ApiError::validation("unknown finding kind"));
        }
        if let Some(s) = &it.severity {
            if !SEVERITIES.contains(&s.as_str()) {
                return Err(ApiError::validation(
                    "severity must be BLOCKER, SHOULD or NICE",
                ));
            }
        }
        if !PROVENANCES.contains(&it.provenance.as_str()) {
            return Err(ApiError::validation("unknown provenance"));
        }
        if let Some(c) = it.confidence {
            if !(0.0..=1.0).contains(&c) {
                return Err(ApiError::validation("confidence must be within 0..1"));
            }
        }
        let resolved = match it.resolved_subjects {
            Some(v) if v.is_array() => v,
            Some(_) => {
                return Err(ApiError::validation(
                    "resolved_subjects must be a JSON array",
                ))
            }
            None => serde_json::json!([]),
        };
        let claim = required_text("claim", &it.claim, 20_000)?;
        let claim = red.text("claim", claim);
        let resolved = red.json("resolved_subjects", resolved);
        // Redact first, then clean/truncate, so a cut token cannot survive as a fragment.
        let subjects = clean_list(red.opt_list("subjects", it.subjects), 32, 120);
        let unresolved = clean_list(
            red.opt_list("unresolved_subjects", it.unresolved_subjects),
            64,
            120,
        );
        let evidence = red
            .opt("evidence", it.evidence)
            .map(|e| e.trim().to_string())
            .filter(|e| !e.is_empty());
        let task_kind = red
            .opt("task_kind", it.task_kind)
            .map(|k| truncate_chars(k.trim(), 80))
            .filter(|k| !k.is_empty());
        let (id, inserted): (i64, bool) = sqlx::query_as(
            "INSERT INTO findings (topic_id, message_id, identity_id, kind, severity, claim, \
               evidence, subjects, resolved_subjects, unresolved_subjects, task_kind, confidence, \
               provenance, content_hash) \
             VALUES ($1, $2, (SELECT identity_id FROM messages WHERE id = $2), \
                     $3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13) \
             ON CONFLICT (topic_id, content_hash) DO UPDATE \
               SET reinforce_count = findings.reinforce_count + 1, updated_at = now() \
             RETURNING id, (xmax = 0) AS inserted",
        )
        .bind(tid)
        .bind(it.message_id)
        .bind(&it.kind)
        .bind(&it.severity)
        .bind(&claim)
        .bind(&evidence)
        .bind(&subjects)
        .bind(&resolved)
        .bind(&unresolved)
        .bind(&task_kind)
        .bind(it.confidence)
        .bind(&it.provenance)
        .bind(content_hash(&claim))
        .fetch_one(&mut *tx)
        .await?;
        if inserted {
            out.created.push(id);
        } else {
            out.reinforced.push(id);
        }
    }
    red.record(&mut tx, Some(tid), "gardener").await?;
    tx.commit().await?;
    Ok(Json(out))
}

#[cfg(test)]
mod tests {
    use super::finding_transition_allowed as ok;

    #[test]
    fn transitions() {
        assert!(ok("open", "confirmed"));
        assert!(ok("open", "false_positive"));
        assert!(ok("confirmed", "fixed"));
        assert!(!ok("confirmed", "false_positive"));
        assert!(!ok("fixed", "confirmed"));
        assert!(ok("fixed", "open"));
        assert!(!ok("open", "open"));
    }
}
