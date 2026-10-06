//! Per-file locking and cross-run ownership tracking.
//!
//! Purpose: concurrent agents must not silently overwrite each other's
//! work. Rules enforced:
//! - Writes to the same path serialize on an async mutex.
//! - A sub-agent may only modify a file "owned" (last written) by itself,
//!   an unowned file, or nothing; modifying another agent's file fails
//!   with a conflict the coordinator must resolve. The coordinator (root
//!   run) may integrate/overwrite any file.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

#[derive(Default)]
pub struct FileState {
    locks: Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>,
    owners: Mutex<HashMap<PathBuf, String>>,
}

pub struct PathLock {
    path: PathBuf,
    lock: Arc<tokio::sync::Mutex<()>>,
}

impl PathLock {
    /// The lock guard holder; call `.lock()` on the wrapped value.
    pub fn lock(&self) -> &tokio::sync::Mutex<()> {
        &self.lock
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl FileState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Get (or create) the async lock for a path.
    pub fn lock_for(&self, path: &Path) -> Arc<tokio::sync::Mutex<()>> {
        let mut map = self.locks.lock().expect("locks map poisoned");
        map.entry(path.to_path_buf()).or_default().clone()
    }

    /// Who last wrote this file (run id), if anyone in this server session.
    pub fn owner_of(&self, path: &Path) -> Option<String> {
        self.owners
            .lock()
            .expect("owners map poisoned")
            .get(path)
            .cloned()
    }

    /// Record `run_id` as the current writer of `path`.
    pub fn record_write(&self, path: &Path, run_id: &str) {
        self.owners
            .lock()
            .expect("owners map poisoned")
            .insert(path.to_path_buf(), run_id.to_string());
    }

    /// Conflict check for sub-agent writes. Returns Err with a helpful
    /// message when another run owns the file.
    pub fn check_write_conflict(
        &self,
        path: &Path,
        run_id: &str,
        root_run_id: &str,
    ) -> Result<(), String> {
        if run_id == root_run_id {
            return Ok(()); // coordinator may always integrate
        }
        match self.owner_of(path) {
            Some(owner) if owner != run_id => Err(format!(
                "file '{}' is owned by another agent run ({owner}). Report the change to the \
                 coordinator instead of overwriting shared work.",
                path.display()
            )),
            _ => Ok(()),
        }
    }

    /// Drop lock/ownership entries for a finished run's files.
    pub fn cleanup_run(&self, run_id: &str) {
        self.owners
            .lock()
            .expect("owners map poisoned")
            .retain(|_, owner| owner != run_id);
        // Keep the per-path locks; they are cheap and shared.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conflict_detection() {
        let fs = FileState::new();
        let p = Path::new("/tmp/x");
        assert!(fs.check_write_conflict(p, "agent-a", "root").is_ok());
        fs.record_write(p, "agent-a");
        assert!(fs.check_write_conflict(p, "agent-b", "root").is_err());
        assert!(fs.check_write_conflict(p, "agent-a", "root").is_ok());
        assert!(fs.check_write_conflict(p, "agent-b", "some-root").is_err());
        assert!(fs.check_write_conflict(p, "agent-a", "agent-a").is_ok());
        fs.cleanup_run("agent-a");
        assert!(fs.owner_of(p).is_none());
    }
}
