//! Topic routes and the manual topic state machine.

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;

use crate::db::{
    add_topic_event, fetch_topic, required_text, resolve_identity, resolve_topic_id,
    valid_topic_name, TOPIC_KINDS, TOPIC_STATUSES,
};
use crate::error::{ApiError, ApiResult};
use crate::extract::{ApiJson, ApiPath, ApiQuery};
use crate::models::{AgentIdent, Topic, TopicEvent, TOPIC_COLS};
use crate::redact::Redactions;
use crate::state::AppState;

/// Manual (PATCH) transitions. `quiet` is entered by the sweep only.
/// `resolved -> archived` additionally needs the topic distilled through its last message.
pub fn transition_allowed(from: &str, to: &str) -> bool {
    matches!(
        (from, to),
        ("active", "needs-human")
            | ("active", "resolved")
            | ("quiet", "active")
            | ("quiet", "needs-human")
            | ("quiet", "resolved")
            | ("needs-human", "active")
            | ("needs-human", "resolved")
            | ("resolved", "active")
            | ("resolved", "needs-human")
            | ("resolved", "archived")
            | ("archived", "active")
            | ("archived", "needs-human")
    )
}

#[derive(Deserialize)]
pub struct ListQuery {
    status: Option<String>,
    kind: Option<String>,
}

pub async fn list(
    State(st): State<AppState>,
    ApiQuery(q): ApiQuery<ListQuery>,
) -> ApiResult<Json<Vec<Topic>>> {
    let status = q.status.as_deref().unwrap_or("open");
    if !["open", "all"].contains(&status) && !TOPIC_STATUSES.contains(&status) {
        return Err(ApiError::validation("unknown status filter"));
    }
    if let Some(k) = &q.kind {
        if !TOPIC_KINDS.contains(&k.as_str()) {
            return Err(ApiError::validation("unknown kind filter"));
        }
    }
    let sql = format!(
        "SELECT {TOPIC_COLS} FROM topics t \
         WHERE ($1 = 'all' OR ($1 = 'open' AND t.status <> 'archived') OR t.status = $1) \
           AND ($2::text IS NULL OR t.kind = $2) \
         ORDER BY t.last_activity_at DESC, t.id DESC"
    );
    let rows = sqlx::query_as::<_, Topic>(&sql)
        .bind(status)
        .bind(q.kind)
        .fetch_all(&st.pool)
        .await?;
    Ok(Json(rows))
}

#[derive(Deserialize)]
pub struct CreatePayload {
    name: String,
    title: String,
    description: Option<String>,
    author: String,
    kind: Option<String>,
    agent: Option<AgentIdent>,
}

pub async fn create(
    State(st): State<AppState>,
    ApiJson(p): ApiJson<CreatePayload>,
) -> ApiResult<(StatusCode, Json<Topic>)> {
    let name = p.name.trim().to_string();
    if !valid_topic_name(&name) {
        return Err(ApiError::validation(
            "name must match ^[a-z][a-z0-9-]{1,79}$",
        ));
    }
    if crate::db::reserved_topic_name(&name) {
        return Err(ApiError::validation(format!(
            "the name '{name}' is reserved"
        )));
    }
    let title = required_text("title", &p.title, 300)?;
    let author = required_text("author", &p.author, 120)?;
    let kind = p.kind.unwrap_or_else(|| "task".into());
    if !TOPIC_KINDS.contains(&kind.as_str()) {
        return Err(ApiError::validation("unknown topic kind"));
    }
    if kind == "job" {
        return Err(ApiError::validation(
            "job topics are created through POST /api/jobs",
        ));
    }
    let description = p
        .description
        .map(|d| d.trim().to_string())
        .filter(|d| !d.is_empty());
    let mut red = Redactions::new();
    let title = red.text("title", title);
    let author = red.text("author", author);
    let description = red.opt("description", description);
    let agent = red.agent(p.agent);

    let mut tx = st.pool.begin().await?;
    let exists: Option<i64> = sqlx::query_scalar("SELECT id FROM topics WHERE name = $1")
        .bind(&name)
        .fetch_optional(&mut *tx)
        .await?;
    if exists.is_some() {
        return Err(ApiError::conflict(format!("topic '{name}' already exists")));
    }
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO topics (name, title, description, created_by, kind) \
         VALUES ($1,$2,$3,$4,$5) RETURNING id",
    )
    .bind(&name)
    .bind(&title)
    .bind(&description)
    .bind(&author)
    .bind(&kind)
    .fetch_one(&mut *tx)
    .await?;
    resolve_identity(&mut tx, &author, agent.as_ref(), Some(id)).await?;
    add_topic_event(
        &mut tx,
        id,
        "created",
        &author,
        serde_json::json!({"kind": kind}),
    )
    .await?;
    red.record(&mut tx, Some(id), &author).await?;
    let topic = fetch_topic(&mut tx, id).await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(topic)))
}

