//! Runtime-editable settings (persisted in SQLite, editable from the UI).

use crate::Mode;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[derive(Default)]
pub enum Theme {
    #[default]
    Dark,
    Light,
}

/// Coarse permission profile. Regardless of profile, backend-enforced
/// "consequential" actions (arbitrary commands, deletions, anything the model
/// explicitly flags) always require explicit human approval.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[derive(Default)]
pub enum PermissionProfile {
    /// Read-only tools; no writes, commands, or previews.
    Restricted,
    /// Workspace reads/writes and safe operations allowed; consequential actions gated.
    #[default]
    Standard,
    /// Like Standard, but safe allowlisted commands run without per-run approval.
    /// Consequential actions are still always gated.
    Trusted,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SearchSettings {
    /// Provider id; only "searxng" is currently implemented.
    pub provider: String,
    /// Base URL of the configured search service, e.g. a local SearXNG instance.
    pub base_url: String,
    pub max_results: u8,
}

impl Default for SearchSettings {
    fn default() -> Self {
        Self {
            provider: "searxng".into(),
            base_url: String::new(),
            max_results: 6,
        }
    }
}

impl SearchSettings {
    pub fn is_configured(&self) -> bool {
        !self.base_url.trim().is_empty()
    }
}

/// Outcome of the Settings "Test search" probe
/// (`POST /api/settings/search/test`). The probe is server-side only: a
/// fixed benign query through the same provider and JSON path `web_search`
/// uses, never a caller-provided URL or query.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchTestStatus {
    /// No search provider is configured (saved URL is empty, non-mock mode).
    NotConfigured,
    /// Web tools are turned off in Settings (Tools).
    Disabled,
    /// Probe connected and returned at least one result.
    Ok,
    /// Probe connected but the provider returned zero results.
    Empty,
    /// Probe failed; see `error_kind` and `detail`.
    Error,
}

/// Sanitized failure category for a search probe. Fixed enum: the server
/// never passes raw transport/provider error text through.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchTestErrorKind {
    /// Unreachable, DNS failure, or timeout.
    Connection,
    /// Provider returned a non-success HTTP status (see `http_status`).
    HttpStatus,
    /// Body was not parseable SearXNG JSON (e.g. JSON output disabled).
    InvalidResponse,
    /// Any other provider-level failure.
    Provider,
}

/// Structured result of `POST /api/settings/search/test`. Always returned
/// with HTTP 200; `status` carries the outcome. The server keeps no probe
/// state; clients discard stale in-flight results against their own saved
/// configuration revision (the URL itself is never echoed back — it could
/// carry credentials).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchTestResult {
    pub status: SearchTestStatus,
    /// Provider that was (or would be) probed: "searxng", "mock", or "".
    pub provider: String,
    /// True when the probe used the offline mock provider; UIs must label
    /// mock results as mock.
    pub mock: bool,
    /// Number of results the probe returned.
    pub result_count: usize,
    /// HTTP status reported by the provider, when `error_kind` is
    /// `http_status`.
    pub http_status: Option<u16>,
    pub error_kind: Option<SearchTestErrorKind>,
    /// Actionable, sanitized human-readable detail. Never contains secrets,
    /// credentials, or raw provider error dumps.
    pub detail: Option<String>,
    /// Unix seconds when the probe completed (server clock).
    pub tested_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ToolSettings {
    pub files: bool,
    pub search: bool,
    pub browser: bool,
    /// Arbitrary command execution. Disabled until the user opts in.
    pub commands: bool,
    pub previews: bool,
    /// Allow the coordinator to spawn sub-agents.
    pub delegation: bool,
    /// Allow media-generation tools (`generate_image`, `generate_video`).
    /// Video stays behind the human approval gate regardless of this toggle.
    pub media: bool,
}

impl Default for ToolSettings {
    fn default() -> Self {
        Self {
            files: true,
            search: true,
            browser: true,
            commands: false,
            previews: true,
            delegation: true,
            media: true,
        }
    }
}

/// Media-generation defaults. Model ids are Settings-overridable; the server
/// fails honestly when a configured model is absent from the catalog.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MediaSettings {
    pub image_model: String,
    pub video_model: String,
    /// OpenRouter file-parser engine for PDF input:
    /// `cloudflare-ai` (free), `mistral-ocr` (paid), or `native`.
    pub pdf_engine: String,
}

