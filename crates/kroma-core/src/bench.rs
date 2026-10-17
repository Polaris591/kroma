//! Item #3 (`v1.4.0`) — Paired, budget-locked A/B benchmark runner (`kroma bench`).
//!
//! Astra's third recommendation was to stop asserting that a mechanism helps and start *measuring*
//! it. This module provides the measurement plumbing:
//!
//! * [`BenchTask`] describes one self-contained task: the initial workspace files, the prompt, and
//!   the post-conditions (plus an optional held-out `verify_command`) that decide whether the task
//!   was resolved.
//! * [`BenchStrategyConfig`] names one arm of the comparison (for example `single_agent` vs
//!   `adaptive_tournament`, or `baseline` vs `p2` projection profiles).
//! * [`BenchExecutor`] runs one arm on one task. Every arm receives the **same**
//!   `budget_tokens` total, so a comparison is a paired, budget-locked measurement rather than
//!   "the arm that spent more won".
//! * [`BenchComparisonReport`] aggregates the per-arm results into pass rate, total tokens,
//!   latency, a Wilson interval per arm, a paired bootstrap interval for each non-baseline arm and
//!   an explicit point-estimate winner.
//!
//! ## Two executors, two different claims
//!
//! * [`SyntheticBenchExecutor`] is **deterministic and synthetic**: it exercises the plumbing
//!   (budget locking, token accounting, report aggregation, paired comparison, path validation) on
//!   a machine with no model endpoint. It fabricates the fix for tasks whose id starts with
//!   `fixable-`, so its numbers can never be evidence that a mechanism improves real task
//!   resolution. The report labels it [`SYNTHETIC_EXECUTOR_LABEL`] and prints the caveat.
//! * [`ProviderBenchExecutor`] drives the real [`crate::agent_loop::AgentRunner`] against a real
//!   [`ModelProvider`] in a per-arm isolated workspace. It never writes the expected answer itself:
//!   the model has to produce it, and the harness then evaluates the post-conditions and the
//!   held-out `verify_command` *independently* of what the model claimed. Its report is labelled
//!   [`PROVIDER_EXECUTOR_LABEL`] and records the model id, wire protocol, git commit, prompt hash,
//!   tool-schema hash and the real self/child token split.
//!
//! ## Scorer boundary (`B09`, extended by `B13`)
//!
//! The verdict is a *workspace-bound* static check: post-conditions are read through a
//! [`WorkspaceCap`] (no symlink component, no special file, bounded read) and the optional
//! `verify_command` runs under [`ProcessSupervisor`]. Static checks alone are not OS isolation. A
//! held-out verification command that executes a script inside the agent-writable workspace can
//! still be edited by the agent, and the harness does not (and cannot, on the same uid) claim to
//! resist an active same-uid attacker.
//!
//! [`Self::evaluate_with_sandbox`] closes that gap for hosts with a sandbox backend: the same
//! `verify_command` runs inside an OS sandbox (bubblewrap or a container runtime — see
//! [`crate::sandbox`]) whose only writable view is the scored workspace, whose network is off by
//! default, and whose host view is a read-only whitelist. It is opt-in: without a sandbox the
//! behaviour is exactly the B07 contract above. Fail-closed: an unavailable backend fails the
//! check and never degrades to a host run. The held-out suite itself still belongs outside the
//! agent's workspace (an external scorer location); that placement is the caller's job (B17).
//!
//! ## Small samples
//!
//! Repetitions + randomized arm order + an interval estimate are the minimum needed to keep a
//! paired comparison honest, but they are not a substitute for a large held-out suite. A handful of
//! tasks can show a large regression; it cannot establish a capability improvement. [`render`]
//! prints that caveat explicitly rather than declaring a winner "better".
//!
//! [`render`]: BenchComparisonReport::render

use crate::agent_loop::{build_default_runtime, AgentRunConfig, AgentRunner};
use crate::budget::BudgetConfig;
use crate::context_projector::ProjectionProfile;
use crate::env::LocalHostEnv;
use crate::events::EventLogger;
use crate::process_supervisor::{EnvironmentPolicy, OneShotCommandSpec, ProcessSupervisor};
use crate::prompt::PromptBuilder;
use crate::provider::ModelProvider;
use crate::tool::policy::{NonInteractiveApprovalHandler, PolicyEngine, PolicyMode};
use crate::tool::registry::ToolRegistry;
use crate::workspace::{WorkspaceCap, WorkspaceError};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::future::Future;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Marker placed in every synthetic report so its numbers can never be mistaken for real evidence.
pub const SYNTHETIC_EXECUTOR_LABEL: &str = "synthetic";

/// Marker placed in every provider-backed report.
pub const PROVIDER_EXECUTOR_LABEL: &str = "provider";

/// Default wall-clock ceiling for one held-out `verify_command`.
pub const DEFAULT_VERIFY_TIMEOUT: Duration = Duration::from_secs(120);

/// Default combined stdout+stderr byte cap for one held-out `verify_command`.
///
/// A verification command emits machine diagnostics, not model prose: 2 MiB is far above any
/// honest pass/fail trace, and a command that produces more is flooding (or looping) and is
/// terminated rather than buffered into the harness. Deliberately below the generic supervisor
/// default (8 MiB), because a bench verdict only ever reads a bounded stderr snippet.
pub const DEFAULT_VERIFY_MAX_OUTPUT_BYTES: usize = 2 * 1024 * 1024;

/// Maximum bytes read from one post-condition file.
///
/// A verdict only needs to know whether a needle is present, so a file past this cap is a failed
/// check rather than a whole-file allocation (the F07 "large file" blow-up). Deliberately the same
/// order as the `verify_command` cap: a bounded, explicit limit instead of an unbounded read.
pub const MAX_POST_CONDITION_READ_BYTES: u64 = 1024 * 1024;

/// Default PRNG seed for the randomized arm order and the paired bootstrap.
pub const DEFAULT_BENCH_SEED: u64 = 0x5EED_1234_ABCD_0001;

/// One self-contained benchmark task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BenchTask {
    pub id: String,
    /// Prompt handed to the agent under test.
    pub prompt: String,
    /// Initial workspace files (workspace-relative path -> contents).
    #[serde(default)]
    pub files: BTreeMap<String, String>,
    /// Post-conditions: each `(path, needle)` must be present in the file for the task to resolve.
    #[serde(default)]
    pub expect_contains: Vec<(String, String)>,
    /// Post-conditions: each path must no longer exist for the task to resolve.
    #[serde(default)]
    pub expect_absent: Vec<String>,
    /// Marks a task as a "regression" task whose failure is a hard regression signal.
    #[serde(default)]
    pub regression_guard: bool,
    /// Held-out verification command (argv, no shell) executed by the harness — never by the model —
    /// inside the task's isolated workspace after the arm finishes. The task only resolves when the
    /// command exits `0` *and* every post-condition holds. `None` means the post-conditions are the
    /// whole verdict.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verify_command: Option<Vec<String>>,
}

/// The independent verdict for one task in one workspace.
///
/// `checks` is the human-readable trace (`contains src/x.rs "needle"`, `exit 0 for <argv>`), so a
/// failed run can be diagnosed without re-running it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskEvaluation {
    pub resolved: bool,
    pub checks: Vec<String>,
    pub failures: Vec<String>,
    /// Wall time spent executing the held-out `verify_command`, from spawn through process-group
    /// cleanup, kept separate from model inference time. `Duration::ZERO` when the task declares no
    /// `verify_command` or its limits were rejected before spawning. Failures and timeouts are
    /// counted too: the cost was real even when the verdict is "not resolved".
    ///
    /// Serialized as the derived `Duration` form (`{"secs":N,"nanos":N}`). The field is new, so
    /// any consumer that builds `TaskEvaluation` with a struct literal must add it.
    #[serde(default)]
    pub verify_duration: Duration,
}

/// Bounds for one held-out `verify_command` execution.
///
/// Both limits must be non-zero; an invalid configuration is reported as a failed check rather than
/// silently falling back to a default, so a misconfigured arm can never run a verify command
/// unbounded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyCommandLimits {
    /// Wall-clock ceiling for the command, including process-group cleanup.
    pub timeout: Duration,
    /// Combined stdout+stderr byte cap; crossing it terminates the process tree and fails the check.
    pub max_output_bytes: usize,
}

impl Default for VerifyCommandLimits {
    fn default() -> Self {
        Self {
            timeout: DEFAULT_VERIFY_TIMEOUT,
            max_output_bytes: DEFAULT_VERIFY_MAX_OUTPUT_BYTES,
        }
    }
}

impl VerifyCommandLimits {
    pub fn new(timeout: Duration, max_output_bytes: usize) -> Self {
        Self {
            timeout,
            max_output_bytes,
        }
    }

    /// `None` when usable, otherwise why the limits are invalid.
    fn invalid_reason(&self) -> Option<String> {
        if self.timeout.is_zero() {
            return Some("timeout must be > 0".to_string());
        }
        if self.max_output_bytes == 0 {
            return Some("max_output_bytes must be > 0".to_string());
        }
        None
    }
}

impl BenchTask {
    /// Evaluate the workspace-relative post-conditions and return the per-check trace.
    ///
    /// The workspace is opened as a [`WorkspaceCap`] and every path is read through
    /// [`WorkspaceCap::open_existing_no_symlink`], so a task whose post-conditions escape the
    /// workspace (absolute path, `..`, drive prefix, ADS, reserved device name) — or that reaches
    /// outside through a leaf *or ancestor* symlink — can never read outside it. A special file
    /// (FIFO/socket/device) is rejected without blocking and the read is capped at
    /// [`MAX_POST_CONDITION_READ_BYTES`].
    ///
    /// A task with no non-trivial oracle, or an `expect_contains` entry whose needle is empty
    /// (`str::contains("")` is true for every file), is a failed check rather than a vacuous pass.
    /// `expect_absent` only passes on a genuine [`WorkspaceError::NotFound`]; a dangling symlink, a
    /// permission/I-O error or any other non-`NotFound` outcome fails the check.
    pub fn check_post_conditions(&self, workspace: &Path) -> (Vec<String>, Vec<String>) {
        let mut checks = Vec::new();
        let mut failures = Vec::new();
        if let Some(defect) = self.oracle_defect() {
            failures.push(format!("failed: invalid oracle ({})", defect));
            return (checks, failures);
        }
        let cap = match WorkspaceCap::new(workspace) {
            Ok(cap) => cap,
            Err(e) => {
                failures.push(format!("failed: cannot open workspace capability ({})", e));
                return (checks, failures);
            }
        };
        for (path, needle) in &self.expect_contains {
            if WorkspaceCap::validate_relative_components(path).is_err() {
                failures.push(format!("invalid post-condition path '{}'", path));
                continue;
            }
            let label = format!("contains {} {:?}", path, needle);
            match read_post_condition(&cap, path) {
                Ok(text) if text.contains(needle.as_str()) => checks.push(format!("ok: {}", label)),
                Ok(_) => failures.push(format!("failed: {}", label)),
                Err(reason) => failures.push(format!("failed: {} ({})", label, reason)),
            }
        }
        for path in &self.expect_absent {
            if WorkspaceCap::validate_relative_components(path).is_err() {
                failures.push(format!("invalid post-condition path '{}'", path));
                continue;
            }
            let label = format!("absent {}", path);
            match cap.open_existing_no_symlink(path) {
                Ok(_) => failures.push(format!("failed: {}", label)),
                Err(WorkspaceError::NotFound { .. }) => checks.push(format!("ok: {}", label)),
                Err(e) => failures.push(format!("failed: {} (cannot prove absence: {})", label, e)),
            }
        }
        (checks, failures)
    }

    /// The static defect that makes a task's verdict meaningless, or `None` for a valid oracle.
    ///
    /// * an empty `expect_contains` needle is true for every file, so the check would "pass" with
    ///   the agent doing nothing;
    /// * a task with no `expect_contains`, no `expect_absent` and no non-empty `verify_command` has
    ///   nothing to verify, so its empty failure list would be reported as `resolved`.
    ///
    /// The same predicate gates [`load_suite`] (the suite is rejected) and the direct
    /// `check_post_conditions`/`evaluate` path (the task fails), so a hostile or careless suite
    /// cannot reach a fake `resolved` through either entry point.
    fn oracle_defect(&self) -> Option<String> {
        for (path, needle) in &self.expect_contains {
            if needle.is_empty() {
                return Some(format!(
                    "empty needle for '{}' is trivially true for every file",
                    path
                ));
            }
        }
        let has_verify = self
            .verify_command
            .as_ref()
            .is_some_and(|argv| !argv.is_empty());
        if self.expect_contains.is_empty() && self.expect_absent.is_empty() && !has_verify {
            return Some(
                "no oracle: declare expect_contains, expect_absent or a non-empty verify_command"
                    .to_string(),
            );
        }
        None
    }

    /// `true` when every post-condition holds. The held-out `verify_command` (if any) is executed by
    /// [`Self::evaluate`], which is the verdict the benchmark actually reports.
    pub fn is_resolved(&self, workspace: &Path) -> bool {
        self.check_post_conditions(workspace).1.is_empty()
    }

    /// The full independent verdict with the default limits ([`VerifyCommandLimits::default`]).
    pub async fn evaluate(&self, workspace: &Path) -> TaskEvaluation {
        self.evaluate_with_limits(workspace, &VerifyCommandLimits::default())
            .await
    }

    /// The full independent verdict with an explicit `verify_command` timeout and the default
    /// output cap.
    pub async fn evaluate_with_timeout(
        &self,
        workspace: &Path,
        timeout: Duration,
    ) -> TaskEvaluation {
        self.evaluate_with_limits(
            workspace,
            &VerifyCommandLimits {
                timeout,
                max_output_bytes: DEFAULT_VERIFY_MAX_OUTPUT_BYTES,
            },
        )
        .await
    }

    /// The full independent verdict: post-conditions **and** the held-out `verify_command`.
    ///
    /// The command is spawned as an argv (never through a shell) in the task's isolated workspace
    /// under the shared [`ProcessSupervisor`]: sanitized environment (host credentials stripped),
    /// its own process group (descendants swept after exit, timeout, deadline or cancellation), and
    /// a combined stdout+stderr cap. Display-only head/tail trimming (`output_trimmed`) never turns
    /// a successful exit into a failure; an incomplete capture (flood cap, drain cut, timeout) can
    /// never be reported as success.
    pub async fn evaluate_with_limits(
        &self,
        workspace: &Path,
        limits: &VerifyCommandLimits,
    ) -> TaskEvaluation {
        self.evaluate_with_sandbox(workspace, limits, None).await
    }

