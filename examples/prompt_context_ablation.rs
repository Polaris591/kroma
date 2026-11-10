//! Benchmark-only 2x2 prompt/context ablation. Default runtime is unchanged.
//! p0c0=baseline; p1c0=short guidance; p0c1=readable tool results; p1c1=both.
//! The durable WAL, full stdout/stderr, error status and tool definitions remain
//! unchanged. No summarization, tool output deletion or extra model calls.
use anyhow::{bail, Context, Result};
use kroma::{
    app,
    config::Config,
    context::ContextConfig,
    model::{
        Completion, ModelBackend, ModelConnection, ModelFuture, NativeActivity, StreamFuture,
        TaskImageCache, TextResult,
    },
    protocol::Message,
};
use serde_json::Value;
use std::{collections::HashSet, path::Path, sync::Arc};

const SHORT_GUIDANCE: &str = "\nFor code changes: locate the named failure and its implementation together. Read bounded evidence, then make the smallest supported edit once the cause is clear; investigate further only when specific evidence is missing. Run the requested targeted tests and relevant regressions after editing. If a test fails, fix its cause and rerun it. Finish after required checks pass. Do not skip verification or change unrelated files.";

#[derive(Clone)]
struct ReadableShellContext {
    inner: Arc<dyn ModelBackend>,
}

fn readable_shell_results(messages: &mut [Message]) -> usize {
    let shell_ids: HashSet<String> = messages
        .iter()
        .filter(|msg| msg.role == "assistant")
        .flat_map(|msg| msg.tool_calls.as_deref().unwrap_or_default())
        .filter(|call| call.function.name == "shell")
        .map(|call| call.id.clone())
        .collect();
    let mut changed = 0;
    for msg in messages.iter_mut() {
        if msg.role != "tool"
            || !msg
                .tool_call_id
                .as_ref()
                .is_some_and(|id| shell_ids.contains(id))
        {
            continue;
        }
        let Some(raw) = msg.content.as_deref() else {
            continue;
        };
        let Ok(payload) = serde_json::from_str::<Value>(raw) else {
            continue;
        };
        let Some(obj) = payload.as_object() else {
            continue;
        };
        if !obj.contains_key("stdout")
            || !obj.contains_key("stderr")
            || !obj.contains_key("exit_code")
        {
            continue;
        }
        let Some(stdout) = obj.get("stdout").and_then(Value::as_str) else {
            continue;
        };
        let Some(stderr) = obj.get("stderr").and_then(Value::as_str) else {
            continue;
        };
        let status = obj
            .get("exit_code")
            .map(Value::to_string)
            .unwrap_or_else(|| "null".into());
        let mut result = format!("[shell result; exit_code={status}]\n");
        for key in [
            "signal",
            "stdout_file",
            "stderr_file",
            "stdout_bytes",
            "stderr_bytes",
        ] {
            if let Some(value) = obj.get(key) {
                result.push_str(&format!("{key}={value}\n"));
            }
        }
        result.push_str("[stdout]\n");
        result.push_str(stdout);
        result.push_str("\n[stderr]\n");
        result.push_str(stderr);
        for (key, value) in obj {
            if !matches!(
                key.as_str(),
                "exit_code"
                    | "signal"
                    | "stdout"
                    | "stderr"
                    | "stdout_file"
                    | "stderr_file"
                    | "stdout_bytes"
                    | "stderr_bytes"
            ) {
                result.push_str(&format!("\n[extra field {key}] {value}"));
            }
        }
        msg.content = Some(result);
        changed += 1;
    }
    changed
}

