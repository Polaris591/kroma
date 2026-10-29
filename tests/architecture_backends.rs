//! P2 integration: register a tool and swap the model without modifying Agent.
use anyhow::{Context, Result};
use kroma::{
    agent::Agent,
    context::{
        BuiltinContextFactory, CompactionUsage, ContextCommit, ContextConfig, ContextFactory,
        ContextPolicy,
    },
    context_state::{ExternalContextState, PersistedContext},
    events::{EventSink, RuntimeEvent},
    model::{ModelBackend, ModelFuture, StreamFuture},
    prompt::{PromptComposer, EXECUTION_GUIDANCE},
    protocol::{FunctionCall, Message, ToolCall},
    provider::{Completion, SummaryResult, Usage},
    tools::{Tool, ToolContext, ToolFuture, ToolRegistry, ToolResult},
};
use serde_json::{json, Value};
use std::{
    path::Path,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

struct CounterPolicy {
    counter: u64,
}

impl ContextPolicy for CounterPolicy {
    fn state(&self) -> PersistedContext {
        PersistedContext::External(ExternalContextState {
            strategy: "test.counter".to_owned(),
            version: 1,
            data: json!({"counter":self.counter}),
        })
    }

    fn abort(&mut self) {}

    fn new_user_turn(&mut self) {}

    fn mark_consumed(&mut self, _: usize) {}

    fn observe(&mut self, _: &[Message], _: Option<u64>, _: Option<u64>) {}

    fn take_usage(&mut self) -> CompactionUsage {
        CompactionUsage::default()
    }

    fn prepare<'a>(
        &'a mut self,
        messages: &'a [Message],
        _: &'a Arc<dyn ModelBackend>,
        store: &'a dyn ContextCommit,
        _: usize,
    ) -> StreamFuture<'a, Vec<Message>> {
        Box::pin(async move {
            self.counter += 1;
            store.commit(&self.state())?;
            Ok(messages.to_vec())
        })
    }

    fn finish_task<'a>(
        &'a mut self,
        _: &'a [Message],
        _: &'a dyn ContextCommit,
    ) -> StreamFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }
}

struct CounterFactory {
    restored: Arc<Mutex<Vec<u64>>>,
}

