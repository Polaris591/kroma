//! Gate 7 (`v1.2.0`) — native Anthropic Messages API (`/v1/messages`) client.
//!
//! Talking to `api.anthropic.com` directly (instead of through an OpenAI-compatible shim) buys two
//! things the kroma actually measures:
//!
//! 1. **Explicit prompt caching.** Anthropic only caches what the caller marks with
//!    `cache_control: {"type": "ephemeral"}`. This client places breakpoints on the static system
//!    block, the last tool definition and the projector's frozen-prefix boundary, which is what
//!    turns a long conversation into a ~90% `cache_read_input_tokens` ratio.
//! 2. **Faithful reasoning + tool-call round-tripping.** `thinking` blocks (with their `signature`)
//!    and `tool_use` / `tool_result` pairing are translated one-to-one instead of being flattened
//!    into text.
//!
//! Failure classification and HTTP framing are shared with every other protocol through
//! [`crate::provider::transport`].

use crate::budget::BudgetConfig;
use crate::messages::{
    AssistantTurn, CacheBreakpoint, FinishReason, ReasoningBlock, TokenUsage, ToolCall, TranscriptItem,
};
use crate::provider::endpoint::{validate_base_url, ValidatedEndpoint};
use crate::provider::sse::MAX_SSE_RESPONSE_BYTES;
use crate::provider::transport::{
    pump_sse_events, read_error_body, read_full_body, send_request, BodyFailure, HttpRequestSpec,
    TransportFailure,
};
use crate::provider::{ModelProvider, ProviderCompletion, ProviderError, ProviderRequest};
use crate::tool::definition::ToolDefinition;
use crate::types::ToolCallId;
use serde_json::{json, Map, Value};
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, Instant};
use tokio::sync::watch;

pub const ANTHROPIC_VERSION: &str = "2023-06-01";
pub const ANTHROPIC_PROMPT_CACHING_BETA: &str = "prompt-caching-2024-07-31";
/// Anthropic requires an explicit output cap; 8192 matches the documented default ceiling for
/// current Claude models without truncating a normal tool-calling turn.
pub const DEFAULT_MAX_TOKENS: u32 = 8192;
const CACHE_CONTROL_EPHEMERAL: &str = "ephemeral";

#[derive(Clone, Default)]
pub struct AnthropicMessagesConfig {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub max_retries: usize,
    pub max_tokens: u32,
}

impl fmt::Debug for AnthropicMessagesConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AnthropicMessagesConfig")
            .field("base_url", &self.base_url)
            .field("api_key", &"x-api-key ***")
            .field("model", &self.model)
            .field("max_retries", &self.max_retries)
            .field("max_tokens", &self.max_tokens)
            .finish()
    }
}

