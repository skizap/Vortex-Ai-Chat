//! Global UI state (signals provided through Leptos context).

use leptos::prelude::*;
use vortex_types::{ApprovalInfo, Capabilities, ConversationSummary, PlanStep, RunStatus, Settings, StoredMessage};

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

#[derive(Clone)]
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
    pub run_status: RwSignal<Option<RunStatus>>,
    pub plan: RwSignal<Vec<PlanStep>>,
    pub tool_calls: RwSignal<Vec<ToolCallDisplay>>,
    pub agents: RwSignal<Vec<AgentDisplay>>,
    pub approvals: RwSignal<Vec<ApprovalInfo>>,
    pub error_banner: RwSignal<Option<String>>,
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
            run_status: RwSignal::new(None),
            plan: RwSignal::new(Vec::new()),
            tool_calls: RwSignal::new(Vec::new()),
            agents: RwSignal::new(Vec::new()),
            approvals: RwSignal::new(Vec::new()),
            error_banner: RwSignal::new(None),
        };
        // Default the mode from the persisted settings once loaded.
        let settings_sig = ctx.settings;
        let mode_sig = ctx.mode;
        Effect::new(move || {
            if let Some(s) = settings_sig.get() {
                mode_sig.set(s.default_mode);
            }
        });
        ctx
    }

    pub fn set_toast(&self, msg: impl Into<String>) {
        let msg: String = msg.into();
        let toast = self.toast;
        toast.set(Some(msg));
        set_timeout(
            move || {
                toast.update(|t| {
                    if t.as_deref() == Some(msg.as_str()) {
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
    let doc = web_sys::window()
        .and_then(|w| w.document())
        .expect("document available");
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
