//! B16: provenance for a scored run.
//!
//! A report must be traceable to *what actually ran*, not to a commit id that may not describe the
//! working tree. A dirty or untracked tree is the normal case during development, so recording only
//! `HEAD` would silently attribute results to source that was never built.
//!
//! Everything here is best-effort and explicitly fallible: when git is unavailable or a value
//! cannot be determined, the field stays `None`. Nothing is invented, and a failed probe never
//! fails the run — it degrades the provenance, which the report then shows as unknown.
//!
//! What is deliberately *not* collected: credentials, environment dumps, absolute host paths (they
//! leak layout without adding identity), file contents, and anything outside the source roots.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Directories that are never part of the source identity.
const EXCLUDED_DIRS: &[&str] = &[
    ".git",
    "target",
    "node_modules",
    "checkpoints",
    ".kroma",
    "dist",
    "build",
];

/// Upper bounds so a pathological tree cannot turn provenance into an unbounded read. Exceeding the
/// bound marks the hash as incomplete instead of silently hashing a prefix.
const MAX_SOURCE_FILES: usize = 20_000;
const MAX_SOURCE_BYTES: u64 = 256 * 1024 * 1024;
/// Wall-clock ceiling for the walk. A fingerprint is a convenience and must never stall a run, so an
/// unexpectedly large tree ends as "capped" instead of blocking.
const MAX_WALK_TIME: std::time::Duration = std::time::Duration::from_secs(5);

/// Identity of the code and binary behind a run.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceFingerprint {
    /// `Some(true)` when the working tree differs from `commit`, `Some(false)` when it matches,
    /// `None` when it cannot be determined (no repository, no git binary).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dirty: Option<bool>,
    /// Number of changed/untracked paths reported by the VCS, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changed_paths: Option<usize>,
    /// Revision id, when the tree is a repository.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// Deterministic hash over the source files present on disk. This — not `commit` — is what
    /// identifies a dirty build.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_sha256: Option<String>,
    /// How many files went into `source_sha256`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_files: Option<usize>,
    /// `true` when the source hash stopped at a bound, so it identifies only what was read.
    #[serde(default, skip_serializing_if = "is_false")]
    pub source_hash_capped: bool,
    /// T18 (KR-23): `true` when a VCS probe hit its wall-clock budget. A timed-out probe is not a
    /// clean tree and not "no repository" — it is an unknown, and the report has to say so.
    #[serde(default, skip_serializing_if = "is_false")]
    pub git_probe_timeout: bool,
    /// T18: `true` when the source identity is knowingly partial (unreadable file, capped walk,
    /// unreadable dependency manifest). `source_sha256` is still reported, but it does not identify
    /// the whole tree, so it must not be used as if it did.
    #[serde(default, skip_serializing_if = "is_false")]
    pub source_identity_incomplete: bool,
    /// Why the identity is incomplete, for the report.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub incomplete_reason: Option<String>,
    /// Hash of the running artifact, when it can be located.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary_bytes: Option<u64>,
    /// Resolved dependency set, when the tree pins one (`Cargo.lock`). A binary built with a
    /// different lockfile resolved is a different artifact even at the same source revision, so this
    /// is recorded separately from the source hash. `None` means the ecosystem/library set could not
    /// be determined — not that there are no dependencies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dependencies_sha256: Option<String>,
}

/// T18 (KR-23): wall-clock budget for one VCS probe.
pub const GIT_PROBE_BUDGET: std::time::Duration = std::time::Duration::from_secs(10);

/// Outcome of one bounded VCS probe. `timed_out` is distinct from "the command failed": a timeout
/// means the identity is unknown, while a failure can legitimately mean "not a repository".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitProbe {
    pub stdout: Option<String>,
    pub timed_out: bool,
}

