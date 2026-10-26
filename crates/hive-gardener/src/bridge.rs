//! `bridge` stage: failed automation runs -> `incident-<kind>-<target-or-host>` topics.
//!
//! Optional: the stage is skipped (and reports success) while `HIVE_GARDENER_BRIDGE_URL` is
//! unset. The source is any service answering `GET /api/runs?since=&limit=` with
//! `{"runs": [...]}` in the shape of [`Run`], such as a container-lifecycle dashboard.
//!
//! A PULL bridge (the source stays read-only and initiates nothing). The cursor lives in the
//! hive (`kv` key `bridge.cursor`), never on this machine. Every post carries an Idempotency-Key
//! derived from the run id, so re-reading the cursor boundary (the `since` filter is inclusive)
//! or catching up after downtime can never duplicate a message.
//!
//! A batch is COALESCED per incident key before anything is written (see [`plan`]): self-heal
//! runs flap fail -> succeed within seconds, and without coalescing every flap would open and
//! resolve a topic.
//! Only the key's FINAL state in the batch matters, and a topic opens only after
//! `GARDENER_BRIDGE_MIN_FAILURES` consecutive failures, or one failure left unanswered for
//! `GARDENER_BRIDGE_GRACE_MIN` minutes.
//!
//! Ceiling: one page of at most 500 runs per pass (the source's own cap); a longer outage
//! than that loses the oldest runs of the window. Upgrade path: page with `until`.

use crate::api::is_fresh;
use crate::logic::{
    incident_topic_name, is_critical_kind, is_valid_topic_name, recovery_candidate_names,
    truncate_chars,
};
use crate::Ctx;
use anyhow::{anyhow, Result};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use nuvek_core::scrub::{gitleaks_clean, redact};
use reqwest::Method;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};

const CURSOR_KEY: &str = "bridge.cursor";
const BRIDGE_AUTHOR: &str = "hive-gardener";
const RUN_LIMIT: usize = 500;
const MAX_SUBJECTS: usize = 20;

#[derive(Debug, Deserialize, Clone, Default, PartialEq)]
#[serde(default)]
pub struct Run {
    pub run_id: String,
    pub kind: String,
    pub host: String,
    pub title: String,
    pub dry_run: bool,
    pub status: String,
    pub started_at: String,
    pub error_count: u32,
    pub warn_count: u32,
    pub targets: Vec<String>,
}

const MAX_TARGETS: usize = 60;
const MAX_TARGET_CHARS: usize = 120;

impl Run {
    /// Every producer-controlled string is redacted and bounded BEFORE it is used for a topic
    /// name, title, subject or message, so a secret in a kind, host, target or title can never
    /// reach the board. Targets are capped at 120 chars each (and 60 per run).
    pub fn sanitized(mut self) -> Run {
        let clean = |s: &str, n: usize| truncate_chars(&redact(s), n);
        self.run_id = clean(&self.run_id, 80);
        self.kind = clean(&self.kind, 80);
        self.host = clean(&self.host, 80);
        self.title = clean(&self.title, 200);
        self.targets = self
            .targets
            .iter()
            .take(MAX_TARGETS)
            .map(|t| clean(t, MAX_TARGET_CHARS))
            .collect();
        self
    }
}

fn agent() -> Value {
    json!({"agent_type": "gardener", "model": "none", "harness": "system"})
}

/// A run target as a graph-resolvable subject: `c:queue-worker` (type prefix `c:` = container,
/// or any other single-letter prefix) becomes `queue-worker`. The raw target stays in the message
/// text; only `subjects` use this form.
pub fn subject_of(target: &str) -> String {
    let mut it = target.chars();
    match (it.next(), it.next()) {
        (Some(a), Some(':')) if a.is_ascii_alphabetic() && !it.as_str().is_empty() => {
            it.as_str().to_string()
        }
        _ => target.to_string(),
    }
}

/// Deduped subjects for the given runs' targets, capped.
fn subjects_of(runs: &[Run]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for t in runs.iter().flat_map(|r| r.targets.iter()) {
        let s = subject_of(t);
        if !out.contains(&s) {
            out.push(s);
        }
        if out.len() >= MAX_SUBJECTS {
            break;
        }
    }
    out
}

/// One-line description of a run. The whole line is redacted again as a belt-and-braces pass
/// over the format arguments (the fields are already sanitised).
fn describe(r: &Run) -> String {
    let targets = if r.targets.is_empty() {
        "no targets".to_string()
    } else {
        truncate_chars(&r.targets.join(", "), 200)
    };
    redact(&format!(
        "Run {} ({}) {}: {} [{} error(s), {} warning(s); {}]",
        r.run_id, r.kind, r.status, r.title, r.error_count, r.warn_count, targets
    ))
}

/// The message for a key's failures in one batch: one line for a lone failure, otherwise a count
/// with the first and last run ids.
fn batch_message(failures: &[Run]) -> String {
    match failures {
        [] => String::new(),
        [only] => describe(only),
        [first, .., last] => redact(&format!(
            "{} failures in this window: first run {} ({}), last run {} ({}). Latest: {}",
            failures.len(),
            first.run_id,
            first.started_at,
            last.run_id,
            last.started_at,
            describe(last)
        )),
    }
}

