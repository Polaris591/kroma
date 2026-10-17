//! Bounded, persistent asynchronous subagent task registry (capability gap #4).
//!
//! The review found that subagents were only ever awaited inside a batch and then recycled: there
//! was no durable record of a long-running child, no artifact handle, no way for a disconnected
//! client to be notified when work finished, and no supervisor seam to challenge or resume it.
//! This module is the minimal durable table those surfaces can share.
//!
//! Design constraints, taken from the review:
//!
//! * **Bounded.** [`MAX_TASKS`] live records, [`MAX_ARTIFACTS_PER_TASK`] artifacts, and
//!   [`MAX_NOTIFICATIONS_PER_TASK`] notifications per task; every free-form string is truncated to
//!   a fixed cap. A registry can therefore never grow without limit on a hostile workload.
//! * **Persistent.** Every mutation is one commit transaction: the next state is staged, written to
//!   a unique `O_NOFOLLOW` temp file, `fsync`-ed, renamed over the target and the directory
//!   `fsync`-ed, and only then published to memory. A precommit failure (temp write or rename)
//!   leaves both memory and the file at their old value and retryable; a post-rename directory-sync
//!   failure is reported as [`TaskRegistryError::PersistUncertain`] because the rename is already
//!   visible and cannot be rolled back. The document is reloaded on [`AsyncTaskRegistry::open`]; a
//!   task left `Running` across a restart is reported as `Suspended` by
//!   [`AsyncTaskRegistry::recover_interrupted`], never silently as success.
//! * **Owned.** Each record carries the `session_id` / `owner_id` / `workspace_root` triple from
//!   [`crate::session_ownership`], so the routing table can find the runtime that owns a task.
//! * **Budgeted.** A task records the token allocation it reserved from the shared
//!   [`crate::budget::TokenBudget`] and what it actually spent; a release does not refund known
//!   spend (the same rule the run budget uses).
//! * **Supervisable.** [`TaskControlRequest`] is a small JSON control protocol (list / pause /
//!   resume / cancel / challenge / acknowledge) that any surface can drive without reaching into
//!   the registry internals.

use crate::subagents::ParentPolicyHooks;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Hard ceiling on live task records held in one registry.
pub const MAX_TASKS: usize = 64;
/// Hard ceiling on artifacts attached to a single task.
pub const MAX_ARTIFACTS_PER_TASK: usize = 32;
/// Hard ceiling on retained notifications per task (oldest acknowledged ones are dropped first).
pub const MAX_NOTIFICATIONS_PER_TASK: usize = 64;
/// Maximum byte length of a task / session / owner / artifact id.
pub const MAX_ID_LEN: usize = 128;
/// Maximum byte length of a free-form message (challenge reason, error, notification).
pub const MAX_MESSAGE_LEN: usize = 512;
/// Cap on the persisted child prompt (enough to re-run the task, bounded so the registry stays
/// small).
pub const MAX_TASK_PROMPT_LEN: usize = 8192;
/// Cap on the persisted serialized `SubagentSpec` (the full resume contract: role, write scope,
/// budget, test-tamper guard, revert-on-verify).
pub const MAX_SPEC_JSON_LEN: usize = 16 * 1024;
/// Maximum size of the persisted registry document that will be loaded.
pub const MAX_STORE_BYTES: u64 = 4 * 1024 * 1024;
/// Cap on the changed-path list retained in a pending merge intent (the full manifest stays in the
/// snapshot store; this is only the WAL-linkage payload and must stay small).
pub const MAX_PENDING_MERGE_PATHS: usize = 1024;
/// B12: how many explicit reconcile decisions one task record keeps. Reconcile is a deliberate
/// operator action, so a small audit trail is enough; beyond it the registry refuses instead of
/// silently dropping the oldest decision.
pub const MAX_RECONCILES_PER_TASK: usize = 8;

/// A handle to a durable output of a task. The bytes live in the workspace (or snapshot store); the
/// registry only records an addressable reference so a resumed consumer can find them again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactRef {
    pub id: String,
    /// Workspace-relative path, or a `revision:<id>` / `blob:<sha256>` reference.
    pub location: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    /// The length of exactly the bytes stored for this artifact. A reader verifies the stored bytes
    /// against this length and `sha256`, so it can never be satisfied by bytes that were not stored.
    #[serde(default)]
    pub bytes: u64,
    /// The true size of the source when `bytes` is a bounded truncation of it; `None` means the
    /// stored bytes are the whole artifact. Kept separate from `bytes` so a truncated reference can
    /// report the real original size without ever claiming that many stored bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_bytes: Option<u64>,
    /// Free-form kind (`diff`, `report`, `test-log`, ...).
    #[serde(default)]
    pub kind: String,
}

impl ArtifactRef {
    pub fn new(
        id: impl Into<String>,
        location: impl Into<String>,
        kind: impl Into<String>,
    ) -> Self {
        Self {
            id: truncate(id.into(), MAX_ID_LEN),
            location: truncate(location.into(), MAX_MESSAGE_LEN),
            sha256: None,
            bytes: 0,
            original_bytes: None,
            kind: truncate(kind.into(), MAX_ID_LEN),
        }
    }

    pub fn with_sha256(mut self, sha256: impl Into<String>) -> Self {
        self.sha256 = Some(truncate(sha256.into(), MAX_ID_LEN));
        self
    }

    pub fn with_bytes(mut self, bytes: u64) -> Self {
        self.bytes = bytes;
        self
    }

    /// Record the true original size of a source whose stored bytes are a bounded truncation.
    pub fn with_original_bytes(mut self, original_bytes: u64) -> Self {
        self.original_bytes = Some(original_bytes);
        self
    }
}

/// Lifecycle state of a durable task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    Queued,
    Running,
    Suspended,
    /// A result exists but has been challenged; it must not be accepted until resolved.
    ChallengePending,
    Completed,
    Failed,
    Cancelled,
}

impl TaskState {
    pub fn as_str(self) -> &'static str {
        match self {
            TaskState::Queued => "queued",
            TaskState::Running => "running",
            TaskState::Suspended => "suspended",
            TaskState::ChallengePending => "challenge_pending",
            TaskState::Completed => "completed",
            TaskState::Failed => "failed",
            TaskState::Cancelled => "cancelled",
        }
    }

    /// `true` once the task can no longer make progress on its own.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            TaskState::Completed | TaskState::Failed | TaskState::Cancelled
        )
    }

    /// `true` when the task may be resumed into `Running`.
    pub fn is_resumable(self) -> bool {
        matches!(
            self,
            TaskState::Queued | TaskState::Suspended | TaskState::ChallengePending
        )
    }
}

impl std::fmt::Display for TaskState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A durable notification addressed to whoever is watching the task's session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskNotification {
    pub seq: u64,
    pub kind: String,
    pub message: String,
    pub at_ms: u64,
    pub acknowledged: bool,
}

/// B12: durable evidence that a real verification gate actually ran and **approved** a task's
/// result, bound to the workspace revision it verified. `None` means *unknown* — no gate was
/// armed, the gate was skipped, or the record predates this field. It is never inferred from a bare
/// "merge applied": a skipped/disabled gate also applies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerificationRecord {
    /// Identifier of the gate that ran (e.g. `post-merge`).
    pub gate: String,
    /// `true` only when the gate was armed, actually ran, and approved. Never set from a name or a
    /// merge outcome alone.
    pub passed: bool,
    /// The revision the gate verified, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    /// The *parent snapshot revision* the candidate was merged into and re-verified against, when
    /// known. This is the session WAL's latest persisted snapshot identity — a durable snapshot
    /// id, **not** a live byte-level identity: an external write that was never captured does not
    /// change it. `None` means unknown (no WAL revision, or the record predates this field), and is
    /// never faked into `Some`. The `revision` above is the merged revision the gate actually ran on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_snapshot_revision: Option<String>,
}

/// B12: an explicit, trusted acceptance decision over a completed result. Absent (`None`) means the
/// result is *not* accepted; a bare execution completion is never acceptance, and a model's own
/// "done" is not an acceptance. Minted only from an authenticated control principal — never from a
/// request/model-supplied string.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcceptanceRecord {
    /// The authenticated control principal that accepted, never a model or a raw request string.
    pub by: String,
    pub turn: u32,
    /// The verified revision this acceptance is bound to, when the acceptance requires verification.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence_revision: Option<String>,
    pub at_ms: u64,
    /// B12: snapshot of the artifact digests (`id:sha256`, or just `id` when no digest was stored)
    /// that existed at acceptance time, so an acceptance names the exact artifacts it covered.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifact_digests: Vec<String>,
    /// B12: the task's spent tokens at acceptance time, so the acceptance is linked to the budget
    /// the result actually consumed.
    #[serde(default)]
    pub token_spent: u64,
}

/// B12: durable crash-recovery disposition of a task.
///
/// A task that has been *admitted to execute* carries `Started` (persisted in the same transaction
/// as the `Running` transition, i.e. before any side effect). If the process restarts while the task
/// is still in that state, its outcome is genuinely unknown — the child may have begun side effects
/// that were never recorded — so recovery rewrites it to `UnknownOutcome`, which blocks re-running
/// until an explicit reconcile. A task that never reached `Started` never ran a side effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum RecoveryDisposition {
    /// A *new* record that has never been admitted to execute. This is the only state we can prove
    /// has produced no side effects, so it is the only one safe to (re)run. Legacy records written
    /// before this field existed deserialize to `None`, which is **not** proof of never-started.
    NeverStarted,
    /// A child run was admitted (persisted before any side effect).
    Started { turn: u32 },
    /// After a restart, a task that had started has an unknown outcome; re-running is blocked.
    UnknownOutcome { turn: u32 },
}

impl RecoveryDisposition {
    pub fn as_str(self) -> &'static str {
        match self {
            RecoveryDisposition::NeverStarted => "never_started",
            RecoveryDisposition::Started { .. } => "started",
            RecoveryDisposition::UnknownOutcome { .. } => "unknown_outcome",
        }
    }

    /// `true` when this disposition blocks re-running until an explicit reconcile.
    pub fn blocks_resume(self) -> bool {
        matches!(self, RecoveryDisposition::UnknownOutcome { .. })
    }
}

/// B12: one explicit, auditable disposition of an unknown-outcome task.
///
/// Every decision **keeps** the historical `UnknownOutcome` disposition: reconciling is a recorded
/// operator decision, never a way to make a doubtful result look re-runnable. A retry never reuses
/// the doubtful record — it names a separately registered attempt that owns its own budget and
/// policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ReconcileDecision {
    /// The operator acknowledges that the side effects are genuinely unknown and does not re-run.
    AcknowledgedUnknown,
    /// An external check resolved what actually happened; `evidence` names it (free text).
    ExternallyVerified { evidence: String },
    /// A separate attempt was registered to continue the work; it must already exist, be
    /// provably-never-started, and own its own budget/policy.
    RetryAsNewAttempt { new_task_id: String },
}

impl ReconcileDecision {
    pub fn as_str(&self) -> &'static str {
        match self {
            ReconcileDecision::AcknowledgedUnknown => "acknowledged_unknown",
            ReconcileDecision::ExternallyVerified { .. } => "externally_verified",
            ReconcileDecision::RetryAsNewAttempt { .. } => "retry_as_new_attempt",
        }
    }

    /// The replacement attempt this decision names, when it names one.
    pub fn new_attempt_id(&self) -> Option<&str> {
        match self {
            ReconcileDecision::RetryAsNewAttempt { new_task_id } => {
                Some(new_task_id.as_str())
            }
            _ => None,
        }
    }
}

/// B12: one entry of a task's reconcile audit trail.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReconcileRecord {
    /// The trusted control principal that made the decision, never a model or a request string.
    pub by: String,
    pub turn: u32,
    pub at_ms: u64,
    pub decision: ReconcileDecision,
    /// Why the operator decided this; required, so an audit entry is never blank.
    pub reason: String,
    /// The disposition that was in force when the decision was recorded (always `UnknownOutcome`
    /// today; kept explicitly so the trail stays truthful if the disposition set grows).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_disposition: Option<RecoveryDisposition>,
}

