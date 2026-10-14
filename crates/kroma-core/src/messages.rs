use crate::attribution::MutationTelemetry;
use crate::types::{EditDiagKind, EditMatchTier, ErrorCode, ToolCallId};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserMessage {
    pub content: String,
}

/// 2026 reasoning model thought / signature block (Claude thinking+signature,
/// Gemini thoughtSignature, DeepSeek reasoning_content, OpenAI reasoning).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ReasoningBlock {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub opaque_payload: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    pub call_id: ToolCallId,
    pub name: String,
    pub raw_arguments: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    Stop,
    ToolCalls,
    Length,
    ContentFilter,
    Unknown(String),
}

impl FinishReason {
    pub fn from_wire(s: &str) -> Self {
        match s {
            "stop" | "end_turn" => FinishReason::Stop,
            "tool_calls" | "tool_use" => FinishReason::ToolCalls,
            "length" | "max_tokens" => FinishReason::Length,
            "content_filter" => FinishReason::ContentFilter,
            other => FinishReason::Unknown(other.to_string()),
        }
    }

    pub fn as_wire(&self) -> &str {
        match self {
            FinishReason::Stop => "stop",
            FinishReason::ToolCalls => "tool_calls",
            FinishReason::Length => "length",
            FinishReason::ContentFilter => "content_filter",
            FinishReason::Unknown(s) => s.as_str(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub cached_input_tokens: Option<u64>,
    pub output_tokens: u64,
    pub reasoning_tokens: Option<u64>,
}

impl TokenUsage {
    pub fn uncached_input_tokens(&self) -> u64 {
        self.input_tokens
            .saturating_sub(self.cached_input_tokens.unwrap_or(0))
    }

    pub fn prefix_cache_hit_ratio(&self) -> f64 {
        if self.input_tokens == 0 {
            0.0
        } else {
            (self.cached_input_tokens.unwrap_or(0) as f64) / (self.input_tokens as f64)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssistantTurn {
    pub content: Option<String>,
    #[serde(default)]
    pub reasoning_blocks: Vec<ReasoningBlock>,
    #[serde(default)]
    pub tool_calls: Vec<ToolCall>,
    pub finish_reason: FinishReason,
    pub usage: Option<TokenUsage>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolOutcome {
    Success,
    Error { code: ErrorCode },
}

impl ToolOutcome {
    pub fn is_success(&self) -> bool {
        matches!(self, ToolOutcome::Success)
    }
}

/// Gate 7 (`v1.2.0`): a binary image payload carried alongside a tool result so a vision-capable
/// model can inspect screenshots, UI mockups and design references without a second round trip.
///
/// Stored on [`ToolResultMeta`] so it is persisted in the session WAL and travels with the message
/// it belongs to. The base64 payload is never written into the human-readable `content` string.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageAttachment {
    pub mime_type: String,
    /// Shared so projecting a turn never deep-clones a multi-megabyte base64 payload.
    #[serde(with = "arc_str_serde")]
    pub base64_data: Arc<str>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    /// Decoded byte length of the original image (not the base64 length).
    pub byte_len: usize,
}

impl ImageAttachment {
    /// `data:image/png;base64,...` form used by the OpenAI `image_url` content part.
    pub fn data_url(&self) -> String {
        format!("data:{};base64,{}", self.mime_type, self.base64_data)
    }

    /// Base64 payload length, used for context-budget accounting.
    pub fn encoded_len(&self) -> usize {
        self.base64_data.len()
    }

    /// One-line summary safe to render in any surface.
    pub fn describe(&self) -> String {
        match (self.width, self.height) {
            (Some(w), Some(h)) => {
                format!("{}, {}x{}, {} bytes", self.mime_type, w, h, self.byte_len)
            }
            _ => format!("{}, {} bytes", self.mime_type, self.byte_len),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ToolResultMeta {
    pub relative_path: Option<String>,
    pub content_sha256: Option<String>,
    pub lines_range: Option<(usize, usize)>,
    pub duration_ms: u64,
    pub truncated: bool,
    pub exit_code: Option<i32>,
    pub terminated_by: Option<String>,
    pub edit_match_tier: Option<EditMatchTier>,
    pub diag_kind: Option<EditDiagKind>,
    pub modified_file: bool,
    /// Gate 7: images attached to this tool result (multimodal `read`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub image_attachments: Vec<ImageAttachment>,
    /// Gate 4: the spans this call mutated plus its post-mutation predictive-check summary.
    /// `serde(default)` keeps pre-Gate-4 WAL transcripts readable.
    #[serde(default)]
    pub mutation: Option<MutationTelemetry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolResultMessage {
    pub call_id: ToolCallId,
    pub tool_name: String,
    pub outcome: ToolOutcome,
    #[serde(with = "arc_str_serde")]
    pub content: Arc<str>,
    pub meta: ToolResultMeta,
}

impl ToolResultMessage {
    /// Gate 7: multimodal attachments carried by this tool result.
    pub fn image_attachments(&self) -> &[ImageAttachment] {
        &self.meta.image_attachments
    }

    /// Gate 7: byte cost of the attached base64 image payloads.
    pub fn image_attachment_bytes(&self) -> usize {
        self.meta
            .image_attachments
            .iter()
            .map(ImageAttachment::encoded_len)
            .sum()
    }
}

pub mod arc_str_serde {
    use serde::{Deserialize, Deserializer, Serializer};
    use std::sync::Arc;

    pub fn serialize<S>(val: &Arc<str>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(val)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Arc<str>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Ok(Arc::from(s))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum TranscriptItem {
    User(UserMessage),
    Assistant(AssistantTurn),
    ToolResult(ToolResultMessage),
}

impl TranscriptItem {
    pub fn estimated_bytes(&self) -> usize {
        match self {
            TranscriptItem::User(u) => u.content.len() + 32,
            TranscriptItem::Assistant(a) => {
                let c_len = a.content.as_ref().map(|s| s.len()).unwrap_or(0);
                let r_len: usize = a
                    .reasoning_blocks
                    .iter()
                    .map(|r| {
                        r.text.as_ref().map(|s| s.len()).unwrap_or(0)
                            + r.signature.as_ref().map(|s| s.len()).unwrap_or(0)
                            + r.opaque_payload.as_ref().map(|s| s.len()).unwrap_or(0)
                    })
                    .sum();
                let tc_len: usize = a
                    .tool_calls
                    .iter()
                    .map(|tc| tc.call_id.0.len() + tc.name.len() + tc.raw_arguments.len() + 32)
                    .sum();
                c_len + r_len + tc_len + 48
            }
            TranscriptItem::ToolResult(tr) => {
                tr.call_id.0.len()
                    + tr.tool_name.len()
                    + tr.content.len()
                    + tr.image_attachment_bytes()
                    + 48
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheBreakpoint {
    ToolsEnd,
    SystemEnd,
    PinnedBaselineEnd,
    TailWindowStart,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheBreakpointMarker {
    pub breakpoint: CacheBreakpoint,
    /// Message index in the projected conversation (0 for ToolsEnd/SystemEnd before history).
    pub message_index: usize,
}