#[tokio::test]
async fn custom_context_can_fork_and_rewind_without_losing_saved_future_state() -> Result<()> {
    let dir = std::env::temp_dir().join(format!(
        "kroma-custom-branch-{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    ));
    std::fs::create_dir_all(&dir)?;
    let restored = Arc::new(Mutex::new(Vec::new()));
    let factory: Arc<dyn ContextFactory> = Arc::new(CounterFactory {
        restored: restored.clone(),
    });
    let model = Arc::new(FakeBackend {
        calls: AtomicUsize::new(0),
        tools_seen: Mutex::new(Vec::new()),
        system_prompts: Mutex::new(Vec::new()),
    });
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(FakeTool))?;
    let mut agent = Agent::with_components(model, tools, factory, dir.clone())?;
    agent.run("first", &mut |_| {}).await?;
    agent.run("second", &mut |_| {}).await?;
    let branch = agent.fork(Some(1))?;
    assert_eq!(agent.history().len(), 2);
    agent.resume_session("default")?;
    assert_eq!(agent.history().len(), 3);
    let future = agent.rewind(1)?;
    assert_eq!(agent.history().len(), 2);
    agent.resume_session(&future)?;
    assert_eq!(agent.history().len(), 3);
    agent.resume_session(&branch)?;
    assert_eq!(agent.history().len(), 2);
    let seen = restored.lock().unwrap().clone();
    assert!(
        seen.contains(&3),
        "full old future must restore prior counter: {seen:?}"
    );
    assert!(
        seen.contains(&1),
        "shortened fork must restore trimmed counter: {seen:?}"
    );
    drop(agent);
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

impl ContextFactory for CounterFactory {
    fn initial(&self) -> PersistedContext {
        CounterPolicy { counter: 0 }.state()
    }

    fn branch_state(
        &self,
        state: &PersistedContext,
        source: &[Message],
        retained: &[Message],
        _: &Path,
        _: &Path,
    ) -> Result<PersistedContext> {
        if source.len() == retained.len() {
            return Ok(state.clone());
        }
        // This demo strategy counts model requests; history truncation resets
        // its derived counter to the number of retained assistant responses.
        let counter = retained
            .iter()
            .filter(|m| m.role == "assistant" && m.tool_calls.is_none())
            .count() as u64;
        Ok(CounterPolicy { counter }.state())
    }

    fn restore(
        &self,
        state: PersistedContext,
        _: ContextConfig,
        _: &Path,
    ) -> Result<Box<dyn ContextPolicy>> {
        let PersistedContext::External(external) = state else {
            anyhow::bail!("wrong context state implementation");
        };
        if external.strategy != "test.counter" || external.version != 1 {
            anyhow::bail!("wrong external context identifier or version");
        }
        let counter = external.data["counter"]
            .as_u64()
            .ok_or_else(|| anyhow::anyhow!("missing counter"))?;
        self.restored.lock().unwrap().push(counter);
        Ok(Box::new(CounterPolicy { counter }))
    }
}

struct FakeBackend {
    calls: AtomicUsize,
    tools_seen: Mutex<Vec<Value>>,
    system_prompts: Mutex<Vec<String>>,
}

impl ModelBackend for FakeBackend {
    fn complete<'a>(
        &'a self,
        messages: &'a [Message],
        tools: &'a [Value],
        on_text: &'a mut (dyn FnMut(&str) + Send),
    ) -> StreamFuture<'a, Completion> {
        Box::pin(async move {
            self.tools_seen.lock().unwrap().extend_from_slice(tools);
            self.system_prompts.lock().unwrap().push(
                messages
                    .first()
                    .and_then(|m| m.content.as_deref())
                    .unwrap_or("")
                    .to_owned(),
            );
            let ordinal = self.calls.fetch_add(1, Ordering::SeqCst);
            let message = if ordinal == 0 {
                Message {
                    role: "assistant".into(),
                    content: Some(String::new()),
                    reasoning_content: Some("fake thought".into()),
                    tool_calls: Some(vec![ToolCall {
                        id: "plugin-call".into(),
                        kind: "function".into(),
                        function: FunctionCall {
                            name: "integration_test".into(),
                            arguments: r#"{"value":"hello"}"#.into(),
                        },
                    }]),
                    tool_call_id: None,
                    provider_items: None,
                }
            } else {
                let tool_message = messages
                    .iter()
                    .find(|m| m.role == "tool" && m.tool_call_id.as_deref() == Some("plugin-call"))
                    .expect("tool result must be committed before continuation");
                assert_eq!(tool_message.content.as_deref(), Some("echo:hello"));
                on_text("finished");
                Message::new("assistant", "finished")
            };
            Ok(Completion {
                message,
                usage: Some(Usage {
                    input: Some(100),
                    output: Some(5),
                    cached: Some(0),
                    ..Default::default()
                }),
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
    ) -> ModelFuture<'a, SummaryResult> {
        Box::pin(async { anyhow::bail!("not expected in short-history test") })
    }
}

struct FakeTool;

impl Tool for FakeTool {
    fn name(&self) -> &str {
        "integration_test"
    }

    fn definition(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": self.name(),
                "description": "Internal architecture test tool",
                "parameters": {
                    "type": "object",
                    "properties": {"value": {"type": "string"}},
                    "required": ["value"]
                }
            }
        })
    }

    fn execute<'a>(&'a self, call: &'a ToolCall, _: ToolContext<'a>) -> ToolFuture<'a> {
        Box::pin(async move {
            let value: Value = serde_json::from_str(&call.function.arguments).unwrap();
            ToolResult::ok(format!("echo:{}", value["value"].as_str().unwrap()))
        })
    }
}

