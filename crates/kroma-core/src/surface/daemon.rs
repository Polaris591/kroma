//! daemon surface of the multi-surface runtime (split from `surface.rs`).
//!
//! # Supported deployment model
//!
//! `kroma serve` is a **local, single-user** daemon:
//!
//! * The default listen endpoint is loopback TCP (`127.0.0.1:7878`). A Unix domain socket
//!   (`unix:///path/to/kroma.sock`) is the recommended local transport: the socket itself is
//!   always created `0600`. A parent directory that the daemon creates itself, or that the daemon
//!   already owns and that is not sticky, is restricted to `0700` (owner-only). A shared parent
//!   directory (`/tmp`-style sticky, or owned by another user) is left untouched and the socket's
//!   own `0600` mode is the protection; a shared parent that is writable by others without the
//!   sticky bit is refused, because another user could replace the socket path. A non-loopback TCP
//!   bind is accepted only with an explicit non-empty `--auth-token`.
//! * There is **no** daemon-to-daemon forwarding and **no** automatic peer discovery: a client
//!   must already know the endpoint it wants to reach, and a session owned by another daemon is
//!   refused rather than guessed at.
//! * There is **no multi-user isolation**: every connection is served with the privileges of the
//!   OS user that started the daemon, and sessions share that user's filesystem. Do not expose a
//!   daemon to users who are not trusted with the daemon's user account.
//! * A non-loopback TCP bind is **plaintext (no TLS)** and the `--auth-token` travels in JSON-RPC
//!   request parameters, so it must only be used on a trusted network. The token value itself is
//!   never logged.

use super::*;

pub(crate) fn looks_like_http_request(trimmed: &str) -> bool {
    const HTTP_PREFIXES: &[&str] = &[
        "GET ", "POST ", "PUT ", "DELETE ", "OPTIONS ", "HEAD ", "CONNECT ", "Host:", "Origin:",
    ];
    HTTP_PREFIXES.iter().any(|p| trimmed.starts_with(p))
}

pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.is_empty() || b.is_empty() {
        return false;
    }
    let ha = sha256_hex64(a);
    let hb = sha256_hex64(b);
    let mut diff = if a.len() == b.len() { 0u8 } else { 1u8 };
    for (&x, &y) in ha.as_bytes().iter().zip(hb.as_bytes().iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

pub(crate) async fn read_bounded_jsonrpc_line<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    max_bytes: usize,
) -> Result<Option<String>, String> {
    let mut buf = Vec::new();
    loop {
        let available = reader
            .fill_buf()
            .await
            .map_err(|e| format!("stream read error: {}", e))?;
        if available.is_empty() {
            if buf.is_empty() {
                return Ok(None);
            }
            let s = String::from_utf8(buf).map_err(|_| "frame is not valid UTF-8".to_string())?;
            return Ok(Some(s));
        }
        if let Some(newline_pos) = available.iter().position(|&b| b == b'\n') {
            let take_len = newline_pos + 1;
            if buf.len().saturating_add(take_len) > max_bytes {
                reader.consume(take_len);
                return Err(format!(
                    "JSON-RPC frame exceeds maximum size of {} bytes",
                    max_bytes
                ));
            }
            buf.extend_from_slice(&available[..newline_pos]);
            reader.consume(take_len);
            let s = String::from_utf8(buf).map_err(|_| "frame is not valid UTF-8".to_string())?;
            return Ok(Some(s));
        } else {
            let chunk_len = available.len();
            if buf.len().saturating_add(chunk_len) > max_bytes {
                reader.consume(chunk_len);
                return Err(format!(
                    "JSON-RPC frame exceeds maximum size of {} bytes",
                    max_bytes
                ));
            }
            buf.extend_from_slice(available);
            reader.consume(chunk_len);
        }
    }
}

/// Validated listen endpoint for `kroma serve` (loopback/token-guarded TCP or local `0600` Unix
/// domain socket).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonListenEndpoint {
    Tcp(SocketAddr),
    Unix(PathBuf),
}

/// Explicit security warning emitted when `kroma serve` binds to a non-loopback TCP address.
///
/// Connection is plaintext (no TLS) and the authentication token travels in JSON-RPC parameters,
/// so it must only be used on a trusted network. The token value itself is deliberately omitted
/// from the warning to avoid credential leakage.
pub const NON_LOOPBACK_PLAINTEXT_WARNING: &str =
    "binding to a non-loopback address; connection is plaintext (no TLS) and the auth token travels in JSON-RPC parameters, so it must only be used on a trusted network";

impl DaemonListenEndpoint {
    /// Returns true if this endpoint is a local loopback interface or Unix domain socket.
    pub fn is_loopback(&self) -> bool {
        match self {
            DaemonListenEndpoint::Unix(_) => true,
            DaemonListenEndpoint::Tcp(addr) => match addr.ip() {
                IpAddr::V4(v4) => v4.is_loopback(),
                IpAddr::V6(v6) => v6.is_loopback(),
            },
        }
    }

    /// If this endpoint is a non-loopback TCP address, returns the explicit security warning.
    pub fn non_loopback_warning(&self) -> Option<&'static str> {
        if self.is_loopback() {
            None
        } else {
            Some(NON_LOOPBACK_PLAINTEXT_WARNING)
        }
    }
}

pub fn is_unix_listen_spec(listen_addr: &str) -> bool {
    let trimmed = listen_addr.trim();
    trimmed.starts_with("unix://")
        || trimmed.starts_with('/')
        || trimmed.starts_with("./")
        || trimmed.ends_with(".sock")
}

/// Validate a `kroma serve` listen endpoint (`127.0.0.1:<PORT>`, `unix:///path/to/sock`, or
/// `/path/to/kroma.sock`) against the Gate 6 security policy.
///
/// Non-loopback TCP bind addresses (`0.0.0.0`, LAN IPs, `[::]`) are strictly rejected unless a
/// non-empty `--auth-token` is configured.
pub fn validate_daemon_listen_endpoint(
    listen_addr: &str,
    auth_token: Option<&str>,
) -> Result<DaemonListenEndpoint, String> {
    let trimmed = listen_addr.trim();
    if trimmed.is_empty() {
        return Err("empty listen address for kroma serve".to_string());
    }
    if is_unix_listen_spec(trimmed) {
        #[cfg(not(unix))]
        {
            return Err("Unix domain sockets are only supported on Unix platforms".to_string());
        }
        #[cfg(unix)]
        {
            let raw_path = trimmed.strip_prefix("unix://").unwrap_or(trimmed).trim();
            if raw_path.is_empty() || raw_path.ends_with('/') {
                return Err(format!("invalid Unix domain socket path '{}'", listen_addr));
            }
            return Ok(DaemonListenEndpoint::Unix(PathBuf::from(raw_path)));
        }
    }

    let normalized = if let Some(port_str) = trimmed.strip_prefix("localhost:") {
        format!("127.0.0.1:{}", port_str)
    } else {
        trimmed.to_string()
    };

    let addr: SocketAddr = normalized
        .parse()
        .or_else(|_| {
            normalized
                .to_socket_addrs()
                .ok()
                .and_then(|mut iter| iter.next())
                .ok_or(())
        })
        .map_err(|_| format!("invalid socket address '{}'", listen_addr))?;

    let is_loopback = match addr.ip() {
        IpAddr::V4(v4) => v4.is_loopback(),
        IpAddr::V6(v6) => v6.is_loopback(),
    };
    let has_token = auth_token
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .is_some();

    if !is_loopback && !has_token {
        return Err(format!(
            "security policy violation: refusing to bind non-loopback address '{}' without --auth-token <TOKEN>",
            addr
        ));
    }

    Ok(DaemonListenEndpoint::Tcp(addr))
}

