//! Conversation usage aggregation route.

use crate::state::{err_json, AppState};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use std::sync::Arc;
use vortex_core::Db;
use vortex_types::ConversationUsage;

/// GET /api/conversations/{id}/usage — latest cumulative provider-reported
/// usage per distinct run across the conversation's whole run tree
/// (coordinator runs, retries, and delegated sub-agent runs). Each run
/// contributes its most recent total exactly once; cumulative SSE events are
/// never summed. Runs whose provider usage is missing are flagged
/// `incomplete`, so partial totals can be labelled "reported tokens" and
/// missing history never looks like zero. Fetched on conversation switch and
/// reconnect; live `UsageUpdate` events reconcile by run id.
pub async fn conversation_usage(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> axum::response::Response {
    match usage_summary(&state.db, &id).await {
        Ok(Some(summary)) => Json(summary).into_response(),
        Ok(None) => err_json(StatusCode::NOT_FOUND, "conversation not found"),
        Err(_) => {
            // Same degraded-response pattern as the other read routes; the
            // failure is traced with a fixed category, never the error text.
            crate::diagnostics::db_failure("conversation_usage", Some(&id));
            err_json(StatusCode::INTERNAL_SERVER_ERROR, "usage lookup failed")
        }
    }
}

/// `Ok(None)` means the conversation does not exist. Free function so tests
/// can drive the aggregation with a plain `Db`.
pub async fn usage_summary(
    db: &Db,
    conversation_id: &str,
) -> vortex_core::Result<Option<ConversationUsage>> {
    if db
        .get_conversation(conversation_id.to_string())
        .await?
        .is_none()
    {
        return Ok(None);
    }
    let runs = db.conversation_usage(conversation_id.to_string()).await?;
    Ok(Some(ConversationUsage {
        conversation_id: conversation_id.to_string(),
        runs,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use vortex_types::{Mode, RunInfo, RunStatus, Usage};

    fn run(
        id: &str,
        conv: Option<&str>,
        parent: Option<&str>,
        status: RunStatus,
        usage: Option<Usage>,
    ) -> RunInfo {
        RunInfo {
            id: id.into(),
            parent_run_id: parent.map(str::to_string),
            conversation_id: conv.map(str::to_string),
            agent: "coordinator".into(),
            task: String::new(),
            model: "m".into(),
            status,
            error: None,
            usage,
            plan: Vec::new(),
            depth: 0,
            created_at: vortex_core::db::now(),
            updated_at: vortex_core::db::now(),
        }
    }

    fn usage(p: u64, c: u64) -> Usage {
        Usage {
            prompt_tokens: p,
            completion_tokens: c,
            total_tokens: p + c,
        }
    }

    #[tokio::test]
    async fn usage_summary_aggregates_the_whole_run_tree_once_per_run() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path().join("usage.db")).unwrap();
        let conv = db.create_conversation("c", Mode::Chat, None).await.unwrap();
        let other = db
            .create_conversation("other", Mode::Chat, None)
            .await
            .unwrap();

        // Coordinator run + a retried coordinator run, each with its own
        // latest cumulative total.
        db.upsert_run(&run(
            "r1",
            Some(&conv.id),
            None,
            RunStatus::Completed,
            Some(usage(10, 5)),
        ))
        .await
        .unwrap();
        db.upsert_run(&run(
            "r2",
            Some(&conv.id),
            None,
            RunStatus::Completed,
            Some(usage(4, 4)),
        ))
        .await
        .unwrap();
        // Delegated sub-agent runs (conversation_id NULL, joined via parent),
        // including a nested one.
        db.upsert_run(&run(
            "r1-sub",
            None,
            Some("r1"),
            RunStatus::Completed,
            Some(usage(2, 1)),
        ))
        .await
        .unwrap();
        db.upsert_run(&run(
            "r1-sub-deep",
            None,
            Some("r1-sub"),
            RunStatus::Completed,
            Some(usage(1, 1)),
        ))
        .await
        .unwrap();
        // Terminal run that never reported usage → incomplete.
        db.upsert_run(&run("r1-failed", None, Some("r1"), RunStatus::Failed, None))
            .await
            .unwrap();
        // Active run without calls yet → NOT permanently incomplete.
        db.upsert_run(&run(
            "r3-live",
            Some(&conv.id),
            None,
            RunStatus::Running,
            None,
        ))
        .await
        .unwrap();
        // Reported usage but a completion inside it never reported → flagged.
        db.upsert_run(&run(
            "r4-gap",
            Some(&conv.id),
            None,
            RunStatus::Completed,
            Some(usage(7, 3)),
        ))
        .await
        .unwrap();
        assert!(db.mark_run_usage_incomplete("r4-gap".into()).await.unwrap());
        // Idempotent: the flag transition happens once.
        assert!(!db.mark_run_usage_incomplete("r4-gap".into()).await.unwrap());
        // Another conversation's run must not leak in.
        db.upsert_run(&run(
            "x1",
            Some(&other.id),
            None,
            RunStatus::Completed,
            Some(usage(100, 100)),
        ))
        .await
        .unwrap();

        let summary = usage_summary(&db, &conv.id)
            .await
            .unwrap()
            .expect("conversation exists");
        assert_eq!(summary.conversation_id, conv.id);
        let ids: Vec<&str> = summary.runs.iter().map(|r| r.run_id.as_str()).collect();
        for expected in [
            "r1",
            "r2",
            "r1-sub",
            "r1-sub-deep",
            "r1-failed",
            "r3-live",
            "r4-gap",
        ] {
            assert!(ids.contains(&expected), "missing {expected} in {ids:?}");
        }
        assert!(!ids.contains(&"x1"), "other conversations excluded");
        assert_eq!(ids.len(), 7, "each run counted exactly once: {ids:?}");

        let by_id = |id: &str| summary.runs.iter().find(|r| r.run_id == id).unwrap();
        // Latest cumulative usage per run, reported as-is (summing is the
        // client's job across distinct runs only).
        assert_eq!(by_id("r1").usage, Some(usage(10, 5)));
        assert!(!by_id("r1").incomplete);
        assert_eq!(by_id("r1-sub-deep").usage, Some(usage(1, 1)));
        assert!(!by_id("r1-sub-deep").incomplete);
        // Terminal without any reported usage → incomplete, usage null.
        assert_eq!(by_id("r1-failed").usage, None);
        assert!(by_id("r1-failed").incomplete);
        // Active without calls yet → not permanently incomplete.
        assert_eq!(by_id("r3-live").usage, None);
        assert!(!by_id("r3-live").incomplete);
        // Persisted missing-call flag wins even though usage was reported.
        assert_eq!(by_id("r4-gap").usage, Some(usage(7, 3)));
        assert!(by_id("r4-gap").incomplete);
    }

    #[tokio::test]
    async fn usage_summary_is_honest_for_unknown_and_empty_conversations() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path().join("usage.db")).unwrap();
        assert!(
            usage_summary(&db, "missing").await.unwrap().is_none(),
            "unknown conversation → None (the route answers 404)"
        );
        let conv = db
            .create_conversation("empty", Mode::Chat, None)
            .await
            .unwrap();
        let summary = usage_summary(&db, &conv.id).await.unwrap().unwrap();
        assert!(
            summary.runs.is_empty(),
            "no runs → empty list, never a fake zero total"
        );
    }
}
