//! Public one-shot process supervision foundation and contract.
//!
//! Provides isolated process execution, sensitive environment sanitization,
//! output flood bounding, pipe drain timeouts, and cross-platform process tree
//! cleanup across timeouts, cancellations, flood caps, and normal exits.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;
use tokio::sync::watch;

use crate::budget::RunDeadline;
use crate::process_group::ChildProcessGroup;
use crate::process_launch::ProcessLaunch;

/// Execution environment sanitization and override policy. Moved to [`crate::process_launch`] and
/// re-exported here so the existing `crate::process_supervisor::EnvironmentPolicy` path (and the
/// one-shot and long-lived launch paths) keep resolving to one definition.
pub use crate::process_launch::{EnvironmentPolicy, ResourceLimits};

/// Byte-exact head + rolling-tail retention for a single output stream.
///
/// Keeps the first `head_cap` bytes and the last `tail_cap` bytes of the stream and counts every
/// byte it has seen, so a caller can tell whether the retained view is the whole stream. It is the
/// byte-oriented counterpart of `tool::output_buffer::HeadTailBuffer`; that buffer is a single,
/// line/String-oriented accumulator that owns its own spill file, while the supervisor exposes
/// `stdout` and `stderr` separately and has no per-stream spill target, so reusing it would either
/// merge the two streams or invent per-stream spill paths.
struct HeadTailBytes {
    head_cap: usize,
    tail_cap: usize,
    head: Vec<u8>,
    tail: VecDeque<u8>,
    observed: usize,
}

impl HeadTailBytes {
    fn new(head_cap: usize, tail_cap: usize) -> Self {
        Self {
            head_cap,
            tail_cap,
            head: Vec::with_capacity(head_cap.min(16 * 1024)),
            tail: VecDeque::new(),
            observed: 0,
        }
    }

    fn push(&mut self, chunk: &[u8]) {
        self.observed += chunk.len();
        let mut rest = chunk;
        if self.head.len() < self.head_cap {
            let take = (self.head_cap - self.head.len()).min(rest.len());
            self.head.extend_from_slice(&rest[..take]);
            rest = &rest[take..];
        }
        if rest.is_empty() || self.tail_cap == 0 {
            return;
        }
        for &byte in rest {
            self.tail.push_back(byte);
        }
        while self.tail.len() > self.tail_cap {
            self.tail.pop_front();
        }
    }

    /// True when the stream contained bytes that are not in the retained head + tail.
    fn is_truncated(&self) -> bool {
        self.observed > self.head.len() + self.tail.len()
    }

    fn finalize(self) -> Vec<u8> {
        if !self.is_truncated() {
            let mut out = self.head;
            out.extend(self.tail);
            return out;
        }
        let omitted = self.observed - self.head.len() - self.tail.len();
        let mut out = self.head;
        out.extend_from_slice(format!("\n\n[... {} bytes omitted ...]\n\n", omitted).as_bytes());
        out.extend(self.tail);
        out
    }
}

struct ProcessGroupGuard {
    group: Option<ChildProcessGroup>,
}

impl ProcessGroupGuard {
    fn new(group: ChildProcessGroup) -> Self {
        Self { group: Some(group) }
    }
    fn group(&self) -> &ChildProcessGroup {
        self.group.as_ref().expect("guard active")
    }
    fn disarm(mut self) -> ChildProcessGroup {
        self.group.take().expect("guard active")
    }
}

impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        if let Some(ref group) = self.group {
            group.terminate_immediately();
        }
    }
}

/// Reason for process termination.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TerminationReason {
    Exit,
    Timeout,
    Deadline,
    Cancelled,
    OutputFlood,
}

/// Specification for a supervised one-shot command execution.
#[derive(Debug, Clone)]
pub struct OneShotCommandSpec {
    pub argv: Vec<String>,
    pub cwd: PathBuf,
    pub env_policy: EnvironmentPolicy,
    pub timeout: Duration,
    pub deadline: Option<RunDeadline>,
    pub max_output_bytes: usize,
    pub head_bytes: usize,
    pub tail_bytes: usize,
    /// Per-process kernel limits applied to the child before `exec` (see [`ResourceLimits`]).
    pub rlimits: Option<ResourceLimits>,
}

