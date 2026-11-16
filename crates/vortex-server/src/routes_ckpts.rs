//! Checkpoint recovery routes: list a conversation's file snapshots and
//! restore one on explicit user action.
//!
//! Trust model: restore is a *human* recovery action (the UI asks for
//! confirmation first), reachable only by the local user through the same
//! loopback/LAN-token guard as every other API route — models have no path
//! to it beyond the run-scoped tools they are given. It therefore does NOT
//! go through the run approval gate, and it intentionally bypasses
//! `file_state` run-ownership checks: it operates outside any run, on behalf
//! of the person who owns the workspace. Restores are confined to the
//! currently configured workspace and are themselves reversible (the current
//! content is snapshotted first).

use crate::state::{err_json, AppState};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use std::collections::HashMap;
use std::path::{Path as StdPath, PathBuf};
use std::sync::Arc;

/// GET /api/conversations/{id}/checkpoints?before=&limit= — newest first,
/// cursor-paginated: pass the smallest `id` of the previous page as `before`
/// to walk further back. Snapshot content is never included in the list.
pub async fn list_checkpoints(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> axum::response::Response {
    let before = q.get("before").and_then(|s| s.parse::<i64>().ok());
    let limit = q
        .get("limit")
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(50);
    match state.db.list_checkpoints(id, before, limit).await {
        Ok(list) => Json(list).into_response(),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

/// Resolve a stored (absolute) checkpoint path inside the *currently*
/// configured workspace. The workspace may have changed since the snapshot
/// was taken; restoring outside the current root would be a confinement
/// escape, so it is refused.
fn resolve_in_workspace(workspace: Option<&str>, stored_path: &str) -> Result<PathBuf, String> {
    let Some(ws) = workspace.filter(|s| !s.trim().is_empty()) else {
        return Err(
            "no workspace is configured; select the workspace this checkpoint belongs to first"
                .to_string(),
        );
    };
    let roots = vortex_agents::workspace::WorkspaceRoots::new(StdPath::new(ws))
        .map_err(|e| e.to_string())?;
    roots
        .resolve(stored_path)
        .map_err(|e| format!("checkpoint path is outside the current workspace '{ws}': {e}"))
}

/// POST /api/checkpoints/{id}/restore — put a file back to its snapshotted
/// state.
///
/// 1. The current content is snapshotted FIRST (same conversation,
///    `run_id = "restore"`), so the restore itself is reversible.
/// 2. `has_original` → the original bytes are written back.
///    `!has_original` (the file did not exist at snapshot time) → the current
///    file is moved to the trash directory (same mechanics as `delete_path`,
///    never unlinked).
pub async fn restore_checkpoint(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> axum::response::Response {
    let (conversation_id, stored_path, original) = match state.db.get_checkpoint(id).await {
        Ok(Some(row)) => row,
        Ok(None) => return err_json(StatusCode::NOT_FOUND, "checkpoint not found"),
        Err(e) => return err_json(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };

    let settings = state.engine.current_settings().await;
    let path = match resolve_in_workspace(settings.workspace.as_deref(), &stored_path) {
        Ok(p) => p,
        Err(e) => return err_json(StatusCode::BAD_REQUEST, e),
    };

    // Snapshot the current content first (reversibility). Same honesty rule
    // as the write tools: absent → NULL; present but non-UTF-8 → refuse,
    // since the snapshot could not represent it and the restore would be
    // irreversible.
    let current = match tokio::fs::read_to_string(&path).await {
        Ok(text) => Some(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) if !path.exists() => None,
        Err(e) => {
            return err_json(
                StatusCode::BAD_REQUEST,
                format!(
                    "current content of '{}' is not readable as text ({e}); refusing to \
                     overwrite it without a reversible snapshot",
                    path.display()
                ),
            )
        }
    };
    if let Err(e) = state
        .db
        .add_checkpoint(
            conversation_id,
            "restore".into(),
            path.display().to_string(),
            current,
        )
        .await
    {
        return err_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("could not record the pre-restore snapshot: {e}"),
        );
    }

    let action = match original {
        Some(text) => {
            if let Some(parent) = path.parent() {
                if let Err(e) = tokio::fs::create_dir_all(parent).await {
                    return err_json(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("creating directory: {e}"),
                    );
                }
            }
            if let Err(e) = tokio::fs::write(&path, text).await {
                return err_json(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("writing file: {e}"),
                );
            }
            "restored"
        }
        None => {
            if path.exists() {
                let trash_root = vortex_agents::trash_dir();
                if let Err(e) = tokio::fs::create_dir_all(&trash_root).await {
                    return err_json(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("preparing trash dir: {e}"),
                    );
                }
                let ts = chrono::Utc::now().timestamp_millis();
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| "file".into());
                let target = trash_root.join(format!("{ts}_{name}"));
                if let Err(e) = tokio::fs::rename(&path, &target).await {
                    return err_json(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("moving to trash: {e}"),
                    );
                }
            }
            "trashed"
        }
    };
    Json(serde_json::json!({
        "ok": true,
        "path": path.display().to_string(),
        "action": action,
    }))
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::resolve_in_workspace;

    #[test]
    fn restore_path_must_stay_inside_the_current_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().to_string_lossy().to_string();
        let inside = dir.path().join("sub/f.txt");
        let inside_str = inside.to_string_lossy().to_string();

        // No workspace configured → refused with setup guidance.
        let err = resolve_in_workspace(None, &inside_str).unwrap_err();
        assert!(err.contains("no workspace is configured"), "{err}");

        // Inside the workspace → resolved absolute path.
        let p = resolve_in_workspace(Some(&ws), &inside_str).unwrap();
        assert!(p.starts_with(dir.path()));

        // Outside the workspace (e.g. the workspace changed since the
        // snapshot) → refused.
        let outside = std::fs::canonicalize(dir.path())
            .unwrap()
            .parent()
            .unwrap()
            .join("vortex-restore-escape.txt");
        let err = resolve_in_workspace(Some(&ws), &outside.to_string_lossy()).unwrap_err();
        assert!(err.contains("outside the current workspace"), "{err}");
    }
}
