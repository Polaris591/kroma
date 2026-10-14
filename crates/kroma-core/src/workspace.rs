use crate::repo_graph::RepoGraphCache;
use crate::sensitive_paths::SensitivePathGuard;
use crate::types::ErrorCode;
use std::collections::HashMap;
#[cfg(unix)]
use std::ffi::CString;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
#[cfg(unix)]
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use thiserror::Error;
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum WorkspaceError {
    #[error("path escape blocked: {reason}")]
    PathEscape { reason: String },
    #[error("symlink blocked: {reason}")]
    SymlinkBlocked { reason: String },
    #[error("sensitive path blocked: {reason}")]
    SensitivePath { reason: String },
    #[error("file not found: {path}")]
    NotFound { path: String },
    #[error("invalid path argument: {reason}")]
    InvalidPath { reason: String },
    #[error("io error: {reason}")]
    Io { reason: String },
}

impl WorkspaceError {
    pub fn error_code(&self) -> ErrorCode {
        match self {
            WorkspaceError::PathEscape { .. } => ErrorCode::PathEscapeBlocked,
            WorkspaceError::SymlinkBlocked { .. } => ErrorCode::SymlinkBlocked,
            WorkspaceError::SensitivePath { .. } => ErrorCode::SensitivePathBlocked,
            WorkspaceError::NotFound { .. } => ErrorCode::FileNotFound,
            WorkspaceError::InvalidPath { .. } => ErrorCode::InvalidArguments,
            WorkspaceError::Io { .. } => ErrorCode::IoError,
        }
    }
}

/// Proof that the operator approved one tool call's access to a specific sensitive path.
///
/// Minted only by the tool runtime (crate-internal constructor) after an interactive `Ask` prompt
/// is answered with an allow, and consumed by a short-lived [`WorkspaceCap`] clone produced by
/// [`WorkspaceCap::with_approval`] that is passed to exactly one tool execution. It is neither
/// `Clone` nor constructible outside this crate, and it names the exact relative path it approved,
/// so a grant can never be replayed, cached, forged, or widened to a different file.
#[derive(Debug)]
pub struct SensitivePathApproval {
    relative_path: PathBuf,
}

impl SensitivePathApproval {
    /// Crate-internal: the tool runtime is the only component allowed to record a user approval.
    pub(crate) fn new(relative_path: impl Into<PathBuf>) -> Self {
        Self {
            relative_path: relative_path.into(),
        }
    }
}

/// Whether `path` is blocked by the sensitive-path guard, honouring an approval that names the
/// exact path (BUG-08). Every other sensitive path stays blocked, so an approval for one file can
/// never open a different one.
fn sensitive_path_is_blocked(approval: Option<&SensitivePathApproval>, path: &Path) -> bool {
    if !SensitivePathGuard::is_sensitive(path) {
        return false;
    }
    match approval {
        Some(approval) => approval.relative_path != path,
        None => true,
    }
}

/// Capability-oriented workspace directory handle enforcing path containment and symlink rules.
#[derive(Debug, Clone)]
pub struct WorkspaceCap {
    root: PathBuf,
    root_dir_fd: Arc<File>,
    /// BUG-06: per-workspace incremental `RepoGraph` parse cache, shared by every clone of this
    /// handle so repeated `code_graph` / `/graph` builds do not re-parse unchanged files. Entries
    /// are content-addressed (verified by SHA-256), so reuse can never return a stale graph.
    repo_graph_cache: Arc<Mutex<RepoGraphCache>>,
    /// BUG-08: set only on the short-lived clone handed to one approved tool call. `None` on the
    /// workspace every other caller sees, so an unapproved call can never bypass the guard.
    sensitive_path_approval: Option<Arc<SensitivePathApproval>>,
}

