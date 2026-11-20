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

/// A model from the provider catalog.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub context_length: Option<u64>,
    /// `Some(false)` means the catalog says the model does not support tool calling.
    #[serde(default)]
    pub supports_tools: Option<bool>,
    #[serde(default)]
    pub pricing_prompt: Option<String>,
    #[serde(default)]
    pub pricing_completion: Option<String>,
    /// Provider-declared input modalities (e.g. `text`, `image`, `video`).
    /// Empty when unknown; a nonempty list names the supported modalities.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub input_modalities: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub output_modalities: Vec<String>,
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
        let out = serde_json::to_string(&m).unwrap();
        assert!(!out.contains("input_modalities"));
    }

    #[test]
    fn model_info_modalities_roundtrip() {
        let m = ModelInfo {
            id: "x".into(),
            name: String::new(),
            context_length: None,
            supports_tools: None,
            pricing_prompt: None,
            pricing_completion: None,
            input_modalities: vec!["text".into(), "image".into()],
            output_modalities: vec!["text".into()],
        };
        let back: ModelInfo = serde_json::from_str(&serde_json::to_string(&m).unwrap()).unwrap();
        assert_eq!(back.input_modalities, vec!["text", "image"]);
        assert_eq!(back.output_modalities, vec!["text"]);
    }
}
