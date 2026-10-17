pub mod anthropic_messages;
pub mod chat_completions;
pub mod endpoint;
pub mod openai_responses;
pub mod sse;
mod transport;
mod channel;

use crate::budget::{BudgetConfig, TokenBudget, TokenReservation};
use crate::messages::{AssistantTurn, CacheBreakpointMarker, FinishReason, TranscriptItem};
use crate::tool::definition::ToolDefinition;
use crate::types::TerminalReason;
use std::collections::{HashSet, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use thiserror::Error;
use tokio::sync::watch;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ProviderError {
    #[error("truncated tool call or response (finish_reason={0})")]
    TruncatedToolCall(String),
    #[error("incomplete SSE stream (missing [DONE] or premature disconnect): {0}")]
    IncompleteStream(String),
    #[error("invalid tool call batch: {0}")]
    InvalidToolBatch(String),
    #[error("insecure or invalid provider base URL: {0}")]
    InsecureUrl(String),
    #[error("HTTP redirect blocked ({0})")]
    RedirectBlocked(u16),
    #[error("HTTP error {status}: {body}")]
    HttpStatus { status: u16, body: String },
    #[error("request cancelled")]
    Cancelled,
    #[error("request timeout")]
    Timeout,
    #[error("max request attempts exhausted")]
    MaxAttemptsExhausted,
    #[error("insufficient token allowance for another provider attempt")]
    RetryBudgetExhausted,
    #[error("transport error: {0}")]
    Transport(String),
    #[error("channel temporarily unavailable; retry after {retry_after_ms}ms")]
    ChannelUnavailable { retry_after_ms: u64 },
    /// Admission failed before the first attempt. Only this wrapper proves that this logical
    /// call sent nothing; a later refused retry must keep its earlier unknown consumption.
    #[error("request not dispatched: {0}")]
    NotDispatched(Box<ProviderError>),
}

impl ProviderError {
    pub fn into_terminal_reason(self) -> TerminalReason {
        match self {
            ProviderError::NotDispatched(reason) => reason.into_terminal_reason(),
            ProviderError::Cancelled => TerminalReason::UserCancelled,
            ProviderError::MaxAttemptsExhausted => TerminalReason::MaxRequestAttempts,
            ProviderError::RetryBudgetExhausted => TerminalReason::TokenBudgetExhausted,
            other => TerminalReason::ProviderProtocolError(other.to_string()),
        }
    }

    /// Returns true if the error represents a provably pre-dispatch failure where no network
    /// transmission ever took place and provider consumption is guaranteed to be zero.
    ///
    /// Only client-side pre-dispatch rejections (e.g. `InsecureUrl` validation failure before any socket
    /// is opened) fall into this category. Post-dispatch errors (HTTP 500/502/503/504, 4xx, redirects,
    /// mid-stream disconnects, timeouts, cancellations, or final rejections following earlier retry
    /// attempts) cannot prove zero backend consumption; those reservations are conservatively left
    /// un-settled so `Drop` commits them to the ledger to keep budget ceilings fail-closed.
    pub fn is_provably_zero_consumption(&self) -> bool {
        matches!(self, ProviderError::InsecureUrl(_) | ProviderError::NotDispatched(_))
    }

    /// Backwards-compatible alias for [`Self::is_provably_zero_consumption`].
    pub fn is_definitive_rejection(&self) -> bool {
        self.is_provably_zero_consumption()
    }
}

#[derive(Debug, Clone, Default)]
pub struct ProviderRequest {
    pub system_prompt: String,
    pub tools: Vec<ToolDefinition>,
    pub messages: Vec<TranscriptItem>,
    pub stream: bool,
    /// Gate 7 (`v1.2.0`): logical cache boundaries from the context projector. Protocols with an
    /// explicit prompt-cache API (Anthropic Messages) place `cache_control` breakpoints here so the
    /// frozen prefix is cached instead of re-billed every turn. Empty for callers that do not
    /// project context (tests, subagent runs without a projector).
    pub cache_breakpoints: Vec<CacheBreakpointMarker>,
    /// F08 (`v1.4.0`): the remaining output-token allowance for *this* request, derived from the
    /// run's token budget. `None` means unbounded (no token budget configured). Protocols map this
    /// onto their native output cap (`max_tokens` / `max_output_tokens`). Input allowance is still
    /// an estimate and providers may report overage; this is not an exact run-token hard cap.
    pub max_output_tokens: Option<u64>,
    /// The runner already reserves the first attempt. Additional HTTP attempts must obtain their
    /// own allowance before transmission; dropping a failed attempt conservatively retains it.
    pub retry_budget: Option<RetryBudget>,
    /// Optional counter the provider increments once per transmitted HTTP attempt, so a failed call
    /// still reports how many requests it actually sent.
    pub wire_attempts: Option<WireAttempts>,
}

/// Counts the HTTP attempts a provider call actually transmitted.
///
/// The runner records the first attempt before dispatch; when a call *fails* after retrying, the
/// completion summary would otherwise only describe the logical turn and under-report what reached
/// the provider. Sharing a counter lets the failure path report the real number too.
#[derive(Debug, Clone, Default)]
pub struct WireAttempts(Arc<std::sync::atomic::AtomicUsize>);

impl WireAttempts {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one transmitted attempt.
    pub fn record(&self) {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }

    /// How many attempts were transmitted so far.
    pub fn get(&self) -> usize {
        self.0.load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// Shared allowance for wire retries. Its presence opts a direct provider caller into accounting;
/// without it, retries retain the historical unbudgeted API behaviour.
#[derive(Debug, Clone)]
pub struct RetryBudget {
    pub ledger: Arc<TokenBudget>,
    pub per_attempt: u64,
}

impl RetryBudget {
    fn reserve(&self) -> Result<TokenReservation, ProviderError> {
        if self.per_attempt == 0 {
            return Err(ProviderError::RetryBudgetExhausted);
        }
        self.ledger
            .try_reserve(self.per_attempt)
            .ok_or(ProviderError::RetryBudgetExhausted)
    }
}

/// First attempt is reserved by AgentRunner; this helper reserves only later wire attempts.
fn reserve_retry(
    req: &ProviderRequest,
    attempts_used: usize,
) -> Result<Option<TokenReservation>, ProviderError> {
    if attempts_used == 0 {
        Ok(None)
    } else {
        req.retry_budget.as_ref().map(RetryBudget::reserve).transpose()
    }
}

#[cfg(test)]
mod retry_budget_tests {
    use super::*;

    #[test]
    fn reported_identity_is_unknown_when_missing_and_conflicts_are_rejected() {
        assert_eq!(model_from_json(r#"{"choices":[]}"#).unwrap(), None);
        assert_eq!(model_from_json(r#"{"model":" "}"#).unwrap(), None);
        assert_eq!(model_from_json(r#"{"model":"returned-model"}"#).unwrap().as_deref(), Some("returned-model"));
        let mut model = Some("first".to_string());
        assert!(capture_reported_model(&mut model, &serde_json::json!({"model":"second"})).is_err());
        assert_eq!(model.as_deref(), Some("first"));
    }

    #[test]
    fn zero_allowance_is_refused_and_dropped_retry_is_unknown() {
        let ledger = Arc::new(TokenBudget::new(Some(200)));
        let mut req = ProviderRequest {
            retry_budget: Some(RetryBudget { ledger: Arc::clone(&ledger), per_attempt: 0 }),
            ..ProviderRequest::default()
        };
        assert!(matches!(reserve_retry(&req, 1), Err(ProviderError::RetryBudgetExhausted)));
        req.retry_budget.as_mut().unwrap().per_attempt = 100;
        assert!(reserve_retry(&req, 0).unwrap().is_none());
        let in_flight = reserve_retry(&req, 1).unwrap().unwrap();
        assert_eq!(ledger.reserved(), 100);
        // Cancellation/unwind drops an in-flight retry: no refund and no invented usage.
        drop(in_flight);
        let snapshot = ledger.accounting_snapshot();
        assert_eq!(snapshot.unknown_tokens, 100);
        assert_eq!(snapshot.settled_tokens, 0);
        assert_eq!(snapshot.reserved, 0);
    }
}

/// Merge repeated stream identity assertions, rejecting conflicting identities instead of silently
/// attributing usage to whichever chunk happened to arrive last.
fn capture_reported_model(current: &mut Option<String>, value: &serde_json::Value) -> Result<(), ProviderError> {
    let Some(model) = value.get("model").and_then(serde_json::Value::as_str)
        .filter(|model| !model.trim().is_empty()) else { return Ok(()); };
    if current.as_deref().is_some_and(|existing| existing != model) {
        return Err(ProviderError::Transport("response changed its reported model identity".to_string()));
    }
    *current = Some(model.to_string());
    Ok(())
}

fn model_from_json(text: &str) -> Result<Option<String>, ProviderError> {
    let value = serde_json::from_str(text).map_err(|e| ProviderError::Transport(format!("invalid response identity JSON: {}", e)))?;
    let mut model = None;
    capture_reported_model(&mut model, &value)?;
    Ok(model)
}

fn settle_retry(reservation: Option<TokenReservation>, turn: &AssistantTurn) {
    if let (Some(reservation), Some(usage)) = (reservation, turn.usage.as_ref()) {
        reservation.settle(usage.input_tokens.saturating_add(usage.output_tokens));
    }
    // No usage: the reservation drops as unknown, never a fictional measured zero.
}

/// Gate 7 (`v1.2.0`): the wire protocol used to talk to a model endpoint.
///
/// `Auto` keeps the historical behaviour: an OpenAI-compatible gateway on `/v1/chat/completions`
/// unless the base URL or model id clearly names another protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProviderProtocol {
    #[default]
    Auto,
    ChatCompletions,
    AnthropicMessages,
    OpenAiResponses,
}

impl ProviderProtocol {
    pub fn as_str(self) -> &'static str {
        match self {
            ProviderProtocol::Auto => "auto",
            ProviderProtocol::ChatCompletions => "chat-completions",
            ProviderProtocol::AnthropicMessages => "anthropic-messages",
            ProviderProtocol::OpenAiResponses => "openai-responses",
        }
    }

    /// Parse a `--protocol` / `KROMA_PROTOCOL` value.
    pub fn parse(value: &str) -> Result<Self, ProviderError> {
        match value.trim().to_ascii_lowercase().as_str() {
            "auto" => Ok(ProviderProtocol::Auto),
            "chat-completions" | "chat_completions" | "openai" | "openai-chat" => {
                Ok(ProviderProtocol::ChatCompletions)
            }
            "anthropic-messages" | "anthropic_messages" | "anthropic" | "claude" => {
                Ok(ProviderProtocol::AnthropicMessages)
            }
            "openai-responses" | "openai_responses" | "responses" => {
                Ok(ProviderProtocol::OpenAiResponses)
            }
            other => Err(ProviderError::Transport(format!(
                "unknown provider protocol '{}'; expected auto|chat-completions|anthropic-messages|openai-responses",
                other
            ))),
        }
    }

    /// Resolve `Auto` into a concrete protocol from the base URL path and the model id.
    pub fn detect(base_url: &str, model: &str) -> Self {
        let url = base_url.to_ascii_lowercase();
        let model = model.to_ascii_lowercase();
        if url.contains("anthropic") || model.starts_with("claude") {
            return ProviderProtocol::AnthropicMessages;
        }
        if url.contains("/responses") || url.contains("openai-responses") {
            return ProviderProtocol::OpenAiResponses;
        }
        ProviderProtocol::ChatCompletions
    }

    /// Resolve this selection against the configured endpoint and model.
    pub fn resolve(self, base_url: &str, model: &str) -> Self {
        match self {
            ProviderProtocol::Auto => ProviderProtocol::detect(base_url, model),
            concrete => concrete,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ProviderCompletion {
    pub turn: AssistantTurn,
    pub request_body_bytes: usize,
    pub latency_ms: u64,
    pub ttft_ms: Option<u64>,
    pub attempts_used: usize,
    /// Exact nonblank identity reported by the response, never backfilled from the request alias.
    pub reported_model: Option<String>,
}

pub trait ModelProvider: Send + Sync {
    fn complete<'a>(
        &'a self,
        req: ProviderRequest,
        budget: &'a BudgetConfig,
        cancel_rx: watch::Receiver<bool>,
    ) -> Pin<Box<dyn Future<Output = Result<ProviderCompletion, ProviderError>> + Send + 'a>>;
}

/// Pre-Dispatch Batch Validation (§4.1):
/// - Rejects `finish_reason == Length`, `ContentFilter`, or `Unknown` before any side effect.
/// - Requires `finish_reason == ToolCalls` (unless `allow_tool_calls_on_stop` is enabled) when tool_calls are present.
/// - Checks `tool_calls.len() <= max_tool_calls_per_response` (default 16).
/// - Checks every `call_id` is non-empty and unique within the batch.
/// - Checks every tool name is non-empty and `raw_arguments` is <= `max_tool_argument_bytes` (1 MiB) and valid JSON object.
pub fn validate_assistant_turn_batch(
    turn: &AssistantTurn,
    budget: &BudgetConfig,
    allow_tool_calls_on_stop: bool,
) -> Result<(), ProviderError> {
    match &turn.finish_reason {
        FinishReason::Length => {
            return Err(ProviderError::TruncatedToolCall("length".to_string()));
        }
        FinishReason::ContentFilter => {
            return Err(ProviderError::TruncatedToolCall(
                "content_filter".to_string(),
            ));
        }
        FinishReason::Unknown(u) => {
            return Err(ProviderError::TruncatedToolCall(format!("unknown({})", u)));
        }
        FinishReason::Stop => {
            if !turn.tool_calls.is_empty() && !allow_tool_calls_on_stop {
                return Err(ProviderError::InvalidToolBatch(
                    "tool_calls present with finish_reason=stop (strict mode requires finish_reason=tool_calls)".to_string(),
                ));
            }
        }
        FinishReason::ToolCalls => {
            if turn.tool_calls.is_empty() {
                return Err(ProviderError::InvalidToolBatch(
                    "finish_reason=tool_calls but tool_calls array is empty".to_string(),
                ));
            }
        }
    }

    if turn.tool_calls.is_empty() {
        return Ok(());
    }

    if turn.tool_calls.len() > budget.max_tool_calls_per_response {
        return Err(ProviderError::InvalidToolBatch(format!(
            "tool_calls count ({}) exceeds max_tool_calls_per_response ({})",
            turn.tool_calls.len(),
            budget.max_tool_calls_per_response
        )));
    }

    let mut seen_ids = HashSet::with_capacity(turn.tool_calls.len());
    for (idx, call) in turn.tool_calls.iter().enumerate() {
        if !call.call_id.is_valid() {
            return Err(ProviderError::InvalidToolBatch(format!(
                "tool_calls[{}] has empty call_id",
                idx
            )));
        }
        if !seen_ids.insert(call.call_id.clone()) {
            return Err(ProviderError::InvalidToolBatch(format!(
                "duplicate tool_call_id '{}' in batch",
                call.call_id
            )));
        }
        if call.name.trim().is_empty() {
            return Err(ProviderError::InvalidToolBatch(format!(
                "tool_calls[{}] has empty function name",
                idx
            )));
        }
        if call.raw_arguments.len() > budget.max_tool_argument_bytes {
            return Err(ProviderError::InvalidToolBatch(format!(
                "tool_calls[{}] arguments size ({} bytes) exceeds max_tool_argument_bytes ({})",
                idx,
                call.raw_arguments.len(),
                budget.max_tool_argument_bytes
            )));
        }
        match serde_json::from_str::<serde_json::Value>(&call.raw_arguments) {
            Ok(v) if v.is_object() => {}
            Ok(_) => {
                return Err(ProviderError::InvalidToolBatch(format!(
                    "tool_calls[{}] arguments must be a JSON object",
                    idx
                )));
            }
            Err(e) => {
                return Err(ProviderError::InvalidToolBatch(format!(
                    "tool_calls[{}] arguments is not valid JSON: {}",
                    idx, e
                )));
            }
        }
    }

    Ok(())
}

/// Deterministic in-memory FakeProvider for offline contract and smoke testing.
#[derive(Clone, Default)]
pub struct FakeProvider {
    responses: Arc<Mutex<VecDeque<Result<AssistantTurn, ProviderError>>>>,
    recorded_requests: Arc<Mutex<Vec<ProviderRequest>>>,
    recorded_serialized_bodies: Arc<Mutex<Vec<String>>>,
}

impl FakeProvider {
    pub fn new(turns: Vec<AssistantTurn>) -> Self {
        let q = turns.into_iter().map(Ok).collect();
        Self {
            responses: Arc::new(Mutex::new(q)),
            recorded_requests: Arc::new(Mutex::new(Vec::new())),
            recorded_serialized_bodies: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn with_results(results: Vec<Result<AssistantTurn, ProviderError>>) -> Self {
        Self {
            responses: Arc::new(Mutex::new(results.into_iter().collect())),
            recorded_requests: Arc::new(Mutex::new(Vec::new())),
            recorded_serialized_bodies: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn recorded_requests(&self) -> Vec<ProviderRequest> {
        self.recorded_requests.lock().unwrap().clone()
    }

    pub fn recorded_serialized_bodies(&self) -> Vec<String> {
        self.recorded_serialized_bodies.lock().unwrap().clone()
    }
}

impl ModelProvider for FakeProvider {
    fn complete<'a>(
        &'a self,
        req: ProviderRequest,
        _budget: &'a BudgetConfig,
        cancel_rx: watch::Receiver<bool>,
    ) -> Pin<Box<dyn Future<Output = Result<ProviderCompletion, ProviderError>> + Send + 'a>> {
        Box::pin(async move {
            if *cancel_rx.borrow() {
                return Err(ProviderError::Cancelled);
            }
            let body_json = chat_completions::serialize_openai_request_body(
                "fake-model",
                &req.system_prompt,
                &req.tools,
                &req.messages,
                req.stream,
            );
            let body_bytes = body_json.len();
            self.recorded_serialized_bodies
                .lock()
                .unwrap()
                .push(body_json);
            self.recorded_requests.lock().unwrap().push(req);

            let next = self
                .responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| {
                    Ok(AssistantTurn {
                        content: Some("Done.".to_string()),
                        reasoning_blocks: Vec::new(),
                        tool_calls: Vec::new(),
                        finish_reason: FinishReason::Stop,
                        usage: None,
                    })
                })?;

            Ok(ProviderCompletion {
                turn: next,
                request_body_bytes: body_bytes,
                latency_ms: 1,
                ttft_ms: Some(1),
                attempts_used: 1,
                reported_model: None,
            })
        })
    }
}

/// Gate 7 (`v1.2.0`): protocol-agnostic provider settings. One struct feeds all three wire
/// protocols so the CLI, the ACP/serve surfaces and subagent runs cannot drift apart.
#[derive(Debug, Clone)]
pub struct ProviderSettings {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub max_retries: usize,
    pub reasoning_effort: Option<String>,
    pub protocol: ProviderProtocol,
    pub max_tokens: u32,
}

/// Build the [`ModelProvider`] for the resolved protocol.
///
/// `Auto` is resolved against the base URL and model id, and the concrete protocol is returned so
/// callers can surface which wire format is in use.
pub fn build_provider(
    settings: &ProviderSettings,
) -> Result<(ProviderProtocol, Arc<dyn ModelProvider>), ProviderError> {
    let resolved = settings
        .protocol
        .resolve(&settings.base_url, &settings.model);
    let provider: Arc<dyn ModelProvider> = match resolved {
        ProviderProtocol::ChatCompletions | ProviderProtocol::Auto => {
            Arc::new(chat_completions::ChatCompletionsClient::new(
                chat_completions::ChatCompletionsConfig {
                    base_url: settings.base_url.clone(),
                    api_key: settings.api_key.clone(),
                    model: settings.model.clone(),
                    max_retries: settings.max_retries,
                    reasoning_effort: settings.reasoning_effort.clone(),
                },
            )?)
        }
        ProviderProtocol::AnthropicMessages => {
            Arc::new(anthropic_messages::AnthropicMessagesClient::new(
                anthropic_messages::AnthropicMessagesConfig {
                    base_url: settings.base_url.clone(),
                    api_key: settings.api_key.clone(),
                    model: settings.model.clone(),
                    max_retries: settings.max_retries,
                    max_tokens: settings.max_tokens,
                },
            )?)
        }
        ProviderProtocol::OpenAiResponses => {
            Arc::new(openai_responses::OpenAiResponsesClient::new(
                openai_responses::OpenAiResponsesConfig {
                    base_url: settings.base_url.clone(),
                    api_key: settings.api_key.clone(),
                    model: settings.model.clone(),
                    max_retries: settings.max_retries,
                    reasoning_effort: settings.reasoning_effort.clone(),
                },
            )?)
        }
    };
    Ok((resolved, provider))
}
