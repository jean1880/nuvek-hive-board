//! Bearer-token middleware for every write route, and for reads while the read gate
//! (`HIVE_AUTH_READS`, on by default) is on. `/readyz` stays open for health checks.
//!
//! CEILING: the shared token proves "a trusted agent or operator", nothing finer.
//! `author` / `actor` fields in request bodies are cooperative and can be spoofed
//! by any token holder. Upgrade path: per-agent tokens that bind the identity
//! server-side.
//!
//! The `Authorization` header is never logged.

use axum::extract::{Request, State};
use axum::http::Method;
use axum::middleware::Next;
use axum::response::Response;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::config::AuthConfig;
use crate::error::ApiError;
use crate::state::AppState;

/// Constant-time token check. Both sides are hashed first so the comparison
/// does not leak the token length.
pub fn token_matches(auth: &AuthConfig, presented: &str) -> bool {
    let p = Sha256::digest(presented.as_bytes());
    let mut ok = subtle::Choice::from(0u8);
    for t in [&auth.token, &auth.token_next].into_iter().flatten() {
        let d = Sha256::digest(t.as_bytes());
        ok |= p.as_slice().ct_eq(d.as_slice());
    }
    bool::from(ok)
}

pub async fn require_token(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Result<Response, ApiError> {
    let safe = matches!(*req.method(), Method::GET | Method::HEAD | Method::OPTIONS);
    // The health probe has no token, so readiness stays open even with gated reads.
    let open_read = safe && !(state.auth.reads && req.uri().path() != "/readyz");
    if open_read || state.auth.off {
        return Ok(next.run(req).await);
    }
    let presented = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| {
            let (scheme, rest) = v.split_once(' ')?;
            scheme.eq_ignore_ascii_case("bearer").then(|| rest.trim())
        });
    match presented {
        Some(t) if token_matches(&state.auth, t) => Ok(next.run(req).await),
        _ => Err(ApiError::Unauthorized),
    }
}
