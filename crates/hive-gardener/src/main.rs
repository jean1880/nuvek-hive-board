//! hive-gardener: the Hive's learning-loop worker. STATELESS: run it from a timer anywhere that
//! can reach hive-board.
//!
//! Nothing is stored here: cursors live in hive-board (`topics.distilled_through`, `kv` keys), and
//! every write goes through its HTTP API with `Authorization: Bearer $HIVE_API_TOKEN` (never a
//! database connection). The Neo4j projection is the one other write target, confined to the
//! `:HIVE` tenant (see `graph.rs`).
//!
//! Logs: one journald line per stage (identifier `hive_gardener`), mirrored on stderr.
//!
//! Exit codes: 0 when every stage succeeded, 1 when a stage failed or the HTTP client could not
//! be built, 2 when the configuration is invalid (`HIVE_TOPOLOGY_LABEL`), 3 when the
//! secret-surface configuration cannot be loaded.

mod api;
mod bridge;
mod distill;
mod graph;
mod logic;
mod stages;

use anyhow::Result;
use clap::{Parser, Subcommand};
use nuvek_core::log::{get_logger, Logger};
use reqwest::Method;
use serde_json::{json, Value};
use std::future::Future;

/// Exit code when `nuvek_core::scrub::init()` fails (a bad or unreadable secret-surfaces file).
const EXIT_SURFACES: i32 = 3;
/// Exit code for an invalid setting (an invalid `HIVE_TOPOLOGY_LABEL`).
const EXIT_CONFIG: i32 = 2;

/// This binary's endpoint and model settings: a variable name plus a default, in one place.
/// Every default is `localhost`; set the variables to reach remote services.
mod endpoints {
    use nuvek_core::env::{env_or, env_parse_or, env_var};

    /// Fallback host when a per-service variable is unset.
    pub const DEFAULT_HOST: &str = "localhost";

    fn url(var: &str, default: String) -> String {
        env_or(var, default).trim_end_matches('/').to_string()
    }

    /// hive-board (same variable and default as hive-mcp).
    pub fn hive_url() -> String {
        url("HIVE_SERVER_URL", format!("http://{DEFAULT_HOST}:8904"))
    }

    /// Bearer token for writes. Never logged; `None` is only legal when the server runs
    /// `HIVE_AUTH=off`.
    pub fn hive_token() -> Option<String> {
        env_var("HIVE_API_TOKEN")
    }

    /// Source of the incident bridge (`HIVE_GARDENER_BRIDGE_URL`). No default: unset means the
    /// `bridge` stage is skipped. Use HTTPS for a remote host, since the bridge sends a bearer
    /// token.
    pub fn bridge_url() -> Option<String> {
        env_var("HIVE_GARDENER_BRIDGE_URL").map(|v| v.trim_end_matches('/').to_string())
    }

    /// Bearer for the bridge source's reads (`HIVE_GARDENER_BRIDGE_TOKEN`). Never logged;
    /// `None` is only legal while the source runs without authentication.
    pub fn bridge_token() -> Option<String> {
        env_var("HIVE_GARDENER_BRIDGE_TOKEN")
    }

    pub fn prometheus_url() -> String {
        url("PROMETHEUS_URL", format!("http://{DEFAULT_HOST}:9090"))
    }

    pub fn ollama_api() -> String {
        format!(
            "{}/api",
            url("OLLAMA_URL_BASE", format!("http://{DEFAULT_HOST}:11434"))
        )
    }

    pub fn neo4j_uri() -> String {
        url("NEO4J_HOST", format!("bolt://{DEFAULT_HOST}:7687"))
    }

    pub fn neo4j_user() -> String {
        env_or("NEO4J_USER", "neo4j")
    }

    /// Never defaulted, never logged.
    pub fn neo4j_password() -> Option<String> {
        env_var("NEO4J_PASSWORD")
    }

