//! Shared application state for the server.

use crate::guard::GuardSettings;
use std::sync::Arc;
use vortex_agents::search::SearchProvider;
use vortex_agents::{Engine, Hub, PreviewRegistry};
use vortex_core::{AppConfig, AttachmentStore, Db};
use vortex_types::{Capabilities, Settings};

/// Build the search provider for the given saved settings. Single source of
/// truth for the selection policy, shared by startup and settings saves so a
/// changed/removed URL applies to subsequent runs without a restart:
/// mock-LLM mode always selects the clearly-labeled offline mock provider,
/// an explicit `provider = "mock"` does the same, a configured SearXNG URL
/// builds the real adapter, and anything else honestly yields `None`
/// (research tools then report the missing setup).
pub fn build_search_provider(
    settings: &Settings,
    is_mock_llm: bool,
) -> Option<Arc<dyn SearchProvider>> {
    if is_mock_llm || settings.search.provider == "mock" {
        Some(Arc::new(vortex_agents::search::MockSearchProvider))
    } else if settings.search.is_configured() {
        Some(Arc::new(vortex_agents::search::SearxngProvider::new(
            settings.search.base_url.trim(),
        )))
    } else {
        None
    }
}

pub struct AppState {
    pub db: Db,
    pub engine: Arc<Engine>,
    pub hub: Arc<Hub>,
    pub config: AppConfig,
    pub previews: Arc<PreviewRegistry>,
    pub attachments: Arc<AttachmentStore>,
    pub is_mock_llm: bool,
    pub version: &'static str,
    /// Inputs for the Host/Origin/LAN guard (split out for testability).
    pub guard: GuardSettings,
    /// Session-scoped diagnostics (env-only opt-in; never persisted).
    pub diagnostics: crate::diagnostics::DiagnosticsState,
    /// Job lifecycle supervisor: created and started EXACTLY ONCE in main
    /// (tick loop + startup catch-up); routes use it for manual-tick tests,
    /// catch-up after review resolution, and the job-event SSE channel.
    pub supervisor: Arc<vortex_agents::supervisor::Supervisor>,
}

impl AppState {
    pub async fn capabilities(&self) -> Capabilities {
        let settings = self.engine.current_settings().await;
        let search_configured = self.is_mock_llm || settings.search.is_configured();
        let search_provider = if self.is_mock_llm {
            "mock".to_string()
        } else {
            settings.search.provider.clone()
        };
        let search_hint = if search_configured {
            String::new()
        } else {
            SEARCH_SETUP_HINT.to_string()
        };
        let browser_ok = self.engine.browser.is_available();
        Capabilities {
            version: self.version.to_string(),
            openrouter: self.is_mock_llm || self.config.api_key().is_some(),
            mock_llm: self.is_mock_llm,
            diagnostics: self.diagnostics.enabled,
            search: search_configured && settings.tools.search,
            search_provider,
            search_hint,
            browser: browser_ok && settings.tools.browser,
            browser_hint: if browser_ok {
                String::new()
            } else {
                vortex_agents::browser::BrowserManager::unavailable_hint().to_string()
            },
            commands: settings.tools.commands,
            previews: settings.tools.previews,
            workspace: settings.workspace.is_some(),
            subagents: settings.tools.delegation,
            max_concurrent_subagents: settings.agents.max_concurrent,
        }
    }
}

/// Fixed next-step text for unconfigured search (capabilities + probe).
pub const SEARCH_SETUP_HINT: &str =
    "Set a SearXNG instance URL in Settings → Search (see README.md 'Search setup').";

/// Uniform JSON error body: `{"error": "..."}`. Never contains secrets.
pub fn err_json(
    status: axum::http::StatusCode,
    message: impl Into<String>,
) -> axum::response::Response {
    let body = serde_json::json!({ "error": message.into() });
    axum::Json(body).into_response_with_status(status)
}

trait IntoResponseWithStatus {
    fn into_response_with_status(self, status: axum::http::StatusCode) -> axum::response::Response;
}

impl IntoResponseWithStatus for axum::Json<serde_json::Value> {
    fn into_response_with_status(self, status: axum::http::StatusCode) -> axum::response::Response {
        let mut resp = axum::response::IntoResponse::into_response(self);
        *resp.status_mut() = status;
        resp
    }
}