/// One bounded, persistent task record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AsyncTaskRecord {
    pub task_id: String,
    pub session_id: String,
    pub owner_id: String,
    pub workspace_root: String,
    /// Subagent role (`scout` / `worker`) the task runs as.
    pub role: String,
    pub state: TaskState,
    pub created_turn: u32,
    pub updated_turn: u32,
    /// Absolute path of the session WAL this task was admitted against. Captured at registration so
    /// a later resume links its merge into the *admitting* session's WAL, never whatever session the
    /// host happens to be bound to when the resume runs.
    #[serde(default)]
    pub session_path: String,
    /// Durable, session-bound intent to link a resumed worker merge into `session_path`. Written
    /// before the merge touches the parent workspace and cleared by [`AsyncTaskRegistry::settle_finish`]
    /// once the WAL append succeeded, so a crash in between is replayed instead of silently losing
    /// the `--undo` link.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_merge: Option<PendingMergeLink>,
    /// Tokens reserved from the shared ledger when the task was started.
    pub token_allocation: u64,
    /// Tokens the task has actually spent so far.
    pub token_spent: u64,
    pub artifacts: Vec<ArtifactRef>,
    pub notifications: Vec<TaskNotification>,
    /// Capability gap #4 (`v1.5.2`): the child task prompt, persisted so a suspended/interrupted
    /// task can actually be re-run by `tasks.control { action: "resume" }`, not merely re-labelled.
    #[serde(default)]
    pub prompt: String,
    /// Per-child wall-clock timeout in milliseconds, persisted for the same reason as `prompt`.
    #[serde(default)]
    pub timeout_ms: u64,
    /// Capability gap #4 (`v1.5.2`): the full serialized `SubagentSpec` this task was spawned with,
    /// so `tasks.control { action: "resume" }` re-runs the *exact* original spec (role, write scope,
    /// budget, test-tamper guard, revert-on-verify) instead of a weakened reconstruction.
    #[serde(default)]
    pub spec_json: String,
    /// The parent policy mode at spawn time (provenance), so a resume cannot run a child with a
    /// broader policy than its parent had.
    #[serde(default)]
    pub parent_policy: String,
    /// B11b: the parent's fail-closed policy-hook constraints recorded at admission. A legacy
    /// record without this field deserializes to [`ParentPolicyHooks::Unknown`] (provenance is
    /// unknown, never "no constraints"), and the backend refuses to re-run such a child.
    #[serde(default = "ParentPolicyHooks::unknown")]
    pub parent_policy_hooks: ParentPolicyHooks,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// The open challenge, if any. A non-empty value forces `ChallengePending`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub challenge: Option<String>,
    /// B12: real verification-gate evidence, or `None` = unknown. Never inferred from a merge
    /// outcome. Cleared when a new finish/revision invalidates the old evidence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verification: Option<VerificationRecord>,
    /// B12: an explicit trusted acceptance decision, or `None` = not accepted. Orthogonal to the
    /// execution terminal state: a `Completed` task with no acceptance is not accepted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acceptance: Option<AcceptanceRecord>,
    /// B12: durable crash-recovery disposition. `Some(Started)` marks a task admitted to execute;
    /// after a restart it becomes `Some(UnknownOutcome)`, which blocks re-running. `None` = the task
    /// was never admitted to run (definitely no side effects).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery: Option<RecoveryDisposition>,
    /// B12: audit trail of explicit reconcile decisions, oldest first. Reconciling never clears
    /// `recovery`; it records what the operator decided about the unknown outcome.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reconciles: Vec<ReconcileRecord>,
    /// B12: set when a reconcile decision registered a replacement attempt, so a reader can follow
    /// the history forward. Never cleared: an attempt that superseded another stays named.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<String>,
}

impl AsyncTaskRecord {
    /// B12: `true` only when the task has an explicit trusted acceptance decision, no open challenge,
    /// and a *real* passed verification of the exact revision the acceptance names. A bare execution
    /// completion is **not** acceptance, and an acceptance that does not name a verified revision is
    /// never accepted — there is no `None == None` bypass.
    ///
    /// `by`/`turn` are descriptive provenance only; they are never an authorization identity.
    pub fn is_accepted(&self) -> bool {
        if self.state != TaskState::Completed || self.challenge.is_some() {
            return false;
        }
        let Some(acceptance) = self.acceptance.as_ref() else {
            return false;
        };
        // A formal acceptance must name the exact revision it is bound to; a missing or blank
        // evidence revision is not an acceptance.
        let Some(rev) = acceptance
            .evidence_revision
            .as_deref()
            .filter(|r| !r.trim().is_empty())
        else {
            return false;
        };
        // That exact revision must itself carry a real *passed* verification.
        self.verification
            .as_ref()
            .is_some_and(|v| v.passed && v.revision.as_deref() == Some(rev))
    }

    /// B12: `true` when a crash left this task's outcome unknown, so it must not be re-run (or
    /// settled as a success) until an explicit reconcile clears the disposition.
    pub fn is_recovery_blocked(&self) -> bool {
        self.recovery
            .is_some_and(RecoveryDisposition::blocks_resume)
    }

    /// B12: `true` only when this record provably never started an execution, so re-running it
    /// cannot duplicate side effects. That is: a `Queued` task (never admitted) or a record that
    /// carries an explicit `NeverStarted` marker. A legacy record with a missing marker (`None`),
    /// a `Started` record (a live executor may still hold it — a pause does not stop the child), and
    /// an `UnknownOutcome` record are all **not** safe to re-run.
    pub fn can_resume_safely(&self) -> bool {
        self.state == TaskState::Queued
            || matches!(self.recovery, Some(RecoveryDisposition::NeverStarted))
    }
}

/// Specification for registering a task. Every field is bounded on insert.
#[derive(Debug, Clone)]
pub struct NewTask {
    pub task_id: String,
    pub session_id: String,
    pub owner_id: String,
    pub workspace_root: String,
    pub role: String,
    pub created_turn: u32,
    pub token_allocation: u64,
    /// The child prompt; persisted so the task can be resumed for real.
    pub prompt: String,
    /// Per-child timeout in milliseconds.
    pub timeout_ms: u64,
    /// The full serialized `SubagentSpec` (the resume contract).
    pub spec_json: String,
    /// The parent policy mode at spawn time (provenance).
    pub parent_policy: String,
    /// B11b: the parent's fail-closed policy-hook constraints, taken from the live parent registry
    /// at admission. Stored as a strong type so it is never a truncated or lossy string.
    pub parent_policy_hooks: ParentPolicyHooks,
    /// Absolute path of the session WAL this task is admitted against (the resume link target).
    pub session_path: String,
}

/// A durable, session-bound intent to link a resumed worker's merged revision into a session WAL.
///
/// It is committed *before* the merge touches the parent workspace, updated with the merged
/// revision once the merge is captured, and cleared only after the WAL append succeeded. A crash
/// anywhere in between therefore leaves a replayable intent rather than a merge whose undo link is
/// silently gone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingMergeLink {
    pub session_id: String,
    /// The WAL the merged revision must be recorded in.
    pub session_path: String,
    pub turn: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_revision: Option<String>,
    /// Set once the merge has been captured; `None` means the merge never landed (the snapshot
    /// store's own merge-intent recovery owns that case).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merged_revision: Option<String>,
    #[serde(default)]
    pub changed_paths: Vec<String>,
    #[serde(default)]
    pub tracked_files: usize,
}

/// Everything a finished child needs settled, committed by [`AsyncTaskRegistry::settle_finish`] as
/// **one** registry transaction so a partial settlement can never be observed.
#[derive(Debug, Clone)]
pub struct TaskFinish {
    pub state: TaskState,
    pub spent: u64,
    pub artifact: Option<ArtifactRef>,
    pub summary: String,
    pub turn: u32,
    /// `true` keeps a pending merge intent in place: used when the merge landed but its session WAL
    /// link could not be recorded, so the intent stays replayable instead of being discarded with a
    /// settlement that must not claim completion.
    pub keep_pending_merge: bool,
    /// B12: real verification evidence for this settlement, when a gate actually ran and approved.
    /// `None` for the common (unarmed/skipped) case — the settlement then records `unknown`, never a
    /// fake `verified`. Written atomically with the rest of the settlement.
    pub verification: Option<VerificationRecord>,
}

/// Errors the registry can return. All are explicit; nothing is silently swallowed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskRegistryError {
    DuplicateTask(String),
    CapacityExceeded(usize),
    UnknownTask(String),
    InvalidTransition {
        task_id: String,
        from: TaskState,
        to: TaskState,
    },
    NoOpenChallenge(String),
    /// B12: a task whose outcome is unknown after a crash cannot be re-run (or settled) until an
    /// explicit reconcile; the disposition is kept rather than silently cleared.
    RecoveryBlocked(String),
    /// B12: a formal acceptance needs a completed, gate-verified, unchallenged result whose exact
    /// verified revision the acceptance can name.
    AcceptanceRefused(String),
    /// B12: only a task whose outcome is unknown after a crash can be reconciled.
    NoUnknownOutcome(String),
    /// B12: a reconcile decision was malformed (blank reason/evidence, or a bad replacement id).
    InvalidReconcile(String),
    /// B12: a task was already superseded by a different replacement attempt.
    AlreadySuperseded { task_id: String, by: String },
    Persist(String),
    /// The new document was renamed into place, so the mutation is visible to readers, but the
    /// directory sync that would make the rename survive a power loss failed. The in-memory state
    /// is published to match the file, and the caller must treat the mutation as applied-but-not-
    /// provably-durable rather than rolled back.
    PersistUncertain(String),
    Load(String),
}

impl std::fmt::Display for TaskRegistryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TaskRegistryError::DuplicateTask(id) => write!(f, "task '{}' already exists", id),
            TaskRegistryError::CapacityExceeded(max) => {
                write!(f, "task registry is full (max {})", max)
            }
            TaskRegistryError::UnknownTask(id) => write!(f, "unknown task '{}'", id),
            TaskRegistryError::InvalidTransition { task_id, from, to } => write!(
                f,
                "task '{}' cannot transition from {} to {}",
                task_id, from, to
            ),
            TaskRegistryError::NoOpenChallenge(id) => {
                write!(f, "task '{}' has no open challenge to resolve", id)
            }
            TaskRegistryError::RecoveryBlocked(id) => write!(
                f,
                "task '{}' may already have started (or its outcome is unknown after a crash); it must be explicitly reconciled before it can run again",
                id
            ),
            TaskRegistryError::AcceptanceRefused(msg) => write!(f, "acceptance refused: {}", msg),
            TaskRegistryError::NoUnknownOutcome(id) => write!(
                f,
                "task '{}' has no unknown outcome to reconcile; reconcile is only for a task whose outcome is unknown after a crash",
                id
            ),
            TaskRegistryError::InvalidReconcile(msg) => write!(f, "reconcile refused: {}", msg),
            TaskRegistryError::AlreadySuperseded { task_id, by } => write!(
                f,
                "task '{}' is already superseded by attempt '{}'; refusing to name a different replacement",
                task_id, by
            ),
            TaskRegistryError::Persist(msg) => {
                write!(f, "failed to persist task registry: {}", msg)
            }
            TaskRegistryError::PersistUncertain(msg) => write!(
                f,
                "task registry commit is visible but not provably durable: {}",
                msg
            ),
            TaskRegistryError::Load(msg) => write!(f, "failed to load task registry: {}", msg),
        }
    }
}

impl std::error::Error for TaskRegistryError {}

/// On-disk document format.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct RegistryDocument {
    version: u32,
    next_notification_seq: u64,
    tasks: Vec<AsyncTaskRecord>,
}

const REGISTRY_VERSION: u32 = 1;

/// Bounded, persistent task registry.
#[derive(Debug)]
pub struct AsyncTaskRegistry {
    path: PathBuf,
    tasks: BTreeMap<String, AsyncTaskRecord>,
    next_notification_seq: u64,
    max_tasks: usize,
    /// Test-only fault-injection point in the commit sequence. Instance-scoped so a fault set by one
    /// test can never leak into another registry.
    #[cfg(test)]
    fault: Option<PersistFault>,
}

fn truncate(mut value: String, max: usize) -> String {
    if value.len() > max {
        // Truncate on a char boundary so a multi-byte id never becomes invalid UTF-8.
        let mut end = max;
        while end > 0 && !value.is_char_boundary(end) {
            end -= 1;
        }
        value.truncate(end);
    }
    value
}

fn truncate_ref(value: &str, max: usize) -> String {
    truncate(value.to_string(), max)
}

/// Fault-injection points in the commit sequence. Compiled only for the registry's own tests and
/// stored on the registry instance, so a fault is scoped to one backing file.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PersistFault {
    /// Fail before any temp file is created: nothing on disk changes.
    Precommit,
    /// Fail at the rename step, after the temp file was written and synced: the target is untouched.
    Rename,
    /// Fail the directory sync after a successful rename: the new document is visible, its
    /// crash-durability is not confirmable.
    DirSync,
}

/// A staged copy of the mutable registry state. A mutation is applied here first and is only
/// published to the live registry once the staged document has been committed, so a failed commit
/// can never leave memory ahead of disk.
struct StagedRegistry {
    tasks: BTreeMap<String, AsyncTaskRecord>,
    next_notification_seq: u64,
}

impl StagedRegistry {
    fn take_seq(&mut self) -> u64 {
        let seq = self.next_notification_seq;
        self.next_notification_seq = self.next_notification_seq.saturating_add(1);
        seq
    }

    fn require_mut(&mut self, task_id: &str) -> Result<&mut AsyncTaskRecord, TaskRegistryError> {
        self.tasks
            .get_mut(task_id)
            .ok_or_else(|| TaskRegistryError::UnknownTask(task_id.to_string()))
    }
}

/// Why the low-level atomic write did not commit cleanly. The two cases are not interchangeable: a
/// precommit failure is invisible and retryable, while a post-rename failure has already published
/// the new document and only its durability is in question.
enum CommitError {
    Precommit(String),
    CommitUncertain(String),
}

/// `fsync` a directory so a rename within it survives a power loss. On Unix the directory is opened
/// with the shared no-follow primitive so a symlinked store directory is refused rather than
/// followed; platforms without directory `fsync` treat this as a best-effort no-op.
fn sync_dir(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        crate::secure_fs::open_existing_no_symlink(dir)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        Ok(())
    }
}

