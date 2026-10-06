//! Miscellaneous routes: health/capabilities, settings, models, approvals,
//! workspace validation, previews.

use crate::state::{err_json, AppState};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use std::sync::Arc;
use vortex_types::{AgentSettings, Settings};

pub async fn health(State(state): State<Arc<AppState>>) -> axum::response::Response {
    Json(state.capabilities().await).into_response()
}

pub async fn get_settings(State(state): State<Arc<AppState>>) -> Json<Settings> {
    Json(state.engine.current_settings().await)
}

pub async fn put_settings(
    State(state): State<Arc<AppState>>,
    Json(mut settings): Json<Settings>,
) -> axum::response::Response {
    // Validate and clamp before accepting anything.
    settings.agents.max_concurrent = settings
        .agents
        .max_concurrent
        .clamp(1, AgentSettings::MAX_LIMIT_CEILING);
    settings.agents.max_iterations = settings.agents.max_iterations.clamp(1, 64);
    settings.agents.run_timeout_secs = settings.agents.run_timeout_secs.clamp(30, 3600);
    settings.agents.approval_timeout_secs = settings.agents.approval_timeout_secs.clamp(10, 3600);
    settings.temperature = settings.temperature.clamp(0.0, 2.0);
    settings.max_tokens = settings.max_tokens.clamp(64, 100_000);
    settings.search.max_results = settings.search.max_results.clamp(1, 20);
    if let Some(ws) = settings.workspace.clone() {
        let trimmed = ws.trim().to_string();
        if trimmed.is_empty() {
            settings.workspace = None;
        } else if !std::path::Path::new(&trimmed).is_dir() {
            return err_json(
                StatusCode::BAD_REQUEST,
                format!("workspace '{}' is not a directory", trimmed),
            );
        } else {
            settings.workspace = Some(trimmed);
        }
    }
    if !state.is_mock_llm && !settings.search.base_url.trim().is_empty() {
        // Validate search URL shape early for a better error message.
        if url::Url::parse(settings.search.base_url.trim()).is_err() {
            return err_json(
                StatusCode::BAD_REQUEST,
                "search base_url is not a valid URL",
            );
        }
    }
    if let Err(e) = state.db.save_settings(&settings).await {
        return err_json(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    }
    state.engine.update_settings(settings.clone()).await;
    Json(settings).into_response()
}

/// GET /api/models — server-side OpenRouter catalog; honest fallback with
/// manual-ID instructions when retrieval fails.
pub async fn list_models(State(state): State<Arc<AppState>>) -> axum::response::Response {
    match state.llm.models().await {
        Ok(models) => Json(serde_json::json!({
            "models": models,
            "configured": state.is_mock_llm || state.config.api_key().is_some(),
        }))
        .into_response(),
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({
                "error": format!("could not fetch model catalog: {e}"),
                "configured": false,
                "hint": "You can still set a model ID manually in Settings (e.g. 'openai/gpt-4o-mini' \
                         or 'anthropic/claude-sonnet-4'). Check your API key and network.",
            })),
        )
            .into_response(),
    }
}

/// GET /api/approvals — approvals pending a human decision.
pub async fn list_approvals(
    State(state): State<Arc<AppState>>,
) -> Json<Vec<vortex_types::ApprovalInfo>> {
    Json(state.engine.approvals.pending_approvals().await)
}

/// POST /api/approvals/{id}/decide {"approve": bool} — the ONLY way an
/// approval can be resolved. Agents (models) have no access to this route's
/// effects beyond the request they initiated.
pub async fn decide_approval(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> axum::response::Response {
    let approve = body
        .get("approve")
        .and_then(|a| a.as_bool())
        .unwrap_or(false);
    match state.engine.approvals.decide(&id, approve).await {
        true => Json(serde_json::json!({"ok": true})).into_response(),
        false => err_json(
            StatusCode::NOT_FOUND,
            "approval not found or already resolved",
        ),
    }
}

/// GET /api/workspace/check?path=... — validate a workspace root candidate.
pub async fn check_workspace(
    State(_state): State<Arc<AppState>>,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> axum::response::Response {
    let Some(path) = q.get("path").map(|s| s.to_string()) else {
        return err_json(StatusCode::BAD_REQUEST, "missing 'path' parameter");
    };
    let path = path.trim().to_string();
    match vortex_agents::workspace::WorkspaceRoots::new(std::path::Path::new(&path)) {
        Ok(roots) => Json(serde_json::json!({
            "ok": true,
            "root": roots.root().display().to_string(),
        }))
        .into_response(),
        Err(e) => err_json(StatusCode::BAD_REQUEST, e.to_string()),
    }
}

pub async fn list_previews(
    State(state): State<Arc<AppState>>,
) -> Json<Vec<vortex_types::PreviewInfo>> {
    Json(state.previews.list())
}
