//! SSRF-safe outbound HTTP used by research tools.
//!
//! Rules:
//! - Only `http`/`https` schemes.
//! - Unless `allow_private_hosts` is set, requests to loopback, private-range,
//!   and link-local IPs are refused — including via DNS resolution — and each
//!   redirect hop is re-checked.
//! - Model-chosen URLs are **IP-pinned**: DNS is resolved exactly once per
//!   hop, every resolved address is validated, and the connection is pinned
//!   to the first validated address via `reqwest::ClientBuilder::resolve`.
//!   This closes the DNS-rebinding TOCTOU window (a hostile resolver cannot
//!   pass validation with a public address and then hand the connection a
//!   loopback/metadata address). `.resolve()` overrides DNS only — TLS SNI
//!   and certificate verification still use the hostname (documented reqwest
//!   behavior).
//! - Response bodies are size-capped.

use std::net::SocketAddr;
use std::time::Duration;

use crate::errors::ToolError;
use crate::html::{default_resolve, strip_html, validated_addrs};

#[derive(Clone)]
pub struct SafeHttp {
    /// Shared unpinned client, used only on the `allow_private_hosts` path
    /// (the user-configured SearXNG base — never model-chosen URLs).
    client: reqwest::Client,
    allow_private_hosts: bool,
}

pub struct FetchedPage {
    pub url: String,
    pub title: Option<String>,
    pub text: String,
    pub content_type: String,
}

