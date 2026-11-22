//! Domain queries: conversations, messages, settings.
//! (Runs, tool calls, approvals and checkpoints live in `store_runs.rs`.)

use crate::db::{now, Db};
use crate::error::Result;
use vortex_types::{ConversationSummary, Mode};

fn parse_mode(s: &str) -> Mode {
    serde_json::from_value(serde_json::Value::String(s.to_string())).unwrap_or_default()
}

fn row_to_conversation(row: &rusqlite::Row<'_>) -> rusqlite::Result<ConversationSummary> {
    Ok(ConversationSummary {
        id: row.get(0)?,
        title: row.get(1)?,
        mode: parse_mode(&row.get::<_, String>(2)?),
        workspace: row.get(3)?,
        created_at: row.get(4)?,
        updated_at: row.get(5)?,
        // Appended last in every conversation SELECT: per-conversation model
        // preference (NULL = inherit the Settings default at send time).
        model: row.get(6)?,
    })
}

/// Escape LIKE wildcards so user input matches literally (used with
/// `ESCAPE '\'`).
fn escape_like(q: &str) -> String {
    let mut out = String::with_capacity(q.len());
    for ch in q.chars() {
        match ch {
            '\\' | '%' | '_' => {
                out.push('\\');
                out.push(ch);
            }
            _ => out.push(ch),
        }
    }
    out
}

impl Db {
    pub async fn create_conversation(
        &self,
        title: impl Into<String> + Send + 'static,
        mode: Mode,
        workspace: Option<String>,
    ) -> Result<ConversationSummary> {
        self.create_conversation_with_model(title, mode, workspace, None)
            .await
    }

