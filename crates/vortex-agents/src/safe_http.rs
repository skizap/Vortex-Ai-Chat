//! SSRF-safe outbound HTTP used by research tools.
//!
//! Rules:
//! - Only `http`/`https` schemes.
//! - Unless `allow_private_hosts` is set, requests to loopback, private-range,
//!   and link-local IPs are refused — including via DNS resolution — and each
//!   redirect hop is re-checked.
//! - Response bodies are size-capped.

use std::time::Duration;

use crate::errors::ToolError;
use crate::html::{ensure_not_private, strip_html};

#[derive(Clone)]
pub struct SafeHttp {
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
    pub async fn check_url(&self, url: &str) -> Result<url::Url, ToolError> {
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
        if !self.allow_private_hosts {
            ensure_not_private(&parsed).await?;
        }
        Ok(parsed)
    }

    /// Fetch a page as plain text, following at most 5 redirects, re-checking
    /// every hop against the private-network policy.
    pub async fn fetch_text(&self, url: &str, max_bytes: u64) -> Result<FetchedPage, ToolError> {
        let mut current = self.check_url(url).await?.to_string();
        for _ in 0..5 {
            let resp = self
                .client
                .get(&current)
                .send()
                .await
                .map_err(|e| ToolError::Execution(format!("request failed: {e}")))?;
            if resp.status().is_redirection() {
                if let Some(loc) = resp.headers().get("location").and_then(|h| h.to_str().ok()) {
                    let joined = url::Url::parse(&current)
                        .and_then(|base| base.join(loc))
                        .map_err(|e| ToolError::Execution(format!("bad redirect: {e}")))?;
                    current = self.check_url(joined.as_str()).await?.to_string();
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
            let bytes = resp
                .bytes()
                .await
                .map_err(|e| ToolError::Execution(format!("body read failed: {e}")))?;
            let capped = &bytes[..(bytes.len().min(max_bytes as usize))];
            let text = String::from_utf8_lossy(capped).to_string();
            let (title, plain) = if content_type.contains("html") {
                strip_html(&text)
            } else {
                (None, text)
            };
            return Ok(FetchedPage {
                url: current,
                title,
                text: plain,
                content_type,
            });
        }
        Err(ToolError::Execution("too many redirects".into()))
    }
}
