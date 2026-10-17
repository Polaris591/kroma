//! Bounded multi-session daemon control plane.
//!
//! A `kroma serve` daemon can host several independent sessions at once. Each session is backed by
//! its own [`RpcDispatcher`] runtime (its own session WAL, workspace capability and provider), and
//! this type routes an incoming JSON-RPC request to the runtime that owns the session named in the
//! request. Two sessions therefore never share transcript, revision or cancel state, and a request
//! that names a session this daemon does not hold is rejected with a structured error instead of
//! mutating the default session by mistake.
//!
//! * **Bounded** — [`SessionManagerConfig::max_sessions`] caps how many live runtimes a daemon holds.
//! * **Persistent** — the session id → workspace/WAL mapping is written to
//!   [`SessionManagerConfig::index_path`] and rebuilt on [`SessionManager::open`], so a daemon
//!   restart recovers the sessions it was hosting.
//! * **Writer-safe** — the cross-process workspace writer lock (`crate::session_ownership`) still
//!   arbitrates writers, so two sessions bound to the same workspace serialise their runs while
//!   sessions bound to different workspaces run in parallel.
//! * **Single-user** — there is no multi-user isolation between sessions; all sessions run
//!   with the privileges and filesystem access of the OS user hosting the daemon.
//!
//! It intentionally does not implement daemon-to-daemon forwarding or automatic peer discovery;
//! a session owned by another daemon is refused, never guessed at.

use super::*;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncRead, AsyncWrite, BufReader};

/// Default upper bound on live sessions in one multi-session `kroma serve` daemon.
pub const DEFAULT_MAX_DAEMON_SESSIONS: usize = 8;

/// Upper bound on the on-disk session index, in bytes. The index holds one short entry per hosted
/// session, so a document anywhere near this size is corrupt or attacker-supplied; it is refused at
/// load time instead of being read into memory.
pub const MAX_SESSION_INDEX_BYTES: u64 = 1024 * 1024;

/// Builds one independent runtime for a session. The daemon supplies the provider wiring; the
/// manager owns routing, persistence and the session bound.
pub trait SessionRuntimeFactory: Send + Sync {
    /// Build a runtime bound to `workspace_root`. When `resume_session_path` is `Some`, the runtime
    /// resumes that WAL; otherwise it starts a fresh session.
    fn build(
        &self,
        workspace_root: &Path,
        resume_session_path: Option<&Path>,
    ) -> Result<RpcDispatcher, String>;
}

/// Persisted metadata for one hosted session, enough to rebuild its runtime after a restart.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionIndexEntry {
    pub session_id: String,
    pub workspace_root: String,
    pub session_path: String,
}

/// The persisted session index document.
///
/// `generation` advances on every successful write. It is the durable counterpart of the in-memory
/// publish: recovery reads the generation alongside the entries, so a daemon can tell the index it
/// loaded from a stale or truncated one, and a legacy bare-array index (written before this field
/// existed) is read as generation `0` and upgraded by the next write.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SessionIndexDocument {
    generation: u64,
    entries: Vec<SessionIndexEntry>,
}

/// Configuration for [`SessionManager`].
pub struct SessionManagerConfig {
    pub factory: Arc<dyn SessionRuntimeFactory>,
    /// Upper bound on live runtimes; a `session.new` past the bound is refused.
    pub max_sessions: usize,
    /// Where the session index is persisted (created if missing).
    pub index_path: PathBuf,
    /// Workspace used for a `session.new` that does not name one.
    pub default_workspace: PathBuf,
}

struct SessionManagerInner {
    factory: Arc<dyn SessionRuntimeFactory>,
    max_sessions: usize,
    index_path: PathBuf,
    default_workspace: PathBuf,
    /// Upper bound on the serialized index this daemon will write. It matches the read cap
    /// ([`MAX_SESSION_INDEX_BYTES`]) so the daemon can never persist a document it would then refuse
    /// to read back on restart; kept in a cell so the boundary is testable with a low cap.
    max_index_bytes: AtomicU64,
    /// Auth token required by the daemon's stream, copied from the primary runtime so every routed
    /// session is gated the same way.
    auth_token: Option<String>,
    primary: RpcDispatcher,
    state: Mutex<SessionState>,
}

/// A capacity slot held by an in-flight [`SessionManager::open_session`].
///
/// It releases the slot on drop, so a runtime build that returns `Err` **or panics** cannot leak a
/// reservation; the commit path calls [`ReservationGuard::commit`] to release the slot exactly once
/// under the same lock that publishes the session. `Drop` runs with no state lock held (the caller
/// only holds one inside `commit`), takes the lock poison-tolerantly, and never panics, so it is
/// safe during unwinding.
struct ReservationGuard<'a> {
    inner: &'a SessionManagerInner,
    committed: bool,
}

impl<'a> ReservationGuard<'a> {
    fn new(inner: &'a SessionManagerInner) -> Self {
        Self {
            inner,
            committed: false,
        }
    }

    /// Commit the reservation into a live session: release the slot exactly once, under the caller's
    /// state lock, and suppress the drop-time release.
    fn commit(&mut self, state: &mut SessionState) {
        state.reserved = state.reserved.saturating_sub(1);
        self.committed = true;
    }
}

impl Drop for ReservationGuard<'_> {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        let mut state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
        state.reserved = state.reserved.saturating_sub(1);
    }
}

