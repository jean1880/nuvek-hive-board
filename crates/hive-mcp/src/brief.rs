//! The "Prior Hive context" brief: Postgres-side findings/digests/topics from `/api/brief`,
//! optionally enriched by live-graph subject matching (Neo4j) and semantic recall (Chroma
//! `hive-findings`). Every optional source that is unavailable is named in an `omitted:` footer;
//! the whole brief is held to a character budget.

use crate::client::Hive;
use serde_json::Value;
use std::time::Duration;

/// Hard cap on the composed brief, footer included.
pub const BUDGET: usize = 6000;
const ENTRY_CAP: usize = 300;
const TRUNC_RESERVE: usize = 60;
const CHROMA_COLLECTION: &str = "hive-findings";

pub struct SemanticHit {
    pub label: String,
    pub text: String,
}

#[derive(Default)]
pub struct Inputs {
    /// Parsed `/api/brief` response; `None` when the call failed.
    pub api: Option<Value>,
    /// Chroma hits; `None` when the source was unavailable.
    pub semantic: Option<Vec<SemanticHit>>,
    /// One short phrase per unavailable source, e.g. "neo4j subjects (NEO4J_PASSWORD unset)".
    pub omitted: Vec<String>,
    /// Topic to leave out of `related_topics` (the topic the brief is for).
    pub exclude_topic: Option<String>,
}

fn clip(s: &str, n: usize) -> String {
    let flat: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= n {
        flat
    } else {
        let mut t: String = flat.chars().take(n.saturating_sub(1)).collect();
        t.push('…');
        t
    }
}

fn s<'a>(v: &'a Value, keys: &[&str]) -> &'a str {
    keys.iter().find_map(|k| v[*k].as_str()).unwrap_or_default()
}

fn api_lines(api: &Value, exclude: Option<&str>) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(fs) = api["findings"].as_array() {
        for f in fs {
            let sev = f["severity"].as_str().unwrap_or("-");
            let subjects: Vec<&str> = f["subjects"]
                .as_array()
                .map(|a| a.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            lines.push(clip(
                &format!(
                    "- finding #{} [{sev}/{}/{}] {} (topic: {}{})",
                    f["id"],
                    s(f, &["status"]),
                    s(f, &["kind"]),
                    s(f, &["claim"]),
                    s(f, &["topic_name", "topic"]),
                    if subjects.is_empty() {
                        String::new()
                    } else {
                        format!("; subjects: {}", subjects.join(","))
                    }
                ),
                ENTRY_CAP,
            ));
        }
    }
    if let Some(ds) = api["digests"].as_array() {
        for d in ds {
            lines.push(clip(
                &format!(
                    "- digest {}: {}",
                    s(d, &["topic_name", "topic"]),
                    s(d, &["summary"])
                ),
                ENTRY_CAP,
            ));
        }
    }
    if let Some(ts) = api["related_topics"].as_array() {
        for t in ts {
            let name = s(t, &["name"]);
            if Some(name) == exclude {
                continue;
            }
            lines.push(clip(
                &format!(
                    "- topic {name} ({}): {}",
                    s(t, &["status"]),
                    s(t, &["title"])
                ),
                ENTRY_CAP,
            ));
        }
    }
    lines
}

