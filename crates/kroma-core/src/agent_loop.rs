use crate::attribution::{
    AttributionCandidate, AttributionEngine, AttributionReport, CounterfactualProbeConfig,
    CounterfactualProbeOutcome, DenyProbes, MutationLedger, MutationTelemetry, ProbeApproval,
    ProbeRequest,
};
use crate::budget::{BudgetConfig, BudgetTracker, RunDeadline, TokenBudget, TokenReservation};
use crate::completion_gate::{failure_fingerprint, WorkspaceCompletionGate};
use crate::context_projector::{source_entry_id, verify_tool_call_pairing, ContextProjector};
use crate::env::{
    AllowAllGate, CompletionGate, CompletionGateContext, ExecutionContext, GateOutcome,
    SharedCompletionGate, SharedExecutionEnv,
};
use crate::events::{EventLogger, EventPayload};
use crate::extensions::{
    is_subtree_context_tool, HookContext, HookEvaluation, SubtreeContextTracker,
};
use crate::loop_guard::{LoopGuard, LoopGuardVerdict};
use crate::messages::{
    ToolCall, ToolOutcome, ToolResultMessage, ToolResultMeta, TranscriptItem, UserMessage,
};
use crate::prompt::PromptBuilder;
use crate::provider::{validate_assistant_turn_batch, ModelProvider, ProviderRequest};
use crate::revision::{RevisionId, SnapshotStore};
use crate::session::{FileTouch, SessionJournal};
use crate::subagents::{
    build_worker_mutation_spans, parse_spawn_subagent_specs, parse_tournament_request,
    render_subagent_batch_report, run_worktree_tournament, IsolatedWorktree,
    ProviderSubagentBackend, SubagentBackend, SubagentExecutionRequest, SubagentResult,
    SubagentRole, SubagentRunOutcome, TournamentCandidate, TournamentRequest, TournamentSpec,
    WorktreeMergeResult, DEFAULT_MAX_SUBAGENT_SPAWNS,
};
use crate::tool::policy::{
    canonical_arguments_digest, ApprovalDecision, ApprovalRequest, AuthEntry, AuthScope,
    GrantRejection, PolicyCheckResult,
};
use crate::tool::runtime::ToolRuntime;
use crate::tools::recall_evidence::execute_recall_evidence;
use crate::tools::ToolExecutionOutput;
use crate::types::{EffectClass, ErrorCode, RunId, RunStatus, TerminalReason};
use sha2::Digest;
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::{mpsc, watch, Semaphore};

pub const READ_ONLY_CONCURRENCY_LIMIT: usize = 8;

/// Gate 4: upper bound on `--predict-cmd` executions per run, so an optional early-check command
/// cannot multiply the cost of a long mutating session without bound.
pub const MAX_PREDICT_CMD_RUNS: usize = 8;
/// Gate 4: number of ranked candidates surfaced in a failure prompt.
pub const ATTRIBUTION_TOP_K: usize = 3;

/// F08 (`v1.4.0`): rough token count for a byte length. Deliberately conservative (~4 bytes per
/// token) so a provider that omits `usage` still consumes the budget rather than bypassing it.
pub(crate) fn approx_tokens(bytes: usize) -> u64 {
    ((bytes as u64).saturating_add(3)) / 4
}

/// Estimated input tokens for one model request: the static prefix plus the projected message
/// history.
///
/// BUG-09: `static_prefix_bytes` already includes `system_prompt.len()` (see
/// `prompt.rs::build_static_prefix`), so the system prompt must not be counted a second time here —
/// doing so would trip `max_total_tokens` early on a long system prompt.
/// T03: bytes reserved for JSON framing, role markers and tool-call scaffolding that belong to no
/// single string in the request.
pub const REQUEST_SERIALIZATION_OVERHEAD_BYTES: usize = 1024;

/// T04 (KR-02): record a settlement that pushed the shared ledger past its limit.
///
/// The overrun is *not* truncated to fit the budget: it is charged, labelled with its basis, and
/// audited. No further request can be reserved afterwards, so the run stops without dispatching
/// anything else.
fn audit_token_settlement(logger: &mut EventLogger, record: &crate::budget::SettlementRecord) {
    if record.budget_overage == 0 {
        return;
    }
    let _ = logger.log(EventPayload::TokenBudgetOverage {
        settlement: format!("{:?}", record.settlement),
        reserved: record.reserved,
        charged: record.charged,
        spent: record.spent_after,
        limit: record.spent_after.saturating_sub(record.budget_overage),
        overage: record.budget_overage,
    });
}

/// T03 (KR-03): bytes of the canonical history. On a `--resume` the projector has not yet absorbed
/// the restored transcript when the pre-send estimate is taken, so summing the canonical items (not
/// the already-projected ones) is what keeps a resumed run from sending an over-window request.
fn canonical_history_bytes(items: &[TranscriptItem]) -> usize {
    items.iter().map(TranscriptItem::estimated_bytes).sum()
}

pub(crate) fn estimate_prompt_input_tokens(
    static_prefix_bytes: usize,
    message_history_bytes: usize,
) -> u64 {
    approx_tokens(static_prefix_bytes.saturating_add(message_history_bytes))
}

/// Account for wire attempts that were actually transmitted. The first attempt is recorded before
/// dispatch, so only the remainder is added here; a saturated cap simply stops recording.
fn record_wire_attempts(tracker: &mut crate::budget::BudgetTracker, attempts: usize) {
    for _ in 1..attempts {
        if !tracker.try_record_request_attempt() {
            break;
        }
    }
}

#[derive(Debug, Clone)]
pub struct RunSummary {
    pub run_id: RunId,
    pub status: RunStatus,
    pub terminal_reason: TerminalReason,
    pub turns_used: usize,
    pub request_attempts_used: usize,
    pub tool_calls_used: usize,
    pub total_input_tokens: u64,
    pub total_cached_input_tokens: u64,
    pub total_output_tokens: u64,
    /// F10 (`v1.4.0`): input/output tokens spent by child subagents and tournament candidates
    /// (including failed/cancelled ones), tracked separately from the parent's own spend.
    pub children_input_tokens: u64,
    /// B16: the cached part of `children_input_tokens`, kept separate so a priced report can use the
    /// cached rate where the operator's table distinguishes it.
    pub children_cached_input_tokens: u64,
    /// B16: `provider_reported` | `byte_estimate_not_measured` | `mixed` | `unknown`. A consumer
    /// must not treat an estimated total as measured provider usage.
    pub usage_basis: String,
    pub children_output_tokens: u64,
    pub cache_hit_ratio: f64,
    pub compaction_reread_count: u64,
    pub modified_files: Vec<String>,
    pub final_assistant_text: Option<String>,
    pub log_degraded: bool,
    pub dropped_events: usize,
    pub transcript: Vec<TranscriptItem>,
}

pub struct AgentRunConfig {
    pub run_id: RunId,
    pub task: String,
    pub budget: BudgetConfig,
    pub stream: bool,
    pub allow_tool_calls_on_stop: bool,
    pub strict_log: bool,
    /// B2 (`v1.4.0`): context projection profile (`baseline`/`p1`/`p2`/`p3`), configurable via
    /// `config.toml` / `.kroma.toml` / `/config set projection_profile <p>`. `None` = default (`P2`).
    pub projection_profile: Option<crate::context_projector::ProjectionProfile>,
    /// Capability gap #3 (`v1.5.2`): opt-in causal citation pinning + dead-end negative-sample
    /// compression. Off by default; the paired-benchmark ablation switch flips it per arm.
    pub causal_pruning: bool,
    /// Capability gap #6 (`v1.5.2`): when set, only the core tool set (plus `discover_tools`) is
    /// pinned in the initial request; every other tool schema is fetched on demand. Off by default.
    pub on_demand_tools: bool,
    /// Capability gap #4 (`v1.5.2`): the durable-task host, when present, records every child
    /// subagent this run spawns (state, spend, artifact, notification) so `tasks.control` reflects
    /// real work. `None` for library/test callers that do not host a control plane.
    pub durable_tasks: Option<Arc<crate::task_host::DurableTaskHost>>,
}

/// Capability gap #6 (`v1.5.2`): the tools pinned in the initial request under on-demand schema
/// mode. Every other policy-permitted tool is still dispatchable, but the model must fetch its
/// schema with `discover_tools` first.
pub const CORE_TOOL_NAMES: &[&str] = &[
    "activate_skill",
    "bash",
    "discover_tools",
    "edit",
    "find",
    "grep",
    "ls",
    "read",
    "recall_evidence",
    "write",
];

/// `true` when `name` belongs to the core tool set pinned under on-demand schema mode.
pub fn is_core_tool(name: &str) -> bool {
    CORE_TOOL_NAMES.contains(&name)
}

impl AgentRunConfig {
    pub fn new(task: impl Into<String>, budget: BudgetConfig) -> Self {
        Self {
            run_id: RunId::new(),
            task: task.into(),
            budget,
            stream: true,
            allow_tool_calls_on_stop: false,
            strict_log: false,
            projection_profile: None,
            causal_pruning: false,
            on_demand_tools: false,
            durable_tasks: None,
        }
    }

    /// Capability gap #4 (`v1.5.2`): attach the durable-task host that records child subagents.
    pub fn with_durable_tasks(
        mut self,
        host: Option<Arc<crate::task_host::DurableTaskHost>>,
    ) -> Self {
        self.durable_tasks = host;
        self
    }

    /// B2 (`v1.4.0`): select the context projection profile for this run.
    pub fn with_projection_profile(
        mut self,
        profile: Option<crate::context_projector::ProjectionProfile>,
    ) -> Self {
        self.projection_profile = profile;
        self
    }

    /// Capability gap #3 (`v1.5.2`): enable causal citation pinning and dead-end compression.
    pub fn with_causal_pruning(mut self, enabled: bool) -> Self {
        self.causal_pruning = enabled;
        self
    }

    /// Capability gap #6 (`v1.5.2`): pin only the core tool schemas and expose the rest through
    /// on-demand discovery (`discover_tools`).
    pub fn with_on_demand_tools(mut self, enabled: bool) -> Self {
        self.on_demand_tools = enabled;
        self
    }
}

pub struct AgentRunner {
    config: AgentRunConfig,
    provider: Arc<dyn ModelProvider>,
    tool_runtime: ToolRuntime,
    prompt_builder: PromptBuilder,
    completion_gate: SharedCompletionGate,
    cancel_rx: watch::Receiver<bool>,
    steer_rx: Option<mpsc::UnboundedReceiver<String>>,
    session_journal: Option<SessionJournal>,
    session_turn_offset: usize,
    session_persist_error: Option<String>,
    revision_error: Option<String>,
    completion_gate_max_retries: usize,
    snapshot_store: Option<Arc<SnapshotStore>>,
    current_revision_id: Option<RevisionId>,
    batch_mutated: bool,
    /// Set when the current tool batch executed a `bash` (`SideEffect`) call, whose file writes are
    /// invisible to the per-tool `modified_file` flag and must still be snapshotted (O1).
    batch_ran_side_effect: bool,
    /// Gate 3: ancestor `AGENTS.md`/`CLAUDE.md` directories already injected into this branch.
    subtree_tracker: SubtreeContextTracker,
    /// Gate 4: session-scoped, branch-aware record of every mutating operation.
    mutation_ledger: MutationLedger,
    /// Gate 4: optional fast command run after a mutating batch to surface failures early.
    predict_cmd: Option<Arc<WorkspaceCompletionGate>>,
    predict_cmd_runs: usize,
    /// Gate 5: backend driving child `Scout` / `Worker` subagent runs.
    subagent_backend: Arc<dyn SubagentBackend>,
    /// Gate 5: maximum number of subagents this session may spawn (`0` disables `spawn_subagent`).
    max_subagent_spawns: usize,
    subagents_spawned: usize,
    /// Gate 5: optional total token cap (input + output) enforced on child subagent runs.
    max_total_tokens: Option<u64>,
    /// F05 (`v1.5.1`): the shared token ledger for this run tree. The parent reserves from it before
    /// every provider request, and every parallel child subagent draws its own allocation from the
    /// same ledger, so a concurrent batch/tournament can never spend more than the run's cap.
    token_budget: Arc<TokenBudget>,
    /// F10 (`v1.4.0`): token spend accumulated by child subagents / tournament candidates during
    /// the current turn, drained into the parent [`BudgetTracker`] after each tool batch so the run
    /// summary can attribute `self` vs `children` cost.
    children_tokens_pending_input: u64,
    children_tokens_pending_output: u64,
    children_tokens_pending_cached: u64,
    children_usage_pending: Option<String>,
    experience: Option<Arc<std::sync::Mutex<crate::experience_runtime::ExperienceSession>>>,
    /// Item #3 (`v1.4.0`): when `>= 2`, a single agent that stalls on repeated verification
    /// failures automatically escalates to a Best-of-N worktree tournament with this many
    /// candidates. `0` disables escalation.
    adaptive_tournament_candidates: usize,
    /// Item #3: guards against escalating more than once per run.
    adaptive_tournament_attempted: bool,
    /// Capability gap #5 (`v1.5.2`): whether a repair race requires a candidate to have actually
    /// passed verification before it may merge. Strict by default, so a `Skipped` gate is never
    /// mistaken for a verified success.
    tournament_require_verified: bool,
    /// T01 (KR-01): host-owned protected globs for repair races. Task-level arguments can only add
    /// to this set, never remove from it.
    host_protected_paths: Vec<String>,
    /// T01 (KR-01): host-owned test-tampering protection. A task-level flag may enable it, never
    /// disable it.
    host_disallow_test_tampering: bool,
    /// T03 (KR-07): the resolved model capabilities for this run, with their provenance.
    model_capabilities: crate::budget::ModelCapabilities,
    /// Capability gap #2 (`v1.5.2`): opt-in isolated counterfactual probe configuration.
    counterfactual_probe: CounterfactualProbeConfig,
    /// Capability gap #2: approval seam; defaults to denying every probe.
    probe_approval: Arc<dyn ProbeApproval>,
    /// Capability gap #2: probes actually executed this run (bounded by the config).
    counterfactual_probes_run: usize,
    /// Capability gap #2: probes whose revert resolved the failure (precision numerator).
    counterfactual_probes_resolved: usize,
    /// F04 (`v1.5.1`): absolute deadline of this run, published by [`Self::prepare_run`] so phases
    /// that run outside the main loop (subagents, tournaments) can clamp to the same instant.
    run_deadline: Option<RunDeadline>,
}

/// Inputs for [`AgentRunner::run_subagent_batch`], grouped so the extracted phase takes a single
/// argument instead of the dozen locals it needs.
struct SubagentBatchPlan<'a> {
    prepared: Vec<(
        String,
        crate::subagents::SubagentSpec,
        Option<crate::subagents::IsolatedWorktree>,
    )>,
    subagents_state_base: std::path::PathBuf,
    workspace_skills: Vec<crate::extensions::SkillEntry>,
    parent_budget: BudgetConfig,
    parent_policy_mode: crate::tool::policy::PolicyMode,
    predictive_check: bool,
    store: Arc<SnapshotStore>,
    absolute_turn: usize,
    decision_label: String,
    logger: &'a mut EventLogger,
    modified_files_set: &'a mut HashSet<String>,
    started: std::time::Instant,
}

/// State produced by [`AgentRunner::prepare_run`] and consumed by the main loop.
///
/// Split out of `run_inner` (1104 lines): the fields are the locals the loop body needs,
/// so the body itself did not have to change.
struct TurnState {
    budget_tracker: BudgetTracker,
    visible_tools: Vec<crate::tool::definition::ToolDefinition>,
    static_prefix: crate::prompt::StaticPrefixMetadata,
    transcript: Vec<TranscriptItem>,
    projector: ContextProjector,
    loop_guard: LoopGuard,
    modified_files_set: HashSet<String>,
    last_input_tokens: Option<u64>,
    last_assistant_text: Option<String>,
    gate_failure_count: usize,
    last_gate_failure_hash: Option<String>,
    current_turn: usize,
    absolute_turn: usize,
    tool_calls: Vec<ToolCall>,
}

impl AgentRunner {
    pub fn new(
        config: AgentRunConfig,
        provider: Arc<dyn ModelProvider>,
        tool_runtime: ToolRuntime,
        cancel_rx: watch::Receiver<bool>,
    ) -> Self {
        let subagent_backend = Arc::new(ProviderSubagentBackend::new(Arc::clone(&provider)));
        // F05 (`v1.5.1`): seed the shared token ledger from the unified cap carried on the run's
        // `BudgetConfig` (set by the CLI, config file or RPC surface).
        let max_total_tokens = config.budget.max_total_tokens;
        // T03 (KR-07): resolve the model window once. When the operator did not configure one, a
        // conservative fallback applies and is pushed into the budget so the context projector
        // compacts against it instead of running unbounded until the provider refuses.
        let model_capabilities = crate::budget::ModelCapabilities::resolve(
            config.budget.model_context_window_tokens,
            None,
        );
        Self {
            config,
            provider,
            tool_runtime,
            prompt_builder: PromptBuilder::default(),
            completion_gate: Arc::new(AllowAllGate),
            cancel_rx,
            steer_rx: None,
            session_journal: None,
            session_turn_offset: 0,
            session_persist_error: None,
            revision_error: None,
            completion_gate_max_retries: 3,
            snapshot_store: None,
            current_revision_id: None,
            batch_mutated: false,
            batch_ran_side_effect: false,
            subtree_tracker: SubtreeContextTracker::default(),
            mutation_ledger: MutationLedger::new(),
            predict_cmd: None,
            predict_cmd_runs: 0,
            subagent_backend,
            max_subagent_spawns: DEFAULT_MAX_SUBAGENT_SPAWNS,
            subagents_spawned: 0,
            max_total_tokens,
            token_budget: Arc::new(TokenBudget::new(max_total_tokens)),
            children_tokens_pending_input: 0,
            children_tokens_pending_output: 0,
            children_tokens_pending_cached: 0,
            children_usage_pending: None,
            experience: None,
            adaptive_tournament_candidates: 0,
            adaptive_tournament_attempted: false,
            tournament_require_verified: true,
            host_protected_paths: Vec::new(),
            host_disallow_test_tampering: false,
            model_capabilities,
            counterfactual_probe: CounterfactualProbeConfig::disabled(),
            probe_approval: Arc::new(DenyProbes),
            counterfactual_probes_run: 0,
            counterfactual_probes_resolved: 0,
            run_deadline: None,
        }
    }

    pub fn with_completion_gate(mut self, gate: SharedCompletionGate) -> Self {
        self.completion_gate = gate;
        self
    }

    pub fn with_steer_receiver(mut self, rx: mpsc::UnboundedReceiver<String>) -> Self {
        self.steer_rx = Some(rx);
        self
    }

    pub fn with_session_journal(mut self, journal: SessionJournal) -> Self {
        // Gate 1: a resumed session must continue the revision chain the WAL already recorded, so
        // the next capture's `parent_revision_id` links to the workspace state on disk.
        self.current_revision_id = journal
            .latest_revision_id()
            .and_then(|id| RevisionId::parse(&id).ok());
        self.session_journal = Some(journal);
        self
    }

    /// Attach a workspace snapshot store so every mutating turn captures a `WorkspaceRevision`
    /// (Gate 1). Without a store the run behaves exactly as before.
    pub fn with_snapshot_store(mut self, store: Arc<SnapshotStore>) -> Self {
        self.snapshot_store = Some(store);
        self
    }

    pub fn with_completion_gate_max_retries(mut self, max_retries: usize) -> Self {
        self.completion_gate_max_retries = max_retries.max(1);
        self
    }

    /// Item #3 (`v1.4.0`): race this many candidates when the single agent stalls on repeated
    /// verification failures. Values below [`crate::subagents::MIN_TOURNAMENT_CANDIDATES`] disable
    /// escalation.
    pub fn with_adaptive_tournament_candidates(mut self, candidates: usize) -> Self {
        self.adaptive_tournament_candidates = candidates;
        self
    }

    /// Capability gap #5 (`v1.5.2`): require a repair-race winner to have actually passed
    /// verification. Defaults to `true`; set `false` to restore the legacy behaviour where a
    /// skipped gate was accepted. The host requirement cannot be weakened by a model argument.
    pub fn with_tournament_require_verified(mut self, require_verified: bool) -> Self {
        self.tournament_require_verified = require_verified;
        self
    }

    /// T01 (KR-01): install the host-owned protection set for repair races. Tool/RPC arguments are
    /// merged into this set and can only expand it.
    pub fn with_host_protected_paths(mut self, globs: Vec<String>) -> Self {
        self.host_protected_paths = globs;
        self
    }

    /// T01 (KR-01): host-owned test-tampering protection for repair races. A model argument may
    /// enable it, but never disable it.
    pub fn with_host_disallow_test_tampering(mut self, value: bool) -> Self {
        self.host_disallow_test_tampering = value;
        self
    }

    /// Capability gap #2 (`v1.5.2`): opt in to the isolated counterfactual probe. Disabled by
    /// default; even when enabled, `approval` must authorize each probe and the live workspace is
    /// never modified or rolled back.
    pub fn with_counterfactual_probe(
        mut self,
        config: CounterfactualProbeConfig,
        approval: Arc<dyn ProbeApproval>,
    ) -> Self {
        self.counterfactual_probe = config;
        self.probe_approval = approval;
        self
    }

    /// Capability gap #2: number of counterfactual probes actually executed this run.
    pub fn counterfactual_probes_run(&self) -> usize {
        self.counterfactual_probes_run
    }

    /// Capability gap #2: probes whose revert resolved the failure.
    pub fn counterfactual_probes_resolved(&self) -> usize {
        self.counterfactual_probes_resolved
    }

    /// Local-host experience decisions are shared so revocation/off is visible before each request.
    pub fn with_experience(
        mut self,
        experience: Arc<std::sync::Mutex<crate::experience_runtime::ExperienceSession>>,
    ) -> Self {
        self.experience = Some(experience);
        self
    }

    pub fn with_prompt_builder(mut self, pb: PromptBuilder) -> Self {
        self.prompt_builder = pb;
        self
    }

    /// Attach the Gate 3 subtree JIT context tracker (already seeded from the session checkpoint
    /// when the run resumes an existing branch).
    pub fn with_subtree_context(mut self, tracker: SubtreeContextTracker) -> Self {
        self.subtree_tracker = tracker;
        self
    }

    /// Attach the Gate 4 mutation ledger (used by tests and callers that build the ledger outside
    /// the session WAL). A `--resume` still re-seeds it from the branch checkpoint.
    pub fn with_mutation_ledger(mut self, ledger: MutationLedger) -> Self {
        self.mutation_ledger = ledger;
        self
    }

    /// Attach an optional `--predict-cmd` gate, run once per mutating batch (bounded by
    /// [`MAX_PREDICT_CMD_RUNS`]) to surface an early predictive failure plus its candidate
    /// attribution before the model claims completion.
    pub fn with_predictive_command(mut self, gate: Arc<WorkspaceCompletionGate>) -> Self {
        self.predict_cmd = Some(gate);
        self
    }

