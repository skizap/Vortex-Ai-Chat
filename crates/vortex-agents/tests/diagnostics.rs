//! Offline tests for the gated diagnostics events at engine/tool/approval/
//! provider/database failure boundaries. No network, no keys.
//!
//! Assertions check two invariants:
//! 1. When `VORTEX_DIAGNOSTICS` gating is force-enabled, failure boundaries
//!    emit fixed-category events carrying only validated identifiers.
//! 2. Sentinel secrets (in prompts, tool args, tool results, provider error
//!    text, panic payloads) never appear in the captured diagnostics output,
//!    and nothing is emitted while the gate is off.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio::sync::mpsc;
use vortex_agents::engine::{Engine, RunError, RunSpec};
use vortex_agents::errors::{Risk, ToolError};
use vortex_agents::registry::{DenyGate, Tool, ToolContext, ToolRegistry, ToolResult};
use vortex_agents::schema::{ParamSpec, ParamType, ToolDef};
use vortex_core::diag;
use vortex_llm::llm::{CompletionRequest, CompletionResponse, LlmClient, LlmEvent};
use vortex_types::{ChatMessage, Mode, RunStatus, Settings};

const SENTINEL: &str = "sk-SENTINEL-must-not-be-logged";
const RUN_ID: &str = "0f1e2d3c-4b5a-6978-8796-a5b4c3d2e1f0";

/// Minimal capturing tracing subscriber (records rendered event messages).
#[derive(Default, Clone)]
struct Capture {
    lines: Arc<Mutex<Vec<String>>>,
}

impl tracing::Subscriber for Capture {
    fn enabled(&self, _: &tracing::metadata::Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        let mut msg = String::new();
        event.record(&mut Recorder(&mut msg));
        self.lines.lock().unwrap().push(msg);
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

struct Recorder<'a>(&'a mut String);
impl tracing::field::Visit for Recorder<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.0.push_str(&format!("{value:?}"));
        }
    }
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "message" {
            self.0.push_str(value);
        }
    }
}

/// A tool that fails with a sentinel-bearing error and sentinel arguments.
struct FailingTool;

#[async_trait]
impl Tool for FailingTool {
    fn def(&self) -> &'static ToolDef {
        static DEF: ToolDef = ToolDef {
            name: "explode",
            description: "always fails",
            params: &[ParamSpec {
                name: "payload",
                ty: ParamType::Str,
                required: true,
                description: "sentinel carrier",
            }],
        };
        &DEF
    }
    async fn execute(
        &self,
        _ctx: &ToolContext,
        _args: &serde_json::Value,
    ) -> Result<ToolResult, ToolError> {
        Err(ToolError::Execution(format!("boom with secret {SENTINEL}")))
    }
}

/// A consequential tool whose approval is always denied (never executed).
struct ConsequentialTool;

#[async_trait]
impl Tool for ConsequentialTool {
    fn def(&self) -> &'static ToolDef {
        static DEF: ToolDef = ToolDef {
            name: "needs_human",
            description: "consequential",
            params: &[],
        };
        &DEF
    }
    fn risk(&self) -> Risk {
        Risk::Consequential
    }
    async fn execute(
        &self,
        _ctx: &ToolContext,
        _args: &serde_json::Value,
    ) -> Result<ToolResult, ToolError> {
        Ok(ToolResult::text("should never run"))
    }
}

struct AlwaysFailingLlm;

#[async_trait]
impl LlmClient for AlwaysFailingLlm {
    async fn complete(
        &self,
        _req: &CompletionRequest,
        _events: mpsc::Sender<LlmEvent>,
    ) -> Result<CompletionResponse, vortex_llm::LlmError> {
        Err(vortex_llm::LlmError::Provider(
            503,
            format!("provider said: {SENTINEL}"),
        ))
    }
    async fn models(&self) -> Result<Vec<vortex_types::ModelInfo>, vortex_llm::LlmError> {
        Err(vortex_llm::LlmError::NotConfigured)
    }
    fn describe(&self) -> &'static str {
        "failing"
    }
}

/// A provider client whose task panics with a sentinel payload: the panic
/// must be recorded as a structured boundary event, never with its payload.
struct PanickingLlm;

