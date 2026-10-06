//! Provider-agnostic completion interface.

use crate::error::LlmError;
use async_trait::async_trait;
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

#[derive(Debug, Clone)]
pub struct CompletionRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    pub tools: Vec<ToolSchema>,
    pub temperature: f32,
    pub max_tokens: u32,
}

/// Live progress events streamed while a completion runs.
#[derive(Debug, Clone)]
pub enum LlmEvent {
    TextDelta(String),
}

#[derive(Debug, Clone)]
pub struct CompletionResponse {
    pub content: String,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Option<Usage>,
    pub finish_reason: String,
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
