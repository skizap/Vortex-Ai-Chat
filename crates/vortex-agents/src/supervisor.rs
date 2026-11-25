//! Job lifecycle state machine: tick loop (driven by [`crate::clock::Clock`]),
//! occurrence admission, catch-up on startup, continuation decisions,
//! budgets + circuit breaker, steer notes, and park notifications.
//! SA5 owns the implementation (Phase 0 freeze: surface).
//!
//! Frozen design decisions:
//! - The job gate is a dedicated counting semaphore (default 2 concurrent
//!   job roots), independent of the sub-agent scheduler; nested delegates
//!   still share the global sub-agent cap. Reusing [`crate::scheduler::Scheduler`]
//!   as a pure semaphore is an explicit decision: job roots DO consume a job
//!   gate slot (unlike interactive coordinators, which never consume a
//!   sub-agent slot).
//! - Single-flight per job: an occurrence never overlaps itself; the overlap
//!   policy (`skip` default, `queue_one`) decides what a colliding tick does.
//!   `queue_one` keeps the colliding occurrence `scheduled` and defers its
//!   admission until the job's other live occurrences drain (per-job notify).
//! - Job events are seq-stamped and replayable, mirroring the hub pattern
//!   (bounded in-memory log; occurrence/job state itself is durable SQLite).
//!
//! Tick semantics: [`crate::clock::next_tick`] is strictly-after, so firing
//! is driven by the persisted `next_run_at` (the previously computed next
//! fire time): once it is due, its tick is inserted (the UNIQUE constraint is
//! the double-fire guard) and admitted, then `next_run_at` is advanced to the
//! next future tick. A brand-new active job seeds `next_run_at` on its first
//! observed tick; missed ticks while the server was down are catch-up's job.
//!
//! Crash-window honesty: an occurrence is NEVER written Running without a
//! run id. It stays `scheduled` while engine admission executes (the
//! in-memory admitting set blocks due-query re-admission), and the single
//! Running write carries the run id. Legacy rows interrupted without a run
//! id are reset to Scheduled by catch-up and re-admitted as fresh gated runs
//! — never replayed.

use crate::clock::{self, Clock, OverlapDecision};
use crate::engine::{
    classify_last_step, ContinuationSafety, ContinuationSeed, Engine, JobRunSpec, RunError,
};
use crate::hub::Hub;
use crate::prompts::job_system_prompt;
use crate::scheduler::Scheduler;
use chrono::{DateTime, Local, SecondsFormat, TimeZone, Utc};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{broadcast, Notify};
use tokio_util::sync::CancellationToken;
use vortex_core::Db;
use vortex_types::{
    ChatMessage, Job, JobEvent, JobStatus, MissedPolicy, Occurrence, OccurrenceStatus, PlanStep,
    Role, RunEvent, StepStatus,
};

/// Consecutive failed/parked-expired occurrences that trip the circuit
/// breaker (Phase 0 freeze decision #12).
const BREAKER_THRESHOLD: u32 = 3;
/// Default concurrent job roots when the caller passes 0.
const DEFAULT_JOB_GATE: usize = 2;
/// Job-event broadcast capacity (mirrors the hub pattern, smaller fan-out).
const JOB_CHANNEL_CAP: usize = 256;
/// Bounded job-event replay log; oldest entries evicted on overflow.
const MAX_JOB_LOG: usize = 1024;

/// Seq-stamped job-event log with the same gap semantics as [`Hub`]:
/// sequences start at 1, `replay_from(since)` returns everything with
/// `seq > since` plus the oldest/newest retained seq, and a cursor below the
/// retained window signals an unrecoverable gap to the caller.
struct JobEventLog {
    tx: broadcast::Sender<(u64, JobEvent)>,
    seq: AtomicU64,
    log: Mutex<VecDeque<(u64, JobEvent)>>,
}

impl JobEventLog {
    fn new() -> Self {
        let (tx, _) = broadcast::channel(JOB_CHANNEL_CAP);
        Self {
            tx,
            seq: AtomicU64::new(0),
            log: Mutex::new(VecDeque::new()),
        }
    }

    /// Seq-stamp, log, and broadcast as one serialized step (log lock held
    /// across all three), so replay + live tail is one coherent sequence.
    fn publish(&self, event: JobEvent) {
        let mut log = self
            .log
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let seq = self.seq.fetch_add(1, Ordering::SeqCst) + 1;
        if log.len() >= MAX_JOB_LOG {
            log.pop_front();
        }
        log.push_back((seq, event.clone()));
        let _ = self.tx.send((seq, event));
    }

    fn replay_from(&self, since: u64) -> (Vec<(u64, JobEvent)>, Option<u64>, Option<u64>) {
        let log = self
            .log
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let oldest = log.front().map(|(s, _)| *s);
        let newest = log.back().map(|(s, _)| *s);
        let events = log
            .iter()
            .filter(|(s, _)| *s > since)
            .cloned()
            .collect::<Vec<_>>();
        (events, oldest, newest)
    }
}

