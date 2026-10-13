use crate::messages::ToolResultMeta;
use crate::prompt::{hex_encode, sha256_hex16};
use crate::tools::ToolExecutionOutput;
use crate::types::ErrorCode;
use crate::workspace::{InodeMutationLock, WorkspaceCap};
use serde::Deserialize;
use sha2::{Digest, Sha256};
#[cfg(unix)]
use std::ffi::CString;
use std::ffi::OsStr;
use std::fs::File;
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
#[cfg(unix)]
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
use std::path::Path;
use std::time::Instant;
use uuid::Uuid;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WriteArgs {
    pub path: String,
    pub content: String,
    pub expected_sha256: Option<String>,
    pub if_not_exists: Option<bool>,
}

pub async fn execute_write(
    workspace: &WorkspaceCap,
    inode_locks: &InodeMutationLock,
    raw_args: &str,
    max_write_content_bytes: usize,
) -> ToolExecutionOutput {
    let started = Instant::now();
    let args: WriteArgs = match serde_json::from_str(raw_args) {
        Ok(a) => a,
        Err(e) => {
            return ToolExecutionOutput::error(
                ErrorCode::InvalidArguments,
                format!("invalid JSON arguments for `write`: {}", e),
                ToolResultMeta::default(),
            );
        }
    };

    if args.content.len() > max_write_content_bytes {
        return ToolExecutionOutput::error(
            ErrorCode::InvalidArguments,
            format!(
                "`content` length ({} bytes) exceeds max_write_content_bytes ({} bytes)",
                args.content.len(),
                max_write_content_bytes
            ),
            ToolResultMeta {
                relative_path: Some(args.path),
                ..Default::default()
            },
        );
    }

    let (norm_rel, _abs_path) = match workspace.resolve_write_target(&args.path) {
        Ok(p) => p,
        Err(e) => {
            return ToolExecutionOutput::error(
                e.error_code(),
                e.to_string(),
                ToolResultMeta {
                    relative_path: Some(args.path),
                    ..Default::default()
                },
            );
        }
    };
    let rel_str = norm_rel.to_string_lossy().replace('\\', "/");

    // Open/create parent directories relative to workspace root_dir_fd without following symlinks
    let (parent_dir, file_name) = match workspace.open_parent_dir_no_symlink(&norm_rel, true) {
        Ok(res) => res,
        Err(e) => {
            return ToolExecutionOutput::error(
                e.error_code(),
                e.to_string(),
                ToolResultMeta {
                    relative_path: Some(rel_str),
                    ..Default::default()
                },
            );
        }
    };

    // F-07: key the mutation lock off the already-open parent directory handle's identity plus the
    // leaf name, never by re-deriving it from an absolute path.
    let (_lock_key, _guard) = match inode_locks
        .acquire_for_file(&parent_dir, Some(file_name.as_os_str()))
        .await
    {
        Ok(g) => g,
        Err(e) => {
            return ToolExecutionOutput::error(
                ErrorCode::IoError,
                format!("failed to acquire inode lock for '{}': {}", rel_str, e),
                ToolResultMeta {
                    relative_path: Some(rel_str),
                    ..Default::default()
                },
            );
        }
    };

    #[cfg(unix)]
    let existing_mode_opt = {
        let c_file_name = match CString::new(file_name.as_bytes()) {
            Ok(c) => c,
            Err(_) => {
                return ToolExecutionOutput::error(
                    ErrorCode::InvalidArguments,
                    "file name contains NUL byte".to_string(),
                    ToolResultMeta {
                        relative_path: Some(rel_str),
                        ..Default::default()
                    },
                );
            }
        };

        // Re-verify target metadata via fstatat(AT_SYMLINK_NOFOLLOW) on parent_dir FD after acquiring lock
        let mut stat_buf: libc::stat = unsafe { std::mem::zeroed() };
        let stat_rc = unsafe {
            libc::fstatat(
                parent_dir.as_raw_fd(),
                c_file_name.as_ptr(),
                &mut stat_buf,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if stat_rc == 0 {
            let fmt = stat_buf.st_mode & libc::S_IFMT;
            if fmt == libc::S_IFLNK {
                return ToolExecutionOutput::error(
                    ErrorCode::SymlinkBlocked,
                    format!("refusing to overwrite symlink '{}'", rel_str),
                    ToolResultMeta {
                        relative_path: Some(rel_str),
                        ..Default::default()
                    },
                );
            }
            if fmt != libc::S_IFREG {
                return ToolExecutionOutput::error(
                    ErrorCode::InvalidArguments,
                    format!("target '{}' exists and is not a regular file", rel_str),
                    ToolResultMeta {
                        relative_path: Some(rel_str),
                        ..Default::default()
                    },
                );
            }
            Some((stat_buf.st_mode & 0o7777) as u32)
        } else {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::ENOENT) {
                None
            } else {
                return ToolExecutionOutput::error(
                    ErrorCode::IoError,
                    format!("failed to stat '{}': {}", rel_str, err),
                    ToolResultMeta {
                        relative_path: Some(rel_str),
                        ..Default::default()
                    },
                );
            }
        }
    };

    #[cfg(windows)]
    let existing_mode_opt = match std::fs::symlink_metadata(&_abs_path) {
        Ok(meta) => {
            if crate::workspace::is_windows_reparse_point(&meta) {
                return ToolExecutionOutput::error(
                    ErrorCode::SymlinkBlocked,
                    format!("refusing to overwrite reparse point / symlink '{}'", rel_str),
                    ToolResultMeta {
                        relative_path: Some(rel_str),
                        ..Default::default()
                    },
                );
            }
            if !meta.file_type().is_file() {
                return ToolExecutionOutput::error(
                    ErrorCode::InvalidArguments,
                    format!("target '{}' exists and is not a regular file", rel_str),
                    ToolResultMeta {
                        relative_path: Some(rel_str),
                        ..Default::default()
                    },
                );
            }
            Some(if meta.permissions().readonly() {
                0o444
            } else {
                0o644
            })
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            return ToolExecutionOutput::error(
                ErrorCode::IoError,
                format!("failed to stat '{}': {}", rel_str, e),
                ToolResultMeta {
                    relative_path: Some(rel_str),
                    ..Default::default()
                },
            );
        }
    };

    let if_not_exists = args.if_not_exists.unwrap_or(false);
    if if_not_exists && existing_mode_opt.is_some() {
        return ToolExecutionOutput::error(
            ErrorCode::FileAlreadyExists,
            format!(
                "file '{}' already exists and `if_not_exists=true` was set",
                rel_str
            ),
            ToolResultMeta {
                relative_path: Some(rel_str),
                ..Default::default()
            },
        );
    }

    // Verify expected_sha256 if provided
    if let Some(ref expected) = args.expected_sha256 {
        let exp_norm = expected.trim().to_ascii_lowercase();
        if exp_norm.len() != 16 && exp_norm.len() != 64 {
            return ToolExecutionOutput::error(
                ErrorCode::InvalidArguments,
                format!(
                    "`expected_sha256` must be a 16-char or 64-char hex string, got '{}'",
                    expected
                ),
                ToolResultMeta {
                    relative_path: Some(rel_str),
                    ..Default::default()
                },
            );
        }
        let Some(_) = existing_mode_opt else {
            return ToolExecutionOutput::error(
                ErrorCode::StaleFile,
                format!(
                    "expected SHA256 {}, but file '{}' does not exist",
                    exp_norm, rel_str
                ),
                ToolResultMeta {
                    relative_path: Some(rel_str),
                    ..Default::default()
                },
            );
        };
        let (_, mut existing_file) = match workspace.open_existing_no_symlink(&args.path) {
            Ok(f) => f,
            Err(e) => {
                return ToolExecutionOutput::error(
                    e.error_code(),
                    format!("failed to open '{}' for SHA256 check: {}", rel_str, e),
                    ToolResultMeta {
                        relative_path: Some(rel_str),
                        ..Default::default()
                    },
                );
            }
        };
        let mut hasher = Sha256::new();
        let mut buf = [0u8; 8192];
        loop {
            match existing_file.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => hasher.update(&buf[..n]),
                Err(e) => {
                    return ToolExecutionOutput::error(
                        ErrorCode::IoError,
                        format!("failed to read '{}' for SHA256 check: {}", rel_str, e),
                        ToolResultMeta {
                            relative_path: Some(rel_str),
                            ..Default::default()
                        },
                    );
                }
            }
        }
        let digest = hasher.finalize();
        let current_sha64 = hex_encode(&digest);
        let current_sha16 = &current_sha64[..16];
        if !current_sha64.starts_with(&exp_norm) {
            return ToolExecutionOutput::error(
                ErrorCode::StaleFile,
                format!(
                    "expected SHA256 {}, but current file '{}' has SHA256 {}",
                    exp_norm, rel_str, current_sha16
                ),
                ToolResultMeta {
                    relative_path: Some(rel_str),
                    content_sha256: Some(current_sha16.to_string()),
                    ..Default::default()
                },
            );
        }
    }

    let existing_mode = existing_mode_opt.unwrap_or(0o644);

    if let Err(code_and_msg) = atomic_write_at(
        &parent_dir,
        &file_name,
        args.content.as_bytes(),
        existing_mode,
        if_not_exists,
    ) {
        return ToolExecutionOutput::error(
            code_and_msg.0,
            code_and_msg.1,
            ToolResultMeta {
                relative_path: Some(rel_str),
                ..Default::default()
            },
        );
    }

    let new_sha16 = sha256_hex16(args.content.as_bytes());
    let line_count = args.content.lines().count();

    ToolExecutionOutput::success(
        format!(
            "[Wrote: {} | Bytes: {} | Lines: {} | SHA256: {}]",
            rel_str,
            args.content.len(),
            line_count,
            new_sha16
        ),
        ToolResultMeta {
            relative_path: Some(rel_str),
            content_sha256: Some(new_sha16),
            lines_range: Some((1, line_count)),
            duration_ms: started.elapsed().as_millis() as u64,
            modified_file: true,
            ..Default::default()
        },
    )
}

