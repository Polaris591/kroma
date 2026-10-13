//! Gate 5 — Controlled Subagents (`Scout` & `Worker`) and Isolated Worktrees.
//!
//! Four invariants govern this module:
//!
//! 1. **Role separation & read-only enforcement.** A `Scout` subagent runs with
//!    `PolicyMode::ReadOnly`, sees only the 7 read-only tools (`activate_skill`, `code_graph`,
//!    `find`, `grep`, `ls`, `read`, `recall_evidence`), and returns a bounded `<subagent_report>`
//!    without polluting the parent's transcript with intermediate tool outputs.
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
use crate::tool::policy::{
    NonInteractiveApprovalHandler, PolicyCheckResult, PolicyEngine, PolicyMode,
};
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

/// F01 (`v1.4.0`): the shared authorization + quota preflight for spawning subagents and
/// tournament candidates. Both the `spawn_subagent` tool path and the RPC `session.tournament`
/// path call this so every entry point enforces the same `max_subagents` quota and the same
/// `PolicyEngine` decision (a `ReadOnly` policy denies the `Mutating` spawn outright; `Ask` returns
/// `PolicyCheckResult::Ask` for the caller to resolve through its approval handler).
///
/// The `PolicyHook` gate (a registered [`crate::extensions::PolicyHook`]) is evaluated by the
/// caller's tool runtime before this function, because only the runtime holds the hook engine.
pub fn subagent_spawn_preflight(
    policy_engine: &PolicyEngine,
    requested: usize,
    already_spawned: usize,
    max_subagents: usize,
    effect: EffectClass,
) -> Result<PolicyCheckResult, String> {
    if max_subagents == 0 {
        return Err(
            "subagent spawning is disabled for this session (max_subagents=0)".to_string(),
        );
    }
    if already_spawned.saturating_add(requested) > max_subagents {
        return Err(format!(
            "subagent spawn budget exceeded: requested {}, already spawned {}, max_subagents={}",
            requested, already_spawned, max_subagents
        ));
    }
    Ok(policy_engine.evaluate("spawn_subagent", effect, false))
}

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
                    "mode": {
                        "type": "string",
                        "enum": ["fanout", "tournament"],
                        "description": "Set to 'tournament' to race N worker candidates in isolated worktrees and merge only the best verified one. Requires a top-level 'task'."
                    },
                    "tournament": {
                        "type": "boolean",
                        "description": "Shorthand for mode='tournament'."
                    },
                    "candidates": {
                        "type": "integer",
                        "description": "Number of racing candidates for a tournament (2-4, default 3)."
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
    /// Item #3 (`v1.4.0`): when `true`, a candidate/worker that modifies a default test/verification
    /// path (see [`default_test_protected_globs`]) is disqualified from winning/merging.
    #[serde(default)]
    pub disallow_test_tampering: bool,
    /// Item #3 (`v1.4.0`): additional explicit protected path globs (union with the default test
    /// paths when `disallow_test_tampering` is set).
    #[serde(default)]
    pub protected_paths: Vec<String>,
}

impl SubagentSpec {
    pub fn scout(task: impl Into<String>) -> Self {
        Self {
            role: SubagentRole::Scout,
            task: task.into(),
            allowed_write_globs: None,
            budget: SubagentBudget::default(),
            revert_on_verify_failure: false,
            disallow_test_tampering: false,
            protected_paths: Vec::new(),
        }
    }

    pub fn worker(task: impl Into<String>) -> Self {
        Self {
            role: SubagentRole::Worker,
            task: task.into(),
            allowed_write_globs: None,
            budget: SubagentBudget::default(),
            revert_on_verify_failure: false,
            disallow_test_tampering: false,
            protected_paths: Vec::new(),
        }
    }

    /// Item #3 (`v1.4.0`): enable default test/verification-path protection.
    pub fn with_disallow_test_tampering(mut self, value: bool) -> Self {
        self.disallow_test_tampering = value;
        self
    }

