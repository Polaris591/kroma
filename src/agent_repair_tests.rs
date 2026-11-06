use super::*;
use crate::{
    context_state::SummarySegment,
    model::{ModelFuture, StreamFuture, TextResult, Usage},
    protocol::{FunctionCall, ToolCall},
    tools::{Tool, ToolFuture, ToolResult},
};
use serde_json::{json, Value};
use std::{
    fs,
    sync::{atomic::AtomicUsize, Mutex},
};
use tokio::sync::Notify;

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Result<Self> {
        let path = std::env::temp_dir().join(format!(
            "kroma-agent-repair-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos()
        ));
        fs::create_dir_all(&path)?;
        Ok(Self(path))
    }

    fn agent(
        &self,
        model: Arc<Backend>,
        events: Option<Arc<dyn EventSink>>,
        window: usize,
    ) -> Result<Agent> {
        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(Evidence))?;
        Agent::with_catalog_backends(
            model,
            tools,
            Arc::new(BuiltinContextFactory),
            PromptComposer::default(),
            events,
            self.0.clone(),
            SessionCatalog::from_default(self.0.join("default.json"))?,
            ContextConfig::with_output_reserve(window, 4096)?,
            false,
        )
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[derive(Default)]
struct Backend {
    calls: AtomicUsize,
    checkpoints: AtomicUsize,
    poison: Option<PathBuf>,
    hang_after: Option<usize>,
    native_search: bool,
    started: Notify,
    seen: Mutex<Vec<Message>>,
    seen_tools: Mutex<Vec<Value>>,
}
impl ModelBackend for Backend {
    fn complete<'a>(
        &'a self,
        messages: &'a [Message],
        tools: &'a [Value],
        _: &'a mut (dyn FnMut(&str) + Send),
    ) -> StreamFuture<'a, Completion> {
        Box::pin(async move {
            *self.seen_tools.lock().unwrap() = tools.to_vec();
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            if self.hang_after == Some(n) {
                self.started.notify_one();
                return std::future::pending().await;
            }
            *self.seen.lock().unwrap() = messages.to_vec();
            let mut message = Message::new("assistant", "ok");
            if self.poison.is_some() && n == 0 {
                message.tool_calls = Some(vec![ToolCall {
                    id: "evidence-1".into(),
                    kind: "function".into(),
                    function: FunctionCall {
                        name: "evidence".into(),
                        arguments: "{}".into(),
                    },
                }]);
            }
            Ok(Completion {
                message,
                usage: if self.poison.is_some() {
                    None
                } else {
                    Some(Usage {
                        input: Some(100),
                        output: Some(5),
                        cached: Some(50),
                        reasoning: Some(0),
                        ..Default::default()
                    })
                },
                elapsed: Duration::ZERO,
                first_token: None,
            })
        })
    }
    fn native_web_search(&self) -> bool {
        self.native_search
    }
    fn generate_text<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
        _: usize,
    ) -> ModelFuture<'a, TextResult> {
        Box::pin(async { bail!("unexpected text generation") })
    }
    fn generate_checkpoint<'a>(
        &'a self,
        _: &'a [Message],
        _: &'a [Value],
        _: &'a str,
        _: usize,
    ) -> ModelFuture<'a, TextResult> {
        Box::pin(async move {
            self.checkpoints.fetch_add(1, Ordering::SeqCst);
            let path = self.poison.as_ref().unwrap();
            fs::rename(path, path.with_extension("saved.json"))?;
            fs::create_dir(path)?;
            Ok(TextResult {
                text: "1. OBJECTIVE\nContinue the task after the tool result.\n2. CONSTRAINTS\nKeep the original request exact.\n3. PROGRESS\nThe first tool call completed and its evidence is retained.\n4. CODE STATE\nNo files changed yet.\n5. UNRESOLVED ISSUES\nNone known.\n6. KEY DECISIONS\nUse the retained tool evidence.\n7. NEXT ACTION\nProduce the final answer without another tool call.".into(),
                usage: None, elapsed: Duration::ZERO, requests: 1,
            })
        })
    }
}
struct WebSearch;
impl Tool for WebSearch {
    fn name(&self) -> &str {
        "web_search"
    }
    fn definition(&self) -> Value {
        json!({"type":"function","function":{"name":"web_search","parameters":{
            "type":"object","properties":{"query":{"type":"string"}}}}})
    }
    fn execute<'a>(&'a self, _: &'a ToolCall, _: ToolContext<'a>) -> ToolFuture<'a> {
        Box::pin(async { ToolResult::ok("results") })
    }
}

struct WebFetch;
impl Tool for WebFetch {
    fn name(&self) -> &str {
        "web_fetch"
    }
    fn definition(&self) -> Value {
        json!({"type":"function","function":{"name":"web_fetch","parameters":{
            "type":"object","properties":{"url":{"type":"string"}}}}})
    }
    fn execute<'a>(&'a self, _: &'a ToolCall, _: ToolContext<'a>) -> ToolFuture<'a> {
        Box::pin(async { ToolResult::ok("page") })
    }
}