pub struct Supervisor {
    db: Db,
    engine: Arc<Engine>,
    hub: Arc<Hub>,
    clock: Arc<dyn Clock>,
    job_gate: Scheduler,
    cancel: CancellationToken,
    events: JobEventLog,
    /// Occurrence ids with an in-flight admission task. Queued (`queue_one`)
    /// occurrences stay `scheduled` while they wait, so the due query would
    /// otherwise re-admit them every tick; this set deduplicates admissions.
    admitting: Mutex<HashSet<String>>,
    /// Per-job wakeups for queued occurrences (notified on terminal outcomes).
    job_notify: Mutex<HashMap<String, Arc<Notify>>>,
}

/// What an admitted occurrence task should start once it holds a gate slot.
enum Admission {
    Fresh,
    Continuation(ContinuationSeed),
}

impl Supervisor {
    /// `job_gate_limit` is clamped server-side (default 2). `cancel` stops the
    /// tick loop cleanly; the server creates the supervisor exactly once.
    pub fn new(
        db: Db,
        engine: Arc<Engine>,
        hub: Arc<Hub>,
        clock: Arc<dyn Clock>,
        job_gate_limit: usize,
        cancel: CancellationToken,
    ) -> Arc<Self> {
        Arc::new(Self {
            db,
            engine,
            hub,
            clock,
            job_gate: Scheduler::new(gate_limit(job_gate_limit)),
            cancel,
            events: JobEventLog::new(),
            admitting: Mutex::new(HashSet::new()),
            job_notify: Mutex::new(HashMap::new()),
        })
    }

    /// Run the tick loop until cancelled. Spawned once at server startup.
    pub async fn run(self: Arc<Self>) {
        self.catch_up().await;
        loop {
            tokio::select! {
                _ = self.cancel.cancelled() => break,
                _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {
                    self.tick_once().await;
                }
            }
        }
    }

    /// Live job-event subscription (seq-stamped, like the run hub).
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<(u64, JobEvent)> {
        self.events.tx.subscribe()
    }

    /// Replay buffered job events with `seq > since`, plus the oldest/newest
    /// retained seq (same gap semantics as `Hub::replay_from`).
    pub fn replay_from(&self, since: u64) -> (Vec<(u64, JobEvent)>, Option<u64>, Option<u64>) {
        self.events.replay_from(since)
    }

    /// One scheduler pass: fire due occurrences, then maintain schedules.
    /// Additive test hook — production drives it from [`Self::run`].
    pub async fn tick_once(self: &Arc<Self>) {
        let now = self.clock.now();
        let now_utc = utc_string(&now);

        // Due `scheduled` rows (including pre-restart leftovers). Rows of
        // non-active jobs stay scheduled and fire once the job is active again.
        match self.db.list_due_occurrences(now_utc).await {
            Ok(due) => {
                for occ in due {
                    let job = match self.db.get_job(occ.job_id.clone()).await {
                        Ok(Some(job)) => job,
                        Ok(None) => continue,
                        Err(e) => {
                            tracing::warn!(error = %e, "supervisor: loading job for due occurrence failed");
                            continue;
                        }
                    };
                    if job.status == JobStatus::Active {
                        self.admit(job, occ).await;
                    }
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "supervisor: due-occurrence query failed");
            }
        }

        // Live ticks + next_run_at maintenance for every job.
        let jobs = match self.db.list_jobs().await {
            Ok(jobs) => jobs,
            Err(e) => {
                tracing::warn!(error = %e, "supervisor: listing jobs failed");
                return;
            }
        };
        for job in jobs {
            if job.status == JobStatus::Active && job.schedule.is_some() {
                self.fire_due_tick(&job, &now).await;
            }
            self.refresh_next_run(&job, &now).await;
        }
    }

    /// Startup catch-up (once): process unhandled interrupted occurrences,
    /// apply missed-tick policy, recompute next fire times. Additive test
    /// hook — production calls it once at the start of [`Self::run`].
    pub async fn catch_up(self: &Arc<Self>) {
        let now = self.clock.now();
        let jobs = match self.db.list_jobs().await {
            Ok(jobs) => jobs,
            Err(e) => {
                tracing::warn!(error = %e, "supervisor: catch-up could not list jobs");
                return;
            }
        };

        // 1. Interrupted occurrences with no reason yet (unprocessed).
        for job in &jobs {
            let occs = match self.db.list_occurrences(job.id.clone(), 200).await {
                Ok(occs) => occs,
                Err(e) => {
                    tracing::warn!(error = %e, "supervisor: catch-up could not list occurrences");
                    continue;
                }
            };
            for occ in occs {
                if occ.status != OccurrenceStatus::Interrupted {
                    continue;
                }
                // Unprocessed rows, plus the legacy crash-window dead end:
                // interrupted without a run id and reason "interrupted
                // before run admission" (written by the old admission
                // ordering before it was fixed).
                let legacy_dead_end = occ.run_id.is_none()
                    && occ.reason.as_deref() == Some("interrupted before run admission");
                if occ.reason.is_some() && !legacy_dead_end {
                    continue;
                }
                self.process_interrupted(job, &occ).await;
            }
        }

        // 2. Missed ticks for active scheduled jobs; 3. next_run_at refresh.
        for job in &jobs {
            if job.status == JobStatus::Active {
                if let Some(spec) = &job.schedule {
                    self.catch_up_missed(job, spec, &now).await;
                }
            }
            self.refresh_next_run(job, &now).await;
        }
    }

