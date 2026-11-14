//! Workspace write tools: write_file, edit_file, delete_path.
//!
//! Every modification:
//! - serializes on the per-file lock,
//! - enforces cross-agent ownership rules,
//! - snapshots the previous content as a checkpoint (when a DB is attached),
//! - reports a unified diff in the tool result.

use crate::errors::{Risk, ToolError};
use crate::registry::{Tool, ToolContext, ToolResult};
use crate::schema::{ParamSpec, ParamType, ToolDef};
use crate::tools_files::rel_display;
use async_trait::async_trait;
use std::path::Path;
use vortex_types::PermissionProfile;

fn profile_allows_writes(ctx: &ToolContext) -> Result<(), ToolError> {
    match ctx.settings.profile {
        PermissionProfile::Restricted => Err(ToolError::NotPermitted(
            "the permission profile is Restricted; file writes are disabled. \
             Change the profile in Settings to Standard or Trusted."
                .into(),
        )),
        _ => Ok(()),
    }
}

async fn checkpoint(ctx: &ToolContext, ws: &crate::workspace::WorkspaceRoots, path: &Path) {
    // Distinguish "file absent" (record `original = NULL`, meaning: did not
    // exist — a restore trashes the file) from "present but not readable as
    // UTF-8 text" (record NOTHING: a NULL would wrongly claim the file never
    // existed, and a restore would then trash a binary file instead of
    // putting its content back — silently destroying data).
    let original = match tokio::fs::read_to_string(path).await {
        Ok(text) => Some(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            tracing::warn!(
                "skipping checkpoint for '{}': the file exists but is not readable as UTF-8 \
                 text ({e}); a text snapshot could not represent it faithfully",
                path.display()
            );
            return;
        }
    };
    if let (Some(db), Some(conv)) = (&ctx.db, &ctx.conversation_id) {
        let res = db
            .add_checkpoint(
                conv.clone(),
                ctx.run_id.clone(),
                path.display().to_string(),
                original,
            )
            .await;
        if let Err(e) = res {
            tracing::warn!("checkpoint failed: {e}");
        }
    }
    let _ = ws;
}

fn unified_diff(before: Option<&str>, after: &str) -> String {
    let before_text = before.unwrap_or("");
    let mut output = String::new();
    for change in similar::TextDiff::from_lines(before_text, after).iter_all_changes() {
        let sign = match change.tag() {
            similar::ChangeTag::Delete => '-',
            similar::ChangeTag::Insert => '+',
            similar::ChangeTag::Equal => ' ',
        };
        output.push(sign);
        output.push_str(change.value());
    }
    if output.len() > 20_000 {
        let mut out = output;
        out.truncate(20_000);
        out.push_str("\n... (diff truncated)\n");
        return out;
    }
    output
}

async fn guarded_write(
    ctx: &ToolContext,
    ws: &crate::workspace::WorkspaceRoots,
    path: &Path,
    new_content: &str,
) -> Result<(String, String), ToolError> {
    let lock = ctx.file_state.lock_for(path);
    let _guard = lock.lock().await;
    if let Err(conflict) = ctx
        .file_state
        .check_write_conflict(path, &ctx.run_id, &ctx.root_run_id)
    {
        return Err(ToolError::NotPermitted(conflict));
    }
    checkpoint(ctx, ws, path).await;
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| ToolError::Execution(format!("creating directory: {e}")))?;
    }
    let before = tokio::fs::read_to_string(path).await.ok();
    tokio::fs::write(path, new_content)
        .await
        .map_err(|e| ToolError::Execution(format!("writing file: {e}")))?;
    ctx.file_state.record_write(path, &ctx.run_id);
    Ok((
        unified_diff(before.as_deref(), new_content),
        rel_display(ws.root(), path),
    ))
}

pub struct WriteFileTool;

pub struct EditFileTool;

const EDIT_DEF: ToolDef = ToolDef {
    name: "edit_file",
    description: "Replace one exact text occurrence in a workspace file. Use for precise edits instead of rewriting whole files.",
    params: &[
        ParamSpec { name: "path", ty: ParamType::Str, required: true, description: "File path relative to the workspace root." },
        ParamSpec { name: "old_text", ty: ParamType::Str, required: true, description: "Exact existing text to replace (must occur exactly once)." },
        ParamSpec { name: "new_text", ty: ParamType::Str, required: true, description: "Replacement text." },
    ],
};

