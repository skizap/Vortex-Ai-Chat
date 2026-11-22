//! OpenRouter client (OpenAI-compatible `chat/completions` with streaming).
//!
//! Security notes:
//! - The API key is only ever sent in the `Authorization` header.
//! - Errors and logs never contain the key or raw request headers.
//! - Retries are bounded and never switch models or providers.

use crate::error::LlmError;
use crate::llm::{
    is_file_annotation, json_size, CompletionRequest, CompletionResponse, LlmClient, LlmEvent,
    ToolSchema,
};
use crate::sse::SseParser;
use async_trait::async_trait;
use std::time::Duration;
use tokio::sync::mpsc;
use vortex_core::diag;
use vortex_types::{ContentPart, ModelInfo, Role, ToolCall, Usage};

const USER_AGENT: &str = concat!("vortex-ai-chat/", env!("CARGO_PKG_VERSION"));

/// Upper bound on provider-supplied tool-call `index` values. A hostile or
/// broken endpoint must not be able to grow the slot vector without limit.
const MAX_TOOL_CALL_SLOTS: usize = 64;
const MAX_ANNOTATIONS: usize = 32;
const MAX_ANNOTATION_CAPTURE_BYTES: usize = 4 * 1024 * 1024;
/// Cap on the provider-reported serving-model string that is captured from
/// completion chunks and persisted with assistant messages.
const MAX_SERVING_MODEL_CHARS: usize = 256;

#[derive(Clone, Copy)]
enum MediaRequest {
    None,
    Attachments,
    Pdf,
}

impl MediaRequest {
    fn error(self, code: u16) -> LlmError {
        let detail = match (code, self) {
            (404, _) => return LlmError::ModelNotFound("check the model ID in Settings".into()),
            (401 | 403 | 402 | 429, _) => return OpenRouterClient::map_status(
                reqwest::StatusCode::from_u16(code).expect("known status"), String::new()),
            (413, _) => "the attachment request is too large; send fewer or smaller files",
            (400..=499, Self::Pdf) => "the provider rejected the attachment request; check PDF validity, the configured PDF parser and model compatibility",
            (400..=499, _) => "the provider rejected the attachment request; try fewer files or a model that supports the input",
            (_, Self::Pdf) => "the provider could not complete the PDF request; retry later or check the configured PDF parser",
            _ => "the provider could not complete the attachment request; retry later",
        };
        LlmError::Provider(code, detail.into())
    }
}

pub struct OpenRouterClient {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
}

impl OpenRouterClient {
    /// `api_key` must come from the server environment/config. `None` yields
    /// [`LlmError::NotConfigured`] only when a request is attempted, so the
    /// server can still start and report capabilities honestly.
    pub fn new(base_url: impl Into<String>, api_key: Option<String>) -> Result<Self, LlmError> {
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .connect_timeout(Duration::from_secs(10))
            // Stall detection for streaming responses (no total cap: long
            // generations are legitimate; the caller can drop to cancel).
            .read_timeout(Duration::from_secs(120))
            .build()
            .map_err(|e| LlmError::Network(format!("building http client: {e}")))?;
        Ok(Self {
            http,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key: api_key.unwrap_or_default(),
        })
    }

    fn key_or_fail(&self) -> Result<&str, LlmError> {
        if self.api_key.is_empty() {
            Err(LlmError::NotConfigured)
        } else {
            Ok(&self.api_key)
        }
    }

    fn map_status(status: reqwest::StatusCode, body: String) -> LlmError {
        let detail = extract_error_message(&body)
            .unwrap_or_else(|| status.canonical_reason().unwrap_or("").to_string());
        match status.as_u16() {
            401 | 403 => LlmError::InvalidCredentials,
            402 => LlmError::InsufficientCredits,
            404 => LlmError::ModelNotFound(detail),
            429 => LlmError::RateLimited,
            422 => LlmError::MalformedStream(format!("rejected request: {detail}")),
            code => LlmError::Provider(code, detail),
        }
    }

