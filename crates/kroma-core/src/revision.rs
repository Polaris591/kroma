//! Workspace revision snapshots, three-state restore, and crash-safe restore journaling.
//!
//! Gate 1 of the v1.0 blueprint: bind workspace file versions to the session WAL so an agent turn
//! can be rewound without ever silently clobbering work the user did outside Kroma.
//!
//! Storage is a manifest + content-addressed blob store kept *outside* the workspace tree. The
//! blueprint prefers Git objects first, but a self-contained CAS avoids requiring `git` and avoids
//! touching the user's repository; [`SnapshotStore`] is the abstraction that lets a Git-backed or
//! Merkle-backed backend be swapped in later once measured against this baseline.

use crate::prompt::sha256_hex64;
use crate::sensitive_paths::SensitivePathGuard;
use crate::session::{SessionError, SessionJournal};
use crate::workspace::{WorkspaceCap, WorkspaceError};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
#[cfg(unix)]
use std::ffi::{CString, OsStr};
use std::fs;
#[cfg(unix)]
use std::fs::File;
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, PermissionsExt};
#[cfg(unix)]
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;
use uuid::Uuid;

const BLOB_DIR: &str = "blobs";
const REVISION_DIR: &str = "revisions";
const RESTORE_INTENT_FILE: &str = "restore-in-progress.json";
const RESTORE_INTENT_VERSION: u32 = 1;
/// F03 (`v1.4.0`): durable intent journal for a multi-file worktree→parent merge, distinct from the
/// restore intent so a partially-applied merge is detectable and recoverable without confusing the
/// `--undo` path.
const MERGE_INTENT_FILE: &str = "merge-in-progress.json";
const MERGE_INTENT_VERSION: u32 = 1;
/// F03 follow-up (`v1.5.2`): durable intent journal for a workspace-bound fork. A fork restores the
/// workspace to the turn's revision and only then re-points the session leaf; a crash between the
/// two phases would otherwise leave the files at the target revision while the WAL still described
/// the old branch. The intent records the target so the next entry point can finish phase 2.
const FORK_INTENT_FILE: &str = "fork-in-progress.json";
const FORK_INTENT_VERSION: u32 = 1;

/// Largest single file that may be tracked. Larger files are recorded in `skipped_paths` so a
/// vendored binary or dataset can never make a snapshot unbounded.
pub const MAX_SNAPSHOT_FILE_BYTES: u64 = 4 * 1024 * 1024;
/// Largest total tracked payload per revision.
pub const MAX_SNAPSHOT_TOTAL_BYTES: u64 = 256 * 1024 * 1024;
/// Directory recursion guard so a pathological/deep tree cannot exhaust the stack.
const MAX_SNAPSHOT_DEPTH: usize = 32;

/// T06 (KR-06): ceiling for a crash-recovery intent journal (merge/fork/restore). These are small
/// JSON documents; anything larger is corruption or an attack, and refusing is better than a
/// partially applied recovery.
const MAX_INTENT_BYTES: u64 = 64 * 1024;
/// Bounded manifest limit. 8 MiB covers tens of thousands of ordinary source files while
/// retaining an absolute reader/writer allocation cap. A sharded manifest is still required
/// for repositories larger than this supported envelope.
const MAX_REVISION_RECORD_BYTES: u64 = 8 * 1024 * 1024;

/// Build outputs and dependency caches are never part of a reviewable revision.
pub(crate) const SNAPSHOT_EXCLUDED_DIRS: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    "target",
    "node_modules",
    ".venv",
    "venv",
    "__pycache__",
    ".mypy_cache",
    ".pytest_cache",
    ".gradle",
    ".tox",
    ".next",
    "dist",
    "build",
];

#[derive(Debug, Error)]
pub enum RevisionError {
    #[error("workspace error: {0}")]
    Workspace(#[from] WorkspaceError),
    #[error("snapshot store I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("snapshot store serialization error: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("unknown workspace revision: {0}")]
    UnknownRevision(String),
    #[error("invalid snapshot store: {0}")]
    InvalidStore(String),
    /// KRV4-01: the record would exceed the same ceiling the reader enforces. Refused here, before
    /// it becomes a revision anything can reference - a record that can be written but never read is
    /// worse than a capture that fails loudly.
    #[error(
        "revision record is {bytes} bytes, over the {limit}-byte ceiling; this workspace has too \
         many tracked files for one manifest"
    )]
    RecordTooLarge { bytes: u64, limit: u64 },
    #[error("workspace merge recovery required: {reason} (journal: {journal_path:?}, restore target: {target_revision})")]
    RecoveryRequired {
        target_revision: RevisionId,
        journal_path: PathBuf,
        reason: String,
    },
}

/// Content-addressed workspace revision identifier: `r-` plus 32 hex chars of the manifest digest.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct RevisionId(String);

impl RevisionId {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Reject anything that is not `<r-><32 hex>` so a caller-supplied `--restore-rev` value can
    /// never be used to address a file outside the revision directory.
    pub fn parse(raw: &str) -> Result<Self, RevisionError> {
        let trimmed = raw.trim();
        let Some(hex) = trimmed.strip_prefix("r-") else {
            return Err(RevisionError::UnknownRevision(raw.to_string()));
        };
        if hex.len() != 32 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(RevisionError::UnknownRevision(raw.to_string()));
        }
        Ok(Self(trimmed.to_string()))
    }
}

impl std::fmt::Display for RevisionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// One tracked regular file inside a revision manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrackedFile {
    pub sha256: String,
    pub mode: u32,
    pub size: u64,
}

/// Metadata for a captured workspace state (§5 `WorkspaceRevision`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceRevision {
    /// Content address of this tree (path + hash + mode). See `docs/adr/0001-snapshot-identity.md`:
    /// identical content *is* the same snapshot, even when it was reached twice.
    pub revision_id: RevisionId,
    /// Best-effort pointer to the revision captured just before this one.
    ///
    /// **Not an immutable history graph.** `revision_id` is content-addressed, so returning to earlier
    /// content overwrites this manifest and rewrites the pointer (A → B → A loses the A→B edge here).
    /// The authoritative history is the session WAL's `RevisionRecord`; undo and `--restore-rev` targets
    /// must be resolved from there (`revision_at_or_before_turn`, checkpoints), never by walking this
    /// field.
    pub parent_revision_id: Option<RevisionId>,
    #[serde(default)]
    pub wal_entry_id: Option<String>,
    pub turn_index: usize,
    pub timestamp_ms: u64,
    pub tracked_files: BTreeMap<String, TrackedFile>,
    /// Sorted paths that differ from the parent revision (added, removed, or modified).
    pub changed_paths: Vec<String>,
    /// Paths deliberately excluded (symlink, sensitive, build output, oversized, special file).
    pub skipped_paths: Vec<String>,
    pub total_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictKind {
    /// The path exists live with a different hash than the expected revision.
    ModifiedExternally,
    /// The expected revision has the path, but it is gone from the live workspace.
    DeletedExternally,
    /// The path is absent from the expected revision but exists live.
    CreatedExternally,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RevisionConflict {
    pub path: String,
    pub kind: ConflictKind,
    pub expected_sha256: Option<String>,
    pub live_sha256: Option<String>,
}

/// Three-state restore outcome. A `bool` cannot express "refused because the user edited the file"
/// or "a previous restore was interrupted and the workspace needs recovery".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum RestoreResult {
    Applied {
        restored_revision: RevisionId,
        restored_files: Vec<String>,
        removed_files: Vec<String>,
    },
    Conflict {
        target_revision: RevisionId,
        conflicting_paths: Vec<RevisionConflict>,
    },
    RecoveryRequired {
        target_revision: RevisionId,
        journal_path: PathBuf,
        reason: String,
    },
}

impl RestoreResult {
    pub fn restored_revision(&self) -> &RevisionId {
        match self {
            RestoreResult::Applied {
                restored_revision, ..
            } => restored_revision,
            RestoreResult::Conflict {
                target_revision, ..
            } => target_revision,
            RestoreResult::RecoveryRequired {
                target_revision, ..
            } => target_revision,
        }
    }
}

/// Deterministic fault points for the crash-recovery tests required by the Gate 1 plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestoreFault {
    /// Crash immediately after the intent journal is durably written, before any file is touched.
    AfterIntentWritten,
    /// Crash after `n` planned operations have been applied.
    AfterAppliedFiles(usize),
    /// Test-only: make the first directory `fsync` performed while applying the operations fail, so
    /// the restore must report [`RestoreResult::RecoveryRequired`] and keep its intent for replay.
    #[cfg(test)]
    DirectorySyncFails,
}

/// Deterministic fault points for the workspace-bound fork crash-recovery tests (P1-2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForkFault {
    /// Crash after the restore applied and cleared its own journal, but before the session leaf
    /// moved. This is exactly the window the durable fork intent closes.
    AfterRestoreBeforeLeafMove,
}

/// Serde default for a per-operation `force` flag that a journal written before the field existed
/// omitted. Such a journal recorded only the intent-level force, which the previous binary applied
/// to its removals, so defaulting a missing flag to `true` keeps a forced restore's removals forced
/// instead of silently degrading them to a no-op on an obstructing symlink/directory (F03). A
/// journal that does carry the flag (every one this binary writes) is unaffected, so a merge-rollback
/// `Remove { force: false }` still round-trips as non-forced.
fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum RestoreOp {
    Write {
        path: String,
        sha256: String,
        mode: u32,
    },
    /// `force` is carried per operation rather than only on [`RestoreIntent`] so a merge-rollback
    /// removal can stay non-forced even when the surrounding intent restores forced writes; a
    /// crash mid-recovery then replays the exact same per-path semantics.
    Remove {
        path: String,
        #[serde(default = "default_true")]
        force: bool,
    },
}

impl RestoreOp {
    fn path(&self) -> &str {
        match self {
            RestoreOp::Write { path, .. } | RestoreOp::Remove { path, .. } => path,
        }
    }
}

/// The durable restore intent: written and fsynced before the first mutation, removed only after
/// the last one, so an interrupted restore is always detectable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestoreIntent {
    pub version: u32,
    pub target_revision: RevisionId,
    pub parent_revision: Option<RevisionId>,
    pub created_at_ms: u64,
    /// Whether the interrupted restore was an explicit `--force-restore`, so recovery re-applies
    /// the same overwrite semantics instead of stalling on an obstructing symlink/directory.
    #[serde(default)]
    pub force: bool,
    pub operations: Vec<RestoreOp>,
}

/// Manifest + content-addressed blob store for workspace revisions.
#[derive(Debug, Clone)]
pub struct SnapshotStore {
    root: PathBuf,
}

/// F03 (`v1.4.0`): durable intent journal for a worktree→parent merge. It is written and fsynced
/// before the first parent file is touched, and removed only after the merged revision has been
/// durably captured. Its presence on startup means a merge was interrupted and the parent workspace
/// may be partially modified.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeIntent {
    pub version: u32,
    /// The revision the parent workspace is restored to if the merge must be rolled back.
    pub pre_merge_revision: RevisionId,
    pub created_at_ms: u64,
    /// Parent-relative paths the merge intends to write.
    #[serde(default)]
    pub written_files: Vec<String>,
    /// Parent-relative paths the merge intends to remove.
    #[serde(default)]
    pub removed_files: Vec<String>,
}

/// F08 follow-up: the post-merge state of one path, as the merge intended to leave it — the content
/// it is about to write there. It is recorded in the merge intent journal *before* the first parent
/// file is touched, so a rollback can prove that the bytes it finds on a written path are the
/// merge's own output. Paths the merge removes need no hash: [`MergeIntent::removed_files`] already
/// records that their intended state is "absent".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeAppliedFile {
    pub path: String,
    pub sha256: String,
    /// The permission bits the merge wrote, with the same `& 0o7777` convention the revision
    /// manifests use. `None` means the journal predates mode evidence: it proves the content the
    /// merge wrote but says nothing about permissions, so a rollback cannot accept it as proof that
    /// the live file is untouched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<u32>,
}

/// F08 follow-up: the merge intent journal exactly as it is stored on disk — the intent plus the
/// optional record of what the merge intended to write.
///
/// Both live in one file so the evidence cannot drift away from the intent it belongs to: a journal
/// read for recovery carries its own `applied_files`. The section is optional because journals
/// written before it existed carry only the intent; such a journal proves the pre-merge state of a
/// path and nothing about the merge's own writes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeIntentJournal {
    #[serde(flatten)]
    pub intent: MergeIntent,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub applied_files: Vec<MergeAppliedFile>,
}

/// F03 follow-up (`v1.5.2`): the durable intent journal for a workspace-bound fork. It is written
/// and fsynced *before* the workspace is restored and removed only after the session leaf has been
/// re-pointed, so a crash between the two phases is detectable and its phase 2 can be completed
/// idempotently. It is bound to the session and workspace it was written for, because one snapshot
/// store can be shared by several sessions in the same state directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForkIntent {
    pub version: u32,
    /// The session WAL whose leaf the fork moves.
    pub session_id: String,
    /// Workspace the fork restores, so a stale intent written for another workspace is ignored.
    pub workspace_root: String,
    pub forked_at_turn: usize,
    pub target_revision: RevisionId,
    /// The leaf's revision at the moment the fork began, used to re-bind the restored revision.
    pub expected_revision: Option<RevisionId>,
    pub created_at_ms: u64,
}

/// F03 follow-up (`v1.5.2`): outcome of the startup pre-check for an interrupted workspace-bound
/// fork.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForkRecoveryOutcome {
    /// No fork intent journal was present (or it belonged to another session/workspace).
    NothingPending,
    /// The interrupted fork's phase 2 was completed: the leaf now names the target and the restored
    /// revision is recorded, matching the files already on disk.
    Completed(WorkspaceBoundForkOutcome),
    /// The workspace could not be brought to the target revision (live edits conflicted), so the
    /// fork was abandoned before the leaf moved. The workspace and leaf are untouched and the
    /// journal was cleared.
    Abandoned {
        forked_at_turn: usize,
        target_revision: RevisionId,
        conflicting_paths: Vec<RevisionConflict>,
    },
}

/// F08 (`v1.5.1`): outcome of the startup pre-check for an interrupted worktree→parent merge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeRecoveryOutcome {
    /// No merge intent journal was present; the workspace was left untouched.
    NothingPending,
    /// A stale merge intent journal was present but the merge it describes had already been
    /// captured as `merged_revision`. The journal was cleared and the workspace was left untouched,
    /// because rolling back a completed merge would silently discard it.
    AlreadyMerged { merged_revision: RevisionId },
    /// The interrupted merge was rolled back to its pre-merge revision and the journal cleared.
    RolledBack {
        pre_merge_revision: RevisionId,
        restored_files: Vec<String>,
        removed_files: Vec<String>,
    },
}

