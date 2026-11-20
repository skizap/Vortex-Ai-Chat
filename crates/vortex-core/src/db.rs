//! SQLite persistence: async wrapper around a blocking `rusqlite` connection.
//!
//! All operations run via `tokio::task::spawn_blocking` so the Tokio runtime is
//! never blocked on SQLite. SQLite is configured in WAL mode for safe
//! concurrent reads/writes from the async runtime.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use crate::error::{CoreError, Result};

/// A single shared SQLite connection guarded by a mutex. Queries are short;
/// spawn_blocking keeps them off the async workers.
#[derive(Clone)]
pub struct Db {
    conn: Arc<Mutex<rusqlite::Connection>>,
    /// Attachment storage root, registered once at startup so conversation
    /// deletion can clean up files after the row cascade commits.
    pub(crate) attachments_root: Arc<OnceLock<PathBuf>>,
}

impl Db {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        if let Some(parent) = path.as_ref().parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = rusqlite::Connection::open(path.as_ref())?;
        // A second process touching the DB (sqlite3 CLI, a stray second
        // server) must produce a short wait, not an instant SQLITE_BUSY.
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        let db = Self {
            conn: Arc::new(Mutex::new(conn)),
            attachments_root: Arc::new(OnceLock::new()),
        };
        db.migrate()?;
        Ok(db)
    }

    /// Register the attachment storage root (idempotent; first call wins).
    /// Enables best-effort file cleanup inside `delete_conversation`.
    pub fn set_attachments_root(&self, root: PathBuf) {
        let _ = self.attachments_root.set(root);
    }

    /// Run one blocking closure against the connection.
    pub async fn with_conn<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&rusqlite::Connection) -> std::result::Result<T, rusqlite::Error>
            + Send
            + 'static,
    {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            // Poisoning recovery: the connection itself is not invalidated by
            // a panic in an unrelated caller; failing every later query would
            // turn one bug into a dead server.
            let guard = conn.lock().unwrap_or_else(|e| e.into_inner());
            f(&guard).map_err(CoreError::Db)
        })
        .await
        .map_err(|e| {
            // Diagnostics carry only the fixed category: a JoinError's display
            // can embed a panic payload, which must never reach diagnostics.
            crate::diag::event(
                tracing::Level::ERROR,
                crate::diag::DB_JOIN_FAILED,
                &[("panicked", crate::diag::Field::Flag(e.is_panic()))],
            );
            CoreError::Config(format!("db task join error: {e}"))
        })?
    }

    /// Synchronous access for one-shot startup tasks only.
    pub fn raw_connection(&self) -> std::sync::MutexGuard<'_, rusqlite::Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn migrate(&self) -> Result<()> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        conn.execute_batch(SCRIPT)?;
        // `CREATE TABLE IF NOT EXISTS` never alters an existing `messages`
        // table, so the attachments column is added explicitly when missing.
        let has_attachments = conn
            .prepare("SELECT 1 FROM pragma_table_info('messages') WHERE name = 'attachments'")?
            .exists([])?;
        if !has_attachments {
            conn.execute_batch(
                "ALTER TABLE messages ADD COLUMN attachments TEXT NOT NULL DEFAULT '[]';",
            )?;
        }
        Ok(())
    }
}

