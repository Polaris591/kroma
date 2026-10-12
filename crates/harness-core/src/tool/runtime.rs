use crate::budget::BudgetConfig;
use crate::env::SharedExecutionEnv;
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
use crate::workspace::WorkspaceCap;
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
        }
    }

    pub fn with_bash_grace_period(mut self, grace: Duration) -> Self {
        self.bash_grace_period = grace;
        self
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
        true
    }

    /// Execute a single tool call through the full security, policy, approval, TOCTOU, and ACI pipeline.
    pub async fn execute_call(
        &self,
        call: &ToolCall,
        cancel_rx: watch::Receiver<bool>,
    ) -> (ToolResultMessage, String) {
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
                );
            }
            Err(ToolLookupError::UnknownTool(_)) => {
                let out = ToolExecutionOutput::error(
                    ErrorCode::UnknownTool,
                    format!("unknown tool '{}'", call.name),
                    ToolResultMeta::default(),
                );
                return (Self::into_message(call, out), "deny_unknown".to_string());
            }
        };

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
                        return (Self::into_message(call, out), "deny_path_escape".to_string());
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
                return (Self::into_message(call, out), format!("deny:{}", reason));
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
                    return (Self::into_message(call, out), "ask_cancelled".to_string());
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
                        let out = ToolExecutionOutput::error(
                            ErrorCode::NotExecutedCancelled,
                            "NOT_EXECUTED_CANCELLED: cancelled during approval prompt".to_string(),
                            ToolResultMeta {
                                relative_path: rel_path_opt,
                                ..Default::default()
                            },
                        );
                        return (Self::into_message(call, out), "ask_cancelled".to_string());
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
                        return (Self::into_message(call, out), "ask_denied".to_string());
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
                        "ask_approved".to_string()
                    }
                }
            }
        };

        // Dispatch to concrete tool implementation
        let exec_out = match call.name.as_str() {
            "read" => execute_read(self.env.workspace(), &effective_raw_args),
            "edit" => {
                execute_edit(
                    self.env.workspace(),
                    self.env.inode_locks(),
                    &effective_raw_args,
                    self.budget.max_edit_file_bytes,
                    self.budget.max_edit_input_bytes,
                )
                .await
            }
            "write" => {
                execute_write(
                    self.env.workspace(),
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
            other => ToolExecutionOutput::error(
                ErrorCode::UnknownTool,
                format!("unknown tool '{}'", other),
                ToolResultMeta::default(),
            ),
        };

        (Self::into_message(call, exec_out), decision_label)
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
        ToolResultMessage {
            call_id: call.call_id.clone(),
            tool_name: call.name.clone(),
            outcome: out.outcome,
            content: Arc::from(out.content),
            meta: out.meta,
        }
    }
}
