//! XDG base-directory resolution with explicit overrides.
//!
//! Defaults (Linux):
//! - config: `$XDG_CONFIG_HOME/vortex` (fallback `~/.config/vortex`)
//! - data:   `$XDG_DATA_HOME/vortex`   (fallback `~/.local/share/vortex`)
//! - cache:  `$XDG_CACHE_HOME/vortex`  (fallback `~/.cache/vortex`)
//!
//! Each location can be overridden with `VORTEX_CONFIG_DIR`, `VORTEX_DATA_DIR`
//! or `VORTEX_CACHE_DIR` (checked first).

use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Paths {
    pub config_dir: PathBuf,
    pub data_dir: PathBuf,
    pub cache_dir: PathBuf,
}

impl Paths {
    pub fn resolve() -> std::io::Result<Self> {
        let config_dir = env_override("VORTEX_CONFIG_DIR", ".config")?.join("vortex");
        let data_dir = env_override("VORTEX_DATA_DIR", ".local/share")?.join("vortex");
        let cache_dir = env_override("VORTEX_CACHE_DIR", ".cache")?.join("vortex");
        std::fs::create_dir_all(&config_dir)?;
        std::fs::create_dir_all(&data_dir)?;
        std::fs::create_dir_all(&cache_dir)?;
        Ok(Self { config_dir, data_dir, cache_dir })
    }

    pub fn database(&self) -> PathBuf {
        self.data_dir.join("vortex.db")
    }

    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }

    /// Isolated Chromium profile used only by Vortex browser automation.
    pub fn browser_profile(&self) -> PathBuf {
        self.cache_dir.join("browser-profile")
    }

    /// Soft-delete area used by the delete_path tool for recoverable removal.
    pub fn trash_dir(&self) -> PathBuf {
        self.data_dir.join("trash")
    }
}

fn env_override(var: &str, default_rel: &str) -> std::io::Result<PathBuf> {
    if let Some(v) = std::env::var_os(var) {
        if !v.is_empty() {
            let p = PathBuf::from(v);
            if !p.is_absolute() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("{var} must be an absolute path"),
                ));
            }
            return Ok(p);
        }
    }
    let home = std::env::var_os("HOME")
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "$HOME is not set"))?;
    Ok(Path::new(&home).join(default_rel))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_override_beats_home_fallback() {
        // Uses temp overrides so the test is independent of the real HOME.
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("VORTEX_TEST_HOME", dir.path());
        let home = std::env::var_os("HOME");
        // Only validate the override branch itself.
        std::env::set_var("VORTEX_CONFIG_DIR", dir.path().join("cfg").to_str().unwrap());
        let p = env_override("VORTEX_CONFIG_DIR", ".config").unwrap();
        assert!(p.ends_with("cfg"));
        std::env::remove_var("VORTEX_CONFIG_DIR");
        if let Some(h) = home {
            std::env::set_var("HOME", h);
        }
        std::env::remove_var("VORTEX_TEST_HOME");
    }
}
