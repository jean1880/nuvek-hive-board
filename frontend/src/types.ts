// Mirrors the hive-board REST contract (see the API section of the README and migrations/).

export type Tab = 'inbox' | 'boards' | 'findings' | 'leaderboard' | 'proposals'

export type TopicKind = 'task' | 'incident' | 'report' | 'design' | 'review' | 'research' | 'job'
export type TopicStatus = 'active' | 'quiet' | 'needs-human' | 'resolved' | 'archived'
export type TopicFilter = TopicStatus | 'open' | 'all'

export type MessageKind =
  | 'note'
  | 'finding'
  | 'decision'
  | 'gotcha'
  | 'question'
  | 'verdict'
  | 'brief'
  | 'system'
  | 'progress'
  | 'result'
  | 'amendment'
export type Severity = 'BLOCKER' | 'SHOULD' | 'NICE'

export interface AgentIdentityInput {
  agent_type: string
  model: string
  harness: string
}

export interface Topic {
  id: number
  name: string
  title: string
  description?: string | null
  created_by: string
  kind: TopicKind
  status: TopicStatus
  outcome?: string | null
  last_activity_at: string
  distilled_through: number
  distilled_at?: string | null
  created_at: string
  updated_at: string
  message_count: number
  open_findings: number
}

/** Topic names must match this (see the README's API section); the server answers 400 otherwise. */
export const TOPIC_NAME_RE = /^[a-z][a-z0-9-]{1,79}$/

/** Topic transition matrix: legal PATCH status moves by current status (the server answers 409 otherwise). */
export const TOPIC_TRANSITIONS: Record<TopicStatus, TopicStatus[]> = {
  active: ['needs-human', 'resolved'],
  quiet: ['active', 'needs-human', 'resolved'],
  'needs-human': ['active', 'resolved'],
  resolved: ['active', 'needs-human', 'archived'],
  archived: ['active', 'needs-human'],
}

export interface Message {
  id: number
  topic_id: number
  topic_name: string
  parent_id?: number | null
  author: string
  identity_id?: number | null
  identity: AgentIdentityInput | null
  content: string
  tags: string[]
  kind: MessageKind
  severity?: Severity | null
  subjects: string[]
  evidence?: string | null
  task_kind?: string | null
  ephemeral: boolean
  compacted_at?: string | null
  created_at: string
  updated_at: string
}

export interface ThreadNode {
  message: Message
  replies: ThreadNode[]
}

export interface TopicEvent {
  id: number
  topic_id: number
  event: string
  actor: string
  detail: Record<string, unknown>
  at: string
}

export type FindingKind = 'finding' | 'decision' | 'gotcha' | 'verdict' | 'question'
export type FindingStatus =
  | 'open'
  | 'confirmed'
  | 'fixed'
  | 'false_positive'
  | 'wontfix'
  | 'superseded'
  | 'expired'
export type ActorClass = 'orchestrator' | 'human' | 'gardener' | 'system'

export interface ResolvedSubject {
  label: string
  name: string
}

export interface FindingEvent {
  id: number
  finding_id: number
  from_status?: string | null
  to_status: string
  actor: string
  actor_class: ActorClass
  note?: string | null
  at: string
}

export interface Finding {
  id: number
  topic_id: number
  topic_name: string
  message_id?: number | null
  identity_id?: number | null
  kind: FindingKind
  severity?: Severity | null
  claim: string
  evidence?: string | null
  subjects: string[]
  resolved_subjects: ResolvedSubject[]
  unresolved_subjects: string[]
  task_kind?: string | null
  confidence?: number | null
  provenance: string
  content_hash?: string
  reinforce_count: number
  duplicate_of?: number | null
  status: FindingStatus
  status_changed_at?: string | null
  status_changed_by?: string | null
  created_at: string
  updated_at: string
  /** Present on GET /api/findings/{id} only. */
  events?: FindingEvent[]
}

export interface DigestSummary {
  id: number
  topic_id: number
  topic_name: string
  version: number
  summary: string
  participants: string[]
  finding_ids: number[]
  through_message_id: number
  producer: string
  created_at: string
}

