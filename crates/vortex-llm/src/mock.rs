//! Scripted mock LLM provider used by tests and offline demos.
//!
//! It never touches the network. `MockTurn`s let tests script exact model
//! behavior: plain text, tool calls, failures.

use crate::error::LlmError;
use crate::llm::{CompletionRequest, CompletionResponse, LlmClient, LlmEvent};
use async_trait::async_trait;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;
use vortex_types::{ModelInfo, ToolCall};

#[derive(Debug, Clone)]
pub struct MockTurn {
    pub content: String,
    pub tool_calls: Vec<ToolCall>,
}

impl MockTurn {
    pub fn text(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            tool_calls: Vec::new(),
        }
    }

    pub fn tool_call(
        name: impl Into<String>,
        id: impl Into<String>,
        arguments: impl Into<String>,
    ) -> Self {
        Self {
            content: String::new(),
            tool_calls: vec![ToolCall {
                id: id.into(),
                name: name.into(),
                arguments: arguments.into(),
            }],
        }
    }
}

pub struct MockLlm {
    turns: tokio::sync::Mutex<Vec<MockTurn>>,
    /// Simulated per-call latency so streaming can be observed and cancellation
    /// exercised deterministically.
    per_delta_delay_ms: u64,
    calls: AtomicUsize,
}

impl MockLlm {
    pub fn new(turns: Vec<MockTurn>) -> Self {
        Self {
            turns: tokio::sync::Mutex::new(turns),
            per_delta_delay_ms: 5,
            calls: AtomicUsize::new(0),
        }
    }

    pub fn with_delay(turns: Vec<MockTurn>, per_delta_delay_ms: u64) -> Self {
        Self {
            turns: tokio::sync::Mutex::new(turns),
            per_delta_delay_ms,
            calls: AtomicUsize::new(0),
        }
    }

    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl LlmClient for MockLlm {
    async fn complete(
        &self,
        _req: &CompletionRequest,
        events: mpsc::Sender<LlmEvent>,
    ) -> Result<CompletionResponse, LlmError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let turn = {
            let mut guard = self.turns.lock().await;
            if guard.is_empty() {
                return Ok(CompletionResponse {
                    content: "mock: no more scripted turns".into(),
                    tool_calls: Vec::new(),
                    usage: None,
                    finish_reason: "stop".into(),
                });
            }
            guard.remove(0)
        };
        // Stream content word-by-word to emulate token streaming.
        let content = turn.content;
        let tool_calls = turn.tool_calls;
        for word in content.split_inclusive(' ') {
            let _ = events.try_send(LlmEvent::TextDelta(word.to_string()));
            if self.per_delta_delay_ms > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(self.per_delta_delay_ms)).await;
            }
        }
        let completion_tokens = content.len() as u64 / 4;
        let has_tool_calls = !tool_calls.is_empty();
        Ok(CompletionResponse {
            content,
            tool_calls,
            usage: Some(vortex_types::Usage {
                prompt_tokens: 10,
                completion_tokens,
                total_tokens: 10 + completion_tokens,
            }),
            finish_reason: if has_tool_calls {
                "tool_calls".into()
            } else {
                "stop".into()
            },
        })
    }

    async fn models(&self) -> Result<Vec<ModelInfo>, LlmError> {
        Ok(vec![ModelInfo {
            id: "mock/model".into(),
            name: "Mock Model".into(),
            context_length: Some(8192),
            supports_tools: Some(true),
            pricing_prompt: None,
            pricing_completion: None,
        }])
    }

    fn describe(&self) -> &'static str {
        "mock"
    }
}

/// Shared handle so tests can construct `Arc<dyn LlmClient>` conveniently.
pub fn shared(turns: Vec<MockTurn>) -> Arc<dyn LlmClient> {
    Arc::new(MockLlm::new(turns))
}
