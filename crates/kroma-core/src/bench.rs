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
//! The verdict is a *workspace-bound* static check: the arm's workspace is sealed
//! ([`crate::job_scope::seal_workspace`]) into a frozen copy, post-conditions are read from that copy
//! through a [`WorkspaceCap`] (no symlink component, no special file, bounded read) and the optional
//! `verify_command` runs in it under [`ProcessSupervisor`]. What the agent writes after the seal
//! changes its workspace, not the judged bytes. Static checks alone are still not OS isolation, and
//! the harness does not (and cannot, on the same uid) claim to resist an attacker that reaches the
//! sealed copy itself.
//!
//! [`Self::evaluate_with_sandbox`] closes that gap for hosts with a sandbox backend: the same
//! `verify_command` runs inside an OS sandbox (bubblewrap or a container runtime — see
//! [`crate::sandbox`]) whose only writable view is the scored workspace, whose network is off by
//! default, and whose host view is a read-only whitelist. It is opt-in: without a sandbox the
//! behaviour is exactly the B07 contract above. Fail-closed: an unavailable backend fails the
//! check and never degrades to a host run. The held-out suite itself still belongs outside the
//! agent's workspace (an external scorer location); that placement is the caller's job (B17).
//!
//! The command runs as a child of the arm's job: its process group is recorded in
//! [`crate::job_scope::ChildRegistry`] and reclaimed before the verdict is returned, so an arm whose
//! children will not die reports a `verdict_unknown` rather than a rejection.
//!
//! ## Small samples
//!
//! Repetitions + randomized arm order + an interval estimate are the minimum needed to keep a
//! paired comparison honest, but they are not a substitute for a large held-out suite. A handful of
//! tasks can show a large regression; it cannot establish a capability improvement. [`render`]
//! prints that caveat explicitly rather than declaring a winner "better".
//!
//! [`render`]: BenchComparisonReport::render

use crate::agent_loop::{build_runtime_with_registry, AgentRunConfig, AgentRunner};
use crate::budget::BudgetConfig;
use crate::context_projector::ProjectionProfile;
use crate::credential_broker;
use crate::env::LocalHostEnv;
use crate::events::EventLogger;
use crate::job_scope::JobScope;
use crate::prereg::PreregGate;
use crate::process_supervisor::{EnvironmentPolicy, OneShotCommandSpec, ProcessSupervisor};
use crate::prompt::PromptBuilder;
use crate::provider::ModelProvider;
use crate::sandbox::{ExecutionClass, ExecutionMode, ExecutionPolicy};
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
    /// inside the sealed snapshot of the task's isolated workspace, after the arm finishes. The task
    /// only resolves when the command exits `0` *and* every post-condition holds. `None` means the
    /// post-conditions are the whole verdict.
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
    /// Why this is *not a verdict*, when the harness could not reach one: the scored bytes could not
    /// be frozen ([`crate::job_scope::seal_workspace`]), or the held-out verification was stopped by
    /// the harness (timeout, cancellation, deadline, output flood, incomplete capture), or the arm's
    /// children could not be reclaimed ([`crate::job_scope::ChildRegistry::reclaim`]). `None` means
    /// `resolved` above is a measurement of the work; `Some` means no measurement was made and the
    /// caller must not report the run as a functional failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verdict_unknown: Option<String>,
}

impl TaskEvaluation {
    /// An arm whose bytes could not be frozen for scoring. The harness has no verdict, and says so
    /// instead of reporting the arm as rejected.
    fn unscorable(reason: String) -> Self {
        Self {
            resolved: false,
            checks: Vec::new(),
            failures: vec![format!("failed: {}", reason)],
            verify_duration: Duration::ZERO,
            verdict_unknown: Some(reason),
        }
    }
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