#[tokio::test]
async fn runtime_accepts_independent_model_and_registered_tool() -> Result<()> {
    let workdir = std::env::temp_dir().join(format!(
        "kroma-test-plugin-{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    ));
    std::fs::create_dir_all(&workdir)?;
    let fake = Arc::new(FakeBackend {
        calls: AtomicUsize::new(0),
        tools_seen: Mutex::new(Vec::new()),
        system_prompts: Mutex::new(Vec::new()),
    });
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(FakeTool))?;
    assert!(registry.register(Arc::new(FakeTool)).is_err());
    let definitions = registry.definitions();
    assert_eq!(definitions.len(), 1);
    assert_eq!(definitions[0]["function"]["name"], "integration_test");
    let mut agent = Agent::with_backends(fake.clone(), registry, workdir.clone())?;
    let mut streamed = String::new();
    let outcome = agent
        .run("call the internal test tool", &mut |text| {
            streamed.push_str(text)
        })
        .await?;
    assert_eq!(outcome, "finished");
    assert!(streamed.contains("finished"));
    assert_eq!(fake.calls.load(Ordering::SeqCst), 2);
    assert_eq!(fake.tools_seen.lock().unwrap().len(), 2);
    drop(agent);
    // Drop the agent before deleting its private session lock and captures.
    let _ = std::fs::remove_dir_all(&workdir);
    Ok(())
}

#[tokio::test]
async fn custom_context_state_survives_wal_and_process_restart() -> Result<()> {
    let dir = std::env::temp_dir().join(format!(
        "kroma-custom-context-{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    ));
    std::fs::create_dir_all(&dir)?;
    let restored = Arc::new(Mutex::new(Vec::new()));
    let factory: Arc<dyn ContextFactory> = Arc::new(CounterFactory {
        restored: restored.clone(),
    });
    let fake = Arc::new(FakeBackend {
        calls: AtomicUsize::new(0),
        tools_seen: Mutex::new(Vec::new()),
        system_prompts: Mutex::new(Vec::new()),
    });
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(FakeTool))?;
    let mut first = Agent::with_components(fake, tools, factory.clone(), dir.clone())?;
    assert_eq!(first.run("first", &mut |_| {}).await?, "finished");
    drop(first);

    // The original raw tool result and the external strategy state are
    // restored together from the snapshot plus WAL, without a new format
    // for built-in sessions or a change to the Agent loop.
    let fake = Arc::new(FakeBackend {
        calls: AtomicUsize::new(1),
        tools_seen: Mutex::new(Vec::new()),
        system_prompts: Mutex::new(Vec::new()),
    });
    let mut second = Agent::with_components(fake, ToolRegistry::new(), factory, dir.clone())?;
    assert_eq!(second.run("second", &mut |_| {}).await?, "finished");
    drop(second);
    assert_eq!(*restored.lock().unwrap(), vec![0, 2]);

    // The saved state must not be silently loaded as the built-in policy.
    let err = Agent::with_backends(
        Arc::new(FakeBackend {
            calls: AtomicUsize::new(1),
            tools_seen: Mutex::new(Vec::new()),
            system_prompts: Mutex::new(Vec::new()),
        }),
        ToolRegistry::builtin(),
        dir.clone(),
    );
    assert!(err.is_err());
    assert!(format!("{:#}", err.err().unwrap()).contains("test.counter"));
    // Disabling an extension must not permanently strand its conversation.
    // Recovery is explicit and retains original messages and tool results.
    assert!(Agent::recover_builtin_context(&dir, "default")?);
    assert!(!Agent::recover_builtin_context(&dir, "default")?);
    let fresh = Arc::new(FakeBackend {
        calls: AtomicUsize::new(1),
        tools_seen: Mutex::new(Vec::new()),
        system_prompts: Mutex::new(Vec::new()),
    });
    let mut recovered = Agent::with_backends(fresh, ToolRegistry::builtin(), dir.clone())?;
    assert_eq!(
        recovered.run("after recovery", &mut |_| {}).await?,
        "finished"
    );
    drop(recovered);
    let _ = std::fs::remove_dir_all(dir);
    Ok(())
}

#[derive(Default)]
struct RecordingEvents(Mutex<Vec<String>>);

impl EventSink for RecordingEvents {
    fn on_event(&self, event: RuntimeEvent<'_>) {
        let name = match event {
            RuntimeEvent::TaskStarted => "task-started".to_owned(),
            RuntimeEvent::ContextPrepared { .. } => "context".to_owned(),
            RuntimeEvent::ModelStarted { request } => format!("model-start-{request}"),
            RuntimeEvent::TextDelta(text) => format!("text:{text}"),
            RuntimeEvent::ReasoningDelta(text) => format!("reasoning:{text}"),
            RuntimeEvent::ModelFinished { request, .. } => format!("model-end-{request}"),
            RuntimeEvent::ModelFailed => "model-failed".to_owned(),
            RuntimeEvent::ToolStarted { name, .. } => format!("tool-start:{name}"),
            RuntimeEvent::ToolOutput { .. } => "tool-output".to_owned(),
            RuntimeEvent::ToolFinished { name, result, .. } => {
                assert_eq!(result, "echo:hello");
                format!("tool-end:{name}")
            }
            RuntimeEvent::TaskFinished { status } => format!("task-end:{status}"),
            RuntimeEvent::SessionCreated { .. }
            | RuntimeEvent::SessionSwitched { .. }
            | RuntimeEvent::SessionCleared { .. }
            | RuntimeEvent::SessionForked { .. }
            | RuntimeEvent::SessionRewound { .. } => "session-event".to_owned(),
        };
        self.0.lock().unwrap().push(name);
    }
}

#[tokio::test]
async fn prompt_contribution_is_projection_only_and_events_have_stable_order() -> Result<()> {
    let dir = std::env::temp_dir().join(format!(
        "kroma-prompt-events-{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    ));
    std::fs::create_dir_all(&dir)?;
    let guidance = "\nTest extension: use deliberate verification.";
    let prompts = PromptComposer::default().with_extra(guidance)?;
    let events = Arc::new(RecordingEvents::default());
    let fake = Arc::new(FakeBackend {
        calls: AtomicUsize::new(0),
        tools_seen: Mutex::new(Vec::new()),
        system_prompts: Mutex::new(Vec::new()),
    });
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(FakeTool))?;
    let mut agent = Agent::with_runtime(
        fake.clone(),
        registry,
        Arc::new(BuiltinContextFactory),
        prompts,
        Some(events.clone()),
        dir.clone(),
    )?;
    assert_eq!(agent.run("first", &mut |_| {}).await?, "finished");
    for system in fake.system_prompts.lock().unwrap().iter() {
        assert_eq!(system.matches(guidance).count(), 1);
        assert_eq!(system.matches(EXECUTION_GUIDANCE).count(), 1);
        assert!(system.contains("Your available tools are provided"));
        assert!(!system.contains("Your only tool is shell"));
    }
    let record = events.0.lock().unwrap().clone();
    assert_eq!(
        record,
        [
            "task-started",
            "context",
            "model-start-1",
            "model-end-1",
            "tool-start:integration_test",
            "tool-end:integration_test",
            "context",
            "model-start-2",
            "text:finished",
            "model-end-2",
            "task-end:completed",
        ]
    );
    drop(agent);

    // Reopen the saved conversation; extensions must remain request-only
    // and must not multiply because the user resumed a session.
    let resumed = Arc::new(FakeBackend {
        calls: AtomicUsize::new(1),
        tools_seen: Mutex::new(Vec::new()),
        system_prompts: Mutex::new(Vec::new()),
    });
    let mut agent = Agent::with_runtime(
        resumed.clone(),
        ToolRegistry::new(),
        Arc::new(BuiltinContextFactory),
        PromptComposer::default().with_extra(guidance)?,
        None,
        dir.clone(),
    )?;
    assert_eq!(agent.run("second", &mut |_| {}).await?, "finished");
    let captured = resumed.system_prompts.lock().unwrap();
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0].matches(guidance).count(), 1);
    assert_eq!(captured[0].matches(EXECUTION_GUIDANCE).count(), 1);
    drop(captured);
    drop(agent);
    let _ = std::fs::remove_dir_all(dir);
    Ok(())
}

#[derive(Default)]
struct DurableEvents {
    archive: Mutex<Option<std::path::PathBuf>>,
    observed: Mutex<Vec<bool>>,
}

impl EventSink for DurableEvents {
    fn on_event(&self, event: RuntimeEvent<'_>) {
        if let RuntimeEvent::ToolFinished { id, result, .. } = event {
            let path = self.archive.lock().unwrap().clone().unwrap();
            let journal = std::fs::read_to_string(format!("{}.wal", path.display())).unwrap();
            assert_eq!(result, "echo:hello");
            self.observed
                .lock()
                .unwrap()
                .push(journal.contains(id) && journal.contains("echo:hello"));
        }
    }
}

#[tokio::test]
async fn tool_finished_is_durable_and_agent_future_is_send() -> Result<()> {
    let dir = std::env::temp_dir().join(format!(
        "kroma-events-send-{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    ));
    std::fs::create_dir_all(&dir)?;
    let events = Arc::new(DurableEvents::default());
    let model = Arc::new(FakeBackend {
        calls: AtomicUsize::new(0),
        tools_seen: Mutex::new(Vec::new()),
        system_prompts: Mutex::new(Vec::new()),
    });
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(FakeTool))?;
    let mut agent = Agent::with_runtime(
        model,
        tools,
        Arc::new(BuiltinContextFactory),
        PromptComposer::default(),
        Some(events.clone()),
        dir.clone(),
    )?;
    let session = {
        let mut hash = 0xcbf29ce484222325u64;
        for byte in dir.to_string_lossy().as_bytes() {
            hash = (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3);
        }
        std::path::PathBuf::from(
            std::env::var_os("HOME")
                .or_else(|| std::env::var_os("USERPROFILE"))
                .context("HOME or USERPROFILE must be set")?,
        )
        .join(".kroma/sessions")
        .join(format!("{hash:016x}.json"))
    };
    *events.archive.lock().unwrap() = Some(session);
    let outcome = tokio::spawn(async move { agent.run("test", &mut |_| {}).await }).await??;
    assert_eq!(outcome, "finished");
    assert_eq!(*events.observed.lock().unwrap(), vec![true]);
    let _ = std::fs::remove_dir_all(dir);
    Ok(())
}

#[tokio::test]
async fn oversized_extension_prompt_is_rejected_before_model_call() -> Result<()> {
    let dir = std::env::temp_dir().join(format!(
        "kroma-prompt-overflow-{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    ));
    std::fs::create_dir_all(&dir)?;
    let backend = Arc::new(FakeBackend {
        calls: AtomicUsize::new(0),
        tools_seen: Mutex::new(Vec::new()),
        system_prompts: Mutex::new(Vec::new()),
    });
    let agent = Agent::with_runtime(
        backend.clone(),
        ToolRegistry::builtin(),
        Arc::new(BuiltinContextFactory),
        PromptComposer::default().with_extra("X".repeat(600_000))?,
        None,
        dir.clone(),
    )?;
    let mut agent = agent;
    let error = agent
        .run("test prompt capacity", &mut |_| {})
        .await
        .unwrap_err();
    assert!(error.to_string().contains("context") || error.to_string().contains("budget"));
    assert_eq!(backend.calls.load(Ordering::SeqCst), 0);
    drop(agent);
    let _ = std::fs::remove_dir_all(dir);
    Ok(())
}

#[tokio::test]
async fn replacing_base_prompt_updates_existing_session_projection() -> Result<()> {
    let dir = std::env::temp_dir().join(format!(
        "kroma-prompt-replace-{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    ));
    std::fs::create_dir_all(&dir)?;
    let backend = Arc::new(FakeBackend {
        calls: AtomicUsize::new(0),
        tools_seen: Mutex::new(Vec::new()),
        system_prompts: Mutex::new(Vec::new()),
    });
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(FakeTool))?;
    let mut first = Agent::with_backends(backend, tools, dir.clone())?;
    first.run("first", &mut |_| {}).await?;
    drop(first);
    let backend = Arc::new(FakeBackend {
        calls: AtomicUsize::new(1),
        tools_seen: Mutex::new(Vec::new()),
        system_prompts: Mutex::new(Vec::new()),
    });
    let mut second = Agent::with_runtime(
        backend.clone(),
        ToolRegistry::builtin(),
        Arc::new(BuiltinContextFactory),
        PromptComposer::default().with_system_prompt("CUSTOM_SESSION_SYSTEM")?,
        None,
        dir.clone(),
    )?;
    second.run("second", &mut |_| {}).await?;
    assert!(backend.system_prompts.lock().unwrap()[0].starts_with("CUSTOM_SESSION_SYSTEM"));
    drop(second);
    let _ = std::fs::remove_dir_all(dir);
    Ok(())
}

#[test]
fn embedding_can_replace_base_prompt_without_affecting_default() -> Result<()> {
    let workdir = Path::new("/tmp/custom-kroma");
    let default = PromptComposer::default().initial_history(workdir);
    let custom = PromptComposer::default()
        .with_system_prompt("Custom agent using a registered tool.")?
        .with_extra("\nRepository rules: keep generated files unchanged.")?;
    let mut history = custom.initial_history(workdir);
    assert_eq!(
        history[0].content.as_deref(),
        Some("Custom agent using a registered tool.")
    );
    custom.apply(&mut history);
    custom.apply(&mut history);
    let prompt = history[0].content.as_deref().unwrap();
    assert_eq!(prompt.matches(EXECUTION_GUIDANCE).count(), 1);
    assert_eq!(prompt.matches("Repository rules:").count(), 1);
    assert!(default[0]
        .content
        .as_deref()
        .unwrap()
        .contains("only tool is shell"));
    Ok(())
}
