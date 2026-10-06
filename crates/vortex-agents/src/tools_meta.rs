//! Meta tools: `update_plan` (task progress) and `request_approval`
//! (explicit human sign-off for consequential actions).

use crate::errors::{Risk, ToolError};
use crate::registry::{Tool, ToolContext, ToolResult};
use crate::schema::{ParamSpec, ParamType, ToolDef};
use async_trait::async_trait;
use vortex_types::{PlanStep, RunEvent, StepStatus};

pub struct UpdatePlanTool;

const PLAN_DEF: ToolDef = ToolDef {
    name: "update_plan",
    description: "Publish the current plan/steps for this task so the user can follow progress. Call whenever the plan changes.",
    params: &[ParamSpec {
        name: "steps",
        ty: ParamType::StrArray,
        required: true,
        description: "Ordered steps. Prefix a step with [done] or [in_progress] to mark status; default is pending.",
    }],
};

#[async_trait]
impl Tool for UpdatePlanTool {
    fn def(&self) -> &'static ToolDef {
        &PLAN_DEF
    }
    async fn execute(
        &self,
        ctx: &ToolContext,
        args: &serde_json::Value,
    ) -> Result<ToolResult, ToolError> {
        let raw: Vec<String> = args["steps"]
            .as_array()
            .expect("validated")
            .iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .collect();
        if raw.is_empty() {
            return Err(ToolError::InvalidArgs(
                "steps".into(),
                "at least one step is required".into(),
            ));
        }
        let steps: Vec<PlanStep> = raw
            .into_iter()
            .map(|mut s| {
                let lower = s.to_lowercase();
                let status = if lower.starts_with("[done]") {
                    StepStatus::Done
                } else if lower.starts_with("[in_progress]") {
                    StepStatus::InProgress
                } else if lower.starts_with("[skipped]") {
                    StepStatus::Skipped
                } else {
                    StepStatus::Pending
                };
                if status != StepStatus::Pending {
                    // Strip the tag.
                    if let Some(pos) = s.find(']') {
                        s = s[pos + 1..].trim().to_string();
                    }
                }
                PlanStep { title: s, status }
            })
            .collect();
        let _ = ctx
            .events
            .send(RunEvent::PlanUpdated {
                run_id: ctx.run_id.clone(),
                steps: steps.clone(),
            })
            .await;
        Ok(ToolResult {
            text: format!("plan updated ({} steps)", steps.len()),
            summary: format!("plan: {} steps", steps.len()),
            diff: None,
        })
    }
}

pub struct RequestApprovalTool;

const APPROVAL_DEF: ToolDef = ToolDef {
    name: "request_approval",
    description: "Pause and ask the human user to approve a consequential action (purchases, publishing, submissions, deletions, account/billing changes...). Describe the exact payload and target. Never proceed without approval; there is no way to self-approve.",
    params: &[
        ParamSpec { name: "action", ty: ParamType::Str, required: true, description: "Short title, e.g. 'Publish blog post'." },
        ParamSpec { name: "payload", ty: ParamType::Str, required: true, description: "Exact content/target that will be submitted, published, or executed." },
    ],
};

#[async_trait]
impl Tool for RequestApprovalTool {
    fn def(&self) -> &'static ToolDef {
        &APPROVAL_DEF
    }
    fn risk(&self) -> Risk {
        Risk::Low // The tool itself is harmless; it exists to pause for approval.
    }
    async fn execute(
        &self,
        ctx: &ToolContext,
        args: &serde_json::Value,
    ) -> Result<ToolResult, ToolError> {
        let action = args["action"].as_str().expect("validated");
        let payload = args["payload"].as_str().expect("validated");
        let approved = ctx.approvals.request(&ctx.run_id, action, payload).await?;
        if approved {
            Ok(ToolResult {
                text: "approved by the user; you may proceed".into(),
                summary: format!("approved: {action}"),
                diff: None,
            })
        } else {
            Err(ToolError::Denied(format!(
                "the user did not approve: {action}"
            )))
        }
    }
}