impl SafeHttp {
    /// `allow_private_hosts` must only be enabled for the configured search
    /// provider base (typically a local SearXNG instance), never for
    /// model-chosen URLs.
    pub fn new(allow_private_hosts: bool) -> Self {
        let client = reqwest::Client::builder()
            .user_agent(concat!("vortex-ai-chat/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(20))
            .redirect(reqwest::redirect::Policy::none()) // hops handled manually
            .build()
            .expect("reqwest client builds");
        Self {
            client,
            allow_private_hosts,
        }
    }

    pub fn allows_private(&self) -> bool {
        self.allow_private_hosts
    }

    /// Validate a URL string for fetching: scheme + private-network policy.
    ///
    /// Returns the parsed URL plus the address the connection must be pinned
    /// to (`None` only on the allow-private path, where DNS resolution is
    /// left to the shared client). Every redirect hop must go through here
    /// again, so a re-resolution that comes back private aborts the hop.
    pub async fn check_url(&self, url: &str) -> Result<(url::Url, Option<SocketAddr>), ToolError> {
        let parsed = url::Url::parse(url)
            .map_err(|e| ToolError::InvalidArgs("url".into(), format!("invalid URL: {e}")))?;
        match parsed.scheme() {
            "http" | "https" => {}
            other => {
                return Err(ToolError::InvalidArgs(
                    "url".into(),
                    format!("scheme '{other}' is not allowed; only http/https can be fetched"),
                ))
            }
        }
        if self.allow_private_hosts {
            return Ok((parsed, None));
        }
        let addrs = validated_addrs(&parsed, default_resolve).await?;
        // Pin the first validated address (validated_addrs never returns an
        // empty list on success).
        let pinned = addrs
            .into_iter()
            .next()
            .expect("validated_addrs returns at least one address");
        Ok((parsed, Some(pinned)))
    }

    /// Per-request client whose DNS for `host` is pinned to `addr`.
    ///
    /// Cost note: this builds one client per hop (a TLS config clone) and
    /// forgoes cross-hop connection pooling — acceptable at tool-call
    /// frequency; a small domain→addr client cache is a future option if it
    /// ever becomes hot.
    fn pinned_client(&self, host: &str, addr: SocketAddr) -> Result<reqwest::Client, ToolError> {
        reqwest::Client::builder()
            .user_agent(concat!("vortex-ai-chat/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(20))
            .redirect(reqwest::redirect::Policy::none()) // hops handled manually
            // `.resolve()` takes (domain, SocketAddr); the addr's port is the
            // connect port. Bracket-stripped for IPv6 literals.
            .resolve(host.trim_matches(['[', ']']), addr)
            .build()
            .map_err(|e| ToolError::Execution(format!("building pinned HTTP client: {e}")))
    }

    /// Fetch a page as plain text, following at most 5 redirects, re-checking
    /// (and re-pinning) every hop against the private-network policy.
    pub async fn fetch_text(&self, url: &str, max_bytes: u64) -> Result<FetchedPage, ToolError> {
        let (mut current, mut pinned) = self.check_url(url).await?;
        for _ in 0..5 {
            let client = match pinned {
                Some(addr) => {
                    let host = current
                        .host_str()
                        .ok_or_else(|| ToolError::Execution("URL has no host".into()))?;
                    self.pinned_client(host, addr)?
                }
                None => self.client.clone(),
            };
            let resp = client
                .get(current.as_str())
                .send()
                .await
                .map_err(|e| ToolError::Execution(format!("request failed: {e}")))?;
            if resp.status().is_redirection() {
                if let Some(loc) = resp.headers().get("location").and_then(|h| h.to_str().ok()) {
                    let joined = current
                        .join(loc)
                        .map_err(|e| ToolError::Execution(format!("bad redirect: {e}")))?;
                    // Re-resolve + re-validate + re-pin on every hop.
                    (current, pinned) = self.check_url(joined.as_str()).await?;
                    continue;
                }
                return Err(ToolError::Execution("redirect without location".into()));
            }
            if !resp.status().is_success() {
                return Err(ToolError::Execution(format!("HTTP {}", resp.status())));
            }
            let content_type = resp
                .headers()
                .get("content-type")
                .and_then(|h| h.to_str().ok())
                .unwrap_or("application/octet-stream")
                .to_string();
            if !content_type.starts_with("text/")
                && !content_type.starts_with("application/json")
                && !content_type.starts_with("application/xml")
                && !content_type.contains("javascript")
            {
                return Err(ToolError::Execution(format!(
                    "refusing to fetch non-text content-type '{content_type}'"
                )));
            }
            // Stream the body with a hard cap: `resp.bytes()` would let a
            // hostile server stream gigabytes into memory within the timeout
            // before the cap is applied.
            use futures::StreamExt;
            let cap = max_bytes as usize;
            let mut buf: Vec<u8> = Vec::new();
            let mut body = resp.bytes_stream();
            while let Some(chunk) = body.next().await {
                let chunk =
                    chunk.map_err(|e| ToolError::Execution(format!("body read failed: {e}")))?;
                let remaining = cap.saturating_sub(buf.len());
                if remaining == 0 {
                    break;
                }
                if chunk.len() > remaining {
                    buf.extend_from_slice(&chunk[..remaining]);
                    break;
                }
                buf.extend_from_slice(&chunk);
            }
            let text = String::from_utf8_lossy(&buf).to_string();
            let (title, plain) = if content_type.contains("html") {
                strip_html(&text)
            } else {
                (None, text)
            };
            return Ok(FetchedPage {
                url: current.to_string(),
                title,
                text: plain,
                content_type,
            });
        }
        Err(ToolError::Execution("too many redirects".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::html::validated_addrs;
    use std::net::IpAddr;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    fn addr(ip: &str, port: u16) -> SocketAddr {
        SocketAddr::new(ip.parse::<IpAddr>().unwrap(), port)
    }

    #[tokio::test]
    async fn ip_literal_urls_are_pinned_without_dns() {
        let http = SafeHttp::new(false);
        let (parsed, pinned) = http.check_url("http://93.184.216.34/page").await.unwrap();
        assert_eq!(parsed.host_str(), Some("93.184.216.34"));
        assert_eq!(pinned, Some(addr("93.184.216.34", 80)));
        let (_, pinned) = http.check_url("https://93.184.216.34/page").await.unwrap();
        assert_eq!(pinned, Some(addr("93.184.216.34", 443)));
        // Private literals are still refused.
        let err = http.check_url("http://127.0.0.1/admin").await.unwrap_err();
        assert!(err.to_string().contains("private/local"), "{err}");
        let err = http
            .check_url("http://[::ffff:169.254.169.254]/")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("private/local"), "{err}");
    }

    #[tokio::test]
    async fn rebinding_resolver_is_caught_on_revalidation() {
        // Simulated DNS rebinding: the first validation (hop 1) returns a
        // public address; by the second validation (the redirect hop) the
        // hostile resolver has flipped the name to loopback. The pin for
        // hop 1 is the validated public address, and hop 2 must abort.
        let calls = Arc::new(AtomicUsize::new(0));
        let c1 = calls.clone();
        let first = move |_host: String, port: u16| {
            let c = c1.clone();
            async move {
                c.fetch_add(1, Ordering::SeqCst);
                Ok(vec![addr("93.184.216.34", port)])
            }
        };
        let parsed = url::Url::parse("http://rebind.example/x").unwrap();
        let addrs = validated_addrs(&parsed, first).await.unwrap();
        assert_eq!(
            addrs,
            vec![addr("93.184.216.34", 80)],
            "pick = first validated addr"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        let c2 = calls.clone();
        let second = move |_host: String, port: u16| {
            let c = c2.clone();
            async move {
                c.fetch_add(1, Ordering::SeqCst);
                // The name rebound to loopback between validations.
                Ok(vec![addr("127.0.0.1", port)])
            }
        };
        let err = validated_addrs(&parsed, second).await.unwrap_err();
        assert!(
            err.to_string().contains("private/local"),
            "rebinding must abort the hop: {err}"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn any_private_address_in_the_set_rejects_the_url() {
        let mixed = move |_host: String, port: u16| async move {
            Ok(vec![addr("93.184.216.34", port), addr("10.1.2.3", port)])
        };
        let parsed = url::Url::parse("http://multi.example/x").unwrap();
        let err = validated_addrs(&parsed, mixed).await.unwrap_err();
        assert!(err.to_string().contains("10.1.2.3"), "{err}");
    }

    #[tokio::test]
    async fn resolver_failures_map_to_tool_errors() {
        let failing = move |host: String, _port: u16| async move {
            Err::<Vec<SocketAddr>, std::io::Error>(std::io::Error::other(format!(
                "no such host: {host}"
            )))
        };
        let parsed = url::Url::parse("http://missing.example/x").unwrap();
        let err = validated_addrs(&parsed, failing).await.unwrap_err();
        assert!(err.to_string().contains("DNS lookup failed"), "{err}");

        let empty = move |_host: String, _port: u16| async { Ok(Vec::new()) };
        let err = validated_addrs(&parsed, empty).await.unwrap_err();
        assert!(err.to_string().contains("no addresses"), "{err}");
    }

    /// One-shot HTTP/1.1 responder for the pinning end-to-end test.
    async fn serve_once(listener: tokio::net::TcpListener) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 2048];
        let n = socket.read(&mut buf).await.unwrap();
        let req = String::from_utf8_lossy(&buf[..n]).to_string();
        let body = "hi";
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        socket.write_all(resp.as_bytes()).await.unwrap();
        req
    }

    #[tokio::test]
    async fn pinned_client_connects_to_the_pinned_address_without_dns() {
        // ".invalid" is reserved (RFC 2606) and can never resolve: if the
        // pin did not override DNS, this request could not succeed offline.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(serve_once(listener));

        let http = SafeHttp::new(false);
        let client = http
            .pinned_client("vortex-pin-test.invalid", addr("127.0.0.1", port))
            .unwrap();
        let resp = client
            .get(format!("http://vortex-pin-test.invalid:{port}/page"))
            .send()
            .await
            .expect("pinned request reaches the local server without DNS");
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.text().await.unwrap(), "hi");

        let req = server.await.unwrap();
        assert!(
            req.to_lowercase().contains("host: vortex-pin-test.invalid"),
            "Host header must keep the hostname (SNI/cert semantics): {req}"
        );
    }

    #[tokio::test]
    async fn allow_private_path_fetches_localhost_via_hostname() {
        // Offline end-to-end fetch through the allow-private (SearXNG-style)
        // path: shared unpinned client, hostname resolving to 127.0.0.1.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(serve_once(listener));

        let http = SafeHttp::new(true);
        let page = http
            .fetch_text(&format!("http://localhost:{port}/search"), 1024)
            .await
            .expect("allow-private fetch succeeds against the local server");
        assert_eq!(page.text, "hi");
        assert!(page.content_type.starts_with("text/plain"));
        let _ = server.await;

        // ...while the model-URL policy still refuses the same localhost.
        let strict = SafeHttp::new(false);
        let err = match strict
            .fetch_text(&format!("http://localhost:{port}/search"), 1024)
            .await
        {
            Err(e) => e,
            Ok(_) => panic!("localhost must be refused on the model-URL path"),
        };
        assert!(err.to_string().contains("private/local"), "{err}");
    }
}
