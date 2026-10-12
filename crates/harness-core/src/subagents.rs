//! Gate 5 — Controlled Subagents (`Scout` & `Worker`) and Isolated Worktrees.
//!
//! Four invariants govern this module:
//!
//! 1. **Role separation & read-only enforcement.** A `Scout` subagent runs with
//!    `PolicyMode::ReadOnly`, sees only the 6 read-only tools (`activate_skill`, `find`, `grep`,
//!    `ls`, `read`, `recall_evidence`), and returns a bounded `<subagent_report>` without polluting
//!    the parent's transcript with intermediate tool outputs.
//! 2. **Physical worktree isolation.** A `Worker` subagent runs inside an [`IsolatedWorktree`]
//!    materialized from the parent's [`SnapshotStore`] baseline revision, with its own
//!    [`WorkspaceCap`] rooted at the isolated worktree directory. It cannot touch or corrupt the
//!    parent workspace while running, and does not require `git` or `.git` to exist.
//! 3. **Anti-recursion & bounded budgets.** Child subagents never receive the `spawn_subagent`
//!    tool (`max_depth == 1`), and each child runs under an independent [`SubagentBudget`]
//!    (`max_turns`, `max_tool_calls`, `max_tokens`, `timeout_ms`) wired to the parent cancellation
//!    channel. Timeout or parent cancellation immediately aborts the child and cleans up its
//!    isolated worktree.
//! 4. **Three-way baseline-aware merge & post-merge verification.** Before applying a worker's
//!    delta (`written_files ∪ removed_files`) to the parent workspace, every touched path is
//!    checked against `allowed_write_globs`, sensitive-path rules, and the parent workspace's live
//!    state relative to `base_revision`. Any concurrent parent edit or scope violation returns
//!    [`WorktreeMergeResult::Conflict`] leaving the parent workspace 100% intact. When a
//!    post-merge [`CompletionGate`] fails and `revert_on_verify_failure` is `true`, the parent
//!    workspace is automatically restored to `revision_before`.

use crate::agent_loop::{build_runtime_with_registry, AgentRunConfig, AgentRunner};
use crate::attribution::{read_workspace_text, top_level_symbol_names, MutatedFileSpan};
use crate::budget::BudgetConfig;
use crate::env::{
    CompletionGateContext, ExecutionEnv, GateOutcome, LocalHostEnv, SharedCompletionGate,
};
use crate::events::EventLogger;
use crate::extensions::{
    ExtensionManifest, HookContext, HookError, HookEvent, PolicyHook, PolicyHookDecision,
    ScopedExtension, SkillEntry, SubtreeContextTracker,
};
use crate::messages::{TranscriptItem, UserMessage};
use crate::prompt::{escape_fence_attribute, fence_untrusted, PromptBuilder};
use crate::provider::ModelProvider;
use crate::revision::{
    ensure_private_dir, is_ancestor_dir_of, live_entry_state, remove_tracked_file,
    stat_live_entry, write_tracked_file, LiveEntryStat, LiveEntryState, RevisionError, RevisionId,
    SnapshotStore, TrackedFile, WorkspaceRevision, SNAPSHOT_EXCLUDED_DIRS,
};
use crate::sensitive_paths::SensitivePathGuard;
use crate::tool::definition::{RegisteredTool, ToolDefinition, ToolMetadata};
use crate::tool::policy::{NonInteractiveApprovalHandler, PolicyEngine, PolicyMode};
use crate::tool::registry::ToolRegistry;
use crate::tools::find::compile_glob_to_regex;
use crate::types::{EffectClass, RunId, RunStatus, TerminalReason};
use crate::workspace::WorkspaceCap;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::future::Future;
use std::path::{Component, Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::watch;

/// Default maximum number of subagents a parent session may spawn (`--max-subagents`).
pub const DEFAULT_MAX_SUBAGENT_SPAWNS: usize = 8;
/// Default turn budget for a single subagent.
pub const DEFAULT_SUBAGENT_MAX_TURNS: usize = 8;
/// Hard ceiling on `max_turns` for any subagent.
pub const MAX_SUBAGENT_TURNS_CEILING: usize = 16;
/// Default tool-call budget for a single subagent.
pub const DEFAULT_SUBAGENT_MAX_TOOL_CALLS: usize = 16;
/// Hard ceiling on `max_tool_calls` for any subagent.
pub const MAX_SUBAGENT_TOOL_CALLS_CEILING: usize = 32;
/// Default token budget (input + output) for a single subagent.
pub const DEFAULT_SUBAGENT_MAX_TOKENS: u64 = 64_000;
/// Hard ceiling on `max_tokens` for any subagent.
pub const MAX_SUBAGENT_TOKENS_CEILING: u64 = 512_000;
/// Default wall-clock timeout in milliseconds for a single subagent.
pub const DEFAULT_SUBAGENT_TIMEOUT_MS: u64 = 60_000;
/// Hard ceiling on `timeout_ms` for any subagent.
pub const MAX_SUBAGENT_TIMEOUT_MS_CEILING: u64 = 300_000;
/// Maximum number of concurrent subagents in a single `spawn_subagent(tasks=[...])` fan-out.
pub const MAX_PARALLEL_SUBAGENTS: usize = 4;
/// Maximum byte length of the inner body of a rendered `<subagent_report>`.
pub const SUBAGENT_REPORT_MAX_BYTES: usize = 16 * 1024;

/// Built-in kernel tool registration for `spawn_subagent` (Gate 5).
///
/// Its schema metadata carries `EffectClass::ReadOnly` so `spawn_subagent` stays visible under
/// `--policy read-only` (where `role: "scout"` is permitted and `role: "worker"` is rejected with
/// `PolicyDenied` at dispatch time).
pub fn spawn_subagent_registration() -> RegisteredTool {
    RegisteredTool {
        definition: ToolDefinition {
            name: "spawn_subagent".to_string(),
            description: "Spawn a bounded child subagent with fresh context (`scout` for strictly read-only exploration or `worker` for isolated worktree mutations with three-way merge), or up to 4 concurrent subagents via `tasks`.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "role": {
                        "type": "string",
                        "enum": ["scout", "worker"],
                        "description": "Subagent role: 'scout' (read-only) or 'worker' (isolated worktree with conflict-checked merge)."
                    },
                    "task": {
                        "type": "string",
                        "description": "Bounded task prompt for a single subagent."
                    },
                    "allowed_paths": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Optional workspace-relative path prefixes or globs that a worker subagent is allowed to modify."
                    },
                    "allowed_write_globs": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Alias for allowed_paths."
                    },
                    "max_turns": {
                        "type": "integer",
                        "description": "Optional turn budget for the subagent (default 8, max 16)."
                    },
                    "max_tool_calls": {
                        "type": "integer",
                        "description": "Optional tool-call budget for the subagent (default 16, max 32)."
                    },
                    "max_tokens": {
                        "type": "integer",
                        "description": "Optional token budget for the subagent (default 64000)."
                    },
                    "timeout_ms": {
                        "type": "integer",
                        "description": "Optional wall-clock timeout in milliseconds for the subagent (default 60000)."
                    },
                    "revert_on_verify_failure": {
                        "type": "boolean",
                        "description": "If true, automatically restore the parent workspace to revision_before when post-merge verification fails."
                    },
                    "tasks": {
                        "type": "array",
                        "description": "Optional batch of 1-4 subagent specs to execute concurrently.",
                        "minItems": 1,
                        "maxItems": 4,
                        "items": {
                            "type": "object",
                            "properties": {
                                "role": { "type": "string", "enum": ["scout", "worker"] },
                                "task": { "type": "string" },
                                "allowed_paths": {
                                    "type": "array",
                                    "items": { "type": "string" }
                                },
                                "allowed_write_globs": {
                                    "type": "array",
                                    "items": { "type": "string" }
                                },
                                "max_turns": { "type": "integer" },
                                "max_tool_calls": { "type": "integer" },
                                "max_tokens": { "type": "integer" },
                                "timeout_ms": { "type": "integer" },
                                "revert_on_verify_failure": { "type": "boolean" }
                            },
                            "required": ["role", "task"],
                            "additionalProperties": false
                        }
                    }
                },
                "additionalProperties": false
            }),
        },
        metadata: ToolMetadata {
            effect_class: EffectClass::ReadOnly,
            default_timeout_sec: 120,
            inline_output_cap_bytes: 64 * 1024,
        },
    }
}

