//! Conversation and chat routes.

use crate::state::{err_json, AppState};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use std::sync::Arc;
use vortex_types::{ChatMessage, ChatRequestDto, ConversationSummary, Mode};

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
        .map(vortex_core::title::normalize_title)
        .unwrap_or_else(|| vortex_core::title::PLACEHOLDER_TITLE.to_string());
    if let Err(rejection) = vortex_core::title::validate_title(&title) {
        return bad(rejection.message());
    }
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
    let summary = match state
        .db
        .create_conversation_with_model(title, mode, workspace, Some(settings.model))
        .await
    {
        Ok(s) => s,
        Err(e) => return err_json(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    axum::response::IntoResponse::into_response(Json(summary))
}

pub async fn get_model(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> axum::response::Response {
    let model = match state.db.get_conversation(id).await {
        Ok(Some(c)) => match c.model {
            Some(model) => model,
            None => state.engine.current_settings().await.model,
        },
        Ok(None) => return err_json(StatusCode::NOT_FOUND, "conversation not found"),
        Err(e) => return err_json(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    Json(serde_json::json!({"model": model})).into_response()
}

pub async fn set_model(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> axum::response::Response {
    let model = match body.get("model") {
        Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(model)) => {
            let model = model.trim();
            if model.is_empty()
                || model.len() > 200
                || model.chars().any(|c| c.is_whitespace() || c.is_control())
            {
                return bad("enter a model ID without whitespace (max 200 bytes)");
            }
            Some(model.to_owned())
        }
        _ => return bad("provide 'model' as a model ID or null"),
    };
    match state.db.set_conversation_model(id, model.clone()).await {
        Ok(true) => Json(serde_json::json!({"ok": true, "model": model})).into_response(),
        Ok(false) => err_json(StatusCode::NOT_FOUND, "conversation not found"),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
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
    let Some(raw) = body.get("title").and_then(|t| t.as_str()) else {
        return bad("missing 'title'");
    };
    match rename_conversation_with(&state.db, &id, raw).await {
        RenameOutcome::Renamed(canonical) => {
            Json(serde_json::json!({"ok": true, "title": canonical})).into_response()
        }
        RenameOutcome::NotFound => err_json(StatusCode::NOT_FOUND, "conversation not found"),
        RenameOutcome::Invalid(message) => bad(message),
        RenameOutcome::DbError(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

/// Rename result. Free function (same pattern as `routes_usage`) so tests can
/// drive the canonicalization/validation rules with a plain `Db`.
#[derive(Debug, PartialEq)]
pub enum RenameOutcome {
    /// Stored; carries the canonical title exactly as persisted.
    Renamed(String),
    NotFound,
    Invalid(&'static str),
    DbError(String),
}

/// Canonicalize once, server-side: trim and collapse whitespace, reject
/// empty or overlong input, and return the stored title so the client never
/// has to guess what was persisted.
pub async fn rename_conversation_with(db: &vortex_core::Db, id: &str, raw: &str) -> RenameOutcome {
    let canonical = vortex_core::title::normalize_title(raw);
    if let Err(rejection) = vortex_core::title::validate_title(&canonical) {
        return RenameOutcome::Invalid(rejection.message());
    }
    match db
        .rename_conversation(id.to_string(), canonical.clone())
        .await
    {
        Ok(true) => RenameOutcome::Renamed(canonical),
        Ok(false) => RenameOutcome::NotFound,
        Err(e) => RenameOutcome::DbError(e.to_string()),
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

    // Rebuild conversation history for the model (user + assistant only).
    let stored = match state.db.get_messages(id.clone()).await {
        Ok(m) => m,
        Err(_) => {
            crate::diagnostics::db_failure("get_messages", Some(&id));
            return err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "could not load conversation history",
            );
        }
    };
    let preflight = match state
        .engine
        .preflight_chat_for_send(
            &id,
            mode,
            conversation.model.as_deref(),
            content,
            &refs,
            &stored,
        )
        .await
    {
        Ok(preflight) => preflight,
        Err(message) => return bad(message),
    };
    let mut user_msg = ChatMessage::user(content);
    user_msg.attachments = refs.clone();
    if let Err(e) = state.db.add_message(id.clone(), user_msg).await {
        return err_json(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    }
    if state.db.touch_conversation(id.clone()).await.is_err() {
        crate::diagnostics::db_failure("touch_conversation", Some(&id));
    }

    // Local auto-title for placeholder chats: derive a short name from the
    // first request clause (or a local label for attachment-only turns).
    // The write is conditional on the placeholder origin, so a manual rename
    // always wins and later turns never retitle an already-named chat.
    let auto_title = maybe_auto_title(&state.db, &id, content, &refs).await;

    let run_id = state
        .engine
        .start_preflighted_chat_run(&id, mode, stored, content, refs, preflight)
        .await;
    let mut response = serde_json::json!({ "run_id": run_id });
    if let Some(title) = auto_title {
        response["title"] = serde_json::Value::String(title);
    }
    Json(response).into_response()
}

/// Derive and conditionally persist an automatic title. Returns the title
/// that was written, or `None` when the conversation is not at its
/// placeholder origin (already named manually or automatically), nothing
/// could be derived, or the write failed (traced, never fatal to the turn).
pub async fn maybe_auto_title(
    db: &vortex_core::Db,
    id: &str,
    content: &str,
    refs: &[vortex_types::AttachmentRef],
) -> Option<String> {
    let derived = vortex_core::title::title_for_first_turn(content, refs)?;
    match db
        .auto_title_conversation(id.to_string(), derived.clone())
        .await
    {
        Ok(true) => Some(derived),
        Ok(false) => None,
        Err(_) => {
            crate::diagnostics::db_failure("auto_title_conversation", Some(id));
            None
        }
    }
}

use axum::response::IntoResponse;

#[cfg(test)]
mod title_tests {
    use super::{maybe_auto_title, rename_conversation_with, RenameOutcome};
    use vortex_core::Db;
    use vortex_types::{AttachmentKind, AttachmentRef, AttachmentStatus, Mode};

    fn test_db() -> (Db, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        (Db::open(dir.path().join("conv.db")).unwrap(), dir)
    }

    fn aref(kind: AttachmentKind) -> AttachmentRef {
        AttachmentRef {
            id: "a".into(),
            kind,
            filename: "f".into(),
            media_type: "application/octet-stream".into(),
            status: AttachmentStatus::Ready,
        }
    }

    #[tokio::test]
    async fn rename_returns_canonical_title_and_marks_manual() {
        let (db, _dir) = test_db();
        let c = db
            .create_conversation("New chat", Mode::Chat, None)
            .await
            .unwrap();
        let outcome = rename_conversation_with(&db, &c.id, "  Fix   Login \n\tbug  ").await;
        assert_eq!(outcome, RenameOutcome::Renamed("Fix Login bug".into()));
        let got = db.get_conversation(c.id.clone()).await.unwrap().unwrap();
        assert_eq!(got.title, "Fix Login bug", "canonical title is persisted");
        assert_eq!(
            db.title_origin(c.id).await.unwrap().as_deref(),
            Some("manual")
        );
    }

    #[tokio::test]
    async fn rename_rejects_empty_whitespace_and_overlong() {
        let (db, _dir) = test_db();
        let c = db
            .create_conversation("New chat", Mode::Chat, None)
            .await
            .unwrap();
        assert_eq!(
            rename_conversation_with(&db, &c.id, "   ").await,
            RenameOutcome::Invalid("title must not be empty")
        );
        assert_eq!(
            rename_conversation_with(&db, &c.id, "").await,
            RenameOutcome::Invalid("title must not be empty")
        );
        let long = "x".repeat(vortex_core::title::MAX_TITLE_CHARS + 1);
        assert_eq!(
            rename_conversation_with(&db, &c.id, &long).await,
            RenameOutcome::Invalid("title is too long (max 200 characters)")
        );
        // Rejected renames leave the row untouched.
        let got = db.get_conversation(c.id).await.unwrap().unwrap();
        assert_eq!(got.title, "New chat");
    }

    #[tokio::test]
    async fn rename_unknown_conversation_is_not_found() {
        let (db, _dir) = test_db();
        assert_eq!(
            rename_conversation_with(&db, "missing", "name").await,
            RenameOutcome::NotFound
        );
    }

    #[tokio::test]
    async fn auto_title_names_placeholder_chat_exactly_once() {
        let (db, _dir) = test_db();
        let c = db
            .create_conversation("New chat", Mode::Chat, None)
            .await
            .unwrap();
        let written = maybe_auto_title(&db, &c.id, "Fix the login bug. Then ship it.", &[]).await;
        assert_eq!(written.as_deref(), Some("Fix the login bug"));
        // Later turns never retitle an automatically named chat.
        assert_eq!(maybe_auto_title(&db, &c.id, "Second task", &[]).await, None);
        let got = db.get_conversation(c.id).await.unwrap().unwrap();
        assert_eq!(got.title, "Fix the login bug");
    }

    #[tokio::test]
    async fn manual_rename_always_beats_auto_title() {
        let (db, _dir) = test_db();
        let c = db
            .create_conversation("New chat", Mode::Chat, None)
            .await
            .unwrap();
        rename_conversation_with(&db, &c.id, "My roadmap").await;
        assert_eq!(
            maybe_auto_title(&db, &c.id, "Do the thing", &[]).await,
            None
        );
        let got = db.get_conversation(c.id).await.unwrap().unwrap();
        assert_eq!(got.title, "My roadmap");
    }

    #[tokio::test]
    async fn auto_title_labels_attachment_only_turns() {
        let (db, _dir) = test_db();
        let c = db
            .create_conversation("New chat", Mode::Chat, None)
            .await
            .unwrap();
        let refs = vec![aref(AttachmentKind::Video), aref(AttachmentKind::Video)];
        let written = maybe_auto_title(&db, &c.id, "   ", &refs).await;
        assert_eq!(written.as_deref(), Some("Review 2 videos"));
    }

    #[tokio::test]
    async fn auto_title_skips_when_nothing_derivable() {
        let (db, _dir) = test_db();
        let c = db
            .create_conversation("New chat", Mode::Chat, None)
            .await
            .unwrap();
        // Markup-only content with no attachments derives nothing.
        assert_eq!(
            maybe_auto_title(&db, &c.id, "```\nfn x() {}\n```", &[]).await,
            None
        );
        let got = db.get_conversation(c.id.clone()).await.unwrap().unwrap();
        assert_eq!(got.title, "New chat");
        assert_eq!(
            db.title_origin(c.id.clone()).await.unwrap().as_deref(),
            Some("placeholder")
        );
        // Unknown conversation: no write, no panic.
        assert_eq!(
            maybe_auto_title(&db, "missing", "hello there", &[]).await,
            None
        );
    }
}
