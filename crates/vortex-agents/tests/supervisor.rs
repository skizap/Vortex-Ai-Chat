//! Supervisor integration tests: fully offline (tempdir SQLite, scripted mock
//! LLM, fake clock, real Engine). No wall-clock sleeps: time is virtual
//! (`tokio::time::pause` + `advance`) and the lifecycle is driven manually
//! through `tick_once` / `catch_up`.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::{DateTime, Duration, Local, NaiveDate, SecondsFormat, TimeZone, Utc};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use vortex_agents::clock::{self, Clock};
use vortex_agents::engine::Engine;
use vortex_agents::errors::{Risk, ToolError};
use vortex_agents::registry::{Tool, ToolContext, ToolRegistry, ToolResult};
use vortex_agents::schema::ToolDef;
use vortex_agents::supervisor::Supervisor;
use vortex_agents::{Hub, PreviewRegistry, Scheduler};
use vortex_core::Db;
use vortex_llm::llm::{CompletionRequest, CompletionResponse, LlmClient, LlmEvent};
use vortex_llm::{MockLlm, MockTurn};
use vortex_types::{
    ApprovalStatus, ChatMessage, Job, JobBudgets, JobStatus, MissedPolicy, Mode, NewJob,
    NewRoutine, OccurrenceStatus, OverlapPolicy, PlanStep, Role, RunInfo, RunStatus, ScheduleSpec,
    Settings, StepStatus, ToolCallRecord,
};

// ----- fixtures -----

struct FakeClock {
    now: Mutex<DateTime<Local>>,
}

impl FakeClock {
    fn new(t: DateTime<Local>) -> Self {
        Self { now: Mutex::new(t) }
    }

    fn set(&self, t: DateTime<Local>) {
        *self.now.lock().unwrap() = t;
    }
}

impl Clock for FakeClock {
    fn now(&self) -> DateTime<Local> {
        *self.now.lock().unwrap()
    }
}

/// Fixed base instant: 2026-06-15 16:03 UTC (12:03 in AST, 16:03 in UTC —
/// `*/5` ticks land on the same absolute instants under either host zone).
fn t0() -> DateTime<Local> {
    Local.from_utc_datetime(
        &NaiveDate::from_ymd_opt(2026, 6, 15)
            .unwrap()
            .and_hms_opt(16, 3, 0)
            .unwrap(),
    )
}

fn utc(t: &DateTime<Local>) -> String {
    t.with_timezone(&Utc)
        .to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn parse_local(s: &str) -> DateTime<Local> {
    DateTime::parse_from_rfc3339(s)
        .unwrap()
        .with_timezone(&Local)
}

fn spec(cron: &str) -> ScheduleSpec {
    ScheduleSpec {
        cron: cron.into(),
        tz: "local".into(),
        missed_policy: MissedPolicy::RunOnceIfMissed,
    }
}

fn next_tick_utc(cron: &str, after: &DateTime<Local>) -> String {
    utc(&clock::next_tick(&spec(cron), after).unwrap().unwrap())
}

/// Advance virtual time in small steps, yielding between steps so spawned
/// admission tasks and their spawn_blocking DB round-trips make progress.
async fn advance(ms: u64) {
    let mut left = ms;
    while left > 0 {
        let step = left.min(5);
        tokio::time::advance(std::time::Duration::from_millis(step)).await;
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        left -= step;
    }
}

/// Enough virtual time for a default-delay mock run (and its DB writes) to
/// finish from any starting point.
async fn settle() {
    advance(2000).await;
}

/// Poll an occurrence's status in small virtual-time steps until it reaches
/// `want` (spawn_blocking DB hops complete in real time, so intermediate
/// states must be observed by condition, not by a fixed advance).
async fn wait_occ(db: &Db, occ_id: &str, want: OccurrenceStatus) {
    for _ in 0..600 {
        let got = db
            .get_occurrence(occ_id.to_string())
            .await
            .unwrap()
            .unwrap();
        if got.status == want {
            return;
        }
        advance(5).await;
    }
    panic!("occurrence {occ_id} never reached {want:?}");
}

async fn first_occ(db: &Db, job: &Job) -> vortex_types::Occurrence {
    let mut occs = db.list_occurrences(job.id.clone(), 100).await.unwrap();
    occs.sort_by(|a, b| a.scheduled_for.cmp(&b.scheduled_for));
    occs.into_iter().next().expect("at least one occurrence")
}

/// Mock LLM that records every request's messages (steer-note / continuation
/// seeding assertions inspect what the model actually received).
struct RecordingLlm {
    inner: MockLlm,
    seen: Mutex<Vec<Vec<ChatMessage>>>,
}

impl RecordingLlm {
    fn new(inner: MockLlm) -> Self {
        Self {
            inner,
            seen: Mutex::new(Vec::new()),
        }
    }

    fn requests_containing(&self, needle: &str) -> usize {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|msgs| msgs.iter().any(|m| m.content.contains(needle)))
            .count()
    }
}

#[async_trait]
impl LlmClient for RecordingLlm {
    async fn complete(
        &self,
        req: &CompletionRequest,
        events: mpsc::Sender<LlmEvent>,
    ) -> Result<CompletionResponse, vortex_llm::LlmError> {
        self.seen.lock().unwrap().push(req.messages.clone());
        self.inner.complete(req, events).await
    }

    async fn models(&self) -> Result<Vec<vortex_types::ModelInfo>, vortex_llm::LlmError> {
        self.inner.models().await
    }

    fn describe(&self) -> &'static str {
        "recording-mock"
    }
}

/// Mock whose catalog fetch sleeps (virtual time), widening the engine
/// admission window so the occurrence's pre-admission state is observable.
struct SlowCatalogLlm {
    inner: MockLlm,
}

