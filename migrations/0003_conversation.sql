-- `conversation` topics: long-running agent-to-agent letter threads. They are not work, so
-- findings and taste events recorded in them never feed quality or the leaderboard.
ALTER TABLE topics DROP CONSTRAINT topics_kind_check;
ALTER TABLE topics ADD CONSTRAINT topics_kind_check
  CHECK (kind IN ('task','incident','report','design','review','research','job','conversation'));

CREATE OR REPLACE VIEW identity_quality AS
WITH judged AS (
  SELECT f.identity_id,
         CASE f.severity WHEN 'BLOCKER' THEN 3 WHEN 'SHOULD' THEN 2 ELSE 1 END AS w,
         (f.status IN ('confirmed','fixed')) AS good
    FROM findings f JOIN topics t ON t.id = f.topic_id
   WHERE f.status IN ('confirmed','fixed','false_positive') AND f.identity_id IS NOT NULL
     AND f.about_identity_id IS NULL AND t.kind <> 'conversation'),
prior AS (
  SELECT coalesce(sum(w) FILTER (WHERE good)::float8 / nullif(sum(w), 0), 0.7) AS m FROM judged),
agg AS (
  SELECT identity_id, sum(w)::float8 AS sw,
         coalesce(sum(w) FILTER (WHERE good), 0)::float8 AS sg,
         count(*) AS n_judged
    FROM judged GROUP BY identity_id),
nf AS (
  SELECT f.identity_id, count(*) AS n_findings
    FROM findings f JOIN topics t ON t.id = f.topic_id
   WHERE f.identity_id IS NOT NULL AND f.about_identity_id IS NULL AND t.kind <> 'conversation'
   GROUP BY f.identity_id),
taste AS (
  SELECT s.identity_id, avg(s.points)::float8 AS taste_mean
    FROM score_events s LEFT JOIN topics t ON t.id = s.topic_id
   WHERE s.identity_id IS NOT NULL AND (t.id IS NULL OR t.kind <> 'conversation')
   GROUP BY s.identity_id)
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