const SCRIPT: &str = r#"
CREATE TABLE IF NOT EXISTS conversations (
    id          TEXT PRIMARY KEY,
    title       TEXT NOT NULL,
    mode        TEXT NOT NULL DEFAULT 'chat',
    workspace   TEXT,
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS messages (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    conversation_id  TEXT NOT NULL,
    role            TEXT NOT NULL,
    content         TEXT NOT NULL,
    tool_calls      TEXT NOT NULL DEFAULT '[]',
    created_at      TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_messages_conversation ON messages(conversation_id);

CREATE TABLE IF NOT EXISTS settings (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS runs (
    id              TEXT PRIMARY KEY,
    conversation_id TEXT,
    parent_run_id   TEXT,
    agent           TEXT NOT NULL,
    task            TEXT NOT NULL DEFAULT '',
    model           TEXT NOT NULL DEFAULT '',
    status          TEXT NOT NULL,
    error           TEXT,
    usage_json      TEXT,
    plan_json       TEXT NOT NULL DEFAULT '[]',
    depth           INTEGER NOT NULL DEFAULT 0,
    created_at      TEXT NOT NULL,
    updated_at      TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_runs_conversation ON runs(conversation_id);
CREATE INDEX IF NOT EXISTS idx_runs_parent ON runs(parent_run_id);

CREATE TABLE IF NOT EXISTS tool_calls (
    id          TEXT PRIMARY KEY,
    run_id      TEXT NOT NULL,
    tool        TEXT NOT NULL,
    args_json   TEXT NOT NULL DEFAULT '{}',
    ok          INTEGER NOT NULL DEFAULT 0,
    summary     TEXT NOT NULL DEFAULT '',
    created_at  TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_tool_calls_run ON tool_calls(run_id);

CREATE TABLE IF NOT EXISTS approvals (
    id         TEXT PRIMARY KEY,
    run_id     TEXT NOT NULL,
    agent      TEXT NOT NULL DEFAULT '',
    title      TEXT NOT NULL,
    payload    TEXT NOT NULL,
    status     TEXT NOT NULL,
    created_at TEXT NOT NULL,
    decided_at TEXT
);
CREATE INDEX IF NOT EXISTS idx_approvals_run ON approvals(run_id);

CREATE TABLE IF NOT EXISTS checkpoints (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    conversation_id TEXT NOT NULL,
    run_id          TEXT NOT NULL,
    path            TEXT NOT NULL,
    original        TEXT,
    created_at      TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_checkpoints_conversation ON checkpoints(conversation_id);

CREATE TABLE IF NOT EXISTS attachments (
    id              TEXT PRIMARY KEY,
    conversation_id TEXT NOT NULL,
    kind            TEXT NOT NULL,
    origin          TEXT NOT NULL,
    status          TEXT NOT NULL,
    filename        TEXT NOT NULL,
    media_type      TEXT NOT NULL,
    size_bytes      INTEGER NOT NULL DEFAULT 0,
    storage_path    TEXT NOT NULL DEFAULT '',
    job_id          TEXT,
    error           TEXT,
    created_at      TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_attachments_conversation ON attachments(conversation_id);

CREATE TABLE IF NOT EXISTS video_jobs (
    id              TEXT PRIMARY KEY,
    provider_job_id TEXT,
    conversation_id TEXT NOT NULL,
    attachment_id   TEXT NOT NULL,
    model           TEXT NOT NULL,
    status          TEXT NOT NULL,
    prompt          TEXT NOT NULL DEFAULT '',
    params_json     TEXT NOT NULL DEFAULT '{}',
    error           TEXT,
    created_at      TEXT NOT NULL,
    updated_at      TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_video_jobs_conversation ON video_jobs(conversation_id);
CREATE INDEX IF NOT EXISTS idx_video_jobs_status ON video_jobs(status);

CREATE TABLE IF NOT EXISTS file_annotations (
    attachment_id    TEXT PRIMARY KEY,
    annotations_json TEXT NOT NULL,
    created_at       TEXT NOT NULL
);
"#;

/// Mark runs that were live when the process died as honestly interrupted,
/// and expire the pending approvals that belonged to them (their in-memory
/// waiters are gone, so leaving them "pending" would surface undismissable
/// ghost approvals after a restart).
/// Never replays anything consequential automatically.
/// Returns `(runs_marked, approvals_expired)`.
pub fn mark_interrupted_on_startup(
    conn: &rusqlite::Connection,
) -> rusqlite::Result<(usize, usize)> {
    let ts = now();
    let runs = conn.execute(
        "UPDATE runs SET status = 'interrupted', updated_at = ?1
         WHERE status IN ('queued','running','waiting_approval')",
        rusqlite::params![ts],
    )?;
    let approvals = conn.execute(
        "UPDATE approvals SET status = 'expired', decided_at = ?1
         WHERE status = 'pending'
           AND run_id IN (SELECT id FROM runs WHERE status = 'interrupted')",
        rusqlite::params![ts],
    )?;
    Ok((runs, approvals))
}

pub fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_marks_runs_interrupted_and_expires_their_approvals() {
        let dir = tempfile::tempdir().unwrap();
        let conn = rusqlite::Connection::open(dir.path().join("t.db")).unwrap();
        conn.execute_batch(SCRIPT).unwrap();
        let ts = now();
        conn.execute(
            "INSERT INTO runs (id, agent, task, model, status, created_at, updated_at)
             VALUES ('r1','a','','','running',?1,?1)",
            rusqlite::params![ts],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO approvals (id, run_id, agent, title, payload, status, created_at)
             VALUES ('a1','r1','a','t','p','pending',?1)",
            rusqlite::params![ts],
        )
        .unwrap();
        // An approval from an already-finished run must not be touched.
        conn.execute(
            "INSERT INTO runs (id, agent, task, model, status, created_at, updated_at)
             VALUES ('r2','a','','','completed',?1,?1)",
            rusqlite::params![ts],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO approvals (id, run_id, agent, title, payload, status, created_at)
             VALUES ('a2','r2','a','t','p','approved',?1)",
            rusqlite::params![ts],
        )
        .unwrap();

        let (runs, approvals) = mark_interrupted_on_startup(&conn).unwrap();
        assert_eq!(runs, 1);
        assert_eq!(approvals, 1);
        let status: String = conn
            .query_row("SELECT status FROM approvals WHERE id='a1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(status, "expired");
        let status: String = conn
            .query_row("SELECT status FROM approvals WHERE id='a2'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(status, "approved");
        let decided: Option<String> = conn
            .query_row("SELECT decided_at FROM approvals WHERE id='a1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert!(decided.is_some(), "expired approvals get a decided_at");
    }

    #[test]
    fn migrate_adds_attachments_column_to_old_messages_table() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old.db");
        // Fixture: the pre-attachments schema, with an existing row.
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE messages (
                     id              INTEGER PRIMARY KEY AUTOINCREMENT,
                     conversation_id TEXT NOT NULL,
                     role            TEXT NOT NULL,
                     content         TEXT NOT NULL,
                     tool_calls      TEXT NOT NULL DEFAULT '[]',
                     created_at      TEXT NOT NULL
                 );
                 INSERT INTO messages (conversation_id, role, content, created_at)
                 VALUES ('c1', 'user', 'legacy message', '2026-01-01T00:00:00.000Z');",
            )
            .unwrap();
        }
        // Opening through Db runs the migration.
        let db = Db::open(&path).unwrap();
        let conn = db.raw_connection();
        let has: bool = conn
            .prepare("SELECT 1 FROM pragma_table_info('messages') WHERE name='attachments'")
            .unwrap()
            .exists([])
            .unwrap();
        assert!(has, "attachments column must be added");
        let att: String = conn
            .query_row("SELECT attachments FROM messages WHERE id=1", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(att, "[]", "existing rows default to no attachments");
        let content: String = conn
            .query_row("SELECT content FROM messages WHERE id=1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(content, "legacy message", "existing data is preserved");
        // Re-opening is idempotent (no duplicate-column error).
        drop(conn);
        drop(db);
        Db::open(&path).unwrap();
    }
}