#[async_trait]
impl LlmClient for SlowCatalogLlm {
    async fn complete(
        &self,
        req: &CompletionRequest,
        events: mpsc::Sender<LlmEvent>,
    ) -> Result<CompletionResponse, vortex_llm::LlmError> {
        self.inner.complete(req, events).await
    }

    async fn models(&self) -> Result<Vec<vortex_types::ModelInfo>, vortex_llm::LlmError> {
        // start_job_run resolves Vortex Auto against the catalog BEFORE it
        // returns a run id; this sleep holds admission open deterministically.
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        self.inner.models().await
    }

    fn describe(&self) -> &'static str {
        "slow-catalog"
    }
}

/// A consequential test tool for the review-flag classification path.
struct ConsequentialProbe;

static PROBE_DEF: ToolDef = ToolDef {
    name: "test_consequential",
    description: "test consequential probe",
    params: &[],
};

#[async_trait]
impl Tool for ConsequentialProbe {
    fn def(&self) -> &'static ToolDef {
        &PROBE_DEF
    }

    fn risk(&self) -> Risk {
        Risk::Consequential
    }

    async fn execute(
        &self,
        _ctx: &ToolContext,
        _args: &serde_json::Value,
    ) -> Result<ToolResult, ToolError> {
        Ok(ToolResult::text("probe done"))
    }
}

struct Setup {
    sup: Arc<Supervisor>,
    db: Db,
    clock: Arc<FakeClock>,
    ws: std::path::PathBuf,
    _dir: tempfile::TempDir,
}

async fn setup(llm: Arc<dyn LlmClient>, gate: usize) -> Setup {
    setup_with(llm, gate, None).await
}

async fn setup_with(
    llm: Arc<dyn LlmClient>,
    gate: usize,
    registry: Option<Arc<ToolRegistry>>,
) -> Setup {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("vortex.db")).unwrap();
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let settings = Settings {
        workspace: Some(ws.to_string_lossy().to_string()),
        model: "mock/model".into(),
        ..Default::default()
    };
    let hub = Arc::new(Hub::new());
    let scheduler = Arc::new(Scheduler::new(settings.agents.max_concurrent));
    let browser = Arc::new(vortex_agents::browser::BrowserManager::new(
        dir.path().join("profile"),
        true,
    ));
    let previews = Arc::new(PreviewRegistry::new());
    let engine_slot = Arc::new(std::sync::OnceLock::new());
    let registry = registry
        .unwrap_or_else(|| vortex_agents::build_registry(previews.clone(), engine_slot.clone()));
    let approvals = Arc::new(vortex_agents::approvals::ApprovalManager::new(
        db.clone(),
        hub.clone(),
        settings.agents.approval_timeout_secs,
    ));
    let engine = Engine::new(
        llm,
        registry,
        hub.clone(),
        scheduler,
        approvals,
        db.clone(),
        browser,
        previews,
        None,
        vortex_agents::safe_http::SafeHttp::new(false),
        settings,
        None,
    );
    let _ = engine_slot.set(engine.clone());
    let clock = Arc::new(FakeClock::new(t0()));
    let sup = Supervisor::new(
        db.clone(),
        engine,
        hub,
        clock.clone(),
        gate,
        CancellationToken::new(),
    );
    Setup {
        sup,
        db,
        clock,
        ws,
        _dir: dir,
    }
}

async fn make_job(
    db: &Db,
    ws: &std::path::Path,
    name: &str,
    overlap: OverlapPolicy,
    missed: MissedPolicy,
    budgets: JobBudgets,
) -> Job {
    let routine = db
        .create_routine(NewRoutine {
            name: format!("{name}-routine"),
            description: String::new(),
            instructions: "run the job".into(),
            mode: Mode::Chat,
            model: None,
        })
        .await
        .unwrap();
    db.activate_routine(routine.id.clone()).await.unwrap();
    let job = db
        .create_job(NewJob {
            name: name.into(),
            routine_id: routine.id.clone(),
            routine_version: 1,
            schedule: Some(ScheduleSpec {
                cron: "*/5 * * * *".into(),
                tz: "local".into(),
                missed_policy: missed,
            }),
            overlap,
            budgets,
            workspace: Some(ws.to_string_lossy().to_string()),
        })
        .await
        .unwrap();
    db.set_job_status(job.id.clone(), JobStatus::Active)
        .await
        .unwrap();
    // Backdate the row into the fake clock's era: catch-up derives the
    // missed-tick window from created_at when no occurrence exists yet.
    // (Both interpolated values are test-controlled: a UUID and a fixed
    // RFC 3339 instant.)
    let created = utc(&(t0() - Duration::hours(1)));
    let sql = format!(
        "UPDATE jobs SET created_at = '{created}', updated_at = '{created}' WHERE id = '{}'",
        job.id
    );
    db.with_conn(move |c| {
        c.execute(&sql, [])?;
        Ok(())
    })
    .await
    .unwrap();
    db.get_job(job.id).await.unwrap().unwrap()
}

async fn occurrences(db: &Db, job: &Job) -> Vec<vortex_types::Occurrence> {
    let mut occs = db.list_occurrences(job.id.clone(), 100).await.unwrap();
    occs.sort_by(|a, b| a.scheduled_for.cmp(&b.scheduled_for));
    occs
}

fn prior_run(id: &str, conversation_id: &str, plan: Vec<PlanStep>) -> RunInfo {
    RunInfo {
        id: id.into(),
        conversation_id: Some(conversation_id.into()),
        parent_run_id: None,
        agent: "coordinator".into(),
        task: "prior job run".into(),
        model: "mock/model".into(),
        status: RunStatus::Interrupted,
        error: None,
        usage: None,
        plan,
        depth: 0,
        created_at: vortex_core::db::now(),
        updated_at: vortex_core::db::now(),
    }
}

