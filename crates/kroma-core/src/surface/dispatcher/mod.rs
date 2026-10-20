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

/// B01: process-wide id for one daemon stream connection. It attributes a run to the connection
/// that started it; it is not an authorization credential.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ConnectionId(u64);

impl ConnectionId {
    fn next() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        Self(NEXT.fetch_add(1, Ordering::Relaxed))
    }
}

/// B01: upper bound on prompt requests a single connection may have read but not yet claimed. Past
/// it the request is busy-rejected without claiming an owner, so the table cannot grow unbounded.
pub(crate) const MAX_PENDING_REQUESTS_PER_CONNECTION: usize = 8;

/// B11: every wire method name that reaches the session cancel entry point. The request aliases and
/// the ACP `session/cancel` notification all pass the same connection-ownership guard, so no
/// spelling can bypass it.
pub(crate) fn is_cancel_method(method: &str) -> bool {
    matches!(method, "session.cancel" | "session/cancel" | "acp.cancel")
}

/// B12: wire method aliases for durable task control.
pub(crate) fn is_task_control_method(method: &str) -> bool {
    matches!(method, "tasks.control" | "tasks/control")
}

/// B12: `true` when `method` is a *mutating* durable-task control request. Read-only `tasks.list`
/// and `tasks.control { action: "list" }` are not mutating. An unknown/malformed action is not
/// classified as mutating here: it is left to the normal dispatch, which rejects it with invalid
/// params, so this pre-block can never execute it or become a bypass.
pub(crate) fn is_mutating_task_control(method: &str, params: &Value) -> bool {
    if !is_task_control_method(method) {
        return false;
    }
    matches!(
        params.get("action").and_then(Value::as_str),
        Some(
            "pause"
                | "resume"
                | "cancel"
                | "challenge"
                | "resolve_challenge"
                | "acknowledge"
                // B12: acceptance and reconcile mint trusted decisions, so the network entry must
                // refuse them exactly like the other mutating actions (the local in-process entry
                // is unaffected).
                | "accept"
                | "reconcile"
        )
    )
}

/// B12: the stable refusal for a mutating `tasks.control` request arriving over the network. There
/// is no persistent authority that can safely claim a durable task, so the request is refused
/// before it reaches the dispatcher (the trusted in-process entry is unaffected).
pub(crate) fn remote_task_control_refused() -> JsonRpcError {
    JsonRpcError::with_data(
        JSONRPC_SESSION_NOT_OWNED,
        "remote task control is unsupported without a persistent authority",
        json!({ "reason": "not_owner" }),
    )
}

/// B01: a prompt request read from a stream but not yet claimed by a run, tagged with the exact
/// session id resolved when the frame was read.
struct PendingRequest {
    session_id: String,
    tx: watch::Sender<bool>,
}

/// B01: per-connection context for a daemon stream, carried into the task that runs a prompt so the
/// run is bound to (and cancelled with) the connection that actually started it. This is context
/// only; it is not an authorization credential.
#[derive(Clone)]
pub(crate) struct ConnectionContext {
    id: ConnectionId,
    alive: Arc<AtomicBool>,
    /// ACP permission sink for this connection, enabled only once this connection owns a run so an
    /// authenticated observer can never redirect another run's permission requests.
    acp_sink: Option<Arc<dyn Fn(Value) + Send + Sync>>,
    /// B01: bounded admission table of prompt requests keyed by unique request seq.
    pending: Arc<Mutex<BTreeMap<u64, PendingRequest>>>,
    next_seq: Arc<std::sync::atomic::AtomicU64>,
    /// B02a: connection-private default session. Initialized to the primary session, and
    /// updated only when a `session.new` on this connection succeeds.
    default_session: Arc<Mutex<Option<String>>>,
}

impl ConnectionContext {
    #[allow(dead_code)]
    pub(crate) fn new(acp_sink: Option<Arc<dyn Fn(Value) + Send + Sync>>) -> Self {
        Self::with_default_session(acp_sink, None)
    }

    pub(crate) fn with_default_session(
        acp_sink: Option<Arc<dyn Fn(Value) + Send + Sync>>,
        default_session: Option<String>,
    ) -> Self {
        Self {
            id: ConnectionId::next(),
            alive: Arc::new(AtomicBool::new(true)),
            acp_sink,
            pending: Arc::new(Mutex::new(BTreeMap::new())),
            next_seq: Arc::new(std::sync::atomic::AtomicU64::new(1)),
            default_session: Arc::new(Mutex::new(default_session)),
        }
    }

