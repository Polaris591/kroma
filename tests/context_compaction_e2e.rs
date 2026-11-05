//! End-to-end regression: one lazy checkpoint must not interrupt tool work.
use anyhow::Result;
use kroma::{
    agent::Agent,
    context::{BuiltinContextFactory, ContextConfig},
    model::{Completion, ModelBackend, ModelFuture, StreamFuture, TextResult, Usage},
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
    checkpoint_delay: Duration,
    usage_divisor: Option<usize>,
    seen: Mutex<Vec<Vec<Message>>>,
}

impl ScriptedModel {
    fn new() -> Self {
        Self {
            completions: AtomicUsize::new(0),
            checkpoints: AtomicUsize::new(0),
            fail_checkpoints: false,
            checkpoint_delay: Duration::ZERO,
            usage_divisor: None,
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
                usage: self.usage_divisor.map(|divisor| Usage {
                    input: Some(
                        (serde_json::to_vec(messages).unwrap().len() / divisor).max(1) as u64,
                    ),
                    cached: Some(0),
                    ..Usage::default()
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
            if !self.checkpoint_delay.is_zero() {
                tokio::time::sleep(self.checkpoint_delay).await;
            }
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

fn manual_agent(model: Arc<ScriptedModel>, dir: &Path) -> Result<Agent> {
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(HugeOutput { bytes: 48_000 }))?;
    Agent::with_runtime_budget(
        model,
        tools,
        Arc::new(BuiltinContextFactory),
        PromptComposer::default(),
        None,
        dir.to_path_buf(),
        ContextConfig::with_output_reserve(500_000, 4_096)?,
    )
}

async fn completed_manual_history(agent: &mut Agent) -> Result<()> {
    agent
        .run(
            &format!("ORIGINAL_TASK {}", "context ".repeat(6_000)),
            &mut |_| {},
        )
        .await?;
    Ok(())
}

#[tokio::test]
async fn compact_command_bypasses_threshold_preserves_history_and_survives_restart() -> Result<()> {
    let dir = isolated_dir()?;
    let model = Arc::new(ScriptedModel::new());
    let mut agent = manual_agent(model.clone(), &dir)?;
    let commands = kroma::commands::CommandRegistry::builtin(Vec::new())?;
    let input = agent.task_input();

    let no_op = commands
        .execute_async("/compact", &mut agent, &input)
        .await?
        .unwrap();
    assert!(no_op.output.contains("Nothing to compact"));
    assert!(commands
        .execute_async("/compact unexpected", &mut agent, &input)
        .await
        .is_err());
    assert_eq!(model.checkpoints.load(Ordering::SeqCst), 0);

    completed_manual_history(&mut agent).await?;
    let original = serde_json::to_vec(agent.messages())?;
    assert_eq!(
        model.checkpoints.load(Ordering::SeqCst),
        0,
        "history is below automatic threshold"
    );
    let reply = commands
        .execute_async("/compact", &mut agent, &input)
        .await?
        .unwrap();
    assert!(reply.output.contains("Context compacted:"));
    assert!(reply.output.contains("estimated"));
    let counts = reply
        .output
        .split_whitespace()
        .filter_map(|word| word.trim_start_matches('~').parse::<usize>().ok())
        .collect::<Vec<_>>();
    assert_eq!(counts.len(), 2);
    assert!(counts[0] < 450_000);
    assert!(counts[1] < counts[0]);
    assert_eq!(model.checkpoints.load(Ordering::SeqCst), 1);
    assert_eq!(
        model.completions.load(Ordering::SeqCst),
        2,
        "manual compact must not run the coding model or tools"
    );
    assert_eq!(
        serde_json::to_vec(agent.messages())?,
        original,
        "no /compact user turn or raw history edits"
    );
    let path = agent.archive_path().to_path_buf();
    let snapshot: Value = serde_json::from_slice(&std::fs::read(&path)?)?;
    assert_eq!(
        snapshot["compression"]["summaries"]
            .as_array()
            .map(Vec::len),
        Some(1)
    );
    drop(agent);

    let mut resumed = manual_agent(model.clone(), &dir)?;
    assert_eq!(serde_json::to_vec(resumed.messages())?, original);
    resumed
        .run("continue after manual checkpoint", &mut |_| {})
        .await?;
    assert_eq!(
        model.checkpoints.load(Ordering::SeqCst),
        1,
        "restart must reuse saved checkpoint"
    );
    assert!(model.seen.lock().unwrap().last().unwrap().iter().any(|m| m
        .content
        .as_deref()
        .is_some_and(|s| s.contains("Compacted conversation checkpoint"))));
    drop(resumed);
    remove_session(&dir);
    Ok(())
}

#[tokio::test]
async fn manual_compact_failure_preserves_raw_history_and_context() -> Result<()> {
    let dir = isolated_dir()?;
    let model = Arc::new(ScriptedModel::with_checkpoint_failure());
    let mut agent = manual_agent(model.clone(), &dir)?;
    completed_manual_history(&mut agent).await?;
    let original = serde_json::to_vec(agent.messages())?;
    let error = agent.compact().await.unwrap_err();
    assert!(error
        .to_string()
        .contains("checkpoint provider unavailable"));
    assert_eq!(serde_json::to_vec(agent.messages())?, original);
    drop(agent);
    let resumed = manual_agent(model.clone(), &dir)?;
    assert_eq!(serde_json::to_vec(resumed.messages())?, original);
    let wal = std::fs::read_to_string(format!("{}.wal", resumed.archive_path().display()))?;
    assert!(!wal.contains("Compacted conversation checkpoint"));
    assert_eq!(model.checkpoints.load(Ordering::SeqCst), 1);
    drop(resumed);
    remove_session(&dir);
    Ok(())
}

#[tokio::test]
async fn cancelled_manual_compact_leaves_session_usable_without_replaying_tools() -> Result<()> {
    let dir = isolated_dir()?;
    let model = Arc::new(ScriptedModel {
        checkpoint_delay: Duration::from_secs(60),
        ..ScriptedModel::new()
    });
    let mut agent = manual_agent(model.clone(), &dir)?;
    completed_manual_history(&mut agent).await?;
    let original = serde_json::to_vec(agent.messages())?;
    assert!(
        tokio::time::timeout(Duration::from_millis(30), agent.compact())
            .await
            .is_err()
    );
    assert_eq!(model.checkpoints.load(Ordering::SeqCst), 1);
    assert_eq!(serde_json::to_vec(agent.messages())?, original);
    agent
        .run("continue after cancellation", &mut |_| {})
        .await?;
    assert_eq!(model.completions.load(Ordering::SeqCst), 3);
    assert_eq!(model.checkpoints.load(Ordering::SeqCst), 1);
    drop(agent);
    remove_session(&dir);
    Ok(())
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
        "context ".repeat(3_800)
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
    // Keep this fixture genuinely below the hard limit with the full built-in
    // V7 prompt included; 48k crosses the current 32k-window hard limit.
    tools.register(Arc::new(HugeOutput { bytes: 47_000 }))?;
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
    // Large enough to cross the 90%-of-window trigger but still below the hard
    // limit. The built-in V7 guidance adds ~200 tokens, so keep ~400 tokens of
    // headroom instead of sitting one prompt addition away from the limit.
    let task = format!(
        "Keep ORIGINAL_TASK and continue after evidence-1. {}",
        "context ".repeat(3_800)
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
                .is_some_and(|text| text.len() == 47_000)),
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
            .is_some_and(|text| text.len() == 47_000)));
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
        "context ".repeat(3_800)
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

#[tokio::test]
async fn calibrated_requests_cross_json_ceiling_without_compaction_or_hook_rejection() -> Result<()>
{
    for with_hook in [false, true] {
        if with_hook
            && std::process::Command::new("node")
                .arg("--version")
                .output()
                .is_err()
        {
            continue;
        }
        let dir = isolated_dir()?;
        let model = Arc::new(ScriptedModel {
            usage_divisor: Some(12),
            ..ScriptedModel::with_checkpoint_failure()
        });
        let budget = ContextConfig::with_output_reserve(32_768, 4_096)?;
        let backend: Arc<dyn ModelBackend> = if with_hook {
            let plugins = dir.join("plugins");
            let plugin = plugins.join("passthrough");
            std::fs::create_dir_all(&plugin)?;
            std::fs::write(
                plugin.join("plugin.toml"),
                "id='passthrough'
name='No-op'
command='node'
args=['main.js']
hooks=['before_model_request']
",
            )?;
            std::fs::write(
                plugin.join("main.js"),
                r#"require('node:readline').createInterface({input:process.stdin}).on('line',()=>process.stdout.write(JSON.stringify({content:'{}'})+String.fromCharCode(10)));"#,
            )?;
            let state = dir.join("plugins.toml");
            kroma::plugins::PluginCatalog::discover_at(plugins.clone(), state.clone(), &dir)?
                .set_enabled("passthrough", true)?;
            kroma::plugins::PluginCatalog::discover_at(plugins, state, &dir)?
                .model_hooks(model.clone(), budget)
        } else {
            model.clone()
        };
        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(HugeOutput { bytes: 0 }))?;
        let mut agent = Agent::with_runtime_budget(
            backend,
            tools,
            Arc::new(BuiltinContextFactory),
            PromptComposer::default(),
            None,
            dir.clone(),
            budget,
        )?;
        for index in 0..12 {
            agent
                .run(&format!("turn {index} {}", "R".repeat(9_000)), &mut |_| {})
                .await?;
        }
        let seen = model.seen.lock().unwrap();
        assert!(
            budget
                .validate_final_request(seen.last().unwrap(), &[])
                .is_err(),
            "fixture must exceed the uncalibrated JSON ceiling"
        );
        assert_eq!(
            model.checkpoints.load(Ordering::SeqCst),
            0,
            "with_hook={with_hook}"
        );
        assert_eq!(
            model.completions.load(Ordering::SeqCst),
            13,
            "with_hook={with_hook}"
        );
        assert!(agent.messages().iter().any(|m| m
            .content
            .as_deref()
            .is_some_and(|s| s.starts_with("turn 0 "))));
        drop(seen);
        drop(agent);
        remove_session(&dir);
    }
    Ok(())
}
