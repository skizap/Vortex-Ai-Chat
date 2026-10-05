//! Workspace file tools: list, read, search (read-only, risk: Low).

use crate::errors::ToolError;
use crate::registry::{Tool, ToolContext, ToolResult};
use crate::schema::{ParamSpec, ParamType, ToolDef};
use async_trait::async_trait;
use std::path::Path;

pub struct ListFilesTool;

const LIST_DEF: ToolDef = ToolDef {
    name: "list_files",
    description: "List files and directories under a path inside the approved workspace.",
    params: &[ParamSpec {
        name: "path",
        ty: ParamType::Str,
        required: false,
        description: "Subdirectory to list; empty or \".\" lists the workspace root.",
    }],
};

#[async_trait]
impl Tool for ListFilesTool {
    fn def(&self) -> &'static ToolDef {
        &LIST_DEF
    }
    async fn execute(
        &self,
        ctx: &ToolContext,
        args: &serde_json::Value,
    ) -> Result<ToolResult, ToolError> {
        let ws = ctx.require_workspace()?;
        let rel = args.get("path").and_then(|p| p.as_str()).unwrap_or(".");
        let dir = ws.resolve(rel)?;
        if !dir.is_dir() {
            return Err(ToolError::InvalidArgs(
                "path".into(),
                "not a directory".into(),
            ));
        }
        let mut entries: Vec<String> = Vec::new();
        let max = 500;
        let mut it = walkdir::WalkDir::new(&dir)
            .max_depth(2)
            .into_iter()
            .filter_entry(|e| {
                let n = e.file_name().to_string_lossy();
                n != ".git" && n != "node_modules"
            });
        while let Some(entry) = it.next() {
            let Ok(entry) = entry else { continue };
            let rel_path = entry
                .path()
                .strip_prefix(ws.root())
                .unwrap_or(entry.path())
                .to_string_lossy()
                .to_string();
            if rel_path.is_empty() {
                continue;
            }
            let marker = if entry.file_type().is_dir() { "/" } else { "" };
            entries.push(format!("{rel_path}{marker}"));
            if entries.len() >= max {
                entries.push("... (truncated)".into());
                break;
            }
        }
        entries.sort();
        Ok(ToolResult::text(entries.join("\n")))
    }
}

pub struct ReadFileTool;

const READ_DEF: ToolDef = ToolDef {
    name: "read_file",
    description: "Read a text file from the approved workspace (up to ~100KB).",
    params: &[ParamSpec {
        name: "path",
        ty: ParamType::Str,
        required: true,
        description: "File path relative to the workspace root.",
    }],
};

#[async_trait]
impl Tool for ReadFileTool {
    fn def(&self) -> &'static ToolDef {
        &READ_DEF
    }
    async fn execute(
        &self,
        ctx: &ToolContext,
        args: &serde_json::Value,
    ) -> Result<ToolResult, ToolError> {
        let ws = ctx.require_workspace()?;
        let rel = args["path"].as_str().expect("validated");
        let path = ws.resolve(rel)?;
        let metadata = std::fs::metadata(&path)
            .map_err(|e| ToolError::Execution(format!("cannot read '{}': {e}", rel)))?;
        if !metadata.is_file() {
            return Err(ToolError::InvalidArgs("path".into(), "not a file".into()));
        }
        if metadata.len() > 2_000_000 {
            return Err(ToolError::Execution(format!(
                "file '{rel}' is too large to read (>2MB)"
            )));
        }
        let content = tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| ToolError::Execution(format!("reading '{rel}': {e}")))?;
        let mut text = content;
        if text.len() > 100_000 {
            text.truncate(100_000);
            text.push_str("\n... (truncated)");
        }
        Ok(ToolResult::text(text))
    }
}

pub struct SearchFilesTool;

const SEARCH_DEF: ToolDef = ToolDef {
    name: "search_files",
    description:
        "Search file contents in the approved workspace with a regular expression (Rust syntax).",
    params: &[
        ParamSpec {
            name: "pattern",
            ty: ParamType::Str,
            required: true,
            description: "Regex pattern to find in file contents.",
        },
        ParamSpec {
            name: "path",
            ty: ParamType::Str,
            required: false,
            description: "Subdirectory to search; defaults to the workspace root.",
        },
    ],
};

#[async_trait]
impl Tool for SearchFilesTool {
    fn def(&self) -> &'static ToolDef {
        &SEARCH_DEF
    }
    async fn execute(
        &self,
        ctx: &ToolContext,
        args: &serde_json::Value,
    ) -> Result<ToolResult, ToolError> {
        let ws = ctx.require_workspace()?;
        let pattern = args["pattern"].as_str().expect("validated");
        let rel = args.get("path").and_then(|p| p.as_str()).unwrap_or(".");
        let root = ws.resolve(rel)?;
        let re = regex::Regex::new(pattern)
            .map_err(|e| ToolError::InvalidArgs("pattern".into(), format!("invalid regex: {e}")))?;
        let mut hits: Vec<String> = Vec::new();
        for entry in walkdir::WalkDir::new(&root)
            .max_depth(6)
            .into_iter()
            .filter_entry(|e| e.file_name().to_string_lossy() != ".git")
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_type().is_file()
                    && e.metadata().map(|m| m.len() < 1_000_000).unwrap_or(false)
            })
        {
            let Ok(content) = std::fs::read_to_string(entry.path()) else {
                continue;
            };
            for (i, line) in content.lines().enumerate() {
                if re.is_match(line) {
                    let rel_path = entry.path().strip_prefix(ws.root()).unwrap_or(entry.path());
                    hits.push(format!("{}:{}: {}", rel_path.display(), i + 1, line.trim()));
                    if hits.len() >= 100 {
                        hits.push("... (truncated)".into());
                        return Ok(ToolResult::text(hits.join("\n")));
                    }
                }
            }
        }
        if hits.is_empty() {
            Ok(ToolResult::text("no matches"))
        } else {
            Ok(ToolResult::text(hits.join("\n")))
        }
    }
}

/// Relative-to-root display helper shared with the write tools.
pub(crate) fn rel_display(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| path.to_string_lossy().to_string())
}
