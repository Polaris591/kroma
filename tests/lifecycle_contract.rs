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
    plugins::PluginCatalog,
    prompt::PromptComposer,
    protocol::{FunctionCall, Message, ToolCall},
    tools::{ToolContext, ToolRegistry},
};
use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
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
            RuntimeEvent::FinalAnswerReady => "final-answer-ready".to_owned(),
            RuntimeEvent::TaskFinished { status } => format!("task-end-{status}"),
            RuntimeEvent::Warning { .. }
            | RuntimeEvent::CompactionStarted
            | RuntimeEvent::TextDelta(_)
            | RuntimeEvent::ReasoningDelta(_)
            | RuntimeEvent::ToolStarted { .. }
            | RuntimeEvent::ToolOutput { .. }
            | RuntimeEvent::ToolFinished { .. }
            | RuntimeEvent::ModelFailed
            | RuntimeEvent::FailedRequestUsage { .. }
            | RuntimeEvent::SessionCreated { .. }
            | RuntimeEvent::SessionSwitched { .. }
            | RuntimeEvent::SessionCleared { .. }
            | RuntimeEvent::SessionForked { .. }
            | RuntimeEvent::SessionRewound { .. } => return,
            RuntimeEvent::TurnGateContinued { plugin, .. } => format!("gate-continue-{plugin}"),
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
            "final-answer-ready",
            "compaction-usage-2",
            "task-end-completed",
        ]
    );
    drop(agent);
    fs::remove_dir_all(dir)?;
    Ok(())
}

