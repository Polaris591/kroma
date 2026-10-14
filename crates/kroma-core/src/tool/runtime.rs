use crate::attribution::{
    added_symbols_for_call, language_name, mutation_summary, removed_symbols_for_call,
    MutationTelemetry, MutatedFileSpan, PredictiveReport, PredictiveValidator,
};
use crate::budget::BudgetConfig;
use crate::env::SharedExecutionEnv;
use crate::extensions::{
    execute_activate_skill, CustomToolContext, ExtensionDispatch, HookContext, HookEvaluation,
};
use crate::loop_guard::MAX_EDIT_FAILURE_STREAK;
use crate::messages::{ToolCall, ToolResultMessage, ToolResultMeta};
use crate::prompt::sha256_hex16;
use crate::sensitive_paths::SensitivePathGuard;
use crate::tool::policy::{
    ApprovalDecision, ApprovalRequest, PolicyCheckResult, PolicyEngine,
};
use crate::tool::registry::{ToolLookupError, ToolRegistry};
use crate::tools::{
    bash::execute_bash, edit::execute_edit, find::execute_find, grep::execute_grep, ls::execute_ls,
    read::execute_read, write::execute_write, ToolExecutionOutput,
};
use crate::types::{EffectClass, ErrorCode};
use crate::workspace::{SensitivePathApproval, WorkspaceCap};
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;

#[derive(Clone)]
pub struct ToolRuntime {
    registry: ToolRegistry,
    policy_engine: PolicyEngine,
    env: SharedExecutionEnv,
    budget: BudgetConfig,
    bash_grace_period: Duration,
    /// Gate 4: run the post-mutation structural check on `edit`/`write` results.
    predictive_check: bool,
}

impl ToolRuntime {
    pub fn new(
        registry: ToolRegistry,
        policy_engine: PolicyEngine,
        env: SharedExecutionEnv,
        budget: BudgetConfig,
    ) -> Self {
        Self {
            registry,
            policy_engine,
            env,
            budget,
            bash_grace_period: Duration::from_secs(2),
            predictive_check: true,
        }
    }

    pub fn with_bash_grace_period(mut self, grace: Duration) -> Self {
        self.bash_grace_period = grace;
        self
    }

    /// Enable/disable the Gate 4 post-mutation structural check (`--no-predictive-check`).
    pub fn with_predictive_check(mut self, enabled: bool) -> Self {
        self.predictive_check = enabled;
        self
    }

    pub fn predictive_check_enabled(&self) -> bool {
        self.predictive_check
    }

    pub fn registry(&self) -> &ToolRegistry {
        &self.registry
    }

    pub fn policy_engine(&self) -> &PolicyEngine {
        &self.policy_engine
    }

    pub fn env(&self) -> &SharedExecutionEnv {
        &self.env
    }

    /// Extract the target relative path from tool arguments if present.
    pub fn extract_relative_path(raw_arguments: &str) -> Option<String> {
        let val: serde_json::Value = serde_json::from_str(raw_arguments).ok()?;
        val.get("path")?.as_str().map(|s| s.to_string())
    }

    /// Determine whether a tool call is a pure read-only operation that can be safely executed
    /// in a concurrent read-only sub-batch (no interactive approval needed).
    pub fn is_parallelizable_read_only(&self, call: &ToolCall) -> bool {
        let Ok(reg) = self
            .registry
            .lookup_for_dispatch(&call.name, self.policy_engine.mode())
        else {
            return false;
        };
        if reg.metadata.effect_class != EffectClass::ReadOnly {
            return false;
        }
        // If sensitive path is targeted directly, do not run in parallel sub-batch
        if let Some(rel) = Self::extract_relative_path(&call.raw_arguments) {
            if let Ok(norm) = WorkspaceCap::validate_relative_components(&rel) {
                if SensitivePathGuard::is_sensitive(&norm) {
                    return false;
                }
            }
        }
        // `recall_evidence` and `spawn_subagent` resolve against the live session / subagent
        // coordinator owned by the agent loop, so they must run on the serial barrier.
        if call.name == "recall_evidence" || call.name == "spawn_subagent" {
            return false;
        }
        true
    }

