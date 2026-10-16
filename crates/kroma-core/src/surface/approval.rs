//! approval surface of the multi-surface runtime (split from `surface.rs`).

use super::*;

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

/// P1 (`v1.5.0`): human-readable approval banner for an interactive surface that must decide on the
/// terminal. Mirrors the headless `TtyApprovalUi` prompt, but is built from the same
/// [`ApprovalPendingInfo`] the broker publishes, so it can be rendered the moment the request is
/// raised (before the synchronous policy wait) instead of after it resolves.
pub fn format_approval_prompt(info: &ApprovalPendingInfo) -> String {
    let mut out = String::new();
    if info.is_sensitive_path {
        out.push_str("=== [HIGH-RISK SENSITIVE PATH APPROVAL REQUIRED] ===\n");
    } else {
        out.push_str("=== [KROMA TOOL APPROVAL REQUIRED] ===\n");
    }
    out.push_str(&format!(
        "Tool:      {}\nEffect:    {}\n",
        sanitize_terminal_text(&info.tool_name),
        sanitize_terminal_text(&info.effect_class)
    ));
    if let Some(ref target) = info.target_path {
        out.push_str(&format!("Target:    {}\n", sanitize_terminal_text(target)));
    }
    if !info.arguments_summary.is_empty() {
        out.push_str(&format!(
            "Summary:   {}\n",
            sanitize_terminal_text(&info.arguments_summary)
        ));
    }
    if !info.risk_reason.is_empty() {
        out.push_str(&format!(
            "Risk:      {}\n",
            sanitize_terminal_text(&info.risk_reason)
        ));
    }
    out.push_str(
        "Choose: [y] Allow once | [a] Allow this tool category for current run | [n] Deny: ",
    );
    out
}

/// P1 (`v1.5.0`): map an operator's one-line answer to the canonical `approval.respond` decision.
/// Anything that is not an explicit approval verb maps to `deny`, so an unrecognized keystroke
/// fails closed.
pub fn approval_decision_from_answer(answer: &str) -> &'static str {
    match answer.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" | "approve_once" | "allow_once" => "approve_once",
        "a" | "all" | "approve_session" | "allow_session" => "approve_session",
        _ => "deny",
    }
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

