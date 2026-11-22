//! Offline demo/testing LLM used when `VORTEX_LLM=mock`.
//!
//! Behaviors:
//! - `echo` (default): streams "You said: <message>".
//! - `tools`: performs one `web_search` tool round-trip and then streams an
//!   answer quoting the tool evidence — proving the full tool pipeline
//!   (tool call → registry validation → provider → result → citations)
//!   without any network or credentials.
//!
//! The turn is derived from the request itself (has a tool result arrived
//! yet?), not from a process-global counter, so *every* run performs the
//! tool round-trip — the counter version silently degraded to plain text
//! for all runs after the first.

use async_trait::async_trait;
use tokio::sync::mpsc;
use vortex_llm::llm::{CompletionRequest, CompletionResponse, LlmClient, LlmEvent};
use vortex_llm::LlmError;
use vortex_types::{ChatMessage, ModelInfo, Role, Usage};

pub struct DemoMockLlm {
    script: String,
}

impl DemoMockLlm {
    pub fn new(script: &str) -> Self {
        Self {
            script: if script.is_empty() {
                "echo".to_string()
            } else {
                script.to_string()
            },
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
        // "First turn" == no tool result in the conversation yet. This is
        // per-request state, so concurrent and subsequent runs behave alike.
        let has_tool_result = req.messages.iter().any(|m| m.role == Role::Tool);
        let usage = Some(Usage {
            prompt_tokens: 12,
            completion_tokens: 20,
            total_tokens: 32,
        });

        if self.script == "tools" && !has_tool_result {
            let query = Self::last_user_message(&req.messages);
            let args = serde_json::json!({ "query": query });
            return Ok(CompletionResponse {
                content: String::new(),
                tool_calls: vec![vortex_types::ToolCall {
                    id: format!("mock-call-{}", uuid::Uuid::new_v4().simple()),
                    name: "web_search".to_string(),
                    arguments: args.to_string(),
                }],
                usage,
                finish_reason: "tool_calls".to_string(),
                reported_completion_tokens: usage.map(|u| u.completion_tokens),
                annotations: None,
                // Deterministic serving model for the offline harness.
                model: Some("mock/model".to_string()),
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
            if events
                .send(LlmEvent::TextDelta(word.to_string()))
                .await
                .is_err()
            {
                break; // consumer hung up
            }
            tokio::time::sleep(std::time::Duration::from_millis(3)).await;
        }
        Ok(CompletionResponse {
            content,
            tool_calls: Vec::new(),
            usage,
            finish_reason: "stop".to_string(),
            reported_completion_tokens: usage.map(|u| u.completion_tokens),
            annotations: None,
            // Deterministic serving model for the offline harness.
            model: Some("mock/model".to_string()),
        })
    }

    async fn models(&self) -> Result<Vec<ModelInfo>, LlmError> {
        // Deterministic offline demo catalog (id-sorted, no network). It
        // exercises the recommended-shortlist filters end to end:
        // - mapped rows with real-ish in-bounds prices → recommended
        // - mapped row above the output bound (thinking-2507 at $3.5/M) → out
        // - mapped restricted-license row (qwen-2.5-72b) → flagged, out
        // - mapped row with unknown price (qwen3.5-9b) → out (never zero)
        // - unmapped free row (mock/model) → out (no verified license) but
        //   still the cheapest Auto pick
        // - router row (openrouter/auto) → out, and no invented output limit
        // Weekly ranks stay None on purpose: the offline harness has no
        // ranking source, so ranking_source reports unavailable and clients
        // omit popularity claims (recommended is id-ordered).
        let text = vec!["text".to_string()];
        let text_image = vec!["text".to_string(), "image".to_string()];
        Ok(vec![
            ModelInfo {
                id: "deepseek/deepseek-chat-v3.1".to_string(),
                name: "DeepSeek: DeepSeek V3.1 (offline demo)".to_string(),
                context_length: Some(163_840),
                supports_tools: Some(true),
                pricing_prompt: Some("0.00000025".into()),
                pricing_completion: Some("0.00000095".into()),
                created: Some(1_753_000_000),
                max_completion_tokens: Some(147_456),
                input_modalities: text.clone(),
                output_modalities: text.clone(),
                ..Default::default()
            },
            ModelInfo {
                id: "mistralai/mistral-nemo".to_string(),
                name: "Mistral: Mistral Nemo (offline demo)".to_string(),
                context_length: Some(131_072),
                supports_tools: Some(true),
                pricing_prompt: Some("0.000000029".into()),
                pricing_completion: Some("0.00000003".into()),
                created: Some(1_721_000_000),
                max_completion_tokens: Some(16_384),
                input_modalities: text.clone(),
                output_modalities: text.clone(),
                ..Default::default()
            },
            ModelInfo {
                id: "mistralai/mistral-small-3.2-24b-instruct".to_string(),
                name: "Mistral: Mistral Small 3.2 24B (offline demo)".to_string(),
                context_length: Some(256_000),
                supports_tools: Some(true),
                pricing_prompt: Some("0.000000075".into()),
                pricing_completion: Some("0.0000002".into()),
                created: Some(1_750_000_000),
                max_completion_tokens: Some(16_384),
                input_modalities: text_image.clone(),
                output_modalities: text.clone(),
                ..Default::default()
            },
            ModelInfo {
                id: "mock/model".to_string(),
                name: "Mock Model (offline)".to_string(),
                context_length: Some(128_000),
                supports_tools: Some(true),
                pricing_prompt: Some("0".into()),
                pricing_completion: Some("0".into()),
                created: Some(1_700_000_000),
                input_modalities: text_image.clone(),
                output_modalities: text.clone(),
                ..Default::default()
            },
            ModelInfo {
                id: "openrouter/auto".to_string(),
                name: "OpenRouter: Auto (offline demo)".to_string(),
                context_length: None,
                supports_tools: None,
                pricing_prompt: Some("0".into()),
                pricing_completion: Some("0".into()),
                created: Some(1_700_000_000),
                input_modalities: text_image.clone(),
                output_modalities: text.clone(),
                ..Default::default()
            },
            ModelInfo {
                id: "qwen/qwen-2.5-72b-instruct".to_string(),
                name: "Qwen2.5 72B Instruct (offline demo)".to_string(),
                context_length: Some(32_768),
                supports_tools: Some(true),
                pricing_prompt: Some("0.00000036".into()),
                pricing_completion: Some("0.0000004".into()),
                created: Some(1_726_000_000),
                max_completion_tokens: Some(16_384),
                input_modalities: text.clone(),
                output_modalities: text.clone(),
                ..Default::default()
            },
            ModelInfo {
                id: "qwen/qwen3-235b-a22b-thinking-2507".to_string(),
                name: "Qwen: Qwen3 235B A22B Thinking 2507 (offline demo)".to_string(),
                context_length: Some(128_000),
                supports_tools: Some(true),
                pricing_prompt: Some("0.00000045".into()),
                pricing_completion: Some("0.0000035".into()),
                created: Some(1_753_000_000),
                max_completion_tokens: Some(16_384),
                input_modalities: text.clone(),
                output_modalities: text.clone(),
                ..Default::default()
            },
            ModelInfo {
                id: "qwen/qwen3-32b".to_string(),
                name: "Qwen: Qwen3 32B (offline demo)".to_string(),
                context_length: Some(131_072),
                supports_tools: Some(true),
                pricing_prompt: Some("0.00000008".into()),
                pricing_completion: Some("0.00000028".into()),
                created: Some(1_745_000_000),
                max_completion_tokens: Some(16_384),
                input_modalities: text.clone(),
                output_modalities: text.clone(),
                ..Default::default()
            },
            ModelInfo {
                id: "qwen/qwen3.5-9b".to_string(),
                name: "Qwen: Qwen3.5 9B (offline demo, price unknown)".to_string(),
                context_length: Some(262_144),
                supports_tools: Some(true),
                pricing_prompt: None,
                pricing_completion: None,
                created: Some(1_760_000_000),
                max_completion_tokens: Some(81_920),
                input_modalities: text.clone(),
                output_modalities: text.clone(),
                ..Default::default()
            },
        ])
    }

    fn describe(&self) -> &'static str {
        "mock"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The offline demo catalog must exercise the recommended-shortlist
    /// filters under `VORTEX_LLM=mock`: verified in-bounds rows are listed,
    /// every exclusion category is present, and ranking stays honestly
    /// unavailable (no popularity claims without a ranking source).
    #[tokio::test]
    async fn demo_catalog_exercises_recommended_filters() {
        let llm = DemoMockLlm::new("echo");
        let models = llm.models().await.unwrap();
        let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
        assert!(
            ids.is_sorted(),
            "demo catalog stays id-sorted/deterministic"
        );

        // No weekly ranks offline → ranking unavailable.
        assert!(models.iter().all(|m| m.weekly_rank.is_none()));
        assert_eq!(vortex_llm::catalog::ranking_source(&models), None);

        let enriched = vortex_llm::catalog::enrich(&models);
        let recommended = vortex_llm::catalog::recommended(
            &enriched,
            vortex_types::RecommendedBounds::default(),
            vortex_llm::catalog::DEFAULT_RECOMMENDED_LIMIT,
        );
        // Verified + in-bounds rows, id-ordered (ranking unavailable).
        assert_eq!(
            recommended,
            vec![
                "deepseek/deepseek-chat-v3.1",
                "mistralai/mistral-nemo",
                "mistralai/mistral-small-3.2-24b-instruct",
                "qwen/qwen3-32b",
            ]
        );
        // Exclusion categories present in the catalog but not recommended:
        // over output bound, restricted license, unknown price, unmapped free
        // row, router alias.
        let find = |id: &str| enriched.iter().find(|m| m.id == id).unwrap();
        assert!(!recommended.contains(&"qwen/qwen3-235b-a22b-thinking-2507".to_string()));
        assert!(find("qwen/qwen-2.5-72b-instruct").restriction.is_some());
        assert!(find("qwen/qwen3.5-9b").pricing_prompt.is_none());
        assert!(find("mock/model").open_weights.is_none());
        assert!(vortex_llm::catalog::is_router_or_batch("openrouter/auto"));
        // A router never gets an invented output limit.
        assert!(find("openrouter/auto").max_completion_tokens.is_none());
        // Auto still resolves to the free unmapped mock row (cheapest, id
        // tie-break): the demo rows must not hijack offline Auto routing.
        let req = vortex_agents::routing::AutoRequirements {
            needs_image: false,
            needs_video: false,
            needs_tools: false,
            image_count: 0,
            est_input_tokens: 100,
            output_allowance: 100,
        };
        assert_eq!(
            vortex_agents::routing::resolve_auto(&enriched, &req).as_deref(),
            Some("mock/model")
        );
    }
}
