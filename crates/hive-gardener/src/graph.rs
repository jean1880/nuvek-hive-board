//! `graph` stage: project hive findings into Neo4j under the `:HIVE` tenant.
//!
//! HARD RULES: every node this crate writes carries `:HIVE`. The topology label
//! (`HIVE_TOPOLOGY_LABEL`: the tenant of a topology graph another tool owns and maintains) is only
//! ever MATCHed, never MERGEd or created. `CONCERNS` edges attach to EXISTING topology nodes found
//! by name; a missing node means an unlinked finding, never a new node. With no topology label
//! configured, topology linking is off: no allowlist query and no `CONCERNS` edges.
//!
//! Writes use `neo4rs` directly because `nuvek-neo4j` is read-only by design; they are confined
//! to the fixed statements below (data only ever travels as parameters). Cypher cannot take a
//! label as a parameter, so the topology label is interpolated, but only as a [`TopologyLabel`],
//! which is validated against `^[A-Za-z][A-Za-z0-9_]{0,63}$` at startup.

use crate::logic::{entities_from_json, entities_json, str_list, Allowlist, Entity};
use crate::Ctx;
use anyhow::{anyhow, Context, Result};
use neo4rs::{query, BoltType, Graph};
use nuvek_neo4j::{Neo4j, Neo4jConfig};
use reqwest::Method;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::time::Duration;

const WRITE_TIMEOUT: Duration = Duration::from_secs(20);
const PAGE: usize = 500;

/// The topology graph's tenant label, validated so it can be interpolated into Cypher.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TopologyLabel(String);

