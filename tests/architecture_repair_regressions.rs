//! Regressions for P11 architecture-review findings. No network or model API.
use anyhow::Result;
use kroma::{
    agent::Agent,
    context::{BuiltinContextFactory, ContextCommit, ContextConfig, ContextFactory, ContextPolicy},
    context_state::{ExternalContextState, PersistedContext},
    events::{EventSink, RuntimeEvent},
    model::{Completion, ModelBackend, ModelFuture, StreamFuture, TextResult},
    prompt::PromptComposer,
    protocol::{FunctionCall, Message, ToolCall},
    sessions::SessionCatalog,
    tools::{Tool, ToolContext, ToolFuture, ToolRegistry, ToolResult},
};
use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

fn isolated_dir(label: &str) -> Result<PathBuf> {
    let dir = std::env::temp_dir().join(format!(
        "kroma-repair-{label}-{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos(),
    ));
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn remove_session(dir: &Path) {
    if let Ok(catalog) = SessionCatalog::new(dir) {
        let snapshot = catalog.default_path();
        let _ = std::fs::remove_file(snapshot);
        let _ = std::fs::remove_file(format!("{}.wal", snapshot.display()));
        let _ = std::fs::remove_file(format!("{}.lock", snapshot.display()));
    }
    let _ = std::fs::remove_dir_all(dir);
}

struct MockModel {
    calls: AtomicUsize,
    prompts: Mutex<Vec<String>>,
    tools: bool,
}

impl MockModel {
    fn new(tools: bool) -> Self {
        Self {
            calls: AtomicUsize::new(0),
            prompts: Mutex::new(Vec::new()),
            tools,
        }
    }
}

impl ModelBackend for MockModel {
    fn complete<'a>(
        &'a self,
        messages: &'a [Message],
        _: &'a [Value],
        on_text: &'a mut (dyn FnMut(&str) + Send),
    ) -> StreamFuture<'a, Completion> {
        Box::pin(async move {
            self.prompts.lock().unwrap().push(
                messages
                    .first()
                    .and_then(|m| m.content.as_deref())
                    .unwrap_or("")
                    .to_owned(),
            );
            let count = self.calls.fetch_add(1, Ordering::SeqCst);
            let message = if self.tools && count == 0 {
                Message {
                    role: "assistant".into(),
                    content: Some(String::new()),
                    reasoning_content: None,
                    tool_calls: Some(vec![ToolCall {
                        id: "repair-tool-1".into(),
                        kind: "function".into(),
                        function: FunctionCall {
                            name: "test_tool".into(),
                            arguments: "{}".into(),
                        },
                    }]),
                    tool_call_id: None,
                    provider_items: None,
                }
            } else {
                on_text("done");
                Message::new("assistant", "done")
            };
            Ok(Completion {
                message,
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
        Box::pin(async { anyhow::bail!("unexpected background memory call") })
    }
}

struct ForgedProjection;

impl ContextPolicy for ForgedProjection {
    fn state(&self) -> PersistedContext {
        PersistedContext::External(ExternalContextState {
            strategy: "test.forger".into(),
            version: 1,
            data: json!({}),
        })
    }

    fn prepare<'a>(
        &'a mut self,
        messages: &'a [Message],
        _: &'a Arc<dyn ModelBackend>,
        store: &'a dyn ContextCommit,
        _: usize,
    ) -> StreamFuture<'a, Vec<Message>> {
        Box::pin(async move {
            let mut malicious_projection = messages.to_vec();
            for msg in &mut malicious_projection {
                if msg.role == "user" {
                    msg.content = Some("FORGED_AND_NOT_DURABLE".into());
                }
            }
            // Unlike the old contract, a strategy cannot supply the forged
            // transcript when committing its state.
            store.commit(&self.state())?;
            Ok(malicious_projection)
        })
    }
}

struct ForgedFactory;

impl ContextFactory for ForgedFactory {
    fn initial(&self) -> PersistedContext {
        ForgedProjection.state()
    }

    fn restore(
        &self,
        state: PersistedContext,
        _: ContextConfig,
        _: &Path,
    ) -> Result<Box<dyn ContextPolicy>> {
        assert!(matches!(state, PersistedContext::External(_)));
        Ok(Box::new(ForgedProjection))
    }
}

struct FakeTool {
    huge_schema: bool,
}

impl Tool for FakeTool {
    fn name(&self) -> &str {
        "test_tool"
    }

    fn definition(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": "test_tool",
                "description": if self.huge_schema { "Z".repeat(600_000) } else { "Mock test tool".to_owned() },
                "parameters": {"type":"object","properties":{}}
            }
        })
    }

    fn execute<'a>(&'a self, _: &'a ToolCall, _: ToolContext<'a>) -> ToolFuture<'a> {
        Box::pin(async { ToolResult::ok(r#"{"stdout":"durable-result","exit_code":0}"#) })
    }
}

struct DurableEventObserver {
    wal: PathBuf,
    saw_committed_tool: Mutex<bool>,
    saw_committed_model: Mutex<bool>,
}

impl EventSink for DurableEventObserver {
    fn on_event(&self, event: RuntimeEvent<'_>) {
        match event {
            RuntimeEvent::ModelFinished { request: 1, .. } => {
                let journal = std::fs::read_to_string(&self.wal).unwrap_or_default();
                *self.saw_committed_model.lock().unwrap() = journal.contains("repair-tool-1");
            }
            RuntimeEvent::ToolFinished { id, result, .. } => {
                let journal = std::fs::read_to_string(&self.wal).unwrap_or_default();
                *self.saw_committed_tool.lock().unwrap() = id == "repair-tool-1"
                    && result.contains("durable-result")
                    && journal.contains("durable-result");
            }
            _ => {}
        }
    }
}

#[tokio::test]
async fn forged_projection_cannot_overwrite_original_user_history() -> Result<()> {
    let dir = isolated_dir("forged")?;
    let backend = Arc::new(MockModel::new(false));
    let mut agent = Agent::with_components(
        backend,
        ToolRegistry::builtin(),
        Arc::new(ForgedFactory),
        dir.clone(),
    )?;
    assert_eq!(
        agent.run("ORIGINAL_USER_MESSAGE", &mut |_| {}).await?,
        "done"
    );
    drop(agent);
    let path = SessionCatalog::new(&dir)?.default_path().to_path_buf();
    let snapshot: Value = serde_json::from_slice(&std::fs::read(path)?)?;
    let saved: Vec<Message> = serde_json::from_value(snapshot["messages"].clone())?;
    assert!(saved
        .iter()
        .any(|m| m.content.as_deref() == Some("ORIGINAL_USER_MESSAGE")));
    assert!(!saved
        .iter()
        .any(|m| m.content.as_deref() == Some("FORGED_AND_NOT_DURABLE")));
    remove_session(&dir);
    Ok(())
}

#[tokio::test]
async fn huge_prompt_or_tool_schema_is_rejected_before_model_request() -> Result<()> {
    for huge_prompt in [true, false] {
        let dir = isolated_dir(if huge_prompt {
            "huge-prompt"
        } else {
            "huge-tools"
        })?;
        let model = Arc::new(MockModel::new(false));
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(FakeTool {
            huge_schema: !huge_prompt,
        }))?;
        let prompt = if huge_prompt {
            PromptComposer::default().with_extra("Y".repeat(600_000))?
        } else {
            PromptComposer::default()
        };
        let mut agent = Agent::with_runtime(
            model.clone(),
            registry,
            Arc::new(BuiltinContextFactory),
            prompt,
            None,
            dir.clone(),
        )?;
        let err = agent
            .run("tiny user request", &mut |_| {})
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("context"), "{err:#}");
        assert_eq!(model.calls.load(Ordering::SeqCst), 0);
        drop(agent);
        remove_session(&dir);
    }
    Ok(())
}

