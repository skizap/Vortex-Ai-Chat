//! Live run events streamed to the UI over SSE.

use crate::chat::Usage;
use serde::{Deserialize, Serialize};

/// Lifecycle status of a run (main chat run or sub-agent run).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Queued,
    Running,
    WaitingApproval,
    Completed,
    Failed,
    Cancelled,
    /// Set by a fresh server start for runs that were live when the process stopped.
    Interrupted,
}

impl RunStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            RunStatus::Queued => "queued",
            RunStatus::Running => "running",
            RunStatus::WaitingApproval => "waiting_approval",
            RunStatus::Completed => "completed",
            RunStatus::Failed => "failed",
            RunStatus::Cancelled => "cancelled",
            RunStatus::Interrupted => "interrupted",
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            RunStatus::Completed | RunStatus::Failed | RunStatus::Cancelled | RunStatus::Interrupted
        )
    }
}

impl std::fmt::Display for RunStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    Pending,
    InProgress,
    Done,
    Skipped,
}

/// One step of a plan the model is following for a run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanStep {
    pub title: String,
    pub status: StepStatus,
}

/// Events emitted while a run executes. All events carry the id of the run
/// that produced them; the server also fans child events up to ancestor runs
/// so a single subscription covers a whole agent tree.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RunEvent {
    RunStarted {
        run_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_run_id: Option<String>,
        agent: String,
        model: String,
        #[serde(default)]
        task: String,
        depth: u32,
    },
    TextDelta {
        run_id: String,
        delta: String,
    },
    PlanUpdated {
        run_id: String,
        steps: Vec<PlanStep>,
    },
    ToolCallStarted {
        run_id: String,
        call_id: String,
        tool: String,
        args: serde_json::Value,
    },
    ToolCallFinished {
        run_id: String,
        call_id: String,
        ok: bool,
        #[serde(default)]
        summary: String,
        /// Unified diff of file changes, when the tool produced one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        diff: Option<String>,
    },
    ApprovalRequested {
        approval_id: String,
        run_id: String,
        agent: String,
        title: String,
        payload: String,
    },
    ApprovalResolved {
        approval_id: String,
        run_id: String,
        approved: bool,
    },
    UsageUpdate {
        run_id: String,
        usage: Usage,
    },
    PreviewStarted {
        run_id: String,
        preview_id: String,
        url: String,
    },
    StatusChanged {
        run_id: String,
        status: RunStatus,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    RunFinished {
        run_id: String,
        status: RunStatus,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
}

impl RunEvent {
    /// The run this event originated from.
    pub fn run_id(&self) -> &str {
        match self {
            RunEvent::RunStarted { run_id, .. }
            | RunEvent::TextDelta { run_id, .. }
            | RunEvent::PlanUpdated { run_id, .. }
            | RunEvent::ToolCallStarted { run_id, .. }
            | RunEvent::ToolCallFinished { run_id, .. }
            | RunEvent::ApprovalRequested { run_id, .. }
            | RunEvent::ApprovalResolved { run_id, .. }
            | RunEvent::UsageUpdate { run_id, .. }
            | RunEvent::PreviewStarted { run_id, .. }
            | RunEvent::StatusChanged { run_id, .. }
            | RunEvent::RunFinished { run_id, .. } => run_id,
        }
    }
}