    /// `HIVE_TOPOLOGY_LABEL`: the label of the topology graph findings link to. No default:
    /// unset (or blank) is `Ok(None)`, topology linking off. An invalid value is `Err`, which
    /// the binary treats as a startup error.
    pub fn topology_label() -> Result<Option<crate::graph::TopologyLabel>, String> {
        env_var("HIVE_TOPOLOGY_LABEL")
            .map(|v| crate::graph::TopologyLabel::parse(v.trim()))
            .transpose()
    }

    /// Hours of silence after which a topic gets the keyword digest while the LLM is down.
    pub fn keyword_fallback_hours() -> i64 {
        env_parse_or("GARDENER_KEYWORD_FALLBACK_H", 24)
    }

    /// Hours of bridge-source history to read when `bridge.cursor` does not exist yet.
    pub fn bridge_lookback_h() -> i64 {
        env_parse_or("GARDENER_BRIDGE_LOOKBACK_H", 24)
    }

    /// Consecutive failures that open an incident topic.
    pub fn bridge_min_failures() -> usize {
        env_parse_or("GARDENER_BRIDGE_MIN_FAILURES", 2)
    }

    /// Minutes a lone failure must stay unanswered by a success before it opens a topic.
    pub fn bridge_grace_min() -> i64 {
        env_parse_or("GARDENER_BRIDGE_GRACE_MIN", 15)
    }

    /// Wall-clock work budget in seconds. Must stay below the systemd unit's TimeoutStartSec
    /// (1800 s) so a slow run defers topics instead of being killed mid-write.
    pub fn run_budget_s() -> u64 {
        env_parse_or("GARDENER_RUN_BUDGET_S", 1500)
    }

    /// LiteLLM gateway, attributed to the automation key. `None` (unset, or `LITELLM_DISABLED=1`)
    /// routes direct to Ollama.
    pub fn gateway() -> Option<nuvek_ollama::GatewayConfig> {
        nuvek_ollama::GatewayConfig::from_env(
            &["LITELLM_API_BASE", "LITELLM_URL"],
            "LITELLM_AUTOMATION_KEY",
            "LITELLM_DISABLED",
        )
    }
}

mod clients {
    use nuvek_core::env::{env_or, env_parse_or};

    pub fn llm_config() -> nuvek_ollama::LlmConfig {
        let model = env_or("GARDENER_EXTRACT_MODEL", "qwen3.5:9b");
        nuvek_ollama::LlmConfig {
            api_base: crate::endpoints::ollama_api(),
            summarize_model: model.clone(),
            extract_model: model,
            extract_num_predict: env_parse_or("GARDENER_EXTRACT_NUM_PREDICT", 2048),
            keep_alive: env_or("NUVEK_KEEP_ALIVE", "5m"),
            gateway: crate::endpoints::gateway(),
        }
    }
}

/// Shared by every stage.
pub struct Ctx {
    pub hive: api::Hive,
    pub log: Logger,
    /// Wall-clock end of this invocation's work budget (`GARDENER_RUN_BUDGET_S`). Stages that
    /// loop over many items stop starting new ones after it and defer the rest to the next run.
    pub deadline: std::time::Instant,
    /// `HIVE_TOPOLOGY_LABEL`, validated. `None` turns topology linking off: no allowlist query,
    /// no subject resolution, no `CONCERNS` edges.
    pub topology: Option<graph::TopologyLabel>,
}

impl Ctx {
    pub fn expired(&self) -> bool {
        std::time::Instant::now() >= self.deadline
    }
}

/// What a stage reports: a one-line summary and counters for the `gardener_runs` row.
#[derive(Default, Debug)]
pub struct Outcome {
    pub summary: String,
    pub swept: i64,
    pub distilled: i64,
    pub findings: i64,
    pub quarantined: i64,
}

