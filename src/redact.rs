//! Server-side secret redaction. The server redacts, it never rejects: a
//! rejection would hide real findings ("found a leaked key in X") and bare
//! prefixes false-positive on ordinary words ("task-kind", "risk-based").
//!
//! SOURCE: the anchored patterns are copied from `nuvek-core/src/scrub.rs`
//! (`SECRET_RE`), split per kind so an audit event can say what was redacted.
//! Keep the two in step when either changes. Differences: the first `sk-` branch
//! gets an explicit left boundary here (the Rust `regex` crate has no
//! lookbehind), and three extra shapes are added (`github_pat_`, `glpat-`, long
//! `Bearer` tokens). Public certificates are not secrets: only `PRIVATE KEY`
//! PEM blocks match. Configuration-shaped credentials (URL userinfo,
//! `password=value`, JSON and XML secret keys) mirror `redact_config` and are
//! applied after the token shapes.
//!
//! RESIDUAL (accepted): a credential with no recognisable prefix is invisible
//! to pattern matching. hive-mcp also redacts client-side; this is the backstop.

use std::sync::LazyLock;

use regex::Regex;
use sqlx::PgConnection;

use crate::db::add_topic_event;
use crate::error::ApiResult;
use crate::models::AgentIdent;

/// `(kind, pattern)`; more specific shapes first. Patterns are compile-time constants.
const PATTERNS: [(&str, &str); 10] = [
    (
        "discord-webhook",
        r"discord(?:app)?\.com/api/webhooks/[0-9]+/[A-Za-z0-9_\-]{20,}",
    ),
    // DELIBERATE FAIL-SAFE: the `|\z` alternative redacts from an unterminated
    // BEGIN line to the end of the text. A truncated or half-pasted key block
    // must never leave its body behind; over-redacting the tail is the accepted
    // cost.
    (
        "private-key",
        r"(?s)-----BEGIN [A-Z ]*PRIVATE KEY-----.*?(?:-----END [A-Z ]*PRIVATE KEY-----|\z)",
    ),
    (
        "jwt",
        r"eyJ[A-Za-z0-9_\-]{10,}\.[A-Za-z0-9_\-]{10,}\.[A-Za-z0-9_\-]{10,}",
    ),
    (
        "github-token",
        r"github_pat_[A-Za-z0-9_]{20,}|ghp_[A-Za-z0-9]{20,}|gh[oprsu]_[A-Za-z0-9]{20,}",
    ),
    ("gitlab-token", r"glpat-[A-Za-z0-9_\-]{20,}"),
    // Two branches. The alnum form catches `sk-` glued to a non-letter (`KEY_sk-...`);
    // group 1 is the boundary (start of text or a non-letter), kept in the output, so
    // letters before `sk-` (`disk-<hash>`, `flask-<hash>`) do not match. The URL-safe
    // form carries `-`/`_`, and its `\b` keeps hyphenated prose ("disk-usage-...",
    // "task-kind") from matching.
    (
        "api-key",
        r"(^|[^A-Za-z])sk-[A-Za-z0-9]{20,}|\bsk-[A-Za-z0-9_\-]{20,}",
    ),
    ("google-key", r"AIza[0-9A-Za-z_\-]{20,}"),
    ("aws-key", r"AKIA[0-9A-Z]{16}"),
    ("slack-token", r"xox[baprs]-[0-9A-Za-z\-]{10,}"),
    // A long `Bearer <token>` credential pasted from a header or a curl line.
    ("bearer", r"(?i)\bbearer\s+[A-Za-z0-9_\-\.=]{30,}"),
];

/// A secret-named key, as in `nuvek-core`'s `CONFIG_SECRET_RES`: `password`, `db_password`,
/// `api_key`, `client_secret`, `access_token`, `auth`, `pass`, `smtp-pass`, ...
const SECRET_KEY: &str = r"(?:[A-Za-z0-9_-]*?(?:api[_-]?key|apikey|password|passwd|secret|token|private[_-]?key|private|preshared[_-]?key|auth)|(?:[A-Za-z0-9]*[_-])?pass)";

/// Kinds of the configuration shapes below.
const CONFIG_KINDS: [&str; 2] = ["config-secret", "url-credential"];

