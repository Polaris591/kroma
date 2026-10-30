//! Adapter between process plugins and Kroma's existing ContextFactory/Policy.
use super::PluginWorker;
use crate::{
    context::{
        BuiltinContextFactory, CompactionUsage, ContextConfig, ContextFactory, ContextPolicy,
    },
    context_state::{ExternalContextState, PersistedContext},
    model::{ModelBackend, StreamFuture},
    ports::ContextCommit,
    protocol::Message,
};
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{path::Path, sync::Arc};

pub(super) struct PluginContextFactory {
    id: String,
    version: u32,
    worker: Arc<PluginWorker>,
}

impl PluginContextFactory {
    pub(super) fn new(id: String, version: u32, worker: Arc<PluginWorker>) -> Self {
        Self {
            id,
            version,
            worker,
        }
    }
}

impl ContextFactory for PluginContextFactory {
    fn initial(&self) -> PersistedContext {
        PersistedContext::External(ExternalContextState {
            strategy: self.id.clone(),
            version: self.version,
            data: Value::Null,
        })
    }

    fn restore(
        &self,
        state: PersistedContext,
        budget: ContextConfig,
        _archive: &Path,
    ) -> Result<Box<dyn ContextPolicy>> {
        let state = match state {
            // Existing sessions keep their builtin strategy. New sessions
            // receive the plugin strategy via initial() instead.
            PersistedContext::Builtin(original) => {
                return BuiltinContextFactory.restore(
                    PersistedContext::Builtin(original),
                    budget,
                    _archive,
                );
            }
            PersistedContext::External(state) => state,
        };
        if state.strategy != self.id || state.version != self.version {
            bail!(
                "context plugin '{}' v{} cannot restore '{}' v{}; original session preserved",
                self.id,
                self.version,
                state.strategy,
                state.version
            );
        }
        Ok(Box::new(PluginContextPolicy {
            state,
            worker: self.worker.clone(),
            budget,
            usage: CompactionUsage::default(),
            lifecycle_enabled: self.version >= 2,
            pending_lifecycle: Vec::new(),
        }))
    }

    fn branch_state(
        &self,
        state: &PersistedContext,
        source: &[Message],
        retained: &[Message],
        source_archive: &Path,
        destination_archive: &Path,
    ) -> Result<PersistedContext> {
        let PersistedContext::External(existing) = state else {
            return BuiltinContextFactory.branch_state(
                state,
                source,
                retained,
                source_archive,
                destination_archive,
            );
        };
        if retained.len() == source.len() {
            return Ok(state.clone());
        }
        if retained.len() <= 1 {
            return Ok(self.initial());
        }
        let response = self.worker.invoke(json!({
            "type":"context_branch", "name":"context_branch",
            "arguments":{"source":source,"retained":retained,"state":existing.data}
        }))?;
        if response.is_error {
            bail!("context branch failed: {}", response.content);
        }
        let patch: ContextPatch = serde_json::from_str(&response.content)
            .context("context_branch must return a JSON object")?;
        Ok(PersistedContext::External(ExternalContextState {
            strategy: self.id.clone(),
            version: self.version,
            data: patch.state.unwrap_or(Value::Null),
        }))
    }
}

#[derive(Deserialize)]
struct ContextPatch {
    #[serde(default)]
    messages: Option<Vec<Message>>,
    #[serde(default)]
    state: Option<Value>,
    #[serde(default)]
    recovered: Option<bool>,
    #[serde(default)]
    usage: Option<PluginCompactionUsage>,
}

#[derive(Deserialize)]
struct PluginCompactionUsage {
    requests: usize,
    #[serde(default)]
    elapsed_ms: u64,
    input: Option<u64>,
    output: Option<u64>,
    cached: Option<u64>,
}

struct PluginContextPolicy {
    state: ExternalContextState,
    worker: Arc<PluginWorker>,
    budget: ContextConfig,
    usage: CompactionUsage,
    lifecycle_enabled: bool,
    pending_lifecycle: Vec<Value>,
}

impl PluginContextPolicy {
    fn update(&mut self, patch: ContextPatch) -> Option<Value> {
        if let Some(usage) = patch.usage {
            let previously_empty = self.usage.requests == 0;
            self.usage.requests = self.usage.requests.saturating_add(usage.requests);
            self.usage.elapsed += std::time::Duration::from_millis(usage.elapsed_ms);
            let accumulate = |old: Option<u64>, next: Option<u64>| {
                if previously_empty {
                    next
                } else {
                    old.zip(next).map(|(a, b)| a.saturating_add(b))
                }
            };
            self.usage.input = accumulate(self.usage.input, usage.input);
            self.usage.output = accumulate(self.usage.output, usage.output);
            self.usage.cached = accumulate(self.usage.cached, usage.cached);
        }
        if let Some(data) = patch.state.clone() {
            self.state.data = data.clone();
            Some(data)
        } else {
            None
        }
    }

