//! Research tools: `web_search` (via the configured provider) and
//! `fetch_url` (SSRF-safe page fetch).

use crate::errors::ToolError;
use crate::registry::{Tool, ToolContext, ToolResult};
use crate::schema::{ParamSpec, ParamType, ToolDef};
use async_trait::async_trait;

pub struct WebSearchTool;

const SEARCH_DEF: ToolDef = ToolDef {
    name: "web_search",
    description: "Search the public internet through the user's configured search provider and return titles, links and snippets.",
    params: &[ParamSpec {
        name: "query",
        ty: ParamType::Str,
        required: true,
        description: "Search query.",
    }],
};

#[async_trait]
impl Tool for WebSearchTool {
    fn def(&self) -> &'static ToolDef {
        &SEARCH_DEF
    }
    fn available(&self, ctx: &ToolContext) -> Result<(), ToolError> {
        if !ctx.settings.tools.search {
            return Err(ToolError::Unavailable(
                "web search is disabled in Settings (Tools).".into(),
            ));
        }
        if ctx.search.is_none() {
            return Err(ToolError::Unavailable(
                "no search provider is configured. Set a SearXNG instance URL in Settings → \
                 Search (see README.md 'Search setup')."
                    .into(),
            ));
        }
        Ok(())
    }
    async fn execute(
        &self,
        ctx: &ToolContext,
        args: &serde_json::Value,
    ) -> Result<ToolResult, ToolError> {
        let query = args["query"].as_str().expect("validated");
        let provider = ctx.search.as_ref().expect("availability checked");
        let limit = ctx.settings.search.max_results.max(1) as usize;
        let results = provider
            .search(query, limit)
            .await
            .map_err(|e| ToolError::Execution(format!("search failed: {e}")))?;
        if results.is_empty() {
            return Ok(ToolResult::text("no results found"));
        }
        let text = results
            .iter()
            .map(|r| {
                format!(
                    "- {} — {}\n  {}\n  URL: {}",
                    r.title, r.snippet, r.url, r.url
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        Ok(ToolResult {
            summary: format!("searched for '{query}' ({} results)", results.len()),
            text,
            diff: None,
        })
    }
}

pub struct FetchUrlTool;

const FETCH_DEF: ToolDef = ToolDef {
    name: "fetch_url",
    description: "Fetch a public web page and return its readable text. Cannot access private/local network addresses.",
    params: &[ParamSpec {
        name: "url",
        ty: ParamType::Str,
        required: true,
        description: "http(s) URL to fetch.",
    }],
};

#[async_trait]
impl Tool for FetchUrlTool {
    fn def(&self) -> &'static ToolDef {
        &FETCH_DEF
    }
    fn available(&self, ctx: &ToolContext) -> Result<(), ToolError> {
        if !ctx.settings.tools.search {
            return Err(ToolError::Unavailable(
                "web tools are disabled in Settings (Tools).".into(),
            ));
        }
        Ok(())
    }
    async fn execute(
        &self,
        ctx: &ToolContext,
        args: &serde_json::Value,
    ) -> Result<ToolResult, ToolError> {
        let url = args["url"].as_str().expect("validated");
        let page = ctx.http.fetch_text(url, 2_000_000).await?;
        let title = page.title.unwrap_or_default();
        let mut text = page.text;
        if text.len() > 12_000 {
            text.truncate(12_000);
            text.push_str("\n... (truncated)");
        }
        let out = format!("URL: {url}\nTitle: {title}\n\n{text}");
        Ok(ToolResult {
            summary: format!("fetched {}", &page.url[..page.url.len().min(60)]),
            text: out,
            diff: None,
        })
    }
}