    // ----- admission -----

    /// Single-flight + daily-cap checks, then hand the occurrence to a
    /// spawned admission task (the tick loop never blocks on a run).
    async fn admit(self: &Arc<Self>, job: Job, occ: Occurrence) {
        {
            let mut admitting = self
                .admitting
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if !admitting.insert(occ.id.clone()) {
                return; // an admission task already owns this occurrence
            }
        }

        // Single-flight: count OTHER live occurrences (this one is already
        // scheduled and appears in the live list).
        let live = match self.db.list_live_occurrences(job.id.clone()).await {
            Ok(live) => live,
            Err(e) => {
                tracing::warn!(error = %e, "supervisor: live-occurrence query failed");
                self.release_admission(&occ.id);
                return;
            }
        };
        let others = live.iter().filter(|o| o.id != occ.id).count();
        match clock::overlap_decision(job.overlap, others) {
            OverlapDecision::Skip => {
                self.release_admission(&occ.id);
                self.set_occurrence(
                    &occ.id,
                    OccurrenceStatus::Skipped,
                    None,
                    Some("previous occurrence still running".into()),
                )
                .await;
                return;
            }
            OverlapDecision::QueueOne => {
                let sup = self.clone();
                tokio::spawn(async move { sup.run_queued(job, occ).await });
                return;
            }
            OverlapDecision::Fire => {}
        }

        // Daily cap. The occurrence itself is already inserted, so it counts
        // toward today's total when it was created today; compare the OTHER
        // occurrences started today against the cap.
        let midnight = local_midnight_utc(&self.clock.now());
        let count = match self
            .db
            .count_occurrences_since(job.id.clone(), midnight.clone())
            .await
        {
            Ok(count) => count,
            Err(e) => {
                tracing::warn!(error = %e, "supervisor: daily-cap query failed");
                self.release_admission(&occ.id);
                return;
            }
        };
        let counts_self = occ.created_at >= midnight;
        if cap_reached(count, job.budgets.daily_occurrence_cap, counts_self) {
            self.release_admission(&occ.id);
            self.set_occurrence(
                &occ.id,
                OccurrenceStatus::Skipped,
                None,
                Some("daily occurrence cap reached".into()),
            )
            .await;
            return;
        }

        let sup = self.clone();
        tokio::spawn(async move { sup.run_occurrence(job, occ, Admission::Fresh).await });
    }

    /// `queue_one`: wait until the job's other live occurrences drain, then
    /// admit normally. The occurrence stays `scheduled` (the queued slot)
    /// while waiting; a third colliding tick is skipped by `overlap_decision`.
    async fn run_queued(self: Arc<Self>, job: Job, occ: Occurrence) {
        let notify = self.notify_for(&job.id);
        loop {
            let notified = notify.notified();
            tokio::pin!(notified);
            // Register interest BEFORE rechecking so a terminal outcome
            // between the check and the await cannot lose the wakeup.
            notified.as_mut().enable();
            if self.cancel.is_cancelled() {
                self.release_admission(&occ.id);
                return;
            }
            let live = self
                .db
                .list_live_occurrences(job.id.clone())
                .await
                .unwrap_or_default();
            if !live.iter().any(|o| o.id != occ.id) {
                break;
            }
            tokio::select! {
                _ = notified => {}
                _ = self.cancel.cancelled() => {
                    self.release_admission(&occ.id);
                    return;
                }
            }
        }

        // The job may have been paused/circuit-broken while queued.
        let job = match self.db.get_job(job.id.clone()).await {
            Ok(Some(job)) => job,
            _ => {
                self.release_admission(&occ.id);
                return;
            }
        };
        if job.status != JobStatus::Active {
            self.release_admission(&occ.id);
            self.set_occurrence(
                &occ.id,
                OccurrenceStatus::Skipped,
                None,
                Some("job not active".into()),
            )
            .await;
            return;
        }
        self.run_occurrence(job, occ, Admission::Fresh).await;
    }

