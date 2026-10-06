//! Domain queries: tool calls.

use crate::db::Db;
use crate::error::Result;
use vortex_types::ToolCallRecord;

impl Db {
    pub async fn add_tool_call(&self, record: &ToolCallRecord) -> Result<()> {
        let record = record.clone();
        self.with_conn(move |c| {
            c.execute(
                "INSERT INTO tool_calls (id, run_id, tool, args_json, ok, summary, created_at)
                 VALUES (?1,?2,?3,?4,?5,?6,?7)
                 ON CONFLICT(id) DO UPDATE SET ok = excluded.ok, summary = excluded.summary",
                rusqlite::params![
                    record.id,
                    record.run_id,
                    record.tool,
                    serde_json::to_string(&record.args).unwrap_or_default(),
                    record.ok,
                    record.summary,
                    record.created_at,
                ],
            )?;
            Ok(())
        })
        .await
    }

    pub async fn list_tool_calls(&self, run_id: String) -> Result<Vec<ToolCallRecord>> {
        self.with_conn(move |c| {
            let mut st = c.prepare(
                "SELECT id, run_id, tool, args_json, ok, summary, created_at
                     FROM tool_calls WHERE run_id = ?1 ORDER BY created_at",
            )?;
            let rows = st
                .query_map(rusqlite::params![run_id], |r| {
                    let args: String = r.get(3)?;
                    Ok(ToolCallRecord {
                        id: r.get(0)?,
                        run_id: r.get(1)?,
                        tool: r.get(2)?,
                        args: serde_json::from_str(&args).unwrap_or(serde_json::Value::Null),
                        ok: r.get::<_, i64>(4)? != 0,
                        summary: r.get(5)?,
                        created_at: r.get(6)?,
                    })
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::now;
    use vortex_types::{Mode, PlanStep, RunInfo, RunStatus, StepStatus, Usage};

    async fn test_db() -> Db {
        Db::open(format!("/tmp/vortex-test-{}.db", uuid::Uuid::new_v4())).unwrap()
    }

    #[tokio::test]
    async fn runs_status_updates() {
        let db = test_db().await;
        let conv = db.create_conversation("c", Mode::Chat, None).await.unwrap();
        let run = RunInfo {
            id: "r1".into(),
            parent_run_id: None,
            conversation_id: Some(conv.id.clone()),
            agent: "coordinator".into(),
            task: "".into(),
            model: "m".into(),
            status: RunStatus::Running,
            error: None,
            usage: Some(Usage {
                prompt_tokens: 5,
                completion_tokens: 6,
                total_tokens: 11,
            }),
            plan: vec![PlanStep {
                title: "step".into(),
                status: StepStatus::Pending,
            }],
            depth: 0,
            created_at: now(),
            updated_at: now(),
        };
        db.upsert_run(&run).await.unwrap();
        db.update_run_status("r1".into(), RunStatus::Completed, None, None, None)
            .await
            .unwrap();
        let got = db.get_run("r1".into()).await.unwrap().unwrap();
        assert_eq!(got.status, RunStatus::Completed);
        assert_eq!(got.plan.len(), 1);
        assert_eq!(got.usage.unwrap().total_tokens, 11);
        let rec = ToolCallRecord {
            id: "tc1".into(),
            run_id: "r1".into(),
            tool: "read_file".into(),
            args: serde_json::json!({"path": "x"}),
            ok: true,
            summary: "ok".into(),
            created_at: now(),
        };
        db.add_tool_call(&rec).await.unwrap();
        assert_eq!(db.list_tool_calls("r1".into()).await.unwrap().len(), 1);
    }
}
