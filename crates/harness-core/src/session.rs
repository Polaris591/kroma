//! Append-only, hash-chained session WAL with branchable transcript history.
//!
//! This persists the exact structured provider payloads exposed by the harness (including
//! provider reasoning signatures/opaque payloads). It does not promise provider-side KV-cache
//! hits: those additionally depend on stable wire serialization and provider cache policy.

use crate::attribution::MutationRecord;
use crate::context_projector::{
    extract_source_entry_id, CompactionReason, ContextProjector, EvidenceLink,
};
use crate::messages::TranscriptItem;
use crate::prompt::sha256_hex64;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;
use uuid::Uuid;

const WAL_VERSION: u32 = 1;
const ZERO_HASH: &str = "0000000000000000000000000000000000000000000000000000000000000000";
/// Hard cap on a session WAL file. Bounds how much damage a mis-supplied `--resume` path can do
/// and keeps recovery memory-bounded (F-01). Public so the CLI can reject an oversized explicit
/// `--resume <path>` before it ever opens the file for writing.
pub const MAX_WAL_FILE_BYTES: u64 = 64 * 1024 * 1024;
/// Hard cap on a single WAL line so a corrupt/hostile file cannot force a huge allocation.
const MAX_WAL_LINE_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Error)]
pub enum SessionError {
    #[error("session WAL I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid session WAL: {0}")]
    Invalid(String),
    #[error("session WAL serialization error: {0}")]
    Serde(#[from] serde_json::Error),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionEntry {
    pub id: String,
    pub parent_id: Option<String>,
    /// Absolute turn index in the session tree; zero is the initial user prompt.
    pub turn: usize,
    pub item: TranscriptItem,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileTouch {
    pub path: String,
    pub last_touched_turn: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionCheckpoint {
    pub leaf_id: Option<String>,
    pub turn: usize,
    pub pinned_baseline_end: usize,
    pub last_touched: Vec<FileTouch>,
    pub reasoning_blocks_sha256: String,
    /// Workspace revision captured before this turn began (Gate 1). `serde(default)` keeps WALs
    /// written before Gate 1 readable.
    #[serde(default)]
    pub workspace_revision_id: Option<String>,
    /// Ancestor directories whose `AGENTS.md`/`CLAUDE.md` JIT guidance was already injected on this
    /// branch (Gate 3). `serde(default)` keeps earlier WALs readable.
    #[serde(default)]
    pub subtree_dirs: Vec<String>,
    /// Gate 4 mutation ledger as of this checkpoint. `serde(default)` keeps earlier WALs readable.
    #[serde(default)]
    pub mutations: Vec<MutationRecord>,
    /// Gate 5 cumulative subagent spawn count on this branch as of this checkpoint.
    #[serde(default)]
    pub subagents_spawned: usize,
}

/// Append-only record binding a captured `WorkspaceRevision` to the WAL entry that was active when
/// it was taken. `--undo` / `--restore-rev` / `--list-revisions` replay these records.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RevisionRecord {
    pub leaf_id: Option<String>,
    pub turn: usize,
    pub revision_id: String,
    pub parent_revision_id: Option<String>,
    pub changed_paths: Vec<String>,
    pub tracked_file_count: usize,
    pub captured_at_ms: u64,
}

#[derive(Debug, Clone)]
struct ProjectionOverlay {
    projected_at_leaf_id: String,
    entry_id: String,
    item: TranscriptItem,
    evidence_links: Vec<EvidenceLink>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "status", content = "reason", rename_all = "snake_case")]
enum CompletionResolution {
    Verified,
    Skipped(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
enum WalPayload {
    Header {
        version: u32,
        session_id: String,
        workspace_root: PathBuf,
        created_at_ms: u64,
    },
    Append { entry: SessionEntry },
    /// Append-only projection overlay for compaction changes to an existing event. `evidence_links`
    /// binds each Outline Stub to the raw evidence it replaced (Gate 2); `serde(default)` keeps
    /// WALs written before Gate 2 readable.
    ProjectionUpdate {
        projected_at_leaf_id: String,
        entry_id: String,
        item: TranscriptItem,
        #[serde(default)]
        evidence_links: Vec<EvidenceLink>,
    },
    SetLeaf { leaf_id: String },
    /// Records the final gate result; subsequent file mutations become dirty again.
    CompletionResolved { leaf_id: String, resolution: CompletionResolution },
    Checkpoint {
        leaf_id: Option<String>,
        turn: usize,
        pinned_baseline_end: usize,
        last_touched: Vec<FileTouch>,
        reasoning_blocks_sha256: String,
        #[serde(default)]
        workspace_revision_id: Option<String>,
        #[serde(default)]
        subtree_dirs: Vec<String>,
        /// Gate 4 mutation ledger (branch-scoped). `serde(default)` keeps earlier WALs readable.
        #[serde(default)]
        mutations: Vec<MutationRecord>,
        /// Gate 5 cumulative subagent spawn count on this branch.
        #[serde(default)]
        subagents_spawned: usize,
    },
    /// A workspace revision snapshot was captured at `turn` (Gate 1).
    RevisionCaptured {
        leaf_id: Option<String>,
        turn: usize,
        revision_id: String,
        parent_revision_id: Option<String>,
        changed_paths: Vec<String>,
        tracked_file_count: usize,
        captured_at_ms: u64,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WalLine {
    seq: u64,
    prev_hash: String,
    hash: String,
    payload: WalPayload,
}

pub struct SessionJournal {
    path: PathBuf,
    session_id: String,
    workspace_root: PathBuf,
    file: File,
    next_seq: u64,
    last_hash: String,
    leaf_id: Option<String>,
    entries: HashMap<String, SessionEntry>,
    checkpoints: Vec<SessionCheckpoint>,
    revision_records: Vec<RevisionRecord>,
    completion_resolution_leaf_ids: Vec<String>,
    projection_updates: Vec<ProjectionOverlay>,
}

impl SessionJournal {
    /// Create a new mode-0600 JSONL WAL beneath `session_dir`.
    pub fn create(
        session_dir: impl AsRef<Path>,
        workspace_root: impl AsRef<Path>,
    ) -> Result<Self, SessionError> {
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(session_dir.as_ref())?;
        // F-08: the session store holds structured transcripts, so keep it user-private (0700).
        fs::set_permissions(session_dir.as_ref(), fs::Permissions::from_mode(0o700))?;
        let session_id = Uuid::new_v4().to_string();
        let path = session_dir.as_ref().join(format!("{}.wal.jsonl", session_id));
        let file = OpenOptions::new()
            .create_new(true)
            .append(true)
            .read(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&path)?;
        acquire_wal_lock(&file)?;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        let mut journal = Self {
            path,
            session_id: session_id.clone(),
            workspace_root: workspace_root.as_ref().to_path_buf(),
            file,
            next_seq: 0,
            last_hash: ZERO_HASH.to_string(),
            leaf_id: None,
            entries: HashMap::new(),
            checkpoints: Vec::new(),
            revision_records: Vec::new(),
            completion_resolution_leaf_ids: Vec::new(),
            projection_updates: Vec::new(),
        };
        journal.append_payload(WalPayload::Header {
            version: WAL_VERSION,
            session_id,
            workspace_root: journal.workspace_root.clone(),
            created_at_ms: now_ms(),
        })?;
        Ok(journal)
    }

    /// Open and verify a WAL. An incomplete final line (power-loss/torn write) is truncated;
    /// corruption in any complete line is rejected instead of silently losing history.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, SessionError> {
        let path = path.as_ref().to_path_buf();

        // F-01: never open a symlink, directory, special file or oversized file as a WAL. A bare
        // `--resume <path>` must not be able to damage an unrelated file.
        let sym_meta = fs::symlink_metadata(&path)?;
        if sym_meta.file_type().is_symlink() {
            return Err(SessionError::Invalid(format!(
                "refusing to open a symlink as a session WAL: {}",
                path.display()
            )));
        }
        if !sym_meta.file_type().is_file() {
            return Err(SessionError::Invalid(format!(
                "session WAL path is not a regular file: {}",
                path.display()
            )));
        }

        // `O_NOFOLLOW` closes the symlink-swap window between `symlink_metadata` and `open`.
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .append(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&path)?;
        acquire_wal_lock(&file)?;

        // Re-check the opened handle itself (fstat) so a raced replacement cannot slip through.
        let opened_meta = file.metadata()?;
        if !opened_meta.file_type().is_file() {
            return Err(SessionError::Invalid(
                "session WAL is not a regular file".into(),
            ));
        }
        // F-01: the pre-open `symlink_metadata` snapshot and the opened handle must be the very same
        // inode. If a concurrent rename/symlink swap replaced the path in the window between the
        // check and the open, `(st_dev, st_ino)` differ and we must refuse rather than operate on
        // (and possibly truncate) a file we never validated.
        if (sym_meta.dev(), sym_meta.ino()) != (opened_meta.dev(), opened_meta.ino()) {
            return Err(SessionError::Invalid(
                "session WAL changed identity between stat and open".into(),
            ));
        }
        if opened_meta.len() == 0 {
            return Err(SessionError::Invalid("empty WAL".into()));
        }
        if opened_meta.len() > MAX_WAL_FILE_BYTES {
            return Err(SessionError::Invalid(format!(
                "session WAL exceeds the maximum size of {} bytes",
                MAX_WAL_FILE_BYTES
            )));
        }

        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        if bytes.is_empty() {
            return Err(SessionError::Invalid("empty WAL".into()));
        }

        // F-01: only *complete* lines (terminated by '\n') are validated before any mutation, and
        // the trailing torn-write fragment is never parsed, trusted, or used to size a truncation.
        let complete_len = if bytes.ends_with(b"\n") {
            bytes.len()
        } else {
            bytes
                .iter()
                .rposition(|b| *b == b'\n')
                .map(|i| i + 1)
                .unwrap_or(0)
        };
        if complete_len == 0 {
            return Err(SessionError::Invalid("no complete WAL header".into()));
        }
        let complete = &bytes[..complete_len];

        let mut expected_seq = 0u64;
        let mut prev_hash = ZERO_HASH.to_string();
        let mut session_id = None;
        let mut workspace_root = None;
        let mut entries: HashMap<String, SessionEntry> = HashMap::new();
        let mut leaf_id: Option<String> = None;
        let mut checkpoints: Vec<SessionCheckpoint> = Vec::new();
        let mut revision_records: Vec<RevisionRecord> = Vec::new();
        let mut completion_resolution_leaf_ids: Vec<String> = Vec::new();
        let mut projection_updates: Vec<ProjectionOverlay> = Vec::new();

        for (line_idx, raw) in complete.split(|b| *b == b'\n').filter(|line| !line.is_empty()).enumerate() {
            if raw.len() > MAX_WAL_LINE_BYTES {
                return Err(SessionError::Invalid(format!(
                    "line {} exceeds the maximum WAL line size of {} bytes",
                    line_idx + 1,
                    MAX_WAL_LINE_BYTES
                )));
            }
            let line: WalLine = serde_json::from_slice(raw).map_err(|e| {
                SessionError::Invalid(format!("line {} is not valid JSON: {}", line_idx + 1, e))
            })?;
            if line.seq != expected_seq {
                return Err(SessionError::Invalid(format!(
                    "sequence mismatch at line {}: expected {}, got {}",
                    line_idx + 1, expected_seq, line.seq
                )));
            }
            if line.prev_hash != prev_hash {
                return Err(SessionError::Invalid(format!("hash-chain predecessor mismatch at line {}", line_idx + 1)));
            }
            let computed = calculate_hash(line.seq, &line.prev_hash, &line.payload)?;
            if line.hash != computed {
                return Err(SessionError::Invalid(format!("hash mismatch at line {}", line_idx + 1)));
            }
            match line.payload {
                WalPayload::Header { version, session_id: sid, workspace_root: root, .. } => {
                    if expected_seq != 0 || version != WAL_VERSION {
                        return Err(SessionError::Invalid("invalid or duplicate WAL header".into()));
                    }
                    session_id = Some(sid);
                    workspace_root = Some(root);
                }
                WalPayload::Append { entry } => {
                    if expected_seq == 0 {
                        return Err(SessionError::Invalid("append before header".into()));
                    }
                    if entry.id.trim().is_empty() || entries.contains_key(&entry.id) {
                        return Err(SessionError::Invalid("empty or duplicate entry ID".into()));
                    }
                    if let Some(ref parent) = entry.parent_id {
                        if !entries.contains_key(parent) {
                            return Err(SessionError::Invalid(format!("unknown parent entry {}", parent)));
                        }
                    }
                    leaf_id = Some(entry.id.clone());
                    entries.insert(entry.id.clone(), entry);
                }
                WalPayload::ProjectionUpdate { projected_at_leaf_id, entry_id, item, evidence_links: links } => {
                    if !entries.contains_key(&projected_at_leaf_id) {
                        return Err(SessionError::Invalid(format!("projection update references unknown projection leaf {}", projected_at_leaf_id)));
                    }
                    if !entries.contains_key(&entry_id) {
                        return Err(SessionError::Invalid(format!("projection update references unknown entry {}", entry_id)));
                    }
                    projection_updates.push(ProjectionOverlay { projected_at_leaf_id, entry_id, item, evidence_links: links });
                }
                WalPayload::SetLeaf { leaf_id: selected } => {
                    if !entries.contains_key(&selected) {
                        return Err(SessionError::Invalid(format!("branch points to unknown entry {}", selected)));
                    }
                    leaf_id = Some(selected);
                }
                WalPayload::CompletionResolved { leaf_id, resolution: _ } => {
                    if !entries.contains_key(&leaf_id) {
                        return Err(SessionError::Invalid(format!("completion resolution references unknown entry {}", leaf_id)));
                    }
                    completion_resolution_leaf_ids.push(leaf_id);
                }
                WalPayload::Checkpoint { leaf_id, turn, pinned_baseline_end, last_touched, reasoning_blocks_sha256, workspace_revision_id, subtree_dirs, mutations, subagents_spawned } => {
                    checkpoints.push(SessionCheckpoint { leaf_id, turn, pinned_baseline_end, last_touched, reasoning_blocks_sha256, workspace_revision_id, subtree_dirs, mutations, subagents_spawned });
                }
                WalPayload::RevisionCaptured { leaf_id, turn, revision_id, parent_revision_id, changed_paths, tracked_file_count, captured_at_ms } => {
                    if !revision_id.starts_with("r-") {
                        return Err(SessionError::Invalid(format!("invalid revision id '{}'", revision_id)));
                    }
                    revision_records.push(RevisionRecord { leaf_id, turn, revision_id, parent_revision_id, changed_paths, tracked_file_count, captured_at_ms });
                }
            }
            prev_hash = line.hash;
            expected_seq += 1;
        }

        let session_id = session_id.ok_or_else(|| SessionError::Invalid("missing WAL header".into()))?;
        let workspace_root = workspace_root.ok_or_else(|| SessionError::Invalid("missing workspace root".into()))?;
        // F-01: only now — with every complete line (header + full hash chain) verified — repair a
        // torn trailing fragment. Any invalid file returned above is left byte-for-byte untouched.
        if complete_len != bytes.len() {
            file.set_len(complete_len as u64)?;
            // Durably record the repair before returning the handle to the caller.
            file.sync_all()?;
        }
        // F-01/F-08: set 0600 on the open FD itself (`fchmod`), never by path (`chmod`).
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        Ok(Self {
            path,
            session_id,
            workspace_root,
            file,
            next_seq: expected_seq,
            last_hash: prev_hash,
            leaf_id,
            entries,
            checkpoints,
            revision_records,
            completion_resolution_leaf_ids,
            projection_updates,
        })
    }

    /// Re-seed a projector's raw-evidence store from this WAL (Gate 2).
    ///
    /// The WAL keeps the pre-overlay `Append` item for every entry while `active_entries()` yields
    /// the compacted stub, so the raw bytes a stub replaced survive a process restart. Only entries
    /// on the active branch are considered and the links come from the branch-filtered
    /// [`SessionJournal::evidence_links`], so a stub recorded on an abandoned branch can never be
    /// re-seeded onto the live one.
    pub fn seed_projector_evidence(&self, projector: &mut ContextProjector) {
        let recorded = self.evidence_links();
        for (index, entry) in self.active_entries().iter().enumerate() {
            let TranscriptItem::ToolResult(active_tr) = &entry.item else { continue; };
            let Some(sid) = extract_source_entry_id(&active_tr.content) else { continue; };
            let Some(TranscriptItem::ToolResult(raw_tr)) = self.raw_entry_item(&entry.id) else {
                continue;
            };
            // Only a genuinely compacted entry (stub shorter than the raw output) has hidden evidence.
            if raw_tr.content.len() <= active_tr.content.len() {
                continue;
            }
            let link = recorded
                .iter()
                .find(|l| l.source_entry_id == sid)
                .cloned()
                .unwrap_or_else(|| EvidenceLink {
                    source_entry_id: sid.clone(),
                    canonical_index: index,
                    call_id: raw_tr.call_id.0.clone(),
                    tool_name: raw_tr.tool_name.clone(),
                    relative_path: raw_tr.meta.relative_path.clone(),
                    original_sha256: sha256_hex64(raw_tr.content.as_bytes()),
                    original_bytes: raw_tr.content.len(),
                    original_lines: raw_tr.content.lines().count(),
                    stub_bytes: active_tr.content.len(),
                    reason: CompactionReason::StepDownArchive,
                });
            projector.register_evidence(link, Arc::clone(&raw_tr.content));
        }
    }

    pub fn path(&self) -> &Path { &self.path }
    pub fn session_id(&self) -> &str { &self.session_id }
    pub fn workspace_root(&self) -> &Path { &self.workspace_root }
    pub fn leaf_id(&self) -> Option<&str> { self.leaf_id.as_deref() }

    /// Evidence links recorded for the active branch, ordered by canonical index (Gate 2).
    ///
    /// One projection overlay per `entry_id` wins, selected by the topological depth of the leaf it
    /// was recorded at (deepest descendant wins, WAL order breaks ties) — the same rule
    /// [`SessionJournal::active_entries`] uses — so an ancestor overlay recorded after a `--fork`
    /// cannot shadow a descendant's, and a rewound entry's links are never returned.
    pub fn evidence_links(&self) -> Vec<EvidenceLink> {
        let depth_map = self.active_depth_map();
        let active_len = self.active_entries().len();
        let mut winner: HashMap<&str, (usize, usize)> = HashMap::new();
        for (wal_index, update) in self.projection_updates.iter().enumerate() {
            let Some(&depth) = depth_map.get(&update.projected_at_leaf_id) else { continue; };
            let slot = winner.entry(update.entry_id.as_str()).or_insert((0, 0));
            if (depth, wal_index) >= *slot {
                *slot = (depth, wal_index);
            }
        }
        let mut links: Vec<EvidenceLink> = Vec::new();
        for (wal_index, update) in self.projection_updates.iter().enumerate() {
            let Some(&depth) = depth_map.get(&update.projected_at_leaf_id) else { continue; };
            if winner.get(update.entry_id.as_str()) != Some(&(depth, wal_index)) {
                continue;
            }
            for link in &update.evidence_links {
                // A link pointing past the active transcript belongs to a rewound branch.
                if link.canonical_index >= active_len {
                    continue;
                }
                if !links.iter().any(|existing| existing.source_entry_id == link.source_entry_id) {
                    links.push(link.clone());
                }
            }
        }
        links.sort_by(|a, b| {
            a.canonical_index
                .cmp(&b.canonical_index)
                .then_with(|| a.source_entry_id.cmp(&b.source_entry_id))
        });
        links
    }

    /// The pre-projection (raw) item for an entry, ignoring any append-only compaction overlay.
    /// `recall_evidence` uses this to recover the raw tool output a stub replaced.
    pub fn raw_entry_item(&self, entry_id: &str) -> Option<&TranscriptItem> {
        self.entries.get(entry_id).map(|entry| &entry.item)
    }

    /// True when `entry_id` is an ancestor of (or equal to) the currently selected leaf.
    pub fn is_entry_on_active_branch(&self, entry_id: &str) -> bool {
        self.active_depth_map().contains_key(entry_id)
    }

    /// Read-only header inspection used by `--resume latest` and workspace inference so scanning
    /// candidate WAL files never acquires exclusive write locks, never mutates permissions, and
    /// never truncates trailing bytes on unselected files.
    pub fn peek_workspace_root(path: impl AsRef<Path>) -> Result<PathBuf, SessionError> {
        let path = path.as_ref();
        let sym_meta = fs::symlink_metadata(path)?;
        if sym_meta.file_type().is_symlink() || !sym_meta.file_type().is_file() {
            return Err(SessionError::Invalid(
                "session WAL candidate is not a regular file".into(),
            ));
        }
        if sym_meta.len() == 0 || sym_meta.len() > MAX_WAL_FILE_BYTES {
            return Err(SessionError::Invalid(
                "session WAL candidate size out of bounds".into(),
            ));
        }
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)?;
        let opened_meta = file.metadata()?;
        if !opened_meta.file_type().is_file()
            || (sym_meta.dev(), sym_meta.ino()) != (opened_meta.dev(), opened_meta.ino())
        {
            return Err(SessionError::Invalid(
                "session WAL candidate changed identity".into(),
            ));
        }
        let mut reader = BufReader::new(file.take(MAX_WAL_LINE_BYTES as u64));
        let mut first_line = Vec::new();
        let n = reader.read_until(b'\n', &mut first_line)?;
        if n == 0 || !first_line.ends_with(b"\n") {
            return Err(SessionError::Invalid("missing complete WAL header".into()));
        }
        let line: WalLine = serde_json::from_slice(&first_line[..first_line.len() - 1])
            .map_err(|e| SessionError::Invalid(format!("invalid WAL header JSON: {}", e)))?;
        if line.seq != 0 || line.prev_hash != ZERO_HASH {
            return Err(SessionError::Invalid("invalid WAL header sequence".into()));
        }
        let computed = calculate_hash(line.seq, &line.prev_hash, &line.payload)?;
        if line.hash != computed {
            return Err(SessionError::Invalid("WAL header hash mismatch".into()));
        }
        match line.payload {
            WalPayload::Header {
                version,
                workspace_root,
                ..
            } if version == WAL_VERSION => Ok(workspace_root),
            _ => Err(SessionError::Invalid("first WAL entry is not a valid header".into())),
        }
    }

    /// Latest checkpoint whose transcript leaf is on the currently selected branch.
    ///
    /// Selection is ordered by topological depth on the active path (the deepest descendant wins)
    /// with WAL append order as the tie-breaker. This prevents a checkpoint recorded at an ancestor
    /// after a `--fork` from shadowing a deeper checkpoint once the descendant leaf is re-selected.
    pub fn latest_active_checkpoint(&self) -> Option<&SessionCheckpoint> {
        let depth_map = self.active_depth_map();
        self.checkpoints
            .iter()
            .enumerate()
            .filter_map(|(wal_index, checkpoint)| {
                let leaf = checkpoint.leaf_id.as_deref()?;
                let depth = *depth_map.get(leaf)?;
                Some((depth, wal_index, checkpoint))
            })
            .max_by_key(|(depth, wal_index, _)| (*depth, *wal_index))
            .map(|(_, _, checkpoint)| checkpoint)
    }

    pub fn max_active_turn(&self) -> usize {
        self.active_entries().last().map(|e| e.turn).unwrap_or(0)
    }

    /// Entry ID of the most recent completion resolution still on the selected branch.
    ///
    /// As with checkpoints, the deepest descendant on the active path wins and WAL order breaks
    /// ties, so an ancestor resolution recorded after a fork cannot mask a descendant's.
    pub fn latest_active_completion_resolution(&self) -> Option<&str> {
        let depth_map = self.active_depth_map();
        self.completion_resolution_leaf_ids
            .iter()
            .enumerate()
            .filter_map(|(wal_index, id)| {
                let depth = *depth_map.get(id.as_str())?;
                Some((depth, wal_index, id))
            })
            .max_by_key(|(depth, wal_index, _)| (*depth, *wal_index))
            .map(|(_, _, id)| id.as_str())
    }

    /// Persisted CompletionGate rejection prompts since the last resolved completion. These
    /// preserve the anti-oscillation retry budget across an explicit process restart.
    pub fn completion_feedback_since_last_resolution(&self) -> Vec<String> {
        let active = self.active_entries();
        let start = self.latest_active_completion_resolution()
            .and_then(|resolution_id| active.iter().position(|entry| entry.id == resolution_id))
            .map(|index| index + 1)
            .unwrap_or(0);
        active.iter().skip(start).filter_map(|entry| match &entry.item {
            TranscriptItem::User(user) if user.content.starts_with("[CompletionGate Rejected, attempt ") => {
                Some(user.content.clone())
            }
            _ => None,
        }).collect()
    }

    /// Modified files after the last resolved completion. This keeps interrupted work dirty while
    /// allowing later no-change tasks to short-circuit verification.
    pub fn modified_files_since_last_resolution(&self) -> Vec<String> {
        let active = self.active_entries();
        let start = self.latest_active_completion_resolution()
            .and_then(|resolution_id| active.iter().position(|entry| entry.id == resolution_id))
            .map(|index| index + 1)
            .unwrap_or(0);
        let mut files = std::collections::BTreeSet::new();
        for entry in active.iter().skip(start) {
            if let TranscriptItem::ToolResult(result) = &entry.item {
                if result.outcome.is_success() && result.meta.modified_file {
                    if let Some(path) = &result.meta.relative_path {
                        files.insert(path.clone());
                    }
                }
            }
        }
        files.into_iter().collect()
    }

    /// Mark the current transcript leaf verified after CompletionGate approved the run.
    pub fn mark_completion_approved(&mut self) -> Result<(), SessionError> {
        self.mark_completion_resolved(CompletionResolution::Verified)
    }

    /// Record an intentional validation skip without representing it as a successful test run.
    pub fn mark_completion_skipped(&mut self, reason: impl Into<String>) -> Result<(), SessionError> {
        self.mark_completion_resolved(CompletionResolution::Skipped(reason.into()))
    }

    fn mark_completion_resolved(&mut self, resolution: CompletionResolution) -> Result<(), SessionError> {
        let leaf_id = self.leaf_id.clone()
            .ok_or_else(|| SessionError::Invalid("cannot resolve completion with no active transcript leaf".into()))?;
        self.append_payload(WalPayload::CompletionResolved { leaf_id: leaf_id.clone(), resolution })?;
        self.completion_resolution_leaf_ids.push(leaf_id);
        Ok(())
    }

    /// Add a single transcript event. Each item is flushed to disk before returning.
    pub fn append_item(&mut self, turn: usize, item: TranscriptItem) -> Result<String, SessionError> {
        let id = Uuid::new_v4().to_string();
        let entry = SessionEntry {
            id: id.clone(),
            parent_id: self.leaf_id.clone(),
            turn,
            item,
        };
        self.append_payload(WalPayload::Append { entry: entry.clone() })?;
        self.leaf_id = Some(id.clone());
        self.entries.insert(id.clone(), entry);
        Ok(id)
    }

    /// Move the active leaf to any existing entry, preserving all abandoned branches in the WAL.
    pub fn fork_at(&mut self, entry_id: &str) -> Result<(), SessionError> {
        if !self.entries.contains_key(entry_id) {
            return Err(SessionError::Invalid(format!("no session entry named {}", entry_id)));
        }
        self.append_payload(WalPayload::SetLeaf { leaf_id: entry_id.to_string() })?;
        self.leaf_id = Some(entry_id.to_string());
        Ok(())
    }

    /// Select the last event at or before a turn on the active path, then continue as a branch.
    pub fn fork_at_turn(&mut self, turn: usize) -> Result<String, SessionError> {
        let selected = self.active_entries().into_iter().rev()
            .find(|entry| entry.turn <= turn)
            .map(|entry| entry.id.clone())
            .ok_or_else(|| SessionError::Invalid(format!("no active entry at or before turn {}", turn)))?;
        self.fork_at(&selected)?;
        Ok(selected)
    }

    pub fn active_transcript(&self) -> Vec<TranscriptItem> {
        self.active_entries().into_iter().map(|entry| entry.item.clone()).collect()
    }

    /// Persist compaction-induced changes to the in-memory canonical transcript without rewriting
    /// old WAL bytes. The active projection is materialized by replaying these append-only overlays.
    /// Each overlay also carries the evidence links that bind its Outline Stub back to the raw
    /// tool output it replaced (Gate 2).
    pub fn sync_projected_transcript(
        &mut self,
        transcript: &[TranscriptItem],
        evidence_links: &[EvidenceLink],
    ) -> Result<(), SessionError> {
        let active_entries = self.active_entries();
        if active_entries.len() != transcript.len() {
            return Err(SessionError::Invalid(format!(
                "projection/transcript length mismatch: WAL has {}, in-memory transcript has {}",
                active_entries.len(), transcript.len()
            )));
        }
        let Some(projected_at_leaf_id) = self.leaf_id.clone() else {
            if transcript.is_empty() { return Ok(()); }
            return Err(SessionError::Invalid("cannot persist projection without an active leaf".into()));
        };
        for (index, (entry, item)) in active_entries.into_iter().zip(transcript.iter()).enumerate() {
            if &entry.item != item {
                let links: Vec<EvidenceLink> = evidence_links
                    .iter()
                    .filter(|link| link.canonical_index == index)
                    .cloned()
                    .collect();
                let overlay = ProjectionOverlay {
                    projected_at_leaf_id: projected_at_leaf_id.clone(),
                    entry_id: entry.id.clone(),
                    item: item.clone(),
                    evidence_links: links.clone(),
                };
                self.append_payload(WalPayload::ProjectionUpdate {
                    projected_at_leaf_id: overlay.projected_at_leaf_id.clone(),
                    entry_id: overlay.entry_id.clone(),
                    item: overlay.item.clone(),
                    evidence_links: links,
                })?;
                self.projection_updates.push(overlay);
            }
        }
        Ok(())
    }

    pub fn active_entries(&self) -> Vec<SessionEntry> {
        let mut path = self.active_path_entries();
        let depth_map = self.active_depth_map();
        // Projection overlays are append-only. Select one overlay per entry by the topological
        // depth of the leaf it was recorded at (deepest descendant wins, WAL order breaks ties),
        // so an ancestor overlay cannot overwrite a descendant's after forking and re-selecting.
        let mut projected_items: HashMap<String, (&TranscriptItem, usize)> = HashMap::new();
        for update in &self.projection_updates {
            let Some(&leaf_depth) = depth_map.get(&update.projected_at_leaf_id) else { continue; };
            if !depth_map.contains_key(&update.entry_id) {
                continue;
            }
            let replace = match projected_items.get(&update.entry_id) {
                Some((_, existing_depth)) => leaf_depth >= *existing_depth,
                None => true,
            };
            if replace {
                projected_items.insert(update.entry_id.clone(), (&update.item, leaf_depth));
            }
        }
        for entry in &mut path {
            if let Some((item, _)) = projected_items.get(&entry.id) {
                entry.item = (**item).clone();
            }
        }
        path
    }

    /// Root-to-leaf entries on the currently selected branch.
    fn active_path_entries(&self) -> Vec<SessionEntry> {
        let mut path = Vec::new();
        let mut next = self.leaf_id.as_deref();
        while let Some(id) = next {
            let Some(entry) = self.entries.get(id) else { break; };
            path.push(entry.clone());
            next = entry.parent_id.as_deref();
        }
        path.reverse();
        path
    }

    /// Active-path entry IDs mapped to their topological depth (root = 0, leaf = deepest).
    fn active_depth_map(&self) -> HashMap<String, usize> {
        let mut path = Vec::new();
        let mut next = self.leaf_id.as_deref();
        while let Some(id) = next {
            let Some(entry) = self.entries.get(id) else { break; };
            path.push(id.to_string());
            next = entry.parent_id.as_deref();
        }
        path.reverse();
        path.into_iter().enumerate().map(|(index, id)| (id, index)).collect()
    }

    /// Persist replay-relevant projector metadata alongside the append-only transcript.
    pub fn checkpoint(
        &mut self,
        turn: usize,
        pinned_baseline_end: usize,
        last_touched: Vec<FileTouch>,
        reasoning_blocks_sha256: impl Into<String>,
    ) -> Result<(), SessionError> {
        self.checkpoint_with_revision(
            turn,
            pinned_baseline_end,
            last_touched,
            reasoning_blocks_sha256,
            None,
            Vec::new(),
            Vec::new(),
        )
    }

    /// Checkpoint variant that also binds the workspace revision captured before this turn (Gate 1),
    /// the ancestor directories whose JIT guidance is already in context (Gate 3), and the branch's
    /// mutation ledger (Gate 4).
    pub fn checkpoint_with_revision(
        &mut self,
        turn: usize,
        pinned_baseline_end: usize,
        last_touched: Vec<FileTouch>,
        reasoning_blocks_sha256: impl Into<String>,
        workspace_revision_id: Option<String>,
        subtree_dirs: Vec<String>,
        mutations: Vec<MutationRecord>,
    ) -> Result<(), SessionError> {
        self.checkpoint_with_subagents(
            turn,
            pinned_baseline_end,
            last_touched,
            reasoning_blocks_sha256,
            workspace_revision_id,
            subtree_dirs,
            mutations,
            0,
        )
    }

    /// Checkpoint variant that also records the cumulative Gate 5 `subagents_spawned` count on the
    /// active branch so `--resume` and `--fork` preserve subagent IDs and spawn budgets.
    #[allow(clippy::too_many_arguments)]
    pub fn checkpoint_with_subagents(
        &mut self,
        turn: usize,
        pinned_baseline_end: usize,
        mut last_touched: Vec<FileTouch>,
        reasoning_blocks_sha256: impl Into<String>,
        workspace_revision_id: Option<String>,
        mut subtree_dirs: Vec<String>,
        mutations: Vec<MutationRecord>,
        subagents_spawned: usize,
    ) -> Result<(), SessionError> {
        last_touched.sort_by(|a, b| a.path.cmp(&b.path));
        subtree_dirs.sort();
        subtree_dirs.dedup();
        let checkpoint = SessionCheckpoint {
            leaf_id: self.leaf_id.clone(),
            turn,
            pinned_baseline_end,
            last_touched,
            reasoning_blocks_sha256: reasoning_blocks_sha256.into(),
            workspace_revision_id,
            subtree_dirs,
            mutations,
            subagents_spawned,
        };
        self.append_payload(WalPayload::Checkpoint {
            leaf_id: checkpoint.leaf_id.clone(),
            turn: checkpoint.turn,
            pinned_baseline_end: checkpoint.pinned_baseline_end,
            last_touched: checkpoint.last_touched.clone(),
            reasoning_blocks_sha256: checkpoint.reasoning_blocks_sha256.clone(),
            workspace_revision_id: checkpoint.workspace_revision_id.clone(),
            subtree_dirs: checkpoint.subtree_dirs.clone(),
            mutations: checkpoint.mutations.clone(),
            subagents_spawned: checkpoint.subagents_spawned,
        })?;
        self.checkpoints.push(checkpoint);
        Ok(())
    }

    /// Record a captured `WorkspaceRevision` against the current transcript leaf (Gate 1).
    pub fn record_revision_capture(
        &mut self,
        turn: usize,
        revision_id: &str,
        parent_revision_id: Option<&str>,
        changed_paths: Vec<String>,
        tracked_file_count: usize,
    ) -> Result<(), SessionError> {
        let record = RevisionRecord {
            leaf_id: self.leaf_id.clone(),
            turn,
            revision_id: revision_id.to_string(),
            parent_revision_id: parent_revision_id.map(|s| s.to_string()),
            changed_paths,
            tracked_file_count,
            captured_at_ms: now_ms(),
        };
        self.append_payload(WalPayload::RevisionCaptured {
            leaf_id: record.leaf_id.clone(),
            turn: record.turn,
            revision_id: record.revision_id.clone(),
            parent_revision_id: record.parent_revision_id.clone(),
            changed_paths: record.changed_paths.clone(),
            tracked_file_count: record.tracked_file_count,
            captured_at_ms: record.captured_at_ms,
        })?;
        self.revision_records.push(record);
        Ok(())
    }

    /// Workspace revisions captured on the currently selected branch, ordered by the topological
    /// depth of the leaf they were bound to (root first, deepest descendant last), with WAL append
    /// order as the tie-breaker. Ordering by depth rather than raw append order means an ancestor
    /// capture recorded *after* a fork can never shadow a deeper descendant's revision (O4), so
    /// `latest_revision_id()` always names the workspace state on the active branch.
    pub fn revision_records(&self) -> Vec<RevisionRecord> {
        let depth_map = self.active_depth_map();
        let mut ordered: Vec<(usize, usize, RevisionRecord)> = self
            .revision_records
            .iter()
            .enumerate()
            .filter_map(|(append_index, record)| {
                let depth = match record.leaf_id.as_deref() {
                    Some(leaf) => *depth_map.get(leaf)?,
                    None => 0,
                };
                Some((depth, append_index, record.clone()))
            })
            .collect();
        ordered.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
        ordered.into_iter().map(|(_, _, record)| record).collect()
    }

    /// Revision the workspace is expected to be in right now, per the active branch.
    pub fn latest_revision_id(&self) -> Option<String> {
        self.revision_records().last().map(|r| r.revision_id.clone())
    }

    fn append_payload(&mut self, payload: WalPayload) -> Result<(), SessionError> {
        let hash = calculate_hash(self.next_seq, &self.last_hash, &payload)?;
        let line = WalLine {
            seq: self.next_seq,
            prev_hash: self.last_hash.clone(),
            hash: hash.clone(),
            payload,
        };
        let mut bytes = serde_json::to_vec(&line)?;
        bytes.push(b'\n');
        let pre_write_len = self.file.metadata()?.len();
        if let Err(e) = self.file.write_all(&bytes) {
            let _ = self.file.set_len(pre_write_len);
            return Err(e.into());
        }
        if let Err(e) = self.file.flush() {
            let _ = self.file.set_len(pre_write_len);
            return Err(e.into());
        }
        if let Err(e) = self.file.sync_data() {
            let _ = self.file.set_len(pre_write_len);
            return Err(e.into());
        }
        self.last_hash = hash;
        self.next_seq += 1;
        Ok(())
    }
}

impl Drop for SessionJournal {
    fn drop(&mut self) {
        // Explicitly release the `flock` on the open file description before closing the FD so a
        // concurrent thread between `fork()` and `execve()` cannot keep the lock alive until exec.
        unsafe {
            let _ = libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

/// Take a non-blocking exclusive `flock` so two processes cannot append to the same WAL.
/// The lock is tied to the open file description, so it must be held on the handle that the
/// `SessionJournal` retains for the lifetime of the session.
fn acquire_wal_lock(file: &File) -> Result<(), SessionError> {
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        return Err(SessionError::Invalid("session WAL is locked by another process".into()));
    }
    Ok(())
}

fn calculate_hash(seq: u64, prev_hash: &str, payload: &WalPayload) -> Result<String, SessionError> {
    let payload_bytes = serde_json::to_vec(payload)?;
    let mut hasher = Sha256::new();
    hasher.update(seq.to_be_bytes());
    hasher.update(prev_hash.as_bytes());
    hasher.update(payload_bytes);
    Ok(format!("{:x}", hasher.finalize()))
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::messages::{AssistantTurn, FinishReason, ReasoningBlock, UserMessage};

    fn tempdir() -> PathBuf {
        let p = std::env::temp_dir().join(format!("harness-session-test-{}", Uuid::new_v4()));
        fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn wal_round_trips_signed_reasoning_and_branching() {
        let dir = tempdir();
        let mut wal = SessionJournal::create(&dir, "/tmp/workspace").unwrap();
        let first = wal.append_item(0, TranscriptItem::User(UserMessage { content: "start".into() })).unwrap();
        wal.append_item(1, TranscriptItem::Assistant(AssistantTurn {
            content: Some("one".into()),
            reasoning_blocks: vec![ReasoningBlock { text: None, signature: Some("opaque-signature".into()), opaque_payload: Some("provider-payload".into()) }],
            tool_calls: vec![], finish_reason: FinishReason::Stop, usage: None,
        })).unwrap();
        let path = wal.path().to_path_buf();
        drop(wal);

        let mut wal = SessionJournal::open(&path).unwrap();
        let round_trip = wal.active_transcript();
        assert!(matches!(&round_trip[1], TranscriptItem::Assistant(a)
            if a.reasoning_blocks[0].signature.as_deref() == Some("opaque-signature")
            && a.reasoning_blocks[0].opaque_payload.as_deref() == Some("provider-payload")));
        wal.append_item(2, TranscriptItem::User(UserMessage { content: "second branch".into() })).unwrap();
        wal.fork_at(&first).unwrap();
        wal.append_item(1, TranscriptItem::User(UserMessage { content: "alternate".into() })).unwrap();
        drop(wal);

        let reopened = SessionJournal::open(path).unwrap();
        let transcript = reopened.active_transcript();
        assert_eq!(transcript.len(), 2);
        assert!(matches!(&transcript[1], TranscriptItem::User(u) if u.content == "alternate"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn projection_updates_are_append_only_and_replayed() {
        use crate::messages::{ToolOutcome, ToolResultMessage, ToolResultMeta};
        use std::sync::Arc;
        let dir = tempdir();
        let mut wal = SessionJournal::create(&dir, "/tmp/workspace").unwrap();
        wal.append_item(0, TranscriptItem::User(UserMessage { content: "start".into() })).unwrap();
        let result_id = wal.append_item(1, TranscriptItem::ToolResult(ToolResultMessage {
            call_id: crate::types::ToolCallId::new("call_1"),
            tool_name: "read".into(),
            outcome: ToolOutcome::Success,
            content: Arc::from("x".repeat(4096)),
            meta: ToolResultMeta::default(),
        })).unwrap();
        wal.append_item(2, TranscriptItem::Assistant(AssistantTurn {
            content: Some("follow-up".into()), reasoning_blocks: vec![], tool_calls: vec![],
            finish_reason: FinishReason::Stop, usage: None,
        })).unwrap();
        let mut projected = wal.active_transcript();
        if let TranscriptItem::ToolResult(tr) = &mut projected[1] {
            tr.content = Arc::from("<outline stub>");
        }
        wal.sync_projected_transcript(&projected, &[]).unwrap();
        let path = wal.path().to_path_buf();
        drop(wal);
        let mut reopened = SessionJournal::open(path).unwrap();
        assert!(matches!(&reopened.active_transcript()[1], TranscriptItem::ToolResult(tr) if &*tr.content == "<outline stub>"));
        reopened.fork_at(&result_id).unwrap();
        assert!(matches!(&reopened.active_transcript()[1], TranscriptItem::ToolResult(tr) if tr.content.len() == 4096));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn approved_completion_clears_dirty_files_until_a_new_mutation() {
        use crate::messages::{ToolOutcome, ToolResultMessage, ToolResultMeta};
        use std::sync::Arc;
        let dir = tempdir();
        let mut wal = SessionJournal::create(&dir, "/tmp/workspace").unwrap();
        wal.append_item(0, TranscriptItem::User(UserMessage { content: "implement".into() })).unwrap();
        wal.append_item(1, TranscriptItem::ToolResult(ToolResultMessage {
            call_id: crate::types::ToolCallId::new("call_edit"),
            tool_name: "edit".into(),
            outcome: ToolOutcome::Success,
            content: Arc::from("changed"),
            meta: ToolResultMeta { relative_path: Some("src/lib.rs".into()), modified_file: true, ..Default::default() },
        })).unwrap();
        wal.append_item(2, TranscriptItem::Assistant(AssistantTurn {
            content: Some("done".into()), reasoning_blocks: vec![], tool_calls: vec![],
            finish_reason: FinishReason::Stop, usage: None,
        })).unwrap();
        assert_eq!(wal.modified_files_since_last_resolution(), vec!["src/lib.rs"]);
        wal.mark_completion_approved().unwrap();
        wal.append_item(3, TranscriptItem::User(UserMessage { content: "explain this code".into() })).unwrap();
        assert!(wal.modified_files_since_last_resolution().is_empty());
        wal.append_item(4, TranscriptItem::ToolResult(ToolResultMessage {
            call_id: crate::types::ToolCallId::new("call_edit_again"),
            tool_name: "edit".into(),
            outcome: ToolOutcome::Success,
            content: Arc::from("changed again"),
            meta: ToolResultMeta { relative_path: Some("src/other.rs".into()), modified_file: true, ..Default::default() },
        })).unwrap();
        assert_eq!(wal.modified_files_since_last_resolution(), vec!["src/other.rs"]);
        let path = wal.path().to_path_buf();
        drop(wal);
        let reopened = SessionJournal::open(path).unwrap();
        assert_eq!(reopened.modified_files_since_last_resolution(), vec!["src/other.rs"]);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn gate_rejections_survive_resume_and_clear_after_resolution() {
        let dir = tempdir();
        let mut wal = SessionJournal::create(&dir, "/tmp/workspace").unwrap();
        wal.append_item(0, TranscriptItem::User(UserMessage { content: "implement".into() })).unwrap();
        wal.append_item(1, TranscriptItem::Assistant(AssistantTurn {
            content: Some("done".into()), reasoning_blocks: vec![], tool_calls: vec![],
            finish_reason: FinishReason::Stop, usage: None,
        })).unwrap();
        let rejection = "[CompletionGate Rejected, attempt 1/3]: error: assertion failed\nFix the reported issue before finishing.";
        wal.append_item(2, TranscriptItem::User(UserMessage { content: rejection.into() })).unwrap();
        assert_eq!(wal.completion_feedback_since_last_resolution().len(), 1);
        let path = wal.path().to_path_buf();
        drop(wal);

        let mut reopened = SessionJournal::open(&path).unwrap();
        assert_eq!(reopened.completion_feedback_since_last_resolution().len(), 1);
        reopened.append_item(3, TranscriptItem::Assistant(AssistantTurn {
            content: Some("fixed".into()), reasoning_blocks: vec![], tool_calls: vec![],
            finish_reason: FinishReason::Stop, usage: None,
        })).unwrap();
        reopened.mark_completion_approved().unwrap();
        assert!(reopened.completion_feedback_since_last_resolution().is_empty());
        drop(reopened);

        let reopened = SessionJournal::open(&path).unwrap();
        assert!(reopened.completion_feedback_since_last_resolution().is_empty());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn wal_repairs_only_incomplete_trailing_line() {
        let dir = tempdir();
        let wal = SessionJournal::create(&dir, "/tmp/workspace").unwrap();
        let path = wal.path().to_path_buf();
        drop(wal);
        OpenOptions::new().append(true).open(&path).unwrap().write_all(b"{\"torn\":").unwrap();
        let opened = SessionJournal::open(&path).unwrap();
        assert_eq!(opened.max_active_turn(), 0);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn non_wal_text_file_is_rejected_without_modification() {
        let dir = tempdir();
        let target = dir.join("README.md");
        let original = b"# Title\n\nline two\nno trailing newline here".to_vec();
        fs::write(&target, &original).unwrap();
        let before = crate::prompt::sha256_hex64(&original);

        assert!(
            SessionJournal::open(&target).is_err(),
            "opening a non-WAL text file must fail"
        );

        let after = fs::read(&target).unwrap();
        assert_eq!(after, original, "rejected WAL open must not modify the file");
        assert_eq!(after.len(), original.len());
        assert_eq!(crate::prompt::sha256_hex64(&after), before);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn single_line_file_without_newline_is_rejected_without_modification() {
        let dir = tempdir();
        let target = dir.join("notes.txt");
        let original = b"just one line, no newline".to_vec();
        fs::write(&target, &original).unwrap();
        assert!(SessionJournal::open(&target).is_err());
        assert_eq!(fs::read(&target).unwrap(), original);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn symlink_wal_is_rejected_and_target_untouched() {
        let dir = tempdir();
        let wal = SessionJournal::create(&dir, "/tmp/workspace").unwrap();
        let wal_path = wal.path().to_path_buf();
        drop(wal);
        let real = dir.join("real.wal.jsonl");
        fs::rename(&wal_path, &real).unwrap();
        let link = dir.join("link.wal.jsonl");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let before = fs::read(&real).unwrap();

        assert!(
            SessionJournal::open(&link).is_err(),
            "symlink WAL must be rejected"
        );
        assert_eq!(
            fs::read(&real).unwrap(),
            before,
            "symlink target must be untouched"
        );
        let _ = fs::remove_dir_all(dir);
    }

    /// F-01: a `--resume` path that resolves through a symlink to a file outside the session store
    /// must be refused, and the external target must remain byte-for-byte unchanged.
    #[test]
    fn symlink_to_external_file_is_rejected_and_target_untouched() {
        let dir = tempdir();
        let outside = dir.join("outside_secret.txt");
        let original = b"external file contents, must never be touched\n".to_vec();
        fs::write(&outside, &original).unwrap();
        let before = crate::prompt::sha256_hex64(&original);

        let link = dir.join("link.wal.jsonl");
        std::os::unix::fs::symlink(&outside, &link).unwrap();

        assert!(
            SessionJournal::open(&link).is_err(),
            "a symlink to an external file must be rejected"
        );
        let after = fs::read(&outside).unwrap();
        assert_eq!(after, original, "external target must be untouched");
        assert_eq!(crate::prompt::sha256_hex64(&after), before);
        let _ = fs::remove_dir_all(dir);
    }

    /// F-01: a file larger than the WAL cap must be rejected up front (before any read/truncate),
    /// leaving the original bytes and length unchanged. A sparse file keeps the test cheap.
    #[test]
    fn oversized_wal_candidate_is_rejected_without_modification() {
        let dir = tempdir();
        let target = dir.join("huge.wal.jsonl");
        let f = File::create(&target).unwrap();
        f.set_len(MAX_WAL_FILE_BYTES + 1).unwrap();
        drop(f);
        let before_len = fs::metadata(&target).unwrap().len();

        assert!(
            SessionJournal::open(&target).is_err(),
            "an oversized WAL candidate must be rejected"
        );
        assert_eq!(
            fs::metadata(&target).unwrap().len(),
            before_len,
            "a rejected oversized WAL must not be truncated or resized"
        );
        let _ = fs::remove_dir_all(dir);
    }
}
