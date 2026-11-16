//! Data transfer objects for the REST API and SSE snapshots.

use crate::chat::{ToolCall, Usage};
use crate::events::{PlanStep, RunStatus};
use crate::Mode;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationSummary {
    pub id: String,
    pub title: String,
    pub mode: Mode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredMessage {
    pub id: i64,
    pub role: crate::chat::Role,
    pub content: String,
    #[serde(default)]
    pub tool_calls: Vec<ToolCall>,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatRequestDto {
    pub content: String,
    #[serde(default)]
    pub mode: Option<Mode>,
}

/// A run (coordinator or sub-agent) as exposed to the UI.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunInfo {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation_id: Option<String>,
    pub agent: String,
    #[serde(default)]
    pub task: String,
    pub model: String,
    pub status: RunStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    #[serde(default)]
    pub plan: Vec<PlanStep>,
    #[serde(default)]
    pub depth: u32,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallRecord {
    pub id: String,
    pub run_id: String,
    pub tool: String,
    pub args: serde_json::Value,
    pub ok: bool,
    pub summary: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalInfo {
    pub id: String,
    pub run_id: String,
    pub agent: String,
    pub title: String,
    pub payload: String,
    pub status: ApprovalStatus,
    pub created_at: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ApprovalStatus {
    Pending,
    Approved,
    Denied,
    Expired,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreviewInfo {
    pub id: String,
    pub run_id: String,
    pub url: String,
    pub root: String,
    pub running: bool,
}

/// File checkpoint metadata for the recovery UI. The snapshot content itself
/// is never shipped in lists (it can be large); restore reads it server-side.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointInfo {
    pub id: i64,
    pub run_id: String,
    /// Absolute path as recorded when the snapshot was taken.
    pub path: String,
    /// `false` means the file did not exist at snapshot time: restoring such
    /// a checkpoint removes (trashes) the file instead of rewriting content.
    pub has_original: bool,
    pub created_at: String,
}

/// Honest, runtime-detected capability report. The UI shows unconfigured
/// capabilities with setup instructions rather than pretending they work.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Capabilities {
    pub version: String,
    /// True when an OpenRouter API key is available server-side.
    pub openrouter: bool,
    /// True when the LLM provider is a local mock (testing/demo mode).
    pub mock_llm: bool,
    /// True when the server runs with session-scoped diagnostics enabled
    /// (env-only `VORTEX_DIAGNOSTICS=1`); the browser reporter activates
    /// only when this is set.
    #[serde(default)]
    pub diagnostics: bool,
    pub search: bool,
    pub search_provider: String,
    pub search_hint: String,
    pub browser: bool,
    pub browser_hint: String,
    pub commands: bool,
    pub previews: bool,
    pub workspace: bool,
    pub subagents: bool,
    pub max_concurrent_subagents: usize,
}

/// What kind of browser-side failure a report describes. Fixed enum: the
/// server rejects anything else, so no free text can ever reach the logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BrowserEventKind {
    /// An uncaught error.
    Error,
    /// An unhandled promise rejection.
    Rejection,
    /// A resource (script/asset) failed to load.
    Resource,
    /// A panic location registered by the WASM panic hook.
    Panic,
}

impl BrowserEventKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            BrowserEventKind::Error => "error",
            BrowserEventKind::Rejection => "rejection",
            BrowserEventKind::Resource => "resource",
            BrowserEventKind::Panic => "panic",
        }
    }
}

/// Which instrumented boundary produced the report. Fixed enum, no payloads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BrowserOperation {
    /// Pre-WASM bootstrap listener (index.html).
    Bootstrap,
    /// `api.rs` transport failure (network error).
    ApiTransport,
    /// `api.rs` unexpected HTTP status.
    ApiStatus,
    /// `api.rs` JSON decode failure.
    ApiDecode,
    /// `sse.rs` connect failure.
    SseConnect,
    /// `sse.rs` frame decode failure.
    SseDecode,
    /// `sse.rs` reconnect scheduling.
    SseReconnect,
    /// `sse.rs` replay-gap/lagged detection.
    SseGap,
    /// `sse.rs` snapshot resync.
    SseResync,
    /// WASM panic hook.
    Panic,
}

