//! Offline catalog-fetch fixtures (plan step 6, catalog half): the documented
//! `sort=top-weekly` request, weekly-rank assignment from response position,
//! `top_provider.max_completion_tokens` and `expiration_date` parsing, and
//! honest absence for routers without a known response limit.
//! No network access and no real credentials are involved.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use axum::extract::{Query, State};
use axum::routing::get;
use serde_json::json;
use vortex_llm::llm::LlmClient;
use vortex_llm::openrouter::OpenRouterClient;

#[derive(Default)]
struct CatalogServerState {
    calls: AtomicUsize,
    last_query: tokio::sync::Mutex<Option<std::collections::HashMap<String, String>>>,
}

async fn models_handler(
    State(state): State<Arc<CatalogServerState>>,
    Query(query): Query<std::collections::HashMap<String, String>>,
) -> axum::Json<serde_json::Value> {
    state.calls.fetch_add(1, Ordering::SeqCst);
    *state.last_query.lock().await = Some(query);
    // Deliberately NOT in price order and NOT in id order: weekly rank must
    // come from the response position, and the wire order stays id-sorted.
    axum::Json(json!({
        "data": [
            { "id": "qwen/qwen3-32b", "name": "Qwen3 32B", "context_length": 131072,
              "supported_parameters": ["tools"],
              "pricing": { "prompt": "0.00000008", "completion": "0.00000028" },
              "top_provider": { "context_length": 131072, "max_completion_tokens": 16384 },
              "expiration_date": null },
            { "id": "deepseek/deepseek-chat-v3.1", "name": "DeepSeek V3.1", "context_length": 163840,
              "supported_parameters": ["tools"],
              "pricing": { "prompt": "0.00000025", "completion": "0.00000095" },
              "top_provider": { "context_length": 163840, "max_completion_tokens": 147456 },
              "expiration_date": "2027-01-31" },
            { "id": "", "name": "malformed row without id" },
            { "id": "openrouter/auto", "name": "OpenRouter Auto", "context_length": null,
              "supported_parameters": [],
              "pricing": { "prompt": "0", "completion": "0" },
              "top_provider": { "context_length": null, "max_completion_tokens": null } }
        ]
    }))
}

async fn spawn_catalog_server() -> (String, Arc<CatalogServerState>) {
    let state = Arc::new(CatalogServerState::default());
    let app = axum::Router::new()
        .route("/v1/models", get(models_handler))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}/v1"), state)
}

/// The fetch must carry the documented ranking parameter, and the recorded
/// weekly ranks must reflect the provider's response order (usage ranking),
/// not the id-sorted wire order and not price order.
#[tokio::test]
async fn fetch_uses_documented_top_weekly_sort_and_records_ranks() {
    let (base, state) = spawn_catalog_server().await;
    let client = OpenRouterClient::new(&base, Some("test-key".into())).unwrap();
    let models = client.models().await.unwrap();

    let query = state.last_query.lock().await;
    assert_eq!(
        query
            .as_ref()
            .and_then(|q| q.get("sort"))
            .map(String::as_str),
        Some("top-weekly"),
        "catalog fetch must request the documented sort=top-weekly ordering"
    );
    assert_eq!(state.calls.load(Ordering::SeqCst), 1);

    // Malformed row skipped; wire order stays deterministic (id-sorted) while
    // ranks preserve the provider's weekly-usage positions.
    let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(
        ids,
        vec![
            "deepseek/deepseek-chat-v3.1",
            "openrouter/auto",
            "qwen/qwen3-32b"
        ]
    );
    let rank_of = |id: &str| models.iter().find(|m| m.id == id).unwrap().weekly_rank;
    assert_eq!(rank_of("qwen/qwen3-32b"), Some(1));
    assert_eq!(rank_of("deepseek/deepseek-chat-v3.1"), Some(2));
    // Position 3 was the malformed row: the next valid model keeps position 4,
    // so ranks stay faithful to the provider's ordering.
    assert_eq!(rank_of("openrouter/auto"), Some(4));
}

/// The advertised output limit comes from `top_provider.max_completion_tokens`
/// and the retirement date from `expiration_date`; a router that reports no
/// limit keeps `None` (never an invented cap).
#[tokio::test]
async fn parses_provider_output_limit_and_expiration() {
    let (base, _) = spawn_catalog_server().await;
    let client = OpenRouterClient::new(&base, Some("test-key".into())).unwrap();
    let models = client.models().await.unwrap();
    let find = |id: &str| models.iter().find(|m| m.id == id).unwrap();

    assert_eq!(find("qwen/qwen3-32b").max_completion_tokens, Some(16_384));
    assert_eq!(find("qwen/qwen3-32b").expires_on, None);
    assert_eq!(
        find("deepseek/deepseek-chat-v3.1").max_completion_tokens,
        Some(147_456)
    );
    assert_eq!(
        find("deepseek/deepseek-chat-v3.1").expires_on.as_deref(),
        Some("2027-01-31")
    );
    // Router without a known response limit: absence stays absence.
    assert_eq!(find("openrouter/auto").max_completion_tokens, None);
    // License provenance is never invented at fetch time; it is attached only
    // from the reviewed mapping at serve time.
    assert!(find("qwen/qwen3-32b").open_weights.is_none());
    assert!(find("qwen/qwen3-32b").license.is_none());
}

/// End-to-end offline: fetched rows flow through enrichment and the shortlist,
/// producing the weekly-ranked recommendation with verified license data.
#[tokio::test]
async fn fetched_catalog_feeds_enrichment_and_shortlist() {
    let (base, _) = spawn_catalog_server().await;
    let client = OpenRouterClient::new(&base, Some("test-key".into())).unwrap();
    let models = client.models().await.unwrap();

    let enriched = vortex_llm::catalog::enrich(&models);
    let qwen = enriched.iter().find(|m| m.id == "qwen/qwen3-32b").unwrap();
    assert_eq!(qwen.open_weights, Some(true));
    assert_eq!(qwen.license.as_deref(), Some("Apache-2.0"));
    let deepseek = enriched
        .iter()
        .find(|m| m.id == "deepseek/deepseek-chat-v3.1")
        .unwrap();
    assert_eq!(deepseek.license.as_deref(), Some("MIT"));
    let router = enriched.iter().find(|m| m.id == "openrouter/auto").unwrap();
    assert!(router.open_weights.is_none());

    assert_eq!(
        vortex_llm::catalog::ranking_source(&enriched),
        Some(vortex_llm::catalog::RANKING_SOURCE)
    );
    let bounds = vortex_types::RecommendedBounds::default();
    // Weekly order: qwen3-32b (rank 1) before deepseek-chat-v3.1 (rank 2);
    // the router is excluded despite zero prices.
    assert_eq!(
        vortex_llm::catalog::recommended_on(&enriched, bounds, 5, "2026-11-22"),
        vec!["qwen/qwen3-32b", "deepseek/deepseek-chat-v3.1"]
    );
}
