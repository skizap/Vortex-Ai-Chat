//! Routine/job authoring tools: `save_routine` (draft only), `list_routines`,
//! `propose_schedule` (draft job). Interactive Agent-mode runs only — these
//! tools are stripped from job-origin runs at admission, and no model path
//! can activate a draft: activation is a human-only server endpoint.
//!
//! Injection defense: the declared parameter lists contain no status or
//! activation fields, and `ToolDef::filter` strips unknown keys before
//! execution, so injected arguments cannot smuggle an activation transition.

use crate::errors::{Risk, ToolError};
use crate::registry::{Tool, ToolContext, ToolResult};
use crate::schema::{ParamSpec, ParamType, ToolDef};
use async_trait::async_trait;
use vortex_types::{
    JobBudgets, MissedPolicy, Mode, NewJob, NewRoutine, OverlapPolicy, ScheduleSpec,
};

/// Honest setup error when the run has no database attached. Production runs
/// always receive one from the server; a missing database means the tool
/// cannot persist anything, so it fails loudly instead of pretending.
fn require_db(ctx: &ToolContext) -> Result<vortex_core::Db, ToolError> {
    ctx.db.clone().ok_or_else(|| {
        ToolError::Unavailable(
            "routine storage is unavailable: this run has no database attached. \
             Start Vortex through the server (which attaches the database) and retry."
                .into(),
        )
    })
}

pub struct SaveRoutineTool;

const SAVE_ROUTINE_DEF: ToolDef = ToolDef {
    name: "save_routine",
    description: "Save a reusable routine as a DRAFT (version 1). A draft never runs or schedules anything: a human must activate it in the Jobs tab first. You cannot activate it yourself.",
    params: &[
        ParamSpec {
            name: "name",
            ty: ParamType::Str,
            required: true,
            description: "Short routine name.",
        },
        ParamSpec {
            name: "instructions",
            ty: ParamType::Str,
            required: true,
            description: "Task instructions the routine will carry out.",
        },
        ParamSpec {
            name: "description",
            ty: ParamType::Str,
            required: false,
            description: "Optional one-line description of what the routine does.",
        },
        ParamSpec {
            name: "mode",
            ty: ParamType::Str,
            required: false,
            description: "Run mode: chat, research, coding, browser, or agent. Defaults to agent.",
        },
    ],
};

#[async_trait]
impl Tool for SaveRoutineTool {
    fn def(&self) -> &'static ToolDef {
        &SAVE_ROUTINE_DEF
    }
    fn risk(&self) -> Risk {
        Risk::Low
    }
    fn available(&self, ctx: &ToolContext) -> Result<(), ToolError> {
        require_db(ctx).map(|_| ())
    }
    async fn execute(
        &self,
        ctx: &ToolContext,
        args: &serde_json::Value,
    ) -> Result<ToolResult, ToolError> {
        let db = require_db(ctx)?;
        let name = args["name"].as_str().expect("validated").trim();
        let instructions = args["instructions"].as_str().expect("validated");
        if name.is_empty() {
            return Err(ToolError::InvalidArgs(
                "save_routine".into(),
                "name must not be empty".into(),
            ));
        }
        if instructions.trim().is_empty() {
            return Err(ToolError::InvalidArgs(
                "save_routine".into(),
                "instructions must not be empty".into(),
            ));
        }
        let description = args["description"]
            .as_str()
            .unwrap_or("")
            .trim()
            .to_string();
        let mode = match args["mode"].as_str() {
            None => Mode::Agent,
            Some(raw) => serde_json::from_value::<Mode>(serde_json::Value::String(raw.to_string()))
                .map_err(|_| {
                    ToolError::InvalidArgs(
                        "save_routine".into(),
                        format!(
                            "invalid mode '{raw}': expected chat, research, coding, browser, \
                             or agent"
                        ),
                    )
                })?,
        };
        let routine = db
            .create_routine(NewRoutine {
                name: name.to_string(),
                description,
                instructions: instructions.to_string(),
                mode,
                model: None,
            })
            .await
            .map_err(|e| ToolError::Execution(format!("could not save routine: {e}")))?;
        Ok(ToolResult {
            text: format!(
                "Saved routine '{}' as draft v1 (id: {}). It cannot run or be scheduled until \
                 a human activates it in the Jobs tab. You can never activate a routine \
                 yourself — only a human can.",
                routine.name, routine.id
            ),
            summary: format!("routine draft: {}", routine.name),
            diff: None,
        })
    }
}

