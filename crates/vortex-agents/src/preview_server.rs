//! Minimal loopback-only static file server for previews (pure Rust).
//! GET/HEAD only; every path is confined to the project directory.

use std::path::{Path, PathBuf};

pub async fn serve(
    listener: tokio::net::TcpListener,
    root: PathBuf,
    abort: tokio_util::sync::CancellationToken,
) {
    let root = std::sync::Arc::new(root);
    loop {
        let accept = tokio::select! {
            a = listener.accept() => a,
            _ = abort.cancelled() => return,
        };
        let Ok((mut socket, _)) = accept else {
            continue;
        };
        let root = root.clone();
        tokio::spawn(async move {
            serve_one(&mut socket, &root).await;
        });
    }
}

async fn serve_one(socket: &mut tokio::net::TcpStream, root: &Path) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut buf = Vec::with_capacity(4096);
    let mut tmp = [0u8; 4096];
    loop {
        match socket.read(&mut tmp).await {
            Ok(0) | Err(_) => return,
            Ok(n) => {
                buf.extend_from_slice(&tmp[..n]);
                if buf.windows(4).any(|w| w == b"\r\n\r\n") || buf.len() > 16_384 {
                    break;
                }
            }
        }
    }
    let head = String::from_utf8_lossy(&buf);
    let Some(request_line) = head.lines().next() else {
        return;
    };
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("/");
    if method != "GET" && method != "HEAD" {
        let _ = socket
            .write_all(b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .await;
        return;
    }
    let path_only = target.split('?').next().unwrap_or("/");
    let rel = percent_decode(path_only);
    if rel.split('/').any(|c| c == "..") {
        let _ = socket
            .write_all(
                b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            )
            .await;
        return;
    }
    let mut file_path = root.to_path_buf();
    for seg in rel.split('/').filter(|s| !s.is_empty() && *s != ".") {
        file_path.push(seg);
    }
    if file_path.is_dir() {
        file_path.push("index.html");
    }
    // Canonicalize to guard against symlink escapes out of the project.
    if let Ok(canonical) = file_path.canonicalize() {
        if !canonical.starts_with(root) {
            let _ = socket
                .write_all(
                    b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await;
            return;
        }
    }
    match tokio::fs::read(&file_path).await {
        Ok(bytes) => {
            let ctype = content_type(&file_path);
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                bytes.len()
            );
            let _ = socket.write_all(header.as_bytes()).await;
            if method == "GET" {
                let _ = socket.write_all(&bytes).await;
            }
        }
        Err(_) => {
            let _ = socket
                .write_all(
                    b"HTTP/1.1 404 Not Found\r\nContent-Type: text/plain\r\nContent-Length: 9\r\nConnection: close\r\n\r\nnot found",
                )
                .await;
        }
    }
}

fn percent_decode(s: &str) -> String {
    percent_encoding::percent_decode_str(s)
        .decode_utf8_lossy()
        .to_string()
}

fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).unwrap_or("") {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css",
        "js" | "mjs" => "text/javascript",
        "json" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "ico" => "image/x-icon",
        "wasm" => "application/wasm",
        "txt" | "md" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {

    #[tokio::test]
    async fn static_preview_serves_confined_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("index.html"), "<h1>hello</h1>").unwrap();
        let reg = crate::preview::PreviewRegistry::new();
        let url = reg.start_static("run-1", dir.path().to_path_buf()).unwrap();
        let resp = raw_get(&url).await;
        assert!(resp.contains("200 OK"), "{resp}");
        assert!(resp.contains("<h1>hello</h1>"));
        // 404 for missing file
        let resp = raw_get(&format!("{url}/missing.css")).await;
        assert!(resp.contains("404 Not Found"));
        // traversal rejected
        let resp = raw_get(&format!("{url}/../../etc/passwd")).await;
        assert!(
            resp.contains("400 Bad Request")
                || resp.contains("403 Forbidden")
                || resp.contains("404"),
            "{resp}"
        );
        reg.stop_all();
    }

    async fn raw_get(url: &str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let parsed = url::Url::parse(url).unwrap();
        let addr = format!("{}:{}", parsed.host_str().unwrap(), parsed.port().unwrap());
        let mut stream = tokio::net::TcpStream::connect(&addr).await.unwrap();
        let path = parsed.path().to_string();
        let req = format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
        stream.write_all(req.as_bytes()).await.unwrap();
        let mut buf = String::new();
        stream.read_to_string(&mut buf).await.unwrap();
        buf
    }
}
