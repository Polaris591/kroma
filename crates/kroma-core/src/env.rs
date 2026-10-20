use crate::budget::{RunDeadline, TokenBudget};
use crate::revision::RevisionId;
use crate::tool::policy::PolicyEngine;
use crate::types::RunId;
use crate::workspace::{InodeMutationLock, WorkspaceCap, WorkspaceError};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::watch;

/// Abstract execution environment decoupling filesystem & process spawning (§3.2 V6-07).
pub trait ExecutionEnv: Send + Sync {
    fn workspace(&self) -> &WorkspaceCap;
    fn inode_locks(&self) -> &InodeMutationLock;
    fn state_dir(&self) -> &Path;
}

/// Sanitized child-process environment shared by every host-side spawn the kroma performs
/// (supervised `bash`, command hooks, and the completion gate).
///
/// Strips `KROMA_*`, provider credentials and any `*_API_KEY` / `*_SECRET` / `*_TOKEN` /
/// `*_PASSWORD` / `*_CREDENTIAL*` variable, so a project-supplied hook can never read the model
/// credentials out of the environment it inherits.
pub fn build_sanitized_env() -> Vec<(std::ffi::OsString, std::ffi::OsString)> {
    crate::tools::bash::collect_sanitized_env()
}

/// Default local host environment backed by `WorkspaceCap` and Unix process groups.
#[derive(Debug, Clone)]
pub struct LocalHostEnv {
    workspace: WorkspaceCap,
    inode_locks: InodeMutationLock,
    state_dir: PathBuf,
}

impl LocalHostEnv {
    pub fn new(
        workspace_root: impl AsRef<Path>,
        state_dir: impl Into<PathBuf>,
    ) -> Result<Self, WorkspaceError> {
        let workspace = WorkspaceCap::new(workspace_root)?;
        Ok(Self {
            workspace,
            inode_locks: InodeMutationLock::new(),
            state_dir: state_dir.into(),
        })
    }
}

impl ExecutionEnv for LocalHostEnv {
    fn workspace(&self) -> &WorkspaceCap {
        &self.workspace
    }

    fn inode_locks(&self) -> &InodeMutationLock {
        &self.inode_locks
    }

    fn state_dir(&self) -> &Path {
        &self.state_dir
    }
}

#[derive(Debug, Clone)]
pub struct CompletionGateContext {
    pub run_id: RunId,
    pub workspace_root: PathBuf,
    pub modified_files: Vec<String>,
    pub turns_used: usize,
    /// F04 (`v1.5.1`): absolute deadline of the enclosing run. A gate that spawns a verification
    /// process clamps its own timeout to the remaining time so verification cannot outlive the run;
    /// `None` means the gate is not owned by a run (unit tests, ad-hoc checks).
    pub deadline: Option<RunDeadline>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateOutcome {
    Approved,
    /// Verification was intentionally skipped because no supported command was discoverable.
    Skipped {
        reason: String,
    },
    Rejected {
        feedback: String,
    },
}

/// Resolved, operator-facing description of what a [`CompletionGate`] will actually do before a run
/// is declared complete. Surfaced through `session.status` so a terminal surface can state the truth
/// instead of assuming verification is armed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompletionGateStatus {
    /// Completion is never verified (`--no-verify`, or a read-only policy).
    Disabled,
    /// Completion runs this exact operator command.
    Command { command: Vec<String> },
    /// Completion auto-detects a project command from workspace manifests.
    AutoDetected { allow_auto_detected: bool },
}

impl CompletionGateStatus {
    /// Whether the gate will actually verify completion.
    pub fn armed(&self) -> bool {
        !matches!(self, CompletionGateStatus::Disabled)
    }

    /// Stable machine name (`disabled` | `command` | `auto`).
    pub fn kind(&self) -> &'static str {
        match self {
            CompletionGateStatus::Disabled => "disabled",
            CompletionGateStatus::Command { .. } => "command",
            CompletionGateStatus::AutoDetected { .. } => "auto",
        }
    }

    /// The explicit verification command, when one is configured.
    pub fn command(&self) -> Option<&[String]> {
        match self {
            CompletionGateStatus::Command { command } => Some(command),
            _ => None,
        }
    }
}

