//! Vortex agent engine: tool registry, permissions, orchestration,
//! scheduling, approvals, and the run loop.

pub mod approvals;
pub mod browser;
pub mod cdp;
pub mod engine;
pub mod errors;
pub mod file_state;
pub mod html;
pub mod hub;
pub mod preview;
pub mod preview_server;
pub mod prompts;
pub mod registry;
pub mod safe_http;
pub mod scheduler;
pub mod schema;
pub mod search;
pub mod tools_browser;
pub mod tools_command;
pub mod tools_delegate;
pub mod tools_files;
pub mod tools_meta;
pub mod tools_preview;
pub mod tools_preview_stop;
pub mod tools_search;
pub mod tools_write;
pub mod workspace;

pub use engine::{allowed_for_mode, build_registry, Engine, RunError, RunSpec};
pub use hub::Hub;
pub use preview::PreviewRegistry;
pub use scheduler::Scheduler;

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
