//! Completion verification gate and bounded failure localization.

use crate::env::{CompletionGate, CompletionGateContext, GateOutcome};
use crate::process_group::ChildProcessGroup;
use crate::tool::output_buffer::{HeadTailBuffer, StreamLineCollector};
use crate::tools::bash::PIPE_DRAIN_TIMEOUT;
use sha2::{Digest, Sha256};
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::watch;

const HEAD_BYTES: usize = 16 * 1024;
const TAIL_BYTES: usize = 48 * 1024;
const DEFAULT_TIMEOUT_SECS: u64 = 180;

/// Runs a project verification command before the agent claims completion.
///
/// The command is argv-based (not shell-evaluated). If no explicit command is supplied, a small
/// set of conventional project manifests is detected. Unknown project types are reported as
/// skipped, rather than pretending that tests ran.
#[derive(Debug, Clone)]
pub struct WorkspaceCompletionGate {
    command: Option<Vec<String>>,
    timeout: Duration,
    /// Whether a command that was *auto-detected* from project manifests (rather than supplied
    /// explicitly via `--verify-cmd` / `KROMA_VERIFY_COMMAND`) may be executed. Running a
    /// repository's own test script is arbitrary host execution, so it is gated behind the same
    /// `--allow-shell` authorization used for the `bash` tool.
    allow_auto_detected: bool,
}

impl WorkspaceCompletionGate {
    /// Explicit command (or no command with auto-detection permitted, kept for callers that have
    /// already applied their own authorization policy).
    pub fn new(command: Option<Vec<String>>, timeout: Duration) -> Self {
        Self::with_auto_detection(command, timeout, true)
    }

    pub fn with_auto_detection(command: Option<Vec<String>>, timeout: Duration, allow_auto_detected: bool) -> Self {
        let command = command.filter(|argv| !argv.is_empty() && !argv[0].trim().is_empty());
        Self { command, timeout, allow_auto_detected }
    }

    pub fn from_environment() -> Self {
        let configured = std::env::var("KROMA_VERIFY_COMMAND").ok().and_then(|raw| {
            parse_command_argv(&raw).ok().filter(|argv| !argv.is_empty())
        });
        Self::new(configured, configured_timeout())
    }

    fn select_command(&self, root: &Path) -> Option<Vec<String>> {
        if let Some(argv) = &self.command {
            return Some(argv.clone());
        }
        if root.join("Cargo.toml").is_file() {
            Some(vec!["cargo".into(), "test".into(), "--workspace".into(), "--no-fail-fast".into()])
        } else if root.join("go.mod").is_file() {
            Some(vec!["go".into(), "test".into(), "./...".into()])
        } else if root.join("pyproject.toml").is_file() || root.join("pytest.ini").is_file() {
            Some(vec!["python".into(), "-m".into(), "pytest".into(), "-q".into()])
        } else if root.join("package.json").is_file() {
            Some(vec!["npm".into(), "test".into()])
        } else {
            None
        }
    }
}

impl Default for WorkspaceCompletionGate {
    fn default() -> Self { Self::from_environment() }
}

