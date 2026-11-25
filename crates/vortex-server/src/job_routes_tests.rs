//! Offline route tests for the jobs/routines REST + SSE surface (SA6).
//! Follows the `search_probe_tests` harness: the full guarded app on an
//! ephemeral loopback port, tempdir SQLite, mock LLM, and a REAL Supervisor
//! whose run loop is deliberately NOT spawned — tests drive `tick_once` /
//! `catch_up` manually. No external network; everything binds 127.0.0.1:0.

use crate::state::AppState;
use axum::http::StatusCode;
use std::sync::Arc;
use tokio_stream::StreamExt;
use tokio_util::sync::CancellationToken;
use vortex_llm::{MockLlm, MockTurn};
use vortex_types::{
    ChatMessage, Job, JobBudgets, JobStatus, MissedPolicy, Mode, NewJob, NewRoutine, Occurrence,
    OccurrenceStatus, OverlapPolicy, PlanStep, RunInfo, RunStatus, ScheduleSpec, Settings,
    StepStatus,
};

// ---------------------------------------------------------------------------
// Full guarded app on an ephemeral loopback port.
// ---------------------------------------------------------------------------

struct Harness {
    base: String,
    state: Arc<AppState>,
    cancel: CancellationToken,
    _dir: tempfile::TempDir,
}

impl Harness {
    fn ws(&self) -> std::path::PathBuf {
        self._dir.path().join("ws")
    }
}

async fn jobs_app(llm: Arc<dyn vortex_llm::LlmClient>) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let db = vortex_core::Db::open(dir.path().join("test.db")).unwrap();
    let settings = Settings {
        model: "mock/model".into(),
        ..Default::default()
    };
    let attachments = Arc::new(
        vortex_core::AttachmentStore::new(dir.path().join("attachments"), db.clone()).unwrap(),
    );
    let hub = Arc::new(vortex_agents::Hub::new());
    let scheduler = Arc::new(vortex_agents::Scheduler::new(
        settings.agents.max_concurrent,
    ));
    let browser = Arc::new(vortex_agents::browser::BrowserManager::new(
        dir.path().join("profile"),
        true,
    ));
    let previews = Arc::new(vortex_agents::PreviewRegistry::new());
    let approvals = Arc::new(vortex_agents::approvals::ApprovalManager::new(
        db.clone(),
        hub.clone(),
        settings.agents.approval_timeout_secs,
    ));
    let engine_slot = Arc::new(std::sync::OnceLock::new());
    let registry = vortex_agents::build_registry(previews.clone(), engine_slot.clone());
    let http = vortex_agents::safe_http::SafeHttp::new(false);
    let search = crate::state::build_search_provider(&settings, true);
    let engine = vortex_agents::Engine::new(
        llm,
        registry,
        hub.clone(),
        scheduler,
        approvals,
        db.clone(),
        browser,
        previews.clone(),
        search,
        http,
        settings.clone(),
        Some(attachments.clone()),
    );
    let _ = engine_slot.set(engine.clone());
    let mut config = vortex_core::AppConfig::default();
    config.llm.provider = "mock".into();
    let cancel = CancellationToken::new();
    let supervisor = vortex_agents::supervisor::Supervisor::new(
        db.clone(),
        engine.clone(),
        hub.clone(),
        Arc::new(vortex_agents::clock::SystemClock),
        2,
        cancel.clone(),
    );
    // run() is NOT spawned: tests drive tick_once/catch_up manually so no
    // background tick can race the assertions.
    let state = Arc::new(AppState {
        db,
        engine,
        hub,
        config,
        previews,
        attachments,
        is_mock_llm: true,
        version: "test",
        guard: crate::guard::GuardSettings {
            lan_enabled: false,
            lan_token: Arc::from(""),
        },
        diagnostics: crate::diagnostics::DiagnosticsState::new(false),
        supervisor,
    });
    let app = crate::build_router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    Harness {
        base: format!("http://{addr}"),
        state,
        cancel,
        _dir: dir,
    }
}

fn mock_llm() -> Arc<dyn vortex_llm::LlmClient> {
    Arc::new(MockLlm::new(vec![MockTurn::text("job done")]))
}

// ---------------------------------------------------------------------------
// HTTP helpers
// ---------------------------------------------------------------------------

async fn post(h: &Harness, path: &str, body: serde_json::Value) -> (StatusCode, serde_json::Value) {
    let resp = reqwest::Client::new()
        .post(format!("{}{path}", h.base))
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = resp.status();
    let body = resp.json().await.unwrap_or(serde_json::Value::Null);
    (status, body)
}

async fn ok_post(h: &Harness, path: &str, body: serde_json::Value) -> serde_json::Value {
    let (status, body) = post(h, path, body).await;
    assert_eq!(status, StatusCode::OK, "POST {path} failed: {body}");
    body
}

async fn get(h: &Harness, path: &str) -> (StatusCode, serde_json::Value) {
    let resp = reqwest::get(format!("{}{path}", h.base)).await.unwrap();
    let status = resp.status();
    let body = resp.json().await.unwrap_or(serde_json::Value::Null);
    (status, body)
}

async fn ok_get(h: &Harness, path: &str) -> serde_json::Value {
    let (status, body) = get(h, path).await;
    assert_eq!(status, StatusCode::OK, "GET {path} failed: {body}");
    body
}

async fn patch(
    h: &Harness,
    path: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let resp = reqwest::Client::new()
        .patch(format!("{}{path}", h.base))
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = resp.status();
    let body = resp.json().await.unwrap_or(serde_json::Value::Null);
    (status, body)
}