/// The text a run puts on the board (title + description + message): what the write-boundary
/// secret scan must pass.
fn board_text(failures: &[Run], last: &Run) -> String {
    format!(
        "{} failing: {}\n{}\n{}",
        last.kind,
        subject_label(last),
        batch_message(failures),
        incident_topic_name(&last.kind, &last.targets, &last.host)
    )
}

/// What a pass should do with one run, decided without any I/O.
#[derive(Debug, PartialEq)]
pub enum Action {
    Ignore,
    Failure,
    Success,
}

pub fn classify(r: &Run) -> Action {
    if r.dry_run {
        return Action::Ignore;
    }
    match r.status.as_str() {
        "failed" | "partial" => Action::Failure,
        "succeeded" => Action::Success,
        _ => Action::Ignore,
    }
}

/// Debounce settings.
#[derive(Debug, Clone, Copy)]
pub struct Cfg {
    /// Consecutive failures that open a topic.
    pub min_failures: usize,
    /// Minutes a lone failure must stay unanswered by a success before it opens a topic.
    pub grace_min: i64,
}

/// What to do for one incident key, after coalescing the whole batch.
#[derive(Debug, PartialEq)]
pub enum Plan {
    /// Final state is failure: create the topic (or append to the open one) with ONE message that
    /// summarises every failure of the key in the batch.
    Failure {
        key: String,
        failures: Vec<Run>,
        /// The topic is open already: the debounce gate does not apply.
        open: bool,
    },
    /// Final state is success and a topic is open: one "recovered" message, then resolve.
    Recover { key: String, run: Run },
    /// A failure that has not yet earned a topic; `since` is where the cursor must stay.
    Defer { key: String, since: String },
}

fn parse_ts(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

/// Coalesce `runs` (ascending by start time) per incident key and decide the one action each key
/// needs. Pure: `open` is the set of open incident topics, `now` the clock.
///
/// - final success + open topic -> `Recover`; final success + no topic -> nothing;
/// - final failure -> `Failure` (one summarising message), gated for a topic that is not open: it
///   needs `min_failures` consecutive failures at the end of the batch, or a last failure at
///   least `grace_min` minutes old (no success answered it); otherwise `Defer`.
///
/// A success run counts toward every key it could be recovering (its host key and one per
/// target) that is open or failed in this batch. Because each key yields at most one plan, a
/// dry run needs no extra state to match a real run.
pub fn plan(runs: &[Run], open: &HashSet<String>, cfg: &Cfg, now: DateTime<Utc>) -> Vec<Plan> {
    let fail_key = |r: &Run| incident_topic_name(&r.kind, &r.targets, &r.host);
    let fail_keys: HashSet<String> = runs
        .iter()
        .filter(|r| classify(r) == Action::Failure)
        .map(fail_key)
        .collect();

    let mut order: Vec<String> = Vec::new();
    let mut events: Events = HashMap::new();
    for r in runs {
        match classify(r) {
            Action::Ignore => {}
            Action::Failure => push_event(&mut order, &mut events, fail_key(r), true, r),
            Action::Success => {
                for cand in recovery_candidate_names(&r.kind, &r.targets, &r.host) {
                    if open.contains(&cand) || fail_keys.contains(&cand) {
                        push_event(&mut order, &mut events, cand, false, r);
                    }
                }
            }
        }
    }

    // Per-target keys whose final state in this batch is failure, as (kind, host): a host-keyed
    // topic of the same kind and host is not recovered while one of these is still failing.
    let still_failing: Vec<(&str, &str)> = order
        .iter()
        .filter_map(|k| {
            let &(failed, r) = events[k].last()?;
            (failed && r.targets.len() == 1).then_some((r.kind.as_str(), r.host.as_str()))
        })
        .collect();

    let mut out = Vec::new();
    for key in order {
        let list = &events[&key];
        let is_open = open.contains(&key);
        let Some(&(last_failed, last)) = list.last() else {
            continue;
        };
        if !last_failed {
            let host_keyed = key == incident_topic_name(&last.kind, &[], &last.host);
            let blocked = host_keyed
                && still_failing
                    .iter()
                    .any(|(k, h)| *k == last.kind && *h == last.host);
            if is_open && !blocked {
                out.push(Plan::Recover {
                    key,
                    run: last.clone(),
                });
            }
            continue;
        }
        let failures: Vec<Run> = list
            .iter()
            .filter(|(f, _)| *f)
            .map(|(_, r)| (*r).clone())
            .collect();
        let trailing = list.iter().rev().take_while(|(f, _)| *f).count();
        if !is_open {
            let aged = parse_ts(&last.started_at)
                // An unreadable timestamp must not hide an incident forever.
                .map(|t| now - t >= ChronoDuration::minutes(cfg.grace_min))
                .unwrap_or(true);
            if trailing < cfg.min_failures && !aged {
                let first_trailing = list
                    .iter()
                    .rev()
                    .take_while(|(f, _)| *f)
                    .last()
                    .map(|(_, r)| r.started_at.clone())
                    .unwrap_or_default();
                out.push(Plan::Defer {
                    key,
                    since: first_trailing,
                });
                continue;
            }
        }
        out.push(Plan::Failure {
            key,
            failures,
            open: is_open,
        });
    }
    out
}

/// Per key, the time-ordered events: (`true` = failure, the run).
type Events<'a> = HashMap<String, Vec<(bool, &'a Run)>>;

