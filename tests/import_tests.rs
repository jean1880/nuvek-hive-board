#![cfg(feature = "import")]

mod common;

use common::TestApp;
use hive_board::import::import_sqlite;
use rusqlite::{params, Connection};
use serde_json::json;

fn legacy_fixture(dir: &std::path::Path) -> std::path::PathBuf {
    let path = dir.join("board.db");
    let c = Connection::open(&path).expect("sqlite");
    c.execute_batch(
        "CREATE TABLE topics (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT UNIQUE NOT NULL, \
           title TEXT NOT NULL, description TEXT, created_by TEXT NOT NULL, created_at TEXT NOT NULL, \
           updated_at TEXT NOT NULL, status TEXT NOT NULL DEFAULT 'active');
         CREATE TABLE messages (id INTEGER PRIMARY KEY AUTOINCREMENT, topic_id INTEGER NOT NULL, \
           parent_id INTEGER, author TEXT NOT NULL, content TEXT NOT NULL, tags TEXT NOT NULL DEFAULT '[]', \
           created_at TEXT NOT NULL, updated_at TEXT NOT NULL);
         CREATE TABLE notable_contributors (name TEXT PRIMARY KEY, score INTEGER NOT NULL DEFAULT 0, \
           tasks_completed INTEGER NOT NULL DEFAULT 1, notable_contributions INTEGER NOT NULL DEFAULT 1, \
           badge TEXT NOT NULL DEFAULT 'Top Contributor', latest_recognition TEXT NOT NULL, updated_at TEXT NOT NULL);
         CREATE TABLE distillations (id INTEGER PRIMARY KEY AUTOINCREMENT, topic_name TEXT NOT NULL, \
           topic_title TEXT NOT NULL, distilled_at TEXT NOT NULL, message_count INTEGER NOT NULL, \
           contributors TEXT NOT NULL, summary TEXT NOT NULL, saved_to_neo4j INTEGER NOT NULL, archive_path TEXT);",
    )
    .expect("schema");
    let ts = "2026-01-02T03:04:05+00:00";
    c.execute(
        "INSERT INTO topics (id, name, title, description, created_by, created_at, updated_at, status) \
         VALUES (1, 'legacy-one', 'Legacy One', 'desc', 'Atlas', ?1, ?1, 'active'), \
                (2, 'legacy-old', 'Legacy Old', NULL, 'Bo', ?1, ?1, 'archived')",
        params![ts],
    )
    .expect("topics");
    // The reply is inserted before its parent id would be known by order: parent id 2 > own id 1 is not
    // needed; a plain parent link is enough to exercise the second pass.
    c.execute(
        "INSERT INTO messages (id, topic_id, parent_id, author, content, tags, created_at, updated_at) VALUES \
         (1, 1, NULL, 'Atlas', 'first legacy post', '[\"a\",\"b\"]', ?1, ?1), \
         (2, 1, 1, 'Bo', 'legacy reply', '[]', ?1, ?1), \
         (3, 2, NULL, 'Bo', 'old post', '[]', ?1, ?1)",
        params![ts],
    )
    .expect("messages");
    c.execute(
        "INSERT INTO notable_contributors (name, score, tasks_completed, notable_contributions, \
           badge, latest_recognition, updated_at) VALUES ('Atlas', 450, 3, 3, 'Top Contributor', 'great', ?1), \
           ('Bo', 40, 2, 0, 'Contributor', 'ok', ?1)",
        params![ts],
    )
    .expect("contributors");
    path
}

