//! XDG base-directory resolution with explicit overrides.
//!
//! Precedence for each location:
//! 1. `VORTEX_CONFIG_DIR` / `VORTEX_DATA_DIR` / `VORTEX_CACHE_DIR`
//!    (must be absolute when set),
//! 2. `XDG_CONFIG_HOME` / `XDG_DATA_HOME` / `XDG_CACHE_HOME`
//!    (used only when absolute, per the XDG spec; relative values are
//!    ignored),
//! 3. `$HOME/.config`, `$HOME/.local/share`, `$HOME/.cache`.
//!
//! Each resolved location gets a `vortex` subdirectory.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Paths {
    pub config_dir: PathBuf,
    pub data_dir: PathBuf,
    pub cache_dir: PathBuf,
}

impl Paths {
    pub fn resolve() -> std::io::Result<Self> {
        let config_dir =
            base_dir("VORTEX_CONFIG_DIR", "XDG_CONFIG_HOME", ".config")?.join("vortex");
        let data_dir = base_dir("VORTEX_DATA_DIR", "XDG_DATA_HOME", ".local/share")?.join("vortex");
        let cache_dir = base_dir("VORTEX_CACHE_DIR", "XDG_CACHE_HOME", ".cache")?.join("vortex");
        std::fs::create_dir_all(&config_dir)?;
        std::fs::create_dir_all(&data_dir)?;
        std::fs::create_dir_all(&cache_dir)?;
        Ok(Self {
            config_dir,
            data_dir,
            cache_dir,
        })
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

    /// Server-side attachment storage (uploads + generated media). Files live
    /// at `attachments/{conversation_id}/{attachment_id}.{ext}` with both path
    /// segments server-generated UUIDs.
    pub fn attachments_dir(&self) -> PathBuf {
        self.data_dir.join("attachments")
    }
}

fn base_dir(vortex_var: &str, xdg_var: &str, default_rel: &str) -> std::io::Result<PathBuf> {
    resolve_base(
        vortex_var,
        std::env::var_os(vortex_var).as_deref(),
        std::env::var_os(xdg_var).as_deref(),
        std::env::var_os("HOME").as_deref(),
        default_rel,
    )
}

/// Pure resolution logic (kept env-free so tests never race on the process
/// environment).
fn resolve_base(
    vortex_var: &str,
    vortex_value: Option<&OsStr>,
    xdg_value: Option<&OsStr>,
    home: Option<&OsStr>,
    default_rel: &str,
) -> std::io::Result<PathBuf> {
    if let Some(v) = vortex_value {
        if !v.is_empty() {
            let p = PathBuf::from(v);
            if !p.is_absolute() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("{vortex_var} must be an absolute path"),
                ));
            }
            return Ok(p);
        }
    }
    // XDG spec: a relative XDG_*_HOME is ignored (falls back to the default).
    if let Some(v) = xdg_value {
        if !v.is_empty() {
            let p = PathBuf::from(v);
            if p.is_absolute() {
                return Ok(p);
            }
        }
    }
    let home =
        home.ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "$HOME is not set"))?;
    Ok(Path::new(home).join(default_rel))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    fn os(s: &str) -> OsString {
        OsString::from(s)
    }

    #[test]
    fn vortex_override_wins_and_must_be_absolute() {
        let got = resolve_base(
            "VORTEX_CONFIG_DIR",
            Some(&os("/opt/vortex-cfg")),
            Some(&os("/xdg/cfg")),
            Some(&os("/home/u")),
            ".config",
        )
        .unwrap();
        assert_eq!(got, PathBuf::from("/opt/vortex-cfg"));

        let err = resolve_base(
            "VORTEX_CONFIG_DIR",
            Some(&os("relative/path")),
            None,
            Some(&os("/home/u")),
            ".config",
        )
        .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[test]
    fn xdg_home_is_honored_when_absolute() {
        let got = resolve_base(
            "VORTEX_CONFIG_DIR",
            None,
            Some(&os("/xdg/cfg")),
            Some(&os("/home/u")),
            ".config",
        )
        .unwrap();
        assert_eq!(got, PathBuf::from("/xdg/cfg"));
    }

    #[test]
    fn relative_or_empty_xdg_home_falls_back_to_home_default() {
        // Relative XDG values are ignored per the XDG spec.
        let got = resolve_base(
            "VORTEX_DATA_DIR",
            None,
            Some(&os("relative/xdg")),
            Some(&os("/home/u")),
            ".local/share",
        )
        .unwrap();
        assert_eq!(got, PathBuf::from("/home/u/.local/share"));

        // Empty values fall through too.
        let got = resolve_base(
            "VORTEX_DATA_DIR",
            Some(&os("")),
            Some(&os("")),
            Some(&os("/home/u")),
            ".local/share",
        )
        .unwrap();
        assert_eq!(got, PathBuf::from("/home/u/.local/share"));
    }

    #[test]
    fn missing_home_is_an_error() {
        let err = resolve_base("VORTEX_CACHE_DIR", None, None, None, ".cache").unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    }
}
