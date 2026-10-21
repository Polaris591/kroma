//! Shared HTTP/1.1 + TLS transport primitives.
//!
//! Gate 7 (`v1.2.0`) introduced two additional wire protocols (Anthropic Messages and OpenAI
//! Responses) alongside the original Chat Completions client. Rather than fork the hand-written
//! TLS stack three times, every protocol client in this crate shares the connection, request-write
//! and body-framing machinery below.
//!
//! Failure classification is deliberately explicit: callers must be able to distinguish a
//! *transient* failure (safe to retry) from a *fatal* one (malformed peer, size violation) and from
//! a mid-stream failure whose retryability depends on whether business data was already observed.

use crate::budget::BudgetConfig;
use crate::provider::endpoint::ValidatedEndpoint;
use crate::provider::ProviderError;
#[cfg(all(unix, not(target_os = "macos")))]
use std::ffi::CString;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use tokio::io::{
    AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader, ReadBuf,
};
use tokio::net::TcpStream;

pub(crate) const MAX_HTTP_HEADER_LINE_BYTES: usize = 16 * 1024;
pub(crate) const MAX_HTTP_HEADERS_COUNT: usize = 128;
pub(crate) const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;

/// F-05: `KROMA_INSECURE_TLS=1` is a dangerous debugging escape hatch. Warn exactly once so the
/// operator cannot miss it, instead of silently accepting unverified certificates.
pub(crate) static INSECURE_TLS_WARNED: std::sync::Once = std::sync::Once::new();

/// A failure raised while establishing or writing a request.
pub(crate) enum TransportFailure {
    /// Connect/TLS/write/idle-timeout. Safe to retry.
    Transient(ProviderError),
    /// The peer sent something structurally invalid. Never retried.
    Fatal(ProviderError),
}

/// A failure raised while reading a response body.
pub(crate) enum BodyFailure {
    /// Framing or size violation. Never retried.
    Fatal(ProviderError),
    /// I/O or timeout. Retryability is decided by the caller (mid-stream failures are fatal once
    /// business data has been observed).
    Stream(ProviderError),
}

/// Whether a body consumer wants more bytes.
///
/// `Stop` is how a caller that already has what it needs (an SSE terminal response) releases the
/// connection instead of waiting for the server to close it. It is not an error: the body is simply
/// no longer read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Flow {
    Continue,
    Stop,
}

// ---------------------------------------------------------------------------------------------
// TLS
// ---------------------------------------------------------------------------------------------

#[cfg(all(unix, not(target_os = "macos")))]
use std::ffi::{c_char, c_int, c_long, c_void};
#[cfg(all(unix, not(target_os = "macos")))]
use std::os::unix::io::AsRawFd;
#[cfg(all(unix, not(target_os = "macos")))]
use tokio::io::unix::AsyncFd;

#[cfg(all(unix, not(target_os = "macos")))]
#[link(name = "ssl")]
#[link(name = "crypto")]
extern "C" {
    fn TLS_client_method() -> *const c_void;
    fn SSL_CTX_new(method: *const c_void) -> *mut c_void;
    fn SSL_CTX_free(ctx: *mut c_void);
    fn SSL_CTX_set_default_verify_paths(ctx: *mut c_void) -> c_int;
    fn SSL_CTX_load_verify_locations(
        ctx: *mut c_void,
        ca_file: *const c_char,
        ca_path: *const c_char,
    ) -> c_int;
    fn SSL_new(ctx: *mut c_void) -> *mut c_void;
    fn SSL_free(ssl: *mut c_void);
    fn SSL_set_fd(ssl: *mut c_void, fd: c_int) -> c_int;
    fn SSL_set_verify(
        ssl: *mut c_void,
        mode: c_int,
        cb: Option<extern "C" fn(c_int, *mut c_void) -> c_int>,
    );
    fn SSL_ctrl(ssl: *mut c_void, cmd: c_int, larg: c_long, parg: *mut c_void) -> c_long;
    fn SSL_set1_host(ssl: *mut c_void, hostname: *const c_char) -> c_int;
    fn SSL_get_verify_result(ssl: *const c_void) -> c_long;
    fn SSL_connect(ssl: *mut c_void) -> c_int;
    fn SSL_read(ssl: *mut c_void, buf: *mut c_void, num: c_int) -> c_int;
    fn SSL_write(ssl: *mut c_void, buf: *const c_void, num: c_int) -> c_int;
    fn SSL_shutdown(ssl: *mut c_void) -> c_int;
    fn SSL_get_error(ssl: *const c_void, ret_code: c_int) -> c_int;
    /// Clear the calling thread's OpenSSL error queue before an I/O call, so `SSL_get_error`
    /// classifies *this* call rather than a stale error left by an earlier one.
    fn ERR_clear_error();
}

#[cfg(all(unix, not(target_os = "macos")))]
const SSL_VERIFY_NONE: c_int = 0x00;
#[cfg(all(unix, not(target_os = "macos")))]
const SSL_VERIFY_PEER: c_int = 0x01;
#[cfg(all(unix, not(target_os = "macos")))]
const X509_V_OK: c_long = 0;
#[cfg(all(unix, not(target_os = "macos")))]
const SSL_CTRL_SET_TLSEXT_HOSTNAME: c_int = 55;
#[cfg(all(unix, not(target_os = "macos")))]
const TLSEXT_NAMETYPE_HOST_NAME: c_long = 0;
#[cfg(all(unix, not(target_os = "macos")))]
const SSL_ERROR_WANT_READ: c_int = 2;
#[cfg(all(unix, not(target_os = "macos")))]
const SSL_ERROR_WANT_WRITE: c_int = 3;
#[cfg(all(unix, not(target_os = "macos")))]
const SSL_ERROR_SYSCALL: c_int = 5;
#[cfg(all(unix, not(target_os = "macos")))]
const SSL_ERROR_ZERO_RETURN: c_int = 6;