    /// The currently mounted hook engine (Gate 3). Rebuilt from the registry on every call so a
    /// live `unregister_extension` is reflected immediately.
    pub fn hook_engine(&self) -> crate::extensions::HookEngine {
        self.registry.hook_engine()
    }

    /// Execute a single tool call through the full security, policy, approval, TOCTOU, and ACI pipeline.
    ///
    /// Returns the tool result, the policy decision label, and every Gate 3 hook evaluation that
    /// contributed to the outcome (for `HookEvaluated` telemetry). `PostToolUse` observers run for
    /// *every* outcome, including early kernel-policy, path-escape, approval and TOCTOU denials, so a
    /// project audit hook never misses a call.
    pub async fn execute_call(
        &self,
        call: &ToolCall,
        turn: usize,
        cancel_rx: watch::Receiver<bool>,
    ) -> (ToolResultMessage, String, Vec<HookEvaluation>) {
        let (message, decision, mut evaluations) =
            self.execute_call_inner(call, turn, cancel_rx).await;
        evaluations.extend(self.post_tool_hooks(call, turn, &message).await);
        (message, decision, evaluations)
    }

    /// Fail-closed `PreToolUse` policy-hook gate.
    ///
    /// Exposed so a call dispatched outside [`ToolRuntime::execute_call`] (currently
    /// `recall_evidence`, which needs the live transcript) still runs through the exact same
    /// `PreToolUse` `PolicyHook`s as every other tool.
    pub async fn pre_tool_hook_gate(
        &self,
        call: &ToolCall,
        turn: usize,
    ) -> Result<Vec<HookEvaluation>, (ToolResultMessage, Vec<HookEvaluation>)> {
        let hook_engine = self.hook_engine();
        if hook_engine.is_empty() {
            return Ok(Vec::new());
        }
        let hook_ctx = HookContext::for_tool(
            turn,
            self.env.workspace().root().to_path_buf(),
            &call.name,
            call.call_id.as_str(),
            &call.raw_arguments,
        );
        let pre = hook_engine
            .evaluate_policy(crate::extensions::HookEvent::PreToolUse, &hook_ctx)
            .await;
        let evaluations = pre.evaluations().to_vec();
        match pre.reason() {
            Some(reason) => {
                let out = ToolExecutionOutput::error(
                    ErrorCode::PolicyDenied,
                    format!("policy hook denied '{}': {}", call.name, reason),
                    ToolResultMeta {
                        relative_path: Self::extract_relative_path(&call.raw_arguments),
                        ..Default::default()
                    },
                );
                Err((Self::into_message(call, out), evaluations))
            }
            None => Ok(evaluations),
        }
    }

    /// Fail-open `PostToolUse` observer pipeline for one tool result.
    pub async fn post_tool_hooks(
        &self,
        call: &ToolCall,
        turn: usize,
        message: &ToolResultMessage,
    ) -> Vec<HookEvaluation> {
        let hook_engine = self.hook_engine();
        if hook_engine.is_empty() {
            return Vec::new();
        }
        let mut ctx = HookContext::for_tool(
            turn,
            self.env.workspace().root().to_path_buf(),
            &call.name,
            call.call_id.as_str(),
            &call.raw_arguments,
        );
        ctx.relative_path = message.meta.relative_path.clone();
        ctx.outcome = Some(if message.outcome.is_success() {
            "success".to_string()
        } else {
            "error".to_string()
        });
        hook_engine
            .run_observers(crate::extensions::HookEvent::PostToolUse, &ctx)
            .await
    }

