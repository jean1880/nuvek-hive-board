//! Jobs: state x route table, owns rules, READY/BLOCKED, claim batches, concurrency, nonce
//! secrecy, round fencing, verifier rules, lifecycle guards.

mod common;

use axum::http::StatusCode;
use common::{send_to, TestApp, OWNS_ROOT_GIT, OWNS_ROOT_SCRIPTS, TOKEN};
use serde_json::{json, Value};

const ROOT: &str = OWNS_ROOT_GIT;

fn p(sub: &str) -> String {
    format!("{ROOT}/{sub}")
}

fn agent(t: &str, m: &str, h: &str) -> Value {
    json!({"agent_type": t, "model": m, "harness": h})
}
fn orchestrator() -> Value {
    agent("orchestrator", "opus", "claude")
}
fn worker() -> Value {
    agent("rust-builder", "sonnet", "claude")
}
fn reviewer() -> Value {
    agent("rust-reviewer", "opus", "claude")
}
fn agy_reviewer() -> Value {
    agent("reviewer", "gemini", "agy")
}

fn job(name: &str, owns: &[String], deps: &[&str]) -> Value {
    json!({"name": name, "title": format!("Job {name}"), "spec": "do the thing",
           "capability": "local-build", "owns": owns, "depends_on": deps})
}

async fn create_raw(t: &TestApp, jobs: Value) -> (StatusCode, Value) {
    t.write(
        "POST",
        "/api/jobs",
        json!({"jobs": jobs, "author": "orch", "agent": orchestrator()}),
    )
    .await
}

async fn create(t: &TestApp, jobs: Value) -> Vec<Value> {
    let (s, v) = create_raw(t, jobs).await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    v.as_array().expect("array").clone()
}

async fn mk(t: &TestApp, name: &str, owns: &[String], deps: &[&str]) -> Value {
    create(t, json!([job(name, owns, deps)])).await.remove(0)
}