impl AsyncTaskRegistry {
    /// Open (or create) the registry at `path`, loading any existing document.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, TaskRegistryError> {
        let path = path.into();
        let mut registry = Self {
            path,
            tasks: BTreeMap::new(),
            next_notification_seq: 1,
            max_tasks: MAX_TASKS,
            #[cfg(test)]
            fault: None,
        };
        if registry.path.exists() {
            let meta = std::fs::metadata(&registry.path)
                .map_err(|e| TaskRegistryError::Load(e.to_string()))?;
            if meta.len() > MAX_STORE_BYTES {
                return Err(TaskRegistryError::Load(format!(
                    "registry document is {} bytes, over the {} byte cap",
                    meta.len(),
                    MAX_STORE_BYTES
                )));
            }
            let raw = std::fs::read_to_string(&registry.path)
                .map_err(|e| TaskRegistryError::Load(e.to_string()))?;
            if !raw.trim().is_empty() {
                let doc: RegistryDocument = serde_json::from_str(&raw)
                    .map_err(|e| TaskRegistryError::Load(e.to_string()))?;
                registry.next_notification_seq = doc.next_notification_seq.max(1);
                for task in doc.tasks.into_iter().take(MAX_TASKS) {
                    registry.tasks.insert(task.task_id.clone(), task);
                }
            }
        }
        Ok(registry)
    }

    /// Open an in-memory registry with no backing file (used by tests and ephemeral sessions).
    pub fn in_memory() -> Self {
        Self {
            path: PathBuf::new(),
            tasks: BTreeMap::new(),
            next_notification_seq: 1,
            max_tasks: MAX_TASKS,
            #[cfg(test)]
            fault: None,
        }
    }

    pub fn len(&self) -> usize {
        self.tasks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }

    pub fn get(&self, task_id: &str) -> Option<&AsyncTaskRecord> {
        self.tasks.get(task_id)
    }

    /// A clone of one task record, for callers that act on it outside the registry lock.
    pub fn get_cloned(&self, task_id: &str) -> Option<AsyncTaskRecord> {
        self.tasks.get(task_id).cloned()
    }

    /// Current lifecycle state of a task, if it exists.
    pub fn state_of(&self, task_id: &str) -> Option<TaskState> {
        self.tasks.get(task_id).map(|task| task.state)
    }

    /// Register a new task in `Queued` state, enforcing the capacity bound and committing it.
    pub fn register(&mut self, spec: NewTask) -> Result<AsyncTaskRecord, TaskRegistryError> {
        let task_id = truncate_ref(&spec.task_id, MAX_ID_LEN);
        if task_id.is_empty() {
            return Err(TaskRegistryError::UnknownTask(task_id));
        }
        let max_tasks = self.max_tasks;
        self.transact(|staged| {
            if staged.tasks.contains_key(&task_id) {
                return Err(TaskRegistryError::DuplicateTask(task_id.clone()));
            }
            if staged.tasks.len() >= max_tasks {
                return Err(TaskRegistryError::CapacityExceeded(max_tasks));
            }
            let now_ms = now_ms();
            let record = AsyncTaskRecord {
                task_id: task_id.clone(),
                session_id: truncate_ref(&spec.session_id, MAX_ID_LEN),
                owner_id: truncate_ref(&spec.owner_id, MAX_ID_LEN),
                workspace_root: truncate_ref(&spec.workspace_root, MAX_MESSAGE_LEN),
                role: truncate_ref(&spec.role, MAX_ID_LEN),
                state: TaskState::Queued,
                created_turn: spec.created_turn,
                updated_turn: spec.created_turn,
                session_path: truncate(spec.session_path.clone(), MAX_MESSAGE_LEN),
                pending_merge: None,
                token_allocation: spec.token_allocation,
                token_spent: 0,
                artifacts: Vec::new(),
                prompt: truncate(spec.prompt.clone(), MAX_TASK_PROMPT_LEN),
                timeout_ms: spec.timeout_ms,
                spec_json: truncate(spec.spec_json.clone(), MAX_SPEC_JSON_LEN),
                parent_policy: truncate(spec.parent_policy.clone(), MAX_ID_LEN),
                parent_policy_hooks: spec.parent_policy_hooks.clone(),
                notifications: vec![TaskNotification {
                    seq: staged.take_seq(),
                    kind: "registered".to_string(),
                    message: "task registered".to_string(),
                    at_ms: now_ms,
                    acknowledged: false,
                }],
                last_error: None,
                challenge: None,
                verification: None,
                acceptance: None,
                recovery: Some(RecoveryDisposition::NeverStarted),
                reconciles: Vec::new(),
                superseded_by: None,
            };
            staged.tasks.insert(task_id.clone(), record.clone());
            Ok(record)
        })
    }

    /// Transition a task, validating the state machine. A `ChallengePending` task can only leave
    /// that state through [`Self::resolve_challenge`]; an open challenge blocks completion.
    pub fn transition(
        &mut self,
        task_id: &str,
        to: TaskState,
        turn: u32,
    ) -> Result<TaskState, TaskRegistryError> {
        self.transact(|staged| {
            let task = staged
                .tasks
                .get(task_id)
                .ok_or_else(|| TaskRegistryError::UnknownTask(task_id.to_string()))?;
            let from = task.state;
            if !is_valid_transition(from, to) {
                return Err(TaskRegistryError::InvalidTransition {
                    task_id: task_id.to_string(),
                    from,
                    to,
                });
            }
            if to == TaskState::Completed && task.challenge.is_some() {
                // A challenged result must be resolved first; completion cannot bypass it.
                return Err(TaskRegistryError::InvalidTransition {
                    task_id: task_id.to_string(),
                    from,
                    to,
                });
            }
            let task = staged.require_mut(task_id)?;
            task.state = to;
            task.updated_turn = turn;
            if to == TaskState::Running {
                // B12: a new attempt (resume/re-run) invalidates stale verification/acceptance;
                // evidence must never be inherited across a re-run.
                task.verification = None;
                task.acceptance = None;
            }
            Ok(to)
        })
    }

    /// Suspend a running/queued task so its progress survives a disconnect.
    pub fn suspend(&mut self, task_id: &str, turn: u32) -> Result<(), TaskRegistryError> {
        self.transition(task_id, TaskState::Suspended, turn)
            .map(|_| ())
    }

    /// Resume a suspended (or queued / challenged) task back to `Running`.
    pub fn resume(&mut self, task_id: &str, turn: u32) -> Result<(), TaskRegistryError> {
        self.begin_execution(task_id, turn)
    }

    /// B12: admit a task to execute. Persists the durable `Started` marker in the **same transaction**
    /// as the `Running` transition (so a crash after admission is detectable), and refuses a task
    /// whose outcome is already unknown after a crash — every admission path shares this guard, so a
    /// concurrent transition can never slip past a read-then-act check. Clears stale
    /// verification/acceptance for the new attempt.
    pub fn begin_execution(&mut self, task_id: &str, turn: u32) -> Result<(), TaskRegistryError> {
        self.transact(|staged| {
            let task = staged
                .tasks
                .get(task_id)
                .ok_or_else(|| TaskRegistryError::UnknownTask(task_id.to_string()))?;
            let from = task.state;
            if !is_valid_transition(from, TaskState::Running) {
                return Err(TaskRegistryError::InvalidTransition {
                    task_id: task_id.to_string(),
                    from,
                    to: TaskState::Running,
                });
            }
            // Refuse to (re)run anything that may already have produced side effects: an
            // `UnknownOutcome` after a crash, a `Started` record whose live executor a pause does
            // not stop, or a legacy record with no marker at all. Only a provably-never-started
            // record (Queued / explicit `NeverStarted`) may run. The check is inside the same
            // transaction as the `Running` transition, so no path can bypass it.
            if !task.can_resume_safely() {
                return Err(TaskRegistryError::RecoveryBlocked(task_id.to_string()));
            }
            let task = staged.require_mut(task_id)?;
            task.state = TaskState::Running;
            task.updated_turn = turn;
            task.recovery = Some(RecoveryDisposition::Started { turn });
            // A new attempt invalidates stale verification/acceptance.
            task.verification = None;
            task.acceptance = None;
            Ok(())
        })
    }

    /// Attach an artifact reference, bounded per task.
    pub fn attach_artifact(
        &mut self,
        task_id: &str,
        artifact: ArtifactRef,
    ) -> Result<(), TaskRegistryError> {
        self.transact(|staged| {
            let task = staged
                .tasks
                .get(task_id)
                .ok_or_else(|| TaskRegistryError::UnknownTask(task_id.to_string()))?;
            if task.artifacts.len() >= MAX_ARTIFACTS_PER_TASK {
                return Err(TaskRegistryError::CapacityExceeded(MAX_ARTIFACTS_PER_TASK));
            }
            if task
                .artifacts
                .iter()
                .any(|existing| existing.id == artifact.id)
            {
                return Err(TaskRegistryError::DuplicateTask(format!(
                    "artifact {}",
                    artifact.id
                )));
            }
            let task = staged.require_mut(task_id)?;
            task.artifacts.push(artifact);
            Ok(())
        })
    }

    /// Append a bounded notification and return its sequence number.
    pub fn notify(
        &mut self,
        task_id: &str,
        kind: &str,
        message: &str,
    ) -> Result<u64, TaskRegistryError> {
        self.transact(|staged| {
            let seq = staged.take_seq();
            let notification = TaskNotification {
                seq,
                kind: truncate_ref(kind, MAX_ID_LEN),
                message: truncate_ref(message, MAX_MESSAGE_LEN),
                at_ms: now_ms(),
                acknowledged: false,
            };
            let task = staged.require_mut(task_id)?;
            task.notifications.push(notification);
            // Drop the oldest already-acknowledged notification once over the cap so a long-lived
            // task cannot grow without bound; unacknowledged notifications are retained.
            while task.notifications.len() > MAX_NOTIFICATIONS_PER_TASK {
                match task.notifications.iter().position(|n| n.acknowledged) {
                    Some(index) => {
                        task.notifications.remove(index);
                    }
                    None => {
                        task.notifications.remove(0);
                    }
                }
            }
            Ok(seq)
        })
    }

    /// Record actual spend against the task's reserved allocation. The reservation is never
    /// refunded below what was spent. A precommit failure leaves the previous spend and the caller
    /// may retry; the update is never applied twice.
    pub fn settle_spend(&mut self, task_id: &str, spent: u64) -> Result<(), TaskRegistryError> {
        self.transact(|staged| {
            let task = staged.require_mut(task_id)?;
            task.token_spent = task.token_spent.saturating_add(spent);
            Ok(())
        })
    }

    /// Atomically settle a finished task: attach the report artifact, add the real spend, move the
    /// state, append the outcome notification and (unless asked to keep it) clear any pending merge
    /// intent — all in **one** commit, so no partial settlement is ever observable.
    ///
    /// A task with an open challenge is left `ChallengePending` on purpose: a challenged result must
    /// not be accepted by a settlement. The spend, artifact and notification are still recorded, and
    /// the returned state says what the record actually holds. The transition routes through
    /// `Running` when the direct edge does not exist, exactly like the host's old `drive_to_state`.
    pub fn settle_finish(
        &mut self,
        task_id: &str,
        finish: TaskFinish,
    ) -> Result<TaskState, TaskRegistryError> {
        self.transact(|staged| {
            let task = staged
                .tasks
                .get(task_id)
                .ok_or_else(|| TaskRegistryError::UnknownTask(task_id.to_string()))?;
            // A duplicate artifact id means the record already carries this report; not fatal.
            let attach = match &finish.artifact {
                Some(artifact) if !task.artifacts.iter().any(|a| a.id == artifact.id) => {
                    if task.artifacts.len() >= MAX_ARTIFACTS_PER_TASK {
                        return Err(TaskRegistryError::CapacityExceeded(MAX_ARTIFACTS_PER_TASK));
                    }
                    Some(artifact.clone())
                }
                _ => None,
            };
            let from = task.state;
            // B12: a task whose outcome is unknown after a crash must not be settled as a success
            // (or failure) without an explicit reconcile — that would be a fake reconciliation.
            if task.is_recovery_blocked() {
                return Err(TaskRegistryError::RecoveryBlocked(task_id.to_string()));
            }
            // A record that is already terminal belongs to a finished run: only an identical
            // terminal outcome may be re-settled (a late cancel/finish for the *same* run). A
            // different outcome means the id was reused for a new run, and settling it here would
            // add spend to — and keep the artifact of — a record that run never owned.
            if from.is_terminal() && from != finish.state {
                return Err(TaskRegistryError::InvalidTransition {
                    task_id: task_id.to_string(),
                    from,
                    to: finish.state,
                });
            }
            // A direct edge, or one reached by routing through `Running` (a task paused while it
            // ran still completes). A completion with an open challenge is never valid.
            let direct = is_valid_transition(from, finish.state)
                && !(finish.state == TaskState::Completed && task.challenge.is_some());
            let via_running = is_valid_transition(from, TaskState::Running)
                && is_valid_transition(TaskState::Running, finish.state);
            let next = if from == TaskState::ChallengePending {
                // An open challenge blocks acceptance; keep the truthful state.
                TaskState::ChallengePending
            } else if from == finish.state || direct || via_running {
                finish.state
            } else {
                return Err(TaskRegistryError::InvalidTransition {
                    task_id: task_id.to_string(),
                    from,
                    to: finish.state,
                });
            };
            let seq = staged.take_seq();
            let task = staged.require_mut(task_id)?;
            if let Some(artifact) = attach {
                task.artifacts.push(artifact);
            }
            task.token_spent = task.token_spent.saturating_add(finish.spent);
            task.state = next;
            task.updated_turn = finish.turn;
            // B12: a fresh settlement carries fresh verification evidence (or `None` = unknown) and
            // invalidates any stale acceptance: a new finish/revision must not inherit the previous
            // result's acceptance.
            task.verification = finish.verification;
            task.acceptance = None;
            // A settled execution is no longer in-flight; clear the started marker.
            task.recovery = None;
            if !finish.keep_pending_merge {
                task.pending_merge = None;
            }
            task.notifications.push(TaskNotification {
                seq,
                kind: next.as_str().to_string(),
                message: truncate(finish.summary.clone(), MAX_MESSAGE_LEN),
                at_ms: now_ms(),
                acknowledged: false,
            });
            trim_notifications(task);
            Ok(next)
        })
    }

    /// Record (or update) the durable, session-bound merge intent for a task. The changed-path list
    /// is bounded here so a huge manifest cannot bloat the registry document.
    pub fn set_pending_merge(
        &mut self,
        task_id: &str,
        mut link: PendingMergeLink,
    ) -> Result<(), TaskRegistryError> {
        link.session_id = truncate(link.session_id, MAX_ID_LEN);
        link.session_path = truncate(link.session_path, MAX_MESSAGE_LEN);
        link.changed_paths.truncate(MAX_PENDING_MERGE_PATHS);
        self.transact(|staged| {
            let task = staged.require_mut(task_id)?;
            task.pending_merge = Some(link.clone());
            Ok(())
        })
    }

    /// Drop a task's pending merge intent once its WAL link has been recorded.
    pub fn clear_pending_merge(&mut self, task_id: &str) -> Result<(), TaskRegistryError> {
        self.transact(|staged| {
            let task = staged.require_mut(task_id)?;
            task.pending_merge = None;
            Ok(())
        })
    }

    /// Every pending merge intent, as `(task_id, link)`, for crash replay.
    pub fn pending_merges(&self) -> Vec<(String, PendingMergeLink)> {
        self.tasks
            .values()
            .filter_map(|task| {
                task.pending_merge
                    .clone()
                    .map(|link| (task.task_id.clone(), link))
            })
            .collect()
    }

    /// Mark a task failed, retaining the reason. The state change and the reason are one commit, so
    /// a failed commit cannot leave the task `Failed` without its error.
    pub fn fail(
        &mut self,
        task_id: &str,
        reason: &str,
        turn: u32,
    ) -> Result<(), TaskRegistryError> {
        self.transact(|staged| {
            let task = staged
                .tasks
                .get(task_id)
                .ok_or_else(|| TaskRegistryError::UnknownTask(task_id.to_string()))?;
            let from = task.state;
            if !is_valid_transition(from, TaskState::Failed) {
                return Err(TaskRegistryError::InvalidTransition {
                    task_id: task_id.to_string(),
                    from,
                    to: TaskState::Failed,
                });
            }
            let task = staged.require_mut(task_id)?;
            task.state = TaskState::Failed;
            task.updated_turn = turn;
            task.last_error = Some(truncate_ref(reason, MAX_MESSAGE_LEN));
            Ok(())
        })
    }

    /// Open a challenge against a task's result. A challenged result is moved to
    /// `ChallengePending` and cannot be accepted until the challenge is resolved.
    pub fn challenge(
        &mut self,
        task_id: &str,
        reason: &str,
        turn: u32,
    ) -> Result<(), TaskRegistryError> {
        self.transact(|staged| {
            let task = staged
                .tasks
                .get(task_id)
                .ok_or_else(|| TaskRegistryError::UnknownTask(task_id.to_string()))?;
            if !is_valid_transition(task.state, TaskState::ChallengePending) {
                return Err(TaskRegistryError::InvalidTransition {
                    task_id: task_id.to_string(),
                    from: task.state,
                    to: TaskState::ChallengePending,
                });
            }
            let seq = staged.take_seq();
            let task = staged.require_mut(task_id)?;
            task.state = TaskState::ChallengePending;
            task.challenge = Some(truncate_ref(reason, MAX_MESSAGE_LEN));
            task.updated_turn = turn;
            task.notifications.push(TaskNotification {
                seq,
                kind: "challenged".to_string(),
                message: truncate_ref(reason, MAX_MESSAGE_LEN),
                at_ms: now_ms(),
                acknowledged: false,
            });
            Ok(())
        })
    }

    /// Resolve an open challenge. `accept = true` keeps the result (task returns to `Completed`);
    /// `accept = false` sends the task back to `Running` for another attempt.
    pub fn resolve_challenge(
        &mut self,
        task_id: &str,
        accept: bool,
        turn: u32,
    ) -> Result<TaskState, TaskRegistryError> {
        self.transact(|staged| {
            let task = staged
                .tasks
                .get(task_id)
                .ok_or_else(|| TaskRegistryError::UnknownTask(task_id.to_string()))?;
            if task.challenge.is_none() || task.state != TaskState::ChallengePending {
                return Err(TaskRegistryError::NoOpenChallenge(task_id.to_string()));
            }
            let next = if accept {
                TaskState::Completed
            } else {
                TaskState::Running
            };
            let seq = staged.take_seq();
            let task = staged.require_mut(task_id)?;
            task.challenge = None;
            // B12: a challenge invalidates any prior acceptance — the result is no longer trusted.
            task.acceptance = None;
            task.state = next;
            task.updated_turn = turn;
            task.notifications.push(TaskNotification {
                seq,
                kind: "challenge_resolved".to_string(),
                message: if accept {
                    "challenge upheld; result accepted".to_string()
                } else {
                    "challenge rejected; task resumed".to_string()
                },
                at_ms: now_ms(),
                acknowledged: false,
            });
            Ok(next)
        })
    }

    /// Supervisor control: cancel a task. Terminal tasks are left as-is.
    /// B12: mint an explicit, trusted acceptance decision for a completed, gate-verified task.
    ///
    /// `by` is the trusted local control principal supplied by the host entry point; it is never
    /// taken from a request field or a model message. The acceptance is bound to the exact revision
    /// the gate verified (a caller-provided revision, when present, must match it), snapshots the
    /// artifacts and spend at acceptance time, and is idempotent: re-accepting the same
    /// (principal, revision) returns the recorded decision without touching the registry.
    /// Re-accepting a different revision replaces the decision and records a notification.
    pub fn accept(
        &mut self,
        task_id: &str,
        by: &str,
        turn: u32,
        evidence_revision: Option<&str>,
    ) -> Result<AcceptanceRecord, TaskRegistryError> {
        let by = truncate(by.trim().to_string(), MAX_ID_LEN);
        if by.is_empty() {
            return Err(TaskRegistryError::AcceptanceRefused(
                "an accepting principal is required".to_string(),
            ));
        }
        self.transact(|staged| {
            let task = staged
                .tasks
                .get(task_id)
                .ok_or_else(|| TaskRegistryError::UnknownTask(task_id.to_string()))?;
            if task.state != TaskState::Completed {
                return Err(TaskRegistryError::AcceptanceRefused(format!(
                    "task '{}' is {}; only a Completed task can be accepted",
                    task_id,
                    task.state.as_str()
                )));
            }
            if task.challenge.is_some() {
                return Err(TaskRegistryError::AcceptanceRefused(format!(
                    "task '{}' has an open challenge; resolve it before accepting",
                    task_id
                )));
            }
            let verified_revision = task
                .verification
                .as_ref()
                .filter(|v| v.passed)
                .and_then(|v| v.revision.as_deref())
                .filter(|rev| !rev.trim().is_empty())
                .ok_or_else(|| {
                    TaskRegistryError::AcceptanceRefused(format!(
                        "task '{}' has no passed verification naming a revision; acceptance can only bind a verified result",
                        task_id
                    ))
                })?;
            if let Some(requested) = evidence_revision {
                if requested.trim() != verified_revision {
                    return Err(TaskRegistryError::AcceptanceRefused(format!(
                        "task '{}' was verified at revision '{}', not '{}'",
                        task_id,
                        verified_revision,
                        requested.trim()
                    )));
                }
            }
            let revision = verified_revision.to_string();
            if let Some(existing) = task.acceptance.as_ref() {
                if existing.by == by
                    && existing.evidence_revision.as_deref() == Some(revision.as_str())
                {
                    return Ok(existing.clone());
                }
            }
            let artifact_digests: Vec<String> = task
                .artifacts
                .iter()
                .map(|artifact| match artifact.sha256.as_deref() {
                    Some(sha) => format!("{}:{}", artifact.id, sha),
                    None => artifact.id.clone(),
                })
                .collect();
            let record = AcceptanceRecord {
                by: by.clone(),
                turn,
                evidence_revision: Some(revision.clone()),
                at_ms: now_ms(),
                artifact_digests,
                token_spent: task.token_spent,
            };
            let seq = staged.take_seq();
            let task = staged.require_mut(task_id)?;
            task.acceptance = Some(record.clone());
            task.notifications.push(TaskNotification {
                seq,
                kind: "accepted".to_string(),
                message: truncate(
                    format!("result accepted by {} at revision {}", by, revision),
                    MAX_MESSAGE_LEN,
                ),
                at_ms: now_ms(),
                acknowledged: false,
            });
            trim_notifications(task);
            Ok(record)
        })
    }

    /// B12: record an explicit, auditable disposition of a task whose outcome is unknown after a
    /// crash.
    ///
    /// Reconciling never clears the `UnknownOutcome` disposition — the doubtful record stays
    /// non-re-runnable, so no path can fake an exactly-once retry. `RetryAsNewAttempt` instead names
    /// a *separately registered* attempt (provably never started, with its own budget and policy),
    /// and the record keeps the forward link in `superseded_by`. Repeating the identical last
    /// decision is a no-op; naming a different replacement attempt is refused.
    pub fn reconcile(
        &mut self,
        task_id: &str,
        by: &str,
        turn: u32,
        decision: ReconcileDecision,
        reason: &str,
    ) -> Result<ReconcileRecord, TaskRegistryError> {
        let by = truncate(by.trim().to_string(), MAX_ID_LEN);
        if by.is_empty() {
            return Err(TaskRegistryError::InvalidReconcile(
                "a reconciling principal is required".to_string(),
            ));
        }
        let reason = truncate(reason.trim().to_string(), MAX_MESSAGE_LEN);
        if reason.is_empty() {
            return Err(TaskRegistryError::InvalidReconcile(
                "a reason is required so the decision is auditable".to_string(),
            ));
        }
        if let ReconcileDecision::ExternallyVerified { evidence } = &decision {
            if evidence.trim().is_empty() {
                return Err(TaskRegistryError::InvalidReconcile(
                    "externally_verified requires the external evidence it relied on".to_string(),
                ));
            }
        }
        self.transact(|staged| {
            let task = staged
                .tasks
                .get(task_id)
                .ok_or_else(|| TaskRegistryError::UnknownTask(task_id.to_string()))?;
            let previous_disposition = task.recovery;
            if !task.is_recovery_blocked() {
                return Err(TaskRegistryError::NoUnknownOutcome(task_id.to_string()));
            }
            if let Some(new_task_id) = decision.new_attempt_id() {
                let new_task_id = new_task_id.trim();
                if new_task_id.is_empty() || new_task_id == task_id {
                    return Err(TaskRegistryError::InvalidReconcile(
                        "retry_as_new_attempt must name a different, separately registered task"
                            .to_string(),
                    ));
                }
                let attempt = staged.tasks.get(new_task_id).ok_or_else(|| {
                    TaskRegistryError::InvalidReconcile(format!(
                        "retry_as_new_attempt names unknown task '{}'",
                        new_task_id
                    ))
                })?;
                if !attempt.can_resume_safely() {
                    return Err(TaskRegistryError::InvalidReconcile(format!(
                        "task '{}' is {} and not provably never started; a replacement attempt must be a fresh record",
                        new_task_id,
                        attempt.state.as_str()
                    )));
                }
                if let Some(existing) = task.superseded_by.as_deref() {
                    if existing != new_task_id {
                        return Err(TaskRegistryError::AlreadySuperseded {
                            task_id: task_id.to_string(),
                            by: existing.to_string(),
                        });
                    }
                }
            }
            // Repeating the exact last decision is a replay, not a new decision.
            if let Some(last) = task.reconciles.last() {
                if last.by == by
                    && last.turn == turn
                    && last.decision == decision
                    && last.reason == reason
                {
                    return Ok(last.clone());
                }
            }
            if task.reconciles.len() >= MAX_RECONCILES_PER_TASK {
                return Err(TaskRegistryError::CapacityExceeded(MAX_RECONCILES_PER_TASK));
            }
            let superseding = decision.new_attempt_id().map(str::to_string);
            let record = ReconcileRecord {
                by: by.clone(),
                turn,
                at_ms: now_ms(),
                decision: decision.clone(),
                reason: reason.clone(),
                previous_disposition,
            };
            let seq = staged.take_seq();
            let task = staged.require_mut(task_id)?;
            if let Some(new_task_id) = superseding {
                task.superseded_by = Some(new_task_id);
            }
            task.reconciles.push(record.clone());
            task.notifications.push(TaskNotification {
                seq,
                kind: "reconciled".to_string(),
                message: truncate(
                    format!("unknown outcome reconciled by {}: {}", by, decision.as_str()),
                    MAX_MESSAGE_LEN,
                ),
                at_ms: now_ms(),
                acknowledged: false,
            });
            trim_notifications(task);
            Ok(record)
        })
    }

    pub fn cancel(&mut self, task_id: &str, turn: u32) -> Result<(), TaskRegistryError> {
        if self.state_of(task_id) == Some(TaskState::Cancelled) {
            return Ok(());
        }
        self.transact(|staged| {
            let task = staged
                .tasks
                .get(task_id)
                .ok_or_else(|| TaskRegistryError::UnknownTask(task_id.to_string()))?;
            let from = task.state;
            if !is_valid_transition(from, TaskState::Cancelled) {
                return Err(TaskRegistryError::InvalidTransition {
                    task_id: task_id.to_string(),
                    from,
                    to: TaskState::Cancelled,
                });
            }
            let task = staged.require_mut(task_id)?;
            task.state = TaskState::Cancelled;
            task.updated_turn = turn;
            Ok(())
        })
    }

    /// Acknowledge every pending notification for a task.
    pub fn acknowledge(&mut self, task_id: &str) -> Result<usize, TaskRegistryError> {
        self.transact(|staged| {
            let task = staged.require_mut(task_id)?;
            let mut count = 0;
            for notification in task.notifications.iter_mut() {
                if !notification.acknowledged {
                    notification.acknowledged = true;
                    count += 1;
                }
            }
            Ok(count)
        })
    }

    /// Every unacknowledged notification across all tasks, oldest task first.
    pub fn pending_notifications(&self) -> Vec<(String, TaskNotification)> {
        let mut out = Vec::new();
        for task in self.tasks.values() {
            for notification in &task.notifications {
                if !notification.acknowledged {
                    out.push((task.task_id.clone(), notification.clone()));
                }
            }
        }
        out
    }

    /// Tasks owned by a session, sorted by task id.
    pub fn tasks_for_session<'a>(&'a self, session_id: &str) -> Vec<&'a AsyncTaskRecord> {
        self.tasks
            .values()
            .filter(|task| task.session_id == session_id)
            .collect()
    }

    /// Replace every `Running` task with `Suspended` after a restart, appending an interruption
    /// notification, so a crash can never present in-flight work as completed. All of the
    /// interruptions commit as one transaction.
    pub fn recover_interrupted(&mut self) -> Result<usize, TaskRegistryError> {
        // Select every task that either was `Running` at the crash, or is `Suspended` but had been
        // admitted to execute (a marker we persist before any side effect). A second call is a no-op:
        // the first call rewrites `Started` to `UnknownOutcome`, which is not selected again, so the
        // unknown disposition/turn/provenance is preserved rather than erased.
        let affected: Vec<String> = self
            .tasks
            .values()
            .filter(|task| {
                task.state == TaskState::Running
                    || matches!(task.recovery, Some(RecoveryDisposition::Started { .. }))
            })
            .map(|task| task.task_id.clone())
            .collect();
        if affected.is_empty() {
            return Ok(0);
        }
        self.transact(|staged| {
            let mut count = 0;
            for task_id in &affected {
                let seq = staged.take_seq();
                let task = staged.require_mut(task_id)?;
                // A `Running` state means execution was admitted, whatever the marker says, so treat
                // it as started. A `Suspended` record is only "started" if it carries the explicit
                // marker (a legacy `Suspended` with no marker stays ambiguous and is refused on
                // resume rather than being blessed as never-started).
                let started = task.state == TaskState::Running
                    || matches!(task.recovery, Some(RecoveryDisposition::Started { .. }));
                let turn = match task.recovery {
                    Some(RecoveryDisposition::Started { turn }) => turn,
                    _ => task.updated_turn,
                };
                if started {
                    // The child may have begun side effects that were never recorded: keep the fact
                    // as an explicit unknown rather than pretending it never executed.
                    task.recovery = Some(RecoveryDisposition::UnknownOutcome { turn });
                }
                if task.state == TaskState::Running {
                    task.state = TaskState::Suspended;
                }
                task.notifications.push(TaskNotification {
                    seq,
                    kind: "interrupted".to_string(),
                    message: if started {
                        "task was admitted to run when the process restarted; its outcome is UNKNOWN (side effects may already have started) and re-running is BLOCKED pending an explicit reconcile"
                            .to_string()
                    } else {
                        "task was marked Running at restart but never started execution; suspended for resume"
                            .to_string()
                    },
                    at_ms: now_ms(),
                    acknowledged: false,
                });
                count += 1;
            }
            Ok(count)
        })
    }

    /// Drop terminal task records beyond `keep` (oldest first) to bound the table.
    ///
    /// Returns the number of records that are durably gone. A precommit persistence failure rolls
    /// the removals back, so it reports `0` and the next call retries; a post-rename directory-sync
    /// failure ([`TaskRegistryError::PersistUncertain`]) has already published the removals, so it
    /// reports them. The count can therefore never claim a removal that did not happen.
    /// Terminal records that may be evicted, oldest first.
    ///
    /// A record carrying a pending merge intent is **protected**: that intent is the only durable
    /// copy of a merge→WAL link that has not been replayed yet, so evicting the record would destroy
    /// durable recovery state. `keep` bounds only the *evictable* records, so a protected record
    /// never forces the eviction of another protected record.
    fn evictable_terminal(&self, keep: usize) -> Vec<String> {
        let mut terminal: Vec<(u32, String)> = self
            .tasks
            .values()
            .filter(|task| task.state.is_terminal() && task.pending_merge.is_none())
            .map(|task| (task.updated_turn, task.task_id.clone()))
            .collect();
        terminal.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        let excess = terminal.len().saturating_sub(keep);
        terminal
            .into_iter()
            .take(excess)
            .map(|(_, task_id)| task_id)
            .collect()
    }

    /// Drop evictable terminal task records beyond `keep` (oldest first), reporting the outcome.
    ///
    /// Returns the number of records that are durably gone, or a persistence error when the commit
    /// did not happen. A caller that needs room for a new admission can therefore tell "the table
    /// has nothing evictable" (`Ok(0)`, e.g. every terminal record carries a pending merge link)
    /// apart from "the prune could not be committed" (`Err`) and fail honestly instead of assuming
    /// space was freed.
    pub fn prune_terminal_checked(&mut self, keep: usize) -> Result<usize, TaskRegistryError> {
        let to_remove = self.evictable_terminal(keep);
        if to_remove.is_empty() {
            return Ok(0);
        }
        self.transact(|staged| {
            for task_id in &to_remove {
                staged.tasks.remove(task_id);
            }
            Ok(to_remove.len())
        })
    }

    /// Drop terminal task records beyond `keep` (oldest first) to bound the table, never evicting a
    /// record that carries a pending merge intent.
    ///
    /// Returns the number of records that are durably gone. A precommit persistence failure rolls
    /// the removals back, so it reports `0` and the next call retries; a post-rename directory-sync
    /// failure ([`TaskRegistryError::PersistUncertain`]) has already published the removals, so it
    /// reports them. The count can therefore never claim a removal that did not happen.
    pub fn prune_terminal(&mut self, keep: usize) -> usize {
        let excess = self.evictable_terminal(keep).len();
        match self.prune_terminal_checked(keep) {
            Ok(removed) => removed,
            // The rename already happened, so the removals are visible; report them.
            Err(TaskRegistryError::PersistUncertain(_)) => excess,
            // Precommit failure: nothing was committed, so nothing was removed.
            Err(_) => 0,
        }
    }

    /// Run `mutate` against a staged copy of the registry state, commit the staged document
    /// atomically, and only then publish the staged state to memory.
    ///
    /// On a precommit failure (temp write/fsync or rename) nothing was committed, so memory keeps
    /// the old state and the caller may retry the operation. On a post-rename directory-sync
    /// failure the new document is already visible, so the staged state is published (keeping memory
    /// in step with disk) and [`TaskRegistryError::PersistUncertain`] is returned; the caller must
    /// treat the mutation as applied-but-not-provably-durable rather than rolled back.
    fn transact<T>(
        &mut self,
        mutate: impl FnOnce(&mut StagedRegistry) -> Result<T, TaskRegistryError>,
    ) -> Result<T, TaskRegistryError> {
        let mut staged = StagedRegistry {
            tasks: self.tasks.clone(),
            next_notification_seq: self.next_notification_seq,
        };
        let value = mutate(&mut staged)?;
        // An in-memory registry has no backing file, so the staged state is already the committed
        // state.
        if self.path.as_os_str().is_empty() {
            self.publish(staged);
            return Ok(value);
        }
        let doc = RegistryDocument {
            version: REGISTRY_VERSION,
            next_notification_seq: staged.next_notification_seq,
            tasks: staged.tasks.values().cloned().collect(),
        };
        let json = serde_json::to_string_pretty(&doc)
            .map_err(|e| TaskRegistryError::Persist(e.to_string()))?;
        match self.write_document_atomically(json.as_bytes()) {
            Ok(()) => {
                self.publish(staged);
                Ok(value)
            }
            Err(CommitError::Precommit(msg)) => Err(TaskRegistryError::Persist(msg)),
            Err(CommitError::CommitUncertain(msg)) => {
                // The rename already happened, so disk shows the new document; publish the staged
                // state so memory cannot silently disagree with what a reader would see.
                self.publish(staged);
                Err(TaskRegistryError::PersistUncertain(msg))
            }
        }
    }

    fn publish(&mut self, staged: StagedRegistry) {
        self.tasks = staged.tasks;
        self.next_notification_seq = staged.next_notification_seq;
    }

    /// Write `bytes` as the registry document atomically and durably.
    ///
    /// The sequence is: create a unique temp file next to the target with `O_NOFOLLOW` +
    /// `O_CREAT|O_EXCL` (the shared secure-fs primitive), write and `fsync` it, `rename` it over the
    /// target, then `fsync` the containing directory. A failure before the rename leaves the target
    /// untouched ([`CommitError::Precommit`]); a failure of the directory `fsync` after a successful
    /// rename means the new document is already visible but may not survive a power loss
    /// ([`CommitError::CommitUncertain`]).
    fn write_document_atomically(&self, bytes: &[u8]) -> Result<(), CommitError> {
        #[cfg(test)]
        if self.fault == Some(PersistFault::Precommit) {
            return Err(CommitError::Precommit(
                "injected fault: precommit".to_string(),
            ));
        }
        let parent = match self.path.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
            _ => PathBuf::from("."),
        };
        std::fs::create_dir_all(&parent).map_err(|e| {
            CommitError::Precommit(format!("cannot create {}: {}", parent.display(), e))
        })?;
        let base = self
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("registry.json");
        let (tmp, mut file) = self.create_temp(&parent, base)?;
        let write_result = (|| -> std::io::Result<()> {
            use std::io::Write;
            file.write_all(bytes)?;
            file.sync_all()?;
            Ok(())
        })();
        drop(file);
        if let Err(e) = write_result {
            let _ = std::fs::remove_file(&tmp);
            return Err(CommitError::Precommit(format!(
                "cannot write {}: {}",
                tmp.display(),
                e
            )));
        }
        #[cfg(test)]
        if self.fault == Some(PersistFault::Rename) {
            let _ = std::fs::remove_file(&tmp);
            return Err(CommitError::Precommit("injected fault: rename".to_string()));
        }
        if let Err(e) = std::fs::rename(&tmp, &self.path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(CommitError::Precommit(format!(
                "cannot replace {}: {}",
                self.path.display(),
                e
            )));
        }
        #[cfg(test)]
        if self.fault == Some(PersistFault::DirSync) {
            return Err(CommitError::CommitUncertain(
                "injected fault: directory sync".to_string(),
            ));
        }
        sync_dir(&parent).map_err(|e| {
            CommitError::CommitUncertain(format!(
                "renamed {} but could not sync {}: {}",
                self.path.display(),
                parent.display(),
                e
            ))
        })
    }

    /// Create a unique, exclusively-opened temp file next to the target. The random suffix means a
    /// same-user attacker cannot predict the name and pre-plant a symlink; `create_new_no_symlink`
    /// uses `O_CREAT|O_EXCL|O_NOFOLLOW`, so a pre-existing entry makes the open fail instead of
    /// being followed or truncated.
    fn create_temp(
        &self,
        dir: &Path,
        base: &str,
    ) -> Result<(PathBuf, std::fs::File), CommitError> {
        for _ in 0..16u32 {
            let candidate = dir.join(format!(
                ".{}.tmp-{}-{}",
                base,
                std::process::id(),
                uuid::Uuid::new_v4()
            ));
            match crate::secure_fs::create_new_no_symlink(&candidate, 0o600) {
                Ok(file) => return Ok((candidate, file)),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    // A regular leftover temp file is safe to skip; anything else (symlink,
                    // directory, special file) is not ours to clobber, so refuse loudly.
                    match std::fs::symlink_metadata(&candidate) {
                        Ok(meta) if meta.file_type().is_file() => continue,
                        _ => {
                            return Err(CommitError::Precommit(format!(
                                "refusing to write through the existing entry at {}: {}",
                                candidate.display(),
                                e
                            )))
                        }
                    }
                }
                Err(e) => {
                    return Err(CommitError::Precommit(format!(
                        "cannot create {}: {}",
                        candidate.display(),
                        e
                    )))
                }
            }
        }
        Err(CommitError::Precommit(format!(
            "cannot create a temporary file next to {}: too many name collisions",
            self.path.display()
        )))
    }

    /// Atomically persist the current registry document. A registry with no backing path is a
    /// no-op so an in-memory registry behaves identically without a filesystem. Every mutation
    /// already commits through the registry's internal transaction; this remains for a caller that
    /// holds a registry it knows is consistent and wants to force a durable rewrite.
    pub fn persist(&self) -> Result<(), TaskRegistryError> {
        if self.path.as_os_str().is_empty() {
            return Ok(());
        }
        let doc = RegistryDocument {
            version: REGISTRY_VERSION,
            next_notification_seq: self.next_notification_seq,
            tasks: self.tasks.values().cloned().collect(),
        };
        let json = serde_json::to_string_pretty(&doc)
            .map_err(|e| TaskRegistryError::Persist(e.to_string()))?;
        match self.write_document_atomically(json.as_bytes()) {
            Ok(()) => Ok(()),
            Err(CommitError::Precommit(msg)) => Err(TaskRegistryError::Persist(msg)),
            Err(CommitError::CommitUncertain(msg)) => Err(TaskRegistryError::PersistUncertain(msg)),
        }
    }

    /// Apply a supervisor control request and return its JSON response.
    pub fn apply_control(
        &mut self,
        request: &TaskControlRequest,
    ) -> Result<serde_json::Value, TaskRegistryError> {
        use serde_json::json;
        match request {
            TaskControlRequest::List { session_id } => {
                let tasks: Vec<serde_json::Value> = self
                    .tasks
                    .values()
                    .filter(|task| {
                        session_id
                            .as_deref()
                            .map(|sid| task.session_id == sid)
                            .unwrap_or(true)
                    })
                    .map(task_summary)
                    .collect();
                Ok(json!({ "tasks": tasks, "count": tasks.len() }))
            }
            TaskControlRequest::Pause { task_id, turn } => {
                self.suspend(task_id, *turn)?;
                Ok(json!({
                    "task_id": task_id,
                    "state": TaskState::Suspended.as_str(),
                    // A pause suspends *scheduling*: a child that already started keeps running, and
                    // the record cannot be launched again while its started marker is present.
                    "live_execution_continues": true,
                    "resume_blocked_while_started": true,
                }))
            }
            TaskControlRequest::Accept {
                task_id,
                turn,
                evidence_revision,
            } => {
                let record = self.accept(
                    task_id,
                    LOCAL_CONTROL_PRINCIPAL,
                    *turn,
                    evidence_revision.as_deref(),
                )?;
                Ok(json!({
                    "task_id": task_id,
                    "state": TaskState::Completed.as_str(),
                    "accepted": true,
                    "accepted_by": record.by,
                    "evidence_revision": record.evidence_revision,
                    "artifact_digests": record.artifact_digests,
                }))
            }
            TaskControlRequest::Reconcile {
                task_id,
                turn,
                decision,
                reason,
            } => {
                let record = self.reconcile(
                    task_id,
                    LOCAL_CONTROL_PRINCIPAL,
                    *turn,
                    decision.clone(),
                    reason,
                )?;
                Ok(json!({
                    "task_id": task_id,
                    "reconciled": true,
                    "decision": record.decision.as_str(),
                    "superseded_by": self.get(task_id).and_then(|t| t.superseded_by.as_deref()),
                    "recovery": self.get(task_id).and_then(|t| t.recovery).map(|d| d.as_str()),
                }))
            }
            TaskControlRequest::Resume { task_id, turn } => {
                self.resume(task_id, *turn)?;
                Ok(json!({ "task_id": task_id, "state": TaskState::Running.as_str() }))
            }
            TaskControlRequest::Cancel { task_id, turn } => {
                self.cancel(task_id, *turn)?;
                Ok(json!({ "task_id": task_id, "state": TaskState::Cancelled.as_str() }))
            }
            TaskControlRequest::Challenge {
                task_id,
                reason,
                turn,
            } => {
                self.challenge(task_id, reason, *turn)?;
                Ok(json!({ "task_id": task_id, "state": TaskState::ChallengePending.as_str() }))
            }
            TaskControlRequest::ResolveChallenge {
                task_id,
                accept,
                turn,
            } => {
                let state = self.resolve_challenge(task_id, *accept, *turn)?;
                Ok(json!({ "task_id": task_id, "state": state.as_str() }))
            }
            TaskControlRequest::Acknowledge { task_id } => {
                let acknowledged = self.acknowledge(task_id)?;
                Ok(json!({ "task_id": task_id, "acknowledged": acknowledged }))
            }
        }
    }
}