#[cfg(all(unix, not(target_os = "macos")))]
pub(crate) struct OpenSslStream {
    ssl: *mut c_void,
    async_fd: AsyncFd<std::net::TcpStream>,
}

/// Process-lifetime holder for the shared client `SSL_CTX`. `*mut c_void` is neither `Send` nor
/// `Sync`, so it is wrapped to make the `OnceLock` usable from a `static`.
#[cfg(all(unix, not(target_os = "macos")))]
struct SharedSslCtx(*mut c_void);
#[cfg(all(unix, not(target_os = "macos")))]
unsafe impl Send for SharedSslCtx {}
#[cfg(all(unix, not(target_os = "macos")))]
unsafe impl Sync for SharedSslCtx {}

/// Build (once) and return the shared client `SSL_CTX` with the default and platform CA bundles
/// loaded.
///
/// The context is immutable after creation — per-connection verification mode is set on the `SSL`,
/// not the context — so it is safe to share across threads: OpenSSL 1.1+ permits `SSL_new` from a
/// shared `SSL_CTX`. It is intentionally never freed: it lives for the whole process, which also
/// means every `SSL` created from it always outlives its parent context.
#[cfg(all(unix, not(target_os = "macos")))]
fn shared_client_ssl_ctx() -> io::Result<*mut c_void> {
    static CTX: std::sync::OnceLock<SharedSslCtx> = std::sync::OnceLock::new();
    if let Some(ctx) = CTX.get() {
        return Ok(ctx.0);
    }
    let created = unsafe {
        let method = TLS_client_method();
        if method.is_null() {
            return Err(io::Error::other("TLS_client_method returned null"));
        }
        let ctx = SSL_CTX_new(method);
        if ctx.is_null() {
            return Err(io::Error::other("SSL_CTX_new returned null"));
        }
        let _ = SSL_CTX_set_default_verify_paths(ctx);
        if let Ok(ca_bundle) = std::env::var("KROMA_CA_BUNDLE") {
            if let Ok(c_ca) = CString::new(ca_bundle) {
                let _ = SSL_CTX_load_verify_locations(ctx, c_ca.as_ptr(), std::ptr::null());
            }
        } else {
            const KNOWN_CA_FILES: &[&str] = &[
                "/data/data/com.termux/files/usr/etc/tls/cert.pem",
                "/data/data/com.termux/files/usr/etc/ssl/cert.pem",
                "/etc/ssl/certs/ca-certificates.crt",
                "/etc/pki/tls/certs/ca-bundle.crt",
                "/etc/ssl/ca-bundle.pem",
                "/etc/pki/tls/cacert.pem",
                "/etc/ssl/cert.pem",
            ];
            for file_path in KNOWN_CA_FILES {
                if std::path::Path::new(file_path).is_file() {
                    if let Ok(c_path) = CString::new(*file_path) {
                        if SSL_CTX_load_verify_locations(ctx, c_path.as_ptr(), std::ptr::null())
                            == 1
                        {
                            break;
                        }
                    }
                }
            }
            const KNOWN_CA_DIRS: &[&str] = &["/system/etc/security/cacerts", "/etc/ssl/certs"];
            for dir_path in KNOWN_CA_DIRS {
                if std::path::Path::new(dir_path).is_dir() {
                    if let Ok(c_path) = CString::new(*dir_path) {
                        let _ =
                            SSL_CTX_load_verify_locations(ctx, std::ptr::null(), c_path.as_ptr());
                    }
                }
            }
        }
        ctx
    };
    if CTX.set(SharedSslCtx(created)).is_ok() {
        Ok(created)
    } else {
        // Lost the initialization race: free the duplicate and reuse the winner.
        unsafe { SSL_CTX_free(created) };
        Ok(CTX
            .get()
            .expect("OnceLock::set failed, so another thread must have won the race")
            .0)
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
// Safety: Each OpenSslStream owns a dedicated SSL* accessed exclusively by &mut self. The shared
// SSL_CTX is immutable after creation and lives for the whole process.
unsafe impl Send for OpenSslStream {}
#[cfg(all(unix, not(target_os = "macos")))]
unsafe impl Sync for OpenSslStream {}

#[cfg(all(unix, not(target_os = "macos")))]
impl Drop for OpenSslStream {
    fn drop(&mut self) {
        unsafe {
            if !self.ssl.is_null() {
                let _ = SSL_shutdown(self.ssl);
                SSL_free(self.ssl);
            }
        }
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
impl OpenSslStream {
    async fn connect(tcp: TcpStream, host: &str) -> io::Result<Self> {
        let std_tcp = tcp.into_std()?;
        std_tcp.set_nonblocking(true)?;
        let async_fd = AsyncFd::new(std_tcp)?;

        let clean_host = host.trim_start_matches('[').trim_end_matches(']');
        let c_host = CString::new(clean_host)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid TLS hostname"))?;

        let verify_mode = if std::env::var("KROMA_INSECURE_TLS").as_deref() == Ok("1") {
            INSECURE_TLS_WARNED.call_once(|| {
                eprintln!(
                    "[SECURITY WARNING] KROMA_INSECURE_TLS=1 is enabled: TLS certificate verification is DISABLED. Never use this outside local debugging."
                );
            });
            SSL_VERIFY_NONE
        } else {
            SSL_VERIFY_PEER
        };

        // Reuse the process-wide context; only the CA bundles are loaded there (never the verify
        // mode), so per-connection settings stay on this SSL.
        let ctx = shared_client_ssl_ctx()?;
        let ssl = unsafe {
            let ssl = SSL_new(ctx);
            if ssl.is_null() {
                return Err(io::Error::other("SSL_new returned null"));
            }
            SSL_set_verify(ssl, verify_mode, None);
            if SSL_set_fd(ssl, async_fd.get_ref().as_raw_fd()) != 1 {
                SSL_free(ssl);
                return Err(io::Error::other("SSL_set_fd failed"));
            }
            if SSL_ctrl(
                ssl,
                SSL_CTRL_SET_TLSEXT_HOSTNAME,
                TLSEXT_NAMETYPE_HOST_NAME,
                c_host.as_ptr() as *mut c_void,
            ) != 1
            {
                SSL_free(ssl);
                return Err(io::Error::other("SSL_ctrl(SNI) failed"));
            }
            if SSL_set1_host(ssl, c_host.as_ptr()) != 1 {
                SSL_free(ssl);
                return Err(io::Error::other("SSL_set1_host failed"));
            }
            ssl
        };

        let stream = Self { ssl, async_fd };
        loop {
            let ret = unsafe {
                ERR_clear_error();
                SSL_connect(stream.ssl)
            };
            if ret == 1 {
                if verify_mode == SSL_VERIFY_PEER {
                    let verify_res = unsafe { SSL_get_verify_result(stream.ssl) };
                    if verify_res != X509_V_OK {
                        return Err(io::Error::new(
                            io::ErrorKind::PermissionDenied,
                            format!(
                                "TLS certificate verification failed (verify_result={})",
                                verify_res
                            ),
                        ));
                    }
                }
                return Ok(stream);
            }
            let err = unsafe { SSL_get_error(stream.ssl, ret) };
            match err {
                SSL_ERROR_WANT_READ => {
                    let mut guard = stream.async_fd.readable().await?;
                    guard.clear_ready();
                }
                SSL_ERROR_WANT_WRITE => {
                    let mut guard = stream.async_fd.writable().await?;
                    guard.clear_ready();
                }
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::ConnectionRefused,
                        format!("TLS handshake failed (SSL_error={})", err),
                    ));
                }
            }
        }
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
impl AsyncRead for OpenSslStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        loop {
            let slice = buf.initialize_unfilled();
            if slice.is_empty() {
                return Poll::Ready(Ok(()));
            }
            let cap = slice.len().min(i32::MAX as usize) as c_int;
            let ret = unsafe {
                ERR_clear_error();
                SSL_read(self.ssl, slice.as_mut_ptr() as *mut c_void, cap)
            };
            if ret > 0 {
                buf.advance(ret as usize);
                return Poll::Ready(Ok(()));
            }
            let err = unsafe { SSL_get_error(self.ssl, ret) };
            match err {
                SSL_ERROR_ZERO_RETURN => return Poll::Ready(Ok(())),
                SSL_ERROR_SYSCALL if ret == 0 => return Poll::Ready(Ok(())),
                SSL_ERROR_WANT_READ => match self.async_fd.poll_read_ready(cx) {
                    Poll::Ready(Ok(mut guard)) => {
                        guard.clear_ready();
                        continue;
                    }
                    Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                    Poll::Pending => return Poll::Pending,
                },
                SSL_ERROR_WANT_WRITE => match self.async_fd.poll_write_ready(cx) {
                    Poll::Ready(Ok(mut guard)) => {
                        guard.clear_ready();
                        continue;
                    }
                    Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                    Poll::Pending => return Poll::Pending,
                },
                _ => {
                    return Poll::Ready(Err(io::Error::other(format!(
                        "TLS read failed (SSL_error={})",
                        err
                    ))));
                }
            }
        }
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
impl AsyncWrite for OpenSslStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        loop {
            let len = buf.len().min(i32::MAX as usize) as c_int;
            let ret = unsafe {
                ERR_clear_error();
                SSL_write(self.ssl, buf.as_ptr() as *const c_void, len)
            };
            if ret > 0 {
                return Poll::Ready(Ok(ret as usize));
            }
            let err = unsafe { SSL_get_error(self.ssl, ret) };
            match err {
                SSL_ERROR_WANT_WRITE => match self.async_fd.poll_write_ready(cx) {
                    Poll::Ready(Ok(mut guard)) => {
                        guard.clear_ready();
                        continue;
                    }
                    Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                    Poll::Pending => return Poll::Pending,
                },
                SSL_ERROR_WANT_READ => match self.async_fd.poll_read_ready(cx) {
                    Poll::Ready(Ok(mut guard)) => {
                        guard.clear_ready();
                        continue;
                    }
                    Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                    Poll::Pending => return Poll::Pending,
                },
                _ => {
                    return Poll::Ready(Err(io::Error::other(format!(
                        "TLS write failed (SSL_error={})",
                        err
                    ))));
                }
            }
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        unsafe {
            let _ = SSL_shutdown(self.ssl);
        }
        Poll::Ready(Ok(()))
    }
}

/// native-tls-backed stream for the platforms that do not link OpenSSL: Schannel on Windows and
/// Security.framework on macOS. Both share the same PEM-bundle / insecure-tls handling below.
#[cfg(any(windows, target_os = "macos"))]
pub(crate) struct NativeTlsStream {
    inner: tokio_native_tls::TlsStream<TcpStream>,
}

#[cfg(any(windows, target_os = "macos"))]
impl NativeTlsStream {
    async fn connect(tcp: TcpStream, host: &str) -> io::Result<Self> {
        let clean_host = host.trim_start_matches('[').trim_end_matches(']');
        let mut builder = native_tls::TlsConnector::builder();
        if std::env::var("KROMA_INSECURE_TLS").as_deref() == Ok("1") {
            INSECURE_TLS_WARNED.call_once(|| {
                eprintln!(
                    "[SECURITY WARNING] KROMA_INSECURE_TLS=1 is enabled: TLS certificate verification is DISABLED. Never use this outside local debugging."
                );
            });
            builder.danger_accept_invalid_certs(true);
            builder.danger_accept_invalid_hostnames(true);
        }
        if let Ok(ca_bundle) = std::env::var("KROMA_CA_BUNDLE") {
            let pem_bytes = std::fs::read(&ca_bundle).map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("failed to read KROMA_CA_BUNDLE '{}': {}", ca_bundle, e),
                )
            })?;
            let pem_text = String::from_utf8_lossy(&pem_bytes);
            const BEGIN_MARKER: &str = "-----BEGIN CERTIFICATE-----";
            const END_MARKER: &str = "-----END CERTIFICATE-----";
            let mut loaded_any = false;
            let mut cursor = pem_text.as_ref();
            while let Some(start) = cursor.find(BEGIN_MARKER) {
                let after_start = &cursor[start..];
                let Some(end_offset) = after_start.find(END_MARKER) else {
                    break;
                };
                let block_end = end_offset + END_MARKER.len();
                let block = &after_start[..block_end];
                let cert = native_tls::Certificate::from_pem(block.as_bytes()).map_err(|e| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("invalid PEM in KROMA_CA_BUNDLE '{}': {}", ca_bundle, e),
                    )
                })?;
                builder.add_root_certificate(cert);
                loaded_any = true;
                cursor = &after_start[block_end..];
            }
            if !loaded_any {
                let cert = native_tls::Certificate::from_pem(&pem_bytes).map_err(|e| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("invalid PEM in KROMA_CA_BUNDLE '{}': {}", ca_bundle, e),
                    )
                })?;
                builder.add_root_certificate(cert);
            }
        }
        let connector = builder
            .build()
            .map_err(|e| io::Error::other(format!("native TLS init failed: {}", e)))?;
        let async_connector = tokio_native_tls::TlsConnector::from(connector);
        let stream = async_connector
            .connect(clean_host, tcp)
            .await
            .map_err(|e| {
                io::Error::new(
                    io::ErrorKind::ConnectionRefused,
                    format!("TLS handshake failed: {}", e),
                )
            })?;
        Ok(Self { inner: stream })
    }
}

