//! Lifecycle ordering regression tests. No provider, credential or network
//! access is used; barriers and drops replace sleeps.
use anyhow::Result;
use kroma::{
    agent::Agent,
    context::{
        BuiltinContextFactory, CompactionUsage, ContextCommit, ContextConfig, ContextFactory,
        ContextPolicy,
    },
    context_state::PersistedContext,
    events::{EventSink, RuntimeEvent},
    model::{Completion, ModelBackend, ModelFuture, StreamFuture, TextResult},
    protocol::Message,
    tools::ToolRegistry,
};
use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc, Arc, Mutex,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::oneshot;

fn isolated_dir(label: &str) -> Result<PathBuf> {
    let dir = std::env::temp_dir().join(format!(
        "kroma-lifecycle-{label}-{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    ));
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn wal(path: &Path) -> String {
    fs::read_to_string(format!("{}.wal", path.display())).unwrap_or_default()
}

struct RecordingEvents {
    archive: Mutex<Option<PathBuf>>,
    events: Mutex<Vec<String>>,
    durable: mpsc::Sender<bool>,
    check_durable: bool,
}

impl RecordingEvents {
    fn new(check_durable: bool) -> (Arc<Self>, mpsc::Receiver<bool>) {
        let (sender, receiver) = mpsc::channel();
        (
            Arc::new(Self {
                archive: Mutex::new(None),
                events: Mutex::new(Vec::new()),
                durable: sender,
                check_durable,
            }),
            receiver,
        )
    }

    fn durable(&self, marker: &str) {
        if !self.check_durable {
            return;
        }
        let path = self.archive.lock().unwrap().clone().unwrap();
        let present = wal(&path).contains(marker);
        self.durable.send(present).unwrap();
    }
}

impl EventSink for RecordingEvents {
    fn set_session_context(&self, _id: &str, archive: &Path) {
        *self.archive.lock().unwrap() = Some(archive.to_path_buf());
    }

    fn on_event(&self, event: RuntimeEvent<'_>) {
        let label = match event {
            RuntimeEvent::TaskStarted => "task-started".to_owned(),
            RuntimeEvent::ModelStarted { request } => format!("model-start-{request}"),
            RuntimeEvent::ModelFinished { request, .. } => format!("model-end-{request}"),
            RuntimeEvent::ModelFailed => {
                self.durable("[Kroma interrupted");
                "model-failed".to_owned()
            }
            RuntimeEvent::TaskFinished { status } => {
                let marker = match status {
                    "completed" => "confirmed",
                    "cancelled" => "[Task cancelled by user.",
                    _ => "[Kroma interrupted",
                };
                self.durable(marker);
                format!("task-end-{status}")
            }
            _ => return,
        };
        self.events.lock().unwrap().push(label);
    }
}

struct FlakyBackend {
    calls: AtomicUsize,
}

impl ModelBackend for FlakyBackend {
    fn complete<'a>(
        &'a self,
        _: &'a [Message],
        _: &'a [Value],
        on_text: &'a mut (dyn FnMut(&str) + Send),
    ) -> StreamFuture<'a, Completion> {
        Box::pin(async move {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                anyhow::bail!("simulated provider failure");
            }
            on_text("confirmed");
            Ok(Completion {
                message: Message::new("assistant", "confirmed"),
                usage: None,
                elapsed: Duration::ZERO,
                first_token: None,
            })
        })
    }

    fn generate_text<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
        _: usize,
    ) -> ModelFuture<'a, TextResult> {
        Box::pin(async { anyhow::bail!("no background summary in this test") })
    }
}

struct BlockingBackend {
    started: Mutex<Option<oneshot::Sender<()>>>,
}

