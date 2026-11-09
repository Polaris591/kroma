//! Tool dispatch belongs here; the Agent orchestrates model turns but does not
//! parse tool arguments or know how individual tools execute.
use crate::{protocol::ToolCall, shell};
use anyhow::{bail, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{future::Future, path::Path, pin::Pin, sync::Arc};

pub type ToolFuture<'a> = Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>>;
pub type HookFuture<'a> = Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>>;

/// Structured runtime outcome. The model transcript still persists the
/// content string unchanged, preserving existing tool-call wire formats.
/// Hosts and extensions can distinguish execution errors without parsing JSON.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolResult {
    pub content: String,
    pub is_error: bool,
    /// Effective arguments when a before-tool hook rewrote the call.
    /// Original model call remains immutable for provider replay.
    pub executed_arguments: Option<String>,
}

impl ToolResult {
    pub fn ok(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
            executed_arguments: None,
        }
    }

    pub fn error(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
            executed_arguments: None,
        }
    }
}

/// One interactive choice request: a question plus the options a host can
/// render. The shape lives here because only the host draws the list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AskRequest {
    pub question: String,
    pub options: Vec<String>,
    pub multiple: bool,
}

/// What the person actually did with an open question.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AskAnswer {
    /// Chosen options and/or the answer typed instead of choosing.
    Answered {
        picked: Vec<String>,
        typed: Option<String>,
    },
    /// Closed without answering. This is not consent and must never be
    /// reported back to the model as an approval.
    Dismissed,
}

/// Host-provided way to ask the person a question and wait for the answer.
/// `None` means the running host has no interactive surface for this run.
pub trait Asker: Send + Sync {
    fn ask<'a>(
        &'a self,
        request: AskRequest,
    ) -> Pin<Box<dyn Future<Output = Option<AskAnswer>> + Send + 'a>>;
}

#[derive(Clone, Copy)]
pub struct ToolContext<'a> {
    pub model_connection: Option<&'a crate::model::ModelConnection>,
    /// Optional current user instruction, provided by the host, never by
    /// tool arguments. A plugin must explicitly opt in to receive it.
    pub user_message: Option<&'a str>,
    pub user_turn: Option<usize>,
    /// Immutable image attachment references from the latest user turn.
    /// Only tools explicitly declaring requires_user_attachments receive them.
    pub user_images: Option<&'a [Value]>,
    /// Images from the immediately preceding user turn for short follow-ups.
    pub previous_user_images: Option<&'a [Value]>,
    pub workdir: &'a Path,
    pub archive: &'a Path,
    pub on_output: Option<&'a shell::OutputCallback<'a>>,
    /// Present only when this run can show an interactive question.
    pub ask: Option<&'a dyn Asker>,
}

pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn definition(&self) -> Value;
    /// Skip finding message attachments for tools that cannot use them.
    fn requires_user_attachments(&self) -> bool {
        false
    }
    /// A successful execution mutates reference data used by model-level
    /// ephemeral context packs. Plugins declare this in their manifest; the
    /// core does not need to know a particular plugin's tool name.
    fn invalidates_model_context(&self) -> bool {
        false
    }
    /// A tool that waits for a person is not a stuck tool: the idle watchdog
    /// that guards silent commands must not abort it.
    fn waits_for_user(&self) -> bool {
        false
    }
    /// Assume an external tool can edit project files unless it explicitly
    /// declares itself read-only. This also covers MCP and plugin tools.
    fn may_mutate_worktree(&self) -> bool {
        true
    }
    /// Tools with a complete call deadline should not be cancelled earlier by
    /// the Agent's generic no-output watchdog.
    fn manages_timeout(&self) -> bool {
        false
    }
    fn execute<'a>(&'a self, call: &'a ToolCall, ctx: ToolContext<'a>) -> ToolFuture<'a>;
}

/// Interceptors can rewrite a call before dispatch and its result afterward.
/// Each hook runs in registration order. The durable tool-call id and tool
/// name remain Kroma-owned; they cannot be changed by a plugin.
pub trait ToolInterceptor: Send + Sync {
    /// Legacy Rust host extension boundary; retained for source compatibility.
    fn before<'a>(&'a self, _call: &'a mut ToolCall, _workdir: &'a Path) -> HookFuture<'a> {
        Box::pin(async { Ok(()) })
    }
    fn after<'a>(
        &'a self,
        _call: &'a ToolCall,
        _result: &'a mut ToolResult,
        _workdir: &'a Path,
    ) -> HookFuture<'a> {
        Box::pin(async { Ok(()) })
    }
    /// Rich context for new plugins. Existing Rust hooks automatically retain
    /// the original workdir-only behavior without any adapter allocation.
    fn before_with_context<'a>(
        &'a self,
        call: &'a mut ToolCall,
        ctx: ToolContext<'a>,
    ) -> HookFuture<'a> {
        self.before(call, ctx.workdir)
    }
    fn after_with_context<'a>(
        &'a self,
        call: &'a ToolCall,
        result: &'a mut ToolResult,
        ctx: ToolContext<'a>,
    ) -> HookFuture<'a> {
        self.after(call, result, ctx.workdir)
    }
}