#[derive(Default)]
struct SessionState {
    runtimes: BTreeMap<String, RpcDispatcher>,
    index: BTreeMap<String, SessionIndexEntry>,
    /// Generation of the last successfully persisted index; `0` means never persisted (a legacy
    /// bare-array index or a fresh daemon).
    generation: u64,
    /// Capacity slots held by an in-flight `open_session` before its runtime is built. Counted by
    /// [`SessionState::live_count`] so concurrent opens cannot all pass the bound check and then
    /// overshoot it once their (slow) builds finish.
    reserved: usize,
}

impl SessionState {
    /// Live sessions plus reserved-but-not-yet-built slots: the number the capacity bound caps.
    fn live_count(&self) -> usize {
        self.runtimes.len() + self.reserved
    }
}

/// Read the session index with the no-follow, size-bounded policy: a symlinked index is refused
/// (never followed), a missing index is `None`, and a document over [`MAX_SESSION_INDEX_BYTES`] is
/// refused rather than read into memory.
fn read_index_bounded(path: &Path) -> Result<Option<Vec<u8>>, String> {
    use std::io::Read;
    let file = match crate::secure_fs::open_existing_no_symlink(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(format!(
                "failed to read the session index {}: {}",
                path.display(),
                e
            ))
        }
    };
    let len = file
        .metadata()
        .map_err(|e| format!("failed to stat the session index {}: {}", path.display(), e))?
        .len();
    if len > MAX_SESSION_INDEX_BYTES {
        return Err(format!(
            "session index {} is {} bytes, over the {} byte cap",
            path.display(),
            len,
            MAX_SESSION_INDEX_BYTES
        ));
    }
    let mut bytes = Vec::with_capacity(len as usize);
    file.take(MAX_SESSION_INDEX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("failed to read the session index {}: {}", path.display(), e))?;
    if bytes.len() as u64 > MAX_SESSION_INDEX_BYTES {
        return Err(format!(
            "session index {} grew past the {} byte cap while being read",
            path.display(),
            MAX_SESSION_INDEX_BYTES
        ));
    }
    Ok(Some(bytes))
}

/// Parse a session index. The current format is a [`SessionIndexDocument`]; a bare array is the
/// pre-generation format written by an earlier daemon and is still accepted, reported as generation
/// `0` so the next write bumps it. Anything else is refused.
fn parse_index(bytes: &[u8]) -> Result<(u64, Vec<SessionIndexEntry>), String> {
    if let Ok(doc) = serde_json::from_slice::<SessionIndexDocument>(bytes) {
        return Ok((doc.generation, doc.entries));
    }
    match serde_json::from_slice::<Vec<SessionIndexEntry>>(bytes) {
        Ok(entries) => Ok((0, entries)),
        Err(e) => Err(e.to_string()),
    }
}

/// Refuse to persist an index larger than the cap the reader enforces, so the daemon can never
/// write a document it would then refuse to read back on restart. Split out so the boundary can be
/// exercised with a low cap as well as the production cap.
fn ensure_index_within_cap(bytes: &[u8], cap: u64) -> Result<(), JsonRpcError> {
    if bytes.len() as u64 > cap {
        return Err(JsonRpcError::new(
            JSONRPC_INTERNAL_ERROR,
            format!(
                "refusing to persist a {} byte session index over the {} byte cap: it could not be read back on restart",
                bytes.len(),
                cap
            ),
        ));
    }
    Ok(())
}

/// Replace the session index at `path` with `bytes` durably and privately: a `0700` parent, a
/// `0600` uniquely named sibling temp file (`O_CREAT|O_EXCL|O_NOFOLLOW`), an `fsync` of the file,
/// a `rename` over the target, then an `fsync` of the parent directory.
///
/// A failure before the `rename` leaves the previous index untouched and is returned as an error, so
/// the caller can roll back its in-memory publish and never report success for an unpersisted
/// session. A failure of the *directory* `fsync` after a successful `rename` is reported on stderr
/// but not returned as an error: the new index is already visible to every reader (and would be
/// recovered on restart), so failing the operation would strand a live session the client was told
/// failed. This does not promise that a lone `rename` resists a sudden power loss; the directory
/// sync is what makes the replacement durable.
fn write_index_atomically(path: &Path, bytes: &[u8]) -> Result<(), JsonRpcError> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    crate::secure_fs::ensure_private_dir(parent).map_err(|e| {
        JsonRpcError::new(
            JSONRPC_INTERNAL_ERROR,
            format!("failed to prepare {}: {}", parent.display(), e),
        )
    })?;
    let base = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("session-index.json");
    let tmp = parent.join(format!(".{}.tmp-{}", base, uuid::Uuid::new_v4()));
    let mut file = crate::secure_fs::create_new_no_symlink(&tmp, 0o600).map_err(|e| {
        JsonRpcError::new(
            JSONRPC_INTERNAL_ERROR,
            format!("failed to write {}: {}", tmp.display(), e),
        )
    })?;
    let write_result = (|| -> std::io::Result<()> {
        use std::io::Write;
        file.write_all(bytes)?;
        file.sync_all()
    })();
    drop(file);
    if let Err(e) = write_result {
        let _ = std::fs::remove_file(&tmp);
        return Err(JsonRpcError::new(
            JSONRPC_INTERNAL_ERROR,
            format!("failed to write {}: {}", tmp.display(), e),
        ));
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(JsonRpcError::new(
            JSONRPC_INTERNAL_ERROR,
            format!("failed to persist the session index {}: {}", path.display(), e),
        ));
    }
    if let Err(e) = crate::secure_fs::sync_dir(parent) {
        eprintln!(
            "[kroma] session index {} was replaced but its directory could not be synced: {}",
            path.display(),
            e
        );
    }
    Ok(())
}

