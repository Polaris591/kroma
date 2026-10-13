use crate::messages::{AssistantTurn, FinishReason, ReasoningBlock, TokenUsage, ToolCall};
use crate::provider::ProviderError;
use crate::types::ToolCallId;
use std::collections::BTreeMap;

pub const MAX_SSE_EVENT_BYTES: usize = 256 * 1024; // 256 KiB
pub const MAX_SSE_RESPONSE_BYTES: usize = 32 * 1024 * 1024; // 32 MiB

#[derive(Debug, Default)]
struct PartialToolCall {
    id: String,
    name: String,
    arguments: String,
}

/// SSE Stream Assembler (§4.2):
/// - Handles arbitrary byte chunking (including 1-byte chunks, split UTF-8, split `\r\n`).
/// - Handles `: keepalive` comment lines, multi-line `data:` payloads, and `data: [DONE]`.
/// - Handles OpenAI `stream_options.include_usage=true` trailing chunk with `choices: []`.
/// - Incrementally aggregates `tool_calls` by `index` and `reasoning_blocks`.
/// - Enforces 256 KiB per-event cap and 2 MiB total response cap.
pub struct SseStreamAssembler {
    raw_line_buf: Vec<u8>,
    current_event_data: Vec<String>,
    current_event_bytes: usize,
    total_response_bytes: usize,
    content_buf: String,
    reasoning_blocks_by_index: BTreeMap<usize, ReasoningBlock>,
    current_reasoning_index: usize,
    last_tool_call_index: Option<usize>,
    tool_calls_by_index: BTreeMap<usize, PartialToolCall>,
    finish_reason: Option<FinishReason>,
    usage: Option<TokenUsage>,
    done_received: bool,
    saw_any_business_chunk: bool,
}

impl Default for SseStreamAssembler {
    fn default() -> Self {
        Self::new()
    }
}

impl SseStreamAssembler {
    pub fn new() -> Self {
        Self {
            raw_line_buf: Vec::new(),
            current_event_data: Vec::new(),
            current_event_bytes: 0,
            total_response_bytes: 0,
            content_buf: String::new(),
            reasoning_blocks_by_index: BTreeMap::new(),
            current_reasoning_index: 0,
            last_tool_call_index: None,
            tool_calls_by_index: BTreeMap::new(),
            finish_reason: None,
            usage: None,
            done_received: false,
            saw_any_business_chunk: false,
        }
    }

    pub fn saw_any_business_chunk(&self) -> bool {
        self.saw_any_business_chunk
    }

    pub fn is_done(&self) -> bool {
        self.done_received
    }

    pub fn feed_bytes(&mut self, chunk: &[u8]) -> Result<(), ProviderError> {
        if chunk.is_empty() {
            return Ok(());
        }
        self.total_response_bytes = self.total_response_bytes.saturating_add(chunk.len());
        if self.total_response_bytes > MAX_SSE_RESPONSE_BYTES {
            return Err(ProviderError::IncompleteStream(format!(
                "SSE response exceeded max size ({} bytes)",
                MAX_SSE_RESPONSE_BYTES
            )));
        }

        for &b in chunk {
            if b == b'\n' {
                if self.raw_line_buf.last() == Some(&b'\r') {
                    self.raw_line_buf.pop();
                }
                let line_bytes = std::mem::take(&mut self.raw_line_buf);
                self.process_line(&line_bytes)?;
            } else {
                self.raw_line_buf.push(b);
                if self.raw_line_buf.len() > MAX_SSE_EVENT_BYTES {
                    return Err(ProviderError::IncompleteStream(
                        "SSE single line exceeded 256 KiB cap".to_string(),
                    ));
                }
            }
        }
        Ok(())
    }