    /// The workspace-relative paths whose bytes the post-conditions judge.
    ///
    /// [`crate::job_scope::seal_workspace`] digests exactly these, so a scorer can prove the frozen
    /// copy holds the bytes it read. `expect_absent` has no bytes: it is decided by the path's
    /// absence from the sealed copy.
    pub fn scored_paths(&self) -> Vec<String> {
        self.expect_contains
            .iter()
            .map(|(path, _)| path.clone())
            .collect()
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
    /// The command is spawned as an argv (never through a shell) in the sealed snapshot of the
    /// task's isolated workspace, under the shared [`ProcessSupervisor`]: sanitized environment
    /// (host credentials stripped),
    /// its own process group (descendants swept after exit, timeout, deadline or cancellation), and
    /// a combined stdout+stderr cap. Display-only head/tail trimming (`output_trimmed`) never turns
    /// a successful exit into a failure; an incomplete capture (flood cap, drain cut, timeout) can
    /// never be reported as success.
    pub async fn evaluate_with_limits(
        &self,
        workspace: &Path,
        limits: &VerifyCommandLimits,
    ) -> TaskEvaluation {
        self.evaluate_with_sandbox(workspace, limits, None, None)
            .await
    }

    /// [`Self::evaluate_with_limits`] with an optional OS sandbox for the held-out
    /// `verify_command`.
    ///
    /// When `sandbox` is `Some`, the command runs inside that sandbox: the sandbox's
    /// `workspace_host` must be exactly the `workspace` being scored, so the guest `/workspace`
    /// view and the scored directory are the same bytes. Fail-closed: an unusable sandbox backend
    /// is a failure, never a host run (the supervisor is not called at all).
    ///
    /// When `children` is `Some`, the verify command is recorded in that job's
    /// [`crate::job_scope::ChildRegistry`] and the registry is reclaimed before the verdict is
    /// returned: a child that will not die means no verdict, not a clean rejection.
    pub async fn evaluate_with_sandbox(
        &self,
        workspace: &Path,
        limits: &VerifyCommandLimits,
        sandbox: Option<&crate::sandbox::SandboxSpec>,
        children: Option<&crate::job_scope::ChildRegistry>,
    ) -> TaskEvaluation {
        // No trusted oracle files: an isolated verifier may only use what the guest already has.
        self.evaluate_with_sandbox_and_oracle(workspace, limits, sandbox, children, &[])
            .await
    }

    /// [`Self::evaluate_with_sandbox`] plus the explicit trusted oracle capability: the only host
    /// files the held-out verifier may be given read-only access to. An isolated verifier that names
    /// an undeclared host path is unscorable, never a functional failure.
    pub async fn evaluate_with_sandbox_and_oracle(
        &self,
        workspace: &Path,
        limits: &VerifyCommandLimits,
        sandbox: Option<&crate::sandbox::SandboxSpec>,
        children: Option<&crate::job_scope::ChildRegistry>,
        trusted_verifier_files: &[crate::prereg::VerifierFileIdentity],
    ) -> TaskEvaluation {
        let (mut checks, mut failures) = self.check_post_conditions(workspace);
        let mut verify_duration = Duration::ZERO;
        let mut verdict_unknown = None;
        if let Some(argv) = &self.verify_command {
            if argv.is_empty() {
                failures.push("failed: empty verify_command argv".to_string());
            } else if let Some(reason) = limits.invalid_reason() {
                failures.push(format!(
                    "failed: invalid verify_command limits ({})",
                    reason
                ));
            } else {
                let label = format!("verify_command exit 0 for {:?}", argv);
                let started = Instant::now();
                let outcome =
                    match prepare_verifier_argv(argv, workspace, sandbox, trusted_verifier_files) {
                        Ok((argv, sandbox)) => {
                            run_verify_command(&argv, workspace, limits, sandbox.as_ref(), children)
                                .await
                        }
                        // The oracle could not be delivered, so nothing was measured: this is the same
                        // "no verdict" class as a harness-stopped command, never a functional failure.
                        Err(reason) => Err(VerifyFailure::Indeterminate(reason)),
                    };
                verify_duration = started.elapsed();
                match outcome {
                    Ok(()) => checks.push(format!("ok: {}", label)),
                    Err(VerifyFailure::Failed(reason)) => {
                        failures.push(format!("failed: {} ({})", label, reason))
                    }
                    Err(VerifyFailure::Indeterminate(reason)) => {
                        // The harness stopped the command, so this is "no verdict", not "the work
                        // failed": record both, and let the caller decide what an unknown means.
                        failures.push(format!("failed: {} ({})", label, reason));
                        verdict_unknown = Some(format!("verify_command {}", reason));
                    }
                }
            }
        }
        // The arm's children must be provably gone before the arm carries a verdict. A survivor is
        // recorded as an unknown verdict (with the pids), never as work that failed its checks.
        if let Some(children) = children {
            if let Err(error) = children.reclaim() {
                let reason = format!("children still alive after reclaim: {:?}", error);
                failures.push(format!("failed: reaping the arm's children ({:?})", error));
                verdict_unknown = Some(match verdict_unknown {
                    Some(previous) => format!("{}; {}", previous, reason),
                    None => reason,
                });
            }
        }
        TaskEvaluation {
            resolved: failures.is_empty(),
            checks,
            failures,
            verify_duration,
            verdict_unknown,
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

/// A held-out verification that did not succeed.
///
/// `Failed` is a verdict about the work (the command decided its own outcome). `Indeterminate` is not:
/// the harness stopped the command (timeout, cancellation, deadline, output flood, incomplete
/// capture), or could not run it at all, so nothing was measured either way.
enum VerifyFailure {
    Failed(String),
    Indeterminate(String),
}

/// R02: how many host files one held-out verifier may have delivered. An explicit bound keeps the
/// guest view of the oracle small and auditable instead of "mount whatever the argv mentions".
const MAX_VERIFIER_MOUNTS: usize = 8;

/// R02: guest directory the delivered verifier files appear under. Never the writable workspace and
/// never the trusted state directory.
const VERIFIER_GUEST_DIR: &str = "/kroma-oracle";

/// R02: one safe path component for a delivered verifier file.
fn verifier_guest_name(host: &Path) -> String {
    let raw = host
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("oracle");
    let safe: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .take(64)
        .collect();
    if safe.is_empty() || safe == "." || safe == ".." {
        "oracle".to_string()
    } else {
        safe
    }
}

/// R02: deliver the held-out verifier's own host files into an isolated guest, read-only, and rewrite
/// the argv to the guest paths.
///
/// An isolated verifier cannot see a host path outside the workspace, and the operator must not be
/// told to copy the oracle into the workspace: the agent can write there. Every absolute path named by
/// the argv is therefore mounted read-only under [`VERIFIER_GUEST_DIR`] and replaced by its guest path
/// (a path already inside the scored workspace is rewritten to the guest workspace view). Anything that
/// cannot be delivered — a missing file, a non-regular file, too many files — is returned as a reason
/// and classified as *no verdict*: the run measured nothing, so it must not be counted as a task the
/// candidate failed.
fn prepare_verifier_argv(
    argv: &[String],
    workspace: &Path,
    sandbox: Option<&crate::sandbox::SandboxSpec>,
    trusted: &[crate::prereg::VerifierFileIdentity],
) -> Result<(Vec<String>, Option<crate::sandbox::SandboxSpec>), String> {
    let Some(spec) = sandbox else {
        // A host run can already reach every host path; nothing to deliver.
        return Ok((argv.to_vec(), None));
    };
    let guest_workspace = PathBuf::from(&spec.workspace_guest);
    let mut prepared = spec.clone();
    let mut rewritten = Vec::with_capacity(argv.len());
    let mut delivered = 0usize;
    for token in argv {
        let path = Path::new(token);
        if !path.is_absolute() {
            // A bare program name resolves through the guest PATH, which the sandbox builds itself.
            rewritten.push(token.clone());
            continue;
        }
        if let Ok(relative) = path.strip_prefix(workspace) {
            rewritten.push(guest_workspace.join(relative).display().to_string());
            continue;
        }
        // Explicit authorization only: the token must name a file the operator declared through
        // `trusted_verifier_files`. No implicit "any absolute path" rule — that would hand the
        // verifier whatever a task file names (`/etc/shadow`, a home directory, a symlink).
        let authorized = trusted.iter().find(|declared| {
            std::fs::canonicalize(&declared.path)
                .ok()
                .zip(std::fs::canonicalize(path).ok())
                .is_some_and(|(declared, actual)| declared == actual)
        });
        let Some(declared) = authorized else {
            return Err(format!(
                "the held-out verifier needs '{}', which is not a declared trusted verifier file; declare it explicitly (trusted_verifier_files) or the run has no verdict",
                path.display()
            ));
        };
        // R02 (TOCTOU): the authorized bytes are the bytes that get mounted. A file that changed
        // after admission is refused as "no verdict", never silently substituted.
        declared.verify_unchanged()?;
        // A declared path must be a real file, reached without symlinks: a symlink target can change
        // between authorization and launch, which would deliver something the operator never named.
        let declared = &declared.path;
        let declared_meta = std::fs::symlink_metadata(declared).map_err(|error| {
            format!(
                "trusted verifier file '{}' is not available on this host ({error}); the oracle cannot be delivered, so this run has no verdict",
                declared.display()
            )
        })?;
        if declared_meta.file_type().is_symlink() || !declared_meta.is_file() {
            return Err(format!(
                "trusted verifier file '{}' must be a regular file reached without symlinks",
                declared.display()
            ));
        }
        let canonical = std::fs::canonicalize(declared).map_err(|error| {
            format!(
                "trusted verifier file '{}' cannot be resolved ({error})",
                declared.display()
            )
        })?;
        if canonical != *declared {
            return Err(format!(
                "trusted verifier file '{}' resolves to '{}'; declare the real path, not a symlink",
                declared.display(),
                canonical.display()
            ));
        }
        let path = canonical.as_path();
        if delivered >= MAX_VERIFIER_MOUNTS {
            return Err(format!(
                "the held-out verifier references more than {MAX_VERIFIER_MOUNTS} host files; the delivery bound is explicit"
            ));
        }
        let guest = format!(
            "{VERIFIER_GUEST_DIR}/{delivered}-{}",
            verifier_guest_name(path)
        );
        let mount = crate::sandbox::ReadOnlyMount::new(path, guest.clone())
            .map_err(|error| format!("verifier delivery rejected: {error}"))?;
        prepared.read_only_mounts.push(mount);
        delivered += 1;
        rewritten.push(guest);
    }
    Ok((rewritten, Some(prepared)))
}

/// Run one held-out verification command under the shared one-shot [`ProcessSupervisor`].
///
/// Returns `Err` for a non-zero exit, a spawn error, a timeout/deadline/cancellation, or an
/// incomplete capture (output flood or pipe-drain cut). A clean exit 0 whose *display* was trimmed
/// by the head/tail window is still `Ok`.
async fn run_verify_command(
    argv: &[String],
    workspace: &Path,
    limits: &VerifyCommandLimits,
    sandbox: Option<&crate::sandbox::SandboxSpec>,
    children: Option<&crate::job_scope::ChildRegistry>,
) -> Result<(), VerifyFailure> {
    let (argv, cwd) = match sandbox {
        Some(sandbox) => {
            // The scored directory and the sandbox view must be the same bytes; a mismatch is a
            // configuration error, not something to paper over.
            if sandbox.workspace_host != workspace {
                return Err(VerifyFailure::Indeterminate(format!(
                    "sandbox workspace '{}' is not the scored workspace '{}'",
                    sandbox.workspace_host.display(),
                    workspace.display()
                )));
            }
            // Fail-closed: an unusable backend fails the check instead of running on the host.
            sandbox
                .probe()
                .map_err(|e| VerifyFailure::Indeterminate(format!("sandbox unavailable: {}", e)))?;
            let wrapped = sandbox.wrap_argv(&argv[0], &argv[1..]).map_err(|e| {
                VerifyFailure::Indeterminate(format!("sandbox rejected the verify command: {}", e))
            })?;
            (wrapped, workspace.to_path_buf())
        }
        None => (argv.to_vec(), workspace.to_path_buf()),
    };
    let spec = OneShotCommandSpec::new(argv, cwd)
        .with_timeout(limits.timeout)
        .with_max_output_bytes(limits.max_output_bytes)
        .with_env_policy(EnvironmentPolicy::SanitizedMinimal);
    let spec = match children {
        Some(children) => spec.with_child_registry(children.clone()),
        None => spec,
    };
    let res = ProcessSupervisor::execute_one_shot(spec, None)
        .await
        .map_err(|e| VerifyFailure::Indeterminate(format!("spawn failed: {}", e)))?;
    if res.is_success() {
        return Ok(());
    }
    Err(describe_verify_failure(&res, limits))
}

/// Classify a supervised verify run that did not count as success.
fn describe_verify_failure(
    res: &crate::process_supervisor::OneShotExecutionResult,
    limits: &VerifyCommandLimits,
) -> VerifyFailure {
    use crate::process_supervisor::TerminationReason;
    if res.truncated && matches!(res.terminated_by, None | Some(TerminationReason::Exit)) {
        return VerifyFailure::Indeterminate(
            "incomplete capture (output closed before EOF)".to_string(),
        );
    }
    match res.terminated_by {
        Some(TerminationReason::Timeout) => {
            VerifyFailure::Indeterminate(format!("timed out after {:?}", limits.timeout))
        }
        Some(TerminationReason::OutputFlood) => VerifyFailure::Indeterminate(format!(
            "output exceeded the {} byte cap",
            limits.max_output_bytes
        )),
        Some(TerminationReason::Cancelled) => VerifyFailure::Indeterminate("cancelled".to_string()),
        Some(TerminationReason::Deadline) => {
            VerifyFailure::Indeterminate("run deadline exceeded".to_string())
        }
        _ => {
            let code = res
                .exit_code
                .map(|c| c.to_string())
                .unwrap_or_else(|| "signal".to_string());
            let stderr = res.stderr.trim().chars().take(400).collect::<String>();
            VerifyFailure::Failed(format!("exit={} stderr={}", code, stderr))
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
    /// The tool set this arm is allowed to see. `None` inherits the executor's registry; a list
    /// narrows it, so "the same agent with fewer schemas" is an arm rather than a build.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_filter: Option<Vec<String>>,
    /// E08: arm ablation — let a router decide the strategy from each turn's own tool activity.
    #[serde(default)]
    pub routing: bool,
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
            tool_filter: None,
            routing: false,
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
            tool_filter: None,
            routing: false,
        }
    }

    /// Override the subagent quota for this arm (`Some(0)` hides `spawn_subagent`).
    pub fn with_max_subagents(mut self, max_subagents: Option<usize>) -> Self {
        self.max_subagents = Some(max_subagents.unwrap_or(0));
        self
    }

    /// Resolve the arm's projection profile, `None` (kernel default = `P2`) when unset/unknown.
    ///
    /// M3-02: a `ctx.*` candidate composes onto `P2`, so an arm that names a candidate runs the
    /// candidate's base profile plus that candidate's stage.
    pub fn resolved_projection_profile(&self) -> Option<ProjectionProfile> {
        match self.resolved_projection_candidate() {
            Some(candidate) => Some(candidate.base_profile()),
            None => self
                .projection_profile
                .as_deref()
                .and_then(|value| ProjectionProfile::parse(value).ok()),
        }
    }

    /// M3-02: the `ctx.*` candidate this arm names, if any.
    pub fn resolved_projection_candidate(
        &self,
    ) -> Option<crate::context_projector::ProjectionCandidate> {
        self.projection_profile
            .as_deref()
            .and_then(|value| crate::context_projector::ProjectionCandidate::parse(value).ok())
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

/// A single completed *parent* model request, without prompt or raw response content.
///
/// Children and unknown/transmitted retries are deliberately NOT synthesized into this list:
/// a gap makes the run unpriceable, rather than silently billing the last model for every token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BenchRequestUsage {
    pub turn: usize,
    /// Digest of the actual wire request, not the prompt or headers themselves.
    pub request_identity_sha256: Option<String>,
    /// The provider's identity for THIS response. Never backfill from the requested alias.
    pub reported_model: Option<String>,
    /// `provider_reported` or the clearly labelled estimation source.
    pub usage_basis: String,
    pub input_tokens: u64,
    pub cached_input_tokens: Option<u64>,
    pub cache_creation_input_tokens: Option<u64>,
    pub output_tokens: u64,
    /// Operator-table estimate for this individual request, not an actual invoice.
    pub estimated_cost_nano_usd: Option<u64>,
    pub cost_unknown_reason: Option<String>,
}

/// Assign a single exact request an operator-table estimate, never applying an arm-level
/// default model to a different provider response or inventing missing cache read/write splits.
fn priced_request_usage(
    record: &crate::events::RequestUsageEvidence,
    price_table: Option<&crate::usage_cost::PriceTable>,
) -> BenchRequestUsage {
    let usage = crate::messages::TokenUsage {
        input_tokens: record.input_tokens,
        cached_input_tokens: record.cached_input_tokens,
        cache_creation_input_tokens: record.cache_creation_input_tokens,
        output_tokens: record.output_tokens,
        reasoning_tokens: None,
    };
    let measured = (record.usage_basis == "provider_reported"
        && record.cached_input_tokens.is_some()
        && record.cache_creation_input_tokens.is_some())
    .then_some(usage);
    let estimate =
        price_table.map(|table| table.estimate(record.reported_model.as_deref(), measured));
    BenchRequestUsage {
        turn: record.turn,
        request_identity_sha256: record.request_identity_sha256.clone(),
        reported_model: record.reported_model.clone(),
        usage_basis: record.usage_basis.clone(),
        input_tokens: record.input_tokens,
        cached_input_tokens: record.cached_input_tokens,
        cache_creation_input_tokens: record.cache_creation_input_tokens,
        output_tokens: record.output_tokens,
        estimated_cost_nano_usd: estimate.as_ref().and_then(|e| e.estimated_nano_usd),
        cost_unknown_reason: estimate.and_then(|e| e.unknown_reason),
    }
}

/// Validate the entire run-tree without trusting any single `complete` claim. In particular, an
/// interrupted child can have a zero-token outcome despite having transmitted a costly request.
fn children_ledger_complete(
    events: &[crate::events::DiagnosticEvent],
    children: &[BenchChildUsage],
    input_tokens: u64,
    cached_input_tokens: u64,
    output_tokens: u64,
) -> bool {
    use crate::events::EventPayload;
    use std::collections::BTreeSet;
    let mut spawned = BTreeSet::new();
    let mut finished = BTreeSet::new();
    let mut duplicate_event = false;
    let mut spawn_count = 0usize;
    let mut finish_count = 0usize;
    for ev in events {
        match &ev.event {
            EventPayload::SubagentSpawned { subagent_id, .. } => {
                spawn_count += 1;
                duplicate_event |= !spawned.insert(subagent_id.as_str());
            }
            EventPayload::SubagentCompleted { subagent_id, .. } => {
                finish_count += 1;
                duplicate_event |= !finished.insert(subagent_id.as_str());
            }
            _ => {}
        }
    }
    if duplicate_event
        || spawn_count != spawned.len()
        || finish_count != finished.len()
        || spawned != finished
        || spawned.len() != children.len()
    {
        return false;
    }
    let mut seen = BTreeSet::new();
    let mut sums = (0u64, 0u64, 0u64);
    for child in children {
        if !spawned.contains(child.subagent_id.as_str())
            || !seen.insert(&child.subagent_id)
            || !child.request_usage_complete
            || child.requests.len() != child.request_attempts_used
        {
            return false;
        }
        let mut itemized = (0u64, 0u64, 0u64);
        for request in &child.requests {
            if request.usage_basis != "provider_reported"
                || request.reported_model.is_none()
                || request.request_identity_sha256.is_none()
            {
                return false;
            }
            let Some(cached) = request.cached_input_tokens else {
                return false;
            };
            if request.cache_creation_input_tokens.is_none() {
                return false;
            }
            let Some(input) = itemized.0.checked_add(request.input_tokens) else {
                return false;
            };
            let Some(cached_total) = itemized.1.checked_add(cached) else {
                return false;
            };
            let Some(output) = itemized.2.checked_add(request.output_tokens) else {
                return false;
            };
            itemized = (input, cached_total, output);
        }
        if itemized
            != (
                child.input_tokens,
                child.cached_input_tokens.unwrap_or(u64::MAX),
                child.output_tokens,
            )
        {
            return false;
        }
        let Some(input) = sums.0.checked_add(child.input_tokens) else {
            return false;
        };
        let Some(cached) = sums
            .1
            .checked_add(child.cached_input_tokens.unwrap_or(u64::MAX))
        else {
            return false;
        };
        let Some(output) = sums.2.checked_add(child.output_tokens) else {
            return false;
        };
        sums = (input, cached, output);
    }
    sums == (input_tokens, cached_input_tokens, output_tokens)
}

/// BF-13: one child/candidate's independently measurable request attempts.
/// Even a losing/failed candidate is included. Missing attempts or incomplete evidence must
/// block a full-run price estimate; a requested tool's zero token summary is not an invoice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BenchChildUsage {
    pub subagent_id: String,
    pub request_attempts_used: usize,
    pub request_usage_complete: bool,
    pub input_tokens: u64,
    pub cached_input_tokens: Option<u64>,
    pub output_tokens: u64,
    pub requests: Vec<BenchRequestUsage>,
}

/// Result of running one arm on one task.
//
// Not `Eq`: the E02/E05 primary metrics below are `f64` ratios, and two runs agreeing on a float is
// not a property worth asserting. `PartialEq` is enough for the report's own comparisons.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    /// Completed parent requests as recorded in the event log. Excludes child requests and unknown
    /// wire attempts; consumers MUST check `request_usage_complete` before pricing.
    #[serde(default)]
    pub request_usage: Vec<BenchRequestUsage>,
    /// Parent request attempts transmitted, including retries whose usage was not returned.
    #[serde(default)]
    pub request_attempts_used: usize,
    /// Every parent transmitted attempt has an itemized, provider-reported usage record.
    /// Does NOT assert child usage completeness or trusted scorer isolation.
    #[serde(default)]
    pub request_usage_complete: bool,
    /// BF-13: child/tournament ledgers with actual wire model identities, including losers.
    #[serde(default)]
    pub child_request_usage: Vec<BenchChildUsage>,
    /// False if any child is missing, was interrupted, or cannot reconcile to parent totals.
    /// Old reports default to unverified, not to silently free subagent execution.
    #[serde(default)]
    pub child_usage_complete: bool,
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
    // ---- E02/E05 primary metrics: measured by the provider, or `null` when it did not measure ----
    // `null` is a measurement gap, not a zero: an arm the provider never reported cache reads for is
    // not a cold cache, and one it never reported TTFT for is not a fast first token.
    /// `sum(cached_input) / sum(input)` over this run's parent requests, from provider-reported
    /// usage only. `null` when no request carried a provider-measured input/cached split.
    #[serde(default)]
    pub cache_hit_ratio: Option<f64>,
    /// Mean time-to-first-token over the parent requests that reported `ttft_ms` (the field is
    /// optional in every provider protocol). `null` when no request reported one.
    #[serde(default)]
    pub ttft_ms_mean: Option<f64>,
    /// Slowest provider-reported `ttft_ms` in this run; `null` when none was reported.
    #[serde(default)]
    pub ttft_ms_max: Option<u64>,
    /// How many requests contributed to the two TTFT fields. `0` means TTFT was not measured.
    #[serde(default)]
    pub ttft_ms_samples: u64,
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
    /// Why this arm has **no verdict**, when the harness could not reach one: the scored bytes could
    /// not be frozen ([`TaskEvaluation::verdict_unknown`]), or the held-out verification was stopped
    /// by the harness rather than deciding its own outcome, or the arm's children would not die.
    /// `None` means `resolved` is a measurement of the work; `Some` means `resolved: false` is a
    /// placeholder and must not be counted as a functional failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verdict_unknown: Option<String>,
    // ---- capability envelope the arm actually ran with (never inferred, always recorded) ----
    /// Subagent spawn quota (`0` = `spawn_subagent` was hidden from the model).
    #[serde(default)]
    pub max_subagents: usize,
    /// `true` when a `bash` (shell) call would have been allowed under the arm's policy.
    #[serde(default)]
    pub bash_allowed: bool,
    /// R06: the tool names this arm actually exposed to the model (its filtered registry, minus
    /// `spawn_subagent` when the arm hides it). Recorded so a report can prove the declared
    /// `tool_filter` reached the real request instead of being a label.
    #[serde(default)]
    pub exposed_tools: Vec<String>,
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
    /// E08: what the router decided and why, turn by turn. Empty means no router was configured,
    /// which is not the same as "the router held".
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub route_decisions: Vec<crate::routing::RouteDecision>,
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

/// `sum(cached_input) / sum(input)` — the E02/E05 `cache_hit_ratio`, as one total per arm.
///
/// `None` when no request was measured. Zero would claim a measured cold cache, which is a
/// different fact from "the provider never reported a cache split".
fn pooled_cache_hit_ratio(input_tokens: u64, cached_input_tokens: u64) -> Option<f64> {
    (input_tokens > 0).then(|| cached_input_tokens as f64 / input_tokens as f64)
}

/// Cache totals over the parent requests a run retained, provider-reported usage only.
///
/// Child/tournament requests are recorded separately (`ChildRequestUsageRecorded`); folding them in
/// here would mix two different ledgers into one ratio.
fn provider_reported_cache_totals<'a>(
    requests: impl Iterator<Item = &'a BenchRequestUsage>,
) -> (u64, u64) {
    requests
        .filter(|request| request.usage_basis == "provider_reported")
        .filter_map(|request| {
            request
                .cached_input_tokens
                .map(|cached| (request.input_tokens, cached))
        })
        .fold(
            (0u64, 0u64),
            |(input, cached), (request_input, request_cached)| {
                (
                    input.saturating_add(request_input),
                    cached.saturating_add(request_cached),
                )
            },
        )
}

/// The E02/E05 primary metrics one run's `ModelRequestCompleted` events actually measured.
///
/// This sits next to the arm aggregation because both levels must apply the same rule: a request
/// that reported no cache split contributes nothing to the ratio, and a request that reported no
/// `ttft_ms` contributes no TTFT sample. Nothing is inferred from latency or from token totals.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct MeasuredMetrics {
    cache_hit_ratio: Option<f64>,
    ttft_ms_mean: Option<f64>,
    ttft_ms_max: Option<u64>,
    ttft_ms_samples: u64,
}

impl MeasuredMetrics {
    fn from_events(events: &[crate::events::DiagnosticEvent]) -> Self {
        use crate::events::EventPayload;
        let mut input_tokens = 0u64;
        let mut cached_input_tokens = 0u64;
        let mut ttft_sum_ms = 0u64;
        let mut ttft_ms_max: Option<u64> = None;
        let mut ttft_ms_samples = 0u64;
        for event in events {
            let EventPayload::ModelRequestCompleted {
                usage_basis,
                input_tokens: request_input,
                cached_input_tokens: request_cached,
                ttft_ms,
                ..
            } = &event.event
            else {
                continue;
            };
            if usage_basis.as_deref() == Some("provider_reported") {
                if let Some(cached) = request_cached {
                    input_tokens = input_tokens.saturating_add(*request_input);
                    cached_input_tokens = cached_input_tokens.saturating_add(*cached);
                }
            }
            if let Some(ttft) = ttft_ms {
                ttft_ms_samples += 1;
                ttft_sum_ms = ttft_sum_ms.saturating_add(*ttft);
                ttft_ms_max = Some(ttft_ms_max.map_or(*ttft, |max| max.max(*ttft)));
            }
        }
        Self {
            cache_hit_ratio: pooled_cache_hit_ratio(input_tokens, cached_input_tokens),
            ttft_ms_mean: (ttft_ms_samples > 0)
                .then(|| ttft_sum_ms as f64 / ttft_ms_samples as f64),
            ttft_ms_max,
            ttft_ms_samples,
        }
    }
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
    /// `sum(cached_input) / sum(input)` pooled over **every** request the arm sent, provider-reported
    /// usage only. `null` when no request was measured — never `0.0` for "unknown".
    #[serde(default)]
    pub cache_hit_ratio: Option<f64>,
    /// Mean TTFT over all request-level samples in the arm, weighted by sample count rather than
    /// averaged across runs; `null` when no request reported a TTFT.
    #[serde(default)]
    pub ttft_ms_mean: Option<f64>,
    /// Slowest provider-reported `ttft_ms` across the arm's runs; `null` when none was reported.
    #[serde(default)]
    pub ttft_ms_max: Option<u64>,
    /// Total request-level TTFT samples behind the two fields above (`0` = not measured).
    #[serde(default)]
    pub ttft_ms_samples: u64,
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
    /// R09: paired `(task, repetition)` observations excluded because at least one side had no
    /// verdict. They are not functional failures, and their presence makes the comparison
    /// `inconclusive` rather than letting the remaining pairs accept.
    #[serde(default)]
    pub unknown_verdict_pairs: usize,
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
    /// Candidates the tournament arm raced with, when the run had one. A preregistration names k,
    /// and `adaptive_tournament_2` and `adaptive_tournament_3` are different experiments, so the
    /// report has to carry which one actually ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tournament_candidates: Option<usize>,
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
    /// SHA-256 of the preregistration this run was admitted under (M2-01). `None` when the run was
    /// not gated, which is what the synthetic executor always is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prereg_sha256: Option<String>,
    /// Which preregistered suite this run scored: `"holdout"` or `"dev"`. Recorded next to the
    /// digest so a report can never be read as a holdout claim it did not make.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prereg_suite_role: Option<String>,
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
    /// The preregistration a paid run was admitted under. `None` for runs that were not gated; the
    /// gate itself is [`crate::prereg::check_prereg`], applied before the executor is built.
    pub prereg: Option<PreregGate>,
}

impl Default for BenchRunOptions {
    fn default() -> Self {
        Self {
            repetitions: 1,
            seed: DEFAULT_BENCH_SEED,
            randomize_order: true,
            thresholds: None,
            holdout_task_ids: Vec::new(),
            prereg: None,
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
            prereg: None,
        }
    }

    /// Record the preregistration this run was admitted under (see the field documentation).
    pub fn with_prereg(mut self, gate: PreregGate) -> Self {
        self.prereg = Some(gate);
        self
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
            // E02/E05: pool the arm from its request-level evidence, not from a mean of run means.
            // The cache ratio is one total over every provider-measured request; TTFT reuses each
            // run's (mean, samples) pair as sufficient statistics of the same samples.
            let (measured_input, measured_cached) =
                provider_reported_cache_totals(rs.iter().flat_map(|r| r.request_usage.iter()));
            let ttft_ms_samples: u64 = rs.iter().map(|r| r.ttft_ms_samples).sum();
            let ttft_ms_max = rs.iter().filter_map(|r| r.ttft_ms_max).max();
            let ttft_ms_mean = (ttft_ms_samples > 0).then(|| {
                rs.iter()
                    .filter_map(|r| r.ttft_ms_mean.map(|mean| mean * r.ttft_ms_samples as f64))
                    .sum::<f64>()
                    / ttft_ms_samples as f64
            });
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
                cache_hit_ratio: pooled_cache_hit_ratio(measured_input, measured_cached),
                ttft_ms_mean,
                ttft_ms_max,
                ttft_ms_samples,
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
        // An arm with no verdict is not a functional failure, and the pass rate below cannot tell
        // the two apart: say how many runs that affects instead of letting `resolved: false` speak.
        let unknown = self
            .results
            .iter()
            .filter(|result| result.verdict_unknown.is_some())
            .count();
        if unknown > 0 {
            caveats.push(format!(
                "{} of {} runs have no verdict: the sealed snapshot could not be frozen, or the \
                 held-out verification was stopped by the harness. Their `resolved: false` is a \
                 placeholder, not a functional failure, and they are still counted in the pass \
                 rates below.",
                unknown,
                self.results.len()
            ));
            // R09: the paired decision is gated on eligibility, so say so where a reader looks for
            // the verdict rather than only in the per-comparison rationale.
            let blocked = self
                .paired_deltas
                .iter()
                .filter(|delta| delta.unknown_verdict_pairs > 0)
                .count();
            if blocked > 0 {
                caveats.push(format!(
                    "{} paired comparison(s) contain observations with no verdict and are reported \
                     `inconclusive`: a pair is only evidence when both sides reached a verdict.",
                    blocked
                ));
            }
        }
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
        if let Some(sha256) = &self.metadata.prereg_sha256 {
            out.push_str(&format!(
                "prereg:  {} ({} suite)\n",
                sha256,
                self.metadata
                    .prereg_suite_role
                    .as_deref()
                    .unwrap_or("unknown")
            ));
        }
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
            .filter(|result| {
                result.usage_basis != "provider_reported" && result.usage_basis != "unknown"
            })
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
        // E02/E05 primary metrics. "not measured" is spelled out because a `0.000` ratio would read
        // as a measured cold cache and a `0ms` TTFT as an unbelievably fast first token.
        out.push_str("cache/ttft (not measured = the provider reported nothing):\n");
        for arm in &self.arms {
            out.push_str(&format!(
                "  {:<24} cache_hit_ratio={} ttft_ms={} ttft_ms_max={} samples={}\n",
                arm.strategy,
                match arm.cache_hit_ratio {
                    Some(ratio) => format!("{:.3}", ratio),
                    None => "not measured".to_string(),
                },
                match arm.ttft_ms_mean {
                    Some(mean) => format!("{:.1}", mean),
                    None => "not measured".to_string(),
                },
                match arm.ttft_ms_max {
                    Some(max) => format!("{}ms", max),
                    None => "not measured".to_string(),
                },
                arm.ttft_ms_samples,
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

    /// Execute one arm inside its job scope.
    ///
    /// The arm's workspace is `scope.path()`, and every child it starts belongs to the job: the
    /// verify command registers its process group in `scope.child_registry()` so the job can
    /// terminate and *check* it instead of assuming it died with its parent.
    fn execute<'a>(
        &'a self,
        task: &'a BenchTask,
        strategy: &'a BenchStrategyConfig,
        budget_tokens: u64,
        scope: &'a JobScope,
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
        scope: &'a JobScope,
    ) -> Pin<Box<dyn Future<Output = BenchArmResult> + Send + 'a>> {
        Box::pin(async move {
            let started = Instant::now();
            let workspace_root = scope.path();
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

            // M1-04-A2: freeze the arm's tree and judge the sealed copy, exactly like the provider
            // path, so the synthetic plumbing check exercises the seal instead of reading the live
            // workspace the "agent" just wrote.
            let sealed = crate::job_scope::seal_workspace(
                workspace_root,
                &arm_state_dir(workspace_root).join("sealed"),
                &task.scored_paths(),
            );
            let (resolved, verdict_unknown) = match &sealed {
                Ok(sealed) => (task.is_resolved(sealed.path()), None),
                Err(error) => (
                    false,
                    Some(format!(
                        "cannot seal the arm workspace for scoring: {:?}",
                        error
                    )),
                ),
            };

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
            match &verdict_unknown {
                Some(reason) => failure_trace.push(reason.clone()),
                None if !resolved => failure_trace
                    .push("synthetic executor: task did not meet its post-conditions".to_string()),
                None => {}
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
                request_usage: Vec::new(),
                request_attempts_used: 0,
                request_usage_complete: false,
                child_request_usage: Vec::new(),
                child_usage_complete: false,
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
                // No provider was involved, so no cache ratio and no TTFT were measured.
                cache_hit_ratio: None,
                ttft_ms_mean: None,
                ttft_ms_max: None,
                ttft_ms_samples: 0,
                repetition: 0,
                model: None,
                prompt_hash: None,
                tool_schema_hash: None,
                terminal_reason: Some("synthetic".to_string()),
                failure_trace,
                verdict_unknown,
                max_subagents: 0,
                bash_allowed: false,
                exposed_tools: Vec::new(),
                counterfactual_probe: strategy.counterfactual_probe,
                causal_pruning: strategy.causal_pruning,
                require_verified: strategy.require_verified,
                on_demand_tools: strategy.on_demand_tools,
                turns_used: 0,
                route_decisions: Vec::new(),
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
    /// arm's sealed workspace snapshot. `None` keeps the historical unsandboxed behaviour.
    ///
    /// R02: this is the legacy scorer-only knob. When [`Self::execution_policy`] is isolated, the
    /// policy's `Verifier` rule decides instead, so the scorer cannot disagree with the agent's
    /// boundary. This field is only consulted while the policy is host.
    pub sandbox: Option<crate::sandbox::SandboxTemplate>,
    /// R02: the operator-declared execution boundary for the whole arm — the agent's tool runtime
    /// (which every child subagent inherits), the development gate and the held-out verifier. It is
    /// installed by [`ProviderBenchExecutor::run_arm`]; `host()` is the historical behaviour.
    pub execution_policy: ExecutionPolicy,
    /// R02: the explicit **trusted oracle capability**. Each entry authorizes exactly one host file
    /// to be delivered read-only to the held-out verifier inside an isolated run, frozen to the bytes
    /// that were authorized. Nothing is delivered implicitly: an isolated verifier argv that names an
    /// absolute host path which is not declared here makes the arm unscorable (no verdict), and no
    /// file outside this list — a system file, a credential, a symlink — is ever mounted. A file
    /// whose bytes changed after it was authorized is refused the same way. Empty means "no external
    /// oracle file".
    pub trusted_verifier_files: Vec<crate::prereg::VerifierFileIdentity>,
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
            execution_policy: ExecutionPolicy::host(),
            trusted_verifier_files: Vec::new(),
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

    /// Run every held-out `verify_command` inside this sandbox, bound to each arm's sealed workspace
    /// snapshot.
    ///
    /// This is what turns the isolation from an available capability into the scored behaviour: an
    /// unusable backend fails the check instead of silently running on the host.
    pub fn with_sandbox(mut self, template: Option<crate::sandbox::SandboxTemplate>) -> Self {
        self.sandbox = template;
        self
    }

    /// R02: the operator-declared execution boundary for the whole arm. The agent runtime, the
    /// development gate and the held-out verifier all resolve this one value.
    pub fn with_execution_policy(mut self, policy: ExecutionPolicy) -> Self {
        self.execution_policy = policy;
        self
    }

    /// R02: authorize one host file for read-only delivery to the held-out verifier. This is the
    /// only way an external oracle file becomes reachable inside an isolated run, and the identity
    /// carries the digest that was authorized, so a later edit cannot widen or swap the capability.
    pub fn with_trusted_verifier_file(
        mut self,
        identity: crate::prereg::VerifierFileIdentity,
    ) -> Self {
        self.trusted_verifier_files.push(identity);
        self
    }

    /// R02: freeze `path` at build time and authorize it. Callers that already hold an admitted
    /// identity (the preregistration gate, or a `--verifier-file` argument) should pass it directly.
    pub fn with_trusted_verifier_path(
        self,
        path: impl AsRef<std::path::Path>,
    ) -> Result<Self, String> {
        let identity = crate::prereg::VerifierFileIdentity::freeze(path.as_ref())?;
        Ok(self.with_trusted_verifier_file(identity))
    }

    /// R02: the sandbox the held-out verifier must run under, resolved against `workspace`.
    ///
    /// An isolated policy is authoritative: the `Verifier` class decides whether the sandbox is
    /// required and whether it may reach the network. The legacy `sandbox` field only applies while
    /// the policy is host, so there is never a second source of truth about the scorer's boundary.
    pub(crate) fn verifier_sandbox(
        &self,
        workspace: &Path,
    ) -> Result<Option<crate::sandbox::SandboxSpec>, crate::sandbox::SandboxError> {
        match self.execution_policy.mode() {
            ExecutionMode::Isolated(template)
                if self
                    .execution_policy
                    .requires_sandbox(ExecutionClass::Verifier) =>
            {
                Ok(Some(
                    template
                        .resolve(workspace.to_path_buf())
                        .with_network(self.execution_policy.rule(ExecutionClass::Verifier).network),
                ))
            }
            _ => Ok(self
                .sandbox
                .as_ref()
                .map(|template| template.resolve(workspace.to_path_buf()))),
        }
    }
}

impl Default for ProviderBenchSettings {
    fn default() -> Self {
        Self::new("unknown", "auto")
    }
}

/// The record of an arm that never ran: it was never given a workspace of its own, or its run could
/// not be set up. It has no measured numbers and no verdict, and says so instead of looking like an
/// arm that ran and scored zero.
fn unstarted_arm_result(
    task: &BenchTask,
    strategy: &BenchStrategyConfig,
    wall_time_ms: u64,
    model: Option<&str>,
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
        request_usage: Vec::new(),
        request_attempts_used: 0,
        request_usage_complete: false,
        child_request_usage: Vec::new(),
        child_usage_complete: false,
        self_tokens: 0,
        children_tokens: 0,
        total_tokens: 0,
        budget_exceeded: false,
        budget_tokens: 0,
        budget_overage: 0,
        cap_ok: true,
        finished_normally: false,
        wall_time_ms,
        verify_time_ms: 0,
        regressions: usize::from(task.regression_guard),
        // The arm never produced a provider request, so neither metric was measured.
        cache_hit_ratio: None,
        ttft_ms_mean: None,
        ttft_ms_max: None,
        ttft_ms_samples: 0,
        repetition: 0,
        model: model.map(str::to_string),
        prompt_hash: None,
        tool_schema_hash: None,
        terminal_reason: Some("BenchSetupFailed".to_string()),
        failure_trace: vec![failure],
        // No verdict was attempted, so this is a setup failure and not an unknown measurement.
        verdict_unknown: None,
        max_subagents: 0,
        bash_allowed: false,
        exposed_tools: Vec::new(),
        counterfactual_probe: strategy.counterfactual_probe,
        causal_pruning: strategy.causal_pruning,
        require_verified: strategy.require_verified,
        on_demand_tools: strategy.on_demand_tools,
        turns_used: 0,
        route_decisions: Vec::new(),
        tool_calls_used: 0,
        modified_files: Vec::new(),
        final_text: None,
        tool_trace: Vec::new(),
        mechanism: None,
    }
}

/// The real executor: drives [`AgentRunner`] against a [`ModelProvider`] in an isolated per-arm
/// workspace, then evaluates the task itself.
///
/// The executor never writes the expected answer. It materializes only the task's *initial* files,
/// runs the agent with the task prompt, seals the resulting workspace and hands the frozen copy to
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
        unstarted_arm_result(
            task,
            strategy,
            started.elapsed().as_millis() as u64,
            Some(&self.settings.model),
            failure,
        )
    }

    async fn run_arm(
        &self,
        task: &BenchTask,
        strategy: &BenchStrategyConfig,
        budget_tokens: u64,
        scope: &JobScope,
    ) -> BenchArmResult {
        let started = Instant::now();
        let workspace_root = scope.path();

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
        // The sealed snapshot of this arm lives beside the event log, outside the workspace the
        // agent can read (M1-04-A2).
        let seal_dir = state_dir.join("sealed");
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

        // R05: every arm dispatches against its own broker ledger, sized to this arm's budget. A
        // single ledger sized to one arm (shared by all arms) let the first arm's spend reduce what
        // a later arm could send, so arm order could turn an owner ceiling into a functional loss.
        // The experiment-wide ceiling stays with the caller that declared it.
        let provider = credential_broker::enforce(
            Arc::clone(&self.provider),
            &format!("kroma-bench/{}", scope.job_id()),
            budget_tokens,
        );

        // An arm that names a tool set runs with exactly that set; the narrowing is recorded in the
        // result so a report cannot call two different exposures the same arm.
        let registry = ToolRegistry::new(strategy.tool_filter.clone());
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
        // R06: the tool set this arm actually exposed, recorded so a report cannot call two
        // different exposures the same arm.
        let exposed_tools: Vec<String> = visible_tools.iter().map(|t| t.name.clone()).collect();
        let static_prefix = self.prompt_builder.build_static_prefix(&visible_tools);
        // R06/R02: the filtered registry and the operator's execution boundary are installed on the
        // runtime the agent really runs with. Previously the filtered registry was discarded and a
        // fresh unfiltered host-policy runtime was built, so `tool_filter` was only a label and the
        // bench agent ran unsandboxed however the run was configured.
        let runtime = build_runtime_with_registry(registry, env, policy, budget.clone())
            .with_execution_policy(self.settings.execution_policy.clone());

        let cfg = AgentRunConfig::new(task.prompt.clone(), budget)
            .with_projection_profile(strategy.resolved_projection_profile())
            .with_projection_candidate(strategy.resolved_projection_candidate())
            // Capability gap #3 (`v1.5.2`): the arm-level causal-pruning ablation switch.
            .with_causal_pruning(strategy.causal_pruning)
            // Capability gap #6 (`v1.5.2`): the arm-level on-demand-schema ablation switch.
            .with_on_demand_tools(strategy.on_demand_tools);
        let cfg = if strategy.routing {
            cfg.with_routing(crate::routing::RoutingPolicy::default_policy())
        } else {
            cfg
        };
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
        let runner = AgentRunner::new(cfg, provider, runtime, cancel_rx)
            .with_prompt_builder(self.prompt_builder.clone())
            .with_completion_gate(self.development_gate.clone().unwrap_or_else(|| {
                Arc::new(
                    crate::completion_gate::WorkspaceCompletionGate::with_auto_detection(
                        None,
                        self.settings.verify_timeout,
                        bash_allowed,
                    ),
                )
            }))
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

        // M1-04-A2: judge the sealed snapshot, never the live workspace. The seal copies the arm's
        // tree out of the agent's workspace and verifies the copy against the digests taken first,
        // so bytes written after it — by a lingering child, the next arm, or the operator — change
        // the workspace, not the verdict. The sandbox is bound to the sealed tree for the same
        // reason: the held-out command must read the bytes that were sealed.
        let limits = VerifyCommandLimits {
            timeout: self.settings.verify_timeout,
            max_output_bytes: self.settings.verify_max_output_bytes,
        };
        let sealed =
            crate::job_scope::seal_workspace(workspace_root, &seal_dir, &task.scored_paths());
        let evaluation = match sealed {
            Ok(sealed) => match self.settings.verifier_sandbox(sealed.path()) {
                Ok(sandbox) => {
                    task.evaluate_with_sandbox_and_oracle(
                        sealed.path(),
                        &limits,
                        sandbox.as_ref(),
                        Some(scope.child_registry()),
                        &self.settings.trusted_verifier_files,
                    )
                    .await
                }
                // No usable boundary, no verdict: the held-out command must not fall back to the
                // host just because its sandbox could not be described.
                Err(error) => TaskEvaluation::unscorable(format!(
                    "cannot resolve the verifier execution boundary: {error}"
                )),
            },
            // No frozen bytes, no verdict: the arm keeps its measured numbers and reports that the
            // judgment never happened instead of turning a harness failure into a rejection.
            Err(error) => TaskEvaluation::unscorable(format!(
                "cannot seal the arm workspace for scoring: {:?}",
                error
            )),
        };
        let mut mechanism = MechanismActivity::default();
        for event in logger.events() {
            if let crate::events::EventPayload::TournamentCompleted {
                adaptive,
                candidate_runs,
                verified_candidates,
                merged,
            } = &event.event
            {
                mechanism.adaptive_tournaments += usize::from(*adaptive);
                mechanism.candidate_runs += candidate_runs;
                mechanism.verified_candidates += verified_candidates;
                mechanism.merged_tournaments += usize::from(*merged);
            }
        }
        // E02/E05 primary metrics: read straight off the provider-reported request events. Nothing
        // here is inferred — a run whose provider reported neither stays `None`.
        let measured = MeasuredMetrics::from_events(logger.events());
        let self_tokens = summary.total_input_tokens + summary.total_output_tokens;
        let children_tokens = summary.children_input_tokens + summary.children_output_tokens;
        let self_input_tokens = summary.total_input_tokens;
        let self_cached_input_tokens = summary.total_cached_input_tokens;
        let self_output_tokens = summary.total_output_tokens;
        let children_input_tokens = summary.children_input_tokens;
        let children_cached_input_tokens = summary.children_cached_input_tokens;
        let children_output_tokens = summary.children_output_tokens;
        // BF-11: retain a per-request, credential-free usage record, including cache *writes*.
        // The earlier implementation priced all parent + child tokens against the LAST reported
        // model and discarded cache creation, undercounting mixed-model and retry costs.
        let request_usage: Vec<BenchRequestUsage> = logger
            .events()
            .iter()
            .filter_map(|event| match &event.event {
                crate::events::EventPayload::ModelRequestCompleted {
                    turn,
                    usage_basis,
                    reported_model,
                    request_identity_sha256,
                    input_tokens,
                    cached_input_tokens,
                    cache_creation_input_tokens,
                    output_tokens,
                    ..
                } => Some(priced_request_usage(
                    &crate::events::RequestUsageEvidence {
                        turn: *turn,
                        request_identity_sha256: request_identity_sha256.clone(),
                        reported_model: reported_model.clone(),
                        usage_basis: usage_basis.as_deref().unwrap_or("unknown").to_string(),
                        input_tokens: *input_tokens,
                        cached_input_tokens: *cached_input_tokens,
                        cache_creation_input_tokens: *cache_creation_input_tokens,
                        output_tokens: *output_tokens,
                    },
                    self.settings.price_table.as_ref(),
                )),
                _ => None,
            })
            .collect();
        let child_request_usage: Vec<BenchChildUsage> = logger
            .events()
            .iter()
            .filter_map(|event| match &event.event {
                crate::events::EventPayload::ChildRequestUsageRecorded {
                    subagent_id,
                    request_attempts_used,
                    request_usage_complete,
                    input_tokens,
                    cached_input_tokens,
                    output_tokens,
                    requests,
                } => Some(BenchChildUsage {
                    subagent_id: subagent_id.clone(),
                    request_attempts_used: *request_attempts_used,
                    request_usage_complete: *request_usage_complete,
                    input_tokens: *input_tokens,
                    cached_input_tokens: *cached_input_tokens,
                    output_tokens: *output_tokens,
                    requests: requests
                        .iter()
                        .map(|request| {
                            priced_request_usage(request, self.settings.price_table.as_ref())
                        })
                        .collect(),
                }),
                _ => None,
            })
            .collect();
        let child_usage_complete = !logger.is_degraded()
            && logger.dropped_events() == 0
            && children_ledger_complete(
                logger.events(),
                &child_request_usage,
                children_input_tokens,
                children_cached_input_tokens,
                children_output_tokens,
            );
        let reported_model = request_usage
            .iter()
            .rev()
            .find_map(|request| request.reported_model.clone());
        let request_usage_complete = summary.request_attempts_used > 0
            && request_usage.len() == summary.request_attempts_used
            && summary.usage_basis == "provider_reported"
            && request_usage
                .iter()
                .all(|request| request.usage_basis == "provider_reported");
        // Only itemized, reconciled, provider-measured attempts may be priced. An interrupted
        // child or missing event keeps the *entire* arm unknown, even if the reported child
        // token totals are zero. The same rule covers losing Tournament candidates.
        let every_request_priced = request_usage
            .iter()
            .chain(
                child_request_usage
                    .iter()
                    .flat_map(|child| child.requests.iter()),
            )
            .all(|request| request.estimated_cost_nano_usd.is_some());
        let first_missing_rate = request_usage
            .iter()
            .chain(
                child_request_usage
                    .iter()
                    .flat_map(|child| child.requests.iter()),
            )
            .find_map(|request| request.cost_unknown_reason.clone());
        let cost_unknown_reason =
            if self.settings.price_table.is_none() {
                Some("operator price table not configured".to_string())
            } else if !request_usage_complete {
                Some("missing or estimated parent request attempt usage".to_string())
            } else if !child_usage_complete {
                Some(
                    "child/tournament request evidence missing, interrupted or unreconciled"
                        .to_string(),
                )
            } else if !every_request_priced {
                Some(first_missing_rate.unwrap_or_else(|| {
                    "one or more requests lack exact model/cache rates".to_string()
                }))
            } else {
                None
            };
        let estimated_cost_nano_usd = if cost_unknown_reason.is_none() {
            request_usage
                .iter()
                .chain(
                    child_request_usage
                        .iter()
                        .flat_map(|child| child.requests.iter()),
                )
                .try_fold(0u64, |sum, request| {
                    sum.checked_add(request.estimated_cost_nano_usd?)
                })
        } else {
            None
        };
        let cost_unknown_reason = cost_unknown_reason.or_else(|| {
            estimated_cost_nano_usd
                .is_none()
                .then(|| "per-request cost overflow".to_string())
        });
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
            estimated_cost_nano_usd,
            cost_unknown_reason,
            request_usage,
            request_attempts_used: summary.request_attempts_used,
            request_usage_complete,
            child_request_usage,
            child_usage_complete,
            budget_exceeded: budget_overage > 0,
            budget_tokens,
            budget_overage,
            cap_ok: budget_overage == 0,
            finished_normally,
            wall_time_ms: started.elapsed().as_millis() as u64,
            verify_time_ms: u64::try_from(evaluation.verify_duration.as_millis())
                .unwrap_or(u64::MAX),
            regressions: usize::from(task.regression_guard && !evaluation.resolved),
            cache_hit_ratio: measured.cache_hit_ratio,
            ttft_ms_mean: measured.ttft_ms_mean,
            ttft_ms_max: measured.ttft_ms_max,
            ttft_ms_samples: measured.ttft_ms_samples,
            repetition: 0,
            model: Some(self.settings.model.clone()),
            prompt_hash: Some(static_prefix.prompt_hash.clone()),
            tool_schema_hash: Some(static_prefix.tool_schema_hash.clone()),
            terminal_reason: Some(format!("{:?}", summary.terminal_reason)),
            failure_trace,
            verdict_unknown: evaluation.verdict_unknown.clone(),
            max_subagents,
            bash_allowed,
            exposed_tools,
            counterfactual_probe: strategy.counterfactual_probe,
            causal_pruning: strategy.causal_pruning,
            require_verified: strategy.require_verified,
            on_demand_tools: strategy.on_demand_tools,
            turns_used: summary.turns_used,
            tool_calls_used: summary.tool_calls_used,
            route_decisions: summary.route_decisions.clone(),
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
            tournament_candidates: None,
            seed: DEFAULT_BENCH_SEED,
            randomized_order: true,
            source_fingerprint,
            price_table_version: self
                .settings
                .price_table
                .as_ref()
                .map(|t| t.version.clone()),
            price_table_source: self.settings.price_table.as_ref().map(|t| t.source.clone()),
            prereg_sha256: None,
            prereg_suite_role: None,
        }
    }

    fn execute<'a>(
        &'a self,
        task: &'a BenchTask,
        strategy: &'a BenchStrategyConfig,
        budget_tokens: u64,
        scope: &'a JobScope,
    ) -> Pin<Box<dyn Future<Output = BenchArmResult> + Send + 'a>> {
        Box::pin(self.run_arm(task, strategy, budget_tokens, scope))
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
///
/// The tournament arm races three candidates, which is the shape the built-in smoke suite was
/// tuned against. A preregistration that names a different k has to ask for it explicitly, because
/// the candidate count changes both the cost and the diversity of the race.
pub fn default_strategy_pair() -> Vec<BenchStrategyConfig> {
    strategy_pair_with_tournament_candidates(3)
}

/// Parse an arm pair an experiment declares, instead of the two shapes the CLI can name.
///
/// The report compares every arm against the first one, so a declared pair must have at least two
/// distinct names. An empty or duplicated name would make the paired deltas ambiguous, and a
/// single arm would produce no delta at all — both are refused here rather than reported as a
/// comparison that never happened.
pub fn parse_arms(text: &str) -> Result<Vec<BenchStrategyConfig>, String> {
    let arms: Vec<BenchStrategyConfig> =
        serde_json::from_str(text).map_err(|e| format!("invalid arm declaration: {}", e))?;
    if arms.len() < 2 {
        return Err(format!(
            "an arm declaration needs at least two arms (got {})",
            arms.len()
        ));
    }
    let mut seen = std::collections::BTreeSet::new();
    for arm in &arms {
        if arm.name.trim().is_empty() {
            return Err("an arm has an empty name".to_string());
        }
        if !seen.insert(arm.name.clone()) {
            return Err(format!("two arms share the name '{}'", arm.name));
        }
    }
    Ok(arms)
}

/// Read a declared arm pair from disk.
pub fn load_arms(path: &Path) -> Result<Vec<BenchStrategyConfig>, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read arm declaration {}: {}", path.display(), e))?;
    parse_arms(&text).map_err(|e| format!("{}: {}", path.display(), e))
}

/// The same pair with an explicit tournament width.
pub fn strategy_pair_with_tournament_candidates(candidates: usize) -> Vec<BenchStrategyConfig> {
    vec![
        BenchStrategyConfig::single_agent(),
        BenchStrategyConfig::adaptive_tournament(candidates),
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
    let inside_workspace =
        suite_canonical.starts_with(&workspace_canonical) || given.starts_with(&workspace_lexical);
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
    // Everything under a strategy directory is a previous run's evidence: an arm workspace, its
    // event log, its sealed snapshot. Clearing it once here — before any arm runs — is the only
    // place a name may be reused, so two arms of *this* run can never wipe each other's directory
    // (M1-04-A1).
    for strategy in strategies {
        let _ = std::fs::remove_dir_all(workspace_root.join(sanitize_arm_name(&strategy.name)));
    }
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
                let arm_dir = workspace_root.join(sanitize_arm_name(&strategy.name));
                let job_id = format!("{}__r{}", sanitize_arm_name(&task.id), repetition);
                // The scope creates the workspace and refuses a name it did not create, so a
                // collision inside this run is caught here instead of silently running one arm in
                // another arm's directory.
                let scope = match JobScope::open(&arm_dir, &job_id) {
                    Ok(scope) => scope,
                    Err(error) => {
                        results.push(unstarted_arm_result(
                            task,
                            strategy,
                            0,
                            executor.metadata().model.as_deref(),
                            format!("cannot own an isolated workspace '{}': {:?}", job_id, error),
                        ));
                        continue;
                    }
                };
                let mut result = executor
                    .execute(task, strategy, budget_tokens, &scope)
                    .await;
                // The arm's tree is the run's evidence and the origin of its sealed snapshot: keep it
                // exactly where the report and the operator already look for it.
                scope.keep();
                result.repetition = repetition;
                results.push(result);
            }
        }
    }

    let mut metadata = executor.metadata();
    if let Some(prereg) = &options.prereg {
        metadata.prereg_sha256 = Some(prereg.sha256.clone());
        metadata.prereg_suite_role = Some(prereg.suite_role.as_str().to_string());
    }
    // Derived from the tasks actually used, so it cannot drift from the suite on disk.
    metadata.suite_sha256 = Some(suite_fingerprint(tasks));
    metadata.repetitions = repetitions;
    metadata.tournament_candidates = strategies
        .iter()
        .map(|s| s.adaptive_tournament_candidates)
        .find(|candidates| *candidates >= 2);
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
    // R09: a pair is only evidence when *both* sides reached a verdict. `resolved: false` with a
    // `verdict_unknown` reason is a placeholder for "the harness could not decide", not a failure,
    // so it must never enter the pass-rate difference as a zero.
    let baseline_by_key: BTreeMap<(String, usize), (bool, bool)> = results
        .iter()
        .filter(|r| r.strategy == baseline)
        .map(|r| {
            (
                (r.task_id.clone(), r.repetition),
                (r.resolved, r.verdict_unknown.is_none()),
            )
        })
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
        // Pairs where either side has no verdict. They are excluded from the effect estimate (an
        // unknown is not a functional failure) and they block a positive verdict: dropping them and
        // then claiming the remaining interval is exactly how a broken baseline becomes a win.
        let mut unknown_verdict_pairs = 0usize;
        for r in results.iter().filter(|r| r.strategy == arm) {
            if let Some((base_resolved, base_has_verdict)) =
                baseline_by_key.get(&(r.task_id.clone(), r.repetition))
            {
                if !base_has_verdict || r.verdict_unknown.is_some() {
                    unknown_verdict_pairs += 1;
                    continue;
                }
                by_task
                    .entry(r.task_id.clone())
                    .or_default()
                    .push(f64::from(r.resolved) - f64::from(*base_resolved));
            }
        }
        let observations: usize = by_task.values().map(Vec::len).sum();
        if observations == 0 && unknown_verdict_pairs == 0 {
            continue;
        }
        let mean = if observations == 0 {
            0.0
        } else {
            by_task.values().flatten().sum::<f64>() / observations as f64
        };
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
                let holdout_mean =
                    holdout_by_task.values().flatten().sum::<f64>() / holdout_observations as f64;
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
            // R09: fail-closed on eligibility before any effect is read. A pair with no verdict on
            // either side means the comparison is truncated, so the only honest machine-consumable
            // outcome is `inconclusive` — never `accept` on the remaining pairs. With no declared
            // thresholds there is no verdict to gate (the counts are still reported).
            Some(_) if unknown_verdict_pairs > 0 => (
                Some("inconclusive".to_string()),
                Some(format!(
                    "[{}] {} of {} paired observation(s) have no verdict (harness could not decide \
                     one side); the comparison is incomplete and cannot accept",
                    "all tasks",
                    unknown_verdict_pairs,
                    unknown_verdict_pairs + observations
                )),
            ),
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
        let decision_basis = match (&decision, unknown_verdict_pairs) {
            (Some(_), _) if unknown_verdict_pairs > 0 => Some("all tasks".to_string()),
            (Some(_), _) => Some(basis_label.to_string()),
            (None, _) => None,
        };
        comparisons.push(PairedComparison {
            baseline: baseline.clone(),
            arm,
            delta_pass_rate: mean,
            ci_low,
            ci_high,
            paired_observations: observations,
            cluster_count,
            unknown_verdict_pairs,
            decision,
            decision_basis,
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

/// R05: resolve the owner's ceiling for a whole experiment, separately from each arm's budget.
///
/// `per_arm_tokens` is what every arm is granted; `planned_arms` is the number of arms the run will
/// execute (`tasks × strategies × repetitions`). The owner may declare a larger ceiling
/// (`declared`), but not a smaller one: a ceiling that cannot cover the planned arms would deny
/// later arms mid-experiment, turning an owner budget limit into a functional result for whichever
/// arm ran last. That is refused before the first request instead.
pub fn experiment_envelope(
    per_arm_tokens: u64,
    planned_arms: usize,
    declared: Option<u64>,
) -> Result<u64, String> {
    let arms = u64::try_from(planned_arms)
        .map_err(|_| format!("planned arm count {planned_arms} does not fit in a token budget"))?;
    if arms == 0 {
        return Err("no arms are planned, so there is no experiment to authorize".to_string());
    }
    let required = per_arm_tokens.checked_mul(arms).ok_or_else(|| {
        format!("per-arm budget {per_arm_tokens} × {arms} planned arms overflows a token budget")
    })?;
    if required == 0 {
        return Err("the per-arm budget is zero, so no arm could dispatch".to_string());
    }
    match declared {
        Some(declared) if declared < required => Err(format!(
            "the declared experiment ceiling {declared} tokens cannot cover {arms} arm(s) × \
             {per_arm_tokens} tokens = {required}; raise --experiment-budget-tokens or lower \
             --budget-tokens"
        )),
        Some(declared) => Ok(declared),
        // No explicit total: the maximum the declared per-arm budgets can spend is the ceiling.
        None => Ok(required),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Clustering is the whole point: repetitions of one task are not independent evidence, so
    /// resampling observations would understate the uncertainty of a claim about the mechanism.
    #[test]
    fn test_an_arm_can_narrow_the_tool_set() {
        let arms = super::parse_arms(
            r#"[{"name":"full"},{"name":"minimal","tool_filter":["read","edit","write","bash"]}]"#,
        )
        .expect("a declared pair parses");
        assert_eq!(
            arms[0].tool_filter, None,
            "an arm without a filter inherits the registry"
        );
        assert_eq!(
            arms[1].tool_filter.as_deref().map(|set| set.len()),
            Some(4),
            "the narrowing is part of the arm, not of the build"
        );
    }

    #[test]
    fn test_an_arm_naming_a_candidate_runs_its_base_profile_plus_the_stage() {
        let taskstate = BenchStrategyConfig {
            name: "taskstate".to_string(),
            projection_profile: Some("ctx.taskstate.v1".to_string()),
            ..BenchStrategyConfig::single_agent()
        };
        assert_eq!(
            taskstate.resolved_projection_candidate(),
            Some(crate::context_projector::ProjectionCandidate::TaskStateV1)
        );
        assert_eq!(
            taskstate.resolved_projection_profile(),
            Some(ProjectionProfile::P2),
            "a candidate composes onto its base profile instead of replacing it"
        );

        // A legacy name stays a legacy profile and is never reported as a candidate.
        let legacy = BenchStrategyConfig {
            name: "p3".to_string(),
            projection_profile: Some("p3".to_string()),
            ..BenchStrategyConfig::single_agent()
        };
        assert_eq!(legacy.resolved_projection_candidate(), None);
        assert_eq!(
            legacy.resolved_projection_profile(),
            Some(ProjectionProfile::P3)
        );
    }

    #[test]
    fn test_parse_arms_requires_two_distinct_named_arms() {
        let pair = r#"[{"name":"direct"},{"name":"deferred","on_demand_tools":true}]"#;
        let arms = super::parse_arms(pair).expect("a declared pair parses");
        assert_eq!(arms.len(), 2);
        assert!(!arms[0].on_demand_tools);
        assert!(arms[1].on_demand_tools);

        // One arm has nothing to compare against, and a duplicate name makes the paired deltas
        // ambiguous: both are refused rather than reported as a comparison that never happened.
        assert!(super::parse_arms(r#"[{"name":"only"}]"#).is_err());
        assert!(super::parse_arms(r#"[{"name":"same"},{"name":"same"}]"#).is_err());
        assert!(super::parse_arms(r#"[{"name":""},{"name":"b"}]"#).is_err());
    }

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
        assert!(
            clustered_width > 0.0,
            "a split must not produce a zero-width interval"
        );
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
        // The arm's workspace survives the run (it is the evidence) and so does the snapshot it
        // was scored on, beside its event log rather than inside the tree the agent can write.
        assert!(
            workspace_root
                .join("single_agent")
                .join("fixable-legal__r0__state")
                .join("sealed")
                .join("src/mod.rs")
                .exists(),
            "the scored bytes must be the arm's sealed snapshot"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// M1-04-A1/A2 on disk: with 24 jobs every arm owns a directory of its own and is scored on its
    /// own sealed snapshot — no arm's workspace is another arm's, and the frozen copy holds the
    /// bytes that were judged.
    #[tokio::test]
    async fn every_arm_owns_its_workspace_and_a_separate_sealed_snapshot() {
        let dir = std::env::temp_dir().join(format!("kroma-bench-arms-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let tasks = builtin_smoke_suite();
        let strategies = default_strategy_pair();
        let report = run_paired_benchmark(
            "builtin-smoke",
            &tasks,
            &strategies,
            8_000,
            &dir,
            &SyntheticBenchExecutor,
            &BenchRunOptions::new(2, 7, false),
        )
        .await;
        assert_eq!(report.results.len(), 24, "6 tasks x 2 arms x 2 repetitions");

        for strategy in &strategies {
            for task in &tasks {
                for repetition in 0..2 {
                    let arm = dir.join(sanitize_arm_name(&strategy.name)).join(format!(
                        "{}__r{}",
                        sanitize_arm_name(&task.id),
                        repetition
                    ));
                    let sealed = arm_state_dir(&arm).join("sealed");
                    assert!(arm.is_dir(), "{} must exist", arm.display());
                    assert!(sealed.is_dir(), "{} must exist", sealed.display());
                    for (path, _) in &task.expect_contains {
                        assert_eq!(
                            std::fs::read(arm.join(path)).unwrap(),
                            std::fs::read(sealed.join(path)).unwrap(),
                            "{} was scored on its own sealed bytes",
                            arm.display()
                        );
                    }
                }
            }
        }
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

    /// M1-04-A2: the verdict is taken on the sealed snapshot, so a write that lands after the seal
    /// changes the workspace without changing what was scored.
    #[tokio::test]
    async fn scoring_reads_the_sealed_snapshot_not_the_live_workspace() {
        let dir = std::env::temp_dir().join(format!("kroma-bench-seal-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let workspace = dir.join("single_agent").join("fixable-1__r0");
        std::fs::create_dir_all(workspace.join("src")).unwrap();
        let task = BenchTask {
            id: "fixable-1".to_string(),
            prompt: "p".to_string(),
            files: BTreeMap::from([("src/lib.rs".to_string(), "fn f() {}\n".to_string())]),
            expect_contains: vec![("src/lib.rs".to_string(), "fixed".to_string())],
            expect_absent: Vec::new(),
            regression_guard: false,
            verify_command: None,
        };
        std::fs::write(workspace.join("src/lib.rs"), "fn f() { fixed }\n").unwrap();

        let sealed =
            crate::job_scope::seal_workspace(&workspace, &dir.join("sealed"), &task.scored_paths())
                .unwrap();

        // The agent — or a child it left behind — keeps writing after the seal.
        std::fs::write(workspace.join("src/lib.rs"), "fn f() {}\n").unwrap();

        assert!(
            task.evaluate(sealed.path()).await.resolved,
            "the sealed bytes are what is judged"
        );
        assert!(
            !task.is_resolved(&workspace),
            "the live workspace is a different fact, not the scored one"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// M1-04-A1: two task ids that sanitize to the same arm name must not share a workspace. The arm
    /// that cannot own its directory is reported instead of running in the other arm's directory.
    #[tokio::test]
    async fn colliding_arm_names_do_not_share_a_workspace() {
        let dir = std::env::temp_dir().join(format!("kroma-bench-collide-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let task = |id: &str| BenchTask {
            id: id.to_string(),
            prompt: "p".to_string(),
            files: BTreeMap::from([("own.txt".to_string(), String::new())]),
            expect_contains: vec![("own.txt".to_string(), "fixed".to_string())],
            expect_absent: Vec::new(),
            regression_guard: false,
            verify_command: None,
        };
        let report = run_paired_benchmark(
            "collision",
            &[task("fixable-1.2"), task("fixable-1:2")],
            &[BenchStrategyConfig::single_agent()],
            8_000,
            &dir,
            &SyntheticBenchExecutor,
            &BenchRunOptions::default(),
        )
        .await;

        assert_eq!(report.results.len(), 2, "no arm may be silently dropped");
        assert!(
            report.results[0].resolved,
            "the arm that owns the directory ran: {:?}",
            report.results[0]
        );
        let loser = &report.results[1];
        assert!(!loser.resolved);
        assert_eq!(loser.terminal_reason.as_deref(), Some("BenchSetupFailed"));
        assert!(
            loser
                .failure_trace
                .iter()
                .any(|f| f.contains("cannot own an isolated workspace")),
            "the collision must be named: {:?}",
            loser.failure_trace
        );
        // The owner's workspace was not wiped by the arm that could not get one.
        let kept =
            std::fs::read_to_string(dir.join("single_agent/fixable-1_2__r0/own.txt")).unwrap();
        assert!(
            kept.contains("fixed"),
            "the owner's bytes survive: {kept:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// M1-04-A3: a verification the harness had to stop is *not* a verdict. The run is still not
    /// resolved, but it is recorded as unknown rather than as work that failed its checks.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_stopped_verification_is_an_unknown_verdict_not_a_failure() {
        let dir = std::env::temp_dir().join(format!("kroma-bench-stop-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let task = |verify: Vec<String>| BenchTask {
            id: "stopped".to_string(),
            prompt: "p".to_string(),
            files: BTreeMap::new(),
            expect_contains: vec![("marker.txt".to_string(), "x".to_string())],
            expect_absent: Vec::new(),
            regression_guard: false,
            verify_command: Some(verify),
        };
        std::fs::write(dir.join("marker.txt"), "x").unwrap();

        let stopped = task(vec!["sleep".to_string(), "5".to_string()])
            .evaluate_with_timeout(&dir, Duration::from_millis(200))
            .await;
        assert!(!stopped.resolved);
        assert!(
            stopped.failures.iter().any(|f| f.contains("timed out")),
            "the stop is still reported: {:?}",
            stopped.failures
        );
        assert!(
            stopped
                .verdict_unknown
                .as_deref()
                .is_some_and(|reason| reason.contains("timed out")),
            "a stopped command is not a verdict: {:?}",
            stopped.verdict_unknown
        );

        // A command that decided its own outcome *is* a verdict, and stays a plain failure.
        let failed = task(vec![
            "sh".to_string(),
            "-c".to_string(),
            "exit 1".to_string(),
        ])
        .evaluate(&dir)
        .await;
        assert!(!failed.resolved);
        assert_eq!(
            failed.verdict_unknown, None,
            "exit 1 is a measurement, not a gap: {:?}",
            failed.failures
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

#[cfg(test)]
mod bf13_child_ledger_tests {
    use super::{children_ledger_complete, BenchChildUsage, BenchRequestUsage};
    use crate::events::{DiagnosticEvent, EventPayload, EVENT_SCHEMA_VERSION};
    use crate::types::RunId;

    fn event(payload: EventPayload) -> DiagnosticEvent {
        DiagnosticEvent {
            schema_version: EVENT_SCHEMA_VERSION.to_string(),
            run_id: RunId::new(),
            seq: 1,
            timestamp_ms: 0,
            event: payload,
        }
    }

    fn child(id: &str, input: u64, cached: u64, output: u64) -> BenchChildUsage {
        BenchChildUsage {
            subagent_id: id.to_string(),
            request_attempts_used: 1,
            request_usage_complete: true,
            input_tokens: input,
            cached_input_tokens: Some(cached),
            output_tokens: output,
            requests: vec![BenchRequestUsage {
                turn: 0,
                request_identity_sha256: Some("a".repeat(64)),
                reported_model: Some("provider-reported-model".to_string()),
                usage_basis: "provider_reported".to_string(),
                input_tokens: input,
                cached_input_tokens: Some(cached),
                cache_creation_input_tokens: Some(0),
                output_tokens: output,
                estimated_cost_nano_usd: None,
                cost_unknown_reason: None,
            }],
        }
    }

    fn lifecycle(id: &str) -> Vec<DiagnosticEvent> {
        vec![
            event(EventPayload::SubagentSpawned {
                subagent_id: id.to_string(),
                role: "worker".to_string(),
                max_turns: 12,
                isolated_worktree: true,
            }),
            event(EventPayload::SubagentCompleted {
                subagent_id: id.to_string(),
                role: "worker".to_string(),
                status: "failed".to_string(),
                turns_used: 1,
                tool_calls_used: 0,
                tokens_used: 8,
                duration_ms: 4,
            }),
        ]
    }

    #[test]
    fn bf13_ledger_accounts_for_failed_and_losing_children() {
        let mut events = lifecycle("tournament-c1");
        events.extend(lifecycle("tournament-c2"));
        let children = vec![
            child("tournament-c1", 5, 2, 3),
            child("tournament-c2", 6, 0, 4),
        ];
        assert!(children_ledger_complete(&events, &children, 11, 2, 7));
        assert!(!children_ledger_complete(&events, &children, 10, 2, 7));
        assert!(!children_ledger_complete(&events, &children[..1], 5, 2, 3));
    }

    #[test]
    fn bf13_ledger_fails_closed_on_missing_or_duplicate_attempts() {
        let events = lifecycle("sub-0001");
        let mut children = vec![child("sub-0001", 5, 2, 3)];
        children[0].request_attempts_used = 2;
        assert!(!children_ledger_complete(&events, &children, 5, 2, 3));
        children[0].request_attempts_used = 1;
        children[0].requests[0].cached_input_tokens = None;
        assert!(!children_ledger_complete(&events, &children, 5, 2, 3));
        children[0].requests[0].cached_input_tokens = Some(2);
        let mut duplicated = events.clone();
        duplicated.extend(lifecycle("sub-0001"));
        assert!(!children_ledger_complete(&duplicated, &children, 5, 2, 3));
        assert!(children_ledger_complete(&[], &[], 0, 0, 0));
        assert!(!children_ledger_complete(&[], &[], 1, 0, 0));
    }
}

#[cfg(test)]
mod e02e05_metric_tests {
    use super::{
        BenchArmResult, BenchComparisonReport, BenchRequestUsage, BenchRunMetadata, MeasuredMetrics,
    };
    use crate::events::{DiagnosticEvent, EventPayload, EVENT_SCHEMA_VERSION};
    use crate::types::RunId;

    fn request_event(
        turn: usize,
        usage_basis: Option<&str>,
        input_tokens: u64,
        cached_input_tokens: Option<u64>,
        ttft_ms: Option<u64>,
    ) -> DiagnosticEvent {
        DiagnosticEvent {
            schema_version: EVENT_SCHEMA_VERSION.to_string(),
            run_id: RunId::new(),
            seq: u64::try_from(turn).unwrap(),
            timestamp_ms: 0,
            event: EventPayload::ModelRequestCompleted {
                turn,
                usage_basis: usage_basis.map(str::to_string),
                reported_model: Some("measured-model".to_string()),
                model_identity_matches: None,
                token_accounting: None,
                input_tokens,
                cached_input_tokens,
                cache_creation_input_tokens: Some(0),
                uncached_input_tokens: input_tokens
                    .saturating_sub(cached_input_tokens.unwrap_or(0)),
                output_tokens: 10,
                reasoning_tokens: None,
                prefix_cache_hit_ratio: 0.0,
                request_identity_sha256: Some("b".repeat(64)),
                latency_ms: 500,
                ttft_ms,
            },
        }
    }

    fn request(input_tokens: u64, cached_input_tokens: Option<u64>) -> BenchRequestUsage {
        BenchRequestUsage {
            turn: 0,
            request_identity_sha256: Some("a".repeat(64)),
            reported_model: Some("measured-model".to_string()),
            usage_basis: if cached_input_tokens.is_some() {
                "provider_reported".to_string()
            } else {
                "byte_estimate_not_measured".to_string()
            },
            input_tokens,
            cached_input_tokens,
            cache_creation_input_tokens: None,
            output_tokens: 1,
            estimated_cost_nano_usd: None,
            cost_unknown_reason: None,
        }
    }

    /// A result as a report written *before* the E02/E05 fields existed: only the fields that have
    /// no serde default. Doubles as the back-compat fixture and as a minimal base for assertions.
    fn legacy_result(task_id: &str, strategy: &str) -> BenchArmResult {
        serde_json::from_value(serde_json::json!({
            "task_id": task_id,
            "strategy": strategy,
            "resolved": true,
            "gate_status": "approved",
            "self_tokens": 0,
            "children_tokens": 0,
            "total_tokens": 0,
            "wall_time_ms": 1,
            "regressions": 0,
        }))
        .expect("a result written before the E02/E05 fields must still deserialize")
    }

    fn report_from(results: Vec<BenchArmResult>) -> BenchComparisonReport {
        BenchComparisonReport::from_results(
            "e02-e05-fixture",
            10_000,
            "synthetic",
            results,
            BenchRunMetadata::default(),
            None,
            None,
            &[],
        )
    }

    /// The run-level numbers come from the provider-reported request events: one summed ratio, and
    /// TTFT over the requests that actually reported it.
    #[test]
    fn measured_run_metrics_sum_provider_reported_usage_and_ttft() {
        let events = vec![
            request_event(0, Some("provider_reported"), 1_000, Some(800), Some(120)),
            request_event(1, Some("provider_reported"), 1_000, Some(400), Some(40)),
            // Estimated usage with no TTFT contributes to neither number.
            request_event(2, None, 9_000, None, None),
        ];
        let measured = MeasuredMetrics::from_events(&events);
        // (800 + 400) / (1 000 + 1 000); the estimated 9 000-token request is not counted.
        assert_eq!(measured.cache_hit_ratio, Some(0.6));
        // (120 + 40) / 2 samples; the 500ms latency of the third request is not used as a proxy.
        assert_eq!(measured.ttft_ms_mean, Some(80.0));
        assert_eq!(measured.ttft_ms_max, Some(120));
        assert_eq!(measured.ttft_ms_samples, 2);
    }

    /// No provider-reported usage and no reported TTFT is *unmeasured*, not zero: the JSON says
    /// `null`, and the arm roll-up inherits the same rule instead of reporting `0.0`.
    #[test]
    fn unmeasured_cache_and_ttft_serialize_as_null() {
        let unmeasured = MeasuredMetrics::from_events(&[
            request_event(0, None, 5_000, None, None),
            request_event(1, Some("byte_estimate_not_measured"), 5_000, Some(0), None),
        ]);
        assert_eq!(unmeasured.cache_hit_ratio, None);
        assert_eq!(unmeasured.ttft_ms_mean, None);
        assert_eq!(unmeasured.ttft_ms_max, None);
        assert_eq!(unmeasured.ttft_ms_samples, 0);

        let mut result = legacy_result("task-0", "cached");
        result.cache_hit_ratio = unmeasured.cache_hit_ratio;
        result.ttft_ms_mean = unmeasured.ttft_ms_mean;
        result.ttft_ms_max = unmeasured.ttft_ms_max;
        result.ttft_ms_samples = unmeasured.ttft_ms_samples;
        let json = serde_json::to_value(&result).expect("a result serializes");
        assert!(json["cache_hit_ratio"].is_null(), "{json}");
        assert!(json["ttft_ms_mean"].is_null(), "{json}");
        assert!(json["ttft_ms_max"].is_null(), "{json}");
        assert_eq!(json["ttft_ms_samples"], serde_json::json!(0));

        let report = report_from(vec![result]);
        let arm = &report.arms[0];
        assert_eq!(arm.cache_hit_ratio, None);
        assert_eq!(arm.ttft_ms_mean, None);
        assert_eq!(arm.ttft_ms_max, None);
        assert_eq!(arm.ttft_ms_samples, 0);
    }

    /// A report written before these fields existed still deserializes, and reads as "not measured"
    /// rather than "measured zero".
    #[test]
    fn legacy_report_without_metric_fields_still_deserializes() {
        let legacy_report = serde_json::json!({
            "suite": "legacy-suite",
            "budget_tokens": 10_000,
            "executor": "synthetic",
            "results": [{
                "task_id": "task-0",
                "strategy": "baseline",
                "resolved": true,
                "gate_status": "approved",
                "self_tokens": 900,
                "children_tokens": 0,
                "total_tokens": 900,
                "wall_time_ms": 12,
                "regressions": 0,
            }],
            "arms": [{
                "strategy": "baseline",
                "tasks": 1,
                "resolved": 1,
                "pass_rate": 1.0,
                "pass_rate_ci_low": 0.207,
                "pass_rate_ci_high": 1.0,
                "total_tokens": 900,
                "wall_time_ms": 12,
                "regressions": 0,
                "cap_ok": 1,
                "budget_overage_tokens": 0,
                "finished_normally": 1,
            }],
            "winner": "baseline",
        });
        let report: BenchComparisonReport = serde_json::from_value(legacy_report)
            .expect("a report written before the E02/E05 fields must still deserialize");
        assert_eq!(report.results[0].cache_hit_ratio, None);
        assert_eq!(report.results[0].ttft_ms_mean, None);
        assert_eq!(report.results[0].ttft_ms_max, None);
        assert_eq!(report.results[0].ttft_ms_samples, 0);
        assert_eq!(report.arms[0].cache_hit_ratio, None);
        assert_eq!(report.arms[0].ttft_ms_mean, None);
        assert_eq!(report.arms[0].ttft_ms_max, None);
        assert_eq!(report.arms[0].ttft_ms_samples, 0);
    }

    /// The arm is pooled from request-level evidence: one ratio over all requests, and a
    /// sample-weighted TTFT rather than a mean of the runs' means.
    #[test]
    fn arm_metrics_pool_request_level_samples_instead_of_run_means() {
        let mut first = legacy_result("task-0", "cached");
        first.request_usage = vec![request(1_000, Some(500))];
        first.cache_hit_ratio = Some(0.5);
        first.ttft_ms_mean = Some(100.0);
        first.ttft_ms_max = Some(120);
        first.ttft_ms_samples = 2;

        let mut second = legacy_result("task-1", "cached");
        second.request_usage = vec![request(500, Some(500)), request(250, None)];
        second.cache_hit_ratio = Some(1.0);
        second.ttft_ms_mean = Some(20.0);
        second.ttft_ms_max = Some(20);
        second.ttft_ms_samples = 1;

        let report = report_from(vec![first, second]);
        let arm = &report.arms[0];
        // (500 + 500) / (1 000 + 500), not the mean of the two run ratios (0.75).
        let ratio = arm
            .cache_hit_ratio
            .expect("both runs reported a cache split");
        assert!((ratio - 2.0 / 3.0).abs() < 1e-12, "{ratio}");
        // (100ms x 2 + 20ms x 1) / 3 samples, not (100 + 20) / 2 = 60ms.
        let mean = arm.ttft_ms_mean.expect("three TTFT samples were reported");
        assert!((mean - 220.0 / 3.0).abs() < 1e-9, "{mean}");
        assert_eq!(arm.ttft_ms_max, Some(120));
        assert_eq!(arm.ttft_ms_samples, 3);
    }
}
