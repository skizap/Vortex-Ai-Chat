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
