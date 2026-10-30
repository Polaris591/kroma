pub use crate::model::{Completion, SummaryResult, Usage};
mod anthropic;
mod responses;
use crate::model::{ModelBackend, ModelFuture, StreamFuture, TaskImageCache};
use anyhow::{bail, Context, Result};
use futures_util::StreamExt;
use reqwest::{header::RETRY_AFTER, Client, StatusCode};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, HashSet},
    fmt,
    sync::atomic::{AtomicBool, Ordering},
    sync::{Arc, RwLock},
    time::{Duration, Instant},
};

const MAX_RETRY_AFTER: Duration = Duration::from_secs(10);
const MAX_SSE_PENDING_BYTES: usize = 4 * 1024 * 1024;
const MAX_SSE_EVENT_BYTES: usize = 2 * 1024 * 1024;
const MAX_STREAM_BYTES: usize = 64 * 1024 * 1024;
const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;
const MAX_MODEL_METADATA_BYTES: usize = 256 * 1024;
const MAX_APEC_RESPONSE_BYTES: usize = 1024 * 1024;
/// A per-turn output budget, not the model's total capability. Reserving a
/// 128k completion for every request would compress the conversation far too
/// early; 32k is still enough for a large edit or a long explanation.
const MAX_TURN_OUTPUT_TOKENS: usize = 32 * 1024;

async fn read_limited_text(response: reqwest::Response, limit: usize) -> Result<String> {
    let mut stream = response.bytes_stream();
    let mut body = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("model response body failed")?;
        if body.len().saturating_add(chunk.len()) > limit {
            bail!("model response body exceeds the configured limit");
        }
        body.extend_from_slice(&chunk);
    }
    Ok(String::from_utf8_lossy(&body).into_owned())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ApiProtocol {
    Chat,
    Responses,
    Anthropic,
}

#[derive(Debug, Clone, Default)]
pub struct ModelInfo {
    pub context_window: Option<usize>,
    pub max_output_tokens: Option<usize>,
    /// Raw capability object from the upstream endpoint, not a model-name guess.
    pub capabilities: Option<Value>,
    pub image_input: Option<bool>,
}

impl ModelInfo {
    fn parse(raw: &Value) -> Self {
        let get = |paths: &[&str], min: usize| {
            paths.iter().find_map(|path| {
                raw.pointer(path)
                    .and_then(Value::as_u64)
                    .and_then(|n| usize::try_from(n).ok())
                    .filter(|n| *n >= min && *n <= 2_000_000)
            })
        };
        Self {
            context_window: get(
                &[
                    "/max_input_tokens",
                    "/context_window",
                    "/context_length",
                    "/input_token_limit",
                    "/max_context_tokens",
                    "/metadata/context_length",
                    "/limits/context_window",
                    "/limits/max_input_tokens",
                ],
                1024,
            ),
            max_output_tokens: get(
                &[
                    "/max_tokens",
                    "/max_output_tokens",
                    "/output_token_limit",
                    "/metadata/max_output_tokens",
                    "/limits/max_output_tokens",
                ],
                1,
            ),
            capabilities: raw.get("capabilities").cloned(),
            image_input: [
                "/input_modalities",
                "/architecture/input_modalities",
                "/supported_modalities",
            ]
            .iter()
            .find_map(|path| raw.pointer(path).and_then(Value::as_array))
            .map(|items| items.iter().any(|item| item.as_str() == Some("image")))
            .or_else(|| raw.get("supports_vision").and_then(Value::as_bool))
            .or_else(|| raw.get("vision").and_then(Value::as_bool)),
        }
    }

    fn supports(&self, path: &str) -> Option<bool> {
        self.capabilities
            .as_ref()?
            .pointer(path)?
            .get("supported")?
            .as_bool()
    }
}

// A small typed error preserves the HTTP status and Retry-After without
// parsing prose. Only transient statuses are considered for retries.
#[derive(Debug)]
struct ModelHttpError {
    status: StatusCode,
    retry_after: Option<Duration>,
    message: String,
}

pub fn is_context_overflow(error: &anyhow::Error) -> bool {
    if error
        .chain()
        .any(|source| source.is::<crate::model::ContextOverflow>())
    {
        return true;
    }
    error
        .chain()
        .find_map(|source| source.downcast_ref::<ModelHttpError>())
        .is_some_and(|http| {
            if !matches!(
                http.status,
                StatusCode::BAD_REQUEST
                    | StatusCode::PAYLOAD_TOO_LARGE
                    | StatusCode::UNPROCESSABLE_ENTITY
            ) {
                return false;
            }
            let message = http.message.to_ascii_lowercase();
            [
                "context_length_exceeded",
                "context length exceeded",
                "maximum context length",
                "prompt is too long",
                "input exceeds the context window",
            ]
            .iter()
            .any(|marker| message.contains(marker))
        })
}

impl fmt::Display for ModelHttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "model returned HTTP {}: {}", self.status, self.message)
    }
}

impl std::error::Error for ModelHttpError {}

fn image_capability_rejection(error: &anyhow::Error) -> bool {
    let Some(http) = error.downcast_ref::<ModelHttpError>() else {
        return false;
    };
    if http.status == StatusCode::UNSUPPORTED_MEDIA_TYPE {
        return true;
    }
    if http.status != StatusCode::BAD_REQUEST && http.status != StatusCode::UNPROCESSABLE_ENTITY {
        return false;
    }
    let detail = http.message.to_ascii_lowercase();
    let mentions_image = ["image", "vision", "multimodal", "input_image"]
        .iter()
        .any(|part| detail.contains(part));
    let is_unsupported = [
        "not supported",
        "unsupported",
        "does not support",
        "doesn't support",
        "text-only",
        "text only",
        "cannot accept",
        "not allowed",
    ]
    .iter()
    .any(|part| detail.contains(part));
    mentions_image && is_unsupported
}

fn retry_delay(error: &anyhow::Error, retry_index: u32) -> Option<Duration> {
    if is_timeout(error)
        || error.chain().any(|cause| {
            cause
                .downcast_ref::<reqwest::Error>()
                .is_some_and(reqwest::Error::is_connect)
        })
    {
        return Some(Duration::from_millis(250 * u64::from(retry_index)));
    }
    let http = error
        .chain()
        .find_map(|cause| cause.downcast_ref::<ModelHttpError>())?;
    if !matches!(
        http.status,
        StatusCode::TOO_MANY_REQUESTS
            | StatusCode::BAD_GATEWAY
            | StatusCode::SERVICE_UNAVAILABLE
            | StatusCode::GATEWAY_TIMEOUT
    ) {
        return None;
    }
    let delay = http
        .retry_after
        .unwrap_or(Duration::from_millis(250 * u64::from(retry_index)));
    (delay <= MAX_RETRY_AFTER).then_some(delay)
}