#[async_trait]
impl LlmClient for PanickingLlm {
    async fn complete(
        &self,
        _req: &CompletionRequest,
        _events: mpsc::Sender<LlmEvent>,
    ) -> Result<CompletionResponse, vortex_llm::LlmError> {
        panic!("provider thread exploded: {SENTINEL}");
    }
    async fn models(&self) -> Result<Vec<vortex_types::ModelInfo>, vortex_llm::LlmError> {
        Err(vortex_llm::LlmError::NotConfigured)
    }
    fn describe(&self) -> &'static str {
        "panicking"
    }
}

fn ctx_with_run() -> ToolContext {
    let (ctx, _rx, _cancel) = vortex_agents::registry::test_context(Settings::default(), &[]);
    ctx
}

/// A provider that queues one text delta into the stream and *then* fails:
/// the regression test for the provider-error ordering (the delta must reach
/// the hub before the failure completion event, exactly as before
/// instrumentation).
struct DeltaThenFailLlm;

#[async_trait]
impl LlmClient for DeltaThenFailLlm {
    async fn complete(
        &self,
        _req: &CompletionRequest,
        events: mpsc::Sender<LlmEvent>,
    ) -> Result<CompletionResponse, vortex_llm::LlmError> {
        events
            .send(LlmEvent::TextDelta("partial answer ".into()))
            .await
            .expect("delta forwarder is alive while the provider task runs");
        Err(vortex_llm::LlmError::Provider(
            503,
            format!("provider said: {SENTINEL}"),
        ))
    }
    async fn models(&self) -> Result<Vec<vortex_types::ModelInfo>, vortex_llm::LlmError> {
        Err(vortex_llm::LlmError::NotConfigured)
    }
    fn describe(&self) -> &'static str {
        "delta-then-fail"
    }
}

/// A tool whose `def()` panics with a sentinel payload. `schemas()` (called
/// synchronously inside the coordinator's `run_loop`) iterates every
/// registered tool, so this panics inside `execute_run` on the detached
/// coordinator task — the fixture for the stuck-run behavior.
struct PanickingDefTool;

#[async_trait]
impl Tool for PanickingDefTool {
    fn def(&self) -> &'static ToolDef {
        panic!("def exploded: {SENTINEL}");
    }
    async fn execute(
        &self,
        _ctx: &ToolContext,
        _args: &serde_json::Value,
    ) -> Result<ToolResult, ToolError> {
        Ok(ToolResult::text("never reached"))
    }
}