    /// [`Self::evaluate_with_limits`] with an optional OS sandbox for the held-out
    /// `verify_command`.
    ///
    /// When `sandbox` is `Some`, the command runs inside that sandbox: the sandbox's
    /// `workspace_host` must be exactly the `workspace` being scored, so the guest `/workspace`
    /// view and the scored directory are the same bytes. Fail-closed: an unusable sandbox backend
    /// is a failure, never a host run (the supervisor is not called at all).
    pub async fn evaluate_with_sandbox(
        &self,
        workspace: &Path,
        limits: &VerifyCommandLimits,
        sandbox: Option<&crate::sandbox::SandboxSpec>,
    ) -> TaskEvaluation {
        let (mut checks, mut failures) = self.check_post_conditions(workspace);
        let mut verify_duration = Duration::ZERO;
        if let Some(argv) = &self.verify_command {
            if argv.is_empty() {
                failures.push("failed: empty verify_command argv".to_string());
            } else if let Some(reason) = limits.invalid_reason() {
                failures.push(format!("failed: invalid verify_command limits ({})", reason));
            } else {
                let label = format!("verify_command exit 0 for {:?}", argv);
                let started = Instant::now();
                let outcome = run_verify_command(argv, workspace, limits, sandbox).await;
                verify_duration = started.elapsed();
                match outcome {
                    Ok(()) => checks.push(format!("ok: {}", label)),
                    Err(reason) => failures.push(format!("failed: {} ({})", label, reason)),
                }
            }
        }
        TaskEvaluation {
            resolved: failures.is_empty(),
            checks,
            failures,
            verify_duration,
        }
    }
}

/// Read one workspace-relative post-condition file through a capability handle.
///
/// [`WorkspaceCap::open_existing_no_symlink`] rejects any symlink component and any special file
/// (FIFO/socket/device) before a byte is read; the read itself is capped at
/// [`MAX_POST_CONDITION_READ_BYTES`] so an oversized artefact is a failure, not an unbounded
/// allocation. Any error (missing, symlink, special file, permission, non-UTF-8) is a failure, never
/// a silent pass.
fn read_post_condition(cap: &WorkspaceCap, rel: &str) -> Result<String, String> {
    let (_, file) = cap
        .open_existing_no_symlink(rel)
        .map_err(|e| e.to_string())?;
    let mut buf = Vec::new();
    file.take(MAX_POST_CONDITION_READ_BYTES + 1)
        .read_to_end(&mut buf)
        .map_err(|e| format!("read failed: {}", e))?;
    if buf.len() as u64 > MAX_POST_CONDITION_READ_BYTES {
        return Err(format!(
            "file exceeds the {} byte read cap",
            MAX_POST_CONDITION_READ_BYTES
        ));
    }
    String::from_utf8(buf).map_err(|_| "file is not valid UTF-8".to_string())
}

/// Run one held-out verification command under the shared one-shot [`ProcessSupervisor`].
///
/// Returns `Err(reason)` for a non-zero exit, a spawn error, a timeout/deadline/cancellation, or an
/// incomplete capture (output flood or pipe-drain cut). A clean exit 0 whose *display* was trimmed
/// by the head/tail window is still `Ok`.
async fn run_verify_command(
    argv: &[String],
    workspace: &Path,
    limits: &VerifyCommandLimits,
    sandbox: Option<&crate::sandbox::SandboxSpec>,
) -> Result<(), String> {
    let (argv, cwd) = match sandbox {
        Some(sandbox) => {
            // The scored directory and the sandbox view must be the same bytes; a mismatch is a
            // configuration error, not something to paper over.
            if sandbox.workspace_host != workspace {
                return Err(format!(
                    "sandbox workspace '{}' is not the scored workspace '{}'",
                    sandbox.workspace_host.display(),
                    workspace.display()
                ));
            }
            // Fail-closed: an unusable backend fails the check instead of running on the host.
            sandbox
                .probe()
                .map_err(|e| format!("sandbox unavailable: {}", e))?;
            let wrapped = sandbox
                .wrap_argv(&argv[0], &argv[1..])
                .map_err(|e| format!("sandbox rejected the verify command: {}", e))?;
            (wrapped, workspace.to_path_buf())
        }
        None => (argv.to_vec(), workspace.to_path_buf()),
    };
    let spec = OneShotCommandSpec::new(argv, cwd)
        .with_timeout(limits.timeout)
        .with_max_output_bytes(limits.max_output_bytes)
        .with_env_policy(EnvironmentPolicy::SanitizedMinimal);
    let res = ProcessSupervisor::execute_one_shot(spec, None)
        .await
        .map_err(|e| format!("spawn failed: {}", e))?;
    if res.is_success() {
        return Ok(());
    }
    Err(describe_verify_failure(&res, limits))
}

/// Human-readable reason a supervised verify run did not count as success.
fn describe_verify_failure(
    res: &crate::process_supervisor::OneShotExecutionResult,
    limits: &VerifyCommandLimits,
) -> String {
    use crate::process_supervisor::TerminationReason;
    if res.truncated && matches!(res.terminated_by, None | Some(TerminationReason::Exit)) {
        return "incomplete capture (output closed before EOF)".to_string();
    }
    match res.terminated_by {
        Some(TerminationReason::Timeout) => format!("timed out after {:?}", limits.timeout),
        Some(TerminationReason::OutputFlood) => {
            format!("output exceeded the {} byte cap", limits.max_output_bytes)
        }
        Some(TerminationReason::Cancelled) => "cancelled".to_string(),
        Some(TerminationReason::Deadline) => "run deadline exceeded".to_string(),
        _ => {
            let code = res
                .exit_code
                .map(|c| c.to_string())
                .unwrap_or_else(|| "signal".to_string());
            let stderr = res.stderr.trim().chars().take(400).collect::<String>();
            format!("exit={} stderr={}", code, stderr)
        }
    }
}

/// One arm of a paired comparison.
///
/// An arm declares both the mechanism under test and the capability envelope it runs with, so a
/// comparison never silently mixes "this arm had fewer tools" with "this mechanism is worse".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BenchStrategyConfig {
    /// Stable arm name used in the report (for example `single_agent`).
    pub name: String,
    /// Context projection profile this arm runs with.
    #[serde(default)]
    pub projection_profile: Option<String>,
    /// Adaptive tournament candidate count for this arm (`0` = single agent).
    #[serde(default)]
    pub adaptive_tournament_candidates: usize,
    /// Subagent spawn quota for this arm. `Some(0)` hides `spawn_subagent` from the model entirely;
    /// `None` inherits the executor's configured quota. Recorded per result so a report can never
    /// call an arm "single agent" while it was spawning children.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_subagents: Option<usize>,
    /// Capability gap #2 (`v1.5.2`): arm ablation — enable the isolated counterfactual probe.
    #[serde(default)]
    pub counterfactual_probe: bool,
    /// Capability gap #3 (`v1.5.2`): arm ablation — causal citation pinning + dead-end compression.
    #[serde(default)]
    pub causal_pruning: bool,
    /// Capability gap #5 (`v1.5.2`): arm ablation — require a repair-race winner to have actually
    /// passed verification (strict by default; the strictness is the baseline, not the treatment).
    #[serde(default = "default_require_verified")]
    pub require_verified: bool,
    /// Capability gap #6 (`v1.5.2`): arm ablation — pin only the core tool schemas and fetch the
    /// rest on demand via `discover_tools`.
    #[serde(default)]
    pub on_demand_tools: bool,
}

fn default_require_verified() -> bool {
    true
}

impl BenchStrategyConfig {
    /// A true single agent: no subagent tool, no adaptive escalation.
    pub fn single_agent() -> Self {
        Self {
            name: "single_agent".to_string(),
            projection_profile: Some("p2".to_string()),
            adaptive_tournament_candidates: 0,
            max_subagents: Some(0),
            counterfactual_probe: false,
            causal_pruning: false,
            require_verified: true,
            on_demand_tools: false,
        }
    }

    /// The adaptive-tournament arm: escalation is the mechanism under test, and subagents stay at
    /// the executor's configured quota (recorded per result).
    pub fn adaptive_tournament(candidates: usize) -> Self {
        Self {
            name: format!("adaptive_tournament_{}", candidates),
            projection_profile: Some("p2".to_string()),
            adaptive_tournament_candidates: candidates.max(2),
            max_subagents: None,
            counterfactual_probe: false,
            causal_pruning: false,
            require_verified: true,
            on_demand_tools: false,
        }
    }

    /// Override the subagent quota for this arm (`Some(0)` hides `spawn_subagent`).
    pub fn with_max_subagents(mut self, max_subagents: Option<usize>) -> Self {
        self.max_subagents = Some(max_subagents.unwrap_or(0));
        self
    }

    /// Resolve the arm's projection profile, `None` (kernel default = `P2`) when unset/unknown.
    pub fn resolved_projection_profile(&self) -> Option<ProjectionProfile> {
        self.projection_profile
            .as_deref()
            .and_then(|value| ProjectionProfile::parse(value).ok())
    }
}

/// Observed activity. Missing legacy telemetry is None, not invented zero activity.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MechanismActivity {
    pub adaptive_tournaments: usize,
    pub candidate_runs: usize,
    pub verified_candidates: usize,
    pub merged_tournaments: usize,
}

/// Result of running one arm on one task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BenchArmResult {
    pub task_id: String,
    pub strategy: String,
    /// `true` when every post-condition **and** the held-out `verify_command` (if any) held.
    pub resolved: bool,
    /// The harness's independent verdict (`approved` / `rejected`), never the model's self-report.
    pub gate_status: String,
    /// Tokens spent by the parent loop itself, from the provider-reported usage.
    pub self_tokens: u64,
    /// Tokens spent by child subagents / tournament candidates.
    pub children_tokens: u64,
    /// `self_tokens + children_tokens`, as reported by the provider.
    pub total_tokens: u64,
    /// B16: provider-reported usage, split so a priced report can use the cached rate where the
    /// operator's table distinguishes it. The `*_tokens` totals above stay the budget-relevant
    /// numbers; these are the same usage, itemised.
    #[serde(default)]
    pub self_input_tokens: u64,
    #[serde(default)]
    pub self_cached_input_tokens: u64,
    #[serde(default)]
    pub self_output_tokens: u64,
    #[serde(default)]
    pub children_input_tokens: u64,
    #[serde(default)]
    pub children_cached_input_tokens: u64,
    #[serde(default)]
    pub children_output_tokens: u64,
    /// Identity the provider actually returned for this arm, when a response stated one. Never
    /// backfilled from the requested alias.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reported_model: Option<String>,
    /// Cost estimate from the operator's price table. `None` means unknown — never "free".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_cost_nano_usd: Option<u64>,
    /// Why the estimate is unknown (missing usage, unknown identity, unknown cache split, ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_unknown_reason: Option<String>,
    /// B16: where this arm's token numbers came from (`provider_reported` /
    /// `byte_estimate_not_measured` / `mixed` / `unknown`). Cost and cap compliance inherit the same
    /// caveat: an estimated total is not measured provider usage.
    #[serde(default = "unknown_usage_basis")]
    pub usage_basis: String,
    /// `true` when the reported spend went past the arm's estimated `budget_tokens` ceiling.
    /// The [`crate::budget::TokenBudget`] ledger bounds *estimated* pre-dispatch spend, so a real
    /// provider can report more than the estimate allowed; that is recorded here, not hidden.
    #[serde(default)]
    pub budget_exceeded: bool,
    /// The arm's declared run-tree token cap for this run.
    #[serde(default)]
    pub budget_tokens: u64,
    /// `total_tokens - budget_tokens`, saturating at 0.
    #[serde(default)]
    pub budget_overage: u64,
    // ---- three independent dimensions; none of them implies another ----
    // `resolved`        functional: the independent post-conditions + verify_command held.
    // `cap_ok`          budget compliance: the *reported* usage stayed within the declared cap.
    // `finished_normally` the run ended on its own instead of being cut off by a limiter.
    /// Cap compliance: reported usage within the declared cap. A functionally correct run can still
    /// fail this (real usage vs the byte-based pre-dispatch estimate), and it must not be reported
    /// as a budget-locked success.
    #[serde(default)]
    pub cap_ok: bool,
    /// `true` when the run ended on its own (model stop) rather than being cut off by a limiter
    /// (token / deadline / turn / attempt cap), a protocol error or a cancellation.
    #[serde(default)]
    pub finished_normally: bool,
    pub wall_time_ms: u64,
    /// Wall time spent on the held-out `verify_command`, from spawn through process-group cleanup,
    /// in milliseconds. It is a *sub-item* of [`Self::wall_time_ms`] (which stays the whole arm wall
    /// time, model inference included), so it must never be added on top of it. `0` when the task
    /// declares no `verify_command`.
    #[serde(default)]
    pub verify_time_ms: u64,
    /// Number of regression-guard tasks this arm broke.
    pub regressions: usize,
    /// Which repetition produced this result (`0`-based).
    #[serde(default)]
    pub repetition: usize,
    /// Model id the arm ran against (`None` for the synthetic executor).
    #[serde(default)]
    pub model: Option<String>,
    /// Static system-prompt hash of the run's config (S2 cache-stability identity).
    #[serde(default)]
    pub prompt_hash: Option<String>,
    /// Model-visible tool-schema hash of the run's config.
    #[serde(default)]
    pub tool_schema_hash: Option<String>,
    /// Why the run stopped (`ModelStop`, `TokenBudgetExhausted`, `ProviderProtocolError(..)`, ...).
    #[serde(default)]
    pub terminal_reason: Option<String>,
    /// Failed checks / stop reason, so a failure can be diagnosed from the report.
    #[serde(default)]
    pub failure_trace: Vec<String>,
    // ---- capability envelope the arm actually ran with (never inferred, always recorded) ----
    /// Subagent spawn quota (`0` = `spawn_subagent` was hidden from the model).
    #[serde(default)]
    pub max_subagents: usize,
    /// `true` when a `bash` (shell) call would have been allowed under the arm's policy.
    #[serde(default)]
    pub bash_allowed: bool,
    /// Capability gap #2/#3/#5 (`v1.5.2`): the arm's ablation envelope, recorded so a report can
    /// attribute a difference to the mechanism instead of guessing which switch was on.
    #[serde(default)]
    pub counterfactual_probe: bool,
    #[serde(default)]
    pub causal_pruning: bool,
    #[serde(default = "default_require_verified")]
    pub require_verified: bool,
    #[serde(default)]
    pub on_demand_tools: bool,
    // ---- trajectory diagnostics, to separate a product defect from a model failure ----
    #[serde(default)]
    pub turns_used: usize,
    #[serde(default)]
    pub tool_calls_used: usize,
    /// Files the run itself reported as modified.
    #[serde(default)]
    pub modified_files: Vec<String>,
    /// Truncated final assistant text — enough to tell "gave up" from "kept looping".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_text: Option<String>,
    /// Ordered tool outcome trace (`name path -> ok|error(CODE)`), capped; the tail is kept because
    /// it shows what the run was still doing when it stopped.
    #[serde(default)]
    pub tool_trace: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mechanism: Option<MechanismActivity>,
}

