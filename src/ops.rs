//! Stats, readiness and the hand-rolled Prometheus exposition.

use std::fmt::Write as _;
use std::time::Duration;

use axum::extract::State;
use axum::http::header;
use axum::response::IntoResponse;
use axum::Json;
use serde::Serialize;

use crate::error::{ApiError, ApiResult};
use crate::models::IdentityQuality;
use crate::scoring::top_identity;
use crate::state::AppState;

#[derive(Serialize)]
pub struct Stats {
    active_topics: i64,
    needs_human: i64,
    open_findings: i64,
    proposals_pending: i64,
    open_blockers: i64,
    total_topics: i64,
    total_messages: i64,
    top_identity: Option<IdentityQuality>,
    ready_jobs: i64,
    stale_jobs: i64,
}

pub async fn stats(State(st): State<AppState>) -> ApiResult<Json<Stats>> {
    let mut conn = st.pool.acquire().await?;
    let (active, needs, total_topics): (i64, i64, i64) = sqlx::query_as(
        "SELECT count(*) FILTER (WHERE status = 'active'), \
                count(*) FILTER (WHERE status = 'needs-human'), count(*) FROM topics",
    )
    .fetch_one(&mut *conn)
    .await?;
    let open_findings: i64 =
        // Job verdicts (about_identity_id set) are not open work.
        sqlx::query_scalar(
            "SELECT count(*) FROM findings WHERE status = 'open' AND about_identity_id IS NULL",
        )
            .fetch_one(&mut *conn)
            .await?;
    let open_blockers: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM findings WHERE status = 'open' AND severity = 'BLOCKER' \
           AND about_identity_id IS NULL",
    )
    .fetch_one(&mut *conn)
    .await?;
    let proposals_pending: i64 =
        sqlx::query_scalar("SELECT count(*) FROM proposals WHERE status = 'proposed'")
            .fetch_one(&mut *conn)
            .await?;
    let total_messages: i64 = sqlx::query_scalar("SELECT count(*) FROM messages")
        .fetch_one(&mut *conn)
        .await?;
    let top = top_identity(&mut conn).await?;
    let jobs = crate::jobs::summary(&mut conn, &st.jobs).await?;
    Ok(Json(Stats {
        active_topics: active,
        needs_human: needs,
        open_findings,
        proposals_pending,
        open_blockers,
        total_topics,
        total_messages,
        top_identity: top,
        ready_jobs: jobs.ready,
        stale_jobs: jobs.stale,
    }))
}

pub async fn readyz(State(st): State<AppState>) -> ApiResult<Json<serde_json::Value>> {
    let probe = sqlx::query_scalar::<_, i32>("SELECT 1").fetch_one(&st.pool);
    match tokio::time::timeout(Duration::from_secs(3), probe).await {
        Ok(Ok(_)) => Ok(Json(serde_json::json!({"db": "ok"}))),
        _ => Err(ApiError::Unavailable),
    }
}

const TOPIC_STATUS: [&str; 5] = ["active", "quiet", "needs-human", "resolved", "archived"];
const FINDING_STATUS: [&str; 7] = [
    "open",
    "confirmed",
    "fixed",
    "false_positive",
    "wontfix",
    "superseded",
    "expired",
];
const PROPOSAL_STATUS: [&str; 5] = ["proposed", "approved", "rejected", "applied", "expired"];

