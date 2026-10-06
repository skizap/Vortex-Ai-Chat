//! Vortex-Ai-Chat core: XDG paths, boot configuration, SQLite persistence.

pub mod config;
pub mod db;
pub mod error;
pub mod paths;
pub mod store;
pub mod store_appr;
pub mod store_calls;
pub mod store_ckpts;
pub mod store_msgs;
pub mod store_runs;

pub use config::AppConfig;
pub use db::Db;
pub use error::{CoreError, Result};
pub use paths::Paths;