#[tokio::test]
async fn import_is_idempotent_and_merges_name_collisions() {
    let t = TestApp::new().await;
    let tmp = tempfile::tempdir().expect("tmp");
    let db = legacy_fixture(tmp.path());
    let digests = tmp.path().join("distilled");
    std::fs::create_dir_all(&digests).expect("dir");
    std::fs::write(
        digests.join("legacy-old_20260105_101500.md"),
        "# Hive Digest: Legacy Old\n\n- **Participants**: Bo, Atlas\n\n## Key Findings & Decisions\n\n- [Bo] Decision: ship it\n\n## Full Thread Transcript\n",
    )
    .expect("digest file");
    std::fs::write(digests.join("orphan-topic_20260105_101500.md"), "x").expect("orphan");

    // A pre-existing topic with a colliding name.
    t.topic("legacy-one").await;

    let r1 = import_sqlite(&t.pool, &db, "nas", Some(&digests))
        .await
        .expect("import 1");
    assert_eq!(r1.topics_inserted, 1);
    assert_eq!(r1.topics_merged, 1);
    assert_eq!(r1.messages_inserted, 3);
    assert_eq!(r1.contributors_inserted, 2);
    assert_eq!(r1.digests_inserted, 1);
    assert_eq!(r1.digests_skipped, 1);

    let r2 = import_sqlite(&t.pool, &db, "nas", Some(&digests))
        .await
        .expect("import 2");
    assert_eq!(r2.topics_inserted, 0);
    assert_eq!(r2.messages_inserted, 0);
    assert_eq!(r2.messages_existing, 3);
    assert_eq!(r2.contributors_inserted, 0);
    assert_eq!(r2.digests_inserted, 0);
    assert_eq!(r2.digests_existing, 1);

    // The same data from another source merges and dedups on (topic, author, created_at, md5).
    let r3 = import_sqlite(&t.pool, &db, "workstation", None)
        .await
        .expect("import 3");
    assert_eq!(r3.topics_inserted, 0);
    assert_eq!(r3.topics_merged, 2);
    assert_eq!(r3.messages_inserted, 0, "{r3:?}");
    assert_eq!(r3.messages_existing, 3);

    // Merged into the existing topic, replies re-linked, ordering kept.
    let (_, msgs) = t.get("/api/topics/legacy-one/messages").await;
    assert_eq!(msgs.as_array().expect("m").len(), 2);
    assert_eq!(msgs[1]["parent_id"], msgs[0]["id"]);
    let (_, topic) = t.get("/api/topics/legacy-old").await;
    assert_eq!(topic["status"], "archived");
    let (_, dl) = t.get("/api/digests?topic=legacy-old").await;
    assert_eq!(dl[0]["producer"], "legacy-import");
    assert!(dl[0]["summary"].as_str().expect("s").contains("ship it"));
    assert_eq!(dl[0]["participants"], json!(["Bo", "Atlas"]));

    // Legacy scores: 450/3 = 150 -> capped at 100.
    let (_, al) = t.get("/api/leaderboard/aliases").await;
    assert_eq!(al[0]["alias"], "Atlas");
    assert_eq!(al[0]["taste_mean"], 100.0);
    assert_eq!(al[1]["taste_mean"], 20.0);
    t.cleanup().await;
}

