//! Workspace revision snapshots, three-state restore, and crash-safe restore journaling.
//!
//! Gate 1 of the v1.0 blueprint: bind workspace file versions to the session WAL so an agent turn
//! can be rewound without ever silently clobbering work the user did outside the harness.
//!
//! Storage is a manifest + content-addressed blob store kept *outside* the workspace tree. The
//! blueprint prefers Git objects first, but a self-contained CAS avoids requiring `git` and avoids
//! touching the user's repository; [`SnapshotStore`] is the abstraction that lets a Git-backed or
//! Merkle-backed backend be swapped in later once measured against this baseline.

use crate::prompt::sha256_hex64;
use crate::sensitive_paths::SensitivePathGuard;
use crate::session::SessionJournal;
use crate::workspace::{WorkspaceCap, WorkspaceError};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
#[cfg(unix)]
use std::ffi::{CString, OsStr};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
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

/// Largest single file that may be tracked. Larger files are recorded in `skipped_paths` so a
/// vendored binary or dataset can never make a snapshot unbounded.
pub const MAX_SNAPSHOT_FILE_BYTES: u64 = 4 * 1024 * 1024;
/// Largest total tracked payload per revision.
pub const MAX_SNAPSHOT_TOTAL_BYTES: u64 = 256 * 1024 * 1024;
/// Directory recursion guard so a pathological/deep tree cannot exhaust the stack.
const MAX_SNAPSHOT_DEPTH: usize = 32;

