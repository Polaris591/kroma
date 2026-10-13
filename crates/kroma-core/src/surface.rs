//! Gate 6 — Multi-Surface Runtime (`v1.0.0`).
//!
//! Decouples the core execution engine (`AgentRunner` + `SessionJournal` + `SnapshotStore` +
//! `ToolRegistry` + `PolicyEngine`) from terminal I/O so all four runtime surfaces share the exact
//! same state machine, policy boundaries, workspace revision history, and diagnostic event stream:
//!
//! 1. **Headless CLI** (`kroma run` / `kroma --task ...` with optional `--output-format jsonl`).
//! 2. **Interactive REPL / TUI** (`kroma repl` / `kroma tui` / `--interactive`), implemented as a
//!    stateless consumer of [`RpcDispatcher`] with zero private session state.
//! 3. **ACP stdio server** (`kroma acp` / `--acp`), speaking newline-delimited JSON-RPC 2.0 on
//!    `stdin`/`stdout` with `stdout` reserved exclusively for protocol frames.
//! 4. **Local SDK / Daemon server** (`kroma serve --listen 127.0.0.1:<PORT>`), serving JSON-RPC 2.0
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
use crate::messages::{ToolCall, TranscriptItem};
use crate::types::ToolCallId;
use crate::prompt::{sha256_hex64, PromptBuilder};
use crate::provider::ModelProvider;
use crate::revision::{RestoreResult, RevisionId, SnapshotStore, WorkspaceRevision};
use crate::session::{RevisionRecord, SessionJournal};
use crate::theme::{StatusDot, Theme, ThemeRenderer};
use crate::tool::policy::{
    ApprovalDecision, ApprovalHandler, ApprovalRequest, PolicyCheckResult, PolicyEngine, PolicyMode,
};
use crate::tool::registry::ToolRegistry;
use crate::tools::recall_evidence::execute_recall_evidence;
use crate::types::{EffectClass, RunId, RunStatus};
use crate::workspace::WorkspaceCap;
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
/// Maximum simultaneous client connections accepted by `kroma serve`.
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

/// `v1.5.0`: width for the Design D spectrum hairline, clamped to a readable range.
fn terminal_hairline_width() -> usize {
    crate::theme::terminal_width().saturating_sub(2).max(20)
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

/// JSON-RPC 2.0 server-to-client notification envelope (`kroma/event`, `approval/requested`, etc.).
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

/// Configuration for [`RpcDispatcher`] / [`KromaRuntimeServer`].
pub struct KromaRuntimeConfig {
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
    /// Item #3 (`v1.4.0`): candidates to race when the single agent stalls (`0` = off).
    pub adaptive_tournament_candidates: usize,
    pub initial_skill: Option<String>,
    pub extensions: Vec<ScopedExtension>,
    pub surface_name: String,
    pub transport_name: String,
    pub auth_token: Option<String>,
    pub approval_timeout: Duration,
    pub quiet: bool,
    /// `v1.3.0`: optional factory used by `/model` and `/protocol` to rebuild the provider client
    /// in place when the active model or wire protocol is hot-swapped.
    pub provider_factory: Option<Arc<dyn ProviderFactory>>,
    /// `v1.3.0`: merged layered configuration (`~/.kroma/config.toml` + `.kroma.toml` + env + CLI).
    pub resolved_config: Option<crate::config_file::ResolvedConfig>,
}

impl KromaRuntimeConfig {
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
            adaptive_tournament_candidates: 0,
            initial_skill: None,
            extensions: Vec::new(),
            surface_name: "acp".to_string(),
            transport_name: "stdio".to_string(),
            auth_token: None,
            approval_timeout: DEFAULT_APPROVAL_TIMEOUT,
            quiet: false,
            provider_factory: None,
            resolved_config: None,
        }
    }

    /// `v1.3.0`: attach a [`ProviderFactory`] so the active model/protocol can be hot-swapped.
    pub fn with_provider_factory(mut self, factory: Arc<dyn ProviderFactory>) -> Self {
        self.provider_factory = Some(factory);
        self
    }

    /// `v1.3.0`: attach the merged layered configuration for `/config` inspection.
    pub fn with_resolved_config(
        mut self,
        resolved: crate::config_file::ResolvedConfig,
    ) -> Self {
        self.resolved_config = Some(resolved);
        self
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

/// `v1.3.0`: accepted thinking levels for `/thinking`.
pub const THINKING_LEVELS: &[&str] = &["off", "none", "low", "medium", "high", "xhigh"];

/// Normalize a user-supplied thinking level. `off`/`none`/`clear` disable reasoning.
pub fn normalize_reasoning_effort(raw: &str) -> Option<String> {
    let value = raw.trim().to_ascii_lowercase();
    match value.as_str() {
        "" | "off" | "none" | "clear" | "disable" | "disabled" => None,
        other => Some(other.to_string()),
    }
}

/// True when `value` looks like a thinking level rather than a model id.
pub fn looks_like_thinking_level(value: &str) -> bool {
    THINKING_LEVELS.contains(&value.trim().to_ascii_lowercase().as_str())
}

type SubscriberCallback = Arc<dyn Fn(&JsonRpcNotification) -> bool + Send + Sync>;

/// `v1.3.0`: the effective model and protocol after a hot-swap, as resolved by the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderSwap {
    /// Concrete wire protocol actually in use (`auto` is resolved by the host).
    pub protocol: String,
    /// Effective model id (the host may substitute its configured default when `model` is empty).
    pub model: String,
}

/// `v1.3.0`: rebuilds the provider client for a hot-swapped model or wire protocol.
///
/// The kernel owns the swap mechanics (locking, cancellation, notification); the host supplies the
/// credentials and endpoint, so `kroma-core` never has to know how the operator configured them.
pub trait ProviderFactory: Send + Sync {
    /// Build a provider for `model` using wire protocol `protocol`
    /// (`auto` | `chat-completions` | `anthropic-messages` | `openai-responses`) and reasoning
    /// effort `reasoning_effort` (`None` clears it).
    ///
    /// An empty `model` means "keep the host's configured default", which lets `/thinking` and
    /// `/protocol` work even before any model has been named.
    fn build(
        &self,
        model: &str,
        protocol: &str,
        reasoning_effort: Option<&str>,
    ) -> Result<(ProviderSwap, Arc<dyn ModelProvider>), String>;
}

