//! Gate 6 — Multi-Surface Runtime (`v1.0.0`).
//!
//! Decouples the core execution engine (`AgentRunner` + `SessionJournal` + `SnapshotStore` +
//! `ToolRegistry` + `PolicyEngine`) from terminal I/O so all four runtime surfaces share the exact
//! same state machine, policy boundaries, workspace revision history, and diagnostic event stream:
//!
//! 1. **Headless CLI** (`harness run` / `harness --task ...` with optional `--output-format jsonl`).
//! 2. **Interactive REPL / TUI** (`harness repl` / `harness tui` / `--interactive`), implemented as a
//!    stateless consumer of [`RpcDispatcher`] with zero private session state.
//! 3. **ACP stdio server** (`harness acp` / `--acp`), speaking newline-delimited JSON-RPC 2.0 on
//!    `stdin`/`stdout` with `stdout` reserved exclusively for protocol frames.
//! 4. **Local SDK / Daemon server** (`harness serve --listen 127.0.0.1:<PORT>`), serving JSON-RPC 2.0
//!    over loopback TCP (or explicit `--auth-token` authenticated TCP) with bounded frame reading,
//!    HTTP/browser request rejection, and connection concurrency limits.

use crate::agent_loop::{build_runtime_with_registry, AgentRunConfig, AgentRunner};
use crate::attribution::{MutationLedger, MutationRecord};
use crate::budget::BudgetConfig;
use crate::completion_gate::WorkspaceCompletionGate;
use crate::context_projector::{source_entry_id, CompactionReason, ContextProjector, EvidenceLink};
use crate::env::{AllowAllGate, ExecutionEnv, LocalHostEnv, SharedCompletionGate};
use crate::events::{DiagnosticEvent, EventLogger, EventPayload};
use crate::extensions::{
    discover_skills, execute_activate_skill, ExtensionId, ScopedExtension, SkillEntry,
    SubtreeContextTracker,
};
use crate::messages::TranscriptItem;
use crate::prompt::{sha256_hex64, PromptBuilder};
use crate::provider::ModelProvider;
use crate::revision::{RestoreResult, RevisionId, SnapshotStore, WorkspaceRevision};
use crate::session::{RevisionRecord, SessionJournal};
use crate::tool::policy::{
    ApprovalDecision, ApprovalHandler, ApprovalRequest, PolicyEngine, PolicyMode,
};
use crate::tool::registry::ToolRegistry;
use crate::tools::recall_evidence::execute_recall_evidence;
use crate::types::{EffectClass, RunId, RunStatus};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc as std_mpsc, Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, watch, Mutex as AsyncMutex, Semaphore};

/// JSON-RPC 2.0 wire version string.
pub const JSONRPC_VERSION: &str = "2.0";
/// Agent Client Protocol (ACP) adapter version string.
pub const ACP_PROTOCOL_VERSION: &str = "1.0";
/// Hard cap on a single incoming JSON-RPC line (4 MiB) to prevent memory-exhaustion DoS.
pub const MAX_RPC_FRAME_BYTES: usize = 4 * 1024 * 1024;
/// Maximum simultaneous client connections accepted by `harness serve`.
pub const MAX_DAEMON_CONNECTIONS: usize = 16;
/// Default timeout for a pending operator approval before failing closed (`ApprovalDecision::Deny`).
pub const DEFAULT_APPROVAL_TIMEOUT: Duration = Duration::from_secs(60);

pub const JSONRPC_PARSE_ERROR: i64 = -32700;
pub const JSONRPC_INVALID_REQUEST: i64 = -32600;
pub const JSONRPC_METHOD_NOT_FOUND: i64 = -32601;
pub const JSONRPC_INVALID_PARAMS: i64 = -32602;
pub const JSONRPC_INTERNAL_ERROR: i64 = -32603;
pub const JSONRPC_UNAUTHORIZED: i64 = -32001;
pub const JSONRPC_RESTORE_CONFLICT: i64 = -32003;
pub const JSONRPC_RECOVERY_REQUIRED: i64 = -32004;

/// Escape untrusted text before rendering it to a terminal surface (F-06).
///
/// Preserves `\n` and `\t` while escaping `\r`, ESC (`\x1b`), C0/C1 control characters, and Unicode
/// bidirectional overrides so untrusted repository content or model output cannot spoof terminal UI.
pub fn sanitize_terminal_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        let cp = ch as u32;
        let is_bidi = (0x202A..=0x202E).contains(&cp) || (0x2066..=0x2069).contains(&cp);
        match ch {
            '\n' => out.push('\n'),
            '\t' => out.push('\t'),
            '\r' => out.push_str("\\r"),
            '\x1b' => out.push_str("\\x1b"),
            _ if cp < 0x20 || cp == 0x7f || (0x80..=0x9f).contains(&cp) || is_bidi => {
                out.push_str(&format!("\\x{:02x}", cp));
            }
            _ => out.push(ch),
        }
    }
    out
}

/// JSON-RPC 2.0 request envelope.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    #[serde(default)]
    pub id: Option<Value>,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

impl JsonRpcRequest {
    pub fn new(id: impl Into<Value>, method: impl Into<String>, params: Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id: Some(id.into()),
            method: method.into(),
            params,
        }
    }
}

/// JSON-RPC 2.0 structured error payload.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl JsonRpcError {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    pub fn with_data(code: i64, message: impl Into<String>, data: Value) -> Self {
        Self {
            code,
            message: message.into(),
            data: Some(data),
        }
    }
}

/// JSON-RPC 2.0 response envelope.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    pub id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

impl JsonRpcResponse {
    pub fn success(id: Value, result: Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn error(id: Value, err: JsonRpcError) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id,
            result: None,
            error: Some(err),
        }
    }

    pub fn is_ok(&self) -> bool {
        self.error.is_none()
    }
}

/// JSON-RPC 2.0 server-to-client notification envelope (`harness/event`, `approval/requested`, etc.).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct JsonRpcNotification {
    pub jsonrpc: String,
    pub method: String,
    pub params: Value,
}

impl JsonRpcNotification {
    pub fn new(method: impl Into<String>, params: Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            method: method.into(),
            params,
        }
    }
}

/// Metadata describing an in-flight operator approval request across ACP, daemon, or REPL surfaces.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ApprovalPendingInfo {
    pub request_id: String,
    pub tool_name: String,
    pub effect_class: String,
    pub is_sensitive_path: bool,
    pub target_path: Option<String>,
    pub arguments_summary: String,
    pub risk_reason: String,
    pub surface: String,
}

/// Result of resolving a pending approval via `approval.respond`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ApprovalResolution {
    pub request_id: String,
    pub decision: String,
    pub resolved: bool,
}

struct PendingApprovalSlot {
    info: ApprovalPendingInfo,
    tx: std_mpsc::Sender<(ApprovalDecision, String)>,
}

struct BrokerInner {
    next_seq: usize,
    closed: bool,
    timeout: Duration,
    pending: BTreeMap<String, PendingApprovalSlot>,
    recorded_events: Vec<EventPayload>,
}

type NotificationCallback = Arc<dyn Fn(JsonRpcNotification) + Send + Sync>;

/// Surface-agnostic approval broker implementing [`ApprovalHandler`].
///
/// When `PolicyMode::Ask` requires confirmation for a mutating/shell tool or sensitive path:
/// 1. Emits an `approval/requested` JSON-RPC notification with `{ request_id, tool_name,
///    arguments_summary, risk_reason, ... }`.
/// 2. Exposes the pending request via `approval.pending` (`pending_requests()`).
/// 3. Resumes immediately when `approval.respond` (`respond()`) supplies `approve_once`,
///    `approve_session`, or `deny`.
/// 4. Enforces fail-closed disconnect and timeout semantics: if the client disconnects (`close()`)
///    or `timeout` expires without an explicit approval, the tool call is automatically denied.
pub struct RpcApprovalBroker {
    surface: String,
    inner: Mutex<BrokerInner>,
    notification_sink: Mutex<Option<NotificationCallback>>,
}

/// Alias matching the task specification (`ChannelApprovalHandler` / `RpcApprovalBroker`).
pub type ChannelApprovalHandler = RpcApprovalBroker;

impl RpcApprovalBroker {
    pub fn new(surface: impl Into<String>) -> Arc<Self> {
        Arc::new(Self {
            surface: surface.into(),
            inner: Mutex::new(BrokerInner {
                next_seq: 0,
                closed: false,
                timeout: DEFAULT_APPROVAL_TIMEOUT,
                pending: BTreeMap::new(),
                recorded_events: Vec::new(),
            }),
            notification_sink: Mutex::new(None),
        })
    }

    pub fn set_timeout(&self, timeout: Duration) {
        let mut guard = self.inner.lock().unwrap();
        guard.timeout = timeout;
    }

    pub fn set_notification_sink(&self, sink: NotificationCallback) {
        let mut guard = self.notification_sink.lock().unwrap();
        *guard = Some(sink);
    }

    fn emit_notification(&self, notification: JsonRpcNotification) {
        let sink = self.notification_sink.lock().unwrap().clone();
        if let Some(cb) = sink {
            cb(notification);
        }
    }

    pub fn pending_requests(&self) -> Vec<ApprovalPendingInfo> {
        let guard = self.inner.lock().unwrap();
        guard.pending.values().map(|s| s.info.clone()).collect()
    }

    pub fn is_closed(&self) -> bool {
        self.inner.lock().unwrap().closed
    }

    /// Re-open the broker when a new client session attaches.
    pub fn reopen(&self) {
        let mut guard = self.inner.lock().unwrap();
        guard.closed = false;
    }

    /// Close the approval channel (e.g. on client EOF/disconnect). Any in-flight or subsequent
    /// approval request fails closed with `ApprovalDecision::Deny`.
    pub fn close(&self) {
        let mut resolved_ids = Vec::new();
        {
            let mut guard = self.inner.lock().unwrap();
            guard.closed = true;
            let drained: Vec<(String, PendingApprovalSlot)> =
                std::mem::take(&mut guard.pending).into_iter().collect();
            for (req_id, slot) in drained {
                let _ = slot
                    .tx
                    .send((ApprovalDecision::Deny, "deny_disconnect".to_string()));
                guard.recorded_events.push(EventPayload::ApprovalResolved {
                    request_id: req_id.clone(),
                    decision: "deny_disconnect".to_string(),
                });
                resolved_ids.push(req_id);
            }
        }
        for req_id in resolved_ids {
            self.emit_notification(JsonRpcNotification::new(
                "approval/resolved",
                json!({
                    "request_id": req_id,
                    "decision": "deny_disconnect",
                }),
            ));
        }
    }

    /// Cancel all currently pending approval prompts (invoked by `session.cancel`).
    pub fn cancel_all_pending(&self) {
        let mut resolved_ids = Vec::new();
        {
            let mut guard = self.inner.lock().unwrap();
            let drained: Vec<(String, PendingApprovalSlot)> =
                std::mem::take(&mut guard.pending).into_iter().collect();
            for (req_id, slot) in drained {
                let _ = slot
                    .tx
                    .send((ApprovalDecision::Deny, "deny_cancelled".to_string()));
                guard.recorded_events.push(EventPayload::ApprovalResolved {
                    request_id: req_id.clone(),
                    decision: "deny_cancelled".to_string(),
                });
                resolved_ids.push(req_id);
            }
        }
        for req_id in resolved_ids {
            self.emit_notification(JsonRpcNotification::new(
                "approval/resolved",
                json!({
                    "request_id": req_id,
                    "decision": "deny_cancelled",
                }),
            ));
        }
    }