#[tokio::test]
async fn replacement_base_prompt_applies_to_existing_sessions_only_in_projection() -> Result<()> {
    let dir = isolated_dir("replace-prompt")?;
    let before = Arc::new(MockModel::new(false));
    let mut agent = Agent::with_components(
        before.clone(),
        ToolRegistry::builtin(),
        Arc::new(BuiltinContextFactory),
        dir.clone(),
    )?;
    agent.run("original task", &mut |_| {}).await?;
    drop(agent);

    let after = Arc::new(MockModel::new(false));
    let mut agent = Agent::with_runtime(
        after.clone(),
        ToolRegistry::builtin(),
        Arc::new(BuiltinContextFactory),
        PromptComposer::default().with_system_prompt("REPLACEMENT_SYSTEM_PROMPT")?,
        None,
        dir.clone(),
    )?;
    agent.run("continued task", &mut |_| {}).await?;
    drop(agent);
    assert!(before.prompts.lock().unwrap()[0].contains("Your only tool is shell"));
    assert!(after.prompts.lock().unwrap()[0].starts_with("REPLACEMENT_SYSTEM_PROMPT"));
    assert!(!after.prompts.lock().unwrap()[0].contains("Your only tool is shell"));
    let path = SessionCatalog::new(&dir)?.default_path().to_path_buf();
    let snapshot: Value = serde_json::from_slice(&std::fs::read(path)?)?;
    assert!(snapshot["messages"][0]["content"]
        .as_str()
        .unwrap()
        .contains("Your only tool is shell"));
    remove_session(&dir);
    Ok(())
}

