//! Message and settings queries.

use crate::db::{now, Db};
use crate::error::Result;
use vortex_types::{ChatMessage, Role, Settings, StoredMessage, ToolCall};

impl Db {
    // ---- messages ---------------------------------------------------------

    pub async fn add_message(&self, conversation_id: String, msg: ChatMessage) -> Result<i64> {
        self.with_conn(move |c| {
            let tool_calls = serde_json::to_string(&msg.tool_calls).unwrap_or_else(|_| "[]".into());
            c.execute(
                "INSERT INTO messages (conversation_id, role, content, tool_calls, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![
                    conversation_id,
                    msg.role.as_str(),
                    msg.content,
                    tool_calls,
                    now()
                ],
            )?;
            Ok(c.last_insert_rowid())
        })
        .await
    }

    pub async fn get_messages(&self, conversation_id: String) -> Result<Vec<StoredMessage>> {
        self.with_conn(move |c| {
            let mut st = c.prepare(
                "SELECT id, role, content, tool_calls, created_at
                     FROM messages WHERE conversation_id = ?1 ORDER BY id",
            )?;
            let rows = st
                .query_map(rusqlite::params![conversation_id], |r| {
                    let role: String = r.get(1)?;
                    let tc: String = r.get(3)?;
                    Ok(StoredMessage {
                        id: r.get(0)?,
                        role: serde_json::from_value(serde_json::Value::String(role))
                            .unwrap_or(Role::Assistant),
                        content: r.get(2)?,
                        tool_calls: serde_json::from_str::<Vec<ToolCall>>(&tc).unwrap_or_default(),
                        created_at: r.get(4)?,
                    })
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
    }

    // ---- settings ---------------------------------------------------------

    pub async fn load_settings(&self) -> Result<Settings> {
        self.with_conn(|c| {
            let row: Option<String> = c
                .query_row(
                    "SELECT value FROM settings WHERE key = 'settings'",
                    [],
                    |r| r.get(0),
                )
                .map(Some)
                .or_else(|e| {
                    if e == rusqlite::Error::QueryReturnedNoRows {
                        Ok(None)
                    } else {
                        Err(e)
                    }
                })?;
            match row {
                Some(json) => match serde_json::from_str::<Settings>(&json) {
                    Ok(s) => Ok(s),
                    Err(e) => {
                        // Never silently discard user data: keep the corrupt
                        // blob for inspection/recovery and say so loudly. In
                        // diagnostics mode the loud log is replaced by a
                        // fixed-category event: the serde message is
                        // attacker/blob-controlled text and must not be
                        // captured. Normal (non-diagnostic) behavior is
                        // unchanged.
                        if crate::diag::enabled() {
                            crate::diag::event(
                                tracing::Level::ERROR,
                                crate::diag::SETTINGS_CORRUPT_DEFAULTS,
                                &[],
                            );
                        } else {
                            tracing::error!(
                                error = %e,
                                "stored settings could not be parsed; falling back to defaults. \
                                 The original blob was preserved under settings key \
                                 'settings_corrupt_backup'."
                            );
                        }
                        let _ = c.execute(
                            "INSERT INTO settings (key, value) VALUES ('settings_corrupt_backup', ?1)
                             ON CONFLICT(key) DO UPDATE SET value = ?1",
                            rusqlite::params![json],
                        );
                        Ok(Settings::default())
                    }
                },
                None => Ok(Settings::default()),
            }
        })
        .await
    }

    pub async fn save_settings(&self, settings: &Settings) -> Result<()> {
        let json = serde_json::to_string(settings)
            .map_err(|e| crate::error::CoreError::Config(format!("serializing settings: {e}")))?;
        self.with_conn(move |c| {
            c.execute(
                "INSERT INTO settings (key, value) VALUES ('settings', ?1)
                 ON CONFLICT(key) DO UPDATE SET value = ?1",
                rusqlite::params![json],
            )?;
            Ok(())
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vortex_types::Mode;

    fn test_db() -> (Db, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        (Db::open(dir.path().join("test.db")).unwrap(), dir)
    }

    #[tokio::test]
    async fn conversation_roundtrip_and_search() {
        let (db, _dir) = test_db();
        let c = db
            .create_conversation("Hello", Mode::Chat, None)
            .await
            .unwrap();
        db.add_message(c.id.clone(), ChatMessage::user("deep topic about ferrets"))
            .await
            .unwrap();
        let found = db.search_conversations("ferrets".into()).await.unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, c.id);
        db.rename_conversation(c.id.clone(), "Renamed".into())
            .await
            .unwrap();
        db.delete_conversation(c.id).await.unwrap();
        assert!(db.list_conversations().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn settings_roundtrip() {
        let (db, _dir) = test_db();
        let mut s = db.load_settings().await.unwrap();
        assert_eq!(s.agents.max_concurrent, 6);
        s.model = "some/model".into();
        s.agents.max_concurrent = 8;
        db.save_settings(&s).await.unwrap();
        let s2 = db.load_settings().await.unwrap();
        assert_eq!(s2.model, "some/model");
        assert_eq!(s2.agents.max_concurrent, 8);
    }

    #[tokio::test]
    async fn corrupt_settings_fall_back_to_defaults_and_keep_backup() {
        let (db, _dir) = test_db();
        db.with_conn(|c| {
            c.execute(
                "INSERT INTO settings (key, value) VALUES ('settings', '{not valid json')",
                [],
            )?;
            Ok(())
        })
        .await
        .unwrap();
        let s = db.load_settings().await.unwrap();
        assert_eq!(s.agents.max_concurrent, 6, "defaults are used");
        let backup: String = db
            .with_conn(|c| {
                c.query_row(
                    "SELECT value FROM settings WHERE key = 'settings_corrupt_backup'",
                    [],
                    |r| r.get(0),
                )
            })
            .await
            .unwrap();
        assert_eq!(backup, "{not valid json", "corrupt blob is preserved");
    }
}
