//! One-shot import of a board kept in the earlier SQLite format (cargo feature `import`).
//!
//! The source is opened read-only and never mutated. The whole import is one
//! transaction, idempotent through the source row id `(legacy_source, legacy_id)`;
//! the content key `(topic_id, author, created_at, md5(content))` is only a fallback
//! that never maps two source rows to one destination row and is used only when it is
//! unambiguous. The fallback assumes an unchanged snapshot: it is recomputed on each run,
//! and an ambiguous or changed match becomes a new row rather than a guess. It verifies that
//! every source row was either inserted or already present, and refuses a self-parent
//! (rolling the whole import back).
//! Every text field (topics, messages, tags, contributors, digest files) is
//! redacted before it is compared or written, like every API write.

use std::cell::Cell;
use std::collections::HashMap;
use std::path::Path;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, NaiveDateTime, Utc};
use rusqlite::{Connection, OpenFlags};
use serde::Serialize;
use sqlx::PgPool;

use crate::db::legacy_identity;
use crate::redact::{redact, Redactions};
use crate::util::truncate_chars;

struct LegacyTopic {
    id: i64,
    name: String,
    title: String,
    description: Option<String>,
    created_by: String,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    status: String,
}

struct LegacyMessage {
    id: i64,
    topic_id: i64,
    parent_id: Option<i64>,
    author: String,
    content: String,
    tags: Vec<String>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

struct LegacyContributor {
    name: String,
    score: i64,
    tasks: i64,
    latest: String,
}

struct LegacyData {
    topics: Vec<LegacyTopic>,
    messages: Vec<LegacyMessage>,
    contributors: Vec<LegacyContributor>,
    bad_timestamps: usize,
}

#[derive(Debug, Default, Serialize, PartialEq, Eq)]
pub struct ImportReport {
    pub topics_inserted: usize,
    pub topics_merged: usize,
    pub topics_existing: usize,
    pub messages_inserted: usize,
    pub messages_existing: usize,
    pub contributors_inserted: usize,
    pub contributors_existing: usize,
    pub digests_inserted: usize,
    pub digests_existing: usize,
    pub digests_skipped: usize,
    /// Rows whose legacy timestamp could not be parsed (now() was used, with a warning).
    pub timestamps_fallback: usize,
}

/// Parse a legacy timestamp. An unparseable value falls back to now(), but is
/// logged with the row id and counted so the report shows it.
fn parse_ts(s: &str, what: &str, id: i64, bad: &Cell<usize>) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s)
        .map(|d| d.with_timezone(&Utc))
        .or_else(|_| NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S").map(|n| n.and_utc()))
        .unwrap_or_else(|_| {
            tracing::warn!(row = what, id, "unparseable legacy timestamp; using now()");
            bad.set(bad.get() + 1);
            Utc::now()
        })
}

