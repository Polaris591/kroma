//! Pre-request projection hooks, without changing the persisted transcript.
use super::{stream::ToolCallFragments, PluginResponse, PluginWorker};
use crate::{
    context::ContextConfig,
    model::{
        Completion, ContextOverflow, ModelBackend, ModelFuture, StreamFuture, TaskImageCache,
        TextResult, Usage,
    },
    protocol::Message,
};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    borrow::Cow,
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
    time::Instant,
};

/// A process-backed provider may opt into incremental JSONL frames. Legacy
/// providers continue returning a single completion response. The provider
/// protocol and credentials remain inside the external executable.
pub(super) struct ProcessModelBackend {
    worker: Arc<PluginWorker>,
    model_name: String,
    streaming: bool,
    thinking_levels: Vec<String>,
    reasoning_level: RwLock<Option<String>>,
    image_input: Option<bool>,
    session: RwLock<Option<(String, PathBuf)>>,
}

/// Compare parsed JSON arguments rather than source formatting. Some
/// upstream providers normalize whitespace in the final tool-call object.
fn process_model_error(reply: &PluginResponse, operation: &str) -> anyhow::Error {
    if reply.code.as_deref() == Some("context_overflow") {
        ContextOverflow(reply.content.clone()).into()
    } else {
        anyhow::anyhow!("{operation}: {}", reply.content)
    }
}

fn validate_assistant_reply(message: &Message) -> Result<()> {
    if message.role != "assistant" || message.tool_call_id.is_some() {
        bail!("process model must return an assistant message without tool_call_id");
    }
    let calls = message.tool_calls.as_deref().unwrap_or(&[]);
    let mut seen = std::collections::BTreeSet::new();
    for call in calls {
        if call.kind != "function"
            || call.id.is_empty()
            || call.function.name.is_empty()
            || !seen.insert(call.id.as_str())
        {
            bail!("process model returned incomplete or duplicate tool calls");
        }
        let arguments: Value = serde_json::from_str(&call.function.arguments)
            .context("process model tool arguments must contain valid JSON")?;
        if !arguments.is_object() {
            bail!("process model tool arguments must be a JSON object");
        }
    }
    if calls.is_empty()
        && message
            .content
            .as_deref()
            .is_none_or(|s| s.trim().is_empty())
    {
        bail!("process model returned neither text nor a tool call");
    }
    Ok(())
}

