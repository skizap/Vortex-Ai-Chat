//! Vortex-Ai-Chat server binary: a localhost-first web application.
//!
//! Startup order: paths/config → database (+ interrupted-run marking) →
//! engine (tools, scheduler, approvals) → HTTP server with graceful shutdown.

mod diagnostics;
mod guard;
mod mock_llm;
mod routes_ckpts;
mod routes_conv;
mod routes_misc;
mod routes_runs;
mod state;
mod static_files;

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

    // Settings (DB) drive the engine; config drives boot-level choices.
    let settings = match db.load_settings().await {
        Ok(s) => s,
        Err(e) => return Err(diagnostics::stage_fatal("settings", e.into())),
    };

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
    let search: Option<Arc<dyn vortex_agents::search::SearchProvider>> =
        if config.is_mock_llm() || settings.search.provider == "mock" {
            Some(Arc::new(vortex_agents::search::MockSearchProvider))
        } else if settings.search.is_configured() {
            Some(Arc::new(vortex_agents::search::SearxngProvider::new(
                &settings.search.base_url,
            )))
        } else {
            tracing::info!("no search provider configured; research tools will say so honestly");
            None
        };

    let approvals = Arc::new(vortex_agents::approvals::ApprovalManager::new(
        db.clone(),
        hub.clone(),
        settings.agents.approval_timeout_secs,
    ));
    let engine_slot = Arc::new(std::sync::OnceLock::new());
    let registry = vortex_agents::build_registry(previews.clone(), engine_slot.clone());
    let http = vortex_agents::safe_http::SafeHttp::new(false); // model URLs never touch local nets
    let engine = vortex_agents::Engine::new(
        llm.clone(),
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
    );
    let _ = engine_slot.set(engine.clone());

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
        llm,
        config,
        previews: previews.clone(),
        is_mock_llm,
        version: env!("CARGO_PKG_VERSION"),
        guard: guard_settings,
        diagnostics: diagnostics::DiagnosticsState::new(diag),
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
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
            tracing::info!("shutting down: cancelling runs, stopping previews, closing browser");
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
