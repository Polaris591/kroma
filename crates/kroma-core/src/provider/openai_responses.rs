//! Gate 7 (`v1.2.0`) — native OpenAI Responses API (`/v1/responses`) client.
//!
//! The Responses API is a different conversation shape from Chat Completions: system content moves
//! to `instructions`, history becomes a flat list of typed `input` items (`message`,
//! `function_call`, `function_call_output`, `input_image`), and the reply arrives as a list of typed
//! `output` items. Cache accounting is reported under `usage.input_tokens_details.cached_tokens`.
//!
//! HTTP framing, retry classification and SSE parsing are shared through
//! [`crate::provider::transport`].

use crate::budget::BudgetConfig;
use crate::messages::{
    AssistantTurn, FinishReason, ReasoningBlock, TokenUsage, ToolCall, TranscriptItem,
};
use crate::provider::endpoint::{validate_base_url, ValidatedEndpoint};
use crate::provider::sse::MAX_SSE_RESPONSE_BYTES;
use crate::provider::transport::{
    pump_sse_events, read_error_body, read_full_body, send_request, BodyFailure, Flow,
    HttpRequestSpec, TransportFailure,
};
use crate::provider::{
    DispatchBinding, ModelProvider, ProviderCompletion, ProviderError, ProviderRequest,
};
use crate::tool::definition::ToolDefinition;
use crate::types::ToolCallId;
use serde_json::{json, Map, Value};
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, Instant};
use tokio::sync::watch;

#[derive(Clone, Default)]
pub struct OpenAiResponsesConfig {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub max_retries: usize,
    pub reasoning_effort: Option<String>,
}

impl fmt::Debug for OpenAiResponsesConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenAiResponsesConfig")
            .field("base_url", &self.base_url)
            .field("api_key", &"Bearer ***")
            .field("model", &self.model)
            .field("max_retries", &self.max_retries)
            .field("reasoning_effort", &self.reasoning_effort)
            .finish()
    }
}

/// GPT-5.6+ cache controls are provider-specific and must never be sent to an unverified model.
/// `Off` retains the original e95845b wire format. The environment is snapshotted at client creation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum OpenAiCacheMode {
    #[default]
    Off,
    Implicit,
    Explicit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpenAiModelCapabilities {
    pub explicit_prompt_cache: bool,
    pub cache_write_usage: bool,
    /// Provider-hosted tool search needs search-item replay in the durable transcript. Until that
    /// path is available we deliberately do not advertise a working native tool-search client.
    pub hosted_tool_search_replay: bool,
}

pub fn model_capabilities(model: &str) -> OpenAiModelCapabilities {
    // Do not infer capabilities from an arbitrary proxy model name or from a family substring.
    // This conservative positive list can be extended when exact provider fixtures exist.
    let supported = ["gpt-5.6", "gpt-5.7", "gpt-6", "gpt-6.1"]
        .iter()
        .any(|prefix| model == *prefix || model.starts_with(&format!("{}-", prefix)));
    OpenAiModelCapabilities {
        explicit_prompt_cache: supported,
        cache_write_usage: supported,
        hosted_tool_search_replay: false,
    }
}

#[derive(Debug, Clone, Default)]
struct CacheWirePolicy {
    mode: OpenAiCacheMode,
    key: Option<String>,
    trace: bool,
}

impl CacheWirePolicy {
    fn from_env(model: &str) -> Result<Self, ProviderError> {
        let mode = match std::env::var("KROMA_OPENAI_CACHE_MODE")
            .unwrap_or_else(|_| "off".to_string())
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "off" | "" => OpenAiCacheMode::Off,
            "implicit" => OpenAiCacheMode::Implicit,
            "explicit" => OpenAiCacheMode::Explicit,
            _ => {
                return Err(ProviderError::Transport(
                    "KROMA_OPENAI_CACHE_MODE must be off|implicit|explicit".to_string(),
                ))
            }
        };
        if mode != OpenAiCacheMode::Off && !model_capabilities(model).explicit_prompt_cache {
            return Err(ProviderError::Transport(
                "prompt_cache_options require a verified GPT-5.6+ model identity".to_string(),
            ));
        }
        let key = std::env::var("KROMA_OPENAI_CACHE_KEY")
            .ok()
            .filter(|s| !s.is_empty());
        if let Some(ref k) = key {
            if k.len() > 128 || k.chars().any(|c| c.is_control()) {
                return Err(ProviderError::Transport(
                    "KROMA_OPENAI_CACHE_KEY must be <=128 bytes and contain no control characters"
                        .to_string(),
                ));
            }
        }
        Ok(Self {
            mode,
            key,
            trace: std::env::var("KROMA_OPENAI_CACHE_TRACE").ok().as_deref() == Some("1"),
        })
    }
}

/// Safe, non-content-bearing diagnostics. This is provider-reported telemetry, not an inferred hit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheMissDiagnostics {
    pub reason: String,
    pub missed_tokens: Option<u64>,
    pub comparison_reusable_tokens: Option<u64>,
}

