//! daemon surface of the multi-surface runtime (split from `surface.rs`).

use super::*;

pub(crate) fn looks_like_http_request(trimmed: &str) -> bool {
    const HTTP_PREFIXES: &[&str] = &[
        "GET ",
        "POST ",
        "PUT ",
        "DELETE ",
        "OPTIONS ",
        "HEAD ",
        "CONNECT ",
        "Host:",
        "Origin:",
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
            let s = String::from_utf8(buf)
                .map_err(|_| "frame is not valid UTF-8".to_string())?;
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
            let s = String::from_utf8(buf)
                .map_err(|_| "frame is not valid UTF-8".to_string())?;
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
    },
}

/// Local TCP or Unix Domain Socket JSON-RPC 2.0 daemon (`kroma serve`).
pub struct KromaDaemon {
    listener: DaemonListenerKind,
    local_addr: SocketAddr,
    unix_path: Option<PathBuf>,
    dispatcher: RpcDispatcher,
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
                    use std::os::unix::fs::PermissionsExt;
                    if let Some(parent) = path.parent() {
                        if !parent.as_os_str().is_empty() {
                            std::fs::create_dir_all(parent).map_err(|e| {
                                format!(
                                    "failed to create parent directory for Unix socket {}: {}",
                                    path.display(),
                                    e
                                )
                            })?;
                        }
                    }
                    if path.exists() {
                        let meta = std::fs::symlink_metadata(&path).map_err(|e| {
                            format!("failed to inspect existing path {}: {}", path.display(), e)
                        })?;
                        if meta.is_dir() {
                            return Err(format!(
                                "Unix socket path {} is a directory",
                                path.display()
                            ));
                        }
                        let _ = std::fs::remove_file(&path);
                    }
                    let listener = tokio::net::UnixListener::bind(&path).map_err(|e| {
                        format!(
                            "failed to bind Unix domain socket on {}: {}",
                            path.display(),
                            e
                        )
                    })?;
                    // A Unix socket cannot be `fchmod`-ed: `open` on a socket path fails `ENXIO`,
                    // `O_PATH` descriptors reject `fchmod` with `EBADF`, and `fchmod` on the bound
                    // socket fd is a silent no-op on Linux. Setting the process-global `umask`
                    // around `bind` would race every other thread, so the mode must be set by path.
                    // The parent directory is created first and the error is propagated (not
                    // swallowed), so a failed or redirected chmod surfaces instead of silently
                    // leaving the socket world-accessible.
                    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                        .map_err(|e| {
                            format!(
                                "failed to set 0600 permissions on Unix socket {}: {}",
                                path.display(),
                                e
                            )
                        })?;
                    Ok(Self {
                        listener: DaemonListenerKind::Unix {
                            listener,
                            path: path.clone(),
                        },
                        local_addr: SocketAddr::from(([127, 0, 0, 1], 0)),
                        unix_path: Some(path),
                        dispatcher,
                    })
                }
            }
        }
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub fn unix_socket_path(&self) -> Option<&Path> {
        self.unix_path.as_deref()
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
                                tokio::spawn(async move {
                                    let _permit = permit;
                                    let _ = Self::handle_tcp_stream(disp, stream).await;
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
                path,
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
                                    tokio::spawn(async move {
                                        let _permit = permit;
                                        let (r, w) = stream.into_split();
                                        let _ = disp.serve_stream(r, w).await;
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
                let _ = std::fs::remove_file(&path);
                return res;
            }
        }
        Ok(())
    }

    async fn handle_tcp_stream(dispatcher: RpcDispatcher, stream: TcpStream) -> Result<(), String> {
        let _ = stream.set_nodelay(true);
        let (read_half, write_half) = stream.into_split();
        dispatcher.serve_stream(read_half, write_half).await
    }
}