    /// Sync ContextPolicy callbacks never wait for external processes.
    /// Forward measurements to the next async prepare/finish invocation.
    fn observe_plugin(&mut self, name: &str, arguments: Value) {
        self.pending_lifecycle
            .push(json!({"type":name,"arguments":arguments}));
    }

    fn commit_patch(&mut self, store: &dyn ContextCommit, patch: ContextPatch) -> Result<()> {
        if let Some(data) = &patch.state {
            let durable = PersistedContext::External(ExternalContextState {
                strategy: self.state.strategy.clone(),
                version: self.state.version,
                data: data.clone(),
            });
            store.commit(&durable)?;
        }
        self.update(patch);
        Ok(())
    }
}

impl ContextPolicy for PluginContextPolicy {
    fn state(&self) -> PersistedContext {
        PersistedContext::External(self.state.clone())
    }

    fn new_user_turn(&mut self) {
        if !self.lifecycle_enabled {
            return;
        }
        self.observe_plugin("context_new_turn", json!({}));
    }

    fn mark_consumed(&mut self, message_count: usize) {
        if !self.lifecycle_enabled {
            return;
        }
        self.observe_plugin(
            "context_mark_consumed",
            json!({
                "message_count":message_count
            }),
        );
    }

    fn observe_with_bytes(
        &mut self,
        _sent: &[Message],
        bytes: usize,
        input: Option<u64>,
        cached: Option<u64>,
    ) {
        if !self.lifecycle_enabled {
            return;
        }
        self.observe_plugin(
            "context_observe",
            json!({
                "serialized_bytes":bytes,
                "input_tokens":input, "cached_tokens":cached
            }),
        );
    }

    fn recover_overflow<'a>(
        &'a mut self,
        messages: &'a [Message],
        _model: &'a Arc<dyn ModelBackend>,
        store: &'a dyn ContextCommit,
    ) -> StreamFuture<'a, bool> {
        Box::pin(async move {
            if !self.lifecycle_enabled {
                return Ok(false);
            }
            let reply = self
                .worker
                .invoke_async(json!({
                    "type":"context_overflow", "name":"context_overflow",
                    "arguments":{"messages":messages,"state":self.state.data}
                }))
                .await?;
            if reply.is_error {
                bail!("context overflow plugin failed: {}", reply.content);
            }
            let patch: ContextPatch = serde_json::from_str(&reply.content)?;
            let recovered = patch.recovered.unwrap_or(false);
            if recovered && patch.state.is_none() {
                bail!("context overflow recovery needs updated state");
            }
            if recovered {
                self.commit_patch(store, patch)?;
                self.pending_lifecycle.clear();
            }
            Ok(recovered)
        })
    }

    fn finish_task<'a>(
        &'a mut self,
        messages: &'a [Message],
        store: &'a dyn ContextCommit,
    ) -> StreamFuture<'a, ()> {
        Box::pin(async move {
            if !self.lifecycle_enabled {
                return Ok(());
            }
            let reply = self
                .worker
                .invoke_async(json!({
                    "type":"context_finish", "name":"context_finish",
                    "arguments":{"messages":messages,"state":self.state.data,
                        "lifecycle":self.pending_lifecycle}
                }))
                .await?;
            if reply.is_error {
                bail!("context finalization failed: {}", reply.content);
            }
            let patch: ContextPatch = serde_json::from_str(&reply.content)?;
            self.commit_patch(store, patch)?;
            self.pending_lifecycle.clear();
            Ok(())
        })
    }

    fn prepare<'a>(
        &'a mut self,
        messages: &'a [Message],
        _model: &'a Arc<dyn ModelBackend>,
        store: &'a dyn ContextCommit,
        extra_tokens: usize,
    ) -> StreamFuture<'a, Vec<Message>> {
        Box::pin(async move {
            let worker = self.worker.clone();
            let snapshot = json!({
                "type":"context_prepare","name":"context_prepare",
                "arguments":{
                    "messages":messages,
                    "state":self.state.data,
                    "max_input_tokens":self.budget.available_input().saturating_sub(extra_tokens),
                    "extra_tokens":extra_tokens,
                    "lifecycle":self.pending_lifecycle
                }
            });
            let response = worker.invoke_async(snapshot).await?;
            if response.is_error {
                bail!("context plugin failed: {}", response.content);
            }
            let mut patch: ContextPatch = serde_json::from_str(&response.content)
                .context("context_prepare must return messages and optional state")?;
            let projection = patch
                .messages
                .take()
                .context("context_prepare returned no messages")?;
            if projection.first().is_none_or(|msg| msg.role != "system") {
                bail!("context plugin projection must begin with a system message");
            }
            self.commit_patch(store, patch)?;
            self.pending_lifecycle.clear();
            Ok(projection)
        })
    }

    fn take_usage(&mut self) -> CompactionUsage {
        std::mem::take(&mut self.usage)
    }
}
