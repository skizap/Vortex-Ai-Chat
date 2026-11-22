//! Run routes: SSE event stream, stop, retry, state snapshot.

use crate::state::{err_json, AppState};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderName, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::IntoResponse;
use axum::Json;
use futures::stream; // for the fallback iterator
use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::Arc;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::Stream;
use tokio_stream::StreamExt;
use vortex_agents::Hub;
use vortex_types::{Role, RunEvent, RunInfo, RunStatus};

/// Replay cursor for an SSE (re-)subscription: the `Last-Event-ID` header
/// (sent automatically by the browser's EventSource on reconnect) and the
/// `?since=<seq>` query parameter (UI manual re-subscribe). When both are
/// present the newer cursor wins; when neither is, replay starts at 0
/// (the full retained log).
const LAST_EVENT_ID: HeaderName = HeaderName::from_static("last-event-id");

fn replay_cursor(headers: &HeaderMap, query: &HashMap<String, String>) -> u64 {
    let from_header = headers
        .get(&LAST_EVENT_ID)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok());
    let from_query = query.get("since").and_then(|s| s.parse::<u64>().ok());
    from_header.max(from_query).unwrap_or(0)
}

/// GET /api/runs/{id}/events — SSE stream of the run and its whole agent tree.
///
/// Events carry monotonically increasing per-run `id:` fields (the hub's
/// sequence numbers). On (re-)subscription the retained run log is replayed
/// from the client's cursor, then the live broadcast is tailed with
/// per-connection dedup for the replay/live overlap. If the cursor predates
/// the retained window, a named `replay_gap` event is emitted first so the
/// client can resync from the run snapshot.
pub async fn run_events(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    axum::extract::Query(q): axum::extract::Query<HashMap<String, String>>,
) -> axum::response::Response {
    let since = replay_cursor(&headers, &q);
    // Subscribe *before* replaying so no event can slip between the log
    // snapshot and the live tail (duplicates are possible instead, and are
    // filtered per connection below).
    let Some(rx) = state.hub.subscribe(&id) else {
        // Terminal/unknown runs replay a single final event so the UI settles.
        let final_status = match state.db.get_run(id.clone()).await {
            Ok(Some(r)) => r.status,
            Ok(None) => RunStatus::Interrupted,
            Err(_) => {
                // Same degraded response as "unknown run"; the DB failure is
                // traced so it is not silently invisible.
                crate::diagnostics::db_failure("get_run", Some(&id));
                RunStatus::Interrupted
            }
        };
        let event = RunEvent::RunFinished {
            run_id: id,
            status: final_status,
            error: None,
        };
        let stream = stream::iter(vec![Ok::<Event, Infallible>(
            Event::default()
                .json_data(event)
                .expect("run event serializes"),
        )]);
        return sse_response(stream);
    };
    let stream = live_event_stream(&state.hub, &id, since, rx);
    sse_response(stream)
}

/// Build the live SSE stream for a registered run: replay everything the log
/// still retains above `since`, then tail the broadcast. `rx` must have been
/// created *before* the replay call so the overlap can only duplicate — the
/// `last_sent` filter then delivers every event exactly once per connection.
fn live_event_stream(
    hub: &Hub,
    id: &str,
    since: u64,
    rx: tokio::sync::broadcast::Receiver<(u64, RunEvent)>,
) -> impl Stream<Item = Result<Event, Infallible>> {
    let (replayed, oldest_retained, newest) = hub.replay_from(id, since);
    let mut head: Vec<Result<Event, Infallible>> = Vec::new();
    // A cursor above the newest logged seq is a future cursor (client bug or
    // stale value): clamp it, otherwise the live tail below would swallow
    // every new event until the sequence caught up to the bogus cursor.
    let mut last_sent = newest.map_or(0, |n| since.min(n));
    // Unrecoverable window gap: the client is missing events between `since`
    // and the oldest retained seq, so it must resync from the snapshot.
    // (A cursor *above* the newest seq is not a gap — just "nothing new yet".)
    if let Some(oldest) = oldest_retained {
        if since.saturating_add(1) < oldest {
            head.push(Ok(Event::default()
                .event("replay_gap")
                .data(oldest.to_string())));
        }
    }
    for (seq, ev) in replayed {
        if seq > last_sent {
            last_sent = seq;
            head.push(Ok(Event::default()
                .id(seq.to_string())
                .json_data(ev)
                .expect("run event serializes")));
        }
    }
    let live = BroadcastStream::new(rx).filter_map(move |item| match item {
        Ok((seq, ev)) => {
            if seq <= last_sent {
                return None; // replay/live overlap: already delivered
            }
            last_sent = seq;
            Some(Ok(Event::default()
                .id(seq.to_string())
                .json_data(ev)
                .expect("run event serializes")))
        }
        Err(_lagged) => {
            // Consumer lagged behind the broadcast buffer; hint the client to
            // resync its snapshot (a following reconnect can then replay from
            // the log properly via Last-Event-ID).
            Some(Ok(Event::default().event("replay_gap").data("lagged")))
        }
    });
    stream::iter(head).chain(live)
}

