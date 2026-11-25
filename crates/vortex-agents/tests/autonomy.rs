//! Autonomy tests (SA3): per-request approval policies (park vs interactive
//! timeout), step-boundary continuation classification, and job-run admission
//! (`start_job_run` / `spawn_continuation`). Fully offline: tempdir SQLite,
//! scripted mock LLM, and virtual time (`start_paused`) for every time-based
//! assertion — no real sleeps, no network, no keys.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use vortex_agents::approvals::ApprovalManager;
use vortex_agents::engine::{
    allowed_for_mode, classify_last_step, ContinuationSafety, ContinuationSeed, Engine, JobRunSpec,
    RunError,
};
use vortex_agents::errors::{Risk, ToolError};
use vortex_agents::registry::{ApprovalGate, Tool, ToolContext, ToolRegistry, ToolResult};
use vortex_agents::schema::ToolDef;
use vortex_agents::{Hub, PreviewRegistry, Scheduler};
use vortex_core::Db;
use vortex_llm::llm::{CompletionRequest, CompletionResponse, LlmClient, LlmEvent};
use vortex_llm::{MockLlm, MockTurn};
use vortex_types::{
    ApprovalPolicy, ApprovalStatus, ChatMessage, JobBudgets, Mode, NewJob, NewRoutine, PlanStep,
    Role, RunInfo, RunOrigin, RunStatus, Settings, StepStatus, ToolCallRecord,
};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

struct ParkFixture {
    mgr: Arc<ApprovalManager>,
    hub: Arc<Hub>,
    db: Db,
    _dir: tempfile::TempDir,
}

fn park_fixture(default_timeout_secs: u64) -> ParkFixture {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("approvals.db")).unwrap();
    let hub = Arc::new(Hub::new());
    let mgr = Arc::new(ApprovalManager::new(
        db.clone(),
        hub.clone(),
        default_timeout_secs,
    ));
    ParkFixture {
        mgr,
        hub,
        db,
        _dir: dir,
    }
}

async fn register_run(f: &ParkFixture, run_id: &str) {
    f.hub.register(
        run_id,
        None,
        None,
        "coordinator",
        "job task",
        "mock/model",
        0,
        CancellationToken::new(),
    );
    // Mirror engine admission: the hub handle starts Queued and flips to
    // Running before any tool (and thus any approval wait) executes.
    f.hub.set_status(run_id, RunStatus::Running);
    f.db.upsert_run(&RunInfo {
        id: run_id.into(),
        parent_run_id: None,
        conversation_id: None,
        agent: "coordinator".into(),
        task: "job task".into(),
        model: "mock/model".into(),
        status: RunStatus::Running,
        error: None,
        usage: None,
        plan: Vec::new(),
        depth: 0,
        created_at: vortex_core::db::now(),
        updated_at: vortex_core::db::now(),
    })
    .await
    .unwrap();
}

/// Drive virtual time until the approval row is visible (persisted). NOTE:
/// the row commits in `add_approval` BEFORE the request task registers the
/// oneshot sender in the pending map, so this alone is NOT a "ready to
/// decide" signal — pair it with `wait_parked` before calling `decide`.
async fn wait_pending(f: &ParkFixture) -> vortex_types::ApprovalInfo {
    for _ in 0..200 {
        if let Some(a) = f.mgr.pending_approvals().await.into_iter().next() {
            return a;
        }
        tokio::time::advance(Duration::from_millis(10)).await;
    }
    panic!("approval never became pending");
}

/// Drive virtual time until the park branch has fully parked the run. The
/// hub status flips to WaitingApproval strictly AFTER the oneshot sender is
/// registered in the pending map (persist → publish → insert → set_status),
/// so observing it guarantees `decide` will find the sender. This is the
/// true "ready to decide" signal under virtual-time contention.
async fn wait_parked(f: &ParkFixture, run_id: &str) {
    for _ in 0..200 {
        if f.hub.get(run_id).map(|h| h.status()) == Some(RunStatus::WaitingApproval) {
            return;
        }
        tokio::time::advance(Duration::from_millis(10)).await;
    }
    panic!("run never reached WaitingApproval");
}

