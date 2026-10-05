//! Workspace confinement: all file tools resolve paths through this guard.
//!
//! Defends against:
//! - absolute paths pointing outside the root,
//! - `..` traversal,
//! - symlink components escaping the root.

use std::path::{Component, Path, PathBuf};

use crate::errors::ToolError;

#[derive(Debug, Clone)]
pub struct WorkspaceRoots {
    root: PathBuf, // canonicalized
}

impl WorkspaceRoots {
    pub fn new(path: &Path) -> Result<Self, ToolError> {
        let canonical = std::fs::canonicalize(path).map_err(|e| {
            ToolError::InvalidArgs(
                "workspace".into(),
                format!("workspace root '{}' cannot be used: {e}", path.display()),
            )
        })?;
        if !canonical.is_dir() {
            return Err(ToolError::InvalidArgs(
                "workspace".into(),
                "workspace root must be a directory".into(),
            ));
        }
        Ok(Self { root: canonical })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Resolve a model-supplied path to an absolute path inside the root.
    /// The path may be given relative to the root or absolute (in which case
    /// it must already be inside the root).
    pub fn resolve(&self, user_path: &str) -> Result<PathBuf, ToolError> {
        let user_path = user_path.trim();
        if user_path.is_empty() {
            return Err(ToolError::InvalidArgs(
                "path".into(),
                "path must not be empty".into(),
            ));
        }
        let raw = Path::new(user_path);
        let rel: PathBuf = if raw.is_absolute() {
            match raw.strip_prefix(&self.root) {
                Ok(r) => r.to_path_buf(),
                Err(_) => {
                    return Err(ToolError::NotPermitted(format!(
                        "path '{}' is outside the approved workspace",
                        user_path
                    )))
                }
            }
        } else {
            raw.to_path_buf()
        };

        // Lexical traversal check first.
        if rel.components().any(|c| matches!(c, Component::ParentDir)) {
            return Err(ToolError::NotPermitted(format!(
                "path '{}' may not contain '..'",
                user_path
            )));
        }

        // Walk components, canonicalizing existing ancestors, so symlinks
        // cannot carry us out of the root.
        let mut current = self.root.clone();
        for comp in rel.components() {
            match comp {
                Component::Normal(part) => {
                    current.push(part);
                    if let Ok(canonical) = std::fs::canonicalize(&current) {
                        if !canonical.starts_with(&self.root) {
                            return Err(ToolError::NotPermitted(format!(
                                "path '{}' resolves outside the approved workspace (symlink escape)",
                                user_path
                            )));
                        }
                        current = canonical;
                    }
                }
                Component::CurDir => {}
                _ => unreachable!("handled above"),
            }
        }
        Ok(current)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (tempfile::TempDir, WorkspaceRoots) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("a.txt"), "hi").unwrap();
        let roots = WorkspaceRoots::new(dir.path()).unwrap();
        (dir, roots)
    }

    #[test]
    fn resolves_relative_and_absolute_inside() {
        let (_d, roots) = setup();
        // `..` is rejected outright (simplest airtight traversal policy).
        assert!(roots.resolve("sub/../a.txt").is_err());
        let p = roots.resolve("a.txt").unwrap();
        assert_eq!(p, roots.root().join("a.txt"));
        let abs = roots.root().join("a.txt");
        let p2 = roots.resolve(abs.to_str().unwrap()).unwrap();
        assert_eq!(p2, abs);
        let p3 = roots.resolve("sub/notes.txt").unwrap();
        assert!(p3.starts_with(roots.root().join("sub")));
    }

    #[test]
    fn rejects_parent_traversal() {
        let (_d, roots) = setup();
        assert!(roots.resolve("../../../etc/passwd").is_err());
        assert!(roots.resolve("a/../../x").is_err());
    }

    #[test]
    fn rejects_outside_absolute() {
        let (_d, roots) = setup();
        assert!(roots.resolve("/etc/passwd").is_err());
    }

    #[test]
    fn rejects_symlink_escape() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("ws")).unwrap();
        std::fs::create_dir_all(dir.path().join("outside")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(dir.path().join("outside"), dir.path().join("ws/escape"))
            .unwrap();
        let roots = WorkspaceRoots::new(&dir.path().join("ws")).unwrap();
        let err = roots.resolve("escape/secret.txt").unwrap_err();
        assert!(err.to_string().contains("outside the approved workspace"));
    }

    #[test]
    fn allows_symlink_inside_workspace() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("real")).unwrap();
        std::fs::write(dir.path().join("real/f.txt"), "x").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("real", dir.path().join("link")).unwrap();
        let roots = WorkspaceRoots::new(dir.path()).unwrap();
        let p = roots.resolve("link/f.txt").unwrap();
        assert!(p.starts_with(roots.root()));
    }
}