fn sse_response<S>(stream: S) -> axum::response::Response
where
    S: Stream<Item = Result<Event, Infallible>> + Send + 'static,
{
    Sse::new(stream)
        .keep_alive(
            KeepAlive::new()
                .interval(std::time::Duration::from_secs(15))
                .text("ping"),
        )
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
    let run = match state.db.get_run(id.clone()).await {
        Ok(Some(run)) => run,
        Ok(None) => return err_json(StatusCode::NOT_FOUND, "run not found"),
        Err(_) => {
            crate::diagnostics::db_failure("get_run", Some(&id));
            return err_json(StatusCode::NOT_FOUND, "run not found");
        }
    };
    let children = match state.db.list_child_runs(id.clone()).await {
        Ok(children) => children,
        Err(_) => {
            crate::diagnostics::db_failure("list_child_runs", Some(&id));
            Vec::new()
        }
    };
    let tool_calls = match state.db.list_tool_calls(id.clone()).await {
        Ok(calls) => calls,
        Err(_) => {
            crate::diagnostics::db_failure("list_tool_calls", Some(&id));
            Vec::new()
        }
    };
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
        Some(conv) => match state.db.list_runs_for_conversation(conv.clone()).await {
            Ok(list) => list,
            Err(_) => {
                crate::diagnostics::db_failure("list_runs_for_conversation", Some(conv));
                Vec::new()
            }
        },
        None => Vec::new(),
    };
    Json(list)
}