impl WorkspaceCap {
    pub fn new(workspace_root: impl AsRef<Path>) -> Result<Self, WorkspaceError> {
        #[cfg(unix)]
        {
            let root =
                fs::canonicalize(workspace_root.as_ref()).map_err(|e| WorkspaceError::Io {
                    reason: format!("failed to canonicalize workspace root: {}", e),
                })?;
            let c_root = CString::new(root.as_os_str().as_bytes()).map_err(|_| {
                WorkspaceError::InvalidPath {
                    reason: "workspace root path contains NUL byte".to_string(),
                }
            })?;
            let raw_fd = unsafe {
                libc::open(
                    c_root.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
                )
            };
            if raw_fd < 0 {
                return Err(WorkspaceError::Io {
                    reason: format!(
                        "failed to open workspace root directory: {}",
                        std::io::Error::last_os_error()
                    ),
                });
            }
            unsafe {
                let flags = libc::fcntl(raw_fd, libc::F_GETFD);
                if flags >= 0 && (flags & libc::FD_CLOEXEC) == 0 {
                    let _ = libc::fcntl(raw_fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
                }
            }
            let dir_fd = unsafe { File::from_raw_fd(raw_fd) };
            Ok(Self {
                root,
                root_dir_fd: Arc::new(dir_fd),
                repo_graph_cache: Arc::new(Mutex::new(RepoGraphCache::new())),
                sensitive_path_approval: None,
            })
        }
        #[cfg(windows)]
        {
            let root =
                fs::canonicalize(workspace_root.as_ref()).map_err(|e| WorkspaceError::Io {
                    reason: format!("failed to canonicalize workspace root: {}", e),
                })?;
            let dir_fd = open_windows_handle_no_reparse(&root, &root, Some(true))?;
            Ok(Self {
                root,
                root_dir_fd: Arc::new(dir_fd),
                repo_graph_cache: Arc::new(Mutex::new(RepoGraphCache::new())),
                sensitive_path_approval: None,
            })
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn root_dir_fd(&self) -> &File {
        &self.root_dir_fd
    }

    /// BUG-06: the incremental `RepoGraph` parse cache shared by every clone of this handle.
    pub fn repo_graph_cache(&self) -> &Arc<Mutex<RepoGraphCache>> {
        &self.repo_graph_cache
    }

    /// BUG-08: build a short-lived clone carrying an interactive approval for `approval`'s path.
    /// The runtime passes this clone to exactly one tool execution and drops it afterwards, so the
    /// grant cannot leak into any later call.
    pub fn with_approval(&self, approval: SensitivePathApproval) -> Self {
        let mut clone = self.clone();
        clone.sensitive_path_approval = Some(Arc::new(approval));
        clone
    }

    /// Whether `path` is blocked by the sensitive-path guard, honouring this handle's approval.
    fn sensitive_path_blocked(&self, path: &Path) -> bool {
        sensitive_path_is_blocked(self.sensitive_path_approval.as_deref(), path)
    }

    /// Race-free kernel-enforced file open relative to `root_dir_fd` rejecting all symlink components and `..` escapes.
    /// Uses Linux `openat2(RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS)` when supported, falling back to step-by-step
    /// `openat(..., O_RDONLY | O_NOFOLLOW | O_CLOEXEC)` on `ENOSYS` or non-Linux platforms.
    pub fn open_existing_no_symlink(&self, rel: &str) -> Result<(PathBuf, File), WorkspaceError> {
        let norm_rel = Self::validate_relative_components(rel)?;
        if norm_rel != Path::new(".") && self.sensitive_path_blocked(&norm_rel) {
            return Err(WorkspaceError::SensitivePath {
                reason: format!(
                    "access to sensitive path '{}' is blocked",
                    norm_rel.display()
                ),
            });
        }

        #[cfg(target_os = "linux")]
        {
            #[repr(C)]
            struct OpenHow {
                flags: u64,
                mode: u64,
                resolve: u64,
            }
            const RESOLVE_NO_SYMLINKS: u64 = 0x04;
            const RESOLVE_BENEATH: u64 = 0x08;

            let c_rel = CString::new(norm_rel.as_os_str().as_bytes()).map_err(|_| {
                WorkspaceError::InvalidPath {
                    reason: "path contains NUL byte".to_string(),
                }
            })?;
            let how = OpenHow {
                flags: (libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NONBLOCK) as u64,
                mode: 0,
                resolve: RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS,
            };
            let raw_fd = unsafe {
                libc::syscall(
                    libc::SYS_openat2,
                    self.root_dir_fd.as_raw_fd(),
                    c_rel.as_ptr(),
                    &how as *const OpenHow,
                    std::mem::size_of::<OpenHow>(),
                )
            };
            if raw_fd >= 0 {
                let file = unsafe { File::from_raw_fd(raw_fd as RawFd) };
                let file = Self::validate_and_clear_nonblock(file, &norm_rel)?;
                return Ok((norm_rel, file));
            }
            let err = std::io::Error::last_os_error();
            match err.raw_os_error() {
                Some(libc::ENOSYS) => {
                    // Fall through to step-by-step openat(O_NOFOLLOW) below
                }
                Some(libc::ENOENT) => {
                    return Err(WorkspaceError::NotFound {
                        path: rel.to_string(),
                    });
                }
                Some(libc::ELOOP) => {
                    return Err(WorkspaceError::SymlinkBlocked {
                        reason: format!("symlink component blocked at '{}'", norm_rel.display()),
                    });
                }
                Some(libc::EXDEV) => {
                    return Err(WorkspaceError::PathEscape {
                        reason: format!("path escape blocked at '{}'", norm_rel.display()),
                    });
                }
                _ => {
                    return Err(WorkspaceError::Io {
                        reason: format!("failed to open '{}': {}", norm_rel.display(), err),
                    });
                }
            }
        }

        self.open_existing_with_openat_steps(rel, norm_rel)
    }

    /// Step-by-step `openat(..., O_RDONLY | O_NOFOLLOW | O_CLOEXEC)` traversal relative to `root_dir_fd`
    /// on Unix, or step-by-step `FILE_FLAG_OPEN_REPARSE_POINT` handle-verified traversal on Windows.
    pub fn open_existing_with_openat_steps(
        &self,
        rel: &str,
        norm_rel: PathBuf,
    ) -> Result<(PathBuf, File), WorkspaceError> {
        #[cfg(unix)]
        {
            if norm_rel == Path::new(".") {
                let c_dot = CString::new(".").unwrap();
                let fd = unsafe {
                    libc::openat(
                        self.root_dir_fd.as_raw_fd(),
                        c_dot.as_ptr(),
                        crate::secure_fs::OPEN_FILE_NO_FOLLOW,
                    )
                };
                if fd < 0 {
                    return Err(WorkspaceError::Io {
                        reason: std::io::Error::last_os_error().to_string(),
                    });
                }
                return Ok((norm_rel, unsafe { File::from_raw_fd(fd) }));
            }

            let mut cur_file: Option<File> = None;
            for comp in norm_rel.components() {
                match comp {
                    Component::Normal(part) => {
                        let c_part = CString::new(part.as_bytes()).map_err(|_| {
                            WorkspaceError::InvalidPath {
                                reason: "path component contains NUL byte".to_string(),
                            }
                        })?;
                        let cur_fd = cur_file
                            .as_ref()
                            .map(|f| f.as_raw_fd())
                            .unwrap_or_else(|| self.root_dir_fd.as_raw_fd());
                        let next_fd = unsafe {
                            libc::openat(
                                cur_fd,
                                c_part.as_ptr(),
                                crate::secure_fs::OPEN_FILE_NO_FOLLOW | libc::O_NONBLOCK,
                            )
                        };
                        if next_fd < 0 {
                            let err = std::io::Error::last_os_error();
                            return Err(match err.raw_os_error() {
                                Some(libc::ENOENT) => WorkspaceError::NotFound {
                                    path: rel.to_string(),
                                },
                                Some(libc::ELOOP) => WorkspaceError::SymlinkBlocked {
                                    reason: format!(
                                        "symlink component blocked at '{}'",
                                        norm_rel.display()
                                    ),
                                },
                                _ => WorkspaceError::Io {
                                    reason: format!(
                                        "failed to open '{}': {}",
                                        norm_rel.display(),
                                        err
                                    ),
                                },
                            });
                        }
                        cur_file = Some(unsafe { File::from_raw_fd(next_fd) });
                    }
                    _ => {
                        return Err(WorkspaceError::PathEscape {
                            reason: format!("invalid component in '{}'", rel),
                        });
                    }
                }
            }

            let file = cur_file.ok_or_else(|| WorkspaceError::InvalidPath {
                reason: "empty relative path".to_string(),
            })?;
            let file = Self::validate_and_clear_nonblock(file, &norm_rel)?;
            Ok((norm_rel, file))
        }
        #[cfg(windows)]
        {
            if norm_rel == Path::new(".") {
                let f = open_windows_handle_no_reparse(&self.root, &norm_rel, Some(true))?;
                let _ = verify_windows_handle_within_root(
                    &f,
                    &self.root,
                    &norm_rel,
                    self.sensitive_path_approval.as_deref(),
                )?;
                return Ok((norm_rel, f));
            }
            let comps: Vec<_> = norm_rel.components().collect();
            let mut cur_path = self.root.clone();
            let mut ancestor_handles: Vec<File> = Vec::with_capacity(comps.len());
            ancestor_handles.push(open_windows_handle_no_reparse(
                &self.root,
                &norm_rel,
                Some(true),
            )?);
            let mut leaf_file: Option<File> = None;
            for (idx, comp) in comps.iter().enumerate() {
                let is_leaf = idx + 1 == comps.len();
                match comp {
                    Component::Normal(part) => {
                        cur_path.push(part);
                        let want_dir = if is_leaf { None } else { Some(true) };
                        let handle = open_windows_handle_no_reparse(&cur_path, &norm_rel, want_dir)
                            .map_err(|e| match e {
                                WorkspaceError::NotFound { .. } => WorkspaceError::NotFound {
                                    path: rel.to_string(),
                                },
                                other => other,
                            })?;
                        if is_leaf {
                            leaf_file = Some(handle);
                        } else {
                            ancestor_handles.push(handle);
                        }
                    }
                    _ => {
                        return Err(WorkspaceError::PathEscape {
                            reason: format!("invalid component in '{}'", rel),
                        });
                    }
                }
            }
            let file = leaf_file.ok_or_else(|| WorkspaceError::InvalidPath {
                reason: "empty relative path".to_string(),
            })?;
            let _ = verify_windows_handle_within_root(
                &file,
                &self.root,
                &norm_rel,
                self.sensitive_path_approval.as_deref(),
            )?;
            drop(ancestor_handles);
            Ok((norm_rel, file))
        }
    }

    /// Open a workspace-relative directory as a race-free, symlink-free `File` handle (F-02).
    ///
    /// Uses the same `openat2(RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS)` guarantee as
    /// [`Self::open_existing_no_symlink`], with `O_DIRECTORY`, so navigation tools can list a
    /// directory from an already-validated handle instead of re-checking a path and racing an
    /// attacker that swaps the directory for a symlink between check and use.
    pub fn open_dir_no_symlink(&self, rel: &str) -> Result<(PathBuf, File), WorkspaceError> {
        let norm_rel = Self::validate_relative_components(rel)?;
        if norm_rel != Path::new(".") && self.sensitive_path_blocked(&norm_rel) {
            return Err(WorkspaceError::SensitivePath {
                reason: format!(
                    "access to sensitive path '{}' is blocked",
                    norm_rel.display()
                ),
            });
        }

        #[cfg(target_os = "linux")]
        {
            #[repr(C)]
            struct OpenHow {
                flags: u64,
                mode: u64,
                resolve: u64,
            }
            const RESOLVE_NO_SYMLINKS: u64 = 0x04;
            const RESOLVE_BENEATH: u64 = 0x08;

            let c_rel = CString::new(norm_rel.as_os_str().as_bytes()).map_err(|_| {
                WorkspaceError::InvalidPath {
                    reason: "path contains NUL byte".to_string(),
                }
            })?;
            let how = OpenHow {
                flags: (crate::secure_fs::OPEN_DIR_NO_FOLLOW) as u64,
                mode: 0,
                resolve: RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS,
            };
            let raw_fd = unsafe {
                libc::syscall(
                    libc::SYS_openat2,
                    self.root_dir_fd.as_raw_fd(),
                    c_rel.as_ptr(),
                    &how as *const OpenHow,
                    std::mem::size_of::<OpenHow>(),
                )
            };
            if raw_fd >= 0 {
                return Ok((norm_rel, unsafe { File::from_raw_fd(raw_fd as RawFd) }));
            }
            let err = std::io::Error::last_os_error();
            match err.raw_os_error() {
                Some(libc::ENOSYS) => {}
                Some(libc::ENOENT) => {
                    return Err(WorkspaceError::NotFound {
                        path: rel.to_string(),
                    });
                }
                Some(libc::ELOOP) => {
                    return Err(WorkspaceError::SymlinkBlocked {
                        reason: format!("symlink component blocked at '{}'", norm_rel.display()),
                    });
                }
                Some(libc::EXDEV) => {
                    return Err(WorkspaceError::PathEscape {
                        reason: format!("path escape blocked at '{}'", norm_rel.display()),
                    });
                }
                Some(libc::ENOTDIR) => {
                    // The kernel reports ENOTDIR for a symlinked directory under
                    // `O_DIRECTORY | RESOLVE_NO_SYMLINKS`; classify it precisely (F-02).
                    if let Some(err) = self.classify_non_dir(&norm_rel) {
                        return Err(err);
                    }
                    return Err(WorkspaceError::InvalidPath {
                        reason: format!("'{}' is not a directory", norm_rel.display()),
                    });
                }
                _ => {
                    return Err(WorkspaceError::Io {
                        reason: format!(
                            "failed to open directory '{}': {}",
                            norm_rel.display(),
                            err
                        ),
                    });
                }
            }
        }

        #[cfg(unix)]
        {
            // Portable fallback: step-by-step openat(O_DIRECTORY | O_NOFOLLOW).
            let mut cur_file: Option<File> = None;
            for comp in norm_rel.components() {
                if let Component::Normal(part) = comp {
                    let c_part =
                        CString::new(part.as_bytes()).map_err(|_| WorkspaceError::InvalidPath {
                            reason: "path component contains NUL byte".to_string(),
                        })?;
                    let cur_fd = cur_file
                        .as_ref()
                        .map(|f| f.as_raw_fd())
                        .unwrap_or_else(|| self.root_dir_fd.as_raw_fd());
                    let next_fd = unsafe {
                        libc::openat(
                            cur_fd,
                            c_part.as_ptr(),
                            crate::secure_fs::OPEN_DIR_NO_FOLLOW,
                        )
                    };
                    if next_fd < 0 {
                        let err = std::io::Error::last_os_error();
                        return Err(match err.raw_os_error() {
                            Some(libc::ENOENT) => WorkspaceError::NotFound {
                                path: rel.to_string(),
                            },
                            Some(libc::ELOOP) => WorkspaceError::SymlinkBlocked {
                                reason: format!(
                                    "symlink component blocked at '{}'",
                                    norm_rel.display()
                                ),
                            },
                            Some(libc::ENOTDIR) => match self.classify_non_dir(&norm_rel) {
                                Some(err) => err,
                                None => WorkspaceError::InvalidPath {
                                    reason: format!("'{}' is not a directory", norm_rel.display()),
                                },
                            },
                            _ => WorkspaceError::Io {
                                reason: format!(
                                    "failed to open directory '{}': {}",
                                    norm_rel.display(),
                                    err
                                ),
                            },
                        });
                    }
                    cur_file = Some(unsafe { File::from_raw_fd(next_fd) });
                } else if norm_rel != Path::new(".") {
                    return Err(WorkspaceError::PathEscape {
                        reason: format!("invalid component in '{}'", rel),
                    });
                }
            }

            // `.` yields no Normal components; duplicate the workspace root directory handle.
            let file = match cur_file {
                Some(f) => f,
                None => {
                    let c_dot = CString::new(".").unwrap();
                    let fd = unsafe {
                        libc::openat(
                            self.root_dir_fd.as_raw_fd(),
                            c_dot.as_ptr(),
                            crate::secure_fs::OPEN_DIR_NO_FOLLOW,
                        )
                    };
                    if fd < 0 {
                        return Err(WorkspaceError::Io {
                            reason: std::io::Error::last_os_error().to_string(),
                        });
                    }
                    unsafe { File::from_raw_fd(fd) }
                }
            };
            Ok((norm_rel, file))
        }
        #[cfg(windows)]
        {
            let mut cur_path = self.root.clone();
            let mut ancestor_handles: Vec<File> = Vec::new();
            ancestor_handles.push(open_windows_handle_no_reparse(
                &self.root,
                &norm_rel,
                Some(true),
            )?);
            for comp in norm_rel.components() {
                if let Component::Normal(part) = comp {
                    cur_path.push(part);
                    let f = open_windows_handle_no_reparse(&cur_path, &norm_rel, Some(true))
                        .map_err(|e| match e {
                            WorkspaceError::NotFound { .. } => WorkspaceError::NotFound {
                                path: rel.to_string(),
                            },
                            other => other,
                        })?;
                    ancestor_handles.push(f);
                } else if norm_rel != Path::new(".") {
                    return Err(WorkspaceError::PathEscape {
                        reason: format!("invalid component in '{}'", rel),
                    });
                }
            }
            let file = match ancestor_handles.pop() {
                Some(f) => f,
                None => open_windows_handle_no_reparse(&self.root, &norm_rel, Some(true))?,
            };
            let _ = verify_windows_handle_within_root(
                &file,
                &self.root,
                &norm_rel,
                self.sensitive_path_approval.as_deref(),
            )?;
            drop(ancestor_handles);
            Ok((norm_rel, file))
        }
    }

    /// Enumerate the immediate children of a workspace-relative directory through a race-free
    /// directory FD (F-02). Entries are `fstatat(AT_SYMLINK_NOFOLLOW)`-classified so callers can
    /// skip symlinks and never follow an entry that was swapped in after a path check.
    pub fn read_dir_no_symlink(
        &self,
        rel: &str,
    ) -> Result<(PathBuf, Vec<DirEntryInfo>), WorkspaceError> {
        #[cfg(unix)]
        {
            let (norm_rel, dir_file) = self.open_dir_no_symlink(rel)?;
            let dup_fd = unsafe { libc::fcntl(dir_file.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
            if dup_fd < 0 {
                return Err(WorkspaceError::Io {
                    reason: format!(
                        "failed to duplicate directory fd: {}",
                        std::io::Error::last_os_error()
                    ),
                });
            }
            let dirp = unsafe { libc::fdopendir(dup_fd) };
            if dirp.is_null() {
                let err = std::io::Error::last_os_error();
                unsafe { libc::close(dup_fd) };
                return Err(WorkspaceError::Io {
                    reason: format!("fdopendir failed: {}", err),
                });
            }

            let mut entries = Vec::new();
            loop {
                let ent = unsafe { libc::readdir(dirp) };
                if ent.is_null() {
                    break;
                }
                let name_bytes =
                    unsafe { std::ffi::CStr::from_ptr((*ent).d_name.as_ptr()) }.to_bytes();
                if name_bytes == b"." || name_bytes == b".." {
                    continue;
                }
                let Ok(name) = std::str::from_utf8(name_bytes) else {
                    continue;
                };
                let Ok(c_name) = CString::new(name_bytes) else {
                    continue;
                };
                let mut st: libc::stat = unsafe { std::mem::zeroed() };
                let rc = unsafe {
                    libc::fstatat(
                        dir_file.as_raw_fd(),
                        c_name.as_ptr(),
                        &mut st,
                        libc::AT_SYMLINK_NOFOLLOW,
                    )
                };
                if rc != 0 {
                    continue;
                }
                let fmt = st.st_mode & libc::S_IFMT;
                entries.push(DirEntryInfo {
                    name: name.to_string(),
                    is_dir: fmt == libc::S_IFDIR,
                    is_file: fmt == libc::S_IFREG,
                    is_symlink: fmt == libc::S_IFLNK,
                });
            }
            unsafe { libc::closedir(dirp) };
            Ok((norm_rel, entries))
        }
        #[cfg(windows)]
        {
            let (norm_rel, dir_file) = self.open_dir_no_symlink(rel)?;
            let dir_path = verify_windows_handle_within_root(
                &dir_file,
                &self.root,
                &norm_rel,
                self.sensitive_path_approval.as_deref(),
            )?;
            let rd = fs::read_dir(&dir_path).map_err(|e| WorkspaceError::Io {
                reason: format!("read_dir failed for '{}': {}", norm_rel.display(), e),
            })?;
            let mut entries = Vec::new();
            for item in rd {
                let Ok(entry) = item else { continue };
                let name = entry.file_name().to_string_lossy().to_string();
                if name == "." || name == ".." {
                    continue;
                }
                let Ok(st) = fs::symlink_metadata(entry.path()) else {
                    continue;
                };
                let is_symlink = is_windows_reparse_point(&st);
                entries.push(DirEntryInfo {
                    name,
                    is_dir: !is_symlink && st.is_dir(),
                    is_file: !is_symlink && st.is_file(),
                    is_symlink,
                });
            }
            drop(dir_file);
            Ok((norm_rel, entries))
        }
    }

    /// Distinguish a symlinked path from a plain non-directory (F-02). `openat2` with
    /// `O_DIRECTORY | RESOLVE_NO_SYMLINKS` reports `ENOTDIR` for both, so re-stat the leaf with
    /// `fstatat(AT_SYMLINK_NOFOLLOW)` relative to its (symlink-free) parent directory.
    #[cfg(unix)]
    fn classify_non_dir(&self, norm_rel: &Path) -> Option<WorkspaceError> {
        let (parent_dir, leaf) = self.open_parent_dir_no_symlink(norm_rel, false).ok()?;
        let c_leaf = CString::new(leaf.as_bytes()).ok()?;
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        let rc = unsafe {
            libc::fstatat(
                parent_dir.as_raw_fd(),
                c_leaf.as_ptr(),
                &mut st,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if rc != 0 {
            return None;
        }
        if st.st_mode & libc::S_IFMT == libc::S_IFLNK {
            return Some(WorkspaceError::SymlinkBlocked {
                reason: format!("symlink directory blocked at '{}'", norm_rel.display()),
            });
        }
        None
    }

    #[cfg(unix)]
    fn validate_and_clear_nonblock(file: File, norm_rel: &Path) -> Result<File, WorkspaceError> {
        let meta = file.metadata().map_err(|e| WorkspaceError::Io {
            reason: format!("failed to stat '{}': {}", norm_rel.display(), e),
        })?;
        let ft = meta.file_type();
        if !ft.is_file() && !ft.is_dir() {
            return Err(WorkspaceError::InvalidPath {
                reason: format!(
                    "'{}' is a special file (FIFO/socket/device), not a regular file",
                    norm_rel.display()
                ),
            });
        }
        unsafe {
            let fd = file.as_raw_fd();
            let flags = libc::fcntl(fd, libc::F_GETFL);
            if flags >= 0 && (flags & libc::O_NONBLOCK) != 0 {
                let _ = libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK);
            }
        }
        Ok(file)
    }

    /// Open (and optionally create) the parent directory of `norm_rel` relative to `root_dir_fd`
    /// using `openat(..., O_RDONLY | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC)` without following symlinks.
    pub fn open_parent_dir_no_symlink(
        &self,
        norm_rel: &Path,
        create_missing: bool,
    ) -> Result<(File, OsString), WorkspaceError> {
        let file_name = norm_rel
            .file_name()
            .ok_or_else(|| WorkspaceError::InvalidPath {
                reason: format!("path '{}' has no file name", norm_rel.display()),
            })?
            .to_os_string();

        #[cfg(unix)]
        {
            let c_dot = CString::new(".").unwrap();
            let root_dup = unsafe {
                libc::openat(
                    self.root_dir_fd.as_raw_fd(),
                    c_dot.as_ptr(),
                    crate::secure_fs::OPEN_DIR_NO_FOLLOW,
                )
            };
            if root_dup < 0 {
                return Err(WorkspaceError::Io {
                    reason: format!(
                        "failed to dup workspace root fd: {}",
                        std::io::Error::last_os_error()
                    ),
                });
            }
            let mut cur_dir = unsafe { File::from_raw_fd(root_dup) };

            if let Some(parent) = norm_rel.parent() {
                for comp in parent.components() {
                    if let Component::Normal(part) = comp {
                        if part.is_empty() {
                            continue;
                        }
                        let c_part = CString::new(part.as_bytes()).map_err(|_| {
                            WorkspaceError::InvalidPath {
                                reason: "directory component contains NUL byte".to_string(),
                            }
                        })?;
                        let mut next_fd = unsafe {
                            libc::openat(
                                cur_dir.as_raw_fd(),
                                c_part.as_ptr(),
                                crate::secure_fs::OPEN_DIR_NO_FOLLOW,
                            )
                        };
                        if next_fd < 0 {
                            let err = std::io::Error::last_os_error();
                            if create_missing && err.raw_os_error() == Some(libc::ENOENT) {
                                let mk_res = unsafe {
                                    libc::mkdirat(cur_dir.as_raw_fd(), c_part.as_ptr(), 0o755)
                                };
                                if mk_res < 0 {
                                    let mk_err = std::io::Error::last_os_error();
                                    if mk_err.raw_os_error() != Some(libc::EEXIST) {
                                        return Err(WorkspaceError::Io {
                                            reason: format!(
                                                "mkdirat failed for '{}': {}",
                                                norm_rel.display(),
                                                mk_err
                                            ),
                                        });
                                    }
                                }
                                next_fd = unsafe {
                                    libc::openat(
                                        cur_dir.as_raw_fd(),
                                        c_part.as_ptr(),
                                        crate::secure_fs::OPEN_DIR_NO_FOLLOW,
                                    )
                                };
                            }
                            if next_fd < 0 {
                                let err2 = std::io::Error::last_os_error();
                                return Err(match err2.raw_os_error() {
                                    Some(libc::ENOENT) => WorkspaceError::NotFound {
                                        path: norm_rel.display().to_string(),
                                    },
                                    Some(libc::ELOOP | libc::ENOTDIR) => {
                                        WorkspaceError::SymlinkBlocked {
                                            reason: format!(
                                                "non-directory or symlink parent blocked in '{}'",
                                                norm_rel.display()
                                            ),
                                        }
                                    }
                                    _ => WorkspaceError::Io {
                                        reason: format!(
                                            "openat parent failed for '{}': {}",
                                            norm_rel.display(),
                                            err2
                                        ),
                                    },
                                });
                            }
                        }
                        cur_dir = unsafe { File::from_raw_fd(next_fd) };
                    }
                }
            }

            Ok((cur_dir, file_name))
        }
        #[cfg(windows)]
        {
            let mut cur_path = self.root.clone();
            let mut ancestor_handles: Vec<File> = Vec::new();
            ancestor_handles.push(open_windows_handle_no_reparse(
                &cur_path,
                norm_rel,
                Some(true),
            )?);
            if let Some(parent) = norm_rel.parent() {
                for comp in parent.components() {
                    if let Component::Normal(part) = comp {
                        if part.is_empty() {
                            continue;
                        }
                        cur_path.push(part);
                        let next_dir = match open_windows_handle_no_reparse(
                            &cur_path,
                            norm_rel,
                            Some(true),
                        ) {
                            Ok(next_dir) => next_dir,
                            Err(WorkspaceError::NotFound { .. }) if create_missing => {
                                match fs::create_dir(&cur_path) {
                                    Ok(()) => {}
                                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                                    Err(e) => {
                                        return Err(WorkspaceError::Io {
                                            reason: format!(
                                                "create_dir failed for '{}': {}",
                                                norm_rel.display(),
                                                e
                                            ),
                                        });
                                    }
                                }
                                open_windows_handle_no_reparse(&cur_path, norm_rel, Some(true))
                                    .map_err(|e| match e {
                                        WorkspaceError::InvalidPath { .. } => {
                                            WorkspaceError::SymlinkBlocked {
                                                reason: format!(
                                                    "non-directory or symlink parent blocked in '{}'",
                                                    norm_rel.display()
                                                ),
                                            }
                                        }
                                        other => other,
                                    })?
                            }
                            Err(WorkspaceError::InvalidPath { .. }) => {
                                return Err(WorkspaceError::SymlinkBlocked {
                                    reason: format!(
                                        "non-directory or symlink parent blocked in '{}'",
                                        norm_rel.display()
                                    ),
                                });
                            }
                            Err(e) => return Err(e),
                        };
                        ancestor_handles.push(next_dir);
                    }
                }
            }
            let cur_dir = ancestor_handles
                .pop()
                .ok_or_else(|| WorkspaceError::InvalidPath {
                    reason: "missing parent handle".to_string(),
                })?;
            let _ = verify_windows_handle_within_root(
                &cur_dir,
                &self.root,
                norm_rel,
                self.sensitive_path_approval.as_deref(),
            )?;
            drop(ancestor_handles);
            Ok((cur_dir, file_name))
        }
    }

    /// Normalize and validate a workspace-relative path without touching the filesystem yet.
    /// Rejects absolute paths, `..` parent traversal, Windows drive/UNC prefixes, reserved DOS
    /// device names (`CON`, `NUL`, `COM1`..`COM9`, `LPT1`..`LPT9`), and NTFS Alternate Data Streams (`:`).
    pub fn validate_relative_components(rel: &str) -> Result<PathBuf, WorkspaceError> {
        let trimmed = rel.trim();
        if trimmed.is_empty() {
            return Err(WorkspaceError::InvalidPath {
                reason: "path must not be empty".to_string(),
            });
        }
        if trimmed.contains('\0') {
            return Err(WorkspaceError::InvalidPath {
                reason: "path contains NUL byte".to_string(),
            });
        }
        let bytes = trimmed.as_bytes();
        let has_drive_prefix =
            bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':';
        let p = Path::new(trimmed);
        if p.is_absolute()
            || trimmed.starts_with('/')
            || trimmed.starts_with('\\')
            || has_drive_prefix
        {
            return Err(WorkspaceError::PathEscape {
                reason: format!("absolute path '{}' is not allowed", rel),
            });
        }

        let slash_norm = trimmed.replace('\\', "/");
        let mut parts: Vec<&str> = Vec::new();
        for seg in slash_norm.split('/') {
            if seg.is_empty() || seg == "." {
                continue;
            }
            if seg == ".." {
                return Err(WorkspaceError::PathEscape {
                    reason: format!("parent traversal '..' in '{}' is not allowed", rel),
                });
            }
            if seg.contains(':') {
                return Err(WorkspaceError::InvalidPath {
                    reason: format!(
                        "NTFS Alternate Data Stream or colon in component '{}' of '{}' is not allowed",
                        seg, rel
                    ),
                });
            }
            if is_windows_reserved_device_name(seg) {
                return Err(WorkspaceError::InvalidPath {
                    reason: format!(
                        "reserved Windows device name '{}' in '{}' is not allowed",
                        seg, rel
                    ),
                });
            }
            parts.push(seg);
        }

        if parts.is_empty() {
            Ok(PathBuf::from("."))
        } else {
            Ok(PathBuf::from(parts.join("/")))
        }
    }

    /// Resolve a relative path for reading, verifying no component is a symlink escaping the workspace.
    pub fn resolve_existing_path(
        &self,
        rel: &str,
        forbid_leaf_symlink: bool,
    ) -> Result<(PathBuf, PathBuf), WorkspaceError> {
        let norm_rel = Self::validate_relative_components(rel)?;
        if norm_rel == Path::new(".") {
            return Ok((norm_rel, self.root.clone()));
        }
        if self.sensitive_path_blocked(&norm_rel) {
            return Err(WorkspaceError::SensitivePath {
                reason: format!(
                    "access to sensitive path '{}' is blocked",
                    norm_rel.display()
                ),
            });
        }

        let mut cur = self.root.clone();
        let components: Vec<_> = norm_rel.components().collect();
        for (idx, comp) in components.iter().enumerate() {
            let is_leaf = idx + 1 == components.len();
            if let Component::Normal(part) = comp {
                cur.push(part);
                let sym_meta = fs::symlink_metadata(&cur).map_err(|e| {
                    if e.kind() == std::io::ErrorKind::NotFound {
                        WorkspaceError::NotFound {
                            path: rel.to_string(),
                        }
                    } else {
                        WorkspaceError::Io {
                            reason: e.to_string(),
                        }
                    }
                })?;

                if is_symlink_or_reparse(&sym_meta) {
                    if is_leaf && forbid_leaf_symlink {
                        return Err(WorkspaceError::SymlinkBlocked {
                            reason: format!(
                                "refusing to modify symlink target at '{}'",
                                norm_rel.display()
                            ),
                        });
                    }
                    let canonical_target =
                        fs::canonicalize(&cur).map_err(|_| WorkspaceError::PathEscape {
                            reason: format!(
                                "dangling or external symlink '{}' blocked",
                                norm_rel.display()
                            ),
                        })?;
                    let Some(rel_target) =
                        path_starts_with_and_strip(&canonical_target, &self.root)
                    else {
                        return Err(WorkspaceError::PathEscape {
                            reason: format!(
                                "symlink '{}' points outside workspace ({})",
                                norm_rel.display(),
                                canonical_target.display()
                            ),
                        });
                    };
                    if self.sensitive_path_blocked(&rel_target) {
                        return Err(WorkspaceError::SensitivePath {
                            reason: format!(
                                "symlink '{}' points to sensitive path '{}'",
                                norm_rel.display(),
                                rel_target.display()
                            ),
                        });
                    }
                }
            }
        }

        let canonical = fs::canonicalize(&cur).map_err(|e| WorkspaceError::Io {
            reason: e.to_string(),
        })?;
        let Some(rel_canonical) = path_starts_with_and_strip(&canonical, &self.root) else {
            return Err(WorkspaceError::PathEscape {
                reason: format!("resolved path '{}' escapes workspace", norm_rel.display()),
            });
        };
        if self.sensitive_path_blocked(&rel_canonical) {
            return Err(WorkspaceError::SensitivePath {
                reason: format!(
                    "resolved path '{}' is a sensitive path",
                    rel_canonical.display()
                ),
            });
        }

        Ok((norm_rel, cur))
    }

    /// Resolve a relative path for `write` (where parent dirs may be created or exist, and leaf may or may not exist).
    /// Rejects overwriting symlinks, directories, FIFOs, or devices.
    pub fn resolve_write_target(&self, rel: &str) -> Result<(PathBuf, PathBuf), WorkspaceError> {
        let norm_rel = Self::validate_relative_components(rel)?;
        if norm_rel == Path::new(".") {
            return Err(WorkspaceError::InvalidPath {
                reason: "cannot write to workspace root directory '.'".to_string(),
            });
        }
        if self.sensitive_path_blocked(&norm_rel) {
            return Err(WorkspaceError::SensitivePath {
                reason: format!(
                    "writing to sensitive path '{}' is blocked",
                    norm_rel.display()
                ),
            });
        }

        let mut cur = self.root.clone();
        let components: Vec<_> = norm_rel.components().collect();
        for (idx, comp) in components.iter().enumerate() {
            let is_leaf = idx + 1 == components.len();
            if let Component::Normal(part) = comp {
                cur.push(part);
                match fs::symlink_metadata(&cur) {
                    Ok(sym_meta) => {
                        if is_symlink_or_reparse(&sym_meta) {
                            if is_leaf {
                                return Err(WorkspaceError::SymlinkBlocked {
                                    reason: format!(
                                        "refusing to overwrite symlink '{}'",
                                        norm_rel.display()
                                    ),
                                });
                            }
                            let canonical_target =
                                fs::canonicalize(&cur).map_err(|_| WorkspaceError::PathEscape {
                                    reason: format!(
                                        "dangling or external directory symlink '{}' blocked",
                                        norm_rel.display()
                                    ),
                                })?;
                            let Some(rel_target) =
                                path_starts_with_and_strip(&canonical_target, &self.root)
                            else {
                                return Err(WorkspaceError::PathEscape {
                                    reason: format!(
                                        "directory symlink '{}' points outside workspace",
                                        norm_rel.display()
                                    ),
                                });
                            };
                            if self.sensitive_path_blocked(&rel_target) {
                                return Err(WorkspaceError::SensitivePath {
                                    reason: format!(
                                        "directory symlink '{}' points to sensitive path '{}'",
                                        norm_rel.display(),
                                        rel_target.display()
                                    ),
                                });
                            }
                        } else if is_leaf && !sym_meta.file_type().is_file() {
                            return Err(WorkspaceError::InvalidPath {
                                reason: format!(
                                    "target '{}' exists and is not a regular file",
                                    norm_rel.display()
                                ),
                            });
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        // Will be created if needed
                    }
                    Err(e) => {
                        return Err(WorkspaceError::Io {
                            reason: e.to_string(),
                        });
                    }
                }
            }
        }

        Ok((norm_rel, cur))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct InodeLockKey {
    pub parent_dev: u64,
    pub parent_ino: u64,
    pub file_name: OsString,
}

/// Classification of a single directory entry obtained through [`WorkspaceCap::read_dir_no_symlink`].
/// The kind is derived from `fstatat(AT_SYMLINK_NOFOLLOW)` on the entry name relative to the
/// directory FD, so it cannot be spoofed by a symlink swapped in after the directory was opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntryInfo {
    pub name: String,
    pub is_dir: bool,
    pub is_file: bool,
    pub is_symlink: bool,
}

pub struct InodeLockGuard {
    key: InodeLockKey,
    guard: Option<OwnedMutexGuard<()>>,
    locks: Arc<Mutex<HashMap<InodeLockKey, Arc<AsyncMutex<()>>>>>,
}

impl Drop for InodeLockGuard {
    fn drop(&mut self) {
        drop(self.guard.take());
        if let Ok(mut map) = self.locks.lock() {
            if let Some(arc_mutex) = map.get(&self.key) {
                if Arc::strong_count(arc_mutex) == 1 {
                    map.remove(&self.key);
                }
            }
        }
    }
}

/// Asynchronous file mutation lock keyed by canonical `(parent_dev, parent_ino, file_name)`
/// so new file creation and atomic `rename` replacement never cause lock key drift (§5.2 V6-05).
#[derive(Debug, Clone, Default)]
pub struct InodeMutationLock {
    locks: Arc<Mutex<HashMap<InodeLockKey, Arc<AsyncMutex<()>>>>>,
}

impl InodeMutationLock {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn active_lock_count(&self) -> usize {
        self.locks.lock().unwrap().len()
    }

    pub fn compute_key(target_abs_path: &Path) -> std::io::Result<InodeLockKey> {
        let parent = target_abs_path.parent().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "missing parent dir")
        })?;
        let file_name = target_abs_path.file_name().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "missing file name")
        })?;
        #[cfg(unix)]
        {
            // F-07: computing a lock key must never create directories or canonicalize as a side
            // effect. `metadata` resolves `..` and symlinks in the parent chain at the OS level, which
            // yields the same `(dev, ino)` as the canonical parent for an existing directory.
            let p_meta = fs::metadata(parent)?;
            Ok(InodeLockKey {
                parent_dev: p_meta.dev(),
                parent_ino: p_meta.ino(),
                file_name: file_name.to_os_string(),
            })
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            use windows_sys::Win32::Storage::FileSystem::{
                FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
            };
            let parent_file = fs::OpenOptions::new()
                .read(true)
                .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
                .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
                .open(parent)?;
            Self::key_for_file(&parent_file, Some(file_name))
        }
    }

    /// Derive the lock key from an already-open file/directory handle.
    pub fn key_for_file(file: &File, leaf_name: Option<&OsStr>) -> std::io::Result<InodeLockKey> {
        #[cfg(unix)]
        {
            Self::key_for_fd(file.as_raw_fd(), leaf_name)
        }
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            use windows_sys::Win32::Foundation::HANDLE;
            use windows_sys::Win32::Storage::FileSystem::{
                GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
            };
            let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
            let ok =
                unsafe { GetFileInformationByHandle(file.as_raw_handle() as HANDLE, &mut info) };
            if ok == 0 {
                return Err(std::io::Error::last_os_error());
            }
            let parent_dev = info.dwVolumeSerialNumber as u64;
            let parent_ino = ((info.nFileIndexHigh as u64) << 32) | (info.nFileIndexLow as u64);
            let norm_leaf = leaf_name
                .map(|s| OsString::from(s.to_string_lossy().to_ascii_lowercase()))
                .unwrap_or_default();
            Ok(InodeLockKey {
                parent_dev,
                parent_ino,
                file_name: norm_leaf,
            })
        }
    }

    /// F-07: derive the lock key from an already-open file/directory FD via `fstat`, never by
    /// re-deriving it from an absolute path. `leaf_name` is the child name when `fd` is a parent
    /// directory, or `None` when `fd` is the file itself.
    #[cfg(unix)]
    pub fn key_for_fd(fd: RawFd, leaf_name: Option<&OsStr>) -> std::io::Result<InodeLockKey> {
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        let rc = unsafe { libc::fstat(fd, &mut st) };
        if rc != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(InodeLockKey {
            parent_dev: st.st_dev,
            parent_ino: st.st_ino,
            file_name: leaf_name.map(|s| s.to_os_string()).unwrap_or_default(),
        })
    }

    pub async fn acquire(
        &self,
        target_abs_path: &Path,
    ) -> std::io::Result<(InodeLockKey, InodeLockGuard)> {
        let key = Self::compute_key(target_abs_path)?;
        self.acquire_key(key).await
    }

    /// Acquire the mutation lock keyed by the identity of an already-open directory/file handle.
    pub async fn acquire_for_file(
        &self,
        file: &File,
        leaf_name: Option<&OsStr>,
    ) -> std::io::Result<(InodeLockKey, InodeLockGuard)> {
        let key = Self::key_for_file(file, leaf_name)?;
        self.acquire_key(key).await
    }

    /// F-07: acquire the mutation lock keyed by the identity of an already-open FD, so a path that
    /// is swapped after the FD was opened cannot redirect the lock to a different directory object.
    #[cfg(unix)]
    pub async fn acquire_for_fd(
        &self,
        fd: RawFd,
        leaf_name: Option<&OsStr>,
    ) -> std::io::Result<(InodeLockKey, InodeLockGuard)> {
        let key = Self::key_for_fd(fd, leaf_name)?;
        self.acquire_key(key).await
    }

    async fn acquire_key(
        &self,
        key: InodeLockKey,
    ) -> std::io::Result<(InodeLockKey, InodeLockGuard)> {
        let arc_mutex = {
            let mut map = self.locks.lock().unwrap();
            map.retain(|_, v| Arc::strong_count(v) > 1);
            map.entry(key.clone())
                .or_insert_with(|| Arc::new(AsyncMutex::new(())))
                .clone()
        };
        let guard = arc_mutex.lock_owned().await;
        Ok((
            key.clone(),
            InodeLockGuard {
                key,
                guard: Some(guard),
                locks: Arc::clone(&self.locks),
            },
        ))
    }
}

/// Check whether a path component is a reserved Windows DOS device name (`CON`, `PRN`, `AUX`,
/// `NUL`, `CONIN$`, `CONOUT$`, `COM1`..`COM9`, `LPT1`..`LPT9`, case-insensitive, with or without
/// extension or trailing spaces/dots).
pub fn is_windows_reserved_device_name(component: &str) -> bool {
    let trimmed = component.trim_end_matches([' ', '.']);
    let stem = trimmed
        .split('.')
        .next()
        .unwrap_or("")
        .trim_end_matches(' ');
    if stem.is_empty() {
        return false;
    }
    let upper = stem.to_ascii_uppercase();
    matches!(
        upper.as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "CONIN$"
            | "CONOUT$"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    )
}

/// Strip `\\?\` or `\\?\UNC\` extended-length path prefixes so canonicalized and non-canonicalized
/// Windows paths can be compared deterministically.
pub fn strip_windows_verbatim_prefix(path: &Path) -> PathBuf {
    let s = path.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        PathBuf::from(format!(r"\\{}", rest))
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        PathBuf::from(rest)
    } else {
        path.to_path_buf()
    }
}

