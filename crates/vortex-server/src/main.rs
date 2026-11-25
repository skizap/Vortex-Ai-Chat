//! Vortex-Ai-Chat server binary: a localhost-first web application.
//!
//! Startup order: paths/config → database (+ interrupted-run marking) →
//! engine (tools, scheduler, approvals) → HTTP server with graceful shutdown.

mod diagnostics;
mod guard;
mod mock_llm;
mod routes_attachments;
mod routes_ckpts;
mod routes_conv;
mod routes_jobs;
mod routes_misc;
mod routes_runs;
mod routes_usage;
mod state;
mod static_files;

#[cfg(test)]
mod job_hardening_tests;
#[cfg(test)]
mod job_routes_tests;
#[cfg(test)]
mod search_probe_tests;

use std::sync::Arc;
use vortex_core::{AppConfig, Db, Paths};
use vortex_llm::LlmClient;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Stage 1: paths + config. `AppConfig::load` loads `.env` first, so the
    // diagnostics gate (env-only, never persisted) is authoritative after it.
    let paths = Paths::resolve().map_err(|e| diagnostics::stage_fatal("paths", e.into()))?;
    let config =
        AppConfig::load(&paths).map_err(|e| diagnostics::stage_fatal("config", e.into()))?;

    // Initialize logging. Normal mode: unchanged compact terminal output.
    // Diagnostics mode (`VORTEX_DIAGNOSTICS=1`): timestamped, levelled,
    // target-labelled, no ANSI escapes, targeted filter, panic hook.
    let diag = diagnostics::enabled();
    diagnostics::init_tracing(diag, &config.log_level);
    if diag {
        diagnostics::install_panic_hook();
        tracing::info!(
            target: diagnostics::HTTP_TARGET,
            diagnostics = true,
            "diagnostics session enabled (VORTEX_DIAGNOSTICS=1, env-only)"
        );
    }

    tracing::info!("Vortex-Ai-Chat {} starting", env!("CARGO_PKG_VERSION"));

    // Database + honest interrupted-run marking (and expiry of the pending
    // approvals that belonged to those runs).
    let db =
        Db::open(paths.database()).map_err(|e| diagnostics::stage_fatal("database", e.into()))?;
    {
        let conn = db.raw_connection();
        let (runs, approvals) = vortex_core::db::mark_interrupted_on_startup(&conn)
            .map_err(|e| diagnostics::stage_fatal("database", e.into()))?;
        if runs > 0 {
            tracing::warn!(
                "marked {runs} run(s) as interrupted from a previous session \
                 ({approvals} pending approval(s) expired); \
                 consequential actions are never replayed automatically"
            );
        }
    }
    // Honest interrupted-occurrence marking (mirrors the runs sweep): live
    // job occurrences become `interrupted`; the supervisor's catch-up then
    // decides continuation per policy — never a mid-loop replay.
    let interrupted_occurrences = db
        .mark_occurrences_interrupted_on_startup()
        .await
        .map_err(|e| diagnostics::stage_fatal("database", e.into()))?;
    if interrupted_occurrences > 0 {
        tracing::warn!(
            "marked {interrupted_occurrences} job occurrence(s) as interrupted from a previous \
             session; the supervisor will review them for safe continuation — consequential \
             actions are never replayed automatically"
        );
    }

    // Shipped draft routine templates: seeded once, only into an empty
    // routines table (never overwritten, never activated). A seeding failure
    // is logged, not fatal — templates are a convenience, not a dependency.
    match vortex_core::job_seeds::seed_draft_routines(&db).await {
        Ok(0) => {}
        Ok(n) => tracing::info!(
            "seeded {n} draft routine template(s); activate them in the Jobs tab to use them"
        ),
        Err(e) => tracing::warn!(
            error = %e,
            "could not seed draft routine templates; continuing startup without them"
        ),
    }

    // Settings (DB) drive the engine; config drives boot-level choices.
    let settings = match db.load_settings().await {
        Ok(s) => s,
        Err(e) => return Err(diagnostics::stage_fatal("settings", e.into())),
    };

    // Attachment storage (uploads + generated media). Registers its root with
    // the Db so conversation deletion cleans up files after the row cascade.
    let attachments = Arc::new(
        vortex_core::AttachmentStore::new(paths.attachments_dir(), db.clone())
            .map_err(|e| diagnostics::stage_fatal("attachments", e.into()))?,
    );

    // LLM provider: real OpenRouter or the offline mock (explicit opt-in).
    // The base URL is configuration-derived and could carry credential-
    // bearing components: in diagnostics mode it is omitted from the log;
    // normal mode keeps the previous behavior exactly.
    let llm: Arc<dyn LlmClient> = if config.is_mock_llm() {
        tracing::info!("LLM provider: built-in mock (offline testing/demo mode)");
        Arc::new(mock_llm::DemoMockLlm::new(&config.llm.mock_script))
    } else {
        match config.api_key() {
            Some(_key) => {
                if diag {
                    tracing::info!("LLM provider: OpenRouter (base URL omitted from logs)");
                } else {
                    tracing::info!("LLM provider: OpenRouter ({})", config.llm.base_url);
                }
            }
            None => {
                // Start anyway so the UI can show setup guidance, but be loud.
                tracing::warn!(
                    "OPENROUTER_API_KEY is not set; chat will fail until it is configured \
                     (see README.md 'Setup')"
                );
            }
        }
        Arc::new(
            vortex_llm::OpenRouterClient::new(&config.llm.base_url, config.api_key())
                .map_err(|e| diagnostics::stage_fatal("provider", e.into()))?,
        )
    };

    let hub = Arc::new(vortex_agents::Hub::new());
    let scheduler = Arc::new(vortex_agents::Scheduler::new(
        settings.agents.max_concurrent,
    ));
    let browser = Arc::new(vortex_agents::browser::BrowserManager::new(
        paths.browser_profile(),
        true, // headless by default
    ));
    let previews = Arc::new(vortex_agents::PreviewRegistry::new());

    // Search provider: SearXNG when configured, mock when the LLM is mocked.
    // Same selection policy as settings saves (state::build_search_provider),
    // so a URL change applies to subsequent runs without a restart.
    let search = state::build_search_provider(&settings, config.is_mock_llm());
    if search.is_none() {
        tracing::info!("no search provider configured; research tools will say so honestly");
    }

    let approvals = Arc::new(vortex_agents::approvals::ApprovalManager::new(
        db.clone(),
        hub.clone(),
        settings.agents.approval_timeout_secs,
    ));
    let engine_slot = Arc::new(std::sync::OnceLock::new());
    let registry = vortex_agents::build_registry(previews.clone(), engine_slot.clone());
    let http = vortex_agents::safe_http::SafeHttp::new(false); // model URLs never touch local nets
    let engine = vortex_agents::Engine::new(
        llm,
        registry,
        hub.clone(),
        scheduler.clone(),
        approvals.clone(),
        db.clone(),
        browser.clone(),
        previews.clone(),
        search.clone(),
        http,
        settings.clone(),
        Some(attachments.clone()),
    );
    let _ = engine_slot.set(engine.clone());

    // Job supervisor: created and started EXACTLY ONCE. Its run loop performs
    // the startup catch-up, then ticks every second until cancelled; the job
    // gate (2 concurrent job roots) is independent of the sub-agent scheduler.
    let supervisor_cancel = tokio_util::sync::CancellationToken::new();
    let supervisor = vortex_agents::supervisor::Supervisor::new(
        db.clone(),
        engine.clone(),
        hub.clone(),
        Arc::new(vortex_agents::clock::SystemClock),
        2,
        supervisor_cancel.clone(),
    );
    tokio::spawn(supervisor.clone().run());

    let is_mock_llm = config.is_mock_llm();
    let config =
        handle_lan_token(config, &paths).map_err(|e| diagnostics::stage_fatal("lan_token", e))?;
    let guard_settings = guard::GuardSettings {
        lan_enabled: config.server.lan.enabled,
        lan_token: Arc::from(config.server.lan.token.as_str()),
    };
    let state = Arc::new(state::AppState {
        db: db.clone(),
        engine: engine.clone(),
        hub: hub.clone(),
        config,
        previews: previews.clone(),
        attachments: attachments.clone(),
        is_mock_llm,
        version: env!("CARGO_PKG_VERSION"),
        guard: guard_settings,
        diagnostics: diagnostics::DiagnosticsState::new(diag),
        supervisor: supervisor.clone(),
    });

    let addr = format!("{}:{}", state.config.server.host, state.config.server.port);
    let app = build_router(state.clone());
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .map_err(|e| diagnostics::stage_fatal("bind", e.into()))?;
    tracing::info!("listening on http://{addr} (browser UI + API)");

    let hub_for_shutdown = hub.clone();
    let previews_for_shutdown = previews.clone();
    let browser_for_shutdown = browser.clone();
    let supervisor_cancel_for_shutdown = supervisor_cancel.clone();
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
            tracing::info!(
                "shutting down: stopping job supervisor, cancelling runs, stopping previews, \
                 closing browser"
            );
            // Stop new job admissions FIRST (the tick loop exits on cancel;
            // no orphan ticks), then cancel live runs and tear down the rest.
            supervisor_cancel_for_shutdown.cancel();
            hub_for_shutdown.cancel_all();
            previews_for_shutdown.stop_all();
            browser_for_shutdown.shutdown().await;
        })
        .await
        .map_err(|e| diagnostics::stage_fatal("serve", e.into()))?;
    Ok(())
}

