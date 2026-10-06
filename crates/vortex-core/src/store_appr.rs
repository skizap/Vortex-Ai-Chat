//! Domain queries: approvals.

use crate::db::{now, Db};
use crate::error::Result;
use vortex_types::{ApprovalInfo, ApprovalStatus};

impl Db {
    pub async fn add_approval(
        &self,
        id: String,
        run_id: String,
        agent: String,
        title: String,
        payload: String,
    ) -> Result<()> {
        self.with_conn(move |c| {
            c.execute(
                "INSERT INTO approvals (id, run_id, agent, title, payload, status, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, 'pending', ?6)",
                rusqlite::params![id, run_id, agent, title, payload, now()],
            )?;
            Ok(())
        })
        .await
    }

    pub async fn resolve_approval(&self, id: String, status: ApprovalStatus) -> Result<bool> {
        let status_str = match status {
            ApprovalStatus::Approved => "approved",
            ApprovalStatus::Denied => "denied",
            ApprovalStatus::Expired => "expired",
            ApprovalStatus::Pending => "pending",
        };
        self.with_conn(move |c| {
            Ok(c.execute(
                "UPDATE approvals SET status = ?2, decided_at = ?3 WHERE id = ?1",
                rusqlite::params![id, status_str, now()],
            )? > 0)
        })
        .await
    }

    fn row_to_approval(row: &rusqlite::Row<'_>) -> rusqlite::Result<ApprovalInfo> {
        let status: String = row.get(5)?;
        Ok(ApprovalInfo {
            id: row.get(0)?,
            run_id: row.get(1)?,
            agent: row.get(2)?,
            title: row.get(3)?,
            payload: row.get(4)?,
            status: match status.as_str() {
                "approved" => ApprovalStatus::Approved,
                "denied" => ApprovalStatus::Denied,
                "expired" => ApprovalStatus::Expired,
                _ => ApprovalStatus::Pending,
            },
            created_at: row.get(6)?,
        })
    }

    pub async fn list_approvals(&self, pending_only: bool) -> Result<Vec<ApprovalInfo>> {
        self.with_conn(move |c| {
            let sql = if pending_only {
                "SELECT id, run_id, agent, title, payload, status, created_at FROM approvals \
                     WHERE status = 'pending' ORDER BY created_at DESC LIMIT 100"
            } else {
                "SELECT id, run_id, agent, title, payload, status, created_at FROM approvals \
                     ORDER BY created_at DESC LIMIT 100"
            };
            let mut st = c.prepare(sql)?;
            let rows = st
                .query_map([], Self::row_to_approval)?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
    }
}
