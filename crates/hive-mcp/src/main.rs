//! hive-mcp — Hive board MCP server over rmcp stdio.
//!
//! A stateless, thin HTTP client of hive-board. Nothing is stored locally: when the server is
//! unreachable every tool returns an error and writes nothing. Responses are passed through as
//! JSON, so minor backend additions never break the client.
//! **stdout is the JSON-RPC channel** — logging goes to stderr via tracing.
//!
//! Exit codes: 0 on a clean shutdown, 1 on a runtime error, 2 ([`EXIT_CONFIG`]) on an invalid
//! `HIVE_TOPOLOGY_LABEL`, 3 ([`EXIT_SURFACES`]) when the secret-surface configuration cannot be
//! loaded.

mod brief;
mod client;
mod endpoints;
mod jobs;

use anyhow::Result;
use client::{idem_key, seg, Hive};
use nuvek_core::mcp::stdio;
use reqwest::Method;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{ServerCapabilities, ServerInfo};
use rmcp::{tool, tool_handler, tool_router, ServerHandler, ServiceExt};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};

type Out = std::result::Result<String, String>;

/// Exit code when `nuvek_core::scrub::init()` fails (a bad or unreadable secret-surfaces file).
const EXIT_SURFACES: i32 = 3;

/// Exit code for an invalid setting (an invalid `HIVE_TOPOLOGY_LABEL`).
const EXIT_CONFIG: i32 = 2;

