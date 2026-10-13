use crate::budget::BudgetConfig;
use crate::messages::{
    AssistantTurn, FinishReason, ReasoningBlock, ToolCall, TranscriptItem,
};
use crate::provider::sse::{parse_usage_json, SseStreamAssembler, MAX_SSE_RESPONSE_BYTES};
use crate::provider::{ModelProvider, ProviderCompletion, ProviderError, ProviderRequest};
use crate::tool::definition::ToolDefinition;
use crate::types::ToolCallId;
use serde_json::{json, Value};
#[cfg(unix)]
use std::ffi::{c_char, c_int, c_long, c_void, CString};
use std::fmt;
use std::future::Future;
use std::io;
#[cfg(unix)]
use std::os::unix::io::AsRawFd;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};
#[cfg(unix)]
use tokio::io::unix::AsyncFd;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader, ReadBuf};
use tokio::net::TcpStream;
use tokio::sync::watch;

const MAX_HTTP_HEADER_LINE_BYTES: usize = 16 * 1024;
const MAX_HTTP_HEADERS_COUNT: usize = 128;
const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;

/// F-05: `HARNESS_INSECURE_TLS=1` is a dangerous debugging escape hatch. Warn exactly once so the
/// operator cannot miss it, instead of silently accepting unverified certificates.
static INSECURE_TLS_WARNED: std::sync::Once = std::sync::Once::new();

#[cfg(unix)]
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
    fn SSL_CTX_set_verify(ctx: *mut c_void, mode: c_int, cb: Option<extern "C" fn(c_int, *mut c_void) -> c_int>);
    fn SSL_new(ctx: *mut c_void) -> *mut c_void;
    fn SSL_free(ssl: *mut c_void);
    fn SSL_set_fd(ssl: *mut c_void, fd: c_int) -> c_int;
    fn SSL_ctrl(ssl: *mut c_void, cmd: c_int, larg: c_long, parg: *mut c_void) -> c_long;
    fn SSL_set1_host(ssl: *mut c_void, hostname: *const c_char) -> c_int;
    fn SSL_get_verify_result(ssl: *const c_void) -> c_long;
    fn SSL_connect(ssl: *mut c_void) -> c_int;
    fn SSL_read(ssl: *mut c_void, buf: *mut c_void, num: c_int) -> c_int;
    fn SSL_write(ssl: *mut c_void, buf: *const c_void, num: c_int) -> c_int;
    fn SSL_shutdown(ssl: *mut c_void) -> c_int;
    fn SSL_get_error(ssl: *const c_void, ret_code: c_int) -> c_int;
}

#[cfg(unix)]
const SSL_VERIFY_NONE: c_int = 0x00;
#[cfg(unix)]
const SSL_VERIFY_PEER: c_int = 0x01;
#[cfg(unix)]
const X509_V_OK: c_long = 0;
#[cfg(unix)]
const SSL_CTRL_SET_TLSEXT_HOSTNAME: c_int = 55;
#[cfg(unix)]
const TLSEXT_NAMETYPE_HOST_NAME: c_long = 0;
#[cfg(unix)]
const SSL_ERROR_WANT_READ: c_int = 2;
#[cfg(unix)]
const SSL_ERROR_WANT_WRITE: c_int = 3;
#[cfg(unix)]
const SSL_ERROR_SYSCALL: c_int = 5;
#[cfg(unix)]
const SSL_ERROR_ZERO_RETURN: c_int = 6;

#[cfg(unix)]
struct OpenSslStream {
    ctx: *mut c_void,
    ssl: *mut c_void,
    async_fd: AsyncFd<std::net::TcpStream>,
}

#[cfg(unix)]
// Safety: Each OpenSslStream owns a dedicated SSL* and SSL_CTX* accessed exclusively by &mut self.
unsafe impl Send for OpenSslStream {}
#[cfg(unix)]
unsafe impl Sync for OpenSslStream {}

#[cfg(unix)]
impl Drop for OpenSslStream {
    fn drop(&mut self) {
        unsafe {
            if !self.ssl.is_null() {
                let _ = SSL_shutdown(self.ssl);
                SSL_free(self.ssl);
            }
            if !self.ctx.is_null() {
                SSL_CTX_free(self.ctx);
            }
        }
    }
}