/// The order of registration is the order presented to the model.
/// Calls are still executed sequentially by Agent, not by the registry.
#[derive(Default, Clone)]
pub struct ToolRegistry {
    tools: Vec<Arc<dyn Tool>>,
    interceptors: Vec<Arc<dyn ToolInterceptor>>,
    capture_facts: bool,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn builtin() -> Self {
        Self {
            tools: vec![Arc::new(BuiltinShell)],
            interceptors: Vec::new(),
            capture_facts: false,
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

    pub fn replace(&mut self, name: &str, tool: Arc<dyn Tool>) -> Result<()> {
        if name != tool.name() {
            bail!("replacement name must match existing tool '{name}'");
        }
        let index = self
            .tools
            .iter()
            .position(|existing| existing.name() == name)
            .ok_or_else(|| anyhow::anyhow!("unknown tool for replacement: {name}"))?;
        self.tools[index] = tool;
        Ok(())
    }

    pub fn disable(&mut self, name: &str) -> Result<()> {
        let index = self
            .tools
            .iter()
            .position(|tool| tool.name() == name)
            .ok_or_else(|| anyhow::anyhow!("unknown tool: {name}"))?;
        self.tools.remove(index);
        Ok(())
    }

    pub(crate) fn capture_execution_facts(&mut self, enabled: bool) {
        self.capture_facts = enabled;
    }

    pub fn register_interceptor(&mut self, interceptor: Arc<dyn ToolInterceptor>) {
        self.interceptors.push(interceptor);
    }

    pub fn definitions(&self) -> Vec<Value> {
        self.tools.iter().map(|tool| tool.definition()).collect()
    }

    pub fn invalidates_model_context(&self, name: &str) -> bool {
        self.tools
            .iter()
            .any(|tool| tool.name() == name && tool.invalidates_model_context())
    }

    pub fn requires_user_attachments(&self, name: &str) -> bool {
        self.tools
            .iter()
            .any(|tool| tool.name() == name && tool.requires_user_attachments())
    }

    pub(crate) fn waits_for_user(&self, name: &str) -> bool {
        self.tools
            .iter()
            .any(|tool| tool.name() == name && tool.waits_for_user())
    }

    pub(crate) fn may_mutate_worktree(&self, name: &str) -> bool {
        // Even a read-only tool can be preceded by a side-effectful hook.
        !self.interceptors.is_empty()
            || self
                .tools
                .iter()
                .any(|tool| tool.name() == name && tool.may_mutate_worktree())
    }

    pub(crate) fn manages_timeout(&self, name: &str) -> bool {
        self.tools
            .iter()
            .any(|tool| tool.name() == name && tool.manages_timeout())
    }

    pub fn is_builtin_only(&self) -> bool {
        self.tools.len() == 1
            && self.tools[0].name() == "shell"
            && self.tools[0].definition() == definitions()[0]
            && self.interceptors.is_empty()
    }

    pub async fn execute(&self, call: &ToolCall, ctx: ToolContext<'_>) -> ToolResult {
        let execution = self.execute_with_warnings(call, ctx).await;
        for warning in execution.warnings {
            crate::runtime_log!("[tool] {warning}");
        }
        execution.result
    }

    /// Execute a tool while keeping interceptor diagnostics separate from the
    /// real outcome. A failing after hook must not turn a completed side effect
    /// into an error result, because that invites the model to repeat it.
    /// Execute one call and keep the raw facts that a hook may later rewrite.
    pub(crate) async fn execute_with_warnings(
        &self,
        call: &ToolCall,
        ctx: ToolContext<'_>,
    ) -> ToolExecution {
        if call.kind != "function" {
            return ToolExecution {
                result: ToolResult::error(
                    json!({"error": format!("unsupported tool: {}", call.function.name)})
                        .to_string(),
                ),
                warnings: Vec::new(),
                raw: RawExecution::default(),
            };
        }
        // The default (no-hook) execution path performs no ToolCall clone,
        // allocation or extra IPC. Plugins only pay for their own hooks.
        if self.interceptors.is_empty() {
            let result = match self
                .tools
                .iter()
                .find(|tool| tool.name() == call.function.name)
            {
                Some(tool) => tool.execute(call, ctx).await,
                None => ToolResult::error(
                    json!({"error": format!("unsupported tool: {}", call.function.name)})
                        .to_string(),
                ),
            };
            let raw = if self.capture_facts {
                raw_execution(
                    &call.function.name,
                    &call.function.arguments,
                    &result,
                    result.is_error,
                )
            } else {
                RawExecution::default()
            };
            return ToolExecution {
                result,
                warnings: Vec::new(),
                raw,
            };
        }
        let mut effective = call.clone();
        for hook in &self.interceptors {
            if let Err(error) = hook.before_with_context(&mut effective, ctx).await {
                return ToolExecution {
                    result: ToolResult::error(format!("before_tool_call failed: {error:#}")),
                    warnings: Vec::new(),
                    raw: RawExecution::default(),
                };
            }
            if effective.id != call.id
                || effective.kind != call.kind
                || effective.function.name != call.function.name
            {
                return ToolExecution {
                    result: ToolResult::error(
                        "tool interceptor cannot change call id, kind or name",
                    ),
                    warnings: Vec::new(),
                    raw: RawExecution::default(),
                };
            }
        }
        // The Agent checks a model-declared edit_file path for nested
        // AGENTS.md before executing a batch. A before hook may rewrite
        // other arguments, but changing the path afterward would bypass that
        // check. Reject only this explicit path change, not general Shell
        // command rewrites, which cannot be safely parsed as file paths.
        if effective.function.name == "edit_file" {
            let path_of = |arguments: &str| {
                serde_json::from_str::<Value>(arguments)
                    .ok()
                    .and_then(|json| json.get("path").and_then(Value::as_str).map(str::to_owned))
            };
            if path_of(&call.function.arguments) != path_of(&effective.function.arguments) {
                return ToolExecution {
                    result: ToolResult::error(
                        "before_tool_call changed edit_file path after project instruction preflight; ask the model to replan with the new file path",
                    ),
                    warnings: Vec::new(),
                    raw: RawExecution::default(),
                };
            }
        }
        let mut result = match self
            .tools
            .iter()
            .find(|tool| tool.name() == effective.function.name)
        {
            Some(tool) => tool.execute(&effective, ctx).await,
            None => ToolResult::error(
                json!({"error": format!("unsupported tool: {}", effective.function.name)})
                    .to_string(),
            ),
        };
        if effective.function.arguments != call.function.arguments {
            result.executed_arguments = Some(effective.function.arguments.clone());
        }
        // Capture facts before the after hook: a rewritten result must never
        // manufacture a passing check.
        let raw = if self.capture_facts {
            raw_execution(
                &effective.function.name,
                &effective.function.arguments,
                &result,
                result.is_error,
            )
        } else {
            RawExecution::default()
        };
        let mut warnings = Vec::new();
        for hook in &self.interceptors {
            // Hooks receive &mut and may patch the result before failing. Keep
            // the last committed outcome so a failed hook cannot leave a
            // partial patch, then stop: later hooks were promised a result
            // whose provenance has already been disrupted.
            let committed = result.clone();
            if let Err(error) = hook.after_with_context(&effective, &mut result, ctx).await {
                result = committed;
                warnings.push(format!("after_tool_result failed: {error:#}"));
                break;
            }
        }
        ToolExecution {
            result,
            warnings,
            raw,
        }
    }
}

/// Raw execution facts captured before any `after_tool_result` hook can rewrite
/// the result. Host-private: not part of [`ToolResult`] or the [`Tool`] trait, so
/// third-party tools keep the existing contract.
#[derive(Debug, Clone, Default)]
pub(crate) struct RawExecution {
    pub is_error: bool,
    pub command: Option<String>,
    pub exit_code: Option<i32>,
}

pub(crate) struct ToolExecution {
    pub result: ToolResult,
    pub warnings: Vec<String>,
    pub raw: RawExecution,
}

/// The built-in shell reports its own decoded command and exit status in its
/// structured envelope. Unknown or absent values stay `None`, never 0.
/// A tool that produced nothing for a whole idle window is indistinguishable
/// from a stuck one. Dropping its future stops waiting, but only some tools
/// (notably the shell) can guarantee process cancellation; side effects remain.
pub(crate) fn idle_timeout(
    name: &str,
    limit: std::time::Duration,
    arguments: &str,
) -> ToolExecution {
    let content = format!(
        "[no output] tool '{name}' produced no output for {}s; Kroma stopped waiting for the result. \
The operation may have partially completed, or a remote/plugin task may still be running. \
Its outcome is unknown: inspect the state before retrying. \
Set KROMA_TOOL_IDLE_SECS=0 to disable this guard.",
        limit.as_secs()
    );
    let result = ToolResult::error(content);
    ToolExecution {
        raw: raw_execution(name, arguments, &result, true),
        result,
        warnings: Vec::new(),
    }
}

fn raw_execution(name: &str, arguments: &str, result: &ToolResult, is_error: bool) -> RawExecution {
    let mut raw = RawExecution {
        is_error,
        ..RawExecution::default()
    };
    if name != "shell" {
        return raw;
    }
    raw.command = serde_json::from_str::<Value>(arguments)
        .ok()
        .and_then(|value| {
            value
                .get("command")
                .and_then(Value::as_str)
                .map(str::to_owned)
        });
    raw.exit_code = serde_json::from_str::<Value>(&result.content)
        .ok()
        .and_then(|value| value.get("exit_code").and_then(Value::as_i64))
        .and_then(|code| i32::try_from(code).ok());
    raw
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
                // Failure comes from the structured exit status, not from
                // re-parsing the already-serialized tool payload.
                Ok(output) if output.failed => ToolResult::error(output.content),
                Ok(output) => ToolResult::ok(output.content),
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
                model_connection: None,
                user_message: None,
                user_turn: None,
                user_images: None,
                previous_user_images: None,
                workdir,
                archive,
                on_output: None,
                ask: None,
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

struct ShellExecution {
    content: String,
    /// Missing exit codes are signal or abnormal termination, never success.
    failed: bool,
}

async fn execute_inner(
    call: &ToolCall,
    workdir: &Path,
    archive: &Path,
    on_output: Option<&shell::OutputCallback<'_>>,
) -> Result<ShellExecution> {
    // The registry dispatches by name, but `execute` is also called directly
    // by hosts, so the name is checked here rather than trusted.
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
    let failed = output.exit_code != Some(0);
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
    Ok(ShellExecution {
        content: payload.to_string(),
        failed,
    })
}

/// The host-side tool behind a plugin that declares `host = "ask_user"`.
/// The plugin supplies the name and the model-facing description; the argument
/// shape, the wait and the answer wording stay here because they depend on what
/// this host can actually render.
pub struct AskTool {
    name: String,
    description: String,
}

impl AskTool {
    pub fn new(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
        }
    }

    /// The only declared copy of the argument shape. A manifest may not
    /// declare its own, so the list the model fills is the list we draw.
    pub fn parameters() -> Value {
        json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["question", "options"],
            "properties": {
                "question": {
                    "type": "string",
                    "description": "One question, addressed to the person who will answer it."
                },
                "options": {
                    "type": "array",
                    "minItems": 2,
                    "items": {"type": "string"},
                    "description": "Two or more concrete choices. A free-text choice is always appended, so never add one."
                },
                "multiple": {
                    "type": "boolean",
                    "description": "True when more than one choice may apply."
                }
            }
        })
    }
}