thread_local! {
    /// Set when a probe in *this thread* timed out, so the fingerprint can report it without threading
    /// a mutable report through every helper. Thread-local because fingerprints are built per run and
    /// tests run in parallel.
    static GIT_PROBE_TIMED_OUT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// T18 (KR-15): sha256 of the request that is about to be transmitted.
///
/// Computed *after* assembly — the system prompt, the final tool list (including any on-demand
/// activations) and the projected messages — so a recorded hash corresponds to the wire request rather
/// than to the configuration that was meant to produce it. Sensitive content is never included: the
/// digest is the only thing that leaves this function.
pub fn request_identity(
    system_prompt: &str,
    tools: &[crate::tool::definition::ToolDefinition],
    messages: &[crate::messages::TranscriptItem],
) -> String {
    let mut hasher = sha2::Sha256::new();
    use sha2::Digest as _;
    hasher.update(b"system\0");
    hasher.update(system_prompt.as_bytes());
    hasher.update(b"\0tools\0");
    // Sorted by name so a reordering of the same set does not change the identity.
    let mut sorted = tools.to_vec();
    sorted.sort_by(|a, b| a.name.cmp(&b.name));
    for tool in &sorted {
        hasher.update(tool.name.as_bytes());
        hasher.update([0u8]);
        hasher.update(tool.description.as_bytes());
        hasher.update([0u8]);
        hasher.update(
            serde_json::to_string(&tool.parameters)
                .unwrap_or_default()
                .as_bytes(),
        );
        hasher.update([0u8]);
    }
    hasher.update(b"\0messages\0");
    for message in messages {
        hasher.update(
            serde_json::to_string(message)
                .unwrap_or_default()
                .as_bytes(),
        );
        hasher.update([0u8]);
    }
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        hex.push_str(&format!("{:02x}", byte));
    }
    hex
}

/// Run one bounded VCS probe.
///
/// Exposed so the timeout behaviour can be tested against a deliberately slow program instead of
/// hoping a real repository is slow enough.
pub fn git_probe(
    program: &str,
    root: &Path,
    args: &[&str],
    budget: std::time::Duration,
) -> GitProbe {
    let mut argv: Vec<String> = vec!["-C".to_string(), root.display().to_string()];
    argv.extend(args.iter().map(|a| (*a).to_string()));
    // A provenance probe must never touch the caller's stdin, take optional locks or run a pager.
    match crate::sandbox::run_bounded(program, &argv, budget) {
        Ok(outcome) if outcome.success => GitProbe {
            stdout: Some(outcome.stdout.trim().to_string()),
            timed_out: false,
        },
        Ok(_) => GitProbe {
            stdout: None,
            timed_out: false,
        },
        Err(e) => GitProbe {
            stdout: None,
            timed_out: e.contains("did not finish"),
        },
    }
}

fn is_false(value: &bool) -> bool {
    !*value
}

impl SourceFingerprint {
    /// `true` when nothing about the source could be established.
    pub fn is_empty(&self) -> bool {
        self.commit.is_none() && self.source_sha256.is_none() && self.binary_sha256.is_none()
    }

    /// A one-line human summary for reports; states unknown fields as unknown rather than omitting
    /// them, so a reader cannot mistake silence for "clean".
    pub fn summary(&self) -> String {
        let commit = self.commit.as_deref().unwrap_or("unknown");
        let dirty = match self.dirty {
            Some(true) => format!(
                "dirty({} paths)",
                self.changed_paths
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| "?".to_string())
            ),
            Some(false) => "clean".to_string(),
            None => "unknown".to_string(),
        };
        let source = match (&self.source_sha256, self.source_files) {
            (Some(hash), Some(files)) => format!(
                "{} ({} files{})",
                &hash[..hash.len().min(16)],
                files,
                if self.source_hash_capped {
                    ", capped"
                } else {
                    ""
                }
            ),
            (Some(hash), None) => hash[..hash.len().min(16)].to_string(),
            _ => "unknown".to_string(),
        };
        let binary = self
            .binary_sha256
            .as_deref()
            .map(|hash| hash[..hash.len().min(16)].to_string())
            .unwrap_or_else(|| "unknown".to_string());
        let deps = self
            .dependencies_sha256
            .as_deref()
            .map(|hash| hash[..hash.len().min(16)].to_string())
            .unwrap_or_else(|| "unknown".to_string());
        format!(
            "commit={} tree={} source={} binary={} deps={}",
            commit, dirty, source, binary, deps
        )
    }
}