fn build_router(state: Arc<state::AppState>) -> axum::Router {
    use axum::routing::{get, post};
    axum::Router::new()
        // UI
        .route("/", get(static_files::index))
        .route("/{*path}", get(static_files::asset))
        // API
        .route("/api/health", get(routes_misc::health))
        .route(
            "/api/settings",
            get(routes_misc::get_settings).put(routes_misc::put_settings),
        )
        // Settings "Test search": guarded POST, no caller-provided URL or
        // query — the fixed benign probe runs server-side against the saved
        // provider. No polling; nothing is cached between calls.
        .route("/api/settings/search/test", post(routes_misc::test_search))
        .route("/api/models", get(routes_misc::list_models))
        .route("/api/approvals", get(routes_misc::list_approvals))
        .route(
            "/api/approvals/{id}/decide",
            post(routes_misc::decide_approval),
        )
        .route("/api/workspace/check", get(routes_misc::check_workspace))
        .route("/api/previews", get(routes_misc::list_previews))
        .route(
            "/api/conversations",
            get(routes_conv::list_conversations).post(routes_conv::create_conversation),
        )
        .route(
            "/api/conversations/{id}",
            axum::routing::patch(routes_conv::rename_conversation)
                .delete(routes_conv::delete_conversation),
        )
        .route(
            "/api/conversations/{id}/messages",
            get(routes_conv::get_messages),
        )
        .route("/api/conversations/{id}/chat", post(routes_conv::post_chat))
        .route(
            "/api/conversations/{id}/model",
            get(routes_conv::get_model).patch(routes_conv::set_model),
        )
        .route(
            "/api/conversations/{id}/usage",
            get(routes_usage::conversation_usage),
        )
        .route(
            "/api/conversations/{id}/attachments",
            get(routes_attachments::list_attachments).post(
                post(routes_attachments::upload_attachments)
                    .layer(routes_attachments::upload_body_limit()),
            ),
        )
        .route(
            "/api/attachments/{id}",
            get(routes_attachments::get_attachment_bytes)
                .delete(routes_attachments::delete_attachment),
        )
        .route(
            "/api/attachments/{id}/meta",
            get(routes_attachments::get_attachment_meta),
        )
        .route(
            "/api/conversations/{id}/checkpoints",
            get(routes_ckpts::list_checkpoints),
        )
        .route(
            "/api/checkpoints/{id}/restore",
            post(routes_ckpts::restore_checkpoint),
        )
        .route("/api/runs", get(routes_runs::list_runs))
        .route("/api/runs/{id}/events", get(routes_runs::run_events))
        .route("/api/runs/{id}/state", get(routes_runs::run_state))
        .route("/api/runs/{id}/stop", post(routes_runs::stop_run))
        .route("/api/runs/{id}/retry", post(routes_runs::retry_run))
        // Jobs & routines (SA6): all inside the guard layer. Static segments
        // (events/inbox) take precedence over the {id} capture in axum 0.8.
        .route(
            "/api/routines",
            get(routes_jobs::list_routines).post(routes_jobs::create_routine),
        )
        .route("/api/routines/{id}", get(routes_jobs::get_routine))
        .route(
            "/api/routines/{id}/activate",
            post(routes_jobs::activate_routine),
        )
        .route(
            "/api/routines/{id}/archive",
            post(routes_jobs::archive_routine),
        )
        .route(
            "/api/routines/{id}/versions",
            post(routes_jobs::add_routine_version),
        )
        .route(
            "/api/jobs",
            get(routes_jobs::list_jobs).post(routes_jobs::create_job),
        )
        .route("/api/jobs/events", get(routes_jobs::job_events))
        .route("/api/jobs/inbox", get(routes_jobs::job_inbox))
        .route(
            "/api/jobs/{id}",
            get(routes_jobs::get_job)
                .patch(routes_jobs::patch_job)
                .delete(routes_jobs::delete_job),
        )
        .route("/api/jobs/{id}/activate", post(routes_jobs::activate_job))
        .route("/api/jobs/{id}/pause", post(routes_jobs::pause_job))
        .route("/api/jobs/{id}/resume", post(routes_jobs::resume_job))
        .route("/api/jobs/{id}/run", post(routes_jobs::run_job))
        .route("/api/jobs/{id}/steer", post(routes_jobs::steer_job))
        .route(
            "/api/jobs/{id}/occurrences",
            get(routes_jobs::list_job_occurrences),
        )
        .route(
            "/api/occurrences/{id}/resolve-review",
            post(routes_jobs::resolve_review),
        )
        // The 8 KiB report cap is enforced inside the handler (gate and
        // content-length/stream checks run before any body extraction), so
        // it holds on this router regardless of layering.
        .route("/api/diagnostics", post(diagnostics::report))
        .layer(axum::middleware::from_fn_with_state(
            state.guard.clone(),
            guard::guard,
        ))
        // Outside the guard: correlation sees guard denials and tags them
        // with the guard's fixed denial category.
        .layer(axum::middleware::from_fn(diagnostics::correlation))
        .layer(axum::extract::DefaultBodyLimit::max(2 * 1024 * 1024))
        .with_state(state)
}

/// If LAN mode is enabled but no token exists, generate one, persist it in the
/// config file (0600) and print a loud warning. The file is *patched*, not
/// regenerated: user comments and unknown keys survive.
fn handle_lan_token(mut config: AppConfig, paths: &Paths) -> anyhow::Result<AppConfig> {
    if config.server.lan.enabled && config.server.lan.token.is_empty() {
        use rand::RngCore;
        let mut bytes = [0u8; 32];
        rand::rng().fill_bytes(&mut bytes);
        config.server.lan.token = hex(&bytes);
        let file = paths.config_file();
        let existing = std::fs::read_to_string(&file).unwrap_or_default();
        let patched = vortex_core::config::patch_lan_token(&existing, &config.server.lan.token);
        std::fs::write(&file, patched)?;
        vortex_core::config::ensure_private_config_perms(&file);
        tracing::warn!(
            "LAN ACCESS ENABLED: this server is exposed to your local network. \
             Every API request must now carry 'Authorization: Bearer <token>' \
             (or ?token=... for SSE). The token was generated and stored in {} \
             — copy it from there to connect from another device.",
            file.display()
        );
    }
    Ok(config)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c().await.ok();
    };
    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}
