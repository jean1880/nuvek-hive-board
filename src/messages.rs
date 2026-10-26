//! Message routes: list, threads, typed posts (which also create findings), edit, delete.

use std::collections::HashMap;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use serde::{Deserialize, Serialize};
use sqlx::PgConnection;

use crate::db::{
    add_topic_event, required_text, resolve_identity, resolve_topic_id, FINDING_KINDS,
    MESSAGE_KINDS, SEVERITIES,
};
use crate::error::{ApiError, ApiResult};
use crate::extract::{ApiJson, ApiPath, ApiQuery};
use crate::models::{AgentIdent, Message, ThreadNode, MSG_COLS, MSG_FROM};
use crate::redact::Redactions;
use crate::state::AppState;
use crate::util::{clamp_limit, clean_list, content_hash, sha256_hex, truncate_chars};

const MAX_CONTENT_CHARS: usize = 100_000;

pub async fn fetch_message(conn: &mut PgConnection, id: i64) -> ApiResult<Message> {
    let sql = format!("SELECT {MSG_COLS} FROM {MSG_FROM} WHERE m.id = $1");
    sqlx::query_as::<_, Message>(&sql)
        .bind(id)
        .fetch_optional(conn)
        .await?
        .ok_or_else(|| ApiError::not_found("message not found"))
}

#[derive(Deserialize)]
pub struct ListQuery {
    after: Option<i64>,
    limit: Option<i64>,
    parent_id: Option<i64>,
    kind: Option<String>,
}

pub async fn list(
    State(st): State<AppState>,
    ApiPath(t): ApiPath<String>,
    ApiQuery(q): ApiQuery<ListQuery>,
) -> ApiResult<Json<Vec<Message>>> {
    let kinds: Option<Vec<String>> = match q.kind.as_deref() {
        None => None,
        Some(raw) => {
            let mut out = Vec::new();
            for k in raw.split(',').map(str::trim) {
                if !MESSAGE_KINDS.contains(&k) {
                    return Err(ApiError::validation("unknown message kind"));
                }
                out.push(k.to_string());
            }
            Some(out)
        }
    };
    let mut conn = st.pool.acquire().await?;
    let tid = resolve_topic_id(&mut conn, &t).await?;
    let sql = format!(
        "SELECT {MSG_COLS} FROM {MSG_FROM} WHERE m.topic_id = $1 AND m.id > $2 \
           AND ($3::bigint IS NULL OR m.parent_id = $3) \
           AND ($5::text[] IS NULL OR m.kind = ANY($5)) ORDER BY m.id ASC LIMIT $4"
    );
    let rows = sqlx::query_as::<_, Message>(&sql)
        .bind(tid)
        .bind(q.after.unwrap_or(0))
        .bind(q.parent_id)
        .bind(clamp_limit(q.limit, 200, 500))
        .bind(&kinds)
        .fetch_all(&mut *conn)
        .await?;
    Ok(Json(rows))
}

/// Replies nested beyond this attach to the capped node's parent, as its siblings. Each level is two
/// JSON levels (node object + replies array), and serde_json clients (hive-mcp,
/// the gardener) refuse documents nested deeper than 128, so 48 keeps every
/// response parseable. Thread output is also capped at `MAX_THREAD_MESSAGES`.
const MAX_THREAD_DEPTH: usize = 48;
const MAX_THREAD_MESSAGES: i64 = 5000;

/// Build reply trees without recursion. `root = None` builds the topic forest;
/// `Some(id)` builds the subtree below that message.
fn build_tree(msgs: Vec<Message>, root: Option<i64>) -> Vec<ThreadNode> {
    let mut kids: HashMap<i64, Vec<usize>> = HashMap::new();
    let mut roots: Vec<usize> = Vec::new();
    for (i, m) in msgs.iter().enumerate() {
        if root == Some(m.id) {
            roots.push(i);
        } else if let Some(p) = m.parent_id {
            kids.entry(p).or_default().push(i);
        } else if root.is_none() {
            roots.push(i);
        }
    }

    // Preorder walk recording each node's effective parent position.
    let mut order: Vec<(usize, Option<usize>)> = Vec::new();
    let mut stack: Vec<(usize, Option<usize>, usize)> =
        roots.iter().rev().map(|&i| (i, None, 0)).collect();
    while let Some((i, par, depth)) = stack.pop() {
        let pos = order.len();
        order.push((i, par));
        if let Some(ch) = kids.get(&msgs[i].id) {
            let (cp, cd) = if depth >= MAX_THREAD_DEPTH {
                (par, depth)
            } else {
                (Some(pos), depth + 1)
            };
            for &c in ch.iter().rev() {
                stack.push((c, cp, cd));
            }
        }
    }

    // Children always follow their parent in preorder, so assemble back to front.
    let mut slots: Vec<Vec<ThreadNode>> = (0..order.len()).map(|_| Vec::new()).collect();
    let mut src: Vec<Option<Message>> = msgs.into_iter().map(Some).collect();
    let mut out: Vec<ThreadNode> = Vec::new();
    for pos in (0..order.len()).rev() {
        let (idx, par) = order[pos];
        let Some(message) = src[idx].take() else {
            continue;
        };
        let mut replies = std::mem::take(&mut slots[pos]);
        replies.reverse();
        let node = ThreadNode { message, replies };
        match par {
            Some(pp) => slots[pp].push(node),
            None => out.push(node),
        }
    }
    out.reverse();
    out
}