/// Validate a `kroma serve` listen address against the Gate 6 security policy.
pub fn validate_daemon_listen_addr(
    listen_addr: &str,
    auth_token: Option<&str>,
) -> Result<SocketAddr, String> {
    match validate_daemon_listen_endpoint(listen_addr, auth_token)? {
        DaemonListenEndpoint::Tcp(addr) => Ok(addr),
        DaemonListenEndpoint::Unix(_) => Ok(SocketAddr::from(([127, 0, 0, 1], 0))),
    }
}

enum DaemonListenerKind {
    Tcp(TcpListener),
    #[cfg(unix)]
    Unix {
        listener: tokio::net::UnixListener,
        path: PathBuf,
        guard: UnixSocketGuard,
    },
}

/// RAII cleanup guard for a bound Unix domain socket.
///
/// Ensures the socket is unlinked when dropped or shut down, but **only** if the filesystem
/// entry still has the exact same device and inode identity as when it was bound. If the path
/// was replaced by an attacker, operator, or concurrent process (with a regular file, symlink,
/// or different socket), `unlink` refuses to delete the replacement file.
#[cfg(unix)]
#[derive(Debug)]
pub(crate) struct UnixSocketGuard {
    path: PathBuf,
    dev: u64,
    ino: u64,
}

#[cfg(unix)]
impl UnixSocketGuard {
    pub(crate) fn new(path: PathBuf, dev: u64, ino: u64) -> Self {
        Self { path, dev, ino }
    }

    pub(crate) fn unlink(&self) -> std::io::Result<bool> {
        use std::os::unix::fs::{FileTypeExt, MetadataExt};
        match std::fs::symlink_metadata(&self.path) {
            Ok(meta) => {
                if meta.file_type().is_symlink() || !meta.file_type().is_socket() {
                    return Ok(false);
                }
                if meta.dev() == self.dev && meta.ino() == self.ino {
                    std::fs::remove_file(&self.path)?;
                    Ok(true)
                } else {
                    Ok(false)
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e),
        }
    }
}

#[cfg(unix)]
impl Drop for UnixSocketGuard {
    fn drop(&mut self) {
        let _ = self.unlink();
    }
}

/// Test-only RAII guard that makes `restrict_socket_no_follow` fail on the current thread
/// until dropped.
#[cfg(all(test, unix))]
pub(crate) struct SocketChmodFaultGuard;

#[cfg(all(test, unix))]
impl SocketChmodFaultGuard {
    pub(crate) fn arm() -> Self {
        SOCKET_CHMOD_FAILING.with(|flag| flag.set(true));
        Self
    }
}

#[cfg(all(test, unix))]
impl Drop for SocketChmodFaultGuard {
    fn drop(&mut self) {
        SOCKET_CHMOD_FAILING.with(|flag| flag.set(false));
    }
}

#[cfg(all(test, unix))]
thread_local! {
    static SOCKET_CHMOD_FAILING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(all(test, unix))]
fn socket_chmod_fault_armed() -> bool {
    SOCKET_CHMOD_FAILING.with(|flag| flag.get())
}

// Only the Unix `restrict_socket_no_follow` calls this; on other targets the function and its
// call site do not exist, so no fallback stub is generated there.
#[cfg(all(unix, not(test)))]
fn socket_chmod_fault_armed() -> bool {
    false
}

/// Test-only RAII hook that intercepts execution after liveliness probe and before stale socket unlink.
#[cfg(all(test, unix))]
pub(crate) struct StaleProbeHookGuard;

#[cfg(all(test, unix))]
type StaleProbeHook = Box<dyn FnMut(&Path)>;

#[cfg(all(test, unix))]
thread_local! {
    static STALE_PROBE_HOOK: std::cell::RefCell<Option<StaleProbeHook>> = const { std::cell::RefCell::new(None) };
}

#[cfg(all(test, unix))]
impl StaleProbeHookGuard {
    pub(crate) fn set(hook: impl FnMut(&Path) + 'static) -> Self {
        STALE_PROBE_HOOK.with(|h| *h.borrow_mut() = Some(Box::new(hook)));
        Self
    }
}

#[cfg(all(test, unix))]
impl Drop for StaleProbeHookGuard {
    fn drop(&mut self) {
        STALE_PROBE_HOOK.with(|h| *h.borrow_mut() = None);
    }
}

#[cfg(all(test, unix))]
fn invoke_stale_probe_hook(path: &Path) {
    STALE_PROBE_HOOK.with(|h| {
        if let Some(ref mut hook) = *h.borrow_mut() {
            hook(path);
        }
    });
}

/// Restrict a Unix domain socket's permissions to `mode` without following symlinks.
#[cfg(unix)]
fn restrict_socket_no_follow(path: &Path, mode: u32) -> std::io::Result<()> {
    if socket_chmod_fault_armed() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "injected socket chmod failure",
        ));
    }

    use std::os::unix::ffi::OsStrExt;
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;

    // Attempt fchmodat with AT_SYMLINK_NOFOLLOW. Modern Linux empirically supports setting
    // socket file node permissions directly without traversing symlinks. If unsupported on
    // another platform, we fail closed rather than falling back to symlink-traversing chmod.
    let ret = unsafe {
        libc::fchmodat(
            libc::AT_FDCWD,
            c_path.as_ptr(),
            mode as libc::mode_t,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if ret == 0 {
        return Ok(());
    }
    // Fail-closed: do NOT fall back to path-based chmod which would follow symlinks.
    Err(std::io::Error::last_os_error())
}

/// What the daemon does with an existing Unix-socket parent directory.
///
/// The daemon is a local single-user surface: it must never tighten a directory it shares with
/// other users, and it must never bind where another user could unlink and replace the socket
/// path.
#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UnixSocketParentPolicy {
    /// The daemon created the directory itself, so it is restricted to `0700` (owner-only).
    RestrictOwnerOnly,
    /// Leave the pre-existing directory exactly as it is and rely on the socket's own `0600` mode.
    LeaveShared,
    /// Refuse the bind: another user can write the directory without the sticky bit, so it could
    /// replace the socket path.
    RefuseSharedWritable,
}

#[cfg(unix)]
fn unix_socket_parent_policy(
    created: bool,
    owner_uid: u32,
    mode: u32,
    euid: u32,
) -> UnixSocketParentPolicy {
    if created {
        // The daemon created this directory itself, so it is ours to tighten.
        return UnixSocketParentPolicy::RestrictOwnerOnly;
    }
    // A sticky directory is only safe if owned by root (system /tmp) or current EUID.
    // If owned by an untrusted third party, the directory owner can unlink/rename entries!
    if mode & 0o1000 != 0 {
        if owner_uid == 0 || owner_uid == euid {
            UnixSocketParentPolicy::LeaveShared
        } else {
            UnixSocketParentPolicy::RefuseSharedWritable
        }
    } else if mode & 0o022 != 0 {
        // A non-sticky directory that is group-writable or other-writable allows other users
        // to unlink/replace entries in it, even if owned by the current user. Refuse it.
        UnixSocketParentPolicy::RefuseSharedWritable
    } else if owner_uid != 0 && owner_uid != euid {
        // Third-party owned directory: even if not writable currently, the third-party owner
        // can chmod or rename it at will.
        UnixSocketParentPolicy::RefuseSharedWritable
    } else {
        // Pre-existing directory owned by root or current EUID, not group/other writable.
        UnixSocketParentPolicy::LeaveShared
    }
}

