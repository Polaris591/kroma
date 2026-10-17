//! Shared path-based filesystem hardening primitives (single source of truth).
//!
//! Every place in Kroma that opens, creates or permission-restricts a *path* that must never
//! follow a symlink goes through this module, so the "refuse the symlink, then `fchmod` the
//! descriptor" policy exists exactly once. Duplicating that policy is what previously let a
//! hardening pass fix two copies and miss a third.
//!
//! Deliberately *not* here: the kernel-level `openat2(RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS)`
//! workspace sandbox in [`crate::workspace`]. That walk is relative to a directory descriptor and
//! has different semantics, so it is not a copy of these path-based primitives.

#[cfg(unix)]
use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::path::{Component, Path, PathBuf};

#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
#[cfg(unix)]
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};

/// `custom_flags` that refuse to follow a symlinked leaf: a symlink fails with `ELOOP` instead of
/// being traversed, and `O_CLOEXEC` keeps the descriptor from leaking into spawned children.
#[cfg(unix)]
pub(crate) const NO_FOLLOW_CUSTOM_FLAGS: libc::c_int = libc::O_NOFOLLOW | libc::O_CLOEXEC;

/// `open`/`openat` flags for reading a file without following a final symlink.
#[cfg(unix)]
pub(crate) const OPEN_FILE_NO_FOLLOW: libc::c_int = libc::O_RDONLY | NO_FOLLOW_CUSTOM_FLAGS;

/// `open`/`openat` flags for opening a directory without following a final symlink.
#[cfg(unix)]
pub(crate) const OPEN_DIR_NO_FOLLOW: libc::c_int =
    libc::O_RDONLY | libc::O_DIRECTORY | NO_FOLLOW_CUSTOM_FLAGS;

/// Failure from a hardening primitive.
#[derive(Debug)]
pub(crate) enum SecureFsError {
    /// The path itself is unacceptable: a symlinked component, a non-directory, no directory
    /// component at all, or an embedded NUL byte.
    InvalidPath(String),
    /// The underlying OS call failed. The original error is preserved so callers can still inspect
    /// `raw_os_error()` (`ELOOP`, `ENOTDIR`, ...).
    Io(std::io::Error),
}

impl From<std::io::Error> for SecureFsError {
    fn from(err: std::io::Error) -> Self {
        SecureFsError::Io(err)
    }
}

impl std::fmt::Display for SecureFsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SecureFsError::InvalidPath(reason) => write!(f, "{}", reason),
            SecureFsError::Io(err) => write!(f, "{}", err),
        }
    }
}

impl std::error::Error for SecureFsError {}

