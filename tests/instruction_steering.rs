//! No real LLM required. Verifies prompt projection, steering order across a
//! complete batched tool round, durable WAL, and separately queued tasks.
use anyhow::{bail, Result};
use kroma::{
    agent::Agent,
    model::{Completion, ModelBackend, ModelFuture, StreamFuture, TextResult},
    protocol::{FunctionCall, Message, ToolCall},
    sessions::SessionCatalog,
    tools::{Tool, ToolContext, ToolFuture, ToolRegistry, ToolResult},
};
use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::oneshot;

fn isolated_dir(label: &str) -> Result<PathBuf> {
    let dir = std::env::temp_dir().join(format!(
        "kroma-steering-{label}-{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    ));
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn cleanup(workdir: &Path) -> Result<()> {
    let catalog = SessionCatalog::new(workdir)?;
    let snapshot = catalog.default_path();
    for path in [
        snapshot.to_path_buf(),
        PathBuf::from(format!("{}.wal", snapshot.display())),
        PathBuf::from(format!("{}.lock", snapshot.display())),
    ] {
        if path.exists() {
            fs::remove_file(path)?;
        }
    }
    fs::remove_dir_all(workdir)?;
    Ok(())
}

struct ModelProbe {
    calls: AtomicUsize,
    seen: Mutex<Vec<Vec<Message>>>,
    started: Mutex<Option<oneshot::Sender<()>>>,
    release: Mutex<Option<oneshot::Receiver<()>>>,
    two_tools: bool,
}

impl ModelBackend for ModelProbe {
    fn complete<'a>(
        &'a self,
        messages: &'a [Message],
        _: &'a [Value],
        on_text: &'a mut (dyn FnMut(&str) + Send),
    ) -> StreamFuture<'a, Completion> {
        Box::pin(async move {
            let index = self.calls.fetch_add(1, Ordering::SeqCst);
            self.seen.lock().unwrap().push(messages.to_vec());
            if index == 0 {
                if let Some(started) = self.started.lock().unwrap().take() {
                    let _ = started.send(());
                }
                let release = self.release.lock().unwrap().take();
                if let Some(release) = release {
                    release
                        .await
                        .map_err(|_| anyhow::anyhow!("probe release cancelled"))?;
                }
            }
            let message = if index == 0 && self.two_tools {
                Message {
                    role: "assistant".into(),
                    content: Some(String::new()),
                    reasoning_content: None,
                    tool_calls: Some(
                        ["first", "second"]
                            .iter()
                            .map(|s| ToolCall {
                                id: format!("call-{s}"),
                                kind: "function".into(),
                                function: FunctionCall {
                                    name: "shell".into(),
                                    arguments: format!(r#"{{"command":"printf {s}"}}"#),
                                },
                            })
                            .collect(),
                    ),
                    tool_call_id: None,
                    provider_items: None,
                }
            } else {
                let text = if index == 0 {
                    "first answer"
                } else {
                    "finished"
                };
                on_text(text);
                Message::new("assistant", text)
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
        Box::pin(async { bail!("unexpected APEC summary during short test") })
    }
}

async fn steering_integration(two_tools: bool) -> Result<()> {
    let dir = isolated_dir(if two_tools { "tools" } else { "final" })?;
    let (started, ready) = oneshot::channel();
    let (release, released) = oneshot::channel();
    let model = Arc::new(ModelProbe {
        calls: AtomicUsize::new(0),
        seen: Mutex::new(Vec::new()),
        started: Mutex::new(Some(started)),
        release: Mutex::new(Some(released)),
        two_tools,
    });
    let mut agent = Agent::with_backends(model.clone(), ToolRegistry::builtin(), dir.clone())?;
    let handle = agent.task_input();
    let sender = handle.clone();
    let injector = tokio::spawn(async move {
        ready.await.unwrap();
        sender.steer("use the newer approach").unwrap();
        sender.steer("and explain briefly").unwrap();
        sender.queue_followup("check another feature").unwrap();
        release.send(()).unwrap();
    });
    let response = agent.run("original task", &mut |_| {}).await?;
    injector.await?;
    assert_eq!(response, "finished");
    assert!(handle.steer("too late").is_err());
    assert_eq!(handle.pending_steering(), 0);
    assert_eq!(model.calls.load(Ordering::SeqCst), 2);
    let all = model.seen.lock().unwrap().clone();
    let second = &all[1];
    let steering_at: Vec<usize> = second
        .iter()
        .enumerate()
        .filter_map(|(i, m)| {
            m.content
                .as_deref()
                .unwrap_or("")
                .contains("[Steering instruction for the current task]")
                .then_some(i)
        })
        .collect();
    assert_eq!(steering_at.len(), 2);
    assert_eq!(
        second[steering_at[0]].content.as_deref(),
        Some("[Steering instruction for the current task]\nuse the newer approach")
    );
    assert_eq!(
        second[steering_at[1]].content.as_deref(),
        Some("[Steering instruction for the current task]\nand explain briefly")
    );
    if two_tools {
        let tool_results: Vec<_> = second
            .iter()
            .enumerate()
            .filter(|(_, m)| m.role == "tool")
            .collect();
        assert_eq!(tool_results.len(), 2);
        assert!(tool_results[1].0 < steering_at[0]);
        assert!(tool_results[0]
            .1
            .content
            .as_deref()
            .unwrap()
            .contains("first"));
        assert!(tool_results[1]
            .1
            .content
            .as_deref()
            .unwrap()
            .contains("second"));
    } else {
        let first_answer = second
            .iter()
            .position(|m| m.role == "assistant" && m.content.as_deref() == Some("first answer"))
            .unwrap();
        assert!(first_answer < steering_at[0]);
    }
    // Queued instructions are separate turns, not injected into this task.
    assert!(!second
        .iter()
        .any(|m| m.content.as_deref() == Some("check another feature")));
    let next = handle.take_followup().unwrap();
    assert_eq!(next, "check another feature");
    assert_eq!(agent.run(&next, &mut |_| {}).await?, "finished");
    assert_eq!(model.calls.load(Ordering::SeqCst), 3);
    drop(agent);
    let raw = fs::read_to_string(format!(
        "{}.wal",
        SessionCatalog::new(&dir)?.default_path().display()
    ))?;
    assert!(raw.contains("check another feature"));
    assert!(raw.contains("use the newer approach"));
    cleanup(&dir)?;
    Ok(())
}

#[tokio::test]
async fn steering_waits_for_all_tool_results_and_queue_is_separate() -> Result<()> {
    steering_integration(true).await
}

#[tokio::test]
async fn steering_after_a_text_only_response_prevents_early_completion() -> Result<()> {
    steering_integration(false).await
}

#[tokio::test]
async fn project_instructions_reload_between_tasks_without_polluting_saved_system() -> Result<()> {
    let dir = isolated_dir("prompt")?;
    fs::write(dir.join("AGENTS.md"), "PROJECT_VERSION_ONE")?;
    let model = Arc::new(ModelProbe {
        calls: AtomicUsize::new(1),
        seen: Mutex::new(Vec::new()),
        started: Mutex::new(None),
        release: Mutex::new(None),
        two_tools: false,
    });
    let mut agent = Agent::with_backends(model.clone(), ToolRegistry::builtin(), dir.clone())?;
    assert_eq!(agent.run("task1", &mut |_| {}).await?, "finished");
    fs::write(dir.join("AGENTS.md"), "PROJECT_VERSION_TWO")?;
    assert_eq!(agent.run("task2", &mut |_| {}).await?, "finished");
    let seen = model.seen.lock().unwrap();
    let first = seen[0][0].content.as_deref().unwrap();
    let second = seen[1][0].content.as_deref().unwrap();
    assert!(first.contains("PROJECT_VERSION_ONE"));
    assert!(!first.contains("PROJECT_VERSION_TWO"));
    assert!(second.contains("PROJECT_VERSION_TWO"));
    assert!(!second.contains("PROJECT_VERSION_ONE"));
    drop(seen);
    drop(agent);
    let snapshot: Value =
        serde_json::from_slice(&fs::read(SessionCatalog::new(&dir)?.default_path())?)?;
    assert!(!snapshot["messages"][0]
        .to_string()
        .contains("PROJECT_VERSION_ONE"));
    assert!(!snapshot["messages"][0]
        .to_string()
        .contains("PROJECT_VERSION_TWO"));
    cleanup(&dir)?;
    Ok(())
}

/// A nested instruction that a declared location points at is loaded before the
/// write; a path that only appears in tool output is not a declared location.
struct NestedInstructionProbe {
    seen: Mutex<Vec<Vec<Message>>>,
    discover_by_result: bool,
}

impl ModelBackend for NestedInstructionProbe {
    fn complete<'a>(
        &'a self,
        messages: &'a [Message],
        _: &'a [Value],
        _: &'a mut (dyn FnMut(&str) + Send),
    ) -> StreamFuture<'a, Completion> {
        Box::pin(async move {
            let mut seen = self.seen.lock().unwrap();
            seen.push(messages.to_vec());
            let turn = seen.len();
            drop(seen);
            let calls = if turn == 1 {
                if self.discover_by_result {
                    vec![
                        ("reveal", "{}".to_string()),
                        (
                            "shell",
                            serde_json::json!({"command":"printf touched > marker.txt"})
                                .to_string(),
                        ),
                    ]
                } else {
                    vec![(
                        "shell",
                        serde_json::json!({"command":"cd src/deep && printf touched > marker.txt"})
                            .to_string(),
                    )]
                }
            } else {
                Vec::new()
            };
            let message = if calls.is_empty() {
                Message::new("assistant", "replanned")
            } else {
                Message {
                    role: "assistant".into(),
                    content: None,
                    reasoning_content: None,
                    tool_calls: Some(
                        calls
                            .into_iter()
                            .enumerate()
                            .map(|(i, (name, arguments))| ToolCall {
                                id: format!("nested-{i}"),
                                kind: "function".into(),
                                function: FunctionCall {
                                    name: name.into(),
                                    arguments,
                                },
                            })
                            .collect(),
                    ),
                    tool_call_id: None,
                    provider_items: None,
                }
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
        Box::pin(async { bail!("unexpected checkpoint") })
    }
}

struct RevealNestedPath;

impl Tool for RevealNestedPath {
    fn name(&self) -> &str {
        "reveal"
    }

    fn definition(&self) -> Value {
        serde_json::json!({"type":"function","function":{"name":"reveal","parameters":{"type":"object"}}})
    }

    fn execute<'a>(&'a self, _: &'a ToolCall, _: ToolContext<'a>) -> ToolFuture<'a> {
        Box::pin(async { ToolResult::ok("src/deep/file.rs:12:4") })
    }
}

async fn nested_instruction_preflight(discover_by_result: bool) -> Result<()> {
    let dir = isolated_dir(if discover_by_result {
        "tool-result"
    } else {
        "tool-arguments"
    })?;
    fs::create_dir_all(dir.join("src/deep"))?;
    fs::write(dir.join("src/deep/AGENTS.md"), "DEEP_DIRECTORY_RULE")?;
    fs::write(dir.join("src/deep/file.rs"), "// source")?;
    let model = Arc::new(NestedInstructionProbe {
        seen: Mutex::new(Vec::new()),
        discover_by_result,
    });
    let mut tools = ToolRegistry::builtin();
    if discover_by_result {
        tools.register(Arc::new(RevealNestedPath))?;
    }
    let mut agent = Agent::with_backends(model.clone(), tools, dir.clone())?;
    assert_eq!(agent.run("Fix the bug", &mut |_| {}).await?, "replanned");
    let seen = model.seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    assert!(!seen[0][0]
        .content
        .as_deref()
        .unwrap()
        .contains("DEEP_DIRECTORY_RULE"));
    let loaded = seen[1][0]
        .content
        .as_deref()
        .unwrap()
        .contains("DEEP_DIRECTORY_RULE");
    let tool_results: Vec<_> = seen[1].iter().filter(|m| m.role == "tool").collect();
    if discover_by_result {
        // A path that only shows up in tool output is not a declared location:
        // the batch runs as returned and no nested rule is loaded for it.
        assert!(!loaded);
        assert_eq!(tool_results.len(), 2);
        assert!(dir.join("marker.txt").exists());
    } else {
        // A declared target directory loads its own instructions first, and the
        // write is replaced by a durable "not executed" result.
        assert!(loaded);
        assert_eq!(tool_results.len(), 1);
        assert!(tool_results
            .last()
            .unwrap()
            .content
            .as_deref()
            .unwrap()
            .contains("not_executed"));
        assert!(!dir.join("marker.txt").exists());
    }
    assert!(!dir.join("src/deep/marker.txt").exists());
    drop(seen);
    drop(agent);
    cleanup(&dir)?;
    Ok(())
}

#[tokio::test]
async fn first_write_into_nested_directory_is_not_dispatched_before_instructions() -> Result<()> {
    nested_instruction_preflight(false).await
}

#[tokio::test]
async fn a_path_seen_only_in_tool_output_does_not_hold_back_the_batch() -> Result<()> {
    nested_instruction_preflight(true).await
}