pub fn parse_cache_miss_diagnostics(response: &Value) -> Option<CacheMissDiagnostics> {
    let value = response.get("prompt_cache_diagnostics")?;
    if value.get("type").and_then(Value::as_str) != Some("cache_miss") {
        return None;
    }
    let reason = match value.get("reason").and_then(Value::as_str) {
        Some("tools_changed") => "tools_changed",
        Some("model_changed") => "model_changed",
        Some("settings_changed") => "settings_changed",
        Some("input_changed") => "input_changed",
        Some("cache_expired") => "cache_expired",
        _ => "other",
    };
    Some(CacheMissDiagnostics {
        reason: reason.to_string(),
        missed_tokens: value.get("cache_missed_tokens").and_then(Value::as_u64),
        comparison_reusable_tokens: value
            .get("comparison_reusable_tokens")
            .and_then(Value::as_u64),
    })
}

fn validate_provider_cache_usage(usage: &TokenUsage) -> Result<(), ProviderError> {
    if usage
        .cached_input_tokens
        .unwrap_or(0)
        .saturating_add(usage.cache_creation_input_tokens.unwrap_or(0))
        > usage.input_tokens
    {
        return Err(ProviderError::Transport(
            "Responses usage cached + cache_write tokens exceed input_tokens".to_string(),
        ));
    }
    Ok(())
}

/// Deterministic serialization of an OpenAI Responses request body.
pub fn serialize_responses_request_body(
    model: &str,
    system_prompt: &str,
    tools: &[ToolDefinition],
    messages: &[TranscriptItem],
    stream: bool,
    reasoning_effort: Option<&str>,
) -> String {
    serialize_responses_request_body_with_limit(
        model,
        system_prompt,
        tools,
        messages,
        stream,
        reasoning_effort,
        None,
    )
}

/// F08/F09 (`v1.4.0`): serialize a Responses request, optionally emitting `max_output_tokens` and
/// always round-tripping provider-native reasoning state (`encrypted_content` -> `opaque_payload`).
pub fn serialize_responses_request_body_with_limit(
    model: &str,
    system_prompt: &str,
    tools: &[ToolDefinition],
    messages: &[TranscriptItem],
    stream: bool,
    reasoning_effort: Option<&str>,
    max_output_tokens: Option<u64>,
) -> String {
    let mut sorted_tools = tools.to_vec();
    sorted_tools.sort_by(|a, b| a.name.as_bytes().cmp(b.name.as_bytes()));

    let tools_json: Vec<Value> = sorted_tools
        .iter()
        .map(|t| {
            json!({
                "type": "function",
                "name": t.name,
                "description": t.description,
                "parameters": t.parameters,
            })
        })
        .collect();

    let mut input: Vec<Value> = Vec::with_capacity(messages.len() + 1);
    for item in messages {
        match item {
            TranscriptItem::User(u) => {
                input.push(json!({
                    "role": "user",
                    "content": [{ "type": "input_text", "text": u.content }],
                }));
            }
            TranscriptItem::Assistant(a) => {
                if let Some(text) = a.content.as_ref().filter(|t| !t.is_empty()) {
                    input.push(json!({
                        "role": "assistant",
                        "content": [{ "type": "output_text", "text": text }],
                    }));
                }
                for rb in &a.reasoning_blocks {
                    // F09 (`v1.4.0`): provider-native reasoning state MUST be replayed verbatim in
                    // the next turn. When `opaque_payload` (the Responses API `encrypted_content`)
                    // is present it is emitted even if the readable `text` summary is empty,
                    // otherwise the model loses its encrypted reasoning chain across tool calls.
                    let mut item = Map::new();
                    item.insert("type".to_string(), Value::String("reasoning".to_string()));
                    if let Some(text) = &rb.text {
                        item.insert(
                            "summary".to_string(),
                            json!([{ "type": "summary_text", "text": text }]),
                        );
                    }
                    if let Some(opaque) = &rb.opaque_payload {
                        item.insert(
                            "encrypted_content".to_string(),
                            Value::String(opaque.clone()),
                        );
                    }
                    if item.len() > 1 {
                        input.push(Value::Object(item));
                    }
                }
                for tc in &a.tool_calls {
                    input.push(json!({
                        "type": "function_call",
                        "call_id": tc.call_id.as_str(),
                        "name": tc.name,
                        "arguments": tc.raw_arguments,
                    }));
                }
            }
            TranscriptItem::ToolResult(tr) => {
                // `function_call_output.output` is a plain string, so images travel as a separate
                // user turn carrying `input_image` parts.
                let text = tr.content.as_ref();
                input.push(json!({
                    "type": "function_call_output",
                    "call_id": tr.call_id.as_str(),
                    "output": text,
                }));
                if !tr.meta.image_attachments.is_empty() {
                    let mut parts: Vec<Value> = Vec::new();
                    for image in &tr.meta.image_attachments {
                        parts.push(json!({
                            "type": "input_image",
                            "image_url": image.data_url(),
                        }));
                    }
                    input.push(json!({ "role": "user", "content": parts }));
                }
            }
        }
    }

    let mut root = Map::new();
    root.insert("model".to_string(), Value::String(model.to_string()));
    root.insert(
        "instructions".to_string(),
        Value::String(system_prompt.to_string()),
    );
    root.insert("input".to_string(), Value::Array(input));
    if !tools_json.is_empty() {
        root.insert("tools".to_string(), Value::Array(tools_json));
    }
    if let Some(effort) = reasoning_effort.map(str::trim).filter(|s| !s.is_empty()) {
        root.insert("reasoning".to_string(), json!({ "effort": effort }));
    }
    root.insert("stream".to_string(), Value::Bool(stream));
    root.insert("store".to_string(), Value::Bool(false));
    if let Some(limit) = max_output_tokens {
        root.insert("max_output_tokens".to_string(), Value::Number(limit.into()));
    }

    serde_json::to_string(&Value::Object(root)).unwrap_or_else(|_| "{}".to_string())
}

