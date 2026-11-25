//! Global UI state (signals provided through Leptos context).

use leptos::prelude::*;
use vortex_types::{
    ApprovalInfo, Capabilities, ConversationSummary, Job, ParkedApproval, PlanStep, Routine,
    RunStatus, Settings, StoredMessage,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Chat,
    Tasks,
    Jobs,
    Settings,
}

#[derive(Debug, Clone)]
pub(crate) enum SearchCheck {
    Untested,
    Testing,
    Finished { message: String, has_results: bool },
}

pub(crate) fn search_key(settings: &Settings) -> (bool, &str) {
    (settings.tools.search, settings.search.base_url.trim())
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
    /// Mobile off-canvas sidebar drawer state (ignored by desktop layout).
    pub sidebar_open: RwSignal<bool>,
    pub conversations: RwSignal<Vec<ConversationSummary>>,
    pub current: RwSignal<Option<String>>,
    pub messages: RwSignal<Vec<StoredMessage>>,
    pub capabilities: RwSignal<Option<Capabilities>>,
    pub settings: RwSignal<Option<Settings>>,
    pub(crate) search_check: RwSignal<SearchCheck>,
    pub(crate) search_revision: RwSignal<u64>,
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
    // Jobs tab state (durable routines / scheduled jobs / parked approvals).
    pub jobs: RwSignal<Vec<Job>>,
    pub routines: RwSignal<Vec<Routine>>,
    pub jobs_inbox: RwSignal<Vec<ParkedApproval>>,
    pub selected_job: RwSignal<Option<String>>,
    /// False until the first successful job-list fetch (honest empty states).
    pub jobs_loaded: RwSignal<bool>,
    /// Staleness guard for jobs-tab list fetches (mirrors `search_revision`):
    /// bumping it discards in-flight list results from a previous tab visit.
    pub jobs_revision: RwSignal<u64>,
    pub error_banner: RwSignal<Option<String>>,
    /// Files staged in the composer (uploading / ready / failed).
    pub pending_attachments: RwSignal<Vec<UploadChip>>,
    parked_attachments: RwSignal<std::collections::HashMap<String, Vec<UploadChip>>>,
    pub chat_drafts: RwSignal<std::collections::HashMap<String, String>>,
    pub chat_sending: RwSignal<bool>,
    pub model_saving: RwSignal<bool>,
    /// Requested model of the subscribed coordinator, independent of the next-turn preference.
    pub run_model: RwSignal<Option<(String, String)>>,
    pub session_usage: RwSignal<crate::session_usage::SessionUsage>,
}

impl AppCtx {
    pub(crate) fn replace_settings(self, saved: Settings) {
        if self
            .settings
            .with_untracked(|old| old.as_ref().map(search_key) != Some(search_key(&saved)))
        {
            self.search_revision.update(|revision| *revision += 1);
            self.search_check.set(SearchCheck::Untested);
        }
        self.settings.set(Some(saved));
    }

    /// Invalidate in-flight jobs-tab list fetches: results captured against
    /// an older revision are discarded when they arrive (same staleness
    /// pattern as `search_revision` in `replace_settings`).
    pub(crate) fn bump_jobs_revision(self) {
        self.jobs_revision.update(|revision| *revision += 1);
    }

    pub fn new() -> Self {
        let ctx = Self {
            tab: RwSignal::new(Tab::Chat),
            sidebar_open: RwSignal::new(false),
            conversations: RwSignal::new(Vec::new()),
            current: RwSignal::new(None),
            messages: RwSignal::new(Vec::new()),
            capabilities: RwSignal::new(None),
            settings: RwSignal::new(None),
            search_check: RwSignal::new(SearchCheck::Untested),
            search_revision: RwSignal::new(0),
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
            jobs: RwSignal::new(Vec::new()),
            routines: RwSignal::new(Vec::new()),
            jobs_inbox: RwSignal::new(Vec::new()),
            selected_job: RwSignal::new(None),
            jobs_loaded: RwSignal::new(false),
            jobs_revision: RwSignal::new(0),
            error_banner: RwSignal::new(None),
            pending_attachments: RwSignal::new(Vec::new()),
            parked_attachments: RwSignal::new(std::collections::HashMap::new()),
            chat_drafts: RwSignal::new(std::collections::HashMap::new()),
            chat_sending: RwSignal::new(false),
            model_saving: RwSignal::new(false),
            run_model: RwSignal::new(None),
            session_usage: RwSignal::new(crate::session_usage::SessionUsage::default()),
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

    pub fn select_attachment_draft(&self, next: &str) {
        let previous = self.current.get_untracked();
        let chips = self.pending_attachments.get_untracked();
        let mut restored = Vec::new();
        self.parked_attachments.update(|drafts| {
            if let Some(previous) = previous {
                if !chips.is_empty() {
                    drafts.insert(previous, chips);
                }
            }
            restored = drafts.remove(next).unwrap_or_default();
        });
        self.pending_attachments.set(restored);
    }

    pub fn has_attachment_chip(&self, conversation: &str, id: u64) -> bool {
        let has_chip = |chips: &[UploadChip]| chips.iter().any(|chip| chip.local_id == id);
        if self.current.get_untracked().as_deref() == Some(conversation) {
            self.pending_attachments
                .with_untracked(|chips| has_chip(chips))
        } else {
            self.parked_attachments.with_untracked(|drafts| {
                drafts
                    .get(conversation)
                    .is_some_and(|chips| has_chip(chips))
            })
        }
    }

    pub fn update_attachment_draft(
        &self,
        conversation: &str,
        update: impl FnOnce(&mut Vec<UploadChip>),
    ) {
        if self.current.get_untracked().as_deref() == Some(conversation) {
            self.pending_attachments.update(update);
        } else {
            self.parked_attachments.update(|drafts| {
                if let Some(chips) = drafts.get_mut(conversation) {
                    update(chips);
                }
            });
        }
    }

    pub fn clear_attachment_draft(&self, conversation: &str) {
        self.parked_attachments.update(|drafts| {
            drafts.remove(conversation);
        });
        if self.current.get_untracked().as_deref() == Some(conversation) {
            self.pending_attachments.set(Vec::new());
        }
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
