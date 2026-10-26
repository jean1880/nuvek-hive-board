//! The small stages: `sweep`, `proposals` and `cost`.

use crate::logic::{fingerprint, qualifying_clusters, str_list, truncate_chars, FindingRef};
use crate::{Ctx, Outcome};
use anyhow::{anyhow, Result};
use nuvek_core::scrub::gitleaks_clean;
use reqwest::Method;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::time::Duration;

// ---------------------------------------------------------------------------------------------
// sweep

/// Ask the server to apply the lifecycle transitions (quiet, auto-resolve, archive, compact).
/// The endpoint has its own `dry_run`, so `--dry-run` still calls it and prints the plan.
pub async fn sweep(ctx: &Ctx) -> Result<Outcome> {
    let body = json!({"dry_run": ctx.hive.is_dry(), "actor": "gardener"});
    let r = ctx
        .hive
        .send_live(Method::POST, "/api/maintenance/sweep", &body)
        .await?;
    let changes = r
        .get("changes")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut by_move: BTreeMap<String, i64> = BTreeMap::new();
    for c in &changes {
        let key = format!(
            "{}->{}",
            c.get("from").and_then(Value::as_str).unwrap_or("?"),
            c.get("to").and_then(Value::as_str).unwrap_or("?")
        );
        *by_move.entry(key).or_default() += 1;
        if ctx.hive.is_dry() {
            ctx.log.info(&format!("DRY-RUN sweep would change {c}"));
        }
    }
    Ok(Outcome {
        summary: format!("changes={} {by_move:?}", changes.len()),
        swept: changes.len() as i64,
        ..Default::default()
    })
}

// ---------------------------------------------------------------------------------------------
// proposals

fn refs(findings: &[Value]) -> Vec<FindingRef> {
    findings
        .iter()
        .filter_map(|f| {
            Some(FindingRef {
                id: f.get("id")?.as_i64()?,
                topic: f.get("topic_name")?.as_str()?.to_string(),
                identity_id: f.get("identity_id").and_then(Value::as_i64),
                status: f.get("status")?.as_str()?.to_string(),
                content_hash: f.get("content_hash")?.as_str()?.to_string(),
                claim: f.get("claim")?.as_str()?.to_string(),
                reinforce: f
                    .get("reinforce_count")
                    .and_then(Value::as_i64)
                    .unwrap_or(1),
                subjects: str_list(f.get("subjects").unwrap_or(&Value::Null)),
            })
        })
        .collect()
}

/// Cluster gotchas across topics and propose a `gotcha-note` for each cluster that clears the
/// bar. Proposals are only ever PROPOSED here; a human approves in the UI.
pub async fn proposals(ctx: &Ctx) -> Result<Outcome> {
    let v = ctx.hive.get("/api/findings?kind=gotcha&limit=500").await?;
    let findings = refs(v.as_array().map(Vec::as_slice).unwrap_or(&[]));
    let clusters = qualifying_clusters(&findings);
    let (mut proposed, mut known, mut skipped) = (0i64, 0i64, 0i64);
    for c in &clusters {
        let lead = c
            .findings
            .iter()
            .max_by_key(|f| (f.reinforce, std::cmp::Reverse(f.id)))
            .ok_or_else(|| anyhow!("empty cluster"))?;
        let mut topics: Vec<&str> = c.findings.iter().map(|f| f.topic.as_str()).collect();
        topics.sort_unstable();
        topics.dedup();
        let ids: Vec<i64> = c.findings.iter().map(|f| f.id).collect();
        let body_text = format!(
            "Recurring gotcha, seen in {} topics ({}); at least one finding is confirmed.\n\n{}\n\nFindings: {}",
            topics.len(),
            topics.join(", "),
            lead.claim,
            ids.iter().map(|i| format!("#{i}")).collect::<Vec<_>>().join(" ")
        );
        let title = format!("Gotcha: {}", truncate_chars(&lead.claim, 90));
        if !gitleaks_clean(&format!("{title}\n{body_text}")) {
            skipped += 1;
            ctx.log.warn("proposals: cluster skipped (secret scan)");
            continue;
        }
        let body = json!({
            "kind": "gotcha-note",
            "title": title,
            "body": body_text,
            "target": lead.subjects.first(),
            "evidence_finding_ids": ids,
            "fingerprint": fingerprint(std::slice::from_ref(&c.hash)),
        });
        match ctx
            .hive
            .write(&ctx.log, Method::POST, "/api/proposals", &body, None)
            .await
        {
            Ok(_) => proposed += 1,
            Err(e) if e.code() == Some(409) => known += 1,
            Err(e) => return Err(anyhow!("proposal POST: {e}")),
        }
    }
    Ok(Outcome {
        summary: format!(
            "gotchas={} clusters={} proposed={proposed} already_known={known} skipped={skipped}",
            findings.len(),
            clusters.len()
        ),
        ..Default::default()
    })
}