/// One arm's aggregated numbers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BenchArmSummary {
    pub strategy: String,
    pub tasks: usize,
    pub resolved: usize,
    pub pass_rate: f64,
    /// Wilson 95% interval for `pass_rate`.
    pub pass_rate_ci_low: f64,
    pub pass_rate_ci_high: f64,
    pub total_tokens: u64,
    pub wall_time_ms: u64,
    pub regressions: usize,
    /// Runs whose reported usage stayed within the declared cap (independent of `resolved`).
    pub cap_ok: usize,
    /// Total tokens reported above the declared caps.
    pub budget_overage_tokens: u64,
    /// Runs that ended on their own instead of being cut off by a limiter.
    pub finished_normally: usize,
}

/// Report-level cap-compliance roll-up: the second axis that functional pass rate must never hide.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CapComplianceSummary {
    pub runs: usize,
    /// Runs whose reported usage stayed within the declared cap.
    pub within_cap: usize,
    /// Runs whose reported usage exceeded the declared cap.
    pub over_cap: usize,
    /// Total tokens reported above the declared caps.
    pub overage_tokens: u64,
    /// Runs cut off by a limiter / protocol error / cancellation rather than a model stop.
    pub cut_off: usize,
}

/// Paired pass-rate difference of one arm against the baseline arm, with a bootstrap interval.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PairedComparison {
    pub baseline: String,
    pub arm: String,
    /// Mean per-task pass-rate difference (`arm - baseline`) over the paired observations.
    pub delta_pass_rate: f64,
    /// 95% percentile bootstrap interval for `delta_pass_rate`.
    pub ci_low: f64,
    pub ci_high: f64,
    /// Number of paired `(task, repetition)` observations behind the estimate.
    pub paired_observations: usize,
    /// Number of independent *tasks* the observations are clustered into. Repetitions of one task
    /// are not independent draws, so the interval resamples tasks, not individual observations.
    #[serde(default)]
    pub cluster_count: usize,
    /// Verdict against the preregistered threshold, when the caller declared one. `None` means no
    /// threshold was declared up front, so the report only shows the interval.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision: Option<String>,
    /// Why that verdict was reached, stated in the report so a reader can audit it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision_rationale: Option<String>,
    /// Statistics over the holdout split only, when one was declared.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub holdout: Option<SplitStats>,
    /// Which split the verdict was taken on (`holdout` or `all tasks`), so a reader can tell whether
    /// the claim rests on held-out evidence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision_basis: Option<String>,
}

/// Pass-rate difference over one split of the suite.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SplitStats {
    pub delta_pass_rate: f64,
    pub ci_low: f64,
    pub ci_high: f64,
    pub paired_observations: usize,
    pub cluster_count: usize,
}

/// The decision thresholds a caller must declare **before** running a comparison.
///
/// Declaring them up front is the point: a threshold chosen after seeing the interval is not a
/// pre-registered criterion, and the report says which threshold was used so that cannot be
/// rewritten silently.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecisionThresholds {
    /// Minimum pass-rate improvement over the baseline that counts as a real effect.
    pub min_effect: f64,
    /// With fewer independent tasks than this, no verdict is issued: a handful of clusters cannot
    /// support a positive claim whatever the interval says.
    #[serde(default = "default_min_clusters")]
    pub min_clusters: usize,
    /// Safety gate: the arm may fail at most this many regression-guard tasks. Defaults to zero —
    /// a change that breaks a guarded behaviour is not shippable, whatever the average says.
    #[serde(default = "default_max_regression_failures")]
    pub max_regression_failures: Option<usize>,
    /// Cost gate: the arm's mean estimated cost may exceed the baseline by at most this many
    /// nano-USD. `None` leaves cost ungated. When it is set and the estimate is unknown the verdict
    /// is `inconclusive`, because affordability cannot be proven from missing data.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_cost_increase_nano_usd: Option<u64>,
}

fn default_min_clusters() -> usize {
    5
}

fn default_max_regression_failures() -> Option<usize> {
    Some(0)
}

impl Default for DecisionThresholds {
    fn default() -> Self {
        Self {
            min_effect: 0.0,
            min_clusters: default_min_clusters(),
            max_regression_failures: default_max_regression_failures(),
            max_cost_increase_nano_usd: None,
        }
    }
}

/// Everything a verdict is decided on. Grouped so adding a gate cannot silently change an existing
/// call site's meaning.
#[derive(Debug, Clone, PartialEq)]
pub struct DecisionEvidence {
    pub ci_low: f64,
    pub ci_high: f64,
    pub clusters: usize,
    /// Regression-guard tasks the arm failed.
    pub regression_failures: usize,
    /// Mean estimated-cost difference (arm − baseline) in nano-USD, or `None` when unknown.
    pub cost_delta_nano_usd: Option<i64>,
}

impl DecisionEvidence {
    pub fn new(ci_low: f64, ci_high: f64, clusters: usize) -> Self {
        Self {
            ci_low,
            ci_high,
            clusters,
            regression_failures: 0,
            cost_delta_nano_usd: None,
        }
    }

    pub fn with_regression_failures(mut self, failures: usize) -> Self {
        self.regression_failures = failures;
        self
    }

    pub fn with_cost_delta(mut self, delta: Option<i64>) -> Self {
        self.cost_delta_nano_usd = delta;
        self
    }
}

/// Identity of the run that produced a report.
///
/// Provider-backed runs fill this in so a number can be traced back to a model, a wire protocol and
/// a source revision. The provider `origin` is scheme/host/port only — credentials and query
/// strings never reach the report.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BenchRunMetadata {
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub protocol: Option<String>,
    #[serde(default)]
    pub provider_origin: Option<String>,
    #[serde(default)]
    pub commit: Option<String>,
    #[serde(default)]
    pub prompt_hash: Option<String>,
    #[serde(default)]
    pub tool_schema_hash: Option<String>,
    /// Repetitions actually executed per (arm, task) pair.
    #[serde(default)]
    pub repetitions: usize,
    /// Seed that made the randomized arm order and the bootstrap reproducible.
    #[serde(default)]
    pub seed: u64,
    #[serde(default)]
    pub randomized_order: bool,
    /// Identity of the source tree and binary behind this run. A commit id alone does not describe
    /// a dirty tree, so the fingerprint carries its own source hash.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_fingerprint: Option<crate::provenance::SourceFingerprint>,
    /// Version of the operator price table used for cost estimates, when one was configured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price_table_version: Option<String>,
    /// Where those rates came from. Reported so a reader can judge the estimate instead of treating
    /// it as a market price.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price_table_source: Option<String>,
    /// Fingerprint of the task set *and its oracles* that produced this report. Two runs whose
    /// fingerprints differ are not the same benchmark, even if the suite name matches.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suite_sha256: Option<String>,
}

/// How many times to repeat the paired comparison, and with which deterministic seed.
#[derive(Debug, Clone, PartialEq)]
pub struct BenchRunOptions {
    pub repetitions: usize,
    pub seed: u64,
    pub randomize_order: bool,
    /// Preregistered decision thresholds. `None` keeps the report descriptive only.
    pub thresholds: Option<DecisionThresholds>,
    /// Task ids that form the **holdout** split. When non-empty, both splits are reported separately
    /// and the preregistered decision is taken on the holdout split only: an arm tuned against the
    /// development tasks has not been tested by them.
    pub holdout_task_ids: Vec<String>,
}

impl Default for BenchRunOptions {
    fn default() -> Self {
        Self {
            repetitions: 1,
            seed: DEFAULT_BENCH_SEED,
            randomize_order: true,
            thresholds: None,
            holdout_task_ids: Vec::new(),
        }
    }
}

impl BenchRunOptions {
    pub fn new(repetitions: usize, seed: u64, randomize_order: bool) -> Self {
        Self {
            repetitions: repetitions.max(1),
            seed,
            randomize_order,
            thresholds: None,
            holdout_task_ids: Vec::new(),
        }
    }

    /// Declare which tasks are the holdout split (see the field documentation).
    pub fn with_holdout_task_ids<I, S>(mut self, ids: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.holdout_task_ids = ids.into_iter().map(Into::into).collect();
        self
    }

    /// Declare the decision thresholds for this run. Callers are expected to do this *before*
    /// looking at results; the report records which threshold produced each verdict.
    pub fn with_thresholds(mut self, thresholds: DecisionThresholds) -> Self {
        self.thresholds = Some(thresholds);
        self
    }
}

/// Full paired comparison report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BenchComparisonReport {
    pub suite: String,
    /// The single total budget every arm received (paired, budget-locked comparison).
    pub budget_tokens: u64,
    /// `synthetic` when the numbers came from the built-in deterministic executor, `provider` when
    /// they came from a real model run.
    pub executor: String,
    #[serde(default)]
    pub metadata: BenchRunMetadata,
    pub results: Vec<BenchArmResult>,
    pub arms: Vec<BenchArmSummary>,
    #[serde(default)]
    pub paired_deltas: Vec<PairedComparison>,
    /// Name of the point-estimate winner, or `None` on a tie. See [`Self::caveats`]: on a small
    /// sample this is a ranking, not evidence of improvement.
    pub winner: Option<String>,
}

impl BenchComparisonReport {
    /// Aggregate raw per-task results into per-arm summaries, paired deltas and a winner.
    ///
    /// Ranking is lexicographic: higher pass rate, then fewer regressions, then lower total tokens,
    /// then lower wall time. This makes "spent more to win" a losing strategy on ties.
    // The report constructor takes the run's inputs one by one (suite, budget, executor, results,
    // metadata, baseline) plus the two judging knobs; bundling them would only move the same list
    // one level down. Same precedent as `task_host::link_merge_in_session`.
    #[allow(clippy::too_many_arguments)]
    pub fn from_results(
        suite: impl Into<String>,
        budget_tokens: u64,
        executor: impl Into<String>,
        results: Vec<BenchArmResult>,
        metadata: BenchRunMetadata,
        baseline: Option<String>,
        thresholds: Option<&DecisionThresholds>,
        holdout_task_ids: &[String],
    ) -> Self {
        let mut by_arm: BTreeMap<String, Vec<&BenchArmResult>> = BTreeMap::new();
        for r in &results {
            by_arm.entry(r.strategy.clone()).or_default().push(r);
        }
        let mut arms: Vec<BenchArmSummary> = Vec::new();
        for (strategy, rs) in by_arm {
            let tasks = rs.len();
            let resolved = rs.iter().filter(|r| r.resolved).count();
            let total_tokens = rs.iter().map(|r| r.total_tokens).sum::<u64>();
            let wall_time_ms = rs.iter().map(|r| r.wall_time_ms).sum::<u64>();
            let regressions = rs.iter().map(|r| r.regressions).sum::<usize>();
            let (pass_rate_ci_low, pass_rate_ci_high) = wilson_interval(resolved, tasks);
            let cap_ok = rs.iter().filter(|r| r.cap_ok).count();
            let budget_overage_tokens = rs.iter().map(|r| r.budget_overage).sum::<u64>();
            let finished_normally = rs.iter().filter(|r| r.finished_normally).count();
            arms.push(BenchArmSummary {
                strategy,
                tasks,
                resolved,
                pass_rate: if tasks == 0 {
                    0.0
                } else {
                    resolved as f64 / tasks as f64
                },
                pass_rate_ci_low,
                pass_rate_ci_high,
                total_tokens,
                wall_time_ms,
                regressions,
                cap_ok,
                budget_overage_tokens,
                finished_normally,
            });
        }
        arms.sort_by(|a, b| {
            b.pass_rate
                .partial_cmp(&a.pass_rate)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.regressions.cmp(&b.regressions))
                .then_with(|| a.total_tokens.cmp(&b.total_tokens))
                .then_with(|| a.wall_time_ms.cmp(&b.wall_time_ms))
                .then_with(|| a.strategy.cmp(&b.strategy))
        });

        let winner = match (arms.first(), arms.get(1)) {
            (Some(a), Some(b))
                if a.pass_rate == b.pass_rate
                    && a.regressions == b.regressions
                    && a.total_tokens == b.total_tokens =>
            {
                None
            }
            (Some(a), _) => Some(a.strategy.clone()),
            (None, _) => None,
        };

        let paired_deltas = paired_deltas(
            &results,
            baseline.as_deref(),
            metadata.seed,
            thresholds,
            holdout_task_ids,
        );