/// F03: a workspace-bound fork completed both phases — the workspace was restored to the revision
/// that was current at `forked_at_turn`, and only then was the session leaf re-pointed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceBoundForkOutcome {
    pub leaf_id: String,
    pub forked_at_turn: usize,
    pub target_revision: RevisionId,
    pub restored_files: Vec<String>,
    pub removed_files: Vec<String>,
    pub turn_count: usize,
    pub latest_revision_id: Option<String>,
}

/// Why a workspace-bound fork did not happen. Every variant leaves the workspace and the session
/// leaf untouched: the restore is attempted first and only a successful [`RestoreResult::Applied`]
/// moves the leaf.
#[derive(Debug, Error)]
pub enum WorkspaceBoundForkError {
    #[error("no active transcript entry at or before turn {0}")]
    NoEntryAtTurn(usize),
    #[error(
        "no workspace revision recorded at or before turn {0}; fork with mode 'conversation' to branch without restoring files"
    )]
    NoRevisionAtTurn(usize),
    #[error(
        "refusing to fork to turn {turn}: {count} path(s) were modified since the expected revision",
        count = conflicting_paths.len()
    )]
    Conflict {
        turn: usize,
        target_revision: RevisionId,
        conflicting_paths: Vec<RevisionConflict>,
    },
    #[error("fork to turn {turn} requires recovery of {target_revision}: {reason}")]
    RecoveryRequired {
        turn: usize,
        target_revision: RevisionId,
        journal_path: PathBuf,
        reason: String,
    },
    #[error(transparent)]
    Revision(#[from] RevisionError),
    #[error(transparent)]
    Session(#[from] SessionError),
}

impl SnapshotStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn intent_path(&self) -> PathBuf {
        self.root.join(RESTORE_INTENT_FILE)
    }

    /// F03 (`v1.4.0`): path of the durable merge intent journal.
    pub fn merge_intent_path(&self) -> PathBuf {
        self.root.join(MERGE_INTENT_FILE)
    }

    /// F03 (`v1.4.0`): durably record that a multi-file merge into the parent workspace is about to
    /// start. Written *before* the first parent file is touched so a crash mid-merge is detectable.
    ///
    /// This form records no evidence of what the merge writes; use
    /// [`Self::write_merge_intent_with_applied`] so an interrupted merge stays rollback-able.
    pub fn write_merge_intent(&self, intent: &MergeIntent) -> Result<(), RevisionError> {
        self.write_merge_intent_with_applied(intent, &[])
    }

    /// F08 follow-up: record the merge intent together with the exact content the merge is about to
    /// write on each path, in the same journal file and the same fsynced write. A rollback can then
    /// prove per path that the live bytes are either the pre-merge state (recorded by the pre-merge
    /// revision) or the merge's own output, instead of overwriting content it cannot account for.
    pub fn write_merge_intent_with_applied(
        &self,
        intent: &MergeIntent,
        applied_files: &[MergeAppliedFile],
    ) -> Result<(), RevisionError> {
        self.ensure_layout()?;
        let journal = MergeIntentJournal {
            intent: intent.clone(),
            applied_files: applied_files.to_vec(),
        };
        write_atomic_bytes(
            &self.merge_intent_path(),
            &serde_json::to_vec(&journal)?,
            0o600,
        )
    }

    /// F03 (`v1.4.0`): read the pending merge intent journal, if one is present.
    pub fn pending_merge_intent(&self) -> Result<Option<MergeIntent>, RevisionError> {
        Ok(self.pending_merge_journal()?.map(|journal| journal.intent))
    }

    /// F08 follow-up: read the pending merge intent journal *with* the applied-content evidence it
    /// was written with, in one read so the two can never come from different journal versions.
    pub fn pending_merge_journal(&self) -> Result<Option<MergeIntentJournal>, RevisionError> {
        let path = self.merge_intent_path();
        let mut file = match crate::secure_fs::open_existing_no_symlink(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        // T06 (KR-06): bounded on the verified handle, not on a pathname length.
        let bytes = crate::secure_fs::read_bounded_handle(
            &mut file,
            "merge intent journal",
            MAX_INTENT_BYTES,
            crate::secure_fs::Overflow::Refuse,
        )?
        .bytes;
        let journal: MergeIntentJournal = serde_json::from_slice(&bytes)?;
        if journal.intent.version != MERGE_INTENT_VERSION {
            return Err(RevisionError::InvalidStore(format!(
                "unsupported merge intent journal version {}",
                journal.intent.version
            )));
        }
        Ok(Some(journal))
    }

    /// F03 (`v1.4.0`): clear the merge intent journal after a successful merge (or a clean
    /// rollback).
    pub fn clear_merge_intent(&self) -> Result<(), RevisionError> {
        match fs::remove_file(self.merge_intent_path()) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        sync_dir(&self.root)
    }

    /// F03 follow-up (`v1.5.2`): path of the durable fork intent journal.
    pub fn fork_intent_path(&self) -> PathBuf {
        self.root.join(FORK_INTENT_FILE)
    }

    /// F03 follow-up (`v1.5.2`): durably record that a workspace-bound fork is about to start,
    /// before the restore touches any file and before the leaf moves.
    pub fn write_fork_intent(&self, intent: &ForkIntent) -> Result<(), RevisionError> {
        self.ensure_layout()?;
        write_atomic_bytes(
            &self.fork_intent_path(),
            &serde_json::to_vec(intent)?,
            0o600,
        )
    }

    /// F03 follow-up (`v1.5.2`): read the pending fork intent journal, if one is present.
    pub fn pending_fork_intent(&self) -> Result<Option<ForkIntent>, RevisionError> {
        let path = self.fork_intent_path();
        let mut file = match crate::secure_fs::open_existing_no_symlink(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let bytes = crate::secure_fs::read_bounded_handle(
            &mut file,
            "fork intent journal",
            MAX_INTENT_BYTES,
            crate::secure_fs::Overflow::Refuse,
        )?
        .bytes;
        let intent: ForkIntent = serde_json::from_slice(&bytes)?;
        if intent.version != FORK_INTENT_VERSION {
            return Err(RevisionError::InvalidStore(format!(
                "unsupported fork intent journal version {}",
                intent.version
            )));
        }
        Ok(Some(intent))
    }

    /// F03 follow-up (`v1.5.2`): clear the fork intent journal after the fork fully finished (or was
    /// abandoned before the leaf moved).
    pub fn clear_fork_intent(&self) -> Result<(), RevisionError> {
        match fs::remove_file(self.fork_intent_path()) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        sync_dir(&self.root)
    }

    /// F03 follow-up (`v1.5.2`): complete or abandon a workspace-bound fork that crashed between
    /// its two phases.
    ///
    /// The restore is re-derived from the durable intent (its own [`RestoreIntent`] still wins if it
    /// is present), then phase 2 is applied idempotently: the leaf is moved to the forked turn and
    /// the restored revision is re-bound in the WAL only if it is not already the current one. A
    /// live edit that prevents reaching the target abandons the fork *before* the leaf moves, so a
    /// half-fork never survives. Intents written for another session or workspace are ignored.
    pub fn recover_pending_fork(
        &self,
        workspace: &WorkspaceCap,
        journal: &mut SessionJournal,
    ) -> Result<ForkRecoveryOutcome, RevisionError> {
        let Some(intent) = self.pending_fork_intent()? else {
            return Ok(ForkRecoveryOutcome::NothingPending);
        };
        if intent.session_id != journal.session_id()
            || intent.workspace_root != workspace.root().to_string_lossy().into_owned()
        {
            return Ok(ForkRecoveryOutcome::NothingPending);
        }

        // Phase 1: finish the restore if it is still mid-flight, or re-apply it when the crash
        // happened before the restore intent was even written (the workspace was then untouched).
        let restore_result = if let Some(restore) = self.pending_restore()? {
            if restore.target_revision != intent.target_revision {
                self.clear_fork_intent()?;
                return Ok(ForkRecoveryOutcome::NothingPending);
            }
            // The target match proves only that this journal belongs to the fork; it is not evidence
            // that a path still holds what the journal recorded. A strict replay therefore decides:
            // an unprovable path is reported below and the fork is abandoned instead of overwriting
            // work that appeared while Kroma was down.
            self.recover_pending_restore(workspace)?
        } else {
            self.restore_revision(workspace, &intent.target_revision, None, false)?
        };
        match restore_result {
            RestoreResult::Applied { .. } => {}
            RestoreResult::Conflict {
                conflicting_paths, ..
            } => {
                // External edits make the fork unsafe. Abandon it before the leaf moves so the
                // WAL never describes files that were not restored.
                self.clear_fork_intent()?;
                return Ok(ForkRecoveryOutcome::Abandoned {
                    forked_at_turn: intent.forked_at_turn,
                    target_revision: intent.target_revision,
                    conflicting_paths,
                });
            }
            RestoreResult::RecoveryRequired { reason, .. } => {
                return Err(RevisionError::InvalidStore(format!(
                    "cannot complete an interrupted fork: {}",
                    reason
                )));
            }
        }

        // Phase 2 (idempotent): move the leaf, then re-bind the restored revision only if the fork
        // had not already recorded it before the crash.
        let leaf_id = journal.fork_at_turn(intent.forked_at_turn).map_err(|e| {
            RevisionError::InvalidStore(format!(
                "cannot complete an interrupted fork: failed to move the session leaf: {}",
                e
            ))
        })?;
        if journal.latest_revision_id().as_deref() != Some(intent.target_revision.as_str()) {
            record_restored_revision(
                journal,
                self,
                &intent.target_revision,
                &[],
                &[],
                intent.expected_revision.as_ref(),
            )
            .map_err(|e| {
                RevisionError::InvalidStore(format!(
                    "cannot complete an interrupted fork: failed to record the restored revision: {}",
                    e
                ))
            })?;
        }
        self.clear_fork_intent()?;
        Ok(ForkRecoveryOutcome::Completed(WorkspaceBoundForkOutcome {
            leaf_id,
            forked_at_turn: intent.forked_at_turn,
            target_revision: intent.target_revision,
            restored_files: Vec::new(),
            removed_files: Vec::new(),
            turn_count: journal.max_active_turn(),
            latest_revision_id: journal.latest_revision_id(),
        }))
    }

    /// F08 (`v1.5.1`): the startup counterpart of [`write_merge_intent`].
    ///
    /// If a worktree→parent merge was interrupted after its intent journal was written, the parent
    /// workspace may hold a half-applied merge. Undo exactly the paths the intent recorded —
    /// restoring each one to its pre-merge content, or removing it when the merge newly created it —
    /// and then clear both journals. This mirrors [`recover_pending_restore`], which replays the
    /// operations of an interrupted restore.
    ///
    /// The rollback is itself journalled through the restore intent, so a crash *during* recovery is
    /// recoverable on the next startup exactly like an interrupted explicit restore.
    ///
    /// A journal is only stale, not interrupted: a crash between capturing the merged revision and
    /// clearing the journal leaves the merge fully applied. That case is detected and only the
    /// journal is dropped ([`MergeRecoveryOutcome::AlreadyMerged`]), because rolling a completed
    /// merge back would silently discard it.
    ///
    /// The rollback only touches a path whose live state it can account for: the pre-merge state the
    /// intent names, or the state the merge itself left behind. Work that appeared on a merge path
    /// while Kroma was down is neither, so it is reported as [`RevisionError::RecoveryRequired`] and
    /// the journal is kept instead of being overwritten.
    pub fn recover_pending_merge(
        &self,
        workspace: &WorkspaceCap,
    ) -> Result<MergeRecoveryOutcome, RevisionError> {
        self.ensure_layout()?;
        let Some(journal) = self.pending_merge_journal()? else {
            return Ok(MergeRecoveryOutcome::NothingPending);
        };
        let MergeIntentJournal {
            intent,
            applied_files,
        } = journal;
        // A restore intent already on disk means a previous recovery (ours or an explicit restore)
        // was interrupted; finish it before layering the merge rollback on top of its half-applied
        // operations. Doing this *before* the completed-merge check keeps an unrelated interrupted
        // restore (for example a post-merge verification revert) from being mistaken for the merge's
        // own state. A restore that cannot finish (external edits, an apply failure) must stop the
        // rollback here: layering it on a half-applied restore would write over a workspace whose
        // real state is still unknown.
        if self.pending_restore()?.is_some() {
            // The rollback this recovery wrote records no parent (the half-merged tree was never
            // captured), so its pre-state is unknown by construction. Belonging to this merge is not
            // evidence that a path still holds what the journal recorded, and a forced replay would
            // overwrite whatever the user changed while Kroma was down. Replay strictly and stop the
            // rollback below when the workspace cannot be proven: recovery then needs an explicit
            // confirmation instead of silently winning.
            match self.recover_pending_restore(workspace)? {
                RestoreResult::Applied { .. } => {}
                RestoreResult::Conflict {
                    conflicting_paths, ..
                } => {
                    return Err(RevisionError::RecoveryRequired {
                        target_revision: intent.pre_merge_revision.clone(),
                        journal_path: self.merge_intent_path(),
                        reason: format!(
                            "cannot roll back the interrupted merge: the interrupted restore that must finish first has {} path(s) changed since it began; resolve them and retry",
                            conflicting_paths.len()
                        ),
                    });
                }
                RestoreResult::RecoveryRequired { reason, .. } => {
                    return Err(RevisionError::InvalidStore(format!(
                        "cannot roll back the interrupted merge: {}",
                        reason
                    )));
                }
            }
        }
        // F03/F08: a crash between capturing the merged revision and clearing the journal leaves a
        // *stale* journal even though the merge fully landed. Rolling it back now would silently
        // discard a successful merge (and desync the workspace from the session WAL), so detect the
        // completed merge and only drop the journal.
        if let Some(merged_revision) = self.merge_intent_already_landed(workspace, &intent)? {
            self.clear_merge_intent()?;
            return Ok(MergeRecoveryOutcome::AlreadyMerged { merged_revision });
        }
        let target = match self.load_revision(&intent.pre_merge_revision) {
            Ok(t) => t,
            Err(_) => {
                return Err(RevisionError::RecoveryRequired {
                    target_revision: intent.pre_merge_revision.clone(),
                    journal_path: self.merge_intent_path(),
                    reason: "the pre-merge revision recorded by the interrupted merge is not in the snapshot store; the parent workspace cannot be rolled back automatically".to_string(),
                });
            }
        };
        let mut touched: BTreeSet<String> = intent.written_files.iter().cloned().collect();
        touched.extend(intent.removed_files.iter().cloned());
        let mut restored_files = Vec::new();
        let mut removed_files = Vec::new();
        let mut removals: Vec<String> = Vec::new();
        let mut writes: Vec<(String, String, u32)> = Vec::new();
        for path in touched {
            match target.tracked_files.get(&path) {
                Some(entry) => {
                    writes.push((path.clone(), entry.sha256.clone(), entry.mode));
                    restored_files.push(path);
                }
                None => {
                    removals.push(path.clone());
                    removed_files.push(path);
                }
            }
        }
        // The same contract as `restore_inner`: every removal is scheduled before every write,
        // deepest path first, so a path whose kind changed between revisions (file <-> directory)
        // is vacated before its replacement lands. `touched` is a `BTreeSet`, so lexicographic order
        // alone would emit `Write("x")` before `Remove("x/y")` and wedge the next startup in
        // `ENOTDIR` (`SymlinkBlocked`) after the intent was already persisted.
        removals.sort_by(|a, b| {
            let depth = |s: &String| s.matches('/').count();
            depth(b).cmp(&depth(a)).then_with(|| b.cmp(a))
        });
        // Rollback removals are non-forced, so a symlink/special file the user parked on a
        // merge-created path is left in place. That is harmless on its own, but when a *later write*
        // needs that same path as a directory (the merge replaced a directory with a file, so the
        // rollback removes the file and restores the directory's children), the forced write would
        // hit `SymlinkBlocked` after the journal was already persisted and wedge every future
        // startup in the rollback replay. Detect it up front and fail with an actionable message
        // instead. A real directory is not a problem: the forced write lands inside it.
        for path in &removals {
            if !writes
                .iter()
                .any(|(written, _, _)| is_ancestor_path(path, written))
            {
                continue;
            }
            if matches!(stat_live_entry(workspace, path)?, LiveEntryStat::NonRegular) {
                return Err(RevisionError::RecoveryRequired {
                    target_revision: intent.pre_merge_revision.clone(),
                    journal_path: self.merge_intent_path(),
                    reason: format!(
                        "cannot roll back the interrupted merge: restoring '{}' requires it to be a directory again, but a symlink or special file is in its place; remove it and retry",
                        path
                    ),
                });
            }
        }
        // The removal check above only covers write paths whose own ancestor is scheduled for
        // removal. Two obstructions on the write path are left, and both would surface *after* the
        // journal was persisted and then wedge every future startup in the replay:
        //   * a symlink/special file on a non-touched ancestor component makes
        //     `open_parent_dir_no_symlink` fail (`SymlinkBlocked`), so the forced write can never
        //     land;
        //   * a directory the user parked on the write path itself would be recursively deleted by
        //     the forced write, destroying external work the merge never created.
        // Detect both before persisting the journal. A directory is only safe when a scheduled
        // removal lives inside it: that is the `directory -> file` rollback, where the removal
        // prunes the merge's own tree before the file is written back.
        for (path, _, _) in &writes {
            let mut prefix = String::new();
            let components: Vec<&str> = path.split('/').collect();
            for component in &components[..components.len().saturating_sub(1)] {
                if !prefix.is_empty() {
                    prefix.push('/');
                }
                prefix.push_str(component);
                match stat_live_entry(workspace, &prefix)? {
                    LiveEntryStat::NonRegular => {
                        return Err(RevisionError::RecoveryRequired {
                            target_revision: intent.pre_merge_revision.clone(),
                            journal_path: self.merge_intent_path(),
                            reason: format!(
                                "cannot roll back the interrupted merge: '{}' is blocked by a symlink or special file at '{}'; remove it and retry",
                                path, prefix
                            ),
                        });
                    }
                    // A plain file on an ancestor component makes `open_parent_dir_no_symlink`
                    // fail with `ENOTDIR` (`SymlinkBlocked`), which would wedge recovery after the
                    // journal was persisted. It is only safe when a scheduled removal unlinks it
                    // first — the `directory -> file` rollback where the merge wrote the file `d`
                    // and the rollback must restore `d/a`.
                    LiveEntryStat::Regular { .. } if !removals.iter().any(|r| r == &prefix) => {
                        return Err(RevisionError::RecoveryRequired {
                            target_revision: intent.pre_merge_revision.clone(),
                            journal_path: self.merge_intent_path(),
                            reason: format!(
                                "cannot roll back the interrupted merge: '{}' is blocked by a plain file at '{}'; remove it and retry",
                                path, prefix
                            ),
                        });
                    }
                    _ => {}
                }
            }
            if matches!(stat_live_entry(workspace, path)?, LiveEntryStat::Directory) {
                // The forced write recursively deletes the whole live directory, so "any removal
                // lives below it" is too coarse: an untracked user file inside would be silently
                // destroyed. Enumerate the actual leaves and require every one of them to be
                // scheduled for removal. An empty directory is allowed — a merge that crashed
                // before writing its files legitimately leaves one behind, and removals only ever
                // name files.
                let mut leaves: Vec<String> = Vec::new();
                collect_leaf_entries(workspace, path, &mut leaves).map_err(|e| {
                    RevisionError::RecoveryRequired {
                        target_revision: intent.pre_merge_revision.clone(),
                        journal_path: self.merge_intent_path(),
                        reason: format!(
                            "cannot roll back the interrupted merge: '{}' could not be inspected ({}); remove it and retry",
                            path, e
                        ),
                    }
                })?;
                if let Some(unaccounted) = leaves
                    .iter()
                    .find(|leaf| !removals.iter().any(|removed| removed == *leaf))
                {
                    return Err(RevisionError::RecoveryRequired {
                        target_revision: intent.pre_merge_revision.clone(),
                        journal_path: self.merge_intent_path(),
                        reason: format!(
                            "cannot roll back the interrupted merge: restoring '{}' would recursively delete '{}', which the merge did not create; remove it and retry",
                            path, unaccounted
                        ),
                    });
                }
            }
        }
        // The structural checks above only cover shapes that make the rollback impossible or that
        // would destroy a directory tree. Content is the remaining hole: a regular file on a
        // merge-touched path may hold what the merge wrote, what the pre-merge revision recorded, or
        // work the user did while Kroma was down. Only the first two are this rollback's business.
        let unproven = self.merge_rollback_unproven_paths(
            workspace,
            &target,
            &intent,
            &applied_files,
            &writes,
            &removals,
        )?;
        if !unproven.is_empty() {
            return Err(RevisionError::RecoveryRequired {
                target_revision: intent.pre_merge_revision.clone(),
                journal_path: self.merge_intent_path(),
                reason: format!(
                    "cannot roll back the interrupted merge: {} path(s) hold content that is neither the pre-merge state nor what this merge recorded writing ({}); review them and retry",
                    unproven.len(),
                    unproven.join("; ")
                ),
            });
        }
        let mut operations: Vec<RestoreOp> = Vec::with_capacity(removals.len() + writes.len());
        // Rollback removals stay non-forced: a merge-created path a user has since replaced with a
        // directory/symlink is external work and must not be recursively deleted. The writes keep
        // `force` so the `file -> directory` shape is vacated and restored.
        for path in removals {
            operations.push(RestoreOp::Remove { path, force: false });
        }
        for (path, sha256, mode) in writes {
            operations.push(RestoreOp::Write { path, sha256, mode });
        }
        let restore_intent = RestoreIntent {
            version: RESTORE_INTENT_VERSION,
            target_revision: intent.pre_merge_revision.clone(),
            parent_revision: None,
            created_at_ms: now_ms(),
            force: true,
            operations,
        };
        write_atomic_bytes(
            &self.intent_path(),
            &serde_json::to_vec(&restore_intent)?,
            0o600,
        )?;
        sync_dir(&self.root)?;
        for op in &restore_intent.operations {
            self.apply_op(workspace, op, true)?;
        }
        self.clear_intent()?;
        self.clear_merge_intent()?;
        restored_files.sort();
        removed_files.sort();
        Ok(MergeRecoveryOutcome::RolledBack {
            pre_merge_revision: intent.pre_merge_revision,
            restored_files,
            removed_files,
        })
    }

    /// F08 follow-up: the paths whose live state this rollback cannot account for.
    ///
    /// Per touched path the rollback is authorised for exactly two live states:
    ///
    /// * the pre-merge state recorded by the pre-merge revision — nothing to undo, or already undone;
    /// * the state the merge itself left behind — the content recorded in `applied_files`, or absence
    ///   on a path the merge removed.
    ///
    /// A regular file matching neither may be work the user did while Kroma was down, and an absent
    /// path that the merge neither created nor removed has been deleted since. Both are returned as a
    /// description instead of being overwritten or deleted. Directories are left to the structural
    /// checks the caller already ran (they refuse to recursively delete anything the merge did not
    /// create), except for a symlink or special file on a path the rollback must write: that is
    /// neither a state the merge produced nor one it recorded, and the forced write would replace it.
    ///
    /// A journal written before `applied_files` existed proves only the pre-merge state, so every
    /// written path it already applied is reported here instead of being blindly rolled back. An
    /// applied entry that records content but no mode proves only that content, so it cannot accept
    /// a live file whose permissions the user changed; the same holds for the pre-merge state, which
    /// is compared with the same content-and-mode rule the restore conflict checks use.
    fn merge_rollback_unproven_paths(
        &self,
        workspace: &WorkspaceCap,
        target: &WorkspaceRevision,
        intent: &MergeIntent,
        applied_files: &[MergeAppliedFile],
        writes: &[(String, String, u32)],
        removals: &[String],
    ) -> Result<Vec<String>, RevisionError> {
        let applied: BTreeMap<&str, &MergeAppliedFile> = applied_files
            .iter()
            .map(|file| (file.path.as_str(), file))
            .collect();
        let rollback_write_paths: BTreeSet<&str> =
            writes.iter().map(|(path, _, _)| path.as_str()).collect();
        let mut unproven = Vec::new();
        for path in writes
            .iter()
            .map(|(path, _, _)| path.as_str())
            .chain(removals.iter().map(String::as_str))
        {
            let pre_merge = target.tracked_files.get(path);
            let applied_entry = applied.get(path).copied();
            // Whether the *merge* removed the path, which is not the same as the rollback restoring
            // it: a path the merge removed is written back from the pre-merge revision.
            let merge_removed_it = !intent.written_files.iter().any(|written| written == path)
                && intent.removed_files.iter().any(|removed| removed == path);
            match live_entry_state(workspace, path)? {
                LiveEntryState::Regular { sha256, mode, .. } => {
                    let live_mode = mode & 0o7777;
                    if pre_merge.is_some_and(|entry| {
                        entry.sha256 == sha256 && (entry.mode & 0o7777) == live_mode
                    }) || applied_entry.is_some_and(|entry| {
                        entry.sha256 == sha256
                            && entry.mode.is_some_and(|mode| (mode & 0o7777) == live_mode)
                    }) {
                        continue;
                    }
                    unproven.push(format!(
                        "'{}' holds content or permissions that are neither the pre-merge state nor what this merge recorded writing",
                        path
                    ));
                }
                LiveEntryState::Absent => {
                    if pre_merge.is_none() || merge_removed_it {
                        continue;
                    }
                    unproven.push(format!(
                        "'{}' is gone, but it existed before the merge and this merge did not record removing it",
                        path
                    ));
                }
                LiveEntryState::Directory => {}
                LiveEntryState::NonRegular => {
                    if rollback_write_paths.contains(path) {
                        unproven.push(format!(
                            "'{}' is a symlink or special file, which is neither the pre-merge state nor something this merge recorded writing",
                            path
                        ));
                    }
                }
            }
        }
        Ok(unproven)
    }

    /// F03/F08: the merged revision an interrupted worktree→parent merge captured, if the merge
    /// actually completed *and* the live workspace is still in that revision's state.
    ///
    /// The merged manifest is an *unlinked* capture (`wal_entry_id == None`) whose parent is the
    /// recorded pre-merge revision and whose tracked files include every path the merge wrote, so a
    /// matching revision is the merge's own capture. Any other revision shape (a WAL-linked capture,
    /// a different parent, or a manifest missing a written path) is not proof and is skipped.
    ///
    /// The manifest alone is not proof of completion: the merge's manifest can survive in the store
    /// while the workspace was reverted by hand (a post-merge gate rejection with
    /// `revert_on_verify_failure == false` keeps the merged revision, and `--undo`/`--force-restore`
    /// never remove it), after which a later merge can crash half-applied. Only a live workspace
    /// that matches the candidate revision for every path the merge touched is treated as completed;
    /// otherwise the caller rolls the half-merged workspace back.
    fn merge_intent_already_landed(
        &self,
        workspace: &WorkspaceCap,
        intent: &MergeIntent,
    ) -> Result<Option<RevisionId>, RevisionError> {
        // A merge that wrote and removed nothing has no observable state to prove it landed, so
        // treat it as not landed rather than matching any unlinked revision with the same parent.
        if intent.written_files.is_empty() && intent.removed_files.is_empty() {
            return Ok(None);
        }
        for revision in self.list_revisions()? {
            if revision.parent_revision_id.as_ref() != Some(&intent.pre_merge_revision)
                || revision.wal_entry_id.is_some()
                || !intent
                    .written_files
                    .iter()
                    .all(|path| revision.tracked_files.contains_key(path))
            {
                continue;
            }
            if self.workspace_matches_merged_revision(workspace, &revision, intent)? {
                return Ok(Some(revision.revision_id));
            }
        }
        Ok(None)
    }

    /// True when the live workspace holds exactly the state `revision` describes for every path the
    /// merge touched: each written path is a regular file whose content hash matches the manifest,
    /// and each removed path is absent. Any other shape (absent, directory, symlink, differing
    /// content) means the merge did not fully land and must not be treated as completed.
    fn workspace_matches_merged_revision(
        &self,
        workspace: &WorkspaceCap,
        revision: &WorkspaceRevision,
        intent: &MergeIntent,
    ) -> Result<bool, RevisionError> {
        for path in &intent.written_files {
            let Some(entry) = revision.tracked_files.get(path) else {
                return Ok(false);
            };
            match live_entry_state(workspace, path)? {
                LiveEntryState::Regular { sha256, .. } if sha256 == entry.sha256 => {}
                _ => return Ok(false),
            }
        }
        for path in &intent.removed_files {
            if !matches!(stat_live_entry(workspace, path)?, LiveEntryStat::Absent) {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Path of a content-addressed blob.
    ///
    /// `sha256` comes from an on-disk revision manifest, so it is untrusted input: it is validated
    /// as exactly 64 ASCII hex characters before it is sliced (`&sha256[..2]`) or joined into a
    /// path. A short/non-ASCII value can therefore no longer panic the process, and a value with
    /// path separators or `..` can no longer escape the `blobs/` subtree.
    pub fn blob_path(&self, sha256: &str) -> Result<PathBuf, RevisionError> {
        validate_blob_sha256(sha256)?;
        Ok(self.root.join(BLOB_DIR).join(&sha256[..2]).join(sha256))
    }

    pub fn revision_path(&self, id: &RevisionId) -> PathBuf {
        self.root
            .join(REVISION_DIR)
            .join(format!("{}.json", id.as_str()))
    }

    pub fn ensure_layout(&self) -> Result<(), RevisionError> {
        ensure_private_dir(&self.root)?;
        ensure_private_dir(&self.root.join(BLOB_DIR))?;
        ensure_private_dir(&self.root.join(REVISION_DIR))?;
        Ok(())
    }

    /// Capture the current workspace state as a new revision.
    /// Read the live tracked-content identity without rewriting any revision/WAL metadata.
    pub fn workspace_content_revision(
        &self,
        workspace: &WorkspaceCap,
    ) -> Result<RevisionId, RevisionError> {
        let (tracked, _) = self.scan_workspace_to_blobs(workspace)?;
        Ok(compute_revision_id(&tracked))
    }

    pub fn capture_revision(
        &self,
        workspace: &WorkspaceCap,
        parent_rev: Option<&RevisionId>,
        wal_entry_id: Option<&str>,
        turn_index: usize,
    ) -> Result<WorkspaceRevision, RevisionError> {
        self.ensure_layout()?;
        let mut tracked: BTreeMap<String, TrackedFile> = BTreeMap::new();
        let mut skipped: BTreeSet<String> = BTreeSet::new();
        let mut total_bytes = 0u64;
        self.scan_dir(
            workspace,
            Path::new("."),
            0,
            &mut tracked,
            &mut skipped,
            &mut total_bytes,
        )?;

        let revision_id = compute_revision_id(&tracked);
        if let Ok(existing) = self.load_revision(&revision_id) {
            // Never overwrite an already-persisted revision with a self-parent (`parent == id`)
            // or strip `wal_entry_id` when an unlinked intermediate capture sees the same tree.
            if parent_rev == Some(&revision_id)
                || (wal_entry_id.is_none() && existing.wal_entry_id.is_some())
            {
                return Ok(existing);
            }
        }

        let parent_files = match parent_rev {
            Some(id) => Some(self.load_revision(id)?.tracked_files),
            None => None,
        };
        let revision = WorkspaceRevision {
            revision_id,
            parent_revision_id: parent_rev.cloned(),
            wal_entry_id: wal_entry_id.map(|s| s.to_string()),
            turn_index,
            timestamp_ms: now_ms(),
            changed_paths: diff_paths(parent_files.as_ref(), &tracked),
            tracked_files: tracked,
            skipped_paths: skipped.into_iter().collect(),
            total_bytes,
        };
        // KRV4-01: the record is machine-written and machine-read, so it is machine-formatted. Pretty
        // printing spent roughly a third of the ceiling on whitespace no reader ever looks at.
        let encoded = serde_json::to_vec(&revision)?;
        // The writer enforces the reader's ceiling. Without this the store can produce a revision
        // that `load_revision` will refuse forever, and `list_revisions` would silently hide it.
        if encoded.len() as u64 > MAX_REVISION_RECORD_BYTES {
            return Err(RevisionError::RecordTooLarge {
                bytes: encoded.len() as u64,
                limit: MAX_REVISION_RECORD_BYTES,
            });
        }
        write_atomic_bytes(&self.revision_path(&revision.revision_id), &encoded, 0o600)?;
        Ok(revision)
    }

    /// Remove a temporary revision manifest that has not been bound to any WAL entry (used when
    /// a Gate 5 worker merge fails post-merge verification and auto-reverts).
    pub(crate) fn remove_unlinked_revision(&self, id: &RevisionId) -> Result<bool, RevisionError> {
        let Ok(rev) = self.load_revision(id) else {
            return Ok(false);
        };
        if rev.wal_entry_id.is_some() {
            return Ok(false);
        }
        match fs::remove_file(self.revision_path(id)) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    pub fn load_revision(&self, id: &RevisionId) -> Result<WorkspaceRevision, RevisionError> {
        let path = self.revision_path(id);
        let mut file = crate::secure_fs::open_existing_no_symlink(&path)
            .map_err(|_| RevisionError::UnknownRevision(id.as_str().to_string()))?;
        let bytes = crate::secure_fs::read_bounded_handle(
            &mut file,
            "revision record",
            MAX_REVISION_RECORD_BYTES,
            crate::secure_fs::Overflow::Refuse,
        )?
        .bytes;
        let revision: WorkspaceRevision = serde_json::from_slice(&bytes)?;
        if revision.revision_id != *id {
            return Err(RevisionError::InvalidStore(format!(
                "revision file {} does not carry its own id",
                id
            )));
        }
        Ok(revision)
    }

    pub fn list_revisions(&self) -> Result<Vec<WorkspaceRevision>, RevisionError> {
        let dir = self.root.join(REVISION_DIR);
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut revisions = Vec::new();
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().to_string();
            let Some(stem) = name.strip_suffix(".json") else {
                continue;
            };
            let Ok(id) = RevisionId::parse(stem) else {
                continue;
            };
            if let Ok(revision) = self.load_revision(&id) {
                revisions.push(revision);
            }
        }
        revisions.sort_by_key(|r| {
            (
                r.turn_index,
                r.timestamp_ms,
                r.revision_id.as_str().to_string(),
            )
        });
        Ok(revisions)
    }

    /// Verify that every revision manifest in the store is readable.
    ///
    /// [`Self::list_revisions`] deliberately skips a manifest it cannot read so one bad file cannot
    /// make read-only listing unusable. Reconciliation gates a state *mutation*, so it must not
    /// inherit that leniency: an unreadable manifest means the WAL can no longer be proven
    /// consistent with the recorded workspace history, and the caller has to surface that instead
    /// of proceeding as if recovery succeeded.
    pub fn check_integrity(&self) -> Result<(), RevisionError> {
        let dir = self.root.join(REVISION_DIR);
        if !dir.exists() {
            return Ok(());
        }
        let mut ids: Vec<RevisionId> = Vec::new();
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().to_string();
            let Some(stem) = name.strip_suffix(".json") else {
                continue;
            };
            if let Ok(id) = RevisionId::parse(stem) {
                ids.push(id);
            }
        }
        ids.sort();
        for id in ids {
            self.load_revision(&id).map_err(|e| {
                RevisionError::InvalidStore(format!(
                    "revision manifest {} is unreadable: {}",
                    id.as_str(),
                    e
                ))
            })?;
        }
        Ok(())
    }

    /// Read the unrecovered restore intent journal, if one is present.
    pub fn pending_restore(&self) -> Result<Option<RestoreIntent>, RevisionError> {
        let path = self.intent_path();
        let mut file = match crate::secure_fs::open_existing_no_symlink(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let bytes = crate::secure_fs::read_bounded_handle(
            &mut file,
            "restore intent journal",
            MAX_INTENT_BYTES,
            crate::secure_fs::Overflow::Refuse,
        )?
        .bytes;
        let intent: RestoreIntent = serde_json::from_slice(&bytes)?;
        if intent.version != RESTORE_INTENT_VERSION {
            return Err(RevisionError::InvalidStore(format!(
                "unsupported restore intent journal version {}",
                intent.version
            )));
        }
        Ok(Some(intent))
    }

    /// Restore the workspace to `target_rev`.
    ///
    /// With `force == false` and an `current_expected_rev` supplied, any live path that differs
    /// from the expected revision is reported as a [`RestoreResult::Conflict`] and nothing is
    /// mutated. External work is therefore never silently overwritten.
    pub fn restore_revision(
        &self,
        workspace: &WorkspaceCap,
        target_rev: &RevisionId,
        current_expected_rev: Option<&RevisionId>,
        force: bool,
    ) -> Result<RestoreResult, RevisionError> {
        self.restore_inner(workspace, target_rev, current_expected_rev, force, None)
    }

    /// `restore_revision` with a deterministic crash point injected, for fault-injection tests.
    pub fn restore_revision_with_fault(
        &self,
        workspace: &WorkspaceCap,
        target_rev: &RevisionId,
        current_expected_rev: Option<&RevisionId>,
        force: bool,
        fault: Option<RestoreFault>,
    ) -> Result<RestoreResult, RevisionError> {
        self.restore_inner(workspace, target_rev, current_expected_rev, force, fault)
    }

    /// Re-apply the operations of a pending restore intent and clear the journal.
    ///
    /// A crash between the durable intent and its last operation leaves the workspace
    /// half-restored, so recovery replays the recorded operations. Replay is only safe on a path
    /// that is still in one of the two states the intent accounts for: the target content (that
    /// operation already landed) or the parent revision's content (it had not). Any other live
    /// state was written while Kroma was down, so it is reported as [`RestoreResult::Conflict`]
    /// with the journal kept instead of being silently overwritten (§8.6, INV-09, INV-10). When the
    /// intent cannot prove a path's pre-restore state at all (no recorded parent, or its manifest is
    /// gone), that unknown is still not permission to write: only paths that are absent or already
    /// hold the target bytes are replayed. Use [`Self::recover_pending_restore_with_force`] to
    /// overwrite them deliberately.
    pub fn recover_pending_restore(
        &self,
        workspace: &WorkspaceCap,
    ) -> Result<RestoreResult, RevisionError> {
        self.recover_pending_restore_with_force(workspace, false)
    }

    /// [`Self::recover_pending_restore`] with this call's `force` acknowledgement: `true` replays
    /// the journal over paths whose live state the journal cannot account for, exactly as an
    /// explicit forced restore overwrites them. `false` refuses them and keeps the journal.
    ///
    /// `force` here is the current caller's decision, not the intent's recorded `force`: that flag
    /// described how the original operation was meant to apply and is never treated as standing
    /// authorization for work that appeared while Kroma was down.
    pub fn recover_pending_restore_with_force(
        &self,
        workspace: &WorkspaceCap,
        force: bool,
    ) -> Result<RestoreResult, RevisionError> {
        self.ensure_layout()?;
        let Some(intent) = self.pending_restore()? else {
            return Err(RevisionError::InvalidStore(
                "no pending restore intent to recover".to_string(),
            ));
        };
        if !force {
            if let Some(conflicting_paths) = self.restore_recovery_conflicts(workspace, &intent)? {
                return Ok(RestoreResult::Conflict {
                    target_revision: intent.target_revision,
                    conflicting_paths,
                });
            }
        }
        let replay_force = intent.force || force;
        let mut restored_files = Vec::new();
        let mut removed_files = Vec::new();
        for op in &intent.operations {
            self.apply_op(workspace, op, replay_force)?;
            match op {
                RestoreOp::Write { path, .. } => restored_files.push(path.clone()),
                RestoreOp::Remove { path, .. } => removed_files.push(path.clone()),
            }
        }
        self.clear_intent()?;
        restored_files.sort();
        removed_files.sort();
        Ok(RestoreResult::Applied {
            restored_revision: intent.target_revision,
            restored_files,
            removed_files,
        })
    }

    /// Live paths a pending restore may not replay over, or `None` when the journal is still
    /// replayable as recorded.
    ///
    /// The pre-restore content of a path is known only through the intent's `parent_revision`. When
    /// the intent recorded none (a legacy journal, or a restore started without a known expected
    /// revision) or that manifest is no longer in the store, the pre-state is **unknown** — which is
    /// not permission to overwrite. Only operations with per-path evidence are replayed: the path is
    /// absent, it already holds the target bytes, or the removal is the documented no-op on a
    /// directory/symlink. Anything else is reported and the journal kept, so an explicit `force` on
    /// the current call is the only thing that authorizes overwriting it. The intent's own `force`
    /// described the operation the user originally asked for; it is not a standing authorization for
    /// changes that appeared after the crash.
    fn restore_recovery_conflicts(
        &self,
        workspace: &WorkspaceCap,
        intent: &RestoreIntent,
    ) -> Result<Option<Vec<RevisionConflict>>, RevisionError> {
        // `None` means the pre-restore content is unknown; it is never read as "unchanged".
        let parent = match intent.parent_revision.as_ref() {
            Some(parent_id) => self.load_revision(parent_id).ok(),
            None => None,
        };
        let mut conflicts = Vec::new();
        for op in &intent.operations {
            let path = op.path();
            let live = live_entry_state(workspace, path)?;
            let parent_sha256 = parent
                .as_ref()
                .and_then(|parent| parent.tracked_files.get(path))
                .map(|entry| entry.sha256.as_str());
            let conflict = match op {
                RestoreOp::Write { sha256, .. } => match &live {
                    // The restore creates the path, or its write already landed.
                    LiveEntryState::Absent => None,
                    LiveEntryState::Regular {
                        sha256: live_sha256,
                        ..
                    } if live_sha256 == sha256 || Some(live_sha256.as_str()) == parent_sha256 => {
                        None
                    }
                    LiveEntryState::Regular {
                        sha256: live_sha256,
                        ..
                    } => Some(RevisionConflict {
                        path: path.to_string(),
                        kind: ConflictKind::ModifiedExternally,
                        expected_sha256: Some(sha256.clone()),
                        live_sha256: Some(live_sha256.clone()),
                    }),
                    // A write would replace the entry, recursively for a directory.
                    LiveEntryState::Directory | LiveEntryState::NonRegular => {
                        Some(RevisionConflict {
                            path: path.to_string(),
                            kind: ConflictKind::ModifiedExternally,
                            expected_sha256: Some(sha256.clone()),
                            live_sha256: None,
                        })
                    }
                },
                RestoreOp::Remove {
                    force: remove_force,
                    ..
                } => match &live {
                    LiveEntryState::Absent => None,
                    // Still the content the parent revision recorded: this is the removal the
                    // intent already scheduled.
                    LiveEntryState::Regular { sha256, .. }
                        if Some(sha256.as_str()) == parent_sha256 =>
                    {
                        None
                    }
                    // A non-forced removal is a no-op on a directory/symlink, so it destroys
                    // nothing; a forced one would delete it.
                    LiveEntryState::Directory | LiveEntryState::NonRegular
                        if !(*remove_force && intent.force) =>
                    {
                        None
                    }
                    LiveEntryState::Regular {
                        sha256: live_sha256,
                        ..
                    } => Some(RevisionConflict {
                        path: path.to_string(),
                        kind: ConflictKind::ModifiedExternally,
                        expected_sha256: parent_sha256.map(str::to_string),
                        live_sha256: Some(live_sha256.clone()),
                    }),
                    LiveEntryState::Directory | LiveEntryState::NonRegular => {
                        Some(RevisionConflict {
                            path: path.to_string(),
                            kind: ConflictKind::ModifiedExternally,
                            expected_sha256: parent_sha256.map(str::to_string),
                            live_sha256: None,
                        })
                    }
                },
            };
            if let Some(conflict) = conflict {
                conflicts.push(conflict);
            }
        }
        if conflicts.is_empty() {
            return Ok(None);
        }
        conflicts.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(Some(conflicts))
    }

    /// Transactional, workspace-bound fork: move the session's active leaf to `turn` **and** restore
    /// the workspace to the revision that was current at that turn.
    ///
    /// The leaf is only re-pointed after the restore has fully applied, so a refused or interrupted
    /// fork never leaves the WAL describing a workspace that was not actually restored. Live edits
    /// that differ from the expected revision are reported as [`WorkspaceBoundForkError::Conflict`]
    /// and nothing is mutated unless `force` is set. The pre-fork revision stays in the
    /// content-addressed store as the backup of the abandoned branch, and the WAL keeps every
    /// abandoned transcript entry, so a fork is non-destructive in both directions.
    pub fn fork_workspace_at_turn(
        &self,
        workspace: &WorkspaceCap,
        journal: &mut SessionJournal,
        turn: usize,
        force: bool,
    ) -> Result<WorkspaceBoundForkOutcome, WorkspaceBoundForkError> {
        self.fork_workspace_at_turn_inner(workspace, journal, turn, force, None)
    }

    /// `fork_workspace_at_turn` with a deterministic crash point injected, for fault-injection
    /// tests of the durable fork intent (P1-2).
    pub fn fork_workspace_at_turn_with_fault(
        &self,
        workspace: &WorkspaceCap,
        journal: &mut SessionJournal,
        turn: usize,
        force: bool,
        fault: Option<ForkFault>,
    ) -> Result<WorkspaceBoundForkOutcome, WorkspaceBoundForkError> {
        self.fork_workspace_at_turn_inner(workspace, journal, turn, force, fault)
    }

    fn fork_workspace_at_turn_inner(
        &self,
        workspace: &WorkspaceCap,
        journal: &mut SessionJournal,
        turn: usize,
        force: bool,
        fault: Option<ForkFault>,
    ) -> Result<WorkspaceBoundForkOutcome, WorkspaceBoundForkError> {
        // Validate the transcript target before touching a single file: a fork that cannot move the
        // leaf must not restore the workspace on its way to failing.
        if journal.entry_id_at_or_before_turn(turn).is_none() {
            return Err(WorkspaceBoundForkError::NoEntryAtTurn(turn));
        }
        let target_record = journal
            .revision_at_or_before_turn(turn)
            .ok_or(WorkspaceBoundForkError::NoRevisionAtTurn(turn))?;
        let target = RevisionId::parse(&target_record.revision_id)?;
        self.rewind_workspace_to_revision(workspace, journal, &target, turn, force, fault)
    }

    /// Restore the workspace to `target` **and** move the active leaf to `turn` as one
    /// crash-atomic operation, for `--undo`.
    ///
    /// `--undo` needs exactly the guarantee [`Self::fork_workspace_at_turn`] gives `--fork`: the
    /// durable [`ForkIntent`] is written before the restore touches a file and cleared only after
    /// the leaf moved and the restored revision was recorded, so a crash between the restore
    /// clearing its own journal and the leaf move is completed by [`Self::recover_pending_fork`] on
    /// the next start. Without it the WAL keeps naming a revision the workspace is no longer on and
    /// the default retry refuses with a misleading "changed outside the kroma" conflict.
    pub fn restore_and_rewind_workspace(
        &self,
        workspace: &WorkspaceCap,
        journal: &mut SessionJournal,
        target: &RevisionId,
        turn: usize,
        force: bool,
    ) -> Result<WorkspaceBoundForkOutcome, WorkspaceBoundForkError> {
        self.restore_and_rewind_workspace_with_fault(workspace, journal, target, turn, force, None)
    }

    /// `restore_and_rewind_workspace` with a deterministic crash point injected, for
    /// fault-injection tests of the durable rewind intent.
    pub fn restore_and_rewind_workspace_with_fault(
        &self,
        workspace: &WorkspaceCap,
        journal: &mut SessionJournal,
        target: &RevisionId,
        turn: usize,
        force: bool,
        fault: Option<ForkFault>,
    ) -> Result<WorkspaceBoundForkOutcome, WorkspaceBoundForkError> {
        if journal.entry_id_at_or_before_turn(turn).is_none() {
            return Err(WorkspaceBoundForkError::NoEntryAtTurn(turn));
        }
        self.rewind_workspace_to_revision(workspace, journal, target, turn, force, fault)
    }

    /// The shared body of a workspace-bound fork and an `--undo` rewind: restore `target`, then
    /// move the leaf to `turn`, guarded by one durable [`ForkIntent`].
    fn rewind_workspace_to_revision(
        &self,
        workspace: &WorkspaceCap,
        journal: &mut SessionJournal,
        target: &RevisionId,
        turn: usize,
        force: bool,
        fault: Option<ForkFault>,
    ) -> Result<WorkspaceBoundForkOutcome, WorkspaceBoundForkError> {
        let expected = journal
            .latest_revision_id()
            .and_then(|id| RevisionId::parse(&id).ok());

        // Refuse *before* writing a fork intent when a restore is already pending: its target may be
        // a different revision, and recovering the wrong restore would corrupt the workspace. This
        // also guarantees that a live fork intent always belongs to this fork's restore.
        if let Some(intent) = self.pending_restore()? {
            return Err(WorkspaceBoundForkError::RecoveryRequired {
                turn,
                target_revision: intent.target_revision,
                journal_path: self.intent_path(),
                reason: "an unrecovered restore intent journal is present; run recover_pending_restore before forking".to_string(),
            });
        }

        // F03 follow-up (P1-2): a durable fork intent written before the restore touches any file.
        // A crash after the restore cleared its own journal but before the leaf moved is then
        // completed by [`Self::recover_pending_fork`] instead of being silently lost.
        self.write_fork_intent(&ForkIntent {
            version: FORK_INTENT_VERSION,
            session_id: journal.session_id().to_string(),
            workspace_root: workspace.root().to_string_lossy().to_string(),
            forked_at_turn: turn,
            target_revision: target.clone(),
            expected_revision: expected.clone(),
            created_at_ms: now_ms(),
        })?;

        // Phase 1: restore the workspace. `restore_revision` journals its own intent, so a crash
        // here is recoverable by `recover_pending_restore`, and refuses external modifications
        // unless `force` is set.
        let (restored_revision, restored_files, removed_files) =
            match self.restore_revision(workspace, target, expected.as_ref(), force)? {
                RestoreResult::Applied {
                    restored_revision,
                    restored_files,
                    removed_files,
                } => (restored_revision, restored_files, removed_files),
                RestoreResult::Conflict {
                    target_revision,
                    conflicting_paths,
                } => {
                    self.clear_fork_intent()?;
                    return Err(WorkspaceBoundForkError::Conflict {
                        turn,
                        target_revision,
                        conflicting_paths,
                    });
                }
                RestoreResult::RecoveryRequired {
                    target_revision,
                    journal_path,
                    reason,
                } => {
                    self.clear_fork_intent()?;
                    return Err(WorkspaceBoundForkError::RecoveryRequired {
                        turn,
                        target_revision,
                        journal_path,
                        reason,
                    });
                }
            };

        // Phase 2: only now move the session leaf onto the forked branch.
        if matches!(fault, Some(ForkFault::AfterRestoreBeforeLeafMove)) {
            return Err(WorkspaceBoundForkError::Revision(
                RevisionError::InvalidStore(
                    "fault injection: crashed after the restore applied, before the leaf moved"
                        .to_string(),
                ),
            ));
        }
        let leaf_id = journal.fork_at_turn(turn)?;
        if journal.latest_revision_id().as_deref() != Some(restored_revision.as_str()) {
            record_restored_revision(
                journal,
                self,
                &restored_revision,
                &restored_files,
                &removed_files,
                expected.as_ref(),
            )?;
        }
        // The fork is durably complete; drop the intent so the next entry point treats the session
        // as consistently forked.
        self.clear_fork_intent()?;
        Ok(WorkspaceBoundForkOutcome {
            leaf_id,
            forked_at_turn: turn,
            target_revision: restored_revision,
            restored_files,
            removed_files,
            turn_count: journal.max_active_turn(),
            latest_revision_id: journal.latest_revision_id(),
        })
    }

    fn restore_inner(
        &self,
        workspace: &WorkspaceCap,
        target_rev: &RevisionId,
        current_expected_rev: Option<&RevisionId>,
        force: bool,
        fault: Option<RestoreFault>,
    ) -> Result<RestoreResult, RevisionError> {
        self.ensure_layout()?;
        let target = self.load_revision(target_rev)?;

        if let Some(intent) = self.pending_restore()? {
            return Ok(RestoreResult::RecoveryRequired {
                target_revision: intent.target_revision,
                journal_path: self.intent_path(),
                reason: "an unrecovered restore intent journal is present; run recover_pending_restore before starting another restore".to_string(),
            });
        }

        let expected = match current_expected_rev {
            Some(id) => Some(self.load_revision(id)?),
            None => None,
        };

        let mut affected: BTreeSet<String> = target.tracked_files.keys().cloned().collect();
        if let Some(exp) = expected.as_ref() {
            affected.extend(exp.tracked_files.keys().cloned());
        }
        // Paths the target revision deliberately skipped (oversized, symlink, special, sensitive)
        // carry no recorded content, so they can neither conflict with the target nor be deleted
        // in its name.
        let target_skipped: BTreeSet<&str> =
            target.skipped_paths.iter().map(|s| s.as_str()).collect();

        if !force {
            let mut conflicts = Vec::new();
            for path in &affected {
                if target_skipped.contains(path.as_str()) {
                    continue;
                }
                let live = live_entry_state(workspace, path)?;
                let conflict = match expected.as_ref() {
                    Some(exp) => classify_conflict_against_expected(exp, &target, path, &live),
                    None => classify_conflict_against_target(&target, path, &live),
                };
                if let Some((kind, expected_sha256, live_sha256)) = conflict {
                    conflicts.push(RevisionConflict {
                        path: path.clone(),
                        kind,
                        expected_sha256,
                        live_sha256,
                    });
                }
            }
            if !conflicts.is_empty() {
                conflicts.sort_by(|a, b| a.path.cmp(&b.path));
                return Ok(RestoreResult::Conflict {
                    target_revision: target.revision_id.clone(),
                    conflicting_paths: conflicts,
                });
            }
        }

        // Every removal is scheduled before every write, deepest path first, so a path whose kind
        // changed between revisions (file <-> directory) is vacated before its replacement lands.
        let mut removals: Vec<String> = Vec::new();
        for path in &affected {
            if target.tracked_files.contains_key(path) || target_skipped.contains(path.as_str()) {
                continue;
            }
            if !matches!(stat_live_entry(workspace, path)?, LiveEntryStat::Absent) {
                removals.push(path.clone());
            }
        }
        removals.sort_by(|a, b| {
            let depth = |s: &String| s.matches('/').count();
            depth(b).cmp(&depth(a)).then_with(|| b.cmp(a))
        });

        let mut operations: Vec<RestoreOp> = removals
            .into_iter()
            .map(|path| RestoreOp::Remove { path, force })
            .collect();
        for (path, entry) in &target.tracked_files {
            operations.push(RestoreOp::Write {
                path: path.clone(),
                sha256: entry.sha256.clone(),
                mode: entry.mode,
            });
        }

        let intent = RestoreIntent {
            version: RESTORE_INTENT_VERSION,
            target_revision: target.revision_id.clone(),
            parent_revision: current_expected_rev.cloned(),
            created_at_ms: now_ms(),
            force,
            operations,
        };
        write_atomic_bytes(&self.intent_path(), &serde_json::to_vec(&intent)?, 0o600)?;
        sync_dir(&self.root)?;

        if matches!(fault, Some(RestoreFault::AfterIntentWritten)) {
            return Ok(RestoreResult::RecoveryRequired {
                target_revision: intent.target_revision.clone(),
                journal_path: self.intent_path(),
                reason: "fault injection: crashed after the restore intent journal was written"
                    .to_string(),
            });
        }

        let mut restored_files = Vec::new();
        let mut removed_files = Vec::new();
        {
            // Test-only: arm the directory-sync fault for the mutation loop only (the intent above
            // is already durable), so a failed mutation sync must retain the intent while the
            // final intent clear is never perturbed. The guard disarms when the loop exits.
            #[cfg(all(test, unix))]
            let _dir_sync_fault = if matches!(fault, Some(RestoreFault::DirectorySyncFails)) {
                Some(crate::secure_fs::DirSyncFaultGuard::arm())
            } else {
                None
            };
            for (index, op) in intent.operations.iter().enumerate() {
                if let Err(e) = self.apply_op(workspace, op, force) {
                    return Ok(RestoreResult::RecoveryRequired {
                        target_revision: intent.target_revision.clone(),
                        journal_path: self.intent_path(),
                        reason: format!(
                            "restore failed while applying '{}' (operation {} of {}): {}",
                            op.path(),
                            index + 1,
                            intent.operations.len(),
                            e
                        ),
                    });
                }
                match op {
                    RestoreOp::Write { path, .. } => restored_files.push(path.clone()),
                    RestoreOp::Remove { path, .. } => removed_files.push(path.clone()),
                }
                if let Some(RestoreFault::AfterAppliedFiles(n)) = fault {
                    if index + 1 >= n {
                        return Ok(RestoreResult::RecoveryRequired {
                            target_revision: intent.target_revision.clone(),
                            journal_path: self.intent_path(),
                            reason: format!(
                                "fault injection: crashed after applying {} of {} operations",
                                index + 1,
                                intent.operations.len()
                            ),
                        });
                    }
                }
            }
        }

        self.clear_intent()?;
        restored_files.sort();
        removed_files.sort();
        Ok(RestoreResult::Applied {
            restored_revision: intent.target_revision.clone(),
            restored_files,
            removed_files,
        })
    }

    fn apply_op(
        &self,
        workspace: &WorkspaceCap,
        op: &RestoreOp,
        force: bool,
    ) -> Result<(), RevisionError> {
        match op {
            RestoreOp::Write { path, sha256, mode } => {
                let bytes = self.read_blob(sha256)?;
                write_tracked_file(workspace, path, &bytes, *mode, force)
            }
            // A removal is forced only when both the intent and the operation agree. The per-op
            // flag keeps a merge rollback's removal non-forced even though the surrounding intent
            // restores forced writes, so it never recursively deletes a directory/symlink a user
            // parked on a merge-created path. The intent flag keeps a *legacy* journal (written
            // before the per-op flag existed, and whose absent flag now defaults to `true`) honest:
            // an unforced restore must not have its removals silently upgraded to forced just
            // because the missing per-op field defaulted to `true`.
            RestoreOp::Remove {
                path,
                force: remove_force,
            } => {
                remove_tracked_file(workspace, path, *remove_force && force)?;
                Ok(())
            }
        }
    }

    fn clear_intent(&self) -> Result<(), RevisionError> {
        match fs::remove_file(self.intent_path()) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        sync_dir(&self.root)
    }

    /// Store a content-addressed blob crash-safely (O2).
    ///
    /// The bytes are written to a sibling temporary file, `fsync`ed, and then atomically renamed
    /// onto the CAS path. A crash or `ENOSPC` mid-write therefore leaves at most a stray temporary
    /// file, never a truncated blob that would poison every future revision that references it.
    fn store_blob(&self, sha256: &str, bytes: &[u8]) -> Result<(), RevisionError> {
        let path = self.blob_path(sha256)?;
        let dir = path.parent().ok_or_else(|| {
            RevisionError::InvalidStore("blob path has no parent directory".to_string())
        })?;
        ensure_private_dir(dir)?;

        // Idempotent fast path: a completed blob of the right size is already durable.
        match fs::symlink_metadata(&path) {
            Ok(meta) => {
                if meta.file_type().is_symlink() || !meta.file_type().is_file() {
                    return Err(RevisionError::InvalidStore(format!(
                        "blob {} exists but is not a regular file",
                        sha256
                    )));
                }
                if meta.len() != bytes.len() as u64 {
                    return Err(RevisionError::InvalidStore(format!(
                        "blob {} already exists with a different size",
                        sha256
                    )));
                }
                return Ok(());
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }

        let tmp_path = dir.join(format!(".{}.tmp-{}", sha256, Uuid::new_v4()));
        let write_result = (|| -> Result<(), RevisionError> {
            let mut file = crate::secure_fs::create_new_no_symlink(&tmp_path, 0o600)?;
            file.write_all(bytes)?;
            file.sync_all()?;
            Ok(())
        })();
        if let Err(e) = write_result {
            let _ = fs::remove_file(&tmp_path);
            return Err(e);
        }
        if let Err(e) = fs::rename(&tmp_path, &path) {
            let _ = fs::remove_file(&tmp_path);
            return Err(e.into());
        }
        sync_dir(dir)
    }

    /// Sorted paths whose tracked content or mode differs between two revisions (O6).
    pub fn compare(&self, a: &RevisionId, b: &RevisionId) -> Result<Vec<String>, RevisionError> {
        let rev_a = self.load_revision(a)?;
        let rev_b = self.load_revision(b)?;
        let mut paths: BTreeSet<String> = rev_a.tracked_files.keys().cloned().collect();
        paths.extend(rev_b.tracked_files.keys().cloned());
        let mut differing = Vec::new();
        for path in paths {
            let left = rev_a.tracked_files.get(&path);
            let right = rev_b.tracked_files.get(&path);
            if left != right {
                differing.push(path);
            }
        }
        Ok(differing)
    }

    /// Newest revision manifest in the store, ordered by turn then capture time (O6).
    pub fn newest_revision(&self) -> Result<Option<WorkspaceRevision>, RevisionError> {
        Ok(self.list_revisions()?.pop())
    }

    /// Adopt a snapshot manifest that was durably written immediately before a crash that stopped
    /// the WAL `RevisionCaptured` append (O6).
    ///
    /// Only a manifest bound to the *current* WAL leaf is eligible. Picking the global max
    /// `turn_index` instead would let another session — or an abandoned branch sharing the same
    /// state directory — that ever reached a higher turn shadow this branch's unrecorded capture.
    /// Manifests that are already recorded are ignored, and an undone/abandoned revision is still
    /// recorded while its manifest is bound to a leaf that is no longer current, so this can never
    /// resurrect a revision that `--undo` / `--fork` moved away from.
    ///
    /// Returns the adopted revision id when a manifest was reconciled.
    pub fn reconcile_with_wal(
        &self,
        journal: &mut SessionJournal,
    ) -> Result<Option<String>, RevisionError> {
        // F10: a corrupt manifest must fail reconciliation (and therefore block the next mutation)
        // instead of being silently skipped as an unrecorded capture.
        self.check_integrity()?;
        let Some(leaf) = journal.leaf_id().map(|id| id.to_string()) else {
            return Ok(None);
        };
        let recorded = journal.revision_records();
        let candidate = self
            .list_revisions()?
            .into_iter()
            .filter(|revision| revision.wal_entry_id.as_deref() == Some(leaf.as_str()))
            .filter(|revision| {
                !recorded
                    .iter()
                    .any(|r| r.revision_id == revision.revision_id.as_str())
            })
            .filter(|revision| {
                recorded
                    .last()
                    .is_none_or(|last| revision.turn_index >= last.turn)
            })
            .max_by_key(|revision| (revision.turn_index, revision.timestamp_ms));
        let Some(newest) = candidate else {
            return Ok(None);
        };
        let turn = journal.max_active_turn();
        let parent = recorded.last().map(|r| r.revision_id.clone());
        journal
            .record_revision_capture(
                turn,
                newest.revision_id.as_str(),
                parent.as_deref(),
                newest.changed_paths.clone(),
                newest.tracked_files.len(),
            )
            .map_err(|e| {
                RevisionError::InvalidStore(format!(
                    "failed to record the reconciled revision in the session WAL: {}",
                    e
                ))
            })?;
        Ok(Some(newest.revision_id.as_str().to_string()))
    }

    /// Scan a workspace (such as an isolated worker worktree) and store its tracked file blobs in
    /// the content-addressed blob store without writing a parent `revisions/r-*.json` manifest.
    pub fn scan_workspace_to_blobs(
        &self,
        workspace: &WorkspaceCap,
    ) -> Result<(BTreeMap<String, TrackedFile>, Vec<String>), RevisionError> {
        self.ensure_layout()?;
        let mut tracked: BTreeMap<String, TrackedFile> = BTreeMap::new();
        let mut skipped: BTreeSet<String> = BTreeSet::new();
        let mut total_bytes = 0u64;
        self.scan_dir(
            workspace,
            Path::new("."),
            0,
            &mut tracked,
            &mut skipped,
            &mut total_bytes,
        )?;
        Ok((tracked, skipped.into_iter().collect()))
    }

    pub fn read_blob(&self, sha256: &str) -> Result<Vec<u8>, RevisionError> {
        let path = self.blob_path(sha256)?;
        let mut file = crate::secure_fs::open_existing_no_symlink(&path).map_err(|e| {
            RevisionError::InvalidStore(format!("cannot read snapshot blob {}: {}", sha256, e))
        })?;
        // T06 (KR-06): a blob is content-addressed, so a read longer than the per-file snapshot
        // ceiling can only be corruption; refusing keeps the CAS from allocating an arbitrary size.
        let bytes = crate::secure_fs::read_bounded_handle(
            &mut file,
            &format!("snapshot blob {}", sha256),
            MAX_SNAPSHOT_FILE_BYTES,
            crate::secure_fs::Overflow::Refuse,
        )?
        .bytes;
        if sha256_hex64(&bytes) != sha256 {
            return Err(RevisionError::InvalidStore(format!(
                "snapshot blob {} is corrupt (content hash mismatch)",
                sha256
            )));
        }
        Ok(bytes)
    }

    #[allow(clippy::too_many_arguments)]
    fn scan_dir(
        &self,
        workspace: &WorkspaceCap,
        rel: &Path,
        depth: usize,
        tracked: &mut BTreeMap<String, TrackedFile>,
        skipped: &mut BTreeSet<String>,
        total_bytes: &mut u64,
    ) -> Result<(), RevisionError> {
        if depth > MAX_SNAPSHOT_DEPTH {
            return Ok(());
        }
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        let (_, entries) = match workspace.read_dir_no_symlink(&rel_str) {
            Ok(v) => v,
            Err(WorkspaceError::SensitivePath { .. }) => return Ok(()),
            Err(e) => return Err(e.into()),
        };

        for entry in entries {
            let child = if rel_str == "." {
                PathBuf::from(&entry.name)
            } else {
                rel.join(&entry.name)
            };
            let child_str = child.to_string_lossy().replace('\\', "/");

            if entry.is_symlink {
                skipped.insert(child_str);
                continue;
            }
            if entry.is_dir {
                if SNAPSHOT_EXCLUDED_DIRS.contains(&entry.name.as_str()) {
                    skipped.insert(child_str);
                    continue;
                }
                self.scan_dir(workspace, &child, depth + 1, tracked, skipped, total_bytes)?;
                continue;
            }
            if !entry.is_file {
                skipped.insert(child_str);
                continue;
            }
            if SensitivePathGuard::is_sensitive(&child) {
                skipped.insert(child_str);
                continue;
            }
            let (_, mut file) = match workspace.open_existing_no_symlink(&child_str) {
                Ok(v) => v,
                Err(_) => {
                    skipped.insert(child_str);
                    continue;
                }
            };
            let meta = file.metadata()?;
            if !meta.file_type().is_file() {
                skipped.insert(child_str);
                continue;
            }
            let size = meta.len();
            if size > MAX_SNAPSHOT_FILE_BYTES
                || total_bytes.saturating_add(size) > MAX_SNAPSHOT_TOTAL_BYTES
            {
                skipped.insert(child_str);
                continue;
            }
            // T06 (KR-06): `size` came from a `stat` before the read; the read itself is bounded to
            // it, so a file that grows in between is refused (and skipped) instead of captured torn.
            let Ok(read) = crate::secure_fs::read_bounded_handle(
                &mut file,
                &child_str,
                size,
                crate::secure_fs::Overflow::Refuse,
            ) else {
                skipped.insert(child_str);
                continue;
            };
            let bytes = read.bytes;
            if bytes.len() as u64 != size {
                // The file changed size mid-read; never record a torn snapshot.
                skipped.insert(child_str);
                continue;
            }
            let sha256 = sha256_hex64(&bytes);
            self.store_blob(&sha256, &bytes)?;
            *total_bytes += size;
            #[cfg(unix)]
            let mode = meta.mode() & 0o7777;
            #[cfg(windows)]
            let mode = if meta.permissions().readonly() {
                0o444
            } else {
                0o644
            };
            tracked.insert(child_str, TrackedFile { sha256, mode, size });
        }
        Ok(())
    }
}

/// Re-bind `restored_revision` as the active branch's current workspace revision in the session
/// WAL, so `latest_revision_id()` keeps naming the bytes actually on disk after a fork/restore.
///
/// Shared by the RPC fork, the CLI `--fork` path and the RPC `revision.restore`/`undo` handlers so
/// every entry point stamps the restored revision the same way.
pub fn record_restored_revision(
    journal: &mut SessionJournal,
    store: &SnapshotStore,
    restored_revision: &RevisionId,
    restored_files: &[String],
    removed_files: &[String],
    expected: Option<&RevisionId>,
) -> Result<(), SessionError> {
    let (changed_paths, tracked_file_count) = match store.load_revision(restored_revision) {
        Ok(manifest) => (manifest.changed_paths.clone(), manifest.tracked_files.len()),
        Err(_) => {
            let mut paths: Vec<String> = restored_files
                .iter()
                .chain(removed_files.iter())
                .cloned()
                .collect();
            paths.sort();
            paths.dedup();
            (paths, restored_files.len())
        }
    };
    let turn = journal.max_active_turn();
    journal.record_revision_capture(
        turn,
        restored_revision.as_str(),
        expected.map(|e| e.as_str()),
        changed_paths,
        tracked_file_count,
    )
}

/// Reject any content hash that is not exactly 64 ASCII hex characters.
///
/// [`SnapshotStore::blob_path`] slices this string (`&sha256[..2]`) and joins it into a filesystem
/// path, so a corrupt/hostile revision manifest could otherwise panic (short or non-ASCII value) or
/// point outside the blob store (separators or `..`). Store corruption is reported as
/// [`RevisionError::InvalidStore`] everywhere else, so it is reported as an error here too.
fn validate_blob_sha256(sha256: &str) -> Result<(), RevisionError> {
    if sha256.len() == 64 && sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(RevisionError::InvalidStore(format!(
            "invalid snapshot blob hash '{}': expected 64 ASCII hex characters",
            sha256
        )))
    }
}
impl From<crate::secure_fs::SecureFsError> for RevisionError {
    fn from(err: crate::secure_fs::SecureFsError) -> Self {
        match err {
            crate::secure_fs::SecureFsError::InvalidPath(reason) => {
                RevisionError::InvalidStore(reason)
            }
            crate::secure_fs::SecureFsError::Io(io) => RevisionError::Io(io),
            crate::secure_fs::SecureFsError::NotRegularFile(display) => {
                RevisionError::InvalidStore(format!("{} is not a regular file", display))
            }
            crate::secure_fs::SecureFsError::TooLarge { display, limit } => {
                RevisionError::InvalidStore(format!(
                    "{} is larger than the {} byte limit",
                    display, limit
                ))
            }
            crate::secure_fs::SecureFsError::NotUtf8 { display } => {
                RevisionError::InvalidStore(format!("{} is not valid UTF-8", display))
            }
        }
    }
}

/// Create (if needed) and lock down a private store directory (O3).
///
/// The hardening walk itself lives in [`crate::secure_fs::ensure_private_dir`] so the policy exists
/// exactly once; this wrapper only maps the shared error type onto [`RevisionError`].
pub(crate) fn ensure_private_dir(dir: &Path) -> Result<(), RevisionError> {
    crate::secure_fs::ensure_private_dir(dir).map_err(RevisionError::from)
}

/// [`ensure_private_dir`] for callers that only speak `std::io::Error` (e.g. the session WAL
/// store). A [`RevisionError::Io`] is returned unchanged so callers keep the OS error kind; any
/// other store error is folded into an `io::Error` with the same message.
#[cfg(unix)]
pub(crate) fn ensure_private_dir_io(dir: &Path) -> std::io::Result<()> {
    crate::secure_fs::ensure_private_dir_io(dir)
}

fn write_atomic_bytes(path: &Path, bytes: &[u8], mode: u32) -> Result<(), RevisionError> {
    let dir = path.parent().ok_or_else(|| {
        RevisionError::InvalidStore("atomic write target has no parent directory".to_string())
    })?;
    let name = path.file_name().ok_or_else(|| {
        RevisionError::InvalidStore("atomic write target has no file name".to_string())
    })?;
    let mut tmp_name = name.to_os_string();
    tmp_name.push(format!(".tmp-{}", Uuid::new_v4()));
    let tmp_path = dir.join(&tmp_name);
    let write_result = (|| -> Result<(), RevisionError> {
        let mut file = crate::secure_fs::create_new_no_symlink(&tmp_path, mode)?;
        file.write_all(bytes)?;
        #[cfg(unix)]
        file.set_permissions(fs::Permissions::from_mode(mode))?;
        file.sync_all()?;
        Ok(())
    })();
    // A failed write/chmod/fsync must not leave a stray `.tmp-<uuid>` behind (matching `store_blob`).
    if let Err(e) = write_result {
        let _ = fs::remove_file(&tmp_path);
        return Err(e);
    }
    if let Err(e) = fs::rename(&tmp_path, path) {
        let _ = fs::remove_file(&tmp_path);
        return Err(e.into());
    }
    sync_dir(dir)
}

fn sync_dir(dir: &Path) -> Result<(), RevisionError> {
    crate::secure_fs::sync_dir(dir).map_err(RevisionError::from)
}

fn compute_revision_id(tracked: &BTreeMap<String, TrackedFile>) -> RevisionId {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    for (path, entry) in tracked {
        hasher.update(path.as_bytes());
        hasher.update([0u8]);
        hasher.update(entry.sha256.as_bytes());
        hasher.update([0u8]);
        hasher.update(entry.mode.to_be_bytes());
        hasher.update([0u8]);
        hasher.update(entry.size.to_be_bytes());
        hasher.update([0u8]);
    }
    let digest = crate::prompt::hex_encode(&hasher.finalize());
    RevisionId(format!("r-{}", &digest[..32]))
}

fn diff_paths(
    parent: Option<&BTreeMap<String, TrackedFile>>,
    current: &BTreeMap<String, TrackedFile>,
) -> Vec<String> {
    let Some(parent) = parent else {
        return current.keys().cloned().collect();
    };
    let mut changed = BTreeSet::new();
    for (path, entry) in current {
        match parent.get(path) {
            Some(previous) if previous == entry => {}
            _ => {
                changed.insert(path.clone());
            }
        }
    }
    for path in parent.keys() {
        if !current.contains_key(path) {
            changed.insert(path.clone());
        }
    }
    changed.into_iter().collect()
}

/// What a workspace path currently is, resolved without following symlinks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LiveEntryStat {
    /// The path, or an ancestor needed to reach it, does not exist (including when an ancestor is
    /// a plain file that blocks directory traversal).
    Absent,
    /// A plain regular file.
    Regular { mode: u32, size: u64 },
    /// A real directory.
    Directory,
    /// A symlink, FIFO, socket, device, or a symlink blocking an ancestor component.
    NonRegular,
}

/// [`LiveEntryStat`] plus the content hash of a regular file, for conflict comparison.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LiveEntryState {
    Absent,
    Regular {
        sha256: String,
        mode: u32,
        size: u64,
    },
    Directory,
    NonRegular,
}

/// Inspect a workspace path without following symlinks, classifying absent / regular / directory /
/// non-regular so a user-created symlink or directory can never masquerade as "unchanged".
pub(crate) fn stat_live_entry(
    workspace: &WorkspaceCap,
    rel: &str,
) -> Result<LiveEntryStat, RevisionError> {
    let norm = match WorkspaceCap::validate_relative_components(rel) {
        Ok(n) => n,
        Err(WorkspaceError::InvalidPath { .. }) => return Ok(LiveEntryStat::Absent),
        Err(e) => return Err(e.into()),
    };
    if SensitivePathGuard::is_sensitive(&norm) {
        return Ok(LiveEntryStat::Absent);
    }
    #[cfg(unix)]
    {
        let (parent_dir, leaf) = match workspace.open_parent_dir_no_symlink(&norm, false) {
            Ok(v) => v,
            Err(WorkspaceError::NotFound { .. }) => return Ok(LiveEntryStat::Absent),
            Err(WorkspaceError::SymlinkBlocked { .. }) => {
                return classify_blocked_parent(workspace, &norm)
            }
            Err(WorkspaceError::SensitivePath { .. }) | Err(WorkspaceError::InvalidPath { .. }) => {
                return Ok(LiveEntryStat::Absent)
            }
            Err(e) => return Err(e.into()),
        };
        let c_leaf = CString::new(leaf.as_bytes()).map_err(|_| {
            RevisionError::InvalidStore("file name contains a NUL byte".to_string())
        })?;
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        let rc = unsafe {
            libc::fstatat(
                parent_dir.as_raw_fd(),
                c_leaf.as_ptr(),
                &mut stat,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if rc != 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::ENOENT) {
                return Ok(LiveEntryStat::Absent);
            }
            return Err(err.into());
        }
        Ok(stat_kind(&stat))
    }
    #[cfg(windows)]
    {
        let mut cur = workspace.root().to_path_buf();
        if let Some(parent) = norm.parent() {
            for comp in parent.components() {
                let Component::Normal(part) = comp else {
                    continue;
                };
                if part.is_empty() {
                    continue;
                }
                cur.push(part);
                match fs::symlink_metadata(&cur) {
                    Ok(meta) => {
                        if crate::workspace::is_windows_reparse_point(&meta) {
                            return Ok(LiveEntryStat::NonRegular);
                        }
                        if !meta.is_dir() {
                            return Ok(LiveEntryStat::Absent);
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        return Ok(LiveEntryStat::Absent);
                    }
                    Err(e) => return Err(e.into()),
                }
            }
        }
        let Some(leaf) = norm.file_name() else {
            return Ok(LiveEntryStat::Absent);
        };
        cur.push(leaf);
        match fs::symlink_metadata(&cur) {
            Ok(meta) => {
                if crate::workspace::is_windows_reparse_point(&meta) {
                    Ok(LiveEntryStat::NonRegular)
                } else if meta.is_dir() {
                    Ok(LiveEntryStat::Directory)
                } else if meta.is_file() {
                    let mode = if meta.permissions().readonly() {
                        0o444
                    } else {
                        0o644
                    };
                    Ok(LiveEntryStat::Regular {
                        mode,
                        size: meta.len(),
                    })
                } else {
                    Ok(LiveEntryStat::NonRegular)
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(LiveEntryStat::Absent),
            Err(e) => Err(e.into()),
        }
    }
}

#[cfg(unix)]
fn stat_kind(stat: &libc::stat) -> LiveEntryStat {
    match stat.st_mode & libc::S_IFMT {
        libc::S_IFREG => LiveEntryStat::Regular {
            // macOS `st_mode` is `u16` while Linux' is `u32`; the cast is a no-op on Linux.
            #[allow(clippy::unnecessary_cast)]
            mode: (stat.st_mode & 0o7777) as u32,
            size: stat.st_size.max(0) as u64,
        },
        libc::S_IFDIR => LiveEntryStat::Directory,
        _ => LiveEntryStat::NonRegular,
    }
}

/// Distinguish "the path is genuinely absent" (an ancestor is a plain file blocking traversal)
/// from "an ancestor is a symlink" (an external entry we must never silently follow).
#[cfg(unix)]
fn classify_blocked_parent(
    workspace: &WorkspaceCap,
    norm: &Path,
) -> Result<LiveEntryStat, RevisionError> {
    let Some(parent) = norm.parent() else {
        return Ok(LiveEntryStat::Absent);
    };
    let c_dot = CString::new(".").unwrap();
    let root_dup = unsafe {
        libc::openat(
            workspace.root_dir_fd().as_raw_fd(),
            c_dot.as_ptr(),
            crate::secure_fs::OPEN_DIR_NO_FOLLOW,
        )
    };
    if root_dup < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let mut cur = unsafe { File::from_raw_fd(root_dup) };
    for comp in parent.components() {
        let Component::Normal(part) = comp else {
            continue;
        };
        if part.is_empty() {
            continue;
        }
        let c_part = CString::new(part.as_bytes()).map_err(|_| {
            RevisionError::InvalidStore("directory component contains a NUL byte".to_string())
        })?;
        let fd = unsafe {
            libc::openat(
                cur.as_raw_fd(),
                c_part.as_ptr(),
                crate::secure_fs::OPEN_DIR_NO_FOLLOW,
            )
        };
        if fd < 0 {
            let err = std::io::Error::last_os_error();
            return Ok(match err.raw_os_error() {
                // `O_NOFOLLOW` reports ELOOP for a symlink component.
                Some(libc::ELOOP) => LiveEntryStat::NonRegular,
                // A plain file (or other non-directory) simply means the leaf cannot exist.
                _ => LiveEntryStat::Absent,
            });
        }
        cur = unsafe { File::from_raw_fd(fd) };
    }
    Ok(LiveEntryStat::Absent)
}

#[cfg(unix)]
fn is_eisdir_error(e: &std::io::Error) -> bool {
    e.raw_os_error() == Some(libc::EISDIR)
}

#[cfg(windows)]
fn is_eisdir_error(_e: &std::io::Error) -> bool {
    false
}

/// Full live state of a path: absent / directory / non-regular, or a regular file with its content
/// hash and permission bits. A regular file larger than the snapshot cap is reported by size
/// rather than hashed in full, since it can never equal a tracked entry.
pub(crate) fn live_entry_state(
    workspace: &WorkspaceCap,
    rel: &str,
) -> Result<LiveEntryState, RevisionError> {
    match stat_live_entry(workspace, rel)? {
        LiveEntryStat::Absent => Ok(LiveEntryState::Absent),
        LiveEntryStat::Directory => Ok(LiveEntryState::Directory),
        LiveEntryStat::NonRegular => Ok(LiveEntryState::NonRegular),
        LiveEntryStat::Regular { mode, size } => {
            if size > MAX_SNAPSHOT_FILE_BYTES {
                return Ok(LiveEntryState::Regular {
                    sha256: format!("oversize:{}", size),
                    mode,
                    size,
                });
            }
            let (_, mut file) = match workspace.open_existing_no_symlink(rel) {
                Ok(v) => v,
                Err(WorkspaceError::NotFound { .. })
                | Err(WorkspaceError::SymlinkBlocked { .. })
                | Err(WorkspaceError::SensitivePath { .. })
                | Err(WorkspaceError::InvalidPath { .. }) => return Ok(LiveEntryState::Absent),
                Err(e) => return Err(e.into()),
            };
            let mut hasher = sha2::Sha256::new();
            use sha2::Digest;
            let mut buf = [0u8; 8192];
            loop {
                match file.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => hasher.update(&buf[..n]),
                    Err(e) if is_eisdir_error(&e) => return Ok(LiveEntryState::Directory),
                    Err(e) => return Err(e.into()),
                }
            }
            Ok(LiveEntryState::Regular {
                sha256: crate::prompt::hex_encode(&hasher.finalize()),
                mode,
                size,
            })
        }
    }
}

/// True when `path` is a real directory that legitimately holds tracked files of `rev` (for
/// example `foo` when `foo/bar.txt` is tracked). Such a directory is expected, not external work.
pub(crate) fn is_ancestor_dir_of(rev: &WorkspaceRevision, path: &str) -> bool {
    let prefix = format!("{}/", path);
    rev.tracked_files
        .keys()
        .any(|candidate| candidate.starts_with(&prefix))
}

fn classify_conflict_against_expected(
    expected: &WorkspaceRevision,
    target: &WorkspaceRevision,
    path: &str,
    live: &LiveEntryState,
) -> Option<(ConflictKind, Option<String>, Option<String>)> {
    if expected.skipped_paths.iter().any(|p| p == path) {
        return None;
    }
    match expected.tracked_files.get(path) {
        Some(entry) => match live {
            LiveEntryState::Absent => Some((
                ConflictKind::DeletedExternally,
                Some(entry.sha256.clone()),
                None,
            )),
            LiveEntryState::Directory | LiveEntryState::NonRegular => Some((
                ConflictKind::ModifiedExternally,
                Some(entry.sha256.clone()),
                None,
            )),
            LiveEntryState::Regular { sha256, mode, .. } => {
                if sha256 != &entry.sha256 || (mode & 0o7777) != (entry.mode & 0o7777) {
                    Some((
                        ConflictKind::ModifiedExternally,
                        Some(entry.sha256.clone()),
                        Some(sha256.clone()),
                    ))
                } else {
                    None
                }
            }
        },
        // The path is only in the target revision: anything already there is external work.
        None => match live {
            LiveEntryState::Absent => None,
            LiveEntryState::Directory if is_ancestor_dir_of(expected, path) => None,
            LiveEntryState::Directory | LiveEntryState::NonRegular => {
                Some((ConflictKind::CreatedExternally, None, None))
            }
            LiveEntryState::Regular { sha256, mode, .. } => match target.tracked_files.get(path) {
                Some(want) if &want.sha256 == sha256 && (want.mode & 0o7777) == (mode & 0o7777) => {
                    None
                }
                _ => Some((ConflictKind::CreatedExternally, None, Some(sha256.clone()))),
            },
        },
    }
}

fn classify_conflict_against_target(
    target: &WorkspaceRevision,
    path: &str,
    live: &LiveEntryState,
) -> Option<(ConflictKind, Option<String>, Option<String>)> {
    let entry = target.tracked_files.get(path)?;
    match live {
        LiveEntryState::Absent => None,
        LiveEntryState::Directory | LiveEntryState::NonRegular => Some((
            ConflictKind::ModifiedExternally,
            Some(entry.sha256.clone()),
            None,
        )),
        LiveEntryState::Regular { sha256, mode, .. } => {
            if sha256 != &entry.sha256 || (mode & 0o7777) != (entry.mode & 0o7777) {
                Some((
                    ConflictKind::ModifiedExternally,
                    Some(entry.sha256.clone()),
                    Some(sha256.clone()),
                ))
            } else {
                None
            }
        }
    }
}

pub(crate) fn write_tracked_file(
    workspace: &WorkspaceCap,
    rel: &str,
    bytes: &[u8],
    mode: u32,
    force: bool,
) -> Result<(), RevisionError> {
    let norm = WorkspaceCap::validate_relative_components(rel)?;
    if SensitivePathGuard::is_sensitive(&norm) {
        return Err(RevisionError::InvalidStore(format!(
            "refusing to restore sensitive path '{}'",
            rel
        )));
    }
    if force {
        // A symlink, directory, or special file parked on the target path would make `renameat`
        // fail and wedge the restore in `RecoveryRequired`; an explicit force clears it first.
        match stat_live_entry(workspace, rel)? {
            LiveEntryStat::Absent | LiveEntryStat::Regular { .. } => {}
            LiveEntryStat::Directory | LiveEntryStat::NonRegular => {
                remove_path_force(workspace, rel)?
            }
        }
    }
    let (parent_dir, leaf) = workspace.open_parent_dir_no_symlink(&norm, true)?;
    crate::tools::write::atomic_write_at(&parent_dir, &leaf, bytes, mode, false).map_err(
        |(code, message)| RevisionError::InvalidStore(format!("{}: {}", code.as_str(), message)),
    )?;
    // `open_parent_dir_no_symlink(create = true)` may have created missing ancestors with
    // `mkdirat`; each new directory entry is only durable once the directory that received it is
    // synced, so a crash cannot leave a freshly created ancestor tree unreachable.
    #[cfg(unix)]
    sync_written_ancestors(workspace, &norm)?;
    Ok(())
}

/// `fsync` every directory whose entry table a restore write changed: the workspace root and each
/// ancestor of the written path. Syncing an unchanged directory is harmless, and syncing only the
/// leaf's parent would leave an ancestor that `open_parent_dir_no_symlink` just created
/// unpersisted.
#[cfg(unix)]
fn sync_written_ancestors(workspace: &WorkspaceCap, norm: &Path) -> Result<(), RevisionError> {
    crate::secure_fs::sync_dir_handle(workspace.root_dir_fd())?;
    let mut rel = PathBuf::new();
    if let Some(parent) = norm.parent() {
        for comp in parent.components() {
            if let Component::Normal(part) = comp {
                if part.is_empty() {
                    continue;
                }
                rel.push(part);
                let (_, dir) = workspace.open_dir_no_symlink(&rel.to_string_lossy())?;
                crate::secure_fs::sync_dir_handle(&dir)?;
            }
        }
    }
    Ok(())
}

/// Unlink a tracked file through its parent directory FD. Without `force` only a plain regular file
/// is removed: a symlink, directory, or special file swapped into the path is external work and is
/// never deleted. With `force` the obstructing entry itself is removed, then any directories the
/// mutation created that are now empty are pruned so an undone nested tree leaves nothing behind.
pub(crate) fn remove_tracked_file(
    workspace: &WorkspaceCap,
    rel: &str,
    force: bool,
) -> Result<bool, RevisionError> {
    let norm = WorkspaceCap::validate_relative_components(rel)?;
    if SensitivePathGuard::is_sensitive(&norm) {
        return Err(RevisionError::InvalidStore(format!(
            "refusing to delete sensitive path '{}'",
            rel
        )));
    }
    #[cfg(unix)]
    {
        let (parent_dir, leaf) = match workspace.open_parent_dir_no_symlink(&norm, false) {
            Ok(v) => v,
            Err(WorkspaceError::NotFound { .. }) => return Ok(false),
            Err(e) => return Err(e.into()),
        };
        let c_leaf = CString::new(leaf.as_bytes()).map_err(|_| {
            RevisionError::InvalidStore("file name contains a NUL byte".to_string())
        })?;
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        let rc = unsafe {
            libc::fstatat(
                parent_dir.as_raw_fd(),
                c_leaf.as_ptr(),
                &mut stat,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if rc != 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::ENOENT) {
                return Ok(false);
            }
            return Err(err.into());
        }
        if stat.st_mode & libc::S_IFMT != libc::S_IFREG {
            if !force {
                return Ok(false);
            }
            drop(parent_dir);
            remove_path_force(workspace, rel)?;
            return Ok(true);
        }
        let rc = unsafe { libc::unlinkat(parent_dir.as_raw_fd(), c_leaf.as_ptr(), 0) };
        if rc != 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::ENOENT) {
                return Ok(false);
            }
            return Err(err.into());
        }
        // The unlink changed the parent directory's entry table: confirm it durably before the
        // recovery intent can be cleared, so a crash cannot resurrect the removed file.
        crate::secure_fs::sync_dir_handle(&parent_dir)?;
        drop(parent_dir);
        prune_empty_ancestors(workspace, &norm)?;
        Ok(true)
    }
    #[cfg(windows)]
    {
        match stat_live_entry(workspace, rel)? {
            LiveEntryStat::Absent => return Ok(false),
            LiveEntryStat::Directory | LiveEntryStat::NonRegular => {
                if !force {
                    return Ok(false);
                }
                remove_path_force(workspace, rel)?;
                return Ok(true);
            }
            LiveEntryStat::Regular { .. } => {}
        }
        let (parent_dir, leaf) = match workspace.open_parent_dir_no_symlink(&norm, false) {
            Ok(v) => v,
            Err(WorkspaceError::NotFound { .. }) => return Ok(false),
            Err(e) => return Err(e.into()),
        };
        let parent_path = crate::workspace::windows_final_path_by_handle(&parent_dir)?;
        let target = parent_path.join(&leaf);
        if let Ok(meta) = fs::symlink_metadata(&target) {
            if meta.permissions().readonly() {
                let mut perms = meta.permissions();
                #[allow(clippy::permissions_set_readonly_false)]
                perms.set_readonly(false);
                let _ = fs::set_permissions(&target, perms);
            }
        }
        match fs::remove_file(&target) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e.into()),
        }
        drop(parent_dir);
        prune_empty_ancestors(workspace, &norm)?;
        Ok(true)
    }
}

/// True when `path` is a proper ancestor directory of `descendant` (`path` is `a` and `descendant`
/// is `a/b/...`), using the `/` separator of the store's normalized, relative paths.
fn is_ancestor_path(path: &str, descendant: &str) -> bool {
    descendant.len() > path.len()
        && descendant.as_bytes().get(path.len()) == Some(&b'/')
        && descendant.starts_with(path)
}

/// Collect the workspace-relative paths of every non-directory entry at or below `rel`, without
/// following symlinks. Directories that hold no entries contribute nothing: a merge that crashed
/// before writing its files legitimately leaves an empty directory behind, and the rollback's
/// removal list only ever names files.
fn collect_leaf_entries(
    workspace: &WorkspaceCap,
    rel: &str,
    out: &mut Vec<String>,
) -> Result<(), RevisionError> {
    let (_, entries) = match workspace.read_dir_no_symlink(rel) {
        Ok(v) => v,
        Err(WorkspaceError::NotFound { .. }) => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    for entry in entries {
        let child = if rel.is_empty() || rel == "." {
            entry.name.clone()
        } else {
            format!("{}/{}", rel, entry.name)
        };
        if entry.is_dir {
            collect_leaf_entries(workspace, &child, out)?;
        } else {
            out.push(child);
        }
    }
    Ok(())
}

/// Best-effort removal of directories the mutation created that are now empty. Stops at the first
/// directory that still holds an entry, so a user's added file keeps its ancestors intact.
fn prune_empty_ancestors(workspace: &WorkspaceCap, norm: &Path) -> Result<(), RevisionError> {
    let mut dirs: Vec<String> = Vec::new();
    if let Some(parent) = norm.parent() {
        for comp in parent.components() {
            if let Component::Normal(part) = comp {
                let part = part.to_string_lossy().to_string();
                if !part.is_empty() {
                    dirs.push(part);
                }
            }
        }
    }
    #[cfg(unix)]
    while let Some(child) = dirs.pop() {
        let parent_rel = if dirs.is_empty() {
            ".".to_string()
        } else {
            dirs.join("/")
        };
        let (_, dir) = match workspace.open_dir_no_symlink(&parent_rel) {
            Ok(v) => v,
            Err(_) => break,
        };
        let Ok(c_child) = CString::new(child.as_bytes()) else {
            break;
        };
        let rc = unsafe { libc::unlinkat(dir.as_raw_fd(), c_child.as_ptr(), libc::AT_REMOVEDIR) };
        if rc != 0 {
            break;
        }
        // Removing `child` changed the parent directory's entry table; confirm the prune durably
        // before the recovery intent can be cleared.
        crate::secure_fs::sync_dir_handle(&dir)?;
    }
    #[cfg(windows)]
    while !dirs.is_empty() {
        let rel_dir = dirs.join("/");
        dirs.pop();
        let target_dir = workspace.root().join(&rel_dir);
        if fs::remove_dir(&target_dir).is_err() {
            break;
        }
    }
    Ok(())
}

/// Delete a path whatever it is (symlink, regular file, or directory tree), resolved relative to
/// its already-open parent directory and never following symlinks.
fn remove_path_force(workspace: &WorkspaceCap, rel: &str) -> Result<(), RevisionError> {
    let norm = WorkspaceCap::validate_relative_components(rel)?;
    if SensitivePathGuard::is_sensitive(&norm) {
        return Err(RevisionError::InvalidStore(format!(
            "refusing to delete sensitive path '{}'",
            rel
        )));
    }
    #[cfg(unix)]
    {
        let (parent_dir, leaf) = match workspace.open_parent_dir_no_symlink(&norm, false) {
            Ok(v) => v,
            Err(WorkspaceError::NotFound { .. }) => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        remove_entry_at(parent_dir.as_raw_fd(), &leaf)?;
        // A forced recursive delete changed the containing directory's entry table; the top-level
        // removal is what makes the whole subtree unreachable after a crash.
        crate::secure_fs::sync_dir_handle(&parent_dir)?;
        Ok(())
    }
    #[cfg(windows)]
    {
        let (parent_dir, leaf) = match workspace.open_parent_dir_no_symlink(&norm, false) {
            Ok(v) => v,
            Err(WorkspaceError::NotFound { .. }) => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        let parent_path = crate::workspace::windows_final_path_by_handle(&parent_dir)?;
        let target = parent_path.join(&leaf);
        let res = remove_entry_windows(&target);
        drop(parent_dir);
        res
    }
}

#[cfg(windows)]
fn remove_entry_windows(target: &Path) -> Result<(), RevisionError> {
    let meta = match fs::symlink_metadata(target) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    if meta.permissions().readonly() {
        let mut perms = meta.permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        perms.set_readonly(false);
        let _ = fs::set_permissions(target, perms);
    }
    if crate::workspace::is_windows_reparse_point(&meta) {
        // Never recurse into a junction or directory symlink; remove only the reparse point itself.
        match fs::remove_file(target) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(_) => match fs::remove_dir(target) {
                Ok(()) => return Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(e) => return Err(e.into()),
            },
        }
    }
    if meta.is_dir() {
        for entry in fs::read_dir(target)? {
            let entry = entry?;
            remove_entry_windows(&entry.path())?;
        }
        match fs::remove_dir(target) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    } else {
        match fs::remove_file(target) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

#[cfg(unix)]
fn remove_entry_at(dir_fd: RawFd, name: &OsStr) -> Result<(), RevisionError> {
    let c_name = CString::new(name.as_bytes())
        .map_err(|_| RevisionError::InvalidStore("entry name contains a NUL byte".to_string()))?;
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    let rc = unsafe {
        libc::fstatat(
            dir_fd,
            c_name.as_ptr(),
            &mut stat,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::ENOENT) {
            return Ok(());
        }
        return Err(err.into());
    }
    if stat.st_mode & libc::S_IFMT != libc::S_IFDIR {
        let rc = unsafe { libc::unlinkat(dir_fd, c_name.as_ptr(), 0) };
        if rc != 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::ENOENT) {
                return Ok(());
            }
            return Err(err.into());
        }
        return Ok(());
    }
    let child_fd = unsafe {
        libc::openat(
            dir_fd,
            c_name.as_ptr(),
            crate::secure_fs::OPEN_DIR_NO_FOLLOW,
        )
    };
    if child_fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let result = remove_dir_entries(child_fd);
    unsafe { libc::close(child_fd) };
    result?;
    let rc = unsafe { libc::unlinkat(dir_fd, c_name.as_ptr(), libc::AT_REMOVEDIR) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::ENOENT) {
            return Ok(());
        }
        return Err(err.into());
    }
    Ok(())
}

/// Empty a directory through its own FD. Entries are listed via `/proc/self/fd/<fd>` and removed
/// with `unlinkat` relative to that FD, so a concurrently swapped ancestor symlink cannot redirect
/// the deletion outside the directory already opened.
#[cfg(unix)]
fn remove_dir_entries(dir_fd: RawFd) -> Result<(), RevisionError> {
    let proc_path = PathBuf::from(format!("/proc/self/fd/{}", dir_fd));
    for entry in fs::read_dir(&proc_path)? {
        let entry = entry?;
        let name = entry.file_name();
        if name == OsStr::new(".") || name == OsStr::new("..") {
            continue;
        }
        remove_entry_at(dir_fd, &name)?;
    }
    Ok(())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tracked(path: &str, body: &str) -> BTreeMap<String, TrackedFile> {
        let mut map = BTreeMap::new();
        map.insert(
            path.to_string(),
            TrackedFile {
                sha256: sha256_hex64(body.as_bytes()),
                mode: 0o644,
                size: body.len() as u64,
            },
        );
        map
    }

    #[test]
    fn revision_id_parse_rejects_traversal_and_accepts_content_addresses() {
        assert!(RevisionId::parse("r-0123456789abcdef0123456789abcdef").is_ok());
        for bad in [
            "../../etc/passwd",
            "r-0123456789abcdef0123456789abcde",
            "r-0123456789abcdef0123456789abcdeg",
            "/tmp/r-0123456789abcdef0123456789abcdef",
            "0123456789abcdef0123456789abcdef",
            "",
        ] {
            assert!(
                RevisionId::parse(bad).is_err(),
                "'{}' must not be accepted as a revision id",
                bad
            );
        }
        assert!(RevisionId::parse("  r-0123456789abcdef0123456789abcdef  ").is_ok());
    }

    #[test]
    fn revision_id_is_content_addressed_and_mode_sensitive() {
        let first = compute_revision_id(&tracked("a.rs", "one"));
        assert_eq!(first, compute_revision_id(&tracked("a.rs", "one")));
        assert_ne!(first, compute_revision_id(&tracked("a.rs", "two")));
        assert_ne!(first, compute_revision_id(&tracked("b.rs", "one")));

        let mut executable = tracked("a.rs", "one");
        executable.get_mut("a.rs").unwrap().mode = 0o755;
        assert_ne!(first, compute_revision_id(&executable));
    }

    #[test]
    fn diff_paths_reports_added_removed_and_modified_paths_only() {
        let mut before = tracked("a.rs", "one");
        before.extend(tracked("gone.rs", "x"));
        let mut after = tracked("a.rs", "two");
        after.extend(tracked("new.rs", "y"));
        assert_eq!(
            diff_paths(Some(&before), &after),
            vec![
                "a.rs".to_string(),
                "gone.rs".to_string(),
                "new.rs".to_string()
            ]
        );
        assert!(diff_paths(Some(&after), &after).is_empty());
        assert_eq!(diff_paths(None, &after).len(), 2);
    }

    /// A directory `fsync` that fails while a restore is applying its operations must surface as
    /// [`RestoreResult::RecoveryRequired`] and leave the intent journal in place, so the next
    /// startup replays the restore instead of trusting a workspace whose directory operations are
    /// not provably durable.
    #[cfg(unix)]
    #[test]
    fn a_directory_sync_failure_keeps_the_intent_and_replays() {
        let root = std::env::temp_dir().join(format!(
            "kroma-revision-dir-sync-{}-{}",
            std::process::id(),
            Uuid::new_v4()
        ));
        let ws_dir = root.join("ws");
        fs::create_dir_all(ws_dir.join("src")).unwrap();
        fs::write(ws_dir.join("src").join("a.rs"), "v1\n").unwrap();

        let workspace = WorkspaceCap::new(&ws_dir).unwrap();
        let store = SnapshotStore::new(root.join("snapshots"));
        let baseline = store.capture_revision(&workspace, None, None, 0).unwrap();
        fs::write(ws_dir.join("src").join("a.rs"), "v2\n").unwrap();
        let mutated = store
            .capture_revision(&workspace, Some(&baseline.revision_id), None, 1)
            .unwrap();

        let result = store
            .restore_revision_with_fault(
                &workspace,
                &baseline.revision_id,
                Some(&mutated.revision_id),
                true,
                Some(RestoreFault::DirectorySyncFails),
            )
            .unwrap();
        match result {
            RestoreResult::RecoveryRequired { journal_path, .. } => {
                assert!(
                    journal_path.exists(),
                    "a directory-sync failure must retain the intent journal"
                );
            }
            other => panic!("expected RecoveryRequired, got {:?}", other),
        }
        assert!(
            store.pending_restore().unwrap().is_some(),
            "the intent must survive a directory-sync failure so replay can finish the restore"
        );

        // The fault is disarmed once the restore returns; replaying the retained intent succeeds.
        let recovered = store.recover_pending_restore(&workspace).unwrap();
        assert!(
            matches!(recovered, RestoreResult::Applied { .. }),
            "expected Applied on replay, got {:?}",
            recovered
        );
        assert!(store.pending_restore().unwrap().is_none());
        assert_eq!(
            fs::read_to_string(ws_dir.join("src").join("a.rs")).unwrap(),
            "v1\n",
            "replay must finish the interrupted restore"
        );
        let _ = fs::remove_dir_all(root);
    }
}
