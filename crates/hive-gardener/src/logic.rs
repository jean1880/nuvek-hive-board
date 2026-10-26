//! Pure logic: slugs, incident keys, subject resolution, cluster thresholds, fingerprints and
//! extraction parsing. No network, no filesystem, no environment; everything here is unit-tested.

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeSet, HashMap, HashSet};

/// Char-safe truncation (never byte-indexes).
pub fn truncate_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// A JSON array of strings as a `Vec<String>` (non-strings skipped, anything else empty).
pub fn str_list(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Lowercase ASCII slug: runs of anything else collapse to one `-`, no leading/trailing `-`.
pub fn slugify(s: &str) -> String {
    let mut out = String::new();
    let mut last_dash = true;
    for c in s.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() {
            out.push(c);
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

/// `^[a-z][a-z0-9-]{1,79}$` (the hive-board topic-name contract), without a regex dependency.
pub fn is_valid_topic_name(name: &str) -> bool {
    let n = name.chars().count();
    let mut chars = name.chars();
    let first_ok = chars.next().is_some_and(|c| c.is_ascii_lowercase());
    first_ok
        && (2..=80).contains(&n)
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

fn incident_name(kind: &str, subject: &str) -> String {
    let k = slugify(kind);
    let s = slugify(subject);
    let raw = format!(
        "incident-{}-{}",
        if k.is_empty() { "unknown" } else { &k },
        if s.is_empty() { "unknown" } else { &s }
    );
    let mut cut = truncate_chars(&raw, 80);
    while cut.ends_with('-') {
        cut.pop();
    }
    cut
}

/// The topic a failing run belongs to: `incident-<kind>-<target-or-host>`. A run with exactly
/// one target is keyed by that target; anything else (none, or a fan-out) by the host.
pub fn incident_topic_name(kind: &str, targets: &[String], host: &str) -> String {
    match targets {
        [only] => incident_name(kind, only),
        _ => incident_name(kind, host),
    }
}

/// Every topic name a SUCCEEDED run could be recovering: the host key plus one per target.
pub fn recovery_candidate_names(kind: &str, targets: &[String], host: &str) -> Vec<String> {
    let mut v = vec![incident_name(kind, host)];
    for t in targets.iter().take(60) {
        let n = incident_name(kind, t);
        if !v.contains(&n) {
            v.push(n);
        }
    }
    v
}

/// Kinds whose failure needs a person now (deploys, rollbacks, quarantines).
pub fn is_critical_kind(kind: &str) -> bool {
    let k = kind.to_lowercase();
    ["deploy", "rollback", "quarantine"]
        .iter()
        .any(|w| k.contains(w))
}

// ---------------------------------------------------------------------------------------------
// Failure rotation

/// Per-topic last-failure times (epoch seconds), persisted by the caller in hive `kv`.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct FailLog {
    map: std::collections::BTreeMap<String, i64>,
    dirty: bool,
}

impl FailLog {
    pub fn from_json(v: &Value) -> Self {
        let map = v
            .as_object()
            .map(|o| {
                o.iter()
                    .filter_map(|(k, t)| Some((k.clone(), t.as_i64()?)))
                    .collect()
            })
            .unwrap_or_default();
        Self { map, dirty: false }
    }

    pub fn to_json(&self) -> Value {
        Value::Object(
            self.map
                .iter()
                .map(|(k, t)| (k.clone(), json!(t)))
                .collect(),
        )
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    pub fn recently_failed(&self, name: &str, now: i64, window_s: i64) -> bool {
        self.map.get(name).is_some_and(|t| now - t < window_s)
    }

    pub fn fail(&mut self, name: &str, now: i64) {
        self.map.insert(name.to_string(), now);
        self.dirty = true;
    }

    pub fn ok(&mut self, name: &str) {
        if self.map.remove(name).is_some() {
            self.dirty = true;
        }
    }

    /// Forget records older than `keep_s`.
    pub fn prune(&mut self, now: i64, keep_s: i64) {
        let before = self.map.len();
        self.map.retain(|_, t| now - *t < keep_s);
        if self.map.len() != before {
            self.dirty = true;
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Subject resolution

/// A graph entity a subject can resolve to.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Entity {
    pub label: String,
    pub name: String,
}

/// Case/dash-normalised form: lowercase, `_`/`.`/space become `-`, runs collapse.
pub fn normalise_name(s: &str) -> String {
    let mut out = String::new();
    let mut last_dash = true;
    for c in s.trim().chars().flat_map(char::to_lowercase) {
        let c = if matches!(c, '_' | '.' | ' ' | '/') {
            '-'
        } else {
            c
        };
        if c == '-' {
            if !last_dash {
                out.push('-');
            }
            last_dash = true;
        } else {
            out.push(c);
            last_dash = false;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

/// The live allowlist of resolvable entities. Fuzzy matching is deliberately absent (no
/// `strsim` in the workspace); upgrade path: add it behind a high threshold in `resolve`.
#[derive(Debug, Default)]
pub struct Allowlist {
    exact: HashMap<String, Vec<Entity>>,
    normalised: HashMap<String, Vec<Entity>>,
}

impl Allowlist {
    pub fn new(entries: impl IntoIterator<Item = Entity>) -> Self {
        let mut a = Allowlist::default();
        for e in entries {
            a.exact.entry(e.name.clone()).or_default().push(e.clone());
            a.normalised
                .entry(normalise_name(&e.name))
                .or_default()
                .push(e);
        }
        a
    }

    pub fn len(&self) -> usize {
        self.exact.values().map(Vec::len).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.exact.is_empty()
    }

    /// Exact name first, then normalised. Every entity sharing the matched name is returned
    /// (a repository and its container are both real subjects).
    pub fn resolve(&self, candidate: &str) -> Vec<Entity> {
        let c = candidate.trim();
        if c.is_empty() {
            return Vec::new();
        }
        if let Some(v) = self.exact.get(c) {
            return v.clone();
        }
        self.normalised
            .get(&normalise_name(c))
            .cloned()
            .unwrap_or_default()
    }

    /// Resolve a list of candidates: (resolved entities, deduped), (unresolved texts, deduped).
    pub fn resolve_all(&self, subjects: &[String]) -> (Vec<Entity>, Vec<String>) {
        let mut resolved: Vec<Entity> = Vec::new();
        let mut unresolved: Vec<String> = Vec::new();
        for s in subjects {
            let hits = self.resolve(s);
            if hits.is_empty() {
                let t = s.trim().to_string();
                if !t.is_empty() && !unresolved.contains(&t) {
                    unresolved.push(t);
                }
            }
            for h in hits {
                if !resolved.contains(&h) {
                    resolved.push(h);
                }
            }
        }
        (resolved, unresolved)
    }
}

pub fn entities_json(v: &[Entity]) -> Value {
    Value::Array(
        v.iter()
            .map(|e| json!({"label": e.label, "name": e.name}))
            .collect(),
    )
}

pub fn entities_from_json(v: &Value) -> Vec<Entity> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|e| {
                    Some(Entity {
                        label: e.get("label")?.as_str()?.to_string(),
                        name: e.get("name")?.as_str()?.to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------------------------
// Proposals: gotcha clusters

pub const MIN_CLUSTER_TOPICS: usize = 3;
pub const MIN_CLUSTER_IDENTITIES: usize = 2;

#[derive(Debug, Clone)]
pub struct FindingRef {
    pub id: i64,
    pub topic: String,
    pub identity_id: Option<i64>,
    pub status: String,
    pub content_hash: String,
    pub claim: String,
    pub reinforce: i64,
    pub subjects: Vec<String>,
}

#[derive(Debug)]
pub struct Cluster {
    pub hash: String,
    pub findings: Vec<FindingRef>,
}

/// Gotcha clusters (same `content_hash` across topics) that clear the promotion bar:
/// at least 3 distinct topics AND 2 distinct known identities AND 1 confirmed finding.
/// False positives, superseded and expired findings never count.
pub fn qualifying_clusters(findings: &[FindingRef]) -> Vec<Cluster> {
    let mut by_hash: HashMap<&str, Vec<&FindingRef>> = HashMap::new();
    for f in findings {
        if matches!(
            f.status.as_str(),
            "false_positive" | "superseded" | "expired"
        ) {
            continue;
        }
        by_hash.entry(&f.content_hash).or_default().push(f);
    }
    let mut out: Vec<Cluster> = by_hash
        .into_iter()
        .filter(|(_, fs)| {
            let topics: HashSet<&str> = fs.iter().map(|f| f.topic.as_str()).collect();
            let idents: HashSet<i64> = fs.iter().filter_map(|f| f.identity_id).collect();
            let confirmed = fs
                .iter()
                .any(|f| f.status == "confirmed" || f.status == "fixed");
            topics.len() >= MIN_CLUSTER_TOPICS
                && idents.len() >= MIN_CLUSTER_IDENTITIES
                && confirmed
        })
        .map(|(h, fs)| {
            let mut findings: Vec<FindingRef> = fs.into_iter().cloned().collect();
            findings.sort_by_key(|f| f.id);
            Cluster {
                hash: h.to_string(),
                findings,
            }
        })
        .collect();
    out.sort_by(|a, b| a.hash.cmp(&b.hash));
    out
}

/// Proposal fingerprint: sha256 over the sorted, de-duplicated finding hashes. It is
/// order-independent and does NOT change when more findings join the same cluster, so a
/// growing cluster keeps answering 409 (already proposed) instead of re-proposing.
pub fn fingerprint(hashes: &[String]) -> String {
    let set: BTreeSet<&str> = hashes.iter().map(String::as_str).collect();
    let mut h = Sha256::new();
    h.update(b"gotcha-note");
    for s in set {
        h.update(b"\n");
        h.update(s.as_bytes());
    }
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

// ---------------------------------------------------------------------------------------------
// Extraction

pub const FINDING_KINDS: [&str; 5] = ["finding", "decision", "gotcha", "verdict", "question"];
pub const SEVERITIES: [&str; 3] = ["BLOCKER", "SHOULD", "NICE"];
pub const MAX_CLAIM_CHARS: usize = 300;
pub const MAX_SUMMARY_CHARS: usize = 1200;
const MAX_EVIDENCE_CHARS: usize = 500;
const MAX_SUBJECTS: usize = 10;

/// JSON schema handed to the model (grammar-constrained on the direct path, quoted in the
/// prompt on the gateway path).
pub fn extraction_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "findings": { "type": "array", "items": {
                "type": "object",
                "properties": {
                    "kind": { "type": "string", "enum": FINDING_KINDS },
                    "claim": { "type": "string" },
                    "severity": { "type": ["string", "null"], "enum": ["BLOCKER", "SHOULD", "NICE", null] },
                    "subjects": { "type": "array", "items": { "type": "string" } },
                    "evidence": { "type": "string" },
                    "confidence": { "type": "number" },
                    "source_message_ids": { "type": "array", "items": { "type": "integer" } }
                },
                "required": ["kind", "claim", "subjects", "confidence", "source_message_ids"]
            }},
            "summary": { "type": "string" }
        },
        "required": ["findings", "summary"]
    })
}

#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub kind: String,
    pub claim: String,
    pub severity: Option<String>,
    pub subjects: Vec<String>,
    pub evidence: String,
    pub confidence: f64,
    pub source_message_ids: Vec<i64>,
}

#[derive(Debug, Default, PartialEq)]
pub struct Parsed {
    pub findings: Vec<Candidate>,
    pub summary: String,
    /// Items that failed schema validation (counted, never stored).
    pub invalid: usize,
}

/// Pull the JSON object out of a model answer that may wrap it in prose or a code fence.
pub fn extract_json_object(raw: &str) -> Option<&str> {
    let start = raw.find('{')?;
    let end = raw.rfind('}')?;
    (end > start).then(|| &raw[start..=end])
}

/// Parse and validate a model answer. `valid_ids` are the message ids that were in the prompt;
/// citations of anything else are dropped (a model cannot cite a message it never saw).
pub fn parse_extraction(raw: &str, valid_ids: &HashSet<i64>) -> Result<Parsed, String> {
    let body = extract_json_object(raw).ok_or("no JSON object in model output")?;
    let data: Value = serde_json::from_str(body).map_err(|e| format!("invalid JSON: {e}"))?;
    let items = data
        .get("findings")
        .and_then(Value::as_array)
        .ok_or("missing findings array")?;
    let mut out = Parsed {
        summary: truncate_chars(
            data.get("summary")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim(),
            MAX_SUMMARY_CHARS,
        ),
        ..Parsed::default()
    };
    for it in items {
        match candidate_from(it, valid_ids) {
            Some(c) => out.findings.push(c),
            None => out.invalid += 1,
        }
    }
    Ok(out)
}

fn candidate_from(it: &Value, valid_ids: &HashSet<i64>) -> Option<Candidate> {
    let kind = it.get("kind")?.as_str()?.trim().to_lowercase();
    if !FINDING_KINDS.contains(&kind.as_str()) {
        return None;
    }
    let claim = truncate_chars(it.get("claim")?.as_str()?.trim(), MAX_CLAIM_CHARS);
    if claim.is_empty() {
        return None;
    }
    let severity = it
        .get("severity")
        .and_then(Value::as_str)
        .map(|s| s.trim().to_uppercase())
        .filter(|s| SEVERITIES.contains(&s.as_str()));
    let mut subjects: Vec<String> = Vec::new();
    for s in it
        .get("subjects")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        let t = truncate_chars(s.trim(), 80);
        if !t.is_empty() && !subjects.contains(&t) && subjects.len() < MAX_SUBJECTS {
            subjects.push(t);
        }
    }
    let evidence = truncate_chars(
        it.get("evidence")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim(),
        MAX_EVIDENCE_CHARS,
    );
    let confidence = it
        .get("confidence")
        .and_then(Value::as_f64)
        .unwrap_or(0.5)
        .clamp(0.0, 1.0);
    let source_message_ids: Vec<i64> = it
        .get("source_message_ids")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_i64)
        .filter(|i| valid_ids.contains(i))
        .collect();
    Some(Candidate {
        kind,
        claim,
        severity,
        subjects,
        evidence,
        confidence,
        source_message_ids,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ent(label: &str, name: &str) -> Entity {
        Entity {
            label: label.into(),
            name: name.into(),
        }
    }

    #[test]
    fn slugify_collapses_and_is_char_safe() {
        assert_eq!(slugify("Deploy.Auto"), "deploy-auto");
        assert_eq!(slugify("  --a__b--  "), "a-b");
        assert_eq!(slugify("héllo wörld€"), "h-llo-w-rld");
        assert_eq!(slugify("€€"), "");
    }

    #[test]
    fn incident_names_follow_the_topic_contract() {
        let n = incident_topic_name("deploy.auto", &["Media".into()], "nas");
        assert_eq!(n, "incident-deploy-auto-media");
        assert!(is_valid_topic_name(&n));
        // fan-out and no-target runs key on the host
        let two = ["a".to_string(), "b".to_string()];
        assert_eq!(
            incident_topic_name("deploy.auto", &two, "NAS"),
            "incident-deploy-auto-nas"
        );
        assert_eq!(incident_topic_name("x", &[], "nas"), "incident-x-nas");
        // junk and overlong input still yields a valid name
        let long = "€".repeat(50) + &"a".repeat(200);
        let n = incident_topic_name("k", &[long], "h");
        assert!(is_valid_topic_name(&n), "{n}");
        assert!(n.chars().count() <= 80);
        assert!(is_valid_topic_name(&incident_topic_name(
            "€",
            &["€".into()],
            "€"
        )));
    }

    #[test]
    fn recovery_candidates_cover_host_and_targets() {
        let c = recovery_candidate_names(
            "deploy.auto",
            &["cache".into(), "cache".into(), "web-app".into()],
            "nas",
        );
        assert_eq!(
            c,
            vec![
                "incident-deploy-auto-nas",
                "incident-deploy-auto-cache",
                "incident-deploy-auto-web-app"
            ]
        );
    }

    #[test]
    fn critical_kinds() {
        assert!(is_critical_kind("deploy.auto"));
        assert!(is_critical_kind("Rollback"));
        assert!(!is_critical_kind("media-sweep"));
    }

    #[test]
    fn topic_name_validation() {
        assert!(is_valid_topic_name("ab"));
        assert!(!is_valid_topic_name("a"));
        assert!(!is_valid_topic_name("1abc"));
        assert!(!is_valid_topic_name("Abc"));
        assert!(!is_valid_topic_name(&"a".repeat(81)));
        assert!(is_valid_topic_name(&"a".repeat(80)));
    }

    #[test]
    fn resolution_exact_then_normalised_and_never_invents() {
        let a = Allowlist::new([
            ent("Repository", "hive-board"),
            ent("Container", "hive-board"),
            ent("Skill", "rust-craftsman"),
            ent("Package", "nuvek_core"),
        ]);
        // exact returns every entity of that name
        assert_eq!(a.resolve("hive-board").len(), 2);
        // case / separator normalised
        assert_eq!(
            a.resolve("Rust_Craftsman"),
            vec![ent("Skill", "rust-craftsman")]
        );
        assert_eq!(a.resolve("nuvek-core"), vec![ent("Package", "nuvek_core")]);
        assert_eq!(a.resolve(" HIVE.board "), a.resolve("hive-board"));
        // unknown stays unresolved
        let (r, u) = a.resolve_all(&[
            "hive-board".into(),
            "made-up".into(),
            "made-up".into(),
            " ".into(),
        ]);
        assert_eq!(r.len(), 2);
        assert_eq!(u, vec!["made-up".to_string()]);
        assert!(a.resolve("hive").is_empty(), "no fuzzy/prefix matching");
    }

    fn fr(id: i64, topic: &str, ident: Option<i64>, status: &str, hash: &str) -> FindingRef {
        FindingRef {
            id,
            topic: topic.into(),
            identity_id: ident,
            status: status.into(),
            content_hash: hash.into(),
            claim: "c".into(),
            reinforce: 1,
            subjects: vec![],
        }
    }

    #[test]
    fn cluster_threshold_needs_topics_identities_and_confirmation() {
        let ok = vec![
            fr(1, "t1", Some(1), "confirmed", "h"),
            fr(2, "t2", Some(2), "open", "h"),
            fr(3, "t3", Some(2), "open", "h"),
        ];
        assert_eq!(qualifying_clusters(&ok).len(), 1);
        // only 2 topics
        assert!(qualifying_clusters(&ok[..2]).is_empty());
        // one identity
        let one_ident = vec![
            fr(1, "t1", Some(1), "confirmed", "h"),
            fr(2, "t2", Some(1), "open", "h"),
            fr(3, "t3", Some(1), "open", "h"),
        ];
        assert!(qualifying_clusters(&one_ident).is_empty());
        // nothing confirmed
        let none = vec![
            fr(1, "t1", Some(1), "open", "h"),
            fr(2, "t2", Some(2), "open", "h"),
            fr(3, "t3", Some(3), "open", "h"),
        ];
        assert!(qualifying_clusters(&none).is_empty());
        // a false positive does not count toward the topics
        let fp = vec![
            fr(1, "t1", Some(1), "confirmed", "h"),
            fr(2, "t2", Some(2), "open", "h"),
            fr(3, "t3", Some(3), "false_positive", "h"),
        ];
        assert!(qualifying_clusters(&fp).is_empty());
        // unknown identities never satisfy the identity bar
        let unknown = vec![
            fr(1, "t1", None, "confirmed", "h"),
            fr(2, "t2", None, "open", "h"),
            fr(3, "t3", Some(1), "open", "h"),
        ];
        assert!(qualifying_clusters(&unknown).is_empty());
    }

    #[test]
    fn fingerprint_is_stable_order_independent_and_growth_stable() {
        let a = fingerprint(&["b".into(), "a".into()]);
        assert_eq!(a, fingerprint(&["a".into(), "b".into(), "a".into()]));
        assert_ne!(a, fingerprint(&["a".into()]));
        assert_eq!(a.len(), 64);
        // a cluster with 3 vs 5 findings of one hash proposes once
        let h = "deadbeef".to_string();
        assert_eq!(
            fingerprint(&vec![h.clone(); 3]),
            fingerprint(&vec![h.clone(); 5])
        );
    }

    #[test]
    fn extraction_parses_fenced_json_and_validates() {
        let ids: HashSet<i64> = [10, 11].into_iter().collect();
        let raw = "Sure!\n```json\n{\"findings\":[\
            {\"kind\":\"Gotcha\",\"claim\":\"  X breaks Y  \",\"severity\":\"should\",\"subjects\":[\"a\",\"a\",\" \"],\"evidence\":\"e\",\"confidence\":7,\"source_message_ids\":[10,99]},\
            {\"kind\":\"rumour\",\"claim\":\"bad kind\",\"subjects\":[],\"confidence\":0.5,\"source_message_ids\":[]},\
            {\"kind\":\"finding\",\"claim\":\"   \",\"subjects\":[],\"confidence\":0.5,\"source_message_ids\":[]}\
            ],\"summary\":\"done\"}\n```";
        let p = parse_extraction(raw, &ids).unwrap();
        assert_eq!(p.invalid, 2);
        assert_eq!(p.findings.len(), 1);
        let f = &p.findings[0];
        assert_eq!(f.kind, "gotcha");
        assert_eq!(f.claim, "X breaks Y");
        assert_eq!(f.severity.as_deref(), Some("SHOULD"));
        assert_eq!(f.subjects, vec!["a".to_string()]);
        assert_eq!(f.confidence, 1.0);
        assert_eq!(f.source_message_ids, vec![10]);
        assert_eq!(p.summary, "done");
    }

    #[test]
    fn extraction_truncates_by_chars_and_rejects_garbage() {
        let ids = HashSet::new();
        let claim = "€".repeat(400);
        let raw = format!(
            "{{\"findings\":[{{\"kind\":\"finding\",\"claim\":\"{claim}\",\"subjects\":[],\"confidence\":0.4,\"source_message_ids\":[]}}],\"summary\":\"{}\"}}",
            "é".repeat(2000)
        );
        let p = parse_extraction(&raw, &ids).unwrap();
        assert_eq!(p.findings[0].claim.chars().count(), MAX_CLAIM_CHARS);
        assert_eq!(p.summary.chars().count(), MAX_SUMMARY_CHARS);
        assert!(parse_extraction("no json here", &ids).is_err());
        assert!(parse_extraction("{\"summary\":\"x\"}", &ids).is_err());
        assert!(parse_extraction("{ not json }", &ids).is_err());
    }

    #[test]
    fn entity_json_roundtrip() {
        let v = vec![ent("Repository", "x")];
        assert_eq!(entities_from_json(&entities_json(&v)), v);
        assert!(entities_from_json(&Value::Null).is_empty());
    }

    #[test]
    fn fail_log_skips_recent_failures_and_forgets_old_ones() {
        let mut f = FailLog::from_json(&json!({"a": 100, "b": 5, "bad": "x"}));
        assert!(!f.is_dirty());
        assert!(f.recently_failed("a", 150, 100));
        assert!(!f.recently_failed("a", 250, 100), "window elapsed");
        assert!(!f.recently_failed("zzz", 150, 100));
        f.prune(150, 100);
        assert!(f.is_dirty(), "b was pruned");
        assert!(!f.recently_failed("b", 6, 100));
        f.ok("a");
        assert!(!f.recently_failed("a", 150, 100));
        f.fail("c", 200);
        assert_eq!(f.to_json(), json!({"c": 200}));
        let mut clean = FailLog::default();
        clean.ok("nothing");
        assert!(!clean.is_dirty());
    }

    #[test]
    fn str_list_keeps_only_strings() {
        assert_eq!(str_list(&json!(["a", 1, null, "b"])), vec!["a", "b"]);
        assert!(str_list(&json!({"a": 1})).is_empty());
        assert!(str_list(&Value::Null).is_empty());
    }
}