/// Drive virtual time until a spawned request resolves; returns its answer.
async fn finish_request(handle: tokio::task::JoinHandle<Result<bool, ToolError>>) -> bool {
    for _ in 0..1000 {
        if handle.is_finished() {
            return handle.await.unwrap().unwrap();
        }
        tokio::time::advance(Duration::from_millis(50)).await;
    }
    panic!("approval request never resolved");
}

async fn approval_row(f: &ParkFixture, id: &str) -> ApprovalStatus {
    f.db.list_approvals(false)
        .await
        .unwrap()
        .into_iter()
        .find(|a| a.id == id)
        .expect("approval row exists")
        .status
}

async fn run_status(f: &ParkFixture, run_id: &str) -> (RunStatus, RunStatus) {
    let hub = f.hub.get(run_id).expect("run registered").status();
    let db = f.db.get_run(run_id.into()).await.unwrap().unwrap().status;
    (hub, db)
}

struct Harness {
    engine: Arc<Engine>,
    db: Db,
    dir: tempfile::TempDir,
}

async fn harness(llm: Arc<dyn LlmClient>, mut settings: Settings) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    if settings.workspace.is_none() {
        settings.workspace = Some(dir.path().to_string_lossy().to_string());
    }
    let db = Db::open(dir.path().join("autonomy.db")).unwrap();
    let hub = Arc::new(Hub::new());
    let scheduler = Arc::new(Scheduler::new(settings.agents.max_concurrent));
    let browser = Arc::new(vortex_agents::browser::BrowserManager::new(
        dir.path().join("profile"),
        true,
    ));
    let previews = Arc::new(PreviewRegistry::new());
    let http = vortex_agents::safe_http::SafeHttp::new(false);
    let engine_slot = Arc::new(std::sync::OnceLock::new());
    let registry = vortex_agents::build_registry(previews.clone(), engine_slot.clone());
    let approvals = Arc::new(ApprovalManager::new(
        db.clone(),
        hub.clone(),
        settings.agents.approval_timeout_secs,
    ));
    let engine = Engine::new(
        llm,
        registry,
        hub,
        scheduler,
        approvals,
        db.clone(),
        browser,
        previews,
        None,
        http,
        settings,
        None,
    );
    let _ = engine_slot.set(engine.clone());
    Harness { engine, db, dir }
}

async fn job_fixture(
    db: &Db,
    mode: Mode,
    workspace: Option<String>,
) -> (vortex_types::Routine, vortex_types::Job) {
    let routine = db
        .create_routine(NewRoutine {
            name: "digest".into(),
            description: "test routine".into(),
            instructions: "do the job".into(),
            mode,
            model: None,
        })
        .await
        .unwrap();
    let job = db
        .create_job(NewJob {
            name: "nightly".into(),
            routine_id: routine.id.clone(),
            routine_version: 1,
            schedule: None,
            overlap: Default::default(),
            budgets: JobBudgets::default(),
            workspace,
        })
        .await
        .unwrap();
    (routine, job)
}

fn prior_run_row(run_id: &str, model: &str, conversation_id: &str) -> RunInfo {
    RunInfo {
        id: run_id.into(),
        parent_run_id: None,
        conversation_id: Some(conversation_id.into()),
        agent: "coordinator".into(),
        task: "nightly tick".into(),
        model: model.into(),
        status: RunStatus::Interrupted,
        error: None,
        usage: None,
        plan: Vec::new(),
        depth: 0,
        created_at: vortex_core::db::now(),
        updated_at: vortex_core::db::now(),
    }
}

/// Mock LLM that also records the tool schemas and messages each completion
/// request actually carried (what the model sees is what admission allowed).
struct RecordingLlm {
    inner: MockLlm,
    seen_tools: tokio::sync::Mutex<Vec<Vec<String>>>,
    seen_messages: tokio::sync::Mutex<Vec<Vec<ChatMessage>>>,
}