impl CompletionGate for WorkspaceCompletionGate {
    fn check<'a>(
        &'a self,
        ctx: &'a CompletionGateContext,
    ) -> Pin<Box<dyn Future<Output = Result<GateOutcome, String>> + Send + 'a>> {
        Box::pin(async move {
            // Dirty-file short circuit: no changed files means no test process should be spawned.
            if ctx.modified_files.is_empty() {
                return Ok(GateOutcome::Approved);
            }
            // Auto-detected project test scripts are untrusted code from the target repository.
            // Only run them when the operator explicitly authorized unattended shell execution;
            // an explicit command (--verify-cmd / KROMA_VERIFY_COMMAND) is its own authorization.
            if self.command.is_none() && !self.allow_auto_detected {
                return Ok(GateOutcome::Skipped {
                    reason: "auto-detected verification requires --allow-shell or explicit --verify-cmd / KROMA_VERIFY_COMMAND".into(),
                });
            }
            let Some(argv) = self.select_command(&ctx.workspace_root) else {
                return Ok(GateOutcome::Skipped {
                    reason: "no supported test manifest found; set KROMA_VERIFY_COMMAND to enforce verification".into(),
                });
            };

            let mut command = Command::new(&argv[0]);
            command.args(&argv[1..])
                .current_dir(&ctx.workspace_root)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .env_clear()
                .envs(crate::tools::bash::collect_sanitized_env())
                .env("CI", "true")
                .env("NO_COLOR", "1")
                .kill_on_drop(true);
            // Verification is arbitrary project code. Give it its own process group / Windows Job Object
            // so timeout cleanup can terminate descendants, and use the same secret-env filtering as `bash`.
            ChildProcessGroup::configure_tokio_command(&mut command);

            let mut child = match command.spawn() {
                Ok(child) => child,
                Err(err) => return Ok(GateOutcome::Rejected {
                    feedback: format!("verification command could not execute ({}): {}", display_argv(&argv), err),
                }),
            };
            let proc_group = ChildProcessGroup::from_tokio_child(&child);
            let stdout = child.stdout.take();
            let stderr = child.stderr.take();

            // Stream both pipes into bounded shared buffers on background tasks so a grandchild that
            // inherits the pipe handles cannot block the gate. Mirroring `tools/bash.rs`, the reader
            // tasks block indefinitely while the child is alive and switch to a per-read 200ms drain
            // timeout once the leader exits.
            let (child_exited_tx, child_exited_rx) = watch::channel(false);
            let (stdout_task, stdout_buf) = spawn_stream_collector(stdout, child_exited_rx.clone());
            let (stderr_task, stderr_buf) = spawn_stream_collector(stderr, child_exited_rx);

            let status = match tokio::time::timeout(self.timeout, child.wait()).await {
                Ok(Ok(status)) => status,
                Ok(Err(err)) => {
                    let _ = child_exited_tx.send(true);
                    let _ = stdout_task.await;
                    let _ = stderr_task.await;
                    proc_group.cleanup_after_exit().await;
                    return Ok(GateOutcome::Rejected {
                        feedback: format!("verification command failed ({}): {}", display_argv(&argv), err),
                    });
                }
                Err(_) => {
                    let _ = child_exited_tx.send(true);
                    proc_group.terminate_immediately();
                    let _ = child.start_kill();
                    let _ = child.wait().await;
                    let _ = stdout_task.await;
                    let _ = stderr_task.await;
                    proc_group.cleanup_after_exit().await;
                    return Ok(GateOutcome::Rejected {
                        feedback: format!("verification timed out after {} seconds: {}", self.timeout.as_secs(), display_argv(&argv)),
                    });
                }
            };

            // The leader has exited: give each collector up to 200ms to reach EOF (or flush the last
            // partial line), then unconditionally sweep any lingering background descendants in
            // the process group / Job Object so a test suite cannot leave orphan processes behind after the gate returns.
            let _ = child_exited_tx.send(true);
            let _ = stdout_task.await;
            let _ = stderr_task.await;
            proc_group.cleanup_after_exit().await;

            let stdout_text = finalize_stream(stdout_buf);
            let stderr_text = finalize_stream(stderr_buf);

            if status.success() {
                return Ok(GateOutcome::Approved);
            }

            // Localize stdout and stderr independently before combining so neither stream's
            // tail diagnostics or root-cause head errors are lost when both streams are large.
            let mut localized = String::new();
            if !stdout_text.is_empty() {
                localized.push_str("STDOUT:\n");
                localized.push_str(&localize_failure(&stdout_text));
            }
            if !stderr_text.is_empty() {
                if !localized.is_empty() {
                    localized.push('\n');
                }
                localized.push_str("STDERR:\n");
                localized.push_str(&localize_failure(&stderr_text));
            }
            Ok(GateOutcome::Rejected {
                feedback: format!(
                    "verification command `{}` exited with {}.\nChanged files: {}\n{}",
                    display_argv(&argv),
                    status,
                    ctx.modified_files.join(", "),
                    localized,
                ),
            })
        })
    }
}

