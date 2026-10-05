//! Pluggable search providers. OpenRouter does not search the web — this is
//! Vortex's own integration. Implemented providers:
//! - `SearxngProvider`: a SearXNG instance (self-hosted or trusted) via its
//!   JSON API.
//! - `MockSearchProvider`: offline results for tests and demo mode.

use async_trait::async_trait;
use std::time::Duration;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

#[async_trait]
pub trait SearchProvider: Send + Sync {
    async fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchResult>, String>;
    fn describe(&self) -> &'static str;
}

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
}

#[async_trait]
impl SearchProvider for SearxngProvider {
    async fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchResult>, String> {
        let url = format!("{}/search", self.base_url);
        let resp = self
            .http
            .get(&url)
            .query(&[("q", query), ("format", "json")])
            .send()
            .await
            .map_err(|e| format!("SearXNG request failed: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("SearXNG returned HTTP {}", resp.status()));
        }
        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("invalid SearXNG response: {e}"))?;
        // SearXNG may refuse JSON output; surface a setup hint instead of failing silently.
        if body.get("results").is_none() {
            return Err(
                "SearXNG response had no results. If JSON format is disabled on the instance, \
                 enable `search.formats: [html, json]` in its settings.yml."
                    .into(),
            );
        }
        let mut out = Vec::new();
        for item in body
            .get("results")
            .and_then(|r| r.as_array())
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .take(limit)
        {
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
    async fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchResult>, String> {
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
}
