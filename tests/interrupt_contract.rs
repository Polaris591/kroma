//! Interruption contract: dropping a running Agent future must stop the Shell
//! it dispatched, keep the interrupted result an error for the provider, and
//! tell the next turn how long that command had been running. No provider,
//! credential or network access is used.
use anyhow::Result;
use kroma::{
    agent::Agent,
    context::{BuiltinContextFactory, ContextConfig},
    model::{Completion, ModelBackend, ModelFuture, StreamFuture, TextResult},
    protocol::{FunctionCall, Message, ToolCall},
    tools::ToolRegistry,
};
use serde_json::Value;
use std::{
    fs,
    path::PathBuf,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

fn isolated_dir(label: &str) -> Result<PathBuf> {
    let path = std::env::temp_dir().join(format!(
        "kroma-interrupt-{label}-{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos(),
    ));
    fs::create_dir_all(&path)?;
    Ok(path)
}

/// Answers the first request with one blocking `shell` call, then answers any
/// later request. The turn is always interrupted before the second answer.
struct ShellCallBackend {
    calls: AtomicUsize,
    command: String,
}

impl ModelBackend for ShellCallBackend {
    fn complete<'a>(
        &'a self,
        _: &'a [Message],
        _: &'a [Value],
        _: &'a mut (dyn FnMut(&str) + Send),
    ) -> StreamFuture<'a, Completion> {
        Box::pin(async move {
            let first = self.calls.fetch_add(1, Ordering::SeqCst) == 0;
            let mut message = Message::new("assistant", if first { "" } else { "done" });
            if first {
                message.tool_calls = Some(vec![ToolCall {
                    id: "call-1".into(),
                    kind: "function".into(),
                    function: FunctionCall {
                        name: "shell".into(),
                        arguments: serde_json::json!({ "command": self.command }).to_string(),
                    },
                }]);
            }
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
        Box::pin(async { anyhow::bail!("no summary in this test") })
    }
}

fn interrupted_result(agent: &Agent) -> Value {
    let message = agent
        .messages()
        .iter()
        .rev()
        .find(|message| message.role == "tool")
        .expect("an interrupted tool result");
    assert!(
        message.tool_failed(),
        "an interrupted tool must stay an error for the provider"
    );
    serde_json::from_str(message.content.as_deref().unwrap_or("")).expect("JSON tool result")
}

fn running_seconds(result: &Value) -> f64 {
    let detail = result["error"].as_str().expect("error text");
    let tail = detail
        .split("had been running for ")
        .nth(1)
        .unwrap_or_else(|| panic!("missing running time: {detail}"));
    tail.split('s')
        .next()
        .unwrap_or_default()
        .parse()
        .unwrap_or_else(|_| panic!("unparsable running time: {detail}"))
}

#[tokio::test]
async fn a_dropped_turn_kills_the_shell_and_records_how_long_it_ran() -> Result<()> {
    let dir = isolated_dir("shell")?;
    let started = dir.join("started");
    let finished = dir.join("finished");
    let command = format!(
        "echo started > \"{}\"; sleep 5; echo finished > \"{}\"",
        started.display(),
        finished.display()
    );
    let mut agent = Agent::with_runtime_budget(
        Arc::new(ShellCallBackend {
            calls: AtomicUsize::new(0),
            command,
        }),
        ToolRegistry::builtin(),
        Arc::new(BuiltinContextFactory),
        Default::default(),
        None,
        dir.clone(),
        ContextConfig::with_output_reserve(8192, 1024)?,
    )?;
    let mut sink = |_: &str| {};
    // Ctrl+C is exactly this: the host drops the running future first.
    {
        let mut run = Box::pin(agent.run("run the long command", &mut sink));
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        loop {
            if started.exists() {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the shell never started"
            );
            tokio::select! {
                _ = &mut run => panic!("the task finished before it was interrupted"),
                _ = tokio::time::sleep(Duration::from_millis(25)) => {}
            }
        }
    }
    agent.cancel()?;

    let result = interrupted_result(&agent);
    let seconds = running_seconds(&result);
    assert!(
        (0.0..5.0).contains(&seconds),
        "the record must say how long the command ran, got {seconds}"
    );
    let note = agent
        .messages()
        .last()
        .and_then(|message| message.content.as_deref())
        .unwrap_or_default();
    assert!(note.contains("Task cancelled by user."), "{note}");
    let durable = agent.messages().len();
    drop(agent);

    // The killed command must never reach its second marker.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(
        !finished.exists(),
        "the interrupted shell kept running after the turn was cancelled"
    );

    // The unknown result and the elapsed time survive a restart: a second
    // agent on the same workdir reads them back from the append journal.
    let reloaded = Agent::with_runtime_budget(
        Arc::new(ShellCallBackend {
            calls: AtomicUsize::new(0),
            command: "true".into(),
        }),
        ToolRegistry::builtin(),
        Arc::new(BuiltinContextFactory),
        Default::default(),
        None,
        dir.clone(),
        ContextConfig::with_output_reserve(8192, 1024)?,
    )?;
    assert_eq!(reloaded.messages().len(), durable);
    let restored = interrupted_result(&reloaded);
    assert_eq!(
        running_seconds(&restored),
        seconds,
        "the durable record must keep the same running time"
    );
    drop(reloaded);
    fs::remove_dir_all(dir)?;
    Ok(())
}