        Self {
            suite: suite.into(),
            budget_tokens,
            executor: executor.into(),
            metadata,
            results,
            arms,
            paired_deltas,
            winner,
        }
    }

    /// The capability envelope each arm actually ran with, in first-seen order. Capabilities are a
    /// property of the arm (not the repetition), so the value recorded on the arm's first result is
    /// the value shown; the per-result JSON carries the same fields for auditing.
    pub fn capabilities(&self) -> Vec<(String, usize, bool)> {
        let mut seen: Vec<(String, usize, bool)> = Vec::new();
        for result in &self.results {
            if !seen.iter().any(|(name, _, _)| name == &result.strategy) {
                seen.push((
                    result.strategy.clone(),
                    result.max_subagents,
                    result.bash_allowed,
                ));
            }
        }
        seen
    }

    /// Capability gap #2/#3/#5 (`v1.5.2`): the per-arm mechanism ablation envelope, recorded on the
    /// first result of each arm, so a reader can see which experimental switch was on.
    pub fn ablation_envelope(&self) -> Vec<(String, bool, bool, bool, bool)> {
        let mut seen: Vec<(String, bool, bool, bool, bool)> = Vec::new();
        for result in &self.results {
            if !seen
                .iter()
                .any(|(name, _, _, _, _)| name == &result.strategy)
            {
                seen.push((
                    result.strategy.clone(),
                    result.counterfactual_probe,
                    result.causal_pruning,
                    result.require_verified,
                    result.on_demand_tools,
                ));
            }
        }
        seen
    }

    /// Cap compliance across every run in the report — an axis independent of functional pass rate.
    pub fn cap_compliance(&self) -> CapComplianceSummary {
        CapComplianceSummary {
            runs: self.results.len(),
            within_cap: self.results.iter().filter(|r| r.cap_ok).count(),
            over_cap: self.results.iter().filter(|r| !r.cap_ok).count(),
            overage_tokens: self.results.iter().map(|r| r.budget_overage).sum(),
            cut_off: self.results.iter().filter(|r| !r.finished_normally).count(),
        }
    }

    /// Honest limits of this report. Printed by [`Self::render`] so a small sample cannot be read as
    /// a capability claim.
    pub fn caveats(&self) -> Vec<String> {
        let mut caveats = Vec::new();
        if self.executor == SYNTHETIC_EXECUTOR_LABEL {
            caveats.push(
                "synthetic executor — validates the benchmark plumbing only; these numbers are NOT \
                 evidence that a mechanism improves real task resolution."
                    .to_string(),
            );
        }
        if self.executor == PROVIDER_EXECUTOR_LABEL {
            let paired = self
                .paired_deltas
                .iter()
                .map(|d| d.paired_observations)
                .max()
                .unwrap_or(0);
            let observations = self.results.len();
            if paired < 20 || observations < 40 {
                caveats.push(format!(
                    "small sample ({} paired observations, {} total runs): the intervals are wide and \
                     the ranking below is a point estimate, not a demonstrated improvement.",
                    paired, observations
                ));
            }
            let compliance = self.cap_compliance();
            if compliance.over_cap > 0 {
                caveats.push(format!(
                    "cap compliance: {} of {} runs reported more than their declared cap ({} tokens over \
                     in total). Functional pass (resolved) is a separate axis: a run can be correct and \
                     still not be a budget-locked success. The overage comes from the byte-based \
                     pre-dispatch estimate under-counting real provider usage.",
                    compliance.over_cap, compliance.runs, compliance.overage_tokens
                ));
            }
            if compliance.cut_off > 0 {
                caveats.push(format!(
                    "{} of {} runs were cut off by a limiter/protocol error rather than ending on their \
                     own; their pass/fail is a lower bound, not a clean measurement.",
                    compliance.cut_off, compliance.runs
                ));
            }
            if self.metadata.model.is_none()
                || self.metadata.prompt_hash.is_none()
                || self.metadata.tool_schema_hash.is_none()
            {
                caveats.push(
                    "run metadata is incomplete (model/prompt/schema hash missing); the numbers are \
                     not traceable to a model and a revision."
                        .to_string(),
                );
            }
            let envelopes: std::collections::BTreeSet<(usize, bool)> = self
                .capabilities()
                .into_iter()
                .map(|(_, subagents, bash)| (subagents, bash))
                .collect();
            if envelopes.len() > 1 {
                caveats.push(
                    "arms ran with different capability envelopes (see `capabilities:` below): a \
                     pass-rate difference may reflect the tools an arm was given, not the mechanism \
                     under test."
                        .to_string(),
                );
            }
        }
        caveats
    }

    /// Human-readable paired comparison report.
    pub fn render(&self) -> String {
        let mut out = String::new();
        match &self.metadata.suite_sha256 {
            Some(fingerprint) => out.push_str(&format!(
                "suite:    {} (oracles {})\n",
                self.suite, fingerprint
            )),
            None => out.push_str(&format!("suite:    {}\n", self.suite)),
        }
        out.push_str(&format!("executor: {}\n", self.executor));
        out.push_str(&format!(
            "budget:   {} tokens declared per arm per task (real usage can exceed it; see cap compliance)\n",
            self.budget_tokens
        ));
        if let Some(model) = &self.metadata.model {
            out.push_str(&format!("model:    {}\n", model));
        }
        if let Some(protocol) = &self.metadata.protocol {
            out.push_str(&format!("protocol: {}\n", protocol));
        }
        if let Some(commit) = &self.metadata.commit {
            out.push_str(&format!("commit:   {}\n", commit));
        }
        if let Some(fingerprint) = &self.metadata.source_fingerprint {
            out.push_str(&format!("source:   {}\n", fingerprint.summary()));
        }
        let estimated_arms: Vec<&str> = self
            .results
            .iter()
            .filter(|result| result.usage_basis != "provider_reported" && result.usage_basis != "unknown")
            .map(|result| result.strategy.as_str())
            .collect();
        if !estimated_arms.is_empty() {
            let mut names = estimated_arms.clone();
            names.sort_unstable();
            names.dedup();
            // Spelled out because an estimated token total looks exactly like a measured one.
            out.push_str(&format!(
                "usage:    byte-based estimates, not measured usage (arms: {})\n",
                names.join(", ")
            ));
        }
        if let Some(version) = &self.metadata.price_table_version {
            let known: u64 = self
                .results
                .iter()
                .filter_map(|result| result.estimated_cost_nano_usd)
                .sum();
            let unknown = self
                .results
                .iter()
                .filter(|result| result.estimated_cost_nano_usd.is_none())
                .count();
            // Spelled out because a bare number here is easy to mistake for a bill.
            out.push_str(&format!(
                "pricing:  table {} ({}) — operator estimate from reported usage, not an invoice and not an account balance\n",
                version,
                self.metadata.price_table_source.as_deref().unwrap_or("source unknown")
            ));
            out.push_str(&format!(
                "cost:     {} nano-USD for {} arm-runs; {} unpriced\n",
                known,
                self.results.len(),
                unknown
            ));
        }
        if let Some(prompt_hash) = &self.metadata.prompt_hash {
            out.push_str(&format!("prompt:   {}\n", prompt_hash));
        }
        if let Some(schema_hash) = &self.metadata.tool_schema_hash {
            out.push_str(&format!("schema:   {}\n", schema_hash));
        }
        out.push_str(&format!(
            "repeats:  {} (seed {}, randomized order: {})\n",
            self.metadata.repetitions, self.metadata.seed, self.metadata.randomized_order
        ));
        if !self.capabilities().is_empty() {
            out.push_str("capabilities:\n");
            for (arm, max_subagents, bash_allowed) in self.capabilities() {
                out.push_str(&format!(
                    "  {:<24} max_subagents={:<3} bash={}\n",
                    arm,
                    max_subagents,
                    if bash_allowed { "on" } else { "off" }
                ));
            }
        }
        let envelope = self.ablation_envelope();
        if !envelope.is_empty() {
            out.push_str("ablation switches:\n");
            for (arm, probe, causal, require_verified, on_demand) in envelope {
                out.push_str(&format!(
                    "  {:<24} counterfactual_probe={} causal_pruning={} require_verified={} on_demand_tools={}\n",
                    arm,
                    if probe { "on" } else { "off" },
                    if causal { "on" } else { "off" },
                    if require_verified { "on" } else { "off" },
                    if on_demand { "on" } else { "off" }
                ));
            }
        }
        out.push('\n');
        for caveat in self.caveats() {
            out.push_str(&format!("caveat:   {}\n", caveat));
        }
        out.push('\n');
        out.push_str(&format!(
            "{:<26} {:>6} {:>8} {:>9} {:>17} {:>11} {:>12} {:>8}\n",
            "arm", "tasks", "pass", "pass_rate", "95% ci", "total_tok", "wall_ms", "regress"
        ));
        for arm in &self.arms {
            out.push_str(&format!(
                "{:<26} {:>6} {:>8} {:>9.1}% [{:>5.1}%, {:>5.1}%] {:>11} {:>12} {:>8}\n",
                arm.strategy,
                arm.tasks,
                arm.resolved,
                arm.pass_rate * 100.0,
                arm.pass_rate_ci_low * 100.0,
                arm.pass_rate_ci_high * 100.0,
                arm.total_tokens,
                arm.wall_time_ms,
                arm.regressions,
            ));
        }
        out.push('\n');
        let compliance = self.cap_compliance();
        out.push_str(&format!(
            "cap compliance: {}/{} runs within the declared cap ({} over, {} tokens over); {}/{} runs \
             ended on their own ({} cut off)\n",
            compliance.within_cap,
            compliance.runs,
            compliance.over_cap,
            compliance.overage_tokens,
            compliance.runs - compliance.cut_off,
            compliance.runs,
            compliance.cut_off,
        ));
        for arm in &self.arms {
            out.push_str(&format!(
                "  {:<24} cap_ok={}/{} overage={} finished_normally={}/{}\n",
                arm.strategy,
                arm.cap_ok,
                arm.tasks,
                arm.budget_overage_tokens,
                arm.finished_normally,
                arm.tasks,
            ));
        }
        out.push('\n');
        if !self.paired_deltas.is_empty() {
            out.push_str("paired pass-rate differences (arm - baseline, 95% bootstrap ci):\n");
            for delta in &self.paired_deltas {
                out.push_str(&format!(
                    "  {:<24} vs {:<24} {:+.3}  [{:+.3}, {:+.3}]  n={} tasks={}\n",
                    delta.arm,
                    delta.baseline,
                    delta.delta_pass_rate,
                    delta.ci_low,
                    delta.ci_high,
                    delta.paired_observations,
                    delta.cluster_count,
                ));
            }
            for delta in &self.paired_deltas {
                if let Some(stats) = &delta.holdout {
                    out.push_str(&format!(
                        "  holdout:  {:<24} {:+.3}  [{:+.3}, {:+.3}]  n={} tasks={}\n",
                        delta.arm,
                        stats.delta_pass_rate,
                        stats.ci_low,
                        stats.ci_high,
                        stats.paired_observations,
                        stats.cluster_count,
                    ));
                }
                if let (Some(decision), Some(rationale)) =
                    (&delta.decision, &delta.decision_rationale)
                {
                    out.push_str(&format!(
                        "  decision: {} -> {} ({})\n",
                        delta.arm, decision, rationale
                    ));
                }
            }
            out.push('\n');
        }
        match &self.winner {
            Some(name) => out.push_str(&format!("winner (point estimate): {}\n", name)),
            None => out.push_str("winner (point estimate): <tie>\n"),
        }
        out
    }
}

/// Execute one arm on one task.
///
/// The async shape exists because a provider-backed arm drives the real
/// [`crate::agent_loop::AgentRunner`]; the synthetic arm simply returns immediately.
pub trait BenchExecutor: Send + Sync {
    fn label(&self) -> &'static str;

    /// Identity of the runs this executor produces. Defaults to "unknown" so the synthetic executor
    /// never fabricates model/revision metadata.
    fn metadata(&self) -> BenchRunMetadata {
        BenchRunMetadata::default()
    }

    fn execute<'a>(
        &'a self,
        task: &'a BenchTask,
        strategy: &'a BenchStrategyConfig,
        budget_tokens: u64,
        workspace_root: &'a Path,
    ) -> Pin<Box<dyn Future<Output = BenchArmResult> + Send + 'a>>;
}

/// Deterministic synthetic executor used by the built-in smoke suite.
///
/// It exercises the *plumbing* — budget locking, self/children token accounting, post-condition
/// evaluation and report aggregation — without a model endpoint. Tasks whose `id` starts with
/// `fixable-` are resolved by the single agent; tasks whose `id` starts with `stall-` require the
/// adaptive tournament arm to resolve, which mirrors the real escalation story.
///
/// It fabricates the answer by design, so it is always labelled [`SYNTHETIC_EXECUTOR_LABEL`].
pub struct SyntheticBenchExecutor;

impl BenchExecutor for SyntheticBenchExecutor {
    fn label(&self) -> &'static str {
        SYNTHETIC_EXECUTOR_LABEL
    }

    fn execute<'a>(
        &'a self,
        task: &'a BenchTask,
        strategy: &'a BenchStrategyConfig,
        budget_tokens: u64,
        workspace_root: &'a Path,
    ) -> Pin<Box<dyn Future<Output = BenchArmResult> + Send + 'a>> {
        Box::pin(async move {
            let started = Instant::now();
            let _ = std::fs::create_dir_all(workspace_root);

            let uses_tournament = strategy.adaptive_tournament_candidates >= 2;
            let needs_tournament = task.id.starts_with("stall-");
            let single_agent_resolves = task.id.starts_with("fixable-");

            materialize_task(task, workspace_root);

            let attempt_applies_fix = if uses_tournament {
                single_agent_resolves || needs_tournament
            } else {
                single_agent_resolves
            };
            if attempt_applies_fix {
                for (path, needle) in &task.expect_contains {
                    let full = workspace_root.join(path);
                    if let Some(parent) = full.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    let mut text = std::fs::read_to_string(&full).unwrap_or_default();
                    if !text.contains(needle.as_str()) {
                        text.push_str(needle);
                        text.push('\n');
                    }
                    let _ = std::fs::write(&full, text);
                }
                for path in &task.expect_absent {
                    let _ = std::fs::remove_file(workspace_root.join(path));
                }
            }

            let resolved = task.is_resolved(workspace_root);

            // Deterministic, budget-locked synthetic token accounting: the single agent spends a
            // fixed per-task amount; a tournament arm spends more *children* tokens but still
            // respects the same total budget.
            let per_task_self = 900u64;
            let per_candidate_children = 250u64;
            let candidates = if uses_tournament {
                strategy.adaptive_tournament_candidates as u64
            } else {
                0
            };
            let mut self_tokens = per_task_self;
            let mut children_tokens = candidates * per_candidate_children;
            // Hard budget lock: never report more than the shared per-arm budget.
            if self_tokens + children_tokens > budget_tokens {
                let over = self_tokens + children_tokens - budget_tokens;
                let cut = over.min(children_tokens);
                children_tokens -= cut;
                let remaining = over - cut;
                if remaining > 0 {
                    self_tokens = self_tokens.saturating_sub(remaining);
                }
            }

            let mut failure_trace = Vec::new();
            if !resolved {
                failure_trace
                    .push("synthetic executor: task did not meet its post-conditions".to_string());
            }

            BenchArmResult {
                task_id: task.id.clone(),
                strategy: strategy.name.clone(),
                resolved,
                gate_status: if resolved { "approved" } else { "rejected" }.to_string(),
            // No provider usage exists for this result, so every priced field stays unknown rather
            // than being defaulted to a free run.
            self_input_tokens: 0,
            self_cached_input_tokens: 0,
            self_output_tokens: 0,
            children_input_tokens: 0,
            children_cached_input_tokens: 0,
            children_output_tokens: 0,
            reported_model: None,
            usage_basis: "unknown".to_string(),
            estimated_cost_nano_usd: None,
            cost_unknown_reason: None,
                self_tokens,
                children_tokens,
                total_tokens: self_tokens + children_tokens,
                budget_exceeded: false,
                budget_tokens,
                budget_overage: 0,
                cap_ok: true,
                // The synthetic executor runs to completion by construction; it is not a model run.
                finished_normally: true,
                wall_time_ms: started.elapsed().as_millis() as u64,
                verify_time_ms: 0,
                regressions: usize::from(task.regression_guard && !resolved),
                repetition: 0,
                model: None,
                prompt_hash: None,
                tool_schema_hash: None,
                terminal_reason: Some("synthetic".to_string()),
                failure_trace,
                max_subagents: 0,
                bash_allowed: false,
                counterfactual_probe: strategy.counterfactual_probe,
                causal_pruning: strategy.causal_pruning,
                require_verified: strategy.require_verified,
                on_demand_tools: strategy.on_demand_tools,
                turns_used: 0,
                tool_calls_used: 0,
                modified_files: Vec::new(),
                final_text: None,
                tool_trace: Vec::new(),
                mechanism: None,
            }
        })
    }
}