/// Open an existing file for reading without ever following a final symlink.
///
/// `O_NOFOLLOW` makes a symlinked leaf fail with `ELOOP` instead of being traversed. On Windows the
/// reparse-point flag is checked both before and after the open, because `FILE_FLAG_OPEN_REPARSE_POINT`
/// alone still lets a swap between the two slip through.
pub(crate) fn open_existing_no_symlink(path: &Path) -> std::io::Result<File> {
    #[cfg(unix)]
    {
        OpenOptions::new()
            .read(true)
            .custom_flags(NO_FOLLOW_CUSTOM_FLAGS)
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

/// Apply "never follow a symlink" hardening to an [`OpenOptions`] builder.
///
/// This is the one place that decides *which* flag expresses that policy:
/// `O_NOFOLLOW | O_CLOEXEC` on Unix (a symlinked leaf fails with `ELOOP` instead of being
/// traversed) and `FILE_FLAG_OPEN_REPARSE_POINT` on Windows (the handle refers to the reparse point
/// itself rather than its target).
///
/// Callers that must also reject a reparse-point *leaf* on Windows — the flag alone is not enough,
/// because the entry can be swapped between the metadata check and the open — should pair this with
/// [`crate::workspace::is_symlink_or_reparse`] on the opened handle.
pub(crate) fn apply_no_follow(opts: &mut OpenOptions) {
    #[cfg(unix)]
    {
        opts.custom_flags(NO_FOLLOW_CUSTOM_FLAGS);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
        opts.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = opts;
    }
}

/// Create a brand-new file exclusively and without following a symlink, at `mode` on Unix.
///
/// `create_new(true)` fails with `EEXIST` if the path already exists (so a pre-planted symlink is
/// refused rather than followed) and `O_NOFOLLOW` closes the race where the entry is swapped
/// between the existence check and the open. `mode` sets the permissions at creation, so no
/// path-based `chmod` is needed; it is masked by the process umask.
pub(crate) fn create_new_no_symlink(path: &Path, mode: u32) -> std::io::Result<File> {
    #[cfg(unix)]
    {
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .custom_flags(NO_FOLLOW_CUSTOM_FLAGS)
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

/// `fchmod` an existing file to `mode` without ever `chmod`-ing it by path.
///
/// `fs::set_permissions` is a `chmod` that follows symlinks, so a same-user attacker who swaps the
/// final component between the write and the `chmod` could redirect it to an arbitrary target. Open
/// the leaf with `O_NOFOLLOW` (refusing a symlink with `ELOOP`) and `fchmod` the resulting
/// descriptor instead.
#[cfg(unix)]
pub(crate) fn restrict_file_no_follow(path: &Path, mode: u32) -> std::io::Result<()> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(NO_FOLLOW_CUSTOM_FLAGS)
        .open(path)?;
    file.set_permissions(fs::Permissions::from_mode(mode))
}

/// Windows has no `fchmod` equivalent and the ACL is inherited from the parent, so this is a no-op
/// — matching the previous per-module behaviour.
#[cfg(not(unix))]
pub(crate) fn restrict_file_no_follow(_path: &Path, _mode: u32) -> std::io::Result<()> {
    Ok(())
}

/// Directory counterpart of [`restrict_file_no_follow`]: open with `O_NOFOLLOW | O_DIRECTORY` and
/// `fchmod`, so a symlinked final component is refused rather than traversed.
#[cfg(unix)]
pub(crate) fn restrict_dir_no_follow(path: &Path, mode: u32) -> std::io::Result<()> {
    let dir = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | NO_FOLLOW_CUSTOM_FLAGS)
        .open(path)?;
    dir.set_permissions(fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
pub(crate) fn restrict_dir_no_follow(_path: &Path, _mode: u32) -> std::io::Result<()> {
    Ok(())
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
pub(crate) fn ensure_private_dir(dir: &Path) -> Result<(), SecureFsError> {
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
            return Err(SecureFsError::InvalidPath(format!(
                "snapshot store path '{}' has no directory component",
                dir.display()
            )));
        }
        let anchor = if absolute {
            Path::new("/")
        } else {
            Path::new(".")
        };
        let mut collapsed = PathBuf::from(anchor);
        for name in &components {
            collapsed.push(name);
        }

        // Fast path: every component is a real directory and we can walk it with dirfds.
        if walk_private_dir(anchor, &components).is_ok() {
            return Ok(());
        }

        // Portable path: some platforms ship OS-owned symlinked ancestors that no user code may
        // rewrite — Android's `/data/data` is a symlink to `/data/user/0`, and macOS `/var`,
        // `/tmp` and `/etc` are symlinks too. Refusing those would make the store unusable on
        // those systems, so fall back to a path-based create that only insists on the *final*
        // component being a real directory: that is the component an attacker would swap to
        // redirect the chmod, and pre-existing ancestors keep their own permissions either way.
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true);
        builder.mode(0o700);
        builder.create(&collapsed).map_err(|e| {
            SecureFsError::Io(std::io::Error::new(
                e.kind(),
                format!(
                    "failed to create snapshot store '{}': {}",
                    collapsed.display(),
                    e
                ),
            ))
        })?;
        // F02: never `chmod` by path. The create above deliberately follows OS-owned symlinked
        // ancestors (the reason this fallback exists), but between that create and a path-based
        // `chmod` an attacker could swap the *final* component and redirect the `chmod` to their
        // target. Open the final component with `O_NOFOLLOW | O_DIRECTORY` and `fchmod` that
        // descriptor instead: a symlink leaf is refused (`ELOOP`) and a non-directory is refused
        // (`ENOTDIR`), so only the verified directory itself is ever restricted.
        let handle = open_real_dir(&collapsed).map_err(|e| match &e {
            SecureFsError::Io(io)
                if matches!(io.raw_os_error(), Some(libc::ELOOP) | Some(libc::ENOTDIR)) =>
            {
                SecureFsError::InvalidPath(format!(
                    "snapshot store path '{}' is not a real directory",
                    collapsed.display()
                ))
            }
            _ => e,
        })?;
        handle
            .set_permissions(fs::Permissions::from_mode(0o700))
            .map_err(|e| {
                SecureFsError::Io(std::io::Error::new(
                    e.kind(),
                    format!(
                        "failed to restrict snapshot store '{}': {}",
                        collapsed.display(),
                        e
                    ),
                ))
            })?;
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
            return Err(SecureFsError::InvalidPath(format!(
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
                    return Err(SecureFsError::InvalidPath(format!(
                        "snapshot store parent '{}' is a reparse point",
                        parent.display()
                    )));
                }
            }
        }
        match fs::symlink_metadata(&collapsed) {
            Ok(meta) => {
                if crate::workspace::is_windows_reparse_point(&meta) || !meta.is_dir() {
                    return Err(SecureFsError::InvalidPath(format!(
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
        let _ =
            crate::workspace::open_windows_handle_no_reparse(&collapsed, &collapsed, Some(true))
                .map_err(|e| SecureFsError::InvalidPath(e.to_string()))?;
        Ok(())
    }
}

/// [`ensure_private_dir`] for callers that only speak `std::io::Error` (e.g. the session WAL
/// store). A [`SecureFsError::Io`] is returned unchanged so callers keep the OS error kind; any
/// other error is folded into an `io::Error` with the same message.
#[cfg(unix)]
pub(crate) fn ensure_private_dir_io(dir: &Path) -> std::io::Result<()> {
    ensure_private_dir(dir).map_err(|e| match e {
        SecureFsError::Io(io) => io,
        other => std::io::Error::new(std::io::ErrorKind::Other, other.to_string()),
    })
}

/// `fsync` a directory so a rename/unlink/mkdir within it survives a power loss.
///
/// On Unix the directory is opened with [`open_existing_no_symlink`] so a symlinked store directory
/// is refused rather than followed; platforms without directory `fsync` treat this as a best-effort
/// no-op. This is the single place every durability seam (snapshot store, session WAL) syncs a
/// directory, so the no-follow policy and the test fault seam live in exactly one module.
pub(crate) fn sync_dir(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        let handle = open_existing_no_symlink(dir)?;
        sync_dir_handle(&handle)
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        Ok(())
    }
}

/// [`sync_dir`] for a caller that already holds the verified directory descriptor (the atomic-write
/// path in `tools::write` and the workspace-relative removals in `revision`). Syncing the handle
/// avoids re-opening by path and races nothing.
#[cfg(unix)]
pub(crate) fn sync_dir_handle(handle: &File) -> std::io::Result<()> {
    #[cfg(test)]
    if dir_sync_fault_armed() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            "injected directory-sync failure",
        ));
    }
    handle.sync_all()
}

/// Test-only RAII guard that makes every directory `fsync` on the current thread fail until it is
/// dropped. Instance-free but thread-scoped, so a test that arms it cannot perturb another test
/// running in parallel; it exists so a directory-sync failure can be exercised without relying on
/// filesystem permissions (which root ignores).
#[cfg(all(test, unix))]
pub(crate) struct DirSyncFaultGuard;

#[cfg(all(test, unix))]
impl DirSyncFaultGuard {
    pub(crate) fn arm() -> Self {
        DIR_SYNC_FAILING.with(|flag| flag.set(true));
        Self
    }
}

#[cfg(all(test, unix))]
impl Drop for DirSyncFaultGuard {
    fn drop(&mut self) {
        DIR_SYNC_FAILING.with(|flag| flag.set(false));
    }
}

#[cfg(all(test, unix))]
thread_local! {
    static DIR_SYNC_FAILING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(all(test, unix))]
fn dir_sync_fault_armed() -> bool {
    DIR_SYNC_FAILING.with(|flag| flag.get())
}

/// Strict dirfd walk: create every missing component with `mkdirat` and re-open each one with
/// `O_NOFOLLOW | O_DIRECTORY`, so a symlinked component is refused rather than traversed. Only the
/// final component is chmod-ed: pre-existing ancestors (e.g. `/tmp`) keep their own permissions and
/// are never touched.
///
/// Fails on platforms whose *own* system paths contain symlinked ancestors (Android's `/data/data`
/// -> `/data/user/0`, macOS `/var` -> `/private/var`); callers fall back to [`ensure_private_dir`]'s
/// portable path-based branch in that case.
#[cfg(unix)]
fn walk_private_dir(anchor: &Path, components: &[std::ffi::OsString]) -> Result<(), SecureFsError> {
    let mut handle = open_real_dir(anchor)?;
    for (index, name) in components.iter().enumerate() {
        let name_c = CString::new(name.as_bytes()).map_err(|_| {
            SecureFsError::InvalidPath("snapshot store path contains a NUL byte".to_string())
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

/// Open `path` as a directory without ever following a final symlink.
#[cfg(unix)]
fn open_real_dir(path: &Path) -> Result<File, SecureFsError> {
    let handle = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | NO_FOLLOW_CUSTOM_FLAGS)
        .open(path)?;
    verify_is_real_dir(&handle, &path.display().to_string())?;
    Ok(handle)
}

/// `openat` a child directory of an already-verified directory descriptor, refusing symlinks.
#[cfg(unix)]
fn open_dir_at(parent_fd: RawFd, name: &CString) -> Result<File, SecureFsError> {
    let fd = unsafe { libc::openat(parent_fd, name.as_ptr(), OPEN_DIR_NO_FOLLOW) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let handle = unsafe { File::from_raw_fd(fd) };
    verify_is_real_dir(&handle, &name.to_string_lossy())?;
    Ok(handle)
}

/// `fstat` the open descriptor and reject anything that is not a directory.
#[cfg(unix)]
fn verify_is_real_dir(handle: &File, display: &str) -> Result<(), SecureFsError> {
    let meta = handle.metadata()?;
    if !meta.is_dir() {
        return Err(SecureFsError::InvalidPath(format!(
            "snapshot store path '{}' is not a real directory",
            display
        )));
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// Every test gets its own directory so the process-wide umask and any parallel test cannot
    /// interfere with the permission assertions.
    fn scratch(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("kroma-securefs-{}-{}", std::process::id(), tag));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn mode_of(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    // ---------------------------------------------------------------- flag constants

    #[test]
    fn flag_constants_carry_the_no_follow_policy() {
        assert_eq!(NO_FOLLOW_CUSTOM_FLAGS & libc::O_NOFOLLOW, libc::O_NOFOLLOW);
        assert_eq!(NO_FOLLOW_CUSTOM_FLAGS & libc::O_CLOEXEC, libc::O_CLOEXEC);
        assert_eq!(OPEN_FILE_NO_FOLLOW, libc::O_RDONLY | NO_FOLLOW_CUSTOM_FLAGS);
        assert_eq!(
            OPEN_DIR_NO_FOLLOW,
            libc::O_RDONLY | libc::O_DIRECTORY | NO_FOLLOW_CUSTOM_FLAGS
        );
    }

    // ---------------------------------------------------------------- open_existing_no_symlink

    #[test]
    fn open_existing_no_symlink_reads_a_regular_file() {
        use std::io::Read;
        let dir = scratch("open-regular");
        let file = dir.join("payload");
        fs::write(&file, b"contents").unwrap();

        let mut opened = open_existing_no_symlink(&file).unwrap();
        let mut buf = String::new();
        opened.read_to_string(&mut buf).unwrap();
        assert_eq!(buf, "contents");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn open_existing_no_symlink_refuses_a_symlinked_leaf() {
        let dir = scratch("open-symlink");
        let victim = dir.join("victim");
        fs::write(&victim, b"secret").unwrap();
        let link = dir.join("link");
        std::os::unix::fs::symlink(&victim, &link).unwrap();

        let err =
            open_existing_no_symlink(&link).expect_err("a symlinked leaf must not be traversed");
        assert_eq!(
            err.raw_os_error(),
            Some(libc::ELOOP),
            "O_NOFOLLOW must fail with ELOOP, got {:?}",
            err
        );
        let _ = fs::remove_dir_all(&dir);
    }

    // ---------------------------------------------------------------- create_new_no_symlink

    #[test]
    fn create_new_no_symlink_creates_at_the_requested_mode() {
        let dir = scratch("create-new");
        let file = dir.join("fresh");
        let handle = create_new_no_symlink(&file, 0o600).unwrap();
        drop(handle);
        assert_eq!(mode_of(&file), 0o600);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn create_new_no_symlink_refuses_an_existing_path() {
        let dir = scratch("create-existing");
        let file = dir.join("already-there");
        fs::write(&file, b"keep me").unwrap();

        create_new_no_symlink(&file, 0o600).expect_err("create_new must refuse an existing path");
        assert_eq!(fs::read(&file).unwrap(), b"keep me");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn create_new_no_symlink_does_not_follow_a_planted_symlink() {
        let dir = scratch("create-symlink");
        let victim = dir.join("victim");
        fs::write(&victim, b"original").unwrap();
        let link = dir.join("planted");
        std::os::unix::fs::symlink(&victim, &link).unwrap();

        create_new_no_symlink(&link, 0o600)
            .expect_err("a pre-planted symlink must be refused with EEXIST, not followed");
        assert_eq!(
            fs::read(&victim).unwrap(),
            b"original",
            "the symlink target must be untouched"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    // ---------------------------------------------------------------- restrict_*_no_follow

    #[test]
    fn restrict_file_no_follow_sets_the_mode_on_a_regular_file() {
        let dir = scratch("restrict-file");
        let file = dir.join("state");
        fs::write(&file, b"x").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();

        restrict_file_no_follow(&file, 0o600).unwrap();
        assert_eq!(mode_of(&file), 0o600);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn restrict_file_no_follow_refuses_a_symlinked_leaf_without_touching_the_target() {
        let dir = scratch("restrict-file-symlink");
        let victim = dir.join("victim");
        fs::write(&victim, b"secret").unwrap();
        fs::set_permissions(&victim, fs::Permissions::from_mode(0o644)).unwrap();
        let link = dir.join("state");
        std::os::unix::fs::symlink(&victim, &link).unwrap();

        restrict_file_no_follow(&link, 0o600)
            .expect_err("a symlinked leaf must be refused, not chmod-ed through");
        assert_eq!(
            mode_of(&victim),
            0o644,
            "a refused chmod must leave the symlink target's own permissions untouched"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn restrict_dir_no_follow_sets_0700_and_refuses_a_symlinked_leaf() {
        let dir = scratch("restrict-dir");
        let real = dir.join("real");
        fs::create_dir(&real).unwrap();
        fs::set_permissions(&real, fs::Permissions::from_mode(0o755)).unwrap();
        restrict_dir_no_follow(&real, 0o700).unwrap();
        assert_eq!(mode_of(&real), 0o700);

        let victim = dir.join("victim");
        fs::create_dir(&victim).unwrap();
        fs::set_permissions(&victim, fs::Permissions::from_mode(0o755)).unwrap();
        let link = dir.join("link");
        std::os::unix::fs::symlink(&victim, &link).unwrap();
        restrict_dir_no_follow(&link, 0o700).expect_err("a symlinked directory must be refused");
        assert_eq!(mode_of(&victim), 0o755);
        let _ = fs::remove_dir_all(&dir);
    }

    // ---------------------------------------------------------------- ensure_private_dir

    #[test]
    fn ensure_private_dir_creates_nested_paths_and_only_restricts_the_last_component() {
        let dir = scratch("private-nested");
        // Pre-existing ancestors keep their own permissions and must never be chmod-ed.
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        let target = dir.join("a").join("b").join("store");

        ensure_private_dir(&target).unwrap();
        assert_eq!(mode_of(&target), 0o700);
        assert_eq!(
            mode_of(&dir.join("a")),
            0o700,
            "intermediate components are created private too"
        );
        assert_eq!(
            mode_of(&dir),
            0o755,
            "a pre-existing ancestor must keep its own permissions"
        );
        // Idempotent: a second call on the existing store must succeed.
        ensure_private_dir(&target).unwrap();
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn ensure_private_dir_refuses_a_symlinked_final_component() {
        let dir = scratch("private-symlink");
        let victim = dir.join("victim");
        fs::create_dir(&victim).unwrap();
        fs::set_permissions(&victim, fs::Permissions::from_mode(0o755)).unwrap();
        let link = dir.join("store");
        std::os::unix::fs::symlink(&victim, &link).unwrap();

        match ensure_private_dir(&link) {
            Err(SecureFsError::InvalidPath(reason)) => {
                assert!(
                    reason.contains("not a real directory"),
                    "unexpected refusal reason: {reason}"
                );
            }
            other => panic!("a symlinked store must be refused, got {other:?}"),
        }
        assert_eq!(
            mode_of(&victim),
            0o755,
            "the symlink target must never be chmod-ed through"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn ensure_private_dir_tolerates_an_os_owned_symlinked_ancestor() {
        // Android's `/data/data` -> `/data/user/0` and macOS `/var` -> `/private/var` are symlinks
        // that no user code may rewrite. The strict dirfd walk cannot traverse them, so the
        // documented fallback must still create a private store *below* them.
        let dir = scratch("private-ancestor");
        let real = dir.join("real");
        fs::create_dir(&real).unwrap();
        let link = dir.join("os-owned-link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let target = link.join("nested").join("store");

        ensure_private_dir(&target).unwrap();
        assert_eq!(mode_of(&real.join("nested").join("store")), 0o700);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn ensure_private_dir_rejects_a_path_without_a_directory_component() {
        match ensure_private_dir(Path::new("/")) {
            Err(SecureFsError::InvalidPath(reason)) => {
                assert!(
                    reason.contains("no directory component"),
                    "unexpected refusal reason: {reason}"
                );
            }
            other => panic!("a component-less path must be refused, got {other:?}"),
        }
    }

    #[test]
    fn ensure_private_dir_io_keeps_the_os_error_kind() {
        let dir = scratch("private-io");
        let victim = dir.join("victim");
        fs::create_dir(&victim).unwrap();
        let link = dir.join("store");
        std::os::unix::fs::symlink(&victim, &link).unwrap();

        let err =
            ensure_private_dir_io(&link).expect_err("the io wrapper must propagate the refusal");
        // The refusal is an InvalidPath, which has no OS error kind, so it is folded into a
        // generic io::Error that still carries the message.
        assert!(
            err.to_string().contains("not a real directory"),
            "got {err}"
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