    fn process_line(&mut self, line_bytes: &[u8]) -> Result<(), ProviderError> {
        let line = std::str::from_utf8(line_bytes).map_err(|e| {
            ProviderError::IncompleteStream(format!("invalid UTF-8 in SSE line: {}", e))
        })?;

        // Blank line dispatches the accumulated event
        if line.is_empty() {
            if !self.current_event_data.is_empty() {
                let data_payload = self.current_event_data.join("\n");
                self.current_event_data.clear();
                self.current_event_bytes = 0;
                self.process_event_payload(&data_payload)?;
            }
            return Ok(());
        }

        // Comment / heartbeat line (`: keepalive`)
        if line.starts_with(':') {
            return Ok(());
        }

        if let Some(rest) = line.strip_prefix("data:") {
            let payload_line = rest.strip_prefix(' ').unwrap_or(rest);
            self.current_event_bytes += payload_line.len();
            if self.current_event_bytes > MAX_SSE_EVENT_BYTES {
                return Err(ProviderError::IncompleteStream(
                    "SSE event payload exceeded 256 KiB cap".to_string(),
                ));
            }
            self.current_event_data.push(payload_line.to_string());
        }

        Ok(())
    }

    fn process_event_payload(&mut self, payload: &str) -> Result<(), ProviderError> {
        let trimmed = payload.trim();
        if trimmed.is_empty() {
            return Ok(());
        }
        if trimmed == "[DONE]" {
            self.done_received = true;
            return Ok(());
        }

        let val: serde_json::Value = serde_json::from_str(trimmed).map_err(|e| {
            ProviderError::IncompleteStream(format!("malformed JSON in SSE data event: {}", e))
        })?;

        // An in-stream error frame is terminal. Surface the provider's detail directly and
        // do NOT mark it as a business chunk, so a failure before any real delta stays
        // retryable instead of being classified as mid-stream progress.
        if let Some(err) = val.get("error") {
            let detail = err
                .get("message")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| err.to_string());
            return Err(ProviderError::IncompleteStream(format!(
                "provider returned an in-stream error: {}",
                detail
            )));
        }

        self.saw_any_business_chunk = true;

        // Parse optional usage object (including trailing chunk with `choices: []`)
        if let Some(usage_obj) = val.get("usage").filter(|u| u.is_object()) {
            if let Some(parsed_usage) = parse_usage_json(usage_obj) {
                self.usage = Some(parsed_usage);
            }
        }

