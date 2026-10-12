use crate::sensitive_paths::SensitivePathGuard;
use crate::types::ErrorCode;
use std::collections::HashMap;
use std::ffi::{CString, OsStr, OsString};
use std::fs::{self, File};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
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

/// Capability-oriented workspace directory handle enforcing path containment and symlink rules.
#[derive(Debug, Clone)]
pub struct WorkspaceCap {
    root: PathBuf,
    root_dir_fd: Arc<File>,
}

impl WorkspaceCap {
    pub fn new(workspace_root: impl AsRef<Path>) -> Result<Self, WorkspaceError> {
        let root = fs::canonicalize(workspace_root.as_ref()).map_err(|e| WorkspaceError::Io {
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
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn root_dir_fd(&self) -> &File {
        &self.root_dir_fd
    }

    /// Race-free kernel-enforced file open relative to `root_dir_fd` rejecting all symlink components and `..` escapes.
    /// Uses Linux `openat2(RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS)` when supported, falling back to step-by-step
    /// `openat(..., O_RDONLY | O_NOFOLLOW | O_CLOEXEC)` on `ENOSYS` or non-Linux platforms.
    pub fn open_existing_no_symlink(&self, rel: &str) -> Result<(PathBuf, File), WorkspaceError> {
        let norm_rel = Self::validate_relative_components(rel)?;
        if norm_rel != Path::new(".") && SensitivePathGuard::is_sensitive(&norm_rel) {
            return Err(WorkspaceError::SensitivePath {
                reason: format!("access to sensitive path '{}' is blocked", norm_rel.display()),
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
                        reason: format!(
                            "symlink component blocked at '{}'",
                            norm_rel.display()
                        ),
                    });
                }
                Some(libc::EXDEV) => {
                    return Err(WorkspaceError::PathEscape {
                        reason: format!(
                            "path escape blocked at '{}'",
                            norm_rel.display()
                        ),
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

    /// Step-by-step `openat(..., O_RDONLY | O_NOFOLLOW | O_CLOEXEC)` traversal relative to `root_dir_fd`.
    pub fn open_existing_with_openat_steps(
        &self,
        rel: &str,
        norm_rel: PathBuf,
    ) -> Result<(PathBuf, File), WorkspaceError> {
        if norm_rel == Path::new(".") {
            let c_dot = CString::new(".").unwrap();
            let fd = unsafe {
                libc::openat(
                    self.root_dir_fd.as_raw_fd(),
                    c_dot.as_ptr(),
                    libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
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
                            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
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
                                reason: format!("failed to open '{}': {}", norm_rel.display(), err),
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

    /// Open a workspace-relative directory as a race-free, symlink-free `File` handle (F-02).
    ///
    /// Uses the same `openat2(RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS)` guarantee as
    /// [`Self::open_existing_no_symlink`], with `O_DIRECTORY`, so navigation tools can list a
    /// directory from an already-validated handle instead of re-checking a path and racing an
    /// attacker that swaps the directory for a symlink between check and use.
    pub fn open_dir_no_symlink(&self, rel: &str) -> Result<(PathBuf, File), WorkspaceError> {
        let norm_rel = Self::validate_relative_components(rel)?;
        if norm_rel != Path::new(".") && SensitivePathGuard::is_sensitive(&norm_rel) {
            return Err(WorkspaceError::SensitivePath {
                reason: format!("access to sensitive path '{}' is blocked", norm_rel.display()),
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
                flags: (libc::O_RDONLY | libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW)
                    as u64,
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

        // Portable fallback: step-by-step openat(O_DIRECTORY | O_NOFOLLOW).
        let mut cur_file: Option<File> = None;
        for comp in norm_rel.components() {
            if let Component::Normal(part) = comp {
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
                        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
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
                        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
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

    /// Enumerate the immediate children of a workspace-relative directory through a race-free
    /// directory FD (F-02). Entries are `fstatat(AT_SYMLINK_NOFOLLOW)`-classified so callers can
    /// skip symlinks and never follow an entry that was swapped in after a path check.
    pub fn read_dir_no_symlink(
        &self,
        rel: &str,
    ) -> Result<(PathBuf, Vec<DirEntryInfo>), WorkspaceError> {
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
            let name_bytes = unsafe { std::ffi::CStr::from_ptr((*ent).d_name.as_ptr()) }.to_bytes();
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

    /// Distinguish a symlinked path from a plain non-directory (F-02). `openat2` with
    /// `O_DIRECTORY | RESOLVE_NO_SYMLINKS` reports `ENOTDIR` for both, so re-stat the leaf with
    /// `fstatat(AT_SYMLINK_NOFOLLOW)` relative to its (symlink-free) parent directory.
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

        let c_dot = CString::new(".").unwrap();
        let root_dup = unsafe {
            libc::openat(
                self.root_dir_fd.as_raw_fd(),
                c_dot.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
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
                            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                        )
                    };
                    if next_fd < 0 {
                        let err = std::io::Error::last_os_error();
                        if create_missing && err.raw_os_error() == Some(libc::ENOENT) {
                            let mk_res =
                                unsafe { libc::mkdirat(cur_dir.as_raw_fd(), c_part.as_ptr(), 0o755) };
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
                                    libc::O_RDONLY
                                        | libc::O_DIRECTORY
                                        | libc::O_NOFOLLOW
                                        | libc::O_CLOEXEC,
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

    /// Normalize and validate a workspace-relative path without touching the filesystem yet.
    /// Rejects absolute paths and any `..` parent traversal.
    pub fn validate_relative_components(rel: &str) -> Result<PathBuf, WorkspaceError> {
        let trimmed = rel.trim();
        if trimmed.is_empty() {
            return Err(WorkspaceError::InvalidPath {
                reason: "path must not be empty".to_string(),
            });
        }
        let p = Path::new(trimmed);
        if p.is_absolute() || trimmed.starts_with('/') || trimmed.starts_with('\\') {
            return Err(WorkspaceError::PathEscape {
                reason: format!("absolute path '{}' is not allowed", rel),
            });
        }

        let mut normalized = PathBuf::new();
        for comp in p.components() {
            match comp {
                Component::CurDir => {}
                Component::Normal(part) => {
                    if part.is_empty() {
                        continue;
                    }
                    normalized.push(part);
                }
                Component::ParentDir => {
                    return Err(WorkspaceError::PathEscape {
                        reason: format!("parent traversal '..' in '{}' is not allowed", rel),
                    });
                }
                Component::RootDir | Component::Prefix(_) => {
                    return Err(WorkspaceError::PathEscape {
                        reason: format!("root/prefix component in '{}' is not allowed", rel),
                    });
                }
            }
        }

        if normalized.as_os_str().is_empty() {
            normalized.push(".");
        }
        Ok(normalized)
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
        if SensitivePathGuard::is_sensitive(&norm_rel) {
            return Err(WorkspaceError::SensitivePath {
                reason: format!("access to sensitive path '{}' is blocked", norm_rel.display()),
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

                if sym_meta.file_type().is_symlink() {
                    if is_leaf && forbid_leaf_symlink {
                        return Err(WorkspaceError::SymlinkBlocked {
                            reason: format!(
                                "refusing to modify symlink target at '{}'",
                                norm_rel.display()
                            ),
                        });
                    }
                    let canonical_target = fs::canonicalize(&cur).map_err(|_| {
                        WorkspaceError::PathEscape {
                            reason: format!(
                                "dangling or external symlink '{}' blocked",
                                norm_rel.display()
                            ),
                        }
                    })?;
                    if !canonical_target.starts_with(&self.root) {
                        return Err(WorkspaceError::PathEscape {
                            reason: format!(
                                "symlink '{}' points outside workspace ({})",
                                norm_rel.display(),
                                canonical_target.display()
                            ),
                        });
                    }
                    let rel_target = canonical_target
                        .strip_prefix(&self.root)
                        .unwrap_or(&canonical_target);
                    if SensitivePathGuard::is_sensitive(rel_target) {
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
        if !canonical.starts_with(&self.root) {
            return Err(WorkspaceError::PathEscape {
                reason: format!("resolved path '{}' escapes workspace", norm_rel.display()),
            });
        }
        let rel_canonical = canonical.strip_prefix(&self.root).unwrap_or(&canonical);
        if SensitivePathGuard::is_sensitive(rel_canonical) {
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
        if SensitivePathGuard::is_sensitive(&norm_rel) {
            return Err(WorkspaceError::SensitivePath {
                reason: format!("writing to sensitive path '{}' is blocked", norm_rel.display()),
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
                        if sym_meta.file_type().is_symlink() {
                            if is_leaf {
                                return Err(WorkspaceError::SymlinkBlocked {
                                    reason: format!(
                                        "refusing to overwrite symlink '{}'",
                                        norm_rel.display()
                                    ),
                                });
                            }
                            let canonical_target = fs::canonicalize(&cur).map_err(|_| {
                                WorkspaceError::PathEscape {
                                    reason: format!(
                                        "dangling or external directory symlink '{}' blocked",
                                        norm_rel.display()
                                    ),
                                }
                            })?;
                            if !canonical_target.starts_with(&self.root) {
                                return Err(WorkspaceError::PathEscape {
                                    reason: format!(
                                        "directory symlink '{}' points outside workspace",
                                        norm_rel.display()
                                    ),
                                });
                            }
                            let rel_target = canonical_target
                                .strip_prefix(&self.root)
                                .unwrap_or(&canonical_target);
                            if SensitivePathGuard::is_sensitive(rel_target) {
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
        // F-07: computing a lock key must never create directories or canonicalize as a side
        // effect. `metadata` resolves `..` and symlinks in the parent chain at the OS level, which
        // yields the same `(dev, ino)` as the canonical parent for an existing directory.
        let p_meta = fs::metadata(parent)?;
        let file_name = target_abs_path
            .file_name()
            .ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::InvalidInput, "missing file name")
            })?
            .to_os_string();
        Ok(InodeLockKey {
            parent_dev: p_meta.dev(),
            parent_ino: p_meta.ino(),
            file_name,
        })
    }

    /// F-07: derive the lock key from an already-open file/directory FD via `fstat`, never by
    /// re-deriving it from an absolute path. `leaf_name` is the child name when `fd` is a parent
    /// directory, or `None` when `fd` is the file itself.
    pub fn key_for_fd(fd: RawFd, leaf_name: Option<&OsStr>) -> std::io::Result<InodeLockKey> {
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        let rc = unsafe { libc::fstat(fd, &mut st) };
        if rc != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(InodeLockKey {
            parent_dev: st.st_dev as u64,
            parent_ino: st.st_ino as u64,
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

    /// F-07: acquire the mutation lock keyed by the identity of an already-open FD, so a path that
    /// is swapped after the FD was opened cannot redirect the lock to a different directory object.
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
