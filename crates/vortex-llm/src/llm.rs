//! Provider-agnostic completion interface.

use crate::error::LlmError;
use async_trait::async_trait;
use serde_json::Value;
use std::collections::BTreeMap;
use tokio::sync::mpsc;
use vortex_types::{ChatMessage, ModelInfo, ToolCall, Usage};

/// Tool definition exposed to the model (OpenAI-style JSON Schema payload).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub parameters: serde_json::Value,
}

impl ToolSchema {
    pub fn function(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: serde_json::Value,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            kind: "function".into(),
            parameters,
        }
    }
}

#[derive(Clone)]
pub struct CompletionRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    pub tools: Vec<ToolSchema>,
    pub temperature: f32,
    pub max_tokens: u32,
    pub pdf_engine: Option<PdfEngine>,
    /// Request-only annotations keyed by assistant message index.
    pub assistant_annotations: BTreeMap<usize, Vec<Value>>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PdfEngine {
    #[default]
    CloudflareAi,
    MistralOcr,
}

impl PdfEngine {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "cloudflare-ai" => Some(Self::CloudflareAi),
            "mistral-ocr" => Some(Self::MistralOcr),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::CloudflareAi => "cloudflare-ai",
            Self::MistralOcr => "mistral-ocr",
        }
    }
}

/// Count serialized bytes without allocating another payload buffer.
pub fn json_size(value: &impl serde::Serialize) -> usize {
    #[derive(Default)]
    struct Counter(usize);
    impl std::io::Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self.0.saturating_add(bytes.len());
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter::default();
    serde_json::to_writer(&mut counter, value).map_or(usize::MAX, |()| counter.0)
}

pub const MAX_FILE_ANNOTATION_BYTES: usize = 1024 * 1024;

fn inline_image(url: &str) -> bool {
    let Some((prefix, data)) = url.split_once(',') else {
        return false;
    };
    let body = data.trim_end_matches('=');
    matches!(
        prefix,
        "data:image/png;base64"
            | "data:image/jpeg;base64"
            | "data:image/webp;base64"
            | "data:image/gif;base64"
    ) && !body.is_empty()
        && data.len().is_multiple_of(4)
        && data.len() - body.len() <= 2
        && body
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/'))
}

/// Accept complete, documented file annotations only. Do not combine partial
/// deltas or replay arbitrary URLs and unrecognized fields from the provider.
pub fn is_file_annotation(value: &Value) -> bool {
    let Some(file) = value.get("file").and_then(Value::as_object) else {
        return false;
    };
    value.as_object().is_some_and(|o| o.len() == 2)
        && value["type"] == "file"
        && file
            .keys()
            .all(|k| matches!(k.as_str(), "hash" | "name" | "content"))
        && file
            .get("hash")
            .and_then(Value::as_str)
            .is_some_and(|h| !h.is_empty() && h.len() <= 256)
        && file
            .get("name")
            .is_none_or(|n| n.as_str().is_some_and(|s| !s.is_empty() && s.len() <= 512))
        && file
            .get("content")
            .and_then(Value::as_array)
            .is_some_and(|parts| {
                !parts.is_empty()
                    && parts.iter().all(|part| {
                        part.as_object().is_some_and(|o| o.len() == 2)
                            && match part["type"].as_str() {
                                Some("text") => part["text"].is_string(),
                                Some("image_url") => {
                                    part["image_url"].as_object().is_some_and(|o| o.len() == 1)
                                        && part["image_url"]["url"]
                                            .as_str()
                                            .is_some_and(inline_image)
                                }
                                _ => false,
                            }
                    })
            })
        && json_size(value) <= MAX_FILE_ANNOTATION_BYTES
}

/// Live progress events streamed while a completion runs.
#[derive(Debug, Clone)]
pub enum LlmEvent {
    TextDelta(String),
}

#[derive(Clone)]
pub struct CompletionResponse {
    pub content: String,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Option<Usage>,
    pub finish_reason: String,
    pub annotations: Option<Vec<Value>>,
}

impl std::fmt::Debug for CompletionResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompletionResponse")
            .field("usage", &self.usage)
            .finish_non_exhaustive()
    }
}

/// A chat completion provider. Implementations must never log credentials.
#[async_trait]
pub trait LlmClient: Send + Sync {
    /// One streaming chat completion. Text deltas are forwarded through
    /// `events` as they arrive; the final value contains the full turn.
    async fn complete(
        &self,
        req: &CompletionRequest,
        events: mpsc::Sender<LlmEvent>,
    ) -> Result<CompletionResponse, LlmError>;

    /// The provider model catalog, when retrievable.
    async fn models(&self) -> Result<Vec<ModelInfo>, LlmError>;

    fn describe(&self) -> &'static str;
}