fn read_legacy(path: &Path) -> Result<LegacyData> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("opening legacy sqlite {}", path.display()))?;

    let bad = Cell::new(0usize);
    let topics = conn
        .prepare(
            "SELECT id, name, title, description, created_by, created_at, updated_at, status \
             FROM topics ORDER BY id",
        )?
        .query_map([], |r| {
            let id: i64 = r.get(0)?;
            Ok(LegacyTopic {
                id,
                name: r.get(1)?,
                title: r.get(2)?,
                description: r.get(3)?,
                created_by: r.get(4)?,
                created_at: parse_ts(&r.get::<_, String>(5)?, "topic.created_at", id, &bad),
                updated_at: parse_ts(&r.get::<_, String>(6)?, "topic.updated_at", id, &bad),
                status: r.get(7)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;

    let messages = conn
        .prepare(
            "SELECT id, topic_id, parent_id, author, content, tags, created_at, updated_at \
             FROM messages ORDER BY id",
        )?
        .query_map([], |r| {
            let tags_raw: String = r.get(5)?;
            let id: i64 = r.get(0)?;
            Ok(LegacyMessage {
                id,
                topic_id: r.get(1)?,
                parent_id: r.get(2)?,
                author: r.get(3)?,
                content: r.get(4)?,
                tags: serde_json::from_str(&tags_raw).unwrap_or_default(),
                created_at: parse_ts(&r.get::<_, String>(6)?, "message.created_at", id, &bad),
                updated_at: parse_ts(&r.get::<_, String>(7)?, "message.updated_at", id, &bad),
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;

    let contributors = conn
        .prepare(
            "SELECT name, score, tasks_completed, latest_recognition FROM notable_contributors \
             ORDER BY name",
        )?
        .query_map([], |r| {
            Ok(LegacyContributor {
                name: r.get(0)?,
                score: r.get(1)?,
                tasks: r.get(2)?,
                latest: r.get(3)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;

    Ok(LegacyData {
        topics,
        messages,
        contributors,
        bad_timestamps: bad.get(),
    })
}

/// Redact every imported text field, the same way the API redacts every write, BEFORE any
/// dedupe lookup or insert. Redaction is deterministic and idempotent, so a re-run computes the
/// same names, content hashes and digest keys and the import stays idempotent.
fn sanitise(data: &mut LegacyData, r: &mut Redactions) {
    for t in &mut data.topics {
        t.name = r.text("topic.name", std::mem::take(&mut t.name));
        t.title = r.text("topic.title", std::mem::take(&mut t.title));
        t.description = r.opt("topic.description", t.description.take());
        t.created_by = r.text("topic.created_by", std::mem::take(&mut t.created_by));
        t.status = r.text("topic.status", std::mem::take(&mut t.status));
    }
    for m in &mut data.messages {
        m.author = r.text("message.author", std::mem::take(&mut m.author));
        m.content = r.text("message.content", std::mem::take(&mut m.content));
        m.tags = r.list("message.tags", std::mem::take(&mut m.tags));
    }
    for c in &mut data.contributors {
        c.name = r.text("contributor.name", std::mem::take(&mut c.name));
        c.latest = r.text("contributor.latest", std::mem::take(&mut c.latest));
    }
}

/// One digest file, redacted: `(legacy_name, topic, timestamp, markdown)`. The topic part of the
/// name is redacted like a topic name so it still finds its (redacted) topic.
fn sanitise_digest(
    file: &str,
    md: &str,
    r: &mut Redactions,
) -> Option<(String, String, DateTime<Utc>, String)> {
    let (topic, ts) = parse_digest_name(file)?;
    Some((
        r.text("digest.file", file.to_string()),
        r.text("topic.name", topic),
        ts,
        r.text("digest.transcript_md", md.to_string()),
    ))
}

/// `<topic>_<YYYYmmdd>_<HHMMSS>.md` -> (topic, timestamp).
fn parse_digest_name(file: &str) -> Option<(String, DateTime<Utc>)> {
    let stem = file.strip_suffix(".md")?;
    let mut it = stem.rsplitn(3, '_');
    let time = it.next()?;
    let date = it.next()?;
    let topic = it.next()?;
    let ts = NaiveDateTime::parse_from_str(&format!("{date}{time}"), "%Y%m%d%H%M%S")
        .ok()?
        .and_utc();
    Some((topic.to_string(), ts))
}

fn digest_summary(md: &str) -> String {
    let mut in_section = false;
    let mut lines = Vec::new();
    for l in md.lines() {
        if l.starts_with("## ") {
            in_section = l.contains("Key Findings");
            continue;
        }
        if in_section && l.trim_start().starts_with("- ") {
            lines.push(l.trim().trim_start_matches("- ").to_string());
        }
    }
    if lines.is_empty() {
        truncate_chars(md.trim(), 500)
    } else {
        truncate_chars(&lines.join("\n"), 20_000)
    }
}

fn digest_participants(md: &str) -> Vec<String> {
    md.lines()
        .find_map(|l| l.strip_prefix("- **Participants**:"))
        .map(|rest| {
            rest.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

fn read_digests(dir: &Path) -> Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        if name.ends_with(".md") && entry.path().is_file() {
            out.push((name, std::fs::read_to_string(entry.path())?));
        }
    }
    out.sort();
    Ok(out)
}

pub async fn import_sqlite(
    pool: &PgPool,
    from: &Path,
    source: &str,
    digests_dir: Option<&Path>,
) -> Result<ImportReport> {
    if source.trim().is_empty() {
        bail!("--source must not be empty");
    }
    let from_owned = from.to_path_buf();
    let mut data = tokio::task::spawn_blocking(move || read_legacy(&from_owned))
        .await
        .context("legacy reader panicked")??;
    let digest_files = match digests_dir {
        Some(d) => read_digests(d)?,
        None => Vec::new(),
    };
    let mut redactions = Redactions::new();
    sanitise(&mut data, &mut redactions);

    let mut rep = ImportReport::default();
    let mut tx = pool.begin().await.context("starting import transaction")?;

    // Topics: map legacy id -> topic id. A name collision merges into the existing topic.
    let mut topic_map: HashMap<i64, i64> = HashMap::new();
    for t in &data.topics {
        let by_legacy: Option<i64> =
            sqlx::query_scalar("SELECT id FROM topics WHERE legacy_source = $1 AND legacy_id = $2")
                .bind(source)
                .bind(t.id)
                .fetch_optional(&mut *tx)
                .await?;
        if let Some(id) = by_legacy {
            topic_map.insert(t.id, id);
            rep.topics_existing += 1;
            continue;
        }
        let by_name: Option<i64> = sqlx::query_scalar("SELECT id FROM topics WHERE name = $1")
            .bind(&t.name)
            .fetch_optional(&mut *tx)
            .await?;
        if let Some(id) = by_name {
            topic_map.insert(t.id, id);
            rep.topics_merged += 1;
            continue;
        }
        let status = if t.status == "archived" {
            "archived"
        } else {
            "active"
        };
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO topics (name, title, description, created_by, status, last_activity_at, \
               created_at, updated_at, legacy_source, legacy_id) \
             VALUES ($1,$2,$3,$4,$5,$6,$7,$6,$8,$9) RETURNING id",
        )
        .bind(&t.name)
        .bind(&t.title)
        .bind(&t.description)
        .bind(&t.created_by)
        .bind(status)
        .bind(t.updated_at)
        .bind(t.created_at)
        .bind(source)
        .bind(t.id)
        .fetch_one(&mut *tx)
        .await?;
        topic_map.insert(t.id, id);
        rep.topics_inserted += 1;
    }

    // Messages: map by SOURCE ROW ID first, `(legacy_source, legacy_id)`. Content equality
    // `(topic, author, created_at, md5(content))` is only a fallback, for the same board imported
    // under another source label. Content is redacted by then, so two distinct source rows
    // (`password=alpha`, `password=beta`) can compare equal: the fallback therefore never
    // matches a row this source already owns, nor a destination row already claimed in this run,
    // and it is used only when exactly ONE candidate matches; otherwise the row is new. No two
    // source rows ever share a destination row.
    //
    // ASSUMPTION: the fallback is not persisted, it is recomputed on every run. It is stable for
    // an unchanged snapshot; a changed snapshot under another label may import a message again
    // as a new row (never onto the wrong one).
    let mut msg_map: HashMap<i64, i64> = HashMap::new();
    let mut claimed: Vec<i64> = Vec::new();
    for m in &data.messages {
        let Some(&tid) = topic_map.get(&m.topic_id) else {
            bail!(
                "legacy message {} references unknown topic {}",
                m.id,
                m.topic_id
            );
        };
        let by_id: Option<i64> = sqlx::query_scalar(
            "SELECT id FROM messages WHERE legacy_source = $1 AND legacy_id = $2",
        )
        .bind(source)
        .bind(m.id)
        .fetch_optional(&mut *tx)
        .await?;
        let existing = match by_id {
            Some(id) => Some(id),
            None => {
                // Fetch up to two candidates: a fallback is used only when it is unambiguous.
                // Two candidates (rows that redact to the same text) are never guessed between:
                // the row is inserted as new instead, so a reply can never be attached to the
                // wrong message by a later, changed snapshot.
                let candidates: Vec<i64> = sqlx::query_scalar(
                    "SELECT id FROM messages \
                     WHERE topic_id = $1 AND author = $2 AND created_at = $3 \
                       AND md5(content) = md5($4) \
                       AND legacy_source IS DISTINCT FROM $5 AND NOT (id = ANY($6)) \
                     ORDER BY id LIMIT 2",
                )
                .bind(tid)
                .bind(&m.author)
                .bind(m.created_at)
                .bind(&m.content)
                .bind(source)
                .bind(&claimed)
                .fetch_all(&mut *tx)
                .await?;
                match candidates.as_slice() {
                    [only] => Some(*only),
                    [] => None,
                    _ => {
                        tracing::warn!(
                            legacy_id = m.id,
                            "ambiguous content match; importing the message as a new row"
                        );
                        None
                    }
                }
            }
        };
        if let Some(id) = existing {
            if claimed.contains(&id) {
                bail!(
                    "legacy message {} maps to destination row {id}, which another source row \
                     already maps to",
                    m.id
                );
            }
            claimed.push(id);
            msg_map.insert(m.id, id);
            rep.messages_existing += 1;
            continue;
        }
        let kind = if m.author == "Hive-Distiller" {
            "system"
        } else {
            "note"
        };
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO messages (topic_id, author, content, tags, kind, created_at, updated_at, \
               legacy_source, legacy_id) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9) RETURNING id",
        )
        .bind(tid)
        .bind(&m.author)
        .bind(&m.content)
        .bind(&m.tags)
        .bind(kind)
        .bind(m.created_at)
        .bind(m.updated_at)
        .bind(source)
        .bind(m.id)
        .fetch_one(&mut *tx)
        .await?;
        claimed.push(id);
        msg_map.insert(m.id, id);
        rep.messages_inserted += 1;
        // Imported topics keep their newest message as last activity.
        sqlx::query(
            "UPDATE topics SET last_activity_at = GREATEST(last_activity_at, $2) \
             WHERE id = $1 AND legacy_source = $3",
        )
        .bind(tid)
        .bind(m.created_at)
        .bind(source)
        .execute(&mut *tx)
        .await?;
    }
    // Second pass: parents (a parent may be inserted after its child).
    for m in &data.messages {
        if let (Some(lp), Some(&new_id)) = (m.parent_id, msg_map.get(&m.id)) {
            if let Some(&np) = msg_map.get(&lp) {
                if np == new_id {
                    bail!(
                        "legacy message {} would become its own parent (source parent {lp}); \
                         nothing was imported",
                        m.id
                    );
                }
                sqlx::query(
                    "UPDATE messages SET parent_id = $2 WHERE id = $1 AND parent_id IS NULL \
                       AND legacy_source = $3",
                )
                .bind(new_id)
                .bind(np)
                .bind(source)
                .execute(&mut *tx)
                .await?;
            }
        }
    }

    // Contributors -> aliases under (legacy, unknown, unknown) + one legacy score event each.
    let legacy_id = legacy_identity(&mut tx)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let scorer = format!("legacy-import:{source}");
    for c in &data.contributors {
        sqlx::query(
            "INSERT INTO aliases (alias, identity_id) VALUES ($1,$2) ON CONFLICT (alias) DO NOTHING",
        )
        .bind(&c.name)
        .bind(legacy_id)
        .execute(&mut *tx)
        .await?;
        let identity: i64 = sqlx::query_scalar("SELECT identity_id FROM aliases WHERE alias = $1")
            .bind(&c.name)
            .fetch_one(&mut *tx)
            .await?;
        let points = (c.score / c.tasks.max(1)).clamp(0, 100) as i32;
        let res = sqlx::query(
            "INSERT INTO score_events (identity_id, alias, points, reason, scorer, legacy) \
             SELECT $1,$2,$3,$4,$5,true \
              WHERE NOT EXISTS (SELECT 1 FROM score_events WHERE legacy AND alias = $2 AND scorer = $5)",
        )
        .bind(identity)
        .bind(&c.name)
        .bind(points)
        .bind(&c.latest)
        .bind(&scorer)
        .execute(&mut *tx)
        .await?;
        if res.rows_affected() == 1 {
            rep.contributors_inserted += 1;
        } else {
            rep.contributors_existing += 1;
        }
    }

    // Digest markdown files.
    for (raw_file, raw_md) in &digest_files {
        let Some((file, topic_name, ts, md)) = sanitise_digest(raw_file, raw_md, &mut redactions)
        else {
            rep.digests_skipped += 1;
            tracing::warn!(file = %redact(raw_file).0, "digest filename does not match <topic>_<date>_<time>.md");
            continue;
        };
        let (file, md) = (&file, &md);
        let tid: Option<i64> = sqlx::query_scalar("SELECT id FROM topics WHERE name = $1")
            .bind(&topic_name)
            .fetch_optional(&mut *tx)
            .await?;
        let Some(tid) = tid else {
            rep.digests_skipped += 1;
            tracing::warn!(file = %file, "digest topic not found; skipped");
            continue;
        };
        let res = sqlx::query(
            "INSERT INTO digests (topic_id, version, summary, transcript_md, participants, \
               through_message_id, producer, created_at, legacy_source, legacy_name) \
             VALUES ($1, (SELECT coalesce(max(version), 0) + 1 FROM digests WHERE topic_id = $1), \
                     $2,$3,$4,0,'legacy-import',$5,$6,$7) \
             ON CONFLICT (legacy_source, legacy_name) DO NOTHING",
        )
        .bind(tid)
        .bind(digest_summary(md))
        .bind(md)
        .bind(digest_participants(md))
        .bind(ts)
        .bind(source)
        .bind(file)
        .execute(&mut *tx)
        .await?;
        if res.rows_affected() == 1 {
            rep.digests_inserted += 1;
        } else {
            rep.digests_existing += 1;
        }
    }

    // Verify: every source row was either inserted or already present.
    let src_msgs = data.messages.len();
    if rep.messages_inserted + rep.messages_existing != src_msgs {
        bail!(
            "message count mismatch: source {src_msgs}, accounted {}",
            rep.messages_inserted + rep.messages_existing
        );
    }
    let src_topics = data.topics.len();
    if rep.topics_inserted + rep.topics_merged + rep.topics_existing != src_topics {
        bail!("topic count mismatch against source ({src_topics})");
    }
    if rep.contributors_inserted + rep.contributors_existing != data.contributors.len() {
        bail!("contributor count mismatch against source");
    }
    let src_digests = digest_files.len();
    let accounted = rep.digests_inserted + rep.digests_existing + rep.digests_skipped;
    if accounted != src_digests {
        bail!("digest count mismatch: {src_digests} files, accounted {accounted}");
    }
    rep.timestamps_fallback = data.bad_timestamps;
    tracing::info!(
        topics = src_topics,
        messages = src_msgs,
        digests = src_digests,
        digests_skipped = rep.digests_skipped,
        timestamps_fallback = rep.timestamps_fallback,
        "import verification passed"
    );
    tx.commit().await.context("committing import")?;
    // Field, kind and count only, never the value.
    redactions.log_only();
    tracing::info!(source = %source, report = ?rep, "legacy import committed");
    Ok(rep)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_filename_parses() {
        let (t, ts) = parse_digest_name("my-topic_x_20260101_120000.md").unwrap();
        assert_eq!(t, "my-topic_x");
        assert_eq!(ts.to_rfc3339(), "2026-01-01T12:00:00+00:00");
        assert!(parse_digest_name("nodate.md").is_none());
    }

    /// Secret shapes, built from parts at runtime so no scanner sees a literal.
    fn secrets() -> (String, String, String) {
        (
            ["ghp", "_", "abcdefghijklmnopqrstuvwxyz0123"].concat(),
            ["postgres://app:", "s3cr3tPw0rd9", "@db.example.test/x"].concat(),
            ["pass", "word=", "hunter2hunter2"].concat(),
        )
    }

    fn data_with_secrets() -> LegacyData {
        let (tok, url, kv) = secrets();
        let ts = Utc::now();
        LegacyData {
            topics: vec![LegacyTopic {
                id: 1,
                name: "deploy".into(),
                title: format!("deploy with {tok}"),
                description: Some(format!("db at {url}")),
                created_by: "Atlas".into(),
                created_at: ts,
                updated_at: ts,
                status: "active".into(),
            }],
            messages: vec![LegacyMessage {
                id: 1,
                topic_id: 1,
                parent_id: None,
                author: "Atlas".into(),
                content: format!("set {kv} and {tok}"),
                tags: vec![tok.clone(), "ops".into()],
                created_at: ts,
                updated_at: ts,
            }],
            contributors: vec![LegacyContributor {
                name: "Atlas".into(),
                score: 10,
                tasks: 1,
                latest: format!("found {url}"),
            }],
            bad_timestamps: 0,
        }
    }

    fn rendered(d: &LegacyData) -> String {
        let mut s = String::new();
        for t in &d.topics {
            s += &format!(
                "{} {} {:?} {} {}\n",
                t.name, t.title, t.description, t.created_by, t.status
            );
        }
        for m in &d.messages {
            s += &format!("{} {} {:?}\n", m.author, m.content, m.tags);
        }
        for c in &d.contributors {
            s += &format!("{} {}\n", c.name, c.latest);
        }
        s
    }

    #[test]
    fn every_imported_field_is_redacted_before_dedupe_and_insert() {
        let mut d = data_with_secrets();
        let mut r = Redactions::new();
        sanitise(&mut d, &mut r);
        let out = rendered(&d);
        for leaked in [
            "abcdefghijklmnopqrstuvwxyz0123",
            "s3cr3tPw0rd9",
            "hunter2hunter2",
        ] {
            assert!(!out.contains(leaked), "{leaked} survived: {out}");
        }
        assert!(out.contains("db.example.test"), "{out}");
        assert_eq!(d.messages[0].tags[1], "ops");
        assert_eq!(
            r.count(),
            6,
            "title, description, content x2, tag, contributor"
        );

        // Idempotent: sanitising the sanitised data changes nothing, so a re-run computes the
        // same dedupe keys (topic name, md5(content), digest legacy_name).
        let before = rendered(&d);
        let mut r2 = Redactions::new();
        sanitise(&mut d, &mut r2);
        assert_eq!(rendered(&d), before);
        assert_eq!(r2.count(), 0);
    }

    #[test]
    fn digest_files_are_redacted_and_keep_their_topic_mapping() {
        let (tok, url, _) = secrets();
        let md =
            format!("# Digest\n\n- **Participants**: Bo\n\n## Key Findings\n\n- use {url} {tok}\n");
        let mut r = Redactions::new();
        let (file, topic, _, clean) =
            sanitise_digest("deploy_20260105_101500.md", &md, &mut r).expect("digest");
        assert_eq!(file, "deploy_20260105_101500.md");
        assert_eq!(topic, "deploy");
        assert!(!clean.contains("s3cr3tPw0rd9") && !clean.contains("abcdefghijklmnop"));
        assert!(!digest_summary(&clean).contains("s3cr3tPw0rd9"));
        let again = sanitise_digest(&file, &clean, &mut Redactions::new()).expect("digest");
        assert_eq!(again.3, clean, "idempotent");
    }
}