/// Compare whether `target` resides inside `base` and return the stripped relative path.
/// On Windows, strips `\\?\` prefixes and compares path components case-insensitively.
pub fn path_starts_with_and_strip(target: &Path, base: &Path) -> Option<PathBuf> {
    #[cfg(unix)]
    {
        target.strip_prefix(base).ok().map(Path::to_path_buf)
    }
    #[cfg(windows)]
    {
        let norm_target = strip_windows_verbatim_prefix(target);
        let norm_base = strip_windows_verbatim_prefix(base);
        let mut t_comps = norm_target.components();
        for b_comp in norm_base.components() {
            let t_comp = t_comps.next()?;
            let b_str = b_comp.as_os_str().to_string_lossy();
            let t_str = t_comp.as_os_str().to_string_lossy();
            if !b_str.eq_ignore_ascii_case(&t_str) {
                return None;
            }
        }
        let mut rel = PathBuf::new();
        for rem in t_comps {
            rel.push(rem.as_os_str());
        }
        Some(rel)
    }
}

#[cfg(unix)]
pub(crate) fn is_symlink_or_reparse(meta: &fs::Metadata) -> bool {
    meta.file_type().is_symlink()
}

#[cfg(windows)]
pub(crate) fn is_symlink_or_reparse(meta: &fs::Metadata) -> bool {
    is_windows_reparse_point(meta)
}