async fn topic_messages(conn: &mut PgConnection, tid: i64) -> ApiResult<Vec<Message>> {
    let sql = format!(
        "SELECT {MSG_COLS} FROM {MSG_FROM} WHERE m.topic_id = $1 ORDER BY m.id ASC LIMIT $2"
    );
    Ok(sqlx::query_as::<_, Message>(&sql)
        .bind(tid)
        .bind(MAX_THREAD_MESSAGES)
        .fetch_all(conn)
        .await?)
}

pub async fn threads(
    State(st): State<AppState>,
    ApiPath(t): ApiPath<String>,
) -> ApiResult<Json<Vec<ThreadNode>>> {
    let mut conn = st.pool.acquire().await?;
    let tid = resolve_topic_id(&mut conn, &t).await?;
    let msgs = topic_messages(&mut conn, tid).await?;
    Ok(Json(build_tree(msgs, None)))
}

pub async fn thread(
    State(st): State<AppState>,
    ApiPath(id): ApiPath<i64>,
) -> ApiResult<Json<ThreadNode>> {
    let mut conn = st.pool.acquire().await?;
    let root = fetch_message(&mut conn, id).await?;
    let msgs = topic_messages(&mut conn, root.topic_id).await?;
    build_tree(msgs, Some(id))
        .into_iter()
        .next()
        .map(Json)
        .ok_or_else(|| ApiError::not_found("message not found"))
}

const EVIDENCE_WARNING: &str = "evidence missing: add a command and its output or file:line";
/// Kinds whose posts must carry a severity (the others score a null severity as NICE).
const SEVERITY_REQUIRED_KINDS: [&str; 3] = ["finding", "gotcha", "verdict"];

/// A posted message plus the tracked finding it created (or reinforced) and any soft warnings.
#[derive(Serialize)]
pub struct PostResponse {
    #[serde(flatten)]
    message: Message,
    finding_id: Option<i64>,
    warnings: Vec<String>,
}

/// The finding tracking a message: its own row, or the one it reinforced (same topic and content).
async fn finding_for_message(conn: &mut PgConnection, m: &Message) -> ApiResult<Option<i64>> {
    if !FINDING_KINDS.contains(&m.kind.as_str()) {
        return Ok(None);
    }
    let own: Option<i64> = sqlx::query_scalar("SELECT id FROM findings WHERE message_id = $1")
        .bind(m.id)
        .fetch_optional(&mut *conn)
        .await?;
    if own.is_some() {
        return Ok(own);
    }
    Ok(
        sqlx::query_scalar("SELECT id FROM findings WHERE topic_id = $1 AND content_hash = $2")
            .bind(m.topic_id)
            .bind(content_hash(&m.content))
            .fetch_optional(&mut *conn)
            .await?,
    )
}

async fn replay_response(conn: &mut PgConnection, id: i64) -> ApiResult<PostResponse> {
    let message = fetch_message(conn, id).await?;
    let finding_id = finding_for_message(conn, &message).await?;
    Ok(PostResponse {
        message,
        finding_id,
        warnings: Vec::new(),
    })
}

#[derive(Deserialize)]
pub struct PostPayload {
    author: String,
    content: String,
    parent_id: Option<i64>,
    tags: Option<Vec<String>>,
    kind: Option<String>,
    severity: Option<String>,
    subjects: Option<Vec<String>>,
    evidence: Option<String>,
    task_kind: Option<String>,
    ephemeral: Option<bool>,
    agent: Option<AgentIdent>,
}

