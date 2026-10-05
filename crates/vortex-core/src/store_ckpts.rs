//! Domain queries: file checkpoints (recovery snapshots taken before writes).

use crate::db::{now, Db};
use crate::error::Result;

impl Db {
    /// Snapshot the original content of a file before it is modified.
    /// `None` records that the file did not exist yet.
    pub async fn add_checkpoint(
        &self,
        conversation_id: String,
        run_id: String,
        path: String,
        original: Option<String>,
    ) -> Result<i64> {
        Ok(self
            .with_conn(move |c| {
                c.execute(
                    "INSERT INTO checkpoints (conversation_id, run_id, path, original, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    rusqlite::params![conversation_id, run_id, path, original, now()],
                )?;
                Ok(c.last_insert_rowid())
            })
            .await?)
    }

    pub async fn list_checkpoints(
        &self,
        conversation_id: String,
    ) -> Result<Vec<(i64, String, Option<String>)>> {
        Ok(self
            .with_conn(move |c| {
                let mut st = c.prepare(
                    "SELECT id, path, original FROM checkpoints WHERE conversation_id = ?1
                     ORDER BY id DESC LIMIT 200",
                )?;
                let rows = st
                    .query_map(rusqlite::params![conversation_id], |r| {
                        Ok((
                            r.get::<_, i64>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, Option<String>>(2)?,
                        ))
                    })?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                Ok(rows)
            })
            .await?)
    }

    /// Restore a checkpoint by id: returns (path, original_content).
    pub async fn get_checkpoint(
        &self,
        checkpoint_id: i64,
    ) -> Result<Option<(String, Option<String>)>> {
        Ok(self
            .with_conn(move |c| {
                let mut st = c.prepare("SELECT path, original FROM checkpoints WHERE id = ?1")?;
                let row = st
                    .query_map(rusqlite::params![checkpoint_id], |r| {
                        Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
                    })?
                    .next()
                    .transpose()?;
                Ok(row)
            })
            .await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vortex_types::{ApprovalInfo, ApprovalStatus};

    #[tokio::test]
    async fn approvals_lifecycle() {
        let db = Db::open(format!("/tmp/vortex-test-{}.db", uuid::Uuid::new_v4())).unwrap();
        db.add_approval(
            "a1".into(),
            "r1".into(),
            "agent".into(),
            "t".into(),
            "p".into(),
        )
        .await
        .unwrap();
        assert_eq!(db.list_approvals(true).await.unwrap().len(), 1);
        db.resolve_approval("a1".into(), ApprovalStatus::Denied)
            .await
            .unwrap();
        assert_eq!(db.list_approvals(true).await.unwrap().len(), 0);
        let all: Vec<ApprovalInfo> = db.list_approvals(false).await.unwrap();
        assert_eq!(all[0].status, ApprovalStatus::Denied);
    }

    #[tokio::test]
    async fn checkpoints_roundtrip() {
        let db = Db::open(format!("/tmp/vortex-test-{}.db", uuid::Uuid::new_v4())).unwrap();
        let id = db
            .add_checkpoint(
                "c1".into(),
                "r1".into(),
                "/tmp/x.txt".into(),
                Some("old".into()),
            )
            .await
            .unwrap();
        let (path, original) = db.get_checkpoint(id).await.unwrap().unwrap();
        assert_eq!(path, "/tmp/x.txt");
        assert_eq!(original.as_deref(), Some("old"));
        assert_eq!(db.list_checkpoints("c1".into()).await.unwrap().len(), 1);
    }
}