impl BrowserOperation {
    pub fn as_str(&self) -> &'static str {
        match self {
            BrowserOperation::Bootstrap => "bootstrap",
            BrowserOperation::ApiTransport => "api_transport",
            BrowserOperation::ApiStatus => "api_status",
            BrowserOperation::ApiDecode => "api_decode",
            BrowserOperation::SseConnect => "sse_connect",
            BrowserOperation::SseDecode => "sse_decode",
            BrowserOperation::SseReconnect => "sse_reconnect",
            BrowserOperation::SseGap => "sse_gap",
            BrowserOperation::SseResync => "sse_resync",
            BrowserOperation::Panic => "panic",
        }
    }
}

/// Bounded, numeric-only source-location metadata. Deliberately carries no
/// file names or text: browsers may attach arbitrary strings there.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrowserSourceLocation {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub column: Option<u32>,
}

/// One sanitized browser diagnostic report (`POST /api/diagnostics`).
///
/// The shape is intentionally tiny: fixed enums, an optional HTTP status,
/// strictly validated ids, and numeric-only location data. No message,
/// stack, or URL fields exist so that neither the browser nor a malicious
/// local client can inject content into server logs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrowserReport {
    pub event: BrowserEventKind,
    pub operation: BrowserOperation,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    /// Server-generated request id echoed back (strict UUID on the server).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<BrowserSourceLocation>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browser_report_roundtrip_with_all_fields() {
        let report = BrowserReport {
            event: BrowserEventKind::Rejection,
            operation: BrowserOperation::SseGap,
            status: Some(503),
            request_id: Some("00000000-0000-4000-8000-000000000001".to_string()),
            run_id: Some("00000000-0000-4000-8000-000000000002".to_string()),
            location: Some(BrowserSourceLocation {
                line: Some(12),
                column: Some(34),
            }),
        };
        let json = serde_json::to_string(&report).unwrap();
        let back: BrowserReport = serde_json::from_str(&json).unwrap();
        assert_eq!(back, report);
        assert_eq!(back.event.as_str(), "rejection");
        assert_eq!(back.operation.as_str(), "sse_gap");
    }

    #[test]
    fn browser_report_rejects_unknown_fields_and_bad_enums() {
        let base = r#"{"event":"error","operation":"panic"}"#;
        assert!(serde_json::from_str::<BrowserReport>(base).is_ok());
        assert!(serde_json::from_str::<BrowserReport>(
            r#"{"event":"error","operation":"panic","message":"x"}"#
        )
        .is_err());
        assert!(serde_json::from_str::<BrowserReport>(
            r#"{"event":"explode","operation":"panic"}"#
        )
        .is_err());
        assert!(serde_json::from_str::<BrowserReport>(
            r#"{"event":"error","operation":"panic","location":{"file":"a.js"}}"#
        )
        .is_err());
        // Empty report and non-object input are rejected.
        assert!(serde_json::from_str::<BrowserReport>("{}").is_err());
        assert!(serde_json::from_str::<BrowserReport>("null").is_err());
    }

    #[test]
    fn capabilities_defaults_diagnostics_to_false_for_old_payloads() {
        let json = r#"{"version":"x","openrouter":true,"mock_llm":false,"search":false,
            "search_provider":"","search_hint":"","browser":false,"browser_hint":"",
            "commands":false,"previews":false,"workspace":false,"subagents":false,
            "max_concurrent_subagents":1}"#;
        let caps: Capabilities = serde_json::from_str(json).unwrap();
        assert!(!caps.diagnostics);
        let caps: Capabilities = serde_json::from_str(&json.replace(
            "\"mock_llm\":false",
            "\"mock_llm\":false,\"diagnostics\":true",
        ))
        .unwrap();
        assert!(caps.diagnostics);
    }
}
