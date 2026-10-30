//! Pre-request projection hooks, without changing the persisted transcript.
use super::PluginWorker;
use crate::{
    context::ContextConfig,
    model::{
        Completion, ModelBackend, ModelFuture, StreamFuture, TaskImageCache, TextResult, Usage,
    },
    protocol::Message,
};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{sync::Arc, time::Instant};

/// A process-backed provider may opt into incremental JSONL frames. Legacy
/// providers continue returning a single completion response. The provider
/// protocol and credentials remain inside the external executable.
pub(super) struct ProcessModelBackend {
    worker: Arc<PluginWorker>,
    model_name: String,
    streaming: bool,
}

#[derive(Deserialize, Serialize)]
struct ProcessCompletion {
    message: Message,
    #[serde(default)]
    usage: Option<Usage>,
}

#[derive(Deserialize)]
struct ProcessGeneration {
    text: String,
    #[serde(default)]
    usage: Option<Usage>,
    #[serde(default = "single_request")]
    requests: usize,
}
fn single_request() -> usize {
    1
}

impl ProcessModelBackend {
    pub(super) fn new(worker: Arc<PluginWorker>, model_name: String, streaming: bool) -> Self {
        Self {
            worker,
            model_name,
            streaming,
        }
    }

    async fn complete_streaming(
        &self,
        messages: &[Message],
        tools: &[Value],
        on_text: &mut (dyn FnMut(&str) + Send),
        on_reasoning: &mut (dyn FnMut(&str) + Send),
    ) -> Result<Completion> {
        let started = Instant::now();
        let mut stream = self.worker.start_stream(json!({
            "type":"model_stream", "name":"model_stream",
            "arguments":{"model":self.model_name,"messages":messages,"tools":tools}
        }));
        let mut text = String::new();
        let mut reasoning = String::new();
        let mut first_token = None;
        while let Some(frame) = stream.next().await {
            match frame["event"].as_str() {
                Some("text_delta") | Some("reasoning_delta") => {
                    let delta = frame["delta"]
                        .as_str()
                        .context("stream delta must be a string")?;
                    if !delta.is_empty() {
                        first_token.get_or_insert_with(|| started.elapsed());
                        if frame["event"] == "text_delta" {
                            text.push_str(delta);
                            on_text(delta);
                        } else {
                            reasoning.push_str(delta);
                            on_reasoning(delta);
                        }
                    }
                }
                Some("error") => {
                    bail!(
                        "external model stream failed: {}",
                        frame["message"].as_str().unwrap_or("unknown plugin error")
                    );
                }
                Some("complete") => {
                    let mut reply: ProcessCompletion = serde_json::from_value(
                        frame
                            .get("result")
                            .cloned()
                            .context("stream completion needs a result")?,
                    )
                    .context("invalid streaming completion payload")?;
                    if reply.message.role != "assistant" {
                        bail!("stream completion must contain an assistant message");
                    }
                    if text.is_empty() {
                        if let Some(final_text) = reply.message.content.as_deref() {
                            if !final_text.is_empty() {
                                first_token.get_or_insert_with(|| started.elapsed());
                                on_text(final_text);
                            }
                        }
                    }
                    if !text.is_empty() {
                        match reply.message.content.as_deref() {
                            None | Some("") => reply.message.content = Some(text),
                            Some(final_text) if final_text != text => {
                                bail!("streamed text differs from the final message");
                            }
                            _ => {}
                        }
                    }
                    if reasoning.is_empty() {
                        if let Some(final_reasoning) = reply.message.reasoning_content.as_deref() {
                            if !final_reasoning.is_empty() {
                                first_token.get_or_insert_with(|| started.elapsed());
                                on_reasoning(final_reasoning);
                            }
                        }
                    }
                    if !reasoning.is_empty() {
                        match reply.message.reasoning_content.as_deref() {
                            None | Some("") => reply.message.reasoning_content = Some(reasoning),
                            Some(final_reasoning) if final_reasoning != reasoning => {
                                bail!("streamed reasoning differs from the final message");
                            }
                            _ => {}
                        }
                    }
                    stream.finish().await?;
                    let elapsed = started.elapsed();
                    return Ok(Completion {
                        message: reply.message,
                        usage: reply.usage,
                        elapsed,
                        first_token,
                    });
                }
                _ => bail!("unknown model streaming frame"),
            }
        }
        stream.finish().await?;
        bail!("model stream ended before a completion frame")
    }
}

impl ModelBackend for ProcessModelBackend {
    fn complete<'a>(
        &'a self,
        messages: &'a [Message],
        tools: &'a [Value],
        on_text: &'a mut (dyn FnMut(&str) + Send),
    ) -> StreamFuture<'a, Completion> {
        Box::pin(async move {
            if self.streaming {
                return self
                    .complete_streaming(messages, tools, on_text, &mut |_| {})
                    .await;
            }
            let started = Instant::now();
            let reply = self
                .worker
                .invoke_async(json!({
                    "type":"model_complete", "name":"model_complete",
                    "arguments":{"model":self.model_name,"messages":messages,"tools":tools}
                }))
                .await?;
            if reply.is_error {
                bail!("external model failed: {}", reply.content);
            }
            let value: ProcessCompletion = serde_json::from_str(&reply.content)
                .context("model_complete must return {message,usage?}")?;
            if value.message.role != "assistant" {
                bail!("model_complete must return an assistant message");
            }
            if let Some(content) = value.message.content.as_deref() {
                on_text(content);
            }
            let elapsed = started.elapsed();
            Ok(Completion {
                message: value.message,
                usage: value.usage,
                elapsed,
                first_token: Some(elapsed),
            })
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
            if self.streaming {
                return self
                    .complete_streaming(messages, tools, on_text, on_reasoning)
                    .await;
            }
            let result = self.complete(messages, tools, on_text).await?;
            if let Some(reasoning) = result.message.reasoning_content.as_deref() {
                on_reasoning(reasoning);
            }
            Ok(result)
        })
    }

    fn generate_text<'a>(
        &'a self,
        system: &'a str,
        text: &'a str,
        output_tokens: usize,
    ) -> ModelFuture<'a, TextResult> {
        Box::pin(async move {
            let started = Instant::now();
            let reply = self
                .worker
                .invoke_async(json!({
                    "type":"model_generate", "name":"model_generate",
                    "arguments":{"model":self.model_name,"system":system,
                        "prompt":text,"output_tokens":output_tokens}
                }))
                .await?;
            if reply.is_error {
                bail!("external text generation failed: {}", reply.content);
            }
            let result: ProcessGeneration = serde_json::from_str(&reply.content)
                .context("model_generate must return {text,usage?,requests?}")?;
            Ok(TextResult {
                text: result.text,
                usage: result.usage,
                elapsed: started.elapsed(),
                requests: result.requests,
            })
        })
    }

    fn generate_short_text<'a>(
        &'a self,
        system: &'a str,
        text: &'a str,
        output_tokens: usize,
    ) -> ModelFuture<'a, TextResult> {
        self.generate_text(system, text, output_tokens)
    }
}

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
