use crate::budget::BudgetConfig;
use crate::messages::{
    AssistantTurn, FinishReason, ReasoningBlock, ToolCall, TranscriptItem,
};
use crate::provider::sse::{parse_usage_json, SseStreamAssembler, MAX_SSE_RESPONSE_BYTES};
use crate::provider::transport::{
    pump_body, read_error_body, read_full_body, send_request, BodyFailure, HttpRequestSpec,
    TransportFailure,
};
use crate::provider::{ModelProvider, ProviderCompletion, ProviderError, ProviderRequest};
use crate::tool::definition::ToolDefinition;
use crate::types::ToolCallId;
use serde_json::{json, Value};
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, Instant};
use tokio::sync::watch;

pub use crate::provider::endpoint::{validate_base_url, ValidatedEndpoint};

#[derive(Clone, Default)]
pub struct ChatCompletionsConfig {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub max_retries: usize,
    pub reasoning_effort: Option<String>,
}

impl fmt::Debug for ChatCompletionsConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChatCompletionsConfig")
            .field("base_url", &self.base_url)
            .field("api_key", &"Bearer ***")
            .field("model", &self.model)
            .field("max_retries", &self.max_retries)
            .field("reasoning_effort", &self.reasoning_effort)
            .finish()
    }
}

/// Deterministic serialization of the OpenAI Chat Completions request body:
/// - `n = 1` fixed
/// - `tools` strictly sorted by ASCII name with deterministic JSON key ordering
/// - `reasoning_blocks` preserved in assistant messages for 2026 reasoning models
pub fn serialize_openai_request_body(
    model: &str,
    system_prompt: &str,
    tools: &[ToolDefinition],
    messages: &[TranscriptItem],
    stream: bool,
) -> String {
    serialize_openai_request_body_with_effort(model, system_prompt, tools, messages, stream, None)
}

pub fn serialize_openai_request_body_with_effort(
    model: &str,
    system_prompt: &str,
    tools: &[ToolDefinition],
    messages: &[TranscriptItem],
    stream: bool,
    reasoning_effort: Option<&str>,
) -> String {
    serialize_openai_request_body_with_limit(
        model,
        system_prompt,
        tools,
        messages,
        stream,
        reasoning_effort,
        None,
    )
}

