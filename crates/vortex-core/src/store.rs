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
        };
        self.with_conn(move |c| {
            c.execute(
                "INSERT INTO conversations (id, title, mode, workspace, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
                rusqlite::params![id, title, mode.as_str(), workspace, ts],
            )?;
            Ok(())
        })
        .await?;
        Ok(out)
    }

    pub async fn get_conversation(&self, id: String) -> Result<Option<ConversationSummary>> {
        self.with_conn(move |c| {
            let mut st = c.prepare(
                "SELECT id, title, mode, workspace, created_at, updated_at
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
                "SELECT id, title, mode, workspace, created_at, updated_at
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
                r#"SELECT DISTINCT c.id, c.title, c.mode, c.workspace, c.created_at, c.updated_at
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
            Ok(c.execute(
                "UPDATE conversations SET title = ?2, updated_at = ?3 WHERE id = ?1",
                rusqlite::params![id, title, now()],
            )? > 0)
        })
        .await
    }

    /// Delete a conversation and everything that belongs to it, atomically:
    /// messages, the run tree (including sub-agent runs), their tool calls
    /// and approvals, and checkpoints. Previously this was three unlinked
    /// DELETEs that orphaned `tool_calls`/`approvals`/`checkpoints` rows
    /// forever and could leave a partial deletion on failure.
    pub async fn delete_conversation(&self, id: String) -> Result<bool> {
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
}