export interface Digest extends DigestSummary {
  transcript_md: string
}

export interface IdentityQuality {
  identity_id: number
  agent_type: string
  model: string
  harness: string
  aliases: string[]
  /** 0..1 */
  quality: number
  n_judged: number
  n_findings: number
  taste_mean?: number | null
  insufficient_data: boolean
  rank: number
}

export interface AliasScore {
  alias: string
  taste_total: number
  taste_mean: number
  events: number
  top_contributor: boolean
  latest_reason?: string | null
}

export interface Stats {
  active_topics: number
  needs_human: number
  open_findings: number
  ready_jobs: number
  stale_jobs: number
  total_topics: number
  total_messages: number
  top_identity: IdentityQuality | null
  /** Added by the UX release; absent from an older server. */
  proposals_pending?: number
  open_blockers?: number
}

export type ProposalKind = 'memory' | 'skill' | 'gotcha-note' | 'sentinel-signature'
export type ProposalStatus = 'proposed' | 'approved' | 'rejected' | 'applied' | 'expired'

export interface Proposal {
  id: number
  kind: ProposalKind
  title: string
  body: string
  target?: string | null
  evidence_finding_ids: number[]
  fingerprint: string
  status: ProposalStatus
  decided_by?: string | null
  decided_at?: string | null
  created_at: string
}

export type SearchKind = 'topic' | 'message' | 'finding' | 'digest'

export interface SearchResult {
  kind: SearchKind
  topic_name: string
  id: number
  author: string
  title_or_snippet: string
  created_at: string
  score: number
}

export interface SearchResponse {
  query: string
  total_results: number
  suggestions: string[]
  results: SearchResult[]
}

export interface PostMessagePayload {
  author: string
  content: string
  parent_id?: number | null
  tags?: string[]
  kind?: MessageKind
  severity?: Severity
  subjects?: string[]
  evidence?: string
}

/** POST /api/topics/{t}/messages: the message plus the tracked finding and any soft warnings (newer servers). */
export interface PostMessageResponse extends Message {
  finding_id?: number | null
  warnings?: string[]
}

export interface ScoreResult {
  alias: string
  identity: unknown
  top_contributor: boolean
}

// ---- Jobs (migration 0002_jobs.sql) ----

export type JobCapability = 'read-only' | 'local-build'
export type JobStatus = 'open' | 'claimed' | 'done' | 'verified' | 'failed' | 'cancelled'
export type JobTier = 'fast' | 'standard' | 'deep'
export type JobVerdict = 'pass' | 'fail'

/**
 * GET /api/jobs[/{id}]: the `jobs` row (never the nonce or its hash) plus topic name/title and the
 * computed fields. A job is keyed by its topic id. Fields marked "assumed" are not guaranteed by
 * every server version and are optional.
 */
export interface Job {
  topic_id: number
  topic_name: string
  title: string
  spec: string
  priority: number
  capability: JobCapability
  security: boolean
  task_kind?: string | null
  owns: string[]
  tier?: JobTier | null
  agent_type?: string | null
  status: JobStatus
  round: number
  verify_fails: number
  attempts: number
  claimed_by?: string | null
  claimed_at?: string | null
  worker_identity_id?: number | null
  result_message_id?: number | null
  verdict_message_id?: number | null
  completed_at?: string | null
  verified_at?: string | null
  ready_at?: string | null
  created_by: string
  created_at: string
  updated_at: string
  ready: boolean
  blocked: boolean
  blocked_reason?: string | null
  blocked_by: number[]
  conflicts_with: number[]
  claim_age_s?: number | null
  stale: boolean
  /** Dependency job topic ids (from job_deps). */
  depends_on: number[]
  /** Assumed: the worker's identity resolved from worker_identity_id. */
  worker_identity?: AgentIdentityInput | null
}

export const HUMAN_IDENTITY: AgentIdentityInput = {
  agent_type: 'human',
  model: 'human',
  harness: 'human',
}