/// Two source messages that differ only in a secret redact to identical text. They must stay two
/// rows, keep their reply link (never a self-parent), and re-import idempotently, also under
/// another source label.
#[tokio::test]
async fn messages_equal_after_redaction_stay_distinct_with_their_parent() {
    let t = TestApp::new().await;
    let tmp = tempfile::tempdir().expect("tmp");
    let path = tmp.path().join("board.db");
    let c = Connection::open(&path).expect("sqlite");
    c.execute_batch(
        "CREATE TABLE topics (id INTEGER PRIMARY KEY, name TEXT, title TEXT, description TEXT, \
           created_by TEXT, created_at TEXT, updated_at TEXT, status TEXT);
         CREATE TABLE messages (id INTEGER PRIMARY KEY, topic_id INTEGER, parent_id INTEGER, \
           author TEXT, content TEXT, tags TEXT, created_at TEXT, updated_at TEXT);
         CREATE TABLE notable_contributors (name TEXT PRIMARY KEY, score INTEGER, \
           tasks_completed INTEGER, latest_recognition TEXT);",
    )
    .expect("schema");
    let ts = "2026-01-02T03:04:05+00:00";
    c.execute(
        "INSERT INTO topics VALUES (1, 'twins', 'Twins', NULL, 'Atlas', ?1, ?1, 'active')",
        params![ts],
    )
    .expect("topic");
    // Built from parts at runtime so no scanner sees a credential-shaped literal.
    let first = ["pass", "word=", "alpha-0001"].concat();
    let second = ["pass", "word=", "beta-0002"].concat();
    c.execute(
        "INSERT INTO messages VALUES (1, 1, NULL, 'Atlas', ?1, '[]', ?3, ?3), \
                                     (2, 1, 1, 'Atlas', ?2, '[]', ?3, ?3)",
        params![first, second, ts],
    )
    .expect("messages");
    drop(c);

    let check = |label: &'static str| {
        let t = &t;
        async move {
            let (_, msgs) = t.get("/api/topics/twins/messages").await;
            let msgs = msgs.as_array().expect("array").clone();
            assert_eq!(msgs.len(), 2, "{label}: {msgs:?}");
            assert_ne!(msgs[0]["id"], msgs[1]["id"], "{label}");
            assert!(msgs[0]["parent_id"].is_null(), "{label}: {msgs:?}");
            assert_eq!(msgs[1]["parent_id"], msgs[0]["id"], "{label}: {msgs:?}");
            let text = serde_json::to_string(&msgs).expect("json");
            assert!(
                !text.contains("alpha-0001") && !text.contains("beta-0002"),
                "{text}"
            );
        }
    };

    let r1 = import_sqlite(&t.pool, &path, "legacy", None)
        .await
        .expect("import 1");
    assert_eq!(
        (r1.messages_inserted, r1.messages_existing),
        (2, 0),
        "{r1:?}"
    );
    check("first import").await;
    let r2 = import_sqlite(&t.pool, &path, "legacy", None)
        .await
        .expect("import 2");
    assert_eq!(
        (r2.messages_inserted, r2.messages_existing),
        (0, 2),
        "{r2:?}"
    );
    check("re-import").await;
    // Another label falls back to content matching. Both rows redact to the same text, so every
    // match is ambiguous: they are imported as new rows rather than guessed onto the old ones.
    let r3 = import_sqlite(&t.pool, &path, "mirror", None)
        .await
        .expect("import 3");
    assert_eq!(
        (r3.messages_inserted, r3.messages_existing),
        (2, 0),
        "{r3:?}"
    );
    let r4 = import_sqlite(&t.pool, &path, "mirror", None)
        .await
        .expect("import 4");
    assert_eq!(
        (r4.messages_inserted, r4.messages_existing),
        (0, 2),
        "the new rows are found again by source id: {r4:?}"
    );
    let (_, msgs) = t.get("/api/topics/twins/messages").await;
    let msgs = msgs.as_array().expect("array").clone();
    assert_eq!(msgs.len(), 4, "{msgs:?}");
    // Each source's reply points at that source's own first message, never at itself.
    for pair in msgs.chunks(2) {
        assert!(pair[0]["parent_id"].is_null(), "{msgs:?}");
        assert_eq!(pair[1]["parent_id"], pair[0]["id"], "{msgs:?}");
    }
    t.cleanup().await;
}

fn reply_fixture(path: &std::path::Path, rows: &[(i64, Option<i64>, &str)]) {
    let _ = std::fs::remove_file(path);
    let c = Connection::open(path).expect("sqlite");
    c.execute_batch(
        "CREATE TABLE topics (id INTEGER PRIMARY KEY, name TEXT, title TEXT, description TEXT, \
           created_by TEXT, created_at TEXT, updated_at TEXT, status TEXT);
         CREATE TABLE messages (id INTEGER PRIMARY KEY, topic_id INTEGER, parent_id INTEGER, \
           author TEXT, content TEXT, tags TEXT, created_at TEXT, updated_at TEXT);
         CREATE TABLE notable_contributors (name TEXT PRIMARY KEY, score INTEGER, \
           tasks_completed INTEGER, latest_recognition TEXT);",
    )
    .expect("schema");
    let ts = "2026-01-02T03:04:05+00:00";
    c.execute(
        "INSERT INTO topics VALUES (1, 'chain', 'Chain', NULL, 'Atlas', ?1, ?1, 'active')",
        params![ts],
    )
    .expect("topic");
    for (id, parent, content) in rows {
        c.execute(
            "INSERT INTO messages VALUES (?1, 1, ?2, 'Atlas', ?3, '[]', ?4, ?4)",
            params![id, parent, content, ts],
        )
        .expect("message");
    }
}

