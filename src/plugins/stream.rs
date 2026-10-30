//! Bounded streaming JSONL frames for external model providers.
//! Stream callbacks run on the async caller, never on the blocking stdout
//! reader. The existing per-plugin request mutex serializes stream and tool
//! calls, and dropping a stream cancels its child process tree.
use super::{PluginCallGuard, PluginProcess, PluginWorker};
use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{sync::mpsc, task::JoinHandle};

const STREAM_FRAME_TIMEOUT: Duration = Duration::from_secs(120);

/// Provider-neutral streamed tool-call arguments. A process plugin normalizes
/// upstream provider fragments into an index, stable id/name and JSON text
/// fragments. Kroma validates the fully assembled call before the Agent sees it.
#[derive(Default)]
pub(super) struct ToolCallFragments {
    calls: std::collections::BTreeMap<usize, PartialToolCall>,
}

#[derive(Default)]
struct PartialToolCall {
    id: Option<String>,
    name: Option<String>,
    arguments: String,
}

impl ToolCallFragments {
    pub(super) fn push(&mut self, frame: &Value) -> Result<()> {
        const MAX_CALLS: usize = 64;
        const MAX_ARGUMENT_BYTES: usize = 1024 * 1024;
        let index = frame["index"]
            .as_u64()
            .and_then(|value| usize::try_from(value).ok())
            .context("tool_call_delta requires a nonnegative integer index")?;
        if index >= MAX_CALLS {
            bail!("streamed tool call index exceeds 63");
        }
        let item = self.calls.entry(index).or_default();
        for (field, target) in [("id", &mut item.id), ("name", &mut item.name)] {
            if let Some(value) = frame.get(field) {
                let text = value
                    .as_str()
                    .context("tool call id/name must be strings")?;
                if text.is_empty() || text.len() > 256 {
                    bail!("tool call {field} has invalid length");
                }
                if target.as_deref().is_some_and(|old| old != text) {
                    bail!("tool call {field} changed during streaming");
                }
                *target = Some(text.to_owned());
            }
        }
        if let Some(fragment) = frame.get("arguments_delta") {
            let text = fragment
                .as_str()
                .context("tool_call_delta arguments_delta must be text")?;
            if item.arguments.len().saturating_add(text.len()) > MAX_ARGUMENT_BYTES {
                bail!("streamed tool arguments exceed 1 MiB");
            }
            item.arguments.push_str(text);
        }
        Ok(())
    }

    pub(super) fn finish(self) -> Result<Option<Vec<crate::protocol::ToolCall>>> {
        use crate::protocol::{FunctionCall, ToolCall};
        if self.calls.is_empty() {
            return Ok(None);
        }
        let mut result = Vec::with_capacity(self.calls.len());
        let mut seen_ids = std::collections::BTreeSet::new();
        for (index, call) in self.calls {
            if index != result.len() {
                bail!("streamed tool-call indexes must start at 0 and be contiguous");
            }
            let id = call.id.context("streamed tool call has no id")?;
            let name = call
                .name
                .context("streamed tool call has no function name")?;
            if !seen_ids.insert(id.clone()) {
                bail!("streamed tool-call IDs must be unique");
            }
            let arguments: Value = serde_json::from_str(&call.arguments)
                .context("streamed tool arguments are not valid JSON")?;
            if !arguments.is_object() {
                bail!("streamed tool arguments must be a JSON object");
            }
            result.push(ToolCall {
                id,
                kind: "function".into(),
                function: FunctionCall {
                    name,
                    arguments: call.arguments,
                },
            });
        }
        Ok(Some(result))
    }
}

pub(super) struct PluginStream {
    receiver: mpsc::Receiver<Value>,
    task: JoinHandle<Result<()>>,
}

impl PluginStream {
    pub(super) async fn next(&mut self) -> Option<Value> {
        match tokio::time::timeout(STREAM_FRAME_TIMEOUT, self.receiver.recv()).await {
            Ok(frame) => frame,
            Err(_) => {
                // Abort the streaming task; its PluginCallGuard kills the
                // plugin process tree, so a stalled plugin cannot linger.
                self.task.abort();
                None
            }
        }
    }

    pub(super) async fn finish(mut self) -> Result<()> {
        (&mut self.task)
            .await
            .context("streaming plugin task failed")?
    }
}

impl Drop for PluginStream {
    fn drop(&mut self) {
        // Cancelling the caller must drop PluginCallGuard even if the plugin
        // stopped sending. The guard kills the plugin tree and unblocks read.
        self.task.abort();
    }
}

impl PluginWorker {
    pub(super) fn start_stream(self: &Arc<Self>, message: Value) -> PluginStream {
        // Backpressure prevents an external model from buffering unlimited
        // streamed tokens in memory when rendering is slow.
        let (sender, receiver) = mpsc::channel(32);
        let worker = self.clone();
        let task = tokio::spawn(async move { worker.invoke_stream(message, sender).await });
        PluginStream { receiver, task }
    }

