//! Integration tests for the OpenRouter client against a local mock server.
//! No network access and no real credentials are involved.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use serde_json::{json, Value};
use vortex_llm::llm::{CompletionRequest, LlmClient, LlmEvent, PdfEngine, ToolSchema};
use vortex_llm::openrouter::OpenRouterClient;
use vortex_types::{ChatMessage, ContentPart, FileContent, ImageUrlContent, VideoUrlContent};

use axum::extract::State;
use axum::routing::{get, post};
use futures::StreamExt;
use tokio::sync::mpsc;

struct MockServerState {
    chat_calls: AtomicUsize,
    responses: tokio::sync::Mutex<Vec<MockResponse>>,
    requests: tokio::sync::Mutex<Vec<Value>>,
}

#[derive(Clone)]
enum MockResponse {
    Sse(String),
    /// Streams the given bytes, then aborts the connection mid-body.
    BrokenStream(String),
    Error(u16, String),
}

async fn chat_handler(
    State(state): axum::extract::State<Arc<MockServerState>>,
    axum::extract::Json(body): axum::extract::Json<serde_json::Value>,
) -> axum::response::Response {
    state.chat_calls.fetch_add(1, Ordering::SeqCst);
    state.requests.lock().await.push(body);
    let resp = state.responses.lock().await.remove(0);
    match resp {
        MockResponse::Sse(body) => axum::http::Response::builder()
            .status(200)
            .header("content-type", "text/event-stream")
            .body(axum::body::Body::from(body))
            .unwrap(),
        MockResponse::BrokenStream(body) => {
            // Deliver the first chunk, give the client time to read it, then
            // abort the connection mid-body (a reset discards undelivered
            // data, so the pause keeps the test deterministic).
            let first = futures::stream::iter(vec![Ok::<axum::body::Bytes, std::io::Error>(
                axum::body::Bytes::from(body),
            )]);
            let abort = futures::stream::once(async {
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                Err(std::io::Error::new(
                    std::io::ErrorKind::ConnectionReset,
                    "connection reset mid-stream",
                ))
            });
            axum::http::Response::builder()
                .status(200)
                .header("content-type", "text/event-stream")
                .body(axum::body::Body::from_stream(first.chain(abort)))
                .unwrap()
        }
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
            { "id": "a/model", "name": "A", "context_length": 4096, "created": 1_750_000_000,
              "supported_parameters": ["tools"],
              "pricing": { "prompt": "1", "completion": "2", "request": "0.004", "image": "0.0002" } },
            { "id": "b/model", "name": "B", "supported_parameters": [] }
        ]
    }))
}