/// A mirror that first matched rows 1 and 2 onto A and B by content, then drops row 1 and adds a
/// reply to row 2: the reply attaches to B, never to A.
#[tokio::test]
async fn a_changed_mirror_snapshot_attaches_a_new_reply_to_the_right_message() {
    let t = TestApp::new().await;
    let tmp = tempfile::tempdir().expect("tmp");
    let path = tmp.path().join("board.db");
    reply_fixture(&path, &[(1, None, "first note"), (2, None, "second note")]);
    import_sqlite(&t.pool, &path, "origin", None)
        .await
        .expect("origin");
    let (_, before) = t.get("/api/topics/chain/messages").await;
    let (a, b) = (before[0]["id"].clone(), before[1]["id"].clone());

    let r1 = import_sqlite(&t.pool, &path, "mirror", None)
        .await
        .expect("mirror 1");
    assert_eq!(
        (r1.messages_inserted, r1.messages_existing),
        (0, 2),
        "{r1:?}"
    );

    reply_fixture(&path, &[(2, None, "second note"), (3, Some(2), "a reply")]);
    let r2 = import_sqlite(&t.pool, &path, "mirror", None)
        .await
        .expect("mirror 2");
    assert_eq!(
        (r2.messages_inserted, r2.messages_existing),
        (1, 1),
        "{r2:?}"
    );
    let (_, msgs) = t.get("/api/topics/chain/messages").await;
    let reply = msgs
        .as_array()
        .expect("array")
        .iter()
        .find(|m| m["content"] == "a reply")
        .expect("reply")
        .clone();
    assert_eq!(reply["parent_id"], b, "the reply attaches to B: {msgs}");
    assert_ne!(reply["parent_id"], a);
    t.cleanup().await;
}

/// A self-parenting source row aborts the import, and the whole transaction rolls back.
#[tokio::test]
async fn a_self_parent_rolls_the_whole_import_back() {
    let t = TestApp::new().await;
    let tmp = tempfile::tempdir().expect("tmp");
    let path = tmp.path().join("board.db");
    reply_fixture(&path, &[(1, None, "fine"), (2, Some(2), "my own parent")]);
    let err = import_sqlite(&t.pool, &path, "broken", None)
        .await
        .expect_err("a self-parent must fail the import");
    assert!(err.to_string().contains("own parent"), "{err:#}");
    let (topics,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM topics WHERE legacy_source = 'broken' OR name = 'chain'",
    )
    .fetch_one(&t.pool)
    .await
    .expect("count");
    let (messages,): (i64,) =
        sqlx::query_as("SELECT count(*) FROM messages WHERE legacy_source = 'broken'")
            .fetch_one(&t.pool)
            .await
            .expect("count");
    assert_eq!(
        (topics, messages),
        (0, 0),
        "nothing from the failed import remains"
    );
    t.cleanup().await;
}

