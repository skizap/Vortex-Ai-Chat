//! Data transfer objects for the REST API and SSE snapshots.

use crate::chat::{AttachmentKind, AttachmentRef, AttachmentStatus, ToolCall, Usage};
use crate::events::{PlanStep, RunStatus};
use crate::Mode;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationSummary {
    pub id: String,
    pub title: String,
    pub mode: Mode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    /// Per-conversation model preference for future sends: `None` inherits
    /// the Settings default, [`crate::VORTEX_AUTO_MODEL`] selects server-side
    /// Auto routing, any other value pins that provider model id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// Effective budget bounds behind the recommended shortlist, echoed by
/// `GET /api/models` so clients can show exactly what was applied. USD per
/// million tokens; a published zero rate is within bounds, an unknown price
/// never is.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RecommendedBounds {
    pub max_input_usd_per_mtok: f64,
    pub max_output_usd_per_mtok: f64,
}

impl Default for RecommendedBounds {
    /// Documented default budget bounds: at most $1 input and $3 output per
    /// million tokens (published zero rates are within bounds).
    fn default() -> Self {
        Self {
            max_input_usd_per_mtok: 1.0,
            max_output_usd_per_mtok: 3.0,
        }
    }
}

impl RecommendedBounds {
    /// Client-editable bounds are clamped, never rejected: NaN/negative or
    /// absurd values fall back to the documented defaults per field.
    pub fn clamped(max_input: f64, max_output: f64) -> Self {
        let d = Self::default();
        let clamp = |v: f64, fallback: f64| {
            if v.is_finite() && v >= 0.0 {
                v.min(1000.0)
            } else {
                fallback
            }
        };
        Self {
            max_input_usd_per_mtok: clamp(max_input, d.max_input_usd_per_mtok),
            max_output_usd_per_mtok: clamp(max_output, d.max_output_usd_per_mtok),
        }
    }
}

/// `GET /api/models` response: the provider catalog plus honest freshness
/// metadata. `stale: true` means the last provider refresh failed and
/// `models`/`fetched_at` describe the last-known-good catalog instead.
/// On cold-start failure the route keeps returning its 502 error shape.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelsResponse {
    pub models: Vec<crate::chat::ModelInfo>,
    /// True when an API key is configured (or the offline mock is active).
    pub configured: bool,
    /// Unix seconds of the last successful provider fetch; `None` if never.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fetched_at: Option<u64>,
    #[serde(default)]
    pub stale: bool,
    /// Presentable refresh error when `stale`; `None` on success.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Verified open-weights shortlist within the effective bounds, ordered by
    /// weekly token-usage rank (unranked entries last, by id). Ids reference
    /// `models`; models outside the shortlist stay visible there.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recommended: Vec<String>,
    /// Bounds actually applied to `recommended` (clamped request or defaults).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recommended_bounds: Option<RecommendedBounds>,
    /// Shortlist size actually applied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recommended_limit: Option<u32>,
    /// Ranking provenance (e.g. `openrouter sort=top-weekly`) when weekly
    /// ranks are present; `None` when ranking is unavailable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ranking_source: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredMessage {
    pub id: i64,
    pub role: crate::chat::Role,
    pub content: String,
    #[serde(default)]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default)]
    pub attachments: Vec<AttachmentRef>,
    pub created_at: String,
    /// Run that produced this message (assistant replies). `None` for user
    /// messages and for rows persisted before provenance tracking existed;
    /// old replies are never inferred from timestamps.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    /// Model id sent to the provider when this message was generated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_model: Option<String>,
    /// Provider-reported model that actually answered. `None` means the
    /// provider never reported one ("serving model not reported").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub serving_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_summary: Option<ReplySummary>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplySummary {
    pub finish: ReplyFinish,
    pub requested_output_tokens: u32,
    pub completion_tokens: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplyFinish {
    Stop,
    Length,
    ContentFilter,
    Error,
    NotReported,
    Other,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatRequestDto {
    pub content: String,
    #[serde(default)]
    pub mode: Option<Mode>,
    /// Attachments (already uploaded, `ready`) to include with this message.
    #[serde(default)]
    pub attachment_ids: Vec<String>,
}

/// Where an attachment came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AttachmentOrigin {
    Upload,
    Generated,
}