impl RecordingLlm {
    fn new(turns: Vec<MockTurn>) -> Arc<Self> {
        Arc::new(Self {
            inner: MockLlm::new(turns),
            seen_tools: tokio::sync::Mutex::new(Vec::new()),
            seen_messages: tokio::sync::Mutex::new(Vec::new()),
        })
    }
}

#[async_trait]
impl LlmClient for RecordingLlm {
    async fn complete(
        &self,
        req: &CompletionRequest,
        events: mpsc::Sender<LlmEvent>,
    ) -> Result<CompletionResponse, vortex_llm::LlmError> {
        self.seen_tools
            .lock()
            .await
            .push(req.tools.iter().map(|t| t.name.clone()).collect());
        self.seen_messages.lock().await.push(req.messages.clone());
        self.inner.complete(req, events).await
    }

    async fn models(&self) -> Result<Vec<vortex_types::ModelInfo>, vortex_llm::LlmError> {
        self.inner.models().await
    }

    fn describe(&self) -> &'static str {
        "recording"
    }
}

const AUTHORING_TOOLS: [&str; 3] = ["save_routine", "list_routines", "propose_schedule"];

// ---------------------------------------------------------------------------
// Approval policies
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn park_until_decided_waits_indefinitely() {
    let f = park_fixture(30);
    register_run(&f, "run-park").await;
    let mgr = f.mgr.clone();
    let handle = tokio::spawn(async move {
        mgr.request(
            "run-park",
            "Publish post",
            "{}",
            ApprovalPolicy::ParkUntilDecided,
        )
        .await
    });
    let approval = wait_pending(&f).await;

    // A full virtual day — far beyond any interactive timeout — still parked.
    tokio::time::advance(Duration::from_secs(86_400)).await;
    assert!(!handle.is_finished(), "parked approval must not time out");
    assert_eq!(f.mgr.pending_approvals().await.len(), 1);
    assert_eq!(
        run_status(&f, "run-park").await,
        (RunStatus::WaitingApproval, RunStatus::WaitingApproval),
        "parked status must be visible in BOTH hub and DB"
    );

    // The human decides: the wait resolves and the run goes back to Running.
    assert!(f.mgr.decide(&approval.id, true).await);
    assert!(finish_request(handle).await);
    assert_eq!(
        run_status(&f, "run-park").await,
        (RunStatus::Running, RunStatus::Running),
        "status must restore after the decision"
    );
    assert_eq!(
        approval_row(&f, &approval.id).await,
        ApprovalStatus::Approved
    );
    assert!(f.mgr.pending_approvals().await.is_empty());
}

#[tokio::test(start_paused = true)]
async fn park_deny_and_late_decide() {
    let f = park_fixture(30);
    register_run(&f, "run-deny").await;
    let mgr = f.mgr.clone();
    let handle = tokio::spawn(async move {
        mgr.request(
            "run-deny",
            "Delete file",
            "{}",
            ApprovalPolicy::ParkUntilDecided,
        )
        .await
    });
    let approval = wait_pending(&f).await;
    // The DB row commits before the sender is registered; only the hub
    // status (set after the insert) proves the waiter is ready to decide.
    wait_parked(&f, "run-deny").await;

    assert!(f.mgr.decide(&approval.id, false).await);
    assert!(
        !finish_request(handle).await,
        "denial must resolve as false"
    );
    assert_eq!(approval_row(&f, &approval.id).await, ApprovalStatus::Denied);
    assert_eq!(
        run_status(&f, "run-deny").await,
        (RunStatus::Running, RunStatus::Running),
        "a denied run continues (the tool reports the denial)"
    );

    // A second, late decide delivers nothing and never overwrites the row.
    assert!(!f.mgr.decide(&approval.id, true).await);
    assert_eq!(approval_row(&f, &approval.id).await, ApprovalStatus::Denied);
}

