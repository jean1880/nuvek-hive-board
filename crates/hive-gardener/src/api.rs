//! Thin hive-board HTTP client. Bearer token on every request; `--dry-run` turns every
//! non-GET into a logged no-op (the sweep endpoint has its own `dry_run` flag and opts out via
//! [`Hive::send_live`]). The token is never logged and never put in an error.

use crate::logic::truncate_chars;
use nuvek_core::log::Logger;
use reqwest::Method;
use serde_json::Value;
use std::time::Duration;

const CALL_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug)]
pub enum ApiError {
    /// The server answered with a non-2xx status.
    Status { code: u16, body: String },
    /// No usable answer (connect, timeout, undecodable body).
    Transport(String),
}

impl ApiError {
    pub fn code(&self) -> Option<u16> {
        match self {
            ApiError::Status { code, .. } => Some(*code),
            ApiError::Transport(_) => None,
        }
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiError::Status { code, body } => write!(f, "hive-board answered {code}: {body}"),
            ApiError::Transport(e) => write!(f, "hive-board unreachable: {e}"),
        }
    }
}

impl std::error::Error for ApiError {}

/// A decoded answer plus the HTTP status (200 vs 201 distinguishes an idempotent replay).
pub struct Reply {
    pub status: u16,
    pub body: Value,
}

/// Whether a write produced something new: HTTP 201, or a dry run (`None`, so the plan prints in
/// full). A 200 is an idempotent replay and must not repeat its side effects.
pub fn is_fresh(reply: &Option<Reply>) -> bool {
    reply.as_ref().map(|r| r.status == 201).unwrap_or(true)
}

pub struct Hive {
    http: reqwest::Client,
    base: String,
    token: Option<String>,
    dry: bool,
}

impl Hive {
    pub fn new(base: String, token: Option<String>, dry: bool) -> anyhow::Result<Self> {
        Ok(Self {
            http: nuvek_core::http::client(CALL_TIMEOUT)?,
            base: base.trim_end_matches('/').to_string(),
            token,
            dry,
        })
    }

    pub fn is_dry(&self) -> bool {
        self.dry
    }

    async fn call(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
        idem: Option<&str>,
    ) -> Result<Reply, ApiError> {
        let mut req = self
            .http
            .request(method, format!("{}{path}", self.base))
            .timeout(CALL_TIMEOUT);
        if let Some(t) = &self.token {
            req = req.bearer_auth(t);
        }
        if let Some(k) = idem {
            req = req.header("Idempotency-Key", k);
        }
        if let Some(b) = body {
            req = req.json(b);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| ApiError::Transport(e.without_url().to_string()))?;
        let status = resp.status().as_u16();
        let text = resp
            .text()
            .await
            .map_err(|e| ApiError::Transport(e.without_url().to_string()))?;
        if !(200..300).contains(&status) {
            // The server's error body is `{"error", "code"}`; keep it short.
            return Err(ApiError::Status {
                code: status,
                body: truncate_chars(&text, 300),
            });
        }
        let body = if text.trim().is_empty() {
            Value::Null
        } else {
            serde_json::from_str(&text)
                .map_err(|e| ApiError::Transport(format!("bad JSON: {e}")))?
        };
        Ok(Reply { status, body })
    }

    pub async fn get(&self, path: &str) -> Result<Value, ApiError> {
        Ok(self.call(Method::GET, path, None, None).await?.body)
    }

    /// A write that really happens, even under `--dry-run` (server-side dry-run endpoints only).
    pub async fn send_live(
        &self,
        method: Method,
        path: &str,
        body: &Value,
    ) -> Result<Value, ApiError> {
        Ok(self.call(method, path, Some(body), None).await?.body)
    }

    /// A write. Under `--dry-run` it logs the plan and returns `Ok(None)`.
    pub async fn write(
        &self,
        log: &Logger,
        method: Method,
        path: &str,
        body: &Value,
        idem: Option<&str>,
    ) -> Result<Option<Reply>, ApiError> {
        if self.dry {
            log.info(&format!(
                "DRY-RUN would {method} {path} {}",
                truncate_chars(&body.to_string(), 240)
            ));
            return Ok(None);
        }
        self.call(method, path, Some(body), idem).await.map(Some)
    }

    /// Read a `kv` value (`None` when the key does not exist yet).
    pub async fn kv_get(&self, key: &str) -> Result<Option<Value>, ApiError> {
        match self.get(&format!("/api/kv/{key}")).await {
            Ok(v) => Ok(v.get("v").cloned()),
            Err(e) if e.code() == Some(404) => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub async fn kv_put(&self, log: &Logger, key: &str, value: &Value) -> Result<(), ApiError> {
        self.write(
            log,
            Method::PUT,
            &format!("/api/kv/{key}"),
            &serde_json::json!({ "v": value }),
            None,
        )
        .await
        .map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    /// One-shot in-test HTTP listener on loopback: answers `status`/`body`, returns the raw
    /// request it saw. No real network.
    fn serve_once(status: &str, body: &str) -> (String, std::thread::JoinHandle<String>) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let resp = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let h = std::thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            let mut buf = [0u8; 4096];
            let n = s.read(&mut buf).unwrap();
            s.write_all(resp.as_bytes()).unwrap();
            String::from_utf8_lossy(&buf[..n]).to_string()
        });
        (format!("http://{addr}"), h)
    }

    #[tokio::test]
    async fn bearer_and_idempotency_headers_are_sent_and_201_is_reported() {
        let (url, h) = serve_once("201 Created", "{\"id\":7}");
        let hive = Hive::new(url, Some("tok-123".into()), false).unwrap();
        let log = nuvek_core::log::Logger::stderr_only("test");
        let r = hive
            .write(
                &log,
                Method::POST,
                "/api/x",
                &serde_json::json!({"a":1}),
                Some("k1"),
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(r.status, 201);
        assert_eq!(r.body["id"], 7);
        let seen = h.join().unwrap().to_lowercase();
        assert!(seen.contains("authorization: bearer tok-123"), "{seen}");
        assert!(seen.contains("idempotency-key: k1"), "{seen}");
    }

    #[tokio::test]
    async fn error_status_maps_to_status_error_and_kv_404_is_none() {
        let (url, h) = serve_once(
            "404 Not Found",
            "{\"error\":\"nope\",\"code\":\"not_found\"}",
        );
        let hive = Hive::new(url, None, false).unwrap();
        assert!(hive.kv_get("bridge.cursor").await.unwrap().is_none());
        h.join().unwrap();
        let (url, h) = serve_once("409 Conflict", "{\"error\":\"dup\",\"code\":\"conflict\"}");
        let hive = Hive::new(url, None, false).unwrap();
        let e = hive.get("/api/x").await.unwrap_err();
        assert_eq!(e.code(), Some(409));
        h.join().unwrap();
    }

    #[tokio::test]
    async fn dry_run_writes_never_touch_the_network() {
        // Port 1 is closed: a real call would be a transport error.
        let hive = Hive::new("http://127.0.0.1:1".into(), None, true).unwrap();
        let log = nuvek_core::log::Logger::stderr_only("test");
        let r = hive
            .write(&log, Method::POST, "/api/x", &serde_json::json!({}), None)
            .await
            .unwrap();
        assert!(r.is_none());
    }
}