fn push_event<'a>(
    order: &mut Vec<String>,
    events: &mut Events<'a>,
    key: String,
    failed: bool,
    r: &'a Run,
) {
    if !events.contains_key(&key) {
        order.push(key.clone());
    }
    events.entry(key).or_default().push((failed, r));
}

/// Total order on timestamp strings: parsed instants (an unreadable one sorts first), then the
/// text. Comparing parsed values keeps `+00:00` / `Z` / sub-second spellings in true time order.
fn ts_key(s: &str) -> (Option<DateTime<Utc>>, &str) {
    (parse_ts(s), s)
}

/// Earliest of two timestamp strings by time, not by spelling.
pub fn earlier<'a>(a: &'a str, b: &'a str) -> &'a str {
    if ts_key(b) < ts_key(a) {
        b
    } else {
        a
    }
}

/// The new cursor: the newest started_at seen, but never past a run still in flight (it may fail
/// later) or a deferred failure (it may be joined by a second one next pass).
///
/// Ceiling: a run stuck in `running` forever pins the cursor at its start, so every pass re-reads
/// (idempotently) everything since. A source that turns a silent run into `lost` after a few
/// missed heartbeats releases the pin; upgrade path: ignore runs older than a hard age limit.
pub fn next_cursor(runs: &[Run], deferred: Option<&str>, old: Option<&str>) -> Option<String> {
    let in_flight = runs
        .iter()
        .filter(|r| r.status == "running")
        .map(|r| r.started_at.as_str())
        .min_by_key(|s| ts_key(s));
    let hold = match (in_flight, deferred) {
        (Some(a), Some(b)) => Some(earlier(a, b)),
        (a, b) => a.or(b),
    };
    let newest = runs
        .iter()
        .map(|r| r.started_at.as_str())
        .max_by_key(|s| ts_key(s));
    match (hold, newest) {
        (Some(h), _) => Some(h.to_string()),
        (None, Some(n)) => Some(n.to_string()),
        (None, None) => old.map(str::to_string),
    }
}

/// Where to start when no cursor exists: `lookback_h` hours before `now`, NOT the beginning of
/// history (a first run must not replay every old failure).
pub fn initial_since(now: DateTime<Utc>, lookback_h: i64) -> String {
    (now - ChronoDuration::hours(lookback_h.max(0)))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string()
}

fn status_of(topic: &Value) -> &str {
    topic.get("status").and_then(Value::as_str).unwrap_or("")
}

pub async fn run(ctx: &Ctx) -> Result<crate::Outcome> {
    let Some(base) = crate::endpoints::bridge_url() else {
        return Ok(crate::Outcome {
            summary: "skipped: HIVE_GARDENER_BRIDGE_URL is not set".to_string(),
            ..Default::default()
        });
    };
    let http = nuvek_core::http::client(std::time::Duration::from_secs(30))?;
    let cfg = Cfg {
        min_failures: crate::endpoints::bridge_min_failures(),
        grace_min: crate::endpoints::bridge_grace_min(),
    };
    let now = Utc::now();

    let old = ctx
        .hive
        .kv_get(CURSOR_KEY)
        .await?
        .and_then(|v| v.get("since").and_then(Value::as_str).map(str::to_string));
    let since = old
        .clone()
        .unwrap_or_else(|| initial_since(now, crate::endpoints::bridge_lookback_h()));
    let mut req = http
        .get(format!("{base}/api/runs"))
        .query(&[("since", since.as_str()), ("limit", &RUN_LIMIT.to_string())]);
    if let Some(token) = crate::endpoints::bridge_token() {
        req = req.bearer_auth(token);
    }
    let resp = req
        .send()
        .await
        .map_err(|e| anyhow!("bridge source unreachable: {}", e.without_url()))?
        .error_for_status()
        .map_err(|e| anyhow!("bridge source: {}", e.without_url()))?;
    let body: Value = resp.json().await?;
    let mut runs: Vec<Run> = serde_json::from_value(body.get("runs").cloned().unwrap_or(json!([])))
        .map_err(|e| anyhow!("unexpected /api/runs shape: {e}"))?;
    runs = runs.into_iter().map(Run::sanitized).collect();
    // The source may answer newest-first; incidents must be replayed oldest-first.
    runs.sort_by_cached_key(|r| (parse_ts(&r.started_at), r.started_at.clone()));

    let mut open: HashSet<String> = ctx
        .hive
        .get("/api/topics?kind=incident&status=open")
        .await?
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter(|t| matches!(status_of(t), "active" | "quiet" | "needs-human"))
        .filter_map(|t| t.get("name").and_then(Value::as_str).map(str::to_string))
        .collect();

    let plans = plan(&runs, &open, &cfg, now);
    let (mut opened, mut appended, mut recovered, mut errors) = (0i64, 0i64, 0i64, 0i64);
    let (mut quarantined, mut deferred_n) = (0i64, 0i64);
    let mut deferred_since: Option<String> = None;
    for p in &plans {
        let step = match p {
            Plan::Defer { key, since } => {
                deferred_n += 1;
                ctx.log.info(&format!(
                    "bridge: {key} deferred (below the failure threshold, inside the grace window)"
                ));
                deferred_since = Some(match deferred_since.as_deref() {
                    Some(d) => earlier(d, since).to_string(),
                    None => since.clone(),
                });
                Ok(())
            }
            Plan::Failure {
                key,
                failures,
                open: is_open,
            } => {
                on_failure(
                    ctx,
                    key,
                    failures,
                    *is_open,
                    &mut open,
                    (&mut opened, &mut appended, &mut quarantined),
                )
                .await
            }
            Plan::Recover { key, run } => {
                on_recovered(ctx, key, run, &mut open, (&mut recovered, &mut quarantined)).await
            }
        };
        if let Err(e) = step {
            errors += 1;
            ctx.log.warn(&format!("bridge: not bridged: {e:#}"));
        }
    }

    // Advance the cursor only when every key in the window was handled.
    if errors == 0 {
        if let Some(c) = next_cursor(&runs, deferred_since.as_deref(), old.as_deref()) {
            ctx.hive
                .kv_put(&ctx.log, CURSOR_KEY, &json!({"since": c}))
                .await?;
        }
    }
    let summary = format!(
        "runs={} keys={} opened={opened} appended={appended} recovered={recovered} \
         deferred={deferred_n} quarantined={quarantined} errors={errors}",
        runs.len(),
        plans.len()
    );
    if errors > 0 && opened + appended + recovered == 0 {
        return Err(anyhow!("bridge: every incident write failed ({summary})"));
    }
    Ok(crate::Outcome {
        summary,
        quarantined,
        ..Default::default()
    })
}