/// Configuration for [`ProviderBenchExecutor`]: everything the real arm needs that is not the task
/// itself.
#[derive(Debug, Clone)]
pub struct ProviderBenchSettings {
    pub model: String,
    pub protocol: String,
    /// `scheme://host[:port]` of the provider endpoint — never credentials, path or query.
    pub provider_origin: Option<String>,
    /// Source revision the binary was built from, when it could be detected.
    pub commit: Option<String>,
    /// Root of the source tree this harness should fingerprint. Defaults to the process working
    /// directory; set it explicitly when the harness runs from somewhere else.
    pub source_root: Option<std::path::PathBuf>,
    pub max_model_turns: usize,
    pub max_run_duration: Duration,
    pub policy_mode: PolicyMode,
    pub max_subagents: usize,
    pub verify_timeout: Duration,
    /// Combined stdout+stderr byte cap for one held-out `verify_command`.
    pub verify_max_output_bytes: usize,
    /// When set, every held-out `verify_command` runs inside this sandbox, resolved against the
    /// arm's own isolated workspace. `None` keeps the historical unsandboxed behaviour.
    pub sandbox: Option<crate::sandbox::SandboxTemplate>,
    /// Operator-supplied price table. When set, every arm records a cost *estimate* from the
    /// provider-reported usage and the identity the provider actually returned. It is never an
    /// invoice, never an account balance, and stays unknown when usage or identity is missing.
    pub price_table: Option<crate::usage_cost::PriceTable>,
}

impl ProviderBenchSettings {
    pub fn new(model: impl Into<String>, protocol: impl Into<String>) -> Self {
        let defaults = BudgetConfig::default();
        Self {
            model: model.into(),
            protocol: protocol.into(),
            provider_origin: None,
            commit: None,
            source_root: None,
            max_model_turns: defaults.max_model_turns,
            max_run_duration: defaults.max_run_duration,
            policy_mode: PolicyMode::AllowWrite,
            max_subagents: crate::subagents::DEFAULT_MAX_SUBAGENT_SPAWNS,
            verify_timeout: DEFAULT_VERIFY_TIMEOUT,
            verify_max_output_bytes: DEFAULT_VERIFY_MAX_OUTPUT_BYTES,
            sandbox: None,
            price_table: None,
        }
    }

    pub fn with_provider_origin(mut self, origin: Option<String>) -> Self {
        self.provider_origin = origin;
        self
    }

    pub fn with_commit(mut self, commit: Option<String>) -> Self {
        self.commit = commit;
        self
    }

    pub fn with_policy_mode(mut self, mode: PolicyMode) -> Self {
        self.policy_mode = mode;
        self
    }

    pub fn with_max_subagents(mut self, max_subagents: usize) -> Self {
        self.max_subagents = max_subagents;
        self
    }

    pub fn with_max_model_turns(mut self, turns: usize) -> Self {
        self.max_model_turns = turns;
        self
    }

    pub fn with_max_run_duration(mut self, duration: Duration) -> Self {
        self.max_run_duration = duration;
        self
    }

    /// Wall-clock ceiling for each held-out `verify_command` this executor runs.
    pub fn with_verify_timeout(mut self, timeout: Duration) -> Self {
        self.verify_timeout = timeout;
        self
    }

    /// Combined stdout+stderr byte cap for each held-out `verify_command` this executor runs.
    pub fn with_verify_max_output_bytes(mut self, max_output_bytes: usize) -> Self {
        self.verify_max_output_bytes = max_output_bytes;
        self
    }

    /// Operator price table for cost estimates (see [`crate::usage_cost`]).
    pub fn with_price_table(mut self, table: Option<crate::usage_cost::PriceTable>) -> Self {
        self.price_table = table;
        self
    }

    /// Source tree this report's provenance describes. Defaults to the process working directory,
    /// which is only walked when it looks like a checkout.
    pub fn with_source_root(mut self, root: impl Into<std::path::PathBuf>) -> Self {
        self.source_root = Some(root.into());
        self
    }

    /// Run every held-out `verify_command` inside this sandbox, bound to each arm's own workspace.
    ///
    /// This is what turns the isolation from an available capability into the scored behaviour: an
    /// unusable backend fails the check instead of silently running on the host.
    pub fn with_sandbox(mut self, template: Option<crate::sandbox::SandboxTemplate>) -> Self {
        self.sandbox = template;
        self
    }
}

impl Default for ProviderBenchSettings {
    fn default() -> Self {
        Self::new("unknown", "auto")
    }
}

/// The real executor: drives [`AgentRunner`] against a [`ModelProvider`] in an isolated per-arm
/// workspace, then evaluates the task itself.
///
/// The executor never writes the expected answer. It materializes only the task's *initial* files,
/// runs the agent with the task prompt, and hands the resulting workspace to
/// [`BenchTask::evaluate`] — post-conditions plus the held-out `verify_command`. Self and child
/// token spend come from the provider-reported usage on the run summary, and the run-tree token
/// ceiling is the arm's `budget_tokens`.
pub struct ProviderBenchExecutor {
    provider: Arc<dyn ModelProvider>,
    settings: ProviderBenchSettings,
    prompt_builder: PromptBuilder,
    development_gate: Option<crate::env::SharedCompletionGate>,
}

impl ProviderBenchExecutor {
    pub fn new(provider: Arc<dyn ModelProvider>, settings: ProviderBenchSettings) -> Self {
        Self {
            provider,
            settings,
            prompt_builder: PromptBuilder::default(),
            development_gate: None,
        }
    }

    /// Operator-authorized development verification, separate from the held-out final scorer.
    /// Its feedback may be shown to the agent; never pass `task.verify_command` here.
    pub fn with_development_gate(mut self, gate: crate::env::SharedCompletionGate) -> Self {
        self.development_gate = Some(gate);
        self
    }

    /// Override the system prompt builder (kept identical to a non-bench run so prompt hashes match
    /// what the operator would get from `kroma run`).
    pub fn with_prompt_builder(mut self, prompt_builder: PromptBuilder) -> Self {
        self.prompt_builder = prompt_builder;
        self
    }

    fn error_result(
        &self,
        task: &BenchTask,
        strategy: &BenchStrategyConfig,
        started: Instant,
        failure: String,
    ) -> BenchArmResult {
        BenchArmResult {
            task_id: task.id.clone(),
            strategy: strategy.name.clone(),
            resolved: false,
            gate_status: "rejected".to_string(),
            // No provider usage exists for this result, so every priced field stays unknown rather
            // than being defaulted to a free run.
            self_input_tokens: 0,
            self_cached_input_tokens: 0,
            self_output_tokens: 0,
            children_input_tokens: 0,
            children_cached_input_tokens: 0,
            children_output_tokens: 0,
            reported_model: None,
            usage_basis: "unknown".to_string(),
            estimated_cost_nano_usd: None,
            cost_unknown_reason: None,
            self_tokens: 0,
            children_tokens: 0,
            total_tokens: 0,
            budget_exceeded: false,
            budget_tokens: 0,
            budget_overage: 0,
            cap_ok: true,
            finished_normally: false,
            wall_time_ms: started.elapsed().as_millis() as u64,
            verify_time_ms: 0,
            regressions: usize::from(task.regression_guard),
            repetition: 0,
            model: Some(self.settings.model.clone()),
            prompt_hash: None,
            tool_schema_hash: None,
            terminal_reason: Some("BenchSetupFailed".to_string()),
            failure_trace: vec![failure],
            max_subagents: 0,
            bash_allowed: false,
            counterfactual_probe: strategy.counterfactual_probe,
            causal_pruning: strategy.causal_pruning,
            require_verified: strategy.require_verified,
            on_demand_tools: strategy.on_demand_tools,
            turns_used: 0,
            tool_calls_used: 0,
            modified_files: Vec::new(),
            final_text: None,
            tool_trace: Vec::new(),
                mechanism: None,
        }
    }