/// B02a: Connection-scoped subscription guard. Tracks subscriber tokens for hosted runtimes.
/// On drop (EOF, error, cancel, or panic), all subscriber callbacks are unregistered, which
/// drops all clones of the outbound sender and allows the writer task to exit cleanly without
/// leaking background tasks.
pub(crate) struct SubscriptionGuard {
    subscriptions: Vec<(RpcDispatcher, u64)>,
}

impl SubscriptionGuard {
    pub(crate) fn new() -> Self {
        Self {
            subscriptions: Vec::new(),
        }
    }

    pub(crate) fn subscribe_session(
        &mut self,
        dispatcher: &RpcDispatcher,
        out_tx: &BoundedOutboundSender,
    ) {
        let tx = out_tx.clone();
        let sub_id = dispatcher.add_subscriber(Arc::new(move |note| {
            if let Ok(serialized) = serde_json::to_string(note) {
                tx.try_send_sync(serialized).is_ok()
            } else {
                true
            }
        }));
        self.subscriptions.push((dispatcher.clone(), sub_id));
    }
}

impl Drop for SubscriptionGuard {
    fn drop(&mut self) {
        for (dispatcher, sub_id) in self.subscriptions.drain(..) {
            dispatcher.remove_subscriber(sub_id);
        }
    }
}

/// The bounded, persistent multi-session control plane.
#[derive(Clone)]
pub struct SessionManager {
    inner: Arc<SessionManagerInner>,
}

fn entry_for(dispatcher: &RpcDispatcher) -> SessionIndexEntry {
    let ownership = dispatcher.ownership();
    SessionIndexEntry {
        session_id: ownership.session_id,
        workspace_root: ownership.workspace_root.display().to_string(),
        session_path: ownership.session_path.display().to_string(),
    }
}