impl AttachmentOrigin {
    pub fn as_str(&self) -> &'static str {
        match self {
            AttachmentOrigin::Upload => "upload",
            AttachmentOrigin::Generated => "generated",
        }
    }
}

/// Full attachment metadata (the UI fetches this by id; messages carry only
/// lightweight `AttachmentRef`s).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachmentInfo {
    pub id: String,
    pub conversation_id: String,
    pub kind: AttachmentKind,
    pub origin: AttachmentOrigin,
    pub status: AttachmentStatus,
    pub filename: String,
    pub media_type: String,
    pub size_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub created_at: String,
}

/// Whether a media model generates images or videos.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MediaKind {
    Image,
    Video,
}

impl MediaKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            MediaKind::Image => "image",
            MediaKind::Video => "video",
        }
    }
}

/// A media-generation model from the provider catalog, with the parameters it
/// supports so the UI can constrain pickers honestly.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaModelInfo {
    pub id: String,
    #[serde(default)]
    pub name: String,
    pub kind: MediaKind,
    #[serde(default)]
    pub input_modalities: Vec<String>,
    #[serde(default)]
    pub output_modalities: Vec<String>,
    #[serde(default)]
    pub resolutions: Vec<String>,
    #[serde(default)]
    pub aspect_ratios: Vec<String>,
    #[serde(default)]
    pub durations: Vec<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pricing_hint: Option<String>,
}

/// Lifecycle of an async video-generation job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VideoJobStatus {
    /// Panel flow: created, awaiting explicit human confirmation.
    PendingConfirmation,
    Submitted,
    InProgress,
    Completed,
    Failed,
    Cancelled,
    Expired,
}

impl VideoJobStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            VideoJobStatus::PendingConfirmation => "pending_confirmation",
            VideoJobStatus::Submitted => "submitted",
            VideoJobStatus::InProgress => "in_progress",
            VideoJobStatus::Completed => "completed",
            VideoJobStatus::Failed => "failed",
            VideoJobStatus::Cancelled => "cancelled",
            VideoJobStatus::Expired => "expired",
        }
    }

    /// True while the provider may still be working (poller keeps watching).
    pub fn is_live(&self) -> bool {
        matches!(self, VideoJobStatus::Submitted | VideoJobStatus::InProgress)
    }

    /// True once no further transition is possible.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            VideoJobStatus::Completed
                | VideoJobStatus::Failed
                | VideoJobStatus::Cancelled
                | VideoJobStatus::Expired
        )
    }
}

/// A video job as exposed to the UI (polls this while live).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VideoJobInfo {
    pub id: String,
    pub conversation_id: String,
    pub status: VideoJobStatus,
    pub model: String,
    pub prompt: String,
    pub attachment_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub created_at: String,
}

/// A run (coordinator or sub-agent) as exposed to the UI.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunInfo {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation_id: Option<String>,
    pub agent: String,
    #[serde(default)]
    pub task: String,
    pub model: String,
    pub status: RunStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    #[serde(default)]
    pub plan: Vec<PlanStep>,
    #[serde(default)]
    pub depth: u32,
    pub created_at: String,
    pub updated_at: String,
}

/// Aggregated provider-reported usage for one conversation
/// (`GET /api/conversations/{id}/usage`). Fetched on conversation switch and
/// reconnect, then reconciled against live `UsageUpdate` events by run id.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationUsage {
    pub conversation_id: String,
    /// One entry per distinct run in the conversation's run tree: coordinator
    /// runs, retries, and every delegated sub-agent run.
    #[serde(default)]
    pub runs: Vec<ConversationRunUsage>,
}