    pub(crate) fn default_session(&self) -> Option<String> {
        self.default_session
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub(crate) fn set_default_session(&self, session_id: String) {
        let mut guard = self
            .default_session
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *guard = Some(session_id);
    }

    pub(crate) fn clear_default_session_if(&self, session_id: &str, fallback: Option<String>) {
        let mut guard = self
            .default_session
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if guard.as_deref() == Some(session_id) {
            *guard = fallback;
        }
    }

    /// Whether this connection has any pending un-retired prompt requests.
    pub(crate) fn has_pending(&self) -> bool {
        let guard = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        !guard.is_empty()
    }

    /// Register a prompt request's cancel capability. Returns `None` at the per-connection
    /// admission cap, so the caller busy-rejects without claiming an owner.
    pub(crate) fn begin_request(&self, session_id: &str) -> Option<RequestCancel> {
        let mut guard = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        if guard.len() >= MAX_PENDING_REQUESTS_PER_CONNECTION {
            return None;
        }
        let seq = self.next_seq.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = watch::channel(false);
        guard.insert(
            seq,
            PendingRequest {
                session_id: session_id.to_string(),
                tx: tx.clone(),
            },
        );
        Some(RequestCancel { seq, tx, rx })
    }

    /// Signal every request registered for `session_id` before this call (the intended pending
    /// operations). A request registered after the cancel is unaffected. Returns true when at
    /// least one request was signalled.
    pub(crate) fn signal_pending(&self, session_id: &str) -> bool {
        let guard = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        let mut any = false;
        for req in guard.values() {
            if req.session_id == session_id {
                let _ = req.tx.send(true);
                any = true;
            }
        }
        any
    }

    /// Signal every registered request (stream EOF), so a run still starting is cancelled too.
    pub(crate) fn cancel_all_pending(&self) -> bool {
        let guard = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        let mut any = false;
        for req in guard.values() {
            let _ = req.tx.send(true);
            any = true;
        }
        any
    }

    /// Remove only this request's admission entry, so a retired request cannot affect a successor.
    pub(crate) fn retire_request(&self, seq: u64) {
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&seq);
    }
}

/// B01: the cancel capability of one prompt request, handed to the task that runs it. `task_local`
/// only carries this context; it grants no authority (the run is still gated by auth + op_lock).
#[derive(Clone)]
pub(crate) struct RequestCancel {
    pub(crate) seq: u64,
    pub(crate) tx: watch::Sender<bool>,
    pub(crate) rx: watch::Receiver<bool>,
}

/// B01: retires a request's admission entry when its task ends (including abort or panic), so a
/// rejected or aborted request can never poison a successor run.
pub(crate) struct RequestGuard {
    conn: ConnectionContext,
    seq: u64,
}

impl RequestGuard {
    pub(crate) fn new(conn: ConnectionContext, seq: u64) -> Self {
        Self { conn, seq }
    }
}

impl Drop for RequestGuard {
    fn drop(&mut self) {
        self.conn.retire_request(self.seq);
    }
}

tokio::task_local! {
    /// B01: connection context of the task dispatching a run. Absent for runs started outside a
    /// daemon stream (REPL / direct `handle_request`), which keep the previous detached behavior.
    pub(crate) static CONNECTION_CONTEXT: ConnectionContext;

    /// B01: the cancel capability of the request being dispatched, if it came from a stream.
    pub(crate) static RUN_REQUEST: RequestCancel;
}