    /// Run one extension tool call, gated against a concurrent `unregister_extension`.
    fn run_extension_tool(
        &self,
        dispatch: &ExtensionDispatch,
        raw_arguments: &str,
    ) -> ToolExecutionOutput {
        let Some(_guard) = dispatch.in_flight.begin() else {
            return ToolExecutionOutput::error(
                ErrorCode::NotExecutedCancelled,
                format!(
                    "NOT_EXECUTED_CANCELLED: extension '{}' was unmounted before '{}' could start",
                    dispatch.extension_id, dispatch.tool_name
                ),
                ToolResultMeta::default(),
            );
        };
        if dispatch.in_flight.is_cancelled() {
            return ToolExecutionOutput::error(
                ErrorCode::NotExecutedCancelled,
                format!(
                    "NOT_EXECUTED_CANCELLED: extension '{}' was unmounted; '{}' was cancelled",
                    dispatch.extension_id, dispatch.tool_name
                ),
                ToolResultMeta::default(),
            );
        }
        let out = dispatch.handler.execute(
            raw_arguments,
            &CustomToolContext {
                workspace: self.env.workspace(),
                in_flight: Some(Arc::clone(&dispatch.in_flight)),
            },
        );
        if out.outcome.is_success()
            && matches!(
                dispatch.effect_class,
                EffectClass::Mutating | EffectClass::SideEffect
            )
        {
            dispatch.in_flight.record_side_effect(&dispatch.tool_name);
        }
        out
    }