/// Deterministic serialization of an Anthropic Messages request body.
pub fn serialize_anthropic_request_body(
    model: &str,
    system_prompt: &str,
    tools: &[ToolDefinition],
    messages: &[TranscriptItem],
    cache_breakpoints: &[crate::messages::CacheBreakpointMarker],
    stream: bool,
    max_tokens: u32,
) -> String {
    let mut sorted_tools = tools.to_vec();
    sorted_tools.sort_by(|a, b| a.name.as_bytes().cmp(b.name.as_bytes()));

    // Breakpoint 1: the static system block. It never changes between turns, so caching it is pure
    // win — this is the largest single saving on a long session. Anthropic rejects an empty text
    // block, so the whole `system` field is omitted when there is no prompt to send.
    let system_blocks = if system_prompt.trim().is_empty() {
        None
    } else {
        Some(json!([{
            "type": "text",
            "text": system_prompt,
            "cache_control": { "type": CACHE_CONTROL_EPHEMERAL },
        }]))
    };

    let tool_count = sorted_tools.len();
    let tools_json: Vec<Value> = sorted_tools
        .iter()
        .enumerate()
        .map(|(idx, t)| {
            let mut obj = Map::new();
            obj.insert("name".to_string(), Value::String(t.name.clone()));
            obj.insert(
                "description".to_string(),
                Value::String(t.description.clone()),
            );
            obj.insert("input_schema".to_string(), t.parameters.clone());
            // Breakpoint 2: the last tool definition closes the tool-schema prefix.
            if idx + 1 == tool_count {
                obj.insert(
                    "cache_control".to_string(),
                    json!({ "type": CACHE_CONTROL_EPHEMERAL }),
                );
            }
            Value::Object(obj)
        })
        .collect();

    let (mut wire_messages, item_to_wire) = build_anthropic_messages(messages);

    // Breakpoint 3: the frozen-prefix boundary. `PinnedBaselineEnd` is the projector's durable
    // boundary; `TailWindowStart` is the fallback when no baseline has been pinned yet. The marker
    // indexes the *transcript*, so it must be translated through the merge map before it can index
    // the wire messages.
    if let Some(idx) =
        pick_prefix_breakpoint_index(cache_breakpoints, &item_to_wire, wire_messages.len())
    {
        if let Some(Value::Object(msg)) = wire_messages.get_mut(idx) {
            if let Some(Value::Array(blocks)) = msg.get_mut("content") {
                if let Some(Value::Object(last)) = blocks.last_mut() {
                    last.insert(
                        "cache_control".to_string(),
                        json!({ "type": CACHE_CONTROL_EPHEMERAL }),
                    );
                }
            }
        }
    }

    let mut root = Map::new();
    root.insert("model".to_string(), Value::String(model.to_string()));
    root.insert(
        "max_tokens".to_string(),
        Value::Number(serde_json::Number::from(max_tokens.max(1))),
    );
    if let Some(system) = system_blocks {
        root.insert("system".to_string(), system);
    }
    root.insert("messages".to_string(), Value::Array(wire_messages));
    if !tools_json.is_empty() {
        root.insert("tools".to_string(), Value::Array(tools_json));
    }
    root.insert("stream".to_string(), Value::Bool(stream));

    serde_json::to_string(&Value::Object(root)).unwrap_or_else(|_| "{}".to_string())
}

fn pick_prefix_breakpoint_index(
    markers: &[crate::messages::CacheBreakpointMarker],
    item_to_wire: &[usize],
    message_count: usize,
) -> Option<usize> {
    if message_count == 0 {
        return None;
    }
    let candidate = markers
        .iter()
        .find(|m| m.breakpoint == CacheBreakpoint::PinnedBaselineEnd)
        .or_else(|| {
            markers
                .iter()
                .find(|m| m.breakpoint == CacheBreakpoint::TailWindowStart)
        })?;
    let mapped = item_to_wire
        .get(candidate.message_index)
        .copied()
        .unwrap_or(candidate.message_index);
    Some(mapped.min(message_count - 1))
}

/// Translate the kroma transcript into Anthropic `messages`.
///
/// Anthropic enforces three wire rules that a naive translation breaks:
/// 1. `user` and `assistant` roles must strictly alternate, so consecutive same-role messages are
///    merged into one (with `tool_result` blocks kept ahead of text blocks).
/// 2. `thinking` / `redacted_thinking` blocks must lead an assistant turn.
/// 3. Text blocks must be non-empty.
///
/// Returns the wire messages plus a transcript-index to wire-index map, because merging changes the
/// positions the `cache_control` prefix breakpoint indexes.
fn build_anthropic_messages(messages: &[TranscriptItem]) -> (Vec<Value>, Vec<usize>) {
    let mut out: Vec<Value> = Vec::with_capacity(messages.len());
    let mut item_to_wire: Vec<usize> = Vec::with_capacity(messages.len());
    for item in messages {
        let (role, blocks) = anthropic_blocks_for_item(item);
        if blocks.is_empty() {
            // An item with nothing to say must not emit an empty content array: point the mapping at
            // the next wire message instead (the caller clamps the final index).
            item_to_wire.push(out.len());
            continue;
        }
        match out.last() {
            Some(prev) if prev.get("role").and_then(Value::as_str) == Some(role) => {
                let idx = out.len() - 1;
                merge_wire_blocks(&mut out[idx], role, blocks);
                item_to_wire.push(idx);
            }
            _ => {
                out.push(json!({ "role": role, "content": blocks }));
                item_to_wire.push(out.len() - 1);
            }
        }
    }
    (out, item_to_wire)
}