/// F08 (`v1.4.0`): same as [`serialize_openai_request_body_with_effort`] but also emits the
/// remaining output-token allowance as `max_tokens`, so a single completion can never overshoot the
/// run's token budget.
pub fn serialize_openai_request_body_with_limit(
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
                "function": {
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.parameters,
                }
            })
        })
        .collect();

    let mut wire_messages: Vec<Value> = Vec::with_capacity(messages.len() + 1);
    wire_messages.push(json!({
        "role": "system",
        "content": system_prompt,
    }));

    // DeepSeek thinking models require `reasoning_content` to be echoed back only for the
    // active (unfinished) turn. Replaying plain-text reasoning from turns before the last
    // user message pollutes the context, so gate `reasoning_content` on `idx >= last_user_idx`.
    // Signature/opaque blocks are still preserved verbatim for every assistant turn.
    let active_reasoning_from = messages
        .iter()
        .rposition(|item| matches!(item, TranscriptItem::User(_)))
        .unwrap_or(0);

    for (idx, item) in messages.iter().enumerate() {
        match item {
            TranscriptItem::User(u) => {
                wire_messages.push(json!({
                    "role": "user",
                    "content": u.content,
                }));
            }
            TranscriptItem::Assistant(a) => {
                let mut msg = serde_json::Map::new();
                msg.insert("role".to_string(), Value::String("assistant".to_string()));
                match &a.content {
                    Some(c) => {
                        msg.insert("content".to_string(), Value::String(c.clone()));
                    }
                    None => {
                        msg.insert("content".to_string(), Value::Null);
                    }
                }
                if !a.reasoning_blocks.is_empty() {
                    if idx >= active_reasoning_from {
                        let combined_text: String = a
                            .reasoning_blocks
                            .iter()
                            .filter_map(|b| b.text.as_deref())
                            .collect::<Vec<_>>()
                            .join("\n");
                        if !combined_text.is_empty() {
                            msg.insert(
                                "reasoning_content".to_string(),
                                Value::String(combined_text),
                            );
                        }
                    }
                    let has_signed_or_opaque = a
                        .reasoning_blocks
                        .iter()
                        .any(|b| b.signature.is_some() || b.opaque_payload.is_some());
                    if has_signed_or_opaque {
                        let thinking_items: Vec<Value> = a
                            .reasoning_blocks
                            .iter()
                            .map(|b| {
                                let mut obj = serde_json::Map::new();
                                if b.text.is_none() && b.opaque_payload.is_some() {
                                    obj.insert(
                                        "type".to_string(),
                                        Value::String("redacted_thinking".to_string()),
                                    );
                                } else {
                                    obj.insert(
                                        "type".to_string(),
                                        Value::String("thinking".to_string()),
                                    );
                                }
                                if let Some(t) = &b.text {
                                    obj.insert("thinking".to_string(), Value::String(t.clone()));
                                }
                                if let Some(s) = &b.signature {
                                    obj.insert("signature".to_string(), Value::String(s.clone()));
                                }
                                if let Some(op) = &b.opaque_payload {
                                    obj.insert("data".to_string(), Value::String(op.clone()));
                                }
                                Value::Object(obj)
                            })
                            .collect();
                        msg.insert("thinking".to_string(), Value::Array(thinking_items));
                    }
                }
                if !a.tool_calls.is_empty() {
                    let tc_list: Vec<Value> = a
                        .tool_calls
                        .iter()
                        .map(|tc| {
                            json!({
                                "id": tc.call_id.as_str(),
                                "type": "function",
                                "function": {
                                    "name": tc.name,
                                    "arguments": tc.raw_arguments,
                                }
                            })
                        })
                        .collect();
                    msg.insert("tool_calls".to_string(), Value::Array(tc_list));
                }
                wire_messages.push(Value::Object(msg));
            }
            TranscriptItem::ToolResult(tr) => {
                // Gate 7 (`v1.2.0`): a tool result carrying image attachments is sent as a content
                // part array so a vision-capable model receives the actual pixels, not just the
                // one-line summary that stays in the text part.
                if tr.meta.image_attachments.is_empty() {
                    wire_messages.push(json!({
                        "role": "tool",
                        "tool_call_id": tr.call_id.as_str(),
                        "name": tr.tool_name,
                        "content": tr.content.as_ref(),
                    }));
                } else {
                    let mut parts: Vec<Value> = vec![json!({
                        "type": "text",
                        "text": tr.content.as_ref(),
                    })];
                    for image in &tr.meta.image_attachments {
                        parts.push(json!({
                            "type": "image_url",
                            "image_url": { "url": image.data_url() },
                        }));
                    }
                    wire_messages.push(json!({
                        "role": "tool",
                        "tool_call_id": tr.call_id.as_str(),
                        "name": tr.tool_name,
                        "content": parts,
                    }));
                }
            }
        }
    }

    let mut root = serde_json::Map::new();
    root.insert("model".to_string(), Value::String(model.to_string()));
    root.insert("n".to_string(), Value::Number(1.into()));
    if let Some(effort) = reasoning_effort.map(str::trim).filter(|s| !s.is_empty()) {
        root.insert(
            "reasoning_effort".to_string(),
            Value::String(effort.to_string()),
        );
    }
    root.insert("messages".to_string(), Value::Array(wire_messages));
    if !tools_json.is_empty() {
        root.insert("tools".to_string(), Value::Array(tools_json));
    }
    if let Some(limit) = max_output_tokens {
        root.insert("max_tokens".to_string(), Value::Number(limit.into()));
    }
    root.insert("stream".to_string(), Value::Bool(stream));
    if stream {
        root.insert(
            "stream_options".to_string(),
            json!({ "include_usage": true }),
        );
    }

    serde_json::to_string(&Value::Object(root)).unwrap_or_else(|_| "{}".to_string())
}