#[tokio::test]
async fn a_cancelled_turn_without_a_dispatched_tool_invents_no_result() -> Result<()> {
    let dir = isolated_dir("notool")?;
    let mut agent = Agent::with_runtime_budget(
        Arc::new(ShellCallBackend {
            calls: AtomicUsize::new(0),
            command: "echo should-not-run".into(),
        }),
        ToolRegistry::builtin(),
        Arc::new(BuiltinContextFactory),
        Default::default(),
        None,
        dir.clone(),
        ContextConfig::with_output_reserve(8192, 1024)?,
    )?;
    agent.cancel()?;
    assert!(
        agent
            .messages()
            .iter()
            .all(|message| message.role != "tool"),
        "nothing ran, so no tool result may be invented"
    );
    drop(agent);
    fs::remove_dir_all(dir)?;
    Ok(())
}

#[test]
fn pending_calls_report_running_time_only_for_the_active_call() {
    let mut messages = vec![
        Message::new("system", "root"),
        Message {
            role: "assistant".into(),
            content: None,
            reasoning_content: None,
            tool_calls: Some(vec![
                ToolCall {
                    id: "done".into(),
                    kind: "function".into(),
                    function: FunctionCall {
                        name: "shell".into(),
                        arguments: "{}".into(),
                    },
                },
                ToolCall {
                    id: "active".into(),
                    kind: "function".into(),
                    function: FunctionCall {
                        name: "shell".into(),
                        arguments: "{}".into(),
                    },
                },
                ToolCall {
                    id: "queued".into(),
                    kind: "function".into(),
                    function: FunctionCall {
                        name: "shell".into(),
                        arguments: "{}".into(),
                    },
                },
            ]),
            tool_call_id: None,
            provider_items: None,
        },
        Message::tool("done".into(), "{\"exit_code\":0}".into()),
    ];
    let interrupted = kroma::session::InterruptedCall {
        id: "active".into(),
        name: "shell".into(),
        elapsed: Duration::from_millis(3200),
    };
    assert!(kroma::session::recover_pending_calls_with(
        &mut messages,
        Some(&interrupted)
    ));
    let text = |id: &str| {
        messages
            .iter()
            .find(|message| message.tool_call_id.as_deref() == Some(id))
            .and_then(|message| message.content.clone())
            .unwrap_or_default()
    };
    assert!(text("active").contains("had been running for 3.2s"));
    assert!(text("active").contains("Session interrupted. Tool result is unknown;"));
    assert!(
        text("queued").contains("Session interrupted. Tool result is unknown; do not assume"),
        "a call that never started keeps the generic wording: {}",
        text("queued")
    );
    assert!(!text("queued").contains("had been running"));
    assert!(!kroma::session::recover_pending_calls_with(
        &mut messages,
        None
    ));
}