pub async fn get(
    State(st): State<AppState>,
    ApiPath(t): ApiPath<String>,
) -> ApiResult<Json<Topic>> {
    let mut conn = st.pool.acquire().await?;
    let id = resolve_topic_id(&mut conn, &t).await?;
    Ok(Json(fetch_topic(&mut conn, id).await?))
}

#[derive(Deserialize)]
pub struct PatchPayload {
    title: Option<String>,
    description: Option<String>,
    status: Option<String>,
    outcome: Option<String>,
    actor: String,
}

pub async fn patch(
    State(st): State<AppState>,
    ApiPath(t): ApiPath<String>,
    ApiJson(p): ApiJson<PatchPayload>,
) -> ApiResult<Json<Topic>> {
    let actor = required_text("actor", &p.actor, 120)?;
    let title = p
        .title
        .as_deref()
        .map(|v| required_text("title", v, 300))
        .transpose()?;
    if let Some(s) = &p.status {
        if !TOPIC_STATUSES.contains(&s.as_str()) {
            return Err(ApiError::validation("unknown status"));
        }
    }
    let mut red = Redactions::new();
    let actor = red.text("actor", actor);
    let title = red.opt("title", title);
    let p = PatchPayload {
        description: red.opt("description", p.description),
        outcome: red.opt("outcome", p.outcome),
        ..p
    };

    let mut tx = st.pool.begin().await?;
    // A status change may touch a job topic: take the global jobs lock FIRST.
    if p.status.is_some() {
        crate::jobs::lock_jobs(&mut tx).await?;
    }
    let id = resolve_topic_id(&mut tx, &t).await?;
    red.record(&mut tx, Some(id), &actor).await?;
    let (from, distilled_through): (String, i64) =
        sqlx::query_as("SELECT status, distilled_through FROM topics WHERE id = $1 FOR UPDATE")
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;

    if let Some(to) = &p.status {
        if !transition_allowed(&from, to) {
            return Err(ApiError::conflict(format!(
                "illegal topic transition {from} -> {to}"
            )));
        }
        if to == "resolved" && crate::jobs::topic_has_active_job(&mut tx, id).await? {
            return Err(ApiError::conflict(
                "the job on this topic is open, claimed or done; cancel, fail or verify it before resolving",
            ));
        }
        if to == "archived" {
            let max_msg: i64 =
                sqlx::query_scalar("SELECT coalesce(max(id), 0) FROM messages WHERE topic_id = $1")
                    .bind(id)
                    .fetch_one(&mut *tx)
                    .await?;
            if distilled_through < max_msg {
                return Err(ApiError::conflict(
                    "topic must be distilled through its last message before archiving",
                ));
            }
        }
        let outcome = if to == "resolved" {
            p.outcome
                .as_deref()
                .map(str::trim)
                .filter(|o| !o.is_empty())
        } else {
            None
        };
        sqlx::query(
            "UPDATE topics SET status = $2, \
               outcome = CASE WHEN $2 = 'resolved' THEN coalesce($3, outcome) \
                              WHEN $2 = 'active' THEN NULL ELSE outcome END, \
               last_activity_at = CASE WHEN $2 = 'active' THEN now() ELSE last_activity_at END, \
               updated_at = now() WHERE id = $1",
        )
        .bind(id)
        .bind(to)
        .bind(outcome)
        .execute(&mut *tx)
        .await?;
        let event = match to.as_str() {
            "active" => "reopened",
            other => other,
        };
        add_topic_event(
            &mut tx,
            id,
            event,
            &actor,
            serde_json::json!({"from": from, "to": to, "outcome": outcome}),
        )
        .await?;
    } else if p.outcome.is_some() && from == "resolved" {
        sqlx::query("UPDATE topics SET outcome = NULLIF($2, ''), updated_at = now() WHERE id = $1")
            .bind(id)
            .bind(p.outcome.as_deref().map(str::trim))
            .execute(&mut *tx)
            .await?;
    }

    if title.is_some() || p.description.is_some() {
        sqlx::query(
            "UPDATE topics SET title = coalesce($2, title), \
               description = CASE WHEN $3::text IS NULL THEN description ELSE NULLIF($3, '') END, updated_at = now() WHERE id = $1",
        )
        .bind(id)
        .bind(&title)
        .bind(p.description.as_deref().map(str::trim))
        .execute(&mut *tx)
        .await?;
    }

    let topic = fetch_topic(&mut tx, id).await?;
    tx.commit().await?;
    Ok(Json(topic))
}