#[tokio::test(start_paused = true)]
async fn park_shutdown_expires() {
    let f = park_fixture(30);
    register_run(&f, "run-shutdown").await;
    let mgr = f.mgr.clone();
    let handle = tokio::spawn(async move {
        mgr.request(
            "run-shutdown",
            "Send email",
            "{}",
            ApprovalPolicy::ParkUntilDecided,
        )
        .await
    });
    let approval = wait_pending(&f).await;

    // Graceful shutdown cancels the run's token: the parked wait resolves
    // promptly as expired, never as a decision nobody made.
    f.hub.cancel("run-shutdown");
    assert!(!finish_request(handle).await);
    assert_eq!(
        approval_row(&f, &approval.id).await,
        ApprovalStatus::Expired
    );
    assert!(f.mgr.pending_approvals().await.is_empty());

    // Single-owner invariant: a late decide cannot overwrite `expired`.
    assert!(!f.mgr.decide(&approval.id, true).await);
    assert_eq!(
        approval_row(&f, &approval.id).await,
        ApprovalStatus::Expired
    );
}

#[tokio::test(start_paused = true)]
async fn interactive_timeout_unchanged() {
    let f = park_fixture(30);
    register_run(&f, "run-interactive").await;

    // Explicit TimeoutSecs(7): auto-denies at 7 virtual seconds.
    let mgr = f.mgr.clone();
    let handle = tokio::spawn(async move {
        mgr.request("run-interactive", "t", "{}", ApprovalPolicy::TimeoutSecs(7))
            .await
    });
    let approval = wait_pending(&f).await;
    tokio::time::advance(Duration::from_secs(6)).await;
    assert!(!handle.is_finished(), "must still wait before the timeout");
    assert_eq!(f.mgr.pending_approvals().await.len(), 1);
    assert_eq!(
        run_status(&f, "run-interactive").await,
        (RunStatus::Running, RunStatus::Running),
        "interactive waits never change run status"
    );
    tokio::time::advance(Duration::from_secs(2)).await;
    assert!(!finish_request(handle).await);
    assert_eq!(
        approval_row(&f, &approval.id).await,
        ApprovalStatus::Expired
    );
    assert_eq!(
        run_status(&f, "run-interactive").await,
        (RunStatus::Running, RunStatus::Running),
        "the timeout branch never changes run status either"
    );

    // TimeoutSecs(0) falls back to the manager's configured default (30s).
    let mgr = f.mgr.clone();
    let handle = tokio::spawn(async move {
        mgr.request(
            "run-interactive",
            "t2",
            "{}",
            ApprovalPolicy::TimeoutSecs(0),
        )
        .await
    });
    let approval = wait_pending(&f).await;
    tokio::time::advance(Duration::from_secs(29)).await;
    assert!(
        !handle.is_finished(),
        "0 must use the manager default, not 5s"
    );
    tokio::time::advance(Duration::from_secs(2)).await;
    assert!(!finish_request(handle).await);
    assert_eq!(
        approval_row(&f, &approval.id).await,
        ApprovalStatus::Expired
    );

    // Explicit values keep the historical 5-second floor.
    let mgr = f.mgr.clone();
    let handle = tokio::spawn(async move {
        mgr.request(
            "run-interactive",
            "t3",
            "{}",
            ApprovalPolicy::TimeoutSecs(1),
        )
        .await
    });
    let approval = wait_pending(&f).await;
    tokio::time::advance(Duration::from_secs(4)).await;
    assert!(!handle.is_finished(), "1s must be floored to 5s");
    tokio::time::advance(Duration::from_secs(2)).await;
    assert!(!finish_request(handle).await);
    assert_eq!(
        approval_row(&f, &approval.id).await,
        ApprovalStatus::Expired
    );
}

// ---------------------------------------------------------------------------
// classify_last_step
// ---------------------------------------------------------------------------

struct StubTool {
    def: &'static ToolDef,
    risk: Risk,
}