impl ModelBackend for ReadableShellContext {
    fn connection_info(&self) -> Option<ModelConnection> {
        self.inner.connection_info()
    }
    fn set_session_context(&self, id: &str, archive: &Path) {
        self.inner.set_session_context(id, archive);
    }
    fn has_task_finished_hook(&self) -> bool {
        self.inner.has_task_finished_hook()
    }
    fn task_finished<'a>(
        &'a self,
        session_id: &'a str,
        archive: &'a Path,
        messages: &'a [Message],
        status: &'a str,
    ) -> ModelFuture<'a, ()> {
        self.inner
            .task_finished(session_id, archive, messages, status)
    }
    fn project_for_budget(&self, messages: &mut [Message]) {
        readable_shell_results(messages);
        self.inner.project_for_budget(messages);
    }
    fn accepts_images(&self) -> Option<bool> {
        self.inner.accepts_images()
    }
    fn native_web_search(&self) -> bool {
        self.inner.native_web_search()
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
            let mut view = messages.to_vec();
            let changed = readable_shell_results(&mut view);
            eprintln!("[ablation] readable_shell_tool_results={changed}");
            self.inner.complete(&view, tools, on_text).await
        })
    }
    fn complete_with_activity<'a>(
        &'a self,
        messages: &'a [Message],
        tools: &'a [Value],
        images: &'a TaskImageCache,
        on_text: &'a mut (dyn FnMut(&str) + Send),
        on_reasoning: &'a mut (dyn FnMut(&str) + Send),
        on_activity: &'a mut (dyn FnMut(NativeActivity) + Send),
    ) -> StreamFuture<'a, Completion> {
        Box::pin(async move {
            let mut view = messages.to_vec();
            let changed = readable_shell_results(&mut view);
            eprintln!("[ablation] readable_shell_tool_results={changed}");
            self.inner
                .complete_with_activity(&view, tools, images, on_text, on_reasoning, on_activity)
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
    fn generate_checkpoint<'a>(
        &'a self,
        messages: &'a [Message],
        tools: &'a [Value],
        instruction: &'a str,
        output_tokens: usize,
    ) -> ModelFuture<'a, TextResult> {
        self.inner
            .generate_checkpoint(messages, tools, instruction, output_tokens)
    }
    fn generate_checkpoint_with_ceiling<'a>(
        &'a self,
        messages: &'a [Message],
        tools: &'a [Value],
        instruction: &'a str,
        output_tokens: usize,
        max_completion_tokens: usize,
    ) -> ModelFuture<'a, TextResult> {
        self.inner.generate_checkpoint_with_ceiling(
            messages,
            tools,
            instruction,
            output_tokens,
            max_completion_tokens,
        )
    }
    fn checkpoint_output_ceiling(&self, output_tokens: usize, reserve: usize) -> usize {
        self.inner.checkpoint_output_ceiling(output_tokens, reserve)
    }
    fn usage_matches_projection(&self) -> bool {
        self.inner.usage_matches_projection()
    }
    fn invalidate_ephemeral_context(&self) {
        self.inner.invalidate_ephemeral_context();
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

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let arm = args
        .next()
        .context("usage: prompt_context_ablation p0c0|p1c0|p0c1|p1c1 <task>")?;
    if !["p0c0", "p1c0", "p0c1", "p1c1"].contains(&arm.as_str()) {
        bail!("unknown arm {arm}");
    }
    let prompt = args.collect::<Vec<_>>().join(" ");
    if prompt.trim().is_empty() {
        bail!("task must not be empty");
    }
    let optimized_prompt = arm.starts_with("p1");
    let readable_context = arm.ends_with("c1");

    let mut config = Config::from_env()?;
    config.reasoning_effort = "high".into();
    config.deepseek_thinking = None;

    let runtime = app::default_runtime_with(config, |components| {
        if optimized_prompt {
            components
                .prompt
                .replace_execution_guidance(SHORT_GUIDANCE)?;
        }
        if readable_context {
            let report = components
                .model_report()
                .cloned()
                .context("model report missing")?;
            let window = report
                .context_window
                .value
                .context("context window required")?;
            let reserve = report
                .output_reserve
                .value
                .context("output reserve required")?;
            let budget = ContextConfig::with_output_reserve(window, reserve)?;
            components.replace_model_with_report(
                Arc::new(ReadableShellContext {
                    inner: components.model(),
                }),
                budget,
                report,
            );
        }
        Ok(())
    })
    .await?;
    let mut agent = runtime.agent;
    std::env::remove_var("KROMA_SESSION_FILE");
    let answer = agent.run(&prompt, &mut |_| {}).await?;
    println!("{answer}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use kroma::protocol::{FunctionCall, ToolCall};

    #[test]
    fn readable_context_preserves_stdout_stderr_exit_code_and_persistent_history() {
        let assistant = Message {
            tool_calls: Some(vec![ToolCall {
                id: "s1".into(),
                kind: "function".into(),
                function: FunctionCall {
                    name: "shell".into(),
                    arguments: "{\"command\":\"rg x\"}".into(),
                },
            }]),
            ..Message::new("assistant", "")
        };
        let raw = r#"{"exit_code":1,"stdout":"value=x\nline 2","stderr":"error: x","stdout_file":"C:/logs/1"}"#;
        let mut view = vec![assistant, Message::tool("s1".into(), raw.into())];
        let durable = view.clone();
        assert_eq!(readable_shell_results(&mut view), 1);
        let content = view[1].content.as_deref().unwrap();
        assert!(content.contains("exit_code=1"));
        assert!(content.contains("value=x\nline 2"));
        assert!(content.contains("error: x"));
        assert!(content.contains("C:/logs/1"));
        assert_eq!(durable[1].content.as_deref(), Some(raw));
        assert_eq!(readable_shell_results(&mut view), 0);
    }

    #[test]
    fn non_shell_tool_results_remain_identical() {
        let assistant = Message {
            tool_calls: Some(vec![ToolCall {
                id: "edit".into(),
                kind: "function".into(),
                function: FunctionCall {
                    name: "edit_file".into(),
                    arguments: "{}".into(),
                },
            }]),
            ..Message::new("assistant", "")
        };
        let raw = r#"{"exit_code":0,"stdout":"code","stderr":""}"#;
        let mut view = vec![assistant, Message::tool("edit".into(), raw.into())];
        assert_eq!(readable_shell_results(&mut view), 0);
        assert_eq!(view[1].content.as_deref(), Some(raw));
    }
}