        if let Some(choices) = val.get("choices").and_then(|c| c.as_array()) {
            if let Some(choice0) = choices.first() {
                if let Some(fr) = choice0.get("finish_reason").and_then(|v| v.as_str()) {
                    self.finish_reason = Some(FinishReason::from_wire(fr));
                }

                if let Some(delta) = choice0.get("delta") {
                    // Snapshot whether the model had already left the reasoning phase before
                    // this delta, so a reasoning delta that also carries content does not
                    // spuriously advance the reasoning block index.
                    let content_or_tools_started_before =
                        !self.content_buf.is_empty() || !self.tool_calls_by_index.is_empty();

                    if let Some(c) = delta.get("content").and_then(|v| v.as_str()) {
                        self.content_buf.push_str(c);
                    }

                    // Support indexed reasoning_blocks / thinking arrays
                    if let Some(arr) = delta
                        .get("reasoning_blocks")
                        .or_else(|| delta.get("thinking").filter(|v| v.is_array()))
                        .and_then(|v| v.as_array())
                    {
                        for (pos, item) in arr.iter().enumerate() {
                            let idx = item
                                .get("index")
                                .and_then(|v| v.as_u64())
                                .map(|n| n as usize)
                                .unwrap_or(pos);
                            let block =
                                self.reasoning_blocks_by_index
                                    .entry(idx)
                                    .or_insert_with(|| ReasoningBlock {
                                        text: None,
                                        signature: None,
                                        opaque_payload: None,
                                    });
                            if let Some(t) = item
                                .get("text")
                                .or_else(|| item.get("thinking"))
                                .or_else(|| item.get("reasoning_content"))
                                .and_then(|v| v.as_str())
                            {
                                block.text.get_or_insert_with(String::new).push_str(t);
                            }
                            if let Some(s) = item
                                .get("signature")
                                .or_else(|| item.get("thought_signature"))
                                .and_then(|v| v.as_str())
                            {
                                block.signature.get_or_insert_with(String::new).push_str(s);
                            }
                            if let Some(op) = item
                                .get("opaque_payload")
                                .or_else(|| item.get("data"))
                                .and_then(|v| v.as_str())
                            {
                                block
                                    .opaque_payload
                                    .get_or_insert_with(String::new)
                                    .push_str(op);
                            }
                            self.current_reasoning_index = idx;
                        }
                    } else {
                        // Support flat reasoning fields on delta. `reasoning_content`,
                        // `reasoning` and `thinking` are mutually exclusive aliases for the same
                        // text, so read exactly one of them instead of concatenating every alias
                        // into a duplicated string.
                        let delta_text = delta
                            .get("reasoning_content")
                            .or_else(|| delta.get("reasoning"))
                            .or_else(|| delta.get("thinking"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        let delta_sig = delta
                            .get("signature")
                            .or_else(|| delta.get("thought_signature"))
                            .and_then(|v| v.as_str());
                        let delta_opaque = delta.get("opaque_payload").and_then(|v| v.as_str());

                        if !delta_text.is_empty() || delta_sig.is_some() || delta_opaque.is_some() {
                            let explicit_idx = delta
                                .get("reasoning_index")
                                .or_else(|| delta.get("block_index"))
                                .and_then(|v| v.as_u64())
                                .map(|n| n as usize);

                            let target_idx = if let Some(idx) = explicit_idx {
                                self.current_reasoning_index = idx;
                                idx
                            } else {
                                // Reasoning blocks are delimited primarily by explicit
                                // `reasoning_index`/`block_index` fields. Without them, a new
                                // block is inferred only once the model has already moved on to
                                // emitting content/tool_calls, or when a fresh reasoning fragment
                                // arrives outside the content phase while the current block already
                                // carries a signature. Signature text itself is never inspected:
                                // Base64 signatures are opaque and may contain any character
                                // (including '_'), so fragment detection must not depend on it.
                                let delta_carries_content_phase = delta
                                    .get("content")
                                    .and_then(|v| v.as_str())
                                    .map(|s| !s.is_empty())
                                    .unwrap_or(false)
                                    || delta
                                        .get("tool_calls")
                                        .and_then(|v| v.as_array())
                                        .map(|a| !a.is_empty())
                                        .unwrap_or(false);
                                let current_has_signature = self
                                    .reasoning_blocks_by_index
                                    .get(&self.current_reasoning_index)
                                    .and_then(|b| b.signature.as_ref())
                                    .map(|s| !s.is_empty())
                                    .unwrap_or(false);
                                let should_advance = content_or_tools_started_before
                                    || (current_has_signature && !delta_carries_content_phase);
                                if should_advance {
                                    self.current_reasoning_index += 1;
                                }
                                self.current_reasoning_index
                            };

                            let block = self
                                .reasoning_blocks_by_index
                                .entry(target_idx)
                                .or_insert_with(|| ReasoningBlock {
                                    text: None,
                                    signature: None,
                                    opaque_payload: None,
                                });
                            if !delta_text.is_empty() {
                                block
                                    .text
                                    .get_or_insert_with(String::new)
                                    .push_str(delta_text);
                            }
                            if let Some(sig) = delta_sig {
                                block
                                    .signature
                                    .get_or_insert_with(String::new)
                                    .push_str(sig);
                            }
                            if let Some(opaque) = delta_opaque {
                                block
                                    .opaque_payload
                                    .get_or_insert_with(String::new)
                                    .push_str(opaque);
                            }
                        }
                    }

                    if let Some(tc_arr) = delta.get("tool_calls").and_then(|v| v.as_array()) {
                        for tc_item in tc_arr {
                            let explicit_idx = tc_item
                                .get("index")
                                .and_then(|v| v.as_u64())
                                .map(|n| n as usize);
                            let id_part = tc_item.get("id").and_then(|v| v.as_str());

                            let idx = match explicit_idx {
                                Some(i) => {
                                    self.last_tool_call_index = Some(i);
                                    i
                                }
                                None => {
                                    if let Some(new_id) = id_part.filter(|s| !s.is_empty()) {
                                        if let Some((&existing_idx, _)) = self
                                            .tool_calls_by_index
                                            .iter()
                                            .find(|(_, v)| v.id == new_id)
                                        {
                                            self.last_tool_call_index = Some(existing_idx);
                                            existing_idx
                                        } else {
                                            let next_idx = self
                                                .tool_calls_by_index
                                                .keys()
                                                .next_back()
                                                .map(|k| k + 1)
                                                .unwrap_or(0);
                                            self.last_tool_call_index = Some(next_idx);
                                            next_idx
                                        }
                                    } else {
                                        self.last_tool_call_index.unwrap_or(0)
                                    }
                                }
                            };

                            let entry = self.tool_calls_by_index.entry(idx).or_default();
                            if let Some(id_str) = id_part {
                                entry.id.push_str(id_str);
                            }
                            if let Some(func) = tc_item.get("function") {
                                if let Some(name_part) = func.get("name").and_then(|v| v.as_str()) {
                                    entry.name.push_str(name_part);
                                }
                                if let Some(arg_part) =
                                    func.get("arguments").and_then(|v| v.as_str())
                                {
                                    entry.arguments.push_str(arg_part);
                                }
                            }
                        }
                    }
                }
            }
        }

        Ok(())
    }

    pub fn finalize(self) -> Result<AssistantTurn, ProviderError> {
        if !self.done_received {
            return Err(ProviderError::IncompleteStream(
                "SSE stream ended without data: [DONE]".to_string(),
            ));
        }
        if !self.raw_line_buf.is_empty() || !self.current_event_data.is_empty() {
            return Err(ProviderError::IncompleteStream(
                "SSE stream ended with undispatched partial event buffer".to_string(),
            ));
        }

        let finish_reason = self.finish_reason.ok_or_else(|| {
            ProviderError::IncompleteStream(
                "SSE stream completed without a finish_reason".to_string(),
            )
        })?;

        let tool_calls: Vec<ToolCall> = self
            .tool_calls_by_index
            .into_values()
            .map(|p| ToolCall {
                call_id: ToolCallId::new(p.id),
                name: p.name,
                raw_arguments: p.arguments,
            })
            .collect();

        let reasoning_blocks: Vec<ReasoningBlock> = self
            .reasoning_blocks_by_index
            .into_values()
            .filter(|b| b.text.is_some() || b.signature.is_some() || b.opaque_payload.is_some())
            .collect();

        let content = (!self.content_buf.is_empty()).then_some(self.content_buf);

        Ok(AssistantTurn {
            content,
            reasoning_blocks,
            tool_calls,
            finish_reason,
            usage: self.usage,
        })
    }
}

pub fn parse_usage_json(usage_obj: &serde_json::Value) -> Option<TokenUsage> {
    let input_tokens = usage_obj
        .get("prompt_tokens")
        .or_else(|| usage_obj.get("input_tokens"))
        .and_then(|v| v.as_u64())?;
    let output_tokens = usage_obj
        .get("completion_tokens")
        .or_else(|| usage_obj.get("output_tokens"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);

    let cached_input_tokens = usage_obj
        .get("prompt_tokens_details")
        .and_then(|d| d.get("cached_tokens"))
        .and_then(|v| v.as_u64())
        .or_else(|| {
            usage_obj
                .get("cache_read_input_tokens")
                .and_then(|v| v.as_u64())
        })
        .or_else(|| {
            usage_obj
                .get("cached_input_tokens")
                .and_then(|v| v.as_u64())
        })
        .or_else(|| usage_obj.get("cached_tokens").and_then(|v| v.as_u64()))
        .or_else(|| {
            usage_obj
                .get("prompt_cache_hit_tokens")
                .and_then(|v| v.as_u64())
        });

    let reasoning_tokens = usage_obj
        .get("completion_tokens_details")
        .and_then(|d| d.get("reasoning_tokens"))
        .and_then(|v| v.as_u64())
        .or_else(|| usage_obj.get("reasoning_tokens").and_then(|v| v.as_u64()))
        .or_else(|| {
            usage_obj
                .get("completion_thinking_tokens")
                .and_then(|v| v.as_u64())
        });

    Some(TokenUsage {
        input_tokens,
        cached_input_tokens,
        output_tokens,
        reasoning_tokens,
    })
}