/// Build outputs and dependency caches are never part of a reviewable revision.
pub(crate) const SNAPSHOT_EXCLUDED_DIRS: &[&str] = &[
    ".git", ".hg", ".svn", "target", "node_modules", ".venv", "venv", "__pycache__",
    ".mypy_cache", ".pytest_cache", ".gradle", ".tox", ".next", "dist", "build",
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
    pub revision_id: RevisionId,
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
            RestoreResult::Applied { restored_revision, .. } => restored_revision,
            RestoreResult::Conflict { target_revision, .. } => target_revision,
            RestoreResult::RecoveryRequired { target_revision, .. } => target_revision,
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
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum RestoreOp {
    Write { path: String, sha256: String, mode: u32 },
    Remove { path: String },
}

impl RestoreOp {
    fn path(&self) -> &str {
        match self {
            RestoreOp::Write { path, .. } | RestoreOp::Remove { path } => path,
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

    pub fn blob_path(&self, sha256: &str) -> PathBuf {
        self.root.join(BLOB_DIR).join(&sha256[..2]).join(sha256)
    }

    pub fn revision_path(&self, id: &RevisionId) -> PathBuf {
        self.root.join(REVISION_DIR).join(format!("{}.json", id.as_str()))
    }

    pub fn ensure_layout(&self) -> Result<(), RevisionError> {
        ensure_private_dir(&self.root)?;
        ensure_private_dir(&self.root.join(BLOB_DIR))?;
        ensure_private_dir(&self.root.join(REVISION_DIR))?;
        Ok(())
    }

    /// Capture the current workspace state as a new revision.
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
        write_atomic_bytes(
            &self.revision_path(&revision.revision_id),
            &serde_json::to_vec_pretty(&revision)?,
            0o600,
        )?;
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
        let mut file = open_store_file_no_symlink(&path)
            .map_err(|_| RevisionError::UnknownRevision(id.as_str().to_string()))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
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
        revisions.sort_by_key(|r| (r.turn_index, r.timestamp_ms, r.revision_id.as_str().to_string()));
        Ok(revisions)
    }

    /// Read the unrecovered restore intent journal, if one is present.
    pub fn pending_restore(&self) -> Result<Option<RestoreIntent>, RevisionError> {
        let path = self.intent_path();
        let mut file = match open_store_file_no_symlink(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
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
    pub fn recover_pending_restore(
        &self,
        workspace: &WorkspaceCap,
    ) -> Result<RestoreResult, RevisionError> {
        self.ensure_layout()?;
        let Some(intent) = self.pending_restore()? else {
            return Err(RevisionError::InvalidStore(
                "no pending restore intent to recover".to_string(),
            ));
        };
        let mut restored_files = Vec::new();
        let mut removed_files = Vec::new();
        for op in &intent.operations {
            self.apply_op(workspace, op, intent.force)?;
            match op {
                RestoreOp::Write { path, .. } => restored_files.push(path.clone()),
                RestoreOp::Remove { path } => removed_files.push(path.clone()),
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
            .map(|path| RestoreOp::Remove { path })
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
                RestoreOp::Remove { path } => removed_files.push(path.clone()),
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

        self.clear_intent()?;
        restored_files.sort();
        removed_files.sort();
        Ok(RestoreResult::Applied {
            restored_revision: intent.target_revision.clone(),
            restored_files,
            removed_files,
        })
    }

    fn apply_op(&self, workspace: &WorkspaceCap, op: &RestoreOp, force: bool) -> Result<(), RevisionError> {
        match op {
            RestoreOp::Write { path, sha256, mode } => {
                let bytes = self.read_blob(sha256)?;
                write_tracked_file(workspace, path, &bytes, *mode, force)
            }
            RestoreOp::Remove { path } => {
                remove_tracked_file(workspace, path, force)?;
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
        let path = self.blob_path(sha256);
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
            let mut file = create_new_store_file_no_symlink(&tmp_path, 0o600)?;
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
        let Some(newest) = candidate else { return Ok(None); };
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
        let path = self.blob_path(sha256);
        let mut file = open_store_file_no_symlink(&path).map_err(|e| {
            RevisionError::InvalidStore(format!("cannot read snapshot blob {}: {}", sha256, e))
        })?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
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
            if size > MAX_SNAPSHOT_FILE_BYTES || total_bytes.saturating_add(size) > MAX_SNAPSHOT_TOTAL_BYTES
            {
                skipped.insert(child_str);
                continue;
            }
            let mut bytes = Vec::with_capacity(size as usize);
            file.read_to_end(&mut bytes)?;
            if bytes.len() as u64 != size {
                // The file changed size mid-read; never record a torn snapshot.
                skipped.insert(child_str);
                continue;
            }
            let sha256 = sha256_hex64(&bytes);
            self.store_blob(&sha256, &bytes)?;
            *total_bytes += size;
            #[cfg(unix)]
            let mode = (meta.mode() & 0o7777) as u32;
            #[cfg(windows)]
            let mode = if meta.permissions().readonly() {
                0o444
            } else {
                0o644
            };
            tracked.insert(
                child_str,
                TrackedFile {
                    sha256,
                    mode,
                    size,
                },
            );
        }
        Ok(())
    }
}

fn open_store_file_no_symlink(path: &Path) -> std::io::Result<File> {
    #[cfg(unix)]
    {
        OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        };
        let sym = fs::symlink_metadata(path)?;
        if crate::workspace::is_windows_reparse_point(&sym) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "refusing to open reparse point in snapshot store",
            ));
        }
        let f = OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .open(path)?;
        let opened_meta = f.metadata()?;
        if crate::workspace::is_windows_reparse_point(&opened_meta) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "refusing to open reparse point in snapshot store",
            ));
        }
        Ok(f)
    }
}

fn create_new_store_file_no_symlink(path: &Path, mode: u32) -> std::io::Result<File> {
    #[cfg(unix)]
    {
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)
    }
    #[cfg(windows)]
    {
        let _ = mode;
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)
    }
}