    /// Item #3 (`v1.4.0`): add explicit protected path globs.
    pub fn with_protected_paths(mut self, globs: Vec<String>) -> Self {
        self.protected_paths = globs;
        self
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
    /// Item #3 (`v1.4.0`): per-task test-path protection override.
    #[serde(default)]
    disallow_test_tampering: Option<bool>,
    /// Item #3 (`v1.4.0`): per-task explicit protected path globs.
    #[serde(default)]
    protected_paths: Option<Vec<String>>,
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
    /// `v1.3.0`: `"tournament"` runs a Best-of-N worktree race instead of a plain fan-out.
    #[serde(default)]
    mode: Option<String>,
    /// `v1.3.0`: shorthand for `"mode": "tournament"`.
    #[serde(default)]
    tournament: Option<bool>,
    /// `v1.3.0`: number of racing candidates (2..=4, default 3).
    #[serde(default)]
    candidates: Option<usize>,
    /// Item #3 (`v1.4.0`): protect default test/verification paths from tampering.
    #[serde(default)]
    disallow_test_tampering: Option<bool>,
    /// Item #3 (`v1.4.0`): explicit protected path globs.
    #[serde(default)]
    protected_paths: Option<Vec<String>>,
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
/// `v1.3.0`: a parsed `spawn_subagent` tournament request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TournamentRequest {
    pub task: String,
    pub candidates: usize,
    pub allowed_write_globs: Option<Vec<String>>,
    pub max_turns: Option<usize>,
    pub timeout_ms: u64,
    /// Item #3 (`v1.4.0`): protect default test/verification paths from tampering.
    pub disallow_test_tampering: bool,
    /// Item #3 (`v1.4.0`): explicit protected path globs.
    pub protected_paths: Vec<String>,
}

/// Detect a tournament request in `spawn_subagent` arguments.
///
/// Returns `Ok(None)` for a plain fan-out call. A tournament is requested either by
/// `"mode": "tournament"` or by `"tournament": true`.
pub fn parse_tournament_request(raw_args: &str) -> Result<Option<TournamentRequest>, String> {
    let parsed: RawSpawnSubagentArgs = serde_json::from_str(raw_args)
        .map_err(|e| format!("invalid JSON arguments for `spawn_subagent`: {}", e))?;

    let mode_is_tournament = parsed
        .mode
        .as_deref()
        .map(|m| m.trim().eq_ignore_ascii_case("tournament"))
        .unwrap_or(false);
    if !mode_is_tournament && parsed.tournament != Some(true) {
        return Ok(None);
    }

    let task = parsed
        .task
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .ok_or_else(|| {
            "a tournament requires a top-level `task` string describing the goal".to_string()
        })?
        .to_string();

    let candidates = parsed.candidates.unwrap_or(3);
    if !(MIN_TOURNAMENT_CANDIDATES..=MAX_TOURNAMENT_CANDIDATES).contains(&candidates) {
        return Err(format!(
            "`candidates` must be between {} and {} (got {})",
            MIN_TOURNAMENT_CANDIDATES, MAX_TOURNAMENT_CANDIDATES, candidates
        ));
    }

    let allowed_write_globs = normalize_and_validate_globs(
        parsed.allowed_paths.clone(),
        parsed.allowed_write_globs.clone(),
    )?;
    let timeout_ms = parsed.timeout_ms.unwrap_or(DEFAULT_SUBAGENT_TIMEOUT_MS);
    if timeout_ms == 0 {
        return Err("`timeout_ms` must be > 0".to_string());
    }

    Ok(Some(TournamentRequest {
        task,
        candidates,
        allowed_write_globs,
        max_turns: parsed.max_turns,
        timeout_ms,
        disallow_test_tampering: parsed.disallow_test_tampering.unwrap_or(false),
        protected_paths: parsed.protected_paths.clone().unwrap_or_default(),
    }))
}

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
                disallow_test_tampering: item
                    .disallow_test_tampering
                    .or(parsed.disallow_test_tampering)
                    .unwrap_or(false),
                protected_paths: item
                    .protected_paths
                    .clone()
                    .or_else(|| parsed.protected_paths.clone())
                    .unwrap_or_default(),
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
        disallow_test_tampering: parsed.disallow_test_tampering.unwrap_or(false),
        protected_paths: parsed.protected_paths.clone().unwrap_or_default(),
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

/// Item #3 (`v1.4.0`): the default set of test / verification path globs protected from tampering.
///
/// A candidate or worker that edits one of these without an explicit override is disqualified from
/// winning/merging, so a race can never "fix" a failing test by rewriting the test itself.
pub fn default_test_protected_globs() -> Vec<String> {
    vec![
        "**/tests/**".to_string(),
        "**/test/**".to_string(),
        "**/__tests__/**".to_string(),
        "**/spec/**".to_string(),
        "**/*_test.rs".to_string(),
        "**/*_test.go".to_string(),
        "**/test_*.py".to_string(),
        "**/*_test.py".to_string(),
        "**/*.test.ts".to_string(),
        "**/*.test.tsx".to_string(),
        "**/*.test.js".to_string(),
        "**/*.spec.ts".to_string(),
        "**/*.spec.js".to_string(),
        "**/conftest.py".to_string(),
    ]
}

/// Item #3 (`v1.4.0`): the effective protected-glob set for a worker/candidate. Explicit
/// `protected_paths` are always honoured; the default test paths are added when
/// `disallow_test_tampering` is set.
pub fn effective_protected_globs(
    disallow_test_tampering: bool,
    explicit: &[String],
) -> Vec<String> {
    let mut globs: Vec<String> = explicit.to_vec();
    if disallow_test_tampering {
        globs.extend(default_test_protected_globs());
    }
    globs.sort();
    globs.dedup();
    globs
}

/// Item #3 (`v1.4.0`): `true` when `rel_path` matches any protected glob.
pub fn path_is_protected(rel_path: &str, protected_globs: &[String]) -> bool {
    if protected_globs.is_empty() {
        return false;
    }
    is_path_allowed_by_scope(rel_path, protected_globs)
}

/// Item #3 (`v1.4.0`): the subset of `changed_paths` that touches a protected path.
pub fn protected_changed_paths(changed_paths: &[String], protected_globs: &[String]) -> Vec<String> {
    changed_paths
        .iter()
        .filter(|p| path_is_protected(p, protected_globs))
        .cloned()
        .collect()
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
        let delta = self.compute_merge_delta(parent_workspace, snapshot_store, allowed_write_globs)?;
        let MergeDelta {
            written_entries,
            mut removed_files,
            delta_paths,
            conflicting_paths,
            reasons,
        } = delta;

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

        // F03 (`v1.4.0`): durably record the merge intent *before* the first parent file is touched,
        // so an interrupted multi-file merge is detectable and recoverable instead of silently
        // leaving the parent workspace half-updated.
        snapshot_store.write_merge_intent(&crate::revision::MergeIntent {
            version: 1,
            pre_merge_revision: pre_merge_rev.clone(),
            created_at_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
            written_files: written_entries.keys().cloned().collect(),
            removed_files: removed_files.clone(),
        })?;

        // Apply removals deepest-first, then writes in sorted path order.
        removed_files.sort_by(|a, b| {
            let depth = |s: &String| s.matches('/').count();
            depth(b).cmp(&depth(a)).then_with(|| b.cmp(a))
        });

        let mut written_files: Vec<String> = Vec::with_capacity(written_entries.len());
        let apply_result: Result<(), RevisionError> = (|| {
            for path in &removed_files {
                remove_tracked_file(parent_workspace, path, false)?;
            }
            for (path, entry) in &written_entries {
                let bytes = snapshot_store.read_blob(&entry.sha256)?;
                write_tracked_file(parent_workspace, path, &bytes, entry.mode, false)?;
                written_files.push(path.clone());
            }
            Ok(())
        })();

        if let Err(apply_err) = apply_result {
            // F03 (`v1.4.0`): a mid-merge I/O failure must not leave the parent workspace partially
            // updated with no recovery material. Try an automatic rollback to the pre-merge
            // revision; a clean rollback clears the intent and returns a normal error, otherwise the
            // intent is left in place and `RecoveryRequired` is surfaced.
            match snapshot_store.restore_revision(parent_workspace, &pre_merge_rev, None, false) {
                Ok(crate::revision::RestoreResult::Applied { .. }) => {
                    // A rollback that leaves its journal behind would re-trigger recovery on the
                    // next startup, so a failed clear is surfaced rather than swallowed.
                    snapshot_store.clear_merge_intent()?;
                    return Err(apply_err);
                }
                _ => {
                    return Err(RevisionError::RecoveryRequired {
                        target_revision: pre_merge_rev,
                        journal_path: snapshot_store.merge_intent_path(),
                        reason: format!(
                            "worktree merge failed midway ({}) and automatic rollback did not complete; restore the parent workspace before continuing",
                            apply_err
                        ),
                    });
                }
            }
        }
        removed_files.sort();

        // F03: the candidate worktree is NOT cleaned up until the merged revision has been durably
        // captured, so a late failure still has the worktree available for recovery.
        let merged_manifest = snapshot_store.capture_revision(
            parent_workspace,
            Some(&pre_merge_rev),
            None,
            turn_index,
        )?;
        let merged_revision = merged_manifest.revision_id;
        // The merged revision is durably captured; the journal is cleared only now. A failure to
        // clear it is reported, and the next startup detects the completed merge (stale journal)
        // instead of rolling the successful merge back.
        snapshot_store.clear_merge_intent()?;

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
                // F03 (`v1.4.0`): the post-merge rollback (or the absence of one) is complete, so
                // the merge intent journal can be cleared.
                snapshot_store.clear_merge_intent()?;
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

        let _ = self.cleanup();

        Ok(WorktreeMergeResult::Applied {
            base_revision: pre_merge_rev,
            merged_revision,
            written_files,
            removed_files,
        })
    }
}