/// Enforce the Unix-socket parent-directory policy, creating the directory when it does not exist.
///
/// Returns `Ok(true)` if the directory is verified to be a controlled private directory (owned by
/// current EUID, with no group or world write permissions).
///
/// A directory the daemon creates itself is the daemon's own and is restricted to `0700`. A
/// pre-existing directory is **never** modified: it is left exactly as it is (so a shared anchor
/// such as `/run` keeps working) and the socket's own `0600` mode is the protection. A pre-existing
/// directory that is writable by other users without the sticky bit is refused with an explicit
/// error naming the directory, because another user could unlink and replace the socket path. A
/// symlinked parent and a non-directory parent are always refused.
///
/// All existing ancestor directories up to the filesystem root are inspected: every ancestor
/// must be a real directory (no symlinks) owned by root (uid 0) or current EUID, and not
/// writable by other users without the sticky bit.
#[cfg(unix)]
fn secure_unix_socket_parent(parent: &Path) -> Result<bool, String> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    // SAFETY: `geteuid` has no preconditions and never fails.
    let euid = unsafe { libc::geteuid() };

    // Threat model & TOCTOU boundary:
    // We trust root (UID 0, system supervisor) and the current operator (EUID).
    // Pre-checking existing ancestor directories by lstat validates that no component in the path
    // chain is an untrusted trap or symlink.
    // NOTE: In POSIX filesystems, checking paths by name cannot claim to be completely race-free if
    // an untrusted user has write permissions in any ancestor, nor within the same EUID / root threat
    // domain. However, verifying that all existing ancestors are real directories owned by root or EUID
    // ensures missing components are never created inside untrusted traps.
    //
    // Step 1: Validate all existing ancestors up to the filesystem root BEFORE creating any directory.
    let mut current = parent.parent();
    while let Some(ancestor) = current {
        if ancestor.as_os_str().is_empty() {
            break;
        }
        match std::fs::symlink_metadata(ancestor) {
            Ok(anc_meta) => {
                if anc_meta.file_type().is_symlink() {
                    return Err(format!(
                        "refusing to bind Unix socket under symlinked ancestor directory '{}'",
                        ancestor.display()
                    ));
                }
                if !anc_meta.is_dir() {
                    return Err(format!(
                        "Unix socket ancestor path '{}' is not a directory",
                        ancestor.display()
                    ));
                }
                match unix_socket_parent_policy(false, anc_meta.uid(), anc_meta.mode(), euid) {
                    UnixSocketParentPolicy::LeaveShared => {}
                    UnixSocketParentPolicy::RestrictOwnerOnly => unreachable!(),
                    UnixSocketParentPolicy::RefuseSharedWritable => {
                        return Err(format!(
                            "refusing to bind Unix socket under untrusted/shared ancestor directory '{}': owned by uid {} (current uid {}), mode {:o}; must be owned by root or current user and not writable by others",
                            ancestor.display(),
                            anc_meta.uid(),
                            euid,
                            anc_meta.mode() & 0o7777,
                        ));
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(format!(
                    "failed to inspect Unix socket ancestor directory '{}': {}",
                    ancestor.display(),
                    e
                ));
            }
        }
        current = ancestor.parent();
    }

    // Step 2: Now inspect or create the parent directory itself.
    let (meta, created) = match std::fs::symlink_metadata(parent) {
        Ok(meta) => (meta, false),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir_all(parent).map_err(|e| {
                format!(
                    "failed to create parent directory for Unix socket parent '{}': {}",
                    parent.display(),
                    e
                )
            })?;
            let created_meta = std::fs::symlink_metadata(parent).map_err(|e| {
                format!(
                    "failed to inspect created Unix socket parent directory '{}': {}",
                    parent.display(),
                    e
                )
            })?;
            (created_meta, true)
        }
        Err(e) => {
            return Err(format!(
                "failed to inspect Unix socket parent directory '{}': {}",
                parent.display(),
                e
            ));
        }
    };

    if meta.file_type().is_symlink() {
        return Err(format!(
            "refusing to bind Unix socket in symlinked parent directory '{}'",
            parent.display()
        ));
    }
    if !meta.is_dir() {
        return Err(format!(
            "Unix socket parent path '{}' is not a directory",
            parent.display()
        ));
    }

    let is_controlled_private = match unix_socket_parent_policy(
        created,
        meta.uid(),
        meta.mode(),
        euid,
    ) {
        UnixSocketParentPolicy::RestrictOwnerOnly => {
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700)).map_err(
                |e| {
                    format!(
                        "failed to set 0700 permissions on Unix socket parent directory '{}': {}",
                        parent.display(),
                        e
                    )
                },
            )?;
            true
        }
        UnixSocketParentPolicy::LeaveShared => meta.uid() == euid && (meta.mode() & 0o022 == 0),
        UnixSocketParentPolicy::RefuseSharedWritable => {
            return Err(format!(
                "refusing to bind Unix socket in untrusted/shared parent directory '{}': owned by uid {} (current uid {}), mode {:o}; must be owned by root or current user and not writable by others",
                parent.display(),
                meta.uid(),
                euid,
                meta.mode() & 0o7777,
            ));
        }
    };

    Ok(is_controlled_private)
}

/// Local TCP or Unix Domain Socket JSON-RPC 2.0 daemon (`kroma serve`).
pub struct KromaDaemon {
    listener: DaemonListenerKind,
    local_addr: SocketAddr,
    unix_path: Option<PathBuf>,
    dispatcher: RpcDispatcher,
    /// When set, the daemon routes every connection through the bounded multi-session control plane
    /// instead of a single runtime.
    manager: Option<SessionManager>,
}

