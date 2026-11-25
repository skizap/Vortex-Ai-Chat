//! Durable routines, jobs, schedules, and occurrences: the shared contract
//! for autonomous agent work. These DTOs are frozen at Phase 0; changes go
//! through the lead, not ad-hoc edits.
//!
//! Safety model (enforced server-side, never by prompt):
//! - Routines drafted by agents land as `draft`; only a human endpoint may
//!   activate them. Jobs pin an exact routine version.
//! - Job-origin runs never receive routine/job-authoring tools.
//! - Consequential actions stay human-gated; job runs park until decided.

use crate::dto::ApprovalInfo;
use crate::Mode;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoutineStatus {
    Draft,
    Active,
    Archived,
}

impl RoutineStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            RoutineStatus::Draft => "draft",
            RoutineStatus::Active => "active",
            RoutineStatus::Archived => "archived",
        }
    }
}

/// A reusable, versioned instruction template. Agents may draft routines;
/// only a human may activate one.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Routine {
    pub id: String,
    pub name: String,
    pub description: String,
    pub status: RoutineStatus,
    /// Highest version number written so far (versions start at 1).
    pub current_version: u32,
    pub created_at: String,
    pub updated_at: String,
}

/// One immutable version of a routine's instructions. Jobs pin a version, so
/// editing a routine never silently changes what a scheduled job runs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutineVersion {
    pub routine_id: String,
    pub version: u32,
    pub instructions: String,
    pub mode: Mode,
    /// `None` means Vortex Auto resolution at run time.
    pub model: Option<String>,
    pub created_at: String,
}

/// Input for creating a routine (version 1 is written atomically).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewRoutine {
    pub name: String,
    pub description: String,
    pub instructions: String,
    pub mode: Mode,
    pub model: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    /// Proposed (by an agent or the wizard) but not yet human-activated.
    Draft,
    Active,
    Paused,
    /// Tripped by consecutive failed/parked-expired occurrences; a human must
    /// resume it explicitly.
    CircuitBroken,
}

impl JobStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            JobStatus::Draft => "draft",
            JobStatus::Active => "active",
            JobStatus::Paused => "paused",
            JobStatus::CircuitBroken => "circuit_broken",
        }
    }
}

/// What to do with ticks that fell due while Vortex was not running.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum MissedPolicy {
    /// Fire exactly one catch-up occurrence for any number of missed ticks.
    #[default]
    RunOnceIfMissed,
    /// Drop missed ticks entirely; wait for the next live tick.
    Skip,
}

/// What to do when a tick fires while an earlier occurrence is still live.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum OverlapPolicy {
    /// Skip the new tick (default).
    #[default]
    Skip,
    /// Keep at most one queued occurrence behind the live one.
    QueueOne,
}

/// A persisted cron schedule. Fires only while Vortex is running.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ScheduleSpec {
    /// Standard 5-field cron expression (minute hour day-of-month month
    /// day-of-week).
    pub cron: String,
    /// IANA timezone name, or "local" for the server's local timezone.
    pub tz: String,
    pub missed_policy: MissedPolicy,
}

impl Default for ScheduleSpec {
    fn default() -> Self {
        Self {
            cron: "0 9 * * *".into(),
            tz: "local".into(),
            missed_policy: MissedPolicy::default(),
        }
    }
}

/// Per-job guardrails. Conservative defaults so unattended loops work out of
/// the box; every cap is visible and editable per job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct JobBudgets {
    /// Max model round-trips per occurrence.
    pub max_turns: u32,
    /// Token budget per occurrence.
    pub max_tokens: u64,
    /// Max occurrences started per rolling day.
    pub daily_occurrence_cap: u32,
    /// Wall-clock budget per occurrence.
    pub run_timeout_secs: u64,
}

impl Default for JobBudgets {
    fn default() -> Self {
        Self {
            max_turns: 12,
            max_tokens: 120_000,
            daily_occurrence_cap: 24,
            run_timeout_secs: 600,
        }
    }
}

/// A durable job: a pinned routine version plus schedule, workspace, and
/// guardrails. Each job owns a dedicated conversation so its runs' transcripts,
/// plans, and checkpoints persist through the existing run machinery.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub id: String,
    pub name: String,
    pub routine_id: String,
    pub routine_version: u32,
    pub status: JobStatus,
    /// `None` = manual-only job (run on demand, never scheduled).
    pub schedule: Option<ScheduleSpec>,
    pub overlap: OverlapPolicy,
    pub budgets: JobBudgets,
    /// `None` = per-job scratch directory under the data dir.
    pub workspace: Option<String>,
    /// Dedicated conversation for this job's runs.
    pub conversation_id: String,
    /// Human steering note applied to the next occurrence/continuation.
    pub steer_note: Option<String>,
    /// Consecutive failed/parked-expired occurrences (circuit-breaker input).
    pub consecutive_failures: u32,
    /// Supervisor-computed next fire time (RFC 3339), for the UI.
    pub next_run_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// Input for creating a job. The store creates the job's dedicated
/// conversation atomically.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewJob {
    pub name: String,
    pub routine_id: String,
    pub routine_version: u32,
    pub schedule: Option<ScheduleSpec>,
    pub overlap: OverlapPolicy,
    pub budgets: JobBudgets,
    pub workspace: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OccurrenceStatus {
    Scheduled,
    Running,
    WaitingApproval,
    Succeeded,
    Failed,
    Interrupted,
    Skipped,
}