    async fn execute_call_inner(
        &self,
        call: &ToolCall,
        turn: usize,
        cancel_rx: watch::Receiver<bool>,
    ) -> (ToolResultMessage, String, Vec<HookEvaluation>) {
        let policy_mode = self.policy_engine.mode();
        let registered = match self.registry.lookup_for_dispatch(&call.name, policy_mode) {
            Ok(r) => r,
            Err(ToolLookupError::PolicyHidden(_)) => {
                let out = ToolExecutionOutput::error(
                    ErrorCode::PolicyDenied,
                    format!(
                        "tool '{}' is disabled and hidden under --policy {}",
                        call.name,
                        policy_mode.as_str()
                    ),
                    ToolResultMeta::default(),
                );
                return (
                    Self::into_message(call, out),
                    "deny_policy_hidden".to_string(),
                    Vec::new(),
                );
            }
            Err(ToolLookupError::FilteredOut(_)) => {
                let out = ToolExecutionOutput::error(
                    ErrorCode::PolicyDenied,
                    format!("tool '{}' is disabled by --tool-filter", call.name),
                    ToolResultMeta::default(),
                );
                return (
                    Self::into_message(call, out),
                    "deny_tool_filter".to_string(),
                    Vec::new(),
                );
            }
            Err(ToolLookupError::UnknownTool(_)) => {
                let out = ToolExecutionOutput::error(
                    ErrorCode::UnknownTool,
                    format!("unknown tool '{}'", call.name),
                    ToolResultMeta::default(),
                );
                return (Self::into_message(call, out), "deny_unknown".to_string(), Vec::new());
            }
        };

        // Gate 3: PreToolUse policy hooks run BEFORE the kernel policy engine and are fail-closed.
        // A hook may only ever restrict; the kernel `PolicyEngine::evaluate` below is evaluated
        // independently, so a hook can never turn a kernel denial into an allow.
        let mut hook_evaluations: Vec<HookEvaluation> = Vec::new();
        match self.pre_tool_hook_gate(call, turn).await {
            Ok(evaluations) => hook_evaluations.extend(evaluations),
            Err((denied, evaluations)) => {
                return (denied, "deny_policy_hook".to_string(), evaluations);
            }
        }

        let rel_path_opt = Self::extract_relative_path(&call.raw_arguments);
        let mut is_sensitive = false;

        if matches!(call.name.as_str(), "read" | "edit" | "write") {
            if let Some(ref rel) = rel_path_opt {
                match WorkspaceCap::validate_relative_components(rel) {
                    Ok(norm) => {
                        if SensitivePathGuard::is_sensitive(&norm) {
                            is_sensitive = true;
                        } else if let Ok((_, abs)) =
                            self.env.workspace().resolve_existing_path(rel, false)
                        {
                            if let Ok(canon) = fs::canonicalize(&abs) {
                                let rel_canon = canon
                                    .strip_prefix(self.env.workspace().root())
                                    .unwrap_or(&canon);
                                if SensitivePathGuard::is_sensitive(rel_canon) {
                                    is_sensitive = true;
                                }
                            }
                        } else if matches!(
                            self.env.workspace().resolve_existing_path(rel, false),
                            Err(crate::workspace::WorkspaceError::SensitivePath { .. })
                        ) {
                            is_sensitive = true;
                        }
                    }
                    Err(e) => {
                        let out = ToolExecutionOutput::error(
                            e.error_code(),
                            e.to_string(),
                            ToolResultMeta {
                                relative_path: Some(rel.clone()),
                                ..Default::default()
                            },
                        );
                        return (Self::into_message(call, out), "deny_path_escape".to_string(), hook_evaluations);
                    }
                }
            }
        }

        // Evaluate Policy & SensitivePathGuard
        let check = self.policy_engine.evaluate(
            &call.name,
            registered.metadata.effect_class,
            is_sensitive,
        );

        let mut effective_raw_args = call.raw_arguments.clone();
        // BUG-08: a user approval for a sensitive path is scoped to exactly this call. The token
        // is minted only in the `ask_approved` arm below and consumed by a short-lived workspace
        // clone at dispatch; nothing outside this call can reuse it.
        let mut sensitive_approval: Option<SensitivePathApproval> = None;

        let decision_label = match check {
            PolicyCheckResult::Allow => "allow".to_string(),
            PolicyCheckResult::Deny(reason) => {
                let code = if is_sensitive {
                    ErrorCode::SensitivePathBlocked
                } else {
                    ErrorCode::PolicyDenied
                };
                let out = ToolExecutionOutput::error(
                    code,
                    format!("{} ({})", reason, call.name),
                    ToolResultMeta {
                        relative_path: rel_path_opt,
                        ..Default::default()
                    },
                );
                return (Self::into_message(call, out), format!("deny:{}", reason), hook_evaluations);
            }
            PolicyCheckResult::Ask => {
                let compute_target_sha = |rel: &str| -> Option<String> {
                    let (_, abs) = self
                        .env
                        .workspace()
                        .resolve_existing_path(rel, false)
                        .ok()?;
                    let meta = fs::symlink_metadata(&abs).ok()?;
                    if !meta.file_type().is_file() || meta.len() > 16 * 1024 * 1024 {
                        return None;
                    }
                    let bytes = fs::read(&abs).ok()?;
                    Some(sha256_hex16(&bytes))
                };

                // Capture pre-approval SHA256 for TOCTOU protection on edit/write (§2.1, §5.2)
                let pre_approval_sha = rel_path_opt
                    .as_deref()
                    .and_then(&compute_target_sha);

                let diff_preview = if matches!(call.name.as_str(), "edit" | "write") {
                    Some(Self::build_approval_diff_preview(
                        &call.name,
                        rel_path_opt.as_deref().unwrap_or("<target>"),
                        &call.raw_arguments,
                    ))
                } else {
                    None
                };

                let command_preview = if call.name == "bash" {
                    Some(Self::build_approval_command_preview(&call.raw_arguments))
                } else {
                    None
                };

                let req = ApprovalRequest {
                    tool_name: call.name.clone(),
                    effect_class: registered.metadata.effect_class,
                    is_sensitive_path: is_sensitive,
                    target_path: rel_path_opt.as_ref().map(PathBuf::from),
                    diff_preview,
                    command_preview,
                    workspace_root: self.env.workspace().root().to_path_buf(),
                };

                let mut approval_cancel_rx = cancel_rx.clone();
                if *approval_cancel_rx.borrow() {
                    let out = ToolExecutionOutput::error(
                        ErrorCode::NotExecutedCancelled,
                        "NOT_EXECUTED_CANCELLED: cancelled before approval prompt".to_string(),
                        ToolResultMeta {
                            relative_path: rel_path_opt,
                            ..Default::default()
                        },
                    );
                    return (Self::into_message(call, out), "ask_cancelled".to_string(), hook_evaluations);
                }

                let pe = self.policy_engine.clone();
                let mut prompt_task =
                    tokio::task::spawn_blocking(move || pe.prompt_approval(&req));

                let decision = tokio::select! {
                    res = &mut prompt_task => res.unwrap_or(ApprovalDecision::Deny),
                    _ = async {
                        loop {
                            if approval_cancel_rx.changed().await.is_err() {
                                std::future::pending::<()>().await;
                            }
                            if *approval_cancel_rx.borrow() {
                                break;
                            }
                        }
                    } => {
                        let _ = tokio::time::timeout(
                            std::time::Duration::from_millis(200),
                            &mut prompt_task,
                        )
                        .await;
                        let out = ToolExecutionOutput::error(
                            ErrorCode::NotExecutedCancelled,
                            "NOT_EXECUTED_CANCELLED: cancelled during approval prompt".to_string(),
                            ToolResultMeta {
                                relative_path: rel_path_opt,
                                ..Default::default()
                            },
                        );
                        return (Self::into_message(call, out), "ask_cancelled".to_string(), hook_evaluations);
                    }
                };

                match decision {
                    ApprovalDecision::Deny => {
                        let code = if is_sensitive {
                            ErrorCode::SensitivePathBlocked
                        } else {
                            ErrorCode::PolicyDenied
                        };
                        let out = ToolExecutionOutput::error(
                            code,
                            format!(
                                "tool '{}' execution denied by approval policy",
                                call.name
                            ),
                            ToolResultMeta {
                                relative_path: rel_path_opt,
                                ..Default::default()
                            },
                        );
                        return (Self::into_message(call, out), "ask_denied".to_string(), hook_evaluations);
                    }
                    ApprovalDecision::AllowOnce | ApprovalDecision::AllowCategoryForRun => {
                        // Secondary SHA-256 TOCTOU check after user approval wait
                        if matches!(call.name.as_str(), "edit" | "write") {
                            if let Some(ref rel) = rel_path_opt {
                                let now_sha = compute_target_sha(rel);
                                if now_sha != pre_approval_sha {
                                    let out = ToolExecutionOutput::error(
                                        ErrorCode::StaleFile,
                                        format!(
                                            "TOCTOU check failed: file '{}' changed during approval wait (was {:?}, now {:?})",
                                            rel, pre_approval_sha, now_sha
                                        ),
                                        ToolResultMeta {
                                            relative_path: Some(rel.clone()),
                                            content_sha256: now_sha,
                                            ..Default::default()
                                        },
                                    );
                                    return (
                                        Self::into_message(call, out),
                                        "toctou_stale".to_string(),
                                        hook_evaluations,
                                    );
                                }
                                // Inject verified SHA-256 / if_not_exists so execute_edit/execute_write re-verifies under InodeMutationLock
                                if let Ok(mut arg_obj) =
                                    serde_json::from_str::<serde_json::Value>(&effective_raw_args)
                                {
                                    if let Some(map) = arg_obj.as_object_mut() {
                                        if let Some(ref sha) = now_sha {
                                            map.entry("expected_sha256".to_string()).or_insert_with(
                                                || serde_json::Value::String(sha.clone()),
                                            );
                                        } else if call.name == "write" {
                                            map.entry("if_not_exists".to_string()).or_insert_with(
                                                || serde_json::Value::Bool(true),
                                            );
                                        }
                                        if let Ok(re_ser) = serde_json::to_string(&arg_obj) {
                                            effective_raw_args = re_ser;
                                        }
                                    }
                                }
                            }
                        }
                        // BUG-08: record the approved sensitive path so the dispatched tool
                        // execution may reach exactly that file. `AllowCategoryForRun` is treated
                        // identically here because the policy engine never records a sensitive path
                        // as an allowed *category* (`PolicyEngine::prompt_approval` skips
                        // `is_sensitive_path`), so a sensitive grant is always per-call.
                        if is_sensitive {
                            if let Some(rel) = rel_path_opt.as_deref() {
                                if let Ok(norm) =
                                    WorkspaceCap::validate_relative_components(rel)
                                {
                                    sensitive_approval = Some(SensitivePathApproval::new(norm));
                                }
                            }
                        }
                        "ask_approved".to_string()
                    }
                }
            }
        };

        // Dispatch to concrete tool implementation
        // BUG-08: hand the approved clone only to the tools whose sensitive-path detection set
        // `is_sensitive`; every other tool (and every unapproved call) keeps the plain workspace.
        let approved_workspace = sensitive_approval
            .map(|approval| self.env.workspace().with_approval(approval));
        let tool_workspace: &WorkspaceCap = approved_workspace
            .as_ref()
            .unwrap_or_else(|| self.env.workspace());
        let exec_out = match call.name.as_str() {
            "read" => execute_read(tool_workspace, &effective_raw_args),
            "edit" => {
                execute_edit(
                    tool_workspace,
                    self.env.inode_locks(),
                    &effective_raw_args,
                    self.budget.max_edit_file_bytes,
                    self.budget.max_edit_input_bytes,
                )
                .await
            }
            "write" => {
                execute_write(
                    tool_workspace,
                    self.env.inode_locks(),
                    &effective_raw_args,
                    self.budget.max_write_content_bytes,
                )
                .await
            }
            "bash" => {
                execute_bash(
                    self.env.workspace().root(),
                    self.env.state_dir(),
                    &call.call_id,
                    &effective_raw_args,
                    &self.budget,
                    cancel_rx,
                    self.bash_grace_period,
                )
                .await
            }
            "ls" => execute_ls(self.env.workspace(), &effective_raw_args),
            "find" => execute_find(self.env.workspace(), &effective_raw_args),
            "grep" => execute_grep(self.env.workspace(), &effective_raw_args),
            // Gate 7 (v1.2.0): symbol dependency graph + Personalized PageRank navigation.
            "code_graph" => {
                crate::repo_graph::execute_code_graph(self.env.workspace(), &effective_raw_args)
            }
            // Gate 3: on-demand skill activation, read-only and visible under every policy mode.
            "activate_skill" => {
                execute_activate_skill(&self.registry.skills(), &effective_raw_args)
            }
            // `recall_evidence` is dispatched by the agent loop, which owns the live transcript and
            // the evidence store. Reaching the runtime directly means no transcript was available.
            "recall_evidence" => ToolExecutionOutput::error(
                ErrorCode::InvalidArguments,
                "recall_evidence must be dispatched by the agent loop against the live session transcript".to_string(),
                ToolResultMeta::default(),
            ),
            // Gate 5: `spawn_subagent` is dispatched by the agent loop, which owns the subagent
            // backend, spawn budget, snapshot store, and completion gate.
            "spawn_subagent" => ToolExecutionOutput::error(
                ErrorCode::InvalidArguments,
                "spawn_subagent must be dispatched by the agent loop against the subagent coordinator".to_string(),
                ToolResultMeta::default(),
            ),
            // Gate 3: an extension-provided tool. It already passed the same registry lookup,
            // `--tool-filter`, kernel `PolicyEngine` and approval pipeline as every kernel tool.
            other => match self.registry.extension_dispatch(other) {
                Some(dispatch) => self.run_extension_tool(&dispatch, &effective_raw_args),
                None => ToolExecutionOutput::error(
                    ErrorCode::UnknownTool,
                    format!("unknown tool '{}'", other),
                    ToolResultMeta::default(),
                ),
            },
        };

        let exec_out = self.attach_gate4_telemetry(call, exec_out, &effective_raw_args);

        (Self::into_message(call, exec_out), decision_label, hook_evaluations)
    }