/// Compose the brief. Pure, so the budget and footer are unit-testable.
pub fn compose(inp: &Inputs, budget: usize) -> String {
    let header = "## Prior Hive context";
    let footer = if inp.omitted.is_empty() {
        String::new()
    } else {
        clip(&format!("omitted: {}", inp.omitted.join("; ")), 400)
    };

    let mut lines = inp
        .api
        .as_ref()
        .map(|a| api_lines(a, inp.exclude_topic.as_deref()))
        .unwrap_or_default();
    if let Some(hits) = &inp.semantic {
        for h in hits {
            lines.push(clip(
                &format!("- semantic [{}] {}", h.label, h.text),
                ENTRY_CAP,
            ));
        }
    }
    if lines.is_empty() {
        lines.push("_No prior context found._".to_string());
    }

    let fixed = header.chars().count() + footer.chars().count() + 4 + TRUNC_RESERVE;
    let room = budget.saturating_sub(fixed);
    let mut used = 0usize;
    let mut kept: Vec<String> = Vec::new();
    for l in &lines {
        let n = l.chars().count() + 1;
        if used + n > room {
            break;
        }
        used += n;
        kept.push(l.clone());
    }
    let dropped = lines.len() - kept.len();
    let mut out = String::from(header);
    out.push('\n');
    for l in &kept {
        out.push_str(l);
        out.push('\n');
    }
    if dropped > 0 {
        out.push_str(&format!("… {dropped} more entries cut (brief budget)\n"));
    }
    if !footer.is_empty() {
        out.push('\n');
        out.push_str(&footer);
    }
    // Belt and braces: the contract is a hard cap.
    out.chars().take(budget).collect()
}

/// Graph node names that occur in `text` as whole tokens (min length 3), deduplicated, capped.
pub fn match_subjects(names: &[String], text: &str) -> Vec<String> {
    let hay: Vec<char> = text.to_lowercase().chars().collect();
    let boundary = |c: Option<&char>| c.is_none_or(|c| !c.is_alphanumeric());
    let mut found: Vec<String> = Vec::new();
    for name in names {
        let needle: Vec<char> = name.to_lowercase().chars().collect();
        if needle.len() < 3 || needle.len() > hay.len() || found.contains(name) {
            continue;
        }
        let hit = (0..=hay.len() - needle.len()).any(|i| {
            hay[i..i + needle.len()] == needle[..]
                && boundary(i.checked_sub(1).and_then(|j| hay.get(j)))
                && boundary(hay.get(i + needle.len()))
        });
        if hit {
            found.push(name.clone());
        }
        if found.len() >= 12 {
            break;
        }
    }
    found
}

/// Where the optional recall sources live. `gather` reads them from the environment; tests pass
/// their own so they never reach a real server.
pub struct Sources {
    /// `None` (no `NEO4J_PASSWORD`) skips graph subject matching.
    pub neo4j: Option<nuvek_neo4j::Neo4jConfig>,
    /// `None` (no `HIVE_TOPOLOGY_LABEL`) skips the graph source entirely, silently.
    pub topology: Option<crate::endpoints::TopologyLabel>,
    pub chroma: nuvek_chroma::ChromaConfig,
}

impl Sources {
    pub fn from_env() -> Self {
        Self {
            neo4j: crate::endpoints::neo4j_config(),
            // Validated at startup (`main` exits on an invalid value), so an error cannot occur
            // here; treat it as "off" all the same.
            topology: crate::endpoints::topology_label().ok().flatten(),
            chroma: crate::endpoints::chroma_config(),
        }
    }
}

/// Names of topology nodes a subject may match. The label is interpolated (Cypher labels
/// cannot be parameters), so it is only ever a validated [`crate::endpoints::TopologyLabel`].
fn topology_names_query(t: &crate::endpoints::TopologyLabel) -> String {
    format!(
        "MATCH (n:{}) WHERE n:Repository OR n:Container OR n:Package OR n:Skill \
         RETURN n.name AS name",
        t.as_str()
    )
}

async fn graph_names(
    cfg: Option<&nuvek_neo4j::Neo4jConfig>,
    topology: &crate::endpoints::TopologyLabel,
) -> Result<Vec<String>, String> {
    let cfg = cfg.ok_or("NEO4J_PASSWORD unset")?;
    let q = topology_names_query(topology);
    let fut = async {
        let g = nuvek_neo4j::Neo4j::connect(cfg)
            .await
            .map_err(|e| format!("{e:#}"))?;
        g.query_rows(&q).await.map_err(|e| format!("{e:#}"))
    };
    let rows = tokio::time::timeout(Duration::from_secs(8), fut)
        .await
        .map_err(|_| "timed out".to_string())??;
    Ok(rows
        .iter()
        .filter_map(|r| r["name"].as_str().map(str::to_string))
        .collect())
}

