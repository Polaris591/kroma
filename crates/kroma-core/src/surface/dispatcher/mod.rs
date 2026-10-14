//! JSON-RPC 2.0 dispatcher: construction, dispatch table, transport plumbing.

mod config;
mod revision;
mod session;
mod tournament;

use super::*;

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

        // F8: a crash midway through a worktree→parent merge leaves the parent workspace
        // half-updated and a merge intent journal behind; roll it back to the recorded pre-merge
        // revision before any new run starts, exactly like the restore-intent pre-check handles an
        // interrupted restore.
        match snapshot_store.recover_pending_merge(env.workspace()) {
            Ok(MergeRecoveryOutcome::NothingPending) => {}
            Ok(MergeRecoveryOutcome::RolledBack {
                pre_merge_revision,
                ..
            }) => {
                eprintln!(
                    "[kroma] recovered an interrupted subagent merge; rolled the workspace back to {}",
                    pre_merge_revision
                );
            }
            Ok(MergeRecoveryOutcome::AlreadyMerged { merged_revision }) => {
                eprintln!(
                    "[kroma] cleared a stale subagent merge journal; revision {} had already been merged",
                    merged_revision
                );
            }
            Err(e) => {
                return Err(format!(
                    "failed to recover an interrupted subagent merge: {}",
                    e
                ));
            }
        }

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