    pub fn mutation_ledger(&self) -> &MutationLedger {
        &self.mutation_ledger
    }

    /// Gate 5: attach a custom [`SubagentBackend`] for `spawn_subagent` calls.
    pub fn with_subagent_backend(mut self, backend: Arc<dyn SubagentBackend>) -> Self {
        self.subagent_backend = backend;
        self
    }

    /// Gate 5: attach a dedicated [`ModelProvider`] for child subagent runs.
    pub fn with_subagent_provider(mut self, provider: Arc<dyn ModelProvider>) -> Self {
        self.subagent_backend = Arc::new(ProviderSubagentBackend::new(provider));
        self
    }

    /// Gate 5: configure the maximum number of subagents this session may spawn (`0` disables
    /// `spawn_subagent` and hides it from the model-visible tool schema).
    pub fn with_max_subagents(mut self, max_subagents: usize) -> Self {
        self.max_subagent_spawns = max_subagents;
        self
    }

    /// P1 (`v1.5.0`): seed the cumulative subagent spawn count for this session.
    ///
    /// A surface tracks spawns it performs outside the agent loop (for example an RPC
    /// `session.tournament`). Seeding it here means a later prompt run measures its own
    /// `spawn_subagent` calls against the `max_subagents` quota already consumed instead of
    /// resetting to whatever the last WAL checkpoint happened to record.
    pub fn with_subagents_spawned(mut self, spawned: usize) -> Self {
        self.subagents_spawned = spawned;
        self
    }

    /// Gate 5: enforce a hard total-token ceiling (input + output) on this runner.
    ///
    /// F05 (`v1.5.1`): this is the runner's *own* ceiling. On a root runner it also becomes the
    /// shared ledger's limit, so child subagents inherit it. A child runner is instead given its
    /// allocation through [`Self::with_token_budget`], which must be called *after* this method.
    pub fn with_max_total_tokens(mut self, max_tokens: Option<u64>) -> Self {
        self.max_total_tokens = max_tokens;
        self.token_budget = Arc::new(TokenBudget::new(max_tokens));
        self
    }

    /// F05 (`v1.5.1`): draw this runner's requests from an existing shared ledger.
    ///
    /// Used for child subagents and tournament candidates: the parent reserves a per-child
    /// allocation from the run's ledger, then hands the child a ledger limited to that allocation.
    /// This overrides both the local ceiling and the ledger set by [`Self::with_max_total_tokens`],
    /// so it must be the last token-budget builder applied.
    pub fn with_token_budget(mut self, token_budget: Arc<TokenBudget>) -> Self {
        self.max_total_tokens = token_budget.limit();
        self.token_budget = token_budget;
        self
    }

    /// F05 (`v1.5.1`): the run tree's shared token ledger. Exposed so a surface can inspect spend
    /// and so tests can assert the cap is shared across parent and children.
    pub fn token_budget(&self) -> &Arc<TokenBudget> {
        &self.token_budget
    }

    /// Run the agent loop, then emit fail-open `RunCompleted` observer hooks (Gate 3).
    ///
    /// The observers run after the loop terminates on *every* exit path, because the wrapper sees
    /// the final summary rather than the ~30 internal `finish_run` returns.
    pub async fn run(self, logger: EventLogger) -> (RunSummary, EventLogger) {
        let hooks = self.tool_runtime.hook_engine();
        let workspace_root = self.tool_runtime.env().workspace().root().to_path_buf();
        let (summary, mut logger) = self.run_inner(logger).await;
        if !hooks.is_empty() {
            let ctx = HookContext::new(summary.turns_used, workspace_root);
            for evaluation in hooks
                .run_observers(crate::extensions::HookEvent::RunCompleted, &ctx)
                .await
            {
                let _ = logger.log(EventPayload::HookEvaluated {
                    hook_id: evaluation.hook_id,
                    hook_kind: evaluation.hook_kind,
                    event: evaluation.event,
                    outcome: evaluation.outcome,
                    duration_ms: evaluation.duration_ms,
                    message: evaluation.message,
                });
            }
        }
        (summary, logger)
    }

    async fn run_inner(mut self, logger: EventLogger) -> (RunSummary, EventLogger) {
        let (mut state, mut logger) = match self.prepare_run(logger).await {
            Ok(value) => value,
            Err(outcome) => return outcome,
        };
        loop {
            logger = match self.run_provider_turn(&mut state, logger).await {
                Ok(returned) => returned,
                Err(outcome) => return outcome,
            };
            if state.tool_calls.is_empty() {
                logger = match self.handle_completion_turn(&mut state, logger).await {
                    Ok(returned) => returned,
                    Err(outcome) => return outcome,
                };
                continue;
            }
            logger = match self.handle_tool_batch_turn(&mut state, logger).await {
                Ok(returned) => returned,
                Err(outcome) => return outcome,
            };
        }
    }

    /// Run the tool batch the model asked for, bind the resulting revision and mutations, run the
    /// optional post-batch prediction, drain mid-run steering and emit the TurnCompleted observers.
    ///
    /// Split out of `run_inner`'s loop body (240 of its 560 lines). Returns `logger` on success or
    /// the finished run.
    async fn handle_tool_batch_turn(
        &mut self,
        state: &mut TurnState,
        mut logger: EventLogger,
    ) -> Result<EventLogger, (RunSummary, EventLogger)> {
        if let Err(e) = logger.log(EventPayload::ToolBatchReceived {
            turn: state.current_turn,
            call_count: state.tool_calls.len(),
            call_ids: state
                .tool_calls
                .iter()
                .map(|tc| tc.call_id.clone())
                .collect(),
        }) {
            self.synthesize_remaining_results(
                &state.tool_calls,
                ErrorCode::NotExecutedCancelled,
                "NOT_EXECUTED_CANCELLED: strict log failure",
                &mut state.transcript,
                state.current_turn,
            );
            return Err(self.finish_run(
                TerminalReason::StrictLogFailure(e.to_string()),
                &state.budget_tracker,
                &state.projector,
                std::mem::take(&mut state.modified_files_set)
                    .into_iter()
                    .collect(),
                state.last_assistant_text.take(),
                std::mem::take(&mut state.transcript),
                logger,
            ));
        }

        // Execute tool batch with Read-Only Parallel Sub-Batches (`Semaphore(8)`) + Mutating Barriers,
        // guaranteeing 1:1 `tool_call_id` closure in exact index order (§4.1 V6-06).
        self.batch_mutated = false;
        self.batch_ran_side_effect = false;
        let batch_abort = self
            .execute_tool_batch(
                &state.tool_calls,
                &mut state.transcript,
                &mut state.budget_tracker,
                &mut state.loop_guard,
                &mut state.projector,
                &mut state.modified_files_set,
                &mut logger,
            )
            .await;

        debug_assert!(verify_tool_call_pairing(&state.transcript));

        // F10 (`v1.4.0`): fold any child subagent / tournament token spend accumulated during
        // this batch into the parent budget tracker so the run summary reports
        // `self` + `children` cost.
        self.drain_children_tokens(&mut state.budget_tracker);

        // Gate 1: a batch that mutated files — directly via `edit`/`write`, or indirectly via a
        // `bash` command whose writes the tool cannot report (O1) — gets a fresh revision bound
        // to this turn.
        let batch_changed_paths = if self.batch_mutated || self.batch_ran_side_effect {
            self.capture_workspace_revision(state.absolute_turn, "mutation", &mut logger)
        } else {
            Vec::new()
        };
        for path in &batch_changed_paths {
            state.modified_files_set.insert(path.clone());
            self.batch_mutated = true;
        }
        // Gate 4: bind the post-batch revision (and any `changed_paths` discovered by the
        // snapshot store for `bash` / side-effect extension calls) to every mutation recorded
        // during this batch, dropping read-only `bash` records that changed 0 files.
        self.mutation_ledger.backfill_revision_and_paths(
            state.absolute_turn as u32,
            self.current_revision_id.clone(),
            &batch_changed_paths,
            self.snapshot_store.is_some() && (self.batch_mutated || self.batch_ran_side_effect),
        );
        if let Some(err) = self.revision_error.clone() {
            return Err(self.finish_run(
                TerminalReason::StrictLogFailure(err),
                &state.budget_tracker,
                &state.projector,
                std::mem::take(&mut state.modified_files_set)
                    .into_iter()
                    .collect(),
                state.last_assistant_text.take(),
                std::mem::take(&mut state.transcript),
                logger,
            ));
        }

        if let Some(err) = self.session_persist_error.clone() {
            return Err(self.finish_run(
                TerminalReason::StrictLogFailure(format!("session WAL append failed: {}", err)),
                &state.budget_tracker,
                &state.projector,
                std::mem::take(&mut state.modified_files_set)
                    .into_iter()
                    .collect(),
                state.last_assistant_text.take(),
                std::mem::take(&mut state.transcript),
                logger,
            ));
        }

        // Gate 3: checkpoint the post-batch leaf so the `subtree_dirs` (and pinned baseline /
        // last-touched ledger) loaded by *this* batch survive a later `--resume` / `--fork`,
        // even when the run ends here (`MaxTurns`, `MaxToolCalls`, `LoopDetected`, cancellation).
        self.persist_session_checkpoint(&state.projector, &state.transcript, state.current_turn);
        if let Some(err) = self.session_persist_error.clone() {
            return Err(self.finish_run(
                TerminalReason::StrictLogFailure(format!("session checkpoint failed: {}", err)),
                &state.budget_tracker,
                &state.projector,
                std::mem::take(&mut state.modified_files_set)
                    .into_iter()
                    .collect(),
                state.last_assistant_text.take(),
                std::mem::take(&mut state.transcript),
                logger,
            ));
        }

        if let Some(abort_reason) = batch_abort {
            let mut mod_list: Vec<String> = std::mem::take(&mut state.modified_files_set)
                .into_iter()
                .collect();
            mod_list.sort();
            return Err(self.finish_run(
                abort_reason,
                &state.budget_tracker,
                &state.projector,
                mod_list,
                state.last_assistant_text.take(),
                std::mem::take(&mut state.transcript),
                logger,
            ));
        }

        // F04 (`v1.5.1`): a tool can consume the last of the run's wall-clock time (or finish a
        // process group right at the deadline) without the pre-dispatch check in
        // `execute_tool_batch` seeing it. The mutation snapshot above has already run, so the
        // deadline stop is still consistent with what is on disk.
        if state.budget_tracker.is_deadline_exceeded() {
            let mut mod_list: Vec<String> = std::mem::take(&mut state.modified_files_set)
                .into_iter()
                .collect();
            mod_list.sort();
            return Err(self.finish_run(
                TerminalReason::RunDeadlineExceeded,
                &state.budget_tracker,
                &state.projector,
                mod_list,
                state.last_assistant_text.take(),
                std::mem::take(&mut state.transcript),
                logger,
            ));
        }

        // Gate 4: optional `--predict-cmd`. Run once per mutating batch (bounded) so a fast
        // syntax/type check can surface an early failure — with its candidate attribution —
        // before the model spends the rest of the budget on a broken tree.
        let mut post_batch_updated = false;
        if (self.batch_mutated || self.batch_ran_side_effect)
            && self.tool_runtime.predictive_check_enabled()
            && self.tool_runtime.policy_engine().mode() != crate::tool::policy::PolicyMode::ReadOnly
            && self.predict_cmd.is_some()
            && self.predict_cmd_runs < MAX_PREDICT_CMD_RUNS
        {
            self.predict_cmd_runs += 1;
            let gate = self.predict_cmd.clone().expect("checked above");
            let mut mod_list: Vec<String> = state.modified_files_set.iter().cloned().collect();
            mod_list.sort();
            let ctx = CompletionGateContext {
                run_id: self.config.run_id,
                workspace_root: self.tool_runtime.env().workspace().root().to_path_buf(),
                modified_files: mod_list,
                turns_used: state.current_turn,
                deadline: Some(state.budget_tracker.deadline()),
            };
            // F04: the predictive check is clamped to the run deadline; the gate itself clamps its
            // own process timeout via `ctx.deadline`, and this outer bound is a backstop.
            let check_res = match tokio::time::timeout_at(
                state.budget_tracker.deadline().tokio_instant(),
                gate.check(&ctx),
            )
            .await
            {
                Ok(res) => res,
                Err(_) => Ok(GateOutcome::Rejected {
                    feedback: "run deadline exceeded during --predict-cmd".to_string(),
                }),
            };
            let predict_changed =
                self.capture_workspace_revision(state.absolute_turn, "predict_cmd", &mut logger);
            if !predict_changed.is_empty() {
                state.modified_files_set.extend(predict_changed);
                post_batch_updated = true;
            }
            if let Some(err) = self.revision_error.clone() {
                return Err(self.finish_run(
                    TerminalReason::StrictLogFailure(err),
                    &state.budget_tracker,
                    &state.projector,
                    std::mem::take(&mut state.modified_files_set)
                        .into_iter()
                        .collect(),
                    state.last_assistant_text.take(),
                    std::mem::take(&mut state.transcript),
                    logger,
                ));
            }
            if let Ok(GateOutcome::Rejected { feedback }) = check_res {
                let report = AttributionEngine::attribute_failure(
                    &self.mutation_ledger,
                    &feedback,
                    "predict_cmd",
                    ATTRIBUTION_TOP_K,
                );
                let _ = logger.log(EventPayload::PredictiveCheck {
                    path: report
                        .top()
                        .and_then(|c| c.relative_path.clone())
                        .unwrap_or_else(|| "<run>".to_string()),
                    status: "warning".to_string(),
                    issue_count: report.anchors.len(),
                    duration_ms: 0,
                    summary: Some(format!(
                        "--predict-cmd failed on attempt {}/{}",
                        self.predict_cmd_runs, MAX_PREDICT_CMD_RUNS
                    )),
                });
                Self::log_failure_attribution(&mut logger, "predict_cmd", &report);
                let mut message = format!(
                    "[Predictive check failed, attempt {}/{}]: {}\nFix this now; the failure is cheaper to resolve before more edits.",
                    self.predict_cmd_runs, MAX_PREDICT_CMD_RUNS, feedback
                );
                if let Some(block) = report.render_for_model() {
                    message.push_str("\n\n");
                    message.push_str(&block);
                }
                self.record_transcript_item(
                    &mut state.transcript,
                    TranscriptItem::User(UserMessage { content: message }),
                    state.current_turn.saturating_add(1),
                );
                post_batch_updated = true;
                if let Some(err) = self.session_persist_error.clone() {
                    return Err(self.finish_run(
                        TerminalReason::StrictLogFailure(format!(
                            "session WAL append failed: {}",
                            err
                        )),
                        &state.budget_tracker,
                        &state.projector,
                        std::mem::take(&mut state.modified_files_set)
                            .into_iter()
                            .collect(),
                        state.last_assistant_text.take(),
                        std::mem::take(&mut state.transcript),
                        logger,
                    ));
                }
            }
        }

        // §3.2 V6-07: Mid-Run Steering — non-blocking check on `steer_rx.try_recv()` after 1:1 tool closure
        let pre_steer_len = state.transcript.len();
        self.drain_steering_messages(&mut state.transcript, state.current_turn.saturating_add(1));
        if state.transcript.len() != pre_steer_len {
            post_batch_updated = true;
        }
        if let Some(err) = self.session_persist_error.clone() {
            return Err(self.finish_run(
                TerminalReason::StrictLogFailure(format!("session WAL append failed: {}", err)),
                &state.budget_tracker,
                &state.projector,
                std::mem::take(&mut state.modified_files_set)
                    .into_iter()
                    .collect(),
                state.last_assistant_text.take(),
                std::mem::take(&mut state.transcript),
                logger,
            ));
        }
        if post_batch_updated {
            self.persist_session_checkpoint(
                &state.projector,
                &state.transcript,
                state.current_turn,
            );
            if let Some(err) = self.session_persist_error.clone() {
                return Err(self.finish_run(
                    TerminalReason::StrictLogFailure(format!("session checkpoint failed: {}", err)),
                    &state.budget_tracker,
                    &state.projector,
                    std::mem::take(&mut state.modified_files_set)
                        .into_iter()
                        .collect(),
                    state.last_assistant_text.take(),
                    std::mem::take(&mut state.transcript),
                    logger,
                ));
            }
        }

        // Gate 3: `TurnCompleted` observers are fail-open telemetry and never affect control flow.
        {
            let hooks = self.tool_runtime.hook_engine();
            if !hooks.is_empty() {
                let ctx = HookContext::new(
                    state.current_turn,
                    self.tool_runtime.env().workspace().root().to_path_buf(),
                );
                for evaluation in hooks
                    .run_observers(crate::extensions::HookEvent::TurnCompleted, &ctx)
                    .await
                {
                    let _ = logger.log(EventPayload::HookEvaluated {
                        hook_id: evaluation.hook_id,
                        hook_kind: evaluation.hook_kind,
                        event: evaluation.event,
                        outcome: evaluation.outcome,
                        duration_ms: evaluation.duration_ms,
                        message: evaluation.message,
                    });
                }
            }
        }

        Ok(logger)
    }

    /// Run one provider turn: preflight budget/cancellation, project context, persist the
    /// checkpoint, call the provider and record the assistant turn plus its tool calls.
    ///
    /// Split out of `run_inner`'s loop body (322 of its 560 lines). Returns `logger` on success
    /// (it is consumed by `finish_run` on the failure paths) or the finished run.
    async fn run_provider_turn(
        &mut self,
        state: &mut TurnState,
        mut logger: EventLogger,
    ) -> Result<EventLogger, (RunSummary, EventLogger)> {
        if *self.cancel_rx.borrow() {
            return Err(self.finish_run(
                TerminalReason::UserCancelled,
                &state.budget_tracker,
                &state.projector,
                std::mem::take(&mut state.modified_files_set)
                    .into_iter()
                    .collect(),
                state.last_assistant_text.take(),
                std::mem::take(&mut state.transcript),
                logger,
            ));
        }

        if state.budget_tracker.is_deadline_exceeded() {
            return Err(self.finish_run(
                TerminalReason::RunDeadlineExceeded,
                &state.budget_tracker,
                &state.projector,
                std::mem::take(&mut state.modified_files_set)
                    .into_iter()
                    .collect(),
                state.last_assistant_text.take(),
                std::mem::take(&mut state.transcript),
                logger,
            ));
        }

        // F08 (`v1.4.0`): budget accounting covers `self` *and* child subagent spend, and the
        // stop reason is a dedicated `TokenBudgetExhausted` (not `MaxTurns`).
        if let Some(max_tok) = self.max_total_tokens {
            if state.budget_tracker.all_total_tokens() >= max_tok {
                return Err(self.finish_run(
                    TerminalReason::TokenBudgetExhausted,
                    &state.budget_tracker,
                    &state.projector,
                    std::mem::take(&mut state.modified_files_set)
                        .into_iter()
                        .collect(),
                    state.last_assistant_text.take(),
                    std::mem::take(&mut state.transcript),
                    logger,
                ));
            }
        }

        if !state.budget_tracker.try_begin_turn() {
            return Err(self.finish_run(
                TerminalReason::MaxTurns,
                &state.budget_tracker,
                &state.projector,
                std::mem::take(&mut state.modified_files_set)
                    .into_iter()
                    .collect(),
                state.last_assistant_text.take(),
                std::mem::take(&mut state.transcript),
                logger,
            ));
        }

        state.current_turn = state.budget_tracker.turns_used();
        // `current_turn` is relative to this process; the projector's turn accounting is over
        // the whole session transcript, so it must receive the absolute session turn. On a
        // `--resume` run `current_turn` starts at 1 again while the restored transcript already
        // holds `session_turn_offset` turns, and feeding the relative turn here would make every
        // pre-existing hot file look "0 turns old" and pin it forever.
        state.absolute_turn = self.session_turn_offset.saturating_add(state.current_turn);

        // T03 (KR-03): when the provider has not reported usage yet (first turn, or a `--resume`
        // with no restored usage), the token watermarks have nothing to compare against and a small
        // window would only be discovered *after* an over-window request was already sent. Feed a
        // conservative estimate of this request instead. Provider-reported usage, once present,
        // wins: it calibrates the estimate rather than being replaced by it.
        let pressure_tokens = match (
            state.last_input_tokens,
            self.model_capabilities.context_window_tokens,
        ) {
            (Some(measured), _) => Some(measured),
            (None, Some(_)) => Some(crate::budget::conservative_bytes_tokens(
                state
                    .static_prefix
                    .static_prefix_bytes
                    .saturating_add(canonical_history_bytes(&state.transcript))
                    .saturating_add(REQUEST_SERIALIZATION_OVERHEAD_BYTES),
            )),
            (None, None) => None,
        };

        // Project context (100% Append-Only unless watermark crossed)
        let mut projected = match state.projector.project(
            &mut state.transcript,
            state.budget_tracker.config(),
            state.static_prefix.static_prefix_bytes,
            pressure_tokens,
            state.absolute_turn,
        ) {
            Ok(p) => p,
            Err(term_reason) => {
                return Err(self.finish_run(
                    term_reason,
                    &state.budget_tracker,
                    &state.projector,
                    std::mem::take(&mut state.modified_files_set)
                        .into_iter()
                        .collect(),
                    state.last_assistant_text.take(),
                    std::mem::take(&mut state.transcript),
                    logger,
                ));
            }
        };

        if let Some(comp) = projected.compaction_event {
            if let Err(e) = logger.log(EventPayload::ContextCompacted {
                before_bytes: comp.before_bytes,
                after_bytes: comp.after_bytes,
                compacted_seq_indices: comp.compacted_seq_indices,
                pinned_files: comp.pinned_files,
            }) {
                return Err(self.finish_run(
                    TerminalReason::StrictLogFailure(e.to_string()),
                    &state.budget_tracker,
                    &state.projector,
                    std::mem::take(&mut state.modified_files_set)
                        .into_iter()
                        .collect(),
                    state.last_assistant_text.take(),
                    std::mem::take(&mut state.transcript),
                    logger,
                ));
            }
        }

        self.persist_session_checkpoint(&state.projector, &state.transcript, state.current_turn);
        if let Some(err) = self.session_persist_error.clone() {
            return Err(self.finish_run(
                TerminalReason::StrictLogFailure(format!("session checkpoint failed: {}", err)),
                &state.budget_tracker,
                &state.projector,
                std::mem::take(&mut state.modified_files_set)
                    .into_iter()
                    .collect(),
                state.last_assistant_text.take(),
                std::mem::take(&mut state.transcript),
                logger,
            ));
        }

        if !state.budget_tracker.try_record_request_attempt() {
            return Err(self.finish_run(
                TerminalReason::MaxRequestAttempts,
                &state.budget_tracker,
                &state.projector,
                std::mem::take(&mut state.modified_files_set)
                    .into_iter()
                    .collect(),
                state.last_assistant_text.take(),
                std::mem::take(&mut state.transcript),
                logger,
            ));
        }

        // Experience is ephemeral request context: it never changes the frozen prefix or session
        // transcript. Re-check live tracked bytes per request so stale status cannot authorize it.
        if let (Some(experience), Some(store)) = (&self.experience, &self.snapshot_store) {
            let workspace = self.tool_runtime.env().workspace();
            let block = {
                let experience = experience.lock().unwrap_or_else(|e| e.into_inner());
                if experience.enabled() {
                    let revision = crate::experience_runtime::live_revision(store, workspace).ok();
                    experience.injection(
                        workspace,
                        revision.as_ref(),
                        self.tool_runtime.policy_engine().mode(),
                    )
                } else {
                    String::new()
                }
            };
            if !block.is_empty() {
                projected.message_history_bytes =
                    projected.message_history_bytes.saturating_add(block.len());
                projected
                    .messages
                    .push(TranscriptItem::User(UserMessage { content: block }));
            }
        }

        if let Err(e) = logger.log(EventPayload::ModelRequestStarted {
            turn: state.current_turn,
            attempt: state.budget_tracker.request_attempts_used(),
            request_body_bytes: state.static_prefix.static_prefix_bytes
                + projected.message_history_bytes,
            message_history_bytes: projected.message_history_bytes,
        }) {
            return Err(self.finish_run(
                TerminalReason::StrictLogFailure(e.to_string()),
                &state.budget_tracker,
                &state.projector,
                std::mem::take(&mut state.modified_files_set)
                    .into_iter()
                    .collect(),
                state.last_assistant_text.take(),
                std::mem::take(&mut state.transcript),
                logger,
            ));
        }

        // F08 (`v1.4.0`): reserve budget *before* sending the request. Estimate the prompt
        // input tokens from the projected byte size and refuse to start a request that would
        // already blow the remaining budget; otherwise cap this request's output so a single
        // completion cannot overshoot what is left.
        // `static_prefix_bytes` already includes `system_prompt.len()` (see
        // `prompt.rs::build_static_prefix`), so it must not be added again here.
        let estimated_input_tokens = estimate_prompt_input_tokens(
            state.static_prefix.static_prefix_bytes,
            projected.message_history_bytes,
        );
        // F05 (`v1.5.1`): the token cap is enforced through the shared [`TokenBudget`], not a
        // per-run counter, so the parent and every concurrent child subagent draw from the same
        // balance. The runner's own ceiling (`max_total_tokens`) still applies — for a child that
        // is the allocation its parent reserved — and the tighter of the two wins.
        let local_remaining = self
            .max_total_tokens
            .map(|max_tok| max_tok.saturating_sub(state.budget_tracker.all_total_tokens()));
        let allowed = match (local_remaining, self.token_budget.remaining()) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        };
        let run_output_allowance = match allowed {
            Some(remaining) if remaining > estimated_input_tokens => {
                Some(remaining - estimated_input_tokens)
            }
            Some(_) => {
                return Err(self.finish_run(
                    TerminalReason::TokenBudgetExhausted,
                    &state.budget_tracker,
                    &state.projector,
                    std::mem::take(&mut state.modified_files_set)
                        .into_iter()
                        .collect(),
                    state.last_assistant_text.take(),
                    std::mem::take(&mut state.transcript),
                    logger,
                ));
            }
            None => None,
        };
        // T04 (KR-02): a request may claim at most a per-request output allowance, never the whole
        // remaining run allowance. Reserving everything up front is what made a single transient
        // failure fatal: the retry then had nothing left to reserve even though the failed attempt
        // had barely consumed anything. `None` (an unbounded run) keeps the historical unbounded
        // request cap so no existing behaviour changes.
        let max_output_tokens = run_output_allowance.map(|_| {
            self.model_capabilities.per_request_output_cap(
                state.budget_tracker.config().max_output_tokens_per_request,
                run_output_allowance,
            )
        });
        // T03 (KR-07): the window check covers the *whole* request — system prompt, tool schemas,
        // history, images and the output reservation — and runs before anything is transmitted.
        // `bytes / 4` is not a tokenizer, so this uses the conservative estimator (non-ASCII text is
        // charged at 1 token per 2 bytes) and says so in the event.
        if let (Some(window), true) = (
            self.model_capabilities.context_window_tokens,
            self.model_capabilities.source == crate::budget::CapabilitySource::Configured,
        ) {
            let estimated_input = crate::budget::conservative_request_tokens(
                &state.static_prefix.system_prompt,
                &projected.messages,
                state.static_prefix.tools_json.len(),
                REQUEST_SERIALIZATION_OVERHEAD_BYTES,
            );
            let reserved_output = self
                .model_capabilities
                .reserved_output_tokens(max_output_tokens);
            if estimated_input.saturating_add(reserved_output) > window {
                let _ = logger.log(EventPayload::ContextBudgetExceeded {
                    estimated_input_tokens: estimated_input,
                    reserved_output_tokens: reserved_output,
                    window_tokens: window,
                    capability_source: self.model_capabilities.source.as_str().to_string(),
                    tool_schema_bytes: state.static_prefix.tools_json.len(),
                    message_count: projected.messages.len(),
                });
                return Err(self.finish_run(
                    TerminalReason::ContextBudgetExceeded,
                    &state.budget_tracker,
                    &state.projector,
                    std::mem::take(&mut state.modified_files_set)
                        .into_iter()
                        .collect(),
                    state.last_assistant_text.take(),
                    std::mem::take(&mut state.transcript),
                    logger,
                ));
            }
        }