const DANGER_DEF: ToolDef = ToolDef {
    name: "danger_tool",
    description: "consequential stub",
    params: &[],
};
const SAFE_DEF: ToolDef = ToolDef {
    name: "safe_tool",
    description: "low-risk stub",
    params: &[],
};

#[async_trait]
impl Tool for StubTool {
    fn def(&self) -> &'static ToolDef {
        self.def
    }
    fn risk(&self) -> Risk {
        self.risk
    }
    async fn execute(
        &self,
        _ctx: &ToolContext,
        _args: &serde_json::Value,
    ) -> Result<ToolResult, ToolError> {
        Ok(ToolResult::text("stub ok"))
    }
}

fn stub_registry() -> ToolRegistry {
    let mut r = ToolRegistry::new();
    r.register(Arc::new(StubTool {
        def: &DANGER_DEF,
        risk: Risk::Consequential,
    }));
    r.register(Arc::new(StubTool {
        def: &SAFE_DEF,
        risk: Risk::Low,
    }));
    r
}

fn call(id: &str, tool: &str, summary: &str) -> ToolCallRecord {
    ToolCallRecord {
        id: id.into(),
        run_id: "r".into(),
        tool: tool.into(),
        args: serde_json::Value::Null,
        ok: summary != "running",
        summary: summary.into(),
        created_at: vortex_core::db::now(),
    }
}

fn review(tool: &str, call_id: &str) -> ContinuationSafety {
    ContinuationSafety::NeedsReview {
        tool: tool.into(),
        call_id: call_id.into(),
    }
}

#[test]
fn classify_last_step_truth_table() {
    let reg = stub_registry();
    // Empty history: nothing unconfirmed.
    assert_eq!(classify_last_step(&reg, &[]), ContinuationSafety::Safe);
    // Confirmed consequential rows never flag (any summary but "running").
    assert_eq!(
        classify_last_step(&reg, &[call("c1", "danger_tool", "ok")]),
        ContinuationSafety::Safe
    );
    assert_eq!(
        classify_last_step(
            &reg,
            &[call("c1", "danger_tool", "the user declined this action")]
        ),
        ContinuationSafety::Safe
    );
    // Unconfirmed consequential: human review required.
    assert_eq!(
        classify_last_step(&reg, &[call("c1", "danger_tool", "running")]),
        review("danger_tool", "c1")
    );
    // Unconfirmed low-risk: safe to continue.
    assert_eq!(
        classify_last_step(&reg, &[call("c1", "safe_tool", "running")]),
        ContinuationSafety::Safe
    );
    // Unconfirmed unknown tool: risk unverifiable → review.
    assert_eq!(
        classify_last_step(&reg, &[call("c1", "ghost_tool", "running")]),
        review("ghost_tool", "c1")
    );
    // Mixed rows: the first unconfirmed consequential row wins.
    let mixed = vec![
        call("c1", "danger_tool", "ok"),
        call("c2", "safe_tool", "running"),
        call("c3", "danger_tool", "running"),
        call("c4", "ghost_tool", "running"),
    ];
    assert_eq!(
        classify_last_step(&reg, &mixed),
        review("danger_tool", "c3")
    );
    // Oldest-first: an earlier unconfirmed unknown tool is flagged first.
    let mixed2 = vec![
        call("c1", "ghost_tool", "running"),
        call("c2", "danger_tool", "running"),
    ];
    assert_eq!(
        classify_last_step(&reg, &mixed2),
        review("ghost_tool", "c1")
    );
}

