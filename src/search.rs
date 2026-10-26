//! Full-text search (websearch_to_tsquery + ts_rank_cd) with trigram suggestions,
//! and the brief used to prime an agent before it starts work.

use axum::extract::State;
use axum::Json;
use serde::{Deserialize, Serialize};
use sqlx::FromRow;

use crate::db::resolve_topic_id;
use crate::error::ApiResult;
use crate::extract::ApiQuery;
use crate::models::{DigestSummary, Finding, Topic, Ts, FINDING_COLS, TOPIC_COLS};
use crate::state::AppState;
use crate::util::clamp_limit;

#[derive(Deserialize)]
pub struct SearchQuery {
    q: Option<String>,
    topic: Option<String>,
    author: Option<String>,
    limit: Option<i64>,
}

#[derive(Serialize, FromRow)]
pub struct SearchHit {
    kind: String,
    topic_name: String,
    id: i64,
    author: String,
    title_or_snippet: String,
    created_at: Ts,
    score: f64,
}

#[derive(Serialize)]
pub struct SearchResponse {
    query: String,
    total_results: usize,
    suggestions: Vec<String>,
    results: Vec<SearchHit>,
}

const HEADLINE: &str = "'MaxFragments=1,MaxWords=30,MinWords=8,StartSel=**,StopSel=**'";

pub async fn search(
    State(st): State<AppState>,
    ApiQuery(q): ApiQuery<SearchQuery>,
) -> ApiResult<Json<SearchResponse>> {
    let query = q.q.unwrap_or_default().trim().to_string();
    if query.is_empty() {
        return Ok(Json(SearchResponse {
            query,
            total_results: 0,
            suggestions: Vec::new(),
            results: Vec::new(),
        }));
    }
    let mut conn = st.pool.acquire().await?;
    let tid = match &q.topic {
        Some(t) => Some(resolve_topic_id(&mut conn, t).await?),
        None => None,
    };
    let author = q
        .author
        .map(|a| a.trim().to_string())
        .filter(|a| !a.is_empty());
    let limit = clamp_limit(q.limit, 25, 100);

    let sql = format!(
        "SELECT * FROM ( \
           SELECT 'topic'::text AS kind, t.name AS topic_name, t.id, t.created_by AS author, \
                  t.title AS title_or_snippet, t.created_at, \
                  ts_rank_cd(t.search, tq)::float8 AS score \
             FROM topics t, websearch_to_tsquery('english', $1) tq \
            WHERE t.search @@ tq AND ($2::bigint IS NULL OR t.id = $2) \
              AND ($3::text IS NULL OR t.created_by = $3) \
           UNION ALL \
           SELECT 'message', t.name, m.id, m.author, \
                  ts_headline('english', m.content, tq, {HEADLINE}), m.created_at, \
                  ts_rank_cd(m.search, tq)::float8 \
             FROM messages m JOIN topics t ON t.id = m.topic_id, \
                  websearch_to_tsquery('english', $1) tq \
            WHERE m.search @@ tq AND ($2::bigint IS NULL OR m.topic_id = $2) \
              AND ($3::text IS NULL OR m.author = $3) \
           UNION ALL \
           SELECT 'finding', t.name, f.id, coalesce(i.agent_type, f.provenance), \
                  ts_headline('english', f.claim, tq, {HEADLINE}), f.created_at, \
                  ts_rank_cd(f.search, tq)::float8 \
             FROM findings f JOIN topics t ON t.id = f.topic_id \
                  LEFT JOIN identities i ON i.id = f.identity_id, \
                  websearch_to_tsquery('english', $1) tq \
            WHERE f.search @@ tq AND ($2::bigint IS NULL OR f.topic_id = $2) AND $3::text IS NULL \
           UNION ALL \
           SELECT 'digest', t.name, d.id, d.producer, \
                  ts_headline('english', d.summary, tq, {HEADLINE}), d.created_at, \
                  ts_rank_cd(d.search, tq)::float8 \
             FROM digests d JOIN topics t ON t.id = d.topic_id, \
                  websearch_to_tsquery('english', $1) tq \
            WHERE d.search @@ tq AND ($2::bigint IS NULL OR d.topic_id = $2) AND $3::text IS NULL \
         ) hits ORDER BY score DESC, created_at DESC LIMIT $4"
    );
    let results = sqlx::query_as::<_, SearchHit>(&sql)
        .bind(&query)
        .bind(tid)
        .bind(&author)
        .bind(limit)
        .fetch_all(&mut *conn)
        .await?;

    // Trigram suggestions over topic names/titles and aliases.
    let suggestions: Vec<String> = sqlx::query_scalar(
        "SELECT cand FROM ( \
           SELECT name AS cand, similarity(name, $1) AS s FROM topics \
           UNION ALL SELECT title, similarity(title, $1) FROM topics \
           UNION ALL SELECT alias, similarity(alias, $1) FROM aliases) x \
          WHERE s > 0.25 GROUP BY cand ORDER BY max(s) DESC, cand LIMIT 5",
    )
    .bind(&query)
    .fetch_all(&mut *conn)
    .await?;

    Ok(Json(SearchResponse {
        query,
        total_results: results.len(),
        suggestions,
        results,
    }))
}

