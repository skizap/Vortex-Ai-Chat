//! `delegate_subtask`: the coordinator's tool for spawning sub-agents.
//! The engine enforces recursion policy and permission inheritance; the
//! scheduler enforces the global concurrency cap.

use crate::engine::Engine;
use crate::errors::{Risk, ToolError};
use crate::registry::{Tool, ToolContext, ToolResult};
use crate::schema::{ParamSpec, ParamType, ToolDef};
use async_trait::async_trait;
use std::sync::{Arc, OnceLock};

pub struct DelegateTool {
    /// Late-bound engine handle (the engine owns the registry that owns this
    /// tool; the server fills the slot right after constructing the engine).
    pub engine_slot: Arc<OnceLock<Arc<Engine>>>,
}

const DELEGATE_DEF: ToolDef = ToolDef {
    name: "delegate_subtask",
    description: "Delegate a self-contained subtask to a sub-agent. Give explicit context, the required output, and a tool list. Sub-agents run concurrently (bounded by the user's configured limit) and cannot exceed your own permissions.",
    params: &[
        ParamSpec { name: "task", ty: ParamType::Str, required: true, description: "Complete, self-contained task description for the sub-agent." },
        ParamSpec { name: "title", ty: ParamType::Str, required: false, description: "Short label for the agents panel." },
        ParamSpec { name: "tools", ty: ParamType::StrArray, required: false, description: "Names of tools the sub-agent may use (subset of your own; defaults to all of yours except delegation)." },
    ],
};

#[async_trait]
impl Tool for DelegateTool {
    fn def(&self) -> &'static ToolDef {
        &DELEGATE_DEF
    }
    fn risk(&self) -> Risk {
        Risk::Medium
    }
    fn available(&self, ctx: &ToolContext) -> Result<(), ToolError> {
        if !ctx.settings.tools.delegation {
            return Err(ToolError::Unavailable(
                "sub-agent delegation is disabled. Enable it in Settings → Tools.".into(),
            ));
        }
        if ctx.depth >= 1 && !ctx.settings.agents.recursive {
            return Err(ToolError::Unavailable(
                "recursive delegation is disabled by default; sub-agents cannot spawn \
                 further sub-agents."
                    .into(),
            ));
        }
        Ok(())
    }
    async fn execute(
        &self,
        ctx: &ToolContext,
        args: &serde_json::Value,
    ) -> Result<ToolResult, ToolError> {
        let engine = self
            .engine_slot
            .get()
            .ok_or_else(|| ToolError::Execution("engine not initialized".into()))?
            .clone();
        let task = args["task"].as_str().expect("validated");
        let title = args
            .get("title")
            .and_then(|t| t.as_str())
            .map(|s| s.chars().take(40).collect::<String>())
            .unwrap_or_else(|| format!("sub-{}", &task[..task.len().min(20)]));
        let agent = format!("subagent-{}", uuid::Uuid::new_v4().simple());
        let requested: Option<Vec<String>> =
            args.get("tools").and_then(|t| t.as_array()).map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            });
        match engine.spawn_subagent(ctx, task, &agent, requested).await {
            Ok(result) => Ok(ToolResult {
                summary: format!("sub-agent '{title}' finished"),
                text: format!("Sub-agent result for '{title}':\n\n{result}"),
                diff: None,
            }),
            Err(ToolError::Cancelled) => Err(ToolError::Cancelled),
            Err(ToolError::NotPermitted(msg)) => Err(ToolError::NotPermitted(msg)),
            Err(e) => Err(ToolError::Execution(format!(
                "sub-agent '{title}' did not complete: {e}"
            ))),
        }
    }
}