/// Role of a Gate 5 subagent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubagentRole {
    /// Strictly read-only exploration subagent running against the parent workspace.
    Scout,
    /// Isolated-worktree mutation subagent whose delta is merged back with conflict detection.
    Worker,
}

impl SubagentRole {
    pub fn as_str(&self) -> &'static str {
        match self {
            SubagentRole::Scout => "scout",
            SubagentRole::Worker => "worker",
        }
    }

    pub fn parse(raw: &str) -> Result<Self, String> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "scout" => Ok(SubagentRole::Scout),
            "worker" => Ok(SubagentRole::Worker),
            other => Err(format!(
                "unknown subagent role '{}'; expected 'scout' or 'worker'",
                other
            )),
        }
    }

    pub fn effect_class(&self) -> EffectClass {
        match self {
            SubagentRole::Scout => EffectClass::ReadOnly,
            SubagentRole::Worker => EffectClass::Mutating,
        }
    }
}

impl std::fmt::Display for SubagentRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Independent resource budget for a single subagent execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentBudget {
    pub max_turns: usize,
    pub max_tool_calls: usize,
    pub max_tokens: u64,
    pub timeout_ms: u64,
}

impl Default for SubagentBudget {
    fn default() -> Self {
        Self {
            max_turns: DEFAULT_SUBAGENT_MAX_TURNS,
            max_tool_calls: DEFAULT_SUBAGENT_MAX_TOOL_CALLS,
            max_tokens: DEFAULT_SUBAGENT_MAX_TOKENS,
            timeout_ms: DEFAULT_SUBAGENT_TIMEOUT_MS,
        }
    }
}

impl SubagentBudget {
    pub fn clamped(&self) -> Self {
        Self {
            max_turns: self.max_turns.clamp(1, MAX_SUBAGENT_TURNS_CEILING),
            max_tool_calls: self.max_tool_calls.clamp(1, MAX_SUBAGENT_TOOL_CALLS_CEILING),
            max_tokens: self.max_tokens.clamp(1, MAX_SUBAGENT_TOKENS_CEILING),
            timeout_ms: self.timeout_ms.clamp(1, MAX_SUBAGENT_TIMEOUT_MS_CEILING),
        }
    }

    pub fn to_budget_config(&self, parent: &BudgetConfig) -> BudgetConfig {
        let clamped = self.clamped();
        let mut cfg = parent.clone();
        cfg.max_model_turns = clamped.max_turns.min(parent.max_model_turns.max(1));
        cfg.max_model_request_attempts = cfg.max_model_turns.saturating_add(4);
        cfg.max_tool_calls = clamped.max_tool_calls.min(parent.max_tool_calls.max(1));
        let sub_dur = Duration::from_millis(clamped.timeout_ms);
        cfg.max_run_duration = sub_dur.min(parent.max_run_duration.max(Duration::from_millis(1)));
        cfg
    }
}

/// Complete specification for spawning one child subagent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentSpec {
    pub role: SubagentRole,
    pub task: String,
    #[serde(default, alias = "allowed_paths")]
    pub allowed_write_globs: Option<Vec<String>>,
    #[serde(default)]
    pub budget: SubagentBudget,
    #[serde(default)]
    pub revert_on_verify_failure: bool,
}

impl SubagentSpec {
    pub fn scout(task: impl Into<String>) -> Self {
        Self {
            role: SubagentRole::Scout,
            task: task.into(),
            allowed_write_globs: None,
            budget: SubagentBudget::default(),
            revert_on_verify_failure: false,
        }
    }

    pub fn worker(task: impl Into<String>) -> Self {
        Self {
            role: SubagentRole::Worker,
            task: task.into(),
            allowed_write_globs: None,
            budget: SubagentBudget::default(),
            revert_on_verify_failure: false,
        }
    }

    pub fn with_allowed_write_globs(mut self, globs: Vec<String>) -> Self {
        self.allowed_write_globs =
            normalize_and_validate_globs(None, Some(globs.clone())).unwrap_or(Some(globs));
        self
    }

    pub fn with_allowed_paths(mut self, paths: Vec<String>) -> Self {
        self.allowed_write_globs =
            normalize_and_validate_globs(Some(paths.clone()), None).unwrap_or(Some(paths));
        self
    }

    pub fn with_budget(mut self, budget: SubagentBudget) -> Self {
        self.budget = budget;
        self
    }

    pub fn with_max_turns(mut self, max_turns: usize) -> Self {
        self.budget.max_turns = max_turns;
        self
    }

    pub fn with_timeout_ms(mut self, timeout_ms: u64) -> Self {
        self.budget.timeout_ms = timeout_ms;
        self
    }