    /// Resolve a pending approval request (`approve_once` | `approve_session` | `deny`).
    pub fn respond(
        &self,
        request_id: Option<&str>,
        decision_raw: &str,
    ) -> Result<ApprovalResolution, String> {
        let (decision, canonical) = match decision_raw.trim().to_ascii_lowercase().as_str() {
            "approve_once" | "allow_once" | "approve" | "allow" | "y" | "yes" => {
                (ApprovalDecision::AllowOnce, "approve_once".to_string())
            }
            "approve_session" | "allow_session" | "allow_category" | "allow_all" | "a" | "all" => (
                ApprovalDecision::AllowCategoryForRun,
                "approve_session".to_string(),
            ),
            "deny" | "reject" | "n" | "no" => (ApprovalDecision::Deny, "deny".to_string()),
            other => {
                return Err(format!(
                    "invalid approval decision '{}'; expected 'approve_once', 'approve_session', or 'deny'",
                    other
                ));
            }
        };

        let resolved_id = {
            let mut guard = self.inner.lock().unwrap();
            let target_id = match request_id.map(str::trim).filter(|s| !s.is_empty()) {
                Some(id) => id.to_string(),
                None if guard.pending.len() == 1 => {
                    guard.pending.keys().next().cloned().unwrap()
                }
                None if guard.pending.is_empty() => {
                    return Err("no pending approval requests".to_string());
                }
                None => {
                    return Err(
                        "multiple approval requests are pending; specify request_id".to_string()
                    );
                }
            };
            let Some(slot) = guard.pending.remove(&target_id) else {
                return Err(format!(
                    "unknown or already resolved approval request_id '{}'",
                    target_id
                ));
            };
            guard.recorded_events.push(EventPayload::ApprovalResolved {
                request_id: target_id.clone(),
                decision: canonical.clone(),
            });
            let _ = slot.tx.send((decision, canonical.clone()));
            target_id
        };

        self.emit_notification(JsonRpcNotification::new(
            "approval/resolved",
            json!({
                "request_id": resolved_id,
                "decision": canonical,
            }),
        ));

        Ok(ApprovalResolution {
            request_id: resolved_id,
            decision: canonical,
            resolved: true,
        })
    }

    fn summarize_request(req: &ApprovalRequest) -> (String, String) {
        let summary = if let Some(ref cmd) = req.command_preview {
            cmd.lines().next().unwrap_or("").chars().take(200).collect()
        } else if let Some(ref path) = req.target_path {
            path.display().to_string()
        } else if let Some(ref diff) = req.diff_preview {
            diff.lines().next().unwrap_or("").chars().take(200).collect()
        } else {
            req.tool_name.clone()
        };

        let risk_reason = if req.is_sensitive_path {
            "sensitive path access requires explicit operator approval".to_string()
        } else {
            match req.effect_class {
                EffectClass::SideEffect => {
                    "shell/side-effect tool requires operator approval under --policy ask"
                        .to_string()
                }
                EffectClass::Mutating => {
                    "mutating workspace tool requires operator approval under --policy ask"
                        .to_string()
                }
                EffectClass::ReadOnly => {
                    "tool requires operator approval under active policy".to_string()
                }
            }
        };
        (summary, risk_reason)
    }
}

impl ApprovalHandler for RpcApprovalBroker {
    fn is_interactive(&self) -> bool {
        true
    }

    fn request_approval(&self, req: &ApprovalRequest) -> ApprovalDecision {
        let (arguments_summary, risk_reason) = Self::summarize_request(req);
        let effect_str = match req.effect_class {
            EffectClass::ReadOnly => "read-only",
            EffectClass::Mutating => "mutating",
            EffectClass::SideEffect => "side-effect",
        };
        let (tx, rx) = std_mpsc::channel::<(ApprovalDecision, String)>();

        let (request_id, info, timeout, already_closed) = {
            let mut guard = self.inner.lock().unwrap();
            guard.next_seq += 1;
            let request_id = format!("apr-{:04}", guard.next_seq);
            let info = ApprovalPendingInfo {
                request_id: request_id.clone(),
                tool_name: req.tool_name.clone(),
                effect_class: effect_str.to_string(),
                is_sensitive_path: req.is_sensitive_path,
                target_path: req.target_path.as_ref().map(|p| p.display().to_string()),
                arguments_summary,
                risk_reason,
                surface: self.surface.clone(),
            };
            guard.recorded_events.push(EventPayload::ApprovalRequested {
                request_id: request_id.clone(),
                tool_name: req.tool_name.clone(),
                surface: self.surface.clone(),
            });
            let closed = guard.closed;
            let timeout = guard.timeout;
            if closed {
                guard.recorded_events.push(EventPayload::ApprovalResolved {
                    request_id: request_id.clone(),
                    decision: "deny_disconnect".to_string(),
                });
            } else {
                guard.pending.insert(
                    request_id.clone(),
                    PendingApprovalSlot {
                        info: info.clone(),
                        tx,
                    },
                );
            }
            (request_id, info, timeout, closed)
        };

        self.emit_notification(JsonRpcNotification::new(
            "approval/requested",
            serde_json::to_value(&info).unwrap_or(Value::Null),
        ));

        if already_closed {
            return ApprovalDecision::Deny;
        }

        let deadline = Instant::now() + timeout;
        let poll_slice = Duration::from_millis(20);
        let wait_for_decision = || loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                let removed = {
                    let mut guard = self.inner.lock().unwrap();
                    let was_pending = guard.pending.remove(&request_id).is_some();
                    if was_pending {
                        guard.recorded_events.push(EventPayload::ApprovalResolved {
                            request_id: request_id.clone(),
                            decision: "deny_timeout".to_string(),
                        });
                    }
                    was_pending
                };
                if removed {
                    self.emit_notification(JsonRpcNotification::new(
                        "approval/resolved",
                        json!({
                            "request_id": request_id,
                            "decision": "deny_timeout",
                        }),
                    ));
                }
                return ApprovalDecision::Deny;
            }
            let wait_for = remaining.min(poll_slice);
            match rx.recv_timeout(wait_for) {
                Ok((decision, canonical)) => {
                    let mut guard = self.inner.lock().unwrap();
                    if guard.pending.remove(&request_id).is_some() {
                        guard.recorded_events.push(EventPayload::ApprovalResolved {
                            request_id,
                            decision: canonical,
                        });
                    }
                    return decision;
                }
                Err(std_mpsc::RecvTimeoutError::Disconnected) => {
                    let mut guard = self.inner.lock().unwrap();
                    if guard.pending.remove(&request_id).is_some() {
                        guard.recorded_events.push(EventPayload::ApprovalResolved {
                            request_id,
                            decision: "deny_disconnect".to_string(),
                        });
                    }
                    return ApprovalDecision::Deny;
                }
                Err(std_mpsc::RecvTimeoutError::Timeout) => {
                    let closed = self.inner.lock().unwrap().closed;
                    if closed {
                        let mut guard = self.inner.lock().unwrap();
                        if guard.pending.remove(&request_id).is_some() {
                            guard.recorded_events.push(EventPayload::ApprovalResolved {
                                request_id,
                                decision: "deny_disconnect".to_string(),
                            });
                        }
                        return ApprovalDecision::Deny;
                    }
                }
            }
        };

        match tokio::runtime::Handle::try_current() {
            Ok(h) if h.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
                tokio::task::block_in_place(wait_for_decision)
            }
            _ => wait_for_decision(),
        }
    }

    fn drain_events(&self) -> Vec<EventPayload> {
        let mut guard = self.inner.lock().unwrap();
        std::mem::take(&mut guard.recorded_events)
    }
}

/// Configuration for [`RpcDispatcher`] / [`HarnessRuntimeServer`].
pub struct HarnessRuntimeConfig {
    pub workspace_root: PathBuf,
    pub state_dir: PathBuf,
    pub policy_mode: PolicyMode,
    pub budget: BudgetConfig,
    pub stream: bool,
    pub strict_log: bool,
    pub tool_filter: Option<Vec<String>>,
    pub resume_session_path: Option<PathBuf>,
    pub fork_turn: Option<usize>,
    pub completion_gate: SharedCompletionGate,
    pub completion_gate_max_retries: usize,
    pub predict_cmd: Option<Arc<WorkspaceCompletionGate>>,
    pub predictive_check: bool,
    pub subtree_context_enabled: bool,
    pub max_subagents: usize,
    pub initial_skill: Option<String>,
    pub extensions: Vec<ScopedExtension>,
    pub surface_name: String,
    pub transport_name: String,
    pub auth_token: Option<String>,
    pub approval_timeout: Duration,
    pub quiet: bool,
}

impl HarnessRuntimeConfig {
    pub fn new(
        workspace_root: impl Into<PathBuf>,
        state_dir: impl Into<PathBuf>,
        policy_mode: PolicyMode,
    ) -> Self {
        Self {
            workspace_root: workspace_root.into(),
            state_dir: state_dir.into(),
            policy_mode,
            budget: BudgetConfig::default(),
            stream: true,
            strict_log: false,
            tool_filter: None,
            resume_session_path: None,
            fork_turn: None,
            completion_gate: Arc::new(AllowAllGate),
            completion_gate_max_retries: 3,
            predict_cmd: None,
            predictive_check: true,
            subtree_context_enabled: true,
            max_subagents: crate::subagents::DEFAULT_MAX_SUBAGENT_SPAWNS,
            initial_skill: None,
            extensions: Vec::new(),
            surface_name: "acp".to_string(),
            transport_name: "stdio".to_string(),
            auth_token: None,
            approval_timeout: DEFAULT_APPROVAL_TIMEOUT,
            quiet: false,
        }
    }

    pub fn with_surface(mut self, surface: impl Into<String>, transport: impl Into<String>) -> Self {
        self.surface_name = surface.into();
        self.transport_name = transport.into();
        self
    }

    pub fn with_auth_token(mut self, token: Option<String>) -> Self {
        self.auth_token = token
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty());
        self
    }

    pub fn with_approval_timeout(mut self, timeout: Duration) -> Self {
        self.approval_timeout = timeout;
        self
    }

    pub fn with_completion_gate(mut self, gate: SharedCompletionGate) -> Self {
        self.completion_gate = gate;
        self
    }
}

#[derive(Debug, Clone, Default)]
struct SessionStatusSnapshot {
    session_id: String,
    session_path: PathBuf,
    turn_count: usize,
    latest_revision_id: Option<String>,
    subtree_dirs: Vec<String>,
    mutations: Vec<MutationRecord>,
    subagents_spawned: usize,
    revisions: Vec<RevisionRecord>,
    activated_skills: Vec<String>,
    total_input_tokens: u64,
    total_cached_input_tokens: u64,
    total_output_tokens: u64,
}

type SubscriberCallback = Arc<dyn Fn(&JsonRpcNotification) -> bool + Send + Sync>;

struct RuntimeEngineInner {
    config: HarnessRuntimeConfig,
    provider: Arc<dyn ModelProvider>,
    env: Arc<LocalHostEnv>,
    snapshot_store: Arc<SnapshotStore>,
    registry: ToolRegistry,
    policy_engine: PolicyEngine,
    approval_broker: Arc<RpcApprovalBroker>,
    workspace_skills: Vec<SkillEntry>,
    mounted_extensions: Vec<ExtensionId>,
    status: Mutex<SessionStatusSnapshot>,
    active_cancel: Mutex<Option<(watch::Sender<bool>, watch::Receiver<bool>)>>,
    in_flight: AtomicBool,
    shutting_down: AtomicBool,
    next_subscriber_id: std::sync::atomic::AtomicU64,
    subscribers: Mutex<Vec<(u64, SubscriberCallback)>>,
    op_lock: AsyncMutex<()>,
}

