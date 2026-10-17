//! T15 (KR-11): layered evidence storage with an explicit availability contract.
//!
//! The in-memory evidence map is a *cache*, not the record. Dropping an entry from it because the
//! run's 16 MiB budget is full must not mean the raw bytes are gone — but it must also not mean
//! "assume they are there". This module is the durable layer, and it answers with a status rather
//! than an `Option`:
//!
//! * [`ArtifactStatus::Available`] — bytes verified against their content hash.
//! * [`ArtifactStatus::Gone`] — never written, deleted by the user, or collected by GC.
//! * [`ArtifactStatus::Corrupt`] — present but the bytes do not hash to the key's digest.
//! * [`ArtifactStatus::Forbidden`] — present but the file or its directory is not private to the
//!   current user, so the bytes cannot be trusted to be the ones the host wrote.
//!
//! The key carries session, branch, event and content hash. The branch component is what stops a
//! `--fork` from resolving its sibling's evidence, and the hash component is what stops a file that
//! happens to have the same name from impersonating an older capture.

use crate::messages::CompactionAvailability;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Hard ceiling for one artifact. Evidence is tool output that was already capped at capture time;
/// anything larger is a bug or an attack, and either way it must not be read into memory.
pub const MAX_ARTIFACT_BYTES: u64 = 32 * 1024 * 1024;

/// Stable identity of one raw evidence object.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ArtifactKey {
    pub session_id: String,
    /// Leaf id of the branch the evidence was captured on. A fork gets a new leaf, so a sibling
    /// branch simply has a different key.
    pub branch_id: String,
    /// Canonical transcript entry this evidence belongs to.
    pub event_id: String,
    /// sha256 of the raw bytes.
    pub sha256: String,
}

impl ArtifactKey {
    pub fn new(
        session_id: impl Into<String>,
        branch_id: impl Into<String>,
        event_id: impl Into<String>,
        sha256: impl Into<String>,
    ) -> Self {
        Self {
            session_id: session_id.into(),
            branch_id: branch_id.into(),
            event_id: event_id.into(),
            sha256: sha256.into(),
        }
    }

    /// `<root>/<session>/<branch>/<event>-<hash>.evidence`
    pub fn relative_path(&self) -> PathBuf {
        let hash: String = self.sha256.chars().take(64).collect();
        PathBuf::from(&self.session_id)
            .join(&self.branch_id)
            .join(format!("{}-{}.evidence", sanitize(&self.event_id), hash))
    }
}

fn sanitize(component: &str) -> String {
    component
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// What a lookup actually found. There is deliberately no `bool` variant that could be optimistically
/// set: every state here is one the caller must handle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArtifactStatus {
    Available { text: String, bytes: u64 },
    Gone,
    Corrupt { reason: String },
    Forbidden { reason: String },
}

impl ArtifactStatus {
    pub fn is_available(&self) -> bool {
        matches!(self, ArtifactStatus::Available { .. })
    }

    /// Map onto the stub's own availability label (T05).
    pub fn availability(&self) -> CompactionAvailability {
        match self {
            ArtifactStatus::Available { .. } => CompactionAvailability::Available,
            ArtifactStatus::Gone => CompactionAvailability::Evicted,
            // Corrupt and forbidden are "present but unusable": the bytes exist, they just cannot be
            // trusted, which for the model is the same as not having them.
            ArtifactStatus::Corrupt { .. } | ArtifactStatus::Forbidden { .. } => {
                CompactionAvailability::Evicted
            }
        }
    }

    pub fn describe(&self) -> String {
        match self {
            ArtifactStatus::Available { bytes, .. } => format!("available ({} bytes)", bytes),
            ArtifactStatus::Gone => "gone (no artifact is stored for this key)".to_string(),
            ArtifactStatus::Corrupt { reason } => format!("corrupt ({})", reason),
            ArtifactStatus::Forbidden { reason } => format!("forbidden ({})", reason),
        }
    }
}

/// Outcome of a garbage-collection pass.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GcReport {
    pub kept: usize,
    pub removed: usize,
    pub bytes_freed: u64,
    /// Paths that should have been removed but were not, with the reason. A GC that silently skips
    /// files is a GC that quietly leaks.
    pub failed: Vec<(PathBuf, String)>,
}

/// Durable artifact store: one private directory tree, one file per evidence object.
#[derive(Debug, Clone)]
pub struct EvidenceStore {
    root: PathBuf,
}

