//! Content-free observations of the assembled logical request.
//!
//! These are UTF-8 payload bytes, not tokens, wire JSON sizes or provider cache hits.
//! Comparing logical prefixes diagnoses request churn without persisting model text,
//! arguments, credentials, filenames or individual message digests in the event stream.

use crate::messages::TranscriptItem;
use crate::prompt::sha256_hex64;
use crate::provider::ProviderRequest;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestComposition {
    pub system_utf8_bytes: usize,
    /// Internal tool definitions, serialized in name order; not protocol wire schemas.
    pub logical_tool_schema_json_bytes: usize,
    pub user_utf8_bytes: usize,
    pub assistant_text_utf8_bytes: usize,
    pub assistant_reasoning_utf8_bytes: usize,
    pub reasoning_signature_utf8_bytes: usize,
    pub reasoning_opaque_utf8_bytes: usize,
    pub tool_argument_utf8_bytes: usize,
    pub tool_result_utf8_bytes: usize,
    pub image_base64_bytes: usize,
    /// Sum of the above payload fields, excluding role/ID/name/framing overhead.
    pub payload_bytes: usize,
    pub message_count: usize,
    pub tool_count: usize,
    pub tool_call_count: usize,
    pub tool_result_count: usize,
    /// None on the first request; these compare logical inputs, not provider caching.
    pub system_unchanged: Option<bool>,
    pub tools_unchanged: Option<bool>,
    /// Longest unchanged prefix against the previous request, excluding tool-result metadata.
    pub matching_history_prefix_messages: Option<usize>,
}

#[derive(Default)]
pub struct RequestObserver {
    previous: Option<PrefixIdentity>,
}

struct PrefixIdentity {
    system: String,
    tools: String,
    messages: Vec<[u8; 32]>,
}

/// Length framing prevents different sequences of strings from sharing the same digest input.
fn field(hash: &mut Sha256, value: &str) {
    hash.update((value.len() as u64).to_le_bytes());
    hash.update(value.as_bytes());
}

impl RequestObserver {
    pub fn observe(&mut self, request: &ProviderRequest) -> RequestComposition {
        let mut tools: Vec<_> = request.tools.iter().collect();
        tools.sort_by(|a, b| a.name.cmp(&b.name));
        // Tool definitions contain only strings and JSON values, whose serialization is infallible.
        let tools_json = serde_json::to_vec(&tools).expect("tool definitions serialize");
        let mut composition = RequestComposition {
            system_utf8_bytes: request.system_prompt.len(),
            logical_tool_schema_json_bytes: tools_json.len(),
            message_count: request.messages.len(),
            tool_count: tools.len(),
            ..Default::default()
        };
        let mut identities = Vec::with_capacity(request.messages.len());
        for item in &request.messages {
            let mut hash = Sha256::new();
            match item {
                TranscriptItem::User(message) => {
                    field(&mut hash, "user");
                    field(&mut hash, &message.content);
                    composition.user_utf8_bytes += message.content.len();
                }
                TranscriptItem::Assistant(message) => {
                    field(&mut hash, "assistant");
                    // Preserve null versus empty string in the logical identity.
                    field(
                        &mut hash,
                        if message.content.is_some() {
                            "some"
                        } else {
                            "none"
                        },
                    );
                    if let Some(text) = &message.content {
                        field(&mut hash, text);
                        composition.assistant_text_utf8_bytes += text.len();
                    }
                    field(&mut hash, &message.reasoning_blocks.len().to_string());
                    for block in &message.reasoning_blocks {
                        for (value, count) in [
                            (&block.text, &mut composition.assistant_reasoning_utf8_bytes),
                            (
                                &block.signature,
                                &mut composition.reasoning_signature_utf8_bytes,
                            ),
                            (
                                &block.opaque_payload,
                                &mut composition.reasoning_opaque_utf8_bytes,
                            ),
                        ] {
                            field(&mut hash, if value.is_some() { "some" } else { "none" });
                            if let Some(value) = value {
                                field(&mut hash, value);
                                *count += value.len();
                            }
                        }
                    }
                    field(&mut hash, &message.tool_calls.len().to_string());
                    for call in &message.tool_calls {
                        field(&mut hash, call.call_id.as_str());
                        field(&mut hash, &call.name);
                        field(&mut hash, &call.raw_arguments);
                        composition.tool_argument_utf8_bytes += call.raw_arguments.len();
                        composition.tool_call_count += 1;
                    }
                }
                TranscriptItem::ToolResult(message) => {
                    field(&mut hash, "tool");
                    field(&mut hash, message.call_id.as_str());
                    field(&mut hash, &message.tool_name);
                    field(&mut hash, &message.content);
                    composition.tool_result_utf8_bytes += message.content.len();
                    composition.tool_result_count += 1;
                    field(&mut hash, &message.meta.image_attachments.len().to_string());
                    for image in &message.meta.image_attachments {
                        field(&mut hash, &image.mime_type);
                        field(&mut hash, &image.base64_data);
                        composition.image_base64_bytes += image.encoded_len();
                    }
                }
            }
            identities.push(hash.finalize().into());
        }
        let identity = PrefixIdentity {
            system: sha256_hex64(request.system_prompt.as_bytes()),
            tools: sha256_hex64(&tools_json),
            messages: identities,
        };
        if let Some(previous) = &self.previous {
            composition.system_unchanged = Some(previous.system == identity.system);
            composition.tools_unchanged = Some(previous.tools == identity.tools);
            composition.matching_history_prefix_messages = Some(
                previous
                    .messages
                    .iter()
                    .zip(&identity.messages)
                    .take_while(|(a, b)| a == b)
                    .count(),
            );
        }
        composition.payload_bytes = composition.system_utf8_bytes
            + composition.logical_tool_schema_json_bytes
            + composition.user_utf8_bytes
            + composition.assistant_text_utf8_bytes
            + composition.assistant_reasoning_utf8_bytes
            + composition.reasoning_signature_utf8_bytes
            + composition.reasoning_opaque_utf8_bytes
            + composition.tool_argument_utf8_bytes
            + composition.tool_result_utf8_bytes
            + composition.image_base64_bytes;
        self.previous = Some(identity);
        composition
    }
}