/// State for the ACP `session/request_permission` bridge (F06).
///
/// When the `acp` surface enables this, [`ApprovalHandler::request_approval`] sends a standard
/// `session/request_permission` **request** to the client and blocks until the client's *response*
/// arrives on the same stream (routed by [`RpcApprovalBroker::resolve_acp_permission`]) instead of
/// emitting the internal `approval/requested` notification.
#[derive(Default)]
struct AcpPermissionState {
    /// B01: the run instance (`run_id`) that currently owns the ACP sink, if any.
    owner: Option<u64>,
    session_id: Option<String>,
    sink: Option<Arc<dyn Fn(Value) + Send + Sync>>,
    pending: BTreeMap<String, (u64, std_mpsc::Sender<Value>)>,
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
    acp: Mutex<AcpPermissionState>,
    #[cfg(test)]
    pre_acp_insert_hook: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
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
            acp: Mutex::new(AcpPermissionState::default()),
            #[cfg(test)]
            pre_acp_insert_hook: Mutex::new(None),
        })
    }

    /// F06: route tool approvals through the standard ACP `session/request_permission` request. The
    /// `sink` writes one serialized JSON-RPC request frame to the client stream; the client's
    /// response is delivered back through [`RpcApprovalBroker::resolve_acp_permission`].
    pub fn enable_acp_permissions(
        &self,
        session_id: String,
        sink: Arc<dyn Fn(Value) + Send + Sync>,
    ) {
        // Legacy/untagged entry: owner `0` is reserved (run ids start at 1).
        self.enable_acp_permissions_for(session_id, sink, 0);
    }

    /// B01: enable the ACP sink tagged with the run instance (`owner`) that owns it, so only that
    /// run's end clears it and only its pending requests are denied.
    pub fn enable_acp_permissions_for(
        &self,
        session_id: String,
        sink: Arc<dyn Fn(Value) + Send + Sync>,
        owner: u64,
    ) {
        let mut guard = self.acp.lock().unwrap();
        guard.owner = Some(owner);
        guard.session_id = Some(session_id);
        guard.sink = Some(sink);
    }

    /// F06: update which session id subsequent `session/request_permission` frames name (called by
    /// ACP `session/new`).
    pub fn set_acp_session_id(&self, session_id: &str) {
        self.acp.lock().unwrap().session_id = Some(session_id.to_string());
    }

    /// F06: stop issuing ACP permission requests and cancel any that are still waiting.
    pub fn disable_acp_permissions(&self) {
        let drained = {
            let mut guard = self.acp.lock().unwrap();
            guard.owner = None;
            guard.session_id = None;
            guard.sink = None;
            std::mem::take(&mut guard.pending)
        };
        for (_, (_, tx)) in drained {
            let _ = tx.send(json!({ "outcome": { "outcome": "cancelled" } }));
        }
    }

    /// B01: clear the ACP sink only when `owner` still owns it, and deny only `owner`'s pending
    /// requests. A successor run's approval channel is never touched.
    pub fn disable_acp_permissions_for(&self, owner: u64) {
        let drained = {
            let mut guard = self.acp.lock().unwrap();
            if guard.owner == Some(owner) {
                guard.owner = None;
                guard.session_id = None;
                guard.sink = None;
            }
            let ids: Vec<String> = guard
                .pending
                .iter()
                .filter(|(_, (pending_owner, _))| *pending_owner == owner)
                .map(|(id, _)| id.clone())
                .collect();
            let mut drained = Vec::with_capacity(ids.len());
            for id in ids {
                if let Some((_, tx)) = guard.pending.remove(&id) {
                    drained.push(tx);
                }
            }
            drained
        };
        for tx in drained {
            let _ = tx.send(json!({ "outcome": { "outcome": "cancelled" } }));
        }
    }

    /// F06: deliver a client's response to a pending `session/request_permission` request.
    ///
    /// Untagged entry point (no owner check); the daemon stream uses
    /// [`RpcApprovalBroker::resolve_acp_permission_owned`] instead. Kept for API compatibility.
    pub fn resolve_acp_permission(&self, request_id: &str, result: Value) -> Result<(), String> {
        let waiter = self.acp.lock().unwrap().pending.remove(request_id);
        match waiter {
            Some((_, tx)) => {
                let _ = tx.send(result);
                Ok(())
            }
            None => Err(format!(
                "unknown or already resolved ACP permission request id '{}'",
                request_id
            )),
        }
    }

    /// Finding 4: resolve a pending ACP permission request only when it belongs to `owner`'s run.
    /// A wrong-owner or stale response is rejected without resolving anything.
    pub fn resolve_acp_permission_owned(
        &self,
        owner: u64,
        request_id: &str,
        result: Value,
    ) -> Result<(), String> {
        let mut guard = self.acp.lock().unwrap();
        if guard.owner != Some(owner) {
            return Err("permission request does not belong to the active run".to_string());
        }
        match guard.pending.remove(request_id) {
            Some((pending_owner, tx)) if pending_owner == owner => {
                let _ = tx.send(result);
                Ok(())
            }
            Some(entry) => {
                // Wrong owner: put it back untouched and fail closed.
                guard.pending.insert(request_id.to_string(), entry);
                Err("permission request does not belong to the active run".to_string())
            }
            None => Err(format!(
                "unknown or already resolved ACP permission request id '{}'",
                request_id
            )),
        }
    }

    /// Cancel every ACP permission wait that is still outstanding (connection close / cancel).
    fn cancel_acp_pending(&self) {
        let drained = std::mem::take(&mut self.acp.lock().unwrap().pending);
        for (_, (_, tx)) in drained {
            let _ = tx.send(json!({ "outcome": { "outcome": "cancelled" } }));
        }
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
        self.cancel_acp_pending();
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
        self.cancel_acp_pending();
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
                None if guard.pending.len() == 1 => guard.pending.keys().next().cloned().unwrap(),
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
            diff.lines()
                .next()
                .unwrap_or("")
                .chars()
                .take(200)
                .collect()
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

    fn record_resolved(&self, request_id: &str, decision: &str) {
        let mut guard = self.inner.lock().unwrap();
        guard.recorded_events.push(EventPayload::ApprovalResolved {
            request_id: request_id.to_string(),
            decision: decision.to_string(),
        });
    }

    /// Map an ACP `RequestPermissionResponse.outcome` to an [`ApprovalDecision`]; anything that is
    /// not an explicit `allow_*` selection fails closed.
    fn decision_from_acp_outcome(result: &Value) -> ApprovalDecision {
        if result
            .get("outcome")
            .and_then(|o| o.get("outcome"))
            .and_then(Value::as_str)
            != Some("selected")
        {
            return ApprovalDecision::Deny;
        }
        match result.pointer("/outcome/optionId").and_then(Value::as_str) {
            Some("allow_once") => ApprovalDecision::AllowOnce,
            Some("allow_always") => ApprovalDecision::AllowCategoryForRun,
            _ => ApprovalDecision::Deny,
        }
    }

    /// F06: gate a tool call with the standard ACP `session/request_permission` request. Returns
    /// `None` when the broker is not in ACP mode so the internal `approval/requested` path runs
    /// unchanged.
    fn request_acp_permission(&self, req: &ApprovalRequest) -> Option<ApprovalDecision> {
        let (sink, session_id, owner) = {
            let guard = self.acp.lock().unwrap();
            (guard.sink.clone(), guard.session_id.clone(), guard.owner)
        };
        let (sink, session_id) = (sink?, session_id?);
        let owner = owner?;

        let (arguments_summary, risk_reason) = Self::summarize_request(req);
        let (request_id, timeout) = {
            let mut guard = self.inner.lock().unwrap();
            guard.next_seq += 1;
            let request_id = format!("apr-{:04}", guard.next_seq);
            guard.recorded_events.push(EventPayload::ApprovalRequested {
                request_id: request_id.clone(),
                tool_name: req.tool_name.clone(),
                surface: self.surface.clone(),
            });
            (request_id, guard.timeout)
        };
        let effect_str = match req.effect_class {
            EffectClass::ReadOnly => "read-only",
            EffectClass::Mutating => "mutating",
            EffectClass::SideEffect => "side-effect",
        };
        // P2-1: reuse the kernel tool-call id as the ACP `toolCallId`, so a client can match the
        // permission request against the `session/update` `tool_call` for the same call. Only a
        // request with no kernel call id (the `session.tournament` RPC gate) falls back to the
        // synthetic request number.
        let tool_call_id = req
            .call_id
            .as_ref()
            .map(|id| id.as_str().to_string())
            .unwrap_or_else(|| request_id.clone());
        let tool_kind = super::acp::acp_tool_kind(&req.tool_name);
        let tool_call = json!({
            "toolCallId": tool_call_id.clone(),
            "title": arguments_summary,
            "name": req.tool_name,
            "kind": tool_kind,
            "status": "pending",
            "_meta": { "riskReason": risk_reason, "effectClass": effect_str }
        });
        let options = json!([
            { "optionId": "allow_once", "name": "Allow once", "kind": "allow_once" },
            { "optionId": "allow_always", "name": "Allow for this session", "kind": "allow_always" },
            { "optionId": "reject_once", "name": "Reject once", "kind": "reject_once" },
            { "optionId": "reject_always", "name": "Reject always", "kind": "reject_always" },
        ]);

        let (tx, rx) = std_mpsc::channel::<Value>();
        #[cfg(test)]
        {
            let hook = self.pre_acp_insert_hook.lock().unwrap().take();
            if let Some(h) = hook {
                h();
            }
        }
        // Finding 3: revalidate the owning run under the same `acp` lock before registering the
        // waiter, so a cleanup that landed between the snapshot and the insert cannot leave a ghost
        // entry that is never owner-cleaned. Fail closed if stale.
        {
            let mut guard = self.acp.lock().unwrap();
            if guard.owner != Some(owner) {
                return Some(ApprovalDecision::Deny);
            }
            guard.pending.insert(request_id.clone(), (owner, tx));
        }
        // Revalidate again before emitting the request frame; if the owner was cleaned up since the
        // insert, fail closed instead of leaking a stale permission request.
        if self.acp.lock().unwrap().owner != Some(owner) {
            let mut guard = self.acp.lock().unwrap();
            guard.pending.remove(&request_id);
            return Some(ApprovalDecision::Deny);
        }
        // Announce the pending tool call before asking for permission, so the permission request's
        // `toolCallId` already names a call the client has seen (ACP clients track tool calls by
        // id). The kernel emits `in_progress`/`completed` updates for the same id once it dispatches.
        if req.call_id.is_some() {
            sink(json!({
                "jsonrpc": JSONRPC_VERSION,
                "method": "session/update",
                "params": {
                    "sessionId": session_id.clone(),
                    "update": {
                        "sessionUpdate": "tool_call",
                        "toolCallId": tool_call_id,
                        "title": tool_call.get("title").cloned().unwrap_or(Value::Null),
                        "name": req.tool_name,
                        "kind": tool_kind,
                        "status": "pending",
                    }
                }
            }));
        }
        sink(json!({
            "jsonrpc": JSONRPC_VERSION,
            "id": request_id,
            "method": "session/request_permission",
            "params": { "sessionId": session_id, "toolCall": tool_call, "options": options }
        }));

        let deadline = Instant::now() + timeout;
        let poll_slice = Duration::from_millis(20);
        loop {
            if Instant::now() >= deadline {
                self.acp.lock().unwrap().pending.remove(&request_id);
                self.record_resolved(&request_id, "deny_timeout");
                return Some(ApprovalDecision::Deny);
            }
            match rx.recv_timeout(poll_slice) {
                Ok(result) => {
                    let decision = Self::decision_from_acp_outcome(&result);
                    let decision_name = match decision {
                        ApprovalDecision::AllowOnce => "approve_once",
                        ApprovalDecision::AllowCategoryForRun => "approve_session",
                        ApprovalDecision::Deny => "deny",
                    };
                    self.record_resolved(&request_id, decision_name);
                    return Some(decision);
                }
                Err(std_mpsc::RecvTimeoutError::Disconnected) => {
                    self.record_resolved(&request_id, "deny_disconnect");
                    return Some(ApprovalDecision::Deny);
                }
                Err(std_mpsc::RecvTimeoutError::Timeout) => {}
            }
        }
    }
}

