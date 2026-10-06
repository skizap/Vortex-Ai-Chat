//! Tool trait, per-run context, and the permission-enforcing registry.

use crate::errors::{Risk, ToolError};
use crate::schema::ToolDef;
use crate::workspace::WorkspaceRoots;
use async_trait::async_trait;
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use vortex_core::Db;
use vortex_types::{RunEvent, Settings};

/// Human-in-the-loop approval gate. The concrete implementation lives in the
/// orchestrator; tools only see this trait, so a sub-agent can never
/// self-approve: decisions only originate from the server API (the user).
#[async_trait]
pub trait ApprovalGate: Send + Sync {
    async fn request(&self, run_id: &str, title: &str, payload: &str) -> Result<bool, ToolError>;
}

/// Everything a tool may touch, scoped to one run. Credentials never appear
/// here; capabilities that are not configured arrive as `None` so tools fail
/// with honest "unavailable" errors.
#[derive(Clone)]
pub struct ToolContext {
    pub run_id: String,
    pub root_run_id: String,
    pub depth: u32,
    pub settings: Settings,
    pub allowed: HashSet<String>,
    pub events: mpsc::Sender<RunEvent>,
    pub cancel: CancellationToken,
    pub approvals: Arc<dyn ApprovalGate>,
    pub workspace: Option<Arc<WorkspaceRoots>>,
    pub db: Option<Db>,
    pub conversation_id: Option<String>,
    pub file_state: Arc<crate::file_state::FileState>,
    pub http: crate::safe_http::SafeHttp,
    pub search: Option<Arc<dyn crate::search::SearchProvider>>,
    pub browser: Option<Arc<crate::browser::BrowserManager>>,
    pub previews: Arc<crate::preview::PreviewRegistry>,
}

impl ToolContext {
    pub(crate) fn require_workspace(&self) -> Result<Arc<WorkspaceRoots>, ToolError> {
        self.workspace.clone().ok_or_else(|| {
            ToolError::Unavailable(
                "no workspace is selected. Pick a workspace directory in the UI sidebar \
                 (Workspace selector) and retry."
                    .into(),
            )
        })
    }
}

/// Result of a successful tool execution.
#[derive(Debug, Clone, Default)]
pub struct ToolResult {
    /// Text returned to the model as the tool result.
    pub text: String,
    /// One-line human summary for the task panel.
    pub summary: String,
    /// Unified diff when the tool changed files.
    pub diff: Option<String>,
}

impl ToolResult {
    pub fn text(text: impl Into<String>) -> Self {
        let text = text.into();
        let summary: String = text.chars().take(120).collect();
        Self {
            text,
            summary,
            diff: None,
        }
    }
}

/// What `ToolRegistry::execute` returns: the result plus success status.
#[derive(Debug, Clone)]
pub struct ToolOutcome {
    pub ok: bool,
    pub result: ToolResult,
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn def(&self) -> &'static ToolDef;
    fn risk(&self) -> Risk {
        Risk::Low
    }
    /// Whether the tool is currently usable given context (workspace set,
    /// search configured, browser present, etc.). Errors must explain setup.
    fn available(&self, _ctx: &ToolContext) -> Result<(), ToolError> {
        Ok(())
    }
    async fn execute(
        &self,
        ctx: &ToolContext,
        args: &serde_json::Value,
    ) -> Result<ToolResult, ToolError>;
}