pub async fn post(
    State(st): State<AppState>,
    ApiPath(t): ApiPath<String>,
    headers: HeaderMap,
    ApiJson(p): ApiJson<PostPayload>,
) -> ApiResult<(StatusCode, Json<PostResponse>)> {
    let author = required_text("author", &p.author, 120)?;
    let content = required_text("content", &p.content, MAX_CONTENT_CHARS)?;
    let kind = p.kind.unwrap_or_else(|| "note".into());
    if !MESSAGE_KINDS.contains(&kind.as_str()) {
        return Err(ApiError::validation("unknown message kind"));
    }
    if let Some(s) = &p.severity {
        if !SEVERITIES.contains(&s.as_str()) {
            return Err(ApiError::validation(
                "severity must be BLOCKER, SHOULD or NICE",
            ));
        }
    }
    if p.severity.is_none() && SEVERITY_REQUIRED_KINDS.contains(&kind.as_str()) {
        return Err(ApiError::validation(format!(
            "severity is required for kind '{kind}': BLOCKER, SHOULD or NICE"
        )));
    }
    let ephemeral = p.ephemeral.unwrap_or(kind == "progress");
    // Redact, never reject (see redact.rs). Redaction comes BEFORE truncation and
    // de-duplication, so a cut token cannot survive as a short fragment, and before
    // hashing and storage.
    let mut red = Redactions::new();
    let author = red.text("author", author);
    let content = red.text("content", content);
    let tags = clean_list(red.opt_list("tags", p.tags), 32, 64);
    let subjects = clean_list(red.opt_list("subjects", p.subjects), 32, 120);
    let evidence = red
        .opt("evidence", p.evidence)
        .map(|e| e.trim().to_string())
        .filter(|e| !e.is_empty());
    let task_kind = red
        .opt("task_kind", p.task_kind)
        .map(|e| truncate_chars(e.trim(), 80))
        .filter(|e| !e.is_empty());
    let agent = red.agent(p.agent);
    let idem = match headers.get("idempotency-key") {
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
            Some(s)
        }
        None => None,
    };

    let mut tx = st.pool.begin().await?;
    let tid = resolve_topic_id(&mut tx, &t).await?;
    let idem_key = idem.map(|k| format!("{tid}:{k}"));

    if let Some(k) = &idem_key {
        let prior: Option<i64> = sqlx::query_scalar("SELECT id FROM messages WHERE idem_key = $1")
            .bind(k)
            .fetch_optional(&mut *tx)
            .await?;
        if let Some(id) = prior {
            let r = replay_response(&mut tx, id).await?;
            return Ok((StatusCode::OK, Json(r)));
        }
    }

    // A post can reopen a job topic (a status change): take the jobs lock before any
    // topic row lock, the same order every job route uses.
    if crate::jobs::is_job_topic(&mut tx, tid).await? {
        crate::jobs::lock_jobs(&mut tx).await?;
    }
    // Serialise against the sweep and other posts on this topic.
    let status: String = sqlx::query_scalar("SELECT status FROM topics WHERE id = $1 FOR UPDATE")
        .bind(tid)
        .fetch_one(&mut *tx)
        .await?;

    if let Some(pid) = p.parent_id {
        let ptopic: Option<i64> = sqlx::query_scalar("SELECT topic_id FROM messages WHERE id = $1")
            .bind(pid)
            .fetch_optional(&mut *tx)
            .await?;
        if ptopic != Some(tid) {
            return Err(ApiError::validation(
                "parent_id is not a message in this topic",
            ));
        }
    }

    let identity_id = resolve_identity(&mut tx, &author, agent.as_ref(), Some(tid)).await?;
    red.record(&mut tx, Some(tid), &author).await?;

    let insert = sqlx::query_scalar::<_, i64>(
        "INSERT INTO messages (topic_id, parent_id, author, identity_id, content, tags, kind, \
           severity, subjects, evidence, task_kind, ephemeral, idem_key) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13) RETURNING id",
    )
    .bind(tid)
    .bind(p.parent_id)
    .bind(&author)
    .bind(identity_id)
    .bind(&content)
    .bind(&tags)
    .bind(&kind)
    .bind(&p.severity)
    .bind(&subjects)
    .bind(&evidence)
    .bind(&task_kind)
    .bind(ephemeral)
    .bind(&idem_key)
    .fetch_one(&mut *tx)
    .await;
    let mid = match insert {
        Ok(id) => id,
        Err(sqlx::Error::Database(e)) if e.code().as_deref() == Some("23505") => {
            // Lost an idempotency race: drop our txn, return the winner.
            drop(tx);
            let mut conn = st.pool.acquire().await?;
            let id: Option<i64> = match &idem_key {
                Some(k) => {
                    sqlx::query_scalar("SELECT id FROM messages WHERE idem_key = $1")
                        .bind(k)
                        .fetch_optional(&mut *conn)
                        .await?
                }
                None => None,
            };
            let id = id.ok_or_else(|| ApiError::conflict("duplicate message"))?;
            let r = replay_response(&mut conn, id).await?;
            return Ok((StatusCode::OK, Json(r)));
        }
        Err(e) => return Err(e.into()),
    };

    // Any new post reopens quiet/resolved/archived; needs-human is left alone.
    if matches!(status.as_str(), "quiet" | "resolved" | "archived") {
        sqlx::query(
            "UPDATE topics SET status = 'active', outcome = NULL, last_activity_at = now(), \
             updated_at = now() WHERE id = $1",
        )
        .bind(tid)
        .execute(&mut *tx)
        .await?;
        add_topic_event(
            &mut tx,
            tid,
            "reopened",
            &author,
            serde_json::json!({"from": status, "message_id": mid}),
        )
        .await?;
    } else {
        sqlx::query("UPDATE topics SET last_activity_at = now() WHERE id = $1")
            .bind(tid)
            .execute(&mut *tx)
            .await?;
    }

    let mut finding_id = None;
    let mut warnings = Vec::new();
    if FINDING_KINDS.contains(&kind.as_str()) {
        if evidence.is_none() {
            warnings.push(EVIDENCE_WARNING.to_string());
        }
        let fid: i64 = sqlx::query_scalar(
            "INSERT INTO findings (topic_id, message_id, identity_id, kind, severity, claim, \
               evidence, subjects, task_kind, provenance, content_hash) \
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,'agent',$10) \
             ON CONFLICT (topic_id, content_hash) DO UPDATE \
               SET reinforce_count = findings.reinforce_count + 1, updated_at = now() \
             RETURNING id",
        )
        .bind(tid)
        .bind(mid)
        .bind(identity_id)
        .bind(&kind)
        .bind(&p.severity)
        .bind(&content)
        .bind(&evidence)
        .bind(&subjects)
        .bind(&task_kind)
        .bind(content_hash(&content))
        .fetch_one(&mut *tx)
        .await?;
        finding_id = Some(fid);
    }

    let message = fetch_message(&mut tx, mid).await?;
    tx.commit().await?;
    Ok((
        StatusCode::CREATED,
        Json(PostResponse {
            message,
            finding_id,
            warnings,
        }),
    ))
}

