-- hive-board v0.2.0 schema. Postgres is the single source of truth.

CREATE EXTENSION IF NOT EXISTS pg_trgm;

-- Generated columns need IMMUTABLE expressions; array_to_string is only STABLE,
-- so wrap it in an IMMUTABLE helper (safe: text[] -> text has no locale dependence).
CREATE FUNCTION hive_join(text[]) RETURNS text
  LANGUAGE sql IMMUTABLE PARALLEL SAFE
  AS $$ SELECT coalesce(array_to_string($1, ' '), '') $$;

CREATE TABLE identities (
  id bigserial PRIMARY KEY,
  agent_type text NOT NULL,
  model text NOT NULL,
  harness text NOT NULL,
  UNIQUE (agent_type, model, harness));

CREATE TABLE aliases (
  alias text PRIMARY KEY,
  identity_id bigint NOT NULL REFERENCES identities(id),
  first_seen timestamptz NOT NULL DEFAULT now());

CREATE TABLE topics (
  id bigserial PRIMARY KEY,
  name text UNIQUE NOT NULL,
  title text NOT NULL,
  description text,
  created_by text NOT NULL,
  kind text NOT NULL DEFAULT 'task'
    CHECK (kind IN ('task','incident','report','design','review','research')),
  status text NOT NULL DEFAULT 'active'
    CHECK (status IN ('active','quiet','needs-human','resolved','archived')),
  outcome text,
  last_activity_at timestamptz NOT NULL DEFAULT now(),
  distilled_through bigint NOT NULL DEFAULT 0,
  distilled_at timestamptz,
  created_at timestamptz NOT NULL DEFAULT now(),
  updated_at timestamptz NOT NULL DEFAULT now(),
  legacy_source text,
  legacy_id bigint,
  UNIQUE (legacy_source, legacy_id),
  search tsvector GENERATED ALWAYS AS (
    to_tsvector('english', coalesce(title,'') || ' ' || coalesce(description,'') || ' ' || name)) STORED);

CREATE TABLE messages (
  id bigserial PRIMARY KEY,
  topic_id bigint NOT NULL REFERENCES topics(id) ON DELETE CASCADE,
  parent_id bigint REFERENCES messages(id) ON DELETE CASCADE,
  author text NOT NULL,
  identity_id bigint REFERENCES identities(id),
  content text NOT NULL,
  tags text[] NOT NULL DEFAULT '{}',
  kind text NOT NULL DEFAULT 'note'
    CHECK (kind IN ('note','finding','decision','gotcha','question','verdict','brief','system','progress')),
  severity text CHECK (severity IN ('BLOCKER','SHOULD','NICE')),
  subjects text[] NOT NULL DEFAULT '{}',
  evidence text,
  task_kind text,
  ephemeral boolean NOT NULL DEFAULT false,
  compacted_at timestamptz,
  idem_key text UNIQUE,
  created_at timestamptz NOT NULL DEFAULT now(),
  updated_at timestamptz NOT NULL DEFAULT now(),
  legacy_source text,
  legacy_id bigint,
  UNIQUE (legacy_source, legacy_id),
  search tsvector GENERATED ALWAYS AS (
    setweight(to_tsvector('english', coalesce(content,'')), 'B') ||
    setweight(to_tsvector('simple', hive_join(tags) || ' ' || hive_join(subjects)), 'A')) STORED);