// ----- 1. schedule → fire → run → succeed -----

#[tokio::test]
async fn schedule_fire_run_succeed() {
    tokio::time::pause();
    let s = setup(Arc::new(MockLlm::new(vec![MockTurn::text("job done")])), 2).await;
    let job = make_job(
        &s.db,
        &s.ws,
        "digest",
        OverlapPolicy::Skip,
        MissedPolicy::RunOnceIfMissed,
        JobBudgets::default(),
    )
    .await;

    // First pass seeds next_run_at (the next future tick).
    s.sup.tick_once().await;
    let tick = next_tick_utc("*/5 * * * *", &t0());
    let seeded = s.db.get_job(job.id.clone()).await.unwrap().unwrap();
    assert_eq!(seeded.next_run_at.as_deref(), Some(tick.as_str()));

    // Clock reaches the tick: the occurrence fires and completes.
    s.clock.set(parse_local(&tick));
    s.sup.tick_once().await;
    settle().await;

    let occs = occurrences(&s.db, &job).await;
    assert_eq!(occs.len(), 1);
    assert_eq!(occs[0].scheduled_for, tick);
    assert_eq!(occs[0].status, OccurrenceStatus::Succeeded);
    let run =
        s.db.get_run(occs[0].run_id.clone().unwrap())
            .await
            .unwrap()
            .unwrap();
    assert_eq!(run.status, RunStatus::Completed);
    let msgs =
        s.db.get_messages(job.conversation_id.clone())
            .await
            .unwrap();
    assert!(
        msgs.iter()
            .any(|m| m.role == Role::Assistant && m.content == "job done"),
        "assistant reply must land in the job conversation"
    );
    let after = s.db.get_job(job.id.clone()).await.unwrap().unwrap();
    assert_eq!(
        after.next_run_at.as_deref(),
        Some(next_tick_utc("*/5 * * * *", &parse_local(&tick)).as_str()),
        "next_run_at advances to the next future tick"
    );
    assert_eq!(after.consecutive_failures, 0);
    let (events, _, _) = s.sup.replay_from(0);
    assert!(events.iter().any(|(_, e)| matches!(
        e,
        vortex_types::JobEvent::OccurrenceUpdated { occurrence }
            if occurrence.status == OccurrenceStatus::Succeeded
    )));
}

// ----- 2/3. missed-tick policies -----

#[tokio::test]
async fn missed_ticks_run_once() {
    tokio::time::pause();
    let s = setup(
        Arc::new(MockLlm::new(vec![MockTurn::text("catch-up done")])),
        2,
    )
    .await;
    let job = make_job(
        &s.db,
        &s.ws,
        "missed",
        OverlapPolicy::Skip,
        MissedPolicy::RunOnceIfMissed,
        JobBudgets::default(),
    )
    .await;

    // Jump past three ticks (16:05, 16:10, 16:15) to 16:18.
    s.clock.set(t0() + Duration::minutes(15));
    s.sup.catch_up().await;
    settle().await;

    let occs = occurrences(&s.db, &job).await;
    assert_eq!(occs.len(), 1, "exactly ONE catch-up occurrence");
    assert_eq!(
        occs[0].scheduled_for,
        utc(&(t0() + Duration::minutes(12))),
        "the catch-up occurrence belongs to the MOST RECENT missed tick"
    );
    assert_eq!(occs[0].status, OccurrenceStatus::Succeeded);
    let after = s.db.get_job(job.id.clone()).await.unwrap().unwrap();
    assert_eq!(
        after.next_run_at.as_deref(),
        Some(utc(&(t0() + Duration::minutes(17))).as_str())
    );
}

#[tokio::test]
async fn missed_skip_policy() {
    tokio::time::pause();
    let s = setup(Arc::new(MockLlm::new(vec![MockTurn::text("never")])), 2).await;
    let job = make_job(
        &s.db,
        &s.ws,
        "skipper",
        OverlapPolicy::Skip,
        MissedPolicy::Skip,
        JobBudgets::default(),
    )
    .await;

    s.clock.set(t0() + Duration::minutes(15));
    s.sup.catch_up().await;
    settle().await;

    assert!(
        occurrences(&s.db, &job).await.is_empty(),
        "Skip policy drops missed ticks entirely"
    );
    let after = s.db.get_job(job.id.clone()).await.unwrap().unwrap();
    assert_eq!(
        after.next_run_at.as_deref(),
        Some(utc(&(t0() + Duration::minutes(17))).as_str()),
        "the next live tick is still scheduled"
    );
}

// ----- 4/5. single-flight and queue_one -----

