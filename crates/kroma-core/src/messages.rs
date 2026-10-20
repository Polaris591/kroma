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
    /// Input tokens served from the provider's prefix cache (`cache_read_input_tokens`).
    pub cached_input_tokens: Option<u64>,
    /// T14 (KR-10): input tokens written *into* the cache this turn
    /// (`cache_creation_input_tokens`). Kept separate from reads: a write costs more than a read and
    /// is not a hit, so folding the two together makes a cold cache look warm.
    #[serde(default)]
    pub cache_creation_input_tokens: Option<u64>,
    pub output_tokens: u64,
    pub reasoning_tokens: Option<u64>,
}

impl TokenUsage {
    /// Input tokens that were neither read from nor written to the cache.
    ///
    /// `input_tokens` is the whole input, so both cache components are subtracted. Counting cache
    /// *creation* as uncached (as the earlier subtraction did) hides the fact that a turn paid the
    /// write premium.
    pub fn uncached_input_tokens(&self) -> u64 {
        self.input_tokens
            .saturating_sub(self.cached_input_tokens.unwrap_or(0))
            .saturating_sub(self.cache_creation_input_tokens.unwrap_or(0))
    }

    /// Tokens written into the cache this turn.
    pub fn cache_creation_tokens(&self) -> u64 {
        self.cache_creation_input_tokens.unwrap_or(0)
    }

    /// Share of the input that was served from cache.
    ///
    /// Reads only. Cache *writes* are not hits; they are the cost of making a later hit possible, and
    /// reporting them as a hit would let a cold cache look warm.
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

/// T05 (KR-04): whether the raw bytes behind a stub can still be produced on demand.
///
/// The stub text is a human-readable hint; *this* is the machine-readable answer. A stub whose
/// evidence was evicted must not be advertised as recoverable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CompactionAvailability {
    /// The raw output is still registered in the evidence store and `recall_evidence` can return it.
    Available,
    /// The raw output is gone (cache eviction, retention limit). The stub remains, the pointer lies.
    Evicted,
    /// Not recorded (pre-T05 WAL): unknown until the evidence store is consulted.
    #[default]
    Unknown,
}

impl CompactionAvailability {
    pub fn as_str(self) -> &'static str {
        match self {
            CompactionAvailability::Available => "available",
            CompactionAvailability::Evicted => "evicted",
            CompactionAvailability::Unknown => "unknown",
        }
    }
}

/// Trusted projection identity, written by the host when an output is archived.
///
/// T05 (KR-04): this typed metadata — not the `source_entry_id=` text inside tool output — is what
/// decides whether an item is compacted and whether its raw bytes are recoverable. Field names map
/// onto the contract as: `source_entry_id` = the host's source event id, `original_sha256` = the raw
/// hash, `reason` = why it was archived, `availability` = whether the raw bytes are still there.
///
/// `serde(default)` on the containing field preserves pre-v2 WAL compatibility, and `availability`
/// defaults to [`CompactionAvailability::Unknown`] so an older WAL never claims availability.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactionMetadata {
    pub schema_version: u32,
    pub source_entry_id: String,
    pub original_sha256: String,
    pub reason: String,
    #[serde(default)]
    pub availability: CompactionAvailability,
}

impl CompactionMetadata {
    /// T05: the host event this stub was minted for (contract name: `source_event_id`).
    pub fn source_event_id(&self) -> &str {
        &self.source_entry_id
    }

    /// T05: hash of the raw bytes this stub replaced (contract name: `raw_hash`).
    pub fn raw_hash(&self) -> &str {
        &self.original_sha256
    }

    /// T05: `true` when the raw bytes can still be produced on demand.
    pub fn is_available(&self) -> bool {
        !matches!(self.availability, CompactionAvailability::Evicted)
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
    /// T09 (KR-18): which execution backend actually ran this command (`host`, `bwrap`,
    /// `container:<runtime>`). Reported, not assumed: a run that required isolation and could not
    /// get it fails instead of recording `host`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_backend: Option<String>,
    /// Host-owned archive metadata. The evidence store, not this immutable marker, is the
    /// authority for whether raw bytes are still available after cache eviction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction: Option<CompactionMetadata>,
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

// The `ToolResult` variant grew past clippy's 200-byte threshold when the typed
// `ToolResultMeta.compaction` metadata landed (T05/KR-04). Boxing it would touch the 126
// construction sites and every `TranscriptItem::ToolResult(..)` pattern match, which is out of
// scope for a correctness patch; the item is moved, not copied, on the hot path. Recorded as a
// deliberate, scoped exception in docs/review/phase1_test_results/.
#[allow(clippy::large_enum_variant)]
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

    /// T03 (KR-03): conservative token estimate for this item.
    ///
    /// Mirrors [`Self::estimated_bytes`] but charges non-ASCII text at 1 token per 2 bytes (CJK is
    /// ~1 token per character) instead of `bytes / 4`, which would under-estimate it. Image payload
    /// bytes are charged at 4 bytes per token. This is an estimate, never presented as a tokenizer.
    pub fn estimated_tokens(&self) -> u64 {
        use crate::budget::conservative_text_tokens;
        match self {
            TranscriptItem::User(u) => conservative_text_tokens(&u.content).saturating_add(8),
            TranscriptItem::Assistant(a) => {
                let mut total = a
                    .content
                    .as_ref()
                    .map(|s| conservative_text_tokens(s))
                    .unwrap_or(0);
                for r in &a.reasoning_blocks {
                    if let Some(s) = &r.text {
                        total = total.saturating_add(conservative_text_tokens(s));
                    }
                    if let Some(s) = &r.signature {
                        total = total.saturating_add(conservative_text_tokens(s));
                    }
                    if let Some(s) = &r.opaque_payload {
                        total = total.saturating_add(conservative_text_tokens(s));
                    }
                }
                for tc in &a.tool_calls {
                    total = total
                        .saturating_add(conservative_text_tokens(&tc.call_id.0))
                        .saturating_add(conservative_text_tokens(&tc.name))
                        .saturating_add(conservative_text_tokens(&tc.raw_arguments))
                        .saturating_add(8);
                }
                total.saturating_add(12)
            }
            TranscriptItem::ToolResult(tr) => conservative_text_tokens(&tr.call_id.0)
                .saturating_add(conservative_text_tokens(&tr.tool_name))
                .saturating_add(conservative_text_tokens(&tr.content))
                .saturating_add((tr.image_attachment_bytes() as u64).saturating_add(3) / 4)
                .saturating_add(12),
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