impl KromaDaemon {
    /// Validate security policy and bind the TCP or Unix domain socket listener.
    pub async fn bind(listen_addr: &str, dispatcher: RpcDispatcher) -> Result<Self, String> {
        match validate_daemon_listen_endpoint(listen_addr, dispatcher.auth_token())? {
            DaemonListenEndpoint::Tcp(addr) => {
                let listener = TcpListener::bind(addr)
                    .await
                    .map_err(|e| format!("failed to bind TCP listener on {}: {}", addr, e))?;
                let local_addr = listener
                    .local_addr()
                    .map_err(|e| format!("failed to query local address: {}", e))?;
                Ok(Self {
                    listener: DaemonListenerKind::Tcp(listener),
                    local_addr,
                    unix_path: None,
                    dispatcher,
                    manager: None,
                })
            }
            DaemonListenEndpoint::Unix(path) => {
                #[cfg(not(unix))]
                {
                    let _ = path;
                    Err("Unix domain sockets are only supported on Unix platforms".to_string())
                }
                #[cfg(unix)]
                {
                    use std::os::unix::fs::{FileTypeExt, MetadataExt};
                    let raw_parent = path.parent().unwrap_or_else(|| Path::new(""));
                    let parent = if raw_parent.as_os_str().is_empty() {
                        Path::new(".")
                    } else {
                        raw_parent
                    };

                    let is_controlled_private = secure_unix_socket_parent(parent)?;

                    match std::fs::symlink_metadata(&path) {
                        Ok(meta) => {
                            if meta.file_type().is_symlink() {
                                return Err(format!(
                                    "refusing to bind Unix socket on symlink '{}'",
                                    path.display()
                                ));
                            }
                            if meta.is_dir() {
                                return Err(format!(
                                    "Unix socket path '{}' is a directory",
                                    path.display()
                                ));
                            }
                            if !meta.file_type().is_socket() {
                                return Err(format!(
                                    "refusing to bind Unix socket on existing non-socket file '{}'",
                                    path.display()
                                ));
                            }

                            // It is an existing socket. Verify owner matches current EUID.
                            let euid = unsafe { libc::geteuid() };
                            if meta.uid() != euid {
                                return Err(format!(
                                    "refusing to overwrite Unix socket '{}' owned by uid {} (current uid {})",
                                    path.display(),
                                    meta.uid(),
                                    euid
                                ));
                            }

                            // Stale socket recovery is only permitted in a controlled private directory.
                            if !is_controlled_private {
                                return Err(format!(
                                    "refusing to remove existing Unix socket in non-private directory '{}'; stale socket recovery is only permitted in a controlled private directory",
                                    parent.display()
                                ));
                            }

                            let stale_dev = meta.dev();
                            let stale_ino = meta.ino();

                            // Probe whether the existing socket is alive.
                            let is_alive = match tokio::time::timeout(
                                std::time::Duration::from_millis(200),
                                tokio::net::UnixStream::connect(&path),
                            )
                            .await
                            {
                                Ok(Ok(_)) => true,
                                Ok(Err(ref e))
                                    if e.kind() == std::io::ErrorKind::ConnectionRefused =>
                                {
                                    false
                                }
                                Ok(Err(_)) => true,
                                Err(_timeout) => true,
                            };

                            if is_alive {
                                return Err(format!(
                                    "refusing to overwrite active Unix socket '{}'",
                                    path.display()
                                ));
                            }

                            #[cfg(all(test, unix))]
                            invoke_stale_probe_hook(&path);

                            // Note on TOCTOU boundary:
                            // In POSIX, `lstat` followed by `unlink` is not atomic. Stale socket recovery
                            // is strictly restricted to a controlled private directory (is_controlled_private == true,
                            // owned by current EUID, mode 0700), which prevents unprivileged cross-user replacement races.
                            // However, within the same EUID / root threat domain, a concurrent process could theoretically
                            // replace the socket path during the liveliness probe. We therefore re-inspect the path
                            // immediately before unlinking: verify it remains a socket, is still owned by current EUID,
                            // and retains the exact same (dev, ino) identity as before the probe. If anything changed,
                            // refuse the unlink. We do not claim absolute race-freedom against same-UID/root concurrent processes.
                            let pre_unlink_meta = std::fs::symlink_metadata(&path).map_err(|e| {
                                format!(
                                    "failed to re-inspect Unix socket '{}' before removing stale socket: {}",
                                    path.display(),
                                    e
                                )
                            })?;
                            if pre_unlink_meta.file_type().is_symlink()
                                || !pre_unlink_meta.file_type().is_socket()
                                || pre_unlink_meta.uid() != euid
                                || pre_unlink_meta.dev() != stale_dev
                                || pre_unlink_meta.ino() != stale_ino
                            {
                                return Err(format!(
                                    "refusing to remove stale Unix socket '{}': socket identity changed during liveliness probe (possible replacement race)",
                                    path.display()
                                ));
                            }

                            // Confirmed stale socket in controlled private directory with matching owner & inode:
                            std::fs::remove_file(&path).map_err(|e| {
                                format!(
                                    "failed to remove stale Unix socket '{}': {}",
                                    path.display(),
                                    e
                                )
                            })?;
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                        Err(e) => {
                            return Err(format!(
                                "failed to inspect existing path '{}': {}",
                                path.display(),
                                e
                            ));
                        }
                    }
                    let listener = tokio::net::UnixListener::bind(&path).map_err(|e| {
                        format!(
                            "failed to bind Unix domain socket on {}: {}",
                            path.display(),
                            e
                        )
                    })?;

                    // Record the bound socket's identity (dev and inode) before setting permissions
                    let bound_meta = std::fs::symlink_metadata(&path).map_err(|e| {
                        format!(
                            "failed to inspect newly bound Unix socket '{}': {}",
                            path.display(),
                            e
                        )
                    })?;
                    if bound_meta.file_type().is_symlink() || !bound_meta.file_type().is_socket() {
                        return Err(format!(
                            "bound Unix socket path '{}' is not a socket",
                            path.display()
                        ));
                    }
                    let bound_dev = bound_meta.dev();
                    let bound_ino = bound_meta.ino();

                    // Construct guard immediately to ensure cleanup if subsequent steps fail
                    let guard = UnixSocketGuard::new(path.clone(), bound_dev, bound_ino);

                    if let Err(e) = restrict_socket_no_follow(&path, 0o600) {
                        let _ = guard.unlink();
                        return Err(format!(
                            "failed to set 0600 permissions on Unix socket {}: {}",
                            path.display(),
                            e
                        ));
                    }

                    // Verify socket identity after set_permissions
                    let post_meta = match std::fs::symlink_metadata(&path) {
                        Ok(meta) => meta,
                        Err(e) => {
                            let _ = guard.unlink();
                            return Err(format!(
                                "failed to verify Unix socket '{}' after chmod: {}",
                                path.display(),
                                e
                            ));
                        }
                    };
                    if post_meta.dev() != bound_dev || post_meta.ino() != bound_ino {
                        // The socket identity changed, meaning another file replaced it.
                        // Guard will be dropped here; guard.unlink() checks (dev, ino),
                        // which will mismatch and refuse to delete the replacement file.
                        return Err(format!(
                            "Unix socket identity mismatch on '{}' after chmod (possible replacement race)",
                            path.display()
                        ));
                    }

                    Ok(Self {
                        listener: DaemonListenerKind::Unix {
                            listener,
                            path: path.clone(),
                            guard,
                        },
                        local_addr: SocketAddr::from(([127, 0, 0, 1], 0)),
                        unix_path: Some(path),
                        dispatcher,
                        manager: None,
                    })
                }
            }
        }
    }

    /// Bind a listener that hosts a bounded, persistent multi-session control plane: each
    /// connection is routed to the runtime that owns the session named in each request.
    pub async fn bind_multi(listen_addr: &str, manager: SessionManager) -> Result<Self, String> {
        let mut daemon = Self::bind(listen_addr, manager.primary()).await?;
        daemon.manager = Some(manager);
        Ok(daemon)
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub fn unix_socket_path(&self) -> Option<&Path> {
        self.unix_path.as_deref()
    }

    /// Returns true if this daemon is bound to a local loopback interface or Unix domain socket.
    pub fn is_loopback(&self) -> bool {
        if self.unix_path.is_some() {
            return true;
        }
        match self.local_addr.ip() {
            IpAddr::V4(v4) => v4.is_loopback(),
            IpAddr::V6(v6) => v6.is_loopback(),
        }
    }

    /// If this daemon is bound to a non-loopback TCP address, returns the explicit security warning.
    /// The connection is plaintext (no TLS) and the auth token travels in JSON-RPC parameters,
    /// so it must only be used on a trusted network. The token value itself is deliberately not logged.
    pub fn non_loopback_warning(&self) -> Option<&'static str> {
        if self.is_loopback() {
            None
        } else {
            Some(NON_LOOPBACK_PLAINTEXT_WARNING)
        }
    }

    pub fn local_endpoint(&self) -> String {
        if let Some(ref p) = self.unix_path {
            format!("unix://{}", p.display())
        } else {
            self.local_addr.to_string()
        }
    }

    pub fn dispatcher(&self) -> &RpcDispatcher {
        &self.dispatcher
    }

    /// Accept and serve JSON-RPC 2.0 connections (TCP or Unix domain socket) until `cancel_rx`
    /// fires or `shutdown` RPC is invoked.
    pub async fn serve(self, mut cancel_rx: watch::Receiver<bool>) -> Result<(), String> {
        let conn_semaphore = Arc::new(Semaphore::new(MAX_DAEMON_CONNECTIONS));
        let Self {
            listener,
            dispatcher,
            manager,
            ..
        } = self;

        match listener {
            DaemonListenerKind::Tcp(tcp_listener) => loop {
                if *cancel_rx.borrow() || dispatcher.is_shutting_down() {
                    break;
                }
                tokio::select! {
                    accept_res = tcp_listener.accept() => {
                        match accept_res {
                            Ok((stream, _peer)) => {
                                let Ok(permit) = conn_semaphore.clone().try_acquire_owned() else {
                                    drop(stream);
                                    continue;
                                };
                                let disp = dispatcher.clone();
                                let mgr = manager.clone();
                                tokio::spawn(async move {
                                    let _permit = permit;
                                    let _ = Self::handle_tcp_stream(disp, mgr, stream).await;
                                });
                            }
                            Err(e) => {
                                return Err(format!("TCP accept error: {}", e));
                            }
                        }
                    }
                    _ = async {
                        loop {
                            if *cancel_rx.borrow() {
                                break;
                            }
                            if cancel_rx.changed().await.is_err() {
                                std::future::pending::<()>().await;
                            }
                        }
                    } => {
                        break;
                    }
                }
            },
            #[cfg(unix)]
            DaemonListenerKind::Unix {
                listener: unix_listener,
                path: _path,
                guard,
            } => {
                let mut res = Ok(());
                loop {
                    if *cancel_rx.borrow() || dispatcher.is_shutting_down() {
                        break;
                    }
                    tokio::select! {
                        accept_res = unix_listener.accept() => {
                            match accept_res {
                                Ok((stream, _peer)) => {
                                    let Ok(permit) = conn_semaphore.clone().try_acquire_owned() else {
                                        drop(stream);
                                        continue;
                                    };
                                    let disp = dispatcher.clone();
                                    let mgr = manager.clone();
                                    tokio::spawn(async move {
                                        let _permit = permit;
                                        let (r, w) = stream.into_split();
                                        match mgr {
                                            Some(manager) => {
                                                let _ = manager.serve_stream(r, w).await;
                                            }
                                            None => {
                                                let _ = disp.serve_stream(r, w).await;
                                            }
                                        }
                                    });
                                }
                                Err(e) => {
                                    res = Err(format!("Unix socket accept error: {}", e));
                                    break;
                                }
                            }
                        }
                        _ = async {
                            loop {
                                if *cancel_rx.borrow() {
                                    break;
                                }
                                if cancel_rx.changed().await.is_err() {
                                    std::future::pending::<()>().await;
                                }
                            }
                        } => {
                            break;
                        }
                    }
                }
                let _ = guard.unlink();
                return res;
            }
        }
        Ok(())
    }

    async fn handle_tcp_stream(
        dispatcher: RpcDispatcher,
        manager: Option<SessionManager>,
        stream: TcpStream,
    ) -> Result<(), String> {
        let _ = stream.set_nodelay(true);
        let (read_half, write_half) = stream.into_split();
        match manager {
            Some(manager) => manager.serve_stream(read_half, write_half).await,
            None => dispatcher.serve_stream(read_half, write_half).await,
        }
    }
}

// The daemon's socket surface is Unix-only (parent-dir policy, no-follow chmod, stale-socket
// recovery); Windows daemon transport is B15 territory.
#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::tool::policy::PolicyMode;
    use std::os::unix::fs::PermissionsExt;

