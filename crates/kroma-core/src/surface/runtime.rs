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

pub(crate) struct RuntimeEngineInner {
pub(crate) config: KromaRuntimeConfig,
pub(crate) provider: Mutex<Arc<dyn ModelProvider>>,
pub(crate) env: Arc<LocalHostEnv>,
pub(crate) snapshot_store: Arc<SnapshotStore>,
pub(crate) registry: ToolRegistry,
pub(crate) policy_engine: PolicyEngine,
pub(crate) approval_broker: Arc<RpcApprovalBroker>,
pub(crate) workspace_skills: Vec<SkillEntry>,
pub(crate) mounted_extensions: Vec<ExtensionId>,
pub(crate) status: Mutex<SessionStatusSnapshot>,
pub(crate) active_cancel: Mutex<Option<(watch::Sender<bool>, watch::Receiver<bool>)>>,
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
pub(crate) active_projection_profile: Mutex<Option<crate::context_projector::ProjectionProfile>>,
    /// Item #3 (`v1.4.0`): active adaptive-tournament candidate count (`0` = off).
pub(crate) active_adaptive_tournament: Mutex<usize>,
    /// `v1.3.0`: layered configuration view, refreshed by `/config set`.
pub(crate) resolved_config: Mutex<Option<crate::config_file::ResolvedConfig>>,
    /// `v1.5.0`: active terminal theme (`/theme zen|classic`); Design D (`Zen`) is the default.
pub(crate) active_theme: Mutex<Theme>,
}

pub(crate) struct InFlightPromptGuard {
pub(crate) inner: Arc<RuntimeEngineInner>,
}

impl Drop for InFlightPromptGuard {
    fn drop(&mut self) {
        self.inner.in_flight.store(false, Ordering::SeqCst);
        *self.inner.active_cancel.lock().unwrap() = None;
    }
}