struct Evidence;
impl Tool for Evidence {
    fn name(&self) -> &str {
        "evidence"
    }
    fn definition(&self) -> Value {
        json!({"type":"function","function":{"name":"evidence","parameters":{"type":"object","properties":{}}}})
    }
    fn execute<'a>(&'a self, _: &'a ToolCall, _: ToolContext<'a>) -> ToolFuture<'a> {
        Box::pin(async { ToolResult::ok("x".repeat(47_000)) })
    }
}

#[derive(Default)]
struct Events {
    labels: Mutex<Vec<String>>,
    steer: Mutex<Option<TaskInput>>,
    steering_accepted: Mutex<Option<bool>>,
}
impl EventSink for Events {
    fn on_event(&self, event: RuntimeEvent<'_>) {
        let label = match event {
            RuntimeEvent::Warning { .. } => "warning",
            RuntimeEvent::SessionSwitched { .. } => "switched",
            RuntimeEvent::FailedRequestUsage {
                input_tokens,
                output_tokens,
                cached_tokens,
                reasoning_tokens,
            } => {
                assert!(
                    input_tokens.is_none()
                        && output_tokens.is_none()
                        && cached_tokens.is_none()
                        && reasoning_tokens.is_none()
                );
                "unknown-request"
            }
            RuntimeEvent::ContextPrepared { .. } => {
                if let Some(input) = self.steer.lock().unwrap().as_ref() {
                    *self.steering_accepted.lock().unwrap() =
                        Some(input.steer("must not be discarded").is_ok());
                    assert!(
                        input.start().is_err(),
                        "compaction must still exclude another foreground task"
                    );
                }
                return;
            }
            _ => return,
        };
        self.labels.lock().unwrap().push(label.into());
    }
}

#[test]
fn navigation_succeeds_and_notifies_when_recent_sidecar_is_unwritable() -> Result<()> {
    let fixture = Fixture::new()?;
    let events = Arc::new(Events::default());
    let mut agent = fixture.agent(Arc::new(Backend::default()), Some(events.clone()), 128_000)?;
    fs::create_dir(fixture.0.join("default.last.json"))?;
    let new = agent.new_session()?;
    assert_eq!(agent.session_id(), new);
    let fork = agent.fork(Some(0))?;
    assert_eq!(agent.session_id(), fork);
    agent.resume_session("default")?;
    let labels = events.labels.lock().unwrap();
    assert_eq!(
        labels.iter().filter(|label| *label == "switched").count(),
        3
    );
    assert_eq!(labels.iter().filter(|label| *label == "warning").count(), 3);
    Ok(())
}