/// Collect the fingerprint for `source_root`, hashing `binary` (usually the running executable).
///
/// Never returns an error: an unavailable probe leaves the corresponding field `None`.
pub fn fingerprint(source_root: &Path, binary: Option<&Path>) -> SourceFingerprint {
    let mut fingerprint = fingerprint_metadata(source_root, binary);
    // The working directory is not necessarily a project: a process started from `/` must not walk
    // the whole filesystem. A tree without VCS metadata can still be fingerprinted explicitly.
    if source_root.join(".git").exists() {
        let (hash, files, capped, incomplete) = hash_source_tree(source_root);
        fingerprint.source_sha256 = hash;
        fingerprint.source_files = files;
        fingerprint.source_hash_capped = capped;
        if let Some(reason) = incomplete {
            fingerprint.source_identity_incomplete = true;
            fingerprint.incomplete_reason = Some(reason);
        }
    }
    fingerprint
}

/// [`fingerprint`] for a tree the caller explicitly names as source even without VCS metadata
/// (a release tarball, an exported directory, a container build context).
pub fn fingerprint_explicit(source_root: &Path, binary: Option<&Path>) -> SourceFingerprint {
    let mut fingerprint = fingerprint_metadata(source_root, binary);
    let (hash, files, capped, incomplete) = hash_source_tree(source_root);
    fingerprint.source_sha256 = hash;
    fingerprint.source_files = files;
    fingerprint.source_hash_capped = capped;
    if let Some(reason) = incomplete {
        fingerprint.source_identity_incomplete = true;
        fingerprint.incomplete_reason = Some(reason);
    }
    if GIT_PROBE_TIMED_OUT.with(|flag| flag.replace(false)) {
        fingerprint.git_probe_timeout = true;
        fingerprint.source_identity_incomplete = true;
        fingerprint
            .incomplete_reason
            .get_or_insert_with(|| "a VCS probe hit its time budget".to_string());
    }
    fingerprint
}

fn fingerprint_metadata(source_root: &Path, binary: Option<&Path>) -> SourceFingerprint {
    let mut fingerprint = SourceFingerprint::default();
    if let Some((commit, changed_paths)) = git_identity(source_root) {
        fingerprint.commit = commit;
        fingerprint.changed_paths = changed_paths.as_ref().map(|paths| paths.len());
        // "dirty" is only meaningful when a revision is known: untracked files in a repository with
        // no commits are still untracked, which `git status` reports.
        fingerprint.dirty = changed_paths.map(|paths| !paths.is_empty());
    }
    if let Some(binary) = binary {
        if let Some((hash, bytes)) = hash_file(binary) {
            fingerprint.binary_sha256 = Some(hash);
            fingerprint.binary_bytes = Some(bytes);
        }
    }
    // A Cargo lockfile is the resolved dependency set. Its absence is unknown, not "no deps".
    fingerprint.dependencies_sha256 =
        hash_file(&source_root.join("Cargo.lock")).map(|(hash, _)| hash);
    fingerprint
}

/// Fingerprint the current process's own artifact plus `source_root`.
pub fn fingerprint_self(source_root: &Path) -> SourceFingerprint {
    let binary = std::env::current_exe().ok();
    fingerprint(source_root, binary.as_deref())
}

fn git(source_root: &Path, args: &[&str]) -> Option<String> {
    // T18 (KR-23): a VCS probe must be bounded. `git status` on a large or network-backed tree can
    // take minutes, and an unbounded `output()` turns a provenance read into a hang. The failure is
    // reported as a timeout so the caller can say "unknown" instead of "clean".
    let probe = git_probe("git", source_root, args, GIT_PROBE_BUDGET);
    if probe.timed_out {
        GIT_PROBE_TIMED_OUT.with(|flag| flag.set(true));
    }
    probe.stdout
}