    pub fn with_revert_on_verify_failure(mut self, revert: bool) -> Self {
        self.revert_on_verify_failure = revert;
        self
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSubagentTaskItem {
    role: String,
    task: String,
    #[serde(default)]
    allowed_paths: Option<Vec<String>>,
    #[serde(default)]
    allowed_write_globs: Option<Vec<String>>,
    #[serde(default)]
    max_turns: Option<usize>,
    #[serde(default)]
    max_tool_calls: Option<usize>,
    #[serde(default)]
    max_tokens: Option<u64>,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    revert_on_verify_failure: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSpawnSubagentArgs {
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    task: Option<String>,
    #[serde(default)]
    allowed_paths: Option<Vec<String>>,
    #[serde(default)]
    allowed_write_globs: Option<Vec<String>>,
    #[serde(default)]
    max_turns: Option<usize>,
    #[serde(default)]
    max_tool_calls: Option<usize>,
    #[serde(default)]
    max_tokens: Option<u64>,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    revert_on_verify_failure: Option<bool>,
    #[serde(default)]
    tasks: Option<Vec<RawSubagentTaskItem>>,
}

fn normalize_and_validate_globs(
    allowed_paths: Option<Vec<String>>,
    allowed_write_globs: Option<Vec<String>>,
) -> Result<Option<Vec<String>>, String> {
    let Some(raw_list) = allowed_paths.or(allowed_write_globs) else {
        return Ok(None);
    };
    let mut normalized = Vec::with_capacity(raw_list.len());
    for raw in raw_list {
        let trimmed = raw.trim().replace('\\', "/");
        let clean = trimmed
            .trim_start_matches("./")
            .trim_matches('/')
            .to_string();
        if clean.is_empty() {
            return Err("allowed_paths entries must not be empty".to_string());
        }
        if Path::new(&clean)
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::RootDir))
        {
            return Err(format!(
                "allowed_paths entry '{}' must not contain '..' or root components",
                raw
            ));
        }
        normalized.push(clean);
    }
    if normalized.is_empty() {
        return Err("allowed_paths array must not be empty when specified".to_string());
    }
    Ok(Some(normalized))
}

/// Parse the JSON arguments of a `spawn_subagent` call into 1..=`MAX_PARALLEL_SUBAGENTS` specs.
pub fn parse_spawn_subagent_specs(raw_args: &str) -> Result<Vec<SubagentSpec>, String> {
    let parsed: RawSpawnSubagentArgs = serde_json::from_str(raw_args)
        .map_err(|e| format!("invalid JSON arguments for `spawn_subagent`: {}", e))?;

    if let Some(items) = parsed.tasks {
        if items.is_empty() {
            return Err("`tasks` must contain at least 1 subagent specification".to_string());
        }
        if items.len() > MAX_PARALLEL_SUBAGENTS {
            return Err(format!(
                "`tasks` count {} exceeds maximum parallel subagent fan-out ({})",
                items.len(),
                MAX_PARALLEL_SUBAGENTS
            ));
        }
        let mut specs = Vec::with_capacity(items.len());
        for item in items {
            let role = SubagentRole::parse(&item.role)?;
            let task = item.task.trim().to_string();
            if task.is_empty() {
                return Err("subagent `task` must not be empty".to_string());
            }
            let max_turns = item
                .max_turns
                .or(parsed.max_turns)
                .unwrap_or(DEFAULT_SUBAGENT_MAX_TURNS);
            let max_tool_calls = item
                .max_tool_calls
                .or(parsed.max_tool_calls)
                .unwrap_or(DEFAULT_SUBAGENT_MAX_TOOL_CALLS);
            let max_tokens = item
                .max_tokens
                .or(parsed.max_tokens)
                .unwrap_or(DEFAULT_SUBAGENT_MAX_TOKENS);
            let timeout_ms = item
                .timeout_ms
                .or(parsed.timeout_ms)
                .unwrap_or(DEFAULT_SUBAGENT_TIMEOUT_MS);
            if max_turns == 0 || timeout_ms == 0 || max_tool_calls == 0 || max_tokens == 0 {
                return Err(
                    "subagent budget values (max_turns, max_tool_calls, max_tokens, timeout_ms) must be > 0"
                        .to_string(),
                );
            }
            let allowed_write_globs = normalize_and_validate_globs(
                item.allowed_paths.or_else(|| parsed.allowed_paths.clone()),
                item.allowed_write_globs
                    .or_else(|| parsed.allowed_write_globs.clone()),
            )?;
            let revert_on_verify_failure = item
                .revert_on_verify_failure
                .or(parsed.revert_on_verify_failure)
                .unwrap_or(false);
            specs.push(SubagentSpec {
                role,
                task,
                allowed_write_globs,
                budget: SubagentBudget {
                    max_turns,
                    max_tool_calls,
                    max_tokens,
                    timeout_ms,
                }
                .clamped(),
                revert_on_verify_failure,
            });
        }
        return Ok(specs);
    }

    let role_raw = parsed
        .role
        .as_deref()
        .ok_or_else(|| "missing required `role` ('scout' or 'worker') or `tasks`".to_string())?;
    let role = SubagentRole::parse(role_raw)?;
    let task = parsed
        .task
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "missing required non-empty `task`".to_string())?
        .to_string();
    let max_turns = parsed.max_turns.unwrap_or(DEFAULT_SUBAGENT_MAX_TURNS);
    let max_tool_calls = parsed
        .max_tool_calls
        .unwrap_or(DEFAULT_SUBAGENT_MAX_TOOL_CALLS);
    let max_tokens = parsed.max_tokens.unwrap_or(DEFAULT_SUBAGENT_MAX_TOKENS);
    let timeout_ms = parsed.timeout_ms.unwrap_or(DEFAULT_SUBAGENT_TIMEOUT_MS);
    if max_turns == 0 || timeout_ms == 0 || max_tool_calls == 0 || max_tokens == 0 {
        return Err(
            "subagent budget values (max_turns, max_tool_calls, max_tokens, timeout_ms) must be > 0"
                .to_string(),
        );
    }
    let allowed_write_globs =
        normalize_and_validate_globs(parsed.allowed_paths, parsed.allowed_write_globs)?;
    Ok(vec![SubagentSpec {
        role,
        task,
        allowed_write_globs,
        budget: SubagentBudget {
            max_turns,
            max_tool_calls,
            max_tokens,
            timeout_ms,
        }
        .clamped(),
        revert_on_verify_failure: parsed.revert_on_verify_failure.unwrap_or(false),
    }])
}

