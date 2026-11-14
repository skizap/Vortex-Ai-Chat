//! Domain queries: file checkpoints (recovery snapshots taken before writes).

use crate::db::{now, Db};
use crate::error::Result;
use vortex_types::CheckpointInfo;

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
        self.with_conn(move |c| {
            c.execute(
                "INSERT INTO checkpoints (conversation_id, run_id, path, original, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![conversation_id, run_id, path, original, now()],
            )?;
            Ok(c.last_insert_rowid())
        })
        .await
    }

    /// Checkpoints for a conversation, newest first, cursor-paginated: pass
    /// the smallest `id` from the previous page as `before` to walk further
    /// back. `limit` is clamped to 1..=200. Snapshot content is never
    /// returned here (lists stay small) — see [`Db::get_checkpoint`].
    pub async fn list_checkpoints(
        &self,
        conversation_id: String,
        before: Option<i64>,
        limit: u32,
    ) -> Result<Vec<CheckpointInfo>> {
        let limit = limit.clamp(1, 200);
        self.with_conn(move |c| {
            let mut st = c.prepare(
                "SELECT id, run_id, path, original IS NOT NULL, created_at
                   FROM checkpoints
                  WHERE conversation_id = ?1 AND (?2 IS NULL OR id < ?2)
                  ORDER BY id DESC LIMIT ?3",
            )?;
            let rows = st
                .query_map(rusqlite::params![conversation_id, before, limit], |r| {
                    Ok(CheckpointInfo {
                        id: r.get::<_, i64>(0)?,
                        run_id: r.get::<_, String>(1)?,
                        path: r.get::<_, String>(2)?,
                        has_original: r.get::<_, bool>(3)?,
                        created_at: r.get::<_, String>(4)?,
                    })
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
    }

    /// Load one checkpoint for restore: `(conversation_id, path, original)`.
    /// The conversation linkage lets a restore snapshot the *current* content
    /// into the same conversation first (making the restore reversible).
    /// `original == None` records that the file did not exist at snapshot
    /// time.
    pub async fn get_checkpoint(
        &self,
        checkpoint_id: i64,
    ) -> Result<Option<(String, String, Option<String>)>> {
        self.with_conn(move |c| {
            let mut st =
                c.prepare("SELECT conversation_id, path, original FROM checkpoints WHERE id = ?1")?;
            let row = st
                .query_map(rusqlite::params![checkpoint_id], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, Option<String>>(2)?,
                    ))
                })?
                .next()
                .transpose()?;
            Ok(row)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vortex_types::{ApprovalInfo, ApprovalStatus};

    #[tokio::test]
    async fn approvals_lifecycle() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path().join("test.db")).unwrap();
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
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path().join("test.db")).unwrap();
        let id = db
            .add_checkpoint(
                "c1".into(),
                "r1".into(),
                "/tmp/x.txt".into(),
                Some("old".into()),
            )
            .await
            .unwrap();
        let (conv, path, original) = db.get_checkpoint(id).await.unwrap().unwrap();
        assert_eq!(conv, "c1");
        assert_eq!(path, "/tmp/x.txt");
        assert_eq!(original.as_deref(), Some("old"));
        // "Did not exist" snapshots round-trip as NULL originals.
        let id2 = db
            .add_checkpoint("c1".into(), "r1".into(), "/tmp/new.txt".into(), None)
            .await
            .unwrap();
        let (_, _, original2) = db.get_checkpoint(id2).await.unwrap().unwrap();
        assert_eq!(original2, None);
        let list = db.list_checkpoints("c1".into(), None, 200).await.unwrap();
        assert_eq!(list.len(), 2);
        // List metadata never carries content, but flags its presence.
        let newest = &list[0];
        assert_eq!(newest.id, id2);
        assert!(!newest.has_original);
        assert_eq!(list[1].id, id);
        assert!(list[1].has_original);
        assert_eq!(list[1].run_id, "r1");
    }

    /// Restore round-trip at the store level: the pre-restore snapshot of the
    /// current content is recorded in the same conversation with run_id
    /// "restore", and both directions are readable.
    #[tokio::test]
    async fn restore_snapshot_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path().join("test.db")).unwrap();
        let original_id = db
            .add_checkpoint(
                "conv".into(),
                "run-1".into(),
                "/w/f.txt".into(),
                Some("v1".into()),
            )
            .await
            .unwrap();
        // Simulate the restore route: snapshot current content ("v2") first.
        db.add_checkpoint(
            "conv".into(),
            "restore".into(),
            "/w/f.txt".into(),
            Some("v2".into()),
        )
        .await
        .unwrap();
        let (conv, _, original) = db.get_checkpoint(original_id).await.unwrap().unwrap();
        assert_eq!(conv, "conv");
        assert_eq!(original.as_deref(), Some("v1"));
        let list = db.list_checkpoints("conv".into(), None, 200).await.unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(
            list[0].run_id, "restore",
            "newest row is the pre-restore snapshot"
        );
    }

    #[tokio::test]
    async fn list_checkpoints_cursor_pagination() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path().join("test.db")).unwrap();
        let mut ids = Vec::new();
        for i in 0..5 {
            ids.push(
                db.add_checkpoint(
                    "cp".into(),
                    format!("r{i}"),
                    format!("/tmp/f{i}.txt"),
                    Some(format!("content-{i}")),
                )
                .await
                .unwrap(),
            );
        }
        // A foreign conversation must not leak into the list.
        db.add_checkpoint("other".into(), "rx".into(), "/tmp/o.txt".into(), None)
            .await
            .unwrap();

        // Page 1: newest two.
        let page = db.list_checkpoints("cp".into(), None, 2).await.unwrap();
        assert_eq!(
            page.iter().map(|c| c.id).collect::<Vec<_>>(),
            vec![ids[4], ids[3]]
        );
        // Page 2: walk back with the cursor.
        let before = page.last().unwrap().id;
        let page = db
            .list_checkpoints("cp".into(), Some(before), 2)
            .await
            .unwrap();
        assert_eq!(
            page.iter().map(|c| c.id).collect::<Vec<_>>(),
            vec![ids[2], ids[1]]
        );
        // Page 3: last item, then the cursor is exhausted.
        let before = page.last().unwrap().id;
        let page = db
            .list_checkpoints("cp".into(), Some(before), 2)
            .await
            .unwrap();
        assert_eq!(page.iter().map(|c| c.id).collect::<Vec<_>>(), vec![ids[0]]);
        let before = page.last().unwrap().id;
        let page = db
            .list_checkpoints("cp".into(), Some(before), 2)
            .await
            .unwrap();
        assert!(page.is_empty());

        // Limit clamping: 0 → 1, huge → 200 (all 5 fit either way).
        assert_eq!(
            db.list_checkpoints("cp".into(), None, 0)
                .await
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            db.list_checkpoints("cp".into(), None, 999_999)
                .await
                .unwrap()
                .len(),
            5
        );
    }
}
