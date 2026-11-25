//! Offline integration tests for the saved search-provider path (plan step 3
//! and its validation bullet): a local SearXNG JSON fixture plus a scripted
//! recording LLM injected *separately* from search (mock-LLM startup mode
//! would auto-select mock search, so the fixture-backed tests run with
//! `is_mock_llm = false` and a stubbed key). No OpenRouter, no self-hosted
//! service, no external network — everything binds 127.0.0.1:0.
//!
//! Covered: save URL → probe → Research `web_search` results reaching the
//! model loop → change/clear URL applying to the *next* run without a
//! restart; disabled/unconfigured states; 403, non-JSON, missing/non-array
//! `results`, oversized body, unreachable provider; empty results; Chat
//! never gaining search; guard preservation on the new POST route; and mock
//! labeling in mock mode.

use crate::state::AppState;
use async_trait::async_trait;
use axum::extract::{Path, Query, State as AxState};
use axum::http::{header, StatusCode};
use axum::response::IntoResponse;
use axum::routing::get;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::mpsc;
use vortex_llm::llm::{CompletionRequest, CompletionResponse, LlmClient, LlmEvent};
use vortex_types::{
    Mode, Role, RunInfo, RunStatus, SearchTestErrorKind, SearchTestResult, SearchTestStatus,
    Settings,
};

// ---------------------------------------------------------------------------
// Fixture SearXNG: one server, behavior chosen by URL path variant so tests
// can "change the saved URL" between instances.
// ---------------------------------------------------------------------------

type FixtureLog = Arc<tokio::sync::Mutex<Vec<(String, String, String)>>>;

async fn fixture_searxng() -> (String, FixtureLog) {
    async fn search(
        AxState(log): AxState<FixtureLog>,
        Path(variant): Path<String>,
        Query(q): Query<HashMap<String, String>>,
    ) -> axum::response::Response {
        log.lock().await.push((
            variant.clone(),
            q.get("q").cloned().unwrap_or_default(),
            q.get("format").cloned().unwrap_or_default(),
        ));
        match variant.as_str() {
            "ok" => axum::Json(serde_json::json!({
                "results": [
                    {
                        "title": "Fixture result one",
                        "url": "https://fixture-a.example/one",
                        "content": "marker-fixture-a snippet one",
                    },
                    {
                        "title": "Fixture result two",
                        "url": "https://fixture-a.example/two",
                        "content": "snippet two",
                    },
                ],
                "unresponsive_engines": [],
            }))
            .into_response(),
            "empty" => axum::Json(serde_json::json!({
                "results": [],
                "unresponsive_engines": [],
            }))
            .into_response(),
            "forbidden" => (StatusCode::FORBIDDEN, "Forbidden").into_response(),
            "html" => (
                [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
                "<html><body>search is disabled for this format</body></html>",
            )
                .into_response(),
            "noresults" => axum::Json(serde_json::json!({"query": "x"})).into_response(),
            "notarray" => {
                axum::Json(serde_json::json!({"results": "not-an-array"})).into_response()
            }
            // > the 4 MiB bound: must be refused mid-stream, never buffered.
            "huge" => axum::Json(serde_json::json!({
                "pad": "x".repeat(5 * 1024 * 1024),
            }))
            .into_response(),
            _ => StatusCode::NOT_FOUND.into_response(),
        }
    }

    let log: FixtureLog = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let app = axum::Router::new()
        .route("/{variant}/search", get(search))
        .with_state(log.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), log)
}

// ---------------------------------------------------------------------------
// Scripted recording LLM (injected separately from the search provider).
// ---------------------------------------------------------------------------

struct RecordingLlm {
    turns: tokio::sync::Mutex<std::collections::VecDeque<vortex_llm::MockTurn>>,
    requests: tokio::sync::Mutex<Vec<CompletionRequest>>,
}

impl RecordingLlm {
    fn new(turns: Vec<vortex_llm::MockTurn>) -> Arc<Self> {
        Arc::new(Self {
            turns: tokio::sync::Mutex::new(turns.into()),
            requests: tokio::sync::Mutex::new(Vec::new()),
        })
    }

    async fn requests(&self) -> Vec<CompletionRequest> {
        self.requests.lock().await.clone()
    }
}

