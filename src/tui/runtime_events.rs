//! Host notifications and runtime-to-presentation mapping, independent of the renderer.
use super::{
    event_queue::EventSender,
    tool_display::{tool_arguments, tool_result_details},
};
use crate::events::RuntimeEvent;

pub enum UiEvent {
    Warning(String),
    GitInfo {
        revision: u64,
        session: String,
        info: super::gitinfo::Info,
        changes: Option<super::gitinfo::Changes>,
    },
    TaskStarted,
    CompactionStarted,
    ModelStarted,
    ModelFinished {
        tools: usize,
    },
    ModelFailed,
    FinalAnswerReady,
    Usage {
        input: Option<u64>,
        output: Option<u64>,
        cached: Option<u64>,
        reasoning: Option<u64>,
        /// Subset of input written into the provider cache, when reported.
        cache_write: Option<u64>,
    },
    TitleUsage {
        session_id: String,
        requests: usize,
        input: Option<u64>,
        output: Option<u64>,
        cached: Option<u64>,
        reasoning: Option<u64>,
    },
    ApecUsage {
        requests: usize,
        input: Option<u64>,
        output: Option<u64>,
        cached: Option<u64>,
        cache_write: Option<u64>,
    },
    UsageBatch(crate::usage::UsageMeter),
    ApecUsageBatch(crate::usage::UsageMeter),
    BacklogOverflow,
    Context {
        estimated: usize,
    },
    ActualInput(u64),
    Text(String),
    ReasoningDelta(String),
    ReasoningFinal(String),
    ToolStarted {
        name: String,
        id: String,
        args: String,
    },
    ToolFinished {
        id: String,
        result: String,
        failed: bool,
    },
    ToolOutput {
        id: String,
        stream: String,
        chunk: String,
    },
    SessionTitleUpdated(String),
    TaskFinished,
}

pub struct UiSink(pub EventSender);