/// The session id named by a request, if any.
pub(crate) fn requested_session_id(params: &Value) -> Option<String> {
    params
        .get("session_id")
        .or_else(|| params.get("sessionId"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

impl SessionManager {
    /// Build a manager around an already-created `primary` runtime (the daemon's own startup
    /// session) and recover any sessions persisted by a previous process.
    pub fn open(config: SessionManagerConfig, primary: RpcDispatcher) -> Result<Self, String> {
        let mut state = SessionState::default();
        let primary_entry = entry_for(&primary);
        state
            .index
            .insert(primary_entry.session_id.clone(), primary_entry.clone());
        state
            .runtimes
            .insert(primary_entry.session_id.clone(), primary.clone());

        let manager = Self {
            inner: Arc::new(SessionManagerInner {
                factory: config.factory,
                max_sessions: config.max_sessions.max(1),
                index_path: config.index_path,
                default_workspace: config.default_workspace,
                max_index_bytes: AtomicU64::new(MAX_SESSION_INDEX_BYTES),
                auth_token: primary.auth_token().map(str::to_string),
                primary,
                state: Mutex::new(state),
            }),
        };
        manager.recover_persisted()?;
        manager.persist_index().map_err(|e| e.message)?;
        Ok(manager)
    }

    /// Test seam: lower the persisted-index cap so the pre-write rejection and its rollback can be
    /// exercised without building a multi-megabyte index.
    #[cfg(all(test, unix))]
    fn set_index_byte_cap(&self, cap: u64) {
        self.inner.max_index_bytes.store(cap, Ordering::SeqCst);
    }

    /// Number of live runtimes the manager currently holds.
    pub fn len(&self) -> usize {
        self.inner
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .runtimes
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The session a request without an explicit id is routed to by default (primary session).
    pub fn default_session_id(&self) -> Option<String> {
        Some(self.inner.primary.session_id())
    }

    /// Session ids this daemon currently owns, sorted.
    pub fn session_ids(&self) -> Vec<String> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .runtimes
            .keys()
            .cloned()
            .collect()
    }

    /// The runtime that owns `session_id`, if this daemon holds it.
    pub fn runtime_for(&self, session_id: &str) -> Option<RpcDispatcher> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .runtimes
            .get(session_id)
            .cloned()
    }

    /// Every live runtime, in session-id order.
    pub fn all_runtimes(&self) -> Vec<RpcDispatcher> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .runtimes
            .values()
            .cloned()
            .collect()
    }

    /// The runtime the daemon was started with (its startup session).
    pub fn primary(&self) -> RpcDispatcher {
        self.inner.primary.clone()
    }

    /// A `shutdown` request to any session stops the whole daemon.
    pub fn is_shutting_down(&self) -> bool {
        self.inner.primary.is_shutting_down()
    }

    /// Route one daemon stream: each request goes to the runtime owning its session (via
    /// [`Self::handle_request`]) and notifications from every hosted session are multiplexed onto
    /// the stream. Mirrors [`RpcDispatcher::serve_stream`] for the multi-session case.
    pub async fn serve_stream<R, W>(&self, reader: R, writer: W) -> Result<(), String>
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        self.serve_stream_with_config(reader, writer, TransportConfig::default()).await
    }

    /// Route one daemon stream with configurable transport parameters (timeouts, capacities).
    pub async fn serve_stream_with_config<R, W>(
        &self,
        reader: R,
        writer: W,
        config: TransportConfig,
    ) -> Result<(), String>
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (out_tx, out_rx, mut state_rx, state) = bounded_outbound_channel(config);
        let writer_task = tokio::spawn(run_bounded_writer(out_rx, writer, state.clone(), state_rx.clone()));

        let mut sub_guard = SubscriptionGuard::new();
        let mut subscribed: BTreeSet<String> = BTreeSet::new();
        let mut authenticated = self.inner.auth_token.is_none();
        // B01/B02a: only an authenticated connection may subscribe to session notifications, so an
        // unauthenticated stream has no business-event outlet at all.
        if authenticated {
            self.sync_subscriptions(&mut sub_guard, &mut subscribed, &out_tx);
        }

        let primary_sid = self.inner.primary.session_id();
        let conn = ConnectionContext::with_default_session(None, Some(primary_sid.clone()));
        let mut buf_reader = BufReader::new(reader);
        let mut frame_reader = FrameReader::new(config.max_frame_bytes);
        let mut in_flight = tokio::task::JoinSet::new();

        let auth_deadline = tokio::time::Instant::now() + config.auth_handshake_timeout;
        let mut auth_failures: usize = 0;

        loop {
            if !matches!(*state_rx.borrow(), TransportState::Open) {
                break;
            }

            let has_active_work = conn.has_pending() || !in_flight.is_empty();
            let next_deadline = if !authenticated {
                auth_deadline
            } else if !has_active_work {
                tokio::time::Instant::now() + config.idle_timeout
            } else {
                tokio::time::Instant::now() + Duration::from_secs(86400 * 365)
            };

            let line_opt = tokio::select! {
                _ = state_rx.changed() => {
                    if !matches!(*state_rx.borrow(), TransportState::Open) {
                        break;
                    }
                    continue;
                }
                Some(join_res) = in_flight.join_next(), if !in_flight.is_empty() => {
                    if let Err(e) = join_res {
                        if e.is_panic() {
                            eprintln!("[kroma] prompt task panicked: {:?}", e);
                        }
                    }
                    continue;
                }
                read_res = tokio::time::timeout_at(next_deadline, frame_reader.read_frame(&mut buf_reader)) => {
                    match read_res {
                        Ok(line_res) => line_res,
                        Err(_timeout) => {
                            if !authenticated {
                                let resp = JsonRpcResponse::error(
                                    Value::Null,
                                    JsonRpcError::new(JSONRPC_UNAUTHORIZED, "authentication handshake timed out"),
                                );
                                if let Ok(s) = serde_json::to_string(&resp) {
                                    let _ = out_tx.try_send_sync(s);
                                }
                            }
                            break;
                        }
                    }
                }
            };

            let line = match line_opt {
                Ok(Some(l)) => l,
                Ok(None) => break,
                Err(frame_err) => {
                    let resp = JsonRpcResponse::error(
                        Value::Null,
                        JsonRpcError::new(JSONRPC_PARSE_ERROR, frame_err),
                    );
                    if let Ok(s) = serde_json::to_string(&resp) {
                        let _ = out_tx.try_send_sync(s);
                    }
                    break;
                }
            };
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            if looks_like_http_request(trimmed) {
                let resp = JsonRpcResponse::error(
                    Value::Null,
                    JsonRpcError::new(
                        JSONRPC_INVALID_REQUEST,
                        "HTTP requests are rejected on raw JSON-RPC transport",
                    ),
                );
                if let Ok(s) = serde_json::to_string(&resp) {
                    let _ = out_tx.try_send_sync(s);
                }
                break;
            }
            let req: JsonRpcRequest = match serde_json::from_str::<Value>(trimmed)
                .map_err(|e| e.to_string())
                .and_then(|v| serde_json::from_value(v).map_err(|e| e.to_string()))
            {
                Ok(r) => r,
                Err(e) => {
                    let resp = JsonRpcResponse::error(
                        Value::Null,
                        JsonRpcError::new(JSONRPC_PARSE_ERROR, format!("invalid JSON-RPC: {}", e)),
                    );
                    if let Ok(s) = serde_json::to_string(&resp) {
                        let _ = out_tx.try_send_sync(s);
                    }
                    continue;
                }
            };

            if !authenticated {
                let expected = self.inner.auth_token.as_deref().unwrap_or("");
                let provided_token = req
                    .params
                    .get("auth_token")
                    .or_else(|| req.params.get("token"))
                    .and_then(Value::as_str);

                if let Some(provided) = provided_token {
                    if constant_time_eq(provided.as_bytes(), expected.as_bytes()) {
                        authenticated = true;
                        self.sync_subscriptions(&mut sub_guard, &mut subscribed, &out_tx);
                    } else {
                        auth_failures += 1;
                        let resp = JsonRpcResponse::error(
                            req.id.clone().unwrap_or(Value::Null),
                            JsonRpcError::new(
                                JSONRPC_UNAUTHORIZED,
                                "unauthorized: missing or invalid auth_token",
                            ),
                        );
                        if let Ok(s) = serde_json::to_string(&resp) {
                            let _ = out_tx.try_send_sync(s);
                        }
                        if auth_failures >= MAX_AUTH_FAILURES {
                            break;
                        }
                        continue;
                    }
                } else {
                    let resp = JsonRpcResponse::error(
                        req.id.clone().unwrap_or(Value::Null),
                        JsonRpcError::new(
                            JSONRPC_UNAUTHORIZED,
                            "unauthorized: missing or invalid auth_token",
                        ),
                    );
                    if let Ok(s) = serde_json::to_string(&resp) {
                        let _ = out_tx.try_send_sync(s);
                    }
                    continue;
                }
            }

            let is_shutdown = matches!(req.method.as_str(), "shutdown" | "acp.shutdown");
            let is_async_prompt = matches!(
                req.method.as_str(),
                "session.start"
                    | "session.prompt"
                    | "acp.prompt"
                    | "session.tournament"
                    | "session/tournament"
            );

            let mut req = req;
            let target_sid = requested_session_id(&req.params)
                .or_else(|| conn.default_session())
                .unwrap_or_else(|| self.inner.primary.session_id());

            // Parameter normalization: rejects arrays / primitives; normalizes null to object and inserts session_id
            if let Err(err) = normalize_request_params(&mut req.params, &target_sid) {
                let resp = JsonRpcResponse::error(req.id.clone().unwrap_or(Value::Null), err);
                if let Ok(s) = serde_json::to_string(&resp) {
                    let _ = out_tx.try_send_sync(s);
                }
                continue;
            }

            // B12: remote durable-task control is refused before any routing/dispatch. There is no
            // persistent authority that can safely claim a task, so no mutating `tasks.control`
            // action is admitted over the network; reads and the trusted in-process entry are
            // unaffected.
            if super::dispatcher::is_mutating_task_control(&req.method, &req.params) {
                let resp = JsonRpcResponse::error(
                    req.id.clone().unwrap_or(Value::Null),
                    super::dispatcher::remote_task_control_refused(),
                );
                if let Ok(s) = serde_json::to_string(&resp) {
                    let _ = out_tx.try_send_sync(s);
                }
                continue;
            }

            // B11: route every cancel spelling through the target session's ownership guard before
            // the generic dispatch, so no alias can reach an unguarded cancel. A cancel naming a
            // session this daemon does not host is refused like any other misrouted request; a
            // cancel from a connection that does not own the target run is refused with a stable
            // `not_owner` error and changes nothing.
            if super::dispatcher::is_cancel_method(&req.method) {
                let id = req.id.clone().unwrap_or(Value::Null);
                let resp = match self.runtime_for(&target_sid) {
                    Some(dispatcher) => match dispatcher.rpc_session_cancel_from(&conn) {
                        Ok(val) => JsonRpcResponse::success(id, val),
                        Err(err) => JsonRpcResponse::error(id, err),
                    },
                    None => JsonRpcResponse::error(
                        id,
                        JsonRpcError::with_data(
                            JSONRPC_SESSION_NOT_OWNED,
                            format!(
                                "session '{}' is not owned by this daemon; it belongs to another \
                                 daemon or is unknown",
                                target_sid
                            ),
                            json!({ "session_id": target_sid }),
                        ),
                    ),
                };
                if let Ok(s) = serde_json::to_string(&resp) {
                    let _ = out_tx.try_send_sync(s);
                }
                continue;
            }

            if is_async_prompt {
                if in_flight.len() >= MAX_PENDING_REQUESTS_PER_CONNECTION {
                    let resp = JsonRpcResponse::error(
                        req.id.clone().unwrap_or(Value::Null),
                        JsonRpcError::new(
                            JSONRPC_INTERNAL_ERROR,
                            "too many in-flight requests on this connection",
                        ),
                    );
                    if let Ok(s) = serde_json::to_string(&resp) {
                        let _ = out_tx.try_send_sync(s);
                    }
                    continue;
                }
                let Some(request) = conn.begin_request(&target_sid) else {
                    let resp = JsonRpcResponse::error(
                        req.id.clone().unwrap_or(Value::Null),
                        JsonRpcError::new(
                            JSONRPC_INTERNAL_ERROR,
                            "too many in-flight requests on this connection",
                        ),
                    );
                    if let Ok(s) = serde_json::to_string(&resp) {
                        let _ = out_tx.try_send_sync(s);
                    }
                    continue;
                };
                let guard = RequestGuard::new(conn.clone(), request.seq);
                let manager = self.clone();
                let tx = out_tx.clone();
                let conn_for_task = conn.clone();
                in_flight.spawn(CONNECTION_CONTEXT.scope(
                    conn_for_task,
                    RUN_REQUEST.scope(request, async move {
                        let _guard = guard;
                        let resp = manager.handle_request(req).await;
                        if let Ok(s) = serde_json::to_string(&resp) {
                            let _ = tx.try_send_sync(s);
                        }
                    }),
                ));
                if authenticated {
                    self.sync_subscriptions(&mut sub_guard, &mut subscribed, &out_tx);
                }
            } else if req.method == "session.new" {
                // B02a: session.new without explicit session_id creates an additional session and
                // updates only this connection's private default session upon success.
                let id = req.id.clone().unwrap_or(Value::Null);
                let workspace = req
                    .params
                    .get("workspace")
                    .and_then(Value::as_str)
                    .map(PathBuf::from);
                let resume = req
                    .params
                    .get("resume")
                    .and_then(Value::as_str)
                    .map(PathBuf::from);
                let (resp, new_sid) =
                    match self.open_session(workspace.as_deref(), resume.as_deref()) {
                        Ok(entry) => {
                            let sid = entry.session_id.clone();
                            (
                                JsonRpcResponse::success(
                                    id,
                                    json!({
                                        "session_id": entry.session_id,
                                        "workspace_root": entry.workspace_root,
                                    }),
                                ),
                                Some(sid),
                            )
                        }
                        Err(err) => (JsonRpcResponse::error(id, err), None),
                    };
                if let Some(sid) = new_sid {
                    conn.set_default_session(sid);
                }
                if let Ok(s) = serde_json::to_string(&resp) {
                    let _ = out_tx.try_send_sync(s);
                }
                if authenticated {
                    self.sync_subscriptions(&mut sub_guard, &mut subscribed, &out_tx);
                }
            } else {
                let is_close = req.method == "session.close";
                let resp = self.handle_request(req).await;
                if is_close && resp.error.is_none() {
                    conn.clear_default_session_if(
                        &target_sid,
                        Some(self.inner.primary.session_id()),
                    );
                }
                if let Ok(s) = serde_json::to_string(&resp) {
                    let _ = out_tx.try_send_sync(s);
                }
                if authenticated {
                    self.sync_subscriptions(&mut sub_guard, &mut subscribed, &out_tx);
                }
                if is_shutdown {
                    break;
                }
            }
        }

        // B01: the client is gone. Cancel only the runs this connection owns; a stranger or an
        // authenticated observer that started no run must not interrupt anyone else's work. A
        // daemon-wide shutdown is a separate event (the daemon's own cancel channel), not a stream
        // EOF. An unauthenticated EOF therefore has no effect on any run.
        if authenticated {
            let _ = conn.cancel_all_pending();
            for runtime in self.all_runtimes() {
                let _ = runtime.end_connection_run(&conn);
            }
        }
        out_tx.initiate_graceful_close();
        drop(sub_guard);
        while let Some(join_res) = in_flight.join_next().await {
            if let Err(e) = join_res {
                if e.is_panic() {
                    eprintln!("[kroma] prompt task panicked on exit: {:?}", e);
                }
            }
        }
        drop(conn);
        drop(out_tx);
        let _ = writer_task.await;
        Ok(())
    }

    /// B02a: Synchronize subscription guard with all hosted runtimes.
    fn sync_subscriptions(
        &self,
        guard: &mut SubscriptionGuard,
        subscribed: &mut BTreeSet<String>,
        out_tx: &BoundedOutboundSender,
    ) {
        for runtime in self.all_runtimes() {
            let session_id = runtime.session_id();
            if subscribed.insert(session_id) {
                guard.subscribe_session(&runtime, out_tx);
            }
        }
    }

    /// Open an additional session in `workspace_root` (defaulting to the daemon's workspace),
    /// optionally resuming `resume_session_path`. Fails when the session bound is reached.
    pub fn open_session(
        &self,
        workspace_root: Option<&Path>,
        resume_session_path: Option<&Path>,
    ) -> Result<SessionIndexEntry, JsonRpcError> {
        let workspace = workspace_root
            .map(Path::to_path_buf)
            .unwrap_or_else(|| self.inner.default_workspace.clone());
        // Reserve a capacity slot before building the runtime. The build is the slow step, so a
        // bound checked only here and committed after it would let every concurrent open pass the
        // check and overshoot the bound; the reservation is counted by `live_count` until it is
        // committed or released.
        {
            let mut state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
            if state.live_count() >= self.inner.max_sessions {
                return Err(JsonRpcError::with_data(
                    JSONRPC_SESSION_LIMIT,
                    format!(
                        "session limit reached ({}); close a session before opening another",
                        self.inner.max_sessions
                    ),
                    json!({ "max_sessions": self.inner.max_sessions }),
                ));
            }
            state.reserved += 1;
        }
        // From here the reservation is owned by the guard: any early return (build `Err`) or panic
        // during the build releases the slot automatically, and the commit path releases it exactly
        // once under the lock that publishes the session.
        let mut reservation = ReservationGuard::new(&self.inner);
        let dispatcher = match self.inner.factory.build(&workspace, resume_session_path) {
            Ok(dispatcher) => dispatcher,
            Err(e) => {
                return Err(JsonRpcError::with_data(
                    JSONRPC_INTERNAL_ERROR,
                    format!(
                        "failed to start a session in {}: {}",
                        workspace.display(),
                        e
                    ),
                    json!({ "workspace_root": workspace.display().to_string() }),
                ));
            }
        };
        let entry = entry_for(&dispatcher);
        let mut state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
        // Commit the reservation into the live map and persist under the same lock, so no reader can
        // observe the new session before it is durable and no concurrent index write can interleave.
        reservation.commit(&mut state);
        state.index.insert(entry.session_id.clone(), entry.clone());
        state.runtimes.insert(entry.session_id.clone(), dispatcher);
        match self.persist_index_locked(&mut state) {
            Ok(()) => Ok(entry),
            Err(e) => {
                // The index write did not commit, so withdraw the in-memory publish: a client told
                // the open failed must not find a live session behind it.
                state.runtimes.remove(&entry.session_id);
                state.index.remove(&entry.session_id);
                Err(e)
            }
        }
    }

    /// Drop a session from this daemon (its runtime is released; the WAL stays on disk).
    /// The daemon's primary session is protected and can never be closed.
    pub fn close_session(&self, session_id: &str) -> Result<(), JsonRpcError> {
        if session_id == self.inner.primary.session_id() {
            return Err(JsonRpcError::with_data(
                JSONRPC_INVALID_PARAMS,
                format!("cannot close daemon primary session '{}'", session_id),
                json!({ "session_id": session_id }),
            ));
        }
        let mut state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
        let Some(dispatcher) = state.runtimes.remove(session_id) else {
            return Err(JsonRpcError::with_data(
                JSONRPC_SESSION_NOT_OWNED,
                format!("session '{}' is not owned by this daemon", session_id),
                json!({ "session_id": session_id }),
            ));
        };
        let entry = state.index.remove(session_id);
        if let Err(e) = self.persist_index_locked(&mut state) {
            // The close did not commit; restore the session so memory and disk agree instead of the
            // session silently resurrecting from the on-disk index after a restart.
            if let Some(entry) = entry {
                state.index.insert(session_id.to_string(), entry);
            }
            state.runtimes.insert(session_id.to_string(), dispatcher);
            return Err(e);
        }
        Ok(())
    }

    /// Route one request to the runtime that owns its session.
    pub async fn handle_request(&self, req: JsonRpcRequest) -> JsonRpcResponse {
        let id = req.id.clone().unwrap_or(Value::Null);
        let requested = requested_session_id(&req.params);

        // `session.new` without an id creates an *additional* session and makes it the default, so a
        // client that never passes an id keeps driving the session it just created.
        if requested.is_none() && matches!(req.method.as_str(), "session.new") {
            let workspace = req
                .params
                .get("workspace")
                .and_then(Value::as_str)
                .map(PathBuf::from);
            let resume = req
                .params
                .get("resume")
                .and_then(Value::as_str)
                .map(PathBuf::from);
            return match self.open_session(workspace.as_deref(), resume.as_deref()) {
                Ok(entry) => JsonRpcResponse::success(
                    id,
                    json!({
                        "session_id": entry.session_id,
                        "workspace_root": entry.workspace_root,
                    }),
                ),
                Err(err) => JsonRpcResponse::error(id, err),
            };
        }

        // B02a: `session.close` closes a hosted session and removes it from the index.
        if matches!(req.method.as_str(), "session.close") {
            let sid = match requested {
                Some(s) => s,
                None => {
                    return JsonRpcResponse::error(
                        id,
                        JsonRpcError::new(
                            JSONRPC_INVALID_PARAMS,
                            "session.close requires a target session_id",
                        ),
                    );
                }
            };
            return match self.close_session(&sid) {
                Ok(()) => {
                    JsonRpcResponse::success(id, json!({ "session_id": sid, "closed": true }))
                }
                Err(err) => JsonRpcResponse::error(id, err),
            };
        }

        let target = {
            let state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
            match &requested {
                Some(sid) => state.runtimes.get(sid).cloned(),
                None => state.runtimes.get(&self.inner.primary.session_id()).cloned(),
            }
        };

        match target {
            Some(dispatcher) => dispatcher.handle_request(req).await,
            None => {
                let sid = requested.unwrap_or_default();
                JsonRpcResponse::error(
                    id,
                    JsonRpcError::with_data(
                        JSONRPC_SESSION_NOT_OWNED,
                        format!(
                            "session '{}' is not owned by this daemon; it belongs to another \
                             daemon or is unknown",
                            sid
                        ),
                        json!({ "session_id": sid }),
                    ),
                )
            }
        }
    }

    /// Persist the in-memory index and advance its generation. The caller must hold the state lock;
    /// on success `state.generation` is bumped, and a failed write leaves both the on-disk index and
    /// `state.generation` untouched so the caller can roll back its in-memory publish.
    fn persist_index_locked(&self, state: &mut SessionState) -> Result<(), JsonRpcError> {
        let next_generation = state.generation.saturating_add(1);
        let document = SessionIndexDocument {
            generation: next_generation,
            entries: state.index.values().cloned().collect(),
        };
        let bytes = serde_json::to_vec_pretty(&document).map_err(|e| {
            JsonRpcError::new(
                JSONRPC_INTERNAL_ERROR,
                format!("failed to serialize the session index: {}", e),
            )
        })?;
        ensure_index_within_cap(&bytes, self.inner.max_index_bytes.load(Ordering::SeqCst))?;
        write_index_atomically(&self.inner.index_path, &bytes)?;
        state.generation = next_generation;
        Ok(())
    }

    fn persist_index(&self) -> Result<(), JsonRpcError> {
        let mut state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
        self.persist_index_locked(&mut state)
    }

    /// Rebuild runtimes for every persisted session this daemon does not already hold. A session
    /// whose workspace or WAL no longer exists is dropped from the index instead of blocking
    /// startup.
    fn recover_persisted(&self) -> Result<(), String> {
        let bytes = match read_index_bounded(&self.inner.index_path)? {
            Some(bytes) => bytes,
            None => return Ok(()),
        };
        let (generation, entries) = parse_index(&bytes).map_err(|e| {
            format!(
                "session index {} is unreadable: {}",
                self.inner.index_path.display(),
                e
            )
        })?;
        {
            let mut state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
            state.generation = generation;
        }

        let mut _recovered = 0usize;
        for entry in entries {
            {
                let state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
                if state.runtimes.contains_key(&entry.session_id) {
                    continue;
                }
                if state.live_count() >= self.inner.max_sessions {
                    break;
                }
            }
            let workspace = PathBuf::from(&entry.workspace_root);
            let session_path = PathBuf::from(&entry.session_path);
            match self
                .inner
                .factory
                .build(&workspace, Some(session_path.as_path()))
            {
                Ok(dispatcher) if dispatcher.session_id() == entry.session_id => {
                    let mut state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
                    state.index.insert(entry.session_id.clone(), entry.clone());
                    state.runtimes.insert(entry.session_id.clone(), dispatcher);
                    _recovered += 1;
                }
                Ok(_) => {
                    eprintln!(
                        "[kroma] session index entry {} no longer matches its WAL; dropping it",
                        entry.session_id
                    );
                }
                Err(e) => {
                    eprintln!(
                        "[kroma] could not recover session {} ({}): {}",
                        entry.session_id, entry.workspace_root, e
                    );
                }
            }
        }
        Ok(())
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::provider::FakeProvider;
    use crate::tool::policy::PolicyMode;

    /// Builds a real runtime per workspace, mirroring the integration-test factory.
    struct UnitFactory {
        base: PathBuf,
    }

    impl SessionRuntimeFactory for UnitFactory {
        fn build(
            &self,
            workspace_root: &Path,
            resume_session_path: Option<&Path>,
        ) -> Result<RpcDispatcher, String> {
            let name = workspace_root
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "ws".to_string());
            let mut cfg = KromaRuntimeConfig::new(
                workspace_root,
                self.base.join("state").join(name),
                PolicyMode::AllowWrite,
            );
            cfg.resume_session_path = resume_session_path.map(Path::to_path_buf);
            RpcDispatcher::new(cfg, Arc::new(FakeProvider::new(vec![])))
        }
    }

    /// A scratch directory unique to this test, so parallel unit tests never share an index path.
    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kroma-session-index-unit-{}-{}",
            std::process::id(),
            tag
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// F12: a directory `fsync` failure after the atomic `rename` does not lose the committed index.
    /// The rename is already visible to every reader (and would be recovered on restart), so the
    /// operation must not be reported as a write failure; only durability is best-effort. The fault
    /// is the shared thread-scoped hook, so arming it here cannot perturb another test.
    #[test]
    fn directory_sync_failure_keeps_the_committed_index() {
        let dir = scratch("dir-sync-fault");
        let path = dir.join("index.json");
        let payload = br#"{"generation":3,"entries":[]}"#;

        let outcome = {
            let _fault = crate::secure_fs::DirSyncFaultGuard::arm();
            write_index_atomically(&path, payload)
        };

        assert!(
            outcome.is_ok(),
            "a directory-sync failure must not be reported as a failed write: {outcome:?}"
        );
        assert_eq!(
            std::fs::read(&path).unwrap(),
            payload,
            "the replacement must be committed even though its directory fsync failed"
        );
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp-"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "a committed write must not leave a temp file behind"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// F12: a pre-rename failure (here the target is a directory, so `rename` fails) leaves the
    /// previous index untouched and leaves no temp file behind.
    #[test]
    fn precommit_failure_leaves_the_previous_index_and_no_temp_file() {
        let dir = scratch("precommit-fault");
        let path = dir.join("index.json");
        std::fs::create_dir(&path).unwrap();

        let outcome = write_index_atomically(&path, br#"{"generation":1,"entries":[]}"#);
        assert!(
            outcome.is_err(),
            "a failed replace must be reported as an error"
        );
        assert!(path.is_dir(), "the obstructing directory must be untouched");
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp-"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "a failed replace must clean up its temp file"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// F1: the write cap is a real boundary — a document over the production limit is refused, a
    /// small one is accepted, and a low cap behaves identically.
    #[test]
    fn the_write_cap_rejects_a_document_over_the_production_limit() {
        assert!(ensure_index_within_cap(b"{}", MAX_SESSION_INDEX_BYTES).is_ok());
        let over = vec![b'x'; MAX_SESSION_INDEX_BYTES as usize + 1];
        let err = ensure_index_within_cap(&over, MAX_SESSION_INDEX_BYTES)
            .expect_err("a document over the cap must be refused");
        assert_eq!(err.code, JSONRPC_INTERNAL_ERROR);
        assert!(ensure_index_within_cap(b"1234", 4).is_ok());
        assert!(ensure_index_within_cap(b"12345", 4).is_err());
    }

    /// F1: an index that would exceed the write cap is refused *before* any write, and the failed
    /// open rolls back — no session is published and the on-disk index is untouched.
    #[test]
    fn an_oversized_index_is_refused_before_writing_and_rolls_back() {
        let dir = scratch("oversize-rollback");
        let ws = dir.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let index_path = dir.join("state").join("daemon-sessions.json");
        let factory = Arc::new(UnitFactory { base: dir.clone() });
        let primary = factory.build(&ws, None).unwrap();
        let manager = SessionManager::open(
            SessionManagerConfig {
                factory: factory.clone(),
                max_sessions: 8,
                index_path: index_path.clone(),
                default_workspace: ws.clone(),
            },
            primary,
        )
        .unwrap();

        // One extra session so we know the size of a two-entry index...
        manager.open_session(Some(&ws), None).unwrap();
        let two_entry_len = std::fs::metadata(&index_path).unwrap().len();
        let before = std::fs::read(&index_path).unwrap();
        // ...then cap the index just below it, so the next entry cannot be persisted.
        manager.set_index_byte_cap(two_entry_len - 1);

        let err = manager
            .open_session(Some(&ws), None)
            .expect_err("an index over the cap must be refused before it is written");
        assert_eq!(err.code, JSONRPC_INTERNAL_ERROR, "{}", err.message);
        assert!(
            err.message.contains("cap"),
            "the error must name the cap: {}",
            err.message
        );
        assert_eq!(manager.len(), 2, "the refused open publishes no session");
        assert_eq!(
            std::fs::read(&index_path).unwrap(),
            before,
            "the refused open must not touch the on-disk index"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
