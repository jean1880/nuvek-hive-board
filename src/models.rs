//! Row and response types. Timestamps serialise as RFC 3339.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;

pub type Ts = DateTime<Utc>;

/// Column list for topics (alias `t`), including the computed message count.
pub const TOPIC_COLS: &str = "t.id, t.name, t.title, t.description, t.created_by, t.kind, \
     t.status, t.outcome, t.last_activity_at, t.distilled_through, t.distilled_at, \
     t.created_at, t.updated_at, \
     (SELECT count(*) FROM messages mc WHERE mc.topic_id = t.id) AS message_count, \
     (SELECT count(*) FROM findings fc WHERE fc.topic_id = t.id AND fc.status = 'open' \
     AND fc.about_identity_id IS NULL) AS open_findings";

#[derive(Debug, Clone, Serialize, FromRow)]
pub struct Topic {
    pub id: i64,
    pub name: String,
    pub title: String,
    pub description: Option<String>,
    pub created_by: String,
    pub kind: String,
    pub status: String,
    pub outcome: Option<String>,
    pub last_activity_at: Ts,
    pub distilled_through: i64,
    pub distilled_at: Option<Ts>,
    pub created_at: Ts,
    pub updated_at: Ts,
    pub message_count: i64,
    pub open_findings: i64,
}

/// Column list for messages: aliases `m` (messages), `t` (topics), `i` (identities, LEFT JOIN).
pub const MSG_COLS: &str = "m.id, m.topic_id, t.name AS topic_name, m.parent_id, m.author, \
     m.identity_id, m.content, m.tags, m.kind, m.severity, m.subjects, m.evidence, \
     m.task_kind, m.ephemeral, m.compacted_at, m.created_at, m.updated_at, \
     i.agent_type AS id_agent_type, i.model AS id_model, i.harness AS id_harness";

/// FROM clause that matches `MSG_COLS`.
pub const MSG_FROM: &str = "messages m JOIN topics t ON t.id = m.topic_id \
     LEFT JOIN identities i ON i.id = m.identity_id";