pub struct ToolRegistry {
    tools: Vec<std::sync::Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self { tools: Vec::new() }
    }

    pub fn register(&mut self, tool: std::sync::Arc<dyn Tool>) {
        self.tools.push(tool);
    }

    pub fn get(&self, name: &str) -> Option<&std::sync::Arc<dyn Tool>> {
        self.tools.iter().find(|t| t.def().name == name)
    }

    pub fn names(&self) -> Vec<&'static str> {
        self.tools.iter().map(|t| t.def().name).collect()
    }

    /// Schemas for the tools enabled for this run, intersected with the
    /// run's allow-list. The registry, not the model, decides what exists.
    pub fn schemas(&self, allowed: &HashSet<String>) -> Vec<vortex_llm::ToolSchema> {
        self.tools
            .iter()
            .filter(|t| allowed.contains(t.def().name))
            .map(|t| t.def().json_schema())
            .collect()
    }

    /// Validate + authorize + execute one tool call. This is the single
    /// enforcement point for names, argument schemas, and risk levels.
    pub async fn execute(
        &self,
        ctx: &ToolContext,
        name: &str,
        args: &serde_json::Value,
    ) -> ToolOutcome {
        let failure = |e: ToolError| ToolOutcome {
            ok: false,
            result: ToolResult {
                text: format!("error: {e}"),
                summary: e.to_string(),
                diff: None,
            },
        };
        let success = |r: ToolResult| ToolOutcome {
            ok: true,
            result: r,
        };
        let Some(tool) = self.get(name) else {
            return failure(ToolError::NotPermitted(format!(
                "tool '{name}' is not available in this session (not enabled, unconfigured, or outside your permission profile)"
            )));
        };
        if !ctx.allowed.contains(name) {
            return failure(ToolError::NotPermitted(format!(
                "tool '{name}' is not permitted for this agent run"
            )));
        }
        if let Err(e) = tool.available(ctx) {
            return failure(e);
        }
        let def = tool.def();
        if let Err(msg) = def.validate(args) {
            return failure(ToolError::InvalidArgs(name.to_string(), msg));
        }
        let filtered = def.filter(args.clone());
        match tool.risk() {
            Risk::Consequential => {
                let payload = serde_json::to_string_pretty(&filtered).unwrap_or_default();
                let title = format!("Approve {}: {}", def.name, def.description);
                match ctx.approvals.request(&ctx.run_id, &title, &payload).await {
                    Ok(true) => {}
                    Ok(false) => {
                        return failure(ToolError::Denied(format!(
                            "the user declined this action ({})",
                            def.name
                        )))
                    }
                    Err(e) => return failure(e),
                }
            }
            _ => {}
        }
        match tool.execute(ctx, &filtered).await {
            Ok(r) => success(r),
            Err(e) => failure(e),
        }
    }
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Helper for tests: a never-approving gate.
pub struct DenyGate;

#[async_trait]
impl ApprovalGate for DenyGate {
    async fn request(
        &self,
        _run_id: &str,
        _title: &str,
        _payload: &str,
    ) -> Result<bool, ToolError> {
        Ok(false)
    }
}

/// Helper: an auto-approving gate for unit tests (never used in production).
pub struct AutoApproveGate;

#[async_trait]
impl ApprovalGate for AutoApproveGate {
    async fn request(
        &self,
        _run_id: &str,
        _title: &str,
        _payload: &str,
    ) -> Result<bool, ToolError> {
        Ok(true)
    }
}

/// Context constructor helper for tests.
pub fn test_context(
    settings: Settings,
    allowed: &[&str],
) -> (ToolContext, mpsc::Receiver<RunEvent>, CancellationToken) {
    let (tx, rx) = mpsc::channel(64);
    let cancel = CancellationToken::new();
    let ctx = ToolContext {
        run_id: "test-run".into(),
        root_run_id: "test-run".into(),
        depth: 0,
        settings,
        allowed: allowed.iter().map(|s| s.to_string()).collect(),
        events: tx,
        cancel: cancel.clone(),
        approvals: Arc::new(AutoApproveGate),
        workspace: None,
        db: None,
        conversation_id: None,
        file_state: Arc::new(crate::file_state::FileState::new()),
        http: crate::safe_http::SafeHttp::new(false),
        search: None,
        browser: None,
        previews: Arc::new(crate::preview::PreviewRegistry::new()),
    };
    (ctx, rx, cancel)
}
