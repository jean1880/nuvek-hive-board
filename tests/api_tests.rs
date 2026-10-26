mod common;

use axum::http::StatusCode;
use common::{TestApp, TOKEN_NEXT};
use serde_json::{json, Value};

fn id_of(v: &Value) -> i64 {
    v["id"].as_i64().expect("numeric id")
}

#[tokio::test]
async fn migrations_apply_and_are_idempotent() {
    let t = TestApp::new().await;
    // A second run is a no-op.
    hive_board::migrate(&t.pool).await.expect("re-migrate");
    let n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM information_schema.tables WHERE table_schema = 'public' \
         AND table_name IN ('topics','messages','findings','digests','topic_events','proposals')",
    )
    .fetch_one(&t.pool)
    .await
    .expect("count");
    assert_eq!(n, 6);
    let (s, v) = t.get("/readyz").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["db"], "ok");
    t.cleanup().await;
}

#[tokio::test]
async fn create_list_post_thread() {
    let t = TestApp::new().await;
    let topic = t.topic("alpha-topic").await;
    assert_eq!(topic["status"], "active");
    assert_eq!(topic["message_count"], 0);

    // Duplicate name and invalid name.
    let (s, v) = t
        .write(
            "POST",
            "/api/topics",
            json!({"name": "alpha-topic", "title": "x", "author": "a"}),
        )
        .await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert_eq!(v["code"], "conflict");
    let (s, v) = t
        .write(
            "POST",
            "/api/topics",
            json!({"name": "1bad", "title": "x", "author": "a"}),
        )
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(v["code"], "validation");

    let root = t
        .post(
            "alpha-topic",
            json!({"author": "Atlas", "content": "root post", "tags": ["x"]}),
        )
        .await;
    let reply = t
        .post(
            "alpha-topic",
            json!({"author": "Bo", "content": "a reply", "parent_id": id_of(&root)}),
        )
        .await;
    assert_eq!(reply["parent_id"], root["id"]);
    assert_eq!(reply["topic_name"], "alpha-topic");

    let (s, v) = t.get("/api/topics/alpha-topic/threads").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v.as_array().expect("array").len(), 1);
    assert_eq!(v[0]["replies"].as_array().expect("replies").len(), 1);

    let (_, v) = t
        .get(&format!(
            "/api/topics/alpha-topic/messages?after={}",
            id_of(&root)
        ))
        .await;
    assert_eq!(v.as_array().expect("array").len(), 1);

    // Numeric id also resolves; unknown topic is a typed 404.
    let (s, _) = t.get(&format!("/api/topics/{}", id_of(&topic))).await;
    assert_eq!(s, StatusCode::OK);
    let (s, v) = t.get("/api/topics/nope-nope").await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(v["code"], "not_found");

    let (s, v) = t.get("/api/topics").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v[0]["message_count"], 2);
    t.cleanup().await;
}

#[tokio::test]
async fn typed_post_creates_and_reinforces_finding() {
    let t = TestApp::new().await;
    t.topic("find-topic").await;
    let m1 = t
        .post(
            "find-topic",
            json!({"author": "rev", "kind": "finding", "severity": "BLOCKER",
                   "content": "The   cache is NOT invalidated", "subjects": ["hive-board"]}),
        )
        .await;
    let (_, list) = t.get("/api/findings").await;
    assert_eq!(list.as_array().expect("array").len(), 1);
    assert_eq!(list[0]["reinforce_count"], 1);
    assert_eq!(list[0]["message_id"], m1["id"]);
    let first_updated = list[0]["updated_at"].as_str().expect("ts").to_string();

    // Same claim, different case and spacing: reinforces instead of duplicating.
    t.post(
        "find-topic",
        json!({"author": "rev2", "kind": "finding", "severity": "BLOCKER",
               "content": "the cache is not invalidated"}),
    )
    .await;
    let (_, list) = t.get("/api/findings").await;
    assert_eq!(list.as_array().expect("array").len(), 1);
    assert_eq!(list[0]["reinforce_count"], 2);
    assert!(list[0]["updated_at"].as_str().expect("ts") >= first_updated.as_str());

    // A plain note creates no finding.
    t.post(
        "find-topic",
        json!({"author": "x", "content": "just a note"}),
    )
    .await;
    let (_, list) = t.get("/api/findings").await;
    assert_eq!(list.as_array().expect("array").len(), 1);

    let (_, by_subject) = t.get("/api/findings?subject=hive-board").await;
    assert_eq!(by_subject.as_array().expect("array").len(), 1);
    t.cleanup().await;
}