#[tokio::test]
async fn poisoned_compaction_stops_before_the_next_model_request() -> Result<()> {
    let fixture = Fixture::new()?;
    let model = Arc::new(Backend {
        poison: Some(fixture.0.join("default.json")),
        ..Default::default()
    });
    let mut agent = fixture.agent(model.clone(), None, 32_768)?;
    let error = agent
        .run(
            &format!(
                "Keep ORIGINAL_TASK and continue after evidence-1. {}",
                "context ".repeat(3_800)
            ),
            &mut |_| {},
        )
        .await
        .unwrap_err();
    assert_eq!(model.checkpoints.load(Ordering::SeqCst), 1);
    assert!(
        format!("{error:#}").contains("persistence previously failed"),
        "{error:#}"
    );
    assert_eq!(model.calls.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test]
async fn cancelled_model_request_is_unknown_and_counted_once() -> Result<()> {
    let fixture = Fixture::new()?;
    let model = Arc::new(Backend {
        hang_after: Some(1),
        ..Default::default()
    });
    let events = Arc::new(Events::default());
    let mut agent = fixture.agent(model.clone(), Some(events.clone()), 128_000)?;
    agent.run("warmup", &mut |_| {}).await?;
    let mut sink = |_: &str| {};
    let mut run = Box::pin(agent.run("cancel this", &mut sink));
    tokio::select! {
        ready = tokio::time::timeout(Duration::from_secs(2), model.started.notified()) => ready?,
        result = &mut run => panic!("unexpected completion: {result:?}"),
    }
    drop(run);
    agent.cancel()?;
    agent.cancel()?;
    assert_eq!(agent.usage().calls, 2);
    assert!(agent.usage().report().contains("+1 unknown request(s)"));
    assert_eq!(
        events
            .labels
            .lock()
            .unwrap()
            .iter()
            .filter(|label| *label == "unknown-request")
            .count(),
        1
    );
    Ok(())
}

#[tokio::test]
async fn manual_compaction_excludes_tasks_but_rejects_steering() -> Result<()> {
    let fixture = Fixture::new()?;
    let events = Arc::new(Events::default());
    let mut agent = fixture.agent(Arc::new(Backend::default()), Some(events.clone()), 128_000)?;
    *events.steer.lock().unwrap() = Some(agent.task_input());
    agent.compact().await?;
    assert_eq!(*events.steering_accepted.lock().unwrap(), Some(false));
    assert_eq!(agent.task_input().pending_steering(), 0);
    assert_eq!(agent.messages().len(), 1);
    let active = agent.task_input.start()?;
    agent
        .task_input
        .steer("ordinary tasks still accept steering")?;
    drop(active);
    Ok(())
}

#[tokio::test]
async fn fork_and_rewind_migrate_summary_captures_before_source_clear() -> Result<()> {
    for rewind in [false, true] {
        let fixture = Fixture::new()?;
        let model = Arc::new(Backend::default());
        let mut agent = fixture.agent(model.clone(), None, 128_000)?;
        let original = fixture.0.join("mcp-image-default-1-2.png");
        fs::write(&original, b"evidence")?;
        let mut call_message = Message::new("assistant", "collect evidence");
        call_message.tool_calls = Some(vec![ToolCall {
            id: "capture".into(),
            kind: "function".into(),
            function: FunctionCall {
                name: "evidence".into(),
                arguments: "{}".into(),
            },
        }]);
        agent.messages = vec![
            agent.messages[0].clone(),
            Message::new("user", "collect evidence"),
            call_message,
            Message::tool(
                "capture".into(),
                json!({"content":[{"type":"image_reference","path":original}]}).to_string(),
            ),
            Message::new("assistant", "captured"),
            Message::new("user", "another turn"),
            Message::new("assistant", "done"),
        ];
        let state = CompactionState {
            covered_until: 5,
            consumed_until: 7,
            revision: 1,
            summaries: vec![SummarySegment {
                until: 5,
                text: format!(
                    "Evidence: {}. JSON: {}",
                    original.display(),
                    serde_json::to_string(&original)?
                ),
            }],
            ..Default::default()
        };
        state.validate(&agent.messages)?;
        agent.session.reset(&agent.messages, &state)?;
        agent.compression = agent.context_factory.restore(
            state.into(),
            agent.context_budget,
            agent.archive_path(),
        )?;
        let branch = if rewind {
            agent.rewind(1)?
        } else {
            agent.fork(None)?
        };
        agent.resume_session(&branch)?;
        let result: Value = serde_json::from_str(agent.messages()[3].content.as_deref().unwrap())?;
        let copied = PathBuf::from(result["content"][0]["path"].as_str().unwrap());
        assert_ne!(copied, original);
        assert_eq!(fs::read(&copied)?, b"evidence");
        agent.resume_session("default")?;
        agent.reset()?;
        assert!(!original.exists());
        assert!(copied.exists());
        agent.resume_session(&branch)?;
        let PersistedContext::Builtin(state) = agent.compression.state() else {
            unreachable!()
        };
        assert_eq!(state.revision, 2);
        assert!(state.summaries[0].text.contains(copied.to_str().unwrap()));
        assert!(state.summaries[0]
            .text
            .contains(&serde_json::to_string(&copied)?));
        assert!(!state.summaries[0].text.contains(original.to_str().unwrap()));
        agent.run("read earlier evidence", &mut |_| {}).await?;
        let projected = model
            .seen
            .lock()
            .unwrap()
            .iter()
            .filter_map(|m| m.content.as_deref())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(projected.contains(copied.to_str().unwrap()));
        assert!(!projected.contains(original.to_str().unwrap()));
    }
    Ok(())
}

#[tokio::test]
async fn native_server_search_suppresses_the_duplicate_local_tool() -> Result<()> {
    for (native_search, offered) in [(true, false), (false, true)] {
        let fixture = Fixture::new()?;
        let model = Arc::new(Backend {
            native_search,
            ..Default::default()
        });
        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(WebSearch))?;
        tools.register(Arc::new(WebFetch))?;
        let mut agent = Agent::with_catalog_backends(
            model.clone(),
            tools,
            Arc::new(BuiltinContextFactory),
            PromptComposer::default(),
            None,
            fixture.0.clone(),
            SessionCatalog::from_default(fixture.0.join("default.json"))?,
            ContextConfig::with_output_reserve(128_000, 4096)?,
            false,
        )?;
        agent.run("search the web", &mut |_| {}).await?;
        let seen = model.seen_tools.lock().unwrap();
        let names = seen
            .iter()
            .filter_map(|tool| tool["function"]["name"].as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            names.contains(&"web_search"),
            offered,
            "native_search={native_search}: {names:?}"
        );
        assert!(
            names.contains(&"web_fetch"),
            "page reading must stay available: {names:?}"
        );
    }
    Ok(())
}