async fn on_failure(
    ctx: &Ctx,
    name: &str,
    failures: &[Run],
    was_open: bool,
    open: &mut HashSet<String>,
    (opened, appended, quarantined): (&mut i64, &mut i64, &mut i64),
) -> Result<()> {
    let (Some(last), true) = (failures.last(), is_valid_topic_name(name)) else {
        return Err(anyhow!(
            "bad incident plan for {name:?} (no failures, or the name violates the contract)"
        ));
    };
    // Write-boundary scan: board text that still trips gitleaks is skipped and counted.
    if !gitleaks_clean(&board_text(failures, last)) {
        *quarantined += 1;
        ctx.log
            .warn(&format!("bridge: {name} quarantined (secret scan)"));
        return Ok(());
    }
    let critical = is_critical_kind(&last.kind);
    let idem = format!("bridge-{}", last.run_id);
    let subjects = subjects_of(failures);
    let content = batch_message(failures);
    let existing = ctx.hive.get(&format!("/api/topics/{name}")).await;
    let reopened = matches!(&existing, Ok(t) if matches!(status_of(t), "resolved" | "archived"));
    let exists = match &existing {
        Ok(_) => true,
        Err(e) if e.code() == Some(404) => false,
        Err(e) => return Err(anyhow!("topic lookup {name}: {e}")),
    };

    if !exists {
        let create = json!({
            "name": name,
            "title": truncate_chars(&format!("{} failing: {}", last.kind, subject_label(last)), 120),
            "description": "Opened by the hive-gardener incident bridge from failed runs.",
            "author": BRIDGE_AUTHOR,
            "kind": "incident",
            "agent": agent(),
        });
        // A concurrent creator (409) is fine; the message below is idempotent either way.
        match ctx
            .hive
            .write(&ctx.log, Method::POST, "/api/topics", &create, None)
            .await
        {
            Ok(_) => {}
            Err(e) if e.code() == Some(409) => {}
            Err(e) => return Err(anyhow!("create {name}: {e}")),
        }
    }
    let mut tags = vec!["incident", "bridge"];
    if exists {
        tags.push("repeat");
    }
    if reopened {
        tags.push("reopened");
    }
    let msg = json!({
        "author": BRIDGE_AUTHOR,
        "content": content,
        "kind": if exists { "note" } else { "finding" },
        "severity": if exists { Value::Null } else if critical { json!("BLOCKER") } else { json!("SHOULD") },
        "subjects": subjects,
        "tags": tags,
        "agent": agent(),
    });
    let reply = ctx
        .hive
        .write(
            &ctx.log,
            Method::POST,
            &format!("/api/topics/{name}/messages"),
            &msg,
            Some(&idem),
        )
        .await?;
    let fresh = is_fresh(&reply);
    // Dry-run fidelity: a topic this pass "would create" is open for the rest of the pass.
    open.insert(name.to_string());
    if fresh {
        if exists || was_open {
            *appended += 1;
        } else {
            *opened += 1;
        }
    }
    let status = existing
        .as_ref()
        .map(|t| status_of(t).to_string())
        .unwrap_or_default();
    // Attempted on a replay too, so a PATCH that failed last pass is retried.
    if should_escalate(critical, fresh, &status) {
        needs_human(ctx, name).await;
    }
    Ok(())
}

/// Whether a critical incident's topic must be moved to `needs-human`. A fresh post reopens the
/// topic, so any status but `needs-human` qualifies; on a replay only a topic still sitting at
/// active/quiet does, so a topic a person resolved is never re-escalated by a re-read window.
pub fn should_escalate(critical: bool, fresh: bool, status: &str) -> bool {
    critical && status != "needs-human" && (fresh || matches!(status, "active" | "quiet" | ""))
}

fn subject_label(r: &Run) -> String {
    match r.targets.as_slice() {
        [only] => only.clone(),
        _ => r.host.clone(),
    }
}

