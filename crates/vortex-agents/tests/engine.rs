//! Engine integration tests with a scripted mock LLM. No network, no keys.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::mpsc;
use vortex_agents::engine::Engine;
use vortex_agents::{Hub, PreviewRegistry, Scheduler};
use vortex_llm::llm::{CompletionRequest, CompletionResponse, LlmClient, LlmEvent};
use vortex_types::{Mode, RunStatus, Settings, Usage};

/// A test LLM whose behavior depends on the requested model:
/// - "coordinator-model": scripted turns (tool calls → final answer)
/// - "blocker-model": never completes until `release` is fired; used to hold
///   sub-agent slots open and observe true concurrency.
struct ScenarioLlm {
    coordinator_turns: tokio::sync::Mutex<Vec<vortex_llm::MockTurn>>,
    active_children: AtomicUsize,
    high_water: AtomicUsize,
    release: tokio_util::sync::CancellationToken,
    /// Tool names visible to sub-agent completion requests (for inheritance tests).
    seen_child_tools: tokio::sync::Mutex<Vec<String>>,
}

fn scenario(turns: Vec<vortex_llm::MockTurn>) -> Arc<ScenarioLlm> {
    Arc::new(ScenarioLlm {
        coordinator_turns: tokio::sync::Mutex::new(turns),
        active_children: AtomicUsize::new(0),
        high_water: AtomicUsize::new(0),
        release: tokio_util::sync::CancellationToken::new(),
        seen_child_tools: tokio::sync::Mutex::new(Vec::new()),
    })
}

#[async_trait]
impl LlmClient for ScenarioLlm {
    async fn complete(
        &self,
        req: &CompletionRequest,
        events: mpsc::Sender<LlmEvent>,
    ) -> Result<CompletionResponse, vortex_llm::LlmError> {
        if req.model == "coordinator-model"
            && !req
                .messages
                .iter()
                .any(|m| m.content.contains("Vortex sub-agent"))
        {
            let turn = {
                let mut turns = self.coordinator_turns.lock().await;
                if turns.is_empty() {
                    vortex_llm::MockTurn::text("done")
                } else {
                    turns.remove(0)
                }
            };
            for word in turn.content.split_inclusive(' ') {
                let _ = events.try_send(LlmEvent::TextDelta(word.to_string()));
            }
            return Ok(CompletionResponse {
                content: turn.content,
                tool_calls: turn.tool_calls,
                usage: Some(Usage {
                    prompt_tokens: 5,
                    completion_tokens: 5,
                    total_tokens: 10,
                }),
                finish_reason: "stop".into(),
            });
        }
        // Blocker child: hold the slot open until released.
        {
            let mut seen = self.seen_child_tools.lock().await;
            seen.clear();
            seen.extend(req.tools.iter().map(|t| t.name.clone()));
        }
        let now = self.active_children.fetch_add(1, Ordering::SeqCst) + 1;
        self.high_water.fetch_max(now, Ordering::SeqCst);
        let _ = events.try_send(LlmEvent::TextDelta("child running".into()));
        self.release.cancelled().await;
        self.active_children.fetch_sub(1, Ordering::SeqCst);
        Ok(CompletionResponse {
            content: "child finished".into(),
            tool_calls: Vec::new(),
            usage: Some(Usage {
                prompt_tokens: 1,
                completion_tokens: 1,
                total_tokens: 2,
            }),
            finish_reason: "stop".into(),
        })
    }

    async fn models(&self) -> Result<Vec<vortex_types::ModelInfo>, vortex_llm::LlmError> {
        Ok(Vec::new())
    }

    fn describe(&self) -> &'static str {
        "scenario"
    }
}

