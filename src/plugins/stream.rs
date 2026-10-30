//! Bounded streaming JSONL frames for external model providers.
//! Stream callbacks run on the async caller, never on the blocking stdout
//! reader. The existing per-plugin request mutex serializes stream and tool
//! calls, and dropping a stream cancels its child process tree.
use super::{PluginCallGuard, PluginProcess, PluginWorker};
use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use tokio::{sync::mpsc, task::JoinHandle};

pub(super) struct PluginStream {
    receiver: mpsc::Receiver<Value>,
    task: JoinHandle<Result<()>>,
}

impl PluginStream {
    pub(super) async fn next(&mut self) -> Option<Value> {
        self.receiver.recv().await
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
                        "text_delta" | "reasoning_delta" | "complete" | "error"
                    ) {
                        bail!("unsupported streaming model event '{event}'");
                    }
                    if matches!(event, "text_delta" | "reasoning_delta")
                        && !frame["delta"].is_string()
                    {
                        bail!("{event} requires a string delta");
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