async fn claim_with(t: &TestApp, limit: i64, force: bool) -> Vec<Value> {
    let (s, v) = t
        .write(
            "POST",
            "/api/jobs/claim",
            json!({"claimant": "orch", "agent": orchestrator(), "limit": limit, "force_overlap": force}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    v.as_array().expect("array").clone()
}

async fn claim(t: &TestApp, limit: i64) -> Vec<Value> {
    claim_with(t, limit, false).await
}

async fn complete(
    t: &TestApp,
    id: &str,
    nonce: &str,
    round: i64,
    status: &str,
) -> (StatusCode, Value) {
    t.write(
        "POST",
        &format!("/api/jobs/{id}/complete"),
        json!({"nonce": nonce, "round": round, "status": status, "worker": worker(),
               "result": "all done", "author": "orch"}),
    )
    .await
}

async fn verify_with(t: &TestApp, id: &str, verdict: &str, verifier: Value) -> (StatusCode, Value) {
    t.write(
        "POST",
        &format!("/api/jobs/{id}/verify"),
        json!({"verdict": verdict, "note": "looks right", "author": "orch",
               "verifier": verifier.clone(), "agent": verifier}),
    )
    .await
}

async fn getj(t: &TestApp, id: &str) -> Value {
    let (s, v) = t.get(&format!("/api/jobs/{id}")).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    v
}

fn s(v: &Value) -> &str {
    v.as_str().expect("string")
}

fn nonce_of(c: &Value) -> String {
    s(&c["nonce"]).to_string()
}

// ------------------------------------------------------------------ the state x route table

struct Ctx {
    name: String,
    nonce: Option<String>,
}

async fn job_in_state(t: &TestApp, state: &str) -> Ctx {
    let name = format!("cell-{state}");
    let owns = [p(&format!("proj-{state}"))];
    mk(t, &name, &owns, &[]).await;
    let mut nonce = None;
    match state {
        "open" => {}
        "cancelled" => {
            let (st, v) = t
                .write(
                    "POST",
                    &format!("/api/jobs/{name}/cancel"),
                    json!({"actor": "orch", "reason": "setup"}),
                )
                .await;
            assert_eq!(st, StatusCode::OK, "{v}");
        }
        _ => {
            let got = claim(t, 10).await;
            assert_eq!(got.len(), 1, "setup claim for {state}");
            let n = nonce_of(&got[0]);
            nonce = Some(n.clone());
            match state {
                "claimed" => {}
                "done" | "verified" => {
                    let (st, v) = complete(t, &name, &n, 1, "done").await;
                    assert_eq!(st, StatusCode::OK, "{v}");
                    if state == "verified" {
                        let (st, v) = verify_with(t, &name, "pass", reviewer()).await;
                        assert_eq!(st, StatusCode::OK, "{v}");
                    }
                }
                "failed" => {
                    let (st, v) = complete(t, &name, &n, 1, "failed").await;
                    assert_eq!(st, StatusCode::OK, "{v}");
                }
                other => panic!("unknown state {other}"),
            }
        }
    }
    assert_eq!(getj(t, &name).await["status"], state);
    Ctx { name, nonce }
}

async fn attempt(t: &TestApp, route: &str, c: &Ctx) -> bool {
    let n = c.nonce.clone().unwrap_or_else(|| "bogus".to_string());
    let name = &c.name;
    let status = match route {
        "claim" => {
            let got = claim(t, 10).await;
            return got
                .iter()
                .any(|x| x["job"]["topic_name"] == c.name.as_str());
        }
        "complete_done" => complete(t, name, &n, 1, "done").await.0,
        "complete_failed" => complete(t, name, &n, 1, "failed").await.0,
        "verify" => verify_with(t, name, "pass", reviewer()).await.0,
        "reclaim" => {
            t.write(
                "POST",
                &format!("/api/jobs/{name}/reclaim"),
                json!({"actor": "orch", "reason": "r"}),
            )
            .await
            .0
        }
        "cancel" => {
            t.write(
                "POST",
                &format!("/api/jobs/{name}/cancel"),
                json!({"actor": "orch", "reason": "r"}),
            )
            .await
            .0
        }
        "retry" => {
            t.write(
                "POST",
                &format!("/api/jobs/{name}/retry"),
                json!({"actor": "orch"}),
            )
            .await
            .0
        }
        "amend" => {
            t.write(
                "POST",
                &format!("/api/jobs/{name}/amend"),
                json!({"author": "orch", "agent": orchestrator(), "text": "clarification"}),
            )
            .await
            .0
        }
        "patch_priority" => {
            t.write(
                "PATCH",
                &format!("/api/jobs/{name}"),
                json!({"priority": 5, "actor": "orch"}),
            )
            .await
            .0
        }
        "patch_owns" => {
            t.write(
                "PATCH",
                &format!("/api/jobs/{name}"),
                json!({"owns": [p("elsewhere")], "actor": "orch"}),
            )
            .await
            .0
        }
        other => panic!("unknown route {other}"),
    };
    assert!(
        status.is_success() || status == StatusCode::CONFLICT,
        "{route} on {name} gave {status}, expected 2xx or 409"
    );
    status.is_success()
}

#[tokio::test]
async fn every_cell_of_the_state_route_table() {
    const ROUTES: [&str; 10] = [
        "claim",
        "complete_done",
        "complete_failed",
        "verify",
        "reclaim",
        "cancel",
        "retry",
        "amend",
        "patch_priority",
        "patch_owns",
    ];
    // Expected success per route, in ROUTES order.
    let table: [(&str, [bool; 10]); 6] = [
        (
            "open",
            [
                true, false, false, false, false, true, false, true, true, true,
            ],
        ),
        (
            "claimed",
            [
                false, true, true, false, true, true, false, true, true, false,
            ],
        ),
        (
            "done",
            [
                false, true, false, true, true, true, false, true, true, false,
            ],
        ),
        (
            "verified",
            [
                false, false, false, false, false, false, false, true, false, false,
            ],
        ),
        (
            "failed",
            [
                false, false, true, false, false, true, true, true, false, false,
            ],
        ),
        (
            "cancelled",
            [
                false, false, false, false, false, false, true, true, false, false,
            ],
        ),
    ];
    for (state, expect) in table {
        for (route, want) in ROUTES.iter().zip(expect) {
            // A fresh database per cell: every cell starts from a clean job in `state`.
            let t = TestApp::new().await;
            let c = job_in_state(&t, state).await;
            let got = attempt(&t, route, &c).await;
            assert_eq!(got, want, "state {state} x route {route}");
            t.cleanup().await;
        }
    }
}

// ------------------------------------------------------------------ owns rules (API)

#[tokio::test]
async fn owns_rules_over_the_api() {
    let t = TestApp::new().await;
    let mut n = 0;
    let mut try_owns = |owns: Vec<String>| {
        n += 1;
        (format!("owns-{n}"), owns)
    };
    for (owns, ok) in [
        (vec![p("hive-board")], true),
        (vec![p("hive-board/.github")], true), // `.github` is not `.git`
        (vec![format!("{OWNS_ROOT_SCRIPTS}/crates/hive-mcp")], true),
        (vec![p("hive-board/Cargo.lock.bak")], true),
        (vec![p("hive-board/.git")], false),
        (vec![p("hive-board/.git/hooks")], false),
        (vec![OWNS_ROOT_SCRIPTS.to_string()], false), // a root itself
        (vec![ROOT.to_string()], false),
        (vec!["~/scripts/crates".to_string()], false),
        (vec![p("hive-board/../x")], false),
        (vec![p("hive-*")], false),
        (vec!["relative/dir".to_string()], false),
        (vec![p("hive-board/Cargo.lock")], false),
        (vec![p("hive-board/frontend/package-lock.json")], false),
        (vec![p("hive-board/pnpm-lock.yaml")], false),
        (vec!["/etc/hosts".to_string()], false),
        (vec![], false), // local-build needs owns
    ] {
        let (name, owns_v) = try_owns(owns.clone());
        let (st, v) = create_raw(&t, json!([job(&name, &owns_v, &[])])).await;
        if ok {
            assert_eq!(st, StatusCode::CREATED, "{owns:?}: {v}");
        } else {
            assert_eq!(st, StatusCode::BAD_REQUEST, "{owns:?}: {v}");
        }
    }
    // Normalisation is applied and stored.
    let v = mk(&t, "norm-job", &[format!("{ROOT}//hive-board/./src/")], &[]).await;
    assert_eq!(v["owns"], json!([p("hive-board/src")]));
    // live-infra is refused outright; read-only may omit owns.
    let (st, v) = create_raw(
        &t,
        json!([{"name": "infra-job", "title": "x", "spec": "x", "capability": "live-infra"}]),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");
    let (st, _) = create_raw(
        &t,
        json!([{"name": "ro-job", "title": "x", "spec": "x", "capability": "read-only"}]),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);
    t.cleanup().await;
}

/// An unconfigured server (no `HIVE_JOB_OWNS_ROOTS`) fails closed: no `owns` entry is
/// accepted, so local-build jobs are refused, while jobs that own nothing still work.
#[tokio::test]
async fn empty_owns_roots_refuse_every_owns_entry() {
    let t = TestApp::with_default_jobs().await;
    let (st, v) = create_raw(&t, json!([job("fc-build", &[p("repo/src")], &[])])).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");
    assert!(
        v.to_string().contains("HIVE_JOB_OWNS_ROOTS"),
        "the error names the setting: {v}"
    );
    let (st, v) = create_raw(
        &t,
        json!([{"name": "fc-ro", "title": "x", "spec": "x", "capability": "read-only"}]),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{v}");
    t.cleanup().await;
}

// ------------------------------------------------------------------ READY / BLOCKED

#[tokio::test]
async fn ready_computation_overlap_both_directions_and_deps() {
    let t = TestApp::new().await;
    // A owns src; B owns its ANCESTOR; D owns a DESCENDANT; C is a sibling-prefix lookalike.
    mk(&t, "job-a", &[p("hive-board/src")], &[]).await;
    mk(&t, "job-b", &[p("hive-board")], &[]).await;
    mk(&t, "job-c", &[p("hive-board2")], &[]).await;
    mk(&t, "job-d", &[p("hive-board/src/api")], &[]).await;
    // Nothing claimed yet: overlap alone does not block.
    let (_, ready) = t.get("/api/jobs?ready=true").await;
    assert_eq!(ready.as_array().expect("r").len(), 4);

    let got = claim(&t, 1).await;
    assert_eq!(got[0]["job"]["topic_name"], "job-a");
    let a_id = got[0]["job"]["id"].as_i64().expect("id");
    let na = nonce_of(&got[0]);

    let b = getj(&t, "job-b").await;
    assert_eq!(b["ready"], false);
    assert_eq!(b["conflicts_with"], json!([a_id])); // ancestor of a claimed job
    let d = getj(&t, "job-d").await;
    assert_eq!(d["ready"], false);
    assert_eq!(d["conflicts_with"], json!([a_id])); // descendant of a claimed job
    assert_eq!(getj(&t, "job-c").await["ready"], true); // whole-component rule

    // done-but-unverified still holds its files.
    let (st, v) = complete(&t, "job-a", &na, 1, "done").await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(getj(&t, "job-b").await["ready"], false);
    // verified releases them.
    let (st, v) = verify_with(&t, "job-a", "pass", reviewer()).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(getj(&t, "job-b").await["ready"], true);
    assert_eq!(getj(&t, "job-d").await["ready"], true);
    t.cleanup().await;
}

#[tokio::test]
async fn dependencies_blocked_and_security_need_verified() {
    let t = TestApp::new().await;
    create(
        &t,
        json!([
            job("dep-a", &[p("proj-a")], &[]),
            job("dep-b", &[p("proj-b")], &["dep-a"]),
            {"name": "sec-a", "title": "sec", "spec": "x", "capability": "local-build",
             "security": true, "owns": [p("proj-sec")]},
            job("sec-b", &[p("proj-secb")], &["sec-a"]),
            job("gone-a", &[p("proj-gone")], &[]),
            job("gone-b", &[p("proj-goneb")], &["gone-a"]),
        ]),
    )
    .await;
    // dep-b waits for dep-a.
    let b = getj(&t, "dep-b").await;
    assert_eq!(b["ready"], false);
    assert_eq!(b["blocked"], false);
    assert_eq!(b["blocked_by"].as_array().expect("b").len(), 1);
    assert_eq!(b["depends_on"][0], getj(&t, "dep-a").await["id"]);

    // Claim and complete dep-a: a non-security dependency only needs `done`.
    let got = claim(&t, 10).await;
    let names: Vec<&str> = got.iter().map(|g| s(&g["job"]["topic_name"])).collect();
    assert!(names.contains(&"dep-a") && names.contains(&"sec-a") && names.contains(&"gone-a"));
    assert!(!names.contains(&"dep-b") && !names.contains(&"sec-b"));
    let nonce = |n: &str| {
        got.iter()
            .find(|g| g["job"]["topic_name"] == n)
            .map(nonce_of)
            .expect("nonce")
    };
    complete(&t, "dep-a", &nonce("dep-a"), 1, "done").await;
    assert_eq!(getj(&t, "dep-b").await["ready"], true);

    // A security dependency must be VERIFIED, not just done.
    complete(&t, "sec-a", &nonce("sec-a"), 1, "done").await;
    assert_eq!(getj(&t, "sec-b").await["ready"], false);
    // Same harness as the worker is refused for a security job; a different harness passes.
    let (st, _) = verify_with(&t, "sec-a", "pass", reviewer()).await;
    assert_eq!(st, StatusCode::CONFLICT);
    let (st, v) = verify_with(&t, "sec-a", "pass", agy_reviewer()).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(getj(&t, "sec-b").await["ready"], true);

    // A cancelled dependency makes the dependent BLOCKED with a reason.
    let (st, _) = t
        .write(
            "POST",
            "/api/jobs/gone-a/cancel",
            json!({"actor": "orch", "reason": "dropped"}),
        )
        .await;
    assert_eq!(st, StatusCode::OK);
    let gb = getj(&t, "gone-b").await;
    assert_eq!(gb["blocked"], true);
    assert!(s(&gb["blocked_reason"]).contains("gone-a"), "{gb}");
    assert_eq!(gb["ready"], false);
    let (_, blocked) = t.get("/api/jobs?blocked=true").await;
    assert_eq!(blocked.as_array().expect("b").len(), 1);
    t.cleanup().await;
}

// ------------------------------------------------------------------ claim batches

#[tokio::test]
async fn a_claim_batch_never_returns_two_overlapping_jobs() {
    let t = TestApp::new().await;
    create(
        &t,
        json!([
            job("ov-1", &[p("ov")], &[]),
            job("ov-2", &[p("ov/sub")], &[]),
            job("ov-3", &[p("ov/sub/deeper")], &[]),
            job("free", &[p("independent")], &[]),
        ]),
    )
    .await;
    let got = claim(&t, 10).await;
    let names: Vec<&str> = got.iter().map(|g| s(&g["job"]["topic_name"])).collect();
    assert_eq!(names.len(), 2, "{names:?}");
    assert!(names.contains(&"ov-1") && names.contains(&"free"));
    // Nothing else is claimable while ov-1 holds its tree.
    assert!(claim(&t, 10).await.is_empty());
    // force_overlap is the recorded orchestrator override.
    let forced = claim_with(&t, 10, true).await;
    assert_eq!(forced.len(), 2);
    let (_, ev) = t.get("/api/topics/ov-2/events").await;
    assert!(ev
        .as_array()
        .expect("e")
        .iter()
        .any(|e| e["event"] == "owns-overlap-forced"));
    t.cleanup().await;
}

#[tokio::test]
async fn twenty_parallel_claims_never_grant_overlapping_owns() {
    let t = TestApp::new().await;
    let mut jobs = Vec::new();
    for g in 1..=4 {
        jobs.push(job(&format!("g{g}-top"), &[p(&format!("grp{g}"))], &[]));
        jobs.push(job(&format!("g{g}-sub"), &[p(&format!("grp{g}/sub"))], &[]));
    }
    create(&t, json!(jobs)).await;
    let mut handles = Vec::new();
    for i in 0..20 {
        let app = t.app.clone();
        handles.push(tokio::spawn(async move {
            send_to(
                app,
                "POST",
                "/api/jobs/claim",
                Some(json!({"claimant": format!("c{i}"), "limit": 3})),
                Some(TOKEN),
                &[],
            )
            .await
        }))
    }
    let mut granted: Vec<String> = Vec::new();
    for h in handles {
        let (st, v) = h.await.expect("task");
        assert_eq!(st, StatusCode::OK, "{v}");
        for g in v.as_array().expect("array") {
            granted.push(s(&g["job"]["topic_name"]).to_string());
        }
    }
    // Each job at most once, and exactly one job per overlapping group.
    let mut uniq = granted.clone();
    uniq.sort();
    uniq.dedup();
    assert_eq!(
        uniq.len(),
        granted.len(),
        "a job was granted twice: {granted:?}"
    );
    assert_eq!(granted.len(), 4, "{granted:?}");
    for g in 1..=4 {
        let n = granted
            .iter()
            .filter(|x| x.starts_with(&format!("g{g}-")))
            .count();
        assert_eq!(n, 1, "group {g} granted {n} jobs: {granted:?}");
    }
    t.cleanup().await;
}

// ------------------------------------------------------------------ nonce, rounds

#[tokio::test]
async fn the_nonce_is_returned_once_and_never_readable() {
    let t = TestApp::new().await;
    mk(&t, "nonce-job", &[p("nonce-proj")], &[]).await;
    let got = claim(&t, 1).await;
    let nonce = nonce_of(&got[0]);
    assert_eq!(nonce.len(), 64);
    assert_eq!(got[0]["replayed"], false);

    let id = got[0]["job"]["id"].as_i64().expect("id").to_string();
    let mut reads = vec![
        t.get(&format!("/api/jobs/{id}")).await.1,
        t.get("/api/jobs").await.1,
        t.get("/api/jobs?claimed_by=orch").await.1,
        t.get("/api/topics/nonce-job/events").await.1,
        t.get("/api/topics/nonce-job/messages").await.1,
        t.get("/api/topics/nonce-job").await.1,
        t.get("/api/stats").await.1,
    ];
    let (_, m) = t.get("/metrics").await;
    reads.push(m);
    for r in &reads {
        let text = r.to_string();
        assert!(!text.contains(&nonce), "nonce leaked: {text}");
        assert!(!text.contains(&common_hash(&nonce)), "hash leaked");
        assert!(!text.contains("claim_nonce"), "{text}");
    }

    // Wrong / missing nonce is a 409; reclaim invalidates the old nonce.
    let (st, _) = complete(&t, "nonce-job", "not-the-nonce", 1, "done").await;
    assert_eq!(st, StatusCode::CONFLICT);
    let (st, _) = t
        .write(
            "POST",
            "/api/jobs/nonce-job/complete",
            json!({"round": 1, "status": "done", "worker": worker(), "result": "r", "author": "a"}),
        )
        .await;
    assert_eq!(st, StatusCode::CONFLICT);
    t.write(
        "POST",
        "/api/jobs/nonce-job/reclaim",
        json!({"actor": "orch", "reason": "restart"}),
    )
    .await;
    let again = claim(&t, 1).await;
    let fresh = nonce_of(&again[0]);
    assert_ne!(fresh, nonce);
    let (st, _) = complete(&t, "nonce-job", &nonce, 1, "done").await;
    assert_eq!(st, StatusCode::CONFLICT, "stale nonce must not complete");
    let (st, v) = complete(&t, "nonce-job", &fresh, 1, "done").await;
    assert_eq!(st, StatusCode::OK, "{v}");
    t.cleanup().await;
}

fn common_hash(nonce: &str) -> String {
    hive_board::util::sha256_hex(nonce)
}

#[tokio::test]
async fn round_fencing_and_idempotent_complete() {
    let t = TestApp::new().await;
    mk(&t, "round-job", &[p("round-proj")], &[]).await;
    let got = claim(&t, 1).await;
    let nonce = nonce_of(&got[0]);
    assert_eq!(got[0]["job"]["round"], 1);

    let (st, first) = complete(&t, "round-job", &nonce, 1, "done").await;
    assert_eq!(st, StatusCode::OK, "{first}");
    // Same nonce + same round + same status: the original, no second result message.
    let (st, again) = complete(&t, "round-job", &nonce, 1, "done").await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(again["result_message_id"], first["result_message_id"]);
    let (_, msgs) = t.get("/api/topics/round-job/messages").await;
    assert_eq!(
        msgs.as_array()
            .expect("m")
            .iter()
            .filter(|m| m["kind"] == "result")
            .count(),
        1
    );
    // Same round, different status: 409.
    let (st, _) = complete(&t, "round-job", &nonce, 1, "failed").await;
    assert_eq!(st, StatusCode::CONFLICT);

    // A failed verification returns the job to the same worker at round 2.
    let (st, v) = verify_with(&t, "round-job", "fail", reviewer()).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["status"], "claimed");
    assert_eq!(v["round"], 2);
    assert_eq!(v["verify_fails"], 1);
    // The stale round-1 completion is fenced off; round 2 with the kept nonce works.
    let (st, _) = complete(&t, "round-job", &nonce, 1, "done").await;
    assert_eq!(st, StatusCode::CONFLICT);
    let (st, v) = complete(&t, "round-job", &nonce, 2, "done").await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let (st, v) = verify_with(&t, "round-job", "pass", reviewer()).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["status"], "verified");
    assert_eq!(v["verify_fails"], 1); // not a first-pass verification
    t.cleanup().await;
}

// ------------------------------------------------------------------ verifier rules

#[tokio::test]
async fn verifier_rules_verdict_finding_and_view_exclusion() {
    let t = TestApp::new().await;
    mk(&t, "ver-job", &[p("ver-proj")], &[]).await;
    let got = claim(&t, 1).await;
    complete(&t, "ver-job", &nonce_of(&got[0]), 1, "done").await;

    // No explicit verifier identity: 400.
    let (st, _) = t
        .write(
            "POST",
            "/api/jobs/ver-job/verify",
            json!({"verdict": "pass", "author": "orch", "agent": reviewer()}),
        )
        .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    // The worker's own identity: 409.
    let (st, _) = verify_with(&t, "ver-job", "pass", worker()).await;
    assert_eq!(st, StatusCode::CONFLICT);
    // Bad verdict value: 400.
    let (st, _) = verify_with(&t, "ver-job", "maybe", reviewer()).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    // A different identity on the SAME harness passes for a non-security job.
    let (st, v) = verify_with(&t, "ver-job", "pass", reviewer()).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["status"], "verified");
    assert!(v["verdict_message_id"].is_i64());

    // The verdict is a finding about the worker, excluded from identity_quality.
    let (_, fl) = t.get("/api/findings?topic=ver-job&kind=verdict").await;
    assert_eq!(fl.as_array().expect("f").len(), 1);
    assert!(fl[0]["about_identity_id"].is_i64());
    assert!(fl[0]["claim"].as_str().expect("c").contains("PASS"));
    let (_, lb) = t.get("/api/leaderboard").await;
    let text = lb.to_string();
    assert!(
        !text.contains("rust-reviewer") && !text.contains("rust-builder"),
        "{text}"
    );
    assert_eq!(lb.as_array().expect("lb").len(), 0);
    // The worker identity is an object on the job.
    assert_eq!(v["worker_identity"]["model"], "sonnet");
    t.cleanup().await;
}

#[tokio::test]
async fn failed_verification_notes_the_dependents() {
    let t = TestApp::new().await;
    create(
        &t,
        json!([
            job("base-job", &[p("base")], &[]),
            job("top-job", &[p("top")], &["base-job"]),
        ]),
    )
    .await;
    let got = claim(&t, 1).await;
    complete(&t, "base-job", &nonce_of(&got[0]), 1, "done").await;
    verify_with(&t, "base-job", "fail", reviewer()).await;
    let (_, ev) = t.get("/api/topics/top-job/events").await;
    assert!(ev
        .as_array()
        .expect("e")
        .iter()
        .any(|e| e["event"] == "dependency-reopened"));
    t.cleanup().await;
}

// ------------------------------------------------------------------ lifecycle guards

async fn age_topic(t: &TestApp, name: &str, hours: i32) {
    sqlx::query(
        "UPDATE topics SET last_activity_at = now() - make_interval(hours => $2), \
         updated_at = now() - make_interval(hours => $2) WHERE name = $1",
    )
    .bind(name)
    .bind(hours)
    .execute(&t.pool)
    .await
    .expect("age");
}

#[tokio::test]
async fn sweep_and_manual_resolve_leave_live_job_topics_alone() {
    let t = TestApp::new().await;
    mk(&t, "guard-job", &[p("guard")], &[]).await;
    age_topic(&t, "guard-job", 200).await;
    let sweep = |dry: bool| json!({"dry_run": dry, "actor": "gardener"});
    for stage in ["open", "claimed", "done"] {
        match stage {
            "claimed" => {
                let got = claim(&t, 1).await;
                let n = nonce_of(&got[0]);
                t.write("POST", "/api/jobs/guard-job/reclaim", json!({"actor": "o"}))
                    .await;
                // re-claim to get a nonce we can finish with
                let got = claim(&t, 1).await;
                let _ = n;
                let _ = got;
            }
            "done" => {
                // Rebuild a done job from the claimed one.
                t.write("POST", "/api/jobs/guard-job/reclaim", json!({"actor": "o"}))
                    .await;
                let got = claim(&t, 1).await;
                complete(&t, "guard-job", &nonce_of(&got[0]), 1, "done").await;
            }
            _ => {}
        }
        age_topic(&t, "guard-job", 200).await;
        let (st, r) = t
            .write("POST", "/api/maintenance/sweep", sweep(false))
            .await;
        assert_eq!(st, StatusCode::OK);
        assert!(
            r["changes"].as_array().expect("c").is_empty(),
            "{stage}: {r}"
        );
        let (_, topic) = t.get("/api/topics/guard-job").await;
        assert_eq!(topic["status"], "active", "{stage}");
        let (st, _) = t
            .write(
                "PATCH",
                "/api/topics/guard-job",
                json!({"status": "resolved", "actor": "a"}),
            )
            .await;
        assert_eq!(st, StatusCode::CONFLICT, "manual resolve while {stage}");
    }
    // Cancelled: the normal lifecycle applies again, and retry reopens a resolved topic.
    t.write("POST", "/api/jobs/guard-job/cancel", json!({"actor": "o"}))
        .await;
    age_topic(&t, "guard-job", 200).await;
    let (_, r) = t
        .write("POST", "/api/maintenance/sweep", sweep(false))
        .await;
    assert!(!r["changes"].as_array().expect("c").is_empty(), "{r}");
    let (_, topic) = t.get("/api/topics/guard-job").await;
    assert_eq!(topic["status"], "resolved");
    let (st, v) = t
        .write("POST", "/api/jobs/guard-job/retry", json!({"actor": "o"}))
        .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let (_, topic) = t.get("/api/topics/guard-job").await;
    assert_eq!(topic["status"], "active");
    t.cleanup().await;
}

#[tokio::test]
async fn reclaim_reopens_a_resolved_job_topic() {
    let t = TestApp::new().await;
    mk(&t, "reopen-job", &[p("reopen")], &[]).await;
    claim(&t, 1).await;
    // Force the topic closed under the job (as a stale restart might leave it).
    sqlx::query("UPDATE topics SET status = 'resolved' WHERE name = 'reopen-job'")
        .execute(&t.pool)
        .await
        .expect("force");
    let (st, v) = t
        .write(
            "POST",
            "/api/jobs/reopen-job/reclaim",
            json!({"actor": "o", "reason": "restart"}),
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let (_, topic) = t.get("/api/topics/reopen-job").await;
    assert_eq!(topic["status"], "active");
    t.cleanup().await;
}

// ------------------------------------------------------------------ create, patch, amend, delete

#[tokio::test]
async fn batch_create_is_all_or_nothing_with_in_batch_names_and_cycle_check() {
    let t = TestApp::new().await;
    // Forward references by name work.
    let made = create(
        &t,
        json!([
            job("wave-2", &[p("w2")], &["wave-1"]),
            job("wave-1", &[p("w1")], &[]),
        ]),
    )
    .await;
    assert_eq!(made.len(), 2);
    let w1 = getj(&t, "wave-1").await["id"].clone();
    assert_eq!(made[0]["depends_on"], json!([w1]));

    // A cycle inside a batch: 409 and nothing is created.
    let (st, v) = create_raw(
        &t,
        json!([
            job("cyc-a", &[p("ca")], &["cyc-b"]),
            job("cyc-b", &[p("cb")], &["cyc-a"])
        ]),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT, "{v}");
    let (st, _) = t.get("/api/topics/cyc-a").await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    // Duplicate names, existing names, unknown deps, oversize batch, empty batch.
    let (st, _) = create_raw(
        &t,
        json!([job("dup-x", &[p("dx")], &[]), job("dup-x", &[p("dy")], &[])]),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    let (st, _) = create_raw(&t, json!([job("wave-1", &[p("zz")], &[])])).await;
    assert_eq!(st, StatusCode::CONFLICT);
    let (st, _) = create_raw(&t, json!([job("lonely", &[p("lo")], &["no-such-job"])])).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    let many: Vec<Value> = (0..51)
        .map(|i| job(&format!("bulk-{i}"), &[p(&format!("b{i}"))], &[]))
        .collect();
    let (st, _) = create_raw(&t, json!(many)).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    let (st, _) = create_raw(&t, json!([])).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    // A job topic cannot be made through the plain topics route.
    let (st, _) = t
        .write(
            "POST",
            "/api/topics",
            json!({"name": "plain-job", "title": "t", "author": "a", "kind": "job"}),
        )
        .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    t.cleanup().await;
}

#[tokio::test]
async fn idempotency_keys_on_create_and_claim() {
    let t = TestApp::new().await;
    let body = json!({"jobs": [job("idem-job", &[p("idem")], &[])], "author": "orch", "agent": orchestrator()});
    let hdr = [("idempotency-key", "k-create")];
    let (s1, v1) = common::send_to(
        t.app.clone(),
        "POST",
        "/api/jobs",
        Some(body.clone()),
        Some(TOKEN),
        &hdr,
    )
    .await;
    let (s2, v2) = common::send_to(
        t.app.clone(),
        "POST",
        "/api/jobs",
        Some(body),
        Some(TOKEN),
        &hdr,
    )
    .await;
    assert_eq!(s1, StatusCode::CREATED);
    assert_eq!(s2, StatusCode::OK);
    assert_eq!(v1[0]["id"], v2[0]["id"]);

    let cl = json!({"claimant": "orch", "limit": 1});
    let h2 = [("idempotency-key", "k-claim")];
    let (_, c1) = common::send_to(
        t.app.clone(),
        "POST",
        "/api/jobs/claim",
        Some(cl.clone()),
        Some(TOKEN),
        &h2,
    )
    .await;
    let (_, c2) = common::send_to(
        t.app.clone(),
        "POST",
        "/api/jobs/claim",
        Some(cl),
        Some(TOKEN),
        &h2,
    )
    .await;
    assert!(c1[0]["nonce"].is_string());
    assert_eq!(c2[0]["job"]["id"], c1[0]["job"]["id"]);
    // The nonce is never stored, so a replay cannot show it again.
    assert!(c2[0]["nonce"].is_null());
    assert_eq!(c2[0]["replayed"], true);
    t.cleanup().await;
}

#[tokio::test]
async fn patch_rules_cycles_and_addressing() {
    let t = TestApp::new().await;
    create(
        &t,
        json!([
            job("pa", &[p("pa")], &[]),
            job("pb", &[p("pb")], &["pa"]),
            job("pc", &[p("pc")], &[]),
        ]),
    )
    .await;
    // A cycle through PATCH is rejected.
    let (st, _) = t
        .write(
            "PATCH",
            "/api/jobs/pa",
            json!({"depends_on": ["pb"], "actor": "o"}),
        )
        .await;
    assert_eq!(st, StatusCode::CONFLICT);
    let (st, _) = t
        .write(
            "PATCH",
            "/api/jobs/pa",
            json!({"depends_on": ["pa"], "actor": "o"}),
        )
        .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    let (st, _) = t
        .write(
            "PATCH",
            "/api/jobs/pa",
            json!({"depends_on": ["nope-nope"], "actor": "o"}),
        )
        .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    // Rewiring an open job works; addressable by id as well as slug.
    let pa_id = getj(&t, "pa").await["id"].as_i64().expect("id");
    let (st, v) = t
        .write(
            "PATCH",
            "/api/jobs/pc",
            json!({"depends_on": [pa_id.to_string()], "priority": 7, "actor": "o"}),
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["priority"], 7);
    assert_eq!(v["depends_on"], json!([pa_id]));
    let (st, _) = t
        .write(
            "PATCH",
            "/api/jobs/pc",
            json!({"owns": ["/etc"], "actor": "o"}),
        )
        .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    let (st, _) = t
        .write("PATCH", "/api/jobs/pc", json!({"actor": "o"}))
        .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    // `?job=` fetches one by slug or id; unknown is 404.
    let (_, one) = t.get("/api/jobs?job=pb").await;
    assert_eq!(one.as_array().expect("o").len(), 1);
    let (_, one) = t.get(&format!("/api/jobs?job={pa_id}")).await;
    assert_eq!(one[0]["topic_name"], "pa");
    let (st, _) = t.get("/api/jobs?job=ghost-job").await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    // Shape the clients rely on.
    let v = getj(&t, "pb").await;
    assert_eq!(v["title"], "Job pb");
    assert_eq!(v["topic_name"], "pb");
    assert!(v["depends_on"][0].is_i64());
    t.cleanup().await;
}

#[tokio::test]
async fn amendments_are_messages_and_the_spec_is_immutable() {
    let t = TestApp::new().await;
    mk(&t, "amend-job", &[p("amend")], &[]).await;
    let (st, m) = t
        .write(
            "POST",
            "/api/jobs/amend-job/amend",
            json!({"author": "orch", "agent": orchestrator(), "text": "also handle X"}),
        )
        .await;
    assert_eq!(st, StatusCode::CREATED, "{m}");
    assert_eq!(m["kind"], "amendment");
    assert_eq!(getj(&t, "amend-job").await["spec"], "do the thing");
    t.cleanup().await;
}

#[tokio::test]
async fn deleting_a_job_topic_names_its_dependents() {
    let t = TestApp::new().await;
    create(
        &t,
        json!([
            job("del-base", &[p("db")], &[]),
            job("del-top", &[p("dt")], &["del-base"])
        ]),
    )
    .await;
    let (st, v) = t
        .send(
            "DELETE",
            "/api/topics/del-base?confirm=del-base",
            None,
            Some(TOKEN),
            &[],
        )
        .await;
    assert_eq!(st, StatusCode::CONFLICT, "{v}");
    assert!(s(&v["error"]).contains("del-top"), "{v}");
    let (st, _) = t
        .send(
            "DELETE",
            "/api/topics/del-top?confirm=del-top",
            None,
            Some(TOKEN),
            &[],
        )
        .await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    let (st, _) = t
        .send(
            "DELETE",
            "/api/topics/del-base?confirm=del-base",
            None,
            Some(TOKEN),
            &[],
        )
        .await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    let (st, _) = t.get("/api/jobs/del-base").await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    t.cleanup().await;
}

// ------------------------------------------------------------------ metrics, stats, stale

#[tokio::test]
async fn metrics_stats_and_stale() {
    let t = TestApp::new().await;
    create(
        &t,
        json!([
            job("m-a", &[p("ma")], &[]),
            job("m-b", &[p("mb")], &["m-a"]),
            job("m-c", &[p("mc")], &[]),
        ]),
    )
    .await;
    let (_, stats) = t.get("/api/stats").await;
    assert_eq!(stats["ready_jobs"], 2);
    assert_eq!(stats["stale_jobs"], 0);
    let got = claim(&t, 1).await;
    assert_eq!(got.len(), 1);
    let claimed = s(&got[0]["job"]["topic_name"]).to_string();
    // Age the claim past HIVE_JOB_STALE_H with no topic activity.
    sqlx::query("UPDATE jobs SET claimed_at = now() - interval '7 hours' WHERE status = 'claimed'")
        .execute(&t.pool)
        .await
        .expect("age claim");
    age_topic(&t, &claimed, 3).await;
    let v = getj(&t, &claimed).await;
    assert_eq!(v["stale"], true);
    assert!(v["claim_age_s"].as_f64().expect("age") > 6.0 * 3600.0);
    let (_, stats) = t.get("/api/stats").await;
    assert_eq!(stats["stale_jobs"], 1);
    let (_, text) = t.get("/metrics").await;
    let text = text.as_str().expect("t").to_string();
    for needle in [
        "hive_jobs{status=\"claimed\"} 1",
        "hive_jobs{status=\"open\"} 2",
        "hive_jobs{status=\"verified\"} 0",
        "hive_jobs_ready ",
        "hive_jobs_blocked 0",
        "hive_jobs_stale 1",
        "hive_job_claim_age_seconds_max ",
    ] {
        assert!(text.contains(needle), "missing {needle}\n{text}");
    }
    t.cleanup().await;
}

#[tokio::test]
async fn blank_or_unknown_worker_and_verifier_identities_are_refused() {
    let t = TestApp::new().await;
    mk(&t, "id-job", &[p("id-proj")], &[]).await;
    let got = claim(&t, 1).await;
    let nonce = nonce_of(&got[0]);
    for bad in [
        json!({"agent_type": "", "model": "sonnet", "harness": "claude"}),
        json!({"agent_type": "rust-builder", "model": "  ", "harness": "claude"}),
        json!({"agent_type": "rust-builder", "model": "sonnet", "harness": "Unknown"}),
        json!({"agent_type": "UNKNOWN", "model": "sonnet", "harness": "claude"}),
    ] {
        let (st, v) = t
            .write(
                "POST",
                "/api/jobs/id-job/complete",
                json!({"nonce": nonce, "round": 1, "status": "done", "worker": bad,
                       "result": "r", "author": "a"}),
            )
            .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "worker {bad}: {v}");
    }
    let (st, v) = complete(&t, "id-job", &nonce, 1, "done").await;
    assert_eq!(st, StatusCode::OK, "{v}");
    for bad in [
        json!({"agent_type": "", "model": "", "harness": ""}),
        json!({"agent_type": "reviewer", "model": "opus", "harness": "unknown"}),
        json!({"agent_type": "reviewer", "model": "Unknown", "harness": "agy"}),
    ] {
        let (st, v) = verify_with(&t, "id-job", "pass", bad.clone()).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "verifier {bad}: {v}");
    }
    assert_eq!(getj(&t, "id-job").await["status"], "done");
    t.cleanup().await;
}

#[tokio::test]
async fn job_results_reach_distill_input() {
    let t = TestApp::new().await;
    mk(&t, "dist-job", &[p("dist-proj")], &[]).await;
    let got = claim(&t, 1).await;
    let nonce = nonce_of(&got[0]);
    let (st, v) = complete(&t, "dist-job", &nonce, 1, "done").await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let (_, input) = t.get("/api/topics/dist-job/distill-input").await;
    assert!(
        input["messages"]
            .as_array()
            .expect("m")
            .iter()
            .any(|m| m["kind"] == "result"),
        "{input}"
    );
    t.cleanup().await;
}

#[tokio::test]
async fn a_repeated_failed_completion_is_idempotent() {
    let t = TestApp::new().await;
    mk(&t, "fail-job", &[p("fail-proj")], &[]).await;
    let got = claim(&t, 1).await;
    let nonce = nonce_of(&got[0]);
    let (st, first) = complete(&t, "fail-job", &nonce, 1, "failed").await;
    assert_eq!(st, StatusCode::OK, "{first}");
    let (st, again) = complete(&t, "fail-job", &nonce, 1, "failed").await;
    assert_eq!(st, StatusCode::OK, "{again}");
    assert_eq!(again["result_message_id"], first["result_message_id"]);
    let (_, msgs) = t.get("/api/topics/fail-job/messages").await;
    assert_eq!(
        msgs.as_array()
            .expect("m")
            .iter()
            .filter(|m| m["kind"] == "result")
            .count(),
        1
    );
    // A different status or a wrong nonce is still a 409.
    assert_eq!(
        complete(&t, "fail-job", &nonce, 1, "done").await.0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        complete(&t, "fail-job", "other", 1, "failed").await.0,
        StatusCode::CONFLICT
    );
    t.cleanup().await;
}

#[tokio::test]
async fn first_pass_follows_verify_fails_and_verdicts_are_not_open_work() {
    let t = TestApp::new().await;
    mk(&t, "fp-clean", &[p("fp-clean")], &[]).await;
    mk(&t, "fp-rework", &[p("fp-rework")], &[]).await;
    let got = claim(&t, 10).await;
    let nonce = |n: &str| {
        got.iter()
            .find(|g| g["job"]["topic_name"] == n)
            .map(nonce_of)
            .expect("nonce")
    };
    complete(&t, "fp-clean", &nonce("fp-clean"), 1, "done").await;
    verify_with(&t, "fp-clean", "pass", reviewer()).await;
    complete(&t, "fp-rework", &nonce("fp-rework"), 1, "done").await;
    verify_with(&t, "fp-rework", "fail", reviewer()).await;
    complete(&t, "fp-rework", &nonce("fp-rework"), 2, "done").await;
    verify_with(&t, "fp-rework", "pass", reviewer()).await;
    let first_pass = |ev: &Value| {
        ev.as_array()
            .expect("e")
            .iter()
            .find(|e| e["event"] == "job-verified")
            .map(|e| e["detail"]["first_pass"].clone())
            .expect("job-verified event")
    };
    let (_, e1) = t.get("/api/topics/fp-clean/events").await;
    let (_, e2) = t.get("/api/topics/fp-rework/events").await;
    assert_eq!(first_pass(&e1), true);
    assert_eq!(first_pass(&e2), false);

    // Verdict findings (about_identity_id set) are not open work anywhere.
    let (_, fl) = t.get("/api/findings?kind=verdict").await;
    assert_eq!(fl.as_array().expect("f").len(), 3);
    let (_, stats) = t.get("/api/stats").await;
    assert_eq!(stats["open_findings"], 0);
    let (_, topic) = t.get("/api/topics/fp-rework").await;
    assert_eq!(topic["open_findings"], 0);
    let (_, text) = t.get("/metrics").await;
    assert!(text
        .as_str()
        .expect("t")
        .contains("hive_findings{status=\"open\"} 0"));
    t.cleanup().await;
}

#[tokio::test]
async fn patch_event_flags_dependency_changes_when_owns_changes_too() {
    let t = TestApp::new().await;
    create(
        &t,
        json!([
            job("pe-a", &[p("pe-a")], &[]),
            job("pe-b", &[p("pe-b")], &[])
        ]),
    )
    .await;
    let (st, v) = t
        .write(
            "PATCH",
            "/api/jobs/pe-b",
            json!({"depends_on": ["pe-a"], "owns": [p("pe-b2")], "actor": "o"}),
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let (_, ev) = t.get("/api/topics/pe-b/events").await;
    let patched = ev
        .as_array()
        .expect("e")
        .iter()
        .find(|e| e["event"] == "job-patched")
        .expect("patched");
    assert_eq!(patched["detail"]["depends_changed"], true);
    t.cleanup().await;
}

#[tokio::test]
async fn reserved_names_and_reserved_kv_keys() {
    let t = TestApp::new().await;
    let (st, _) = create_raw(&t, json!([job("claim", &[p("claim")], &[])])).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    let (st, _) = t
        .write(
            "POST",
            "/api/topics",
            json!({"name": "claim", "title": "t", "author": "a"}),
        )
        .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    let (st, _) = t
        .write("PUT", "/api/kv/idem:jobs-claim:k", json!({"v": []}))
        .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    let (st, _) = t.write("PUT", "/api/kv/ordinary", json!({"v": 1})).await;
    assert_eq!(st, StatusCode::OK);
    t.cleanup().await;
}

#[tokio::test]
async fn a_done_job_left_unverified_goes_stale() {
    let t = TestApp::new().await;
    mk(&t, "dstale", &[p("dstale")], &[]).await;
    let got = claim(&t, 1).await;
    complete(&t, "dstale", &nonce_of(&got[0]), 1, "done").await;
    assert_eq!(getj(&t, "dstale").await["stale"], false);
    sqlx::query("UPDATE jobs SET completed_at = now() - interval '7 hours' WHERE status = 'done'")
        .execute(&t.pool)
        .await
        .expect("age");
    assert_eq!(getj(&t, "dstale").await["stale"], true);
    let (_, stats) = t.get("/api/stats").await;
    assert_eq!(stats["stale_jobs"], 1);
    t.cleanup().await;
}

#[tokio::test]
async fn migration_0002_constraints_and_message_kinds() {
    let t = TestApp::new().await;
    // The kind CHECKs were replaced under their generated names.
    let names: Vec<String> = sqlx::query_scalar(
        "SELECT conname FROM pg_constraint WHERE conname IN ('topics_kind_check','messages_kind_check') ORDER BY 1",
    )
    .fetch_all(&t.pool)
    .await
    .expect("names");
    assert_eq!(names, vec!["messages_kind_check", "topics_kind_check"]);
    // `result` and `amendment` are not user-postable kinds.
    t.topic("kind-topic").await;
    let (st, _) = t
        .write(
            "POST",
            "/api/topics/kind-topic/messages",
            json!({"author": "a", "content": "x", "kind": "result"}),
        )
        .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    t.cleanup().await;
}