    /// Gate slot → start the run → forward approval events → await the
    /// outcome → record it. The gate guard is dropped only AFTER the outcome
    /// is recorded, so a waiting occurrence never sees a half-updated state.
    async fn run_occurrence(self: Arc<Self>, job: Job, occ: Occurrence, admission: Admission) {
        let guard = tokio::select! {
            guard = self.job_gate.acquire() => guard,
            _ = self.cancel.cancelled() => {
                // Leave the occurrence as-is (scheduled/interrupted); the
                // next startup's due query / catch-up picks it up again.
                self.release_admission(&occ.id);
                return;
            }
        };

        let is_continuation = matches!(admission, Admission::Continuation(_));
        let started = match admission {
            Admission::Fresh => self.start_fresh(&job, &occ).await,
            Admission::Continuation(seed) => self.engine.spawn_continuation(seed).await,
        };

        match started {
            Err(e) => {
                let reason = e.to_string();
                self.set_occurrence(
                    &occ.id,
                    OccurrenceStatus::Failed,
                    None,
                    Some(reason.clone()),
                )
                .await;
                self.record_failure(&job.id, &reason).await;
            }
            Ok((run_id, mut handle)) => {
                let reason = if is_continuation {
                    Some("continuation_spawned".to_string())
                } else {
                    None
                };
                self.set_occurrence(
                    &occ.id,
                    OccurrenceStatus::Running,
                    Some(run_id.clone()),
                    reason,
                )
                .await;

                // Subscribe first, then replay what was already emitted, so
                // no approval event can fall between the two (duplicates are
                // filtered by seq; gaps cannot happen).
                let mut rx = self.hub.subscribe(&run_id);
                let (replayed, _, newest) = self.hub.replay_from(&run_id, 0);
                let mut last_seq = newest.unwrap_or(0);
                for (_, ev) in replayed {
                    self.forward_approval(&job.id, &occ.id, &ev).await;
                }

                let outcome = loop {
                    tokio::select! {
                        result = &mut handle => break result,
                        received = recv_optional(&mut rx) => {
                            match received {
                                Ok((seq, ev)) => {
                                    if seq > last_seq {
                                        last_seq = seq;
                                        self.forward_approval(&job.id, &occ.id, &ev).await;
                                    }
                                }
                                Err(broadcast::error::RecvError::Lagged(_)) => {
                                    let (more, _, newest) = self.hub.replay_from(&run_id, last_seq);
                                    if let Some(newest) = newest {
                                        last_seq = newest;
                                    }
                                    for (_, ev) in more {
                                        self.forward_approval(&job.id, &occ.id, &ev).await;
                                    }
                                }
                                Err(broadcast::error::RecvError::Closed) => {
                                    rx = None; // run handle gone; just await the outcome
                                }
                            }
                        }
                    }
                };

                match outcome {
                    Ok(Ok(_)) => {
                        self.set_occurrence(&occ.id, OccurrenceStatus::Succeeded, None, None)
                            .await;
                        let had_failures = self
                            .db
                            .get_job(job.id.clone())
                            .await
                            .ok()
                            .flatten()
                            .map(|j| j.consecutive_failures > 0)
                            .unwrap_or(false);
                        if self.db.reset_job_failures(job.id.clone()).await.is_ok() && had_failures
                        {
                            self.emit_job_updated(&job.id).await;
                        }
                    }
                    Ok(Err(RunError::Cancelled)) => {
                        // Human action: never counts toward the breaker.
                        self.set_occurrence(
                            &occ.id,
                            OccurrenceStatus::Failed,
                            None,
                            Some("cancelled by user".into()),
                        )
                        .await;
                    }
                    Ok(Err(RunError::Failed(msg))) => {
                        self.set_occurrence(
                            &occ.id,
                            OccurrenceStatus::Failed,
                            None,
                            Some(msg.clone()),
                        )
                        .await;
                        self.record_failure(&job.id, &msg).await;
                    }
                    Err(_) => {
                        // Join error: never log the panic payload.
                        self.set_occurrence(
                            &occ.id,
                            OccurrenceStatus::Failed,
                            None,
                            Some("run task crashed".into()),
                        )
                        .await;
                        self.record_failure(&job.id, "run task crashed").await;
                    }
                }
            }
        }

        self.release_admission(&occ.id);
        self.notify_for(&job.id).notify_waiters();
        drop(guard);
    }

    /// Fresh admission: workspace, pinned instructions (+ steer note), then
    /// engine admission. The occurrence stays `scheduled` throughout: the
    /// single Running write (WITH the run id) happens in `run_occurrence`
    /// only after admission succeeds, so a crash can never leave a Running
    /// row without a run id. Errors here fail the occurrence and count
    /// toward the breaker exactly like a failed run.
    async fn start_fresh(
        &self,
        job: &Job,
        occ: &Occurrence,
    ) -> Result<(String, tokio::task::JoinHandle<Result<String, RunError>>), RunError> {
        let workspace = job.workspace.clone().unwrap_or_else(|| {
            crate::job_scratch_dir(&job.id)
                .to_string_lossy()
                .into_owned()
        });
        if let Err(e) = std::fs::create_dir_all(&workspace) {
            return Err(RunError::Failed(format!(
                "creating job workspace '{workspace}': {e}"
            )));
        }

        let version = match self
            .db
            .get_routine_version(job.routine_id.clone(), job.routine_version)
            .await
        {
            Ok(Some(version)) => version,
            Ok(None) => {
                return Err(RunError::Failed(format!(
                    "pinned routine version {} of '{}' no longer exists",
                    job.routine_version, job.routine_id
                )));
            }
            Err(e) => {
                return Err(RunError::Failed(format!(
                    "loading pinned routine version: {e}"
                )));
            }
        };

        let instructions = steer_instructions(version.instructions, job.steer_note.clone());
        if job.steer_note.is_some() {
            // Consumed once: the note applies to this run only.
            if let Err(e) = self.db.set_job_steer_note(job.id.clone(), None).await {
                tracing::warn!(error = %e, "supervisor: clearing steer note failed");
            }
        }

        self.engine
            .start_job_run(JobRunSpec {
                job_id: job.id.clone(),
                occurrence_id: occ.id.clone(),
                conversation_id: job.conversation_id.clone(),
                task: format!("{} ({})", job.name, occ.scheduled_for),
                instructions,
                mode: version.mode,
                model: version.model,
                workspace: Some(workspace),
                budgets: job.budgets,
            })
            .await
    }