fn parse_retry_after(response: &reqwest::Response) -> Option<Duration> {
    // The common Retry-After seconds form is supported. An unsupported
    // HTTP-date form must not be retried earlier than the server requested.
    response
        .headers()
        .get(RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
}

pub use crate::protocol::{FunctionCall, Message, ToolCall};

// Opaque reasoning is bound to the original model/protocol/endpoint. Legacy
// untagged records are safe only within the current tool-use turn, where the
// Agent cannot switch models. Older untagged turns remain in the raw WAL but
// must not leak an unverifiable signature into a different model's request.
fn replay_opaque_reasoning(msg: &Message, index: usize, active_turn: usize, origin: &str) -> bool {
    let stored = msg
        .provider_items
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .find(|item| item["type"] == "kroma_opaque_reasoning_origin")
        .and_then(|item| item["id"].as_str());
    stored.map_or(index >= active_turn, |id| id == origin)
}

#[derive(Deserialize)]
struct StreamChunk {
    #[serde(default)]
    choices: Vec<StreamChoice>,
    usage: Option<Value>,
    error: Option<Value>,
}

#[derive(Deserialize)]
struct StreamChoice {
    delta: StreamDelta,
    finish_reason: Option<String>,
}

#[derive(Default, Deserialize)]
struct StreamDelta {
    content: Option<String>,
    reasoning_content: Option<String>,
    tool_calls: Option<Vec<ToolCallDelta>>,
}

#[derive(Deserialize)]
struct ToolCallDelta {
    index: usize,
    id: Option<String>,
    #[serde(rename = "type")]
    kind: Option<String>,
    function: Option<FunctionDelta>,
}

#[derive(Deserialize)]
struct FunctionDelta {
    name: Option<String>,
    arguments: Option<String>,
}

#[derive(Default)]
struct PartialToolCall {
    id: String,
    kind: String,
    name: String,
    arguments: String,
}

#[derive(Default)]
struct StreamResult {
    content: String,
    reasoning_content: Option<String>,
    calls: BTreeMap<usize, PartialToolCall>,
    finish_reason: Option<String>,
    done: bool,
    received_delta: bool,
    usage: Option<Usage>,
}

impl Usage {
    fn from_value(value: &Value) -> Option<Self> {
        let num = |key: &str| value.get(key).and_then(Value::as_u64);
        let cached = value
            .pointer("/prompt_tokens_details/cached_tokens")
            .or_else(|| value.get("prompt_cache_hit_tokens"))
            .or_else(|| value.pointer("/input_tokens_details/cached_tokens"))
            .and_then(Value::as_u64);
        let usage = Self {
            input: num("prompt_tokens").or_else(|| num("input_tokens")),
            output: num("completion_tokens").or_else(|| num("output_tokens")),
            total: num("total_tokens"),
            cached,
            reasoning: value
                .pointer("/completion_tokens_details/reasoning_tokens")
                .or_else(|| value.pointer("/output_tokens_details/reasoning_tokens"))
                .or_else(|| value.get("reasoning_tokens"))
                .and_then(Value::as_u64),
        };
        if usage.reasoning.is_some()
            || usage.input.is_some()
            || usage.output.is_some()
            || usage.total.is_some()
            || usage.cached.is_some()
        {
            Some(usage)
        } else {
            None
        }
    }
}

impl StreamResult {
    #[cfg(test)]
    fn apply(&mut self, data: &str, on_text: &mut (impl FnMut(&str) + Send)) -> Result<()> {
        self.apply_with_reasoning(data, on_text, &mut |_| {})
    }

    fn apply_with_reasoning(
        &mut self,
        data: &str,
        on_text: &mut (impl FnMut(&str) + Send),
        on_reasoning: &mut (impl FnMut(&str) + Send),
    ) -> Result<()> {
        if data == "[DONE]" {
            self.done = true;
            return Ok(());
        }
        let chunk: StreamChunk =
            serde_json::from_str(data).context("invalid streaming model event")?;
        if let Some(error) = chunk.error {
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            bail!("upstream streaming error: {message}");
        }
        if let Some(usage) = chunk.usage.as_ref().and_then(Usage::from_value) {
            self.usage = Some(usage);
        }
        if let Some(choice) = chunk.choices.into_iter().next() {
            if let Some(reasoning) = choice.delta.reasoning_content {
                if !reasoning.is_empty() {
                    self.received_delta = true;
                    on_reasoning(&reasoning);
                }
                self.reasoning_content
                    .get_or_insert_with(String::new)
                    .push_str(&reasoning);
            }
            if let Some(content) = choice.delta.content {
                if !content.is_empty() {
                    self.received_delta = true;
                }
                self.content.push_str(&content);
                if !content.is_empty() {
                    on_text(&content);
                }
            }
            for delta in choice.delta.tool_calls.unwrap_or_default() {
                self.received_delta = true;
                let call = self.calls.entry(delta.index).or_default();
                if let Some(id) = delta.id {
                    call.id.push_str(&id);
                }
                if let Some(kind) = delta.kind {
                    call.kind.push_str(&kind);
                }
                if let Some(function) = delta.function {
                    if let Some(name) = function.name {
                        call.name.push_str(&name);
                    }
                    if let Some(args) = function.arguments {
                        call.arguments.push_str(&args);
                    }
                }
            }
            if let Some(reason) = choice.finish_reason {
                self.finish_reason = Some(reason);
            }
        }
        Ok(())
    }

    fn finish(self) -> Result<(Message, Option<Usage>)> {
        match self.finish_reason.as_deref() {
            Some("stop" | "tool_calls") => {}
            Some("length") => bail!("model response was truncated (finish_reason=length)"),
            Some("content_filter") => bail!("model response was blocked (content_filter)"),
            Some(reason) => bail!("unrecognized model finish_reason: {reason}"),
            None => bail!("model stream ended without a successful finish_reason"),
        }
        let calls = self
            .calls
            .into_values()
            .map(|part| {
                if part.id.is_empty() || part.name.is_empty() || part.kind != "function" {
                    bail!("incomplete tool call in model stream");
                }
                let arguments: Value = serde_json::from_str(&part.arguments)
                    .with_context(|| format!("invalid JSON arguments in tool call {}", part.id))?;
                if !arguments.is_object() {
                    bail!("tool call {} arguments must be a JSON object", part.id);
                }
                Ok(ToolCall {
                    id: part.id,
                    kind: part.kind,
                    function: FunctionCall {
                        name: part.name,
                        arguments: part.arguments,
                    },
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let mut seen = HashSet::new();
        if calls.iter().any(|call| !seen.insert(call.id.as_str())) {
            bail!("model returned duplicate tool_call_id values");
        }
        if calls.is_empty() && self.content.trim().is_empty() {
            bail!("model returned neither text nor a tool call");
        }
        let message = Message {
            role: "assistant".to_string(),
            content: if self.content.is_empty() && calls.is_empty() {
                None
            } else {
                Some(self.content)
            },
            reasoning_content: self.reasoning_content,
            tool_calls: if calls.is_empty() { None } else { Some(calls) },
            tool_call_id: None,
            provider_items: None,
        };
        Ok((message, self.usage))
    }
}

#[cfg(test)]
fn process_sse_line(
    line: &[u8],
    data_lines: &mut Vec<String>,
    data_bytes: &mut usize,
    result: &mut CompletionStream,
    on_text: &mut (impl FnMut(&str) + Send),
) -> Result<()> {
    process_sse_line_with_reasoning(line, data_lines, data_bytes, result, on_text, &mut |_| {})
}

fn process_sse_line_with_reasoning(
    line: &[u8],
    data_lines: &mut Vec<String>,
    data_bytes: &mut usize,
    result: &mut CompletionStream,
    on_text: &mut (impl FnMut(&str) + Send),
    on_reasoning: &mut (impl FnMut(&str) + Send),
) -> Result<()> {
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    if line.is_empty() {
        if !data_lines.is_empty() {
            result.apply_with_reasoning(&data_lines.join("\n"), on_text, on_reasoning)?;
            data_lines.clear();
            *data_bytes = 0;
        }
    } else if let Some(data) = line.strip_prefix(b"data:") {
        let data = data.strip_prefix(b" ").unwrap_or(data);
        *data_bytes = data_bytes.saturating_add(data.len());
        if *data_bytes > MAX_SSE_EVENT_BYTES {
            bail!("SSE event exceeds the 2 MiB limit");
        }
        data_lines.push(std::str::from_utf8(data)?.to_string());
    }
    Ok(())
}

/// Consume only new bytes when looking for completed SSE lines. A large
/// chunk with thousands of lines used to drain (and shift) the entire
/// remaining buffer once per line. Compact leftovers once per chunk instead.
fn consume_sse_lines(
    pending: &mut Vec<u8>,
    chunk: &[u8],
    mut on_line: impl FnMut(&[u8]) -> Result<bool>,
) -> Result<bool> {
    let first_new = pending.len();
    if pending.len().saturating_add(chunk.len()) > MAX_SSE_PENDING_BYTES {
        bail!("SSE pending buffer exceeds the 4 MiB limit");
    }
    pending.extend_from_slice(chunk);
    let mut start = 0;
    for end in first_new..pending.len() {
        if pending[end] != b'\n' {
            continue;
        }
        if end.saturating_sub(start) > MAX_SSE_EVENT_BYTES {
            bail!("SSE event exceeds the 2 MiB limit");
        }
        if on_line(&pending[start..end])? {
            return Ok(true);
        }
        start = end + 1;
    }
    if start > 0 {
        pending.drain(..start);
    }
    Ok(false)
}

#[derive(Clone)]
pub struct ModelClient {
    http: Client,
    base_url: String,
    endpoint: String,
    api_key: Option<String>,
    model: String,
    deepseek_thinking: Option<String>,
    protocol: ApiProtocol,
    reasoning_effort: String,
    // Levels advertised by the saved profile. Empty means no restriction.
    thinking_levels: Vec<String>,
    // Only an explicitly selected TUI level overrides the startup profile.
    // Shared across cloned clients; each outbound request reads one snapshot.
    reasoning_override: Arc<RwLock<Option<String>>>,
    image_rejected: Arc<AtomicBool>,
    usage_stream_rejected: Arc<AtomicBool>,
    reasoning_summary_rejected: Arc<AtomicBool>,
    anthropic_cache_rejected: Arc<AtomicBool>,
    model_info: ModelInfo,
    output_limit: Option<usize>,
    requested_output: Option<usize>,
    stream_idle_timeout: Duration,
}

enum CompletionStream {
    Chat(StreamResult),
    Responses(responses::Stream),
    Anthropic(anthropic::Stream),
}

impl CompletionStream {
    fn apply_with_reasoning(
        &mut self,
        data: &str,
        on_text: &mut (impl FnMut(&str) + Send),
        on_reasoning: &mut (impl FnMut(&str) + Send),
    ) -> Result<()> {
        match self {
            Self::Chat(stream) => stream.apply_with_reasoning(data, on_text, on_reasoning),
            Self::Responses(stream) => stream.apply_with_reasoning(data, on_text, on_reasoning),
            Self::Anthropic(stream) => stream.apply_with_reasoning(data, on_text, on_reasoning),
        }
    }

    fn received_delta(&self) -> bool {
        match self {
            Self::Chat(s) => s.received_delta,
            Self::Responses(s) => s.received_delta,
            Self::Anthropic(s) => s.received_delta,
        }
    }

    fn done(&self) -> bool {
        match self {
            Self::Chat(s) => s.done,
            Self::Responses(s) => s.done,
            Self::Anthropic(s) => s.done,
        }
    }

    fn finish(self) -> Result<(Message, Option<Usage>)> {
        match self {
            Self::Chat(s) => s.finish(),
            Self::Responses(s) => s.finish(),
            Self::Anthropic(s) => s.finish(),
        }
    }
}

#[derive(Deserialize)]
struct TextCompletion {
    choices: Vec<TextChoice>,
    usage: Option<Value>,
}

#[derive(Deserialize)]
struct TextChoice {
    message: Message,
    finish_reason: Option<String>,
}

// Non-stream APEC summaries are not agent responses. A thinking model can
// consume the entire max_tokens allowance on internal reasoning before it
// emits any summary text. A truncated response must never be used as memory.
enum ApecTextAttempt {
    Complete(String, Option<Usage>),
    Truncated(Option<Usage>),
}

fn parse_apec_text_response(body: &str) -> Result<ApecTextAttempt> {
    let response: TextCompletion =
        serde_json::from_str(body).context("invalid APEC chat completions response")?;
    let usage = response.usage.as_ref().and_then(Usage::from_value);
    let first = response
        .choices
        .into_iter()
        .next()
        .context("empty APEC choices")?;
    if first.finish_reason.as_deref() == Some("length") {
        return Ok(ApecTextAttempt::Truncated(usage));
    }
    if !matches!(first.finish_reason.as_deref(), Some("stop")) {
        bail!("APEC summary ended unexpectedly: {:?}", first.finish_reason);
    }
    let text = first.message.content.unwrap_or_default();
    if first.message.role != "assistant"
        || text.trim().is_empty()
        || first
            .message
            .tool_calls
            .as_ref()
            .is_some_and(|calls| !calls.is_empty())
    {
        bail!("APEC model did not provide a valid text summary");
    }
    Ok(ApecTextAttempt::Complete(text.trim().to_owned(), usage))
}

fn combine_apec_usage(first: Option<Usage>, second: Option<Usage>) -> Option<Usage> {
    let (Some(first), Some(second)) = (first, second) else {
        return None;
    };
    let add = |left: Option<u64>, right: Option<u64>| {
        left.and_then(|left| right.and_then(|right| left.checked_add(right)))
    };
    Some(Usage {
        input: add(first.input, second.input),
        output: add(first.output, second.output),
        total: add(first.total, second.total),
        cached: add(first.cached, second.cached),
        reasoning: add(first.reasoning, second.reasoning),
    })
}

// APEC still targets a short memory segment. The larger completion allowance
// is for hidden reasoning tokens, not a larger persisted summary.
fn apec_completion_budget(output_tokens: usize, truncated_responses: u32) -> usize {
    output_tokens
        .max(128)
        .saturating_mul(2)
        .max(1024)
        .saturating_mul(1usize << truncated_responses.min(3))
}

fn summary_capability_rejection(error: &anyhow::Error) -> bool {
    rejected_request_field(error, &["reasoning.summary", "reasoning[summary]"])
}

fn anthropic_cache_rejection(error: &anyhow::Error) -> bool {
    rejected_request_field(error, &["cache_control"])
}

/// Treat structured error.param as authoritative when present. Otherwise
/// check *only the actual error message*, never the serialized JSON wrapper:
/// invalid_request_error contains "invalid" even when the described error
/// is unrelated to the requested feature.
fn rejected_request_field(error: &anyhow::Error, fields: &[&str]) -> bool {
    error.downcast_ref::<ModelHttpError>().is_some_and(|http| {
        if !matches!(
            http.status,
            StatusCode::BAD_REQUEST | StatusCode::UNPROCESSABLE_ENTITY
        ) {
            return false;
        }
        let raw: Option<Value> = serde_json::from_str(&http.message).ok();
        let detail = raw.as_ref().map(|body| body.get("error").unwrap_or(body));
        let parameter = detail.and_then(|body| {
            body.get("param")
                .or_else(|| body.get("field"))
                .or_else(|| body.get("parameter"))
                .and_then(Value::as_str)
        });
        let message = detail
            .and_then(|body| body.get("message").and_then(Value::as_str))
            .unwrap_or(&http.message)
            .to_ascii_lowercase();
        let rejection = [
            "unsupported",
            "not supported",
            "not support",
            "unknown",
            "unrecognized",
            "not allowed",
            "not permitted",
            "extra inputs",
            "extra_forbidden",
            "unexpected",
            "invalid field",
            "invalid parameter",
            "unrecognized field",
        ]
        .iter()
        .any(|word| message.contains(word));
        if let Some(parameter) = parameter {
            let matching_field = fields
                .iter()
                .any(|field| parameter.eq_ignore_ascii_case(field));
            let code = detail
                .and_then(|body| body.get("code").and_then(Value::as_str))
                .unwrap_or("");
            return matching_field
                && (rejection
                    || matches!(
                        code,
                        "extra_forbidden" | "unknown_field" | "unsupported_parameter"
                    ));
        }
        let field_present = fields.iter().any(|field| {
            let field = field.to_ascii_lowercase();
            // A dot, bracket, or underscore is part of the field spelling.
            // Require a word boundary so unrelated parameter names cannot
            // accidentally disable a supported provider capability.
            message.match_indices(&field).any(|(start, _)| {
                let end = start + field.len();
                let before = message[..start].chars().next_back();
                let after = message[end..].chars().next();
                let name_char = |ch: char| ch.is_alphanumeric() || ch == '_';
                !before.is_some_and(name_char) && !after.is_some_and(name_char)
            })
        });
        field_present && rejection
    })
}

impl ModelClient {
    pub fn new(base_url: &str, api_key: Option<String>, model: String) -> Result<Self> {
        let http = Client::builder()
            // No whole-request deadline: a healthy streamed reasoning
            // response can legitimately take longer than 120 seconds.
            .connect_timeout(Duration::from_secs(20))
            .build()
            .context("failed to initialize HTTP client")?;
        Ok(Self {
            http,
            base_url: base_url.trim_end_matches('/').to_owned(),
            endpoint: format!("{}/chat/completions", base_url.trim_end_matches('/')),
            api_key,
            model,
            deepseek_thinking: None,
            protocol: ApiProtocol::Chat,
            reasoning_effort: "auto".into(),
            thinking_levels: Vec::new(),
            reasoning_override: Arc::new(RwLock::new(None)),
            image_rejected: Arc::new(AtomicBool::new(false)),
            usage_stream_rejected: Arc::new(AtomicBool::new(false)),
            reasoning_summary_rejected: Arc::new(AtomicBool::new(false)),
            anthropic_cache_rejected: Arc::new(AtomicBool::new(false)),
            model_info: ModelInfo::default(),
            output_limit: None,
            requested_output: None,
            stream_idle_timeout: Duration::from_secs(300),
        })
    }

    pub fn with_protocol(mut self, protocol: &str) -> Result<Self> {
        self.protocol = match protocol {
            "chat" => ApiProtocol::Chat,
            "responses" => ApiProtocol::Responses,
            "anthropic" => ApiProtocol::Anthropic,
            _ => bail!("unsupported API protocol: {protocol}"),
        };
        self.endpoint = match self.protocol {
            ApiProtocol::Chat => format!("{}/chat/completions", self.base_url),
            ApiProtocol::Responses => format!("{}/responses", self.base_url),
            ApiProtocol::Anthropic => format!("{}/messages", self.base_url),
        };
        Ok(self)
    }

    pub fn with_reasoning(mut self, effort: String, deepseek: Option<String>) -> Self {
        self.reasoning_effort = effort;
        self.deepseek_thinking = deepseek;
        self
    }

    /// Restrict the levels this model accepts, as declared by its profile.
    pub fn with_thinking_levels(mut self, levels: Vec<String>) -> Self {
        self.thinking_levels = levels;
        self
    }

    pub fn with_output_tokens(mut self, requested: Option<usize>) -> Self {
        self.requested_output = requested;
        self
    }

    pub fn effective_output_tokens(&self) -> Option<usize> {
        self.requested_output
            .map(|n| {
                let requested = n.min(MAX_TURN_OUTPUT_TOKENS);
                self.output_limit
                    .map_or(requested, |limit| requested.min(limit))
            })
            .or_else(|| self.output_limit.map(|limit| limit.min(4096)))
    }

    fn authenticated(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match (&self.api_key, self.protocol) {
            (Some(key), ApiProtocol::Anthropic) => request.header("x-api-key", key),
            (Some(key), _) => request.bearer_auth(key),
            _ => request,
        }
    }

    fn request(&self, payload: &Value) -> reqwest::RequestBuilder {
        let req = self.http.post(&self.endpoint).json(payload);
        if self.protocol == ApiProtocol::Anthropic {
            self.authenticated(req.header("anthropic-version", "2023-06-01"))
        } else {
            self.authenticated(req)
        }
    }

    /// A metadata lookup is optional. Many OpenAI-compatible /models
    /// endpoints return only the ID and omit token limits.
    pub async fn discover_model(&mut self) -> Option<ModelInfo> {
        let mut endpoint = reqwest::Url::parse(&self.base_url).ok()?;
        endpoint
            .path_segments_mut()
            .ok()?
            .pop_if_empty()
            .push("models")
            .push(&self.model);
        let mut req = self.http.get(endpoint);
        if self.protocol == ApiProtocol::Anthropic {
            req = req.header("anthropic-version", "2023-06-01");
        }
        let result = tokio::time::timeout(Duration::from_secs(4), async {
            let response = self.authenticated(req).send().await.ok()?;
            if !response.status().is_success() {
                return None;
            }
            let body = read_limited_text(response, MAX_MODEL_METADATA_BYTES)
                .await
                .ok()?;
            serde_json::from_str::<Value>(&body).ok()
        })
        .await;
        let info = match result {
            Ok(Some(value)) => ModelInfo::parse(&value),
            _ => ModelInfo::default(),
        };
        crate::runtime_log!(
            "[model] upstream limits: context={:?} output={:?} (missing values use Kroma defaults)",
            info.context_window,
            info.max_output_tokens
        );
        self.output_limit = info.max_output_tokens;
        self.model_info = info.clone();
        Some(info)
    }

    #[cfg(test)]
    fn payload(
        &self,
        messages: &[Message],
        tools: &[Value],
        max_tokens: Option<usize>,
    ) -> Result<Value> {
        self.payload_with_images(messages, tools, max_tokens, None)
    }

    fn payload_with_images(
        &self,
        messages: &[Message],
        tools: &[Value],
        max_tokens: Option<usize>,
        images: Option<&TaskImageCache>,
    ) -> Result<Value> {
        let output = max_tokens.or_else(|| self.effective_output_tokens());
        match self.protocol {
            ApiProtocol::Chat => {
                let mut value = self.chat_payload_with_tools(messages, tools);
                if let Some(items) = value.get_mut("messages").and_then(Value::as_array_mut) {
                    for (item, message) in items.iter_mut().zip(messages.iter()) {
                        if message.role == "user" {
                            item["content"] = crate::attachments::content_with_cache(
                                message.content.as_deref().unwrap_or(""),
                                message.provider_items.as_deref().unwrap_or(&[]),
                                "chat",
                                images,
                            )?;
                        }
                    }
                }
                // Preserve Chat Completions' historical omission of max_tokens;
                // many OpenAI-compatible thinking models reject that field.
                // An explicit requested cap remains opt-in.
                if let Some(limit) = self.requested_output.map(|_| output.unwrap()) {
                    value["max_tokens"] = json!(limit);
                }
                if self.reasoning_effort != "auto"
                    && self.deepseek_thinking.is_none()
                    && self
                        .reasoning_override
                        .read()
                        .expect("reasoning lock poisoned")
                        .is_none()
                {
                    value["reasoning_effort"] = json!(if self.reasoning_effort == "off" {
                        "none"
                    } else {
                        &self.reasoning_effort
                    });
                }
                Ok(value)
            }
            ApiProtocol::Responses => {
                let mut request = responses::payload_with_cache(
                    &self.model,
                    messages,
                    tools,
                    Some(output.unwrap_or(4096)),
                    self.reasoning_override
                        .read()
                        .expect("reasoning lock poisoned")
                        .as_deref()
                        .unwrap_or(&self.reasoning_effort),
                    &self.opaque_reasoning_origin(),
                    images,
                )?;
                if self.reasoning_summary_rejected.load(Ordering::Relaxed) {
                    if let Some(reasoning) =
                        request.get_mut("reasoning").and_then(Value::as_object_mut)
                    {
                        reasoning.remove("summary");
                        if reasoning.is_empty() {
                            request.as_object_mut().unwrap().remove("reasoning");
                        }
                    }
                }
                Ok(request)
            }
            ApiProtocol::Anthropic => {
                let mut request = anthropic::payload_with_cache(
                    &self.model,
                    messages,
                    tools,
                    output.unwrap_or(4096),
                    self.reasoning_override
                        .read()
                        .expect("reasoning lock poisoned")
                        .as_deref()
                        .unwrap_or(&self.reasoning_effort),
                    &self.model_info,
                    &self.opaque_reasoning_origin(),
                    images,
                )?;
                if self.anthropic_cache_rejected.load(Ordering::Relaxed) {
                    request.as_object_mut().unwrap().remove("cache_control");
                }
                Ok(request)
            }
        }
    }

    /// Enable DeepSeek-specific thinking parameters only on request. Some
    /// OpenAI-compatible gateways do not recognize these fields.
    pub fn with_deepseek_thinking(mut self, mode: Option<String>) -> Self {
        self.deepseek_thinking = mode;
        self
    }

    // Bind replayable Chat reasoning to its originating endpoint and model.
    // Store only a fingerprint, never the endpoint URL or API credentials in
    // the transcript metadata. This survives session resume and model swaps.
    fn chat_reasoning_origin(&self) -> String {
        // Use a fixed algorithm rather than DefaultHasher, whose output is
        // not guaranteed stable across Rust/compiler upgrades.
        let hash = self
            .base_url
            .bytes()
            .chain(std::iter::once(0))
            .chain(self.model.bytes())
            .fold(0xcbf29ce484222325u64, |hash, byte| {
                (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
            });
        format!("chat-v1-{hash:016x}")
    }

    fn tag_chat_reasoning(&self, message: &mut Message) {
        if self.protocol != ApiProtocol::Chat || message.reasoning_content.is_none() {
            return;
        }
        message
            .provider_items
            .get_or_insert_with(Vec::new)
            .push(json!({
                "type": "kroma_chat_reasoning_origin",
                "id": self.chat_reasoning_origin()
            }));
    }

    fn opaque_reasoning_origin(&self) -> String {
        let protocol = match self.protocol {
            ApiProtocol::Chat => "chat",
            ApiProtocol::Responses => "responses",
            ApiProtocol::Anthropic => "anthropic",
        };
        format!("{protocol}-{}", self.chat_reasoning_origin())
    }

    fn tag_opaque_reasoning(&self, message: &mut Message) {
        let has_opaque = message
            .provider_items
            .as_deref()
            .unwrap_or(&[])
            .iter()
            .any(|item| match self.protocol {
                ApiProtocol::Responses => item["type"] == "reasoning",
                ApiProtocol::Anthropic => matches!(
                    item["type"].as_str(),
                    Some("thinking" | "redacted_thinking")
                ),
                ApiProtocol::Chat => false,
            });
        if has_opaque {
            message
                .provider_items
                .get_or_insert_with(Vec::new)
                .push(json!({
                    "type": "kroma_opaque_reasoning_origin",
                    "id": self.opaque_reasoning_origin()
                }));
        }
    }

    fn chat_payload_with_tools(&self, messages: &[Message], tools: &[Value]) -> Value {
        // DeepSeek V4 requires a non-null content field on assistant tool
        // messages; older persisted sessions had content=null. Fix the
        // outbound projection without rewriting authoritative raw history.
        // tool_choice defaults to auto when tools are present; DeepSeek V4
        // thinking rejects explicitly passing the tool_choice parameter.
        let mut payload = json!({
            "model": self.model,
            "messages": messages,
            "tools": tools,
            "stream": true,
            "stream_options": {"include_usage": true}
        });
        if self.usage_stream_rejected.load(Ordering::Relaxed) {
            payload.as_object_mut().unwrap().remove("stream_options");
        }
        // DeepSeek's tool-thinking protocol replays reasoning from ALL turns,
        // including assistant messages without a tool call. Provenance-tagged
        // history can be safely replayed across user turns/model switches.
        // Older untagged records are accepted only in the active user turn:
        // their origin cannot be established after a model switch.
        let active_turn = messages
            .iter()
            .rposition(|message| message.role == "user")
            .map_or(0, |index| index + 1);
        let replay_reasoning = !tools.is_empty()
            && (self.deepseek_thinking.is_some()
                || self.model.to_ascii_lowercase().contains("deepseek"))
            && self.reasoning_level().as_deref() != Some("off");
        let expected_origin = self.chat_reasoning_origin();
        if let Some(items) = payload.get_mut("messages").and_then(Value::as_array_mut) {
            for (index, message) in items.iter_mut().enumerate() {
                let origin = messages[index]
                    .provider_items
                    .as_deref()
                    .unwrap_or(&[])
                    .iter()
                    .find(|part| part["type"] == "kroma_chat_reasoning_origin")
                    .and_then(|part| part["id"].as_str());
                message
                    .as_object_mut()
                    .map(|map| map.remove("provider_items"));
                if !replay_reasoning
                    || message["role"] != "assistant"
                    || !match origin {
                        Some(id) => id == expected_origin,
                        None => index >= active_turn,
                    }
                {
                    message
                        .as_object_mut()
                        .map(|map| map.remove("reasoning_content"));
                }
                // Strict DeepSeek thinking endpoints expect this field even
                // when a tool-calling assistant emitted no thought text.
                // Never add it to ordinary OpenAI-compatible models.
                if replay_reasoning
                    && message["role"] == "assistant"
                    && message["tool_calls"]
                        .as_array()
                        .is_some_and(|calls| !calls.is_empty())
                    && message.get("reasoning_content").is_none()
                {
                    message["reasoning_content"] = json!("");
                }
                if message.get("role").and_then(Value::as_str) == Some("assistant")
                    && message
                        .get("tool_calls")
                        .and_then(Value::as_array)
                        .is_some_and(|calls| !calls.is_empty())
                    && message.get("content").is_none_or(Value::is_null)
                {
                    message["content"] = json!("");
                }
            }
        }
        let selected = self
            .reasoning_override
            .read()
            .expect("reasoning lock poisoned")
            .clone();
        match selected.as_deref() {
            Some("off") => {
                if self.deepseek_thinking.is_some() {
                    payload["thinking"] = json!({"type":"disabled"});
                } else {
                    payload["reasoning_effort"] = json!("none");
                }
            }
            Some("auto") => {
                if self.deepseek_thinking.is_some() {
                    payload["thinking"] = json!({"type":"enabled"});
                }
            }
            Some(level) => {
                if self.deepseek_thinking.is_some() {
                    payload["thinking"] = json!({"type":"enabled"});
                }
                payload["reasoning_effort"] = json!(level);
            }
            None => {
                if let Some(mode) = &self.deepseek_thinking {
                    payload["thinking"] = json!({"type":"enabled"});
                    if mode != "enabled" {
                        payload["reasoning_effort"] = json!(mode);
                    }
                }
            }
        }
        payload
    }

    pub async fn complete_with_tools(
        &self,
        messages: &[Message],
        tools: &[Value],
        on_text: &mut (impl FnMut(&str) + Send),
    ) -> Result<Completion> {
        self.complete_with_tools_stream(messages, tools, None, on_text, &mut |_| {})
            .await
    }

    async fn complete_with_tools_stream(
        &self,
        messages: &[Message],
        tools: &[Value],
        images: Option<&TaskImageCache>,
        on_text: &mut (impl FnMut(&str) + Send),
        on_reasoning: &mut (impl FnMut(&str) + Send),
    ) -> Result<Completion> {
        // Only retry transport failures or transient HTTP statuses before
        // any response body is received. Never replay a partial stream.
        let mut text_fallback: Option<Vec<Message>> = None;
        let mut usage_fallback = false;
        for attempt in 0..=5 {
            let mut received_stream_data = false;
            crate::runtime_log!("[model] request attempt={} started", attempt + 1);
            match self
                .complete_once(
                    text_fallback.as_deref().unwrap_or(messages),
                    tools,
                    images,
                    on_text,
                    on_reasoning,
                    &mut received_stream_data,
                )
                .await
            {
                Err(error)
                    if !received_stream_data
                        && self.protocol == ApiProtocol::Anthropic
                        && !self.anthropic_cache_rejected.load(Ordering::Relaxed)
                        && anthropic_cache_rejection(&error) =>
                {
                    // Some Anthropic-compatible gateways don't implement
                    // the official cache field. Retry at most once without
                    // it; never replay a partially delivered stream.
                    self.anthropic_cache_rejected.store(true, Ordering::Relaxed);
                    crate::runtime_log!("[model] gateway rejected Anthropic cache_control; retrying without prompt caching");
                }
                Err(error)
                    if !received_stream_data
                        && self.protocol == ApiProtocol::Responses
                        && !self.reasoning_summary_rejected.load(Ordering::Relaxed)
                        && summary_capability_rejection(&error) =>
                {
                    self.reasoning_summary_rejected
                        .store(true, Ordering::Relaxed);
                    crate::runtime_log!("[model] gateway rejected reasoning.summary; retrying without visible summaries");
                }
                Err(error)
                    if !received_stream_data
                        && !usage_fallback
                        && self.protocol == ApiProtocol::Chat
                        && rejected_request_field(&error, &["stream_options"]) =>
                {
                    self.usage_stream_rejected.store(true, Ordering::Relaxed);
                    usage_fallback = true;
                    crate::runtime_log!("[model] gateway rejected stream_options; retrying without usage streaming metadata");
                }
                Err(error)
                    if !received_stream_data
                        && text_fallback.is_none()
                        && messages.iter().any(|msg| {
                            msg.provider_items
                                .as_deref()
                                .unwrap_or(&[])
                                .iter()
                                .any(crate::attachments::inline_image)
                        })
                        && image_capability_rejection(&error) =>
                {
                    // One immediate fallback, not another retry policy:
                    // the provider explicitly rejected visual input.
                    let mut view = messages.to_vec();
                    for msg in &mut view {
                        for part in msg.provider_items.as_mut().into_iter().flatten() {
                            if crate::attachments::is_image(part) {
                                part["mode"] = json!("reference");
                            }
                        }
                    }
                    crate::runtime_log!("[model] upstream rejected visual input; retrying once with explicit image references");
                    self.image_rejected.store(true, Ordering::Relaxed);
                    text_fallback = Some(view);
                }
                // Never replay a partially delivered stream: it could duplicate visible output.
                Err(error)
                    if attempt < 3
                        && !received_stream_data
                        && retry_delay(&error, attempt + 1).is_some() =>
                {
                    let delay = retry_delay(&error, attempt + 1).unwrap();
                    crate::runtime_log!(
                        "[model] request attempt={} failed; usage unknown",
                        attempt + 1
                    );
                    crate::runtime_log!(
                        "[model] transient request failure; retrying ({}/3) after {}ms: {error:#}",
                        attempt + 1,
                        delay.as_millis()
                    );
                    tokio::time::sleep(delay).await;
                }
                result => {
                    if result.is_err() {
                        crate::runtime_log!(
                            "[model] request attempt={} failed; usage unknown",
                            attempt + 1
                        );
                    }
                    return result.map(|mut completion| {
                        self.tag_chat_reasoning(&mut completion.message);
                        self.tag_opaque_reasoning(&mut completion.message);
                        completion
                    });
                }
            }
        }
        unreachable!()
    }

    /// The model adapter handles generic tool-free text completion. The
    /// caller owns domain-specific prompts such as memory summarization.
    pub async fn text_completion(
        &self,
        system: &str,
        text: &str,
        output_tokens: usize,
    ) -> Result<SummaryResult> {
        self.text_completion_with_budget(system, text, output_tokens, false)
            .await
    }

    async fn short_text_completion(
        &self,
        system: &str,
        text: &str,
        output_tokens: usize,
    ) -> Result<SummaryResult> {
        self.text_completion_with_budget(system, text, output_tokens, true)
            .await
    }

    async fn text_completion_with_budget(
        &self,
        system: &str,
        text: &str,
        output_tokens: usize,
        short: bool,
    ) -> Result<SummaryResult> {
        let mut timeouts = 0;
        let mut truncations = 0;
        let mut requests = 0;
        let mut elapsed = Duration::ZERO;
        let mut spent_usage: Option<Usage> = None;
        let mut usage_known = true;
        loop {
            // APEC needs a large hidden-reasoning reserve. A short session
            // title does not: cap the generation budget and never retry a
            // truncated title with an exponentially larger allowance.
            let budget = if short {
                output_tokens.clamp(32, 160)
            } else {
                apec_completion_budget(output_tokens, truncations)
            };
            let max_tokens = self.output_limit.map_or(budget, |limit| budget.min(limit));
            let mut payload = json!({
                "model": self.model,
                "stream": false,
                "max_tokens": max_tokens,
                "messages": [
                    {"role":"system", "content": system},
                    {"role":"user", "content": text}
                ]
            });
            // Memory consolidation needs inexpensive factual extraction, not
            // the coding task's High reasoning budget. DeepSeek defaults to
            // High unless an effort is explicitly requested. For generic
            // OpenAI-compatible models avoid sending a nonstandard field.
            if self.deepseek_thinking.is_some()
                || self.model.to_ascii_lowercase().contains("deepseek")
            {
                payload["reasoning_effort"] = json!("low");
            }
            let start = Instant::now();
            requests += 1;
            let payload = match self.protocol {
                ApiProtocol::Chat => payload,
                ApiProtocol::Responses => {
                    let mut p = responses::payload(
                        &self.model,
                        &[Message::new("system", system), Message::new("user", text)],
                        &[],
                        Some(max_tokens),
                        "auto",
                        &self.opaque_reasoning_origin(),
                    )?;
                    p["stream"] = json!(false);
                    // Tool-free memory generation does not display reasoning.
                    p.as_object_mut().unwrap().remove("reasoning");
                    p
                }
                ApiProtocol::Anthropic => {
                    let mut p = anthropic::payload(
                        &self.model,
                        &[Message::new("system", system), Message::new("user", text)],
                        &[],
                        max_tokens,
                        "auto",
                        &self.model_info,
                        &self.opaque_reasoning_origin(),
                    )?;
                    p["stream"] = json!(false);
                    // Each APEC summary is a one-off prompt. Caching it would
                    // pay a write premium without expecting a read.
                    p.as_object_mut().unwrap().remove("cache_control");
                    p
                }
            };
            let request = self.request(&payload);
            let result = tokio::time::timeout(Duration::from_secs(120), async {
                let response = request
                    .send()
                    .await
                    .context("tool-free model request failed")?;
                let status = response.status();
                let body = read_limited_text(response, MAX_APEC_RESPONSE_BYTES)
                    .await
                    .context("failed to read APEC response")?;
                if !status.is_success() {
                    let excerpt: String = body.chars().take(500).collect();
                    bail!("tool-free model returned HTTP {status}: {excerpt}");
                }
                match self.protocol {
                    ApiProtocol::Chat => parse_apec_text_response(&body),
                    ApiProtocol::Responses => responses::parse_text(&body),
                    ApiProtocol::Anthropic => anthropic::parse_text(&body),
                }
            })
            .await
            .map_err(|err| {
                anyhow::Error::new(err).context("tool-free model request exceeded 120 seconds")
            })
            .and_then(|response| response);
            elapsed += start.elapsed();
            match result {
                Ok(ApecTextAttempt::Complete(text, usage)) => {
                    let usage = if requests == 1 {
                        usage
                    } else if usage_known {
                        combine_apec_usage(spent_usage, usage)
                    } else {
                        None
                    };
                    return Ok(SummaryResult {
                        text,
                        usage,
                        elapsed,
                        requests,
                    });
                }
                Ok(ApecTextAttempt::Truncated(usage)) => {
                    if short {
                        // The caller supplies a free local title fallback.
                        // Never spend another request just to name a session.
                        crate::runtime_log!(
                            "[title] short response truncated (output={}); using local title",
                            usage
                                .as_ref()
                                .and_then(|u| u.output)
                                .map_or_else(|| "unknown".to_owned(), |n| n.to_string())
                        );
                        return Ok(SummaryResult {
                            text: String::new(),
                            usage,
                            elapsed,
                            requests,
                        });
                    }
                    spent_usage = if requests == 1 {
                        usage
                    } else if usage_known {
                        combine_apec_usage(spent_usage, usage)
                    } else {
                        None
                    };
                    usage_known &= spent_usage.is_some();
                    if truncations >= 3 {
                        bail!("APEC summary remained truncated after 4 completion budgets; original history preserved");
                    }
                    truncations += 1;
                    crate::runtime_log!(
                        "[apec] summary truncated; retrying with a larger completion allowance (attempt {}/4)",
                        truncations + 1
                    );
                }
                Err(error) if !short && timeouts < 3 && is_timeout(&error) => {
                    timeouts += 1;
                    usage_known = false;
                    crate::runtime_log!("[apec] request timed out; retrying ({timeouts}/3)");
                }
                Err(error) => return Err(error),
            }
        }
    }

    async fn complete_once(
        &self,
        messages: &[Message],
        tools: &[Value],
        images: Option<&TaskImageCache>,
        on_text: &mut (impl FnMut(&str) + Send),
        on_reasoning: &mut (impl FnMut(&str) + Send),
        received_stream_data: &mut bool,
    ) -> Result<Completion> {
        let started = Instant::now();
        let payload = self.payload_with_images(messages, tools, None, images)?;
        let request = self.request(&payload);

        let response = tokio::time::timeout(self.stream_idle_timeout, request.send())
            .await
            .context("model response headers idle timeout")?
            .context("model request failed")?;
        let status = response.status();
        if !status.is_success() {
            let retry_after_raw = response.headers().contains_key(RETRY_AFTER);
            let retry_after = parse_retry_after(&response);
            let text = tokio::time::timeout(
                self.stream_idle_timeout,
                read_limited_text(response, MAX_ERROR_BODY_BYTES),
            )
            .await
            .context("model error response body idle timeout")?
            .context("failed to read model error")?;
            let excerpt: String = text.chars().take(1000).collect();
            // Unsupported Retry-After (e.g. HTTP-date) must not be ignored.
            let retry_after = if retry_after_raw && retry_after.is_none() {
                Some(MAX_RETRY_AFTER + Duration::from_secs(1))
            } else {
                retry_after
            };
            return Err(ModelHttpError {
                status,
                retry_after,
                message: excerpt,
            }
            .into());
        }

        let mut stream = response.bytes_stream();
        let mut pending = Vec::<u8>::new();
        let mut data_lines = Vec::new();
        let mut data_bytes = 0usize;
        let mut result = match self.protocol {
            ApiProtocol::Chat => CompletionStream::Chat(StreamResult::default()),
            ApiProtocol::Responses => CompletionStream::Responses(responses::Stream::default()),
            ApiProtocol::Anthropic => CompletionStream::Anthropic(anthropic::Stream::default()),
        };
        let mut first_token = None;
        let mut total_stream_bytes = 0usize;
        while let Some(chunk) = tokio::time::timeout(self.stream_idle_timeout, stream.next())
            .await
            .context("model streaming idle timeout")?
        {
            let chunk = chunk.context("model streaming connection failed")?;
            total_stream_bytes = total_stream_bytes.saturating_add(chunk.len());
            if total_stream_bytes > MAX_STREAM_BYTES {
                bail!("model stream exceeds the 64 MiB limit");
            }
            if !chunk.is_empty() {
                *received_stream_data = true;
            }
            let done = consume_sse_lines(&mut pending, &chunk, |line| {
                process_sse_line_with_reasoning(
                    line,
                    &mut data_lines,
                    &mut data_bytes,
                    &mut result,
                    on_text,
                    on_reasoning,
                )?;
                if first_token.is_none() && result.received_delta() {
                    first_token = Some(started.elapsed());
                }
                Ok(result.done())
            })?;
            if done {
                let (message, usage) = result.finish()?;
                return Ok(Completion {
                    message,
                    usage,
                    elapsed: started.elapsed(),
                    first_token,
                });
            }
        }
        if !pending.is_empty() {
            process_sse_line_with_reasoning(
                &pending,
                &mut data_lines,
                &mut data_bytes,
                &mut result,
                on_text,
                on_reasoning,
            )?;
        }
        process_sse_line_with_reasoning(
            &[],
            &mut data_lines,
            &mut data_bytes,
            &mut result,
            on_text,
            on_reasoning,
        )?;
        if first_token.is_none() && result.received_delta() {
            first_token = Some(started.elapsed());
        }
        let (message, usage) = result.finish()?;
        Ok(Completion {
            message,
            usage,
            elapsed: started.elapsed(),
            first_token,
        })
    }
}

impl ModelBackend for ModelClient {
    fn project_for_budget(&self, messages: &mut [Message]) {
        let active_turn = messages
            .iter()
            .rposition(|msg| msg.role == "user")
            .unwrap_or(0);
        let expected_chat = self.chat_reasoning_origin();
        let expected_opaque = self.opaque_reasoning_origin();
        let deepseek_replay = (self.deepseek_thinking.is_some()
            || self.model.to_ascii_lowercase().contains("deepseek"))
            && self.reasoning_level().as_deref() != Some("off");
        for (index, message) in messages.iter_mut().enumerate() {
            if message.role == "tool" {
                // The tool's error bit is used by some adapters but its
                // metadata envelope is not transmitted to any provider.
                message.provider_items = None;
            } else if message.role == "assistant" {
                match self.protocol {
                    ApiProtocol::Chat => {
                        let stored = message
                            .provider_items
                            .as_deref()
                            .unwrap_or(&[])
                            .iter()
                            .find(|item| item["type"] == "kroma_chat_reasoning_origin")
                            .and_then(|item| item["id"].as_str());
                        let matching =
                            stored.map_or(index >= active_turn, |id| id == expected_chat);
                        if !deepseek_replay || !matching {
                            message.reasoning_content = None;
                        }
                        message.provider_items = None;
                    }
                    ApiProtocol::Responses | ApiProtocol::Anthropic => {
                        message.reasoning_content = None; // visual summary, not wire history
                        let matching =
                            replay_opaque_reasoning(message, index, active_turn, &expected_opaque);
                        if let Some(items) = &mut message.provider_items {
                            items.retain(|item| {
                                matching
                                    && match self.protocol {
                                        ApiProtocol::Responses => item["type"] == "reasoning",
                                        ApiProtocol::Anthropic => matches!(
                                            item["type"].as_str(),
                                            Some("thinking" | "redacted_thinking")
                                        ),
                                        ApiProtocol::Chat => false,
                                    }
                            });
                        }
                    }
                }
            }
        }
    }

    fn accepts_images(&self) -> Option<bool> {
        if self.image_rejected.load(Ordering::Relaxed) {
            return Some(false);
        }
        if let Some(value) = self.model_info.image_input {
            return Some(value);
        }
        let caps = self.model_info.capabilities.as_ref()?;
        for path in [
            "/input/image/supported",
            "/vision/supported",
            "/image/supported",
        ] {
            if let Some(value) = caps.pointer(path).and_then(Value::as_bool) {
                return Some(value);
            }
            if let Some(value) = caps
                .pointer(path)
                .and_then(|value| value.get("supported"))
                .and_then(Value::as_bool)
            {
                return Some(value);
            }
        }
        caps.get("vision")
            .and_then(Value::as_bool)
            .or_else(|| caps.get("supports_vision").and_then(Value::as_bool))
    }
    fn reasoning_level(&self) -> Option<String> {
        let selected = self
            .reasoning_override
            .read()
            .expect("reasoning lock poisoned");
        Some(selected.clone().unwrap_or_else(|| {
            match self.deepseek_thinking.as_deref() {
                Some("high" | "max") => self.deepseek_thinking.clone().unwrap(),
                // "enabled" is a mode, not an effort. Saving the effort keeps
                // the mode in the profile and gives restoration a valid tier.
                _ => crate::profiles::normalize_reasoning(&self.reasoning_effort)
                    .unwrap_or_else(|_| "auto".into()),
            }
        }))
    }

    fn thinking_levels(&self) -> &[String] {
        &self.thinking_levels
    }

    fn set_reasoning_level(&self, level: &str) -> Result<()> {
        let level = crate::profiles::normalize_reasoning(level)?;
        // "auto" always means "provider default", so it stays available even
        // when a profile advertises a narrower set of concrete tiers.
        if level != "auto"
            && !self.thinking_levels.is_empty()
            && !self.thinking_levels.iter().any(|allowed| allowed == &level)
        {
            bail!(
                "this model profile allows only {}; requested {level}",
                self.thinking_levels.join(", ")
            );
        }
        *self
            .reasoning_override
            .write()
            .expect("reasoning lock poisoned") = Some(level);
        Ok(())
    }

    fn complete<'a>(
        &'a self,
        messages: &'a [Message],
        tools: &'a [Value],
        on_text: &'a mut (dyn FnMut(&str) + Send),
    ) -> StreamFuture<'a, Completion> {
        Box::pin(async move {
            self.complete_with_tools(messages, tools, &mut |delta| on_text(delta))
                .await
        })
    }

    fn complete_with_reasoning<'a>(
        &'a self,
        messages: &'a [Message],
        tools: &'a [Value],
        on_text: &'a mut (dyn FnMut(&str) + Send),
        on_reasoning: &'a mut (dyn FnMut(&str) + Send),
    ) -> StreamFuture<'a, Completion> {
        Box::pin(async move {
            self.complete_with_tools_stream(
                messages,
                tools,
                None,
                &mut |delta| on_text(delta),
                &mut |delta| on_reasoning(delta),
            )
            .await
        })
    }

    fn complete_with_task_images<'a>(
        &'a self,
        messages: &'a [Message],
        tools: &'a [Value],
        images: &'a TaskImageCache,
        on_text: &'a mut (dyn FnMut(&str) + Send),
        on_reasoning: &'a mut (dyn FnMut(&str) + Send),
    ) -> StreamFuture<'a, Completion> {
        Box::pin(async move {
            self.complete_with_tools_stream(
                messages,
                tools,
                Some(images),
                &mut |delta| on_text(delta),
                &mut |delta| on_reasoning(delta),
            )
            .await
        })
    }

    fn generate_text<'a>(
        &'a self,
        system: &'a str,
        text: &'a str,
        output_tokens: usize,
    ) -> ModelFuture<'a, SummaryResult> {
        Box::pin(self.text_completion(system, text, output_tokens))
    }

    fn generate_short_text<'a>(
        &'a self,
        system: &'a str,
        text: &'a str,
        output_tokens: usize,
    ) -> ModelFuture<'a, SummaryResult> {
        Box::pin(self.short_text_completion(system, text, output_tokens))
    }
}