/// F03/F06 (`v1.4.0`): the three-way merge delta computed against the live parent workspace,
/// without mutating anything. Split out so both the non-mutating pre-merge conflict check and the
/// actual [`IsolatedWorktree::merge_into_parent`] share exactly one conflict-detection code path.
struct MergeDelta {
    written_entries: BTreeMap<String, TrackedFile>,
    removed_files: Vec<String>,
    delta_paths: BTreeSet<String>,
    conflicting_paths: BTreeSet<String>,
    reasons: Vec<String>,
}

impl IsolatedWorktree {
    /// F06 (`v1.4.0`): compute the worker's delta against the live parent workspace and report
    /// whether it merges without conflict, WITHOUT touching the parent workspace or the worktree.
    pub fn preflight_conflict_check(
        &self,
        parent_workspace: &WorkspaceCap,
        snapshot_store: &SnapshotStore,
        allowed_write_globs: Option<&[String]>,
    ) -> Result<bool, RevisionError> {
        let delta = self.compute_merge_delta(parent_workspace, snapshot_store, allowed_write_globs)?;
        Ok(delta.conflicting_paths.is_empty())
    }

    /// Item #3 (`v1.4.0`): the workspace-relative paths this worker changed relative to its base
    /// revision (written ∪ removed), without touching the parent workspace.
    pub fn delta_paths(
        &self,
        parent_workspace: &WorkspaceCap,
        snapshot_store: &SnapshotStore,
    ) -> Result<Vec<String>, RevisionError> {
        let delta = self.compute_merge_delta(parent_workspace, snapshot_store, None)?;
        Ok(delta.delta_paths.into_iter().collect())
    }

