//! Execution ownership (R08, R13): a stopped or finished call must leave no live process behind.
//!
//! Three properties are pinned, each with the process it actually started:
//!
//! 1. **Dropping the executor future kills its process group.** The parent run bounds a child with
//!    `select!`/`timeout`; a discarded future runs no code, so the group has to be owned by a token
//!    whose `Drop` force-kills it. The old behaviour left an ignoring `TERM` process alive.
//! 2. **A parent deadline/cancel does not outlive the child's cleanup contract.** A real
//!    `AgentRunner` → `spawn_subagent(worker)` child whose `bash` ignores `TERM` must be gone by the
//!    time the parent's batch returns, not still holding its process group; and the parent must wait
//!    for the child's own graceful sweep instead of abandoning it after 150 ms.
//! 3. **`run_bounded` sweeps the group after a normal leader exit.** A control command that
//!    backgrounds a grandchild and exits 0 closes its pipes, so the old `truncated`-only sweep never
//!    ran and the grandchild survived the call.
//!
//! A zombie is not a live process: it has already stopped executing and every side effect it will
//! perform has happened. These tests therefore require "no live, non-zombie process", not "pid
//! still exists".

#![cfg(unix)]

use kroma_core::agent_loop::{build_runtime_with_registry, AgentRunConfig, AgentRunner};
use kroma_core::budget::BudgetConfig;
use kroma_core::env::{ExecutionEnv, LocalHostEnv};
use kroma_core::events::EventLogger;
use kroma_core::messages::{AssistantTurn, FinishReason, TokenUsage, ToolCall, TranscriptItem};
use kroma_core::process_group::ChildProcessGroup;
use kroma_core::provider::{ModelProvider, ProviderCompletion, ProviderError, ProviderRequest};
use kroma_core::revision::SnapshotStore;
use kroma_core::sandbox::{self, ExecutionPolicy};
use kroma_core::session::SessionJournal;
use kroma_core::tool::policy::{NonInteractiveApprovalHandler, PolicyEngine, PolicyMode};
use kroma_core::tool::registry::ToolRegistry;
use kroma_core::tools::bash::execute_bash_with_deadline;
use kroma_core::types::{RunStatus, ToolCallId};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::watch;

fn scratch(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("kroma-t30-{}-{}", tag, uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).expect("create scratch");
    root
}

/// `true` only while the process is still executing. A zombie has exited; its pid is a tombstone.
fn is_live(pid: i32) -> bool {
    if !ChildProcessGroup::is_process_alive(pid) {
        return false;
    }
    #[cfg(target_os = "linux")]
    {
        // `/proc/<pid>/stat` is `pid (comm) state ...`; `comm` may contain spaces and parentheses,
        // so the state is the first field after the final `)`.
        if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            if let Some(state) = stat
                .rsplit(')')
                .next()
                .and_then(|rest| rest.trim_start().chars().next())
            {
                return state != 'Z';
            }
        }
    }
    true
}

fn read_pid(path: &Path) -> i32 {
    std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("cannot read pidfile {}: {}", path.display(), e))
        .trim()
        .parse()
        .expect("pidfile holds a pid")
}

