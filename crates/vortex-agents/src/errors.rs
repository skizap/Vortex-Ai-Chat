//! Tool-layer errors. All variants render to user- and model-safe strings;
//! no secrets or raw internals leak through them.

#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("invalid arguments for {0}: {1}")]
    InvalidArgs(String, String),
    #[error("not permitted: {0}")]
    NotPermitted(String),
    #[error("tool unavailable: {0}")]
    Unavailable(String),
    #[error("denied by the user: {0}")]
    Denied(String),
    #[error("operation was cancelled")]
    Cancelled,
    #[error("operation timed out")]
    Timeout,
    #[error("{0}")]
    Execution(String),
}

/// Risk classification used by the server-side permission policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Risk {
    /// Read-only / harmless.
    Low,
    /// Reversible workspace actions; allowed under Standard/Trusted profiles.
    Medium,
    /// Always requires explicit human approval, regardless of profile,
    /// including when requested by a sub-agent.
    Consequential,
}

impl ToolError {
    /// True when the error means "this cannot be retried as-is" and should be
    /// surfaced to the model as a definitive tool failure.
    pub fn is_fatal(&self) -> bool {
        matches!(self, ToolError::Denied(_) | ToolError::NotPermitted(_))
    }
}
