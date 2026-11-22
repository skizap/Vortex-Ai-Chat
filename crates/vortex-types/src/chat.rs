//! Chat-level message types (OpenAI/OpenRouter compatible shapes).

use serde::{Deserialize, Serialize};

/// Maximum size of a single chat upload. Some attachment kinds have lower caps.
pub const MAX_ATTACHMENT_UPLOAD_BYTES: usize = 20 * 1024 * 1024;

/// Chat roles used in the conversation transcript.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

impl Role {
    pub fn as_str(&self) -> &'static str {
        match self {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        }
    }
}

/// A function/tool call requested by a model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// Raw JSON arguments as produced by the provider.
    pub arguments: String,
}

/// What kind of file an attachment is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum AttachmentKind {
    Image,
    Pdf,
    #[default]
    Text,
    Video,
}

impl AttachmentKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            AttachmentKind::Image => "image",
            AttachmentKind::Pdf => "pdf",
            AttachmentKind::Text => "text",
            AttachmentKind::Video => "video",
        }
    }
}

/// Lifecycle of an attachment. Uploads are `Ready` immediately; generated
/// videos start `Pending` and become `Ready` (or `Failed`) when the job ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum AttachmentStatus {
    Pending,
    #[default]
    Ready,
    Failed,
}

impl AttachmentStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            AttachmentStatus::Pending => "pending",
            AttachmentStatus::Ready => "ready",
            AttachmentStatus::Failed => "failed",
        }
    }
}

/// Lightweight attachment reference carried on messages (persisted + replayed).
/// Full metadata lives in `AttachmentInfo` and is fetched by id.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AttachmentRef {
    pub id: String,
    pub kind: AttachmentKind,
    pub filename: String,
    pub media_type: String,
    #[serde(default)]
    pub status: AttachmentStatus,
}

/// `image_url` payload of a multimodal content part.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImageUrlContent {
    pub url: String,
}

/// `video_url` payload of an OpenRouter chat completions content part.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VideoUrlContent {
    pub url: String,
}

/// `file` payload of a multimodal content part (OpenRouter PDF input).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileContent {
    pub filename: String,
    pub file_data: String,
}

/// One part of a multimodal message content array (OpenAI/OpenRouter shape).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    Text { text: String },
    ImageUrl { image_url: ImageUrlContent },
    VideoUrl { video_url: VideoUrlContent },
    File { file: FileContent },
}

/// A single message in a conversation (tool results use `tool_call_id`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: Role,
    pub content: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    /// Attachment references shown in the UI and hydrated for the model.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<AttachmentRef>,
    /// Multimodal content parts. When present, the provider serializer sends
    /// these instead of the plain `content` string.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parts: Option<Vec<ContentPart>>,
}

impl ChatMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            tool_name: None,
            attachments: Vec::new(),
            parts: None,
        }
    }
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            tool_name: None,
            attachments: Vec::new(),
            parts: None,
        }
    }
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            tool_name: None,
            attachments: Vec::new(),
            parts: None,
        }
    }
    /// A tool result message returned to the model.
    pub fn tool_result(
        tool_call_id: impl Into<String>,
        name: impl Into<String>,
        content: impl Into<String>,
    ) -> Self {
        Self {
            role: Role::Tool,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: Some(tool_call_id.into()),
            tool_name: Some(name.into()),
            attachments: Vec::new(),
            parts: None,
        }
    }
}

/// Token usage reported by a provider (best effort; may be absent).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub prompt_tokens: u64,
    #[serde(default)]
    pub completion_tokens: u64,
    #[serde(default)]
    pub total_tokens: u64,
}

impl std::ops::Add for Usage {
    type Output = Usage;
    fn add(self, rhs: Usage) -> Usage {
        Usage {
            prompt_tokens: self.prompt_tokens + rhs.prompt_tokens,
            completion_tokens: self.completion_tokens + rhs.completion_tokens,
            total_tokens: self.total_tokens + rhs.total_tokens,
        }
    }
}

/// Preference constant for server-side Auto routing. Not a provider id: it is
/// resolved to a concrete catalog model once per turn, before a run starts.
/// Distinct from the provider's own `openrouter/auto` router alias, which is
/// passed through unchanged like any pinned model.
pub const VORTEX_AUTO_MODEL: &str = "vortex/auto";

