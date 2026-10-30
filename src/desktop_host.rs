//! Lightweight event adapter for graphical hosts. The Rust transcript/WAL is
//! authoritative; this is a bounded, transient stream for painting frames.
use crate::{
    agent::Agent,
    events::{EventSink, RuntimeEvent},
    model::Usage,
    sessions::{SessionCatalog, SessionUsage},
};
use anyhow::Result;
use serde_json::{json, Value};
use std::{collections::VecDeque, sync::Mutex};

const MAX_PENDING: usize = 512;
const MAX_DELTA: usize = 16 * 1024;

#[derive(Default)]
struct Pending {
    events: VecDeque<Value>,
    usage: Vec<Usage>,
    compaction: Vec<(usize, Usage)>,
}

/// One sink per runtime. Cheap synchronous writes from the model/tool threads;
/// a desktop host drains at its frame rate instead of painting every token.
#[derive(Default)]
pub struct DesktopEvents {
    pending: Mutex<Pending>,
}
impl DesktopEvents {
    pub fn drain(&self) -> Vec<Value> {
        let mut lock = self.pending.lock().expect("desktop event lock");
        lock.events.drain(..).collect()
    }

    pub fn persist_usage(&self, catalog: &SessionCatalog, session: &str) -> Result<SessionUsage> {
        let mut pending = self.pending.lock().expect("desktop event lock");
        let usage = std::mem::take(&mut pending.usage);
        let compaction = std::mem::take(&mut pending.compaction);
        let changed = !usage.is_empty() || !compaction.is_empty();
        drop(pending);
        let mut saved = catalog.usage(session)?;
        for measure in &usage {
            saved.coding.record(Some(measure));
        }
        for (requests, measure) in compaction {
            saved.apec.record_batch(requests, Some(&measure));
        }
        if changed {
            catalog.set_usage(session, saved.clone())?;
        }
        Ok(saved)
    }

    fn push(&self, event: Value) {
        let mut state = self.pending.lock().expect("desktop event lock");
        let delta = matches!(
            event["kind"].as_str(),
            Some("text_delta" | "reasoning_delta" | "tool_output")
        );
        if delta {
            if let Some(tail) = state.events.back_mut() {
                if tail["kind"] == event["kind"]
                    && tail["id"] == event["id"]
                    && tail["stream"] == event["stream"]
                {
                    let old = tail["text"].as_str().unwrap_or("");
                    let next = event["text"].as_str().unwrap_or("");
                    if old.len() + next.len() <= MAX_DELTA {
                        tail["text"] = json!(format!("{old}{next}"));
                        return;
                    }
                }
            }
            if state.events.len() >= MAX_PENDING {
                // Drop only transient deltas; the host will resync from WAL
                // on task completion. Never drop tool status or error events.
                if let Some(index) = state.events.iter().position(|item| {
                    matches!(
                        item["kind"].as_str(),
                        Some("text_delta" | "reasoning_delta" | "tool_output")
                    )
                }) {
                    state.events.remove(index);
                } else {
                    return;
                }
            }
        }
        state.events.push_back(event);
    }
}

