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

    pub async fn list_conversations(&self) -> Result<Vec<ConversationSummary>> {
        Ok(self
            .with_conn(|c| {
                let mut st = c.prepare(
                    "SELECT id, title, mode, workspace, created_at, updated_at
                     FROM conversations ORDER BY updated_at DESC",
                )?;
                let rows = st
                    .query_map([], row_to_conversation)?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                Ok(rows)
            })
            .await?)
    }

    pub async fn search_conversations(&self, q: String) -> Result<Vec<ConversationSummary>> {
        Ok(self
            .with_conn(move |c| {
                let mut st = c.prepare(
                    "SELECT DISTINCT c.id, c.title, c.mode, c.workspace, c.created_at, c.updated_at
                     FROM conversations c
                     LEFT JOIN messages m ON m.conversation_id = c.id
                     WHERE c.title LIKE ?1 OR m.content LIKE ?1
                     ORDER BY c.updated_at DESC",
                )?;
                let like = format!("%{q}%");
                let rows = st
                    .query_map(rusqlite::params![like], row_to_conversation)?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                Ok(rows)
            })
            .await?)
    }

    pub async fn rename_conversation(&self, id: String, title: String) -> Result<bool> {
        Ok(self
            .with_conn(move |c| {
                Ok(c.execute(
                    "UPDATE conversations SET title = ?2, updated_at = ?3 WHERE id = ?1",
                    rusqlite::params![id, title, now()],
                )? > 0)
            })
            .await?)
    }

    pub async fn delete_conversation(&self, id: String) -> Result<bool> {
        Ok(self
            .with_conn(move |c| {
                c.execute("DELETE FROM messages WHERE conversation_id = ?1", rusqlite::params![id])?;
                c.execute("DELETE FROM runs WHERE conversation_id = ?1", rusqlite::params![id])?;
                Ok(c.execute("DELETE FROM conversations WHERE id = ?1", rusqlite::params![id])? > 0)
            })
            .await?)
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