/// A model from the provider catalog. Display data only — never instructions.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub context_length: Option<u64>,
    /// `Some(false)` means the catalog says the model does not support tool calling.
    #[serde(default)]
    pub supports_tools: Option<bool>,
    /// USD per token (provider string form; unknown when None, never zero).
    #[serde(default)]
    pub pricing_prompt: Option<String>,
    #[serde(default)]
    pub pricing_completion: Option<String>,
    /// Provider creation date (Unix seconds) when the catalog reports one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created: Option<i64>,
    /// USD per request, when the provider charges a known request fee.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pricing_request: Option<String>,
    /// USD per image input, when the provider reports a per-image fee.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pricing_image: Option<String>,
    /// Provider-declared input modalities (e.g. `text`, `image`, `video`).
    /// Empty when unknown; a nonempty list names the supported modalities.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub input_modalities: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub output_modalities: Vec<String>,
    /// Advertised maximum completion tokens (`top_provider.max_completion_tokens`).
    /// Provider-specific and not a guaranteed limit; `None` when unreported
    /// (routers such as `openrouter/auto` have no known response limit).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_completion_tokens: Option<u64>,
    /// 1-based position in the provider's weekly token-usage ranking
    /// (`sort=top-weekly`). Usage popularity, never quality. `None` when the
    /// ranking is unavailable — clients must then omit popularity claims.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weekly_rank: Option<u32>,
    /// `Some(true)` only from the reviewed exact-ID open-weights mapping.
    /// A Hugging Face id, brand name or free price alone never sets this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open_weights: Option<bool>,
    /// Publisher of the weights, from the reviewed mapping only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publisher: Option<String>,
    /// License name from the reviewed mapping (e.g. `Apache-2.0`, `MIT`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
    /// Link to the license text or model card carrying it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license_url: Option<String>,
    /// Link to the publisher's weights page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weights_url: Option<String>,
    /// Custom license terms beyond the standard license text, when flagged by
    /// the reviewed mapping. Such models stay out of the recommended shortlist.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restriction: Option<String>,
    /// Provider-reported date (YYYY-MM-DD) after which the model may be
    /// removed; `None` when no expiration is reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_on: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_message_old_payload_decodes_unchanged() {
        let json = r#"{"role":"user","content":"hello"}"#;
        let m: ChatMessage = serde_json::from_str(json).unwrap();
        assert_eq!(m.content, "hello");
        assert!(m.attachments.is_empty());
        assert!(m.parts.is_none());
        // Old payloads round-trip without the new keys.
        let out = serde_json::to_string(&m).unwrap();
        assert!(!out.contains("attachments"));
        assert!(!out.contains("parts"));
    }

    #[test]
    fn chat_message_with_attachments_and_parts_roundtrip() {
        let m = ChatMessage {
            attachments: vec![AttachmentRef {
                id: "00000000-0000-4000-8000-000000000001".into(),
                kind: AttachmentKind::Image,
                filename: "cat.png".into(),
                media_type: "image/png".into(),
                status: AttachmentStatus::Ready,
            }],
            parts: Some(vec![
                ContentPart::Text {
                    text: "what is this?".into(),
                },
                ContentPart::ImageUrl {
                    image_url: ImageUrlContent {
                        url: "data:image/png;base64,AAAA".into(),
                    },
                },
                ContentPart::File {
                    file: FileContent {
                        filename: "doc.pdf".into(),
                        file_data: "data:application/pdf;base64,JVBER".into(),
                    },
                },
            ]),
            ..ChatMessage::user("what is this?")
        };
        let json = serde_json::to_string(&m).unwrap();
        let back: ChatMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(back.attachments, m.attachments);
        assert_eq!(back.parts, m.parts);
        // Wire shapes match the OpenRouter multimodal contract.
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        let parts = v["parts"].as_array().unwrap();
        assert_eq!(parts[0]["type"], "text");
        assert_eq!(parts[1]["type"], "image_url");
        assert_eq!(parts[1]["image_url"]["url"], "data:image/png;base64,AAAA");
        assert_eq!(parts[2]["type"], "file");
        assert_eq!(parts[2]["file"]["filename"], "doc.pdf");
    }

    #[test]
    fn attachment_ref_defaults_status_to_ready() {
        let json = r#"{"id":"x","kind":"pdf","filename":"a.pdf","media_type":"application/pdf"}"#;
        let r: AttachmentRef = serde_json::from_str(json).unwrap();
        assert_eq!(r.status, AttachmentStatus::Ready);
        assert_eq!(r.kind, AttachmentKind::Pdf);
    }

    #[test]
    fn attachment_enums_serialize_lowercase() {
        assert_eq!(
            serde_json::to_string(&AttachmentKind::Image).unwrap(),
            "\"image\""
        );
        assert_eq!(
            serde_json::to_string(&AttachmentStatus::Pending).unwrap(),
            "\"pending\""
        );
        assert_eq!(
            serde_json::to_string(&AttachmentKind::Video).unwrap(),
            "\"video\""
        );
    }

    #[test]
    fn model_info_old_payload_decodes_without_modalities() {
        let json = r#"{"id":"openai/gpt-4o-mini","name":"GPT-4o mini"}"#;
        let m: ModelInfo = serde_json::from_str(json).unwrap();
        assert!(m.input_modalities.is_empty());
        assert!(m.output_modalities.is_empty());
        assert!(m.created.is_none());
        assert!(m.pricing_request.is_none());
        assert!(m.pricing_image.is_none());
        assert!(m.max_completion_tokens.is_none());
        assert!(m.weekly_rank.is_none());
        assert!(m.open_weights.is_none());
        assert!(m.license.is_none());
        assert!(m.restriction.is_none());
        assert!(m.expires_on.is_none());
        let out = serde_json::to_string(&m).unwrap();
        assert!(!out.contains("input_modalities"));
        assert!(!out.contains("created"));
        assert!(!out.contains("pricing_request"));
        assert!(!out.contains("max_completion_tokens"));
        assert!(!out.contains("weekly_rank"));
        assert!(!out.contains("open_weights"));
        assert!(!out.contains("license"));
        assert!(!out.contains("restriction"));
        assert!(!out.contains("expires_on"));
    }

    #[test]
    fn model_info_modalities_roundtrip() {
        let m = ModelInfo {
            id: "x".into(),
            input_modalities: vec!["text".into(), "image".into()],
            output_modalities: vec!["text".into()],
            ..Default::default()
        };
        let back: ModelInfo = serde_json::from_str(&serde_json::to_string(&m).unwrap()).unwrap();
        assert_eq!(back.input_modalities, vec!["text", "image"]);
        assert_eq!(back.output_modalities, vec!["text"]);
    }

    #[test]
    fn model_info_created_and_fee_details_roundtrip() {
        let m = ModelInfo {
            id: "google/gemini-x".into(),
            name: "Gemini X".into(),
            context_length: Some(1_000_000),
            supports_tools: Some(true),
            pricing_prompt: Some("0.0000004".into()),
            pricing_completion: Some("0.0000012".into()),
            created: Some(1_750_000_000),
            pricing_request: Some("0.004".into()),
            pricing_image: Some("0.0002".into()),
            input_modalities: vec!["text".into(), "image".into(), "video".into()],
            output_modalities: vec!["text".into()],
            ..Default::default()
        };
        let back: ModelInfo = serde_json::from_str(&serde_json::to_string(&m).unwrap()).unwrap();
        assert_eq!(back.created, Some(1_750_000_000));
        assert_eq!(back.pricing_request.as_deref(), Some("0.004"));
        assert_eq!(back.pricing_image.as_deref(), Some("0.0002"));
    }

    /// Catalog provenance fields (rank, output limit, verified license data,
    /// retirement date) survive the wire and stay absent when unknown.
    #[test]
    fn model_info_catalog_provenance_roundtrip() {
        let m = ModelInfo {
            id: "qwen/qwen3-32b".into(),
            name: "Qwen3 32B".into(),
            max_completion_tokens: Some(16_384),
            weekly_rank: Some(7),
            open_weights: Some(true),
            publisher: Some("Alibaba Cloud Qwen Team".into()),
            license: Some("Apache-2.0".into()),
            license_url: Some("https://www.apache.org/licenses/LICENSE-2.0".into()),
            weights_url: Some("https://huggingface.co/Qwen/Qwen3-32B".into()),
            restriction: None,
            expires_on: Some("2027-01-31".into()),
            ..Default::default()
        };
        let back: ModelInfo = serde_json::from_str(&serde_json::to_string(&m).unwrap()).unwrap();
        assert_eq!(back.max_completion_tokens, Some(16_384));
        assert_eq!(back.weekly_rank, Some(7));
        assert_eq!(back.open_weights, Some(true));
        assert_eq!(back.license.as_deref(), Some("Apache-2.0"));
        assert!(back.restriction.is_none());
        assert_eq!(back.expires_on.as_deref(), Some("2027-01-31"));

        // Unmapped model: license provenance keys are omitted entirely, so a
        // Hugging Face id or a free price can never masquerade as verified.
        let plain = ModelInfo {
            id: "vendor/unknown-lineage".into(),
            weekly_rank: Some(3),
            ..Default::default()
        };
        let out = serde_json::to_string(&plain).unwrap();
        assert!(!out.contains("open_weights"));
        assert!(!out.contains("license"));
        assert!(out.contains("weekly_rank"));
    }

    #[test]
    fn vortex_auto_constant_is_distinct_from_provider_alias() {
        assert_eq!(VORTEX_AUTO_MODEL, "vortex/auto");
        assert_ne!(VORTEX_AUTO_MODEL, "openrouter/auto");
    }
}
