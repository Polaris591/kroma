//! Capability gap #4 (`v1.5.2`): the *executable* half of the durable async-task control plane.
//!
//! [`crate::async_tasks::AsyncTaskRegistry`] is the durable state machine; this module connects it
//! to the real `spawn_subagent` lifecycle so a task record reflects work that actually ran:
//!
//! * [`DurableTaskHost::register_child`] is called by the agent loop when a child subagent is about
//!   to run — it records the child (its full [`SubagentSpec`], allocation, provenance) and binds the
//!   child's cancellation channel.
//! * [`DurableTaskHost::finish_child`] settles the child's real token spend, attaches the child's
//!   report as an artifact, transitions the state to match the real outcome, and raises a
//!   notification.
//! * [`DurableTaskHost::cancel_task`] is *executable*: it fires the live child's cancel channel (so
//!   the subagent actually stops) in addition to moving the durable state to `Cancelled`.
//! * [`DurableTaskHost::resume_task`] is *executable*: it re-runs a suspended/interrupted task from
//!   its persisted [`SubagentSpec`], isolated in a worktree for `worker` tasks, holding the
//!   cross-process workspace writer lock for the whole run.
//! * [`DurableTaskHost::recover_interrupted`] runs at startup so a task left `Running` by a crash is
//!   marked `Suspended` and can be resumed.
//!
//! **Admission is a single transaction.** `resume_task` takes one host-level mutex that guards both
//! the cancellation bindings and the "resume in flight" set, and only then checks the registry
//! state, acquires the workspace writer lock, transitions the task to `Running`, and binds its
//! canceller. A `Running`/`Queued` task with a live binding is refused, so two concurrent resumes
//! can never run the backend twice, and a resume that cannot take the writer lock fails structurally
//! **without** calling the backend and leaves the task resumable.
//!
//! **Resume obeys the original contract.** It re-runs the exact persisted [`SubagentSpec`] (role,
//! `allowed_write_globs`, budget, test-tamper guard, `revert_on_verify_failure`) under the parent
//! policy recorded at spawn time and the *remaining* allocation, merges a worker through the same
//! post-merge completion gate, records the merged revision in the session WAL so `--undo` works, and
//! never marks a conflict or a verification failure as `Completed`.

use crate::async_tasks::{
    ArtifactRef, AsyncTaskRecord, AsyncTaskRegistry, NewTask, PendingMergeLink, TaskFinish,
    TaskRegistryError, TaskState, VerificationRecord,
};
use crate::budget::BudgetConfig;
use crate::env::SharedCompletionGate;
use crate::extensions::SkillEntry;
use crate::provider::ModelProvider;
use crate::revision::{RevisionId, SnapshotStore};
use crate::session::SessionJournal;
use crate::subagents::{
    GateVerificationStatus, IsolatedWorktree, ParentPolicyHooks, ProviderSubagentBackend,
    SubagentBackend, SubagentExecutionRequest, SubagentRole, SubagentSpec, WorktreeMergeResult,
};
use crate::tool::policy::PolicyMode;
use crate::types::RunStatus;
use crate::workspace::WorkspaceCap;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::sync::watch;

/// Cap on a persisted task report. A report is the child's full summary; the registry only keeps a
/// 512-byte notification, so the bytes live here and must stay bounded. A summary over this cap is
/// stored truncated and explicitly marked rather than failing the settlement.
pub const MAX_REPORT_BYTES: usize = 1024 * 1024;

/// Bytes reserved for the truncation notice appended to an over-cap report. The notice is two
/// decimal numbers plus fixed text (well under this reserve), so `kept + notice` always fits under
/// [`MAX_REPORT_BYTES`].
const REPORT_TRUNCATION_NOTICE_RESERVE: usize = 256;

/// How many derived durable ids a colliding admission may try before it is refused. Bounds the loop
/// that keeps durable recording working after a session restart resets the caller's id counter.
const MAX_DERIVED_TASK_IDS: u32 = 32;

/// The session context a task is admitted against. New admissions use the host's *current* binding;
/// an admitted record keeps its own durable copy (`AsyncTaskRecord::session_path`) so a later resume
/// writes the admitting session's WAL even after the dispatcher has switched sessions.
#[derive(Debug, Clone)]
struct SessionBinding {
    session_id: String,
    owner_id: String,
    session_path: PathBuf,
}

/// What happened when a resumed worker's merge was linked into its session WAL.
///
/// The three outcomes are deliberately not interchangeable: only [`Self::PendingReplay`] may claim
/// that a replay is pending, and only when the durable intent actually names the merged revision.
#[derive(Debug, Clone, PartialEq, Eq)]
enum MergeLinkOutcome {
    /// The merged revision is recorded in the session WAL (or already was).
    Linked,
    /// The merge landed but the append failed. The durable intent names the merged revision, so the
    /// link is replayed at the next startup.
    PendingReplay(String),
    /// The merge landed but the durable intent does **not** name the merged revision, so the link can
    /// never be replayed. The workspace and the session history disagree and an operator must
    /// reconcile them.
    RecoveryRequired(String),
}

impl MergeLinkOutcome {
    /// Whether the pending-merge intent must be retained for a later replay.
    fn retain_intent(&self) -> bool {
        matches!(self, MergeLinkOutcome::PendingReplay(_))
    }

    /// The sentence appended to the task summary. Only `PendingReplay` claims a replay is pending.
    fn note(&self) -> String {
        match self {
            MergeLinkOutcome::Linked => String::new(),
            MergeLinkOutcome::PendingReplay(e) => format!(
                "merge applied but its session WAL link could not be recorded ({}); NOT completed — the merged revision is durably pending and will be replayed",
                e
            ),
            MergeLinkOutcome::RecoveryRequired(e) => format!(
                "merge applied but its session WAL link could not be recorded and cannot be replayed ({}); RECOVERY REQUIRED — the workspace and the session history disagree",
                e
            ),
        }
    }
}

/// Outcome of a startup replay of pending merge intents.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct MergeReplayReport {
    /// Intents whose merged revision is now recorded in their session WAL *and* whose intent was
    /// cleared: fully resolved.
    pub linked: usize,
    /// Task ids whose intent carried no merged revision. Such an intent can never be replayed — the
    /// merge either never landed (the snapshot store's own merge recovery owns the workspace) or it
    /// landed without its link being recorded. The intent is cleared so it cannot become a permanent
    /// orphan, and the condition is reported for an operator to reconcile.
    pub unresolved: Vec<String>,
    /// Intents whose replay did not complete, as `(task_id, reason)`. The intent is **left in
    /// place** so the next startup retries it (replay is idempotent); a locked or missing session
    /// WAL is a normal, transient condition and never abandons the other intents in the same
    /// startup.
    pub failed: Vec<(String, String)>,
}

