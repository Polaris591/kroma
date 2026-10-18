use crate::budget::BudgetConfig;
use crate::messages::ToolResultMeta;
use crate::process_launch::{EnvironmentPolicy, ProcessLaunch};
use crate::tool::output_buffer::{HeadTailBuffer, StreamLineCollector};
use crate::tools::ToolExecutionOutput;
use crate::types::{ErrorCode, ToolCallId};
use serde::Deserialize;
use std::path::Path;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;
use tokio::sync::{watch, Notify};

pub const PIPE_DRAIN_TIMEOUT: Duration = Duration::from_millis(200);

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BashArgs {
    pub command: String,
    pub timeout_sec: Option<u64>,
}

/// Check if an environment variable name matches the sensitive blacklist (§5.4 V6-05):
/// `KROMA_*`, `OPENAI_*`, `ANTHROPIC_*`, `*_API_KEY`, `*_SECRET`, `*_TOKEN`, `*_PASSWORD`, `*_CREDENTIAL*`.
pub fn is_blacklisted_env_var(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    upper.starts_with("KROMA_")
        || upper.starts_with("OPENAI_")
        || upper.starts_with("ANTHROPIC_")
        || upper.starts_with("AWS_")
        || upper.starts_with("GITHUB_")
        || upper.starts_with("GEMINI_")
        || upper.ends_with("_API_KEY")
        || upper == "API_KEY"
        || upper.ends_with("_KEY")
        || upper.contains("_SECRET")
        || upper == "SECRET"
        || upper.ends_with("_TOKEN")
        || upper == "TOKEN"
        || upper.ends_with("_PASSWORD")
        || upper == "PASSWORD"
        || upper.contains("_CREDENTIAL")
        || upper.starts_with("CREDENTIAL")
        // Build/CI markers are scrubbed as well: a supervised child must behave the same whether kroma
        // runs on a laptop or on a CI runner. npm, pytest and friends change behaviour when they see
        // `CI`, and `NO_COLOR` would change output that the harness parses. Leaking them makes the
        // child's behaviour depend on where the agent happens to run - and made this suite fail only in
        // CI, which is exactly the class of bug a scrub is supposed to prevent.
        || upper == "CI"
        || upper == "CONTINUOUS_INTEGRATION"
        || upper == "NO_COLOR"
        || upper == "FORCE_COLOR"
}

pub fn collect_sanitized_env() -> Vec<(std::ffi::OsString, std::ffi::OsString)> {
    std::env::vars_os()
        .filter(|(k, _)| {
            let name = k.to_string_lossy();
            !is_blacklisted_env_var(&name)
        })
        .collect()
}