    async fn run_arm(
        &self,
        task: &BenchTask,
        strategy: &BenchStrategyConfig,
        budget_tokens: u64,
        workspace_root: &Path,
    ) -> BenchArmResult {
        let started = Instant::now();

        if let Err(e) = std::fs::create_dir_all(workspace_root) {
            return self.error_result(
                task,
                strategy,
                started,
                format!(
                    "cannot create isolated workspace {}: {}",
                    workspace_root.display(),
                    e
                ),
            );
        }
        materialize_task(task, workspace_root);

        // The run's diagnostics must not sit inside the workspace the agent can read: an event log
        // discovered by `find`/`ls` is both a leak and a distraction (the tool trace of the first
        // real smoke showed both arms burning turns reading `.kroma-bench-state/<run>.jsonl`).
        // Keep it in a sibling directory, outside the agent's capability root.
        let state_dir = arm_state_dir(workspace_root);
        let env = match LocalHostEnv::new(workspace_root, state_dir.clone()) {
            Ok(env) => Arc::new(env),
            Err(e) => {
                return self.error_result(
                    task,
                    strategy,
                    started,
                    format!("cannot initialize workspace capability: {}", e),
                )
            }
        };

        let budget = BudgetConfig {
            max_model_turns: self.settings.max_model_turns,
            max_run_duration: self.settings.max_run_duration,
            // The arm's shared run-tree ceiling: parent + every child subagent draw from this ledger.
            max_total_tokens: Some(budget_tokens),
            ..BudgetConfig::default()
        };

        let registry = ToolRegistry::new(None);
        let policy = PolicyEngine::new(
            self.settings.policy_mode,
            Arc::new(NonInteractiveApprovalHandler),
        );
        // The capability envelope of this arm: `single_agent` hides `spawn_subagent` outright so a
        // "single agent" number can never include child runs.
        let max_subagents = strategy
            .max_subagents
            .unwrap_or(self.settings.max_subagents);
        let bash_allowed = self
            .settings
            .policy_mode
            .is_effect_enabled(crate::types::EffectClass::SideEffect, false);
        let mut visible_tools = registry.visible_definitions_for_engine(&policy);
        if max_subagents == 0 {
            visible_tools.retain(|t| t.name != "spawn_subagent");
        }
        let static_prefix = self.prompt_builder.build_static_prefix(&visible_tools);
        let runtime = build_default_runtime(env, policy, budget.clone(), None);

        let cfg = AgentRunConfig::new(task.prompt.clone(), budget)
            .with_projection_profile(strategy.resolved_projection_profile())
            // Capability gap #3 (`v1.5.2`): the arm-level causal-pruning ablation switch.
            .with_causal_pruning(strategy.causal_pruning)
            // Capability gap #6 (`v1.5.2`): the arm-level on-demand-schema ablation switch.
            .with_on_demand_tools(strategy.on_demand_tools);
        let run_id = cfg.run_id;
        let logger = match EventLogger::new(run_id, Some(state_dir), false) {
            Ok(logger) => logger,
            Err(e) => {
                return self.error_result(
                    task,
                    strategy,
                    started,
                    format!("cannot open bench event log: {}", e),
                )
            }
        };

        let (_cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        let runner = AgentRunner::new(cfg, Arc::clone(&self.provider), runtime, cancel_rx)
            .with_prompt_builder(self.prompt_builder.clone())
            .with_completion_gate(self.development_gate.clone().unwrap_or_else(|| Arc::new(
                crate::completion_gate::WorkspaceCompletionGate::with_auto_detection(
                    None, self.settings.verify_timeout, bash_allowed,
                ),
            )))
            .with_max_subagents(max_subagents)
            .with_adaptive_tournament_candidates(strategy.adaptive_tournament_candidates)
            // Capability gap #5 (`v1.5.2`): the arm-level strict-verification switch.
            .with_tournament_require_verified(strategy.require_verified);
        // Capability gap #2 (`v1.5.2`): the arm-level isolated-probe ablation switch. Enabling it
        // is the arm's declared, auditable consent for the bounded probe.
        let runner = if strategy.counterfactual_probe {
            runner.with_counterfactual_probe(
                crate::attribution::CounterfactualProbeConfig::enabled_with(2),
                Arc::new(crate::attribution::AllowProbes),
            )
        } else {
            runner
        };
        let (summary, logger) = runner.run(logger).await;

        // The workspace only exists now, so the recipe is bound to the arm's real directory here.
        let sandbox = self
            .settings
            .sandbox
            .as_ref()
            .map(|template| template.resolve(workspace_root));

        let evaluation = task
            .evaluate_with_sandbox(
                workspace_root,
                &VerifyCommandLimits {
                    timeout: self.settings.verify_timeout,
                    max_output_bytes: self.settings.verify_max_output_bytes,
                },
                sandbox.as_ref(),
            )
            .await;
        let mut mechanism = MechanismActivity::default();
        for event in logger.events() {
            if let crate::events::EventPayload::TournamentCompleted {
                adaptive, candidate_runs, verified_candidates, merged,
            } = &event.event {
                mechanism.adaptive_tournaments += usize::from(*adaptive);
                mechanism.candidate_runs += candidate_runs;
                mechanism.verified_candidates += verified_candidates;
                mechanism.merged_tournaments += usize::from(*merged);
            }
        }
        let self_tokens = summary.total_input_tokens + summary.total_output_tokens;
        let children_tokens = summary.children_input_tokens + summary.children_output_tokens;
        let self_input_tokens = summary.total_input_tokens;
        let self_cached_input_tokens = summary.total_cached_input_tokens;
        let self_output_tokens = summary.total_output_tokens;
        let children_input_tokens = summary.children_input_tokens;
        let children_cached_input_tokens = summary.children_cached_input_tokens;
        let children_output_tokens = summary.children_output_tokens;
        // The identity to price against is the one the provider returned, taken from this run's own
        // events. A request alias is not an identity, so a run that never reported one cannot be
        // priced at all.
        let reported_model = logger
            .events()
            .iter()
            .rev()
            .find_map(|event| match &event.event {
                crate::events::EventPayload::ModelRequestCompleted { reported_model, .. } => {
                    reported_model.clone()
                }
                _ => None,
            });
        let recorded_tokens = self_input_tokens
            .saturating_add(self_cached_input_tokens)
            .saturating_add(self_output_tokens)
            .saturating_add(children_input_tokens)
            .saturating_add(children_cached_input_tokens)
            .saturating_add(children_output_tokens);
        // Deliberately `None` when nothing was recorded: "no usage recorded" must not be priced as a
        // free run. A priced zero only comes from a provider that reported zeroes.
        let usage = (recorded_tokens > 0).then(|| crate::messages::TokenUsage {
            input_tokens: self_input_tokens.saturating_add(children_input_tokens),
            cached_input_tokens: Some(
                self_cached_input_tokens.saturating_add(children_cached_input_tokens),
            ),
            output_tokens: self_output_tokens.saturating_add(children_output_tokens),
            reasoning_tokens: None,
        });
        let cost = self
            .settings
            .price_table
            .as_ref()
            .map(|table| table.estimate(reported_model.as_deref(), usage));
        let total_tokens = self_tokens + children_tokens;
        // Completion is its own dimension: only a model stop means the run ended on its own.
        let finished_normally = matches!(
            summary.terminal_reason,
            crate::types::TerminalReason::ModelStop
        );

        let mut failure_trace = evaluation.failures.clone();
        failure_trace.push(format!("terminal_reason: {:?}", summary.terminal_reason));
        let budget_overage = total_tokens.saturating_sub(budget_tokens);
        if budget_overage > 0 {
            failure_trace.push(format!(
                "cap_violation: reported {} tokens > declared cap {} (overage {}; pre-dispatch \
                 estimates are byte-based, so a real provider usage can exceed the ledger)",
                total_tokens, budget_tokens, budget_overage
            ));
        }
        if !finished_normally {
            failure_trace.push(format!(
                "run_cut_off: {:?} (functional result may be a lower bound)",
                summary.terminal_reason
            ));
        }

        BenchArmResult {
            task_id: task.id.clone(),
            strategy: strategy.name.clone(),
            resolved: evaluation.resolved,
            gate_status: if evaluation.resolved {
                "approved".to_string()
            } else {
                "rejected".to_string()
            },
            self_tokens,
            children_tokens,
            total_tokens,
            self_input_tokens,
            self_cached_input_tokens,
            self_output_tokens,
            children_input_tokens,
            children_cached_input_tokens,
            children_output_tokens,
            reported_model,
            usage_basis: summary.usage_basis.clone(),
            estimated_cost_nano_usd: cost.as_ref().and_then(|estimate| estimate.estimated_nano_usd),
            cost_unknown_reason: cost.as_ref().and_then(|estimate| estimate.unknown_reason.clone()),
            budget_exceeded: budget_overage > 0,
            budget_tokens,
            budget_overage,
            cap_ok: budget_overage == 0,
            finished_normally,
            wall_time_ms: started.elapsed().as_millis() as u64,
            verify_time_ms: u64::try_from(evaluation.verify_duration.as_millis())
                .unwrap_or(u64::MAX),
            regressions: usize::from(task.regression_guard && !evaluation.resolved),
            repetition: 0,
            model: Some(self.settings.model.clone()),
            prompt_hash: Some(static_prefix.prompt_hash.clone()),
            tool_schema_hash: Some(static_prefix.tool_schema_hash.clone()),
            terminal_reason: Some(format!("{:?}", summary.terminal_reason)),
            failure_trace,
            max_subagents,
            bash_allowed,
            counterfactual_probe: strategy.counterfactual_probe,
            causal_pruning: strategy.causal_pruning,
            require_verified: strategy.require_verified,
            on_demand_tools: strategy.on_demand_tools,
            turns_used: summary.turns_used,
            tool_calls_used: summary.tool_calls_used,
            modified_files: summary.modified_files.clone(),
            final_text: summary.final_assistant_text.as_deref().map(truncate_text),
            tool_trace: tool_trace_from(&summary),
            mechanism: Some(mechanism),
        }
    }
}

/// Keep a diagnostics snippet bounded so a report cannot balloon with model prose.
fn truncate_text(text: &str) -> String {
    const LIMIT: usize = 400;
    let cleaned = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if cleaned.chars().count() <= LIMIT {
        cleaned
    } else {
        let head: String = cleaned.chars().take(LIMIT).collect();
        format!("{}…", head)
    }
}

/// Compact, bounded tool outcome trace: `name path -> ok|error(CODE)`. Keeping the tail is what
/// makes it possible to tell "the model never wrote anything" from "it wrote and then looped".
fn tool_trace_from(summary: &crate::agent_loop::RunSummary) -> Vec<String> {
    const MAX_ENTRIES: usize = 40;
    let mut trace: Vec<String> = summary
        .transcript
        .iter()
        .filter_map(|item| match item {
            crate::messages::TranscriptItem::ToolResult(result) => {
                let outcome = match &result.outcome {
                    crate::messages::ToolOutcome::Success => "ok".to_string(),
                    crate::messages::ToolOutcome::Error { code } => format!("error({:?})", code),
                };
                let path = result.meta.relative_path.as_deref().unwrap_or("");
                Some(format!("{} {} -> {}", result.tool_name, path, outcome))
            }
            _ => None,
        })
        .collect();
    if trace.len() > MAX_ENTRIES {
        let dropped = trace.len() - MAX_ENTRIES;
        trace = trace.split_off(dropped);
        trace.insert(0, format!("… {} earlier tool calls", dropped));
    }
    trace
}

impl BenchExecutor for ProviderBenchExecutor {
    fn label(&self) -> &'static str {
        PROVIDER_EXECUTOR_LABEL
    }

    fn metadata(&self) -> BenchRunMetadata {
        // Provenance is collected here so every provider-backed report carries it, whether or not
        // the caller passed an explicit commit. `settings.commit` still wins when given: an operator
        // building from a detached export may know the revision better than the working tree does.
        let source_root = self
            .settings
            .source_root
            .clone()
            .or_else(|| std::env::current_dir().ok());
        let source_fingerprint = source_root
            .as_deref()
            .map(crate::provenance::fingerprint_self);
        let commit = self
            .settings
            .commit
            .clone()
            .or_else(|| source_fingerprint.as_ref().and_then(|f| f.commit.clone()));
        BenchRunMetadata {
            model: Some(self.settings.model.clone()),
            protocol: Some(self.settings.protocol.clone()),
            provider_origin: self.settings.provider_origin.clone(),
            commit,
            prompt_hash: None,
            tool_schema_hash: None,
            suite_sha256: None,
            repetitions: 1,
            seed: DEFAULT_BENCH_SEED,
            randomized_order: true,
            source_fingerprint,
            price_table_version: self.settings.price_table.as_ref().map(|t| t.version.clone()),
            price_table_source: self.settings.price_table.as_ref().map(|t| t.source.clone()),
        }
    }

    fn execute<'a>(
        &'a self,
        task: &'a BenchTask,
        strategy: &'a BenchStrategyConfig,
        budget_tokens: u64,
        workspace_root: &'a Path,
    ) -> Pin<Box<dyn Future<Output = BenchArmResult> + Send + 'a>> {
        Box::pin(self.run_arm(task, strategy, budget_tokens, workspace_root))
    }
}

/// Diagnostics/state directory for one arm, kept *outside* the agent's workspace root.
fn arm_state_dir(workspace_root: &Path) -> std::path::PathBuf {
    match (workspace_root.parent(), workspace_root.file_name()) {
        (Some(parent), Some(name)) => parent.join(format!("{}__state", name.to_string_lossy())),
        _ => workspace_root.join(".kroma-bench-state"),
    }
}

/// Write the task's initial files into an (already isolated) per-arm directory.
fn materialize_task(task: &BenchTask, workspace_root: &Path) {
    for (path, contents) in &task.files {
        let full = workspace_root.join(path);
        if let Some(parent) = full.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(&full, contents);
    }
}

/// Extract `scheme://host[:port]` from a base URL, dropping credentials, path and query so a report
/// can name the endpoint without ever carrying a secret.
pub fn provider_origin_of(base_url: &str) -> Option<String> {
    let (scheme, rest) = base_url.split_once("://")?;
    if scheme.is_empty() {
        return None;
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = authority.rsplit('@').next().unwrap_or(authority);
    if host.is_empty() {
        return None;
    }
    Some(format!("{}://{}", scheme, host))
}

/// Wall-clock ceiling for the provenance `git` read. A repository probe that needs longer is not
/// worth delaying the report.
const GIT_COMMIT_TIMEOUT: Duration = Duration::from_secs(2);

/// Combined stdout+stderr byte cap for the provenance `git` read. `rev-parse HEAD` prints one
/// object id, so anything past this is a flood, not a commit.
const GIT_COMMIT_MAX_OUTPUT_BYTES: usize = 64 * 1024;

/// Detect the git commit of `dir` for report provenance.
///
/// Runs the fixed argv `git -C <dir> rev-parse HEAD` under the shared one-shot
/// [`ProcessSupervisor`], so the child gets a credential-scrubbed environment with
/// `GIT_TERMINAL_PROMPT=0` (no interactive credential prompt), a short timeout, and a small
/// combined output cap instead of the unbounded `.output()` it used to call. Returns `None` outside
/// a git work tree, when `git` is unavailable, when the command does not exit 0, when its capture
/// was cut short (timeout, flood, cancellation), or when the output is not a well-formed object id.
/// It never fails the run.
pub async fn detect_git_commit(dir: &Path) -> Option<String> {
    // `-C <dir>` resolves `dir` against the child's working directory, so keep it at the process
    // cwd exactly as the previous direct spawn inherited it (never at `dir`, which would resolve a
    // relative `dir` against itself).
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let spec = OneShotCommandSpec::new(
        vec![
            "git".to_string(),
            "-C".to_string(),
            dir.to_string_lossy().into_owned(),
            "rev-parse".to_string(),
            "HEAD".to_string(),
        ],
        cwd,
    )
    .with_timeout(GIT_COMMIT_TIMEOUT)
    .with_max_output_bytes(GIT_COMMIT_MAX_OUTPUT_BYTES)
    .with_env_policy(EnvironmentPolicy::SanitizedWithCustomNoBaseline(vec![(
        "GIT_TERMINAL_PROMPT".to_string(),
        "0".to_string(),
    )]));
    let result = ProcessSupervisor::execute_one_shot(spec, None).await.ok()?;
    if !result.is_success() {
        return None;
    }
    let commit = result.stdout.trim();
    if is_git_object_id(commit) {
        Some(commit.to_string())
    } else {
        None
    }
}

/// True when `value` is a full git object id: exactly 40 (SHA-1) or 64 (SHA-256) hex digits.
///
/// The report only ever carries a commit id, so anything a hostile or misconfigured `git` might
/// print instead — a credential, a token, a banner — is rejected rather than attached to the report.
fn is_git_object_id(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The built-in self-contained smoke suite (no `--suite` file required).
pub fn builtin_smoke_suite() -> Vec<BenchTask> {
    let mut tasks = Vec::new();
    for i in 1..=3 {
        tasks.push(BenchTask {
            id: format!("fixable-{}", i),
            prompt: format!("Fix the off-by-one bug in module {}.", i),
            files: BTreeMap::from([(
                format!("src/mod{}.rs", i),
                "fn compute() -> i32 { 0 }\n".to_string(),
            )]),
            expect_contains: vec![(format!("src/mod{}.rs", i), "fixed".to_string())],
            expect_absent: Vec::new(),
            regression_guard: false,
            verify_command: None,
        });
    }
    for i in 1..=3 {
        tasks.push(BenchTask {
            id: format!("stall-{}", i),
            prompt: format!(
                "The verification command keeps failing after the first fix attempt ({}).",
                i
            ),
            files: BTreeMap::from([(
                format!("src/stall{}.rs", i),
                "fn compute() -> i32 { 0 }\n".to_string(),
            )]),
            expect_contains: vec![(format!("src/stall{}.rs", i), "fixed".to_string())],
            expect_absent: Vec::new(),
            regression_guard: true,
            verify_command: None,
        });
    }
    tasks
}

/// The default arm pair used when a suite does not declare its own arms.
pub fn default_strategy_pair() -> Vec<BenchStrategyConfig> {
    vec![
        BenchStrategyConfig::single_agent(),
        BenchStrategyConfig::adaptive_tournament(3),
    ]
}

/// Load a benchmark suite from a JSON file.
///
/// Every workspace-relative path in `files`, `expect_contains` and `expect_absent` is validated up
/// front (task id + field name in the error) so a hostile suite cannot make an executor write or
/// read outside the scratch workspace. Failing here — before any arm runs — beats discovering the
/// escape at the first `fs::write`/`read_to_string`.
pub fn load_suite(path: &Path) -> Result<Vec<BenchTask>, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read benchmark suite {}: {}", path.display(), e))?;
    let tasks: Vec<BenchTask> = serde_json::from_str(&text)
        .map_err(|e| format!("invalid benchmark suite {}: {}", path.display(), e))?;
    for task in &tasks {
        validate_task_paths(task)
            .map_err(|e| format!("invalid benchmark suite {}: {}", path.display(), e))?;
    }
    Ok(tasks)
}

/// Load a held-out suite, refusing one the graded agent could edit.
///
/// A verifier that lives inside the workspace under test is not held out: the agent can rewrite the
/// oracle, or repoint a path the harness trusts, and the harness would then be grading the agent's
/// own replacement. This refuses both directions:
///  * the suite's resolved target inside the workspace, and
///  * the path as given inside the workspace (so a symlink placed in the workspace cannot be
///    repointed at a file the agent controls).
pub fn load_held_out_suite(path: &Path, workspace_root: &Path) -> Result<Vec<BenchTask>, String> {
    let workspace_canonical = std::fs::canonicalize(workspace_root).map_err(|e| {
        format!(
            "cannot resolve workspace {}: {}",
            workspace_root.display(),
            e
        )
    })?;
    let suite_canonical = std::fs::canonicalize(path)
        .map_err(|e| format!("cannot resolve benchmark suite {}: {}", path.display(), e))?;
    let given = absolute_lexical(path)?;
    let workspace_lexical = absolute_lexical(workspace_root)?;
    let inside_workspace = suite_canonical.starts_with(&workspace_canonical)
        || given.starts_with(&workspace_lexical);
    if inside_workspace {
        return Err(format!(
            "refusing held-out suite {}: it is inside the graded workspace {} — the agent could edit \
             the oracle; keep the suite in an agent-unwritable location",
            given.display(),
            workspace_lexical.display()
        ));
    }
    load_suite(&suite_canonical)
}

/// Make `path` absolute and collapse `.`/`..` lexically, without following symlinks. Used to compare
/// where a suite path *appears* to live, which is what a repointed symlink would move.
fn absolute_lexical(path: &Path) -> Result<std::path::PathBuf, String> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|e| format!("cannot determine the current directory: {}", e))?
            .join(path)
    };
    let mut normalized = std::path::PathBuf::new();
    for component in absolute.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    Ok(normalized)
}

