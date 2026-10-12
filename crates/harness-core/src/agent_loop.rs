use crate::attribution::{
    AttributionEngine, AttributionReport, MutationLedger, MutationTelemetry,
};
use crate::budget::{BudgetConfig, BudgetTracker};
use crate::context_projector::{source_entry_id, verify_tool_call_pairing, ContextProjector};
use crate::completion_gate::{failure_fingerprint, WorkspaceCompletionGate};
use crate::session::{FileTouch, SessionJournal};
use crate::env::{
    AllowAllGate, CompletionGate, CompletionGateContext, GateOutcome, SharedCompletionGate,
    SharedExecutionEnv,
};
use crate::events::{EventLogger, EventPayload};
use crate::loop_guard::{LoopGuard, LoopGuardVerdict};
use crate::messages::{
    ToolCall, ToolOutcome, ToolResultMessage, ToolResultMeta, TranscriptItem, UserMessage,
};
use crate::prompt::PromptBuilder;
use crate::provider::{
    validate_assistant_turn_batch, ModelProvider, ProviderRequest,
};
use crate::revision::{RevisionId, SnapshotStore};
use crate::subagents::{
    build_worker_mutation_spans, parse_spawn_subagent_specs, render_subagent_batch_report,
    IsolatedWorktree, ProviderSubagentBackend, SubagentBackend, SubagentExecutionRequest,
    SubagentResult, SubagentRole, SubagentRunOutcome, WorktreeMergeResult,
    DEFAULT_MAX_SUBAGENT_SPAWNS,
};
use crate::extensions::{
    is_subtree_context_tool, HookContext, HookEvaluation, SubtreeContextTracker,
};
use crate::tool::policy::{ApprovalDecision, ApprovalRequest, PolicyCheckResult};
use crate::tool::runtime::ToolRuntime;
use crate::tools::recall_evidence::execute_recall_evidence;
use crate::tools::ToolExecutionOutput;
use crate::types::{EffectClass, ErrorCode, RunId, RunStatus, TerminalReason};
use std::collections::HashSet;
use sha2::Digest;
use std::sync::Arc;
use tokio::sync::{mpsc, watch, Semaphore};

pub const READ_ONLY_CONCURRENCY_LIMIT: usize = 8;

/// Gate 4: upper bound on `--predict-cmd` executions per run, so an optional early-check command
/// cannot multiply the cost of a long mutating session without bound.
pub const MAX_PREDICT_CMD_RUNS: usize = 8;
/// Gate 4: number of ranked candidates surfaced in a failure prompt.
pub const ATTRIBUTION_TOP_K: usize = 3;

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
        }
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
}

