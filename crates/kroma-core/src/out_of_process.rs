//! KR-O02: out-of-process fault domain for extensions.
//!
//! The task's rule is that isolation is only worth its cost where a failure would otherwise take the
//! host down, and that built-in high-frequency safety tools stay on the direct hot path. This module
//! is the prototype for the first half: run an extension command in its own process group, with a
//! deadline, and turn a crash, a hang or a non-zero exit into a value the caller can handle instead
//! of a dead host.

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// How an isolated extension call ended. Every variant is a *value*, which is the point: a fault in
/// the extension must not be able to propagate as a panic or a hang in the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IsolatedOutcome {
    /// The command exited 0 within the deadline.
    Ok { stdout: String },
    /// The command ran but exited non-zero.
    Failed { code: Option<i32>, stderr: String },
    /// The deadline passed; the child was killed.
    TimedOut { after_ms: u128 },
    /// The command could not be started at all.
    Unavailable { reason: String },
}

/// Run one extension command in isolation.
///
/// `deadline` bounds the whole call. Output is capped at `max_stdout_bytes` so a chatty extension
/// cannot exhaust memory in the host before the deadline fires.
pub fn run_isolated(
    program: &str,
    args: &[&str],
    deadline: Duration,
    max_stdout_bytes: usize,
) -> IsolatedOutcome {
    let started = Instant::now();
    let mut child = match Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => {
            return IsolatedOutcome::Unavailable {
                reason: e.to_string(),
            }
        }
    };

    // Poll rather than block so the deadline is enforced even for a child that ignores signals.
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut stdout = String::new();
                if let Some(mut out) = child.stdout.take() {
                    let mut buf = vec![0u8; max_stdout_bytes];
                    let n = out.read(&mut buf).unwrap_or(0);
                    stdout = String::from_utf8_lossy(&buf[..n]).to_string();
                }
                let mut stderr = String::new();
                if let Some(mut err) = child.stderr.take() {
                    let mut buf = vec![0u8; 4096];
                    let n = err.read(&mut buf).unwrap_or(0);
                    stderr = String::from_utf8_lossy(&buf[..n]).to_string();
                }
                return if status.success() {
                    IsolatedOutcome::Ok { stdout }
                } else {
                    IsolatedOutcome::Failed {
                        code: status.code(),
                        stderr,
                    }
                };
            }
            Ok(None) => {
                if started.elapsed() > deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return IsolatedOutcome::TimedOut {
                        after_ms: started.elapsed().as_millis(),
                    };
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(e) => {
                let _ = child.kill();
                return IsolatedOutcome::Unavailable {
                    reason: e.to_string(),
                };
            }
        }
    }
}

/// Whether a tool should be isolated. Built-in safety tools stay direct: they run on every edit, and
/// paying a process spawn for them would be the cost the task warns about.
pub fn should_isolate(tool_name: &str) -> bool {
    !matches!(
        tool_name,
        "read" | "write" | "edit" | "bash" | "grep" | "find" | "ls"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_successful_extension_returns_its_output() {
        let out = run_isolated("/bin/echo", &["hello"], Duration::from_secs(5), 1024);
        assert_eq!(
            out,
            IsolatedOutcome::Ok {
                stdout: "hello\n".to_string()
            }
        );
    }

    #[test]
    fn a_failing_extension_is_a_value_not_a_panic() {
        let out = run_isolated("/bin/false", &[], Duration::from_secs(5), 1024);
        match out {
            IsolatedOutcome::Failed { code, .. } => assert_eq!(code, Some(1)),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn a_hanging_extension_is_killed_at_the_deadline() {
        let started = Instant::now();
        let out = run_isolated("/bin/sleep", &["30"], Duration::from_millis(200), 1024);
        assert!(
            matches!(out, IsolatedOutcome::TimedOut { .. }),
            "got {out:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the host must not wait for the child's own timeout"
        );
    }

    #[test]
    fn a_missing_extension_is_reported_as_unavailable() {
        let out = run_isolated(
            "/nonexistent/extension-binary",
            &[],
            Duration::from_secs(1),
            64,
        );
        assert!(
            matches!(out, IsolatedOutcome::Unavailable { .. }),
            "got {out:?}"
        );
    }

    #[test]
    fn builtin_hot_path_tools_are_not_isolated() {
        for tool in ["read", "write", "edit", "bash", "grep"] {
            assert!(!should_isolate(tool), "{tool} must stay on the direct path");
        }
        assert!(should_isolate("third_party_scanner"));
    }
}