/// One run's latest cumulative provider-reported usage. Cumulative SSE events
/// are never summed: a run contributes its most recent total exactly once.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationRunUsage {
    pub run_id: String,
    /// Latest cumulative usage for the run; `None` when the provider never
    /// reported any for it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    /// `true` when this run's totals are permanently partial: a completion
    /// in it never reported usage, or the run is terminal with no reported
    /// usage at all. An active run that simply has not called the provider
    /// yet is *not* incomplete. Totals across runs are then "reported
    /// tokens"; missing history is never zero.
    #[serde(default)]
    pub incomplete: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallRecord {
    pub id: String,
    pub run_id: String,
    pub tool: String,
    pub args: serde_json::Value,
    pub ok: bool,
    pub summary: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalInfo {
    pub id: String,
    pub run_id: String,
    pub agent: String,
    pub title: String,
    pub payload: String,
    pub status: ApprovalStatus,
    pub created_at: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ApprovalStatus {
    Pending,
    Approved,
    Denied,
    Expired,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreviewInfo {
    pub id: String,
    pub run_id: String,
    pub url: String,
    pub root: String,
    pub running: bool,
}

/// File checkpoint metadata for the recovery UI. The snapshot content itself
/// is never shipped in lists (it can be large); restore reads it server-side.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointInfo {
    pub id: i64,
    pub run_id: String,
    /// Absolute path as recorded when the snapshot was taken.
    pub path: String,
    /// `false` means the file did not exist at snapshot time: restoring such
    /// a checkpoint removes (trashes) the file instead of rewriting content.
    pub has_original: bool,
    pub created_at: String,
}

/// Honest, runtime-detected capability report. The UI shows unconfigured
/// capabilities with setup instructions rather than pretending they work.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Capabilities {
    pub version: String,
    /// True when an OpenRouter API key is available server-side.
    pub openrouter: bool,
    /// True when the LLM provider is a local mock (testing/demo mode).
    pub mock_llm: bool,
    /// True when the server runs with session-scoped diagnostics enabled
    /// (env-only `VORTEX_DIAGNOSTICS=1`); the browser reporter activates
    /// only when this is set.
    #[serde(default)]
    pub diagnostics: bool,
    pub search: bool,
    pub search_provider: String,
    pub search_hint: String,
    pub browser: bool,
    pub browser_hint: String,
    pub commands: bool,
    pub previews: bool,
    pub workspace: bool,
    pub subagents: bool,
    pub max_concurrent_subagents: usize,
}

/// What kind of browser-side failure a report describes. Fixed enum: the
/// server rejects anything else, so no free text can ever reach the logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BrowserEventKind {
    /// An uncaught error.
    Error,
    /// An unhandled promise rejection.
    Rejection,
    /// A resource (script/asset) failed to load.
    Resource,
    /// A panic location registered by the WASM panic hook.
    Panic,
}

impl BrowserEventKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            BrowserEventKind::Error => "error",
            BrowserEventKind::Rejection => "rejection",
            BrowserEventKind::Resource => "resource",
            BrowserEventKind::Panic => "panic",
        }
    }
}

/// Which instrumented boundary produced the report. Fixed enum, no payloads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BrowserOperation {
    /// Pre-WASM bootstrap listener (index.html).
    Bootstrap,
    /// `api.rs` transport failure (network error).
    ApiTransport,
    /// `api.rs` unexpected HTTP status.
    ApiStatus,
    /// `api.rs` JSON decode failure.
    ApiDecode,
    /// `sse.rs` connect failure.
    SseConnect,
    /// `sse.rs` frame decode failure.
    SseDecode,
    /// `sse.rs` reconnect scheduling.
    SseReconnect,
    /// `sse.rs` replay-gap/lagged detection.
    SseGap,
    /// `sse.rs` snapshot resync.
    SseResync,
    /// WASM panic hook.
    Panic,
}

impl BrowserOperation {
    pub fn as_str(&self) -> &'static str {
        match self {
            BrowserOperation::Bootstrap => "bootstrap",
            BrowserOperation::ApiTransport => "api_transport",
            BrowserOperation::ApiStatus => "api_status",
            BrowserOperation::ApiDecode => "api_decode",
            BrowserOperation::SseConnect => "sse_connect",
            BrowserOperation::SseDecode => "sse_decode",
            BrowserOperation::SseReconnect => "sse_reconnect",
            BrowserOperation::SseGap => "sse_gap",
            BrowserOperation::SseResync => "sse_resync",
            BrowserOperation::Panic => "panic",
        }
    }
}

/// Bounded, numeric-only source-location metadata. Deliberately carries no
/// file names or text: browsers may attach arbitrary strings there.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrowserSourceLocation {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub column: Option<u32>,
}