/// Check whether a workspace-relative path matches at least one entry in `allowed_globs` (either
/// as an exact file path, a directory prefix anchored at the workspace root, or a glob pattern).
pub fn is_path_allowed_by_scope(rel_path: &str, allowed_globs: &[String]) -> bool {
    let sanitized_rel = rel_path.trim().replace('\\', "/");
    let Ok(validated_rel) = WorkspaceCap::validate_relative_components(&sanitized_rel) else {
        return false;
    };
    let norm_path = validated_rel.to_string_lossy().replace('\\', "/");
    if norm_path.is_empty() || norm_path == "." {
        return false;
    }
    for raw_pat in allowed_globs {
        let trimmed = raw_pat
            .trim()
            .replace('\\', "/")
            .trim_start_matches("./")
            .trim_matches('/')
            .to_string();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed == "." || trimmed == "*" || trimmed == "**" {
            return true;
        }
        if Path::new(&trimmed)
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::RootDir))
        {
            continue;
        }
        let mut pat_parts = Vec::new();
        for comp in Path::new(&trimmed).components() {
            if let Component::Normal(part) = comp {
                pat_parts.push(part.to_string_lossy().to_string());
            }
        }
        let pat = pat_parts.join("/");
        if pat.is_empty() {
            continue;
        }
        let has_glob_meta =
            pat.contains('*') || pat.contains('?') || pat.contains('[') || pat.contains('{');
        if !has_glob_meta {
            if norm_path == pat || norm_path.starts_with(&format!("{}/", pat)) {
                return true;
            }
        } else if let Ok(re) = compile_glob_to_regex(&pat) {
            if re.is_match(&norm_path) {
                return true;
            }
        }
    }
    false
}

/// Fail-closed `PreToolUse` `PolicyHook` mounted inside a `Worker` subagent when `allowed_paths`
/// / `allowed_write_globs` is specified, rejecting out-of-scope `edit` and `write` calls before
/// they touch the worktree.
struct AllowedWriteGlobsPolicyHook {
    globs: Vec<String>,
}

impl PolicyHook for AllowedWriteGlobsPolicyHook {
    fn id(&self) -> &str {
        "worker-allowed-write-globs"
    }

    fn events(&self) -> &'static [HookEvent] {
        &[HookEvent::PreToolUse]
    }

    fn evaluate(
        &self,
        _event: HookEvent,
        ctx: &HookContext,
    ) -> Result<PolicyHookDecision, HookError> {
        let Some(tool_name) = ctx.tool_name.as_deref() else {
            return Ok(PolicyHookDecision::Allow);
        };
        if !matches!(tool_name, "edit" | "write") {
            return Ok(PolicyHookDecision::Allow);
        }
        let rel_path = ctx.relative_path.clone().or_else(|| {
            let raw = ctx.raw_arguments.as_deref()?;
            let val: serde_json::Value = serde_json::from_str(raw).ok()?;
            val.get("path")?.as_str().map(|s| s.to_string())
        });
        let Some(path) = rel_path else {
            return Ok(PolicyHookDecision::Deny {
                reason: "missing target path for scoped worker write".to_string(),
            });
        };
        if is_path_allowed_by_scope(&path, &self.globs) {
            Ok(PolicyHookDecision::Allow)
        } else {
            Ok(PolicyHookDecision::Deny {
                reason: format!(
                    "path '{}' is outside worker allowed_paths {:?}",
                    path, self.globs
                ),
            })
        }
    }
}

/// Outcome of merging an isolated `Worker` subagent's worktree back into the parent workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum WorktreeMergeResult {
    /// Clean three-way merge applied to the parent workspace (and post-merge verification passed
    /// or was not configured).
    Applied {
        base_revision: RevisionId,
        merged_revision: RevisionId,
        written_files: Vec<String>,
        removed_files: Vec<String>,
    },
    /// Merge refused because the parent workspace modified one or more of the same paths since
    /// `base_revision`, or because a worker change violated `allowed_paths` / security boundaries.
    /// The parent workspace was not modified.
    Conflict {
        base_revision: RevisionId,
        conflicting_paths: Vec<String>,
        reasons: Vec<String>,
        worker_worktree_preserved: bool,
        preserved_worktree_path: Option<PathBuf>,
    },
    /// The worker delta merged cleanly, but the post-merge [`CompletionGate`] rejected the merged
    /// parent workspace. When `reverted == true` (`revert_on_verify_failure: true`), the parent
    /// workspace was automatically restored to `revision_before`.
    VerificationFailed {
        merged_revision: RevisionId,
        revision_before: Option<RevisionId>,
        reverted: bool,
        written_files: Vec<String>,
        removed_files: Vec<String>,
        gate_output: String,
    },
}

impl WorktreeMergeResult {
    pub fn status_str(&self) -> &'static str {
        match self {
            WorktreeMergeResult::Applied { .. } => "applied",
            WorktreeMergeResult::Conflict { .. } => "conflict",
            WorktreeMergeResult::VerificationFailed { reverted: true, .. } => {
                "verification_failed_reverted"
            }
            WorktreeMergeResult::VerificationFailed {
                reverted: false, ..
            } => "verification_failed",
        }
    }

    pub fn is_applied(&self) -> bool {
        matches!(self, WorktreeMergeResult::Applied { .. })
    }

    pub fn is_conflict(&self) -> bool {
        matches!(self, WorktreeMergeResult::Conflict { .. })
    }

    pub fn is_verification_failed(&self) -> bool {
        matches!(self, WorktreeMergeResult::VerificationFailed { .. })
    }
}

/// Isolated filesystem worktree materialized from a parent [`SnapshotStore`] revision.
///
/// The worktree holds its own [`WorkspaceCap`] rooted at `root`, so a `Worker` subagent running
/// against it is physically isolated from the parent workspace. Unless preserved on a merge
/// conflict, dropping the `IsolatedWorktree` removes `root` from disk, guaranteeing deterministic
/// cleanup on cancellation, timeout, or early return.
pub struct IsolatedWorktree {
    subagent_id: String,
    root: PathBuf,
    workspace: WorkspaceCap,
    base_revision: WorkspaceRevision,
    cleaned_up: bool,
    preserve_on_drop: bool,
}

impl IsolatedWorktree {
    /// Materialize a fresh isolated worktree directory from `base_revision` (or capture a fresh
    /// baseline revision from `parent_workspace` if `base_revision` is `None`).
    pub fn materialize(
        subagent_id: impl Into<String>,
        parent_workspace: &WorkspaceCap,
        snapshot_store: &SnapshotStore,
        base_revision: Option<&RevisionId>,
        worktree_root: PathBuf,
    ) -> Result<Self, RevisionError> {
        let subagent_id = subagent_id.into();
        let base_manifest = match base_revision {
            Some(rev_id) => snapshot_store.load_revision(rev_id)?,
            None => snapshot_store.capture_revision(parent_workspace, None, None, 0)?,
        };
        if worktree_root.exists() {
            let _ = fs::remove_dir_all(&worktree_root);
        }
        ensure_private_dir(&worktree_root)?;
        let workspace = WorkspaceCap::new(&worktree_root)?;
        for (path, entry) in &base_manifest.tracked_files {
            let bytes = snapshot_store.read_blob(&entry.sha256)?;
            write_tracked_file(&workspace, path, &bytes, entry.mode, false)?;
        }
        Ok(Self {
            subagent_id,
            root: worktree_root,
            workspace,
            base_revision: base_manifest,
            cleaned_up: false,
            preserve_on_drop: false,
        })
    }

