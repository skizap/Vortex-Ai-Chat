//! Run registry and event fan-out.
//!
//! Each run owns a `broadcast` channel and a bounded in-memory event log.
//! Events are seq-stamped per run handle and published on the run's channel
//! AND on every ancestor's channel, so one subscription to the root run
//! covers the whole agent tree. The log lets a (re-)subscribing SSE client
//! replay what it missed ([`Hub::replay_from`]); it lives and dies with the
//! run handle — runs never survive restarts (they are marked interrupted at
//! startup), so persisting it would buy nothing.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use vortex_types::{PlanStep, RunEvent, RunStatus, Usage};

/// Per-run replay log capacity. Oldest entries are evicted on overflow; a
/// client whose cursor predates the retained window gets a gap signal and
/// resyncs from the run snapshot instead. Note on memory: `ToolCallStarted`
/// args are schema-capped at 200 KB per string, so a full log can reach a few
/// MiB per run — bounded and acceptable for live runs only.
pub const MAX_RUN_LOG: usize = 8192;

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
    pub tx: broadcast::Sender<(u64, RunEvent)>,
    pub cancel: CancellationToken,
    /// Sequence counter for this handle's channel/log namespace. Ancestors
    /// have their own counters: the same event carries a different seq on
    /// each channel, and every stream is internally consistent.
    seq: AtomicU64,
    /// Bounded replay log, kept in lockstep with the broadcast channel.
    log: Mutex<VecDeque<(u64, RunEvent)>>,
}

impl RunHandle {
    pub fn status(&self) -> RunStatus {
        self.meta
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .status
    }
    pub fn agent(&self) -> String {
        self.meta
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .agent
            .clone()
    }

    /// Seq-stamp, log, and broadcast one event as a single serialized step.
    /// Allocation + log insert + send all happen under the log lock, so two
    /// concurrent publishers can never appear in a different order in the log
    /// versus the live stream — a subscriber that replays and then tails the
    /// broadcast sees one coherent sequence (duplicates possible, gaps not).
    fn publish(&self, event: RunEvent) {
        let mut log = self
            .log
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Sequences start at 1 so `since = 0` replays the whole log.
        let seq = self.seq.fetch_add(1, Ordering::SeqCst) + 1;
        if log.len() >= MAX_RUN_LOG {
            log.pop_front();
        }
        log.push_back((seq, event.clone()));
        // Ignore send errors: no subscriber means nobody is watching.
        let _ = self.tx.send((seq, event));
    }

    /// Buffered events with `seq > since`, plus the oldest and newest seq
    /// currently retained (`None` when the log is empty).
    fn replay_from(&self, since: u64) -> (Vec<(u64, RunEvent)>, Option<u64>, Option<u64>) {
        let log = self
            .log
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let oldest = log.front().map(|(s, _)| *s);
        let newest = log.back().map(|(s, _)| *s);
        let events = log
            .iter()
            .filter(|(s, _)| *s > since)
            .cloned()
            .collect::<Vec<_>>();
        (events, oldest, newest)
    }
}

