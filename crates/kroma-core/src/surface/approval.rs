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