/// Content fingerprint of a task set, including its oracles.
///
/// A report must be traceable to the *scoring procedure*, not only to the model and the source: two
/// runs of "the same benchmark" with different oracles are not comparable. Hashing the serialized
/// tasks covers the ids, the initial files, the post-conditions and the held-out `verify_command`,
/// so editing any of them changes the fingerprint.
pub fn suite_fingerprint(tasks: &[BenchTask]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    // `to_string` on the slice is deterministic for a given struct layout and field order.
    match serde_json::to_string(tasks) {
        Ok(canonical) => hasher.update(canonical.as_bytes()),
        Err(_) => hasher.update(b"unserializable"),
    }
    let digest = hasher.finalize();
    let mut out = String::new();
    for byte in digest.iter().take(8) {
        out.push_str(&format!("{:02x}", byte));
    }
    out
}

/// Validate every workspace-relative path a [`BenchTask`] names, rejecting absolute paths, parent
/// traversal, Windows drive/UNC prefixes, reserved device names and NTFS Alternate Data Streams.
fn validate_task_paths(task: &BenchTask) -> Result<(), String> {
    for path in task.files.keys() {
        WorkspaceCap::validate_relative_components(path).map_err(|e| {
            format!(
                "task '{}': invalid path in 'files' '{}': {}",
                task.id, path, e
            )
        })?;
    }
    for (path, _) in &task.expect_contains {
        WorkspaceCap::validate_relative_components(path).map_err(|e| {
            format!(
                "task '{}': invalid path in 'expect_contains' '{}': {}",
                task.id, path, e
            )
        })?;
    }
    for path in &task.expect_absent {
        WorkspaceCap::validate_relative_components(path).map_err(|e| {
            format!(
                "task '{}': invalid path in 'expect_absent' '{}': {}",
                task.id, path, e
            )
        })?;
    }
    if let Some(argv) = &task.verify_command {
        if argv.is_empty() || argv[0].trim().is_empty() {
            return Err(format!(
                "task '{}': 'verify_command' must be a non-empty argv",
                task.id
            ));
        }
        if argv.iter().any(|arg| arg.contains('\0')) {
            return Err(format!(
                "task '{}': 'verify_command' arguments must not contain NUL",
                task.id
            ));
        }
    }
    if let Some(defect) = task.oracle_defect() {
        return Err(format!("task '{}': {}", task.id, defect));
    }
    Ok(())
}

/// Run a paired, budget-locked comparison of `strategies` over `tasks`.
///
/// Each arm runs in its own scratch directory under `workspace_root` and receives the identical
/// `budget_tokens`, so the comparison is paired by construction. When `options.randomize_order` is
/// set, the arms of each `(task, repetition)` pair run in a seeded random order, so a warm cache or
/// a leftover file cannot systematically favour one arm.
pub async fn run_paired_benchmark(
    suite_name: &str,
    tasks: &[BenchTask],
    strategies: &[BenchStrategyConfig],
    budget_tokens: u64,
    workspace_root: &Path,
    executor: &dyn BenchExecutor,
    options: &BenchRunOptions,
) -> BenchComparisonReport {
    let repetitions = options.repetitions.max(1);
    let mut results = Vec::with_capacity(tasks.len() * strategies.len() * repetitions);
    for repetition in 0..repetitions {
        for task in tasks.iter() {
            let mut order: Vec<usize> = (0..strategies.len()).collect();
            if options.randomize_order {
                let seed = options.seed
                    ^ (hash_str(&task.id) << 1)
                    ^ (repetition as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
                let mut rng = BenchRng::new(seed);
                rng.shuffle(&mut order);
            }
            for &arm_index in &order {
                let strategy = &strategies[arm_index];
                let arm_root = workspace_root
                    .join(sanitize_arm_name(&strategy.name))
                    .join(format!("{}__r{}", sanitize_arm_name(&task.id), repetition));
                let _ = std::fs::remove_dir_all(&arm_root);
                let _ = std::fs::create_dir_all(&arm_root);
                let mut result = executor
                    .execute(task, strategy, budget_tokens, &arm_root)
                    .await;
                result.repetition = repetition;
                results.push(result);
            }
        }
    }

    let mut metadata = executor.metadata();
    // Derived from the tasks actually used, so it cannot drift from the suite on disk.
    metadata.suite_sha256 = Some(suite_fingerprint(tasks));
    metadata.repetitions = repetitions;
    metadata.seed = options.seed;
    metadata.randomized_order = options.randomize_order;
    if metadata.prompt_hash.is_none() {
        metadata.prompt_hash = results.iter().find_map(|r| r.prompt_hash.clone());
    }
    if metadata.tool_schema_hash.is_none() {
        metadata.tool_schema_hash = results.iter().find_map(|r| r.tool_schema_hash.clone());
    }

    let baseline = strategies.first().map(|s| s.name.clone());
    BenchComparisonReport::from_results(
        suite_name,
        budget_tokens,
        executor.label(),
        results,
        metadata,
        baseline,
        options.thresholds.as_ref(),
        &options.holdout_task_ids,
    )
}

fn sanitize_arm_name(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// FNV-1a over the task id: stable across processes and platforms, so a report can be reproduced
/// from `(suite, seed, repetitions)` alone.
fn hash_str(value: &str) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in value.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
    }
    hash
}

/// SplitMix64: tiny, deterministic and good enough for shuffling a handful of arms.
struct BenchRng(u64);

impl BenchRng {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, bound: usize) -> usize {
        if bound == 0 {
            0
        } else {
            (self.next_u64() % bound as u64) as usize
        }
    }

    fn shuffle<T>(&mut self, items: &mut [T]) {
        for i in (1..items.len()).rev() {
            let j = self.below(i + 1);
            items.swap(i, j);
        }
    }
}

/// Wilson score interval (95%) for a binomial proportion.
fn wilson_interval(successes: usize, total: usize) -> (f64, f64) {
    if total == 0 {
        return (0.0, 0.0);
    }
    const Z: f64 = 1.959_963_984_540_054;
    let n = total as f64;
    let phat = successes as f64 / n;
    let z2 = Z * Z;
    let denominator = 1.0 + z2 / n;
    let centre = phat + z2 / (2.0 * n);
    let margin = Z * ((phat * (1.0 - phat) / n) + z2 / (4.0 * n * n)).sqrt();
    (
        ((centre - margin) / denominator).max(0.0),
        ((centre + margin) / denominator).min(1.0),
    )
}

/// Paired per-`(task, repetition)` pass-rate difference of every non-baseline arm against the
/// baseline, with a 95% percentile bootstrap interval.
fn paired_deltas(
    results: &[BenchArmResult],
    baseline: Option<&str>,
    seed: u64,
    thresholds: Option<&DecisionThresholds>,
    holdout_task_ids: &[String],
) -> Vec<PairedComparison> {
    let baseline = match baseline {
        Some(name) => name.to_string(),
        None => match results.first() {
            Some(first) => first.strategy.clone(),
            None => return Vec::new(),
        },
    };
    let mut arms: Vec<String> = Vec::new();
    for r in results {
        if r.strategy != baseline && !arms.contains(&r.strategy) {
            arms.push(r.strategy.clone());
        }
    }
    let baseline_by_key: BTreeMap<(String, usize), bool> = results
        .iter()
        .filter(|r| r.strategy == baseline)
        .map(|r| ((r.task_id.clone(), r.repetition), r.resolved))
        .collect();
    // Costs are compared pairwise; a pair with an unknown estimate on either side makes the whole
    // cost comparison unknown rather than silently cheaper.
    let baseline_cost_by_key: BTreeMap<(String, usize), Option<u64>> = results
        .iter()
        .filter(|r| r.strategy == baseline)
        .map(|r| ((r.task_id.clone(), r.repetition), r.estimated_cost_nano_usd))
        .collect();

    arms.sort();
    let mut comparisons = Vec::new();
    for arm in arms {
        // Group by task: repetitions of one task are repeated measurements of the same thing, not
        // independent evidence, so they travel together in the resampling.
        let mut by_task: BTreeMap<String, Vec<f64>> = BTreeMap::new();
        for r in results.iter().filter(|r| r.strategy == arm) {
            if let Some(base) = baseline_by_key.get(&(r.task_id.clone(), r.repetition)) {
                by_task
                    .entry(r.task_id.clone())
                    .or_default()
                    .push(f64::from(r.resolved) - f64::from(*base));
            }
        }
        let observations: usize = by_task.values().map(Vec::len).sum();
        if observations == 0 {
            continue;
        }
        let mean = by_task
            .values()
            .flatten()
            .sum::<f64>()
            / observations as f64;
        let order: Vec<String> = by_task.keys().cloned().collect();
        let clusters: Vec<Vec<f64>> = by_task.values().cloned().collect();
        let cluster_count = clusters.len();
        let (ci_low, ci_high) = bootstrap_interval_clustered(&clusters, seed ^ hash_str(&arm));

        // Holdout-only statistics: the same estimator applied to the tasks that were *not* used while
        // iterating. Without a declared split there is nothing held out, and the report says so by
        // leaving `decision_basis` as "all tasks".
        let holdout = if holdout_task_ids.is_empty() {
            None
        } else {
            let mut holdout_by_task: BTreeMap<String, Vec<f64>> = BTreeMap::new();
            for (task_id, values) in order.iter().zip(clusters.iter()) {
                if holdout_task_ids.iter().any(|id| id == task_id) {
                    holdout_by_task.insert(task_id.clone(), values.clone());
                }
            }
            if holdout_by_task.is_empty() {
                None
            } else {
                let holdout_observations: usize = holdout_by_task.values().map(Vec::len).sum();
                let holdout_mean = holdout_by_task.values().flatten().sum::<f64>()
                    / holdout_observations as f64;
                let holdout_clusters: Vec<Vec<f64>> = holdout_by_task.into_values().collect();
                let (holdout_low, holdout_high) =
                    bootstrap_interval_clustered(&holdout_clusters, seed ^ hash_str(&arm) ^ 0x51ED);
                Some(SplitStats {
                    delta_pass_rate: holdout_mean,
                    ci_low: holdout_low,
                    ci_high: holdout_high,
                    paired_observations: holdout_observations,
                    cluster_count: holdout_clusters.len(),
                })
            }
        };

        let regression_failures: usize = results
            .iter()
            .filter(|result| result.strategy == arm)
            .map(|result| result.regressions)
            .sum();
        let cost_delta = cost_delta_nano_usd(results, &arm, &baseline_cost_by_key);

        // The verdict is taken on the holdout split whenever one exists: a development-set improvement
        // is a hypothesis, not evidence.
        let (basis_low, basis_high, basis_clusters, basis_label) = match &holdout {
            Some(stats) => (stats.ci_low, stats.ci_high, stats.cluster_count, "holdout"),
            None => (ci_low, ci_high, cluster_count, "all tasks"),
        };
        let (decision, rationale) = match thresholds {
            Some(thresholds) => {
                let (verdict, why) = decide(
                    &DecisionEvidence::new(basis_low, basis_high, basis_clusters)
                        .with_regression_failures(regression_failures)
                        .with_cost_delta(cost_delta),
                    thresholds,
                );
                (Some(verdict), Some(format!("[{}] {}", basis_label, why)))
            }
            None => (None, None),
        };
        comparisons.push(PairedComparison {
            baseline: baseline.clone(),
            arm,
            delta_pass_rate: mean,
            ci_low,
            ci_high,
            paired_observations: observations,
            cluster_count,
            decision,
            decision_basis: rationale.as_ref().map(|_| basis_label.to_string()),
            decision_rationale: rationale,
            holdout,
        });
    }
    comparisons
}

/// Judge one comparison against thresholds declared before the run.
///
/// `accept` requires the whole interval to clear the effect threshold; `reject` requires the whole
/// interval to fall below it; anything straddling the threshold is `inconclusive`, which is a real
/// outcome rather than a failed run.
fn decide(evidence: &DecisionEvidence, thresholds: &DecisionThresholds) -> (String, String) {
    let ci_low = &evidence.ci_low;
    let ci_high = &evidence.ci_high;
    let clusters = evidence.clusters;
    if clusters < thresholds.min_clusters.max(1) {
        return (
            "inconclusive".to_string(),
            format!(
                "only {} independent task(s); at least {} are required before any verdict",
                clusters,
                thresholds.min_clusters.max(1)
            ),
        );
    }
    // Safety before effect: breaking a guarded behaviour outranks any average improvement.
    if let Some(limit) = thresholds.max_regression_failures {
        if evidence.regression_failures > limit {
            return (
                "reject".to_string(),
                format!(
                    "{} regression-guard task(s) failed, above the preregistered limit of {}",
                    evidence.regression_failures, limit
                ),
            );
        }
    }
    if let Some(limit) = thresholds.max_cost_increase_nano_usd {
        match evidence.cost_delta_nano_usd {
            None => {
                return (
                    "inconclusive".to_string(),
                    "the cost increase is unknown, so affordability cannot be shown".to_string(),
                )
            }
            Some(delta) if delta > limit as i64 => {
                return (
                    "reject".to_string(),
                    format!(
                        "the mean estimated cost rises by {} nano-USD, above the preregistered limit of {}",
                        delta, limit
                    ),
                )
            }
            Some(_) => {}
        }
    }
    if *ci_low >= thresholds.min_effect {
        (
            "accept".to_string(),
            format!(
                "interval [{:.3}, {:.3}] clears the preregistered minimum effect {:.3}",
                ci_low, ci_high, thresholds.min_effect
            ),
        )
    } else if *ci_high < thresholds.min_effect {
        (
            "reject".to_string(),
            format!(
                "interval [{:.3}, {:.3}] lies below the preregistered minimum effect {:.3}",
                ci_low, ci_high, thresholds.min_effect
            ),
        )
    } else {
        (
            "inconclusive".to_string(),
            format!(
                "interval [{:.3}, {:.3}] straddles the preregistered minimum effect {:.3}",
                ci_low, ci_high, thresholds.min_effect
            ),
        )
    }
}

