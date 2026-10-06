//! Runtime-editable settings (persisted in SQLite, editable from the UI).

use crate::Mode;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[derive(Default)]
pub enum Theme {
    #[default]
    Dark,
    Light,
}

/// Coarse permission profile. Regardless of profile, backend-enforced
/// "consequential" actions (arbitrary commands, deletions, anything the model
/// explicitly flags) always require explicit human approval.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[derive(Default)]
pub enum PermissionProfile {
    /// Read-only tools; no writes, commands, or previews.
    Restricted,
    /// Workspace reads/writes and safe operations allowed; consequential actions gated.
    #[default]
    Standard,
    /// Like Standard, but safe allowlisted commands run without per-run approval.
    /// Consequential actions are still always gated.
    Trusted,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SearchSettings {
    /// Provider id; only "searxng" is currently implemented.
    pub provider: String,
    /// Base URL of the configured search service, e.g. a local SearXNG instance.
    pub base_url: String,
    pub max_results: u8,
}

impl Default for SearchSettings {
    fn default() -> Self {
        Self {
            provider: "searxng".into(),
            base_url: String::new(),
            max_results: 6,
        }
    }
}

impl SearchSettings {
    pub fn is_configured(&self) -> bool {
        !self.base_url.trim().is_empty()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ToolSettings {
    pub files: bool,
    pub search: bool,
    pub browser: bool,
    /// Arbitrary command execution. Disabled until the user opts in.
    pub commands: bool,
    pub previews: bool,
    /// Allow the coordinator to spawn sub-agents.
    pub delegation: bool,
}

impl Default for ToolSettings {
    fn default() -> Self {
        Self {
            files: true,
            search: true,
            browser: true,
            commands: false,
            previews: true,
            delegation: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentSettings {
    /// Concurrent sub-agent limit. Enforced server-side by a semaphore;
    /// must never be hard-coded below 6 (product requirement).
    pub max_concurrent: usize,
    /// Allow sub-agents to spawn further sub-agents (counted in the same limit).
    pub recursive: bool,
    /// Max model round-trips per run.
    pub max_iterations: u32,
    /// Wall-clock budget per run.
    pub run_timeout_secs: u64,
    /// Token budget per run.
    pub max_total_tokens: u64,
    /// How long an approval may sit unanswered before it is auto-denied.
    pub approval_timeout_secs: u64,
}

impl Default for AgentSettings {
    fn default() -> Self {
        Self {
            max_concurrent: 6,
            recursive: false,
            max_iterations: 16,
            run_timeout_secs: 600,
            max_total_tokens: 200_000,
            approval_timeout_secs: 300,
        }
    }
}

impl AgentSettings {
    /// Hard ceiling for the user-configurable concurrency limit.
    pub const MAX_LIMIT_CEILING: usize = 16;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub model: String,
    pub temperature: f32,
    pub max_tokens: u32,
    pub theme: Theme,
    pub default_mode: Mode,
    /// Approved workspace root (absolute path). File tools are refused without one.
    pub workspace: Option<String>,
    /// Command argv prefixes that may run without approval when commands are enabled.
    pub command_allowlist: Vec<Vec<String>>,
    pub search: SearchSettings,
    pub tools: ToolSettings,
    pub profile: PermissionProfile,
    pub agents: AgentSettings,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            model: "openrouter/auto".into(),
            temperature: 0.7,
            max_tokens: 4096,
            theme: Theme::Dark,
            default_mode: Mode::Chat,
            workspace: None,
            command_allowlist: Vec::new(),
            search: SearchSettings::default(),
            tools: ToolSettings::default(),
            profile: PermissionProfile::Standard,
            agents: AgentSettings::default(),
        }
    }
}