async fn delete(h: &Harness, path: &str) -> (StatusCode, serde_json::Value) {
    let resp = reqwest::Client::new()
        .delete(format!("{}{path}", h.base))
        .send()
        .await
        .unwrap();
    let status = resp.status();
    let body = resp.json().await.unwrap_or(serde_json::Value::Null);
    (status, body)
}

/// Poll an occurrence until it reaches `want` (real time, like `wait_run` in
/// search_probe_tests: spawned admission tasks and spawn_blocking DB hops
/// need wall-clock progress).
async fn wait_occ(h: &Harness, occ_id: &str, want: OccurrenceStatus) -> Occurrence {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let occ = h
            .state
            .db
            .get_occurrence(occ_id.to_string())
            .await
            .unwrap()
            .expect("occurrence exists");
        if occ.status == want {
            return occ;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "occurrence {occ_id} never reached {want:?}: {occ:?}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}

// ---------------------------------------------------------------------------
// Store-level seeding helpers
// ---------------------------------------------------------------------------

fn spec(cron: &str) -> ScheduleSpec {
    ScheduleSpec {
        cron: cron.into(),
        tz: "local".into(),
        missed_policy: MissedPolicy::RunOnceIfMissed,
    }
}

async fn seed_active_job(h: &Harness, name: &str, schedule: Option<ScheduleSpec>) -> Job {
    let routine = h
        .state
        .db
        .create_routine(NewRoutine {
            name: format!("{name}-routine"),
            description: String::new(),
            instructions: "run the job".into(),
            mode: Mode::Agent,
            model: None,
        })
        .await
        .unwrap();
    h.state
        .db
        .activate_routine(routine.id.clone())
        .await
        .unwrap();
    let job = h
        .state
        .db
        .create_job(NewJob {
            name: name.into(),
            routine_id: routine.id,
            routine_version: 1,
            schedule,
            overlap: OverlapPolicy::Skip,
            budgets: JobBudgets::default(),
            workspace: Some(h.ws().to_string_lossy().into_owned()),
        })
        .await
        .unwrap();
    h.state
        .db
        .set_job_status(job.id.clone(), JobStatus::Active)
        .await
        .unwrap();
    h.state.db.get_job(job.id).await.unwrap().unwrap()
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

// ---------------------------------------------------------------------------
// SSE helpers (mirror routes_runs tests)
// ---------------------------------------------------------------------------

/// Connect and accumulate SSE frames until `done` accepts the buffer (or
/// 10 s elapse). Returns everything received so far.
async fn read_sse_until(
    url: &str,
    headers: &[(&str, &str)],
    done: impl Fn(&str) -> bool,
) -> String {
    let client = reqwest::Client::new();
    let mut req = client.get(url);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let resp = req.send().await.expect("sse connect");
    assert_eq!(resp.status(), 200);
    let mut stream = resp.bytes_stream();
    let mut buf = String::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    while tokio::time::Instant::now() < deadline && !done(&buf) {
        match tokio::time::timeout(std::time::Duration::from_secs(5), stream.next()).await {
            Ok(Some(Ok(bytes))) => buf.push_str(&String::from_utf8_lossy(&bytes)),
            Ok(Some(Err(e))) => panic!("sse stream error: {e}"),
            Ok(None) => break,
            Err(_) => break,
        }
    }
    buf
}

fn sse_ids(buf: &str) -> Vec<u64> {
    buf.lines()
        .filter_map(|l| l.strip_prefix("id:"))
        .filter_map(|v| v.trim().parse::<u64>().ok())
        .collect()
}

// ---------------------------------------------------------------------------
// Guard
// ---------------------------------------------------------------------------

#[tokio::test]
async fn jobs_routes_keep_the_request_guard() {
    let h = jobs_app(mock_llm()).await;
    let client = reqwest::Client::new();
    let wizard = serde_json::json!({
        "name": "guarded", "instructions": "do work", "schedule": null
    });

    // Cross-origin writes are refused on every new write route.
    for (method, path) in [
        ("POST", "/api/jobs"),
        ("POST", "/api/routines"),
        ("DELETE", "/api/jobs/whatever"),
    ] {
        let req = match method {
            "POST" => client.post(format!("{}{path}", h.base)).json(&wizard),
            _ => client.delete(format!("{}{path}", h.base)),
        };
        let resp = req
            .header("Origin", "http://evil.example")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN, "{method} {path}");
    }

    // Wrong method on the collection route.
    let resp = client
        .put(format!("{}/api/jobs", h.base))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);

    // Non-loopback Host is refused (DNS-rebinding defense), reads included.
    for path in ["/api/jobs", "/api/jobs/events", "/api/jobs/inbox"] {
        let resp = client
            .get(format!("{}{path}", h.base))
            .header(reqwest::header::HOST, "attacker.example")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN, "GET {path}");
    }

    // The loopback origin passes.
    let resp = client
        .post(format!("{}/api/jobs", h.base))
        .header("Origin", h.base.as_str())
        .json(&wizard)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

// ---------------------------------------------------------------------------
// Routines
// ---------------------------------------------------------------------------

#[tokio::test]
async fn routine_lifecycle_draft_activate_version_archive() {
    let h = jobs_app(mock_llm()).await;

    // Create: always draft, version 1, mode defaults to agent.
    let r = ok_post(
        &h,
        "/api/routines",
        serde_json::json!({"name": "digest", "instructions": "v1 instructions"}),
    )
    .await;
    assert_eq!(r["status"], "draft");
    assert_eq!(r["current_version"], 1);
    let id = r["id"].as_str().unwrap().to_string();
    let listed = ok_get(&h, "/api/routines").await;
    assert!(listed.as_array().unwrap().iter().any(|x| x["id"] == id));

    let detail = ok_get(&h, &format!("/api/routines/{id}")).await;
    assert_eq!(detail["routine"]["id"], id);
    assert_eq!(detail["versions"].as_array().unwrap().len(), 1);
    assert_eq!(detail["versions"][0]["mode"], "agent");
    assert_eq!(detail["versions"][0]["instructions"], "v1 instructions");

    // Activate: draft → active; a second activation is a conflict.
    let ok = ok_post(
        &h,
        &format!("/api/routines/{id}/activate"),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(ok["ok"], true);
    let detail = ok_get(&h, &format!("/api/routines/{id}")).await;
    assert_eq!(detail["routine"]["status"], "active");
    let (status, _) = post(
        &h,
        &format!("/api/routines/{id}/activate"),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);

    // New version bumps current_version; status stays active; versions are
    // newest-first and the pinned v1 is untouched.
    let v2 = ok_post(
        &h,
        &format!("/api/routines/{id}/versions"),
        serde_json::json!({"instructions": "v2 instructions", "mode": "research"}),
    )
    .await;
    assert_eq!(v2["version"], 2);
    assert_eq!(v2["mode"], "research");
    let detail = ok_get(&h, &format!("/api/routines/{id}")).await;
    assert_eq!(detail["routine"]["current_version"], 2);
    assert_eq!(detail["routine"]["status"], "active");
    let versions = detail["versions"].as_array().unwrap();
    assert_eq!(versions.len(), 2);
    assert_eq!(versions[0]["version"], 2);
    assert_eq!(versions[1]["instructions"], "v1 instructions");

    // Archive; archived routines can neither activate nor re-archive.
    ok_post(
        &h,
        &format!("/api/routines/{id}/archive"),
        serde_json::json!({}),
    )
    .await;
    let detail = ok_get(&h, &format!("/api/routines/{id}")).await;
    assert_eq!(detail["routine"]["status"], "archived");
    let (status, _) = post(
        &h,
        &format!("/api/routines/{id}/activate"),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, _) = post(
        &h,
        &format!("/api/routines/{id}/archive"),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);

    // Missing routine: 404 everywhere.
    for path in [
        "/api/routines/missing",
        "/api/routines/missing/activate",
        "/api/routines/missing/archive",
    ] {
        let (status, _) = if path.ends_with("activate") || path.ends_with("archive") {
            post(&h, path, serde_json::json!({})).await
        } else {
            get(&h, path).await
        };
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
    }
    let (status, _) = post(
        &h,
        "/api/routines/missing/versions",
        serde_json::json!({"instructions": "x"}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Actionable 400s: empty name/instructions, unknown mode, missing field.
    for body in [
        serde_json::json!({"name": "", "instructions": "x"}),
        serde_json::json!({"name": "x", "instructions": "  "}),
        serde_json::json!({"name": "x", "instructions": "y", "mode": "wizard"}),
        serde_json::json!({"name": "x"}),
    ] {
        let (status, b) = post(&h, "/api/routines", body.clone()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "body {body}: {b}");
        assert!(b["error"].is_string());
    }
}

// ---------------------------------------------------------------------------
// Job creation: wizard + pin
// ---------------------------------------------------------------------------

#[tokio::test]
async fn job_wizard_creates_active_routine_job_and_conversation() {
    let h = jobs_app(mock_llm()).await;
    let job = ok_post(
        &h,
        "/api/jobs",
        serde_json::json!({
            "name": "nightly",
            "instructions": "summarize the day",
            "schedule": {"cron": "*/5 * * * *"},
        }),
    )
    .await;
    assert_eq!(job["status"], "active");
    assert_eq!(job["routine_version"], 1);
    assert_eq!(job["schedule"]["cron"], "*/5 * * * *");
    assert_eq!(job["schedule"]["tz"], "local");
    assert_eq!(job["schedule"]["missed_policy"], "run_once_if_missed");
    assert_eq!(job["overlap"], "skip");
    assert_eq!(job["budgets"]["max_turns"], 12);
    assert_eq!(job["budgets"]["daily_occurrence_cap"], 24);
    let job_id = job["id"].as_str().unwrap().to_string();
    let routine_id = job["routine_id"].as_str().unwrap().to_string();

    // The wizard is explicit human creation: the routine is ACTIVE too.
    let detail = ok_get(&h, &format!("/api/routines/{routine_id}")).await;
    assert_eq!(detail["routine"]["status"], "active");
    assert_eq!(detail["versions"][0]["mode"], "agent");

    // Dedicated conversation created atomically with the job.
    let conv_id = job["conversation_id"].as_str().unwrap().to_string();
    let conv = h
        .state
        .db
        .get_conversation(conv_id.clone())
        .await
        .unwrap()
        .expect("job conversation exists");
    assert_eq!(conv.title, "Job: nightly");
    assert_eq!(conv.mode, Mode::Agent);

    let listed = ok_get(&h, "/api/jobs").await;
    assert!(listed.as_array().unwrap().iter().any(|j| j["id"] == job_id));
    let detail = ok_get(&h, &format!("/api/jobs/{job_id}")).await;
    assert_eq!(detail["job"]["id"], job_id);
    assert!(detail["occurrences"].as_array().unwrap().is_empty());

    // schedule: null → manual-only job, still active.
    let manual = ok_post(
        &h,
        "/api/jobs",
        serde_json::json!({"name": "manual", "instructions": "on demand", "schedule": null}),
    )
    .await;
    assert_eq!(manual["status"], "active");
    assert!(manual["schedule"].is_null());

    // Explicit mode/model/budgets/overlap/workspace pass through.
    let ws = h.ws().to_string_lossy().into_owned();
    let custom = ok_post(
        &h,
        "/api/jobs",
        serde_json::json!({
            "name": "custom",
            "instructions": "custom work",
            "mode": "research",
            "model": "mock/model",
            "schedule": null,
            "workspace": ws,
            "budgets": {"max_turns": 3, "max_tokens": 1000, "daily_occurrence_cap": 2, "run_timeout_secs": 60},
            "overlap": "queue_one",
        }),
    )
    .await;
    assert_eq!(custom["budgets"]["max_turns"], 3);
    assert_eq!(custom["overlap"], "queue_one");
    assert_eq!(custom["workspace"], ws);
    let rid = custom["routine_id"].as_str().unwrap();
    let detail = ok_get(&h, &format!("/api/routines/{rid}")).await;
    assert_eq!(detail["versions"][0]["mode"], "research");
    assert_eq!(detail["versions"][0]["model"], "mock/model");

    // Invalid schedules → 400 with the clock's actionable message.
    for schedule in [
        serde_json::json!({"cron": "not a cron"}),
        serde_json::json!({"cron": "0 0 9 * * *"}), // 6 fields rejected
        serde_json::json!({"cron": "0 9 * * *", "tz": "Europe/Paris"}),
    ] {
        let (status, b) = post(
            &h,
            "/api/jobs",
            serde_json::json!({"name": "bad", "instructions": "x", "schedule": schedule}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "schedule {schedule}: {b}");
        assert!(b["error"].is_string());
    }

    // Missing schedule key, empty name, neither body shape → 400.
    for body in [
        serde_json::json!({"name": "x", "instructions": "y"}),
        serde_json::json!({"name": "", "instructions": "y", "schedule": null}),
        serde_json::json!({"name": "x"}),
        serde_json::json!({}),
    ] {
        let (status, _) = post(&h, "/api/jobs", body.clone()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "body {body}");
    }

    // Missing job → 404.
    let (status, _) = get(&h, "/api/jobs/missing").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn job_pin_validates_routine_and_version() {
    let h = jobs_app(mock_llm()).await;
    let r = ok_post(
        &h,
        "/api/routines",
        serde_json::json!({"name": "pinnable", "instructions": "v1"}),
    )
    .await;
    let routine_id = r["id"].as_str().unwrap().to_string();

    // Pin to the draft routine's v1: job ACTIVE, routine status untouched.
    let job = ok_post(
        &h,
        "/api/jobs",
        serde_json::json!({
            "name": "pinned",
            "routine_id": routine_id,
            "routine_version": 1,
            "schedule": null,
        }),
    )
    .await;
    assert_eq!(job["status"], "active");
    assert_eq!(job["routine_id"], routine_id);
    assert_eq!(job["routine_version"], 1);
    let detail = ok_get(&h, &format!("/api/routines/{routine_id}")).await;
    assert_eq!(detail["routine"]["status"], "draft");

    // Pin with a schedule persists it.
    let scheduled = ok_post(
        &h,
        "/api/jobs",
        serde_json::json!({
            "name": "pinned-scheduled",
            "routine_id": routine_id,
            "routine_version": 1,
            "schedule": {"cron": "0 9 * * *"},
        }),
    )
    .await;
    assert_eq!(scheduled["schedule"]["cron"], "0 9 * * *");

    // Unknown routine / unknown version → 404.
    let (status, _) = post(
        &h,
        "/api/jobs",
        serde_json::json!({
            "name": "bad", "routine_id": "nope", "routine_version": 1, "schedule": null
        }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = post(
        &h,
        "/api/jobs",
        serde_json::json!({
            "name": "bad", "routine_id": routine_id, "routine_version": 99, "schedule": null
        }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Invalid cron on the pin shape → 400.
    let (status, b) = post(
        &h,
        "/api/jobs",
        serde_json::json!({
            "name": "bad", "routine_id": routine_id, "routine_version": 1,
            "schedule": {"cron": "61 * * * *"}
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(b["error"].as_str().unwrap().contains("cron"));
}

// ---------------------------------------------------------------------------
// Draft activation ordering
// ---------------------------------------------------------------------------

#[tokio::test]
async fn draft_job_activation_activates_the_pinned_routine_first() {
    let h = jobs_app(mock_llm()).await;
    // Draft routine + draft job, seeded directly through the store (the
    // agent-drafted path: propose_schedule lands both as drafts).
    let routine = h
        .state
        .db
        .create_routine(NewRoutine {
            name: "draft-routine".into(),
            description: String::new(),
            instructions: "draft instructions".into(),
            mode: Mode::Agent,
            model: None,
        })
        .await
        .unwrap();
    let job = h
        .state
        .db
        .create_job(NewJob {
            name: "draft-job".into(),
            routine_id: routine.id.clone(),
            routine_version: 1,
            schedule: None,
            overlap: OverlapPolicy::Skip,
            budgets: JobBudgets::default(),
            workspace: Some(h.ws().to_string_lossy().into_owned()),
        })
        .await
        .unwrap();
    assert_eq!(job.status, JobStatus::Draft);

    let ok = ok_post(
        &h,
        &format!("/api/jobs/{}/activate", job.id),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(ok["ok"], true);
    let after = h.state.db.get_job(job.id.clone()).await.unwrap().unwrap();
    assert_eq!(after.status, JobStatus::Active);
    let routine_after = h
        .state
        .db
        .get_routine(routine.id.clone())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(routine_after.status, vortex_types::RoutineStatus::Active);

    // Second activation → 409; missing → 404; non-draft (paused) → 409.
    let (status, _) = post(
        &h,
        &format!("/api/jobs/{}/activate", job.id),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, _) = post(&h, "/api/jobs/missing/activate", serde_json::json!({})).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    h.state
        .db
        .set_job_status(job.id.clone(), JobStatus::Paused)
        .await
        .unwrap();
    let (status, _) = post(
        &h,
        &format!("/api/jobs/{}/activate", job.id),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
}

// ---------------------------------------------------------------------------
// pause / resume / steer / patch / delete / occurrences
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pause_resume_steer_patch_delete_roundtrips() {
    let h = jobs_app(mock_llm()).await;
    let job = ok_post(
        &h,
        "/api/jobs",
        serde_json::json!({
            "name": "lifecycle", "instructions": "work", "schedule": {"cron": "*/5 * * * *"}
        }),
    )
    .await;
    let id = job["id"].as_str().unwrap().to_string();
    let conv_id = job["conversation_id"].as_str().unwrap().to_string();

    // pause: active → paused; anything else → 409.
    ok_post(&h, &format!("/api/jobs/{id}/pause"), serde_json::json!({})).await;
    let detail = ok_get(&h, &format!("/api/jobs/{id}")).await;
    assert_eq!(detail["job"]["status"], "paused");
    let (status, _) = post(&h, &format!("/api/jobs/{id}/pause"), serde_json::json!({})).await;
    assert_eq!(status, StatusCode::CONFLICT);

    // resume resets the consecutive-failure counter.
    h.state.db.record_job_failure(id.clone()).await.unwrap();
    h.state.db.record_job_failure(id.clone()).await.unwrap();
    ok_post(&h, &format!("/api/jobs/{id}/resume"), serde_json::json!({})).await;
    let detail = ok_get(&h, &format!("/api/jobs/{id}")).await;
    assert_eq!(detail["job"]["status"], "active");
    assert_eq!(detail["job"]["consecutive_failures"], 0);
    let (status, _) = post(&h, &format!("/api/jobs/{id}/resume"), serde_json::json!({})).await;
    assert_eq!(status, StatusCode::CONFLICT);

    // circuit_broken resumes too (human re-enable after the breaker trips).
    h.state
        .db
        .set_job_status(id.clone(), JobStatus::CircuitBroken)
        .await
        .unwrap();
    for _ in 0..3 {
        h.state.db.record_job_failure(id.clone()).await.unwrap();
    }
    ok_post(&h, &format!("/api/jobs/{id}/resume"), serde_json::json!({})).await;
    let detail = ok_get(&h, &format!("/api/jobs/{id}")).await;
    assert_eq!(detail["job"]["status"], "active");
    assert_eq!(detail["job"]["consecutive_failures"], 0);

    // steer: set, clear, and a missing note key is a 400.
    ok_post(
        &h,
        &format!("/api/jobs/{id}/steer"),
        serde_json::json!({"note": "focus on X"}),
    )
    .await;
    let detail = ok_get(&h, &format!("/api/jobs/{id}")).await;
    assert_eq!(detail["job"]["steer_note"], "focus on X");
    ok_post(
        &h,
        &format!("/api/jobs/{id}/steer"),
        serde_json::json!({"note": null}),
    )
    .await;
    let detail = ok_get(&h, &format!("/api/jobs/{id}")).await;
    assert!(detail["job"]["steer_note"].is_null());
    let (status, _) = post(&h, &format!("/api/jobs/{id}/steer"), serde_json::json!({})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = post(
        &h,
        "/api/jobs/missing/steer",
        serde_json::json!({"note": null}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // PATCH: partial merge — budgets keep untouched caps, name/overlap apply.
    let (status, updated) = patch(
        &h,
        &format!("/api/jobs/{id}"),
        serde_json::json!({"name": "renamed", "budgets": {"max_turns": 4}, "overlap": "queue_one"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(updated["name"], "renamed");
    assert_eq!(updated["budgets"]["max_turns"], 4);
    assert_eq!(
        updated["budgets"]["max_tokens"], 120_000,
        "untouched budget caps must keep their current values"
    );
    assert_eq!(updated["overlap"], "queue_one");

    // PATCH schedule: replace, clear with null, invalid cron → 400.
    let (_, updated) = patch(
        &h,
        &format!("/api/jobs/{id}"),
        serde_json::json!({"schedule": {"cron": "0 8 * * *"}}),
    )
    .await;
    assert_eq!(updated["schedule"]["cron"], "0 8 * * *");
    let (_, updated) = patch(
        &h,
        &format!("/api/jobs/{id}"),
        serde_json::json!({"schedule": null}),
    )
    .await;
    assert!(updated["schedule"].is_null());
    let (status, _) = patch(
        &h,
        &format!("/api/jobs/{id}"),
        serde_json::json!({"schedule": {"cron": "bad"}}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = patch(
        &h,
        &format!("/api/jobs/{id}"),
        serde_json::json!({"name": ""}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = patch(&h, "/api/jobs/missing", serde_json::json!({"name": "x"})).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Occurrences listing: default, limit, cap 200, actionable 400s.
    let base = chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00.000Z").unwrap();
    for i in 0..201 {
        let t = (base + chrono::Duration::minutes(i))
            .with_timezone(&chrono::Utc)
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        h.state
            .db
            .insert_occurrence(id.clone(), t)
            .await
            .unwrap()
            .unwrap();
    }
    let occs = ok_get(&h, &format!("/api/jobs/{id}/occurrences")).await;
    assert_eq!(occs.as_array().unwrap().len(), 50, "default limit is 50");
    let occs = ok_get(&h, &format!("/api/jobs/{id}/occurrences?limit=1")).await;
    assert_eq!(occs.as_array().unwrap().len(), 1);
    let occs = ok_get(&h, &format!("/api/jobs/{id}/occurrences?limit=500")).await;
    assert_eq!(
        occs.as_array().unwrap().len(),
        200,
        "limit is capped at 200"
    );
    for q in ["limit=0", "limit=abc"] {
        let (status, _) = get(&h, &format!("/api/jobs/{id}/occurrences?{q}")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{q}");
    }
    let (status, _) = get(&h, "/api/jobs/missing/occurrences").await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // DELETE cascades the dedicated conversation; second delete → 404.
    let ok = delete(&h, &format!("/api/jobs/{id}")).await;
    assert_eq!(ok.0, StatusCode::OK);
    assert_eq!(ok.1["ok"], true);
    let (status, _) = get(&h, &format!("/api/jobs/{id}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(h
        .state
        .db
        .get_conversation(conv_id)
        .await
        .unwrap()
        .is_none());
    let (status, _) = delete(&h, &format!("/api/jobs/{id}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// Manual run
// ---------------------------------------------------------------------------

#[tokio::test]
async fn manual_run_is_admitted_by_the_supervisor_and_succeeds() {
    let h = jobs_app(mock_llm()).await;
    let job = seed_active_job(&h, "manual", None).await;

    let (status, occ) = post(
        &h,
        &format!("/api/jobs/{}/run", job.id),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(occ["status"], "scheduled");
    let scheduled_for = occ["scheduled_for"].as_str().unwrap();
    assert!(
        scheduled_for.ends_with('Z'),
        "UTC RFC 3339 millis: {scheduled_for}"
    );
    assert!(
        scheduled_for.contains('.'),
        "millis precision: {scheduled_for}"
    );
    let occ_id = occ["id"].as_str().unwrap().to_string();

    // The due-query admits the manual occurrence on the next tick.
    h.state.supervisor.tick_once().await;
    let done = wait_occ(&h, &occ_id, OccurrenceStatus::Succeeded).await;
    let run_id = done.run_id.expect("run attached");
    let run = h.state.db.get_run(run_id).await.unwrap().unwrap();
    assert_eq!(run.status, RunStatus::Completed);

    let detail = ok_get(&h, &format!("/api/jobs/{}", job.id)).await;
    assert_eq!(detail["occurrences"][0]["status"], "succeeded");

    // A second manual run at a new instant creates a new occurrence.
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    let (status, occ2) = post(
        &h,
        &format!("/api/jobs/{}/run", job.id),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_ne!(occ2["id"], occ["id"]);

    // Non-active jobs refuse manual runs; missing jobs 404.
    h.state
        .db
        .set_job_status(job.id.clone(), JobStatus::Paused)
        .await
        .unwrap();
    let (status, b) = post(
        &h,
        &format!("/api/jobs/{}/run", job.id),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(b["error"].as_str().unwrap().contains("active"));
    let (status, _) = post(&h, "/api/jobs/missing/run", serde_json::json!({})).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn manual_run_double_fire_guard_returns_409() {
    let h = jobs_app(mock_llm()).await;
    let job = seed_active_job(&h, "conflict", None).await;
    // Pre-insert the next 1000 ms of instants so the handler's `now` is
    // guaranteed to collide with UNIQUE(job_id, scheduled_for) regardless of
    // request timing (the loopback round-trip is far below 1 s).
    let start = chrono::DateTime::parse_from_rfc3339(&vortex_core::db::now()).unwrap();
    for i in 0..1000 {
        let t = (start + chrono::Duration::milliseconds(i))
            .with_timezone(&chrono::Utc)
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        assert!(h
            .state
            .db
            .insert_occurrence(job.id.clone(), t)
            .await
            .unwrap()
            .is_some());
    }
    let (status, b) = post(
        &h,
        &format!("/api/jobs/{}/run", job.id),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(b["error"], "already fired for this instant");
}

// ---------------------------------------------------------------------------
// Inbox
// ---------------------------------------------------------------------------

#[tokio::test]
async fn inbox_lists_parked_approvals_and_existing_decide_resolves() {
    let h = jobs_app(mock_llm()).await;
    let job = seed_active_job(&h, "parked", None).await;
    let occ = h
        .state
        .db
        .insert_occurrence(job.id.clone(), vortex_core::db::now())
        .await
        .unwrap()
        .unwrap();
    h.state
        .db
        .update_occurrence(
            occ.id.clone(),
            OccurrenceStatus::WaitingApproval,
            Some("run-job".into()),
            None,
        )
        .await
        .unwrap();

    // Register a real parked approval through the gate: it persists the row
    // AND the in-memory waiter that POST /api/approvals/{id}/decide delivers
    // to (a DB-only seed could not be decided through the existing endpoint).
    let approvals = h.state.engine.approvals.clone();
    let waiter = tokio::spawn(async move {
        use vortex_agents::registry::ApprovalGate;
        approvals
            .request(
                "run-job",
                "Delete file?",
                "{}",
                vortex_types::ApprovalPolicy::ParkUntilDecided,
            )
            .await
    });

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    let parked = loop {
        let inbox = ok_get(&h, "/api/jobs/inbox").await;
        if let Some(arr) = inbox.as_array() {
            if !arr.is_empty() {
                break arr[0].clone();
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "parked approval never appeared in the inbox"
        );
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    };
    assert_eq!(parked["job_id"], job.id);
    assert_eq!(parked["job_name"], "parked");
    assert_eq!(parked["occurrence_id"], occ.id);
    assert_eq!(parked["approval"]["title"], "Delete file?");
    assert_eq!(parked["approval"]["status"], "pending");
    let approval_id = parked["approval"]["id"].as_str().unwrap().to_string();

    // Decide through the EXISTING endpoint (no duplicate route).
    let (status, b) = post(
        &h,
        &format!("/api/approvals/{approval_id}/decide"),
        serde_json::json!({"approve": true}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(b["ok"], true);
    assert!(
        waiter.await.unwrap().unwrap(),
        "decision delivered to the gate"
    );
    let inbox = ok_get(&h, "/api/jobs/inbox").await;
    assert!(inbox.as_array().unwrap().is_empty());

    // Already resolved → 404 from the existing endpoint.
    let (status, _) = post(
        &h,
        &format!("/api/approvals/{approval_id}/decide"),
        serde_json::json!({"approve": true}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// resolve-review
// ---------------------------------------------------------------------------

#[tokio::test]
async fn resolve_review_continue_clears_reason_and_spawns_continuation() {
    let h = jobs_app(Arc::new(MockLlm::new(vec![MockTurn::text(
        "continued work",
    )])))
    .await;
    let job = seed_active_job(&h, "review", None).await;

    // Prior interrupted run with a plan + transcript; the occurrence carries
    // the review flag exactly as the supervisor's catch-up leaves it.
    h.state
        .db
        .upsert_run(&prior_run(
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
    h.state
        .db
        .add_message(
            job.conversation_id.clone(),
            ChatMessage::user("start the job"),
        )
        .await
        .unwrap();
    h.state
        .db
        .add_message(
            job.conversation_id.clone(),
            ChatMessage::assistant("working on step one"),
        )
        .await
        .unwrap();
    let occ = h
        .state
        .db
        .insert_occurrence(job.id.clone(), vortex_core::db::now())
        .await
        .unwrap()
        .unwrap();
    h.state
        .db
        .update_occurrence(
            occ.id.clone(),
            OccurrenceStatus::Interrupted,
            Some("prior-run-1".into()),
            None,
        )
        .await
        .unwrap();
    h.state
        .db
        .flag_occurrence_review(
            occ.id.clone(),
            "unconfirmed consequential step 'write_file' (tc-1) needs human review".into(),
        )
        .await
        .unwrap();

    // 404 missing; 409 unflagged; 400 unknown action.
    let (status, _) = post(
        &h,
        "/api/occurrences/missing/resolve-review",
        serde_json::json!({"action": "continue"}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let plain = h
        .state
        .db
        .insert_occurrence(job.id.clone(), "2020-01-01T00:00:00.000Z".into())
        .await
        .unwrap()
        .unwrap();
    let (status, _) = post(
        &h,
        &format!("/api/occurrences/{}/resolve-review", plain.id),
        serde_json::json!({"action": "continue"}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, _) = post(
        &h,
        &format!("/api/occurrences/{}/resolve-review", occ.id),
        serde_json::json!({"action": "explode"}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Continue: reason cleared (catch_up re-processes the reason-less
    // Interrupted row) and the continuation is admitted promptly.
    let (status, b) = post(
        &h,
        &format!("/api/occurrences/{}/resolve-review", occ.id),
        serde_json::json!({"action": "continue"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(b["ok"], true);

    let done = wait_occ(&h, &occ.id, OccurrenceStatus::Succeeded).await;
    let new_run = done.run_id.expect("continuation attaches a new run");
    assert_ne!(new_run, "prior-run-1", "a NEW run continues the work");
    assert!(!done.needs_human_review, "resolved review no longer blocks");
    assert!(done.reason.is_none());
    let runs = h
        .state
        .db
        .list_runs_for_conversation(job.conversation_id.clone())
        .await
        .unwrap();
    assert_eq!(runs.len(), 2, "prior run + continuation");
    let run = h.state.db.get_run(new_run).await.unwrap().unwrap();
    assert_eq!(run.status, RunStatus::Completed);

    // Re-resolution is a conflict now that the flag is cleared.
    let (status, _) = post(
        &h,
        &format!("/api/occurrences/{}/resolve-review", occ.id),
        serde_json::json!({"action": "continue"}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn resolve_review_dismiss_fails_without_breaker_increment() {
    let h = jobs_app(mock_llm()).await;
    let job = seed_active_job(&h, "dismiss", None).await;
    let occ = h
        .state
        .db
        .insert_occurrence(job.id.clone(), vortex_core::db::now())
        .await
        .unwrap()
        .unwrap();
    h.state
        .db
        .update_occurrence(
            occ.id.clone(),
            OccurrenceStatus::Interrupted,
            Some("prior-run-x".into()),
            None,
        )
        .await
        .unwrap();
    h.state
        .db
        .flag_occurrence_review(occ.id.clone(), "needs review".into())
        .await
        .unwrap();

    let (status, b) = post(
        &h,
        &format!("/api/occurrences/{}/resolve-review", occ.id),
        serde_json::json!({"action": "dismiss"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(b["ok"], true);
    let got = h
        .state
        .db
        .get_occurrence(occ.id.clone())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(got.status, OccurrenceStatus::Failed);
    assert_eq!(got.reason.as_deref(), Some("dismissed by human"));
    assert!(!got.needs_human_review);
    let after = h.state.db.get_job(job.id.clone()).await.unwrap().unwrap();
    assert_eq!(
        after.consecutive_failures, 0,
        "dismissal never trips the breaker"
    );
    assert_eq!(after.status, JobStatus::Active);

    let (status, _) = post(
        &h,
        &format!("/api/occurrences/{}/resolve-review", occ.id),
        serde_json::json!({"action": "dismiss"}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
}

// ---------------------------------------------------------------------------
// SSE: replay, live tail, cursors, gap
// ---------------------------------------------------------------------------

#[tokio::test]
async fn job_events_replay_live_tail_cursors_and_gap() {
    let h = jobs_app(mock_llm()).await;
    let job = seed_active_job(&h, "sse", Some(spec("*/5 * * * *"))).await;
    let url = format!("{}/api/jobs/events", h.base);

    // The first tick seeds next_run_at → one JobUpdated (seq 1).
    h.state.supervisor.tick_once().await;
    let buf = read_sse_until(&format!("{url}?since=0"), &[], |b| !sse_ids(b).is_empty()).await;
    assert_eq!(sse_ids(&buf), vec![1]);
    assert!(
        buf.contains("\"type\":\"job_updated\""),
        "missing event data: {buf}"
    );
    assert!(buf.contains(&job.id));

    // since=1: nothing replayed; a live event arrives exactly once. The
    // publisher clears next_run_at so the tick recomputes it (→ JobUpdated).
    let publisher = spawn_next_run_publisher(&h, &job.id, None);
    let buf = read_sse_until(&format!("{url}?since=1"), &[], |b| !sse_ids(b).is_empty()).await;
    publisher.await.unwrap();
    assert_eq!(sse_ids(&buf), vec![2]);

    // Last-Event-ID header (browser auto-reconnect).
    let publisher = spawn_next_run_publisher(&h, &job.id, None);
    let buf = read_sse_until(&url, &[("Last-Event-ID", "2")], |b| !sse_ids(b).is_empty()).await;
    publisher.await.unwrap();
    assert_eq!(sse_ids(&buf), vec![3]);

    // Header + query together: the newer (larger) cursor wins. Header 3 beats
    // query 1, so nothing is replayed (newest is 3) and only the live event
    // arrives; had the smaller cursor won, seqs 2 and 3 would replay first.
    let publisher = spawn_next_run_publisher(&h, &job.id, None);
    let buf = read_sse_until(&format!("{url}?since=1"), &[("Last-Event-ID", "3")], |b| {
        !sse_ids(b).is_empty()
    })
    .await;
    publisher.await.unwrap();
    assert_eq!(sse_ids(&buf), vec![4]);

    // Overflow the retained log (supervisor MAX_JOB_LOG = 1024): a paused
    // job's next_run_at maintenance emits one JobUpdated per tick without
    // firing anything. 1100 events guarantee eviction.
    h.state
        .db
        .set_job_status(job.id.clone(), JobStatus::Paused)
        .await
        .unwrap();
    for _ in 0..1100 {
        h.state
            .db
            .set_job_next_run(job.id.clone(), Some("2020-01-01T00:00:00.000Z".into()))
            .await
            .unwrap();
        h.state.supervisor.tick_once().await;
    }
    let (_, oldest, newest) = h.state.supervisor.replay_from(0);
    let oldest = oldest.expect("log non-empty");
    let newest = newest.expect("log non-empty");
    assert!(oldest > 1, "log must have overflowed its retained window");
    let want = (newest - oldest + 1) as usize;
    let buf = read_sse_until(&format!("{url}?since=1"), &[], |b| sse_ids(b).len() >= want).await;
    assert!(buf.contains("replay_gap"), "missing gap signal: {buf}");
    let gap_pos = buf.find("replay_gap").unwrap();
    assert!(
        buf.find("id:").map(|p| p > gap_pos).unwrap_or(false),
        "the gap signal must precede the replayed events"
    );
    assert!(
        buf.contains(&format!("data:{oldest}")) || buf.contains(&format!("data: {oldest}")),
        "gap event must carry the oldest retained seq ({oldest}): {buf}"
    );
    assert_eq!(sse_ids(&buf), (oldest..=newest).collect::<Vec<_>>());

    // A future cursor is not a gap: nothing replayed, live tail still flows.
    let publisher = spawn_next_run_publisher(&h, &job.id, Some("2020-01-01T00:00:00.000Z"));
    let buf = read_sse_until(&format!("{url}?since=999999"), &[], |b| {
        !sse_ids(b).is_empty()
    })
    .await;
    publisher.await.unwrap();
    assert!(
        !buf.contains("replay_gap"),
        "future cursor is not a gap: {buf}"
    );
    assert_eq!(sse_ids(&buf), vec![newest + 1]);
}

/// Clear/seed `next_run_at` shortly after the SSE connection opens, then tick
/// so the supervisor recomputes it and emits exactly one JobUpdated.
fn spawn_next_run_publisher(
    h: &Harness,
    job_id: &str,
    seed: Option<&str>,
) -> tokio::task::JoinHandle<()> {
    let db = h.state.db.clone();
    let sup = h.state.supervisor.clone();
    let job_id = job_id.to_string();
    let seed = seed.map(str::to_string);
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        db.set_job_next_run(job_id, seed).await.unwrap();
        sup.tick_once().await;
    })
}

// ---------------------------------------------------------------------------
// Supervisor lifecycle
// ---------------------------------------------------------------------------

#[tokio::test]
async fn supervisor_run_loop_exits_promptly_on_cancel() {
    let h = jobs_app(mock_llm()).await;
    // AppState holds ONE supervisor instance (main.rs spawns run() exactly
    // once on this same Arc); the harness shares it.
    let sup = h.state.supervisor.clone();
    assert!(Arc::ptr_eq(&sup, &h.state.supervisor));
    let handle = tokio::spawn(async move { sup.run().await });
    // Let the startup catch-up finish and the loop enter its first sleep.
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    h.cancel.cancel();
    tokio::time::timeout(std::time::Duration::from_secs(5), handle)
        .await
        .expect("run loop must exit promptly on cancel (no orphan ticks)")
        .unwrap();
}
