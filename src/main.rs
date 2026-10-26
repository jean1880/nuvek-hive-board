//! hive-board: web service and API server for the Hive message board.
//!
//! Subcommands: (none) serve, `healthcheck`, and `import-sqlite` (feature `import`).

use std::time::Duration;

use anyhow::{bail, Context, Result};
use hive_board::config::Config;
use hive_board::state::AppState;
use nuvek_web::{serve, telemetry};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn main() -> Result<()> {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()?
        .block_on(async_main())
}

async fn async_main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None | Some("serve") => run_server().await,
        Some("healthcheck") => healthcheck().await,
        Some("import-sqlite") => import_cmd(&args[1..]).await,
        Some(other) => {
            bail!("unknown subcommand '{other}' (expected: serve, healthcheck, import-sqlite)")
        }
    }
}

/// GET http://127.0.0.1:$PORT/readyz with a short timeout. The runtime image
/// has no curl, so this is a plain TCP request. Exit 0 on 200, else 1.
async fn healthcheck() -> Result<()> {
    let port = hive_board::config::port_from_env();
    let probe = async {
        let mut s = tokio::net::TcpStream::connect(format!("127.0.0.1:{port}")).await?;
        s.write_all(b"GET /readyz HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
            .await?;
        let mut buf = Vec::new();
        s.take(2048).read_to_end(&mut buf).await?;
        Ok::<_, std::io::Error>(String::from_utf8_lossy(&buf).starts_with("HTTP/1.1 200"))
    };
    match tokio::time::timeout(Duration::from_secs(3), probe).await {
        Ok(Ok(true)) => Ok(()),
        _ => std::process::exit(1),
    }
}

async fn import_cmd(args: &[String]) -> Result<()> {
    #[cfg(not(feature = "import"))]
    {
        let _ = args;
        bail!("this build lacks the `import` feature; rebuild with --features import");
    }
    #[cfg(feature = "import")]
    {
        let (mut from, mut source, mut digests) = (None, None, None);
        let mut it = args.iter();
        while let Some(a) = it.next() {
            let v = it.next().with_context(|| format!("{a} needs a value"))?;
            match a.as_str() {
                "--from" => from = Some(v.clone()),
                "--source" => source = Some(v.clone()),
                "--digests" => digests = Some(v.clone()),
                other => bail!("unknown option {other}"),
            }
        }
        let from = from.context("--from <path> is required")?;
        let source = source.context("--source <label> is required")?;
        telemetry::init();
        let cfg = Config::from_env()?;
        let pool = connect_and_migrate(&cfg).await?;
        let rep = hive_board::import::import_sqlite(
            &pool,
            std::path::Path::new(&from),
            &source,
            digests.as_deref().map(std::path::Path::new),
        )
        .await?;
        println!("{}", serde_json::to_string_pretty(&rep)?);
        Ok(())
    }
}

async fn connect_and_migrate(cfg: &Config) -> Result<sqlx::PgPool> {
    // Never print the sqlx error here: keep the URL and any credential out of logs.
    let pool = match hive_board::connect(&cfg.database_url, cfg.db_max_connections).await {
        Ok(p) => p,
        Err(_) => {
            bail!("cannot connect to Postgres (check HIVE_DATABASE_URL and that the server is up)")
        }
    };
    hive_board::migrate(&pool)
        .await
        .context("applying database migrations")?;
    Ok(pool)
}

async fn run_server() -> Result<()> {
    telemetry::init();

    let cfg = Config::from_env()?;
    if cfg.auth.off {
        tracing::warn!("HIVE_AUTH=off: write routes are NOT authenticated");
    }
    tracing::info!(bind = %cfg.bind, port = cfg.port, "initializing hive-board");

    let pool = connect_and_migrate(&cfg).await?;
    tracing::info!("postgres connected, migrations applied");

    let state = AppState::new(pool, cfg.auth, cfg.lifecycle).with_jobs(cfg.jobs);
    let api_router = hive_board::api::router(state).merge(nuvek_web::health::routes());

    let assets = match nuvek_web::config::optional_env("DIST_DIR") {
        Some(d) if !d.trim().is_empty() => serve::SpaAssets::new(d),
        _ => serve::SpaAssets::detect(&["/app/dist", "frontend/dist", "dist"]),
    };
    // Wrap again so the SPA fallback carries the security headers too.
    let app = hive_board::api::secure(serve::attach_spa(api_router, assets));

    let addr = format!("{}:{}", cfg.bind, cfg.port);
    tracing::info!("hive-board listening on http://{addr}");
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .with_context(|| format!("failed to bind address {addr}"))?;
    axum::serve(listener, app).await.context("server error")?;
    Ok(())
}