impl EvidenceStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn path_for(&self, key: &ArtifactKey) -> PathBuf {
        self.root.join(key.relative_path())
    }

    /// Write `raw` under `key`.
    ///
    /// The write is content-addressed: the key's digest must match the bytes, otherwise nothing is
    /// written. Directories are private (`0700`), the file is created fresh (`0600`) and never
    /// follows a symlink, and the data lands via a temporary file plus rename so a crash cannot leave
    /// a half-written artifact that would later read as `Corrupt`.
    pub fn put(&self, key: &ArtifactKey, raw: &str) -> Result<u64, String> {
        let digest = crate::prompt::sha256_hex64(raw.as_bytes());
        if digest != key.sha256 {
            return Err(format!(
                "artifact key digest {} does not match the {} bytes offered",
                key.sha256,
                raw.len()
            ));
        }
        if raw.len() as u64 > MAX_ARTIFACT_BYTES {
            return Err(format!(
                "artifact of {} bytes exceeds the {} byte cap",
                raw.len(),
                MAX_ARTIFACT_BYTES
            ));
        }
        let path = self.path_for(key);
        let parent = path.parent().ok_or("artifact path has no parent")?;
        crate::secure_fs::ensure_private_dir(parent)
            .map_err(|e| format!("cannot create {}: {}", parent.display(), e))?;

        let tmp = parent.join(format!(".tmp-{}-{}", std::process::id(), &digest[..12]));
        {
            use std::io::Write as _;
            let mut file = crate::secure_fs::create_new_no_symlink(&tmp, 0o600)
                .map_err(|e| format!("cannot create {}: {}", tmp.display(), e))?;
            file.write_all(raw.as_bytes())
                .map_err(|e| format!("cannot write {}: {}", tmp.display(), e))?;
            file.sync_all()
                .map_err(|e| format!("cannot sync {}: {}", tmp.display(), e))?;
        }
        std::fs::rename(&tmp, &path).map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            format!("cannot publish {}: {}", path.display(), e)
        })?;
        Ok(raw.len() as u64)
    }

    /// Read the artifact for `key`, verifying it before handing the bytes back.
    ///
    /// Never falls back to the workspace's current file: the only source is the artifact path, so an
    /// edited file cannot masquerade as the evidence that was captured earlier.
    pub fn get(&self, key: &ArtifactKey) -> ArtifactStatus {
        let path = self.path_for(key);
        let parent = match path.parent() {
            Some(p) => p,
            None => return ArtifactStatus::Gone,
        };
        if !parent.is_dir() {
            return ArtifactStatus::Gone;
        }
        // The directory must be private: a world-writable evidence directory means anyone could have
        // put bytes there.
        match std::fs::symlink_metadata(parent) {
            Ok(meta) => {
                if !meta.is_dir() {
                    return ArtifactStatus::Forbidden {
                        reason: format!("{} is not a directory", parent.display()),
                    };
                }
                if !crate::secure_fs::is_private_dir(&meta) {
                    return ArtifactStatus::Forbidden {
                        reason: format!("{} is not private to this user", parent.display()),
                    };
                }
            }
            Err(_) => return ArtifactStatus::Gone,
        }
        let read = match crate::secure_fs::read_bounded_file(
            &path,
            MAX_ARTIFACT_BYTES,
            crate::secure_fs::Overflow::Refuse,
        ) {
            Ok(read) => read,
            Err(crate::secure_fs::SecureFsError::Io(e))
                if e.kind() == std::io::ErrorKind::NotFound =>
            {
                return ArtifactStatus::Gone
            }
            Err(crate::secure_fs::SecureFsError::NotRegularFile(display)) => {
                return ArtifactStatus::Forbidden {
                    reason: format!("{} is not a regular file", display),
                }
            }
            Err(e) => {
                return ArtifactStatus::Corrupt {
                    reason: e.to_string(),
                }
            }
        };
        let digest = crate::prompt::sha256_hex64(&read.bytes);
        if digest != key.sha256 {
            return ArtifactStatus::Corrupt {
                reason: format!(
                    "content hashes to {} but the key records {}",
                    &digest[..12],
                    &key.sha256[..12.min(key.sha256.len())]
                ),
            };
        }
        match String::from_utf8(read.bytes) {
            Ok(text) => ArtifactStatus::Available {
                bytes: text.len() as u64,
                text,
            },
            Err(_) => ArtifactStatus::Corrupt {
                reason: "artifact is not valid UTF-8".to_string(),
            },
        }
    }

    /// Remove every artifact whose key is not in `live`.
    ///
    /// `live` is the manifest: the set of objects the retained sessions still reference. Nothing is
    /// deleted by age alone, because "old" is not the same as "unreferenced".
    pub fn gc(&self, live: &BTreeSet<ArtifactKey>) -> GcReport {
        let mut report = GcReport::default();
        let mut stack = vec![self.root.clone()];
        while let Some(dir) = stack.pop() {
            let entries = match std::fs::read_dir(&dir) {
                Ok(entries) => entries,
                Err(_) => continue,
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let meta = match std::fs::symlink_metadata(&path) {
                    Ok(meta) => meta,
                    Err(e) => {
                        report.failed.push((path, e.to_string()));
                        continue;
                    }
                };
                if meta.is_dir() {
                    stack.push(path);
                    continue;
                }
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let referenced = live.iter().any(|key| {
                    let expected = self.path_for(key);
                    expected == path
                });
                if referenced {
                    report.kept += 1;
                    continue;
                }
                // Temp files from a crashed write are collectable too.
                let size = meta.len();
                match std::fs::remove_file(&path) {
                    Ok(()) => {
                        report.removed += 1;
                        report.bytes_freed = report.bytes_freed.saturating_add(size);
                        let _ = name;
                    }
                    Err(e) => report.failed.push((path, e.to_string())),
                }
            }
        }
        report
    }

    /// Delete one artifact (user request). Afterwards the status is [`ArtifactStatus::Gone`].
    pub fn remove(&self, key: &ArtifactKey) -> ArtifactStatus {
        let path = self.path_for(key);
        match std::fs::remove_file(&path) {
            Ok(()) => ArtifactStatus::Gone,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => ArtifactStatus::Gone,
            Err(e) => ArtifactStatus::Forbidden {
                reason: format!("cannot remove {}: {}", path.display(), e),
            },
        }
    }
}