CREATE TABLE findings (
  id bigserial PRIMARY KEY,
  topic_id bigint NOT NULL REFERENCES topics(id) ON DELETE CASCADE,
  message_id bigint REFERENCES messages(id) ON DELETE SET NULL,
  identity_id bigint REFERENCES identities(id),
  kind text NOT NULL CHECK (kind IN ('finding','decision','gotcha','verdict','question')),
  severity text CHECK (severity IN ('BLOCKER','SHOULD','NICE')),
  claim text NOT NULL,
  evidence text,
  subjects text[] NOT NULL DEFAULT '{}',
  resolved_subjects jsonb NOT NULL DEFAULT '[]',
  unresolved_subjects text[] NOT NULL DEFAULT '{}',
  task_kind text,
  confidence real,
  provenance text NOT NULL DEFAULT 'agent'
    CHECK (provenance IN ('agent','orchestrator','gardener','human','external')),
  content_hash text NOT NULL,
  reinforce_count int NOT NULL DEFAULT 1,
  duplicate_of bigint REFERENCES findings(id),
  status text NOT NULL DEFAULT 'open'
    CHECK (status IN ('open','confirmed','fixed','false_positive','wontfix','superseded','expired')),
  status_changed_at timestamptz,
  status_changed_by text,
  graph_synced_at timestamptz,
  created_at timestamptz NOT NULL DEFAULT now(),
  updated_at timestamptz NOT NULL DEFAULT now(),
  UNIQUE (topic_id, content_hash),
  search tsvector GENERATED ALWAYS AS (
    to_tsvector('english', claim || ' ' || coalesce(evidence,'') || ' ' || hive_join(subjects))) STORED);

CREATE TABLE finding_events (
  id bigserial PRIMARY KEY,
  finding_id bigint NOT NULL REFERENCES findings(id) ON DELETE CASCADE,
  from_status text,
  to_status text NOT NULL,
  actor text NOT NULL,
  actor_class text NOT NULL CHECK (actor_class IN ('orchestrator','human','gardener','system')),
  note text,
  at timestamptz NOT NULL DEFAULT now());

CREATE TABLE score_events (
  id bigserial PRIMARY KEY,
  identity_id bigint REFERENCES identities(id),
  alias text NOT NULL,
  topic_id bigint REFERENCES topics(id) ON DELETE SET NULL,
  points int NOT NULL CHECK (points BETWEEN 0 AND 100),
  reason text NOT NULL,
  scorer text NOT NULL,
  top_contributor boolean NOT NULL DEFAULT false,
  legacy boolean NOT NULL DEFAULT false,
  at timestamptz NOT NULL DEFAULT now());

CREATE TABLE digests (
  id bigserial PRIMARY KEY,
  topic_id bigint NOT NULL REFERENCES topics(id) ON DELETE CASCADE,
  version int NOT NULL,
  summary text NOT NULL,
  transcript_md text NOT NULL,
  participants text[] NOT NULL DEFAULT '{}',
  finding_ids bigint[] NOT NULL DEFAULT '{}',
  through_message_id bigint NOT NULL DEFAULT 0,
  producer text NOT NULL,
  created_at timestamptz NOT NULL DEFAULT now(),
  UNIQUE (topic_id, version),
  legacy_source text,
  legacy_name text,
  UNIQUE (legacy_source, legacy_name),
  search tsvector GENERATED ALWAYS AS (to_tsvector('english', summary)) STORED);

CREATE TABLE topic_events (
  id bigserial PRIMARY KEY,
  topic_id bigint NOT NULL REFERENCES topics(id) ON DELETE CASCADE,
  event text NOT NULL,
  actor text NOT NULL,
  detail jsonb NOT NULL DEFAULT '{}',
  at timestamptz NOT NULL DEFAULT now());

CREATE TABLE proposals (
  id bigserial PRIMARY KEY,
  kind text NOT NULL CHECK (kind IN ('memory','skill','gotcha-note','sentinel-signature')),
  title text NOT NULL,
  body text NOT NULL,
  target text,
  evidence_finding_ids bigint[] NOT NULL DEFAULT '{}',
  fingerprint text UNIQUE NOT NULL,
  status text NOT NULL DEFAULT 'proposed'
    CHECK (status IN ('proposed','approved','rejected','applied','expired')),
  decided_by text,
  decided_at timestamptz,
  created_at timestamptz NOT NULL DEFAULT now());

CREATE TABLE kv (
  k text PRIMARY KEY,
  v jsonb NOT NULL,
  updated_at timestamptz NOT NULL DEFAULT now());