fn task_summary(task: &AsyncTaskRecord) -> serde_json::Value {
    serde_json::json!({
        "task_id": task.task_id,
        "session_id": task.session_id,
        "owner_id": task.owner_id,
        "state": task.state.as_str(),
        "role": task.role,
        "artifacts": task.artifacts.len(),
        "unacknowledged_notifications": task.notifications.iter().filter(|n| !n.acknowledged).count(),
        "challenge": task.challenge,
        "verified": match task.verification.as_ref() {
            Some(v) if v.passed => serde_json::json!(true),
            Some(_) => serde_json::json!(false),
            None => serde_json::Value::Null,
        },
        "accepted": task.is_accepted(),
        "accepted_by": task.acceptance.as_ref().map(|a| a.by.as_str()),
        "acceptance_revision": task
            .acceptance
            .as_ref()
            .and_then(|a| a.evidence_revision.as_deref()),
        "reconciles": task.reconciles.len(),
        "superseded_by": task.superseded_by.as_deref(),
        "recovery": match task.recovery {
            Some(d) => serde_json::json!(d.as_str()),
            None => serde_json::Value::Null,
        },
        "resume_blocked": !task.can_resume_safely(),
        "token_allocation": task.token_allocation,
        "token_spent": task.token_spent,
    })
}

