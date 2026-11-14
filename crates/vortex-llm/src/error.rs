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
    /// Errors worth retrying with a bounded backoff. Never includes auth
    /// errors, and never includes deterministic 4xx provider errors: a bad
    /// request will fail identically on retry, so only 5xx `Provider` codes
    /// qualify (429 maps to `RateLimited`, which is retryable).
    pub fn is_retryable(&self) -> bool {
        match self {
            LlmError::RateLimited | LlmError::Timeout(_) | LlmError::Network(_) => true,
            LlmError::Provider(code, _) => *code >= 500,
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retryable_classification() {
        assert!(LlmError::RateLimited.is_retryable());
        assert!(LlmError::Timeout(120).is_retryable());
        assert!(LlmError::Network("x".into()).is_retryable());
        assert!(LlmError::Provider(500, "boom".into()).is_retryable());
        assert!(LlmError::Provider(503, "unavailable".into()).is_retryable());
        assert!(!LlmError::Provider(400, "bad request".into()).is_retryable());
        assert!(!LlmError::Provider(405, "method".into()).is_retryable());
        assert!(!LlmError::InvalidCredentials.is_retryable());
        assert!(!LlmError::InsufficientCredits.is_retryable());
        assert!(!LlmError::ModelNotFound("m".into()).is_retryable());
        assert!(!LlmError::MalformedStream("m".into()).is_retryable());
        assert!(!LlmError::NotConfigured.is_retryable());
    }
}