/// Shared engine construction for the run-level fixtures below.
fn engine_with_registry(
    llm: Arc<dyn LlmClient>,
    registry: Arc<ToolRegistry>,
    dir: &tempfile::TempDir,
) -> (Arc<Engine>, Arc<vortex_agents::Hub>) {
    let db = vortex_core::Db::open(dir.path().join("diag-engine.db")).unwrap();
    let hub = Arc::new(vortex_agents::Hub::new());
    let scheduler = Arc::new(vortex_agents::Scheduler::new(2));
    let browser = Arc::new(vortex_agents::browser::BrowserManager::new(
        dir.path().join("profile"),
        false,
    ));
    let previews = Arc::new(vortex_agents::PreviewRegistry::new());
    let engine_slot = Arc::new(std::sync::OnceLock::new());
    let approvals = Arc::new(vortex_agents::approvals::ApprovalManager::new(
        db.clone(),
        hub.clone(),
        30,
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
        Settings::default(),
        None,
    );
    let _ = engine_slot.set(engine.clone());
    (engine, hub)
}

fn engine_with(
    llm: Arc<dyn LlmClient>,
    dir: &tempfile::TempDir,
) -> (Arc<Engine>, Arc<vortex_agents::Hub>) {
    let previews = Arc::new(vortex_agents::PreviewRegistry::new());
    let engine_slot = Arc::new(std::sync::OnceLock::new());
    let registry = vortex_agents::build_registry(previews.clone(), engine_slot.clone());
    engine_with_registry(llm, registry, dir)
}

/// Regression for the provider-error ordering: a provider that queues a delta
/// and then fails must have that delta forwarded (and observed by the hub)
/// strictly before the failure completion event is published. This is the
/// exact ordering the original `select` + `fwd.await` + `resp?` structure
/// guaranteed. The run is hub-registered by the test itself and driven via
/// `execute_run` directly so the replay log is deterministic (the detached
/// coordinator path removes the hub entry on normal completion, which would
/// race the replay read).
#[tokio::test]
async fn provider_error_forwards_queued_deltas_before_failure() {
    let _guard = diag::force_enabled_for_tests();
    let capture = Capture::default();
    let dir = tempfile::tempdir().unwrap();
    let (engine, hub) = engine_with(Arc::new(DeltaThenFailLlm), &dir);

    let run_id = RUN_ID.to_string();
    hub.register(
        &run_id,
        None,
        None,
        "coordinator",
        "task",
        "test-model",
        0,
        tokio_util::sync::CancellationToken::new(),
    );

    let _sub = tracing::subscriber::set_default(capture.clone());
    let result = engine
        .execute_run(RunSpec {
            run_id: run_id.clone(),
            root_run_id: run_id.clone(),
            conversation_id: None,
            parent_run_id: None,
            agent: "coordinator".into(),
            task: "hi".into(),
            depth: 0,
            allowed: Default::default(),
            messages: vec![ChatMessage::user("hi")],
            model: "test-model".into(),
            settings: Settings::default(),
            search: None,
            cancel: tokio_util::sync::CancellationToken::new(),
            is_root: true,
            needs_slot: false,
            origin: vortex_types::RunOrigin::Interactive,
            approval_policy: vortex_types::ApprovalPolicy::TimeoutSecs(300),
        })
        .await;
    assert!(matches!(result, Err(RunError::Failed(_))));

    // The run is never hub-removed on this direct path, so the replay log is
    // complete; poll until the failure completion event has landed.
    let events = loop {
        let (events, _, _) = hub.replay_from(&run_id, 0);
        if events.iter().any(|(_, e)| {
            matches!(
                e,
                vortex_types::RunEvent::RunFinished {
                    status: RunStatus::Failed,
                    ..
                }
            )
        }) {
            break events;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    };
    drop(_sub);

    let delta_idx = events
        .iter()
        .position(|(_, e)| matches!(e, vortex_types::RunEvent::TextDelta { .. }))
        .expect("queued delta must be forwarded");
    let failed_idx = events
        .iter()
        .position(|(_, e)| {
            matches!(
                e,
                vortex_types::RunEvent::RunFinished {
                    status: RunStatus::Failed,
                    ..
                }
            )
        })
        .expect("run must finish with Failed");
    assert!(
        delta_idx < failed_idx,
        "delta (log idx {delta_idx}) must be forwarded before the failure \
         completion (log idx {failed_idx}); events: {events:?}"
    );

    // The provider boundary is still recorded, safely: fixed category, the
    // run UUID, and the numeric status — never the provider error text.
    let lines = capture.lines.lock().unwrap().clone();
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("category=run_provider_failed")
                && l.contains(&format!("run_id={run_id}"))
                && l.contains("reason=provider_status")
                && l.contains("status=503")),
        "expected provider boundary event: {lines:?}"
    );
    assert!(
        !lines.iter().any(|l| l.contains("SENTINEL")),
        "provider error text must not reach diagnostics: {lines:?}"
    );
}

/// Regression for the detached coordinator panic behavior: a panic inside
/// `execute_run` must be recorded as a structured boundary event and then
/// resume unwinding, leaving the hub entry exactly as the pre
/// instrumentation code did — the run stays registered (the pre-existing
/// stuck-run state), and no hub cleanup happens on panic.
#[tokio::test]
async fn coordinator_panic_preserves_stuck_run_state() {
    let _guard = diag::force_enabled_for_tests();
    let capture = Capture::default();
    let dir = tempfile::tempdir().unwrap();
    // A tool whose def() panics: `schemas()` inside the coordinator's
    // run_loop iterates every registered tool, so this panics on the
    // detached coordinator task. A minimal registry keeps the panicking def
    // isolated from the other fixtures.
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(PanickingDefTool));
    let (engine, hub) = engine_with_registry(Arc::new(AlwaysFailingLlm), Arc::new(registry), &dir);

    let _sub = tracing::subscriber::set_default(capture.clone());
    let run_id = engine
        .start_chat_run(
            &uuid::Uuid::new_v4().to_string(),
            Mode::Chat,
            Vec::new(),
            "hi",
            Vec::new(),
        )
        .await;

    // Wait for the panic boundary event before asserting hub state: the
    // detached task logs it right before resuming the unwind.
    let mut saw_panic_boundary = false;
    for _ in 0..250 {
        if capture
            .lines
            .lock()
            .unwrap()
            .iter()
            .any(|l| l.starts_with("category=coordinator_task_panic"))
        {
            saw_panic_boundary = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    drop(_sub);

    assert!(
        saw_panic_boundary,
        "expected coordinator_task_panic boundary event: {:?}",
        capture.lines.lock().unwrap()
    );
    // The pre-existing stuck-run behavior: the hub entry must still exist
    // after the panic (no hub.remove on the panic path), still marked
    // Running — recovery for such runs happens at startup, unchanged.
    let handle = hub
        .get(&run_id)
        .expect("hub entry must survive a coordinator panic (stuck-run behavior preserved)");
    assert_eq!(
        handle.status(),
        RunStatus::Running,
        "stuck run must keep its pre-panic status"
    );
    // And the payload (sentinel) never reaches diagnostics.
    let lines = capture.lines.lock().unwrap().clone();
    assert!(
        !lines.iter().any(|l| l.contains("SENTINEL")),
        "panic payload must not reach diagnostics: {lines:?}"
    );
}

#[tokio::test]
async fn tool_failure_boundary_emits_only_safe_fields() {
    let _guard = diag::force_enabled_for_tests();
    let capture = Capture::default();
    let mut ctx = ctx_with_run();
    ctx.run_id = RUN_ID.into();
    ctx.allowed.insert("explode".into());

    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(FailingTool));
    let args_with_secret = serde_json::json!({ "payload": SENTINEL });
    let _sub = tracing::subscriber::set_default(capture.clone());
    let outcome = registry.execute(&ctx, "explode", &args_with_secret).await;
    drop(_sub);
    assert!(!outcome.ok);

    let lines = capture.lines.lock().unwrap().clone();
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("category=tool_failed")
                && l.contains(&format!("run_id={RUN_ID}"))),
        "expected tool_failed boundary event, got: {lines:?}"
    );
    assert!(
        lines.iter().any(|l| l.contains("tool=explode")),
        "validated tool name should be present: {lines:?}"
    );
    assert!(
        !lines.iter().any(|l| l.contains("SENTINEL")),
        "tool error text must not reach diagnostics: {lines:?}"
    );
}

