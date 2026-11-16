//! Request guards: Host allow-list (DNS-rebinding defense), Origin checks
//! (CSRF defense), bearer-token auth for LAN mode, and body limits.

use axum::extract::{Request, State};
use axum::http::{header, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use std::sync::Arc;

/// The guard's inputs, split out of `AppState` so the exact guard semantics
/// can be exercised by lightweight tests. Semantics are identical to the
/// previous `AppState`-based version.
#[derive(Clone)]
pub struct GuardSettings {
    pub lan_enabled: bool,
    pub lan_token: Arc<str>,
}

/// Fixed, non-content labels a denial is tagged with. The correlation
/// middleware logs only this category — never the Host/Origin/token values
/// themselves.
#[derive(Debug, Clone, Copy)]
pub enum DenialCategory {
    Host,
    Origin,
    Auth,
}

impl DenialCategory {
    pub fn as_str(&self) -> &'static str {
        match self {
            DenialCategory::Host => "host",
            DenialCategory::Origin => "origin",
            DenialCategory::Auth => "auth",
        }
    }
}

/// Host header must be a loopback name unless LAN mode is explicitly enabled.
/// This blocks DNS-rebinding attacks where a public domain resolves to 127.0.0.1.
pub async fn guard(State(gs): State<GuardSettings>, req: Request, next: Next) -> Response {
    let lan = gs.lan_enabled;

    if !lan {
        let host = req
            .headers()
            .get(header::HOST)
            .and_then(|h| h.to_str().ok())
            .unwrap_or_default();
        let host_only = host.split(':').next().unwrap_or_default();
        if !matches!(host_only, "127.0.0.1" | "localhost" | "::1" | "[::1]") {
            return denied(
                DenialCategory::Host,
                format!(
                    "refusing request for host '{host}': this server only accepts localhost access \
                     (see docs/SECURITY.md)"
                ),
            );
        }
    }

    // CSRF: browsers always send Origin on cross-origin writes. If present,
    // it must match the loopback origin (or any origin in LAN mode is rejected
    // unless it matches the LAN listen address).
    if !matches!(*req.method(), Method::GET | Method::HEAD | Method::OPTIONS) {
        if let Some(origin) = req
            .headers()
            .get(header::ORIGIN)
            .and_then(|o| o.to_str().ok())
        {
            let ok = if lan {
                true // token auth below is the real gate in LAN mode
            } else {
                let o = origin.trim_start_matches("http://");
                matches!(o.split(':').next(), Some("127.0.0.1") | Some("localhost"))
            };
            if !ok {
                return denied(
                    DenialCategory::Origin,
                    format!("cross-origin request from '{origin}' is not allowed"),
                );
            }
        }
    }

    // LAN mode: every request must carry the token (query param works for
    // EventSource, which cannot set headers).
    if lan {
        let token = &gs.lan_token;
        let provided = req
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|a| a.to_str().ok())
            .and_then(|a| a.strip_prefix("Bearer "))
            .map(|s| s.to_string())
            .or_else(|| {
                req.uri().query().and_then(|q| {
                    q.split('&').find_map(|kv| {
                        let (k, v) = kv.split_once('=')?;
                        (k == "token").then(|| urldecode(v))
                    })
                })
            });
        match provided {
            Some(t) if ct_eq(t.as_bytes(), token.as_bytes()) => {}
            _ => {
                return denied(
                    DenialCategory::Auth,
                    "missing or invalid LAN access token".to_string(),
                )
            }
        }
    }

    next.run(req).await
}

/// Constant-time equality for the LAN token: `==` on strings short-circuits
/// on the first differing byte, which leaks token prefix information through
/// response timing. Length mismatch is inherently revealed (unavoidable
/// without a MAC); the byte loop itself always runs to completion.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn urldecode(s: &str) -> String {
    percent_encoding::percent_decode_str(s)
        .decode_utf8_lossy()
        .to_string()
}

fn denied(category: DenialCategory, msg: String) -> Response {
    let mut resp = (
        StatusCode::FORBIDDEN,
        axum::Json(serde_json::json!({ "error": msg })),
    )
        .into_response();
    // Visible to the outer correlation middleware as a fixed label only.
    resp.extensions_mut().insert(category);
    resp
}

#[cfg(test)]
mod tests {
    use super::ct_eq;

    #[test]
    fn constant_time_equality_matches_str_eq_semantics() {
        assert!(ct_eq(b"secret-token", b"secret-token"));
        assert!(!ct_eq(b"secret-token", b"secret-tokem"));
        assert!(!ct_eq(b"secret-token", b"secret"));
        assert!(!ct_eq(b"", b"x"));
        assert!(ct_eq(b"", b""));
        // Same prefix, differing length must not match.
        assert!(!ct_eq(b"abc", b"abcd"));
    }
}