struct InFlightPromptGuard {
    inner: Arc<RuntimeEngineInner>,
}

impl Drop for InFlightPromptGuard {
    fn drop(&mut self) {
        self.inner.in_flight.store(false, Ordering::SeqCst);
        *self.inner.active_cancel.lock().unwrap() = None;
    }
}

/// Unified multi-surface runtime engine and JSON-RPC 2.0 dispatcher.
#[derive(Clone)]
pub struct RpcDispatcher {
    inner: Arc<RuntimeEngineInner>,
}

/// Alias matching the task specification (`HarnessRuntimeServer` / `RpcDispatcher`).
pub type HarnessRuntimeServer = RpcDispatcher;

impl RpcDispatcher {
    pub fn new(
        mut config: HarnessRuntimeConfig,
        provider: Arc<dyn ModelProvider>,
    ) -> Result<Self, String> {
        config.auth_token = config
            .auth_token
            .take()
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty());
        let env = Arc::new(
            LocalHostEnv::new(&config.workspace_root, config.state_dir.clone())
                .map_err(|e| format!("failed to initialize workspace capability: {}", e))?,
        );
        let snapshot_store = Arc::new(SnapshotStore::new(config.state_dir.join("snapshots")));
        let workspace_skills = discover_skills(env.workspace());

        if let Some(ref skill_name) = config.initial_skill {
            if !workspace_skills.iter().any(|s| s.name == *skill_name) {
                return Err(format!("unknown initial skill '{}'", skill_name));
            }
        }

        let mut journal = if let Some(ref path) = config.resume_session_path {
            let mut j = SessionJournal::open(path)
                .map_err(|e| format!("failed to open session WAL {}: {}", path.display(), e))?;
            if j.workspace_root() != env.workspace().root() {
                return Err(format!(
                    "session WAL belongs to workspace {}, not {}",
                    j.workspace_root().display(),
                    env.workspace().root().display()
                ));
            }
            let _ = snapshot_store.reconcile_with_wal(&mut j);
            if let Some(turn) = config.fork_turn {
                j.fork_at_turn(turn)
                    .map_err(|e| format!("failed to fork session at turn {}: {}", turn, e))?;
            }
            j
        } else {
            SessionJournal::create(config.state_dir.join("sessions"), env.workspace().root())
                .map_err(|e| format!("failed to create session WAL: {}", e))?
        };
        let _ = snapshot_store.reconcile_with_wal(&mut journal);
        let mut initial_snapshot = Self::snapshot_from_journal(&journal);
        if let Some(ref skill_name) = config.initial_skill {
            initial_snapshot.activated_skills.push(skill_name.clone());
        }
        drop(journal);

        let approval_broker = RpcApprovalBroker::new(config.surface_name.clone());
        approval_broker.set_timeout(config.approval_timeout);
        let policy_engine = PolicyEngine::new(
            config.policy_mode,
            Arc::clone(&approval_broker) as Arc<dyn ApprovalHandler>,
        );

        let mut registry =
            ToolRegistry::new(config.tool_filter.clone()).with_workspace_skills(workspace_skills.clone());
        if config.max_subagents == 0 {
            registry = registry.without_subagents();
        }
        let mut mounted_extensions = Vec::new();
        for ext in std::mem::take(&mut config.extensions) {
            let id = registry
                .register_extension(ext)
                .map_err(|e| format!("failed to mount extension: {}", e))?;
            mounted_extensions.push(id);
        }

        let inner = Arc::new(RuntimeEngineInner {
            config,
            provider,
            env,
            snapshot_store,
            registry,
            policy_engine,
            approval_broker: Arc::clone(&approval_broker),
            workspace_skills,
            mounted_extensions,
            status: Mutex::new(initial_snapshot),
            active_cancel: Mutex::new(None),
            in_flight: AtomicBool::new(false),
            shutting_down: AtomicBool::new(false),
            next_subscriber_id: std::sync::atomic::AtomicU64::new(1),
            subscribers: Mutex::new(Vec::new()),
            op_lock: AsyncMutex::new(()),
        });

        let weak_inner = Arc::downgrade(&inner);
        approval_broker.set_notification_sink(Arc::new(move |note| {
            if let Some(strong) = weak_inner.upgrade() {
                Self::broadcast_to_subscribers(&strong, note);
            }
        }));

        // Emit the Gate 6 `SurfaceStarted` event into the state directory diagnostic log.
        if let Ok(mut startup_logger) = EventLogger::new(
            RunId::new(),
            Some(inner.config.state_dir.clone()),
            inner.config.strict_log,
        ) {
            let _ = startup_logger.log(EventPayload::SurfaceStarted {
                surface: inner.config.surface_name.clone(),
                transport: inner.config.transport_name.clone(),
            });
        }

