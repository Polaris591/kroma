//! Tool dispatch belongs here; the Agent orchestrates model turns but does not
//! parse tool arguments or know how individual tools execute.
use crate::{protocol::ToolCall, shell};
use anyhow::{bail, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{future::Future, path::Path, pin::Pin, sync::Arc};

pub type ToolFuture<'a> = Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>>;

/// Structured runtime outcome. The model transcript still persists the
/// content string unchanged, preserving existing tool-call wire formats.
/// Hosts and extensions can distinguish execution errors without parsing JSON.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolResult {
    pub content: String,
    pub is_error: bool,
}

impl ToolResult {
    pub fn ok(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
        }
    }

    pub fn error(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
        }
    }
}

#[derive(Clone, Copy)]
pub struct ToolContext<'a> {
    pub workdir: &'a Path,
    pub archive: &'a Path,
    pub on_output: Option<&'a shell::OutputCallback<'a>>,
}

pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn definition(&self) -> Value;
    fn execute<'a>(&'a self, call: &'a ToolCall, ctx: ToolContext<'a>) -> ToolFuture<'a>;
}

/// The order of registration is the order presented to the model.
/// Calls are still executed sequentially by Agent, not by the registry.
#[derive(Default)]
pub struct ToolRegistry {
    tools: Vec<Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn builtin() -> Self {
        Self {
            tools: vec![Arc::new(BuiltinShell)],
        }
    }

    pub fn register(&mut self, tool: Arc<dyn Tool>) -> Result<()> {
        if tool.name().trim().is_empty() {
            bail!("tool name must not be empty");
        }
        if self
            .tools
            .iter()
            .any(|existing| existing.name() == tool.name())
        {
            bail!("duplicate tool name: {}", tool.name());
        }
        self.tools.push(tool);
        Ok(())
    }

    pub fn definitions(&self) -> Vec<Value> {
        self.tools.iter().map(|tool| tool.definition()).collect()
    }

    pub fn is_builtin_only(&self) -> bool {
        self.tools.len() == 1
            && self.tools[0].name() == "shell"
            && self.tools[0].definition() == definitions()[0]
    }

    pub async fn execute(&self, call: &ToolCall, ctx: ToolContext<'_>) -> ToolResult {
        if call.kind != "function" {
            return ToolResult::error(
                json!({"error": format!("unsupported tool: {}", call.function.name)}).to_string(),
            );
        }
        match self
            .tools
            .iter()
            .find(|tool| tool.name() == call.function.name)
        {
            Some(tool) => tool.execute(call, ctx).await,
            None => ToolResult::error(
                json!({"error": format!("unsupported tool: {}", call.function.name)}).to_string(),
            ),
        }
    }
}

pub struct BuiltinShell;

impl Tool for BuiltinShell {
    fn name(&self) -> &str {
        "shell"
    }

    fn definition(&self) -> Value {
        definitions().remove(0)
    }

    fn execute<'a>(&'a self, call: &'a ToolCall, ctx: ToolContext<'a>) -> ToolFuture<'a> {
        Box::pin(async move {
            match execute_inner(call, ctx.workdir, ctx.archive, ctx.on_output).await {
                Ok(output) => {
                    // A signal-terminated process has no numeric exit code.
                    // Treat missing/null as abnormal termination, not success.
                    let failed = serde_json::from_str::<Value>(&output)
                        .ok()
                        .and_then(|value| value.get("exit_code").and_then(Value::as_i64))
                        != Some(0);
                    if failed {
                        ToolResult::error(output)
                    } else {
                        ToolResult::ok(output)
                    }
                }
                Err(error) => ToolResult::error(json!({"error": error.to_string()}).to_string()),
            }
        })
    }
}

#[derive(Deserialize)]
struct ShellArgs {
    command: String,
}

pub async fn execute(call: &ToolCall, workdir: &Path, archive: &Path) -> ToolResult {
    BuiltinShell
        .execute(
            call,
            ToolContext {
                workdir,
                archive,
                on_output: None,
            },
        )
        .await
}

