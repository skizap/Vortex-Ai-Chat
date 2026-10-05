//! Errors surfaced by LLM providers.

#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    #[error("OPENROUTER_API_KEY is not configured. Add it to your environment or config.toml (see README.md 'Setup').")]
    NotConfigured,
    #[error("invalid or expired API credentials (HTTP 401/403); check your OpenRouter key")]
    InvalidCredentials,
    #[error("insufficient credits (HTTP 402); check your OpenRouter account")]
    InsufficientCredits,
    #[error("rate limited by the provider (HTTP 429); retry later or lower request frequency")]
    RateLimited,
    #[error("provider server error (HTTP {0}): {1}")]
    Provider(u16, String),
    #[error("request timed out after {0}s")]
    Timeout(u64),
    #[error("malformed response from provider: {0}")]
    MalformedStream(String),
    #[error("model not found: {0}")]
    ModelNotFound(String),
    #[error("network error talking to the provider: {0}")]
    Network(String),
}

impl LlmError {
    /// Errors worth retrying with a bounded backoff. Never includes auth errors.
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            LlmError::RateLimited
                | LlmError::Provider(_, _)
                | LlmError::Timeout(_)
                | LlmError::Network(_)
        )
    }
}