#[async_trait]
impl Tool for EditFileTool {
    fn def(&self) -> &'static ToolDef {
        &EDIT_DEF
    }
    fn risk(&self) -> Risk {
        Risk::Medium
    }
    async fn execute(
        &self,
        ctx: &ToolContext,
        args: &serde_json::Value,
    ) -> Result<ToolResult, ToolError> {
        profile_allows_writes(ctx)?;
        let ws = ctx.require_workspace()?;
        let rel = args["path"].as_str().expect("validated");
        let old_text = args["old_text"].as_str().expect("validated");
        let new_text = args["new_text"].as_str().expect("validated");
        let path = ws.resolve(rel)?;
        let lock = ctx.file_state.lock_for(&path);
        let _guard = lock.lock().await;
        if let Err(conflict) =
            ctx.file_state
                .check_write_conflict(&path, &ctx.run_id, &ctx.root_run_id)
        {
            return Err(ToolError::NotPermitted(conflict));
        }
        let original = tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| ToolError::Execution(format!("reading '{rel}': {e}")))?;
        let count = original.matches(old_text).count();
        if count != 1 {
            return Err(ToolError::InvalidArgs(
                "old_text".into(),
                format!("old_text occurs {count} times; it must occur exactly once"),
            ));
        }
        let updated = original.replacen(old_text, new_text, 1);
        let diff = unified_diff(Some(&original), &updated);
        checkpoint(ctx, ws.as_ref(), &path).await;
        tokio::fs::write(&path, &updated)
            .await
            .map_err(|e| ToolError::Execution(format!("writing file: {e}")))?;
        ctx.file_state.record_write(&path, &ctx.run_id);
        let display = rel_display(ws.root(), &path);
        Ok(ToolResult {
            text: format!("edited {display}"),
            summary: format!("edited {display}"),
            diff: Some(diff),
        })
    }
}

/// Deletes are recoverable: the file is moved into the Vortex trash directory
/// (documented in README) instead of being unlinked. Risk: Consequential, so
/// the registry always requires explicit human approval first.
pub struct DeletePathTool;

const DELETE_DEF: ToolDef = ToolDef {
    name: "delete_path",
    description: "Delete a file inside the approved workspace. The content is moved to a local trash area, not destroyed, and requires user approval.",
    params: &[ParamSpec { name: "path", ty: ParamType::Str, required: true, description: "File path to delete (files only)." }],
};

#[async_trait]
impl Tool for DeletePathTool {
    fn def(&self) -> &'static ToolDef {
        &DELETE_DEF
    }
    fn risk(&self) -> Risk {
        Risk::Consequential
    }
    async fn execute(
        &self,
        ctx: &ToolContext,
        args: &serde_json::Value,
    ) -> Result<ToolResult, ToolError> {
        profile_allows_writes(ctx)?;
        let ws = ctx.require_workspace()?;
        let rel = args["path"].as_str().expect("validated");
        let path = ws.resolve(rel)?;
        if !path.is_file() {
            return Err(ToolError::InvalidArgs(
                "path".into(),
                "only files can be deleted with this tool".into(),
            ));
        }
        checkpoint(ctx, ws.as_ref(), &path).await;
        let trash_root = crate::trash_dir();
        let ts = chrono::Utc::now().timestamp_millis();
        let target = trash_root.join(format!(
            "{ts}_{}",
            rel_display(ws.root(), &path).replace('/', "_")
        ));
        tokio::fs::create_dir_all(&trash_root)
            .await
            .map_err(|e| ToolError::Execution(format!("preparing trash dir: {e}")))?;
        tokio::fs::rename(&path, &target)
            .await
            .map_err(|e| ToolError::Execution(format!("moving to trash: {e}")))?;
        let display = rel_display(ws.root(), &path);
        Ok(ToolResult {
            text: format!("moved {display} to local trash ({})", target.display()),
            summary: format!("deleted {display} (recoverable)"),
            diff: None,
        })
    }
}