    fn temp_test_root(label: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("kroma-daemon-{}-{}", label, uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn test_dispatcher(auth_token: Option<String>, root: &Path) -> RpcDispatcher {
        let ws = root.join("ws");
        let state = root.join("state");
        let _ = std::fs::create_dir_all(&ws);
        let _ = std::fs::create_dir_all(&state);
        let cfg = KromaRuntimeConfig::new(&ws, &state, PolicyMode::ReadOnly)
            .with_surface("serve", "tcp")
            .with_auth_token(auth_token);
        let provider = Arc::new(crate::provider::FakeProvider::new(vec![]));
        RpcDispatcher::new(cfg, provider).unwrap()
    }

    #[tokio::test]
    async fn test_daemon_non_loopback_warning_emitted_and_does_not_leak_token() {
        let root = temp_test_root("warning-test");
        let token = "test-secret-auth-token-xyz123";
        let disp_non_loopback = test_dispatcher(Some(token.to_string()), &root);

        // Binding to non-loopback (0.0.0.0:0) with token
        let daemon = KromaDaemon::bind("0.0.0.0:0", disp_non_loopback)
            .await
            .expect("should bind non-loopback with auth token");

        assert!(!daemon.is_loopback());
        let warning = daemon.non_loopback_warning();
        assert!(
            warning.is_some(),
            "non-loopback bind must emit explicit warning"
        );
        let warning_text = warning.unwrap();
        assert!(
            warning_text.contains("plaintext") && warning_text.contains("no TLS"),
            "warning must state connection is plaintext (no TLS)"
        );
        assert!(
            warning_text.contains("JSON-RPC parameters"),
            "warning must state auth token travels in JSON-RPC parameters"
        );
        assert!(
            warning_text.contains("trusted network"),
            "warning must state daemon must only be used on trusted network"
        );
        assert!(
            !warning_text.contains(token),
            "warning must NOT silently log the token value itself"
        );

        // Binding to loopback (127.0.0.1:0)
        let disp_loopback = test_dispatcher(None, &root);
        let loopback_daemon = KromaDaemon::bind("127.0.0.1:0", disp_loopback)
            .await
            .expect("should bind loopback");
        assert!(loopback_daemon.is_loopback());
        assert_eq!(
            loopback_daemon.non_loopback_warning(),
            None,
            "loopback bind must not emit non-loopback warning"
        );

        // Binding to Unix domain socket
        let sock_path = root.join("loopback-test.sock");
        let disp_unix = test_dispatcher(None, &root);
        let unix_daemon = KromaDaemon::bind(&format!("unix://{}", sock_path.display()), disp_unix)
            .await
            .expect("should bind unix domain socket");
        assert!(unix_daemon.is_loopback());
        assert_eq!(
            unix_daemon.non_loopback_warning(),
            None,
            "unix socket bind must not emit non-loopback warning"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn test_daemon_unix_socket_parent_dir_mode_is_0700() {
        let root = temp_test_root("parent-mode-test");

        // Case A: parent directory does not exist yet -> daemon creates it with mode 0700
        let new_parent = root.join("new_parent_dir");
        let sock_a = new_parent.join("daemon_a.sock");
        let disp_a = test_dispatcher(None, &root);

        assert!(!new_parent.exists());
        let daemon_a = KromaDaemon::bind(&format!("unix://{}", sock_a.display()), disp_a)
            .await
            .expect("should bind socket in new parent directory");

        assert!(new_parent.exists());
        let parent_mode = std::fs::symlink_metadata(&new_parent)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            parent_mode, 0o700,
            "created Unix socket parent directory must have mode 0700 (owner-only), got {:o}",
            parent_mode
        );

        let socket_mode = std::fs::symlink_metadata(&sock_a)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            socket_mode, 0o600,
            "Unix domain socket must have mode 0600, got {:o}",
            socket_mode
        );
        drop(daemon_a);

        // Case B: a pre-existing parent directory is never modified (0755 stays 0755), so a shared
        // anchor such as `/run` is never tightened by a root bind.
        let existing_parent = root.join("existing_parent_dir");
        std::fs::create_dir(&existing_parent).unwrap();
        std::fs::set_permissions(&existing_parent, std::fs::Permissions::from_mode(0o755)).unwrap();

        let initial_mode = std::fs::symlink_metadata(&existing_parent)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(initial_mode, 0o755);

        let sock_b = existing_parent.join("daemon_b.sock");
        let disp_b = test_dispatcher(None, &root);
        let daemon_b = KromaDaemon::bind(&format!("unix://{}", sock_b.display()), disp_b)
            .await
            .expect("should bind socket in existing parent directory");

        let restricted_mode = std::fs::symlink_metadata(&existing_parent)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            restricted_mode, 0o755,
            "a pre-existing Unix socket parent directory must be left untouched, got {:o}",
            restricted_mode
        );

        let socket_b_mode = std::fs::symlink_metadata(&sock_b)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            socket_b_mode, 0o600,
            "Unix domain socket must have mode 0600, got {:o}",
            socket_b_mode
        );
        drop(daemon_b);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn test_daemon_unix_socket_shared_sticky_parent_left_untouched() {
        let root = temp_test_root("sticky-parent-test");

        // A `/tmp`-style sticky shared directory must be left exactly as it is: other users keep
        // working, and the socket's own 0600 mode is the protection.
        let shared = root.join("shared_sticky");
        std::fs::create_dir(&shared).unwrap();
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o1777)).unwrap();