    fn serialize_messages(req: &CompletionRequest) -> serde_json::Value {
        let mut out = Vec::with_capacity(req.messages.len());
        for (index, m) in req.messages.iter().enumerate() {
            let content = match &m.parts {
                Some(parts) => serde_json::json!(parts),
                None => serde_json::json!(m.content),
            };
            let mut obj = serde_json::json!({ "role": m.role.as_str() });
            obj["content"] = content;
            if m.role == Role::Assistant {
                if let Some(annotations) = req
                    .assistant_annotations
                    .get(&index)
                    .filter(|a| !a.is_empty())
                {
                    obj["annotations"] = serde_json::json!(annotations);
                }
            }
            match m.role {
                Role::Assistant if !m.tool_calls.is_empty() => {
                    let calls: Vec<serde_json::Value> = m
                        .tool_calls
                        .iter()
                        .map(|tc| {
                            serde_json::json!({
                                "id": tc.id,
                                "type": "function",
                                "function": { "name": tc.name, "arguments": tc.arguments },
                            })
                        })
                        .collect();
                    obj["tool_calls"] = serde_json::json!(calls);
                }
                Role::Tool => {
                    obj["tool_call_id"] =
                        serde_json::json!(m.tool_call_id.clone().unwrap_or_default());
                }
                _ => {}
            }
            out.push(obj);
        }
        serde_json::Value::Array(out)
    }

    fn build_body(&self, req: &CompletionRequest, media: MediaRequest) -> serde_json::Value {
        // Keep openrouter/auto as the model when sending video_url parts:
        // the router filters its candidates by the request's input modalities.
        let mut body = serde_json::json!({
            "model": req.model,
            "stream": true,
            "temperature": req.temperature,
            "max_tokens": req.max_tokens,
        });
        body["messages"] = Self::serialize_messages(req);
        if matches!(media, MediaRequest::Pdf) {
            body["plugins"] = serde_json::json!([{
                "id": "file-parser", "pdf": {"engine": req.pdf_engine.unwrap_or_default().as_str()}
            }]);
        }
        if !req.tools.is_empty() {
            body["tools"] = serde_json::json!(req
                .tools
                .iter()
                .map(|t: &ToolSchema| serde_json::json!({
                    "type": "function",
                    "function": { "name": t.name, "description": t.description, "parameters": t.parameters }
                }))
                .collect::<Vec<_>>());
        }
        body
    }
}