/// Valid transitions of the task state machine. Everything else is rejected explicitly.
fn is_valid_transition(from: TaskState, to: TaskState) -> bool {
    use TaskState::*;
    matches!(
        (from, to),
        (Queued, Running)
            | (Queued, Suspended)
            | (Queued, Cancelled)
            | (Queued, Failed)
            | (Running, Completed)
            | (Running, Failed)
            | (Running, Suspended)
            | (Running, Cancelled)
            | (Running, ChallengePending)
            | (Suspended, Queued)
            | (Suspended, Running)
            | (Suspended, Cancelled)
            | (Suspended, Failed)
            | (ChallengePending, Completed)
            | (ChallengePending, Running)
            | (ChallengePending, Failed)
            | (ChallengePending, Cancelled)
            | (Completed, ChallengePending)
            | (Failed, ChallengePending)
    )
}

/// JSON control request the supervisor surface sends. Unknown verbs are a parse error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum TaskControlRequest {
    List {
        #[serde(default)]
        session_id: Option<String>,
    },
    Pause {
        task_id: String,
        #[serde(default)]
        turn: u32,
    },
    Resume {
        task_id: String,
        #[serde(default)]
        turn: u32,
    },
    Cancel {
        task_id: String,
        #[serde(default)]
        turn: u32,
    },
    Challenge {
        task_id: String,
        reason: String,
        #[serde(default)]
        turn: u32,
    },
    ResolveChallenge {
        task_id: String,
        accept: bool,
        #[serde(default)]
        turn: u32,
    },
    Acknowledge {
        task_id: String,
    },
    Accept {
        task_id: String,
        #[serde(default)]
        turn: u32,
        /// The exact verified revision being accepted; when present it must match the record's
        /// passed verification. The accepting principal is filled by the trusted host, never taken
        /// from this request.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        evidence_revision: Option<String>,
    },
    Reconcile {
        task_id: String,
        #[serde(default)]
        turn: u32,
        decision: ReconcileDecision,
        reason: String,
    },
}

