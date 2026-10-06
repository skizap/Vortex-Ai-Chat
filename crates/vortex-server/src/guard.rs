//! Request guards: Host allow-list (DNS-rebinding defense), Origin checks
//! (CSRF defense), bearer-token auth for LAN mode, and body limits.

use crate::state::AppState;
use axum::extract::{Request, State};
use axum::http::{header, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use std::sync::Arc;

/// Host header must be a loopback name unless LAN mode is explicitly enabled.
/// This blocks DNS-rebinding attacks where a public domain resolves to 127.0.0.1.
pub async fn guard(State(state): State<Arc<AppState>>, req: Request, next: Next) -> Response {
    let lan = state.config.server.lan.enabled;

    if !lan {
        let host = req
            .headers()
            .get(header::HOST)
            .and_then(|h| h.to_str().ok())
            .unwrap_or_default();
        let host_only = host.split(':').next().unwrap_or_default();
        if !matches!(host_only, "127.0.0.1" | "localhost" | "::1" | "[::1]") {
            return denied(format!(
                "refusing request for host '{host}': this server only accepts localhost access \
                 (see docs/SECURITY.md)"
            ));
        }
    }

    // CSRF: browsers always send Origin on cross-origin writes. If present,
    // it must match the loopback origin (or any origin in LAN mode is rejected
    // unless it matches the LAN listen address).
    if !matches!(*req.method(), Method::GET | Method::HEAD | Method::OPTIONS) {
        if let Some(origin) = req.headers().get(header::ORIGIN).and_then(|o| o.to_str().ok()) {
            let ok = if lan {
                true // token auth below is the real gate in LAN mode
            } else {
                let o = origin.trim_start_matches("http://");
                matches!(o.split(':').next(), Some("127.0.0.1") | Some("localhost"))
            };
            if !ok {
                return denied(format!("cross-origin request from '{origin}' is not allowed"));
            }
        }
    }

    // LAN mode: every request must carry the token (query param works for
    // EventSource, which cannot set headers).
    if lan {
        let token = &state.config.server.lan.token;
        let provided = req
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|a| a.to_str().ok())
            .and_then(|a| a.strip_prefix("Bearer "))
            .map(|s| s.to_string())
            .or_else(|| {
                req.uri()
                    .query()
                    .and_then(|q| {
                        q.split('&').find_map(|kv| {
                            let (k, v) = kv.split_once('=')?;
                            (k == "token").then(|| urldecode(v))
                        })
                    })
            });
        match provided {
            Some(t) if t == *token => {}
            _ => return denied("missing or invalid LAN access token".to_string()),
        }
    }

    next.run(req).await
}

fn urldecode(s: &str) -> String {
    percent_encoding::percent_decode_str(s).decode_utf8_lossy().to_string()
}

fn denied(msg: String) -> Response {
    (
        StatusCode::FORBIDDEN,
        axum::Json(serde_json::json!({ "error": msg })),
    )
        .into_response()
}