#[cfg(unix)]
impl OpenSslStream {
    async fn connect(tcp: TcpStream, host: &str) -> io::Result<Self> {
        let std_tcp = tcp.into_std()?;
        std_tcp.set_nonblocking(true)?;
        let async_fd = AsyncFd::new(std_tcp)?;

        let clean_host = host.trim_start_matches('[').trim_end_matches(']');
        let c_host = CString::new(clean_host)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid TLS hostname"))?;

        let verify_mode = if std::env::var("HARNESS_INSECURE_TLS").as_deref() == Ok("1") {
            INSECURE_TLS_WARNED.call_once(|| {
                eprintln!(
                    "[SECURITY WARNING] HARNESS_INSECURE_TLS=1 is enabled: TLS certificate verification is DISABLED. Never use this outside local debugging."
                );
            });
            SSL_VERIFY_NONE
        } else {
            SSL_VERIFY_PEER
        };

        let (ctx, ssl) = unsafe {
            let method = TLS_client_method();
            if method.is_null() {
                return Err(io::Error::other("TLS_client_method returned null"));
            }
            let ctx = SSL_CTX_new(method);
            if ctx.is_null() {
                return Err(io::Error::other("SSL_CTX_new returned null"));
            }
            if SSL_CTX_set_default_verify_paths(ctx) != 1 {
                SSL_CTX_free(ctx);
                return Err(io::Error::other("SSL_CTX_set_default_verify_paths failed"));
            }
            if let Ok(ca_bundle) = std::env::var("HARNESS_CA_BUNDLE") {
                let c_ca = CString::new(ca_bundle).map_err(|_| {
                    SSL_CTX_free(ctx);
                    io::Error::new(io::ErrorKind::InvalidInput, "invalid HARNESS_CA_BUNDLE")
                })?;
                if SSL_CTX_load_verify_locations(ctx, c_ca.as_ptr(), std::ptr::null()) != 1 {
                    SSL_CTX_free(ctx);
                    return Err(io::Error::other("SSL_CTX_load_verify_locations failed"));
                }
            }
            SSL_CTX_set_verify(ctx, verify_mode, None);

            let ssl = SSL_new(ctx);
            if ssl.is_null() {
                SSL_CTX_free(ctx);
                return Err(io::Error::other("SSL_new returned null"));
            }
            if SSL_set_fd(ssl, async_fd.get_ref().as_raw_fd()) != 1 {
                SSL_free(ssl);
                SSL_CTX_free(ctx);
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
                SSL_CTX_free(ctx);
                return Err(io::Error::other("SSL_ctrl(SNI) failed"));
            }
            if SSL_set1_host(ssl, c_host.as_ptr()) != 1 {
                SSL_free(ssl);
                SSL_CTX_free(ctx);
                return Err(io::Error::other("SSL_set1_host failed"));
            }
            (ctx, ssl)
        };

        let stream = Self { ctx, ssl, async_fd };
        loop {
            let ret = unsafe { SSL_connect(stream.ssl) };
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

#[cfg(unix)]
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
            let ret = unsafe { SSL_read(self.ssl, slice.as_mut_ptr() as *mut c_void, cap) };
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

#[cfg(unix)]
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
            let ret = unsafe { SSL_write(self.ssl, buf.as_ptr() as *const c_void, len) };
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

#[cfg(windows)]
struct SchannelTlsStream {
    inner: tokio_native_tls::TlsStream<TcpStream>,
}

#[cfg(windows)]
impl SchannelTlsStream {
    async fn connect(tcp: TcpStream, host: &str) -> io::Result<Self> {
        let clean_host = host.trim_start_matches('[').trim_end_matches(']');
        let mut builder = native_tls::TlsConnector::builder();
        if std::env::var("HARNESS_INSECURE_TLS").as_deref() == Ok("1") {
            INSECURE_TLS_WARNED.call_once(|| {
                eprintln!(
                    "[SECURITY WARNING] HARNESS_INSECURE_TLS=1 is enabled: TLS certificate verification is DISABLED. Never use this outside local debugging."
                );
            });
            builder.danger_accept_invalid_certs(true);
            builder.danger_accept_invalid_hostnames(true);
        }
        if let Ok(ca_bundle) = std::env::var("HARNESS_CA_BUNDLE") {
            let pem_bytes = std::fs::read(&ca_bundle).map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("failed to read HARNESS_CA_BUNDLE '{}': {}", ca_bundle, e),
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
                        format!("invalid PEM in HARNESS_CA_BUNDLE '{}': {}", ca_bundle, e),
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
                        format!("invalid PEM in HARNESS_CA_BUNDLE '{}': {}", ca_bundle, e),
                    )
                })?;
                builder.add_root_certificate(cert);
            }
        }
        let connector = builder
            .build()
            .map_err(|e| io::Error::other(format!("Windows Schannel TLS init failed: {}", e)))?;
        let async_connector = tokio_native_tls::TlsConnector::from(connector);
        let stream = async_connector.connect(clean_host, tcp).await.map_err(|e| {
            io::Error::new(
                io::ErrorKind::ConnectionRefused,
                format!("TLS handshake failed: {}", e),
            )
        })?;
        Ok(Self { inner: stream })
    }
}

#[cfg(windows)]
impl AsyncRead for SchannelTlsStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