/// Drain one child pipe on a background task into a bounded shared buffer.
///
/// The reader blocks without a timeout while the child is alive, then switches to a bounded
/// total `PIPE_DRAIN_TIMEOUT` deadline once the leader has exited. This lets us harvest a
/// complete stream that a lingering grandchild kept open without ever hanging on trickle I/O.
fn spawn_stream_collector<R>(
    reader: Option<R>,
    mut exited_rx: watch::Receiver<bool>,
) -> (tokio::task::JoinHandle<()>, Arc<Mutex<HeadTailBuffer>>)
where
    R: AsyncRead + Unpin + Send + 'static,
{
    // Reuse the shared 16 KiB head + 48 KiB rolling tail implementation. No spill path is
    // configured here, so even pathological test output stays memory-bounded for the gate.
    let buffer = Arc::new(Mutex::new(HeadTailBuffer::new(HEAD_BYTES, TAIL_BYTES, 100_000, 0, None)));
    let handle = {
        let buffer = Arc::clone(&buffer);
        tokio::spawn(async move {
            let Some(mut reader) = reader else { return; };
            let mut collector = StreamLineCollector::new(2048);
            let mut bytes = [0u8; 8192];
            let mut drain_deadline: Option<tokio::time::Instant> = None;
            loop {
                let read_result = if *exited_rx.borrow() {
                    let deadline = *drain_deadline
                        .get_or_insert_with(|| tokio::time::Instant::now() + PIPE_DRAIN_TIMEOUT);
                    match tokio::time::timeout_at(deadline, reader.read(&mut bytes)).await {
                        Ok(result) => result,
                        Err(_) => break,
                    }
                } else {
                    tokio::select! {
                        result = reader.read(&mut bytes) => result,
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
                            match tokio::time::timeout_at(deadline, reader.read(&mut bytes)).await {
                                Ok(result) => result,
                                Err(_) => break,
                            }
                        }
                    }
                };
                match read_result {
                    Ok(0) => break,
                    Ok(n) => collector.push_chunk(&bytes[..n], |segment| buffer.lock().unwrap().push_str(segment)),
                    Err(_) => break,
                }
            }
            collector.finish(|segment| buffer.lock().unwrap().push_str(segment));
        })
    };
    (handle, buffer)
}

fn finalize_stream(buffer: Arc<Mutex<HeadTailBuffer>>) -> String {
    let mut guard = buffer.lock().unwrap();
    std::mem::replace(&mut *guard, HeadTailBuffer::new(0, 0, 1, 0, None)).finalize().0
}

fn display_argv(argv: &[String]) -> String { argv.join(" ") }

/// Completion-check timeout from `KROMA_VERIFY_TIMEOUT_SECS`, falling back to the 180s default.
pub fn configured_timeout() -> Duration {
    let timeout_secs = std::env::var("KROMA_VERIFY_TIMEOUT_SECS")
        .ok().and_then(|s| s.parse::<u64>().ok()).filter(|s| *s > 0)
        .unwrap_or(DEFAULT_TIMEOUT_SECS);
    Duration::from_secs(timeout_secs)
}