pub fn parse_non_streaming_response(body: &str) -> Result<AssistantTurn, ProviderError> {
    let val: Value = serde_json::from_str(body).map_err(|e| {
        ProviderError::Transport(format!("invalid JSON in non-stream response: {}", e))
    })?;

    let choices = val
        .get("choices")
        .and_then(|c| c.as_array())
        .ok_or_else(|| ProviderError::Transport("missing choices array in response".to_string()))?;

    let choice0 = choices
        .first()
        .ok_or_else(|| ProviderError::Transport("empty choices array in response".to_string()))?;

    let finish_reason = choice0
        .get("finish_reason")
        .and_then(|v| v.as_str())
        .map(FinishReason::from_wire)
        .unwrap_or(FinishReason::Unknown("missing".to_string()));

    let message = choice0
        .get("message")
        .ok_or_else(|| ProviderError::Transport("missing message in choice".to_string()))?;

    let content = message
        .get("content")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let mut reasoning_blocks: Vec<ReasoningBlock> = Vec::new();
    if let Some(arr) = message
        .get("reasoning_blocks")
        .or_else(|| message.get("thinking").filter(|v| v.is_array()))
        .and_then(|v| v.as_array())
    {
        for item in arr {
            let text = item
                .get("text")
                .or_else(|| item.get("thinking"))
                .or_else(|| item.get("reasoning_content"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let signature = item
                .get("signature")
                .or_else(|| item.get("thought_signature"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let opaque_payload = item
                .get("opaque_payload")
                .or_else(|| item.get("data"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            if text.is_some() || signature.is_some() || opaque_payload.is_some() {
                reasoning_blocks.push(ReasoningBlock {
                    text,
                    signature,
                    opaque_payload,
                });
            }
        }
    } else {
        let text = message
            .get("reasoning_content")
            .or_else(|| message.get("reasoning"))
            .or_else(|| message.get("thinking"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let signature = message
            .get("signature")
            .or_else(|| message.get("thought_signature"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let opaque_payload = message
            .get("opaque_payload")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        if text.is_some() || signature.is_some() || opaque_payload.is_some() {
            reasoning_blocks.push(ReasoningBlock {
                text,
                signature,
                opaque_payload,
            });
        }
    }

    let mut tool_calls = Vec::new();
    if let Some(tc_arr) = message.get("tool_calls").and_then(|v| v.as_array()) {
        for tc in tc_arr {
            let id = tc
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let func = tc.get("function");
            let name = func
                .and_then(|f| f.get("name"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let raw_arguments = func
                .and_then(|f| f.get("arguments"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            tool_calls.push(ToolCall {
                call_id: ToolCallId::new(id),
                name,
                raw_arguments,
            });
        }
    }

    let usage = val.get("usage").and_then(parse_usage_json);

    Ok(AssistantTurn {
        content,
        reasoning_blocks,
        tool_calls,
        finish_reason,
        usage,
    })
}

/// OpenAI-compatible HTTP/1.1 + SSE Client supporting bounded retries, `Retry-After`,
/// redirect prohibition, and URL security validation (§4.1–§4.3).
pub struct ChatCompletionsClient {
    config: ChatCompletionsConfig,
    endpoint: ValidatedEndpoint,
}

impl ChatCompletionsClient {
    pub fn new(config: ChatCompletionsConfig) -> Result<Self, ProviderError> {
        // Reject CR/LF in values that reach the HTTP request line/headers to prevent
        // header injection: `api_key` is written into the `Authorization` header and
        // `model` is interpolated into the request body.
        // Reject any ASCII control characters (including CR, LF, NUL, DEL) in values that reach
        // HTTP headers or the request body.
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
}

impl ModelProvider for ChatCompletionsClient {
    fn complete<'a>(
        &'a self,
        req: ProviderRequest,
        budget: &'a BudgetConfig,
        mut cancel_rx: watch::Receiver<bool>,
    ) -> Pin<Box<dyn Future<Output = Result<ProviderCompletion, ProviderError>> + Send + 'a>> {
        Box::pin(async move {
            let body_json = serialize_openai_request_body_with_limit(
                &self.config.model,
                &req.system_prompt,
                &req.tools,
                &req.messages,
                req.stream,
                self.config.reasoning_effort.as_deref(),
                req.max_output_tokens,
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
                    res = self.execute_single_http_attempt(&body_json, req.stream, budget, attempt_start) => res,
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
                    Err(AttemptFailure::Fatal(err)) => {
                        return Err(err);
                    }
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

impl ChatCompletionsClient {
    async fn execute_single_http_attempt(
        &self,
        body_json: &str,
        stream: bool,
        budget: &BudgetConfig,
        attempt_start: Instant,
    ) -> Result<(AssistantTurn, u64, Option<u64>), AttemptFailure> {
        let request_deadline = attempt_start + budget.request_timeout;
        let accept_hdr = if stream {
            "text/event-stream"
        } else {
            "application/json"
        };
        let path = if self.endpoint.base_path.ends_with("/chat/completions") {
            self.endpoint.base_path.clone()
        } else if self.endpoint.base_path.ends_with("/v1") {
            format!("{}/chat/completions", self.endpoint.base_path)
        } else {
            format!("{}/v1/chat/completions", self.endpoint.base_path)
        };
        let headers = vec![(
            "Authorization".to_string(),
            format!("Bearer {}", self.config.api_key),
        )];
        let spec = HttpRequestSpec {
            method: "POST",
            path: &path,
            headers: &headers,
            body: body_json,
            accept: accept_hdr,
        };

        let mut head = send_request(&self.endpoint, &spec, budget, request_deadline)
            .await
            .map_err(|failure| match failure {
                TransportFailure::Transient(err) => AttemptFailure::Retryable {
                    err,
                    retry_after: None,
                },
                TransportFailure::Fatal(err) => AttemptFailure::Fatal(err),
            })?;

        // §4.3: Never follow redirects (301, 302, 303, 307, 308)
        if matches!(head.status, 301 | 302 | 303 | 307 | 308) {
            return Err(AttemptFailure::Fatal(ProviderError::RedirectBlocked(
                head.status,
            )));
        }

        // §4.3: Retryable status codes: 429, 502, 503, 504
        if matches!(head.status, 429 | 502 | 503 | 504) {
            let body = read_error_body(&mut head, budget, request_deadline).await;
            return Err(AttemptFailure::Retryable {
                err: ProviderError::HttpStatus {
                    status: head.status,
                    body: String::from_utf8_lossy(&body).to_string(),
                },
                retry_after: head.retry_after,
            });
        }

        if head.status != 200 {
            let body = read_error_body(&mut head, budget, request_deadline).await;
            return Err(AttemptFailure::Fatal(ProviderError::HttpStatus {
                status: head.status,
                body: String::from_utf8_lossy(&body).to_string(),
            }));
        }

        if !stream {
            let body_bytes =
                read_full_body(&mut head, budget, request_deadline, MAX_SSE_RESPONSE_BYTES)
                    .await
                    .map_err(AttemptFailure::Fatal)?;
            let body_str = std::str::from_utf8(&body_bytes)
                .map_err(|e| AttemptFailure::Fatal(ProviderError::Transport(e.to_string())))?;
            let turn = parse_non_streaming_response(body_str).map_err(AttemptFailure::Fatal)?;
            let latency_ms = attempt_start.elapsed().as_millis() as u64;
            return Ok((turn, latency_ms, Some(latency_ms)));
        }

        // Streaming SSE mode: once any business chunk is observed, mid-stream failure is NEVER retried (§4.3).
        // If failure occurs before any business chunk is observed, it is Retryable.
        let mut assembler = SseStreamAssembler::new();
        let mut ttft_ms: Option<u64> = None;
        let classify_sse_err = |saw_business: bool, err: ProviderError| -> AttemptFailure {
            if saw_business {
                AttemptFailure::Fatal(err)
            } else {
                AttemptFailure::Retryable {
                    err,
                    retry_after: None,
                }
            }
        };

        let pump_outcome = {
            let ttft_ref = &mut ttft_ms;
            let assembler_ref = &mut assembler;
            pump_body(
                &mut head,
                budget,
                request_deadline,
                MAX_SSE_RESPONSE_BYTES,
                move |chunk| {
                    if ttft_ref.is_none() {
                        *ttft_ref = Some(attempt_start.elapsed().as_millis() as u64);
                    }
                    assembler_ref.feed_bytes(chunk)
                },
            )
            .await
        };
        pump_outcome.map_err(|failure| match failure {
            BodyFailure::Fatal(err) => AttemptFailure::Fatal(err),
            BodyFailure::Stream(err) => classify_sse_err(assembler.saw_any_business_chunk(), err),
        })?;

        let saw_business = assembler.saw_any_business_chunk();
        let turn = assembler
            .finalize()
            .map_err(|e| classify_sse_err(saw_business, e))?;
        let latency_ms = attempt_start.elapsed().as_millis() as u64;
        Ok((turn, latency_ms, ttft_ms))
    }
}