#[tokio::test]
async fn finding_transitions_are_enforced() {
    let t = TestApp::new().await;
    t.topic("tr-topic").await;
    t.post(
        "tr-topic",
        json!({"author": "a", "kind": "gotcha", "severity": "NICE", "content": "a gotcha"}),
    )
    .await;
    let (_, list) = t.get("/api/findings").await;
    let fid = id_of(&list[0]);

    // The gardener cannot judge.
    let (s, v) = t
        .write(
            "PATCH",
            &format!("/api/findings/{fid}"),
            json!({"status": "confirmed", "actor": "gardener", "actor_class": "gardener"}),
        )
        .await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");

    let (s, v) = t
        .write(
            "PATCH",
            &format!("/api/findings/{fid}"),
            json!({"status": "confirmed", "actor": "orchestrator-1", "actor_class": "orchestrator", "note": "verified"}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["status"], "confirmed");

    // confirmed -> false_positive is illegal.
    let (s, _) = t
        .write(
            "PATCH",
            &format!("/api/findings/{fid}"),
            json!({"status": "false_positive", "actor": "orchestrator-1", "actor_class": "orchestrator"}),
        )
        .await;
    assert_eq!(s, StatusCode::CONFLICT);

    // Gardener metadata updates are fine.
    let (s, _) = t
        .write(
            "PATCH",
            &format!("/api/findings/{fid}"),
            json!({"actor": "gardener", "actor_class": "gardener", "graph_synced": true,
                   "resolved_subjects": [{"label": "Repository", "name": "hive-board"}]}),
        )
        .await;
    assert_eq!(s, StatusCode::OK);

    let (_, detail) = t.get(&format!("/api/findings/{fid}")).await;
    assert_eq!(detail["events"].as_array().expect("events").len(), 1);
    assert_eq!(detail["events"][0]["to_status"], "confirmed");
    assert!(detail["graph_synced_at"].is_string());

    let (s, v) = t
        .write(
            "PATCH",
            &format!("/api/findings/{fid}"),
            json!({"status": "bogus", "actor": "a", "actor_class": "human"}),
        )
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    t.cleanup().await;
}

#[tokio::test]
async fn auth_guards_writes_and_accepts_next_token() {
    let t = TestApp::new().await;
    let body = json!({"name": "auth-topic", "title": "T", "author": "a"});
    let (s, v) = t
        .send("POST", "/api/topics", Some(body.clone()), None, &[])
        .await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    assert_eq!(v, json!({"error": "unauthorized", "code": "unauthorized"}));
    let (s, _) = t
        .send(
            "POST",
            "/api/topics",
            Some(body.clone()),
            Some("wrong-token"),
            &[],
        )
        .await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, _) = t
        .send("POST", "/api/topics", Some(body), Some(TOKEN_NEXT), &[])
        .await;
    assert_eq!(s, StatusCode::CREATED);
    // Reads stay open.
    let (s, _) = t.get("/api/topics").await;
    assert_eq!(s, StatusCode::OK);
    // DELETE is a write too.
    let (s, _) = t
        .send("DELETE", "/api/topics/auth-topic", None, None, &[])
        .await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    t.cleanup().await;
}

#[tokio::test]
async fn idempotent_post_replays_the_original() {
    let t = TestApp::new().await;
    t.topic("idem-topic").await;
    let body = json!({"author": "a", "content": "once only"});
    let uri = "/api/topics/idem-topic/messages";
    let hdr = [("idempotency-key", "k-123")];
    let (s1, m1) = t
        .send("POST", uri, Some(body.clone()), Some(common::TOKEN), &hdr)
        .await;
    let (s2, m2) = t
        .send("POST", uri, Some(body), Some(common::TOKEN), &hdr)
        .await;
    assert_eq!(s1, StatusCode::CREATED);
    assert_eq!(s2, StatusCode::OK);
    assert_eq!(m1["id"], m2["id"]);
    let (_, list) = t.get(uri).await;
    assert_eq!(list.as_array().expect("array").len(), 1);
    t.cleanup().await;
}