/// Split a verification command string into argv, honoring single/double quotes and `\` escapes.
///
/// Whitespace inside quotes is preserved so `sh -c "cargo test --workspace"` becomes
/// `["sh", "-c", "cargo test --workspace"]` rather than being split on the quoted spaces.
pub fn parse_command_argv(raw: &str) -> Result<Vec<String>, String> {
    let mut argv = Vec::new();
    let mut current = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut has_token = false;
    let mut chars = raw.chars();

    while let Some(c) = chars.next() {
        if in_single {
            if c == '\'' {
                in_single = false;
            } else {
                current.push(c);
            }
        } else if in_double {
            match c {
                '"' => in_double = false,
                '\\' => match chars.next() {
                    // Inside double quotes a backslash only escapes the shell-significant set.
                    Some(next @ ('"' | '\\' | '$' | '`')) => current.push(next),
                    Some(other) => {
                        current.push('\\');
                        current.push(other);
                    }
                    None => current.push('\\'),
                },
                other => current.push(other),
            }
        } else {
            match c {
                '\'' => in_single = true,
                '"' => in_double = true,
                '\\' => match chars.next() {
                    Some(next) => current.push(next),
                    None => return Err("trailing backslash in verification command".into()),
                },
                c if c.is_whitespace() => {
                    if has_token {
                        argv.push(std::mem::take(&mut current));
                        has_token = false;
                    }
                    continue;
                }
                other => current.push(other),
            }
        }
        has_token = true;
    }

    if in_single || in_double {
        return Err("unbalanced quote in verification command".into());
    }
    if has_token {
        argv.push(current);
    }
    Ok(argv)
}

/// Normalize a diagnostic line by stripping volatile elapsed durations (`0.42s`, `15ms`),
/// source line/column suffixes (`:42:5`), and hex memory addresses (`0x7fff...`) while keeping
/// semantic assertion values intact.
fn canonicalize_diagnostic_line(line: &str) -> String {
    let mut normalized_tokens = Vec::new();
    for raw_tok in line.split_whitespace() {
        let trimmed = raw_tok.trim_end_matches(|c: char| matches!(c, ',' | ';' | ')'));
        // Strip duration tokens like `0.42s`, `12ms`, `(0.01s)`
        let inner = trimmed.trim_start_matches('(');
        if let Some(num_part) = inner.strip_suffix("ms").or_else(|| inner.strip_suffix('s')) {
            if !num_part.is_empty() && num_part.chars().all(|c| c.is_ascii_digit() || c == '.') && num_part.chars().any(|c| c.is_ascii_digit()) {
                normalized_tokens.push("<duration>".to_string());
                continue;
            }
        }
        // Strip hex addresses like `0x7fff1234abcd`
        if let Some(hex) = inner.strip_prefix("0x").or_else(|| inner.strip_prefix("0X")) {
            if !hex.is_empty() && hex.chars().all(|c| c.is_ascii_hexdigit()) {
                normalized_tokens.push("<hex>".to_string());
                continue;
            }
        }
        // Normalize `file.rs:42:5` or `file.rs:42` location suffixes
        if let Some(colon_idx) = raw_tok.find(':') {
            let (prefix, rest) = raw_tok.split_at(colon_idx);
            let coords = rest.trim_start_matches(':').trim_end_matches(':');
            if !prefix.is_empty()
                && !coords.is_empty()
                && coords.split(':').all(|seg| !seg.is_empty() && seg.chars().all(|c| c.is_ascii_digit()))
            {
                normalized_tokens.push(format!("{}:<loc>", prefix));
                continue;
            }
        }
        normalized_tokens.push(raw_tok.to_string());
    }
    normalized_tokens.join(" ")
}

/// Stable signature for the anti-oscillation breaker; hashes localized output only.
pub fn failure_fingerprint(feedback: &str) -> String {
    // Sample from the tail first so passing tests near the top of STDOUT cannot crowd out the
    // 64-line budget, normalize volatile timings/line-numbers, and sort for parallel stability.
    let mut relevant: Vec<String> = feedback
        .lines()
        .rev()
        .filter(|line| {
            let line = line.to_ascii_lowercase();
            [
                "error", "failed", "failure", "panicked", "assertion", "traceback",
                "test result", "not ok", "expected", "actual", "--- fail:", "fail\t",
            ]
            .iter()
            .any(|needle| line.contains(needle))
        })
        .take(64)
        .map(canonicalize_diagnostic_line)
        .collect();
    relevant.sort_unstable();
    relevant.dedup();
    let normalized = if relevant.is_empty() {
        canonicalize_diagnostic_line(feedback)
    } else {
        relevant.join("\n")
    };
    format!("{:x}", Sha256::digest(normalized.as_bytes()))
}