    /// Compute the worker's three-way delta against the live parent workspace. Pure read-only: it
    /// never writes to the parent workspace, so it is safe to call for ranking before any merge.
    fn compute_merge_delta(
        &self,
        parent_workspace: &WorkspaceCap,
        snapshot_store: &SnapshotStore,
        allowed_write_globs: Option<&[String]>,
    ) -> Result<MergeDelta, RevisionError> {
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
                        let anc_str = cur_anc.to_string_lossy().replace('\\', "/");
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
        }        Ok(MergeDelta {
            written_entries,
            removed_files,
            delta_paths,
            conflicting_paths,
            reasons,
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
    /// F10 (`v1.4.0`): total tokens (input + output) spent by this child run.
    pub tokens_used: u64,
    /// F10 (`v1.4.0`): input tokens spent by this child run, so the parent can attribute cost.
    pub input_tokens: u64,
    /// F10 (`v1.4.0`): output tokens spent by this child run, so the parent can attribute cost.
    pub output_tokens: u64,
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

            // F01 (`v1.4.0`): a `Worker` mutates an isolated worktree that is later merged into the
            // parent workspace, so it must NEVER be granted more authority than its parent. The
            // previous mapping silently promoted a `ReadOnly` parent to `AllowWrite`, letting a
            // read-only session write (and merge) files through RPC/`/race`. A `ReadOnly` parent now
            // rejects any `Worker` outright.
            let policy_mode = match req.spec.role {
                SubagentRole::Scout => PolicyMode::ReadOnly,
                SubagentRole::Worker => {
                    if req.parent_policy_mode == PolicyMode::ReadOnly {
                        return Err(
                            "policy denied: cannot spawn a `worker` subagent while the parent policy is read-only"
                                .to_string(),
                        );
                    }
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
                    // F6: this is the worker's *only* write-scope guardrail. A silent failure here
                    // would let the worker write anywhere, so a registration failure is propagated
                    // (as a policy denial) instead of being discarded.
                    registry.register_extension(ext).map_err(|e| {
                        format!(
                            "policy denied: failed to install the worker write-scope guard for allowed_write_globs {:?}: {}",
                            globs, e
                        )
                    })?;
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
                    "\n\n[SUBAGENT ROLE: scout — Strictly read-only exploration subagent. You may only use read-only tools (`code_graph`, `read`, `grep`, `find`, `ls`, `activate_skill`, `recall_evidence`). `spawn_subagent`, `edit`, `write`, and `bash` are disabled. Start with `code_graph` to locate the relevant call chain, then `read` only the files it ranks. Return a concise factual summary with relevant file paths and line numbers.]"
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
                input_tokens: summary.total_input_tokens,
                output_tokens: summary.total_output_tokens,
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

// ---------------------------------------------------------------------------------------------
// v1.3.0 — Best-of-N Worktree Tournament
// ---------------------------------------------------------------------------------------------

/// Minimum number of candidates a tournament may run.
pub const MIN_TOURNAMENT_CANDIDATES: usize = 2;
/// Maximum number of candidates a tournament may run (bounded by parallel subagent capacity).
pub const MAX_TOURNAMENT_CANDIDATES: usize = MAX_PARALLEL_SUBAGENTS;

/// One competing `Worker` candidate in a tournament.
///
/// Every candidate starts from the same parent `base_revision` in its own isolated worktree, so the
/// candidates cannot observe or corrupt each other.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TournamentCandidate {
    /// Short stable label used in the `<subagent_tournament_report>` (for example `c1`).
    pub label: String,
    /// Free-form strategy hint recorded in the report so the parent model can see what was tried.
    #[serde(default)]
    pub strategy: Option<String>,
    /// The `Worker` specification. `role` must be `Worker`; a `Scout` candidate is rejected.
    pub spec: SubagentSpec,
}

impl TournamentCandidate {
    pub fn worker(label: impl Into<String>, task: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            strategy: None,
            spec: SubagentSpec::worker(task),
        }
    }

    pub fn with_strategy(mut self, strategy: impl Into<String>) -> Self {
        self.strategy = Some(strategy.into());
        self
    }
}

/// Inputs for [`run_worktree_tournament`].
pub struct TournamentSpec {
    pub candidates: Vec<TournamentCandidate>,
    /// Base directory that holds the `wt-race-000N` worktrees.
    pub worktree_base: PathBuf,
    /// Base directory that holds per-candidate child state.
    pub state_base: PathBuf,
    pub allowed_write_globs: Option<Vec<String>>,
    pub budget: BudgetConfig,
    pub policy_mode: PolicyMode,
    pub workspace_skills: Vec<SkillEntry>,
    pub predictive_check: bool,
    pub cancel_rx: watch::Receiver<bool>,
    pub timeout_ms: u64,
    /// Item #3 (`v1.4.0`): protect default test/verification paths from tampering.
    pub disallow_test_tampering: bool,
    /// Item #3 (`v1.4.0`): explicit protected path globs (union with the defaults when
    /// `disallow_test_tampering` is set).
    pub protected_paths: Vec<String>,
}

/// F05 (`v1.4.0`): explicit verification status for a tournament candidate, so a *skipped*
/// verification is never silently treated as a *passed* one when choosing a winner.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateVerificationStatus {
    /// The candidate's own worktree passed the completion gate (`Approved`).
    Passed,
    /// Verification was skipped (no gate configured / nothing to verify).
    Skipped,
    /// The candidate's own worktree failed the completion gate.
    Failed,
    /// The verification command itself errored (infrastructure, not the candidate's patch).
    InfrastructureError,
    /// The candidate never reached verification (did not complete).
    NotRun,
}

impl GateVerificationStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            GateVerificationStatus::Passed => "passed",
            GateVerificationStatus::Skipped => "skipped",
            GateVerificationStatus::Failed => "failed",
            GateVerificationStatus::InfrastructureError => "infrastructure_error",
            GateVerificationStatus::NotRun => "not_run",
        }
    }

    fn not_run() -> Self {
        GateVerificationStatus::NotRun
    }
}

/// Deterministic per-candidate score. Ranking is lexicographic over these fields, in the order
/// documented on [`TournamentOutcome::scores`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TournamentScore {
    pub label: String,
    pub candidate_index: usize,
    /// Child loop status (`completed`, `failed`, `cancelled`, `budget_exhausted`).
    pub status: String,
    /// `true` when the candidate's own isolated worktree passed the completion gate (`Approved`).
    /// A `Skipped` gate (verification never ran) leaves this `false`; read [`Self::gate_status`] to
    /// tell a skipped verification apart from a failed one. Do not infer "verified" from this
    /// boolean alone.
    pub gate_passed: bool,
    /// F05 (`v1.4.0`): explicit verification status (never inferred from `gate_passed` alone).
    #[serde(default = "GateVerificationStatus::not_run")]
    pub gate_status: GateVerificationStatus,
    /// F05 (`v1.4.0`): `true` when the candidate actually changed at least one file. A zero-change
    /// candidate is never eligible to win a repair tournament, regardless of its (skipped) gate.
    #[serde(default)]
    pub has_changes: bool,
    /// Human-readable gate detail (verification output excerpt, skip reason, or the reason the
    /// candidate never reached verification).
    pub gate_detail: String,
    /// Number of structural (syntax/delimiter) warnings reported by [`PredictiveValidator`].
    pub predictive_warnings: usize,
    /// `true` when the candidate's delta merges cleanly against the live parent workspace.
    pub conflict_free_with_parent: bool,
    pub files_modified: usize,
    /// Approximate changed-line count (lines added + lines removed) — Occam's razor tie-breaker.
    pub total_changed_lines: usize,
    pub turns_used: usize,
    /// F10 (`v1.4.0`): input tokens spent by this candidate (kept for cost attribution).
    #[serde(default)]
    pub input_tokens: u64,
    /// F10 (`v1.4.0`): output tokens spent by this candidate (kept for cost attribution).
    #[serde(default)]
    pub output_tokens: u64,
    /// Set on exactly one candidate when a merge actually happened.
    pub winner: bool,
}