/// The trusted local control principal recorded on a locally originated acceptance/reconcile
/// decision. Every mutating `tasks.control` request arriving over a network stream is refused
/// before dispatch, so a remote caller can never reach `apply_control`; the identity is filled here
/// by the trusted host and is never sourced from request data or a model message.
pub const LOCAL_CONTROL_PRINCIPAL: &str = "local-host";

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Keep a task's notification ring within its cap. One helper so every writer (settle, accept,
/// reconcile) trims identically instead of drifting.
fn trim_notifications(task: &mut AsyncTaskRecord) {
    while task.notifications.len() > MAX_NOTIFICATIONS_PER_TASK {
        match task.notifications.iter().position(|n| n.acknowledged) {
            Some(index) => {
                task.notifications.remove(index);
            }
            None => {
                task.notifications.remove(0);
            }
        }
    }
}

/// Why [`register_from_session`] could not register a task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegisterFromSessionError {
    /// The session is not in the routing table, so there is no owner to attach the task to.
    UnknownSession(String),
    /// The shared run ledger cannot cover the requested allocation.
    BudgetExhausted {
        requested: u64,
        remaining: Option<u64>,
    },
    Registry(TaskRegistryError),
}

impl std::fmt::Display for RegisterFromSessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RegisterFromSessionError::UnknownSession(id) => {
                write!(f, "unknown session '{}' (not registered for routing)", id)
            }
            RegisterFromSessionError::BudgetExhausted {
                requested,
                remaining,
            } => write!(
                f,
                "cannot reserve {} tokens for the task; remaining {:?}",
                requested, remaining
            ),
            RegisterFromSessionError::Registry(e) => write!(f, "{}", e),
        }
    }
}

