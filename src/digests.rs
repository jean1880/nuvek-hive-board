//! Digests: distill input for the gardener, digest writes (with a keyword
//! fallback), and reads. Distillation never deletes anything.

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::db::{add_topic_event, fetch_topic, resolve_topic_id};
use crate::error::{ApiError, ApiResult};
use crate::extract::{ApiPath, ApiQuery};
use crate::models::{Digest, DigestSummary, Message, Topic, MSG_COLS, MSG_FROM};
use crate::redact::Redactions;
use crate::state::AppState;
use crate::util::{clamp_limit, truncate_chars};

const DIGEST_COLS: &str = "d.id, d.topic_id, t.name AS topic_name, d.version, d.summary, \
     d.transcript_md, d.participants, d.finding_ids, d.through_message_id, d.producer, d.created_at";
const DIGEST_SUMMARY_COLS: &str = "d.id, d.topic_id, t.name AS topic_name, d.version, d.summary, \
     d.participants, d.finding_ids, d.through_message_id, d.producer, d.created_at";

#[derive(Deserialize)]
pub struct InputQuery {
    after: Option<i64>,
}

#[derive(Serialize)]
pub struct DistillInput {
    topic: Topic,
    messages: Vec<Message>,
    distilled_through: i64,
}

/// Untyped messages (`note`/`progress`) plus job `result`/`verdict` messages, each with no finding
/// row: typed posts are already structured and are the source of truth for their own findings
/// (a job verdict normally has its row; only a reinforced duplicate lacks one and is kept here).
pub async fn distill_input(
    State(st): State<AppState>,
    ApiPath(t): ApiPath<String>,
    ApiQuery(q): ApiQuery<InputQuery>,
) -> ApiResult<Json<DistillInput>> {
    let mut conn = st.pool.acquire().await?;
    let tid = resolve_topic_id(&mut conn, &t).await?;
    let topic = fetch_topic(&mut conn, tid).await?;
    let after = q.after.unwrap_or(topic.distilled_through);
    let sql = format!(
        "SELECT {MSG_COLS} FROM {MSG_FROM} \
         WHERE m.topic_id = $1 AND m.id > $2 AND m.kind IN ('note','progress','result','verdict') \
           AND m.compacted_at IS NULL \
           AND NOT EXISTS (SELECT 1 FROM findings f WHERE f.message_id = m.id) \
         ORDER BY m.id ASC LIMIT 500"
    );
    let messages = sqlx::query_as::<_, Message>(&sql)
        .bind(tid)
        .bind(after)
        .fetch_all(&mut *conn)
        .await?;
    Ok(Json(DistillInput {
        distilled_through: topic.distilled_through,
        topic,
        messages,
    }))
}

#[derive(Deserialize, Default)]
pub struct DigestPayload {
    summary: Option<String>,
    transcript_md: Option<String>,
    finding_ids: Option<Vec<i64>>,
    through_message_id: Option<i64>,
    producer: Option<String>,
    actor: Option<String>,
}

const KEY_MARKERS: [&str; 6] = [
    "Decision:",
    "Fixed:",
    "Resolution:",
    "Conclusion:",
    "Takeaway:",
    "Top Contributor Recognition:",
];

/// Server-side keyword digest: marker lines first, else the last three message previews.
fn keyword_summary(msgs: &[Message]) -> String {
    let mut lines: Vec<String> = Vec::new();
    for m in msgs {
        for l in m.content.lines().map(str::trim) {
            if KEY_MARKERS.iter().any(|k| l.contains(k)) {
                lines.push(format!("[{}] {}", m.author, truncate_chars(l, 300)));
            }
        }
    }
    if lines.is_empty() {
        for m in msgs.iter().rev().take(3).rev() {
            lines.push(format!(
                "[{}] {}",
                m.author,
                truncate_chars(m.content.trim(), 120)
            ));
        }
    }
    if lines.is_empty() {
        "No messages yet.".to_string()
    } else {
        lines.join("\n")
    }
}

