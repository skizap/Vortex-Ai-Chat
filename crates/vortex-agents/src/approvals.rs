//! Human-in-the-loop approval workflow.
//!
//! When a tool (or a sub-agent) needs approval, `ApprovalGate::request`
//! persists the request, streams an `ApprovalRequested` event to the UI, and
//! waits for the user's decision. Interactive runs auto-deny on timeout;
//! job-origin runs (`ParkUntilDecided`) park indefinitely until a human
//! decides or the run is cancelled (shutdown expires the parked approval).
//! Only the server API can resolve approvals — agents can never self-approve.

use crate::errors::ToolError;
use crate::hub::Hub;
use crate::registry::ApprovalGate;
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{oneshot, Mutex};
use vortex_core::diag;
use vortex_core::Db;
use vortex_types::{ApprovalPolicy, ApprovalStatus, RunEvent, RunStatus};

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
    /// approval existed, was still pending, AND the decision was delivered
    /// to the waiter.
    ///
    /// The sender is removed under the lock, so exactly one of `decide` or
    /// the timeout branch owns the DB write for a given approval. If the
    /// waiter has already timed out (send fails), the row is recorded as
    /// `expired` — never as a decision nobody consumed.
    pub async fn decide(&self, id: &str, approve: bool) -> bool {
        let sender = self.pending.lock().await.remove(id);
        let Some(sender) = sender else { return false };
        if sender.send(approve).is_err() {
            // Waiter timed out between the timeout firing and this call.
            let resolved = self
                .db
                .resolve_approval(id.to_string(), ApprovalStatus::Expired)
                .await;
            if resolved.is_err() {
                diag::event(
                    tracing::Level::ERROR,
                    diag::APPROVAL_RESOLVE_FAILED,
                    &[
                        ("approval_id", diag::Field::Uuid(id)),
                        ("outcome", diag::Field::Static("expired")),
                    ],
                );
            }
            return false;
        }
        let status = if approve {
            ApprovalStatus::Approved
        } else {
            ApprovalStatus::Denied
        };
        let resolved = self.db.resolve_approval(id.to_string(), status).await;
        if resolved.is_err() {
            diag::event(
                tracing::Level::ERROR,
                diag::APPROVAL_RESOLVE_FAILED,
                &[
                    ("approval_id", diag::Field::Uuid(id)),
                    (
                        "outcome",
                        diag::Field::Static(if approve { "approved" } else { "denied" }),
                    ),
                ],
            );
        }
        true
    }

    /// Approvals pending resolution right now.
    pub async fn pending_approvals(&self) -> Vec<vortex_types::ApprovalInfo> {
        self.db.list_approvals(true).await.unwrap_or_default()
    }
}

