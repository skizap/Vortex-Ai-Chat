//! Opt-in LIVE smoke test against the real OpenRouter API (KNOWN_ISSUES P14).
//!
//! This costs API credits, so it is `#[ignore]`d and never runs in CI or a
//! plain `cargo test`. Run it explicitly when you want to verify real
//! provider behavior end-to-end (streaming framing, usage accounting,
//! finish reasons):
//!
//! ```sh
//! OPENROUTER_API_KEY=sk-or-... \
//!   cargo test -p vortex-llm --test live_openrouter -- --ignored --nocapture
//! ```
//!
//! Model override: `OPENROUTER_TEST_MODEL` (default `openrouter/auto`).
//! The key is read from the environment only and is never printed.

use tokio::sync::mpsc;
use vortex_llm::llm::{CompletionRequest, LlmClient, LlmEvent};
use vortex_llm::OpenRouterClient;
use vortex_types::ChatMessage;

#[tokio::test]
#[ignore = "costs API credits; run explicitly: cargo test -p vortex-llm --test live_openrouter -- --ignored (needs OPENROUTER_API_KEY)"]
async fn live_openrouter_smoke() {
    let key = std::env::var("OPENROUTER_API_KEY").unwrap_or_else(|_| {
        panic!(
            "OPENROUTER_API_KEY must be set to run this ignored live test, e.g.\n  \
             OPENROUTER_API_KEY=sk-or-... cargo test -p vortex-llm --test live_openrouter -- --ignored"
        )
    });
    assert!(!key.trim().is_empty(), "OPENROUTER_API_KEY is empty");
    let model =
        std::env::var("OPENROUTER_TEST_MODEL").unwrap_or_else(|_| "openrouter/auto".to_string());
    let base_url = std::env::var("VORTEX_OPENROUTER_BASE_URL")
        .unwrap_or_else(|_| "https://openrouter.ai/api/v1".to_string());
    let client =
        OpenRouterClient::new(&base_url, Some(key)).expect("OpenRouter client builds with a key");

    let req = CompletionRequest {
        model: model.clone(),
        messages: vec![ChatMessage::user("Reply with exactly one word: pong")],
        tools: Vec::new(),
        temperature: 0.0,
        max_tokens: 16,
    };
    let (tx, mut rx) = mpsc::channel::<LlmEvent>(256);
    let counter = tokio::spawn(async move {
        let mut deltas = 0usize;
        while let Some(ev) = rx.recv().await {
            if matches!(ev, LlmEvent::TextDelta(_)) {
                deltas += 1;
            }
        }
        deltas
    });

    let resp = client
        .complete(&req, tx)
        .await
        .expect("live completion must succeed with a valid key");
    let deltas = counter.await.unwrap();

    // Diagnostics only: provider description and usage — never the key,
    // headers, or request bodies.
    eprintln!("provider: {}", client.describe());
    eprintln!(
        "model: {model} | streamed text deltas: {deltas} | finish_reason: {}",
        resp.finish_reason
    );
    if let Some(u) = resp.usage {
        eprintln!(
            "usage: prompt={} completion={} total={}",
            u.prompt_tokens, u.completion_tokens, u.total_tokens
        );
    } else {
        eprintln!("usage: (provider reported none)");
    }

    assert!(
        !resp.content.trim().is_empty(),
        "provider returned empty content"
    );
    assert!(
        !resp.finish_reason.trim().is_empty(),
        "finish_reason must be set"
    );
    assert!(resp.tool_calls.is_empty(), "no tools were offered");
}