    async fn invoke_stream(
        self: &Arc<Self>,
        message: Value,
        sender: mpsc::Sender<Value>,
    ) -> Result<()> {
        let id = self.next_request.fetch_add(1, Ordering::Relaxed);
        let cancelled = Arc::new(AtomicBool::new(false));
        let mut guard = PluginCallGuard {
            worker: self.clone(),
            id,
            cancelled: cancelled.clone(),
            armed: true,
        };
        let worker = self.clone();
        let result = tokio::task::spawn_blocking(move || {
            worker.invoke_with(message, Some((id, &cancelled)), |process, request| {
                process.write_request(&request)?;
                let request_id = request.get("request_id");
                loop {
                    let frame = process.read_frame()?;
                    PluginProcess::validate_frame(&frame, request_id, true)?;
                    let event = frame
                        .get("event")
                        .and_then(Value::as_str)
                        .context("stream frame has no event")?;
                    if !matches!(
                        event,
                        "text_delta"
                            | "reasoning_delta"
                            | "tool_call_delta"
                            | "output_delta"
                            | "complete"
                            | "error"
                    ) {
                        bail!("unsupported streaming model event '{event}'");
                    }
                    if matches!(event, "text_delta" | "reasoning_delta" | "output_delta")
                        && !frame["delta"].is_string()
                    {
                        bail!("{event} requires a string delta");
                    }
                    if event == "output_delta"
                        && frame.get("stream").is_some()
                        && !matches!(frame["stream"].as_str(), Some("stdout" | "stderr"))
                    {
                        bail!("output_delta stream must be stdout or stderr");
                    }
                    let terminal = matches!(event, "complete" | "error");
                    sender
                        .blocking_send(frame)
                        .map_err(|_| anyhow::anyhow!("stream listener dropped"))?;
                    if terminal {
                        return Ok(());
                    }
                }
            })
        })
        .await
        .context("streaming plugin worker failed")?;
        // Only disarm after the worker released its process and received a
        // terminal frame. Cancellation and malformed JSON kill the worker.
        if result.is_ok() {
            guard.armed = false;
        }
        result
    }
}

#[cfg(test)]
mod tool_fragments_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn fragments_are_ordered_and_validated() {
        let mut assembled = ToolCallFragments::default();
        assembled
            .push(&json!({"index": 1, "id":"b", "name":"lookup", "arguments_delta":"{\"query\":"}))
            .unwrap();
        assembled
            .push(
                &json!({"index": 0, "id":"a", "name":"shell", "arguments_delta":"{\"command\":\""}),
            )
            .unwrap();
        assembled
            .push(&json!({"index": 1, "arguments_delta":"\"汉字\"}"}))
            .unwrap();
        assembled
            .push(&json!({"index": 0, "arguments_delta":"pwd\"}"}))
            .unwrap();
        let calls = assembled.finish().unwrap().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].id, "a");
        assert_eq!(calls[0].function.arguments, r#"{"command":"pwd"}"#);
        assert_eq!(calls[1].function.name, "lookup");
        assert_eq!(calls[1].function.arguments, r#"{"query":"汉字"}"#);
    }

    #[test]
    fn rejects_holes_duplicate_ids_and_mutated_metadata() {
        let mut sparse = ToolCallFragments::default();
        sparse
            .push(&json!({"index":1,"id":"a","name":"shell","arguments_delta":"{}"}))
            .unwrap();
        assert!(sparse.finish().is_err());

        let mut duplicates = ToolCallFragments::default();
        for index in 0..2 {
            duplicates
                .push(&json!({"index":index,"id":"same","name":"shell","arguments_delta":"{}"}))
                .unwrap();
        }
        assert!(duplicates.finish().is_err());

        let mut changed = ToolCallFragments::default();
        changed
            .push(&json!({"index":0,"id":"a","name":"shell","arguments_delta":"{}"}))
            .unwrap();
        assert!(changed.push(&json!({"index":0,"name":"other"})).is_err());
    }

    #[test]
    fn rejects_invalid_arguments_and_oversized_calls() {
        let mut invalid = ToolCallFragments::default();
        invalid
            .push(&json!({"index":0,"id":"a","name":"shell","arguments_delta":"{"}))
            .unwrap();
        assert!(invalid.finish().is_err());

        let mut huge = ToolCallFragments::default();
        huge.push(&json!({"index":0,"id":"a","name":"shell"}))
            .unwrap();
        assert!(huge
            .push(&json!({"index":0,"arguments_delta":"x".repeat(1024*1024+1)}))
            .is_err());
    }
}
