//! The default network trust model, checked without a database: a server configured with only
//! the required variables refuses anonymous reads and writes. The pool is lazy and points at a
//! closed port, so a request that got past the auth layer would fail with a database error
//! (never 401), which keeps the two outcomes distinct.

use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use hive_board::config::Config;
use hive_board::state::AppState;
use sqlx::postgres::PgPoolOptions;
use tower::ServiceExt;

// Built from parts so secret scanners never see a token-shaped literal.
fn token() -> String {
    ["default", "trust", "token", "0123456789abcdef"].join("-") // gitleaks:allow (test token)
}

fn default_app() -> Router {
    let tok = token();
    let get = move |k: &str| match k {
        "HIVE_DATABASE_URL" => Some("postgres://hive@127.0.0.1:1/hive".to_string()),
        "HIVE_API_TOKEN" => Some(tok.clone()),
        _ => None,
    };
    let cfg = Config::from_lookup(&get).expect("config from the required variables only");
    assert_eq!(cfg.bind, "127.0.0.1", "default bind is loopback");
    assert!(cfg.auth.reads, "reads are gated by default");
    let pool = PgPoolOptions::new()
        .acquire_timeout(Duration::from_millis(300))
        .connect_lazy(&cfg.database_url)
        .expect("lazy pool");
    let state = AppState::new(pool, cfg.auth, cfg.lifecycle).with_jobs(cfg.jobs);
    hive_board::api::secure(hive_board::api::router(state))
}

async fn status(app: Router, method: &str, uri: &str, bearer: Option<&str>) -> StatusCode {
    let mut b = Request::builder().method(method).uri(uri);
    if let Some(t) = bearer {
        b = b.header("authorization", format!("Bearer {t}"));
    }
    let body = if method == "GET" {
        Body::empty()
    } else {
        b = b.header("content-type", "application/json");
        Body::from(r#"{"name":"t-1","title":"T","author":"anon"}"#)
    };
    app.oneshot(b.body(body).expect("request"))
        .await
        .expect("response")
        .status()
}

#[tokio::test]
async fn anonymous_get_is_refused_by_default() {
    for uri in ["/api/topics", "/api/stats", "/api/findings", "/metrics"] {
        assert_eq!(
            status(default_app(), "GET", uri, None).await,
            StatusCode::UNAUTHORIZED,
            "{uri}"
        );
    }
}

#[tokio::test]
async fn anonymous_post_is_refused_by_default() {
    assert_eq!(
        status(default_app(), "POST", "/api/topics", None).await,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn wrong_token_is_refused_and_right_token_passes_the_gate() {
    assert_eq!(
        status(default_app(), "GET", "/api/topics", Some("wrong")).await,
        StatusCode::UNAUTHORIZED
    );
    // Past the gate, the unreachable database answers; anything but 401 proves the token worked.
    let s = status(default_app(), "GET", "/api/topics", Some(&token())).await;
    assert_ne!(s, StatusCode::UNAUTHORIZED);
}
