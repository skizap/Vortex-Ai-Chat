//! LLM provider abstraction: OpenRouter streaming client and mock provider.

pub mod error;
pub mod llm;
pub mod mock;
pub mod openrouter;
pub mod sse;

pub use error::LlmError;
pub use llm::{CompletionRequest, CompletionResponse, LlmClient, LlmEvent, ToolSchema};
pub use mock::{MockLlm, MockTurn};
pub use openrouter::OpenRouterClient;