#[tokio::test]
async fn single_flight_overlap_skip() {
    tokio::time::pause();
    let s = setup(
        Arc::new(MockLlm::with_delay(vec![MockTurn::text("slow reply")], 250)),
        2,
    )
    .await;
    let job = make_job(
        &s.db,
        &s.ws,
        "slow",
        OverlapPolicy::Skip,
        MissedPolicy::RunOnceIfMissed,
        JobBudgets::default(),
    )
    .await;

    s.sup.tick_once().await; // seed next_run_at = 16:05
    s.clock.set(t0() + Duration::minutes(2));
    s.sup.tick_once().await;
    let occs = occurrences(&s.db, &job).await;
    assert_eq!(occs.len(), 1);
    wait_occ(&s.db, &occs[0].id, OccurrenceStatus::Running).await;

    // Next tick while the first run is still live: skipped, never overlapped.
    s.clock.set(t0() + Duration::minutes(7));
    s.sup.tick_once().await;
    let occs = occurrences(&s.db, &job).await;
    assert_eq!(occs.len(), 2);
    assert_eq!(occs[1].status, OccurrenceStatus::Skipped);
    assert_eq!(
        occs[1].reason.as_deref(),
        Some("previous occurrence still running")
    );
    let live = s.db.list_live_occurrences(job.id.clone()).await.unwrap();
    assert_eq!(live.len(), 1, "never two live occurrences under Skip");
    assert_eq!(live[0].id, occs[0].id);

    wait_occ(&s.db, &occs[0].id, OccurrenceStatus::Succeeded).await;
    assert!(s
        .db
        .list_live_occurrences(job.id.clone())
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn queue_one() {
    tokio::time::pause();
    let s = setup(
        Arc::new(MockLlm::with_delay(
            vec![MockTurn::text("slow reply"), MockTurn::text("queued reply")],
            250,
        )),
        2,
    )
    .await;
    let job = make_job(
        &s.db,
        &s.ws,
        "queued",
        OverlapPolicy::QueueOne,
        MissedPolicy::RunOnceIfMissed,
        JobBudgets::default(),
    )
    .await;

    s.sup.tick_once().await; // seed 16:05
    s.clock.set(t0() + Duration::minutes(2));
    s.sup.tick_once().await;
    let occs = occurrences(&s.db, &job).await;
    wait_occ(&s.db, &occs[0].id, OccurrenceStatus::Running).await;

    // Second tick: queued behind the live one (stays scheduled, not running).
    s.clock.set(t0() + Duration::minutes(7));
    s.sup.tick_once().await;
    advance(20).await;
    let occs = occurrences(&s.db, &job).await;
    assert_eq!(occs.len(), 2);
    assert_eq!(occs[0].status, OccurrenceStatus::Running);
    assert_eq!(
        occs[1].status,
        OccurrenceStatus::Scheduled,
        "the queued occurrence waits, it does not run concurrently"
    );

    // Third tick: one live + one queued already → skipped.
    s.clock.set(t0() + Duration::minutes(12));
    s.sup.tick_once().await;
    let occs = occurrences(&s.db, &job).await;
    assert_eq!(occs.len(), 3);
    assert_eq!(occs[2].status, OccurrenceStatus::Skipped);

    // First run finishes → the queued occurrence takes over and completes.
    wait_occ(&s.db, &occs[0].id, OccurrenceStatus::Succeeded).await;
    wait_occ(&s.db, &occs[1].id, OccurrenceStatus::Running).await;
    wait_occ(&s.db, &occs[1].id, OccurrenceStatus::Succeeded).await;
    let occs = occurrences(&s.db, &job).await;
    assert_eq!(occs[2].status, OccurrenceStatus::Skipped);
    assert!(s
        .db
        .list_live_occurrences(job.id.clone())
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        s.db.list_runs_for_conversation(job.conversation_id.clone())
            .await
            .unwrap()
            .len(),
        2,
        "exactly the live run and the queued run executed"
    );
}

// ----- 6. circuit breaker -----

#[tokio::test]
async fn circuit_breaker() {
    tokio::time::pause();
    let s = setup(
        Arc::new(MockLlm::new(vec![MockTurn::failure("provider down"); 12])),
        2,
    )
    .await;
    let job = make_job(
        &s.db,
        &s.ws,
        "flaky",
        OverlapPolicy::Skip,
        MissedPolicy::RunOnceIfMissed,
        JobBudgets::default(),
    )
    .await;

    s.sup.tick_once().await; // seed 16:05
    for i in 1..=3 {
        s.clock.set(t0() + Duration::minutes(5 * i));
        s.sup.tick_once().await;
        settle().await;
        let occs = occurrences(&s.db, &job).await;
        assert_eq!(occs.len() as i64, i);
        assert_eq!(occs[i as usize - 1].status, OccurrenceStatus::Failed);
    }

    let after = s.db.get_job(job.id.clone()).await.unwrap().unwrap();
    assert_eq!(after.status, JobStatus::CircuitBroken);
    assert_eq!(after.consecutive_failures, 3);
    let (events, _, _) = s.sup.replay_from(0);
    assert!(events.iter().any(|(_, e)| matches!(
        e,
        vortex_types::JobEvent::JobCircuitBroken { job_id, .. } if job_id == &job.id
    )));

    // Fourth tick: a circuit-broken job never fires.
    s.clock.set(t0() + Duration::minutes(20));
    s.sup.tick_once().await;
    settle().await;
    assert_eq!(occurrences(&s.db, &job).await.len(), 3);
}

// ----- 7. per-occurrence turn budget -----

#[tokio::test]
async fn budget_exhausted() {
    tokio::time::pause();
    let llm = Arc::new(MockLlm::new(vec![
        MockTurn::tool_call(
            "update_plan",
            "c1",
            serde_json::json!({ "steps": ["one step"] }).to_string(),
        ),
        MockTurn::text("should never be requested"),
    ]));
    let s = setup(llm.clone(), 2).await;
    let job = make_job(
        &s.db,
        &s.ws,
        "budgeted",
        OverlapPolicy::Skip,
        MissedPolicy::RunOnceIfMissed,
        JobBudgets {
            max_turns: 1,
            ..JobBudgets::default()
        },
    )
    .await;

    s.sup.tick_once().await;
    s.clock.set(t0() + Duration::minutes(2));
    s.sup.tick_once().await;
    settle().await;

    let occs = occurrences(&s.db, &job).await;
    assert_eq!(occs.len(), 1);
    assert_eq!(occs[0].status, OccurrenceStatus::Failed);
    let reason = occs[0].reason.clone().unwrap_or_default();
    assert!(
        reason.contains("iteration limit"),
        "turn cap must surface as the iteration-limit failure, got: {reason}"
    );
    assert_eq!(llm.calls(), 1, "no second model call past the turn cap");
    let after = s.db.get_job(job.id.clone()).await.unwrap().unwrap();
    assert_eq!(after.consecutive_failures, 1);
}

// ----- 8. daily occurrence cap -----

#[tokio::test]
async fn daily_cap() {
    tokio::time::pause();
    let s = setup(
        Arc::new(MockLlm::new(vec![
            MockTurn::text("first"),
            MockTurn::text("second"),
        ])),
        2,
    )
    .await;
    let job = make_job(
        &s.db,
        &s.ws,
        "capped",
        OverlapPolicy::Skip,
        MissedPolicy::RunOnceIfMissed,
        JobBudgets {
            daily_occurrence_cap: 1,
            ..JobBudgets::default()
        },
    )
    .await;

    s.sup.tick_once().await;
    s.clock.set(t0() + Duration::minutes(2));
    s.sup.tick_once().await;
    settle().await;
    let occs = occurrences(&s.db, &job).await;
    assert_eq!(occs.len(), 1);
    assert_eq!(occs[0].status, OccurrenceStatus::Succeeded);

    // Same local day, next tick: skipped by the cap.
    s.clock.set(t0() + Duration::minutes(7));
    s.sup.tick_once().await;
    settle().await;
    let occs = occurrences(&s.db, &job).await;
    assert_eq!(occs.len(), 2);
    assert_eq!(occs[1].status, OccurrenceStatus::Skipped);
    assert!(occs[1]
        .reason
        .as_deref()
        .unwrap_or_default()
        .contains("daily occurrence cap"));
}

// ----- 9. restart → safe continuation -----

#[tokio::test]
async fn restart_continuation_safe() {
    tokio::time::pause();
    let rec = Arc::new(RecordingLlm::new(MockLlm::with_delay(
        vec![MockTurn::text("continued work")],
        1000,
    )));
    let s = setup(rec.clone(), 2).await;
    let job = make_job(
        &s.db,
        &s.ws,
        "resume",
        OverlapPolicy::Skip,
        MissedPolicy::RunOnceIfMissed,
        JobBudgets::default(),
    )
    .await;

    // Prior run interrupted mid-plan; last tool call confirmed (none here).
    s.db.upsert_run(&prior_run(
        "prior-run-1",
        &job.conversation_id,
        vec![
            PlanStep {
                title: "step one".into(),
                status: StepStatus::Done,
            },
            PlanStep {
                title: "step two".into(),
                status: StepStatus::InProgress,
            },
        ],
    ))
    .await
    .unwrap();
    s.db.add_message(
        job.conversation_id.clone(),
        ChatMessage::user("start the job"),
    )
    .await
    .unwrap();
    s.db.add_message(
        job.conversation_id.clone(),
        ChatMessage::assistant("working on step one"),
    )
    .await
    .unwrap();
    let occ =
        s.db.insert_occurrence(job.id.clone(), utc(&(t0() + Duration::minutes(2))))
            .await
            .unwrap()
            .unwrap();
    s.db.update_occurrence(
        occ.id.clone(),
        OccurrenceStatus::Interrupted,
        Some("prior-run-1".into()),
        None,
    )
    .await
    .unwrap();

    s.sup.catch_up().await;
    // The 1000ms-per-delta mock keeps the continuation live while we observe
    // the Running transition (poll, don't assume a fixed virtual instant).
    wait_occ(&s.db, &occ.id, OccurrenceStatus::Running).await;

    let got = s.db.get_occurrence(occ.id.clone()).await.unwrap().unwrap();
    assert_eq!(got.status, OccurrenceStatus::Running);
    assert_eq!(got.reason.as_deref(), Some("continuation_spawned"));
    let new_run = got.run_id.clone().unwrap();
    assert_ne!(new_run, "prior-run-1");

    // The model received the restart context: plan + no-redo rule + transcript.
    // The request is recorded when the provider task starts, which lags the
    // Running transition by a few async hops — poll for it.
    for _ in 0..600 {
        if rec.requests_containing("interrupted by a server restart") > 0 {
            break;
        }
        advance(5).await;
    }
    assert_eq!(
        rec.requests_containing("interrupted by a server restart"),
        1
    );
    assert_eq!(rec.requests_containing("- [done] step one"), 1);
    assert_eq!(rec.requests_containing("Do NOT redo completed steps"), 1);
    assert_eq!(rec.requests_containing("working on step one"), 1);

    wait_occ(&s.db, &occ.id, OccurrenceStatus::Succeeded).await;
    let msgs =
        s.db.get_messages(job.conversation_id.clone())
            .await
            .unwrap();
    assert!(msgs
        .iter()
        .any(|m| m.role == Role::Assistant && m.content == "continued work"));
}

// ----- 10. restart → unconfirmed consequential step → human review -----

#[tokio::test]
async fn restart_review_flag() {
    tokio::time::pause();
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(ConsequentialProbe));
    let s = setup_with(
        Arc::new(MockLlm::new(vec![MockTurn::text("never runs")])),
        2,
        Some(Arc::new(registry)),
    )
    .await;
    let job = make_job(
        &s.db,
        &s.ws,
        "risky",
        OverlapPolicy::Skip,
        MissedPolicy::RunOnceIfMissed,
        JobBudgets::default(),
    )
    .await;

    s.db.upsert_run(&prior_run("prior-run-2", &job.conversation_id, Vec::new()))
        .await
        .unwrap();
    let occ =
        s.db.insert_occurrence(job.id.clone(), utc(&(t0() + Duration::minutes(2))))
            .await
            .unwrap()
            .unwrap();
    s.db.update_occurrence(
        occ.id.clone(),
        OccurrenceStatus::Interrupted,
        Some("prior-run-2".into()),
        None,
    )
    .await
    .unwrap();
    // Unconfirmed consequential step: the engine wrote "running" before
    // execution and the process died before the completion rewrite.
    s.db.add_tool_call(&ToolCallRecord {
        id: "tc-1".into(),
        run_id: "prior-run-2".into(),
        tool: "test_consequential".into(),
        args: serde_json::json!({}),
        ok: false,
        summary: "running".into(),
        created_at: vortex_core::db::now(),
    })
    .await
    .unwrap();

    s.sup.catch_up().await;
    settle().await;

    let got = s.db.get_occurrence(occ.id.clone()).await.unwrap().unwrap();
    assert!(got.needs_human_review);
    assert_eq!(got.status, OccurrenceStatus::Interrupted);
    assert_eq!(got.reason.as_deref(), Some("review_flagged"));
    assert_eq!(got.run_id.as_deref(), Some("prior-run-2"));
    assert_eq!(
        s.db.list_runs_for_conversation(job.conversation_id.clone())
            .await
            .unwrap()
            .len(),
        1,
        "NO continuation run may be spawned for an unconfirmed consequential step"
    );
    let after = s.db.get_job(job.id.clone()).await.unwrap().unwrap();
    assert_eq!(
        after.consecutive_failures, 0,
        "review flags never count toward the breaker"
    );
    assert_eq!(after.status, JobStatus::Active);
}

// ----- 11. parked-expired approvals on restart → breaker -----

#[tokio::test]
async fn parked_expired_breaker() {
    tokio::time::pause();
    let s = setup(Arc::new(MockLlm::new(Vec::new())), 2).await;
    let job = make_job(
        &s.db,
        &s.ws,
        "parked",
        OverlapPolicy::Skip,
        MissedPolicy::RunOnceIfMissed,
        JobBudgets::default(),
    )
    .await;

    // One parked-expired occurrence: failure counted, job still active.
    for i in 1..=1 {
        add_parked_expired(&s.db, &job, i).await;
    }
    s.sup.catch_up().await;
    settle().await;
    let after = s.db.get_job(job.id.clone()).await.unwrap().unwrap();
    assert_eq!(after.consecutive_failures, 1);
    assert_eq!(after.status, JobStatus::Active);

    // Two more: the third consecutive parked-expired trips the breaker.
    for i in 2..=3 {
        add_parked_expired(&s.db, &job, i).await;
    }
    s.sup.catch_up().await;
    settle().await;

    let after = s.db.get_job(job.id.clone()).await.unwrap().unwrap();
    assert_eq!(after.consecutive_failures, 3);
    assert_eq!(after.status, JobStatus::CircuitBroken);
    for occ in occurrences(&s.db, &job).await {
        assert_eq!(occ.status, OccurrenceStatus::Interrupted);
        assert_eq!(
            occ.reason.as_deref(),
            Some("parked approval expired on restart")
        );
    }
    let (events, _, _) = s.sup.replay_from(0);
    assert!(events.iter().any(|(_, e)| matches!(
        e,
        vortex_types::JobEvent::JobCircuitBroken { job_id, .. } if job_id == &job.id
    )));
    assert_eq!(
        s.db.list_runs_for_conversation(job.conversation_id.clone())
            .await
            .unwrap()
            .len(),
        3,
        "parked-expired occurrences are never auto-continued"
    );
}

async fn add_parked_expired(db: &Db, job: &Job, i: i64) {
    let run_id = format!("prior-parked-{i}");
    db.upsert_run(&prior_run(&run_id, &job.conversation_id, Vec::new()))
        .await
        .unwrap();
    let occ = db
        .insert_occurrence(job.id.clone(), utc(&(t0() + Duration::minutes(5 * i))))
        .await
        .unwrap()
        .unwrap();
    db.update_occurrence(
        occ.id.clone(),
        OccurrenceStatus::Interrupted,
        Some(run_id.clone()),
        None,
    )
    .await
    .unwrap();
    db.add_approval(
        format!("a-parked-{i}"),
        run_id,
        "agent".into(),
        "Delete file?".into(),
        "{}".into(),
    )
    .await
    .unwrap();
    db.resolve_approval(format!("a-parked-{i}"), ApprovalStatus::Expired)
        .await
        .unwrap();
}

// ----- 12. steer note applied once -----

#[tokio::test]
async fn steer_note_consumed() {
    tokio::time::pause();
    let rec = Arc::new(RecordingLlm::new(MockLlm::new(vec![
        MockTurn::text("done"),
        MockTurn::text("done again"),
    ])));
    let s = setup(rec.clone(), 2).await;
    let job = make_job(
        &s.db,
        &s.ws,
        "steered",
        OverlapPolicy::Skip,
        MissedPolicy::RunOnceIfMissed,
        JobBudgets::default(),
    )
    .await;
    s.db.set_job_steer_note(job.id.clone(), Some("focus on ferrets".into()))
        .await
        .unwrap();

    s.sup.tick_once().await;
    s.clock.set(t0() + Duration::minutes(2));
    s.sup.tick_once().await;
    settle().await;

    assert_eq!(
        rec.requests_containing(
            "STEERING NOTE FROM THE USER (apply to this run): focus on ferrets"
        ),
        1,
        "the fired run's instructions must carry the steer note"
    );
    assert!(
        s.db.get_job(job.id.clone())
            .await
            .unwrap()
            .unwrap()
            .steer_note
            .is_none(),
        "the note is consumed once"
    );

    // The next occurrence runs without the note.
    s.clock.set(t0() + Duration::minutes(7));
    s.sup.tick_once().await;
    settle().await;
    assert_eq!(
        rec.requests_containing("focus on ferrets"),
        1,
        "the consumed note must not leak into later runs"
    );
    let occs = occurrences(&s.db, &job).await;
    assert_eq!(occs.len(), 2);
    assert_eq!(occs[1].status, OccurrenceStatus::Succeeded);
}

// ----- 13. double-fire guard -----

#[tokio::test]
async fn double_fire_guard() {
    tokio::time::pause();
    let s = setup(Arc::new(MockLlm::new(vec![MockTurn::text("once")])), 2).await;
    let job = make_job(
        &s.db,
        &s.ws,
        "guarded",
        OverlapPolicy::Skip,
        MissedPolicy::RunOnceIfMissed,
        JobBudgets::default(),
    )
    .await;

    // The tick already has an occurrence (e.g. inserted before a restart)
    // and next_run_at still points at it.
    let tick = next_tick_utc("*/5 * * * *", &t0());
    s.db.set_job_next_run(job.id.clone(), Some(tick.clone()))
        .await
        .unwrap();
    let pre =
        s.db.insert_occurrence(job.id.clone(), tick.clone())
            .await
            .unwrap()
            .unwrap();

    s.clock.set(parse_local(&tick));
    s.sup.tick_once().await;
    settle().await;

    let occs = occurrences(&s.db, &job).await;
    assert_eq!(occs.len(), 1, "no second occurrence for the same tick");
    assert_eq!(occs[0].id, pre.id);
    assert_eq!(occs[0].status, OccurrenceStatus::Succeeded);
    assert_eq!(
        s.db.list_runs_for_conversation(job.conversation_id.clone())
            .await
            .unwrap()
            .len(),
        1,
        "the pre-inserted occurrence runs exactly once"
    );
}

// ----- 14. job gate bounds concurrent job roots -----

#[tokio::test]
async fn job_gate_limit() {
    tokio::time::pause();
    let s = setup(
        Arc::new(MockLlm::with_delay(
            vec![MockTurn::text("alpha reply"), MockTurn::text("beta reply")],
            250,
        )),
        1,
    )
    .await;
    let a = make_job(
        &s.db,
        &s.ws,
        "alpha",
        OverlapPolicy::Skip,
        MissedPolicy::RunOnceIfMissed,
        JobBudgets::default(),
    )
    .await;
    let b = make_job(
        &s.db,
        &s.ws,
        "beta",
        OverlapPolicy::Skip,
        MissedPolicy::RunOnceIfMissed,
        JobBudgets::default(),
    )
    .await;

    s.sup.tick_once().await; // seed both next_run_at = 16:05
    s.clock.set(t0() + Duration::minutes(2));
    s.sup.tick_once().await; // both due simultaneously
    assert_eq!(occurrences(&s.db, &a).await.len(), 1);
    assert_eq!(occurrences(&s.db, &b).await.len(), 1);

    // Gate 1: exactly one job root runs while the other waits for a slot.
    let mut observed = None;
    for _ in 0..600 {
        let sa = first_occ(&s.db, &a).await.status;
        let sb = first_occ(&s.db, &b).await.status;
        if (sa == OccurrenceStatus::Running && sb == OccurrenceStatus::Scheduled)
            || (sa == OccurrenceStatus::Scheduled && sb == OccurrenceStatus::Running)
        {
            observed = Some((sa, sb));
            break;
        }
        assert!(
            !(sa == OccurrenceStatus::Running && sb == OccurrenceStatus::Running),
            "gate 1 must never run two job roots concurrently"
        );
        advance(5).await;
    }
    assert!(
        observed.is_some(),
        "expected one running + one queued occurrence"
    );

    // First run finishes; the waiter takes the slot and runs in turn.
    let mut handoff = false;
    for _ in 0..600 {
        let sa = first_occ(&s.db, &a).await.status;
        let sb = first_occ(&s.db, &b).await.status;
        if (sa == OccurrenceStatus::Succeeded && sb == OccurrenceStatus::Running)
            || (sa == OccurrenceStatus::Running && sb == OccurrenceStatus::Succeeded)
        {
            handoff = true;
            break;
        }
        advance(5).await;
    }
    assert!(handoff, "the waiting job must run after the first finishes");

    let occ_a = first_occ(&s.db, &a).await;
    let occ_b = first_occ(&s.db, &b).await;
    wait_occ(&s.db, &occ_a.id, OccurrenceStatus::Succeeded).await;
    wait_occ(&s.db, &occ_b.id, OccurrenceStatus::Succeeded).await;
}

// ----- 15/16. crash-window recovery (lead-reported live bug) -----

/// A process kill in the old admission window left the occurrence Running
/// with run_id NULL; the startup sweep interrupts it. catch_up must reset it
/// to Scheduled so the due query re-admits it as a FRESH gated run.
#[tokio::test]
async fn crash_window_running_without_run_id_readmits() {
    tokio::time::pause();
    let s = setup(Arc::new(MockLlm::new(vec![MockTurn::text("recovered")])), 2).await;
    let job = make_job(
        &s.db,
        &s.ws,
        "crash-window",
        OverlapPolicy::Skip,
        MissedPolicy::RunOnceIfMissed,
        JobBudgets::default(),
    )
    .await;

    // Crash-window state: Running without a run id, and NO run row exists.
    let tick = next_tick_utc("*/5 * * * *", &t0());
    let occ =
        s.db.insert_occurrence(job.id.clone(), tick.clone())
            .await
            .unwrap()
            .unwrap();
    s.db.update_occurrence(occ.id.clone(), OccurrenceStatus::Running, None, None)
        .await
        .unwrap();
    assert!(s
        .db
        .list_runs_for_conversation(job.conversation_id.clone())
        .await
        .unwrap()
        .is_empty());

    // Startup sweep (runs sweep + occurrence sweep, as main.rs does).
    assert_eq!(
        s.db.mark_occurrences_interrupted_on_startup()
            .await
            .unwrap(),
        1
    );
    s.clock.set(parse_local(&tick));
    s.sup.catch_up().await;

    // Recovery: back to Scheduled, reason cleared, still no run id.
    let got = s.db.get_occurrence(occ.id.clone()).await.unwrap().unwrap();
    assert_eq!(got.status, OccurrenceStatus::Scheduled);
    assert!(got.reason.is_none());
    assert!(got.run_id.is_none());

    // The next tick re-admits it as a fresh gated run and it succeeds.
    s.sup.tick_once().await;
    wait_occ(&s.db, &occ.id, OccurrenceStatus::Succeeded).await;
    let got = s.db.get_occurrence(occ.id.clone()).await.unwrap().unwrap();
    assert!(got.run_id.is_some());
    let runs =
        s.db.list_runs_for_conversation(job.conversation_id.clone())
            .await
            .unwrap();
    assert_eq!(
        runs.len(),
        1,
        "exactly one run for the recovered occurrence"
    );
    assert_eq!(runs[0].id, got.run_id.unwrap());
    assert_eq!(runs[0].status, RunStatus::Completed);
    assert_eq!(occurrences(&s.db, &job).await.len(), 1);
    assert!(s
        .db
        .list_live_occurrences(job.id.clone())
        .await
        .unwrap()
        .is_empty());
}

/// Legacy dead-end rows already stamped "interrupted before run admission"
/// by the old catch_up must also recover (the live-smoke evidence).
#[tokio::test]
async fn legacy_reason_interrupted_before_admission_readmits() {
    tokio::time::pause();
    let s = setup(
        Arc::new(MockLlm::new(vec![MockTurn::text("recovered legacy")])),
        2,
    )
    .await;
    let job = make_job(
        &s.db,
        &s.ws,
        "legacy-dead-end",
        OverlapPolicy::Skip,
        MissedPolicy::RunOnceIfMissed,
        JobBudgets::default(),
    )
    .await;

    let tick = next_tick_utc("*/5 * * * *", &t0());
    let occ =
        s.db.insert_occurrence(job.id.clone(), tick.clone())
            .await
            .unwrap()
            .unwrap();
    s.db.update_occurrence(occ.id.clone(), OccurrenceStatus::Running, None, None)
        .await
        .unwrap();
    s.db.mark_occurrences_interrupted_on_startup()
        .await
        .unwrap();
    // The old catch_up stamped the dead-end reason on this row.
    s.db.update_occurrence(
        occ.id.clone(),
        OccurrenceStatus::Interrupted,
        None,
        Some("interrupted before run admission".into()),
    )
    .await
    .unwrap();

    s.clock.set(parse_local(&tick));
    s.sup.catch_up().await;
    let got = s.db.get_occurrence(occ.id.clone()).await.unwrap().unwrap();
    assert_eq!(got.status, OccurrenceStatus::Scheduled);
    assert!(got.reason.is_none(), "the dead-end reason must be cleared");

    s.sup.tick_once().await;
    wait_occ(&s.db, &occ.id, OccurrenceStatus::Succeeded).await;
    assert_eq!(
        s.db.list_runs_for_conversation(job.conversation_id.clone())
            .await
            .unwrap()
            .len(),
        1
    );

    // Idempotent: a later catch_up never disturbs the terminal row.
    s.sup.catch_up().await;
    settle().await;
    let occs = occurrences(&s.db, &job).await;
    assert_eq!(occs.len(), 1);
    assert_eq!(occs[0].status, OccurrenceStatus::Succeeded);
}

// ----- 17. admission ordering invariant -----

/// The occurrence must NEVER be observable Running without a run id: it
/// stays Scheduled for the whole engine-admission window (held open here by
/// a slow catalog fetch), then transitions to Running WITH the run id in a
/// single write.
#[tokio::test]
async fn admission_ordering() {
    tokio::time::pause();
    let s = setup(
        Arc::new(SlowCatalogLlm {
            inner: MockLlm::new(vec![MockTurn::text("ordered")]),
        }),
        2,
    )
    .await;
    let job = make_job(
        &s.db,
        &s.ws,
        "ordering",
        OverlapPolicy::Skip,
        MissedPolicy::RunOnceIfMissed,
        JobBudgets::default(),
    )
    .await;

    s.sup.tick_once().await; // seed next_run_at = 16:05
    s.clock.set(t0() + Duration::minutes(2));
    s.sup.tick_once().await;
    let occs = occurrences(&s.db, &job).await;
    assert_eq!(occs.len(), 1);
    let occ_id = occs[0].id.clone();

    // Poll across the admission window (the catalog fetch sleeps 500ms of
    // virtual time inside start_job_run, before any run id exists).
    let mut saw_scheduled = false;
    let mut saw_running_with_id = false;
    for _ in 0..600 {
        let got = s.db.get_occurrence(occ_id.clone()).await.unwrap().unwrap();
        assert!(
            !(got.status == OccurrenceStatus::Running && got.run_id.is_none()),
            "Running must never be observable without a run id"
        );
        match got.status {
            OccurrenceStatus::Scheduled => saw_scheduled = true,
            OccurrenceStatus::Running => {
                assert!(got.run_id.is_some());
                saw_running_with_id = true;
                break;
            }
            OccurrenceStatus::Succeeded => {
                saw_running_with_id = true;
                break;
            }
            other => panic!("unexpected status during admission: {other:?}"),
        }
        advance(10).await;
    }
    assert!(
        saw_scheduled,
        "the pre-admission Scheduled window must be observable"
    );
    assert!(saw_running_with_id, "admission must complete");
    wait_occ(&s.db, &occ_id, OccurrenceStatus::Succeeded).await;
}