        // F05: reserve *before* dispatching. A failed reservation means another request or child
        // already holds the remaining balance, so this request must not be sent (zero backend
        // calls). Dropping the reservation later (cancel/timeout) records the estimate as spent.
        let reserve_amount = estimated_input_tokens.saturating_add(max_output_tokens.unwrap_or(0));
        let token_reservation = match self.token_budget.try_reserve(reserve_amount) {
            Some(reservation) => reservation,
            None => {
                return Err(self.finish_run(
                    TerminalReason::TokenBudgetExhausted,
                    &state.budget_tracker,
                    &state.projector,
                    std::mem::take(&mut state.modified_files_set)
                        .into_iter()
                        .collect(),
                    state.last_assistant_text.take(),
                    std::mem::take(&mut state.transcript),
                    logger,
                ));
            }
        };

        // B16: track the attempts that are really transmitted, so a failed call can report them.
        let wire_attempts = crate::provider::WireAttempts::new();
        // T12 (KR-09): activated discovered tools join the request, appended in activation order so
        // the list is stable across turns. They are resolved from the live registry and policy, so a
        // tool that was unmounted or denied after activation simply stops appearing.
        let mut request_tools = state.visible_tools.clone();
        for definition in self.tool_runtime.activated_tool_definitions() {
            if !request_tools
                .iter()
                .any(|existing| existing.name == definition.name)
            {
                request_tools.push(definition);
            }
        }
        let req = ProviderRequest {
            system_prompt: state.static_prefix.system_prompt.clone(),
            tools: request_tools,
            messages: projected.messages,
            stream: self.config.stream,
            // Gate 7: hand the projector's logical cache boundaries to the protocol client so
            // Anthropic-style explicit prompt caching can pin the frozen prefix.
            cache_breakpoints: projected.breakpoints,
            // F08: remaining output-token allowance derived from `--max-tokens`.
            max_output_tokens,
            retry_budget: Some(crate::provider::RetryBudget {
                ledger: Arc::clone(&self.token_budget),
                per_attempt: reserve_amount,
            }),
            wire_attempts: Some(wire_attempts.clone()),
        };

        // F04 (`v1.5.1`): every provider phase is clamped to the run's absolute deadline.
        // F09 (`v1.5.1`): hand the provider only the *remaining* request attempts (plus the one
        // already reserved above), so its internal retry loop cannot send more HTTP requests than
        // the run still allows.
        let deadline = state.budget_tracker.deadline();
        let remaining_attempts = state
            .budget_tracker
            .config()
            .max_model_request_attempts
            .saturating_sub(state.budget_tracker.request_attempts_used())
            .saturating_add(1)
            .max(1);
        let provider_budget = state
            .budget_tracker
            .config()
            .clamped_to_deadline(deadline, remaining_attempts);

        let completion = match tokio::time::timeout_at(
            deadline.tokio_instant(),
            self.provider
                .complete(req, &provider_budget, self.cancel_rx.clone()),
        )
        .await
        {
            Ok(Ok(c)) => c,
            Ok(Err(err)) => {
                let reason = if deadline.is_exceeded() {
                    TerminalReason::RunDeadlineExceeded
                } else {
                    err.clone().into_terminal_reason()
                };
                // Only provably pre-dispatch errors where zero network transmission took place
                // (e.g. client-side InsecureUrl validation failure) are proven zero-consumption.
                // Any post-dispatch failure (HttpStatus 500/502/503/504, retried transport drops,
                // redirects, timeouts, cancellations) cannot prove zero backend consumption;
                // those reservations are conservatively left un-settled so Drop commits them to
                // the ledger to keep budget ceilings fail-closed.
                if err.is_provably_zero_consumption() && wire_attempts.get() == 0 {
                    // T04 (KR-02): only a request that provably never left the process refunds its
                    // allowance. `settle_not_dispatched` refuses (and leaves the reservation to be
                    // charged as unknown) if a wire attempt was in fact transmitted.
                    match token_reservation.settle_not_dispatched(wire_attempts.get() as u64) {
                        Ok(record) => audit_token_settlement(&mut logger, &record),
                        Err(e) => {
                            let _ = logger.log(EventPayload::PolicyDecision {
                                call_id: crate::types::ToolCallId::new("token-budget"),
                                tool_name: "budget".to_string(),
                                decision: "settlement_refused".to_string(),
                                reason: Some(e.to_string()),
                            });
                        }
                    }
                    state.budget_tracker.refund_unsent_request_attempt();
                }
                // A failed call must still report the attempts it actually transmitted: the summary
                // is the only accounting a failed sample has.
                record_wire_attempts(&mut state.budget_tracker, wire_attempts.get());
                return Err(self.finish_run(
                    reason,
                    &state.budget_tracker,
                    &state.projector,
                    std::mem::take(&mut state.modified_files_set)
                        .into_iter()
                        .collect(),
                    state.last_assistant_text.take(),
                    std::mem::take(&mut state.transcript),
                    logger,
                ));
            }
            Err(_) => {
                return Err(self.finish_run(
                    TerminalReason::RunDeadlineExceeded,
                    &state.budget_tracker,
                    &state.projector,
                    std::mem::take(&mut state.modified_files_set)
                        .into_iter()
                        .collect(),
                    state.last_assistant_text.take(),
                    std::mem::take(&mut state.transcript),
                    logger,
                ));
            }
        };

        // F05 (`v1.5.1`): settle the reservation as soon as the response is in hand. A provider
        // that reports `usage` gives an exact count; otherwise the pre-dispatch byte estimate is
        // recorded so an omitted `usage` cannot bypass the cap. Settling here (rather than after
        // the attempts loop below) keeps the accounting exact even on an early return.
        let estimated_output_tokens = approx_tokens(
            completion
                .turn
                .content
                .as_ref()
                .map(|t| t.len())
                .unwrap_or(0)
                .saturating_add(
                    completion
                        .turn
                        .tool_calls
                        .iter()
                        .map(|c| c.raw_arguments.len() + c.name.len())
                        .sum::<usize>(),
                ),
        );
        if completion.attempts_used > 1 {
            // T04 (KR-02): the first attempt failed after transmission, so what it consumed cannot
            // be proven. Charge the whole reservation as unknown; the protocol client settled the
            // successful retry against its own reservation.
            let record = token_reservation.settle_unknown();
            audit_token_settlement(&mut logger, &record);
        } else if let Some(usage) = completion.turn.usage.as_ref() {
            let record =
                token_reservation.settle_with_usage(usage.input_tokens, usage.output_tokens);
            audit_token_settlement(&mut logger, &record);
        } else {
            // No usage reported: charge the labelled pre-dispatch estimate, never a measured zero.
            let record =
                token_reservation.settle_estimate(estimated_input_tokens, estimated_output_tokens);
            audit_token_settlement(&mut logger, &record);
        }

        // Prefer the observed count: a provider that mis-reports `attempts_used` cannot make the
        // summary under-count what it sent.
        let observed_attempts = wire_attempts.get().max(completion.attempts_used);
        for _ in 1..observed_attempts {
            if !state.budget_tracker.try_record_request_attempt() {
                return Err(self.finish_run(
                    TerminalReason::MaxRequestAttempts,
                    &state.budget_tracker,
                    &state.projector,
                    std::mem::take(&mut state.modified_files_set)
                        .into_iter()
                        .collect(),
                    state.last_assistant_text.take(),
                    std::mem::take(&mut state.transcript),
                    logger,
                ));
            }
        }

        let assistant_turn = completion.turn;
        if let Some(usage) = assistant_turn.usage {
            state.budget_tracker.record_token_usage(
                usage.input_tokens,
                usage.cached_input_tokens,
                usage.output_tokens,
            );
            state.last_input_tokens = Some(usage.input_tokens);
            if let Err(e) = logger.log(EventPayload::ModelRequestCompleted {
                turn: state.current_turn,
                usage_basis: Some("provider_reported".to_string()),
                reported_model: completion.reported_model.clone(),
                token_accounting: Some(self.token_budget.accounting_snapshot()),
                input_tokens: usage.input_tokens,
                cached_input_tokens: usage.cached_input_tokens,
                cache_creation_input_tokens: usage.cache_creation_input_tokens,
                uncached_input_tokens: usage.uncached_input_tokens(),
                output_tokens: usage.output_tokens,
                reasoning_tokens: usage.reasoning_tokens,
                prefix_cache_hit_ratio: usage.prefix_cache_hit_ratio(),
                latency_ms: completion.latency_ms,
                ttft_ms: completion.ttft_ms,
            }) {
                return Err(self.finish_run(
                    TerminalReason::StrictLogFailure(e.to_string()),
                    &state.budget_tracker,
                    &state.projector,
                    std::mem::take(&mut state.modified_files_set)
                        .into_iter()
                        .collect(),
                    state.last_assistant_text.take(),
                    std::mem::take(&mut state.transcript),
                    logger,
                ));
            }
        } else {
            // F08 (`v1.4.0`): a provider that omits `usage` must not bypass the token cap.
            // Conservatively estimate both directions from byte length (~4 bytes per token).
            let est_input = estimated_input_tokens;
            let est_output = estimated_output_tokens;
            state
                .budget_tracker
                .record_estimated_token_usage(est_input, None, est_output);
            state.last_input_tokens = Some(est_input);
            if let Err(e) = logger.log(EventPayload::ModelRequestCompleted {
                turn: state.current_turn,
                usage_basis: Some("byte_estimate_not_measured".to_string()),
                reported_model: completion.reported_model.clone(),
                token_accounting: Some(self.token_budget.accounting_snapshot()),
                input_tokens: est_input,
                cached_input_tokens: None,
                cache_creation_input_tokens: None,
                uncached_input_tokens: est_input,
                output_tokens: est_output,
                reasoning_tokens: None,
                prefix_cache_hit_ratio: 0.0,
                latency_ms: completion.latency_ms,
                ttft_ms: completion.ttft_ms,
            }) {
                return Err(self.finish_run(
                    TerminalReason::StrictLogFailure(e.to_string()),
                    &state.budget_tracker,
                    &state.projector,
                    std::mem::take(&mut state.modified_files_set)
                        .into_iter()
                        .collect(),
                    state.last_assistant_text.take(),
                    std::mem::take(&mut state.transcript),
                    logger,
                ));
            }
        }

        // §4.1: Pre-dispatch batch validation BEFORE adding to transcript or running any tools!
        if let Err(batch_err) = validate_assistant_turn_batch(
            &assistant_turn,
            state.budget_tracker.config(),
            self.config.allow_tool_calls_on_stop,
        ) {
            return Err(self.finish_run(
                batch_err.into_terminal_reason(),
                &state.budget_tracker,
                &state.projector,
                std::mem::take(&mut state.modified_files_set)
                    .into_iter()
                    .collect(),
                state.last_assistant_text.take(),
                std::mem::take(&mut state.transcript),
                logger,
            ));
        }

        if let Some(ref c) = assistant_turn.content {
            state.last_assistant_text = Some(c.clone());
        }

        state.tool_calls = assistant_turn.tool_calls.clone();
        self.record_transcript_item(
            &mut state.transcript,
            TranscriptItem::Assistant(assistant_turn),
            state.current_turn,
        );
        if let Some(err) = self.session_persist_error.clone() {
            return Err(self.finish_run(
                TerminalReason::StrictLogFailure(format!("session WAL append failed: {}", err)),
                &state.budget_tracker,
                &state.projector,
                std::mem::take(&mut state.modified_files_set)
                    .into_iter()
                    .collect(),
                state.last_assistant_text.take(),
                std::mem::take(&mut state.transcript),
                logger,
            ));
        }

        // If model stopped with no tool calls, run CompletionGate if files were modified (§3.2 V6-07)

