//! Test harness: each test gets its own freshly created Postgres database.
//! Needs HIVE_TEST_DATABASE_URL (an admin URL such as postgres://u:p@host:port/postgres).
//! Never point this at a live server.

#![allow(dead_code)]

use std::sync::atomic::{AtomicU32, Ordering};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use hive_board::config::{AuthConfig, JobsConfig, Lifecycle};
use hive_board::state::AppState;
use http_body_util::BodyExt;
use serde_json::Value;
use sqlx::{Connection, PgConnection, PgPool};
use tower::ServiceExt;

pub const TOKEN: &str = "test-token-0123456789-0123456789-abcdef";
pub const TOKEN_NEXT: &str = "next-token-0123456789-0123456789-abcde";

/// The `owns` roots the test server is configured with (`HIVE_JOB_OWNS_ROOTS`).
pub const OWNS_ROOT_SCRIPTS: &str = "/srv/example/scripts";
pub const OWNS_ROOT_GIT: &str = "/srv/example/git";

fn test_jobs() -> JobsConfig {
    JobsConfig {
        owns_roots: vec![OWNS_ROOT_SCRIPTS.to_string(), OWNS_ROOT_GIT.to_string()],
        ..JobsConfig::default()
    }
}

static COUNTER: AtomicU32 = AtomicU32::new(0);

pub struct TestApp {
    pub app: Router,
    pub pool: PgPool,
    admin_url: String,
    db_name: String,
}

fn admin_url() -> String {
    std::env::var("HIVE_TEST_DATABASE_URL")
        .expect("HIVE_TEST_DATABASE_URL must point at a throwaway Postgres")
}

fn with_db(url: &str, db: &str) -> String {
    let base = url.split('?').next().unwrap_or(url);
    let cut = base.rfind('/').expect("database url has a path");
    format!("{}/{}", &base[..cut], db)
}

impl TestApp {
    pub async fn new() -> Self {
        Self::with_lifecycle(Lifecycle::default()).await
    }

    pub async fn with_lifecycle(lifecycle: Lifecycle) -> Self {
        Self::build(lifecycle, false, test_jobs()).await
    }

    /// `HIVE_AUTH_READS=1` equivalent.
    pub async fn with_gated_reads() -> Self {
        Self::build(Lifecycle::default(), true, test_jobs()).await
    }

    /// The jobs settings an unconfigured server gets: no `owns` roots.
    pub async fn with_default_jobs() -> Self {
        Self::build(Lifecycle::default(), false, JobsConfig::default()).await
    }

    async fn build(lifecycle: Lifecycle, reads: bool, jobs: JobsConfig) -> Self {
        let admin = admin_url();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let db_name = format!(
            "hive_test_{}_{}_{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst),
            nanos % 1_000_000
        );
        let mut conn = PgConnection::connect(&admin).await.expect("admin connect");
        sqlx::query(&format!("CREATE DATABASE {db_name}"))
            .execute(&mut conn)
            .await
            .expect("create database");
        let pool = hive_board::connect(&with_db(&admin, &db_name), 4)
            .await
            .expect("connect test db");
        hive_board::migrate(&pool).await.expect("migrate");
        let state = AppState::new(
            pool.clone(),
            AuthConfig {
                off: false,
                token: Some(TOKEN.to_string()),
                token_next: Some(TOKEN_NEXT.to_string()),
                reads,
            },
            lifecycle,
        )
        .with_jobs(jobs);
        Self {
            app: hive_board::api::router(state),
            pool,
            admin_url: admin,
            db_name,
        }
    }

    /// Drop the database (best effort).
    pub async fn cleanup(self) {
        self.pool.close().await;
        if let Ok(mut conn) = PgConnection::connect(&self.admin_url).await {
            let _ = sqlx::query(&format!(
                "DROP DATABASE IF EXISTS {} WITH (FORCE)",
                self.db_name
            ))
            .execute(&mut conn)
            .await;
        }
    }

    pub async fn send(
        &self,
        method: &str,
        uri: &str,
        body: Option<Value>,
        token: Option<&str>,
        extra: &[(&str, &str)],
    ) -> (StatusCode, Value) {
        send_to(self.app.clone(), method, uri, body, token, extra).await
    }

    pub async fn get(&self, uri: &str) -> (StatusCode, Value) {
        self.send("GET", uri, None, None, &[]).await
    }

    /// Authenticated write.
    pub async fn write(&self, method: &str, uri: &str, body: Value) -> (StatusCode, Value) {
        self.send(method, uri, Some(body), Some(TOKEN), &[]).await
    }

    pub async fn topic(&self, name: &str) -> Value {
        let (s, v) = self
            .write(
                "POST",
                "/api/topics",
                serde_json::json!({"name": name, "title": format!("Title {name}"), "author": "tester"}),
            )
            .await;
        assert_eq!(s, StatusCode::CREATED, "{v}");
        v
    }

    pub async fn post(&self, topic: &str, body: Value) -> Value {
        let (s, v) = self
            .write("POST", &format!("/api/topics/{topic}/messages"), body)
            .await;
        assert_eq!(s, StatusCode::CREATED, "{v}");
        v
    }
}

/// Send one request to a router (cloneable, so tests can fire many in parallel tasks).
pub async fn send_to(
    app: Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
    token: Option<&str>,
    extra: &[(&str, &str)],
) -> (StatusCode, Value) {
    let mut b = Request::builder().method(method).uri(uri);
    if let Some(t) = token {
        b = b.header("authorization", format!("Bearer {t}"));
    }
    for (k, v) in extra {
        b = b.header(*k, *v);
    }
    let req = match body {
        Some(v) => b
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&v).expect("json")))
            .expect("request"),
        None => b.body(Body::empty()).expect("request"),
    };
    let res = app.oneshot(req).await.expect("response");
    let status = res.status();
    let bytes = res.into_body().collect().await.expect("body").to_bytes();
    let json = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).to_string()));
    (status, json)
}

impl TestApp {
    /// GET returning the response headers (for security-header checks).
    pub async fn get_headers(
        &self,
        uri: &str,
        token: Option<&str>,
    ) -> (StatusCode, axum::http::HeaderMap) {
        let mut b = Request::builder().method("GET").uri(uri);
        if let Some(t) = token {
            b = b.header("authorization", format!("Bearer {t}"));
        }
        let res = self
            .app
            .clone()
            .oneshot(b.body(Body::empty()).expect("request"))
            .await
            .expect("response");
        (res.status(), res.headers().clone())
    }
}
