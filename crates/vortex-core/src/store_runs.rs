//! Domain queries: runs.

use crate::db::{now, Db};
use crate::error::Result;
use vortex_types::{PlanStep, RunInfo, RunStatus, Usage};

fn row_to_run(row: &rusqlite::Row<'_>) -> rusqlite::Result<RunInfo> {
    let usage_json: Option<String> = row.get(8)?;
    let plan_json: String = row.get(9)?;
    Ok(RunInfo {
        id: row.get(0)?,
        conversation_id: row.get(1)?,
        parent_run_id: row.get(2)?,
        agent: row.get(3)?,
        task: row.get(4)?,
        model: row.get(5)?,
        status: serde_json::from_value(serde_json::Value::String(row.get::<_, String>(6)?))
            .unwrap_or(RunStatus::Failed),
        error: row.get(7)?,
        usage: usage_json.and_then(|j| serde_json::from_str(&j).ok()),
        plan: serde_json::from_str::<Vec<PlanStep>>(&plan_json).unwrap_or_default(),
        depth: row.get::<_, i64>(10)? as u32,
        created_at: row.get(11)?,
        updated_at: row.get(12)?,
    })
}

const RUN_COLS: &str =
    "id, conversation_id, parent_run_id, agent, task, model, status, error, usage_json, \
     plan_json, depth, created_at, updated_at";

impl Db {
    pub async fn upsert_run(&self, info: &RunInfo) -> Result<()> {
        let info = info.clone();
        self.with_conn(move |c| {
            let usage = info
                .usage
                .map(|u| serde_json::to_string(&u).unwrap_or_default());
            let plan = serde_json::to_string(&info.plan).unwrap_or_else(|_| "[]".into());
            c.execute(
                "INSERT INTO runs (id, conversation_id, parent_run_id, agent, task, model, status, \
                 error, usage_json, plan_json, depth, created_at, updated_at)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?12)
                 ON CONFLICT(id) DO UPDATE SET
                   status = excluded.status,
                   error = excluded.error,
                   usage_json = excluded.usage_json,
                   plan_json = excluded.plan_json,
                   updated_at = excluded.updated_at",
                rusqlite::params![
                    info.id,
                    info.conversation_id,
                    info.parent_run_id,
                    info.agent,
                    info.task,
                    info.model,
                    info.status.as_str(),
                    info.error,
                    usage,
                    plan,
                    info.depth,
                    now(),
                ],
            )?;
            Ok(())
        })
        .await
    }

    pub async fn update_run_status(
        &self,
        id: String,
        status: RunStatus,
        error: Option<String>,
        usage: Option<Usage>,
        plan: Option<Vec<PlanStep>>,
    ) -> Result<bool> {
        Ok(self
            .with_conn(move |c| {
                let usage = usage.map(|u| serde_json::to_string(&u).unwrap_or_default());
                let plan = plan.map(|p| serde_json::to_string(&p).unwrap_or_else(|_| "[]".into()));
                let n = c.execute(
                    "UPDATE runs SET status = ?2, error = ?3, updated_at = ?6,
                     usage_json = COALESCE(?4, usage_json),
                     plan_json = COALESCE(?5, plan_json)
                     WHERE id = ?1",
                    rusqlite::params![id, status.as_str(), error, usage, plan, now()],
                )?;
                Ok(n > 0)
            })
            .await?)
    }

    pub async fn get_run(&self, id: String) -> Result<Option<RunInfo>> {
        Ok(self
            .with_conn(move |c| {
                let sql = format!("SELECT {RUN_COLS} FROM runs WHERE id = ?1");
                let mut st = c.prepare(&sql)?;
                let row = st
                    .query_map(rusqlite::params![id], row_to_run)?
                    .next()
                    .transpose()?;
                Ok(row)
            })
            .await?)
    }

    pub async fn list_runs_for_conversation(
        &self,
        conversation_id: String,
    ) -> Result<Vec<RunInfo>> {
        self.list_runs_inner(Some(conversation_id), None).await
    }

    pub async fn list_child_runs(&self, parent_run_id: String) -> Result<Vec<RunInfo>> {
        self.list_runs_inner(None, Some(parent_run_id)).await
    }

    async fn list_runs_inner(
        &self,
        conversation: Option<String>,
        parent: Option<String>,
    ) -> Result<Vec<RunInfo>> {
        Ok(self
            .with_conn(move |c| {
                let sql = format!(
                    "SELECT {RUN_COLS} FROM runs \
                     WHERE (?1 IS NULL OR conversation_id = ?1) \
                       AND (?2 IS NULL OR parent_run_id = ?2) \
                     ORDER BY created_at"
                );
                let mut st = c.prepare(&sql)?;
                let rows = st
                    .query_map(rusqlite::params![conversation, parent], row_to_run)?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                Ok(rows)
            })
            .await?)
    }
}