#[cfg(windows)]
pub(crate) fn is_windows_reparse_point(meta: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
    meta.file_type().is_symlink() || (meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT) != 0
}

#[cfg(windows)]
pub(crate) fn windows_final_path_by_handle(file: &File) -> std::io::Result<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::Storage::FileSystem::GetFinalPathNameByHandleW;

    let raw = file.as_raw_handle() as HANDLE;
    let mut cap = 260usize;
    loop {
        let mut wbuf = vec![0u16; cap];
        let written =
            unsafe { GetFinalPathNameByHandleW(raw, wbuf.as_mut_ptr(), wbuf.len() as u32, 0) };
        if written == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let written_usize = written as usize;
        if written_usize < wbuf.len() {
            return Ok(PathBuf::from(std::ffi::OsString::from_wide(
                &wbuf[..written_usize],
            )));
        }
        cap = written_usize.saturating_add(1);
    }
}

#[cfg(windows)]
pub(crate) fn verify_windows_handle_within_root(
    file: &File,
    root: &Path,
    display_rel: &Path,
    approval: Option<&SensitivePathApproval>,
) -> Result<PathBuf, WorkspaceError> {
    let final_path = windows_final_path_by_handle(file).map_err(|e| WorkspaceError::Io {
        reason: format!(
            "GetFinalPathNameByHandleW failed for '{}': {}",
            display_rel.display(),
            e
        ),
    })?;
    let Some(rel) = path_starts_with_and_strip(&final_path, root) else {
        return Err(WorkspaceError::PathEscape {
            reason: format!(
                "opened handle for '{}' resolved outside workspace ({})",
                display_rel.display(),
                final_path.display()
            ),
        });
    };
    if !rel.as_os_str().is_empty() && sensitive_path_is_blocked(approval, &rel) {
        return Err(WorkspaceError::SensitivePath {
            reason: format!(
                "opened handle for '{}' resolved to sensitive path '{}'",
                display_rel.display(),
                rel.display()
            ),
        });
    }
    Ok(final_path)
}

