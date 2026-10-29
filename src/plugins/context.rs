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
}

struct PluginContextPolicy {
    state: ExternalContextState,
    worker: Arc<PluginWorker>,
    budget: ContextConfig,
}

impl ContextPolicy for PluginContextPolicy {
    fn state(&self) -> PersistedContext {
        PersistedContext::External(self.state.clone())
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
                    "extra_tokens":extra_tokens
                }
            });
            let response = worker.invoke_async(snapshot).await?;
            if response.is_error {
                bail!("context plugin failed: {}", response.content);
            }
            let patch: ContextPatch = serde_json::from_str(&response.content)
                .context("context_prepare must return messages and optional state")?;
            let projection = patch
                .messages
                .context("context_prepare returned no messages")?;
            if projection.first().is_none_or(|msg| msg.role != "system") {
                bail!("context plugin projection must begin with a system message");
            }
            if let Some(data) = patch.state {
                let next = PersistedContext::External(ExternalContextState {
                    strategy: self.state.strategy.clone(),
                    version: self.state.version,
                    data,
                });
                store.commit(&next)?;
                if let PersistedContext::External(updated) = next {
                    self.state = updated;
                }
            }
            Ok(projection)
        })
    }

    fn take_usage(&mut self) -> CompactionUsage {
        CompactionUsage::default()
    }
}