struct CacheRequestOptions<'a> {
    stream: bool,
    reasoning_effort: Option<&'a str>,
    max_output_tokens: Option<u64>,
    policy: &'a CacheWirePolicy,
}

/// Cache-aware Responses serialization. Explicit mode moves the static `instructions` into a
/// developer input block because `instructions` cannot carry an explicit breakpoint. All dynamic
/// history remains after that block, and `store=false` plus encrypted reasoning replay are kept.
fn serialize_responses_request_body_with_cache(
    model: &str,
    system_prompt: &str,
    tools: &[ToolDefinition],
    messages: &[TranscriptItem],
    options: CacheRequestOptions<'_>,
) -> String {
    let old = serialize_responses_request_body_with_limit(
        model,
        system_prompt,
        tools,
        messages,
        options.stream,
        options.reasoning_effort,
        options.max_output_tokens,
    );
    let policy = options.policy;
    if policy.mode == OpenAiCacheMode::Off && policy.key.is_none() {
        return old;
    }
    let mut root: Value = serde_json::from_str(&old).expect("serializer emits JSON");
    if let Some(key) = &policy.key {
        root["prompt_cache_key"] = json!(key);
    }
    if policy.mode != OpenAiCacheMode::Off {
        root["prompt_cache_options"] = json!({
            "mode": if policy.mode == OpenAiCacheMode::Explicit { "explicit" } else { "implicit" },
            "ttl": "30m"
        });
    }
    if policy.mode == OpenAiCacheMode::Explicit && !system_prompt.is_empty() {
        // Never place a marker on a dynamic tool output; cache only the stable developer prefix.
        root.as_object_mut().expect("object").remove("instructions");
        let mut input_text = json!({ "type": "input_text", "text": system_prompt });
        input_text["prompt_cache_breakpoint"] = json!({ "mode": "explicit" });
        root["input"].as_array_mut().expect("input array").insert(
            0,
            json!({
                "role": "developer", "content": [input_text]
            }),
        );
    }
    serde_json::to_string(&root).expect("cache-aware request serializes")
}

pub fn parse_responses_response(body: &str) -> Result<AssistantTurn, ProviderError> {
    let val: Value = serde_json::from_str(body).map_err(|e| {
        ProviderError::Transport(format!("invalid JSON in Responses API response: {}", e))
    })?;
    if let Some(err) = val.get("error") {
        return Err(ProviderError::Transport(format!(
            "Responses API error: {}",
            err
        )));
    }

    let mut text = String::new();
    let mut reasoning: Vec<ReasoningBlock> = Vec::new();
    let mut calls: Vec<ToolCall> = Vec::new();
    let output = val.get("output").and_then(Value::as_array).ok_or_else(|| {
        ProviderError::Transport("Responses API completion must carry an output array".into())
    })?;
    {
        for item in output {
            match item.get("type").and_then(Value::as_str) {
                Some("message") => {
                    if let Some(parts) = item.get("content").and_then(Value::as_array) {
                        for part in parts {
                            if let Some(t) = part.get("text").and_then(Value::as_str) {
                                if !text.is_empty() {
                                    text.push('\n');
                                }
                                text.push_str(t);
                            }
                        }
                    }
                }
                Some("reasoning") => {
                    let mut block = ReasoningBlock::default();
                    if let Some(summary) = item.get("summary").and_then(Value::as_array) {
                        let joined: Vec<&str> = summary
                            .iter()
                            .filter_map(|s| s.get("text").and_then(Value::as_str))
                            .collect();
                        if !joined.is_empty() {
                            block.text = Some(joined.join("\n"));
                        }
                    }
                    if let Some(encrypted) = item.get("encrypted_content").and_then(Value::as_str) {
                        block.opaque_payload = Some(encrypted.to_string());
                    }
                    if block.text.is_some() || block.opaque_payload.is_some() {
                        reasoning.push(block);
                    }
                }
                Some("function_call") => {
                    calls.push(ToolCall {
                        call_id: ToolCallId::new(
                            item.get("call_id")
                                .or_else(|| item.get("id"))
                                .and_then(Value::as_str)
                                .unwrap_or(""),
                        ),
                        name: item
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                        raw_arguments: item
                            .get("arguments")
                            .and_then(Value::as_str)
                            .unwrap_or("{}")
                            .to_string(),
                    });
                }
                Some("tool_search_call" | "tool_search_output" | "additional_tools") => {
                    return Err(ProviderError::UnsupportedSemantic(
                        "provider-native tool search items require durable catalog/replay support"
                            .to_string(),
                    ));
                }
                // Unrecognized provider-executed tool items cannot be safely omitted: they may
                // change what the model can call or contain an external side effect.
                Some(kind) if kind.ends_with("_call") || kind.ends_with("_output") => {
                    return Err(ProviderError::UnsupportedSemantic(format!(
                        "unhandled Responses output item type {kind}"
                    )));
                }
                _ => {}
            }
        }
    }

    let status = val
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("completed");
    let finish_reason = match status {
        "completed" if !calls.is_empty() => FinishReason::ToolCalls,
        "completed" => FinishReason::Stop,
        "incomplete" => FinishReason::Length,
        // A failed or cancelled response carries no usable turn: surfacing it as a transport error
        // is strictly better than reporting a half-empty `Stop`/`ToolCalls` turn as success.
        "failed" | "cancelled" => {
            return Err(ProviderError::Transport(format!(
                "Responses API returned status `{}`",
                status
            )))
        }
        other => FinishReason::Unknown(other.to_string()),
    };

    if let Some(usage) = val.get("usage") {
        validate_provider_cache_usage(&parse_responses_usage(usage))?;
    }
    Ok(AssistantTurn {
        content: if text.is_empty() { None } else { Some(text) },
        reasoning_blocks: reasoning,
        tool_calls: calls,
        finish_reason,
        usage: val.get("usage").map(parse_responses_usage),
    })
}