#[tokio::test]
async fn post_response_carries_finding_id_and_warnings() {
    let t = TestApp::new().await;
    t.topic("resp-topic").await;
    let uri = "/api/topics/resp-topic/messages";

    // Typed post without evidence: accepted, tracked, with a warning.
    let m = t
        .post(
            "resp-topic",
            json!({"author": "a", "kind": "finding", "severity": "SHOULD", "content": "no proof"}),
        )
        .await;
    assert!(m["id"].is_i64());
    assert_eq!(m["kind"], "finding");
    let (_, list) = t.get("/api/findings").await;
    assert_eq!(m["finding_id"], list[0]["id"]);
    assert_eq!(
        m["warnings"],
        json!(["evidence missing: add a command and its output or file:line"])
    );

    // With evidence: no warnings.
    let m = t
        .post(
            "resp-topic",
            json!({"author": "a", "kind": "gotcha", "severity": "NICE",
                   "content": "with proof", "evidence": "ls -l"}),
        )
        .await;
    assert!(m["finding_id"].is_i64());
    assert_eq!(m["warnings"], json!([]));

    // Untyped kind: null finding id.
    let m = t
        .post("resp-topic", json!({"author": "a", "content": "n"}))
        .await;
    assert!(m["finding_id"].is_null());
    assert_eq!(m["warnings"], json!([]));

    // Optional severity on question/decision.
    let (s, _) = t
        .write(
            "POST",
            uri,
            json!({"author": "a", "kind": "question", "content": "why?"}),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED);

    // Replay returns the same finding id and no warnings.
    let body =
        json!({"author": "a", "kind": "finding", "severity": "BLOCKER", "content": "replayed"});
    let hdr = [("idempotency-key", "k-find")];
    let (s1, r1) = t
        .send("POST", uri, Some(body.clone()), Some(common::TOKEN), &hdr)
        .await;
    let (s2, r2) = t
        .send("POST", uri, Some(body), Some(common::TOKEN), &hdr)
        .await;
    assert_eq!(s1, StatusCode::CREATED);
    assert_eq!(s2, StatusCode::OK);
    assert!(r1["finding_id"].is_i64());
    assert_eq!(r1["finding_id"], r2["finding_id"]);
    assert_eq!(r2["warnings"], json!([]));
    t.cleanup().await;
}

#[tokio::test]
async fn severity_is_required_for_finding_gotcha_verdict() {
    let t = TestApp::new().await;
    t.topic("sev-topic").await;
    let uri = "/api/topics/sev-topic/messages";
    for k in ["finding", "gotcha", "verdict"] {
        let (s, v) = t
            .write(
                "POST",
                uri,
                json!({"author": "a", "kind": k, "content": "x"}),
            )
            .await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
        assert!(
            v.to_string().contains(&format!(
                "severity is required for kind '{k}': BLOCKER, SHOULD or NICE"
            )),
            "{v}"
        );
    }
    t.cleanup().await;
}

#[tokio::test]
async fn messages_filter_by_kind() {
    let t = TestApp::new().await;
    t.topic("kind-topic").await;
    t.post("kind-topic", json!({"author": "a", "content": "plain"}))
        .await;
    t.post(
        "kind-topic",
        json!({"author": "a", "kind": "finding", "severity": "NICE", "content": "f"}),
    )
    .await;
    t.post(
        "kind-topic",
        json!({"author": "a", "kind": "question", "content": "q"}),
    )
    .await;
    let (s, v) = t.get("/api/topics/kind-topic/messages?kind=finding").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v.as_array().expect("array").len(), 1);
    let (_, v) = t
        .get("/api/topics/kind-topic/messages?kind=finding,question")
        .await;
    assert_eq!(v.as_array().expect("array").len(), 2);
    let (s, _) = t
        .get("/api/topics/kind-topic/messages?kind=finding,bogus")
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (_, v) = t.get("/api/topics/kind-topic/messages").await;
    assert_eq!(v.as_array().expect("array").len(), 3);
    t.cleanup().await;
}

