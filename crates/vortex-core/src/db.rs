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
        // Title origin tracks how a conversation got its name: `placeholder`
        // (still "New chat"), `automatic` (derived locally from the first
        // turn) or `manual` (renamed by the user; always wins). Added
        // additively; the one-time legacy classify/backfill runs only in the
        // same pass that adds the column, so re-opening never repeats it.
        let has_title_origin = conn
            .prepare(
                "SELECT 1 FROM pragma_table_info('conversations') WHERE name = 'title_origin'",
            )?
            .exists([])?;
        if !has_title_origin {
            // One transaction for the column and the legacy classify/backfill:
            // an interrupt must never leave the column present with legacy
            // rows still misclassified as placeholders.
            let tx = conn.unchecked_transaction()?;
            tx.execute_batch(
                "ALTER TABLE conversations ADD COLUMN title_origin TEXT NOT NULL DEFAULT 'placeholder';",
            )?;
            backfill_title_origins(&tx)?;
            tx.commit()?;
        }
        // Model provenance on messages: the run that produced the message,
        // the model id sent to the provider, and the provider-reported model
        // that actually answered. All nullable — legacy rows keep NULL and
        // old replies are never inferred from timestamps.
        let has_run_id = conn
            .prepare("SELECT 1 FROM pragma_table_info('messages') WHERE name = 'run_id'")?
            .exists([])?;
        if !has_run_id {
            conn.execute_batch(
                "ALTER TABLE messages ADD COLUMN run_id TEXT;
                 ALTER TABLE messages ADD COLUMN requested_model TEXT;
                 ALTER TABLE messages ADD COLUMN serving_model TEXT;",
            )?;
        }
        if !conn
            .prepare("SELECT 1 FROM pragma_table_info('messages') WHERE name = 'reply_summary'")?
            .exists([])?
        {
            conn.execute_batch("ALTER TABLE messages ADD COLUMN reply_summary TEXT;")?;
        }
        // Set once a completion inside a run reported no usage: that run's
        // totals stay permanently partial even if later calls report usage.
        let has_usage_incomplete = conn
            .prepare("SELECT 1 FROM pragma_table_info('runs') WHERE name = 'usage_incomplete'")?
            .exists([])?;
        if !has_usage_incomplete {
            conn.execute_batch(
                "ALTER TABLE runs ADD COLUMN usage_incomplete INTEGER NOT NULL DEFAULT 0;",
            )?;
        }
        // Per-conversation model preference (Auto routing / pinned model id).
        // NULL means "inherit the Settings default at send time"; legacy rows
        // keep NULL and are seeded only when explicitly changed. Additive and
        // nullable, so old databases upgrade without a rewrite.
        let has_conv_model = conn
            .prepare("SELECT 1 FROM pragma_table_info('conversations') WHERE name = 'model'")?
            .exists([])?;
        if !has_conv_model {
            conn.execute_batch("ALTER TABLE conversations ADD COLUMN model TEXT;")?;
        }
        Ok(())
    }
}

