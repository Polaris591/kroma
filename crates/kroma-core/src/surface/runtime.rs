//! runtime surface of the multi-surface runtime (split from `surface.rs`).

use super::*;

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
    /// Capability gap #2 (`v1.5.2`): opt-in isolated counterfactual probe on repeated verification
    /// failure. Off by default; enabling it also installs operator consent for each probe.
    pub counterfactual_probe: bool,
    /// Capability gap #5 (`v1.5.2`): whether a repair race requires a real verification pass.
    pub tournament_require_verified: bool,
    /// Capability gap #6 (`v1.5.2`): pin only core tool schemas and discover the rest on demand.
    pub on_demand_tools: bool,
    /// Capability gap #3 (`v1.5.2`): replace dead-end branch output with evidence-linked stubs so it
    /// does not crowd the context window. Off by default; the CLI exposes `--causal-pruning`.
    pub causal_pruning: bool,
    pub initial_skill: Option<String>,
    pub extensions: Vec<ScopedExtension>,
    pub surface_name: String,
    pub transport_name: String,
    pub auth_token: Option<String>,
    pub approval_timeout: Duration,
    pub quiet: bool,
    /// F06 (`v1.5.2`): serve the official Agent Client Protocol v1 method names (`initialize`,
    /// `session/new`, `session/prompt`, `session/cancel`, `session/update`,
    /// `session/request_permission`) on this runtime's stdio stream instead of the internal Kroma
    /// method names. The internal API stays available on every runtime; this only selects which
    /// method vocabulary the `acp` surface speaks.
    pub acp_protocol: bool,
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
            counterfactual_probe: false,
            tournament_require_verified: true,
            on_demand_tools: false,
            causal_pruning: false,
            initial_skill: None,
            extensions: Vec::new(),
            surface_name: "acp".to_string(),
            transport_name: "stdio".to_string(),
            auth_token: None,
            approval_timeout: DEFAULT_APPROVAL_TIMEOUT,
            quiet: false,
            acp_protocol: false,
            provider_factory: None,
            resolved_config: None,
        }
    }

    /// F06 (`v1.5.2`): speak the official Agent Client Protocol v1 vocabulary on the stdio stream.
    pub fn with_acp_protocol(mut self, enabled: bool) -> Self {
        self.acp_protocol = enabled;
        self
    }

    /// Capability gap #2 (`v1.5.2`): enable the opt-in isolated counterfactual probe.
    pub fn with_counterfactual_probe(mut self, enabled: bool) -> Self {
        self.counterfactual_probe = enabled;
        self
    }

    /// Capability gap #6 (`v1.5.2`): enable on-demand tool schema discovery.
    pub fn with_on_demand_tools(mut self, enabled: bool) -> Self {
        self.on_demand_tools = enabled;
        self
    }

    /// `v1.3.0`: attach a [`ProviderFactory`] so the active model/protocol can be hot-swapped.
    pub fn with_provider_factory(mut self, factory: Arc<dyn ProviderFactory>) -> Self {
        self.provider_factory = Some(factory);
        self
    }

    /// `v1.3.0`: attach the merged layered configuration for `/config` inspection.
    pub fn with_resolved_config(mut self, resolved: crate::config_file::ResolvedConfig) -> Self {
        self.resolved_config = Some(resolved);
        self
    }

    pub fn with_surface(
        mut self,
        surface: impl Into<String>,
        transport: impl Into<String>,
    ) -> Self {
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
pub(crate) struct SessionStatusSnapshot {
    pub(crate) session_id: String,
    pub(crate) session_path: PathBuf,
    pub(crate) turn_count: usize,
    pub(crate) latest_revision_id: Option<String>,
    pub(crate) subtree_dirs: Vec<String>,
    pub(crate) mutations: Vec<MutationRecord>,
    pub(crate) subagents_spawned: usize,
    pub(crate) revisions: Vec<RevisionRecord>,
    pub(crate) activated_skills: Vec<String>,
    pub(crate) total_input_tokens: u64,
    pub(crate) total_cached_input_tokens: u64,
    pub(crate) total_output_tokens: u64,
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

/// Stable, process-local identity of one runtime instance and the session it currently owns.
///
/// Exposed so a control plane can route a request to the runtime that owns a session instead of
/// sharing one global session across clients (see [`crate::session_ownership`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionOwnership {
    pub dispatcher_id: String,
    pub session_id: String,
    pub session_path: PathBuf,
    pub workspace_root: PathBuf,
    pub surface: String,
    pub transport: String,
}

pub(crate) struct RuntimeEngineInner {
    pub(crate) config: KromaRuntimeConfig,
    /// Unique id of this runtime instance; independent of the (resumable) session id.
    pub(crate) dispatcher_id: String,
    pub(crate) provider: Mutex<Arc<dyn ModelProvider>>,
    pub(crate) env: Arc<LocalHostEnv>,
    pub(crate) snapshot_store: Arc<SnapshotStore>,
    pub(crate) registry: ToolRegistry,
    pub(crate) policy_engine: PolicyEngine,
    pub(crate) approval_broker: Arc<RpcApprovalBroker>,
    pub(crate) workspace_skills: Vec<SkillEntry>,
    pub(crate) mounted_extensions: Vec<ExtensionId>,
    pub(crate) status: Mutex<SessionStatusSnapshot>,
    /// B01: the run currently active on this dispatcher. The connection that started it (if any)
    /// and its cancel channel live in one slot, so a stream disconnect can never observe an owner
    /// without a channel to cancel, and a stale guard can never clear a newer run.
    pub(crate) active_run: Mutex<Option<ActiveRun>>,
    /// B01: monotonic id distinguishing successive runs on this dispatcher.
    pub(crate) next_run_id: std::sync::atomic::AtomicU64,
    pub(crate) in_flight: AtomicBool,
    pub(crate) shutting_down: AtomicBool,
    pub(crate) next_subscriber_id: std::sync::atomic::AtomicU64,
    pub(crate) subscribers: Mutex<Vec<(u64, SubscriberCallback)>>,
    pub(crate) op_lock: AsyncMutex<()>,
    /// `v1.3.0`: active model id after any `/model` hot-swap.
    pub(crate) active_model: Mutex<Option<String>>,
    /// `v1.3.0`: active wire protocol after any `/protocol` hot-swap.
    pub(crate) active_protocol: Mutex<Option<String>>,
    /// `v1.3.0`: active reasoning/thinking effort after any `/thinking` hot-swap (`None` = off).
    pub(crate) active_reasoning_effort: Mutex<Option<String>>,
    /// B2 (`v1.4.0`): active context projection profile after `/config set projection_profile`.
    pub(crate) active_projection_profile:
        Mutex<Option<crate::context_projector::ProjectionProfile>>,
    /// Item #3 (`v1.4.0`): active adaptive-tournament candidate count (`0` = off).
    pub(crate) active_adaptive_tournament: Mutex<usize>,
    /// F05 (`v1.5.1`): live run-tree token cap (`/config set max_tokens`). Seeded from the layered
    /// config/CLI and applied to every run and tournament this runtime starts, so `/config set`
    /// hot-changes the same budget the CLI `--max-tokens` sets.
    pub(crate) active_max_tokens: Mutex<Option<u64>>,
    /// `v1.3.0`: layered configuration view, refreshed by `/config set`.
    pub(crate) resolved_config: Mutex<Option<crate::config_file::ResolvedConfig>>,
    /// Capability gap #4 (`v1.5.2`): bounded, persistent registry of durable async subagent tasks,
    /// exposed to the control plane through the `tasks.list` / `tasks.control` RPC methods. Shared
    /// with [`DurableTaskHost`] so the records reflect real spawns.
    pub(crate) async_tasks: Arc<Mutex<crate::async_tasks::AsyncTaskRegistry>>,
    /// Capability gap #4 (`v1.5.2`): the executable durable-task host that registers children at
    /// spawn, settles their spend/artifact/notification, and can cancel/resume them for real.
    pub(crate) durable_tasks: Arc<crate::task_host::DurableTaskHost>,
    /// `v1.5.0`: active terminal theme (`/theme zen|classic`); Design D (`Zen`) is the default.
    pub(crate) active_theme: Mutex<Theme>,
    /// B01 (test only): a two-phase barrier that lets a unit test hold a run between the in-flight
    /// gate and `claim_run`, so cancel-before-claim ordering is deterministic. Never compiled
    /// outside test builds.
    #[cfg(test)]
    pub(crate) pre_claim_gate: Mutex<Option<Arc<PreClaimGate>>>,
}

/// B01 (test only): see [`RuntimeEngineInner::pre_claim_gate`].
#[cfg(test)]
pub(crate) struct PreClaimGate {
    reached: Arc<tokio::sync::Notify>,
    proceed: Arc<tokio::sync::Notify>,
}

#[cfg(test)]
impl PreClaimGate {
    pub(crate) fn new() -> Self {
        Self {
            reached: Arc::new(tokio::sync::Notify::new()),
            proceed: Arc::new(tokio::sync::Notify::new()),
        }
    }

    /// Signal that a run has reached the gate, then wait for the test to release it.
    pub(crate) async fn hold(&self) {
        self.reached.notify_one();
        self.proceed.notified().await;
    }

    /// Resolves once a run has reached the pre-claim gate.
    pub(crate) async fn wait_reached(&self) {
        self.reached.notified().await;
    }

    /// Lets the held run proceed.
    pub(crate) fn release(&self) {
        self.proceed.notify_one();
    }
}

/// B01: the run currently active on a dispatcher. `owner` is the daemon connection that started
/// it, when the run came from a stream; `run_id` identifies this specific run so a late guard does
/// not clear a newer run's slot.
pub(crate) struct ActiveRun {
    pub(crate) run_id: u64,
    pub(crate) owner: Option<ConnectionId>,
    pub(crate) tx: watch::Sender<bool>,
    pub(crate) rx: watch::Receiver<bool>,
}

/// B01: releases a run's ownership slot and (for prompt runs) the dispatcher's in-flight gate when
/// the run ends. The ownership clear is identity-checked so a late guard cannot erase a newer run,
/// and the ACP sink is cleared only when this run still owns it.
pub(crate) struct RunGuard {
    pub(crate) inner: Arc<RuntimeEngineInner>,
    pub(crate) run_id: u64,
    pub(crate) in_flight: bool,
    pub(crate) acp_owner: Option<u64>,
}

impl Drop for RunGuard {
    fn drop(&mut self) {
        // Clear the ownership slot first (identity-checked), then release the in-flight gate, so a
        // run that starts after this one cannot have its slot cleared by this guard. The ACP sink
        // is cleared inside the same critical section, serialised with a new claim's enable.
        {
            let mut guard = self
                .inner
                .active_run
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if guard.as_ref().map(|r| r.run_id) == Some(self.run_id) {
                *guard = None;
                if let Some(owner) = self.acp_owner {
                    self.inner
                        .approval_broker
                        .disable_acp_permissions_for(owner);
                }
            }
        }
        if self.in_flight {
            self.inner.in_flight.store(false, Ordering::SeqCst);
        }
    }
}