/// Extract `{"error":{"message":"..."}}` style details from provider bodies.
fn extract_error_message(body: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let msg = v
        .get("error")
        .and_then(|e| e.get("message"))
        .and_then(|m| m.as_str())
        .or_else(|| v.get("error").and_then(|e| e.as_str()))
        .or_else(|| v.get("message").and_then(|m| m.as_str()))?;
    Some(msg.chars().take(500).collect())
}
impl OpenRouterClient {
    /// A single attempt; a failed request can still have consumed paid tokens.
    async fn try_complete_once(
        &self,
        url: &str,
        body: &serde_json::Value,
        media: MediaRequest,
        events: &mpsc::Sender<LlmEvent>,
        streamed: &mut bool,
        attempt: u32,
    ) -> Result<CompletionResponse, LlmError> {
        // Never log headers or the full body: bodies contain user content.
        let resp = self
            .http
            .post(url)
            .bearer_auth(&self.api_key)
            .header("HTTP-Referer", "http://127.0.0.1")
            .header("X-Title", "Vortex-Ai-Chat")
            .json(body)
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    // connect_timeout(10s) vs read_timeout(120s): report the
                    // phase that actually failed instead of a hardcoded value.
                    let connect = e.is_connect();
                    diag::event(
                        tracing::Level::WARN,
                        diag::PROVIDER_REQUEST_FAILED,
                        &[
                            ("attempt", diag::Field::Num(u64::from(attempt))),
                            (
                                "phase",
                                diag::Field::Static(if connect {
                                    "connect_timeout"
                                } else {
                                    "read_timeout"
                                }),
                            ),
                        ],
                    );
                    LlmError::Timeout(if connect { 10 } else { 120 })
                } else {
                    diag::event(
                        tracing::Level::WARN,
                        diag::PROVIDER_REQUEST_FAILED,
                        &[
                            ("attempt", diag::Field::Num(u64::from(attempt))),
                            (
                                "phase",
                                diag::Field::Static(if e.is_connect() {
                                    "connect"
                                } else {
                                    "request"
                                }),
                            ),
                        ],
                    );
                    LlmError::Network("request failed".into())
                }
            })?;

        let status = resp.status();
        if !status.is_success() {
            if !matches!(media, MediaRequest::None) {
                return Err(media.error(status.as_u16()));
            }
            let text = resp.text().await.unwrap_or_default();
            return Err(Self::map_status(status, text));
        }

        let mut parser = SseParser::new();
        let mut stream = resp.bytes_stream();
        let mut content = String::new();
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        let mut usage = None;
        let mut reported_completion_tokens = None;
        let mut finish_reason = String::new();
        let mut annotations = Vec::new();
        let mut annotation_bytes = 0;
        let mut annotation_overflow = false;
        // Provider-reported serving model: first non-empty `model` field wins
        // (a hostile late chunk must not rewrite the recorded provenance).
        let mut serving_model: Option<String> = None;
        // Once the event consumer is gone, stop trying to deliver deltas but
        // keep accumulating so the final response stays complete.
        let mut sink_alive = true;

        use futures::StreamExt;
        loop {
            let (payloads, eof) = match stream.next().await {
                Some(chunk) => {
                    let bytes = chunk.map_err(|e| {
                        if e.is_timeout() {
                            diag::event(
                                tracing::Level::WARN,
                                diag::SSE_READ_FAILED,
                                &[
                                    ("attempt", diag::Field::Num(u64::from(attempt))),
                                    ("phase", diag::Field::Static("read_timeout")),
                                ],
                            );
                            LlmError::Timeout(120)
                        } else {
                            diag::event(
                                tracing::Level::WARN,
                                diag::SSE_READ_FAILED,
                                &[
                                    ("attempt", diag::Field::Num(u64::from(attempt))),
                                    ("phase", diag::Field::Static("read")),
                                ],
                            );
                            LlmError::Network("stream read failed".into())
                        }
                    })?;
                    let p = parser.push(&bytes);
                    if parser.take_overflow() {
                        diag::event(
                            tracing::Level::WARN,
                            diag::SSE_OVERFLOW,
                            &[("attempt", diag::Field::Num(u64::from(attempt)))],
                        );
                        return Err(LlmError::MalformedStream(
                            "SSE payload exceeded the 1 MiB line / 8 MiB event cap".into(),
                        ));
                    }
                    (p, false)
                }
                None => {
                    // Stream ended without [DONE]: flush a trailing event that
                    // was never terminated by a blank line, then return what
                    // we accumulated.
                    let tail = parser.finish();
                    if parser.take_overflow() {
                        return Err(LlmError::MalformedStream(
                            "SSE payload exceeded the 1 MiB line / 8 MiB event cap".into(),
                        ));
                    }
                    (tail.into_iter().collect(), true)
                }
            };
            for payload in payloads {
                if payload == "[DONE]" {
                    return Ok(CompletionResponse {
                        content,
                        tool_calls,
                        usage,
                        finish_reason,
                        reported_completion_tokens,
                        annotations: (!annotations.is_empty()).then_some(annotations),
                        model: serving_model,
                    });
                }
                let v: serde_json::Value = match serde_json::from_str(&payload) {
                    Ok(v) => v,
                    Err(_) if eof && matches!(media, MediaRequest::None) => break,
                    Err(e) => {
                        // Diagnostics carry only the fixed category: the serde
                        // message is provider-controlled text.
                        diag::event(
                            tracing::Level::WARN,
                            diag::SSE_DECODE_FAILED,
                            &[("attempt", diag::Field::Num(u64::from(attempt)))],
                        );
                        return Err(LlmError::MalformedStream(
                            if matches!(media, MediaRequest::None) {
                                format!("bad chunk: {e}")
                            } else {
                                "invalid attachment response; retry or send without attachments"
                                    .into()
                            },
                        ));
                    }
                };
                if v.get("error").is_some_and(|e| !e.is_null()) {
                    let code = v
                        .pointer("/error/code")
                        .and_then(|c| c.as_u64().or_else(|| c.as_str()?.parse().ok()))
                        .and_then(|c| u16::try_from(c).ok())
                        .filter(|c| (400..600).contains(c))
                        .unwrap_or(502);
                    if !matches!(media, MediaRequest::None) {
                        return Err(media.error(code));
                    }
                    // OpenRouter can report an error after sending HTTP 200 and
                    // some text. Persist the partial reply, but never treat it as a stop.
                    finish_reason = "error".into();
                }
                if let Some(u) = v.get("usage").filter(|u| !u.is_null()) {
                    usage = Some(parse_usage(u));
                    if let Some(tokens) = u.get("completion_tokens").and_then(|v| v.as_u64()) {
                        reported_completion_tokens = Some(tokens);
                    }
                }
                if serving_model.is_none() {
                    if let Some(m) = v.get("model").and_then(|m| m.as_str()) {
                        if !m.is_empty() {
                            serving_model = Some(m.chars().take(MAX_SERVING_MODEL_CHARS).collect());
                        }
                    }
                }
                let Some(choice) = v.get("choices").and_then(|c| c.get(0)) else {
                    continue;
                };
                if matches!(media, MediaRequest::Pdf) && !annotation_overflow {
                    for a in [
                        choice.pointer("/delta/annotations"),
                        choice.pointer("/message/annotations"),
                    ]
                    .into_iter()
                    .flatten()
                    .filter_map(|v| v.as_array())
                    .flatten()
                    {
                        if !is_file_annotation(a) || annotations.contains(a) {
                            continue;
                        }
                        let size = json_size(a) + 1;
                        if annotations.len() == MAX_ANNOTATIONS
                            || annotation_bytes + size > MAX_ANNOTATION_CAPTURE_BYTES
                        {
                            // An omitted candidate could contradict a captured one.
                            annotations.clear();
                            annotation_overflow = true;
                            break;
                        }
                        annotation_bytes += size;
                        annotations.push(a.clone());
                    }
                }
                if let Some(reason) = choice.get("finish_reason").and_then(|f| f.as_str()) {
                    if reason == "error" && !matches!(media, MediaRequest::None) {
                        return Err(media.error(502));
                    }
                    if finish_reason != "error" {
                        finish_reason = reason.to_string();
                    }
                }
                let delta = choice.get("delta");
                if let Some(txt) = delta
                    .and_then(|d| d.get("content"))
                    .and_then(|c| c.as_str())
                {
                    if !txt.is_empty() {
                        content.push_str(txt);
                        *streamed = true;
                        if !eof
                            && sink_alive
                            && events
                                .send(LlmEvent::TextDelta(txt.to_string()))
                                .await
                                .is_err()
                        {
                            // Consumer hung up; keep accumulating the final text.
                            sink_alive = false;
                        }
                    }
                }
                // Keep the legacy tail handling for text and tools. The tail
                // contributes to final text, but isn't a dispatched delta.
                if eof {
                    continue;
                }
                if let Some(tcs) = delta
                    .and_then(|d| d.get("tool_calls"))
                    .and_then(|t| t.as_array())
                {
                    for tc in tcs {
                        let index = tc.get("index").and_then(|i| i.as_u64()).unwrap_or(0);
                        if index >= MAX_TOOL_CALL_SLOTS as u64 {
                            tracing::warn!(
                                index,
                                cap = MAX_TOOL_CALL_SLOTS,
                                "ignoring tool-call delta with out-of-range index"
                            );
                            continue;
                        }
                        let index = index as usize;
                        let id = tc.get("id").and_then(|i| i.as_str()).unwrap_or_default();
                        let name = tc
                            .pointer("/function/name")
                            .and_then(|n| n.as_str())
                            .unwrap_or_default();
                        let args = tc
                            .pointer("/function/arguments")
                            .and_then(|a| a.as_str())
                            .unwrap_or_default();
                        while tool_calls.len() <= index {
                            tool_calls.push(ToolCall {
                                id: String::new(),
                                name: String::new(),
                                arguments: String::new(),
                            });
                        }
                        let slot = &mut tool_calls[index];
                        if !id.is_empty() {
                            slot.id = id.to_string();
                        }
                        if !name.is_empty() {
                            slot.name = name.to_string();
                        }
                        slot.arguments.push_str(args);
                    }
                }
            }
            if eof {
                return Ok(CompletionResponse {
                    content,
                    tool_calls,
                    usage,
                    finish_reason,
                    reported_completion_tokens,
                    annotations: (!annotations.is_empty()).then_some(annotations),
                    model: serving_model,
                });
            }
        }
    }
}

