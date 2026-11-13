//! Optional synchronous runtime notifications. Not an event bus or journal.
//! Durable history and tool results remain owned by SessionStore.

pub enum RuntimeEvent<'a> {
    TaskStarted,
    /// A synchronous context checkpoint is awaiting the model, not hung in preparation.
    CompactionStarted,
    ContextPrepared {
        messages: usize,
        estimated_input_tokens: usize,
        available_input_tokens: usize,
    },
    ModelStarted {
        request: usize,
    },
    TextDelta(&'a str),
    /// Transient, provider-exposed reasoning only; not persisted until the
    /// complete assistant response passes validation.
    ReasoningDelta(&'a str),
    ModelFinished {
        request: usize,
        tools: usize,
        reasoning: Option<&'a str>,
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
        reasoning_tokens: Option<u64>,
        cached_tokens: Option<u64>,
        /// Subset of the reported input written into the provider cache.
        cache_write_tokens: Option<u64>,
        /// Subset of `cache_write_tokens` on the one-hour TTL, when reported.
        cache_write_1h_tokens: Option<u64>,
    },
    /// The final answer is durable and this turn will not accept more steering.
    /// Emitted before optional post-answer context maintenance.
    FinalAnswerReady,
    /// Usage from a failed model attempt, including attempts later recovered
    /// by context compression. Missing counters remain unknown, not zero.
    FailedRequestUsage {
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
        cached_tokens: Option<u64>,
        reasoning_tokens: Option<u64>,
        cache_write_tokens: Option<u64>,
        cache_write_1h_tokens: Option<u64>,
    },
    /// Emitted after the failed model turn's durable interruption attempt.
    /// A partially streamed response was not committed as assistant content.
    ModelFailed,
    /// Aggregated, finalized checkpoint generation usage; not a foreground request.
    /// Checkpoints are synchronous, so no background summary traffic remains
    /// after this event is emitted.
    CompactionUsage {
        requests: usize,
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
        cached_tokens: Option<u64>,
        cache_write_tokens: Option<u64>,
    },
    ToolStarted {
        name: &'a str,
        id: &'a str,
        arguments: &'a str,
    },
    /// Non-durable, UI-only progress while a Shell command is running.
    /// These bytes are never appended to the model transcript or WAL.
    ToolOutput {
        id: &'a str,
        stream: &'a str,
        chunk: &'a str,
    },
    /// Fired only after the result is durable in the session WAL.
    /// `result` is the original tool response and may contain an output-file
    /// reference for very large Shell results.
    ToolFinished {
        name: &'a str,
        id: &'a str,
        result: &'a str,
        is_error: bool,
    },
    /// Emitted only after the corresponding session state is durable.
    SessionCreated {
        id: &'a str,
    },
    SessionSwitched {
        from: &'a str,
        to: &'a str,
    },
    SessionCleared {
        id: &'a str,
    },
    SessionForked {
        from: &'a str,
        to: &'a str,
        checkpoint: usize,
    },
    SessionRewound {
        id: &'a str,
        saved_future: &'a str,
        checkpoint: usize,
    },
    /// A non-fatal runtime problem that the host should surface, for example
    /// a skipped file snapshot or a plugin hook warning. It is not part of the
    /// model transcript and never turns a successful call into a failure.
    Warning {
        message: &'a str,
    },
    /// Final foreground task notification. For `completed`, the assistant
    /// turn is durable and any required checkpoint has already completed; for
    /// `failed`/`cancelled`, the durable interruption/cancel records were
    /// attempted first. The host may accept the next user operation.
    TaskFinished {
        status: &'a str,
    },
    /// A generic `before_turn_end` plugin asked the model to keep working
    /// instead of ending the turn. Emitted only after the gate note became
    /// durable, so the host can show which plugin insisted and the user can
    /// still stop the task.
    TurnGateContinued {
        plugin: &'a str,
        feedback: &'a str,
    },
}

pub trait EventSink: Send + Sync {
    fn set_session_context(&self, _id: &str, _archive: &std::path::Path) {}
    fn on_event(&self, event: RuntimeEvent<'_>);
}
