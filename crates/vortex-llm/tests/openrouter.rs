//! Integration tests for the OpenRouter client against a local mock server.
//! No network access and no real credentials are involved.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use vortex_llm::llm::{CompletionRequest, CompletionResponse, LlmClient, LlmEvent, ToolSchema};
use vortex_llm::openrouter::OpenRouterClient;
use vortex_types::ChatMessage;

use axum::extract::State;
use axum::routing::{get, post};
use tokio::sync::mpsc;

struct MockServerState {
    chat_calls: AtomicUsize,
    responses: tokio::sync::Mutex<Vec<MockResponse>>,
}

#[derive(Clone)]
enum MockResponse {
    Sse(String),
    Error(u16, String),
}

async fn chat_handler(
    State(state): axum::extract::State<Arc<MockServerState>>,
    axum::extract::Json(_body): axum::extract::Json<serde_json::Value>,
) -> axum::response::Response {
    state.chat_calls.fetch_add(1, Ordering::SeqCst);
    let resp = state.responses.lock().await.remove(0);
    match resp {
        MockResponse::Sse(body) => axum::http::Response::builder()
            .status(200)
            .header("content-type", "text/event-stream")
            .body(axum::body::Body::from(body))
            .unwrap(),
        MockResponse::Error(code, msg) => axum::http::Response::builder()
            .status(code)
            .header("content-type", "application/json")
            .body(axum::body::Body::from(
                serde_json::json!({ "error": { "message": msg } }).to_string(),
            ))
            .unwrap(),
    }
}

async fn models_handler() -> axum::Json<serde_json::Value> {
    axum::Json(serde_json::json!({
        "data": [
            { "id": "a/model", "name": "A", "context_length": 4096,
              "supported_parameters": ["tools"], "pricing": { "prompt": "1", "completion": "2" } },
            { "id": "b/model", "name": "B", "supported_parameters": [] }
        ]
    }))
}

async fn spawn_server(responses: Vec<MockResponse>) -> (String, Arc<MockServerState>) {
    let state = Arc::new(MockServerState {
        chat_calls: AtomicUsize::new(0),
        responses: tokio::sync::Mutex::new(responses),
    });
    let app = axum::Router::new()
        .route("/v1/chat/completions", post(chat_handler))
        .route("/v1/models", get(models_handler))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}/v1"), state)
}

