//! Human-in-the-loop approval workflow.
//!
//! When a tool (or a sub-agent) needs approval, `ApprovalGate::request`
//! persists the request, streams an `ApprovalRequested` event to the UI, and
//! waits for the user's decision (with a timeout that auto-denies).
//! Only the server API can resolve approvals — agents can never self-approve.

use crate::errors::ToolError;
use crate::hub::Hub;
use crate::registry::ApprovalGate;
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{oneshot, Mutex};
use vortex_core::Db;
use vortex_types::{ApprovalStatus, RunEvent};

pub struct ApprovalManager {
    db: Db,
    hub: Arc<Hub>,
    pending: Mutex<HashMap<String, oneshot::Sender<bool>>>,
    timeout_secs: u64,
}

impl ApprovalManager {
    pub fn new(db: Db, hub: Arc<Hub>, timeout_secs: u64) -> Self {
        Self {
            db,
            hub,
            pending: Mutex::new(HashMap::new()),
            timeout_secs,
        }
    }

    /// Resolve a pending approval from the server API. Returns whether the
    /// approval existed and was still pending.
    pub async fn decide(&self, id: &str, approve: bool) -> bool {
        let sender = self.pending.lock().await.remove(id);
        let Some(sender) = sender else { return false };
        let status = if approve {
            ApprovalStatus::Approved
        } else {
            ApprovalStatus::Denied
        };
        let _ = self.db.resolve_approval(id.to_string(), status).await;
        sender.send(approve).is_ok()
    }

    /// Approvals pending resolution right now.
    pub async fn pending_approvals(&self) -> Vec<vortex_types::ApprovalInfo> {
        self.db.list_approvals(true).await.unwrap_or_default()
    }
}

#[async_trait]
impl ApprovalGate for ApprovalManager {
    async fn request(&self, run_id: &str, title: &str, payload: &str) -> Result<bool, ToolError> {
        let id = uuid::Uuid::new_v4().to_string();
        let agent = self
            .hub
            .get(run_id)
            .map(|h| h.agent())
            .unwrap_or_else(|| "agent".into());
        self.db
            .add_approval(
                id.clone(),
                run_id.to_string(),
                agent.clone(),
                title.to_string(),
                payload.to_string(),
            )
            .await
            .map_err(|e| ToolError::Execution(format!("persisting approval: {e}")))?;
        self.hub.publish(
            run_id,
            RunEvent::ApprovalRequested {
                approval_id: id.clone(),
                run_id: run_id.to_string(),
                agent,
                title: title.to_string(),
                payload: payload.to_string(),
            },
        );
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id.clone(), tx);
        let timeout = Duration::from_secs(self.timeout_secs.max(5));
        let approved = match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(v)) => v,
            Ok(Err(_)) => {
                let _ = self
                    .db
                    .resolve_approval(id.clone(), ApprovalStatus::Expired)
                    .await;
                false
            }
            Err(_) => {
                self.pending.lock().await.remove(&id);
                let _ = self
                    .db
                    .resolve_approval(id.clone(), ApprovalStatus::Expired)
                    .await;
                tracing::info!(approval_id = %id, "approval timed out and was auto-denied");
                false
            }
        };
        self.hub.publish(
            run_id,
            RunEvent::ApprovalResolved {
                approval_id: id,
                run_id: run_id.to_string(),
                approved,
            },
        );
        Ok(approved)
    }
}

/// Test gate: approvals auto-resolve with the configured default answer,
/// with a small timeout so slow paths behave like the real manager.
pub struct TestGate {
    default: bool,
}

impl TestGate {
    pub fn new(default: bool) -> Self {
        Self { default }
    }
}

#[async_trait]
impl ApprovalGate for TestGate {
    async fn request(
        &self,
        _run_id: &str,
        _title: &str,
        _payload: &str,
    ) -> Result<bool, ToolError> {
        // Simulate the human decision delay so waits are observable.
        tokio::time::sleep(Duration::from_millis(10)).await;
        Ok(self.default)
    }
}
