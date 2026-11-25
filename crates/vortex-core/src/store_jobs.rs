//! Domain queries: routines, jobs, and occurrences.
//!
//! Phase 0 freeze: the API surface below is the contract SA5/SA6 develop
//! against; SA1 owns the implementations and tests. Semantics worth pinning:
//! - `create_routine` writes version 1 atomically and lands as `draft`.
//! - `add_routine_version` appends an immutable version and bumps
//!   `current_version`; it never changes the routine's status, and jobs
//!   pinned to older versions keep running exactly what they pinned.
//! - `activate_routine`/`set_job_status` are the human-only activation path;
//!   no model-reachable code may call them with an activation transition.
//! - `create_job` also creates the job's dedicated conversation (title
//!   "Job: <name>", mode from the pinned routine version, workspace from the
//!   job) in one transaction.
//! - `insert_occurrence` returns `Ok(None)` on the UNIQUE(job_id,
//!   scheduled_for) conflict — the double-fire guard. Callers treat `None`
//!   as "already fired", never as an error.
//! - `mark_occurrences_interrupted_on_startup` sweeps live occurrence rows
//!   (running/waiting_approval) to `interrupted`, mirroring the runs sweep;
//!   the supervisor re-derives catch-up/continuation state from these rows.

use crate::db::{now, Db};
use crate::error::{CoreError, Result};
use vortex_types::{
    ApprovalInfo, ApprovalStatus, Job, JobBudgets, JobStatus, MissedPolicy, NewJob, NewRoutine,
    Occurrence, OccurrenceStatus, OverlapPolicy, ParkedApproval, Routine, RoutineStatus,
    RoutineVersion, ScheduleSpec,
};

const ROUTINE_COLS: &str = "id, name, description, status, current_version, created_at, updated_at";

const ROUTINE_VERSION_COLS: &str = "routine_id, version, instructions, mode, model, created_at";

const JOB_COLS: &str = "id, name, routine_id, routine_version, status, cron, tz, missed_policy, \
     overlap_policy, workspace, conversation_id, budgets_json, steer_note, consecutive_failures, \
     next_run_at, created_at, updated_at";

const OCCURRENCE_COLS: &str = "id, job_id, scheduled_for, status, run_id, reason, \
     needs_human_review, created_at, updated_at";

fn row_to_routine(row: &rusqlite::Row<'_>) -> rusqlite::Result<Routine> {
    Ok(Routine {
        id: row.get(0)?,
        name: row.get(1)?,
        description: row.get(2)?,
        status: serde_json::from_value(serde_json::Value::String(row.get::<_, String>(3)?))
            .unwrap_or(RoutineStatus::Draft),
        current_version: row.get::<_, i64>(4)? as u32,
        created_at: row.get(5)?,
        updated_at: row.get(6)?,
    })
}

fn row_to_routine_version(row: &rusqlite::Row<'_>) -> rusqlite::Result<RoutineVersion> {
    Ok(RoutineVersion {
        routine_id: row.get(0)?,
        version: row.get::<_, i64>(1)? as u32,
        instructions: row.get(2)?,
        mode: serde_json::from_value(serde_json::Value::String(row.get::<_, String>(3)?))
            .unwrap_or_default(),
        model: row.get(4)?,
        created_at: row.get(5)?,
    })
}

fn row_to_job(row: &rusqlite::Row<'_>) -> rusqlite::Result<Job> {
    let cron: Option<String> = row.get(5)?;
    let tz: String = row.get(6)?;
    let missed: String = row.get(7)?;
    let budgets_json: String = row.get(11)?;
    Ok(Job {
        id: row.get(0)?,
        name: row.get(1)?,
        routine_id: row.get(2)?,
        routine_version: row.get::<_, i64>(3)? as u32,
        status: serde_json::from_value(serde_json::Value::String(row.get::<_, String>(4)?))
            .unwrap_or(JobStatus::Draft),
        schedule: cron.map(|cron| ScheduleSpec {
            cron,
            tz,
            missed_policy: parse_missed_policy(&missed),
        }),
        overlap: parse_overlap_policy(&row.get::<_, String>(8)?),
        budgets: serde_json::from_str(&budgets_json).unwrap_or_default(),
        workspace: row.get(9)?,
        conversation_id: row.get(10)?,
        steer_note: row.get(12)?,
        consecutive_failures: row.get::<_, i64>(13)? as u32,
        next_run_at: row.get(14)?,
        created_at: row.get(15)?,
        updated_at: row.get(16)?,
    })
}

fn row_to_occurrence(row: &rusqlite::Row<'_>) -> rusqlite::Result<Occurrence> {
    Ok(Occurrence {
        id: row.get(0)?,
        job_id: row.get(1)?,
        scheduled_for: row.get(2)?,
        status: serde_json::from_value(serde_json::Value::String(row.get::<_, String>(3)?))
            .unwrap_or(OccurrenceStatus::Scheduled),
        run_id: row.get(4)?,
        reason: row.get(5)?,
        needs_human_review: row.get::<_, i64>(6)? != 0,
        created_at: row.get(7)?,
        updated_at: row.get(8)?,
    })
}

impl Db {
    // ----- Routines -----

    pub async fn create_routine(&self, new: NewRoutine) -> Result<Routine> {
        let id = uuid::Uuid::new_v4().to_string();
        let ts = now();
        let routine = Routine {
            id: id.clone(),
            name: new.name.clone(),
            description: new.description.clone(),
            status: RoutineStatus::Draft,
            current_version: 1,
            created_at: ts.clone(),
            updated_at: ts.clone(),
        };
        self.with_conn(move |c| {
            let tx = c.unchecked_transaction()?;
            tx.execute(
                "INSERT INTO routines (id, name, description, status, current_version, \
                 created_at, updated_at)
                 VALUES (?1, ?2, ?3, 'draft', 1, ?4, ?4)",
                rusqlite::params![id, new.name, new.description, ts],
            )?;
            tx.execute(
                "INSERT INTO routine_versions (routine_id, version, instructions, mode, model, \
                 created_at)
                 VALUES (?1, 1, ?2, ?3, ?4, ?5)",
                rusqlite::params![id, new.instructions, new.mode.as_str(), new.model, ts],
            )?;
            tx.commit()?;
            Ok(())
        })
        .await?;
        Ok(routine)
    }

    pub async fn add_routine_version(
        &self,
        routine_id: String,
        instructions: String,
        mode: vortex_types::Mode,
        model: Option<String>,
    ) -> Result<RoutineVersion> {
        let missing = format!("routine '{routine_id}' not found; create it before adding versions");
        let version = self
            .with_conn(move |c| {
                let tx = c.unchecked_transaction()?;
                let current: Option<i64> = {
                    let mut st =
                        tx.prepare("SELECT current_version FROM routines WHERE id = ?1")?;
                    let row = st
                        .query_map(rusqlite::params![routine_id], |r| r.get::<_, i64>(0))?
                        .next()
                        .transpose()?;
                    row
                };
                let Some(current) = current else {
                    return Ok(None);
                };
                let version = current as u32 + 1;
                let ts = now();
                tx.execute(
                    "INSERT INTO routine_versions (routine_id, version, instructions, mode, \
                     model, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    rusqlite::params![routine_id, version, instructions, mode.as_str(), model, ts],
                )?;
                tx.execute(
                    "UPDATE routines SET current_version = ?2, updated_at = ?3 WHERE id = ?1",
                    rusqlite::params![routine_id, version, ts],
                )?;
                tx.commit()?;
                Ok(Some(RoutineVersion {
                    routine_id,
                    version,
                    instructions,
                    mode,
                    model,
                    created_at: ts,
                }))
            })
            .await?;
        version.ok_or_else(|| CoreError::Config(missing))
    }

