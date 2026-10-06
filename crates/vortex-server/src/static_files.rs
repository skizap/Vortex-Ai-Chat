//! Embedded UI assets. `ui-dist/` is produced by `scripts/build-ui.sh`
//! (Trunk → WASM). A build script guarantees a placeholder exists so the
//! server always compiles and serves something helpful.

use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "ui-dist/"]
struct UiAssets;

const PLACEHOLDER: &str = r#"<!doctype html>
<html><head><meta charset="utf-8"><title>Vortex-Ai-Chat</title>
<style>body{font-family:system-ui;background:#14161c;color:#e6e6e6;max-width:40rem;margin:3rem auto;padding:0 1rem;line-height:1.5}h1{color:#7aa2f7}code{background:#1f2335;padding:.1rem .3rem;border-radius:.2rem}</style>
</head><body>
<h1>Vortex-Ai-Chat</h1>
<p>The UI assets have not been built into this binary yet.</p>
<p>Build them with:</p>
<pre><code>scripts/build-ui.sh        # or scripts/build-release.sh for a full release</code></pre>
<p>The API is already running — try <code>curl http://127.0.0.1:8417/api/health</code>.</p>
</body></html>"#;

pub async fn index() -> Response {
    serve_path("index.html")
}

pub async fn asset(axum::extract::Path(path): axum::extract::Path<String>) -> Response {
    serve_path(&path)
}

fn serve_path(path: &str) -> Response {
    // Block any traversal-looking path.
    if path.contains("..") || path.starts_with('/') {
        return (StatusCode::BAD_REQUEST, "bad path").into_response();
    }
    match UiAssets::get(path) {
        Some(file) => {
            let ctype = mime_for(path);
            (
                StatusCode::OK,
                [
                    (header::CONTENT_TYPE, ctype),
                    (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
                    (header::CACHE_CONTROL, "no-cache"),
                ],
                file.data,
            )
                .into_response()
        }
        None => {
            // SPA: unknown non-asset paths fall back to index.html.
            if !path.contains('.') {
                serve_path("index.html")
            } else if path == "index.html" {
                // Trunk output missing entirely — placeholder.
                (
                    StatusCode::OK,
                    [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
                    PLACEHOLDER,
                )
                    .into_response()
            } else {
                (StatusCode::NOT_FOUND, "not found").into_response()
            }
        }
    }
}

fn mime_for(path: &str) -> &'static str {
    let ext = path.rsplit('.').next().unwrap_or("");
    match ext {
        "html" | "htm" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript",
        "css" => "text/css",
        "json" => "application/json",
        "wasm" => "application/wasm",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        "map" => "application/json",
        _ => "application/octet-stream",
    }
}
