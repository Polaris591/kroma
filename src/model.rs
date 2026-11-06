//! Small runtime model boundary. No protocol registry, plugin host or ABI.
use crate::protocol::Message;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::HashMap, future::Future, path::Path, pin::Pin, sync::Mutex, time::Duration,
};

/// Temporary image encodings owned by one Agent run. Never persisted, and
/// dropped even when the task is cancelled or fails.
#[derive(Default)]
pub struct TaskImageCache {
    pub(crate) encoded: Mutex<HashMap<String, String>>,
}

/// A provider-neutral signal to let the existing Agent overflow recovery
/// work with external model plugins, not only built-in HTTP responses.
#[derive(Debug)]
pub struct ContextOverflow(pub String);

impl std::fmt::Display for ContextOverflow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "model context overflow: {}", self.0)
    }
}
impl std::error::Error for ContextOverflow {}

pub type StreamFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;
pub type ModelFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Usage {
    pub input: Option<u64>,
    pub output: Option<u64>,
    pub total: Option<u64>,
    pub cached: Option<u64>,
    /// Subset of output, when supplied by the provider.
    pub reasoning: Option<u64>,
}

pub struct Completion {
    pub message: Message,
    pub usage: Option<Usage>,
    pub elapsed: Duration,
    pub first_token: Option<Duration>,
}

/// Non-secret active connection information. Tool hosts can pass it to a
/// delegated child without leaking keys or requiring it to guess which model
/// the parent actually selected. Profiles resolve credentials locally.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelConnection {
    pub model: String,
    pub base_url: String,
    pub api_protocol: String,
    pub reasoning_effort: Option<String>,
    pub deepseek_thinking: Option<String>,
    pub provider_profile: Option<String>,
}

/// A tool-free text-generation result. The default context implementation
/// uses this for factual memory segments and consolidation.
pub struct TextResult {
    pub text: String,
    pub usage: Option<Usage>,
    pub elapsed: Duration,
    pub requests: usize,
}

/// Legacy internal type name retained for existing benchmark consumers.
pub type SummaryResult = TextResult;