fn parse_usage(v: &serde_json::Value) -> Usage {
    Usage {
        prompt_tokens: v.get("prompt_tokens").and_then(|x| x.as_u64()).unwrap_or(0),
        completion_tokens: v
            .get("completion_tokens")
            .and_then(|x| x.as_u64())
            .unwrap_or(0),
        total_tokens: v.get("total_tokens").and_then(|x| x.as_u64()).unwrap_or(0),
    }
}

#[async_trait]
impl LlmClient for OpenRouterClient {
    async fn complete(
        &self,
        req: &CompletionRequest,
        events: mpsc::Sender<LlmEvent>,
    ) -> Result<CompletionResponse, LlmError> {
        self.key_or_fail()?;
        let url = format!("{}/chat/completions", self.base_url);
        let mut media = MediaRequest::None;
        for message in &req.messages {
            if let Some(parts) = &message.parts {
                if parts.iter().any(|p| matches!(p, ContentPart::File { .. })) {
                    media = MediaRequest::Pdf;
                    break;
                }
                media = MediaRequest::Attachments;
            }
            if !message.attachments.is_empty() {
                media = MediaRequest::Attachments;
            }
        }
        let body = self.build_body(req, media);

        let mut streamed_any = false;
        let result = self
            .try_complete_once(&url, &body, media, &events, &mut streamed_any, 0)
            .await;
        if let Err(e) = &result {
            if streamed_any {
                diag::event(
                    tracing::Level::WARN,
                    diag::PROVIDER_MIDSTREAM_ABORT,
                    &[("reason", diag::Field::Static(e.category()))],
                );
            }
        }
        result
    }