/// Semantic top-4 from `hive-findings`. `Ok(None)` = the collection does not exist (silent).
async fn semantic(
    cfg: nuvek_chroma::ChromaConfig,
    q: &str,
) -> Result<Option<Vec<SemanticHit>>, String> {
    let fut = async {
        let rag = nuvek_chroma::Rag::new(cfg).map_err(|e| format!("{e:#}"))?;
        // find_collection_id never creates: query_collection would create the collection on a
        // miss, and a brief must not write to Chroma. A missing collection is skipped silently.
        let exists = rag
            .find_collection_id(CHROMA_COLLECTION)
            .await
            .map_err(|e| format!("{e:#}"))?
            .is_some();
        if !exists {
            return Ok(None);
        }
        rag.query_collection(q, CHROMA_COLLECTION, 4, None, false)
            .await
            .map(Some)
            .map_err(|e| format!("{e:#}"))
    };
    match tokio::time::timeout(Duration::from_secs(10), fut).await {
        Err(_) => Err("timed out".into()),
        Ok(Err(e)) => Err(e),
        Ok(Ok(None)) => Ok(None),
        Ok(Ok(Some(recs))) => Ok(Some(
            recs.into_iter()
                .map(|r| SemanticHit {
                    label: ["topic", "topic_name", "status"]
                        .iter()
                        .find_map(|k| r.metadata[*k].as_str())
                        .unwrap_or("finding")
                        .to_string(),
                    text: r.document,
                })
                .collect(),
        )),
    }
}

/// Gather every source and compose. Never fails: unavailable sources land in the footer.
pub async fn gather(
    hive: &Hive,
    query: &str,
    given_subjects: &[String],
    exclude_topic: Option<&str>,
) -> String {
    gather_with(
        hive,
        Sources::from_env(),
        query,
        given_subjects,
        exclude_topic,
    )
    .await
}

