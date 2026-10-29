use crate::{
    context::{
        BuiltinContextFactory, CompactionUsage, ContextConfig, ContextFactory, ContextPolicy,
    },
    context_state::{CompactionState, PersistedContext},
    events::{EventSink, RuntimeEvent},
    model::ModelBackend,
    prompt::PromptComposer,
    protocol::Message,
    provider::{Completion, ModelClient},
    session::{recover_pending_calls, SessionStore},
    sessions::SessionCatalog,
    shell,
    steering::TaskInput,
    tools::{ToolContext, ToolRegistry},
    usage::UsageMeter,
};
use anyhow::{bail, Result};
use std::{
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

#[cfg(test)]
use crate::prompt::EXECUTION_GUIDANCE;

#[cfg(test)]
fn initial_history(workdir: &std::path::Path) -> Vec<Message> {
    PromptComposer::default().initial_history(workdir)
}

#[cfg(test)]
fn add_batch_guidance(view: &mut [Message]) {
    PromptComposer::default().apply(view);
}

pub struct Agent {
    model: Arc<dyn ModelBackend>,
    tools: ToolRegistry,
    prompt: PromptComposer,
    events: Option<Arc<dyn EventSink>>,
    messages: Vec<Message>,
    workdir: PathBuf,
    session: SessionStore,
    catalog: SessionCatalog,
    compression: Box<dyn ContextPolicy>,
    context_factory: Arc<dyn ContextFactory>,
    context_budget: ContextConfig,
    task_input: TaskInput,
    usage: UsageMeter,
}

/// Completed conversation checkpoints. A checkpoint is the message prefix
/// ending at an assistant response with no outstanding tool calls.
#[derive(Debug, Clone)]
pub struct HistoryPoint {
    pub number: usize,
    pub until: usize,
    pub prompt: String,
    pub response: String,
}

fn active_attachment_paths(
    messages: &[Message],
    attachments: &[serde_json::Value],
    vision: Option<bool>,
) -> std::collections::HashSet<String> {
    use std::collections::HashSet;
    let mut active: HashSet<String> = attachments
        .iter()
        .filter(|part| part["type"] == "kroma_attachment")
        .filter_map(|part| part["path"].as_str().map(str::to_owned))
        .collect();
    if vision == Some(false) {
        return active;
    }
    // A fresh log or file should not make an image from the immediately
    // preceding user turn disappear. New attachments take precedence when
    // the combined image allowance would be exceeded.
    let used = attachments
        .iter()
        .filter(|part| crate::attachments::is_image(part))
        .map(|part| {
            part["bytes"]
                .as_u64()
                .unwrap_or(crate::attachments::MAX_IMAGE_BYTES as u64)
        })
        .sum::<u64>();
    let mut remaining = crate::attachments::MAX_ACTIVE_IMAGE_BYTES.saturating_sub(used);
    if let Some(previous) = messages.iter().rev().find(|msg| msg.role == "user") {
        for part in previous.provider_items.as_deref().unwrap_or(&[]) {
            if !crate::attachments::is_image(part) {
                continue;
            }
            if let Some(path) = part["path"].as_str() {
                let bytes = part["bytes"]
                    .as_u64()
                    .unwrap_or(crate::attachments::MAX_IMAGE_BYTES as u64);
                if !active.contains(path) && bytes <= remaining {
                    active.insert(path.to_owned());
                    remaining -= bytes;
                }
            }
        }
    }
    active
}

fn history_points(messages: &[Message]) -> Vec<HistoryPoint> {
    let mut points = vec![HistoryPoint {
        number: 0,
        until: 1,
        prompt: "Start of conversation".into(),
        response: String::new(),
    }];
    let mut previous = 1;
    for (index, msg) in messages.iter().enumerate().skip(1) {
        if msg.role != "assistant"
            || msg
                .tool_calls
                .as_ref()
                .is_some_and(|calls| !calls.is_empty())
        {
            continue;
        }
        let prompt = messages[previous..=index]
            .iter()
            .rev()
            .find(|m| m.role == "user")
            .and_then(|m| m.content.clone())
            .unwrap_or_else(|| "(continuation)".into());
        points.push(HistoryPoint {
            number: points.len(),
            until: index + 1,
            prompt,
            response: msg.content.clone().unwrap_or_default(),
        });
        previous = index + 1;
    }
    points
}

impl Agent {
    /// Explicit recovery when a custom context implementation is unavailable.
    /// The authoritative raw conversation is kept; only its derived memory
    /// state is reset. Call this with no Agent holding the session lock.
    pub fn recover_builtin_context(workdir: &std::path::Path, session_id: &str) -> Result<bool> {
        let catalog = SessionCatalog::new(workdir)?;
        let path = if session_id == "default" {
            catalog.default_path().to_path_buf()
        } else {
            catalog.resolve(session_id)?
        };
        let store = SessionStore::open_at(path, workdir)?;
        let loaded = store
            .load()?
            .ok_or_else(|| anyhow::anyhow!("session does not exist"))?;
        if matches!(loaded.context, PersistedContext::Builtin(_)) {
            return Ok(false);
        }
        store.save(&loaded.messages, &CompactionState::default())?;
        Ok(true)
    }

    pub fn new(model: ModelClient, workdir: PathBuf) -> Result<Self> {
        Self::with_backends(Arc::new(model), ToolRegistry::builtin(), workdir)
    }

    pub fn new_with_budget(
        model: ModelClient,
        workdir: PathBuf,
        budget: ContextConfig,
    ) -> Result<Self> {
        Self::with_runtime_budget(
            Arc::new(model),
            ToolRegistry::builtin(),
            Arc::new(BuiltinContextFactory),
            PromptComposer::default(),
            None,
            workdir,
            budget,
        )
    }

    pub fn with_backends(
        model: Arc<dyn ModelBackend>,
        tools: ToolRegistry,
        workdir: PathBuf,
    ) -> Result<Self> {
        Self::with_components(model, tools, Arc::new(BuiltinContextFactory), workdir)
    }

    /// Internal composition point, exercised by replacement-policy tests.
    pub fn with_components(
        model: Arc<dyn ModelBackend>,
        tools: ToolRegistry,
        context_factory: Arc<dyn ContextFactory>,
        workdir: PathBuf,
    ) -> Result<Self> {
        Self::with_runtime(
            model,
            tools,
            context_factory,
            PromptComposer::default(),
            None,
            workdir,
        )
    }

    pub fn with_runtime(
        model: Arc<dyn ModelBackend>,
        tools: ToolRegistry,
        context_factory: Arc<dyn ContextFactory>,
        prompt: PromptComposer,
        events: Option<Arc<dyn EventSink>>,
        workdir: PathBuf,
    ) -> Result<Self> {
        Self::with_runtime_budget(
            model,
            tools,
            context_factory,
            prompt,
            events,
            workdir,
            ContextConfig::from_env()?,
        )
    }

    pub fn with_runtime_budget(
        model: Arc<dyn ModelBackend>,
        tools: ToolRegistry,
        context_factory: Arc<dyn ContextFactory>,
        prompt: PromptComposer,
        events: Option<Arc<dyn EventSink>>,
        workdir: PathBuf,
        budget: ContextConfig,
    ) -> Result<Self> {
        let prompt = prompt.for_registered_tools(tools.is_builtin_only());
        let catalog = SessionCatalog::new(&workdir)?;
        Self::with_catalog_backends(
            model,
            tools,
            context_factory,
            prompt,
            events,
            workdir,
            catalog,
            budget,
        )
    }

    #[cfg(test)]
    fn with_catalog(model: ModelClient, workdir: PathBuf, catalog: SessionCatalog) -> Result<Self> {
        Self::with_catalog_backends(
            Arc::new(model),
            ToolRegistry::builtin(),
            Arc::new(BuiltinContextFactory),
            PromptComposer::default(),
            None,
            workdir,
            catalog,
            ContextConfig::from_env()?,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn with_catalog_backends(
        model: Arc<dyn ModelBackend>,
        tools: ToolRegistry,
        context_factory: Arc<dyn ContextFactory>,
        prompt: PromptComposer,
        events: Option<Arc<dyn EventSink>>,
        workdir: PathBuf,
        catalog: SessionCatalog,
        context_budget: ContextConfig,
    ) -> Result<Self> {
        let session = SessionStore::open_at(catalog.default_path().to_path_buf(), &workdir)?;
        let previous = session.load()?;
        let (messages, state, new_session) = match previous {
            Some(saved) => (saved.messages, saved.context, false),
            None => (
                prompt.initial_history(&workdir),
                context_factory.initial(),
                true,
            ),
        };
        let compression = context_factory.restore(state, context_budget, session.archive_path())?;
        let agent = Self {
            model,
            tools,
            prompt,
            events,
            messages,
            workdir,
            session,
            catalog,
            compression,
            context_factory,
            context_budget,
            task_input: TaskInput::default(),
            usage: UsageMeter::default(),
        };
        if new_session {
            agent
                .session
                .save(&agent.messages, &agent.compression.state())?;
        }
        if let Err(error) = shell::cleanup_session_orphans(
            &agent.workdir,
            agent.session.archive_path(),
            &agent.messages,
            false,
        ) {
            crate::runtime_log!("[shell] unable to clean orphan captures: {error:#}");
        }
        Ok(agent)
    }

    pub fn reset(&mut self) -> Result<()> {
        let fresh = self.prompt.initial_history(&self.workdir);
        let budget = self.context_budget;
        let new_compression = self.context_factory.restore(
            self.context_factory.initial(),
            budget,
            self.session.archive_path(),
        )?;
        self.session.reset(&fresh, &new_compression.state())?;
        shell::cleanup_cleared_history(&self.workdir, self.session.archive_path(), &self.messages);
        if let Err(error) =
            shell::cleanup_session_orphans(&self.workdir, self.session.archive_path(), &[], true)
        {
            crate::runtime_log!("[shell] unable to clear orphan captures: {error:#}");
        }
        self.compression.abort();
        self.compression = new_compression;
        self.context_budget = budget;
        self.messages = fresh;
        if let Some(events) = &self.events {
            events.on_event(RuntimeEvent::SessionCleared {
                id: self.session_id(),
            });
        }
        Ok(())
    }

    pub fn session_id(&self) -> &str {
        self.catalog
            .id_for_path(self.session.archive_path())
            .unwrap_or("unknown")
    }

    /// Read-only durable conversation for host renderers (CLI/TUI).
    /// UI state does not own or mutate the authoritative transcript.
    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    /// Total context window for host usage displays, including output capacity.
    pub fn context_window_tokens(&self) -> usize {
        self.context_budget.window()
    }

    /// Usable input context (total window minus reserved output capacity).
    pub fn available_context_tokens(&self) -> usize {
        self.context_budget.available_input()
    }

    pub fn usage(&self) -> &UsageMeter {
        &self.usage
    }

    /// Suppress speculative memory requests in a one-shot process. Synchronous
    /// compaction needed to fit the next model request remains enabled.
    pub fn set_background_compaction(&mut self, enabled: bool) {
        self.compression.set_background_compaction(enabled);
    }

    pub fn workdir(&self) -> &std::path::Path {
        &self.workdir
    }

    /// Swap models only between tasks. The durable conversation is unchanged.
    /// Rebuild the existing context policy against the new window in one step.
    pub fn change_model(
        &mut self,
        model: Arc<dyn ModelBackend>,
        budget: ContextConfig,
    ) -> Result<()> {
        self.session.ensure_writable()?;
        let context = self.context_factory.restore(
            self.compression.state(),
            budget,
            self.session.archive_path(),
        )?;
        self.compression.abort();
        self.compression = context;
        self.model = model;
        self.context_budget = budget;
        Ok(())
    }

    pub fn archive_path(&self) -> &std::path::Path {
        self.session.archive_path()
    }

    /// Clone this handle before running the agent to steer a running task or
    /// queue follow-up tasks from the CLI or another host.
    pub fn task_input(&self) -> TaskInput {
        self.task_input.clone()
    }

    pub fn available_tool_names(&self) -> Vec<String> {
        self.tools
            .definitions()
            .into_iter()
            .filter_map(|definition| definition["function"]["name"].as_str().map(str::to_owned))
            .collect()
    }

    pub fn with_tools(mut self, tools: ToolRegistry) -> Self {
        self.prompt = self.prompt.for_registered_tools(tools.is_builtin_only());
        self.tools = tools;
        self
    }

    pub fn list_sessions(&self) -> Result<Vec<crate::sessions::SessionEntry>> {
        self.catalog.list()
    }
    pub fn rename_session(&self, title: &str) -> Result<()> {
        self.catalog.name_session(self.session_id(), title)
    }
    pub fn resolve_session_name(&self, name: &str) -> Result<String> {
        self.catalog.resolve_name(name)
    }

    pub fn history(&self) -> Vec<HistoryPoint> {
        history_points(&self.messages)
    }

    fn checkpoint_until(&self, number: usize) -> Result<usize> {
        self.history()
            .get(number)
            .map(|point| point.until)
            .ok_or_else(|| {
                anyhow::anyhow!("no checkpoint #{number}; use /history to list checkpoints")
            })
    }

    /// Clone the completed prefix into a new session and switch to it.
    /// The source session and its WAL are not modified.
    pub fn fork(&mut self, checkpoint: Option<usize>) -> Result<String> {
        self.session.ensure_writable()?;
        let source_id = self.session_id().to_owned();
        let number = checkpoint.unwrap_or_else(|| self.history().len() - 1);
        let until = self.checkpoint_until(number)?;
        let (id, path) = self.catalog.new_path()?;
        let state = self.context_factory.branch_state(
            &self.compression.state(),
            &self.messages,
            &self.messages[..until],
            self.session.archive_path(),
            &path,
        )?;
        let compression = self
            .context_factory
            .restore(state, self.context_budget, &path)?;
        let messages = shell::clone_session_captures(
            &self.workdir,
            self.session.archive_path(),
            &path,
            &self.messages[..until],
        )?;
        let next = SessionStore::open_at(path, &self.workdir)?;
        next.reset(&messages, &compression.state())?;
        self.catalog.copy_model_choice(&source_id, &id)?;
        self.compression.abort();
        self.session = next;
        self.messages = messages;
        self.compression = compression;
        self.catalog.remember_active(self.session_id())?;
        if let Some(events) = &self.events {
            events.on_event(RuntimeEvent::SessionForked {
                from: &source_id,
                to: &id,
                checkpoint: number,
            });
            events.on_event(RuntimeEvent::SessionSwitched {
                from: &source_id,
                to: &id,
            });
        }
        Ok(id)
    }

    /// Non-destructive rewind: archive the full current history as a sibling
    /// session *before* replacing this session with an earlier prefix.
    /// The returned ID can be passed to /resume to revisit the old future.
    pub fn rewind(&mut self, checkpoint: usize) -> Result<String> {
        self.session.ensure_writable()?;
        let until = self.checkpoint_until(checkpoint)?;
        if until == self.messages.len() {
            bail!("already at checkpoint #{checkpoint}; no history to rewind");
        }
        let (backup_id, backup_path) = self.catalog.new_path()?;
        let backup_state = self.context_factory.branch_state(
            &self.compression.state(),
            &self.messages,
            &self.messages,
            self.session.archive_path(),
            &backup_path,
        )?;
        let retained_state = self.context_factory.branch_state(
            &self.compression.state(),
            &self.messages,
            &self.messages[..until],
            self.session.archive_path(),
            self.session.archive_path(),
        )?;
        let archived_compression =
            self.context_factory
                .restore(backup_state, self.context_budget, &backup_path)?;
        let next_compression = self.context_factory.restore(
            retained_state,
            self.context_budget,
            self.session.archive_path(),
        )?;
        let archived = shell::clone_session_captures(
            &self.workdir,
            self.session.archive_path(),
            &backup_path,
            &self.messages,
        )?;
        let backup = SessionStore::open_at(backup_path, &self.workdir)?;
        backup.reset(&archived, &archived_compression.state())?;
        self.catalog
            .copy_model_choice(self.session_id(), &backup_id)?;

        let shortened = self.messages[..until].to_vec();
        self.session.reset(&shortened, &next_compression.state())?;
        self.compression.abort();
        self.messages = shortened;
        self.compression = next_compression;
        if let Some(events) = &self.events {
            events.on_event(RuntimeEvent::SessionRewound {
                id: self.session_id(),
                saved_future: &backup_id,
                checkpoint,
            });
        }
        // Do not delete tool capture files here: the retained prefix still
        // references some of them. The backup owns copies of its captures.
        Ok(backup_id)
    }

    /// Undo the most recent completed conversation turn. If a task failed
    /// mid-turn, discard just that unfinished tail instead.
    pub fn undo(&mut self) -> Result<(usize, String)> {
        let points = self.history();
        let last = points.last().expect("system checkpoint exists");
        let number = if last.until < self.messages.len() {
            last.number
        } else {
            last.number.saturating_sub(1)
        };
        if self.messages.len() == 1 {
            bail!("nothing to undo");
        }
        let backup = self.rewind(number)?;
        Ok((number, backup))
    }

    pub fn new_session(&mut self) -> Result<String> {
        let (id, path) = self.catalog.new_path()?;
        self.open_session(path)?;
        Ok(id)
    }

    pub fn resume_session(&mut self, id: &str) -> Result<()> {
        let path = self.catalog.resolve(id)?;
        if path == self.session.archive_path() {
            return Ok(());
        }
        self.open_session(path)
    }

    fn open_session(&mut self, path: PathBuf) -> Result<()> {
        // Load and validate the target before mutating the running Agent.
        // This preserves the current session if the target is corrupt/locked.
        let source_id = self.session_id().to_owned();
        let next_store = SessionStore::open_at(path, &self.workdir)?;
        let previous = next_store.load()?;
        let (messages, state, fresh) = match previous {
            Some(saved) => (saved.messages, saved.context, false),
            None => (
                self.prompt.initial_history(&self.workdir),
                self.context_factory.initial(),
                true,
            ),
        };
        let next_budget = self.context_budget;
        let next_compression =
            self.context_factory
                .restore(state, next_budget, next_store.archive_path())?;
        if fresh {
            next_store.save(&messages, &next_compression.state())?;
        }
        if let Err(error) = shell::cleanup_session_orphans(
            &self.workdir,
            next_store.archive_path(),
            &messages,
            false,
        ) {
            crate::runtime_log!("[shell] unable to clean orphan captures: {error:#}");
        }
        self.compression.abort();
        self.session = next_store;
        self.messages = messages;
        self.compression = next_compression;
        self.context_budget = next_budget;
        self.catalog.remember_active(self.session_id())?;
        if let Some(events) = &self.events {
            if fresh {
                events.on_event(RuntimeEvent::SessionCreated {
                    id: self.session_id(),
                });
            }
            events.on_event(RuntimeEvent::SessionSwitched {
                from: &source_id,
                to: self.session_id(),
            });
        }
        Ok(())
    }

    /// Called after the currently executing future has been dropped.
    /// Never replay commands interrupted partway through execution.
    pub fn cancel(&mut self) -> Result<()> {
        self.compression.abort();
        self.session.ensure_writable()?;
        let before = self.messages.len();
        recover_pending_calls(&mut self.messages);
        // Cancellation can fill several missing batched tool results. Persist
        // every synthesized "unknown" in order before the cancel note.
        for end in before + 1..=self.messages.len() {
            self.session
                .append(&self.messages[..end], &self.compression.state())?;
        }
        self.messages.push(Message::new(
            "assistant",
            "[Task cancelled by user. Incomplete commands may have had side effects. The partial streamed response, if any, was not retained.]",
        ));
        self.session
            .append(&self.messages, &self.compression.state())?;
        if let Some(events) = &self.events {
            events.on_event(RuntimeEvent::TaskFinished {
                status: "cancelled",
            });
        }
        Ok(())
    }

    pub async fn run(
        &mut self,
        user_input: &str,
        on_text: &mut (impl FnMut(&str) + Send),
    ) -> Result<String> {
        self.run_with_attachments(user_input, &[], on_text).await
    }

    pub async fn run_with_attachments(
        &mut self,
        user_input: &str,
        attachments: &[serde_json::Value],
        on_text: &mut (impl FnMut(&str) + Send),
    ) -> Result<String> {
        let _active = self.task_input.start()?;
        if let Some(events) = &self.events {
            events.on_event(RuntimeEvent::TaskStarted);
        }
        // Keep one tiny task-level usage guard. Both explicit failure and
        // normal completion report here; dropping this future on Ctrl+C
        // reports cancellation, without adding a monitoring framework.
        let mut stats = TaskStats::begin();
        let result = self
            .run_inner(user_input, attachments, on_text, &mut stats)
            .await;
        stats.record_compaction(self.compression.take_usage());
        let status = if result.is_ok() {
            "completed"
        } else {
            "failed"
        };
        stats.finish(status);
        if let Some(events) = &self.events {
            events.on_event(RuntimeEvent::TaskFinished { status });
        }
        result
    }

    async fn run_inner(
        &mut self,
        user_input: &str,
        attachments: &[serde_json::Value],
        on_text: &mut (impl FnMut(&str) + Send),
        stats: &mut TaskStats,
    ) -> Result<String> {
        if user_input.trim().is_empty() && attachments.is_empty() {
            bail!("input cannot be empty");
        }
        // A partial WAL write may leave the in-memory transcript ahead of
        // disk. Never send another model request or execute another command
        // in a poisoned session. Restart to replay the authoritative WAL.
        self.session.ensure_writable()?;
        self.prompt.refresh_for_task(&self.workdir, user_input)?;
        self.compression.new_user_turn();
        let active_attachment_paths =
            active_attachment_paths(&self.messages, attachments, self.model.accepts_images());
        let mut message = Message::new("user", user_input);
        if !attachments.is_empty() {
            message.provider_items = Some(attachments.to_vec());
        }
        self.messages.push(message);
        let journal_started = Instant::now();
        self.session
            .append(&self.messages, &self.compression.state())?;
        stats.journal_time += journal_started.elapsed();
        if let Err(error) = self.catalog.remember_active(self.session_id()) {
            crate::runtime_log!("[session] unable to record recent conversation: {error:#}");
        }

        // Tools and prompt contributions cannot change during one run.
        // Calculate the extra budget once, rather than rebuilding the tool
        // schema and serializing it on every model round trip.
        let tool_definitions = self.tools.definitions();
        let tool_schema_bytes = serde_json::to_vec(&tool_definitions)?.len();
        let base_extra_tokens = self
            .prompt
            .extra_estimated_tokens(&self.messages[0])
            .saturating_add(tool_schema_bytes.div_ceil(3));

        let mut overflow_retries = 0;
        loop {
            // Steering is a durable new user instruction, delivered only
            // after the preceding model turn and all its tool results.
            self.append_pending_steering()?;
            // The provider can discover that vision is unsupported during an
            // earlier tool round. Keep the image reserve consistent with the
            // actual projection instead of using the first round's estimate.
            let extra_tokens =
                base_extra_tokens.saturating_add(crate::attachments::active_image_tokens(
                    &self.messages,
                    &active_attachment_paths,
                    self.model.accepts_images(),
                ));
            let prepare_started = Instant::now();
            let mut view = self
                .compression
                .prepare(
                    &self.messages,
                    &self.model,
                    &self.session.context_committer(&self.messages),
                    extra_tokens,
                )
                .await?;
            self.prompt.apply(&mut view);
            // Model context is a projection, not the WAL. Keep images for the
            // current task (including its tool rounds), but don't repeatedly
            // re-upload old images for every later user turn.
            crate::attachments::project_history(
                &mut view,
                &active_attachment_paths,
                self.model.accepts_images(),
            );
            // Plan with the metadata the active provider actually sends.
            // The complete view and WAL remain untouched, including opaque
            // reasoning from other models and human-visible thinking history.
            let mut budget_view = view.clone();
            self.model.project_for_budget(&mut budget_view);
            let estimated_input_tokens = self
                .context_budget
                .validate_final_request(&budget_view, &tool_definitions)?;
            if let Some(events) = &self.events {
                events.on_event(RuntimeEvent::ContextPrepared {
                    messages: view.len(),
                    estimated_input_tokens,
                    available_input_tokens: self.context_budget.available_input(),
                });
            }
            stats.context_time += prepare_started.elapsed();
            if stats.profile {
                // Payload bytes are diagnostic, not a model token count.
                // Only pay for this extra serialization when profiling.
                let bytes = serde_json::to_vec(&view)?.len();
                stats.peak_context_bytes = stats.peak_context_bytes.max(bytes);
                stats.total_context_bytes = stats.total_context_bytes.saturating_add(bytes);
                stats.total_original_bytes = stats
                    .total_original_bytes
                    .saturating_add(serde_json::to_vec(&self.messages)?.len());
                // Emit before awaiting the model: if an external controller
                // cancels a long run, the last phase is still visible.
                crate::runtime_log!(
                    "[profile] model_start request={} elapsed_ms={:.2} context_messages={}",
                    stats.calls + 1,
                    stats
                        .started
                        .map_or(Duration::ZERO, |start| start.elapsed())
                        .as_secs_f64()
                        * 1_000.0,
                    view.len()
                );
            }
            let mut printed_text = false;
            let mut streamed_reasoning = false;
            if let Some(events) = &self.events {
                events.on_event(RuntimeEvent::ModelStarted {
                    request: stats.calls + 1,
                });
            }
            let event_sink = self.events.clone();
            let completion = self
                .model
                .complete_with_reasoning(
                    &view,
                    &tool_definitions,
                    &mut |delta| {
                        printed_text = true;
                        if let Some(events) = &event_sink {
                            events.on_event(RuntimeEvent::TextDelta(delta));
                        }
                        on_text(delta);
                    },
                    &mut |delta| {
                        streamed_reasoning = true;
                        if let Some(events) = &event_sink {
                            events.on_event(RuntimeEvent::ReasoningDelta(delta));
                        }
                    },
                )
                .await;
            let completion = match completion {
                Ok(completion) => {
                    overflow_retries = 0;
                    completion
                }
                Err(error) => {
                    if !printed_text
                        && !streamed_reasoning
                        && overflow_retries == 0
                        && crate::provider::is_context_overflow(&error)
                    {
                        overflow_retries += 1;
                        match self.compression.recover_overflow(
                            &self.messages,
                            &self.model,
                            &self.session.context_committer(&self.messages),
                        ).await {
                            Ok(true) => {
                                crate::runtime_log!("[context] upstream context overflow; APEC recovered one history segment, retrying request once");
                                continue;
                            }
                            Ok(false) => crate::runtime_log!("[context] upstream context overflow; no complete history segment available for recovery"),
                            Err(recovery_error) => crate::runtime_log!("[context] overflow recovery unavailable: {recovery_error:#}"),
                        }
                    }
                    if let Some(events) = &self.events {
                        events.on_event(RuntimeEvent::ModelFailed);
                    }
                    // A partially streamed answer is not a valid assistant
                    // message or tool call. Only durable earlier tool results
                    // may be trusted when the next user turn resumes.
                    let note = if printed_text || streamed_reasoning {
                        "[Kroma interrupted: the partial streamed response was not saved. Prior completed tool results are preserved; unfinished work has not been verified.]"
                    } else {
                        "[Kroma interrupted before a complete model response. Prior completed tool results are preserved; unfinished work has not been verified.]"
                    };
                    self.messages.push(Message::new("assistant", note));
                    if let Err(journal_error) = self
                        .session
                        .append(&self.messages, &self.compression.state())
                    {
                        self.messages.pop();
                        crate::runtime_log!(
                            "[session] failed to save interruption note: {journal_error:#}"
                        );
                    }
                    crate::runtime_log!(
                        "[usage] incomplete model request: token usage unknown; {} completed request(s) this turn",
                        stats.calls
                    );
                    return Err(error);
                }
            };
            // Mark only successfully answered requests as having consumed
            // their tool results. The freshly returned calls/results remain
            // protected until a subsequent successful model response.
            self.compression.mark_consumed(self.messages.len());
            self.compression.observe(
                &budget_view,
                completion.usage.as_ref().and_then(|usage| usage.input),
                completion.usage.as_ref().and_then(|usage| usage.cached),
            );
            self.usage.record(completion.usage.as_ref());
            if printed_text {
                on_text("\n");
            }
            stats.record(&completion);
            let response = completion.message;
            let calls = response.tool_calls.clone().unwrap_or_default();
            if stats.profile {
                crate::runtime_log!(
                    "[profile] model_done request={} elapsed_ms={:.2} first_delta_ms={} returned_tools={}",
                    stats.calls,
                    completion.elapsed.as_secs_f64() * 1_000.0,
                    completion.first_token.map_or_else(|| "n/a".to_string(), |t| format!("{:.2}", t.as_secs_f64() * 1_000.0)),
                    calls.len()
                );
            }
            let text = response.content.clone();
            self.messages.push(response);
            let journal_started = Instant::now();
            self.session
                .append(&self.messages, &self.compression.state())?;
            stats.journal_time += journal_started.elapsed();
            if let Some(events) = &self.events {
                events.on_event(RuntimeEvent::ModelFinished {
                    request: stats.calls,
                    tools: calls.len(),
                    input_tokens: completion.usage.as_ref().and_then(|usage| usage.input),
                    output_tokens: completion.usage.as_ref().and_then(|usage| usage.output),
                    reasoning_tokens: completion.usage.as_ref().and_then(|usage| usage.reasoning),
                    cached_tokens: completion.usage.as_ref().and_then(|usage| usage.cached),
                    reasoning: self
                        .messages
                        .last()
                        .and_then(|m| m.reasoning_content.as_deref()),
                });
            }

            if calls.is_empty() {
                let finish_started = Instant::now();
                if let Err(error) = self
                    .compression
                    .finish_task(
                        &self.messages,
                        &self.session.context_committer(&self.messages),
                    )
                    .await
                {
                    // Nonessential memory maintenance must not turn an
                    // otherwise successful user task into a failed task.
                    crate::runtime_log!(
                        "[apec] task completed; summary was not committed: {error:#}"
                    );
                }
                stats.apec_finalize_time += finish_started.elapsed();
                // Steering accepted during streaming or finalization should
                // get another model turn, not be silently lost at completion.
                if !self.task_input.finish_if_no_steering() {
                    continue;
                }
                // The completed turn is already durable in the append journal.
                // Avoid rewriting the entire transcript at every task boundary.
                return Ok(text.unwrap_or_default());
            }

            // Multiple tool calls share one model response (fewer model
            // round-trips), but run strictly in the returned order. Each
            // result retains its own tool_call_id and is saved before the
            // next call; no concurrency or command-safety classifier.
            for call in calls {
                if let Some(events) = &self.events {
                    events.on_event(RuntimeEvent::ToolStarted {
                        name: &call.function.name,
                        id: &call.id,
                        arguments: &call.function.arguments,
                    });
                }
                if stats.profile {
                    crate::runtime_log!(
                        "[profile] tool_start number={} after_request={} tool={}",
                        stats.tools + 1,
                        stats.calls,
                        call.function.name
                    );
                }
                let tool_started = Instant::now();
                let observer = self.events.clone();
                let output_event = |stream: &str, chunk: &str| {
                    if let Some(events) = &observer {
                        events.on_event(RuntimeEvent::ToolOutput {
                            id: &call.id,
                            stream,
                            chunk,
                        });
                    }
                };
                let result = self
                    .tools
                    .execute(
                        &call,
                        ToolContext {
                            workdir: &self.workdir,
                            archive: self.session.archive_path(),
                            on_output: self
                                .events
                                .as_ref()
                                .map(|_| &output_event as &crate::shell::OutputCallback<'_>),
                        },
                    )
                    .await;
                stats.tool_time += tool_started.elapsed();
                stats.tools += 1;
                if stats.profile {
                    crate::runtime_log!(
                        "[profile] tool_done number={} elapsed_ms={:.2}",
                        stats.tools,
                        tool_started.elapsed().as_secs_f64() * 1_000.0
                    );
                }
                self.messages.push(Message::tool_with_status(
                    call.id.clone(),
                    result.content,
                    result.is_error,
                ));
                let journal_started = Instant::now();
                self.session
                    .append(&self.messages, &self.compression.state())?;
                stats.journal_time += journal_started.elapsed();
                if let Some(events) = &self.events {
                    events.on_event(RuntimeEvent::ToolFinished {
                        name: &call.function.name,
                        id: &call.id,
                        is_error: result.is_error,
                        result: self
                            .messages
                            .last()
                            .and_then(|msg| msg.content.as_deref())
                            .unwrap_or(""),
                    });
                }
            }
        }
    }

    fn append_pending_steering(&mut self) -> Result<()> {
        let mut pending = self.task_input.take_steering();
        while let Some(text) = pending.pop_front() {
            self.messages.push(Message::new(
                "user",
                format!("[Steering instruction for the current task]\n{text}"),
            ));
            if let Err(error) = self
                .session
                .append(&self.messages, &self.compression.state())
            {
                self.messages.pop();
                pending.push_front(text);
                self.task_input.restore_steering(pending);
                return Err(error);
            }
        }
        Ok(())
    }
}

#[derive(Default)]
struct TaskStats {
    calls: usize,
    elapsed: Duration,
    input: Option<u64>,
    output: Option<u64>,
    cached: Option<u64>,
    reasoning: Option<u64>,
    compaction: CompactionUsage,
    profile: bool,
    started: Option<Instant>,
    context_time: Duration,
    tool_time: Duration,
    journal_time: Duration,
    apec_finalize_time: Duration,
    tools: usize,
    peak_context_bytes: usize,
    total_context_bytes: usize,
    total_original_bytes: usize,
    reported: bool,
}

impl TaskStats {
    fn begin() -> Self {
        let mut stats = Self::default();
        stats.profile = std::env::var("KROMA_PROFILE").is_ok_and(|v| v == "1");
        stats.started = Some(Instant::now());
        stats
    }

    fn record_compaction(&mut self, usage: CompactionUsage) {
        self.compaction = usage;
    }

    fn record(&mut self, completion: &Completion) {
        let usage = completion.usage.as_ref();
        let first = completion.first_token.map_or_else(
            || "n/a".to_string(),
            |value| format!("{:.2}s", value.as_secs_f64()),
        );
        let input = usage.and_then(|v| v.input);
        let output = usage.and_then(|v| v.output);
        let cached = usage.and_then(|v| v.cached);
        let reasoning = usage.and_then(|v| v.reasoning);
        let hit_rate = match (input, cached) {
            (Some(input), Some(cached)) if input > 0 && cached <= input => {
                format!(" ({:.1}%)", 100.0 * cached as f64 / input as f64)
            }
            _ => String::new(),
        };
        crate::runtime_log!(
            "[usage] request: {:.2}s | first_delta: {first} | input: {} | output: {} | cached: {}{hit_rate} | reasoning: {}",
            completion.elapsed.as_secs_f64(),
            display_count(input),
            display_count(output),
            display_count(cached),
            display_count(reasoning),
        );
        self.input = add_reported(self.input, input, self.calls == 0);
        self.output = add_reported(self.output, output, self.calls == 0);
        self.cached = add_reported(self.cached, cached, self.calls == 0);
        self.reasoning = add_reported(self.reasoning, reasoning, self.calls == 0);
        self.calls += 1;
        self.elapsed += completion.elapsed;
    }

    fn finish(&mut self, status: &str) {
        if self.reported {
            return;
        }
        self.reported = true;
        crate::runtime_log!(
            "[usage] task_status={status} | completed_model_requests={} | completed_tool_calls={} | failed_attempt_usage=unknown_if_any",
            self.calls,
            self.tools,
        );
        self.report();
    }

    fn report(&self) {
        crate::runtime_log!(
            "[usage] task: {} request(s), {:.2}s model time | input: {} | output: {} | cached: {} | reasoning: {}",
            self.calls,
            self.elapsed.as_secs_f64(),
            display_count(self.input),
            display_count(self.output),
            display_count(self.cached),
            display_count(self.reasoning)
        );
        if self.compaction.requests > 0 {
            crate::runtime_log!(
                "[usage] APEC: {} request(s), {:.2}s model time | input: {} | output: {} | cached: {}",
                self.compaction.requests,
                self.compaction.elapsed.as_secs_f64(),
                display_count(self.compaction.input),
                display_count(self.compaction.output),
                display_count(self.compaction.cached),
            );
            crate::runtime_log!(
                "[usage] combined: {} model request(s) | input: {} | output: {} | cached: {}",
                self.calls + self.compaction.requests,
                display_count(add_reported(self.input, self.compaction.input, false)),
                display_count(add_reported(self.output, self.compaction.output, false)),
                display_count(add_reported(self.cached, self.compaction.cached, false))
            );
        }
        let (total_input, total_cached) = if self.compaction.requests == 0 {
            (self.input, self.cached)
        } else {
            (
                add_reported(self.input, self.compaction.input, false),
                add_reported(self.cached, self.compaction.cached, false),
            )
        };
        // Unknown provider usage must stay unknown, never silently 0.
        let uncached =
            total_input.and_then(|input| total_cached.and_then(|cached| input.checked_sub(cached)));
        crate::runtime_log!(
            "[usage] effective: foreground={} APEC={} | uncached_input={} | cached_input={} | failed_attempt_usage=unknown_if_any",
            self.calls,
            self.compaction.requests,
            display_count(uncached),
            display_count(total_cached)
        );
        if self.profile {
            let ms = |time: Duration| format!("{:.2}", time.as_secs_f64() * 1_000.0);
            let wall = self.started.map_or(Duration::ZERO, |start| start.elapsed());
            crate::runtime_log!(
                "[profile] wall_ms={} | context_ms={} | model_ms={} | shell_ms={} | journal_ms={} | apec_finish_ms={} | model_requests={} | tool_calls={} | peak_context_json_bytes={} | sent_messages_json_bytes={} | raw_history_json_bytes={}",
                ms(wall),
                ms(self.context_time),
                ms(self.elapsed),
                ms(self.tool_time),
                ms(self.journal_time),
                ms(self.apec_finalize_time),
                self.calls,
                self.tools,
                self.peak_context_bytes,
                self.total_context_bytes,
                self.total_original_bytes
            );
        }
    }
}

impl Drop for TaskStats {
    fn drop(&mut self) {
        if !self.reported {
            // A dropped agent.run future is the Ctrl+C cancellation path.
            // Count only successful model calls; an interrupted attempt has
            // unknown provider usage and is not silently treated as free.
            self.finish("cancelled");
        }
    }
}

fn display_count(value: Option<u64>) -> String {
    value.map_or_else(|| "n/a".into(), |number| number.to_string())
}

fn add_reported(total: Option<u64>, next: Option<u64>, first: bool) -> Option<u64> {
    if first {
        next
    } else {
        total.and_then(|sum| next.and_then(|value| sum.checked_add(value)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::Usage;
    use serde_json::json;

    #[test]
    fn previous_image_survives_a_new_log_but_respects_the_image_budget() {
        let mut previous = Message::new("user", "Inspect A");
        previous.provider_items = Some(vec![json!({
            "type":"kroma_attachment","kind":"image","path":"A.png","bytes":4_000_000
        })]);
        let history = vec![Message::new("system", "system"), previous];
        let log = json!({"type":"kroma_attachment","kind":"log","path":"new.log"});
        let active = active_attachment_paths(&history, std::slice::from_ref(&log), Some(true));
        assert!(active.contains("A.png"));
        assert!(active.contains("new.log"));
        let mut projected = history.clone();
        crate::attachments::project_history(&mut projected, &active, Some(true));
        assert_ne!(
            projected[1].provider_items.as_ref().unwrap()[0]["mode"],
            "reference"
        );

        let big_image = json!({"type":"kroma_attachment","kind":"image",
            "path":"B.png","bytes":13_000_000});
        let active = active_attachment_paths(&history, &[log, big_image], Some(true));
        assert!(
            !active.contains("A.png"),
            "new images take priority within the 16 MiB limit"
        );
        assert!(active.contains("B.png"));
        assert!(!active_attachment_paths(&history, &[], Some(false)).contains("A.png"));
        let history = vec![
            history[0].clone(),
            history[1].clone(),
            Message::new("user", "follow-up"),
        ];
        assert!(
            !active_attachment_paths(&history, &[], Some(true)).contains("A.png"),
            "only the immediately preceding user turn can provide an image"
        );
    }

    #[tokio::test]
    async fn failed_user_wal_append_prevents_followup_model_and_tool_execution() {
        let dir = std::env::temp_dir().join(format!(
            "kroma-wal-agent-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session.json");
        let catalog = SessionCatalog::from_default(path.clone()).unwrap();
        // No listener: if the session incorrectly tries a provider request,
        // this test would attempt to connect and report an unrelated error.
        let model = ModelClient::new("http://127.0.0.1:9/v1", None, "mock".into()).unwrap();
        let mut agent = Agent::with_catalog(model, dir.clone(), catalog).unwrap();
        agent.session.inject_partial_append_failure_at(1);
        let first = agent.run("first", &mut |_| {}).await.unwrap_err();
        assert!(first.to_string().contains("restart"));
        let count = agent.messages.len();
        let second = agent.run("second", &mut |_| {}).await.unwrap_err();
        assert!(second.to_string().contains("restart"));
        assert_eq!(
            agent.messages.len(),
            count,
            "no new user turn after an ambiguous WAL write"
        );
        drop(agent);
        let store = SessionStore::open_at(path, &dir).unwrap();
        let recovered = store.load().unwrap().unwrap().messages;
        assert_eq!(
            recovered.len(),
            1,
            "partial user record must not be replayed"
        );
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn interrupted_stream_is_recorded_and_recovered_without_replaying_tools() {
        use std::io::{BufRead, BufReader, Read, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(&stream);
            let mut body_size = 0usize;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    break;
                }
                if let Some(raw) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    body_size = raw.trim().parse().unwrap();
                }
            }
            let mut body = vec![0; body_size];
            reader.read_exact(&mut body).unwrap();
            drop(reader);
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {{\"choices\":[{{\"delta\":{{\"content\":\"partial answer\"}},\"finish_reason\":null}}]}}\n\n").unwrap();
            stream.flush().unwrap();
        });
        let dir = std::env::temp_dir().join(format!(
            "kroma-p8-interruption-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let session_path = dir.join("session.json");
        let model = ModelClient::new(&format!("http://{addr}/v1"), None, "mock".into()).unwrap();
        let catalog = SessionCatalog::from_default(session_path.clone()).unwrap();
        let mut agent = Agent::with_catalog(model, dir.clone(), catalog).unwrap();
        let mut visible = String::new();
        let result = agent
            .run("Inspect the project", &mut |piece| visible.push_str(piece))
            .await;
        assert!(result.is_err());
        assert_eq!(visible, "partial answer");
        assert!(agent
            .messages
            .last()
            .unwrap()
            .content
            .as_deref()
            .unwrap()
            .contains("Kroma interrupted"));
        assert!(agent
            .messages
            .iter()
            .all(|message| message.tool_calls.is_none()));
        drop(agent);
        server.join().unwrap();
        let saved = SessionStore::open_at(session_path, &dir)
            .unwrap()
            .load()
            .unwrap()
            .unwrap();
        assert_eq!(saved.messages.len(), 3);
        assert_eq!(saved.messages[1].role, "user");
        assert!(saved.messages[2]
            .content
            .as_deref()
            .unwrap()
            .contains("partial streamed response was not saved"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn switching_sessions_keeps_histories_and_compaction_isolated() {
        let dir = std::env::temp_dir().join(format!(
            "kroma-switch-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let catalog = SessionCatalog::from_default(dir.join("default.json")).unwrap();
        let model = ModelClient::new("http://localhost/v1", None, "stub".into()).unwrap();
        let mut agent = Agent::with_catalog(model, dir.clone(), catalog).unwrap();
        assert_eq!(agent.session_id(), "default");
        agent.messages.push(Message::new("user", "default task"));
        agent
            .session
            .append(&agent.messages, &agent.compression.state())
            .unwrap();

        let named_id = agent.new_session().unwrap();
        assert_eq!(agent.session_id(), named_id);
        assert_eq!(agent.messages.len(), 1);
        agent.messages.push(Message::new("user", "named task"));
        agent
            .session
            .append(&agent.messages, &agent.compression.state())
            .unwrap();
        assert_eq!(agent.list_sessions().unwrap().len(), 2);

        agent.resume_session("default").unwrap();
        assert_eq!(agent.messages[1].content.as_deref(), Some("default task"));
        agent.resume_session(&named_id).unwrap();
        assert_eq!(agent.messages[1].content.as_deref(), Some("named task"));
        assert!(agent.resume_session("../invalid").is_err());
        assert_eq!(agent.session_id(), named_id);
        assert_eq!(agent.messages[1].content.as_deref(), Some("named task"));
        drop(agent);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn batching_guidance_is_stable_for_new_and_resumed_sessions() {
        let mut original = initial_history(std::path::Path::new("/tmp/kroma-project"));
        let untouched = original[0].content.clone();
        original.push(Message::new("assistant", "previous conversation"));
        for _ in 0..2 {
            let mut projected = original.clone();
            add_batch_guidance(&mut projected);
            assert_eq!(projected.len(), original.len());
            assert!(projected[0]
                .content
                .as_ref()
                .unwrap()
                .ends_with(EXECUTION_GUIDANCE));
            assert_eq!(
                projected[0]
                    .content
                    .as_ref()
                    .unwrap()
                    .matches(EXECUTION_GUIDANCE)
                    .count(),
                1
            );
            assert_eq!(projected[1].content, original[1].content);
            assert_eq!(original[0].content, untouched);
        }
    }

    #[test]
    fn execution_guidance_is_targeted_without_skipping_checks() {
        assert!(EXECUTION_GUIDANCE.contains(
            "once the needed evidence is found, edit rather than re-inspecting unchanged files"
        ));
        assert!(EXECUTION_GUIDANCE.contains("requested tests"));
        assert!(EXECUTION_GUIDANCE.contains("relevant regression checks"));
        assert!(EXECUTION_GUIDANCE.contains("unrelated doctests"));
        assert!(EXECUTION_GUIDANCE.contains("If a check fails"));
        assert!(EXECUTION_GUIDANCE.contains("Do not omit required verification"));
    }

    #[test]
    fn totals_are_unknown_if_any_request_has_missing_usage() {
        let mut stats = TaskStats::default();
        for usage in [
            Some(Usage {
                input: Some(100),
                output: Some(5),
                cached: Some(80),
                ..Default::default()
            }),
            None,
        ] {
            stats.record(&Completion {
                message: Message::new("assistant", "ok"),
                usage,
                elapsed: Duration::from_millis(100),
                first_token: None,
            });
        }
        assert_eq!(stats.calls, 2);
        assert_eq!(stats.input, None);
        assert_eq!(stats.cached, None);
    }
}