#[tokio::test]
async fn unknown_tool_never_logs_the_model_chosen_name() {
    let _guard = diag::force_enabled_for_tests();
    let capture = Capture::default();
    let mut ctx = ctx_with_run();
    ctx.run_id = RUN_ID.into();

    let registry = ToolRegistry::new();
    let injected = "web_search'; DROP TABLE tools; --";
    let _sub = tracing::subscriber::set_default(capture.clone());
    let outcome = registry
        .execute(&ctx, injected, &serde_json::json!({}))
        .await;
    drop(_sub);
    assert!(!outcome.ok);

    let lines = capture.lines.lock().unwrap().clone();
    assert!(
        lines.iter().any(|l| l.starts_with("category=tool_unknown")),
        "expected tool_unknown event: {lines:?}"
    );
    assert!(
        !lines.iter().any(|l| l.contains("DROP")),
        "unvalidated model-chosen identifier must not be logged: {lines:?}"
    );
}

#[tokio::test]
async fn consequential_tool_denial_is_recorded_without_payload() {
    let _guard = diag::force_enabled_for_tests();
    let capture = Capture::default();
    let mut ctx = ctx_with_run();
    ctx.run_id = RUN_ID.into();
    ctx.allowed.insert("needs_human".into());
    // Deny everything, like the real gate does on timeout.
    ctx.approvals = Arc::new(DenyGate);

    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(ConsequentialTool));

    let _sub = tracing::subscriber::set_default(capture.clone());
    let outcome = registry
        .execute(&ctx, "needs_human", &serde_json::json!({}))
        .await;
    drop(_sub);
    assert!(!outcome.ok);

    let lines = capture.lines.lock().unwrap().clone();
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("category=approval_denied")),
        "expected approval_denied event: {lines:?}"
    );
    // Approval payloads (tool arguments) are user-visible content: never logged.
    assert!(
        !lines.iter().any(|l| l.contains("SENTINEL")),
        "approval payload must not reach diagnostics: {lines:?}"
    );
}

#[tokio::test]
async fn nothing_is_emitted_while_the_gate_is_off() {
    let _guard = diag::force_disabled_for_tests();
    let capture = Capture::default();
    let mut ctx = ctx_with_run();
    ctx.run_id = RUN_ID.into();

    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(FailingTool));
    let _sub = tracing::subscriber::set_default(capture.clone());
    let outcome = registry
        .execute(&ctx, "explode", &serde_json::json!({ "payload": SENTINEL }))
        .await;
    drop(_sub);
    assert!(!outcome.ok);

    let lines = capture.lines.lock().unwrap().clone();
    assert!(
        lines.is_empty(),
        "diagnostics must be silent when gated off: {lines:?}"
    );
}