/// [`gather`] against explicit sources.
///
/// The query (a topic title and description, or a search) and the subjects are redacted ONCE,
/// first, with `redact_config` (token shapes plus URL credentials and `KEY=value` forms), before
/// any truncation, and only the redacted text reaches any source: the board, the embedding model
/// and the graph matcher.
pub async fn gather_with(
    hive: &Hive,
    sources: Sources,
    query: &str,
    given_subjects: &[String],
    exclude_topic: Option<&str>,
) -> String {
    let query = nuvek_core::scrub::redact_config(query);
    let query = query.as_str();
    let mut inp = Inputs {
        exclude_topic: exclude_topic.map(str::to_string),
        ..Default::default()
    };
    let mut subjects: Vec<String> = given_subjects
        .iter()
        .map(|s| nuvek_core::scrub::redact_config(s))
        .collect();
    // No topology label: the graph source is off by configuration, not missing, so it is
    // skipped without a footer note.
    if let Some(topology) = sources.topology.as_ref() {
        match graph_names(sources.neo4j.as_ref(), topology).await {
            Ok(names) => {
                for m in match_subjects(&names, query) {
                    if !subjects.contains(&m) {
                        subjects.push(m);
                    }
                }
            }
            Err(e) => inp.omitted.push(format!("neo4j subjects ({e})")),
        }
    }

    let mut q = vec![("limit", "8".to_string())];
    if !query.trim().is_empty() {
        q.push(("q", query.chars().take(500).collect()));
    }
    if !subjects.is_empty() {
        q.push(("subjects", subjects.join(",")));
    }
    let (api, sem) = tokio::join!(hive.get("/api/brief", &q), semantic(sources.chroma, query));
    match api {
        Ok(v) => inp.api = Some(v),
        Err(e) => inp.omitted.push(format!("hive-board brief ({e})")),
    }
    match sem {
        Ok(Some(h)) => inp.semantic = Some(h),
        Ok(None) => {}
        Err(e) => inp.omitted.push(format!("chroma hive-findings ({e})")),
    }
    compose(&inp, BUDGET)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn big_api(n: usize) -> Value {
        let findings: Vec<Value> = (0..n)
            .map(|i| {
                json!({"id": i, "severity": "SHOULD", "status": "open", "kind": "gotcha",
                       "claim": "x".repeat(250), "topic_name": "t", "subjects": ["hive-board"]})
            })
            .collect();
        json!({"findings": findings, "digests": [], "related_topics": []})
    }

    #[test]
    fn budget_is_hard_and_footer_survives() {
        let inp = Inputs {
            api: Some(big_api(100)),
            omitted: vec!["neo4j subjects (NEO4J_PASSWORD unset)".into()],
            ..Default::default()
        };
        let out = compose(&inp, BUDGET);
        assert!(out.chars().count() <= BUDGET, "len {}", out.chars().count());
        assert!(out.contains("more entries cut"));
        assert!(out.ends_with("omitted: neo4j subjects (NEO4J_PASSWORD unset)"));
    }

    #[test]
    fn small_budget_still_holds_with_multibyte() {
        let inp = Inputs {
            api: Some(json!({"findings": [{"id": 1, "claim": "é".repeat(400)}]})),
            omitted: vec!["chroma hive-findings (timed out)".into()],
            ..Default::default()
        };
        let out = compose(&inp, 300);
        assert!(out.chars().count() <= 300);
        assert!(out.contains("omitted: chroma"));
    }

    #[test]
    fn empty_brief_says_so_and_no_footer_when_nothing_omitted() {
        let out = compose(&Inputs::default(), BUDGET);
        assert!(out.contains("No prior context found"));
        assert!(!out.contains("omitted:"));
    }

    #[test]
    fn excludes_own_topic_and_renders_semantic() {
        let inp = Inputs {
            api: Some(json!({"related_topics": [
                {"name": "me", "status": "active", "title": "self"},
                {"name": "other", "status": "resolved", "title": "prior"}]})),
            semantic: Some(vec![SemanticHit {
                label: "t".into(),
                text: "hit".into(),
            }]),
            exclude_topic: Some("me".into()),
            ..Default::default()
        };
        let out = compose(&inp, BUDGET);
        assert!(!out.contains("topic me"));
        assert!(out.contains("topic other (resolved)"));
        assert!(out.contains("semantic [t] hit"));
    }

    #[test]
    fn topology_query_uses_only_a_validated_label_and_never_writes() {
        use crate::endpoints::TopologyLabel;
        for l in ["Topo", "infra_graph_2"] {
            let t = TopologyLabel::parse(l).unwrap();
            let q = topology_names_query(&t);
            assert!(q.starts_with(&format!("MATCH (n:{l}) ")), "{q}");
            for w in [
                "CREATE", "MERGE", "DELETE", "DETACH", "SET", "REMOVE", "DROP", "CALL",
            ] {
                let hit = q
                    .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                    .any(|tok| tok.eq_ignore_ascii_case(w));
                assert!(!hit, "{w} in {q}");
            }
        }
        for bad in [
            "",
            "1a",
            "a b",
            "a`b",
            "x) DETACH DELETE (n",
            "é",
            &"a".repeat(65),
        ] {
            assert!(TopologyLabel::parse(bad).is_err(), "{bad:?}");
        }
    }

    #[tokio::test]
    async fn no_topology_label_skips_the_graph_source_silently() {
        // A Neo4j config pointing nowhere: with no topology label it must not even be tried.
        let hive = Hive::new("http://127.0.0.1:9", None).unwrap();
        let sources = Sources {
            neo4j: Some(nuvek_neo4j::Neo4jConfig {
                uri: "bolt://127.0.0.1:9".into(),
                user: "u".into(),
                password: "p".into(),
                timeout: Some(Duration::from_millis(200)),
            }),
            topology: None,
            chroma: nuvek_chroma::ChromaConfig {
                chroma_base: "http://127.0.0.1:9".into(),
                ollama_base: "http://127.0.0.1:9".into(),
                ..Default::default()
            },
        };
        let out = gather_with(&hive, sources, "anything", &[], None).await;
        assert!(!out.contains("neo4j subjects"), "{out}");
    }

    #[test]
    fn subject_matching_is_whole_token() {
        let names: Vec<String> = ["hive-board", "ha", "nas", "cache"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let got = match_subjects(&names, "Fix the Hive-Board client; nasty bug on cache.");
        assert_eq!(got, vec!["hive-board".to_string(), "cache".to_string()]);
    }

    /// Loopback server playing hive-board, ChromaDB and Ollama at once. Every connection is
    /// closed after one response, so each request arrives whole on its own connection. Returns
    /// the base URL and a handle yielding every raw request (request line, headers and body).
    fn recall_mock() -> (String, std::thread::JoinHandle<Vec<String>>) {
        use std::io::{Read, Write};
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        l.set_nonblocking(true).unwrap();
        let h = std::thread::spawn(move || {
            let mut seen = Vec::new();
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while std::time::Instant::now() < deadline && seen.len() < 4 {
                let Ok((mut s, _)) = l.accept() else {
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                };
                s.set_nonblocking(false).unwrap();
                s.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                // Read the headers, then exactly content-length bytes of body.
                loop {
                    let n = s.read(&mut chunk).unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    let text = String::from_utf8_lossy(&buf).to_string();
                    if let Some(end) = text.find("\r\n\r\n") {
                        let len = text[..end]
                            .lines()
                            .find_map(|l| {
                                let (k, v) = l.split_once(':')?;
                                k.eq_ignore_ascii_case("content-length")
                                    .then(|| v.trim().parse::<usize>().ok())?
                            })
                            .unwrap_or(0);
                        if buf.len() >= end + 4 + len {
                            break;
                        }
                    }
                }
                let req = String::from_utf8_lossy(&buf).to_string();
                let line = req.lines().next().unwrap_or("").to_string();
                let body = if line.contains("/collections/c1/query") {
                    r#"{"documents":[[]],"metadatas":[[]],"distances":[[]]}"#
                } else if line.contains("/collections") {
                    r#"[{"name":"hive-findings","id":"c1"}]"#
                } else if line.contains("/embeddings") {
                    r#"{"embedding":[0.1,0.2]}"#
                } else {
                    "{}"
                };
                let resp = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = s.write_all(resp.as_bytes());
                seen.push(req);
            }
            seen
        });
        (url, h)
    }

    #[tokio::test]
    async fn secrets_in_a_brief_query_never_reach_any_source() {
        let (url, h) = recall_mock();
        let hive = Hive::new(&url, None).unwrap();
        // Built at runtime so no scanner sees a credential-shaped literal.
        let token = ["ghp", "_", "abcdefghijklmnopqrstuvwxyz0123"].concat();
        let dsn = ["postgres://app:", "s3cr3tpw9", "@db.example.test/x"].concat();
        let kv = ["pass", "word=", "hunter2hunter2"].concat();
        let query = format!("Deploy title {token}\nDescription: connect {dsn} with {kv}");
        let sources = Sources {
            neo4j: None,
            topology: None,
            chroma: nuvek_chroma::ChromaConfig {
                chroma_base: url.clone(),
                ollama_base: url.clone(),
                ..Default::default()
            },
        };
        let out = gather_with(&hive, sources, &query, std::slice::from_ref(&kv), None).await;
        let reqs = h.join().unwrap();
        assert!(
            reqs.iter().any(|r| r.contains("/embeddings")),
            "the embedding endpoint was called: {reqs:?} / {out}"
        );
        assert!(reqs.iter().any(|r| r.contains("/api/brief")), "{reqs:?}");
        for r in &reqs {
            for secret in [
                "abcdefghijklmnopqrstuvwxyz0123",
                "s3cr3tpw9",
                "hunter2hunter2",
            ] {
                assert!(!r.contains(secret), "{secret} leaked in {r}");
            }
        }
        let embed = reqs.iter().find(|r| r.contains("/embeddings")).unwrap();
        assert!(embed.contains("Deploy title"), "{embed}");
        assert!(embed.contains("REDACTED"), "{embed}");
    }
}