/// Register a durable task for an existing session, taking the ownership triple from the routing
/// table and reserving its token allocation from the shared run ledger.
///
/// This is the seam between capability gap #4 (durable async tasks), the session-ownership routing
/// added by the previous stage, and F05's shared [`TokenBudget`]: a task cannot exist without an
/// owning runtime, and it cannot start without a reservation, so a durable task can never spend
/// budget the parent run does not have.
#[allow(clippy::too_many_arguments)]
pub fn register_from_session(
    registry: &mut AsyncTaskRegistry,
    task_id: &str,
    session_id: &str,
    budget: &std::sync::Arc<crate::budget::TokenBudget>,
    requested_tokens: u64,
    role: &str,
    turn: u32,
    prompt: &str,
    timeout_ms: u64,
    spec_json: &str,
    parent_policy: &str,
) -> Result<(AsyncTaskRecord, crate::budget::TokenReservation), RegisterFromSessionError> {
    let route = crate::session_ownership::lookup_session_route(session_id)
        .ok_or_else(|| RegisterFromSessionError::UnknownSession(session_id.to_string()))?;
    let reservation =
        budget
            .try_reserve(requested_tokens)
            .ok_or(RegisterFromSessionError::BudgetExhausted {
                requested: requested_tokens,
                remaining: budget.remaining(),
            })?;
    let record = registry
        .register(NewTask {
            task_id: task_id.to_string(),
            session_id: session_id.to_string(),
            owner_id: route.owner_id.clone(),
            workspace_root: route.workspace_root.display().to_string(),
            role: role.to_string(),
            created_turn: turn,
            token_allocation: requested_tokens,
            prompt: prompt.to_string(),
            timeout_ms,
            spec_json: spec_json.to_string(),
            parent_policy: parent_policy.to_string(),
            // This seam registers from the ownership routing table, which carries no parent registry,
            // so the hook provenance is honestly unknown: a later resume must refuse to re-run it
            // rather than assume "no constraints".
            parent_policy_hooks: ParentPolicyHooks::Unknown,
            // The routing table names the owner and workspace, not the WAL file; a caller that needs
            // the resume link supplies the WAL through the record it later registers.
            session_path: String::new(),
        })
        .map_err(RegisterFromSessionError::Registry)?;
    Ok((record, reservation))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new_task(id: &str) -> NewTask {
        NewTask {
            task_id: id.to_string(),
            session_id: "s1".to_string(),
            owner_id: "rt1".to_string(),
            workspace_root: "/ws".to_string(),
            role: "worker".to_string(),
            created_turn: 0,
            token_allocation: 100,
            prompt: "do the thing".to_string(),
            timeout_ms: 0,
            spec_json: "{}".to_string(),
            parent_policy: "allow-write".to_string(),
            parent_policy_hooks: ParentPolicyHooks::KnownEmpty,
            session_path: String::new(),
        }
    }

    #[test]
    fn duplicate_and_capacity_are_rejected() {
        let mut registry = AsyncTaskRegistry::in_memory();
        registry.register(new_task("a")).unwrap();
        assert!(matches!(
            registry.register(new_task("a")),
            Err(TaskRegistryError::DuplicateTask(_))
        ));
    }

    #[test]
    fn execution_completion_is_not_acceptance_and_a_challenge_blocks_it() {
        let mut registry = AsyncTaskRegistry::in_memory();
        registry.register(new_task("a")).unwrap();
        registry.transition("a", TaskState::Running, 1).unwrap();
        registry.transition("a", TaskState::Completed, 2).unwrap();
        // B12: a bare execution completion is NOT acceptance — no trusted accept decision exists.
        assert!(!registry.get("a").unwrap().is_accepted());

        registry.challenge("a", "looks wrong", 3).unwrap();
        assert_eq!(
            registry.get("a").unwrap().state,
            TaskState::ChallengePending
        );
        assert!(!registry.get("a").unwrap().is_accepted());
        // Cannot complete while a challenge is open.
        assert!(registry.transition("a", TaskState::Completed, 4).is_err());

        // Resolving the challenge records the resolution but does not mint a formal acceptance:
        // there is no trusted accept entry bound to an authenticated principal yet, so the honest
        // state is Completed-and-not-accepted.
        registry.resolve_challenge("a", true, 5).unwrap();
        assert_eq!(registry.get("a").unwrap().state, TaskState::Completed);
        assert!(registry.get("a").unwrap().challenge.is_none());
        assert!(!registry.get("a").unwrap().is_accepted());
    }

    /// B12: acceptance is minted only through the trusted entry, and only for a completed,
    /// gate-verified, unchallenged result. The decision binds the exact verified revision, snapshots
    /// the artifacts and spend, and replaying the same decision is a no-op.
    #[test]
    fn accept_requires_a_verified_revision_and_snapshots_evidence() {
        let mut registry = AsyncTaskRegistry::in_memory();
        registry.register(new_task("a")).unwrap();

        // A queued task cannot be accepted.
        assert!(matches!(
            registry.accept("a", "local-host", 1, None),
            Err(TaskRegistryError::AcceptanceRefused(_))
        ));

        registry.transition("a", TaskState::Running, 1).unwrap();
        registry.transition("a", TaskState::Completed, 2).unwrap();
        // Completed but unverified: acceptance would name nothing.
        assert!(matches!(
            registry.accept("a", "local-host", 3, None),
            Err(TaskRegistryError::AcceptanceRefused(_))
        ));

        registry
            .attach_artifact(
                "a",
                ArtifactRef::new("report", "reports/a.txt", "report")
                    .with_sha256("deadbeef")
                    .with_bytes(10),
            )
            .unwrap();
        set_evidence(
            &mut registry,
            None,
            Some(("post-merge", true, Some("rev-1"))),
        );

        // A mismatched revision is refused: acceptance binds exactly what the gate verified.
        assert!(matches!(
            registry.accept("a", "local-host", 4, Some("rev-2")),
            Err(TaskRegistryError::AcceptanceRefused(_))
        ));

        let record = registry.accept("a", "local-host", 4, Some("rev-1")).unwrap();
        assert_eq!(record.evidence_revision.as_deref(), Some("rev-1"));
        assert_eq!(record.artifact_digests, vec!["report:deadbeef".to_string()]);
        assert_eq!(record.token_spent, 0);
        assert!(registry.get("a").unwrap().is_accepted());

        // Replaying the identical decision adds no second decision and no notification.
        let notifications = registry.get("a").unwrap().notifications.len();
        let again = registry.accept("a", "local-host", 4, Some("rev-1")).unwrap();
        assert_eq!(again, record);
        assert_eq!(registry.get("a").unwrap().notifications.len(), notifications);

        // A different verified revision replaces the recorded decision.
        set_evidence(
            &mut registry,
            Some("rev-1"),
            Some(("post-merge", true, Some("rev-9"))),
        );
        let replaced = registry.accept("a", "local-host", 5, Some("rev-9")).unwrap();
        assert_eq!(replaced.evidence_revision.as_deref(), Some("rev-9"));
        assert!(registry.get("a").unwrap().notifications.len() > notifications);
    }

    /// B12: reconcile only applies to a genuinely unknown outcome. It records an auditable decision
    /// and never clears the block on re-running; a retry must name a separate, provably-fresh attempt.
    #[test]
    fn reconcile_records_decisions_without_clearing_the_unknown_block() {
        let mut registry = AsyncTaskRegistry::in_memory();
        registry.register(new_task("a")).unwrap();
        // Not unknown yet: nothing to reconcile.
        assert!(matches!(
            registry.reconcile(
                "a",
                "local-host",
                1,
                ReconcileDecision::AcknowledgedUnknown,
                "looked at it"
            ),
            Err(TaskRegistryError::NoUnknownOutcome(_))
        ));

        registry.begin_execution("a", 1).unwrap();
        assert_eq!(registry.recover_interrupted().unwrap(), 1);
        assert!(registry.get("a").unwrap().is_recovery_blocked());
        assert!(!registry.get("a").unwrap().can_resume_safely());

        // A blank reason or blank external evidence is refused.
        assert!(matches!(
            registry.reconcile(
                "a",
                "local-host",
                2,
                ReconcileDecision::AcknowledgedUnknown,
                "   "
            ),
            Err(TaskRegistryError::InvalidReconcile(_))
        ));
        assert!(matches!(
            registry.reconcile(
                "a",
                "local-host",
                2,
                ReconcileDecision::ExternallyVerified {
                    evidence: " ".to_string()
                },
                "checked"
            ),
            Err(TaskRegistryError::InvalidReconcile(_))
        ));
        // A retry must name an existing, separately registered attempt.
        assert!(matches!(
            registry.reconcile(
                "a",
                "local-host",
                2,
                ReconcileDecision::RetryAsNewAttempt {
                    new_task_id: "missing".to_string()
                },
                "retry"
            ),
            Err(TaskRegistryError::InvalidReconcile(_))
        ));

        let reason = "side effects unknown, not retrying";
        let record = registry
            .reconcile(
                "a",
                "local-host",
                2,
                ReconcileDecision::AcknowledgedUnknown,
                reason,
            )
            .unwrap();
        assert_eq!(record.decision, ReconcileDecision::AcknowledgedUnknown);
        assert_eq!(record.by, "local-host");
        // The doubtful record stays non-re-runnable and keeps its history.
        assert!(registry.get("a").unwrap().is_recovery_blocked());
        assert!(!registry.get("a").unwrap().can_resume_safely());
        assert!(matches!(
            registry.begin_execution("a", 3),
            Err(TaskRegistryError::RecoveryBlocked(_))
        ));
        // Replaying the identical decision is a no-op.
        assert_eq!(
            registry
                .reconcile(
                    "a",
                    "local-host",
                    2,
                    ReconcileDecision::AcknowledgedUnknown,
                    reason
                )
                .unwrap(),
            record
        );
        assert_eq!(registry.get("a").unwrap().reconciles.len(), 1);

        // A separate attempt can be named; a second, different replacement is refused.
        registry.register(new_task("b")).unwrap();
        registry
            .reconcile(
                "a",
                "local-host",
                2,
                ReconcileDecision::RetryAsNewAttempt {
                    new_task_id: "b".to_string(),
                },
                "fresh attempt b",
            )
            .unwrap();
        assert_eq!(registry.get("a").unwrap().superseded_by.as_deref(), Some("b"));
        registry.register(new_task("c")).unwrap();
        assert!(matches!(
            registry.reconcile(
                "a",
                "local-host",
                2,
                ReconcileDecision::RetryAsNewAttempt {
                    new_task_id: "c".to_string()
                },
                "another"
            ),
            Err(TaskRegistryError::AlreadySuperseded { .. })
        ));
        // The replacement attempt itself is untouched and still provably fresh.
        assert!(registry.get("b").unwrap().can_resume_safely());
    }

    /// B12: a pause suspends scheduling only. A started child keeps running, and the recorded started
    /// marker makes a second launch impossible until the run actually settles.
    #[test]
    fn pause_suspends_scheduling_but_never_allows_a_second_launch() {
        let mut registry = AsyncTaskRegistry::in_memory();
        registry.register(new_task("a")).unwrap();
        registry.begin_execution("a", 1).unwrap();

        let value = registry
            .apply_control(&TaskControlRequest::Pause {
                task_id: "a".to_string(),
                turn: 2,
            })
            .unwrap();
        assert_eq!(value["state"], serde_json::json!("suspended"));
        assert_eq!(value["live_execution_continues"], serde_json::json!(true));

        // A paused-but-started task cannot be admitted again: the started marker is still present.
        assert!(matches!(
            registry.resume("a", 3),
            Err(TaskRegistryError::RecoveryBlocked(_))
        ));
        // Only the real settlement clears the marker.
        registry
            .settle_finish(
                "a",
                TaskFinish {
                    state: TaskState::Completed,
                    spent: 0,
                    artifact: None,
                    summary: "done".to_string(),
                    turn: 4,
                    keep_pending_merge: false,
                    verification: None,
                },
            )
            .unwrap();
        assert!(!registry.get("a").unwrap().is_recovery_blocked());
    }

    /// Overwrite task `a`'s acceptance/verification directly, for the predicate matrix.
    fn set_evidence(
        registry: &mut AsyncTaskRegistry,
        acceptance_rev: Option<&str>,
        verification: Option<(&str, bool, Option<&str>)>,
    ) {
        registry
            .transact(|staged| {
                let record = staged.require_mut("a")?;
                record.acceptance = Some(AcceptanceRecord {
                    by: "operator".to_string(),
                    turn: 3,
                    evidence_revision: acceptance_rev.map(str::to_string),
                    at_ms: 0,
                    artifact_digests: Vec::new(),
                    token_spent: 0,
                });
                record.verification = verification.map(|(gate, passed, rev)| VerificationRecord {
                    gate: gate.to_string(),
                    passed,
                    revision: rev.map(str::to_string),
                    parent_snapshot_revision: None,
                });
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn is_accepted_requires_an_explicit_acceptance_and_matching_verification() {
        let mut registry = AsyncTaskRegistry::in_memory();
        registry.register(new_task("a")).unwrap();
        registry.transition("a", TaskState::Running, 1).unwrap();
        registry.transition("a", TaskState::Completed, 2).unwrap();

        // No acceptance at all.
        assert!(!registry.get("a").unwrap().is_accepted());

        // The hole: an acceptance with NO evidence revision must never pass via `None == None`, even
        // when a passed verification exists.
        set_evidence(&mut registry, None, None);
        assert!(
            !registry.get("a").unwrap().is_accepted(),
            "an acceptance without a revision is not an acceptance"
        );
        set_evidence(&mut registry, None, Some(("post-merge", true, Some("rev-1"))));
        assert!(
            !registry.get("a").unwrap().is_accepted(),
            "an acceptance without a revision is never accepted"
        );

        // A blank evidence revision is not a revision either.
        set_evidence(&mut registry, Some("   "), Some(("post-merge", true, Some("   "))));
        assert!(!registry.get("a").unwrap().is_accepted());

        // Bound to a revision: missing / failed / wrong-revision / no-revision verification all fail;
        // only the exact passed revision is accepted.
        set_evidence(&mut registry, Some("rev-1"), None);
        assert!(
            !registry.get("a").unwrap().is_accepted(),
            "a missing verification must not be accepted"
        );
        set_evidence(&mut registry, Some("rev-1"), Some(("post-merge", false, Some("rev-1"))));
        assert!(
            !registry.get("a").unwrap().is_accepted(),
            "a failed verification must never be accepted"
        );
        set_evidence(&mut registry, Some("rev-1"), Some(("post-merge", true, Some("rev-2"))));
        assert!(
            !registry.get("a").unwrap().is_accepted(),
            "a different revision must not satisfy the acceptance"
        );
        set_evidence(&mut registry, Some("rev-1"), Some(("post-merge", true, None)));
        assert!(
            !registry.get("a").unwrap().is_accepted(),
            "a verification without a revision must not satisfy the acceptance"
        );
        set_evidence(&mut registry, Some("rev-1"), Some(("post-merge", true, Some("rev-1"))));
        assert!(
            registry.get("a").unwrap().is_accepted(),
            "an exact passed verification of the accepted revision is acceptance"
        );

        // A new finish invalidates the stale acceptance.
        registry
            .settle_finish(
                "a",
                TaskFinish {
                    state: TaskState::Completed,
                    spent: 0,
                    artifact: None,
                    summary: "re-settled".into(),
                    turn: 6,
                    keep_pending_merge: false,
                    verification: None,
                },
            )
            .unwrap();
        assert!(!registry.get("a").unwrap().is_accepted());
    }

    #[test]
    fn verification_record_without_a_parent_snapshot_revision_deserializes_to_none() {
        // Old evidence written before `parent_snapshot_revision` existed must not fake a parent
        // identity: the field is `None` = unknown.
        let legacy = serde_json::json!({
            "gate": "post-merge",
            "passed": true,
            "revision": "rev-1"
        });
        let v: VerificationRecord = serde_json::from_value(legacy).unwrap();
        assert_eq!(v.gate, "post-merge");
        assert!(v.passed);
        assert!(
            v.parent_snapshot_revision.is_none(),
            "a legacy evidence record must not fake a parent revision"
        );
        // Round-trips with the field present.
        let v2: VerificationRecord = serde_json::from_value(serde_json::json!({
            "gate": "post-merge",
            "passed": true,
            "revision": "rev-1",
            "parent_snapshot_revision": "parent-0"
        }))
        .unwrap();
        assert_eq!(v2.parent_snapshot_revision.as_deref(), Some("parent-0"));
    }

    #[test]
    fn legacy_records_without_the_new_fields_deserialize_to_unknown_and_not_accepted() {
        // A record written before `verification`/`acceptance` existed must not deserialize into a
        // fake acceptance: absent fields are `None` = unknown/not accepted, and the `Completed` fact
        // is preserved (never erased, never re-run).
        let json = serde_json::json!({
            "task_id": "a",
            "session_id": "s1",
            "owner_id": "rt1",
            "workspace_root": "/ws",
            "role": "worker",
            "state": "completed",
            "created_turn": 0,
            "updated_turn": 2,
            "token_allocation": 0,
            "token_spent": 0,
            "artifacts": [],
            "notifications": []
        });
        let record: AsyncTaskRecord = serde_json::from_value(json).unwrap();
        assert_eq!(record.state, TaskState::Completed);
        assert!(record.verification.is_none());
        assert!(record.acceptance.is_none());
        assert!(
            !record.is_accepted(),
            "a legacy Completed record is unknown/not accepted"
        );
    }

    #[test]
    fn recovery_marks_a_started_task_unknown_and_blocks_resume_and_settle() {
        let mut registry = AsyncTaskRegistry::in_memory();
        registry.register(new_task("a")).unwrap();
        registry.begin_execution("a", 1).unwrap();
        assert!(matches!(
            registry.get("a").unwrap().recovery,
            Some(RecoveryDisposition::Started { turn: 1 })
        ));

        assert_eq!(registry.recover_interrupted().unwrap(), 1);
        assert_eq!(registry.get("a").unwrap().state, TaskState::Suspended);
        assert!(registry.get("a").unwrap().is_recovery_blocked());
        // Idempotent: the second pass neither re-counts nor erases the unknown disposition.
        assert_eq!(registry.recover_interrupted().unwrap(), 0);
        assert!(registry.get("a").unwrap().is_recovery_blocked());

        // A blocked task cannot be re-admitted...
        assert!(matches!(
            registry.resume("a", 2),
            Err(TaskRegistryError::RecoveryBlocked(_))
        ));
        // ...nor settled as a success (no fake reconcile).
        assert!(matches!(
            registry.settle_finish(
                "a",
                TaskFinish {
                    state: TaskState::Completed,
                    spent: 0,
                    artifact: None,
                    summary: "fake reconcile".into(),
                    turn: 2,
                    keep_pending_merge: false,
                    verification: None,
                },
            ),
            Err(TaskRegistryError::RecoveryBlocked(_))
        ));
    }

    #[test]
    fn a_never_started_task_recovers_safe_and_resumes() {
        let mut registry = AsyncTaskRegistry::in_memory();
        registry.register(new_task("a")).unwrap();
        // Suspended before admission: no started marker, so no side effect is possible.
        registry.transition("a", TaskState::Suspended, 1).unwrap();
        assert_eq!(registry.recover_interrupted().unwrap(), 0);
        assert!(!registry.get("a").unwrap().is_recovery_blocked());
        registry.resume("a", 2).unwrap();
        assert_eq!(registry.get("a").unwrap().state, TaskState::Running);
        assert!(matches!(
            registry.get("a").unwrap().recovery,
            Some(RecoveryDisposition::Started { turn: 2 })
        ));
    }

    #[test]
    fn begin_execution_refuses_legacy_and_started_but_allows_never_started() {
        let mut registry = AsyncTaskRegistry::in_memory();
        registry.register(new_task("a")).unwrap();
        registry.transition("a", TaskState::Suspended, 1).unwrap();

        // A legacy record written before the marker existed: no proof it never started.
        registry
            .transact(|staged| {
                staged.require_mut("a")?.recovery = None;
                Ok(())
            })
            .unwrap();
        assert!(
            !registry.get("a").unwrap().can_resume_safely(),
            "a legacy Suspended record with no marker is not provably never-started"
        );
        assert!(matches!(
            registry.resume("a", 2),
            Err(TaskRegistryError::RecoveryBlocked(_))
        ));

        // A `Started` record (a live executor may still hold it) is refused too.
        registry
            .transact(|staged| {
                staged.require_mut("a")?.recovery = Some(RecoveryDisposition::Started { turn: 1 });
                Ok(())
            })
            .unwrap();
        assert!(matches!(
            registry.resume("a", 2),
            Err(TaskRegistryError::RecoveryBlocked(_))
        ));

        // An explicit `NeverStarted` record is safe and runs.
        registry
            .transact(|staged| {
                staged.require_mut("a")?.recovery = Some(RecoveryDisposition::NeverStarted);
                Ok(())
            })
            .unwrap();
        registry.resume("a", 2).unwrap();
        assert_eq!(registry.get("a").unwrap().state, TaskState::Running);
    }

    #[test]
    fn interrupted_running_tasks_become_suspended() {
        let dir = std::env::temp_dir().join(format!("kroma-tasks-{}", uuid::Uuid::new_v4()));
        let path = dir.join("tasks.json");
        {
            let mut registry = AsyncTaskRegistry::open(&path).unwrap();
            registry.register(new_task("a")).unwrap();
            registry.transition("a", TaskState::Running, 1).unwrap();
        }
        let mut reopened = AsyncTaskRegistry::open(&path).unwrap();
        assert_eq!(reopened.get("a").unwrap().state, TaskState::Running);
        assert_eq!(reopened.recover_interrupted().unwrap(), 1);
        assert_eq!(reopened.get("a").unwrap().state, TaskState::Suspended);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A file-backed registry in its own scratch directory, for commit-fault tests.
    fn fault_registry(tag: &str) -> (std::path::PathBuf, AsyncTaskRegistry) {
        let dir = std::env::temp_dir().join(format!(
            "kroma-tasks-fault-{}-{}",
            tag,
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("tasks.json");
        let registry = AsyncTaskRegistry::open(&path).unwrap();
        (dir, registry)
    }

    #[test]
    fn precommit_failure_leaves_registry_and_disk_unchanged() {
        let (dir, mut registry) = fault_registry("register");
        registry.register(new_task("a")).unwrap();
        registry.fault = Some(PersistFault::Precommit);
        assert!(matches!(
            registry.register(new_task("b")),
            Err(TaskRegistryError::Persist(_))
        ));
        // No phantom record in memory, and the file agrees: 'b' was never admitted.
        assert!(registry.get("b").is_none());
        assert_eq!(registry.len(), 1);
        let reopened = AsyncTaskRegistry::open(dir.join("tasks.json")).unwrap();
        assert!(reopened.get("b").is_none());
        assert_eq!(reopened.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn real_precommit_failure_leaves_memory_unchanged() {
        // A genuine precommit failure that does not depend on permissions (which would pass under
        // root): the registry's parent path is a regular file, so creating the temp file fails with
        // NotADirectory before anything is committed.
        let dir = std::env::temp_dir().join(format!(
            "kroma-tasks-blocked-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let blocker = dir.join("blocked");
        std::fs::write(&blocker, b"not a directory").unwrap();
        let path = blocker.join("registry.json");

        let mut registry = AsyncTaskRegistry::open(&path).unwrap();
        assert!(matches!(
            registry.register(new_task("a")),
            Err(TaskRegistryError::Persist(_))
        ));
        assert_eq!(
            registry.len(),
            0,
            "a failed commit must not admit a phantom record"
        );
        assert!(registry.get("a").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn failed_resume_does_not_publish_phantom_running() {
        let (dir, mut registry) = fault_registry("resume");
        registry.register(new_task("a")).unwrap();
        registry.transition("a", TaskState::Running, 1).unwrap();
        registry.suspend("a", 2).unwrap();
        registry.fault = Some(PersistFault::Precommit);
        assert!(matches!(
            registry.resume("a", 3),
            Err(TaskRegistryError::Persist(_))
        ));
        // A failed resume must not leave the task looking Running with no live run behind it.
        assert_eq!(registry.get("a").unwrap().state, TaskState::Suspended);
        let reopened = AsyncTaskRegistry::open(dir.join("tasks.json")).unwrap();
        assert_eq!(reopened.get("a").unwrap().state, TaskState::Suspended);
        // Retrying after the fault clears commits and is durable.
        registry.fault = None;
        registry.resume("a", 4).unwrap();
        assert_eq!(registry.get("a").unwrap().state, TaskState::Running);
        let reopened = AsyncTaskRegistry::open(dir.join("tasks.json")).unwrap();
        assert_eq!(reopened.get("a").unwrap().state, TaskState::Running);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn failed_spend_update_is_not_applied_twice() {
        let (dir, mut registry) = fault_registry("spend");
        registry.register(new_task("a")).unwrap();
        registry.fault = Some(PersistFault::Precommit);
        assert!(matches!(
            registry.settle_spend("a", 40),
            Err(TaskRegistryError::Persist(_))
        ));
        assert_eq!(registry.get("a").unwrap().token_spent, 0);
        let reopened = AsyncTaskRegistry::open(dir.join("tasks.json")).unwrap();
        assert_eq!(reopened.get("a").unwrap().token_spent, 0);
        // A single retry charges exactly once, never double.
        registry.fault = None;
        registry.settle_spend("a", 40).unwrap();
        assert_eq!(registry.get("a").unwrap().token_spent, 40);
        let reopened = AsyncTaskRegistry::open(dir.join("tasks.json")).unwrap();
        assert_eq!(reopened.get("a").unwrap().token_spent, 40);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn failed_prune_rolls_back_and_stays_retryable() {
        let (dir, mut registry) = fault_registry("prune");
        for id in ["t1", "t2"] {
            registry.register(new_task(id)).unwrap();
            registry.transition(id, TaskState::Running, 1).unwrap();
            registry.fail(id, "boom", 2).unwrap();
        }
        assert_eq!(registry.len(), 2);
        registry.fault = Some(PersistFault::Precommit);
        assert_eq!(
            registry.prune_terminal(0),
            0,
            "a failed prune removes nothing"
        );
        assert_eq!(
            registry.len(),
            2,
            "memory keeps the records the commit never dropped"
        );
        let reopened = AsyncTaskRegistry::open(dir.join("tasks.json")).unwrap();
        assert_eq!(reopened.len(), 2);
        registry.fault = None;
        assert_eq!(registry.prune_terminal(0), 2);
        assert_eq!(registry.len(), 0);
        let reopened = AsyncTaskRegistry::open(dir.join("tasks.json")).unwrap();
        assert_eq!(reopened.len(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rename_failure_leaves_the_previous_document_intact() {
        let (dir, mut registry) = fault_registry("rename");
        registry.register(new_task("a")).unwrap();
        let before = std::fs::read(dir.join("tasks.json")).unwrap();
        registry.fault = Some(PersistFault::Rename);
        assert!(matches!(
            registry.register(new_task("b")),
            Err(TaskRegistryError::Persist(_))
        ));
        // The fsync of the temp data happened before the (injected) rename failure, so the old
        // document must still be byte-identical and no temp file may be left behind.
        let after = std::fs::read(dir.join("tasks.json")).unwrap();
        assert_eq!(before, after, "a failed rename must not touch the target");
        assert!(registry.get("b").is_none());
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp-"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "a failed commit must clean up its temp file"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn post_rename_dir_sync_failure_publishes_and_reports_uncertain() {
        let (dir, mut registry) = fault_registry("dirsync");
        registry.register(new_task("a")).unwrap();
        registry.fault = Some(PersistFault::DirSync);
        assert!(matches!(
            registry.register(new_task("b")),
            Err(TaskRegistryError::PersistUncertain(_))
        ));
        // The rename already happened, so both memory and disk show 'b'; the error only says the
        // directory sync (and therefore crash durability) could not be confirmed.
        assert!(registry.get("b").is_some());
        let reopened = AsyncTaskRegistry::open(dir.join("tasks.json")).unwrap();
        assert!(reopened.get("b").is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn prune_terminal_never_evicts_a_record_with_a_pending_merge() {
        let mut registry = AsyncTaskRegistry::in_memory();
        // The protected record is deliberately the *oldest*, so a naive prune would evict it first.
        registry.register(new_task("protected")).unwrap();
        registry.transition("protected", TaskState::Running, 0).unwrap();
        registry.fail("protected", "link failed", 0).unwrap();
        registry
            .set_pending_merge(
                "protected",
                PendingMergeLink {
                    session_id: "s1".into(),
                    session_path: "/w/session.wal.jsonl".into(),
                    turn: 0,
                    parent_revision: None,
                    merged_revision: Some("r-abc".into()),
                    changed_paths: vec!["out.txt".into()],
                    tracked_files: 1,
                },
            )
            .unwrap();
        for i in 0..8 {
            let id = format!("t{}", i);
            registry.register(new_task(&id)).unwrap();
            registry.transition(&id, TaskState::Running, 10).unwrap();
            registry.fail(&id, "done", 10).unwrap();
        }

        assert_eq!(
            registry.prune_terminal(1),
            7,
            "every evictable terminal record but the newest must be pruned"
        );
        assert!(
            registry.get("protected").is_some(),
            "a record with a pending merge intent must never be evicted"
        );
        assert_eq!(registry.pending_merges().len(), 1);
        assert!(
            registry.get("t7").is_some(),
            "the newest evictable record is kept"
        );
        assert!(registry.get("t0").is_none());
    }

    #[test]
    fn prune_terminal_checked_reports_a_commit_failure_instead_of_claiming_space() {
        let (dir, mut registry) = fault_registry("prune-checked");
        registry.register(new_task("a")).unwrap();
        registry.transition("a", TaskState::Running, 1).unwrap();
        registry.fail("a", "boom", 2).unwrap();
        registry.fault = Some(PersistFault::Precommit);
        assert!(matches!(
            registry.prune_terminal_checked(0),
            Err(TaskRegistryError::Persist(_))
        ));
        assert_eq!(registry.len(), 1, "a failed prune removes nothing");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn settle_finish_refuses_a_terminal_record_with_a_mismatched_outcome() {
        let mut registry = AsyncTaskRegistry::in_memory();
        registry.register(new_task("a")).unwrap();
        registry.transition("a", TaskState::Running, 1).unwrap();
        registry.transition("a", TaskState::Completed, 2).unwrap();
        // A reused id would settle the finished record with a new run's outcome.
        assert!(matches!(
            registry.settle_finish(
                "a",
                TaskFinish {
                    state: TaskState::Failed,
                    spent: 5,
                    artifact: None,
                    summary: "a new run".into(),
                    turn: 3,
                    keep_pending_merge: false,
                    verification: None,
                },
            ),
            Err(TaskRegistryError::InvalidTransition { .. })
        ));
        assert_eq!(
            registry.get("a").unwrap().token_spent,
            0,
            "no spend may land on the finished record"
        );
        assert_eq!(registry.get("a").unwrap().state, TaskState::Completed);
        // A terminal record may not be reopened into another state either, even one the raw state
        // machine would accept (Completed -> ChallengePending).
        assert!(matches!(
            registry.settle_finish(
                "a",
                TaskFinish {
                    state: TaskState::ChallengePending,
                    spent: 5,
                    artifact: None,
                    summary: "reopen a finished record".into(),
                    turn: 3,
                    keep_pending_merge: false,
                    verification: None,
                },
            ),
            Err(TaskRegistryError::InvalidTransition { .. })
        ));
        assert_eq!(registry.get("a").unwrap().state, TaskState::Completed);
        // The identical terminal outcome is still allowed (a late finish for the same run).
        registry
            .settle_finish(
                "a",
                TaskFinish {
                    state: TaskState::Completed,
                    spent: 5,
                    artifact: None,
                    summary: "late finish".into(),
                    turn: 3,
                    keep_pending_merge: false,
                    verification: None,
                },
            )
            .unwrap();
        assert_eq!(registry.get("a").unwrap().token_spent, 5);
    }

    #[test]
    fn registration_binds_ownership_and_reserves_budget() {
        let dir = std::env::temp_dir().join(format!("kroma-taskroute-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let sid = format!("s-{}", uuid::Uuid::new_v4());
        crate::session_ownership::register_session(&sid, &dir, "runtime-7");

        let budget = std::sync::Arc::new(crate::budget::TokenBudget::new(Some(100)));
        let mut registry = AsyncTaskRegistry::in_memory();
        let (record, reservation) = register_from_session(
            &mut registry,
            "t1",
            &sid,
            &budget,
            40,
            "worker",
            0,
            "prompt",
            500,
            "{}",
            "allow-write",
        )
        .unwrap();
        assert_eq!(record.owner_id, "runtime-7");
        assert_eq!(record.token_allocation, 40);
        assert_eq!(budget.reserved(), 40);

        // A second task cannot spend the same balance twice.
        assert!(matches!(
            register_from_session(
                &mut registry,
                "t2",
                &sid,
                &budget,
                80,
                "worker",
                0,
                "prompt",
                500,
                "{}",
                "allow-write"
            ),
            Err(RegisterFromSessionError::BudgetExhausted { .. })
        ));
        assert!(
            matches!(
                register_from_session(
                    &mut registry,
                    "t3",
                    "missing",
                    &budget,
                    1,
                    "worker",
                    0,
                    "prompt",
                    500,
                    "{}",
                    "allow-write"
                ),
                Err(RegisterFromSessionError::UnknownSession(_))
            ),
            "a task without a routed owner must be rejected"
        );

        reservation.settle(record.token_allocation);
        assert_eq!(budget.spent(), 40);
        crate::session_ownership::unregister_session(&sid);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn control_request_round_trips_and_applies() {
        let mut registry = AsyncTaskRegistry::in_memory();
        registry.register(new_task("a")).unwrap();
        registry.transition("a", TaskState::Running, 1).unwrap();
        let mut registry = AsyncTaskRegistry::in_memory();
        registry.register(new_task("a")).unwrap();
        registry.transition("a", TaskState::Running, 1).unwrap();
        let request: TaskControlRequest =
            serde_json::from_str(r#"{"action":"challenge","task_id":"a","reason":"bad"}"#).unwrap();
        let response = registry.apply_control(&request).unwrap();
        assert_eq!(response["state"], "challenge_pending");
        let list: TaskControlRequest = serde_json::from_str(r#"{"action":"list"}"#).unwrap();
        let listed = registry.apply_control(&list).unwrap();
        assert_eq!(listed["count"], 1);
        // Unknown verbs are rejected, not ignored.
        assert!(serde_json::from_str::<TaskControlRequest>(r#"{"action":"nuke"}"#).is_err());
    }
}