    pub fn subagent_id(&self) -> &str {
        &self.subagent_id
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn workspace(&self) -> &WorkspaceCap {
        &self.workspace
    }

    pub fn base_revision(&self) -> &WorkspaceRevision {
        &self.base_revision
    }

    pub fn is_cleaned_up(&self) -> bool {
        self.cleaned_up
    }

    /// Explicitly remove the isolated worktree directory from disk.
    pub fn cleanup(&mut self) -> Result<(), std::io::Error> {
        self.preserve_on_drop = false;
        if !self.cleaned_up && self.root.exists() {
            fs::remove_dir_all(&self.root)?;
        }
        self.cleaned_up = true;
        Ok(())
    }

    /// Three-way baseline-aware merge of this isolated worktree back into `parent_workspace`,
    /// followed by optional `post_merge_gate` verification and `revert_on_verify_failure` rollback.
    #[allow(clippy::too_many_arguments)]
    pub async fn merge_into_parent(
        &mut self,
        parent_workspace: &WorkspaceCap,
        snapshot_store: &SnapshotStore,
        current_parent_revision: Option<&RevisionId>,
        allowed_write_globs: Option<&[String]>,
        post_merge_gate: Option<&SharedCompletionGate>,
        revert_on_verify_failure: bool,
        turn_index: usize,
    ) -> Result<WorktreeMergeResult, RevisionError> {
        let (worker_tracked, worker_skipped) =
            snapshot_store.scan_workspace_to_blobs(&self.workspace)?;
        let base_files = &self.base_revision.tracked_files;
        let base_skipped: BTreeSet<&str> = self
            .base_revision
            .skipped_paths
            .iter()
            .map(|s| s.as_str())
            .collect();

        let mut written_entries: BTreeMap<String, TrackedFile> = BTreeMap::new();
        for (path, entry) in &worker_tracked {
            if base_files.get(path) != Some(entry) {
                written_entries.insert(path.clone(), entry.clone());
            }
        }
        let mut removed_files: Vec<String> = Vec::new();
        for path in base_files.keys() {
            if !worker_tracked.contains_key(path) && !base_skipped.contains(path.as_str()) {
                removed_files.push(path.clone());
            }
        }

        let mut delta_paths: BTreeSet<String> = written_entries.keys().cloned().collect();
        delta_paths.extend(removed_files.iter().cloned());

        let mut conflicting_paths: BTreeSet<String> = BTreeSet::new();
        let mut reasons: Vec<String> = Vec::new();

        // `IsolatedWorktree::materialize` never copies `base_skipped` entries into the worktree,
        // so every entry in `worker_skipped` was created during the worker's run. Ignore harmless
        // excluded build/cache directories (`target`, `node_modules`, etc.) when they are real
        // directories, and unconditionally flag any symlink, FIFO/special file, sensitive file, or
        // oversized file (even if its path happened to appear in `base_skipped`).
        for skipped in &worker_skipped {
            let is_harmless_excluded_dir = Path::new(skipped)
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|name| SNAPSHOT_EXCLUDED_DIRS.contains(&name))
                && matches!(
                    stat_live_entry(&self.workspace, skipped)?,
                    LiveEntryStat::Directory
                );
            if !is_harmless_excluded_dir {
                conflicting_paths.insert(skipped.clone());
                reasons.push(format!(
                    "worker created disallowed skipped/non-regular/sensitive entry '{}'",
                    skipped
                ));
            }
        }

        for path in &delta_paths {
            if let Some(globs) = allowed_write_globs {
                if !is_path_allowed_by_scope(path, globs) {
                    conflicting_paths.insert(path.clone());
                    reasons.push(format!(
                        "path '{}' violates worker allowed_paths {:?}",
                        path, globs
                    ));
                    continue;
                }
            }
            let norm = match WorkspaceCap::validate_relative_components(path) {
                Ok(n) => n,
                Err(e) => {
                    conflicting_paths.insert(path.clone());
                    reasons.push(format!("invalid path '{}': {}", path, e));
                    continue;
                }
            };
            if SensitivePathGuard::is_sensitive(&norm) {
                conflicting_paths.insert(path.clone());
                reasons.push(format!("sensitive path '{}' cannot be merged", path));
                continue;
            }

            // Check that no ancestor component in the parent workspace was replaced by a symlink
            // or a conflicting regular file (unless that regular file is itself in `removed_files`).
            if let Some(parent_rel) = norm.parent() {
                let mut cur_anc = PathBuf::new();
                for comp in parent_rel.components() {
                    if let Component::Normal(part) = comp {
                        cur_anc.push(part);
                        let anc_str = cur_anc.to_string_lossy().to_string();
                        match stat_live_entry(parent_workspace, &anc_str)? {
                            LiveEntryStat::Absent | LiveEntryStat::Directory => {}
                            LiveEntryStat::Regular { .. }
                                if removed_files.iter().any(|r| r == &anc_str) => {}
                            LiveEntryStat::Regular { .. } | LiveEntryStat::NonRegular => {
                                conflicting_paths.insert(path.clone());
                                reasons.push(format!(
                                    "ancestor '{}' in parent workspace is a non-directory entry",
                                    anc_str
                                ));
                            }
                        }
                    }
                }
            }

            // Baseline conflict check against the live parent workspace.
            let parent_live = live_entry_state(parent_workspace, path)?;
            match base_files.get(path) {
                Some(base_entry) => match parent_live {
                    LiveEntryState::Regular { sha256, mode, .. }
                        if sha256 == base_entry.sha256
                            && (mode & 0o7777) == (base_entry.mode & 0o7777) => {}
                    LiveEntryState::Absent => {
                        conflicting_paths.insert(path.clone());
                        reasons.push(format!(
                            "parent workspace deleted '{}' since base revision {}",
                            path, self.base_revision.revision_id
                        ));
                    }
                    _ => {
                        conflicting_paths.insert(path.clone());
                        reasons.push(format!(
                            "parent workspace modified '{}' since base revision {}",
                            path, self.base_revision.revision_id
                        ));
                    }
                },
                None => match parent_live {
                    LiveEntryState::Absent => {}
                    LiveEntryState::Directory if is_ancestor_dir_of(&self.base_revision, path) => {
                        conflicting_paths.insert(path.clone());
                        reasons.push(format!(
                            "path '{}' is a tracked directory in parent workspace",
                            path
                        ));
                    }
                    LiveEntryState::Regular { sha256, mode, .. } => {
                        let matches_worker = written_entries.get(path).is_some_and(|w| {
                            w.sha256 == sha256 && (w.mode & 0o7777) == (mode & 0o7777)
                        });
                        if !matches_worker {
                            conflicting_paths.insert(path.clone());
                            reasons.push(format!(
                                "parent workspace concurrently created '{}' since base revision {}",
                                path, self.base_revision.revision_id
                            ));
                        }
                    }
                    _ => {
                        conflicting_paths.insert(path.clone());
                        reasons.push(format!(
                            "parent workspace concurrently created '{}' since base revision {}",
                            path, self.base_revision.revision_id
                        ));
                    }
                },
            }
        }

        if !conflicting_paths.is_empty() {
            self.preserve_on_drop = true;
            return Ok(WorktreeMergeResult::Conflict {
                base_revision: self.base_revision.revision_id.clone(),
                conflicting_paths: conflicting_paths.into_iter().collect(),
                reasons,
                worker_worktree_preserved: true,
                preserved_worktree_path: Some(self.root.clone()),
            });
        }

        if delta_paths.is_empty() {
            let _ = self.cleanup();
            let rev = current_parent_revision
                .cloned()
                .unwrap_or_else(|| self.base_revision.revision_id.clone());
            return Ok(WorktreeMergeResult::Applied {
                base_revision: rev.clone(),
                merged_revision: rev,
                written_files: Vec::new(),
                removed_files: Vec::new(),
            });
        }

        // Always capture a fresh pre-merge revision of the live `parent_workspace` immediately
        // before applying `removed_files`/`written_entries`, so `revert_on_verify_failure` and
        // `--undo` restore the exact pre-merge workspace without clobbering un-snapshotted parent files.
        let pre_merge_rev = snapshot_store
            .capture_revision(parent_workspace, current_parent_revision, None, turn_index)?
            .revision_id;

        // Apply removals deepest-first, then writes in sorted path order.
        removed_files.sort_by(|a, b| {
            let depth = |s: &String| s.matches('/').count();
            depth(b).cmp(&depth(a)).then_with(|| b.cmp(a))
        });
        for path in &removed_files {
            remove_tracked_file(parent_workspace, path, false)?;
        }
        removed_files.sort();

        let mut written_files: Vec<String> = Vec::with_capacity(written_entries.len());
        for (path, entry) in &written_entries {
            let bytes = snapshot_store.read_blob(&entry.sha256)?;
            write_tracked_file(parent_workspace, path, &bytes, entry.mode, false)?;
            written_files.push(path.clone());
        }

        let _ = self.cleanup();

        let merged_manifest = snapshot_store.capture_revision(
            parent_workspace,
            Some(&pre_merge_rev),
            None,
            turn_index,
        )?;
        let merged_revision = merged_manifest.revision_id;

        if let Some(gate) = post_merge_gate {
            let mut mod_list: Vec<String> = delta_paths.into_iter().collect();
            mod_list.sort();
            let gate_ctx = CompletionGateContext {
                run_id: RunId::new(),
                workspace_root: parent_workspace.root().to_path_buf(),
                modified_files: mod_list,
                turns_used: turn_index,
            };
            let failure_feedback = match gate.check(&gate_ctx).await {
                Ok(GateOutcome::Approved) | Ok(GateOutcome::Skipped { .. }) => None,
                Ok(GateOutcome::Rejected { feedback }) => Some(feedback),
                Err(e) => Some(format!("post-merge verification gate error: {}", e)),
            };
            if let Some(gate_output) = failure_feedback {
                let mut reverted = false;
                if revert_on_verify_failure {
                    if let Ok(crate::revision::RestoreResult::Applied { .. }) = snapshot_store
                        .restore_revision(
                            parent_workspace,
                            &pre_merge_rev,
                            Some(&merged_revision),
                            false,
                        )
                    {
                        reverted = true;
                        if merged_revision != pre_merge_rev {
                            let _ = snapshot_store.remove_unlinked_revision(&merged_revision);
                        }
                    }
                }
                return Ok(WorktreeMergeResult::VerificationFailed {
                    merged_revision,
                    revision_before: Some(pre_merge_rev),
                    reverted,
                    written_files,
                    removed_files,
                    gate_output,
                });
            }
        }

        Ok(WorktreeMergeResult::Applied {
            base_revision: pre_merge_rev,
            merged_revision,
            written_files,
            removed_files,
        })
    }
}