/// What the agent loop tells the host about a child it is about to spawn.
#[derive(Debug, Clone)]
pub struct ChildTaskSpec {
    pub task_id: String,
    /// The full spec, persisted verbatim so resume re-runs the original contract.
    pub spec: SubagentSpec,
    /// The tokens reserved for this child out of the run tree's shared ledger.
    pub allocation: u64,
    pub turn: u32,
    /// The parent policy mode at spawn time (provenance for resume).
    pub parent_policy_mode: PolicyMode,
    /// B11b: the parent's fail-closed policy-hook constraints at spawn time. Persisted so a resume
    /// re-runs the child under the *same* constraint decision instead of silently dropping it.
    pub parent_policy_hooks: ParentPolicyHooks,
}

/// How a child finished, used to settle its durable record.
#[derive(Debug, Clone)]
pub struct ChildFinish {
    pub state: TaskState,
    pub spent: u64,
    pub summary: String,
    /// `true` when the merge landed but its session WAL link could not be recorded: the pending
    /// merge intent is kept so the linkage is replayed instead of being discarded.
    pub keep_pending_merge: bool,
    /// B12: real verification evidence from the merge's post-merge gate, when one actually ran and
    /// approved. `None` = unknown (unarmed/skipped/absent); never inferred from `Applied` alone.
    pub verification: Option<VerificationRecord>,
}

/// The host-level state guarded by one mutex, so a resume's "is it already running?" check, its
/// state transition and its canceller binding are one atomic transaction.
#[derive(Default)]
struct HostState {
    cancellers: HashMap<String, watch::Sender<bool>>,
    /// Tasks whose resume has been admitted but has not finished yet.
    resuming: HashSet<String>,
}

/// Clears a task's `resuming` slot when its background run ends, even on an early return or panic.
struct ResumeSlot {
    host: Arc<DurableTaskHost>,
    task_id: String,
}

impl Drop for ResumeSlot {
    fn drop(&mut self) {
        let mut state = self.host.state.lock().unwrap_or_else(|e| e.into_inner());
        state.resuming.remove(&self.task_id);
    }
}

/// The executable durable-task host for one runtime.
pub struct DurableTaskHost {
    registry: Arc<Mutex<AsyncTaskRegistry>>,
    state: Mutex<HostState>,
    provider: Arc<dyn ModelProvider>,
    snapshot_store: Arc<SnapshotStore>,
    parent_workspace: WorkspaceCap,
    state_base: PathBuf,
    worktree_base: PathBuf,
    workspace_skills: Vec<SkillEntry>,
    parent_policy_mode: PolicyMode,
    parent_budget: BudgetConfig,
    predictive_check: bool,
    completion_gate: SharedCompletionGate,
    /// The session new admissions are currently bound to. Replaced by [`Self::rebind_session`] when
    /// the dispatcher switches sessions; already-admitted tasks keep their own recorded context.
    binding: Mutex<SessionBinding>,
    /// Directory that holds the private, durable task report bytes (`file:reports/<id>.report`).
    report_base: PathBuf,
}

