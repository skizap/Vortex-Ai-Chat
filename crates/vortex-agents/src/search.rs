//! Pluggable search providers. OpenRouter does not search the web — this is
//! Vortex's own integration. Implemented providers:
//! - `SearxngProvider`: a SearXNG instance (self-hosted or trusted) via its
//!   JSON API.
//! - `MockSearchProvider`: offline results for tests and demo mode.
//!
//! Provider errors are structured ([`SearchError`]) and sanitized: transport
//! error text can embed the request URL (which may carry credentials), so
//! only fixed, actionable messages are ever surfaced — to the model via the
//! `web_search` tool and to the UI via the Settings search probe.

use async_trait::async_trait;
use std::time::Duration;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

/// Structured search failure. The `Display` text is what the `web_search`
/// tool reports to the model and what the Settings probe maps into
/// `SearchTestResult.detail`; every variant is a fixed message (plus an HTTP
/// status code), never raw provider/transport output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchError {
    /// TCP connect / DNS failure: the instance is unreachable.
    Connect,
    /// The request exceeded the client timeout.
    Timeout,
    /// Some other transport-level failure before a response arrived.
    Transport,
    /// The provider answered with a non-success HTTP status.
    HttpStatus(u16),
    /// The body was not decodable as JSON (e.g. an HTML page was returned).
    InvalidJson,
    /// JSON decoded but had no usable `results` array — missing or non-array
    /// (e.g. JSON output disabled). Never treated as "connected, empty".
    MissingResults,
    /// The response exceeded [`MAX_BODY_BYTES`]; it is never fully buffered.
    ResponseTooLarge,
    /// Any other provider-level failure with a fixed description.
    Provider(&'static str),
}

impl SearchError {
    /// True when the failure is transport-level (connection/timeout).
    pub fn is_connection(&self) -> bool {
        matches!(self, Self::Connect | Self::Timeout | Self::Transport)
    }
}

impl std::fmt::Display for SearchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Connect => f.write_str(
                "could not connect to the search provider (is the service running and the \
                 configured URL correct?)",
            ),
            Self::Timeout => f.write_str(
                "the search provider did not respond within the 15s limit (service too slow \
                 or unreachable)",
            ),
            Self::Transport => f.write_str("the search request could not be completed"),
            Self::HttpStatus(403) => f.write_str(
                "the search provider returned HTTP 403 (forbidden). If this is a SearXNG \
                 instance, JSON output may be disabled: enable `search.formats: [html, json]` \
                 in its settings.yml",
            ),
            Self::HttpStatus(code) => write!(f, "the search provider returned HTTP {code}"),
            Self::InvalidJson => f.write_str(
                "the search provider returned a body that was not JSON. If this is a SearXNG \
                 instance, enable `search.formats: [html, json]` in its settings.yml",
            ),
            Self::MissingResults => f.write_str(
                "the search provider returned JSON without a usable `results` array. If JSON \
                 format is disabled on the instance, enable `search.formats: [html, json]` in \
                 its settings.yml",
            ),
            Self::ResponseTooLarge => f.write_str(
                "the search provider sent more than the 4 MiB response bound; the response was \
                 not buffered (is the configured URL pointing at a search instance?)",
            ),
            Self::Provider(msg) => f.write_str(msg),
        }
    }
}

impl std::error::Error for SearchError {}

#[async_trait]
pub trait SearchProvider: Send + Sync {
    async fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchResult>, SearchError>;
    fn describe(&self) -> &'static str;
}

/// Fixed benign probe used by the Settings "Test search" action. The probe
/// query and limit are server-side constants — callers can never supply them.
pub const PROBE_QUERY: &str = "vortex connectivity test";
pub const PROBE_LIMIT: usize = 3;

/// Hard bound on buffered provider response bytes. Real SearXNG JSON pages
/// are tens of KiB; anything larger is refused instead of buffered.
pub const MAX_BODY_BYTES: usize = 4 * 1024 * 1024;

/// SearXNG adapter. Uses the instance's `format=json` endpoint; the instance
/// must have JSON output enabled. Local instances are allowed because the
/// base URL is explicitly configured by the user (not model-chosen).
pub struct SearxngProvider {
    base_url: String,
    http: reqwest::Client,
}

