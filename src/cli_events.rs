//! JSON Lines output for external scripts, built on the same runtime events
//! as the TUI. No RPC server or second Agent implementation.
use crate::events::{EventSink, RuntimeEvent};
use serde_json::json;
use std::io::Write;

pub struct JsonEvents;

impl EventSink for JsonEvents {
    fn on_event(&self, event: RuntimeEvent<'_>) {
        let value = match event {
            RuntimeEvent::TaskStarted => json!({"type":"task_started"}),
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
                ..
            } => {
                json!({"type":"model_finished","request":request,"tools":tools,"input":input_tokens,
                    "output":output_tokens,"cached":cached_tokens,"reasoning_tokens":reasoning_tokens})
            }
            RuntimeEvent::ModelFailed => json!({"type":"model_failed"}),
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
        };
        println!("{value}");
        // JSONL is a streaming interface: pipes must receive each event
        // immediately, not only when the process exits.
        let _ = std::io::stdout().flush();
    }
}