#[cfg(any(windows, target_os = "macos"))]
impl AsyncRead for NativeTlsStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

#[cfg(any(windows, target_os = "macos"))]
impl AsyncWrite for NativeTlsStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
pub(crate) type PlatformTlsStream = OpenSslStream;
#[cfg(any(windows, target_os = "macos"))]
pub(crate) type PlatformTlsStream = NativeTlsStream;

pub(crate) enum MaybeTlsStream {
    Plain(TcpStream),
    Tls(PlatformTlsStream),
}

impl AsyncRead for MaybeTlsStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match &mut *self {
            MaybeTlsStream::Plain(s) => Pin::new(s).poll_read(cx, buf),
            MaybeTlsStream::Tls(s) => Pin::new(s).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for MaybeTlsStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match &mut *self {
            MaybeTlsStream::Plain(s) => Pin::new(s).poll_write(cx, buf),
            MaybeTlsStream::Tls(s) => Pin::new(s).poll_write(cx, buf),
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match &mut *self {
            MaybeTlsStream::Plain(s) => Pin::new(s).poll_flush(cx),
            MaybeTlsStream::Tls(s) => Pin::new(s).poll_flush(cx),
        }
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match &mut *self {
            MaybeTlsStream::Plain(s) => Pin::new(s).poll_shutdown(cx),
            MaybeTlsStream::Tls(s) => Pin::new(s).poll_shutdown(cx),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Request / response
// ---------------------------------------------------------------------------------------------

/// Everything needed to write one HTTP/1.1 request. `headers` are emitted verbatim after the
/// mandatory `Host`/`Content-Length` pair, so protocol-specific authentication schemes
/// (`Authorization: Bearer`, `x-api-key`) are handled uniformly.
pub(crate) struct HttpRequestSpec<'a> {
    pub method: &'a str,
    pub path: &'a str,
    pub headers: &'a [(String, String)],
    pub body: &'a str,
    pub accept: &'a str,
}

pub(crate) struct HttpResponseHead {
    pub status: u16,
    pub content_type: Option<String>,
    pub content_length: Option<usize>,
    pub is_chunked: bool,
    pub retry_after: Option<Duration>,
    pub reader: BufReader<MaybeTlsStream>,
}

fn step_timeout(deadline: Instant, preferred: Duration) -> Result<Duration, ProviderError> {
    let now = Instant::now();
    if now >= deadline {
        return Err(ProviderError::Timeout);
    }
    Ok(preferred.min(deadline.duration_since(now)))
}

/// Connect (TCP + TLS), write the request, and parse the status line and headers.
///
/// Any HTTP status is returned to the caller — retry/redirect policy belongs to the protocol client.
pub(crate) async fn send_request(
    endpoint: &ValidatedEndpoint,
    spec: &HttpRequestSpec<'_>,
    budget: &BudgetConfig,
    deadline: Instant,
) -> Result<HttpResponseHead, TransportFailure> {
    let connect_to =
        step_timeout(deadline, budget.connect_timeout).map_err(TransportFailure::Transient)?;
    // KR-E06: resolve the destination exactly once and connect to that address. Letting
    // `TcpStream::connect` resolve the name means the address that was authorized and the address
    // that is dialed come from two separate lookups, and a DNS answer that changes between them is
    // the rebinding window. TLS still uses `endpoint.host` for SNI, so certificate validation is
    // unchanged -- only the dial target is pinned.
    let addrs: Vec<std::net::SocketAddr> =
        match tokio::time::timeout(connect_to, tokio::net::lookup_host(endpoint.connect_addr()))
            .await
        {
            Ok(Ok(iter)) => iter.collect(),
            Ok(Err(e)) => {
                return Err(TransportFailure::Transient(ProviderError::Transport(
                    format!("resolve failed: {}", e),
                )));
            }
            Err(_) => return Err(TransportFailure::Transient(ProviderError::Timeout)),
        };
    if addrs.is_empty() {
        return Err(TransportFailure::Transient(ProviderError::Transport(
            "resolve returned no address".to_string(),
        )));
    }
    // KR-E06 (second half): a hostname is authorized by name, but the name is what resolves to an
    // address. If a public name resolves into a private/loopback range, the request would be sent to
    // a destination nobody approved -- that is the SSRF shape, not a rebinding race. IP literals are
    // judged by the broker before dispatch, so only resolved names are checked here.
    // Loopback is excluded on purpose: `localhost` is a legitimate local fixture, and an operator
    // pointing at 127.0.0.1 has already declared that intent. What must not happen is a public name
    // quietly resolving into a private network (10/8, 172.16/12, 192.168/16, link-local, ULA).
    if endpoint.host.parse::<std::net::IpAddr>().is_err()
        && addrs.iter().any(|addr| {
            !addr.ip().is_loopback() && crate::credential_broker::is_private(&addr.ip().to_string())
        })
    {
        return Err(TransportFailure::Transient(ProviderError::Transport(
            format!(
                "refusing to connect to {}: the name resolved to a private address ({})",
                endpoint.host,
                addrs
                    .iter()
                    .map(|a| a.ip().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        )));
    }
    let mut last_error: Option<String> = None;
    let mut connected: Option<TcpStream> = None;
    for addr in &addrs {
        let attempt_to =
            step_timeout(deadline, budget.connect_timeout).map_err(TransportFailure::Transient)?;
        match tokio::time::timeout(attempt_to, TcpStream::connect(addr)).await {
            Ok(Ok(stream)) => {
                connected = Some(stream);
                break;
            }
            Ok(Err(e)) => last_error = Some(e.to_string()),
            Err(_) => return Err(TransportFailure::Transient(ProviderError::Timeout)),
        }
    }
    let tcp = match connected {
        Some(stream) => stream,
        None => {
            return Err(TransportFailure::Transient(ProviderError::Transport(
                format!(
                    "connect failed: {}",
                    last_error.unwrap_or_else(|| "no address attempted".to_string())
                ),
            )));
        }
    };

    let mut transport = if endpoint.scheme == "https" {
        let tls_to =
            step_timeout(deadline, budget.connect_timeout).map_err(TransportFailure::Transient)?;
        match tokio::time::timeout(tls_to, PlatformTlsStream::connect(tcp, &endpoint.host)).await {
            Ok(Ok(tls)) => MaybeTlsStream::Tls(tls),
            Ok(Err(e)) => {
                return Err(TransportFailure::Transient(ProviderError::Transport(
                    format!("TLS handshake failed: {}", e),
                )));
            }
            Err(_) => return Err(TransportFailure::Transient(ProviderError::Timeout)),
        }
    } else {
        MaybeTlsStream::Plain(tcp)
    };

    let mut wire = format!(
        "{} {} HTTP/1.1\r\nHost: {}\r\n",
        spec.method,
        spec.path,
        endpoint.host_header()
    );
    for (name, value) in spec.headers {
        wire.push_str(name);
        wire.push_str(": ");
        wire.push_str(value);
        wire.push_str("\r\n");
    }
    wire.push_str(&format!(
        "Content-Type: application/json\r\nAccept: {}\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",
        spec.accept,
        spec.body.len()
    ));
    wire.push_str(spec.body);

    let write_to =
        step_timeout(deadline, budget.idle_timeout).map_err(TransportFailure::Transient)?;
    match tokio::time::timeout(write_to, transport.write_all(wire.as_bytes())).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            return Err(TransportFailure::Transient(ProviderError::Transport(
                format!("write failed: {}", e),
            )));
        }
        Err(_) => return Err(TransportFailure::Transient(ProviderError::Timeout)),
    }

    let mut reader = BufReader::new(transport);
    let status_line = read_line_bounded(
        &mut reader,
        MAX_HTTP_HEADER_LINE_BYTES,
        budget.idle_timeout,
        deadline,
    )
    .await
    .map_err(TransportFailure::Transient)?;
    if status_line.is_empty() {
        return Err(TransportFailure::Transient(ProviderError::Transport(
            "server closed connection before status".into(),
        )));
    }

    let status = parse_http_status_code(&status_line).ok_or_else(|| {
        TransportFailure::Fatal(ProviderError::Transport(format!(
            "invalid status line: {}",
            status_line.trim()
        )))
    })?;

    let mut retry_after: Option<Duration> = None;
    let mut content_type: Option<String> = None;
    let mut is_chunked = false;
    let mut content_length: Option<usize> = None;
    let mut header_count = 0usize;

    loop {
        header_count += 1;
        if header_count > MAX_HTTP_HEADERS_COUNT {
            return Err(TransportFailure::Fatal(ProviderError::Transport(
                "HTTP response exceeded max header count".to_string(),
            )));
        }
        let hdr_line = read_line_bounded(
            &mut reader,
            MAX_HTTP_HEADER_LINE_BYTES,
            budget.idle_timeout,
            deadline,
        )
        .await
        .map_err(TransportFailure::Transient)?;
        if hdr_line.is_empty() || hdr_line == "\r\n" || hdr_line == "\n" {
            break;
        }
        if let Some((k, v)) = hdr_line.split_once(':') {
            let key = k.trim().to_ascii_lowercase();
            let val = v.trim();
            if key == "content-type" {
                if content_type.is_some() {
                    return Err(TransportFailure::Fatal(ProviderError::Transport(
                        "duplicate Content-Type response header".to_string(),
                    )));
                }
                content_type = Some(val.to_string());
            } else if key == "retry-after" {
                if let Ok(secs) = val.parse::<u64>() {
                    retry_after = Some(Duration::from_secs(secs));
                } else if let Ok(ms) = val.strip_suffix("ms").unwrap_or("").parse::<u64>() {
                    retry_after = Some(Duration::from_millis(ms));
                }
            } else if key == "transfer-encoding" {
                // F-04: accept only a single, exact `chunked` transfer-coding. Ambiguous or
                // unknown codings are rejected rather than guessed at.
                let codings: Vec<String> = val
                    .split(',')
                    .map(|c| c.trim().to_ascii_lowercase())
                    .filter(|c| !c.is_empty())
                    .collect();
                if codings.len() != 1 || codings[0] != "chunked" {
                    return Err(TransportFailure::Fatal(ProviderError::Transport(format!(
                        "unsupported or ambiguous Transfer-Encoding: {}",
                        val
                    ))));
                }
                if is_chunked {
                    return Err(TransportFailure::Fatal(ProviderError::Transport(
                        "duplicate Transfer-Encoding header".to_string(),
                    )));
                }
                is_chunked = true;
            } else if key == "content-length" {
                // F-04: parse the value; reject non-numeric and conflicting duplicates.
                let parsed = val.parse::<usize>().map_err(|_| {
                    TransportFailure::Fatal(ProviderError::Transport(format!(
                        "invalid Content-Length header value: {}",
                        val
                    )))
                })?;
                if let Some(existing) = content_length {
                    if existing != parsed {
                        return Err(TransportFailure::Fatal(ProviderError::Transport(
                            "conflicting Content-Length headers".to_string(),
                        )));
                    }
                } else {
                    content_length = Some(parsed);
                }
            }
        }
    }

    if is_chunked && content_length.is_some() {
        return Err(TransportFailure::Fatal(ProviderError::Transport(
            "HTTP response contains both Transfer-Encoding: chunked and Content-Length".to_string(),
        )));
    }

    Ok(HttpResponseHead {
        status,
        content_type,
        content_length,
        is_chunked,
        retry_after,
        reader,
    })
}

/// Stream the response body, invoking `on_chunk` for every decoded byte run.
///
/// Handles both `Transfer-Encoding: chunked` and `Content-Length` framing, enforcing
/// `max_bytes` as a hard cap in both cases.
pub(crate) async fn pump_body<F>(
    head: &mut HttpResponseHead,
    budget: &BudgetConfig,
    deadline: Instant,
    max_bytes: usize,
    mut on_chunk: F,
) -> Result<(), BodyFailure>
where
    F: FnMut(&[u8]) -> Result<Flow, ProviderError>,
{
    let mut buf = [0u8; 4096];

    if head.is_chunked {
        let mut total_decoded = 0usize;
        loop {
            let size_line = read_line_bounded(
                &mut head.reader,
                MAX_HTTP_HEADER_LINE_BYTES,
                budget.idle_timeout,
                deadline,
            )
            .await
            .map_err(|e| {
                BodyFailure::Stream(ProviderError::IncompleteStream(format!(
                    "chunked size read error: {}",
                    e
                )))
            })?;
            if size_line.is_empty() {
                return Err(BodyFailure::Stream(ProviderError::IncompleteStream(
                    "unexpected EOF before terminating 0 chunk".to_string(),
                )));
            }
            let trimmed = size_line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let hex_part = trimmed.split(';').next().unwrap_or("");
            let chunk_size = usize::from_str_radix(hex_part, 16).map_err(|e| {
                BodyFailure::Stream(ProviderError::IncompleteStream(format!(
                    "invalid HTTP chunk size: {}",
                    e
                )))
            })?;
            if chunk_size > max_bytes || total_decoded.saturating_add(chunk_size) > max_bytes {
                return Err(BodyFailure::Fatal(ProviderError::IncompleteStream(
                    format!(
                        "HTTP chunk size ({}) exceeds max body size ({})",
                        chunk_size, max_bytes
                    ),
                )));
            }
            if chunk_size == 0 {
                // Drain trailing CRLF / trailer headers.
                let _ = read_line_bounded(
                    &mut head.reader,
                    MAX_HTTP_HEADER_LINE_BYTES,
                    budget.idle_timeout,
                    deadline,
                )
                .await;
                return Ok(());
            }

            let mut remaining = chunk_size;
            while remaining > 0 {
                let to_read = remaining.min(buf.len());
                let read_to = step_timeout(deadline, budget.idle_timeout).map_err(|e| {
                    BodyFailure::Stream(ProviderError::IncompleteStream(e.to_string()))
                })?;
                match tokio::time::timeout(read_to, head.reader.read(&mut buf[..to_read])).await {
                    Ok(Ok(0)) => {
                        return Err(BodyFailure::Stream(ProviderError::IncompleteStream(
                            "unexpected EOF inside HTTP chunk".to_string(),
                        )));
                    }
                    Ok(Ok(n)) => {
                        remaining -= n;
                        total_decoded += n;
                        match on_chunk(&buf[..n]).map_err(BodyFailure::Fatal)? {
                            Flow::Continue => {}
                            Flow::Stop => return Ok(()),
                        }
                    }
                    Ok(Err(e)) => {
                        return Err(BodyFailure::Stream(ProviderError::IncompleteStream(
                            format!("body read error: {}", e),
                        )));
                    }
                    Err(_) => {
                        return Err(BodyFailure::Stream(ProviderError::IncompleteStream(
                            "body idle timeout".to_string(),
                        )));
                    }
                }
            }

            // Consume the trailing CRLF after chunk data. This is *not* the end of the body: the
            // terminating 0-size chunk (and its trailers) has not been seen yet, so a timeout, EOF
            // or I/O error here means the transfer was cut short, not that it completed cleanly.
            // Only the reads *after* the 0 chunk may ignore errors.
            let mut crlf = [0u8; 2];
            let crlf_to = step_timeout(deadline, budget.idle_timeout)
                .map_err(|e| BodyFailure::Stream(ProviderError::IncompleteStream(e.to_string())))?;
            match tokio::time::timeout(crlf_to, head.reader.read_exact(&mut crlf)).await {
                Ok(Ok(_)) => {}
                Ok(Err(e)) if e.kind() == io::ErrorKind::UnexpectedEof => {
                    return Err(BodyFailure::Stream(ProviderError::IncompleteStream(
                        "unexpected EOF before the chunk-terminating CRLF".to_string(),
                    )));
                }
                Ok(Err(e)) => {
                    return Err(BodyFailure::Stream(ProviderError::IncompleteStream(
                        format!("chunk terminator read error: {}", e),
                    )));
                }
                Err(_) => {
                    return Err(BodyFailure::Stream(ProviderError::IncompleteStream(
                        "idle timeout before the chunk-terminating CRLF".to_string(),
                    )));
                }
            }
            if crlf != *b"\r\n" {
                return Err(BodyFailure::Stream(ProviderError::IncompleteStream(
                    format!(
                        "malformed HTTP chunk terminator: expected CRLF, got {:02x?}",
                        crlf
                    ),
                )));
            }
        }
    }

    let mut total_read = 0usize;
    loop {
        if let Some(len) = head.content_length {
            if total_read >= len {
                break;
            }
        }
        let max_to_read = match head.content_length {
            Some(len) => (len - total_read).min(buf.len()),
            None => buf.len(),
        };
        let read_to = step_timeout(deadline, budget.idle_timeout)
            .map_err(|e| BodyFailure::Stream(ProviderError::IncompleteStream(e.to_string())))?;
        match tokio::time::timeout(read_to, head.reader.read(&mut buf[..max_to_read])).await {
            Ok(Ok(0)) => break,
            Ok(Ok(n)) => {
                if total_read.saturating_add(n) > max_bytes {
                    return Err(BodyFailure::Fatal(ProviderError::Transport(format!(
                        "HTTP response body exceeded max size ({} bytes)",
                        max_bytes
                    ))));
                }
                total_read += n;
                match on_chunk(&buf[..n]).map_err(BodyFailure::Fatal)? {
                    Flow::Continue => {}
                    Flow::Stop => return Ok(()),
                }
            }
            Ok(Err(e)) => {
                return Err(BodyFailure::Stream(ProviderError::IncompleteStream(
                    format!("body read error: {}", e),
                )));
            }
            Err(_) => {
                return Err(BodyFailure::Stream(ProviderError::IncompleteStream(
                    "body idle timeout".to_string(),
                )));
            }
        }
    }
    if let Some(len) = head.content_length {
        if total_read < len {
            return Err(BodyFailure::Stream(ProviderError::IncompleteStream(
                format!(
                    "HTTP body ended prematurely: expected {} bytes, received {}",
                    len, total_read
                ),
            )));
        }
    }
    Ok(())
}

/// Read a complete (non-streaming) body, enforcing `max_bytes`.
pub(crate) async fn read_full_body(
    head: &mut HttpResponseHead,
    budget: &BudgetConfig,
    deadline: Instant,
    max_bytes: usize,
) -> Result<Vec<u8>, ProviderError> {
    let cap = match head.content_length {
        Some(len) => len.min(max_bytes),
        None => max_bytes,
    };
    let mut out: Vec<u8> = Vec::new();
    pump_body(head, budget, deadline, cap, |chunk| {
        out.extend_from_slice(chunk);
        Ok(Flow::Continue)
    })
    .await
    .map_err(|e| match e {
        BodyFailure::Fatal(e) | BodyFailure::Stream(e) => e,
    })?;
    if let Some(len) = head.content_length {
        if out.len() < len {
            return Err(ProviderError::IncompleteStream(format!(
                "HTTP body ended prematurely: expected {} bytes, received {}",
                len,
                out.len()
            )));
        }
    }
    Ok(out)
}

pub(crate) fn parse_http_status_code(status_line: &str) -> Option<u16> {
    let mut parts = status_line.split_whitespace();
    let _http_ver = parts.next()?;
    let code_str = parts.next()?;
    code_str.parse::<u16>().ok()
}

pub(crate) async fn read_line_bounded<R: AsyncBufReadExt + Unpin>(
    reader: &mut R,
    max_bytes: usize,
    idle_timeout: Duration,
    request_deadline: Instant,
) -> Result<String, ProviderError> {
    let now = Instant::now();
    if now >= request_deadline {
        return Err(ProviderError::Timeout);
    }
    let to = idle_timeout.min(request_deadline.duration_since(now));
    let mut line = String::new();
    let mut limited = reader.take((max_bytes + 1) as u64);
    match tokio::time::timeout(to, limited.read_line(&mut line)).await {
        Ok(Ok(_)) => {
            if line.len() > max_bytes {
                return Err(ProviderError::Transport(format!(
                    "HTTP line exceeded max length ({} bytes)",
                    max_bytes
                )));
            }
            Ok(line)
        }
        Ok(Err(e)) => Err(ProviderError::Transport(e.to_string())),
        Err(_) => Err(ProviderError::Timeout),
    }
}

/// Drain an error body (bounded) so the connection is not left mid-response.
pub(crate) async fn read_error_body(
    head: &mut HttpResponseHead,
    budget: &BudgetConfig,
    deadline: Instant,
) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::new();
    let _ = pump_body(head, budget, deadline, MAX_ERROR_BODY_BYTES, |chunk| {
        out.extend_from_slice(chunk);
        Ok(Flow::Continue)
    })
    .await;
    out.truncate(MAX_ERROR_BODY_BYTES);
    out
}

/// Stream an SSE body, invoking `on_event(event_name, data_payload)` once per complete event.
///
/// Anthropic and OpenAI Responses both use named SSE events (`event:` + `data:` lines terminated by
/// a blank line), so the framing lives here instead of being re-implemented per protocol. The raw
/// byte budget is enforced by [`pump_body`].
pub(crate) async fn pump_sse_events<F>(
    head: &mut HttpResponseHead,
    budget: &BudgetConfig,
    deadline: Instant,
    max_bytes: usize,
    mut on_event: F,
) -> Result<(), BodyFailure>
where
    F: FnMut(&str, &str) -> Result<Flow, ProviderError>,
{
    let mut pending: Vec<u8> = Vec::new();
    let mut event_name = String::new();
    let mut data = String::new();

    pump_body(head, budget, deadline, max_bytes, |chunk| {
        pending.extend_from_slice(chunk);
        while let Some(pos) = pending.iter().position(|b| *b == b'\n') {
            let mut line_bytes: Vec<u8> = pending.drain(..=pos).collect();
            if line_bytes.last() == Some(&b'\n') {
                line_bytes.pop();
            }
            if line_bytes.last() == Some(&b'\r') {
                line_bytes.pop();
            }
            let line = String::from_utf8_lossy(&line_bytes).to_string();
            if line.is_empty() {
                if !data.is_empty() || !event_name.is_empty() {
                    let name = if event_name.is_empty() {
                        "message".to_string()
                    } else {
                        std::mem::take(&mut event_name)
                    };
                    let payload = std::mem::take(&mut data);
                    match on_event(&name, &payload)? {
                        Flow::Continue => {}
                        Flow::Stop => return Ok(Flow::Stop),
                    }
                }
                event_name.clear();
                data.clear();
                continue;
            }
            if let Some(rest) = line.strip_prefix("event:") {
                event_name = rest.trim().to_string();
            } else if let Some(rest) = line.strip_prefix("data:") {
                if !data.is_empty() {
                    data.push('\n');
                }
                data.push_str(rest.trim_start());
            }
            // `id:` / `retry:` / comment lines are ignored: neither protocol uses them.
        }
        Ok(Flow::Continue)
    })
    .await?;

    // A stream that ends without a trailing blank line still delivers its last event.
    if !data.is_empty() || !event_name.is_empty() {
        let name = if event_name.is_empty() {
            "message".to_string()
        } else {
            event_name
        };
        match on_event(&name, &data).map_err(BodyFailure::Fatal)? {
            Flow::Continue => {}
            Flow::Stop => return Ok(()),
        }
    }
    Ok(())
}