#[async_trait]
impl ApprovalGate for ApprovalManager {
    async fn request(
        &self,
        run_id: &str,
        title: &str,
        payload: &str,
        policy: ApprovalPolicy,
    ) -> Result<bool, ToolError> {
        let id = uuid::Uuid::new_v4().to_string();
        let agent = self
            .hub
            .get(run_id)
            .map(|h| h.agent())
            .unwrap_or_else(|| "agent".into());
        let persisted = self
            .db
            .add_approval(
                id.clone(),
                run_id.to_string(),
                agent.clone(),
                title.to_string(),
                payload.to_string(),
            )
            .await;
        if let Err(e) = persisted {
            // Diagnostics note only the boundary; the error text (which can
            // echo DB internals) stays on the normal path only.
            diag::event(
                tracing::Level::ERROR,
                diag::APPROVAL_PERSIST_FAILED,
                &[
                    ("approval_id", diag::Field::Uuid(id.as_str())),
                    ("run_id", diag::Field::Uuid(run_id)),
                ],
            );
            return Err(ToolError::Execution(format!("persisting approval: {e}")));
        }
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
        let approved = match policy {
            ApprovalPolicy::TimeoutSecs(secs) => {
                // 0 falls back to the manager's configured timeout; any
                // explicit value keeps the historical 5-second floor.
                let effective = if secs == 0 { self.timeout_secs } else { secs };
                let timeout = Duration::from_secs(effective.max(5));
                match tokio::time::timeout(timeout, rx).await {
                    Ok(Ok(v)) => v,
                    Ok(Err(_)) => {
                        // Sender dropped without a decision (manager shutting down).
                        let resolved = self
                            .db
                            .resolve_approval(id.clone(), ApprovalStatus::Expired)
                            .await;
                        if resolved.is_err() {
                            diag::event(
                                tracing::Level::ERROR,
                                diag::APPROVAL_RESOLVE_FAILED,
                                &[
                                    ("approval_id", diag::Field::Uuid(id.as_str())),
                                    ("outcome", diag::Field::Static("expired")),
                                ],
                            );
                        }
                        diag::event(
                            tracing::Level::WARN,
                            diag::APPROVAL_WAITER_DROPPED,
                            &[("approval_id", diag::Field::Uuid(id.as_str()))],
                        );
                        false
                    }
                    Err(_) => {
                        // Timeout. Only write `expired` if this branch still owns the
                        // sender: if `decide` removed it first, that call owns the DB
                        // write (decision delivered, or `expired` on late arrival),
                        // and a second write here could overwrite the user's choice.
                        let owned = self.pending.lock().await.remove(&id).is_some();
                        if owned {
                            let resolved = self
                                .db
                                .resolve_approval(id.clone(), ApprovalStatus::Expired)
                                .await;
                            if resolved.is_err() {
                                diag::event(
                                    tracing::Level::ERROR,
                                    diag::APPROVAL_RESOLVE_FAILED,
                                    &[
                                        ("approval_id", diag::Field::Uuid(id.as_str())),
                                        ("outcome", diag::Field::Static("expired")),
                                    ],
                                );
                            }
                        }
                        tracing::info!(approval_id = %id, "approval timed out and was auto-denied");
                        diag::event(
                            tracing::Level::INFO,
                            diag::APPROVAL_TIMEOUT,
                            &[("approval_id", diag::Field::Uuid(id.as_str()))],
                        );
                        false
                    }
                }
            }
            ApprovalPolicy::ParkUntilDecided => {
                // Parked job runs are visibly waiting: status in BOTH hub and
                // DB, only under this policy (interactive runs never change
                // status here).
                self.hub.set_status(run_id, RunStatus::WaitingApproval);
                let parked = self
                    .db
                    .update_run_status(
                        run_id.to_string(),
                        RunStatus::WaitingApproval,
                        None,
                        None,
                        None,
                    )
                    .await;
                if parked.is_err() {
                    diag::event(
                        tracing::Level::ERROR,
                        diag::DB_PERSIST_FAILED,
                        &[
                            ("op", diag::Field::Static("run_status")),
                            ("run_id", diag::Field::Uuid(run_id)),
                        ],
                    );
                }
                // No timeout: wait for the human decision, but resolve
                // promptly when the run is cancelled (graceful shutdown), so
                // a parked wait never outlives its run.
                let cancel = self.hub.get(run_id).map(|h| h.cancel.clone());
                let decision = match cancel {
                    Some(token) => tokio::select! {
                        r = rx => Some(r),
                        _ = token.cancelled() => None,
                    },
                    None => Some(rx.await),
                };
                match decision {
                    Some(Ok(v)) => {
                        // Decided (approve or deny): the run loop continues
                        // and owns every later status transition.
                        self.hub.set_status(run_id, RunStatus::Running);
                        let restored = self
                            .db
                            .update_run_status(
                                run_id.to_string(),
                                RunStatus::Running,
                                None,
                                None,
                                None,
                            )
                            .await;
                        if restored.is_err() {
                            diag::event(
                                tracing::Level::ERROR,
                                diag::DB_PERSIST_FAILED,
                                &[
                                    ("op", diag::Field::Static("run_status")),
                                    ("run_id", diag::Field::Uuid(run_id)),
                                ],
                            );
                        }
                        v
                    }
                    Some(Err(_)) => {
                        // Sender dropped without a decision (manager shutting
                        // down). The run is dying; no status restore needed.
                        let resolved = self
                            .db
                            .resolve_approval(id.clone(), ApprovalStatus::Expired)
                            .await;
                        if resolved.is_err() {
                            diag::event(
                                tracing::Level::ERROR,
                                diag::APPROVAL_RESOLVE_FAILED,
                                &[
                                    ("approval_id", diag::Field::Uuid(id.as_str())),
                                    ("outcome", diag::Field::Static("expired")),
                                ],
                            );
                        }
                        diag::event(
                            tracing::Level::WARN,
                            diag::APPROVAL_WAITER_DROPPED,
                            &[("approval_id", diag::Field::Uuid(id.as_str()))],
                        );
                        false
                    }
                    None => {
                        // Run cancelled while parked. Same single-owner rule
                        // as the timeout branch: only write `expired` if this
                        // branch still owns the sender; a concurrent `decide`
                        // owns the DB write and must never be overwritten.
                        let owned = self.pending.lock().await.remove(&id).is_some();
                        if owned {
                            let resolved = self
                                .db
                                .resolve_approval(id.clone(), ApprovalStatus::Expired)
                                .await;
                            if resolved.is_err() {
                                diag::event(
                                    tracing::Level::ERROR,
                                    diag::APPROVAL_RESOLVE_FAILED,
                                    &[
                                        ("approval_id", diag::Field::Uuid(id.as_str())),
                                        ("outcome", diag::Field::Static("expired")),
                                    ],
                                );
                            }
                        }
                        tracing::info!(approval_id = %id, "parked approval expired: run cancelled");
                        false
                    }
                }
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
        _policy: ApprovalPolicy,
    ) -> Result<bool, ToolError> {
        // Simulate the human decision delay so waits are observable.
        tokio::time::sleep(Duration::from_millis(10)).await;
        Ok(self.default)
    }
}