fn parse_responses_usage(usage: &Value) -> TokenUsage {
    TokenUsage {
        input_tokens: usage
            .get("input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        cached_input_tokens: usage
            .get("input_tokens_details")
            .and_then(|d| d.get("cached_tokens"))
            .and_then(Value::as_u64),
        // GPT-5.6+ Responses reports cache writes separately. Older model responses may omit
        // the field; preserve that distinction rather than guessing a zero write count.
        cache_creation_input_tokens: usage
            .get("input_tokens_details")
            .and_then(|d| d.get("cache_write_tokens"))
            .and_then(Value::as_u64),
        output_tokens: usage
            .get("output_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        reasoning_tokens: usage
            .get("output_tokens_details")
            .and_then(|d| d.get("reasoning_tokens"))
            .and_then(Value::as_u64),
    }
}

/// Until the durable transcript can replay every search/load result, reject these native
/// output items in both JSON and SSE paths. Merely ignoring them loses catalog and approval
/// state and can make a resumed conversation non-equivalent to the original one.
fn reject_unsupported_native_output(item: &Value) -> Result<(), ProviderError> {
    match item.get("type").and_then(Value::as_str) {
        Some("tool_search_call" | "tool_search_output" | "additional_tools") => {
            Err(ProviderError::UnsupportedSemantic(
                "provider-native tool search requires durable catalog/replay support".into(),
            ))
        }
        Some(kind) if kind.ends_with("_call") && kind != "function_call" => {
            Err(ProviderError::UnsupportedSemantic(format!(
                "unhandled Responses output item type {kind}"
            )))
        }
        Some(kind) if kind.ends_with("_output") => Err(ProviderError::UnsupportedSemantic(
            format!("unhandled Responses output item type {kind}"),
        )),
        _ => Ok(()),
    }
}

#[derive(Default)]
struct ResponsesStreamAssembler {
    text: String,
    reasoning: Vec<ReasoningBlock>,
    calls: Vec<ToolCall>,
    /// `output_index` → in-flight `function_call` (call_id, name, arguments).
    pending_calls: std::collections::HashMap<usize, (String, String, String)>,
    usage: Option<TokenUsage>,
    reported_model: Option<String>,
    status: Option<String>,
    cache_miss: Option<CacheMissDiagnostics>,
    saw_business: bool,
    /// Set only on `response.completed` / `response.incomplete`: the explicit end-of-stream signal.
    saw_terminal_event: bool,
}

impl ResponsesStreamAssembler {
    fn feed(&mut self, event: &str, data: &str) -> Result<(), ProviderError> {
        if data.trim().is_empty() || data.trim() == "[DONE]" {
            return Ok(());
        }
        let val: Value = serde_json::from_str(data).map_err(|e| {
            ProviderError::IncompleteStream(format!("invalid Responses SSE payload: {}", e))
        })?;
        if let Some(response) = val.get("response") {
            super::capture_reported_model(&mut self.reported_model, response)?;
            if let Some(diagnostic) = parse_cache_miss_diagnostics(response) {
                self.cache_miss = Some(diagnostic);
            }
        }
        match event {
            "response.created" | "response.in_progress" => {
                self.saw_business = true;
            }
            "response.output_item.added" => {
                let index = val.get("output_index").and_then(Value::as_u64).unwrap_or(0) as usize;
                if let Some(item) = val.get("item") {
                    reject_unsupported_native_output(item)?;
                    if item.get("type").and_then(Value::as_str) == Some("function_call") {
                        self.pending_calls.insert(
                            index,
                            (
                                item.get("call_id")
                                    .or_else(|| item.get("id"))
                                    .and_then(Value::as_str)
                                    .unwrap_or("")
                                    .to_string(),
                                item.get("name")
                                    .and_then(Value::as_str)
                                    .unwrap_or("")
                                    .to_string(),
                                item.get("arguments")
                                    .and_then(Value::as_str)
                                    .unwrap_or("")
                                    .to_string(),
                            ),
                        );
                    }
                }
                self.saw_business = true;
            }
            "response.output_text.delta" => {
                if let Some(delta) = val.get("delta").and_then(Value::as_str) {
                    self.text.push_str(delta);
                }
                self.saw_business = true;
            }
            "response.reasoning_summary_text.delta" => {
                if let Some(delta) = val.get("delta").and_then(Value::as_str) {
                    if let Some(last) = self.reasoning.last_mut() {
                        last.text.get_or_insert_with(String::new).push_str(delta);
                    } else {
                        self.reasoning.push(ReasoningBlock {
                            text: Some(delta.to_string()),
                            signature: None,
                            opaque_payload: None,
                        });
                    }
                }
                self.saw_business = true;
            }
            "response.function_call_arguments.delta" => {
                let index = val.get("output_index").and_then(Value::as_u64).unwrap_or(0) as usize;
                if let Some(delta) = val.get("delta").and_then(Value::as_str) {
                    if let Some(entry) = self.pending_calls.get_mut(&index) {
                        entry.2.push_str(delta);
                    }
                }
                self.saw_business = true;
            }
            "response.function_call_arguments.done" => {
                let index = val.get("output_index").and_then(Value::as_u64).unwrap_or(0) as usize;
                if let Some((call_id, name, args)) = self.pending_calls.remove(&index) {
                    self.calls.push(ToolCall {
                        call_id: ToolCallId::new(call_id),
                        name,
                        raw_arguments: if args.trim().is_empty() {
                            "{}".to_string()
                        } else {
                            args
                        },
                    });
                }
            }
            "response.output_item.done" => {
                let index = val.get("output_index").and_then(Value::as_u64).unwrap_or(0) as usize;
                if let Some((call_id, name, args)) = self.pending_calls.remove(&index) {
                    self.calls.push(ToolCall {
                        call_id: ToolCallId::new(call_id),
                        name,
                        raw_arguments: if args.trim().is_empty() {
                            "{}".to_string()
                        } else {
                            args
                        },
                    });
                }
                // F09 (`v1.4.0`): a completed `reasoning` item carries the provider-native
                // `encrypted_content` that must be replayed on the next turn. Capture it into the
                // matching `ReasoningBlock` (or a fresh one when only the opaque payload is known).
                if let Some(item) = val.get("item") {
                    reject_unsupported_native_output(item)?;
                    if item.get("type").and_then(Value::as_str) == Some("reasoning") {
                        if let Some(encrypted) =
                            item.get("encrypted_content").and_then(Value::as_str)
                        {
                            if !encrypted.is_empty() {
                                if let Some(block) = self.reasoning.last_mut() {
                                    if block.opaque_payload.is_none() {
                                        block.opaque_payload = Some(encrypted.to_string());
                                    }
                                } else {
                                    self.reasoning.push(ReasoningBlock {
                                        text: None,
                                        signature: None,
                                        opaque_payload: Some(encrypted.to_string()),
                                    });
                                }
                            }
                        }
                    }
                }
            }
            "response.completed" => {
                if let Some(resp) = val.get("response") {
                    if let Some(items) = resp.get("output").and_then(Value::as_array) {
                        for item in items {
                            reject_unsupported_native_output(item)?;
                        }
                    }
                    self.status = resp
                        .get("status")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    if let Some(usage) = resp.get("usage") {
                        let parsed = parse_responses_usage(usage);
                        validate_provider_cache_usage(&parsed)?;
                        self.usage = Some(parsed);
                    }
                }
                self.saw_terminal_event = true;
                self.saw_business = true;
            }
            "response.incomplete" => {
                self.status = Some("incomplete".to_string());
                self.saw_terminal_event = true;
                self.saw_business = true;
            }
            "response.failed" | "error" => {
                return Err(ProviderError::IncompleteStream(format!(
                    "Responses API stream failure: {}",
                    val
                )));
            }
            _ => {}
        }
        Ok(())
    }

    fn finalize(self) -> Result<AssistantTurn, ProviderError> {
        if !self.saw_business {
            return Err(ProviderError::IncompleteStream(
                "Responses SSE stream contained no events".to_string(),
            ));
        }
        // A stream that ends without `response.completed`/`response.incomplete`, or with a
        // `function_call` whose arguments never finished, is truncated. Promoting those in-flight
        // calls with `{}` would hand the model a tool invocation it never asked for and silently
        // execute it, so the turn is rejected instead (§4.3).
        if !self.saw_terminal_event {
            return Err(ProviderError::IncompleteStream(
                "Responses SSE stream ended before response.completed".to_string(),
            ));
        }
        if !self.pending_calls.is_empty() {
            return Err(ProviderError::IncompleteStream(format!(
                "Responses SSE stream ended with {} unclosed function_call item(s)",
                self.pending_calls.len()
            )));
        }

        let finish_reason = if self.status.as_deref() == Some("incomplete") {
            FinishReason::Length
        } else if !self.calls.is_empty() {
            FinishReason::ToolCalls
        } else {
            FinishReason::Stop
        };

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

pub struct OpenAiResponsesClient {
    channel: std::sync::Arc<crate::channel_governor::ChannelGovernor>,
    config: OpenAiResponsesConfig,
    endpoint: ValidatedEndpoint,
    cache_wire_policy: CacheWirePolicy,
}

impl OpenAiResponsesClient {
    pub fn new(config: OpenAiResponsesConfig) -> Result<Self, ProviderError> {
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
        let cache_wire_policy = CacheWirePolicy::from_env(&config.model)?;
        let channel = super::channel::shared_channel(&endpoint)?;
        Ok(Self {
            config,
            endpoint,
            channel,
            cache_wire_policy,
        })
    }

    pub fn endpoint(&self) -> &ValidatedEndpoint {
        &self.endpoint
    }

    fn responses_path(&self) -> String {
        let base = &self.endpoint.base_path;
        if base.ends_with("/responses") {
            base.clone()
        } else if base.ends_with("/v1") {
            format!("{}/responses", base)
        } else {
            format!("{}/v1/responses", base)
        }
    }
}

impl ModelProvider for OpenAiResponsesClient {
    /// The exact destination and model interpolated into the `/v1/responses` request below.
    fn dispatch_binding(&self) -> Option<DispatchBinding> {
        Some(DispatchBinding {
            endpoint: self.endpoint.authority(),
            model: self.config.model.clone(),
        })
    }

    /// C00-06: what this wire format can express. Undeclared semantics are refused, not dropped.
    fn message_semantics(&self) -> crate::provider::MessageSemantics {
        crate::provider::MessageSemantics::default()
    }

    fn complete<'a>(
        &'a self,
        req: ProviderRequest,
        budget: &'a BudgetConfig,
        mut cancel_rx: watch::Receiver<bool>,
    ) -> Pin<Box<dyn Future<Output = Result<ProviderCompletion, ProviderError>> + Send + 'a>> {
        Box::pin(async move {
            crate::provider::check_semantics(&req, self.message_semantics())?;
            let body_json = serialize_responses_request_body_with_cache(
                &self.config.model,
                &req.system_prompt,
                &req.tools,
                &req.messages,
                CacheRequestOptions {
                    stream: req.stream,
                    reasoning_effort: self.config.reasoning_effort.as_deref(),
                    max_output_tokens: req.max_output_tokens,
                    policy: &self.cache_wire_policy,
                },
            );
            let request_body_bytes = body_json.len();
            let max_attempts = self
                .config
                .max_retries
                .saturating_add(1)
                .min(budget.max_model_request_attempts)
                .min(self.channel.policy().max_attempts);
            if max_attempts == 0 {
                return Err(ProviderError::NotDispatched(Box::new(
                    ProviderError::MaxAttemptsExhausted,
                )));
            }
            let mut attempts_used = 0usize;

            loop {
                let attempt_start = Instant::now();
                let channel_permit = super::channel::admit(
                    &self.channel,
                    &mut cancel_rx,
                    budget.request_timeout,
                    attempts_used,
                )
                .await?;
                let retry_reservation = super::reserve_retry(&req, attempts_used)?;
                attempts_used += 1;
                if let Some(wire) = &req.wire_attempts {
                    wire.record();
                }
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

                match &outcome {
                    Ok(_) => super::channel::record_outcome(&self.channel, None),
                    Err(
                        AttemptFailure::Fatal(error) | AttemptFailure::Retryable { err: error, .. },
                    ) => {
                        super::channel::record_outcome(&self.channel, Some(error));
                    }
                }
                drop(channel_permit); // Covers the complete response body, not just HTTP headers.
                match outcome {
                    Ok((turn, latency_ms, ttft_ms, reported_model)) => {
                        super::settle_retry(retry_reservation, &turn);
                        return Ok(ProviderCompletion {
                            turn,
                            request_body_bytes,
                            latency_ms,
                            ttft_ms,
                            attempts_used,
                            reported_model,
                        });
                    }
                    Err(AttemptFailure::Fatal(err)) => return Err(err),
                    Err(AttemptFailure::Retryable { err, retry_after }) => {
                        if attempts_used >= max_attempts {
                            return Err(err);
                        }
                        super::channel::backoff(
                            &self.channel,
                            attempts_used,
                            retry_after,
                            budget.request_timeout,
                            &mut cancel_rx,
                        )
                        .await?;
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

impl OpenAiResponsesClient {
    fn log_cache_telemetry(&self, turn: &AssistantTurn, miss: Option<CacheMissDiagnostics>) {
        if let Some(usage) = &turn.usage {
            eprintln!(
                "kroma_cache input={} cached={} written={} write_known={} miss_reason={} missed_tokens={}",
                usage.input_tokens,
                usage.cached_input_tokens.unwrap_or(0),
                usage.cache_creation_input_tokens.unwrap_or(0),
                usage.cache_creation_input_tokens.is_some(),
                miss.as_ref().map(|d| d.reason.as_str()).unwrap_or("none"),
                miss.as_ref().and_then(|d| d.missed_tokens).unwrap_or(0),
            );
        }
    }

    async fn attempt(
        &self,
        body_json: &str,
        stream: bool,
        budget: &BudgetConfig,
        attempt_start: Instant,
    ) -> Result<(AssistantTurn, u64, Option<u64>, Option<String>), AttemptFailure> {
        let deadline = attempt_start + budget.request_timeout;
        let path = self.responses_path();
        let headers = vec![(
            "Authorization".to_string(),
            format!("Bearer {}", self.config.api_key),
        )];
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
        if matches!(head.status, 429 | 502 | 503 | 504) {
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
            let turn = parse_responses_response(text).map_err(AttemptFailure::Fatal)?;
            if self.cache_wire_policy.trace {
                if let Ok(value) = serde_json::from_str::<Value>(text) {
                    self.log_cache_telemetry(&turn, parse_cache_miss_diagnostics(&value));
                }
            }
            let latency_ms = attempt_start.elapsed().as_millis() as u64;
            let reported_model = super::model_from_json(text).map_err(AttemptFailure::Fatal)?;
            return Ok((turn, latency_ms, Some(latency_ms), reported_model));
        }

        let mut assembler = ResponsesStreamAssembler::default();
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
                    assembler_ref.feed(event, data).map(|()| Flow::Continue)
                },
            )
            .await
        };
        let saw_business = assembler.saw_business;
        outcome.map_err(|failure| classify_stream_failure(saw_business, failure))?;

        let reported_model = assembler.reported_model.clone();
        let cache_miss = assembler.cache_miss.clone();
        let turn = assembler
            .finalize()
            .map_err(|e| classify_error(saw_business, e))?;
        if self.cache_wire_policy.trace {
            self.log_cache_telemetry(&turn, cache_miss);
        }
        let latency_ms = attempt_start.elapsed().as_millis() as u64;
        Ok((turn, latency_ms, ttft_ms, reported_model))
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

#[cfg(test)]
mod cache_write_accounting_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn gpt_5_6_usage_parses_cache_writes_separately_from_reads() {
        let usage = json!({
            "input_tokens": 1000,
            "input_tokens_details": {"cached_tokens": 600, "cache_write_tokens": 300},
            "output_tokens": 40,
            "output_tokens_details": {"reasoning_tokens": 10}
        });
        let parsed = parse_responses_usage(&usage);
        assert_eq!(parsed.input_tokens, 1000);
        assert_eq!(parsed.cached_input_tokens, Some(600));
        assert_eq!(parsed.cache_creation_input_tokens, Some(300));
        assert_eq!(parsed.uncached_input_tokens(), 100);
        assert_eq!(parsed.output_tokens, 40);
        assert_eq!(parsed.reasoning_tokens, Some(10));
    }

    #[test]
    fn old_responses_usage_preserves_unknown_cache_write_metadata() {
        let old = parse_responses_usage(&json!({
            "input_tokens": 90,
            "input_tokens_details": {"cached_tokens": 20},
            "output_tokens": 4
        }));
        assert_eq!(old.cached_input_tokens, Some(20));
        assert_eq!(old.cache_creation_input_tokens, None);
        let new_zero = parse_responses_usage(&json!({
            "input_tokens": 90,
            "input_tokens_details": {"cached_tokens": 20, "cache_write_tokens": 0},
            "output_tokens": 4
        }));
        assert_eq!(new_zero.cache_creation_input_tokens, Some(0));
    }

    #[test]
    fn complete_response_exposes_write_usage_to_cost_accounting() {
        let body = json!({
            "id": "resp_fixture",
            "status": "completed",
            "output": [{"type":"message","role":"assistant","content":[{"type":"output_text","text":"ok"}]}],
            "usage": {"input_tokens": 1000,
                      "input_tokens_details": {"cached_tokens": 600, "cache_write_tokens": 300},
                      "output_tokens": 10}
        });
        let response = parse_responses_response(&body.to_string()).unwrap();
        let usage = response.usage.unwrap();
        assert_eq!(usage.cache_creation_input_tokens, Some(300));
    }
}

#[cfg(test)]
mod nextgen_cache_wire_tests {
    use super::*;

    fn body(model: &str, mode: OpenAiCacheMode) -> Value {
        serde_json::from_str(&serialize_responses_request_body_with_cache(
            model,
            "stable\nconstraints",
            &[],
            &[],
            CacheRequestOptions {
                stream: false,
                reasoning_effort: None,
                max_output_tokens: None,
                policy: &CacheWirePolicy {
                    mode,
                    key: Some("tenant-workspace-1".into()),
                    trace: false,
                },
            },
        ))
        .unwrap()
    }

    #[test]
    fn explicit_only_marks_developer_input_block_without_dropping_store_false() {
        let val = body("gpt-6", OpenAiCacheMode::Explicit);
        assert!(val.get("instructions").is_none());
        assert_eq!(val["input"][0]["role"], "developer");
        assert_eq!(val["input"][0]["content"][0]["text"], "stable\nconstraints");
        assert_eq!(
            val["input"][0]["content"][0]["prompt_cache_breakpoint"]["mode"],
            "explicit"
        );
        assert_eq!(val["prompt_cache_options"]["mode"], "explicit");
        assert_eq!(val["prompt_cache_options"]["ttl"], "30m");
        assert_eq!(val["prompt_cache_key"], "tenant-workspace-1");
        assert_eq!(val["store"], false);
    }

    #[test]
    fn implicit_keeps_original_instructions_and_skips_explicit_markers() {
        let val = body("gpt-5.6", OpenAiCacheMode::Implicit);
        assert_eq!(val["instructions"], "stable\nconstraints");
        assert!(val["input"].as_array().unwrap().is_empty());
        assert_eq!(val["prompt_cache_options"]["mode"], "implicit");
    }

    #[test]
    fn off_is_byte_identical_to_baseline_serializer() {
        let old = serialize_responses_request_body_with_limit(
            "gpt-5.5",
            "s",
            &[],
            &[],
            false,
            None,
            None,
        );
        let new = serialize_responses_request_body_with_cache(
            "gpt-5.5",
            "s",
            &[],
            &[],
            CacheRequestOptions {
                stream: false,
                reasoning_effort: None,
                max_output_tokens: None,
                policy: &CacheWirePolicy::default(),
            },
        );
        assert_eq!(old, new);
        assert!(!model_capabilities("my-gpt-6-proxy").explicit_prompt_cache);
        assert!(!model_capabilities("gpt-5.5").explicit_prompt_cache);
        assert!(model_capabilities("gpt-6.1-sol").cache_write_usage);
    }

    #[test]
    fn bad_usage_is_rejected_not_priced_as_free() {
        let raw = json!({
            "status": "completed", "output": [],
            "usage": { "input_tokens": 5,
                "input_tokens_details": { "cached_tokens": 4, "cache_write_tokens": 4},
                "output_tokens": 1 }
        });
        assert!(parse_responses_response(&raw.to_string()).is_err());
    }

    #[test]
    fn diagnostics_reason_is_allowlisted_and_counts_are_exact() {
        let val = json!({"prompt_cache_diagnostics": {
            "type":"cache_miss", "reason":"secret-path=/users/private",
            "cache_missed_tokens": 512, "comparison_reusable_tokens": 768 }});
        let diag = parse_cache_miss_diagnostics(&val).unwrap();
        assert_eq!(diag.reason, "other");
        assert_eq!(diag.missed_tokens, Some(512));
    }
}

#[cfg(test)]
mod native_tool_search_replay_guard_tests {
    use super::*;

    #[test]
    fn json_tool_search_items_cannot_disappear_from_replayed_transcript() {
        for kind in [
            "tool_search_call",
            "tool_search_output",
            "additional_tools",
            "mcp_call",
        ] {
            let value = json!({"status":"completed", "output":[{"type":kind,"call_id":"call-x"}]});
            let err = parse_responses_response(&value.to_string()).unwrap_err();
            assert!(
                matches!(err, ProviderError::UnsupportedSemantic(_)),
                "{kind}: {err}"
            );
        }
    }

    #[test]
    fn sse_search_items_cannot_disappear_from_replayed_transcript() {
        for kind in ["tool_search_call", "tool_search_output", "mcp_call"] {
            let mut assembler = ResponsesStreamAssembler::default();
            let event = json!({"output_index":0,"item":{"type":kind,"call_id":"x"}});
            let err = assembler
                .feed("response.output_item.added", &event.to_string())
                .unwrap_err();
            assert!(
                matches!(err, ProviderError::UnsupportedSemantic(_)),
                "{kind}: {err}"
            );
        }
    }
}

#[cfg(test)]
mod malformed_response_shape_tests {
    use super::*;
    #[test]
    fn missing_or_wrong_output_is_not_an_empty_success() {
        for body in [
            r#"{"status":"completed"}"#,
            r#"{"status":"completed","output":{}}"#,
        ] {
            assert!(parse_responses_response(body).is_err());
        }
        assert!(parse_responses_response(r#"{"status":"completed","output":[]}"#).is_ok());
    }
}
