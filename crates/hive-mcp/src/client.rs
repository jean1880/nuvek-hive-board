//! Thin HTTP client for hive-board. Stateless: every failure is a tool error, never a local
//! write. The bearer token is attached to every request and never logged.

use anyhow::{Context, Result};
use reqwest::{Client, Method};
use serde_json::Value;
use std::time::Duration;

const TOKEN_REJECTED: &str =
    "hive-board rejected the token (HIVE_API_TOKEN missing or wrong in the MCP server's environment)";

#[derive(Clone)]
pub struct Hive {
    http: Client,
    base: String,
    token: Option<String>,
}

/// Percent-encode one URL path segment (slug or id).
pub fn seg(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Keys whose text is pasted command output or config (`content`, `evidence`, job `result`,
/// `spec`, amendment `text`, verdict `note`): these also get
/// `redact_config` (URL credentials, `KEY=value` forms). Every other string gets `redact`.
const CONFIG_TEXT_KEYS: [&str; 6] = ["content", "evidence", "result", "spec", "text", "note"];

fn redact_value(v: &mut Value, config: bool) {
    match v {
        Value::String(s) => {
            *s = if config {
                nuvek_core::scrub::redact_config(s)
            } else {
                nuvek_core::scrub::redact(s)
            };
        }
        Value::Array(a) => a.iter_mut().for_each(|x| redact_value(x, config)),
        Value::Object(o) => {
            for (k, x) in o.iter_mut() {
                // `config ||`: values nested under content/evidence keep redact_config.
                redact_value(x, config || CONFIG_TEXT_KEYS.contains(&k.as_str()));
            }
        }
        _ => {}
    }
}

/// Redact token-shaped secrets from EVERY string in an outgoing body, at any depth (objects and
/// arrays), so a field added later cannot be skipped. The server redacts again as a backstop;
/// this keeps secrets off the wire. The pre-redaction text is never logged.
pub fn redact_body(body: &mut Value) {
    redact_value(body, false);
}

/// Redact every query-string value (search text, filters): URLs end up in server logs.
pub fn redact_query<'a>(q: &[(&'a str, String)]) -> Vec<(&'a str, String)> {
    q.iter()
        .map(|(k, v)| (*k, nuvek_core::scrub::redact(v)))
        .collect()
}