#[derive(Deserialize)]
pub struct BriefQuery {
    subjects: Option<String>,
    q: Option<String>,
    limit: Option<i64>,
}

#[derive(Serialize)]
pub struct Brief {
    findings: Vec<Finding>,
    digests: Vec<DigestSummary>,
    related_topics: Vec<Topic>,
}

pub async fn brief(
    State(st): State<AppState>,
    ApiQuery(q): ApiQuery<BriefQuery>,
) -> ApiResult<Json<Brief>> {
    let subjects: Vec<String> = q
        .subjects
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let text = q.q.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    // With no free text, search by the subjects themselves.
    let fts = text.clone().or_else(|| {
        if subjects.is_empty() {
            None
        } else {
            Some(subjects.join(" or "))
        }
    });
    let limit = clamp_limit(q.limit, 8, 50);
    let mut conn = st.pool.acquire().await?;

    let fsql = format!(
        "SELECT {FINDING_COLS} FROM findings f JOIN topics t ON t.id = f.topic_id \
         WHERE f.status IN ('open','confirmed','fixed') \
           AND ( ($1::text[] = '{{}}' AND $2::text IS NULL) \
                 OR f.subjects && $1 \
                 OR ($2::text IS NOT NULL AND f.search @@ websearch_to_tsquery('english', $2)) ) \
         ORDER BY CASE f.severity WHEN 'BLOCKER' THEN 0 WHEN 'SHOULD' THEN 1 ELSE 2 END, \
                  f.reinforce_count DESC, f.updated_at DESC LIMIT $3"
    );
    let findings = sqlx::query_as::<_, Finding>(&fsql)
        .bind(&subjects)
        .bind(&fts)
        .bind(limit)
        .fetch_all(&mut *conn)
        .await?;

    let dsql = "SELECT d.id, d.topic_id, t.name AS topic_name, d.version, d.summary, \
                       d.participants, d.finding_ids, d.through_message_id, d.producer, d.created_at \
                  FROM digests d JOIN topics t ON t.id = d.topic_id \
                 WHERE $1::text IS NULL OR d.search @@ websearch_to_tsquery('english', $1) \
                 ORDER BY CASE WHEN $1::text IS NULL THEN 0 \
                          ELSE ts_rank_cd(d.search, websearch_to_tsquery('english', $1)) END DESC, \
                          d.created_at DESC LIMIT $2";
    let digests = sqlx::query_as::<_, DigestSummary>(dsql)
        .bind(&fts)
        .bind(limit)
        .fetch_all(&mut *conn)
        .await?;

    let tsql = format!(
        "SELECT {TOPIC_COLS} FROM topics t \
         WHERE $1::text IS NULL OR t.search @@ websearch_to_tsquery('english', $1) \
         ORDER BY CASE WHEN $1::text IS NULL THEN 0 \
                  ELSE ts_rank_cd(t.search, websearch_to_tsquery('english', $1)) END DESC, \
                  t.last_activity_at DESC LIMIT $2"
    );
    let related_topics = sqlx::query_as::<_, Topic>(&tsql)
        .bind(&fts)
        .bind(limit)
        .fetch_all(&mut *conn)
        .await?;

    Ok(Json(Brief {
        findings,
        digests,
        related_topics,
    }))
}
