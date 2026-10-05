//! `stop_preview` tool.

use crate::errors::ToolError;
use crate::preview::PreviewRegistry;
use crate::registry::{Tool, ToolContext, ToolResult};
use crate::schema::{ParamSpec, ParamType, ToolDef};
use async_trait::async_trait;

pub struct StopPreviewTool {
    pub registry: std::sync::Arc<PreviewRegistry>,
}

const STOP_DEF: ToolDef = ToolDef {
    name: "stop_preview",
    description: "Stop a preview server started earlier in this run.",
    params: &[ParamSpec {
        name: "url",
        ty: ParamType::Str,
        required: true,
        description: "The preview URL that was reported when it started.",
    }],
};

#[async_trait]
impl Tool for StopPreviewTool {
    fn def(&self) -> &'static ToolDef {
        &STOP_DEF
    }
    fn available(&self, ctx: &ToolContext) -> Result<(), ToolError> {
        if !ctx.settings.tools.previews {
            return Err(ToolError::Unavailable(
                "previews are disabled in Settings (Tools).".into(),
            ));
        }
        Ok(())
    }
    async fn execute(
        &self,
        ctx: &ToolContext,
        args: &serde_json::Value,
    ) -> Result<ToolResult, ToolError> {
        let url = args["url"].as_str().expect("validated");
        let found = self
            .registry
            .list()
            .into_iter()
            .find(|p| p.url == url && p.run_id == ctx.run_id);
        match found {
            Some(p) => {
                self.registry.stop(&p.id);
                Ok(ToolResult::text(format!("stopped preview {url}")))
            }
            None => Err(ToolError::Execution(format!(
                "no active preview {url} owned by this run"
            ))),
        }
    }
}