async fn needs_human(ctx: &Ctx, name: &str) {
    let body = json!({"status": "needs-human", "actor": "gardener"});
    if let Err(e) = ctx
        .hive
        .write(
            &ctx.log,
            Method::PATCH,
            &format!("/api/topics/{name}"),
            &body,
            None,
        )
        .await
    {
        // 409 = already there or transition not legal from here; neither is worth failing the pass.
        ctx.log.warn(&format!("bridge: needs-human {name}: {e}"));
    }
}

async fn on_recovered(
    ctx: &Ctx,
    name: &str,
    r: &Run,
    open: &mut HashSet<String>,
    (recovered, quarantined): (&mut i64, &mut i64),
) -> Result<()> {
    let text = format!("Recovered: {}", describe(r));
    if !gitleaks_clean(&text) {
        *quarantined += 1;
        ctx.log.warn(&format!(
            "bridge: recovery of {name} quarantined (secret scan)"
        ));
        return Ok(());
    }
    let idem = format!("bridge-rec-{}-{name}", r.run_id);
    let msg = json!({
        "author": BRIDGE_AUTHOR,
        "content": text,
        "kind": "note",
        "tags": ["incident", "bridge", "recovered"],
        "agent": agent(),
    });
    let reply = ctx
        .hive
        .write(
            &ctx.log,
            Method::POST,
            &format!("/api/topics/{name}/messages"),
            &msg,
            Some(&idem),
        )
        .await?;
    // A replay means the message landed on an earlier pass, but the resolve PATCH may not have:
    // always attempt it (a topic already resolved answers 409, tolerated below) and let only the
    // `recovered` counter depend on freshness.
    let fresh = is_fresh(&reply);
    let patch = json!({
        "status": "resolved",
        "outcome": truncate_chars(&format!("recovered: run {} succeeded", r.run_id), 200),
        "actor": "gardener",
    });
    match ctx
        .hive
        .write(
            &ctx.log,
            Method::PATCH,
            &format!("/api/topics/{name}"),
            &patch,
            None,
        )
        .await
    {
        Ok(_) => {
            open.remove(name);
            if fresh {
                *recovered += 1;
            }
        }
        Err(e) if e.code() == Some(409) => {
            open.remove(name);
        }
        Err(e) => return Err(anyhow!("resolve {name}: {e}")),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(id: &str, status: &str, started: &str) -> Run {
        Run {
            run_id: id.into(),
            kind: "deploy.auto".into(),
            host: "nas".into(),
            status: status.into(),
            started_at: started.into(),
            ..Default::default()
        }
    }

    fn heal(id: &str, status: &str, started: &str) -> Run {
        Run {
            kind: "heal".into(),
            targets: vec!["c:queue-worker".into()],
            ..run(id, status, started)
        }
    }

    const CFG: Cfg = Cfg {
        min_failures: 2,
        grace_min: 15,
    };
    const KEY: &str = "incident-heal-c-queue-worker";

    fn at(s: &str) -> DateTime<Utc> {
        parse_ts(s).unwrap()
    }

    #[test]
    fn classification_ignores_dry_runs_and_in_flight_runs() {
        assert_eq!(classify(&run("a", "failed", "t")), Action::Failure);
        assert_eq!(classify(&run("a", "partial", "t")), Action::Failure);
        assert_eq!(classify(&run("a", "succeeded", "t")), Action::Success);
        assert_eq!(classify(&run("a", "running", "t")), Action::Ignore);
        assert_eq!(classify(&run("a", "lost", "t")), Action::Ignore);
        let mut dry = run("a", "failed", "t");
        dry.dry_run = true;
        assert_eq!(classify(&dry), Action::Ignore);
    }

    #[test]
    fn cursor_never_passes_a_running_run_or_a_deferred_failure() {
        let runs = vec![
            run("1", "failed", "2026-10-07T01:00:00Z"),
            run("2", "running", "2026-10-07T02:00:00Z"),
            run("3", "succeeded", "2026-10-07T03:00:00Z"),
        ];
        assert_eq!(
            next_cursor(&runs, None, None).as_deref(),
            Some("2026-10-07T02:00:00Z")
        );
        assert_eq!(
            next_cursor(&runs[..1], None, None).as_deref(),
            Some("2026-10-07T01:00:00Z")
        );
        // a deferred failure earlier than the in-flight run holds the cursor back further
        assert_eq!(
            next_cursor(&runs, Some("2026-10-07T01:30:00Z"), None).as_deref(),
            Some("2026-10-07T01:30:00Z")
        );
        assert_eq!(
            next_cursor(&runs[2..], Some("2026-10-07T02:30:00Z"), None).as_deref(),
            Some("2026-10-07T02:30:00Z")
        );
        assert_eq!(next_cursor(&[], None, Some("old")).as_deref(), Some("old"));
        assert_eq!(next_cursor(&[], None, None), None);
    }

    #[test]
    fn first_run_starts_a_lookback_ago_not_at_the_beginning_of_history() {
        let now = at("2026-10-07T12:00:00Z");
        assert_eq!(initial_since(now, 24), "2026-10-06T12:00:00Z");
        assert_eq!(initial_since(now, 1), "2026-10-07T11:00:00Z");
        // a nonsense negative lookback never reaches into the future
        assert_eq!(initial_since(now, -5), "2026-10-07T12:00:00Z");
    }

    #[test]
    fn subjects_strip_a_single_letter_type_prefix_but_text_keeps_the_raw_target() {
        assert_eq!(subject_of("c:queue-worker"), "queue-worker");
        assert_eq!(subject_of("x:thing"), "thing");
        assert_eq!(subject_of("queue-worker"), "queue-worker");
        assert_eq!(
            subject_of("ab:cd"),
            "ab:cd",
            "two letters is not a type prefix"
        );
        assert_eq!(subject_of("c:"), "c:", "an empty remainder is kept as is");
        assert_eq!(subject_of("€:x"), "€:x", "non-ASCII is not a prefix");
        let runs = vec![heal("1", "failed", "t"), heal("2", "failed", "t")];
        assert_eq!(subjects_of(&runs), vec!["queue-worker".to_string()]);
        assert!(describe(&runs[0]).contains("c:queue-worker"));
    }

    #[test]
    fn a_flapping_heal_that_ends_in_success_with_no_topic_does_nothing() {
        let runs = vec![
            heal("1", "failed", "2026-10-07T10:00:00Z"),
            heal("2", "succeeded", "2026-10-07T10:00:05Z"),
            heal("3", "failed", "2026-10-07T10:01:00Z"),
            heal("4", "succeeded", "2026-10-07T10:01:06Z"),
        ];
        let p = plan(&runs, &HashSet::new(), &CFG, at("2026-10-07T10:02:00Z"));
        assert!(p.is_empty(), "{p:?}");
    }

    #[test]
    fn final_success_with_an_open_topic_recovers_once() {
        let runs = vec![
            heal("1", "failed", "2026-10-07T10:00:00Z"),
            heal("2", "succeeded", "2026-10-07T10:00:05Z"),
            heal("3", "failed", "2026-10-07T10:01:00Z"),
            heal("4", "succeeded", "2026-10-07T10:01:06Z"),
        ];
        let open: HashSet<String> = [KEY.to_string()].into();
        let p = plan(&runs, &open, &CFG, at("2026-10-07T10:02:00Z"));
        assert_eq!(p.len(), 1);
        match &p[0] {
            Plan::Recover { key, run } => {
                assert_eq!(key, KEY);
                assert_eq!(run.run_id, "4");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn final_failure_coalesces_the_batch_into_one_plan() {
        let runs = vec![
            heal("1", "failed", "2026-10-07T10:00:00Z"),
            heal("2", "succeeded", "2026-10-07T10:00:05Z"),
            heal("3", "failed", "2026-10-07T10:01:00Z"),
            heal("4", "failed", "2026-10-07T10:02:00Z"),
        ];
        let p = plan(&runs, &HashSet::new(), &CFG, at("2026-10-07T10:03:00Z"));
        assert_eq!(p.len(), 1);
        match &p[0] {
            Plan::Failure {
                key,
                failures,
                open,
            } => {
                assert_eq!(key, KEY);
                assert!(!open);
                assert_eq!(failures.len(), 3, "every failure in the batch is counted");
                assert_eq!(failures[0].run_id, "1");
                assert_eq!(failures[2].run_id, "4");
            }
            other => panic!("{other:?}"),
        }
        let msg = batch_message(match &p[0] {
            Plan::Failure { failures, .. } => failures,
            _ => unreachable!(),
        });
        assert!(
            msg.contains("3 failures") && msg.contains("first run 1") && msg.contains("last run 4")
        );
    }

    #[test]
    fn a_topic_needs_consecutive_failures_or_an_unanswered_grace_period() {
        let one = vec![heal("1", "failed", "2026-10-07T10:00:00Z")];
        // fresh single failure: deferred, and the cursor is held at it
        let p = plan(&one, &HashSet::new(), &CFG, at("2026-10-07T10:05:00Z"));
        assert_eq!(
            p,
            vec![Plan::Defer {
                key: KEY.into(),
                since: "2026-10-07T10:00:00Z".into()
            }]
        );
        // the same failure after the grace window opens a topic
        let p = plan(&one, &HashSet::new(), &CFG, at("2026-10-07T10:16:00Z"));
        assert!(matches!(&p[0], Plan::Failure { .. }));
        // two consecutive failures open one immediately
        let two = vec![
            heal("1", "failed", "2026-10-07T10:00:00Z"),
            heal("2", "failed", "2026-10-07T10:01:00Z"),
        ];
        let p = plan(&two, &HashSet::new(), &CFG, at("2026-10-07T10:02:00Z"));
        assert!(matches!(&p[0], Plan::Failure { .. }));
        // a failure, a success, then one more failure is NOT consecutive
        let flap = vec![
            heal("1", "failed", "2026-10-07T10:00:00Z"),
            heal("2", "succeeded", "2026-10-07T10:00:05Z"),
            heal("3", "failed", "2026-10-07T10:01:00Z"),
        ];
        let p = plan(&flap, &HashSet::new(), &CFG, at("2026-10-07T10:02:00Z"));
        assert_eq!(
            p,
            vec![Plan::Defer {
                key: KEY.into(),
                since: "2026-10-07T10:01:00Z".into()
            }],
            "the cursor holds at the FIRST trailing failure"
        );
        // an already-open topic is appended to at once
        let open: HashSet<String> = [KEY.to_string()].into();
        let p = plan(&one, &open, &CFG, at("2026-10-07T10:00:30Z"));
        assert!(matches!(&p[0], Plan::Failure { open: true, .. }));
        // an unreadable timestamp is treated as aged rather than hidden forever
        let bad = vec![heal("1", "failed", "not-a-time")];
        assert!(matches!(
            &plan(&bad, &HashSet::new(), &CFG, at("2026-10-07T10:00:00Z"))[0],
            Plan::Failure { .. }
        ));
    }

    #[test]
    fn a_multi_target_success_recovers_the_per_target_topic() {
        let mut fail = heal("1", "failed", "2026-10-07T10:00:00Z");
        fail.targets = vec!["c:web-app".into()];
        let ok = Run {
            kind: "heal".into(),
            targets: vec!["c:web-app".into(), "c:cache".into()],
            ..run("2", "succeeded", "2026-10-07T10:05:00Z")
        };
        let open: HashSet<String> = ["incident-heal-c-web-app".to_string()].into();
        let p = plan(&[fail, ok], &open, &CFG, at("2026-10-07T10:06:00Z"));
        assert_eq!(p.len(), 1);
        assert!(matches!(&p[0], Plan::Recover { key, .. } if key == "incident-heal-c-web-app"));
    }

    #[test]
    fn keys_are_independent_and_dry_runs_never_plan() {
        let a = heal("1", "failed", "2026-10-07T10:00:00Z");
        let b = Run {
            targets: vec!["c:web-app".into()],
            ..heal("2", "failed", "2026-10-07T10:00:10Z")
        };
        let mut dry = heal("3", "failed", "2026-10-07T10:00:20Z");
        dry.dry_run = true;
        let runs = vec![a.clone(), b.clone(), a, b, dry];
        let p = plan(&runs, &HashSet::new(), &CFG, at("2026-10-07T10:01:00Z"));
        assert_eq!(p.len(), 2);
        assert!(p
            .iter()
            .all(|x| matches!(x, Plan::Failure { failures, .. } if failures.len() == 2)));
    }

    /// Loopback server answering every request 200 `{}` (a replayed POST and a good PATCH);
    /// returns each request's first line, collected for up to 3 s or until `n` arrive.
    fn serve_n(n: usize) -> (String, std::thread::JoinHandle<Vec<String>>) {
        use std::io::{Read, Write};
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.set_nonblocking(true).unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        let h = std::thread::spawn(move || {
            let mut seen = Vec::new();
            let end = std::time::Instant::now() + std::time::Duration::from_secs(3);
            while seen.len() < n && std::time::Instant::now() < end {
                match l.accept() {
                    Ok((mut s, _)) => {
                        s.set_nonblocking(false).unwrap();
                        let mut buf = [0u8; 8192];
                        let k = s.read(&mut buf).unwrap_or(0);
                        let text = String::from_utf8_lossy(&buf[..k]).to_string();
                        seen.push(text.lines().next().unwrap_or("").to_string());
                        let _ = s.write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
                        );
                    }
                    Err(_) => std::thread::sleep(std::time::Duration::from_millis(10)),
                }
            }
            seen
        });
        (url, h)
    }

    /// The server answers 200 (an idempotent replay) to the recovery message: the resolve PATCH
    /// must still be sent, and the `recovered` counter must not move.
    #[tokio::test]
    async fn a_replayed_recovery_still_attempts_the_resolve() {
        let (url, h) = serve_n(2);
        let ctx = Ctx {
            hive: crate::api::Hive::new(url, None, false).unwrap(),
            log: nuvek_core::log::Logger::stderr_only("test"),
            deadline: std::time::Instant::now() + std::time::Duration::from_secs(60),
            topology: None,
        };
        let mut open: HashSet<String> = [KEY.to_string()].into();
        let (mut recovered, mut quarantined) = (0i64, 0i64);
        let r = heal("4", "succeeded", "2026-10-07T10:01:06Z");
        on_recovered(&ctx, KEY, &r, &mut open, (&mut recovered, &mut quarantined))
            .await
            .unwrap();
        let seen = h.join().unwrap();
        assert_eq!(seen.len(), 2, "{seen:?}");
        assert!(
            seen[0].starts_with("POST ") && seen[0].contains("/messages"),
            "{seen:?}"
        );
        assert!(
            seen[1].starts_with("PATCH ") && seen[1].contains(KEY),
            "{seen:?}"
        );
        assert_eq!(recovered, 0, "a replay is not a new recovery");
        assert!(
            !open.contains(KEY),
            "the topic is no longer tracked as open"
        );
    }

    #[test]
    fn escalation_is_retried_on_replays_but_never_overrides_a_human() {
        assert!(
            should_escalate(true, true, "resolved"),
            "a fresh post reopened it"
        );
        assert!(should_escalate(true, true, "active"));
        assert!(
            should_escalate(true, false, "active"),
            "replay: retry a failed PATCH"
        );
        assert!(should_escalate(true, false, "quiet"));
        assert!(
            !should_escalate(true, false, "resolved"),
            "a person closed it"
        );
        assert!(!should_escalate(true, false, "archived"));
        assert!(!should_escalate(true, true, "needs-human"));
        assert!(!should_escalate(false, true, "active"));
    }

    #[test]
    fn timestamps_compare_by_instant_not_by_spelling() {
        // textually "+00:00" > "Z" at index 19 ('+' < 'Z' actually); the instants decide
        let a = "2026-10-07T10:00:00.500Z";
        let b = "2026-10-07T10:00:00+00:00";
        assert_eq!(earlier(a, b), b);
        assert_eq!(earlier(b, a), b);
        // a non-UTC offset: 11:00+02:00 is 09:00Z, earlier than 10:00Z
        assert_eq!(
            earlier("2026-10-07T10:00:00Z", "2026-10-07T11:00:00+02:00"),
            "2026-10-07T11:00:00+02:00"
        );
        let runs = vec![
            run("1", "running", "2026-10-07T11:00:00+02:00"),
            run("2", "succeeded", "2026-10-07T10:30:00Z"),
        ];
        assert_eq!(
            next_cursor(&runs, None, None).as_deref(),
            Some("2026-10-07T11:00:00+02:00"),
            "the running run (09:00Z) pins the cursor before the 10:30Z run"
        );
        let newest = vec![
            run("1", "succeeded", "2026-10-07T11:00:00+02:00"),
            run("2", "succeeded", "2026-10-07T10:30:00Z"),
        ];
        assert_eq!(
            next_cursor(&newest, None, None).as_deref(),
            Some("2026-10-07T10:30:00Z")
        );
        // unreadable timestamps sort first and never panic a sort
        let mut v = vec!["b", "2026-10-07T10:00:00Z", "a"];
        v.sort_by_cached_key(|s| (parse_ts(s), s.to_string()));
        assert_eq!(v, vec!["a", "b", "2026-10-07T10:00:00Z"]);
    }

    #[test]
    fn a_host_keyed_topic_is_not_recovered_while_a_target_of_that_host_still_fails() {
        // fan-out failure opened the host topic earlier; this batch: web-app heals, db still fails
        let host_key = "incident-heal-nas".to_string();
        let ok = Run {
            kind: "heal".into(),
            targets: vec!["c:web-app".into(), "c:db".into()],
            ..run("2", "succeeded", "2026-10-07T10:05:00Z")
        };
        let bad = Run {
            targets: vec!["c:db".into()],
            ..heal("3", "failed", "2026-10-07T10:06:00Z")
        };
        let open: HashSet<String> = [host_key.clone()].into();
        let p = plan(&[ok.clone(), bad], &open, &CFG, at("2026-10-07T10:06:30Z"));
        assert!(
            !p.iter()
                .any(|x| matches!(x, Plan::Recover { key, .. } if *key == host_key)),
            "{p:?}"
        );
        // nothing failing: the host topic recovers
        let p = plan(&[ok], &open, &CFG, at("2026-10-07T10:06:30Z"));
        assert!(p
            .iter()
            .any(|x| matches!(x, Plan::Recover { key, .. } if *key == host_key)));
    }

    #[test]
    fn describe_scrubs_the_producer_title() {
        let secret = format!("ghp_{}", "a1B2c3D4e5F6g7H8i9J0k1L2m3N4o5P6q7R8"); // gitleaks:allow
        let mut r = run("r1", "failed", "t");
        r.title = format!("pull failed with {secret}");
        let d = describe(&r);
        assert!(!d.contains(&secret), "{d}");
        assert!(d.contains("Run r1"));
    }

    #[test]
    fn sanitising_redacts_every_field_and_bounds_targets() {
        let secret = format!("ghp_{}", "a1B2c3D4e5F6g7H8i9J0k1L2m3N4o5P6q7R8"); // gitleaks:allow
        let r = Run {
            run_id: format!("id-{secret}"),
            kind: format!("k-{secret}"),
            host: format!("h-{secret}"),
            title: format!("t-{secret}"),
            targets: vec![format!("x{secret}"), "€".repeat(500)],
            ..Default::default()
        }
        .sanitized();
        let all = format!(
            "{} {} {} {} {}",
            r.run_id,
            r.kind,
            r.host,
            r.title,
            r.targets.join(" ")
        );
        assert!(!all.contains(&secret), "{all}");
        assert!(r
            .targets
            .iter()
            .all(|t| t.chars().count() <= MAX_TARGET_CHARS));
        // names derived from the sanitised run are valid and secret-free
        let n = incident_topic_name(&r.kind, &r.targets[..1], &r.host);
        assert!(is_valid_topic_name(&n) && !n.contains("ghp"), "{n}");
        // a fan-out is capped
        let many = Run {
            targets: (0..500).map(|i| i.to_string()).collect(),
            ..Default::default()
        }
        .sanitized();
        assert_eq!(many.targets.len(), MAX_TARGETS);
        let one = [r.clone()];
        assert!(!board_text(&one, &r).contains(&secret));
    }

    #[test]
    fn run_deserialises_the_dashboard_shape_with_extra_fields() {
        let v = json!({"run_id":"r","kind":"k","host":"h","trigger":"timer","title":"t","dry_run":false,
            "status":"failed","started_at":"s","ended_at":null,"last_event_at":"e","progress":null,
            "eta_s":null,"current_step":null,"error_count":2,"warn_count":0,"targets":["a"]});
        let r: Run = serde_json::from_value(v).unwrap();
        assert_eq!(r.error_count, 2);
        assert_eq!(r.targets, vec!["a".to_string()]);
    }
}