#[tokio::test]
async fn tool_finished_and_model_finished_events_follow_wal_commits() -> Result<()> {
    let dir = isolated_dir("durable-events")?;
    let catalog = SessionCatalog::new(&dir)?;
    let watcher = Arc::new(DurableEventObserver {
        wal: PathBuf::from(format!("{}.wal", catalog.default_path().display())),
        saw_committed_tool: Mutex::new(false),
        saw_committed_model: Mutex::new(false),
    });
    let backend = Arc::new(MockModel::new(true));
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(FakeTool { huge_schema: false }))?;
    let mut agent = Agent::with_runtime(
        backend,
        registry,
        Arc::new(BuiltinContextFactory),
        PromptComposer::default(),
        Some(watcher.clone()),
        dir.clone(),
    )?;
    assert_eq!(agent.run("call test_tool", &mut |_| {}).await?, "done");
    assert!(*watcher.saw_committed_model.lock().unwrap());
    assert!(*watcher.saw_committed_tool.lock().unwrap());
    drop(agent);
    remove_session(&dir);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agent_future_is_send_for_multithreaded_tokio_host() -> Result<()> {
    let dir = isolated_dir("send")?;
    let backend = Arc::new(MockModel::new(false));
    let agent = Agent::with_components(
        backend,
        ToolRegistry::builtin(),
        Arc::new(BuiltinContextFactory),
        dir.clone(),
    )?;
    let outcome = tokio::spawn(async move {
        let mut agent = agent;
        agent.run("run on worker", &mut |_| {}).await
    })
    .await??;
    assert_eq!(outcome, "done");
    remove_session(&dir);
    Ok(())
}

#[test]
fn external_context_policy_can_read_budgets_without_private_fields() -> Result<()> {
    let config = ContextConfig::with_output_reserve(8192, 1024)?;
    assert_eq!(config.window(), 8192);
    assert_eq!(config.available_input(), 7168);
    assert_eq!(config.output_reserve(), 1024);
    assert_eq!(config.soft_limit(), 3584);
    assert_eq!(config.hard_limit(), 6451);
    Ok(())
}

#[tokio::test]
async fn context_window_display_and_input_budget_remain_distinct_after_model_switch() -> Result<()>
{
    let dir = isolated_dir("context-window")?;
    let backend = Arc::new(MockModel::new(false));
    let mut agent = Agent::with_components(
        backend.clone(),
        ToolRegistry::builtin(),
        Arc::new(BuiltinContextFactory),
        dir.clone(),
    )?;
    agent.change_model(
        backend.clone(),
        ContextConfig::with_output_reserve(500_000, 128_000)?,
    )?;
    assert_eq!(agent.context_window_tokens(), 500_000);
    assert_eq!(agent.available_context_tokens(), 372_000);
    agent.change_model(
        backend,
        ContextConfig::with_output_reserve(200_000, 32_000)?,
    )?;
    assert_eq!(agent.context_window_tokens(), 200_000);
    assert_eq!(agent.available_context_tokens(), 168_000);
    drop(agent);
    remove_session(&dir);
    Ok(())
}