fn render_transcript(topic: &Topic, msgs: &[Message]) -> String {
    let mut md = format!(
        "# Hive Digest: {}\n\n- **Topic**: `{}`\n\n",
        topic.title, topic.name
    );
    for m in msgs {
        md.push_str(&format!(
            "### Message #{}: {} ({}) [{}]\n\n{}\n\n---\n\n",
            m.id,
            m.author,
            m.created_at.to_rfc3339(),
            m.kind,
            m.content
        ));
    }
    md
}

pub async fn write_digest(
    State(st): State<AppState>,
    ApiPath(t): ApiPath<String>,
    body: axum::body::Bytes,
) -> ApiResult<(StatusCode, Json<Digest>)> {
    // An empty body means "keyword fallback".
    let p: DigestPayload = if body.iter().all(u8::is_ascii_whitespace) {
        DigestPayload::default()
    } else {
        serde_json::from_slice(&body)
            .map_err(|_| ApiError::validation("invalid JSON body for digest"))?
    };
    let actor = p
        .actor
        .as_deref()
        .map(str::trim)
        .filter(|a| !a.is_empty())
        .unwrap_or("system")
        .to_string();
    let keyword = p.summary.as_deref().is_none_or(|s| s.trim().is_empty())
        || p.producer.as_deref() == Some("keyword");
    let producer = if keyword {
        "keyword".to_string()
    } else {
        match p.producer.as_deref().map(str::trim) {
            // Truncated only after redaction (below), so a cut token cannot survive.
            Some(v) if !v.is_empty() => v.to_string(),
            _ => return Err(ApiError::validation("producer is required with a summary")),
        }
    };

    if p.through_message_id.is_some_and(|v| v < 0) {
        return Err(ApiError::validation(
            "through_message_id must not be negative",
        ));
    }
    let mut want_ids = p.finding_ids.clone().unwrap_or_default();
    want_ids.sort_unstable();
    want_ids.dedup();

    // Version race: unique violation retries once, then 409.
    for attempt in 0..2 {
        let mut tx = st.pool.begin().await?;
        let tid = resolve_topic_id(&mut tx, &t).await?;
        let topic = fetch_topic(&mut tx, tid).await?;
        let max_id: i64 =
            sqlx::query_scalar("SELECT coalesce(max(id), 0) FROM messages WHERE topic_id = $1")
                .bind(tid)
                .fetch_one(&mut *tx)
                .await?;
        let through = p.through_message_id.unwrap_or(max_id);
        if through > max_id {
            return Err(ApiError::conflict(
                "through_message_id is above the topic's newest message",
            ));
        }
        if through < topic.distilled_through {
            return Err(ApiError::conflict(
                "through_message_id is below the topic's distilled_through",
            ));
        }
        let sql = format!(
            "SELECT {MSG_COLS} FROM {MSG_FROM} WHERE m.topic_id = $1 AND m.id <= $2 \
             ORDER BY m.id ASC LIMIT 2000"
        );
        let msgs = sqlx::query_as::<_, Message>(&sql)
            .bind(tid)
            .bind(through)
            .fetch_all(&mut *tx)
            .await?;
        let mut participants: Vec<String> = msgs.iter().map(|m| m.author.clone()).collect();
        participants.sort();
        participants.dedup();

        let summary = match (&p.summary, keyword) {
            (Some(s), false) => s.trim().to_string(),
            _ => keyword_summary(&msgs),
        };
        let transcript = match &p.transcript_md {
            Some(md) if !md.trim().is_empty() => md.clone(),
            _ => render_transcript(&topic, &msgs),
        };
        let mut red = Redactions::new();
        let summary = red.text("summary", summary);
        let transcript = red.text("transcript_md", transcript);
        let actor = red.text("actor", actor.clone());
        let producer = truncate_chars(&red.text("producer", producer.clone()), 120);
        let finding_ids = want_ids.clone();
        if !finding_ids.is_empty() {
            let known: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM findings WHERE topic_id = $1 AND id = ANY($2)",
            )
            .bind(tid)
            .bind(&finding_ids)
            .fetch_one(&mut *tx)
            .await?;
            if known != finding_ids.len() as i64 {
                return Err(ApiError::validation(
                    "finding_ids must all be findings of this topic",
                ));
            }
        }

        let res = sqlx::query_scalar::<_, i64>(
            "INSERT INTO digests (topic_id, version, summary, transcript_md, participants, \
               finding_ids, through_message_id, producer) \
             VALUES ($1, (SELECT coalesce(max(version), 0) + 1 FROM digests WHERE topic_id = $1), \
                     $2,$3,$4,$5,$6,$7) RETURNING id",
        )
        .bind(tid)
        .bind(&summary)
        .bind(&transcript)
        .bind(&participants)
        .bind(&finding_ids)
        .bind(through)
        .bind(&producer)
        .fetch_one(&mut *tx)
        .await;
        let did = match res {
            Ok(id) => id,
            Err(sqlx::Error::Database(e)) if e.code().as_deref() == Some("23505") => {
                drop(tx);
                if attempt == 0 {
                    continue;
                }
                return Err(ApiError::conflict("digest version race, retry later"));
            }
            Err(e) => return Err(e.into()),
        };
        sqlx::query(
            "UPDATE topics SET distilled_through = GREATEST(distilled_through, $2), \
               distilled_at = now(), updated_at = now() WHERE id = $1",
        )
        .bind(tid)
        .bind(through)
        .execute(&mut *tx)
        .await?;
        add_topic_event(
            &mut tx,
            tid,
            "distilled",
            &actor,
            serde_json::json!({"digest_id": did, "through_message_id": through, "producer": producer}),
        )
        .await?;
        red.record(&mut tx, Some(tid), &actor).await?;
        let sql = format!(
            "SELECT {DIGEST_COLS} FROM digests d JOIN topics t ON t.id = d.topic_id WHERE d.id = $1"
        );
        let digest = sqlx::query_as::<_, Digest>(&sql)
            .bind(did)
            .fetch_one(&mut *tx)
            .await?;
        tx.commit().await?;
        return Ok((StatusCode::CREATED, Json(digest)));
    }
    Err(ApiError::conflict("digest version race, retry later"))
}