impl ModelBackend for BlockingBackend {
    fn complete<'a>(
        &'a self,
        _: &'a [Message],
        _: &'a [Value],
        _: &'a mut (dyn FnMut(&str) + Send),
    ) -> StreamFuture<'a, Completion> {
        Box::pin(async move {
            if let Some(started) = self.started.lock().unwrap().take() {
                let _ = started.send(());
            }
            std::future::pending::<Result<Completion>>().await
        })
    }

    fn generate_text<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
        _: usize,
    ) -> ModelFuture<'a, TextResult> {
        Box::pin(async { anyhow::bail!("no background summary in this test") })
    }
}

struct UsagePolicy;

impl ContextPolicy for UsagePolicy {
    fn state(&self) -> PersistedContext {
        PersistedContext::default()
    }

    fn take_usage(&mut self) -> CompactionUsage {
        CompactionUsage {
            requests: 2,
            input: Some(10),
            output: Some(5),
            cached: Some(0),
            ..Default::default()
        }
    }

    fn prepare<'a>(
        &'a mut self,
        messages: &'a [Message],
        _: &'a Arc<dyn ModelBackend>,
        _: &'a dyn ContextCommit,
        _: usize,
    ) -> StreamFuture<'a, Vec<Message>> {
        Box::pin(async move { Ok(messages.to_vec()) })
    }
}

struct UsageFactory;

impl ContextFactory for UsageFactory {
    fn initial(&self) -> PersistedContext {
        PersistedContext::default()
    }

    fn restore(
        &self,
        _: PersistedContext,
        _: ContextConfig,
        _: &Path,
    ) -> Result<Box<dyn ContextPolicy>> {
        Ok(Box::new(UsagePolicy))
    }
}

struct OkBackend;

impl ModelBackend for OkBackend {
    fn complete<'a>(
        &'a self,
        _: &'a [Message],
        _: &'a [Value],
        on_text: &'a mut (dyn FnMut(&str) + Send),
    ) -> StreamFuture<'a, Completion> {
        Box::pin(async move {
            on_text("ok");
            Ok(Completion {
                message: Message::new("assistant", "ok"),
                usage: None,
                elapsed: Duration::ZERO,
                first_token: None,
            })
        })
    }

    fn generate_text<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
        _: usize,
    ) -> ModelFuture<'a, TextResult> {
        Box::pin(async { anyhow::bail!("no background summary in this test") })
    }
}

struct OrderEvents(Mutex<Vec<String>>);

impl EventSink for OrderEvents {
    fn on_event(&self, event: RuntimeEvent<'_>) {
        let label = match event {
            RuntimeEvent::TaskStarted => "task-started".to_owned(),
            RuntimeEvent::ContextPrepared { .. } => "context".to_owned(),
            RuntimeEvent::ModelStarted { request } => format!("model-start-{request}"),
            RuntimeEvent::ModelFinished { request, .. } => format!("model-end-{request}"),
            RuntimeEvent::CompactionUsage { requests, .. } => {
                format!("compaction-usage-{requests}")
            }
            RuntimeEvent::TaskFinished { status } => format!("task-end-{status}"),
            RuntimeEvent::TextDelta(_)
            | RuntimeEvent::ReasoningDelta(_)
            | RuntimeEvent::ToolStarted { .. }
            | RuntimeEvent::ToolOutput { .. }
            | RuntimeEvent::ToolFinished { .. }
            | RuntimeEvent::ModelFailed
            | RuntimeEvent::SessionCreated { .. }
            | RuntimeEvent::SessionSwitched { .. }
            | RuntimeEvent::SessionCleared { .. }
            | RuntimeEvent::SessionForked { .. }
            | RuntimeEvent::SessionRewound { .. } => return,
        };
        self.0.lock().unwrap().push(label);
    }
}