/// B01: a run's ownership claim: the id that identifies its slot, the cancel receiver the run
/// observes, and the ACP owner tag when this run enabled an ACP permission sink.
pub(crate) struct RunClaim {
    pub(crate) run_id: u64,
    pub(crate) cancel_rx: watch::Receiver<bool>,
    pub(crate) acp_owner: Option<u64>,
}

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
        // Stable identity of this runtime instance; the session id it owns can change on
        // `session.new` / `session.resume`, but routing always names the same owner.
        let dispatcher_id = Uuid::new_v4().to_string();
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
            // F10: a failed reconciliation blocks the resume instead of silently starting from a
            // WAL that may disagree with the recorded workspace history.
            snapshot_store.reconcile_with_wal(&mut j).map_err(|e| {
                format!(
                    "failed to reconcile session WAL {} with the snapshot store: {}",
                    path.display(),
                    e
                )
            })?;
            if let Some(turn) = config.fork_turn {
                // F03 (`v1.5.1`): forking a session also restores the workspace to the revision that
                // was current at that turn. The session leaf is only moved after the restore applied,
                // and external edits make the fork fail closed instead of being overwritten.
                let outcome = snapshot_store
                    .fork_workspace_at_turn(env.workspace(), &mut j, turn, false)
                    .map_err(|e| format!("failed to fork workspace at turn {}: {}", turn, e))?;
                if !config.quiet {
                    eprintln!(
                        "[kroma] forked workspace at turn {} (restored {} file(s), revision {})",
                        outcome.forked_at_turn,
                        outcome.restored_files.len(),
                        outcome.target_revision
                    );
                }
            }
            j
        } else {
            SessionJournal::create(config.state_dir.join("sessions"), env.workspace().root())
                .map_err(|e| format!("failed to create session WAL: {}", e))?
        };
        snapshot_store
            .reconcile_with_wal(&mut journal)
            .map_err(|e| {
                format!(
                    "failed to reconcile the session WAL with the snapshot store: {}",
                    e
                )
            })?;
        // F03 follow-up (P1-2): finish or abandon a workspace-bound fork that crashed between its
        // restore and its leaf move. Run before `snapshot_from_journal` so the published status
        // names the forked leaf, and propagate a recovery-store failure so the runtime refuses to
        // start on an inconsistent store.
        match snapshot_store.recover_pending_fork(env.workspace(), &mut journal) {
            Ok(ForkRecoveryOutcome::NothingPending) => {}
            Ok(ForkRecoveryOutcome::Completed(outcome)) => {
                eprintln!(
                    "[kroma] completed an interrupted fork to turn {} (revision {})",
                    outcome.forked_at_turn, outcome.target_revision
                );
            }
            Ok(ForkRecoveryOutcome::Abandoned {
                forked_at_turn,
                target_revision,
                conflicting_paths,
            }) => {
                eprintln!(
                    "[kroma] abandoned an interrupted fork to turn {} (revision {}): {} path(s) changed on disk since the fork began",
                    forked_at_turn,
                    target_revision,
                    conflicting_paths.len()
                );
            }
            Err(e) => {
                return Err(format!("failed to recover an interrupted fork: {}", e));
            }
        }
        let mut initial_snapshot = Self::snapshot_from_journal(&journal);
        if let Some(ref skill_name) = config.initial_skill {
            initial_snapshot.activated_skills.push(skill_name.clone());
        }
        // Session routing: publish which runtime owns which session/workspace so a control plane can
        // route by session id instead of sharing one global session.
        crate::session_ownership::register_session(
            &initial_snapshot.session_id,
            env.workspace().root(),
            &dispatcher_id,
        );
        drop(journal);

        // F8: a crash midway through a worktree→parent merge leaves the parent workspace
        // half-updated and a merge intent journal behind; roll it back to the recorded pre-merge
        // revision before any new run starts, exactly like the restore-intent pre-check handles an
        // interrupted restore.
        match snapshot_store.recover_pending_merge(env.workspace()) {
            Ok(MergeRecoveryOutcome::NothingPending) => {}
            Ok(MergeRecoveryOutcome::RolledBack {
                pre_merge_revision, ..
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

        let mut registry = ToolRegistry::new(config.tool_filter.clone())
            .with_workspace_skills(workspace_skills.clone());
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

        // Capability gap #6 (`v1.5.2`): load trusted MCP servers (user config / explicit CLI only —
        // never the workspace file) and register their tools so they are dispatchable like any other
        // tool. A configured server that fails to start is surfaced, not silently dropped.
        if let Some(resolved) = config.resolved_config.as_ref() {
            match crate::mcp::trusted_mcp_extension(&resolved.mcp_servers) {
                Ok(Some((ext, tool_count))) => {
                    let server_count = resolved.mcp_servers.len();
                    match registry.register_extension(ext) {
                        Ok(id) => {
                            mounted_extensions.push(id);
                            if !config.quiet {
                                eprintln!(
                                    "[kroma] mounted {} MCP tool(s) from {} trusted server(s)",
                                    tool_count, server_count
                                );
                            }
                        }
                        Err(e) => {
                            return Err(format!("failed to mount MCP tools: {}", e));
                        }
                    }
                }
                Ok(None) => {}
                Err(e) => {
                    return Err(format!("failed to start a configured MCP server: {}", e));
                }
            }
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
        // F05 (`v1.5.1`): the live token cap starts from the startup budget (`--max-tokens` /
        // `max_tokens` config / env) and is hot-swappable through `/config set max_tokens`.
        let initial_max_tokens = config.budget.max_total_tokens;
        let initial_resolved = config.resolved_config.clone();
        // `v1.5.0`: Design D (`Zen`) is the default terminal theme; `KROMA_UI_THEME` overrides it.
        let initial_theme = Theme::from_env();
        // Capability gap #4 (`v1.5.2`): open the durable async-task registry and mark any task left
        // `Running` by a previous process as `Suspended`, so a crash can never present in-flight
        // work as completed. The same `Arc` backs the executable [`DurableTaskHost`] below.
        let async_tasks = Arc::new(Mutex::new({
            let path = config.state_dir.join("tasks").join("registry.json");
            let mut registry = crate::async_tasks::AsyncTaskRegistry::open(&path).map_err(|e| {
                format!(
                    "failed to open async task registry at {}: {}",
                    path.display(),
                    e
                )
            })?;
            if let Err(e) = registry.recover_interrupted() {
                return Err(format!("failed to recover interrupted tasks: {}", e));
            }
            registry
        }));
        let durable_tasks = Arc::new(
            crate::task_host::DurableTaskHost::new(
                Arc::clone(&async_tasks),
                Arc::clone(&provider),
                Arc::clone(&snapshot_store),
                env.workspace().clone(),
                config.state_dir.join("subagents"),
                config.state_dir.join("worktrees"),
                workspace_skills.clone(),
                config.policy_mode,
                config.budget.clone(),
                config.predictive_check,
                Arc::clone(&config.completion_gate),
                initial_snapshot.session_id.clone(),
                dispatcher_id.clone(),
                initial_snapshot.session_path.clone(),
            )
            // R01: a resumed child runs under the same operator-resolved boundary as the live
            // session's tool runtime. `resume_task` refuses to run at all when no policy was
            // attached, so this can never silently fall back to the host.
            .with_execution_policy(config.execution_policy.clone()),
        );
        // A crash between a resumed worker's merge and its session-WAL append leaves a durable,
        // session-bound merge intent. Replay it before any new run so the merge's `--undo` link is
        // re-recorded instead of silently lost. A locked WAL is reported, not silently skipped.
        match durable_tasks.replay_pending_merges() {
            Ok(report) => {
                if !config.quiet && report.linked > 0 {
                    eprintln!(
                        "[kroma] replayed {} pending session WAL merge link(s) for resumed tasks",
                        report.linked
                    );
                }
                if !report.unresolved.is_empty() {
                    eprintln!(
                        "[kroma] {} task merge link(s) could not be replayed and were cleared; RECOVERY REQUIRED for: {:?}",
                        report.unresolved.len(),
                        report.unresolved
                    );
                }
                if !report.failed.is_empty() {
                    // A locked/missing WAL is a normal condition: the intent is retained and the next
                    // startup retries it, but the operator must know the merge's undo link is still
                    // not recorded.
                    eprintln!(
                        "[kroma] {} task merge link(s) could not be replayed and were retained; RECOVERY REQUIRED for: {:?}",
                        report.failed.len(),
                        report.failed
                    );
                }
            }
            Err(e) => {
                eprintln!("[kroma] could not replay a pending task merge link: {}", e);
            }
        }
        let inner = Arc::new(RuntimeEngineInner {
            config,
            dispatcher_id,
            provider: Mutex::new(provider),
            env,
            snapshot_store,
            experience: Arc::new(Mutex::new(
                crate::experience_runtime::ExperienceSession::default(),
            )),
            registry,
            policy_engine,
            approval_broker: Arc::clone(&approval_broker),
            workspace_skills,
            mounted_extensions,
            status: Mutex::new(initial_snapshot),
            active_run: Mutex::new(None),
            next_run_id: std::sync::atomic::AtomicU64::new(1),
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
            active_max_tokens: Mutex::new(initial_max_tokens),
            resolved_config: Mutex::new(initial_resolved),
            active_theme: Mutex::new(initial_theme),
            #[cfg(test)]
            pre_claim_gate: Mutex::new(None),
            async_tasks,
            durable_tasks,
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
    /// Identity of this runtime and the session it currently owns (routing seam for the control
    /// plane; see [`crate::session_ownership`]).
    pub fn ownership(&self) -> SessionOwnership {
        let snap = self.inner.status.lock().unwrap_or_else(|e| e.into_inner());
        SessionOwnership {
            dispatcher_id: self.inner.dispatcher_id.clone(),
            session_id: snap.session_id.clone(),
            session_path: snap.session_path.clone(),
            workspace_root: self.inner.env.workspace().root().to_path_buf(),
            surface: self.inner.config.surface_name.clone(),
            transport: self.inner.config.transport_name.clone(),
        }
    }
    /// F10: reconcile the WAL with the snapshot store and **propagate** a failed recovery. A
    /// mutation entry point calls this and refuses to proceed on error; only read-only status
    /// degrades instead of failing.
    pub(crate) fn reconcile_journal(
        &self,
        journal: &mut SessionJournal,
    ) -> Result<(), JsonRpcError> {
        self.inner
            .snapshot_store
            .reconcile_with_wal(journal)
            .map(|_| ())
            .map_err(|e| {
                JsonRpcError::with_data(
                    JSONRPC_RECOVERY_REQUIRED,
                    format!("session WAL reconciliation failed: {}", e),
                    json!({ "session_id": journal.session_id() }),
                )
            })?;
        // F03 follow-up (P1-2): a mutation entry point must also converge an interrupted
        // workspace-bound fork before it runs, so no request resumes on a session whose leaf and
        // files disagree.
        self.inner
            .snapshot_store
            .recover_pending_fork(self.inner.env.workspace(), journal)
            .map_err(|e| {
                JsonRpcError::with_data(
                    JSONRPC_RECOVERY_REQUIRED,
                    format!("interrupted fork recovery failed: {}", e),
                    json!({ "session_id": journal.session_id() }),
                )
            })?;
        Ok(())
    }
    /// F10: refuse a new mutation while an interrupted restore is unrecovered, because the
    /// workspace may be half-applied.
    pub(crate) fn ensure_no_pending_restore(&self) -> Result<(), JsonRpcError> {
        match self.inner.snapshot_store.pending_restore() {
            Ok(None) => Ok(()),
            Ok(Some(intent)) => Err(JsonRpcError::with_data(
                JSONRPC_RECOVERY_REQUIRED,
                format!(
                    "an interrupted restore of {} must be recovered before a new mutation",
                    intent.target_revision
                ),
                json!({
                    "target_revision": intent.target_revision.as_str(),
                    "intent_journal": self.inner.snapshot_store.intent_path().display().to_string(),
                }),
            )),
            Err(e) => Err(JsonRpcError::with_data(
                JSONRPC_RECOVERY_REQUIRED,
                format!("failed to read the restore intent journal: {}", e),
                Value::Null,
            )),
        }
    }
    /// Workspace writer ownership: at most one live writer leases a workspace root across every
    /// runtime instance in this process. Different workspaces therefore run independently.
    pub(crate) fn acquire_writer_lease(
        &self,
    ) -> Result<crate::session_ownership::WriterLease, JsonRpcError> {
        let workspace = self.inner.env.workspace().root().to_path_buf();
        let owner = format!("{}:{}", self.inner.dispatcher_id, self.session_id());
        crate::session_ownership::try_acquire_writer(&workspace, &owner).map_err(|conflict| {
            JsonRpcError::with_data(
                JSONRPC_WORKSPACE_BUSY,
                conflict.to_string(),
                json!({
                    "workspace_root": conflict.workspace_root.display().to_string(),
                    "current_owner": conflict.current_owner,
                }),
            )
        })
    }
    /// Reject a request whose explicit `session_id`/`sessionId` parameter names a session this
    /// runtime does not own, so a misrouted request cannot mutate the wrong session.
    pub(crate) fn validate_session_param(&self, params: &Value) -> Result<(), JsonRpcError> {
        let requested = params
            .get("session_id")
            .or_else(|| params.get("sessionId"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let Some(requested) = requested else {
            return Ok(());
        };
        let owned = self.session_id();
        if requested != owned {
            return Err(JsonRpcError::with_data(
                JSONRPC_SESSION_NOT_OWNED,
                format!(
                    "this runtime owns session {} but the request targets {}",
                    owned, requested
                ),
                json!({ "session_id": owned, "requested_session_id": requested }),
            ));
        }
        Ok(())
    }
    /// Publish that this runtime now owns `session_id` for its workspace, replacing any previous
    /// route for that session id on this dispatcher.
    pub(crate) fn register_owned_session(&self, session_id: &str) {
        crate::session_ownership::register_session(
            session_id,
            self.inner.env.workspace().root(),
            &self.inner.dispatcher_id,
        );
    }
    pub fn is_shutting_down(&self) -> bool {
        self.inner.shutting_down.load(Ordering::SeqCst)
    }
    /// Subscribe to real-time JSON-RPC notifications (`kroma/event`, `approval/requested`,
    /// `approval/resolved`).
    pub fn subscribe_notifications(&self) -> mpsc::UnboundedReceiver<JsonRpcNotification> {
        let (tx, rx) = mpsc::unbounded_channel();
        let _id = self.add_subscriber(Arc::new(move |note| tx.send(note.clone()).is_ok()));
        rx
    }
    /// B02a: Register an external subscriber callback. Returns a unique subscriber id.
    pub(crate) fn add_subscriber(
        &self,
        cb: Arc<dyn Fn(&JsonRpcNotification) -> bool + Send + Sync>,
    ) -> u64 {
        let sub_id = self.inner.next_subscriber_id.fetch_add(1, Ordering::SeqCst);
        self.inner.subscribers.lock().unwrap().push((sub_id, cb));
        sub_id
    }
    /// B02a: Remove subscriber callback by unique subscriber id.
    pub(crate) fn remove_subscriber(&self, id: u64) {
        self.inner
            .subscribers
            .lock()
            .unwrap()
            .retain(|(existing_id, _)| *existing_id != id);
    }
    /// B02a: Count of currently registered notification subscribers.
    /// Internal test-only visibility; retained for connection lifecycle integration tests.
    #[doc(hidden)]
    pub fn subscriber_count(&self) -> usize {
        self.inner.subscribers.lock().unwrap().len()
    }
    /// B02a: Snapshot subscriber callbacks inside lock, execute outside lock to prevent lock
    /// inversion / deadlocks. Dropped subscribers returning false are cleaned up.
    fn broadcast_to_subscribers(inner: &RuntimeEngineInner, note: JsonRpcNotification) {
        let subs = inner.subscribers.lock().unwrap().clone();
        let mut dropped = Vec::new();
        for (id, cb) in subs {
            if !cb(&note) {
                dropped.push(id);
            }
        }
        if !dropped.is_empty() {
            let mut guard = inner.subscribers.lock().unwrap();
            guard.retain(|(id, _)| !dropped.contains(id));
        }
    }
    pub fn emit_notification(&self, note: JsonRpcNotification) {
        Self::broadcast_to_subscribers(&self.inner, note);
    }
    /// Trusted local-host control only; deliberately not registered as a network RPC method.
    pub fn experience_local_control(
        &self,
        action: &str,
        argument: Option<&str>,
    ) -> Result<String, String> {
        let _guard = self
            .inner
            .op_lock
            .try_lock()
            .map_err(|_| "experience control requires an idle local session".to_string())?;
        if self.inner.in_flight.load(Ordering::SeqCst) {
            return Err("experience control requires an idle local session".into());
        }
        let workspace = self.inner.env.workspace();
        let revision =
            crate::experience_runtime::live_revision(&self.inner.snapshot_store, workspace)?;
        let mut experience = self
            .inner
            .experience
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if action == "load" {
            experience.load(
                std::path::Path::new(argument.ok_or("experience load needs an absolute file")?),
                workspace,
                &revision,
            )
        } else {
            experience.control(
                action,
                argument,
                workspace,
                &revision,
                self.inner.policy_engine.mode(),
            )
        }
    }

    fn snapshot_from_journal(journal: &SessionJournal) -> SessionStatusSnapshot {
        let turn_count = journal.max_active_turn();
        let latest_revision_id = journal.latest_revision_id();
        let records = journal.revision_records();
        let reverted_active_revisions = (|| -> Option<BTreeSet<String>> {
            let last = records.last()?;
            let first_idx = records
                .iter()
                .position(|r| r.revision_id == last.revision_id)?;
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
            // Capability gap #4 (`v1.5.2`): durable async task control plane.
            "tasks.list",
            "tasks.control",
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
                    format!(
                        "unsupported jsonrpc version '{}'; expected '2.0'",
                        req.jsonrpc
                    ),
                ),
            );
        }

        // Both dispatch futures are boxed because `handle_request` is the seam every surface calls:
        // kept inline, this future carried the whole method table (every `rpc_*` arm) inside each
        // caller that holds it across a suspension point, and a debug build then spends ~1 MiB of
        // stack on the poll chain alone — enough to abort a 2 MiB tokio worker thread. One heap
        // allocation per request buys back the depth.
        let res = if self.inner.config.acp_protocol && is_acp_client_method(req.method.as_str()) {
            Box::pin(self.dispatch_acp(&req.method, &req.params)).await
        } else {
            Box::pin(self.dispatch_internal(&req.method, &req.params)).await
        };

        match res {
            Ok(val) => JsonRpcResponse::success(id, val),
            Err(err) => JsonRpcResponse::error(id, err),
        }
    }
    /// F06: route the official ACP v1 client → agent methods. Unsupported optional methods fail with
    /// `method not found` instead of silently doing nothing, so a client is never misled about the
    /// negotiated capabilities.
    async fn dispatch_acp(&self, method: &str, params: &Value) -> Result<Value, JsonRpcError> {
        match method {
            "initialize" => self.acp_initialize(params).await,
            "session/new" => self.acp_session_new(params).await,
            "session/prompt" => self.acp_session_prompt(params).await,
            "session/cancel" => {
                self.acp_session_cancel(params).await?;
                Ok(json!({ "cancelled": true }))
            }
            // Optional capabilities this agent does not advertise (loadSession/authMethods/session
            // modes). A conformant client will not call them; if it does, be explicit.
            "authenticate" | "session/load" | "session/set_mode" => Err(JsonRpcError::new(
                JSONRPC_METHOD_NOT_FOUND,
                format!("ACP method '{}' is not supported by this agent", method),
            )),
            other => self.dispatch_internal(other, params).await,
        }
    }
    /// The internal Kroma JSON-RPC API (retained unchanged). `params` is the request params.
    async fn dispatch_internal(&self, method: &str, params: &Value) -> Result<Value, JsonRpcError> {
        match method {
            "initialize" | "acp.initialize" => self.rpc_initialize(params).await,
            "session.new" | "session/new" | "acp.session.new" => self.rpc_session_new(params).await,
            "session.start" | "session.prompt" | "acp.prompt" | "session/prompt" => {
                self.rpc_session_prompt(params).await
            }
            "approval.pending" | "approval/pending" => self.rpc_approval_pending().await,
            "approval.respond" | "approval/respond" => self.rpc_approval_respond(params).await,
            "session.cancel" | "session/cancel" | "acp.cancel" => self.rpc_session_cancel().await,
            "session.status" | "session/status" => self.rpc_session_status().await,
            "session.fork" | "session/fork" => self.rpc_session_fork(params).await,
            "revision.list" | "revision/list" => self.rpc_revision_list().await,
            "revision.undo" | "revision/undo" => self.rpc_revision_undo(params).await,
            "revision.restore" | "revision/restore" => self.rpc_revision_restore(params).await,
            "evidence.recall" | "evidence/recall" => self.rpc_evidence_recall(params).await,
            // Gate 7 (v1.2.0): symbol dependency graph + Personalized PageRank navigation.
            "workspace.graph" | "workspace/graph" => self.rpc_workspace_graph(params).await,
            // v1.3.0: layered configuration + live model/protocol hot-swapping.
            "config.get" | "config/get" => self.rpc_config_get(params).await,
            "config.set" | "config/set" | "session.configure" | "session/configure" => {
                self.rpc_config_set(params).await
            }
            // v1.3.0: in-REPL session listing and switching.
            "sessions.list" | "sessions/list" | "session.list" => self.rpc_sessions_list().await,
            "session.resume" | "session/resume" => self.rpc_session_resume(params).await,
            // v1.3.0: Best-of-N worktree tournament.
            "session.tournament" | "session/tournament" => {
                self.rpc_session_tournament(params).await
            }
            "skills.list" | "skills/list" => self.rpc_skills_list().await,
            "skills.activate" | "skills/activate" => self.rpc_skills_activate(params).await,
            // Capability gap #4 (`v1.5.2`): supervisor control over durable async tasks.
            "tasks.list" | "tasks/list" => self.rpc_tasks_list(params).await,
            "tasks.control" | "tasks/control" => self.rpc_tasks_control(params).await,
            "shutdown" | "acp.shutdown" => self.rpc_shutdown().await,
            other => Err(JsonRpcError::new(
                JSONRPC_METHOD_NOT_FOUND,
                format!("unknown RPC method '{}'", other),
            )),
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
        *self
            .inner
            .active_theme
            .lock()
            .unwrap_or_else(|e| e.into_inner())
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
                format!(
                    "session prefix '{}' is ambiguous ({} matches)",
                    requested, n
                ),
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
    pub async fn serve_stream<R, W>(&self, reader: R, writer: W) -> Result<(), String>
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        self.serve_stream_with_config(reader, writer, TransportConfig::default())
            .await
    }

    /// Serve newline-delimited JSON-RPC 2.0 with configurable transport bounds and timeouts.
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

        // F06: on the ACP surface, tool approvals become standard `session/request_permission`
        // requests written to this stream. B01: the sink is enabled only once this connection owns
        // a run (see `claim_run`), so an unauthenticated or observer connection cannot receive or
        // redirect another run's approval traffic.
        let acp_sink: Option<Arc<dyn Fn(Value) + Send + Sync>> = if self.inner.config.acp_protocol {
            let out_tx_acp = out_tx.clone();
            Some(Arc::new(move |frame: Value| {
                if let Ok(serialized) = serde_json::to_string(&frame) {
                    let _ = out_tx_acp.try_send_sync(serialized);
                }
            }))
        } else {
            None
        };
        let conn = ConnectionContext::with_default_session(acp_sink, Some(self.session_id()));

        let writer_task = tokio::spawn(run_bounded_writer(
            out_rx,
            writer,
            state.clone(),
            state_rx.clone(),
        ));

        let mut buf_reader = BufReader::new(reader);
        let mut frame_reader = FrameReader::new(config.max_frame_bytes);
        let mut connection_authenticated = self.inner.config.auth_token.is_none();
        let mut sub_id: Option<u64> = None;
        let mut in_flight_prompts = tokio::task::JoinSet::new();

        let activate_authenticated_stream = |sub_id_slot: &mut Option<u64>| {
            if sub_id_slot.is_none() {
                let out_tx_notes = out_tx.clone();
                let id = self.add_subscriber(Arc::new(move |note| {
                    if let Ok(serialized) = serde_json::to_string(note) {
                        out_tx_notes.try_send_sync(serialized).is_ok()
                    } else {
                        true
                    }
                }));
                *sub_id_slot = Some(id);
            }
        };

        if connection_authenticated {
            activate_authenticated_stream(&mut sub_id);
        }

        let auth_deadline = tokio::time::Instant::now() + config.auth_handshake_timeout;
        let mut auth_failures: usize = 0;

        loop {
            if !matches!(*state_rx.borrow(), TransportState::Open) {
                break;
            }

            let has_active_work = conn.has_pending() || !in_flight_prompts.is_empty();
            let next_deadline = if !connection_authenticated {
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
                Some(join_res) = in_flight_prompts.join_next(), if !in_flight_prompts.is_empty() => {
                    if let Err(e) = join_res {
                        if e.is_panic() {
                            eprintln!("[kroma] single dispatcher prompt task panicked: {:?}", e);
                        }
                    }
                    continue;
                }
                read_res = tokio::time::timeout_at(next_deadline, frame_reader.read_frame(&mut buf_reader)) => {
                    match read_res {
                        Ok(line_res) => line_res,
                        Err(_timeout) => {
                            if !connection_authenticated {
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
                Ok(None) => {
                    break;
                }
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
                    let _ = out_tx.try_send_sync(s);
                }
                break;
            }

            let frame: Value = match serde_json::from_str(trimmed) {
                Ok(v) => v,
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

            // F06: on the ACP surface a frame with an `id` and `result`/`error` but no `method` is a
            // client's response to an agent-initiated request (`session/request_permission`).
            // Finding 4: it is only accepted from an authenticated connection that owns the exact
            // active ACP run; a predictable request id is not authority.
            if self.inner.config.acp_protocol && frame.get("method").is_none() {
                if !connection_authenticated {
                    let resp = JsonRpcResponse::error(
                        frame.get("id").cloned().unwrap_or(Value::Null),
                        JsonRpcError::new(
                            JSONRPC_UNAUTHORIZED,
                            "unauthorized: a permission response requires an authenticated stream",
                        ),
                    );
                    if let Ok(s) = serde_json::to_string(&resp) {
                        let _ = out_tx.try_send_sync(s);
                    }
                    continue;
                }
                if let Some(id) = frame.get("id").and_then(Value::as_str) {
                    let result = frame.get("result").cloned().unwrap_or(Value::Null);
                    if let Err(e) = self.resolve_acp_response_from(&conn, id, result) {
                        let resp = JsonRpcResponse::error(
                            frame.get("id").cloned().unwrap_or(Value::Null),
                            JsonRpcError::new(JSONRPC_INVALID_REQUEST, e),
                        );
                        if let Ok(s) = serde_json::to_string(&resp) {
                            let _ = out_tx.try_send_sync(s);
                        }
                    }
                }
                continue;
            }

            let mut req: JsonRpcRequest = match serde_json::from_value(frame) {
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

            if !connection_authenticated {
                let expected = self.inner.config.auth_token.as_deref().unwrap_or("");
                let provided_token = req
                    .params
                    .get("auth_token")
                    .or_else(|| req.params.get("token"))
                    .and_then(Value::as_str);

                if let Some(provided) = provided_token {
                    if constant_time_eq(provided.as_bytes(), expected.as_bytes()) {
                        connection_authenticated = true;
                        activate_authenticated_stream(&mut sub_id);
                    } else {
                        auth_failures += 1;
                        let resp = JsonRpcResponse::error(
                            req.id.unwrap_or(Value::Null),
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
                        req.id.unwrap_or(Value::Null),
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
            // F06: ACP `session/cancel` is a notification (no `id`) and must not receive a
            // response. Handle it here and return to reading so a subsequent `session/prompt`
            // response can still be written.
            if self.inner.config.acp_protocol && req.id.is_none() {
                if req.method == "session/cancel" {
                    // B11: the ACP cancel notification passes the same connection-ownership guard as
                    // the request aliases. The routed session is validated first (a misrouted
                    // notification is ignored), and a non-owner's notification is ignored because a
                    // notification carries no response.
                    if self.acp_owned_session(&req.params).is_ok() {
                        let _ = self.rpc_session_cancel_from(&conn);
                    }
                }
                continue;
            }

            let sid = super::session_manager::requested_session_id(&req.params)
                .unwrap_or_else(|| self.session_id());

            // Parameter normalization: rejects arrays / primitives; normalizes null to object and inserts session_id
            if let Err(err) = normalize_request_params(&mut req.params, &sid) {
                let resp = JsonRpcResponse::error(req.id.clone().unwrap_or(Value::Null), err);
                if let Ok(s) = serde_json::to_string(&resp) {
                    let _ = out_tx.try_send_sync(s);
                }
                continue;
            }

            // B12: remote durable-task control is refused before the generic dispatch. There is no
            // persistent authority that can safely claim a task, so no mutating `tasks.control`
            // action is admitted over the network; reads and the trusted in-process entry are
            // unaffected. This is an explicit, stable authorization refusal, not the accidental
            // schema rejection the normalized `session_id` used to cause.
            if is_mutating_task_control(&req.method, &req.params) {
                let resp = JsonRpcResponse::error(
                    req.id.clone().unwrap_or(Value::Null),
                    remote_task_control_refused(),
                );
                if let Ok(s) = serde_json::to_string(&resp) {
                    let _ = out_tx.try_send_sync(s);
                }
                continue;
            }

            // B11: every cancel spelling a stream can send goes through the same ownership guard
            // before the generic dispatch, so no alias can reach the unguarded trusted cancel. The
            // guard also delivers a connection's own not-yet-claimed request.
            if is_cancel_method(&req.method) {
                let id = req.id.clone().unwrap_or(Value::Null);
                let resp = match self.rpc_session_cancel_from(&conn) {
                    Ok(val) => JsonRpcResponse::success(id, val),
                    Err(err) => JsonRpcResponse::error(id, err),
                };
                if let Ok(s) = serde_json::to_string(&resp) {
                    let _ = out_tx.try_send_sync(s);
                }
                continue;
            }
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
                if in_flight_prompts.len() >= MAX_PENDING_REQUESTS_PER_CONNECTION {
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
                let Some(request) = conn.begin_request(&sid) else {
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
                let dispatcher = self.clone();
                let out_tx_prompt = out_tx.clone();
                let conn_for_task = conn.clone();
                in_flight_prompts.spawn(CONNECTION_CONTEXT.scope(
                    conn_for_task,
                    RUN_REQUEST.scope(request, async move {
                        let _guard = guard;
                        let resp = dispatcher.handle_request(req).await;
                        if let Ok(s) = serde_json::to_string(&resp) {
                            let _ = out_tx_prompt.try_send_sync(s);
                        }
                    }),
                ));
            } else {
                let resp = self.handle_request(req).await;
                if let Ok(s) = serde_json::to_string(&resp) {
                    let _ = out_tx.try_send_sync(s);
                }
                if is_shutdown {
                    break;
                }
            }
        }

        // B01: only the connection that owns the active run may tear its approval channel down; an
        // authenticated observer's EOF must not cancel or deny another connection's run. Registered
        // but unclaimed requests are cancelled too. The owner check and the broker close/disable are
        // serialised inside `end_connection_run` (Finding 2), so no stale run id can wipe a
        // successor's approvals.
        if connection_authenticated {
            let _ = conn.cancel_all_pending();
            let _ = self.end_connection_run(&conn);
        }
        out_tx.initiate_graceful_close();
        if let Some(id) = sub_id {
            self.remove_subscriber(id);
        }
        while let Some(join_res) = in_flight_prompts.join_next().await {
            if let Err(e) = join_res {
                if e.is_panic() {
                    eprintln!(
                        "[kroma] single dispatcher prompt task panicked on exit: {:?}",
                        e
                    );
                }
            }
        }
        // B01: the connection context holds this stream's ACP sink, which captures an `out_tx`
        // clone. Drop it (after the prompt tasks that also hold it) before awaiting the writer task,
        // otherwise the writer task never observes the channel close and the connection leaks.
        drop(conn);
        drop(out_tx);
        let _ = writer_task.await;
        Ok(())
    }
}
