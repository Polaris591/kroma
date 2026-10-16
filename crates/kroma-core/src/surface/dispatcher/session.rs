//! `RpcDispatcher` methods for the session surface.

use super::*;

/// B11a: retires a run's approval grants exactly when the run ends, for every exit path — normal
/// return, cancellation, panic/unwind, or the future being dropped when the daemon shuts down.
/// It holds the same `PolicyEngine` the run used and, in `Drop`, takes only the grant table's own
/// lock; it never holds another lock or runs a callback while unwinding.
struct GrantInvalidationGuard {
    policy_engine: PolicyEngine,
    run_id: RunId,
}

impl Drop for GrantInvalidationGuard {
    fn drop(&mut self) {
        self.policy_engine.invalidate_run(self.run_id);
    }
}

impl RpcDispatcher {
    /// Item #3 (`v1.4.0`): active adaptive-tournament candidate count (`0` = off).
    pub(crate) fn adaptive_tournament_candidates(&self) -> usize {
        *self
            .inner
            .active_adaptive_tournament
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }
    /// B2 (`v1.4.0`): active context projection profile (`None` = default `P2`).
    pub(crate) fn active_projection_profile(
        &self,
    ) -> Option<crate::context_projector::ProjectionProfile> {
        *self
            .inner
            .active_projection_profile
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }
    /// F05 (`v1.5.1`): the live run-tree token cap, hot-swapped by `/config set max_tokens`.
    pub(crate) fn active_max_tokens(&self) -> Option<u64> {
        *self
            .inner
            .active_max_tokens
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }
    /// B01: bind the run that is about to start to the connection that dispatched it, creating or
    /// adopting the run's cancel channel in the same critical section. Returns `None` when the
    /// connection already closed, so the caller must not start a run whose owner is gone. Owner and
    /// channel are written together under one lock, so a disconnect can never observe an owner
    /// without a channel to cancel.
    pub(crate) fn claim_run(
        &self,
        conn: Option<&ConnectionContext>,
        request: Option<&RequestCancel>,
    ) -> Option<RunClaim> {
        let acp_session = match conn {
            Some(c) if c.acp_sink.is_some() => Some(self.session_id()),
            _ => None,
        };
        let claim = {
            let mut guard = self
                .inner
                .active_run
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if let Some(conn) = conn {
                if !conn.alive.load(Ordering::SeqCst) {
                    return None;
                }
            }
            let run_id = self.inner.next_run_id.fetch_add(1, Ordering::SeqCst);
            let owner = conn.map(|c| c.id);
            // B01: adopt the request's cancel capability when it came from a stream, so a cancel
            // ordered before this claim is already observed. Direct callers get a fresh channel.
            let (tx, rx) = match request {
                Some(request) => (request.tx.clone(), request.rx.clone()),
                None => watch::channel(false),
            };
            *guard = Some(ActiveRun {
                run_id,
                owner,
                tx,
                rx: rx.clone(),
            });
            // Finding 1: a successful claim always reopens the broker, so an owner EOF that closed
            // it cannot permanently fail-closed every later run (non-ACP included). Serialised with
            // the owner's close/cleanup by the `active_run` lock.
            self.inner.approval_broker.reopen();
            // Enable the ACP sink inside the ownership critical section so a new claim and an old
            // run's cleanup can never interleave. Tagged with this run's id.
            let acp_owner = match (acp_session, conn) {
                (Some(session_id), Some(conn)) => match &conn.acp_sink {
                    Some(sink) => {
                        self.inner.approval_broker.enable_acp_permissions_for(
                            session_id,
                            Arc::clone(sink),
                            run_id,
                        );
                        Some(run_id)
                    }
                    None => None,
                },
                _ => None,
            };
            RunClaim {
                run_id,
                cancel_rx: rx,
                acp_owner,
            }
        };
        Some(claim)
    }

    #[cfg(test)]
    pub(crate) fn set_pre_claim_gate(&self, gate: Option<Arc<PreClaimGate>>) {
        *self
            .inner
            .pre_claim_gate
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = gate;
    }

    /// B01: mark a daemon connection dead and cancel the run it owns, if any. Returns the owned
    /// run's id so the caller can tear down exactly that run's approval channel; an observer
    /// connection returns `None` and changes nothing.
    ///
    /// Finding 2: the owner check, cancel, broker close and ACP disable all run inside the same
    /// `active_run` critical section as `claim_run`/`RunGuard`, so a stale owner can never wipe a
    /// successor run's approvals. Lock order is always `active_run` -> broker (`inner`/`acp`); the
    /// broker never takes `active_run`, so there is no cycle.
    pub(crate) fn end_connection_run(&self, conn: &ConnectionContext) -> Option<u64> {
        let guard = self
            .inner
            .active_run
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        conn.alive.store(false, Ordering::SeqCst);
        match guard.as_ref() {
            Some(run) if run.owner == Some(conn.id) => {
                let run_id = run.run_id;
                let _ = run.tx.send(true);
                self.inner.approval_broker.close();
                self.inner
                    .approval_broker
                    .disable_acp_permissions_for(run_id);
                Some(run_id)
            }
            _ => None,
        }
    }