        let sock = shared.join("daemon.sock");
        let disp = test_dispatcher(None, &root);
        let daemon = KromaDaemon::bind(&format!("unix://{}", sock.display()), disp)
            .await
            .expect("bind in a sticky shared parent directory must succeed");

        let parent_mode = std::fs::symlink_metadata(&shared)
            .unwrap()
            .permissions()
            .mode()
            & 0o7777;
        assert_eq!(
            parent_mode, 0o1777,
            "sticky shared parent directory must be left untouched, got {:o}",
            parent_mode
        );
        let socket_mode = std::fs::symlink_metadata(&sock)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            socket_mode, 0o600,
            "Unix domain socket must still be created 0600 in a shared parent, got {:o}",
            socket_mode
        );
        drop(daemon);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn test_unix_socket_parent_policy_never_tightens_or_binds_shared_directories() {
        // A directory the daemon created itself -> restrict to 0700.
        assert_eq!(
            unix_socket_parent_policy(true, 1000, 0o755, 1000),
            UnixSocketParentPolicy::RestrictOwnerOnly
        );
        assert_eq!(
            unix_socket_parent_policy(true, 0, 0o755, 1000),
            UnixSocketParentPolicy::RestrictOwnerOnly
        );
        // A pre-existing directory is never tightened, whoever owns it.
        assert_eq!(
            unix_socket_parent_policy(false, 1000, 0o755, 1000),
            UnixSocketParentPolicy::LeaveShared
        );
        // A pre-existing directory owned by current user but group/world-writable without sticky
        // bit MUST be refused (F10: another user could unlink/replace our socket).
        assert_eq!(
            unix_socket_parent_policy(false, 1000, 0o777, 1000),
            UnixSocketParentPolicy::RefuseSharedWritable
        );
        assert_eq!(
            unix_socket_parent_policy(false, 1000, 0o770, 1000),
            UnixSocketParentPolicy::RefuseSharedWritable
        );
        // Sticky (e.g. `/tmp`), ours (euid) or root (uid 0) -> leave untouched.
        assert_eq!(
            unix_socket_parent_policy(false, 1000, 0o1777, 1000),
            UnixSocketParentPolicy::LeaveShared
        );
        assert_eq!(
            unix_socket_parent_policy(false, 0, 0o1777, 1000),
            UnixSocketParentPolicy::LeaveShared
        );
        // Third-party sticky directory (owner != 0 && owner != euid) MUST be refused:
        // directory owner can unlink/rename files even with sticky bit!
        assert_eq!(
            unix_socket_parent_policy(false, 9999, 0o1777, 1000),
            UnixSocketParentPolicy::RefuseSharedWritable
        );
        // Another user's directory we cannot write (root-owned `/run`, uid 0) -> leave untouched.
        assert_eq!(
            unix_socket_parent_policy(false, 0, 0o755, 1000),
            UnixSocketParentPolicy::LeaveShared
        );
        // Third-party non-sticky directory (uid != 0 && uid != euid) -> refuse (owner could chmod/rename).
        assert_eq!(
            unix_socket_parent_policy(false, 9999, 0o755, 1000),
            UnixSocketParentPolicy::RefuseSharedWritable
        );
        // Another user's world-writable non-sticky directory -> refuse: it could replace the socket.
        assert_eq!(
            unix_socket_parent_policy(false, 0, 0o777, 1000),
            UnixSocketParentPolicy::RefuseSharedWritable
        );
    }

    #[tokio::test]
    async fn test_daemon_unix_socket_symlinked_parent_refused() {
        let root = temp_test_root("symlinked-parent-test");

        // Case A: symlinked parent directory pointing to a valid real directory
        let real_dir = root.join("real_target_dir");
        std::fs::create_dir(&real_dir).unwrap();

        let sym_dir = root.join("symlinked_parent");
        std::os::unix::fs::symlink(&real_dir, &sym_dir).unwrap();

        let sock_path = sym_dir.join("daemon.sock");
        let disp = test_dispatcher(None, &root);

        let res = KromaDaemon::bind(&format!("unix://{}", sock_path.display()), disp).await;
        assert!(res.is_err(), "bind in symlinked parent directory must fail");
        let err = res.err().unwrap();
        assert!(
            err.contains("symlink"),
            "error message must mention symlink refusal, got: {}",
            err
        );
        assert!(
            !sock_path.exists(),
            "socket file must not have been created"
        );
        assert!(
            !real_dir.join("daemon.sock").exists(),
            "socket file must not have been created in target directory"
        );

        // Case B: broken symlinked parent directory pointing to non-existent target
        let broken_sym_dir = root.join("broken_symlinked_parent");
        std::os::unix::fs::symlink(root.join("nonexistent_target"), &broken_sym_dir).unwrap();

        let broken_sock_path = broken_sym_dir.join("daemon.sock");
        let disp_broken = test_dispatcher(None, &root);

        let res_broken = KromaDaemon::bind(
            &format!("unix://{}", broken_sock_path.display()),
            disp_broken,
        )
        .await;
        assert!(
            res_broken.is_err(),
            "bind in broken symlinked parent directory must fail"
        );
        let err_broken = res_broken.err().unwrap();
        assert!(
            err_broken.contains("symlink"),
            "error message must mention symlink refusal, got: {}",
            err_broken
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn test_daemon_unix_socket_regular_file_not_overwritten_bytes_preserved() {
        let root = temp_test_root("reg");
        let file_path = root.join("operator-data.bin");
        let original_bytes = b"operator-data-critical-state-must-not-be-deleted";
        std::fs::write(&file_path, original_bytes).unwrap();

        let disp = test_dispatcher(None, &root);
        let res = KromaDaemon::bind(&format!("unix://{}", file_path.display()), disp).await;

        assert!(
            res.is_err(),
            "binding Unix socket to existing regular file path must fail"
        );
        assert!(
            file_path.exists(),
            "regular file must not have been removed"
        );
        let meta = std::fs::symlink_metadata(&file_path).unwrap();
        assert!(meta.is_file(), "path must remain a regular file");
        assert_eq!(
            std::fs::read(&file_path).unwrap(),
            original_bytes,
            "regular file content/bytes must be completely preserved"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn test_daemon_unix_socket_symlink_not_overwritten_target_preserved() {
        let root = temp_test_root("sym");
        let target_path = root.join("target.txt");
        let target_bytes = b"preserve-target-content";
        std::fs::write(&target_path, target_bytes).unwrap();

        let symlink_path = root.join("symlink.sock");
        std::os::unix::fs::symlink(&target_path, &symlink_path).unwrap();

        let disp = test_dispatcher(None, &root);
        let res = KromaDaemon::bind(&format!("unix://{}", symlink_path.display()), disp).await;

        assert!(
            res.is_err(),
            "binding Unix socket to existing symlink path must fail"
        );
        let sym_meta = std::fs::symlink_metadata(&symlink_path).unwrap();
        assert!(
            sym_meta.file_type().is_symlink(),
            "path must remain a symlink without being unlinked or followed"
        );
        assert_eq!(
            std::fs::read_link(&symlink_path).unwrap(),
            target_path,
            "symlink target must remain unchanged"
        );
        assert_eq!(
            std::fs::read(&target_path).unwrap(),
            target_bytes,
            "target file content must remain unchanged"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn test_daemon_unix_socket_active_socket_not_preempted() {
        let root = temp_test_root("act");
        let priv_dir = root.join("priv");
        std::fs::create_dir(&priv_dir).unwrap();
        std::fs::set_permissions(&priv_dir, std::fs::Permissions::from_mode(0o700)).unwrap();

        let sock_path = priv_dir.join("active.sock");
        let disp1 = test_dispatcher(None, &root);
        let daemon1 = KromaDaemon::bind(&format!("unix://{}", sock_path.display()), disp1)
            .await
            .expect("first daemon bind must succeed");

        let disp2 = test_dispatcher(None, &root);
        let res = KromaDaemon::bind(&format!("unix://{}", sock_path.display()), disp2).await;
        assert!(
            res.is_err(),
            "second bind to an active listening socket must fail and not preempt it"
        );

        // Verify first daemon socket is still functional
        let probe = tokio::net::UnixStream::connect(&sock_path).await;
        assert!(
            probe.is_ok(),
            "active socket must still be listening and accepting connections"
        );

        drop(daemon1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn test_daemon_unix_socket_controlled_stale_socket_recovered() {
        let root = temp_test_root("cst");
        let priv_dir = root.join("priv");
        std::fs::create_dir(&priv_dir).unwrap();
        std::fs::set_permissions(&priv_dir, std::fs::Permissions::from_mode(0o700)).unwrap();

        let sock_path = priv_dir.join("stale.sock");
        // Create an un-served stale socket by binding and dropping a std UnixListener without unlinking
        {
            let std_listener = std::os::unix::net::UnixListener::bind(&sock_path).unwrap();
            drop(std_listener);
        }
        assert!(sock_path.exists());
        // Verify socket is not listening (connection refused)
        let probe = tokio::net::UnixStream::connect(&sock_path).await;
        assert!(
            probe.is_err(),
            "stale socket must not have an active listener"
        );

        let disp = test_dispatcher(None, &root);
        let daemon = KromaDaemon::bind(&format!("unix://{}", sock_path.display()), disp)
            .await
            .expect("stale socket in controlled private directory must be safely recovered");

        // Verify recovered daemon is active
        let stream = tokio::net::UnixStream::connect(&sock_path).await;
        assert!(
            stream.is_ok(),
            "recovered daemon socket must be actively accepting connections"
        );

        drop(daemon);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn test_daemon_unix_socket_drop_does_not_delete_replaced_path() {
        use std::os::unix::fs::{FileTypeExt, MetadataExt};
        let root = temp_test_root("drp");
        let priv_dir = root.join("priv");
        std::fs::create_dir(&priv_dir).unwrap();
        std::fs::set_permissions(&priv_dir, std::fs::Permissions::from_mode(0o700)).unwrap();

        // Sub-case 1: replaced by regular sensitive file
        let sock_path = priv_dir.join("d1.sock");
        let disp = test_dispatcher(None, &root);
        let daemon1 = KromaDaemon::bind(&format!("unix://{}", sock_path.display()), disp)
            .await
            .expect("bind must succeed");
        assert!(sock_path.exists());

        std::fs::remove_file(&sock_path).unwrap();
        let replacement_bytes = b"important-operator-configuration-do-not-delete";
        std::fs::write(&sock_path, replacement_bytes).unwrap();

        // Dropping daemon1 must NOT unlink the replacement regular file
        drop(daemon1);

        assert!(
            sock_path.exists(),
            "drop must not unlink a path replaced by a regular file"
        );
        let meta = std::fs::symlink_metadata(&sock_path).unwrap();
        assert!(
            meta.is_file(),
            "replacement path must still be a regular file"
        );
        assert_eq!(
            std::fs::read(&sock_path).unwrap(),
            replacement_bytes,
            "replacement file content must be preserved"
        );
        std::fs::remove_file(&sock_path).unwrap();

        // Sub-case 2: replaced by another Unix socket with different inode
        let sock_path2 = priv_dir.join("d2.sock");
        let disp2 = test_dispatcher(None, &root);
        let daemon2 = KromaDaemon::bind(&format!("unix://{}", sock_path2.display()), disp2)
            .await
            .expect("bind must succeed");
        let orig_inode = std::fs::symlink_metadata(&sock_path2).unwrap().ino();

        std::fs::remove_file(&sock_path2).unwrap();
        let replacement_listener = std::os::unix::net::UnixListener::bind(&sock_path2).unwrap();
        let repl_meta = std::fs::symlink_metadata(&sock_path2).unwrap();
        assert!(repl_meta.file_type().is_socket());
        let new_inode = repl_meta.ino();
        assert_ne!(
            orig_inode, new_inode,
            "new socket must have a distinct inode"
        );

        // Dropping daemon2 must NOT unlink replacement_listener's socket because dev/inode mismatch
        drop(daemon2);

        assert!(
            sock_path2.exists(),
            "drop must not unlink a socket path replaced by another socket with different inode"
        );
        assert_eq!(
            std::fs::symlink_metadata(&sock_path2).unwrap().ino(),
            new_inode,
            "replacement socket inode must still be preserved"
        );
        drop(replacement_listener);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn test_daemon_unix_socket_shared_parent_mode_unchanged_and_shared_writable_refused() {
        let root = temp_test_root("sp");

        // 1. Non-sticky 0777 directory owned by current user must be refused
        let shared_0777 = root.join("s_777");
        std::fs::create_dir(&shared_0777).unwrap();
        std::fs::set_permissions(&shared_0777, std::fs::Permissions::from_mode(0o777)).unwrap();

        let sock_0777 = shared_0777.join("d.sock");
        let disp1 = test_dispatcher(None, &root);
        let res_0777 = KromaDaemon::bind(&format!("unix://{}", sock_0777.display()), disp1).await;
        assert!(
            res_0777.is_err(),
            "bind in non-sticky 0777 directory must be refused even if owned by current user"
        );
        let mode_0777 = std::fs::symlink_metadata(&shared_0777)
            .unwrap()
            .permissions()
            .mode()
            & 0o7777;
        assert_eq!(
            mode_0777, 0o777,
            "refused shared writable directory mode must not be modified"
        );

        // 2. Non-sticky 0770 directory owned by current user must be refused
        let shared_0770 = root.join("s_770");
        std::fs::create_dir(&shared_0770).unwrap();
        std::fs::set_permissions(&shared_0770, std::fs::Permissions::from_mode(0o770)).unwrap();

        let sock_0770 = shared_0770.join("d.sock");
        let disp2 = test_dispatcher(None, &root);
        let res_0770 = KromaDaemon::bind(&format!("unix://{}", sock_0770.display()), disp2).await;
        assert!(
            res_0770.is_err(),
            "bind in non-sticky 0770 directory must be refused"
        );
        let mode_0770 = std::fs::symlink_metadata(&shared_0770)
            .unwrap()
            .permissions()
            .mode()
            & 0o7777;
        assert_eq!(
            mode_0770, 0o770,
            "refused shared writable directory mode must not be modified"
        );

        // 3. Shared sticky directory (like /tmp, 01777): bind succeeds when socket does not exist,
        // but existing stale socket in shared sticky directory is NOT recovered (fail-closed)
        let shared_sticky = root.join("s_stk");
        std::fs::create_dir(&shared_sticky).unwrap();
        std::fs::set_permissions(&shared_sticky, std::fs::Permissions::from_mode(0o1777)).unwrap();

        let stale_shared = shared_sticky.join("st.sock");
        {
            let std_listener = std::os::unix::net::UnixListener::bind(&stale_shared).unwrap();
            drop(std_listener);
        }
        let disp3 = test_dispatcher(None, &root);
        let res_stale_shared =
            KromaDaemon::bind(&format!("unix://{}", stale_shared.display()), disp3).await;
        assert!(
            res_stale_shared.is_err(),
            "stale socket in non-private shared directory must not be removed"
        );
        let sticky_mode = std::fs::symlink_metadata(&shared_sticky)
            .unwrap()
            .permissions()
            .mode()
            & 0o7777;
        assert_eq!(
            sticky_mode, 0o1777,
            "shared sticky parent directory mode must not be modified"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn test_daemon_unix_socket_normal_drop_cleans_up_socket() {
        let root = temp_test_root("dcl");
        let priv_dir = root.join("priv");
        std::fs::create_dir(&priv_dir).unwrap();
        std::fs::set_permissions(&priv_dir, std::fs::Permissions::from_mode(0o700)).unwrap();

        let sock_path = priv_dir.join("daemon.sock");
        let disp = test_dispatcher(None, &root);
        let daemon = KromaDaemon::bind(&format!("unix://{}", sock_path.display()), disp)
            .await
            .expect("bind must succeed");
        assert!(
            sock_path.exists(),
            "socket must exist while daemon is alive"
        );

        drop(daemon);

        assert!(
            !sock_path.exists(),
            "normal drop must clean up its bound socket file"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn test_daemon_unix_socket_bind_chmod_failure_cleans_up_socket() {
        let root = temp_test_root("bfail");
        let priv_dir = root.join("priv");
        std::fs::create_dir(&priv_dir).unwrap();
        std::fs::set_permissions(&priv_dir, std::fs::Permissions::from_mode(0o700)).unwrap();

        let sock_path = priv_dir.join("chmod_fail.sock");
        let disp = test_dispatcher(None, &root);

        // Arm the fault guard to simulate chmod failure on the newly bound socket
        let _guard = SocketChmodFaultGuard::arm();

        let res = KromaDaemon::bind(&format!("unix://{}", sock_path.display()), disp).await;
        assert!(
            res.is_err(),
            "bind must fail when socket permissions cannot be restricted"
        );
        let err = res.err().unwrap();
        assert!(
            err.contains("failed to set 0600 permissions"),
            "error message must describe chmod failure, got: {}",
            err
        );
        assert!(
            !sock_path.exists(),
            "socket file must NOT be orphaned on disk when chmod fails"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn test_daemon_unix_socket_symlinked_ancestor_refused() {
        let root = temp_test_root("anc-sym");
        let real_base = root.join("real_base");
        std::fs::create_dir(&real_base).unwrap();

        let sym_base = root.join("sym_base");
        std::os::unix::fs::symlink(&real_base, &sym_base).unwrap();

        let child_dir = sym_base.join("child");
        let sock_path = child_dir.join("daemon.sock");
        let disp = test_dispatcher(None, &root);

        let res = KromaDaemon::bind(&format!("unix://{}", sock_path.display()), disp).await;
        assert!(
            res.is_err(),
            "bind under symlinked ancestor directory must fail"
        );
        let err = res.err().unwrap();
        assert!(
            err.contains("symlink"),
            "error message must describe symlink ancestor refusal, got: {}",
            err
        );
        assert!(
            !sock_path.exists(),
            "socket file must not have been created"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn test_daemon_unix_socket_uncreated_parent_ancestor_symlink_not_created() {
        let root = temp_test_root("anc-not-created");
        let target_dir = root.join("sensitive_target");
        std::fs::create_dir(&target_dir).unwrap();
        std::fs::set_permissions(&target_dir, std::fs::Permissions::from_mode(0o755)).unwrap();

        let symlink_trap = root.join("symlink_trap");
        std::os::unix::fs::symlink(&target_dir, &symlink_trap).unwrap();

        let new_dir = symlink_trap.join("untrusted_new_dir");
        let sock_path = new_dir.join("daemon.sock");
        let disp = test_dispatcher(None, &root);

        let res = KromaDaemon::bind(&format!("unix://{}", sock_path.display()), disp).await;
        assert!(res.is_err(), "bind under symlinked ancestor must fail");

        // The critical security assertion:
        // New directory must NOT have been created inside the target directory!
        assert!(
            !target_dir.join("untrusted_new_dir").exists(),
            "untrusted new_dir must NOT be created before ancestor validation"
        );
        assert!(!new_dir.exists(), "new_dir must completely not exist");
        let target_mode = std::fs::symlink_metadata(&target_dir)
            .unwrap()
            .permissions()
            .mode()
            & 0o7777;
        assert_eq!(
            target_mode, 0o755,
            "target directory permissions must remain completely unchanged"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn test_daemon_unix_socket_stale_replacement_before_unlink_refused() {
        let root = temp_test_root("stale-replace");
        let priv_dir = root.join("priv");
        std::fs::create_dir(&priv_dir).unwrap();
        std::fs::set_permissions(&priv_dir, std::fs::Permissions::from_mode(0o700)).unwrap();

        let sock_path = priv_dir.join("stale.sock");
        // Create an un-served stale socket by binding and dropping a std UnixListener
        {
            let std_listener = std::os::unix::net::UnixListener::bind(&sock_path).unwrap();
            drop(std_listener);
        }
        assert!(sock_path.exists());

        let replacement_bytes = b"sensitive-data-planted-during-liveliness-probe";

        // Arm hook: right after liveliness probe and before unlink, replace stale socket with a regular file
        let path_clone = sock_path.clone();
        let _guard = StaleProbeHookGuard::set(move |_| {
            let _ = std::fs::remove_file(&path_clone);
            std::fs::write(&path_clone, replacement_bytes).unwrap();
        });

        let disp = test_dispatcher(None, &root);
        let res = KromaDaemon::bind(&format!("unix://{}", sock_path.display()), disp).await;
        assert!(
            res.is_err(),
            "stale socket recovery must be aborted if socket identity changed before unlink"
        );
        let err = res.err().unwrap();
        assert!(
            err.contains("socket identity changed during liveliness probe"),
            "error message must describe identity mismatch during liveliness probe, got: {}",
            err
        );
        assert!(
            sock_path.exists(),
            "replacement file must NOT have been unlinked"
        );
        assert_eq!(
            std::fs::read(&sock_path).unwrap(),
            replacement_bytes,
            "replacement file bytes must be preserved"
        );

        let _ = std::fs::remove_dir_all(&root);
    }
}
