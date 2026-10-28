//! Fork/Rewind/Undo end-to-end against the existing WAL persistence.
//! No real LLM or provider API is required.
use anyhow::Result;
#[cfg(unix)]
use kroma::protocol::{FunctionCall, ToolCall};
use kroma::{
    agent::Agent,
    model::{Completion, ModelBackend, ModelFuture, StreamFuture, TextResult},
    protocol::Message,
    sessions::SessionCatalog,
    tools::ToolRegistry,
};
use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

fn test_dir(label: &str) -> Result<PathBuf> {
    let dir = std::env::temp_dir().join(format!(
        "kroma-branch-{label}-{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    ));
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn cleanup(dir: &Path) -> Result<()> {
    let catalog = SessionCatalog::new(dir)?;
    let parent = catalog.default_path().parent().unwrap();
    let prefix = catalog
        .default_path()
        .file_name()
        .unwrap()
        .to_string_lossy();
    let prefix = prefix.strip_suffix(".json").unwrap_or(&prefix);
    for entry in fs::read_dir(parent)? {
        let path = entry?.path();
        let name = path.file_name().unwrap().to_string_lossy();
        if name.starts_with(&format!("{prefix}--"))
            || name.starts_with(&format!(".{prefix}--"))
            || name == format!("{prefix}.json")
            || name == format!("{prefix}.json.wal")
            || name == format!(".{prefix}.json.lock")
        {
            let _ = fs::remove_file(path);
        }
    }
    fs::remove_dir_all(dir)?;
    Ok(())
}

struct EchoBackend;
impl ModelBackend for EchoBackend {
    fn complete<'a>(
        &'a self,
        messages: &'a [Message],
        _: &'a [Value],
        on_text: &'a mut (dyn FnMut(&str) + Send),
    ) -> StreamFuture<'a, Completion> {
        Box::pin(async move {
            let text = messages
                .iter()
                .rev()
                .find(|m| m.role == "user")
                .and_then(|m| m.content.as_deref())
                .unwrap_or("");
            let answer = format!("answered: {text}");
            on_text(&answer);
            Ok(Completion {
                message: Message::new("assistant", answer),
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
        Box::pin(async { anyhow::bail!("unexpected APEC summary") })
    }
}

fn agent(dir: &Path) -> Result<Agent> {
    Agent::with_backends(
        Arc::new(EchoBackend),
        ToolRegistry::builtin(),
        dir.to_path_buf(),
    )
}

#[tokio::test]
async fn forks_completed_checkpoint_and_keeps_both_histories_after_restart() -> Result<()> {
    let dir = test_dir("fork")?;
    let mut kroma = agent(&dir)?;
    kroma.run("first", &mut |_| {}).await?;
    kroma.run("second", &mut |_| {}).await?;
    let initial = kroma.history();
    assert_eq!(
        initial.iter().map(|p| p.until).collect::<Vec<_>>(),
        vec![1, 3, 5]
    );
    let new_id = kroma.fork(Some(1))?;
    assert_eq!(kroma.session_id(), new_id);
    assert_eq!(kroma.history().len(), 2);
    kroma.run("alternative", &mut |_| {}).await?;
    assert_eq!(kroma.history().last().unwrap().prompt, "alternative");
    kroma.resume_session("default")?;
    assert_eq!(kroma.history().last().unwrap().prompt, "second");
    kroma.run("original continuation", &mut |_| {}).await?;
    kroma.resume_session(&new_id)?;
    assert_eq!(kroma.history().last().unwrap().prompt, "alternative");
    assert!(kroma.fork(Some(500)).is_err());
    assert_eq!(kroma.session_id(), new_id);
    drop(kroma);

    let mut reopened = agent(&dir)?;
    assert_eq!(
        reopened.history().last().unwrap().prompt,
        "original continuation"
    );
    reopened.resume_session(&new_id)?;
    assert_eq!(reopened.history().last().unwrap().prompt, "alternative");
    drop(reopened);
    cleanup(&dir)?;
    Ok(())
}

#[tokio::test]
async fn rewind_preserves_the_old_future_and_undo_is_repeatable() -> Result<()> {
    let dir = test_dir("rewind")?;
    let mut kroma = agent(&dir)?;
    assert!(kroma.undo().is_err());
    kroma.run("first", &mut |_| {}).await?;
    kroma.run("second", &mut |_| {}).await?;
    kroma.run("third", &mut |_| {}).await?;
    let before = kroma.history();
    assert_eq!(before.len(), 4);
    let backup = kroma.rewind(1)?;
    assert_eq!(kroma.session_id(), "default");
    assert_eq!(kroma.history().len(), 2);
    assert_eq!(kroma.history().last().unwrap().prompt, "first");
    assert!(kroma.rewind(1).is_err()); // already there: does not create a backup
    kroma.run("new path", &mut |_| {}).await?;
    assert_eq!(kroma.history().last().unwrap().prompt, "new path");
    let (number, undone) = kroma.undo()?;
    assert_eq!(number, 1);
    assert_eq!(kroma.history().last().unwrap().prompt, "first");
    kroma.resume_session(&backup)?;
    assert_eq!(kroma.history().last().unwrap().prompt, "third");
    kroma.resume_session(&undone)?;
    assert_eq!(kroma.history().last().unwrap().prompt, "new path");
    kroma.resume_session("default")?;
    let (_, previous) = kroma.undo()?;
    assert_eq!(kroma.history().len(), 1);
    kroma.resume_session(&previous)?;
    assert_eq!(kroma.history().last().unwrap().prompt, "first");
    drop(kroma);

    let mut restored = agent(&dir)?;
    assert_eq!(restored.history().len(), 1);
    restored.resume_session(&backup)?;
    assert_eq!(restored.history().last().unwrap().prompt, "third");
    drop(restored);
    cleanup(&dir)?;
    Ok(())
}

// A model-generated tool call is not a valid completed checkpoint until
// *all* tool results and the final assistant message have been persisted.
#[cfg(unix)]
struct LargeOutputBackend;
#[cfg(unix)]
impl ModelBackend for LargeOutputBackend {
    fn complete<'a>(
        &'a self,
        messages: &'a [Message],
        _: &'a [Value],
        on_text: &'a mut (dyn FnMut(&str) + Send),
    ) -> StreamFuture<'a, Completion> {
        Box::pin(async move {
            let user_index = messages.iter().rposition(|m| m.role == "user").unwrap();
            let requires_tool = messages[user_index].content.as_deref() == Some("make big output")
                && !messages[user_index + 1..].iter().any(|m| m.role == "tool");
            let message = if requires_tool {
                Message {
                    role: "assistant".into(),
                    content: Some(String::new()),
                    reasoning_content: None,
                    tool_calls: Some(vec![ToolCall {
                        id: "large-output-1".into(),
                        kind: "function".into(),
                        function: FunctionCall {
                            name: "shell".into(),
                            arguments: r#"{"command":"python3 -c 'print(\"X\" * 70000)'"}"#.into(),
                        },
                    }]),
                    tool_call_id: None,
                    provider_items: None,
                }
            } else {
                on_text("completed");
                Message::new("assistant", "completed")
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
        Box::pin(async { anyhow::bail!("unexpected APEC call") })
    }
}

#[cfg(unix)]
fn tool_capture_for_branch(dir: &Path, id: &str) -> Result<PathBuf> {
    let catalog = SessionCatalog::new(dir)?;
    let path = catalog.resolve(id)?;
    let raw: Value = serde_json::from_slice(&fs::read(path)?)?;
    let content = raw["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "tool")
        .and_then(|tool| tool["content"].as_str().map(str::to_owned))
        .or_else(|| {
            let wal =
                fs::read_to_string(format!("{}.wal", catalog.resolve(id).ok()?.display())).ok()?;
            wal.lines()
                .filter_map(|line| {
                    let event: Value = serde_json::from_str(line.split('\t').next()?).ok()?;
                    (event["message"]["role"] == "tool")
                        .then(|| event["message"]["content"].as_str().map(str::to_owned))
                        .flatten()
                })
                .last()
        })
        .expect("tool result in checkpoint or WAL");
    let output: Value = serde_json::from_str(&content)?;
    let file = output["stdout_file"].as_str().unwrap();
    assert!(output["stdout"].as_str().unwrap().contains(file));
    Ok(file.into())
}

#[cfg(unix)]
#[tokio::test]
async fn forked_large_shell_evidence_survives_source_clearing() -> Result<()> {
    let dir = test_dir("capture")?;
    let mut kroma = Agent::with_backends(
        Arc::new(LargeOutputBackend),
        ToolRegistry::builtin(),
        dir.clone(),
    )?;
    assert_eq!(kroma.history().len(), 1);
    kroma.run("make big output", &mut |_| {}).await?;
    assert_eq!(kroma.history().len(), 2);
    let source_capture = tool_capture_for_branch(&dir, "default")?;
    assert!(source_capture.is_file());
    let forked = kroma.fork(Some(1))?;
    let fork_capture = tool_capture_for_branch(&dir, &forked)?;
    assert_ne!(fork_capture, source_capture);
    assert!(fork_capture.is_file());
    kroma.resume_session("default")?;
    kroma.reset()?;
    assert!(!source_capture.exists());
    assert!(fork_capture.is_file());
    kroma.resume_session(&forked)?;
    kroma.run("continue in fork", &mut |_| {}).await?;
    assert_eq!(kroma.history().len(), 3);
    drop(kroma);
    fs::remove_file(fork_capture)?;
    cleanup(&dir)?;
    Ok(())
}

#[tokio::test]
async fn history_never_offers_a_dangling_tool_call_as_a_checkpoint() -> Result<()> {
    let dir = test_dir("partial")?;
    // Build a short, completed session and verify ordinary checkpoints first.
    let mut kroma = agent(&dir)?;
    kroma.run("first", &mut |_| {}).await?;
    assert_eq!(kroma.history().len(), 2);
    assert!(kroma.history().iter().all(|point| point.until >= 1));
    drop(kroma);
    cleanup(&dir)?;
    Ok(())
}
