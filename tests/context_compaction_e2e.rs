//! End-to-end regression: one lazy checkpoint must not interrupt tool work.
use anyhow::Result;
use kroma::{
    agent::Agent,
    context::{BuiltinContextFactory, ContextConfig},
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

const CHECKPOINT: &str = "1. OBJECTIVE\nContinue the task after the tool result.\n\
2. CONSTRAINTS\nKeep the original request exact.\n\
3. PROGRESS\nThe first tool call completed and its evidence is retained.\n\
4. CODE STATE\nNo files changed yet.\n\
5. UNRESOLVED ISSUES\nNone known.\n\
6. KEY DECISIONS\nUse the retained tool evidence.\n\
7. NEXT ACTION\nProduce the final answer without another tool call.";

struct HugeOutput {
    bytes: usize,
}

impl Tool for HugeOutput {
    fn name(&self) -> &str {
        "huge_output"
    }

    fn definition(&self) -> Value {
        json!({"type":"function","function":{
            "name":"huge_output","description":"return a large evidence blob",
            "parameters":{"type":"object","properties":{}}
        }})
    }

    fn execute<'a>(&'a self, _: &'a ToolCall, _: ToolContext<'a>) -> ToolFuture<'a> {
        Box::pin(async move { ToolResult::ok("E".repeat(self.bytes)) })
    }
}

struct ScriptedModel {
    completions: AtomicUsize,
    checkpoints: AtomicUsize,
    fail_checkpoints: bool,
    seen: Mutex<Vec<Vec<Message>>>,
}

impl ScriptedModel {
    fn new() -> Self {
        Self {
            completions: AtomicUsize::new(0),
            checkpoints: AtomicUsize::new(0),
            fail_checkpoints: false,
            seen: Mutex::new(Vec::new()),
        }
    }

    fn with_checkpoint_failure() -> Self {
        Self {
            fail_checkpoints: true,
            ..Self::new()
        }
    }
}

impl ModelBackend for ScriptedModel {
    fn complete<'a>(
        &'a self,
        messages: &'a [Message],
        _: &'a [Value],
        on_text: &'a mut (dyn FnMut(&str) + Send),
    ) -> StreamFuture<'a, Completion> {
        Box::pin(async move {
            self.seen.lock().unwrap().push(messages.to_vec());
            let call = self.completions.fetch_add(1, Ordering::SeqCst);
            let message = if call == 0 {
                Message {
                    role: "assistant".into(),
                    content: Some("collecting evidence".into()),
                    reasoning_content: None,
                    tool_calls: Some(vec![ToolCall {
                        id: "evidence-1".into(),
                        kind: "function".into(),
                        function: FunctionCall {
                            name: "huge_output".into(),
                            arguments: "{}".into(),
                        },
                    }]),
                    tool_call_id: None,
                    provider_items: None,
                }
            } else {
                on_text("finished after compaction");
                Message::new("assistant", "finished after compaction")
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
        Box::pin(async { anyhow::bail!("generic text generation is not expected") })
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
            if self.fail_checkpoints {
                anyhow::bail!("checkpoint provider unavailable");
            }
            Ok(TextResult {
                text: CHECKPOINT.into(),
                usage: None,
                elapsed: Duration::ZERO,
                requests: 1,
            })
        })
    }
}

fn isolated_dir() -> Result<PathBuf> {
    static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "kroma-compaction-e2e-{}-{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos(),
        NEXT_DIR.fetch_add(1, Ordering::Relaxed),
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

#[tokio::test]
async fn one_checkpoint_then_tool_work_continues_and_raw_wal_survives() -> Result<()> {
    let dir = isolated_dir()?;
    let model = Arc::new(ScriptedModel::new());
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(HugeOutput { bytes: 48_000 }))?;
    let budget = ContextConfig::with_output_reserve(32_768, 4_096)?;
    let mut agent = Agent::with_runtime_budget(
        model.clone(),
        tools,
        Arc::new(BuiltinContextFactory),
        PromptComposer::default(),
        None,
        dir.clone(),
        budget,
    )?;
    let task = format!(
        "Keep ORIGINAL_TASK and continue after evidence-1. {}",
        "context ".repeat(4_000)
    );
    let answer = agent.run(&task, &mut |_| {}).await?;
    assert_eq!(answer, "finished after compaction");
    assert_eq!(model.checkpoints.load(Ordering::SeqCst), 1);
    assert_eq!(model.completions.load(Ordering::SeqCst), 2);

    let seen = model.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 2);
    let after = &seen[1];
    assert!(
        after.iter().any(|message| message
            .content
            .as_deref()
            .is_some_and(|text| text.contains("Compacted conversation checkpoint"))),
        "the second model request must use the committed checkpoint"
    );
    assert!(after.iter().any(|message| message.role == "assistant"
        && message
            .tool_calls
            .as_ref()
            .is_some_and(|calls| calls.iter().any(|call| call.id == "evidence-1"))));
    assert!(after.iter().any(|message| message.role == "tool"
        && message.tool_call_id.as_deref() == Some("evidence-1")
        && message
            .content
            .as_deref()
            .is_some_and(|text| text.len() == 48_000)));

    let snapshot_path = SessionCatalog::new(&dir)?.default_path().to_path_buf();
    let snapshot: Value = serde_json::from_slice(&std::fs::read(&snapshot_path)?)?;
    let saved: Vec<Message> = serde_json::from_value(snapshot["messages"].clone())?;
    assert!(saved.iter().any(|message| message
        .content
        .as_deref()
        .is_some_and(|text| text.contains("ORIGINAL_TASK"))));
    assert!(saved.iter().any(|message| message.role == "tool"
        && message
            .content
            .as_deref()
            .is_some_and(|text| text.len() == 48_000)));
    assert_eq!(
        snapshot["compression"]["summaries"]
            .as_array()
            .map(Vec::len),
        Some(1)
    );
    drop(agent);
    remove_session(&dir);
    Ok(())
}

