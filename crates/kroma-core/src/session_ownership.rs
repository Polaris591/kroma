//! Runtime session ownership and workspace writer routing.
//!
//! The multi-surface runtime hosts one session per dispatcher, but several dispatchers (REPL, ACP
//! stdio, `kroma serve` connections, and the CLI) can be alive in one process and may point at the
//! same workspace tree. This module makes that topology explicit instead of leaving it implicit in
//! per-dispatcher mutexes that cannot see each other:
//!
//! * **Workspace writer ownership** — [`try_acquire_writer`] hands out one exclusive, RAII
//!   [`WriterLease`] per canonical workspace root. A mutation surface holds the lease for the whole
//!   run, so two sessions can never interleave writes into the same tree, while sessions bound to
//!   different workspaces run fully independently.
//! * **Session routing** — [`register_session`] / [`lookup_session_route`] map a session id to the
//!   workspace root and the runtime that owns it, so a control plane can route a request to the
//!   runtime that owns the session instead of guessing or sharing one global session.
//!
//! The registry is process-local metadata only; it never holds workspace contents, WAL state, or
//! provider state. A future persistent task table can build on this routing seam.
//!
//! **Cross-process safety.** The process-local registry cannot see a second `kroma serve` process
//! on the same workspace, so [`try_acquire_writer`] additionally takes a non-blocking advisory
//! `flock` on a lock file under a *stable, user-scoped* directory that is deliberately **not**
//! derived from `TMPDIR`/`HOME`/`XDG_*` (a task could otherwise move the lock domain and slip a
//! second writer past the guard). On Unix the directory is `/tmp/kroma-writer-locks-<uid>`, owned
//! by the current user and mode `0700`; a pre-existing symlink or a directory owned by another user
//! is refused. The lock file is opened `O_NOFOLLOW` and verified regular-file/owned by `fstat`, so
//! a planted symlink cannot redirect the lock. The lock is tied to the lease's file descriptor, so
//! the kernel releases it automatically when the holding process exits — there is no stale-lock
//! window to reap. The file records the owner id and PID for diagnostics.
//!
//! T08 (KR-26): Windows has a native equivalent, not a fallback. The lock file is opened with every
//! share mode allowed and then claimed with `LockFileEx(LOCKFILE_EXCLUSIVE_LOCK |
//! LOCKFILE_FAIL_IMMEDIATELY)` on byte 0 — `flock(LOCK_EX | LOCK_NB)`'s counterpart. The kernel
//! drops the range when the handle closes, including on process kill, so a crashed writer never
//! leaves a stale lease. Lock identity is the canonical workspace path with the `\\?\` verbatim
//! prefix stripped and the remainder lowercased, so case and prefix aliases share one lock domain.

use std::collections::HashMap;
use std::fs::File;
#[cfg(any(unix, windows))]
use std::fs::OpenOptions;
#[cfg(any(unix, windows))]
use std::io::{Read, Seek, SeekFrom, Write};
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
#[cfg(unix)]
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// Identity of a runtime that owns a session, as seen by the routing table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRoute {
    pub session_id: String,
    pub workspace_root: PathBuf,
    /// Stable id of the runtime instance (one per dispatcher), independent of the session id.
    pub owner_id: String,
}

/// A second writer tried to claim a workspace that already has an active writer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriterConflict {
    pub workspace_root: PathBuf,
    pub current_owner: String,
}

impl std::fmt::Display for WriterConflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "workspace {} already has an active writer ({})",
            self.workspace_root.display(),
            self.current_owner
        )
    }
}

impl std::error::Error for WriterConflict {}

/// One live write claim on a workspace.
///
/// `leases` counts every `WriterLease` handed out to the same owner, so a nested (re-entrant)
/// acquire cannot be released out from under its parent: dropping one lease decrements the count
/// and only the last drop removes the claim and closes the file (which is what releases the
/// cross-process `flock`).
struct WriterEntry {
    owner_id: String,
    leases: usize,
    /// Shared with the registry and with every lease of this owner.
    lock_file: Option<std::sync::Arc<File>>,
}

#[derive(Default)]
struct Registry {
    writers: HashMap<PathBuf, WriterEntry>,
    routes: HashMap<String, SessionRoute>,
}

fn registry() -> &'static Mutex<Registry> {
    static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(Registry::default()))
}

