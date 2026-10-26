//! Topic lifecycle sweep: quiet, auto-resolve, archive, compact.
//!
//! Every step is one UPDATE whose WHERE re-checks the condition (including
//! `last_activity_at`), so a post racing the sweep wins. `needs-human` topics
//! are never touched. A dry run executes the identical statements and rolls the
//! transaction back, so the preview cannot drift from the real run.

use axum::extract::State;
use axum::Json;
use serde::{Deserialize, Serialize};
use sqlx::PgConnection;

use crate::config::Lifecycle;
use crate::db::{add_topic_event, required_text};
use crate::error::ApiResult;
use crate::extract::ApiJson;
use crate::state::AppState;

#[derive(Deserialize)]
pub struct SweepPayload {
    #[serde(default)]
    dry_run: bool,
    actor: String,
}

#[derive(Debug, Serialize)]
pub struct Change {
    pub topic: String,
    pub from: String,
    pub to: String,
    pub reason: String,
}

#[derive(Serialize)]
pub struct SweepResult {
    pub dry_run: bool,
    pub changes: Vec<Change>,
}

pub async fn sweep(
    State(st): State<AppState>,
    ApiJson(p): ApiJson<SweepPayload>,
) -> ApiResult<Json<SweepResult>> {
    let actor = required_text("actor", &p.actor, 120)?;
    let mut red = crate::redact::Redactions::new();
    let actor = red.text("actor", actor);
    red.log_only();
    let mut tx = st.pool.begin().await?;
    // The sweep's job-topic guard reads job state: take the global jobs lock FIRST.
    crate::jobs::lock_jobs(&mut tx).await?;
    let changes = run_sweep(&mut tx, &st.lifecycle, &actor).await?;
    if p.dry_run {
        tx.rollback().await?;
    } else {
        tx.commit().await?;
    }
    Ok(Json(SweepResult {
        dry_run: p.dry_run,
        changes,
    }))
}

/// One sweep step: the statements differ only in these literals, so they stay positional.
#[allow(clippy::too_many_arguments)]
async fn transition(
    conn: &mut PgConnection,
    sql: &str,
    hours: i64,
    from: &str,
    to: &str,
    reason: &str,
    event: &str,
    actor: &str,
    out: &mut Vec<Change>,
) -> ApiResult<()> {
    let rows: Vec<(i64, String)> = sqlx::query_as(sql)
        .bind(hours)
        .fetch_all(&mut *conn)
        .await?;
    for (id, name) in rows {
        add_topic_event(
            conn,
            id,
            event,
            actor,
            serde_json::json!({"from": from, "to": to, "reason": reason}),
        )
        .await?;
        out.push(Change {
            topic: name,
            from: from.into(),
            to: to.into(),
            reason: reason.into(),
        });
    }
    Ok(())
}

pub async fn run_sweep(
    conn: &mut PgConnection,
    lc: &Lifecycle,
    actor: &str,
) -> ApiResult<Vec<Change>> {
    let mut out = Vec::new();

    // active -> quiet
    transition(
        conn,
        "UPDATE topics SET status = 'quiet', updated_at = now() \
         WHERE status = 'active' AND last_activity_at < now() - make_interval(hours => $1::bigint::int) \
           AND NOT EXISTS (SELECT 1 FROM jobs j WHERE j.topic_id = topics.id \
                            AND j.status IN ('open','claimed','done')) \
         RETURNING id, name",
        lc.quiet_after_h,
        "active",
        "quiet",
        &format!("no activity for {}h", lc.quiet_after_h),
        "quiet",
        actor,
        &mut out,
    )
    .await?;

    // quiet -> resolved (auto): quiet for resolve_after_h on top of the quiet window.
    transition(
        conn,
        "UPDATE topics SET status = 'resolved', outcome = 'auto: inactive', updated_at = now() \
         WHERE status = 'quiet' \
           AND NOT EXISTS (SELECT 1 FROM jobs j WHERE j.topic_id = topics.id \
                            AND j.status IN ('open','claimed','done')) \
           AND last_activity_at < now() - make_interval(hours => $1::bigint::int) \
         RETURNING id, name",
        lc.quiet_after_h + lc.resolve_after_h,
        "quiet",
        "resolved",
        &format!("quiet for {}h", lc.resolve_after_h),
        "resolved",
        actor,
        &mut out,
    )
    .await?;

    // resolved -> archived: only once distilled through the last message.
    transition(
        conn,
        "UPDATE topics t SET status = 'archived', updated_at = now() \
         WHERE t.status = 'resolved' \
           AND NOT EXISTS (SELECT 1 FROM jobs j WHERE j.topic_id = t.id \
                            AND j.status IN ('open','claimed','done')) \
           AND t.updated_at < now() - make_interval(hours => $1::bigint::int) \
           AND t.last_activity_at < now() - make_interval(hours => $1::bigint::int) \
           AND t.distilled_through >= coalesce((SELECT max(id) FROM messages m WHERE m.topic_id = t.id), 0) \
         RETURNING t.id, t.name",
        lc.archive_after_h,
        "resolved",
        "archived",
        &format!("resolved and distilled for {}h", lc.archive_after_h),
        "archived",
        actor,
        &mut out,
    )
    .await?;

    // Compaction: ephemeral messages only, archived topics only, covered by a digest.
    let compacted: Vec<(i64, String, i64)> = sqlx::query_as(
        "WITH c AS ( \
           UPDATE messages m SET compacted_at = now(), updated_at = now(), \
             content = '[compacted into digest v' || (SELECT min(d.version) FROM digests d \
                WHERE d.topic_id = m.topic_id AND d.through_message_id >= m.id) || ']' \
           FROM topics t \
           WHERE t.id = m.topic_id AND t.status = 'archived' \
             AND t.last_activity_at < now() - make_interval(days => $1::bigint::int) \
             AND m.ephemeral AND m.compacted_at IS NULL \
             AND EXISTS (SELECT 1 FROM digests d WHERE d.topic_id = m.topic_id \
                          AND d.through_message_id >= m.id) \
           RETURNING m.topic_id, t.name) \
         SELECT topic_id, name, count(*) FROM c GROUP BY topic_id, name ORDER BY topic_id",
    )
    .bind(lc.compact_after_d)
    .fetch_all(&mut *conn)
    .await?;
    for (id, name, n) in compacted {
        let reason = format!("compacted {n} ephemeral message(s) into digests");
        add_topic_event(
            conn,
            id,
            "compacted",
            actor,
            serde_json::json!({"messages": n}),
        )
        .await?;
        out.push(Change {
            topic: name,
            from: "archived".into(),
            to: "archived".into(),
            reason,
        });
    }
    Ok(out)
}