impl crate::events::EventSink for UiSink {
    fn on_event(&self, event: RuntimeEvent<'_>) {
        let message = match event {
            RuntimeEvent::Warning { message } => Some(UiEvent::Warning(message.to_owned())),
            RuntimeEvent::TaskStarted => Some(UiEvent::TaskStarted),
            RuntimeEvent::CompactionStarted => Some(UiEvent::CompactionStarted),
            RuntimeEvent::ModelStarted { .. } => Some(UiEvent::ModelStarted),
            RuntimeEvent::ContextPrepared {
                estimated_input_tokens,
                ..
            } => Some(UiEvent::Context {
                estimated: estimated_input_tokens,
            }),
            RuntimeEvent::TextDelta(delta) => Some(UiEvent::Text(delta.to_owned())),
            RuntimeEvent::ReasoningDelta(delta) => Some(UiEvent::ReasoningDelta(delta.to_owned())),
            RuntimeEvent::ModelFinished {
                tools,
                input_tokens,
                output_tokens,
                reasoning_tokens,
                cached_tokens,
                reasoning,
                cache_write_tokens,
                ..
            } => {
                let _ = self.0.send(UiEvent::Usage {
                    input: input_tokens,
                    output: output_tokens,
                    cached: cached_tokens,
                    reasoning: reasoning_tokens,
                    cache_write: cache_write_tokens,
                });
                if let Some(tokens) = input_tokens {
                    let _ = self.0.send(UiEvent::ActualInput(tokens));
                }
                if let Some(text) = reasoning.filter(|s| !s.is_empty()) {
                    let _ = self.0.send(UiEvent::ReasoningFinal(text.to_owned()));
                }
                Some(UiEvent::ModelFinished { tools })
            }
            RuntimeEvent::ModelFailed => Some(UiEvent::ModelFailed),
            RuntimeEvent::FinalAnswerReady => Some(UiEvent::FinalAnswerReady),
            RuntimeEvent::FailedRequestUsage {
                input_tokens,
                output_tokens,
                cached_tokens,
                reasoning_tokens,
                cache_write_tokens,
                ..
            } => {
                let _ = self.0.send(UiEvent::Usage {
                    input: input_tokens,
                    output: output_tokens,
                    cached: cached_tokens,
                    reasoning: reasoning_tokens,
                    cache_write: cache_write_tokens,
                });
                None
            }
            RuntimeEvent::CompactionUsage {
                requests,
                input_tokens,
                output_tokens,
                cached_tokens,
                cache_write_tokens,
            } => Some(UiEvent::ApecUsage {
                requests,
                input: input_tokens,
                output: output_tokens,
                cached: cached_tokens,
                cache_write: cache_write_tokens,
            }),
            RuntimeEvent::ToolStarted {
                name,
                id,
                arguments,
            } => Some(UiEvent::ToolStarted {
                name: name.to_owned(),
                id: id.to_owned(),
                args: tool_arguments(name, arguments),
            }),
            RuntimeEvent::ToolFinished {
                id,
                result,
                is_error,
                ..
            } => Some(UiEvent::ToolFinished {
                id: id.to_owned(),
                result: tool_result_details(result),
                failed: is_error,
            }),
            RuntimeEvent::ToolOutput { id, stream, chunk } => Some(UiEvent::ToolOutput {
                id: id.to_owned(),
                stream: stream.to_owned(),
                chunk: chunk.to_owned(),
            }),
            RuntimeEvent::TaskFinished { .. } => Some(UiEvent::TaskFinished),
            // The user must always be able to see which plugin kept the turn
            // open; a stop can then be explained, not guessed.
            RuntimeEvent::TurnGateContinued { plugin, feedback } => Some(UiEvent::Warning(
                format!("{plugin}: 插件要求继续这一轮：{feedback}"),
            )),
            _ => None,
        };
        if let Some(message) = message {
            let _ = self.0.send(message);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::EventSink;

    #[test]
    fn model_accounting_precedes_final_reasoning_and_preserves_unknowns() {
        let (sender, mut receiver) = super::super::event_queue::channel();
        UiSink(sender).on_event(RuntimeEvent::ModelFinished {
            request: 1,
            tools: 0,
            reasoning: Some("visible reasoning"),
            input_tokens: Some(42),
            output_tokens: None,
            reasoning_tokens: None,
            cached_tokens: Some(0),
            cache_write_tokens: None,
            cache_write_1h_tokens: None,
        });
        let UiEvent::UsageBatch(batch) = receiver.try_recv().unwrap() else {
            panic!("usage must retain its queue position");
        };
        assert_eq!(batch.calls, 1);
        let usage = batch.last_request().unwrap();
        assert_eq!(
            (usage.input, usage.output, usage.cached),
            (Some(42), None, Some(0))
        );
        assert!(matches!(
            receiver.try_recv().unwrap(),
            UiEvent::ActualInput(42)
        ));
        assert!(
            matches!(receiver.try_recv().unwrap(), UiEvent::ReasoningFinal(text) if text == "visible reasoning")
        );
        assert!(matches!(
            receiver.try_recv().unwrap(),
            UiEvent::ModelFinished { tools: 0 }
        ));
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn live_tool_presentation_preserves_failure_and_warnings() {
        let (sender, mut receiver) = super::super::event_queue::channel();
        let sink = UiSink(sender);
        sink.on_event(RuntimeEvent::ToolStarted {
            name: "shell",
            id: "call",
            arguments: r#"{"command":"echo hello"}"#,
        });
        sink.on_event(RuntimeEvent::ToolFinished {
            name: "shell",
            id: "call",
            is_error: true,
            result: r#"{"exit_code":1,"stderr":"diagnostic","stdout_file":"capture.log"}"#,
        });
        sink.on_event(RuntimeEvent::Warning {
            message: "hook unavailable",
        });
        assert!(
            matches!(receiver.try_recv().unwrap(), UiEvent::ToolStarted { args, .. } if args == "echo hello")
        );
        assert!(
            matches!(receiver.try_recv().unwrap(), UiEvent::ToolFinished { failed: true, result, .. }
            if result == "exit: 1
stderr: diagnostic
stdout_file: capture.log")
        );
        assert!(
            matches!(receiver.try_recv().unwrap(), UiEvent::Warning(text) if text == "hook unavailable")
        );
    }
}