/// The classifier consumes `Db::list_tool_calls` order (oldest first) — the
/// exact persisted shape an interrupted run leaves behind.
#[tokio::test]
async fn classify_last_step_over_persisted_calls() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("calls.db")).unwrap();
    let mut older = call("c1", "danger_tool", "running");
    older.created_at = "2026-01-01T00:00:00.000Z".into();
    let mut newer = call("c2", "safe_tool", "ok");
    newer.created_at = "2026-01-02T00:00:00.000Z".into();
    // Insert newest first: the store must still return oldest first.
    db.add_tool_call(&newer).await.unwrap();
    db.add_tool_call(&older).await.unwrap();
    let calls = db.list_tool_calls("r".into()).await.unwrap();
    assert_eq!(
        calls.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
        vec!["c1", "c2"]
    );
    assert_eq!(
        classify_last_step(&stub_registry(), &calls),
        review("danger_tool", "c1")
    );
}

// ---------------------------------------------------------------------------
// start_job_run
// ---------------------------------------------------------------------------

#[tokio::test]
async fn start_job_run_end_to_end() {
    let llm = RecordingLlm::new(vec![MockTurn::text("job done")]);
    let h = harness(llm.clone(), Settings::default()).await;
    let ws = h.dir.path().join("job-ws");
    std::fs::create_dir_all(&ws).unwrap();
    let ws_str = ws.to_string_lossy().to_string();
    let (_r, job) = job_fixture(&h.db, Mode::Agent, Some(ws_str.clone())).await;

    let spec = JobRunSpec {
        job_id: job.id.clone(),
        occurrence_id: "occ-1".into(),
        conversation_id: job.conversation_id.clone(),
        task: "nightly tick".into(),
        instructions: "summarize the news".into(),
        mode: Mode::Agent,
        model: Some("mock/model".into()),
        workspace: Some(ws_str),
        budgets: JobBudgets::default(),
    };
    let (run_id, handle) = h.engine.start_job_run(spec).await.unwrap();
    assert_eq!(handle.await.unwrap().unwrap(), "job done");

    let run = h.db.get_run(run_id.clone()).await.unwrap().unwrap();
    assert_eq!(run.status, RunStatus::Completed);
    assert_eq!(run.model, "mock/model");
    assert_eq!(run.agent, "coordinator");
    assert_eq!(run.depth, 0);
    assert!(run.parent_run_id.is_none());
    assert_eq!(
        run.conversation_id.as_deref(),
        Some(job.conversation_id.as_str()),
        "the run belongs to the job's dedicated conversation"
    );

    // The assistant reply persists in the job conversation.
    let msgs =
        h.db.get_messages(job.conversation_id.clone())
            .await
            .unwrap();
    let assistant = msgs
        .iter()
        .find(|m| m.role == Role::Assistant)
        .expect("assistant reply persisted");
    assert_eq!(assistant.content, "job done");
    assert_eq!(assistant.run_id.as_deref(), Some(run_id.as_str()));

    // Authoring tools are stripped from job origins at admission...
    let settings = h.engine.current_settings().await;
    let job_origin = RunOrigin::Job {
        job_id: job.id.clone(),
        occurrence_id: "occ-1".into(),
    };
    let job_allowed = allowed_for_mode(Mode::Agent, &settings, true, &job_origin);
    for t in AUTHORING_TOOLS {
        assert!(!job_allowed.contains(t), "job origin must not expose {t}");
    }
    // ...while interactive Agent mode keeps them.
    let interactive_allowed =
        allowed_for_mode(Mode::Agent, &settings, true, &RunOrigin::Interactive);
    for t in AUTHORING_TOOLS {
        assert!(
            interactive_allowed.contains(t),
            "interactive Agent mode exposes {t}"
        );
    }
    // ...and the provider never even saw their schemas.
    let seen = llm.seen_tools.lock().await;
    assert!(!seen.is_empty());
    for tools in seen.iter() {
        for t in AUTHORING_TOOLS {
            assert!(!tools.iter().any(|n| n == t), "provider request leaked {t}");
        }
    }
    // The hub handle is cleaned up after completion.
    assert!(h.engine.hub.get(&run_id).is_none());
}