/// The provider/run failure boundary through the real engine loop: the model
/// call fails with a sentinel-bearing provider error; diagnostics must carry
/// only fixed categories, the run UUID, and the HTTP status number.
#[tokio::test]
async fn provider_and_run_failures_emit_fixed_categories_only() {
    let _guard = diag::force_enabled_for_tests();
    let capture = Capture::default();
    let dir = tempfile::tempdir().unwrap();
    let db = vortex_core::Db::open(dir.path().join("diag-test.db")).unwrap();
    let hub = Arc::new(vortex_agents::Hub::new());
    let scheduler = Arc::new(vortex_agents::Scheduler::new(2));
    let browser = Arc::new(vortex_agents::browser::BrowserManager::new(
        dir.path().join("profile"),
        false,
    ));
    let previews = Arc::new(vortex_agents::PreviewRegistry::new());
    let engine_slot = Arc::new(std::sync::OnceLock::new());
    let registry = vortex_agents::build_registry(previews.clone(), engine_slot.clone());
    let approvals = Arc::new(vortex_agents::approvals::ApprovalManager::new(
        db.clone(),
        hub.clone(),
        30,
    ));
    let engine = Engine::new(
        Arc::new(AlwaysFailingLlm),
        registry,
        hub.clone(),
        scheduler,
        approvals,
        db.clone(),
        browser,
        previews,
        None,
        vortex_agents::safe_http::SafeHttp::new(false),
        Settings::default(),
        None,
    );
    let _ = engine_slot.set(engine.clone());

    let prompt_with_secret = format!("please leak this {SENTINEL}");
    let _sub = tracing::subscriber::set_default(capture.clone());
    let result = engine
        .execute_run(RunSpec {
            run_id: RUN_ID.into(),
            root_run_id: RUN_ID.into(),
            conversation_id: None,
            parent_run_id: None,
            agent: "coordinator".into(),
            task: prompt_with_secret,
            depth: 0,
            allowed: Default::default(),
            messages: vec![ChatMessage::user("hi")],
            model: "test-model".into(),
            settings: Settings::default(),
            search: None,
            cancel: tokio_util::sync::CancellationToken::new(),
            is_root: true,
            needs_slot: false,
            origin: vortex_types::RunOrigin::Interactive,
            approval_policy: vortex_types::ApprovalPolicy::TimeoutSecs(300),
        })
        .await;
    drop(_sub);
    assert!(matches!(result, Err(RunError::Failed(_))));
    // A directly executed run is never hub-registered (start_chat_run does
    // that); the failure boundary events are what this test asserts.

    let lines = capture.lines.lock().unwrap().clone();
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("category=run_provider_failed")
                && l.contains(&format!("run_id={RUN_ID}"))
                && l.contains("reason=provider_status")
                && l.contains("status=503")),
        "expected provider boundary with status only: {lines:?}"
    );
    assert!(
        lines.iter().any(|l| l.starts_with("category=run_failed")),
        "expected final run failure boundary: {lines:?}"
    );
    assert!(
        !lines.iter().any(|l| l.contains("SENTINEL")),
        "provider error text and prompts must not reach diagnostics: {lines:?}"
    );
}