impl AgentRunner {
    pub fn new(
        config: AgentRunConfig,
        provider: Arc<dyn ModelProvider>,
        tool_runtime: ToolRuntime,
        cancel_rx: watch::Receiver<bool>,
    ) -> Self {
        let subagent_backend = Arc::new(ProviderSubagentBackend::new(Arc::clone(&provider)));
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
            max_total_tokens: None,
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

    /// Gate 5: enforce a hard total-token ceiling (input + output) on this runner.
    pub fn with_max_total_tokens(mut self, max_tokens: Option<u64>) -> Self {
        self.max_total_tokens = max_tokens;
        self
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

    async fn run_inner(mut self, mut logger: EventLogger) -> (RunSummary, EventLogger) {
        let mut budget_tracker = match BudgetTracker::new(self.config.budget.clone()) {
            Ok(b) => b,
            Err(e) => {
                let reason = TerminalReason::ProviderProtocolError(e.to_string());
                return self.finish_run(
                    reason,
                    &BudgetTracker::new(BudgetConfig::default()).unwrap(),
                    &ContextProjector::new(),
                    Vec::new(),
                    None,
                    Vec::new(),
                    logger,
                );
            }
        };

        let policy_engine = self.tool_runtime.policy_engine();
        let policy_mode = policy_engine.mode();
        let mut visible_tools = self
            .tool_runtime
            .registry()
            .visible_definitions_for_engine(policy_engine);
        if self.max_subagent_spawns == 0 {
            visible_tools.retain(|t| t.name != "spawn_subagent");
        }
        let static_prefix = self.prompt_builder.build_static_prefix(&visible_tools);

        if let Err(e) = logger.log(EventPayload::RunStarted {
            prompt_hash: static_prefix.prompt_hash.clone(),
            tool_schema_hash: static_prefix.tool_schema_hash.clone(),
            static_prefix_bytes: static_prefix.static_prefix_bytes,
            policy: policy_mode.as_str().to_string(),
            budget_config: self.config.budget.clone(),
        }) {
            return self.finish_run(
                TerminalReason::StrictLogFailure(e.to_string()),
                &budget_tracker,
                &ContextProjector::new(),
                Vec::new(),
                None,
                Vec::new(),
                logger,
            );
        }

        let mut transcript = self.session_journal.as_ref()
            .map(SessionJournal::active_transcript)
            .unwrap_or_default();
        self.session_turn_offset = self.session_journal.as_ref()
            .map(SessionJournal::max_active_turn)
            .unwrap_or(0);
        let is_new_session = transcript.is_empty();
        if !is_new_session {
            if let Err(err) = self.recover_trailing_tool_batch(&mut transcript) {
                return self.finish_run(
                    TerminalReason::ProviderProtocolError(format!("session recovery failed: {}", err)),
                    &budget_tracker, &ContextProjector::new(), Vec::new(), None, transcript, logger,
                );
            }
        }
        let task_turn = if is_new_session { 0 } else { 1 };
        let initial_task_item = TranscriptItem::User(UserMessage { content: self.config.task.clone() });
        // If `--restore-rev` (or an intra-turn `--undo`) reverted the workspace to an earlier
        // revision on the active branch, compute the surviving revision set before the new baseline
        // capture appends to `revision_records()`.
        let reverted_active_revisions = self.session_journal.as_ref().and_then(|journal| {
            let records = journal.revision_records();
            let last = records.last()?;
            let first_idx = records.iter().position(|r| r.revision_id == last.revision_id)?;
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
            return self.finish_run(
                TerminalReason::StrictLogFailure(err),
                &budget_tracker, &ContextProjector::new(), Vec::new(), None, transcript, logger,
            );
        }
        self.record_transcript_item(&mut transcript, initial_task_item, task_turn);
        if let Some(err) = self.session_persist_error.clone() {
            return self.finish_run(
                TerminalReason::StrictLogFailure(format!("session WAL append failed: {}", err)),
                &budget_tracker, &ContextProjector::new(), Vec::new(), None, transcript, logger,
            );
        }
        let mut projector = ContextProjector::new();
        // Freeze the system-prefix / tool-schema byte split so the Gate 2 budget breakdown can
        // report them separately instead of one opaque `static_prefix_bytes`.
        projector.set_prefix_breakdown(
            static_prefix.system_prompt.len(),
            static_prefix.tools_json.len(),
        );
        if let Some(checkpoint) = self.session_journal.as_ref()
            .and_then(SessionJournal::latest_active_checkpoint).cloned()
        {
            projector.restore_pinned_baseline_end(checkpoint.pinned_baseline_end);
            projector.restore_last_touched(
                checkpoint.last_touched.into_iter().map(|item| (item.path, item.last_touched_turn))
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
            self.subagents_spawned = checkpoint.subagents_spawned;
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
                if tr.outcome.is_success() && !self.mutation_ledger.has_call_id(tr.call_id.as_str()) {
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
        let mut loop_guard = LoopGuard::new();
        // Reconstruct dirty files from the selected session branch so a resumed run cannot
        // bypass CompletionGate merely because the write happened before this process started.
        let mut modified_files_set: HashSet<String> = self.session_journal.as_ref()
            .map(SessionJournal::modified_files_since_last_resolution)
            .unwrap_or_default()
            .into_iter()
            .collect();
        let persisted_gate_feedback = self.session_journal.as_ref()
            .map(SessionJournal::completion_feedback_since_last_resolution)
            .unwrap_or_default();
        let mut last_input_tokens: Option<u64> = None;
        let mut last_assistant_text: Option<String> = None;
        let mut gate_failure_count = persisted_gate_feedback.len();
        let mut last_gate_failure_hash: Option<String> = persisted_gate_feedback.last()
            .and_then(|message| Self::completion_gate_feedback_payload(message))
            .map(failure_fingerprint);

        if gate_failure_count >= self.completion_gate_max_retries && !modified_files_set.is_empty() {
            return self.finish_run(
                TerminalReason::ProviderProtocolError(format!(
                    "CompletionGate already rejected this unresolved session {} times (max retries={}); fork before the failed attempts or start a new session to reset the retry budget",
                    gate_failure_count, self.completion_gate_max_retries
                )),
                &budget_tracker, &projector, modified_files_set.into_iter().collect(),
                None, transcript, logger,
            );
        }

        loop {
            if *self.cancel_rx.borrow() {
                return self.finish_run(
                    TerminalReason::UserCancelled,
                    &budget_tracker,
                    &projector,
                    modified_files_set.into_iter().collect(),
                    last_assistant_text,
                    transcript,
                    logger,
                );
            }

            if budget_tracker.is_deadline_exceeded() {
                return self.finish_run(
                    TerminalReason::RunDeadlineExceeded,
                    &budget_tracker,
                    &projector,
                    modified_files_set.into_iter().collect(),
                    last_assistant_text,
                    transcript,
                    logger,
                );
            }

            if let Some(max_tok) = self.max_total_tokens {
                if budget_tracker
                    .total_input_tokens()
                    .saturating_add(budget_tracker.total_output_tokens())
                    >= max_tok
                {
                    return self.finish_run(
                        TerminalReason::MaxTurns,
                        &budget_tracker,
                        &projector,
                        modified_files_set.into_iter().collect(),
                        last_assistant_text,
                        transcript,
                        logger,
                    );
                }
            }

            if !budget_tracker.try_begin_turn() {
                return self.finish_run(
                    TerminalReason::MaxTurns,
                    &budget_tracker,
                    &projector,
                    modified_files_set.into_iter().collect(),
                    last_assistant_text,
                    transcript,
                    logger,
                );
            }

            let current_turn = budget_tracker.turns_used();
            // `current_turn` is relative to this process; the projector's turn accounting is over
            // the whole session transcript, so it must receive the absolute session turn. On a
            // `--resume` run `current_turn` starts at 1 again while the restored transcript already
            // holds `session_turn_offset` turns, and feeding the relative turn here would make every
            // pre-existing hot file look "0 turns old" and pin it forever.
            let absolute_turn = self.session_turn_offset.saturating_add(current_turn);

            // Project context (100% Append-Only unless watermark crossed)
            let projected = match projector.project(
                &mut transcript,
                budget_tracker.config(),
                static_prefix.static_prefix_bytes,
                last_input_tokens,
                absolute_turn,
            ) {
                Ok(p) => p,
                Err(term_reason) => {
                    return self.finish_run(
                        term_reason,
                        &budget_tracker,
                        &projector,
                        modified_files_set.into_iter().collect(),
                        last_assistant_text,
                        transcript,
                        logger,
                    );
                }
            };

            if let Some(comp) = projected.compaction_event {
                if let Err(e) = logger.log(EventPayload::ContextCompacted {
                    before_bytes: comp.before_bytes,
                    after_bytes: comp.after_bytes,
                    compacted_seq_indices: comp.compacted_seq_indices,
                    pinned_files: comp.pinned_files,
                }) {
                    return self.finish_run(
                        TerminalReason::StrictLogFailure(e.to_string()),
                        &budget_tracker,
                        &projector,
                        modified_files_set.into_iter().collect(),
                        last_assistant_text,
                        transcript,
                        logger,
                    );
                }
            }

            self.persist_session_checkpoint(&projector, &transcript, current_turn);
            if let Some(err) = self.session_persist_error.clone() {
                return self.finish_run(
                    TerminalReason::StrictLogFailure(format!("session checkpoint failed: {}", err)),
                    &budget_tracker, &projector, modified_files_set.into_iter().collect(),
                    last_assistant_text, transcript, logger,
                );
            }

            if !budget_tracker.try_record_request_attempt() {
                return self.finish_run(
                    TerminalReason::MaxRequestAttempts,
                    &budget_tracker,
                    &projector,
                    modified_files_set.into_iter().collect(),
                    last_assistant_text,
                    transcript,
                    logger,
                );
            }

            if let Err(e) = logger.log(EventPayload::ModelRequestStarted {
                turn: current_turn,
                attempt: budget_tracker.request_attempts_used(),
                request_body_bytes: static_prefix.static_prefix_bytes
                    + projected.message_history_bytes,
                message_history_bytes: projected.message_history_bytes,
            }) {
                return self.finish_run(
                    TerminalReason::StrictLogFailure(e.to_string()),
                    &budget_tracker,
                    &projector,
                    modified_files_set.into_iter().collect(),
                    last_assistant_text,
                    transcript,
                    logger,
                );
            }

            let req = ProviderRequest {
                system_prompt: static_prefix.system_prompt.clone(),
                tools: visible_tools.clone(),
                messages: projected.messages,
                stream: self.config.stream,
            };

            let completion = match self
                .provider
                .complete(req, budget_tracker.config(), self.cancel_rx.clone())
                .await
            {
                Ok(c) => c,
                Err(err) => {
                    return self.finish_run(
                        err.into_terminal_reason(),
                        &budget_tracker,
                        &projector,
                        modified_files_set.into_iter().collect(),
                        last_assistant_text,
                        transcript,
                        logger,
                    );
                }
            };

            for _ in 1..completion.attempts_used {
                if !budget_tracker.try_record_request_attempt() {
                    return self.finish_run(
                        TerminalReason::MaxRequestAttempts,
                        &budget_tracker,
                        &projector,
                        modified_files_set.into_iter().collect(),
                        last_assistant_text,
                        transcript,
                        logger,
                    );
                }
            }

            let assistant_turn = completion.turn;
            if let Some(usage) = assistant_turn.usage {
                budget_tracker.record_token_usage(
                    usage.input_tokens,
                    usage.cached_input_tokens,
                    usage.output_tokens,
                );
                last_input_tokens = Some(usage.input_tokens);
                if let Err(e) = logger.log(EventPayload::ModelRequestCompleted {
                    turn: current_turn,
                    input_tokens: usage.input_tokens,
                    cached_input_tokens: usage.cached_input_tokens,
                    uncached_input_tokens: usage.uncached_input_tokens(),
                    output_tokens: usage.output_tokens,
                    reasoning_tokens: usage.reasoning_tokens,
                    prefix_cache_hit_ratio: usage.prefix_cache_hit_ratio(),
                    latency_ms: completion.latency_ms,
                    ttft_ms: completion.ttft_ms,
                }) {
                    return self.finish_run(
                        TerminalReason::StrictLogFailure(e.to_string()),
                        &budget_tracker,
                        &projector,
                        modified_files_set.into_iter().collect(),
                        last_assistant_text,
                        transcript,
                        logger,
                    );
                }
            } else if let Err(e) = logger.log(EventPayload::ModelRequestCompleted {
                turn: current_turn,
                input_tokens: 0,
                cached_input_tokens: None,
                uncached_input_tokens: 0,
                output_tokens: 0,
                reasoning_tokens: None,
                prefix_cache_hit_ratio: 0.0,
                latency_ms: completion.latency_ms,
                ttft_ms: completion.ttft_ms,
            }) {
                return self.finish_run(
                    TerminalReason::StrictLogFailure(e.to_string()),
                    &budget_tracker,
                    &projector,
                    modified_files_set.into_iter().collect(),
                    last_assistant_text,
                    transcript,
                    logger,
                );
            }

            // §4.1: Pre-dispatch batch validation BEFORE adding to transcript or running any tools!
            if let Err(batch_err) = validate_assistant_turn_batch(
                &assistant_turn,
                budget_tracker.config(),
                self.config.allow_tool_calls_on_stop,
            ) {
                return self.finish_run(
                    batch_err.into_terminal_reason(),
                    &budget_tracker,
                    &projector,
                    modified_files_set.into_iter().collect(),
                    last_assistant_text,
                    transcript,
                    logger,
                );
            }

            if let Some(ref c) = assistant_turn.content {
                last_assistant_text = Some(c.clone());
            }

            let tool_calls = assistant_turn.tool_calls.clone();
            self.record_transcript_item(
                &mut transcript,
                TranscriptItem::Assistant(assistant_turn),
                current_turn,
            );
            if let Some(err) = self.session_persist_error.clone() {
                return self.finish_run(
                    TerminalReason::StrictLogFailure(format!("session WAL append failed: {}", err)),
                    &budget_tracker, &projector, modified_files_set.into_iter().collect(),
                    last_assistant_text, transcript, logger,
                );
            }

            // If model stopped with no tool calls, run CompletionGate if files were modified (§3.2 V6-07)
            if tool_calls.is_empty() {
                // Gate 3: `PreCompletion` policy hooks run before the completion gate. A denial is
                // fail-closed and is treated exactly like a gate rejection, so the model must keep
                // working instead of the run silently finishing.
                let pre_completion_denial = {
                    let hooks = self.tool_runtime.hook_engine();
                    if hooks.is_empty() {
                        None
                    } else {
                        let ctx = HookContext::new(
                            current_turn,
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
                    gate_failure_count = gate_failure_count.saturating_add(1);
                    let mut mod_list: Vec<String> = modified_files_set.iter().cloned().collect();
                    mod_list.sort();
                    if gate_failure_count >= self.completion_gate_max_retries {
                        return self.finish_run(
                            TerminalReason::ProviderProtocolError(format!(
                                "PreCompletion policy hook denied completion {} times (max retries={}): {}",
                                gate_failure_count, self.completion_gate_max_retries, reason
                            )),
                            &budget_tracker, &projector, mod_list, last_assistant_text,
                            transcript, logger,
                        );
                    }
                    self.record_transcript_item(
                        &mut transcript,
                        TranscriptItem::User(UserMessage {
                            content: format!(
                                "[PreCompletion policy hook denied completion, attempt {}/{}]: {}\nAddress the denial before finishing.",
                                gate_failure_count, self.completion_gate_max_retries, reason
                            ),
                        }),
                        current_turn.saturating_add(1),
                    );
                    if let Some(err) = self.session_persist_error.clone() {
                        return self.finish_run(
                            TerminalReason::StrictLogFailure(format!("session WAL append failed: {}", err)),
                            &budget_tracker, &projector, mod_list, last_assistant_text,
                            transcript, logger,
                        );
                    }
                    continue;
                }
                if !modified_files_set.is_empty() {
                    let mut mod_list: Vec<String> = modified_files_set.iter().cloned().collect();
                    mod_list.sort();
                    let gate_ctx = CompletionGateContext {
                        run_id: self.config.run_id,
                        workspace_root: self
                            .tool_runtime
                            .env()
                            .workspace()
                            .root()
                            .to_path_buf(),
                        modified_files: mod_list.clone(),
                        turns_used: current_turn,
                    };
                    match self.completion_gate.check(&gate_ctx).await {
                        Ok(GateOutcome::Approved) => {
                            if let Some(journal) = self.session_journal.as_mut() {
                                if let Err(err) = journal.mark_completion_approved() {
                                    self.session_persist_error = Some(err.to_string());
                                }
                            }
                            if let Some(err) = self.session_persist_error.clone() {
                                return self.finish_run(
                                    TerminalReason::StrictLogFailure(format!("session completion marker failed: {}", err)),
                                    &budget_tracker, &projector, mod_list, last_assistant_text,
                                    transcript, logger,
                                );
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
                                return self.finish_run(
                                    TerminalReason::StrictLogFailure(format!("session completion marker failed: {}", err)),
                                    &budget_tracker, &projector, mod_list, last_assistant_text,
                                    transcript, logger,
                                );
                            }
                        }
                        Ok(GateOutcome::Rejected { feedback }) => {
                            gate_failure_count = gate_failure_count.saturating_add(1);
                            let fingerprint = failure_fingerprint(&feedback);
                            let repeated = last_gate_failure_hash.as_deref() == Some(fingerprint.as_str());
                            last_gate_failure_hash = Some(fingerprint.clone());
                            let mut gate_message = format!(
                                "[CompletionGate Rejected, attempt {}/{}]: {}\nFix the reported issue before finishing.",
                                gate_failure_count, self.completion_gate_max_retries, feedback
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
                            if let Some(block) = report.render_for_model() {
                                gate_message.push_str("\n\n");
                                gate_message.push_str(&block);
                            }
                            self.record_transcript_item(
                                &mut transcript,
                                TranscriptItem::User(UserMessage { content: gate_message }),
                                current_turn.saturating_add(1),
                            );
                            self.drain_steering_messages(
                                &mut transcript,
                                current_turn.saturating_add(1),
                            );
                            if let Some(err) = self.session_persist_error.clone() {
                                return self.finish_run(
                                    TerminalReason::StrictLogFailure(format!("session WAL append failed: {}", err)),
                                    &budget_tracker, &projector, mod_list, last_assistant_text,
                                    transcript, logger,
                                );
                            }
                            if gate_failure_count >= self.completion_gate_max_retries {
                                let excerpt: String = feedback.chars().take(1600).collect();
                                return self.finish_run(
                                    TerminalReason::ProviderProtocolError(format!(
                                        "CompletionGate rejected completion {} times (max retries={}); fingerprint={}; last failure:\n{}",
                                        gate_failure_count, self.completion_gate_max_retries, fingerprint, excerpt
                                    )),
                                    &budget_tracker, &projector, mod_list, last_assistant_text,
                                    transcript, logger,
                                );
                            }
                            continue;
                        }
                        Err(err) => {
                            return self.finish_run(
                                TerminalReason::ProviderProtocolError(format!(
                                    "CompletionGate error: {}",
                                    err
                                )),
                                &budget_tracker,
                                &projector,
                                mod_list,
                                last_assistant_text,
                                transcript,
                                logger,
                            );
                        }
                    }
                }

                // O1: a verification command can rewrite the workspace (formatters, codegen).
                // Capture the resulting state so `latest_revision_id()` never drifts from disk.
                // The capture must carry the *absolute* session turn: `current_turn` restarts at 1
                // on `--resume`, which would misorder revision records against the restored turns.
                self.capture_workspace_revision(absolute_turn, "verification", &mut logger);
                if let Some(err) = self.revision_error.clone() {
                    return self.finish_run(
                        TerminalReason::StrictLogFailure(err),
                        &budget_tracker,
                        &projector,
                        modified_files_set.into_iter().collect(),
                        last_assistant_text,
                        transcript,
                        logger,
                    );
                }

                let mut mod_list: Vec<String> = modified_files_set.into_iter().collect();
                mod_list.sort();
                return self.finish_run(
                    TerminalReason::ModelStop,
                    &budget_tracker,
                    &projector,
                    mod_list,
                    last_assistant_text,
                    transcript,
                    logger,
                );
            }

            if let Err(e) = logger.log(EventPayload::ToolBatchReceived {
                turn: current_turn,
                call_count: tool_calls.len(),
                call_ids: tool_calls.iter().map(|tc| tc.call_id.clone()).collect(),
            }) {
                self.synthesize_remaining_results(
                    &tool_calls,
                    ErrorCode::NotExecutedCancelled,
                    "NOT_EXECUTED_CANCELLED: strict log failure",
                    &mut transcript,
                    current_turn,
                );
                return self.finish_run(
                    TerminalReason::StrictLogFailure(e.to_string()),
                    &budget_tracker,
                    &projector,
                    modified_files_set.into_iter().collect(),
                    last_assistant_text,
                    transcript,
                    logger,
                );
            }

            // Execute tool batch with Read-Only Parallel Sub-Batches (`Semaphore(8)`) + Mutating Barriers,
            // guaranteeing 1:1 `tool_call_id` closure in exact index order (§4.1 V6-06).
            self.batch_mutated = false;
            self.batch_ran_side_effect = false;
            let batch_abort = self
                .execute_tool_batch(
                    &tool_calls,
                    &mut transcript,
                    &mut budget_tracker,
                    &mut loop_guard,
                    &mut projector,
                    &mut modified_files_set,
                    &mut logger,
                )
                .await;

            debug_assert!(verify_tool_call_pairing(&transcript));

            // Gate 1: a batch that mutated files — directly via `edit`/`write`, or indirectly via a
            // `bash` command whose writes the tool cannot report (O1) — gets a fresh revision bound
            // to this turn.
            let batch_changed_paths = if self.batch_mutated || self.batch_ran_side_effect {
                self.capture_workspace_revision(absolute_turn, "mutation", &mut logger)
            } else {
                Vec::new()
            };
            for path in &batch_changed_paths {
                modified_files_set.insert(path.clone());
                self.batch_mutated = true;
            }
            // Gate 4: bind the post-batch revision (and any `changed_paths` discovered by the
            // snapshot store for `bash` / side-effect extension calls) to every mutation recorded
            // during this batch, dropping read-only `bash` records that changed 0 files.
            self.mutation_ledger.backfill_revision_and_paths(
                absolute_turn as u32,
                self.current_revision_id.clone(),
                &batch_changed_paths,
                self.snapshot_store.is_some() && (self.batch_mutated || self.batch_ran_side_effect),
            );
            if let Some(err) = self.revision_error.clone() {
                return self.finish_run(
                    TerminalReason::StrictLogFailure(err),
                    &budget_tracker, &projector, modified_files_set.into_iter().collect(),
                    last_assistant_text, transcript, logger,
                );
            }

            if let Some(err) = self.session_persist_error.clone() {
                return self.finish_run(
                    TerminalReason::StrictLogFailure(format!("session WAL append failed: {}", err)),
                    &budget_tracker, &projector, modified_files_set.into_iter().collect(),
                    last_assistant_text, transcript, logger,
                );
            }

            // Gate 3: checkpoint the post-batch leaf so the `subtree_dirs` (and pinned baseline /
            // last-touched ledger) loaded by *this* batch survive a later `--resume` / `--fork`,
            // even when the run ends here (`MaxTurns`, `MaxToolCalls`, `LoopDetected`, cancellation).
            self.persist_session_checkpoint(&projector, &transcript, current_turn);
            if let Some(err) = self.session_persist_error.clone() {
                return self.finish_run(
                    TerminalReason::StrictLogFailure(format!("session checkpoint failed: {}", err)),
                    &budget_tracker, &projector, modified_files_set.into_iter().collect(),
                    last_assistant_text, transcript, logger,
                );
            }

            if let Some(abort_reason) = batch_abort {
                let mut mod_list: Vec<String> = modified_files_set.into_iter().collect();
                mod_list.sort();
                return self.finish_run(
                    abort_reason,
                    &budget_tracker,
                    &projector,
                    mod_list,
                    last_assistant_text,
                    transcript,
                    logger,
                );
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
                let mut mod_list: Vec<String> = modified_files_set.iter().cloned().collect();
                mod_list.sort();
                let ctx = CompletionGateContext {
                    run_id: self.config.run_id,
                    workspace_root: self.tool_runtime.env().workspace().root().to_path_buf(),
                    modified_files: mod_list,
                    turns_used: current_turn,
                };
                let check_res = gate.check(&ctx).await;
                let predict_changed =
                    self.capture_workspace_revision(absolute_turn, "predict_cmd", &mut logger);
                if !predict_changed.is_empty() {
                    modified_files_set.extend(predict_changed);
                    post_batch_updated = true;
                }
                if let Some(err) = self.revision_error.clone() {
                    return self.finish_run(
                        TerminalReason::StrictLogFailure(err),
                        &budget_tracker, &projector, modified_files_set.into_iter().collect(),
                        last_assistant_text, transcript, logger,
                    );
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
                        &mut transcript,
                        TranscriptItem::User(UserMessage { content: message }),
                        current_turn.saturating_add(1),
                    );
                    post_batch_updated = true;
                    if let Some(err) = self.session_persist_error.clone() {
                        return self.finish_run(
                            TerminalReason::StrictLogFailure(format!("session WAL append failed: {}", err)),
                            &budget_tracker, &projector, modified_files_set.into_iter().collect(),
                            last_assistant_text, transcript, logger,
                        );
                    }
                }
            }

            // §3.2 V6-07: Mid-Run Steering — non-blocking check on `steer_rx.try_recv()` after 1:1 tool closure
            let pre_steer_len = transcript.len();
            self.drain_steering_messages(
                &mut transcript,
                current_turn.saturating_add(1),
            );
            if transcript.len() != pre_steer_len {
                post_batch_updated = true;
            }
            if let Some(err) = self.session_persist_error.clone() {
                return self.finish_run(
                    TerminalReason::StrictLogFailure(format!("session WAL append failed: {}", err)),
                    &budget_tracker, &projector, modified_files_set.into_iter().collect(),
                    last_assistant_text, transcript, logger,
                );
            }
            if post_batch_updated {
                self.persist_session_checkpoint(&projector, &transcript, current_turn);
                if let Some(err) = self.session_persist_error.clone() {
                    return self.finish_run(
                        TerminalReason::StrictLogFailure(format!("session checkpoint failed: {}", err)),
                        &budget_tracker, &projector, modified_files_set.into_iter().collect(),
                        last_assistant_text, transcript, logger,
                    );
                }
            }

            // Gate 3: `TurnCompleted` observers are fail-open telemetry and never affect control flow.
            {
                let hooks = self.tool_runtime.hook_engine();
                if !hooks.is_empty() {
                    let ctx = HookContext::new(
                        current_turn,
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
        }
    }

    /// Close a trailing tool batch interrupted by a process crash before sending another request.
    /// Existing completed results are retained; only the missing suffix is synthesized and logged.
    fn recover_trailing_tool_batch(&mut self, transcript: &mut Vec<TranscriptItem>) -> Result<(), String> {
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
        let expected_ids: Vec<_> = expected_calls.iter().map(|call| call.call_id.clone()).collect();
        if expected_ids == seen_ids {
            return Ok(());
        }
        if seen_ids.len() > expected_ids.len() || expected_ids[..seen_ids.len()] != seen_ids[..] {
            return Err("trailing tool results are not a prefix of the pending tool-call batch".into());
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

    fn drain_steering_messages(&mut self, transcript: &mut Vec<TranscriptItem>, relative_turn: usize) {
        let mut messages = Vec::new();
        if let Some(ref mut rx) = self.steer_rx {
            while let Ok(msg) = rx.try_recv() {
                if !msg.trim().is_empty() { messages.push(msg); }
            }
        }
        for msg in messages {
            let steered = format!(
                "[STEERING INSTRUCTION — high priority for the next model turn; respect system and safety constraints]\n{}",
                msg
            );
            self.record_transcript_item(transcript, TranscriptItem::User(UserMessage { content: steered }), relative_turn);
        }
    }

    /// Rebuild the `recall_evidence` store from a resumed session WAL (Gate 2).
    fn seed_evidence_from_journal(&self, projector: &mut ContextProjector) {
        let Some(journal) = self.session_journal.as_ref() else { return; };
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
        let wal_entry_id = self.session_journal.as_ref()
            .and_then(|journal| journal.leaf_id())
            .map(|id| id.to_string());
        let parent = self.current_revision_id.clone();
        match store.capture_revision(&workspace, parent.as_ref(), wal_entry_id.as_deref(), absolute_turn) {
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
                    parent_revision_id: revision.parent_revision_id.as_ref().map(|p| p.as_str().to_string()),
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
        let workspace_revision_id = self.current_revision_id.as_ref().map(|id| id.as_str().to_string());
        let Some(journal) = self.session_journal.as_mut() else { return; };
        if let Err(err) = journal.sync_projected_transcript(transcript, projector.evidence_links()) {
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
        let last_touched = std::mem::take(&mut touches).into_iter()
            .map(|(path, last_touched_turn)| FileTouch { path, last_touched_turn }).collect();
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
                    &tool_calls[idx..], ErrorCode::NotExecutedCancelled,
                    "NOT_EXECUTED_CANCELLED: run cancelled before tool execution", transcript,
                    budget_tracker.turns_used(),
                );
                return Some(TerminalReason::UserCancelled);
            }
            if budget_tracker.is_deadline_exceeded() {
                self.synthesize_remaining_results(
                    &tool_calls[idx..], ErrorCode::NotExecutedBudgetExhausted,
                    "NOT_EXECUTED_BUDGET_EXHAUSTED: run deadline exceeded before tool execution", transcript,
                    budget_tracker.turns_used(),
                );
                return Some(TerminalReason::RunDeadlineExceeded);
            }

            // Determine if we have a contiguous sub-batch of parallelizable ReadOnly calls
            let mut ro_end = idx;
            while ro_end < total && self.tool_runtime.is_parallelizable_read_only(&tool_calls[ro_end])
            {
                ro_end += 1;
            }

            if ro_end > idx {
                // Pre-validate budget and LoopGuard in deterministic index order `idx..ro_end`
                let mut tasks_to_run: Vec<(usize, ToolCall, String, Option<String>)> = Vec::new();
                let mut pre_resolved: Vec<Option<ToolResultMessage>> =
                    vec![None; ro_end - idx];

                for (local_i, call) in tool_calls[idx..ro_end].iter().enumerate() {
                    if !budget_tracker.try_consume_tool_call() {
                        self.synthesize_remaining_results(
                            &tool_calls[idx..], ErrorCode::NotExecutedBudgetExhausted,
                            "NOT_EXECUTED_BUDGET_EXHAUSTED: max_tool_calls limit reached", transcript,
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
                            &tool_calls[idx..], ErrorCode::NotExecutedCancelled,
                            "NOT_EXECUTED_CANCELLED: strict log failure", transcript,
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
                    let handle = tokio::spawn(async move {
                        let _permit = sem.acquire_owned().await.ok();
                        let (msg, decision, hooks) =
                            rt.execute_call(&call_for_task, batch_turn, cancel).await;
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
                    if let Some(mut msg) = pre_resolved[local_i].take() {
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
                        self.apply_subtree_context(&mut msg, logger);
                        // Gate 4: record the mutation against the canonical transcript index of this
                        // tool result, so the candidate can carry the Gate 2 `source_entry_id`.
                        let canonical_index = transcript.len();
                        self.record_mutation_from_result(
                            &msg,
                            canonical_index,
                            budget_tracker.turns_used(),
                            logger,
                        );
                        self.record_transcript_item(
                            transcript, TranscriptItem::ToolResult(msg),
                            budget_tracker.turns_used(),
                        );
                    }
                }

                idx = ro_end;
                continue;
            }

            // Barrier: Single serial execution for Mutating / SideEffect / Sensitive tool call
            let call = &tool_calls[idx];
            if !budget_tracker.try_consume_tool_call() {
                self.synthesize_remaining_results(
                    &tool_calls[idx..], ErrorCode::NotExecutedBudgetExhausted,
                    "NOT_EXECUTED_BUDGET_EXHAUSTED: max_tool_calls limit reached", transcript,
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
                    self.record_transcript_item(transcript, TranscriptItem::ToolResult(ToolResultMessage {
                        call_id: call.call_id.clone(),
                        tool_name: call.name.clone(),
                        outcome: ToolOutcome::Error { code },
                        content: Arc::from(message),
                        meta: ToolResultMeta { relative_path: rel_path, ..Default::default() },
                    }), budget_tracker.turns_used());
                }
                LoopGuardVerdict::Allow { signature } => {
                    if let Err(e) = logger.log(EventPayload::ToolStarted {
                        call_id: call.call_id.clone(),
                        tool_name: call.name.clone(),
                        args_sha256: signature,
                        relative_path: rel_path.clone(),
                    }) {
                        self.synthesize_remaining_results(
                            &tool_calls[idx..], ErrorCode::NotExecutedCancelled,
                            "NOT_EXECUTED_CANCELLED: strict log failure", transcript,
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
                                let (message, decision) = if let PolicyCheckResult::Deny(reason) = check
                                {
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
                                    let out = execute_recall_evidence(projector, &call.raw_arguments);
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
                                        .post_tool_hooks(call, budget_tracker.turns_used(), &message)
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
                        self.tool_runtime
                            .execute_call(call, budget_tracker.turns_used(), self.cancel_rx.clone())
                            .await
                    };
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
                            .lookup_for_dispatch(&call.name, self.tool_runtime.policy_engine().mode())
                            .is_ok_and(|reg| {
                                matches!(
                                    reg.metadata.effect_class,
                                    EffectClass::Mutating | EffectClass::SideEffect
                                )
                            });
                    if is_side_effect_or_mutating && msg.outcome.is_success() {
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

                    self.apply_subtree_context(&mut msg, logger);
                    // Gate 4: record the mutation (including a pathless `bash` side-effect record).
                    let canonical_index = transcript.len();
                    self.record_mutation_from_result(
                        &msg,
                        canonical_index,
                        budget_tracker.turns_used(),
                        logger,
                    );
                    self.record_transcript_item(
                        transcript, TranscriptItem::ToolResult(msg),
                        budget_tracker.turns_used(),
                    );
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

        let specs = match parse_spawn_subagent_specs(&call.raw_arguments) {
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
        };

        if self.subagents_spawned.saturating_add(specs.len()) > self.max_subagent_spawns {
            return (
                ToolExecutionOutput::error(
                    ErrorCode::PolicyDenied,
                    format!(
                        "subagent spawn budget exceeded: requested {}, already spawned {}, max_subagents={}",
                        specs.len(),
                        self.subagents_spawned,
                        self.max_subagent_spawns
                    ),
                    ToolResultMeta::default(),
                ),
                "deny:subagent_budget_exceeded".to_string(),
            );
        }

        let has_worker = specs.iter().any(|s| s.role == SubagentRole::Worker);
        let effective_effect = if has_worker {
            EffectClass::Mutating
        } else {
            EffectClass::ReadOnly
        };

        let check = self
            .tool_runtime
            .policy_engine()
            .evaluate(&call.name, effective_effect, false);
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
                    effect_class: effective_effect,
                    is_sensitive_path: false,
                    target_path: None,
                    diff_preview: None,
                    command_preview: Some(call.raw_arguments.clone()),
                    workspace_root: self.tool_runtime.env().workspace().root().to_path_buf(),
                };
                let pe = self.tool_runtime.policy_engine().clone();
                let mut approval_cancel_rx = self.cancel_rx.clone();
                let mut prompt_task =
                    tokio::task::spawn_blocking(move || pe.prompt_approval(&req));
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

        // If earlier tools in this same batch mutated the parent workspace, capture a revision now
        // so any worker subagent starts from the up-to-date parent state.
        if has_worker && (self.batch_mutated || self.batch_ran_side_effect) {
            let prior_changed =
                self.capture_workspace_revision(absolute_turn, "mutation", logger);
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
                            format!("failed to capture baseline revision for worker worktree: {}", e),
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
        let parent_budget = self.config.budget.clone();
        let predictive_check = self.tool_runtime.predictive_check_enabled();

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

        let mut join_handles = Vec::with_capacity(prepared.len());
        let mut worktree_slots: Vec<(String, crate::subagents::SubagentSpec, Option<IsolatedWorktree>)> =
            Vec::with_capacity(prepared.len());

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

            let handle = tokio::spawn(async move {
                let started_sub = std::time::Instant::now();
                let (child_cancel_tx, child_cancel_rx) = watch::channel(*parent_cancel.borrow());
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
                    parent_budget: budget_clone,
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
                            duration_ms: started_sub.elapsed().as_millis() as u64,
                            visible_tools: Vec::new(),
                        })
                    }
                };
                watcher.abort();
                res
            });

            join_handles.push(handle);
            worktree_slots.push((subagent_id, spec, worktree));
        }

        let mut results: Vec<SubagentResult> = Vec::with_capacity(worktree_slots.len());
        let mut batch_worker_spans = Vec::new();
        let mut first_modified_path: Option<String> = None;
        let mut in_batch_parent_rev = self.current_revision_id.clone();

        for (handle, (subagent_id, spec, mut worktree)) in
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

            let mut merge_result = None;
            let mut worktree_cleaned_up = true;

            if let Some(ref mut wt) = worktree {
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
                                        revision_after: Some(
                                            merged_revision.as_str().to_string(),
                                        ),
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
                                            revision_before
                                                .as_ref()
                                                .map(|r| r.as_str().to_string())
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

        (ToolExecutionOutput::success(report_text, meta), decision_label)
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
    fn log_failure_attribution(
        logger: &mut EventLogger,
        source: &str,
        report: &AttributionReport,
    ) {
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
            self.record_transcript_item(transcript, TranscriptItem::ToolResult(ToolResultMessage {
                call_id: call.call_id.clone(),
                tool_name: call.name.clone(),
                outcome: ToolOutcome::Error { code },
                content: Arc::from(reason_text.to_string()),
                meta: ToolResultMeta::default(),
            }), relative_turn);
        }
    }

    fn completion_gate_feedback_payload(message: &str) -> Option<&str> {
        if !message.starts_with("[CompletionGate Rejected, attempt ") {
            return None;
        }
        let (_, body) = message.split_once("]: ")?;
        Some(body.split_once("\nFix the reported issue before finishing.")
            .map(|(feedback, _)| feedback)
            .unwrap_or(body))
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