/// Keep the first 16 KiB and rolling last 48 KiB; prioritize a small set of failure lines from
/// the retained tail without dropping the original tail context needed to inspect stack traces.
pub fn localize_failure(output: &str) -> String {
    let total = output.len();
    let head_end = floor_char_boundary(output, HEAD_BYTES.min(total));
    let tail_start = ceil_char_boundary(output, total.saturating_sub(TAIL_BYTES));
    if total <= HEAD_BYTES + TAIL_BYTES {
        return output.to_string();
    }

    let head = &output[..head_end];
    let tail = &output[tail_start..];
    let mut highlights = Vec::new();
    let mut highlight_bytes = 0usize;
    const MAX_HIGHLIGHT_BYTES: usize = 8 * 1024;
    let mut context_after = 0usize;
    for line in tail.lines() {
        let l = line.to_ascii_lowercase();
        let is_diag = [
            "error", "failed", "failure", "panicked", "assertion", "traceback",
            "test result", "not ok", "--- fail:", "fail\t",
        ]
        .iter()
        .any(|needle| l.contains(needle));
        if is_diag {
            context_after = 2;
        }
        if is_diag || context_after > 0 {
            if !is_diag {
                context_after = context_after.saturating_sub(1);
            }
            let remaining = MAX_HIGHLIGHT_BYTES.saturating_sub(highlight_bytes);
            if remaining == 0 { break; }
            let end = floor_char_boundary(line, line.len().min(remaining));
            if end == 0 { continue; }
            let excerpt = line[..end].to_string();
            if highlights.last().map(|last: &String| last != &excerpt).unwrap_or(true) {
                highlights.push(excerpt);
                highlight_bytes = highlight_bytes.saturating_add(end + 1);
            }
        }
        if highlight_bytes >= MAX_HIGHLIGHT_BYTES { break; }
    }
    let mut result = String::new();
    result.push_str(head);
    result.push_str(&format!("\n\n[CompletionGate omitted {} bytes; retained 16 KiB head + 48 KiB tail]\n\n", total.saturating_sub(head_end + (total - tail_start))));
    if !highlights.is_empty() {
        result.push_str("Failure-focused lines from the retained tail:\n");
        for line in highlights { result.push_str(&line); result.push('\n'); }
        result.push_str("\nFull retained tail:\n");
    }
    result.push_str(tail);
    result
}