fn canonical_key(workspace_root: &Path) -> PathBuf {
    // KR-D02: `canonicalize` needs the path to exist. A workspace that has not been created yet (a
    // `--resume` into a fresh directory, a lock taken before `mkdir`) would otherwise fall back to
    // the caller's literal spelling, so `ws`, `./ws`, `ws/` and an absolute form of the same
    // directory could each get their own writer slot -- two writers, one workspace.
    let canonical = match std::fs::canonicalize(workspace_root) {
        Ok(path) => path,
        Err(_) => lexical_normalize(workspace_root),
    };
    #[cfg(windows)]
    {
        // T08 (KR-26): Windows paths are case-insensitive and `canonicalize` may hand back either the
        // verbatim `\\?\C:\...` form or a plain one, so `C:\Work\Repo`, `c:\work\repo` and
        // `\\?\C:\Work\Repo` are three spellings of one workspace. The lock file is keyed by a
        // hash of this string, so normalising here is what makes those aliases share one lock
        // domain instead of each getting its own writer slot.
        let text = canonical.to_string_lossy().into_owned();
        let stripped = text.strip_prefix(r"\\?\").unwrap_or(&text);
        return PathBuf::from(stripped.to_lowercase());
    }
    #[cfg(not(windows))]
    {
        canonical
    }
}

/// Make a not-yet-existing path absolute and lexically clean.
///
/// This deliberately does **not** resolve symlinks -- it cannot, the path may not exist -- so it is
/// only the fallback for paths `canonicalize` refused. Its job is the weaker but still load-bearing
/// guarantee: one spelling per path, so an alias cannot manufacture a second writer slot.
fn lexical_normalize(path: &Path) -> PathBuf {
    use std::path::Component;

    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        match std::env::current_dir() {
            Ok(cwd) => cwd.join(path),
            Err(_) => path.to_path_buf(),
        }
    };

    let mut out: Vec<Component> = Vec::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                // Pop a real directory name; never pop above the root.
                match out.last() {
                    Some(Component::Normal(_)) => {
                        out.pop();
                    }
                    Some(Component::RootDir) | Some(Component::Prefix(_)) => {}
                    _ => out.push(component),
                }
            }
            other => out.push(other),
        }
    }
    let normalized: PathBuf = out.iter().collect();
    // A trailing separator must not be a separate spelling.
    if normalized.as_os_str().is_empty() {
        PathBuf::from(std::path::MAIN_SEPARATOR.to_string())
    } else {
        normalized
    }
}

/// An exclusive, RAII claim on a workspace's write side.
///
/// Dropping the lease releases the claim; holding it across an `.await` is intended (a run spans
/// many awaits) because the lease is plain data with no lock guard inside. Dropping the lease also
/// closes the advisory lock file descriptor, which releases the cross-process `flock`.
#[derive(Debug)]
pub struct WriterLease {
    workspace_root: PathBuf,
    owner_id: String,
    /// Shared with the registry and every sibling lease of the same owner. The descriptor is closed
    /// only when the last holder drops, so a nested acquire never releases the parent's `flock`.
    #[allow(dead_code)]
    lock_file: Option<std::sync::Arc<File>>,
}

impl WriterLease {
    pub fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    pub fn owner_id(&self) -> &str {
        &self.owner_id
    }
}

impl Drop for WriterLease {
    fn drop(&mut self) {
        let mut reg = registry().lock().unwrap_or_else(|e| e.into_inner());
        let Some(entry) = reg.writers.get_mut(&self.workspace_root) else {
            return;
        };
        if entry.owner_id != self.owner_id {
            return;
        }
        entry.leases = entry.leases.saturating_sub(1);
        if entry.leases == 0 {
            reg.writers.remove(&self.workspace_root);
        }
    }
}

/// Claim the write side of `workspace_root` for `owner_id`.
///
/// Re-claiming by the same owner is allowed (idempotent) so a mutation that internally nests a
/// second acquire cannot deadlock against itself. Any *different* owner is rejected with the
/// current owner's id so the caller can report who holds the workspace. In addition to the
/// process-local registry, this takes a cross-process advisory `flock`; a conflict in either layer
/// is reported the same way, so a second `kroma serve` process is rejected exactly like a second
/// runtime in this process.
pub fn try_acquire_writer(
    workspace_root: &Path,
    owner_id: &str,
) -> Result<WriterLease, WriterConflict> {
    let key = canonical_key(workspace_root);
    let mut reg = registry().lock().unwrap_or_else(|e| e.into_inner());
    // A same-owner re-claim shares the existing entry *and* its file lock. Returning early here is
    // what makes the nesting safe: the parent's claim survives until the last lease drops.
    if let Some(entry) = reg.writers.get_mut(&key) {
        if entry.owner_id != owner_id {
            return Err(WriterConflict {
                workspace_root: key,
                current_owner: entry.owner_id.clone(),
            });
        }
        entry.leases += 1;
        let lock_file = entry.lock_file.clone();
        return Ok(WriterLease {
            workspace_root: key,
            owner_id: owner_id.to_string(),
            lock_file,
        });
    }
    let lock_file = match acquire_workspace_lock(&key, owner_id) {
        Ok(file) => file.map(std::sync::Arc::new),
        Err(current_owner) => {
            return Err(WriterConflict {
                workspace_root: key,
                current_owner,
            });
        }
    };
    reg.writers.insert(
        key.clone(),
        WriterEntry {
            owner_id: owner_id.to_string(),
            leases: 1,
            lock_file: lock_file.clone(),
        },
    );
    Ok(WriterLease {
        workspace_root: key,
        owner_id: owner_id.to_string(),
        lock_file,
    })
}

/// Current writer of `workspace_root`, if any live lease exists.
pub fn current_writer(workspace_root: &Path) -> Option<String> {
    let key = canonical_key(workspace_root);
    registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .writers
        .get(&key)
        .map(|entry| entry.owner_id.clone())
}