    /// Gate 4: run the post-mutation structural check on a successful `edit`/`write`, attach the
    /// advisory warning block to the tool output, and record the mutation spans + check summary in
    /// the result metadata so the agent loop can build a `MutationRecord`.
    ///
    /// The change is never rolled back here: the block is advisory evidence only.
    fn attach_gate4_telemetry(
        &self,
        call: &ToolCall,
        mut out: ToolExecutionOutput,
        raw_args: &str,
    ) -> ToolExecutionOutput {
        if !out.outcome.is_success() || !matches!(call.name.as_str(), "edit" | "write") {
            return out;
        }
        let Some(rel) = out.meta.relative_path.clone() else {
            return out;
        };
        let content = crate::attribution::read_workspace_text(self.env.workspace(), &rel).ok();
        let report = match &content {
            Some(content) => PredictiveValidator::new(self.predictive_check)
                .validate_content(&rel, content),
            None => PredictiveReport::skipped(
                &rel,
                language_name(&rel),
                "file could not be re-read after the mutation",
                0,
            ),
        };
        if let Some(block) = report.render_block() {
            let mut appended = String::from(out.content.as_str());
            appended.push('\n');
            appended.push_str(&block);
            out.content = appended;
        }
        let (start_line, end_line) = out.meta.lines_range.unwrap_or((0, 0));
        let span = MutatedFileSpan {
            relative_path: rel.clone(),
            start_line,
            end_line,
            added_symbols: added_symbols_for_call(&call.name, raw_args, content.as_deref()),
            removed_symbols: removed_symbols_for_call(&call.name, raw_args),
            summary: mutation_summary(&call.name, &rel, start_line, end_line),
        };
        out.meta.mutation = Some(MutationTelemetry {
            spans: vec![span],
            // A `skipped` report (check disabled via `--no-predictive-check`, unsupported file type,
            // unreadable or oversized file) produces no `PredictiveCheck` telemetry: an event means
            // a structural check actually ran.
            predictive: if report.status == "skipped" {
                None
            } else {
                Some(report.summary())
            },
        });
        out
    }