impl Hub {
    pub fn new() -> Self {
        Self {
            runs: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Register a run; fields mirror [`RunMeta`] by design.
    #[allow(clippy::too_many_arguments)]
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
            seq: AtomicU64::new(0),
            log: Mutex::new(VecDeque::new()),
        });
        self.runs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id.to_string(), handle.clone());
        handle
    }

    pub fn get(&self, id: &str) -> Option<Arc<RunHandle>> {
        self.runs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(id)
            .cloned()
    }

    pub fn remove(&self, id: &str) {
        self.runs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(id);
    }

    pub fn subscribe(&self, id: &str) -> Option<broadcast::Receiver<(u64, RunEvent)>> {
        self.get(id).map(|h| h.tx.subscribe())
    }

    /// Replay buffered events for a run: everything with `seq > since` in
    /// publication order, plus the oldest and newest seq still retained (both
    /// `None` when the log is empty). `since` below the retained window means
    /// the caller has an unrecoverable gap and should resync from the run
    /// snapshot; `since` above `newest` is a future cursor with nothing new
    /// yet.
    pub fn replay_from(
        &self,
        run_id: &str,
        since: u64,
    ) -> (Vec<(u64, RunEvent)>, Option<u64>, Option<u64>) {
        match self.get(run_id) {
            Some(handle) => handle.replay_from(since),
            None => (Vec::new(), None, None),
        }
    }

    /// Publish to the run's channel and to all ancestors. Each handle
    /// seq-stamps the event into its own namespace (see [`RunHandle::publish`]).
    pub fn publish(&self, run_id: &str, event: RunEvent) {
        let mut current = run_id.to_string();
        let mut depth_left = 16; // tree depth guard
        loop {
            let Some(handle) = self.get(&current) else {
                return;
            };
            handle.publish(event.clone());
            depth_left -= 1;
            if depth_left == 0 {
                return;
            }
            let parent = handle
                .meta
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .parent_run_id
                .clone();
            match parent {
                Some(p) => current = p,
                None => return,
            }
        }
    }

    pub fn set_status(&self, run_id: &str, status: RunStatus) {
        if let Some(handle) = self.get(run_id) {
            handle
                .meta
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .status = status;
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
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .filter(|h| {
                let s = h
                    .meta
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .status;
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
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .map(|h| {
                let m = h
                    .meta
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn text_event(run_id: &str, delta: &str) -> RunEvent {
        RunEvent::TextDelta {
            run_id: run_id.to_string(),
            delta: delta.to_string(),
        }
    }

    fn registered_hub(run_id: &str) -> Hub {
        let hub = Hub::new();
        hub.register(
            run_id,
            None,
            None,
            "agent",
            "task",
            "model",
            0,
            CancellationToken::new(),
        );
        hub
    }

    #[test]
    fn replay_returns_only_events_after_since_in_order() {
        let hub = registered_hub("r1");
        for i in 0..10 {
            hub.publish("r1", text_event("r1", &format!("d{i}")));
        }
        // Full replay from 0: everything, in publication order, seqs 1..=10.
        let (all, oldest, newest) = hub.replay_from("r1", 0);
        assert_eq!(oldest, Some(1));
        assert_eq!(newest, Some(10));
        assert_eq!(all.len(), 10);
        assert_eq!(
            all.iter().map(|(s, _)| *s).collect::<Vec<_>>(),
            (1..=10).collect::<Vec<_>>()
        );
        // since=7 → exactly seq 8,9,10.
        let (tail, oldest, _) = hub.replay_from("r1", 7);
        assert_eq!(oldest, Some(1));
        assert_eq!(
            tail.iter().map(|(s, _)| *s).collect::<Vec<_>>(),
            vec![8, 9, 10]
        );
        // since beyond the newest seq (a future cursor) → empty, window intact.
        let (none, oldest, newest) = hub.replay_from("r1", 10);
        assert!(none.is_empty());
        assert_eq!(oldest, Some(1));
        assert_eq!(newest, Some(10));
        // Unknown run → empty, no retained window.
        let (none, oldest, newest) = hub.replay_from("missing", 0);
        assert!(none.is_empty());
        assert_eq!(oldest, None);
        assert_eq!(newest, None);
    }

    #[test]
    fn overflow_evicts_oldest_and_advances_oldest_retained() {
        let hub = registered_hub("r1");
        let total = MAX_RUN_LOG + 25;
        for i in 0..total {
            hub.publish("r1", text_event("r1", &format!("d{i}")));
        }
        let (all, oldest, newest) = hub.replay_from("r1", 0);
        // Oldest 25 evicted; retained window starts at seq 26.
        assert_eq!(oldest, Some(26));
        assert_eq!(newest, Some(total as u64));
        assert_eq!(all.len(), MAX_RUN_LOG);
        assert_eq!(all.first().map(|(s, _)| *s), Some(26));
        assert_eq!(all.last().map(|(s, _)| *s), Some(total as u64));
        // A cursor below the retained window still replays what is there;
        // the caller detects the gap via `oldest_retained > since + 1`.
        let (from20, oldest, _) = hub.replay_from("r1", 20);
        assert_eq!(oldest, Some(26));
        assert_eq!(from20.first().map(|(s, _)| *s), Some(26));
    }

    #[test]
    fn ancestors_get_their_own_seq_namespace() {
        let hub = Hub::new();
        hub.register(
            "root",
            None,
            None,
            "coord",
            "t",
            "m",
            0,
            CancellationToken::new(),
        );
        hub.register(
            "child",
            Some("root".into()),
            None,
            "sub",
            "t",
            "m",
            1,
            CancellationToken::new(),
        );
        hub.publish("root", text_event("root", "a"));
        hub.publish("child", text_event("child", "b"));
        hub.publish("child", text_event("child", "c"));

        // Root sees all three events with its own 1..=3 numbering.
        let (root_events, oldest, newest) = hub.replay_from("root", 0);
        assert_eq!(oldest, Some(1));
        assert_eq!(newest, Some(3));
        assert_eq!(
            root_events.iter().map(|(s, _)| *s).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        assert_eq!(root_events[1].1.run_id(), "child");
        // Child sees only its two events, numbered 1..=2 in its own namespace.
        let (child_events, oldest, newest) = hub.replay_from("child", 0);
        assert_eq!(oldest, Some(1));
        assert_eq!(newest, Some(2));
        assert_eq!(
            child_events.iter().map(|(s, _)| *s).collect::<Vec<_>>(),
            vec![1, 2]
        );
    }

    #[tokio::test]
    async fn subscriber_sees_live_events_with_seqs() {
        let hub = registered_hub("r1");
        let mut rx = hub.subscribe("r1").expect("registered");
        hub.publish("r1", text_event("r1", "live"));
        let (seq, ev) = rx.recv().await.unwrap();
        assert_eq!(seq, 1);
        assert_eq!(ev.run_id(), "r1");
    }
}
