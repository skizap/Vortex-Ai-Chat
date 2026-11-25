//! Miscellaneous routes: health/capabilities, settings, models, approvals,
//! workspace validation, previews.

use crate::state::{err_json, AppState};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use std::sync::Arc;
use vortex_types::{
    AgentSettings, SearchTestErrorKind, SearchTestResult, SearchTestStatus, Settings,
};

pub async fn health(State(state): State<Arc<AppState>>) -> axum::response::Response {
    Json(state.capabilities().await).into_response()
}

pub async fn get_settings(State(state): State<Arc<AppState>>) -> Json<Settings> {
    Json(state.engine.current_settings().await)
}

pub async fn put_settings(
    State(state): State<Arc<AppState>>,
    Json(mut settings): Json<Settings>,
) -> axum::response::Response {
    // Validate and clamp before accepting anything.
    if vortex_llm::llm::PdfEngine::parse(&settings.media.pdf_engine).is_none() {
        return err_json(
            StatusCode::BAD_REQUEST,
            "PDF parser must be cloudflare-ai or mistral-ocr",
        );
    }
    settings.agents.max_concurrent = settings
        .agents
        .max_concurrent
        .clamp(1, AgentSettings::MAX_LIMIT_CEILING);
    settings.agents.max_iterations = settings.agents.max_iterations.clamp(1, 64);
    settings.agents.run_timeout_secs = settings.agents.run_timeout_secs.clamp(30, 3600);
    settings.agents.approval_timeout_secs = settings.agents.approval_timeout_secs.clamp(10, 3600);
    settings.temperature = settings.temperature.clamp(0.0, 2.0);
    settings.max_tokens = settings.max_tokens.clamp(64, 100_000);
    settings.search.max_results = settings.search.max_results.clamp(1, 20);
    if let Some(ws) = settings.workspace.clone() {
        let trimmed = ws.trim().to_string();
        if trimmed.is_empty() {
            settings.workspace = None;
        } else if !std::path::Path::new(&trimmed).is_dir() {
            return err_json(
                StatusCode::BAD_REQUEST,
                format!("workspace '{}' is not a directory", trimmed),
            );
        } else {
            settings.workspace = Some(trimmed);
        }
    }
    if !state.is_mock_llm && !settings.search.base_url.trim().is_empty() {
        // Validate search URL shape early for a better error message.
        if url::Url::parse(settings.search.base_url.trim()).is_err() {
            return err_json(
                StatusCode::BAD_REQUEST,
                "search base_url is not a valid URL",
            );
        }
    }
    if let Err(e) = state.db.save_settings(&settings).await {
        return err_json(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    }
    // Rebuild the search provider from the just-saved settings (same policy
    // as startup) and apply settings + provider under ONE atomic engine
    // update, so concurrent readers never pair new settings with a stale
    // provider. A new or removed URL applies to subsequent runs without a
    // restart; runs already admitted keep their snapshot.
    let provider = crate::state::build_search_provider(&settings, state.is_mock_llm);
    state
        .engine
        .apply_saved_settings(settings.clone(), provider)
        .await;
    Json(settings).into_response()
}

/// Route-level hard bound for the search probe. The SearXNG client itself is
/// bounded (5s connect / 15s total); this backstop guarantees the HTTP
/// response always returns even if a future provider misconfigures timeouts.
const SEARCH_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// POST /api/settings/search/test — verify the saved search provider without
/// a model call. Guarded like every route; takes no body, no caller-provided
/// URL and no query: the fixed benign probe (`search::PROBE_QUERY`) goes
/// through the same server-side provider and JSON parsing as `web_search`.
/// Always answers 200 with a structured [`SearchTestResult`]; the configured
/// URL is never echoed (it could carry credentials) and no probe state is
/// kept server-side, so nothing here turns `/api/health` or any poll into
/// network I/O. Error details are fixed, sanitized strings — never raw
/// transport output.
pub async fn test_search(State(state): State<Arc<AppState>>) -> Json<SearchTestResult> {
    use vortex_agents::search::{SearchError, PROBE_LIMIT, PROBE_QUERY};

    // One atomic snapshot: the enablement flag and the provider always
    // belong to the same saved-settings generation.
    let (settings, provider) = state.engine.settings_and_search().await;
    let tested_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let result = |status: SearchTestStatus,
                  provider: &str,
                  result_count: usize,
                  http_status: Option<u16>,
                  error_kind: Option<SearchTestErrorKind>,
                  detail: Option<String>| SearchTestResult {
        status,
        mock: provider == "mock",
        provider: provider.to_string(),
        result_count,
        http_status,
        error_kind,
        detail,
        tested_at,
    };

    if !settings.tools.search {
        let name = provider.as_ref().map(|p| p.describe()).unwrap_or("");
        return Json(result(
            SearchTestStatus::Disabled,
            name,
            0,
            None,
            None,
            Some(
                "Web tools are turned off in Settings → Tools; enable them to use search."
                    .to_string(),
            ),
        ));
    }
    let Some(provider) = provider else {
        return Json(result(
            SearchTestStatus::NotConfigured,
            "",
            0,
            None,
            None,
            Some(crate::state::SEARCH_SETUP_HINT.to_string()),
        ));
    };
    let name = provider.describe();
    match tokio::time::timeout(
        SEARCH_PROBE_TIMEOUT,
        provider.search(PROBE_QUERY, PROBE_LIMIT),
    )
    .await
    {
        Ok(Ok(results)) if results.is_empty() => Json(result(
            SearchTestStatus::Empty,
            name,
            0,
            None,
            None,
            Some(
                "Connected successfully, but the provider returned no results for the test \
                 probe."
                    .to_string(),
            ),
        )),
        Ok(Ok(results)) => Json(result(
            SearchTestStatus::Ok,
            name,
            results.len(),
            None,
            None,
            None,
        )),
        Ok(Err(err)) => {
            let (kind, http_status) = match &err {
                SearchError::Connect | SearchError::Timeout | SearchError::Transport => {
                    (SearchTestErrorKind::Connection, None)
                }
                SearchError::HttpStatus(code) => (SearchTestErrorKind::HttpStatus, Some(*code)),
                SearchError::InvalidJson | SearchError::MissingResults => {
                    (SearchTestErrorKind::InvalidResponse, None)
                }
                SearchError::Provider(_) | SearchError::ResponseTooLarge => {
                    (SearchTestErrorKind::Provider, None)
                }
            };
            Json(result(
                SearchTestStatus::Error,
                name,
                0,
                http_status,
                Some(kind),
                Some(err.to_string()),
            ))
        }
        Err(_elapsed) => Json(result(
            SearchTestStatus::Error,
            name,
            0,
            None,
            Some(SearchTestErrorKind::Connection),
            Some(
                "The test request timed out before the provider answered. Check that the \
                 service is running and responsive."
                    .to_string(),
            ),
        )),
    }
}

/// GET /api/models — server-side OpenRouter catalog enriched with the
/// reviewed open-weights mapping, plus the budget-filtered recommended
/// shortlist (plan step 3). Optional editable query bounds:
/// `max_input_usd_per_mtok` (default 1.0), `max_output_usd_per_mtok`
/// (default 3.0), `recommended_limit` (default 5); effective values are
/// echoed. Unknown prices/licenses stay out of `recommended` but remain in
/// `models`. Honest fallback with manual-ID instructions on cold-start
/// failure; a stale catalog still yields a (labeled) last-known-good result.
pub async fn list_models(
    State(state): State<Arc<AppState>>,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> axum::response::Response {
    let catalog = state.engine.models_status().await;
    match catalog.models {
        Some(models) => {
            let parse_num = |key: &str| {
                q.get(key)
                    .and_then(|s| s.trim().parse::<f64>().ok())
                    .unwrap_or(f64::NAN) // NaN → per-field default in clamped()
            };
            let bounds = vortex_types::RecommendedBounds::clamped(
                parse_num("max_input_usd_per_mtok"),
                parse_num("max_output_usd_per_mtok"),
            );
            let limit = vortex_llm::catalog::clamp_limit(
                q.get("recommended_limit")
                    .and_then(|s| s.trim().parse::<u32>().ok())
                    .unwrap_or(vortex_llm::catalog::DEFAULT_RECOMMENDED_LIMIT),
            );
            let enriched = vortex_llm::catalog::enrich(models.as_ref());
            let recommended = vortex_llm::catalog::recommended(&enriched, bounds, limit);
            let ranking_source = vortex_llm::catalog::ranking_source(&enriched);
            Json(serde_json::json!({
                "models": enriched,
                "configured": state.is_mock_llm || state.config.api_key().is_some(),
                "fetched_at": catalog.fetched_at,
                "stale": catalog.stale,
                "error": catalog.error,
                "recommended": recommended,
                "recommended_bounds": bounds,
                "recommended_limit": limit,
                "ranking_source": ranking_source,
            }))
            .into_response()
        }
        None => (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({
                "error": format!("could not fetch model catalog: {}", catalog.error.as_deref().unwrap_or("no catalog available")),
                "configured": false,
                "hint": "You can still set a model ID manually in Settings (e.g. 'openai/gpt-4o-mini' \
                         or 'anthropic/claude-sonnet-4'). Check your API key and network.",
            })),
        )
            .into_response(),
    }
}

/// GET /api/approvals — approvals pending a human decision.
pub async fn list_approvals(
    State(state): State<Arc<AppState>>,
) -> Json<Vec<vortex_types::ApprovalInfo>> {
    Json(state.engine.approvals.pending_approvals().await)
}

/// POST /api/approvals/{id}/decide {"approve": bool} — the ONLY way an
/// approval can be resolved. Agents (models) have no access to this route's
/// effects beyond the request they initiated.
pub async fn decide_approval(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> axum::response::Response {
    let approve = body
        .get("approve")
        .and_then(|a| a.as_bool())
        .unwrap_or(false);
    match state.engine.approvals.decide(&id, approve).await {
        true => Json(serde_json::json!({"ok": true})).into_response(),
        false => err_json(
            StatusCode::NOT_FOUND,
            "approval not found or already resolved",
        ),
    }
}

/// GET /api/workspace/check?path=... — validate a workspace root candidate.
pub async fn check_workspace(
    State(_state): State<Arc<AppState>>,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> axum::response::Response {
    let Some(path) = q.get("path").map(|s| s.to_string()) else {
        return err_json(StatusCode::BAD_REQUEST, "missing 'path' parameter");
    };
    let path = path.trim().to_string();
    match vortex_agents::workspace::WorkspaceRoots::new(std::path::Path::new(&path)) {
        Ok(roots) => Json(serde_json::json!({
            "ok": true,
            "root": roots.root().display().to_string(),
        }))
        .into_response(),
        Err(e) => err_json(StatusCode::BAD_REQUEST, e.to_string()),
    }
}

pub async fn list_previews(
    State(state): State<Arc<AppState>>,
) -> Json<Vec<vortex_types::PreviewInfo>> {
    Json(state.previews.list())
}

#[cfg(test)]
mod catalog_route_tests {
    use super::*;
    use vortex_llm::llm::{CompletionRequest, CompletionResponse, LlmClient, LlmEvent};
    use vortex_types::ModelInfo;

    /// Catalog-only stub: the models route never calls `complete`.
    struct CatalogStub {
        catalog: Vec<ModelInfo>,
        fail: bool,
    }

    #[async_trait::async_trait]
    impl LlmClient for CatalogStub {
        async fn complete(
            &self,
            _req: &CompletionRequest,
            _events: tokio::sync::mpsc::Sender<LlmEvent>,
        ) -> Result<CompletionResponse, vortex_llm::LlmError> {
            Err(vortex_llm::LlmError::Network(
                "not used by catalog tests".into(),
            ))
        }

        async fn models(&self) -> Result<Vec<ModelInfo>, vortex_llm::LlmError> {
            if self.fail {
                return Err(vortex_llm::LlmError::Network("catalog offline".into()));
            }
            Ok(self.catalog.clone())
        }

        fn describe(&self) -> &'static str {
            "catalog-stub"
        }
    }

    /// Full guarded app on an ephemeral loopback port with a stubbed catalog.
    async fn catalog_app(catalog: Vec<ModelInfo>, fail: bool) -> (String, tempfile::TempDir) {
        let llm: Arc<dyn LlmClient> = Arc::new(CatalogStub { catalog, fail });
        let dir = tempfile::tempdir().unwrap();
        let db = vortex_core::Db::open(dir.path().join("test.db")).unwrap();
        let settings = Settings::default();
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
        let engine = vortex_agents::Engine::new(
            llm,
            registry,
            hub.clone(),
            scheduler,
            approvals,
            db.clone(),
            browser,
            previews.clone(),
            None,
            http,
            settings.clone(),
            Some(attachments.clone()),
        );
        let _ = engine_slot.set(engine.clone());
        let mut config = vortex_core::AppConfig::default();
        config.llm.provider = "mock".into();
        let supervisor = vortex_agents::supervisor::Supervisor::new(
            db.clone(),
            engine.clone(),
            hub.clone(),
            Arc::new(vortex_agents::clock::SystemClock),
            2,
            tokio_util::sync::CancellationToken::new(),
        );
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
        let app = crate::build_router(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{addr}"), dir)
    }

    fn row(id: &str, rank: Option<u32>, input: Option<&str>, output: Option<&str>) -> ModelInfo {
        ModelInfo {
            id: id.into(),
            name: id.into(),
            weekly_rank: rank,
            context_length: Some(128_000),
            supports_tools: Some(true),
            pricing_prompt: input.map(str::to_string),
            pricing_completion: output.map(str::to_string),
            input_modalities: vec!["text".into()],
            output_modalities: vec!["text".into()],
            ..Default::default()
        }
    }

    /// Mixed fixture covering every exclusion reason at once: in-bounds
    /// verified rows, an over-output-bound row, a restricted-license row, an
    /// unknown-price row, an unmapped free row and a router.
    fn mixed_catalog() -> Vec<ModelInfo> {
        vec![
            row(
                "deepseek/deepseek-chat-v3.1",
                Some(1),
                Some("0.00000025"),
                Some("0.00000095"),
            ),
            row(
                "qwen/qwen3-32b",
                Some(2),
                Some("0.00000008"),
                Some("0.00000028"),
            ),
            row(
                "qwen/qwen3-235b-a22b-thinking-2507",
                Some(3),
                Some("0.00000045"),
                Some("0.0000035"),
            ),
            row(
                "qwen/qwen-2.5-72b-instruct",
                Some(4),
                Some("0.00000036"),
                Some("0.0000004"),
            ),
            row("qwen/qwen3.5-9b", Some(5), None, None),
            row("mock/model", Some(6), Some("0"), Some("0")),
            row("openrouter/auto", Some(7), Some("0"), Some("0")),
        ]
    }

    async fn get_models(base: &str, query: &str) -> serde_json::Value {
        let url = if query.is_empty() {
            format!("{base}/api/models")
        } else {
            format!("{base}/api/models?{query}")
        };
        reqwest::get(url)
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn models_route_serves_ranked_shortlist_and_verified_metadata() {
        let (base, _dir) = catalog_app(mixed_catalog(), false).await;
        let body = get_models(&base, "").await;

        // Weekly-rank order; every excluded category stays out.
        let recommended: Vec<&str> = body["recommended"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(
            recommended,
            vec!["deepseek/deepseek-chat-v3.1", "qwen/qwen3-32b"]
        );
        // Effective defaults echoed.
        assert_eq!(body["recommended_bounds"]["max_input_usd_per_mtok"], 1.0);
        assert_eq!(body["recommended_bounds"]["max_output_usd_per_mtok"], 3.0);
        assert_eq!(body["recommended_limit"], 5);
        assert_eq!(
            body["ranking_source"].as_str(),
            Some(vortex_llm::catalog::RANKING_SOURCE)
        );

        // Enrichment: verified rows carry reviewed provenance, unmapped rows
        // never do, restricted rows are flagged, routers report no invented
        // output limit.
        let models = body["models"].as_array().unwrap();
        let find = |id: &str| models.iter().find(|m| m["id"] == id).unwrap();
        assert_eq!(find("deepseek/deepseek-chat-v3.1")["license"], "MIT");
        assert_eq!(find("deepseek/deepseek-chat-v3.1")["open_weights"], true);
        assert_eq!(find("qwen/qwen3-32b")["license"], "Apache-2.0");
        assert_eq!(find("qwen/qwen3-32b")["weekly_rank"], 2);
        assert_eq!(find("qwen/qwen-2.5-72b-instruct")["open_weights"], true);
        assert!(find("qwen/qwen-2.5-72b-instruct")["restriction"].is_string());
        assert!(find("mock/model")["open_weights"].is_null());
        assert!(find("mock/model")["license"].is_null());
        assert!(find("openrouter/auto")["max_completion_tokens"].is_null());
        // Every catalog row stays visible regardless of shortlist membership.
        assert_eq!(models.len(), mixed_catalog().len());
    }

    #[tokio::test]
    async fn models_route_bounds_and_limit_are_editable() {
        let (base, _dir) = catalog_app(mixed_catalog(), false).await;

        // Tight output bound flips the shortlist to the only row under it and
        // the limit truncates; effective values are echoed, never silent.
        let body = get_models(&base, "max_output_usd_per_mtok=0.5&recommended_limit=1").await;
        assert_eq!(body["recommended"], serde_json::json!(["qwen/qwen3-32b"]));
        assert_eq!(body["recommended_bounds"]["max_output_usd_per_mtok"], 0.5);
        assert_eq!(body["recommended_limit"], 1);

        // Free-only bounds: nothing mapped is published-free here → empty
        // shortlist, but the full catalog is untouched.
        let body = get_models(&base, "max_input_usd_per_mtok=0&max_output_usd_per_mtok=0").await;
        assert_eq!(body["recommended"], serde_json::json!([]));
        assert_eq!(body["models"].as_array().unwrap().len(), 7);

        // Unparseable bounds fall back per field to the documented defaults.
        let body = get_models(&base, "max_input_usd_per_mtok=abc&recommended_limit=xyz").await;
        assert_eq!(body["recommended_bounds"]["max_input_usd_per_mtok"], 1.0);
        assert_eq!(body["recommended_limit"], 5);
        // Out-of-range limit clamps to the server ceiling.
        let body = get_models(&base, "recommended_limit=9999").await;
        assert_eq!(
            body["recommended_limit"],
            vortex_llm::catalog::MAX_RECOMMENDED_LIMIT
        );
    }

    #[tokio::test]
    async fn models_route_without_ranks_omits_popularity_claims() {
        let unranked: Vec<ModelInfo> = mixed_catalog()
            .into_iter()
            .map(|m| ModelInfo {
                weekly_rank: None,
                ..m
            })
            .collect();
        let (base, _dir) = catalog_app(unranked, false).await;
        let body = get_models(&base, "").await;
        assert!(body["ranking_source"].is_null());
        // Still a working shortlist, deterministically id-ordered.
        assert_eq!(
            body["recommended"],
            serde_json::json!(["deepseek/deepseek-chat-v3.1", "qwen/qwen3-32b"])
        );
    }

    #[tokio::test]
    async fn models_route_cold_failure_keeps_502_and_hint() {
        let (base, _dir) = catalog_app(Vec::new(), true).await;
        let resp = reqwest::get(format!("{base}/api/models")).await.unwrap();
        assert_eq!(resp.status(), 502);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert!(body["error"]
            .as_str()
            .unwrap()
            .contains("could not fetch model catalog"));
        assert!(body["hint"].as_str().unwrap().contains("manually"));
    }
}