    pub async fn create_conversation_with_model(
        &self,
        title: impl Into<String> + Send + 'static,
        mode: Mode,
        workspace: Option<String>,
        model: Option<String>,
    ) -> Result<ConversationSummary> {
        let id = uuid::Uuid::new_v4().to_string();
        let title = title.into();
        let ts = now();
        let out = ConversationSummary {
            id: id.clone(),
            title: title.clone(),
            mode,
            workspace: workspace.clone(),
            created_at: ts.clone(),
            updated_at: ts.clone(),
            model: model.clone(),
        };
        self.with_conn(move |c| {
            // A conversation created with the default placeholder title can
            // still be auto-named later; any other title was chosen by the
            // user and is treated as manual from the start.
            c.execute(
                "INSERT INTO conversations (id, title, mode, workspace, created_at, updated_at, title_origin, model)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?5,
                          CASE WHEN ?2 = ?6 THEN 'placeholder' ELSE 'manual' END, ?7)",
                rusqlite::params![id, title, mode.as_str(), workspace, ts, crate::title::PLACEHOLDER_TITLE, model],
            )?;
            Ok(())
        })
        .await?;
        Ok(out)
    }

    pub async fn get_conversation(&self, id: String) -> Result<Option<ConversationSummary>> {
        self.with_conn(move |c| {
            let mut st = c.prepare(
                "SELECT id, title, mode, workspace, created_at, updated_at, model
                     FROM conversations WHERE id = ?1",
            )?;
            let row = st
                .query_map(rusqlite::params![id], row_to_conversation)?
                .next()
                .transpose()?;
            Ok(row)
        })
        .await
    }

    pub async fn list_conversations(&self) -> Result<Vec<ConversationSummary>> {
        self.with_conn(|c| {
            let mut st = c.prepare(
                "SELECT id, title, mode, workspace, created_at, updated_at, model
                     FROM conversations ORDER BY updated_at DESC",
            )?;
            let rows = st
                .query_map([], row_to_conversation)?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
    }

    pub async fn search_conversations(&self, q: String) -> Result<Vec<ConversationSummary>> {
        self.with_conn(move |c| {
            let mut st = c.prepare(
                r#"SELECT DISTINCT c.id, c.title, c.mode, c.workspace, c.created_at, c.updated_at, c.model
                     FROM conversations c
                     LEFT JOIN messages m ON m.conversation_id = c.id
                     WHERE c.title LIKE ?1 ESCAPE '\' OR m.content LIKE ?1 ESCAPE '\'
                     ORDER BY c.updated_at DESC"#,
            )?;
            let like = format!("%{}%", escape_like(&q));
            let rows = st
                .query_map(rusqlite::params![like], row_to_conversation)?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
    }

    pub async fn rename_conversation(&self, id: String, title: String) -> Result<bool> {
        self.with_conn(move |c| {
            // Manual renames always win: mark the origin so automatic naming
            // can never overwrite it later, even if the user typed the exact
            // placeholder title.
            Ok(c.execute(
                "UPDATE conversations SET title = ?2, updated_at = ?3, title_origin = 'manual'
                  WHERE id = ?1",
                rusqlite::params![id, title, now()],
            )? > 0)
        })
        .await
    }

    /// Conditional automatic title write: succeeds only while the
    /// conversation is still at its placeholder origin, so a manual rename
    /// (past or concurrent) is never clobbered. Returns whether it wrote.
    pub async fn auto_title_conversation(&self, id: String, title: String) -> Result<bool> {
        self.with_conn(move |c| {
            Ok(c.execute(
                "UPDATE conversations SET title = ?2, updated_at = ?3, title_origin = 'automatic'
                  WHERE id = ?1 AND title_origin = 'placeholder'",
                rusqlite::params![id, title, now()],
            )? > 0)
        })
        .await
    }

    /// Current title origin (`placeholder` / `automatic` / `manual`), if the
    /// conversation exists. Internal to the server; not part of the summary
    /// DTO.
    pub async fn title_origin(&self, id: String) -> Result<Option<String>> {
        self.with_conn(move |c| {
            let mut st = c.prepare("SELECT title_origin FROM conversations WHERE id = ?1")?;
            let row = st
                .query_map(rusqlite::params![id], |r| r.get::<_, String>(0))?
                .next()
                .transpose()?;
            Ok(row)
        })
        .await
    }

    /// Delete a conversation and everything that belongs to it, atomically:
    /// messages, the run tree (including sub-agent runs), their tool calls
    /// and approvals, checkpoints, attachments, video jobs and file
    /// annotations. Previously this was three unlinked DELETEs that orphaned
    /// `tool_calls`/`approvals`/`checkpoints` rows forever and could leave a
    /// partial deletion on failure. Attachment files are removed after the
    /// transaction commits (best-effort, traced).
    pub async fn delete_conversation(&self, id: String) -> Result<bool> {
        let attachments_root = self.attachments_root.get().cloned();
        self.with_conn(move |c| {
            let tx = c.unchecked_transaction()?;
            // Collect the whole run tree (runs of this conversation plus all
            // descendant sub-agent runs, which have conversation_id NULL).
            let mut run_ids: Vec<String> = Vec::new();
            {
                let mut st = tx.prepare(
                    "WITH RECURSIVE tree(id) AS (
                         SELECT id FROM runs WHERE conversation_id = ?1
                         UNION ALL
                         SELECT r.id FROM runs r JOIN tree t ON r.parent_run_id = t.id
                     ) SELECT id FROM tree",
                )?;
                let rows = st.query_map(rusqlite::params![id], |r| r.get::<_, String>(0))?;
                for row in rows {
                    run_ids.push(row?);
                }
            }
            for rid in &run_ids {
                tx.execute(
                    "DELETE FROM tool_calls WHERE run_id = ?1",
                    rusqlite::params![rid],
                )?;
                tx.execute(
                    "DELETE FROM approvals WHERE run_id = ?1",
                    rusqlite::params![rid],
                )?;
                tx.execute(
                    "DELETE FROM checkpoints WHERE run_id = ?1",
                    rusqlite::params![rid],
                )?;
            }
            tx.execute(
                "DELETE FROM checkpoints WHERE conversation_id = ?1",
                rusqlite::params![id],
            )?;
            tx.execute(
                "DELETE FROM file_annotations WHERE attachment_id IN
                     (SELECT id FROM attachments WHERE conversation_id = ?1)",
                rusqlite::params![id],
            )?;
            tx.execute(
                "DELETE FROM video_jobs WHERE conversation_id = ?1",
                rusqlite::params![id],
            )?;
            tx.execute(
                "DELETE FROM attachments WHERE conversation_id = ?1",
                rusqlite::params![id],
            )?;
            tx.execute(
                "DELETE FROM messages WHERE conversation_id = ?1",
                rusqlite::params![id],
            )?;
            for rid in &run_ids {
                tx.execute("DELETE FROM runs WHERE id = ?1", rusqlite::params![rid])?;
            }
            let deleted = tx.execute(
                "DELETE FROM conversations WHERE id = ?1",
                rusqlite::params![id],
            )? > 0;
            tx.commit()?;
            // Best-effort file cleanup after the rows are gone. The directory
            // name is the conversation UUID (validated), never user input.
            if deleted {
                if let Some(root) = attachments_root {
                    if uuid::Uuid::parse_str(&id).is_ok() {
                        let dir = root.join(&id);
                        if dir.exists() {
                            if let Err(e) = std::fs::remove_dir_all(&dir) {
                                tracing::warn!(
                                    conversation_id = %id,
                                    error = %e,
                                    "could not remove attachment files for deleted conversation"
                                );
                            }
                        }
                    }
                }
            }
            Ok(deleted)
        })
        .await
    }

    pub async fn touch_conversation(&self, id: String) -> Result<()> {
        self.with_conn(move |c| {
            c.execute(
                "UPDATE conversations SET updated_at = ?2 WHERE id = ?1",
                rusqlite::params![id, now()],
            )?;
            Ok(())
        })
        .await
    }

    /// Set (or with `None`, clear back to "inherit the Settings default") the
    /// per-conversation model preference. Affects future sends only and never
    /// bumps `updated_at`: changing the model must not reorder the sidebar.
    pub async fn set_conversation_model(&self, id: String, model: Option<String>) -> Result<bool> {
        self.with_conn(move |c| {
            Ok(c.execute(
                "UPDATE conversations SET model = ?2 WHERE id = ?1",
                rusqlite::params![id, model],
            )? > 0)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::now;
    use vortex_types::{ApprovalStatus, ChatMessage, Mode, RunInfo, RunStatus, ToolCallRecord};

    fn test_db() -> (Db, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        (Db::open(dir.path().join("test.db")).unwrap(), dir)
    }

    fn run_info(id: &str, conv: Option<&str>, parent: Option<&str>) -> RunInfo {
        RunInfo {
            id: id.into(),
            parent_run_id: parent.map(str::to_string),
            conversation_id: conv.map(str::to_string),
            agent: "a".into(),
            task: "".into(),
            model: "m".into(),
            status: RunStatus::Completed,
            error: None,
            usage: None,
            plan: Vec::new(),
            depth: 0,
            created_at: now(),
            updated_at: now(),
        }
    }

    #[tokio::test]
    async fn delete_conversation_cascades_all_related_rows() {
        let (db, _dir) = test_db();
        let c = db
            .create_conversation("conv", Mode::Coding, None)
            .await
            .unwrap();
        db.add_message(c.id.clone(), ChatMessage::user("hi"))
            .await
            .unwrap();
        // Root run + a sub-agent run (conversation_id NULL, linked via parent).
        db.upsert_run(&run_info("r1", Some(&c.id), None))
            .await
            .unwrap();
        db.upsert_run(&run_info("r1-sub", None, Some("r1")))
            .await
            .unwrap();
        db.add_tool_call(&ToolCallRecord {
            id: "tc1".into(),
            run_id: "r1".into(),
            tool: "read_file".into(),
            args: serde_json::Value::Null,
            ok: true,
            summary: "s".into(),
            created_at: now(),
        })
        .await
        .unwrap();
        db.add_tool_call(&ToolCallRecord {
            id: "tc2".into(),
            run_id: "r1-sub".into(),
            tool: "read_file".into(),
            args: serde_json::Value::Null,
            ok: true,
            summary: "s".into(),
            created_at: now(),
        })
        .await
        .unwrap();
        db.add_approval("a1".into(), "r1".into(), "a".into(), "t".into(), "p".into())
            .await
            .unwrap();
        db.add_checkpoint(c.id.clone(), "r1".into(), "/x".into(), None)
            .await
            .unwrap();

        assert!(db.delete_conversation(c.id.clone()).await.unwrap());

        let count = |sql: &'static str| {
            let db = db.clone();
            async move {
                db.with_conn(move |conn| {
                    let n: i64 = conn.query_row(sql, [], |r| r.get(0))?;
                    Ok(n)
                })
                .await
                .unwrap()
            }
        };
        assert_eq!(count("SELECT COUNT(*) FROM messages").await, 0);
        assert_eq!(count("SELECT COUNT(*) FROM runs").await, 0);
        assert_eq!(count("SELECT COUNT(*) FROM tool_calls").await, 0);
        assert_eq!(count("SELECT COUNT(*) FROM approvals").await, 0);
        assert_eq!(count("SELECT COUNT(*) FROM checkpoints").await, 0);
        assert_eq!(count("SELECT COUNT(*) FROM conversations").await, 0);
        // Deleting again reports "not found" rather than failing.
        assert!(!db.delete_conversation(c.id).await.unwrap());
    }

    #[tokio::test]
    async fn search_escapes_like_wildcards() {
        let (db, _dir) = test_db();
        let c = db
            .create_conversation("plain title", Mode::Chat, None)
            .await
            .unwrap();
        db.add_message(c.id.clone(), ChatMessage::user("literal text"))
            .await
            .unwrap();
        // '%' must match literally, not act as a wildcard matching everything.
        assert!(db
            .search_conversations("%".into())
            .await
            .unwrap()
            .is_empty());
        assert!(db
            .search_conversations("_".into())
            .await
            .unwrap()
            .is_empty());
        assert_eq!(
            db.search_conversations("literal".into())
                .await
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            db.search_conversations("plain".into()).await.unwrap().len(),
            1
        );
    }

    #[tokio::test]
    async fn get_conversation_by_id() {
        let (db, _dir) = test_db();
        let c = db.create_conversation("t", Mode::Chat, None).await.unwrap();
        let got = db.get_conversation(c.id.clone()).await.unwrap();
        assert_eq!(got.map(|g| g.id), Some(c.id));
        assert!(db.get_conversation("nope".into()).await.unwrap().is_none());
    }

    #[test]
    fn escape_like_handles_all_wildcards() {
        assert_eq!(escape_like("a%b_c\\d"), r"a\%b\_c\\d");
        assert_eq!(escape_like("plain"), "plain");
    }

    #[tokio::test]
    async fn resolve_approval_statuses_persist() {
        let (db, _dir) = test_db();
        db.add_approval("a".into(), "r".into(), "ag".into(), "t".into(), "p".into())
            .await
            .unwrap();
        db.resolve_approval("a".into(), ApprovalStatus::Expired)
            .await
            .unwrap();
        let all = db.list_approvals(false).await.unwrap();
        assert_eq!(all[0].status, ApprovalStatus::Expired);
    }

    #[tokio::test]
    async fn create_classifies_title_origin() {
        let (db, _dir) = test_db();
        let placeholder = db
            .create_conversation("New chat", Mode::Chat, None)
            .await
            .unwrap();
        assert_eq!(
            db.title_origin(placeholder.id).await.unwrap().as_deref(),
            Some("placeholder")
        );
        let named = db
            .create_conversation("Roadmap", Mode::Chat, None)
            .await
            .unwrap();
        assert_eq!(
            db.title_origin(named.id).await.unwrap().as_deref(),
            Some("manual")
        );
    }

    #[tokio::test]
    async fn auto_title_only_writes_placeholder_origin() {
        let (db, _dir) = test_db();
        let c = db
            .create_conversation("New chat", Mode::Chat, None)
            .await
            .unwrap();
        // First automatic write succeeds.
        assert!(db
            .auto_title_conversation(c.id.clone(), "Fix login bug".into())
            .await
            .unwrap());
        let got = db.get_conversation(c.id.clone()).await.unwrap().unwrap();
        assert_eq!(got.title, "Fix login bug");
        assert_eq!(
            db.title_origin(c.id.clone()).await.unwrap().as_deref(),
            Some("automatic")
        );
        // A second automatic write must not replace the first.
        assert!(!db
            .auto_title_conversation(c.id.clone(), "Something else".into())
            .await
            .unwrap());
        let got = db.get_conversation(c.id.clone()).await.unwrap().unwrap();
        assert_eq!(got.title, "Fix login bug");
    }

    #[tokio::test]
    async fn manual_rename_wins_over_auto_title_even_for_placeholder_text() {
        let (db, _dir) = test_db();
        let c = db
            .create_conversation("Draft", Mode::Chat, None)
            .await
            .unwrap();
        // User renames it to the exact placeholder text: still manual.
        assert!(db
            .rename_conversation(c.id.clone(), "New chat".into())
            .await
            .unwrap());
        assert_eq!(
            db.title_origin(c.id.clone()).await.unwrap().as_deref(),
            Some("manual")
        );
        assert!(!db
            .auto_title_conversation(c.id.clone(), "Derived".into())
            .await
            .unwrap());
        let got = db.get_conversation(c.id.clone()).await.unwrap().unwrap();
        assert_eq!(got.title, "New chat", "manual title is never clobbered");
    }

    #[tokio::test]
    async fn auto_title_unknown_conversation_is_false() {
        let (db, _dir) = test_db();
        assert!(!db
            .auto_title_conversation("missing".into(), "x".into())
            .await
            .unwrap());
        assert_eq!(db.title_origin("missing".into()).await.unwrap(), None);
    }

    #[tokio::test]
    async fn conversation_model_preference_roundtrip_and_clear() {
        let (db, _dir) = test_db();
        let c = db.create_conversation("t", Mode::Chat, None).await.unwrap();
        assert_eq!(c.model, None, "new conversations inherit until seeded");

        // Pin a model: visible in every summary read path, ordering untouched.
        assert!(db
            .set_conversation_model(c.id.clone(), Some("anthropic/claude-sonnet-4".into()))
            .await
            .unwrap());
        let got = db.get_conversation(c.id.clone()).await.unwrap().unwrap();
        assert_eq!(got.model.as_deref(), Some("anthropic/claude-sonnet-4"));
        assert_eq!(
            got.updated_at, c.updated_at,
            "changing the model must not bump updated_at"
        );
        let listed = db.list_conversations().await.unwrap();
        assert_eq!(
            listed[0].model.as_deref(),
            Some("anthropic/claude-sonnet-4")
        );
        let found = db.search_conversations("t".into()).await.unwrap();
        assert_eq!(found[0].model.as_deref(), Some("anthropic/claude-sonnet-4"));

        // The Auto constant is stored verbatim.
        assert!(db
            .set_conversation_model(c.id.clone(), Some("vortex/auto".into()))
            .await
            .unwrap());
        assert_eq!(
            db.get_conversation(c.id.clone())
                .await
                .unwrap()
                .unwrap()
                .model
                .as_deref(),
            Some("vortex/auto")
        );

        // Clearing returns to inherit-default.
        assert!(db.set_conversation_model(c.id.clone(), None).await.unwrap());
        assert!(db
            .get_conversation(c.id.clone())
            .await
            .unwrap()
            .unwrap()
            .model
            .is_none());

        // Unknown conversation reports "not found" rather than failing.
        assert!(!db
            .set_conversation_model("missing".into(), Some("x".into()))
            .await
            .unwrap());
    }
}
