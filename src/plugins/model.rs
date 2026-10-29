//! Pre-request projection hooks, without changing the persisted transcript.
use super::PluginWorker;
use crate::{
    context::ContextConfig,
    model::{Completion, ModelBackend, ModelFuture, StreamFuture, TaskImageCache, TextResult},
    protocol::Message,
};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::sync::Arc;

pub(super) struct PluginModelHooks {
    inner: Arc<dyn ModelBackend>,
    budget: ContextConfig,
    workers: Vec<Arc<PluginWorker>>,
}

impl PluginModelHooks {
    pub(super) fn new(
        inner: Arc<dyn ModelBackend>,
        budget: ContextConfig,
        workers: Vec<Arc<PluginWorker>>,
    ) -> Self {
        Self {
            inner,
            budget,
            workers,
        }
    }

    async fn projection(&self, messages: &[Message], tools: &[Value]) -> Result<Vec<Message>> {
        let mut result = messages.to_vec();
        for worker in &self.workers {
            let request = json!({
                "type":"hook","name":"before_model_request",
                "arguments":{"messages":result,"tools":tools}
            });
            let worker = worker.clone();
            let reply = worker.invoke_async(request).await?;
            if reply.is_error {
                bail!("model request hook rejected: {}", reply.content);
            }
            let patch: Value = serde_json::from_str(&reply.content)
                .context("before_model_request must return a JSON patch")?;
            if patch.get("tools").is_some() {
                bail!("request hooks cannot mutate tool schemas; use tool registration instead");
            }
            if let Some(new_messages) = patch.get("messages") {
                let transformed: Vec<Message> = serde_json::from_value(new_messages.clone())
                    .context("invalid replacement model messages")?;
                if transformed.len() != result.len() {
                    bail!("request hook cannot add or remove messages");
                }
                for (before, after) in result.iter().zip(&transformed) {
                    if before.role != after.role
                        || before.tool_call_id != after.tool_call_id
                        || serde_json::to_value(&before.tool_calls)?
                            != serde_json::to_value(&after.tool_calls)?
                        || serde_json::to_value(&before.provider_items)?
                            != serde_json::to_value(&after.provider_items)?
                    {
                        bail!("request hook cannot alter message roles, tool calls, IDs or opaque provider state");
                    }
                }
                result = transformed;
            }
        }
        let mut planned = result.clone();
        self.inner.project_for_budget(&mut planned);
        self.budget.validate_final_request(&planned, tools)?;
        Ok(result)
    }
}

impl ModelBackend for PluginModelHooks {
    fn project_for_budget(&self, messages: &mut [Message]) {
        self.inner.project_for_budget(messages);
    }
    fn accepts_images(&self) -> Option<bool> {
        self.inner.accepts_images()
    }
    fn reasoning_level(&self) -> Option<String> {
        self.inner.reasoning_level()
    }
    fn thinking_levels(&self) -> &[String] {
        self.inner.thinking_levels()
    }
    fn set_reasoning_level(&self, level: &str) -> Result<()> {
        self.inner.set_reasoning_level(level)
    }

    fn complete<'a>(
        &'a self,
        messages: &'a [Message],
        tools: &'a [Value],
        on_text: &'a mut (dyn FnMut(&str) + Send),
    ) -> StreamFuture<'a, Completion> {
        Box::pin(async move {
            let view = self.projection(messages, tools).await?;
            self.inner.complete(&view, tools, on_text).await
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
            let view = self.projection(messages, tools).await?;
            self.inner
                .complete_with_reasoning(&view, tools, on_text, on_reasoning)
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
            let view = self.projection(messages, tools).await?;
            self.inner
                .complete_with_task_images(&view, tools, images, on_text, on_reasoning)
                .await
        })
    }

    fn generate_text<'a>(
        &'a self,
        system: &'a str,
        text: &'a str,
        output_tokens: usize,
    ) -> ModelFuture<'a, TextResult> {
        self.inner.generate_text(system, text, output_tokens)
    }

    fn generate_short_text<'a>(
        &'a self,
        system: &'a str,
        text: &'a str,
        output_tokens: usize,
    ) -> ModelFuture<'a, TextResult> {
        self.inner.generate_short_text(system, text, output_tokens)
    }
}