/// Build the Anthropic content blocks for a single transcript item, or an empty vector when the
/// item carries nothing Anthropic will accept.
fn anthropic_blocks_for_item(item: &TranscriptItem) -> (&'static str, Vec<Value>) {
    match item {
        TranscriptItem::User(u) => {
            if u.content.is_empty() {
                return ("user", Vec::new());
            }
            ("user", vec![json!({ "type": "text", "text": u.content })])
        }
        TranscriptItem::Assistant(a) => {
            let mut blocks: Vec<Value> = Vec::new();
            // Rule 2: extended-thinking blocks lead the assistant turn.
            for rb in &a.reasoning_blocks {
                match (&rb.text, &rb.opaque_payload) {
                    (None, Some(data)) => blocks.push(json!({
                        "type": "redacted_thinking",
                        "data": data,
                    })),
                    _ => {
                        let mut obj = Map::new();
                        obj.insert("type".to_string(), Value::String("thinking".to_string()));
                        if let Some(t) = &rb.text {
                            obj.insert("thinking".to_string(), Value::String(t.clone()));
                        }
                        if let Some(s) = &rb.signature {
                            obj.insert("signature".to_string(), Value::String(s.clone()));
                        }
                        if let Some(p) = &rb.opaque_payload {
                            obj.insert("data".to_string(), Value::String(p.clone()));
                        }
                        blocks.push(Value::Object(obj));
                    }
                }
            }
            if let Some(text) = a.content.as_ref().filter(|t| !t.is_empty()) {
                blocks.push(json!({ "type": "text", "text": text }));
            }
            for tc in &a.tool_calls {
                let input =
                    serde_json::from_str::<Value>(&tc.raw_arguments).unwrap_or_else(|_| json!({}));
                blocks.push(json!({
                    "type": "tool_use",
                    "id": tc.call_id.as_str(),
                    "name": tc.name,
                    "input": input,
                }));
            }
            ("assistant", blocks)
        }
        TranscriptItem::ToolResult(tr) => {
            // Rule 3: an empty text block is rejected, so only non-empty content is sent.
            let mut content: Vec<Value> = Vec::new();
            if !tr.content.is_empty() {
                content.push(json!({ "type": "text", "text": tr.content.as_ref() }));
            }
            for image in &tr.meta.image_attachments {
                content.push(json!({
                    "type": "image",
                    "source": {
                        "type": "base64",
                        "media_type": image.mime_type,
                        "data": &*image.base64_data,
                    }
                }));
            }
            if content.is_empty() {
                return ("user", Vec::new());
            }
            (
                "user",
                vec![json!({
                    "type": "tool_result",
                    "tool_use_id": tr.call_id.as_str(),
                    "content": content,
                    "is_error": !tr.outcome.is_success(),
                })],
            )
        }
    }
}

/// Append `blocks` into an existing wire message of the same role.
///
/// Anthropic requires every `tool_result` block to precede any text/image block inside a `user`
/// message, so merged `user` turns are re-partitioned rather than naively appended.
fn merge_wire_blocks(msg: &mut Value, role: &str, mut blocks: Vec<Value>) {
    let Some(Value::Array(existing)) = msg.get_mut("content") else {
        return;
    };
    if role != "user" {
        existing.append(&mut blocks);
        return;
    }
    let mut tool_blocks: Vec<Value> = Vec::new();
    let mut other: Vec<Value> = Vec::new();
    for block in existing.drain(..).chain(blocks.drain(..)) {
        if block.get("type").and_then(Value::as_str) == Some("tool_result") {
            tool_blocks.push(block);
        } else {
            other.push(block);
        }
    }
    existing.extend(tool_blocks);
    existing.extend(other);
}

pub fn parse_anthropic_response(body: &str) -> Result<AssistantTurn, ProviderError> {
    let val: Value = serde_json::from_str(body).map_err(|e| {
        ProviderError::Transport(format!("invalid JSON in Anthropic response: {}", e))
    })?;
    if let Some(err) = val.get("error") {
        return Err(ProviderError::Transport(format!(
            "Anthropic error response: {}",
            err
        )));
    }
    let stop_reason = val
        .get("stop_reason")
        .and_then(Value::as_str)
        .unwrap_or("end_turn");
    let (content, reasoning_blocks, tool_calls) = parse_anthropic_content_blocks(
        val.get("content")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[]),
    );
    let usage = val.get("usage").map(parse_anthropic_usage);
    Ok(AssistantTurn {
        content,
        reasoning_blocks,
        tool_calls,
        finish_reason: FinishReason::from_wire(stop_reason),
        usage,
    })
}