impl Tool for AskTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn definition(&self) -> Value {
        json!({"type": "function", "function": {
            "name": self.name,
            "description": self.description,
            "parameters": Self::parameters()
        }})
    }

    fn waits_for_user(&self) -> bool {
        true
    }

    fn may_mutate_worktree(&self) -> bool {
        false
    }

    fn execute<'a>(&'a self, call: &'a ToolCall, ctx: ToolContext<'a>) -> ToolFuture<'a> {
        Box::pin(async move {
            let request = match parse_ask(&call.function.arguments) {
                Ok(request) => request,
                Err(error) => return ToolResult::error(format!("invalid ask call: {error:#}")),
            };
            let Some(asker) = ctx.ask else {
                return ToolResult::error(
                    "This interface cannot show a question. Decide from what you already know and say what you chose.",
                );
            };
            match asker.ask(request).await {
                Some(AskAnswer::Answered { picked, typed }) => {
                    ToolResult::ok(format_answer(&picked, typed.as_deref()))
                }
                Some(AskAnswer::Dismissed) => {
                    ToolResult::error("User dismissed the question without answering")
                }
                None => ToolResult::error(
                    "The question could not be shown. Decide from what you already know and say what you chose.",
                ),
            }
        })
    }
}

fn parse_ask(arguments: &str) -> Result<AskRequest> {
    #[derive(Deserialize)]
    struct Args {
        question: String,
        options: Vec<String>,
        #[serde(default)]
        multiple: bool,
    }
    let args: Args = serde_json::from_str(arguments)?;
    let question = args.question.trim();
    if question.is_empty() {
        bail!("question must not be empty");
    }
    let mut options: Vec<String> = Vec::new();
    for option in args.options {
        let option = option.trim();
        if option.is_empty() {
            bail!("options must not contain empty strings");
        }
        if options.iter().any(|existing| existing == option) {
            bail!("options must be distinct; '{option}' appears twice");
        }
        options.push(option.to_owned());
    }
    if options.len() < 2 {
        bail!("options needs at least two choices; ask an open question in plain text instead");
    }
    Ok(AskRequest {
        question: question.to_owned(),
        options,
        multiple: args.multiple,
    })
}

