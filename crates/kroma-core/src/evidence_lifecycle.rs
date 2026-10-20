//! C00-07: revision-bound facts, pinned evidence, and the boundary of both.
//!
//! Two things the baseline does not have, and one it already has:
//!
//! * **Facts expire.** "cargo test exited 0" was true of *some* tree. `FactBinding` records which
//!   file revisions a fact was observed against and answers `Fresh`/`Stale` against the current
//!   ones. A fact whose subject changed is stale, not silently still true.
//! * **Pins survive GC.** `EvidenceStore::gc` deletes everything the caller does not list as live.
//!   A pin is how a checkpoint or an active task keeps an artifact alive without being on that
//!   list.
//! * **Compaction is already atomic.** It commits through the session WAL, which the crash tests
//!   cover; this module does not re-implement it (see `docs/engineering/LIFECYCLE_C0007.md`).

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use sha2::Digest;

/// Identity of one file revision: cheap fields first, content hash last.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileRevision {
    pub path: String,
    pub len: u64,
    pub mtime_ns: u64,
    pub sha256: String,
}

impl FileRevision {
    /// The revision `relative` has under `root` right now, or `None` when it is gone.
    ///
    /// This is the only way a `Freshness` check learns what the workspace currently looks like, so
    /// it reads the file rather than trusting a cached digest: a cached answer would make every
    /// fact look fresh.
    pub fn of(root: &std::path::Path, relative: &str) -> Option<Self> {
        let path = root.join(relative);
        let metadata = std::fs::metadata(&path).ok()?;
        let bytes = std::fs::read(&path).ok()?;
        Some(Self {
            path: relative.to_string(),
            len: metadata.len(),
            mtime_ns: metadata
                .modified()
                .ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0),
            sha256: format!("{:x}", sha2::Sha256::digest(&bytes)),
        })
    }
}

/// A fact plus the revisions it was observed against.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FactBinding {
    pub fact: String,
    pub subject: BTreeMap<String, FileRevision>,
    /// Revision of the build configuration / toolchain, if the fact depended on one.
    pub environment: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Freshness {
    Fresh,
    /// The subject changed, so the fact must be re-established rather than reused.
    Stale {
        changed: Vec<String>,
    },
    /// A subject is missing entirely; absence is not freshness.
    Missing {
        paths: Vec<String>,
    },
    /// The environment moved (toolchain, build config) without any subject changing.
    EnvironmentMoved {
        expected: String,
        observed: Option<String>,
    },
}

impl FactBinding {
    pub fn new(fact: impl Into<String>) -> Self {
        Self {
            fact: fact.into(),
            subject: BTreeMap::new(),
            environment: None,
        }
    }

    pub fn observing(mut self, revision: FileRevision) -> Self {
        self.subject.insert(revision.path.clone(), revision);
        self
    }

    pub fn in_environment(mut self, environment: impl Into<String>) -> Self {
        self.environment = Some(environment.into());
        self
    }

    /// Compare against what the workspace looks like now.
    ///
    /// `current` is looked up per path; `None` means the file is gone. The environment is checked
    /// first because a toolchain move invalidates the fact even when every subject is unchanged.
    pub fn check(
        &self,
        current: impl Fn(&str) -> Option<FileRevision>,
        environment_now: Option<&str>,
    ) -> Freshness {
        if let Some(expected) = &self.environment {
            let observed = environment_now.map(|value| value.to_string());
            if observed.as_deref() != Some(expected.as_str()) {
                return Freshness::EnvironmentMoved {
                    expected: expected.clone(),
                    observed,
                };
            }
        }

        let mut changed = Vec::new();
        let mut missing = Vec::new();
        for (path, recorded) in &self.subject {
            match current(path) {
                None => missing.push(path.clone()),
                Some(now) => {
                    if now.len != recorded.len
                        || now.mtime_ns != recorded.mtime_ns
                        || now.sha256 != recorded.sha256
                    {
                        changed.push(path.clone());
                    }
                }
            }
        }
        if !missing.is_empty() {
            return Freshness::Missing { paths: missing };
        }
        if !changed.is_empty() {
            return Freshness::Stale { changed };
        }
        Freshness::Fresh
    }
}

/// Why an artifact must not be collected. The reason is carried so a GC report can say *why* it
/// kept something instead of leaving a mystery file behind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pin {
    pub key: String,
    pub reason: String,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PinnedSet {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pins: BTreeMap<String, BTreeSet<String>>,
}

impl PinnedSet {
    pub fn is_empty(&self) -> bool {
        self.pins.is_empty()
    }

    pub fn pin(&mut self, key: impl Into<String>, reason: impl Into<String>) {
        self.pins
            .entry(key.into())
            .or_default()
            .insert(reason.into());
    }

    /// Release one reason. The artifact stays pinned while any other reason remains.
    pub fn release(&mut self, key: &str, reason: &str) -> bool {
        match self.pins.get_mut(key) {
            None => false,
            Some(reasons) => {
                let removed = reasons.remove(reason);
                if reasons.is_empty() {
                    self.pins.remove(key);
                }
                removed
            }
        }
    }

