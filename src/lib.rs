//! Library interface for hive-board: Postgres-backed Hive message board.

use std::time::Duration;

use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;

pub mod api;
pub mod auth;
pub mod config;
pub mod db;
pub mod digests;
pub mod error;
pub mod extract;
pub mod extras;
pub mod findings;
pub mod jobs;
pub mod lifecycle;
pub mod messages;
pub mod models;
pub mod ops;
pub mod redact;
pub mod scoring;
pub mod search;
pub mod state;
pub mod topics;
pub mod util;

#[cfg(feature = "import")]
pub mod import;

/// Connect to Postgres. Fails if the server is unreachable (the caller exits
/// non-zero at boot); afterwards the pool reconnects by itself.
pub async fn connect(url: &str, max_connections: u32) -> Result<PgPool, sqlx::Error> {
    PgPoolOptions::new()
        .max_connections(max_connections)
        .min_connections(0)
        .acquire_timeout(Duration::from_secs(5))
        .idle_timeout(Duration::from_secs(300))
        .connect(url)
        .await
}

/// Apply the embedded migrations. `ignore_missing` lets this binary boot against a
/// database a newer release has already migrated (rollback is "re-pin the old image").
pub async fn migrate(pool: &PgPool) -> Result<(), sqlx::migrate::MigrateError> {
    let mut migrator = sqlx::migrate!("./migrations");
    migrator.set_ignore_missing(true);
    migrator.run(pool).await
}
