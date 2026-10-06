//! Run routes: SSE event stream, stop, retry, state snapshot.

use crate::state::{err_json, AppState};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::IntoResponse;
use axum::Json;
use futures::stream; // for the fallback iterator
use tokio_stream::StreamExt;
use std::convert::Infallible;
use std::sync::Arc;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::Stream;
use vortex_types::{Role, RunEvent, RunInfo, RunStatus};

/// GET /api/runs/{id}/events — SSE stream of the run and its whole agent tree.
pub async fn run_events(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> axum::response::Response {
    // Terminal/unknown runs replay a single final event so the UI settles.
    let Some(rx) = state.hub.subscribe(&id) else {
        let final_status = state
            .db
            .get_run(id.clone())
            .await
            .ok()
            .flatten()
            .map(|r| r.status)
            .unwrap_or(RunStatus::Interrupted);
        let event = RunEvent::RunFinished {
            run_id: id,
            status: final_status,
            error: None,
        };
        let stream = stream::iter(vec![Ok::<Event, Infallible>(
            Event::default().json_data(event).expect("run event serializes"),
        )]);
        return sse_response(stream);
    };
    let stream = BroadcastStream::new(rx).filter_map(|item| match item {
        Ok(ev) => Some(Ok(Event::default().json_data(ev).expect("run event serializes"))),
        Err(_lagged) => {
            // Consumer lagged; hint the client to refresh its snapshot.
            Some(Ok(Event::default()
                .event("lagged")
                .data("snapshot")))
        }
    });
    sse_response(stream)
}

fn sse_response<S>(stream: S) -> axum::response::Response
where
    S: Stream<Item = Result<Event, Infallible>> + Send + 'static,
{
    Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(std::time::Duration::from_secs(15)).text("ping"))
        .into_response()
}

/// POST /api/runs/{id}/stop — cancel the run (children cancel via linked tokens).
pub async fn stop_run(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> axum::response::Response {
    if state.hub.get(&id).is_none() {
        return err_json(StatusCode::NOT_FOUND, "run not found or already finished");
    }
    state.hub.cancel(&id);
    Json(serde_json::json!({"ok": true})).into_response()
}

/// GET /api/runs/{id}/state — snapshot: run info, child agents, tool calls.
pub async fn run_state(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> axum::response::Response {
    let Some(run) = state.db.get_run(id.clone()).await.unwrap_or(None) else {
        return err_json(StatusCode::NOT_FOUND, "run not found");
    };
    let children = state.db.list_child_runs(id.clone()).await.unwrap_or_default();
    let tool_calls = state.db.list_tool_calls(id).await.unwrap_or_default();
    Json(serde_json::json!({
        "run": run,
        "agents": children,
        "tool_calls": tool_calls,
    }))
    .into_response()
}

/// GET /api/runs?conversation_id=... — run history for a conversation.
pub async fn list_runs(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Json<Vec<RunInfo>> {
    let list = match q.get("conversation_id") {
        Some(conv) => state.db.list_runs_for_conversation(conv.clone()).await,
        None => Ok(Vec::new()),
    }
    .unwrap_or_default();
    Json(list)
}

/// POST /api/runs/{id}/retry — rerun the last user message of the
/// run's conversation as a new run.
pub async fn retry_run(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> axum::response::Response {
    let Some(run) = state.db.get_run(id.clone()).await.unwrap_or(None) else {
        return err_json(StatusCode::NOT_FOUND, "run not found");
    };
    let Some(conv_id) = run.conversation_id.clone() else {
        return err_json(StatusCode::BAD_REQUEST, "run has no conversation to retry");
    };
    let messages = state.db.get_messages(conv_id.clone()).await.unwrap_or_default();
    let Some(last_user) = messages.iter().rev().find(|m| m.role == Role::User) else {
        return err_json(StatusCode::BAD_REQUEST, "no user message to retry");
    };
    let retry_content = last_user.content.clone();
    let mode = state
        .db
        .list_conversations()
        .await
        .unwrap_or_default()
        .into_iter()
        .find(|c| c.id == conv_id)
        .map(|c| c.mode)
        .unwrap_or_default();
    let last_user_idx = messages
        .iter()
        .rposition(|m| m.role == Role::User)
        .expect("checked above");
    let history: Vec<vortex_types::ChatMessage> = messages
        .into_iter()
        .enumerate()
        .take(last_user_idx + 1)
        .map(|(_, m)| vortex_types::ChatMessage {
            role: m.role,
            content: m.content,
            tool_calls: Vec::new(),
            tool_call_id: None,
            tool_name: None,
        })
        .collect();
    let run_id = state
        .engine
        .start_chat_run(&conv_id, mode, history, &retry_content)
        .await;
    Json(serde_json::json!({"run_id": run_id})).into_response()
}