        Ok(logger)
    }

    /// Handle a turn where the model stopped without calling any tool: run the pre-completion
    /// policy hooks, the completion gate and the adaptive escalation, or finish the run.
    ///
    /// Split out of `run_inner`'s loop body (341 lines of its 905). The body is the original code
    /// verbatim; `state` is threaded through by reference and `logger` is returned because
    /// `finish_run` consumes it.
    async fn handle_completion_turn(
        &mut self,
        state: &mut TurnState,
        mut logger: EventLogger,
    ) -> Result<EventLogger, (RunSummary, EventLogger)> {
        // Gate 3: `PreCompletion` policy hooks run before the completion gate. A denial is
        // fail-closed and is treated exactly like a gate rejection, so the model must keep
        // working instead of the run silently finishing.
        let pre_completion_denial = {
            let hooks = self.tool_runtime.hook_engine();
            if hooks.is_empty() {
                None
            } else {
                let ctx = HookContext::new(
                    state.current_turn,
                    self.tool_runtime.env().workspace().root().to_path_buf(),
                );
                let outcome = hooks
                    .evaluate_policy(crate::extensions::HookEvent::PreCompletion, &ctx)
                    .await;
                Self::log_hook_evaluations(&mut logger, outcome.evaluations());
                outcome.reason().map(|r| r.to_string())
            }
        };
        if let Some(reason) = pre_completion_denial {
            state.gate_failure_count = state.gate_failure_count.saturating_add(1);
            let mut mod_list: Vec<String> = state.modified_files_set.iter().cloned().collect();
            mod_list.sort();
            if state.gate_failure_count >= self.completion_gate_max_retries {
                return Err(self.finish_run(
                    TerminalReason::ProviderProtocolError(format!(
                        "PreCompletion policy hook denied completion {} times (max retries={}): {}",
                        state.gate_failure_count, self.completion_gate_max_retries, reason
                    )),
                    &state.budget_tracker,
                    &state.projector,
                    mod_list,
                    state.last_assistant_text.take(),
                    std::mem::take(&mut state.transcript),
                    logger,
                ));
            }
            self.record_transcript_item(
                &mut state.transcript,
                TranscriptItem::User(UserMessage {
                    content: format!(
                        "[PreCompletion policy hook denied completion, attempt {}/{}]: {}\nAddress the denial before finishing.",
                        state.gate_failure_count, self.completion_gate_max_retries, reason
                    ),
                }),
                state.current_turn.saturating_add(1),
            );
            if let Some(err) = self.session_persist_error.clone() {
                return Err(self.finish_run(
                    TerminalReason::StrictLogFailure(format!("session WAL append failed: {}", err)),
                    &state.budget_tracker,
                    &state.projector,
                    mod_list,
                    state.last_assistant_text.take(),
                    std::mem::take(&mut state.transcript),
                    logger,
                ));
            }
            return Ok(logger);
        }
        if !state.modified_files_set.is_empty() {
            let mut mod_list: Vec<String> = state.modified_files_set.iter().cloned().collect();
            mod_list.sort();
            let gate_ctx = CompletionGateContext {
                run_id: self.config.run_id,
                workspace_root: self.tool_runtime.env().workspace().root().to_path_buf(),
                modified_files: mod_list.clone(),
                turns_used: state.current_turn,
                deadline: Some(state.budget_tracker.deadline()),
            };
            // F04 (`v1.5.1`): verification is bounded by the run's absolute deadline. A compliant
            // gate clamps its own process timeout via `ctx.deadline`; this outer bound is a backstop.
            let gate_outcome = match tokio::time::timeout_at(
                state.budget_tracker.deadline().tokio_instant(),
                self.completion_gate.check(&gate_ctx),
            )
            .await
            {
                Ok(res) => res,
                Err(_) => Err("run deadline exceeded during completion verification".to_string()),
            };
            // F02 (`v1.5.1`): a verification gate can rewrite the workspace (formatter, codegen, a
            // failing test that regenerates fixtures). Capture it on *every* outcome — Approved,
            // Skipped, Rejected and Err — before acting on the result, so the latest revision and
            // the WAL always match disk and a rejected run can still be undone. The capture carries
            // the absolute session turn (`current_turn` restarts at 1 on `--resume`).
            let verification_changed =
                self.capture_workspace_revision(state.absolute_turn, "verification", &mut logger);
            if !verification_changed.is_empty() {
                state.modified_files_set.extend(verification_changed);
                mod_list = state.modified_files_set.iter().cloned().collect();
                mod_list.sort();
            }
            if let Some(err) = self.revision_error.clone() {
                return Err(self.finish_run(
                    TerminalReason::StrictLogFailure(err),
                    &state.budget_tracker,
                    &state.projector,
                    mod_list,
                    state.last_assistant_text.take(),
                    std::mem::take(&mut state.transcript),
                    logger,
                ));
            }
            if state.budget_tracker.is_deadline_exceeded() {
                return Err(self.finish_run(
                    TerminalReason::RunDeadlineExceeded,
                    &state.budget_tracker,
                    &state.projector,
                    mod_list,
                    state.last_assistant_text.take(),
                    std::mem::take(&mut state.transcript),
                    logger,
                ));
            }
            match gate_outcome {
                Ok(GateOutcome::Approved) => {
                    if let Some(journal) = self.session_journal.as_mut() {
                        if let Err(err) = journal.mark_completion_approved() {
                            self.session_persist_error = Some(err.to_string());
                        }
                    }
                    if let Some(err) = self.session_persist_error.clone() {
                        return Err(self.finish_run(
                            TerminalReason::StrictLogFailure(format!(
                                "session completion marker failed: {}",
                                err
                            )),
                            &state.budget_tracker,
                            &state.projector,
                            mod_list,
                            state.last_assistant_text.take(),
                            std::mem::take(&mut state.transcript),
                            logger,
                        ));
                    }
                }
                Ok(GateOutcome::Skipped { reason }) => {
                    if logger.live_stderr() {
                        eprintln!("[CompletionGate] verification skipped: {}", reason);
                    }
                    if let Some(journal) = self.session_journal.as_mut() {
                        if let Err(err) = journal.mark_completion_skipped(reason) {
                            self.session_persist_error = Some(err.to_string());
                        }
                    }
                    if let Some(err) = self.session_persist_error.clone() {
                        return Err(self.finish_run(
                            TerminalReason::StrictLogFailure(format!(
                                "session completion marker failed: {}",
                                err
                            )),
                            &state.budget_tracker,
                            &state.projector,
                            mod_list,
                            state.last_assistant_text.take(),
                            std::mem::take(&mut state.transcript),
                            logger,
                        ));
                    }
                }
                Ok(GateOutcome::Rejected { feedback }) => {
                    state.gate_failure_count = state.gate_failure_count.saturating_add(1);
                    let fingerprint = failure_fingerprint(&feedback);
                    let repeated =
                        state.last_gate_failure_hash.as_deref() == Some(fingerprint.as_str());
                    state.last_gate_failure_hash = Some(fingerprint.clone());
                    let mut gate_message = format!(
                        "[CompletionGate Rejected, attempt {}/{}]: {}\nFix the reported issue before finishing.",
                        state.gate_failure_count, self.completion_gate_max_retries, feedback
                    );
                    if repeated {
                        gate_message.push_str(
                            "\n\n[ANTI-OSCILLATION WARNING] The failure fingerprint is identical to the previous attempt. First use `read` to inspect the relevant source and surrounding context; choose a different fix strategy; do not repeat the previous `edit` unchanged."
                        );
                    }
                    // Gate 4: rank the session's recorded mutations against the gate's
                    // diagnostics and hand the model a *candidate* list with the exact
                    // evidence pointer and pre-change revision for each hypothesis.
                    let report = AttributionEngine::attribute_failure(
                        &self.mutation_ledger,
                        &feedback,
                        "completion_gate",
                        ATTRIBUTION_TOP_K,
                    );
                    Self::log_failure_attribution(&mut logger, "completion_gate", &report);
                    // Capability gap #2 (`v1.5.2`): opt-in isolated counterfactual probe. Only
                    // annotates the failure message with evidence; it never reverts anything.
                    let probe_outcomes = match self.current_revision_id.clone() {
                        Some(current_revision) => {
                            self.run_counterfactual_probes(
                                &report,
                                state.absolute_turn,
                                &current_revision,
                                &mut logger,
                            )
                            .await
                        }
                        None => Vec::new(),
                    };
                    if !probe_outcomes.is_empty() {
                        gate_message.push_str(
                            "\n\n[Counterfactual probe — isolated; the parent workspace was not modified]\n",
                        );
                        for outcome in &probe_outcomes {
                            gate_message.push_str(&format!("- {}\n", outcome.note));
                        }
                    }
                    if let Some(block) = report.render_for_model() {
                        gate_message.push_str("\n\n");
                        gate_message.push_str(&block);
                    }
                    self.record_transcript_item(
                        &mut state.transcript,
                        TranscriptItem::User(UserMessage {
                            content: gate_message,
                        }),
                        state.current_turn.saturating_add(1),
                    );
                    self.drain_steering_messages(
                        &mut state.transcript,
                        state.current_turn.saturating_add(1),
                    );
                    if let Some(err) = self.session_persist_error.clone() {
                        return Err(self.finish_run(
                            TerminalReason::StrictLogFailure(format!(
                                "session WAL append failed: {}",
                                err
                            )),
                            &state.budget_tracker,
                            &state.projector,
                            mod_list,
                            state.last_assistant_text.take(),
                            std::mem::take(&mut state.transcript),
                            logger,
                        ));
                    }
                    // Item #3 (`v1.4.0`): adaptive escalation. Instead of tripping the
                    // circuit breaker on a stall (2+ consecutive verification failures),
                    // race N diverse repair candidates in isolated worktrees and merge the
                    // best verified one.
                    if !self.adaptive_tournament_attempted
                        && state.gate_failure_count >= 2
                        && self.adaptive_tournament_candidates
                            >= crate::subagents::MIN_TOURNAMENT_CANDIDATES
                        && self.max_subagent_spawns > 0
                        && !*self.cancel_rx.borrow()
                    {
                        let preflight = crate::subagents::subagent_spawn_preflight(
                            self.tool_runtime.policy_engine(),
                            self.adaptive_tournament_candidates,
                            self.subagents_spawned,
                            self.max_subagent_spawns,
                            EffectClass::Mutating,
                        );
                        if matches!(preflight, Ok(PolicyCheckResult::Allow)) {
                            self.adaptive_tournament_attempted = true;
                            let excerpt: String = feedback.chars().take(1200).collect();
                            let request = crate::subagents::TournamentRequest {
                                task: format!(
                                    "{}\n\n[ADAPTIVE ESCALATION] The single-agent attempt failed verification {} time(s). Race independent repair strategies. Last verification failure:\n{}",
                                    self.config.task, state.gate_failure_count, excerpt
                                ),
                                candidates: self.adaptive_tournament_candidates,
                                allowed_write_globs: None,
                                max_turns: None,
                                timeout_ms: crate::subagents::DEFAULT_SUBAGENT_TIMEOUT_MS,
                                disallow_test_tampering: true,
                                protected_paths: Vec::new(),
                                // Capability gap #5 (`v1.5.2`): the adaptive repair race inherits
                                // the run's strict-verification policy.
                                require_verified: Some(self.tournament_require_verified),
                            };
                            let (tournament_out, _label, merged) = self
                                .execute_spawn_tournament(
                                    request,
                                    &crate::types::ToolCallId::new("adaptive_tournament"),
                                    state.absolute_turn,
                                    &mut state.modified_files_set,
                                    &mut logger,
                                    "adaptive_tournament".to_string(),
                                )
                                .await;
                            // F10 (`v1.5.0`): the adaptive path never reaches
                            // `execute_tool_batch`, so drain the child spend here instead of
                            // leaking it when the model stops on the next turn.
                            self.drain_children_tokens(&mut state.budget_tracker);
                            if merged {
                                // A candidate was verified and merged: the stall is broken.
                                state.gate_failure_count = 0;
                                state.last_gate_failure_hash = None;
                                self.record_transcript_item(
                                    &mut state.transcript,
                                    TranscriptItem::User(UserMessage {
                                        content: format!(
                                            "[ADAPTIVE TOURNAMENT MERGED] {}",
                                            tournament_out.content
                                        ),
                                    }),
                                    state.current_turn.saturating_add(1),
                                );
                                // BUG-02 (`v1.5.1`): the merge already landed on disk, but
                                // this branch `continue`s past the post-tool-batch
                                // capture/ledger/checkpoint block. Record it here so the
                                // merge is durably in the WAL (and `--undo` can revert it)
                                // even when the run ends via MaxTurns/cancel before another
                                // completion-gate cycle, and so Gate 4 can attribute a
                                // tournament-introduced regression to a mutation.
                                let revision_before = self.current_revision_id.clone();
                                let merged_changed_paths = self.capture_workspace_revision(
                                    state.absolute_turn,
                                    "mutation",
                                    &mut logger,
                                );
                                for path in &merged_changed_paths {
                                    state.modified_files_set.insert(path.clone());
                                }
                                if !merged_changed_paths.is_empty() {
                                    let spans = build_worker_mutation_spans(
                                        self.tool_runtime.env().workspace(),
                                        "adaptive_tournament",
                                        &merged_changed_paths,
                                        &[],
                                    );
                                    self.mutation_ledger.record_mutation(
                                        state.absolute_turn as u32,
                                        "adaptive_tournament",
                                        format!("adaptive-tournament-{}", state.absolute_turn),
                                        None,
                                        revision_before,
                                        spans,
                                    );
                                }
                                self.mutation_ledger.backfill_revision_and_paths(
                                    state.absolute_turn as u32,
                                    self.current_revision_id.clone(),
                                    &merged_changed_paths,
                                    self.snapshot_store.is_some()
                                        && (self.batch_mutated
                                            || self.batch_ran_side_effect
                                            || !merged_changed_paths.is_empty()),
                                );
                                // The merge is captured; clear the batch flags so a later
                                // read-only turn cannot re-trigger a stale capture.
                                self.batch_mutated = false;
                                self.batch_ran_side_effect = false;
                                if let Some(err) = self.revision_error.clone() {
                                    return Err(self.finish_run(
                                        TerminalReason::StrictLogFailure(err),
                                        &state.budget_tracker,
                                        &state.projector,
                                        std::mem::take(&mut state.modified_files_set)
                                            .into_iter()
                                            .collect(),
                                        state.last_assistant_text.take(),
                                        std::mem::take(&mut state.transcript),
                                        logger,
                                    ));
                                }
                                self.persist_session_checkpoint(
                                    &state.projector,
                                    &state.transcript,
                                    state.current_turn,
                                );
                                if let Some(err) = self.session_persist_error.clone() {
                                    return Err(self.finish_run(
                                        TerminalReason::StrictLogFailure(format!(
                                            "session checkpoint failed: {}",
                                            err
                                        )),
                                        &state.budget_tracker,
                                        &state.projector,
                                        std::mem::take(&mut state.modified_files_set)
                                            .into_iter()
                                            .collect(),
                                        state.last_assistant_text.take(),
                                        std::mem::take(&mut state.transcript),
                                        logger,
                                    ));
                                }
                                return Ok(logger);
                            }
                        }
                    }
                    if state.gate_failure_count >= self.completion_gate_max_retries {
                        let excerpt: String = feedback.chars().take(1600).collect();
                        return Err(self.finish_run(
                            TerminalReason::ProviderProtocolError(format!(
                                "CompletionGate rejected completion {} times (max retries={}); fingerprint={}; last failure:\n{}",
                                state.gate_failure_count, self.completion_gate_max_retries, fingerprint, excerpt
                            )),
                            &state.budget_tracker, &state.projector, mod_list, state.last_assistant_text.take(),
                            std::mem::take(&mut state.transcript), logger,
                        ));
                    }
                    return Ok(logger);
                }
                Err(err) => {
                    return Err(self.finish_run(
                        TerminalReason::ProviderProtocolError(format!(
                            "CompletionGate error: {}",
                            err
                        )),
                        &state.budget_tracker,
                        &state.projector,
                        mod_list,
                        state.last_assistant_text.take(),
                        std::mem::take(&mut state.transcript),
                        logger,
                    ));
                }
            }
        }

        // O1: a verification command can rewrite the workspace (formatters, codegen).
        // Capture the resulting state so `latest_revision_id()` never drifts from disk.
        // The capture must carry the *absolute* session turn: `current_turn` restarts at 1
        // on `--resume`, which would misorder revision records against the restored turns.
        self.capture_workspace_revision(state.absolute_turn, "verification", &mut logger);
        if let Some(err) = self.revision_error.clone() {
            return Err(self.finish_run(
                TerminalReason::StrictLogFailure(err),
                &state.budget_tracker,
                &state.projector,
                std::mem::take(&mut state.modified_files_set)
                    .into_iter()
                    .collect(),
                state.last_assistant_text.take(),
                std::mem::take(&mut state.transcript),
                logger,
            ));
        }

        let mut mod_list: Vec<String> = std::mem::take(&mut state.modified_files_set)
            .into_iter()
            .collect();
        mod_list.sort();
        Err(self.finish_run(
            TerminalReason::ModelStop,
            &state.budget_tracker,
            &state.projector,
            mod_list,
            state.last_assistant_text.take(),
            std::mem::take(&mut state.transcript),
            logger,
        ))
    }

    /// Restore session state, capture the baseline revision and build everything the main
    /// loop needs, so `run_inner` reads as "prepare, then loop".
    ///
    /// Returns the prepared state together with `logger` (which an early exit consumes via
    /// `finish_run`), or the finished run summary when preparation itself fails.
    async fn prepare_run(
        &mut self,
        mut logger: EventLogger,
    ) -> Result<(TurnState, EventLogger), (RunSummary, EventLogger)> {
        let budget_tracker = match BudgetTracker::new(self.config.budget.clone()) {
            Ok(b) => b,
            Err(e) => {
                let reason = TerminalReason::ProviderProtocolError(e.to_string());
                return Err(self.finish_run(
                    reason,
                    &BudgetTracker::new(BudgetConfig::default()).unwrap(),
                    &ContextProjector::new(),
                    Vec::new(),
                    None,
                    Vec::new(),
                    logger,
                ));
            }
        };

        // F04 (`v1.5.1`): publish the run's absolute deadline to the tool runtime and to the
        // phases that run outside the main loop (subagents, tournaments), so `bash`, the approval
        // prompt, verification and child runs all clamp to the same instant.
        self.run_deadline = Some(budget_tracker.deadline());
        self.tool_runtime
            .set_run_deadline(Some(budget_tracker.deadline()));
        let initial_exec_ctx = ExecutionContext::new(
            Some(self.config.run_id),
            Some(budget_tracker.deadline()),
            Some(self.cancel_rx.clone()),
            Some(Arc::clone(&self.token_budget)),
            Some(Arc::new(self.tool_runtime.policy_engine().clone())),
        )
        .with_revision(self.current_revision_id.clone());
        self.tool_runtime.set_execution_context(initial_exec_ctx);

        let policy_engine = self.tool_runtime.policy_engine();
        let policy_mode = policy_engine.mode();
        let mut visible_tools = self
            .tool_runtime
            .registry()
            .visible_definitions_for_engine(policy_engine);
        if self.max_subagent_spawns == 0 {
            visible_tools.retain(|t| t.name != "spawn_subagent");
        }
        // Capability gap #6 (`v1.5.2`): on-demand tool schema mode. The `discover_tools` kernel tool
        // stays visible so the model can fetch any hidden schema, and the runtime can still
        // dispatch every policy-permitted tool; only the *initial* schema set shrinks.
        if self.config.on_demand_tools {
            visible_tools.retain(|t| is_core_tool(&t.name));
        }
        let static_prefix = self.prompt_builder.build_static_prefix(&visible_tools);

        if let Err(e) = logger.log(EventPayload::RunStarted {
            prompt_hash: static_prefix.prompt_hash.clone(),
            tool_schema_hash: static_prefix.tool_schema_hash.clone(),
            static_prefix_bytes: static_prefix.static_prefix_bytes,
            policy: policy_mode.as_str().to_string(),
            budget_config: self.config.budget.clone(),
        }) {
            return Err(self.finish_run(
                TerminalReason::StrictLogFailure(e.to_string()),
                &budget_tracker,
                &ContextProjector::new(),
                Vec::new(),
                None,
                Vec::new(),
                logger,
            ));
        }

        let mut transcript = self
            .session_journal
            .as_ref()
            .map(SessionJournal::active_transcript)
            .unwrap_or_default();
        self.session_turn_offset = self
            .session_journal
            .as_ref()
            .map(SessionJournal::max_active_turn)
            .unwrap_or(0);
        let is_new_session = transcript.is_empty();
        if !is_new_session {
            if let Err(err) = self.recover_trailing_tool_batch(&mut transcript) {
                return Err(self.finish_run(
                    TerminalReason::ProviderProtocolError(format!(
                        "session recovery failed: {}",
                        err
                    )),
                    &budget_tracker,
                    &ContextProjector::new(),
                    Vec::new(),
                    None,
                    transcript,
                    logger,
                ));
            }
        }
        let task_turn = if is_new_session { 0 } else { 1 };
        let initial_task_item = TranscriptItem::User(UserMessage {
            content: self.config.task.clone(),
        });
        // If `--restore-rev` (or an intra-turn `--undo`) reverted the workspace to an earlier
        // revision on the active branch, compute the surviving revision set before the new baseline
        // capture appends to `revision_records()`.
        let reverted_active_revisions = self.session_journal.as_ref().and_then(|journal| {
            let records = journal.revision_records();
            let last = records.last()?;
            let first_idx = records
                .iter()
                .position(|r| r.revision_id == last.revision_id)?;
            if first_idx + 1 < records.len() {
                Some(
                    records[..=first_idx]
                        .iter()
                        .map(|r| r.revision_id.clone())
                        .collect::<std::collections::BTreeSet<String>>(),
                )
            } else {
                None
            }
        });
        // Gate 1: capture the pre-mutation workspace state BEFORE the new `UserMessage` becomes the
        // WAL leaf, so `--undo` after this resumed turn rewinds to the same leaf the baseline was
        // recorded at and cannot evict the baseline revision from the active branch.
        let baseline_turn = self.session_turn_offset;
        self.capture_workspace_revision(baseline_turn, "baseline", &mut logger);
        if let Some(err) = self.revision_error.clone() {
            return Err(self.finish_run(
                TerminalReason::StrictLogFailure(err),
                &budget_tracker,
                &ContextProjector::new(),
                Vec::new(),
                None,
                transcript,
                logger,
            ));
        }
        self.record_transcript_item(&mut transcript, initial_task_item, task_turn);
        if let Some(err) = self.session_persist_error.clone() {
            return Err(self.finish_run(
                TerminalReason::StrictLogFailure(format!("session WAL append failed: {}", err)),
                &budget_tracker,
                &ContextProjector::new(),
                Vec::new(),
                None,
                transcript,
                logger,
            ));
        }
        let mut projector = ContextProjector::new();
        // T15 (KR-11): attach the durable evidence layer. Memory is a cache with a 16 MiB cap; once
        // an entry is evicted, `recall_evidence` must still be able to produce the bytes (or say
        // exactly why it cannot). The branch component is the leaf the run started on, fixed for the
        // run, so a forked run gets its own artifact domain and cannot read its sibling's evidence.
        if let Some(journal) = self.session_journal.as_ref() {
            let state_dir = journal
                .path()
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_else(std::env::temp_dir);
            projector = projector.with_evidence_store(
                std::sync::Arc::new(crate::evidence_store::EvidenceStore::new(
                    state_dir.join("evidence"),
                )),
                journal.session_id().to_string(),
                journal.leaf_id().unwrap_or("root").to_string(),
            );
        }
        // B2 (`v1.4.0`): honour a configured projection profile (default `P2`).
        if let Some(profile) = self.config.projection_profile {
            projector = projector.with_profile(profile);
        }
        // Capability gap #3 (`v1.5.2`): opt-in causal pruning / dead-end compression.
        if self.config.causal_pruning {
            projector = projector.with_causal_pruning(true);
        }
        // Freeze the system-prefix / tool-schema byte split so the Gate 2 budget breakdown can
        // report them separately instead of one opaque `static_prefix_bytes`.
        projector.set_prefix_breakdown(
            static_prefix.system_prompt.len(),
            static_prefix.tools_json.len(),
        );
        if let Some(checkpoint) = self
            .session_journal
            .as_ref()
            .and_then(SessionJournal::latest_active_checkpoint)
            .cloned()
        {
            projector.restore_pinned_baseline_end(checkpoint.pinned_baseline_end);
            projector.restore_last_touched(
                checkpoint
                    .last_touched
                    .into_iter()
                    .map(|item| (item.path, item.last_touched_turn)),
            );
            // Gate 3: restore the subtree directories whose JIT guidance is already present in the
            // restored (still active) transcript, so `--resume` / `--fork` never re-inject it.
            self.subtree_tracker
                .restore_loaded_dirs(checkpoint.subtree_dirs.iter().cloned());
            // Gate 4: restore the branch's mutation ledger, then drop every record from a turn the
            // active branch never reached. A `--fork <TURN>` rewind selects an earlier leaf, so
            // attribution can never blame a mutation that only exists on an abandoned branch.
            self.mutation_ledger.restore(checkpoint.mutations.clone());
            self.mutation_ledger
                .prune_after_turn(self.session_turn_offset as u32);
            if let Some(ref active_revs) = reverted_active_revisions {
                self.mutation_ledger.prune_reverted_revisions(active_revs);
            }
            self.subagents_spawned = self.subagents_spawned.max(checkpoint.subagents_spawned);
        }
        // Replay any completed mutating tool result in the active transcript that was persisted
        // after the last checkpoint before an interruption.
        for (idx, item) in transcript.iter().enumerate() {
            if let TranscriptItem::ToolResult(tr) = item {
                if tr.tool_name == "spawn_subagent" && tr.outcome.is_success() {
                    // Advance `subagents_spawned` past any `<subagent_report ... id="sub-XXXX">`
                    // present on the active branch so resumed/forked sessions never reuse IDs.
                    let mut search_from = 0usize;
                    while let Some(pos) = tr.content[search_from..].find("id=\"sub-") {
                        let start = search_from + pos + "id=\"sub-".len();
                        let digits: String = tr.content[start..]
                            .chars()
                            .take_while(|c| c.is_ascii_digit())
                            .collect();
                        if let Ok(parsed_id) = digits.parse::<usize>() {
                            self.subagents_spawned = self.subagents_spawned.max(parsed_id);
                        }
                        search_from = start;
                    }
                }
                if tr.outcome.is_success() && !self.mutation_ledger.has_call_id(tr.call_id.as_str())
                {
                    if let Some(MutationTelemetry { spans, .. }) = tr.meta.mutation.clone() {
                        self.mutation_ledger.record_mutation(
                            self.session_turn_offset as u32,
                            tr.tool_name.clone(),
                            tr.call_id.as_str().to_string(),
                            Some(source_entry_id(idx, tr.call_id.as_str())),
                            self.current_revision_id.clone(),
                            spans,
                        );
                    }
                }
            }
        }
        // Gate 2: re-seed the raw-evidence store from the WAL so `recall_evidence` can still resolve
        // stubs compacted in an earlier process. The WAL keeps the pre-overlay (raw) item for every
        // entry while `active_entries()` yields the compacted stub, so the raw bytes survive resume.
        self.seed_evidence_from_journal(&mut projector);
        let loop_guard = LoopGuard::new();
        // Reconstruct dirty files from the selected session branch so a resumed run cannot
        // bypass CompletionGate merely because the write happened before this process started.
        let modified_files_set: HashSet<String> = self
            .session_journal
            .as_ref()
            .map(SessionJournal::modified_files_since_last_resolution)
            .unwrap_or_default()
            .into_iter()
            .collect();
        let persisted_gate_feedback = self
            .session_journal
            .as_ref()
            .map(SessionJournal::completion_feedback_since_last_resolution)
            .unwrap_or_default();
        let last_input_tokens: Option<u64> = None;
        let last_assistant_text: Option<String> = None;
        let gate_failure_count = persisted_gate_feedback.len();
        let last_gate_failure_hash: Option<String> = persisted_gate_feedback
            .last()
            .and_then(|message| Self::completion_gate_feedback_payload(message))
            .map(failure_fingerprint);

        if gate_failure_count >= self.completion_gate_max_retries && !modified_files_set.is_empty()
        {
            return Err(self.finish_run(
                TerminalReason::ProviderProtocolError(format!(
                    "CompletionGate already rejected this unresolved session {} times (max retries={}); fork before the failed attempts or start a new session to reset the retry budget",
                    gate_failure_count, self.completion_gate_max_retries
                )),
                &budget_tracker, &projector, modified_files_set.into_iter().collect(),
                None, transcript, logger,
            ));
        }

        Ok((
            TurnState {
                budget_tracker,
                visible_tools,
                static_prefix,
                transcript,
                projector,
                loop_guard,
                modified_files_set,
                last_input_tokens,
                last_assistant_text,
                gate_failure_count,
                last_gate_failure_hash,
                current_turn: 0,
                absolute_turn: 0,
                tool_calls: Vec::new(),
            },
            logger,
        ))
    }

    /// Close a trailing tool batch interrupted by a process crash before sending another request.
    /// Existing completed results are retained; only the missing suffix is synthesized and logged.
    fn recover_trailing_tool_batch(
        &mut self,
        transcript: &mut Vec<TranscriptItem>,
    ) -> Result<(), String> {
        let mut expected_calls = Vec::new();
        let mut seen_ids = Vec::new();
        for item in transcript.iter() {
            match item {
                TranscriptItem::Assistant(a) => {
                    expected_calls = a.tool_calls.clone();
                    seen_ids.clear();
                }
                TranscriptItem::ToolResult(result) => seen_ids.push(result.call_id.clone()),
                TranscriptItem::User(_) => {
                    expected_calls.clear();
                    seen_ids.clear();
                }
            }
        }
        if expected_calls.is_empty() {
            return Ok(());
        }
        let expected_ids: Vec<_> = expected_calls
            .iter()
            .map(|call| call.call_id.clone())
            .collect();
        if expected_ids == seen_ids {
            return Ok(());
        }
        if seen_ids.len() > expected_ids.len() || expected_ids[..seen_ids.len()] != seen_ids[..] {
            return Err(
                "trailing tool results are not a prefix of the pending tool-call batch".into(),
            );
        }
        let missing = expected_calls[seen_ids.len()..].to_vec();
        self.synthesize_remaining_results(
            &missing,
            ErrorCode::NotExecutedCancelled,
            "NOT_EXECUTED_CANCELLED: recovered after process interruption; tool was not confirmed complete",
            transcript,
            0,
        );
        if let Some(err) = self.session_persist_error.clone() {
            return Err(format!("failed to persist recovery markers: {}", err));
        }
        Ok(())
    }

    fn record_transcript_item(
        &mut self,
        transcript: &mut Vec<TranscriptItem>,
        item: TranscriptItem,
        relative_turn: usize,
    ) {
        transcript.push(item.clone());
        let absolute_turn = self.session_turn_offset.saturating_add(relative_turn);
        if let Some(journal) = self.session_journal.as_mut() {
            if let Err(err) = journal.append_item(absolute_turn, item) {
                self.session_persist_error = Some(err.to_string());
            }
        }
    }

    fn drain_steering_messages(
        &mut self,
        transcript: &mut Vec<TranscriptItem>,
        relative_turn: usize,
    ) {
        let mut messages = Vec::new();
        if let Some(ref mut rx) = self.steer_rx {
            while let Ok(msg) = rx.try_recv() {
                if !msg.trim().is_empty() {
                    messages.push(msg);
                }
            }
        }
        for msg in messages {
            let steered = format!(
                "[STEERING INSTRUCTION — high priority for the next model turn; respect system and safety constraints]\n{}",
                msg
            );
            self.record_transcript_item(
                transcript,
                TranscriptItem::User(UserMessage { content: steered }),
                relative_turn,
            );
        }
    }

    /// Rebuild the `recall_evidence` store from a resumed session WAL (Gate 2).
    fn seed_evidence_from_journal(&self, projector: &mut ContextProjector) {
        let Some(journal) = self.session_journal.as_ref() else {
            return;
        };
        journal.seed_projector_evidence(projector);
    }

    /// Capture a `WorkspaceRevision`, bind it to the current WAL leaf, and emit telemetry.
    ///
    /// A capture failure is fail-closed: if the store is attached, the run must not continue while
    /// believing it has undo protection it does not actually have.
    fn capture_workspace_revision(
        &mut self,
        absolute_turn: usize,
        trigger: &str,
        logger: &mut EventLogger,
    ) -> Vec<String> {
        let Some(store) = self.snapshot_store.clone() else {
            return Vec::new();
        };
        let workspace = self.tool_runtime.env().workspace().clone();
        let wal_entry_id = self
            .session_journal
            .as_ref()
            .and_then(|journal| journal.leaf_id())
            .map(|id| id.to_string());
        let parent = self.current_revision_id.clone();
        match store.capture_revision(
            &workspace,
            parent.as_ref(),
            wal_entry_id.as_deref(),
            absolute_turn,
        ) {
            Ok(revision) => {
                // O1: a `bash`/verification command that left the workspace byte-identical must not
                // append a duplicate `RevisionCaptured` record. Only the baseline capture is always
                // recorded, so `--undo` always has a starting point.
                let unchanged = trigger != "baseline"
                    && self.current_revision_id.as_ref() == Some(&revision.revision_id);
                if unchanged {
                    self.current_revision_id = Some(revision.revision_id);
                    return Vec::new();
                }
                let changed_paths = revision.changed_paths.clone();
                if let Some(journal) = self.session_journal.as_mut() {
                    if let Err(err) = journal.record_revision_capture(
                        absolute_turn,
                        revision.revision_id.as_str(),
                        revision.parent_revision_id.as_ref().map(|p| p.as_str()),
                        revision.changed_paths.clone(),
                        revision.tracked_files.len(),
                    ) {
                        self.revision_error = Some(format!(
                            "failed to record workspace revision {} in the session WAL: {}",
                            revision.revision_id, err
                        ));
                    }
                }
                let _ = logger.log(EventPayload::RevisionCaptured {
                    turn: absolute_turn,
                    revision_id: revision.revision_id.as_str().to_string(),
                    parent_revision_id: revision
                        .parent_revision_id
                        .as_ref()
                        .map(|p| p.as_str().to_string()),
                    changed_paths: revision.changed_paths.clone(),
                    tracked_file_count: revision.tracked_files.len(),
                    trigger: trigger.to_string(),
                });
                self.current_revision_id = Some(revision.revision_id);
                changed_paths
            }
            Err(err) => {
                self.revision_error = Some(format!(
                    "workspace revision capture failed ({}); aborting rather than continuing without undo protection: {}",
                    trigger, err
                ));
                Vec::new()
            }
        }
    }

    fn persist_session_checkpoint(
        &mut self,
        projector: &ContextProjector,
        transcript: &[TranscriptItem],
        relative_turn: usize,
    ) {
        let absolute_turn = self.session_turn_offset.saturating_add(relative_turn);
        let workspace_revision_id = self
            .current_revision_id
            .as_ref()
            .map(|id| id.as_str().to_string());
        let Some(journal) = self.session_journal.as_mut() else {
            return;
        };
        if let Err(err) = journal.sync_projected_transcript(transcript, projector.evidence_links())
        {
            self.session_persist_error = Some(err.to_string());
            return;
        }
        let mut touches: std::collections::BTreeMap<String, usize> = projector
            .last_touched_turns()
            .iter()
            .map(|(path, turn)| (path.clone(), *turn))
            .collect();
        let mut reasoning = Vec::new();
        for item in transcript {
            if let TranscriptItem::Assistant(a) = item {
                reasoning.extend(a.reasoning_blocks.iter().cloned());
            }
        }
        let last_touched = std::mem::take(&mut touches)
            .into_iter()
            .map(|(path, last_touched_turn)| FileTouch {
                path,
                last_touched_turn,
            })
            .collect();
        let reasoning_hash = serde_json::to_vec(&reasoning)
            .map(|bytes| format!("{:x}", sha2::Sha256::digest(bytes)))
            .unwrap_or_default();
        if let Err(err) = journal.checkpoint_with_subagents(
            absolute_turn,
            projector.last_pinned_baseline_end(),
            last_touched,
            reasoning_hash,
            workspace_revision_id,
            self.subtree_tracker.loaded_dirs(),
            self.mutation_ledger.records().to_vec(),
            self.subagents_spawned,
        ) {
            self.session_persist_error = Some(err.to_string());
        }
    }

    #[allow(clippy::too_many_arguments)]
    /// Finish one tool call: apply subtree context, record its mutation, and append the result.
    ///
    /// Both the parallel read-only sub-batch and the serial barrier must complete a call exactly
    /// this way, and the ordering is load-bearing: `canonical_index` has to be the transcript
    /// length *before* the result is appended, because Gate 4 binds the mutation to that entry so
    /// the candidate can carry the Gate 2 `source_entry_id`. Spelling the sequence out twice let
    /// the two paths drift, so it lives here once.
    fn record_tool_result(
        &mut self,
        mut msg: ToolResultMessage,
        transcript: &mut Vec<TranscriptItem>,
        budget_tracker: &mut BudgetTracker,
        logger: &mut EventLogger,
    ) {
        self.apply_subtree_context(&mut msg, logger);
        let canonical_index = transcript.len();
        self.record_mutation_from_result(
            &msg,
            canonical_index,
            budget_tracker.turns_used(),
            logger,
        );
        self.record_transcript_item(
            transcript,
            TranscriptItem::ToolResult(msg),
            budget_tracker.turns_used(),
        );
    }

    // A deliberate orchestrator: the tool-batch scheduler needs the transcript, budget, loop
    // guard, projector, modified-file set and logger together, and bundling them would only move
    // the same list into a struct the one caller has to build.
    #[allow(clippy::too_many_arguments)]
    async fn execute_tool_batch(
        &mut self,
        tool_calls: &[ToolCall],
        transcript: &mut Vec<TranscriptItem>,
        budget_tracker: &mut BudgetTracker,
        loop_guard: &mut LoopGuard,
        projector: &mut ContextProjector,
        modified_files_set: &mut HashSet<String>,
        logger: &mut EventLogger,
    ) -> Option<TerminalReason> {
        let mut idx = 0usize;
        let total = tool_calls.len();
        let semaphore = Arc::new(Semaphore::new(READ_ONLY_CONCURRENCY_LIMIT));

        while idx < total {
            // Check for cancellation / deadline / tool budget BEFORE dispatching next item(s)
            if *self.cancel_rx.borrow() {
                self.synthesize_remaining_results(
                    &tool_calls[idx..],
                    ErrorCode::NotExecutedCancelled,
                    "NOT_EXECUTED_CANCELLED: run cancelled before tool execution",
                    transcript,
                    budget_tracker.turns_used(),
                );
                return Some(TerminalReason::UserCancelled);
            }
            if budget_tracker.is_deadline_exceeded() {
                self.synthesize_remaining_results(
                    &tool_calls[idx..],
                    ErrorCode::NotExecutedBudgetExhausted,
                    "NOT_EXECUTED_BUDGET_EXHAUSTED: run deadline exceeded before tool execution",
                    transcript,
                    budget_tracker.turns_used(),
                );
                return Some(TerminalReason::RunDeadlineExceeded);
            }

            // Determine if we have a contiguous sub-batch of parallelizable ReadOnly calls
            let mut ro_end = idx;
            while ro_end < total
                && self
                    .tool_runtime
                    .is_parallelizable_read_only(&tool_calls[ro_end])
            {
                ro_end += 1;
            }

            if ro_end > idx {
                // Pre-validate budget and LoopGuard in deterministic index order `idx..ro_end`
                let mut tasks_to_run: Vec<(usize, ToolCall, String, Option<String>)> = Vec::new();
                let mut pre_resolved: Vec<Option<ToolResultMessage>> = vec![None; ro_end - idx];

                for (local_i, call) in tool_calls[idx..ro_end].iter().enumerate() {
                    if !budget_tracker.try_consume_tool_call() {
                        self.synthesize_remaining_results(
                            &tool_calls[idx..],
                            ErrorCode::NotExecutedBudgetExhausted,
                            "NOT_EXECUTED_BUDGET_EXHAUSTED: max_tool_calls limit reached",
                            transcript,
                            budget_tracker.turns_used(),
                        );
                        return Some(TerminalReason::MaxToolCalls);
                    }
                    let rel_path = ToolRuntime::extract_relative_path(&call.raw_arguments);
                    match loop_guard.check_and_record(
                        &call.name,
                        &call.raw_arguments,
                        rel_path.as_deref(),
                    ) {
                        LoopGuardVerdict::Allow { signature } => {
                            tasks_to_run.push((local_i, call.clone(), signature, rel_path));
                        }
                        LoopGuardVerdict::Block {
                            code,
                            guard_kind,
                            signature,
                            message,
                        } => {
                            let _ = logger.log(EventPayload::LoopGuardTriggered {
                                call_id: call.call_id.clone(),
                                tool_name: call.name.clone(),
                                guard_kind: guard_kind.to_string(),
                                signature,
                            });
                            pre_resolved[local_i] = Some(ToolResultMessage {
                                call_id: call.call_id.clone(),
                                tool_name: call.name.clone(),
                                outcome: ToolOutcome::Error { code },
                                content: Arc::from(message),
                                meta: ToolResultMeta {
                                    relative_path: rel_path,
                                    ..Default::default()
                                },
                            });
                        }
                    }
                }

                // Log ToolStarted in exact index order
                for (_, call, sig, rel_path) in &tasks_to_run {
                    if let Err(e) = logger.log(EventPayload::ToolStarted {
                        call_id: call.call_id.clone(),
                        tool_name: call.name.clone(),
                        args_sha256: sig.clone(),
                        relative_path: rel_path.clone(),
                    }) {
                        self.synthesize_remaining_results(
                            &tool_calls[idx..],
                            ErrorCode::NotExecutedCancelled,
                            "NOT_EXECUTED_CANCELLED: strict log failure",
                            transcript,
                            budget_tracker.turns_used(),
                        );
                        return Some(TerminalReason::StrictLogFailure(e.to_string()));
                    }
                }

                // Spawn concurrent execution with Semaphore(8)
                let batch_turn = budget_tracker.turns_used();
                let mut join_handles = Vec::with_capacity(tasks_to_run.len());
                for (local_i, call, _sig, rel_path) in tasks_to_run {
                    let sem = Arc::clone(&semaphore);
                    let rt = self.tool_runtime.clone();
                    let cancel = self.cancel_rx.clone();
                    let call_for_task = call.clone();
                    let batch_exec_ctx = ExecutionContext::new(
                        Some(self.config.run_id),
                        Some(budget_tracker.deadline()),
                        Some(cancel),
                        Some(Arc::clone(&self.token_budget)),
                        Some(Arc::new(rt.policy_engine().clone())),
                    )
                    .with_revision(self.current_revision_id.clone());
                    let handle = tokio::spawn(async move {
                        let _permit = sem.acquire_owned().await.ok();
                        let (msg, decision, hooks) = rt
                            .execute_call_with_context(&call_for_task, batch_turn, &batch_exec_ctx)
                            .await;
                        (local_i, msg, decision, hooks)
                    });
                    join_handles.push((local_i, call, rel_path, handle));
                }

                let mut decisions: Vec<Option<String>> = vec![None; ro_end - idx];
                let mut hook_reports: Vec<Vec<HookEvaluation>> = vec![Vec::new(); ro_end - idx];
                for (local_i, call, rel_path, handle) in join_handles {
                    match handle.await {
                        Ok((resolved_i, msg, decision, hooks)) => {
                            pre_resolved[resolved_i] = Some(msg);
                            decisions[resolved_i] = Some(decision);
                            hook_reports[resolved_i] = hooks;
                        }
                        Err(join_err) => {
                            let code = if join_err.is_cancelled() {
                                ErrorCode::NotExecutedCancelled
                            } else {
                                ErrorCode::IoError
                            };
                            pre_resolved[local_i] = Some(ToolResultMessage {
                                call_id: call.call_id.clone(),
                                tool_name: call.name.clone(),
                                outcome: ToolOutcome::Error { code },
                                content: Arc::from(format!(
                                    "{}: concurrent read-only tool task failed: {}",
                                    code.as_str(),
                                    join_err
                                )),
                                meta: ToolResultMeta {
                                    relative_path: rel_path,
                                    ..Default::default()
                                },
                            });
                            decisions[local_i] = Some("task_join_error".to_string());
                        }
                    }
                }

                // Commit results strictly in original `tool_calls` index order!
                let commit_limit = ro_end - idx;

                for local_i in 0..commit_limit {
                    let call = &tool_calls[idx + local_i];
                    Self::log_hook_evaluations(logger, &hook_reports[local_i]);
                    if let Some(msg) = pre_resolved[local_i].take() {
                        if let Some(ref dec) = decisions[local_i] {
                            let _ = logger.log(EventPayload::PolicyDecision {
                                call_id: call.call_id.clone(),
                                tool_name: call.name.clone(),
                                decision: dec.clone(),
                                reason: None,
                            });
                            // A policy-hook denial means the call never ran, so it must not count
                            // toward the loop/oscillation detectors.
                            if dec == "deny_policy_hook" {
                                loop_guard.rollback_recorded(&LoopGuard::compute_signature(
                                    &call.name,
                                    &call.raw_arguments,
                                ));
                            }
                        }
                        if let Some(ref path) = msg.meta.relative_path {
                            loop_guard.record_file_tool_outcome(
                                &msg.tool_name,
                                path,
                                msg.outcome.is_success(),
                            );
                            if msg.tool_name == "read" && msg.outcome.is_success() {
                                projector.notify_file_read(path);
                            }
                        }
                        let _ = logger.log(EventPayload::ToolCompleted {
                            call_id: msg.call_id.clone(),
                            tool_name: msg.tool_name.clone(),
                            relative_path: msg.meta.relative_path.clone(),
                            duration_ms: msg.meta.duration_ms,
                            truncated: msg.meta.truncated,
                            exit_code: msg.meta.exit_code,
                            outcome: if msg.outcome.is_success() {
                                "success".to_string()
                            } else {
                                "error".to_string()
                            },
                            edit_match_tier: msg.meta.edit_match_tier,
                            diag_kind: msg.meta.diag_kind,
                            compaction_reread_count: projector.compaction_reread_count(),
                        });
                        self.record_tool_result(msg, transcript, budget_tracker, logger);
                    }
                }

                idx = ro_end;
                continue;
            }

            // Barrier: Single serial execution for Mutating / SideEffect / Sensitive tool call
            let call = &tool_calls[idx];
            if !budget_tracker.try_consume_tool_call() {
                self.synthesize_remaining_results(
                    &tool_calls[idx..],
                    ErrorCode::NotExecutedBudgetExhausted,
                    "NOT_EXECUTED_BUDGET_EXHAUSTED: max_tool_calls limit reached",
                    transcript,
                    budget_tracker.turns_used(),
                );
                return Some(TerminalReason::MaxToolCalls);
            }

            let rel_path = ToolRuntime::extract_relative_path(&call.raw_arguments);
            match loop_guard.check_and_record(&call.name, &call.raw_arguments, rel_path.as_deref())
            {
                LoopGuardVerdict::Block {
                    code,
                    guard_kind,
                    signature,
                    message,
                } => {
                    let _ = logger.log(EventPayload::LoopGuardTriggered {
                        call_id: call.call_id.clone(),
                        tool_name: call.name.clone(),
                        guard_kind: guard_kind.to_string(),
                        signature,
                    });
                    self.record_transcript_item(
                        transcript,
                        TranscriptItem::ToolResult(ToolResultMessage {
                            call_id: call.call_id.clone(),
                            tool_name: call.name.clone(),
                            outcome: ToolOutcome::Error { code },
                            content: Arc::from(message),
                            meta: ToolResultMeta {
                                relative_path: rel_path,
                                ..Default::default()
                            },
                        }),
                        budget_tracker.turns_used(),
                    );
                }
                LoopGuardVerdict::Allow { signature } => {
                    if let Err(e) = logger.log(EventPayload::ToolStarted {
                        call_id: call.call_id.clone(),
                        tool_name: call.name.clone(),
                        args_sha256: signature,
                        relative_path: rel_path.clone(),
                    }) {
                        self.synthesize_remaining_results(
                            &tool_calls[idx..],
                            ErrorCode::NotExecutedCancelled,
                            "NOT_EXECUTED_CANCELLED: strict log failure",
                            transcript,
                            budget_tracker.turns_used(),
                        );
                        return Some(TerminalReason::StrictLogFailure(e.to_string()));
                    }

                    // `recall_evidence` resolves against the live session transcript, which the
                    // agent loop owns. It is still resolved through the registry so `--policy` and
                    // `--tool-filter` hide/deny it exactly like every other tool (a filtered-out
                    // call falls through to the runtime, which returns `deny_tool_filter`).
                    let recall_available = call.name == "recall_evidence"
                        && self
                            .tool_runtime
                            .registry()
                            .lookup_for_dispatch(
                                "recall_evidence",
                                self.tool_runtime.policy_engine().mode(),
                            )
                            .is_ok();
                    let spawn_subagent_available = call.name == "spawn_subagent"
                        && self
                            .tool_runtime
                            .registry()
                            .lookup_for_dispatch(
                                "spawn_subagent",
                                self.tool_runtime.policy_engine().mode(),
                            )
                            .is_ok();
                    let (mut msg, decision, serial_hook_evaluations) = if recall_available {
                        // Gate 3: `recall_evidence` is dispatched here because it resolves against the
                        // live transcript, but it still runs the exact same fail-closed `PreToolUse`
                        // policy-hook gate, kernel `PolicyEngine` evaluation and fail-open
                        // `PostToolUse` observer pipeline as every tool routed through the runtime.
                        match self
                            .tool_runtime
                            .pre_tool_hook_gate(call, budget_tracker.turns_used())
                            .await
                        {
                            Err((denied, mut hooks)) => {
                                // PostToolUse observers run for denied calls too, exactly as in
                                // `ToolRuntime::execute_call`.
                                hooks.extend(
                                    self.tool_runtime
                                        .post_tool_hooks(call, budget_tracker.turns_used(), &denied)
                                        .await,
                                );
                                (denied, "deny_policy_hook".to_string(), hooks)
                            }
                            Ok(mut hooks) => {
                                let check = self.tool_runtime.policy_engine().evaluate(
                                    &call.name,
                                    EffectClass::ReadOnly,
                                    false,
                                );
                                let (message, decision) =
                                    if let PolicyCheckResult::Deny(reason) = check {
                                        let out = ToolExecutionOutput::error(
                                            ErrorCode::PolicyDenied,
                                            format!("{} ({})", reason, call.name),
                                            ToolResultMeta {
                                                relative_path: rel_path.clone(),
                                                ..Default::default()
                                            },
                                        );
                                        (
                                            ToolResultMessage {
                                                call_id: call.call_id.clone(),
                                                tool_name: call.name.clone(),
                                                outcome: out.outcome,
                                                content: Arc::from(out.content),
                                                meta: out.meta,
                                            },
                                            format!("deny:{}", reason),
                                        )
                                    } else {
                                        // Gate 2: resolve the stub's raw evidence against the live
                                        // projector store (seeded from the WAL on `--resume`).
                                        let out =
                                            execute_recall_evidence(projector, &call.raw_arguments);
                                        if out.outcome.is_success() {
                                            projector.record_recall_hit();
                                        } else {
                                            projector.record_recall_miss();
                                        }
                                        (
                                            ToolResultMessage {
                                                call_id: call.call_id.clone(),
                                                tool_name: call.name.clone(),
                                                outcome: out.outcome,
                                                content: Arc::from(out.content),
                                                meta: out.meta,
                                            },
                                            "allow_evidence".to_string(),
                                        )
                                    };
                                hooks.extend(
                                    self.tool_runtime
                                        .post_tool_hooks(
                                            call,
                                            budget_tracker.turns_used(),
                                            &message,
                                        )
                                        .await,
                                );
                                (message, decision, hooks)
                            }
                        }
                    } else if spawn_subagent_available {
                        self.execute_spawn_subagent_call(
                            call,
                            budget_tracker.turns_used(),
                            modified_files_set,
                            logger,
                        )
                        .await
                    } else {
                        let call_exec_ctx = ExecutionContext::new(
                            Some(self.config.run_id),
                            Some(budget_tracker.deadline()),
                            Some(self.cancel_rx.clone()),
                            Some(Arc::clone(&self.token_budget)),
                            Some(Arc::new(self.tool_runtime.policy_engine().clone())),
                        )
                        .with_revision(self.current_revision_id.clone());
                        self.tool_runtime
                            .execute_call_with_context(
                                call,
                                budget_tracker.turns_used(),
                                &call_exec_ctx,
                            )
                            .await
                    };
                    for approval_ev in self.tool_runtime.policy_engine().drain_approval_events() {
                        let _ = logger.log(approval_ev);
                    }
                    Self::log_hook_evaluations(logger, &serial_hook_evaluations);

                    // A call denied by a `PreToolUse` policy hook never executed, so it must not
                    // count toward the loop/oscillation detectors.
                    if decision == "deny_policy_hook" {
                        loop_guard.rollback_recorded(&LoopGuard::compute_signature(
                            &call.name,
                            &call.raw_arguments,
                        ));
                    }

                    // O1: a `bash` call or a mutating/side-effect extension tool can rewrite the
                    // workspace without setting `modified_file`, so flag the batch for a post-batch
                    // revision capture.
                    let is_side_effect_or_mutating = call.name == "bash"
                        || self
                            .tool_runtime
                            .registry()
                            .lookup_for_dispatch(
                                &call.name,
                                self.tool_runtime.policy_engine().mode(),
                            )
                            .is_ok_and(|reg| {
                                matches!(
                                    reg.metadata.effect_class,
                                    EffectClass::Mutating | EffectClass::SideEffect
                                )
                            });
                    // F01 (`v1.5.1`): a dispatched `bash` / side-effect tool can have written to the
                    // workspace even when it reported failure (non-zero exit, timeout, cancellation,
                    // output flood) or an extension error. The snapshot trigger must follow dispatch,
                    // not business success; a pre-dispatch refusal must not fabricate a mutation.
                    if is_side_effect_or_mutating
                        && crate::tool::runtime::decision_dispatched(&decision)
                    {
                        self.batch_ran_side_effect = true;
                    }

                    let _ = logger.log(EventPayload::PolicyDecision {
                        call_id: call.call_id.clone(),
                        tool_name: call.name.clone(),
                        decision,
                        reason: None,
                    });

                    if let Some(path) = msg.meta.relative_path.clone() {
                        let streak = loop_guard.record_file_tool_outcome(
                            &msg.tool_name,
                            &path,
                            msg.outcome.is_success(),
                        );
                        if msg.tool_name == "edit" && !msg.outcome.is_success() {
                            ToolRuntime::augment_edit_streak_warning(&mut msg, streak);
                        }
                        if msg.meta.modified_file {
                            modified_files_set.insert(path.clone());
                            self.batch_mutated = true;
                        }
                        if msg.tool_name == "read" && msg.outcome.is_success() {
                            projector.notify_file_read(&path);
                        }
                    }

                    let _ = logger.log(EventPayload::ToolCompleted {
                        call_id: msg.call_id.clone(),
                        tool_name: msg.tool_name.clone(),
                        relative_path: msg.meta.relative_path.clone(),
                        duration_ms: msg.meta.duration_ms,
                        truncated: msg.meta.truncated,
                        exit_code: msg.meta.exit_code,
                        outcome: if msg.outcome.is_success() {
                            "success".to_string()
                        } else {
                            "error".to_string()
                        },
                        edit_match_tier: msg.meta.edit_match_tier,
                        diag_kind: msg.meta.diag_kind,
                        compaction_reread_count: projector.compaction_reread_count(),
                    });

                    self.record_tool_result(msg, transcript, budget_tracker, logger);
                }
            }

            idx += 1;
        }

        None
    }

    /// Emit `HookEvaluated` telemetry for every Gate 3 hook that contributed to a tool call.
    fn log_hook_evaluations(logger: &mut EventLogger, evaluations: &[HookEvaluation]) {
        for evaluation in evaluations {
            let _ = logger.log(EventPayload::HookEvaluated {
                hook_id: evaluation.hook_id.clone(),
                hook_kind: evaluation.hook_kind.clone(),
                event: evaluation.event.clone(),
                outcome: evaluation.outcome.clone(),
                duration_ms: evaluation.duration_ms,
                message: evaluation.message.clone(),
            });
        }
    }

    /// Gate 5: execute a `spawn_subagent` call through the PreToolUse policy-hook gate, kernel
    /// `PolicyEngine`, subagent spawn budget, isolated worktree materialization, concurrent
    /// execution, three-way merge, post-merge verification, and PostToolUse observer hooks.
    async fn execute_spawn_subagent_call(
        &mut self,
        call: &ToolCall,
        relative_turn: usize,
        modified_files_set: &mut HashSet<String>,
        logger: &mut EventLogger,
    ) -> (ToolResultMessage, String, Vec<HookEvaluation>) {
        match self
            .tool_runtime
            .pre_tool_hook_gate(call, relative_turn)
            .await
        {
            Err((denied, mut hooks)) => {
                hooks.extend(
                    self.tool_runtime
                        .post_tool_hooks(call, relative_turn, &denied)
                        .await,
                );
                (denied, "deny_policy_hook".to_string(), hooks)
            }
            Ok(mut hooks) => {
                let (out, decision) = self
                    .execute_spawn_subagent_inner(call, relative_turn, modified_files_set, logger)
                    .await;
                let message = ToolResultMessage {
                    call_id: call.call_id.clone(),
                    tool_name: call.name.clone(),
                    outcome: out.outcome,
                    content: Arc::from(out.content),
                    meta: out.meta,
                };
                hooks.extend(
                    self.tool_runtime
                        .post_tool_hooks(call, relative_turn, &message)
                        .await,
                );
                (message, decision, hooks)
            }
        }
    }

    /// `v1.3.0`: run a Best-of-N worktree tournament and merge only the best verified candidate.
    ///
    /// Mirrors the `session.tournament` RPC path: every candidate races in its own isolated
    /// worktree, is verified inside that worktree, and only a gate-passing winner is merged. When no
    /// candidate passes, the parent workspace is left byte-for-byte untouched.
    /// F10 (`v1.5.0`): move any pending child subagent / tournament token spend into the parent
    /// budget tracker exactly once. Both the post-tool-batch path and the adaptive escalation path
    /// call this, so child cost is never dropped when a run ends without another tool batch.
    fn drain_children_tokens(&mut self, budget_tracker: &mut BudgetTracker) {
        if self.children_tokens_pending_input > 0 || self.children_tokens_pending_output > 0 {
            budget_tracker.record_children_token_usage(
                self.children_tokens_pending_input,
                Some(self.children_tokens_pending_cached),
                self.children_tokens_pending_output,
            );
            self.children_tokens_pending_input = 0;
            self.children_tokens_pending_output = 0;
            self.children_tokens_pending_cached = 0;
        }
        if let Some(basis) = self.children_usage_pending.take() {
            budget_tracker.record_children_usage_basis(&basis);
        }
    }

    fn record_child_usage_metadata(&mut self, cached: Option<u64>, basis: &str) {
        self.children_tokens_pending_cached = self
            .children_tokens_pending_cached
            .saturating_add(cached.unwrap_or(0));
        self.children_usage_pending = Some(self.children_usage_pending.as_deref().map_or_else(
            || basis.to_string(),
            |previous| crate::budget::combine_usage_basis(previous, basis).to_string(),
        ));
    }

    /// Run a Best-of-N tournament and merge the winning candidate.
    ///
    /// Returns the tool output, the decision label, and whether a candidate was merged into the
    /// parent workspace. The merged flag lets callers detect a merge without relying on
    /// `current_revision_id`, which (matching the plain worker path) is left for
    /// [`Self::capture_workspace_revision`] to advance so the merge is durably recorded in the WAL.
    async fn execute_spawn_tournament(
        &mut self,
        request: TournamentRequest,
        call_id: &crate::types::ToolCallId,
        absolute_turn: usize,
        modified_files_set: &mut HashSet<String>,
        logger: &mut EventLogger,
        decision_label: String,
    ) -> (ToolExecutionOutput, String, bool) {
        let store = self.snapshot_store.clone().unwrap_or_else(|| {
            Arc::new(SnapshotStore::new(
                self.tool_runtime.env().state_dir().join("snapshots"),
            ))
        });
        let workspace = self.tool_runtime.env().workspace().clone();

        // Capture a fresh baseline of the live parent workspace so every candidate forks from the
        // exact bytes on disk right now.
        let baseline = match store.capture_revision(
            &workspace,
            self.current_revision_id.as_ref(),
            None,
            absolute_turn,
        ) {
            Ok(rev) => rev,
            Err(e) => {
                return (
                    ToolExecutionOutput::error(
                        ErrorCode::IoError,
                        format!("failed to capture tournament baseline revision: {}", e),
                        ToolResultMeta::default(),
                    ),
                    decision_label,
                    false,
                );
            }
        };
        let base_revision = baseline.revision_id.clone();

        let strategies = [
            "Fix the root cause with the smallest possible change.",
            "Consider an alternative design and pick the most robust minimal fix.",
            "Re-derive the failure from first principles before editing.",
            "Prefer a defensive fix that also adds a regression guard.",
        ];
        let mut candidates: Vec<TournamentCandidate> = Vec::with_capacity(request.candidates);
        for index in 0..request.candidates {
            let label = format!("c{}", index + 1);
            let strategy = strategies[index % strategies.len()].to_string();
            let mut spec = crate::subagents::SubagentSpec::worker(format!(
                "{}\n\n[TOURNAMENT CANDIDATE {} STRATEGY] {}",
                request.task, label, strategy
            ));
            if let Some(globs) = &request.allowed_write_globs {
                spec = spec.with_allowed_write_globs(globs.clone());
            }
            if let Some(turns) = request.max_turns {
                spec = spec.with_max_turns(turns);
            }
            spec = spec.with_timeout_ms(request.timeout_ms.max(1));
            candidates.push(TournamentCandidate {
                label,
                strategy: Some(strategy),
                spec,
            });
        }

        self.subagents_spawned += request.candidates;
        for candidate in &candidates {
            let _ = logger.log(EventPayload::SubagentSpawned {
                subagent_id: format!("tournament-{}", candidate.label),
                role: "worker".to_string(),
                max_turns: candidate.spec.budget.max_turns,
                isolated_worktree: true,
            });
        }

        // F04 (`v1.5.1`): bound the tournament by the parent run's remaining wall-clock time —
        // both the candidate budget and the tournament's own timeout.
        let deadline = self.run_deadline;
        let verification = crate::subagents::resolve_tournament_verification(
            self.tournament_require_verified,
            request.require_verified,
        );
        let audit_reason = match verification.source {
            crate::subagents::VerificationPolicySource::ModelDowngradeIgnored => Some(
                "model requested require_verified=false but the host requires verification; the \
                 request was ignored"
                    .to_string(),
            ),
            crate::subagents::VerificationPolicySource::ModelRequestedStrict => Some(
                "model requested stricter verification than the host required; granted".to_string(),
            ),
            crate::subagents::VerificationPolicySource::HostRelaxed => Some(
                "operator-owned relaxed verification policy: a skipped gate may still win"
                    .to_string(),
            ),
            crate::subagents::VerificationPolicySource::HostStrict => None,
        };
        // T01 (KR-01): host-owned protection wins; task-level arguments can only add to it.
        let disallow_test_tampering = crate::subagents::effective_disallow_test_tampering(
            self.host_disallow_test_tampering,
            request.disallow_test_tampering,
        );
        let protected_paths = crate::subagents::merge_protected_globs(
            &self.host_protected_paths,
            &request.protected_paths,
        );

        // F04: the same clamped budget and timeout are approved and executed.
        let effective_budget = deadline.map_or_else(
            || self.config.budget.clone(),
            |d| {
                self.config
                    .budget
                    .clamped_to_deadline(d, self.config.budget.max_model_request_attempts)
            },
        );
        let effective_timeout_ms = deadline.map_or(request.timeout_ms.max(1), |d| {
            let remaining_ms = d.remaining().as_millis().min(u64::MAX as u128) as u64;
            request.timeout_ms.max(1).min(remaining_ms.max(1))
        });

        // T02 (KR-30): build the one normalized intent that the hook, the audit trail and the run
        // all share. A hook therefore sees the real task, every expanded candidate strategy, the
        // effective budget/timeout, the merged protection set and the resolved verification policy
        // — never a `role=worker` placeholder.
        let intent = crate::subagents::CanonicalTournamentIntent {
            task: request.task.clone(),
            candidates: candidates.len(),
            strategies: candidates
                .iter()
                .filter_map(|c| c.strategy.clone())
                .collect(),
            allowed_paths: request.allowed_write_globs.clone(),
            max_turns: request.max_turns,
            timeout_ms: effective_timeout_ms,
            disallow_test_tampering,
            protected_paths: protected_paths.clone(),
            require_verified: verification.require_verified,
            verification_policy_source: verification.source.as_str().to_string(),
            budget: crate::subagents::CanonicalTournamentBudget::from_config(&effective_budget),
            predictive_check: self.tool_runtime.predictive_check_enabled(),
        };
        let intent_json = intent.canonical_json();
        let hook_call = crate::messages::ToolCall {
            call_id: call_id.clone(),
            name: "spawn_subagent".to_string(),
            raw_arguments: intent_json,
        };
        if let Err((denied, _)) = self
            .tool_runtime
            .pre_tool_hook_gate(&hook_call, absolute_turn)
            .await
        {
            return (
                ToolExecutionOutput::error(
                    ErrorCode::PolicyDenied,
                    format!(
                        "tournament denied by policy hook on the canonical intent: {}",
                        denied.content
                    ),
                    ToolResultMeta::default(),
                ),
                "deny_hook".to_string(),
                false,
            );
        }
        let intent_digest = intent.digest();
        let _ = logger.log(EventPayload::PolicyDecision {
            call_id: call_id.clone(),
            tool_name: "spawn_subagent".to_string(),
            decision: "allow".to_string(),
            reason: Some(match &audit_reason {
                Some(reason) => format!(
                    "tournament_intent digest={} candidates={} require_verified={} verification_policy={} {}",
                    intent_digest,
                    intent.candidates,
                    intent.require_verified,
                    verification.source.as_str(),
                    reason
                ),
                None => format!(
                    "tournament_intent digest={} candidates={} require_verified={} verification_policy={}",
                    intent_digest,
                    intent.candidates,
                    intent.require_verified,
                    verification.source.as_str()
                ),
            }),
        });

        let spec = TournamentSpec {
            candidates,
            worktree_base: self.tool_runtime.env().state_dir().join("worktrees"),
            state_base: self.tool_runtime.env().state_dir().join("subagents"),
            allowed_write_globs: request.allowed_write_globs.clone(),
            budget: effective_budget,
            // F05 (`v1.5.1`): candidates reserve from the same run-tree ledger as the parent and
            // its plain subagents, so a race cannot overspend the remaining budget.
            token_budget: Some(Arc::clone(&self.token_budget)),
            policy_mode: self.tool_runtime.policy_engine().mode(),
            // B11b: read the parent's fail-closed policy-hook constraints from the real registry so
            // a tournament candidate is refused rather than run without the parent's constraints.
            parent_policy_hooks: crate::subagents::ParentPolicyHooks::from_registry(
                self.tool_runtime.registry(),
            ),
            workspace_skills: self.tool_runtime.registry().skills(),
            predictive_check: self.tool_runtime.predictive_check_enabled(),
            cancel_rx: self.cancel_rx.clone(),
            timeout_ms: effective_timeout_ms,
            // Test path protection remains an independent policy: verified repairs may
            // legitimately add new regression tests. Frozen oracles need their own host-
            // managed protected paths (T19), not a blanket ban on editing tests.
            disallow_test_tampering,
            // Tool-provided paths can only expand, never shrink, the protection set.
            protected_paths,
            // Model options can request stricter verification, but cannot relax host policy.
            require_verified: verification.require_verified,
        };

        let outcome = match run_worktree_tournament(
            spec,
            &workspace,
            &store,
            Arc::clone(&self.subagent_backend),
            Some(&base_revision),
            &self.completion_gate,
        )
        .await
        {
            Ok(outcome) => outcome,
            Err(e) => {
                return (
                    ToolExecutionOutput::error(
                        ErrorCode::IoError,
                        format!("tournament failed: {}", e),
                        ToolResultMeta::default(),
                    ),
                    decision_label,
                    false,
                );
            }
        };

        for score in &outcome.scores {
            let _ = logger.log(EventPayload::SubagentCompleted {
                subagent_id: format!("tournament-{}", score.label),
                role: "worker".to_string(),
                status: score.status.clone(),
                turns_used: score.turns_used,
                tool_calls_used: score.tool_calls_used,
                tokens_used: score.input_tokens.saturating_add(score.output_tokens),
                duration_ms: score.duration_ms,
            });
        }
        let _ = logger.log(EventPayload::TournamentCompleted {
            adaptive: decision_label == "adaptive_tournament",
            candidate_runs: outcome.scores.iter().filter(|s| s.turns_used > 0).count(),
            verified_candidates: outcome.scores.iter().filter(|s| s.gate_passed).count(),
            merged: outcome.merged_revision.is_some(),
        });
        if let Some(merge) = &outcome.merge {
            let (written_files, removed_files, conflict_paths, revision_after) = match merge {
                WorktreeMergeResult::Applied {
                    written_files,
                    removed_files,
                    merged_revision,
                    ..
                } => (
                    written_files.clone(),
                    removed_files.clone(),
                    Vec::new(),
                    Some(merged_revision.as_str().to_string()),
                ),
                WorktreeMergeResult::Conflict {
                    conflicting_paths, ..
                } => (Vec::new(), Vec::new(), conflicting_paths.clone(), None),
                WorktreeMergeResult::VerificationFailed { .. } => {
                    (Vec::new(), Vec::new(), Vec::new(), None)
                }
            };
            let _ = logger.log(EventPayload::WorktreeMerged {
                subagent_id: outcome
                    .winner
                    .map(|i| format!("tournament-{}", outcome.scores[i].label))
                    .unwrap_or_else(|| "tournament".to_string()),
                status: merge.status_str().to_string(),
                written_files,
                removed_files,
                conflict_paths,
                revision_after,
            });
        }

        let merged = if let Some(rev) = outcome.merged_revision.clone() {
            let written: Vec<String> = store
                .load_revision(&rev)
                .map(|m| m.changed_paths.clone())
                .unwrap_or_default();
            for path in &written {
                modified_files_set.insert(path.clone());
            }
            self.batch_mutated = true;
            // P1 (`v1.5.0`): mirror the plain worker path. When a snapshot store is present, leave
            // `current_revision_id` untouched so the next `capture_workspace_revision` observes the
            // content change, appends a `RevisionCaptured` record to the session WAL, and `/undo`
            // can revert the merge. Setting it here made the capture see an "unchanged" workspace
            // and skip the WAL record entirely. Without a store there is nothing to record, so the
            // in-memory id is advanced for callers that detect merges by revision change.
            if self.snapshot_store.is_none() {
                self.current_revision_id = Some(rev);
            }
            true
        } else {
            false
        };

        // F10 (`v1.4.0`): aggregate the token spend of every tournament candidate, including the
        // losers and any candidate that failed or was cancelled.
        self.record_child_usage_metadata(outcome.total_cached_input_tokens, &outcome.usage_basis);
        self.children_tokens_pending_input = self
            .children_tokens_pending_input
            .saturating_add(outcome.total_input_tokens);
        self.children_tokens_pending_output = self
            .children_tokens_pending_output
            .saturating_add(outcome.total_output_tokens);

        let report = outcome.render_report();
        (
            ToolExecutionOutput::success(report, ToolResultMeta::default()),
            decision_label,
            merged,
        )
    }

    async fn execute_spawn_subagent_inner(
        &mut self,
        call: &ToolCall,
        relative_turn: usize,
        modified_files_set: &mut HashSet<String>,
        logger: &mut EventLogger,
    ) -> (ToolExecutionOutput, String) {
        let started = std::time::Instant::now();
        if self.max_subagent_spawns == 0 {
            return (
                ToolExecutionOutput::error(
                    ErrorCode::PolicyDenied,
                    "spawn_subagent is disabled for this session (--no-subagents / max_subagents=0)"
                        .to_string(),
                    ToolResultMeta::default(),
                ),
                "deny:subagents_disabled".to_string(),
            );
        }

        // `v1.3.0`: `"mode": "tournament"` races N `Worker` candidates in isolated worktrees and
        // merges only the single best verified candidate. Detected before the plain fan-out parser,
        // which requires an explicit `role`.
        let tournament_request = match parse_tournament_request(&call.raw_arguments) {
            Ok(r) => r,
            Err(err) => {
                return (
                    ToolExecutionOutput::error(
                        ErrorCode::InvalidArguments,
                        err,
                        ToolResultMeta::default(),
                    ),
                    "deny_invalid_args".to_string(),
                );
            }
        };

        let specs = if tournament_request.is_some() {
            Vec::new()
        } else {
            match parse_spawn_subagent_specs(&call.raw_arguments) {
                Ok(s) => s,
                Err(err) => {
                    return (
                        ToolExecutionOutput::error(
                            ErrorCode::InvalidArguments,
                            err,
                            ToolResultMeta::default(),
                        ),
                        "deny_invalid_args".to_string(),
                    );
                }
            }
        };

        let requested_spawns = tournament_request
            .as_ref()
            .map(|t| t.candidates)
            .unwrap_or(specs.len());

        let has_worker =
            tournament_request.is_some() || specs.iter().any(|s| s.role == SubagentRole::Worker);
        let effective_effect = if has_worker {
            EffectClass::Mutating
        } else {
            EffectClass::ReadOnly
        };

        // F01 (`v1.4.0`): the shared quota + policy preflight (identical to the RPC tournament
        // entry point) so every entry path enforces the same `max_subagents` cap and policy mode.
        let check = match crate::subagents::subagent_spawn_preflight(
            self.tool_runtime.policy_engine(),
            requested_spawns,
            self.subagents_spawned,
            self.max_subagent_spawns,
            effective_effect,
        ) {
            Ok(check) => check,
            Err(reason) => {
                return (
                    ToolExecutionOutput::error(
                        ErrorCode::PolicyDenied,
                        reason,
                        ToolResultMeta::default(),
                    ),
                    "deny:subagent_budget_exceeded".to_string(),
                );
            }
        };
        let decision_label = match check {
            PolicyCheckResult::Allow => {
                if has_worker {
                    "allow_subagent_worker".to_string()
                } else {
                    "allow_subagent_scout".to_string()
                }
            }
            PolicyCheckResult::Deny(reason) => {
                return (
                    ToolExecutionOutput::error(
                        ErrorCode::PolicyDenied,
                        format!(
                            "{} (spawn_subagent role={})",
                            reason,
                            if has_worker { "worker" } else { "scout" }
                        ),
                        ToolResultMeta::default(),
                    ),
                    format!("deny:{}", reason),
                );
            }
            PolicyCheckResult::Ask => {
                if *self.cancel_rx.borrow() {
                    return (
                        ToolExecutionOutput::error(
                            ErrorCode::NotExecutedCancelled,
                            "spawn_subagent cancelled before approval prompt".to_string(),
                            ToolResultMeta::default(),
                        ),
                        "ask_cancelled".to_string(),
                    );
                }
                let req = ApprovalRequest {
                    tool_name: call.name.clone(),
                    call_id: Some(call.call_id.clone()),
                    effect_class: effective_effect,
                    is_sensitive_path: false,
                    target_path: None,
                    diff_preview: None,
                    command_preview: Some(call.raw_arguments.clone()),
                    workspace_root: self.tool_runtime.env().workspace().root().to_path_buf(),
                };
                // B11a: bind the approval to the subagent entry point and this run, so a grant for
                // the kernel tool dispatcher (or another run) can never authorize a spawn.
                let auth_scope = AuthScope {
                    entry: AuthEntry::Subagent,
                    tool_name: call.name.clone(),
                    run_id: Some(self.config.run_id),
                    workspace_root: self.tool_runtime.env().workspace().root().to_path_buf(),
                    revision: self.current_revision_id.clone(),
                };
                let args_digest = canonical_arguments_digest(&call.raw_arguments);
                let budget_remaining = self.run_deadline.map(|d| d.remaining());
                let pe = self.tool_runtime.policy_engine().clone();
                let scope_for_prompt = auth_scope.clone();
                let digest_for_prompt = args_digest.clone();
                let mut approval_cancel_rx = self.cancel_rx.clone();
                let mut prompt_task = tokio::task::spawn_blocking(move || {
                    pe.prompt_approval_scoped(
                        &scope_for_prompt,
                        &req,
                        digest_for_prompt.as_deref(),
                        budget_remaining,
                    )
                });
                let decision = tokio::select! {
                    res = &mut prompt_task => res.unwrap_or(ApprovalDecision::Deny),
                    _ = async {
                        loop {
                            if *approval_cancel_rx.borrow() {
                                break;
                            }
                            if approval_cancel_rx.changed().await.is_err() {
                                std::future::pending::<()>().await;
                            }
                        }
                    } => {
                        let _ = tokio::time::timeout(
                            std::time::Duration::from_millis(200),
                            &mut prompt_task,
                        )
                        .await;
                        for approval_ev in self.tool_runtime.policy_engine().drain_approval_events() {
                            let _ = logger.log(approval_ev);
                        }
                        return (
                            ToolExecutionOutput::error(
                                ErrorCode::NotExecutedCancelled,
                                "spawn_subagent approval cancelled by user".to_string(),
                                ToolResultMeta::default(),
                            ),
                            "ask_cancelled".to_string(),
                        );
                    }
                };
                for approval_ev in self.tool_runtime.policy_engine().drain_approval_events() {
                    let _ = logger.log(approval_ev);
                }
                if *self.cancel_rx.borrow() {
                    return (
                        ToolExecutionOutput::error(
                            ErrorCode::NotExecutedCancelled,
                            "spawn_subagent approval cancelled by user".to_string(),
                            ToolResultMeta::default(),
                        ),
                        "ask_cancelled".to_string(),
                    );
                }
                match decision {
                    ApprovalDecision::AllowOnce | ApprovalDecision::AllowCategoryForRun => {
                        // B11a: consume the grant the approval just minted for this exact scope and
                        // argument digest. Anything but a base-policy `Missing` fails closed.
                        if let Some(ref digest) = args_digest {
                            if let Err(rejection) = self
                                .tool_runtime
                                .policy_engine()
                                .consume_grant(&auth_scope, digest)
                            {
                                if !matches!(rejection, GrantRejection::Missing) {
                                    return (
                                        ToolExecutionOutput::error(
                                            ErrorCode::PolicyDenied,
                                            format!(
                                                "spawn_subagent approval grant rejected: {:?}",
                                                rejection
                                            ),
                                            ToolResultMeta::default(),
                                        ),
                                        "deny_grant_rejected".to_string(),
                                    );
                                }
                            }
                        }
                        "ask_approved".to_string()
                    }
                    ApprovalDecision::Deny => {
                        return (
                            ToolExecutionOutput::error(
                                ErrorCode::PolicyDenied,
                                "spawn_subagent execution denied by approval policy".to_string(),
                                ToolResultMeta::default(),
                            ),
                            "ask_denied".to_string(),
                        );
                    }
                }
            }
        };

        let absolute_turn = self.session_turn_offset.saturating_add(relative_turn);

        if let Some(request) = tournament_request {
            let (output, label, _merged) = self
                .execute_spawn_tournament(
                    request,
                    &call.call_id,
                    absolute_turn,
                    modified_files_set,
                    logger,
                    decision_label,
                )
                .await;
            return (output, label);
        }

        // If earlier tools in this same batch mutated the parent workspace, capture a revision now
        // so any worker subagent starts from the up-to-date parent state.
        if has_worker && (self.batch_mutated || self.batch_ran_side_effect) {
            let prior_changed = self.capture_workspace_revision(absolute_turn, "mutation", logger);
            for path in &prior_changed {
                modified_files_set.insert(path.clone());
            }
            self.mutation_ledger.backfill_revision_and_paths(
                absolute_turn as u32,
                self.current_revision_id.clone(),
                &prior_changed,
                self.snapshot_store.is_some(),
            );
            self.batch_mutated = false;
            self.batch_ran_side_effect = false;
        }

        let store = self.snapshot_store.clone().unwrap_or_else(|| {
            Arc::new(SnapshotStore::new(
                self.tool_runtime.env().state_dir().join("snapshots"),
            ))
        });

        if has_worker && self.current_revision_id.is_none() {
            match store.capture_revision(
                self.tool_runtime.env().workspace(),
                None,
                None,
                absolute_turn,
            ) {
                Ok(rev) => {
                    self.current_revision_id = Some(rev.revision_id);
                }
                Err(e) => {
                    return (
                        ToolExecutionOutput::error(
                            ErrorCode::IoError,
                            format!(
                                "failed to capture baseline revision for worker worktree: {}",
                                e
                            ),
                            ToolResultMeta::default(),
                        ),
                        decision_label,
                    );
                }
            }
        }

        let worktrees_base = self.tool_runtime.env().state_dir().join("worktrees");
        let subagents_state_base = self.tool_runtime.env().state_dir().join("subagents");
        let workspace_skills = self.tool_runtime.registry().skills();
        let parent_policy_mode = self.tool_runtime.policy_engine().mode();
        // F04 (`v1.5.1`): clamp the child-run ceiling to the parent's remaining wall-clock time, so
        // `to_budget_config` cannot grant a subagent a longer run than the parent has left.
        let parent_budget = match self.run_deadline {
            Some(d) => self
                .config
                .budget
                .clamped_to_deadline(d, self.config.budget.max_model_request_attempts),
            None => self.config.budget.clone(),
        };
        let predictive_check = self.tool_runtime.predictive_check_enabled();

        // B11b: refuse an unsupported delegation *before* any worktree or state dir is materialized,
        // so a rejection leaves no side effect (the backend repeats this check as the final gate).
        if let Err(reason) =
            crate::subagents::ParentPolicyHooks::from_registry(self.tool_runtime.registry())
                .ensure_delegatable()
        {
            return (
                ToolExecutionOutput::error(
                    ErrorCode::PolicyDenied,
                    reason,
                    ToolResultMeta::default(),
                ),
                "deny:unsupported_delegation".to_string(),
            );
        }
        for spec in &specs {
            if let Err(reason) =
                crate::subagents::child_policy_mode_for(parent_policy_mode, spec.role)
            {
                return (
                    ToolExecutionOutput::error(
                        ErrorCode::PolicyDenied,
                        reason,
                        ToolResultMeta::default(),
                    ),
                    "deny:capability_decay".to_string(),
                );
            }
        }

        let mut prepared = Vec::with_capacity(specs.len());
        for spec in specs {
            self.subagents_spawned += 1;
            let subagent_id = format!("sub-{:04}", self.subagents_spawned);
            let _ = logger.log(EventPayload::SubagentSpawned {
                subagent_id: subagent_id.clone(),
                role: spec.role.as_str().to_string(),
                max_turns: spec.budget.max_turns,
                isolated_worktree: spec.role == SubagentRole::Worker,
            });
            let worktree = if spec.role == SubagentRole::Worker {
                let wt_dir = worktrees_base.join(format!("wt-{}", subagent_id));
                match IsolatedWorktree::materialize(
                    &subagent_id,
                    self.tool_runtime.env().workspace(),
                    &store,
                    self.current_revision_id.as_ref(),
                    wt_dir,
                ) {
                    Ok(wt) => Some(wt),
                    Err(e) => {
                        return (
                            ToolExecutionOutput::error(
                                ErrorCode::IoError,
                                format!(
                                    "failed to materialize isolated worktree for {}: {}",
                                    subagent_id, e
                                ),
                                ToolResultMeta::default(),
                            ),
                            decision_label,
                        );
                    }
                }
            } else {
                None
            };
            prepared.push((subagent_id, spec, worktree));
        }

        self.run_subagent_batch(SubagentBatchPlan {
            prepared,
            subagents_state_base,
            workspace_skills,
            parent_budget,
            parent_policy_mode,
            predictive_check,
            store,
            absolute_turn,
            decision_label,
            logger,
            modified_files_set,
            started,
        })
        .await
    }

    /// Materialise, run and collect the isolated worktree workers for a single `spawn_subagent`
    /// call, then merge the surviving child edits back into the parent workspace.
    ///
    /// Split out of [`Self::execute_spawn_subagent_inner`] (which was 717 lines). The inputs travel
    /// in a [`SubagentBatchPlan`] so this phase takes one argument, and the body below is the
    /// original code verbatim — the destructuring `let` re-binds the same local names.
    async fn run_subagent_batch(
        &mut self,
        plan: SubagentBatchPlan<'_>,
    ) -> (ToolExecutionOutput, String) {
        let SubagentBatchPlan {
            prepared,
            subagents_state_base,
            workspace_skills,
            parent_budget,
            parent_policy_mode,
            predictive_check,
            store,
            absolute_turn,
            decision_label,
            logger,
            modified_files_set,
            started,
        } = plan;
        let mut join_handles = Vec::with_capacity(prepared.len());
        let mut worktree_slots: Vec<(
            String,
            crate::subagents::SubagentSpec,
            Option<IsolatedWorktree>,
            Option<String>,
        )> = Vec::with_capacity(prepared.len());
        // F05 (`v1.5.1`): reserve a per-child token allocation out of the run tree's shared ledger
        // *before* launching the child, so concurrent children cannot both spend the same balance.
        // The child's ledger is limited to its allocation; after the child ends the reservation is
        // settled with what the child actually consumed and the remainder returns to the pool.
        let mut child_ledgers: Vec<Arc<TokenBudget>> = Vec::with_capacity(prepared.len());
        let mut child_reservations: Vec<TokenReservation> = Vec::with_capacity(prepared.len());
        let mut budget_rejections: Vec<SubagentResult> = Vec::new();
        // B11b: read the parent's fail-closed policy-hook constraints once from the real registry.
        // The child runs in its own registry and cannot execute these hooks, so the chokepoint
        // refuses the delegation instead of running an unconstrained child.
        let parent_policy_hooks =
            crate::subagents::ParentPolicyHooks::from_registry(self.tool_runtime.registry());

        for (subagent_id, spec, worktree) in prepared {
            let child_workspace = match &worktree {
                Some(wt) => wt.workspace().clone(),
                None => self.tool_runtime.env().workspace().clone(),
            };
            let backend = Arc::clone(&self.subagent_backend);
            let mut parent_cancel = self.cancel_rx.clone();
            let timeout_ms = spec.budget.timeout_ms;
            let req_id = subagent_id.clone();
            let req_spec = spec.clone();
            let child_state_dir = subagents_state_base.join(&subagent_id);
            let skills = workspace_skills.clone();
            let budget_clone = parent_budget.clone();

            let allocation = match self.token_budget.remaining() {
                Some(remaining) => spec.budget.clamped().max_tokens.min(remaining),
                None => spec.budget.clamped().max_tokens,
            };
            if allocation == 0 {
                if let Some(mut wt) = worktree {
                    let _ = wt.cleanup();
                }
                budget_rejections.push(SubagentResult {
                    subagent_id,
                    role: spec.role,
                    status: RunStatus::BudgetExhausted,
                    terminal_reason: format!("{:?}", TerminalReason::TokenBudgetExhausted),
                    summary: "subagent skipped: no token budget remaining".to_string(),
                    relevant_files: Vec::new(),
                    findings: Vec::new(),
                    turns_used: 0,
                    tool_calls_used: 0,
                    tokens_used: 0,
                    duration_ms: started.elapsed().as_millis() as u64,
                    merge_result: None,
                    worktree_cleaned_up: true,
                });
                continue;
            }
            let reservation = match self.token_budget.try_reserve(allocation) {
                Some(reservation) => reservation,
                // A concurrent request or sibling batch may have taken the balance between the
                // `remaining()` read above and this reservation. Lose gracefully: skip the child
                // (zero backend calls) instead of treating an impossible-in-isolation allocation
                // as a hard invariant and panicking.
                None => {
                    if let Some(mut wt) = worktree {
                        let _ = wt.cleanup();
                    }
                    budget_rejections.push(SubagentResult {
                        subagent_id,
                        role: spec.role,
                        status: RunStatus::BudgetExhausted,
                        terminal_reason: format!("{:?}", TerminalReason::TokenBudgetExhausted),
                        summary: "subagent skipped: token budget was claimed before dispatch"
                            .to_string(),
                        relevant_files: Vec::new(),
                        findings: Vec::new(),
                        turns_used: 0,
                        tool_calls_used: 0,
                        tokens_used: 0,
                        duration_ms: started.elapsed().as_millis() as u64,
                        merge_result: None,
                        worktree_cleaned_up: true,
                    });
                    continue;
                }
            };
            let child_ledger = Arc::new(TokenBudget::new(Some(allocation)));
            child_ledgers.push(Arc::clone(&child_ledger));
            child_reservations.push(reservation);

            // Capability gap #4 (`v1.5.2`): create the child's cancel channel here (not inside the
            // spawned task) so the durable-task host can bind it before the child runs; a
            // `tasks.control { cancel }` then actually stops the live subagent.
            let (child_cancel_tx, child_cancel_rx) = watch::channel(*parent_cancel.borrow());
            let durable_task_id = match self.config.durable_tasks.as_ref() {
                Some(host) => {
                    let child = crate::task_host::ChildTaskSpec {
                        task_id: req_id.clone(),
                        spec: spec.clone(),
                        allocation,
                        turn: absolute_turn as u32,
                        parent_policy_mode,
                        parent_policy_hooks: parent_policy_hooks.clone(),
                    };
                    match host.register_child(&child, child_cancel_tx.clone()) {
                        // The host may derive a distinct durable id when the requested one is already
                        // taken by a finished record, so settle under the id it actually admitted.
                        Ok(durable_id) => Some(durable_id),
                        Err(e) => {
                            // A capacity/duplicate failure must not stop the child from running;
                            // the record is best-effort and the error is surfaced to the log.
                            eprintln!(
                                "[kroma] durable task registration failed for {}: {}",
                                req_id, e
                            );
                            None
                        }
                    }
                }
                None => None,
            };

            let child_parent_policy_hooks = parent_policy_hooks.clone();
            let handle = tokio::spawn(async move {
                let started_sub = std::time::Instant::now();
                let cancel_tx_for_signal = child_cancel_tx.clone();
                let mut parent_cancel_for_watcher = parent_cancel.clone();
                let watcher = tokio::spawn(async move {
                    tokio::select! {
                        _ = tokio::time::sleep( std::time::Duration::from_millis(timeout_ms)) => {
                            let _ = cancel_tx_for_signal.send(true);
                        }
                        _ = async {
                            loop {
                                if *parent_cancel_for_watcher.borrow() {
                                    break;
                                }
                                if parent_cancel_for_watcher.changed().await.is_err() {
                                    std::future::pending::<()>().await;
                                }
                            }
                        } => {
                            let _ = cancel_tx_for_signal.send(true);
                        }
                    }
                });

                let req = SubagentExecutionRequest {
                    subagent_id: req_id,
                    spec: req_spec,
                    workspace: child_workspace,
                    state_dir: child_state_dir,
                    workspace_skills: skills,
                    parent_policy_mode,
                    parent_policy_hooks: child_parent_policy_hooks,
                    parent_budget: budget_clone,
                    token_budget: Some(child_ledger),
                    predictive_check,
                    cancel_rx: child_cancel_rx,
                };

                // Allow the child loop a short grace window after `child_cancel_tx` fires to
                // sweep any child processes cleanly, then hard-bound the join.
                let hard_deadline =
                    std::time::Duration::from_millis(timeout_ms.saturating_add(150));
                let res = tokio::select! {
                    r = backend.run_subagent(req) => r,
                    _ = tokio::time::sleep(hard_deadline) => {
                        let _ = child_cancel_tx.send(true);
                        Ok(SubagentRunOutcome {
                            status: RunStatus::Cancelled,
                            terminal_reason: TerminalReason::RunDeadlineExceeded,
                            summary: "subagent timed out and was cancelled".to_string(),
                            relevant_files: Vec::new(),
                            findings: Vec::new(),
                            turns_used: 0,
                            tool_calls_used: 0,
                            tokens_used: 0,
                            input_tokens: 0,
                            cached_input_tokens: None,
                            usage_basis: "unknown".to_string(),
                            output_tokens: 0,
                            duration_ms: started_sub.elapsed().as_millis() as u64,
                            visible_tools: Vec::new(),
                        })
                    }
                    _ = async {
                        loop {
                            if *parent_cancel.borrow() {
                                break;
                            }
                            if parent_cancel.changed().await.is_err() {
                                std::future::pending::<()>().await;
                            }
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    } => {
                        let _ = child_cancel_tx.send(true);
                        Ok(SubagentRunOutcome {
                            status: RunStatus::Cancelled,
                            terminal_reason: TerminalReason::UserCancelled,
                            summary: "subagent cancelled by parent run".to_string(),
                            relevant_files: Vec::new(),
                            findings: Vec::new(),
                            turns_used: 0,
                            tool_calls_used: 0,
                            tokens_used: 0,
                            input_tokens: 0,
                            cached_input_tokens: None,
                            usage_basis: "unknown".to_string(),
                            output_tokens: 0,
                            duration_ms: started_sub.elapsed().as_millis() as u64,
                            visible_tools: Vec::new(),
                        })
                    }
                };
                watcher.abort();
                // Budget reservations are settled separately against the shared ledger below
                // (keeping unknown-consumption fail-closed without double charging). Factual
                // observed usage in `outcome` must reflect only measured provider tokens and
                // never mislabel withheld reserves as measured tokens.
                res
            });

            join_handles.push(handle);
            worktree_slots.push((subagent_id, spec, worktree, durable_task_id));
        }

        let mut results: Vec<SubagentResult> = budget_rejections;
        let mut batch_worker_spans = Vec::new();
        let mut first_modified_path: Option<String> = None;
        let mut in_batch_parent_rev = self.current_revision_id.clone();

        for (handle, (subagent_id, spec, mut worktree, durable_task_id)) in
            join_handles.into_iter().zip(worktree_slots.into_iter())
        {
            let outcome = match handle.await {
                Ok(Ok(o)) => o,
                Ok(Err(err)) => SubagentRunOutcome {
                    status: RunStatus::Failed,
                    terminal_reason: TerminalReason::ProviderProtocolError(err.clone()),
                    summary: format!("subagent error: {}", err),
                    relevant_files: Vec::new(),
                    findings: Vec::new(),
                    turns_used: 0,
                    tool_calls_used: 0,
                    tokens_used: 0,
                    input_tokens: 0,
                    cached_input_tokens: None,
                    usage_basis: "unknown".to_string(),
                    output_tokens: 0,
                    duration_ms: 0,
                    visible_tools: Vec::new(),
                },
                Err(join_err) => SubagentRunOutcome {
                    status: RunStatus::Cancelled,
                    terminal_reason: TerminalReason::UserCancelled,
                    summary: format!("subagent task join error: {}", join_err),
                    relevant_files: Vec::new(),
                    findings: Vec::new(),
                    turns_used: 0,
                    tool_calls_used: 0,
                    tokens_used: 0,
                    input_tokens: 0,
                    cached_input_tokens: None,
                    usage_basis: "unknown".to_string(),
                    output_tokens: 0,
                    duration_ms: 0,
                    visible_tools: Vec::new(),
                },
            };

            let status_str = match outcome.status {
                RunStatus::Completed => "completed",
                RunStatus::Cancelled => "cancelled",
                RunStatus::BudgetExhausted => "budget_exhausted",
                RunStatus::Failed => "failed",
                _ => "failed",
            };
            let _ = logger.log(EventPayload::SubagentCompleted {
                subagent_id: subagent_id.clone(),
                role: spec.role.as_str().to_string(),
                status: status_str.to_string(),
                turns_used: outcome.turns_used,
                tool_calls_used: outcome.tool_calls_used,
                tokens_used: outcome.tokens_used,
                duration_ms: outcome.duration_ms,
            });

            // F10 (`v1.4.0`): attribute every child run's spend (completed, failed or cancelled) to
            // the parent run so `children_*` cost is never silently dropped.
            self.record_child_usage_metadata(outcome.cached_input_tokens, &outcome.usage_basis);
            self.children_tokens_pending_input = self
                .children_tokens_pending_input
                .saturating_add(outcome.input_tokens);
            self.children_tokens_pending_output = self
                .children_tokens_pending_output
                .saturating_add(outcome.output_tokens);

            let mut merge_result = None;
            let mut worktree_cleaned_up = true;

            if let Some(ref mut wt) = worktree {
                // Item #3 (`v1.4.0`): a worker that edited a protected verification/test path is
                // disqualified from merging, so a fix can never be "verified" by rewriting the test.
                let worker_protected_globs = crate::subagents::effective_protected_globs(
                    spec.disallow_test_tampering,
                    &spec.protected_paths,
                );
                let tampered_paths = if worker_protected_globs.is_empty() {
                    Vec::new()
                } else {
                    match wt.delta_paths(self.tool_runtime.env().workspace(), &store) {
                        Ok(paths) => crate::subagents::protected_changed_paths(
                            &paths,
                            &worker_protected_globs,
                        ),
                        Err(_) => Vec::new(),
                    }
                };
                if !tampered_paths.is_empty() {
                    let _ = logger.log(EventPayload::SubagentCompleted {
                        subagent_id: subagent_id.clone(),
                        role: spec.role.as_str().to_string(),
                        status: "disqualified_test_tampering".to_string(),
                        turns_used: outcome.turns_used,
                        tool_calls_used: outcome.tool_calls_used,
                        tokens_used: outcome.tokens_used,
                        duration_ms: outcome.duration_ms,
                    });
                    let _ = wt.cleanup();
                    results.push(SubagentResult {
                        subagent_id,
                        role: spec.role,
                        status: RunStatus::Failed,
                        terminal_reason: "policy_denied: protected verification/test path".to_string(),
                        summary: format!(
                            "worker disqualified: it modified protected verification/test path(s): {}",
                            tampered_paths.join(", ")
                        ),
                        relevant_files: outcome.relevant_files,
                        findings: outcome.findings,
                        turns_used: outcome.turns_used,
                        tool_calls_used: outcome.tool_calls_used,
                        tokens_used: outcome.tokens_used,
                        duration_ms: outcome.duration_ms,
                        merge_result: None,
                        worktree_cleaned_up: true,
                    });
                    continue;
                }
                if outcome.status == RunStatus::Completed && !*self.cancel_rx.borrow() {
                    match wt
                        .merge_into_parent(
                            self.tool_runtime.env().workspace(),
                            &store,
                            in_batch_parent_rev.as_ref(),
                            spec.allowed_write_globs.as_deref(),
                            Some(&self.completion_gate),
                            spec.revert_on_verify_failure,
                            absolute_turn,
                        )
                        .await
                    {
                        Ok(merge) => {
                            match &merge {
                                WorktreeMergeResult::Applied {
                                    merged_revision,
                                    written_files,
                                    removed_files,
                                    ..
                                } => {
                                    let _ = logger.log(EventPayload::WorktreeMerged {
                                        subagent_id: subagent_id.clone(),
                                        status: "applied".to_string(),
                                        written_files: written_files.clone(),
                                        removed_files: removed_files.clone(),
                                        conflict_paths: Vec::new(),
                                        revision_after: Some(merged_revision.as_str().to_string()),
                                    });
                                    if !written_files.is_empty() || !removed_files.is_empty() {
                                        for p in written_files.iter().chain(removed_files.iter()) {
                                            if first_modified_path.is_none() {
                                                first_modified_path = Some(p.clone());
                                            }
                                            modified_files_set.insert(p.clone());
                                        }
                                        self.batch_mutated = true;
                                        batch_worker_spans.extend(build_worker_mutation_spans(
                                            self.tool_runtime.env().workspace(),
                                            &subagent_id,
                                            written_files,
                                            removed_files,
                                        ));
                                        in_batch_parent_rev = Some(merged_revision.clone());
                                    }
                                }
                                WorktreeMergeResult::Conflict {
                                    conflicting_paths, ..
                                } => {
                                    worktree_cleaned_up = false;
                                    let _ = logger.log(EventPayload::WorktreeMerged {
                                        subagent_id: subagent_id.clone(),
                                        status: "conflict".to_string(),
                                        written_files: Vec::new(),
                                        removed_files: Vec::new(),
                                        conflict_paths: conflicting_paths.clone(),
                                        revision_after: in_batch_parent_rev
                                            .as_ref()
                                            .map(|r| r.as_str().to_string()),
                                    });
                                }
                                WorktreeMergeResult::VerificationFailed {
                                    merged_revision,
                                    revision_before,
                                    reverted,
                                    written_files,
                                    removed_files,
                                    ..
                                } => {
                                    let _ = logger.log(EventPayload::WorktreeMerged {
                                        subagent_id: subagent_id.clone(),
                                        status: merge.status_str().to_string(),
                                        written_files: written_files.clone(),
                                        removed_files: removed_files.clone(),
                                        conflict_paths: Vec::new(),
                                        revision_after: if *reverted {
                                            revision_before.as_ref().map(|r| r.as_str().to_string())
                                        } else {
                                            Some(merged_revision.as_str().to_string())
                                        },
                                    });
                                    if !*reverted
                                        && (!written_files.is_empty() || !removed_files.is_empty())
                                    {
                                        for p in written_files.iter().chain(removed_files.iter()) {
                                            if first_modified_path.is_none() {
                                                first_modified_path = Some(p.clone());
                                            }
                                            modified_files_set.insert(p.clone());
                                        }
                                        self.batch_mutated = true;
                                        batch_worker_spans.extend(build_worker_mutation_spans(
                                            self.tool_runtime.env().workspace(),
                                            &subagent_id,
                                            written_files,
                                            removed_files,
                                        ));
                                        in_batch_parent_rev = Some(merged_revision.clone());
                                    }
                                }
                            }
                            merge_result = Some(merge);
                        }
                        Err(e) => {
                            let _ = wt.cleanup();
                            let _ = logger.log(EventPayload::WorktreeMerged {
                                subagent_id: subagent_id.clone(),
                                status: format!("error:{}", e),
                                written_files: Vec::new(),
                                removed_files: Vec::new(),
                                conflict_paths: Vec::new(),
                                revision_after: in_batch_parent_rev
                                    .as_ref()
                                    .map(|r| r.as_str().to_string()),
                            });
                        }
                    }
                } else {
                    let _ = wt.cleanup();
                    worktree_cleaned_up = wt.is_cleaned_up();
                    let _ = logger.log(EventPayload::WorktreeMerged {
                        subagent_id: subagent_id.clone(),
                        status: "aborted_cleanup".to_string(),
                        written_files: Vec::new(),
                        removed_files: Vec::new(),
                        conflict_paths: Vec::new(),
                        revision_after: in_batch_parent_rev
                            .as_ref()
                            .map(|r| r.as_str().to_string()),
                    });
                }
            }

            // Capability gap #4 (`v1.5.2`): settle the durable record with the child's real outcome,
            // spend, report artifact and notification. A `worker` whose delta conflicted on merge is
            // recorded as failed, so the durable state never claims an unmerged result.
            if let (Some(host), Some(task_id)) = (
                self.config.durable_tasks.as_ref(),
                durable_task_id.as_deref(),
            ) {
                let merged_conflict = merge_result
                    .as_ref()
                    .map(|merge| merge.is_conflict())
                    .unwrap_or(false);
                let state = match outcome.status {
                    RunStatus::Completed if !merged_conflict => {
                        crate::async_tasks::TaskState::Completed
                    }
                    RunStatus::Cancelled => crate::async_tasks::TaskState::Cancelled,
                    _ => crate::async_tasks::TaskState::Failed,
                };
                let summary = outcome.summary.clone();
                // The host persists the report bytes and builds the matching artifact reference
                // before committing the finish metadata, so no caller invents a `task:` reference
                // that has no durable bytes behind it.
                let finish = crate::task_host::ChildFinish {
                    state,
                    spent: outcome.tokens_used,
                    summary,
                    keep_pending_merge: false,
                    verification: None,
                };
                if let Err(e) = host.finish_child(task_id, &finish) {
                    eprintln!(
                        "[kroma] durable task settlement failed for {}: {}",
                        task_id, e
                    );
                }
            }

            results.push(SubagentResult {
                subagent_id,
                role: spec.role,
                status: outcome.status,
                terminal_reason: format!("{:?}", outcome.terminal_reason),
                summary: outcome.summary,
                relevant_files: outcome.relevant_files,
                findings: outcome.findings,
                turns_used: outcome.turns_used,
                tool_calls_used: outcome.tool_calls_used,
                tokens_used: outcome.tokens_used,
                duration_ms: outcome.duration_ms,
                merge_result,
                worktree_cleaned_up,
            });
        }

        if self.snapshot_store.is_none() {
            self.current_revision_id = in_batch_parent_rev;
        }

        // F05 (`v1.5.1`): settle every child allocation against what the child actually consumed;
        // the unused remainder returns to the shared ledger so a later batch can reuse it.
        for (reservation, ledger) in child_reservations.into_iter().zip(child_ledgers.iter()) {
            reservation.settle(ledger.spent());
        }

        let report_text = render_subagent_batch_report(&results);
        let modified_any = !batch_worker_spans.is_empty();
        let meta = ToolResultMeta {
            relative_path: first_modified_path,
            modified_file: modified_any,
            duration_ms: started.elapsed().as_millis() as u64,
            mutation: if modified_any {
                Some(MutationTelemetry {
                    spans: batch_worker_spans,
                    predictive: None,
                })
            } else {
                None
            },
            ..Default::default()
        };

        (
            ToolExecutionOutput::success(report_text, meta),
            decision_label,
        )
    }

    /// Gate 4: append a `MutationRecord` for one successful tool result.
    ///
    /// `edit`/`write` carry their spans in `meta.mutation` (built by the tool runtime). A `bash`
    /// call can rewrite files the tool cannot report (O1), so it is recorded with no spans: a
    /// candidate may point at the turn but must never invent a filename the tool never named.
    fn record_mutation_from_result(
        &mut self,
        msg: &ToolResultMessage,
        canonical_index: usize,
        relative_turn: usize,
        logger: &mut EventLogger,
    ) {
        if !msg.outcome.is_success() {
            return;
        }
        let absolute_turn = self.session_turn_offset.saturating_add(relative_turn) as u32;
        let source_entry_id = Some(source_entry_id(canonical_index, msg.call_id.as_str()));
        match msg.meta.mutation.clone() {
            Some(MutationTelemetry { spans, predictive }) => {
                self.mutation_ledger.record_mutation(
                    absolute_turn,
                    msg.tool_name.clone(),
                    msg.call_id.as_str().to_string(),
                    source_entry_id,
                    self.current_revision_id.clone(),
                    spans,
                );
                if let Some(predictive) = predictive {
                    let _ = logger.log(EventPayload::PredictiveCheck {
                        path: msg.meta.relative_path.clone().unwrap_or_default(),
                        status: predictive.status,
                        issue_count: predictive.issue_count,
                        duration_ms: predictive.duration_ms,
                        summary: Some(predictive.summary),
                    });
                }
            }
            None => {
                let is_side_effect_or_mutating = msg.tool_name == "bash"
                    || self
                        .tool_runtime
                        .registry()
                        .lookup_for_dispatch(
                            &msg.tool_name,
                            self.tool_runtime.policy_engine().mode(),
                        )
                        .is_ok_and(|reg| {
                            matches!(
                                reg.metadata.effect_class,
                                EffectClass::Mutating | EffectClass::SideEffect
                            )
                        });
                if is_side_effect_or_mutating {
                    self.mutation_ledger.record_mutation(
                        absolute_turn,
                        msg.tool_name.clone(),
                        msg.call_id.as_str().to_string(),
                        source_entry_id,
                        self.current_revision_id.clone(),
                        Vec::new(),
                    );
                }
            }
        }
    }

    /// Gate 4: persist the ranked candidate attribution as machine-readable telemetry.
    fn log_failure_attribution(logger: &mut EventLogger, source: &str, report: &AttributionReport) {
        let top = report.top();
        let _ = logger.log(EventPayload::FailureAttributed {
            source: source.to_string(),
            candidate_count: report.candidates.len(),
            top_mutation_id: top.map(|c| c.mutation_id.clone()),
            top_path: top.and_then(|c| c.relative_path.clone()),
            top_confidence: top.map(|c| c.confidence),
            top_revision_before: top
                .and_then(|c| c.revision_before.as_ref())
                .map(|rev| rev.as_str().to_string()),
        });
    }

    /// Capability gap #2 (`v1.5.2`): optionally probe the top-ranked candidate attributions by
    /// reverting each inside a throwaway isolated worktree and re-running the completion gate.
    ///
    /// Safety properties this method preserves, matching the review's requirements:
    /// * the probe is disabled unless `counterfactual_probe.is_enabled()`, and it is capped per run;
    /// * each probe must be authorized by the approval seam ([`ProbeApproval`]);
    /// * the *live* workspace is never reverted — the throwaway worktree is discarded, so a wrong
    ///   heuristic can never roll back the user's code;
    /// * the whole phase is bounded by the run's remaining deadline.
    async fn run_counterfactual_probes(
        &mut self,
        report: &AttributionReport,
        absolute_turn: usize,
        current_revision: &RevisionId,
        logger: &mut EventLogger,
    ) -> Vec<CounterfactualProbeOutcome> {
        if !self.counterfactual_probe.is_enabled() {
            return Vec::new();
        }
        let Some(store) = self.snapshot_store.clone() else {
            return Vec::new();
        };
        // Do not start a probe when the run is nearly out of time.
        if let Some(deadline) = self.run_deadline {
            if deadline.remaining() < std::time::Duration::from_millis(250) {
                return Vec::new();
            }
        }
        let workspace = self.tool_runtime.env().workspace().clone();
        let worktrees_base = self.tool_runtime.env().state_dir().join("worktrees");
        let mut outcomes = Vec::new();

        let candidates: Vec<AttributionCandidate> = report
            .candidates
            .iter()
            .take(self.counterfactual_probe.top_k.max(1))
            .cloned()
            .collect();
        for candidate in candidates {
            if self.counterfactual_probes_run >= self.counterfactual_probe.max_probes_per_run {
                break;
            }
            let Some(rev_before) = candidate.revision_before.clone() else {
                continue;
            };
            if &rev_before == current_revision {
                continue;
            }
            let request = ProbeRequest {
                mutation_id: candidate.mutation_id.clone(),
                turn: candidate.turn,
                relative_path: candidate.relative_path.clone(),
            };
            if !self.probe_approval.approve(&request) {
                if logger.live_stderr() {
                    eprintln!(
                        "[counterfactual] probe of {} not approved; skipping",
                        candidate.mutation_id
                    );
                }
                continue;
            }
            self.counterfactual_probes_run += 1;

            // Each probe gets its own throwaway directory so it can never clobber a concurrent
            // tournament worktree, and so the isolation is visible in the path for telemetry.
            let probe_dir_name = format!(
                "counterfactual-{}",
                candidate
                    .mutation_id
                    .chars()
                    .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                        c
                    } else {
                        '_'
                    })
                    .collect::<String>()
            );
            let materialized = crate::subagents::IsolatedWorktree::materialize(
                probe_dir_name.clone(),
                &workspace,
                &store,
                Some(current_revision),
                worktrees_base.join(&probe_dir_name),
            );
            let Ok(mut worktree) = materialized else {
                outcomes.push(CounterfactualProbeOutcome {
                    mutation_id: candidate.mutation_id.clone(),
                    revision_before: rev_before,
                    failure_resolved_on_revert: false,
                    worktree_cleaned_up: false,
                    note: format!(
                        "probe of {} could not materialize an isolated worktree",
                        candidate.mutation_id
                    ),
                });
                continue;
            };
            let isolated = worktree.workspace().clone();

            let applied = matches!(
                store.restore_revision(&isolated, &rev_before, Some(current_revision), false),
                Ok(crate::revision::RestoreResult::Applied { .. })
            );
            let resolved = if applied {
                let ctx = CompletionGateContext {
                    run_id: self.config.run_id,
                    workspace_root: isolated.root().to_path_buf(),
                    modified_files: candidate.relative_path.clone().into_iter().collect(),
                    turns_used: absolute_turn,
                    deadline: self.run_deadline,
                };
                let outcome = match self.run_deadline {
                    Some(deadline) => tokio::time::timeout_at(
                        deadline.tokio_instant(),
                        self.completion_gate.check(&ctx),
                    )
                    .await
                    .unwrap_or_else(|_| Err("probe deadline exceeded".to_string())),
                    None => self.completion_gate.check(&ctx).await,
                };
                matches!(outcome, Ok(GateOutcome::Approved))
            } else {
                false
            };
            let cleaned = worktree.cleanup().is_ok();
            if resolved {
                self.counterfactual_probes_resolved += 1;
            }
            let note = if !applied {
                format!(
                    "probe of {}: isolated revert to {} did not apply",
                    candidate.mutation_id, rev_before
                )
            } else if resolved {
                format!(
                    "probe of {}: reverting to {} resolved the failure (isolated; parent workspace untouched)",
                    candidate.mutation_id, rev_before
                )
            } else {
                format!(
                    "probe of {}: failure persisted at {} (candidate is not the sole cause; isolated; parent untouched)",
                    candidate.mutation_id, rev_before
                )
            };
            outcomes.push(CounterfactualProbeOutcome {
                mutation_id: candidate.mutation_id.clone(),
                revision_before: rev_before,
                failure_resolved_on_revert: resolved,
                worktree_cleaned_up: cleaned,
                note,
            });
        }
        if !outcomes.is_empty() && logger.live_stderr() {
            eprintln!(
                "[counterfactual] ran {} probe(s), {} resolved the failure",
                outcomes.len(),
                outcomes
                    .iter()
                    .filter(|o| o.failure_resolved_on_revert)
                    .count()
            );
        }
        outcomes
    }

    /// Gate 3: append not-yet-loaded ancestor `AGENTS.md`/`CLAUDE.md` guidance to a tool result.
    ///
    /// The guidance is attached to the *current* tool result only; the frozen static prefix and
    /// every earlier turn stay byte-for-byte unchanged, so the prefix KV cache is preserved.
    fn apply_subtree_context(&mut self, msg: &mut ToolResultMessage, logger: &mut EventLogger) {
        if !self.subtree_tracker.is_enabled() || !is_subtree_context_tool(&msg.tool_name) {
            return;
        }
        let Some(rel_path) = msg.meta.relative_path.clone() else {
            return;
        };
        let workspace = self.tool_runtime.env().workspace().clone();
        let loads = self.subtree_tracker.load_for_path(&workspace, &rel_path);
        if loads.is_empty() {
            return;
        }
        let mut appended = String::from(msg.content.as_ref());
        for load in &loads {
            appended.push_str("\n\n");
            appended.push_str(&load.rendered);
            let _ = logger.log(EventPayload::SubtreeContextLoaded {
                directory: load.directory.clone(),
                file_name: load.file_name.clone(),
                bytes: load.bytes,
            });
        }
        msg.content = Arc::from(appended);
    }

    fn synthesize_remaining_results(
        &mut self,
        remaining_calls: &[ToolCall],
        code: ErrorCode,
        reason_text: &str,
        transcript: &mut Vec<TranscriptItem>,
        relative_turn: usize,
    ) {
        // `record_transcript_item` adds `session_turn_offset` itself, so callers pass the turn
        // relative to this process (e.g. `budget_tracker.turns_used()` or 0 for recovery).
        for call in remaining_calls {
            self.record_transcript_item(
                transcript,
                TranscriptItem::ToolResult(ToolResultMessage {
                    call_id: call.call_id.clone(),
                    tool_name: call.name.clone(),
                    outcome: ToolOutcome::Error { code },
                    content: Arc::from(reason_text.to_string()),
                    meta: ToolResultMeta::default(),
                }),
                relative_turn,
            );
        }
    }

    fn completion_gate_feedback_payload(message: &str) -> Option<&str> {
        if !message.starts_with("[CompletionGate Rejected, attempt ") {
            return None;
        }
        let (_, body) = message.split_once("]: ")?;
        Some(
            body.split_once("\nFix the reported issue before finishing.")
                .map(|(feedback, _)| feedback)
                .unwrap_or(body),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn finish_run(
        &self,
        terminal_reason: TerminalReason,
        budget_tracker: &BudgetTracker,
        projector: &ContextProjector,
        modified_files: Vec<String>,
        final_assistant_text: Option<String>,
        transcript: Vec<TranscriptItem>,
        mut logger: EventLogger,
    ) -> (RunSummary, EventLogger) {
        let status = terminal_reason.to_status();
        let duration_ms = budget_tracker.elapsed().as_millis() as u64;

        let _ = logger.log(EventPayload::RunTerminated {
            status,
            terminal_reason: terminal_reason.clone(),
            turns: budget_tracker.turns_used(),
            total_tool_calls: budget_tracker.tool_calls_used(),
            total_input_tokens: budget_tracker.total_input_tokens(),
            total_cached_input_tokens: budget_tracker.total_cached_input_tokens(),
            total_output_tokens: budget_tracker.total_output_tokens(),
            cache_hit_ratio: budget_tracker.overall_cache_hit_ratio(),
            compaction_reread_count: projector.compaction_reread_count(),
            duration_ms,
            log_degraded: logger.is_degraded(),
            dropped_events: logger.dropped_events(),
        });

        let summary = RunSummary {
            run_id: self.config.run_id,
            status,
            terminal_reason,
            turns_used: budget_tracker.turns_used(),
            request_attempts_used: budget_tracker.request_attempts_used(),
            tool_calls_used: budget_tracker.tool_calls_used(),
            total_input_tokens: budget_tracker.total_input_tokens(),
            total_cached_input_tokens: budget_tracker.total_cached_input_tokens(),
            total_output_tokens: budget_tracker.total_output_tokens(),
            children_input_tokens: budget_tracker.children_input_tokens(),
            children_cached_input_tokens: budget_tracker.children_cached_input_tokens(),
            usage_basis: budget_tracker.usage_basis().to_string(),
            children_output_tokens: budget_tracker.children_output_tokens(),
            cache_hit_ratio: budget_tracker.overall_cache_hit_ratio(),
            compaction_reread_count: projector.compaction_reread_count(),
            modified_files,
            final_assistant_text,
            log_degraded: logger.is_degraded(),
            dropped_events: logger.dropped_events(),
            transcript,
        };

        (summary, logger)
    }
}

pub fn build_default_runtime(
    env: SharedExecutionEnv,
    policy_engine: crate::tool::policy::PolicyEngine,
    budget: BudgetConfig,
    tool_filter: Option<Vec<String>>,
) -> ToolRuntime {
    let registry = crate::tool::registry::ToolRegistry::new(tool_filter);
    ToolRuntime::new(registry, policy_engine, env, budget)
}

/// Build a runtime from an already-configured registry, so Gate 3 extensions, hooks and skills
/// mounted on it stay attached to every tool dispatch.
pub fn build_runtime_with_registry(
    registry: crate::tool::registry::ToolRegistry,
    env: SharedExecutionEnv,
    policy_engine: crate::tool::policy::PolicyEngine,
    budget: BudgetConfig,
) -> ToolRuntime {
    ToolRuntime::new(registry, policy_engine, env, budget)
}

#[cfg(test)]
mod budget_estimation_tests {
    use super::{approx_tokens, estimate_prompt_input_tokens};
    use crate::prompt::PromptBuilder;

    /// BUG-09: the request-input estimate must count the system prompt exactly once. It is already
    /// part of `static_prefix_bytes`, so adding `system_prompt.len()` again would inflate the
    /// estimate and trip `max_total_tokens` early on a long system prompt.
    #[test]
    fn test_bug09_input_estimate_does_not_double_count_the_system_prompt() {
        let system_prompt = "S".repeat(40_000);
        let prefix = PromptBuilder::new(system_prompt.clone()).build_static_prefix(&[]);
        assert!(
            prefix.static_prefix_bytes >= system_prompt.len(),
            "static_prefix_bytes must already include the system prompt"
        );

        let history = 5_000usize;
        let estimate = estimate_prompt_input_tokens(prefix.static_prefix_bytes, history);
        assert_eq!(
            estimate,
            approx_tokens(prefix.static_prefix_bytes + history)
        );
        assert_ne!(
            estimate,
            approx_tokens(prefix.static_prefix_bytes + history + system_prompt.len()),
            "the system prompt must not be counted twice"
        );
    }
}