/// The answer is scoped to the one question that was asked. It never claims
/// approval of anything else, and a typed answer replaces the choice.
fn format_answer(picked: &[String], typed: Option<&str>) -> String {
    let mut parts = Vec::new();
    if !picked.is_empty() {
        parts.push(format!("User selected: {}", picked.join(", ")));
    }
    if let Some(text) = typed.map(str::trim).filter(|text| !text.is_empty()) {
        parts.push(format!("User typed: {text}"));
    }
    if parts.is_empty() {
        "User answered without choosing an option".to_owned()
    } else {
        parts.join(
            "
",
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::FunctionCall;

    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    struct NoopHook;
    impl ToolInterceptor for NoopHook {}

    #[tokio::test]
    #[ignore = "local dispatch/process benchmark; no provider requests"]
    async fn perf_tool_dispatch_and_shell_process() -> Result<()> {
        use std::{hint::black_box, time::Instant};
        let tool = Arc::new(EchoForHook);
        let mut registry = ToolRegistry::new();
        registry.register(tool.clone())?;
        let mut hooked = registry.clone();
        hooked.register_interceptor(Arc::new(NoopHook));
        let workdir = std::env::temp_dir();
        let archive = workdir.join("kroma-perf-unused-session.json");
        let ctx = ToolContext {
            model_connection: None,
            user_message: None,
            user_turn: None,
            user_images: None,
            previous_user_images: None,
            workdir: &workdir,
            archive: &archive,
            on_output: None,
            ask: None,
        };
        let call = test_call("echo_hook", r#"{"value":"sample"}"#);
        let mut samples = [Vec::new(), Vec::new(), Vec::new()];
        for round in 0..33 {
            for kind in if round % 2 == 0 { [0, 1, 2] } else { [2, 1, 0] } {
                let start = Instant::now();
                for _ in 0..1000 {
                    let result = match kind {
                        0 => tool.execute(&call, ctx).await,
                        1 => registry.execute(&call, ctx).await,
                        _ => hooked.execute(&call, ctx).await,
                    };
                    black_box(result);
                }
                if round > 1 {
                    samples[kind].push(start.elapsed() / 1000);
                }
            }
        }
        for (name, times) in ["tool.direct", "tool.registry", "tool.noop_hook"]
            .into_iter()
            .zip(samples)
        {
            crate::performance::report(
                name,
                &times,
                json!({"iterations_per_sample":1000,"in_process":true,"alternating_order":true}),
            );
        }
        let shell = ToolRegistry::builtin();
        let call = test_call("shell", r#"{"command":"echo perf"}"#);
        let mut samples = Vec::new();
        for _ in 0..11 {
            let start = Instant::now();
            let result = shell.execute(&call, ctx).await;
            samples.push(start.elapsed());
            assert!(!result.is_error, "{}", result.content);
        }
        crate::performance::report(
            "tool.shell_echo",
            &samples,
            json!({"command":"echo perf","includes_process_start":true}),
        );
        Ok(())
    }

    fn test_call(name: &str, arguments: &str) -> ToolCall {
        ToolCall {
            id: format!("call-{name}"),
            kind: "function".into(),
            function: FunctionCall {
                name: name.into(),
                arguments: arguments.into(),
            },
        }
    }

    fn test_context() -> ToolContext<'static> {
        ToolContext {
            model_connection: None,
            user_message: None,
            user_turn: None,
            user_images: None,
            previous_user_images: None,
            workdir: Path::new("."),
            archive: Path::new("session.json"),
            on_output: None,
            ask: None,
        }
    }

    #[test]
    fn checkpoint_policy_covers_mutating_tools_and_hooks_only() {
        let mut registry = ToolRegistry::new();
        registry
            .register(Arc::new(crate::history_tools::HistorySearch))
            .unwrap();
        registry
            .register(Arc::new(crate::file_edit::FileEdit))
            .unwrap();
        registry
            .register(Arc::new(AskTool::new("ask", "Ask a question")))
            .unwrap();
        assert!(!registry.may_mutate_worktree("history_search"));
        assert!(!registry.may_mutate_worktree("ask"));
        assert!(registry.may_mutate_worktree("edit_file"));
        assert!(!registry.may_mutate_worktree("unknown"));
        registry.register_interceptor(Arc::new(LegacyHook));
        assert!(registry.may_mutate_worktree("history_search"));
    }

    #[test]
    fn idle_timeout_never_claims_remote_operation_stopped() {
        let outcome = idle_timeout("plugin_tool", Duration::from_secs(1), "{}");
        assert!(outcome.result.is_error);
        assert!(outcome.result.content.contains("outcome is unknown"));
        assert!(!outcome.result.content.contains("was stopped"));
    }

    #[tokio::test]
    async fn edit_file_hook_cannot_bypass_nested_instruction_preflight() {
        let executions = Arc::new(AtomicUsize::new(0));
        let mut registry = ToolRegistry::new();
        registry
            .register(Arc::new(TestTool {
                name: "edit_file",
                result: ToolResult::ok("edited"),
                executions: executions.clone(),
            }))
            .unwrap();
        registry.register_interceptor(Arc::new(RewriteBefore(
            r#"{"path":"src/deep/file.rs","old_text":"old","new_text":"new"}"#,
        )));
        let call = test_call(
            "edit_file",
            r#"{"path":"src/other/file.rs","old_text":"old","new_text":"new"}"#,
        );
        let result = registry.execute(&call, test_context()).await;
        assert!(result.is_error);
        assert!(result.content.contains("project instruction preflight"));
        assert_eq!(executions.load(Ordering::SeqCst), 0);
    }

    struct TestTool {
        name: &'static str,
        result: ToolResult,
        executions: Arc<AtomicUsize>,
    }

    impl Tool for TestTool {
        fn name(&self) -> &str {
            self.name
        }

        fn definition(&self) -> Value {
            json!({"type":"function","function":{"name":self.name,"parameters":{"type":"object"}}})
        }

        fn execute<'a>(&'a self, _call: &'a ToolCall, _ctx: ToolContext<'a>) -> ToolFuture<'a> {
            let executions = self.executions.clone();
            let result = self.result.clone();
            Box::pin(async move {
                executions.fetch_add(1, Ordering::SeqCst);
                result
            })
        }
    }

    struct RewriteBefore(&'static str);

    impl ToolInterceptor for RewriteBefore {
        fn before<'a>(&'a self, call: &'a mut ToolCall, _workdir: &'a Path) -> HookFuture<'a> {
            let arguments = self.0.to_owned();
            Box::pin(async move {
                call.function.arguments = arguments;
                Ok(())
            })
        }
    }

    struct RejectBefore;

    impl ToolInterceptor for RejectBefore {
        fn before<'a>(&'a self, _call: &'a mut ToolCall, _workdir: &'a Path) -> HookFuture<'a> {
            Box::pin(async { Err(anyhow::anyhow!("rejected by test hook")) })
        }
    }

    struct MutatingFailAfter {
        runs: Arc<AtomicUsize>,
    }

    impl ToolInterceptor for MutatingFailAfter {
        fn after<'a>(
            &'a self,
            _call: &'a ToolCall,
            result: &'a mut ToolResult,
            _workdir: &'a Path,
        ) -> HookFuture<'a> {
            let runs = self.runs.clone();
            Box::pin(async move {
                runs.fetch_add(1, Ordering::SeqCst);
                result.content = "partial hook mutation".into();
                result.is_error = !result.is_error;
                result.executed_arguments = Some("poisoned".into());
                Err(anyhow::anyhow!("hook exploded"))
            })
        }
    }

    struct CountingPatchAfter {
        runs: Arc<AtomicUsize>,
    }

    impl ToolInterceptor for CountingPatchAfter {
        fn after<'a>(
            &'a self,
            _call: &'a ToolCall,
            result: &'a mut ToolResult,
            _workdir: &'a Path,
        ) -> HookFuture<'a> {
            let runs = self.runs.clone();
            Box::pin(async move {
                runs.fetch_add(1, Ordering::SeqCst);
                result.content = "late hook patch".into();
                Ok(())
            })
        }
    }

    struct LegacyPatchAfter;

    impl ToolInterceptor for LegacyPatchAfter {
        fn after<'a>(
            &'a self,
            _call: &'a ToolCall,
            result: &'a mut ToolResult,
            _workdir: &'a Path,
        ) -> HookFuture<'a> {
            Box::pin(async move {
                result.content.push_str("+legacy-after");
                Ok(())
            })
        }
    }

    struct EchoForHook;
    impl Tool for EchoForHook {
        fn name(&self) -> &str {
            "echo_hook"
        }
        fn definition(&self) -> Value {
            json!({"type":"function","function":{"name":"echo_hook","parameters":{"type":"object"}}})
        }
        fn execute<'a>(&'a self, call: &'a ToolCall, _ctx: ToolContext<'a>) -> ToolFuture<'a> {
            Box::pin(async move { ToolResult::ok(call.function.arguments.clone()) })
        }
    }

    struct LegacyHook;
    impl ToolInterceptor for LegacyHook {
        fn before<'a>(&'a self, call: &'a mut ToolCall, _workdir: &'a Path) -> HookFuture<'a> {
            Box::pin(async move {
                call.function.arguments = r#"{"origin":"legacy"}"#.into();
                Ok(())
            })
        }
    }

    #[tokio::test]
    async fn older_rust_hook_signature_remains_source_compatible() {
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(EchoForHook)).unwrap();
        registry.register_interceptor(Arc::new(LegacyHook));
        let call = ToolCall {
            id: "old".into(),
            kind: "function".into(),
            function: FunctionCall {
                name: "echo_hook".into(),
                arguments: "{}".into(),
            },
        };
        let result = registry
            .execute(
                &call,
                ToolContext {
                    model_connection: None,
                    user_message: None,
                    user_turn: None,
                    user_images: None,
                    previous_user_images: None,
                    workdir: Path::new("."),
                    archive: Path::new("session.json"),
                    on_output: None,
                    ask: None,
                },
            )
            .await;
        assert!(!result.is_error);
        assert_eq!(result.content, r#"{"origin":"legacy"}"#);
        assert_eq!(
            result.executed_arguments.as_deref(),
            Some(r#"{"origin":"legacy"}"#)
        );
        assert_eq!(call.function.arguments, "{}", "durable call stays intact");
    }

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

    #[tokio::test]
    async fn after_hook_failure_keeps_real_success_and_runs_side_effect_once() {
        let executions = Arc::new(AtomicUsize::new(0));
        let hook_runs = Arc::new(AtomicUsize::new(0));
        let later_runs = Arc::new(AtomicUsize::new(0));
        let mut registry = ToolRegistry::new();
        registry
            .register(Arc::new(TestTool {
                name: "counted",
                result: ToolResult::ok("real result"),
                executions: executions.clone(),
            }))
            .unwrap();
        registry.register_interceptor(Arc::new(RewriteBefore(r#"{"rewritten":true}"#)));
        registry.register_interceptor(Arc::new(MutatingFailAfter {
            runs: hook_runs.clone(),
        }));
        registry.register_interceptor(Arc::new(CountingPatchAfter {
            runs: later_runs.clone(),
        }));

        let execution = registry
            .execute_with_warnings(&test_call("counted", "{}"), test_context())
            .await;
        let result = execution.result;
        let warnings = execution.warnings;

        assert_eq!(
            executions.load(Ordering::SeqCst),
            1,
            "side effect runs once"
        );
        assert_eq!(hook_runs.load(Ordering::SeqCst), 1);
        assert_eq!(
            later_runs.load(Ordering::SeqCst),
            0,
            "later after hooks do not run"
        );
        assert!(!result.is_error, "real success survives hook diagnostics");
        assert_eq!(result.content, "real result");
        assert_eq!(
            result.executed_arguments.as_deref(),
            Some(r#"{"rewritten":true}"#)
        );
        assert_eq!(
            warnings,
            vec!["after_tool_result failed: hook exploded".to_string()]
        );
    }

    #[tokio::test]
    async fn after_hook_failure_does_not_mask_an_original_tool_failure() {
        let executions = Arc::new(AtomicUsize::new(0));
        let mut registry = ToolRegistry::new();
        registry
            .register(Arc::new(TestTool {
                name: "failing",
                result: ToolResult::error("real failure"),
                executions: executions.clone(),
            }))
            .unwrap();
        registry.register_interceptor(Arc::new(MutatingFailAfter {
            runs: Arc::new(AtomicUsize::new(0)),
        }));

        let execution = registry
            .execute_with_warnings(&test_call("failing", "{}"), test_context())
            .await;
        let result = execution.result;
        let warnings = execution.warnings;

        assert_eq!(executions.load(Ordering::SeqCst), 1);
        assert!(result.is_error);
        assert_eq!(result.content, "real failure");
        assert_eq!(warnings.len(), 1);
    }

    #[tokio::test]
    async fn successful_hook_patches_keep_legacy_and_effective_arguments() {
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(EchoForHook)).unwrap();
        registry.register_interceptor(Arc::new(LegacyHook));
        registry.register_interceptor(Arc::new(LegacyPatchAfter));

        let execution = registry
            .execute_with_warnings(&test_call("echo_hook", "{}"), test_context())
            .await;
        let result = execution.result;
        let warnings = execution.warnings;

        assert!(warnings.is_empty());
        assert!(!result.is_error);
        assert_eq!(result.content, r#"{"origin":"legacy"}+legacy-after"#);
        assert_eq!(
            result.executed_arguments.as_deref(),
            Some(r#"{"origin":"legacy"}"#)
        );
    }

    #[tokio::test]
    async fn before_hook_failure_prevents_execution_without_a_warning() {
        let executions = Arc::new(AtomicUsize::new(0));
        let mut registry = ToolRegistry::new();
        registry
            .register(Arc::new(TestTool {
                name: "guarded",
                result: ToolResult::ok("must not run"),
                executions: executions.clone(),
            }))
            .unwrap();
        registry.register_interceptor(Arc::new(RejectBefore));

        let execution = registry
            .execute_with_warnings(&test_call("guarded", "{}"), test_context())
            .await;
        let result = execution.result;
        let warnings = execution.warnings;

        assert_eq!(executions.load(Ordering::SeqCst), 0);
        assert!(result.is_error);
        assert_eq!(
            result.content,
            "before_tool_call failed: rejected by test hook"
        );
        assert!(warnings.is_empty());
    }

    #[tokio::test]
    async fn no_hook_execution_returns_no_warnings() {
        let executions = Arc::new(AtomicUsize::new(0));
        let mut registry = ToolRegistry::new();
        registry
            .register(Arc::new(TestTool {
                name: "plain",
                result: ToolResult::ok("plain result"),
                executions: executions.clone(),
            }))
            .unwrap();

        let execution = registry
            .execute_with_warnings(&test_call("plain", "{}"), test_context())
            .await;
        let result = execution.result;
        let warnings = execution.warnings;

        assert_eq!(executions.load(Ordering::SeqCst), 1);
        assert!(!result.is_error);
        assert_eq!(result.content, "plain result");
        assert!(warnings.is_empty());
    }

    #[tokio::test]
    async fn nonzero_shell_exit_is_a_failed_tool_result() {
        let outcome = execute(
            &test_call("shell", r#"{"command":"exit 7"}"#),
            &std::env::temp_dir(),
            Path::new("/tmp/kroma-exit-test.json"),
        )
        .await;

        assert!(outcome.is_error);
        let data: Value = serde_json::from_str(&outcome.content).unwrap();
        assert_eq!(data["exit_code"], 7);
        assert!(data.get("stdout_bytes").is_none());
        assert!(data.get("stderr_bytes").is_none());
    }
}

#[cfg(test)]
mod ask_tests {
    use super::*;
    use crate::protocol::FunctionCall;
    use std::sync::Mutex;

    fn call(arguments: &str) -> ToolCall {
        ToolCall {
            id: "call-ask".into(),
            kind: "function".into(),
            function: FunctionCall {
                name: "ask".into(),
                arguments: arguments.into(),
            },
        }
    }

    /// Records the request it was given and answers with a fixed result.
    struct Fixed {
        seen: Mutex<Option<AskRequest>>,
        answer: Option<AskAnswer>,
    }

    impl Asker for Fixed {
        fn ask<'a>(
            &'a self,
            request: AskRequest,
        ) -> Pin<Box<dyn Future<Output = Option<AskAnswer>> + Send + 'a>> {
            if let Ok(mut seen) = self.seen.lock() {
                *seen = Some(request);
            }
            let answer = self.answer.clone();
            Box::pin(async move { answer })
        }
    }

    fn context<'a>(ask: Option<&'a dyn Asker>) -> ToolContext<'a> {
        ToolContext {
            model_connection: None,
            user_message: None,
            user_turn: None,
            user_images: None,
            previous_user_images: None,
            workdir: Path::new("."),
            archive: Path::new("kroma-ask-test.json"),
            on_output: None,
            ask,
        }
    }

    #[tokio::test]
    async fn a_single_choice_reports_the_label_without_claiming_approval() {
        let asker = Fixed {
            seen: Mutex::new(None),
            answer: Some(AskAnswer::Answered {
                picked: vec!["Postgres 16".into()],
                typed: None,
            }),
        };
        let tool = AskTool::new("ask", "Ask the user to choose");
        let result = tool
            .execute(
                &call(r#"{"question":"Which database?","options":["Postgres 16","SQLite"]}"#),
                context(Some(&asker)),
            )
            .await;
        assert!(!result.is_error);
        assert_eq!(result.content, "User selected: Postgres 16");
        let seen = asker.seen.lock().unwrap().clone().unwrap();
        assert_eq!(seen.options.len(), 2);
        assert!(!seen.multiple);
    }

    #[tokio::test]
    async fn multiple_choices_keep_option_order_and_carry_typed_text() {
        let asker = Fixed {
            seen: Mutex::new(None),
            answer: Some(AskAnswer::Answered {
                picked: vec!["A".into(), "C".into()],
                typed: Some(" plus a read replica".into()),
            }),
        };
        let tool = AskTool::new("ask", "Ask the user to choose");
        let result = tool
            .execute(
                &call(r#"{"question":"Which ones?","options":["A","B","C"],"multiple":true}"#),
                context(Some(&asker)),
            )
            .await;
        assert_eq!(
            result.content,
            "User selected: A, C\nUser typed: plus a read replica"
        );
    }

    #[tokio::test]
    async fn a_dismissed_question_is_an_error_and_a_typed_answer_is_not() {
        let dismissed = Fixed {
            seen: Mutex::new(None),
            answer: Some(AskAnswer::Dismissed),
        };
        let typed = Fixed {
            seen: Mutex::new(None),
            answer: Some(AskAnswer::Answered {
                picked: Vec::new(),
                typed: Some("use the staging replica".into()),
            }),
        };
        let tool = AskTool::new("ask", "Ask the user to choose");
        let arguments = r#"{"question":"Which database?","options":["Postgres","SQLite"]}"#;
        let closed = tool
            .execute(&call(arguments), context(Some(&dismissed)))
            .await;
        assert!(closed.is_error);
        assert!(closed.content.contains("dismissed"));
        let answered = tool.execute(&call(arguments), context(Some(&typed))).await;
        assert!(!answered.is_error);
        assert_eq!(answered.content, "User typed: use the staging replica");
    }

    #[tokio::test]
    async fn a_host_without_an_interface_fails_at_once_instead_of_waiting() {
        let tool = AskTool::new("ask", "Ask the user to choose");
        let result = tool
            .execute(
                &call(r#"{"question":"Which database?","options":["Postgres","SQLite"]}"#),
                context(None),
            )
            .await;
        assert!(result.is_error);
        assert!(result.content.contains("cannot show a question"));
        assert!(
            tool.waits_for_user(),
            "a person's answer is not a stuck tool"
        );
    }

    #[test]
    fn arguments_must_offer_distinct_real_choices() {
        assert!(parse_ask(r#"{"question":"Pick","options":["a","b"]}"#).is_ok());
        assert!(parse_ask(r#"{"question":"  ","options":["a","b"]}"#).is_err());
        assert!(parse_ask(r#"{"question":"Pick","options":["a"]}"#).is_err());
        assert!(parse_ask(r#"{"question":"Pick","options":["a","a"]}"#).is_err());
        assert!(parse_ask(r#"{"question":"Pick","options":["a"," "]}"#).is_err());
        let parsed =
            parse_ask(r#"{"question":" Pick ","options":[" a ","b"],"multiple":true}"#).unwrap();
        assert_eq!(parsed.question, "Pick");
        assert_eq!(parsed.options, vec!["a".to_owned(), "b".to_owned()]);
        assert!(parsed.multiple);
    }
}
