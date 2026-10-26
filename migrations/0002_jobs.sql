-- hive-board v0.3.0: Jobs, Phase A. Additive; v0.2 rows are untouched.
-- Rollback (sqlx has no down migrations): see "Rolling back 0002" in README.md.

-- Constraint names are the ones Postgres generated for the column CHECKs in 0001.
ALTER TABLE topics DROP CONSTRAINT topics_kind_check;
ALTER TABLE topics ADD CONSTRAINT topics_kind_check
  CHECK (kind IN ('task','incident','report','design','review','research','job'));

ALTER TABLE messages DROP CONSTRAINT messages_kind_check;
ALTER TABLE messages ADD CONSTRAINT messages_kind_check CHECK (kind IN
  ('note','finding','decision','gotcha','question','verdict','brief','system','progress','result','amendment'));

-- Whose work a verdict judges (set on job verdict findings only).
ALTER TABLE findings ADD COLUMN about_identity_id bigint REFERENCES identities(id);

CREATE TABLE jobs (
  topic_id bigint PRIMARY KEY REFERENCES topics(id) ON DELETE CASCADE,
  spec text NOT NULL,                       -- immutable; changes are `amendment` messages
  priority int NOT NULL DEFAULT 100,        -- lower runs first
  capability text NOT NULL CHECK (capability IN ('read-only','local-build')),  -- live-infra refused in Phase A
  security boolean NOT NULL DEFAULT false,  -- set at creation, immutable: verify needs a different HARNESS
  task_kind text,
  owns text[] NOT NULL DEFAULT '{}',        -- normalised absolute dirs/files; non-empty for local-build
  tier text CHECK (tier IN ('fast','standard','deep')),
  agent_type text,
  status text NOT NULL DEFAULT 'open'
    CHECK (status IN ('open','claimed','done','verified','failed','cancelled')),
  claimed_by text,
  claimed_at timestamptz,
  claim_nonce_hash text,                    -- sha256 of the nonce; the nonce is returned ONCE, never readable
  round int NOT NULL DEFAULT 0,             -- fencing: claim sets 1, verify-fail adds 1, reclaim resets to 0
  verify_fails int NOT NULL DEFAULT 0,      -- first-pass = verified AND verify_fails = 0
  worker_identity_id bigint REFERENCES identities(id),  -- who did the work (set at complete)
  result_message_id bigint REFERENCES messages(id) ON DELETE SET NULL,
  verdict_message_id bigint REFERENCES messages(id) ON DELETE SET NULL,
  completed_at timestamptz,
  verified_at timestamptz,
  ready_at timestamptz NOT NULL DEFAULT now(),   -- when the job last became open
  attempts int NOT NULL DEFAULT 0,          -- claim count; history is topic_events
  created_by text NOT NULL,
  created_at timestamptz NOT NULL DEFAULT now(),
  updated_at timestamptz NOT NULL DEFAULT now());

CREATE TABLE job_deps (
  job_id bigint NOT NULL REFERENCES jobs(topic_id) ON DELETE CASCADE,
  depends_on bigint NOT NULL REFERENCES jobs(topic_id) ON DELETE RESTRICT,
  PRIMARY KEY (job_id, depends_on),
  CHECK (job_id <> depends_on));

CREATE INDEX jobs_status_priority_idx ON jobs (status, priority);
CREATE INDEX job_deps_depends_on_idx ON job_deps (depends_on);
CREATE INDEX findings_about_identity_idx ON findings (about_identity_id) WHERE about_identity_id IS NOT NULL;

-- Job verdicts neither feed quality nor put verifiers on the leaderboard in Phase A:
-- `about_identity_id IS NULL` in the judged part AND in the finding-count (nf) part.
CREATE OR REPLACE VIEW identity_quality AS
WITH judged AS (
  SELECT identity_id,
         CASE severity WHEN 'BLOCKER' THEN 3 WHEN 'SHOULD' THEN 2 ELSE 1 END AS w,
         (status IN ('confirmed','fixed')) AS good
    FROM findings
   WHERE status IN ('confirmed','fixed','false_positive') AND identity_id IS NOT NULL
     AND about_identity_id IS NULL),
prior AS (
  SELECT coalesce(sum(w) FILTER (WHERE good)::float8 / nullif(sum(w), 0), 0.7) AS m FROM judged),
agg AS (
  SELECT identity_id, sum(w)::float8 AS sw,
         coalesce(sum(w) FILTER (WHERE good), 0)::float8 AS sg,
         count(*) AS n_judged
    FROM judged GROUP BY identity_id),
nf AS (
  SELECT identity_id, count(*) AS n_findings FROM findings
   WHERE identity_id IS NOT NULL AND about_identity_id IS NULL GROUP BY identity_id),
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