impl OneShotCommandSpec {
    pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);
    pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 8 * 1024 * 1024; // 8 MiB
    pub const DEFAULT_HEAD_BYTES: usize = 16 * 1024; // 16 KiB
    pub const DEFAULT_TAIL_BYTES: usize = 48 * 1024; // 48 KiB

    pub fn new(argv: Vec<String>, cwd: impl Into<PathBuf>) -> Self {
        Self {
            argv,
            cwd: cwd.into(),
            env_policy: EnvironmentPolicy::SanitizedMinimal,
            timeout: Self::DEFAULT_TIMEOUT,
            deadline: None,
            max_output_bytes: Self::DEFAULT_MAX_OUTPUT_BYTES,
            head_bytes: Self::DEFAULT_HEAD_BYTES,
            tail_bytes: Self::DEFAULT_TAIL_BYTES,
            rlimits: None,
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn with_deadline(mut self, deadline: Option<RunDeadline>) -> Self {
        self.deadline = deadline;
        self
    }

    pub fn with_env_policy(mut self, env_policy: EnvironmentPolicy) -> Self {
        self.env_policy = env_policy;
        self
    }

    pub fn with_max_output_bytes(mut self, max: usize) -> Self {
        self.max_output_bytes = max;
        self
    }

    pub fn with_head_tail(mut self, head: usize, tail: usize) -> Self {
        self.head_bytes = head;
        self.tail_bytes = tail;
        self
    }

    /// Bound the child's kernel resources (address space, CPU time, file size, open files).
    pub fn with_rlimits(mut self, limits: ResourceLimits) -> Self {
        self.rlimits = Some(limits);
        self
    }

    pub fn with_execution_context(mut self, ctx: &crate::env::ExecutionContext) -> Self {
        if self.deadline.is_none() {
            self.deadline = ctx.deadline;
        }
        self
    }
}

/// Result of a supervised one-shot command execution.
#[derive(Debug, Clone)]
pub struct OneShotExecutionResult {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    /// The retained stdout/stderr were trimmed to the head + tail window for display. This is a
    /// presentation concern only and never affects [`Self::is_success`].
    pub output_trimmed: bool,
    /// The captured output is not the complete output the child produced: the run stopped before
    /// both pipes reached EOF (flood cap, drain deadline, timeout, run deadline, or cancellation).
    pub truncated: bool,
    pub terminated_by: Option<TerminationReason>,
    pub duration: Duration,
}

impl OneShotExecutionResult {
    /// True when the child exited 0 *and* its output was captured completely.
    ///
    /// Display-only head/tail trimming (`output_trimmed`) is orthogonal, so a successful command
    /// with large output is still a success, matching `bash`. Only a run whose capture was cut
    /// short (`truncated`) is not a success.
    pub fn is_success(&self) -> bool {
        self.exit_code == Some(0)
            && matches!(self.terminated_by, None | Some(TerminationReason::Exit))
            && !self.truncated
    }

    pub fn combined_output(&self) -> String {
        if self.stderr.is_empty() {
            self.stdout.clone()
        } else if self.stdout.is_empty() {
            self.stderr.clone()
        } else {
            format!("{}\n{}", self.stdout, self.stderr)
        }
    }
}

/// One-shot process supervisor.
pub struct ProcessSupervisor;

impl ProcessSupervisor {
    pub async fn execute_one_shot(
        spec: OneShotCommandSpec,
        mut cancel_rx: Option<watch::Receiver<bool>>,
    ) -> Result<OneShotExecutionResult, std::io::Error> {
        let start = Instant::now();
        if spec.argv.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "argv must not be empty",
            ));
        }

        // Pre-cancellation check (zero spawn)
        if let Some(ref rx) = cancel_rx {
            if *rx.borrow() {
                return Ok(OneShotExecutionResult {
                    exit_code: None,
                    stdout: String::new(),
                    stderr: String::new(),
                    output_trimmed: false,
                    truncated: false,
                    terminated_by: Some(TerminationReason::Cancelled),
                    duration: Duration::ZERO,
                });
            }
        }

        // Pre-deadline check (zero spawn)
        if let Some(ref deadline) = spec.deadline {
            if deadline.is_exceeded() {
                return Ok(OneShotExecutionResult {
                    exit_code: None,
                    stdout: String::new(),
                    stderr: String::new(),
                    output_trimmed: false,
                    truncated: false,
                    terminated_by: Some(TerminationReason::Deadline),
                    duration: Duration::ZERO,
                });
            }
        }

        let launch = ProcessLaunch::new(spec.argv[0].clone(), spec.argv[1..].to_vec())
            .cwd(spec.cwd.clone())
            .env_policy(spec.env_policy.clone())
            .stdio(
                std::process::Stdio::null(),
                std::process::Stdio::piped(),
                std::process::Stdio::piped(),
            );
        let launch = match spec.rlimits {
            Some(limits) => launch.rlimits(limits),
            None => launch,
        };
        let (mut child, group) = launch.spawn_tokio()?;
        let guard = ProcessGroupGuard::new(group);

        let mut stdout = child.stdout.take();
        let mut stderr = child.stderr.take();

        let mut stdout_buf = HeadTailBytes::new(spec.head_bytes, spec.tail_bytes);
        let mut stderr_buf = HeadTailBytes::new(spec.head_bytes, spec.tail_bytes);
        let mut total_observed = 0usize;
        let mut truncated = false;
        let mut terminated_by = None;

        let mut stdout_chunk = [0u8; 8192];
        let mut stderr_chunk = [0u8; 8192];
        let mut stdout_done = stdout.is_none();
        let mut stderr_done = stderr.is_none();
        let mut child_exited = false;
        let mut exit_status = None;

        let timeout_sleep = tokio::time::sleep(spec.timeout);
        tokio::pin!(timeout_sleep);

        let has_deadline = spec.deadline.is_some();
        let deadline_sleep = match spec.deadline {
            Some(ref dl) => tokio::time::sleep_until(dl.tokio_instant()),
            None => tokio::time::sleep(Duration::from_secs(86400 * 365)),
        };
        tokio::pin!(deadline_sleep);

        let pipe_drain_sleep = tokio::time::sleep(Duration::from_secs(86400 * 365));
        tokio::pin!(pipe_drain_sleep);
        let mut pipe_drain_active = false;

        loop {
            if child_exited && (stdout_done && stderr_done) {
                break;
            }
            // Combined flood cap: checked before the next read allocates anything, so retained
            // output can never grow past the budget by one chunk (`+ 8192` slack is gone).
            if total_observed > spec.max_output_bytes {
                truncated = true;
                terminated_by = Some(TerminationReason::OutputFlood);
                break;
            }

            tokio::select! {
                // Cancellation signal
                _ = async {
                    if let Some(ref mut rx) = cancel_rx {
                        while rx.changed().await.is_ok() {
                            if *rx.borrow() {
                                return;
                            }
                        }
                    }
                    std::future::pending::<()>().await;
                }, if cancel_rx.is_some() => {
                    terminated_by = Some(TerminationReason::Cancelled);
                    if !stdout_done || !stderr_done {
                        truncated = true;
                    }
                    break;
                }

                // Command timeout
                _ = &mut timeout_sleep => {
                    terminated_by = Some(TerminationReason::Timeout);
                    if !stdout_done || !stderr_done {
                        truncated = true;
                    }
                    break;
                }

                // Deadline
                _ = &mut deadline_sleep, if has_deadline => {
                    terminated_by = Some(TerminationReason::Deadline);
                    if !stdout_done || !stderr_done {
                        truncated = true;
                    }
                    break;
                }

                // Pipe drain deadline after child process exit. The leader exited, but if either
                // pipe has not reached EOF we stopped reading before the stream ended, so the
                // captured output is incomplete and must not be reported as a clean success.
                _ = &mut pipe_drain_sleep, if pipe_drain_active => {
                    if !stdout_done || !stderr_done {
                        truncated = true;
                    }
                    break;
                }

                // Child process status
                status = child.wait(), if !child_exited => {
                    match status {
                        Ok(st) => {
                            child_exited = true;
                            exit_status = Some(st);
                            if stdout_done && stderr_done {
                                break;
                            }
                            // Start pipe drain timeout (200ms)
                            pipe_drain_sleep.as_mut().reset(tokio::time::Instant::now() + Duration::from_millis(200));
                            pipe_drain_active = true;
                        }
                        Err(e) => {
                            return Err(e);
                        }
                    }
                }

                // Stdout stream
                res = async {
                    match stdout.as_mut() {
                        Some(reader) => reader.read(&mut stdout_chunk).await,
                        None => std::future::pending().await,
                    }
                }, if !stdout_done => {
                    match res {
                        Ok(0) => {
                            stdout_done = true;
                        }
                        Ok(n) => {
                            total_observed += n;
                            // Retention is bounded by head + tail, so this only records the byte
                            // count; the top-of-loop check decides whether the cap was crossed.
                            stdout_buf.push(&stdout_chunk[..n]);
                        }
                        Err(_) => {
                            stdout_done = true;
                        }
                    }
                }

                // Stderr stream
                res = async {
                    match stderr.as_mut() {
                        Some(reader) => reader.read(&mut stderr_chunk).await,
                        None => std::future::pending().await,
                    }
                }, if !stderr_done => {
                    match res {
                        Ok(0) => {
                            stderr_done = true;
                        }
                        Ok(n) => {
                            total_observed += n;
                            stderr_buf.push(&stderr_chunk[..n]);
                        }
                        Err(_) => {
                            stderr_done = true;
                        }
                    }
                }
            }
        }

        // Wrap-up flood backstop (mirrors `bash`): if the loop exited because the child had
        // finished and both pipes had reached EOF while the combined output already crossed the cap,
        // never report that as a clean, complete exit.
        if total_observed > spec.max_output_bytes
            && terminated_by != Some(TerminationReason::OutputFlood)
        {
            truncated = true;
            terminated_by = Some(TerminationReason::OutputFlood);
        }

        // Head/tail retention trims the display only; it is orthogonal to capture completeness
        // (`truncated`) and to `is_success`.
        let output_trimmed = stdout_buf.is_truncated() | stderr_buf.is_truncated();
        let stdout_bytes = stdout_buf.finalize();
        let stderr_bytes = stderr_buf.finalize();

        if let Some(reason) = terminated_by {
            guard.group().terminate_immediately();
            let _ = child.kill().await;
            let _ = child.wait().await;
            let group = guard.disarm();
            group.cleanup_after_exit().await;

            return Ok(OneShotExecutionResult {
                exit_code: if reason == TerminationReason::Exit {
                    exit_status.and_then(|s| s.code())
                } else {
                    None
                },
                stdout: String::from_utf8_lossy(&stdout_bytes).to_string(),
                stderr: String::from_utf8_lossy(&stderr_bytes).to_string(),
                output_trimmed,
                truncated,
                terminated_by: Some(reason),
                duration: start.elapsed(),
            });
        }

        let group = guard.disarm();
        group.cleanup_after_exit().await;

        Ok(OneShotExecutionResult {
            exit_code: exit_status.and_then(|s| s.code()),
            stdout: String::from_utf8_lossy(&stdout_bytes).to_string(),
            stderr: String::from_utf8_lossy(&stderr_bytes).to_string(),
            output_trimmed,
            truncated,
            terminated_by: Some(TerminationReason::Exit),
            duration: start.elapsed(),
        })
    }

    pub async fn execute_one_shot_with_context(
        spec: OneShotCommandSpec,
        ctx: &crate::env::ExecutionContext,
    ) -> Result<OneShotExecutionResult, std::io::Error> {
        let cancel = ctx.cancel.clone();
        let spec = spec.with_execution_context(ctx);
        Self::execute_one_shot(spec, cancel).await
    }
}
