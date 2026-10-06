//! Global sub-agent scheduler.
//!
//! Enforces a server-side cap on concurrently running sub-agents with a
//! dynamic gate (so the user can change the limit at runtime). The limit
//! defaults to 6 and may not be set below 1; the hard ceiling is 16. Nested
//! sub-agents acquire from the same gate — the cap is truly global.
//! The coordinator/main run does NOT consume a slot: the requirement is six
//! active sub-agents *in addition to* the coordinator.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::Notify;
use vortex_types::AgentSettings;

pub struct Scheduler {
    inner: Arc<Inner>,
}

struct Inner {
    limit: AtomicUsize,
    active: AtomicUsize,
    notify: Notify,
}

/// Releases the slot when dropped — including on cancellation unwinds.
pub struct SlotGuard {
    inner: Arc<Inner>,
}

impl Drop for SlotGuard {
    fn drop(&mut self) {
        self.inner.active.fetch_sub(1, Ordering::SeqCst);
        self.inner.notify.notify_waiters();
    }
}

impl Scheduler {
    pub fn new(limit: usize) -> Self {
        Self {
            inner: Arc::new(Inner {
                limit: AtomicUsize::new(limit.clamp(1, AgentSettings::MAX_LIMIT_CEILING)),
                active: AtomicUsize::new(0),
                notify: Notify::new(),
            }),
        }
    }

    /// A lightweight clone sharing the same counters (used by tests and the
    /// engine when moving the scheduler into spawned tasks).
    pub fn cloneable(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }

    pub fn limit(&self) -> usize {
        self.inner.limit.load(Ordering::SeqCst)
    }

    pub fn set_limit(&self, limit: usize) {
        self.inner.limit.store(
            limit.clamp(1, AgentSettings::MAX_LIMIT_CEILING),
            Ordering::SeqCst,
        );
        self.inner.notify.notify_waiters();
    }

    pub fn active(&self) -> usize {
        self.inner.active.load(Ordering::SeqCst)
    }

    /// Wait for a slot. Returns a guard that releases the slot on drop,
    /// so cancellation and panics cannot leak permits.
    pub async fn acquire(&self) -> SlotGuard {
        loop {
            let limit = self.inner.limit.load(Ordering::SeqCst);
            let active = self.inner.active.load(Ordering::SeqCst);
            if active < limit {
                let prev = self.inner.active.fetch_add(1, Ordering::SeqCst);
                if prev < self.inner.limit.load(Ordering::SeqCst) {
                    return SlotGuard {
                        inner: self.inner.clone(),
                    };
                }
                self.inner.active.fetch_sub(1, Ordering::SeqCst);
            }
            let notified = self.inner.notify.notified();
            tokio::select! {
                _ = notified => {}
                _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Core acceptance test: at a limit of 6, exactly six sub-agent futures
    /// run concurrently and further ones queue until slots free up.
    #[tokio::test]
    async fn six_concurrent_slots_and_seventh_queues() {
        let scheduler = Scheduler::new(6);
        let observed_high_water = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();
        for _ in 0..10 {
            let scheduler = scheduler.cloneable();
            let high = observed_high_water.clone();
            handles.push(tokio::spawn(async move {
                let _slot = scheduler.acquire().await;
                let now = scheduler.active();
                high.fetch_max(now, Ordering::SeqCst);
                tokio::time::sleep(std::time::Duration::from_millis(120)).await;
            }));
        }
        for _ in 0..20 {
            assert!(scheduler.active() <= 6);
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        }
        for h in handles {
            h.await.unwrap();
        }
        let high = observed_high_water.load(Ordering::SeqCst);
        assert_eq!(
            high, 6,
            "expected exactly 6 concurrent sub-agents, saw {high}"
        );
        assert_eq!(scheduler.active(), 0);
    }

    #[tokio::test]
    async fn limit_can_go_below_and_above_default() {
        let scheduler = Scheduler::new(2);
        assert_eq!(scheduler.limit(), 2);
        scheduler.set_limit(10);
        assert_eq!(scheduler.limit(), 10);
        scheduler.set_limit(0); // clamped to 1
        assert_eq!(scheduler.limit(), 1);
        let big = Scheduler::new(99); // clamped to ceiling
        assert_eq!(big.limit(), 16);
    }

    #[tokio::test]
    async fn queued_waiter_gets_slot_when_released() {
        let scheduler = Scheduler::new(2);
        let slot1 = scheduler.acquire().await;
        let slot2 = scheduler.acquire().await;
        assert_eq!(scheduler.active(), 2);
        let waiter_done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let waiter = tokio::spawn({
            let s = scheduler.cloneable();
            let done = waiter_done.clone();
            async move {
                let _slot = s.acquire().await;
                done.store(true, Ordering::SeqCst);
            }
        });
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        assert!(
            !waiter_done.load(Ordering::SeqCst),
            "waiter must be queued at limit 2"
        );
        drop(slot1);
        drop(slot2);
        tokio::time::timeout(std::time::Duration::from_secs(2), waiter)
            .await
            .expect("waiter finishes")
            .unwrap();
        assert!(waiter_done.load(Ordering::SeqCst));
        assert_eq!(scheduler.active(), 0, "guards release their slots on drop");
    }
}