/// Create (if needed) and lock down a private store directory (O3).
///
/// Create `dir` as a real, symlink-free directory at mode 0700.
///
/// `DirBuilder::recursive(true)` follows symlinked ancestors, and path-based `fs::set_permissions`
/// is a `chmod` that follows them too — so a symlinked store path (or a symlinked ancestor swapped
/// in between the check and the chmod) could redirect the chmod to an attacker-chosen target.
/// Instead every component is created with `mkdirat` on the *verified* parent descriptor and then
/// re-opened with `O_NOFOLLOW | O_DIRECTORY`, so a symlinked component is refused rather than
/// traversed. Only the final component is chmod-ed: pre-existing ancestors (e.g. `/tmp`) keep their
/// own permissions and are never touched.
pub(crate) fn ensure_private_dir(dir: &Path) -> Result<(), RevisionError> {
    #[cfg(unix)]
    {
        // Lexically normalize (resolving `.` / `..`) and split into (anchor, components). Every
        // intermediate component is re-opened with `O_NOFOLLOW`, so it is always a real directory and
        // lexical `..` resolution matches what the filesystem would do.
        let mut absolute = false;
        let mut components: Vec<std::ffi::OsString> = Vec::new();
        for component in dir.components() {
            match component {
                Component::RootDir => {
                    absolute = true;
                    components.clear();
                }
                Component::CurDir => {}
                Component::ParentDir => {
                    components.pop();
                }
                other => components.push(other.as_os_str().to_os_string()),
            }
        }
        if components.is_empty() {
            return Err(RevisionError::InvalidStore(format!(
                "snapshot store path '{}' has no directory component",
                dir.display()
            )));
        }
        let anchor = if absolute { Path::new("/") } else { Path::new(".") };
        let mut handle = open_real_dir(anchor)?;
        for (index, name) in components.iter().enumerate() {
            let name_c = CString::new(name.as_bytes()).map_err(|_| {
                RevisionError::InvalidStore("snapshot store path contains a NUL byte".to_string())
            })?;
            let rc = unsafe { libc::mkdirat(handle.as_raw_fd(), name_c.as_ptr(), 0o700) };
            if rc != 0 {
                let err = std::io::Error::last_os_error();
                if err.kind() != std::io::ErrorKind::AlreadyExists {
                    return Err(err.into());
                }
            }
            handle = open_dir_at(handle.as_raw_fd(), &name_c)?;
            if index + 1 == components.len() {
                handle.set_permissions(fs::Permissions::from_mode(0o700))?;
            }
        }
        Ok(())
    }
    #[cfg(windows)]
    {
        let mut prefix_or_root = PathBuf::new();
        let mut normals: Vec<std::ffi::OsString> = Vec::new();
        for component in dir.components() {
            match component {
                Component::Prefix(p) => {
                    prefix_or_root.push(p.as_os_str());
                    normals.clear();
                }
                Component::RootDir => {
                    prefix_or_root.push(component.as_os_str());
                    normals.clear();
                }
                Component::CurDir => {}
                Component::ParentDir => {
                    normals.pop();
                }
                Component::Normal(part) => {
                    normals.push(part.to_os_string());
                }
            }
        }
        if normals.is_empty() {
            return Err(RevisionError::InvalidStore(format!(
                "snapshot store path '{}' has no directory component",
                dir.display()
            )));
        }
        let mut collapsed = if prefix_or_root.as_os_str().is_empty() {
            PathBuf::from(".")
        } else {
            prefix_or_root
        };
        for part in &normals {
            collapsed.push(part);
        }
        if let Some(parent) = collapsed.parent() {
            if !parent.as_os_str().is_empty() && !parent.exists() {
                fs::create_dir_all(parent)?;
            }
            if let Ok(p_meta) = fs::symlink_metadata(parent) {
                if crate::workspace::is_windows_reparse_point(&p_meta) {
                    return Err(RevisionError::InvalidStore(format!(
                        "snapshot store parent '{}' is a reparse point",
                        parent.display()
                    )));
                }
            }
        }
        match fs::symlink_metadata(&collapsed) {
            Ok(meta) => {
                if crate::workspace::is_windows_reparse_point(&meta) || !meta.is_dir() {
                    return Err(RevisionError::InvalidStore(format!(
                        "snapshot store path '{}' is not a real directory",
                        collapsed.display()
                    )));
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir(&collapsed)?;
            }
            Err(e) => return Err(e.into()),
        }
        let _ = crate::workspace::open_windows_handle_no_reparse(&collapsed, &collapsed, Some(true))
            .map_err(|e| RevisionError::InvalidStore(e.to_string()))?;
        Ok(())
    }
}

/// Open `path` as a directory without ever following a final symlink.
#[cfg(unix)]
fn open_real_dir(path: &Path) -> Result<File, RevisionError> {
    let handle = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    verify_is_real_dir(&handle, &path.display().to_string())?;
    Ok(handle)
}

/// `openat` a child directory of an already-verified directory descriptor, refusing symlinks.
#[cfg(unix)]
fn open_dir_at(parent_fd: RawFd, name: &CString) -> Result<File, RevisionError> {
    let fd = unsafe {
        libc::openat(
            parent_fd,
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let handle = unsafe { File::from_raw_fd(fd) };
    verify_is_real_dir(&handle, &name.to_string_lossy())?;
    Ok(handle)
}

/// `fstat` the open descriptor and reject anything that is not a directory.
#[cfg(unix)]
fn verify_is_real_dir(handle: &File, display: &str) -> Result<(), RevisionError> {
    let meta = handle.metadata()?;
    if !meta.is_dir() {
        return Err(RevisionError::InvalidStore(format!(
            "snapshot store path '{}' is not a real directory",
            display
        )));
    }
    Ok(())
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
        let mut file = create_new_store_file_no_symlink(&tmp_path, mode)?;
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
    #[cfg(unix)]
    {
        let handle = File::open(dir)?;
        handle.sync_all()?;
        Ok(())
    }
    #[cfg(windows)]
    {
        let _ = dir;
        Ok(())
    }
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
    Regular { sha256: String, mode: u32, size: u64 },
    Directory,
    NonRegular,
}

/// Inspect a workspace path without following symlinks, classifying absent / regular / directory /
/// non-regular so a user-created symlink or directory can never masquerade as "unchanged".
pub(crate) fn stat_live_entry(workspace: &WorkspaceCap, rel: &str) -> Result<LiveEntryStat, RevisionError> {
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
                let Component::Normal(part) = comp else { continue };
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
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if root_dup < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let mut cur = unsafe { File::from_raw_fd(root_dup) };
    for comp in parent.components() {
        let Component::Normal(part) = comp else { continue };
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
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
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
pub(crate) fn live_entry_state(workspace: &WorkspaceCap, rel: &str) -> Result<LiveEntryState, RevisionError> {
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
                    Err(e) if is_eisdir_error(&e) => {
                        return Ok(LiveEntryState::Directory)
                    }
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
    rev.tracked_files.keys().any(|candidate| candidate.starts_with(&prefix))
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
            LiveEntryState::Regular { sha256, mode, .. } => {
                match target.tracked_files.get(path) {
                    Some(want)
                        if &want.sha256 == sha256 && (want.mode & 0o7777) == (mode & 0o7777) =>
                    {
                        None
                    }
                    _ => Some((ConflictKind::CreatedExternally, None, Some(sha256.clone()))),
                }
            }
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
        |(code, message)| {
            RevisionError::InvalidStore(format!("{}: {}", code.as_str(), message))
        },
    )
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
        let _ = parent_dir.sync_all();
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
        remove_entry_at(parent_dir.as_raw_fd(), &leaf)
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
    let rc = unsafe { libc::fstatat(dir_fd, c_name.as_ptr(), &mut stat, libc::AT_SYMLINK_NOFOLLOW) };
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
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
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
            vec!["a.rs".to_string(), "gone.rs".to_string(), "new.rs".to_string()]
        );
        assert!(diff_paths(Some(&after), &after).is_empty());
        assert_eq!(diff_paths(None, &after).len(), 2);
    }
}