/// A fresh random 128-bit hex idempotency key per tool call.
pub fn idem_key() -> String {
    let mut b = [0u8; 16];
    if getrandom::fill(&mut b).is_err() {
        // OS CSPRNG unavailable (practically never): degrade to clock + pid, still unique per call.
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        b.copy_from_slice(&(n ^ u128::from(std::process::id())).to_le_bytes());
    }
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Innermost cause of a transport error, without the URL reqwest embeds.
fn reason(e: &reqwest::Error) -> String {
    let mut msg = e.to_string();
    let mut src: Option<&dyn std::error::Error> = std::error::Error::source(e);
    while let Some(s) = src {
        msg = s.to_string();
        src = s.source();
    }
    if e.is_timeout() {
        msg = format!("timed out ({msg})");
    }
    msg
}

impl Hive {
    pub fn new(base: &str, token: Option<String>) -> Result<Self> {
        let http =
            nuvek_core::http::client(Duration::from_secs(15)).context("creating HTTP client")?;
        Ok(Self {
            http,
            base: base.trim_end_matches('/').to_string(),
            token,
        })
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    /// One request. `Err` is a complete, human-readable tool error.
    pub async fn call(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<Value>,
        idem: Option<String>,
    ) -> Result<Value, String> {
        let write = method != Method::GET;
        let url = format!("{}{}", self.base, path);
        let mut req = self.http.request(method, &url);
        if !query.is_empty() {
            req = req.query(&redact_query(query));
        }
        // Every request carries the token: hive-board gates reads by default (`HIVE_AUTH_READS`).
        if let Some(t) = &self.token {
            req = req.bearer_auth(t);
        }
        if let Some(k) = idem {
            req = req.header("Idempotency-Key", k);
        }
        if let Some(mut b) = body {
            redact_body(&mut b);
            req = req.json(&b);
        }
        let res = match req.send().await {
            Ok(r) => r,
            Err(e) => {
                return Err(if write && e.is_connect() {
                    format!(
                        "hive-board unreachable at {}: {}; nothing was written — retry later",
                        self.base,
                        reason(&e)
                    )
                } else if write {
                    format!(
                        "hive-board request failed at {}: {}; the write may or may not have \
                         landed — check with board_get_messages (or board_list_topics) before \
                         retrying",
                        self.base,
                        reason(&e)
                    )
                } else {
                    format!("hive-board unreachable at {}: {}", self.base, reason(&e))
                })
            }
        };
        let status = res.status();
        let text = res.text().await.unwrap_or_default();
        if status.as_u16() == 401 {
            return Err(TOKEN_REJECTED.to_string());
        }
        if !status.is_success() {
            let parsed: Option<Value> = serde_json::from_str(&text).ok();
            let (code, msg) = parsed
                .as_ref()
                .map(|v| {
                    (
                        v["code"].as_str().unwrap_or("error").to_string(),
                        v["error"].as_str().unwrap_or(&text).to_string(),
                    )
                })
                .unwrap_or_else(|| ("error".into(), text.clone()));
            return Err(format!(
                "hive-board {} ({code}): {}",
                status.as_u16(),
                msg.chars().take(500).collect::<String>()
            ));
        }
        if text.trim().is_empty() {
            return Ok(serde_json::json!({"ok": true, "status": status.as_u16()}));
        }
        serde_json::from_str(&text).map_err(|e| format!("hive-board returned invalid JSON: {e}"))
    }

    pub async fn get(&self, path: &str, query: &[(&str, String)]) -> Result<Value, String> {
        self.call(Method::GET, path, query, None, None).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_body_scrubs_every_string_at_any_depth() {
        let fake = "ghp_abcdefghijklmnopqrstuvwxyz0123"; // gitleaks:allow — invented fixture
        let mut b = serde_json::json!({
            "content": format!("token {fake} here"),
            "evidence": format!("env GH={fake}"),
            "note": fake,
            "spec": format!("use {fake}"),
            "tags": [fake, "x"],
            "findings": [{"claim": fake, "evidence": format!("v {fake}"),
                          "deep": {"result": [format!("r {fake}")]}}],
            "author": "a",
            "n": 5,
        });
        redact_body(&mut b);
        let s = b.to_string();
        assert!(!s.contains("ghp_abcdefghij"), "{s}");
        assert!(s.contains("REDACTED"));
        assert!(b["content"].as_str().unwrap().starts_with("token "));
        assert_eq!(b["author"], "a");
        assert_eq!(b["n"], 5);
        assert_eq!(b["tags"][1], "x");
    }

    #[test]
    fn nested_values_under_content_keep_config_redaction() {
        let url = "postgresql://u:9f8e7d6c5b4a@db:5432/x"; // gitleaks:allow — invented fixture
        let mut b = serde_json::json!({
            "content": {"inner": {"deep": [url]}},
            "evidence": [{"x": url}],
            "other": {"x": url},
        });
        redact_body(&mut b);
        assert!(!b["content"].to_string().contains("9f8e7d6c5b4a"));
        assert!(!b["evidence"].to_string().contains("9f8e7d6c5b4a"));
        // Outside content/evidence only the token-shape pass applies.
        assert!(b["other"].to_string().contains("9f8e7d6c5b4a"));
    }

    #[test]
    fn query_values_are_redacted() {
        let fake = "ghp_abcdefghijklmnopqrstuvwxyz0123"; // gitleaks:allow — invented fixture
        let q = redact_query(&[("q", format!("find {fake}")), ("limit", "5".to_string())]);
        assert!(!q[0].1.contains("ghp_abcdefghij"));
        assert!(q[0].1.starts_with("find "));
        assert_eq!(q[1], ("limit", "5".to_string()));
    }

    #[tokio::test]
    async fn query_secret_never_reaches_the_wire() {
        let (url, h) = mock("HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\n[]");
        let hive = Hive::new(&url, None).unwrap();
        let fake = "sk-abcdefghijklmnopqrstuvwx"; // gitleaks:allow — invented fixture
        hive.get("/api/search", &[("q", format!("x {fake}"))])
            .await
            .unwrap();
        assert!(!h.join().unwrap().contains("sk-abcdefghij"));
    }

    #[test]
    fn content_and_evidence_get_config_redaction() {
        let url = "postgresql://u:9f8e7d6c5b4a@db:5432/x"; // gitleaks:allow — invented fixture
        let mut b = serde_json::json!({
            "content": url, "evidence": url, "result": url, "spec": url,
            "text": url, "note": url, "reason": url,
            "jobs": [{"spec": url}],
        });
        redact_body(&mut b);
        for k in ["content", "evidence", "result", "spec", "text", "note"] {
            assert!(!b[k].as_str().unwrap().contains("9f8e7d6c5b4a"), "{k}");
        }
        assert!(!b["jobs"].to_string().contains("9f8e7d6c5b4a"));
        // `reason` only gets the token-shape pass, which does not know URL credentials.
        assert!(b["reason"].as_str().unwrap().contains("9f8e7d6c5b4a"));
    }

    #[tokio::test]
    async fn secret_never_reaches_the_wire() {
        let (url, h) = mock("HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\n{}");
        let hive = Hive::new(&url, None).unwrap();
        let fake = "sk-abcdefghijklmnopqrstuvwx"; // gitleaks:allow — invented fixture
        hive.call(
            Method::POST,
            "/api/topics/t/messages",
            &[],
            Some(serde_json::json!({"content": format!("leaked {fake}")})),
            None,
        )
        .await
        .unwrap();
        let req = h.join().unwrap();
        assert!(!req.contains("sk-abcdefghij"), "{req}");
        assert!(req.contains("leaked"));
    }

    #[tokio::test]
    async fn delete_carries_confirm_query() {
        let (url, h) = mock("HTTP/1.1 204 No Content\r\n\r\n");
        let hive = Hive::new(&url, Some("t".into())).unwrap();
        hive.call(
            Method::DELETE,
            "/api/topics/my-topic",
            &[("confirm", "my-topic".to_string())],
            None,
            None,
        )
        .await
        .unwrap();
        let req = h.join().unwrap();
        assert!(
            req.starts_with("delete /api/topics/my-topic?confirm=my-topic "),
            "{req}"
        );
    }

    #[test]
    fn seg_encodes_reserved() {
        assert_eq!(seg("a b/c"), "a%20b%2Fc");
        assert_eq!(seg("hive-v2_build"), "hive-v2_build");
    }

    /// One-shot HTTP server: returns the canned response, hands back the raw request text.
    fn mock(response: &'static str) -> (String, std::thread::JoinHandle<String>) {
        use std::io::{Read, Write};
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        let h = std::thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            let mut buf = [0u8; 8192];
            let n = s.read(&mut buf).unwrap();
            s.write_all(response.as_bytes()).unwrap();
            String::from_utf8_lossy(&buf[..n]).to_lowercase()
        });
        (url, h)
    }

    #[tokio::test]
    async fn unauthorized_maps_to_token_message_and_sends_bearer_without_leaking_it() {
        let (url, h) = mock("HTTP/1.1 401 Unauthorized\r\ncontent-length: 2\r\n\r\n{}");
        let hive = Hive::new(&url, Some("s3cret-token".into())).unwrap();
        let e = hive
            .call(
                Method::POST,
                "/api/topics",
                &[],
                Some(serde_json::json!({})),
                Some(idem_key()),
            )
            .await
            .unwrap_err();
        assert_eq!(e, TOKEN_REJECTED);
        assert!(!e.contains("s3cret"));
        let req = h.join().unwrap();
        assert!(req.contains("authorization: bearer s3cret-token"));
        assert!(req.contains("idempotency-key:"));
    }

    #[tokio::test]
    async fn gets_carry_the_token_for_the_read_gate() {
        let (url, h) = mock("HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\n[]");
        let hive = Hive::new(&url, Some("tok".into())).unwrap();
        assert_eq!(
            hive.get("/api/topics", &[]).await.unwrap(),
            serde_json::json!([])
        );
        assert!(h.join().unwrap().contains("authorization: bearer tok"));
    }

    #[tokio::test]
    async fn no_token_sends_no_authorization_header() {
        let (url, h) = mock("HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\n[]");
        let hive = Hive::new(&url, None).unwrap();
        hive.get("/api/topics", &[]).await.unwrap();
        assert!(!h.join().unwrap().contains("authorization"));
    }

    #[tokio::test]
    async fn dropped_connection_after_connect_is_not_claimed_unwritten() {
        let (url, _h) = mock("");
        let hive = Hive::new(&url, None).unwrap();
        let e = hive
            .call(Method::POST, "/api/topics", &[], None, None)
            .await
            .unwrap_err();
        assert!(e.contains("may or may not have landed"), "{e}");
        assert!(!e.contains("nothing was written"), "{e}");
    }

    #[tokio::test]
    async fn dead_url_is_a_clear_write_error() {
        let hive = Hive::new("http://127.0.0.1:9", None).unwrap();
        let e = hive
            .call(Method::POST, "/api/topics", &[], None, None)
            .await
            .unwrap_err();
        assert!(
            e.starts_with("hive-board unreachable at http://127.0.0.1:9: "),
            "{e}"
        );
        assert!(e.ends_with("nothing was written — retry later"), "{e}");
    }

    #[test]
    fn idem_keys_are_unique() {
        assert_ne!(idem_key(), idem_key());
        assert_eq!(idem_key().len(), 32);
    }
}
