//! `start_preview` tool (static + process modes).

use crate::errors::{Risk, ToolError};
use crate::preview::PreviewRegistry;
use crate::registry::{Tool, ToolContext, ToolResult};
use crate::schema::{ParamSpec, ParamType, ToolDef};
use async_trait::async_trait;
use std::path::PathBuf;

pub struct StartPreviewTool {
    pub registry: std::sync::Arc<PreviewRegistry>,
}

const START_DEF: ToolDef = ToolDef {
    name: "start_preview",
    description: "Start a local preview for a website project in the workspace. Static projects are served by the built-in loopback server; if `cmd` is given, that dev-server command is run instead (requires the project runtime and command execution to be enabled).",
    params: &[
        ParamSpec { name: "path", ty: ParamType::Str, required: true, description: "Project directory inside the workspace." },
        ParamSpec { name: "cmd", ty: ParamType::StrArray, required: false, description: "Optional dev-server argv (e.g. [\"npm\",\"run\",\"dev\"]). If omitted, the directory is served statically." },
    ],
};

#[async_trait]
impl Tool for StartPreviewTool {
    fn def(&self) -> &'static ToolDef {
        &START_DEF
    }
    fn risk(&self) -> Risk {
        Risk::Medium
    }
    fn available(&self, ctx: &ToolContext) -> Result<(), ToolError> {
        if !ctx.settings.tools.previews {
            return Err(ToolError::Unavailable(
                "previews are disabled in Settings (Tools).".into(),
            ));
        }
        if ctx.workspace.is_none() {
            return Err(ToolError::Unavailable(
                "no workspace is selected. Pick one in the UI sidebar first.".into(),
            ));
        }
        Ok(())
    }
    async fn execute(
        &self,
        ctx: &ToolContext,
        args: &serde_json::Value,
    ) -> Result<ToolResult, ToolError> {
        let ws = ctx.require_workspace()?;
        let rel = args["path"].as_str().expect("validated");
        let root = ws.resolve(rel)?;
        if !root.is_dir() {
            return Err(ToolError::InvalidArgs(
                "path".into(),
                "project directory not found".into(),
            ));
        }
        let cmd: Option<Vec<String>> = args.get("cmd").and_then(|c| c.as_array()).map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        });
        match cmd {
            None => {
                let url = self.registry.start_static(&ctx.run_id, root)?;
                Ok(ToolResult {
                    summary: format!("static preview at {url}"),
                    text: format!(
                        "Preview running at {url}\nIt is bound to 127.0.0.1 and is served by \
                         Vortex. Open the URL in your browser to review the site."
                    ),
                    diff: None,
                })
            }
            Some(cmd) => run_process_preview(self.registry.clone(), ctx, root, cmd).await,
        }
    }
}

/// Dev-server previews are command execution: approval is required, and the
/// child is spawned in its own process group so cancellation can clean it up.
async fn run_process_preview(
    registry: std::sync::Arc<PreviewRegistry>,
    ctx: &ToolContext,
    root: PathBuf,
    cmd: Vec<String>,
) -> Result<ToolResult, ToolError> {
    use crate::tools_command::{kill_process_group, resolve_binary_pub};
    if cmd.is_empty() {
        return Err(ToolError::InvalidArgs(
            "cmd".into(),
            "argv must not be empty".into(),
        ));
    }
    if !ctx.settings.tools.commands {
        return Err(ToolError::Unavailable(
            "process-based previews require command execution, which is disabled in Settings \
             (Tools). Static previews still work without it."
                .into(),
        ));
    }
    let payload = serde_json::to_string_pretty(&serde_json::json!({
        "cmd": cmd,
        "cwd": root.display().to_string(),
    }))
    .unwrap_or_default();
    let approved = ctx
        .approvals
        .request(&ctx.run_id, "Approve dev-server preview", &payload)
        .await?;
    if !approved {
        return Err(ToolError::Denied(
            "the user declined to start this dev server".into(),
        ));
    }
    let binary = resolve_binary_pub(&cmd[0])?;
    let mut proc = tokio::process::Command::new(&binary);
    proc.args(&cmd[1..])
        .current_dir(&root)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    proc.process_group(0);
    let mut child = proc
        .spawn()
        .map_err(|e| ToolError::Execution(format!("starting dev server: {e}")))?;
    let pid = child.id();
    let port = wait_for_port(&mut child).await;
    let Some(port) = port else {
        if let Some(pid) = pid {
            kill_process_group(pid as i32).await;
        }
        return Err(ToolError::Execution(
            "could not detect the dev server port. Ensure the command binds to 127.0.0.1 and \
             prints its URL/port."
                .into(),
        ));
    };
    let url = format!("http://127.0.0.1:{port}");
    registry.attach_child(
        &ctx.run_id,
        root,
        url.clone(),
        pid.unwrap() as u32,
        ctx.cancel.clone(),
    );
    Ok(ToolResult {
        summary: format!("dev-server preview at {url}"),
        text: format!(
            "Dev server running at {url} (bound to loopback). It will be stopped when this \
             task ends or is stopped."
        ),
        diff: None,
    })
}

/// Read the child's stdout looking for a bound port, bounded to ~20 seconds.
async fn wait_for_port(child: &mut tokio::process::Child) -> Option<u16> {
    use tokio::io::{AsyncBufReadExt, BufReader};
    let stdout = child.stdout.take()?;
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let re = regex::Regex::new(r"(?:localhost|127\.0\.0\.1|0\.0\.0\.0):(\d{2,5})|port\s+(\d{2,5})")
        .ok()?;
    loop {
        if std::time::Instant::now() > deadline {
            return None;
        }
        line.clear();
        let n = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            reader.read_line(&mut line),
        )
        .await
        .ok()?
        .ok()?;
        if n == 0 {
            return None;
        }
        let lower = line.to_lowercase();
        if let Some(caps) = re.captures(&lower) {
            let port = caps
                .get(1)
                .or_else(|| caps.get(2))
                .and_then(|m| m.as_str().parse::<u16>().ok())?;
            if port > 0 && port != 8417 {
                return Some(port);
            }
        }
    }
}