/// Stable directory holding the cross-process writer lock files.
///
/// Deliberately **not** built from `std::env::temp_dir()`/`TMPDIR`/`HOME`/`XDG_*`: those are task
/// environment and a task could point them elsewhere to get a second lock domain for the same
/// workspace. On Unix it is `/tmp/kroma-writer-locks-<uid>` (same user → same domain; different
/// users get isolated domains and `/tmp`'s sticky bit stops them replacing each other's entry).
pub fn workspace_lock_dir() -> PathBuf {
    #[cfg(unix)]
    {
        // SAFETY: `getuid` has no preconditions and never fails.
        let uid = unsafe { libc::getuid() };
        PathBuf::from(format!("/tmp/kroma-writer-locks-{}", uid))
    }
    #[cfg(windows)]
    {
        // T08 (KR-26): NOT `temp_dir()`. On Windows that reads TMP/TEMP from the environment, so a
        // task that overrides them for its children would get a second lock domain for the same
        // workspace — the exact hole the Unix side avoids by deriving the directory from the uid.
        // `C:\ProgramData` is machine-wide and is not a per-process temp variable.
        let machine = PathBuf::from(r"C:\ProgramData");
        if machine.is_dir() {
            machine.join("kroma-writer-locks")
        } else {
            // Documented fallback for stripped-down images that lack ProgramData.
            std::env::temp_dir().join("kroma-writer-locks")
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        std::env::temp_dir().join("kroma-writer-locks")
    }
}

/// Create the lock directory if needed and refuse to use a hostile one.
///
/// A pre-existing symlink, a non-directory, or a directory owned by another user is rejected rather
/// than followed, so a planted link cannot redirect the locks (and therefore the writes) elsewhere.
#[cfg(unix)]
fn ensure_lock_dir(dir: &Path) -> Result<(), String> {
    match std::fs::create_dir(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(format!("cannot create {}: {}", dir.display(), e)),
    }
    // `symlink_metadata` does not follow a final symlink, so a planted link is detected here.
    let meta = std::fs::symlink_metadata(dir)
        .map_err(|e| format!("cannot inspect {}: {}", dir.display(), e))?;
    if meta.file_type().is_symlink() {
        return Err(format!(
            "refusing symlinked writer-lock directory {}",
            dir.display()
        ));
    }
    if !meta.is_dir() {
        return Err(format!(
            "writer-lock path {} is not a directory",
            dir.display()
        ));
    }
    // SAFETY: `getuid` has no preconditions and never fails.
    let uid = unsafe { libc::getuid() };
    if meta.uid() != uid {
        return Err(format!(
            "writer-lock directory {} is owned by uid {}, not the current user {}",
            dir.display(),
            meta.uid(),
            uid
        ));
    }
    if meta.mode() & 0o777 != 0o700 {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| format!("cannot tighten permissions on {}: {}", dir.display(), e))?;
    }
    Ok(())
}

/// T08 (KR-26): the Windows lock directory has no mode bits, so the checks are reparse point,
/// directory-ness and (where available) owner. A planted junction cannot redirect the lock.
#[cfg(windows)]
fn ensure_lock_dir(dir: &Path) -> Result<(), String> {
    match std::fs::create_dir(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(format!("cannot create {}: {}", dir.display(), e)),
    }
    let meta = std::fs::symlink_metadata(dir)
        .map_err(|e| format!("cannot inspect {}: {}", dir.display(), e))?;
    if crate::workspace::is_windows_reparse_point(&meta) {
        return Err(format!(
            "refusing reparse-point writer-lock directory {}",
            dir.display()
        ));
    }
    if !meta.is_dir() {
        return Err(format!(
            "writer-lock path {} is not a directory",
            dir.display()
        ));
    }
    Ok(())
}

/// Path of the advisory lock file that arbitrates writers for `workspace_root`.
///
/// Keyed by a hash of the canonical workspace root, so two processes that resolve the same tree to
/// the same canonical path agree on one lock file without embedding a (possibly long) path in the
/// file name. Exposed so operators can inspect the lock and tests can simulate a competing process.
pub fn workspace_lock_path(workspace_root: &Path) -> PathBuf {
    let key = canonical_key(workspace_root);
    let digest = crate::prompt::sha256_hex64(key.to_string_lossy().as_bytes());
    workspace_lock_dir().join(format!("{}.lock", digest))
}

