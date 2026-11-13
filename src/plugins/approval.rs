//! Bounded source scanning for idle worker reuse, not an approval workflow.
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    time::SystemTime,
};

/// Tracked authored code excludes generated dependency/build folders. This
/// scan detects source changes for idle worker replacement, never asks the
/// user to approve edits or forbids enabled plugins from running.
const PLUGIN_IGNORED_DIRECTORIES: &[&str] = &[
    "node_modules",
    ".git",
    "target",
    "__pycache__",
    ".pytest_cache",
];

/// One tracked plugin file: path, content digest and modification time. The
/// digest is `None` only when reading the file failed.
type PluginEntryPoint = (PathBuf, Option<String>, Option<SystemTime>);

#[derive(Clone, Default)]
pub(super) struct PluginCodeScan {
    pub(super) entrypoints: Vec<PluginEntryPoint>,
    /// Why this scan is not a complete view of the plugin's local code.
    pub(super) error: Option<String>,
}

#[derive(Clone, Copy)]
pub(super) struct PluginScanLimits {
    /// Filesystem entries visited before the scan gives up.
    pub(super) max_entries: usize,
    /// Total code bytes hashed before the scan gives up.
    pub(super) max_bytes: u64,
    /// Largest single source file included in a change scan.
    pub(super) max_file_bytes: u64,
    /// Largest explicit `command`/`args` script that can be approved.
    pub(super) max_explicit_bytes: u64,
}

/// Generous for authored code (a multi-megabyte JS helper is still tracked and
/// far smaller than a vendored dependency tree), so an ordinary plugin never
/// hits a bound while a pathological directory cannot turn discovery into an
/// unbounded read.
pub(super) const PLUGIN_SCAN_LIMITS: PluginScanLimits = PluginScanLimits {
    max_entries: 20_000,
    max_bytes: 64 * 1024 * 1024,
    max_file_bytes: 8 * 1024 * 1024,
    max_explicit_bytes: 16 * 1024 * 1024,
};

fn is_ignored_plugin_directory(name: Option<&str>) -> bool {
    name.is_some_and(|name| PLUGIN_IGNORED_DIRECTORIES.contains(&name))
}