#[derive(Deserialize, JsonSchema)]
struct CreateTopicArgs {
    /// Topic slug: ^[a-z][a-z0-9-]{1,79}$ (lowercase letters, digits, hyphens; e.g. 'auth-refactor')
    name: String,
    /// Human-readable topic title
    title: String,
    /// Chosen name of the creating agent
    author: String,
    /// Optional summary or mission description (also feeds the prior-context brief)
    description: Option<String>,
    /// Optional kind: task (default), incident, report, design, review, research
    kind: Option<String>,
    /// YOUR agent type, e.g. "rust-reviewer" or "general-purpose" (defaults to HIVE_AGENT_TYPE, then "unknown")
    agent_type: Option<String>,
    /// YOUR model tier, e.g. "sonnet", "opus", "haiku" (defaults to HIVE_MODEL, then "unknown")
    model: Option<String>,
    /// YOUR harness: claude, agy, ollama, human (defaults to HIVE_HARNESS, then "unknown")
    harness: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct ListTopicsArgs {
    /// active, quiet, needs-human, resolved, archived, open (everything but archived; default) or all
    status: Option<String>,
    /// Optional kind filter: task, incident, report, design, review, research
    kind: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct TopicArg {
    /// Topic slug or integer ID
    topic: String,
}

#[derive(Deserialize, JsonSchema)]
struct UpdateTopicArgs {
    /// Topic slug or integer ID
    topic: String,
    /// New title
    title: Option<String>,
    /// New description
    description: Option<String>,
    /// New status: active, quiet, needs-human, resolved, archived (illegal transitions return 409)
    status: Option<String>,
    /// Free-text resolution, normally set via board_resolve_topic
    outcome: Option<String>,
    /// Who is making the change (your chosen name)
    actor: String,
}

#[derive(Deserialize, JsonSchema)]
struct PostMessageArgs {
    /// Topic slug or integer ID
    topic: String,
    /// Chosen name of the agent posting
    author: String,
    /// Message body (markdown supported). The server stores the FULL content as the claim, so put a one-line claim first, then detail.
    content: String,
    /// Optional parent message ID to reply to
    parent_id: Option<i64>,
    /// Optional tags
    tags: Option<Vec<String>>,
    /// note (default), finding, decision, gotcha, question, verdict, progress. The first five typed kinds also become a tracked finding
    kind: Option<String>,
    /// BLOCKER, SHOULD or NICE. REQUIRED for kind finding, gotcha and verdict
    severity: Option<String>,
    /// Things this concerns: repo, container, package or skill names (e.g. ["hive-board","postgres"])
    subjects: Option<Vec<String>>,
    /// Proof for the claim: a command and its output, file:line, a log excerpt
    evidence: Option<String>,
    /// Kind of task this contributes to (used by board_route_hint), e.g. "rust-review"
    task_kind: Option<String>,
    /// True for progress/heartbeat chatter that may be compacted into a digest later
    ephemeral: Option<bool>,
    /// YOUR agent type, e.g. "rust-reviewer" or "general-purpose" (defaults to HIVE_AGENT_TYPE, then "unknown")
    agent_type: Option<String>,
    /// YOUR model tier, e.g. "sonnet", "opus", "haiku" (defaults to HIVE_MODEL, then "unknown")
    model: Option<String>,
    /// YOUR harness: claude, agy, ollama, human (defaults to HIVE_HARNESS, then "unknown")
    harness: Option<String>,
    /// Idempotency key for this post. Omit for a fresh one; REUSE THE SAME KEY when retrying after a timeout so the post cannot duplicate
    idempotency_key: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct GetMessagesArgs {
    /// Topic slug or integer ID
    topic: String,
    /// Optional parent message ID (replies to one message)
    parent_id: Option<i64>,
    /// Maximum number of messages (default 50, max 500)
    limit: Option<usize>,
    /// Only messages with an ID greater than this (poll for new posts)
    after: Option<i64>,
    /// Only these kinds, comma-separated (e.g. "finding,verdict"); unknown kinds are rejected by the server
    kind: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct GetThreadArgs {
    /// Root message ID
    message_id: i64,
}

#[derive(Deserialize, JsonSchema)]
struct UpdateMessageArgs {
    /// Message ID
    message_id: i64,
    /// Author name (must match the original author)
    author: String,
    /// Updated content
    content: String,
}

#[derive(Deserialize, JsonSchema)]
struct DeleteMessageArgs {
    /// Message ID to delete
    message_id: i64,
}

#[derive(Deserialize, JsonSchema)]
struct SearchArgs {
    /// Full-text query across topics, messages, findings and digests
    query: String,
    /// Optional topic slug filter
    topic: Option<String>,
    /// Optional author filter
    author: Option<String>,
    /// Max results (default 20)
    limit: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
struct BriefArgs {
    /// Comma-free list of subject names (repos, containers, packages, skills)
    subjects: Option<Vec<String>>,
    /// Free-text description of the task
    query: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct ListFindingsArgs {
    /// open, confirmed, fixed, false_positive, wontfix, superseded, expired
    status: Option<String>,
    /// BLOCKER, SHOULD or NICE
    severity: Option<String>,
    /// finding, decision, gotcha, verdict, question
    kind: Option<String>,
    /// Subject name the finding concerns
    subject: Option<String>,
    /// Topic slug
    topic: Option<String>,
    /// Max results (default 50)
    limit: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
struct SetFindingStatusArgs {
    /// Finding ID (from board_list_findings or a posted typed message)
    finding_id: i64,
    /// confirmed, fixed, false_positive, wontfix, superseded, expired, or open (reopen)
    status: String,
    /// Orchestrator name making the call
    actor: String,
    /// Why (verification evidence)
    note: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct ResolveTopicArgs {
    /// Topic slug or integer ID
    topic: String,
    /// What was decided or delivered
    outcome: String,
    /// Orchestrator name
    actor: String,
}

#[derive(Deserialize, JsonSchema)]
struct ScoreAgentArgs {
    /// Chosen name of the agent to score
    agent_name: String,
    /// Taste score 0-100
    score: i64,
    /// Evaluation details and rationale
    reason: String,
    /// Optional topic where the contribution occurred
    topic: Option<String>,
    /// Award 'Top Contributor' (default: true if score >= 80)
    award_top_contributor: Option<bool>,
    /// Name of the scorer (the orchestrator's chosen name)
    scorer: Option<String>,
    /// Identity of the AGENT BEING SCORED: agent type, e.g. "rust-reviewer" (defaults to HIVE_AGENT_TYPE, then "unknown")
    agent_type: Option<String>,
    /// Identity of the agent being scored: model, e.g. "sonnet" (defaults to HIVE_MODEL, then "unknown")
    model: Option<String>,
    /// Identity of the agent being scored: harness, e.g. "claude" (defaults to HIVE_HARNESS, then "unknown")
    harness: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct LimitArgs {
    /// Max rows (max 100, default 25)
    limit: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
struct RouteHintArgs {
    /// Kind of task you are about to delegate, e.g. "rust-review"
    task_kind: String,
}

#[derive(Deserialize, JsonSchema)]
struct ListProposalsArgs {
    /// proposed, approved, rejected, applied, expired
    status: Option<String>,
}

#[derive(Clone)]
struct HiveBoardServer {
    tool_router: ToolRouter<Self>,
    hive: Hive,
}

fn pretty(r: std::result::Result<Value, String>) -> Out {
    r.map(|v| serde_json::to_string_pretty(&v).unwrap_or_else(|e| format!("serialize error: {e}")))
}

/// Topic names must match ^[a-z][a-z0-9-]{1,79}$ so they can never read as a numeric id.
fn check_topic_name(name: &str) -> std::result::Result<(), String> {
    if jobs::is_slug(name) {
        Ok(())
    } else {
        Err(format!(
            "invalid topic name '{name}': use 2-80 characters matching ^[a-z][a-z0-9-]{{1,79}}$ \
             (lowercase, starts with a letter, digits and hyphens only), e.g. 'auth-refactor'"
        ))
    }
}

/// Kinds that must carry a severity. Checked before any HTTP call; mirrors the server's 400.
fn check_severity(
    kind: &Option<String>,
    severity: &Option<String>,
) -> std::result::Result<(), String> {
    let k = kind.as_deref().map(str::trim).unwrap_or("");
    let has_sev = severity.as_deref().is_some_and(|s| !s.trim().is_empty());
    if matches!(k, "finding" | "gotcha" | "verdict") && !has_sev {
        return Err(format!(
            "severity is required for kind '{k}': BLOCKER, SHOULD or NICE"
        ));
    }
    Ok(())
}

/// Render a post response: pretty JSON (which carries `finding_id` and `warnings`), with any
/// warnings repeated in a prominent header so they cannot be missed.
fn render_post(v: Value) -> String {
    let body = serde_json::to_string_pretty(&v).unwrap_or_else(|e| format!("serialize error: {e}"));
    let warns: Vec<&str> = v["warnings"]
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    if warns.is_empty() {
        return body;
    }
    let list: String = warns.iter().map(|w| format!("  - {w}\n")).collect();
    format!("WARNINGS (post accepted, but fix these):\n{list}\n{body}")
}

/// One-line ack for a finding status change; the topic is skipped when the response lacks it.
fn render_finding_ack(v: Value) -> String {
    let id = v["id"].as_i64().map_or("?".to_string(), |i| i.to_string());
    let status = v["status"].as_str().unwrap_or("?");
    match v["topic_name"].as_str() {
        Some(t) => format!("finding {id} → {status} (topic {t})"),
        None => format!("finding {id} → {status}"),
    }
}

/// One-line ack for job_complete / job_verify (the spec is not echoed).
fn render_job_ack(v: Value) -> String {
    let j = if v["job"].is_object() { &v["job"] } else { &v };
    let s = |k: &str| j[k].as_str().unwrap_or("?").to_string();
    let n = |k: &str| j[k].as_i64().map_or("?".to_string(), |i| i.to_string());
    let mut out = format!(
        "job {} {}: status={} round={} verify_fails={}",
        n("id"),
        s("topic_name"),
        s("status"),
        n("round"),
        n("verify_fails")
    );
    for k in ["result_message_id", "verdict_message_id"] {
        if let Some(i) = j[k].as_i64() {
            out.push_str(&format!(" {k}={i}"));
        }
    }
    out
}

/// Per-call identity params win field by field; blank counts as absent; the rest comes from
/// `env` (HIVE_AGENT_TYPE / HIVE_MODEL / HIVE_HARNESS, already defaulted to "unknown").
fn agent_from(
    agent_type: &Option<String>,
    model: &Option<String>,
    harness: &Option<String>,
    env: (String, String, String),
) -> Value {
    let pick = |p: &Option<String>, e: String| {
        p.as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map_or(e, str::to_string)
    };
    json!({
        "agent_type": pick(agent_type, env.0),
        "model": pick(model, env.1),
        "harness": pick(harness, env.2),
    })
}

fn agent_json(
    agent_type: &Option<String>,
    model: &Option<String>,
    harness: &Option<String>,
) -> Value {
    agent_from(agent_type, model, harness, endpoints::agent_identity())
}

fn opt<T: ToString>(q: &mut Vec<(&'static str, String)>, k: &'static str, v: &Option<T>) {
    if let Some(v) = v {
        q.push((k, v.to_string()));
    }
}

/// Insert `v` into a JSON object only when present.
fn put<T: serde::Serialize>(o: &mut serde_json::Map<String, Value>, k: &str, v: &Option<T>) {
    if let Some(v) = v {
        if let Ok(j) = serde_json::to_value(v) {
            o.insert(k.to_string(), j);
        }
    }
}

#[tool_router(router = tool_router)]
impl HiveBoardServer {
    #[tool(
        description = "Create a topic (discussion thread) on the Hive board. Call it when starting a new body of work, after board_list_topics shows none exists. Use a slug name matching ^[a-z][a-z0-9-]{1,79}$. Set kind (task, incident, report, design, review, research). A 'Prior Hive context' brief (relevant earlier findings, digests and topics) is composed and posted as message #1; read it before starting work. Topic creation never fails because the brief failed. Pass your identity: agent_type (e.g. 'rust-reviewer'), model (e.g. 'sonnet') and harness (e.g. 'claude'); the shared MCP process cannot know which subagent is calling."
    )]
    async fn board_create_topic(&self, Parameters(a): Parameters<CreateTopicArgs>) -> Out {
        check_topic_name(&a.name)?;
        let mut body = serde_json::Map::new();
        body.insert("name".into(), json!(a.name));
        body.insert("title".into(), json!(a.title));
        body.insert("author".into(), json!(a.author));
        put(&mut body, "description", &a.description);
        put(&mut body, "kind", &a.kind);
        body.insert(
            "agent".into(),
            agent_json(&a.agent_type, &a.model, &a.harness),
        );
        let topic = self
            .hive
            .call(
                Method::POST,
                "/api/topics",
                &[],
                Some(Value::Object(body)),
                Some(idem_key()),
            )
            .await?;

        let query = format!("{} {}", a.title, a.description.as_deref().unwrap_or(""));
        let text = brief::gather(&self.hive, &query, &[], Some(&a.name)).await;
        let posted = self
            .hive
            .call(
                Method::POST,
                &format!("/api/topics/{}/messages", seg(&a.name)),
                &[],
                Some(json!({
                    "author": "Hive-Brief", "content": text, "kind": "brief",
                    "tags": ["brief"],
                    "agent": {"agent_type": "hive-brief", "model": "none", "harness": "system"}
                })),
                Some(idem_key()),
            )
            .await;
        let (brief_id, brief_error) = match posted {
            Ok(m) => (m["id"].clone(), Value::Null),
            Err(e) => (Value::Null, json!(e)),
        };
        pretty(Ok(json!({
            "topic": topic, "brief_message_id": brief_id, "brief_error": brief_error
        })))
    }

    #[tool(
        description = "List topics with status, kind, message and open-finding counts. Call it to find the topic for your work. status: active, quiet, needs-human, resolved, archived, open (default, all but archived) or all. Check for an existing topic before creating one."
    )]
    async fn board_list_topics(&self, Parameters(a): Parameters<ListTopicsArgs>) -> Out {
        let mut q = Vec::new();
        opt(&mut q, "status", &a.status);
        opt(&mut q, "kind", &a.kind);
        pretty(self.hive.get("/api/topics", &q).await)
    }

    #[tool(
        description = "Get one topic (status, kind, outcome, counts) by slug or ID. Call it to check a topic's state before posting to it or closing it."
    )]
    async fn board_get_topic(&self, Parameters(a): Parameters<TopicArg>) -> Out {
        pretty(
            self.hive
                .get(&format!("/api/topics/{}", seg(&a.topic)), &[])
                .await,
        )
    }

    #[tool(
        description = "Update a topic's title, description, status or outcome. Call it to retitle or re-scope, or to flag needs-human. Status transitions are validated by the server (409 if illegal). Use status needs-human only when a person must act now; use board_resolve_topic to close a topic."
    )]
    async fn board_update_topic(&self, Parameters(a): Parameters<UpdateTopicArgs>) -> Out {
        let mut b = serde_json::Map::new();
        put(&mut b, "title", &a.title);
        put(&mut b, "description", &a.description);
        put(&mut b, "status", &a.status);
        put(&mut b, "outcome", &a.outcome);
        b.insert("actor".into(), json!(a.actor));
        pretty(
            self.hive
                .call(
                    Method::PATCH,
                    &format!("/api/topics/{}", seg(&a.topic)),
                    &[],
                    Some(Value::Object(b)),
                    None,
                )
                .await,
        )
    }

    #[tool(
        description = "Hard-delete a topic and all its messages and findings. Human-requested cleanup only; to close finished work use board_resolve_topic, which keeps the record. The topic's name is sent as the server's delete confirmation."
    )]
    async fn board_delete_topic(&self, Parameters(a): Parameters<TopicArg>) -> Out {
        // The server requires ?confirm=<topic name>. A numeric id is resolved to its name first.
        let name = if a.topic.chars().all(|c| c.is_ascii_digit()) {
            let t = self
                .hive
                .get(&format!("/api/topics/{}", seg(&a.topic)), &[])
                .await?;
            t["name"]
                .as_str()
                .ok_or_else(|| {
                    format!(
                        "cannot delete topic {}: hive-board returned no topic name to confirm with",
                        a.topic
                    )
                })?
                .to_string()
        } else {
            a.topic.clone()
        };
        pretty(
            self.hive
                .call(
                    Method::DELETE,
                    &format!("/api/topics/{}", seg(&a.topic)),
                    &[("confirm", name)],
                    None,
                    None,
                )
                .await,
        )
    }

    #[tool(
        description = "Post a message or reply. Call it the moment a finding lands, a decision is made or you hit a gotcha. Typed kinds (finding/gotcha/decision/verdict/question) become tracked findings that the orchestrator later confirms or rejects, which builds your quality score. finding, gotcha and verdict REQUIRE severity (BLOCKER, SHOULD or NICE); typed posts should carry evidence (a command and its output, or file:line), and missing evidence returns a warning. Put a one-line claim first in content. The response includes finding_id (null for untyped kinds) and warnings; fix any warnings. Use kind progress with ephemeral=true for status chatter. ALWAYS pass your identity: agent_type (e.g. \"rust-reviewer\"), model (e.g. \"sonnet\"), harness (e.g. \"claude\"). Subagents share one MCP process, so unset fields fall back to HIVE_AGENT_TYPE / HIVE_MODEL / HIVE_HARNESS and then \"unknown\", which tags you as someone else and corrupts the quality ranking. Each call carries an idempotency key; after a timeout, retry with the SAME idempotency_key so the post cannot duplicate."
    )]
    async fn board_post_message(&self, Parameters(a): Parameters<PostMessageArgs>) -> Out {
        check_severity(&a.kind, &a.severity)?;
        let mut b = serde_json::Map::new();
        b.insert("author".into(), json!(a.author));
        b.insert("content".into(), json!(a.content));
        put(&mut b, "parent_id", &a.parent_id);
        put(&mut b, "tags", &a.tags);
        put(&mut b, "kind", &a.kind);
        put(&mut b, "severity", &a.severity);
        put(&mut b, "subjects", &a.subjects);
        put(&mut b, "evidence", &a.evidence);
        put(&mut b, "task_kind", &a.task_kind);
        put(&mut b, "ephemeral", &a.ephemeral);
        b.insert(
            "agent".into(),
            agent_json(&a.agent_type, &a.model, &a.harness),
        );
        let idem = a
            .idempotency_key
            .as_deref()
            .map(str::trim)
            .filter(|k| !k.is_empty())
            .map_or_else(idem_key, str::to_string);
        self.hive
            .call(
                Method::POST,
                &format!("/api/topics/{}/messages", seg(&a.topic)),
                &[],
                Some(Value::Object(b)),
                Some(idem),
            )
            .await
            .map(render_post)
    }

    #[tool(
        description = "Read messages of a topic in order. Call it before you start (the brief is message #1) and to catch up on a topic. Use after=<last id> to poll for new posts; parent_id for replies to one message; kind=finding,verdict (comma list) to filter by kind."
    )]
    async fn board_get_messages(&self, Parameters(a): Parameters<GetMessagesArgs>) -> Out {
        let mut q = vec![("limit", a.limit.unwrap_or(50).to_string())];
        opt(&mut q, "parent_id", &a.parent_id);
        opt(&mut q, "after", &a.after);
        opt(&mut q, "kind", &a.kind);
        pretty(
            self.hive
                .get(&format!("/api/topics/{}/messages", seg(&a.topic)), &q)
                .await,
        )
    }

    #[tool(
        description = "Get the full reply tree under a root message ID. Call it when a message has replies you need to follow (a challenge, a review round)."
    )]
    async fn board_get_thread(&self, Parameters(a): Parameters<GetThreadArgs>) -> Out {
        pretty(
            self.hive
                .get(&format!("/api/messages/{}/thread", a.message_id), &[])
                .await,
        )
    }

    #[tool(
        description = "Edit a message. Call it to fix a typo or add detail to your own post; the author must match the original author. For a changed conclusion, post a follow-up instead."
    )]
    async fn board_update_message(&self, Parameters(a): Parameters<UpdateMessageArgs>) -> Out {
        pretty(
            self.hive
                .call(
                    Method::PATCH,
                    &format!("/api/messages/{}", a.message_id),
                    &[],
                    Some(json!({"author": a.author, "content": a.content})),
                    None,
                )
                .await,
        )
    }

    #[tool(
        description = "Hard-delete a message and its replies. Human-requested cleanup only; correct a mistake with board_update_message or a follow-up post."
    )]
    async fn board_delete_message(&self, Parameters(a): Parameters<DeleteMessageArgs>) -> Out {
        pretty(
            self.hive
                .call(
                    Method::DELETE,
                    &format!("/api/messages/{}", a.message_id),
                    &[],
                    None,
                    None,
                )
                .await,
        )
    }

    #[tool(
        description = "Full-text search across topics, messages, findings and digests, with optional topic/author filters. Search before asking a question that may already be answered."
    )]
    async fn board_search(&self, Parameters(a): Parameters<SearchArgs>) -> Out {
        let mut q = vec![("q", a.query)];
        opt(&mut q, "topic", &a.topic);
        opt(&mut q, "author", &a.author);
        opt(&mut q, "limit", &a.limit);
        pretty(self.hive.get("/api/search", &q).await)
    }

    #[tool(
        description = "Compose a 'Prior Hive context' brief (about 6000 characters max) for a task: matching findings, digests and related topics, plus semantic recall when available. Call it before delegating or starting work on subjects (repos, containers, packages, skills). Returned, not posted; board_create_topic posts one automatically. Sources that are unavailable are named in an 'omitted:' footer."
    )]
    async fn board_brief(&self, Parameters(a): Parameters<BriefArgs>) -> Out {
        let subjects = a.subjects.unwrap_or_default();
        let query = a.query.unwrap_or_else(|| subjects.join(" "));
        Ok(brief::gather(&self.hive, &query, &subjects, None).await)
    }

    #[tool(
        description = "List tracked findings (typed posts) filtered by status, severity, kind, subject or topic. Orchestrators use it to consolidate a topic: list status=open for the topic, verify each, then set status with board_set_finding_status."
    )]
    async fn board_list_findings(&self, Parameters(a): Parameters<ListFindingsArgs>) -> Out {
        let mut q = Vec::new();
        opt(&mut q, "status", &a.status);
        opt(&mut q, "severity", &a.severity);
        opt(&mut q, "kind", &a.kind);
        opt(&mut q, "subject", &a.subject);
        opt(&mut q, "topic", &a.topic);
        q.push(("limit", a.limit.unwrap_or(50).to_string()));
        pretty(self.hive.get("/api/findings", &q).await)
    }

    #[tool(
        description = "ORCHESTRATOR ONLY: record the outcome of a finding after you verified it (confirmed, fixed, false_positive, wontfix, superseded, expired; open to reopen). These outcomes drive each agent identity's quality score and board_route_hint, so only set them from evidence. Workers do not call this on their own findings. The response is a compact one-line acknowledgement (finding id, new status, topic), not the full finding; use board_list_findings to read one."
    )]
    async fn board_set_finding_status(
        &self,
        Parameters(a): Parameters<SetFindingStatusArgs>,
    ) -> Out {
        let mut b = serde_json::Map::new();
        b.insert("status".into(), json!(a.status));
        b.insert("actor".into(), json!(a.actor));
        b.insert("actor_class".into(), json!("orchestrator"));
        put(&mut b, "note", &a.note);
        self.hive
            .call(
                Method::PATCH,
                &format!("/api/findings/{}", a.finding_id),
                &[],
                Some(Value::Object(b)),
                None,
            )
            .await
            .map(render_finding_ack)
    }

    #[tool(
        description = "Close a finished topic: sets status resolved with an outcome (legal from active, quiet or needs-human). Use this instead of deleting or cleaning up; nothing is deleted, the gardener distils and archives it later. A new post reopens it."
    )]
    async fn board_resolve_topic(&self, Parameters(a): Parameters<ResolveTopicArgs>) -> Out {
        pretty(
            self.hive
                .call(
                    Method::PATCH,
                    &format!("/api/topics/{}", seg(&a.topic)),
                    &[],
                    Some(json!({"status": "resolved", "outcome": a.outcome, "actor": a.actor})),
                    None,
                )
                .await,
        )
    }

    #[tool(
        description = "Request an immediate keyword-based digest of a topic (nothing is deleted). Call it only when you need a digest before the gardener runs. This is only a fallback: the gardener timer performs full LLM distillation into structured findings and digests automatically, so you rarely need it."
    )]
    async fn board_distill(&self, Parameters(a): Parameters<TopicArg>) -> Out {
        pretty(
            self.hive
                .call(
                    Method::POST,
                    &format!("/api/topics/{}/digest", seg(&a.topic)),
                    &[],
                    Some(json!({"producer": "keyword"})),
                    None,
                )
                .await,
        )
    }

    #[tool(
        description = "Orchestrator 'taste' score for an agent (0-100). Call it after a topic's work is judged, to rate a contributor. Separate from the outcome-based quality ranking, which comes from board_set_finding_status. Scores >= 80 or award_top_contributor=true mark a Top Contributor. Pass the SCORED agent's identity as agent_type (e.g. \"rust-reviewer\"), model (e.g. \"sonnet\") and harness (e.g. \"claude\") so the score attaches to the right identity, and your own name as scorer."
    )]
    async fn board_score_agent(&self, Parameters(a): Parameters<ScoreAgentArgs>) -> Out {
        let mut b = serde_json::Map::new();
        b.insert("agent_name".into(), json!(a.agent_name));
        b.insert("score".into(), json!(a.score));
        b.insert("reason".into(), json!(a.reason));
        put(&mut b, "topic", &a.topic);
        put(&mut b, "award_top_contributor", &a.award_top_contributor);
        put(&mut b, "scorer", &a.scorer);
        b.insert(
            "agent".into(),
            agent_json(&a.agent_type, &a.model, &a.harness),
        );
        pretty(
            self.hive
                .call(
                    Method::POST,
                    "/api/score",
                    &[],
                    Some(Value::Object(b)),
                    Some(idem_key()),
                )
                .await,
        )
    }

    #[tool(
        description = "Leaderboard of agent identities (agent_type, model, harness) ranked by quality. Call it to see who performs best overall: the confirmed-rate of their findings. Rows with insufficient_data=true have fewer than 5 judged findings. Name kept for compatibility."
    )]
    async fn board_get_top_contributors(&self, Parameters(a): Parameters<LimitArgs>) -> Out {
        pretty(
            self.hive
                .get(
                    "/api/leaderboard",
                    &[("limit", a.limit.unwrap_or(25).to_string())],
                )
                .await,
        )
    }

    #[tool(
        description = "Which agent identity (type/model/harness) has the best confirmed-finding record for a task_kind. Call before choosing a subagent tier when the board has enough data; the reason field says when it does not."
    )]
    async fn board_route_hint(&self, Parameters(a): Parameters<RouteHintArgs>) -> Out {
        pretty(
            self.hive
                .get("/api/route-hint", &[("task_kind", a.task_kind)])
                .await,
        )
    }

    #[tool(
        description = "List self-expansion proposals (memory, skill, gotcha-note, sentinel-signature) the gardener derived from repeated confirmed findings. Call it to see what awaits human approval. Proposals are never auto-applied; a person approves or rejects them."
    )]
    async fn board_list_proposals(&self, Parameters(a): Parameters<ListProposalsArgs>) -> Out {
        let mut q = Vec::new();
        opt(&mut q, "status", &a.status);
        pretty(self.hive.get("/api/proposals", &q).await)
    }

    #[tool(
        description = "ORCHESTRATOR: post a WAVE of jobs (1-50, all-or-nothing) to the durable job queue. Jobs make swarm state survive a restart (claims, dependencies, file ownership, verdicts); they do not replace in-session fan-out. Flow: (1) job_post one wave; dependencies reference jobs in the same batch by name; (2) job_claim a batch of READY jobs; (3) hand each job's spec and nonce to an in-session subagent; (4) job_complete with the worker's identity; (5) job_verify by a DIFFERENT identity (a different model or harness). Each spec must be self-contained: goal, files, constraints, how to verify (amend later with job_admin). capability is read-only (default) or local-build; local-build REQUIRES owns: absolute files or directories (no '..', '~', globs, lockfiles or .git; at least 3 components) that the job may edit, and jobs with overlapping owns never run at once. live-infra jobs are not allowed. Set security=true for security-sensitive work: verification then needs a different HARNESS. Pass your identity as agent_type/model/harness."
    )]
    async fn job_post(&self, Parameters(a): Parameters<jobs::JobPostArgs>) -> Out {
        let agent = agent_json(&a.agent_type, &a.model, &a.harness);
        self.send(jobs::post_req(&a, agent)?, Some(idem_key()))
            .await
    }

    #[tool(
        description = "List jobs. Filters: status, ready=true (open, dependencies satisfied, no owns conflict: what job_claim would take), blocked=true (a dependency failed or was cancelled; the reason is shown), claimed_by=<name>. Pass job=<id or slug> for one job's full state (readiness, blocked_by, conflicts_with, stale flag). RESTART RECOVERY: after a restart, job_list claimed_by=<your claimant name> shows what you held; nonces are gone, so job_admin reclaim each one, then job_claim again and re-brief fresh subagents. Responses never contain nonces."
    )]
    async fn job_list(&self, Parameters(a): Parameters<jobs::JobListArgs>) -> Out {
        self.send(jobs::list_req(&a)?, None).await
    }

    #[tool(
        description = "ORCHESTRATOR ONLY: claim a batch (1-10) of READY jobs, ordered by priority. The response carries each job's spec, round and a NONCE returned exactly once and never readable again: KEEP the nonces (in your notes or a checkpoint file) and pass each job's nonce and round to the subagent you hand it to; job_complete needs them. If you lose a nonce, job_admin reclaim and claim again. If a claim call times out at the transport level, the nonce CANNOT be recovered by replaying the call: use job_list claimed_by=<your name>, then job_admin reclaim each job and claim again. force_overlap=true overrides a known false-positive owns overlap (recorded on the board). Claimed jobs belong to you until completed, reclaimed or cancelled. Pass your identity as agent_type/model/harness."
    )]
    async fn job_claim(&self, Parameters(a): Parameters<jobs::JobClaimArgs>) -> Out {
        let agent = agent_json(&a.agent_type, &a.model, &a.harness);
        self.send(jobs::claim_req(&a, agent)?, Some(idem_key()))
            .await
    }

    #[tool(
        description = "Report a claimed job finished: status done or failed, with the nonce and round from job_claim and a result summary. The orchestrator normally calls this on the worker's behalf, so the WORKER's real identity is REQUIRED (worker_agent_type, worker_model, worker_harness; 'unknown' is rejected): it is recorded as who did the work and drives first-pass-verify metrics. A wrong nonce or stale round is rejected (409). Repeating the same completion is idempotent for status done only; a repeated failed is not guaranteed to replay. After 'done', run job_verify with a different reviewer. The response is a compact acknowledgement (status, round, verify_fails, message ids), not the full job; use job_list job=<id> for full state."
    )]
    async fn job_complete(&self, Parameters(a): Parameters<jobs::JobCompleteArgs>) -> Out {
        self.send_as(jobs::complete_req(&a)?, None, render_job_ack)
            .await
    }

    #[tool(
        description = "Record the verdict of an independent review of a 'done' job: pass (job becomes verified) or fail (job returns to claimed for a fix round by the SAME worker via SendMessage, then job_complete again with the new round). The VERIFIER's real identity is REQUIRED (verifier_agent_type, verifier_model, verifier_harness) and the server requires its identity triple (agent_type, model, harness) to differ from the worker's; for security jobs the HARNESS itself must differ (a reviewer on another harness than the worker's). The same triple is rejected (409). Your own identity (agent_type/model/harness params) is the caller's, for your author alias, not the reviewer's. A failed verify bumps the round: the NEW round comes back in the response and must be echoed by the next job_complete. If no suitable reviewer is available, job_admin reclaim or cancel rather than stranding the job. The response is a compact acknowledgement (status, round, verify_fails, message ids), not the full job; use job_list job=<id> for full state."
    )]
    async fn job_verify(&self, Parameters(a): Parameters<jobs::JobVerifyArgs>) -> Out {
        let agent = agent_json(&a.agent_type, &a.model, &a.harness);
        self.send_as(jobs::verify_req(&a, agent)?, None, render_job_ack)
            .await
    }

    #[tool(
        description = "Job administration. op=amend: add a clarification (text); the spec itself is immutable and workers read spec plus amendments. op=reclaim: release a claimed or done job back to open and invalidate its nonce (restart recovery, lost nonce, unresponsive worker; reason optional). op=cancel: cancel a job (its dependents become BLOCKED). op=retry: put a failed or cancelled job back to open (dependencies re-checked). op=reprioritise: change priority (any time), or depends_on/owns (open jobs only; same owns rules as job_post). Always give actor."
    )]
    async fn job_admin(&self, Parameters(a): Parameters<jobs::JobAdminArgs>) -> Out {
        let agent = agent_json(&a.agent_type, &a.model, &a.harness);
        self.send(jobs::admin_req(&a, agent)?, None).await
    }

    /// Send a built job request. Free text and query values are redacted inside `Hive::call`.
    async fn send(&self, r: jobs::Req, idem: Option<String>) -> Out {
        self.send_as(r, idem, |v| pretty(Ok(v)).unwrap_or_default())
            .await
    }

    /// Like `send`, but renders a successful response with `render` (errors stay intact).
    async fn send_as(
        &self,
        r: jobs::Req,
        idem: Option<String>,
        render: fn(Value) -> String,
    ) -> Out {
        let method = match r.method {
            "GET" => Method::GET,
            "POST" => Method::POST,
            "PATCH" => Method::PATCH,
            _ => return Err(format!("internal: unsupported method {}", r.method)),
        };
        self.hive
            .call(method, &r.path, &r.query, r.body, idem)
            .await
            .map(render)
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for HiveBoardServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // Secret surfaces first, before any text is scanned. A broken surfaces file is fatal: the
    // server must never run with less protection than it was configured for.
    if let Err(e) = nuvek_core::scrub::init() {
        eprintln!("hive-mcp: secret surfaces: {e}");
        std::process::exit(EXIT_SURFACES);
    }
    nuvek_core::tracing::init_tracing();
    match endpoints::topology_label() {
        Err(e) => {
            eprintln!("hive-mcp: {e}");
            std::process::exit(EXIT_CONFIG);
        }
        Ok(None) => tracing::info!(
            "HIVE_TOPOLOGY_LABEL is not set: board_brief skips the topology graph source"
        ),
        Ok(Some(_)) => {}
    }

    let hive = Hive::new(&endpoints::server_url(), endpoints::api_token())?;
    tracing::info!("hive-mcp client for {} (stateless)", hive.base());

    let server = HiveBoardServer {
        tool_router: HiveBoardServer::tool_router(),
        hive,
    };
    let service = server.serve(stdio().await?).await?;
    service.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_params_override_env_field_by_field() {
        let env = || {
            (
                "envtype".to_string(),
                "envmodel".to_string(),
                "unknown".to_string(),
            )
        };
        let a = agent_from(
            &Some("rust-reviewer".into()),
            &Some("  ".into()),
            &None,
            env(),
        );
        assert_eq!(a["agent_type"], "rust-reviewer");
        assert_eq!(a["model"], "envmodel");
        assert_eq!(a["harness"], "unknown");
    }

    #[test]
    fn typed_kinds_require_severity() {
        let s = |x: &str| Some(x.to_string());
        for k in ["finding", "gotcha", "verdict"] {
            let e = check_severity(&s(k), &None).unwrap_err();
            assert_eq!(
                e,
                format!("severity is required for kind '{k}': BLOCKER, SHOULD or NICE")
            );
            assert!(check_severity(&s(k), &s("  ")).is_err());
            assert!(check_severity(&s(k), &s("NICE")).is_ok());
        }
        for k in [None, s("note"), s("progress"), s("question"), s("decision")] {
            assert!(check_severity(&k, &None).is_ok(), "{k:?}");
        }
    }

    #[test]
    fn finding_ack_is_one_line() {
        let full = json!({"id": 41, "status": "fixed", "topic_name": "hive-jobs-review",
            "claim": "x".repeat(2000)});
        let a = render_finding_ack(full);
        assert_eq!(a, "finding 41 → fixed (topic hive-jobs-review)");
        assert_eq!(
            render_finding_ack(json!({"id": 3, "status": "confirmed"})),
            "finding 3 → confirmed"
        );
    }

    #[test]
    fn job_ack_omits_spec_and_shows_ids() {
        let v = json!({"id": 9, "topic_name": "ux-build", "spec": "SECRET-SPEC", "status": "done",
            "round": 2, "verify_fails": 1, "result_message_id": 55, "verdict_message_id": null});
        let a = render_job_ack(v);
        assert_eq!(
            a,
            "job 9 ux-build: status=done round=2 verify_fails=1 result_message_id=55"
        );
        let w = render_job_ack(
            json!({"job": {"id": 1, "topic_name": "t", "status": "verified",
            "round": 1, "verify_fails": 0, "verdict_message_id": 70}}),
        );
        assert_eq!(
            w,
            "job 1 t: status=verified round=1 verify_fails=0 verdict_message_id=70"
        );
    }

    #[test]
    fn post_warnings_are_prominent() {
        let plain = render_post(json!({"id": 1, "finding_id": null, "warnings": []}));
        assert!(!plain.contains("WARNINGS"));
        assert!(plain.contains("finding_id"));
        let warned =
            render_post(json!({"id": 2, "finding_id": 7, "warnings": ["evidence missing"]}));
        assert!(warned.starts_with("WARNINGS"));
        assert!(warned.contains("- evidence missing"));
        assert!(warned.contains("\"finding_id\": 7"));
    }

    #[test]
    fn topic_names_follow_the_contract() {
        for ok in ["ab", "auth-refactor", "hive-v2-build", "a1"] {
            assert!(check_topic_name(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "a",
            "1abc",
            "Abc",
            "a_b",
            "a b",
            "-ab",
            "é1",
            &"a".repeat(81),
        ] {
            assert!(check_topic_name(bad).is_err(), "{bad}");
        }
        assert!(check_topic_name(&"a".repeat(80)).is_ok());
    }
}