impl TopologyLabel {
    /// `^[A-Za-z][A-Za-z0-9_]{0,63}$`: a plain identifier, so it cannot close the label, add a
    /// clause or inject anything into a statement.
    pub fn parse(raw: &str) -> Result<Self, String> {
        let mut cs = raw.chars();
        let ok = raw.len() <= 64
            && cs.next().is_some_and(|c| c.is_ascii_alphabetic())
            && cs.all(|c| c.is_ascii_alphanumeric() || c == '_');
        if ok {
            Ok(Self(raw.to_string()))
        } else {
            Err(format!(
                "HIVE_TOPOLOGY_LABEL {raw:?} is not a valid label (^[A-Za-z][A-Za-z0-9_]{{0,63}}$)"
            ))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Topology nodes a finding subject may resolve to, with their type label.
fn allowlist_query(t: &TopologyLabel) -> String {
    let l = t.as_str();
    format!(
        "MATCH (n:{l}) WHERE n:Repository OR n:Container OR n:Package OR n:Skill \
         OR n:AnsibleRole RETURN [x IN labels(n) WHERE x <> '{l}'][0] AS label, n.name AS name"
    )
}

/// Link a finding to an EXISTING topology node: the node is MATCHed, only the edge is MERGEd.
fn concerns_query(t: &TopologyLabel) -> String {
    format!(
        "MATCH (f:HIVE:Finding {{id: $id}}) MATCH (n:{} {{name: $name}}) \
         WHERE $label IN labels(n) MERGE (f)-[:CONCERNS]->(n) RETURN count(*) AS c",
        t.as_str()
    )
}

const TOPIC: &str =
    "MERGE (t:HIVE:HiveTopic {name: $name}) SET t.title = $title, t.kind = $kind, t.status = $status";
const FINDING: &str =
    "MERGE (f:HIVE:Finding {id: $id}) SET f.kind = $kind, f.severity = $severity, \
     f.claim = $claim, f.status = $status, f.topic = $topic, \
     f.name = 'finding #' + toString($id) + ': ' + left($claim, 60)";
// `name` is what graph viewers render; without it every node shows `?`.
const IDENTITY: &str = "MERGE (i:HIVE:HiveIdentity {key: $key}) SET i.agent_type = $agent_type, \
     i.model = $model, i.harness = $harness, i.name = $key";
const FROM_TOPIC: &str =
    "MATCH (f:HIVE:Finding {id: $id}) MATCH (t:HIVE:HiveTopic {name: $topic}) \
     MERGE (f)-[:FROM_TOPIC]->(t) RETURN count(*) AS c";
const RAISED_BY: &str = "MATCH (f:HIVE:Finding {id: $id}) MATCH (i:HIVE:HiveIdentity {key: $key}) \
     MERGE (f)-[:RAISED_BY]->(i)";
const CLEAR_CONCERNS: &str = "MATCH (:HIVE:Finding {id: $id})-[r:CONCERNS]->() DELETE r";

pub async fn connect() -> Result<Neo4j> {
    let password =
        crate::endpoints::neo4j_password().ok_or_else(|| anyhow!("NEO4J_PASSWORD is not set"))?;
    Neo4j::connect(&Neo4jConfig {
        uri: crate::endpoints::neo4j_uri(),
        user: crate::endpoints::neo4j_user(),
        password,
        timeout: None,
    })
    .await
    .context("neo4j connect")
}

pub async fn fetch_allowlist(neo: &Neo4j, topology: &TopologyLabel) -> Result<Allowlist> {
    let rows = neo.query_rows(&allowlist_query(topology)).await?;
    let entries = rows.iter().filter_map(|r| {
        Some(Entity {
            label: r.get("label")?.as_str()?.to_string(),
            name: r.get("name")?.as_str()?.to_string(),
        })
    });
    Ok(Allowlist::new(entries))
}

async fn exec(graph: &Graph, q: neo4rs::Query) -> Result<()> {
    tokio::time::timeout(WRITE_TIMEOUT, graph.run(q))
        .await
        .map_err(|_| anyhow!("neo4j write timed out"))?
        .context("neo4j write")
}

/// Run a statement that ends in `RETURN count(*) AS c` and give back `c`. A MATCH that finds
/// nothing yields 0, which is how a missing endpoint is told apart from a successful MERGE.
async fn exec_count(graph: &Graph, q: neo4rs::Query) -> Result<i64> {
    tokio::time::timeout(WRITE_TIMEOUT, async {
        let mut res = graph.execute(q).await.context("neo4j write")?;
        let mut c = 0i64;
        while let Some(row) = res.next().await.context("neo4j rows")? {
            let v: Value = row.to::<Value>().unwrap_or(Value::Null);
            c += v.get("c").and_then(Value::as_i64).unwrap_or(0);
        }
        Ok::<_, anyhow::Error>(c)
    })
    .await
    .map_err(|_| anyhow!("neo4j write timed out"))?
}

fn s(v: &Value, k: &str) -> String {
    v.get(k).and_then(Value::as_str).unwrap_or("").to_string()
}

/// Newest page-walk of `/api/findings` (ceiling: ids assumed ascending per page; the hive holds
/// at most a few thousand findings, and a page boundary only costs a re-sync next run).
async fn all_findings(ctx: &Ctx) -> Result<Vec<Value>> {
    let mut out = Vec::new();
    let mut after = 0i64;
    for _ in 0..40 {
        let page = ctx
            .hive
            .get(&format!("/api/findings?limit={PAGE}&after={after}"))
            .await?;
        let items = page.as_array().cloned().unwrap_or_default();
        let max = items
            .iter()
            .filter_map(|f| f.get("id").and_then(Value::as_i64))
            .max()
            .unwrap_or(after);
        let n = items.len();
        out.extend(items);
        if n < PAGE || max <= after {
            break;
        }
        after = max;
    }
    Ok(out)
}

fn needs_sync(f: &Value) -> bool {
    let synced = f
        .get("graph_synced_at")
        .and_then(Value::as_str)
        .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok());
    let updated = f
        .get("updated_at")
        .and_then(Value::as_str)
        .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok());
    match (synced, updated) {
        (None, _) => true,
        // 2 s of slack: the PATCH that sets graph_synced also touches updated_at.
        (Some(s), Some(u)) => (u - s).num_seconds() > 2,
        (Some(_), None) => false,
    }
}

/// `resync` re-merges EVERY finding, ignoring `graph_synced_at`. All statements are MERGEs keyed
/// on ids, so it is idempotent; use it after the topology graph recreates its nodes (see
/// `sync_one`). Without a topology label, findings, topics and identities are still projected,
/// but subjects are not resolved and no `CONCERNS` edge is written.
pub async fn sync(ctx: &Ctx, resync: bool) -> Result<crate::Outcome> {
    let neo = connect().await?;
    let graph = neo.graph();
    let topo: Option<(&TopologyLabel, Allowlist)> = match ctx.topology.as_ref() {
        Some(t) => Some((t, fetch_allowlist(&neo, t).await?)),
        None => None,
    };

    let topics: HashMap<String, Value> = ctx
        .hive
        .get("/api/topics?status=all")
        .await?
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|t| (s(&t, "name"), t))
        .collect();
    let identities: HashMap<i64, (String, String, String)> = ctx
        .hive
        .get("/api/leaderboard?limit=100")
        .await
        .ok()
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|i| {
            Some((
                i.get("identity_id")?.as_i64()?,
                (s(&i, "agent_type"), s(&i, "model"), s(&i, "harness")),
            ))
        })
        .collect();

    let pending: Vec<Value> = all_findings(ctx)
        .await?
        .into_iter()
        .filter(|f| resync || needs_sync(f))
        .collect();
    let mut synced = 0i64;
    let mut unlinked = 0i64;
    let mut retry = 0i64;
    let mut failed = 0i64;
    let mut deferred = 0usize;
    let mut consecutive = 0;
    for (i, f) in pending.iter().enumerate() {
        if ctx.expired() {
            deferred = pending.len() - i;
            ctx.log.warn(&format!(
                "graph: run budget spent, deferring {deferred} finding(s)"
            ));
            break;
        }
        let Some(id) = f.get("id").and_then(Value::as_i64) else {
            continue;
        };
        let link = topo.as_ref().map(|(t, a)| (*t, a));
        match sync_one(ctx, graph, link, &topics, &identities, f, id).await {
            Ok(r) => {
                consecutive = 0;
                if r.marked {
                    synced += 1;
                } else {
                    retry += 1;
                }
                if !r.linked {
                    unlinked += 1;
                }
            }
            Err(e) => {
                failed += 1;
                consecutive += 1;
                ctx.log
                    .warn(&format!("graph: finding {id} not synced: {e:#}"));
                if consecutive >= 3 {
                    return Err(anyhow!(
                        "graph: 3 consecutive failures, aborting (synced {synced}, failed {failed})"
                    ));
                }
            }
        }
    }
    Ok(crate::Outcome {
        summary: format!(
            "pending={} synced={synced} unlinked={unlinked} retry={retry} failed={failed} \
             deferred={deferred} resync={resync} allowlist={}",
            pending.len(),
            topo.as_ref()
                .map_or_else(|| "off".to_string(), |(_, a)| a.len().to_string())
        ),
        ..Default::default()
    })
}