#[cfg(test)]
impl ModelClient {
    // Keep original provider unit-test helpers out of the production model
    // boundary; only the runtime selects the available tool definitions.
    fn chat_payload(&self, messages: &[Message]) -> Value {
        self.chat_payload_with_tools(messages, &crate::tools::definitions())
    }

    async fn complete(
        &self,
        messages: &[Message],
        on_text: &mut (impl FnMut(&str) + Send),
    ) -> Result<Completion> {
        self.complete_with_tools(messages, &crate::tools::definitions(), on_text)
            .await
    }
}

fn is_timeout(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<reqwest::Error>()
            .is_some_and(reqwest::Error::is_timeout)
            || cause.is::<tokio::time::error::Elapsed>()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_fallback_uses_actual_error_field_not_json_wrapper() {
        let error = |body: &str| {
            anyhow::Error::new(ModelHttpError {
                status: StatusCode::BAD_REQUEST,
                retry_after: None,
                message: body.to_owned(),
            })
        };
        assert!(!anthropic_cache_rejection(&error(
            r#"{"error":{"type":"invalid_request_error","message":"cache_control: value is malformed"}}"#
        )));
        assert!(anthropic_cache_rejection(&error(
            r#"{"error":{"message":"cache_control is not permitted"}}"#
        )));
        assert!(anthropic_cache_rejection(&error(
            r#"{"error":{"param":"cache_control","type":"invalid_request_error","message":"extra inputs"}}"#
        )));
        assert!(!anthropic_cache_rejection(&error(
            r#"{"error":{"param":"other_field","message":"cache_control is not permitted"}}"#
        )));
        assert!(!anthropic_cache_rejection(&error(
            r#"{"error":{"param":"cache_control","message":"value too large"}}"#
        )));
        assert!(summary_capability_rejection(&error(
            r#"{"error":{"message":"reasoning.summary is not permitted"}}"#
        )));
        assert!(!summary_capability_rejection(&error(
            r#"{"error":{"type":"invalid_request_error","message":"reasoning.summary generated successfully"}}"#
        )));
    }

    #[tokio::test]
    async fn anthropic_cache_rejection_retries_once_and_remembers_gateway_capability() {
        use std::io::{BufRead, BufReader, Read, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let mut requests = Vec::new();
            for attempt in 0..3 {
                let (mut connection, _) = listener.accept().unwrap();
                connection
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut reader = BufReader::new(&connection);
                let mut len = 0usize;
                loop {
                    let mut header = String::new();
                    reader.read_line(&mut header).unwrap();
                    if header == "\r\n" {
                        break;
                    }
                    if let Some(raw) = header.to_ascii_lowercase().strip_prefix("content-length:") {
                        len = raw.trim().parse().unwrap();
                    }
                }
                let mut bytes = vec![0; len];
                reader.read_exact(&mut bytes).unwrap();
                requests.push(serde_json::from_slice::<Value>(&bytes).unwrap());
                drop(reader);
                if attempt == 0 {
                    let body =
                        r#"{"type":"error","error":{"message":"unknown field cache_control"}}"#;
                    write!(connection, "HTTP/1.1 400 Bad Request\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
                } else {
                    let events = [
                        json!({"type":"message_start","message":{"usage":{"input_tokens":100,"output_tokens":0}}}),
                        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":"ok"}}),
                        json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}}),
                        json!({"type":"message_stop"}),
                    ];
                    let mut body = String::new();
                    for event in &events {
                        body.push_str("data: ");
                        body.push_str(&event.to_string());
                        body.push_str("\n\n");
                    }
                    write!(connection, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
                }
                connection.flush().unwrap();
            }
            requests
        });
        let client = ModelClient::new(&format!("http://{address}/v1"), None, "mock".into())
            .unwrap()
            .with_protocol("anthropic")
            .unwrap();
        let history = [Message::new("user", "Check caching")];
        for _ in 0..2 {
            let answer = client
                .complete_with_tools(&history, &[], &mut |_| {})
                .await
                .unwrap();
            assert_eq!(answer.message.content.as_deref(), Some("ok"));
        }
        let requests = server.join().unwrap();
        assert_eq!(requests[0]["cache_control"]["type"], "ephemeral");
        assert!(requests[1].get("cache_control").is_none());
        assert!(requests[2].get("cache_control").is_none());
    }

    #[tokio::test]
    async fn visual_rejection_once_falls_back_to_text_references_without_changing_history() {
        use std::io::{BufRead, BufReader, Read, Write};
        use std::net::TcpListener;
        use std::time::{SystemTime, UNIX_EPOCH};
        let socket = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = socket.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let mut requests = Vec::new();
            for attempt in 0..2 {
                let (mut stream, _) = socket.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(4)))
                    .unwrap();
                let mut reader = BufReader::new(&stream);
                let mut length = 0usize;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = value.trim().parse().unwrap();
                    }
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                drop(reader);
                requests.push(String::from_utf8(body).unwrap());
                if attempt == 0 {
                    let body = r#"{"error":"image input not supported by model"}"#;
                    write!(stream,"HTTP/1.1 400 Bad Request\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
                } else {
                    let body="data: {\"choices\":[{\"delta\":{\"content\":\"text fallback\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
                    write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
                }
                stream.flush().unwrap();
            }
            requests
        });
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("kroma-unsupported-image-{nonce}.png"));
        std::fs::write(&path, b"image pixels").unwrap();
        let mut message = Message::new("user", "Review screenshot");
        message.provider_items = Some(vec![json!({"type":"kroma_attachment","kind":"image",
            "name":"screenshot.png","path":path,"mime":"image/png","bytes":12})]);
        let snapshot = serde_json::to_string(&message).unwrap();
        let client =
            ModelClient::new(&format!("http://{address}/v1"), None, "unknown".into()).unwrap();
        let mut streamed = String::new();
        let answer = client
            .complete_with_tools(&[message.clone()], &[], &mut |text| streamed.push_str(text))
            .await
            .unwrap();
        assert_eq!(answer.message.content.as_deref(), Some("text fallback"));
        assert_eq!(streamed, "text fallback");
        assert_eq!(client.accepts_images(), Some(false));
        let received = server.join().unwrap();
        assert_eq!(received.len(), 2);
        assert!(received[0].contains("image_url"));
        assert!(!received[1].contains("image_url"));
        assert!(received[1].contains("omitted from model input"));
        assert_eq!(serde_json::to_string(&message).unwrap(), snapshot);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn declared_model_modalities_drive_image_compatibility() {
        let text = ModelInfo::parse(&json!({"architecture":{"input_modalities":["text"]}}));
        assert_eq!(text.image_input, Some(false));
        let vision = ModelInfo::parse(&json!({"input_modalities":["text","image"]}));
        assert_eq!(vision.image_input, Some(true));
        let unknown = ModelInfo::parse(&json!({"context_window":128000}));
        assert_eq!(unknown.image_input, None);
    }

    #[test]
    fn malformed_image_is_an_error_not_a_reason_to_downgrade_every_image() {
        let malformed = anyhow::Error::new(ModelHttpError {
            status: StatusCode::BAD_REQUEST,
            retry_after: None,
            message: "invalid image data".into(),
        });
        assert!(!image_capability_rejection(&malformed));
        let unsupported = anyhow::Error::new(ModelHttpError {
            status: StatusCode::BAD_REQUEST,
            retry_after: None,
            message: "image input is not supported".into(),
        });
        assert!(image_capability_rejection(&unsupported));
    }

    #[test]
    fn changing_reasoning_updates_next_payload_without_rebuilding_backend() {
        let client = ModelClient::new("http://localhost/v1", None, "mock".into())
            .unwrap()
            .with_deepseek_thinking(Some("high".into()));
        let data = [Message::new("user", "hello")];
        assert_eq!(client.chat_payload(&data)["reasoning_effort"], "high");
        ModelBackend::set_reasoning_level(&client, "medium").unwrap();
        assert_eq!(client.chat_payload(&data)["reasoning_effort"], "medium");
        let clone = client.clone();
        ModelBackend::set_reasoning_level(&clone, "off").unwrap();
        assert_eq!(client.chat_payload(&data)["thinking"]["type"], "disabled");
        assert!(client.chat_payload(&data).get("reasoning_effort").is_none());
        ModelBackend::set_reasoning_level(&clone, "auto").unwrap();
        assert_eq!(client.chat_payload(&data)["thinking"]["type"], "enabled");
        assert!(client.chat_payload(&data).get("reasoning_effort").is_none());
        let response = ModelClient::new("http://localhost/v1", None, "mock".into())
            .unwrap()
            .with_protocol("responses")
            .unwrap();
        ModelBackend::set_reasoning_level(&response, "xhigh").unwrap();
        assert_eq!(
            response.payload(&data, &[], None).unwrap()["reasoning"]["effort"],
            "xhigh"
        );
    }

    #[test]
    fn profile_levels_reject_unadvertised_reasoning_tiers() {
        let client = ModelClient::new("http://localhost/v1", None, "mock".into())
            .unwrap()
            .with_reasoning("high".into(), None)
            .with_thinking_levels(vec!["medium".into(), "high".into(), "xhigh".into()]);
        assert_eq!(client.reasoning_level().as_deref(), Some("high"));
        assert!(ModelBackend::set_reasoning_level(&client, "low").is_err());
        assert!(ModelBackend::set_reasoning_level(&client, "off").is_err());
        assert_eq!(client.reasoning_level().as_deref(), Some("high"));
        ModelBackend::set_reasoning_level(&client, "xhigh").unwrap();
        assert_eq!(client.reasoning_level().as_deref(), Some("xhigh"));
        // Resetting to the provider default is never blocked.
        ModelBackend::set_reasoning_level(&client, "auto").unwrap();
        assert_eq!(client.reasoning_level().as_deref(), Some("auto"));
    }

    #[tokio::test]
    async fn healthy_stream_can_outlive_idle_timeout() {
        use std::io::{BufRead, BufReader, Read, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut reader = BufReader::new(&stream);
            let mut size = 0;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    break;
                }
                if let Some(n) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    size = n.trim().parse().unwrap();
                }
            }
            let mut body = vec![0; size];
            reader.read_exact(&mut body).unwrap();
            drop(reader);
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n"
            )
            .unwrap();
            stream.flush().unwrap();
            // Deliberately exceed one idle window in total while every chunk
            // still arrives within it. Leave scheduling headroom for busy CI
            // hosts; the previous 45/95 ms ratio was flaky on macOS runners.
            for (content, reason) in [("Hello", "null"), (" world", "null"), ("!", "\"stop\"")] {
                std::thread::sleep(Duration::from_millis(350));
                write!(stream, "data: {{\"choices\":[{{\"delta\":{{\"content\":\"{content}\"}},\"finish_reason\":{reason}}}]}}\n\n").unwrap();
                stream.flush().unwrap();
            }
            write!(stream, "data: [DONE]\n\n").unwrap();
            stream.flush().unwrap();
        });
        let mut client =
            ModelClient::new(&format!("http://{addr}/v1"), None, "mock".into()).unwrap();
        client.stream_idle_timeout = Duration::from_secs(1);
        let mut output = String::new();
        let result = client
            .complete(&[Message::new("user", "hello")], &mut |s| {
                output.push_str(s)
            })
            .await
            .unwrap();
        assert_eq!(output, "Hello world!");
        assert_eq!(result.message.content.as_deref(), Some("Hello world!"));
        assert!(result.elapsed >= Duration::from_secs(1));
        server.join().unwrap();
    }

    #[tokio::test]
    async fn incomplete_stream_times_out_without_becoming_a_valid_response() {
        use std::io::{BufRead, BufReader, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(&stream);
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    break;
                }
            }
            drop(reader);
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {{\"choices\":[{{\"delta\":{{\"content\":\"partial\"}},\"finish_reason\":null}}]}}\n\n").unwrap();
            stream.flush().unwrap();
            std::thread::sleep(Duration::from_millis(200));
        });
        let mut client =
            ModelClient::new(&format!("http://{addr}/v1"), None, "mock".into()).unwrap();
        client.stream_idle_timeout = Duration::from_millis(55);
        let mut output = String::new();
        let result = client
            .complete(&[Message::new("user", "hello")], &mut |s| {
                output.push_str(s)
            })
            .await;
        assert!(result
            .err()
            .expect("partial stream must fail")
            .to_string()
            .contains("idle timeout"));
        assert_eq!(output, "partial");
        server.join().unwrap();
    }

    #[tokio::test]
    async fn first_stream_timeout_retries_before_any_response_body() {
        use std::io::{BufRead, BufReader, Read, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            for attempt in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(&stream);
                let mut body_size = 0usize;
                loop {
                    let mut header = String::new();
                    reader.read_line(&mut header).unwrap();
                    if header == "\r\n" {
                        break;
                    }
                    if let Some(size) = header.to_ascii_lowercase().strip_prefix("content-length:")
                    {
                        body_size = size.trim().parse().unwrap();
                    }
                }
                let mut body = vec![0; body_size];
                reader.read_exact(&mut body).unwrap();
                drop(reader);
                if attempt == 0 {
                    // The first attempt never sends HTTP headers or SSE;
                    // retrying cannot duplicate a visible answer or tool.
                    std::thread::sleep(Duration::from_millis(130));
                } else {
                    write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {{\"choices\":[{{\"delta\":{{\"content\":\"recovered\"}},\"finish_reason\":\"stop\"}}]}}\n\ndata: [DONE]\n\n").unwrap();
                    stream.flush().unwrap();
                }
            }
        });
        let mut client =
            ModelClient::new(&format!("http://{addr}/v1"), None, "mock".into()).unwrap();
        client.stream_idle_timeout = Duration::from_millis(80);
        let mut text = String::new();
        let result = client
            .complete(&[Message::new("user", "hello")], &mut |s| text.push_str(s))
            .await
            .unwrap();
        assert_eq!(text, "recovered");
        assert_eq!(result.message.content.as_deref(), Some("recovered"));
        server.join().unwrap();
    }

    #[test]
    fn apec_truncated_response_is_never_accepted_as_memory() {
        let body = json!({
            "choices": [{"message": {"role":"assistant","content":"incomplete note"},
                         "finish_reason":"length"}],
            "usage":{"prompt_tokens":10,"completion_tokens":50}
        })
        .to_string();
        assert!(matches!(
            parse_apec_text_response(&body).unwrap(),
            ApecTextAttempt::Truncated(Some(_))
        ));
        assert_eq!(apec_completion_budget(1024, 0), 2048);
        assert_eq!(apec_completion_budget(1024, 1), 4096);
        assert_eq!(apec_completion_budget(1024, 3), 16384);
    }

    #[tokio::test]
    async fn apec_retries_truncated_summary_and_counts_both_requests() {
        use std::io::{BufRead, BufReader, Read, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let mut budgets = Vec::new();
            let mut system_prompts = Vec::new();
            for step in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut reader = BufReader::new(&stream);
                let mut content_length = 0usize;
                loop {
                    let mut header = String::new();
                    reader.read_line(&mut header).unwrap();
                    if header == "\r\n" {
                        break;
                    }
                    if let Some(value) = header.to_ascii_lowercase().strip_prefix("content-length:")
                    {
                        content_length = value.trim().parse().unwrap();
                    }
                }
                let mut body = vec![0u8; content_length];
                reader.read_exact(&mut body).unwrap();
                drop(reader);
                let payload: Value = serde_json::from_slice(&body).unwrap();
                budgets.push(payload["max_tokens"].as_u64().unwrap());
                system_prompts.push(
                    payload["messages"][0]["content"]
                        .as_str()
                        .unwrap()
                        .to_owned(),
                );
                let (reason, text) = if step == 0 {
                    ("length", "partial")
                } else {
                    ("stop", "complete note")
                };
                let reply = json!({
                    "choices": [{"message": {"role":"assistant","content":text},"finish_reason":reason}],
                    "usage": {"prompt_tokens":10,"completion_tokens":20,"total_tokens":30,
                              "prompt_tokens_details":{"cached_tokens":2}}
                }).to_string();
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", reply.len(), reply).unwrap();
                stream.flush().unwrap();
            }
            (budgets, system_prompts)
        });
        let client = ModelClient::new(&format!("http://{addr}/v1"), None, "mock".into()).unwrap();
        let response = client
            .generate_text("Consolidate the memory", "historical evidence", 1024)
            .await
            .unwrap();
        assert_eq!(response.text, "complete note");
        assert_eq!(response.requests, 2);
        let usage = response.usage.unwrap();
        assert_eq!(usage.input, Some(20));
        assert_eq!(usage.output, Some(40));
        assert_eq!(usage.cached, Some(4));
        let (budgets, prompts) = server.join().unwrap();
        assert_eq!(budgets, vec![2048, 4096]);
        assert_eq!(
            prompts,
            vec!["Consolidate the memory"; 2],
            "generic model text generation must not inject APEC instructions"
        );
    }

    #[tokio::test]
    async fn short_title_uses_small_budget_without_touching_coding_reasoning() {
        use std::io::{BufRead, BufReader, Read, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut reader = BufReader::new(&stream);
            let mut len = 0;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    break;
                }
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    len = value.trim().parse().unwrap();
                }
            }
            let mut body = vec![0u8; len];
            reader.read_exact(&mut body).unwrap();
            drop(reader);
            let payload: Value = serde_json::from_slice(&body).unwrap();
            let reply = json!({
                "choices": [{"message":{"role":"assistant","content":"Kroma 介绍"},
                    "finish_reason":"stop"}],
                "usage":{"prompt_tokens":20,"completion_tokens":12}
            })
            .to_string();
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                reply.len(),
                reply
            ).unwrap();
            stream.flush().unwrap();
            payload
        });
        let client = ModelClient::new(&url, None, "deepseek".into())
            .unwrap()
            .with_reasoning("high".into(), None);
        let result = client
            .generate_short_text("Name conversation", "User: 你好", 128)
            .await
            .unwrap();
        assert_eq!(result.text, "Kroma 介绍");
        assert_eq!(result.requests, 1);
        let payload = server.join().unwrap();
        assert_eq!(
            payload["max_tokens"], 128,
            "title must not get APEC's 1024-token floor"
        );
        assert_eq!(payload["reasoning_effort"], "low");
        assert_eq!(payload["stream"], false);
        assert_eq!(client.reasoning_level().as_deref(), Some("high"));
    }

    #[tokio::test]
    async fn truncated_short_title_keeps_measured_usage_and_does_not_retry() {
        use std::io::{BufRead, BufReader, Read, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut reader = BufReader::new(&stream);
            let mut len = 0;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    break;
                }
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    len = value.trim().parse().unwrap();
                }
            }
            let mut body = vec![0u8; len];
            reader.read_exact(&mut body).unwrap();
            drop(reader);
            let request: Value = serde_json::from_slice(&body).unwrap();
            let reply = json!({
                "choices": [{"message":{"role":"assistant","content":"unfinished"},
                    "finish_reason":"length"}],
                "usage":{"prompt_tokens":55,"completion_tokens":128,
                    "prompt_cache_hit_tokens":0}
            })
            .to_string();
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                reply.len(),
                reply
            ).unwrap();
            stream.flush().unwrap();
            request
        });
        let model = ModelClient::new(&url, None, "deepseek".into()).unwrap();
        let result = model
            .generate_short_text("Title", "User: hello", 128)
            .await
            .unwrap();
        assert!(
            result.text.is_empty(),
            "never use a truncated session title"
        );
        assert_eq!(result.requests, 1);
        assert_eq!(
            result.usage.unwrap().output,
            Some(128),
            "retain actual cost"
        );
        assert_eq!(server.join().unwrap()["max_tokens"], 128);
    }

    #[tokio::test]
    async fn deepseek_apec_uses_low_effort_without_changing_coding_effort() {
        use std::io::{BufRead, BufReader, Read, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut reader = BufReader::new(&stream);
            let mut len = 0;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    break;
                }
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    len = value.trim().parse().unwrap();
                }
            }
            let mut bytes = vec![0; len];
            reader.read_exact(&mut bytes).unwrap();
            drop(reader);
            let payload: Value = serde_json::from_slice(&bytes).unwrap();
            let reply = json!({
                "choices": [{"message":{"role":"assistant","content":"summary"},
                    "finish_reason":"stop"}],
                "usage":{"prompt_tokens":12,"completion_tokens":8}
            })
            .to_string();
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                reply.len(),
                reply
            ).unwrap();
            stream.flush().unwrap();
            payload
        });
        let client = ModelClient::new(&url, None, "deepseek".into())
            .unwrap()
            .with_reasoning("high".into(), None);
        let output = client.generate_text("Memory", "Facts", 512).await.unwrap();
        assert_eq!(output.text, "summary");
        let sent = server.join().unwrap();
        assert_eq!(sent["reasoning_effort"], "low");
        assert!(sent.get("thinking").is_none());
        assert_eq!(client.reasoning_level().as_deref(), Some("high"));
    }

    #[test]
    fn deepseek_stream_reasoning_survives_tool_replay_and_serde() {
        let mut stream = CompletionStream::Chat(StreamResult::default());
        let mut visible = String::new();
        let mut live_reasoning = String::new();
        stream
            .apply_with_reasoning(
                r#"{"choices":[{"delta":{"reasoning_content":"Need to "},"finish_reason":null}]}"#,
                &mut |text| visible.push_str(text),
                &mut |text| live_reasoning.push_str(text),
            )
            .unwrap();
        assert_eq!(
            live_reasoning, "Need to ",
            "reasoning must stream before completion"
        );
        stream
            .apply_with_reasoning(
                r#"{"choices":[{"delta":{"reasoning_content":"inspect files","tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"shell","arguments":"{\"command\":\"pwd\"}"}}]},"finish_reason":"tool_calls"}]}"#,
                &mut |text| visible.push_str(text),
                &mut |text| live_reasoning.push_str(text),
            )
            .unwrap();
        let (assistant, _) = stream.finish().unwrap();
        assert!(
            visible.is_empty(),
            "reasoning must not leak into normal output"
        );
        assert_eq!(live_reasoning, "Need to inspect files");
        assert_eq!(assistant.content.as_deref(), Some(""));
        assert_eq!(
            assistant.reasoning_content.as_deref(),
            Some("Need to inspect files")
        );
        let restored: Message =
            serde_json::from_str(&serde_json::to_string(&assistant).unwrap()).unwrap();
        assert_eq!(restored.reasoning_content, assistant.reasoning_content);
        assert_eq!(restored.tool_calls.as_ref().unwrap()[0].id, "call_1");

        let client = ModelClient::new("http://localhost/v1", None, "deepseek".into())
            .unwrap()
            .with_deepseek_thinking(Some("high".into()));
        let mut old_assistant = assistant.clone();
        old_assistant.content = None; // pre-P1 checkpoints used content:null
        let history = [
            Message::new("system", "You are Kroma"),
            Message::new("user", "Inspect"),
            old_assistant.clone(),
            Message::tool("call_1".into(), "pwd result".into()),
        ];
        let payload = client.chat_payload(&history);
        assert_eq!(payload["thinking"]["type"], "enabled");
        assert_eq!(payload["reasoning_effort"], "high");
        assert!(payload.get("tool_choice").is_none());
        assert_eq!(payload["messages"][2]["content"], "");
        assert_eq!(
            payload["messages"][2]["reasoning_content"],
            "Need to inspect files"
        );
        assert_eq!(payload["messages"][2]["tool_calls"][0]["id"], "call_1");
        assert_eq!(payload["messages"][3]["tool_call_id"], "call_1");
        assert_eq!(
            old_assistant.content, None,
            "projection must not rewrite history"
        );
    }

    #[test]
    fn deepseek_reasoning_effort_only_preserves_tool_turn_thoughts() {
        let client = ModelClient::new("http://localhost/v1", None, "deepseek".into())
            .unwrap()
            .with_reasoning("high".into(), None);
        let mut assistant = Message::new("assistant", "Checking the result");
        assistant.reasoning_content = Some("Already inspected".into());
        let history = [
            Message::new("system", "Code"),
            Message::new("user", "Fix it"),
            assistant,
            Message::tool("call1".into(), "result".into()),
        ];
        let tools = crate::tools::definitions();
        let payload = client.payload(&history, &tools, None).unwrap();
        assert_eq!(payload["reasoning_effort"], "high");
        assert_eq!(
            payload["messages"][2]["reasoning_content"],
            "Already inspected"
        );
        assert_eq!(payload["messages"][2]["content"], "Checking the result");
    }

    #[test]
    fn deepseek_empty_tool_reasoning_is_explicit_but_sol_is_not() {
        let mut with_tool = Message::new("assistant", "");
        with_tool.tool_calls = Some(vec![ToolCall {
            id: "c1".into(),
            kind: "function".into(),
            function: FunctionCall {
                name: "shell".into(),
                arguments: r#"{"command":"pwd"}"#.into(),
            },
        }]);
        let history = vec![
            Message::new("system", "Code"),
            Message::new("user", "Read a file"),
            with_tool,
            Message::tool("c1".into(), "ok".into()),
        ];
        let defs = crate::tools::definitions();
        let deepseek = ModelClient::new("http://localhost/v1", None, "deepseek".into())
            .unwrap()
            .with_reasoning("high".into(), None);
        let sol = ModelClient::new("http://localhost/v1", None, "gpt-6.1-sol".into())
            .unwrap()
            .with_reasoning("medium".into(), None);
        let d = deepseek.chat_payload_with_tools(&history, &defs);
        let s = sol.chat_payload_with_tools(&history, &defs);
        assert_eq!(d["messages"][2]["reasoning_content"], "");
        assert!(s["messages"][2].get("reasoning_content").is_none());
    }

    #[test]
    fn deepseek_replay_does_not_leak_old_model_thoughts() {
        let client = ModelClient::new("http://localhost/v1", None, "deepseek".into())
            .unwrap()
            .with_deepseek_thinking(Some("high".into()));
        let mut previous_model = Message::new("assistant", "Old model answer");
        previous_model.reasoning_content = Some("private unrelated reasoning".into());
        let mut current_model = Message::new("assistant", "Using tools");
        current_model.reasoning_content = Some("current reasoning".into());
        let history = [
            Message::new("system", "Code"),
            Message::new("user", "Earlier question"),
            previous_model,
            Message::new("user", "Current question"),
            current_model,
        ];
        let payload = client.chat_payload_with_tools(&history, &crate::tools::definitions());
        assert!(payload["messages"][2].get("reasoning_content").is_none());
        assert_eq!(
            payload["messages"][4]["reasoning_content"],
            "current reasoning"
        );
    }

    #[test]
    fn deepseek_replays_tagged_reasoning_across_user_turns_and_resume() {
        let client = ModelClient::new("http://localhost/v1", None, "deepseek".into())
            .unwrap()
            .with_reasoning("high".into(), None);
        let mut first = Message::new("assistant", "First answer, no tools");
        first.reasoning_content = Some("First thought".into());
        client.tag_chat_reasoning(&mut first);
        let first: Message = serde_json::from_str(&serde_json::to_string(&first).unwrap()).unwrap();
        let mut second = Message::new("assistant", "I will inspect");
        second.reasoning_content = Some("Second thought".into());
        client.tag_chat_reasoning(&mut second);
        let history = [
            Message::new("system", "Code"),
            Message::new("user", "First request"),
            first,
            Message::new("user", "Second request"),
            second,
        ];
        let payload = client.chat_payload_with_tools(&history, &crate::tools::definitions());
        assert_eq!(payload["messages"][2]["reasoning_content"], "First thought");
        assert_eq!(
            payload["messages"][4]["reasoning_content"],
            "Second thought"
        );
        assert!(payload["messages"][2].get("provider_items").is_none());
    }

    #[test]
    fn deepseek_reasoning_origin_prevents_cross_model_leakage() {
        let original = ModelClient::new("http://localhost/v1", None, "deepseek".into())
            .unwrap()
            .with_reasoning("high".into(), None);
        let other = ModelClient::new("http://localhost/v1", None, "deepseek-other".into())
            .unwrap()
            .with_reasoning("high".into(), None);
        let mut message = Message::new("assistant", "Done");
        message.reasoning_content = Some("Private to original".into());
        original.tag_chat_reasoning(&mut message);
        let history = [
            Message::new("user", "Old request"),
            message,
            Message::new("user", "New request"),
        ];
        let payload = other.chat_payload_with_tools(&history, &crate::tools::definitions());
        assert!(payload["messages"][1].get("reasoning_content").is_none());
        // Even the original model must not send thoughts if thinking is off.
        original.set_reasoning_level("off").unwrap();
        let disabled = original.chat_payload_with_tools(&history, &crate::tools::definitions());
        assert!(disabled["messages"][1].get("reasoning_content").is_none());
    }

    #[test]
    fn usage_reports_reasoning_as_part_of_output() {
        let usage = Usage::from_value(&json!({
            "prompt_tokens": 100, "completion_tokens": 40,
            "completion_tokens_details": {"reasoning_tokens": 17}
        }))
        .unwrap();
        assert_eq!(usage.output, Some(40));
        assert_eq!(usage.reasoning, Some(17));
    }

    #[test]
    fn thinking_deltas_are_not_displayed_as_assistant_text() {
        let mut stream = StreamResult::default();
        let mut visible = String::new();
        stream
            .apply(
                r#"{"choices":[{"delta":{"reasoning_content":"internal only"},"finish_reason":null}]}"#,
                &mut |text| visible.push_str(text),
            )
            .unwrap();
        stream
            .apply(
                r#"{"choices":[{"delta":{"content":"visible"},"finish_reason":"stop"}]}"#,
                &mut |text| visible.push_str(text),
            )
            .unwrap();
        let (assistant, _) = stream.finish().unwrap();
        assert_eq!(visible, "visible");
        assert_eq!(assistant.content.as_deref(), Some("visible"));
        assert_eq!(
            assistant.reasoning_content.as_deref(),
            Some("internal only")
        );
        let client = ModelClient::new("http://localhost/v1", None, "plain".into()).unwrap();
        let payload = client.chat_payload(&[assistant]);
        assert!(payload.get("thinking").is_none());
        assert!(payload.get("reasoning_effort").is_none());
        assert!(payload.get("tool_choice").is_none());
    }

    #[test]
    fn duplicate_tool_ids_are_rejected_before_any_command_executes() {
        let mut stream = StreamResult::default();
        stream.apply(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"duplicate","type":"function","function":{"name":"shell","arguments":"{\"command\":\"echo 1\"}"}},{"index":1,"id":"duplicate","type":"function","function":{"name":"shell","arguments":"{\"command\":\"echo 2\"}"}}]},"finish_reason":"tool_calls"}]}"#,
            &mut |_| {},
        ).unwrap();
        assert!(stream
            .finish()
            .unwrap_err()
            .to_string()
            .contains("duplicate tool_call_id"));
    }

    #[test]
    fn malformed_chat_tool_arguments_are_rejected_before_persisting_assistant() {
        let mut stream = StreamResult::default();
        stream.apply(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_bad","type":"function","function":{"name":"shell","arguments":"{bad json"}}]},"finish_reason":"tool_calls"}]}"#,
            &mut |_| {},
        ).unwrap();
        assert!(
            stream.finish().is_err(),
            "invalid JSON tool arguments must not become durable assistant tool_calls"
        );
    }

    #[test]
    fn chat_tool_arguments_must_be_objects_not_arrays_or_scalars() {
        for arguments in ["[]", "42", "null", "\"\""] {
            let mut stream = StreamResult::default();
            let event = json!({"choices":[{"delta":{"tool_calls":[{
                "index":0, "id":"bad", "type":"function",
                "function":{"name":"shell", "arguments":arguments}
            }]},"finish_reason":"tool_calls"}]});
            stream.apply(&event.to_string(), &mut |_| {}).unwrap();
            assert!(stream.finish().is_err(), "accepted {arguments}");
        }
        let mut stream = StreamResult::default();
        let event = json!({"choices":[{"delta":{"tool_calls":[{
            "index":0, "id":"good", "type":"function",
            "function":{"name":"shell", "arguments":"{}"}
        }]},"finish_reason":"tool_calls"}]});
        stream.apply(&event.to_string(), &mut |_| {}).unwrap();
        assert_eq!(stream.finish().unwrap().0.tool_calls.unwrap().len(), 1);
    }

    #[test]
    fn reassembles_streaming_tool_calls() {
        let mut stream = StreamResult::default();
        let mut text = String::new();
        let mut sink = |fragment: &str| text.push_str(fragment);
        stream.apply(
            r#"{"choices":[{"delta":{"content":"Executing ","tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"shell","arguments":"{\"command\":\""}}]},"finish_reason":null}]}"#,
            &mut sink,
        ).unwrap();
        stream.apply(
            r#"{"choices":[{"delta":{"content":"now","tool_calls":[{"index":0,"function":{"arguments":"pwd\"}"}}]},"finish_reason":null}]}"#,
            &mut sink,
        ).unwrap();
        stream
            .apply(
                r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
                &mut sink,
            )
            .unwrap();
        stream.apply("[DONE]", &mut sink).unwrap();
        let (message, usage) = stream.finish().unwrap();
        assert!(usage.is_none());
        assert_eq!(text, "Executing now");
        assert_eq!(message.content.as_deref(), Some("Executing now"));
        let calls = message.tool_calls.unwrap();
        assert_eq!(calls[0].id, "call_1");
        assert_eq!(calls[0].function.name, "shell");
        assert_eq!(calls[0].function.arguments, r#"{"command":"pwd"}"#);
    }

    #[test]
    fn task_image_cache_preserves_full_wire_payload_across_all_protocols() -> Result<()> {
        use std::fs;
        let directory = std::env::temp_dir().join(format!(
            "kroma-payload-image-cache-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos()
        ));
        fs::create_dir_all(&directory)?;
        let path = directory.join("screenshot.png");
        fs::write(&path, b"original")?;
        let mut user = Message::new("user", "Compare this \u{e000}");
        user.provider_items = Some(vec![json!({
            "type":"kroma_attachment", "kind":"image", "name":"screenshot.png",
            "path":path, "mime":"image/png", "bytes":8, "marker":"\u{e000}"
        })]);
        let messages = [Message::new("system", "You are Kroma"), user];
        for protocol in ["chat", "responses", "anthropic"] {
            let client = ModelClient::new("http://127.0.0.1:9/v1", None, "mock".into())?
                .with_protocol(protocol)?;
            let cache = TaskImageCache::default();
            let normal = client.payload(&messages, &[], None)?;
            let cached = client.payload_with_images(&messages, &[], None, Some(&cache))?;
            assert_eq!(
                normal, cached,
                "cold cache must preserve {protocol} payload"
            );
            assert_eq!(cache.encoded.lock().unwrap().len(), 1);
            fs::write(&path, b"different")?;
            let next_round = client.payload_with_images(&messages, &[], None, Some(&cache))?;
            assert_eq!(
                next_round, cached,
                "{protocol} must reuse current task bytes"
            );
            let new_task = client.payload_with_images(
                &messages,
                &[],
                None,
                Some(&TaskImageCache::default()),
            )?;
            assert_ne!(new_task, cached, "{protocol} starts a fresh cache");
            fs::write(&path, b"original")?;
        }
        fs::remove_dir_all(&directory)?;
        Ok(())
    }

    #[test]
    fn sse_incremental_scanner_preserves_line_boundaries_across_chunks() -> Result<()> {
        let source = b"data: one\r\n\r\ndata: two\n\n: comment\n\nleftover";
        let expected: Vec<Vec<u8>> = source
            .split_inclusive(|byte| *byte == b'\n')
            .filter(|line| line.ends_with(b"\n"))
            .map(|line| line[..line.len() - 1].to_vec())
            .collect();
        for stride in [1, 2, 3, 5, 64] {
            let mut pending = Vec::new();
            let mut lines = Vec::new();
            for chunk in source.chunks(stride) {
                let done = consume_sse_lines(&mut pending, chunk, |line| {
                    lines.push(line.to_vec());
                    Ok(false)
                })?;
                assert!(!done);
            }
            assert_eq!(lines, expected, "chunk size={stride}");
            assert_eq!(pending, b"leftover", "trailing partial line retained");
        }
        let mut pending = Vec::new();
        let mut calls = 0;
        let lines = b"first\nsecond\nthird\n";
        assert!(consume_sse_lines(&mut pending, lines, |_| {
            calls += 1;
            Ok(calls == 2)
        })?);
        assert_eq!(calls, 2, "terminal event must stop parsing immediately");
        Ok(())
    }

    #[test]
    fn sse_handles_crlf_and_data_lines() {
        let mut stream = CompletionStream::Chat(StreamResult::default());
        let mut lines = Vec::new();
        let mut data_bytes = 0;
        let mut tokens = String::new();
        let mut sink = |text: &str| tokens.push_str(text);
        process_sse_line(
            br#"data: {"choices":[{"delta":{"content":"Hello"},"finish_reason":"stop"}]}"#,
            &mut lines,
            &mut data_bytes,
            &mut stream,
            &mut sink,
        )
        .unwrap();
        process_sse_line(b"\r", &mut lines, &mut data_bytes, &mut stream, &mut sink).unwrap();
        process_sse_line(
            b"data: [DONE]",
            &mut lines,
            &mut data_bytes,
            &mut stream,
            &mut sink,
        )
        .unwrap();
        process_sse_line(b"", &mut lines, &mut data_bytes, &mut stream, &mut sink).unwrap();
        assert_eq!(tokens, "Hello");
        let (message, _) = stream.finish().unwrap();
        assert_eq!(message.content.as_deref(), Some("Hello"));
    }

    #[test]
    fn incomplete_stream_is_not_treated_as_success() {
        let mut stream = StreamResult::default();
        stream
            .apply(
                r#"{"choices":[{"delta":{"content":"partial"},"finish_reason":null}]}"#,
                &mut |_| {},
            )
            .unwrap();
        assert!(stream.finish().is_err());
    }

    #[test]
    fn usage_only_sse_chunk_preserves_token_counts() {
        let mut stream = StreamResult::default();
        stream
            .apply(
                r#"{"choices":[{"delta":{"content":"ok"},"finish_reason":"stop"}]}"#,
                &mut |_| {},
            )
            .unwrap();
        stream
            .apply(
                r#"{"choices":[],"usage":{"prompt_tokens":130,"completion_tokens":11,"total_tokens":141,"prompt_tokens_details":{"cached_tokens":100}}}"#,
                &mut |_| {},
            )
            .unwrap();
        stream.apply("[DONE]", &mut |_| {}).unwrap();
        let (message, usage) = stream.finish().unwrap();
        assert_eq!(message.content.as_deref(), Some("ok"));
        let usage = usage.unwrap();
        assert_eq!(usage.input, Some(130));
        assert_eq!(usage.output, Some(11));
        assert_eq!(usage.total, Some(141));
        assert_eq!(usage.cached, Some(100));
    }

    #[test]
    fn deepseek_cache_usage_is_not_assumed_when_absent() {
        let usage = Usage::from_value(&json!({
            "prompt_tokens": 150,
            "completion_tokens": 12,
            "prompt_cache_hit_tokens": 117,
            "prompt_cache_miss_tokens": 33
        }))
        .unwrap();
        assert_eq!(usage.cached, Some(117));
        let unknown = Usage::from_value(&json!({"prompt_tokens": 150})).unwrap();
        assert_eq!(unknown.cached, None);
    }

    #[test]
    fn upstream_error_event_is_not_treated_as_success() {
        let mut stream = StreamResult::default();
        let result = stream.apply(
            r#"{"error":{"message":"model is unavailable"}}"#,
            &mut |_| {},
        );
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("model is unavailable"));
    }

    #[test]
    fn tool_result_contains_call_id() {
        let msg = Message::tool("id-7".into(), "ok".into());
        let value = serde_json::to_value(&msg).unwrap();
        assert_eq!(value["role"], "tool");
        assert_eq!(value["tool_call_id"], "id-7");
        assert!(value.get("tool_calls").is_none());
    }

    #[test]
    fn unknown_finish_reason_is_rejected() {
        let mut result = StreamResult::default();
        result
            .apply(
                r#"{"choices":[{"delta":{"content":"partial"},"finish_reason":"other"}]}"#,
                &mut |_| {},
            )
            .unwrap();
        result.apply("[DONE]", &mut |_| {}).unwrap();
        assert!(result.finish().is_err());
    }

    #[test]
    fn model_metadata_does_not_guess_missing_limits() {
        let info = ModelInfo::parse(&json!({
            "id":"example","max_input_tokens":200_000,"max_tokens":32_768,
            "capabilities":{
                "thinking":{"types":{"adaptive":{"supported":true},"disabled":{"supported":false}}},
                "effort":{"high":{"supported":true},"max":{"supported":false}}
            }
        }));
        assert_eq!(info.context_window, Some(200_000));
        assert_eq!(info.max_output_tokens, Some(32_768));
        assert_eq!(info.supports("/thinking/types/adaptive"), Some(true));
        assert_eq!(info.supports("/effort/max"), Some(false));
        assert!(ModelInfo::parse(&json!({"id":"no-limits"}))
            .context_window
            .is_none());
        let mut client = ModelClient::new("https://api.example/v1", None, "model".into()).unwrap();
        client.model_info = info.clone();
        client.output_limit = info.max_output_tokens;
        assert_eq!(client.effective_output_tokens(), Some(4096));
        client = client.with_output_tokens(Some(80_000));
        assert_eq!(client.effective_output_tokens(), Some(32_768));
        assert_eq!(
            client
                .payload(&[Message::new("user", "hi")], &[], None)
                .unwrap()["max_tokens"],
            32_768
        );
        let uncapped = ModelClient::new("https://api.example/v1", None, "model".into())
            .unwrap()
            .with_output_tokens(Some(128_000));
        assert_eq!(uncapped.effective_output_tokens(), Some(32_768));
    }

    #[test]
    fn opaque_reasoning_is_not_replayed_after_a_model_switch() {
        for protocol in ["responses", "anthropic"] {
            let first = ModelClient::new("https://api.example/v1", None, "old-model".into())
                .unwrap()
                .with_protocol(protocol)
                .unwrap();
            let replacement = ModelClient::new("https://api.example/v1", None, "new-model".into())
                .unwrap()
                .with_protocol(protocol)
                .unwrap();
            let mut assistant = Message::new("assistant", "completed");
            assistant.provider_items = Some(vec![if protocol == "responses" {
                json!({"type":"reasoning","id":"rs_opaque","encrypted_content":"opaque-secret"})
            } else {
                json!({"type":"thinking","thinking":"analysis","signature":"opaque-secret"})
            }]);
            first.tag_opaque_reasoning(&mut assistant);
            let history = vec![
                Message::new("system", "system"),
                Message::new("user", "first"),
                assistant.clone(),
                Message::new("user", "next"),
            ];
            let raw = serde_json::to_string(&history).unwrap();
            assert!(
                first
                    .payload(&history, &[], None)
                    .unwrap()
                    .to_string()
                    .contains("opaque-secret"),
                "same model {protocol} must preserve reasoning continuity"
            );
            assert!(
                !replacement
                    .payload(&history, &[], None)
                    .unwrap()
                    .to_string()
                    .contains("opaque-secret"),
                "other model must not receive incompatible opaque reasoning"
            );
            assert_eq!(
                serde_json::to_string(&history).unwrap(),
                raw,
                "projection must never rewrite the authoritative session"
            );
            // A legacy untagged message can still complete the immediate
            // tool-use turn, but must not carry unknown provenance forward.
            let mut legacy = assistant;
            legacy
                .provider_items
                .as_mut()
                .unwrap()
                .retain(|item| item["type"] != "kroma_opaque_reasoning_origin");
            let legacy_history = vec![
                Message::new("user", "first"),
                legacy,
                Message::new("user", "next"),
            ];
            assert!(!first
                .payload(&legacy_history, &[], None)
                .unwrap()
                .to_string()
                .contains("opaque-secret"));
        }
    }

    #[test]
    fn protocol_specific_payloads_do_not_cross_contaminate() {
        let old = ModelClient::new("https://api.example/v1", None, "model".into()).unwrap();
        let mut assistant = Message::new("assistant", "hello");
        assistant.provider_items = Some(vec![
            json!({"type":"reasoning","id":"rs1","encrypted_content":"secret"}),
        ]);
        assert!(old.chat_payload(&[assistant.clone()])["messages"][0]
            .get("provider_items")
            .is_none());
        let responses = old.clone().with_protocol("responses").unwrap();
        let payload = responses.payload(&[assistant], &[], None).unwrap();
        assert_eq!(payload["input"][0]["type"], "reasoning");
        assert_eq!(payload["stream"], true);
        assert_eq!(payload["max_output_tokens"], 4096);
        let anthropic = old.with_protocol("anthropic").unwrap();
        let payload = anthropic
            .payload(&[Message::new("user", "hello")], &[], None)
            .unwrap();
        assert_eq!(payload["messages"][0]["role"], "user");
        assert_eq!(payload["max_tokens"], 4096);
    }

    #[tokio::test]
    async fn discovery_uses_upstream_model_capabilities_and_authentication() {
        use std::io::{BufRead, BufReader, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut reader = BufReader::new(&stream);
            let mut lines = Vec::new();
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    break;
                }
                lines.push(line);
            }
            assert!(
                lines[0].starts_with("GET /v1/models/provider%2Fmodel "),
                "request: {:?}",
                lines[0]
            );
            assert!(lines
                .iter()
                .any(|line| line.to_ascii_lowercase().starts_with("x-api-key: test-key")));
            assert!(lines
                .iter()
                .any(|line| line.to_ascii_lowercase().starts_with("anthropic-version:")));
            let body = json!({"max_input_tokens":180000,"max_tokens":16000,
                "capabilities":{"thinking":{"types":{"adaptive":{"supported":true}}}}})
            .to_string();
            write!(stream,"HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{}",body.len(),body).unwrap();
            stream.flush().unwrap();
        });
        let mut client = ModelClient::new(
            &format!("http://{addr}/v1"),
            Some("test-key".into()),
            "provider/model".into(),
        )
        .unwrap()
        .with_protocol("anthropic")
        .unwrap();
        let info = client.discover_model().await.unwrap();
        assert_eq!(info.context_window, Some(180000));
        assert_eq!(info.max_output_tokens, Some(16000));
        assert_eq!(client.effective_output_tokens(), Some(4096));
        let payload = client
            .payload(&[Message::new("user", "hi")], &[], None)
            .unwrap();
        assert_eq!(payload["thinking"]["type"], "adaptive");
        server.join().unwrap();
    }

    #[tokio::test]
    async fn metadata_missing_or_rejected_uses_explicit_fallback_without_guessing() {
        use std::io::{BufRead, BufReader, Write};
        use std::net::TcpListener;
        for (status, data) in [
            ("200 OK", r#"{"id":"mock-model"}"#),
            (
                "403 Forbidden",
                r#"{"error":"no permission for model metadata"}"#,
            ),
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            let server = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut reader = BufReader::new(&stream);
                let mut first = String::new();
                reader.read_line(&mut first).unwrap();
                assert!(first.starts_with("GET /v1/models/mock-model "));
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" {
                        break;
                    }
                }
                drop(reader);
                write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{data}",
                    data.len()
                )
                .unwrap();
                stream.flush().unwrap();
            });
            let mut client =
                ModelClient::new(&format!("http://{addr}/v1"), None, "mock-model".into())
                    .unwrap()
                    .with_protocol("responses")
                    .unwrap();
            let metadata = client.discover_model().await.unwrap();
            assert_eq!(metadata.context_window, None);
            assert_eq!(metadata.max_output_tokens, None);
            assert_eq!(client.effective_output_tokens(), None);
            assert_eq!(
                client
                    .payload(&[Message::new("user", "test")], &[], None)
                    .unwrap()["max_output_tokens"],
                4096
            );
            server.join().unwrap();
        }
    }
}