#[tokio::test]
async fn streams_text_and_parses_models() {
    let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"Hi\"}}]}\n\n\
               data: {\"choices\":[{\"delta\":{\"content\":\" there\"},\"finish_reason\":null}]}\n\n\
               data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":3,\"total_tokens\":10}}\n\n\
               data: [DONE]\n\n";
    let (base, state) = spawn_server(vec![MockResponse::Sse(sse.into())]).await;
    let client = OpenRouterClient::new(&base, Some("test-key".into())).unwrap();

    let (tx, mut rx) = mpsc::channel::<LlmEvent>(64);
    let resp = client.complete(&req(), tx).await.unwrap();
    assert_eq!(resp.content, "Hi there");
    assert_eq!(resp.finish_reason, "stop");
    assert_eq!(resp.usage.unwrap().total_tokens, 10);
    let mut deltas = Vec::new();
    while let Ok(LlmEvent::TextDelta(d)) = rx.try_recv() {
        deltas.push(d);
    }
    assert_eq!(deltas.concat(), "Hi there");

    let models = client.models().await.unwrap();
    assert_eq!(models.len(), 2);
    assert_eq!(models[0].id, "a/model");
    assert_eq!(models[0].supports_tools, Some(true));
    assert_eq!(models[1].supports_tools, Some(false));
    assert_eq!(state.chat_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn accumulates_tool_calls_across_deltas() {
    let sse = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"web_search\",\"arguments\":\"{\\\"que\"}}]}}]}\n\n\
               data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"ry\\\": \\\"cats\\\"}\"}}]}}]}\n\n\
               data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n\
               data: [DONE]\n\n";
    let (base, _) = spawn_server(vec![MockResponse::Sse(sse.into())]).await;
    let client = OpenRouterClient::new(&base, Some("test-key".into())).unwrap();
    let (tx, _rx) = mpsc::channel::<LlmEvent>(64);
    let resp = client.complete(&req(), tx).await.unwrap();
    assert_eq!(resp.tool_calls.len(), 1);
    assert_eq!(resp.tool_calls[0].id, "call_1");
    assert_eq!(resp.tool_calls[0].name, "web_search");
    assert_eq!(resp.tool_calls[0].arguments, r#"{"query": "cats"}"#);
    assert_eq!(resp.finish_reason, "tool_calls");
}

#[tokio::test]
async fn maps_auth_error_without_retry() {
    let (base, state) = spawn_server(vec![MockResponse::Error(401, "bad key".into())]).await;
    let client = OpenRouterClient::new(&base, Some("wrong-key".into())).unwrap();
    let (tx, _rx) = mpsc::channel::<LlmEvent>(64);
    let err = client.complete(&req(), tx).await.unwrap_err();
    assert!(matches!(err, vortex_llm::LlmError::InvalidCredentials));
    assert_eq!(
        state.chat_calls.load(Ordering::SeqCst),
        1,
        "auth errors must not be retried"
    );
}

#[tokio::test]
async fn retries_rate_limit_then_succeeds() {
    let ok_sse = "data: {\"choices\":[{\"delta\":{\"content\":\"done\"}}]}\n\ndata: [DONE]\n\n";
    let (base, state) = spawn_server(vec![
        MockResponse::Error(429, "slow down".into()),
        MockResponse::Sse(ok_sse.into()),
    ])
    .await;
    let client = OpenRouterClient::new(&base, Some("k".into())).unwrap();
    let (tx, _rx) = mpsc::channel::<LlmEvent>(64);
    let resp: CompletionResponse = client.complete(&req(), tx).await.unwrap();
    assert_eq!(resp.content, "done");
    assert_eq!(state.chat_calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn missing_key_is_reported_clearly() {
    let (base, _) = spawn_server(vec![]).await;
    let client = OpenRouterClient::new(&base, None).unwrap();
    let (tx, _rx) = mpsc::channel::<LlmEvent>(64);
    let err = client.complete(&req(), tx).await.unwrap_err();
    assert!(matches!(err, vortex_llm::LlmError::NotConfigured));
    assert!(err.to_string().contains("OPENROUTER_API_KEY"));
}

#[tokio::test]
async fn malformed_stream_is_detected() {
    let (base, _) = spawn_server(vec![MockResponse::Sse("data: {not json}\n\n".into())]).await;
    let client = OpenRouterClient::new(&base, Some("k".into())).unwrap();
    let (tx, _rx) = mpsc::channel::<LlmEvent>(64);
    let err = client.complete(&req(), tx).await.unwrap_err();
    assert!(matches!(err, vortex_llm::LlmError::MalformedStream(_)));
}

#[tokio::test]
async fn stream_end_without_done_marker_still_returns() {
    let (base, _) = spawn_server(vec![MockResponse::Sse(
        "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n".into(),
    )])
    .await;
    let client = OpenRouterClient::new(&base, Some("k".into())).unwrap();
    let (tx, _rx) = mpsc::channel::<LlmEvent>(64);
    let resp = client.complete(&req(), tx).await.unwrap();
    assert_eq!(resp.content, "partial");
}

#[tokio::test]
async fn model_not_found_maps_to_clear_error() {
    let (base, state) = spawn_server(vec![
        MockResponse::Error(404, "model nope".into()),
        MockResponse::Error(404, "model nope".into()),
        MockResponse::Error(404, "model nope".into()),
    ])
    .await;
    let client = OpenRouterClient::new(&base, Some("k".into())).unwrap();
    let (tx, _rx) = mpsc::channel::<LlmEvent>(64);
    let err = client.complete(&req(), tx).await.unwrap_err();
    match err {
        vortex_llm::LlmError::ModelNotFound(m) => assert!(m.contains("nope")),
        other => panic!("unexpected: {other:?}"),
    }
    assert_eq!(state.chat_calls.load(Ordering::SeqCst), 1);
}

fn req() -> CompletionRequest {
    CompletionRequest {
        model: "test/model".into(),
        messages: vec![ChatMessage::user("hello")],
        tools: vec![ToolSchema::function(
            "web_search",
            "search the web",
            serde_json::json!({"type": "object", "properties": {}, "required": []}),
        )],
        temperature: 0.5,
        max_tokens: 128,
    }
}