fn same_tool_calls(
    reconstructed: &[crate::protocol::ToolCall],
    final_calls: &[crate::protocol::ToolCall],
) -> Result<bool> {
    if reconstructed.len() != final_calls.len() {
        return Ok(false);
    }
    for (draft, final_call) in reconstructed.iter().zip(final_calls) {
        if draft.id != final_call.id
            || draft.kind != final_call.kind
            || draft.function.name != final_call.function.name
        {
            return Ok(false);
        }
        let draft_args: Value = serde_json::from_str(&draft.function.arguments)?;
        let final_args: Value = serde_json::from_str(&final_call.function.arguments)
            .context("final streamed tool call has invalid JSON arguments")?;
        if draft_args != final_args {
            return Ok(false);
        }
    }
    Ok(true)
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
    pub(super) fn new(
        worker: Arc<PluginWorker>,
        model_name: String,
        streaming: bool,
        thinking_levels: Vec<String>,
        reasoning_level: Option<String>,
        image_input: Option<bool>,
    ) -> Self {
        Self {
            worker,
            model_name,
            streaming,
            thinking_levels,
            reasoning_level: RwLock::new(reasoning_level),
            image_input,
            session: RwLock::new(None),
        }
    }

    fn session_fields(&self) -> (Option<String>, Option<PathBuf>) {
        self.session
            .read()
            .ok()
            .and_then(|data| data.clone())
            .map_or((None, None), |(id, archive)| (Some(id), Some(archive)))
    }

    async fn complete_streaming(
        &self,
        messages: &[Message],
        tools: &[Value],
        on_text: &mut (dyn FnMut(&str) + Send),
        on_reasoning: &mut (dyn FnMut(&str) + Send),
    ) -> Result<Completion> {
        let started = Instant::now();
        let (session_id, archive) = self.session_fields();
        let mut stream = self.worker.start_stream(json!({
            "session_id":session_id, "archive":archive,
            "type":"model_stream", "name":"model_stream",
            "arguments":{"model":self.model_name,"messages":messages,"tools":tools,
                "reasoning_level":self.reasoning_level()}
        }));
        let mut text = String::new();
        let mut reasoning = String::new();
        let mut tool_fragments = ToolCallFragments::default();
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
                Some("tool_call_delta") => {
                    first_token.get_or_insert_with(|| started.elapsed());
                    tool_fragments.push(&frame)?;
                }
                Some("error") => {
                    let message = frame["message"].as_str().unwrap_or("unknown plugin error");
                    if frame["code"] == "context_overflow" {
                        return Err(ContextOverflow(message.to_owned()).into());
                    }
                    bail!("external model stream failed: {message}");
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
                    if let Some(reconstructed) = tool_fragments.finish()? {
                        match reply.message.tool_calls.as_ref() {
                            Some(final_calls) if !same_tool_calls(&reconstructed, final_calls)? => {
                                bail!("streamed tool calls differ from final message");
                            }
                            Some(_) => {}
                            None => reply.message.tool_calls = Some(reconstructed),
                        }
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
                    validate_assistant_reply(&reply.message)?;
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
    fn set_session_context(&self, id: &str, archive: &Path) {
        if let Ok(mut session) = self.session.write() {
            *session = Some((id.to_owned(), archive.to_path_buf()));
        }
    }

    fn accepts_images(&self) -> Option<bool> {
        self.image_input
    }

    fn reasoning_level(&self) -> Option<String> {
        self.reasoning_level
            .read()
            .ok()
            .and_then(|value| value.clone())
    }

    fn thinking_levels(&self) -> &[String] {
        &self.thinking_levels
    }

    fn set_reasoning_level(&self, level: &str) -> Result<()> {
        if level.trim().is_empty()
            || (!self.thinking_levels.is_empty()
                && !self
                    .thinking_levels
                    .iter()
                    .any(|supported| supported == level))
        {
            bail!("unsupported external model thinking level '{level}'");
        }
        *self
            .reasoning_level
            .write()
            .map_err(|_| anyhow::anyhow!("model thinking state unavailable"))? =
            Some(level.to_owned());
        Ok(())
    }

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
            let (session_id, archive) = self.session_fields();
            let reply = self
                .worker
                .invoke_async(json!({
                    "session_id":session_id,"archive":archive,
                    "type":"model_complete", "name":"model_complete",
                    "arguments":{"model":self.model_name,"messages":messages,"tools":tools,
                        "reasoning_level":self.reasoning_level()}
                }))
                .await?;
            if reply.is_error {
                return Err(process_model_error(&reply, "external model failed"));
            }
            let value: ProcessCompletion = serde_json::from_str(&reply.content)
                .context("model_complete must return {message,usage?}")?;
            validate_assistant_reply(&value.message)?;
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
            let (session_id, archive) = self.session_fields();
            let reply = self
                .worker
                .invoke_async(json!({
                    "session_id":session_id,"archive":archive,
                    "type":"model_generate", "name":"model_generate",
                    "arguments":{"model":self.model_name,"system":system,
                        "prompt":text,"output_tokens":output_tokens,
                        "reasoning_level":self.reasoning_level()}
                }))
                .await?;
            if reply.is_error {
                return Err(process_model_error(
                    &reply,
                    "external text generation failed",
                ));
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
    session: RwLock<Option<(String, PathBuf)>>,
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
            session: RwLock::new(None),
        }
    }

    async fn projection<'a>(
        &self,
        messages: &'a [Message],
        tools: &'a [Value],
    ) -> Result<(Vec<Message>, Cow<'a, [Value]>)> {
        // Model request Hooks operate on ephemeral provider input, never the
        // durable transcript or ToolRegistry. No extra history copies when a
        // Hook leaves the request unchanged.
        let mut view: Cow<'_, [Message]> = Cow::Borrowed(messages);
        let mut schemas: Cow<'_, [Value]> = Cow::Borrowed(tools);
        let (session_id, archive) = self
            .session
            .read()
            .ok()
            .and_then(|data| data.clone())
            .map_or((None, None), |(id, path)| (Some(id), Some(path)));
        for worker in &self.workers {
            let reply = worker
                .invoke_async(json!({
                    "type":"hook", "name":"before_model_request",
                    "session_id":session_id,"archive":archive,
                    "arguments":{"messages":view.as_ref(),"tools":schemas.as_ref()}
                }))
                .await?;
            if reply.is_error {
                bail!("model request hook rejected: {}", reply.content);
            }
            let mut patch: Value = serde_json::from_str(&reply.content)
                .context("before_model_request must return a JSON patch")?;
            let fields = patch
                .as_object_mut()
                .context("model request hook must return an object")?;
            if let Some(messages) = fields.remove("messages") {
                view = Cow::Owned(
                    serde_json::from_value(messages)
                        .context("invalid replacement model messages")?,
                );
            }
            if let Some(tools) = fields.remove("tools") {
                schemas = Cow::Owned(
                    serde_json::from_value(tools).context("invalid replacement model tools")?,
                );
            }
        }
        let mut planned = view.as_ref().to_vec();
        self.inner.project_for_budget(&mut planned);
        self.budget
            .validate_final_request(&planned, schemas.as_ref())?;
        Ok((view.into_owned(), schemas))
    }
}

impl ModelBackend for PluginModelHooks {
    fn set_session_context(&self, id: &str, archive: &Path) {
        self.inner.set_session_context(id, archive);
        if let Ok(mut session) = self.session.write() {
            *session = Some((id.to_owned(), archive.to_path_buf()));
        }
    }

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
            let (view, schemas) = self.projection(messages, tools).await?;
            self.inner.complete(&view, schemas.as_ref(), on_text).await
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
            let (view, schemas) = self.projection(messages, tools).await?;
            self.inner
                .complete_with_reasoning(&view, schemas.as_ref(), on_text, on_reasoning)
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
            let (view, schemas) = self.projection(messages, tools).await?;
            self.inner
                .complete_with_task_images(&view, schemas.as_ref(), images, on_text, on_reasoning)
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