/// Drain one child pipe into the shared head/tail buffer.
///
/// Runs until the child exits (plus a bounded drain grace period so a `setsid`ed grandchild
/// cannot pin the reader), the flood cap trips, or the pipe closes. Both pipes run exactly this
/// logic -- only the stream differs -- so it lives here once instead of being spelled out for
/// stdout and stderr separately.
fn spawn_pipe_drain<R>(
    stream: Option<R>,
    shared_buf: Arc<Mutex<HeadTailBuffer>>,
    flood_notify: Arc<Notify>,
    mut exited_rx: watch::Receiver<bool>,
    flood_cap: usize,
) -> tokio::task::JoinHandle<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let Some(mut reader) = stream else { return };
        let mut collector = StreamLineCollector::new(2048);
        let mut chunk = [0u8; 4096];
        let mut drain_deadline: Option<tokio::time::Instant> = None;
        loop {
            let read_res = if *exited_rx.borrow() {
                let deadline = *drain_deadline
                    .get_or_insert_with(|| tokio::time::Instant::now() + PIPE_DRAIN_TIMEOUT);
                match tokio::time::timeout_at(deadline, reader.read(&mut chunk)).await {
                    Ok(res) => res,
                    Err(_) => break,
                }
            } else {
                tokio::select! {
                    res = reader.read(&mut chunk) => res,
                    _ = async {
                        loop {
                            if exited_rx.changed().await.is_err() {
                                std::future::pending::<()>().await;
                            }
                            if *exited_rx.borrow() {
                                break;
                            }
                        }
                    } => {
                        let deadline = *drain_deadline
                            .get_or_insert_with(|| tokio::time::Instant::now() + PIPE_DRAIN_TIMEOUT);
                        match tokio::time::timeout_at(deadline, reader.read(&mut chunk)).await {
                            Ok(res) => res,
                            Err(_) => break,
                        }
                    }
                }
            };
            match read_res {
                Ok(0) => break,
                Ok(n) => {
                    let mut exceeded = false;
                    collector.push_chunk(&chunk[..n], |seg| {
                        let mut guard = shared_buf.lock().unwrap();
                        guard.push_str(seg);
                        if guard.total_observed_bytes() > flood_cap {
                            exceeded = true;
                        }
                    });
                    if exceeded {
                        flood_notify.notify_one();
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        collector.finish(|seg| {
            let mut guard = shared_buf.lock().unwrap();
            guard.push_str(seg);
            if guard.total_observed_bytes() > flood_cap {
                flood_notify.notify_one();
            }
        });
    })
}

// T09 added the run's execution policy, which belongs beside the budget and deadline rather than
// inside them: it is a security decision, not a resource limit. Grouping these into a struct would
// only move the same eight values one level down.
#[allow(clippy::too_many_arguments)]
pub async fn execute_bash(
    workspace_root: &Path,
    state_dir: &Path,
    call_id: &ToolCallId,
    raw_args: &str,
    budget: &BudgetConfig,
    cancel_rx: watch::Receiver<bool>,
    grace_period: Duration,
    policy: &crate::sandbox::ExecutionPolicy,
) -> ToolExecutionOutput {
    execute_bash_with_deadline(
        workspace_root,
        state_dir,
        call_id,
        raw_args,
        budget,
        cancel_rx,
        grace_period,
        None,
        policy,
    )
    .await
}

/// F04 (`v1.5.1`): `bash` clamped to the enclosing run's absolute deadline.
///
/// The command's own `timeout_sec` (and `budget.max_run_duration`) are still honoured, but the
/// process group is also terminated at `deadline`, so a long command cannot outlive the run. The
/// termination path is the same graceful-then-`SIGKILL` sweep used for the other stop triggers —
/// the future is never simply dropped, which would leave the process group and its pipes alive.
#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_arguments)]
pub async fn execute_bash_with_deadline(
    workspace_root: &Path,
    state_dir: &Path,
    call_id: &ToolCallId,
    raw_args: &str,
    budget: &BudgetConfig,
    mut cancel_rx: watch::Receiver<bool>,
    grace_period: Duration,
    deadline: Option<crate::budget::RunDeadline>,
    policy: &crate::sandbox::ExecutionPolicy,
) -> ToolExecutionOutput {
    let started = Instant::now();
    let args: BashArgs = match serde_json::from_str(raw_args) {
        Ok(a) => a,
        Err(e) => {
            return ToolExecutionOutput::error(
                ErrorCode::InvalidArguments,
                format!("invalid JSON arguments for `bash`: {}", e),
                ToolResultMeta::default(),
            );
        }
    };

    if args.command.trim().is_empty() {
        return ToolExecutionOutput::error(
            ErrorCode::InvalidArguments,
            "`command` must not be empty".to_string(),
            ToolResultMeta::default(),
        );
    }

    let timeout_sec = args.timeout_sec.unwrap_or(120);
    if timeout_sec == 0 {
        return ToolExecutionOutput::error(
            ErrorCode::InvalidArguments,
            "`timeout_sec` must be > 0".to_string(),
            ToolResultMeta::default(),
        );
    }
    let timeout_duration = Duration::from_secs(timeout_sec).min(budget.max_run_duration);

    // F04: if the run deadline has already passed, refuse to spawn a process at all rather than
    // start one only to kill it immediately.
    if deadline.is_some_and(|d| d.is_exceeded()) {
        return ToolExecutionOutput::error(
            ErrorCode::ToolTimeout,
            "run deadline exceeded before the command could start".to_string(),
            ToolResultMeta::default(),
        );
    }

    let (shell_bin, shell_args) = crate::process_group::resolve_shell_invocation(&args.command);

    // F04: the shared launch seam owns the credential scrub, the process group / Windows Job
    // Object, and the fail-closed containment attach, so `bash` cannot drift from the other
    // supervised spawns. `SanitizedWithCustomNoBaseline` keeps `bash`'s original environment
    // exactly: the scrub only, with no `CI` / `NO_COLOR` baseline injected. `shell_invocation`
    // preserves the Windows `cmd.exe` verbatim `/D /S /C "<raw>"` argv contract. Stdio is spelled
    // out because `bash` depends on null stdin and piped stdout/stderr.
    // T09 (KR-18): every real spawn goes through the run's execution policy. In isolated mode this
    // translates the shell invocation into the sandbox argv, and if the backend is unavailable the
    // command is refused — there is deliberately no branch that runs it on the host instead.
    let prepared = match policy.launch_argv(
        crate::sandbox::ExecutionClass::AgentBash,
        workspace_root,
        state_dir,
        &shell_bin,
        &shell_args,
    ) {
        Ok(prepared) => prepared,
        Err(e) => {
            return ToolExecutionOutput::error(
                ErrorCode::SandboxUnavailable,
                format!("refusing to run bash: {}", e),
                ToolResultMeta {
                    execution_backend: Some("none:refused".to_string()),
                    ..Default::default()
                },
            );
        }
    };
    let execution_backend = prepared.backend.clone();

    // The host path is the process's working directory on the host; the sandbox's own `--chdir`
    // puts the command inside the guest view, so the two must not be confused.
    let mut launch = ProcessLaunch::new(prepared.program, prepared.args).cwd(workspace_root);
    if prepared.backend == "host" {
        // On the host the shell argv is re-derived from the raw command (Windows needs the
        // `cmd.exe /D /S /C "<raw>"` form). Inside a sandbox the shell invocation is already the
        // tail of the wrapped argv, so re-deriving it would append the command a second time.
        launch = launch.shell_invocation(&args.command);
    }
    let (mut child, proc_group) = match launch
        .stdio(Stdio::null(), Stdio::piped(), Stdio::piped())
        .env_policy(EnvironmentPolicy::SanitizedWithCustomNoBaseline(Vec::new()))
        .spawn_tokio()
    {
        Ok(pair) => pair,
        Err(e) => {
            return ToolExecutionOutput::error(
                ErrorCode::IoError,
                format!("failed to spawn shell process: {}", e),
                ToolResultMeta::default(),
            );
        }
    };
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let spill_path = state_dir.join(format!("tool-{}.log", call_id.as_str()));
    let shared_buf = Arc::new(Mutex::new(HeadTailBuffer::new(
        budget.bash_head_bytes,
        budget.bash_tail_bytes,
        budget.max_tool_inline_lines,
        budget.bash_spill_bytes,
        Some(spill_path),
    )));

    let flood_cap = budget.max_process_output_observed_bytes;
    let flood_notify = Arc::new(Notify::new());
    let (child_exited_tx, child_exited_rx) = watch::channel(false);

    let stdout_task = spawn_pipe_drain(
        stdout,
        Arc::clone(&shared_buf),
        Arc::clone(&flood_notify),
        child_exited_rx.clone(),
        flood_cap,
    );

    let stderr_task = spawn_pipe_drain(
        stderr,
        Arc::clone(&shared_buf),
        Arc::clone(&flood_notify),
        child_exited_rx,
        flood_cap,
    );

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum StopTrigger {
        NormalExit(Option<i32>),
        Timeout,
        Deadline,
        Cancelled,
        OutputFloodCap,
    }

    let mut trigger = if *cancel_rx.borrow() {
        StopTrigger::Cancelled
    } else {
        tokio::select! {
            status_res = child.wait() => {
                let code = status_res.ok().and_then(|s| s.code());
                StopTrigger::NormalExit(code)
            }
            _ = tokio::time::sleep(timeout_duration) => {
                StopTrigger::Timeout
            }
            _ = async {
                match deadline {
                    Some(d) => tokio::time::sleep_until(d.tokio_instant()).await,
                    None => std::future::pending::<()>().await,
                }
            } => {
                StopTrigger::Deadline
            }
            _ = flood_notify.notified() => {
                StopTrigger::OutputFloodCap
            }
            _ = async {
                loop {
                    if cancel_rx.changed().await.is_err() {
                        std::future::pending::<()>().await;
                    }
                    if *cancel_rx.borrow() {
                        break;
                    }
                }
            } => {
                StopTrigger::Cancelled
            }
        }
    };

    let mut exit_code: Option<i32> = None;
    let mut terminated_by: Option<String> = None;

    match trigger {
        StopTrigger::NormalExit(code) => {
            exit_code = code;
        }
        StopTrigger::Timeout
        | StopTrigger::Deadline
        | StopTrigger::Cancelled
        | StopTrigger::OutputFloodCap => {
            terminated_by = Some(
                match trigger {
                    StopTrigger::Timeout => "Timeout",
                    StopTrigger::Deadline => "RunDeadlineExceeded",
                    StopTrigger::Cancelled => "Cancelled",
                    StopTrigger::OutputFloodCap => "OutputFloodCap",
                    StopTrigger::NormalExit(_) => unreachable!(),
                }
                .to_string(),
            );
            // A deadline stop must not be delayed by the full graceful grace period; the group is
            // still swept with SIGTERM-then-SIGKILL, just with a shorter grace.
            let grace = if trigger == StopTrigger::Deadline {
                grace_period.min(Duration::from_millis(200))
            } else {
                grace_period
            };
            proc_group.terminate_gracefully(&mut child, grace).await;
        }
    }

    // Signal reader tasks that the child has exited so each individual `reader.read` call
    // has a 200ms timeout (continuing to drain while data is available, and flushing incomplete last lines).
    let _ = child_exited_tx.send(true);
    let _ = stdout_task.await;
    let _ = stderr_task.await;

    // If the leader exited normally, sweep any lingering background processes in the process group / Job Object
    if matches!(trigger, StopTrigger::NormalExit(_)) {
        proc_group.cleanup_after_exit().await;
    }

    let buf_inner = {
        let mut guard = shared_buf.lock().unwrap();
        std::mem::replace(&mut *guard, HeadTailBuffer::new(0, 0, 1, 0, None))
    };

    if buf_inner.total_observed_bytes() > flood_cap && trigger != StopTrigger::OutputFloodCap {
        trigger = StopTrigger::OutputFloodCap;
        exit_code = None;
        terminated_by = Some("OutputFloodCap".to_string());
        proc_group
            .terminate_gracefully(&mut child, grace_period)
            .await;
    }

    // T10 (KR-17): a container workload is not the client process. Once the client is done (normal
    // exit, timeout, cancel or output cap) the workload is stopped, removed and verified — otherwise
    // a cancelled run leaves the real work running in the daemon.
    let container_note = match prepared.container.as_ref() {
        Some(workload) => {
            let outcome = workload.cancel(
                &crate::sandbox::ProcessProbeTransport,
                crate::sandbox::DEFAULT_CONTROL_BUDGET,
            );
            match outcome {
                crate::sandbox::CleanupOutcome::RecoveryRequired {
                    container_id,
                    reason,
                    retry_command,
                } => Some(format!(
                    "container cleanup requires recovery (id {}): {}; retry: {}",
                    container_id.unwrap_or_else(|| "<unknown>".to_string()),
                    reason,
                    retry_command
                )),
                _ => None,
            }
        }
        None => None,
    };

    let (mut body_output, truncated) = buf_inner.finalize();
    if let Some(note) = container_note {
        body_output.push_str("\n[");
        body_output.push_str(&note);
        body_output.push(']');
    }
    let duration_ms = started.elapsed().as_millis() as u64;

    let meta = ToolResultMeta {
        duration_ms,
        truncated,
        exit_code,
        terminated_by: terminated_by.clone(),
        execution_backend: Some(execution_backend),
        ..Default::default()
    };

    match trigger {
        StopTrigger::OutputFloodCap => ToolExecutionOutput::error(
            ErrorCode::OutputFloodCap,
            format!(
                "Process group terminated by OutputFloodCap (exceeded {} bytes) after {}ms.\n{}",
                budget.max_process_output_observed_bytes, duration_ms, body_output
            ),
            meta,
        ),
        StopTrigger::Timeout => ToolExecutionOutput::error(
            ErrorCode::ToolTimeout,
            format!(
                "Command timed out after {}s ({}ms) and process group was terminated.\n{}",
                timeout_sec, duration_ms, body_output
            ),
            meta,
        ),
        StopTrigger::Deadline => ToolExecutionOutput::error(
            ErrorCode::ToolTimeout,
            format!(
                "Run deadline reached ({}ms); command process group was terminated.\n{}",
                duration_ms, body_output
            ),
            meta,
        ),
        StopTrigger::Cancelled => ToolExecutionOutput::error(
            ErrorCode::NotExecutedCancelled,
            format!(
                "Command cancelled and process group was terminated after {}ms.\n{}",
                duration_ms, body_output
            ),
            meta,
        ),
        StopTrigger::NormalExit(code) => {
            let code_val = code.unwrap_or(-1);
            let header = format!(
                "[Command exit_code={} | duration_ms={}]",
                code_val, duration_ms
            );
            let full_text = if body_output.is_empty() {
                header
            } else {
                format!("{}\n{}", header, body_output)
            };
            if code_val == 0 {
                ToolExecutionOutput::success(full_text, meta)
            } else {
                ToolExecutionOutput::error(ErrorCode::IoError, full_text, meta)
            }
        }
    }
}