struct RuntimeEngineInner {
    config: KromaRuntimeConfig,
    provider: Mutex<Arc<dyn ModelProvider>>,
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
    /// `v1.3.0`: active model id after any `/model` hot-swap.
    active_model: Mutex<Option<String>>,
    /// `v1.3.0`: active wire protocol after any `/protocol` hot-swap.
    active_protocol: Mutex<Option<String>>,
    /// `v1.3.0`: active reasoning/thinking effort after any `/thinking` hot-swap (`None` = off).
    active_reasoning_effort: Mutex<Option<String>>,
    /// B2 (`v1.4.0`): active context projection profile after `/config set projection_profile`.
    active_projection_profile: Mutex<Option<crate::context_projector::ProjectionProfile>>,
    /// Item #3 (`v1.4.0`): active adaptive-tournament candidate count (`0` = off).
    active_adaptive_tournament: Mutex<usize>,
    /// `v1.3.0`: layered configuration view, refreshed by `/config set`.
    resolved_config: Mutex<Option<crate::config_file::ResolvedConfig>>,
    /// `v1.5.0`: active terminal theme (`/theme zen|classic`); Design D (`Zen`) is the default.
    active_theme: Mutex<Theme>,
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

/// Alias matching the task specification (`KromaRuntimeServer` / `RpcDispatcher`).
pub type KromaRuntimeServer = RpcDispatcher;

impl RpcDispatcher {
    pub fn new(
        mut config: KromaRuntimeConfig,
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

        let initial_model = config
            .resolved_config
            .as_ref()
            .and_then(|c| c.model.clone());
        let initial_protocol = config
            .resolved_config
            .as_ref()
            .and_then(|c| c.protocol.clone());
        let initial_reasoning = config
            .resolved_config
            .as_ref()
            .and_then(|c| c.reasoning_effort.clone());
        // B2 (`v1.4.0`): resolve the configured context projection profile (invalid values are
        // rejected at config-parse time, so this only ever sees a valid profile).
        let initial_projection_profile = config
            .resolved_config
            .as_ref()
            .and_then(|c| c.projection_profile.as_deref())
            .and_then(|v| crate::context_projector::ProjectionProfile::parse(v).ok());
        // Item #3 (`v1.4.0`): adaptive tournament escalation count (config default, overridable by
        // `/config set adaptive_tournament <N>`).
        let initial_adaptive_tournament = config.adaptive_tournament_candidates;
        let initial_resolved = config.resolved_config.clone();
        // `v1.5.0`: Design D (`Zen`) is the default terminal theme; `KROMA_UI_THEME` overrides it.
        let initial_theme = Theme::from_env();
        let inner = Arc::new(RuntimeEngineInner {
            config,
            provider: Mutex::new(provider),
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
            active_model: Mutex::new(initial_model),
            active_protocol: Mutex::new(initial_protocol),
            active_reasoning_effort: Mutex::new(initial_reasoning),
            active_projection_profile: Mutex::new(initial_projection_profile),
            active_adaptive_tournament: Mutex::new(initial_adaptive_tournament),
            resolved_config: Mutex::new(initial_resolved),
            active_theme: Mutex::new(initial_theme),
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

    /// Subscribe to real-time JSON-RPC notifications (`kroma/event`, `approval/requested`,
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
        // P1 (`v1.5.0`): never let a journal read lower the live spawn count. Spawns reserved by an
        // RPC `session.tournament` are counted in memory; a checkpoint written before the session's
        // first transcript entry has no leaf and is invisible to `snapshot_from_journal`, which
        // would otherwise reset the count to 0 and re-open the `max_subagents` bypass.
        next.subagents_spawned = next.subagents_spawned.max(guard.subagents_spawned);
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
            "workspace.graph",
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
            // Gate 7 (v1.2.0): symbol dependency graph + Personalized PageRank navigation.
            "workspace.graph" | "workspace/graph" => self.rpc_workspace_graph(&req.params).await,
            // v1.3.0: layered configuration + live model/protocol hot-swapping.
            "config.get" | "config/get" => self.rpc_config_get(&req.params).await,
            "config.set" | "config/set" | "session.configure" | "session/configure" => {
                self.rpc_config_set(&req.params).await
            }
            // v1.3.0: in-REPL session listing and switching.
            "sessions.list" | "sessions/list" | "session.list" => {
                self.rpc_sessions_list().await
            }
            "session.resume" | "session/resume" => self.rpc_session_resume(&req.params).await,
            // v1.3.0: Best-of-N worktree tournament.
            "session.tournament" | "session/tournament" => {
                self.rpc_session_tournament(&req.params).await
            }
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
                "graph": true,
            },
            "supported_methods": Self::supported_methods(),
            "tools": visible_tools,
            "skills": skills,
        }))
    }

    /// Item #3 (`v1.4.0`): active adaptive-tournament candidate count (`0` = off).
    fn adaptive_tournament_candidates(&self) -> usize {
        *self
            .inner
            .active_adaptive_tournament
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// B2 (`v1.4.0`): active context projection profile (`None` = default `P2`).
    fn active_projection_profile(&self) -> Option<crate::context_projector::ProjectionProfile> {
        *self
            .inner
            .active_projection_profile
            .lock()
            .unwrap_or_else(|e| e.into_inner())
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
        let _ = std::fs::create_dir_all(&sessions_dir);
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
            if crate::workspace::path_starts_with_and_strip(&canon_target, &canon_sessions)
                .is_none()
            {
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
            if crate::workspace::strip_windows_verbatim_prefix(j.workspace_root())
                != crate::workspace::strip_windows_verbatim_prefix(
                    self.inner.env.workspace().root(),
                )
            {
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
        // B2 (`v1.4.0`): apply the configured context projection profile.
        run_cfg.projection_profile = self.active_projection_profile();

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
                JsonRpcNotification::new("kroma/event", payload),
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
            Arc::clone(&self.inner.provider.lock().unwrap_or_else(|e| e.into_inner())),
            runtime,
            cancel_rx,
        )
        .with_prompt_builder(prompt_builder)
        .with_session_journal(journal)
        .with_snapshot_store(Arc::clone(&self.inner.snapshot_store))
        .with_subtree_context(subtree_tracker)
        .with_completion_gate(Arc::clone(&self.inner.config.completion_gate))
        .with_completion_gate_max_retries(self.inner.config.completion_gate_max_retries)
        .with_max_subagents(self.inner.config.max_subagents)
        // P1 (`v1.5.0`): carry the session's live spawn count into the run so tournaments run
        // through the RPC surface count against the same `max_subagents` quota as tool spawns.
        .with_subagents_spawned(
            self.inner
                .status
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .subagents_spawned,
        )
        .with_adaptive_tournament_candidates(self.adaptive_tournament_candidates());

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
            "children_input_tokens": summary.children_input_tokens,
            "children_output_tokens": summary.children_output_tokens,
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
                            JsonRpcNotification::new("kroma/event", payload),
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

    /// P1 (`v1.5.0`): carry the cumulative `subagents_spawned` count (and the merged revision, when
    /// one was produced) forward in a checkpoint on the active branch.
    ///
    /// Existing checkpoint state is preserved so `--resume` / `--fork` never lose the pinned
    /// baseline, the subtree directories, or the mutation ledger. A session that has never written a
    /// checkpoint gets a minimal one, which is harmless because there is no earlier state to keep.
    fn persist_spawn_count(
        journal: &mut SessionJournal,
        spawned: usize,
        merged_revision: Option<&RevisionId>,
    ) -> Result<(), String> {
        let (prev_turn, pinned, last_touched, reasoning_sha, prev_rev, subtree, mutations) =
            match journal.latest_active_checkpoint() {
                Some(cp) => (
                    cp.turn,
                    cp.pinned_baseline_end,
                    cp.last_touched.clone(),
                    cp.reasoning_blocks_sha256.clone(),
                    cp.workspace_revision_id.clone(),
                    cp.subtree_dirs.clone(),
                    cp.mutations.clone(),
                ),
                None => (
                    0,
                    0,
                    Vec::new(),
                    String::new(),
                    None,
                    Vec::new(),
                    Vec::new(),
                ),
            };
        let revision = merged_revision
            .map(|r| r.as_str().to_string())
            .or(prev_rev);
        let turn = journal.max_active_turn().max(prev_turn);
        journal
            .checkpoint_with_subagents(
                turn,
                pinned,
                last_touched,
                reasoning_sha,
                revision,
                subtree,
                mutations,
                spawned,
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

    /// Gate 7: symbol dependency graph + Personalized PageRank navigation for non-terminal surfaces.
    async fn rpc_workspace_graph(&self, params: &Value) -> Result<Value, JsonRpcError> {
        let max_nodes = params
            .get("max_nodes")
            .and_then(Value::as_u64)
            .map(|v| v as usize)
            .unwrap_or(crate::repo_graph::DEFAULT_MAX_NODES)
            .clamp(1, crate::repo_graph::MAX_MAX_NODES);
        let query = params
            .get("query")
            .and_then(Value::as_str)
            .map(str::to_string);
        let query_terms: Vec<String> = query
            .as_deref()
            .unwrap_or("")
            .split(|c: char| !(c.is_alphanumeric() || c == '_' || c == ':' || c == '.'))
            .map(str::trim)
            .filter(|t| t.len() >= 2)
            .map(str::to_string)
            .take(16)
            .collect();
        let mut focus_files: Vec<String> = Vec::new();
        if let Some(list) = params.get("focus_files").and_then(Value::as_array) {
            if list.len() > 32 {
                return Err(JsonRpcError::new(
                    JSONRPC_INVALID_PARAMS,
                    "focus_files accepts at most 32 entries",
                ));
            }
            for entry in list {
                let Some(raw) = entry.as_str() else {
                    continue;
                };
                let normalized = raw.trim().trim_start_matches("./").replace('\\', "/");
                if normalized.is_empty() {
                    continue;
                }
                WorkspaceCap::validate_relative_components(&normalized).map_err(|e| {
                    JsonRpcError::new(
                        JSONRPC_INVALID_PARAMS,
                        format!("invalid focus_files entry '{}': {}", raw, e),
                    )
                })?;
                focus_files.push(normalized);
            }
        }

        let graph = crate::repo_graph::RepoGraph::build_from_workspace(
            self.inner.env.workspace(),
            crate::repo_graph::MAX_GRAPH_FILES,
        );
        let report = graph.query(&query_terms, &focus_files, max_nodes);
        let ranked: Vec<Value> = report
            .ranked
            .iter()
            .map(|node| {
                json!({
                    "path": node.path,
                    "score": node.score,
                    "symbols": node
                        .symbols
                        .iter()
                        .map(|s| json!({
                            "name": s.name,
                            "kind": s.kind.as_str(),
                            "line": s.line,
                            "signature": s.signature,
                        }))
                        .collect::<Vec<Value>>(),
                    "called_by": node.called_by,
                    "calls_into": node.calls_into,
                })
            })
            .collect();

        Ok(json!({
            "query_terms": report.query_terms,
            "focus_files": report.focus_files,
            "total_files": report.total_files,
            "total_symbols": report.total_symbols,
            "total_edges": report.total_edges,
            "truncated": report.truncated,
            "skipped_files": graph.skipped_files,
            "ranked": ranked,
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

    // -----------------------------------------------------------------------------------------
    // v1.3.0 — persistent configuration and live model/protocol hot-swapping
    // -----------------------------------------------------------------------------------------

    /// Current active model id (after any `/model` hot-swap), or `"<unset>"`.
    pub fn active_model(&self) -> Option<String> {
        self.inner
            .active_model
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Current active wire protocol name (after any `/protocol` hot-swap).
    pub fn active_protocol(&self) -> Option<String> {
        self.inner
            .active_protocol
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Current active reasoning/thinking effort (`None` means reasoning is off).
    pub fn active_reasoning_effort(&self) -> Option<String> {
        self.inner
            .active_reasoning_effort
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// `v1.5.0`: current terminal theme (Design D `Zen` by default).
    pub fn active_theme(&self) -> Theme {
        *self.inner.active_theme.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// `v1.5.0`: build a [`ThemeRenderer`] for the active theme, auto-detecting colour support.
    pub fn theme_renderer(&self) -> ThemeRenderer {
        ThemeRenderer::detect(self.active_theme())
    }

    /// F01 (`v1.4.0`): shared authorization + quota preflight for `session.tournament`.
    ///
    /// Mirrors the `spawn_subagent` tool path exactly: `max_subagents` quota, `PolicyEngine`
    /// evaluation with `EffectClass::Mutating`, the registered fail-closed `PolicyHook` gate, and an
    /// interactive approval round-trip for `Ask`. Before this fix the RPC path skipped all of these,
    /// so a `ReadOnly` session could write (and merge) files through `/race`.
    async fn tournament_preflight(&self, candidate_count: usize) -> Result<(), JsonRpcError> {
        // Quota + policy mode (shared helper, same as the tool path). P1 (`v1.5.0`): the live
        // session spawn count is passed in so repeated `/race` calls cannot bypass `max_subagents`.
        let already_spawned = self
            .inner
            .status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .subagents_spawned;
        let check = match crate::subagents::subagent_spawn_preflight(
            &self.inner.policy_engine,
            candidate_count,
            already_spawned,
            self.inner.config.max_subagents,
            EffectClass::Mutating,
        ) {
            Ok(check) => check,
            Err(reason) => {
                return Err(JsonRpcError::new(JSONRPC_INTERNAL_ERROR, reason));
            }
        };

        // `PolicyHook` gate: a registered fail-closed hook can veto the tournament before any
        // worktree is materialized.
        let runtime = build_runtime_with_registry(
            self.inner.registry.clone(),
            Arc::clone(&self.inner.env) as Arc<dyn ExecutionEnv>,
            self.inner.policy_engine.clone(),
            self.inner.config.budget.clone(),
        );
        let hook_call = ToolCall {
            call_id: ToolCallId::new("rpc-tournament"),
            name: "spawn_subagent".to_string(),
            raw_arguments: "{\"role\":\"worker\"}".to_string(),
        };
        if let Err((denied, _)) = runtime.pre_tool_hook_gate(&hook_call, 0).await {
            return Err(JsonRpcError::new(
                JSONRPC_INTERNAL_ERROR,
                format!(
                    "policy hook denied session.tournament: {}",
                    denied.content
                ),
            ));
        }

        match check {
            PolicyCheckResult::Allow => Ok(()),
            PolicyCheckResult::Deny(reason) => Err(JsonRpcError::new(
                JSONRPC_INTERNAL_ERROR,
                format!("{} (session.tournament requires write access)", reason),
            )),
            PolicyCheckResult::Ask => {
                let mut cancel_rx = self.ensure_active_cancel_receiver();
                if *cancel_rx.borrow() {
                    return Err(JsonRpcError::new(
                        JSONRPC_INTERNAL_ERROR,
                        "session.tournament cancelled before approval prompt",
                    ));
                }
                let req = ApprovalRequest {
                    tool_name: "spawn_subagent".to_string(),
                    effect_class: EffectClass::Mutating,
                    is_sensitive_path: false,
                    target_path: None,
                    diff_preview: None,
                    command_preview: None,
                    workspace_root: self.inner.env.workspace().root().to_path_buf(),
                };
                let engine = self.inner.policy_engine.clone();
                let mut prompt_task =
                    tokio::task::spawn_blocking(move || engine.prompt_approval(&req));
                let decision = tokio::select! {
                    res = &mut prompt_task => res.unwrap_or(ApprovalDecision::Deny),
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
                        let _ = tokio::time::timeout(
                            std::time::Duration::from_millis(200),
                            &mut prompt_task,
                        )
                        .await;
                        return Err(JsonRpcError::new(
                            JSONRPC_INTERNAL_ERROR,
                            "session.tournament approval cancelled",
                        ));
                    }
                };
                match decision {
                    ApprovalDecision::AllowOnce | ApprovalDecision::AllowCategoryForRun => Ok(()),
                    ApprovalDecision::Deny => Err(JsonRpcError::new(
                        JSONRPC_INTERNAL_ERROR,
                        "session.tournament denied by approval policy",
                    )),
                }
            }
        }
    }

    /// `session.tournament`: run a Best-of-N worktree tournament and merge only the winner.
    ///
    /// Each candidate runs in its own isolated worktree, is verified inside that worktree, and is
    /// ranked deterministically. The parent workspace is only touched when a candidate passed
    /// verification, and the winning merge is recorded as a single WAL revision so `/undo` reverts
    /// the whole tournament in one step.
    async fn rpc_session_tournament(&self, params: &Value) -> Result<Value, JsonRpcError> {
        if self.inner.in_flight.load(Ordering::SeqCst) {
            return Err(JsonRpcError::new(
                JSONRPC_INTERNAL_ERROR,
                "cannot run a tournament while a prompt is in flight; cancel or await completion first",
            ));
        }
        let Ok(_guard) = self.inner.op_lock.try_lock() else {
            return Err(JsonRpcError::new(
                JSONRPC_INTERNAL_ERROR,
                "cannot run a tournament while another session operation is in progress",
            ));
        };

        let task = params
            .get("task")
            .or_else(|| params.get("prompt"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                JsonRpcError::new(
                    JSONRPC_INVALID_PARAMS,
                    "missing non-empty 'task' (or 'prompt') string parameter",
                )
            })?
            .to_string();

        let count = params
            .get("candidates")
            .and_then(Value::as_u64)
            .unwrap_or(3) as usize;
        if !(crate::subagents::MIN_TOURNAMENT_CANDIDATES
            ..=crate::subagents::MAX_TOURNAMENT_CANDIDATES)
            .contains(&count)
        {
            return Err(JsonRpcError::new(
                JSONRPC_INVALID_PARAMS,
                format!(
                    "'candidates' must be between {} and {}",
                    crate::subagents::MIN_TOURNAMENT_CANDIDATES,
                    crate::subagents::MAX_TOURNAMENT_CANDIDATES
                ),
            ));
        }

        // F01 (`v1.4.0`): enforce the same authorization + quota as the `spawn_subagent` tool path
        // BEFORE any candidate worktree is materialized.
        self.tournament_preflight(count).await?;

        // P1 (`v1.5.0`): reserve the candidate budget in the live session counter immediately
        // after preflight (matching `execute_spawn_tournament`) so a second `/race` on the same
        // session is measured against what this one is about to spend.
        {
            let mut status = self
                .inner
                .status
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            status.subagents_spawned = status.subagents_spawned.saturating_add(count);
        }

        let strategies: Vec<String> = params
            .get("strategies")
            .and_then(Value::as_array)
            .map(|list| {
                list.iter()
                    .filter_map(Value::as_str)
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        let allowed_write_globs: Option<Vec<String>> = params
            .get("allowed_paths")
            .or_else(|| params.get("allowed_write_globs"))
            .and_then(Value::as_array)
            .map(|list| {
                list.iter()
                    .filter_map(Value::as_str)
                    .map(|s| s.to_string())
                    .collect()
            });
        let max_turns = params
            .get("max_turns")
            .and_then(Value::as_u64)
            .map(|n| n as usize);
        let timeout_ms = params
            .get("timeout_ms")
            .and_then(Value::as_u64)
            .unwrap_or(crate::subagents::DEFAULT_SUBAGENT_TIMEOUT_MS);
        // Item #3 (`v1.4.0`): protected test/verification paths.
        let disallow_test_tampering = params
            .get("disallow_test_tampering")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let protected_paths: Vec<String> = params
            .get("protected_paths")
            .and_then(Value::as_array)
            .map(|list| {
                list.iter()
                    .filter_map(Value::as_str)
                    .map(|s| s.to_string())
                    .collect()
            })
            .unwrap_or_default();

        // Candidate strategy hints: distinct by default so the candidates actually diverge.
        let default_strategies = [
            "Fix the root cause with the smallest possible change.",
            "Consider an alternative design and pick the most robust minimal fix.",
            "Re-derive the failure from first principles before editing.",
            "Prefer a defensive fix that also adds a regression guard.",
        ];

        let mut candidates: Vec<crate::subagents::TournamentCandidate> = Vec::with_capacity(count);
        for index in 0..count {
            let label = format!("c{}", index + 1);
            let strategy = strategies
                .get(index)
                .cloned()
                .unwrap_or_else(|| default_strategies[index % default_strategies.len()].to_string());
            let mut spec = crate::subagents::SubagentSpec::worker(format!(
                "{}\n\n[TOURNAMENT CANDIDATE {} STRATEGY] {}",
                task,
                label,
                strategy
            ));
            if let Some(globs) = &allowed_write_globs {
                spec = spec.with_allowed_write_globs(globs.clone());
            }
            if let Some(turns) = max_turns {
                spec = spec.with_max_turns(turns);
            }
            spec = spec.with_timeout_ms(timeout_ms);
            candidates.push(
                crate::subagents::TournamentCandidate {
                    label,
                    strategy: Some(strategy),
                    spec,
                },
            );
        }

        let workspace = self.inner.env.workspace().clone();
        let store = Arc::clone(&self.inner.snapshot_store);
        let provider = Arc::clone(&self.inner.provider.lock().unwrap_or_else(|e| e.into_inner()));
        let backend: Arc<dyn crate::subagents::SubagentBackend> =
            Arc::new(crate::subagents::ProviderSubagentBackend::new(provider));

        let parent_revision_id: Option<RevisionId> = self
            .inner
            .status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .latest_revision_id
            .clone()
            .map(RevisionId::new);
        let turn = self
            .inner
            .status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .turn_count;

        // Capture a fresh baseline of the live parent workspace so every candidate forks from the
        // exact bytes on disk right now (not from a possibly stale earlier revision).
        let baseline = store
            .capture_revision(&workspace, parent_revision_id.as_ref(), None, turn)
            .map_err(|e| {
                JsonRpcError::new(
                    JSONRPC_INTERNAL_ERROR,
                    format!("failed to capture tournament baseline revision: {}", e),
                )
            })?;
        let base_revision = baseline.revision_id.clone();

        let spec = crate::subagents::TournamentSpec {
            candidates,
            worktree_base: self.inner.config.state_dir.join("worktrees"),
            state_base: self.inner.config.state_dir.join("subagents"),
            allowed_write_globs,
            budget: self.inner.config.budget.clone(),
            policy_mode: self.inner.policy_engine.mode(),
            workspace_skills: self.inner.registry.skills(),
            predictive_check: self.inner.config.predictive_check,
            cancel_rx: self.ensure_active_cancel_receiver(),
            timeout_ms,
            disallow_test_tampering,
            protected_paths: protected_paths.clone(),
        };

        let outcome = crate::subagents::run_worktree_tournament(
            spec,
            &workspace,
            &store,
            backend,
            Some(&base_revision),
            &self.inner.config.completion_gate,
        )
        .await
        .map_err(|e| JsonRpcError::new(JSONRPC_INTERNAL_ERROR, e))?;

        // P1 (`v1.5.0`): persist the tournament outcome in the session WAL. The winning merge is
        // recorded as one `RevisionCaptured` record so `/undo` reverts it atomically (F04), and the
        // cumulative `subagents_spawned` count is carried forward in a checkpoint so later prompt
        // runs and `--resume` / `--fork` honour `max_subagents`.
        //
        // F04 (`v1.4.0`): a WAL failure is an explicit RPC error, never a silent success — when a
        // merge happened the revision must be durably recorded. When nothing merged the in-memory
        // count still enforces the quota, so a WAL failure is tolerated.
        if let Ok(mut journal) = SessionJournal::open(&self.session_path()) {
            if let Some(rev) = outcome.merged_revision.clone() {
                match store.load_revision(&rev) {
                    Ok(manifest) => {
                        if let Err(e) = journal.record_revision_capture(
                            journal.max_active_turn(),
                            rev.as_str(),
                            Some(base_revision.as_str()),
                            manifest.changed_paths.clone(),
                            manifest.tracked_files.len(),
                        ) {
                            return Err(JsonRpcError::new(
                                JSONRPC_INTERNAL_ERROR,
                                format!(
                                    "tournament merged revision {} but appending it to the session WAL failed ({}); the merge is on disk but unrecorded",
                                    rev.as_str(),
                                    e
                                ),
                            ));
                        }
                    }
                    Err(e) => {
                        return Err(JsonRpcError::new(
                            JSONRPC_INTERNAL_ERROR,
                            format!(
                                "tournament merged revision {} but its manifest could not be loaded ({}); the merge is on disk but unrecorded",
                                rev.as_str(),
                                e
                            ),
                        ));
                    }
                }
            }
            let spawned = self
                .inner
                .status
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .subagents_spawned;
            if let Err(e) =
                Self::persist_spawn_count(&mut journal, spawned, outcome.merged_revision.as_ref())
            {
                if let Some(rev) = &outcome.merged_revision {
                    return Err(JsonRpcError::new(
                        JSONRPC_INTERNAL_ERROR,
                        format!(
                            "tournament merged revision {} but its spawn count could not be recorded ({}); the merge is on disk but unrecorded",
                            rev.as_str(),
                            e
                        ),
                    ));
                }
            }
            self.update_snapshot_from_journal(&journal);
        } else if let Some(rev) = &outcome.merged_revision {
            return Err(JsonRpcError::new(
                JSONRPC_INTERNAL_ERROR,
                format!(
                    "tournament merged revision {} but the session WAL could not be opened; the merge is on disk but unrecorded",
                    rev.as_str()
                ),
            ));
        }

        let note = JsonRpcNotification::new(
            "kroma/event",
            json!({
                "schema_version": crate::events::EVENT_SCHEMA_VERSION,
                "event": "tournament_completed",
                "candidates": outcome.scores.len(),
                "merged_revision": outcome.merged_revision.as_ref().map(|r| r.as_str().to_string()),
                "winner": outcome.winner.map(|i| outcome.scores[i].label.clone()),
            }),
        );
        Self::broadcast_to_subscribers(&self.inner, note);

        Ok(json!({
            "session_id": self.session_id(),
            "candidates": outcome.scores.iter().map(|s| json!({
                "label": s.label,
                "status": s.status,
                "gate_passed": s.gate_passed,
                "gate_detail": s.gate_detail,
                "predictive_warnings": s.predictive_warnings,
                "conflict_free_with_parent": s.conflict_free_with_parent,
                "files_modified": s.files_modified,
                "total_changed_lines": s.total_changed_lines,
                "turns_used": s.turns_used,
                "winner": s.winner,
            })).collect::<Vec<Value>>(),
            "winner": outcome.winner.map(|i| outcome.scores[i].label.clone()),
            "merged_revision": outcome.merged_revision.as_ref().map(|r| r.as_str().to_string()),
            "parent_untouched": outcome.parent_untouched(),
            "all_worktrees_cleaned_up": outcome.all_worktrees_cleaned_up,
            "report": outcome.render_report(),
        }))
    }

    /// `config.get`: redacted layered configuration plus the active model/protocol.
    async fn rpc_config_get(&self, _params: &Value) -> Result<Value, JsonRpcError> {
        let resolved = self
            .inner
            .resolved_config
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let active_model = self.active_model();
        let active_protocol = self.active_protocol();
        let hot_swap_available = self.inner.config.provider_factory.is_some();

        let (summary, rejected, user_path, workspace_path) = match &resolved {
            Some(cfg) => (
                cfg.redacted_summary(),
                cfg.rejected_workspace_keys.clone(),
                cfg.user_config_path
                    .as_ref()
                    .map(|p| p.display().to_string()),
                cfg.workspace_config_path
                    .as_ref()
                    .map(|p| p.display().to_string()),
            ),
            // Surfaces started without layered configuration still report the live state so
            // `/config show` is never blank.
            None => (
                format!(
                    "profile:          <none>\nmodel:            {}\nprotocol:         {}\nreasoning_effort: {}\nnote:             no ~/.kroma/config.toml or .kroma.toml was loaded\n",
                    active_model.clone().unwrap_or_else(|| "<unset>".to_string()),
                    active_protocol.clone().unwrap_or_else(|| "<unset>".to_string()),
                    self.active_reasoning_effort()
                        .unwrap_or_else(|| "off".to_string()),
                ),
                Vec::new(),
                None,
                None,
            ),
        };

        Ok(json!({
            "active_model": active_model,
            "active_protocol": active_protocol,
            "ui_theme": self.active_theme().as_str(),
            "hot_swap_available": hot_swap_available,
            "summary": summary,
            "rejected_workspace_keys": rejected,
            "workspace_guard_triggered": !rejected.is_empty(),
            "user_config_path": user_path,
            "workspace_config_path": workspace_path,
            "api_key": if resolved.as_ref().and_then(|c| c.api_key.as_ref()).is_some() {
                crate::config_file::REDACTED_API_KEY
            } else {
                "<unset>"
            },
        }))
    }

    /// `config.set` / `session.configure`: hot-swap the active model, wire protocol and/or profile
    /// without restarting the session, optionally persisting the change to `~/.kroma/config.toml`.
    async fn rpc_config_set(&self, params: &Value) -> Result<Value, JsonRpcError> {
        let model = params
            .get("model")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let protocol = params
            .get("protocol")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let profile = params
            .get("profile")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        // `reasoning_effort` may be an explicit level, or `off`/`none`/`clear` to disable it.
        let reasoning_raw = params
            .get("reasoning_effort")
            .or_else(|| params.get("thinking"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        // B2 (`v1.4.0`): `projection_profile` is a safe, hot-swappable key that does not require a
        // provider rebuild.
        let projection_profile_raw = params
            .get("projection_profile")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        // Item #3 (`v1.4.0`): `adaptive_tournament` is also a safe, hot-swappable key.
        // P2 (`v1.5.0`): `/config set adaptive_tournament 3` arrives as a JSON string (the REPL
        // forwards raw text), so accept both a number and a numeric string.
        let adaptive_tournament_raw = params
            .get("adaptive_tournament")
            .and_then(|v| {
                v.as_u64().or_else(|| {
                    v.as_str()
                        .map(str::trim)
                        .and_then(|s| s.parse::<u64>().ok())
                })
            })
            .map(|n| n as usize);
        // `v1.5.0`: `ui_theme` is a safe, hot-swappable presentation key (`zen` | `classic`).
        let ui_theme_raw = params
            .get("ui_theme")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let save = params.get("save").and_then(Value::as_bool).unwrap_or(false);

        if model.is_none()
            && protocol.is_none()
            && profile.is_none()
            && reasoning_raw.is_none()
            && projection_profile_raw.is_none()
            && adaptive_tournament_raw.is_none()
            && ui_theme_raw.is_none()
        {
            return Err(JsonRpcError::new(
                JSONRPC_INVALID_PARAMS,
                "config.set requires at least one of 'model', 'protocol', 'reasoning_effort', 'profile', 'projection_profile', 'adaptive_tournament' or 'ui_theme'"
                    .to_string(),
            ));
        }

        let new_theme = match &ui_theme_raw {
            Some(raw) => Some(
                Theme::parse(raw).map_err(|e| JsonRpcError::new(JSONRPC_INVALID_PARAMS, e))?,
            ),
            None => None,
        };

        let new_projection_profile = match &projection_profile_raw {
            Some(raw) => Some(
                crate::context_projector::ProjectionProfile::parse(raw)
                    .map_err(|e| JsonRpcError::new(JSONRPC_INVALID_PARAMS, e))?,
            ),
            None => self.active_projection_profile(),
        };

        let mut new_model = self.active_model();
        let mut new_protocol = self.active_protocol();
        let mut new_reasoning = self.active_reasoning_effort();

        if let Some(name) = &profile {
            let user = crate::config_file::KromaConfigFile::load_user_global()
                .map_err(|e| JsonRpcError::new(JSONRPC_INTERNAL_ERROR, e))?;
            let entry = user.profiles.get(name).ok_or_else(|| {
                let known: Vec<&str> = user.profiles.keys().map(String::as_str).collect();
                JsonRpcError::new(
                    JSONRPC_INVALID_PARAMS,
                    format!(
                        "unknown profile '{}'; available: {}",
                        name,
                        if known.is_empty() {
                            "<none>".to_string()
                        } else {
                            known.join(", ")
                        }
                    ),
                )
            })?;
            if let Some(m) = &entry.model {
                new_model = Some(m.clone());
            }
            if let Some(p) = &entry.protocol {
                new_protocol = Some(p.clone());
            }
            if let Some(r) = &entry.reasoning_effort {
                new_reasoning = Some(r.clone());
            }
        }
        if let Some(m) = &model {
            new_model = Some(m.clone());
        }
        if let Some(p) = &protocol {
            new_protocol = Some(p.clone());
        }
        if let Some(raw) = &reasoning_raw {
            new_reasoning = normalize_reasoning_effort(raw);
        }

        let needs_provider_rebuild = model.is_some()
            || protocol.is_some()
            || profile.is_some()
            || reasoning_raw.is_some();

        if !needs_provider_rebuild {
            // Only safe hot-swappable keys changed: apply them in place without touching the
            // provider.
            if projection_profile_raw.is_some() {
                *self
                    .inner
                    .active_projection_profile
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = new_projection_profile;
            }
            if let Some(n) = adaptive_tournament_raw {
                *self
                    .inner
                    .active_adaptive_tournament
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = n;
            }
            if let Some(theme) = new_theme {
                *self
                    .inner
                    .active_theme
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = theme;
            }
            {
                let mut guard = self
                    .inner
                    .resolved_config
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                if let Some(cfg) = guard.as_mut() {
                    if projection_profile_raw.is_some() {
                        cfg.projection_profile = projection_profile_raw.clone();
                    }
                    if let Some(n) = adaptive_tournament_raw {
                        cfg.adaptive_tournament = Some(n);
                    }
                }
            }
            let mut persisted_to = None;
            if save {
                let mut file = crate::config_file::KromaConfigFile::load_user_global()
                    .map_err(|e| JsonRpcError::new(JSONRPC_INTERNAL_ERROR, e))?;
                if projection_profile_raw.is_some() {
                    file.projection_profile = projection_profile_raw.clone();
                }
                if let Some(n) = adaptive_tournament_raw {
                    file.adaptive_tournament = Some(n);
                }
                let path = file
                    .save_user_global()
                    .map_err(|e| JsonRpcError::new(JSONRPC_INTERNAL_ERROR, e))?;
                persisted_to = Some(path.display().to_string());
            }
            return Ok(json!({
                "projection_profile": new_projection_profile.map(|p| p.as_str()),
                "adaptive_tournament": adaptive_tournament_raw,
                "ui_theme": self.active_theme().as_str(),
                "persisted_to": persisted_to,
            }));
        }

        let factory = self.inner.config.provider_factory.as_ref().ok_or_else(|| {
            JsonRpcError::new(
                JSONRPC_INTERNAL_ERROR,
                "this surface was started without a provider factory; restart with --model/--protocol"
                    .to_string(),
            )
        })?;
        let requested_model = new_model.clone().unwrap_or_default();
        let effective_protocol = new_protocol.clone().unwrap_or_else(|| "auto".to_string());

        let (swap, provider) = factory
            .build(
                &requested_model,
                &effective_protocol,
                new_reasoning.as_deref(),
            )
            .map_err(|e| JsonRpcError::new(JSONRPC_INVALID_PARAMS, e))?;
        let resolved_protocol = swap.protocol.clone();
        let effective_model = swap.model.clone();
        if effective_model.trim().is_empty() {
            return Err(JsonRpcError::new(
                JSONRPC_INVALID_PARAMS,
                "no model is configured; pass 'model' or set model in ~/.kroma/config.toml"
                    .to_string(),
            ));
        }

        {
            let mut guard = self
                .inner
                .provider
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            *guard = provider;
        }
        *self
            .inner
            .active_model
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(effective_model.clone());
        *self
            .inner
            .active_protocol
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(resolved_protocol.clone());
        *self
            .inner
            .active_reasoning_effort
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = new_reasoning.clone();
        *self
            .inner
            .active_projection_profile
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = new_projection_profile;
        if let Some(n) = adaptive_tournament_raw {
            *self
                .inner
                .active_adaptive_tournament
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = n;
        }
        if let Some(theme) = new_theme {
            *self
                .inner
                .active_theme
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = theme;
        }
        {
            let mut guard = self
                .inner
                .resolved_config
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if let Some(cfg) = guard.as_mut() {
                cfg.model = Some(effective_model.clone());
                cfg.protocol = Some(resolved_protocol.clone());
                cfg.reasoning_effort = new_reasoning.clone();
                if let Some(name) = &profile {
                    cfg.active_profile = Some(name.clone());
                }
            }
        }

        let mut persisted_to = None;
        if save {
            let mut file = crate::config_file::KromaConfigFile::load_user_global()
                .map_err(|e| JsonRpcError::new(JSONRPC_INTERNAL_ERROR, e))?;
            if model.is_some() || profile.is_some() {
                file.model = Some(effective_model.clone());
            }
            if protocol.is_some() {
                file.protocol = Some(resolved_protocol.clone());
            }
            if reasoning_raw.is_some() {
                file.reasoning_effort = new_reasoning.clone();
            }
            if let Some(name) = &profile {
                file.default_profile = Some(name.clone());
            }
            if let Some(raw) = &projection_profile_raw {
                file.projection_profile = Some(raw.clone());
            }
            if let Some(n) = adaptive_tournament_raw {
                file.adaptive_tournament = Some(n);
            }
            let path = file
                .save_user_global()
                .map_err(|e| JsonRpcError::new(JSONRPC_INTERNAL_ERROR, e))?;
            persisted_to = Some(path.display().to_string());
        }

        let note = JsonRpcNotification::new(
            "kroma/event",
            json!({
                "schema_version": crate::events::EVENT_SCHEMA_VERSION,
                "event": "config_updated",
                "model": effective_model,
                "protocol": resolved_protocol,
                "reasoning_effort": new_reasoning,
                "persisted_to": persisted_to,
            }),
        );
        Self::broadcast_to_subscribers(&self.inner, note);

        Ok(json!({
            "model": effective_model,
            "protocol": resolved_protocol,
            "reasoning_effort": new_reasoning,
            "profile": profile,
            "persisted_to": persisted_to,
        }))
    }

    /// `sessions.list`: recent session WALs in the state directory, newest first.
    async fn rpc_sessions_list(&self) -> Result<Value, JsonRpcError> {
        let sessions_dir = self.inner.config.state_dir.join("sessions");
        let active = self
            .inner
            .status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .session_id
            .clone();
        let mut out: Vec<Value> = Vec::new();
        let entries = match std::fs::read_dir(&sessions_dir) {
            Ok(e) => e,
            Err(_) => return Ok(json!({ "sessions": [], "active_session_id": active })),
        };
        let mut candidates: Vec<(u128, PathBuf)> = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            let name = match path.file_name().and_then(|n| n.to_str()) {
                Some(n) => n.to_string(),
                None => continue,
            };
            if !name.ends_with(".wal.jsonl") {
                continue;
            }
            let modified = entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis())
                .unwrap_or(0);
            candidates.push((modified, path));
        }
        candidates.sort_by(|a, b| b.0.cmp(&a.0));
        for (modified, path) in candidates.into_iter().take(20) {
            let file_name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string();
            let id = file_name.trim_end_matches(".wal.jsonl").to_string();
            let (entries, turns) = match SessionJournal::open(&path) {
                Ok(journal) => (
                    journal.active_transcript().len(),
                    journal.max_active_turn(),
                ),
                Err(_) => (0, 0),
            };
            out.push(json!({
                "session_id": id,
                "path": path.display().to_string(),
                "entries": entries,
                "turns": turns,
                "modified_ms": modified,
                "active": id == active,
            }));
        }
        Ok(json!({ "sessions": out, "active_session_id": active }))
    }

    /// `session.resume`: attach an existing session WAL (by full id, unique prefix, or `latest`).
    async fn rpc_session_resume(&self, params: &Value) -> Result<Value, JsonRpcError> {
        let requested = params
            .get("session_id")
            .or_else(|| params.get("id"))
            .or_else(|| params.get("resume"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("latest");

        let sessions_dir = self.inner.config.state_dir.join("sessions");
        let target = self.resolve_session_path(&sessions_dir, requested)?;
        let resume = target.display().to_string();
        self.rpc_session_new(&json!({ "resume": resume })).await
    }

    /// Resolve `latest`, an exact session id, or a unique id prefix to a session WAL path.
    fn resolve_session_path(
        &self,
        sessions_dir: &Path,
        requested: &str,
    ) -> Result<PathBuf, JsonRpcError> {
        let mut candidates: Vec<(u128, PathBuf, String)> = Vec::new();
        if let Ok(entries) = std::fs::read_dir(sessions_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                let name = match path.file_name().and_then(|n| n.to_str()) {
                    Some(n) => n.to_string(),
                    None => continue,
                };
                if !name.ends_with(".wal.jsonl") {
                    continue;
                }
                let id = name.trim_end_matches(".wal.jsonl").to_string();
                let modified = entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_millis())
                    .unwrap_or(0);
                candidates.push((modified, path, id));
            }
        }
        candidates.sort_by(|a, b| b.0.cmp(&a.0));

        if requested.eq_ignore_ascii_case("latest") {
            return candidates
                .into_iter()
                .next()
                .map(|(_, path, _)| path)
                .ok_or_else(|| {
                    JsonRpcError::new(
                        JSONRPC_INVALID_PARAMS,
                        "no sessions found to resume".to_string(),
                    )
                });
        }

        let matches: Vec<PathBuf> = candidates
            .into_iter()
            .filter(|(_, _, id)| id == requested || id.starts_with(requested))
            .map(|(_, path, _)| path)
            .collect();
        match matches.len() {
            0 => Err(JsonRpcError::new(
                JSONRPC_INVALID_PARAMS,
                format!("no session matches '{}'", requested),
            )),
            1 => Ok(matches.into_iter().next().unwrap()),
            n => Err(JsonRpcError::new(
                JSONRPC_INVALID_PARAMS,
                format!("session prefix '{}' is ambiguous ({} matches)", requested, n),
            )),
        }
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

    /// Serve newline-delimited JSON-RPC 2.0 over a bidirectional async stream (`kroma acp` stdio
    /// or `kroma serve` TCP/Unix connection).
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
            // F02 (`v1.4.0`): `session.tournament` is dispatched asynchronously like a prompt so a
            // `session.cancel` sent on the same stream connection is read by the loop and reaches
            // the tournament's cancel channel. Previously it was awaited inline, so the connection
            // could not process any other request (including a cancel) while a race was running.
            let is_async_prompt = matches!(
                req.method.as_str(),
                "session.start"
                    | "session.prompt"
                    | "acp.prompt"
                    | "session/prompt"
                    | "session.tournament"
                    | "session/tournament"
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

/// Stateless interactive REPL / TUI consumer (`kroma repl` / `kroma tui` / `--interactive`).
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

        let active_model = self
            .dispatcher
            .active_model()
            .unwrap_or_else(|| "<unset>".to_string());
        let active_thinking = self
            .dispatcher
            .active_reasoning_effort()
            .unwrap_or_else(|| "off".to_string());
        let active_protocol = self
            .dispatcher
            .active_protocol()
            .unwrap_or_else(|| "auto".to_string());

        let renderer = self.dispatcher.theme_renderer();
        if renderer.theme() == Theme::Zen {
            // Design D (`v1.5.0`): borderless ultra-minimal header, session facts, and a one-line
            // hint. The facts line keeps the stable `model: / thinking: / protocol: / policy:`
            // labels that scripts and the existing surface-parity tests key on.
            let mut banner = renderer.banner(
                &active_model,
                &active_thinking,
                &active_protocol,
                hit_pct,
                rev,
                ws,
                session_id,
                policy,
                turns,
            );
            banner.push_str(&format!(
                "{}\n",
                renderer.status_line(
                    StatusDot::Tool,
                    "tokens",
                    &format!("in={} cached={} hit={:.1}%", in_tok, cached_tok, hit_pct)
                )
            ));
            banner.push_str(&format!(
                "{}\n",
                renderer.status_line(
                    StatusDot::Thinking,
                    "thinking",
                    &format!("effort={}", sanitize_terminal_text(&active_thinking))
                )
            ));
            banner.push_str(&format!(
                "{}\n",
                renderer.status_line(
                    StatusDot::Success,
                    "gate",
                    "workspace completion gate armed"
                )
            ));
            banner.push_str(&format!(
                "{}\n",
                renderer.status_line(
                    StatusDot::Alert,
                    "keys",
                    "/theme /status /diff /model /thinking /race /undo"
                )
            ));
            banner.push_str(&format!("{}\n", renderer.hairline(terminal_hairline_width())));
            return banner;
        }

        format!(
            "=== Kroma v{} Interactive REPL ===\n\
             workspace: {} | policy: {} | session: {}\n\
             model: {} | thinking: {} | protocol: {}\n\
             turns: {} | revision: {} | tokens: in={} (cached={}, hit={:.1}%)\n\
             Type a task prompt, or /help for all commands.\n\
             Key commands: /model  /thinking  /resume  /sessions  /new  /undo  /revisions  /graph  /race  /config  /theme  /status  /help  /quit\n",
            crate::version(),
            sanitize_terminal_text(ws),
            sanitize_terminal_text(policy),
            sanitize_terminal_text(session_id),
            sanitize_terminal_text(&active_model),
            sanitize_terminal_text(&active_thinking),
            sanitize_terminal_text(&active_protocol),
            turns,
            sanitize_terminal_text(rev),
            in_tok,
            cached_tok,
            hit_pct,
        )
    }

    /// Shared handler for `/model`, `/thinking`, `/protocol` and `/config`.
    async fn repl_config_command(
        &self,
        kind: &str,
        rest: &[&str],
    ) -> Result<ReplStepOutcome, String> {
        let save = rest.iter().any(|a| *a == "--save");
        let args: Vec<&str> = rest.iter().copied().filter(|a| *a != "--save").collect();
        let out = |text: String| {
            Ok(ReplStepOutcome {
                output: text,
                should_exit: false,
            })
        };

        match kind {
            "model" => {
                if args.is_empty() {
                    let resp = self
                        .dispatcher
                        .handle_request(JsonRpcRequest::new(1, "config.get", json!({})))
                        .await;
                    let val = resp.result.unwrap_or_else(|| json!({}));
                    let active = val
                        .get("active_model")
                        .and_then(Value::as_str)
                        .unwrap_or("<unset>");
                    return out(format!(
                        "model: {}\n",
                        sanitize_terminal_text(active)
                    ));
                }
                let requested = args[0];
                // Mix-up guard: `/model high` is almost certainly a `/thinking` invocation.
                if looks_like_thinking_level(requested) {
                    return out(format!(
                        "hint: '{}' is a thinking level, not a model id.\n  did you mean: /thinking {} [--save]\n",
                        sanitize_terminal_text(requested),
                        sanitize_terminal_text(requested)
                    ));
                }
                let resp = self
                    .dispatcher
                    .handle_request(JsonRpcRequest::new(
                        1,
                        "config.set",
                        json!({ "model": requested, "save": save }),
                    ))
                    .await;
                match resp.result {
                    Some(val) => {
                        let model = val.get("model").and_then(Value::as_str).unwrap_or("-");
                        let protocol = val.get("protocol").and_then(Value::as_str).unwrap_or("-");
                        let persisted = val.get("persisted_to").and_then(Value::as_str);
                        out(format!(
                            "model -> {} (protocol: {}){}\n",
                            sanitize_terminal_text(model),
                            sanitize_terminal_text(protocol),
                            persisted
                                .map(|p| format!(" [saved to {}]", sanitize_terminal_text(p)))
                                .unwrap_or_default()
                        ))
                    }
                    None => out(format!(
                        "error: {}\n",
                        sanitize_terminal_text(
                            &resp
                                .error
                                .map(|e| e.message)
                                .unwrap_or_else(|| "config.set failed".to_string())
                        )
                    )),
                }
            }
            "thinking" => {
                if args.is_empty() {
                    let resp = self
                        .dispatcher
                        .handle_request(JsonRpcRequest::new(1, "config.get", json!({})))
                        .await;
                    let val = resp.result.unwrap_or_else(|| json!({}));
                    let level = val
                        .get("summary")
                        .and_then(Value::as_str)
                        .and_then(|summary| {
                            summary.lines().find_map(|line| {
                                line.strip_prefix("reasoning_effort:")
                                    .map(|v| v.trim().to_string())
                            })
                        })
                        .unwrap_or_else(|| "<unset>".to_string());
                    return out(format!(
                        "thinking: {}\n",
                        sanitize_terminal_text(&level)
                    ));
                }
                let requested = args[0];
                if !looks_like_thinking_level(requested) {
                    return out(format!(
                        "hint: '{}' is not a thinking level (off|low|medium|high|xhigh).\n  did you mean: /model {} [--save]\n",
                        sanitize_terminal_text(requested),
                        sanitize_terminal_text(requested)
                    ));
                }
                let resp = self
                    .dispatcher
                    .handle_request(JsonRpcRequest::new(
                        1,
                        "config.set",
                        json!({ "reasoning_effort": requested, "save": save }),
                    ))
                    .await;
                match resp.result {
                    Some(val) => {
                        let level = val
                            .get("reasoning_effort")
                            .and_then(Value::as_str)
                            .unwrap_or("off");
                        let persisted = val.get("persisted_to").and_then(Value::as_str);
                        out(format!(
                            "thinking -> {}{}\n",
                            sanitize_terminal_text(level),
                            persisted
                                .map(|p| format!(" [saved to {}]", sanitize_terminal_text(p)))
                                .unwrap_or_default()
                        ))
                    }
                    None => out(format!(
                        "error: {}\n",
                        sanitize_terminal_text(
                            &resp
                                .error
                                .map(|e| e.message)
                                .unwrap_or_else(|| "config.set failed".to_string())
                        )
                    )),
                }
            }
            "protocol" => {
                if args.is_empty() {
                    let resp = self
                        .dispatcher
                        .handle_request(JsonRpcRequest::new(1, "config.get", json!({})))
                        .await;
                    let val = resp.result.unwrap_or_else(|| json!({}));
                    let active = val
                        .get("active_protocol")
                        .and_then(Value::as_str)
                        .unwrap_or("<unset>");
                    return out(format!(
                        "protocol: {}\n",
                        sanitize_terminal_text(active)
                    ));
                }
                let resp = self
                    .dispatcher
                    .handle_request(JsonRpcRequest::new(
                        1,
                        "config.set",
                        json!({ "protocol": args[0], "save": save }),
                    ))
                    .await;
                match resp.result {
                    Some(val) => {
                        let protocol = val.get("protocol").and_then(Value::as_str).unwrap_or("-");
                        out(format!(
                            "protocol -> {}\n",
                            sanitize_terminal_text(protocol)
                        ))
                    }
                    None => out(format!(
                        "error: {}\n",
                        sanitize_terminal_text(
                            &resp
                                .error
                                .map(|e| e.message)
                                .unwrap_or_else(|| "config.set failed".to_string())
                        )
                    )),
                }
            }
            _ => {
                // `/config [show | set <key> <value> | profile <name>]`
                let sub = args.first().copied().unwrap_or("show");
                match sub {
                    "show" => {
                        let resp = self
                            .dispatcher
                            .handle_request(JsonRpcRequest::new(1, "config.get", json!({})))
                            .await;
                        let val = resp.result.unwrap_or_else(|| json!({}));
                        let summary = val
                            .get("summary")
                            .and_then(Value::as_str)
                            .unwrap_or("<no layered configuration>");
                        let guard = val
                            .get("rejected_workspace_keys")
                            .and_then(Value::as_array)
                            .map(|list| {
                                list.iter()
                                    .filter_map(Value::as_str)
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            })
                            .unwrap_or_default();
                        let mut text = sanitize_terminal_text(summary);
                        if !guard.is_empty() {
                            text.push_str(&format!(
                                "guard: ignored untrusted workspace keys: {}\n",
                                sanitize_terminal_text(&guard)
                            ));
                        }
                        out(text)
                    }
                    "set" => {
                        if args.len() < 3 {
                            return out(
                                "usage: /config set <key> <value>\n  keys: default_profile, model, base_url, api_key, protocol, reasoning_effort, policy, verify_command, max_subagents, projection_profile, adaptive_tournament\n"
                                    .to_string(),
                            );
                        }
                        let key = args[1];
                        let value = args[2..].join(" ");
                        let mut payload = json!({ "save": true });
                        if let Value::Object(ref mut map) = payload {
                            map.insert(key.to_string(), Value::String(value.clone()));
                        }
                        let resp = self
                            .dispatcher
                            .handle_request(JsonRpcRequest::new(1, "config.set", payload))
                            .await;
                        match resp.result {
                            Some(val) => out(format!(
                                "{} = {} [saved to {}]\n",
                                sanitize_terminal_text(key),
                                sanitize_terminal_text(&value),
                                sanitize_terminal_text(
                                    val.get("persisted_to")
                                        .and_then(Value::as_str)
                                        .unwrap_or("<unknown>")
                                )
                            )),
                            None => out(format!(
                                "error: {}\n",
                                sanitize_terminal_text(
                                    &resp
                                        .error
                                        .map(|e| e.message)
                                        .unwrap_or_else(|| "config.set failed".to_string())
                                )
                            )),
                        }
                    }
                    "profile" => {
                        if args.len() < 2 {
                            return out("usage: /config profile <name>\n".to_string());
                        }
                        let resp = self
                            .dispatcher
                            .handle_request(JsonRpcRequest::new(
                                1,
                                "config.set",
                                json!({ "profile": args[1], "save": save }),
                            ))
                            .await;
                        match resp.result {
                            Some(val) => out(format!(
                                "profile -> {} (model: {}, protocol: {})\n",
                                sanitize_terminal_text(args[1]),
                                sanitize_terminal_text(
                                    val.get("model").and_then(Value::as_str).unwrap_or("-")
                                ),
                                sanitize_terminal_text(
                                    val.get("protocol").and_then(Value::as_str).unwrap_or("-")
                                ),
                            )),
                            None => out(format!(
                                "error: {}\n",
                                sanitize_terminal_text(
                                    &resp
                                        .error
                                        .map(|e| e.message)
                                        .unwrap_or_else(|| "config.set failed".to_string())
                                )
                            )),
                        }
                    }
                    other => out(format!(
                        "unknown /config subcommand '{}'; use show | set <key> <value> | profile <name>\n",
                        sanitize_terminal_text(other)
                    )),
                }
            }
        }
    }

    /// `/sessions`: list recent session WALs.
    async fn repl_sessions_command(&self) -> Result<ReplStepOutcome, String> {
        let resp = self
            .dispatcher
            .handle_request(JsonRpcRequest::new(1, "sessions.list", json!({})))
            .await;
        let val = match resp.result {
            Some(v) => v,
            None => {
                return Ok(ReplStepOutcome {
                    output: format!(
                        "error: {}\n",
                        sanitize_terminal_text(
                            &resp
                                .error
                                .map(|e| e.message)
                                .unwrap_or_else(|| "sessions.list failed".to_string())
                        )
                    ),
                    should_exit: false,
                })
            }
        };
        let sessions = val
            .get("sessions")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if sessions.is_empty() {
            return Ok(ReplStepOutcome {
                output: "no sessions found.\n".to_string(),
                should_exit: false,
            });
        }
        let mut text = format!("sessions ({}):\n", sessions.len());
        for entry in sessions {
            let id = entry.get("session_id").and_then(Value::as_str).unwrap_or("-");
            let entries = entry.get("entries").and_then(Value::as_u64).unwrap_or(0);
            let turns = entry.get("turns").and_then(Value::as_u64).unwrap_or(0);
            let active = entry.get("active").and_then(Value::as_bool).unwrap_or(false);
            text.push_str(&format!(
                "  {}{}  entries={} turns={}\n",
                if active { "* " } else { "  " },
                sanitize_terminal_text(id),
                entries,
                turns
            ));
        }
        text.push_str("use /resume <id|latest> to attach one.\n");
        Ok(ReplStepOutcome {
            output: text,
            should_exit: false,
        })
    }

    /// `/resume [<latest|id>]`: attach an existing session WAL.
    async fn repl_resume_command(&self, rest: &[&str]) -> Result<ReplStepOutcome, String> {
        let target = rest.first().copied().unwrap_or("latest");
        let resp = self
            .dispatcher
            .handle_request(JsonRpcRequest::new(
                1,
                "session.resume",
                json!({ "session_id": target }),
            ))
            .await;
        match resp.result {
            Some(val) => {
                let sid = val.get("session_id").and_then(Value::as_str).unwrap_or("-");
                let turns = val.get("turn_count").and_then(Value::as_u64).unwrap_or(0);
                Ok(ReplStepOutcome {
                    output: format!(
                        "resumed session {} ({} turns restored)\n",
                        sanitize_terminal_text(sid),
                        turns
                    ),
                    should_exit: false,
                })
            }
            None => Ok(ReplStepOutcome {
                output: format!(
                    "error: {}\n",
                    sanitize_terminal_text(
                        &resp
                            .error
                            .map(|e| e.message)
                            .unwrap_or_else(|| "session.resume failed".to_string())
                    )
                ),
                should_exit: false,
            }),
        }
    }

    /// `/new`: start a fresh session without exiting the REPL.
    async fn repl_new_session_command(&self) -> Result<ReplStepOutcome, String> {
        let resp = self
            .dispatcher
            .handle_request(JsonRpcRequest::new(1, "session.new", json!({})))
            .await;
        let sid = resp
            .result
            .and_then(|v| v.get("session_id").and_then(Value::as_str).map(String::from))
            .unwrap_or_default();
        Ok(ReplStepOutcome {
            output: format!("started new session {}\n", sanitize_terminal_text(&sid)),
            should_exit: false,
        })
    }

    /// `/race <N> <task>`: Best-of-N worktree tournament.
    async fn repl_race_command(&self, rest: &[&str]) -> Result<ReplStepOutcome, String> {
        if rest.len() < 2 {
            return Ok(ReplStepOutcome {
                output: "usage: /race <N> <task>   (N between 2 and 4)\n".to_string(),
                should_exit: false,
            });
        }
        let count = match rest[0].parse::<usize>() {
            Ok(n) => n,
            Err(_) => {
                return Ok(ReplStepOutcome {
                    output: format!(
                        "usage: /race <N> <task>   ('{}' is not a number)\n",
                        sanitize_terminal_text(rest[0])
                    ),
                    should_exit: false,
                })
            }
        };
        let task = rest[1..].join(" ");
        let resp = self
            .dispatcher
            .handle_request(JsonRpcRequest::new(
                1,
                "session.tournament",
                json!({ "candidates": count, "task": task }),
            ))
            .await;
        match resp.result {
            Some(val) => {
                let winner = val
                    .get("winner")
                    .and_then(Value::as_str)
                    .unwrap_or("<none>");
                let merged = val
                    .get("merged_revision")
                    .and_then(Value::as_str)
                    .unwrap_or("<none>");
                let cleaned = val
                    .get("all_worktrees_cleaned_up")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let mut text = format!(
                    "tournament complete: winner={} merged_revision={} worktrees_cleaned_up={}\n",
                    sanitize_terminal_text(winner),
                    sanitize_terminal_text(merged),
                    cleaned
                );
                if let Some(list) = val.get("candidates").and_then(Value::as_array) {
                    for c in list {
                        text.push_str(&format!(
                            "  {} status={} gate_passed={} warnings={} changed_lines={} turns={}{}\n",
                            sanitize_terminal_text(
                                c.get("label").and_then(Value::as_str).unwrap_or("-")
                            ),
                            sanitize_terminal_text(
                                c.get("status").and_then(Value::as_str).unwrap_or("-")
                            ),
                            c.get("gate_passed").and_then(Value::as_bool).unwrap_or(false),
                            c.get("predictive_warnings").and_then(Value::as_u64).unwrap_or(0),
                            c.get("total_changed_lines").and_then(Value::as_u64).unwrap_or(0),
                            c.get("turns_used").and_then(Value::as_u64).unwrap_or(0),
                            if c.get("winner").and_then(Value::as_bool).unwrap_or(false) {
                                "  <-- winner"
                            } else {
                                ""
                            }
                        ));
                    }
                }
                Ok(ReplStepOutcome {
                    output: text,
                    should_exit: false,
                })
            }
            None => Ok(ReplStepOutcome {
                output: format!(
                    "error: {}\n",
                    sanitize_terminal_text(
                        &resp
                            .error
                            .map(|e| e.message)
                            .unwrap_or_else(|| "session.tournament failed".to_string())
                    )
                ),
                should_exit: false,
            }),
        }
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
                        "  /help                       Show this command reference\n",
                        "  /status                     Show active session, model, thinking, revision and token status\n",
                        "  /model [<id>] [--save]      Show or hot-swap the active model (--save persists it)\n",
                        "  /thinking [<lvl>] [--save]  Show or hot-swap reasoning effort (off|low|medium|high|xhigh)\n",
                        "  /protocol [<name>]          Show or hot-swap the wire protocol (auto|chat-completions|\n",
                        "                              anthropic-messages|openai-responses)\n",
                        "  /config [show|set <k> <v>|profile <name>]  Inspect or persist layered configuration\n",
                        "  /theme [zen|classic]        Show or switch the terminal theme (zen = Design D, default)\n",
                        "  /sessions                   List recent sessions in the state directory\n",
                        "  /resume [<latest|id>]       Attach an existing session WAL as the active session\n",
                        "  /new | /clear               Start a fresh session in this workspace\n",
                        "  /revisions                  List captured workspace revisions for this session\n",
                        "  /undo [--force]             Restore workspace & session leaf to previous revision\n",
                        "  /restore <rev> [--force]    Restore workspace to a specific revision ID\n",
                        "  /diff                       Show files changed in the latest revision\n",
                        "  /skills [<name>]            List workspace skills or activate <name>\n",
                        "  /graph [<query>]            Rank files with Personalized PageRank over the symbol graph\n",
                        "  /race <N> <task>            Best-of-N worktree tournament: race N workers, merge the best\n",
                        "  /fork <turn>                Rewind/branch active session at <turn>\n",
                        "  /inspect <id>               Recall raw tool evidence by source_entry_id\n",
                        "  /quit | /exit               Shut down the interactive session\n"
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
                    // `v1.5.0`: prefix the live view with a Design D status-dot line. The body keeps
                    // its stable `key: value` lines so scripts and parity tests are unaffected.
                    let renderer = self.dispatcher.theme_renderer();
                    let mut out = String::new();
                    if renderer.theme() == Theme::Zen {
                        out.push_str(&renderer.status_line(
                            StatusDot::Tool,
                            "status",
                            &format!(
                                "session={} policy={} turns={} rev={}",
                                sanitize_terminal_text(sid),
                                sanitize_terminal_text(policy),
                                turns,
                                crate::theme::rev_label(rev),
                            ),
                        ));
                        out.push('\n');
                    }
                    out.push_str(&format!(
                        "session_id: {}\npolicy: {}\nturn_count: {}\nlatest_revision: {}\nmutations: {}\nsubagents_spawned: {}\n",
                        sanitize_terminal_text(sid),
                        sanitize_terminal_text(policy),
                        turns,
                        sanitize_terminal_text(rev),
                        mut_count,
                        sub_count,
                    ));
                    Ok(ReplStepOutcome {
                        output: out,
                        should_exit: false,
                    })
                }
                "/theme" | "/ui" => {
                    // `v1.5.0`: show or switch the Design D / classic terminal theme.
                    let renderer = self.dispatcher.theme_renderer();
                    let Some(&requested) = rest.first() else {
                        return Ok(ReplStepOutcome {
                            output: format!(
                                "theme: {} (available: zen, classic)\n",
                                renderer.theme().as_str()
                            ),
                            should_exit: false,
                        });
                    };
                    if Theme::parse(requested).is_err() {
                        return Ok(ReplStepOutcome {
                            output: format!(
                                "error: unknown theme '{}'; expected 'zen' or 'classic'\n",
                                sanitize_terminal_text(requested)
                            ),
                            should_exit: false,
                        });
                    }
                    let resp = self
                        .dispatcher
                        .handle_request(JsonRpcRequest::new(
                            1,
                            "config.set",
                            json!({ "ui_theme": requested }),
                        ))
                        .await;
                    match resp.result {
                        Some(val) => {
                            let theme = val
                                .get("ui_theme")
                                .and_then(Value::as_str)
                                .unwrap_or(requested);
                            let renderer = self.dispatcher.theme_renderer();
                            let mut out = String::new();
                            if renderer.theme() == Theme::Zen {
                                out.push_str(&renderer.status_line(
                                    StatusDot::Success,
                                    "theme",
                                    &format!("switched to {}", theme),
                                ));
                                out.push('\n');
                            }
                            out.push_str(&format!(
                                "theme -> {}{}\n",
                                sanitize_terminal_text(theme),
                                if theme == "zen" {
                                    " (Design D: borderless, ultra-minimal)"
                                } else {
                                    ""
                                }
                            ));
                            Ok(ReplStepOutcome {
                                output: out,
                                should_exit: false,
                            })
                        }
                        None => Ok(ReplStepOutcome {
                            output: format!(
                                "error: {}\n",
                                sanitize_terminal_text(
                                    &resp
                                        .error
                                        .map(|e| e.message)
                                        .unwrap_or_else(|| "config.set failed".to_string())
                                )
                            ),
                            should_exit: false,
                        }),
                    }
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
                    let mut out = String::new();
                    if cmd == "/diff" {
                        // `v1.5.0`: Design D header for the live diff view. The stable
                        // `revisions (N captured):` body below is preserved for scripts.
                        let renderer = self.dispatcher.theme_renderer();
                        if renderer.theme() == Theme::Zen {
                            out.push_str(&renderer.status_line(
                                StatusDot::Tool,
                                "diff",
                                &format!("{} workspace revision(s) captured", revs.len()),
                            ));
                            out.push('\n');
                            out.push_str(&renderer.hairline(terminal_hairline_width()));
                            out.push('\n');
                        }
                    }
                    out.push_str(&format!("revisions ({} captured):\n", revs.len()));
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
                "/graph" => {
                    let query = rest.join(" ");
                    let params = if query.trim().is_empty() {
                        json!({})
                    } else {
                        json!({ "query": query })
                    };
                    let resp = self
                        .dispatcher
                        .handle_request(JsonRpcRequest::new(1, "workspace.graph", params))
                        .await;
                    let val = resp.result.ok_or_else(|| {
                        resp.error
                            .map(|e| e.message)
                            .unwrap_or_else(|| "workspace.graph failed".to_string())
                    })?;
                    let total_files = val
                        .get("total_files")
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    let total_symbols = val
                        .get("total_symbols")
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    let total_edges = val
                        .get("total_edges")
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    let ranked = val
                        .get("ranked")
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default();
                    if ranked.is_empty() {
                        return Ok(ReplStepOutcome {
                            output: format!(
                                "repo graph: {} files, {} symbols, {} edges; no ranked files.\n",
                                total_files, total_symbols, total_edges
                            ),
                            should_exit: false,
                        });
                    }
                    let mut out = format!(
                        "repo graph: {} files, {} symbols, {} edges\n",
                        total_files, total_symbols, total_edges
                    );
                    for (i, node) in ranked.iter().enumerate() {
                        let path = node.get("path").and_then(Value::as_str).unwrap_or("-");
                        let score = node.get("score").and_then(Value::as_f64).unwrap_or(0.0);
                        out.push_str(&format!(
                            "  {} {} (score={:.4})\n",
                            i + 1,
                            sanitize_terminal_text(path),
                            score
                        ));
                        if let Some(syms) = node.get("symbols").and_then(Value::as_array) {
                            for s in syms {
                                let kind = s.get("kind").and_then(Value::as_str).unwrap_or("");
                                let sig = s.get("signature").and_then(Value::as_str).unwrap_or("");
                                out.push_str(&format!(
                                    "      {} {}\n",
                                    kind,
                                    sanitize_terminal_text(sig)
                                ));
                            }
                        }
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
                "/model" => self.repl_config_command("model", &rest).await,
                "/thinking" => self.repl_config_command("thinking", &rest).await,
                "/protocol" => self.repl_config_command("protocol", &rest).await,
                "/config" => self.repl_config_command("config", &rest).await,
                "/sessions" => self.repl_sessions_command().await,
                "/resume" => self.repl_resume_command(&rest).await,
                "/new" => self.repl_new_session_command().await,
                "/race" => self.repl_race_command(&rest).await,
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
                let in_tok = val
                    .get("total_input_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let out_tok = val
                    .get("total_output_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let child_in_tok = val
                    .get("children_input_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let child_out_tok = val
                    .get("children_output_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let hit_pct = val
                    .get("cache_hit_ratio")
                    .and_then(Value::as_f64)
                    .unwrap_or(0.0)
                    * 100.0;
                let modified: Vec<String> = val
                    .get("modified_files")
                    .and_then(Value::as_array)
                    .map(|arr| {
                        arr.iter()
                            .filter_map(Value::as_str)
                            .map(sanitize_terminal_text)
                            .collect()
                    })
                    .unwrap_or_default();
                let mut out = String::new();
                if !text.is_empty() {
                    out.push_str(&sanitize_terminal_text(text));
                    if !out.ends_with('\n') {
                        out.push('\n');
                    }
                }
                if status != "completed" {
                    if let Some(reason) = val.get("terminal_reason").and_then(Value::as_str) {
                        out.push_str(&format!("error: {}\n", sanitize_terminal_text(reason)));
                    }
                }
                out.push_str(&format!(
                    "[status={} turns={} tools={} rev={}]\n",
                    sanitize_terminal_text(status),
                    turns,
                    tools,
                    sanitize_terminal_text(rev),
                ));
                // `v1.5.0` Design D footer: a spectrum status dot, the files touched, and a
                // compact telemetry line (`self + child` token spend, cache hit, revision).
                let renderer = self.dispatcher.theme_renderer();
                if renderer.theme() == Theme::Zen {
                    let dot = match status {
                        "completed" => StatusDot::Success,
                        "cancelled" => StatusDot::Alert,
                        _ => StatusDot::Error,
                    };
                    out.push_str(&renderer.status_line(
                        dot,
                        "run",
                        &format!("status={} turns={} tools={}", status, turns, tools),
                    ));
                    out.push('\n');
                    if !modified.is_empty() {
                        out.push_str(&renderer.status_line(
                            StatusDot::Success,
                            "edit",
                            &format!("{} file(s) changed: {}", modified.len(), modified.join(", ")),
                        ));
                        out.push('\n');
                    }
                    out.push_str(&renderer.telemetry_footer(
                        in_tok.saturating_add(out_tok),
                        child_in_tok.saturating_add(child_out_tok),
                        hit_pct,
                        rev,
                        0,
                    ));
                    out.push('\n');
                }
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

    /// `v1.5.0`: map a live `kroma/event` notification to a Design D status line.
    ///
    /// Returns `None` for notifications that belong to another session, are not `kroma/event`, or
    /// carry no terminal-worthy state. This is what makes tool start/complete, model turns, policy
    /// decisions, predictive checks, revision captures and subagent activity stream live.
    pub fn render_live_event(&self, note: &JsonRpcNotification) -> Option<String> {
        if note.method != "kroma/event" {
            return None;
        }
        let params = &note.params;
        let active_session = self.dispatcher.session_id();
        if let Some(session) = params.get("session_id").and_then(Value::as_str) {
            if !active_session.is_empty() && session != active_session {
                return None;
            }
        }
        // Serde serialises `EventPayload` with the variant name as the tag (`ToolStarted`); accept
        // the snake_case spelling too so hand-written emitters work as well.
        let event_type = params.get("event_type").and_then(Value::as_str)?;
        let event_key = event_type.to_ascii_lowercase().replace('_', "");
        let payload = params.get("payload").cloned().unwrap_or(Value::Null);
        let renderer = self.dispatcher.theme_renderer();
        if renderer.theme() != Theme::Zen {
            return None;
        }
        let str_of = |key: &str| {
            payload
                .get(key)
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        };
        let num_of = |key: &str| payload.get(key).and_then(Value::as_u64).unwrap_or(0);

        let line = match event_key.as_str() {
            "toolstarted" => {
                let tool = str_of("tool_name");
                let dot = if matches!(tool.as_str(), "edit" | "write") {
                    StatusDot::Success
                } else {
                    StatusDot::Tool
                };
                let path = str_of("relative_path");
                renderer.status_line(dot, &tool, &path)
            }
            "toolcompleted" => {
                let tool = str_of("tool_name");
                let outcome = str_of("outcome");
                let ok = outcome == "success";
                let dot = if ok { StatusDot::Success } else { StatusDot::Error };
                let path = str_of("relative_path");
                let tier = payload
                    .get("edit_match_tier")
                    .and_then(Value::as_str)
                    .map(|t| format!(" [{}]", t))
                    .unwrap_or_default();
                let detail = format!(
                    "{}{} {} {}ms",
                    if ok { "\u{2713}" } else { "\u{2717}" },
                    tier,
                    path,
                    num_of("duration_ms")
                );
                renderer.status_line(dot, &tool, detail.trim())
            }
            "modelrequestcompleted" => {
                let hit = payload
                    .get("prefix_cache_hit_ratio")
                    .and_then(Value::as_f64)
                    .unwrap_or(0.0)
                    * 100.0;
                let detail = format!(
                    "turn {} · in {} (cached {}) / out {} · cache {:.1}% · {}ms",
                    num_of("turn"),
                    num_of("input_tokens"),
                    num_of("cached_input_tokens"),
                    num_of("output_tokens"),
                    hit,
                    num_of("latency_ms")
                );
                renderer.status_line(StatusDot::Thinking, "thinking", &detail)
            }
            "policydecision" => {
                let decision = str_of("decision");
                let dot = if decision.contains("deny") {
                    StatusDot::Alert
                } else {
                    StatusDot::Tool
                };
                renderer.status_line(
                    dot,
                    "policy",
                    &format!("{} → {}", str_of("tool_name"), decision),
                )
            }
            "predictivecheck" => {
                let status = str_of("status");
                let dot = if status == "ok" {
                    StatusDot::Success
                } else if status == "skipped" {
                    StatusDot::Tool
                } else {
                    StatusDot::Alert
                };
                renderer.status_line(
                    dot,
                    "check",
                    &format!(
                        "{} · {} issue(s) · {}ms",
                        str_of("path"),
                        num_of("issue_count"),
                        num_of("duration_ms")
                    ),
                )
            }
            "revisioncaptured" => {
                let changed = payload
                    .get("changed_paths")
                    .and_then(Value::as_array)
                    .map(|a| a.len())
                    .unwrap_or(0);
                renderer.status_line(
                    StatusDot::Success,
                    "revision",
                    &format!(
                        "rev {} · {} file(s) · {}",
                        crate::theme::short_rev(&str_of("revision_id")),
                        changed,
                        str_of("trigger")
                    ),
                )
            }
            "subagentspawned" => renderer.status_line(
                StatusDot::Thinking,
                "subagent",
                &format!(
                    "{} ({}) · worktree={}",
                    str_of("subagent_id"),
                    str_of("role"),
                    payload
                        .get("isolated_worktree")
                        .and_then(Value::as_bool)
                        .unwrap_or(false)
                ),
            ),
            "subagentcompleted" => {
                let status = str_of("status");
                let dot = if status == "completed" {
                    StatusDot::Success
                } else {
                    StatusDot::Alert
                };
                renderer.status_line(
                    dot,
                    "subagent",
                    &format!(
                        "{} · {} turn(s) · {} tok · {}ms",
                        status,
                        num_of("turns_used"),
                        num_of("tokens_used"),
                        num_of("duration_ms")
                    ),
                )
            }
            "approvalrequested" => renderer.status_line(
                StatusDot::Alert,
                "approval",
                &format!("{} awaiting decision on {}", str_of("tool_name"), str_of("surface")),
            ),
            "loopguardtriggered" => renderer.status_line(
                StatusDot::Alert,
                "loop-guard",
                &format!("{} blocked ({})", str_of("tool_name"), str_of("guard_kind")),
            ),
            "runterminated" => {
                let status = str_of("status");
                let dot = if status.to_ascii_lowercase().contains("completed") {
                    StatusDot::Success
                } else {
                    StatusDot::Error
                };
                renderer.status_line(
                    dot,
                    "gate",
                    &format!(
                        "run {} · {} turn(s) · {} tool call(s)",
                        status,
                        num_of("turns"),
                        num_of("total_tool_calls")
                    ),
                )
            }
            _ => return None,
        };
        Some(format!("{}\n", line))
    }

    /// `v1.5.0`: run one prompt turn while streaming Design D status lines for its live events.
    ///
    /// The turn executes on a worker task so this task can drain `kroma/event` notifications and
    /// print them as they happen; the final [`ReplStepOutcome`] is returned to the caller, which
    /// owns the summary and telemetry footer.
    pub async fn run_turn_streaming<W>(
        &self,
        prompt: &str,
        writer: &mut W,
    ) -> Result<ReplStepOutcome, String>
    where
        W: AsyncWrite + Unpin,
    {
        let mut rx = self.dispatcher.subscribe_notifications();
        let turn_repl = InteractiveRepl::new(self.dispatcher.clone());
        let prompt_owned = prompt.to_string();
        let mut handle = tokio::spawn(async move { turn_repl.handle_line(&prompt_owned).await });

        let step = loop {
            tokio::select! {
                biased;
                res = &mut handle => {
                    break res.map_err(|e| format!("REPL turn task failed: {}", e))??;
                }
                maybe = rx.recv() => {
                    if let Some(note) = maybe {
                        if let Some(line) = self.render_live_event(&note) {
                            writer.write_all(line.as_bytes()).await.map_err(|e| e.to_string())?;
                            writer.flush().await.map_err(|e| e.to_string())?;
                        }
                    }
                }
            }
        };
        // Flush any events that were queued just before termination.
        while let Ok(note) = rx.try_recv() {
            if let Some(line) = self.render_live_event(&note) {
                writer.write_all(line.as_bytes()).await.map_err(|e| e.to_string())?;
            }
        }
        writer.flush().await.map_err(|e| e.to_string())?;
        Ok(step)
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
            let step = if prompt.starts_with('/') {
                self.handle_line(prompt).await?
            } else {
                // `v1.5.0`: stream Design D status lines for the live events of this turn.
                self.run_turn_streaming(prompt, &mut writer).await?
            };
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
            // P2 (`v1.5.0`): recompute the prefix every iteration so `/theme` and `/ui` switches take
            // effect on the very next prompt instead of staying stuck on the startup theme.
            let renderer = self.dispatcher.theme_renderer();
            let prompt_prefix = if renderer.theme() == Theme::Zen {
                renderer.prompt_prefix()
            } else {
                "kroma> ".to_string()
            };
            writer
                .write_all(prompt_prefix.as_bytes())
                .await
                .map_err(|e| e.to_string())?;
            writer.flush().await.map_err(|e| e.to_string())?;

            let Some(line) = read_bounded_jsonrpc_line(&mut reader, MAX_RPC_FRAME_BYTES).await?
            else {
                break;
            };
            let step = if line.trim_start().starts_with('/') {
                self.handle_line(&line).await?
            } else {
                self.run_turn_streaming(&line, &mut writer).await?
            };
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