/// One sanitized browser diagnostic report (`POST /api/diagnostics`).
///
/// The shape is intentionally tiny: fixed enums, an optional HTTP status,
/// strictly validated ids, and numeric-only location data. No message,
/// stack, or URL fields exist so that neither the browser nor a malicious
/// local client can inject content into server logs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrowserReport {
    pub event: BrowserEventKind,
    pub operation: BrowserOperation,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    /// Server-generated request id echoed back (strict UUID on the server).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<BrowserSourceLocation>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browser_report_roundtrip_with_all_fields() {
        let report = BrowserReport {
            event: BrowserEventKind::Rejection,
            operation: BrowserOperation::SseGap,
            status: Some(503),
            request_id: Some("00000000-0000-4000-8000-000000000001".to_string()),
            run_id: Some("00000000-0000-4000-8000-000000000002".to_string()),
            location: Some(BrowserSourceLocation {
                line: Some(12),
                column: Some(34),
            }),
        };
        let json = serde_json::to_string(&report).unwrap();
        let back: BrowserReport = serde_json::from_str(&json).unwrap();
        assert_eq!(back, report);
        assert_eq!(back.event.as_str(), "rejection");
        assert_eq!(back.operation.as_str(), "sse_gap");
    }

    #[test]
    fn browser_report_rejects_unknown_fields_and_bad_enums() {
        let base = r#"{"event":"error","operation":"panic"}"#;
        assert!(serde_json::from_str::<BrowserReport>(base).is_ok());
        assert!(serde_json::from_str::<BrowserReport>(
            r#"{"event":"error","operation":"panic","message":"x"}"#
        )
        .is_err());
        assert!(serde_json::from_str::<BrowserReport>(
            r#"{"event":"explode","operation":"panic"}"#
        )
        .is_err());
        assert!(serde_json::from_str::<BrowserReport>(
            r#"{"event":"error","operation":"panic","location":{"file":"a.js"}}"#
        )
        .is_err());
        // Empty report and non-object input are rejected.
        assert!(serde_json::from_str::<BrowserReport>("{}").is_err());
        assert!(serde_json::from_str::<BrowserReport>("null").is_err());
    }

    #[test]
    fn capabilities_defaults_diagnostics_to_false_for_old_payloads() {
        let json = r#"{"version":"x","openrouter":true,"mock_llm":false,"search":false,
            "search_provider":"","search_hint":"","browser":false,"browser_hint":"",
            "commands":false,"previews":false,"workspace":false,"subagents":false,
            "max_concurrent_subagents":1}"#;
        let caps: Capabilities = serde_json::from_str(json).unwrap();
        assert!(!caps.diagnostics);
        let caps: Capabilities = serde_json::from_str(&json.replace(
            "\"mock_llm\":false",
            "\"mock_llm\":false,\"diagnostics\":true",
        ))
        .unwrap();
        assert!(caps.diagnostics);
    }

    #[test]
    fn conversation_summary_model_is_optional_and_skipped_when_none() {
        let json = r#"{"id":"c1","title":"t","mode":"chat","created_at":"x","updated_at":"y"}"#;
        let s: ConversationSummary = serde_json::from_str(json).unwrap();
        assert!(s.model.is_none());
        assert!(!serde_json::to_string(&s).unwrap().contains("model"));
        let pinned = ConversationSummary {
            model: Some("anthropic/claude-sonnet-4".into()),
            ..s
        };
        let out = serde_json::to_string(&pinned).unwrap();
        assert!(out.contains(r#""model":"anthropic/claude-sonnet-4""#));
    }

    #[test]
    fn models_response_defaults_and_roundtrip() {
        // Old/compact payloads: stale defaults false, fetched_at/error absent.
        let json = r#"{"models":[],"configured":true}"#;
        let r: ModelsResponse = serde_json::from_str(json).unwrap();
        assert!(!r.stale);
        assert!(r.fetched_at.is_none());
        assert!(r.error.is_none());
        assert!(r.recommended.is_empty());
        assert!(r.recommended_bounds.is_none());
        assert!(r.recommended_limit.is_none());
        assert!(r.ranking_source.is_none());
        let out = serde_json::to_string(&r).unwrap();
        assert!(!out.contains("fetched_at"));
        assert!(!out.contains("error"));
        assert!(!out.contains("recommended"));
        assert!(!out.contains("ranking_source"));
        let stale = ModelsResponse {
            models: Vec::new(),
            configured: true,
            fetched_at: Some(1_750_000_000),
            stale: true,
            error: Some("provider offline".into()),
            recommended: vec!["qwen/qwen3-32b".into()],
            recommended_bounds: Some(RecommendedBounds::default()),
            recommended_limit: Some(5),
            ranking_source: Some("openrouter sort=top-weekly".into()),
        };
        let back: ModelsResponse =
            serde_json::from_str(&serde_json::to_string(&stale).unwrap()).unwrap();
        assert!(back.stale);
        assert_eq!(back.fetched_at, Some(1_750_000_000));
        assert_eq!(back.error.as_deref(), Some("provider offline"));
        assert_eq!(back.recommended, vec!["qwen/qwen3-32b".to_string()]);
        assert_eq!(back.recommended_bounds, Some(RecommendedBounds::default()));
        assert_eq!(back.recommended_limit, Some(5));
        assert_eq!(
            back.ranking_source.as_deref(),
            Some("openrouter sort=top-weekly")
        );
    }

    #[test]
    fn recommended_bounds_defaults_and_clamping() {
        let d = RecommendedBounds::default();
        assert_eq!(d.max_input_usd_per_mtok, 1.0);
        assert_eq!(d.max_output_usd_per_mtok, 3.0);
        // Edited values pass through unchanged.
        assert_eq!(
            RecommendedBounds::clamped(0.5, 10.0),
            RecommendedBounds {
                max_input_usd_per_mtok: 0.5,
                max_output_usd_per_mtok: 10.0,
            }
        );
        // Zero is a legal bound (free-only shortlists).
        assert_eq!(
            RecommendedBounds::clamped(0.0, 0.0).max_input_usd_per_mtok,
            0.0
        );
        // NaN, negative or non-finite per-field values fall back to defaults;
        // absurd values clamp to the ceiling.
        assert_eq!(
            RecommendedBounds::clamped(f64::NAN, -2.0),
            RecommendedBounds::default()
        );
        assert_eq!(
            RecommendedBounds::clamped(f64::INFINITY, 5000.0).max_output_usd_per_mtok,
            1000.0
        );
    }

    #[test]
    fn stored_message_old_payload_defaults_attachments() {
        let json = r#"{"id":1,"role":"user","content":"hi","tool_calls":[],"created_at":"t"}"#;
        let m: StoredMessage = serde_json::from_str(json).unwrap();
        assert!(m.attachments.is_empty());
    }

    #[test]
    fn stored_message_legacy_rows_keep_null_provenance() {
        // Rows persisted before provenance tracking decode as null — old
        // replies are never inferred — and null keys are omitted on the wire.
        let json = r#"{"id":1,"role":"assistant","content":"hi","tool_calls":[],"created_at":"t"}"#;
        let m: StoredMessage = serde_json::from_str(json).unwrap();
        assert!(m.run_id.is_none());
        assert!(m.requested_model.is_none());
        assert!(m.serving_model.is_none());
        assert!(m.reply_summary.is_none());
        let out = serde_json::to_string(&m).unwrap();
        assert!(!out.contains("run_id"));
        assert!(!out.contains("serving_model"));

        let full = StoredMessage {
            run_id: Some("00000000-0000-4000-8000-0000000000aa".into()),
            requested_model: Some("requested/model".into()),
            serving_model: Some("provider/served".into()),
            ..m
        };
        let json = serde_json::to_string(&full).unwrap();
        assert!(json.contains("\"serving_model\":\"provider/served\""));
        let back: StoredMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(back.run_id, full.run_id);
        assert_eq!(back.requested_model, full.requested_model);
        assert_eq!(back.serving_model, full.serving_model);
    }

    #[test]
    fn conversation_usage_roundtrip_and_legacy_defaults() {
        let usage = ConversationUsage {
            conversation_id: "c1".into(),
            runs: vec![
                ConversationRunUsage {
                    run_id: "r1".into(),
                    usage: Some(Usage {
                        prompt_tokens: 10,
                        completion_tokens: 5,
                        total_tokens: 15,
                    }),
                    incomplete: false,
                },
                ConversationRunUsage {
                    run_id: "r2".into(),
                    usage: None,
                    incomplete: true,
                },
            ],
        };
        let json = serde_json::to_string(&usage).unwrap();
        // A run without reported usage serializes null usage + incomplete.
        assert!(json.contains("\"incomplete\":true"));
        assert!(
            !json.contains("\"usage\":null"),
            "null usage is omitted: {json}"
        );
        let back: ConversationUsage = serde_json::from_str(&json).unwrap();
        assert_eq!(back.runs.len(), 2);
        assert_eq!(back.runs[0].usage.unwrap().total_tokens, 15);
        assert!(back.runs[1].incomplete);
        // Payloads missing the new keys entirely still decode.
        let minimal = r#"{"run_id":"r3"}"#;
        let run: ConversationRunUsage = serde_json::from_str(minimal).unwrap();
        assert!(run.usage.is_none());
        assert!(!run.incomplete);
    }

    #[test]
    fn chat_request_old_payload_defaults_attachment_ids() {
        let json = r#"{"content":"hi"}"#;
        let r: ChatRequestDto = serde_json::from_str(json).unwrap();
        assert!(r.attachment_ids.is_empty());
        assert!(r.mode.is_none());
        let json = r#"{"content":"hi","attachment_ids":["a","b"],"mode":"chat"}"#;
        let r: ChatRequestDto = serde_json::from_str(json).unwrap();
        assert_eq!(r.attachment_ids, vec!["a", "b"]);
    }

    #[test]
    fn attachment_info_roundtrip() {
        let info = AttachmentInfo {
            id: "00000000-0000-4000-8000-000000000001".into(),
            conversation_id: "00000000-0000-4000-8000-000000000002".into(),
            kind: AttachmentKind::Video,
            origin: AttachmentOrigin::Generated,
            status: AttachmentStatus::Pending,
            filename: "clip.mp4".into(),
            media_type: "video/mp4".into(),
            size_bytes: 0,
            job_id: Some("00000000-0000-4000-8000-000000000003".into()),
            error: None,
            created_at: "t".into(),
        };
        let json = serde_json::to_string(&info).unwrap();
        assert!(json.contains("\"origin\":\"generated\""));
        assert!(json.contains("\"status\":\"pending\""));
        assert!(!json.contains("error"));
        let back: AttachmentInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, info.id);
        assert_eq!(back.kind, AttachmentKind::Video);
        assert_eq!(back.job_id, info.job_id);
    }

    #[test]
    fn media_model_info_roundtrip_and_defaults() {
        let json = r#"{"id":"google/veo-3.1-lite","kind":"video"}"#;
        let m: MediaModelInfo = serde_json::from_str(json).unwrap();
        assert!(m.resolutions.is_empty());
        assert!(m.durations.is_empty());
        assert!(m.pricing_hint.is_none());
        let m = MediaModelInfo {
            id: "google/gemini-2.5-flash-image".into(),
            name: "Gemini Image".into(),
            kind: MediaKind::Image,
            input_modalities: vec!["text".into(), "image".into()],
            output_modalities: vec!["image".into()],
            resolutions: vec!["1024x1024".into()],
            aspect_ratios: vec!["1:1".into(), "16:9".into()],
            durations: vec![],
            pricing_hint: Some("~$0.03/image".into()),
        };
        let back: MediaModelInfo =
            serde_json::from_str(&serde_json::to_string(&m).unwrap()).unwrap();
        assert_eq!(back.aspect_ratios, vec!["1:1", "16:9"]);
        assert_eq!(back.kind, MediaKind::Image);
    }

    #[test]
    fn video_job_status_wire_names_and_lifecycle() {
        assert_eq!(
            serde_json::to_string(&VideoJobStatus::PendingConfirmation).unwrap(),
            "\"pending_confirmation\""
        );
        assert_eq!(
            serde_json::to_string(&VideoJobStatus::InProgress).unwrap(),
            "\"in_progress\""
        );
        assert!(VideoJobStatus::Submitted.is_live());
        assert!(VideoJobStatus::InProgress.is_live());
        assert!(!VideoJobStatus::PendingConfirmation.is_live());
        assert!(VideoJobStatus::Completed.is_terminal());
        assert!(VideoJobStatus::Expired.is_terminal());
        assert!(!VideoJobStatus::InProgress.is_terminal());
        let info = VideoJobInfo {
            id: "j".into(),
            conversation_id: "c".into(),
            status: VideoJobStatus::Submitted,
            model: "google/veo-3.1-lite".into(),
            prompt: "a cat".into(),
            attachment_id: "a".into(),
            error: None,
            created_at: "t".into(),
        };
        let back: VideoJobInfo =
            serde_json::from_str(&serde_json::to_string(&info).unwrap()).unwrap();
        assert_eq!(back.status, VideoJobStatus::Submitted);
    }
}