impl ApprovalHandler for RpcApprovalBroker {
    fn is_interactive(&self) -> bool {
        true
    }

    fn request_approval(&self, req: &ApprovalRequest) -> ApprovalDecision {
        if let Some(decision) = self.request_acp_permission(req) {
            return decision;
        }
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

#[cfg(test)]
impl RpcApprovalBroker {
    pub(crate) fn set_pre_acp_insert_hook(&self, hook: Option<Box<dyn Fn() + Send + Sync>>) {
        *self.pre_acp_insert_hook.lock().unwrap() = hook;
    }

    pub(crate) fn acp_pending_count(&self) -> usize {
        self.acp.lock().unwrap().pending.len()
    }

    pub(crate) fn acp_owner(&self) -> Option<u64> {
        self.acp.lock().unwrap().owner
    }

    pub(crate) fn insert_test_pending_acp(
        &self,
        owner: u64,
        request_id: &str,
        tx: std_mpsc::Sender<Value>,
    ) {
        self.acp
            .lock()
            .unwrap()
            .pending
            .insert(request_id.to_string(), (owner, tx));
    }
}

#[cfg(test)]
mod b01_broker_tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// Finding 3: If owner cleanup happens after snapshot but before insert in `request_acp_permission`,
    /// the pre-insert check rejects the insert and returns Deny without resurrecting a ghost waiter,
    /// and without emitting a stale request frame to the sink.
    #[test]
    fn acp_cleanup_before_insert_does_not_resurrect_waiter() {
        let broker = RpcApprovalBroker::new("test");
        broker.set_timeout(Duration::from_millis(10));
        let sink_called = Arc::new(AtomicBool::new(false));
        let sc = Arc::clone(&sink_called);
        broker.enable_acp_permissions_for("s1".into(), Arc::new(move |_| {
            sc.store(true, Ordering::SeqCst);
        }), 42);

        let b = Arc::clone(&broker);
        broker.set_pre_acp_insert_hook(Some(Box::new(move || {
            // Simulate owner 42 disconnect / cleanup occurring after snapshot but before insert
            b.disable_acp_permissions_for(42);
        })));

        let req = ApprovalRequest {
            tool_name: "write".to_string(),
            call_id: None,
            effect_class: EffectClass::Mutating,
            is_sensitive_path: false,
            target_path: Some(PathBuf::from("foo.txt")),
            diff_preview: None,
            command_preview: None,
            workspace_root: PathBuf::from("/ws"),
        };

        let decision = broker.request_approval(&req);
        assert_eq!(decision, ApprovalDecision::Deny);
        assert_eq!(broker.acp_pending_count(), 0, "must not resurrect a ghost waiter");
        assert!(!sink_called.load(Ordering::SeqCst), "stale request frame must not be sent to sink");
    }

    /// Finding 2: `disable_acp_permissions_for(owner)` clears only that owner's state and pending
    /// waiters. A successor run's owner, sink, and pending approvals are completely untouched.
    #[test]
    fn acp_disable_for_stale_owner_does_not_touch_successor() {
        let broker = RpcApprovalBroker::new("test");
        // Owner 1 registers
        broker.enable_acp_permissions_for("s1".into(), Arc::new(|_| ()), 1);
        let (tx1, _rx1) = std_mpsc::channel();
        broker.acp.lock().unwrap().pending.insert("apr-0001".into(), (1, tx1));

        // Successor Owner 2 registers
        broker.enable_acp_permissions_for("s2".into(), Arc::new(|_| ()), 2);
        let (tx2, _rx2) = std_mpsc::channel();
        broker.acp.lock().unwrap().pending.insert("apr-0002".into(), (2, tx2));

        // Stale cleanup for Owner 1
        broker.disable_acp_permissions_for(1);

        // Assert: Owner 2 is still the owner!
        assert_eq!(broker.acp_owner(), Some(2));
        // apr-0001 was drained, but apr-0002 is still pending!
        assert_eq!(broker.acp_pending_count(), 1);

        // Resolving with stale owner 1 must fail
        assert!(broker.resolve_acp_permission_owned(1, "apr-0002", json!({"outcome": "allow_once"})).is_err());
        assert_eq!(broker.acp_pending_count(), 1);

        // Resolving with real owner 2 succeeds
        assert!(broker.resolve_acp_permission_owned(2, "apr-0002", json!({"outcome": "allow_once"})).is_ok());
        assert_eq!(broker.acp_pending_count(), 0);
    }
}