        Ok(Self { inner })
    }

    pub fn approval_broker(&self) -> &Arc<RpcApprovalBroker> {
        &self.inner.approval_broker
    }

    pub fn auth_token(&self) -> Option<&str> {
        self.inner.config.auth_token.as_deref()
    }

    pub fn session_id(&self) -> String {
        self.inner.status.lock().unwrap().session_id.clone()
    }

    pub fn session_path(&self) -> PathBuf {
        self.inner.status.lock().unwrap().session_path.clone()
    }

    pub fn workspace_root(&self) -> &Path {
        self.inner.env.workspace().root()
    }

    pub fn is_shutting_down(&self) -> bool {
        self.inner.shutting_down.load(Ordering::SeqCst)
    }

    /// Subscribe to real-time JSON-RPC notifications (`harness/event`, `approval/requested`,
    /// `approval/resolved`).
    pub fn subscribe_notifications(&self) -> mpsc::UnboundedReceiver<JsonRpcNotification> {
        let (tx, rx) = mpsc::unbounded_channel();
        let sub_id = self.inner.next_subscriber_id.fetch_add(1, Ordering::SeqCst);
        self.inner
            .subscribers
            .lock()
            .unwrap()
            .push((sub_id, Arc::new(move |note| tx.send(note.clone()).is_ok())));
        rx
    }

    fn broadcast_to_subscribers(inner: &RuntimeEngineInner, note: JsonRpcNotification) {
        let mut subs = inner.subscribers.lock().unwrap();
        subs.retain(|(_, cb)| cb(&note));
    }

    pub fn emit_notification(&self, note: JsonRpcNotification) {
        Self::broadcast_to_subscribers(&self.inner, note);
    }

    fn snapshot_from_journal(journal: &SessionJournal) -> SessionStatusSnapshot {
        let turn_count = journal.max_active_turn();
        let latest_revision_id = journal.latest_revision_id();
        let records = journal.revision_records();
        let reverted_active_revisions = (|| -> Option<BTreeSet<String>> {
            let last = records.last()?;
            let first_idx = records.iter().position(|r| r.revision_id == last.revision_id)?;
            if first_idx + 1 < records.len() {
                Some(
                    records[..=first_idx]
                        .iter()
                        .map(|r| r.revision_id.clone())
                        .collect(),
                )
            } else {
                None
            }
        })();

        let (subtree_dirs, mutations, subagents_spawned) = match journal.latest_active_checkpoint()
        {
            Some(cp) => {
                let mut ledger = MutationLedger::new();
                ledger.restore(cp.mutations.clone());
                ledger.prune_after_turn(turn_count as u32);
                if let Some(ref active_revs) = reverted_active_revisions {
                    ledger.prune_reverted_revisions(active_revs);
                }
                (
                    cp.subtree_dirs.clone(),
                    ledger.records().to_vec(),
                    cp.subagents_spawned,
                )
            }
            None => (Vec::new(), Vec::new(), 0),
        };

        SessionStatusSnapshot {
            session_id: journal.session_id().to_string(),
            session_path: journal.path().to_path_buf(),
            turn_count,
            latest_revision_id,
            subtree_dirs,
            mutations,
            subagents_spawned,
            revisions: records,
            activated_skills: Vec::new(),
            total_input_tokens: 0,
            total_cached_input_tokens: 0,
            total_output_tokens: 0,
        }
    }

    fn update_snapshot_from_journal(&self, journal: &SessionJournal) {
        let mut next = Self::snapshot_from_journal(journal);
        let mut guard = self.inner.status.lock().unwrap();
        next.activated_skills = guard.activated_skills.clone();
        next.total_input_tokens = guard.total_input_tokens;
        next.total_cached_input_tokens = guard.total_cached_input_tokens;
        next.total_output_tokens = guard.total_output_tokens;
        *guard = next;
    }

    pub fn supported_methods() -> Vec<&'static str> {
        vec![
            "initialize",
            "acp.initialize",
            "session.new",
            "session.start",
            "session.prompt",
            "acp.prompt",
            "session.cancel",
            "session.status",
            "session.fork",
            "approval.pending",
            "approval.respond",
            "revision.list",
            "revision.undo",
            "revision.restore",
            "evidence.recall",
            "skills.list",
            "skills.activate",
            "shutdown",
        ]
    }

    /// Dispatch a single JSON-RPC 2.0 request and return its structured response.
    pub async fn handle_request(&self, req: JsonRpcRequest) -> JsonRpcResponse {
        let id = req.id.clone().unwrap_or(Value::Null);
        if req.jsonrpc != JSONRPC_VERSION {
            return JsonRpcResponse::error(
                id,
                JsonRpcError::new(
                    JSONRPC_INVALID_REQUEST,
                    format!("unsupported jsonrpc version '{}'; expected '2.0'", req.jsonrpc),
                ),
            );
        }

        let res = match req.method.as_str() {
            "initialize" | "acp.initialize" => self.rpc_initialize(&req.params).await,
            "session.new" | "session/new" | "acp.session.new" => {
                self.rpc_session_new(&req.params).await
            }
            "session.start" | "session.prompt" | "acp.prompt" | "session/prompt" => {
                self.rpc_session_prompt(&req.params).await
            }
            "approval.pending" | "approval/pending" => self.rpc_approval_pending().await,
            "approval.respond" | "approval/respond" => {
                self.rpc_approval_respond(&req.params).await
            }
            "session.cancel" | "session/cancel" | "acp.cancel" => self.rpc_session_cancel().await,
            "session.status" | "session/status" => self.rpc_session_status().await,
            "session.fork" | "session/fork" => self.rpc_session_fork(&req.params).await,
            "revision.list" | "revision/list" => self.rpc_revision_list().await,
            "revision.undo" | "revision/undo" => self.rpc_revision_undo(&req.params).await,
            "revision.restore" | "revision/restore" => {
                self.rpc_revision_restore(&req.params).await
            }
            "evidence.recall" | "evidence/recall" => self.rpc_evidence_recall(&req.params).await,
            "skills.list" | "skills/list" => self.rpc_skills_list().await,
            "skills.activate" | "skills/activate" => self.rpc_skills_activate(&req.params).await,
            "shutdown" | "acp.shutdown" => self.rpc_shutdown().await,
            other => Err(JsonRpcError::new(
                JSONRPC_METHOD_NOT_FOUND,
                format!("unknown RPC method '{}'", other),
            )),
        };

        match res {
            Ok(val) => JsonRpcResponse::success(id, val),
            Err(err) => JsonRpcResponse::error(id, err),
        }
    }

    async fn rpc_initialize(&self, _params: &Value) -> Result<Value, JsonRpcError> {
        let mut visible_tools: Vec<String> = self
            .inner
            .registry
            .visible_definitions_for_engine(&self.inner.policy_engine)
            .into_iter()
            .map(|d| d.name)
            .collect();
        if self.inner.config.max_subagents == 0 {
            visible_tools.retain(|n| n != "spawn_subagent");
        }
        let skills: Vec<Value> = self
            .inner
            .workspace_skills
            .iter()
            .map(|s| {
                json!({
                    "name": s.name,
                    "description": s.description,
                    "path": s.path,
                })
            })
            .collect();
        let snap = self.inner.status.lock().unwrap().clone();

        Ok(json!({
            "version": crate::version(),
            "protocol_version": ACP_PROTOCOL_VERSION,
            "event_schema_version": crate::events::EVENT_SCHEMA_VERSION,
            "surface": self.inner.config.surface_name,
            "transport": self.inner.config.transport_name,
            "workspace_root": self.inner.env.workspace().root().display().to_string(),
            "session_id": snap.session_id,
            "policy_mode": self.inner.policy_engine.mode().as_str(),
            "capabilities": {
                "session": true,
                "cancel": true,
                "update": true,
                "permission": true,
                "revisions": true,
                "evidence": true,
                "skills": true,
            },
            "supported_methods": Self::supported_methods(),
            "tools": visible_tools,
            "skills": skills,
        }))
    }

    fn ensure_active_cancel_receiver(&self) -> watch::Receiver<bool> {
        let mut guard = self.inner.active_cancel.lock().unwrap();
        if let Some((_, ref rx)) = *guard {
            return rx.clone();
        }
        let (tx, rx) = watch::channel(false);
        *guard = Some((tx, rx.clone()));
        rx
    }

    async fn rpc_session_new(&self, params: &Value) -> Result<Value, JsonRpcError> {
        if self.inner.in_flight.load(Ordering::SeqCst) {
            return Err(JsonRpcError::new(
                JSONRPC_INTERNAL_ERROR,
                "cannot start a new session while a prompt is in flight; cancel or await completion first",
            ));
        }
        let Ok(_guard) = self.inner.op_lock.try_lock() else {
            return Err(JsonRpcError::new(
                JSONRPC_INTERNAL_ERROR,
                "cannot start a new session while another session operation is in progress",
            ));
        };
        let sessions_dir = self.inner.config.state_dir.join("sessions");
        let journal = if let Some(resume_str) = params.get("resume").and_then(Value::as_str) {
            let raw_path = PathBuf::from(resume_str);
            let canon_sessions = sessions_dir
                .canonicalize()
                .unwrap_or_else(|_| sessions_dir.clone());
            let canon_target = raw_path.canonicalize().map_err(|e| {
                JsonRpcError::new(
                    JSONRPC_INVALID_PARAMS,
                    format!("failed to resolve session WAL {}: {}", raw_path.display(), e),
                )
            })?;
            if !canon_target.starts_with(&canon_sessions) {
                return Err(JsonRpcError::new(
                    JSONRPC_INVALID_PARAMS,
                    format!(
                        "session WAL {} is outside the session store {}",
                        raw_path.display(),
                        sessions_dir.display()
                    ),
                ));
            }
            let mut j = SessionJournal::open(&canon_target).map_err(|e| {
                JsonRpcError::new(
                    JSONRPC_INVALID_PARAMS,
                    format!("failed to open session WAL {}: {}", raw_path.display(), e),
                )
            })?;
            if j.workspace_root() != self.inner.env.workspace().root() {
                return Err(JsonRpcError::new(
                    JSONRPC_INVALID_PARAMS,
                    format!(
                        "session WAL belongs to workspace {}, not {}",
                        j.workspace_root().display(),
                        self.inner.env.workspace().root().display()
                    ),
                ));
            }
            let _ = self.inner.snapshot_store.reconcile_with_wal(&mut j);
            j
        } else {
            SessionJournal::create(sessions_dir, self.inner.env.workspace().root()).map_err(|e| {
                JsonRpcError::new(
                    JSONRPC_INTERNAL_ERROR,
                    format!("failed to create session WAL: {}", e),
                )
            })?
        };
        let next_snap = Self::snapshot_from_journal(&journal);
        drop(journal);
        let session_id = next_snap.session_id.clone();
        let turn_count = next_snap.turn_count;
        let latest_rev = next_snap.latest_revision_id.clone();
        *self.inner.status.lock().unwrap() = next_snap;

        Ok(json!({
            "session_id": session_id,
            "workspace_root": self.inner.env.workspace().root().display().to_string(),
            "policy_mode": self.inner.policy_engine.mode().as_str(),
            "turn_count": turn_count,
            "latest_revision_id": latest_rev,
        }))
    }

    async fn rpc_session_prompt(&self, params: &Value) -> Result<Value, JsonRpcError> {
        let prompt_text = params
            .get("prompt")
            .or_else(|| params.get("task"))
            .or_else(|| params.get("message"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                JsonRpcError::new(
                    JSONRPC_INVALID_PARAMS,
                    "missing non-empty 'prompt' (or 'task') string parameter",
                )
            })?
            .to_string();

        if self.inner.shutting_down.load(Ordering::SeqCst) {
            return Err(JsonRpcError::new(
                JSONRPC_INTERNAL_ERROR,
                "runtime is shutting down",
            ));
        }

        let Ok(_guard) = self.inner.op_lock.try_lock() else {
            return Err(JsonRpcError::new(
                JSONRPC_INTERNAL_ERROR,
                "a session prompt or mutation is already in flight",
            ));
        };
        if self.inner.in_flight.swap(true, Ordering::SeqCst) {
            return Err(JsonRpcError::new(
                JSONRPC_INTERNAL_ERROR,
                "a session prompt is already in flight",
            ));
        }
        let _in_flight_guard = InFlightPromptGuard {
            inner: Arc::clone(&self.inner),
        };
        let cancel_rx = self.ensure_active_cancel_receiver();

        let session_path = self.session_path();
        let mut journal = SessionJournal::open(&session_path).map_err(|e| {
            JsonRpcError::new(
                JSONRPC_INTERNAL_ERROR,
                format!("failed to open session WAL {}: {}", session_path.display(), e),
            )
        })?;
        let _ = self.inner.snapshot_store.reconcile_with_wal(&mut journal);

        let mut prompt_builder =
            PromptBuilder::default().with_workspace_context(self.inner.env.workspace());
        let activated_skills = self.inner.status.lock().unwrap().activated_skills.clone();
        for skill_name in &activated_skills {
            if let Ok(pb) =
                prompt_builder.clone().with_activated_skill(self.inner.env.workspace(), skill_name)
            {
                prompt_builder = pb;
            }
        }

        let mut subtree_tracker =
            SubtreeContextTracker::new(self.inner.config.subtree_context_enabled);
        if let Some(cp) = journal.latest_active_checkpoint() {
            subtree_tracker.restore_loaded_dirs(cp.subtree_dirs.clone());
        }

        let mut run_cfg = AgentRunConfig::new(prompt_text, self.inner.config.budget.clone());
        run_cfg.stream = self.inner.config.stream;
        run_cfg.strict_log = self.inner.config.strict_log;

        let mut logger = EventLogger::new(
            run_cfg.run_id,
            Some(self.inner.config.state_dir.clone()),
            self.inner.config.strict_log,
        )
        .map_err(|e| JsonRpcError::new(JSONRPC_INTERNAL_ERROR, e.to_string()))?;
        logger.set_live_stderr(false);

        let session_id_for_sink = journal.session_id().to_string();
        let inner_for_sink = Arc::clone(&self.inner);
        logger.set_event_sink(Arc::new(move |ev: &DiagnosticEvent| {
            let mut payload = serde_json::to_value(ev).unwrap_or(Value::Null);
            if let Value::Object(ref mut map) = payload {
                map.insert(
                    "session_id".to_string(),
                    Value::String(session_id_for_sink.clone()),
                );
            }
            Self::broadcast_to_subscribers(
                &inner_for_sink,
                JsonRpcNotification::new("harness/event", payload),
            );
        }));

        let _ = logger.log(EventPayload::SurfaceStarted {
            surface: self.inner.config.surface_name.clone(),
            transport: self.inner.config.transport_name.clone(),
        });

        let runtime = build_runtime_with_registry(
            self.inner.registry.clone(),
            Arc::clone(&self.inner.env) as Arc<dyn ExecutionEnv>,
            self.inner.policy_engine.clone(),
            self.inner.config.budget.clone(),
        )
        .with_predictive_check(self.inner.config.predictive_check);

        let mut runner = AgentRunner::new(
            run_cfg,
            Arc::clone(&self.inner.provider),
            runtime,
            cancel_rx,
        )
        .with_prompt_builder(prompt_builder)
        .with_session_journal(journal)
        .with_snapshot_store(Arc::clone(&self.inner.snapshot_store))
        .with_subtree_context(subtree_tracker)
        .with_completion_gate(Arc::clone(&self.inner.config.completion_gate))
        .with_completion_gate_max_retries(self.inner.config.completion_gate_max_retries)
        .with_max_subagents(self.inner.config.max_subagents);

        if let Some(ref pred) = self.inner.config.predict_cmd {
            if self.inner.policy_engine.mode() != PolicyMode::ReadOnly
                && self.inner.config.predictive_check
            {
                runner = runner.with_predictive_command(Arc::clone(pred));
            }
        }

        let (summary, _logger) = runner.run(logger).await;

        drop(_in_flight_guard);

        if let Ok(reopened) = SessionJournal::open(&session_path) {
            self.update_snapshot_from_journal(&reopened);
        }
        {
            let mut snap = self.inner.status.lock().unwrap();
            snap.total_input_tokens = snap
                .total_input_tokens
                .saturating_add(summary.total_input_tokens);
            snap.total_cached_input_tokens = snap
                .total_cached_input_tokens
                .saturating_add(summary.total_cached_input_tokens);
            snap.total_output_tokens = snap
                .total_output_tokens
                .saturating_add(summary.total_output_tokens);
        }

        let snap = self.inner.status.lock().unwrap().clone();
        let status_str = match summary.status {
            RunStatus::Completed => "completed",
            RunStatus::Cancelled => "cancelled",
            RunStatus::BudgetExhausted => "budget_exhausted",
            RunStatus::Failed => "failed",
            _ => "failed",
        };

        Ok(json!({
            "session_id": snap.session_id,
            "run_id": summary.run_id.to_string(),
            "status": status_str,
            "terminal_reason": format!("{:?}", summary.terminal_reason),
            "final_text": summary.final_assistant_text,
            "turns_used": summary.turns_used,
            "tool_calls_used": summary.tool_calls_used,
            "total_input_tokens": summary.total_input_tokens,
            "total_cached_input_tokens": summary.total_cached_input_tokens,
            "total_output_tokens": summary.total_output_tokens,
            "cache_hit_ratio": summary.cache_hit_ratio,
            "modified_files": summary.modified_files,
            "latest_revision_id": snap.latest_revision_id,
            "pending_approval": self.inner.approval_broker.pending_requests(),
        }))
    }

    async fn rpc_approval_pending(&self) -> Result<Value, JsonRpcError> {
        let pending = self.inner.approval_broker.pending_requests();
        Ok(json!({
            "pending": pending,
        }))
    }

    async fn rpc_approval_respond(&self, params: &Value) -> Result<Value, JsonRpcError> {
        let req_id = params.get("request_id").and_then(Value::as_str);
        let decision = params
            .get("decision")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                JsonRpcError::new(
                    JSONRPC_INVALID_PARAMS,
                    "missing 'decision' parameter ('approve_once' | 'approve_session' | 'deny')",
                )
            })?;
        let res = self
            .inner
            .approval_broker
            .respond(req_id, decision)
            .map_err(|e| JsonRpcError::new(JSONRPC_INVALID_PARAMS, e))?;
        Ok(serde_json::to_value(res).unwrap_or(Value::Null))
    }

    async fn rpc_session_cancel(&self) -> Result<Value, JsonRpcError> {
        let had_active = {
            let guard = self.inner.active_cancel.lock().unwrap();
            if let Some((ref tx, _)) = *guard {
                let _ = tx.send(true);
                true
            } else {
                false
            }
        };
        self.inner.approval_broker.cancel_all_pending();
        Ok(json!({
            "session_id": self.session_id(),
            "cancelled": true,
            "was_in_flight": had_active,
        }))
    }

    async fn rpc_session_status(&self) -> Result<Value, JsonRpcError> {
        if let Ok(_guard) = self.inner.op_lock.try_lock() {
            let path = self.session_path();
            if let Ok(mut journal) = SessionJournal::open(&path) {
                let _ = self.inner.snapshot_store.reconcile_with_wal(&mut journal);
                self.update_snapshot_from_journal(&journal);
            }
        }
        let snap = self.inner.status.lock().unwrap().clone();
        let pending = self.inner.approval_broker.pending_requests();
        let cache_hit_ratio = if snap.total_input_tokens > 0 {
            snap.total_cached_input_tokens as f64 / snap.total_input_tokens as f64
        } else {
            0.0
        };
        Ok(json!({
            "session_id": snap.session_id,
            "workspace_root": self.inner.env.workspace().root().display().to_string(),
            "policy_mode": self.inner.policy_engine.mode().as_str(),
            "in_flight": self.inner.in_flight.load(Ordering::SeqCst),
            "turn_count": snap.turn_count,
            "latest_revision_id": snap.latest_revision_id,
            "subtree_dirs": snap.subtree_dirs,
            "mutations": snap.mutations,
            "subagents_spawned": snap.subagents_spawned,
            "activated_skills": snap.activated_skills,
            "pending_approvals": pending,
            "total_input_tokens": snap.total_input_tokens,
            "total_cached_input_tokens": snap.total_cached_input_tokens,
            "total_output_tokens": snap.total_output_tokens,
            "cache_hit_ratio": cache_hit_ratio,
        }))
    }

    async fn rpc_session_fork(&self, params: &Value) -> Result<Value, JsonRpcError> {
        let turn = params
            .get("turn")
            .and_then(Value::as_u64)
            .ok_or_else(|| {
                JsonRpcError::new(
                    JSONRPC_INVALID_PARAMS,
                    "missing non-negative integer 'turn' parameter for session.fork",
                )
            })? as usize;

        if self.inner.in_flight.load(Ordering::SeqCst) {
            return Err(JsonRpcError::new(
                JSONRPC_INTERNAL_ERROR,
                "cannot fork session while a prompt is in flight; cancel or await completion first",
            ));
        }
        let Ok(_guard) = self.inner.op_lock.try_lock() else {
            return Err(JsonRpcError::new(
                JSONRPC_INTERNAL_ERROR,
                "cannot fork session while another session operation is in progress",
            ));
        };
        let path = self.session_path();
        let mut journal = SessionJournal::open(&path).map_err(|e| {
            JsonRpcError::new(
                JSONRPC_INTERNAL_ERROR,
                format!("failed to open session WAL: {}", e),
            )
        })?;
        let _ = self.inner.snapshot_store.reconcile_with_wal(&mut journal);
        let leaf_id = journal.fork_at_turn(turn).map_err(|e| {
            JsonRpcError::new(
                JSONRPC_INVALID_PARAMS,
                format!("failed to fork session at turn {}: {}", turn, e),
            )
        })?;
        self.update_snapshot_from_journal(&journal);
        let snap = self.inner.status.lock().unwrap().clone();
        Ok(json!({
            "session_id": snap.session_id,
            "forked_at_turn": turn,
            "turn_count": snap.turn_count,
            "leaf_id": leaf_id,
            "latest_revision_id": snap.latest_revision_id,
        }))
    }

    async fn rpc_revision_list(&self) -> Result<Value, JsonRpcError> {
        if let Ok(_guard) = self.inner.op_lock.try_lock() {
            let path = self.session_path();
            if let Ok(mut journal) = SessionJournal::open(&path) {
                let _ = self.inner.snapshot_store.reconcile_with_wal(&mut journal);
                self.update_snapshot_from_journal(&journal);
            }
        }
        let snap = self.inner.status.lock().unwrap().clone();
        let current_idx = snap
            .revisions
            .iter()
            .rposition(|r| Some(&r.revision_id) == snap.latest_revision_id.as_ref());
        let revisions: Vec<Value> = snap
            .revisions
            .iter()
            .enumerate()
            .map(|(idx, r)| {
                json!({
                    "turn": r.turn,
                    "revision_id": r.revision_id,
                    "parent_revision_id": r.parent_revision_id,
                    "changed_paths": r.changed_paths,
                    "tracked_file_count": r.tracked_file_count,
                    "captured_at_ms": r.captured_at_ms,
                    "is_current": Some(idx) == current_idx,
                })
            })
            .collect();
        let store_revisions: Vec<WorkspaceRevision> = self
            .inner
            .snapshot_store
            .list_revisions()
            .unwrap_or_default();

        Ok(json!({
            "session_id": snap.session_id,
            "latest_revision_id": snap.latest_revision_id,
            "revisions": revisions,
            "store_revision_count": store_revisions.len(),
        }))
    }

    async fn rpc_revision_undo(&self, params: &Value) -> Result<Value, JsonRpcError> {
        let force = params
            .get("force")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if self.inner.in_flight.load(Ordering::SeqCst) {
            return Err(JsonRpcError::new(
                JSONRPC_INTERNAL_ERROR,
                "cannot undo workspace revision while a prompt is in flight; cancel or await completion first",
            ));
        }
        let Ok(_guard) = self.inner.op_lock.try_lock() else {
            return Err(JsonRpcError::new(
                JSONRPC_INTERNAL_ERROR,
                "cannot undo workspace revision while another session operation is in progress",
            ));
        };
        let path = self.session_path();
        let mut journal = SessionJournal::open(&path).map_err(|e| {
            JsonRpcError::new(
                JSONRPC_INTERNAL_ERROR,
                format!("failed to open session WAL: {}", e),
            )
        })?;
        let _ = self.inner.snapshot_store.reconcile_with_wal(&mut journal);
        let records = journal.revision_records();
        let Some(current) = records.last().cloned() else {
            return Err(JsonRpcError::new(
                JSONRPC_INVALID_PARAMS,
                "no workspace revision recorded for this session",
            ));
        };
        let Some(previous) = records
            .iter()
            .rev()
            .find(|r| r.revision_id != current.revision_id)
            .cloned()
        else {
            return Err(JsonRpcError::new(
                JSONRPC_INVALID_PARAMS,
                format!(
                    "nothing to undo; workspace is unchanged since revision {}",
                    current.revision_id
                ),
            ));
        };
        let target = RevisionId::parse(&previous.revision_id)
            .map_err(|e| JsonRpcError::new(JSONRPC_INTERNAL_ERROR, e.to_string()))?;
        let expected = RevisionId::parse(&current.revision_id)
            .map_err(|e| JsonRpcError::new(JSONRPC_INTERNAL_ERROR, e.to_string()))?;

        self.apply_restore_operation(
            &mut journal,
            &target,
            Some(&expected),
            Some(previous.turn),
            force,
        )
    }

    async fn rpc_revision_restore(&self, params: &Value) -> Result<Value, JsonRpcError> {
        let raw_rev = params
            .get("revision_id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                JsonRpcError::new(
                    JSONRPC_INVALID_PARAMS,
                    "missing 'revision_id' parameter for revision.restore",
                )
            })?;
        let force = params
            .get("force")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let target = RevisionId::parse(raw_rev)
            .map_err(|e| JsonRpcError::new(JSONRPC_INVALID_PARAMS, e.to_string()))?;

        if self.inner.in_flight.load(Ordering::SeqCst) {
            return Err(JsonRpcError::new(
                JSONRPC_INTERNAL_ERROR,
                "cannot restore workspace revision while a prompt is in flight; cancel or await completion first",
            ));
        }
        let Ok(_guard) = self.inner.op_lock.try_lock() else {
            return Err(JsonRpcError::new(
                JSONRPC_INTERNAL_ERROR,
                "cannot restore workspace revision while another session operation is in progress",
            ));
        };
        let path = self.session_path();
        let mut journal = SessionJournal::open(&path).map_err(|e| {
            JsonRpcError::new(
                JSONRPC_INTERNAL_ERROR,
                format!("failed to open session WAL: {}", e),
            )
        })?;
        let _ = self.inner.snapshot_store.reconcile_with_wal(&mut journal);
        let expected = journal
            .latest_revision_id()
            .and_then(|id| RevisionId::parse(&id).ok());

        self.apply_restore_operation(&mut journal, &target, expected.as_ref(), None, force)
    }

    fn apply_restore_operation(
        &self,
        journal: &mut SessionJournal,
        target: &RevisionId,
        expected: Option<&RevisionId>,
        rewind_turn: Option<usize>,
        force: bool,
    ) -> Result<Value, JsonRpcError> {
        let workspace = self.inner.env.workspace();
        let store = &self.inner.snapshot_store;

        let restore_res = match store.pending_restore() {
            Ok(Some(intent)) if &intent.target_revision == target => {
                store.recover_pending_restore(workspace)
            }
            Ok(Some(intent)) => {
                return Err(JsonRpcError::with_data(
                    JSONRPC_RECOVERY_REQUIRED,
                    format!(
                        "an interrupted restore of {} must be completed before restoring {}",
                        intent.target_revision, target
                    ),
                    json!({
                        "pending_target_revision": intent.target_revision.as_str(),
                        "intent_journal": store.intent_path().display().to_string(),
                    }),
                ));
            }
            Ok(None) => store.restore_revision(workspace, target, expected, force),
            Err(e) => Err(e),
        }
        .map_err(|e| JsonRpcError::new(JSONRPC_INTERNAL_ERROR, format!("restore failed: {}", e)))?;

        match restore_res {
            RestoreResult::Applied {
                restored_revision,
                restored_files,
                removed_files,
            } => {
                if let Ok(mut logger) = EventLogger::new(
                    RunId::new(),
                    Some(self.inner.config.state_dir.clone()),
                    self.inner.config.strict_log,
                ) {
                    let session_id_for_sink = journal.session_id().to_string();
                    let inner_for_sink = Arc::clone(&self.inner);
                    logger.set_event_sink(Arc::new(move |ev: &DiagnosticEvent| {
                        let mut payload = serde_json::to_value(ev).unwrap_or(Value::Null);
                        if let Value::Object(ref mut map) = payload {
                            map.insert(
                                "session_id".to_string(),
                                Value::String(session_id_for_sink.clone()),
                            );
                        }
                        Self::broadcast_to_subscribers(
                            &inner_for_sink,
                            JsonRpcNotification::new("harness/event", payload),
                        );
                    }));
                    let _ = logger.log(EventPayload::RevisionRestored {
                        revision_id: restored_revision.as_str().to_string(),
                        restored_files: restored_files.len(),
                        removed_files: removed_files.len(),
                    });
                }

                if let Some(turn) = rewind_turn {
                    journal.fork_at_turn(turn).map_err(|e| {
                        JsonRpcError::new(
                            JSONRPC_INTERNAL_ERROR,
                            format!("workspace restored but session leaf could not be rewound: {}", e),
                        )
                    })?;
                    if journal.latest_revision_id().as_deref() != Some(restored_revision.as_str()) {
                        Self::record_restored_revision_in_wal(
                            journal,
                            store,
                            &restored_revision,
                            &restored_files,
                            &removed_files,
                            expected,
                        )
                        .map_err(|e| JsonRpcError::new(JSONRPC_INTERNAL_ERROR, e))?;
                    }
                } else {
                    Self::record_restored_revision_in_wal(
                        journal,
                        store,
                        &restored_revision,
                        &restored_files,
                        &removed_files,
                        expected,
                    )
                    .map_err(|e| JsonRpcError::new(JSONRPC_INTERNAL_ERROR, e))?;
                }

                self.update_snapshot_from_journal(journal);
                Ok(json!({
                    "status": "applied",
                    "restored_revision": restored_revision.as_str(),
                    "restored_files": restored_files,
                    "removed_files": removed_files,
                    "rewound_turn": rewind_turn,
                }))
            }
            RestoreResult::Conflict {
                target_revision,
                conflicting_paths,
            } => {
                let conflicts: Vec<Value> = conflicting_paths
                    .iter()
                    .map(|c| {
                        json!({
                            "path": c.path,
                            "kind": format!("{:?}", c.kind),
                        })
                    })
                    .collect();
                Err(JsonRpcError::with_data(
                    JSONRPC_RESTORE_CONFLICT,
                    format!(
                        "refusing to restore {}; {} path(s) modified externally (pass force=true to overwrite)",
                        target_revision,
                        conflicts.len()
                    ),
                    json!({
                        "target_revision": target_revision.as_str(),
                        "conflicts": conflicts,
                    }),
                ))
            }
            RestoreResult::RecoveryRequired {
                target_revision,
                journal_path,
                reason,
            } => Err(JsonRpcError::with_data(
                JSONRPC_RECOVERY_REQUIRED,
                format!(
                    "restore of {} requires recovery: {}",
                    target_revision, reason
                ),
                json!({
                    "target_revision": target_revision.as_str(),
                    "intent_journal": journal_path.display().to_string(),
                    "reason": reason,
                }),
            )),
        }
    }

    fn record_restored_revision_in_wal(
        journal: &mut SessionJournal,
        store: &SnapshotStore,
        restored_revision: &RevisionId,
        restored_files: &[String],
        removed_files: &[String],
        expected: Option<&RevisionId>,
    ) -> Result<(), String> {
        let (changed_paths, tracked_file_count) = match store.load_revision(restored_revision) {
            Ok(manifest) => (manifest.changed_paths.clone(), manifest.tracked_files.len()),
            Err(_) => {
                let mut paths: Vec<String> = restored_files
                    .iter()
                    .chain(removed_files.iter())
                    .cloned()
                    .collect();
                paths.sort();
                paths.dedup();
                (paths, restored_files.len())
            }
        };
        let turn = journal.max_active_turn();
        journal
            .record_revision_capture(
                turn,
                restored_revision.as_str(),
                expected.map(|e| e.as_str()),
                changed_paths,
                tracked_file_count,
            )
            .map_err(|e| e.to_string())
    }

    async fn rpc_evidence_recall(&self, params: &Value) -> Result<Value, JsonRpcError> {
        let path = self.session_path();
        let journal = SessionJournal::open(&path).map_err(|e| {
            JsonRpcError::new(
                JSONRPC_INTERNAL_ERROR,
                format!("failed to open session WAL: {}", e),
            )
        })?;

        let mut projector = ContextProjector::new();
        journal.seed_projector_evidence(&mut projector);

        // Also register uncompacted tool results from the active branch so IDE/ACP clients can
        // inspect any tool call's evidence (including `source_entry_id` pointers in `MutationRecord`
        // before the session crosses the compaction watermark).
        for (index, entry) in journal.active_entries().iter().enumerate() {
            let raw_item = journal.raw_entry_item(&entry.id).unwrap_or(&entry.item);
            if let TranscriptItem::ToolResult(tr) = raw_item {
                let sid = source_entry_id(index, tr.call_id.as_str());
                if projector.evidence_link(&sid).is_none() {
                    let link = EvidenceLink {
                        source_entry_id: sid,
                        canonical_index: index,
                        call_id: tr.call_id.0.clone(),
                        tool_name: tr.tool_name.clone(),
                        relative_path: tr.meta.relative_path.clone(),
                        original_sha256: sha256_hex64(tr.content.as_bytes()),
                        original_bytes: tr.content.len(),
                        original_lines: tr.content.lines().count(),
                        stub_bytes: tr.content.len(),
                        reason: CompactionReason::StepDownArchive,
                    };
                    projector.register_evidence(link, Arc::clone(&tr.content));
                }
            }
        }

        let available_ids: Vec<String> = projector
            .evidence_links()
            .iter()
            .map(|l| l.source_entry_id.clone())
            .collect();

        let requested_id = params
            .get("source_entry_id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let resolved_id = match requested_id {
            Some("latest") => available_ids.last().cloned().ok_or_else(|| {
                JsonRpcError::new(
                    JSONRPC_INVALID_PARAMS,
                    "no tool evidence recorded in this session yet",
                )
            })?,
            Some(id) => id.to_string(),
            None => {
                return Err(JsonRpcError::new(
                    JSONRPC_INVALID_PARAMS,
                    "missing 'source_entry_id' parameter for evidence.recall",
                ))
            }
        };

        let start_line = params
            .get("start_line")
            .or_else(|| params.get("offset"))
            .and_then(Value::as_u64)
            .map(|v| (v as usize).max(1));
        let end_line = match (
            params.get("end_line").and_then(Value::as_u64),
            params.get("limit").and_then(Value::as_u64),
        ) {
            (Some(end), _) => Some(end as usize),
            (None, Some(limit)) => {
                let s = start_line.unwrap_or(1);
                Some(s.saturating_add((limit as usize).saturating_sub(1)))
            }
            (None, None) => None,
        };
        let pattern = params
            .get("pattern")
            .and_then(Value::as_str)
            .map(|s| s.to_string());

        let mut call_args = serde_json::Map::new();
        call_args.insert("source_entry_id".to_string(), Value::String(resolved_id.clone()));
        if let Some(s) = start_line {
            call_args.insert("start_line".to_string(), json!(s));
        }
        if let Some(e) = end_line {
            call_args.insert("end_line".to_string(), json!(e));
        }
        if let Some(p) = pattern {
            call_args.insert("pattern".to_string(), Value::String(p));
        }

        let raw_args = Value::Object(call_args).to_string();
        let out = execute_recall_evidence(&projector, &raw_args);
        if !out.outcome.is_success() {
            return Err(JsonRpcError::with_data(
                JSONRPC_INVALID_PARAMS,
                out.content,
                json!({ "available_evidence_ids": available_ids }),
            ));
        }

        Ok(json!({
            "source_entry_id": resolved_id,
            "ok": true,
            "content": out.content,
            "truncated": out.meta.truncated,
            "available_evidence_ids": available_ids,
        }))
    }

    async fn rpc_skills_list(&self) -> Result<Value, JsonRpcError> {
        let discovered = discover_skills(self.inner.env.workspace());
        let activated = self.inner.status.lock().unwrap().activated_skills.clone();
        let skills: Vec<Value> = discovered
            .iter()
            .map(|s| {
                json!({
                    "name": s.name,
                    "description": s.description,
                    "path": s.path,
                    "activated": activated.contains(&s.name),
                })
            })
            .collect();
        Ok(json!({ "skills": skills }))
    }

    async fn rpc_skills_activate(&self, params: &Value) -> Result<Value, JsonRpcError> {
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                JsonRpcError::new(
                    JSONRPC_INVALID_PARAMS,
                    "missing 'name' parameter for skills.activate",
                )
            })?;
        let raw_args = json!({ "name": name }).to_string();
        let catalog = discover_skills(self.inner.env.workspace());
        let out = execute_activate_skill(&catalog, &raw_args);
        if !out.outcome.is_success() {
            return Err(JsonRpcError::new(JSONRPC_INVALID_PARAMS, out.content));
        }
        {
            let mut snap = self.inner.status.lock().unwrap();
            if !snap.activated_skills.iter().any(|s| s == name) {
                snap.activated_skills.push(name.to_string());
            }
        }
        Ok(json!({
            "name": name,
            "activated": true,
            "content": out.content,
        }))
    }

    async fn rpc_shutdown(&self) -> Result<Value, JsonRpcError> {
        self.inner.shutting_down.store(true, Ordering::SeqCst);
        let _ = self.rpc_session_cancel().await;
        for ext_id in &self.inner.mounted_extensions {
            let _ = self.inner.registry.unregister_extension(ext_id);
        }
        Ok(json!({
            "shutdown": true,
            "session_id": self.session_id(),
        }))
    }

    /// Serve newline-delimited JSON-RPC 2.0 over a bidirectional async stream (`harness acp` stdio
    /// or `harness serve` TCP/Unix connection).
    ///
    /// Long-running `session.prompt` / `session.start` / `acp.prompt` requests are spawned onto a
    /// background task so the stream loop continues processing incoming `approval.pending`,
    /// `approval.respond`, `session.cancel`, and `session.status` frames while a prompt is in
    /// flight. If an authenticated client stream exits (EOF, frame error, or HTTP/CSRF abort) while
    /// an approval is pending or a prompt is in flight, the approval broker immediately closes and
    /// fails closed (`ApprovalDecision::Deny`) and any active run is cancelled.
    pub async fn serve_stream<R, W>(&self, reader: R, mut writer: W) -> Result<(), String>
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<String>();

        let writer_task = tokio::spawn(async move {
            while let Some(line) = out_rx.recv().await {
                if writer.write_all(line.as_bytes()).await.is_err() {
                    break;
                }
                if writer.write_all(b"\n").await.is_err() {
                    break;
                }
                if writer.flush().await.is_err() {
                    break;
                }
            }
        });

        let mut buf_reader = BufReader::new(reader);
        let mut connection_authenticated = self.inner.config.auth_token.is_none();
        let mut sub_id: Option<u64> = None;
        let mut in_flight_prompts = Vec::new();

        let activate_authenticated_stream = |sub_id_slot: &mut Option<u64>| {
            if sub_id_slot.is_none() {
                self.inner.approval_broker.reopen();
                let out_tx_notes = out_tx.clone();
                let id = self
                    .inner
                    .next_subscriber_id
                    .fetch_add(1, Ordering::SeqCst);
                self.inner.subscribers.lock().unwrap().push((
                    id,
                    Arc::new(move |note| {
                        if let Ok(serialized) = serde_json::to_string(note) {
                            out_tx_notes.send(serialized).is_ok()
                        } else {
                            true
                        }
                    }),
                ));
                *sub_id_slot = Some(id);
            }
        };

        if connection_authenticated {
            activate_authenticated_stream(&mut sub_id);
        }

        loop {
            let line_opt = read_bounded_jsonrpc_line(&mut buf_reader, MAX_RPC_FRAME_BYTES).await;
            let line = match line_opt {
                Ok(Some(l)) => l,
                Ok(None) => {
                    break;
                }
                Err(frame_err) => {
                    let resp = JsonRpcResponse::error(
                        Value::Null,
                        JsonRpcError::new(JSONRPC_PARSE_ERROR, frame_err),
                    );
                    if let Ok(s) = serde_json::to_string(&resp) {
                        let _ = out_tx.send(s);
                    }
                    break;
                }
            };

            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            // Reject HTTP request lines (CSRF / browser cross-origin protection on TCP socket).
            if looks_like_http_request(trimmed) {
                let resp = JsonRpcResponse::error(
                    Value::Null,
                    JsonRpcError::new(
                        JSONRPC_INVALID_REQUEST,
                        "HTTP requests are rejected on raw JSON-RPC transport",
                    ),
                );
                if let Ok(s) = serde_json::to_string(&resp) {
                    let _ = out_tx.send(s);
                }
                break;
            }

            let req: JsonRpcRequest = match serde_json::from_str(trimmed) {
                Ok(r) => r,
                Err(e) => {
                    let resp = JsonRpcResponse::error(
                        Value::Null,
                        JsonRpcError::new(JSONRPC_PARSE_ERROR, format!("invalid JSON-RPC: {}", e)),
                    );
                    if let Ok(s) = serde_json::to_string(&resp) {
                        let _ = out_tx.send(s);
                    }
                    continue;
                }
            };

            if !connection_authenticated {
                let expected = self.inner.config.auth_token.as_deref().unwrap_or("");
                let provided = req
                    .params
                    .get("auth_token")
                    .or_else(|| req.params.get("token"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if constant_time_eq(provided.as_bytes(), expected.as_bytes()) {
                    connection_authenticated = true;
                    activate_authenticated_stream(&mut sub_id);
                } else {
                    let resp = JsonRpcResponse::error(
                        req.id.unwrap_or(Value::Null),
                        JsonRpcError::new(
                            JSONRPC_UNAUTHORIZED,
                            "unauthorized: missing or invalid auth_token",
                        ),
                    );
                    if let Ok(s) = serde_json::to_string(&resp) {
                        let _ = out_tx.send(s);
                    }
                    continue;
                }
            }

            let is_shutdown = matches!(req.method.as_str(), "shutdown" | "acp.shutdown");
            let is_async_prompt = matches!(
                req.method.as_str(),
                "session.start" | "session.prompt" | "acp.prompt" | "session/prompt"
            );

            if is_async_prompt {
                if !self.inner.in_flight.load(Ordering::SeqCst) {
                    let _ = self.ensure_active_cancel_receiver();
                }
                let dispatcher = self.clone();
                let out_tx_prompt = out_tx.clone();
                let handle = tokio::spawn(async move {
                    let resp = dispatcher.handle_request(req).await;
                    if let Ok(s) = serde_json::to_string(&resp) {
                        let _ = out_tx_prompt.send(s);
                    }
                });
                in_flight_prompts.push(handle);
            } else {
                let resp = self.handle_request(req).await;
                if let Ok(s) = serde_json::to_string(&resp) {
                    let _ = out_tx.send(s);
                }
                if is_shutdown {
                    break;
                }
            }
        }

        if connection_authenticated {
            self.inner.approval_broker.close();
            let _ = self.rpc_session_cancel().await;
        }
        for handle in in_flight_prompts {
            let _ = handle.await;
        }
        if let Some(id) = sub_id {
            self.inner
                .subscribers
                .lock()
                .unwrap()
                .retain(|(existing_id, _)| *existing_id != id);
        }
        drop(out_tx);
        let _ = writer_task.await;
        Ok(())
    }
}