const WRITE_DEF: ToolDef = ToolDef {
    name: "write_file",
    description: "Create or overwrite a text file in the approved workspace. The previous version is snapshotted for recovery.",
    params: &[
        ParamSpec { name: "path", ty: ParamType::Str, required: true, description: "File path relative to the workspace root." },
        ParamSpec { name: "content", ty: ParamType::Str, required: true, description: "Full new file content." },
    ],
};

#[async_trait]
impl Tool for WriteFileTool {
    fn def(&self) -> &'static ToolDef {
        &WRITE_DEF
    }
    fn risk(&self) -> Risk {
        Risk::Medium
    }
    async fn execute(
        &self,
        ctx: &ToolContext,
        args: &serde_json::Value,
    ) -> Result<ToolResult, ToolError> {
        profile_allows_writes(ctx)?;
        let ws = ctx.require_workspace()?;
        let rel = args["path"].as_str().expect("validated");
        let content = args["content"].as_str().expect("validated");
        let path = ws.resolve(rel)?;
        let (diff, display) = guarded_write(ctx, ws.as_ref(), &path, content).await?;
        Ok(ToolResult {
            text: format!("wrote {display} ({} bytes)", content.len()),
            summary: format!("wrote {display}"),
            diff: Some(diff),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vortex_types::Settings;

    async fn ctx_with_db(
        dir: &tempfile::TempDir,
    ) -> (ToolContext, vortex_core::Db, tempfile::TempDir) {
        let db_dir = tempfile::tempdir().unwrap();
        let db = vortex_core::Db::open(db_dir.path().join("t.db")).unwrap();
        let settings = Settings {
            workspace: Some(dir.path().to_string_lossy().to_string()),
            ..Default::default()
        };
        let (mut ctx, _rx, _cancel) = crate::registry::test_context(settings, &[]);
        ctx.db = Some(db.clone());
        ctx.conversation_id = Some("conv-ckpt".into());
        ctx.run_id = "run-ckpt".into();
        (ctx, db, db_dir)
    }

    /// Recording flaw fix: a file that exists but is not valid UTF-8 must NOT
    /// get a checkpoint row — a NULL `original` would mean "didn't exist",
    /// and a later restore would trash the binary file instead of keeping it.
    #[tokio::test]
    async fn checkpoint_skips_present_non_utf8_files() {
        let dir = tempfile::tempdir().unwrap();
        let (ctx, db, _db_dir) = ctx_with_db(&dir).await;
        let ws = crate::workspace::WorkspaceRoots::new(dir.path()).unwrap();

        let bin = dir.path().join("data.bin");
        std::fs::write(&bin, [0xff_u8, 0xfe, 0x00, 0x80, 0xff]).unwrap();
        checkpoint(&ctx, &ws, &bin).await;
        let list = db
            .list_checkpoints("conv-ckpt".into(), None, 100)
            .await
            .unwrap();
        assert!(
            list.is_empty(),
            "a non-UTF-8 file must not produce a (misleading NULL) checkpoint row: {list:?}"
        );
    }

    /// Text files snapshot their content; absent files record `NULL`
    /// ("did not exist") — the two cases must stay distinguishable.
    #[tokio::test]
    async fn checkpoint_records_text_and_absent_files() {
        let dir = tempfile::tempdir().unwrap();
        let (ctx, db, _db_dir) = ctx_with_db(&dir).await;
        let ws = crate::workspace::WorkspaceRoots::new(dir.path()).unwrap();

        let txt = dir.path().join("notes.txt");
        std::fs::write(&txt, "hello").unwrap();
        checkpoint(&ctx, &ws, &txt).await;
        let absent = dir.path().join("brand-new.txt");
        checkpoint(&ctx, &ws, &absent).await;

        let list = db
            .list_checkpoints("conv-ckpt".into(), None, 100)
            .await
            .unwrap();
        assert_eq!(list.len(), 2);
        // Newest first: the absent file, flagged has_original = false.
        assert!(list[0].path.ends_with("brand-new.txt"));
        assert!(!list[0].has_original);
        assert!(list[1].path.ends_with("notes.txt"));
        assert!(list[1].has_original);
        let (_, _, original) = db.get_checkpoint(list[1].id).await.unwrap().unwrap();
        assert_eq!(original.as_deref(), Some("hello"));
    }
}