// ---------------------------------------------------------------------------------------------
// cost

/// Labels that name the agent (subagent type) on a metric, best first.
pub const AGENT_LABELS: [&str; 4] = ["agent_type", "subagent_type", "agent_name", "agent"];
const MODEL_LABEL: &str = "model";
const PREFERRED_METRIC: &str = "claude_code_token_usage_tokens_total";

/// Pick the token metric from the `__name__` values: the known name, else the first `claude*`
/// name mentioning tokens.
pub fn pick_metric(names: &[String]) -> Option<String> {
    if names.iter().any(|n| n == PREFERRED_METRIC) {
        return Some(PREFERRED_METRIC.to_string());
    }
    names
        .iter()
        .find(|n| n.contains("claude") && n.contains("token"))
        .cloned()
}

/// The labels to group by: at most one agent label plus the model label, those that exist.
pub fn identity_labels(labels: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(a) = AGENT_LABELS
        .iter()
        .find(|l| labels.iter().any(|x| x == **l))
    {
        out.push((*a).to_string());
    }
    if labels.iter().any(|x| x == MODEL_LABEL) {
        out.push(MODEL_LABEL.to_string());
    }
    out
}

/// PromQL: tokens per identity over 7 days. When a `type` label exists only input/output count
/// (cache reads would swamp the figure and are not what a finding costs).
pub fn cost_query(metric: &str, group: &[String], has_type: bool) -> String {
    let sel = if has_type {
        format!("{metric}{{type=~\"input|output\"}}")
    } else {
        metric.to_string()
    };
    format!("sum by ({}) (increase({sel}[7d]))", group.join(","))
}

async fn prom_get(
    http: &reqwest::Client,
    base: &str,
    path: &str,
    query: &[(&str, &str)],
) -> Result<Value> {
    let v: Value = http
        .get(format!("{base}{path}"))
        .query(query)
        .send()
        .await
        .map_err(|e| anyhow!("prometheus unreachable: {}", e.without_url()))?
        .error_for_status()
        .map_err(|e| anyhow!("prometheus: {}", e.without_url()))?
        .json()
        .await?;
    Ok(v.get("data").cloned().unwrap_or(Value::Null))
}

/// Entries for kv `cost.by_identity` from a Prometheus instant-query result.
pub fn entries_from_result(result: &Value, group: &[String]) -> Vec<Value> {
    let agent_label = group.iter().find(|g| g.as_str() != MODEL_LABEL);
    result
        .as_array()
        .map(|rows| {
            rows.iter()
                .filter_map(|r| {
                    let m = r.get("metric")?;
                    let tokens: f64 = r.get("value")?.get(1)?.as_str()?.parse().ok()?;
                    if !tokens.is_finite() {
                        return None;
                    }
                    let get = |l: &str| {
                        m.get(l)
                            .and_then(Value::as_str)
                            .filter(|s| !s.is_empty())
                            .unwrap_or("unknown")
                            .to_string()
                    };
                    Some(json!({
                        "agent_type": agent_label.map(|l| get(l)).unwrap_or_else(|| "unknown".into()),
                        "model": if group.iter().any(|g| g == MODEL_LABEL) { get(MODEL_LABEL) } else { "unknown".into() },
                        "harness": "claude",
                        "tokens_7d": tokens.round() as i64,
                    }))
                })
                .collect()
        })
        .unwrap_or_default()
}

type Found = (String, Vec<String>, Vec<Value>);

/// `Err(reason)` is a recorded gap (no metric, no usable label, Prometheus down), not a failure.
async fn discover_and_query(http: &reqwest::Client, base: &str) -> Result<Found, String> {
    let names = prom_get(http, base, "/api/v1/label/__name__/values", &[])
        .await
        .map_err(|e| e.to_string())?;
    let names: Vec<String> = str_list(&names)
        .into_iter()
        .filter(|n| n.contains("claude"))
        .collect();
    let metric = pick_metric(&names)
        .ok_or_else(|| format!("no claude token metric among {} claude* names", names.len()))?;
    let sel = format!("{{__name__=\"{metric}\"}}");
    let labels = prom_get(http, base, "/api/v1/labels", &[("match[]", sel.as_str())])
        .await
        .map_err(|e| e.to_string())?;
    let labels = str_list(&labels);
    let group = identity_labels(&labels);
    if group.is_empty() {
        return Err(format!(
            "{metric} has no model/agent label (labels: {})",
            labels.join(",")
        ));
    }
    let has_type = labels.iter().any(|l| l == "type");
    let q = cost_query(&metric, &group, has_type);
    let data = prom_get(http, base, "/api/v1/query", &[("query", q.as_str())])
        .await
        .map_err(|e| e.to_string())?;
    let entries = entries_from_result(data.get("result").unwrap_or(&Value::Null), &group);
    Ok((metric, group, entries))
}