#[derive(Deserialize)]
pub struct ListQuery {
    topic: Option<String>,
    limit: Option<i64>,
}

pub async fn list(
    State(st): State<AppState>,
    ApiQuery(q): ApiQuery<ListQuery>,
) -> ApiResult<Json<Vec<DigestSummary>>> {
    let mut conn = st.pool.acquire().await?;
    let tid = match &q.topic {
        Some(t) => Some(resolve_topic_id(&mut conn, t).await?),
        None => None,
    };
    let sql = format!(
        "SELECT {DIGEST_SUMMARY_COLS} FROM digests d JOIN topics t ON t.id = d.topic_id \
         WHERE ($1::bigint IS NULL OR d.topic_id = $1) ORDER BY d.created_at DESC, d.id DESC LIMIT $2"
    );
    let rows = sqlx::query_as::<_, DigestSummary>(&sql)
        .bind(tid)
        .bind(clamp_limit(q.limit, 50, 500))
        .fetch_all(&mut *conn)
        .await?;
    Ok(Json(rows))
}

pub async fn get(State(st): State<AppState>, ApiPath(id): ApiPath<i64>) -> ApiResult<Json<Digest>> {
    let sql = format!(
        "SELECT {DIGEST_COLS} FROM digests d JOIN topics t ON t.id = d.topic_id WHERE d.id = $1"
    );
    sqlx::query_as::<_, Digest>(&sql)
        .bind(id)
        .fetch_optional(&st.pool)
        .await?
        .map(Json)
        .ok_or_else(|| ApiError::not_found("digest not found"))
}