impl DurableTaskHost {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        registry: Arc<Mutex<AsyncTaskRegistry>>,
        provider: Arc<dyn ModelProvider>,
        snapshot_store: Arc<SnapshotStore>,
        parent_workspace: WorkspaceCap,
        state_base: PathBuf,
        worktree_base: PathBuf,
        workspace_skills: Vec<SkillEntry>,
        parent_policy_mode: PolicyMode,
        parent_budget: BudgetConfig,
        predictive_check: bool,
        completion_gate: SharedCompletionGate,
        session_id: String,
        owner_id: String,
        session_path: PathBuf,
    ) -> Self {
        Self {
            registry,
            state: Mutex::new(HostState::default()),
            provider,
            snapshot_store,
            parent_workspace,
            state_base: state_base.clone(),
            worktree_base,
            workspace_skills,
            parent_policy_mode,
            parent_budget,
            predictive_check,
            completion_gate,
            binding: Mutex::new(SessionBinding {
                session_id,
                owner_id,
                session_path,
            }),
            report_base: state_base.join("reports"),
        }
    }

    /// A clone of the current session binding (never held across a registry mutation).
    fn current_binding(&self) -> SessionBinding {
        self.binding
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Point new admissions at the session the dispatcher now owns. A task already admitted keeps
    /// the session context captured at its own admission, so switching sessions can never redirect
    /// an existing task's WAL writes.
    pub fn rebind_session(&self, session_id: String, owner_id: String, session_path: PathBuf) {
        let mut binding = self.binding.lock().unwrap_or_else(|e| e.into_inner());
        *binding = SessionBinding {
            session_id,
            owner_id,
            session_path,
        };
    }

    /// Lock order is always `state` → `registry`; never the other way round.
    fn registry(&self) -> std::sync::MutexGuard<'_, AsyncTaskRegistry> {
        self.registry.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn state(&self) -> std::sync::MutexGuard<'_, HostState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Record a child at spawn time and bind its cancellation channel.
    ///
    /// Returns the durable task id the child was recorded under. The requested id is used when it
    /// is free. When it is already taken by a *finished* record the id reuse is rejected (that
    /// record's report bytes and spend must stay intact) and the child is admitted under a derived
    /// unique id instead, so the durable table keeps recording children across a session restart
    /// without ever reusing another record's identity. A live or recovery-pending record refuses the
    /// admission outright. The capacity arm checks the prune result: a table with nothing evictable
    /// fails the admission honestly instead of destroying durable recovery state.
    pub fn register_child(
        &self,
        spec: &ChildTaskSpec,
        cancel: watch::Sender<bool>,
    ) -> Result<String, String> {
        let spec_json = serde_json::to_string(&spec.spec).map_err(|e| e.to_string())?;
        // New admissions use the *current* binding; the captured context is stored on the record so
        // a later resume can never be redirected to a different session's WAL.
        let binding = self.current_binding();
        let mut state = self.state();
        let mut registry = self.registry();
        let new_task = |task_id: &str| NewTask {
            task_id: task_id.to_string(),
            session_id: binding.session_id.clone(),
            owner_id: binding.owner_id.clone(),
            workspace_root: self.parent_workspace.root().display().to_string(),
            role: spec.spec.role.as_str().to_string(),
            created_turn: spec.turn,
            token_allocation: spec.allocation,
            prompt: spec.spec.task.clone(),
            timeout_ms: spec.spec.budget.timeout_ms,
            spec_json: spec_json.clone(),
            parent_policy: serde_json::to_string(&spec.parent_policy_mode).unwrap_or_default(),
            parent_policy_hooks: spec.parent_policy_hooks.clone(),
            session_path: binding.session_path.display().to_string(),
        };

        let mut durable_id = spec.task_id.clone();
        let mut derived = 0u32;
        let mut capacity_retried = false;
        loop {
            match registry.register(new_task(&durable_id)) {
                Ok(_) => break,
                Err(crate::async_tasks::TaskRegistryError::DuplicateTask(_)) => {
                    match registry.get(&durable_id) {
                        // A run that has not finished owns this identity; never reuse it.
                        Some(existing) if !existing.state.is_terminal() => {
                            return Err(format!(
                                "task '{}' is already recorded and not terminal ({}); refusing to reuse its id",
                                durable_id,
                                existing.state.as_str()
                            ));
                        }
                        // A record whose merge→WAL link is still pending owns its identity too.
                        Some(existing) if existing.pending_merge.is_some() => {
                            return Err(format!(
                                "task '{}' is already recorded with a pending merge link; refusing to reuse its id",
                                durable_id
                            ));
                        }
                        // A stale terminal record: reject the id reuse and admit under a derived id.
                        _ => {
                            derived += 1;
                            if derived > MAX_DERIVED_TASK_IDS {
                                return Err(format!(
                                    "task '{}' is already recorded and {} derived ids are taken; refusing to reuse any of them",
                                    spec.task_id, MAX_DERIVED_TASK_IDS
                                ));
                            }
                            durable_id = format!("{}#{}", spec.task_id, derived);
                        }
                    }
                }
                Err(crate::async_tasks::TaskRegistryError::CapacityExceeded(_)) => {
                    if capacity_retried {
                        return Err(format!(
                            "task registry is full and '{}' could not be admitted",
                            durable_id
                        ));
                    }
                    capacity_retried = true;
                    let removed = registry
                        .prune_terminal_checked(1)
                        .map_err(|e| format!("task registry is full and pruning failed: {}", e))?;
                    if removed == 0 {
                        return Err(
                            "task registry is full and no terminal record is evictable (records with a pending merge link are protected)"
                                .to_string(),
                        );
                    }
                }
                Err(e) => return Err(e.to_string()),
            }
        }

        if registry.state_of(&durable_id) == Some(TaskState::Queued) {
            // A transition failure here must not leave the child unrecorded-but-running: surface it
            // while still binding the canceller so the child can be stopped. `begin_execution`
            // persists the durable "started" marker in the same transaction as the transition.
            let transition = registry.begin_execution(&durable_id, spec.turn);
            if let Err(e) = transition {
                state.cancellers.insert(durable_id.clone(), cancel);
                return Err(format!(
                    "task '{}' was admitted but could not be moved to Running: {}",
                    durable_id, e
                ));
            }
        }
        state.cancellers.insert(durable_id.clone(), cancel);
        Ok(durable_id)
    }

    /// Settle a child's real spend, report artifact, state and notification in one durable commit,
    /// and always release its canceller binding.
    ///
    /// The full report bytes are written to a private, hash-addressed file **before** the finish
    /// metadata is committed, so a declared `report` artifact can never outlive its bytes. A
    /// persistence failure is propagated: the caller must not treat the child as settled. The
    /// binding is released either way because the run is over — a stale canceller can then never be
    /// signalled for a task that is no longer running.
    pub fn finish_child(&self, task_id: &str, finish: &ChildFinish) -> Result<(), String> {
        let mut state = self.state();
        let mut registry = self.registry();
        let result = (|| -> Result<(), String> {
            if registry.get(task_id).is_none() {
                return Err(format!("unknown task '{}'", task_id));
            }
            // 1. Durable report bytes first, with the digest of exactly what was stored.
            let artifact = self.store_report(task_id, &finish.summary)?;
            let turn = registry
                .get(task_id)
                .map(|task| task.updated_turn)
                .unwrap_or(0);
            // 2. One atomic commit: artifact + spend + state + notification (+ intent clear).
            registry
                .settle_finish(
                    task_id,
                    TaskFinish {
                        state: finish.state,
                        spent: finish.spent,
                        artifact: Some(artifact),
                        summary: finish.summary.clone(),
                        turn,
                        keep_pending_merge: finish.keep_pending_merge,
                        verification: finish.verification.clone(),
                    },
                )
                .map_err(|e| e.to_string())?;
            Ok(())
        })();
        state.cancellers.remove(task_id);
        state.resuming.remove(task_id);
        result
    }

    /// Forget a child's cancellation binding (used when a spawn was rejected before running).
    pub fn forget_child(&self, task_id: &str) {
        self.state().cancellers.remove(task_id);
    }

    /// Executable cancel: signal the live child (if any) and move the durable state to `Cancelled`.
    /// Returns `true` when a live run was signalled.
    pub fn cancel_task(&self, task_id: &str) -> Result<bool, String> {
        let state = self.state();
        // A failed send means the receiver is gone; never claim the live run was signalled.
        let signalled = match state.cancellers.get(task_id) {
            Some(tx) => tx.send(true).is_ok(),
            None => false,
        };
        let mut registry = self.registry();
        if registry.get(task_id).is_none() {
            return Err(format!("unknown task '{}'", task_id));
        }
        // Already-terminal states are left as-is; a live/queued task is cancelled. A persistence
        // failure is propagated instead of being reported as a successful cancel.
        if let Some(task_state) = registry.state_of(task_id) {
            if !task_state.is_terminal() {
                registry.cancel(task_id, 0).map_err(|e| e.to_string())?;
            }
        }
        Ok(signalled)
    }

    /// Executable resume: re-run a suspended/interrupted task from its persisted spec.
    ///
    /// Admission is atomic: the "already running?" check, the writer-lock acquisition, the
    /// `Running` transition and the canceller binding happen under one host lock, so a task with a
    /// live run (or an in-flight resume) is refused. A resume that cannot take the workspace writer
    /// lock fails with a structured error, calls the backend **zero** times, and leaves the task
    /// resumable.
    pub fn resume_task(self: &Arc<Self>, task_id: &str) -> Result<(), String> {
        let mut state = self.state();
        if state.resuming.contains(task_id) || state.cancellers.contains_key(task_id) {
            return Err(format!(
                "task '{}' already has a live run; refusing a second resume",
                task_id
            ));
        }

        let record = {
            let registry = self.registry();
            registry
                .get_cloned(task_id)
                .ok_or_else(|| format!("unknown task '{}'", task_id))?
        };
        if record.state.is_terminal() {
            return Err(format!(
                "task '{}' is {} and cannot be resumed",
                task_id,
                record.state.as_str()
            ));
        }
        if record.state == TaskState::Running {
            return Err(format!(
                "task '{}' is already running; refusing to resume it",
                task_id
            ));
        }
        // A crash can leave a task whose outcome is unknown; `begin_execution` refuses to re-run it
        // (the authoritative guard lives in the admission transaction, so no path can bypass it).
        let spec: SubagentSpec = serde_json::from_str(&record.spec_json).map_err(|e| {
            format!(
                "task '{}' has no usable persisted spec to resume: {}",
                task_id, e
            )
        })?;
        let remaining = record.token_allocation.saturating_sub(record.token_spent);
        if remaining == 0 {
            return Err(format!(
                "task '{}' has no token allocation remaining to resume",
                task_id
            ));
        }
        let parent_policy: PolicyMode =
            serde_json::from_str(&record.parent_policy).unwrap_or(self.parent_policy_mode);
        // B11b: restore the parent's recorded fail-closed policy-hook constraints. The strong type
        // deserializes a legacy record without the field to `Unknown` (never an empty set), and the
        // backend chokepoint then refuses the re-run instead of dropping the constraints.
        let parent_policy_hooks = record.parent_policy_hooks.clone();

        // The session this task belongs to is the one recorded at its admission, never whatever
        // session the host is bound to now. A legacy record without a captured path falls back to
        // the current binding (documented residual).
        let session_path = if record.session_path.trim().is_empty() {
            self.current_binding().session_path
        } else {
            PathBuf::from(&record.session_path)
        };
        let session_id = if record.session_id.trim().is_empty() {
            self.current_binding().session_id
        } else {
            record.session_id.clone()
        };

        // Acquire the writer lock BEFORE admitting the run. A failure is structured and leaves the
        // task's state untouched (still `Suspended`/`Queued`) so it can be resumed later.
        let owner = format!("{}:resume:{}", self.current_binding().owner_id, task_id);
        let lease = match crate::session_ownership::try_acquire_writer(
            self.parent_workspace.root(),
            &owner,
        ) {
            Ok(lease) => lease,
            Err(conflict) => {
                return Err(format!(
                    "cannot resume task '{}': workspace {} already has a writer ({}); the task remains {} and can be resumed later",
                    task_id,
                    conflict.workspace_root.display(),
                    conflict.current_owner,
                    record.state.as_str()
                ));
            }
        };

        // A worker resume records its merged revision in the session WAL. Take that WAL's exclusive
        // lock *before* the task is admitted and before the parent workspace is touched, so a WAL
        // held by a concurrent conversation rejects the resume instead of silently losing the undo
        // link after a merge already mutated the parent. Held for the whole detached run.
        let journal = if spec.role == SubagentRole::Worker {
            match SessionJournal::open(&session_path) {
                Ok(journal) => Some(journal),
                Err(e) => {
                    return Err(format!(
                        "cannot resume task '{}': session WAL {} is unavailable ({}); the task remains {} and can be resumed later",
                        task_id,
                        session_path.display(),
                        e,
                        record.state.as_str()
                    ));
                }
            }
        } else {
            None
        };

        // Transition + bind canceller inside the same admission transaction.
        {
            let mut registry = self.registry();
            match registry.resume(task_id, record.updated_turn) {
                Ok(()) => {}
                // `PersistUncertain` means the rename already happened: the `Running` state is
                // visible in memory and on disk, and only the directory fsync that would make it
                // provably crash-durable failed. Treating that as a refusal would strand a durable
                // `Running` record with no canceller and no run, and every later resume would then
                // be refused as "already running". So the transition is treated as applied and the
                // admission continues exactly as on success (bind the canceller and spawn the run).
                // Every other error is a structured refusal that leaves the task resumable.
                Err(TaskRegistryError::PersistUncertain(_)) => {}
                Err(e) => return Err(e.to_string()),
            }
        }
        let (cancel_tx, cancel_rx) = watch::channel(false);
        state.cancellers.insert(task_id.to_string(), cancel_tx);
        state.resuming.insert(task_id.to_string());
        drop(state);

        let host = Arc::clone(self);
        let task_id = task_id.to_string();
        tokio::spawn(async move {
            // Keep the writer lease and the WAL lock alive for the whole run (moved into the task).
            let _writer_lease = lease;
            let _slot = ResumeSlot {
                host: Arc::clone(&host),
                task_id: task_id.clone(),
            };
            host.run_resume(
                task_id,
                spec,
                remaining,
                parent_policy,
                parent_policy_hooks,
                cancel_rx,
                session_id,
                session_path,
                journal,
            )
            .await;
        });
        Ok(())
    }

    /// The background body of a resume: run the persisted spec, merge a worker through the gate,
    /// record the merge in the admitting session's WAL, and settle the durable record.
    #[allow(clippy::too_many_arguments)]
    async fn run_resume(
        self: Arc<Self>,
        task_id: String,
        mut spec: SubagentSpec,
        remaining: u64,
        parent_policy: PolicyMode,
        parent_policy_hooks: ParentPolicyHooks,
        cancel_rx: watch::Receiver<bool>,
        session_id: String,
        session_path: PathBuf,
        mut journal: Option<SessionJournal>,
    ) {
        let role = spec.role;
        // B11b: refuse an unsupported delegation *before* the resume materializes a worktree, so a
        // refusal leaves the parent workspace untouched and the durable record/WAL intact (the
        // backend repeats this check as the final gate). Never swallowed into a success.
        let refusal = parent_policy_hooks
            .ensure_delegatable()
            .err()
            .or_else(|| crate::subagents::child_policy_mode_for(parent_policy, role).err());
        if let Some(reason) = refusal {
            let _ = self.finish_child(
                &task_id,
                &ChildFinish {
                    state: TaskState::Failed,
                    spent: 0,
                    summary: format!("resume refused: {}", reason),
                    keep_pending_merge: false,
                    verification: None,
                },
            );
            return;
        }
        // The remaining allocation is the effective per-child cap.
        spec.budget.max_tokens = spec.budget.max_tokens.min(remaining);
        let turn = self
            .registry()
            .get(&task_id)
            .map(|t| t.updated_turn)
            .unwrap_or(0);

        let mut worktree = match role {
            SubagentRole::Worker => {
                let path = self.worktree_base.join(format!("resume-{}", task_id));
                match IsolatedWorktree::materialize(
                    task_id.clone(),
                    &self.parent_workspace,
                    &self.snapshot_store,
                    None,
                    path,
                ) {
                    Ok(wt) => Some(wt),
                    Err(e) => {
                        let _ = self.finish_child(
                            &task_id,
                            &ChildFinish {
                                state: TaskState::Failed,
                                spent: 0,
                                summary: format!(
                                    "resume failed: cannot materialize a worktree: {}",
                                    e
                                ),
                                keep_pending_merge: false,
                                verification: None,
                            },
                        );
                        return;
                    }
                }
            }
            SubagentRole::Scout => None,
        };
        let workspace = worktree
            .as_ref()
            .map(|wt| wt.workspace().clone())
            .unwrap_or_else(|| self.parent_workspace.clone());

        let backend = ProviderSubagentBackend::new(Arc::clone(&self.provider));
        // Keep a handle on the cancellation signal: it is moved into the request below, and the
        // merge decision must still be able to observe a cancellation that arrived while the child
        // ran (mirroring the first-run guard in the agent loop).
        let cancel_rx_for_merge = cancel_rx.clone();
        let req = SubagentExecutionRequest {
            subagent_id: task_id.clone(),
            spec: spec.clone(),
            workspace,
            state_dir: self.state_base.join(format!("resume-{}", task_id)),
            workspace_skills: self.workspace_skills.clone(),
            parent_policy_mode: parent_policy,
            parent_policy_hooks,
            parent_budget: self.parent_budget.clone(),
            token_budget: Some(Arc::new(crate::budget::TokenBudget::new(Some(remaining)))),
            predictive_check: self.predictive_check,
            cancel_rx,
        };
        let outcome = backend.run_subagent(req).await;

        let mut keep_pending_merge = false;
        // B12: real verification evidence for this settlement, set only when the merge's post-merge
        // gate actually ran and approved.
        let mut verification: Option<VerificationRecord> = None;
        // B12: the session WAL's latest persisted snapshot identity — the durable parent snapshot
        // revision the merge is re-verified against. It is a snapshot id, **not** a live byte-level
        // identity (an uncaptured external write does not change it). `None` (no WAL revision) stays
        // unknown and is never faked into `Some`.
        let parent_snapshot_revision: Option<RevisionId> = journal
            .as_ref()
            .and_then(|j| j.latest_revision_id())
            .and_then(|id| RevisionId::parse(&id).ok());
        let (state, summary, spent) = match outcome {
            Ok(o) => {
                let mut state = match o.status {
                    RunStatus::Completed => TaskState::Completed,
                    RunStatus::Cancelled => TaskState::Cancelled,
                    _ => TaskState::Failed,
                };
                let mut summary = o.summary.clone();
                if role == SubagentRole::Worker {
                    match worktree.as_mut() {
                        None => {
                            state = TaskState::Failed;
                            summary =
                                format!("{}; worker resume had no worktree to merge", summary);
                        }
                        // Mirror the first-run guard exactly: only a genuinely completed run that was
                        // not cancelled may touch the parent workspace or its WAL. A cancelled /
                        // failed / budget-exhausted worker keeps whatever partial delta it made
                        // isolated in its worktree.
                        Some(_) if o.status != RunStatus::Completed => {
                            summary = format!(
                                "{}; worker delta NOT merged: the run ended {:?}",
                                summary, o.status
                            );
                        }
                        Some(_) if *cancel_rx_for_merge.borrow() => {
                            state = TaskState::Cancelled;
                            summary = format!(
                                "{}; worker delta NOT merged: the resume was cancelled before the merge",
                                summary
                            );
                        }
                        Some(wt) => {
                            // A durable, session-bound merge intent is committed *before* the merge
                            // touches the parent, so a crash between the merge and the WAL append
                            // is replayed instead of silently losing the undo link.
                            let intent = PendingMergeLink {
                                session_id: session_id.clone(),
                                session_path: session_path.display().to_string(),
                                turn,
                                parent_revision: None,
                                merged_revision: None,
                                changed_paths: Vec::new(),
                                tracked_files: 0,
                            };
                            let intent_result = {
                                let mut registry = self.registry();
                                registry
                                    .set_pending_merge(&task_id, intent)
                                    .map_err(|e| e.to_string())
                            };
                            if let Err(e) = intent_result {
                                // The parent has not been touched; refuse rather than merge with no
                                // recoverable link.
                                state = TaskState::Failed;
                                summary = format!(
                                    "{}; merge NOT applied: could not persist the session-bound merge intent: {}",
                                    summary, e
                                );
                            } else {
                                match wt
                                    .merge_into_parent(
                                        &self.parent_workspace,
                                        &self.snapshot_store,
                                        parent_snapshot_revision.as_ref(),
                                        spec.allowed_write_globs.as_deref(),
                                        Some(&self.completion_gate),
                                        spec.revert_on_verify_failure,
                                        turn as usize,
                                    )
                                    .await
                                {
                                    Ok(WorktreeMergeResult::Applied {
                                        base_revision,
                                        merged_revision,
                                        written_files,
                                        removed_files,
                                        gate,
                                    }) => {
                                        // B12: record real verification evidence only when the
                                        // gate was armed and approved. A skipped/disabled/absent
                                        // gate stays `None` (unknown), never a fake `verified`.
                                        if gate == GateVerificationStatus::Passed {
                                            verification = Some(VerificationRecord {
                                                gate: "post-merge".to_string(),
                                                passed: true,
                                                revision: Some(merged_revision.to_string()),
                                                parent_snapshot_revision: parent_snapshot_revision
                                                    .as_ref()
                                                    .map(|r| r.to_string()),
                                            });
                                        }
                                        // Link the merge into the admitting session's WAL so
                                        // `--undo` can revert it. Only a `Linked` outcome may be
                                        // reported as an applied merge; the other outcomes say
                                        // exactly what is (and is not) recoverable.
                                        let link = self.link_merge_in_session(
                                            &task_id,
                                            &session_id,
                                            &session_path,
                                            journal.as_mut(),
                                            &merged_revision,
                                            Some(&base_revision),
                                            turn as usize,
                                        );
                                        match link {
                                            MergeLinkOutcome::Linked => {
                                                summary = format!(
                                                    "{}; merge: applied (revision {}, written {:?}, removed {:?})",
                                                    summary, merged_revision, written_files, removed_files
                                                );
                                            }
                                            other => {
                                                state = TaskState::Failed;
                                                keep_pending_merge = other.retain_intent();
                                                summary = format!("{}; {}", summary, other.note());
                                            }
                                        }
                                    }
                                    Ok(WorktreeMergeResult::Conflict {
                                        conflicting_paths,
                                        reasons,
                                        ..
                                    }) => {
                                        state = TaskState::Failed;
                                        summary = format!(
                                            "{}; merge conflict, result NOT applied (paths {:?}): {}",
                                            summary,
                                            conflicting_paths,
                                            reasons.join("; ")
                                        );
                                    }
                                    Ok(WorktreeMergeResult::VerificationFailed {
                                        reverted,
                                        gate_output,
                                        ..
                                    }) => {
                                        // A merge that failed post-merge verification is never a
                                        // success, even when it was rolled back.
                                        state = TaskState::Failed;
                                        summary = format!(
                                            "{}; post-merge verification failed (reverted={}), NOT accepted: {}",
                                            summary,
                                            reverted,
                                            gate_output.lines().take(8).collect::<Vec<_>>().join(" | ")
                                        );
                                    }
                                    Err(e) => {
                                        state = TaskState::Failed;
                                        summary = format!(
                                            "{}; merge failed, result NOT applied: {}",
                                            summary, e
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
                (state, summary, o.tokens_used)
            }
            Err(e) => (TaskState::Failed, format!("resume error: {}", e), 0),
        };
        if let Some(mut wt) = worktree {
            let _ = wt.cleanup();
        }
        if let Err(e) = self.finish_child(
            &task_id,
            &ChildFinish {
                state,
                spent,
                summary,
                keep_pending_merge,
                verification,
            },
        ) {
            eprintln!(
                "[kroma] durable task settlement failed for '{}': {}",
                task_id, e
            );
        }
    }

    /// Link a resumed worker's merged revision into the task's own session WAL.
    ///
    /// The durable intent is bound to the merged revision **before** the append, so an intent that
    /// exists always names the revision to replay and the append is idempotent. The residual crash
    /// window is the interval between `merge_into_parent` returning and this intent update: the
    /// merge API clears its own journal before returning, so closing that window would require an
    /// API change inside the merge itself (not made here).
    #[allow(clippy::too_many_arguments)]
    fn link_merge_in_session(
        &self,
        task_id: &str,
        session_id: &str,
        session_path: &Path,
        journal: Option<&mut SessionJournal>,
        merged_revision: &RevisionId,
        parent: Option<&RevisionId>,
        turn: usize,
    ) -> MergeLinkOutcome {
        let Some(journal) = journal else {
            return MergeLinkOutcome::RecoveryRequired(
                "no session WAL was opened for this resume".to_string(),
            );
        };
        // A missing manifest only degrades the recorded metadata; the revision id — the part
        // `--undo` needs — is what the link must carry.
        let manifest = self.snapshot_store.load_revision(merged_revision).ok();
        // 1. Bind the merged revision to the durable intent first: from here on, any retained intent
        //    can be replayed. A failure here means the link cannot be recorded at all.
        let updated = {
            let mut registry = self.registry();
            registry.set_pending_merge(
                task_id,
                PendingMergeLink {
                    session_id: session_id.to_string(),
                    session_path: session_path.display().to_string(),
                    turn: turn as u32,
                    parent_revision: parent.map(|p| p.as_str().to_string()),
                    merged_revision: Some(merged_revision.as_str().to_string()),
                    changed_paths: manifest
                        .as_ref()
                        .map(|m| m.changed_paths.clone())
                        .unwrap_or_default(),
                    tracked_files: manifest
                        .as_ref()
                        .map(|m| m.tracked_files.len())
                        .unwrap_or(0),
                },
            )
        };
        if let Err(e) = updated {
            return MergeLinkOutcome::RecoveryRequired(format!(
                "the merge is applied but its session-bound link intent could not be updated ({})",
                e
            ));
        }
        let (changed_paths, tracked_files) = match &manifest {
            Some(manifest) => (manifest.changed_paths.clone(), manifest.tracked_files.len()),
            None => (Vec::new(), 0),
        };
        // 2. Append (idempotent: a revision already on this WAL is not written twice).
        if journal
            .revision_records()
            .iter()
            .any(|record| record.revision_id == merged_revision.as_str())
        {
            return MergeLinkOutcome::Linked;
        }
        match journal.record_revision_capture(
            turn,
            merged_revision.as_str(),
            parent.map(|p| p.as_str()),
            changed_paths,
            tracked_files,
        ) {
            Ok(()) => MergeLinkOutcome::Linked,
            Err(e) => MergeLinkOutcome::PendingReplay(format!("session WAL append failed: {}", e)),
        }
    }

    /// Replay session-bound merge intents left by a crash between a worker merge and its WAL append.
    ///
    /// Each replayable intent names the session WAL that admitted the task and the merged revision,
    /// so the link is re-recorded idempotently (a revision already present is skipped) and the intent
    /// cleared. An intent that carries **no** merged revision can never be replayed: it is cleared
    /// and reported in [`MergeReplayReport::unresolved`] so the condition is visible instead of
    /// silently skipped forever as a permanent orphan.
    ///
    /// Replay is **per intent**: an intent whose WAL cannot be opened (a locked WAL is a normal,
    /// transient condition), whose append fails, or whose intent cannot be cleared is recorded in
    /// [`MergeReplayReport::failed`] and left in place, and the remaining intents are still
    /// replayed. Only an intent that was fully resolved is counted in [`MergeReplayReport::linked`]
    /// and cleared. The outer `Result` is retained for callers that treat the whole replay as one
    /// step; per-intent failures are reported in the report, not by abandoning the loop.
    pub fn replay_pending_merges(&self) -> Result<MergeReplayReport, String> {
        let pending = self.registry().pending_merges();
        let mut report = MergeReplayReport::default();
        for (task_id, link) in pending {
            let Some(merged) = link.merged_revision.clone() else {
                // Explicitly unresolvable: report it and clear it rather than leaving an orphan.
                match self.registry().clear_pending_merge(&task_id) {
                    Ok(()) => report.unresolved.push(task_id),
                    Err(e) => report.failed.push((
                        task_id,
                        format!(
                            "the intent carries no merged revision and could not be cleared: {}",
                            e
                        ),
                    )),
                }
                continue;
            };
            let path = PathBuf::from(&link.session_path);
            let mut journal = match SessionJournal::open(&path) {
                Ok(journal) => journal,
                Err(e) => {
                    // A locked WAL is a normal condition, not a reason to abandon the other tasks.
                    // Keep this intent so the next startup retries it.
                    report.failed.push((
                        task_id,
                        format!(
                            "cannot open session WAL {} to replay the merge link: {}",
                            path.display(),
                            e
                        ),
                    ));
                    continue;
                }
            };
            if !journal
                .revision_records()
                .iter()
                .any(|record| record.revision_id == merged)
            {
                // Re-derive the metadata when the intent recorded none (the manifest was unreadable
                // when the link was first attempted).
                let (changed_paths, tracked_files) = if link.changed_paths.is_empty() {
                    match self
                        .snapshot_store
                        .load_revision(&RevisionId::new(merged.clone()))
                    {
                        Ok(manifest) => {
                            (manifest.changed_paths.clone(), manifest.tracked_files.len())
                        }
                        Err(_) => (Vec::new(), link.tracked_files),
                    }
                } else {
                    (link.changed_paths.clone(), link.tracked_files)
                };
                if let Err(e) = journal.record_revision_capture(
                    link.turn as usize,
                    &merged,
                    link.parent_revision.as_deref(),
                    changed_paths,
                    tracked_files,
                ) {
                    report
                        .failed
                        .push((task_id, format!("cannot replay the WAL link: {}", e)));
                    continue;
                }
            }
            if let Err(e) = self.registry().clear_pending_merge(&task_id) {
                // The link is recorded (idempotently) but the intent survived, so the next startup
                // must still see it. Report it rather than claiming a fully resolved replay.
                report.failed.push((
                    task_id,
                    format!(
                        "the WAL link was recorded but its intent could not be cleared: {}",
                        e
                    ),
                ));
                continue;
            }
            report.linked += 1;
        }
        Ok(report)
    }

    /// Persist the child's full report privately and return the matching artifact reference.
    ///
    /// The report is stored under the runtime state directory (never the workspace) with mode
    /// `0600`, bounded by [`MAX_REPORT_BYTES`], and the reference carries the digest and length of
    /// exactly the bytes written. An over-cap summary never blocks settlement: the first bytes are
    /// stored with a trailing notice naming the true original size, and the reference records the
    /// original size in [`ArtifactRef::original_bytes`] so it can never claim bytes that were not
    /// stored. The task id is validated as a single path component so it cannot escape the report
    /// directory.
    fn store_report(&self, task_id: &str, summary: &str) -> Result<ArtifactRef, String> {
        let component = report_file_component(task_id)?;
        let path = self.report_base.join(format!("{}.report", component));
        let (stored, truncated) = bounded_report_bytes(summary);
        write_private_file_atomically(&path, &stored)?;
        let mut artifact = ArtifactRef::new(
            format!("{}-report", task_id),
            format!("file:reports/{}.report", component),
            "report",
        )
        .with_sha256(crate::prompt::sha256_hex64(&stored))
        .with_bytes(stored.len() as u64);
        if truncated {
            artifact = artifact.with_original_bytes(summary.len() as u64);
        }
        Ok(artifact)
    }

    /// Resolve a task's durable report bytes, verifying the reference's digest and length.
    ///
    /// This is the consumer seam the artifact reference promises: after a restart the bytes are read
    /// back from the private report file and checked against the recorded hash, so a reference can
    /// never be resolved to bytes that do not match it.
    pub fn read_report(&self, task_id: &str) -> Result<String, String> {
        let record = self
            .registry()
            .get_cloned(task_id)
            .ok_or_else(|| format!("unknown task '{}'", task_id))?;
        let artifact = record
            .artifacts
            .iter()
            .find(|artifact| artifact.kind == "report")
            .ok_or_else(|| format!("task '{}' has no report artifact", task_id))?;
        let relative = artifact.location.strip_prefix("file:").ok_or_else(|| {
            format!(
                "report artifact for task '{}' is not a durable file reference: {}",
                task_id, artifact.location
            )
        })?;
        let path = self.report_base.join(
            Path::new(relative)
                .file_name()
                .ok_or_else(|| format!("report reference '{}' has no file name", relative))?,
        );
        // T06 (KR-06): refuse an oversized or symlinked report *before* allocating, instead of
        // reading whatever is on disk and comparing lengths afterwards. The limit is the length the
        // reference already recorded (capped by the report ceiling), so a report that grew — or a
        // path swapped for a symlink — fails closed rather than being parsed.
        let limit = artifact.bytes.min(MAX_REPORT_BYTES as u64);
        let bytes =
            crate::secure_fs::read_bounded_file(&path, limit, crate::secure_fs::Overflow::Refuse)
                .map_err(|e| format!("cannot read report {}: {}", path.display(), e))?
                .bytes;
        if bytes.len() as u64 != artifact.bytes {
            return Err(format!(
                "report for task '{}' is {} bytes but the reference records {}",
                task_id,
                bytes.len(),
                artifact.bytes
            ));
        }
        if let Some(expected) = &artifact.sha256 {
            let actual = crate::prompt::sha256_hex64(&bytes);
            if &actual != expected {
                return Err(format!(
                    "report for task '{}' hashes to {} but the reference records {}",
                    task_id, actual, expected
                ));
            }
        }
        String::from_utf8(bytes)
            .map_err(|e| format!("report for task '{}' is not UTF-8: {}", task_id, e))
    }

    /// Mark every task left `Running` by a previous process as `Suspended` so it can be resumed.
    pub fn recover_interrupted(&self) -> Result<usize, String> {
        let mut registry = self.registry();
        registry.recover_interrupted().map_err(|e| e.to_string())
    }

    /// `true` when a live run is bound to `task_id`.
    pub fn has_live_run(&self, task_id: &str) -> bool {
        self.state().cancellers.contains_key(task_id)
    }

    /// Number of live cancellation bindings.
    pub fn live_runs(&self) -> usize {
        self.state().cancellers.len()
    }

    /// Number of resumes admitted but not yet finished.
    pub fn resuming_count(&self) -> usize {
        self.state().resuming.len()
    }

    /// Path of the session WAL new admissions are bound to. An already-admitted task's own WAL is
    /// recorded on its registry record, not here.
    pub fn session_path(&self) -> PathBuf {
        self.current_binding().session_path
    }
}

/// Bound a report to [`MAX_REPORT_BYTES`].
///
/// Returns the bytes to store and whether the summary was truncated. An over-cap summary keeps its
/// first bytes (cut on a char boundary) and gains a trailing notice naming the true original size,
/// so a report is always storable and its truncation is explicit in the stored bytes themselves.
fn bounded_report_bytes(summary: &str) -> (Vec<u8>, bool) {
    let bytes = summary.as_bytes();
    if bytes.len() <= MAX_REPORT_BYTES {
        return (bytes.to_vec(), false);
    }
    let mut kept = MAX_REPORT_BYTES - REPORT_TRUNCATION_NOTICE_RESERVE;
    while kept > 0 && !summary.is_char_boundary(kept) {
        kept -= 1;
    }
    let notice = report_truncation_notice(bytes.len(), kept);
    // The notice is two decimal numbers plus fixed text, so it is far smaller than the reserve.
    debug_assert!(kept + notice.len() <= MAX_REPORT_BYTES);
    let mut stored = Vec::with_capacity(kept + notice.len());
    stored.extend_from_slice(&bytes[..kept]);
    stored.extend_from_slice(notice.as_bytes());
    (stored, true)
}

/// The trailing line appended to a truncated report: it names the true original byte count and
/// states that the stored bytes were truncated, so a reader can never mistake the stored bytes for
/// the whole summary.
fn report_truncation_notice(original_bytes: usize, kept_bytes: usize) -> String {
    format!(
        "\n[kroma] report truncated: the original summary was {} bytes; only the first {} bytes plus this notice were stored\n",
        original_bytes, kept_bytes
    )
}

/// Validate a task id as a single, safe path component for the private report directory.
fn report_file_component(task_id: &str) -> Result<&str, String> {
    let trimmed = task_id.trim();
    if trimmed.is_empty() {
        return Err("cannot store a report for an empty task id".to_string());
    }
    if trimmed == "." || trimmed == ".." {
        return Err(format!(
            "task id '{}' is not a usable report file name",
            task_id
        ));
    }
    if trimmed.contains('/')
        || trimmed.contains('\\')
        || trimmed.contains('\0')
        || trimmed.contains(':')
    {
        return Err(format!(
            "task id '{}' contains a path separator and cannot name a report file",
            task_id
        ));
    }
    Ok(trimmed)
}

/// Write `bytes` to `path` durably and privately: private directory, `0600` unique temp file,
/// `fsync`, rename. A failure cleans up its temp file and never leaves a half-written report at the
/// final name.
fn write_private_file_atomically(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| format!("report path {} has no directory", path.display()))?;
    crate::secure_fs::ensure_private_dir(parent).map_err(|e| e.to_string())?;
    let base = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("report");
    let tmp = parent.join(format!(".{}.tmp-{}", base, uuid::Uuid::new_v4()));
    let mut file = crate::secure_fs::create_new_no_symlink(&tmp, 0o600)
        .map_err(|e| format!("cannot create {}: {}", tmp.display(), e))?;
    let write_result = (|| -> std::io::Result<()> {
        use std::io::Write;
        file.write_all(bytes)?;
        file.sync_all()?;
        Ok(())
    })();
    drop(file);
    if let Err(e) = write_result {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("cannot write {}: {}", tmp.display(), e));
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("cannot replace {}: {}", path.display(), e));
    }
    Ok(())
}

/// Summary view of one task for tests/telemetry.
pub fn task_state_label(task: &AsyncTaskRecord) -> &'static str {
    task.state.as_str()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subagents::SubagentBudget;

    fn seed(registry: &mut AsyncTaskRegistry, spec_json: &str, state: TaskState) {
        registry
            .register(NewTask {
                task_id: "t".into(),
                session_id: "s".into(),
                owner_id: "o".into(),
                workspace_root: "/w".into(),
                role: "scout".into(),
                created_turn: 0,
                token_allocation: 10,
                prompt: "p".into(),
                timeout_ms: 1,
                spec_json: spec_json.into(),
                parent_policy: "allow-write".into(),
                parent_policy_hooks: ParentPolicyHooks::KnownEmpty,
                session_path: String::new(),
            })
            .unwrap();
        registry.transition("t", TaskState::Running, 0).unwrap();
        if state == TaskState::Suspended {
            registry.suspend("t", 1).unwrap();
        }
    }

    fn settle(registry: &mut AsyncTaskRegistry, desired: TaskState) -> Result<TaskState, String> {
        registry
            .settle_finish(
                "t",
                TaskFinish {
                    state: desired,
                    spent: 0,
                    artifact: None,
                    summary: "settled".into(),
                    turn: 2,
                    keep_pending_merge: false,
                    verification: None,
                },
            )
            .map_err(|e| e.to_string())
    }

    #[test]
    fn settle_finish_reaches_suspended_task_completion() {
        let mut registry = AsyncTaskRegistry::in_memory();
        seed(&mut registry, "{}", TaskState::Suspended);
        assert_eq!(
            settle(&mut registry, TaskState::Completed).unwrap(),
            TaskState::Completed
        );
        assert_eq!(registry.get("t").unwrap().state, TaskState::Completed);
    }

    #[test]
    fn settle_finish_never_overwrites_an_open_challenge() {
        let mut registry = AsyncTaskRegistry::in_memory();
        seed(&mut registry, "{}", TaskState::Running);
        registry.challenge("t", "bad", 1).unwrap();
        assert_eq!(
            settle(&mut registry, TaskState::Completed).unwrap(),
            TaskState::ChallengePending
        );
        assert_eq!(
            registry.get("t").unwrap().state,
            TaskState::ChallengePending
        );
    }

    #[test]
    fn settle_finish_is_one_commit_of_artifact_spend_state_and_notification() {
        let mut registry = AsyncTaskRegistry::in_memory();
        seed(&mut registry, "{}", TaskState::Running);
        registry
            .settle_finish(
                "t",
                TaskFinish {
                    state: TaskState::Completed,
                    spent: 7,
                    artifact: Some(ArtifactRef::new(
                        "t-report",
                        "file:reports/t.report",
                        "report",
                    )),
                    summary: "done".into(),
                    turn: 3,
                    keep_pending_merge: false,
                    verification: None,
                },
            )
            .unwrap();
        let task = registry.get("t").unwrap();
        assert_eq!(task.token_spent, 7);
        assert_eq!(task.artifacts.len(), 1);
        assert!(task.notifications.iter().any(|n| n.kind == "completed"));
    }

    #[test]
    fn keep_pending_merge_preserves_the_intent() {
        let mut registry = AsyncTaskRegistry::in_memory();
        seed(&mut registry, "{}", TaskState::Running);
        let link = PendingMergeLink {
            session_id: "s".into(),
            session_path: "/w/session.wal.jsonl".into(),
            turn: 2,
            parent_revision: None,
            merged_revision: Some("r-abc".into()),
            changed_paths: vec!["out.txt".into()],
            tracked_files: 1,
        };
        registry.set_pending_merge("t", link.clone()).unwrap();
        registry
            .settle_finish(
                "t",
                TaskFinish {
                    state: TaskState::Failed,
                    spent: 1,
                    artifact: None,
                    summary: "link failed".into(),
                    turn: 3,
                    keep_pending_merge: true,
                    verification: None,
                },
            )
            .unwrap();
        assert_eq!(registry.pending_merges(), vec![("t".to_string(), link)]);
        registry
            .settle_finish(
                "t",
                TaskFinish {
                    state: TaskState::Failed,
                    spent: 0,
                    artifact: None,
                    summary: "link replayed".into(),
                    turn: 4,
                    keep_pending_merge: false,
                    verification: None,
                },
            )
            .unwrap();
        assert!(registry.pending_merges().is_empty());
    }

    #[test]
    fn only_a_replayable_intent_may_claim_a_pending_replay() {
        assert!(!MergeLinkOutcome::Linked.retain_intent());
        assert!(!MergeLinkOutcome::Linked.note().contains("pending"));
        assert!(MergeLinkOutcome::PendingReplay("boom".into()).retain_intent());
        assert!(MergeLinkOutcome::PendingReplay("boom".into())
            .note()
            .contains("will be replayed"));
        let recovery = MergeLinkOutcome::RecoveryRequired("boom".into());
        assert!(!recovery.retain_intent());
        let note = recovery.note();
        assert!(note.contains("RECOVERY REQUIRED"), "{}", note);
        assert!(
            !note.contains("pending"),
            "a non-replayable link must not claim a pending replay: {}",
            note
        );
    }

    #[test]
    fn a_spec_round_trips_through_the_registry() {
        // The persisted spec must carry the whole contract, not just the prompt.
        let spec = SubagentSpec {
            role: SubagentRole::Worker,
            task: "edit a file".into(),
            allowed_write_globs: Some(vec!["src/**".into()]),
            budget: SubagentBudget {
                max_tokens: 500,
                timeout_ms: 1234,
                ..SubagentBudget::default()
            },
            revert_on_verify_failure: true,
            disallow_test_tampering: true,
            protected_paths: vec!["tests/**".into()],
        };
        let json = serde_json::to_string(&spec).unwrap();
        let back: SubagentSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(back, spec);
    }
}