impl Drop for IsolatedWorktree {
    fn drop(&mut self) {
        if !self.cleaned_up && !self.preserve_on_drop && self.root.exists() {
            let _ = fs::remove_dir_all(&self.root);
            self.cleaned_up = true;
        }
    }
}

/// Bounded structured handoff artifact returned by a completed (or cancelled/failed) subagent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubagentResult {
    pub subagent_id: String,
    pub role: SubagentRole,
    pub status: RunStatus,
    pub terminal_reason: String,
    pub summary: String,
    pub relevant_files: Vec<String>,
    pub findings: Vec<String>,
    pub turns_used: usize,
    pub tool_calls_used: usize,
    pub tokens_used: u64,
    pub duration_ms: u64,
    pub merge_result: Option<WorktreeMergeResult>,
    pub worktree_cleaned_up: bool,
}

impl SubagentResult {
    /// Status label placed in the `<subagent_report status="...">` attribute and telemetry.
    pub fn report_status(&self) -> &'static str {
        if let Some(ref merge) = self.merge_result {
            match merge {
                WorktreeMergeResult::Conflict { .. } => return "conflict",
                WorktreeMergeResult::VerificationFailed { reverted: true, .. } => {
                    return "verification_failed_reverted"
                }
                WorktreeMergeResult::VerificationFailed {
                    reverted: false, ..
                } => return "verification_failed",
                WorktreeMergeResult::Applied { .. } => {}
            }
        }
        match self.status {
            RunStatus::Completed => "completed",
            RunStatus::Cancelled => "cancelled",
            RunStatus::BudgetExhausted => "budget_exhausted",
            RunStatus::Failed => "failed",
            _ => "failed",
        }
    }

    /// Render this subagent handoff into a sanitized `<subagent_report ...>...</subagent_report>`
    /// block suitable for the parent model's `ToolResultMessage`.
    pub fn render_report(&self) -> String {
        let status = self.report_status();
        let mut attrs = format!(
            " role=\"{}\" id=\"{}\" status=\"{}\"",
            escape_fence_attribute(self.role.as_str()),
            escape_fence_attribute(&self.subagent_id),
            escape_fence_attribute(status),
        );
        if let Some(WorktreeMergeResult::VerificationFailed { reverted, .. }) = &self.merge_result {
            attrs.push_str(&format!(" reverted=\"{}\"", reverted));
        }

        let mut body = String::new();
        body.push_str(&format!(
            "subagent_id: {}\nrole: {}\nstatus: {}\nterminal_reason: {}\nbudget_usage: turns={} tool_calls={} tokens={} duration_ms={}\n",
            self.subagent_id,
            self.role.as_str(),
            status,
            self.terminal_reason,
            self.turns_used,
            self.tool_calls_used,
            self.tokens_used,
            self.duration_ms,
        ));
        if !self.relevant_files.is_empty() {
            body.push_str(&format!(
                "relevant_files: {}\n",
                self.relevant_files.join(", ")
            ));
        }
        if !self.findings.is_empty() {
            body.push_str("findings:\n");
            for finding in &self.findings {
                body.push_str(&format!("  - {}\n", finding));
            }
        }
        if let Some(ref merge) = self.merge_result {
            match merge {
                WorktreeMergeResult::Applied {
                    base_revision,
                    merged_revision,
                    written_files,
                    removed_files,
                } => {
                    body.push_str(&format!(
                        "worktree_merge: applied (base_revision={}, merged_revision={}, written={:?}, removed={:?})\n",
                        base_revision, merged_revision, written_files, removed_files
                    ));
                }
                WorktreeMergeResult::Conflict {
                    base_revision,
                    conflicting_paths,
                    reasons,
                    worker_worktree_preserved,
                    ..
                } => {
                    body.push_str(&format!(
                        "worktree_merge: conflict (base_revision={}, conflicting_paths={:?}, preserved={})\n",
                        base_revision, conflicting_paths, worker_worktree_preserved
                    ));
                    for reason in reasons {
                        body.push_str(&format!("  conflict_reason: {}\n", reason));
                    }
                }
                WorktreeMergeResult::VerificationFailed {
                    merged_revision,
                    revision_before,
                    reverted,
                    written_files,
                    removed_files,
                    gate_output,
                } => {
                    body.push_str(&format!(
                        "worktree_merge: verification_failed (reverted={}, revision_before={}, merged_revision={}, written={:?}, removed={:?})\ngate_output: {}\n",
                        reverted,
                        revision_before
                            .as_ref()
                            .map(|r| r.as_str())
                            .unwrap_or("<none>"),
                        merged_revision,
                        written_files,
                        removed_files,
                        gate_output.lines().take(12).collect::<Vec<_>>().join(" | ")
                    ));
                }
            }
        }
        body.push_str("summary:\n");
        body.push_str(&self.summary);

        fence_untrusted("subagent_report", &attrs, &body, SUBAGENT_REPORT_MAX_BYTES)
    }
}