/// `(commit, changed_paths)`. `changed_paths` is `None` when the status probe failed, which is
/// different from "clean".
fn git_identity(source_root: &Path) -> Option<(Option<String>, Option<Vec<String>>)> {
    let inside = git(source_root, &["rev-parse", "--is-inside-work-tree"])?;
    if inside != "true" {
        return None;
    }
    let commit = git(source_root, &["rev-parse", "HEAD"]).filter(|value| !value.is_empty());
    let changed = git(source_root, &["status", "--porcelain"]).map(|text| {
        text.lines()
            .map(str::to_string)
            .filter(|line| !line.trim().is_empty())
            .collect::<Vec<_>>()
    });
    Some((commit, changed))
}

fn hash_file(path: &Path) -> Option<(String, u64)> {
    use sha2::{Digest, Sha256};
    let bytes = std::fs::read(path).ok()?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    Some((hex(&hasher.finalize()), bytes.len() as u64))
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{:02x}", byte));
    }
    out
}

/// Deterministic hash over the source files under `root`.
///
/// Files are visited in sorted path order and each path is hashed before its bytes, so a rename
/// changes the result. Symlinks are recorded as their target text and never followed, so the walk
/// cannot escape the root or loop.
fn hash_source_tree(root: &Path) -> (Option<String>, Option<usize>, bool, Option<String>) {
    use sha2::{Digest, Sha256};
    if !root.is_dir() {
        return (None, None, false, None);
    }
    let started = std::time::Instant::now();
    let mut files: Vec<PathBuf> = Vec::new();
    let mut capped = false;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if started.elapsed() > MAX_WALK_TIME {
            capped = true;
            break;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_symlink() {
                files.push(path);
            } else if file_type.is_dir() {
                if !EXCLUDED_DIRS.contains(&name.as_str()) {
                    stack.push(path);
                }
            } else if file_type.is_file() {
                files.push(path);
            }
            // A FIFO, socket or device is neither descended into nor read: opening one can block
            // forever, and a fingerprint must never stall a run.
            if files.len() > MAX_SOURCE_FILES {
                capped = true;
                break;
            }
        }
        if capped {
            break;
        }
    }
    if capped {
        // A capped walk cannot claim to identify the tree; drop the count to reflect that.
        files.truncate(MAX_SOURCE_FILES);
    }
    files.sort();
    let mut hasher = Sha256::new();
    let mut bytes_read: u64 = 0;
    let mut hashed = 0usize;
    let mut incomplete: Option<String> = None;
    for path in &files {
        if started.elapsed() > MAX_WALK_TIME {
            capped = true;
            break;
        }
        let Ok(metadata) = std::fs::symlink_metadata(path) else {
            continue;
        };
        let Ok(relative) = path.strip_prefix(root) else {
            continue;
        };
        hasher.update(relative.to_string_lossy().as_bytes());
        hasher.update([0u8]);
        if metadata.file_type().is_symlink() {
            let target = std::fs::read_link(path)
                .map(|t| t.to_string_lossy().to_string())
                .unwrap_or_default();
            hasher.update(b"symlink:");
            hasher.update(target.as_bytes());
        } else if metadata.is_file() {
            if bytes_read.saturating_add(metadata.len()) > MAX_SOURCE_BYTES {
                capped = true;
                break;
            }
            // T18 (KR-24): the size check above is a diagnostic. The read itself is bounded to the
            // remaining budget, so a file that grows after the check cannot blow past it.
            let remaining = MAX_SOURCE_BYTES.saturating_sub(bytes_read).max(1);
            match crate::secure_fs::read_bounded_file(
                path,
                remaining,
                crate::secure_fs::Overflow::Refuse,
            ) {
                Ok(read) => {
                    bytes_read = bytes_read.saturating_add(read.bytes.len() as u64);
                    // T18: the executable bit is part of the artifact's identity — a chmod +x
                    // changes what the tree builds into, so it has to change the hash.
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        hasher.update([(metadata.permissions().mode() & 0o111 != 0) as u8]);
                    }
                    hasher.update(&read.bytes);
                }
                Err(e) => {
                    // T18 (KR-24): a file we could not read means this hash does not identify the
                    // tree. Skipping it silently would produce a "complete" identity that is not.
                    incomplete = Some(format!(
                        "could not hash {}: {}",
                        relative.to_string_lossy(),
                        e
                    ));
                    continue;
                }
            }
        } else {
            // Raced into something unreadable between listing and stat: skip, never open it.
            continue;
        }
        hashed += 1;
    }
    if capped && incomplete.is_none() {
        incomplete = Some("source walk stopped at a bound".to_string());
    }
    (
        Some(hex(&hasher.finalize())),
        Some(hashed),
        capped,
        incomplete,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("kroma-prov-{}-{}", label, uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn git_init(root: &Path) {
        use std::process::Command;
        let run = |args: &[&str]| {
            Command::new("git")
                .arg("-C")
                .arg(root)
                .args(args)
                .stdin(std::process::Stdio::null())
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@example.com")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@example.com")
                .output()
                .expect("git available")
        };
        run(&["init", "-q"]);
        std::fs::write(root.join("a.rs"), "fn a() {}\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-qm", "init"]);
    }

    /// The core promise: a dirty tree must not be identified by its commit alone.
    #[test]
    fn a_dirty_tree_is_distinguishable_from_the_clean_tree_at_the_same_commit() {
        let root = temp_dir("dirty");
        git_init(&root);
        let clean = fingerprint(&root, None);
        assert_eq!(clean.dirty, Some(false));
        assert_eq!(clean.changed_paths, Some(0));
        assert!(clean.commit.is_some());
        assert!(clean.source_sha256.is_some());

        // An untracked file changes the tree identity without changing the commit.
        std::fs::write(root.join("new.rs"), "fn b() {}\n").unwrap();
        let dirty = fingerprint(&root, None);
        assert_eq!(dirty.commit, clean.commit, "the commit itself is unchanged");
        assert_eq!(dirty.dirty, Some(true));
        assert!(dirty.changed_paths.unwrap() >= 1);
        assert_ne!(
            dirty.source_sha256, clean.source_sha256,
            "the source hash must distinguish a dirty tree from the committed one"
        );
        assert!(dirty.summary().contains("dirty"));

        // Reverting the tree restores the original identity.
        std::fs::remove_file(root.join("new.rs")).unwrap();
        assert_eq!(fingerprint(&root, None).source_sha256, clean.source_sha256);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A modified *tracked* file must also change the source hash, not only an added one.
    #[test]
    fn editing_a_tracked_file_changes_the_source_hash() {
        let root = temp_dir("edit");
        git_init(&root);
        let before = fingerprint(&root, None);
        std::fs::write(root.join("a.rs"), "fn a() { /* changed */ }\n").unwrap();
        let after = fingerprint(&root, None);
        assert_eq!(before.commit, after.commit);
        assert_ne!(before.source_sha256, after.source_sha256);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Outside a repository nothing is invented: unknown stays unknown. A plain directory is also
    /// **not** walked by default, because a process started from `/` must not hash the filesystem.
    #[test]
    fn a_non_repository_reports_unknown_rather_than_a_fake_commit() {
        let root = temp_dir("nogit");
        std::fs::write(root.join("x.rs"), "fn x() {}\n").unwrap();
        let print = fingerprint(&root, None);
        assert!(print.commit.is_none(), "no repository means no commit");
        assert!(print.dirty.is_none(), "unknown is not 'clean'");
        assert!(
            print.source_sha256.is_none(),
            "a directory with no checkout marker must not be walked: {print:?}"
        );
        assert!(print.summary().contains("unknown"));
        // An operator who names the tree explicitly still gets a content hash (tarball exports).
        let explicit = fingerprint_explicit(&root, None);
        assert!(explicit.source_sha256.is_some());
        assert_eq!(explicit.source_files, Some(1));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A fingerprint must never open a FIFO/socket/device: reading one blocks forever, and the
    /// fingerprint is only a convenience. This is the regression for a process whose working
    /// directory happened to be `/`.
    ///
    /// Unix-only: `mkfifo` is the POSIX way to plant a blocking special file, and the Windows test
    /// binary must still compile (`cargo test --target x86_64-pc-windows-gnu --no-run`).
    #[cfg(unix)]
    #[test]
    fn special_files_are_never_read_and_cannot_stall_the_walk() {
        let root = temp_dir("fifo");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join("real.rs"), "fn real() {}\n").unwrap();
        let fifo = root.join("blocking.fifo");
        let c_path = std::ffi::CString::new(fifo.to_string_lossy().as_bytes()).unwrap();
        // SAFETY: a plain mkfifo(3) call on a path inside our own temp directory.
        let created = unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) } == 0;
        if !created {
            // Without FIFO support there is nothing to regress against; fail loudly instead of
            // silently skipping, because this is the only guard for the blocking case.
            panic!("could not create a FIFO to test the non-blocking guarantee");
        }
        let started = std::time::Instant::now();
        let print = fingerprint(&root, None);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(3),
            "the walk must not block on a special file, took {:?}",
            started.elapsed()
        );
        assert!(
            print.source_sha256.is_some(),
            "regular files are still hashed"
        );
        assert_eq!(
            print.source_files,
            Some(1),
            "the FIFO must not be counted as a hashed file: {print:?}"
        );
        // Opening the FIFO for reading would have blocked; the fingerprint completed instead.
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A walk of the filesystem root must be refused rather than attempted.
    #[test]
    fn the_filesystem_root_is_not_walked() {
        let started = std::time::Instant::now();
        let print = fingerprint(std::path::Path::new("/"), None);
        assert!(
            print.source_sha256.is_none(),
            "no checkout marker at /: {print:?}"
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "refusing must be immediate, took {:?}",
            started.elapsed()
        );
    }

    /// Build and dependency directories must not be part of the identity; otherwise every run would
    /// differ because of compiled artifacts.
    #[test]
    fn build_and_vcs_directories_are_excluded() {
        let root = temp_dir("exclude");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join("keep.rs"), "fn keep() {}\n").unwrap();
        let baseline = fingerprint(&root, None).source_sha256;
        assert!(baseline.is_some(), "a checkout is walked");
        std::fs::create_dir_all(root.join("target/debug")).unwrap();
        std::fs::write(root.join("target/debug/kroma"), b"binary").unwrap();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        assert_eq!(
            fingerprint(&root, None).source_sha256,
            baseline,
            "build/VCS directories must not change the source identity"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The resolved dependency set is part of the artifact identity, and an absent lockfile is
    /// "unknown" rather than "no dependencies".
    #[test]
    fn the_resolved_dependency_set_is_recorded_when_pinned() {
        let root = temp_dir("deps");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join("Cargo.lock"), "version = 4\n").unwrap();
        let first = fingerprint(&root, None);
        assert!(first.dependencies_sha256.is_some(), "{first:?}");
        assert!(first.summary().contains("deps="));
        std::fs::write(root.join("Cargo.lock"), "version = 4\n# bump\n").unwrap();
        assert_ne!(
            first.dependencies_sha256,
            fingerprint(&root, None).dependencies_sha256,
            "a changed lockfile must change the dependency fingerprint"
        );
        std::fs::remove_file(root.join("Cargo.lock")).unwrap();
        assert!(
            fingerprint(&root, None).dependencies_sha256.is_none(),
            "unknown, not empty"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_binary_is_identified_by_content() {
        let root = temp_dir("binary");
        let binary = root.join("artifact");
        std::fs::write(&binary, b"version-one").unwrap();
        let first = fingerprint(&root, Some(&binary));
        assert!(first.binary_sha256.is_some());
        assert_eq!(first.binary_bytes, Some(11));
        std::fs::write(&binary, b"version-two").unwrap();
        let second = fingerprint(&root, Some(&binary));
        assert_ne!(first.binary_sha256, second.binary_sha256);
        let _ = std::fs::remove_dir_all(&root);
    }
}