#[tokio::test]
async fn start_job_run_resolves_auto_model() {
    // None and the Vortex Auto preference both resolve once at admission via
    // the offline mock catalog; a pinned id would pass through verbatim.
    for model in [None, Some(vortex_types::VORTEX_AUTO_MODEL.to_string())] {
        let llm = RecordingLlm::new(vec![MockTurn::text("auto done")]);
        let h = harness(llm.clone(), Settings::default()).await;
        let (_r, job) = job_fixture(&h.db, Mode::Agent, None).await;
        let spec = JobRunSpec {
            job_id: job.id.clone(),
            occurrence_id: "occ-auto".into(),
            conversation_id: job.conversation_id.clone(),
            task: "auto tick".into(),
            instructions: "do the job".into(),
            mode: Mode::Agent,
            model,
            workspace: None,
            budgets: JobBudgets::default(),
        };
        let (run_id, handle) = h.engine.start_job_run(spec).await.unwrap();
        assert_eq!(handle.await.unwrap().unwrap(), "auto done");
        let run = h.db.get_run(run_id).await.unwrap().unwrap();
        assert_eq!(run.status, RunStatus::Completed);
        assert_eq!(
            run.model, "mock/model",
            "Auto must resolve to the offline catalog's concrete model"
        );
    }
}

#[tokio::test]
async fn start_job_run_budget_turn_cap() {
    // A mock that keeps emitting a harmless registered tool call
    // (update_plan, Risk::Low): with max_turns=1 the run must fail at the
    // iteration limit after exactly one provider call.
    let plan_turn = MockTurn::tool_call(
        "update_plan",
        "c1",
        serde_json::json!({ "steps": ["one step"] }).to_string(),
    );
    let llm = Arc::new(MockLlm::new(vec![
        plan_turn.clone(),
        plan_turn.clone(),
        plan_turn,
    ]));
    let h = harness(llm.clone(), Settings::default()).await;
    let (_r, job) = job_fixture(&h.db, Mode::Agent, None).await;
    let spec = JobRunSpec {
        job_id: job.id.clone(),
        occurrence_id: "occ-budget".into(),
        conversation_id: job.conversation_id.clone(),
        task: "budget tick".into(),
        instructions: "loop forever".into(),
        mode: Mode::Agent,
        model: Some("mock/model".into()),
        workspace: None,
        budgets: JobBudgets {
            max_turns: 1,
            ..Default::default()
        },
    };
    let (run_id, handle) = h.engine.start_job_run(spec).await.unwrap();
    let err = handle.await.unwrap().unwrap_err();
    let msg = match err {
        RunError::Failed(msg) => msg,
        other => panic!("expected iteration-limit failure, got {other:?}"),
    };
    assert!(
        msg.contains("iteration limit (1)"),
        "budget must surface as the iteration limit: {msg}"
    );
    assert_eq!(llm.calls(), 1, "no provider call beyond the turn cap");
    let run = h.db.get_run(run_id).await.unwrap().unwrap();
    assert_eq!(run.status, RunStatus::Failed);
    assert_eq!(run.error.as_deref(), Some(msg.as_str()));
}