/// Render a batch of 1..=4 subagent results into a single `ToolResultMessage` payload.
pub fn render_subagent_batch_report(results: &[SubagentResult]) -> String {
    results
        .iter()
        .map(SubagentResult::render_report)
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Execution context handed to [`SubagentBackend::run_subagent`] for one child subagent.
pub struct SubagentExecutionRequest {
    pub subagent_id: String,
    pub spec: SubagentSpec,
    /// For `Scout`, the parent's `WorkspaceCap`; for `Worker`, the isolated worktree's `WorkspaceCap`.
    pub workspace: WorkspaceCap,
    pub state_dir: PathBuf,
    pub workspace_skills: Vec<SkillEntry>,
    pub parent_policy_mode: PolicyMode,
    pub parent_budget: BudgetConfig,
    pub predictive_check: bool,
    pub cancel_rx: watch::Receiver<bool>,
}

/// Raw execution outcome returned by [`SubagentBackend::run_subagent`] before worktree merge.
#[derive(Debug, Clone)]
pub struct SubagentRunOutcome {
    pub status: RunStatus,
    pub terminal_reason: TerminalReason,
    pub summary: String,
    pub relevant_files: Vec<String>,
    pub findings: Vec<String>,
    pub turns_used: usize,
    pub tool_calls_used: usize,
    pub tokens_used: u64,
    pub duration_ms: u64,
    /// Visible tool names that were exposed to this subagent (for invariant verification).
    pub visible_tools: Vec<String>,
}

/// Pluggable execution backend for child subagents.
///
/// Production runs use [`ProviderSubagentBackend`], which drives a real child [`AgentRunner`] loop
/// with fresh context (`context: "fresh"`), bounded budget, and `spawn_subagent` physically removed
/// from the child's [`ToolRegistry`].
pub trait SubagentBackend: Send + Sync {
    fn run_subagent(
        &self,
        req: SubagentExecutionRequest,
    ) -> Pin<Box<dyn Future<Output = Result<SubagentRunOutcome, String>> + Send + '_>>;
}

/// Default [`SubagentBackend`] that runs a child [`AgentRunner`] using a [`ModelProvider`].
pub struct ProviderSubagentBackend {
    provider: Arc<dyn ModelProvider>,
}

impl ProviderSubagentBackend {
    pub fn new(provider: Arc<dyn ModelProvider>) -> Self {
        Self { provider }
    }
}

impl SubagentBackend for ProviderSubagentBackend {
    fn run_subagent(
        &self,
        req: SubagentExecutionRequest,
    ) -> Pin<Box<dyn Future<Output = Result<SubagentRunOutcome, String>> + Send + '_>> {
        let provider = Arc::clone(&self.provider);
        Box::pin(async move {
            let started = Instant::now();
            fs::create_dir_all(&req.state_dir)
                .map_err(|e| format!("failed to create subagent state dir: {}", e))?;
            let env = Arc::new(
                LocalHostEnv::new(req.workspace.root(), req.state_dir.clone())
                    .map_err(|e| format!("failed to initialize subagent execution env: {}", e))?,
            );

            // Anti-recursion invariant: child subagents NEVER have `spawn_subagent` registered.
            let registry = ToolRegistry::new(None)
                .without_subagents()
                .with_workspace_skills(req.workspace_skills.clone());

            // Role-specific policy & scoped write guard.
            let policy_mode = match req.spec.role {
                SubagentRole::Scout => PolicyMode::ReadOnly,
                SubagentRole::Worker => {
                    if req.parent_policy_mode.allows_shell_auto() {
                        PolicyMode::FullAutonomous
                    } else {
                        PolicyMode::AllowWrite
                    }
                }
            };
            if req.spec.role == SubagentRole::Worker {
                if let Some(ref globs) = req.spec.allowed_write_globs {
                    let mut manifest = ExtensionManifest::operator_trusted(
                        "worker-scope-guard",
                        "spawn_subagent.allowed_paths",
                    );
                    manifest.capabilities.push("policy_hook".to_string());
                    let ext = ScopedExtension::new(manifest).with_policy_hook(Arc::new(
                        AllowedWriteGlobsPolicyHook {
                            globs: globs.clone(),
                        },
                    ));
                    let _ = registry.register_extension(ext);
                }
            }

            let policy_engine =
                PolicyEngine::new(policy_mode, Arc::new(NonInteractiveApprovalHandler));
            let visible_tools: Vec<String> = registry
                .visible_definitions_for_engine(&policy_engine)
                .into_iter()
                .map(|d| d.name)
                .collect();

            let child_budget = req.spec.budget.clamped().to_budget_config(&req.parent_budget);
            let runtime = build_runtime_with_registry(
                registry,
                env.clone(),
                policy_engine,
                child_budget.clone(),
            )
            .with_predictive_check(req.predictive_check);

            let role_banner = match req.spec.role {
                SubagentRole::Scout => {
                    "\n\n[SUBAGENT ROLE: scout — Strictly read-only exploration subagent. You may only use read-only tools (`read`, `grep`, `find`, `ls`, `activate_skill`, `recall_evidence`). `spawn_subagent`, `edit`, `write`, and `bash` are disabled. Return a concise factual summary with relevant file paths and line numbers.]"
                }
                SubagentRole::Worker => {
                    "\n\n[SUBAGENT ROLE: worker — Isolated worktree mutation subagent. You are operating inside an isolated copy of the workspace; `spawn_subagent` is disabled. Make minimal, targeted changes within your assigned scope and summarize the modified files.]"
                }
            };
            let system_prompt = format!(
                "{}{}",
                crate::prompt::DEFAULT_SYSTEM_PROMPT,
                role_banner
            );
            let prompt_builder =
                PromptBuilder::new(system_prompt).with_workspace_context(env.workspace());

            let child_run_duration = child_budget.max_run_duration;
            let run_cfg = AgentRunConfig::new(req.spec.task.clone(), child_budget);
            let mut child_logger =
                EventLogger::new(run_cfg.run_id, Some(req.state_dir.clone()), false)
                    .map_err(|e| format!("failed to create subagent event logger: {}", e))?;
            child_logger.set_live_stderr(false);

            let mut parent_cancel = req.cancel_rx;
            let (child_cancel_tx, child_cancel_rx) = watch::channel(*parent_cancel.borrow());
            let timer_watcher = tokio::spawn(async move {
                tokio::select! {
                    _ = tokio::time::sleep(child_run_duration) => {
                        let _ = child_cancel_tx.send(true);
                    }
                    _ = async {
                        loop {
                            if *parent_cancel.borrow() {
                                break;
                            }
                            if parent_cancel.changed().await.is_err() {
                                std::future::pending::<()>().await;
                            }
                        }
                    } => {
                        let _ = child_cancel_tx.send(true);
                    }
                }
            });

            let runner = AgentRunner::new(run_cfg, provider, runtime, child_cancel_rx)
                .with_prompt_builder(prompt_builder)
                .with_subtree_context(SubtreeContextTracker::new(true))
                .with_max_subagents(0)
                .with_max_total_tokens(Some(req.spec.budget.clamped().max_tokens));

            let (summary, _child_logger) = runner.run(child_logger).await;
            timer_watcher.abort();
            let duration_ms = started.elapsed().as_millis() as u64;

            let (relevant_files, findings) =
                extract_relevant_files_and_findings(&summary.transcript, &summary.modified_files);
            let final_text = summary
                .final_assistant_text
                .clone()
                .unwrap_or_else(|| format!("subagent terminated with {:?}", summary.terminal_reason));
            let tokens_used = summary
                .total_input_tokens
                .saturating_add(summary.total_output_tokens);

            Ok(SubagentRunOutcome {
                status: summary.status,
                terminal_reason: summary.terminal_reason,
                summary: final_text,
                relevant_files,
                findings,
                turns_used: summary.turns_used,
                tool_calls_used: summary.tool_calls_used,
                tokens_used,
                duration_ms,
                visible_tools,
            })
        })
    }
}