/// Configuration-shaped credentials that carry no token prefix, mirroring `nuvek-core`'s
/// `redact_config`: `(kind, pattern, open, close)`. Each pattern captures the kept prefix as
/// group 1 (the key, or the URL scheme and user), the secret as group `v`, and an optional kept
/// suffix as group `s`; `open`/`close` wrap the placeholder (quotes for JSON, `@` for a URL).
///
/// Idempotence: a value is left alone only when it IS a complete placeholder this redactor
/// emits (see [`is_placeholder`]). Any other value is masked, including one that merely starts
/// with `[` (`password=[hunter2]`).
///
/// Not mirrored: `nuvek-core`'s bare 43-character base64 shape, which would cut into inline
/// `data:` images that the board renders.
static CONFIG_PATTERNS: LazyLock<Vec<(&'static str, String, &'static str, &'static str)>> =
    LazyLock::new(|| {
        let k = SECRET_KEY;
        vec![
            // JSON: "password": "value"
            (
                "config-secret",
                // Escape-aware: `\"` inside the value does not end it.
                format!(r#"(?i)("{k}"\s*:\s*)"(?P<v>(?:\\.|[^"\\])+)""#),
                "\"",
                "\"",
            ),
            // XML: <password>value</password>
            (
                "config-secret",
                format!(r"(?i)(<{k}>)(?P<v>[^<]+)(?P<s></)"),
                "",
                "",
            ),
            // password=value, PASSWORD: value, api_key = 'value'
            (
                "config-secret",
                // Quoted scalars are escape-aware too (`"ab\"cd"`, `'ab\'cd'`).
                format!(
                    r#"(?i)(\b{k}\s*[:=]\s*)(?P<v>"(?:\\.|[^"\\])*"|'(?:\\.|[^'\\])*'|[^\s"']\S*)"#
                ),
                "",
                "",
            ),
            // URL userinfo: scheme://user:password@host (the user may be empty). The password
            // stops at `/?#@`, which cannot appear unencoded in userinfo, so
            // `https://host:8080/a@b` is safe.
            (
                "url-credential",
                r"(?i)\b([a-z][a-z0-9+.\-]*://[^\s:/@]*:)(?P<v>[^\s/?#@]+)@".to_string(),
                "",
                "@",
            ),
        ]
    });

/// Exactly one placeholder this redactor emits (`[REDACTED:<known kind>]`), optionally quoted
/// and optionally followed by trailing punctuation from the surrounding text. Nothing else
/// counts, so `[hunter2]` or `[REDACTED:made-up]x` is still masked.
fn is_placeholder(v: &str) -> bool {
    static RE: LazyLock<Option<Regex>> =
        LazyLock::new(|| Regex::new(r#"^(["']?)\[REDACTED:([a-z-]+)\](["']?)[,;.)}\]]*$"#).ok());
    let Some(c) = RE.as_ref().and_then(|re| re.captures(v)) else {
        return false;
    };
    let quotes_match = c.get(1).map(|m| m.as_str()) == c.get(3).map(|m| m.as_str());
    let kind = c.get(2).map_or("", |m| m.as_str());
    quotes_match && (PATTERNS.iter().any(|(k, _)| *k == kind) || CONFIG_KINDS.contains(&kind))
}

/// Token shapes: `(kind, regex, replacement)`. `${1}` keeps a pattern's boundary capture
/// (empty when it has none).
static COMPILED: LazyLock<Vec<(&'static str, Option<Regex>, String)>> = LazyLock::new(|| {
    // A pattern that failed to compile would be a programming error caught by the
    // unit tests; at runtime it is skipped rather than panicking.
    PATTERNS
        .iter()
        .map(|(kind, pat)| {
            (
                *kind,
                Regex::new(pat).ok(),
                format!("${{1}}[REDACTED:{kind}]"),
            )
        })
        .collect()
});

/// One compiled configuration shape: `(kind, regex, open, close)`.
type ConfigShape = (&'static str, Option<Regex>, &'static str, &'static str);

/// Configuration shapes, compiled.
static COMPILED_CONFIG: LazyLock<Vec<ConfigShape>> = LazyLock::new(|| {
    CONFIG_PATTERNS
        .iter()
        .map(|(kind, pat, open, close)| (*kind, Regex::new(pat).ok(), *open, *close))
        .collect()
});

fn add_hit(hits: &mut Vec<(&'static str, usize)>, kind: &'static str, n: usize) {
    if n == 0 {
        return;
    }
    match hits.iter_mut().find(|(k, _)| *k == kind) {
        Some(h) => h.1 += n,
        None => hits.push((kind, n)),
    }
}

/// Redact `text`. Returns the clean text and `(kind, count)` per kind that matched.
/// Idempotent: placeholders do not match any token pattern, and a configuration value that is
/// already a placeholder is kept as it is (and not counted).
pub fn redact(text: &str) -> (String, Vec<(&'static str, usize)>) {
    let mut out = text.to_string();
    let mut hits: Vec<(&'static str, usize)> = Vec::new();
    for (kind, re, rep) in COMPILED.iter() {
        let Some(re) = re else { continue };
        let n = re.find_iter(&out).count();
        if n > 0 {
            out = re.replace_all(&out, rep.as_str()).into_owned();
            add_hit(&mut hits, kind, n);
        }
    }
    for (kind, re, open, close) in COMPILED_CONFIG.iter() {
        let Some(re) = re else { continue };
        let mut n = 0usize;
        let next = re
            .replace_all(&out, |c: &regex::Captures| {
                let whole = c.get(0).map_or("", |m| m.as_str());
                let v = c.name("v").map_or("", |m| m.as_str());
                if is_placeholder(v) {
                    return whole.to_string();
                }
                n += 1;
                format!(
                    "{}{open}[REDACTED:{kind}]{close}{}",
                    c.get(1).map_or("", |m| m.as_str()),
                    c.name("s").map_or("", |m| m.as_str())
                )
            })
            .into_owned();
        out = next;
        add_hit(&mut hits, kind, n);
    }
    (out, hits)
}

/// Collects redactions across the fields of one request.
#[derive(Default)]
pub struct Redactions {
    hits: Vec<(String, &'static str, usize)>,
}

impl Redactions {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn text(&mut self, field: &str, s: String) -> String {
        let (clean, hits) = redact(&s);
        for (kind, n) in hits {
            self.hits.push((field.to_string(), kind, n));
        }
        clean
    }

    pub fn opt(&mut self, field: &str, s: Option<String>) -> Option<String> {
        s.map(|v| self.text(field, v))
    }

    pub fn list(&mut self, field: &str, v: Vec<String>) -> Vec<String> {
        v.into_iter().map(|s| self.text(field, s)).collect()
    }

    /// Redact every string inside a JSON value (keys are left alone).
    pub fn json(&mut self, field: &str, v: serde_json::Value) -> serde_json::Value {
        use serde_json::Value;
        match v {
            Value::String(s) => Value::String(self.text(field, s)),
            Value::Array(a) => Value::Array(a.into_iter().map(|x| self.json(field, x)).collect()),
            Value::Object(m) => Value::Object(
                m.into_iter()
                    .map(|(k, x)| {
                        let x = self.json(field, x);
                        (k, x)
                    })
                    .collect(),
            ),
            other => other,
        }
    }

    /// Redact an optional list (before any truncation or de-duplication).
    pub fn opt_list(&mut self, field: &str, v: Option<Vec<String>>) -> Option<Vec<String>> {
        v.map(|items| self.list(field, items))
    }

    /// Redact the strings of an agent identity.
    pub fn agent(&mut self, a: Option<AgentIdent>) -> Option<AgentIdent> {
        a.map(|a| AgentIdent {
            agent_type: self.text("agent.agent_type", a.agent_type),
            model: self.text("agent.model", a.model),
            harness: self.text("agent.harness", a.harness),
        })
    }

    /// Total number of redactions (for tests).
    pub fn count(&self) -> usize {
        self.hits.iter().map(|(_, _, n)| n).sum()
    }

    fn merged(&self) -> Vec<(&str, &str, usize)> {
        let mut merged: Vec<(&str, &str, usize)> = Vec::new();
        for (field, kind, n) in &self.hits {
            match merged.iter_mut().find(|(f, k, _)| f == field && k == kind) {
                Some(m) => m.2 += n,
                None => merged.push((field.as_str(), kind, *n)),
            }
        }
        merged
    }

    /// For routes with no topic in scope (proposals, kv, gardener runs): log only.
    pub fn log_only(&self) {
        for (field, kind, count) in self.merged() {
            tracing::warn!(field, kind, count, "secret-shaped text redacted");
        }
    }

    /// Record `topic_events` `secret-redacted` rows (one per field and kind, never the
    /// value). Without a topic in scope the same facts go to the log.
    pub async fn record(
        &self,
        conn: &mut PgConnection,
        topic_id: Option<i64>,
        actor: &str,
    ) -> ApiResult<()> {
        for (field, kind, count) in self.merged() {
            tracing::warn!(field, kind, count, "secret-shaped text redacted");
            if let Some(tid) = topic_id {
                add_topic_event(
                    conn,
                    tid,
                    "secret-redacted",
                    actor,
                    serde_json::json!({"field": field, "kind": kind, "count": count}),
                )
                .await?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_patterns_compile() {
        assert!(COMPILED.iter().all(|(_, re, _)| re.is_some()));
        assert!(COMPILED_CONFIG.iter().all(|(_, re, _, _)| re.is_some()));
        assert!(is_placeholder("[REDACTED:config-secret]"));
    }

    #[test]
    fn escaped_quotes_do_not_end_a_quoted_secret() {
        // Built from parts at runtime so no scanner sees a credential-shaped literal.
        let tail = ["secret", "-tail-", "9f8e7d"].concat();
        for (input, want) in [
            (
                format!(r#"{{"password":"ab\"{tail}"}}"#),
                r#"{"password":"[REDACTED:config-secret]"}"#.to_string(),
            ),
            (
                format!(r#"{{"api_key": "x\\\"{tail}", "n": 1}}"#),
                r#"{"api_key": "[REDACTED:config-secret]", "n": 1}"#.to_string(),
            ),
            (
                format!(r#"password="ab\"{tail}" next"#),
                "password=[REDACTED:config-secret] next".to_string(),
            ),
            (
                format!(r"token='ab\'{tail}' next"),
                "token=[REDACTED:config-secret] next".to_string(),
            ),
        ] {
            let (out, hits) = redact(&input);
            assert!(!out.contains(&tail), "{input} -> {out}");
            assert_eq!(out, want, "{input}");
            assert_eq!(hits.iter().map(|h| h.1).sum::<usize>(), 1, "{input}");
            assert_eq!(redact(&out).0, out, "idempotent: {input}");
        }
    }

    #[test]
    fn bracket_prefixed_secrets_are_masked_and_only_real_placeholders_are_kept() {
        // Built from parts at runtime so no scanner sees a credential-shaped literal.
        let pw = ["s3cr3t", "Pw0rd9"].concat();
        for input in [
            format!("password=[{pw}]"),
            format!("password: [{pw}"),
            format!(r#"{{"password": "[{pw}]"}}"#),
            format!("<password>[{pw}]</password>"),
            format!("postgres://app:[{pw}]@db.example.test/x"),
            format!("token=[REDACTED:made-up]{pw}"),
            format!("token=[REDACTED:config-secret]{pw}"),
            r#"{"secret": "[REDACTED:not-a-kind]"}"#.to_string(),
        ] {
            let (out, hits) = redact(&input);
            assert!(!out.contains(&pw), "{input} -> {out}");
            assert!(!hits.is_empty(), "{input}");
            let (again, rehits) = redact(&out);
            assert_eq!(again, out, "idempotent: {input}");
            assert!(rehits.is_empty(), "counted again: {input} {rehits:?}");
        }
        // The not-a-kind case is masked, not kept.
        let (out, _) = redact(r#"{"secret": "[REDACTED:not-a-kind]"}"#);
        assert_eq!(out, r#"{"secret": "[REDACTED:config-secret]"}"#);
        // Genuine placeholders, also with trailing punctuation, are left alone.
        for kept in [
            "password=[REDACTED:config-secret]",
            "password=[REDACTED:config-secret],",
            r#"{"password": "[REDACTED:config-secret]"}"#,
            "<password>[REDACTED:config-secret]</password>",
            "postgres://app:[REDACTED:url-credential]@db.example.test/x",
            "GITHUB_TOKEN=[REDACTED:github-token]",
            "token='[REDACTED:api-key]'",
        ] {
            let (out, hits) = redact(kept);
            assert_eq!(out, kept);
            assert!(hits.is_empty(), "{kept}: {hits:?}");
        }
    }

    #[test]
    fn config_shaped_credentials_are_redacted_idempotently() {
        // Built from parts at runtime so no scanner sees a credential-shaped literal.
        let pw = ["s3cr3t", "Pw0rd9"].concat();
        let url = format!("postgres://app:{pw}@db.example.test:5432/hive");
        let cases = [
            (
                format!("connect {url} now"),
                "postgres://app:[REDACTED:url-credential]@db.example.test:5432/hive",
            ),
            (
                format!("redis://:{pw}@cache.example.test"),
                "redis://:[REDACTED:url-credential]@cache.example.test",
            ),
            (
                format!("DB_PASSWORD={pw}"),
                "DB_PASSWORD=[REDACTED:config-secret]",
            ),
            (
                format!("password: {pw}"),
                "password: [REDACTED:config-secret]",
            ),
            (
                format!("api_key = '{pw} more'"),
                "api_key = [REDACTED:config-secret]",
            ),
            (
                format!(r#"{{"client_secret": "{pw}"}}"#),
                r#"{"client_secret": "[REDACTED:config-secret]"}"#,
            ),
            (
                format!("<password>{pw}</password>"),
                "<password>[REDACTED:config-secret]</password>",
            ),
        ];
        for (input, want) in cases {
            let (out, hits) = redact(&input);
            assert!(!out.contains(&pw), "{input} -> {out}");
            assert!(out.contains(want), "{input} -> {out}");
            assert_eq!(
                hits.iter().map(|h| h.1).sum::<usize>(),
                1,
                "{input}: {hits:?}"
            );
            let (again, rehits) = redact(&out);
            assert_eq!(again, out, "idempotent");
            assert!(
                rehits.is_empty(),
                "a placeholder is never counted again: {rehits:?}"
            );
        }
        // A token-shaped value behind a secret key is redacted once, as the token.
        let (out, hits) = redact(&format!(
            "GITHUB_TOKEN={}",
            ["ghp", "_", "abcdefghijklmnopqrstuvwxyz0123"].concat()
        ));
        assert_eq!(out, "GITHUB_TOKEN=[REDACTED:github-token]");
        assert_eq!(hits, vec![("github-token", 1)]);
    }

    #[test]
    fn config_patterns_leave_ordinary_text_alone() {
        for s in [
            "https://example.test:8080/path@v2",
            "max_tokens: 512 and tokens=12",
            "bypass=1, compass: north",
            "the password policy needs review",
            "see https://user@example.test/repo",
        ] {
            let (out, hits) = redact(s);
            assert!(hits.is_empty(), "false positive on {s:?}: {out}");
        }
    }

    // Parity with nuvek-core's scrub fixtures (fabricated / documented examples).
    #[test]
    fn redacts_every_nuvek_core_fixture() {
        let cases = [
            "AIzaSyABCDEFGHIJKLMNOPQRSTUVWXYZ012345", // gitleaks:allow test fixture
            "AKIAIOSFODNN7EXAMPLE",                   // gitleaks:allow test fixture
            "xoxb-123456789012-abcdefghijklmnop",     // gitleaks:allow test fixture
            "eyJhbGciOiJIUzI1.eyJzdWIiOiIxMjM0NTY.SflKxwRJSMeKKF", // gitleaks:allow test fixture
            "-----BEGIN RSA PRIVATE KEY-----",        // gitleaks:allow test fixture
            "https://discord.com/api/webhooks/1234567890/AbCdEfGhIjKlMnOpQrStUvWxYz_0123-xyz", // gitleaks:allow test fixture
            "ghp_abcdefghijklmnopqrstuvwxyz0123", // gitleaks:allow test fixture
            "sk-abcdefghijklmnopqrstuvwx",        // gitleaks:allow test fixture
            "sk-X2ab_cd-EfGhIjKlMnOpQr_s",        // gitleaks:allow test fixture
            "OPENAI_sk-abcdefghijklmnopqrstuvwx", // gitleaks:allow test fixture
            "github_pat_11ABCDEFG0123456789_abcdefghijklmnop", // gitleaks:allow test fixture
            "glpat-abcdefghijklmnopqrstuv",       // gitleaks:allow test fixture
        ];
        for c in cases {
            let (out, hits) = redact(c);
            assert!(!hits.is_empty(), "should detect: {c}");
            assert!(out.contains("[REDACTED:"), "should redact: {c} -> {out}");
        }
        let (out, _) = redact("OPENAI_API_KEY=sk-X2ab_cd-EfGhIjKlMnOpQr_s done"); // gitleaks:allow
        assert!(!out.contains("_cd-EfGh") && !out.contains("Qr_s"), "{out}");
    }

    #[test]
    fn no_false_positives_on_prose_or_public_certs() {
        for s in [
            "task-kind",
            "risk-based",
            "disk-a1b2c3d4e5f6g7h8i9j0k1",
            "flask-0123456789abcdefghijk",
            "see the disk-usage-report-for-the-nas-array and risk-assessment-notes",
            "-----BEGIN CERTIFICATE-----",
            "-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----",
            "connect to 198.51.100.7:8000",
        ] {
            let (out, hits) = redact(s);
            assert!(hits.is_empty(), "false positive on {s:?}");
            assert_eq!(out, s);
        }
    }

    #[test]
    fn private_key_block_is_redacted_whole_and_idempotent() {
        let pem = "before\n-----BEGIN PRIVATE KEY-----\nMIIEvQIBADANBgkq\n-----END PRIVATE KEY-----\nafter"; // gitleaks:allow test fixture
        let (out, hits) = redact(pem);
        assert_eq!(hits, vec![("private-key", 1)]);
        assert!(!out.contains("MIIEvQ"));
        assert!(out.starts_with("before") && out.ends_with("after"));
        assert_eq!(redact(&out).0, out);
    }

    #[test]
    fn sk_boundary_character_is_kept() {
        // Underscore, `=`, space and start-of-text are all valid left boundaries.
        let key = "sk-abcdefghijklmnopqrstuvwx"; // gitleaks:allow invented fixture
        for (input, expected) in [
            (format!("OPENAI_{key}"), "OPENAI_[REDACTED:api-key]"),
            (format!("K={key} done"), "K=[REDACTED:api-key] done"),
            (format!("a {key}"), "a [REDACTED:api-key]"),
            (key.to_string(), "[REDACTED:api-key]"),
            (
                format!("{key} and {key}"),
                "[REDACTED:api-key] and [REDACTED:api-key]",
            ),
        ] {
            assert_eq!(redact(&input).0, expected, "input {input}");
        }
    }

    #[test]
    fn unterminated_private_key_block_is_redacted_to_the_end() {
        // Deliberate fail-safe: a half-pasted key must not leave its body behind.
        let pem = "note\n-----BEGIN RSA PRIVATE KEY-----\nMIIEowIBAAKCAQEA\nmore body"; // gitleaks:allow test fixture
        let (out, hits) = redact(pem);
        assert_eq!(hits, vec![("private-key", 1)]);
        assert_eq!(out, "note\n[REDACTED:private-key]");
    }

    #[test]
    fn agent_and_list_helpers_redact_before_truncation() {
        let mut r = Redactions::new();
        let long = format!("x{}", "ghp_abcdefghijklmnopqrstuvwxyz0123"); // gitleaks:allow test fixture
        let list = r.opt_list("tags", Some(vec![long]));
        assert_eq!(list, Some(vec!["x[REDACTED:github-token]".to_string()]));
        let a = r.agent(Some(AgentIdent {
            agent_type: "t".into(),
            model: "ghp_abcdefghijklmnopqrstuvwxyz0123".into(), // gitleaks:allow test fixture
            harness: "h".into(),
        }));
        assert_eq!(
            a.map(|a| a.model),
            Some("[REDACTED:github-token]".to_string())
        );
        assert_eq!(r.count(), 2);
    }

    #[test]
    fn collects_per_field_counts() {
        let mut r = Redactions::new();
        let a = r.text(
            "content",
            "x AKIAIOSFODNN7EXAMPLE y AKIAIOSFODNN7EXAMPLF".into(), // gitleaks:allow test fixture
        );
        assert_eq!(a, "x [REDACTED:aws-key] y [REDACTED:aws-key]");
        assert_eq!(r.count(), 2);
        let j = r.json(
            "v",
            serde_json::json!({"a": ["ghp_abcdefghijklmnopqrstuvwxyz0123"]}),
        ); // gitleaks:allow test fixture
        assert_eq!(j["a"][0], "[REDACTED:github-token]");
    }
}