async fn test_engine(
    llm: Arc<dyn LlmClient>,
    settings: Settings,
) -> (Arc<Engine>, vortex_core::Db) {
    let dir = tempfile::tempdir().unwrap();
    let db = vortex_core::Db::open(format!(
        "/tmp/vortex-engine-test-{}.db",
        uuid::Uuid::new_v4()
    ))
    .unwrap();
    let hub = Arc::new(Hub::new());
    let scheduler = Arc::new(Scheduler::new(settings.agents.max_concurrent));
    let browser = Arc::new(vortex_agents::browser::BrowserManager::new(
        dir.path().join("profile"),
        true,
    ));
    let previews = Arc::new(PreviewRegistry::new());
    let search = None;
    let http = vortex_agents::safe_http::SafeHttp::new(false);
    let engine_slot = Arc::new(std::sync::OnceLock::new());
    let registry = vortex_agents::build_registry(previews.clone(), engine_slot.clone());
    let approvals = Arc::new(vortex_agents::approvals::ApprovalManager::new(
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
        search,
        http,
        settings,
    );
    let _ = engine_slot.set(engine.clone());
    (engine, db)
}

/// ACCEPTANCE: the coordinator (which never takes a slot) delegates 10
/// subtasks; with the user limit set to 6, exactly six sub-agents run
/// concurrently while the rest wait in the queue, and all complete after the
/// release. Also verifies events fan out through the root run's SSE channel.
#[tokio::test]
async fn six_subagents_run_concurrently_plus_coordinator() {
    let (mut settings, _dir) = settings_with_workspace();
    settings.model = "coordinator-model".into();
    settings.agents.max_concurrent = 6;
    settings.agents.recursive = false;

    let scenario = scenario(vec![turn_with_ten_delegates()]);
    let (engine, db) = test_engine(scenario.clone(), settings.clone()).await;

    let conv = db
        .create_conversation("t", Mode::Coding, None)
        .await
        .unwrap();
    db.add_message(
        conv.id.clone(),
        vortex_types::ChatMessage::user("run 10 agents"),
    )
    .await
    .unwrap();

    let run_id = engine
        .start_chat_run(&conv.id, Mode::Coding, Vec::new(), "run 10 agents")
        .await;
    let mut events = engine.hub.subscribe(&run_id).expect("root run registered");

    // While children are held, the high-water mark of concurrent sub-agents
    // must be exactly 6 (the configured limit), with 10 requested.
    tokio::time::sleep(std::time::Duration::from_millis(800)).await;
    let high = scenario.high_water.load(Ordering::SeqCst);
    assert_eq!(
        high, 6,
        "expected exactly 6 concurrent sub-agents, saw {high}"
    );
    assert_eq!(engine.scheduler.active(), 6);
    assert_eq!(scenario.active_children.load(Ordering::SeqCst), 6);

    // The 7th..10th are queued, visible as child runs with status queued.
    let queued = db.list_child_runs(run_id.clone()).await.unwrap();
    assert_eq!(queued.len(), 10, "all ten children registered immediately");
    assert!(
        queued
            .iter()
            .filter(|r| r.status == RunStatus::Queued)
            .count()
            >= 3
    );

    // Release the children; everything finishes and statuses resolve.
    scenario.release.cancel();
    let mut finished_root = false;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while std::time::Instant::now() < deadline {
        match events.try_recv() {
            Ok((
                _,
                vortex_types::RunEvent::RunFinished {
                    status: RunStatus::Completed,
                    ..
                },
            )) => {
                finished_root = true;
                break;
            }
            Ok(_) => {}
            Err(tokio::sync::broadcast::error::TryRecvError::Empty) => {
                tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            }
            Err(_) => break,
        }
    }
    assert!(
        finished_root,
        "coordinator must finish after children complete"
    );
    assert_eq!(engine.scheduler.active(), 0);
}

fn turn_with_ten_delegates() -> vortex_llm::MockTurn {
    let mut calls = Vec::new();
    for i in 0..10 {
        calls.push(vortex_types::ToolCall {
            id: format!("call-{i}"),
            name: "delegate_subtask".into(),
            arguments: serde_json::json!({
                "task": format!("research topic {i}"),
                "title": format!("agent-{i}"),
                "tools": ["web_search", "fetch_url"],
            })
            .to_string(),
        });
    }
    vortex_llm::MockTurn {
        content: String::new(),
        tool_calls: calls,
        fail_with: None,
    }
}

/// Sub-agents inherit at most the coordinator's tool set, and recursive
/// delegation is refused server-side (not just by prompt).
#[tokio::test]
async fn subagent_recursion_guard_is_enforced_by_engine() {
    let (mut settings, _dir) = settings_with_workspace();
    settings.model = "coordinator-model".into();
    settings.tools.delegation = true;
    let scenario = scenario(vec![]);
    let (engine, _db) = test_engine(scenario.clone(), settings).await;
    scenario.release.cancel();

    // A depth-1 "agent" (sub-agent) must not be able to delegate further.
    let (mut child_ctx, _rx, cancel) =
        vortex_agents::registry::test_context(engine.current_settings().await, &[]);
    child_ctx.depth = 1;
    child_ctx.run_id = "child-run".into();
    child_ctx.root_run_id = "root-run".into();
    child_ctx.cancel = cancel;
    let denied = engine
        .spawn_subagent(&child_ctx, "nested", "subagent-y", None)
        .await;
    assert!(
        denied.is_err(),
        "recursive delegation must be refused by the engine"
    );
    let msg = denied.unwrap_err().to_string();
    assert!(
        msg.contains("recursive"),
        "error must explain the policy: {msg}"
    );

    // The coordinator (depth 0) may delegate.
    let (mut root_ctx, _rx2, cancel2) =
        vortex_agents::registry::test_context(engine.current_settings().await, &[]);
    root_ctx.depth = 0;
    root_ctx.run_id = "root-run".into();
    root_ctx.root_run_id = "root-run".into();
    root_ctx.cancel = cancel2;
    // (Not awaited to completion here — just verifying the guard passes.)
    let spawned = engine
        .spawn_subagent(&root_ctx, "ok task", "subagent-ok", None)
        .await;
    assert!(
        spawned.is_ok(),
        "coordinator delegation must be allowed: {spawned:?}"
    );
}

/// Permission inheritance: a child may only use tools in the parent's
/// allow-list — requesting more does not grant more. Verified by recording
/// the tool schemas the child's completion request actually contained.
#[tokio::test]
async fn subagent_never_exceeds_parent_toolset() {
    let (mut settings, _dir) = settings_with_workspace();
    settings.model = "coordinator-model".into();
    settings.tools.delegation = true;
    let scenario = scenario(vec![]);
    scenario.release.cancel();
    let (engine, _db) = test_engine(scenario.clone(), settings).await;

    let (mut ctx, _rx, cancel) =
        vortex_agents::registry::test_context(engine.current_settings().await, &["update_plan"]);
    ctx.run_id = "root-run".into();
    ctx.root_run_id = "root-run".into();
    ctx.cancel = cancel;
    let result = engine
        .spawn_subagent(
            &ctx,
            "do research",
            "subagent-r",
            Some(vec![
                "web_search".to_string(),
                "run_command".to_string(),
                "update_plan".to_string(),
            ]),
        )
        .await;
    assert!(result.is_ok());
    let seen = scenario.seen_child_tools.lock().await.clone();
    assert!(
        !seen.iter().any(|t| t == "run_command"),
        "child must not see run_command (not in parent allow-list), saw {seen:?}"
    );
    assert!(
        !seen.iter().any(|t| t == "web_search"),
        "child must not see web_search (not in parent allow-list), saw {seen:?}"
    );
    assert!(seen.iter().any(|t| t == "update_plan"));
    assert!(
        !seen.iter().any(|t| t == "delegate_subtask"),
        "recursive delegation is off; child must not see the delegate tool"
    );
}

fn settings_with_workspace() -> (Settings, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let settings = Settings {
        workspace: Some(dir.path().to_string_lossy().to_string()),
        agents: vortex_types::AgentSettings {
            max_concurrent: 6,
            ..Default::default()
        },
        ..Default::default()
    };
    (settings, dir)
}

/// A consequential tool call (run_command, not allowlisted) cannot complete
/// without human approval: with no decision it times out, is denied, and the
/// run finishes honestly rather than pretending success.
#[tokio::test]
async fn unapproved_command_times_out_and_is_denied() {
    let (mut settings, _dir) = settings_with_workspace();
    settings.model = "coordinator-model".into();
    settings.tools.commands = true;
    settings.agents.approval_timeout_secs = 5; // short for the test
    let scenario = scenario(vec![
        vortex_llm::MockTurn {
            content: String::new(),
            tool_calls: vec![vortex_types::ToolCall {
                id: "cmd1".into(),
                name: "run_command".into(),
                arguments: serde_json::json!({ "cmd": ["cargo", "--version"] }).to_string(),
            }],
            fail_with: None,
        },
        vortex_llm::MockTurn::text("done after command"),
    ]);
    let (engine, db) = test_engine(scenario, settings).await;
    let conv = db
        .create_conversation("t", Mode::Coding, None)
        .await
        .unwrap();
    let run_id = engine
        .start_chat_run(&conv.id, Mode::Coding, Vec::new(), "run a command")
        .await;

    let mut events = engine.hub.subscribe(&run_id).unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let mut approval_requested = false;
    let mut tool_denied = false;
    let mut run_finished = false;
    while std::time::Instant::now() < deadline {
        match events.try_recv() {
            Ok((_, vortex_types::RunEvent::ApprovalRequested { .. })) => approval_requested = true,
            Ok((_, vortex_types::RunEvent::ToolCallFinished { ok: false, .. })) => {
                tool_denied = true
            }
            Ok((_, vortex_types::RunEvent::RunFinished { .. })) => {
                run_finished = true;
                break;
            }
            Ok(_) => {}
            Err(tokio::sync::broadcast::error::TryRecvError::Empty) => {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            Err(_) => break,
        }
    }
    assert!(
        approval_requested,
        "an approval request must be surfaced to the UI"
    );
    assert!(
        tool_denied,
        "the undecided command must be reported as denied"
    );
    assert!(run_finished, "run must finish honestly after the denial");
    let msgs = db.get_messages(conv.id.clone()).await.unwrap();
    assert!(
        msgs.iter().any(|m| m.role == vortex_types::Role::Assistant),
        "final assistant message persisted"
    );
    assert_eq!(engine.scheduler.active(), 0);
}

/// Stopping the coordinator run cancels in-flight children (linked tokens)
/// and releases their scheduler slots promptly.
#[tokio::test]
async fn stopping_root_cancels_children_and_releases_slots() {
    let (mut settings, _dir) = settings_with_workspace();
    settings.model = "coordinator-model".into();
    let scenario = scenario(vec![turn_with_ten_delegates()]);
    let (engine, db) = test_engine(scenario.clone(), settings).await;
    let conv = db
        .create_conversation("t", Mode::Coding, None)
        .await
        .unwrap();
    let run_id = engine
        .start_chat_run(&conv.id, Mode::Coding, Vec::new(), "run agents then stop")
        .await;

    tokio::time::sleep(std::time::Duration::from_millis(700)).await;
    assert_eq!(scenario.active_children.load(Ordering::SeqCst), 6);

    engine.hub.cancel(&run_id);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while engine.scheduler.active() > 0 && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(
        engine.scheduler.active(),
        0,
        "scheduler slots must be released after cancel"
    );

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let kids = db.list_child_runs(run_id.clone()).await.unwrap();
        if !kids.is_empty() && kids.iter().all(|k| k.status.is_terminal()) {
            assert!(
                kids.iter().any(|k| k.status == RunStatus::Cancelled),
                "children must be marked cancelled, got {:?}",
                kids.iter().map(|k| k.status).collect::<Vec<_>>()
            );
            break;
        }
        if std::time::Instant::now() > deadline {
            panic!("children never reached a terminal status");
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

/// ACCEPTANCE (C7 drain fix): events queued through the tool-event forwarder
/// (`PlanUpdated` is sent by `update_plan` via `ctx.events`) and the directly
/// published `ToolCallFinished` must all be observable from the root
/// subscription *before* `RunFinished` — the forwarder is drained, not
/// aborted, so `RunFinished` is strictly last.
#[tokio::test]
async fn tool_events_are_delivered_before_run_finished() {
    let (settings, _dir) = settings_with_workspace();
    let llm = Arc::new(vortex_llm::MockLlm::new(vec![
        vortex_llm::MockTurn::tool_call(
            "update_plan",
            "c1",
            serde_json::json!({ "steps": ["[in_progress] do the thing"] }).to_string(),
        ),
        vortex_llm::MockTurn::text("all done"),
    ]));
    let (engine, db) = test_engine(llm, settings).await;
    let conv = db.create_conversation("t", Mode::Chat, None).await.unwrap();
    let run_id = engine
        .start_chat_run(&conv.id, Mode::Chat, Vec::new(), "plan something")
        .await;
    let mut events = engine.hub.subscribe(&run_id).unwrap();

    let mut seq_plan: Option<u64> = None;
    let mut seq_tool_finished: Option<u64> = None;
    let mut seq_run_finished: Option<u64> = None;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while std::time::Instant::now() < deadline {
        match events.try_recv() {
            Ok((seq, vortex_types::RunEvent::PlanUpdated { .. })) => {
                if seq_plan.is_none() {
                    seq_plan = Some(seq);
                }
            }
            Ok((seq, vortex_types::RunEvent::ToolCallFinished { .. })) => {
                if seq_tool_finished.is_none() {
                    seq_tool_finished = Some(seq);
                }
            }
            Ok((seq, vortex_types::RunEvent::RunFinished { status, .. })) => {
                assert_eq!(status, RunStatus::Completed);
                seq_run_finished = Some(seq);
                break;
            }
            Ok(_) => {}
            Err(tokio::sync::broadcast::error::TryRecvError::Empty) => {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            Err(_) => break,
        }
    }
    let finished = seq_run_finished.expect("run must finish");
    let plan = seq_plan.expect("update_plan must publish a PlanUpdated event");
    let tool = seq_tool_finished.expect("the tool call must report ToolCallFinished");
    assert!(
        plan < finished,
        "PlanUpdated (queued through the forwarder) must arrive before RunFinished: {plan} vs {finished}"
    );
    assert!(
        tool < finished,
        "ToolCallFinished must arrive before RunFinished: {tool} vs {finished}"
    );
}

/// ACCEPTANCE (P15): once the run's token budget is used up, the engine must
/// fail *before* the next provider call — not after another completion — and
/// the request budget clamp must not let a second call through.
#[tokio::test]
async fn token_budget_is_enforced_before_the_next_call() {
    let (mut settings, _dir) = settings_with_workspace();
    // The first mock turn reports exactly 10 total tokens (10 prompt + 0
    // completion for an empty-content tool-call turn).
    settings.agents.max_total_tokens = 10;
    let llm = Arc::new(vortex_llm::MockLlm::new(vec![
        vortex_llm::MockTurn::tool_call(
            "update_plan",
            "c1",
            serde_json::json!({ "steps": ["one step"] }).to_string(),
        ),
        vortex_llm::MockTurn::text("should never be requested"),
    ]));
    let (engine, db) = test_engine(llm.clone(), settings).await;
    let conv = db.create_conversation("t", Mode::Chat, None).await.unwrap();
    let run_id = engine
        .start_chat_run(&conv.id, Mode::Chat, Vec::new(), "budget")
        .await;
    let mut events = engine.hub.subscribe(&run_id).unwrap();

    let mut finished: Option<(RunStatus, Option<String>)> = None;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while std::time::Instant::now() < deadline {
        match events.try_recv() {
            Ok((_, vortex_types::RunEvent::RunFinished { status, error, .. })) => {
                finished = Some((status, error));
                break;
            }
            Ok(_) => {}
            Err(tokio::sync::broadcast::error::TryRecvError::Empty) => {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            Err(_) => break,
        }
    }
    let (status, error) = finished.expect("run must finish");
    assert_eq!(status, RunStatus::Failed);
    let msg = error.unwrap_or_default();
    assert!(
        msg.contains("token budget exhausted"),
        "run must fail with the pre-call budget error, got: {msg}"
    );
    assert_eq!(
        llm.calls(),
        1,
        "no second provider call may happen once the budget is gone"
    );
}

/// A provider whose completion takes ~60 simulated seconds and reports when
/// its future is dropped (N8: cancel must abort the provider task, not leave
/// it running detached).
struct AbortProbeLlm {
    active: AtomicUsize,
}

struct ProbeGuard<'a>(&'a AtomicUsize);

impl Drop for ProbeGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

#[async_trait]
impl LlmClient for AbortProbeLlm {
    async fn complete(
        &self,
        _req: &CompletionRequest,
        _events: mpsc::Sender<LlmEvent>,
    ) -> Result<CompletionResponse, vortex_llm::LlmError> {
        self.active.fetch_add(1, Ordering::SeqCst);
        let _guard = ProbeGuard(&self.active);
        for _ in 0..6000 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        Ok(CompletionResponse {
            content: "never reached".into(),
            tool_calls: Vec::new(),
            usage: None,
            finish_reason: "stop".into(),
        })
    }

    async fn models(&self) -> Result<Vec<vortex_types::ModelInfo>, vortex_llm::LlmError> {
        Ok(Vec::new())
    }

    fn describe(&self) -> &'static str {
        "abort-probe"
    }
}

/// ACCEPTANCE (N8): cancelling a run mid-completion must abort the spawned
/// provider task promptly. Before the fix, dropping the JoinHandle left the
/// completion running detached (stream kept being consumed/billed).
#[tokio::test]
async fn cancel_aborts_the_provider_task() {
    let (settings, _dir) = settings_with_workspace();
    let probe = Arc::new(AbortProbeLlm {
        active: AtomicUsize::new(0),
    });
    let (engine, db) = test_engine(probe.clone(), settings).await;
    let conv = db.create_conversation("t", Mode::Chat, None).await.unwrap();
    let run_id = engine
        .start_chat_run(&conv.id, Mode::Chat, Vec::new(), "long completion")
        .await;
    let mut events = engine.hub.subscribe(&run_id).unwrap();

    // Wait until the provider task is demonstrably in flight.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while probe.active.load(Ordering::SeqCst) == 0 && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(
        probe.active.load(Ordering::SeqCst),
        1,
        "provider task never started"
    );

    engine.hub.cancel(&run_id);

    // Abort must drop the completion future promptly; a detached task would
    // keep it alive for the full 60 simulated seconds.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while probe.active.load(Ordering::SeqCst) == 1 && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(
        probe.active.load(Ordering::SeqCst),
        0,
        "provider task was not aborted on cancel (it is still running detached)"
    );

    // And the run must finish honestly as cancelled.
    let mut finished = false;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        match events.try_recv() {
            Ok((_, vortex_types::RunEvent::RunFinished { status, .. })) => {
                assert_eq!(status, RunStatus::Cancelled);
                finished = true;
                break;
            }
            Ok(_) => {}
            Err(tokio::sync::broadcast::error::TryRecvError::Empty) => {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            Err(_) => break,
        }
    }
    assert!(finished, "cancelled run must publish RunFinished");
}