#[tokio::test]
async fn stats_count_pending_proposals_and_open_blockers() {
    let t = TestApp::new().await;
    let (_, v) = t.get("/api/stats").await;
    assert_eq!(v["proposals_pending"], 0);
    assert_eq!(v["open_blockers"], 0);
    t.topic("stat-topic").await;
    t.post(
        "stat-topic",
        json!({"author": "a", "kind": "finding", "severity": "BLOCKER", "content": "b1"}),
    )
    .await;
    t.post(
        "stat-topic",
        json!({"author": "a", "kind": "finding", "severity": "NICE", "content": "n1"}),
    )
    .await;
    let (s, _) = t
        .write(
            "POST",
            "/api/proposals",
            json!({"kind": "memory", "title": "p", "body": "b", "target": "x", "fingerprint": "fp-stats"}),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED);
    let (_, v) = t.get("/api/stats").await;
    assert_eq!(v["proposals_pending"], 1);
    assert_eq!(v["open_blockers"], 1);
    assert_eq!(v["open_findings"], 2);
    t.cleanup().await;
}

#[tokio::test]
async fn posts_reopen_but_not_needs_human() {
    let t = TestApp::new().await;
    t.topic("reopen-topic").await;
    let (s, v) = t
        .write(
            "PATCH",
            "/api/topics/reopen-topic",
            json!({"status": "resolved", "outcome": "done", "actor": "orchestrator-1"}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["outcome"], "done");
    t.post(
        "reopen-topic",
        json!({"author": "a", "content": "more news"}),
    )
    .await;
    let (_, v) = t.get("/api/topics/reopen-topic").await;
    assert_eq!(v["status"], "active");
    let (_, ev) = t.get("/api/topics/reopen-topic/events").await;
    assert!(ev
        .as_array()
        .expect("events")
        .iter()
        .any(|e| e["event"] == "reopened"));

    // needs-human is sticky; illegal transition is 409.
    t.write(
        "PATCH",
        "/api/topics/reopen-topic",
        json!({"status": "needs-human", "actor": "agent"}),
    )
    .await;
    t.post(
        "reopen-topic",
        json!({"author": "a", "content": "still here"}),
    )
    .await;
    let (_, v) = t.get("/api/topics/reopen-topic").await;
    assert_eq!(v["status"], "needs-human");
    let (s, _) = t
        .write(
            "PATCH",
            "/api/topics/reopen-topic",
            json!({"status": "archived", "actor": "a"}),
        )
        .await;
    assert_eq!(s, StatusCode::CONFLICT);
    t.cleanup().await;
}

async fn age_topic(t: &TestApp, name: &str, hours: i64) {
    sqlx::query(
        "UPDATE topics SET last_activity_at = now() - make_interval(hours => $2::int), \
         updated_at = now() - make_interval(hours => $2::int) WHERE name = $1",
    )
    .bind(name)
    .bind(hours as i32)
    .execute(&t.pool)
    .await
    .expect("age");
}

#[tokio::test]
async fn sweep_dry_run_then_real() {
    let t = TestApp::new().await;
    t.topic("idle-topic").await;
    t.topic("fresh-topic").await;
    t.post(
        "idle-topic",
        json!({"author": "a", "content": "undistilled"}),
    )
    .await;
    t.topic("human-topic").await;
    t.write(
        "PATCH",
        "/api/topics/human-topic",
        json!({"status": "needs-human", "actor": "a"}),
    )
    .await;
    age_topic(&t, "idle-topic", 100).await;
    age_topic(&t, "human-topic", 1000).await;

    let (s, dry) = t
        .write(
            "POST",
            "/api/maintenance/sweep",
            json!({"dry_run": true, "actor": "gardener"}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{dry}");
    let changes = dry["changes"].as_array().expect("changes");
    assert!(changes
        .iter()
        .any(|c| c["topic"] == "idle-topic" && c["to"] == "quiet"));
    assert!(changes
        .iter()
        .any(|c| c["topic"] == "idle-topic" && c["to"] == "resolved"));
    assert!(changes
        .iter()
        .all(|c| c["topic"] != "human-topic" && c["topic"] != "fresh-topic"));
    // Dry run changed nothing.
    let (_, v) = t.get("/api/topics/idle-topic").await;
    assert_eq!(v["status"], "active");
    let (_, ev) = t.get("/api/topics/idle-topic/events").await;
    assert_eq!(ev.as_array().expect("events").len(), 1);

    let (_, real) = t
        .write(
            "POST",
            "/api/maintenance/sweep",
            json!({"dry_run": false, "actor": "gardener"}),
        )
        .await;
    assert_eq!(real["changes"], dry["changes"]);
    let (_, v) = t.get("/api/topics/idle-topic").await;
    assert_eq!(v["status"], "resolved");
    assert_eq!(v["outcome"], "auto: inactive");
    let (_, v) = t.get("/api/topics/human-topic").await;
    assert_eq!(v["status"], "needs-human");

    // Resolved but not distilled: not archived, even when old.
    age_topic(&t, "idle-topic", 100).await;
    let (_, again) = t
        .write(
            "POST",
            "/api/maintenance/sweep",
            json!({"dry_run": false, "actor": "gardener"}),
        )
        .await;
    assert!(again["changes"].as_array().expect("changes").is_empty());
    t.cleanup().await;
}

#[tokio::test]
async fn archive_and_compaction_need_a_digest() {
    let t = TestApp::new().await;
    t.topic("arch-topic").await;
    let prog = t
        .post(
            "arch-topic",
            json!({"author": "a", "kind": "progress", "content": "heartbeat 1"}),
        )
        .await;
    assert_eq!(prog["ephemeral"], true);
    t.post(
        "arch-topic",
        json!({"author": "a", "content": "a keeper note"}),
    )
    .await;
    // Digest through the last message (keyword fallback, empty body).
    let (s, d) = t
        .write("POST", "/api/topics/arch-topic/digest", json!({}))
        .await;
    assert_eq!(s, StatusCode::CREATED, "{d}");
    assert_eq!(d["version"], 1);
    assert_eq!(d["producer"], "keyword");
    t.write(
        "PATCH",
        "/api/topics/arch-topic",
        json!({"status": "resolved", "outcome": "ok", "actor": "a"}),
    )
    .await;
    age_topic(&t, "arch-topic", 24 * 40).await;
    let (_, r1) = t
        .write(
            "POST",
            "/api/maintenance/sweep",
            json!({"dry_run": false, "actor": "gardener"}),
        )
        .await;
    let changes = r1["changes"].as_array().expect("c");
    assert!(changes.iter().any(|c| c["to"] == "archived"));
    // Archived and long idle in the same sweep: only the ephemeral message is compacted.
    assert!(changes.iter().any(|c| c["reason"]
        .as_str()
        .expect("reason")
        .contains("compacted 1")));
    let (_, msgs) = t.get("/api/topics/arch-topic/messages").await;
    assert_eq!(msgs[0]["content"], "[compacted into digest v1]");
    assert!(msgs[0]["compacted_at"].is_string());
    assert_eq!(msgs[1]["content"], "a keeper note");
    t.cleanup().await;
}

#[tokio::test]
async fn digest_rules_and_distill_input() {
    let t = TestApp::new().await;
    t.topic("dig-topic").await;
    t.post("dig-topic", json!({"author": "a", "content": "plain note"}))
        .await;
    t.post(
        "dig-topic",
        json!({"author": "a", "kind": "decision", "content": "we decide X"}),
    )
    .await;
    let (_, input) = t.get("/api/topics/dig-topic/distill-input").await;
    // Typed (already structured) messages are excluded.
    assert_eq!(input["messages"].as_array().expect("m").len(), 1);
    assert_eq!(input["messages"][0]["content"], "plain note");

    let (s, d) = t
        .write(
            "POST",
            "/api/topics/dig-topic/digest",
            json!({"summary": "S", "producer": "gardener:qwen", "actor": "gardener"}),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{d}");
    let through = d["through_message_id"].as_i64().expect("through");
    let (_, topic) = t.get("/api/topics/dig-topic").await;
    assert_eq!(topic["distilled_through"], through);

    let (s, _) = t
        .write(
            "POST",
            "/api/topics/dig-topic/digest",
            json!({"summary": "S2", "producer": "p", "through_message_id": through - 1}),
        )
        .await;
    assert_eq!(s, StatusCode::CONFLICT);

    // Above the newest message: 409. Negative: 400. Foreign finding ids: 400.
    let (s, _) = t
        .write(
            "POST",
            "/api/topics/dig-topic/digest",
            json!({"summary": "S3", "producer": "p", "through_message_id": through + 1000}),
        )
        .await;
    assert_eq!(s, StatusCode::CONFLICT);
    let (s, _) = t
        .write(
            "POST",
            "/api/topics/dig-topic/digest",
            json!({"summary": "S4", "producer": "p", "through_message_id": -1}),
        )
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _) = t
        .write(
            "POST",
            "/api/topics/dig-topic/digest",
            json!({"summary": "S5", "producer": "p", "finding_ids": [999999]}),
        )
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (_, fl) = t.get("/api/findings?topic=dig-topic").await;
    let (s, d2) = t
        .write(
            "POST",
            "/api/topics/dig-topic/digest",
            json!({"summary": "S6", "producer": "p", "finding_ids": [id_of(&fl[0]), id_of(&fl[0])]}),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{d2}");
    assert_eq!(d2["finding_ids"].as_array().expect("ids").len(), 1);
    let _ = d2;

    let (_, list) = t.get("/api/digests?topic=dig-topic").await;
    assert_eq!(list.as_array().expect("l").len(), 2);
    assert!(list[0].get("transcript_md").is_none());
    let (s, full) = t.get(&format!("/api/digests/{}", id_of(&d))).await;
    assert_eq!(s, StatusCode::OK);
    assert!(full["transcript_md"]
        .as_str()
        .expect("md")
        .contains("plain note"));
    t.cleanup().await;
}

#[tokio::test]
async fn leaderboard_uses_outcomes_and_taste_is_a_mean() {
    let t = TestApp::new().await;
    t.topic("lb-topic").await;
    let good = json!({"agent_type": "rust-reviewer", "model": "sonnet", "harness": "claude"});
    let bad = json!({"agent_type": "sloppy", "model": "haiku", "harness": "claude"});
    for (alias, agent, verdict_ok) in [("good-one", &good, true), ("bad-one", &bad, false)] {
        for i in 0..5 {
            t.post(
                "lb-topic",
                json!({"author": alias, "kind": "finding", "severity": "SHOULD",
                       "content": format!("{alias} claim number {i}"), "agent": agent}),
            )
            .await;
        }
        let _ = verdict_ok;
    }
    let (_, findings) = t.get("/api/findings?limit=50").await;
    for f in findings.as_array().expect("f") {
        let claim = f["claim"].as_str().expect("claim");
        let status = if claim.starts_with("good-one") {
            "confirmed"
        } else {
            "false_positive"
        };
        let (s, v) = t
            .write(
                "PATCH",
                &format!("/api/findings/{}", id_of(f)),
                json!({"status": status, "actor": "orchestrator-1", "actor_class": "orchestrator"}),
            )
            .await;
        assert_eq!(s, StatusCode::OK, "{v}");
    }
    let (_, lb) = t.get("/api/leaderboard").await;
    let rows = lb.as_array().expect("lb");
    assert_eq!(rows[0]["agent_type"], "rust-reviewer");
    assert_eq!(rows[0]["rank"], 1);
    assert_eq!(rows[0]["n_judged"], 5);
    assert_eq!(rows[0]["insufficient_data"], false);
    let q_good = rows[0]["quality"].as_f64().expect("q");
    let q_bad = rows[1]["quality"].as_f64().expect("q");
    assert!(
        q_good > 0.7 && q_bad < 0.7 && q_good > q_bad,
        "{q_good} {q_bad}"
    );
    assert_eq!(rows[0]["aliases"], json!(["good-one"]));

    // Taste: two 50s must not outrank one 88 by mean.
    for (name, pts) in [("steady", 50), ("steady", 50), ("star", 88)] {
        let (s, v) = t
            .write(
                "POST",
                "/api/score",
                json!({"agent_name": name, "score": pts, "reason": "r", "topic": "lb-topic"}),
            )
            .await;
        assert_eq!(s, StatusCode::OK, "{v}");
    }
    let (_, aliases) = t.get("/api/leaderboard/aliases").await;
    assert_eq!(aliases[0]["alias"], "star");
    assert_eq!(aliases[0]["taste_mean"], 88.0);
    assert_eq!(aliases[1]["alias"], "steady");
    assert_eq!(aliases[1]["taste_total"], 100);
    assert_eq!(aliases[1]["taste_mean"], 50.0);
    let (s, _) = t
        .write(
            "POST",
            "/api/score",
            json!({"agent_name": "x", "score": 101, "reason": "r"}),
        )
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    // Scoring noise goes to the timeline, never into the thread.
    let (_, msgs) = t.get("/api/topics/lb-topic/messages?limit=500").await;
    assert_eq!(msgs.as_array().expect("m").len(), 10);
    let (_, ev) = t.get("/api/topics/lb-topic/events").await;
    assert!(ev
        .as_array()
        .expect("e")
        .iter()
        .any(|e| e["event"] == "scored"));

    let (_, hint) = t.get("/api/route-hint").await;
    assert_eq!(hint["recommendation"]["agent_type"], "rust-reviewer");
    t.cleanup().await;
}

#[tokio::test]
async fn first_alias_write_wins_and_mismatch_is_recorded() {
    let t = TestApp::new().await;
    t.topic("alias-topic").await;
    let a = json!({"agent_type": "a", "model": "m", "harness": "claude"});
    let b = json!({"agent_type": "b", "model": "m", "harness": "agy"});
    let m1 = t
        .post(
            "alias-topic",
            json!({"author": "who", "content": "one", "agent": a}),
        )
        .await;
    let m2 = t
        .post(
            "alias-topic",
            json!({"author": "who", "content": "two", "agent": b}),
        )
        .await;
    assert_eq!(m1["identity"]["agent_type"], "a");
    assert_eq!(m2["identity"]["agent_type"], "a");
    let (_, ev) = t.get("/api/topics/alias-topic/events").await;
    assert!(ev
        .as_array()
        .expect("e")
        .iter()
        .any(|e| e["event"] == "alias-mismatch"));
    t.cleanup().await;
}

#[tokio::test]
async fn search_finds_message_and_finding_words() {
    let t = TestApp::new().await;
    t.topic("search-topic").await;
    t.post(
        "search-topic",
        json!({"author": "a", "content": "the quokka migrated overnight"}),
    )
    .await;
    t.post(
        "search-topic",
        json!({"author": "a", "kind": "gotcha", "severity": "NICE",
               "content": "wombat state is not persisted"}),
    )
    .await;
    let (s, v) = t.get("/api/search?q=quokka").await;
    assert_eq!(s, StatusCode::OK);
    assert!(v["results"]
        .as_array()
        .expect("r")
        .iter()
        .any(|r| r["kind"] == "message" && r["topic_name"] == "search-topic"));
    let (_, v) = t.get("/api/search?q=wombat").await;
    assert!(v["results"]
        .as_array()
        .expect("r")
        .iter()
        .any(|r| r["kind"] == "finding"));
    // Topic names come back as trigram suggestions for near-misses.
    let (_, v) = t.get("/api/search?q=search-topik").await;
    assert!(v["suggestions"]
        .as_array()
        .expect("s")
        .iter()
        .any(|s| s == "search-topic"));
    let (_, v) = t.get("/api/brief?q=wombat").await;
    assert_eq!(v["findings"].as_array().expect("f").len(), 1);
    t.cleanup().await;
}

#[tokio::test]
async fn proposals_kv_and_runs() {
    let t = TestApp::new().await;
    let p = json!({"kind": "skill", "title": "T", "body": "B", "evidence_finding_ids": [], "fingerprint": "fp1"});
    let (s, v) = t.write("POST", "/api/proposals", p.clone()).await;
    assert_eq!(s, StatusCode::CREATED);
    let (s, _) = t.write("POST", "/api/proposals", p).await;
    assert_eq!(s, StatusCode::CONFLICT);
    let (s, v2) = t
        .write(
            "PATCH",
            &format!("/api/proposals/{}", id_of(&v)),
            json!({"status": "approved", "actor": "me"}),
        )
        .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v2["decided_by"], "me");
    let (_, l) = t.get("/api/proposals?status=approved").await;
    assert_eq!(l.as_array().expect("l").len(), 1);

    let (s, _) = t
        .write("PUT", "/api/kv/cursor", json!({"v": {"n": 3}}))
        .await;
    assert_eq!(s, StatusCode::OK);
    let (_, kv) = t.get("/api/kv/cursor").await;
    assert_eq!(kv["v"]["n"], 3);

    let (s, run) = t
        .write("POST", "/api/gardener/runs", json!({"stage": "sweep"}))
        .await;
    assert_eq!(s, StatusCode::CREATED);
    let (s, run) = t
        .write(
            "PATCH",
            &format!("/api/gardener/runs/{}", id_of(&run)),
            json!({"ok": true, "topics_swept": 2}),
        )
        .await;
    assert_eq!(s, StatusCode::OK);
    assert!(run["finished_at"].is_string());

    let (s, text) = t.get("/metrics").await;
    assert_eq!(s, StatusCode::OK);
    let text = text.as_str().expect("text").to_string();
    for needle in [
        "hive_db_up 1",
        "hive_topics{status=\"active\"}",
        "hive_proposals{status=\"approved\"} 1",
        "hive_last_gardener_run_ok 1",
        "hive_http_requests_total{route=",
    ] {
        assert!(text.contains(needle), "missing {needle}");
    }
    let (_, stats) = t.get("/api/stats").await;
    assert_eq!(stats["total_topics"], 0);
    t.cleanup().await;
}

#[tokio::test]
async fn database_outage_is_503_not_500() {
    let t = TestApp::new().await;
    t.pool.close().await;
    let (s, v) = t.get("/api/topics").await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(v["code"], "unavailable");
    let (s, _) = t.get("/readyz").await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
    let (s, text) = t.get("/metrics").await;
    assert_eq!(s, StatusCode::OK);
    assert!(text.as_str().expect("t").contains("hive_db_up 0"));
    t.cleanup().await;
}

#[tokio::test]
async fn gardener_cannot_undo_a_judgement() {
    let t = TestApp::new().await;
    t.topic("judge-topic").await;
    t.post(
        "judge-topic",
        json!({"author": "a", "kind": "finding", "severity": "NICE", "content": "judged claim"}),
    )
    .await;
    let (_, list) = t.get("/api/findings").await;
    let uri = format!("/api/findings/{}", id_of(&list[0]));
    t.write(
        "PATCH",
        &uri,
        json!({"status": "false_positive", "actor": "orchestrator-1", "actor_class": "orchestrator"}),
    )
    .await;
    let (s, _) = t
        .write(
            "PATCH",
            &uri,
            json!({"status": "open", "actor": "gardener", "actor_class": "gardener"}),
        )
        .await;
    assert_eq!(s, StatusCode::CONFLICT);
    // The orchestrator may reopen.
    let (s, v) = t
        .write(
            "PATCH",
            &uri,
            json!({"status": "open", "actor": "orchestrator-1", "actor_class": "orchestrator"}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    t.cleanup().await;
}

#[tokio::test]
async fn nul_characters_are_rejected_as_400() {
    let t = TestApp::new().await;
    t.topic("nul-topic").await;
    let (s, v) = t
        .write(
            "POST",
            "/api/topics/nul-topic/messages",
            json!({"author": "a", "content": "bad \u{0} byte"}),
        )
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    // Optional fields hit the database error mapping instead of a 500.
    let (s, v) = t
        .write(
            "POST",
            "/api/topics/nul-topic/messages",
            json!({"author": "a", "content": "ok", "evidence": "bad \u{0} byte"}),
        )
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    t.cleanup().await;
}

#[tokio::test]
async fn deep_threads_are_bounded_and_empty_text_becomes_null() {
    let t = TestApp::new().await;
    t.topic("deep-topic").await;
    let mut parent: Option<i64> = None;
    for i in 0..230 {
        let m = t
            .post(
                "deep-topic",
                json!({"author": "a", "content": format!("reply {i}"), "parent_id": parent}),
            )
            .await;
        parent = Some(id_of(&m));
    }
    let (s, v) = t.get("/api/topics/deep-topic/threads").await;
    assert_eq!(s, StatusCode::OK);
    let (mut count, mut max_depth) = (0usize, 0usize);
    let mut stack = vec![(&v[0], 1usize)];
    while let Some((n, d)) = stack.pop() {
        count += 1;
        max_depth = max_depth.max(d);
        for r in n["replies"].as_array().expect("replies") {
            stack.push((r, d + 1));
        }
    }
    assert_eq!(count, 230);
    assert!(max_depth <= 50, "depth {max_depth}");
    let root_id = id_of(&v[0]["message"]);
    let (s, one) = t.get(&format!("/api/messages/{root_id}/thread")).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(one["message"]["id"], root_id);

    // Empty description / outcome are stored as NULL.
    let (_, topic) = t
        .write(
            "PATCH",
            "/api/topics/deep-topic",
            json!({"description": "  ", "actor": "a"}),
        )
        .await;
    assert!(topic["description"].is_null());
    t.cleanup().await;
}

#[tokio::test]
async fn gardener_gauges_are_omitted_until_a_run_exists() {
    let t = TestApp::new().await;
    let (_, text) = t.get("/metrics").await;
    let text = text.as_str().expect("text");
    assert!(text.contains("hive_gardener_runs_total 0"));
    assert!(!text.contains("hive_last_gardener_run_ok"));
    t.cleanup().await;
}