#[cfg(windows)]
impl AsyncWrite for SchannelTlsStream {
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

#[cfg(unix)]
type PlatformTlsStream = OpenSslStream;
#[cfg(windows)]
type PlatformTlsStream = SchannelTlsStream;

enum MaybeTlsStream {
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

#[derive(Clone, Default)]
pub struct ChatCompletionsConfig {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub max_retries: usize,
    pub reasoning_effort: Option<String>,
}

impl fmt::Debug for ChatCompletionsConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChatCompletionsConfig")
            .field("base_url", &self.base_url)
            .field("api_key", &"Bearer ***")
            .field("model", &self.model)
            .field("max_retries", &self.max_retries)
            .field("reasoning_effort", &self.reasoning_effort)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedEndpoint {
    pub scheme: String,
    pub host: String,
    pub port: u16,
    pub base_path: String,
}

/// Validate `base_url` security rules (§4.3):
/// - Rejects any URL containing userinfo (`user:pass@host`).
/// - Enforces HTTPS for remote hosts; allows HTTP ONLY for `127.0.0.1`, `::1`, `[::1]`, `localhost`.
pub fn validate_base_url(raw_url: &str) -> Result<ValidatedEndpoint, ProviderError> {
    // F-04: the URL is split by hand and its path is interpolated directly into a hand-written
    // HTTP request line, so reject every control byte and whitespace character up front.
    if let Some(bad) = raw_url
        .bytes()
        .find(|b| *b < 0x20 || *b == 0x7f || *b == b' ' || *b == b'\t')
    {
        return Err(ProviderError::InsecureUrl(format!(
            "URL contains a forbidden control or whitespace byte (0x{:02x})",
            bad
        )));
    }
    let trimmed = raw_url.trim();
    let (scheme, rest) = if let Some(r) = trimmed.strip_prefix("https://") {
        ("https", r)
    } else if let Some(r) = trimmed.strip_prefix("http://") {
        ("http", r)
    } else {
        return Err(ProviderError::InsecureUrl(format!(
            "URL '{}' must start with https:// or http://",
            raw_url
        )));
    };

    if trimmed.contains('?') || trimmed.contains('#') {
        return Err(ProviderError::InsecureUrl(
            "URL must not contain query ('?') or fragment ('#')".to_string(),
        ));
    }

    let (authority, path_part) = match rest.find('/') {
        Some(idx) => (&rest[..idx], &rest[idx..]),
        None => (rest, ""),
    };

    if authority.contains('@') {
        return Err(ProviderError::InsecureUrl(
            "URL containing userinfo ('user:pass@host') is forbidden".to_string(),
        ));
    }

    let (host, port) = parse_host_port(authority, if scheme == "https" { 443 } else { 80 })?;

    if scheme == "http" && !is_loopback_host(&host) {
        return Err(ProviderError::InsecureUrl(format!(
            "plain HTTP is only permitted for localhost/127.0.0.1/::1, rejected remote host '{}'",
            host
        )));
    }

    let clean_path = path_part.trim_end_matches('/').to_string();

    Ok(ValidatedEndpoint {
        scheme: scheme.to_string(),
        host,
        port,
        base_path: clean_path,
    })
}

fn is_loopback_host(host: &str) -> bool {
    matches!(
        host.to_ascii_lowercase().as_str(),
        "localhost" | "127.0.0.1" | "::1" | "[::1]"
    )
}

fn parse_host_port(authority: &str, default_port: u16) -> Result<(String, u16), ProviderError> {
    if authority.is_empty() {
        return Err(ProviderError::InsecureUrl("empty host".to_string()));
    }
    if authority.starts_with('[') {
        // IPv6 literal `[::1]:8080`
        let close = authority
            .find(']')
            .ok_or_else(|| ProviderError::InsecureUrl("unclosed IPv6 bracket".to_string()))?;
        let host = &authority[..=close];
        let after = &authority[close + 1..];
        if after.is_empty() {
            return Ok((host.to_string(), default_port));
        }
        if let Some(p_str) = after.strip_prefix(':') {
            let p = p_str
                .parse::<u16>()
                .map_err(|_| ProviderError::InsecureUrl("invalid port".to_string()))?;
            return Ok((host.to_string(), p));
        }
        return Err(ProviderError::InsecureUrl("invalid authority".to_string()));
    }

    if let Some((h, p_str)) = authority.rsplit_once(':') {
        let p = p_str
            .parse::<u16>()
            .map_err(|_| ProviderError::InsecureUrl("invalid port".to_string()))?;
        Ok((h.to_string(), p))
    } else {
        Ok((authority.to_string(), default_port))
    }
}

/// Deterministic serialization of the OpenAI Chat Completions request body:
/// - `n = 1` fixed
/// - `tools` strictly sorted by ASCII name with deterministic JSON key ordering
/// - `reasoning_blocks` preserved in assistant messages for 2026 reasoning models
pub fn serialize_openai_request_body(
    model: &str,
    system_prompt: &str,
    tools: &[ToolDefinition],
    messages: &[TranscriptItem],
    stream: bool,
) -> String {
    serialize_openai_request_body_with_effort(model, system_prompt, tools, messages, stream, None)
}

pub fn serialize_openai_request_body_with_effort(
    model: &str,
    system_prompt: &str,
    tools: &[ToolDefinition],
    messages: &[TranscriptItem],
    stream: bool,
    reasoning_effort: Option<&str>,
) -> String {
    let mut sorted_tools = tools.to_vec();
    sorted_tools.sort_by(|a, b| a.name.as_bytes().cmp(b.name.as_bytes()));

    let tools_json: Vec<Value> = sorted_tools
        .iter()
        .map(|t| {
            json!({
                "type": "function",
                "function": {
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.parameters,
                }
            })
        })
        .collect();

    let mut wire_messages: Vec<Value> = Vec::with_capacity(messages.len() + 1);
    wire_messages.push(json!({
        "role": "system",
        "content": system_prompt,
    }));

    // DeepSeek thinking models require `reasoning_content` to be echoed back only for the
    // active (unfinished) turn. Replaying plain-text reasoning from turns before the last
    // user message pollutes the context, so gate `reasoning_content` on `idx >= last_user_idx`.
    // Signature/opaque blocks are still preserved verbatim for every assistant turn.
    let active_reasoning_from = messages
        .iter()
        .rposition(|item| matches!(item, TranscriptItem::User(_)))
        .unwrap_or(0);

    for (idx, item) in messages.iter().enumerate() {
        match item {
            TranscriptItem::User(u) => {
                wire_messages.push(json!({
                    "role": "user",
                    "content": u.content,
                }));
            }
            TranscriptItem::Assistant(a) => {
                let mut msg = serde_json::Map::new();
                msg.insert("role".to_string(), Value::String("assistant".to_string()));
                match &a.content {
                    Some(c) => {
                        msg.insert("content".to_string(), Value::String(c.clone()));
                    }
                    None => {
                        msg.insert("content".to_string(), Value::Null);
                    }
                }
                if !a.reasoning_blocks.is_empty() {
                    if idx >= active_reasoning_from {
                        let combined_text: String = a
                            .reasoning_blocks
                            .iter()
                            .filter_map(|b| b.text.as_deref())
                            .collect::<Vec<_>>()
                            .join("\n");
                        if !combined_text.is_empty() {
                            msg.insert(
                                "reasoning_content".to_string(),
                                Value::String(combined_text),
                            );
                        }
                    }
                    let has_signed_or_opaque = a
                        .reasoning_blocks
                        .iter()
                        .any(|b| b.signature.is_some() || b.opaque_payload.is_some());
                    if has_signed_or_opaque {
                        let thinking_items: Vec<Value> = a
                            .reasoning_blocks
                            .iter()
                            .map(|b| {
                                let mut obj = serde_json::Map::new();
                                if b.text.is_none() && b.opaque_payload.is_some() {
                                    obj.insert(
                                        "type".to_string(),
                                        Value::String("redacted_thinking".to_string()),
                                    );
                                } else {
                                    obj.insert(
                                        "type".to_string(),
                                        Value::String("thinking".to_string()),
                                    );
                                }
                                if let Some(t) = &b.text {
                                    obj.insert("thinking".to_string(), Value::String(t.clone()));
                                }
                                if let Some(s) = &b.signature {
                                    obj.insert("signature".to_string(), Value::String(s.clone()));
                                }
                                if let Some(op) = &b.opaque_payload {
                                    obj.insert("data".to_string(), Value::String(op.clone()));
                                }
                                Value::Object(obj)
                            })
                            .collect();
                        msg.insert("thinking".to_string(), Value::Array(thinking_items));
                    }
                }
                if !a.tool_calls.is_empty() {
                    let tc_list: Vec<Value> = a
                        .tool_calls
                        .iter()
                        .map(|tc| {
                            json!({
                                "id": tc.call_id.as_str(),
                                "type": "function",
                                "function": {
                                    "name": tc.name,
                                    "arguments": tc.raw_arguments,
                                }
                            })
                        })
                        .collect();
                    msg.insert("tool_calls".to_string(), Value::Array(tc_list));
                }
                wire_messages.push(Value::Object(msg));
            }
            TranscriptItem::ToolResult(tr) => {
                wire_messages.push(json!({
                    "role": "tool",
                    "tool_call_id": tr.call_id.as_str(),
                    "name": tr.tool_name,
                    "content": tr.content.as_ref(),
                }));
            }
        }
    }

    let mut root = serde_json::Map::new();
    root.insert("model".to_string(), Value::String(model.to_string()));
    root.insert("n".to_string(), Value::Number(1.into()));
    if let Some(effort) = reasoning_effort.map(str::trim).filter(|s| !s.is_empty()) {
        root.insert(
            "reasoning_effort".to_string(),
            Value::String(effort.to_string()),
        );
    }
    root.insert("messages".to_string(), Value::Array(wire_messages));
    if !tools_json.is_empty() {
        root.insert("tools".to_string(), Value::Array(tools_json));
    }
    root.insert("stream".to_string(), Value::Bool(stream));
    if stream {
        root.insert(
            "stream_options".to_string(),
            json!({ "include_usage": true }),
        );
    }

    serde_json::to_string(&Value::Object(root)).unwrap_or_else(|_| "{}".to_string())
}

pub fn parse_non_streaming_response(body: &str) -> Result<AssistantTurn, ProviderError> {
    let val: Value = serde_json::from_str(body).map_err(|e| {
        ProviderError::Transport(format!("invalid JSON in non-stream response: {}", e))
    })?;

    let choices = val
        .get("choices")
        .and_then(|c| c.as_array())
        .ok_or_else(|| ProviderError::Transport("missing choices array in response".to_string()))?;

    let choice0 = choices
        .first()
        .ok_or_else(|| ProviderError::Transport("empty choices array in response".to_string()))?;

    let finish_reason = choice0
        .get("finish_reason")
        .and_then(|v| v.as_str())
        .map(FinishReason::from_wire)
        .unwrap_or(FinishReason::Unknown("missing".to_string()));

    let message = choice0
        .get("message")
        .ok_or_else(|| ProviderError::Transport("missing message in choice".to_string()))?;

    let content = message
        .get("content")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let mut reasoning_blocks: Vec<ReasoningBlock> = Vec::new();
    if let Some(arr) = message
        .get("reasoning_blocks")
        .or_else(|| message.get("thinking").filter(|v| v.is_array()))
        .and_then(|v| v.as_array())
    {
        for item in arr {
            let text = item
                .get("text")
                .or_else(|| item.get("thinking"))
                .or_else(|| item.get("reasoning_content"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let signature = item
                .get("signature")
                .or_else(|| item.get("thought_signature"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let opaque_payload = item
                .get("opaque_payload")
                .or_else(|| item.get("data"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            if text.is_some() || signature.is_some() || opaque_payload.is_some() {
                reasoning_blocks.push(ReasoningBlock {
                    text,
                    signature,
                    opaque_payload,
                });
            }
        }
    } else {
        let text = message
            .get("reasoning_content")
            .or_else(|| message.get("reasoning"))
            .or_else(|| message.get("thinking"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let signature = message
            .get("signature")
            .or_else(|| message.get("thought_signature"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let opaque_payload = message
            .get("opaque_payload")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        if text.is_some() || signature.is_some() || opaque_payload.is_some() {
            reasoning_blocks.push(ReasoningBlock {
                text,
                signature,
                opaque_payload,
            });
        }
    }

    let mut tool_calls = Vec::new();
    if let Some(tc_arr) = message.get("tool_calls").and_then(|v| v.as_array()) {
        for tc in tc_arr {
            let id = tc
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let func = tc.get("function");
            let name = func
                .and_then(|f| f.get("name"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let raw_arguments = func
                .and_then(|f| f.get("arguments"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            tool_calls.push(ToolCall {
                call_id: ToolCallId::new(id),
                name,
                raw_arguments,
            });
        }
    }

    let usage = val.get("usage").and_then(parse_usage_json);

    Ok(AssistantTurn {
        content,
        reasoning_blocks,
        tool_calls,
        finish_reason,
        usage,
    })
}

/// OpenAI-compatible HTTP/1.1 + SSE Client supporting bounded retries, `Retry-After`,
/// redirect prohibition, and URL security validation (§4.1–§4.3).
pub struct ChatCompletionsClient {
    config: ChatCompletionsConfig,
    endpoint: ValidatedEndpoint,
}

impl ChatCompletionsClient {
    pub fn new(config: ChatCompletionsConfig) -> Result<Self, ProviderError> {
        // Reject CR/LF in values that reach the HTTP request line/headers to prevent
        // header injection: `api_key` is written into the `Authorization` header and
        // `model` is interpolated into the request body.
        // Reject any ASCII control characters (including CR, LF, NUL, DEL) in values that reach
        // HTTP headers or the request body.
        if config.api_key.bytes().any(|b| b < 0x20 || b == 0x7f) {
            return Err(ProviderError::Transport(
                "api_key must not contain ASCII control characters".to_string(),
            ));
        }
        if config.model.bytes().any(|b| b < 0x20 || b == 0x7f) {
            return Err(ProviderError::Transport(
                "model must not contain ASCII control characters".to_string(),
            ));
        }
        let endpoint = validate_base_url(&config.base_url)?;
        Ok(Self { config, endpoint })
    }

    pub fn endpoint(&self) -> &ValidatedEndpoint {
        &self.endpoint
    }
}

impl ModelProvider for ChatCompletionsClient {
    fn complete<'a>(
        &'a self,
        req: ProviderRequest,
        budget: &'a BudgetConfig,
        mut cancel_rx: watch::Receiver<bool>,
    ) -> Pin<Box<dyn Future<Output = Result<ProviderCompletion, ProviderError>> + Send + 'a>> {
        Box::pin(async move {
            let body_json = serialize_openai_request_body_with_effort(
                &self.config.model,
                &req.system_prompt,
                &req.tools,
                &req.messages,
                req.stream,
                self.config.reasoning_effort.as_deref(),
            );
            let request_body_bytes = body_json.len();

            let max_attempts = (self.config.max_retries + 1).min(budget.max_model_request_attempts);
            let mut attempts_used = 0usize;

            loop {
                if *cancel_rx.borrow() {
                    return Err(ProviderError::Cancelled);
                }
                attempts_used += 1;

                let attempt_start = Instant::now();
                let outcome = tokio::select! {
                    res = self.execute_single_http_attempt(&body_json, req.stream, budget, attempt_start) => res,
                    _ = async {
                        loop {
                            if cancel_rx.changed().await.is_err() {
                                std::future::pending::<()>().await;
                            }
                            if *cancel_rx.borrow() {
                                break;
                            }
                        }
                    } => Err(AttemptFailure::Fatal(ProviderError::Cancelled)),
                };

                match outcome {
                    Ok((turn, latency_ms, ttft_ms)) => {
                        return Ok(ProviderCompletion {
                            turn,
                            request_body_bytes,
                            latency_ms,
                            ttft_ms,
                            attempts_used,
                        });
                    }
                    Err(AttemptFailure::Fatal(err)) => {
                        return Err(err);
                    }
                    Err(AttemptFailure::Retryable { err, retry_after }) => {
                        if attempts_used >= max_attempts {
                            return Err(err);
                        }
                        let backoff = retry_after.unwrap_or_else(|| {
                            Duration::from_millis(50 * (1u64 << (attempts_used - 1)))
                        });
                        tokio::select! {
                            _ = tokio::time::sleep(backoff) => {}
                            _ = async {
                                loop {
                                    if cancel_rx.changed().await.is_err() {
                                        std::future::pending::<()>().await;
                                    }
                                    if *cancel_rx.borrow() {
                                        break;
                                    }
                                }
                            } => return Err(ProviderError::Cancelled),
                        }
                    }
                }
            }
        })
    }
}

enum AttemptFailure {
    Fatal(ProviderError),
    Retryable {
        err: ProviderError,
        retry_after: Option<Duration>,
    },
}

impl ChatCompletionsClient {
    async fn execute_single_http_attempt(
        &self,
        body_json: &str,
        stream: bool,
        budget: &BudgetConfig,
        attempt_start: Instant,
    ) -> Result<(AssistantTurn, u64, Option<u64>), AttemptFailure> {
        let request_deadline = attempt_start + budget.request_timeout;
        let step_timeout = |preferred: Duration| -> Result<Duration, ProviderError> {
            let now = Instant::now();
            if now >= request_deadline {
                return Err(ProviderError::Timeout);
            }
            Ok(preferred.min(request_deadline.duration_since(now)))
        };

        let connect_addr = format!("{}:{}", self.endpoint.host, self.endpoint.port);
        let connect_to = step_timeout(budget.connect_timeout).map_err(|err| {
            AttemptFailure::Retryable {
                err,
                retry_after: None,
            }
        })?;
        let tcp = match tokio::time::timeout(connect_to, TcpStream::connect(&connect_addr)).await {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => {
                return Err(AttemptFailure::Retryable {
                    err: ProviderError::Transport(format!("connect failed: {}", e)),
                    retry_after: None,
                });
            }
            Err(_) => {
                return Err(AttemptFailure::Retryable {
                    err: ProviderError::Timeout,
                    retry_after: None,
                });
            }
        };

        let mut transport = if self.endpoint.scheme == "https" {
            let tls_to = step_timeout(budget.connect_timeout).map_err(|err| {
                AttemptFailure::Retryable {
                    err,
                    retry_after: None,
                }
            })?;
            match tokio::time::timeout(
                tls_to,
                PlatformTlsStream::connect(tcp, &self.endpoint.host),
            )
            .await
            {
                Ok(Ok(tls)) => MaybeTlsStream::Tls(tls),
                Ok(Err(e)) => {
                    return Err(AttemptFailure::Retryable {
                        err: ProviderError::Transport(format!("TLS handshake failed: {}", e)),
                        retry_after: None,
                    });
                }
                Err(_) => {
                    return Err(AttemptFailure::Retryable {
                        err: ProviderError::Timeout,
                        retry_after: None,
                    });
                }
            }
        } else {
            MaybeTlsStream::Plain(tcp)
        };

        let path = if self.endpoint.base_path.ends_with("/chat/completions") {
            self.endpoint.base_path.clone()
        } else if self.endpoint.base_path.ends_with("/v1") {
            format!("{}/chat/completions", self.endpoint.base_path)
        } else {
            format!("{}/v1/chat/completions", self.endpoint.base_path)
        };

        let accept_hdr = if stream {
            "text/event-stream"
        } else {
            "application/json"
        };
        let is_default_port = (self.endpoint.scheme == "http" && self.endpoint.port == 80)
            || (self.endpoint.scheme == "https" && self.endpoint.port == 443);
        let host_hdr = if is_default_port {
            self.endpoint.host.clone()
        } else {
            format!("{}:{}", self.endpoint.host, self.endpoint.port)
        };
        let req_wire = format!(
            "POST {} HTTP/1.1\r\n\
             Host: {}\r\n\
             Authorization: Bearer {}\r\n\
             Content-Type: application/json\r\n\
             Accept: {}\r\n\
             Connection: close\r\n\
             Content-Length: {}\r\n\r\n{}",
            path,
            host_hdr,
            self.config.api_key,
            accept_hdr,
            body_json.len(),
            body_json
        );

        let write_to = step_timeout(budget.idle_timeout).map_err(|err| {
            AttemptFailure::Retryable {
                err,
                retry_after: None,
            }
        })?;
        match tokio::time::timeout(write_to, transport.write_all(req_wire.as_bytes())).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                return Err(AttemptFailure::Retryable {
                    err: ProviderError::Transport(format!("write failed: {}", e)),
                    retry_after: None,
                });
            }
            Err(_) => {
                return Err(AttemptFailure::Retryable {
                    err: ProviderError::Timeout,
                    retry_after: None,
                });
            }
        }

        let mut reader = BufReader::new(transport);
        let status_line = read_line_bounded(
            &mut reader,
            MAX_HTTP_HEADER_LINE_BYTES,
            budget.idle_timeout,
            request_deadline,
        )
        .await
        .map_err(|err| AttemptFailure::Retryable {
            err,
            retry_after: None,
        })?;
        if status_line.is_empty() {
            return Err(AttemptFailure::Retryable {
                err: ProviderError::Transport("server closed connection before status".into()),
                retry_after: None,
            });
        }

        let status_code = parse_http_status_code(&status_line).ok_or_else(|| {
            AttemptFailure::Fatal(ProviderError::Transport(format!(
                "invalid status line: {}",
                status_line.trim()
            )))
        })?;

        let mut retry_after_hdr: Option<Duration> = None;
        let mut is_chunked = false;
        let mut content_length: Option<usize> = None;
        let mut header_count = 0usize;

        loop {
            header_count += 1;
            if header_count > MAX_HTTP_HEADERS_COUNT {
                return Err(AttemptFailure::Fatal(ProviderError::Transport(
                    "HTTP response exceeded max header count".to_string(),
                )));
            }
            let hdr_line = read_line_bounded(
                &mut reader,
                MAX_HTTP_HEADER_LINE_BYTES,
                budget.idle_timeout,
                request_deadline,
            )
            .await
            .map_err(|err| AttemptFailure::Retryable {
                err,
                retry_after: None,
            })?;
            if hdr_line.is_empty() || hdr_line == "\r\n" || hdr_line == "\n" {
                break;
            }
            if let Some((k, v)) = hdr_line.split_once(':') {
                let key = k.trim().to_ascii_lowercase();
                let val = v.trim();
                if key == "retry-after" {
                    if let Ok(secs) = val.parse::<u64>() {
                        retry_after_hdr = Some(Duration::from_secs(secs));
                    } else if let Ok(ms) = val.strip_suffix("ms").unwrap_or("").parse::<u64>() {
                        retry_after_hdr = Some(Duration::from_millis(ms));
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
                        return Err(AttemptFailure::Fatal(ProviderError::Transport(format!(
                            "unsupported or ambiguous Transfer-Encoding: {}",
                            val
                        ))));
                    }
                    if is_chunked {
                        return Err(AttemptFailure::Fatal(ProviderError::Transport(
                            "duplicate Transfer-Encoding header".to_string(),
                        )));
                    }
                    is_chunked = true;
                } else if key == "content-length" {
                    // F-04: parse the value; reject non-numeric and conflicting duplicates.
                    let parsed = val.parse::<usize>().map_err(|_| {
                        AttemptFailure::Fatal(ProviderError::Transport(format!(
                            "invalid Content-Length header value: {}",
                            val
                        )))
                    })?;
                    if let Some(existing) = content_length {
                        if existing != parsed {
                            return Err(AttemptFailure::Fatal(ProviderError::Transport(
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
            return Err(AttemptFailure::Fatal(ProviderError::Transport(
                "HTTP response contains both Transfer-Encoding: chunked and Content-Length"
                    .to_string(),
            )));
        }

        // §4.3: Never follow redirects (301, 302, 303, 307, 308)
        if matches!(status_code, 301 | 302 | 303 | 307 | 308) {
            return Err(AttemptFailure::Fatal(ProviderError::RedirectBlocked(
                status_code,
            )));
        }

        // §4.3: Retryable status codes: 429, 502, 503, 504
        if matches!(status_code, 429 | 502 | 503 | 504) {
            let err_body = if is_chunked {
                let mut decoded = Vec::new();
                let _ = read_chunked_body(
                    &mut reader,
                    &mut decoded,
                    budget.idle_timeout,
                    request_deadline,
                )
                .await;
                decoded.truncate(MAX_ERROR_BODY_BYTES);
                decoded
            } else {
                read_body_bounded(
                    &mut reader,
                    MAX_ERROR_BODY_BYTES,
                    budget.idle_timeout,
                    request_deadline,
                )
                .await
                .unwrap_or_default()
            };
            let body_str = String::from_utf8_lossy(&err_body).to_string();
            return Err(AttemptFailure::Retryable {
                err: ProviderError::HttpStatus {
                    status: status_code,
                    body: body_str,
                },
                retry_after: retry_after_hdr,
            });
        }

        if status_code != 200 {
            let err_body = if is_chunked {
                let mut decoded = Vec::new();
                let _ = read_chunked_body(
                    &mut reader,
                    &mut decoded,
                    budget.idle_timeout,
                    request_deadline,
                )
                .await;
                decoded.truncate(MAX_ERROR_BODY_BYTES);
                decoded
            } else {
                read_body_bounded(
                    &mut reader,
                    MAX_ERROR_BODY_BYTES,
                    budget.idle_timeout,
                    request_deadline,
                )
                .await
                .unwrap_or_default()
            };
            let body_str = String::from_utf8_lossy(&err_body).to_string();
            return Err(AttemptFailure::Fatal(ProviderError::HttpStatus {
                status: status_code,
                body: body_str,
            }));
        }

        if !stream {
            let mut body_bytes = Vec::new();
            if is_chunked {
                read_chunked_body(
                    &mut reader,
                    &mut body_bytes,
                    budget.idle_timeout,
                    request_deadline,
                )
                .await
                .map_err(AttemptFailure::Fatal)?;
            } else {
                let read_cap = match content_length {
                    Some(len) => len.min(MAX_SSE_RESPONSE_BYTES),
                    None => MAX_SSE_RESPONSE_BYTES,
                };
                body_bytes = read_body_bounded(
                    &mut reader,
                    read_cap,
                    budget.idle_timeout,
                    request_deadline,
                )
                .await
                .map_err(AttemptFailure::Fatal)?;
                // F-04: a declared Content-Length must be satisfied exactly; a short read means the
                // peer closed early (premature EOF) and the body must not be treated as complete.
                if let Some(len) = content_length {
                    if body_bytes.len() < len {
                        return Err(AttemptFailure::Fatal(ProviderError::IncompleteStream(
                            format!(
                                "HTTP body ended prematurely: expected {} bytes, received {}",
                                len,
                                body_bytes.len()
                            ),
                        )));
                    }
                }
            }
            let body_str = std::str::from_utf8(&body_bytes)
                .map_err(|e| AttemptFailure::Fatal(ProviderError::Transport(e.to_string())))?;
            let turn = parse_non_streaming_response(body_str).map_err(AttemptFailure::Fatal)?;
            let latency_ms = attempt_start.elapsed().as_millis() as u64;
            return Ok((turn, latency_ms, Some(latency_ms)));
        }

        // Streaming SSE mode: once any business chunk is observed, mid-stream failure is NEVER retried (§4.3).
        // If failure occurs before any business chunk is observed, it is Retryable.
        let mut assembler = SseStreamAssembler::new();
        let mut ttft_ms: Option<u64> = None;
        let classify_sse_err = |saw_business: bool, err: ProviderError| -> AttemptFailure {
            if saw_business {
                AttemptFailure::Fatal(err)
            } else {
                AttemptFailure::Retryable {
                    err,
                    retry_after: None,
                }
            }
        };

        if is_chunked {
            let mut total_decoded = 0usize;
            let mut buf = [0u8; 4096];
            loop {
                let size_line = read_line_bounded(
                    &mut reader,
                    MAX_HTTP_HEADER_LINE_BYTES,
                    budget.idle_timeout,
                    request_deadline,
                )
                .await
                .map_err(|e| {
                    classify_sse_err(
                        assembler.saw_any_business_chunk(),
                        ProviderError::IncompleteStream(format!("SSE chunked size read error: {}", e)),
                    )
                })?;
                if size_line.is_empty() {
                    return Err(classify_sse_err(
                        assembler.saw_any_business_chunk(),
                        ProviderError::IncompleteStream(
                            "unexpected EOF before terminating 0 chunk in SSE stream".to_string(),
                        ),
                    ));
                }
                let trimmed = size_line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let hex_part = trimmed.split(';').next().unwrap_or("");
                let chunk_size = usize::from_str_radix(hex_part, 16).map_err(|e| {
                    classify_sse_err(
                        assembler.saw_any_business_chunk(),
                        ProviderError::IncompleteStream(format!("invalid HTTP chunk size: {}", e)),
                    )
                })?;
                if chunk_size > MAX_SSE_RESPONSE_BYTES
                    || total_decoded.saturating_add(chunk_size) > MAX_SSE_RESPONSE_BYTES
                {
                    return Err(AttemptFailure::Fatal(ProviderError::IncompleteStream(
                        format!(
                            "HTTP chunk size ({}) exceeds MAX_SSE_RESPONSE_BYTES ({})",
                            chunk_size, MAX_SSE_RESPONSE_BYTES
                        ),
                    )));
                }
                if chunk_size == 0 {
                    // Drain trailing CRLF / trailer headers
                    let _ = read_line_bounded(
                        &mut reader,
                        MAX_HTTP_HEADER_LINE_BYTES,
                        budget.idle_timeout,
                        request_deadline,
                    )
                    .await;
                    break;
                }

                let mut remaining = chunk_size;
                while remaining > 0 {
                    let to_read = remaining.min(buf.len());
                    let read_to = step_timeout(budget.idle_timeout).map_err(|_| {
                        classify_sse_err(
                            assembler.saw_any_business_chunk(),
                            ProviderError::IncompleteStream("SSE stream timeout".to_string()),
                        )
                    })?;
                    match tokio::time::timeout(read_to, reader.read(&mut buf[..to_read])).await {
                        Ok(Ok(0)) => {
                            return Err(classify_sse_err(
                                assembler.saw_any_business_chunk(),
                                ProviderError::IncompleteStream(
                                    "unexpected EOF inside HTTP chunk".to_string(),
                                ),
                            ));
                        }
                        Ok(Ok(n)) => {
                            if ttft_ms.is_none() {
                                ttft_ms = Some(attempt_start.elapsed().as_millis() as u64);
                            }
                            remaining -= n;
                            total_decoded += n;
                            assembler
                                .feed_bytes(&buf[..n])
                                .map_err(AttemptFailure::Fatal)?;
                        }
                        Ok(Err(e)) => {
                            return Err(classify_sse_err(
                                assembler.saw_any_business_chunk(),
                                ProviderError::IncompleteStream(format!(
                                    "SSE stream read error: {}",
                                    e
                                )),
                            ));
                        }
                        Err(_) => {
                            return Err(classify_sse_err(
                                assembler.saw_any_business_chunk(),
                                ProviderError::IncompleteStream(
                                    "SSE stream idle timeout".to_string(),
                                ),
                            ));
                        }
                    }
                }

                // Consume trailing \r\n after chunk data
                let mut crlf = [0u8; 2];
                let crlf_to = step_timeout(budget.idle_timeout).map_err(|_| {
                    classify_sse_err(
                        assembler.saw_any_business_chunk(),
                        ProviderError::IncompleteStream("SSE stream timeout".to_string()),
                    )
                })?;
                if tokio::time::timeout(crlf_to, reader.read_exact(&mut crlf))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        } else {
            let mut total_read = 0usize;
            let mut buf = [0u8; 4096];
            loop {
                if let Some(len) = content_length {
                    if total_read >= len {
                        break;
                    }
                }
                let max_to_read = match content_length {
                    Some(len) => (len - total_read).min(buf.len()),
                    None => buf.len(),
                };
                let read_to = step_timeout(budget.idle_timeout).map_err(|_| {
                    classify_sse_err(
                        assembler.saw_any_business_chunk(),
                        ProviderError::IncompleteStream("SSE stream timeout".to_string()),
                    )
                })?;
                let read_res =
                    tokio::time::timeout(read_to, reader.read(&mut buf[..max_to_read])).await;
                match read_res {
                    Ok(Ok(0)) => break,
                    Ok(Ok(n)) => {
                        total_read += n;
                        if ttft_ms.is_none() {
                            ttft_ms = Some(attempt_start.elapsed().as_millis() as u64);
                        }
                        assembler
                            .feed_bytes(&buf[..n])
                            .map_err(AttemptFailure::Fatal)?;
                    }
                    Ok(Err(e)) => {
                        return Err(classify_sse_err(
                            assembler.saw_any_business_chunk(),
                            ProviderError::IncompleteStream(format!(
                                "SSE stream read error: {}",
                                e
                            )),
                        ));
                    }
                    Err(_) => {
                        return Err(classify_sse_err(
                            assembler.saw_any_business_chunk(),
                            ProviderError::IncompleteStream("SSE stream idle timeout".to_string()),
                        ));
                    }
                }
            }
            if let Some(len) = content_length {
                if total_read < len {
                    return Err(classify_sse_err(
                        assembler.saw_any_business_chunk(),
                        ProviderError::IncompleteStream(format!(
                            "SSE body ended prematurely: expected {} bytes, received {}",
                            len, total_read
                        )),
                    ));
                }
            }
        }

        let saw_business = assembler.saw_any_business_chunk();
        let turn = assembler
            .finalize()
            .map_err(|e| classify_sse_err(saw_business, e))?;
        let latency_ms = attempt_start.elapsed().as_millis() as u64;
        Ok((turn, latency_ms, ttft_ms))
    }
}



fn parse_http_status_code(status_line: &str) -> Option<u16> {
    let mut parts = status_line.split_whitespace();
    let _http_ver = parts.next()?;
    let code_str = parts.next()?;
    code_str.parse::<u16>().ok()
}

async fn read_line_bounded<R: AsyncBufReadExt + Unpin>(
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

async fn read_body_bounded<R: AsyncRead + Unpin>(
    reader: &mut R,
    max_bytes: usize,
    idle_timeout: Duration,
    request_deadline: Instant,
) -> Result<Vec<u8>, ProviderError> {
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let now = Instant::now();
        if now >= request_deadline {
            return Err(ProviderError::Timeout);
        }
        let to = idle_timeout.min(request_deadline.duration_since(now));
        match tokio::time::timeout(to, reader.read(&mut buf)).await {
            Ok(Ok(0)) => break,
            Ok(Ok(n)) => {
                if out.len().saturating_add(n) > max_bytes {
                    return Err(ProviderError::Transport(format!(
                        "HTTP response body exceeded max size ({} bytes)",
                        max_bytes
                    )));
                }
                out.extend_from_slice(&buf[..n]);
            }
            Ok(Err(e)) => return Err(ProviderError::Transport(e.to_string())),
            Err(_) => return Err(ProviderError::Timeout),
        }
    }
    Ok(out)
}

async fn read_chunked_body<R: AsyncBufReadExt + Unpin>(
    reader: &mut R,
    out: &mut Vec<u8>,
    idle_timeout: Duration,
    request_deadline: Instant,
) -> Result<(), ProviderError> {
    let mut buf = [0u8; 4096];
    loop {
        let size_line = read_line_bounded(
            reader,
            MAX_HTTP_HEADER_LINE_BYTES,
            idle_timeout,
            request_deadline,
        )
        .await?;
        if size_line.is_empty() {
            break;
        }
        let trimmed = size_line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let hex_part = trimmed.split(';').next().unwrap_or("");
        let chunk_size = usize::from_str_radix(hex_part, 16)
            .map_err(|e| ProviderError::Transport(format!("invalid chunk size: {}", e)))?;
        if chunk_size > MAX_SSE_RESPONSE_BYTES
            || out.len().saturating_add(chunk_size) > MAX_SSE_RESPONSE_BYTES
        {
            return Err(ProviderError::Transport(format!(
                "HTTP chunk size ({}) exceeds max response size ({})",
                chunk_size, MAX_SSE_RESPONSE_BYTES
            )));
        }
        if chunk_size == 0 {
            let _ = read_line_bounded(
                reader,
                MAX_HTTP_HEADER_LINE_BYTES,
                idle_timeout,
                request_deadline,
            )
            .await;
            break;
        }
        let mut remaining = chunk_size;
        while remaining > 0 {
            let now = Instant::now();
            if now >= request_deadline {
                return Err(ProviderError::Timeout);
            }
            let to = idle_timeout.min(request_deadline.duration_since(now));
            let to_read = remaining.min(buf.len());
            match tokio::time::timeout(to, reader.read(&mut buf[..to_read])).await {
                Ok(Ok(0)) => {
                    return Err(ProviderError::Transport(
                        "unexpected EOF inside HTTP chunk".to_string(),
                    ));
                }
                Ok(Ok(n)) => {
                    out.extend_from_slice(&buf[..n]);
                    remaining -= n;
                }
                Ok(Err(e)) => return Err(ProviderError::Transport(e.to_string())),
                Err(_) => return Err(ProviderError::Timeout),
            }
        }
        let now = Instant::now();
        if now >= request_deadline {
            return Err(ProviderError::Timeout);
        }
        let to = idle_timeout.min(request_deadline.duration_since(now));
        let mut crlf = [0u8; 2];
        let _ = tokio::time::timeout(to, reader.read_exact(&mut crlf)).await;
    }
    Ok(())
}