    fn build_approval_diff_preview(tool_name: &str, rel_path: &str, raw_args: &str) -> String {
        let Ok(val) = serde_json::from_str::<serde_json::Value>(raw_args) else {
            return raw_args.to_string();
        };
        let mut diff = format!("--- a/{}\n+++ b/{}\n", rel_path, rel_path);
        if tool_name == "write" {
            if let Some(content) = val.get("content").and_then(|v| v.as_str()) {
                for line in content.lines().take(50) {
                    diff.push('+');
                    diff.push_str(line);
                    diff.push('\n');
                }
                return diff;
            }
        } else if tool_name == "edit" {
            if let Some(edits) = val.get("edits").and_then(|v| v.as_array()) {
                for (i, item) in edits.iter().enumerate() {
                    diff.push_str(&format!("@@ edit #{} @@\n", i + 1));
                    if let Some(old_t) = item.get("old_text").and_then(|v| v.as_str()) {
                        for l in old_t.lines().take(25) {
                            diff.push('-');
                            diff.push_str(l);
                            diff.push('\n');
                        }
                    }
                    if let Some(new_t) = item.get("new_text").and_then(|v| v.as_str()) {
                        for l in new_t.lines().take(25) {
                            diff.push('+');
                            diff.push_str(l);
                            diff.push('\n');
                        }
                    }
                }
                return diff;
            }
        }
        raw_args.to_string()
    }