async fn spawn_server(responses: Vec<MockResponse>) -> (String, Arc<MockServerState>) {
    let state = Arc::new(MockServerState {
        chat_calls: AtomicUsize::new(0),
        responses: tokio::sync::Mutex::new(responses),
        requests: tokio::sync::Mutex::new(Vec::new()),
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
    assert!(resp.annotations.is_none());
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
    let requests = state.requests.lock().await;
    assert_eq!(
        requests[0]["messages"],
        json!([{"role":"user","content":"hello"}])
    );
    assert!(requests[0].get("plugins").is_none());
}

/// Catalog parsing exposes the provider creation date and optional fee
/// details with explicit units (USD strings, as reported); unreported fields
/// stay unknown and are never normalized to zero.
#[tokio::test]
async fn parses_model_created_and_fee_details() {
    let (base, _) = spawn_server(Vec::new()).await;
    let client = OpenRouterClient::new(&base, Some("test-key".into())).unwrap();
    let models = client.models().await.unwrap();
    assert_eq!(models[0].created, Some(1_750_000_000));
    assert_eq!(models[0].pricing_prompt.as_deref(), Some("1"));
    assert_eq!(models[0].pricing_completion.as_deref(), Some("2"));
    assert_eq!(models[0].pricing_request.as_deref(), Some("0.004"));
    assert_eq!(models[0].pricing_image.as_deref(), Some("0.0002"));
    // b/model reports nothing optional: every field stays unknown.
    assert_eq!(models[1].created, None);
    assert!(models[1].pricing_prompt.is_none());
    assert!(models[1].pricing_completion.is_none());
    assert!(models[1].pricing_request.is_none());
    assert!(models[1].pricing_image.is_none());
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
async fn rate_limit_is_not_retried_automatically() {
    let ok_sse = "data: {\"choices\":[{\"delta\":{\"content\":\"done\"}}]}\n\ndata: [DONE]\n\n";
    let (base, state) = spawn_server(vec![
        MockResponse::Error(429, "slow down".into()),
        MockResponse::Sse(ok_sse.into()),
    ])
    .await;
    let client = OpenRouterClient::new(&base, Some("k".into())).unwrap();
    let (tx, _rx) = mpsc::channel::<LlmEvent>(64);
    let err = client.complete(&req(), tx).await.unwrap_err();
    assert!(matches!(err, vortex_llm::LlmError::RateLimited));
    assert_eq!(state.chat_calls.load(Ordering::SeqCst), 1);
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

/// C3 regression: a retryable failure *after* deltas were delivered must not
/// retry (which used to re-stream the whole answer into the same channel).
#[tokio::test]
async fn inline_provider_error_is_not_a_normal_finish() {
    let sse = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n",
        "data: {\"error\":{\"code\":502},\"choices\":[{\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n",
    );
    let (base, state) = spawn_server(vec![MockResponse::Sse(sse.into())]).await;
    let client = OpenRouterClient::new(&base, Some("k".into())).unwrap();
    let (tx, _rx) = mpsc::channel::<LlmEvent>(8);
    let resp = client.complete(&req(), tx).await.unwrap();
    assert_eq!(resp.content, "partial");
    assert_eq!(resp.finish_reason, "error");
    assert_eq!(state.chat_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn no_retry_after_partial_stream_delivery() {
    let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n";
    let (base, state) = spawn_server(vec![
        MockResponse::BrokenStream(sse.into()),
        MockResponse::Sse(
            "data: {\"choices\":[{\"delta\":{\"content\":\"AGAIN\"}}]}\n\ndata: [DONE]\n\n".into(),
        ),
    ])
    .await;
    let client = OpenRouterClient::new(&base, Some("k".into())).unwrap();
    let (tx, mut rx) = mpsc::channel::<LlmEvent>(64);
    let err = client.complete(&req(), tx).await.unwrap_err();
    assert!(
        matches!(err, vortex_llm::LlmError::Network(_)),
        "unexpected: {err:?}"
    );
    assert_eq!(
        state.chat_calls.load(Ordering::SeqCst),
        1,
        "must not retry once deltas were streamed to the consumer"
    );
    let mut deltas = Vec::new();
    while let Ok(LlmEvent::TextDelta(d)) = rx.try_recv() {
        deltas.push(d);
    }
    assert_eq!(
        deltas.concat(),
        "partial",
        "consumer must see the delivered text exactly once"
    );
}

/// C13 regression: deterministic 4xx (not 429) must fail fast without retries.
#[tokio::test]
async fn bad_request_is_not_retried() {
    let (base, state) = spawn_server(vec![
        MockResponse::Error(400, "bad request".into()),
        MockResponse::Error(400, "bad request".into()),
        MockResponse::Error(400, "bad request".into()),
    ])
    .await;
    let client = OpenRouterClient::new(&base, Some("k".into())).unwrap();
    let (tx, _rx) = mpsc::channel::<LlmEvent>(64);
    let err = client.complete(&req(), tx).await.unwrap_err();
    match err {
        vortex_llm::LlmError::Provider(400, m) => assert!(m.contains("bad request")),
        other => panic!("unexpected: {other:?}"),
    }
    assert_eq!(state.chat_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn server_error_is_not_retried_automatically() {
    let ok_sse = "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\ndata: [DONE]\n\n";
    let (base, state) = spawn_server(vec![
        MockResponse::Error(503, "unavailable".into()),
        MockResponse::Sse(ok_sse.into()),
    ])
    .await;
    let client = OpenRouterClient::new(&base, Some("k".into())).unwrap();
    let (tx, _rx) = mpsc::channel::<LlmEvent>(64);
    let err = client.complete(&req(), tx).await.unwrap_err();
    assert!(matches!(err, vortex_llm::LlmError::Provider(503, _)));
    assert_eq!(state.chat_calls.load(Ordering::SeqCst), 1);
}

/// C24 regression: a spec-compliant provider may split one JSON payload across
/// multiple `data:` lines; they must be joined, not rejected as malformed.
#[tokio::test]
async fn multiline_data_events_are_joined() {
    let sse = "data: {\"choices\":\ndata: [{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n\n";
    let (base, _) = spawn_server(vec![MockResponse::Sse(sse.into())]).await;
    let client = OpenRouterClient::new(&base, Some("k".into())).unwrap();
    let (tx, _rx) = mpsc::channel::<LlmEvent>(64);
    let resp = client.complete(&req(), tx).await.unwrap();
    assert_eq!(resp.content, "hi");
}

/// P3 regression: a hostile provider-supplied tool-call index must not be
/// able to grow the slot vector without bound.
#[tokio::test]
async fn out_of_range_tool_call_index_is_ignored() {
    let sse = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":4294967295,\"id\":\"x\",\"function\":{\"name\":\"n\",\"arguments\":\"{}\"}}]}}]}\n\n\
               data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n\
               data: [DONE]\n\n";
    let (base, _) = spawn_server(vec![MockResponse::Sse(sse.into())]).await;
    let client = OpenRouterClient::new(&base, Some("k".into())).unwrap();
    let (tx, _rx) = mpsc::channel::<LlmEvent>(64);
    let resp = client.complete(&req(), tx).await.unwrap();
    assert!(
        resp.tool_calls.is_empty(),
        "out-of-range index must be ignored, got {:?}",
        resp.tool_calls.len()
    );
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
        pdf_engine: None,
        assistant_annotations: Default::default(),
    }
}

fn pdf_req() -> CompletionRequest {
    let mut req = req();
    req.messages[0].parts = Some(vec![
        ContentPart::Text {
            text: "hello".into(),
        },
        ContentPart::ImageUrl {
            image_url: ImageUrlContent {
                url: "data:image/png;base64,AAAA".into(),
            },
        },
        ContentPart::File {
            file: FileContent {
                filename: "fixture.pdf".into(),
                file_data: "data:application/pdf;base64,JVBERg==".into(),
            },
        },
    ]);
    req
}

fn annotation(name: &str) -> Value {
    json!({"type":"file","file":{"name":name,"hash":"opaque-fixture","content":[{"type":"text","text":"fixture"}]}})
}

#[tokio::test]
async fn mixed_parts_and_complete_streamed_annotations_round_trip() {
    let first = annotation("fixture.pdf");
    let tail = annotation("tail.pdf");
    let frame = json!({"choices":[{"delta":{"content":"hi","annotations":[first, {"type":"file","file":{"name":"incomplete.pdf"}}]}}]});
    let final_frame = json!({"choices":[{"message":{"annotations":[first,tail]},"delta":{"content":"!"},"finish_reason":"stop"}]});
    let (base, state) = spawn_server(vec![
        MockResponse::Sse(format!("data: {frame}\n\ndata: {final_frame}")),
        MockResponse::Sse("data: [DONE]\n\n".into()),
    ])
    .await;
    let llm = OpenRouterClient::new(base, Some("key".into())).unwrap();
    let mut req = pdf_req();
    let (tx, mut rx) = mpsc::channel(8);
    let resp = llm.complete(&req, tx).await.unwrap();
    assert_eq!(resp.content, "hi!");
    assert!(matches!(rx.try_recv(), Ok(LlmEvent::TextDelta(t)) if t == "hi"));
    assert!(rx.try_recv().is_err()); // Legacy text tail is final-response-only.
    assert_eq!(resp.annotations.as_ref().unwrap(), &[first.clone(), tail]);
    req.pdf_engine = Some(PdfEngine::MistralOcr);
    req.messages.push(ChatMessage::assistant(resp.content));
    req.assistant_annotations.insert(1, vec![first.clone()]);
    req.assistant_annotations.insert(0, vec![first.clone()]);
    let (tx, _rx) = mpsc::channel(8);
    assert!(llm.complete(&req, tx).await.unwrap().annotations.is_none());
    let bodies = state.requests.lock().await;
    assert_eq!(
        bodies[0]["messages"][0]["content"],
        json!(req.messages[0].parts)
    );
    assert_eq!(
        bodies[0]["plugins"],
        json!([{"id":"file-parser","pdf":{"engine":"cloudflare-ai"}}])
    );
    assert_eq!(bodies[1]["plugins"][0]["pdf"]["engine"], "mistral-ocr");
    assert_eq!(bodies[1]["messages"][1]["annotations"], json!([first]));
    assert!(bodies[1]["messages"][0].get("annotations").is_none());
}

#[tokio::test]
async fn attachment_failures_redact_http_and_sse_bodies_without_parser_fallback() {
    let (base, state) = spawn_server(vec![
        MockResponse::Error(422, "private data:application/pdf;base64,AAAA".into()),
        MockResponse::Sse("data: {\"error\":{\"code\":400,\"message\":\"private data\"}}".into()),
    ])
    .await;
    let llm = OpenRouterClient::new(base, Some("key".into())).unwrap();
    for _ in 0..2 {
        let (tx, _rx) = mpsc::channel(8);
        let err = llm.complete(&pdf_req(), tx).await.unwrap_err().to_string();
        assert!(err.contains("PDF parser"));
        assert!(!err.contains("private"));
    }
    assert_eq!(state.chat_calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn annotation_capture_overflow_does_not_interrupt_the_answer() {
    let annotations: Vec<_> = (0..33).map(|n| annotation(&format!("{n}.pdf"))).collect();
    let frame = json!({"choices":[{"delta":{"content":"answer","annotations":annotations}}]});
    let (base, _) = spawn_server(vec![MockResponse::Sse(format!(
        "data: {frame}\n\ndata: [DONE]\n\n"
    ))])
    .await;
    let llm = OpenRouterClient::new(base, Some("key".into())).unwrap();
    let (tx, _rx) = mpsc::channel(8);
    let resp = llm.complete(&pdf_req(), tx).await.unwrap();
    assert_eq!(resp.content, "answer");
    assert!(resp.annotations.is_none());
}

#[tokio::test]
async fn video_on_auto_router_keeps_tool_wire_fields_and_omits_the_pdf_parser() {
    let (base, state) = spawn_server(vec![MockResponse::Sse("data: [DONE]\n\n".into())]).await;
    let llm = OpenRouterClient::new(base, Some("key".into())).unwrap();
    let mut req = pdf_req();
    req.model = "openrouter/auto".into();
    req.pdf_engine = Some(PdfEngine::MistralOcr);
    let parts = req.messages[0].parts.as_mut().unwrap();
    parts.pop();
    parts.push(ContentPart::VideoUrl {
        video_url: VideoUrlContent {
            url: "data:video/mp4;base64,AAAA".into(),
        },
    });
    let mut assistant = ChatMessage::assistant("");
    assistant.tool_calls.push(vortex_types::ToolCall {
        id: "fixture-call".into(),
        name: "fixture-tool".into(),
        arguments: "{}".into(),
    });
    req.messages.push(assistant);
    req.messages.push(ChatMessage::tool_result(
        "fixture-call",
        "fixture-tool",
        "fixture result",
    ));
    let (tx, _rx) = mpsc::channel(8);
    llm.complete(&req, tx).await.unwrap();
    let requests = state.requests.lock().await;
    let wire = &requests[0];
    assert!(wire.get("plugins").is_none());
    assert_eq!(wire["model"], "openrouter/auto");
    let parts = wire["messages"][0]["content"].as_array().unwrap();
    assert_eq!(parts.len(), 3);
    assert_eq!(
        parts[2],
        json!({"type":"video_url","video_url":{"url":"data:video/mp4;base64,AAAA"}})
    );
    assert_eq!(
        wire["messages"][1],
        json!({
            "role":"assistant", "content":"", "tool_calls":[{
                "type":"function", "id":"fixture-call", "function":{"name":"fixture-tool", "arguments":"{}"}
            }]
        })
    );
    assert_eq!(
        wire["messages"][2],
        json!({
            "role":"tool", "tool_call_id":"fixture-call", "content":"fixture result"
        })
    );
}

/// Plan step 5: the provider-reported model from completion chunks is
/// captured on the response and may legitimately differ from the requested
/// model (router aliases resolve server-side).
#[tokio::test]
async fn captures_provider_reported_serving_model() {
    let sse = "data: {\"model\":\"provider/actually-served\",\"choices\":[{\"delta\":{\"content\":\"Hi\"}}]}\n\n\
               data: {\"model\":\"provider/actually-served\",\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":3,\"total_tokens\":10}}\n\n\
               data: [DONE]\n\n";
    let (base, _) = spawn_server(vec![MockResponse::Sse(sse.into())]).await;
    let client = OpenRouterClient::new(&base, Some("test-key".into())).unwrap();
    let (tx, _rx) = mpsc::channel::<LlmEvent>(64);
    // The request asks for "test/model" (see `req()`); the provider reports
    // a different serving model and the response must carry the reported one.
    let resp = client.complete(&req(), tx).await.unwrap();
    assert_eq!(resp.model.as_deref(), Some("provider/actually-served"));
    assert_eq!(resp.content, "Hi");
    assert_eq!(resp.reported_completion_tokens, Some(3));
}

/// A stream that never reports a model stays honestly `None` — the serving
/// model is never inferred from the request. Empty strings are ignored, the
/// first non-empty report wins (a late chunk must not rewrite provenance),
/// and oversized model strings are capped.
#[tokio::test]
async fn serving_model_is_never_inferred_first_wins_and_capped() {
    // Never reported → None.
    let sse =
        "data: {\"choices\":[{\"delta\":{\"content\":\"x\"},\"finish_reason\":\"stop\"}]}\n\n\
               data: [DONE]\n\n";
    let (base, _) = spawn_server(vec![MockResponse::Sse(sse.into())]).await;
    let client = OpenRouterClient::new(&base, Some("k".into())).unwrap();
    let (tx, _rx) = mpsc::channel::<LlmEvent>(64);
    let resp = client.complete(&req(), tx).await.unwrap();
    assert!(
        resp.model.is_none(),
        "missing provider model must stay None, got {:?}",
        resp.model
    );

    // Empty then real then a hostile late rewrite → first non-empty wins.
    let sse = "data: {\"model\":\"\",\"choices\":[{\"delta\":{\"content\":\"a\"}}]}\n\n\
               data: {\"model\":\"first/real\",\"choices\":[{\"delta\":{\"content\":\"b\"}}]}\n\n\
               data: {\"model\":\"late/rewrite\",\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
               data: [DONE]\n\n";
    let (base, _) = spawn_server(vec![MockResponse::Sse(sse.into())]).await;
    let client = OpenRouterClient::new(&base, Some("k".into())).unwrap();
    let (tx, _rx) = mpsc::channel::<LlmEvent>(64);
    let resp = client.complete(&req(), tx).await.unwrap();
    assert_eq!(resp.content, "ab");
    assert_eq!(resp.model.as_deref(), Some("first/real"));

    // Oversized provider model string is capped, not persisted unbounded.
    let long = "m".repeat(600);
    let sse = format!(
        "data: {{\"model\":\"{long}\",\"choices\":[{{\"delta\":{{\"content\":\"c\"}},\"finish_reason\":\"stop\"}}]}}\n\n\
         data: [DONE]\n\n"
    );
    let (base, _) = spawn_server(vec![MockResponse::Sse(sse)]).await;
    let client = OpenRouterClient::new(&base, Some("k".into())).unwrap();
    let (tx, _rx) = mpsc::channel::<LlmEvent>(64);
    let resp = client.complete(&req(), tx).await.unwrap();
    assert_eq!(resp.model.as_ref().map(|m| m.chars().count()), Some(256));
}

/// The serving model survives streams that end without a `[DONE]` marker
/// (the flushed-tail path returns the same accumulated response).
#[tokio::test]
async fn serving_model_survives_stream_end_without_done() {
    let sse = "data: {\"model\":\"provider/tail-served\",\"choices\":[{\"delta\":{\"content\":\"partial\"}}],\"usage\":{\"prompt_tokens\":7}}\n\n";
    let (base, _) = spawn_server(vec![MockResponse::Sse(sse.into())]).await;
    let client = OpenRouterClient::new(&base, Some("k".into())).unwrap();
    let (tx, _rx) = mpsc::channel::<LlmEvent>(64);
    let resp = client.complete(&req(), tx).await.unwrap();
    assert_eq!(resp.content, "partial");
    assert_eq!(resp.model.as_deref(), Some("provider/tail-served"));
    assert!(resp.finish_reason.is_empty());
    assert_eq!(resp.reported_completion_tokens, None);
}