fn wait_for_pidfile(path: &Path, within: Duration) -> i32 {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if let Ok(text) = std::fs::read_to_string(path) {
            if let Ok(pid) = text.trim().parse::<i32>() {
                return pid;
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("{} was not written within {:?}", path.display(), within);
}

fn wait_for_exit(pid: i32, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if !is_live(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    !is_live(pid)
}

/// A process that ignores `SIGTERM` and is made the shell's own image with `exec`, so its pid is the
/// leader pid recorded in `pidfile` and only the group's `SIGKILL` (or the ownership token's drop)
/// can end it.
fn term_ignoring_command(pidfile: &Path) -> String {
    format!(
        "echo $$ > {}; exec python3 -c \"import signal, time; signal.signal(signal.SIGTERM, signal.SIG_IGN); time.sleep(30)\"",
        pidfile.display()
    )
}

/// R08-A: the executor future is discarded while the process ignores `TERM`; its owned group must
/// still be killed. This is the regression the review recorded as
/// `BASH_AFTER_CANCEL_THEN_FUTURE_DROP` (the old code left `ALIVE=true`).
#[tokio::test]
async fn dropping_the_bash_future_kills_its_process_group() {
    let root = scratch("drop");
    let ws = root.join("ws");
    let state = root.join("state");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    let pidfile = root.join("bash.pid");

    let raw = serde_json::json!({ "command": term_ignoring_command(&pidfile) }).to_string();
    let ws_for_task = ws.clone();
    let state_for_task = state.clone();
    let pidfile_for_task = pidfile.clone();
    let handle = tokio::spawn(async move {
        let (_, cancel_rx) = watch::channel(false);
        execute_bash_with_deadline(
            &ws_for_task,
            &state_for_task,
            &ToolCallId::new("call_drop"),
            &raw,
            &BudgetConfig::default(),
            cancel_rx,
            Duration::from_secs(2),
            None,
            &ExecutionPolicy::host(),
        )
        .await
    });

    let pid = tokio::task::spawn_blocking(move || {
        wait_for_pidfile(&pidfile_for_task, Duration::from_secs(5))
    })
    .await
    .unwrap();
    assert!(
        is_live(pid),
        "the bash leader must be running before the drop"
    );

    // Exactly what a parent `timeout`/`select!` does to the child future.
    handle.abort();
    let _ = handle.await;

    let exited = tokio::task::spawn_blocking(move || wait_for_exit(pid, Duration::from_secs(2)))
        .await
        .unwrap();
    let _ = std::fs::remove_dir_all(&root);
    assert!(
        exited,
        "the process group survived the dropped executor future; a cancelled call must leave no live process"
    );
}

// ---------------------------------------------------------------------------------------------
// R08-B: the whole product chain, parent `AgentRunner` → `spawn_subagent(worker)` → child `bash`
// ---------------------------------------------------------------------------------------------

fn usage() -> TokenUsage {
    TokenUsage {
        input_tokens: 10,
        cached_input_tokens: Some(0),
        cache_creation_input_tokens: None,
        output_tokens: 5,
        reasoning_tokens: None,
    }
}

fn tool_turn(id: &str, name: &str, args: serde_json::Value) -> AssistantTurn {
    AssistantTurn {
        content: None,
        tool_calls: vec![ToolCall {
            call_id: ToolCallId::new(id),
            name: name.to_string(),
            raw_arguments: args.to_string(),
        }],
        reasoning_blocks: Vec::new(),
        finish_reason: FinishReason::ToolCalls,
        usage: Some(usage()),
    }
}

fn stop_turn(text: &str) -> AssistantTurn {
    AssistantTurn {
        content: Some(text.to_string()),
        tool_calls: Vec::new(),
        reasoning_blocks: Vec::new(),
        finish_reason: FinishReason::Stop,
        usage: Some(usage()),
    }
}

struct ScriptProvider<F>(F);

impl<F> ModelProvider for ScriptProvider<F>
where
    F: Fn(ProviderRequest) -> Result<AssistantTurn, ProviderError> + Send + Sync + 'static,
{
    fn complete<'a>(
        &'a self,
        req: ProviderRequest,
        _budget: &'a BudgetConfig,
        _cancel_rx: watch::Receiver<bool>,
    ) -> Pin<Box<dyn Future<Output = Result<ProviderCompletion, ProviderError>> + Send + 'a>> {
        let turn = (self.0)(req);
        Box::pin(async move {
            Ok(ProviderCompletion {
                turn: turn?,
                request_body_bytes: 64,
                latency_ms: 1,
                ttft_ms: Some(1),
                attempts_used: 1,
                reported_model: None,
            })
        })
    }
}

struct ParentChildRun {
    elapsed: Duration,
    pid: i32,
    status: RunStatus,
    spawn_report: String,
    root: PathBuf,
}

/// Drive the real parent→child chain with a child `bash` that ignores `TERM`, then report the
/// parent's elapsed time, the recorded leader pid and the parent's `spawn_subagent` report.
async fn run_parent_with_hanging_worker(
    tag: &str,
    child_timeout_ms: u64,
    cancel_after: Option<Duration>,
) -> ParentChildRun {
    let root = scratch(tag);
    let ws = root.join("ws");
    let state = root.join("state");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(ws.join("file.txt"), "content\n").unwrap();
    let pidfile = root.join("child-bash.pid");
    let pidfile_for_child = pidfile.clone();

    let parent_turns = Arc::new(AtomicUsize::new(0));
    let child_turns = Arc::new(AtomicUsize::new(0));
    let parent_turns_c = Arc::clone(&parent_turns);
    let child_turns_c = Arc::clone(&child_turns);
    let provider = Arc::new(ScriptProvider(move |req: ProviderRequest| {
        if req.system_prompt.contains("[SUBAGENT ROLE: worker") {
            if child_turns_c.fetch_add(1, Ordering::SeqCst) == 0 {
                Ok(tool_turn(
                    "child_hang",
                    "bash",
                    serde_json::json!({ "command": term_ignoring_command(&pidfile_for_child) }),
                ))
            } else {
                Ok(stop_turn("child finished"))
            }
        } else if parent_turns_c.fetch_add(1, Ordering::SeqCst) == 0 {
            Ok(tool_turn(
                "call_worker_hang",
                "spawn_subagent",
                serde_json::json!({
                    "role": "worker",
                    "task": "run a command that ignores TERM",
                    "timeout_ms": child_timeout_ms,
                    "max_turns": 3
                }),
            ))
        } else {
            Ok(stop_turn("worker call returned"))
        }
    }));

    let env = Arc::new(LocalHostEnv::new(&ws, state.clone()).unwrap());
    let policy_engine = PolicyEngine::new(
        PolicyMode::FullAutonomous,
        Arc::new(NonInteractiveApprovalHandler),
    );
    let runtime = build_runtime_with_registry(
        ToolRegistry::default(),
        env.clone(),
        policy_engine,
        BudgetConfig::default(),
    );
    let journal = SessionJournal::create(state.join("sessions"), env.workspace().root()).unwrap();
    let store = Arc::new(SnapshotStore::new(state.join("snapshots")));
    let (cancel_tx, cancel_rx) = watch::channel(false);
    if let Some(delay) = cancel_after {
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            let _ = cancel_tx.send(true);
        });
    }
    let cfg = AgentRunConfig::new("prove the child process dies", BudgetConfig::default());
    let logger = EventLogger::new(cfg.run_id, Some(state.clone()), false).unwrap();
    let runner = AgentRunner::new(cfg, provider, runtime, cancel_rx)
        .with_session_journal(journal)
        .with_snapshot_store(store)
        .with_max_subagents(2);

    let started = Instant::now();
    let (summary, _logger) = runner.run(logger).await;
    let elapsed = started.elapsed();
    let spawn_report = summary
        .transcript
        .iter()
        .find_map(|item| match item {
            TranscriptItem::ToolResult(tr) if tr.tool_name == "spawn_subagent" => {
                Some(tr.content.to_string())
            }
            _ => None,
        })
        .unwrap_or_default();
    let pid =
        tokio::task::spawn_blocking(move || wait_for_pidfile(&pidfile, Duration::from_secs(1)))
            .await
            .unwrap();
    ParentChildRun {
        elapsed,
        pid,
        status: summary.status,
        spawn_report,
        root,
    }
}

