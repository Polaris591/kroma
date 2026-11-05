//! Bounded plugin content scanning and approval fingerprints. No process or Host ownership.
use anyhow::{bail, Context, Result};
use std::{
    collections::BTreeSet,
    fs,
    io::Read,
    path::{Path, PathBuf},
    time::SystemTime,
};

/// Trust boundary for project plugin approval.
///
/// Kroma is not a sandbox: an enabled plugin is a child process with the
/// user's own privileges and can read anything that user can read. What the
/// approval binds is the plugin's authored local code: the manifest, every
/// regular file below the plugin directory, and the script paths
/// named by `command`/`args`. Dependency and build output (`node_modules`,
/// `.git`, `target`) is excluded on purpose, because it is install artifact
/// and not plugin source, and the real dependency graph can be huge.
/// Everything else that carries code is bounded, and anything the scan cannot
/// read or bound makes the scan incomplete. An incomplete scan never approves
/// code, and a partial set of files is never trusted as a complete one.
const PLUGIN_IGNORED_DIRECTORIES: &[&str] = &[
    "node_modules",
    ".git",
    "target",
    "__pycache__",
    ".pytest_cache",
];

/// One tracked plugin file: path, content digest and modification time. The
/// digest is `None` only when the scan failed to read the file, and a read
/// failure blocks approval instead of falling back to a size or mtime.
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
    /// Largest single plugin-local code file that can be approved.
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

/// Read at most `limit + 1` bytes, so a file that grew after its metadata was
/// read cannot turn a bounded approval read into an unbounded allocation.
fn read_bounded(path: &Path, limit: u64) -> std::io::Result<Vec<u8>> {
    let file = fs::File::open(path)?;
    let mut bytes = Vec::new();
    file.take(limit.saturating_add(1)).read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// Length-prefixed fields keep "ab"+"c" from hashing like "a"+"bc".
fn push_approval_field(material: &mut Vec<u8>, bytes: &[u8]) {
    material.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
    material.extend_from_slice(bytes);
}

/// Bind a project plugin approval to the bytes that will actually run: the
/// manifest, every authored plugin-local file the bounded scan tracks (including
/// non-JS helpers and native artifacts the plugin can load or exec), and
/// the explicit `command`/`args` scripts. Dependency/build directories are
/// outside this boundary on purpose, so this is an approval and change
/// boundary, not a sandbox: an approved plugin still runs with user rights.
/// The scan must be complete: an unreadable directory or file, an exhausted
/// budget or an oversized file fails closed instead of approving a partial
/// set. The v3 tag is a deterministic domain separator, so approvals written
/// by the older JS-only v2 scan never match this stricter boundary.
pub(super) fn fingerprint(directory: &Path, scan: &PluginCodeScan) -> Result<String> {
    if let Some(reason) = scan.error.as_deref() {
        bail!("local code scan is incomplete: {reason}");
    }
    let manifest = fs::read(directory.join("plugin.toml"))
        .context("reading the plugin manifest for approval")?;
    let mut material = Vec::new();
    push_approval_field(&mut material, b"kroma-project-plugin-approval-v3");
    push_approval_field(&mut material, &manifest);
    let mut entrypoints: Vec<_> = scan.entrypoints.iter().collect();
    entrypoints.sort_by(|left, right| left.0.cmp(&right.0));
    for (path, digest, _) in entrypoints {
        let digest = digest
            .as_deref()
            .with_context(|| format!("cannot read {} for approval", path.display()))?;
        push_approval_field(&mut material, path.to_string_lossy().as_bytes());
        push_approval_field(&mut material, digest.as_bytes());
    }
    Ok(crate::mcp::content_digest(&material))
}

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
            match read_bounded(&path, limits.max_file_bytes) {
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
        match read_bounded(&path, limits.max_explicit_bytes) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_preserves_v3_field_layout_and_ignores_mtime() -> Result<()> {
        let root = std::env::temp_dir().join(format!(
            "kroma-approval-layout-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos()
        ));
        fs::create_dir_all(&root)?;
        let manifest = b"id='layout'
name='Layout'
prompt='unchanged'
";
        fs::write(root.join("plugin.toml"), manifest)?;
        let mut scan = PluginCodeScan {
            error: None,
            entrypoints: vec![
                (root.join("b"), Some("digest-b".into()), None),
                (root.join("a"), Some("digest-a".into()), None),
            ],
        };
        let mut reference = Vec::new();
        for field in [
            b"kroma-project-plugin-approval-v3".as_slice(),
            manifest.as_slice(),
        ] {
            reference.extend_from_slice(&(field.len() as u64).to_le_bytes());
            reference.extend_from_slice(field);
        }
        for (name, digest) in [("a", "digest-a"), ("b", "digest-b")] {
            let path = root.join(name).to_string_lossy().into_owned();
            for field in [path.as_bytes(), digest.as_bytes()] {
                reference.extend_from_slice(&(field.len() as u64).to_le_bytes());
                reference.extend_from_slice(field);
            }
        }
        let expected = crate::mcp::content_digest(&reference);
        assert_eq!(fingerprint(&root, &scan)?, expected);
        scan.entrypoints[0].2 = Some(SystemTime::now());
        assert_eq!(fingerprint(&root, &scan)?, expected);
        scan.error = Some("incomplete".into());
        assert!(fingerprint(&root, &scan).is_err());
        fs::remove_dir_all(root)?;
        Ok(())
    }
}
