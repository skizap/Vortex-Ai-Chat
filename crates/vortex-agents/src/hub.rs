//! Run registry and event fan-out.
//!
//! Each run owns a `broadcast` channel. Events are published on the run's
//! channel AND on every ancestor's channel, so one subscription to the root
//! run covers the whole agent tree.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use vortex_types::{PlanStep, RunEvent, RunStatus, Usage};

pub struct RunMeta {
    pub agent: String,
    pub task: String,
    pub model: String,
    pub status: RunStatus,
    pub depth: u32,
    pub usage: Usage,
    pub plan: Vec<PlanStep>,
    pub conversation_id: Option<String>,
    pub parent_run_id: Option<String>,
}

#[derive(Clone)]
pub struct Hub {
    runs: Arc<Mutex<HashMap<String, Arc<RunHandle>>>>,
}

pub struct RunHandle {
    pub id: String,
    pub meta: Mutex<RunMeta>,
    pub tx: broadcast::Sender<RunEvent>,
    pub cancel: CancellationToken,
}

impl RunHandle {
    pub fn status(&self) -> RunStatus {
        self.meta.lock().expect("run meta").status
    }
    pub fn agent(&self) -> String {
        self.meta.lock().expect("run meta").agent.clone()
    }
}

impl Hub {
    pub fn new() -> Self {
        Self {
            runs: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn register(
        &self,
        id: &str,
        parent_run_id: Option<String>,
        conversation_id: Option<String>,
        agent: &str,
        task: &str,
        model: &str,
        depth: u32,
        cancel: CancellationToken,
    ) -> Arc<RunHandle> {
        let (tx, _) = broadcast::channel(512);
        let handle = Arc::new(RunHandle {
            id: id.to_string(),
            meta: Mutex::new(RunMeta {
                agent: agent.to_string(),
                task: task.to_string(),
                model: model.to_string(),
                status: RunStatus::Queued,
                usage: Usage::default(),
                plan: Vec::new(),
                conversation_id,
                parent_run_id,
                depth,
            }),
            tx,
            cancel,
        });
        self.runs
            .lock()
            .expect("hub")
            .insert(id.to_string(), handle.clone());
        handle
    }

    pub fn get(&self, id: &str) -> Option<Arc<RunHandle>> {
        self.runs.lock().expect("hub").get(id).cloned()
    }

    pub fn remove(&self, id: &str) {
        self.runs.lock().expect("hub").remove(id);
    }

    pub fn subscribe(&self, id: &str) -> Option<broadcast::Receiver<RunEvent>> {
        self.get(id).map(|h| h.tx.subscribe())
    }

    /// Publish to the run's channel and to all ancestors.
    pub fn publish(&self, run_id: &str, event: RunEvent) {
        let mut current = run_id.to_string();
        let mut depth_left = 16; // tree depth guard
        loop {
            let Some(handle) = self.get(&current) else {
                return;
            };
            // Ignore send errors: no subscriber means nobody is watching.
            let _ = handle.tx.send(event.clone());
            depth_left -= 1;
            if depth_left == 0 {
                return;
            }
            let parent = handle.meta.lock().expect("run meta").parent_run_id.clone();
            match parent {
                Some(p) => current = p,
                None => return,
            }
        }
    }

    pub fn set_status(&self, run_id: &str, status: RunStatus) {
        if let Some(handle) = self.get(run_id) {
            handle.meta.lock().expect("run meta").status = status;
        }
    }

    pub fn cancel(&self, run_id: &str) {
        if let Some(handle) = self.get(run_id) {
            handle.cancel.cancel();
        }
    }

    /// Cancel every live run (used on server shutdown).
    pub fn cancel_all(&self) {
        let runs: Vec<Arc<RunHandle>> = self
            .runs
            .lock()
            .expect("hub")
            .values()
            .filter(|h| {
                let s = h.meta.lock().expect("run meta").status;
                !s.is_terminal()
            })
            .cloned()
            .collect();
        for h in runs {
            h.cancel.cancel();
        }
    }

    /// All runs currently registered (live registry; history is in SQLite).
    pub fn live_runs(&self) -> Vec<(String, String, RunStatus, u32)> {
        self.runs
            .lock()
            .expect("hub")
            .values()
            .map(|h| {
                let m = h.meta.lock().expect("run meta");
                (h.id.clone(), m.agent.clone(), m.status, m.depth)
            })
            .collect()
    }
}

impl Default for Hub {
    fn default() -> Self {
        Self::new()
    }
}
