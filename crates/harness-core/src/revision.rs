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
use crate::workspace::{WorkspaceCap, WorkspaceError};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::CString;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
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
const SNAPSHOT_EXCLUDED_DIRS: &[&str] = &[
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

        let parent_files = match parent_rev {
            Some(id) => Some(self.load_revision(id)?.tracked_files),
            None => None,
        };
        let revision = WorkspaceRevision {
            revision_id: compute_revision_id(&tracked),
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

    pub fn load_revision(&self, id: &RevisionId) -> Result<WorkspaceRevision, RevisionError> {
        let path = self.revision_path(id);
        let mut file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&path)
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
        let mut file = match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&path)
        {
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
            self.apply_op(workspace, op)?;
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

        if !force {
            if let Some(exp) = expected.as_ref() {
                let mut conflicts = Vec::new();
                for path in &affected {
                    let live = live_sha256(workspace, path)?;
                    let expected_sha = exp.tracked_files.get(path).map(|e| e.sha256.clone());
                    if live == expected_sha {
                        continue;
                    }
                    let kind = match (&expected_sha, &live) {
                        (Some(_), Some(_)) => ConflictKind::ModifiedExternally,
                        (Some(_), None) => ConflictKind::DeletedExternally,
                        (None, Some(_)) => ConflictKind::CreatedExternally,
                        (None, None) => continue,
                    };
                    conflicts.push(RevisionConflict {
                        path: path.clone(),
                        kind,
                        expected_sha256: expected_sha,
                        live_sha256: live,
                    });
                }
                if !conflicts.is_empty() {
                    conflicts.sort_by(|a, b| a.path.cmp(&b.path));
                    return Ok(RestoreResult::Conflict {
                        target_revision: target.revision_id.clone(),
                        conflicting_paths: conflicts,
                    });
                }
            }
        }

        let mut operations = Vec::new();
        for (path, entry) in &target.tracked_files {
            operations.push(RestoreOp::Write {
                path: path.clone(),
                sha256: entry.sha256.clone(),
                mode: entry.mode,
            });
        }
        for path in &affected {
            if target.tracked_files.contains_key(path) {
                continue;
            }
            if live_sha256(workspace, path)?.is_some() {
                operations.push(RestoreOp::Remove { path: path.clone() });
            }
        }

        let intent = RestoreIntent {
            version: RESTORE_INTENT_VERSION,
            target_revision: target.revision_id.clone(),
            parent_revision: current_expected_rev.cloned(),
            created_at_ms: now_ms(),
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
            if let Err(e) = self.apply_op(workspace, op) {
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

    fn apply_op(&self, workspace: &WorkspaceCap, op: &RestoreOp) -> Result<(), RevisionError> {
        match op {
            RestoreOp::Write { path, sha256, mode } => {
                let bytes = self.read_blob(sha256)?;
                write_tracked_file(workspace, path, &bytes, *mode)
            }
            RestoreOp::Remove { path } => {
                remove_tracked_file(workspace, path)?;
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

    fn store_blob(&self, sha256: &str, bytes: &[u8]) -> Result<(), RevisionError> {
        let path = self.blob_path(sha256);
        let dir = path.parent().ok_or_else(|| {
            RevisionError::InvalidStore("blob path has no parent directory".to_string())
        })?;
        ensure_private_dir(dir)?;
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&path)
        {
            Ok(mut file) => {
                file.write_all(bytes)?;
                file.sync_all()?;
                Ok(())
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let existing = fs::metadata(&path)?;
                if existing.len() != bytes.len() as u64 {
                    return Err(RevisionError::InvalidStore(format!(
                        "blob {} already exists with a different size",
                        sha256
                    )));
                }
                Ok(())
            }
            Err(e) => Err(e.into()),
        }
    }

    fn read_blob(&self, sha256: &str) -> Result<Vec<u8>, RevisionError> {
        let path = self.blob_path(sha256);
        let mut file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&path)
            .map_err(|e| {
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
        let rel_str = rel.to_string_lossy().to_string();
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
            let child_str = child.to_string_lossy().to_string();

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
            tracked.insert(
                child_str,
                TrackedFile {
                    sha256,
                    mode: (meta.mode() & 0o7777) as u32,
                    size,
                },
            );
        }
        Ok(())
    }
}

fn ensure_private_dir(dir: &Path) -> Result<(), RevisionError> {
    match DirBuilder::new().recursive(true).mode(0o700).create(dir) {
        Ok(()) => {
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            let meta = fs::symlink_metadata(dir)?;
            if meta.file_type().is_symlink() || !meta.file_type().is_dir() {
                return Err(RevisionError::InvalidStore(format!(
                    "snapshot store path '{}' is not a real directory",
                    dir.display()
                )));
            }
            Ok(())
        }
        Err(e) => Err(e.into()),
    }
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
    {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&tmp_path)?;
        file.write_all(bytes)?;
        file.set_permissions(fs::Permissions::from_mode(mode))?;
        file.sync_all()?;
    }
    fs::rename(&tmp_path, path)?;
    sync_dir(dir)
}

fn sync_dir(dir: &Path) -> Result<(), RevisionError> {
    let handle = File::open(dir)?;
    handle.sync_all()?;
    Ok(())
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

/// Hash a live workspace file, treating "absent", "not a regular file", and "symlink" alike: none
/// of them can be a tracked revision entry, so they never silently satisfy an expected hash.
fn live_sha256(workspace: &WorkspaceCap, rel: &str) -> Result<Option<String>, RevisionError> {
    let (_, mut file) = match workspace.open_existing_no_symlink(rel) {
        Ok(v) => v,
        Err(WorkspaceError::NotFound { .. })
        | Err(WorkspaceError::SymlinkBlocked { .. })
        | Err(WorkspaceError::SensitivePath { .. }) => return Ok(None),
        Err(WorkspaceError::InvalidPath { .. }) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let mut hasher = sha2::Sha256::new();
    use sha2::Digest;
    let mut buf = [0u8; 8192];
    loop {
        match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => hasher.update(&buf[..n]),
            Err(e) if e.raw_os_error() == Some(libc::EISDIR) => return Ok(None),
            Err(e) => return Err(e.into()),
        }
    }
    Ok(Some(crate::prompt::hex_encode(&hasher.finalize())))
}

fn write_tracked_file(
    workspace: &WorkspaceCap,
    rel: &str,
    bytes: &[u8],
    mode: u32,
) -> Result<(), RevisionError> {
    let norm = WorkspaceCap::validate_relative_components(rel)?;
    if SensitivePathGuard::is_sensitive(&norm) {
        return Err(RevisionError::InvalidStore(format!(
            "refusing to restore sensitive path '{}'",
            rel
        )));
    }
    let (parent_dir, leaf) = workspace.open_parent_dir_no_symlink(&norm, true)?;
    crate::tools::write::atomic_write_at(&parent_dir, &leaf, bytes, mode, false).map_err(
        |(code, message)| {
            RevisionError::InvalidStore(format!("{}: {}", code.as_str(), message))
        },
    )
}

/// Unlink a tracked file through its parent directory FD, and only when the leaf is still a plain
/// regular file: a symlink, directory, or special file swapped into the path is never deleted.
fn remove_tracked_file(workspace: &WorkspaceCap, rel: &str) -> Result<bool, RevisionError> {
    let norm = WorkspaceCap::validate_relative_components(rel)?;
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
        return Ok(false);
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
    Ok(true)
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