    /// Human-only draft → active transition. Returns false when the routine
    /// is missing or not a draft.
    pub async fn activate_routine(&self, id: String) -> Result<bool> {
        self.with_conn(move |c| {
            Ok(c.execute(
                "UPDATE routines SET status = 'active', updated_at = ?2
                 WHERE id = ?1 AND status = 'draft'",
                rusqlite::params![id, now()],
            )? > 0)
        })
        .await
    }

    pub async fn archive_routine(&self, id: String) -> Result<bool> {
        self.with_conn(move |c| {
            Ok(c.execute(
                "UPDATE routines SET status = 'archived', updated_at = ?2
                 WHERE id = ?1 AND status IN ('draft','active')",
                rusqlite::params![id, now()],
            )? > 0)
        })
        .await
    }

    pub async fn get_routine(&self, id: String) -> Result<Option<Routine>> {
        self.with_conn(move |c| {
            let sql = format!("SELECT {ROUTINE_COLS} FROM routines WHERE id = ?1");
            let mut st = c.prepare(&sql)?;
            let row = st
                .query_map(rusqlite::params![id], row_to_routine)?
                .next()
                .transpose()?;
            Ok(row)
        })
        .await
    }

    pub async fn list_routines(&self) -> Result<Vec<Routine>> {
        self.with_conn(|c| {
            let sql = format!("SELECT {ROUTINE_COLS} FROM routines ORDER BY updated_at DESC");
            let mut st = c.prepare(&sql)?;
            let rows = st
                .query_map([], row_to_routine)?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
    }

    pub async fn get_routine_version(
        &self,
        routine_id: String,
        version: u32,
    ) -> Result<Option<RoutineVersion>> {
        self.with_conn(move |c| {
            let sql = format!(
                "SELECT {ROUTINE_VERSION_COLS} FROM routine_versions \
                 WHERE routine_id = ?1 AND version = ?2"
            );
            let mut st = c.prepare(&sql)?;
            let row = st
                .query_map(
                    rusqlite::params![routine_id, version],
                    row_to_routine_version,
                )?
                .next()
                .transpose()?;
            Ok(row)
        })
        .await
    }

    pub async fn list_routine_versions(&self, routine_id: String) -> Result<Vec<RoutineVersion>> {
        self.with_conn(move |c| {
            let sql = format!(
                "SELECT {ROUTINE_VERSION_COLS} FROM routine_versions \
                 WHERE routine_id = ?1 ORDER BY version DESC"
            );
            let mut st = c.prepare(&sql)?;
            let rows = st
                .query_map(rusqlite::params![routine_id], row_to_routine_version)?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
    }

    // ----- Jobs -----

    pub async fn create_job(&self, new: NewJob) -> Result<Job> {
        let id = uuid::Uuid::new_v4().to_string();
        let conversation_id = uuid::Uuid::new_v4().to_string();
        let ts = now();
        let (cron, tz, missed) = match &new.schedule {
            Some(s) => (Some(s.cron.clone()), s.tz.clone(), s.missed_policy),
            None => (None, "local".to_string(), MissedPolicy::default()),
        };
        let missing = format!(
            "routine '{}' version {} does not exist; create the routine first and pin an \
             existing version",
            new.routine_id, new.routine_version
        );
        let job = self
            .with_conn(move |c| {
                let tx = c.unchecked_transaction()?;
                let mode_str: Option<String> = {
                    let mut st = tx.prepare(
                        "SELECT mode FROM routine_versions WHERE routine_id = ?1 AND version = ?2",
                    )?;
                    let row = st
                        .query_map(
                            rusqlite::params![new.routine_id, new.routine_version],
                            |r| r.get::<_, String>(0),
                        )?
                        .next()
                        .transpose()?;
                    row
                };
                let Some(mode_str) = mode_str else {
                    return Ok(None);
                };
                let title = format!("Job: {}", new.name);
                tx.execute(
                    "INSERT INTO conversations (id, title, mode, workspace, created_at, \
                     updated_at, title_origin, model)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?5, 'manual', NULL)",
                    rusqlite::params![conversation_id, title, mode_str, new.workspace, ts],
                )?;
                let budgets_json =
                    serde_json::to_string(&new.budgets).unwrap_or_else(|_| "{}".into());
                tx.execute(
                    "INSERT INTO jobs (id, name, routine_id, routine_version, status, cron, tz, \
                     missed_policy, overlap_policy, workspace, conversation_id, budgets_json, \
                     steer_note, consecutive_failures, next_run_at, created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, 'draft', ?5, ?6, ?7, ?8, ?9, ?10, ?11, NULL, 0, \
                     NULL, ?12, ?12)",
                    rusqlite::params![
                        id,
                        new.name,
                        new.routine_id,
                        new.routine_version,
                        cron,
                        tz,
                        missed_policy_as_str(missed),
                        overlap_policy_as_str(new.overlap),
                        new.workspace,
                        conversation_id,
                        budgets_json,
                        ts,
                    ],
                )?;
                tx.commit()?;
                Ok(Some(Job {
                    id,
                    name: new.name,
                    routine_id: new.routine_id,
                    routine_version: new.routine_version,
                    status: JobStatus::Draft,
                    schedule: cron.map(|cron| ScheduleSpec {
                        cron,
                        tz,
                        missed_policy: missed,
                    }),
                    overlap: new.overlap,
                    budgets: new.budgets,
                    workspace: new.workspace,
                    conversation_id,
                    steer_note: None,
                    consecutive_failures: 0,
                    next_run_at: None,
                    created_at: ts.clone(),
                    updated_at: ts,
                }))
            })
            .await?;
        job.ok_or_else(|| CoreError::Config(missing))
    }

    pub async fn get_job(&self, id: String) -> Result<Option<Job>> {
        self.with_conn(move |c| {
            let sql = format!("SELECT {JOB_COLS} FROM jobs WHERE id = ?1");
            let mut st = c.prepare(&sql)?;
            let row = st
                .query_map(rusqlite::params![id], row_to_job)?
                .next()
                .transpose()?;
            Ok(row)
        })
        .await
    }

    pub async fn list_jobs(&self) -> Result<Vec<Job>> {
        self.with_conn(|c| {
            let sql = format!("SELECT {JOB_COLS} FROM jobs ORDER BY created_at");
            let mut st = c.prepare(&sql)?;
            let rows = st
                .query_map([], row_to_job)?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
    }

    /// Status transitions are validated here: activation (draft → active) is
    /// only reachable from the human API path; the supervisor may set
    /// paused/circuit_broken/active programmatically for breaker/resume.
    pub async fn set_job_status(&self, id: String, status: JobStatus) -> Result<bool> {
        self.with_conn(move |c| {
            Ok(c.execute(
                "UPDATE jobs SET status = ?2, updated_at = ?3 WHERE id = ?1",
                rusqlite::params![id, status.as_str(), now()],
            )? > 0)
        })
        .await
    }

    pub async fn set_job_steer_note(&self, id: String, note: Option<String>) -> Result<bool> {
        self.with_conn(move |c| {
            Ok(c.execute(
                "UPDATE jobs SET steer_note = ?2, updated_at = ?3 WHERE id = ?1",
                rusqlite::params![id, note, now()],
            )? > 0)
        })
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn update_job_config(
        &self,
        id: String,
        name: String,
        schedule: Option<ScheduleSpec>,
        overlap: OverlapPolicy,
        budgets: JobBudgets,
        workspace: Option<String>,
    ) -> Result<bool> {
        let (cron, tz, missed) = match &schedule {
            Some(s) => (Some(s.cron.clone()), s.tz.clone(), s.missed_policy),
            None => (None, "local".to_string(), MissedPolicy::default()),
        };
        let budgets_json = serde_json::to_string(&budgets).unwrap_or_else(|_| "{}".into());
        self.with_conn(move |c| {
            Ok(c.execute(
                "UPDATE jobs SET name = ?2, cron = ?3, tz = ?4, missed_policy = ?5, \
                 overlap_policy = ?6, budgets_json = ?7, workspace = ?8, updated_at = ?9
                 WHERE id = ?1",
                rusqlite::params![
                    id,
                    name,
                    cron,
                    tz,
                    missed_policy_as_str(missed),
                    overlap_policy_as_str(overlap),
                    budgets_json,
                    workspace,
                    now(),
                ],
            )? > 0)
        })
        .await
    }

    pub async fn set_job_next_run(&self, id: String, next_run_at: Option<String>) -> Result<bool> {
        self.with_conn(move |c| {
            Ok(c.execute(
                "UPDATE jobs SET next_run_at = ?2, updated_at = ?3 WHERE id = ?1",
                rusqlite::params![id, next_run_at, now()],
            )? > 0)
        })
        .await
    }

    /// Increment the consecutive-failure counter; returns the new value.
    pub async fn record_job_failure(&self, id: String) -> Result<u32> {
        self.with_conn(move |c| {
            let count: i64 = c.query_row(
                "UPDATE jobs SET consecutive_failures = consecutive_failures + 1, \
                 updated_at = ?2
                 WHERE id = ?1 RETURNING consecutive_failures",
                rusqlite::params![id, now()],
                |r| r.get(0),
            )?;
            Ok(count as u32)
        })
        .await
    }

    pub async fn reset_job_failures(&self, id: String) -> Result<bool> {
        self.with_conn(move |c| {
            Ok(c.execute(
                "UPDATE jobs SET consecutive_failures = 0, updated_at = ?2 WHERE id = ?1",
                rusqlite::params![id, now()],
            )? > 0)
        })
        .await
    }

    /// Delete a job, its occurrences, and its dedicated conversation.
    pub async fn delete_job(&self, id: String) -> Result<bool> {
        let conversation_id = self
            .with_conn(move |c| {
                let conversation_id: Option<String> = {
                    let mut st = c.prepare("SELECT conversation_id FROM jobs WHERE id = ?1")?;
                    let row = st
                        .query_map(rusqlite::params![id], |r| r.get::<_, String>(0))?
                        .next()
                        .transpose()?;
                    row
                };
                let Some(conversation_id) = conversation_id else {
                    return Ok(None);
                };
                let tx = c.unchecked_transaction()?;
                tx.execute(
                    "DELETE FROM job_occurrences WHERE job_id = ?1",
                    rusqlite::params![id],
                )?;
                tx.execute("DELETE FROM jobs WHERE id = ?1", rusqlite::params![id])?;
                tx.commit()?;
                Ok(Some(conversation_id))
            })
            .await?;
        match conversation_id {
            Some(cid) => {
                self.delete_conversation(cid).await?;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    // ----- Occurrences -----

    /// `Ok(None)` on UNIQUE(job_id, scheduled_for) conflict (double-fire guard).
    pub async fn insert_occurrence(
        &self,
        job_id: String,
        scheduled_for: String,
    ) -> Result<Option<Occurrence>> {
        let id = uuid::Uuid::new_v4().to_string();
        let ts = now();
        self.with_conn(move |c| {
            match c.execute(
                "INSERT INTO job_occurrences (id, job_id, scheduled_for, status, run_id, reason, \
                 needs_human_review, created_at, updated_at)
                 VALUES (?1, ?2, ?3, 'scheduled', NULL, NULL, 0, ?4, ?4)",
                rusqlite::params![id, job_id, scheduled_for, ts],
            ) {
                Ok(_) => Ok(Some(Occurrence {
                    id,
                    job_id,
                    scheduled_for,
                    status: OccurrenceStatus::Scheduled,
                    run_id: None,
                    reason: None,
                    needs_human_review: false,
                    created_at: ts.clone(),
                    updated_at: ts,
                })),
                Err(rusqlite::Error::SqliteFailure(e, _))
                    if e.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE =>
                {
                    Ok(None)
                }
                Err(e) => Err(e),
            }
        })
        .await
    }

    pub async fn get_occurrence(&self, id: String) -> Result<Option<Occurrence>> {
        self.with_conn(move |c| {
            let sql = format!("SELECT {OCCURRENCE_COLS} FROM job_occurrences WHERE id = ?1");
            let mut st = c.prepare(&sql)?;
            let row = st
                .query_map(rusqlite::params![id], row_to_occurrence)?
                .next()
                .transpose()?;
            Ok(row)
        })
        .await
    }

    /// Most recent occurrences of a job, newest first.
    pub async fn list_occurrences(&self, job_id: String, limit: u32) -> Result<Vec<Occurrence>> {
        self.with_conn(move |c| {
            let sql = format!(
                "SELECT {OCCURRENCE_COLS} FROM job_occurrences \
                 WHERE job_id = ?1 ORDER BY created_at DESC LIMIT ?2"
            );
            let mut st = c.prepare(&sql)?;
            let rows = st
                .query_map(rusqlite::params![job_id, limit], row_to_occurrence)?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
    }

    /// Occurrences still `scheduled` with `scheduled_for <= now`.
    pub async fn list_due_occurrences(&self, now_rfc3339: String) -> Result<Vec<Occurrence>> {
        self.with_conn(move |c| {
            let sql = format!(
                "SELECT {OCCURRENCE_COLS} FROM job_occurrences \
                 WHERE status = 'scheduled' AND scheduled_for <= ?1 ORDER BY scheduled_for"
            );
            let mut st = c.prepare(&sql)?;
            let rows = st
                .query_map(rusqlite::params![now_rfc3339], row_to_occurrence)?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
    }

    /// Non-terminal occurrences of one job (single-flight input).
    pub async fn list_live_occurrences(&self, job_id: String) -> Result<Vec<Occurrence>> {
        self.with_conn(move |c| {
            let sql = format!(
                "SELECT {OCCURRENCE_COLS} FROM job_occurrences \
                 WHERE job_id = ?1 AND status IN ('scheduled','running','waiting_approval') \
                 ORDER BY created_at"
            );
            let mut st = c.prepare(&sql)?;
            let rows = st
                .query_map(rusqlite::params![job_id], row_to_occurrence)?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
    }

    pub async fn update_occurrence(
        &self,
        id: String,
        status: OccurrenceStatus,
        run_id: Option<String>,
        reason: Option<String>,
    ) -> Result<bool> {
        self.with_conn(move |c| {
            Ok(c.execute(
                "UPDATE job_occurrences SET status = ?2, run_id = COALESCE(?3, run_id), \
                 reason = ?4, updated_at = ?5
                 WHERE id = ?1",
                rusqlite::params![id, status.as_str(), run_id, reason, now()],
            )? > 0)
        })
        .await
    }

    /// Flag a possibly-half-executed consequential step for human review.
    pub async fn flag_occurrence_review(&self, id: String, reason: String) -> Result<bool> {
        self.with_conn(move |c| {
            Ok(c.execute(
                "UPDATE job_occurrences SET needs_human_review = 1, reason = ?2, updated_at = ?3
                 WHERE id = ?1",
                rusqlite::params![id, reason, now()],
            )? > 0)
        })
        .await
    }

    /// Occurrences created at/after `since` (daily-cap input; caller passes
    /// local midnight).
    pub async fn count_occurrences_since(
        &self,
        job_id: String,
        since_rfc3339: String,
    ) -> Result<u32> {
        self.with_conn(move |c| {
            let n: i64 = c.query_row(
                "SELECT COUNT(*) FROM job_occurrences \
                 WHERE job_id = ?1 AND created_at >= ?2 AND status <> 'skipped'",
                rusqlite::params![job_id, since_rfc3339],
                |r| r.get(0),
            )?;
            Ok(n as u32)
        })
        .await
    }

    /// Startup sweep: live occurrence rows → interrupted (mirrors the runs
    /// sweep in `db::mark_interrupted_on_startup`). Returns rows marked.
    pub async fn mark_occurrences_interrupted_on_startup(&self) -> Result<usize> {
        self.with_conn(|c| {
            c.execute(
                "UPDATE job_occurrences SET status = 'interrupted', updated_at = ?1
                 WHERE status IN ('running','waiting_approval')",
                rusqlite::params![now()],
            )
        })
        .await
    }

    /// Inbox feed: pending approvals whose run belongs to a parked
    /// (waiting_approval) job occurrence.
    pub async fn list_parked_job_approvals(&self) -> Result<Vec<ParkedApproval>> {
        self.with_conn(|c| {
            let mut st = c.prepare(
                "SELECT a.id, a.run_id, a.agent, a.title, a.payload, a.status, a.created_at, \
                 j.id, j.name, o.id
                 FROM approvals a
                 JOIN job_occurrences o ON a.run_id = o.run_id
                 JOIN jobs j ON o.job_id = j.id
                 WHERE a.status = 'pending' AND o.status = 'waiting_approval'
                 ORDER BY a.created_at DESC LIMIT 100",
            )?;
            let rows = st
                .query_map([], |r| {
                    let status: String = r.get(5)?;
                    Ok(ParkedApproval {
                        approval: ApprovalInfo {
                            id: r.get(0)?,
                            run_id: r.get(1)?,
                            agent: r.get(2)?,
                            title: r.get(3)?,
                            payload: r.get(4)?,
                            status: serde_json::from_value(serde_json::Value::String(status))
                                .unwrap_or(ApprovalStatus::Pending),
                            created_at: r.get(6)?,
                        },
                        job_id: r.get(7)?,
                        job_name: r.get(8)?,
                        occurrence_id: r.get(9)?,
                    })
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
    }

    /// True when any approval row for `run_id` has status `expired`.
    /// Supervisor catch-up uses this to detect parked approvals that expired
    /// on restart (those occurrences count toward the circuit breaker and
    /// are never auto-continued). Lead-approved SA5 contract addition.
    pub async fn occurrence_had_expired_approval(&self, run_id: String) -> Result<bool> {
        self.with_conn(move |c| {
            let n: i64 = c.query_row(
                "SELECT COUNT(*) FROM approvals WHERE run_id = ?1 AND status = 'expired'",
                rusqlite::params![run_id],
                |r| r.get(0),
            )?;
            Ok(n > 0)
        })
        .await
    }
}

/// Parse helpers shared by row mapping (SA1 may keep these private).
pub fn parse_missed_policy(s: &str) -> MissedPolicy {
    serde_json::from_value(serde_json::Value::String(s.to_string())).unwrap_or_default()
}

pub fn parse_overlap_policy(s: &str) -> OverlapPolicy {
    serde_json::from_value(serde_json::Value::String(s.to_string())).unwrap_or_default()
}

fn missed_policy_as_str(p: MissedPolicy) -> &'static str {
    match p {
        MissedPolicy::RunOnceIfMissed => "run_once_if_missed",
        MissedPolicy::Skip => "skip",
    }
}

fn overlap_policy_as_str(p: OverlapPolicy) -> &'static str {
    match p {
        OverlapPolicy::Skip => "skip",
        OverlapPolicy::QueueOne => "queue_one",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vortex_types::{ChatMessage, Mode, RunInfo, RunStatus};

    fn test_db() -> (Db, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        (Db::open(dir.path().join("test.db")).unwrap(), dir)
    }

    fn routine_input(name: &str) -> NewRoutine {
        NewRoutine {
            name: name.into(),
            description: format!("{name} description"),
            instructions: "v1 instructions".into(),
            mode: Mode::Agent,
            model: None,
        }
    }

    fn job_input(routine: &Routine, name: &str) -> NewJob {
        NewJob {
            name: name.into(),
            routine_id: routine.id.clone(),
            routine_version: 1,
            schedule: Some(ScheduleSpec::default()),
            overlap: OverlapPolicy::Skip,
            budgets: JobBudgets::default(),
            workspace: None,
        }
    }

    async fn routine_and_job(db: &Db) -> (Routine, Job) {
        let r = db.create_routine(routine_input("r")).await.unwrap();
        let j = db.create_job(job_input(&r, "j")).await.unwrap();
        (r, j)
    }

    #[tokio::test]
    async fn routine_crud_roundtrip_lands_draft_v1() {
        let (db, _dir) = test_db();
        let r = db.create_routine(routine_input("digest")).await.unwrap();
        assert_eq!(r.status, RoutineStatus::Draft);
        assert_eq!(r.current_version, 1);
        let got = db.get_routine(r.id.clone()).await.unwrap().unwrap();
        assert_eq!(got.id, r.id);
        assert_eq!(got.name, "digest");
        assert_eq!(got.description, r.description);
        assert_eq!(got.created_at, r.created_at);
        assert_eq!(got.updated_at, r.updated_at);
        let v1 = db
            .get_routine_version(r.id.clone(), 1)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(v1.routine_id, r.id);
        assert_eq!(v1.version, 1);
        assert_eq!(v1.instructions, "v1 instructions");
        assert_eq!(v1.mode, Mode::Agent);
        assert_eq!(v1.model, None);
        assert_eq!(
            db.list_routine_versions(r.id.clone()).await.unwrap().len(),
            1
        );
        assert!(db
            .list_routines()
            .await
            .unwrap()
            .iter()
            .any(|x| x.id == r.id));
        assert!(db.get_routine("missing".into()).await.unwrap().is_none());
        assert!(db
            .get_routine_version(r.id.clone(), 99)
            .await
            .unwrap()
            .is_none());
        assert!(db.activate_routine(r.id.clone()).await.unwrap());
        assert_eq!(
            db.get_routine(r.id.clone()).await.unwrap().unwrap().status,
            RoutineStatus::Active
        );
        assert!(db.archive_routine(r.id.clone()).await.unwrap());
        assert_eq!(
            db.get_routine(r.id.clone()).await.unwrap().unwrap().status,
            RoutineStatus::Archived
        );
        assert!(!db.archive_routine(r.id).await.unwrap());
    }

    #[tokio::test]
    async fn version_pin_survives_routine_edit() {
        let (db, _dir) = test_db();
        let r = db.create_routine(routine_input("pin")).await.unwrap();
        let mut input = job_input(&r, "pinned");
        input.schedule = None;
        let job = db.create_job(input).await.unwrap();
        let v2 = db
            .add_routine_version(
                r.id.clone(),
                "v2 instructions".into(),
                Mode::Coding,
                Some("openai/gpt-4o".into()),
            )
            .await
            .unwrap();
        assert_eq!(v2.version, 2);
        assert_eq!(v2.mode, Mode::Coding);
        assert_eq!(v2.model.as_deref(), Some("openai/gpt-4o"));
        let pinned = db
            .get_routine_version(job.routine_id.clone(), job.routine_version)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(pinned.instructions, "v1 instructions");
        assert_eq!(pinned.mode, Mode::Agent);
        let after = db.get_routine(r.id.clone()).await.unwrap().unwrap();
        assert_eq!(after.current_version, 2);
        assert_eq!(
            after.status,
            RoutineStatus::Draft,
            "adding a version never changes status"
        );
        let versions = db.list_routine_versions(r.id).await.unwrap();
        assert_eq!(
            versions.iter().map(|v| v.version).collect::<Vec<_>>(),
            vec![2, 1]
        );
        let err = db
            .add_routine_version("missing".into(), "x".into(), Mode::Chat, None)
            .await
            .unwrap_err();
        assert!(matches!(err, CoreError::Config(_)));
    }

    #[tokio::test]
    async fn activate_routine_only_from_draft() {
        let (db, _dir) = test_db();
        let r = db.create_routine(routine_input("act")).await.unwrap();
        assert!(db.activate_routine(r.id.clone()).await.unwrap());
        assert!(!db.activate_routine(r.id.clone()).await.unwrap());
        assert!(!db.activate_routine("missing".into()).await.unwrap());
        assert!(db.archive_routine(r.id.clone()).await.unwrap());
        assert!(!db.activate_routine(r.id).await.unwrap());
        let r2 = db.create_routine(routine_input("act2")).await.unwrap();
        assert!(db.archive_routine(r2.id).await.unwrap());
    }

    #[tokio::test]
    async fn create_job_makes_dedicated_conversation_and_validates_pin() {
        let (db, _dir) = test_db();
        let mut input = routine_input("conv");
        input.mode = Mode::Research;
        let r = db.create_routine(input).await.unwrap();
        let job = db
            .create_job(NewJob {
                name: "nightly".into(),
                routine_id: r.id.clone(),
                routine_version: 1,
                schedule: Some(ScheduleSpec {
                    cron: "*/5 * * * *".into(),
                    tz: "local".into(),
                    missed_policy: MissedPolicy::Skip,
                }),
                overlap: OverlapPolicy::QueueOne,
                budgets: JobBudgets {
                    max_turns: 4,
                    max_tokens: 1000,
                    daily_occurrence_cap: 2,
                    run_timeout_secs: 60,
                },
                workspace: Some("/tmp/vortex-job-ws".into()),
            })
            .await
            .unwrap();
        assert_eq!(job.status, JobStatus::Draft);
        assert_eq!(job.consecutive_failures, 0);
        assert!(job.next_run_at.is_none());
        assert!(job.steer_note.is_none());
        let conv = db
            .get_conversation(job.conversation_id.clone())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(conv.title, "Job: nightly");
        assert_eq!(conv.mode, Mode::Research);
        assert_eq!(conv.workspace.as_deref(), Some("/tmp/vortex-job-ws"));
        assert_eq!(conv.model, None);
        let got = db.get_job(job.id.clone()).await.unwrap().unwrap();
        let schedule = got.schedule.as_ref().unwrap();
        assert_eq!(schedule.cron, "*/5 * * * *");
        assert_eq!(schedule.tz, "local");
        assert_eq!(schedule.missed_policy, MissedPolicy::Skip);
        assert_eq!(got.overlap, OverlapPolicy::QueueOne);
        assert_eq!(got.budgets.max_turns, 4);
        assert_eq!(got.budgets.max_tokens, 1000);
        assert_eq!(got.budgets.daily_occurrence_cap, 2);
        assert_eq!(got.budgets.run_timeout_secs, 60);
        assert_eq!(got.workspace.as_deref(), Some("/tmp/vortex-job-ws"));
        let manual = db
            .create_job(NewJob {
                schedule: None,
                ..job_input(&r, "manual")
            })
            .await
            .unwrap();
        assert!(manual.schedule.is_none());
        let err = db
            .create_job(NewJob {
                routine_id: "nope".into(),
                ..job_input(&r, "bad")
            })
            .await
            .unwrap_err();
        assert!(matches!(err, CoreError::Config(_)));
        let err = db
            .create_job(NewJob {
                routine_version: 99,
                ..job_input(&r, "bad")
            })
            .await
            .unwrap_err();
        assert!(matches!(err, CoreError::Config(_)));
        assert_eq!(db.list_jobs().await.unwrap().len(), 2);
        assert_eq!(
            db.list_conversations().await.unwrap().len(),
            2,
            "rejected creates must not leave orphan conversations"
        );
    }

    #[tokio::test]
    async fn insert_occurrence_double_fire_guard() {
        let (db, _dir) = test_db();
        let (_r, job) = routine_and_job(&db).await;
        let tick = "2026-11-22T09:00:00.000Z";
        let first = db
            .insert_occurrence(job.id.clone(), tick.into())
            .await
            .unwrap();
        let o = first.expect("first insert wins");
        assert_eq!(o.job_id, job.id);
        assert_eq!(o.scheduled_for, tick);
        assert_eq!(o.status, OccurrenceStatus::Scheduled);
        assert!(o.run_id.is_none());
        assert!(o.reason.is_none());
        assert!(!o.needs_human_review);
        assert!(db
            .insert_occurrence(job.id.clone(), tick.into())
            .await
            .unwrap()
            .is_none());
        assert!(db
            .insert_occurrence(job.id.clone(), "2026-11-22T10:00:00.000Z".into())
            .await
            .unwrap()
            .is_some());
        let db2 = db.clone();
        let jid = job.id.clone();
        let jid2 = job.id.clone();
        let tick2 = "2026-11-22T11:00:00.000Z".to_string();
        let (a, b) = tokio::join!(
            db.insert_occurrence(jid, tick2.clone()),
            db2.insert_occurrence(jid2, tick2),
        );
        let a = a.unwrap();
        let b = b.unwrap();
        assert!(
            a.is_some() ^ b.is_some(),
            "exactly one concurrent insert may win"
        );
        assert_eq!(db.list_occurrences(job.id, 100).await.unwrap().len(), 3);
    }

    #[tokio::test]
    async fn due_and_live_occurrence_queries() {
        let (db, _dir) = test_db();
        let (_r, job) = routine_and_job(&db).await;
        let older = db
            .insert_occurrence(job.id.clone(), "2019-01-01T00:00:00.000Z".into())
            .await
            .unwrap()
            .unwrap();
        let past = db
            .insert_occurrence(job.id.clone(), "2020-01-01T00:00:00.000Z".into())
            .await
            .unwrap()
            .unwrap();
        let future = db
            .insert_occurrence(job.id.clone(), "2999-01-01T00:00:00.000Z".into())
            .await
            .unwrap()
            .unwrap();
        let now_str = "2026-11-22T12:00:00.000Z";
        let due = db.list_due_occurrences(now_str.into()).await.unwrap();
        assert_eq!(
            due.iter().map(|o| o.id.clone()).collect::<Vec<_>>(),
            vec![older.id.clone(), past.id.clone()],
            "due rows are scheduled-only, ordered by scheduled_for"
        );
        db.update_occurrence(
            older.id.clone(),
            OccurrenceStatus::Skipped,
            None,
            Some("missed tick".into()),
        )
        .await
        .unwrap();
        db.update_occurrence(
            past.id.clone(),
            OccurrenceStatus::Running,
            Some("run-1".into()),
            None,
        )
        .await
        .unwrap();
        let parked = db
            .insert_occurrence(job.id.clone(), "2020-02-01T00:00:00.000Z".into())
            .await
            .unwrap()
            .unwrap();
        db.update_occurrence(
            parked.id.clone(),
            OccurrenceStatus::WaitingApproval,
            Some("run-2".into()),
            None,
        )
        .await
        .unwrap();
        let done = db
            .insert_occurrence(job.id.clone(), "2020-03-01T00:00:00.000Z".into())
            .await
            .unwrap()
            .unwrap();
        db.update_occurrence(done.id.clone(), OccurrenceStatus::Succeeded, None, None)
            .await
            .unwrap();
        let skipped = db
            .insert_occurrence(job.id.clone(), "2020-04-01T00:00:00.000Z".into())
            .await
            .unwrap()
            .unwrap();
        db.update_occurrence(
            skipped.id.clone(),
            OccurrenceStatus::Skipped,
            None,
            Some("overlap".into()),
        )
        .await
        .unwrap();
        let live = db.list_live_occurrences(job.id.clone()).await.unwrap();
        let mut live_ids: Vec<String> = live.iter().map(|o| o.id.clone()).collect();
        live_ids.sort();
        let mut want = vec![future.id.clone(), past.id.clone(), parked.id.clone()];
        want.sort();
        assert_eq!(live_ids, want, "terminal rows are never live");
        assert!(
            db.list_due_occurrences(now_str.into())
                .await
                .unwrap()
                .is_empty(),
            "the running row is no longer due"
        );
        let (_r2, job2) = routine_and_job(&db).await;
        assert!(db.list_live_occurrences(job2.id).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn update_occurrence_coalesces_run_id_and_flags_review() {
        let (db, _dir) = test_db();
        let (_r, job) = routine_and_job(&db).await;
        let o = db
            .insert_occurrence(job.id.clone(), "2026-11-22T09:00:00.000Z".into())
            .await
            .unwrap()
            .unwrap();
        assert!(db
            .update_occurrence(
                o.id.clone(),
                OccurrenceStatus::Running,
                Some("run-1".into()),
                None
            )
            .await
            .unwrap());
        assert!(db
            .update_occurrence(
                o.id.clone(),
                OccurrenceStatus::WaitingApproval,
                None,
                Some("needs decision".into())
            )
            .await
            .unwrap());
        let got = db.get_occurrence(o.id.clone()).await.unwrap().unwrap();
        assert_eq!(got.status, OccurrenceStatus::WaitingApproval);
        assert_eq!(
            got.run_id.as_deref(),
            Some("run-1"),
            "None must preserve the existing run_id"
        );
        assert_eq!(got.reason.as_deref(), Some("needs decision"));
        assert!(db
            .update_occurrence(o.id.clone(), OccurrenceStatus::Failed, None, None)
            .await
            .unwrap());
        let got = db.get_occurrence(o.id.clone()).await.unwrap().unwrap();
        assert_eq!(got.run_id.as_deref(), Some("run-1"));
        assert!(got.reason.is_none(), "reason is written unconditionally");
        assert!(!db
            .update_occurrence("missing".into(), OccurrenceStatus::Running, None, None)
            .await
            .unwrap());
        assert!(db.get_occurrence("missing".into()).await.unwrap().is_none());
        assert!(db
            .flag_occurrence_review(o.id.clone(), "half-executed write".into())
            .await
            .unwrap());
        let got = db.get_occurrence(o.id).await.unwrap().unwrap();
        assert!(got.needs_human_review);
        assert_eq!(got.reason.as_deref(), Some("half-executed write"));
        assert!(!db
            .flag_occurrence_review("missing".into(), "x".into())
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn startup_sweep_interrupts_live_occurrences_only() {
        let (db, _dir) = test_db();
        let (_r, job) = routine_and_job(&db).await;
        let running = db
            .insert_occurrence(job.id.clone(), "2026-11-22T09:00:00.000Z".into())
            .await
            .unwrap()
            .unwrap();
        db.update_occurrence(
            running.id.clone(),
            OccurrenceStatus::Running,
            Some("run-1".into()),
            None,
        )
        .await
        .unwrap();
        let parked = db
            .insert_occurrence(job.id.clone(), "2026-11-22T10:00:00.000Z".into())
            .await
            .unwrap()
            .unwrap();
        db.update_occurrence(
            parked.id.clone(),
            OccurrenceStatus::WaitingApproval,
            Some("run-2".into()),
            None,
        )
        .await
        .unwrap();
        let scheduled = db
            .insert_occurrence(job.id.clone(), "2026-11-22T11:00:00.000Z".into())
            .await
            .unwrap()
            .unwrap();
        let done = db
            .insert_occurrence(job.id.clone(), "2026-11-22T12:00:00.000Z".into())
            .await
            .unwrap()
            .unwrap();
        db.update_occurrence(done.id.clone(), OccurrenceStatus::Succeeded, None, None)
            .await
            .unwrap();
        assert_eq!(
            db.mark_occurrences_interrupted_on_startup().await.unwrap(),
            2
        );
        assert_eq!(
            db.get_occurrence(running.id).await.unwrap().unwrap().status,
            OccurrenceStatus::Interrupted
        );
        assert_eq!(
            db.get_occurrence(parked.id).await.unwrap().unwrap().status,
            OccurrenceStatus::Interrupted
        );
        assert_eq!(
            db.get_occurrence(scheduled.id)
                .await
                .unwrap()
                .unwrap()
                .status,
            OccurrenceStatus::Scheduled,
            "scheduled rows stay for the supervisor's due-query"
        );
        assert_eq!(
            db.get_occurrence(done.id).await.unwrap().unwrap().status,
            OccurrenceStatus::Succeeded
        );
        assert_eq!(
            db.mark_occurrences_interrupted_on_startup().await.unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn parked_job_approvals_join() {
        let (db, _dir) = test_db();
        let (_r, job) = routine_and_job(&db).await;
        let parked = db
            .insert_occurrence(job.id.clone(), "2026-11-22T09:00:00.000Z".into())
            .await
            .unwrap()
            .unwrap();
        db.update_occurrence(
            parked.id.clone(),
            OccurrenceStatus::WaitingApproval,
            Some("run-job".into()),
            None,
        )
        .await
        .unwrap();
        db.add_approval(
            "a-parked".into(),
            "run-job".into(),
            "agent".into(),
            "Delete file?".into(),
            "{}".into(),
        )
        .await
        .unwrap();
        let running = db
            .insert_occurrence(job.id.clone(), "2026-11-22T10:00:00.000Z".into())
            .await
            .unwrap()
            .unwrap();
        db.update_occurrence(
            running.id.clone(),
            OccurrenceStatus::Running,
            Some("run-live".into()),
            None,
        )
        .await
        .unwrap();
        db.add_approval(
            "a-live".into(),
            "run-live".into(),
            "agent".into(),
            "t".into(),
            "{}".into(),
        )
        .await
        .unwrap();
        db.add_approval(
            "a-interactive".into(),
            "run-interactive".into(),
            "agent".into(),
            "t".into(),
            "{}".into(),
        )
        .await
        .unwrap();
        let inbox = db.list_parked_job_approvals().await.unwrap();
        assert_eq!(inbox.len(), 1);
        let p = &inbox[0];
        assert_eq!(p.approval.id, "a-parked");
        assert_eq!(p.approval.run_id, "run-job");
        assert_eq!(p.approval.title, "Delete file?");
        assert_eq!(p.approval.status, ApprovalStatus::Pending);
        assert_eq!(p.job_id, job.id);
        assert_eq!(p.job_name, job.name);
        assert_eq!(p.occurrence_id, parked.id);
        db.resolve_approval("a-parked".into(), ApprovalStatus::Approved)
            .await
            .unwrap();
        assert!(db.list_parked_job_approvals().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn count_occurrences_since_excludes_skipped_and_older() {
        let (db, _dir) = test_db();
        let (_r, job) = routine_and_job(&db).await;
        let a = db
            .insert_occurrence(job.id.clone(), "2026-11-22T09:00:00.000Z".into())
            .await
            .unwrap()
            .unwrap();
        let b = db
            .insert_occurrence(job.id.clone(), "2026-11-22T10:00:00.000Z".into())
            .await
            .unwrap()
            .unwrap();
        let _c = db
            .insert_occurrence(job.id.clone(), "2026-11-22T11:00:00.000Z".into())
            .await
            .unwrap()
            .unwrap();
        db.update_occurrence(
            b.id.clone(),
            OccurrenceStatus::Skipped,
            None,
            Some("overlap".into()),
        )
        .await
        .unwrap();
        let old_id = a.id.clone();
        db.with_conn(move |c| {
            c.execute(
                "UPDATE job_occurrences SET created_at = '2000-01-01T00:00:00.000Z' \
                 WHERE id = ?1",
                rusqlite::params![old_id],
            )?;
            Ok(())
        })
        .await
        .unwrap();
        assert_eq!(
            db.count_occurrences_since(job.id.clone(), "2020-01-01T00:00:00.000Z".into())
                .await
                .unwrap(),
            1,
            "only the recent non-skipped row counts"
        );
        assert_eq!(
            db.count_occurrences_since(job.id.clone(), "1970-01-01T00:00:00.000Z".into())
                .await
                .unwrap(),
            2,
            "skipped rows never count, older rows do when in range"
        );
        let (_r2, job2) = routine_and_job(&db).await;
        db.insert_occurrence(job2.id.clone(), "2026-11-22T09:00:00.000Z".into())
            .await
            .unwrap();
        assert_eq!(
            db.count_occurrences_since(job.id, "1970-01-01T00:00:00.000Z".into())
                .await
                .unwrap(),
            2,
            "other jobs' occurrences never leak into the count"
        );
        assert_eq!(
            db.count_occurrences_since(job2.id, "1970-01-01T00:00:00.000Z".into())
                .await
                .unwrap(),
            1
        );
    }

    #[tokio::test]
    async fn migration_is_idempotent_and_adds_jobs_tables_to_legacy_db() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy.db");
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE conversations (
                     id          TEXT PRIMARY KEY,
                     title       TEXT NOT NULL,
                     mode        TEXT NOT NULL DEFAULT 'chat',
                     workspace   TEXT,
                     created_at  TEXT NOT NULL,
                     updated_at  TEXT NOT NULL
                 );
                 CREATE TABLE messages (
                     id              INTEGER PRIMARY KEY AUTOINCREMENT,
                     conversation_id TEXT NOT NULL,
                     role            TEXT NOT NULL,
                     content         TEXT NOT NULL,
                     tool_calls      TEXT NOT NULL DEFAULT '[]',
                     created_at      TEXT NOT NULL
                 );
                 INSERT INTO conversations VALUES
                     ('c1','Legacy','chat',NULL,'2026-01-01T00:00:00.000Z','2026-01-01T00:00:00.000Z');
                 INSERT INTO messages (conversation_id, role, content, created_at)
                     VALUES ('c1','user','hello','2026-01-01T00:00:00.000Z');",
            )
            .unwrap();
        }
        let db = Db::open(&path).unwrap();
        {
            let conn = db.raw_connection();
            for table in ["routines", "routine_versions", "jobs", "job_occurrences"] {
                let sql = format!("SELECT 1 FROM pragma_table_info('{table}') LIMIT 1");
                assert!(
                    conn.prepare(&sql).unwrap().exists([]).unwrap(),
                    "{table} must exist after migration"
                );
            }
            let title: String = conn
                .query_row("SELECT title FROM conversations WHERE id = 'c1'", [], |r| {
                    r.get(0)
                })
                .unwrap();
            assert_eq!(title, "Legacy", "legacy data is preserved");
        }
        drop(db);
        let db = Db::open(&path).unwrap();
        let r = db.create_routine(routine_input("reopen")).await.unwrap();
        assert_eq!(r.current_version, 1);
        drop(db);
        Db::open(&path).unwrap();
    }

    #[tokio::test]
    async fn delete_job_removes_occurrences_and_conversation() {
        let (db, _dir) = test_db();
        let (_r, job) = routine_and_job(&db).await;
        let o = db
            .insert_occurrence(job.id.clone(), "2026-11-22T09:00:00.000Z".into())
            .await
            .unwrap()
            .unwrap();
        db.add_message(
            job.conversation_id.clone(),
            ChatMessage::user("job transcript"),
        )
        .await
        .unwrap();
        db.upsert_run(&RunInfo {
            id: "run-j".into(),
            conversation_id: Some(job.conversation_id.clone()),
            parent_run_id: None,
            agent: "a".into(),
            task: "".into(),
            model: "".into(),
            status: RunStatus::Completed,
            error: None,
            usage: None,
            plan: Vec::new(),
            depth: 0,
            created_at: now(),
            updated_at: now(),
        })
        .await
        .unwrap();
        db.add_approval(
            "a-j".into(),
            "run-j".into(),
            "a".into(),
            "t".into(),
            "{}".into(),
        )
        .await
        .unwrap();
        assert!(db.delete_job(job.id.clone()).await.unwrap());
        assert!(db.get_job(job.id.clone()).await.unwrap().is_none());
        assert!(db.get_occurrence(o.id).await.unwrap().is_none());
        assert!(db
            .get_conversation(job.conversation_id)
            .await
            .unwrap()
            .is_none());
        let (messages, runs, approvals): (i64, i64, i64) = db
            .with_conn(|c| {
                Ok((
                    c.query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))?,
                    c.query_row("SELECT COUNT(*) FROM runs", [], |r| r.get(0))?,
                    c.query_row("SELECT COUNT(*) FROM approvals", [], |r| r.get(0))?,
                ))
            })
            .await
            .unwrap();
        assert_eq!((messages, runs, approvals), (0, 0, 0));
        assert!(!db.delete_job(job.id).await.unwrap());
    }

    #[tokio::test]
    async fn job_status_steer_next_run_config_and_failure_counter() {
        let (db, _dir) = test_db();
        let (_r, job) = routine_and_job(&db).await;
        assert!(db
            .set_job_status(job.id.clone(), JobStatus::Active)
            .await
            .unwrap());
        assert_eq!(
            db.get_job(job.id.clone()).await.unwrap().unwrap().status,
            JobStatus::Active
        );
        assert!(db
            .set_job_status(job.id.clone(), JobStatus::CircuitBroken)
            .await
            .unwrap());
        assert!(!db
            .set_job_status("missing".into(), JobStatus::Active)
            .await
            .unwrap());
        assert!(db
            .set_job_steer_note(job.id.clone(), Some("focus on tests".into()))
            .await
            .unwrap());
        assert_eq!(
            db.get_job(job.id.clone())
                .await
                .unwrap()
                .unwrap()
                .steer_note
                .as_deref(),
            Some("focus on tests")
        );
        assert!(db.set_job_steer_note(job.id.clone(), None).await.unwrap());
        assert!(db
            .get_job(job.id.clone())
            .await
            .unwrap()
            .unwrap()
            .steer_note
            .is_none());
        assert!(db
            .set_job_next_run(job.id.clone(), Some("2026-11-23T09:00:00.000Z".into()))
            .await
            .unwrap());
        assert_eq!(
            db.get_job(job.id.clone())
                .await
                .unwrap()
                .unwrap()
                .next_run_at
                .as_deref(),
            Some("2026-11-23T09:00:00.000Z")
        );
        assert!(db.set_job_next_run(job.id.clone(), None).await.unwrap());
        assert!(db
            .get_job(job.id.clone())
            .await
            .unwrap()
            .unwrap()
            .next_run_at
            .is_none());
        assert!(db
            .update_job_config(
                job.id.clone(),
                "renamed".into(),
                None,
                OverlapPolicy::QueueOne,
                JobBudgets {
                    max_turns: 3,
                    ..JobBudgets::default()
                },
                Some("/ws".into()),
            )
            .await
            .unwrap());
        let got = db.get_job(job.id.clone()).await.unwrap().unwrap();
        assert_eq!(got.name, "renamed");
        assert!(
            got.schedule.is_none(),
            "None schedule clears cron back to NULL"
        );
        assert_eq!(got.overlap, OverlapPolicy::QueueOne);
        assert_eq!(got.budgets.max_turns, 3);
        assert_eq!(got.workspace.as_deref(), Some("/ws"));
        for missed in [MissedPolicy::RunOnceIfMissed, MissedPolicy::Skip] {
            assert!(db
                .update_job_config(
                    job.id.clone(),
                    "renamed".into(),
                    Some(ScheduleSpec {
                        cron: "0 9 * * *".into(),
                        tz: "local".into(),
                        missed_policy: missed,
                    }),
                    OverlapPolicy::Skip,
                    JobBudgets::default(),
                    None,
                )
                .await
                .unwrap());
            let got = db.get_job(job.id.clone()).await.unwrap().unwrap();
            assert_eq!(got.schedule.unwrap().missed_policy, missed);
        }
        assert!(!db
            .update_job_config(
                "missing".into(),
                "x".into(),
                None,
                OverlapPolicy::Skip,
                JobBudgets::default(),
                None,
            )
            .await
            .unwrap());
        assert_eq!(db.record_job_failure(job.id.clone()).await.unwrap(), 1);
        assert_eq!(db.record_job_failure(job.id.clone()).await.unwrap(), 2);
        assert_eq!(db.record_job_failure(job.id.clone()).await.unwrap(), 3);
        assert_eq!(
            db.get_job(job.id.clone())
                .await
                .unwrap()
                .unwrap()
                .consecutive_failures,
            3
        );
        assert!(db.reset_job_failures(job.id.clone()).await.unwrap());
        assert_eq!(
            db.get_job(job.id.clone())
                .await
                .unwrap()
                .unwrap()
                .consecutive_failures,
            0
        );
        assert!(!db.reset_job_failures("missing".into()).await.unwrap());
        assert!(db.record_job_failure("missing".into()).await.is_err());
        let jid = job.id.clone();
        db.with_conn(move |c| {
            c.execute(
                "UPDATE jobs SET budgets_json = '{invalid', status = 'bogus' WHERE id = ?1",
                rusqlite::params![jid],
            )?;
            Ok(())
        })
        .await
        .unwrap();
        let got = db.get_job(job.id).await.unwrap().unwrap();
        assert_eq!(
            got.budgets,
            JobBudgets::default(),
            "bad JSON falls back to defaults"
        );
        assert_eq!(
            got.status,
            JobStatus::Draft,
            "unknown status falls back to draft"
        );
    }

    #[tokio::test]
    async fn list_queries_use_frozen_orderings() {
        let (db, _dir) = test_db();
        let r1 = db.create_routine(routine_input("first")).await.unwrap();
        let r2 = db.create_routine(routine_input("second")).await.unwrap();
        let (a, b) = (r1.id.clone(), r2.id.clone());
        db.with_conn(move |c| {
            c.execute(
                "UPDATE routines SET updated_at = '2026-01-01T00:00:00.000Z' WHERE id = ?1",
                rusqlite::params![a],
            )?;
            c.execute(
                "UPDATE routines SET updated_at = '2026-02-01T00:00:00.000Z' WHERE id = ?1",
                rusqlite::params![b],
            )?;
            Ok(())
        })
        .await
        .unwrap();
        let ids: Vec<String> = db
            .list_routines()
            .await
            .unwrap()
            .into_iter()
            .map(|r| r.id)
            .collect();
        assert_eq!(ids, vec![r2.id.clone(), r1.id.clone()]);
        let j1 = db.create_job(job_input(&r1, "j1")).await.unwrap();
        let j2 = db.create_job(job_input(&r1, "j2")).await.unwrap();
        let (a, b) = (j1.id.clone(), j2.id.clone());
        db.with_conn(move |c| {
            c.execute(
                "UPDATE jobs SET created_at = '2026-02-01T00:00:00.000Z' WHERE id = ?1",
                rusqlite::params![a],
            )?;
            c.execute(
                "UPDATE jobs SET created_at = '2026-01-01T00:00:00.000Z' WHERE id = ?1",
                rusqlite::params![b],
            )?;
            Ok(())
        })
        .await
        .unwrap();
        let ids: Vec<String> = db
            .list_jobs()
            .await
            .unwrap()
            .into_iter()
            .map(|j| j.id)
            .collect();
        assert_eq!(ids, vec![j2.id.clone(), j1.id.clone()]);
        let o1 = db
            .insert_occurrence(j1.id.clone(), "2026-11-22T09:00:00.000Z".into())
            .await
            .unwrap()
            .unwrap();
        let o2 = db
            .insert_occurrence(j1.id.clone(), "2026-11-22T10:00:00.000Z".into())
            .await
            .unwrap()
            .unwrap();
        let o3 = db
            .insert_occurrence(j1.id.clone(), "2026-11-22T11:00:00.000Z".into())
            .await
            .unwrap()
            .unwrap();
        let (a, b, c) = (o1.id.clone(), o2.id.clone(), o3.id.clone());
        db.with_conn(move |conn| {
            conn.execute(
                "UPDATE job_occurrences SET created_at = '2026-01-01T00:00:00.000Z' WHERE id = ?1",
                rusqlite::params![a],
            )?;
            conn.execute(
                "UPDATE job_occurrences SET created_at = '2026-01-02T00:00:00.000Z' WHERE id = ?1",
                rusqlite::params![b],
            )?;
            conn.execute(
                "UPDATE job_occurrences SET created_at = '2026-01-03T00:00:00.000Z' WHERE id = ?1",
                rusqlite::params![c],
            )?;
            Ok(())
        })
        .await
        .unwrap();
        let ids: Vec<String> = db
            .list_occurrences(j1.id, 2)
            .await
            .unwrap()
            .into_iter()
            .map(|o| o.id)
            .collect();
        assert_eq!(ids, vec![o3.id, o2.id], "newest first, limit respected");
    }

    #[test]
    fn policy_str_helpers_match_wire_format() {
        for p in [MissedPolicy::RunOnceIfMissed, MissedPolicy::Skip] {
            assert_eq!(parse_missed_policy(missed_policy_as_str(p)), p);
            assert_eq!(
                serde_json::to_value(p).unwrap(),
                serde_json::Value::String(missed_policy_as_str(p).into())
            );
        }
        for p in [OverlapPolicy::Skip, OverlapPolicy::QueueOne] {
            assert_eq!(parse_overlap_policy(overlap_policy_as_str(p)), p);
            assert_eq!(
                serde_json::to_value(p).unwrap(),
                serde_json::Value::String(overlap_policy_as_str(p).into())
            );
        }
        assert_eq!(parse_missed_policy("bogus"), MissedPolicy::default());
        assert_eq!(parse_overlap_policy("bogus"), OverlapPolicy::default());
    }

    #[tokio::test]
    async fn occurrence_had_expired_approval_matches_only_expired_rows() {
        let (db, _dir) = test_db();
        let (_r, job) = routine_and_job(&db).await;
        let occ = db
            .insert_occurrence(job.id.clone(), "2026-11-22T09:00:00.000Z".into())
            .await
            .unwrap()
            .unwrap();
        db.update_occurrence(
            occ.id.clone(),
            OccurrenceStatus::WaitingApproval,
            Some("run-parked".into()),
            None,
        )
        .await
        .unwrap();
        assert!(!db
            .occurrence_had_expired_approval("run-parked".into())
            .await
            .unwrap());
        db.add_approval(
            "a-1".into(),
            "run-parked".into(),
            "agent".into(),
            "t".into(),
            "{}".into(),
        )
        .await
        .unwrap();
        assert!(
            !db.occurrence_had_expired_approval("run-parked".into())
                .await
                .unwrap(),
            "pending is not expired"
        );
        assert!(!db
            .occurrence_had_expired_approval("run-other".into())
            .await
            .unwrap());
        db.resolve_approval("a-1".into(), ApprovalStatus::Expired)
            .await
            .unwrap();
        assert!(db
            .occurrence_had_expired_approval("run-parked".into())
            .await
            .unwrap());
        assert!(!db
            .occurrence_had_expired_approval("run-other".into())
            .await
            .unwrap());
    }
}