    /// Finding 4: resolve an ACP permission response only when `conn` owns the exact active run,
    /// and let the broker re-check the pending entry's owner. The request id is not authority.
    pub(crate) fn resolve_acp_response_from(
        &self,
        conn: &ConnectionContext,
        request_id: &str,
        result: Value,
    ) -> Result<(), String> {
        let guard = self
            .inner
            .active_run
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let run_id = match guard.as_ref() {
            Some(run) if run.owner == Some(conn.id) => run.run_id,
            _ => return Err("no active run owned by this connection".to_string()),
        };
        self.inner
            .approval_broker
            .resolve_acp_permission_owned(run_id, request_id, result)
    }

    pub(crate) fn ensure_active_cancel_receiver(&self) -> watch::Receiver<bool> {
        let mut guard = self
            .inner
            .active_run
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(run) = guard.as_ref() {
            return run.rx.clone();
        }
        let run_id = self.inner.next_run_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = watch::channel(false);
        *guard = Some(ActiveRun {
            run_id,
            owner: None,
            tx,
            rx: rx.clone(),
        });
        rx
    }
    pub(crate) async fn rpc_session_new(&self, params: &Value) -> Result<Value, JsonRpcError> {
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
        let previous_session_id = self.session_id();
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
                    format!(
                        "failed to resolve session WAL {}: {}",
                        raw_path.display(),
                        e
                    ),
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
            // F10: a failed reconciliation fails the resume instead of silently continuing from a
            // WAL that may disagree with the recorded workspace history.
            self.reconcile_journal(&mut j)?;
            j
        } else {
            SessionJournal::create(sessions_dir, self.inner.env.workspace().root()).map_err(
                |e| {
                    JsonRpcError::new(
                        JSONRPC_INTERNAL_ERROR,
                        format!("failed to create session WAL: {}", e),
                    )
                },
            )?
        };
        let next_snap = Self::snapshot_from_journal(&journal);
        drop(journal);
        let session_id = next_snap.session_id.clone();
        let session_path = next_snap.session_path.clone();
        let turn_count = next_snap.turn_count;
        let latest_rev = next_snap.latest_revision_id.clone();
        *self.inner.status.lock().unwrap() = next_snap;
        self.register_owned_session(&session_id);
        if previous_session_id != session_id {
            crate::session_ownership::unregister_session(&previous_session_id);
        }
        // The host outlives the session it was built with. Point *new* admissions at the session
        // this runtime now owns; tasks already admitted keep the context captured at their own
        // admission, so switching sessions can never redirect an existing task's WAL writes.
        self.inner.durable_tasks.rebind_session(
            session_id.clone(),
            self.inner.dispatcher_id.clone(),
            session_path,
        );