/// Take the non-blocking cross-process advisory lock for `canonical_root`.
///
/// Returns `Err(current_owner)` when another process already holds it, `Ok(Some(file))` when this
/// call acquired it (the lock lives until `file` is dropped, which the kernel also does on process
/// exit, so a crash never leaves a stale lock), and `Ok(None)` on targets without an advisory lock.
fn acquire_workspace_lock(canonical_root: &Path, owner_id: &str) -> Result<Option<File>, String> {
    #[cfg(unix)]
    {
        let dir = workspace_lock_dir();
        ensure_lock_dir(&dir)?;
        let path = workspace_lock_path(canonical_root);
        // `O_NOFOLLOW` + `O_EXCL`-free create, then verify with `fstat` (on the descriptor, not the
        // path) that we opened a regular file we own — so a symlink or a foreign file cannot be
        // used to redirect the lock.
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)
            .map_err(|e| format!("cannot open {}: {}", path.display(), e))?;
        // SAFETY: `getuid` has no preconditions and never fails.
        let uid = unsafe { libc::getuid() };
        let meta = file
            .metadata()
            .map_err(|e| format!("cannot inspect {}: {}", path.display(), e))?;
        if !meta.is_file() {
            return Err(format!("{} is not a regular file", path.display()));
        }
        if meta.uid() != uid {
            return Err(format!(
                "writer-lock file {} is owned by uid {}, not the current user {}",
                path.display(),
                meta.uid(),
                uid
            ));
        }
        let fd = file.as_raw_fd();
        // SAFETY: `fd` is a valid, open descriptor owned by `file` for the duration of the call.
        let rc = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
        if rc != 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
                return Err(
                    read_lock_owner(&path).unwrap_or_else(|| "<unknown process>".to_string())
                );
            }
            return Err(format!(
                "advisory lock on {} failed: {}",
                path.display(),
                err
            ));
        }
        // Best-effort diagnostics only: record who holds the lock. Never a correctness dependency.
        let _ = file.set_len(0);
        let _ = file.seek(SeekFrom::Start(0));
        let _ = writeln!(file, "{}\t{}", owner_id, std::process::id());
        let _ = file.sync_all();
        Ok(Some(file))
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Foundation::{
            ERROR_LOCK_VIOLATION, ERROR_SHARING_VIOLATION, FALSE,
        };
        use windows_sys::Win32::Storage::FileSystem::{
            LockFileEx, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE, FILE_SHARE_READ,
            FILE_SHARE_WRITE, LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY,
        };
        use windows_sys::Win32::System::IO::OVERLAPPED;

        let dir = workspace_lock_dir();
        ensure_lock_dir(&dir)?;
        let path = workspace_lock_path(canonical_root);
        // Open with every share mode allowed (so a competitor can still *read* who holds it) and
        // take an exclusive byte-range lock. This is the native counterpart of
        // `flock(LOCK_EX | LOCK_NB)`: `LOCKFILE_FAIL_IMMEDIATELY` makes it non-blocking, and the
        // kernel releases the range when the handle closes — including when the process is killed —
        // so a crashed writer never leaves a stale lease behind.
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(&path)
            .map_err(|e| format!("cannot open {}: {}", path.display(), e))?;
        // Verify on the *handle*, not the path: a reparse point or a non-file cannot be a lock.
        let meta = file
            .metadata()
            .map_err(|e| format!("cannot inspect {}: {}", path.display(), e))?;
        if crate::workspace::is_windows_reparse_point(&meta) {
            return Err(format!(
                "refusing reparse-point writer-lock file {}",
                path.display()
            ));
        }
        if !meta.is_file() {
            return Err(format!("{} is not a regular file", path.display()));
        }
        // SAFETY: `file` owns a valid handle for the whole call; `overlapped` is a zeroed,
        // correctly-sized OVERLAPPED that outlives it.
        let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
        let rc = unsafe {
            LockFileEx(
                std::os::windows::io::AsRawHandle::as_raw_handle(&file) as _,
                LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
                0,
                1,
                0,
                &mut overlapped,
            )
        };
        if rc == FALSE {
            let err = std::io::Error::last_os_error();
            let code = err.raw_os_error().unwrap_or(0) as u32;
            if code == ERROR_LOCK_VIOLATION || code == ERROR_SHARING_VIOLATION {
                return Err(
                    read_lock_owner(&path).unwrap_or_else(|| "<unknown process>".to_string())
                );
            }
            return Err(format!("writer lock on {} failed: {}", path.display(), err));
        }
        // Best-effort diagnostics only: record who holds the lock.
        let _ = file.set_len(0);
        let _ = file.seek(SeekFrom::Start(0));
        let _ = writeln!(file, "{}\t{}", owner_id, std::process::id());
        let _ = file.sync_all();
        Ok(Some(file))
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (canonical_root, owner_id);
        Ok(None)
    }
}