/// Mean estimated-cost difference (arm − baseline) in nano-USD, or `None` when it cannot be
/// computed: no comparable pairs, or any pair with an unknown estimate on either side.
fn cost_delta_nano_usd(
    results: &[BenchArmResult],
    arm: &str,
    baseline_cost_by_key: &BTreeMap<(String, usize), Option<u64>>,
) -> Option<i64> {
    let mut arm_total: i64 = 0;
    let mut baseline_total: i64 = 0;
    let mut pairs: usize = 0;
    for result in results.iter().filter(|result| result.strategy == arm) {
        let key = (result.task_id.clone(), result.repetition);
        let Some(base) = baseline_cost_by_key.get(&key) else {
            continue;
        };
        let (Some(arm_cost), Some(base_cost)) = (result.estimated_cost_nano_usd, *base) else {
            return None;
        };
        arm_total += arm_cost as i64;
        baseline_total += base_cost as i64;
        pairs += 1;
    }
    if pairs == 0 {
        return None;
    }
    Some((arm_total - baseline_total) / pairs as i64)
}

/// Percentile bootstrap over *clusters* rather than individual observations.
///
/// Resampling observations would treat repeated runs of one task as independent evidence and report
/// an interval that is too narrow. Drawing whole tasks with replacement (keeping all their
/// repetitions) keeps within-task correlation inside the unit that is actually uncertain.
fn bootstrap_interval_clustered(clusters: &[Vec<f64>], seed: u64) -> (f64, f64) {
    const RESAMPLES: usize = 2_000;
    if clusters.is_empty() {
        return (0.0, 0.0);
    }
    let mut rng = BenchRng::new(seed);
    let mut means = Vec::with_capacity(RESAMPLES);
    for _ in 0..RESAMPLES {
        let mut sum = 0.0;
        let mut count = 0usize;
        for _ in 0..clusters.len() {
            let picked = (rng.next_u64() as usize) % clusters.len();
            for value in &clusters[picked] {
                sum += value;
                count += 1;
            }
        }
        if count > 0 {
            means.push(sum / count as f64);
        }
    }
    percentile_interval(&mut means)
}

/// Percentile bootstrap interval for the mean of `values`. Deterministic in `seed` so a report can
/// be reproduced; 2000 resamples keeps a small suite fast.
#[allow(dead_code)]
fn bootstrap_interval(values: &[f64], seed: u64) -> (f64, f64) {
    const RESAMPLES: usize = 2_000;
    if values.is_empty() {
        return (0.0, 0.0);
    }
    let mut rng = BenchRng::new(seed);
    let mut means = Vec::with_capacity(RESAMPLES);
    for _ in 0..RESAMPLES {
        let mut sum = 0.0;
        for _ in 0..values.len() {
            sum += values[rng.below(values.len())];
        }
        means.push(sum / values.len() as f64);
    }
    percentile_interval(&mut means)
}

/// 95% percentile interval from a sorted-in-place vector of bootstrap means.
fn percentile_interval(means: &mut [f64]) -> (f64, f64) {
    if means.is_empty() {
        return (0.0, 0.0);
    }
    means.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let last = means.len() - 1;
    let low_index = ((means.len() as f64) * 0.025) as usize;
    let high_index = (((means.len() as f64) * 0.975) as usize).min(last);
    (means[low_index], means[high_index])
}

fn unknown_usage_basis() -> String {
    "unknown".to_string()
}

/// The default total budget (tokens) for one arm of a `kroma bench` run.
pub fn default_bench_budget_tokens() -> u64 {
    BudgetConfig::default().max_model_turns as u64 * 2_000
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Clustering is the whole point: repetitions of one task are not independent evidence, so
    /// resampling observations would understate the uncertainty of a claim about the mechanism.
    #[test]
    fn clustered_bootstrap_is_wider_when_repetitions_are_correlated() {
        // Three tasks, four identical repetitions each: within-task correlation is total, so the
        // only real uncertainty is which tasks were drawn.
        let clusters = vec![vec![1.0; 4], vec![1.0; 4], vec![0.0; 4]];
        let flat: Vec<f64> = clusters.iter().flatten().copied().collect();
        let (clustered_low, clustered_high) = bootstrap_interval_clustered(&clusters, 7);
        let (naive_low, naive_high) = bootstrap_interval(&flat, 7);
        let clustered_width = clustered_high - clustered_low;
        let naive_width = naive_high - naive_low;
        assert!(
            clustered_width > naive_width,
            "clustered width {clustered_width} must exceed the naive width {naive_width}"
        );
        assert!(clustered_width > 0.0, "a split must not produce a zero-width interval");
    }

    /// A verdict needs enough independent tasks, whatever the interval looks like.
    #[test]
    fn a_verdict_needs_enough_independent_tasks() {
        let thresholds = DecisionThresholds {
            min_effect: 0.5,
            min_clusters: 5,
            ..DecisionThresholds::default()
        };
        let (verdict, why) = decide(&DecisionEvidence::new(0.9, 1.0, 3), &thresholds);
        assert_eq!(verdict, "inconclusive");
        assert!(why.contains("3"), "{why}");

        // With enough tasks the interval decides, and the threshold is quoted back.
        assert_eq!(
            decide(&DecisionEvidence::new(0.9, 1.0, 6), &thresholds).0,
            "accept"
        );
        assert_eq!(
            decide(&DecisionEvidence::new(-1.0, -0.2, 6), &thresholds).0,
            "reject"
        );
        let (straddling, rationale) = decide(&DecisionEvidence::new(-0.1, 0.9, 6), &thresholds);
        assert_eq!(straddling, "inconclusive");
        assert!(rationale.contains("straddles"), "{rationale}");
    }

    /// Safety outranks effect size: a change that breaks a guarded behaviour is rejected even when
    /// the average improvement clears the effect threshold.
    #[test]
    fn the_safety_gate_outranks_a_clearing_effect() {
        let thresholds = DecisionThresholds {
            min_effect: 0.5,
            ..DecisionThresholds::default()
        };
        assert_eq!(
            decide(&DecisionEvidence::new(0.9, 1.0, 6), &thresholds).0,
            "accept"
        );
        let (verdict, why) = decide(
            &DecisionEvidence::new(0.9, 1.0, 6).with_regression_failures(1),
            &thresholds,
        );
        assert_eq!(verdict, "reject");
        assert!(why.contains("regression-guard"), "{why}");
    }

    /// The cost gate rejects an over-budget arm and — importantly — refuses to *accept* one whose
    /// cost is unknown: affordability cannot be assumed from missing data.
    #[test]
    fn the_cost_gate_rejects_over_budget_and_refuses_unknown_cost() {
        let thresholds = DecisionThresholds {
            min_effect: 0.5,
            max_cost_increase_nano_usd: Some(1_000),
            ..DecisionThresholds::default()
        };
        assert_eq!(
            decide(
                &DecisionEvidence::new(0.9, 1.0, 6).with_cost_delta(Some(500)),
                &thresholds
            )
            .0,
            "accept"
        );
        let (verdict, why) = decide(
            &DecisionEvidence::new(0.9, 1.0, 6).with_cost_delta(Some(5_000)),
            &thresholds,
        );
        assert_eq!(verdict, "reject");
        assert!(why.contains("cost rises"), "{why}");

        let (verdict, why) = decide(
            &DecisionEvidence::new(0.9, 1.0, 6).with_cost_delta(None),
            &thresholds,
        );
        assert_eq!(verdict, "inconclusive");
        assert!(why.contains("unknown"), "{why}");
    }

    #[tokio::test]
    async fn test_paired_benchmark_is_budget_locked_and_picks_a_winner() {
        let dir = std::env::temp_dir().join(format!("kroma-bench-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let tasks = builtin_smoke_suite();
        let strategies = default_strategy_pair();
        let budget = 8_000u64;
        let report = run_paired_benchmark(
            "builtin-smoke",
            &tasks,
            &strategies,
            budget,
            &dir,
            &SyntheticBenchExecutor,
            &BenchRunOptions::default(),
        )
        .await;

        // Every arm respected the shared budget.
        for r in &report.results {
            assert!(
                r.total_tokens <= budget,
                "arm {} task {} exceeded budget: {} > {}",
                r.strategy,
                r.task_id,
                r.total_tokens,
                budget
            );
        }
        assert_eq!(report.arms.len(), 2);
        // The tournament arm resolves the stall tasks, so it wins on pass rate.
        assert_eq!(report.winner.as_deref(), Some("adaptive_tournament_3"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_synthetic_executor_label_is_marked() {
        let dir = std::env::temp_dir().join(format!("kroma-bench2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let report = run_paired_benchmark(
            "builtin-smoke",
            &builtin_smoke_suite(),
            &default_strategy_pair(),
            8_000,
            &dir,
            &SyntheticBenchExecutor,
            &BenchRunOptions::default(),
        )
        .await;
        assert_eq!(report.executor, SYNTHETIC_EXECUTOR_LABEL);
        assert!(report.render().contains("synthetic executor"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A hostile suite must not be able to smuggle a workspace-escaping path through any of the
    /// three path-bearing fields; `load_suite` rejects it up front and names the task + field.
    #[test]
    fn test_load_suite_rejects_escaping_paths_in_every_field() {
        let dir = std::env::temp_dir().join(format!("kroma-bench-suite-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let cases: [(&str, &str); 6] = [
            (
                "files",
                r#"[{"id":"t1","prompt":"p","files":{"../escape.txt":"x"}}]"#,
            ),
            (
                "files",
                r#"[{"id":"t1","prompt":"p","files":{"/tmp/abs.txt":"x"}}]"#,
            ),
            (
                "expect_contains",
                r#"[{"id":"t2","prompt":"p","expect_contains":[["../escape.txt","x"]]}]"#,
            ),
            (
                "expect_contains",
                r#"[{"id":"t2","prompt":"p","expect_contains":[["/tmp/abs.txt","x"]]}]"#,
            ),
            (
                "expect_absent",
                r#"[{"id":"t3","prompt":"p","expect_absent":["../escape.txt"]}]"#,
            ),
            (
                "expect_absent",
                r#"[{"id":"t3","prompt":"p","expect_absent":["/tmp/abs.txt"]}]"#,
            ),
        ];
        for (index, (field, body)) in cases.iter().enumerate() {
            let path = dir.join(format!("suite-{}.json", index));
            std::fs::write(&path, body).unwrap();
            let err = load_suite(&path).expect_err("an escaping path must be rejected");
            assert!(err.contains("task '"), "error must name the task: {}", err);
            assert!(err.contains(field), "error must name the field: {}", err);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A legal suite still loads, materializes into its per-arm scratch directory and resolves.
    #[tokio::test]
    async fn test_load_suite_accepts_legal_paths_and_synthetic_arm_materializes_and_resolves() {
        let dir = std::env::temp_dir().join(format!("kroma-bench-legal-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let suite_path = dir.join("suite.json");
        std::fs::write(
            &suite_path,
            r#"[{"id":"fixable-legal","prompt":"p","files":{"src/mod.rs":"fn compute() -> i32 { 0 }\n"},"expect_contains":[["src/mod.rs","fixed"]],"expect_absent":["gone.txt"]}]"#,
        )
        .unwrap();
        let tasks = load_suite(&suite_path).expect("a legal suite must load");
        assert_eq!(tasks.len(), 1);
        let workspace_root = dir.join("scratch");
        let report = run_paired_benchmark(
            "legal",
            &tasks,
            &[BenchStrategyConfig::single_agent()],
            8_000,
            &workspace_root,
            &SyntheticBenchExecutor,
            &BenchRunOptions::default(),
        )
        .await;
        assert_eq!(report.results.len(), 1);
        assert!(
            report.results[0].resolved,
            "the synthetic fixable task must resolve"
        );
        assert!(
            workspace_root
                .join("single_agent")
                .join("fixable-legal__r0")
                .join("src/mod.rs")
                .exists(),
            "the legal task file must be materialized inside the arm scratch directory"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `is_resolved` is defense in depth: even a task built in code (bypassing `load_suite`) cannot
    /// use an escaping post-condition path to read outside the workspace.
    #[test]
    fn test_is_resolved_rejects_escaping_post_condition_paths() {
        let dir = std::env::temp_dir().join(format!("kroma-bench-resolve-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Plant a file outside the workspace that an escaping `expect_contains` would otherwise read.
        std::fs::write(dir.join("escape.txt"), "needle").unwrap();
        let workspace = dir.join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        let task = BenchTask {
            id: "hostile".to_string(),
            prompt: "p".to_string(),
            files: BTreeMap::new(),
            expect_contains: vec![("../escape.txt".to_string(), "needle".to_string())],
            expect_absent: vec!["/tmp/kroma-should-not-exist".to_string()],
            regression_guard: false,
            verify_command: None,
        };
        assert!(
            !task.is_resolved(&workspace),
            "an escaping post-condition must never count as resolved"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_provider_origin_strips_credentials_and_path() {
        assert_eq!(
            provider_origin_of("https://user:secret@api.example.com/v1?x=1"),
            Some("https://api.example.com".to_string())
        );
        assert_eq!(
            provider_origin_of("http://127.0.0.1:8080/v1"),
            Some("http://127.0.0.1:8080".to_string())
        );
        assert_eq!(provider_origin_of("not-a-url"), None);
    }
}