#[derive(Deserialize)]
pub struct DeleteQuery {
    confirm: Option<String>,
}

/// Hard delete: token-gated by the auth layer and confirmed by name, so a stray
/// DELETE (or a wrong id) cannot remove a topic and its whole history.
/// CEILING: the shared token can still delete anything; per-actor tokens are the upgrade path.
pub async fn delete(
    State(st): State<AppState>,
    ApiPath(t): ApiPath<String>,
    ApiQuery(q): ApiQuery<DeleteQuery>,
) -> ApiResult<StatusCode> {
    let mut tx = st.pool.begin().await?;
    crate::jobs::lock_jobs(&mut tx).await?;
    let id = resolve_topic_id(&mut tx, &t).await?;
    let name: String = sqlx::query_scalar("SELECT name FROM topics WHERE id = $1")
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
    if q.confirm.as_deref() != Some(name.as_str()) {
        return Err(ApiError::validation("confirm must equal the topic name"));
    }
    // A job that others depend on cannot be deleted: name the dependents.
    let dependents = crate::jobs::dependents_of(&mut tx, id).await?;
    if !dependents.is_empty() {
        return Err(ApiError::conflict(format!(
            "this job is a dependency of: {}; remove those dependencies first",
            dependents.join(", ")
        )));
    }
    tracing::info!(topic = %name, "topic deleted");
    sqlx::query("DELETE FROM topics WHERE id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn events(
    State(st): State<AppState>,
    ApiPath(t): ApiPath<String>,
) -> ApiResult<Json<Vec<TopicEvent>>> {
    let mut conn = st.pool.acquire().await?;
    let id = resolve_topic_id(&mut conn, &t).await?;
    let rows = sqlx::query_as::<_, TopicEvent>(
        "SELECT id, topic_id, event, actor, detail, at FROM topic_events \
         WHERE topic_id = $1 ORDER BY id ASC",
    )
    .bind(id)
    .fetch_all(&mut *conn)
    .await?;
    Ok(Json(rows))
}

#[cfg(test)]
mod tests {
    use super::transition_allowed as ok;

    #[test]
    fn matrix() {
        assert!(ok("active", "resolved"));
        assert!(!ok("active", "quiet"));
        assert!(!ok("active", "archived"));
        assert!(!ok("quiet", "archived"));
        assert!(!ok("needs-human", "quiet"));
        assert!(ok("resolved", "archived"));
        assert!(!ok("archived", "resolved"));
        assert!(!ok("active", "active"));
    }
}
