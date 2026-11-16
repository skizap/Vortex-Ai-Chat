//! OpenRouter client (OpenAI-compatible `chat/completions` with streaming).
//!
//! Security notes:
//! - The API key is only ever sent in the `Authorization` header.
//! - Errors and logs never contain the key or raw request headers.
//! - Retries are bounded and never switch models or providers.

use crate::error::LlmError;
use crate::llm::{CompletionRequest, CompletionResponse, LlmClient, LlmEvent, ToolSchema};
use crate::sse::SseParser;
use async_trait::async_trait;
use std::time::Duration;
use tokio::sync::mpsc;
use vortex_core::diag;
use vortex_types::{ChatMessage, ModelInfo, Role, ToolCall, Usage};

const USER_AGENT: &str = concat!("vortex-ai-chat/", env!("CARGO_PKG_VERSION"));

/// Upper bound on provider-supplied tool-call `index` values. A hostile or
/// broken endpoint must not be able to grow the slot vector without limit.
const MAX_TOOL_CALL_SLOTS: usize = 64;

pub struct OpenRouterClient {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
    max_retries: u32,
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
            max_retries: 2,
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

    fn serialize_messages(messages: &[ChatMessage]) -> serde_json::Value {
        let mut out = Vec::with_capacity(messages.len());
        for m in messages {
            let mut obj = serde_json::json!({ "role": m.role.as_str(), "content": m.content });
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
        serde_json::json!(out)
    }

    fn build_body(&self, req: &CompletionRequest) -> serde_json::Value {
        let mut body = serde_json::json!({
            "model": req.model,
            "messages": Self::serialize_messages(&req.messages),
            "stream": true,
            "temperature": req.temperature,
            "max_tokens": req.max_tokens,
        });
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
    /// One attempt. `streamed` is set to true as soon as any delta has been
    /// forwarded to the caller; the retry loop uses it to avoid re-delivering
    /// text the consumer has already seen (duplicate output + double billing).
    async fn try_complete_once(
        &self,
        url: &str,
        body: &serde_json::Value,
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
            let text = resp.text().await.unwrap_or_default();
            return Err(Self::map_status(status, text));
        }

        let mut parser = SseParser::new();
        let mut stream = resp.bytes_stream();
        let mut content = String::new();
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        let mut usage = None;
        let mut finish_reason = String::new();
        // Once the event consumer is gone, stop trying to deliver deltas but
        // keep accumulating so the final response stays complete.
        let mut sink_alive = true;

        use futures::StreamExt;
        loop {
            let payloads = match stream.next().await {
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
                    p
                }
                None => {
                    // Stream ended without [DONE]: flush a trailing event that
                    // was never terminated by a blank line, then return what
                    // we accumulated.
                    if let Some(tail) = parser.finish() {
                        if tail != "[DONE]" {
                            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&tail) {
                                if let Some(u) = v.get("usage").filter(|u| !u.is_null()) {
                                    usage = Some(parse_usage(u));
                                }
                                if let Some(choice) = v.get("choices").and_then(|c| c.get(0)) {
                                    if let Some(reason) =
                                        choice.get("finish_reason").and_then(|f| f.as_str())
                                    {
                                        finish_reason = reason.to_string();
                                    }
                                    if let Some(txt) = choice
                                        .get("delta")
                                        .and_then(|d| d.get("content"))
                                        .and_then(|c| c.as_str())
                                    {
                                        if !txt.is_empty() {
                                            content.push_str(txt);
                                            *streamed = true;
                                        }
                                    }
                                }
                            }
                        }
                    }
                    return Ok(CompletionResponse {
                        content,
                        tool_calls,
                        usage,
                        finish_reason,
                    });
                }
            };
            for payload in payloads {
                if payload == "[DONE]" {
                    return Ok(CompletionResponse {
                        content,
                        tool_calls,
                        usage,
                        finish_reason,
                    });
                }
                let v: serde_json::Value = serde_json::from_str(&payload).map_err(|e| {
                    // Diagnostics carry only the fixed category: the serde
                    // message is provider-controlled text.
                    diag::event(
                        tracing::Level::WARN,
                        diag::SSE_DECODE_FAILED,
                        &[("attempt", diag::Field::Num(u64::from(attempt)))],
                    );
                    LlmError::MalformedStream(format!("bad chunk: {e}"))
                })?;
                if let Some(u) = v.get("usage").filter(|u| !u.is_null()) {
                    usage = Some(parse_usage(u));
                }
                let Some(choice) = v.get("choices").and_then(|c| c.get(0)) else {
                    continue;
                };
                if let Some(reason) = choice.get("finish_reason").and_then(|f| f.as_str()) {
                    finish_reason = reason.to_string();
                }
                let delta = choice.get("delta");
                if let Some(txt) = delta
                    .and_then(|d| d.get("content"))
                    .and_then(|c| c.as_str())
                {
                    if !txt.is_empty() {
                        content.push_str(txt);
                        *streamed = true;
                        if sink_alive
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
        let body = self.build_body(req);

        let mut attempt: u32 = 0;
        // Once any attempt has streamed text to the caller, retrying would
        // re-deliver already-shown content into the same channel (duplicated
        // UI text + double billing), so retries stop at that point.
        let mut streamed_any = false;
        loop {
            match self
                .try_complete_once(&url, &body, &events, &mut streamed_any, attempt)
                .await
            {
                Ok(resp) => return Ok(resp),
                Err(e) => {
                    if e.is_retryable() && attempt < self.max_retries && !streamed_any {
                        attempt += 1;
                        // Bounded backoff: 1s, 3s. Never longer.
                        tokio::time::sleep(Duration::from_secs(1 + 2 * attempt as u64)).await;
                        tracing::warn!(retry = attempt, "retrying provider request");
                        diag::event(
                            tracing::Level::WARN,
                            diag::PROVIDER_RETRY,
                            &[
                                ("attempt", diag::Field::Num(u64::from(attempt))),
                                ("reason", diag::Field::Static(e.category())),
                            ],
                        );
                        continue;
                    }
                    if streamed_any && e.is_retryable() {
                        tracing::warn!(
                            "provider failed mid-stream after partial delivery; not retrying \
                             to avoid duplicating streamed output"
                        );
                        diag::event(
                            tracing::Level::WARN,
                            diag::PROVIDER_MIDSTREAM_ABORT,
                            &[("reason", diag::Field::Static(e.category()))],
                        );
                    }
                    return Err(e);
                }
            }
        }
    }

    async fn models(&self) -> Result<Vec<ModelInfo>, LlmError> {
        self.key_or_fail()?;
        let url = format!("{}/models", self.base_url);
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
        for m in arr {
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
            models.push(ModelInfo {
                id,
                name: m
                    .get("name")
                    .and_then(|x| x.as_str())
                    .unwrap_or_default()
                    .to_string(),
                context_length: m.get("context_length").and_then(|x| x.as_u64()),
                supports_tools,
                pricing_prompt: m
                    .pointer("/pricing/prompt")
                    .and_then(|x| x.as_str())
                    .map(|s| s.to_string()),
                pricing_completion: m
                    .pointer("/pricing/completion")
                    .and_then(|x| x.as_str())
                    .map(|s| s.to_string()),
            });
        }
        models.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(models)
    }

    fn describe(&self) -> &'static str {
        "openrouter"
    }
}