    /// Park/unpark forwarding from the run's event channel.
    async fn forward_approval(&self, job_id: &str, occ_id: &str, ev: &RunEvent) {
        match ev {
            RunEvent::ApprovalRequested {
                approval_id,
                run_id,
                title,
                ..
            } => {
                self.set_occurrence(occ_id, OccurrenceStatus::WaitingApproval, None, None)
                    .await;
                self.events.publish(JobEvent::ApprovalParked {
                    job_id: job_id.to_string(),
                    occurrence_id: occ_id.to_string(),
                    approval_id: approval_id.clone(),
                    run_id: run_id.clone(),
                    title: title.clone(),
                });
            }
            RunEvent::ApprovalResolved { .. } => {
                self.set_occurrence(occ_id, OccurrenceStatus::Running, None, None)
                    .await;
            }
            _ => {}
        }
    }

    // ----- catch-up -----

    /// Decision tree for one unprocessed interrupted occurrence (Decision 2:
    /// step-boundary continuation, never mid-loop replay).
    async fn process_interrupted(self: &Arc<Self>, job: &Job, occ: &Occurrence) {
        let Some(run_id) = occ.run_id.clone() else {
            // Crash-window recovery: the row was interrupted without any run
            // attached (the legacy admission ordering marked it Running
            // before the engine returned a run id, or it died mid-admission).
            // There is nothing to continue; reset to Scheduled (reason
            // cleared) so the due query re-admits it as a FRESH gated run.
            // Any orphaned run from the crash window was honestly interrupted
            // by the runs sweep; nothing consequential auto-replays —
            // approvals still park. Non-active jobs simply keep the row
            // scheduled until they are activated again.
            tracing::info!(
                job_id = %job.id,
                occurrence_id = %occ.id,
                "supervisor: occurrence interrupted without a run id; resetting to scheduled for fresh admission"
            );
            self.set_occurrence(&occ.id, OccurrenceStatus::Scheduled, None, None)
                .await;
            return;
        };

        if job.status != JobStatus::Active {
            self.set_occurrence(
                &occ.id,
                OccurrenceStatus::Interrupted,
                None,
                Some("deferred: job not active".into()),
            )
            .await;
            return;
        }

        // Parked approvals expired on restart: count toward the breaker,
        // never auto-continue (a human must re-decide).
        if self
            .db
            .occurrence_had_expired_approval(run_id.clone())
            .await
            .unwrap_or(false)
        {
            self.record_failure(&job.id, "parked approval expired on restart")
                .await;
            self.set_occurrence(
                &occ.id,
                OccurrenceStatus::Interrupted,
                None,
                Some("parked approval expired on restart".into()),
            )
            .await;
            return;
        }

        let calls = self
            .db
            .list_tool_calls(run_id.clone())
            .await
            .unwrap_or_default();
        match classify_last_step(&self.engine.registry, &calls) {
            ContinuationSafety::NeedsReview { tool, call_id } => {
                if let Err(e) = self
                    .db
                    .flag_occurrence_review(
                        occ.id.clone(),
                        format!(
                            "unconfirmed consequential step '{tool}' ({call_id}) needs human review"
                        ),
                    )
                    .await
                {
                    tracing::warn!(error = %e, "supervisor: flagging review failed");
                }
                self.set_occurrence(
                    &occ.id,
                    OccurrenceStatus::Interrupted,
                    None,
                    Some("review_flagged".into()),
                )
                .await;
            }
            ContinuationSafety::Safe => {
                let seed = self.build_continuation_seed(job, occ, &run_id).await;
                {
                    let mut admitting = self
                        .admitting
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    admitting.insert(occ.id.clone());
                }
                let sup = self.clone();
                let job = job.clone();
                let occ = occ.clone();
                tokio::spawn(async move {
                    sup.run_occurrence(job, occ, Admission::Continuation(seed))
                        .await
                });
            }
        }
    }

    /// Seed a continuation from persisted state: job system prompt + the
    /// bounded recent transcript (same 40-turn user/assistant window the
    /// engine uses) + a continuation instruction carrying the prior plan.
    async fn build_continuation_seed(
        &self,
        job: &Job,
        occ: &Occurrence,
        prior_run_id: &str,
    ) -> ContinuationSeed {
        let settings = self.engine.current_settings().await;
        let mut messages = vec![ChatMessage::system(job_system_prompt(&settings))];

        let stored = self
            .db
            .get_messages(job.conversation_id.clone())
            .await
            .unwrap_or_default();
        let window: Vec<_> = stored
            .iter()
            .filter(|m| matches!(m.role, Role::User | Role::Assistant))
            .collect();
        let skip = window.len().saturating_sub(40);
        for m in window.into_iter().skip(skip) {
            let mut message = match m.role {
                Role::User => ChatMessage::user(m.content.clone()),
                _ => ChatMessage::assistant(m.content.clone()),
            };
            message.attachments = m.attachments.clone();
            messages.push(message);
        }

        let plan = self
            .db
            .get_run(prior_run_id.to_string())
            .await
            .ok()
            .flatten()
            .map(|run| run.plan)
            .unwrap_or_default();

        // The steer note applies to the next occurrence/continuation and is
        // consumed once (plan decision: steering v1).
        let steer = job.steer_note.clone();
        if steer.is_some() {
            if let Err(e) = self.db.set_job_steer_note(job.id.clone(), None).await {
                tracing::warn!(error = %e, "supervisor: clearing steer note failed");
            }
        }
        messages.push(ChatMessage::user(continuation_instruction(
            &plan,
            steer.as_deref(),
        )));

        ContinuationSeed {
            job_id: job.id.clone(),
            occurrence_id: occ.id.clone(),
            prior_run_id: prior_run_id.to_string(),
            conversation_id: job.conversation_id.clone(),
            task: format!("{} (continuation)", job.name),
            messages,
            plan,
            review_flags: Vec::new(),
        }
    }