#[async_trait]
impl LlmClient for RecordingLlm {
    async fn complete(
        &self,
        req: &CompletionRequest,
        events: mpsc::Sender<LlmEvent>,
    ) -> Result<CompletionResponse, vortex_llm::LlmError> {
        self.requests.lock().await.push(req.clone());
        let turn = {
            let mut turns = self.turns.lock().await;
            turns
                .pop_front()
                .unwrap_or_else(|| vortex_llm::MockTurn::text("done"))
        };
        let content = turn.content;
        for word in content.split_inclusive(' ') {
            if events
                .send(LlmEvent::TextDelta(word.to_string()))
                .await
                .is_err()
            {
                break;
            }
        }
        let has_tool_calls = !turn.tool_calls.is_empty();
        Ok(CompletionResponse {
            content,
            tool_calls: turn.tool_calls,
            usage: Some(vortex_types::Usage {
                prompt_tokens: 10,
                completion_tokens: 5,
                total_tokens: 15,
            }),
            finish_reason: if has_tool_calls { "tool_calls" } else { "stop" }.into(),
            reported_completion_tokens: Some(5),
            annotations: None,
            model: Some("scripted/served".into()),
        })
    }

    async fn models(&self) -> Result<Vec<vortex_types::ModelInfo>, vortex_llm::LlmError> {
        Ok(Vec::new())
    }

    fn describe(&self) -> &'static str {
        "recording-scripted"
    }
}

// ---------------------------------------------------------------------------
// Full guarded app on an ephemeral loopback port.
// ---------------------------------------------------------------------------

async fn search_app(
    llm: Arc<RecordingLlm>,
    is_mock_llm: bool,
    settings: Settings,
) -> (String, Arc<AppState>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let db = vortex_core::Db::open(dir.path().join("test.db")).unwrap();
    let attachments = Arc::new(
        vortex_core::AttachmentStore::new(dir.path().join("attachments"), db.clone()).unwrap(),
    );
    let hub = Arc::new(vortex_agents::Hub::new());
    let scheduler = Arc::new(vortex_agents::Scheduler::new(
        settings.agents.max_concurrent,
    ));
    let browser = Arc::new(vortex_agents::browser::BrowserManager::new(
        dir.path().join("profile"),
        true,
    ));
    let previews = Arc::new(vortex_agents::PreviewRegistry::new());
    let approvals = Arc::new(vortex_agents::approvals::ApprovalManager::new(
        db.clone(),
        hub.clone(),
        settings.agents.approval_timeout_secs,
    ));
    let engine_slot = Arc::new(std::sync::OnceLock::new());
    let registry = vortex_agents::build_registry(previews.clone(), engine_slot.clone());
    let http = vortex_agents::safe_http::SafeHttp::new(false);
    // Same selection policy as production startup.
    let search = crate::state::build_search_provider(&settings, is_mock_llm);
    let engine = vortex_agents::Engine::new(
        llm,
        registry,
        hub.clone(),
        scheduler,
        approvals,
        db.clone(),
        browser,
        previews.clone(),
        search,
        http,
        settings.clone(),
        Some(attachments.clone()),
    );
    let _ = engine_slot.set(engine.clone());
    let mut config = vortex_core::AppConfig::default();
    if is_mock_llm {
        config.llm.provider = "mock".into();
    } else {
        // Stub key so the honest chat preflight passes; the scripted LLM
        // never touches OpenRouter.
        config.llm.openrouter_api_key = Some("test-key-not-real".into());
    }
    let supervisor = vortex_agents::supervisor::Supervisor::new(
        db.clone(),
        engine.clone(),
        hub.clone(),
        Arc::new(vortex_agents::clock::SystemClock),
        2,
        tokio_util::sync::CancellationToken::new(),
    );
    let state = Arc::new(AppState {
        db,
        engine,
        hub,
        config,
        previews,
        attachments,
        is_mock_llm,
        version: "test",
        guard: crate::guard::GuardSettings {
            lan_enabled: false,
            lan_token: Arc::from(""),
        },
        diagnostics: crate::diagnostics::DiagnosticsState::new(false),
        supervisor,
    });
    let app = crate::build_router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), state, dir)
}