/// Never fails the run for a missing metric or label: the gap is recorded in `cost.status`.
pub async fn cost(ctx: &Ctx) -> Result<Outcome> {
    let base = crate::endpoints::prometheus_url();
    let http = nuvek_core::http::client(Duration::from_secs(30))?;
    match discover_and_query(&http, &base).await {
        Ok((metric, group, entries)) => {
            let n = entries.len();
            let v = json!({
                "window": "7d",
                "metric": metric,
                "group_by": group,
                "generated_at": chrono::Utc::now().to_rfc3339(),
                "entries": entries,
            });
            ctx.hive.kv_put(&ctx.log, "cost.by_identity", &v).await?;
            Ok(Outcome {
                summary: format!("metric={metric} identities={n}"),
                ..Default::default()
            })
        }
        Err(reason) => {
            let v = json!({"available": false, "reason": truncate_chars(&reason, 300)});
            ctx.hive.kv_put(&ctx.log, "cost.status", &v).await?;
            Ok(Outcome {
                summary: format!("unavailable: {}", truncate_chars(&reason, 160)),
                ..Default::default()
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metric_is_picked_by_name() {
        let names = vec![
            "claude_code_session_count_total".to_string(),
            "claude_code_token_usage_tokens_total".to_string(),
        ];
        assert_eq!(
            pick_metric(&names).as_deref(),
            Some("claude_code_token_usage_tokens_total")
        );
        let alt = vec!["claude_tokens_used".to_string()];
        assert_eq!(pick_metric(&alt).as_deref(), Some("claude_tokens_used"));
        assert_eq!(pick_metric(&["claude_cost".to_string()]), None);
        assert_eq!(pick_metric(&[]), None);
    }

    #[test]
    fn labels_group_by_agent_and_model_when_present() {
        let l = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            identity_labels(&l(&["model", "type", "session_id"])),
            vec!["model"]
        );
        assert_eq!(
            identity_labels(&l(&["subagent_type", "model"])),
            vec!["subagent_type", "model"]
        );
        assert!(identity_labels(&l(&["type", "session_id"])).is_empty());
        assert_eq!(
            cost_query("m_total", &l(&["model"]), true),
            "sum by (model) (increase(m_total{type=~\"input|output\"}[7d]))"
        );
        assert_eq!(
            cost_query("m", &l(&["model"]), false),
            "sum by (model) (increase(m[7d]))"
        );
    }

    #[test]
    fn result_rows_become_identity_entries() {
        let res = json!([
            {"metric": {"subagent_type": "rust-reviewer", "model": "opus"}, "value": [1.0, "1234.6"]},
            {"metric": {"model": ""}, "value": [1.0, "NaN"]},
            {"metric": {"model": "sonnet"}, "value": [1.0, "10"]}
        ]);
        let group = vec!["subagent_type".to_string(), "model".to_string()];
        let e = entries_from_result(&res, &group);
        assert_eq!(e.len(), 2, "NaN row dropped");
        assert_eq!(e[0]["agent_type"], "rust-reviewer");
        assert_eq!(e[0]["model"], "opus");
        assert_eq!(e[0]["tokens_7d"], 1235);
        assert_eq!(e[0]["harness"], "claude");
        assert_eq!(e[1]["agent_type"], "unknown");
        // model-only grouping
        let m = entries_from_result(&res, &["model".to_string()]);
        assert_eq!(m[0]["agent_type"], "unknown");
    }

    #[test]
    fn proposal_rows_parse_and_skip_incomplete_findings() {
        let v = vec![
            json!({"id": 1, "topic_name": "t", "status": "open", "content_hash": "h", "claim": "c", "identity_id": 3, "reinforce_count": 2, "subjects": ["a"]}),
            json!({"id": 2, "topic_name": "t"}),
        ];
        let r = refs(&v);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].identity_id, Some(3));
        assert_eq!(r[0].subjects, vec!["a".to_string()]);
    }
}