CREATE TABLE gardener_runs (
  id bigserial PRIMARY KEY,
  started_at timestamptz NOT NULL DEFAULT now(),
  finished_at timestamptz,
  ok boolean,
  stage text,
  topics_swept int DEFAULT 0,
  topics_distilled int DEFAULT 0,
  findings_written int DEFAULT 0,
  quarantined int DEFAULT 0,
  notes text);

CREATE INDEX topics_search_idx ON topics USING gin (search);
CREATE INDEX messages_search_idx ON messages USING gin (search);
CREATE INDEX findings_search_idx ON findings USING gin (search);
CREATE INDEX digests_search_idx ON digests USING gin (search);
CREATE INDEX messages_subjects_idx ON messages USING gin (subjects);
CREATE INDEX findings_subjects_idx ON findings USING gin (subjects);
CREATE INDEX messages_tags_idx ON messages USING gin (tags);
CREATE INDEX topics_name_trgm_idx ON topics USING gin (name gin_trgm_ops);
CREATE INDEX topics_title_trgm_idx ON topics USING gin (title gin_trgm_ops);
CREATE INDEX aliases_alias_trgm_idx ON aliases USING gin (alias gin_trgm_ops);
CREATE INDEX messages_topic_idx ON messages (topic_id, id);
CREATE INDEX findings_status_idx ON findings (status);
CREATE INDEX findings_hash_idx ON findings (content_hash);
CREATE INDEX findings_updated_idx ON findings (updated_at);
CREATE INDEX topics_status_activity_idx ON topics (status, last_activity_at);
CREATE INDEX topic_events_topic_idx ON topic_events (topic_id, id);
CREATE INDEX score_events_identity_idx ON score_events (identity_id);

-- Routing quality: Bayesian confirmed-rate over finding outcomes per identity,
-- severity-weighted (BLOCKER 3, SHOULD 2, NICE/null 1), prior m (global rate, 0.7 fallback), k = 8.
-- taste_mean is the orchestrator score signal and is never mixed into quality.
CREATE VIEW identity_quality AS
WITH judged AS (
  SELECT identity_id,
         CASE severity WHEN 'BLOCKER' THEN 3 WHEN 'SHOULD' THEN 2 ELSE 1 END AS w,
         (status IN ('confirmed','fixed')) AS good
    FROM findings
   WHERE status IN ('confirmed','fixed','false_positive') AND identity_id IS NOT NULL),
prior AS (
  SELECT coalesce(sum(w) FILTER (WHERE good)::float8 / nullif(sum(w), 0), 0.7) AS m FROM judged),
agg AS (
  SELECT identity_id, sum(w)::float8 AS sw,
         coalesce(sum(w) FILTER (WHERE good), 0)::float8 AS sg,
         count(*) AS n_judged
    FROM judged GROUP BY identity_id),
nf AS (
  SELECT identity_id, count(*) AS n_findings FROM findings
   WHERE identity_id IS NOT NULL GROUP BY identity_id),
taste AS (
  SELECT identity_id, avg(points)::float8 AS taste_mean FROM score_events
   WHERE identity_id IS NOT NULL GROUP BY identity_id)
SELECT i.id AS identity_id, i.agent_type, i.model, i.harness,
       ((coalesce(a.sg, 0) + 8 * p.m) / (coalesce(a.sw, 0) + 8))::float8 AS quality,
       coalesce(a.n_judged, 0)::bigint AS n_judged,
       coalesce(nf.n_findings, 0)::bigint AS n_findings,
       t.taste_mean,
       (coalesce(a.n_judged, 0) < 5) AS insufficient_data
  FROM identities i
 CROSS JOIN prior p
  LEFT JOIN agg a ON a.identity_id = i.id
  LEFT JOIN nf ON nf.identity_id = i.id
  LEFT JOIN taste t ON t.identity_id = i.id
 WHERE nf.n_findings IS NOT NULL OR t.taste_mean IS NOT NULL;