    /// Missed-tick policy: at most ONE catch-up occurrence, for the most
    /// recent missed tick (the unique constraint guards double fires).
    async fn catch_up_missed(
        self: &Arc<Self>,
        job: &Job,
        spec: &vortex_types::ScheduleSpec,
        now: &DateTime<Local>,
    ) {
        let recent = self
            .db
            .list_occurrences(job.id.clone(), 1)
            .await
            .unwrap_or_default();
        let last_fired = recent
            .first()
            .and_then(|o| parse_local(&o.scheduled_for))
            .or_else(|| parse_local(&job.created_at));
        let Some(last_fired) = last_fired else {
            return;
        };
        let missed = match clock::missed_ticks(spec, &last_fired, now) {
            Ok(missed) => missed,
            Err(e) => {
                tracing::warn!(error = %e, "supervisor: missed-tick enumeration failed");
                return;
            }
        };
        if missed.is_empty() || spec.missed_policy != MissedPolicy::RunOnceIfMissed {
            return;
        }
        let tick = missed.last().expect("non-empty checked above");
        match self
            .db
            .insert_occurrence(job.id.clone(), utc_string(tick))
            .await
        {
            Ok(Some(occ)) => self.admit(job.clone(), occ).await,
            Ok(None) => {} // already present (double-fire guard)
            Err(e) => {
                tracing::warn!(error = %e, "supervisor: catch-up occurrence insert failed");
            }
        }
    }

    // ----- schedule maintenance -----

    /// Fire the persisted next tick once it is due. `Ok(None)` from the
    /// insert means the tick already fired (double-fire guard): do nothing.
    async fn fire_due_tick(self: &Arc<Self>, job: &Job, now: &DateTime<Local>) {
        let Some(next) = job.next_run_at.as_deref().and_then(parse_local) else {
            return;
        };
        if next > *now {
            return;
        }
        match self
            .db
            .insert_occurrence(job.id.clone(), utc_string(&next))
            .await
        {
            Ok(Some(occ)) => self.admit(job.clone(), occ).await,
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(error = %e, "supervisor: occurrence insert failed");
            }
        }
    }

    /// Recompute `next_run_at` (next future tick for active scheduled jobs,
    /// cleared otherwise). Emits JobUpdated only when the value changes.
    async fn refresh_next_run(&self, job: &Job, now: &DateTime<Local>) {
        let desired: Option<String> = if job.status == JobStatus::Active {
            match job.schedule.as_ref() {
                Some(spec) => match clock::next_tick(spec, now) {
                    Ok(Some(tick)) => Some(utc_string(&tick)),
                    Ok(None) => None, // expression can never fire again
                    Err(e) => {
                        tracing::warn!(error = %e, "supervisor: next-tick computation failed");
                        None
                    }
                },
                None => None,
            }
        } else {
            None
        };
        if desired != job.next_run_at {
            match self.db.set_job_next_run(job.id.clone(), desired).await {
                Ok(true) => self.emit_job_updated(&job.id).await,
                Ok(false) => {}
                Err(e) => {
                    tracing::warn!(error = %e, "supervisor: updating next_run_at failed");
                }
            }
        }
    }

    // ----- outcomes / events -----

    /// Increment the consecutive-failure counter; trip the breaker at the
    /// threshold. Emits JobUpdated for the counter change (and the status
    /// change when the breaker trips).
    async fn record_failure(&self, job_id: &str, reason: &str) {
        let count = match self.db.record_job_failure(job_id.to_string()).await {
            Ok(count) => count,
            Err(e) => {
                tracing::warn!(error = %e, "supervisor: recording job failure failed");
                return;
            }
        };
        self.emit_job_updated(job_id).await;
        if count >= BREAKER_THRESHOLD {
            if let Err(e) = self
                .db
                .set_job_status(job_id.to_string(), JobStatus::CircuitBroken)
                .await
            {
                tracing::warn!(error = %e, "supervisor: tripping circuit breaker failed");
            }
            self.events.publish(JobEvent::JobCircuitBroken {
                job_id: job_id.to_string(),
                reason: format!("{count} consecutive failed occurrences; last failure: {reason}"),
            });
            self.emit_job_updated(job_id).await;
        }
    }

    /// Persist an occurrence transition and emit OccurrenceUpdated.
    async fn set_occurrence(
        &self,
        occ_id: &str,
        status: OccurrenceStatus,
        run_id: Option<String>,
        reason: Option<String>,
    ) {
        if let Err(e) = self
            .db
            .update_occurrence(occ_id.to_string(), status, run_id, reason)
            .await
        {
            tracing::warn!(error = %e, "supervisor: occurrence update failed");
            return;
        }
        match self.db.get_occurrence(occ_id.to_string()).await {
            Ok(Some(occ)) => self
                .events
                .publish(JobEvent::OccurrenceUpdated { occurrence: occ }),
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(error = %e, "supervisor: re-reading occurrence failed");
            }
        }
    }

    /// Emit JobUpdated with the fresh job row (callers invoke this only when
    /// status / next_run_at / failure counter actually changed).
    async fn emit_job_updated(&self, job_id: &str) {
        match self.db.get_job(job_id.to_string()).await {
            Ok(Some(job)) => self.events.publish(JobEvent::JobUpdated { job }),
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(error = %e, "supervisor: re-reading job failed");
            }
        }
    }

    fn notify_for(&self, job_id: &str) -> Arc<Notify> {
        let mut map = self
            .job_notify
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        map.entry(job_id.to_string()).or_default().clone()
    }

    fn release_admission(&self, occ_id: &str) {
        self.admitting
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(occ_id);
    }
}

