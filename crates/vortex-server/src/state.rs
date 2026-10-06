//! Shared application state for the server.

use std::sync::Arc;
use vortex_agents::{Engine, Hub, PreviewRegistry};
use vortex_core::{AppConfig, Db};
use vortex_llm::LlmClient;
use vortex_types::Capabilities;

pub struct AppState {
    pub db: Db,
    pub engine: Arc<Engine>,
    pub hub: Arc<Hub>,
    pub llm: Arc<dyn LlmClient>,
    pub config: AppConfig,
    pub previews: Arc<PreviewRegistry>,
    pub is_mock_llm: bool,
    pub version: &'static str,
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
            "Set a SearXNG instance URL in Settings → Search (see README.md 'Search setup')."
                .to_string()
        };
        let browser_ok = self.engine.browser.is_available();
        Capabilities {
            version: self.version.to_string(),
            openrouter: self.is_mock_llm || self.config.api_key().is_some(),
            mock_llm: self.is_mock_llm,
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