impl EventSink for DesktopEvents {
    fn on_event(&self, event: RuntimeEvent<'_>) {
        match event {
            RuntimeEvent::TaskStarted => self.push(json!({"kind":"task_started"})),
            RuntimeEvent::ModelStarted { request } => {
                self.push(json!({"kind":"model_started","request":request}))
            }
            RuntimeEvent::TextDelta(text) => self.push(json!({"kind":"text_delta","text":text})),
            RuntimeEvent::ReasoningDelta(text) => {
                self.push(json!({"kind":"reasoning_delta","text":text}))
            }
            RuntimeEvent::ContextPrepared {
                estimated_input_tokens,
                available_input_tokens,
                ..
            } => self.push(json!({"kind":"context","estimated":estimated_input_tokens,
                    "available":available_input_tokens})),
            RuntimeEvent::ToolStarted {
                name,
                id,
                arguments,
            } => {
                self.push(json!({"kind":"tool_started","name":name,"id":id,"arguments":arguments}))
            }
            RuntimeEvent::ToolOutput { id, stream, chunk } => {
                self.push(json!({"kind":"tool_output","id":id,"stream":stream,"text":chunk}))
            }
            RuntimeEvent::ToolFinished {
                name,
                id,
                result,
                is_error,
            } => self.push(json!({"kind":"tool_finished","name":name,"id":id,
                    "result":result,"failed":is_error})),
            RuntimeEvent::ModelFinished {
                request,
                tools,
                reasoning,
                input_tokens,
                output_tokens,
                cached_tokens,
                reasoning_tokens,
            } => {
                self.pending
                    .lock()
                    .expect("desktop event lock")
                    .usage
                    .push(Usage {
                        input: input_tokens,
                        output: output_tokens,
                        cached: cached_tokens,
                        reasoning: reasoning_tokens,
                        total: None,
                    });
                self.push(
                    json!({"kind":"model_finished","request":request,"tools":tools,
                    "reasoning":reasoning,"input":input_tokens,"output":output_tokens,
                    "cached":cached_tokens,"reasoning_tokens":reasoning_tokens}),
                );
            }
            RuntimeEvent::CompactionUsage {
                requests,
                input_tokens,
                output_tokens,
                cached_tokens,
            } => {
                self.pending
                    .lock()
                    .expect("desktop event lock")
                    .compaction
                    .push((
                        requests,
                        Usage {
                            input: input_tokens,
                            output: output_tokens,
                            cached: cached_tokens,
                            reasoning: None,
                            total: None,
                        },
                    ));
                self.push(
                    json!({"kind":"compaction_usage","requests":requests,"input":input_tokens,
                    "output":output_tokens,"cached":cached_tokens}),
                );
            }
            RuntimeEvent::ModelFailed => self.push(json!({"kind":"model_failed"})),
            RuntimeEvent::TaskFinished { status } => {
                self.push(json!({"kind":"task_finished","status":status}))
            }
            RuntimeEvent::SessionCreated { id } => {
                self.push(json!({"kind":"session_created","id":id}))
            }
            RuntimeEvent::SessionSwitched { from, to } => {
                self.push(json!({"kind":"session_switched","from":from,"to":to}))
            }
            RuntimeEvent::SessionForked {
                from,
                to,
                checkpoint,
            } => self.push(json!({"kind":"session_forked","from":from,"to":to,
                    "checkpoint":checkpoint})),
            RuntimeEvent::SessionRewound {
                id,
                saved_future,
                checkpoint,
            } => self.push(json!({"kind":"session_rewound","id":id,
                    "saved_future":saved_future,"checkpoint":checkpoint})),
            RuntimeEvent::SessionCleared { id } => {
                self.push(json!({"kind":"session_cleared","id":id}))
            }
        }
    }
}

/// The transcript is returned only at lifecycle boundaries. The frontend must
/// never treat a partial streamed answer as durable history.
pub fn snapshot(agent: &Agent) -> Result<Value> {
    let catalog = SessionCatalog::new(agent.workdir())?;
    let usage = if agent.session_id() == "unknown" {
        SessionUsage::default()
    } else {
        catalog.usage(agent.session_id())?
    };
    Ok(json!({
        "session_id":agent.session_id(),
        "messages":agent.messages(),
        "usage":usage,
        "context_window":agent.context_window_tokens(),
        "available_input":agent.available_context_tokens(),
        "tool_names":agent.available_tool_names(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn high_frequency_text_is_coalesced_and_completion_is_never_lost() {
        let sink = DesktopEvents::default();
        for _ in 0..5000 {
            sink.on_event(RuntimeEvent::TextDelta("abc"));
        }
        sink.on_event(RuntimeEvent::ToolStarted {
            name: "bash",
            id: "1",
            arguments: "{}",
        });
        sink.on_event(RuntimeEvent::TaskFinished {
            status: "completed",
        });
        let events = sink.drain();
        assert!(events.len() < 50, "unbounded UI growth: {}", events.len());
        assert_eq!(events[events.len() - 1]["kind"], "task_finished");
        assert_eq!(events[events.len() - 2]["kind"], "tool_started");
    }
    #[test]
    fn output_frames_keep_channel_and_tool_identity() {
        let sink = DesktopEvents::default();
        sink.on_event(RuntimeEvent::ToolOutput {
            id: "a",
            stream: "stdout",
            chunk: "abc",
        });
        sink.on_event(RuntimeEvent::ToolOutput {
            id: "a",
            stream: "stderr",
            chunk: "warning",
        });
        sink.on_event(RuntimeEvent::ToolOutput {
            id: "a",
            stream: "stderr",
            chunk: "!",
        });
        let events = sink.drain();
        assert_eq!(events.len(), 2);
        assert_eq!(events[1]["text"], "warning!");
    }
}