#[derive(Debug, Clone, Serialize)]
pub struct IdentityRef {
    pub agent_type: String,
    pub model: String,
    pub harness: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Message {
    pub id: i64,
    pub topic_id: i64,
    pub topic_name: String,
    pub parent_id: Option<i64>,
    pub author: String,
    pub identity_id: Option<i64>,
    pub identity: Option<IdentityRef>,
    pub content: String,
    pub tags: Vec<String>,
    pub kind: String,
    pub severity: Option<String>,
    pub subjects: Vec<String>,
    pub evidence: Option<String>,
    pub task_kind: Option<String>,
    pub ephemeral: bool,
    pub compacted_at: Option<Ts>,
    pub created_at: Ts,
    pub updated_at: Ts,
}

impl<'r> FromRow<'r, sqlx::postgres::PgRow> for Message {
    fn from_row(row: &'r sqlx::postgres::PgRow) -> Result<Self, sqlx::Error> {
        use sqlx::Row;
        let agent_type: Option<String> = row.try_get("id_agent_type")?;
        let model: Option<String> = row.try_get("id_model")?;
        let harness: Option<String> = row.try_get("id_harness")?;
        let identity = match (agent_type, model, harness) {
            (Some(agent_type), Some(model), Some(harness)) => Some(IdentityRef {
                agent_type,
                model,
                harness,
            }),
            _ => None,
        };
        Ok(Self {
            id: row.try_get("id")?,
            topic_id: row.try_get("topic_id")?,
            topic_name: row.try_get("topic_name")?,
            parent_id: row.try_get("parent_id")?,
            author: row.try_get("author")?,
            identity_id: row.try_get("identity_id")?,
            identity,
            content: row.try_get("content")?,
            tags: row.try_get("tags")?,
            kind: row.try_get("kind")?,
            severity: row.try_get("severity")?,
            subjects: row.try_get("subjects")?,
            evidence: row.try_get("evidence")?,
            task_kind: row.try_get("task_kind")?,
            ephemeral: row.try_get("ephemeral")?,
            compacted_at: row.try_get("compacted_at")?,
            created_at: row.try_get("created_at")?,
            updated_at: row.try_get("updated_at")?,
        })
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ThreadNode {
    pub message: Message,
    pub replies: Vec<ThreadNode>,
}

#[derive(Debug, Clone, Serialize, FromRow)]
pub struct TopicEvent {
    pub id: i64,
    pub topic_id: i64,
    pub event: String,
    pub actor: String,
    pub detail: serde_json::Value,
    pub at: Ts,
}

/// Column list for findings (alias `f`, joined to topics `t`).
pub const FINDING_COLS: &str = "f.id, f.topic_id, t.name AS topic_name, f.message_id, \
     f.identity_id, f.kind, f.severity, f.claim, f.evidence, f.subjects, f.resolved_subjects, \
     f.unresolved_subjects, f.task_kind, f.confidence, f.provenance, f.content_hash, \
     f.reinforce_count, f.duplicate_of, f.status, f.status_changed_at, f.status_changed_by, \
     f.graph_synced_at, f.created_at, f.updated_at, f.about_identity_id";

#[derive(Debug, Clone, Serialize, FromRow)]
pub struct Finding {
    pub id: i64,
    pub topic_id: i64,
    pub topic_name: String,
    pub message_id: Option<i64>,
    pub identity_id: Option<i64>,
    pub kind: String,
    pub severity: Option<String>,
    pub claim: String,
    pub evidence: Option<String>,
    pub subjects: Vec<String>,
    pub resolved_subjects: serde_json::Value,
    pub unresolved_subjects: Vec<String>,
    pub task_kind: Option<String>,
    pub confidence: Option<f32>,
    pub provenance: String,
    pub content_hash: String,
    pub reinforce_count: i32,
    pub duplicate_of: Option<i64>,
    pub status: String,
    pub status_changed_at: Option<Ts>,
    pub status_changed_by: Option<String>,
    pub graph_synced_at: Option<Ts>,
    /// Whose work a job verdict judges (set on job verdict findings only).
    pub about_identity_id: Option<i64>,
    pub created_at: Ts,
    pub updated_at: Ts,
}

#[derive(Debug, Clone, Serialize, FromRow)]
pub struct FindingEvent {
    pub id: i64,
    pub finding_id: i64,
    pub from_status: Option<String>,
    pub to_status: String,
    pub actor: String,
    pub actor_class: String,
    pub note: Option<String>,
    pub at: Ts,
}

#[derive(Debug, Clone, Serialize)]
pub struct FindingDetail {
    #[serde(flatten)]
    pub finding: Finding,
    pub events: Vec<FindingEvent>,
}

#[derive(Debug, Clone, Serialize, FromRow)]
pub struct Digest {
    pub id: i64,
    pub topic_id: i64,
    pub topic_name: String,
    pub version: i32,
    pub summary: String,
    pub transcript_md: String,
    pub participants: Vec<String>,
    pub finding_ids: Vec<i64>,
    pub through_message_id: i64,
    pub producer: String,
    pub created_at: Ts,
}

#[derive(Debug, Clone, Serialize, FromRow)]
pub struct DigestSummary {
    pub id: i64,
    pub topic_id: i64,
    pub topic_name: String,
    pub version: i32,
    pub summary: String,
    pub participants: Vec<String>,
    pub finding_ids: Vec<i64>,
    pub through_message_id: i64,
    pub producer: String,
    pub created_at: Ts,
}

#[derive(Debug, Clone, Serialize, FromRow)]
pub struct IdentityQuality {
    pub identity_id: i64,
    pub agent_type: String,
    pub model: String,
    pub harness: String,
    pub aliases: Vec<String>,
    pub quality: f64,
    pub n_judged: i64,
    pub n_findings: i64,
    pub taste_mean: Option<f64>,
    pub insufficient_data: bool,
    pub rank: i64,
}

#[derive(Debug, Clone, Serialize, FromRow)]
pub struct Proposal {
    pub id: i64,
    pub kind: String,
    pub title: String,
    pub body: String,
    pub target: Option<String>,
    pub evidence_finding_ids: Vec<i64>,
    pub fingerprint: String,
    pub status: String,
    pub decided_by: Option<String>,
    pub decided_at: Option<Ts>,
    pub created_at: Ts,
}

#[derive(Debug, Clone, Serialize, FromRow)]
pub struct KvEntry {
    pub k: String,
    pub v: serde_json::Value,
    pub updated_at: Ts,
}

#[derive(Debug, Clone, Serialize, FromRow)]
pub struct GardenerRun {
    pub id: i64,
    pub started_at: Ts,
    pub finished_at: Option<Ts>,
    pub ok: Option<bool>,
    pub stage: Option<String>,
    pub topics_swept: Option<i32>,
    pub topics_distilled: Option<i32>,
    pub findings_written: Option<i32>,
    pub quarantined: Option<i32>,
    pub notes: Option<String>,
}

/// Agent identity as posted by clients.
#[derive(Debug, Clone, Deserialize)]
pub struct AgentIdent {
    pub agent_type: String,
    pub model: String,
    pub harness: String,
}