/// Receive from an optional broadcast receiver; a `None` receiver (run handle
/// already removed) pends forever so the select loop just awaits the outcome.
async fn recv_optional(
    rx: &mut Option<broadcast::Receiver<(u64, RunEvent)>>,
) -> Result<(u64, RunEvent), broadcast::error::RecvError> {
    match rx {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

/// Gate limit: 0 means the default (2); otherwise clamped to 1..=16 by the
/// scheduler itself.
fn gate_limit(requested: usize) -> usize {
    if requested == 0 {
        DEFAULT_JOB_GATE
    } else {
        requested
    }
}

/// UTC RFC 3339 millis — the frozen storage format (freeze decision #9), so
/// lexicographic SQL comparison is chronological.
fn utc_string<Tz: TimeZone>(t: &DateTime<Tz>) -> String {
    t.with_timezone(&Utc)
        .to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn parse_local(s: &str) -> Option<DateTime<Local>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|t| t.with_timezone(&Local))
}

/// Local midnight of `now`'s date, as a UTC RFC 3339 millis string (daily-cap
/// window start). Nonexistent local midtimes (DST) resolve to the earliest
/// instant, matching the clock module's contract.
fn local_midnight_utc(now: &DateTime<Local>) -> String {
    let midnight = now
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .expect("00:00:00 is a valid wall time");
    let local = Local
        .from_local_datetime(&midnight)
        .earliest()
        .unwrap_or_else(|| now.with_timezone(&Local));
    utc_string(&local)
}

/// Append the one-shot steer note to pinned routine instructions.
fn steer_instructions(instructions: String, note: Option<String>) -> String {
    match note {
        Some(note) => {
            format!("{instructions}\n\nSTEERING NOTE FROM THE USER (apply to this run): {note}")
        }
        None => instructions,
    }
}

/// Daily-cap decision: `count` is today's non-skipped occurrence total; when
/// the occurrence being admitted was created today it is part of that count,
/// so compare the OTHER occurrences against the cap.
fn cap_reached(count: u32, cap: u32, counts_self: bool) -> bool {
    let others = if counts_self {
        count.saturating_sub(1)
    } else {
        count
    };
    others >= cap
}

fn step_status_str(status: StepStatus) -> &'static str {
    match status {
        StepStatus::Pending => "pending",
        StepStatus::InProgress => "in_progress",
        StepStatus::Done => "done",
        StepStatus::Skipped => "skipped",
    }
}