fn floor_char_boundary(s: &str, mut i: usize) -> usize {
    while i > 0 && !s.is_char_boundary(i) { i -= 1; }
    i
}
fn ceil_char_boundary(s: &str, mut i: usize) -> usize {
    while i < s.len() && !s.is_char_boundary(i) { i += 1; }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn localizer_keeps_head_tail_and_finds_failure_lines() {
        let mut output = "beginning context\n".to_string();
        output.push_str(&"x".repeat(70 * 1024));
        output.push_str("\nerror[E0308]: mismatched types\nstack trace tail\n");
        let localized = localize_failure(&output);
        assert!(localized.starts_with("beginning context"));
        assert!(localized.contains("error[E0308]"));
        assert!(localized.contains("stack trace tail"));
        assert!(localized.contains("omitted"));
        assert!(localized.len() < output.len());
    }

    #[test]
    fn failure_fingerprint_ignores_whitespace_only_differences() {
        assert_eq!(failure_fingerprint("error: foo\n"), failure_fingerprint("error:   foo"));
        assert_ne!(failure_fingerprint("error foo"), failure_fingerprint("error bar"));
    }

    #[test]
    fn failure_fingerprint_ignores_progress_noise_but_keeps_diagnostic_changes() {
        assert_eq!(
            failure_fingerprint("compile unit 123\nerror: expected 1, got 2"),
            failure_fingerprint("compile unit 987\nerror: expected 1, got 2")
        );
        assert_ne!(
            failure_fingerprint("compile unit 123\nerror: expected 1, got 2"),
            failure_fingerprint("compile unit 987\nerror: expected 1, got 3")
        );
        assert_eq!(
            failure_fingerprint("panicked at src/lib.rs:42:5:\nassertion failed\ntest result: FAILED. 1 failed; finished in 0.42s"),
            failure_fingerprint("panicked at src/lib.rs:108:9:\nassertion failed\ntest result: FAILED. 1 failed; finished in 1.87s")
        );
    }

    #[tokio::test]
    async fn streaming_capture_keeps_a_bounded_head_and_rolling_tail() {
        use tokio::io::{duplex, AsyncWriteExt};
        let (mut writer, reader) = duplex(4096);
        let writer_task = tokio::spawn(async move {
            for i in 0..5000 {
                writer.write_all(format!("line-{i:05} padding 01234567890123456789\n").as_bytes()).await.unwrap();
            }
            writer.write_all(b"error: final assertion failed\n").await.unwrap();
        });
        let (_exited_tx, exited_rx) = watch::channel(false);
        let (collector, buffer) = spawn_stream_collector(Some(reader), exited_rx);
        collector.await.unwrap();
        let retained = finalize_stream(buffer);
        writer_task.await.unwrap();
        assert!(retained.starts_with("line-00000"));
        assert!(retained.contains("Only the head and rolling tail were retained"));
        assert!(retained.contains("error: final assertion failed"));
        assert!(retained.len() < 70 * 1024);
    }

    #[tokio::test]
    async fn gate_200ms_pipe_drain_prevents_grandchild_hang_and_sweeps_pgid() {
        use crate::env::{CompletionGateContext, GateOutcome};
        use crate::types::RunId;
        use std::time::Instant;
        let root = std::env::temp_dir().join(format!("kroma-gate-drain-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        // The backgrounded `sleep 30` inherits stdout/stderr and keeps both pipes open long after
        // the shell leader exits. Without the post-exit drain timeout the gate would block on pipe
        // EOF until the 5s total timeout; with it, the gate must return well under 1s.
        #[cfg(unix)]
        let cmd_vec = vec![
            "sh".into(),
            "-c".into(),
            "sleep 30 & printf 'all tests passed\n'; exit 0".into(),
        ];
        #[cfg(windows)]
        let cmd_vec = vec![
            "cmd.exe".into(),
            "/C".into(),
            "echo all tests passed & exit /B 0".into(),
        ];
        let gate = WorkspaceCompletionGate::new(Some(cmd_vec), Duration::from_secs(5));
        let ctx = CompletionGateContext {
            run_id: RunId::new(), workspace_root: root.clone(), modified_files: vec!["src/lib.rs".into()], turns_used: 1,
        };
        let started = Instant::now();
        let outcome = gate.check(&ctx).await.unwrap();
        let elapsed = started.elapsed();
        assert_eq!(outcome, GateOutcome::Approved, "gate should approve the fast exit");
        assert!(elapsed < Duration::from_secs(1), "gate took {:?}; the 200ms pipe drain did not bound the grandchild hang", elapsed);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn auto_detected_verification_without_shell_authorization_returns_skipped() {
        use crate::env::{CompletionGateContext, GateOutcome};
        use crate::types::RunId;
        let root = std::env::temp_dir().join(format!("kroma-gate-auto-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("package.json"), "{\"name\":\"untrusted\"}\n").unwrap();
        // No explicit command and auto-detection not authorized: the gate must not silently run
        // the repository's own `npm test`; it reports a skip instead.
        let gate = WorkspaceCompletionGate::with_auto_detection(None, Duration::from_secs(3), false);
        let ctx = CompletionGateContext {
            run_id: RunId::new(), workspace_root: root.clone(), modified_files: vec!["src/index.js".into()], turns_used: 1,
        };
        match gate.check(&ctx).await.unwrap() {
            GateOutcome::Skipped { reason } => assert!(reason.contains("--allow-shell")),
            other => panic!("expected skipped gate outcome, got {:?}", other),
        }
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn parse_command_argv_preserves_quoted_segments_and_escapes() {
        assert_eq!(
            parse_command_argv("sh -c \"cargo test --workspace\"").unwrap(),
            vec!["sh".to_string(), "-c".to_string(), "cargo test --workspace".to_string()]
        );
        assert_eq!(
            parse_command_argv("cargo test --workspace").unwrap(),
            vec!["cargo".to_string(), "test".to_string(), "--workspace".to_string()]
        );
        assert_eq!(
            parse_command_argv("echo 'single quoted arg' plain").unwrap(),
            vec!["echo".to_string(), "single quoted arg".to_string(), "plain".to_string()]
        );
        assert_eq!(
            parse_command_argv("printf a\\ b").unwrap(),
            vec!["printf".to_string(), "a b".to_string()]
        );
        assert_eq!(parse_command_argv("   ").unwrap(), Vec::<String>::new());
        assert!(parse_command_argv("sh -c \"unterminated").is_err());
    }

    #[tokio::test]
    async fn failing_verification_returns_the_structured_failure_evidence() {
        use crate::env::{CompletionGateContext, GateOutcome};
        use crate::types::RunId;
        let root = std::env::temp_dir().join(format!("kroma-gate-fail-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        #[cfg(unix)]
        let fail_cmd = vec![
            "sh".into(),
            "-c".into(),
            "printf 'error: assertion failed\n' >&2; exit 42".into(),
        ];
        #[cfg(windows)]
        let fail_cmd = vec![
            "cmd.exe".into(),
            "/C".into(),
            "echo error: assertion failed 1>&2 & exit /B 42".into(),
        ];
        let gate = WorkspaceCompletionGate::new(Some(fail_cmd), Duration::from_secs(3));
        let ctx = CompletionGateContext {
            run_id: RunId::new(), workspace_root: root.clone(), modified_files: vec!["src/lib.rs".into()], turns_used: 2,
        };
        match gate.check(&ctx).await.unwrap() {
            GateOutcome::Rejected { feedback } => assert!(feedback.contains("error: assertion failed")),
            other => panic!("expected rejected gate outcome, got {:?}", other),
        }
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn dirty_file_short_circuit_does_not_spawn_an_invalid_command() {
        use crate::env::{CompletionGateContext, GateOutcome};
        use crate::types::RunId;
        let gate = WorkspaceCompletionGate::new(Some(vec!["/definitely/not/a/real/command".into()]), Duration::from_secs(1));
        let ctx = CompletionGateContext {
            run_id: RunId::new(), workspace_root: std::env::temp_dir(), modified_files: vec![], turns_used: 1,
        };
        assert_eq!(gate.check(&ctx).await.unwrap(), GateOutcome::Approved);
    }

    #[test]
    fn auto_detects_cargo_workspace_verification() {
        let root = std::env::temp_dir().join(format!("kroma-gate-cargo-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("Cargo.toml"), "[workspace]\n").unwrap();
        let gate = WorkspaceCompletionGate::new(None, Duration::from_secs(1));
        assert_eq!(gate.select_command(&root).unwrap(), vec!["cargo".to_string(), "test".to_string(), "--workspace".to_string(), "--no-fail-fast".to_string()]);
        let _ = std::fs::remove_dir_all(root);
    }
}