#[tokio::test]
async fn cancelled_task_settles_completed_checkpoint_usage_immediately() -> Result<()> {
    let dir = isolated_dir("cancelled-checkpoint")?;
    let events = Arc::new(OrderEvents(Mutex::new(Vec::new())));
    let (started, ready) = oneshot::channel();
    let mut agent = Agent::with_runtime_budget(
        Arc::new(BlockingBackend {
            started: Mutex::new(Some(started)),
        }),
        ToolRegistry::builtin(),
        Arc::new(UsageFactory),
        Default::default(),
        Some(events.clone()),
        dir.clone(),
        ContextConfig::with_output_reserve(8192, 1024)?,
    )?;
    let mut on_text = |_: &str| {};
    let mut future = Box::pin(agent.run("cancel after checkpoint", &mut on_text));
    tokio::select! {
        result = ready => result?,
        result = &mut future => panic!("unexpected early completion: {result:?}"),
    }
    drop(future);
    agent.cancel()?;
    let recorded = events.0.lock().unwrap().clone();
    assert_eq!(
        recorded,
        [
            "task-started",
            "context",
            "model-start-1",
            "compaction-usage-2",
            "task-end-cancelled"
        ],
        "completed checkpoint usage must be charged to the cancelled task",
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

struct DelayedTaskFinished(AtomicUsize);

impl ModelBackend for DelayedTaskFinished {
    fn has_task_finished_hook(&self) -> bool {
        true
    }

    fn complete<'a>(
        &'a self,
        _: &'a [Message],
        _: &'a [Value],
        _: &'a mut (dyn FnMut(&str) + Send),
    ) -> StreamFuture<'a, Completion> {
        Box::pin(async {
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
        Box::pin(async { anyhow::bail!("not used") })
    }

    fn task_finished<'a>(
        &'a self,
        _: &'a str,
        archive: &'a Path,
        _: &'a [Message],
        status: &'a str,
    ) -> ModelFuture<'a, ()> {
        Box::pin(async move {
            // The WAL boundary must exist before the plugin gets called.
            assert!(wal(archive).contains("ok"));
            assert_eq!(status, "completed");
            tokio::time::sleep(Duration::from_millis(30)).await;
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }
}

#[tokio::test]
async fn short_lived_host_can_flush_post_wal_notifications_before_shutdown() -> Result<()> {
    let dir = isolated_dir("flush-notifications")?;
    let model = Arc::new(DelayedTaskFinished(AtomicUsize::new(0)));
    let mut agent = Agent::with_backends(model.clone(), ToolRegistry::builtin(), dir.clone())?;
    assert_eq!(agent.run("run once", &mut |_| {}).await?, "ok");
    assert_eq!(
        model.0.load(Ordering::SeqCst),
        0,
        "foreground completion should not synchronously wait for plugin maintenance"
    );
    agent.flush_task_notifications().await;
    assert_eq!(
        model.0.load(Ordering::SeqCst),
        1,
        "one-shot CLI shutdown must not discard a pending task_finished hook"
    );
    drop(agent);
    fs::remove_dir_all(dir)?;
    Ok(())
}

/// A subagent child is driven by a parent model, so its user-role text is
/// machine-written. It must never be offered to a plugin as a trusted human
/// quote that a memory write could promote into a durable rule.
#[tokio::test]
async fn child_tasks_never_offer_a_trusted_user_quote() -> Result<()> {
    if Command::new("node").arg("--version").output().is_err() {
        return Ok(());
    }
    let dir = isolated_dir("trusted-user")?;
    let plugin = dir.join("plugins").join("audit");
    fs::create_dir_all(&plugin)?;
    fs::write(
        plugin.join("plugin.toml"),
        "id='audit'\nname='Audit'\ncommand='node'\nargs=['main.js']\n[[tools]]\nname='audit_write'\ndescription='write an audit entry'\nrequires_user_context=true\n",
    )?;
    fs::write(
        plugin.join("main.js"),
        r#"const fs=require("node:fs");
require("node:readline").createInterface({input:process.stdin}).on("line",line=>{
 const req=JSON.parse(line);
 fs.appendFileSync("seen.log",JSON.stringify(req.trusted_user_context??null)+"\n");
 process.stdout.write(JSON.stringify({protocol_version:1,request_id:req.request_id,content:"ok"})+"\n");
});"#,
    )?;
    let state = dir.join("plugins.toml");
    PluginCatalog::discover_at(dir.join("plugins"), state.clone(), &dir)?
        .set_enabled("audit", true)?;
    let catalog = PluginCatalog::discover_at(dir.join("plugins"), state, &dir)?;
    let mut tools = ToolRegistry::builtin();
    assert!(catalog
        .contribute(&mut tools, &mut PromptComposer::default())
        .is_empty());
    let call = ToolCall {
        id: "audit-1".into(),
        kind: "function".into(),
        function: FunctionCall {
            name: "audit_write".into(),
            arguments: "{}".into(),
        },
    };
    let context = || ToolContext {
        model_connection: None,
        user_message: Some("Always use pnpm in this repository"),
        user_turn: Some(1),
        user_images: None,
        previous_user_images: None,
        workdir: &dir,
        archive: &dir,
        on_output: None,
        ask: None,
    };
    std::env::remove_var("KROMA_CHILD_SKIP_TASK_SUMMARY");
    assert!(!tools.execute(&call, context()).await.is_error);
    let seen = fs::read_to_string(plugin.join("seen.log"))?;
    assert!(
        seen.contains("\"message\":\"Always use pnpm in this repository\""),
        "a foreground turn may carry a trusted human quote: {seen}"
    );
    std::env::set_var("KROMA_CHILD_SKIP_TASK_SUMMARY", "1");
    let child = tools.execute(&call, context()).await;
    std::env::remove_var("KROMA_CHILD_SKIP_TASK_SUMMARY");
    assert!(!child.is_error, "{}", child.content);
    let seen = fs::read_to_string(plugin.join("seen.log"))?;
    let last = seen.lines().last().unwrap_or_default();
    assert_eq!(
        last, "null",
        "a child task must not turn model-written text into a trusted quote"
    );
    drop(tools);
    // A live plugin process keeps its directory busy on Windows.
    let _ = fs::remove_dir_all(dir);
    Ok(())
}