impl OccurrenceStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            OccurrenceStatus::Scheduled => "scheduled",
            OccurrenceStatus::Running => "running",
            OccurrenceStatus::WaitingApproval => "waiting_approval",
            OccurrenceStatus::Succeeded => "succeeded",
            OccurrenceStatus::Failed => "failed",
            OccurrenceStatus::Interrupted => "interrupted",
            OccurrenceStatus::Skipped => "skipped",
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            OccurrenceStatus::Succeeded
                | OccurrenceStatus::Failed
                | OccurrenceStatus::Interrupted
                | OccurrenceStatus::Skipped
        )
    }
}

/// One scheduled (or catch-up/manual) execution of a job. The unique
/// `(job_id, scheduled_for)` constraint is the double-fire guard.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Occurrence {
    pub id: String,
    pub job_id: String,
    /// RFC 3339 tick this occurrence belongs to (catch-up occurrences use the
    /// most recent missed tick; manual runs use their start time).
    pub scheduled_for: String,
    pub status: OccurrenceStatus,
    pub run_id: Option<String>,
    /// Human-readable outcome detail (failure reason, skip reason, ...).
    pub reason: Option<String>,
    /// Set when a possibly-half-executed consequential step blocks automatic
    /// continuation; a human must review before the job proceeds.
    pub needs_human_review: bool,
    pub created_at: String,
    pub updated_at: String,
}

/// Where a run came from. Job-origin runs park approvals and never receive
/// routine/job-authoring tools; delegates inherit their root's origin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RunOrigin {
    #[default]
    Interactive,
    Job {
        job_id: String,
        occurrence_id: String,
    },
}

impl RunOrigin {
    pub fn is_job(&self) -> bool {
        matches!(self, RunOrigin::Job { .. })
    }
}

/// How an approval wait behaves. Interactive runs keep today's timeout;
/// job-origin runs park until a human decides (no auto-deny). The gate stays
/// human-only under both policies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "policy", content = "value", rename_all = "snake_case")]
pub enum ApprovalPolicy {
    TimeoutSecs(u64),
    ParkUntilDecided,
}

impl Default for ApprovalPolicy {
    fn default() -> Self {
        ApprovalPolicy::TimeoutSecs(300)
    }
}

/// A pending approval that belongs to a parked job occurrence (inbox feed).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParkedApproval {
    pub approval: ApprovalInfo,
    pub job_id: String,
    pub job_name: String,
    pub occurrence_id: String,
}

/// Supervisor-emitted events streamed over the jobs SSE channel. Mirrors the
/// hub's seq-stamped replay pattern (seq lives in the channel, not the DTO).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum JobEvent {
    JobUpdated {
        job: Job,
    },
    RoutineUpdated {
        routine: Routine,
    },
    OccurrenceUpdated {
        occurrence: Occurrence,
    },
    ApprovalParked {
        job_id: String,
        occurrence_id: String,
        approval_id: String,
        run_id: String,
        title: String,
    },
    JobCircuitBroken {
        job_id: String,
        reason: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_shapes_are_snake_case() {
        assert_eq!(
            serde_json::to_value(JobStatus::CircuitBroken).unwrap(),
            "circuit_broken"
        );
        assert_eq!(
            serde_json::to_value(MissedPolicy::RunOnceIfMissed).unwrap(),
            "run_once_if_missed"
        );
        assert_eq!(
            serde_json::to_value(OverlapPolicy::QueueOne).unwrap(),
            "queue_one"
        );
        assert_eq!(
            serde_json::to_value(OccurrenceStatus::WaitingApproval).unwrap(),
            "waiting_approval"
        );
        let origin = RunOrigin::Job {
            job_id: "j".into(),
            occurrence_id: "o".into(),
        };
        let json = serde_json::to_value(&origin).unwrap();
        assert_eq!(json["kind"], "job");
        assert_eq!(json["job_id"], "j");
        let back: RunOrigin = serde_json::from_value(json).unwrap();
        assert_eq!(back, origin);
        let policy = ApprovalPolicy::TimeoutSecs(42);
        let json = serde_json::to_value(policy).unwrap();
        assert_eq!(json["policy"], "timeout_secs");
        assert_eq!(json["value"], 42);
        let back: ApprovalPolicy = serde_json::from_value(json).unwrap();
        assert_eq!(back, policy);
        let park = serde_json::to_value(ApprovalPolicy::ParkUntilDecided).unwrap();
        assert_eq!(park["policy"], "park_until_decided");
    }

    #[test]
    fn defaults_are_conservative() {
        let b = JobBudgets::default();
        assert!(b.max_turns <= 16);
        assert!(b.max_tokens <= 200_000);
        assert!(b.daily_occurrence_cap <= 48);
        assert_eq!(MissedPolicy::default(), MissedPolicy::RunOnceIfMissed);
        assert_eq!(OverlapPolicy::default(), OverlapPolicy::Skip);
        assert_eq!(RunOrigin::default(), RunOrigin::Interactive);
        assert!(!RunOrigin::default().is_job());
    }

    #[test]
    fn schedule_spec_decodes_partial_payloads() {
        let s: ScheduleSpec = serde_json::from_str(r#"{"cron":"*/5 * * * *"}"#).unwrap();
        assert_eq!(s.tz, "local");
        assert_eq!(s.missed_policy, MissedPolicy::RunOnceIfMissed);
    }
}