#[derive(Deserialize)]
pub struct EditPayload {
    author: String,
    content: String,
}

pub async fn edit(
    State(st): State<AppState>,
    ApiPath(id): ApiPath<i64>,
    ApiJson(p): ApiJson<EditPayload>,
) -> ApiResult<Json<Message>> {
    let author = required_text("author", &p.author, 120)?;
    let content = required_text("content", &p.content, MAX_CONTENT_CHARS)?;
    let mut red = Redactions::new();
    let author = red.text("author", author);
    let content = red.text("content", content);
    let mut tx = st.pool.begin().await?;
    let row: Option<(String, i64, String)> =
        sqlx::query_as("SELECT author, topic_id, content FROM messages WHERE id = $1 FOR UPDATE")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
    let Some((owner, tid, old_content)) = row else {
        return Err(ApiError::not_found("message not found"));
    };
    if owner != author {
        return Err(ApiError::conflict(
            "author does not match the message author",
        ));
    }
    sqlx::query("UPDATE messages SET content = $2, updated_at = now() WHERE id = $1")
        .bind(id)
        .bind(&content)
        .execute(&mut *tx)
        .await?;
    // Audit trail: the old text is not kept, only its hash and length.
    add_topic_event(
        &mut tx,
        tid,
        "message-edited",
        &author,
        serde_json::json!({
            "message_id": id,
            "old_sha256": sha256_hex(&old_content),
            "old_length": old_content.chars().count(),
            "new_length": content.chars().count(),
        }),
    )
    .await?;
    red.record(&mut tx, Some(tid), &author).await?;
    let m = fetch_message(&mut tx, id).await?;
    tx.commit().await?;
    Ok(Json(m))
}

pub async fn delete(
    State(st): State<AppState>,
    ApiPath(id): ApiPath<i64>,
) -> ApiResult<StatusCode> {
    let mut tx = st.pool.begin().await?;
    let row: Option<(String, i64, String)> =
        sqlx::query_as("SELECT author, topic_id, content FROM messages WHERE id = $1 FOR UPDATE")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
    let Some((author, tid, content)) = row else {
        return Err(ApiError::not_found("message not found"));
    };
    // Replies are removed with their parent (ON DELETE CASCADE); count them for the audit row.
    let replies: i64 = sqlx::query_scalar(
        "WITH RECURSIVE d AS (SELECT id FROM messages WHERE parent_id = $1 \
           UNION SELECT m.id FROM messages m JOIN d ON m.parent_id = d.id) \
         SELECT count(*) FROM d",
    )
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query("DELETE FROM messages WHERE id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    add_topic_event(
        &mut tx,
        tid,
        "message-deleted",
        "api",
        serde_json::json!({
            "message_id": id,
            "author": author,
            "sha256": sha256_hex(&content),
            "length": content.chars().count(),
            "replies_deleted": replies,
        }),
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}