/// R08-B1: the child's own `timeout_ms` deadline path must finish (its group sweep included) before
/// the parent reports, and must leave no live process. The old 150 ms hard join cut through the
/// child's 200 ms deadline grace and abandoned a live group.
#[tokio::test]
async fn parent_deadline_leaves_no_live_child_process() {
    let run = run_parent_with_hanging_worker("parent-deadline", 600, None).await;
    assert_eq!(
        run.status,
        RunStatus::Completed,
        "the parent run must finish normally: {}",
        run.spawn_report
    );
    let exited =
        tokio::task::spawn_blocking(move || wait_for_exit(run.pid, Duration::from_secs(2)))
            .await
            .unwrap();
    let _ = std::fs::remove_dir_all(&run.root);
    assert!(
        exited,
        "the child's bash process is still alive after the parent returned; a timed-out subagent must not leave a live process group"
    );
}

/// R08-B2: a parent cancellation must wait for the child's own graceful sweep (the executor gives
/// `SIGTERM` a 2 s grace), not abandon it. The elapsed time is the evidence that the owner stayed
/// alive for the cleanup contract, and the pid check is the evidence that it then stopped.
#[tokio::test]
async fn parent_cancel_waits_for_the_child_cleanup_contract() {
    let run =
        run_parent_with_hanging_worker("parent-cancel", 30_000, Some(Duration::from_millis(200)))
            .await;
    let exited =
        tokio::task::spawn_blocking(move || wait_for_exit(run.pid, Duration::from_secs(2)))
            .await
            .unwrap();
    let elapsed = run.elapsed;
    let _ = std::fs::remove_dir_all(&run.root);
    assert!(
        elapsed >= Duration::from_millis(1_500),
        "the parent returned after {elapsed:?}: it abandoned the child's 2 s SIGTERM grace instead of letting the owned cleanup finish"
    );
    assert!(
        exited,
        "the child's bash process is still alive after the cancelled parent returned"
    );
}

/// R13: a control command that backgrounds a grandchild and exits 0 must not leave the grandchild
/// behind just because no pipe stayed open.
#[test]
fn run_bounded_sweeps_a_background_descendant_after_normal_exit() {
    let root = scratch("run-bounded");
    let pidfile = root.join("sleep.pid");
    let command = format!(
        "sleep 30 >/dev/null 2>&1 & echo $! > {}; exit 0",
        pidfile.display()
    );
    let outcome = sandbox::run_bounded("sh", &["-c".to_string(), command], Duration::from_secs(5))
        .expect("the control command completes");
    assert!(outcome.success, "the leader exits cleanly: {:?}", outcome);
    assert!(
        !outcome.truncated,
        "nothing holds the pipes; this is the normal-exit case, not the flood case"
    );

    let pid = read_pid(&pidfile);
    let exited = wait_for_exit(pid, Duration::from_secs(2));
    let _ = std::fs::remove_dir_all(&root);
    assert!(
        exited,
        "run_bounded returned while the backgrounded grandchild was still running"
    );
}