/// POST /api/runs/{id}/retry — rerun the last user message of the
/// run's conversation as a new run.
pub async fn retry_run(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> axum::response::Response {
    let run = match state.db.get_run(id.clone()).await {
        Ok(Some(run)) => run,
        Ok(None) => return err_json(StatusCode::NOT_FOUND, "run not found"),
        Err(_) => {
            crate::diagnostics::db_failure("get_run", Some(&id));
            return err_json(StatusCode::NOT_FOUND, "run not found");
        }
    };
    let Some(conv_id) = run.conversation_id.clone() else {
        return err_json(StatusCode::BAD_REQUEST, "run has no conversation to retry");
    };

    // Honest pre-flight (same as post_chat): refuse to start a run that
    // cannot talk to a provider, instead of failing deep inside it.
    if !state.is_mock_llm && state.config.api_key().is_none() {
        return err_json(
            StatusCode::SERVICE_UNAVAILABLE,
            "OPENROUTER_API_KEY is not configured. Add it to your environment or config.toml \
             (see README.md 'Setup'), or run with VORTEX_LLM=mock for an offline demo.",
        );
    }

    let mut messages = match state.db.get_messages(conv_id.clone()).await {
        Ok(m) => m,
        Err(_) => {
            crate::diagnostics::db_failure("get_messages", Some(&conv_id));
            Vec::new()
        }
    };
    let Some(last_user_idx) = messages.iter().rposition(|m| m.role == Role::User) else {
        return err_json(StatusCode::BAD_REQUEST, "no user message to retry");
    };
    let retry_content = messages[last_user_idx].content.clone();
    let conversation = match state.db.get_conversation(conv_id.clone()).await {
        Ok(Some(c)) => c,
        Ok(None) => return err_json(StatusCode::NOT_FOUND, "conversation not found"),
        Err(_) => {
            crate::diagnostics::db_failure("get_conversation", Some(&conv_id));
            return err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "could not load conversation",
            );
        }
    };
    let retry_attachments = messages[last_user_idx].attachments.clone();
    // Start with everything before the retried message; the engine appends
    // the retried user turn and filters out stored tool-role rows.
    messages.truncate(last_user_idx);
    let preflight = match state
        .engine
        .preflight_chat_for_send(
            &conv_id,
            conversation.mode,
            conversation.model.as_deref(),
            &retry_content,
            &retry_attachments,
            &messages,
        )
        .await
    {
        Ok(preflight) => preflight,
        Err(message) => return err_json(StatusCode::BAD_REQUEST, message),
    };
    let run_id = state
        .engine
        .start_preflighted_chat_run(
            &conv_id,
            conversation.mode,
            messages,
            &retry_content,
            retry_attachments,
            preflight,
        )
        .await;
    Json(serde_json::json!({"run_id": run_id})).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_util::sync::CancellationToken;

    fn delta(run_id: &str, d: &str) -> RunEvent {
        RunEvent::TextDelta {
            run_id: run_id.to_string(),
            delta: d.to_string(),
        }
    }

    fn registered_hub() -> Arc<Hub> {
        let hub = Arc::new(Hub::new());
        hub.register(
            "r1",
            None,
            None,
            "agent",
            "task",
            "model",
            0,
            CancellationToken::new(),
        );
        hub
    }

    /// Local server exposing the same replay/tail pipeline as `run_events`,
    /// with only the Hub as state (the terminal-run fallback needs the DB and
    /// is covered by runtime smoke tests).
    async fn spawn_sse_server(hub: Arc<Hub>) -> String {
        async fn handler(
            State(hub): State<Arc<Hub>>,
            Path(id): Path<String>,
            headers: HeaderMap,
            axum::extract::Query(q): axum::extract::Query<HashMap<String, String>>,
        ) -> axum::response::Response {
            let since = replay_cursor(&headers, &q);
            let Some(rx) = hub.subscribe(&id) else {
                return err_json(StatusCode::NOT_FOUND, "run not found");
            };
            sse_response(live_event_stream(&hub, &id, since, rx))
        }
        let app = axum::Router::new()
            .route("/api/runs/{id}/events", axum::routing::get(handler))
            .with_state(hub);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    /// Connect and accumulate SSE frames until `done` accepts the buffer
    /// (or 10 s elapse). Returns everything received so far.
    async fn read_until(
        url: &str,
        headers: &[(&str, &str)],
        done: impl Fn(&str) -> bool,
    ) -> String {
        let client = reqwest::Client::new();
        let mut req = client.get(url);
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        let resp = req.send().await.expect("sse connect");
        assert_eq!(resp.status(), 200);
        let mut stream = resp.bytes_stream();
        let mut buf = String::new();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        while tokio::time::Instant::now() < deadline && !done(&buf) {
            match tokio::time::timeout(std::time::Duration::from_secs(5), stream.next()).await {
                Ok(Some(Ok(bytes))) => buf.push_str(&String::from_utf8_lossy(&bytes)),
                Ok(Some(Err(e))) => panic!("sse stream error: {e}"),
                Ok(None) => break,
                Err(_) => break,
            }
        }
        buf
    }

    /// Every `id:` field in receipt order.
    fn ids(buf: &str) -> Vec<u64> {
        buf.lines()
            .filter_map(|l| l.strip_prefix("id:"))
            .filter_map(|v| v.trim().parse::<u64>().ok())
            .collect()
    }

    #[tokio::test]
    async fn replays_log_then_tails_live_without_duplicates() {
        let hub = registered_hub();
        for i in 1..=5 {
            hub.publish("r1", delta("r1", &format!("d{i}")));
        }
        let base = spawn_sse_server(hub.clone()).await;
        let url = format!("{base}/api/runs/r1/events");

        // Publish a live event shortly after the connection opens: whether it
        // lands in the replay window or the live tail, it must arrive exactly
        // once (server-side per-connection dedup).
        let hub_pub = hub.clone();
        let publisher = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            hub_pub.publish("r1", delta("r1", "d6"));
        });

        let buf = read_until(&url, &[], |b| ids(b).len() >= 6).await;
        publisher.await.unwrap();
        assert_eq!(
            ids(&buf),
            vec![1, 2, 3, 4, 5, 6],
            "replay + live tail must deliver each event exactly once, in order"
        );
        // Data frames carry the serialized run events.
        assert!(
            buf.contains("\"delta\":\"d1\""),
            "replayed data missing: {buf}"
        );
        assert!(buf.contains("\"delta\":\"d6\""), "live data missing: {buf}");
    }

    #[tokio::test]
    async fn since_query_limits_the_replay_window() {
        let hub = registered_hub();
        for i in 1..=5 {
            hub.publish("r1", delta("r1", &format!("d{i}")));
        }
        let base = spawn_sse_server(hub).await;
        let buf = read_until(&format!("{base}/api/runs/r1/events?since=3"), &[], |b| {
            ids(b).len() >= 2
        })
        .await;
        assert_eq!(ids(&buf), vec![4, 5], "only seq > since may be replayed");
        assert!(buf.contains("\"delta\":\"d4\""));
        assert!(!buf.contains("\"delta\":\"d3\""));
    }

    #[tokio::test]
    async fn last_event_id_header_and_max_cursor_semantics() {
        let hub = registered_hub();
        for i in 1..=5 {
            hub.publish("r1", delta("r1", &format!("d{i}")));
        }
        let base = spawn_sse_server(hub).await;
        // Browser auto-reconnect sends Last-Event-ID.
        let buf = read_until(
            &format!("{base}/api/runs/r1/events"),
            &[("Last-Event-ID", "4")],
            |b| !ids(b).is_empty(),
        )
        .await;
        assert_eq!(ids(&buf), vec![5]);
        // Header and query together: the newer (larger) cursor wins.
        let buf = read_until(
            &format!("{base}/api/runs/r1/events?since=2"),
            &[("Last-Event-ID", "4")],
            |b| !ids(b).is_empty(),
        )
        .await;
        assert_eq!(ids(&buf), vec![5]);
    }

    #[tokio::test]
    async fn replay_gap_for_evicted_window_but_not_for_future_cursors() {
        let hub = registered_hub();
        let total = (vortex_agents::hub::MAX_RUN_LOG + 8) as u64;
        for _ in 0..total {
            hub.publish("r1", delta("r1", "x"));
        }
        // Retained window: seqs 9..=total (oldest = total - MAX_RUN_LOG + 1).
        let oldest = total - vortex_agents::hub::MAX_RUN_LOG as u64 + 1;
        let base = spawn_sse_server(hub.clone()).await;

        // A cursor *below* the retained window: gap signal before any replay.
        let buf = read_until(&format!("{base}/api/runs/r1/events?since=1"), &[], |b| {
            b.contains("replay_gap")
        })
        .await;
        assert!(buf.contains("replay_gap"), "missing gap signal: {buf}");
        assert!(
            buf.contains(&format!("data:{oldest}")) || buf.contains(&format!("data: {oldest}")),
            "gap event must carry the oldest retained seq ({oldest}): {buf}"
        );
        let gap_pos = buf.find("replay_gap").unwrap();
        assert!(
            buf.find("id:").map(|p| p > gap_pos).unwrap_or(true),
            "the gap signal must precede the replayed events"
        );

        // A cursor *above* the newest seq is a future cursor, not a gap: no
        // replay_gap, nothing to replay — but live events must still flow
        // (the cursor is clamped, not honored literally).
        let hub_pub = hub.clone();
        let publisher = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            hub_pub.publish("r1", delta("r1", "live"));
        });
        let buf = read_until(
            &format!("{base}/api/runs/r1/events?since=999999"),
            &[],
            |b| !ids(b).is_empty(),
        )
        .await;
        publisher.await.unwrap();
        assert!(
            !buf.contains("replay_gap"),
            "future cursor is not a gap: {buf}"
        );
        assert_eq!(ids(&buf), vec![total + 1]);
        assert!(buf.contains("\"delta\":\"live\""));
    }

    #[test]
    fn replay_cursor_parsing() {
        let mut headers = HeaderMap::new();
        headers.insert("last-event-id", "42".parse().unwrap());
        let mut query = HashMap::new();
        query.insert("since".to_string(), "17".to_string());
        assert_eq!(replay_cursor(&headers, &query), 42);
        headers.remove("last-event-id");
        assert_eq!(replay_cursor(&headers, &query), 17);
        query.insert("since".to_string(), "not-a-number".to_string());
        assert_eq!(replay_cursor(&headers, &query), 0);
        assert_eq!(replay_cursor(&HeaderMap::new(), &HashMap::new()), 0);
    }
}