    pub fn is_pinned(&self, key: &str) -> bool {
        self.pins.contains_key(key)
    }

    /// Every key this set currently holds, so a caller can re-check each obligation it agreed to.
    pub fn keys(&self) -> impl Iterator<Item = &String> {
        self.pins.keys()
    }

    pub fn reasons(&self, key: &str) -> Option<&BTreeSet<String>> {
        self.pins.get(key)
    }

    /// The keys GC may consider: everything live minus everything pinned. Pinned keys are reported
    /// separately so the caller can log the decision.
    pub fn partition(&self, live: &BTreeSet<String>) -> (BTreeSet<String>, BTreeSet<String>) {
        let mut collectable = BTreeSet::new();
        let mut kept = BTreeSet::new();
        for key in live {
            if self.is_pinned(key) {
                kept.insert(key.clone());
            } else {
                collectable.insert(key.clone());
            }
        }
        (collectable, kept)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn revision(path: &str, len: u64, sha: &str) -> FileRevision {
        FileRevision {
            path: path.to_string(),
            len,
            mtime_ns: 1_000,
            sha256: sha.to_string(),
        }
    }

    /// C00-07-A1: a fact stops being fresh the moment its subject changes — including the case
    /// where the length and mtime are unchanged and only the bytes differ, and the case where the
    /// subject disappears.
    #[test]
    fn facts_go_stale_when_their_subject_or_environment_moves() {
        let binding = FactBinding::new("cargo test exited 0")
            .observing(revision("src/lib.rs", 100, "aa"))
            .in_environment("rust 1.85.1");

        assert_eq!(
            binding.check(
                |_| Some(revision("src/lib.rs", 100, "aa")),
                Some("rust 1.85.1")
            ),
            Freshness::Fresh
        );
        assert_eq!(
            binding.check(
                |_| Some(revision("src/lib.rs", 100, "bb")),
                Some("rust 1.85.1")
            ),
            Freshness::Stale {
                changed: vec!["src/lib.rs".to_string()]
            },
            "same length and mtime, different bytes is still a change"
        );
        assert_eq!(
            binding.check(|_| None, Some("rust 1.85.1")),
            Freshness::Missing {
                paths: vec!["src/lib.rs".to_string()]
            },
            "a missing subject is not fresh"
        );
        assert_eq!(
            binding.check(
                |_| Some(revision("src/lib.rs", 100, "aa")),
                Some("rust 1.86.0")
            ),
            Freshness::EnvironmentMoved {
                expected: "rust 1.85.1".to_string(),
                observed: Some("rust 1.86.0".to_string()),
            },
            "a toolchain move invalidates the fact even with identical sources"
        );
        assert!(matches!(
            binding.check(|_| Some(revision("src/lib.rs", 100, "aa")), None),
            Freshness::EnvironmentMoved { observed: None, .. }
        ));
    }

    /// C00-07-A3: GC keeps pinned artifacts, and a pin with two reasons survives releasing one.
    #[test]
    fn pins_survive_gc_until_the_last_reason_is_released() {
        let mut pinned = PinnedSet::default();
        pinned.pin("ev-0001", "checkpoint");
        pinned.pin("ev-0001", "active-task");
        pinned.pin("ev-0002", "checkpoint");

        let live: BTreeSet<String> = ["ev-0001", "ev-0002", "ev-0003"]
            .iter()
            .map(|key| key.to_string())
            .collect();
        let (collectable, kept) = pinned.partition(&live);
        assert!(
            collectable.contains("ev-0003"),
            "unpinned live evidence is collectable"
        );
        assert_eq!(kept.len(), 2, "both pinned artifacts are kept");
        assert!(!collectable.contains("ev-0001"));

        assert!(pinned.release("ev-0001", "checkpoint"));
        assert!(
            pinned.is_pinned("ev-0001"),
            "the active-task pin still holds the artifact"
        );
        assert_eq!(
            pinned.reasons("ev-0001").map(|reasons| reasons.len()),
            Some(1)
        );
        assert!(pinned.release("ev-0001", "active-task"));
        assert!(!pinned.is_pinned("ev-0001"));

        assert!(
            !pinned.release("ev-0001", "active-task"),
            "releasing twice is not an error path"
        );
        let (collectable, kept) = pinned.partition(&live);
        assert!(
            collectable.contains("ev-0001"),
            "unpinned evidence becomes collectable"
        );
        assert!(kept.contains("ev-0002"));
    }

    /// The binding round-trips, because a fact's dependencies have to be persisted with it.
    #[test]
    fn bindings_serialise_with_their_dependencies() {
        let binding = FactBinding::new("graph build succeeded")
            .observing(revision("src/a.rs", 10, "cc"))
            .in_environment("rust 1.85.1");
        let json = serde_json::to_string(&binding).unwrap();
        let back: FactBinding = serde_json::from_str(&json).unwrap();
        assert_eq!(back, binding);
        assert!(json.contains("src/a.rs") && json.contains("rust 1.85.1"));
    }
}