impl Default for MediaSettings {
    fn default() -> Self {
        Self {
            image_model: "google/gemini-2.5-flash-image".into(),
            video_model: "google/veo-3.1-lite".into(),
            pdf_engine: "cloudflare-ai".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentSettings {
    /// Concurrent sub-agent limit. Enforced server-side by a semaphore;
    /// must never be hard-coded below 6 (product requirement).
    pub max_concurrent: usize,
    /// Allow sub-agents to spawn further sub-agents (counted in the same limit).
    pub recursive: bool,
    /// Max model round-trips per run.
    pub max_iterations: u32,
    /// Wall-clock budget per run.
    pub run_timeout_secs: u64,
    /// Token budget per run.
    pub max_total_tokens: u64,
    /// How long an approval may sit unanswered before it is auto-denied.
    pub approval_timeout_secs: u64,
}

impl Default for AgentSettings {
    fn default() -> Self {
        Self {
            max_concurrent: 6,
            recursive: false,
            max_iterations: 16,
            run_timeout_secs: 600,
            max_total_tokens: 200_000,
            approval_timeout_secs: 300,
        }
    }
}

impl AgentSettings {
    /// Hard ceiling for the user-configurable concurrency limit.
    pub const MAX_LIMIT_CEILING: usize = 16;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub model: String,
    pub temperature: f32,
    pub max_tokens: u32,
    pub theme: Theme,
    pub default_mode: Mode,
    /// Approved workspace root (absolute path). File tools are refused without one.
    pub workspace: Option<String>,
    /// Command argv prefixes that may run without approval when commands are enabled.
    pub command_allowlist: Vec<Vec<String>>,
    pub search: SearchSettings,
    pub tools: ToolSettings,
    pub profile: PermissionProfile,
    pub agents: AgentSettings,
    pub media: MediaSettings,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            // Fresh installs default to Vortex Auto (server-side routing).
            // Upgraded installs keep their saved blob verbatim, so an explicit
            // past choice — including the distinct `openrouter/auto` provider
            // alias — is preserved.
            model: crate::chat::VORTEX_AUTO_MODEL.into(),
            temperature: 0.7,
            max_tokens: 16_384,
            theme: Theme::Dark,
            default_mode: Mode::Chat,
            workspace: None,
            command_allowlist: Vec::new(),
            search: SearchSettings::default(),
            tools: ToolSettings::default(),
            profile: PermissionProfile::Standard,
            agents: AgentSettings::default(),
            media: MediaSettings::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_model_is_vortex_auto_and_explicit_choices_survive() {
        assert_eq!(Settings::default().model, crate::chat::VORTEX_AUTO_MODEL);
        // Upgrade path: a stored blob with an explicit model keeps it verbatim;
        // the provider alias is never rewritten to Vortex Auto.
        let s: Settings = serde_json::from_str(r#"{"model":"openrouter/auto"}"#).unwrap();
        assert_eq!(s.model, "openrouter/auto");
        let s: Settings = serde_json::from_str(r#"{"model":"anthropic/claude-sonnet-4"}"#).unwrap();
        assert_eq!(s.model, "anthropic/claude-sonnet-4");
    }

    #[test]
    fn old_settings_payload_defaults_media() {
        // A pre-media settings blob must decode with media defaults.
        let json = r#"{"model":"openrouter/auto","temperature":0.7,"max_tokens":4096,
            "theme":"dark","default_mode":"chat","workspace":null,"command_allowlist":[],
            "search":{"provider":"searxng","base_url":"","max_results":6},
            "tools":{"files":true,"search":true,"browser":true,"commands":false,
                     "previews":true,"delegation":true},
            "profile":"standard",
            "agents":{"max_concurrent":6,"recursive":false,"max_iterations":16,
                      "run_timeout_secs":600,"max_total_tokens":200000,
                      "approval_timeout_secs":300}}"#;
        let s: Settings = serde_json::from_str(json).unwrap();
        assert!(s.tools.media, "media toggle defaults on");
        assert_eq!(s.media.image_model, "google/gemini-2.5-flash-image");
        assert_eq!(s.media.video_model, "google/veo-3.1-lite");
        assert_eq!(s.media.pdf_engine, "cloudflare-ai");
    }

    #[test]
    fn search_test_result_wire_shape() {
        // The UI parses snake_case status/kind labels; pin the wire shape.
        let r = SearchTestResult {
            status: SearchTestStatus::Error,
            provider: "searxng".into(),
            mock: false,
            result_count: 0,
            http_status: Some(403),
            error_kind: Some(SearchTestErrorKind::HttpStatus),
            detail: Some("provider said no".into()),
            tested_at: 1_795_000_000,
        };
        let json = serde_json::to_value(&r).unwrap();
        assert_eq!(json["status"], "error");
        assert_eq!(json["error_kind"], "http_status");
        assert_eq!(json["http_status"], 403);
        // The configured URL is never echoed (it could carry credentials).
        assert!(json.get("base_url").is_none());
        let back: SearchTestResult = serde_json::from_value(json).unwrap();
        assert_eq!(back, r);
        for (status, label) in [
            (SearchTestStatus::NotConfigured, "not_configured"),
            (SearchTestStatus::Disabled, "disabled"),
            (SearchTestStatus::Ok, "ok"),
            (SearchTestStatus::Empty, "empty"),
        ] {
            let json = serde_json::to_value(status).unwrap();
            assert_eq!(json, label);
        }
        for (kind, label) in [
            (SearchTestErrorKind::Connection, "connection"),
            (SearchTestErrorKind::InvalidResponse, "invalid_response"),
            (SearchTestErrorKind::Provider, "provider"),
        ] {
            let json = serde_json::to_value(kind).unwrap();
            assert_eq!(json, label);
        }
    }

    #[test]
    fn media_settings_roundtrip() {
        let mut s = Settings::default();
        s.tools.media = false;
        s.media.image_model = "openai/gpt-image-1".into();
        s.media.pdf_engine = "mistral-ocr".into();
        let back: Settings = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert!(!back.tools.media);
        assert_eq!(back.media.image_model, "openai/gpt-image-1");
        assert_eq!(back.media.pdf_engine, "mistral-ocr");
        assert_eq!(back.media.video_model, "google/veo-3.1-lite");
    }
}