/// The continuation instruction: honest about the restart, carries the prior
/// plan, and forbids redoing completed steps.
fn continuation_instruction(plan: &[PlanStep], steer: Option<&str>) -> String {
    let mut s = String::from(
        "The previous run of this job was interrupted by a server restart. Here is the plan so far:\n",
    );
    if plan.is_empty() {
        s.push_str("(no plan was recorded)\n");
    } else {
        for step in plan {
            s.push_str(&format!(
                "- [{}] {}\n",
                step_status_str(step.status),
                step.title
            ));
        }
    }
    s.push_str(
        "Continue from where it left off. Do NOT redo completed steps: pick up the first \
         unfinished step and carry the work forward to completion. If a step's outcome is \
         unclear, verify its effect before repeating any part of it.",
    );
    if let Some(note) = steer {
        s.push_str(&format!(
            "\n\nSTEERING NOTE FROM THE USER (apply to this run): {note}"
        ));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn local_at(y: i32, m: u32, d: u32, h: u32, min: u32) -> DateTime<Local> {
        Local.from_utc_datetime(
            &NaiveDate::from_ymd_opt(y, m, d)
                .unwrap()
                .and_hms_opt(h, min, 0)
                .unwrap(),
        )
    }

    #[test]
    fn gate_limit_zero_means_default_and_scheduler_clamps() {
        assert_eq!(gate_limit(0), 2);
        assert_eq!(gate_limit(1), 1);
        assert_eq!(gate_limit(7), 7);
        assert_eq!(Scheduler::new(gate_limit(0)).limit(), 2);
        assert_eq!(Scheduler::new(gate_limit(1)).limit(), 1);
        assert_eq!(Scheduler::new(gate_limit(99)).limit(), 16);
    }

    #[test]
    fn job_event_log_replay_and_gap_semantics() {
        let log = JobEventLog::new();
        let mut rx = log.tx.subscribe();
        let ev = |i: usize| JobEvent::JobCircuitBroken {
            job_id: format!("j{i}"),
            reason: "r".into(),
        };
        for i in 0..10 {
            log.publish(ev(i));
        }
        let (all, oldest, newest) = log.replay_from(0);
        assert_eq!((oldest, newest), (Some(1), Some(10)));
        assert_eq!(
            all.iter().map(|(s, _)| *s).collect::<Vec<_>>(),
            (1..=10).collect::<Vec<_>>()
        );
        let (tail, _, _) = log.replay_from(7);
        assert_eq!(
            tail.iter().map(|(s, _)| *s).collect::<Vec<_>>(),
            vec![8, 9, 10]
        );
        let (none, oldest, newest) = log.replay_from(10);
        assert!(none.is_empty());
        assert_eq!((oldest, newest), (Some(1), Some(10)));
        let (seq, _) = rx.try_recv().unwrap();
        assert_eq!(seq, 1);

        // Overflow evicts the oldest; a stale cursor sees the gap via oldest.
        for i in 10..(MAX_JOB_LOG + 25) {
            log.publish(ev(i));
        }
        let total = MAX_JOB_LOG + 25;
        let (all, oldest, newest) = log.replay_from(0);
        assert_eq!(oldest, Some((total - MAX_JOB_LOG + 1) as u64));
        assert_eq!(newest, Some(total as u64));
        assert_eq!(all.len(), MAX_JOB_LOG);
        let (from_stale, oldest, _) = log.replay_from(5);
        assert_eq!(oldest, Some((total - MAX_JOB_LOG + 1) as u64));
        assert_eq!(
            from_stale.first().map(|(s, _)| *s),
            Some((total - MAX_JOB_LOG + 1) as u64)
        );
    }

    #[test]
    fn utc_string_is_rfc3339_millis_z() {
        let t = local_at(2026, 6, 15, 16, 5);
        let s = utc_string(&t);
        assert!(s.ends_with("Z"), "{s}");
        assert!(s.contains(".000"), "{s}");
        assert_eq!(
            DateTime::parse_from_rfc3339(&s).unwrap().timestamp_millis(),
            t.timestamp_millis()
        );
    }

    #[test]
    fn local_midnight_utc_is_local_midnight_in_utc() {
        for now in [
            local_at(2026, 6, 15, 16, 5),
            local_at(2026, 1, 1, 0, 0),
            local_at(2026, 12, 31, 23, 59),
        ] {
            let s = local_midnight_utc(&now);
            let parsed = DateTime::parse_from_rfc3339(&s)
                .unwrap()
                .with_timezone(&Local);
            assert_eq!(parsed.date_naive(), now.date_naive());
            assert_eq!(
                parsed.time(),
                chrono::NaiveTime::from_hms_opt(0, 0, 0).unwrap()
            );
            assert!(parsed <= now);
        }
    }

    #[test]
    fn steer_note_appended_once() {
        assert_eq!(steer_instructions("base".into(), None), "base");
        let with = steer_instructions("base".into(), Some("focus on X".into()));
        assert!(with.starts_with("base\n\n"));
        assert!(with.contains("STEERING NOTE FROM THE USER (apply to this run): focus on X"));
    }

    #[test]
    fn daily_cap_counts_other_occurrences() {
        // Self created today is part of the count: cap 1 allows the first.
        assert!(!cap_reached(1, 1, true));
        assert!(cap_reached(2, 1, true));
        assert!(cap_reached(3, 2, true));
        assert!(!cap_reached(2, 2, true));
        // Pre-restart row created yesterday: not part of today's count.
        assert!(cap_reached(1, 1, false));
        assert!(!cap_reached(0, 1, false));
        assert!(!cap_reached(0, 1, true));
    }

    #[test]
    fn continuation_instruction_carries_plan_and_rules() {
        let plan = vec![
            PlanStep {
                title: "gather sources".into(),
                status: StepStatus::Done,
            },
            PlanStep {
                title: "write digest".into(),
                status: StepStatus::InProgress,
            },
        ];
        let s = continuation_instruction(&plan, None);
        assert!(s.contains("interrupted by a server restart"));
        assert!(s.contains("- [done] gather sources"));
        assert!(s.contains("- [in_progress] write digest"));
        assert!(s.contains("Do NOT redo completed steps"));
        assert!(!s.contains("STEERING NOTE"));
        let empty = continuation_instruction(&[], Some("be brief"));
        assert!(empty.contains("(no plan was recorded)"));
        assert!(empty.contains("STEERING NOTE FROM THE USER (apply to this run): be brief"));
    }
}
