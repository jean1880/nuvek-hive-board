//! Shared database helpers: topic resolution, events, identity resolution.

use sqlx::PgConnection;

use crate::error::{ApiError, ApiResult};
use crate::models::{AgentIdent, Topic, TOPIC_COLS};
use crate::util::truncate_chars;

pub const TOPIC_KINDS: [&str; 7] = [
    "task", "incident", "report", "design", "review", "research", "job",
];
pub const TOPIC_STATUSES: [&str; 5] = ["active", "quiet", "needs-human", "resolved", "archived"];
pub const MESSAGE_KINDS: [&str; 9] = [
    "note", "finding", "decision", "gotcha", "question", "verdict", "brief", "system", "progress",
];
pub const FINDING_KINDS: [&str; 5] = ["finding", "decision", "gotcha", "verdict", "question"];
pub const SEVERITIES: [&str; 3] = ["BLOCKER", "SHOULD", "NICE"];

/// `^[a-z][a-z0-9-]{1,79}$`, so a name can never be read as a numeric id.
pub fn valid_topic_name(name: &str) -> bool {
    let n = name.chars().count();
    let mut chars = name.chars();
    let first_ok = chars.next().is_some_and(|c| c.is_ascii_lowercase());
    first_ok
        && (2..=80).contains(&n)
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// Names that would shadow a fixed route segment (`/api/jobs/claim`): rejected for every
/// topic and job, so a slug can never be unreachable.
pub fn reserved_topic_name(name: &str) -> bool {
    name == "claim"
}

/// Required, trimmed, length-capped text field.
pub fn required_text(field: &str, value: &str, max_chars: usize) -> ApiResult<String> {
    let t = value.trim();
    if t.contains('\0') {
        return Err(ApiError::validation(format!(
            "{field} must not contain NUL characters"
        )));
    }
    if t.is_empty() {
        return Err(ApiError::validation(format!("{field} must not be empty")));
    }
    if t.chars().count() > max_chars {
        return Err(ApiError::validation(format!(
            "{field} must be at most {max_chars} characters"
        )));
    }
    Ok(t.to_string())
}

/// Resolve `{t}` (slug or numeric id) to a topic id.
pub async fn resolve_topic_id(conn: &mut PgConnection, t: &str) -> ApiResult<i64> {
    let as_id = t.trim().parse::<i64>().ok();
    let id: Option<i64> = sqlx::query_scalar(
        "SELECT id FROM topics WHERE name = $1 OR ($2::bigint IS NOT NULL AND id = $2) \
         ORDER BY (name = $1) DESC LIMIT 1",
    )
    .bind(t.trim())
    .bind(as_id)
    .fetch_optional(conn)
    .await?;
    id.ok_or_else(|| ApiError::not_found(format!("topic '{}' not found", truncate_chars(t, 80))))
}

pub async fn fetch_topic(conn: &mut PgConnection, id: i64) -> ApiResult<Topic> {
    let sql = format!("SELECT {TOPIC_COLS} FROM topics t WHERE t.id = $1");
    sqlx::query_as::<_, Topic>(&sql)
        .bind(id)
        .fetch_optional(conn)
        .await?
        .ok_or_else(|| ApiError::not_found("topic not found"))
}

pub async fn add_topic_event(
    conn: &mut PgConnection,
    topic_id: i64,
    event: &str,
    actor: &str,
    detail: serde_json::Value,
) -> ApiResult<()> {
    sqlx::query("INSERT INTO topic_events (topic_id, event, actor, detail) VALUES ($1,$2,$3,$4)")
        .bind(topic_id)
        .bind(event)
        .bind(actor)
        .bind(detail)
        .execute(conn)
        .await?;
    Ok(())
}

pub fn clean_part(s: &str) -> String {
    let t = truncate_chars(s.trim(), 80);
    if t.is_empty() {
        "unknown".to_string()
    } else {
        t
    }
}

pub async fn upsert_identity(
    conn: &mut PgConnection,
    agent_type: &str,
    model: &str,
    harness: &str,
) -> ApiResult<i64> {
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO identities (agent_type, model, harness) VALUES ($1,$2,$3) \
         ON CONFLICT (agent_type, model, harness) DO UPDATE SET agent_type = EXCLUDED.agent_type \
         RETURNING id",
    )
    .bind(agent_type)
    .bind(model)
    .bind(harness)
    .fetch_one(conn)
    .await?;
    Ok(id)
}

/// Resolve the identity for a write. First write wins per alias: a later write
/// whose `agent` object disagrees keeps the original identity and records a
/// `topic_events` `alias-mismatch` row (when a topic is in scope). No error.
pub async fn resolve_identity(
    conn: &mut PgConnection,
    alias: &str,
    agent: Option<&AgentIdent>,
    topic_id: Option<i64>,
) -> ApiResult<i64> {
    let alias = truncate_chars(alias.trim(), 120);
    let given = agent.map(|a| {
        (
            clean_part(&a.agent_type),
            clean_part(&a.model),
            clean_part(&a.harness),
        )
    });

    let existing: Option<(i64, String, String, String)> = sqlx::query_as(
        "SELECT i.id, i.agent_type, i.model, i.harness FROM aliases a \
         JOIN identities i ON i.id = a.identity_id WHERE a.alias = $1",
    )
    .bind(&alias)
    .fetch_optional(&mut *conn)
    .await?;

    if let Some((id, at, md, hn)) = existing {
        if let (Some(g), Some(tid)) = (&given, topic_id) {
            if (g.0.as_str(), g.1.as_str(), g.2.as_str()) != (at.as_str(), md.as_str(), hn.as_str())
            {
                add_topic_event(
                    conn,
                    tid,
                    "alias-mismatch",
                    &alias,
                    serde_json::json!({
                        "kept": {"agent_type": at, "model": md, "harness": hn},
                        "offered": {"agent_type": g.0, "model": g.1, "harness": g.2},
                    }),
                )
                .await?;
            }
        }
        return Ok(id);
    }

    let (at, md, hn) = given.unwrap_or_else(|| {
        (
            "unknown".to_string(),
            "unknown".to_string(),
            "unknown".to_string(),
        )
    });
    let wanted = upsert_identity(&mut *conn, &at, &md, &hn).await?;
    sqlx::query(
        "INSERT INTO aliases (alias, identity_id) VALUES ($1,$2) ON CONFLICT (alias) DO NOTHING",
    )
    .bind(&alias)
    .bind(wanted)
    .execute(&mut *conn)
    .await?;
    // A concurrent writer may have won the alias; honour first-write-wins.
    let winner: i64 = sqlx::query_scalar("SELECT identity_id FROM aliases WHERE alias = $1")
        .bind(&alias)
        .fetch_one(conn)
        .await?;
    Ok(winner)
}

/// Identity used for findings and events written by the legacy importer.
pub async fn legacy_identity(conn: &mut PgConnection) -> ApiResult<i64> {
    upsert_identity(conn, "legacy", "unknown", "unknown").await
}

#[cfg(test)]
mod tests {
    use super::valid_topic_name;

    #[test]
    fn topic_name_rule() {
        assert!(valid_topic_name("hive-v2-build"));
        assert!(valid_topic_name("ab"));
        assert!(!valid_topic_name("a"));
        assert!(!valid_topic_name("1abc"));
        assert!(!valid_topic_name("Abc"));
        assert!(!valid_topic_name("a_b"));
        assert!(!valid_topic_name(&format!("a{}", "b".repeat(80))));
        assert!(valid_topic_name(&format!("a{}", "b".repeat(79))));
    }
}
