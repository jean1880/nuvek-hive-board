//! Leaderboard (SQL over finding outcomes), the taste signal, and route hints.
//! There are no additive counters: every number is derived from rows.

use axum::extract::State;
use axum::Json;
use serde::{Deserialize, Serialize};
use sqlx::FromRow;

use crate::db::{add_topic_event, required_text, resolve_identity, resolve_topic_id};
use crate::error::{ApiError, ApiResult};
use crate::extract::{ApiJson, ApiQuery};
use crate::models::{AgentIdent, IdentityQuality};
use crate::redact::Redactions;
use crate::state::AppState;
use crate::util::clamp_limit;

const LEADERBOARD_SQL: &str = "SELECT iq.identity_id, iq.agent_type, iq.model, iq.harness, \
       coalesce((SELECT array_agg(a.alias ORDER BY a.alias) FROM aliases a \
                  WHERE a.identity_id = iq.identity_id), '{}') AS aliases, \
       iq.quality, iq.n_judged, iq.n_findings, iq.taste_mean, iq.insufficient_data, \
       row_number() OVER (ORDER BY iq.quality DESC, iq.taste_mean DESC NULLS LAST, \
                                   iq.n_judged DESC, iq.identity_id ASC) AS rank \
     FROM identity_quality iq";

pub async fn top_identity(
    conn: &mut sqlx::PgConnection,
) -> Result<Option<IdentityQuality>, sqlx::Error> {
    let sql = format!("SELECT * FROM ({LEADERBOARD_SQL}) r ORDER BY rank LIMIT 1");
    sqlx::query_as::<_, IdentityQuality>(&sql)
        .fetch_optional(conn)
        .await
}

#[derive(Deserialize)]
pub struct LimitQuery {
    limit: Option<i64>,
}

pub async fn leaderboard(
    State(st): State<AppState>,
    ApiQuery(q): ApiQuery<LimitQuery>,
) -> ApiResult<Json<Vec<IdentityQuality>>> {
    let sql = format!("SELECT * FROM ({LEADERBOARD_SQL}) r ORDER BY rank LIMIT $1");
    let rows = sqlx::query_as::<_, IdentityQuality>(&sql)
        .bind(clamp_limit(q.limit, 100, 100))
        .fetch_all(&st.pool)
        .await?;
    Ok(Json(rows))
}

#[derive(Serialize, FromRow)]
pub struct AliasRow {
    alias: String,
    taste_total: i64,
    taste_mean: f64,
    events: i64,
    top_contributor: bool,
    latest_reason: Option<String>,
}

pub async fn leaderboard_aliases(
    State(st): State<AppState>,
    ApiQuery(q): ApiQuery<LimitQuery>,
) -> ApiResult<Json<Vec<AliasRow>>> {
    let rows = sqlx::query_as::<_, AliasRow>(
        "SELECT alias, sum(points)::bigint AS taste_total, avg(points)::float8 AS taste_mean, \
                count(*)::bigint AS events, bool_or(top_contributor) AS top_contributor, \
                (array_agg(reason ORDER BY at DESC, id DESC))[1] AS latest_reason \
           FROM score_events GROUP BY alias \
          ORDER BY taste_mean DESC, events DESC, alias ASC LIMIT $1",
    )
    .bind(clamp_limit(q.limit, 100, 100))
    .fetch_all(&st.pool)
    .await?;
    Ok(Json(rows))
}

#[derive(Deserialize)]
pub struct ScorePayload {
    agent_name: String,
    score: i64,
    reason: String,
    topic: Option<String>,
    award_top_contributor: Option<bool>,
    agent: Option<AgentIdent>,
    scorer: Option<String>,
}

#[derive(Serialize)]
pub struct ScoreIdentity {
    id: i64,
    agent_type: String,
    model: String,
    harness: String,
}

#[derive(Serialize)]
pub struct ScoreResult {
    alias: String,
    identity: ScoreIdentity,
    top_contributor: bool,
}