    async fn models(&self) -> Result<Vec<ModelInfo>, LlmError> {
        self.key_or_fail()?;
        // Documented server-side ranking: tokens processed in the last week.
        // Response position becomes `weekly_rank` (usage popularity, never
        // quality); the id sort below keeps the wire order deterministic.
        let url = format!("{}/models?sort=top-weekly", self.base_url);
        let resp = self
            .http
            .get(&url)
            .bearer_auth(&self.api_key)
            .timeout(Duration::from_secs(20))
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    diag::event(
                        tracing::Level::WARN,
                        diag::PROVIDER_MODELS_FAILED,
                        &[("phase", diag::Field::Static("timeout"))],
                    );
                    LlmError::Timeout(20)
                } else {
                    diag::event(
                        tracing::Level::WARN,
                        diag::PROVIDER_MODELS_FAILED,
                        &[("phase", diag::Field::Static("network"))],
                    );
                    LlmError::Network("models request failed".into())
                }
            })?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            let err = Self::map_status(status, text);
            diag::event(
                tracing::Level::WARN,
                diag::PROVIDER_MODELS_FAILED,
                &[
                    ("reason", diag::Field::Static(err.category())),
                    ("status", diag::Field::Num(u64::from(status.as_u16()))),
                ],
            );
            return Err(err);
        }
        let v: serde_json::Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(e) => {
                diag::event(
                    tracing::Level::WARN,
                    diag::PROVIDER_MODELS_FAILED,
                    &[("reason", diag::Field::Static("malformed_stream"))],
                );
                return Err(LlmError::MalformedStream(format!("models body: {e}")));
            }
        };
        let arr = v
            .get("data")
            .and_then(|d| d.as_array())
            .cloned()
            .unwrap_or_default();
        let mut models = Vec::new();
        for (position, m) in arr.into_iter().enumerate() {
            let id = m
                .get("id")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_string();
            if id.is_empty() {
                continue;
            }
            let supports_tools = m
                .get("supported_parameters")
                .and_then(|x| x.as_array())
                .map(|a| a.iter().any(|p| p.as_str() == Some("tools")));
            let modalities = |field: &str| -> Vec<String> {
                m.pointer(&format!("/architecture/{field}"))
                    .and_then(|x| x.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default()
            };
            // Optional fee details stay None when unreported: unknown fees are
            // never normalized to zero (Auto ranking treats prompt/completion
            // as mandatory and these as known-applicable extras).
            let fee = |field: &str| -> Option<String> {
                m.pointer(&format!("/pricing/{field}"))
                    .and_then(|x| x.as_str())
                    .map(|s| s.to_string())
            };
            models.push(ModelInfo {
                id,
                name: m
                    .get("name")
                    .and_then(|x| x.as_str())
                    .unwrap_or_default()
                    .to_string(),
                context_length: m.get("context_length").and_then(|x| x.as_u64()),
                supports_tools,
                pricing_prompt: fee("prompt"),
                pricing_completion: fee("completion"),
                created: m.get("created").and_then(|x| x.as_i64()),
                pricing_request: fee("request"),
                pricing_image: fee("image"),
                input_modalities: modalities("input_modalities"),
                output_modalities: modalities("output_modalities"),
                // Provider-specific advertised output limit; routers may not
                // report one — absence stays absence (never an invented cap).
                max_completion_tokens: m
                    .pointer("/top_provider/max_completion_tokens")
                    .and_then(|x| x.as_u64()),
                // 1-based position in the requested top-weekly ordering
                // (weekly token usage). Unscored models are placed last by
                // the provider; their position is still a list position.
                weekly_rank: Some((position + 1).min(u32::MAX as usize) as u32),
                expires_on: m
                    .get("expiration_date")
                    .and_then(|x| x.as_str())
                    .filter(|s| !s.is_empty())
                    .map(str::to_string),
                ..Default::default()
            });
        }
        models.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(models)
    }

    fn describe(&self) -> &'static str {
        "openrouter"
    }
}
