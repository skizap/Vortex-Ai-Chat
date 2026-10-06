//! Offline demo/testing LLM used when `VORTEX_LLM=mock`.
//!
//! Behaviors:
//! - `echo` (default): streams "You said: <message>".
//! - `tools`: performs one `web_search` tool round-trip and then streams an
//!   answer quoting the tool evidence — proving the full tool pipeline
//!   (tool call → registry validation → provider → result → citations)
//!   without any network or credentials.

use async_trait::async_trait;
use std::sync::Mutex;
use tokio::sync::mpsc;
use vortex_llm::llm::{CompletionRequest, CompletionResponse, LlmClient, LlmEvent};
use vortex_llm::LlmError;
use vortex_types::{ChatMessage, ModelInfo, Role, Usage};

pub struct DemoMockLlm {
    script: String,
    /// Turns used by this "session"; every run starts fresh.
    turn_counter: Mutex<u32>,
}

impl DemoMockLlm {
    pub fn new(script: &str) -> Self {
        Self {
            script: if script.is_empty() {
                "echo".to_string()
            } else {
                script.to_string()
            },
            turn_counter: Mutex::new(0),
        }
    }

    fn last_user_message(messages: &[ChatMessage]) -> String {
        messages
            .iter()
            .rev()
            .find(|m| m.role == Role::User)
            .map(|m| m.content.clone())
            .unwrap_or_default()
    }

    fn last_tool_result(messages: &[ChatMessage]) -> String {
        messages
            .iter()
            .rev()
            .find(|m| m.role == Role::Tool)
            .map(|m| m.content.clone())
            .unwrap_or_default()
    }
}

#[async_trait]
impl LlmClient for DemoMockLlm {
    async fn complete(
        &self,
        req: &CompletionRequest,
        events: mpsc::Sender<LlmEvent>,
    ) -> Result<CompletionResponse, LlmError> {
        let turn = {
            let mut c = self.turn_counter.lock().expect("mock turn counter");
            let t = *c;
            *c += 1;
            t
        };
        let usage = Some(Usage {
            prompt_tokens: 12,
            completion_tokens: 20,
            total_tokens: 32,
        });

        if self.script == "tools" && turn == 0 {
            let query = Self::last_user_message(&req.messages);
            let args = serde_json::json!({ "query": query });
            return Ok(CompletionResponse {
                content: String::new(),
                tool_calls: vec![vortex_types::ToolCall {
                    id: "mock-call-1".to_string(),
                    name: "web_search".to_string(),
                    arguments: args.to_string(),
                }],
                usage,
                finish_reason: "tool_calls".to_string(),
            });
        }

        let content = if self.script == "tools" {
            let evidence = Self::last_tool_result(&req.messages);
            let head: String = evidence.chars().take(400).collect();
            format!(
                "Here is what the research found (offline mock data, no real network):\n\n{head}\n\n\
                 Sources are listed above as returned by the search tool."
            )
        } else {
            format!("You said: {}", Self::last_user_message(&req.messages))
        };

        for word in content.split_inclusive(' ') {
            let _ = events.try_send(LlmEvent::TextDelta(word.to_string()));
            tokio::time::sleep(std::time::Duration::from_millis(3)).await;
        }
        Ok(CompletionResponse {
            content,
            tool_calls: Vec::new(),
            usage,
            finish_reason: "stop".to_string(),
        })
    }

    async fn models(&self) -> Result<Vec<ModelInfo>, LlmError> {
        Ok(vec![ModelInfo {
            id: "mock/model".to_string(),
            name: "Mock Model (offline)".to_string(),
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