#[derive(Parser)]
#[command(
    name = "hive-gardener",
    version,
    about = "Hive learning-loop worker: sweep, distill, graph, proposals, bridge, cost"
)]
struct Cli {
    /// Plan only: no writes anywhere (hive-board, Neo4j). Reads and the LLM still run.
    #[arg(long, global = true)]
    dry_run: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Clone, Copy)]
enum Cmd {
    /// Apply the lifecycle transitions server-side (quiet / auto-resolve / archive / compact).
    Sweep,
    /// Distill resolved or quiet topics into typed findings and a digest.
    Distill,
    /// Project findings into Neo4j under the :HIVE tenant.
    Graph {
        /// Re-merge every finding, not just unsynced ones (idempotent). Use after the topology
        /// graph recreated its nodes.
        #[arg(long)]
        resync: bool,
    },
    /// Propose a gotcha-note for recurring, confirmed gotchas.
    Proposals,
    /// Turn failed runs from the bridge source into incident topics.
    Bridge,
    /// Record Claude token spend per identity (or why it is unavailable).
    Cost,
    /// Every stage in order, recorded in gardener_runs.
    Run,
}

/// One runnable stage; `Cmd::Run` is the sequence of all of them.
#[derive(Clone, Copy)]
enum Stage {
    Sweep,
    Distill,
    Graph { resync: bool },
    Proposals,
    Bridge,
    Cost,
}

impl Cmd {
    fn stage(self) -> Option<Stage> {
        Some(match self {
            Cmd::Sweep => Stage::Sweep,
            Cmd::Distill => Stage::Distill,
            Cmd::Graph { resync } => Stage::Graph { resync },
            Cmd::Proposals => Stage::Proposals,
            Cmd::Bridge => Stage::Bridge,
            Cmd::Cost => Stage::Cost,
            Cmd::Run => return None,
        })
    }
}

impl Stage {
    fn name(self) -> &'static str {
        match self {
            Stage::Sweep => "sweep",
            Stage::Distill => "distill",
            Stage::Graph { .. } => "graph",
            Stage::Proposals => "proposals",
            Stage::Bridge => "bridge",
            Stage::Cost => "cost",
        }
    }
}

/// Run one stage, log its single journald line, and return (ok, outcome).
async fn stage<F>(ctx: &Ctx, name: &str, fut: F) -> (bool, Outcome)
where
    F: Future<Output = Result<Outcome>>,
{
    match fut.await {
        Ok(o) => {
            ctx.log.info(&format!("stage={name} ok=true {}", o.summary));
            (true, o)
        }
        Err(e) => {
            ctx.log.error(&format!("stage={name} ok=false error={e:#}"));
            (
                false,
                Outcome {
                    summary: format!("error: {e:#}"),
                    ..Default::default()
                },
            )
        }
    }
}

async fn one(ctx: &Ctx, st: Stage) -> (bool, Outcome) {
    let name = st.name();
    match st {
        Stage::Sweep => stage(ctx, name, stages::sweep(ctx)).await,
        Stage::Distill => stage(ctx, name, distill::run(ctx)).await,
        Stage::Graph { resync } => stage(ctx, name, graph::sync(ctx, resync)).await,
        Stage::Proposals => stage(ctx, name, stages::proposals(ctx)).await,
        Stage::Bridge => stage(ctx, name, bridge::run(ctx)).await,
        Stage::Cost => stage(ctx, name, stages::cost(ctx)).await,
    }
}