impl TournamentScore {
    /// `true` when the candidate's gate did not reject it: it actually passed verification, or
    /// verification was intentionally skipped (no gate configured). A `Skipped` gate is a legitimate
    /// "cannot verify" outcome the tournament accepts, but it is *not* a pass — read `gate_status`
    /// when the distinction matters.
    fn gate_accepted(&self) -> bool {
        self.gate_passed || self.gate_status == GateVerificationStatus::Skipped
    }

    /// F05 (`v1.4.0`): only a candidate that actually changed files *and* whose gate accepted it
    /// (passed, or verification was skipped) is eligible to win and merge. A zero-change candidate
    /// must never out-rank a real passing fix.
    pub fn eligible_to_win(&self) -> bool {
        self.has_changes && self.gate_accepted()
    }

    /// Lexicographic ranking key. Lower sorts better. Ineligible candidates sort last.
    fn rank_key(&self) -> (u8, u8, usize, u8, usize, usize, usize, usize) {
        (
            u8::from(!self.eligible_to_win()),
            u8::from(!self.gate_accepted()),
            self.predictive_warnings,
            u8::from(!self.conflict_free_with_parent),
            self.total_changed_lines,
            self.files_modified,
            self.turns_used,
            self.candidate_index,
        )
    }
}

/// Result of a completed tournament.
#[derive(Debug, Clone)]
pub struct TournamentOutcome {
    /// Per-candidate scores, ordered by candidate index (not by rank).
    pub scores: Vec<TournamentScore>,
    /// Index into `scores` of the merged candidate, if one was merged.
    pub winner: Option<usize>,
    pub merged_revision: Option<RevisionId>,
    pub merge: Option<WorktreeMergeResult>,
    /// `true` when every candidate worktree directory was removed from disk.
    pub all_worktrees_cleaned_up: bool,
    /// F10 (`v1.4.0`): total input tokens spent by every candidate (including losers, failures and
    /// cancellations), so the parent run can attribute the whole tournament's cost.
    pub total_input_tokens: u64,
    /// F10 (`v1.4.0`): total output tokens spent by every candidate.
    pub total_output_tokens: u64,
}

impl TournamentOutcome {
    /// `true` when the parent workspace was left byte-for-byte untouched.
    pub fn parent_untouched(&self) -> bool {
        self.merged_revision.is_none()
    }

    /// Render the sanitized `<subagent_tournament_report>` block for the parent transcript.
    pub fn render_report(&self) -> String {
        let mut out = String::new();
        out.push_str("<subagent_tournament_report>\n");
        out.push_str(&format!("candidates: {}\n", self.scores.len()));
        out.push_str(&format!(
            "merged_revision: {}\n",
            self.merged_revision
                .as_ref()
                .map(|r| r.as_str().to_string())
                .unwrap_or_else(|| "<none>".to_string())
        ));
        out.push_str(&format!(
            "all_worktrees_cleaned_up: {}\n",
            self.all_worktrees_cleaned_up
        ));
        if self.winner.is_none() {
            out.push_str(
                "outcome: no candidate passed verification; the parent workspace was not modified\n",
            );
        }
        for score in &self.scores {
            out.push_str(&format!(
                "candidate {} [{}]: gate_passed={} gate_status={} has_changes={} warnings={} conflict_free={} changed_lines={} files_modified={} turns_used={} winner={}\n",
                score.label,
                score.status,
                score.gate_passed,
                score.gate_status.as_str(),
                score.has_changes,
                score.predictive_warnings,
                score.conflict_free_with_parent,
                score.total_changed_lines,
                score.files_modified,
                score.turns_used,
                score.winner,
            ));
            if !score.gate_detail.is_empty() {
                out.push_str(&format!(
                    "  detail: {}\n",
                    escape_fence_attribute(&score.gate_detail)
                ));
            }
        }
        out.push_str("</subagent_tournament_report>");
        out
    }
}

/// Approximate changed-line count between two byte blobs.
///
/// Uses a line multiset difference (deterministic and O(n)) rather than an LCS, which is enough to
/// implement Occam's razor between candidates without ever making the ranking non-deterministic.
fn approximate_changed_lines(base: &[u8], updated: &[u8]) -> usize {
    if base == updated {
        return 0;
    }
    let to_lines = |bytes: &[u8]| -> Vec<Vec<u8>> {
        let text = String::from_utf8_lossy(bytes);
        text.lines().map(|l| l.as_bytes().to_vec()).collect()
    };
    let mut base_counts: BTreeMap<Vec<u8>, usize> = BTreeMap::new();
    for line in to_lines(base) {
        *base_counts.entry(line).or_insert(0) += 1;
    }
    let mut removed = 0usize;
    let mut new_lines: Vec<Vec<u8>> = Vec::new();
    for line in to_lines(updated) {
        match base_counts.get_mut(&line) {
            Some(count) if *count > 0 => *count -= 1,
            _ => new_lines.push(line),
        }
    }
    removed += base_counts.values().sum::<usize>();
    removed + new_lines.len()
}

