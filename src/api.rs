//! Router assembly: routes, bearer auth on writes, request counters.

use axum::extract::{MatchedPath, Request, State};
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::{get, patch, post};
use axum::Router;

use crate::state::AppState;
use crate::{
    auth, digests, extras, findings, jobs, lifecycle, messages, ops, scoring, search, topics,
};

pub fn router(state: AppState) -> Router {
    let router = Router::new()
        .route("/readyz", get(ops::readyz))
        .route("/metrics", get(ops::metrics))
        .route("/api/stats", get(ops::stats))
        .route("/api/topics", get(topics::list).post(topics::create))
        .route(
            "/api/topics/{t}",
            get(topics::get).patch(topics::patch).delete(topics::delete),
        )
        .route(
            "/api/topics/{t}/messages",
            get(messages::list).post(messages::post),
        )
        .route("/api/topics/{t}/threads", get(messages::threads))
        .route("/api/topics/{t}/events", get(topics::events))
        .route("/api/topics/{t}/findings", post(findings::batch_upsert))
        .route("/api/topics/{t}/distill-input", get(digests::distill_input))
        .route("/api/topics/{t}/digest", post(digests::write_digest))
        .route("/api/messages/{id}/thread", get(messages::thread))
        .route(
            "/api/messages/{id}",
            patch(messages::edit).delete(messages::delete),
        )
        .route("/api/findings", get(findings::list))
        .route(
            "/api/findings/{id}",
            get(findings::get).patch(findings::patch),
        )
        .route("/api/digests", get(digests::list))
        .route("/api/digests/{id}", get(digests::get))
        .route("/api/maintenance/sweep", post(lifecycle::sweep))
        .route("/api/brief", get(search::brief))
        .route("/api/search", get(search::search))
        .route("/api/leaderboard", get(scoring::leaderboard))
        .route(
            "/api/leaderboard/aliases",
            get(scoring::leaderboard_aliases),
        )
        .route("/api/score", post(scoring::score))
        .route("/api/route-hint", get(scoring::route_hint))
        .route(
            "/api/proposals",
            get(extras::list_proposals).post(extras::create_proposal),
        )
        .route("/api/proposals/{id}", patch(extras::patch_proposal))
        .route("/api/kv/{k}", get(extras::kv_get).put(extras::kv_put))
        .route("/api/gardener/runs", post(extras::create_run))
        .route("/api/gardener/runs/{id}", patch(extras::patch_run))
        .route("/api/jobs", get(jobs::list).post(jobs::create))
        .route("/api/jobs/claim", post(jobs::claim))
        .route("/api/jobs/{t}", get(jobs::get).patch(jobs::patch))
        .route("/api/jobs/{t}/amend", post(jobs::amend))
        .route("/api/jobs/{t}/complete", post(jobs::complete))
        .route("/api/jobs/{t}/verify", post(jobs::verify))
        .route("/api/jobs/{t}/reclaim", post(jobs::reclaim))
        .route("/api/jobs/{t}/cancel", post(jobs::cancel))
        .route("/api/jobs/{t}/retry", post(jobs::retry))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth::require_token,
        ))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            count_requests,
        ))
        .with_state(state);
    secure(router)
}

/// Browser hardening. `img-src` has no `https:`: remote images would let a message make
/// the viewer's browser contact other LAN services. Inline styles are allowed because
/// the SPA's component library sets them.
pub const CSP: &str = "default-src 'self'; img-src 'self' data:; connect-src 'self'; \
     style-src 'self' 'unsafe-inline'; frame-ancestors 'none'; base-uri 'none'";

/// Add the security headers to every response of `router` (API and SPA alike; the
/// binary wraps the SPA-attached router again, and `insert` makes that idempotent).
pub fn secure(router: Router) -> Router {
    router.layer(middleware::map_response(add_security_headers))
}

async fn add_security_headers(mut res: Response) -> Response {
    let h = res.headers_mut();
    h.insert(
        axum::http::header::CONTENT_SECURITY_POLICY,
        axum::http::HeaderValue::from_static(CSP),
    );
    h.insert(
        axum::http::header::X_CONTENT_TYPE_OPTIONS,
        axum::http::HeaderValue::from_static("nosniff"),
    );
    h.insert(
        axum::http::header::REFERRER_POLICY,
        axum::http::HeaderValue::from_static("no-referrer"),
    );
    res
}

/// Counts responses per route template (bounded cardinality) for `/metrics`.
async fn count_requests(State(st): State<AppState>, req: Request, next: Next) -> Response {
    let route = req
        .extensions()
        .get::<MatchedPath>()
        .map_or_else(|| "unmatched".to_string(), |m| m.as_str().to_string());
    let res = next.run(req).await;
    st.http.inc(&route, res.status().as_u16());
    res
}
