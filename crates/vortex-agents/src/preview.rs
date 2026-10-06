//! Controlled local preview servers for user projects.
//!
//! - Static previews: a tiny loopback-only HTTP file server (pure Rust),
//!   bound to 127.0.0.1 on an ephemeral port (see `preview_server.rs`).
//! - Process previews: a user project's dev server (e.g. `npm run dev`),
//!   spawned in its own process group; killed when stopped, when the owning
//!   run ends, or when the server shuts down.
//!
//! Previews are always owned by the run that started them.

use crate::errors::ToolError;
use crate::tools_command::kill_process_group;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub struct PreviewRegistry {
    inner: Arc<Mutex<Vec<PreviewHandle>>>,
}

struct PreviewHandle {
    id: String,
    run_id: String,
    url: String,
    root: PathBuf,
    running: bool,
    abort: CancellationToken,
    child_pid: Option<u32>,
}

impl Default for PreviewRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl PreviewRegistry {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn start_static(&self, run_id: &str, root: PathBuf) -> Result<String, ToolError> {
        let id = uuid::Uuid::new_v4().to_string();
        let abort = CancellationToken::new();
        let server_abort = abort.clone();
        let serve_root = root.clone();
        let listener = std::net::TcpListener::bind("127.0.0.1:0")
            .map_err(|e| ToolError::Execution(format!("binding preview port: {e}")))?;
        let port = listener
            .local_addr()
            .map_err(|e| ToolError::Execution(format!("preview port: {e}")))?
            .port();
        let url = format!("http://127.0.0.1:{port}");
        listener
            .set_nonblocking(true)
            .map_err(|e| ToolError::Execution(format!("preview socket: {e}")))?;
        let listener = tokio::net::TcpListener::from_std(listener)
            .map_err(|e| ToolError::Execution(format!("preview socket: {e}")))?;
        let task = tokio::spawn(crate::preview_server::serve(
            listener,
            serve_root,
            server_abort,
        ));
        tokio::spawn(async move {
            let _ = task.await;
        });
        self.inner.lock().expect("previews").push(PreviewHandle {
            id: id.clone(),
            run_id: run_id.to_string(),
            url: url.clone(),
            root,
            running: true,
            abort,
            child_pid: None,
        });
        Ok(url)
    }

    pub fn attach_child(
        &self,
        run_id: &str,
        root: PathBuf,
        url: String,
        pid: u32,
        cancel: CancellationToken,
    ) {
        self.inner.lock().expect("previews").push(PreviewHandle {
            id: uuid::Uuid::new_v4().to_string(),
            run_id: run_id.to_string(),
            url,
            root,
            running: true,
            abort: cancel,
            child_pid: Some(pid),
        });
    }

    pub fn stop(&self, id: &str) -> bool {
        let mut guard = self.inner.lock().expect("previews");
        let Some(handle) = guard.iter_mut().find(|p| p.id == id) else {
            return false;
        };
        handle.abort.cancel();
        if let Some(pid) = handle.child_pid.take() {
            let pid = pid as i32;
            tokio::spawn(async move {
                kill_process_group(pid).await;
            });
        }
        handle.running = false;
        true
    }

    /// Stop everything owned by a run (used when a run finishes or is stopped).
    pub fn stop_for_run(&self, run_id: &str) {
        let ids: Vec<String> = self
            .inner
            .lock()
            .expect("previews")
            .iter()
            .filter(|p| p.run_id == run_id)
            .map(|p| p.id.clone())
            .collect();
        for id in ids {
            self.stop(&id);
        }
        self.inner
            .lock()
            .expect("previews")
            .retain(|p| p.run_id != run_id);
    }

    /// Stop all previews (server shutdown).
    pub fn stop_all(&self) {
        let ids: Vec<String> = self
            .inner
            .lock()
            .expect("previews")
            .iter()
            .map(|p| p.id.clone())
            .collect();
        for id in ids {
            self.stop(&id);
        }
        self.inner.lock().expect("previews").clear();
    }

    pub fn list(&self) -> Vec<vortex_types::PreviewInfo> {
        self.inner
            .lock()
            .expect("previews")
            .iter()
            .map(|p| vortex_types::PreviewInfo {
                id: p.id.clone(),
                run_id: p.run_id.clone(),
                url: p.url.clone(),
                root: p.root.display().to_string(),
                running: p.running,
            })
            .collect()
    }
}