fn label_value(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

async fn grouped(
    conn: &mut sqlx::PgConnection,
    sql: &str,
) -> Result<Vec<(String, i64)>, sqlx::Error> {
    sqlx::query_as(sql).fetch_all(conn).await
}

fn emit_group(out: &mut String, name: &str, keys: &[&str], rows: &[(String, i64)]) {
    let _ = writeln!(out, "# TYPE {name} gauge");
    for k in keys {
        let v = rows.iter().find(|(s, _)| s == k).map_or(0, |(_, n)| *n);
        let _ = writeln!(out, "{name}{{status=\"{k}\"}} {v}");
    }
}

async fn db_metrics(st: &AppState, out: &mut String) -> Result<(), sqlx::Error> {
    let mut conn = st.pool.acquire().await?;
    let topics = grouped(
        &mut conn,
        "SELECT status, count(*) FROM topics GROUP BY status",
    )
    .await?;
    emit_group(out, "hive_topics", &TOPIC_STATUS, &topics);
    let findings = grouped(
        &mut conn,
        "SELECT status, count(*) FROM findings WHERE about_identity_id IS NULL GROUP BY status",
    )
    .await?;
    emit_group(out, "hive_findings", &FINDING_STATUS, &findings);
    let proposals = grouped(
        &mut conn,
        "SELECT status, count(*) FROM proposals GROUP BY status",
    )
    .await?;
    emit_group(out, "hive_proposals", &PROPOSAL_STATUS, &proposals);

    let msgs: i64 = sqlx::query_scalar("SELECT count(*) FROM messages")
        .fetch_one(&mut *conn)
        .await?;
    let _ = writeln!(
        out,
        "# TYPE hive_messages_total gauge\nhive_messages_total {msgs}"
    );
    let needs = topics
        .iter()
        .find(|(s, _)| s == "needs-human")
        .map_or(0, |(_, n)| *n);
    let _ = writeln!(
        out,
        "# TYPE hive_needs_human_open gauge\nhive_needs_human_open {needs}"
    );
    let stale: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM topics WHERE status = 'active' \
           AND last_activity_at < now() - make_interval(hours => $1::bigint::int)",
    )
    .bind(st.lifecycle.quiet_after_h)
    .fetch_one(&mut *conn)
    .await?;
    let _ = writeln!(
        out,
        "# TYPE hive_stale_active_topics gauge\nhive_stale_active_topics {stale}"
    );
    let last: Option<(f64, Option<bool>)> = sqlx::query_as(
        "SELECT extract(epoch FROM coalesce(finished_at, started_at))::float8, ok \
           FROM gardener_runs ORDER BY id DESC LIMIT 1",
    )
    .fetch_optional(&mut *conn)
    .await?;
    let jobs = crate::jobs::summary(&mut conn, &st.jobs).await?;
    let _ = writeln!(out, "# TYPE hive_jobs gauge");
    for (status, n) in &jobs.by_status {
        let _ = writeln!(out, "hive_jobs{{status=\"{status}\"}} {n}");
    }
    let _ = writeln!(
        out,
        "# TYPE hive_jobs_ready gauge\nhive_jobs_ready {}\n\
         # TYPE hive_jobs_blocked gauge\nhive_jobs_blocked {}\n\
         # TYPE hive_jobs_stale gauge\nhive_jobs_stale {}\n\
         # TYPE hive_job_claim_age_seconds_max gauge\nhive_job_claim_age_seconds_max {}",
        jobs.ready, jobs.blocked, jobs.stale, jobs.max_claim_age_s
    );
    let runs: i64 = sqlx::query_scalar("SELECT count(*) FROM gardener_runs")
        .fetch_one(&mut *conn)
        .await?;
    let _ = writeln!(
        out,
        "# TYPE hive_gardener_runs_total gauge\nhive_gardener_runs_total {runs}"
    );
    // Omitted until a run exists, so absent() alerts can tell "never ran" from "failed".
    if let Some((ts, ok)) = last {
        let _ = writeln!(
            out,
            "# TYPE hive_last_gardener_run_timestamp_seconds gauge\n\
             hive_last_gardener_run_timestamp_seconds {ts}\n\
             # TYPE hive_last_gardener_run_ok gauge\nhive_last_gardener_run_ok {}",
            i32::from(ok == Some(true))
        );
    }
    Ok(())
}

pub async fn metrics(State(st): State<AppState>) -> impl IntoResponse {
    let mut out = String::new();
    let mut body = String::new();
    let up = db_metrics(&st, &mut body).await.is_ok();
    let _ = writeln!(out, "# TYPE hive_db_up gauge\nhive_db_up {}", i32::from(up));
    if up {
        out.push_str(&body);
    }
    let _ = writeln!(out, "# TYPE hive_http_requests_total counter");
    for ((route, code), n) in st.http.snapshot() {
        let _ = writeln!(
            out,
            "hive_http_requests_total{{route=\"{}\",code=\"{code}\"}} {n}",
            label_value(&route)
        );
    }
    (
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        out,
    )
}