fn parse_anthropic_content_blocks(
    blocks: &[Value],
) -> (Option<String>, Vec<ReasoningBlock>, Vec<ToolCall>) {
    let mut text = String::new();
    let mut reasoning: Vec<ReasoningBlock> = Vec::new();
    let mut calls: Vec<ToolCall> = Vec::new();
    for block in blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(t) = block.get("text").and_then(Value::as_str) {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(t);
                }
            }
            Some("thinking") | Some("redacted_thinking") => {
                reasoning.push(ReasoningBlock {
                    text: block
                        .get("thinking")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    signature: block
                        .get("signature")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    opaque_payload: block
                        .get("data")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                });
            }
            Some("tool_use") => {
                let id = block.get("id").and_then(Value::as_str).unwrap_or("");
                let name = block.get("name").and_then(Value::as_str).unwrap_or("");
                let input = block.get("input").cloned().unwrap_or_else(|| json!({}));
                calls.push(ToolCall {
                    call_id: ToolCallId::new(id),
                    name: name.to_string(),
                    raw_arguments: input.to_string(),
                });
            }
            _ => {}
        }
    }
    (
        if text.is_empty() { None } else { Some(text) },
        reasoning,
        calls,
    )
}

fn parse_anthropic_usage(usage: &Value) -> TokenUsage {
    let uncached = usage
        .get("input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let cache_read = usage
        .get("cache_read_input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let cache_creation = usage
        .get("cache_creation_input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    TokenUsage {
        input_tokens: uncached + cache_read + cache_creation,
        cached_input_tokens: Some(cache_read),
        output_tokens: usage
            .get("output_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        reasoning_tokens: None,
    }
}

/// Incremental assembler for the Anthropic SSE event stream.
#[derive(Default)]
struct AnthropicStreamAssembler {
    text: String,
    reasoning: Vec<ReasoningBlock>,
    calls: Vec<ToolCall>,
    /// Anthropic emits each tool call's `input` as a sequence of `input_json_delta` fragments.
    pending_json: std::collections::HashMap<usize, String>,
    current_kind: std::collections::HashMap<usize, String>,
    current_id: std::collections::HashMap<usize, String>,
    current_name: std::collections::HashMap<usize, String>,
    stop_reason: Option<String>,
    usage: Option<TokenUsage>,
    saw_business: bool,
    /// Set only on `message_stop`: the explicit end-of-stream signal.
    saw_message_stop: bool,
}

impl AnthropicStreamAssembler {
    fn feed(&mut self, event: &str, data: &str) -> Result<(), ProviderError> {
        if data.trim().is_empty() {
            return Ok(());
        }
        let val: Value = serde_json::from_str(data).map_err(|e| {
            ProviderError::IncompleteStream(format!("invalid Anthropic SSE payload: {}", e))
        })?;
        match event {
            "message_start" => {
                if let Some(usage) = val.get("message").and_then(|m| m.get("usage")) {
                    self.usage = Some(parse_anthropic_usage(usage));
                }
                self.saw_business = true;
            }
            "content_block_start" => {
                let index = val.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                if let Some(block) = val.get("content_block") {
                    let kind = block
                        .get("type")
                        .and_then(Value::as_str)
                        .unwrap_or("text")
                        .to_string();
                    self.current_kind.insert(index, kind.clone());
                    if kind == "tool_use" {
                        self.current_id.insert(
                            index,
                            block
                                .get("id")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string(),
                        );
                        self.current_name.insert(
                            index,
                            block
                                .get("name")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string(),
                        );
                        self.pending_json.insert(index, String::new());
                    }
                }
                self.saw_business = true;
            }
            "content_block_delta" => {
                let index = val.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                if let Some(delta) = val.get("delta") {
                    match delta.get("type").and_then(Value::as_str) {
                        Some("text_delta") => {
                            if let Some(t) = delta.get("text").and_then(Value::as_str) {
                                self.text.push_str(t);
                            }
                        }
                        Some("thinking_delta") => {
                            if let Some(t) = delta.get("thinking").and_then(Value::as_str) {
                                if let Some(last) = self.reasoning.last_mut() {
                                    last.text.get_or_insert_with(String::new).push_str(t);
                                } else {
                                    self.reasoning.push(ReasoningBlock {
                                        text: Some(t.to_string()),
                                        signature: None,
                                        opaque_payload: None,
                                    });
                                }
                            }
                        }
                        Some("signature_delta") => {
                            if let Some(s) = delta.get("signature").and_then(Value::as_str) {
                                if let Some(last) = self.reasoning.last_mut() {
                                    last.signature = Some(s.to_string());
                                }
                            }
                        }
                        Some("input_json_delta") => {
                            if let Some(part) = delta.get("partial_json").and_then(Value::as_str) {
                                self.pending_json.entry(index).or_default().push_str(part);
                            }
                        }
                        _ => {}
                    }
                }
                self.saw_business = true;
            }
            "content_block_stop" => {
                let index = val.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                if let Some(raw) = self.pending_json.remove(&index) {
                    let args = if raw.trim().is_empty() {
                        "{}".to_string()
                    } else {
                        raw
                    };
                    self.calls.push(ToolCall {
                        call_id: ToolCallId::new(
                            self.current_id.remove(&index).unwrap_or_default(),
                        ),
                        name: self.current_name.remove(&index).unwrap_or_default(),
                        raw_arguments: args,
                    });
                }
                self.current_kind.remove(&index);
            }
            "message_delta" => {
                if let Some(reason) = val
                    .get("delta")
                    .and_then(|d| d.get("stop_reason"))
                    .and_then(Value::as_str)
                {
                    self.stop_reason = Some(reason.to_string());
                }
                if let Some(usage) = val.get("usage") {
                    let mut merged = self.usage.take().unwrap_or_default();
                    if let Some(out) = usage.get("output_tokens").and_then(Value::as_u64) {
                        merged.output_tokens = out;
                    }
                    if let Some(cache_read) = usage
                        .get("cache_read_input_tokens")
                        .and_then(Value::as_u64)
                    {
                        merged.cached_input_tokens = Some(cache_read);
                    }
                    self.usage = Some(merged);
                }
            }
            "error" => {
                return Err(ProviderError::IncompleteStream(format!(
                    "Anthropic stream error: {}",
                    val
                )));
            }
            "message_stop" => {
                self.saw_message_stop = true;
            }
            _ => {}
        }
        Ok(())
    }

    fn finalize(self) -> Result<AssistantTurn, ProviderError> {
        if !self.saw_business {
            return Err(ProviderError::IncompleteStream(
                "Anthropic SSE stream contained no message events".to_string(),
            ));
        }
        // A stream that ends before `message_stop` (proxy disconnect, chunked trailer cut short,
        // idle-timeout abort) carries no usable turn. Defaulting the finish reason here would
        // silently drop any unclosed `tool_use` block and report a half-answer as success (§4.3).
        if !self.saw_message_stop {
            return Err(ProviderError::IncompleteStream(
                "Anthropic SSE stream ended before message_stop".to_string(),
            ));
        }
        let Some(stop_reason) = self.stop_reason.as_deref() else {
            return Err(ProviderError::IncompleteStream(
                "Anthropic SSE stream ended before message_delta carried a stop_reason".to_string(),
            ));
        };
        if !self.pending_json.is_empty() {
            return Err(ProviderError::IncompleteStream(format!(
                "Anthropic SSE stream ended with {} unclosed tool_use block(s)",
                self.pending_json.len()
            )));
        }
        let finish_reason = FinishReason::from_wire(stop_reason);
        Ok(AssistantTurn {
            content: if self.text.is_empty() {
                None
            } else {
                Some(self.text)
            },
            reasoning_blocks: self.reasoning,
            tool_calls: self.calls,
            finish_reason,
            usage: self.usage,
        })
    }
}

pub struct AnthropicMessagesClient {
    config: AnthropicMessagesConfig,
    endpoint: ValidatedEndpoint,
}

impl AnthropicMessagesClient {
    pub fn new(config: AnthropicMessagesConfig) -> Result<Self, ProviderError> {
        if config.api_key.bytes().any(|b| b < 0x20 || b == 0x7f) {
            return Err(ProviderError::Transport(
                "api_key must not contain ASCII control characters".to_string(),
            ));
        }
        if config.model.bytes().any(|b| b < 0x20 || b == 0x7f) {
            return Err(ProviderError::Transport(
                "model must not contain ASCII control characters".to_string(),
            ));
        }
        let endpoint = validate_base_url(&config.base_url)?;
        Ok(Self { config, endpoint })
    }

    pub fn endpoint(&self) -> &ValidatedEndpoint {
        &self.endpoint
    }

    fn messages_path(&self) -> String {
        let base = &self.endpoint.base_path;
        if base.ends_with("/messages") {
            base.clone()
        } else if base.ends_with("/v1") {
            format!("{}/messages", base)
        } else {
            format!("{}/v1/messages", base)
        }
    }
}

impl ModelProvider for AnthropicMessagesClient {
    fn complete<'a>(
        &'a self,
        req: ProviderRequest,
        budget: &'a BudgetConfig,
        mut cancel_rx: watch::Receiver<bool>,
    ) -> Pin<Box<dyn Future<Output = Result<ProviderCompletion, ProviderError>> + Send + 'a>> {
        Box::pin(async move {
            let configured_max_tokens = if self.config.max_tokens == 0 {
                DEFAULT_MAX_TOKENS
            } else {
                self.config.max_tokens
            };
            // F08 (`v1.4.0`): clamp the per-request output allowance to the remaining token budget
            // so one completion cannot overshoot the run's spend cap.
            let max_tokens = match req.max_output_tokens {
                Some(limit) => configured_max_tokens.min(limit.min(u64::from(u32::MAX)) as u32).max(1),
                None => configured_max_tokens,
            };
            let body_json = serialize_anthropic_request_body(
                &self.config.model,
                &req.system_prompt,
                &req.tools,
                &req.messages,
                &req.cache_breakpoints,
                req.stream,
                max_tokens,
            );
            let request_body_bytes = body_json.len();
            let max_attempts = (self.config.max_retries + 1).min(budget.max_model_request_attempts);
            let mut attempts_used = 0usize;

            loop {
                if *cancel_rx.borrow() {
                    return Err(ProviderError::Cancelled);
                }
                attempts_used += 1;
                let attempt_start = Instant::now();
                let outcome = tokio::select! {
                    res = self.attempt(&body_json, req.stream, budget, attempt_start) => res,
                    _ = async {
                        loop {
                            if cancel_rx.changed().await.is_err() {
                                std::future::pending::<()>().await;
                            }
                            if *cancel_rx.borrow() {
                                break;
                            }
                        }
                    } => Err(AttemptFailure::Fatal(ProviderError::Cancelled)),
                };

                match outcome {
                    Ok((turn, latency_ms, ttft_ms)) => {
                        return Ok(ProviderCompletion {
                            turn,
                            request_body_bytes,
                            latency_ms,
                            ttft_ms,
                            attempts_used,
                        });
                    }
                    Err(AttemptFailure::Fatal(err)) => return Err(err),
                    Err(AttemptFailure::Retryable { err, retry_after }) => {
                        if attempts_used >= max_attempts {
                            return Err(err);
                        }
                        let backoff = retry_after.unwrap_or_else(|| {
                            Duration::from_millis(50 * (1u64 << (attempts_used - 1)))
                        });
                        tokio::select! {
                            _ = tokio::time::sleep(backoff) => {}
                            _ = async {
                                loop {
                                    if cancel_rx.changed().await.is_err() {
                                        std::future::pending::<()>().await;
                                    }
                                    if *cancel_rx.borrow() {
                                        break;
                                    }
                                }
                            } => return Err(ProviderError::Cancelled),
                        }
                    }
                }
            }
        })
    }
}

enum AttemptFailure {
    Fatal(ProviderError),
    Retryable {
        err: ProviderError,
        retry_after: Option<Duration>,
    },
}

impl AnthropicMessagesClient {
    async fn attempt(
        &self,
        body_json: &str,
        stream: bool,
        budget: &BudgetConfig,
        attempt_start: Instant,
    ) -> Result<(AssistantTurn, u64, Option<u64>), AttemptFailure> {
        let deadline = attempt_start + budget.request_timeout;
        let path = self.messages_path();
        let headers = vec![
            ("x-api-key".to_string(), self.config.api_key.clone()),
            ("anthropic-version".to_string(), ANTHROPIC_VERSION.to_string()),
            (
                "anthropic-beta".to_string(),
                ANTHROPIC_PROMPT_CACHING_BETA.to_string(),
            ),
        ];
        let spec = HttpRequestSpec {
            method: "POST",
            path: &path,
            headers: &headers,
            body: body_json,
            accept: if stream {
                "text/event-stream"
            } else {
                "application/json"
            },
        };

        let mut head = send_request(&self.endpoint, &spec, budget, deadline)
            .await
            .map_err(map_transport_failure)?;

        if matches!(head.status, 301 | 302 | 303 | 307 | 308) {
            return Err(AttemptFailure::Fatal(ProviderError::RedirectBlocked(
                head.status,
            )));
        }
        if matches!(head.status, 429 | 500 | 502 | 503 | 504 | 529) {
            let body = read_error_body(&mut head, budget, deadline).await;
            return Err(AttemptFailure::Retryable {
                err: ProviderError::HttpStatus {
                    status: head.status,
                    body: String::from_utf8_lossy(&body).to_string(),
                },
                retry_after: head.retry_after,
            });
        }
        if head.status != 200 {
            let body = read_error_body(&mut head, budget, deadline).await;
            return Err(AttemptFailure::Fatal(ProviderError::HttpStatus {
                status: head.status,
                body: String::from_utf8_lossy(&body).to_string(),
            }));
        }

        if !stream {
            let bytes = read_full_body(&mut head, budget, deadline, MAX_SSE_RESPONSE_BYTES)
                .await
                .map_err(AttemptFailure::Fatal)?;
            let text = std::str::from_utf8(&bytes)
                .map_err(|e| AttemptFailure::Fatal(ProviderError::Transport(e.to_string())))?;
            let turn = parse_anthropic_response(text).map_err(AttemptFailure::Fatal)?;
            let latency_ms = attempt_start.elapsed().as_millis() as u64;
            return Ok((turn, latency_ms, Some(latency_ms)));
        }

        let mut assembler = AnthropicStreamAssembler::default();
        let mut ttft_ms: Option<u64> = None;
        let outcome = {
            let assembler_ref = &mut assembler;
            let ttft_ref = &mut ttft_ms;
            pump_sse_events(
                &mut head,
                budget,
                deadline,
                MAX_SSE_RESPONSE_BYTES,
                move |event, data| {
                    if ttft_ref.is_none() {
                        *ttft_ref = Some(attempt_start.elapsed().as_millis() as u64);
                    }
                    assembler_ref.feed(event, data)
                },
            )
            .await
        };
        let saw_business = assembler.saw_business;
        outcome.map_err(|failure| classify_stream_failure(saw_business, failure))?;

        let turn = assembler
            .finalize()
            .map_err(|e| classify_error(saw_business, e))?;
        let latency_ms = attempt_start.elapsed().as_millis() as u64;
        Ok((turn, latency_ms, ttft_ms))
    }
}

fn map_transport_failure(failure: TransportFailure) -> AttemptFailure {
    match failure {
        TransportFailure::Transient(err) => AttemptFailure::Retryable {
            err,
            retry_after: None,
        },
        TransportFailure::Fatal(err) => AttemptFailure::Fatal(err),
    }
}

fn classify_stream_failure(saw_business: bool, failure: BodyFailure) -> AttemptFailure {
    match failure {
        BodyFailure::Fatal(err) => AttemptFailure::Fatal(err),
        BodyFailure::Stream(err) => classify_error(saw_business, err),
    }
}

/// Mid-stream failures are never retried once business data has been observed (§4.3).
fn classify_error(saw_business: bool, err: ProviderError) -> AttemptFailure {
    if saw_business {
        AttemptFailure::Fatal(err)
    } else {
        AttemptFailure::Retryable {
            err,
            retry_after: None,
        }
    }
}
