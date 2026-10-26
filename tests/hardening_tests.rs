//! v0.2.1 hardening: redaction, audit trail, delete confirmation, headers, gated reads.
//! Secret-shaped fixtures are assembled at runtime so no literal token sits in the source.

mod common;

use axum::http::StatusCode;
use common::TestApp;
use serde_json::{json, Value};

fn id_of(v: &Value) -> i64 {
    v["id"].as_i64().expect("numeric id")
}

fn fake_github_token() -> String {
    format!("ghp_{}", "abcdefghijklmnopqrstuvwxyz0123")
}

fn fake_aws_key() -> String {
    format!("AKIA{}", "IOSFODNN7EXAMPLE")
}

#[tokio::test]
async fn secrets_are_redacted_never_rejected_and_audited() {
    let t = TestApp::new().await;
    t.topic("redact-topic").await;
    let token = fake_github_token();
    let aws = fake_aws_key();
    let m = t
        .post(
            "redact-topic",
            json!({"author": "a", "content": format!("leaked {token} in the log"),
                   "evidence": format!("{aws} seen")}),
        )
        .await;
    let body = m["content"].as_str().expect("content");
    assert!(!body.contains("ghp_"), "{body}");
    assert!(body.contains("[REDACTED:github-token]"));
    assert_eq!(m["evidence"], "[REDACTED:aws-key] seen");

    let (_, ev) = t.get("/api/topics/redact-topic/events").await;
    let red: Vec<&Value> = ev
        .as_array()
        .expect("e")
        .iter()
        .filter(|e| e["event"] == "secret-redacted")
        .collect();
    assert_eq!(red.len(), 2);
    assert!(red.iter().any(|e| e["detail"]["field"] == "content"
        && e["detail"]["kind"] == "github-token"
        && e["detail"]["count"] == 1));
    // Never the value, anywhere in the audit rows.
    assert!(!ev.to_string().contains(&token));
    assert!(!ev.to_string().contains(&aws));

    // Other write routes redact too: topic title, kv, score reason, finding batch, digest.
    let (s, v) = t
        .write(
            "POST",
            "/api/topics",
            json!({"name": "redact-two", "title": format!("key {token}"), "author": "a"}),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    assert_eq!(v["title"], "key [REDACTED:github-token]");
    let (_, kv) = t
        .write("PUT", "/api/kv/k1", json!({"v": {"a": [token.clone()]}}))
        .await;
    assert_eq!(kv["v"]["a"][0], "[REDACTED:github-token]");
    let (_, sc) = t
        .write(
            "POST",
            "/api/score",
            json!({"agent_name": "x", "score": 50, "reason": format!("used {token}"),
                   "topic": "redact-topic"}),
        )
        .await;
    assert_eq!(sc["alias"], "x");
    let (s, fb) = t
        .write(
            "POST",
            "/api/topics/redact-topic/findings",
            json!({"findings": [{"kind": "finding", "claim": format!("claim {token}"),
                                 "provenance": "gardener"}]}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{fb}");
    let (_, fl) = t.get("/api/findings?topic=redact-topic").await;
    assert!(fl.to_string().contains("[REDACTED:github-token]"));
    assert!(!fl.to_string().contains(&token));
    let (s, d) = t
        .write(
            "POST",
            "/api/topics/redact-topic/digest",
            json!({"summary": format!("sum {token}"), "producer": "p"}),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{d}");
    assert_eq!(d["summary"], "sum [REDACTED:github-token]");
    let (_, ev) = t.get("/api/topics/redact-topic/events").await;
    assert!(!ev.to_string().contains(&token));
    t.cleanup().await;
}

#[tokio::test]
async fn ordinary_words_and_public_certs_are_not_redacted() {
    let t = TestApp::new().await;
    t.topic("clean-topic").await;
    let text = "task-kind and risk-based, -----BEGIN CERTIFICATE-----";
    let m = t
        .post("clean-topic", json!({"author": "a", "content": text}))
        .await;
    assert_eq!(m["content"], text);
    let (_, ev) = t.get("/api/topics/clean-topic/events").await;
    assert!(!ev
        .as_array()
        .expect("e")
        .iter()
        .any(|e| e["event"] == "secret-redacted"));
    t.cleanup().await;
}

#[tokio::test]
async fn edits_and_deletes_leave_an_audit_trail() {
    let t = TestApp::new().await;
    t.topic("audit-topic").await;
    let m = t
        .post(
            "audit-topic",
            json!({"author": "al", "content": "original text"}),
        )
        .await;
    let reply = t
        .post(
            "audit-topic",
            json!({"author": "bo", "content": "reply", "parent_id": id_of(&m)}),
        )
        .await;
    let uri = format!("/api/messages/{}", id_of(&m));
    let (s, _) = t
        .write(
            "PATCH",
            &uri,
            json!({"author": "al", "content": "edited text"}),
        )
        .await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = t.send("DELETE", &uri, None, Some(common::TOKEN), &[]).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (_, ev) = t.get("/api/topics/audit-topic/events").await;
    let evs = ev.as_array().expect("e");
    let edited = evs
        .iter()
        .find(|e| e["event"] == "message-edited")
        .expect("edited");
    assert_eq!(
        edited["detail"]["old_sha256"],
        hive_board::util::sha256_hex("original text")
    );
    assert_eq!(edited["detail"]["old_length"], 13);
    assert!(!ev.to_string().contains("original text"));
    let deleted = evs
        .iter()
        .find(|e| e["event"] == "message-deleted")
        .expect("deleted");
    assert_eq!(deleted["detail"]["author"], "al");
    assert_eq!(
        deleted["detail"]["sha256"],
        hive_board::util::sha256_hex("edited text")
    );
    assert_eq!(deleted["detail"]["replies_deleted"], 1);
    let (s, _) = t
        .get(&format!("/api/messages/{}/thread", id_of(&reply)))
        .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    t.cleanup().await;
}

#[tokio::test]
async fn topic_delete_needs_the_exact_name() {
    let t = TestApp::new().await;
    let topic = t.topic("delete-me").await;
    let id = id_of(&topic);
    // Missing, wrong, wrong-case and id-as-confirm all fail with 400.
    for uri in [
        "/api/topics/delete-me".to_string(),
        "/api/topics/delete-me?confirm=other".to_string(),
        "/api/topics/delete-me?confirm=Delete-Me".to_string(),
        format!("/api/topics/{id}?confirm={id}"),
    ] {
        let (s, v) = t.send("DELETE", &uri, None, Some(common::TOKEN), &[]).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{uri}: {v}");
        assert_eq!(v["error"], "confirm must equal the topic name");
        assert_eq!(v["code"], "validation");
    }
    let (s, _) = t.get("/api/topics/delete-me").await;
    assert_eq!(s, StatusCode::OK);
    // A numeric id works when confirm is the resolved name.
    let (s, _) = t
        .send(
            "DELETE",
            &format!("/api/topics/{id}?confirm=delete-me"),
            None,
            Some(common::TOKEN),
            &[],
        )
        .await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, _) = t.get("/api/topics/delete-me").await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    t.cleanup().await;
}

#[tokio::test]
async fn security_headers_are_on_every_response() {
    let t = TestApp::new().await;
    for (uri, want) in [
        ("/api/topics", StatusCode::OK),
        ("/readyz", StatusCode::OK),
        ("/nope", StatusCode::NOT_FOUND),
    ] {
        let (s, h) = t.get_headers(uri, None).await;
        assert_eq!(s, want, "{uri}");
        assert_eq!(h["content-security-policy"], hive_board::api::CSP, "{uri}");
        assert_eq!(h["x-content-type-options"], "nosniff");
        assert_eq!(h["referrer-policy"], "no-referrer");
    }
    assert!(hive_board::api::CSP.contains("img-src 'self' data:;"));
    assert!(!hive_board::api::CSP.contains("https:"));
    t.cleanup().await;
}

#[tokio::test]
async fn gated_reads_need_the_token_except_readyz() {
    let t = TestApp::with_gated_reads().await;
    let (s, _) = t.get("/api/topics").await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, _) = t.get("/metrics").await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, _) = t
        .send("GET", "/api/topics", None, Some(common::TOKEN), &[])
        .await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = t.get("/readyz").await;
    assert_eq!(s, StatusCode::OK);
    t.cleanup().await;
}

#[tokio::test]
async fn foreign_key_violations_are_409_naming_the_relation() {
    let t = TestApp::new().await;
    t.topic("fk-topic").await;
    let (s, v) = t
        .write(
            "POST",
            "/api/topics/fk-topic/findings",
            json!({"findings": [{"kind": "finding", "claim": "c", "provenance": "agent",
                                 "message_id": 999999}]}),
        )
        .await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert_eq!(v["code"], "conflict");
    assert!(v["error"].as_str().expect("e").contains("message"), "{v}");
    t.cleanup().await;
}

#[tokio::test]
async fn redaction_happens_before_truncation_and_covers_agent_and_lookalikes() {
    let t = TestApp::new().await;
    t.topic("trunc-topic").await;
    // A token sitting across the 64-character tag cap must not survive as a fragment.
    let tag = format!("{}{}", "a".repeat(55), fake_github_token());
    let m = t
        .post(
            "trunc-topic",
            json!({"author": "a", "content": "disk-a1b2c3d4e5f6g7h8i9j0k1 flask-0123456789abcdefghijk",
                   "tags": [tag],
                   "agent": {"agent_type": "t", "model": fake_github_token(), "harness": "h"}}),
        )
        .await;
    assert!(!m["tags"].to_string().contains("ghp_"), "{}", m["tags"]);
    assert_eq!(m["identity"]["model"], "[REDACTED:github-token]");
    // Look-alikes with letters before `sk-` are ordinary words.
    assert_eq!(
        m["content"],
        "disk-a1b2c3d4e5f6g7h8i9j0k1 flask-0123456789abcdefghijk"
    );
    t.cleanup().await;
}

#[tokio::test]
async fn boots_against_a_database_migrated_by_a_newer_release() {
    let t = TestApp::new().await;
    sqlx::query(
        "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time) \
         VALUES (9999, 'from the future', true, '\\x00', 0)",
    )
    .execute(&t.pool)
    .await
    .expect("fake future migration");
    hive_board::migrate(&t.pool)
        .await
        .expect("an unknown applied migration must not stop the boot");
    t.cleanup().await;
}