    fn build_approval_command_preview(raw_args: &str) -> String {
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(raw_args) {
            if let Some(cmd) = val.get("command").and_then(|v| v.as_str()) {
                return cmd.to_string();
            }
        }
        raw_args.to_string()
    }

    pub fn augment_edit_streak_warning(msg: &mut ToolResultMessage, streak: usize) {
        if streak >= MAX_EDIT_FAILURE_STREAK {
            let path = msg
                .meta
                .relative_path
                .as_deref()
                .unwrap_or("<target_file>");
            let appended = format!(
                "{}\n[CIRCUIT BREAKER: {} consecutive failed `edit` attempts on '{}'. \
                You MUST call `read` on '{}' around the target lines before attempting another `edit`.]",
                msg.content, streak, path, path
            );
            msg.content = Arc::from(appended);
        }
    }

    fn into_message(call: &ToolCall, out: ToolExecutionOutput) -> ToolResultMessage {
        let mut meta = out.meta;
        // Gate 7: multimodal attachments travel with the result message so they are persisted in the
        // session WAL and can be projected into the next provider request.
        if !out.image_attachments.is_empty() {
            meta.image_attachments = out.image_attachments;
        }
        ToolResultMessage {
            call_id: call.call_id.clone(),
            tool_name: call.name.clone(),
            outcome: out.outcome,
            content: Arc::from(out.content),
            meta,
        }
    }
}