pub struct ListRoutinesTool;

const LIST_ROUTINES_DEF: ToolDef = ToolDef {
    name: "list_routines",
    description: "List the saved routines with their status, current version, and description. Read-only; it never changes or activates anything.",
    params: &[],
};

#[async_trait]
impl Tool for ListRoutinesTool {
    fn def(&self) -> &'static ToolDef {
        &LIST_ROUTINES_DEF
    }
    fn risk(&self) -> Risk {
        Risk::Low
    }
    fn available(&self, ctx: &ToolContext) -> Result<(), ToolError> {
        require_db(ctx).map(|_| ())
    }
    async fn execute(
        &self,
        ctx: &ToolContext,
        _args: &serde_json::Value,
    ) -> Result<ToolResult, ToolError> {
        let db = require_db(ctx)?;
        let routines = db
            .list_routines()
            .await
            .map_err(|e| ToolError::Execution(format!("could not list routines: {e}")))?;
        if routines.is_empty() {
            return Ok(ToolResult::text("no routines yet"));
        }
        let text = routines
            .iter()
            .map(|r| {
                if r.description.is_empty() {
                    format!(
                        "- {} [{}] v{} (id: {})",
                        r.name,
                        r.status.as_str(),
                        r.current_version,
                        r.id
                    )
                } else {
                    format!(
                        "- {} [{}] v{} (id: {}) — {}",
                        r.name,
                        r.status.as_str(),
                        r.current_version,
                        r.id,
                        r.description
                    )
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        Ok(ToolResult {
            summary: format!("listed {} routines", routines.len()),
            text,
            diff: None,
        })
    }
}

pub struct ProposeScheduleTool;

const PROPOSE_SCHEDULE_DEF: ToolDef = ToolDef {
    name: "propose_schedule",
    description: "Propose a cron schedule for an existing routine as a DRAFT job pinned to the routine's current version. A draft job never fires: a human must activate it in the Jobs tab first, and schedules only fire while Vortex is running. You cannot activate it yourself.",
    params: &[
        ParamSpec {
            name: "routine_id",
            ty: ParamType::Str,
            required: true,
            description: "Id of an existing routine (from save_routine or list_routines).",
        },
        ParamSpec {
            name: "cron",
            ty: ParamType::Str,
            required: true,
            description: "Standard 5-field cron expression (minute hour day-of-month month day-of-week), e.g. \"0 9 * * *\".",
        },
        ParamSpec {
            name: "name",
            ty: ParamType::Str,
            required: false,
            description: "Optional job name. Defaults to \"Scheduled: <routine name>\".",
        },
        ParamSpec {
            name: "missed_policy",
            ty: ParamType::Str,
            required: false,
            description: "What to do with ticks missed while Vortex was off: run_once_if_missed (default) or skip.",
        },
    ],
};

#[async_trait]
impl Tool for ProposeScheduleTool {
    fn def(&self) -> &'static ToolDef {
        &PROPOSE_SCHEDULE_DEF
    }
    fn risk(&self) -> Risk {
        Risk::Low
    }
    fn available(&self, ctx: &ToolContext) -> Result<(), ToolError> {
        require_db(ctx).map(|_| ())
    }
    async fn execute(
        &self,
        ctx: &ToolContext,
        args: &serde_json::Value,
    ) -> Result<ToolResult, ToolError> {
        let db = require_db(ctx)?;
        let routine_id = args["routine_id"].as_str().expect("validated");
        let cron = args["cron"].as_str().expect("validated");
        let missed_policy = match args["missed_policy"].as_str() {
            None | Some("run_once_if_missed") => MissedPolicy::RunOnceIfMissed,
            Some("skip") => MissedPolicy::Skip,
            Some(other) => {
                return Err(ToolError::InvalidArgs(
                    "propose_schedule".into(),
                    format!(
                        "invalid missed_policy '{other}': expected \"run_once_if_missed\" or \
                         \"skip\""
                    ),
                ))
            }
        };
        let spec = ScheduleSpec {
            cron: cron.to_string(),
            tz: "local".into(),
            missed_policy,
        };
        crate::clock::validate_schedule(&spec)
            .map_err(|msg| ToolError::InvalidArgs("propose_schedule".into(), msg))?;
        let routine = db
            .get_routine(routine_id.to_string())
            .await
            .map_err(|e| ToolError::Execution(format!("could not look up routine: {e}")))?
            .ok_or_else(|| {
                ToolError::InvalidArgs(
                    "propose_schedule".into(),
                    format!(
                        "routine '{routine_id}' not found; save it with save_routine first and \
                         use the id from that result"
                    ),
                )
            })?;
        let name = match args["name"].as_str().map(str::trim) {
            Some(n) if !n.is_empty() => n.to_string(),
            _ => format!("Scheduled: {}", routine.name),
        };
        let job = db
            .create_job(NewJob {
                name,
                routine_id: routine.id.clone(),
                routine_version: routine.current_version,
                schedule: Some(spec),
                overlap: OverlapPolicy::default(),
                budgets: JobBudgets::default(),
                workspace: None,
            })
            .await
            .map_err(|e| ToolError::Execution(format!("could not create draft job: {e}")))?;
        Ok(ToolResult {
            text: format!(
                "Created draft job '{}' (id: {}) pinned to routine '{}' version {} with cron \
                 '{}'. It will not fire until a human activates it in the Jobs tab; you can \
                 never activate a job yourself. Schedules only fire while Vortex is running.",
                job.name, job.id, routine.name, job.routine_version, cron
            ),
            summary: format!("job draft: {}", job.name),
            diff: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::allowed_for_mode;
    use crate::registry::{test_context, ToolRegistry};
    use std::collections::HashSet;
    use vortex_types::{JobStatus, RoutineStatus, RunOrigin, Settings};

    fn ctx_with_db(allowed: &[&str]) -> (ToolContext, vortex_core::Db, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db = vortex_core::Db::open(dir.path().join("routines.db")).unwrap();
        let (mut ctx, _rx, _cancel) = test_context(Settings::default(), allowed);
        ctx.db = Some(db.clone());
        (ctx, db, dir)
    }

    fn set(names: &[&str]) -> HashSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    fn new_routine(name: &str, instructions: &str) -> NewRoutine {
        NewRoutine {
            name: name.into(),
            description: String::new(),
            instructions: instructions.into(),
            mode: Mode::Agent,
            model: None,
        }
    }

    #[tokio::test]
    async fn save_routine_lands_draft_v1_and_roundtrips_instructions() {
        let (ctx, db, _dir) = ctx_with_db(&["save_routine"]);
        let args = serde_json::json!({
            "name": "Morning digest",
            "instructions": "Summarize the news.",
            "description": "Daily research digest",
            "mode": "research",
        });
        SaveRoutineTool.def().validate(&args).unwrap();
        let res = SaveRoutineTool
            .execute(&ctx, &SaveRoutineTool.def().filter(args))
            .await
            .unwrap();
        assert_eq!(res.summary, "routine draft: Morning digest");
        assert!(res.text.contains("draft v1"));
        assert!(res.text.contains("human"));
        assert!(res.text.contains("Jobs tab"));
        assert!(res.text.contains("never activate"));
        let routines = db.list_routines().await.unwrap();
        assert_eq!(routines.len(), 1);
        let r = &routines[0];
        assert_eq!(r.status, RoutineStatus::Draft);
        assert_eq!(r.current_version, 1);
        assert_eq!(r.description, "Daily research digest");
        assert!(res.text.contains(&r.id));
        let v1 = db
            .get_routine_version(r.id.clone(), 1)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(v1.instructions, "Summarize the news.");
        assert_eq!(v1.mode, Mode::Research);
        assert_eq!(v1.model, None);
    }

    #[tokio::test]
    async fn save_routine_defaults_to_agent_mode() {
        let (ctx, db, _dir) = ctx_with_db(&["save_routine"]);
        let args = serde_json::json!({"name": "plain", "instructions": "do the thing"});
        SaveRoutineTool.execute(&ctx, &args).await.unwrap();
        let r = &db.list_routines().await.unwrap()[0];
        assert_eq!(r.description, "");
        let v1 = db
            .get_routine_version(r.id.clone(), 1)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(v1.mode, Mode::Agent);
    }

    #[tokio::test]
    async fn save_routine_rejects_invalid_mode() {
        let (ctx, db, _dir) = ctx_with_db(&["save_routine"]);
        let args = serde_json::json!({"name": "x", "instructions": "i", "mode": "superuser"});
        let err = SaveRoutineTool.execute(&ctx, &args).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs(_, _)));
        assert!(err.to_string().contains("invalid mode"));
        assert!(db.list_routines().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn save_routine_without_db_is_unavailable() {
        let (mut ctx, _db, _dir) = ctx_with_db(&["save_routine"]);
        ctx.db = None;
        let args = serde_json::json!({"name": "x", "instructions": "i"});
        let err = SaveRoutineTool.execute(&ctx, &args).await.unwrap_err();
        assert!(matches!(err, ToolError::Unavailable(_)));
        assert!(err.to_string().contains("no database attached"));
        assert!(matches!(
            SaveRoutineTool.available(&ctx),
            Err(ToolError::Unavailable(_))
        ));
    }

    /// Injection attempt: extra activation-shaped keys must be stripped by
    /// the schema filter, and the full registry path must still land a DRAFT.
    #[tokio::test]
    async fn injection_attempt_cannot_activate_a_routine() {
        let (ctx, db, _dir) = ctx_with_db(&["save_routine"]);
        let def = SaveRoutineTool.def();
        let names: Vec<&str> = def.params.iter().map(|p| p.name).collect();
        assert_eq!(names, vec!["name", "instructions", "description", "mode"]);
        let injected = serde_json::json!({"name": "x", "instructions": "i", "status": "active", "activate": true});
        def.validate(&injected).unwrap();
        let filtered = def.filter(injected.clone());
        assert_eq!(
            filtered,
            serde_json::json!({"name": "x", "instructions": "i"})
        );
        let mut registry = ToolRegistry::new();
        registry.register(std::sync::Arc::new(SaveRoutineTool));
        let outcome = registry.execute(&ctx, "save_routine", &injected).await;
        assert!(outcome.ok, "{}", outcome.result.text);
        let routines = db.list_routines().await.unwrap();
        assert_eq!(routines.len(), 1);
        assert_eq!(routines[0].status, RoutineStatus::Draft);
        assert_eq!(routines[0].current_version, 1);
    }

    /// No authoring tool declares a status/activation parameter, and all
    /// three are Low risk (drafts are harmless; activation is human-only).
    #[test]
    fn authoring_defs_declare_no_activation_params_and_are_low_risk() {
        for tool in [
            &SaveRoutineTool as &dyn Tool,
            &ListRoutinesTool as &dyn Tool,
            &ProposeScheduleTool as &dyn Tool,
        ] {
            assert_eq!(tool.risk(), Risk::Low);
            for p in tool.def().params {
                assert!(
                    !matches!(p.name, "status" | "activate" | "active"),
                    "{} declares an activation-shaped parameter '{}'",
                    tool.def().name,
                    p.name
                );
            }
        }
    }

    #[tokio::test]
    async fn propose_schedule_creates_draft_job_pinned_to_current_version() {
        let (ctx, db, _dir) = ctx_with_db(&["propose_schedule"]);
        let r = db
            .create_routine(new_routine("digest", "v1 instructions"))
            .await
            .unwrap();
        db.add_routine_version(r.id.clone(), "v2 instructions".into(), Mode::Agent, None)
            .await
            .unwrap();
        let args = serde_json::json!({"routine_id": r.id, "cron": "0 9 * * *"});
        ProposeScheduleTool.def().validate(&args).unwrap();
        let res = ProposeScheduleTool
            .execute(&ctx, &ProposeScheduleTool.def().filter(args))
            .await
            .unwrap();
        let jobs = db.list_jobs().await.unwrap();
        assert_eq!(jobs.len(), 1);
        let job = &jobs[0];
        assert_eq!(job.status, JobStatus::Draft);
        assert_eq!(job.routine_id, r.id);
        assert_eq!(job.routine_version, 2, "pins current_version, not always 1");
        assert_eq!(job.name, "Scheduled: digest");
        let sched = job.schedule.as_ref().unwrap();
        assert_eq!(sched.cron, "0 9 * * *");
        assert_eq!(sched.tz, "local");
        assert_eq!(sched.missed_policy, MissedPolicy::RunOnceIfMissed);
        assert_eq!(job.overlap, OverlapPolicy::Skip);
        assert_eq!(job.budgets, JobBudgets::default());
        assert!(job.workspace.is_none());
        assert_eq!(res.summary, "job draft: Scheduled: digest");
        assert!(res.text.contains("draft"));
        assert!(res.text.contains(&job.id));
        assert!(res.text.contains("version 2"));
        assert!(res.text.contains("Jobs tab"));
        assert!(res.text.contains("never activate"));
        assert!(res.text.contains("while Vortex is running"));
        let pinned = db
            .get_routine_version(job.routine_id.clone(), job.routine_version)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(pinned.instructions, "v2 instructions");
        assert_eq!(
            db.get_routine(r.id.clone()).await.unwrap().unwrap().status,
            RoutineStatus::Draft,
            "proposing a schedule never activates the routine"
        );
    }

    #[tokio::test]
    async fn propose_schedule_rejects_invalid_cron() {
        let (ctx, db, _dir) = ctx_with_db(&["propose_schedule"]);
        let r = db
            .create_routine(new_routine("digest", "instructions"))
            .await
            .unwrap();
        for bad in ["", "not a cron", "99 * * * *"] {
            let args = serde_json::json!({"routine_id": r.id, "cron": bad});
            let err = ProposeScheduleTool.execute(&ctx, &args).await.unwrap_err();
            assert!(matches!(err, ToolError::InvalidArgs(_, _)), "cron: {bad}");
            assert!(err.to_string().contains("cron"), "cron: {bad}");
        }
        let six_fields = serde_json::json!({"routine_id": r.id, "cron": "0 0 9 * * *"});
        let err = ProposeScheduleTool
            .execute(&ctx, &six_fields)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs(_, _)));
        assert!(
            err.to_string().contains("5 fields"),
            "actionable message must explain the 5-field contract"
        );
        assert!(db.list_jobs().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn propose_schedule_rejects_unknown_routine() {
        let (ctx, db, _dir) = ctx_with_db(&["propose_schedule"]);
        let args = serde_json::json!({"routine_id": "missing", "cron": "0 9 * * *"});
        let err = ProposeScheduleTool.execute(&ctx, &args).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs(_, _)));
        assert!(err.to_string().contains("not found"));
        assert!(db.list_jobs().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn propose_schedule_parses_missed_policy() {
        let (ctx, db, _dir) = ctx_with_db(&["propose_schedule"]);
        let r = db
            .create_routine(new_routine("digest", "instructions"))
            .await
            .unwrap();
        let skip = serde_json::json!({
            "routine_id": r.id, "cron": "0 9 * * *", "missed_policy": "skip"
        });
        ProposeScheduleTool.execute(&ctx, &skip).await.unwrap();
        let default = serde_json::json!({"routine_id": r.id, "cron": "30 8 * * *"});
        ProposeScheduleTool.execute(&ctx, &default).await.unwrap();
        let jobs = db.list_jobs().await.unwrap();
        assert_eq!(jobs.len(), 2);
        let skip_job = jobs
            .iter()
            .find(|j| j.schedule.as_ref().unwrap().cron == "0 9 * * *")
            .unwrap();
        let default_job = jobs
            .iter()
            .find(|j| j.schedule.as_ref().unwrap().cron == "30 8 * * *")
            .unwrap();
        assert_eq!(
            skip_job.schedule.as_ref().unwrap().missed_policy,
            MissedPolicy::Skip
        );
        assert_eq!(
            default_job.schedule.as_ref().unwrap().missed_policy,
            MissedPolicy::RunOnceIfMissed
        );
        assert!(jobs.iter().all(|j| j.status == JobStatus::Draft));
        let bad = serde_json::json!({
            "routine_id": r.id, "cron": "0 9 * * *", "missed_policy": "sometimes"
        });
        let err = ProposeScheduleTool.execute(&ctx, &bad).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs(_, _)));
        assert!(err.to_string().contains("missed_policy"));
        assert_eq!(db.list_jobs().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn propose_schedule_honors_custom_name_and_empty_fallback() {
        let (ctx, db, _dir) = ctx_with_db(&["propose_schedule"]);
        let r = db
            .create_routine(new_routine("digest", "instructions"))
            .await
            .unwrap();
        let named = serde_json::json!({
            "routine_id": r.id, "cron": "0 9 * * *", "name": "Nightly digest"
        });
        let res = ProposeScheduleTool.execute(&ctx, &named).await.unwrap();
        assert_eq!(res.summary, "job draft: Nightly digest");
        let blank = serde_json::json!({"routine_id": r.id, "cron": "0 10 * * *", "name": "  "});
        ProposeScheduleTool.execute(&ctx, &blank).await.unwrap();
        let jobs = db.list_jobs().await.unwrap();
        assert!(jobs.iter().any(|j| j.name == "Nightly digest"));
        assert!(jobs.iter().any(|j| j.name == "Scheduled: digest"));
    }

    #[tokio::test]
    async fn propose_schedule_without_db_is_unavailable() {
        let (mut ctx, _db, _dir) = ctx_with_db(&["propose_schedule"]);
        ctx.db = None;
        let args = serde_json::json!({"routine_id": "r", "cron": "0 9 * * *"});
        let err = ProposeScheduleTool.execute(&ctx, &args).await.unwrap_err();
        assert!(matches!(err, ToolError::Unavailable(_)));
        assert!(err.to_string().contains("no database attached"));
    }

    #[tokio::test]
    async fn list_routines_empty_is_honest() {
        let (ctx, _db, _dir) = ctx_with_db(&["list_routines"]);
        let res = ListRoutinesTool
            .execute(&ctx, &serde_json::json!({}))
            .await
            .unwrap();
        assert!(res.text.contains("no routines yet"));
    }

    #[tokio::test]
    async fn list_routines_populated_shows_status_version_and_description() {
        let (ctx, db, _dir) = ctx_with_db(&["list_routines"]);
        let mut with_desc = new_routine("digest", "instructions");
        with_desc.description = "Daily research digest".into();
        let a = db.create_routine(with_desc).await.unwrap();
        let b = db
            .create_routine(new_routine("bare", "instructions"))
            .await
            .unwrap();
        let res = ListRoutinesTool
            .execute(&ctx, &serde_json::json!({}))
            .await
            .unwrap();
        for r in [&a, &b] {
            assert!(res.text.contains(&r.name));
            assert!(res.text.contains(&r.id));
        }
        assert!(res.text.contains("draft"));
        assert!(res.text.contains("v1"));
        assert!(res.text.contains("Daily research digest"));
        assert!(res.summary.contains('2'));
    }

    #[tokio::test]
    async fn list_routines_without_db_is_unavailable() {
        let (mut ctx, _db, _dir) = ctx_with_db(&["list_routines"]);
        ctx.db = None;
        let err = ListRoutinesTool
            .execute(&ctx, &serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Unavailable(_)));
    }

    /// Regression: Chat/Research/Coding/Browser toolsets are unchanged, and
    /// the Agent arm is their union plus the three authoring tools — minus
    /// those tools for job-origin runs. Read-only use of allowed_for_mode.
    #[test]
    fn toolset_regression_pins_mode_sets_and_job_origin_stripping() {
        let settings = Settings::default();
        let interactive = RunOrigin::Interactive;
        let meta = ["update_plan", "request_approval"];

        let chat = allowed_for_mode(Mode::Chat, &settings, true, &interactive);
        assert_eq!(chat, set(&meta));

        let research = allowed_for_mode(Mode::Research, &settings, true, &interactive);
        assert_eq!(
            research,
            set(&[
                "update_plan",
                "request_approval",
                "web_search",
                "fetch_url",
                "delegate_subtask",
            ])
        );

        // commands defaults to false: no run_command in any default set.
        let coding = allowed_for_mode(Mode::Coding, &settings, true, &interactive);
        assert_eq!(
            coding,
            set(&[
                "update_plan",
                "request_approval",
                "list_files",
                "read_file",
                "write_file",
                "edit_file",
                "search_files",
                "delete_path",
                "start_preview",
                "stop_preview",
                "delegate_subtask",
            ])
        );

        let browser = allowed_for_mode(Mode::Browser, &settings, true, &interactive);
        assert_eq!(
            browser,
            set(&[
                "update_plan",
                "request_approval",
                "browser_navigate",
                "browser_read",
                "browser_click",
                "browser_type",
                "fetch_url",
                "delegate_subtask",
            ])
        );

        let mut union = research.clone();
        union.extend(coding.iter().cloned());
        union.extend(browser.iter().cloned());
        let authoring = ["save_routine", "list_routines", "propose_schedule"];
        let mut want_agent = union.clone();
        want_agent.extend(authoring.iter().map(|s| s.to_string()));

        let agent = allowed_for_mode(Mode::Agent, &settings, true, &interactive);
        assert_eq!(agent, want_agent);

        let job_origin = RunOrigin::Job {
            job_id: "j".into(),
            occurrence_id: "o".into(),
        };
        let agent_job = allowed_for_mode(Mode::Agent, &settings, true, &job_origin);
        assert_eq!(agent_job, union, "job runs keep the union, lose authoring");
        for t in authoring {
            assert!(!agent_job.contains(t), "job-origin run must not expose {t}");
        }
    }
}
