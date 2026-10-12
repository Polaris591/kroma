use crate::budget::BudgetConfig;
use crate::messages::ToolResultMeta;
use crate::tool::output_buffer::{HeadTailBuffer, StreamLineCollector};
use crate::tools::ToolExecutionOutput;
use crate::types::{ErrorCode, ToolCallId};
use serde::Deserialize;
use std::path::Path;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::{Notify, watch};

pub const PIPE_DRAIN_TIMEOUT: Duration = Duration::from_millis(200);

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BashArgs {
    pub command: String,
    pub timeout_sec: Option<u64>,
}

/// Check if an environment variable name matches the sensitive blacklist (§5.4 V6-05):
/// `HARNESS_*`, `OPENAI_*`, `ANTHROPIC_*`, `*_API_KEY`, `*_SECRET`, `*_TOKEN`, `*_PASSWORD`, `*_CREDENTIAL*`.
pub fn is_blacklisted_env_var(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    upper.starts_with("HARNESS_")
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
}

pub fn collect_sanitized_env() -> Vec<(std::ffi::OsString, std::ffi::OsString)> {
    std::env::vars_os()
        .filter(|(k, _)| {
            let name = k.to_string_lossy();
            !is_blacklisted_env_var(&name)
        })
        .collect()
}

pub async fn execute_bash(
    workspace_root: &Path,
    state_dir: &Path,
    call_id: &ToolCallId,
    raw_args: &str,
    budget: &BudgetConfig,
    mut cancel_rx: watch::Receiver<bool>,
    grace_period: Duration,
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

    let shell_bin = if Path::new("/bin/bash").exists() {
        "/bin/bash"
    } else {
        "/bin/sh"
    };

    let mut cmd = Command::new(shell_bin);
    cmd.arg("-c")
        .arg(&args.command)
        .current_dir(workspace_root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env_clear()
        .envs(collect_sanitized_env());

    // Put child process into its own Unix process group (PGID = child PID)
    #[cfg(unix)]
    cmd.process_group(0);

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return ToolExecutionOutput::error(
                ErrorCode::IoError,
                format!("failed to spawn shell process: {}", e),
                ToolResultMeta::default(),
            );
        }
    };

    let pgid = child.id().unwrap_or(0) as i32;
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

    let stdout_task = {
        let buf = Arc::clone(&shared_buf);
        let notify = Arc::clone(&flood_notify);
        let mut exited_rx = child_exited_rx.clone();
        tokio::spawn(async move {
            let Some(mut reader) = stdout else { return };
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
                            let mut guard = buf.lock().unwrap();
                            guard.push_str(seg);
                            if guard.total_observed_bytes() > flood_cap {
                                exceeded = true;
                            }
                        });
                        if exceeded {
                            notify.notify_one();
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            collector.finish(|seg| {
                let mut guard = buf.lock().unwrap();
                guard.push_str(seg);
                if guard.total_observed_bytes() > flood_cap {
                    notify.notify_one();
                }
            });
        })
    };

    let stderr_task = {
        let buf = Arc::clone(&shared_buf);
        let notify = Arc::clone(&flood_notify);
        let mut exited_rx = child_exited_rx;
        tokio::spawn(async move {
            let Some(mut reader) = stderr else { return };
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
                            let mut guard = buf.lock().unwrap();
                            guard.push_str(seg);
                            if guard.total_observed_bytes() > flood_cap {
                                exceeded = true;
                            }
                        });
                        if exceeded {
                            notify.notify_one();
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            collector.finish(|seg| {
                let mut guard = buf.lock().unwrap();
                guard.push_str(seg);
                if guard.total_observed_bytes() > flood_cap {
                    notify.notify_one();
                }
            });
        })
    };

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum StopTrigger {
        NormalExit(Option<i32>),
        Timeout,
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
        StopTrigger::Timeout | StopTrigger::Cancelled | StopTrigger::OutputFloodCap => {
            terminated_by = Some(
                match trigger {
                    StopTrigger::Timeout => "Timeout",
                    StopTrigger::Cancelled => "Cancelled",
                    StopTrigger::OutputFloodCap => "OutputFloodCap",
                    StopTrigger::NormalExit(_) => unreachable!(),
                }
                .to_string(),
            );
            terminate_process_group(pgid, &mut child, grace_period).await;
        }
    }

    // Signal reader tasks that the child has exited so each individual `reader.read` call
    // has a 200ms timeout (continuing to drain while data is available, and flushing incomplete last lines).
    let _ = child_exited_tx.send(true);
    let _ = stdout_task.await;
    let _ = stderr_task.await;

    // If the leader exited normally, sweep any lingering background processes in `-pgid`
    if matches!(trigger, StopTrigger::NormalExit(_)) {
        cleanup_process_group_after_exit(pgid).await;
    }

    let buf_inner = {
        let mut guard = shared_buf.lock().unwrap();
        std::mem::replace(
            &mut *guard,
            HeadTailBuffer::new(0, 0, 1, 0, None),
        )
    };

    if buf_inner.total_observed_bytes() > flood_cap && trigger != StopTrigger::OutputFloodCap {
        trigger = StopTrigger::OutputFloodCap;
        exit_code = None;
        terminated_by = Some("OutputFloodCap".to_string());
        terminate_process_group(pgid, &mut child, grace_period).await;
    }

    let (body_output, truncated) = buf_inner.finalize();
    let duration_ms = started.elapsed().as_millis() as u64;

    let meta = ToolResultMeta {
        duration_ms,
        truncated,
        exit_code,
        terminated_by: terminated_by.clone(),
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

pub(crate) async fn cleanup_process_group_after_exit(pgid: i32) {
    #[cfg(unix)]
    if pgid > 1 {
        let alive = unsafe { libc::kill(-pgid, 0) == 0 };
        if alive {
            unsafe {
                libc::kill(-pgid, libc::SIGTERM);
            }
            let deadline = Instant::now() + Duration::from_millis(100);
            while Instant::now() < deadline {
                if unsafe { libc::kill(-pgid, 0) != 0 } {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            if unsafe { libc::kill(-pgid, 0) == 0 } {
                unsafe {
                    libc::kill(-pgid, libc::SIGKILL);
                }
                for _ in 0..5 {
                    if unsafe { libc::kill(-pgid, 0) != 0 } {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            }
        }
    }
}

async fn terminate_process_group(
    pgid: i32,
    child: &mut tokio::process::Child,
    grace_period: Duration,
) {
    #[cfg(unix)]
    if pgid > 1 {
        unsafe {
            libc::kill(-pgid, libc::SIGTERM);
        }
    }

    if tokio::time::timeout(grace_period, child.wait())
        .await
        .is_err()
    {
        #[cfg(unix)]
        if pgid > 1 {
            unsafe {
                libc::kill(-pgid, libc::SIGKILL);
            }
        }
        let _ = child.kill().await;
        let _ = child.wait().await;
    } else {
        // Even if the leader exited on SIGTERM, ensure any remaining processes in -pgid are cleaned up
        #[cfg(unix)]
        if pgid > 1 {
            unsafe {
                libc::kill(-pgid, libc::SIGKILL);
            }
        }
    }
}
