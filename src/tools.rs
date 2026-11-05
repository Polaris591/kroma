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
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn builtin() -> Self {
        Self {
            tools: vec![Arc::new(BuiltinShell)],
            interceptors: Vec::new(),
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

    pub fn register_interceptor(&mut self, interceptor: Arc<dyn ToolInterceptor>) {
        self.interceptors.push(interceptor);
    }

    pub fn definitions(&self) -> Vec<Value> {
        self.tools.iter().map(|tool| tool.definition()).collect()
    }

    pub fn is_builtin_only(&self) -> bool {
        self.tools.len() == 1
            && self.tools[0].name() == "shell"
            && self.tools[0].definition() == definitions()[0]
            && self.interceptors.is_empty()
    }

    pub async fn execute(&self, call: &ToolCall, ctx: ToolContext<'_>) -> ToolResult {
        let (result, warnings) = self.execute_with_warnings(call, ctx).await;
        for warning in warnings {
            crate::runtime_log!("[tool] {warning}");
        }
        result
    }

    /// Execute a tool while keeping interceptor diagnostics separate from the
    /// real outcome. A failing after hook must not turn a completed side effect
    /// into an error result, because that invites the model to repeat it.
    pub(crate) async fn execute_with_warnings(
        &self,
        call: &ToolCall,
        ctx: ToolContext<'_>,
    ) -> (ToolResult, Vec<String>) {
        if call.kind != "function" {
            return (
                ToolResult::error(
                    json!({"error": format!("unsupported tool: {}", call.function.name)})
                        .to_string(),
                ),
                Vec::new(),
            );
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
            return (result, Vec::new());
        }
        let mut effective = call.clone();
        for hook in &self.interceptors {
            if let Err(error) = hook.before_with_context(&mut effective, ctx).await {
                return (
                    ToolResult::error(format!("before_tool_call failed: {error:#}")),
                    Vec::new(),
                );
            }
            if effective.id != call.id
                || effective.kind != call.kind
                || effective.function.name != call.function.name
            {
                return (
                    ToolResult::error("tool interceptor cannot change call id, kind or name"),
                    Vec::new(),
                );
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
        (result, warnings)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::FunctionCall;

    use std::sync::atomic::{AtomicUsize, Ordering};

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
            workdir: &workdir,
            archive: &archive,
            on_output: None,
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
            workdir: Path::new("."),
            archive: Path::new("session.json"),
            on_output: None,
        }
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
                    workdir: Path::new("."),
                    archive: Path::new("session.json"),
                    on_output: None,
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

        let (result, warnings) = registry
            .execute_with_warnings(&test_call("counted", "{}"), test_context())
            .await;

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

        let (result, warnings) = registry
            .execute_with_warnings(&test_call("failing", "{}"), test_context())
            .await;

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

        let (result, warnings) = registry
            .execute_with_warnings(&test_call("echo_hook", "{}"), test_context())
            .await;

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

        let (result, warnings) = registry
            .execute_with_warnings(&test_call("guarded", "{}"), test_context())
            .await;

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

        let (result, warnings) = registry
            .execute_with_warnings(&test_call("plain", "{}"), test_context())
            .await;

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