#[cfg(windows)]
pub(crate) fn open_windows_handle_no_reparse(
    target_path: &Path,
    display_rel: &Path,
    require_dir: Option<bool>,
) -> Result<File, WorkspaceError> {
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, GetFileType, BY_HANDLE_FILE_INFORMATION,
        FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        FILE_TYPE_DISK,
    };

    if let Ok(sym) = fs::symlink_metadata(target_path) {
        if is_windows_reparse_point(&sym) {
            return Err(WorkspaceError::SymlinkBlocked {
                reason: format!(
                    "reparse point / symlink / junction blocked at '{}'",
                    display_rel.display()
                ),
            });
        }
    }

    // Do not grant FILE_SHARE_DELETE on directory handles during traversal so an ancestor
    // directory cannot be renamed or unlinked while a child open is in flight.
    let share_mode = if require_dir == Some(true) {
        FILE_SHARE_READ | FILE_SHARE_WRITE
    } else {
        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE
    };

    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
        .share_mode(share_mode)
        .open(target_path)
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                WorkspaceError::NotFound {
                    path: display_rel.display().to_string(),
                }
            } else {
                WorkspaceError::Io {
                    reason: format!("failed to open '{}': {}", display_rel.display(), e),
                }
            }
        })?;

    let raw = file.as_raw_handle() as HANDLE;
    let ftype = unsafe { GetFileType(raw) };
    if ftype != FILE_TYPE_DISK {
        return Err(WorkspaceError::InvalidPath {
            reason: format!(
                "'{}' is a special file (pipe/device), not a regular disk file",
                display_rel.display()
            ),
        });
    }

    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    let ok = unsafe { GetFileInformationByHandle(raw, &mut info) };
    if ok == 0 {
        return Err(WorkspaceError::Io {
            reason: format!(
                "GetFileInformationByHandle failed for '{}': {}",
                display_rel.display(),
                std::io::Error::last_os_error()
            ),
        });
    }
    if (info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT) != 0 {
        return Err(WorkspaceError::SymlinkBlocked {
            reason: format!(
                "reparse point / symlink / junction blocked at '{}'",
                display_rel.display()
            ),
        });
    }
    let is_dir = (info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY) != 0;
    if let Some(want_dir) = require_dir {
        if want_dir && !is_dir {
            return Err(WorkspaceError::InvalidPath {
                reason: format!("'{}' is not a directory", display_rel.display()),
            });
        }
        if !want_dir && is_dir {
            return Err(WorkspaceError::InvalidPath {
                reason: format!(
                    "'{}' is a directory, not a regular file",
                    display_rel.display()
                ),
            });
        }
    }
    Ok(file)
}