fn extract_relevant_files_and_findings(
    transcript: &[TranscriptItem],
    modified_files: &[String],
) -> (Vec<String>, Vec<String>) {
    let mut files: BTreeSet<String> = modified_files.iter().cloned().collect();
    let mut findings: Vec<String> = Vec::new();
    for item in transcript {
        if let TranscriptItem::ToolResult(tr) = item {
            if let Some(ref rel) = tr.meta.relative_path {
                if rel != "." && !rel.is_empty() {
                    files.insert(rel.clone());
                }
            }
            if findings.len() < 8 {
                let first_line = tr
                    .content
                    .lines()
                    .find(|l| !l.trim().is_empty())
                    .unwrap_or("")
                    .trim();
                if !first_line.is_empty() {
                    let snippet: String = first_line.chars().take(120).collect();
                    findings.push(format!(
                        "{}({}) -> {}: {}",
                        tr.tool_name,
                        tr.meta.relative_path.as_deref().unwrap_or("-"),
                        if tr.outcome.is_success() { "ok" } else { "err" },
                        snippet
                    ));
                }
            }
        } else if let TranscriptItem::User(UserMessage { .. }) = item {
            // Skip user prompt
        }
    }
    (files.into_iter().collect(), findings)
}

/// Build Gate 4 [`MutatedFileSpan`] entries for files written or removed by a merged `Worker`
/// subagent so the parent's `MutationLedger` and `AttributionEngine` can attribute regressions to
/// the `spawn_subagent` call.
pub fn build_worker_mutation_spans(
    parent_workspace: &WorkspaceCap,
    subagent_id: &str,
    written_files: &[String],
    removed_files: &[String],
) -> Vec<MutatedFileSpan> {
    let mut spans = Vec::with_capacity(written_files.len() + removed_files.len());
    for path in written_files {
        let content = read_workspace_text(parent_workspace, path).ok();
        let line_count = content
            .as_deref()
            .map(|c| c.lines().count().max(1))
            .unwrap_or(1);
        let symbols = content
            .as_deref()
            .map(|c| top_level_symbol_names(c, 16))
            .unwrap_or_default();
        spans.push(MutatedFileSpan {
            relative_path: path.clone(),
            start_line: 1,
            end_line: line_count,
            added_symbols: symbols,
            removed_symbols: Vec::new(),
            summary: format!("spawn_subagent({}) wrote {}", subagent_id, path),
        });
    }
    for path in removed_files {
        spans.push(MutatedFileSpan {
            relative_path: path.clone(),
            start_line: 0,
            end_line: 0,
            added_symbols: Vec::new(),
            removed_symbols: Vec::new(),
            summary: format!("spawn_subagent({}) removed {}", subagent_id, path),
        });
    }
    spans
}