pub async fn score(
    State(st): State<AppState>,
    ApiJson(p): ApiJson<ScorePayload>,
) -> ApiResult<Json<ScoreResult>> {
    let alias = required_text("agent_name", &p.agent_name, 120)?;
    let reason = required_text("reason", &p.reason, 2000)?;
    if !(0..=100).contains(&p.score) {
        return Err(ApiError::validation("score must be within 0..100"));
    }
    let scorer = p
        .scorer
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("orchestrator")
        .to_string();
    let top = p.award_top_contributor.unwrap_or(p.score >= 80);
    let mut red = Redactions::new();
    let alias = red.text("agent_name", alias);
    let reason = red.text("reason", reason);
    let scorer = red.text("scorer", scorer);
    let agent = red.agent(p.agent);

    let mut tx = st.pool.begin().await?;
    let tid = match &p.topic {
        Some(t) => Some(resolve_topic_id(&mut tx, t).await?),
        None => None,
    };
    red.record(&mut tx, tid, &scorer).await?;
    let identity_id = resolve_identity(&mut tx, &alias, agent.as_ref(), tid).await?;
    sqlx::query(
        "INSERT INTO score_events (identity_id, alias, topic_id, points, reason, scorer, top_contributor) \
         VALUES ($1,$2,$3,$4,$5,$6,$7)",
    )
    .bind(identity_id)
    .bind(&alias)
    .bind(tid)
    .bind(p.score as i32)
    .bind(&reason)
    .bind(&scorer)
    .bind(top)
    .execute(&mut *tx)
    .await?;
    if let Some(tid) = tid {
        add_topic_event(
            &mut tx,
            tid,
            "scored",
            &scorer,
            serde_json::json!({"alias": alias, "points": p.score, "reason": reason, "top_contributor": top}),
        )
        .await?;
    }
    let (agent_type, model, harness): (String, String, String) =
        sqlx::query_as("SELECT agent_type, model, harness FROM identities WHERE id = $1")
            .bind(identity_id)
            .fetch_one(&mut *tx)
            .await?;
    tx.commit().await?;
    Ok(Json(ScoreResult {
        alias,
        identity: ScoreIdentity {
            id: identity_id,
            agent_type,
            model,
            harness,
        },
        top_contributor: top,
    }))
}

#[derive(Deserialize)]
pub struct HintQuery {
    task_kind: Option<String>,
}

#[derive(Serialize)]
pub struct RouteHint {
    task_kind: Option<String>,
    candidates: Vec<IdentityQuality>,
    recommendation: Option<IdentityQuality>,
    reason: String,
}

pub async fn route_hint(
    State(st): State<AppState>,
    ApiQuery(q): ApiQuery<HintQuery>,
) -> ApiResult<Json<RouteHint>> {
    let task_kind = q
        .task_kind
        .map(|k| k.trim().to_string())
        .filter(|k| !k.is_empty());
    let sql = format!(
        "SELECT * FROM ({LEADERBOARD_SQL} \
           WHERE $1::text IS NULL OR iq.identity_id IN ( \
             SELECT identity_id FROM findings WHERE task_kind = $1 AND identity_id IS NOT NULL \
             UNION SELECT identity_id FROM messages WHERE task_kind = $1 AND identity_id IS NOT NULL)) r \
         ORDER BY rank LIMIT 10"
    );
    let candidates = sqlx::query_as::<_, IdentityQuality>(&sql)
        .bind(&task_kind)
        .fetch_all(&st.pool)
        .await?;
    let recommendation = candidates.iter().find(|c| !c.insufficient_data).cloned();
    let reason = match (&recommendation, candidates.is_empty()) {
        (Some(r), _) => format!(
            "highest outcome-based quality ({:.2}) among identities with at least 5 judged findings",
            r.quality
        ),
        (None, true) => "no identity has worked on this task kind yet".to_string(),
        (None, false) => "insufficient data: no candidate has 5 judged findings yet".to_string(),
    };
    Ok(Json(RouteHint {
        task_kind,
        candidates,
        recommendation,
        reason,
    }))
}