// ---------------------------------------------------------------------------
// spawn_continuation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn spawn_continuation_seeds_transcript() {
    let llm = RecordingLlm::new(vec![MockTurn::text("continued and finished")]);
    let h = harness(llm.clone(), Settings::default()).await;
    let ws = h.dir.path().join("job-ws");
    std::fs::create_dir_all(&ws).unwrap();
    let (_r, job) = job_fixture(&h.db, Mode::Agent, Some(ws.to_string_lossy().to_string())).await;
    h.db.upsert_run(&prior_run_row(
        "prior-run",
        "prior/model",
        &job.conversation_id,
    ))
    .await
    .unwrap();

    let seed_messages = vec![
        ChatMessage::system("job system prompt"),
        ChatMessage::user("earlier work"),
        ChatMessage::assistant("partial"),
    ];
    let seed = ContinuationSeed {
        job_id: job.id.clone(),
        occurrence_id: "occ-2".into(),
        prior_run_id: "prior-run".into(),
        conversation_id: job.conversation_id.clone(),
        task: "continue nightly".into(),
        messages: seed_messages.clone(),
        plan: vec![PlanStep {
            title: "carry forward".into(),
            status: StepStatus::InProgress,
        }],
        review_flags: Vec::new(),
    };
    let (run_id, handle) = h.engine.spawn_continuation(seed).await.unwrap();
    assert_eq!(handle.await.unwrap().unwrap(), "continued and finished");

    let run = h.db.get_run(run_id.clone()).await.unwrap().unwrap();
    assert_eq!(run.status, RunStatus::Completed);
    assert_eq!(
        run.model, "prior/model",
        "continuation inherits the prior run's resolved model"
    );
    assert_eq!(
        run.conversation_id.as_deref(),
        Some(job.conversation_id.as_str())
    );

    // The seeded transcript reaches the provider VERBATIM (fresh run loop;
    // nothing re-executed at admission).
    let seen = llm.seen_messages.lock().await;
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].len(), seed_messages.len());
    for (got, want) in seen[0].iter().zip(seed_messages.iter()) {
        assert_eq!(got.role, want.role);
        assert_eq!(got.content, want.content);
    }

    // The continuation's reply persists in the same job conversation.
    let msgs =
        h.db.get_messages(job.conversation_id.clone())
            .await
            .unwrap();
    assert!(msgs
        .iter()
        .any(|m| m.role == Role::Assistant && m.content == "continued and finished"));
    assert!(h.engine.hub.get(&run_id).is_none());
}

#[tokio::test]
async fn spawn_continuation_missing_job_fails() {
    let h = harness(vortex_llm::mock::shared(vec![]), Settings::default()).await;
    let seed = ContinuationSeed {
        job_id: "ghost-job".into(),
        occurrence_id: "occ-x".into(),
        prior_run_id: "prior-x".into(),
        conversation_id: "conv-x".into(),
        task: "continue".into(),
        messages: vec![ChatMessage::system("s"), ChatMessage::user("continue")],
        plan: Vec::new(),
        review_flags: Vec::new(),
    };
    let err = h.engine.spawn_continuation(seed).await.unwrap_err();
    match err {
        RunError::Failed(msg) => assert!(msg.contains("ghost-job"), "actionable message: {msg}"),
        other => panic!("expected Failed, got {other:?}"),
    }
}

#[tokio::test]
async fn spawn_continuation_creates_scratch_workspace() {
    // Only this test in the binary reads VORTEX_DATA_DIR (workspace None).
    let data_dir = tempfile::tempdir().unwrap();
    std::env::set_var("VORTEX_DATA_DIR", data_dir.path());
    let llm = RecordingLlm::new(vec![MockTurn::text("scratch done")]);
    let h = harness(llm.clone(), Settings::default()).await;
    let (_r, job) = job_fixture(&h.db, Mode::Agent, None).await;
    h.db.upsert_run(&prior_run_row(
        "prior-scratch",
        "prior/model",
        &job.conversation_id,
    ))
    .await
    .unwrap();
    let seed = ContinuationSeed {
        job_id: job.id.clone(),
        occurrence_id: "occ-scratch".into(),
        prior_run_id: "prior-scratch".into(),
        conversation_id: job.conversation_id.clone(),
        task: "continue scratch".into(),
        messages: vec![
            ChatMessage::system("job system prompt"),
            ChatMessage::user("earlier work"),
            ChatMessage::user("continue where you left off"),
        ],
        plan: Vec::new(),
        review_flags: Vec::new(),
    };
    let (run_id, handle) = h.engine.spawn_continuation(seed).await.unwrap();
    let scratch = data_dir.path().join("jobs").join(&job.id).join("workspace");
    assert!(
        scratch.is_dir(),
        "the per-job scratch workspace must exist at admission"
    );
    assert_eq!(handle.await.unwrap().unwrap(), "scratch done");
    assert_eq!(
        h.db.get_run(run_id).await.unwrap().unwrap().status,
        RunStatus::Completed
    );
}