#[tokio::test]
async fn checkpoint_failure_below_hard_limit_does_not_abort_the_task() -> Result<()> {
    let dir = isolated_dir()?;
    let model = Arc::new(ScriptedModel::with_checkpoint_failure());
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(HugeOutput { bytes: 48_000 }))?;
    let budget = ContextConfig::with_output_reserve(32_768, 4_096)?;
    let mut agent = Agent::with_runtime_budget(
        model.clone(),
        tools,
        Arc::new(BuiltinContextFactory),
        PromptComposer::default(),
        None,
        dir.clone(),
        budget,
    )?;
    // Large enough to cross the 90%-of-window trigger but still below the hard limit.
    let task = format!(
        "Keep ORIGINAL_TASK and continue after evidence-1. {}",
        "context ".repeat(4_000)
    );
    let answer = agent.run(&task, &mut |_| {}).await?;
    assert_eq!(answer, "finished after compaction");
    assert_eq!(
        model.checkpoints.load(Ordering::SeqCst),
        1,
        "one failed checkpoint attempt is bounded"
    );
    assert_eq!(model.completions.load(Ordering::SeqCst), 2);

    let seen = model.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 2);
    assert!(
        !seen[1].iter().any(|message| message
            .content
            .as_deref()
            .is_some_and(|text| text.contains("Compacted conversation checkpoint"))),
        "the failed checkpoint must not change the model projection"
    );
    assert!(
        seen[1].iter().any(|message| message.role == "tool"
            && message
                .content
                .as_deref()
                .is_some_and(|text| text.len() == 48_000)),
        "the full tool evidence must remain available for the current task"
    );

    let saved = agent.messages().to_vec();
    assert!(saved.iter().any(|message| message
        .content
        .as_deref()
        .is_some_and(|text| text.contains("ORIGINAL_TASK"))));
    assert!(saved.iter().any(|message| message.role == "tool"
        && message
            .content
            .as_deref()
            .is_some_and(|text| text.len() == 48_000)));
    let wal = std::fs::read_to_string(format!("{}.wal", agent.archive_path().display()))?;
    assert!(wal.contains("ORIGINAL_TASK"));
    drop(agent);
    remove_session(&dir);
    Ok(())
}

#[tokio::test]
async fn checkpoint_failure_at_hard_limit_uses_preview_and_continues() -> Result<()> {
    let dir = isolated_dir()?;
    let model = Arc::new(ScriptedModel::with_checkpoint_failure());
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(HugeOutput { bytes: 70_000 }))?;
    let budget = ContextConfig::with_output_reserve(32_768, 4_096)?;
    let mut agent = Agent::with_runtime_budget(
        model.clone(),
        tools,
        Arc::new(BuiltinContextFactory),
        PromptComposer::default(),
        None,
        dir.clone(),
        budget,
    )?;
    let task = format!(
        "Keep ORIGINAL_TASK and continue after evidence-1. {}",
        "context ".repeat(4_000)
    );
    let answer = agent.run(&task, &mut |_| {}).await?;
    assert_eq!(answer, "finished after compaction");
    assert_eq!(model.checkpoints.load(Ordering::SeqCst), 1);
    assert_eq!(model.completions.load(Ordering::SeqCst), 2);

    let seen = model.seen.lock().unwrap().clone();
    let after = &seen[1];
    assert!(after.iter().any(|message| message.role == "tool"
        && message
            .content
            .as_deref()
            .is_some_and(|text| text.contains("kroma_context_overflow"))));
    assert!(after.iter().any(|message| message.role == "tool"
        && message
            .content
            .as_deref()
            .is_some_and(|text| text.len() < 10_000)));

    let saved = agent.messages().to_vec();
    assert!(saved.iter().any(|message| message.role == "tool"
        && message
            .content
            .as_deref()
            .is_some_and(|text| text.len() == 70_000)));
    let wal = std::fs::read_to_string(format!("{}.wal", agent.archive_path().display()))?;
    assert!(wal.contains("ORIGINAL_TASK"));
    assert!(wal.len() > 70_000);
    drop(agent);
    remove_session(&dir);
    Ok(())
}
