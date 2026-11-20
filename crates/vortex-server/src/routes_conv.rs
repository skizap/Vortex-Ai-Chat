//! Conversation and chat routes.

use crate::state::{err_json, AppState};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use std::sync::Arc;
use vortex_types::{ChatMessage, ChatRequestDto, ConversationSummary, Mode, Settings};

fn bad(msg: impl Into<String>) -> axum::response::Response {
    err_json(StatusCode::BAD_REQUEST, msg)
}

pub async fn create_conversation(
    State(state): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>,
) -> axum::response::Response {
    let title = body
        .get("title")
        .and_then(|t| t.as_str())
        .map(|s| s.chars().take(120).collect::<String>())
        .unwrap_or_else(|| "New chat".to_string());
    let mode = body
        .get("mode")
        .and_then(|m| serde_json::from_value::<Mode>(m.clone()).ok())
        .unwrap_or_default();
    let settings = state.engine.current_settings().await;
    let workspace = if mode == Mode::Coding || mode == Mode::Browser {
        settings.workspace.clone()
    } else {
        None
    };
    let summary = match state.db.create_conversation(title, mode, workspace).await {
        Ok(s) => s,
        Err(e) => return err_json(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    axum::response::IntoResponse::into_response(Json(summary))
}

#[derive(Deserialize)]
pub struct ListQuery {
    pub q: Option<String>,
}

pub async fn list_conversations(
    State(state): State<Arc<AppState>>,
    Query(q): Query<ListQuery>,
) -> Json<Vec<ConversationSummary>> {
    let result = match q.q {
        Some(query) if !query.trim().is_empty() => state.db.search_conversations(query).await,
        _ => state.db.list_conversations().await,
    };
    let list = match result {
        Ok(list) => list,
        Err(_) => {
            // Swallowed to keep the response shape; failure is traced with a
            // fixed category (never the error string).
            crate::diagnostics::db_failure("list_conversations", None);
            Vec::new()
        }
    };
    Json(list)
}

pub async fn rename_conversation(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> axum::response::Response {
    let title = body
        .get("title")
        .and_then(|t| t.as_str())
        .map(|s| s.chars().take(200).collect::<String>());
    let Some(title) = title else {
        return bad("missing 'title'");
    };
    if title.trim().is_empty() {
        return bad("title must not be empty");
    }
    match state.db.rename_conversation(id, title).await {
        Ok(true) => Json(serde_json::json!({"ok": true})).into_response(),
        Ok(false) => err_json(StatusCode::NOT_FOUND, "conversation not found"),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

pub async fn delete_conversation(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> axum::response::Response {
    match state.db.delete_conversation(id).await {
        Ok(true) => Json(serde_json::json!({"ok": true})).into_response(),
        Ok(false) => err_json(StatusCode::NOT_FOUND, "conversation not found"),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

pub async fn get_messages(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> axum::response::Response {
    let stored = match state.db.get_messages(id.clone()).await {
        Ok(m) => m,
        Err(_) => {
            crate::diagnostics::db_failure("get_messages", Some(&id));
            Vec::new()
        }
    };
    Json(stored).into_response()
}

/// POST /api/conversations/{id}/chat — persist the user message and start a
/// coordinator run. Returns the run id; the UI follows via SSE.
pub async fn post_chat(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<ChatRequestDto>,
) -> axum::response::Response {
    let content = body.content.trim();
    let attachment_ids = body.attachment_ids;
    if attachment_ids.len() > crate::routes_attachments::MAX_PER_MESSAGE {
        return bad(format!(
            "at most {} attachments per message",
            crate::routes_attachments::MAX_PER_MESSAGE
        ));
    }
    if content.is_empty() && attachment_ids.is_empty() {
        return bad("message content must not be empty (or attach a file)");
    }
    if content.len() > 100_000 {
        return bad("message too long (max 100KB)");
    }
    let conversation = match state.db.get_conversation(id.clone()).await {
        Ok(Some(c)) => c,
        // DB errors intentionally degrade to "not found" here; the failure
        // is traced so silent persistence breakage stays visible.
        Ok(None) => return err_json(StatusCode::NOT_FOUND, "conversation not found"),
        Err(_) => {
            crate::diagnostics::db_failure("get_conversation", Some(&id));
            return err_json(StatusCode::NOT_FOUND, "conversation not found");
        }
    };
    let mode = body.mode.unwrap_or(conversation.mode);

    // Honest pre-flight: refuse to start runs that cannot talk to a provider.
    if !state.is_mock_llm && state.config.api_key().is_none() {
        return err_json(
            StatusCode::SERVICE_UNAVAILABLE,
            "OPENROUTER_API_KEY is not configured. Add it to your environment or config.toml \
             (see README.md 'Setup'), or run with VORTEX_LLM=mock for an offline demo.",
        );
    }

    // Validate attachments: each must exist, belong to this conversation, and
    // be ready. Deduplicate while preserving order.
    let mut refs: Vec<vortex_types::AttachmentRef> = Vec::new();
    for aid in &attachment_ids {
        if refs.iter().any(|r| &r.id == aid) {
            continue;
        }
        match state.attachments.get_meta(aid).await {
            Ok(Some(m))
                if m.conversation_id == id && m.status == vortex_types::AttachmentStatus::Ready =>
            {
                refs.push(vortex_types::AttachmentRef {
                    id: m.id,
                    kind: m.kind,
                    filename: m.filename,
                    media_type: m.media_type,
                    status: m.status,
                });
            }
            Ok(Some(_)) => {
                return bad("attachment does not belong to this conversation or is not ready yet");
            }
            _ => return bad(format!("attachment '{aid}' not found")),
        }
    }

    let preflight = match state.engine.preflight_chat(&id, &refs).await {
        Ok(preflight) => preflight,
        Err(message) => return bad(message),
    };

    // Rebuild conversation history for the model (user + assistant only).
    let stored = match state.db.get_messages(id.clone()).await {
        Ok(m) => m,
        Err(_) => {
            crate::diagnostics::db_failure("get_messages", Some(&id));
            Vec::new()
        }
    };
    let mut user_msg = ChatMessage::user(content);
    user_msg.attachments = refs.clone();
    if let Err(e) = state.db.add_message(id.clone(), user_msg).await {
        return err_json(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    }
    if state.db.touch_conversation(id.clone()).await.is_err() {
        crate::diagnostics::db_failure("touch_conversation", Some(&id));
    }

    let run_id = state
        .engine
        .start_preflighted_chat_run(&id, mode, stored, content, refs, preflight)
        .await;
    Json(serde_json::json!({"run_id": run_id})).into_response()
}

use axum::response::IntoResponse;

#[allow(unused)]
fn _keep(_: Option<Settings>) {}
