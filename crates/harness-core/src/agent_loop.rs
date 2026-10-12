use crate::budget::{BudgetConfig, BudgetTracker};
use crate::context_projector::{verify_tool_call_pairing, ContextProjector};
use crate::completion_gate::failure_fingerprint;
use crate::session::{FileTouch, SessionJournal};
use crate::env::{
    AllowAllGate, CompletionGateContext, GateOutcome, SharedCompletionGate, SharedExecutionEnv,
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
use crate::tool::runtime::ToolRuntime;
use crate::types::{ErrorCode, RunId, RunStatus, TerminalReason};
use std::collections::HashSet;
use sha2::Digest;
use std::sync::Arc;
use tokio::sync::{mpsc, watch, Semaphore};

pub const READ_ONLY_CONCURRENCY_LIMIT: usize = 8;

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
}

impl AgentRunner {
    pub fn new(
        config: AgentRunConfig,
        provider: Arc<dyn ModelProvider>,
        tool_runtime: ToolRuntime,
        cancel_rx: watch::Receiver<bool>,
    ) -> Self {
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

    pub async fn run(mut self, mut logger: EventLogger) -> (RunSummary, EventLogger) {
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
        let visible_tools = self
            .tool_runtime
            .registry()
            .visible_definitions_for_engine(policy_engine);
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
        self.record_transcript_item(&mut transcript, initial_task_item, task_turn);
        if let Some(err) = self.session_persist_error.clone() {
            return self.finish_run(
                TerminalReason::StrictLogFailure(format!("session WAL append failed: {}", err)),
                &budget_tracker, &ContextProjector::new(), Vec::new(), None, transcript, logger,
            );
        }
        // Gate 1: capture the pre-mutation workspace state so the first turn can be undone.
        let baseline_turn = self.session_turn_offset;
        self.capture_workspace_revision(baseline_turn, "baseline", &mut logger);
        if let Some(err) = self.revision_error.clone() {
            return self.finish_run(
                TerminalReason::StrictLogFailure(err),
                &budget_tracker, &ContextProjector::new(), Vec::new(), None, transcript, logger,
            );
        }
        let mut projector = ContextProjector::new();
        if let Some(checkpoint) = self.session_journal.as_ref()
            .and_then(SessionJournal::latest_active_checkpoint).cloned()
        {
            projector.restore_pinned_baseline_end(checkpoint.pinned_baseline_end);
            projector.restore_last_touched(
                checkpoint.last_touched.into_iter().map(|item| (item.path, item.last_touched_turn))
            );
        }
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

            // Gate 1: a batch that mutated files gets a fresh revision bound to this turn.
            if self.batch_mutated {
                self.capture_workspace_revision(absolute_turn, "mutation", &mut logger);
            }
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

            // §3.2 V6-07: Mid-Run Steering — non-blocking check on `steer_rx.try_recv()` after 1:1 tool closure
            self.drain_steering_messages(
                &mut transcript,
                current_turn.saturating_add(1),
            );
            if let Some(err) = self.session_persist_error.clone() {
                return self.finish_run(
                    TerminalReason::StrictLogFailure(format!("session WAL append failed: {}", err)),
                    &budget_tracker, &projector, modified_files_set.into_iter().collect(),
                    last_assistant_text, transcript, logger,
                );
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

    /// Capture a `WorkspaceRevision`, bind it to the current WAL leaf, and emit telemetry.
    ///
    /// A capture failure is fail-closed: if the store is attached, the run must not continue while
    /// believing it has undo protection it does not actually have.
    fn capture_workspace_revision(
        &mut self,
        absolute_turn: usize,
        trigger: &str,
        logger: &mut EventLogger,
    ) {
        let Some(store) = self.snapshot_store.clone() else { return; };
        let workspace = self.tool_runtime.env().workspace().clone();
        let wal_entry_id = self.session_journal.as_ref()
            .and_then(|journal| journal.leaf_id())
            .map(|id| id.to_string());
        let parent = self.current_revision_id.clone();
        match store.capture_revision(&workspace, parent.as_ref(), wal_entry_id.as_deref(), absolute_turn) {
            Ok(revision) => {
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
            }
            Err(err) => {
                self.revision_error = Some(format!(
                    "workspace revision capture failed ({}); aborting rather than continuing without undo protection: {}",
                    trigger, err
                ));
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
        if let Err(err) = journal.sync_projected_transcript(transcript) {
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
        if let Err(err) = journal.checkpoint_with_revision(absolute_turn, projector.last_pinned_baseline_end(), last_touched, reasoning_hash, workspace_revision_id) {
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
                let mut join_handles = Vec::with_capacity(tasks_to_run.len());
                for (local_i, call, _sig, rel_path) in tasks_to_run {
                    let sem = Arc::clone(&semaphore);
                    let rt = self.tool_runtime.clone();
                    let cancel = self.cancel_rx.clone();
                    let call_for_task = call.clone();
                    let handle = tokio::spawn(async move {
                        let _permit = sem.acquire_owned().await.ok();
                        let (msg, decision) = rt.execute_call(&call_for_task, cancel).await;
                        (local_i, msg, decision)
                    });
                    join_handles.push((local_i, call, rel_path, handle));
                }

                let mut decisions: Vec<Option<String>> = vec![None; ro_end - idx];
                for (local_i, call, rel_path, handle) in join_handles {
                    match handle.await {
                        Ok((resolved_i, msg, decision)) => {
                            pre_resolved[resolved_i] = Some(msg);
                            decisions[resolved_i] = Some(decision);
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
                    if let Some(msg) = pre_resolved[local_i].take() {
                        if let Some(ref dec) = decisions[local_i] {
                            let _ = logger.log(EventPayload::PolicyDecision {
                                call_id: call.call_id.clone(),
                                tool_name: call.name.clone(),
                                decision: dec.clone(),
                                reason: None,
                            });
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

                    let (mut msg, decision) = self
                        .tool_runtime
                        .execute_call(call, self.cancel_rx.clone())
                        .await;

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