/// Length-prefixed fields keep "ab"+"c" from hashing like "a"+"bc".
/// Bounded scan of plugin-local code plus explicit `command`/`args` script
/// paths. It is cycle-safe and budgeted, and every read or listing failure
/// is reported instead of skipped: an unreadable directory, an unreadable
/// file, an oversized file, an exhausted entry or byte budget, or a file
/// that grew while it was read all make the scan incomplete, and an
/// incomplete scan is never approval. Only dependency/build output
/// directories are omitted deliberately.
pub(super) fn scan(
    dir: &Path,
    command: Option<&str>,
    args: &[String],
    limits: &PluginScanLimits,
) -> PluginCodeScan {
    let mut entrypoints: Vec<PluginEntryPoint> = Vec::new();
    let mut error: Option<String> = None;
    let mut pending = vec![dir.to_path_buf()];
    // A project plugin can contain a junction/symlink back to its own
    // directory or to a huge external tree. Never visit the same
    // canonical folder twice, and bound the total work rather than
    // silently dropping queued folders.
    let mut visited = BTreeSet::new();
    let mut scanned_entries = 0usize;
    let mut scanned_bytes = 0u64;
    'walk: while let Some(folder) = pending.pop() {
        let canonical = folder.canonicalize().unwrap_or_else(|_| folder.clone());
        if !visited.insert(canonical) {
            continue;
        }
        let entries = match fs::read_dir(&folder) {
            Ok(entries) => entries,
            // An unreadable directory could hold code, so it must not be
            // treated as empty.
            Err(failure) => {
                error.get_or_insert_with(|| format!("cannot list {}: {failure}", folder.display()));
                continue;
            }
        };
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(failure) => {
                    error.get_or_insert_with(|| {
                        format!("cannot read an entry in {}: {failure}", folder.display())
                    });
                    continue;
                }
            };
            if scanned_entries >= limits.max_entries {
                error.get_or_insert_with(|| {
                    format!(
                        "stopped after {} filesystem entries, too many to approve completely",
                        limits.max_entries
                    )
                });
                break 'walk;
            }
            scanned_entries += 1;
            let path = entry.path();
            // Follow links: code reachable through a link is code this
            // plugin can run. Unreadable/dangling entries fail closed.
            let metadata = match fs::metadata(&path) {
                Ok(metadata) => metadata,
                Err(failure) => {
                    error.get_or_insert_with(|| {
                        format!("cannot inspect {}: {failure}", path.display())
                    });
                    continue;
                }
            };
            if metadata.is_dir() {
                if !is_ignored_plugin_directory(entry.file_name().to_str()) {
                    pending.push(path);
                }
                continue;
            }
            if !metadata.is_file() {
                continue;
            }
            if metadata.len() > limits.max_file_bytes {
                error.get_or_insert_with(|| {
                    format!(
                        "{} is larger than the {} byte limit for one plugin file",
                        path.display(),
                        limits.max_file_bytes
                    )
                });
                continue;
            }
            if scanned_bytes.saturating_add(metadata.len()) > limits.max_bytes {
                error.get_or_insert_with(|| {
                    format!(
                        "plugin code exceeds the {} byte approval budget",
                        limits.max_bytes
                    )
                });
                break 'walk;
            }
            match crate::attachments::read_bounded(&path, limits.max_file_bytes) {
                // The file grew after its metadata was read.
                Ok(bytes) if bytes.len() as u64 > limits.max_file_bytes => {
                    error.get_or_insert_with(|| {
                        format!("{} changed while it was being approved", path.display())
                    });
                }
                Ok(bytes) => {
                    scanned_bytes = scanned_bytes.saturating_add(bytes.len() as u64);
                    entrypoints.push((
                        path,
                        Some(crate::mcp::content_digest(&bytes)),
                        metadata.modified().ok(),
                    ));
                }
                Err(failure) => {
                    error.get_or_insert_with(|| {
                        format!("cannot read {}: {failure}", path.display())
                    });
                }
            }
            if scanned_bytes > limits.max_bytes {
                error.get_or_insert_with(|| {
                    format!(
                        "plugin code exceeds the {} byte approval budget",
                        limits.max_bytes
                    )
                });
                break 'walk;
            }
        }
    }
    // Explicit script paths also cover non-JS entry points such as a
    // Python, PowerShell or shell helper named by the manifest.
    for value in command.into_iter().chain(args.iter().map(String::as_str)) {
        let candidate = Path::new(value);
        let path = if candidate.is_absolute() {
            candidate.to_path_buf()
        } else {
            dir.join(candidate)
        };
        if entrypoints.iter().any(|(existing, _, _)| existing == &path) {
            continue;
        }
        let metadata = match fs::metadata(&path) {
            Ok(metadata) => metadata,
            // A plain program name or flag is not a plugin path; the OS
            // resolves it and its bytes are not part of the approval.
            Err(failure)
                if failure.kind() == std::io::ErrorKind::NotFound
                    && !candidate.is_absolute()
                    && !value.starts_with("./")
                    && !value.starts_with("../")
                    && !value.starts_with(".\\")
                    && !value.starts_with("..\\") =>
            {
                continue
            }
            Err(failure) => {
                error
                    .get_or_insert_with(|| format!("cannot inspect {}: {failure}", path.display()));
                continue;
            }
        };
        if !metadata.is_file() {
            continue;
        }
        if metadata.len() > limits.max_explicit_bytes {
            error.get_or_insert_with(|| {
                format!(
                    "{} is larger than the {} byte limit for a script entry",
                    path.display(),
                    limits.max_explicit_bytes
                )
            });
            continue;
        }
        if scanned_bytes.saturating_add(metadata.len()) > limits.max_bytes {
            error.get_or_insert_with(|| {
                format!(
                    "plugin code exceeds the {} byte approval budget",
                    limits.max_bytes
                )
            });
            break;
        }
        match crate::attachments::read_bounded(&path, limits.max_explicit_bytes) {
            Ok(bytes) if bytes.len() as u64 > limits.max_explicit_bytes => {
                error.get_or_insert_with(|| {
                    format!("{} changed while it was being approved", path.display())
                });
            }
            Ok(bytes) => {
                scanned_bytes = scanned_bytes.saturating_add(bytes.len() as u64);
                entrypoints.push((
                    path,
                    Some(crate::mcp::content_digest(&bytes)),
                    metadata.modified().ok(),
                ));
            }
            Err(failure) => {
                error.get_or_insert_with(|| format!("cannot read {}: {failure}", path.display()));
            }
        }
    }
    if scanned_bytes > limits.max_bytes {
        error.get_or_insert_with(|| {
            format!(
                "plugin code exceeds the {} byte approval budget",
                limits.max_bytes
            )
        });
    }
    entrypoints.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    PluginCodeScan { entrypoints, error }
}

impl PluginCodeScan {
    pub(super) fn same_content(&self, next: &Self) -> bool {
        self.error == next.error
            && self
                .entrypoints
                .iter()
                .map(|(path, digest, _)| (path, digest))
                .eq(next
                    .entrypoints
                    .iter()
                    .map(|(path, digest, _)| (path, digest)))
    }
}