/// A panicking provider task is recorded as a structured boundary event; the
/// panic payload (sentinel-bearing) never reaches diagnostics.
#[tokio::test]
async fn provider_task_panic_is_recorded_without_payload() {
    let _guard = diag::force_enabled_for_tests();
    let capture = Capture::default();
    let dir = tempfile::tempdir().unwrap();
    let db = vortex_core::Db::open(dir.path().join("diag-panic-test.db")).unwrap();
    let hub = Arc::new(vortex_agents::Hub::new());
    let scheduler = Arc::new(vortex_agents::Scheduler::new(2));
    let browser = Arc::new(vortex_agents::browser::BrowserManager::new(
        dir.path().join("profile"),
        false,
    ));
    let previews = Arc::new(vortex_agents::PreviewRegistry::new());
    let engine_slot = Arc::new(std::sync::OnceLock::new());
    let registry = vortex_agents::build_registry(previews.clone(), engine_slot.clone());
    let approvals = Arc::new(vortex_agents::approvals::ApprovalManager::new(
        db.clone(),
        hub.clone(),
        30,
    ));
    let engine = Engine::new(
        Arc::new(PanickingLlm),
        registry,
        hub.clone(),
        scheduler,
        approvals,
        db.clone(),
        browser,
        previews,
        None,
        vortex_agents::safe_http::SafeHttp::new(false),
        Settings::default(),
        None,
    );
    let _ = engine_slot.set(engine.clone());

    let _sub = tracing::subscriber::set_default(capture.clone());
    let result = engine
        .execute_run(RunSpec {
            run_id: RUN_ID.into(),
            root_run_id: RUN_ID.into(),
            conversation_id: None,
            parent_run_id: None,
            agent: "coordinator".into(),
            task: "trigger".into(),
            depth: 0,
            allowed: Default::default(),
            messages: vec![ChatMessage::user("hi")],
            model: "test-model".into(),
            settings: Settings::default(),
            search: None,
            cancel: tokio_util::sync::CancellationToken::new(),
            is_root: true,
            needs_slot: false,
            origin: vortex_types::RunOrigin::Interactive,
            approval_policy: vortex_types::ApprovalPolicy::TimeoutSecs(300),
        })
        .await;
    drop(_sub);
    match result {
        Err(RunError::Failed(msg)) => {
            assert!(msg.contains("provider task failed"));
        }
        other => panic!("expected provider task failure, got {other:?}"),
    }

    let lines = capture.lines.lock().unwrap().clone();
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("category=run_provider_task_panic")
                && l.contains(&format!("run_id={RUN_ID}"))
                && l.contains("cancelled=false")),
        "expected provider task panic boundary: {lines:?}"
    );
    assert!(
        !lines.iter().any(|l| l.contains("SENTINEL")),
        "panic payload must not reach diagnostics: {lines:?}"
    );
}

/// Ignored persistence errors (the `let _ = db...` sites) are surfaced through
/// the gated channel without changing recovery behavior, including on the
/// detached coordinator task.
#[tokio::test]
async fn ignored_db_persistence_failures_are_surfaced() {
    let _guard = diag::force_enabled_for_tests();
    let capture = Capture::default();
    let dir = tempfile::tempdir().unwrap();
    let db = vortex_core::Db::open(dir.path().join("diag-db-test.db")).unwrap();
    // Sabotage persistence without changing any recovery logic: dropping the
    // `runs` table makes every run write fail until restart.
    {
        let conn = db.raw_connection();
        conn.execute("DROP TABLE runs", []).unwrap();
    }
    let hub = Arc::new(vortex_agents::Hub::new());
    let scheduler = Arc::new(vortex_agents::Scheduler::new(2));
    let browser = Arc::new(vortex_agents::browser::BrowserManager::new(
        dir.path().join("profile"),
        false,
    ));
    let previews = Arc::new(vortex_agents::PreviewRegistry::new());
    let engine_slot = Arc::new(std::sync::OnceLock::new());
    let registry = vortex_agents::build_registry(previews.clone(), engine_slot.clone());
    let approvals = Arc::new(vortex_agents::approvals::ApprovalManager::new(
        db.clone(),
        hub.clone(),
        30,
    ));
    let engine = Engine::new(
        Arc::new(AlwaysFailingLlm),
        registry,
        hub.clone(),
        scheduler,
        approvals,
        db.clone(),
        browser,
        previews,
        None,
        vortex_agents::safe_http::SafeHttp::new(false),
        Settings::default(),
        None,
    );
    let _ = engine_slot.set(engine.clone());

    // Detached coordinator run: exercises the spawn wrapper, the run-upsert
    // persist failure, and the run-status persist failure in one path.
    let conversation_id = uuid::Uuid::new_v4().to_string();
    let _sub = tracing::subscriber::set_default(capture.clone());
    let run_id = engine
        .start_chat_run(
            &conversation_id,
            Mode::Chat,
            Vec::new(),
            "hello",
            Vec::new(),
        )
        .await;

    // The run executes in the background; wait for the failure boundary to
    // settle by polling the hub status.
    for _ in 0..150 {
        if let Some(h) = hub.get(&run_id) {
            if matches!(
                h.status(),
                RunStatus::Failed | RunStatus::Completed | RunStatus::Cancelled
            ) {
                break;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    drop(_sub);

    let lines = capture.lines.lock().unwrap().clone();
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("category=db_persist_failed") && l.contains("op=run_upsert")),
        "ignored run upsert failure should be surfaced: {lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("category=db_persist_failed") && l.contains("op=run_status")),
        "ignored run status persist failure should be surfaced: {lines:?}"
    );
    assert!(
        !lines.iter().any(|l| l.contains("SENTINEL")),
        "no dynamic content may leak: {lines:?}"
    );
}