#[tokio::test]
async fn compaction_usage_is_settled_before_foreground_task_finished() -> Result<()> {
    let dir = isolated_dir("usage-order")?;
    let events = Arc::new(OrderEvents(Mutex::new(Vec::new())));
    let mut agent = Agent::with_runtime_budget(
        Arc::new(OkBackend),
        ToolRegistry::builtin(),
        Arc::new(UsageFactory),
        Default::default(),
        Some(events.clone()),
        dir.clone(),
        ContextConfig::with_output_reserve(8192, 1024)?,
    )?;
    assert_eq!(agent.run("usage", &mut |_| {}).await?, "ok");
    assert_eq!(
        *events.0.lock().unwrap(),
        [
            "task-started",
            "context",
            "model-start-1",
            "model-end-1",
            "compaction-usage-2",
            "task-end-completed",
        ]
    );
    drop(agent);
    fs::remove_dir_all(dir)?;
    Ok(())
}

#[tokio::test]
async fn model_failure_is_durable_before_events_and_next_task_is_clean() -> Result<()> {
    let dir = isolated_dir("failure")?;
    let (events, durable) = RecordingEvents::new(true);
    let model = Arc::new(FlakyBackend {
        calls: AtomicUsize::new(0),
    });
    let mut agent = Agent::with_runtime_budget(
        model,
        ToolRegistry::builtin(),
        Arc::new(BuiltinContextFactory),
        Default::default(),
        Some(events.clone()),
        dir.clone(),
        ContextConfig::with_output_reserve(8192, 1024)?,
    )?;
    let input = agent.task_input();
    assert!(agent.run("fail once", &mut |_| {}).await.is_err());
    assert!(durable.try_recv().unwrap());
    assert!(durable.try_recv().unwrap());
    assert!(input.steer("must not leak").is_err());
    assert_eq!(input.pending_steering(), 0);

    assert_eq!(agent.run("recover", &mut |_| {}).await?, "confirmed");
    assert!(durable.try_recv().unwrap());
    let record = events.events.lock().unwrap().clone();
    assert_eq!(
        record,
        [
            "task-started",
            "model-start-1",
            "model-failed",
            "task-end-failed",
            "task-started",
            "model-start-1",
            "model-end-1",
            "task-end-completed",
        ]
    );
    let archive = events.archive.lock().unwrap().clone().unwrap();
    let log = wal(&archive);
    assert!(log.contains("[Kroma interrupted"), "failure note missing");
    assert!(log.contains("recover"));
    drop(agent);
    fs::remove_dir_all(dir)?;
    Ok(())
}

#[tokio::test]
async fn cancellation_is_durable_before_task_finished_and_drops_steering() -> Result<()> {
    let dir = isolated_dir("cancel")?;
    let (events, durable) = RecordingEvents::new(true);
    let (started, ready) = oneshot::channel();
    let model = Arc::new(BlockingBackend {
        started: Mutex::new(Some(started)),
    });
    let mut agent = Agent::with_runtime_budget(
        model,
        ToolRegistry::builtin(),
        Arc::new(BuiltinContextFactory),
        Default::default(),
        Some(events.clone()),
        dir.clone(),
        ContextConfig::with_output_reserve(8192, 1024)?,
    )?;
    let input = agent.task_input();
    let mut on_text = |_: &str| {};
    let mut future = Box::pin(agent.run("cancel this", &mut on_text));
    tokio::select! {
        result = ready => result?,
        result = &mut future => panic!("unexpected early completion: {result:?}"),
    }
    input.steer("drop with the cancelled task")?;
    drop(future);
    agent.cancel()?;
    assert!(durable.try_recv().unwrap());
    assert_eq!(input.pending_steering(), 0);
    assert!(input.steer("late").is_err());
    let record = events.events.lock().unwrap().clone();
    assert_eq!(
        record,
        ["task-started", "model-start-1", "task-end-cancelled"]
    );
    let archive = events.archive.lock().unwrap().clone().unwrap();
    let log = wal(&archive);
    assert!(log.contains("[Task cancelled by user."));
    assert!(log.contains("cancel this"));
    drop(agent);
    fs::remove_dir_all(dir)?;
    Ok(())
}
