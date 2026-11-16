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

    /// Fixed, payload-free category for diagnostics. Deliberately discards
    /// every dynamic string this error carries (provider body excerpts,
    /// serde messages): diagnostics must never carry arbitrary error text.
    pub fn category(&self) -> &'static str {
        match self {
            LlmError::NotConfigured => "not_configured",
            LlmError::InvalidCredentials => "invalid_credentials",
            LlmError::InsufficientCredits => "insufficient_credits",
            LlmError::RateLimited => "rate_limited",
            LlmError::Provider(..) => "provider_status",
            LlmError::Timeout(_) => "timeout",
            LlmError::MalformedStream(_) => "malformed_stream",
            LlmError::ModelNotFound(_) => "model_not_found",
            LlmError::Network(_) => "network",
        }
    }

    /// HTTP status code when one is known; safe to log (a plain number).
    pub fn status(&self) -> Option<u16> {
        match self {
            LlmError::Provider(code, _) => Some(*code),
            _ => None,
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

    #[test]
    fn diagnostic_categories_are_fixed_and_status_is_numeric_only() {
        assert_eq!(LlmError::NotConfigured.category(), "not_configured");
        assert_eq!(
            LlmError::InvalidCredentials.category(),
            "invalid_credentials"
        );
        assert_eq!(
            LlmError::InsufficientCredits.category(),
            "insufficient_credits"
        );
        assert_eq!(LlmError::RateLimited.category(), "rate_limited");
        assert_eq!(LlmError::Timeout(99).category(), "timeout");
        assert_eq!(LlmError::Network("x".into()).category(), "network");
        assert_eq!(
            LlmError::MalformedStream(String::new()).category(),
            "malformed_stream"
        );
        assert_eq!(
            LlmError::ModelNotFound(String::new()).category(),
            "model_not_found"
        );
        assert_eq!(
            LlmError::Provider(503, "boom".into()).category(),
            "provider_status"
        );
        assert_eq!(LlmError::Provider(503, "boom".into()).status(), Some(503));
        assert_eq!(LlmError::RateLimited.status(), None);
        // Categories are fixed labels, never derived from dynamic strings.
        for category in [
            LlmError::NotConfigured.category(),
            LlmError::InvalidCredentials.category(),
            LlmError::InsufficientCredits.category(),
            LlmError::RateLimited.category(),
            LlmError::Provider(0, String::new()).category(),
            LlmError::Timeout(0).category(),
            LlmError::MalformedStream(String::new()).category(),
            LlmError::ModelNotFound(String::new()).category(),
            LlmError::Network(String::new()).category(),
        ] {
            assert!(!category.is_empty());
            assert!(category.chars().all(|c| c.is_ascii_lowercase() || c == '_'));
        }
    }
}