fn looks_like_http_request(trimmed: &str) -> bool {
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

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
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

async fn read_bounded_jsonrpc_line<R: AsyncBufRead + Unpin>(
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

/// Validated listen endpoint for `harness serve` (loopback/token-guarded TCP or local `0600` Unix
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

/// Validate a `harness serve` listen endpoint (`127.0.0.1:<PORT>`, `unix:///path/to/sock`, or
/// `/path/to/harness.sock`) against the Gate 6 security policy.
///
/// Non-loopback TCP bind addresses (`0.0.0.0`, LAN IPs, `[::]`) are strictly rejected unless a
/// non-empty `--auth-token` is configured.
pub fn validate_daemon_listen_endpoint(
    listen_addr: &str,
    auth_token: Option<&str>,
) -> Result<DaemonListenEndpoint, String> {
    let trimmed = listen_addr.trim();
    if trimmed.is_empty() {
        return Err("empty listen address for harness serve".to_string());
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

/// Validate a `harness serve` listen address against the Gate 6 security policy.
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

/// Local TCP or Unix Domain Socket JSON-RPC 2.0 daemon (`harness serve`).
pub struct HarnessDaemon {
    listener: DaemonListenerKind,
    local_addr: SocketAddr,
    unix_path: Option<PathBuf>,
    dispatcher: RpcDispatcher,
}

impl HarnessDaemon {
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

/// Output of a single interactive REPL input line (`InteractiveRepl::handle_line`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplStepOutcome {
    pub output: String,
    pub should_exit: bool,
}

/// Stateless interactive REPL / TUI consumer (`harness repl` / `harness tui` / `--interactive`).
///
/// Per Blueprint v2.0 Gate 6 Invariant #3, `InteractiveRepl` holds zero private session state:
/// every slash command and prompt turn is dispatched as a JSON-RPC 2.0 request to [`RpcDispatcher`].
pub struct InteractiveRepl {
    dispatcher: RpcDispatcher,
}

impl InteractiveRepl {
    pub fn new(dispatcher: RpcDispatcher) -> Self {
        Self { dispatcher }
    }

    pub fn dispatcher(&self) -> &RpcDispatcher {
        &self.dispatcher
    }

    /// Render the top status banner by querying `session.status` via [`RpcDispatcher`].
    pub async fn render_status_banner(&self) -> String {
        let resp = self
            .dispatcher
            .handle_request(JsonRpcRequest::new(0, "session.status", json!({})))
            .await;
        let result = resp.result.unwrap_or_else(|| json!({}));
        let session_id = result
            .get("session_id")
            .and_then(Value::as_str)
            .unwrap_or("<unknown>");
        let ws = result
            .get("workspace_root")
            .and_then(Value::as_str)
            .unwrap_or("<unknown>");
        let policy = result
            .get("policy_mode")
            .and_then(Value::as_str)
            .unwrap_or("ask");
        let turns = result.get("turn_count").and_then(Value::as_u64).unwrap_or(0);
        let rev = result
            .get("latest_revision_id")
            .and_then(Value::as_str)
            .unwrap_or("<none>");
        let in_tok = result
            .get("total_input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let cached_tok = result
            .get("total_cached_input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let hit_pct = result
            .get("cache_hit_ratio")
            .and_then(Value::as_f64)
            .unwrap_or(0.0)
            * 100.0;

        format!(
            "=== Harness v{} Interactive REPL ===\n\
             workspace: {} | policy: {} | session: {}\n\
             turns: {} | revision: {} | tokens: in={} (cached={}, hit={:.1}%)\n\
             Type a task prompt or /help for commands (/status, /revisions, /undo, /skills, /fork, /clear, /quit).\n",
            crate::version(),
            sanitize_terminal_text(ws),
            sanitize_terminal_text(policy),
            sanitize_terminal_text(session_id),
            turns,
            sanitize_terminal_text(rev),
            in_tok,
            cached_tok,
            hit_pct,
        )
    }

    /// Process one user input line (either a `/slash` command or a prompt turn) through
    /// [`RpcDispatcher`].
    pub async fn handle_line(&self, raw_line: &str) -> Result<ReplStepOutcome, String> {
        let line = raw_line.trim();
        if line.is_empty() {
            return Ok(ReplStepOutcome {
                output: String::new(),
                should_exit: false,
            });
        }

        if line.starts_with('/') {
            let mut parts = line.split_whitespace();
            let cmd = parts.next().unwrap_or("");
            let rest: Vec<&str> = parts.collect();
            return match cmd {
                "/help" | "/?" => Ok(ReplStepOutcome {
                    output: concat!(
                        "Available commands:\n",
                        "  /help                  Show this command reference\n",
                        "  /status                Show active session, revision, subtree, and token status\n",
                        "  /revisions             List captured workspace revisions for this session\n",
                        "  /undo [--force]        Restore workspace & session leaf to previous revision\n",
                        "  /restore <rev> [--force] Restore workspace to a specific revision ID\n",
                        "  /diff                  Show files changed in the latest revision\n",
                        "  /skills [<name>]       List workspace skills or activate <name>\n",
                        "  /fork <turn>           Rewind/branch active session at <turn>\n",
                        "  /inspect <id>          Recall raw tool evidence by source_entry_id\n",
                        "  /clear                 Start a fresh session in this workspace\n",
                        "  /quit | /exit          Shut down the interactive session\n"
                    )
                    .to_string(),
                    should_exit: false,
                }),
                "/quit" | "/exit" => {
                    let _ = self
                        .dispatcher
                        .handle_request(JsonRpcRequest::new(1, "shutdown", json!({})))
                        .await;
                    Ok(ReplStepOutcome {
                        output: "Session closed.\n".to_string(),
                        should_exit: true,
                    })
                }
                "/status" => {
                    let resp = self
                        .dispatcher
                        .handle_request(JsonRpcRequest::new(1, "session.status", json!({})))
                        .await;
                    let val = resp.result.ok_or_else(|| {
                        resp.error
                            .map(|e| e.message)
                            .unwrap_or_else(|| "status failed".to_string())
                    })?;
                    let sid = val.get("session_id").and_then(Value::as_str).unwrap_or("-");
                    let policy = val.get("policy_mode").and_then(Value::as_str).unwrap_or("-");
                    let turns = val.get("turn_count").and_then(Value::as_u64).unwrap_or(0);
                    let rev = val
                        .get("latest_revision_id")
                        .and_then(Value::as_str)
                        .unwrap_or("<none>");
                    let mut_count = val
                        .get("mutations")
                        .and_then(Value::as_array)
                        .map(|a| a.len())
                        .unwrap_or(0);
                    let sub_count = val
                        .get("subagents_spawned")
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    let out = format!(
                        "session_id: {}\npolicy: {}\nturn_count: {}\nlatest_revision: {}\nmutations: {}\nsubagents_spawned: {}\n",
                        sanitize_terminal_text(sid),
                        sanitize_terminal_text(policy),
                        turns,
                        sanitize_terminal_text(rev),
                        mut_count,
                        sub_count,
                    );
                    Ok(ReplStepOutcome {
                        output: out,
                        should_exit: false,
                    })
                }
                "/revisions" | "/diff" => {
                    let resp = self
                        .dispatcher
                        .handle_request(JsonRpcRequest::new(1, "revision.list", json!({})))
                        .await;
                    let val = resp.result.ok_or_else(|| {
                        resp.error
                            .map(|e| e.message)
                            .unwrap_or_else(|| "revision.list failed".to_string())
                    })?;
                    let revs = val
                        .get("revisions")
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default();
                    if revs.is_empty() {
                        return Ok(ReplStepOutcome {
                            output: "no workspace revisions captured yet.\n".to_string(),
                            should_exit: false,
                        });
                    }
                    let mut out = format!("revisions ({} captured):\n", revs.len());
                    for r in &revs {
                        let turn = r.get("turn").and_then(Value::as_u64).unwrap_or(0);
                        let rid = r.get("revision_id").and_then(Value::as_str).unwrap_or("-");
                        let files = r
                            .get("tracked_file_count")
                            .and_then(Value::as_u64)
                            .unwrap_or(0);
                        let is_cur = r.get("is_current").and_then(Value::as_bool).unwrap_or(false);
                        let changed: Vec<String> = r
                            .get("changed_paths")
                            .and_then(Value::as_array)
                            .map(|arr| {
                                arr.iter()
                                    .filter_map(Value::as_str)
                                    .map(sanitize_terminal_text)
                                    .collect()
                            })
                            .unwrap_or_default();
                        let marker = if is_cur { " <- current" } else { "" };
                        out.push_str(&format!(
                            "  turn {}  {}  files={} changed=[{}]{}\n",
                            turn,
                            sanitize_terminal_text(rid),
                            files,
                            changed.join(", "),
                            marker
                        ));
                    }
                    Ok(ReplStepOutcome {
                        output: out,
                        should_exit: false,
                    })
                }
                "/undo" => {
                    let force = rest.iter().any(|&arg| arg == "--force" || arg == "-f");
                    let resp = self
                        .dispatcher
                        .handle_request(JsonRpcRequest::new(
                            1,
                            "revision.undo",
                            json!({ "force": force }),
                        ))
                        .await;
                    match (resp.result, resp.error) {
                        (Some(val), _) => {
                            let rev = val
                                .get("restored_revision")
                                .and_then(Value::as_str)
                                .unwrap_or("-");
                            let turn = val.get("rewound_turn").and_then(Value::as_u64).unwrap_or(0);
                            Ok(ReplStepOutcome {
                                output: format!(
                                    "restored workspace to {} (rewound to turn {})\n",
                                    sanitize_terminal_text(rev),
                                    turn
                                ),
                                should_exit: false,
                            })
                        }
                        (_, Some(err)) => Ok(ReplStepOutcome {
                            output: format!("undo error: {}\n", sanitize_terminal_text(&err.message)),
                            should_exit: false,
                        }),
                        _ => Err("unexpected empty response from revision.undo".to_string()),
                    }
                }
                "/restore" => {
                    let Some(&rev_id) = rest.first() else {
                        return Ok(ReplStepOutcome {
                            output: "usage: /restore <revision_id> [--force]\n".to_string(),
                            should_exit: false,
                        });
                    };
                    let force = rest.iter().skip(1).any(|&arg| arg == "--force" || arg == "-f");
                    let resp = self
                        .dispatcher
                        .handle_request(JsonRpcRequest::new(
                            1,
                            "revision.restore",
                            json!({ "revision_id": rev_id, "force": force }),
                        ))
                        .await;
                    match (resp.result, resp.error) {
                        (Some(val), _) => {
                            let rev = val
                                .get("restored_revision")
                                .and_then(Value::as_str)
                                .unwrap_or("-");
                            Ok(ReplStepOutcome {
                                output: format!(
                                    "restored workspace to {}\n",
                                    sanitize_terminal_text(rev)
                                ),
                                should_exit: false,
                            })
                        }
                        (_, Some(err)) => Ok(ReplStepOutcome {
                            output: format!(
                                "restore error: {}\n",
                                sanitize_terminal_text(&err.message)
                            ),
                            should_exit: false,
                        }),
                        _ => Err("unexpected empty response from revision.restore".to_string()),
                    }
                }
                "/skills" => {
                    if let Some(&skill_name) = rest.first() {
                        let resp = self
                            .dispatcher
                            .handle_request(JsonRpcRequest::new(
                                1,
                                "skills.activate",
                                json!({ "name": skill_name }),
                            ))
                            .await;
                        return match (resp.result, resp.error) {
                            (Some(val), _) => {
                                let content = val
                                    .get("content")
                                    .and_then(Value::as_str)
                                    .unwrap_or("");
                                Ok(ReplStepOutcome {
                                    output: format!(
                                        "activated skill '{}':\n{}\n",
                                        sanitize_terminal_text(skill_name),
                                        sanitize_terminal_text(content)
                                    ),
                                    should_exit: false,
                                })
                            }
                            (_, Some(err)) => Ok(ReplStepOutcome {
                                output: format!(
                                    "skill error: {}\n",
                                    sanitize_terminal_text(&err.message)
                                ),
                                should_exit: false,
                            }),
                            _ => Err("empty response from skills.activate".to_string()),
                        };
                    }
                    let resp = self
                        .dispatcher
                        .handle_request(JsonRpcRequest::new(1, "skills.list", json!({})))
                        .await;
                    let val = resp.result.unwrap_or_else(|| json!({}));
                    let skills = val
                        .get("skills")
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default();
                    if skills.is_empty() {
                        return Ok(ReplStepOutcome {
                            output: "no skills discovered in .agents/skills/.\n".to_string(),
                            should_exit: false,
                        });
                    }
                    let mut out = format!("skills ({} discovered):\n", skills.len());
                    for s in &skills {
                        let name = s.get("name").and_then(Value::as_str).unwrap_or("-");
                        let desc = s.get("description").and_then(Value::as_str).unwrap_or("");
                        let act = s.get("activated").and_then(Value::as_bool).unwrap_or(false);
                        let tag = if act { " [active]" } else { "" };
                        out.push_str(&format!(
                            "  {}{}: {}\n",
                            sanitize_terminal_text(name),
                            tag,
                            sanitize_terminal_text(desc)
                        ));
                    }
                    Ok(ReplStepOutcome {
                        output: out,
                        should_exit: false,
                    })
                }
                "/fork" => {
                    let Some(turn_str) = rest.first() else {
                        return Ok(ReplStepOutcome {
                            output: "usage: /fork <turn>\n".to_string(),
                            should_exit: false,
                        });
                    };
                    let Ok(turn) = turn_str.parse::<usize>() else {
                        return Ok(ReplStepOutcome {
                            output: "error: /fork expects a non-negative integer turn\n"
                                .to_string(),
                            should_exit: false,
                        });
                    };
                    let resp = self
                        .dispatcher
                        .handle_request(JsonRpcRequest::new(
                            1,
                            "session.fork",
                            json!({ "turn": turn }),
                        ))
                        .await;
                    match (resp.result, resp.error) {
                        (Some(val), _) => {
                            let leaf = val.get("leaf_id").and_then(Value::as_str).unwrap_or("-");
                            Ok(ReplStepOutcome {
                                output: format!(
                                    "forked session at turn {} (leaf {})\n",
                                    turn,
                                    sanitize_terminal_text(leaf)
                                ),
                                should_exit: false,
                            })
                        }
                        (_, Some(err)) => Ok(ReplStepOutcome {
                            output: format!(
                                "fork error: {}\n",
                                sanitize_terminal_text(&err.message)
                            ),
                            should_exit: false,
                        }),
                        _ => Err("empty response from session.fork".to_string()),
                    }
                }
                "/inspect" | "/evidence" => {
                    let sid = rest.first().copied().unwrap_or("latest");
                    let resp = self
                        .dispatcher
                        .handle_request(JsonRpcRequest::new(
                            1,
                            "evidence.recall",
                            json!({ "source_entry_id": sid }),
                        ))
                        .await;
                    match (resp.result, resp.error) {
                        (Some(val), _) => {
                            let content = val
                                .get("content")
                                .and_then(Value::as_str)
                                .unwrap_or("");
                            Ok(ReplStepOutcome {
                                output: format!("{}\n", sanitize_terminal_text(content)),
                                should_exit: false,
                            })
                        }
                        (_, Some(err)) => Ok(ReplStepOutcome {
                            output: format!(
                                "evidence error: {}\n",
                                sanitize_terminal_text(&err.message)
                            ),
                            should_exit: false,
                        }),
                        _ => Err("empty response from evidence.recall".to_string()),
                    }
                }
                "/clear" => {
                    let resp = self
                        .dispatcher
                        .handle_request(JsonRpcRequest::new(1, "session.new", json!({})))
                        .await;
                    let sid = resp
                        .result
                        .and_then(|v| v.get("session_id").and_then(Value::as_str).map(String::from))
                        .unwrap_or_default();
                    Ok(ReplStepOutcome {
                        output: format!(
                            "started new session {}\n",
                            sanitize_terminal_text(&sid)
                        ),
                        should_exit: false,
                    })
                }
                other => Ok(ReplStepOutcome {
                    output: format!(
                        "unknown command '{}'; type /help for available commands.\n",
                        sanitize_terminal_text(other)
                    ),
                    should_exit: false,
                }),
            };
        }

        let resp = self
            .dispatcher
            .handle_request(JsonRpcRequest::new(
                1,
                "session.prompt",
                json!({ "prompt": line }),
            ))
            .await;
        match (resp.result, resp.error) {
            (Some(val), _) => {
                let status = val.get("status").and_then(Value::as_str).unwrap_or("unknown");
                let text = val
                    .get("final_text")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let turns = val.get("turns_used").and_then(Value::as_u64).unwrap_or(0);
                let tools = val
                    .get("tool_calls_used")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let rev = val
                    .get("latest_revision_id")
                    .and_then(Value::as_str)
                    .unwrap_or("<none>");
                let mut out = String::new();
                if !text.is_empty() {
                    out.push_str(&sanitize_terminal_text(text));
                    if !out.ends_with('\n') {
                        out.push('\n');
                    }
                }
                out.push_str(&format!(
                    "[status={} turns={} tools={} rev={}]\n",
                    sanitize_terminal_text(status),
                    turns,
                    tools,
                    sanitize_terminal_text(rev),
                ));
                Ok(ReplStepOutcome {
                    output: out,
                    should_exit: false,
                })
            }
            (_, Some(err)) => Ok(ReplStepOutcome {
                output: format!("error: {}\n", sanitize_terminal_text(&err.message)),
                should_exit: false,
            }),
            _ => Err("empty response from session.prompt".to_string()),
        }
    }

    /// Drive the interactive REPL loop over arbitrary async buffered reader and writer streams.
    pub async fn run_io<R, W>(&self, reader: R, writer: W) -> Result<(), String>
    where
        R: AsyncBufRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        self.run_io_with_initial_prompt(reader, writer, None).await
    }

    /// Drive the interactive REPL loop, optionally executing an initial `--prompt`/`--task` as Turn
    /// 1 before entering the interactive input loop.
    pub async fn run_io_with_initial_prompt<R, W>(
        &self,
        mut reader: R,
        mut writer: W,
        initial_prompt: Option<&str>,
    ) -> Result<(), String>
    where
        R: AsyncBufRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        let banner = self.render_status_banner().await;
        writer
            .write_all(banner.as_bytes())
            .await
            .map_err(|e| e.to_string())?;
        writer.flush().await.map_err(|e| e.to_string())?;

        if let Some(prompt) = initial_prompt.map(str::trim).filter(|s| !s.is_empty()) {
            let step = self.handle_line(prompt).await?;
            if !step.output.is_empty() {
                writer
                    .write_all(step.output.as_bytes())
                    .await
                    .map_err(|e| e.to_string())?;
                writer.flush().await.map_err(|e| e.to_string())?;
            }
            if step.should_exit {
                return Ok(());
            }
        }

        loop {
            writer
                .write_all(b"harness> ")
                .await
                .map_err(|e| e.to_string())?;
            writer.flush().await.map_err(|e| e.to_string())?;

            let Some(line) = read_bounded_jsonrpc_line(&mut reader, MAX_RPC_FRAME_BYTES).await?
            else {
                break;
            };
            let step = self.handle_line(&line).await?;
            if !step.output.is_empty() {
                writer
                    .write_all(step.output.as_bytes())
                    .await
                    .map_err(|e| e.to_string())?;
                writer.flush().await.map_err(|e| e.to_string())?;
            }
            if step.should_exit {
                break;
            }
        }
        Ok(())
    }
}