        Ok(json!({
            "session_id": session_id,
            "workspace_root": self.inner.env.workspace().root().display().to_string(),
            "policy_mode": self.inner.policy_engine.mode().as_str(),
            "turn_count": turn_count,
            "latest_revision_id": latest_rev,
        }))
    }
    pub(crate) async fn rpc_session_prompt(&self, params: &Value) -> Result<Value, JsonRpcError> {
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
        self.run_session_prompt(prompt_text, params).await
    }
    /// Shared prompt-turn core: used by the internal `session.prompt` / `acp.prompt` methods and by
    /// the ACP `session/prompt` adapter (F06). Returns the internal result object; each caller maps
    /// it to its own response shape (`stopReason` for ACP).
    pub(crate) async fn run_session_prompt(
        &self,
        prompt_text: String,
        params: &Value,
    ) -> Result<Value, JsonRpcError> {
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
        // Session routing/ownership: reject a misrouted request and claim the workspace's write
        // side for the whole run so two runtimes can never interleave writes into one tree.
        self.validate_session_param(params)?;
        let _writer_lease = self.acquire_writer_lease()?;
        self.ensure_no_pending_restore()?;
        if self.inner.in_flight.swap(true, Ordering::SeqCst) {
            return Err(JsonRpcError::new(
                JSONRPC_INTERNAL_ERROR,
                "a session prompt is already in flight",
            ));
        }
        // B01 (test only): let a unit test hold the run here so cancel-before-claim ordering is
        // deterministic. Compiled only in test builds.
        #[cfg(test)]
        {
            let gate = self
                .inner
                .pre_claim_gate
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            if let Some(gate) = gate {
                gate.hold().await;
            }
        }
        // B01: claim this run's ownership slot only after the in-flight gate is held, so a busy or
        // refused request never becomes the owner. A connection that closed while this run was
        // starting is refused instead of leaking a run past its owner. The request's cancel
        // capability (when it came from a stream) is adopted atomically.
        let conn = CONNECTION_CONTEXT.try_with(|c| c.clone()).ok();
        let request = RUN_REQUEST.try_with(|r| r.clone()).ok();
        let Some(claim) = self.claim_run(conn.as_ref(), request.as_ref()) else {
            self.inner.in_flight.store(false, Ordering::SeqCst);
            return Err(JsonRpcError::new(
                JSONRPC_INTERNAL_ERROR,
                "the connection that started this run closed before it began",
            ));
        };
        let _in_flight_guard = RunGuard {
            inner: Arc::clone(&self.inner),
            run_id: claim.run_id,
            in_flight: true,
            acp_owner: claim.acp_owner,
        };
        let cancel_rx = claim.cancel_rx;

        let session_path = self.session_path();
        let mut journal = SessionJournal::open(&session_path).map_err(|e| {
            JsonRpcError::new(
                JSONRPC_INTERNAL_ERROR,
                format!(
                    "failed to open session WAL {}: {}",
                    session_path.display(),
                    e
                ),
            )
        })?;
        // F10: a failed reconciliation aborts the run instead of mutating a workspace whose WAL and
        // snapshot history could not be proven consistent.
        self.reconcile_journal(&mut journal)?;

        let mut prompt_builder =
            PromptBuilder::default().with_workspace_context(self.inner.env.workspace());
        let activated_skills = self.inner.status.lock().unwrap().activated_skills.clone();
        for skill_name in &activated_skills {
            if let Ok(pb) = prompt_builder
                .clone()
                .with_activated_skill(self.inner.env.workspace(), skill_name)
            {
                prompt_builder = pb;
            }
        }

        let mut subtree_tracker =
            SubtreeContextTracker::new(self.inner.config.subtree_context_enabled);
        if let Some(cp) = journal.latest_active_checkpoint() {
            subtree_tracker.restore_loaded_dirs(cp.subtree_dirs.clone());
        }

        // F05 (`v1.5.1`): a per-request `max_tokens` override lets an RPC client set the same
        // unified token cap the CLI/config expose, without mutating the daemon's default budget.
        // The runtime's live cap (`/config set max_tokens`) is the default for every request, so a
        // hot-swap applies here without a provider rebuild.
        let mut run_budget = self.inner.config.budget.clone();
        run_budget.max_total_tokens = self.active_max_tokens();
        if let Some(max_tokens) = params.get("max_tokens").and_then(Value::as_u64) {
            if max_tokens == 0 {
                return Err(JsonRpcError::new(
                    JSONRPC_INVALID_PARAMS,
                    "'max_tokens' must be > 0",
                ));
            }
            run_budget.max_total_tokens = Some(max_tokens);
        }
        let mut run_cfg = AgentRunConfig::new(prompt_text, run_budget.clone());
        run_cfg.stream = self.inner.config.stream;
        run_cfg.strict_log = self.inner.config.strict_log;
        // B2 (`v1.4.0`): apply the configured context projection profile.
        run_cfg.projection_profile = self.active_projection_profile();
        // Capability gap #6 (`v1.5.2`): on-demand tool schemas (off by default).
        run_cfg.on_demand_tools = self.inner.config.on_demand_tools;
        // Capability gap #3 (`v1.5.2`): causal pruning of dead-end context (off by default).
        run_cfg.causal_pruning = self.inner.config.causal_pruning;
        // Capability gap #4 (`v1.5.2`): record every spawned child as a durable task.
        run_cfg.durable_tasks = Some(Arc::clone(&self.inner.durable_tasks));

        let mut logger = EventLogger::new(
            run_cfg.run_id,
            Some(self.inner.config.state_dir.clone()),
            self.inner.config.strict_log,
        )
        .map_err(|e| JsonRpcError::new(JSONRPC_INTERNAL_ERROR, e.to_string()))?;
        logger.set_live_stderr(false);

        let session_id_for_sink = journal.session_id().to_string();
        let inner_for_sink = Arc::clone(&self.inner);
        // F06: on the ACP surface, translate diagnostics into standard `session/update`
        // notifications; every other surface keeps the internal `kroma/event` stream.
        let acp_protocol = self.inner.config.acp_protocol;
        let workspace_root_for_sink = self.inner.env.workspace().root().to_path_buf();
        logger.set_event_sink(Arc::new(move |ev: &DiagnosticEvent| {
            if acp_protocol {
                if let Some(update) =
                    crate::surface::acp::acp_update_for_event(ev, &workspace_root_for_sink)
                {
                    Self::broadcast_to_subscribers(
                        &inner_for_sink,
                        crate::surface::acp::acp_session_update(&session_id_for_sink, update),
                    );
                }
                return;
            }
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
            run_budget,
        )
        .with_predictive_check(self.inner.config.predictive_check);

        // B11a: register the grant-retirement guard before the run starts, so every exit path
        // (normal return, cancellation, panic/unwind, or a dropped future) invalidates exactly this
        // run's grants. The run id is a fresh UUID, so a successor run's grants are never touched.
        let _grant_guard = GrantInvalidationGuard {
            policy_engine: self.inner.policy_engine.clone(),
            run_id: run_cfg.run_id,
        };
        let runner = AgentRunner::new(
            run_cfg,
            Arc::clone(
                &self
                    .inner
                    .provider
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()),
            ),
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
        .with_adaptive_tournament_candidates(self.adaptive_tournament_candidates())
        // Capability gap #5 (`v1.5.2`): strict verification policy for a repair race.
        .with_tournament_require_verified(self.inner.config.tournament_require_verified);

        // Capability gap #2 (`v1.5.2`): opt-in isolated counterfactual probe. ENABLING IT IS THE
        // OPERATOR'S CONSENT (the `--counterfactual-probe` flag / runtime config), so the approval
        // seam allows each bounded probe; without the flag the default `DenyProbes` applies.
        let mut runner = runner;
        if self.inner.config.counterfactual_probe {
            runner = runner.with_counterfactual_probe(
                crate::attribution::CounterfactualProbeConfig::enabled_with(2),
                Arc::new(crate::attribution::AllowProbes),
            );
        }

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
    pub(crate) async fn rpc_approval_pending(&self) -> Result<Value, JsonRpcError> {
        let pending = self.inner.approval_broker.pending_requests();
        Ok(json!({
            "pending": pending,
        }))
    }
    pub(crate) async fn rpc_approval_respond(&self, params: &Value) -> Result<Value, JsonRpcError> {
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
    pub(crate) async fn rpc_session_cancel(&self) -> Result<Value, JsonRpcError> {
        let had_active = {
            let guard = self.inner.active_run.lock().unwrap();
            if let Some(run) = guard.as_ref() {
                let _ = run.tx.send(true);
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
    /// B11: remote `session.cancel` from a daemon stream. Admitted only for the connection that
    /// owns the *exact* current active run, or for a connection cancelling its own not-yet-claimed
    /// request. The owner check, the run's cancel signal, and its approval cleanup all run in one
    /// `active_run` critical section, so the run cannot switch between deciding and acting and a
    /// previous owner can never stop a successor run. B01 lock order is preserved:
    /// `active_run` -> broker.
    ///
    /// A refused cancel is a pure no-op: it does not deny approvals, signal waiters, or cancel any
    /// token. An absent active run is *not* treated as ownership; only this connection's own
    /// registered, unclaimed request may still be cancelled, and that path never touches another
    /// run.
    pub(crate) fn rpc_session_cancel_from(
        &self,
        conn: &ConnectionContext,
    ) -> Result<Value, JsonRpcError> {
        let session_id = self.session_id();
        let owned_active = {
            let guard = self
                .inner
                .active_run
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            match guard.as_ref() {
                Some(run) if run.owner == Some(conn.id) => {
                    let _ = run.tx.send(true);
                    // While `active_run` is held no successor run can claim, and approvals only
                    // exist inside a claimed run, so every pending approval here belongs to the run
                    // being cancelled -- never to a successor waiter.
                    self.inner.approval_broker.cancel_all_pending();
                    Some(true)
                }
                Some(_) => Some(false),
                None => None,
            }
        };
        match owned_active {
            Some(true) => Ok(json!({
                "session_id": session_id,
                "cancelled": true,
                "was_in_flight": true,
            })),
            Some(false) => Err(Self::not_owner_error(&session_id)),
            None => {
                // No active run to own: admit only this connection's own not-yet-claimed request,
                // whose cancel channel is registered in this connection's private pending table.
                if conn.signal_pending(&session_id) {
                    Ok(json!({
                        "session_id": session_id,
                        "cancelled": true,
                        "was_in_flight": false,
                    }))
                } else {
                    Err(Self::not_owner_error(&session_id))
                }
            }
        }
    }
    /// B11: the stable refusal for a cancel from a connection that does not own the active run.
    fn not_owner_error(session_id: &str) -> JsonRpcError {
        JsonRpcError::new(
            JSONRPC_SESSION_NOT_OWNED,
            format!(
                "not_owner: this connection does not own the active run on session '{}'",
                session_id
            ),
        )
    }
    pub(crate) async fn rpc_session_status(&self) -> Result<Value, JsonRpcError> {
        // F10: status is read-only, so a failed reconciliation degrades the response with a reason
        // instead of failing it, while every mutation entry point refuses to proceed.
        let mut recovery_error: Option<String> = None;
        if let Ok(_guard) = self.inner.op_lock.try_lock() {
            let path = self.session_path();
            match SessionJournal::open(&path) {
                Ok(mut journal) => match self.inner.snapshot_store.reconcile_with_wal(&mut journal)
                {
                    Ok(_) => self.update_snapshot_from_journal(&journal),
                    Err(e) => recovery_error = Some(format!("{}", e)),
                },
                Err(e) => recovery_error = Some(format!("failed to open session WAL: {}", e)),
            }
        }
        let snap = self.inner.status.lock().unwrap().clone();
        let pending = self.inner.approval_broker.pending_requests();
        let cache_hit_ratio = if snap.total_input_tokens > 0 {
            snap.total_cached_input_tokens as f64 / snap.total_input_tokens as f64
        } else {
            0.0
        };
        // P3 (`v1.5.0`): report what the resolved completion gate will actually do, so a terminal
        // surface can state the truth instead of assuming verification is armed.
        let gate = self.inner.config.completion_gate.status();
        let gate_command = gate
            .command()
            .map(|argv| argv.join(" "))
            .unwrap_or_default();
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
            "completion_gate": {
                "armed": gate.armed(),
                "kind": gate.kind(),
                "command": gate_command,
            },
            "recovery": {
                "degraded": recovery_error.is_some(),
                "reason": recovery_error.unwrap_or_default(),
            },
        }))
    }

    /// P2 (`v1.5.0`): cancel the run currently in flight on this dispatcher, if any.
    ///
    /// Signals the same `active_cancel` watch the run's provider and tool calls observe, and denies
    /// any pending approval, so an interactive `^C` aborts a stuck turn instead of leaking it until
    /// the provider budget expires. Returns `true` when a run was in flight.
    pub fn cancel_active_run(&self) -> bool {
        let signalled = {
            let guard = self.inner.active_run.lock().unwrap();
            match guard.as_ref() {
                Some(run) => run.tx.send(true).is_ok(),
                None => false,
            }
        };
        self.inner.approval_broker.cancel_all_pending();
        signalled
    }
    /// `session.fork`: branch the session transcript at `turn` and, by default, restore the
    /// workspace to the revision that was current at that turn.
    ///
    /// * `mode: "workspace"` (default) — restore first, then move the leaf. External edits make the
    ///   fork fail with `JSONRPC_RESTORE_CONFLICT` and nothing is mutated; an interrupted restore
    ///   makes it fail with `JSONRPC_RECOVERY_REQUIRED`.
    /// * `mode: "conversation"` — branch the transcript only, exactly the historical behaviour.
    /// * `force: true` overwrites conflicting live paths (workspace mode only).
    pub(crate) async fn rpc_session_fork(&self, params: &Value) -> Result<Value, JsonRpcError> {
        let turn = params.get("turn").and_then(Value::as_u64).ok_or_else(|| {
            JsonRpcError::new(
                JSONRPC_INVALID_PARAMS,
                "missing non-negative integer 'turn' parameter for session.fork",
            )
        })? as usize;
        let mode = params
            .get("mode")
            .and_then(Value::as_str)
            .map(|s| s.trim().to_ascii_lowercase())
            .unwrap_or_else(|| "workspace".to_string());
        let conversation_only = match mode.as_str() {
            "workspace" | "workspace_bound" | "workspace-bound" => false,
            "conversation" | "conversation_only" | "conversation-only" | "chat" => true,
            other => {
                return Err(JsonRpcError::new(
                    JSONRPC_INVALID_PARAMS,
                    format!(
                        "unknown session.fork mode '{}'; expected 'workspace' or 'conversation'",
                        other
                    ),
                ))
            }
        };
        let force = params
            .get("force")
            .and_then(Value::as_bool)
            .unwrap_or(false);

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
        self.validate_session_param(params)?;
        let _writer_lease = self.acquire_writer_lease()?;
        self.ensure_no_pending_restore()?;
        let path = self.session_path();
        let mut journal = SessionJournal::open(&path).map_err(|e| {
            JsonRpcError::new(
                JSONRPC_INTERNAL_ERROR,
                format!("failed to open session WAL: {}", e),
            )
        })?;
        self.reconcile_journal(&mut journal)?;

        if conversation_only {
            let leaf_id = journal.fork_at_turn(turn).map_err(|e| {
                JsonRpcError::new(
                    JSONRPC_INVALID_PARAMS,
                    format!("failed to fork session at turn {}: {}", turn, e),
                )
            })?;
            self.update_snapshot_from_journal(&journal);
            let snap = self.inner.status.lock().unwrap().clone();
            return Ok(json!({
                "session_id": snap.session_id,
                "mode": "conversation",
                "restored_workspace": false,
                "forked_at_turn": turn,
                "turn_count": snap.turn_count,
                "leaf_id": leaf_id,
                "latest_revision_id": snap.latest_revision_id,
            }));
        }

        let outcome = self
            .inner
            .snapshot_store
            .fork_workspace_at_turn(self.inner.env.workspace(), &mut journal, turn, force)
            .map_err(Self::fork_error_to_rpc)?;
        self.update_snapshot_from_journal(&journal);
        let snap = self.inner.status.lock().unwrap().clone();
        Ok(json!({
            "session_id": snap.session_id,
            "mode": "workspace",
            "restored_workspace": true,
            "forked_at_turn": outcome.forked_at_turn,
            "turn_count": outcome.turn_count,
            "leaf_id": outcome.leaf_id,
            "restored_revision": outcome.target_revision.as_str(),
            "restored_files": outcome.restored_files,
            "removed_files": outcome.removed_files,
            "latest_revision_id": outcome.latest_revision_id.or(snap.latest_revision_id),
        }))
    }
    fn fork_error_to_rpc(err: crate::revision::WorkspaceBoundForkError) -> JsonRpcError {
        use crate::revision::WorkspaceBoundForkError as E;
        match err {
            E::NoEntryAtTurn(turn) => JsonRpcError::new(
                JSONRPC_INVALID_PARAMS,
                format!("no session entry at or before turn {}", turn),
            ),
            E::NoRevisionAtTurn(turn) => JsonRpcError::new(
                JSONRPC_INVALID_PARAMS,
                format!(
                    "no workspace revision recorded at or before turn {}; use mode 'conversation' to branch without restoring files",
                    turn
                ),
            ),
            E::Conflict {
                turn,
                target_revision,
                conflicting_paths,
            } => {
                let conflicts: Vec<Value> = conflicting_paths
                    .iter()
                    .map(|c| json!({ "path": c.path, "kind": format!("{:?}", c.kind) }))
                    .collect();
                JsonRpcError::with_data(
                    JSONRPC_RESTORE_CONFLICT,
                    format!(
                        "refusing to fork to turn {}; {} path(s) modified externally (pass force=true to overwrite)",
                        turn,
                        conflicts.len()
                    ),
                    json!({
                        "turn": turn,
                        "target_revision": target_revision.as_str(),
                        "conflicts": conflicts,
                    }),
                )
            }
            E::RecoveryRequired {
                turn,
                target_revision,
                journal_path,
                reason,
            } => JsonRpcError::with_data(
                JSONRPC_RECOVERY_REQUIRED,
                format!(
                    "fork to turn {} requires recovery of {}: {}",
                    turn, target_revision, reason
                ),
                json!({
                    "turn": turn,
                    "target_revision": target_revision.as_str(),
                    "intent_journal": journal_path.display().to_string(),
                }),
            ),
            E::Revision(e) => JsonRpcError::new(JSONRPC_INTERNAL_ERROR, e.to_string()),
            E::Session(e) => JsonRpcError::new(JSONRPC_INTERNAL_ERROR, e.to_string()),
        }
    }
    /// `sessions.list`: recent session WALs in the state directory, newest first.
    pub(crate) async fn rpc_sessions_list(&self) -> Result<Value, JsonRpcError> {
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
                Ok(journal) => (journal.active_transcript().len(), journal.max_active_turn()),
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
    pub(crate) async fn rpc_session_resume(&self, params: &Value) -> Result<Value, JsonRpcError> {
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
    pub(crate) async fn rpc_shutdown(&self) -> Result<Value, JsonRpcError> {
        self.inner.shutting_down.store(true, Ordering::SeqCst);
        let _ = self.rpc_session_cancel().await;
        // Unmounting runs each extension's teardown (which bounds and reaps an MCP worker/child).
        // The report is surfaced instead of discarded so a cancelled-but-not-drained call or an
        // irreversible side effect is not silently lost.
        let mut unmounted = Vec::new();
        for ext_id in &self.inner.mounted_extensions {
            match self.inner.registry.unregister_extension(ext_id) {
                Ok(report) => unmounted.push(json!({
                    "extension_id": report.extension_id.0,
                    "cancelled_in_flight": report.cancelled_in_flight,
                    "drained_in_flight": report.drained_in_flight,
                    "irreversible_side_effects": report.irreversible_side_effects,
                })),
                Err(e) => {
                    eprintln!(
                        "[kroma] failed to unmount extension '{}' during shutdown: {}",
                        ext_id.0, e
                    );
                    unmounted.push(json!({ "extension_id": ext_id.0, "error": e.to_string() }));
                }
            }
        }
        crate::session_ownership::unregister_session(&self.session_id());
        Ok(json!({
            "shutdown": true,
            "session_id": self.session_id(),
            "unmounted": unmounted,
        }))
    }

    /// Capability gap #4 (`v1.5.2`): `tasks.list` returns the durable async-task table, optionally
    /// filtered to one session id. It is a read-only view over the persistent registry.
    pub(crate) async fn rpc_tasks_list(&self, params: &Value) -> Result<Value, JsonRpcError> {
        let session_id = params
            .get("session_id")
            .and_then(Value::as_str)
            .map(str::to_string);
        let mut registry = self
            .inner
            .async_tasks
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        registry
            .apply_control(&crate::async_tasks::TaskControlRequest::List { session_id })
            .map_err(|e| JsonRpcError::new(JSONRPC_INTERNAL_ERROR, e.to_string()))
    }

    /// Capability gap #4 (`v1.5.2`): `tasks.control` is the supervisor control interface
    /// (pause / resume / cancel / challenge / resolve_challenge / acknowledge) over durable tasks.
    ///
    /// `cancel` and `resume` are *executable*: they act on the live subagent (fire its cancellation
    /// channel / re-run it from the persisted prompt), not just on the durable record.
    pub(crate) async fn rpc_tasks_control(&self, params: &Value) -> Result<Value, JsonRpcError> {
        let request: crate::async_tasks::TaskControlRequest =
            serde_json::from_value(params.clone()).map_err(|e| {
                JsonRpcError::new(
                    JSONRPC_INVALID_PARAMS,
                    format!("invalid task control request: {}", e),
                )
            })?;
        match &request {
            crate::async_tasks::TaskControlRequest::Cancel { task_id, .. } => {
                let signalled = self
                    .inner
                    .durable_tasks
                    .cancel_task(task_id)
                    .map_err(|e| JsonRpcError::new(JSONRPC_INVALID_PARAMS, e))?;
                // Report the state the record actually holds, not an assumed one: a terminal task is
                // left as-is, and a failed durable cancel is an error above rather than a fake
                // "cancelled".
                let state = self
                    .inner
                    .async_tasks
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .state_of(task_id)
                    .map(crate::async_tasks::TaskState::as_str)
                    .unwrap_or("unknown");
                return Ok(json!({
                    "task_id": task_id,
                    "state": state,
                    "live_run_signalled": signalled,
                }));
            }
            crate::async_tasks::TaskControlRequest::Resume { task_id, .. } => {
                self.inner
                    .durable_tasks
                    .resume_task(task_id)
                    .map_err(|e| JsonRpcError::new(JSONRPC_INVALID_PARAMS, e))?;
                return Ok(json!({
                    "task_id": task_id,
                    "state": crate::async_tasks::TaskState::Running.as_str(),
                    "resumed": true,
                }));
            }
            _ => {}
        }
        let mut registry = self
            .inner
            .async_tasks
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        registry.apply_control(&request).map_err(|e| match e {
            crate::async_tasks::TaskRegistryError::UnknownTask(_)
            | crate::async_tasks::TaskRegistryError::InvalidTransition { .. }
            | crate::async_tasks::TaskRegistryError::NoOpenChallenge(_)
            | crate::async_tasks::TaskRegistryError::DuplicateTask(_)
            | crate::async_tasks::TaskRegistryError::AcceptanceRefused(_)
            | crate::async_tasks::TaskRegistryError::NoUnknownOutcome(_)
            | crate::async_tasks::TaskRegistryError::InvalidReconcile(_)
            | crate::async_tasks::TaskRegistryError::AlreadySuperseded { .. } => {
                JsonRpcError::new(JSONRPC_INVALID_PARAMS, e.to_string())
            }
            other => JsonRpcError::new(JSONRPC_INTERNAL_ERROR, other.to_string()),
        })
    }
}

#[cfg(test)]
mod b01_ownership_tests {
    use super::*;
    use crate::provider::FakeProvider;
    use crate::tool::policy::PolicyMode;

    fn test_dispatcher() -> RpcDispatcher {
        let root = std::env::temp_dir().join(format!("kroma-b01-unit-{}", Uuid::new_v4()));
        let ws = root.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let cfg = KromaRuntimeConfig::new(&ws, root.join("state"), PolicyMode::AllowWrite);
        RpcDispatcher::new(cfg, Arc::new(FakeProvider::new(vec![]))).unwrap()
    }

    fn prompt_request(id: i64) -> JsonRpcRequest {
        serde_json::from_value(json!({
            "jsonrpc": "2.0", "id": id, "method": "session.prompt",
            "params": { "prompt": "x" }
        }))
        .unwrap()
    }

    /// Deterministic: a cancel processed before the run claims is still observed by the run.
    #[tokio::test]
    async fn cancel_queued_before_claim_cancels_the_run() {
        let dispatcher = test_dispatcher();
        let gate = Arc::new(PreClaimGate::new());
        dispatcher.set_pre_claim_gate(Some(Arc::clone(&gate)));
        let conn = ConnectionContext::new(None);
        let request = conn.begin_request("s").unwrap();
        let guard = RequestGuard::new(conn.clone(), request.seq);
        let d = dispatcher.clone();
        let handle = tokio::spawn(CONNECTION_CONTEXT.scope(
            conn.clone(),
            RUN_REQUEST.scope(request, async move {
                let _guard = guard;
                d.handle_request(prompt_request(1)).await
            }),
        ));

        gate.wait_reached().await; // run is held after the in-flight gate, before claim
        assert!(conn.signal_pending("s")); // explicit cancel arrives here
        gate.release();

        let resp = serde_json::to_value(handle.await.unwrap()).unwrap();
        assert_eq!(resp["result"]["status"], json!("cancelled"), "{resp}");
    }

    /// A cancel signals only requests registered for that session before the cancel.
    #[test]
    fn cancel_targets_only_requests_registered_before_it() {
        let conn = ConnectionContext::new(None);
        let first = conn.begin_request("a").unwrap();
        assert!(conn.signal_pending("a"));
        assert!(*first.rx.borrow(), "registered request must be cancelled");
        // A request registered after the cancel is fresh, not poisoned by the earlier marker.
        let later = conn.begin_request("a").unwrap();
        assert!(!*later.rx.borrow(), "post-cancel request must be fresh");
        assert!(!conn.signal_pending("b"), "other sessions are untouched");
    }

    /// A busy/retired request removes only its own entry; a successor is untouched.
    #[test]
    fn retiring_one_request_does_not_remove_a_successor() {
        let conn = ConnectionContext::new(None);
        let first = conn.begin_request("a").unwrap();
        let second = conn.begin_request("a").unwrap();
        conn.retire_request(first.seq);
        assert!(conn.signal_pending("a"));
        assert!(!*first.rx.borrow(), "retired request is gone");
        assert!(*second.rx.borrow(), "successor must still be cancelled");
    }

    /// Past the admission cap the request is refused without claiming an owner.
    #[test]
    fn admission_cap_busy_rejects_without_claiming() {
        let conn = ConnectionContext::new(None);
        for _ in 0..super::MAX_PENDING_REQUESTS_PER_CONNECTION {
            assert!(conn.begin_request("a").is_some());
        }
        assert!(
            conn.begin_request("a").is_none(),
            "past the cap the request must be refused"
        );
    }

    /// Finding 4: resolve_acp_response_from refuses any connection that does not own the active
    /// run, and succeeds only for the owning connection.
    #[test]
    fn acp_response_refuses_wrong_owner_connection() {
        let dispatcher = test_dispatcher();
        let owner_conn = ConnectionContext::new(None);
        let stranger_conn = ConnectionContext::new(None);

        let claim = dispatcher.claim_run(Some(&owner_conn), None).unwrap();
        let (tx, rx) = std_mpsc::channel();
        dispatcher.inner.approval_broker.enable_acp_permissions_for(
            "s1".to_string(),
            Arc::new(|_| ()),
            claim.run_id,
        );
        dispatcher.inner.approval_broker.insert_test_pending_acp(
            claim.run_id,
            "apr-0001",
            tx,
        );

        // Wrong connection (stranger / non-owner): refused with specific error
        let err = dispatcher.resolve_acp_response_from(
            &stranger_conn,
            "apr-0001",
            json!({ "outcome": { "outcome": "selected", "optionId": "allow_once" } }),
        );
        assert_eq!(
            err,
            Err("no active run owned by this connection".to_string())
        );
        assert!(rx.try_recv().is_err(), "waiter must not be resolved by stranger");

        // True owner connection: accepted
        let ok = dispatcher.resolve_acp_response_from(
            &owner_conn,
            "apr-0001",
            json!({ "outcome": { "outcome": "selected", "optionId": "allow_once" } }),
        );
        assert!(ok.is_ok(), "owner must succeed: {:?}", ok);
        assert!(rx.try_recv().is_ok(), "waiter must receive approval");
    }

    /// Finding 4: resolve_acp_response_from refuses when there is no active run.
    #[test]
    fn acp_response_refuses_when_no_active_run() {
        let dispatcher = test_dispatcher();
        let conn = ConnectionContext::new(None);
        let err = dispatcher.resolve_acp_response_from(
            &conn,
            "apr-0001",
            json!({ "outcome": { "outcome": "selected", "optionId": "allow_once" } }),
        );
        assert_eq!(
            err,
            Err("no active run owned by this connection".to_string())
        );
    }
}

#[cfg(test)]
mod b11a_grant_guard_tests {
    use super::*;
    use crate::tool::policy::{AuthEntry, AuthScope};
    use std::time::Duration;

    struct AllowCategory;

    impl ApprovalHandler for AllowCategory {
        fn request_approval(&self, _req: &ApprovalRequest) -> ApprovalDecision {
            ApprovalDecision::AllowCategoryForRun
        }

        fn is_interactive(&self) -> bool {
            true
        }
    }

    /// F2: the run-scoped grant guard retires the run's grants on drop, so a panic/unwind or a
    /// dropped run future cannot leave an approval usable past the run (until TTL/cap eviction).
    #[test]
    fn dropping_the_guard_retires_the_runs_grants() {
        let engine = PolicyEngine::new(PolicyMode::Ask, Arc::new(AllowCategory));
        let run_id = RunId::new();
        let workspace_root = std::path::PathBuf::from("/ws");
        let scope = AuthScope {
            entry: AuthEntry::Tool,
            tool_name: "write".to_string(),
            run_id: Some(run_id),
            workspace_root: workspace_root.clone(),
            // B12a: a reusable category grant requires a real revision identity.
            revision: Some(crate::revision::RevisionId::new(
                "r-00000000000000000000000000000000",
            )),
        };
        let req = ApprovalRequest {
            tool_name: "write".to_string(),
            call_id: None,
            effect_class: EffectClass::Mutating,
            is_sensitive_path: false,
            target_path: None,
            diff_preview: None,
            command_preview: None,
            workspace_root,
        };

        let decision =
            engine.prompt_approval_scoped(&scope, &req, Some("d"), Some(Duration::from_secs(60)));
        assert_eq!(decision, ApprovalDecision::AllowCategoryForRun);
        assert_eq!(
            engine.evaluate_with_context(&scope, EffectClass::Mutating, false),
            PolicyCheckResult::Allow
        );

        // A panic/unwind or dropped future drops the guard, which retires exactly this run's grants.
        {
            let _guard = GrantInvalidationGuard {
                policy_engine: engine.clone(),
                run_id,
            };
        }
        assert_eq!(
            engine.evaluate_with_context(&scope, EffectClass::Mutating, false),
            PolicyCheckResult::Ask,
            "the guard's Drop must invalidate the run's grants"
        );
    }
}
