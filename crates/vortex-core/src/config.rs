//! Boot-time configuration (`config.toml` + environment).
//!
//! Split from runtime [`vortex_types::Settings`]: this file controls server
//! binding, credentials, and provider selection — things the UI must not be
//! able to change at runtime. Everything the UI edits lives in SQLite settings.

use crate::error::{CoreError, Result};
use crate::paths::Paths;
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    /// Bind address. Only loopback addresses are permitted unless `lan` is
    /// explicitly enabled (see docs/SECURITY.md).
    pub host: String,
    pub port: u16,
    pub lan: LanConfig,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".into(),
            port: 8417,
            lan: LanConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LanConfig {
    /// Opt-in LAN exposure. When true, a bearer token is required on all
    /// API requests and the server binds on all interfaces.
    pub enabled: bool,
    /// Required bearer token when `enabled` is true. Generated on first boot
    /// with 0600 permissions on the config file.
    pub token: String,
}

impl Default for LanConfig {
    fn default() -> Self {
        Self { enabled: false, token: String::new() }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LlmConfig {
    /// "openrouter" (default) or "mock" (offline testing/demo; no network).
    pub provider: String,
    pub base_url: String,
    /// Kept in config or env only; never served to the browser, never logged.
    pub openrouter_api_key: Option<String>,
    pub api_key_env: String,
    pub mock_script: String,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            provider: "openrouter".into(),
            base_url: "https://openrouter.ai/api/v1".into(),
            openrouter_api_key: None,
            api_key_env: "OPENROUTER_API_KEY".into(),
            mock_script: "echo".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub server: ServerConfig,
    pub llm: LlmConfig,
    pub log_level: String,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            server: ServerConfig::default(),
            llm: LlmConfig::default(),
            log_level: "info".into(),
        }
    }
}

impl AppConfig {
    /// Load `.env` from the working directory (if present), then
    /// `<config dir>/config.toml`, then environment overrides.
    pub fn load(paths: &Paths) -> Result<Self> {
        load_dotenv();

        let file = paths.config_file();
        let mut cfg: AppConfig = if file.exists() {
            let text = std::fs::read_to_string(&file)
                .map_err(|e| CoreError::Config(format!("reading {}: {e}", file.display())))?;
            toml::from_str(&text)
                .map_err(|e| CoreError::Config(format!("parsing {}: {e}", file.display())))?
        } else {
            AppConfig::default()
        };

        if let Ok(v) = std::env::var("VORTEX_HOST") {
            cfg.server.host = v;
        }
        if let Ok(v) = std::env::var("VORTEX_PORT") {
            cfg.server.port =
                v.parse().map_err(|_| CoreError::Config("VORTEX_PORT must be a port number".into()))?;
        }
        if let Ok(v) = std::env::var("VORTEX_OPENROUTER_BASE_URL") {
            if !v.trim().is_empty() {
                cfg.llm.base_url = v.trim_end_matches('/').to_string();
            }
        }
        if let Ok(v) = std::env::var("VORTEX_LLM") {
            if v == "openrouter" || v == "mock" {
                cfg.llm.provider = v;
            }
        }
        if let Ok(v) = std::env::var("VORTEX_MOCK_SCRIPT") {
            if !v.is_empty() {
                cfg.llm.mock_script = v;
            }
        }
        if let Ok(v) = std::env::var("RUST_LOG") {
            if !v.is_empty() {
                cfg.log_level = v;
            }
        }

        if !cfg.is_loopback_host() && !cfg.server.lan.enabled {
            return Err(CoreError::Config(format!(
                "refusing to bind non-loopback host '{}' without an explicit [server.lan] \
                 enabled=true entry in {} (see docs/SECURITY.md)",
                cfg.server.host,
                file.display()
            )));
        }
        Ok(cfg)
    }

    pub fn is_loopback_host(&self) -> bool {
        matches!(self.server.host.as_str(), "127.0.0.1" | "::1" | "localhost")
    }

    /// Effective API key: environment first, then config file.
    pub fn api_key(&self) -> Option<String> {
        if let Ok(v) = std::env::var(&self.llm.api_key_env) {
            if !v.trim().is_empty() {
                return Some(v.trim().to_string());
            }
        }
        self.llm
            .openrouter_api_key
            .as_ref()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }

    /// True when the LLM provider is the built-in offline mock.
    pub fn is_mock_llm(&self) -> bool {
        self.llm.provider == "mock"
    }
}

/// Minimal `.env` loader: KEY=VALUE lines, `#` comments, no interpolation.
/// Never overwrites variables that are already set in the environment.
fn load_dotenv() {
    let Ok(text) = std::fs::read_to_string(".env") else { return };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else { continue };
        let k = k.trim();
        if k.is_empty() || std::env::var_os(k).is_some() {
            continue;
        }
        let mut v = v.trim();
        if (v.starts_with('"') && v.ends_with('"') && v.len() >= 2)
            || (v.starts_with('\'') && v.ends_with('\'') && v.len() >= 2)
        {
            v = &v[1..v.len() - 1];
        }
        std::env::set_var(k, v);
    }
}

/// Restrict config file permissions to the owner (best effort on Linux).
pub fn ensure_private_config_perms(file: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = std::fs::metadata(file) {
        let mode = meta.permissions().mode();
        if mode & 0o077 != 0 {
            let _ = std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o600));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_minimal_toml() {
        let cfg: AppConfig = toml::from_str("").unwrap();
        assert_eq!(cfg.server.host, "127.0.0.1");
        assert!(!cfg.server.lan.enabled);
        assert_eq!(cfg.llm.base_url, "https://openrouter.ai/api/v1");
    }

    #[test]
    fn parse_lan_entry() {
        let cfg: AppConfig = toml::from_str(
            r#"
[server.lan]
enabled = true
token = "abc"
"#,
        )
        .unwrap();
        assert!(cfg.server.lan.enabled);
        assert_eq!(cfg.server.lan.token, "abc");
        assert!(cfg.is_loopback_host());
    }
}