/// Pre-terminal completion gate executed when model stops after modifying files (§3.2 V6-07).
pub trait CompletionGate: Send + Sync {
    fn check<'a>(
        &'a self,
        ctx: &'a CompletionGateContext,
    ) -> Pin<Box<dyn Future<Output = Result<GateOutcome, String>> + Send + 'a>>;

    /// What this gate will do. Defaults to auto-detection for third-party gates that do not
    /// describe themselves; the built-in gates override it so the status is never guessed.
    fn status(&self) -> CompletionGateStatus {
        CompletionGateStatus::AutoDetected {
            allow_auto_detected: true,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct AllowAllGate;

impl CompletionGate for AllowAllGate {
    fn check<'a>(
        &'a self,
        _ctx: &'a CompletionGateContext,
    ) -> Pin<Box<dyn Future<Output = Result<GateOutcome, String>> + Send + 'a>> {
        Box::pin(async { Ok(GateOutcome::Approved) })
    }

    fn status(&self) -> CompletionGateStatus {
        CompletionGateStatus::Disabled
    }
}

pub type SharedExecutionEnv = Arc<dyn ExecutionEnv>;
pub type SharedCompletionGate = Arc<dyn CompletionGate>;

/// F05: Unified execution context for one run, passed through tools, extensions, and MCP.
///
/// Carries the run's identity, absolute deadline, cancellation signal receiver, shared token
/// budget reference, capability/policy engine reference, and the active workspace revision the run
/// is currently authorizing against.
#[derive(Clone)]
pub struct ExecutionContext {
    pub run_id: Option<RunId>,
    pub deadline: Option<RunDeadline>,
    pub cancel: Option<watch::Receiver<bool>>,
    pub budget: Option<Arc<TokenBudget>>,
    pub policy: Option<Arc<PolicyEngine>>,
    /// B12a: the run's current workspace revision identity (from the `SnapshotStore` /
    /// `AgentRunner`), carried into every [`crate::tool::policy::AuthScope`] so an approval grant
    /// minted against one revision is invisible once the workspace moves to another. `None` when
    /// the run has no snapshot store / revision identity.
    pub revision: Option<RevisionId>,
}

impl std::fmt::Debug for ExecutionContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExecutionContext")
            .field("run_id", &self.run_id)
            .field("deadline", &self.deadline)
            .field("has_cancel", &self.cancel.is_some())
            .field("has_budget", &self.budget.is_some())
            .field("has_policy", &self.policy.is_some())
            .field("revision", &self.revision)
            .finish()
    }
}

impl ExecutionContext {
    /// Create an empty context without run constraints (for legacy/isolated unit tests only).
    pub fn empty() -> Self {
        Self {
            run_id: None,
            deadline: None,
            cancel: None,
            budget: None,
            policy: None,
            revision: None,
        }
    }

    pub fn new(
        run_id: Option<RunId>,
        deadline: Option<RunDeadline>,
        cancel: Option<watch::Receiver<bool>>,
        budget: Option<Arc<TokenBudget>>,
        policy: Option<Arc<PolicyEngine>>,
    ) -> Self {
        Self {
            run_id,
            deadline,
            cancel,
            budget,
            policy,
            revision: None,
        }
    }

    /// B12a: bind this run's authorization scope to the active workspace revision. A grant minted
    /// under one revision cannot be consumed under another, so a mutation that advances the
    /// revision retires the run's reusable category approvals instead of letting them outlive the
    /// state the operator approved.
    pub fn with_revision(mut self, revision: Option<RevisionId>) -> Self {
        self.revision = revision;
        self
    }

    /// True if cancellation has been signalled. Checks *rx.borrow() synchronously.
    pub fn is_cancelled(&self) -> bool {
        self.cancel.as_ref().is_some_and(|rx| *rx.borrow())
    }

    /// True if the run deadline has already passed.
    pub fn is_deadline_exceeded(&self) -> bool {
        self.deadline.as_ref().is_some_and(|d| d.is_exceeded())
    }

    /// Remaining time until the absolute deadline; Duration::ZERO if exceeded; None if no deadline.
    pub fn remaining_time(&self) -> Option<Duration> {
        self.deadline.map(|d| d.remaining())
    }

    /// Clamp a local timeout duration to the absolute deadline remaining time.
    /// Does not extend the deadline: takes the min of local timeout and remaining deadline.
    pub fn clamp_timeout(&self, local_timeout: Duration) -> Duration {
        match self.deadline {
            Some(d) => d.clamp(local_timeout),
            None => local_timeout,
        }
    }

    /// Effective absolute instant deadline combining local timeout and run deadline.
    pub fn effective_deadline(&self, local_timeout: Duration) -> Instant {
        let local_instant = Instant::now() + local_timeout;
        match self.deadline {
            Some(d) => {
                let clamped_dur = d.clamp(local_timeout);
                Instant::now() + clamped_dur
            }
            None => local_instant,
        }
    }
}
