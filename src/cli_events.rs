//! JSON Lines output for external scripts, built on the same runtime events
//! as the TUI. No RPC server or second Agent implementation.
use crate::{
    events::{EventSink, RuntimeEvent},
    output::CliOutput,
};
use serde_json::json;
use std::sync::Arc;

pub struct JsonEvents {
    output: Arc<CliOutput>,
}

impl JsonEvents {
    pub fn new(output: Arc<CliOutput>) -> Self {
        Self { output }
    }
}

impl EventSink for JsonEvents {
    fn on_event(&self, event: RuntimeEvent<'_>) {
        if self.output.broken() {
            return;
        }
        let value = match event {
            RuntimeEvent::FinalAnswerReady => return,
            RuntimeEvent::TaskStarted => json!({"type":"task_started"}),
            RuntimeEvent::CompactionStarted => json!({"type":"compaction_started"}),
            RuntimeEvent::ContextPrepared {
                estimated_input_tokens,
                available_input_tokens,
                ..
            } => json!({"type":"context","estimated_input":estimated_input_tokens,
                    "available":available_input_tokens}),
            RuntimeEvent::ModelStarted { request } => {
                json!({"type":"model_started","request":request})
            }
            RuntimeEvent::TextDelta(text) => json!({"type":"text_delta","text":text}),
            RuntimeEvent::ReasoningDelta(text) => json!({"type":"reasoning_delta","text":text}),
            RuntimeEvent::ModelFinished {
                request,
                tools,
                input_tokens,
                output_tokens,
                reasoning_tokens,
                cached_tokens,
                cache_write_tokens,
                cache_write_1h_tokens,
                ..
            } => {
                json!({"type":"model_finished","request":request,"tools":tools,"input":input_tokens,
                    "output":output_tokens,"cached":cached_tokens,"reasoning_tokens":reasoning_tokens,
                    "cache_write":cache_write_tokens,"cache_write_1h":cache_write_1h_tokens})
            }
            RuntimeEvent::ModelFailed => json!({"type":"model_failed"}),
            RuntimeEvent::FailedRequestUsage {
                input_tokens,
                output_tokens,
                cached_tokens,
                reasoning_tokens,
                cache_write_tokens,
                ..
            } => json!({"type":"failed_request_usage", "input":input_tokens,
                "output":output_tokens,"cached":cached_tokens,"reasoning_tokens":reasoning_tokens,
                "cache_write":cache_write_tokens}),
            RuntimeEvent::CompactionUsage {
                requests,
                input_tokens,
                output_tokens,
                cached_tokens,
                cache_write_tokens,
            } => json!({"type":"apec_usage","requests":requests,"input":input_tokens,
                "output":output_tokens,"cached":cached_tokens,"cache_write":cache_write_tokens}),
            RuntimeEvent::ToolStarted { name, id, .. } => {
                json!({"type":"tool_started","name":name,"id":id})
            }
            RuntimeEvent::ToolOutput { id, stream, chunk } => {
                json!({"type":"tool_output","id":id,"stream":stream,"text":chunk})
            }
            RuntimeEvent::ToolFinished {
                name, id, is_error, ..
            } => json!({"type":"tool_finished","name":name,"id":id,"is_error":is_error}),
            RuntimeEvent::TaskFinished { status } => {
                json!({"type":"task_finished","status":status})
            }
            RuntimeEvent::TurnGateContinued { plugin, feedback } => json!({
                "type":"turn_gate_continued","plugin":plugin,"feedback":feedback
            }),
            RuntimeEvent::SessionCreated { id } => json!({"type":"session_created","id":id}),
            RuntimeEvent::SessionSwitched { from, to } => {
                json!({"type":"session_switched","from":from,"to":to})
            }
            RuntimeEvent::SessionCleared { id } => json!({"type":"session_cleared","id":id}),
            RuntimeEvent::SessionForked {
                from,
                to,
                checkpoint,
            } => json!({"type":"session_forked","from":from,"to":to,"checkpoint":checkpoint}),
            RuntimeEvent::SessionRewound {
                id,
                saved_future,
                checkpoint,
            } => {
                json!({"type":"session_rewound","id":id,"future":saved_future,"checkpoint":checkpoint})
            }
            RuntimeEvent::Warning { message } => json!({"type":"warning","message":message}),
        };
        let _ = self.output.line(&value.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_broken_pipe_stops_emission_without_panicking() {
        let output = CliOutput::new();
        output.mark_broken();
        let sink = JsonEvents::new(output);
        sink.on_event(RuntimeEvent::TaskStarted);
    }
}