/// Read the owner id the lock holder recorded, for a conflict message.
#[cfg(any(unix, windows))]
fn read_lock_owner(path: &Path) -> Option<String> {
    let mut file = File::open(path).ok()?;
    let mut text = String::new();
    file.read_to_string(&mut text).ok()?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Record that `owner_id` now owns `session_id` in `workspace_root`.
pub fn register_session(session_id: &str, workspace_root: &Path, owner_id: &str) {
    let mut reg = registry().lock().unwrap_or_else(|e| e.into_inner());
    reg.routes.insert(
        session_id.to_string(),
        SessionRoute {
            session_id: session_id.to_string(),
            workspace_root: canonical_key(workspace_root),
            owner_id: owner_id.to_string(),
        },
    );
}

/// Drop `session_id` from the routing table (session closed / runtime shut down).
pub fn unregister_session(session_id: &str) {
    registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .routes
        .remove(session_id);
}

/// Resolve a session id to the runtime and workspace that own it.
pub fn lookup_session_route(session_id: &str) -> Option<SessionRoute> {
    registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .routes
        .get(session_id)
        .cloned()
}

/// All sessions the routing table knows about for a workspace, sorted by session id.
pub fn sessions_for_workspace(workspace_root: &Path) -> Vec<SessionRoute> {
    let key = canonical_key(workspace_root);
    let mut routes: Vec<SessionRoute> = registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .routes
        .values()
        .filter(|route| route.workspace_root == key)
        .cloned()
        .collect();
    routes.sort_by(|a, b| a.session_id.cmp(&b.session_id));
    routes
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serializes the tests that hold raw lock file descriptors and the tests that spawn child
    /// processes: `fork` briefly duplicates every open descriptor into the child before `exec`
    /// closes the `CLOEXEC` ones, so running them concurrently makes a held `flock` appear held by
    /// a child for a few microseconds. Taking this guard removes that test-only race.
    #[cfg(unix)]
    static PROCESS_FORK_GUARD: Mutex<()> = Mutex::new(());

    #[test]
    fn different_workspaces_are_independent_and_same_workspace_is_exclusive() {
        let root = std::env::temp_dir().join(format!("kroma-own-{}", uuid::Uuid::new_v4()));
        let a = root.join("a");
        let b = root.join("b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();

        let lease_a = try_acquire_writer(&a, "owner-a").unwrap();
        let lease_b = try_acquire_writer(&b, "owner-b").unwrap();
        assert_eq!(lease_a.owner_id(), "owner-a");
        assert_eq!(lease_b.owner_id(), "owner-b");

        let conflict = try_acquire_writer(&a, "owner-c").unwrap_err();
        assert_eq!(conflict.current_owner, "owner-a");
        assert_eq!(current_writer(&a).as_deref(), Some("owner-a"));

        // Same-owner re-claim is idempotent, not a conflict.
        let again = try_acquire_writer(&a, "owner-a").unwrap();

        // KR-D01: dropping the *outer* lease while the re-entrant one is still alive must not
        // release the claim. The registry is refcounted and the file lock is shared, so a third
        // owner is still refused and the cross-process flock is still held.
        drop(lease_a);
        assert_eq!(current_writer(&a).as_deref(), Some("owner-a"));
        let conflict = try_acquire_writer(&a, "owner-c").unwrap_err();
        assert_eq!(conflict.current_owner, "owner-a");

        // Only the last lease of the owner releases the workspace.
        drop(again);
        assert_eq!(current_writer(&a), None);
        let lease_c = try_acquire_writer(&a, "owner-c").unwrap();
        drop(lease_c);
        drop(lease_b);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn session_routes_are_scoped_to_the_workspace() {
        let root = std::env::temp_dir().join(format!("kroma-route-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let sid = format!("s-{}", uuid::Uuid::new_v4());
        register_session(&sid, &root, "runtime-1");
        let route = lookup_session_route(&sid).unwrap();
        assert_eq!(route.owner_id, "runtime-1");
        assert_eq!(route.workspace_root, canonical_key(&root));
        assert_eq!(sessions_for_workspace(&root).len(), 1);
        unregister_session(&sid);
        assert!(lookup_session_route(&sid).is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// P1-4: a writer in *another process* is rejected too. A second flock on the workspace lock
    /// file (which is exactly what a second `kroma serve` process does) is held here to stand in
    /// for that process, since the advisory lock is arbitrated by the kernel across processes.
    #[cfg(unix)]
    #[test]
    fn a_competing_process_lock_blocks_the_writer_and_is_reclaimed_on_release() {
        let _guard = PROCESS_FORK_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        use std::io::Write;
        use std::os::unix::io::AsRawFd;

        let root = std::env::temp_dir().join(format!("kroma-xproc-{}", uuid::Uuid::new_v4()));
        let ws = root.join("ws");
        std::fs::create_dir_all(&ws).unwrap();

        let lock_path = workspace_lock_path(&ws);
        std::fs::create_dir_all(lock_path.parent().unwrap()).unwrap();
        let mut competitor = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .unwrap();
        // SAFETY: `competitor` owns a valid open descriptor for the duration of the call.
        let rc = unsafe { libc::flock(competitor.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        assert_eq!(rc, 0, "the competitor must take the lock");
        writeln!(competitor, "other-process\t424242").unwrap();

        let conflict = try_acquire_writer(&ws, "owner-in-this-process").unwrap_err();
        assert!(
            conflict.current_owner.contains("other-process"),
            "the conflict must name the other process, got {:?}",
            conflict.current_owner
        );

        // Releasing the competitor's lock (its process exited) frees the workspace immediately:
        // no stale lock window to reap.
        drop(competitor);
        let lease = try_acquire_writer(&ws, "owner-in-this-process").unwrap();
        drop(lease);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The lock file must never be a symlink an attacker planted: `O_NOFOLLOW` + `fstat` refuse it
    /// instead of writing through it.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_lock_file_is_refused() {
        let _guard = PROCESS_FORK_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let root = std::env::temp_dir().join(format!("kroma-locklink-{}", uuid::Uuid::new_v4()));
        let ws = root.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let dir = workspace_lock_dir();
        ensure_lock_dir(&dir).unwrap();
        let lock = workspace_lock_path(&ws);
        let _ = std::fs::remove_file(&lock);
        let target = root.join("hostile-target");
        std::fs::write(&target, b"hostile").unwrap();
        std::os::unix::fs::symlink(&target, &lock).unwrap();

        let result = try_acquire_writer(&ws, "owner");
        assert!(
            result.is_err(),
            "a symlinked lock file must be refused, not followed"
        );
        assert_eq!(
            std::fs::read(&target).unwrap(),
            b"hostile",
            "the hostile symlink target must not be written through"
        );

        let _ = std::fs::remove_file(&lock);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Child helper driven by [`different_tmpdir_does_not_create_a_second_lock_domain`]. It is a
    /// no-op in a normal run; the parent sets `KROMA_LOCK_ROLE` and the paths before re-executing
    /// this test binary with `--exact`.
    #[cfg(unix)]
    #[test]
    fn lock_holder_child() {
        let Ok(role) = std::env::var("KROMA_LOCK_ROLE") else {
            return;
        };
        let ws = std::env::var("KROMA_LOCK_WS").unwrap();
        let ready = PathBuf::from(std::env::var("KROMA_LOCK_READY").unwrap());
        let release = PathBuf::from(std::env::var("KROMA_LOCK_RELEASE").unwrap());
        match role.as_str() {
            "hold" => {
                let lease = try_acquire_writer(Path::new(&ws), "child-holder")
                    .expect("child holder must acquire the lock");
                std::fs::write(&ready, b"ready").unwrap();
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
                while !release.exists() && std::time::Instant::now() < deadline {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                drop(lease);
            }
            "try" => match try_acquire_writer(Path::new(&ws), "child-tryer") {
                Ok(_) => println!("KROMA_LOCK_RESULT=ACQUIRED"),
                Err(_) => println!("KROMA_LOCK_RESULT=BLOCKED"),
            },
            _ => {}
        }
    }

    /// P1-4 regression: the writer-lock domain must not depend on `TMPDIR`. Two real child processes
    /// with *different* `TMPDIR` values still contend on one lock, so the second is blocked. (The
    /// previous `std::env::temp_dir()` domain gave each process its own lock and let both write.)
    #[cfg(unix)]
    #[test]
    fn different_tmpdir_does_not_create_a_second_lock_domain() {
        let _guard = PROCESS_FORK_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let exe = std::env::current_exe().unwrap();
        let root = std::env::temp_dir().join(format!("kroma-tmpdir-{}", uuid::Uuid::new_v4()));
        let ws = root.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let ready = root.join("ready");
        let release = root.join("release");
        let tmp_a = root.join("tmp-a");
        let tmp_b = root.join("tmp-b");
        std::fs::create_dir_all(&tmp_a).unwrap();
        std::fs::create_dir_all(&tmp_b).unwrap();

        let child_command = |role: &str, tmpdir: &Path| {
            let mut cmd = std::process::Command::new(&exe);
            cmd.args([
                "--exact",
                "session_ownership::tests::lock_holder_child",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("KROMA_LOCK_ROLE", role)
            .env("KROMA_LOCK_WS", &ws)
            .env("KROMA_LOCK_READY", &ready)
            .env("KROMA_LOCK_RELEASE", &release)
            .env("TMPDIR", tmpdir);
            cmd
        };

        let mut holder = child_command("hold", &tmp_a)
            .spawn()
            .expect("holder child starts");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while !ready.exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "holder child never acquired the lock"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }

        // A process under a different TMPDIR must still be blocked by the same lock.
        let blocked = child_command("try", &tmp_b).output().unwrap();
        let blocked_text = String::from_utf8_lossy(&blocked.stdout);
        assert!(
            blocked_text.contains("KROMA_LOCK_RESULT=BLOCKED"),
            "a different TMPDIR must not open a second lock domain; got: {}",
            blocked_text
        );

        std::fs::write(&release, b"go").unwrap();
        let _ = holder.wait();

        // Once the holder exits, either TMPDIR can acquire.
        let acquired = child_command("try", &tmp_b).output().unwrap();
        let acquired_text = String::from_utf8_lossy(&acquired.stdout);
        assert!(
            acquired_text.contains("KROMA_LOCK_RESULT=ACQUIRED"),
            "the lock must be free after the holder exits; got: {}",
            acquired_text
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    // ---- T08 (KR-26): Windows cross-process ownership -------------------------------------------

    /// Lock identity is a *workspace* identity, not a spelling: `.`/`..` components and a trailing
    /// separator must all resolve to one lock file (the Windows half additionally folds case and the
    /// verbatim prefix, see `windows_path_aliases_share_lock_domain`).
    #[test]
    fn lock_path_is_stable_across_equivalent_spellings() {
        let root = std::env::temp_dir().join(format!("kroma-alias-{}", uuid::Uuid::new_v4()));
        let ws = root.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::create_dir_all(ws.join("sub")).unwrap();
        let plain = workspace_lock_path(&ws);
        let dotted = workspace_lock_path(&ws.join("."));
        let nested = workspace_lock_path(&ws.join("sub").join(".."));
        assert_eq!(
            plain, dotted,
            "a `.` component must not create a new domain"
        );
        assert_eq!(
            plain, nested,
            "a `..` component must not create a new domain"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Reader/writer compatibility matrix, as implemented:
    ///
    /// | holder \ asker | writer | reader |
    /// | --- | --- | --- |
    /// | none | allowed | allowed |
    /// | writer | **refused** | allowed |
    /// | reader | allowed | allowed |
    ///
    /// A reader takes no lease, so it is never blocked — and never blocks. It reads the last
    /// committed WAL state. That is the documented behaviour rather than an accident, so it is
    /// pinned here: while a writer lease is held, an unleased reader can still open the workspace.
    #[test]
    fn a_reader_is_never_blocked_by_a_writer_lease() {
        let root = std::env::temp_dir().join(format!("kroma-matrix-{}", uuid::Uuid::new_v4()));
        let ws = root.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(ws.join("file.txt"), b"committed").unwrap();

        let lease = try_acquire_writer(&ws, "writer").unwrap();
        // A reader path: no lease, plain read of the workspace.
        assert_eq!(std::fs::read(ws.join("file.txt")).unwrap(), b"committed");
        // A second writer is refused, naming the holder.
        let conflict = try_acquire_writer(&ws, "second-writer").unwrap_err();
        assert_eq!(conflict.current_owner, "writer");
        drop(lease);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Windows runtime gate — **must run on a real Windows runner**.
    ///
    /// Two independent child processes compete for one workspace: the holder takes the lease and
    /// acknowledges, the parent (a third process) is refused, then the holder is killed and the
    /// lease is reacquired without any stale-lock window. Compiled for `x86_64-pc-windows-gnu` as
    /// part of T08; execution is recorded as BLOCKED where no Windows runner is available.
    #[cfg(windows)]
    #[test]
    fn windows_two_processes_same_workspace_have_one_writer() {
        let root = std::env::temp_dir().join(format!("kroma-win-xproc-{}", uuid::Uuid::new_v4()));
        let ws = root.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let ack = root.join("holder.ack");
        let exe = std::env::current_exe().unwrap();
        let mut child = std::process::Command::new(&exe)
            .args([
                "--exact",
                "session_ownership::tests::windows_lock_holder_child",
                "--nocapture",
            ])
            .env("KROMA_WIN_LOCK_WS", &ws)
            .env("KROMA_WIN_LOCK_ACK", &ack)
            .spawn()
            .expect("spawn the holder process");

        // Wait (bounded) for the child to actually hold the lease.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while !ack.exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "the holder never acknowledged"
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        }

        let conflict = try_acquire_writer(&ws, "parent-process")
            .expect_err("a second process must not become the writer");
        assert!(
            conflict.current_owner.contains("windows-holder"),
            "the conflict must name the other process, got {:?}",
            conflict.current_owner
        );

        // Kill the holder: the OS releases the byte-range lock with the handle, so the workspace is
        // immediately reclaimable and there is no stale lease to reap.
        child.kill().expect("kill the holder");
        let _ = child.wait();
        let lease = try_acquire_writer(&ws, "parent-process")
            .expect("a killed writer must release its lease");
        drop(lease);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Child helper for [`windows_two_processes_same_workspace_have_one_writer`]. A no-op unless the
    /// parent set `KROMA_WIN_LOCK_WS`; it holds the lease until it is killed.
    #[cfg(windows)]
    #[test]
    fn windows_lock_holder_child() {
        let Ok(ws) = std::env::var("KROMA_WIN_LOCK_WS") else {
            return;
        };
        let ack = std::env::var("KROMA_WIN_LOCK_ACK").expect("ack path");
        let _lease = try_acquire_writer(Path::new(&ws), "windows-holder")
            .expect("the holder must acquire the lease");
        std::fs::write(&ack, b"held").unwrap();
        std::thread::sleep(std::time::Duration::from_secs(120));
    }

    /// Windows path aliases must land on one lock domain: same directory spelled with different
    /// case, a trailing separator, or the `\\?\` verbatim prefix.
    #[cfg(windows)]
    #[test]
    fn windows_path_aliases_share_lock_domain() {
        let root = std::env::temp_dir().join(format!("kroma-win-alias-{}", uuid::Uuid::new_v4()));
        let ws = root.join("Workspace");
        std::fs::create_dir_all(&ws).unwrap();

        let canonical = workspace_lock_path(&ws);
        let upper = workspace_lock_path(&PathBuf::from(ws.to_string_lossy().to_uppercase()));
        let verbatim = workspace_lock_path(&PathBuf::from(format!(r"\\?\{}", ws.display())));
        let trailing = workspace_lock_path(&PathBuf::from(format!("{}\\", ws.display())));
        assert_eq!(canonical, upper, "case must not create a second domain");
        assert_eq!(canonical, verbatim, "the verbatim prefix must not either");
        assert_eq!(canonical, trailing, "a trailing separator must not either");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// KR-D01: a re-entrant lease is a real lease. While it is alive the workspace still has a
    /// writer, so a third owner must be refused and the file lock must still be held.
    #[test]
    fn a_reentrant_lease_keeps_the_workspace_claimed() {
        let root = std::env::temp_dir().join(format!("kroma-reent-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();

        let outer = try_acquire_writer(&root, "owner-a").unwrap();
        let inner = try_acquire_writer(&root, "owner-a").unwrap();
        assert_eq!(current_writer(&root).as_deref(), Some("owner-a"));

        drop(outer);
        assert_eq!(
            current_writer(&root).as_deref(),
            Some("owner-a"),
            "the inner lease is still alive, so the workspace is still claimed"
        );
        let conflict = try_acquire_writer(&root, "owner-b").unwrap_err();
        assert_eq!(conflict.current_owner, "owner-a");

        drop(inner);
        assert_eq!(current_writer(&root), None);
        // After the last lease is gone the workspace is genuinely free.
        let other = try_acquire_writer(&root, "owner-b").unwrap();
        assert_eq!(other.owner_id(), "owner-b");
        drop(other);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// KR-D01: dropping the inner lease first must leave the outer claim intact, and a further
    /// same-owner acquire must still succeed.
    #[test]
    fn dropping_the_inner_lease_first_keeps_the_outer_claim() {
        let root = std::env::temp_dir().join(format!("kroma-reent2-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();

        let outer = try_acquire_writer(&root, "owner-a").unwrap();
        let inner = try_acquire_writer(&root, "owner-a").unwrap();
        drop(inner);

        assert_eq!(current_writer(&root).as_deref(), Some("owner-a"));
        assert!(
            try_acquire_writer(&root, "owner-b").is_err(),
            "the outer claim must survive the inner drop"
        );
        let third = try_acquire_writer(&root, "owner-a").unwrap();
        assert_eq!(third.owner_id(), "owner-a");
        drop(third);
        assert_eq!(current_writer(&root).as_deref(), Some("owner-a"));
        drop(outer);
        assert_eq!(current_writer(&root), None);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// KR-D01: an unwinding panic must release every lease it drops, and must not release a claim
    /// it never owned.
    #[test]
    fn a_panicking_scope_releases_its_lease() {
        let root = std::env::temp_dir().join(format!("kroma-panic-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();

        let result = std::panic::catch_unwind(|| {
            let _lease = try_acquire_writer(&root, "owner-panic").unwrap();
            panic!("boom");
        });
        assert!(result.is_err());
        assert_eq!(
            current_writer(&root),
            None,
            "the lease must be released while unwinding"
        );
        let after = try_acquire_writer(&root, "owner-b").unwrap();
        drop(after);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// KR-D02: a workspace that does not exist yet must still have exactly one writer slot.
    /// `canonicalize` fails on a missing path, so the lexical fallback has to remove the aliases.
    #[test]
    fn a_missing_workspace_still_has_one_identity_per_path() {
        let root = std::env::temp_dir().join(format!("kroma-alias-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let missing = root.join("not-created-yet");

        let absolute = canonical_key(&missing);
        let with_dot = canonical_key(&root.join(".").join("not-created-yet"));
        let with_parent = canonical_key(&root.join("sub").join("..").join("not-created-yet"));
        let trailing = canonical_key(&PathBuf::from(format!("{}/", missing.display())));

        assert_eq!(absolute, with_dot, "a `.` component is not a new workspace");
        assert_eq!(
            absolute, with_parent,
            "a `..` component is not a new workspace"
        );
        assert_eq!(
            absolute, trailing,
            "a trailing separator is not a new workspace"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// KR-D02: a relative spelling resolves against the cwd, not to its own slot.
    ///
    /// This exercises `lexical_normalize` directly rather than calling `set_current_dir`: that is
    /// process-global state and mutating it from a test breaks every other test running in parallel.
    #[test]
    fn a_relative_spelling_resolves_against_the_cwd() {
        let cwd = std::env::current_dir().unwrap();
        let relative = lexical_normalize(Path::new("some/rel/ws"));
        let absolute = lexical_normalize(&cwd.join("some/rel/ws"));
        assert_eq!(
            relative, absolute,
            "a relative spelling must resolve against the cwd"
        );
    }

    /// KR-D02: two spellings of the same existing directory (a symlink and the real path) must
    /// share one writer slot, so a symlink cannot be used to get a second writer.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_spelling_shares_the_writers_slot() {
        let root = std::env::temp_dir().join(format!("kroma-link-{}", uuid::Uuid::new_v4()));
        let real = root.join("real");
        std::fs::create_dir_all(&real).unwrap();
        let link = root.join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let lease = try_acquire_writer(&link, "owner-a").unwrap();
        let conflict = try_acquire_writer(&real, "owner-b").unwrap_err();
        assert_eq!(
            conflict.current_owner, "owner-a",
            "the real path must not be a second writer slot"
        );
        drop(lease);
        assert_eq!(current_writer(&real), None);
        let _ = std::fs::remove_dir_all(&root);
    }
}