/// Imported topics, messages, tags, contributors and digests are redacted like API writes, and a
/// re-import still finds every row (the redaction is deterministic).
#[tokio::test]
async fn import_redacts_secret_shapes_and_stays_idempotent() {
    let t = TestApp::new().await;
    let tmp = tempfile::tempdir().expect("tmp");
    // Built from parts at runtime so no scanner sees a credential-shaped literal.
    let tok = ["ghp", "_", "abcdefghijklmnopqrstuvwxyz0123"].concat();
    let url = ["postgres://app:", "s3cr3tPw0rd9", "@db.example.test/x"].concat();
    let kv = ["pass", "word=", "hunter2hunter2"].concat();
    let path = tmp.path().join("board.db");
    let c = Connection::open(&path).expect("sqlite");
    c.execute_batch(
        "CREATE TABLE topics (id INTEGER PRIMARY KEY, name TEXT, title TEXT, description TEXT, \
           created_by TEXT, created_at TEXT, updated_at TEXT, status TEXT);
         CREATE TABLE messages (id INTEGER PRIMARY KEY, topic_id INTEGER, parent_id INTEGER, \
           author TEXT, content TEXT, tags TEXT, created_at TEXT, updated_at TEXT);
         CREATE TABLE notable_contributors (name TEXT PRIMARY KEY, score INTEGER, \
           tasks_completed INTEGER, latest_recognition TEXT);",
    )
    .expect("schema");
    let ts = "2026-01-02T03:04:05+00:00";
    c.execute(
        "INSERT INTO topics VALUES (1, 'leaky', ?1, ?2, 'Atlas', ?3, ?3, 'active')",
        params![format!("deploy {tok}"), format!("db {url}"), ts],
    )
    .expect("topic");
    let tags = serde_json::to_string(&[tok.as_str(), "ops"]).expect("tags");
    c.execute(
        "INSERT INTO messages VALUES (1, 1, NULL, 'Atlas', ?1, ?2, ?3, ?3)",
        params![format!("set {kv} then {tok}"), tags, ts],
    )
    .expect("message");
    c.execute(
        "INSERT INTO notable_contributors VALUES ('Atlas', 10, 1, ?1)",
        params![format!("found {url}")],
    )
    .expect("contributor");
    drop(c);
    let digests = tmp.path().join("distilled");
    std::fs::create_dir_all(&digests).expect("dir");
    std::fs::write(
        digests.join("leaky_20260105_101500.md"),
        format!("# Digest\n\n## Key Findings\n\n- use {url} with {tok}\n"),
    )
    .expect("digest");

    let r1 = import_sqlite(&t.pool, &path, "legacy", Some(&digests))
        .await
        .expect("import 1");
    assert_eq!(
        (
            r1.topics_inserted,
            r1.messages_inserted,
            r1.digests_inserted
        ),
        (1, 1, 1)
    );
    let r2 = import_sqlite(&t.pool, &path, "legacy", Some(&digests))
        .await
        .expect("import 2");
    assert_eq!(
        (
            r2.topics_existing,
            r2.messages_existing,
            r2.digests_existing
        ),
        (1, 1, 1)
    );
    // Another source dedups on the redacted content hash.
    let r3 = import_sqlite(&t.pool, &path, "other", None)
        .await
        .expect("import 3");
    assert_eq!(
        (r3.messages_inserted, r3.messages_existing),
        (0, 1),
        "{r3:?}"
    );

    let mut seen = String::new();
    for uri in [
        "/api/topics/leaky",
        "/api/topics/leaky/messages",
        "/api/digests?topic=leaky",
        "/api/leaderboard/aliases",
    ] {
        let (_, v) = t.get(uri).await;
        seen.push_str(&v.to_string());
    }
    let (_, dl) = t.get("/api/digests?topic=leaky").await;
    let (_, full) = t
        .get(&format!(
            "/api/digests/{}",
            dl[0]["id"].as_i64().expect("id")
        ))
        .await;
    seen.push_str(&full.to_string());
    let (_, sc) = sqlx::query_as::<_, (i64, String)>(
        "SELECT 0::bigint, coalesce(string_agg(reason, ' '), '') FROM score_events",
    )
    .fetch_one(&t.pool)
    .await
    .expect("reasons");
    seen.push_str(&sc);
    for leaked in [
        "abcdefghijklmnopqrstuvwxyz0123",
        "s3cr3tPw0rd9",
        "hunter2hunter2",
    ] {
        assert!(
            !seen.contains(leaked),
            "{leaked} survived the import: {seen}"
        );
    }
    assert!(seen.contains("REDACTED"), "{seen}");
    t.cleanup().await;
}
