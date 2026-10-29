//! Optional synchronous runtime notifications. Not an event bus or journal.
//! Durable history and tool results remain owned by SessionStore.

pub enum RuntimeEvent<'a> {
    TaskStarted,
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
    },
    ModelFailed,
    /// Aggregated, finalized APEC generation usage; not a foreground request.
    CompactionUsage {
        requests: usize,
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
        cached_tokens: Option<u64>,
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
    TaskFinished {
        status: &'a str,
    },
}

pub trait EventSink: Send + Sync {
    fn on_event(&self, event: RuntimeEvent<'_>);
}