/// Run a Best-of-N worktree tournament.
///
/// Every candidate runs in parallel in its own isolated worktree, is verified **inside** that
/// worktree (completion gate + predictive validation), and is then ranked. Only the single best
/// candidate that passed verification is merged into `parent_workspace`; every loser worktree is
/// removed. When no candidate passes verification the parent workspace is not modified at all.
#[allow(clippy::too_many_arguments)]
pub async fn run_worktree_tournament(
    spec: TournamentSpec,
    parent_workspace: &WorkspaceCap,
    snapshot_store: &SnapshotStore,
    backend: Arc<dyn SubagentBackend>,
    parent_revision: Option<&RevisionId>,
    completion_gate: &SharedCompletionGate,
) -> Result<TournamentOutcome, String> {
    if spec.candidates.len() < MIN_TOURNAMENT_CANDIDATES
        || spec.candidates.len() > MAX_TOURNAMENT_CANDIDATES
    {
        return Err(format!(
            "a tournament needs between {} and {} candidates (got {})",
            MIN_TOURNAMENT_CANDIDATES,
            MAX_TOURNAMENT_CANDIDATES,
            spec.candidates.len()
        ));
    }
    for candidate in &spec.candidates {
        if candidate.spec.role != SubagentRole::Worker {
            return Err(format!(
                "tournament candidate '{}' must be role=worker (got {})",
                candidate.label, candidate.spec.role
            ));
        }
    }

    // Materialize every worktree from the same baseline before any candidate starts.
    let mut worktrees: Vec<(String, IsolatedWorktree)> = Vec::with_capacity(spec.candidates.len());
    for (index, candidate) in spec.candidates.iter().enumerate() {
        let wt_dir = spec
            .worktree_base
            .join(format!("wt-race-{:04}", index + 1));
        let wt = IsolatedWorktree::materialize(
            format!("race-{:04}-{}", index + 1, candidate.label),
            parent_workspace,
            snapshot_store,
            parent_revision,
            wt_dir,
        )
        .map_err(|e| format!("failed to materialize tournament worktree: {}", e))?;
        worktrees.push((candidate.label.clone(), wt));
    }

    let mut handles = Vec::with_capacity(spec.candidates.len());
    for (index, candidate) in spec.candidates.iter().enumerate() {
        let child_workspace = worktrees[index].1.workspace().clone();
        let backend = Arc::clone(&backend);
        let mut parent_cancel = spec.cancel_rx.clone();
        let timeout_ms = spec.timeout_ms;
        let req_spec = candidate.spec.clone();
        let req_id = format!("race-{:04}-{}", index + 1, candidate.label);
        let child_state_dir = spec.state_base.join(&req_id);
        let skills = spec.workspace_skills.clone();
        let budget = spec.budget.clone();
        let policy_mode = spec.policy_mode;
        let predictive_check = spec.predictive_check;

        handles.push(tokio::spawn(async move {
            let (child_cancel_tx, child_cancel_rx) = watch::channel(*parent_cancel.borrow());
            let cancel_tx_for_timer = child_cancel_tx.clone();
            let timer = tokio::spawn(async move {
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_millis(timeout_ms)) => {
                        let _ = cancel_tx_for_timer.send(true);
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
                        let _ = cancel_tx_for_timer.send(true);
                    }
                }
            });

            let req = SubagentExecutionRequest {
                subagent_id: req_id,
                spec: req_spec,
                workspace: child_workspace,
                state_dir: child_state_dir,
                workspace_skills: skills,
                parent_policy_mode: policy_mode,
                parent_budget: budget,
                predictive_check,
                cancel_rx: child_cancel_rx,
            };
            let hard_deadline = Duration::from_millis(timeout_ms.saturating_add(150));
            let outcome = tokio::select! {
                res = backend.run_subagent(req) => res,
                _ = tokio::time::sleep(hard_deadline) => {
                    let _ = child_cancel_tx.send(true);
                    Ok(SubagentRunOutcome {
                        status: RunStatus::Cancelled,
                        terminal_reason: TerminalReason::RunDeadlineExceeded,
                        summary: "tournament candidate timed out and was cancelled".to_string(),
                        relevant_files: Vec::new(),
                        findings: Vec::new(),
                        turns_used: 0,
                        tool_calls_used: 0,
                        tokens_used: 0,
                        input_tokens: 0,
                        output_tokens: 0,
                        duration_ms: 0,
                        visible_tools: Vec::new(),
                    })
                }
            };
            timer.abort();
            outcome
        }));
    }

    // Item #3 (`v1.4.0`): the effective protected test/verification glob set for this tournament.
    let protected_globs =
        effective_protected_globs(spec.disallow_test_tampering, &spec.protected_paths);

    let mut scores: Vec<TournamentScore> = Vec::with_capacity(spec.candidates.len());
    let mut join_failures: Vec<String> = Vec::new();
    for (index, handle) in handles.into_iter().enumerate() {
        let candidate = &spec.candidates[index];
        let base_revision = worktrees[index].1.base_revision().clone();
        let worktree = &worktrees[index].1;

        let outcome = match handle.await {
            Ok(Ok(o)) => o,
            Ok(Err(err)) => {
                join_failures.push(format!("{}: {}", candidate.label, err));
                SubagentRunOutcome {
                    status: RunStatus::Failed,
                    terminal_reason: TerminalReason::ProviderProtocolError(err),
                    summary: "tournament candidate failed".to_string(),
                    relevant_files: Vec::new(),
                    findings: Vec::new(),
                    turns_used: 0,
                    tool_calls_used: 0,
                    tokens_used: 0,
                    input_tokens: 0,
                    output_tokens: 0,
                    duration_ms: 0,
                    visible_tools: Vec::new(),
                }
            }
            Err(join_err) => {
                join_failures.push(format!("{}: {}", candidate.label, join_err));
                SubagentRunOutcome {
                    status: RunStatus::Cancelled,
                    terminal_reason: TerminalReason::UserCancelled,
                    summary: "tournament candidate join error".to_string(),
                    relevant_files: Vec::new(),
                    findings: Vec::new(),
                    turns_used: 0,
                    tool_calls_used: 0,
                    tokens_used: 0,
                    input_tokens: 0,
                    output_tokens: 0,
                    duration_ms: 0,
                    visible_tools: Vec::new(),
                }
            }
        };

        let status_str = match outcome.status {
            RunStatus::Completed => "completed",
            RunStatus::Cancelled => "cancelled",
            RunStatus::BudgetExhausted => "budget_exhausted",
            _ => "failed",
        };

        // F10 (`v1.4.0`): capture this candidate's token spend before the outcome is consumed.
        let candidate_input_tokens = outcome.input_tokens;
        let candidate_output_tokens = outcome.output_tokens;

        // Compute this candidate's delta against its own baseline revision.
        let mut changed_paths: Vec<String> = Vec::new();
        let mut total_changed_lines = 0usize;
        let mut predictive_warnings = 0usize;
        let mut gate_passed = false;
        let mut gate_status = GateVerificationStatus::NotRun;
        let gate_detail: String;

        if outcome.status != RunStatus::Completed {
            gate_detail = format!("candidate did not complete ({})", status_str);
        } else {
            let scan = snapshot_store
                .scan_workspace_to_blobs(worktree.workspace())
                .map_err(|e| format!("failed to scan tournament worktree: {}", e))?;
            let base_files = &base_revision.tracked_files;
            for (path, entry) in &scan.0 {
                if base_files.get(path) == Some(entry) {
                    continue;
                }
                changed_paths.push(path.clone());
                let base_bytes = base_files
                    .get(path)
                    .map(|b| snapshot_store.read_blob(&b.sha256))
                    .transpose()
                    .map_err(|e| format!("failed to read base blob: {}", e))?
                    .unwrap_or_default();
                let new_bytes = snapshot_store
                    .read_blob(&entry.sha256)
                    .map_err(|e| format!("failed to read candidate blob: {}", e))?;
                total_changed_lines += approximate_changed_lines(&base_bytes, &new_bytes);
            }
            for path in base_files.keys() {
                if !scan.0.contains_key(path) {
                    changed_paths.push(path.clone());
                    if let Some(base_entry) = base_files.get(path) {
                        let base_bytes = snapshot_store
                            .read_blob(&base_entry.sha256)
                            .map_err(|e| format!("failed to read base blob: {}", e))?;
                        total_changed_lines += approximate_changed_lines(&base_bytes, &[]);
                    }
                }
            }
            changed_paths.sort();
            changed_paths.dedup();

            let tampered = protected_changed_paths(&changed_paths, &protected_globs);
            if changed_paths.is_empty() {
                // F05 (`v1.4.0`): a zero-change candidate has *nothing verified*. It is recorded as
                // `Skipped` (not `Passed`) and `has_changes = false`, so `eligible_to_win()` rejects
                // it and a real passing fix can never lose to an empty diff.
                gate_status = GateVerificationStatus::Skipped;
                gate_detail = "no changes; nothing to verify".to_string();
            } else if !tampered.is_empty() {
                // Item #3 (`v1.4.0`): a candidate that rewrote the verification/test surface is
                // disqualified — it can never "pass" by editing the check itself.
                gate_passed = false;
                gate_status = GateVerificationStatus::Failed;
                gate_detail = format!(
                    "tampered with protected verification/test path: {}",
                    tampered.join(", ")
                );
            } else if *spec.cancel_rx.borrow() {
                // F02 (`v1.4.0`): cancellation observed *before* verification must not fall through
                // to a merge.
                gate_detail = "cancelled before verification".to_string();
            } else {
                // Predictive validation runs on the candidate's own worktree copy.
                if spec.predictive_check {
                    let validator = crate::attribution::PredictiveValidator::default();
                    for path in &changed_paths {
                        let report = validator.validate_file(worktree.workspace(), path);
                        predictive_warnings += report.issues.len();
                    }
                }

                // Verification runs INSIDE the isolated worktree, before the parent is touched.
                let gate_ctx = CompletionGateContext {
                    run_id: RunId::new(),
                    workspace_root: worktree.root().to_path_buf(),
                    modified_files: changed_paths.clone(),
                    turns_used: outcome.turns_used,
                };
                match completion_gate.check(&gate_ctx).await {
                    Ok(GateOutcome::Approved) => {
                        gate_passed = true;
                        gate_status = GateVerificationStatus::Passed;
                        gate_detail = "verification passed".to_string();
                    }
                    Ok(GateOutcome::Skipped { reason }) => {
                        // Verification never ran: record it as such and leave `gate_passed` false, so
                        // a consumer reading only the boolean cannot mistake "not verified" for
                        // "verified". `gate_accepted()` keeps the tournament's merge eligibility.
                        gate_status = GateVerificationStatus::Skipped;
                        gate_detail = format!("verification skipped: {}", reason);
                    }
                    Ok(GateOutcome::Rejected { feedback }) => {
                        gate_status = GateVerificationStatus::Failed;
                        gate_detail = format!("verification failed: {}", feedback);
                    }
                    Err(e) => {
                        gate_status = GateVerificationStatus::InfrastructureError;
                        gate_detail = format!("verification error: {}", e);
                    }
                }
            }
        }

        let has_changes = !changed_paths.is_empty();
        scores.push(TournamentScore {
            label: candidate.label.clone(),
            candidate_index: index,
            status: status_str.to_string(),
            gate_passed,
            gate_status,
            has_changes,
            gate_detail,
            predictive_warnings,
            // Filled in below once the pre-merge conflict check runs against the live parent.
            conflict_free_with_parent: false,
            files_modified: changed_paths.len(),
            total_changed_lines,
            turns_used: outcome.turns_used,
            input_tokens: candidate_input_tokens,
            output_tokens: candidate_output_tokens,
            winner: false,
        });
    }

    // F10 (`v1.4.0`): aggregate the token spend of *every* candidate, including losers, failures
    // and cancellations, so the parent run can account for the whole tournament.
    let total_input_tokens = scores.iter().map(|s| s.input_tokens).sum::<u64>();
    let total_output_tokens = scores.iter().map(|s| s.output_tokens).sum::<u64>();

    // F06 (`v1.4.0`): run the *real* three-way conflict pre-check against the live parent workspace
    // BEFORE ranking, so `conflict_free_with_parent` reflects reality instead of being hardcoded
    // `false`.
    {
        let allowed = spec.allowed_write_globs.as_deref();
        for (index, score) in scores.iter_mut().enumerate() {
            if !score.has_changes || !score.gate_passed {
                continue;
            }
            let worktree = &worktrees[index].1;
            match worktree.preflight_conflict_check(
                parent_workspace,
                snapshot_store,
                allowed,
            ) {
                Ok(conflict_free) => score.conflict_free_with_parent = conflict_free,
                Err(_) => score.conflict_free_with_parent = false,
            }
        }
    }

    // Rank: eligible candidates first, then gate, then zero predictive warnings, then
    // conflict-freedom, then minimal diff.
    let mut order: Vec<usize> = (0..scores.len()).collect();
    order.sort_by_key(|&i| scores[i].rank_key());

    let mut merged_revision: Option<RevisionId> = None;
    let mut merge_result: Option<WorktreeMergeResult> = None;
    let mut winner: Option<usize> = None;

    // F02/F06/F07 (`v1.4.0`): try every eligible candidate in ranked order until one merges AND
    // passes the post-merge gate on the *actual* merged parent workspace. A rank-#1 conflict or a
    // post-merge verification failure (with automatic rollback) falls back to the next candidate.
    for &index in &order {
        if !scores[index].eligible_to_win() {
            continue;
        }
        // F02 (`v1.4.0`): never merge after the run has been cancelled.
        if *spec.cancel_rx.borrow() {
            break;
        }
        let (label, worktree) = &mut worktrees[index];
        let allowed = spec.allowed_write_globs.as_deref();
        let merge = worktree
            .merge_into_parent(
                parent_workspace,
                snapshot_store,
                parent_revision,
                allowed,
                Some(completion_gate),
                true,
                scores[index].turns_used,
            )
            .await
            .map_err(|e| format!("tournament merge failed for '{}': {}", label, e))?;
        // F7: a tournament removes *every* worktree from disk once the race is decided, so a
        // conflicting candidate cannot keep the worktree that `merge_into_parent` preserves for the
        // single-worker path. Clear the preserve flag and report the truth instead of advertising a
        // `preserved_worktree_path` that the cleanup loop below is about to delete.
        let merge = match merge {
            WorktreeMergeResult::Conflict {
                base_revision,
                conflicting_paths,
                reasons,
                ..
            } => {
                worktree.preserve_on_drop = false;
                WorktreeMergeResult::Conflict {
                    base_revision,
                    conflicting_paths,
                    reasons,
                    worker_worktree_preserved: false,
                    preserved_worktree_path: None,
                }
            }
            other => other,
        };
        match &merge {
            WorktreeMergeResult::Applied {
                merged_revision: rev,
                ..
            } => {
                scores[index].conflict_free_with_parent = true;
                scores[index].winner = true;
                merged_revision = Some(rev.clone());
                winner = Some(index);
                merge_result = Some(merge);
                break;
            }
            WorktreeMergeResult::Conflict {
                conflicting_paths, ..
            } => {
                scores[index].conflict_free_with_parent = false;
                scores[index].gate_detail = format!(
                    "{} (merge conflict on: {})",
                    scores[index].gate_detail,
                    conflicting_paths.join(", ")
                );
                merge_result = Some(merge);
            }
            WorktreeMergeResult::VerificationFailed {
                reverted,
                gate_output,
                ..
            } => {
                // F07 (`v1.4.0`): the candidate's own worktree passed, but the *combined* parent
                // state failed post-merge verification. It has been rolled back; try the next one.
                scores[index].gate_status = GateVerificationStatus::Failed;
                scores[index].gate_detail = format!(
                    "{} (post-merge verification failed, reverted={}: {})",
                    scores[index].gate_detail,
                    reverted,
                    gate_output
                );
                merge_result = Some(merge);
            }
        }
    }

    // Every worktree, winner included, is removed from disk.
    let mut all_cleaned = true;
    for (_, worktree) in worktrees.iter_mut() {
        if worktree.cleanup().is_err() {
            all_cleaned = false;
        }
    }

    if let Some(index) = winner {
        let _ = join_failures; // failures are already reflected in per-candidate status
        let _ = index;
    }

    Ok(TournamentOutcome {
        scores,
        winner,
        merged_revision,
        merge: merge_result,
        all_worktrees_cleaned_up: all_cleaned,
        total_input_tokens,
        total_output_tokens,
    })
}
