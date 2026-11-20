//! Global UI state (signals provided through Leptos context).

use leptos::prelude::*;
use vortex_types::{
    ApprovalInfo, Capabilities, ConversationSummary, PlanStep, RunStatus, Settings, StoredMessage,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Chat,
    Tasks,
    Settings,
}

/// A live tool call as displayed in the task panel.
#[derive(Debug, Clone)]
pub struct ToolCallDisplay {
    pub call_id: String,
    pub tool: String,
    pub args: serde_json::Value,
    pub status: RunStatus,
    pub summary: String,
    pub diff: Option<String>,
    pub running: bool,
}

#[derive(Debug, Clone)]
pub struct AgentDisplay {
    pub run_id: String,
    pub agent: String,
    pub task: String,
    pub model: String,
    pub status: RunStatus,
    pub usage: Option<vortex_types::Usage>,
}

/// A file staged in the composer before sending.
#[derive(Debug, Clone)]
pub struct UploadChip {
    pub local_id: u64,
    pub filename: String,
    pub size_bytes: u64,
    pub state: UploadState,
}

#[derive(Debug, Clone)]
pub enum UploadState {
    Uploading,
    Ready(vortex_types::AttachmentInfo),
    Error(String),
}

/// All fields are Copy `RwSignal`s, so the context itself is `Copy` —
/// required for Leptos 0.8 view closures (which re-run and must only
/// capture `Copy` state).
#[derive(Debug, Clone, Copy)]
pub struct AppCtx {
    pub tab: RwSignal<Tab>,
    pub conversations: RwSignal<Vec<ConversationSummary>>,
    pub current: RwSignal<Option<String>>,
    pub messages: RwSignal<Vec<StoredMessage>>,
    pub capabilities: RwSignal<Option<Capabilities>>,
    pub settings: RwSignal<Option<Settings>>,
    pub search: RwSignal<String>,
    pub mode: RwSignal<vortex_types::Mode>,
    pub toast: RwSignal<Option<String>>,
    // Active run state
    pub run_id: RwSignal<Option<String>>,
    pub stream_text: RwSignal<String>,
    /// Throttled mirror of `stream_text` for the streaming bubble: updated at
    /// most every 125 ms (App-level interval), so markdown + syntax highlight
    /// re-render ≤8×/s instead of on every SSE delta.
    pub stream_display: RwSignal<String>,
    pub run_status: RwSignal<Option<RunStatus>>,
    pub plan: RwSignal<Vec<PlanStep>>,
    pub tool_calls: RwSignal<Vec<ToolCallDisplay>>,
    pub agents: RwSignal<Vec<AgentDisplay>>,
    pub approvals: RwSignal<Vec<ApprovalInfo>>,
    pub error_banner: RwSignal<Option<String>>,
    /// Files staged in the composer (uploading / ready / failed).
    pub pending_attachments: RwSignal<Vec<UploadChip>>,
}

impl AppCtx {
    pub fn new() -> Self {
        let ctx = Self {
            tab: RwSignal::new(Tab::Chat),
            conversations: RwSignal::new(Vec::new()),
            current: RwSignal::new(None),
            messages: RwSignal::new(Vec::new()),
            capabilities: RwSignal::new(None),
            settings: RwSignal::new(None),
            search: RwSignal::new(String::new()),
            mode: RwSignal::new(vortex_types::Mode::Chat),
            toast: RwSignal::new(None),
            run_id: RwSignal::new(None),
            stream_text: RwSignal::new(String::new()),
            stream_display: RwSignal::new(String::new()),
            run_status: RwSignal::new(None),
            plan: RwSignal::new(Vec::new()),
            tool_calls: RwSignal::new(Vec::new()),
            agents: RwSignal::new(Vec::new()),
            approvals: RwSignal::new(Vec::new()),
            error_banner: RwSignal::new(None),
            pending_attachments: RwSignal::new(Vec::new()),
        };
        // Default the mode from the persisted settings on FIRST load only.
        // Re-applying on every settings write used to reset the user's mode
        // selection mid-conversation (e.g. after a theme toggle saved
        // settings, the response overwrote the mode signal).
        let settings_sig = ctx.settings;
        let mode_sig = ctx.mode;
        let initialized = std::cell::Cell::new(false);
        Effect::new(move || {
            if let Some(s) = settings_sig.get() {
                if !initialized.get() {
                    initialized.set(true);
                    mode_sig.set(s.default_mode);
                }
            }
        });
        ctx
    }

    pub fn set_toast(&self, msg: impl Into<String>) {
        let msg: String = msg.into();
        let toast = self.toast;
        let for_timeout = msg.clone();
        toast.set(Some(msg));
        set_timeout(
            move || {
                toast.update(|t| {
                    if t.as_deref() == Some(for_timeout.as_str()) {
                        *t = None;
                    }
                });
            },
            std::time::Duration::from_secs(4),
        );
    }
}

impl Default for AppCtx {
    fn default() -> Self {
        Self::new()
    }
}

/// Apply the theme on the <html> element and persist it locally.
pub fn apply_theme(theme: vortex_types::Theme) {
    // No document (e.g. non-browser context): nothing to theme, don't trap.
    let Some(doc) = web_sys::window().and_then(|w| w.document()) else {
        return;
    };
    if let Some(html) = doc.document_element() {
        let _ = html.set_attribute(
            "data-theme",
            match theme {
                vortex_types::Theme::Dark => "dark",
                vortex_types::Theme::Light => "light",
            },
        );
    }
}