impl SearxngProvider {
    pub fn new(base_url: impl Into<String>) -> Self {
        let http = reqwest::Client::builder()
            .user_agent(concat!("vortex-ai-chat/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(15))
            .build()
            .expect("reqwest client builds");
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            http,
        }
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Classify a transport failure without echoing reqwest's error text:
    /// its Display embeds the request URL, which could carry credentials
    /// from an operator-configured base URL.
    fn classify_transport(e: &reqwest::Error) -> SearchError {
        if e.is_timeout() {
            SearchError::Timeout
        } else if e.is_connect() {
            SearchError::Connect
        } else {
            SearchError::Transport
        }
    }
}

#[async_trait]
impl SearchProvider for SearxngProvider {
    async fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchResult>, SearchError> {
        let url = format!("{}/search", self.base_url);
        let mut resp = self
            .http
            .get(&url)
            .query(&[("q", query), ("format", "json")])
            .send()
            .await
            .map_err(|e| Self::classify_transport(&e))?;
        if !resp.status().is_success() {
            return Err(SearchError::HttpStatus(resp.status().as_u16()));
        }
        // Bounded read: never buffer more than MAX_BODY_BYTES. Mid-body
        // failures (including the client's total timeout expiring while
        // reading) go through the same sanitized transport classification.
        let mut bytes: Vec<u8> = Vec::new();
        while let Some(chunk) = resp
            .chunk()
            .await
            .map_err(|e| Self::classify_transport(&e))?
        {
            if bytes.len() + chunk.len() > MAX_BODY_BYTES {
                return Err(SearchError::ResponseTooLarge);
            }
            bytes.extend_from_slice(&chunk);
        }
        let body: serde_json::Value =
            serde_json::from_slice(&bytes).map_err(|_| SearchError::InvalidJson)?;
        // SearXNG may refuse JSON output; surface a setup hint instead of
        // failing silently. A missing *or* non-array `results` field is an
        // invalid response — never "connected with no results".
        let Some(results) = body.get("results").and_then(|r| r.as_array()) else {
            return Err(SearchError::MissingResults);
        };
        let mut out = Vec::new();
        for item in results.iter().take(limit) {
            let url = item
                .get("url")
                .and_then(|u| u.as_str())
                .unwrap_or_default()
                .to_string();
            if url.is_empty() {
                continue;
            }
            out.push(SearchResult {
                title: item
                    .get("title")
                    .and_then(|t| t.as_str())
                    .unwrap_or_default()
                    .to_string(),
                url,
                snippet: item
                    .get("content")
                    .and_then(|c| c.as_str())
                    .unwrap_or_default()
                    .to_string(),
            });
        }
        Ok(out)
    }

    fn describe(&self) -> &'static str {
        "searxng"
    }
}

/// Offline provider for tests and demo mode. Returns clearly-labeled fake data.
pub struct MockSearchProvider;

#[async_trait]
impl SearchProvider for MockSearchProvider {
    async fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchResult>, SearchError> {
        let q = query.to_string();
        Ok((1..=limit as u64)
            .map(|i| SearchResult {
                title: format!("Mock result {i} for '{q}'"),
                url: format!(
                    "https://example.com/results/{i}?q={}",
                    urlencoding_min(q.as_str())
                ),
                snippet: format!(
                    "Example snippet {i} about {q}. This is mock data for offline testing."
                ),
            })
            .collect())
    }

    fn describe(&self) -> &'static str {
        "mock"
    }
}

fn urlencoding_min(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' | '~' => c.to_string(),
            ' ' => "+".to_string(),
            _ => format!("%{:02X}", c as u32),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mock_provider_returns_labeled_results() {
        let p = MockSearchProvider;
        let results = p.search("rust", 3).await.unwrap();
        assert_eq!(results.len(), 3);
        assert!(results[0].url.starts_with("https://example.com/"));
        assert!(results[0].snippet.contains("mock data"));
    }

    /// Error Display text is what the model and the Settings probe see: it
    /// must stay actionable and must never embed transport error output
    /// (which can carry the request URL, including any credentials).
    #[test]
    fn error_display_is_fixed_and_actionable() {
        let json_hint = "`search.formats: [html, json]`";
        assert!(SearchError::HttpStatus(403).to_string().contains(json_hint));
        assert!(SearchError::InvalidJson.to_string().contains(json_hint));
        assert!(SearchError::MissingResults.to_string().contains(json_hint));
        assert!(SearchError::HttpStatus(500)
            .to_string()
            .contains("HTTP 500"));
        assert!(SearchError::Connect
            .to_string()
            .contains("could not connect"));
        assert!(SearchError::Timeout.to_string().contains("15s"));
        assert!(SearchError::Transport.to_string().len() < 200);
        assert!(SearchError::Connect.is_connection());
        assert!(SearchError::Timeout.is_connection());
        assert!(SearchError::Transport.is_connection());
        assert!(!SearchError::HttpStatus(403).is_connection());
    }

    /// An unreachable loopback port must classify as a connection failure
    /// with sanitized text (no reqwest Display leakage). Local test server
    /// semantics only — no external network.
    #[tokio::test]
    async fn unreachable_instance_is_a_sanitized_connect_error() {
        // Bind and drop a listener to obtain a port nothing listens on.
        let port = {
            let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            l.local_addr().unwrap().port()
        };
        let p = SearxngProvider::new(format!("http://127.0.0.1:{port}"));
        let err = p.search("anything", 3).await.unwrap_err();
        assert_eq!(err, SearchError::Connect);
        let text = err.to_string();
        assert!(!text.contains("127.0.0.1"), "no URL leakage: {text}");
        assert!(
            !text.to_lowercase().contains("os error"),
            "no raw io text: {text}"
        );
    }
}
