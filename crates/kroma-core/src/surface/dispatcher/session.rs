//! `RpcDispatcher` methods for the session surface.

use super::*;

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
    pub(crate) fn ensure_active_cancel_receiver(&self) -> watch::Receiver<bool> {
        let mut guard = self.inner.active_cancel.lock().unwrap();
        if let Some((_, ref rx)) = *guard {
            return rx.clone();
        }
        let (tx, rx) = watch::channel(false);
        *guard = Some((tx, rx.clone()));
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
            let _ = self.inner.snapshot_store.reconcile_with_wal(&mut j);
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
                format!(
                    "failed to open session WAL {}: {}",
                    session_path.display(),
                    e
                ),
            )
        })?;
        let _ = self.inner.snapshot_store.reconcile_with_wal(&mut journal);

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
    pub(crate) async fn rpc_session_status(&self) -> Result<Value, JsonRpcError> {
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
    pub(crate) async fn rpc_session_fork(&self, params: &Value) -> Result<Value, JsonRpcError> {
        let turn = params.get("turn").and_then(Value::as_u64).ok_or_else(|| {
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
        for ext_id in &self.inner.mounted_extensions {
            let _ = self.inner.registry.unregister_extension(ext_id);
        }
        Ok(json!({
            "shutdown": true,
            "session_id": self.session_id(),
        }))
    }
}