fn scripted_settings() -> Settings {
    Settings {
        model: "scripted-model".into(),
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// HTTP helpers
// ---------------------------------------------------------------------------

async fn probe(base: &str) -> SearchTestResult {
    let resp = reqwest::Client::new()
        .post(format!("{base}/api/settings/search/test"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    resp.json().await.unwrap()
}

async fn put_settings(base: &str, settings: &Settings) -> Settings {
    let resp = reqwest::Client::new()
        .put(format!("{base}/api/settings"))
        .json(settings)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    resp.json().await.unwrap()
}

async fn set_url(base: &str, state: &AppState, url: &str) {
    let mut s = state.engine.current_settings().await;
    s.search.base_url = url.to_string();
    put_settings(base, &s).await;
}

async fn health(base: &str) -> serde_json::Value {
    reqwest::get(format!("{base}/api/health"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

async fn create_conv(base: &str, mode: Mode) -> String {
    let resp = reqwest::Client::new()
        .post(format!("{base}/api/conversations"))
        .json(&serde_json::json!({ "title": "probe test", "mode": mode }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v: serde_json::Value = resp.json().await.unwrap();
    v["id"].as_str().unwrap().to_string()
}

async fn chat(base: &str, conv: &str, content: &str) -> String {
    let resp = reqwest::Client::new()
        .post(format!("{base}/api/conversations/{conv}/chat"))
        .json(&serde_json::json!({ "content": content }))
        .send()
        .await
        .unwrap();
    let status = resp.status();
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(status, StatusCode::OK, "chat rejected: {body}");
    body["run_id"].as_str().unwrap().to_string()
}

async fn wait_run(state: &AppState, run_id: &str) -> RunInfo {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        if let Ok(Some(info)) = state.db.get_run(run_id.to_string()).await {
            if info.status.is_terminal() {
                return info;
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "run {run_id} never reached a terminal status"
        );
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}

fn tool_turn(id: &str, query: &str) -> vortex_llm::MockTurn {
    vortex_llm::MockTurn::tool_call(
        "web_search",
        id,
        serde_json::json!({ "query": query }).to_string(),
    )
}

fn tool_message(req: &CompletionRequest) -> &vortex_types::ChatMessage {
    req.messages
        .iter()
        .find(|m| m.role == Role::Tool)
        .expect("second request must carry the tool result")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// The full saved-provider lifecycle: unconfigured → save URL (probe ok) →
/// Research run whose `web_search` results reach the model loop → change URL
/// → clear URL, each applying to the NEXT run without a restart.
#[tokio::test]
async fn saved_url_probe_and_research_run_share_one_provider_path() {
    let (fixture, log) = fixture_searxng().await;
    let llm = RecordingLlm::new(vec![
        tool_turn("c1", "alpha"),
        vortex_llm::MockTurn::text("answer one"),
        tool_turn("c2", "beta"),
        vortex_llm::MockTurn::text("answer two"),
        tool_turn("c3", "gamma"),
        vortex_llm::MockTurn::text("answer three"),
    ]);
    let settings = scripted_settings();
    assert!(!settings.search.is_configured());
    let (base, state, _dir) = search_app(llm.clone(), false, settings).await;

    // 1) Unconfigured: honest probe + capabilities flag, no network I/O.
    let r = probe(&base).await;
    assert_eq!(r.status, SearchTestStatus::NotConfigured);
    assert_eq!(r.provider, "");
    assert!(!r.mock);
    assert!(r.detail.unwrap().contains("Settings → Search"));
    assert_eq!(health(&base).await["search"], false);

    // 2) Save the URL: probe connects without a restart.
    set_url(&base, &state, &format!("{fixture}/ok")).await;
    let r = probe(&base).await;
    assert_eq!(r.status, SearchTestStatus::Ok);
    assert_eq!(r.provider, "searxng");
    assert!(!r.mock);
    assert_eq!(r.result_count, 2);
    assert!(r.error_kind.is_none() && r.http_status.is_none());
    assert!(r.detail.is_none());
    assert!(r.tested_at > 1_700_000_000);
    // The probe used the fixed benign server-side query with format=json.
    let entries = log.lock().await.clone();
    assert!(
        entries
            .iter()
            .any(|(v, q, f)| v == "ok" && q == vortex_agents::search::PROBE_QUERY && f == "json"),
        "probe must hit the fixture with the fixed query: {entries:?}"
    );
    // Saved configuration state is refreshed for the UI.
    let caps = health(&base).await;
    assert_eq!(caps["search"], true);
    assert_eq!(caps["search_provider"], "searxng");

    // 3) Research run: fixture results reach the model loop.
    let conv = create_conv(&base, Mode::Research).await;
    let run_id = chat(&base, &conv, "find public sources about alpha").await;
    let info = wait_run(&state, &run_id).await;
    assert_eq!(info.status, RunStatus::Completed);
    let calls = state.db.list_tool_calls(run_id.clone()).await.unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].tool, "web_search");
    assert!(calls[0].ok, "web_search must succeed: {}", calls[0].summary);
    let reqs = llm.requests().await;
    assert_eq!(reqs.len(), 2);
    // Research tool policy exposes web_search; the system prompt carries the
    // step-2 instruction (search when evidence is needed, report honestly).
    assert!(reqs[0].tools.iter().any(|t| t.name == "web_search"));
    let system = &reqs[0].messages[0].content;
    assert!(system.contains("MODE: Research"));
    assert!(system.contains("If you did not search"));
    let msg = tool_message(&reqs[1]);
    assert!(
        msg.content.contains("https://fixture-a.example/one"),
        "fixture result URL must reach the model loop: {}",
        msg.content
    );
    assert!(msg.content.contains("marker-fixture-a"));

    // 4) Change the URL: the NEXT probe and run see the new instance with no
    //    restart (empty-results fixture).
    set_url(&base, &state, &format!("{fixture}/empty")).await;
    let r = probe(&base).await;
    assert_eq!(r.status, SearchTestStatus::Empty);
    assert_eq!(r.result_count, 0);
    assert!(r.detail.unwrap().contains("no results"));
    let run_id = chat(&base, &conv, "find public sources about beta").await;
    let info = wait_run(&state, &run_id).await;
    assert_eq!(info.status, RunStatus::Completed);
    let reqs = llm.requests().await;
    assert_eq!(tool_message(&reqs[3]).content, "no results found");

    // 5) Clear the URL: probe reports unconfigured and the next run's tool
    //    call fails honestly with the setup hint (never a silent success).
    set_url(&base, &state, "").await;
    assert_eq!(probe(&base).await.status, SearchTestStatus::NotConfigured);
    assert_eq!(health(&base).await["search"], false);
    let run_id = chat(&base, &conv, "find public sources about gamma").await;
    let info = wait_run(&state, &run_id).await;
    assert_eq!(info.status, RunStatus::Completed);
    let calls = state.db.list_tool_calls(run_id).await.unwrap();
    assert_eq!(calls.len(), 1);
    assert!(!calls[0].ok);
    let reqs = llm.requests().await;
    let msg = tool_message(&reqs[5]);
    assert!(
        msg.content.contains("no search provider is configured"),
        "cleared URL must surface the setup hint to the model: {}",
        msg.content
    );
}

/// Probe failure classification: 403, non-JSON, missing/non-array `results`,
/// oversized body, unreachable instance — all sanitized and actionable, plus
/// the disabled state and re-enabling without a restart.
#[tokio::test]
async fn probe_classifies_failures_with_sanitized_actionable_details() {
    let (fixture, _log) = fixture_searxng().await;
    let llm = RecordingLlm::new(vec![]);
    let mut settings = scripted_settings();
    settings.search.base_url = format!("{fixture}/forbidden");
    let (base, state, _dir) = search_app(llm, false, settings).await;

    // HTTP 403 carries the SearXNG JSON-disabled hint.
    let r = probe(&base).await;
    assert_eq!(r.status, SearchTestStatus::Error);
    assert_eq!(r.error_kind, Some(SearchTestErrorKind::HttpStatus));
    assert_eq!(r.http_status, Some(403));
    let d = r.detail.unwrap();
    assert!(d.contains("HTTP 403"), "{d}");
    assert!(d.contains("search.formats"), "{d}");

    // Non-JSON (HTML) body → invalid response, hint included.
    set_url(&base, &state, &format!("{fixture}/html")).await;
    let r = probe(&base).await;
    assert_eq!(r.status, SearchTestStatus::Error);
    assert_eq!(r.error_kind, Some(SearchTestErrorKind::InvalidResponse));
    assert!(r.http_status.is_none());
    assert!(r.detail.unwrap().contains("search.formats"));

    // JSON without a usable `results` array is an invalid response — never
    // "connected with no results".
    set_url(&base, &state, &format!("{fixture}/noresults")).await;
    let r = probe(&base).await;
    assert_eq!(r.status, SearchTestStatus::Error);
    assert_eq!(r.error_kind, Some(SearchTestErrorKind::InvalidResponse));
    set_url(&base, &state, &format!("{fixture}/notarray")).await;
    let r = probe(&base).await;
    assert_eq!(r.status, SearchTestStatus::Error);
    assert_eq!(r.error_kind, Some(SearchTestErrorKind::InvalidResponse));

    // Oversized body hits the 4 MiB bound instead of buffering.
    set_url(&base, &state, &format!("{fixture}/huge")).await;
    let r = probe(&base).await;
    assert_eq!(r.status, SearchTestStatus::Error);
    assert_eq!(r.error_kind, Some(SearchTestErrorKind::Provider));
    assert!(r.detail.unwrap().contains("4 MiB"));

    // Unreachable instance → connection category with sanitized detail.
    let dead_port = {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        l.local_addr().unwrap().port()
    };
    set_url(&base, &state, &format!("http://127.0.0.1:{dead_port}")).await;
    let r = probe(&base).await;
    assert_eq!(r.status, SearchTestStatus::Error);
    assert_eq!(r.error_kind, Some(SearchTestErrorKind::Connection));
    let d = r.detail.unwrap();
    assert!(d.contains("could not connect"), "{d}");
    assert!(
        !d.contains("127.0.0.1") && !d.contains(&dead_port.to_string()),
        "detail must not leak the configured URL: {d}"
    );
    assert!(!d.to_lowercase().contains("os error"), "{d}");

    // Disabled wins over the configured URL; re-enabling applies at once.
    let mut s = state.engine.current_settings().await;
    s.search.base_url = format!("{fixture}/ok");
    s.tools.search = false;
    put_settings(&base, &s).await;
    let r = probe(&base).await;
    assert_eq!(r.status, SearchTestStatus::Disabled);
    assert_eq!(r.provider, "searxng");
    assert!(r.detail.unwrap().contains("Settings → Tools"));
    s.tools.search = true;
    put_settings(&base, &s).await;
    let r = probe(&base).await;
    assert_eq!(r.status, SearchTestStatus::Ok);
    assert_eq!(r.result_count, 2);
}

/// Mock-LLM startup mode automatically selects mock search — clearly labeled
/// as mock — even with no saved URL, and mock results still flow through the
/// real `web_search` tool into the model loop.
#[tokio::test]
async fn mock_mode_selects_labeled_mock_search() {
    let llm = RecordingLlm::new(vec![
        tool_turn("c1", "anything"),
        vortex_llm::MockTurn::text("mock answer"),
    ]);
    let settings = scripted_settings();
    assert!(!settings.search.is_configured());
    let (base, state, _dir) = search_app(llm.clone(), true, settings).await;

    let r = probe(&base).await;
    assert_eq!(r.status, SearchTestStatus::Ok);
    assert!(r.mock, "mock results must be labeled as mock");
    assert_eq!(r.provider, "mock");
    assert_eq!(r.result_count, vortex_agents::search::PROBE_LIMIT);

    let conv = create_conv(&base, Mode::Research).await;
    let run_id = chat(&base, &conv, "research something").await;
    let info = wait_run(&state, &run_id).await;
    assert_eq!(info.status, RunStatus::Completed);
    let reqs = llm.requests().await;
    let msg = tool_message(&reqs[1]);
    assert!(msg.content.contains("https://example.com/results/1"));
    assert!(msg.content.contains("mock data"));
}

/// Plain Chat never gains search: no search tool schemas are sent (only the
/// always-present meta tools), a model-attempted `web_search` is refused by
/// the registry, and the fixture is never queried.
#[tokio::test]
async fn chat_mode_never_gains_search() {
    let (fixture, log) = fixture_searxng().await;
    let llm = RecordingLlm::new(vec![
        tool_turn("c1", "sneaky"),
        vortex_llm::MockTurn::text("chat answer"),
    ]);
    let mut settings = scripted_settings();
    settings.search.base_url = format!("{fixture}/ok");
    let (base, state, _dir) = search_app(llm.clone(), false, settings).await;

    let conv = create_conv(&base, Mode::Chat).await;
    let run_id = chat(&base, &conv, "hello there").await;
    let info = wait_run(&state, &run_id).await;
    assert_eq!(info.status, RunStatus::Completed);

    let reqs = llm.requests().await;
    // Chat exposes only the always-present meta tools — never search tools.
    let names: Vec<&str> = reqs[0].tools.iter().map(|t| t.name.as_str()).collect();
    assert!(
        names
            .iter()
            .all(|n| matches!(*n, "update_plan" | "request_approval")),
        "Chat must not expose search or other work tools: {names:?}"
    );
    assert!(reqs[0].messages[0].content.contains("MODE: Chat"));
    let calls = state.db.list_tool_calls(run_id).await.unwrap();
    assert_eq!(calls.len(), 1);
    assert!(!calls[0].ok);
    assert!(
        calls[0].summary.contains("not permitted"),
        "refusal must be honest: {}",
        calls[0].summary
    );
    assert!(tool_message(&reqs[1]).content.contains("not permitted"));
    // The provider was configured but never touched in Chat mode.
    assert!(log.lock().await.is_empty());
}

/// The new POST route sits behind the existing guard: cross-origin writes
/// are refused, loopback requests pass.
#[tokio::test]
async fn probe_route_keeps_the_request_guard() {
    let llm = RecordingLlm::new(vec![]);
    let (base, _state, _dir) = search_app(llm, true, scripted_settings()).await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("{base}/api/settings/search/test"))
        .header("Origin", "http://evil.example")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    let resp = client
        .post(format!("{base}/api/settings/search/test"))
        .header("Origin", base.as_str())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}