/// One-time legacy migration (runs only when `title_origin` is first added):
/// - non-default titles were chosen by the user → `manual`
/// - exact `New chat` rows with a first user turn get a locally derived title
///   → `automatic`, without touching `updated_at` (ordering must not change)
/// - chats with no derivable first turn stay `placeholder`
fn backfill_title_origins(conn: &rusqlite::Connection) -> rusqlite::Result<()> {
    use crate::title::PLACEHOLDER_TITLE;
    conn.execute(
        "UPDATE conversations SET title_origin = 'manual' WHERE title <> ?1",
        rusqlite::params![PLACEHOLDER_TITLE],
    )?;
    let candidates: Vec<(String, String, String)> = {
        let mut st = conn.prepare(
            "SELECT c.id,
                    COALESCE((SELECT m.content FROM messages m
                              WHERE m.conversation_id = c.id AND m.role = 'user'
                              ORDER BY m.id LIMIT 1), ''),
                    COALESCE((SELECT m.attachments FROM messages m
                              WHERE m.conversation_id = c.id AND m.role = 'user'
                              ORDER BY m.id LIMIT 1), '[]')
               FROM conversations c
              WHERE c.title = ?1 AND c.title_origin = 'placeholder'",
        )?;
        let rows = st.query_map(rusqlite::params![PLACEHOLDER_TITLE], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    for (id, content, attachments_json) in candidates {
        let refs: Vec<vortex_types::AttachmentRef> =
            serde_json::from_str(&attachments_json).unwrap_or_default();
        let Some(title) = crate::title::title_for_first_turn(&content, &refs) else {
            continue;
        };
        // Conditional: only rewrite rows still at the exact placeholder, so a
        // concurrent manual rename can never be clobbered.
        conn.execute(
            "UPDATE conversations SET title = ?2, title_origin = 'automatic'
              WHERE id = ?1 AND title = ?3 AND title_origin = 'placeholder'",
            rusqlite::params![id, title, PLACEHOLDER_TITLE],
        )?;
    }
    Ok(())
}

const SCRIPT: &str = r#"
CREATE TABLE IF NOT EXISTS conversations (
    id           TEXT PRIMARY KEY,
    title        TEXT NOT NULL,
    mode         TEXT NOT NULL DEFAULT 'chat',
    workspace    TEXT,
    created_at   TEXT NOT NULL,
    updated_at   TEXT NOT NULL,
    title_origin TEXT NOT NULL DEFAULT 'placeholder'
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

    /// Fixture of the pre-title_origin schema with legacy rows covering every
    /// classification path.
    fn legacy_title_fixture(path: &std::path::Path) {
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE conversations (
                 id          TEXT PRIMARY KEY,
                 title       TEXT NOT NULL,
                 mode        TEXT NOT NULL DEFAULT 'chat',
                 workspace   TEXT,
                 created_at  TEXT NOT NULL,
                 updated_at  TEXT NOT NULL
             );
             CREATE TABLE messages (
                 id              INTEGER PRIMARY KEY AUTOINCREMENT,
                 conversation_id TEXT NOT NULL,
                 role            TEXT NOT NULL,
                 content         TEXT NOT NULL,
                 tool_calls      TEXT NOT NULL DEFAULT '[]',
                 attachments     TEXT NOT NULL DEFAULT '[]',
                 created_at      TEXT NOT NULL
             );
             INSERT INTO conversations VALUES
                 ('c-renamed','My roadmap','chat',NULL,'2026-01-01T00:00:00.000Z','2026-01-04T00:00:00.000Z'),
                 ('c-history','New chat','chat',NULL,'2026-01-01T00:00:00.000Z','2026-01-03T00:00:00.000Z'),
                 ('c-empty','New chat','chat',NULL,'2026-01-01T00:00:00.000Z','2026-01-02T00:00:00.000Z'),
                 ('c-attach','New chat','chat',NULL,'2026-01-01T00:00:00.000Z','2026-01-01T12:00:00.000Z');
             INSERT INTO messages (conversation_id, role, content, created_at) VALUES
                 ('c-history','user','First of all: fix the login bug. Then ship.','2026-01-02T00:00:00.000Z'),
                 ('c-history','user','a later message','2026-01-03T00:00:00.000Z'),
                 ('c-history','assistant','done','2026-01-03T00:00:01.000Z');
             INSERT INTO messages (conversation_id, role, content, attachments, created_at) VALUES
                 ('c-attach','user','',
                  '[{\"id\":\"a1\",\"kind\":\"video\",\"filename\":\"v.mp4\",\"media_type\":\"video/mp4\",\"status\":\"ready\"},
                    {\"id\":\"a2\",\"kind\":\"video\",\"filename\":\"w.mp4\",\"media_type\":\"video/mp4\",\"status\":\"ready\"}]',
                  '2026-01-01T12:00:00.000Z');",
        )
        .unwrap();
    }

    #[test]
    fn migrate_adds_title_origin_and_classifies_legacy_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("titles.db");
        legacy_title_fixture(&path);

        let db = Db::open(&path).unwrap();
        let conn = db.raw_connection();
        let row = |id: &str| -> (String, String, String) {
            conn.query_row(
                "SELECT title, title_origin, updated_at FROM conversations WHERE id = ?1",
                rusqlite::params![id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap()
        };

        let (title, origin, updated) = row("c-renamed");
        assert_eq!((title.as_str(), origin.as_str()), ("My roadmap", "manual"));
        assert_eq!(updated, "2026-01-04T00:00:00.000Z");

        // Backfilled from the FIRST user message, ordering untouched.
        let (title, origin, updated) = row("c-history");
        assert_eq!(title, "First of all: fix the login bug");
        assert_eq!(origin, "automatic");
        assert_eq!(
            updated, "2026-01-03T00:00:00.000Z",
            "backfill must not change updated_at (ordering)"
        );

        // Empty chat stays a placeholder.
        let (title, origin, _) = row("c-empty");
        assert_eq!(
            (title.as_str(), origin.as_str()),
            ("New chat", "placeholder")
        );

        // Attachment-only turn gets the local label.
        let (title, origin, _) = row("c-attach");
        assert_eq!(
            (title.as_str(), origin.as_str()),
            ("Review 2 videos", "automatic")
        );

        drop(conn);
        drop(db);

        // Idempotent: re-opening changes nothing.
        let db = Db::open(&path).unwrap();
        let conn = db.raw_connection();
        let (title, origin): (String, String) = conn
            .query_row(
                "SELECT title, title_origin FROM conversations WHERE id = 'c-history'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            (title.as_str(), origin.as_str()),
            ("First of all: fix the login bug", "automatic")
        );
    }

    #[test]
    fn fresh_db_has_title_origin_without_backfill_pass() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path().join("fresh.db")).unwrap();
        let conn = db.raw_connection();
        let has: bool = conn
            .prepare("SELECT 1 FROM pragma_table_info('conversations') WHERE name='title_origin'")
            .unwrap()
            .exists([])
            .unwrap();
        assert!(has, "new databases get title_origin from the create script");
    }

    #[test]
    fn migrate_adds_conversation_model_column_and_preserves_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old-model.db");
        // Fixture: a conversations table from before per-conversation model
        // preferences existed, with one legacy row.
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE conversations (
                     id          TEXT PRIMARY KEY,
                     title       TEXT NOT NULL,
                     mode        TEXT NOT NULL DEFAULT 'chat',
                     workspace   TEXT,
                     created_at  TEXT NOT NULL,
                     updated_at  TEXT NOT NULL
                 );
                 INSERT INTO conversations VALUES
                     ('c1','Legacy','chat',NULL,'2026-01-01T00:00:00.000Z','2026-01-02T00:00:00.000Z');",
            )
            .unwrap();
        }
        let db = Db::open(&path).unwrap();
        let conn = db.raw_connection();
        let has: bool = conn
            .prepare("SELECT 1 FROM pragma_table_info('conversations') WHERE name='model'")
            .unwrap()
            .exists([])
            .unwrap();
        assert!(has, "model column must be added to old databases");
        // Legacy rows inherit (NULL) rather than getting a pinned preference,
        // and their existing data is untouched.
        let (title, model): (String, Option<String>) = conn
            .query_row(
                "SELECT title, model FROM conversations WHERE id='c1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(title, "Legacy");
        assert_eq!(model, None);
        drop(conn);
        drop(db);
        // Re-opening is idempotent.
        Db::open(&path).unwrap();
    }

    #[test]
    fn migrate_adds_message_provenance_and_run_usage_flag_columns() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old.db");
        // Fixture: pre-provenance schema with existing rows.
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE messages (
                     id              INTEGER PRIMARY KEY AUTOINCREMENT,
                     conversation_id TEXT NOT NULL,
                     role            TEXT NOT NULL,
                     content         TEXT NOT NULL,
                     tool_calls      TEXT NOT NULL DEFAULT '[]',
                     attachments     TEXT NOT NULL DEFAULT '[]',
                     created_at      TEXT NOT NULL
                 );
                 CREATE TABLE runs (
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
                 INSERT INTO messages (conversation_id, role, content, created_at)
                 VALUES ('c1', 'assistant', 'legacy reply', '2026-01-01T00:00:00.000Z');
                 INSERT INTO runs (id, agent, task, model, status, created_at, updated_at)
                 VALUES ('r1', 'a', '', '', 'completed', '2026-01-01T00:00:00.000Z', '2026-01-01T00:00:00.000Z');",
            )
            .unwrap();
        }
        let db = Db::open(&path).unwrap();
        let conn = db.raw_connection();
        for column in [
            "run_id",
            "requested_model",
            "serving_model",
            "reply_summary",
        ] {
            let sql =
                format!("SELECT 1 FROM pragma_table_info('messages') WHERE name = '{column}'");
            let has: bool = conn.prepare(&sql).unwrap().exists([]).unwrap();
            assert!(has, "messages.{column} column must be added");
        }
        let has_flag: bool = conn
            .prepare("SELECT 1 FROM pragma_table_info('runs') WHERE name = 'usage_incomplete'")
            .unwrap()
            .exists([])
            .unwrap();
        assert!(has_flag, "runs.usage_incomplete column must be added");
        // Legacy rows: provenance stays NULL (never inferred), flag defaults 0.
        let provenance: (Option<String>, Option<String>, Option<String>) = conn
            .query_row(
                "SELECT run_id, requested_model, serving_model FROM messages WHERE id = 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(provenance, (None, None, None));
        let summary: Option<String> = conn
            .query_row("SELECT reply_summary FROM messages WHERE id = 1", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert!(
            summary.is_none(),
            "legacy replies must not acquire invented diagnostics"
        );
        let flag: i64 = conn
            .query_row(
                "SELECT usage_incomplete FROM runs WHERE id = 'r1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(flag, 0);
        let content: String = conn
            .query_row("SELECT content FROM messages WHERE id = 1", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(content, "legacy reply", "existing data is preserved");
        drop(conn);
        drop(db);
        // Re-opening is idempotent (no duplicate-column error).
        Db::open(&path).unwrap();
    }
}
