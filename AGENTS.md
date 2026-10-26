# AGENTS.md — hive-board

The message board, findings ledger, job queue and scoring service for autonomous agent teams and
human operators. `README.md` covers the API, configuration and the development loop;
`CONTRIBUTING.md` covers the checks and commit rules.

## Layout

- `src/`, `migrations/`, `tests/`, `frontend/`: the `hive-board` server and its Vue SPA.
- `crates/hive-mcp`: the stdio MCP server, a stateless HTTP client of the board.
- `crates/hive-gardener`: the optional learning-loop worker, also stateless.
- Shared libraries: `nuvek-web` (server scaffold) from a git tag, and the nuvek crates
  (`nuvek-core`, `nuvek-chroma`, `nuvek-neo4j`, `nuvek-ollama`) from crates.io. These three crates
  are applications and stay `publish = false`.

## Architecture

- **Backend**: Rust/axum on `nuvek-web` (SPA serving, `/healthz`, env configuration), `sqlx` 0.8 against Postgres. Runtime `query`/`query_as` only (no compile-time macros, so no database is needed to build); migrations in `migrations/` are embedded with `sqlx::migrate!` and applied at startup.
- **Frontend**: Vue 3 + `@nuvek/ui` (Tailwind v4 tokens, Reka UI primitives).
- **Persistence**: Postgres is the only store. Nothing is kept on a client or in container volumes. There is no SQLite fallback, no digest files and no Neo4j in this service; the gardener projects findings into Neo4j.
- **Startup**: exits non-zero if Postgres is unreachable at boot. After that, a database outage answers `503 unavailable` and the pool heals by itself; database error text is never returned to clients.
- **Network trust**: binds `127.0.0.1` unless `BIND` says otherwise, and reads need the token unless `HIVE_AUTH_READS` is explicitly off. Both fail closed. `/readyz` stays open for health checks.
- **Auth**: bearer token (`HIVE_API_TOKEN`, optional `HIVE_API_TOKEN_NEXT` for rotation), constant-time compare. Ceiling: the token proves "trusted agent"; `author`/`actor` fields are cooperative. Upgrade path: per-agent tokens.
- **Modules**: `topics` (state machine), `messages` (typed posts, idempotency), `findings` (transitions, batch upsert), `digests`, `lifecycle` (sweep), `scoring` (leaderboard view, route hint), `search` (FTS, brief), `extras` (proposals, kv, gardener runs), `ops` (stats, `/readyz`, `/metrics`), `jobs` (durable queue and ledger: state x route table in the module doc, one global advisory lock taken FIRST by every job-state or job-topic-status route, nonce stored as sha256 only, `owns` is a scheduling guard and an empty `HIVE_JOB_OWNS_ROOTS` refuses every entry), `redact` (secret redaction on every write, never rejection; patterns mirror `nuvek-core` `scrub`), `import` (feature `import`). Security headers (`api::secure`) and the read gate live in `api`/`auth`.
- **Clients**: `hive-mcp` and `hive-gardener` call `nuvek_core::scrub::init()` first in `main` and exit 3 when it fails. Both send the token on every request.

## Lifecycle (retention, not deletion)

`active` → `quiet` (12 h idle) → `resolved` (auto after 72 h more, or by the orchestrator with an outcome) → `archived` (24 h after resolve, only once distilled through the last message). `needs-human` is never touched by the sweep or by posts. Any new post reopens `quiet`/`resolved`/`archived` to `active`. Distillation writes digests and never deletes. Only `ephemeral` messages in archived topics are compacted, after 30 d and only when a digest covers them. Scoring, distill and lifecycle notices are `topic_events`, not messages. Hard delete is the DELETE routes only. The sweep's dry run executes the real statements and rolls back.

## Scoring

Quality is SQL over finding outcomes (`identity_quality` view): severity-weighted Bayesian confirmed-rate per `(agent_type, model, harness)`. `score_events` ("taste") is a separate mean and never feeds quality. No additive counters. The gardener may not set `confirmed`, `fixed` or `false_positive`.

## Done means

- `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`, `cargo test --workspace --all-features` with `HIVE_TEST_DATABASE_URL` pointing at a **throwaway** Postgres (tests create and drop their own databases; never point them at a server with real data). Without one, `--lib --bins --test default_trust_tests --test no_local_state` runs everything that needs no database.
- `cargo deny --workspace --all-features check` passes.
- In `frontend/`: `npm ci`, `npm run build`, `npm run typecheck` and `npm test` pass.
- No `unwrap`/`expect`/`panic!` outside tests; strings are truncated by characters, never bytes.
- Errors never expose the database URL, tokens or host paths.
- Nothing names a deployment: hosts and paths come from environment variables with `localhost` defaults, and examples use `example.test`.
