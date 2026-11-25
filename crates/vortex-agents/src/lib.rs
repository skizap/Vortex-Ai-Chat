//! Vortex agent engine: tool registry, permissions, orchestration,
//! scheduling, approvals, and the run loop.

pub mod approvals;
pub mod browser;
pub mod cdp;
pub mod clock;
pub mod engine;
pub mod errors;
pub mod file_state;
pub mod html;
pub mod hub;
mod media_context;
pub mod preview;
pub mod preview_server;
pub mod prompts;
pub mod registry;
pub mod routing;
pub mod safe_http;
pub mod scheduler;
pub mod schema;
pub mod search;
pub mod supervisor;
pub mod tools_browser;
pub mod tools_command;
pub mod tools_delegate;
pub mod tools_files;
pub mod tools_meta;
pub mod tools_preview;
pub mod tools_preview_stop;
pub mod tools_routines;
pub mod tools_search;
pub mod tools_write;
pub mod workspace;

pub use engine::{
    allowed_for_mode, build_registry, classify_last_step, ContinuationSafety, ContinuationSeed,
    Engine, JobRunSpec, RunError, RunSpec,
};
pub use hub::Hub;
pub use preview::PreviewRegistry;
pub use scheduler::Scheduler;

/// Per-job scratch workspace root (frozen convention): `<data dir>/jobs/
/// <job_id>/workspace`. Used when a job declares no explicit workspace.
/// Respects the same XDG overrides as the rest of the app. The supervisor
/// creates it at admission; the engine's continuation path resolves the same
/// location from the job row.
pub fn job_scratch_dir(job_id: &str) -> std::path::PathBuf {
    let base = if let Some(v) = std::env::var_os("VORTEX_DATA_DIR") {
        std::path::PathBuf::from(v)
    } else {
        let home = std::env::var_os("HOME").unwrap_or_default();
        std::path::Path::new(&home).join(".local/share/vortex")
    };
    base.join("jobs").join(job_id).join("workspace")
}

/// Trash directory used by `delete_path`. Respects the same XDG overrides as
/// the rest of the app (see README.md "Storage locations").
pub fn trash_dir() -> std::path::PathBuf {
    if let Some(v) = std::env::var_os("VORTEX_DATA_DIR") {
        let p = std::path::PathBuf::from(v).join("trash");
        let _ = std::fs::create_dir_all(&p);
        return p;
    }
    let home = std::env::var_os("HOME").unwrap_or_default();
    let p = std::path::Path::new(&home).join(".local/share/vortex/trash");
    let _ = std::fs::create_dir_all(&p);
    p
}
