//! Small runtime model boundary. No protocol registry, plugin host or ABI.
use crate::protocol::Message;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::HashMap, future::Future, pin::Pin, sync::Mutex, time::Duration};

/// Temporary image encodings owned by one Agent run. Never persisted, and
/// dropped even when the task is cancelled or fails.
#[derive(Default)]
pub struct TaskImageCache {
    pub(crate) encoded: Mutex<HashMap<String, String>>,
}

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
    /// provider. This is Send for background APEC tasks.
    fn generate_text<'a>(
        &'a self,
        system: &'a str,
        text: &'a str,
        output_tokens: usize,
    ) -> ModelFuture<'a, TextResult>;

    /// Optional low-budget title/label request. Unlike APEC, short text
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
