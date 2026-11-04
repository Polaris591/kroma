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
    /// Paint events discarded because the host did not drain fast enough.
    dropped: usize,
}

/// One sink per runtime. Cheap synchronous writes from the model/tool threads;
/// a desktop host drains at its frame rate instead of painting every token.
#[derive(Default)]
pub struct DesktopEvents {
    pending: Mutex<Pending>,
    /// Serializes ledger flushes so batches stay in observation order.
    flush: Mutex<()>,
}
impl DesktopEvents {
    pub fn drain(&self) -> Vec<Value> {
        let mut lock = self.pending.lock().expect("desktop event lock");
        let dropped = std::mem::replace(&mut lock.dropped, 0);
        let mut events: Vec<Value> = lock.events.drain(..).collect();
        if dropped > 0 {
            // Tell the host that paint events were lost and the durable
            // transcript, not this stream, is authoritative.
            events.insert(0, json!({"kind": "resync_required", "dropped": dropped}));
        }
        events
    }

    pub fn persist_usage(&self, catalog: &SessionCatalog, session: &str) -> Result<SessionUsage> {
        // One flush at a time so batches reach the ledger in observation order.
        let _flush = self.flush.lock().expect("desktop usage flush lock");
        let (usage, compaction) = {
            let mut pending = self.pending.lock().expect("desktop event lock");
            (
                std::mem::take(&mut pending.usage),
                std::mem::take(&mut pending.compaction),
            )
        };
        match catalog.add_usage(session, &usage, &compaction) {
            Ok(saved) => Ok(saved),
            Err(error) => {
                // Never delete measured usage that was not written. Restore the
                // batch ahead of anything observed since the failed attempt.
                let mut pending = self.pending.lock().expect("desktop event lock");
                let mut restored_usage = usage;
                restored_usage.append(&mut pending.usage);
                pending.usage = restored_usage;
                let mut restored_compaction = compaction;
                restored_compaction.append(&mut pending.compaction);
                pending.compaction = restored_compaction;
                Err(error)
            }
        }
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
        }
        if state.events.len() >= MAX_PENDING {
            // Drop a coalescible delta first: the host resyncs from WAL on
            // task completion. If a stalled host leaves nothing to merge,
            // the oldest paint event goes instead of growing without
            // bound; the durable transcript remains authoritative.
            let coalesced = state.events.iter().position(|item| {
                matches!(
                    item["kind"].as_str(),
                    Some("text_delta" | "reasoning_delta" | "tool_output")
                )
            });
            if let Some(index) = coalesced {
                state.events.remove(index);
            } else {
                state.events.pop_front();
            }
            state.dropped = state.dropped.saturating_add(1);
        }
        state.events.push_back(event);
    }
}

impl EventSink for DesktopEvents {
    fn on_event(&self, event: RuntimeEvent<'_>) {
        match event {
            RuntimeEvent::TaskStarted => self.push(json!({"kind":"task_started"})),
            RuntimeEvent::CompactionStarted => self.push(json!({"kind":"compaction_started"})),
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
            RuntimeEvent::FailedRequestUsage {
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
                self.push(json!({"kind":"failed_request_usage","input":input_tokens,
                    "output":output_tokens,"cached":cached_tokens,
                    "reasoning_tokens":reasoning_tokens}));
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
const MAX_SNAPSHOT_MESSAGES: usize = 1_000;

pub fn snapshot(agent: &Agent) -> Result<Value> {
    snapshot_page(agent, None, MAX_SNAPSHOT_MESSAGES)
}

/// Return one contiguous slice of the durable transcript. `before` is an
/// exclusive end index; `next_before` is the cursor for older pages.
pub fn snapshot_page(agent: &Agent, before: Option<usize>, limit: usize) -> Result<Value> {
    let catalog = SessionCatalog::new(agent.workdir())?;
    let usage = if agent.session_id() == "unknown" {
        SessionUsage::default()
    } else {
        catalog.usage(agent.session_id())?
    };
    let messages = agent.messages();
    let total = messages.len();
    let end = before.unwrap_or(total).min(total);
    let start = end.saturating_sub(limit.clamp(1, MAX_SNAPSHOT_MESSAGES));
    Ok(json!({
        "session_id":agent.session_id(),
        "messages":&messages[start..end],
        "total_messages":total,
        "history_truncated":start > 0,
        "next_before":(start > 0).then_some(start),
        "usage":usage,
        "context_window":agent.context_window_tokens(),
        "model_limits":agent.model_report(),
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
    fn a_stalled_host_cannot_grow_the_paint_queue_without_bound() {
        let events = DesktopEvents::default();
        for request in 0..600 {
            events.on_event(RuntimeEvent::ModelStarted { request });
        }
        let drained = events.drain();
        assert_eq!(
            drained.len(),
            MAX_PENDING + 1,
            "the paint queue must stay bounded regardless of event kind"
        );
        assert_eq!(drained[0]["kind"], "resync_required");
        assert_eq!(drained[0]["dropped"], 600 - MAX_PENDING);
        assert_eq!(drained.last().unwrap()["request"], 599);
        let empty = events.drain();
        assert!(empty.is_empty(), "a drained queue reports nothing new");
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
    #[test]
    fn a_failed_usage_write_keeps_the_batch_for_the_next_attempt() -> Result<()> {
        let dir = std::env::temp_dir().join(format!(
            "kroma-desktop-usage-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir)?;
        // Keep every byte inside the temp dir: `new` resolves the real HOME
        // session file, which does not exist on a fresh CI runner.
        let catalog = SessionCatalog::from_default(dir.join("default.json"))?;
        std::fs::write(catalog.default_path(), "{}")?;
        let events = DesktopEvents::default();
        events.on_event(RuntimeEvent::ModelFinished {
            request: 1,
            tools: 0,
            reasoning: None,
            input_tokens: Some(123),
            output_tokens: Some(4),
            reasoning_tokens: None,
            cached_tokens: Some(0),
        });
        let ledger = catalog.default_path().with_extension("usage.json");
        std::fs::write(&ledger, b"not json")?;
        assert!(
            events.persist_usage(&catalog, "default").is_err(),
            "a corrupt ledger must fail loudly"
        );
        std::fs::write(&ledger, b"{}")?;
        let saved = events.persist_usage(&catalog, "default")?;
        assert_eq!(
            saved.coding.calls, 1,
            "measured usage must survive a failed flush"
        );
        assert_eq!(saved.coding.input, 123);
        let again = events.persist_usage(&catalog, "default")?;
        assert_eq!(again.coding.calls, 1, "a flushed batch is never re-applied");
        std::fs::remove_dir_all(dir).ok();
        Ok(())
    }
}