/// OpenAI-compatible schema for the builtin tool. Agent and Provider do not
/// need to know its arguments or implementation.
pub fn definitions() -> Vec<serde_json::Value> {
    // The dialect is resolved from the host, so the schema always describes the
    // shell the command will really run in.
    let description = shell::tool_description();
    vec![json!({
        "type": "function",
        "function": {
            "name": "shell",
            "description": description,
            "parameters": {
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "Shell command to execute"}
                },
                "required": ["command"],
                "additionalProperties": false
            }
        }
    })]
}

async fn execute_inner(
    call: &ToolCall,
    workdir: &Path,
    archive: &Path,
    on_output: Option<&shell::OutputCallback<'_>>,
) -> Result<String> {
    if call.kind != "function" || call.function.name != "shell" {
        bail!("unsupported tool: {}", call.function.name);
    }
    let args: ShellArgs = serde_json::from_str(&call.function.arguments)?;
    crate::runtime_log!("[shell] {}", args.command);
    let output =
        shell::execute_for_session_with_progress(&args.command, workdir, archive, on_output)
            .await?;
    // Large outputs remain file-backed. Inline stdout/stderr bytes are
    // redundant with the complete returned strings and need not occupy
    // model context. Preserve file references and byte totals when omitted.
    let mut payload = serde_json::to_value(&output)?;
    let object = payload
        .as_object_mut()
        .expect("ShellOutput is a JSON object");
    if !object.contains_key("stdout_file") {
        object.remove("stdout_bytes");
    }
    if !object.contains_key("stderr_file") {
        object.remove("stderr_bytes");
    }
    Ok(payload.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::FunctionCall;

    #[tokio::test]
    async fn tool_dispatch_rejects_unknown_tool_without_execution() {
        let call = ToolCall {
            id: "t1".into(),
            kind: "function".into(),
            function: FunctionCall {
                name: "not-shell".into(),
                arguments: "{}".into(),
            },
        };
        let result = execute(&call, Path::new("."), Path::new("fake.json")).await;
        assert!(result.is_error);
        assert!(result.content.contains("unsupported tool"));
    }

    #[tokio::test]
    async fn inline_tool_output_omits_redundant_byte_counts() {
        let call = ToolCall {
            id: "t2".into(),
            kind: "function".into(),
            function: FunctionCall {
                name: "shell".into(),
                arguments: r#"{"command":"printf p4-inline-ok"}"#.into(),
            },
        };
        let path = std::env::temp_dir();
        let outcome = execute(&call, &path, Path::new("/tmp/p4-test.json")).await;
        assert!(!outcome.is_error);
        let result: serde_json::Value = serde_json::from_str(&outcome.content).unwrap();
        assert_eq!(result["exit_code"], 0);
        assert_eq!(result["stdout"], "p4-inline-ok");
        assert!(result.get("stdout_bytes").is_none());
        assert!(result.get("stderr_bytes").is_none());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn bash_features_work_when_installed() {
        if !Path::new("/bin/bash").is_file() {
            return;
        }
        let result = shell::execute(
            "set -o pipefail; false | true; echo ${PIPESTATUS[0]}",
            Path::new("/tmp"),
        )
        .await
        .unwrap();
        let content = serde_json::to_value(result).unwrap();
        assert_eq!(content["stdout"], "1\n");
        assert_eq!(content["exit_code"], 0);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn signal_termination_is_not_reported_as_a_successful_tool() {
        let call = ToolCall {
            id: "signal-test".into(),
            kind: "function".into(),
            function: FunctionCall {
                name: "shell".into(),
                arguments: r#"{"command":"kill -TERM $$"}"#.into(),
            },
        };
        let result = execute(
            &call,
            Path::new("/tmp"),
            Path::new("/tmp/kroma-signal-test.json"),
        )
        .await;
        let data: Value = serde_json::from_str(&result.content).unwrap();
        assert!(result.is_error);
        assert_eq!(data["exit_code"], Value::Null);
        assert_eq!(data["signal"], 15);
    }
}