/// What `sync_one` achieved for a finding.
struct SyncResult {
    /// At least one `CONCERNS` edge exists.
    linked: bool,
    /// `graph_synced` was set. False when a resolved subject's topology node was not found, so
    /// the finding is retried next run instead of being recorded as done.
    marked: bool,
}

/// Ceiling: a `CONCERNS` edge dies with its topology node. A topology tool that deletes and later
/// recreates its nodes silently drops every edge into them while the finding still reads as
/// synced. Upgrade path: `graph --resync` re-merges everything (idempotent); a periodic resync
/// would automate it.
///
/// `topo` is `None` when no topology label is configured: subjects stay unresolved and no
/// `CONCERNS` edge is cleared or written.
async fn sync_one(
    ctx: &Ctx,
    graph: &Graph,
    topo: Option<(&TopologyLabel, &Allowlist)>,
    topics: &HashMap<String, Value>,
    identities: &HashMap<i64, (String, String, String)>,
    f: &Value,
    id: i64,
) -> Result<SyncResult> {
    let topic = s(f, "topic_name");
    let existing_resolved = entities_from_json(f.get("resolved_subjects").unwrap_or(&Value::Null));
    let existing_unresolved = f
        .get("unresolved_subjects")
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or(0);
    let subjects = str_list(f.get("subjects").unwrap_or(&Value::Null));

    // A typed post arrives with free-text subjects only; resolve them here.
    let mut patch = json!({"actor": "gardener", "actor_class": "gardener", "graph_synced": true});
    let resolved = match topo {
        None => Vec::new(),
        Some((_, allow))
            if existing_resolved.is_empty() && existing_unresolved == 0 && !subjects.is_empty() =>
        {
            let (r, u) = allow.resolve_all(&subjects);
            patch["resolved_subjects"] = entities_json(&r);
            patch["unresolved_subjects"] = json!(u);
            r
        }
        Some(_) => existing_resolved,
    };

    if ctx.hive.is_dry() {
        ctx.log.info(&format!(
            "DRY-RUN would MERGE :HIVE:Finding {id} ({topic}) with {} CONCERNS edge(s)",
            resolved.len()
        ));
        return Ok(SyncResult {
            linked: !resolved.is_empty(),
            marked: true,
        });
    }

    if let Some(t) = topics.get(&topic) {
        exec(
            graph,
            query(TOPIC)
                .param("name", topic.clone())
                .param("title", s(t, "title"))
                .param("kind", s(t, "kind"))
                .param("status", s(t, "status")),
        )
        .await?;
    }
    let severity = match f.get("severity").and_then(Value::as_str) {
        Some(v) => BoltType::from(v.to_string()),
        None => BoltType::Null(Default::default()),
    };
    exec(
        graph,
        query(FINDING)
            .param("id", id)
            .param("kind", s(f, "kind"))
            .param("severity", severity)
            .param("claim", s(f, "claim"))
            .param("status", s(f, "status"))
            .param("topic", topic.clone()),
    )
    .await?;
    let from = exec_count(
        graph,
        query(FROM_TOPIC)
            .param("id", id)
            .param("topic", topic.clone()),
    )
    .await?;
    if from == 0 {
        return Err(anyhow!("topic node {topic:?} not found for FROM_TOPIC"));
    }
    if let Some((at, model, harness)) = f
        .get("identity_id")
        .and_then(Value::as_i64)
        .and_then(|i| identities.get(&i))
    {
        let key = format!("{at}|{model}|{harness}");
        exec(
            graph,
            query(IDENTITY)
                .param("key", key.clone())
                .param("agent_type", at.clone())
                .param("model", model.clone())
                .param("harness", harness.clone()),
        )
        .await?;
        exec(graph, query(RAISED_BY).param("id", id).param("key", key)).await?;
    }
    let mut edges = 0i64;
    let mut missing = 0usize;
    if let Some((label, _)) = topo {
        exec(graph, query(CLEAR_CONCERNS).param("id", id)).await?;
        let concerns = concerns_query(label);
        for e in &resolved {
            let n = exec_count(
                graph,
                query(&concerns)
                    .param("id", id)
                    .param("name", e.name.clone())
                    .param("label", e.label.clone()),
            )
            .await?;
            if n == 0 {
                missing += 1;
                ctx.log.warn(&format!(
                    "graph: finding {id}: no topology {} named {:?}; edge skipped",
                    e.label, e.name
                ));
            }
            edges += n;
        }
    }
    // Do not record the finding as synced while a resolved subject has no node to attach to.
    let marked = missing == 0;
    if !marked {
        if let Some(o) = patch.as_object_mut() {
            o.remove("graph_synced");
        }
    }
    // A PATCH carrying nothing new (not synced, no fresh resolution) would be a no-op write.
    if marked || patch.get("resolved_subjects").is_some() {
        ctx.hive
            .write(
                &ctx.log,
                Method::PATCH,
                &format!("/api/findings/{id}"),
                &patch,
                None,
            )
            .await?;
    }
    Ok(SyncResult {
        linked: edges > 0,
        marked,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two unrelated sample labels: every safety rule must hold for any valid label.
    fn labels() -> [TopologyLabel; 2] {
        [
            TopologyLabel::parse("Topo").unwrap(),
            TopologyLabel::parse("infra_graph_2").unwrap(),
        ]
    }

    /// Every write statement, with the topology label `t` interpolated where it is used.
    fn writes(t: &TopologyLabel) -> Vec<String> {
        [TOPIC, FINDING, IDENTITY, FROM_TOPIC, RAISED_BY]
            .iter()
            .map(|s| s.to_string())
            .chain([concerns_query(t)])
            .collect()
    }

    /// The topology label is never MERGEd/created, every node MERGE carries :HIVE, and no
    /// statement deletes a node (only CONCERNS edges of :HIVE findings are ever deleted).
    #[test]
    fn writes_are_confined_to_the_hive_tenant() {
        for t in labels() {
            let topo = format!(":{}", t.as_str());
            for stmt in writes(&t).iter().chain([&CLEAR_CONCERNS.to_string()]) {
                let up = stmt.to_uppercase();
                assert!(!up.contains("CREATE"), "no CREATE: {stmt}");
                assert!(!up.contains("CALL"), "no CALL: {stmt}");
                assert!(!up.contains("DETACH"), "no node deletion: {stmt}");
                // CLEAR_CONCERNS is the only statement allowed to delete anything (edges).
                if stmt != CLEAR_CONCERNS {
                    let words = up.split(|c: char| !c.is_ascii_alphanumeric() && c != '_');
                    assert!(!words.clone().any(|w| w == "DELETE"), "no DELETE: {stmt}");
                    assert!(!words.clone().any(|w| w == "REMOVE"), "no REMOVE: {stmt}");
                }
                for (i, _) in stmt.match_indices("MERGE (") {
                    let pat = &stmt[i + "MERGE (".len()..];
                    let head: String = pat.chars().take_while(|c| *c != ' ' && *c != ')').collect();
                    if head.contains(':') {
                        assert!(head.contains(":HIVE"), "node MERGE without :HIVE: {stmt}");
                        assert!(!head.contains(&topo), "MERGE of {topo}: {stmt}");
                    }
                }
            }
        }
    }

    #[test]
    fn the_topology_label_is_only_matched_by_name_never_merged() {
        for t in labels() {
            let l = t.as_str();
            let concerns = concerns_query(&t);
            // The single write that mentions the label matches it and MERGEs only the edge.
            assert!(
                concerns.contains(&format!("MATCH (n:{l} {{name: $name}})")),
                "{concerns}"
            );
            assert!(!concerns.contains("MERGE (n"), "{concerns}");
            assert!(
                concerns.contains("MERGE (f)-[:CONCERNS]->(n)"),
                "{concerns}"
            );
            for stmt in [
                TOPIC,
                FINDING,
                IDENTITY,
                FROM_TOPIC,
                RAISED_BY,
                CLEAR_CONCERNS,
            ] {
                assert!(!stmt.contains(&format!(":{l}")), "{stmt}");
            }
        }
        // The one DELETE removes edges from a :HIVE finding, never a node.
        assert!(CLEAR_CONCERNS.starts_with("MATCH (:HIVE:Finding"));
        assert!(CLEAR_CONCERNS.ends_with("DELETE r"));
    }

    #[test]
    fn allowlist_query_is_read_only_per_the_nuvek_guard() {
        // nuvek-neo4j rejects write words; this query must pass its guard.
        for t in labels() {
            let q = allowlist_query(&t);
            assert!(q.starts_with(&format!("MATCH (n:{}) ", t.as_str())), "{q}");
            for w in [
                "CREATE", "MERGE", "DELETE", "DETACH", "SET", "REMOVE", "DROP", "CALL",
            ] {
                let hit = q
                    .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                    .any(|tok| tok.eq_ignore_ascii_case(w));
                assert!(!hit, "{w} in allowlist query: {q}");
            }
        }
    }

    #[test]
    fn topology_labels_are_validated_before_interpolation() {
        for ok in [
            "T",
            "Topo",
            "infra_graph_2",
            &format!("A{}", "b".repeat(63)),
        ] {
            assert!(TopologyLabel::parse(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "1abc",
            "_x",
            "a-b",
            "a b",
            "a:b",
            "a`b",
            "x) DETACH DELETE (n",
            "é",
            &format!("A{}", "b".repeat(64)),
        ] {
            assert!(TopologyLabel::parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn sync_selection_uses_synced_at_with_slack() {
        let never = json!({"graph_synced_at": null, "updated_at": "2026-10-07T00:00:00+00:00"});
        assert!(needs_sync(&never));
        let fresh = json!({"graph_synced_at": "2026-10-07T00:00:00+00:00", "updated_at": "2026-10-07T00:00:01+00:00"});
        assert!(!needs_sync(&fresh));
        let stale = json!({"graph_synced_at": "2026-10-07T00:00:00+00:00", "updated_at": "2026-10-07T00:05:00+00:00"});
        assert!(needs_sync(&stale));
    }
}
