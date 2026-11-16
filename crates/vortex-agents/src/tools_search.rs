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

/// Cap tool-output text by bytes without ever splitting a UTF-8 character:
/// `String::truncate` panics on a non-char boundary, and fetched pages are
/// full of multi-byte text (same bug class as C6/N6).
fn cap_text(mut text: String, cap: usize) -> String {
    if text.len() > cap {
        let mut cut = cap;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
        text.push_str("\n... (truncated)");
    }
    text
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
        let text = cap_text(page.text, 12_000);
        let out = format!("URL: {url}\nTitle: {title}\n\n{text}");
        // Truncate by characters, not bytes: URLs are model/provider-provided
        // and a byte cut can land inside a multi-byte UTF-8 character.
        let short_url: String = page.url.chars().take(60).collect();
        Ok(ToolResult {
            summary: format!("fetched {short_url}"),
            text: out,
            diff: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::cap_text;

    #[test]
    fn cap_text_never_splits_a_utf8_character() {
        // Each char is 3 bytes; 12_000 is a multiple of 3 but the fetched
        // text rarely aligns — mix widths so the cut lands mid-character.
        let big = format!("{}{}", "あ".repeat(5_000), "é".repeat(2_000));
        let out = cap_text(big, 12_000);
        assert!(out.ends_with("\n... (truncated)"));
        assert!(out.len() <= 12_000 + "\n... (truncated)".len());
        // Everything kept must be whole characters (no panic, no mojibake).
        let kept: String = out.trim_end_matches("\n... (truncated)").to_string();
        assert!(kept.chars().all(|c| c == 'あ' || c == 'é'));
        assert!(kept.len() <= 12_000);
    }

    #[test]
    fn cap_text_passes_short_text_through() {
        assert_eq!(cap_text("short".into(), 12_000), "short");
        let exact = "x".repeat(12_000);
        assert_eq!(cap_text(exact.clone(), 12_000), exact);
    }
}