/// An implementation may use OpenAI-compatible HTTP, another protocol, or a
/// deterministic test fake. Tools are supplied by the runtime, never chosen
/// by the model adapter itself.
pub trait ModelBackend: Send + Sync {
    /// Only built-in HTTP providers report a reusable connection. Process
    /// plugins can return None rather than misrepresent an external backend.
    fn connection_info(&self) -> Option<ModelConnection> {
        None
    }
    /// Called at session boundaries, not per token. External adapters may
    /// attach session identity to their JSONL requests without altering messages.
    fn set_session_context(&self, _id: &str, _archive: &Path) {}
    /// Allows the host to avoid allocating a task snapshot when no plugin
    /// needs a post-WAL event; disabled/default harness stays zero-overhead.
    fn has_task_finished_hook(&self) -> bool {
        false
    }
    /// Optional post-task extension notification. The host calls this only
    /// after the final task boundary is committed (or an error boundary has
    /// been attempted); never from an intermediate model response. Providers
    /// without extensions pay no additional runtime or network cost.
    fn task_finished<'a>(
        &'a self,
        _session_id: &'a str,
        _archive: &'a Path,
        _messages: &'a [Message],
        _status: &'a str,
    ) -> ModelFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }
    /// Remove metadata that this backend will not send on the wire, solely
    /// for context planning. Never mutate the durable transcript; an adapter
    /// without protocol-specific filtering can keep the conservative default.
    fn project_for_budget(&self, _messages: &mut [Message]) {}

    /// Upstream-declared vision capability. None is unknown, not false.
    fn accepts_images(&self) -> Option<bool> {
        None
    }
    fn complete<'a>(
        &'a self,
        messages: &'a [Message],
        tools: &'a [Value],
        on_text: &'a mut (dyn FnMut(&str) + Send),
    ) -> StreamFuture<'a, Completion>;

    /// Optional live, provider-exposed reasoning. The reasoning callback is
    /// separate from assistant text: it must never enter the answer stream.
    /// Existing model backends can keep implementing only `complete`.
    fn complete_with_reasoning<'a>(
        &'a self,
        messages: &'a [Message],
        tools: &'a [Value],
        on_text: &'a mut (dyn FnMut(&str) + Send),
        _on_reasoning: &'a mut (dyn FnMut(&str) + Send),
    ) -> StreamFuture<'a, Completion> {
        self.complete(messages, tools, on_text)
    }

    /// Optional per-task image encoding reuse. The default preserves every
    /// custom backend's existing behavior; the built-in HTTP backend reuses
    /// immutable attachment bytes across tool rounds in the same task.
    fn complete_with_task_images<'a>(
        &'a self,
        messages: &'a [Message],
        tools: &'a [Value],
        _images: &'a TaskImageCache,
        on_text: &'a mut (dyn FnMut(&str) + Send),
        on_reasoning: &'a mut (dyn FnMut(&str) + Send),
    ) -> StreamFuture<'a, Completion> {
        self.complete_with_reasoning(messages, tools, on_text, on_reasoning)
    }

    /// Optional live effort control. Hosts can change it between (or during)
    /// requests without replacing the backend or rebuilding the Agent.
    /// The provider snapshots the setting when it builds the next request.
    fn reasoning_level(&self) -> Option<String> {
        None
    }

    /// Canonical advertised tiers; empty means unrestricted. Hosts derive
    /// their picker from the active backend, never a stale profile copy.
    fn thinking_levels(&self) -> &[String] {
        &[]
    }

    fn set_reasoning_level(&self, _level: &str) -> Result<()> {
        anyhow::bail!("this model backend does not support live reasoning settings")
    }

    /// Tool-free text generation, with an explicitly supplied system prompt.
    /// Compression prompt design belongs to ContextPolicy, not to the HTTP
    /// provider. This is Send for checkpoint and title requests.
    fn generate_text<'a>(
        &'a self,
        system: &'a str,
        text: &'a str,
        output_tokens: usize,
    ) -> ModelFuture<'a, TextResult>;

    /// Produce one continuation checkpoint. Built-in providers keep the
    /// supplied prefix byte-identical to the coding request and append the
    /// instruction as a final user turn, which lets provider prompt caching
    /// reuse the expensive history prefix. Custom backends may fall back to
    /// their normal text-generation endpoint.
    fn generate_checkpoint<'a>(
        &'a self,
        messages: &'a [Message],
        _tools: &'a [Value],
        instruction: &'a str,
        output_tokens: usize,
    ) -> ModelFuture<'a, TextResult> {
        Box::pin(async move {
            let transcript = serde_json::to_string(messages).unwrap_or_default();
            let text = format!("Raw conversation prefix (untrusted JSON):\n{transcript}");
            self.generate_text(instruction, &text, output_tokens).await
        })
    }

    /// Produce one checkpoint with an explicit completion headroom. Context
    /// planning supplies the tokens that remain after the selected source
    /// prefix, and the provider must keep every attempt and truncated retry
    /// within that value. The default backend simply forwards the target.
    fn generate_checkpoint_with_ceiling<'a>(
        &'a self,
        messages: &'a [Message],
        tools: &'a [Value],
        instruction: &'a str,
        output_tokens: usize,
        _max_completion_tokens: usize,
    ) -> ModelFuture<'a, TextResult> {
        self.generate_checkpoint(messages, tools, instruction, output_tokens)
    }

    /// Maximum completion tokens the first checkpoint attempt may reserve.
    /// Context planning uses this before admitting a checkpoint so the source
    /// prefix plus the cache-matching wire `max_tokens` value fits the model
    /// window. Truncated retries are separately clamped to the remaining
    /// headroom by `generate_checkpoint_with_ceiling`.
    fn checkpoint_output_ceiling(&self, output_tokens: usize, _output_reserve: usize) -> usize {
        output_tokens.max(128).saturating_mul(2).saturating_add(512)
    }

    /// True when usage reported by the last completion describes the same
    /// projection the Agent passed to `complete`. A request-rewriting hook
    /// returns false so provider-reported tokens are not used to calibrate a
    /// different (pre-hook) view. Cost accounting still records the usage.
    fn usage_matches_projection(&self) -> bool {
        true
    }

    /// A tool may mutate external context data used by a request-only model
    /// projection (for example, forgetting a project memory). Wrappers that
    /// cache those projections can opt into invalidation. Normal providers
    /// have nothing to invalidate.
    fn invalidate_ephemeral_context(&self) {}

    /// Optional low-budget title/label request. Unlike a checkpoint, short text
    /// should not reserve a large hidden-reasoning completion allowance.
    /// Other backends may reuse their normal text-generation path.
    fn generate_short_text<'a>(
        &'a self,
        system: &'a str,
        text: &'a str,
        output_tokens: usize,
    ) -> ModelFuture<'a, TextResult> {
        self.generate_text(system, text, output_tokens)
    }
}