#[cfg(unix)]
struct TempFileAtGuard {
    dir_fd: RawFd,
    tmp_name: CString,
    committed: bool,
}

#[cfg(unix)]
impl TempFileAtGuard {
    fn new(dir_fd: RawFd, tmp_name: CString) -> Self {
        Self {
            dir_fd,
            tmp_name,
            committed: false,
        }
    }

    fn mark_committed(&mut self) {
        self.committed = true;
    }
}

#[cfg(unix)]
impl Drop for TempFileAtGuard {
    fn drop(&mut self) {
        if !self.committed {
            unsafe {
                let _ = libc::unlinkat(self.dir_fd, self.tmp_name.as_ptr(), 0);
            }
        }
    }
}

/// Race-free atomic file commit relative to an opened parent directory handle using
/// `openat` + (`linkat` + `unlinkat` for `no_clobber`, or `renameat` for overwrite) on Unix,
/// or `GetFinalPathNameByHandleW` + `MoveFileExW(..., MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH)` on Windows.
pub fn atomic_write_at(
    parent_dir: &File,
    file_name: &OsStr,
    bytes: &[u8],
    mode: u32,
    no_clobber: bool,
) -> Result<(), (ErrorCode, String)> {
    #[cfg(unix)]
    {
        let parent_fd = parent_dir.as_raw_fd();
        let file_name_lossy = file_name.to_string_lossy();
        let c_target = CString::new(file_name.as_bytes()).map_err(|_| {
            (
                ErrorCode::InvalidArguments,
                "target file name contains NUL byte".to_string(),
            )
        })?;

        let short_prefix: String = file_name_lossy.chars().take(64).collect();
        let tmp_name_str = format!(".kroma-tmp-{}-{}", short_prefix, Uuid::new_v4());
        let c_tmp = CString::new(tmp_name_str.as_bytes()).map_err(|_| {
            (
                ErrorCode::InvalidArguments,
                "temporary file name contains NUL byte".to_string(),
            )
        })?;

        let raw_tmp_fd = unsafe {
            libc::openat(
                parent_fd,
                c_tmp.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                (mode & 0o7777) as libc::c_uint,
            )
        };
        if raw_tmp_fd < 0 {
            return Err((
                ErrorCode::IoError,
                format!(
                    "openat temporary file failed for '{}': {}",
                    file_name_lossy,
                    std::io::Error::last_os_error()
                ),
            ));
        }

        let mut temp_guard = TempFileAtGuard::new(parent_fd, c_tmp.clone());

        let write_res = (|| -> std::io::Result<()> {
            let mut tmp_file = unsafe { File::from_raw_fd(raw_tmp_fd) };
            unsafe {
                let _ = libc::fchmod(tmp_file.as_raw_fd(), (mode & 0o7777) as libc::mode_t);
            }
            tmp_file.write_all(bytes)?;
            tmp_file.sync_all()?;
            Ok(())
        })();

        if let Err(e) = write_res {
            return Err((
                ErrorCode::IoError,
                format!("atomic write failed for '{}': {}", file_name_lossy, e),
            ));
        }

        if no_clobber {
            let link_rc = unsafe {
                libc::linkat(
                    parent_fd,
                    c_tmp.as_ptr(),
                    parent_fd,
                    c_target.as_ptr(),
                    0,
                )
            };
            if link_rc == 0 {
                unsafe {
                    let _ = libc::unlinkat(parent_fd, c_tmp.as_ptr(), 0);
                }
                let _ = parent_dir.sync_all();
                temp_guard.mark_committed();
                return Ok(());
            }
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EEXIST) {
                return Err((
                    ErrorCode::FileAlreadyExists,
                    format!("file '{}' already exists", file_name_lossy),
                ));
            }
            return Err((
                ErrorCode::IoError,
                format!(
                    "atomic no-clobber linkat failed for '{}': {}",
                    file_name_lossy, err
                ),
            ));
        }

        // Verify target is not a symlink right before renameat
        let mut stat_buf: libc::stat = unsafe { std::mem::zeroed() };
        let stat_rc = unsafe {
            libc::fstatat(
                parent_fd,
                c_target.as_ptr(),
                &mut stat_buf,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if stat_rc == 0 {
            let fmt = stat_buf.st_mode & libc::S_IFMT;
            if fmt == libc::S_IFLNK {
                return Err((
                    ErrorCode::SymlinkBlocked,
                    format!("refusing to overwrite symlink '{}'", file_name_lossy),
                ));
            }
            if fmt != libc::S_IFREG {
                return Err((
                    ErrorCode::InvalidArguments,
                    format!(
                        "target '{}' exists and is not a regular file",
                        file_name_lossy
                    ),
                ));
            }
        }

        let rename_rc = unsafe {
            libc::renameat(
                parent_fd,
                c_tmp.as_ptr(),
                parent_fd,
                c_target.as_ptr(),
            )
        };
        if rename_rc < 0 {
            return Err((
                ErrorCode::IoError,
                format!(
                    "atomic renameat failed for '{}': {}",
                    file_name_lossy,
                    std::io::Error::last_os_error()
                ),
            ));
        }

        let _ = parent_dir.sync_all();
        temp_guard.mark_committed();
        Ok(())
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt as WinOsStrExt;
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{
            MoveFileExW, FILE_FLAG_OPEN_REPARSE_POINT, MOVEFILE_REPLACE_EXISTING,
            MOVEFILE_WRITE_THROUGH,
        };

        let file_name_lossy = file_name.to_string_lossy();
        if file_name_lossy.contains('\0') {
            return Err((
                ErrorCode::InvalidArguments,
                "target file name contains NUL byte".to_string(),
            ));
        }

        // Resolve parent directory path directly from the verified open directory HANDLE
        let parent_path =
            crate::workspace::windows_final_path_by_handle(parent_dir).map_err(|e| {
                (
                    ErrorCode::IoError,
                    format!("GetFinalPathNameByHandleW failed: {}", e),
                )
            })?;
        let short_prefix: String = file_name_lossy.chars().take(64).collect();
        let tmp_name_str = format!(".kroma-tmp-{}-{}", short_prefix, Uuid::new_v4());
        let tmp_path = parent_path.join(&tmp_name_str);
        let target_path = parent_path.join(file_name);

        let write_res = (|| -> std::io::Result<()> {
            let mut tmp_file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
                .open(&tmp_path)?;
            tmp_file.write_all(bytes)?;
            tmp_file.sync_all()?;
            Ok(())
        })();

        if let Err(e) = write_res {
            let _ = std::fs::remove_file(&tmp_path);
            return Err((
                ErrorCode::IoError,
                format!("atomic write failed for '{}': {}", file_name_lossy, e),
            ));
        }

        if !no_clobber {
            if let Ok(meta) = std::fs::symlink_metadata(&target_path) {
                if crate::workspace::is_windows_reparse_point(&meta) {
                    let _ = std::fs::remove_file(&tmp_path);
                    return Err((
                        ErrorCode::SymlinkBlocked,
                        format!(
                            "refusing to overwrite reparse point / symlink '{}'",
                            file_name_lossy
                        ),
                    ));
                }
                if !meta.file_type().is_file() {
                    let _ = std::fs::remove_file(&tmp_path);
                    return Err((
                        ErrorCode::InvalidArguments,
                        format!(
                            "target '{}' exists and is not a regular file",
                            file_name_lossy
                        ),
                    ));
                }
                if meta.permissions().readonly() {
                    let mut perms = meta.permissions();
                    #[allow(clippy::permissions_set_readonly_false)]
                    perms.set_readonly(false);
                    let _ = std::fs::set_permissions(&target_path, perms);
                }
            }
        }

        let tmp_wide: Vec<u16> = tmp_path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let target_wide: Vec<u16> = target_path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let flags = if no_clobber {
            MOVEFILE_WRITE_THROUGH
        } else {
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH
        };
        let moved = unsafe { MoveFileExW(tmp_wide.as_ptr(), target_wide.as_ptr(), flags) };
        if moved == 0 {
            let err = std::io::Error::last_os_error();
            let _ = std::fs::remove_file(&tmp_path);
            if no_clobber && matches!(err.raw_os_error(), Some(80) | Some(183)) {
                return Err((
                    ErrorCode::FileAlreadyExists,
                    format!("file '{}' already exists", file_name_lossy),
                ));
            }
            return Err((
                ErrorCode::IoError,
                format!("MoveFileExW failed for '{}': {}", file_name_lossy, err),
            ));
        }

        let want_readonly = (mode & 0o222) == 0;
        let meta = std::fs::metadata(&target_path).map_err(|e| {
            (
                ErrorCode::IoError,
                format!("metadata failed after write for '{}': {}", file_name_lossy, e),
            )
        })?;
        if meta.permissions().readonly() != want_readonly {
            let mut perms = meta.permissions();
            #[allow(clippy::permissions_set_readonly_false)]
            perms.set_readonly(want_readonly);
            std::fs::set_permissions(&target_path, perms).map_err(|e| {
                (
                    ErrorCode::IoError,
                    format!(
                        "set_permissions failed after write for '{}': {}",
                        file_name_lossy, e
                    ),
                )
            })?;
        }
        Ok(())
    }
}

pub fn atomic_write_file(
    target_abs_path: &Path,
    bytes: &[u8],
    mode: u32,
    no_clobber: bool,
) -> Result<(), (ErrorCode, String)> {
    let parent = target_abs_path.parent().ok_or_else(|| {
        (
            ErrorCode::InvalidArguments,
            "target path has no parent directory".to_string(),
        )
    })?;
    let file_name = target_abs_path.file_name().ok_or_else(|| {
        (
            ErrorCode::InvalidArguments,
            "target path has no file name".to_string(),
        )
    })?;
    #[cfg(unix)]
    {
        let c_parent = CString::new(parent.as_os_str().as_bytes()).map_err(|_| {
            (
                ErrorCode::InvalidArguments,
                "parent path contains NUL byte".to_string(),
            )
        })?;
        let parent_fd = unsafe {
            libc::open(
                c_parent.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if parent_fd < 0 {
            return Err((
                ErrorCode::IoError,
                format!(
                    "failed to open parent directory '{}': {}",
                    parent.display(),
                    std::io::Error::last_os_error()
                ),
            ));
        }
        let parent_dir = unsafe { File::from_raw_fd(parent_fd) };
        atomic_write_at(&parent_dir, file_name, bytes, mode, no_clobber)
    }
    #[cfg(windows)]
    {
        let parent_dir = crate::workspace::open_windows_handle_no_reparse(parent, parent, Some(true))
            .map_err(|e| (e.error_code(), e.to_string()))?;
        atomic_write_at(&parent_dir, file_name, bytes, mode, no_clobber)
    }
}