/// sweep -> bridge -> distill -> graph -> proposals -> cost. A failing stage is logged and the
/// next one still runs; the run reports failure (non-zero exit) if ANY stage failed, so a
/// persistently broken stage is visible to systemd.
async fn run_all(ctx: &Ctx) -> bool {
    let run_id = start_run(ctx).await;
    let order = [
        Stage::Sweep,
        Stage::Bridge,
        Stage::Distill,
        Stage::Graph { resync: false },
        Stage::Proposals,
        Stage::Cost,
    ];
    let mut total = Outcome::default();
    let (mut ok_n, mut notes) = (0usize, Vec::new());
    let mut last = "";
    for cmd in order {
        let (ok, o) = one(ctx, cmd).await;
        if ok {
            ok_n += 1;
        }
        last = cmd.name();
        total.swept += o.swept;
        total.distilled += o.distilled;
        total.findings += o.findings;
        total.quarantined += o.quarantined;
        notes.push(format!(
            "{}: {} {}",
            cmd.name(),
            if ok { "ok" } else { "FAILED" },
            o.summary
        ));
    }
    finish_run(
        ctx,
        run_id,
        ok_n == order.len(),
        last,
        &total,
        &notes.join(" | "),
    )
    .await;
    ctx.log.info(&format!(
        "run done: {ok_n}/{} stages ok swept={} distilled={} findings={} quarantined={}",
        order.len(),
        total.swept,
        total.distilled,
        total.findings,
        total.quarantined
    ));
    ok_n == order.len()
}

async fn start_run(ctx: &Ctx) -> Option<i64> {
    match ctx
        .hive
        .write(
            &ctx.log,
            Method::POST,
            "/api/gardener/runs",
            &json!({"stage": "start"}),
            None,
        )
        .await
    {
        Ok(Some(r)) => r.body.get("id").and_then(Value::as_i64),
        Ok(None) => None,
        Err(e) => {
            ctx.log
                .warn(&format!("could not record the run start: {e}"));
            None
        }
    }
}

async fn finish_run(ctx: &Ctx, id: Option<i64>, ok: bool, stage: &str, t: &Outcome, notes: &str) {
    let Some(id) = id else { return };
    let body = json!({
        "finished": true,
        "ok": ok,
        "stage": stage,
        "topics_swept": t.swept,
        "topics_distilled": t.distilled,
        "findings_written": t.findings,
        "quarantined": t.quarantined,
        "notes": logic::truncate_chars(notes, 1500),
    });
    if let Err(e) = ctx
        .hive
        .write(
            &ctx.log,
            Method::PATCH,
            &format!("/api/gardener/runs/{id}"),
            &body,
            None,
        )
        .await
    {
        ctx.log.warn(&format!("could not record the run end: {e}"));
    }
}

#[tokio::main]
async fn main() {
    // Parse first: `--help` / bad flags exit here, before anything else happens.
    let cli = Cli::parse();
    // This binary feeds thread text to a model: load the secret surfaces before any of it is
    // read. A broken surfaces file is fatal, never a silent fallback to the built-ins.
    if let Err(e) = nuvek_core::scrub::init() {
        eprintln!("hive-gardener: secret surfaces: {e}");
        std::process::exit(EXIT_SURFACES);
    }
    let log = get_logger("hive_gardener");
    let topology = match endpoints::topology_label() {
        Ok(t) => t,
        Err(e) => {
            log.error(&e);
            std::process::exit(EXIT_CONFIG);
        }
    };
    if topology.is_none() {
        log.info("HIVE_TOPOLOGY_LABEL is not set: topology linking is off (no subject resolution, no CONCERNS edges)");
    }
    let hive = match api::Hive::new(endpoints::hive_url(), endpoints::hive_token(), cli.dry_run) {
        Ok(h) => h,
        Err(e) => {
            log.error(&format!("cannot build the HTTP client: {e:#}"));
            std::process::exit(1);
        }
    };
    if cli.dry_run {
        log.info("DRY-RUN: no writes to hive-board or Neo4j");
    }
    let ctx = Ctx {
        hive,
        log,
        deadline: std::time::Instant::now()
            + std::time::Duration::from_secs(endpoints::run_budget_s()),
        topology,
    };
    let ok = match cli.cmd.stage() {
        Some(st) => one(&ctx, st).await.0,
        None => run_all(&ctx).await,
    };
    if !ok {
        std::process::exit(1);
    }
}
