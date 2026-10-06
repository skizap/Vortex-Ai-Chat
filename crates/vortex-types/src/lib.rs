//! Shared, serializable types for Vortex-Ai-Chat.
//!
//! This crate is intentionally dependency-light (serde only) so it can be
//! compiled into both the native server and the WebAssembly UI, giving both
//! sides one typed contract for chat, runs, events, and settings.

pub mod chat;
pub mod dto;
pub mod events;
pub mod settings;

pub use chat::{ChatMessage, ModelInfo, Role, ToolCall, Usage};
pub use dto::{
    ApprovalInfo, ApprovalStatus, Capabilities, ChatRequestDto, ConversationSummary, PreviewInfo,
    RunInfo, StoredMessage, ToolCallRecord,
};
pub use events::{PlanStep, RunEvent, RunStatus, StepStatus};
pub use settings::{
    AgentSettings, PermissionProfile, SearchSettings, Settings, Theme, ToolSettings,
};

/// Interaction modes shown in the UI. Each mode changes which tools the
/// backend will expose to the model for a given run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
#[derive(Default)]
pub enum Mode {
    #[default]
    Chat,
    Research,
    Coding,
    Browser,
}

impl Mode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Mode::Chat => "chat",
            Mode::Research => "research",
            Mode::Coding => "coding",
            Mode::Browser => "browser",
        }
    }
}

impl std::fmt::Display for Mode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}
