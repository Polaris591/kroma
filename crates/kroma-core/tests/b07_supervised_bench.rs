//! B07/F08 contract: the bench-held-out `verify_command` runs under the shared one-shot
//! `ProcessSupervisor`, so a hostile or broken verification command can no longer
//!
//! * leak host credentials into the child (environment is sanitized),
//! * escape as a surviving background grandchild after the leader exits (process group swept),
//! * flood the harness with unbounded stdout/stderr (combined byte cap), or
//! * ignore the operator-configured timeout.
//!
//! Display-only head/tail trimming of a large-but-bounded success must stay a success; an
//! incomplete capture (flood cap / drain cut / timeout) must never resolve. Every fixture here is
//! finite and small (max 2 MiB of output) so it can run under a memory-capped cgroup.

#![cfg(unix)]

use kroma_core::bench::{BenchTask, TaskEvaluation, VerifyCommandLimits};
use std::collections::BTreeMap;
use std::fs;
use std::time::{Duration, Instant};
use uuid::Uuid;

fn temp_workspace(label: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("kroma-b07-{}-{}", label, Uuid::new_v4()));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn task_with_verify(id: &str, verify_command: Vec<String>) -> BenchTask {
    BenchTask {
        id: id.to_string(),
        prompt: "bench fixture".to_string(),
        files: BTreeMap::new(),
        expect_contains: Vec::new(),
        expect_absent: Vec::new(),
        regression_guard: false,
        verify_command: Some(verify_command),
    }
}

fn sh(script: &str) -> Vec<String> {
    vec!["sh".to_string(), "-c".to_string(), script.to_string()]
}

/// Kills a leftover pid on drop so a red assertion cannot leak a sleeper into later tests.
struct PidGuard(i32);
impl Drop for PidGuard {
    fn drop(&mut self) {
        if self.0 > 0 {
            unsafe {
                let _ = libc::kill(self.0, libc::SIGKILL);
            }
        }
    }
}

/// Whether `pid` has terminated for the purpose of a cleanup assertion (a zombie counts as gone).
fn process_terminated(pid: i32) -> bool {
    if !kroma_core::process_group::ChildProcessGroup::is_process_alive(pid) {
        return true;
    }
    fs::read_to_string(format!("/proc/{}/stat", pid))
        .ok()
        .and_then(|stat| {
            stat.rsplit_once(')')
                .and_then(|(_, rest)| rest.trim().chars().next())
        })
        .map(|state| state == 'Z')
        .unwrap_or(false)
}

/// Sets an environment variable for the duration of a test and restores it on drop.
struct EnvGuard {
    key: std::ffi::OsString,
    previous: Option<std::ffi::OsString>,
}

impl EnvGuard {
    fn set(key: impl Into<std::ffi::OsString>, value: impl Into<std::ffi::OsString>) -> Self {
        let key = key.into();
        let previous = std::env::var_os(&key);
        std::env::set_var(&key, value.into());
        Self { key, previous }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match self.previous.take() {
            Some(value) => std::env::set_var(&self.key, value),
            None => std::env::remove_var(&self.key),
        }
    }
}

fn assert_failure_mentions(verdict: &TaskEvaluation, needle: &str) {
    assert!(
        verdict
            .failures
            .iter()
            .any(|f| f.contains("verify_command") && f.contains(needle)),
        "expected a verify failure mentioning {:?}, got {:?}",
        needle,
        verdict.failures
    );
}

/// The leader exits 0 but leaves a background grandchild; the supervisor must sweep the process
/// group before the verdict is returned.
#[tokio::test]
async fn b07_verify_command_cleans_grandchild_on_normal_exit() {
    let ws = temp_workspace("grandchild");
    let task = task_with_verify(
        "grandchild",
        sh("sleep 30 >/dev/null 2>&1 & echo $! > bg.pid; exit 0"),
    );

    let verdict = task.evaluate(&ws).await;
    assert!(
        verdict.resolved,
        "a clean verify exit must resolve: {:?}",
        verdict.failures
    );
    assert!(
        verdict.verify_duration > Duration::ZERO,
        "a real verify run must be accounted"
    );

    let pid: i32 = fs::read_to_string(ws.join("bg.pid"))
        .expect("fixture must record the grandchild pid")
        .trim()
        .parse()
        .expect("pid");
    let _guard = PidGuard(pid);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        process_terminated(pid),
        "background grandchild {} must be swept after the verify command exits",
        pid
    );

    let _ = fs::remove_dir_all(&ws);
}

/// A host credential must not reach the verification command.
#[tokio::test]
async fn b07_verify_command_env_is_sanitized() {
    let _guard = EnvGuard::set("KROMA_BENCH_SENTINEL_SECRET", "leaked-credential");
    let ws = temp_workspace("env");
    let task = task_with_verify(
        "env",
        sh("test -z \"$KROMA_BENCH_SENTINEL_SECRET\""),
    );

    let verdict = task.evaluate(&ws).await;
    assert!(
        verdict.resolved,
        "the credential-shaped host variable must not be visible to verify_command: {:?}",
        verdict.failures
    );

    let _ = fs::remove_dir_all(&ws);
}

/// The operator-configured timeout must actually bound the verification command.
#[tokio::test]
async fn b07_verify_command_custom_timeout_is_enforced() {
    let ws = temp_workspace("timeout");
    let task = task_with_verify("timeout", sh("sleep 5"));

    let started = Instant::now();
    let verdict = task
        .evaluate_with_timeout(&ws, Duration::from_millis(500))
        .await;
    let elapsed = started.elapsed();

    assert!(
        !verdict.resolved,
        "a command that outlives the configured timeout must not resolve"
    );
    assert_failure_mentions(&verdict, "timed out");
    assert!(
        elapsed < Duration::from_secs(3),
        "the 500ms timeout must be honored, not the 120s default (took {:?})",
        elapsed
    );
    assert!(
        verdict.verify_duration > Duration::ZERO,
        "a timed-out verify still costs real time and must be accounted"
    );

    let _ = fs::remove_dir_all(&ws);
}

/// A finite 2 MiB flood against a low cap must be terminated and must not resolve.
#[tokio::test]
async fn b07_verify_command_flood_cap_rejects_incomplete_capture() {
    let ws = temp_workspace("flood");
    // `head -c` bounds the stream: 2 MiB, finite, no `yes`/`/dev/zero` runaway.
    let task = task_with_verify(
        "flood",
        sh("head -c 2097152 /dev/zero | tr '\\0' 'x'; exit 0"),
    );

    let verdict = task
        .evaluate_with_limits(&ws, &VerifyCommandLimits::new(Duration::from_secs(30), 64 * 1024))
        .await;

    assert!(
        !verdict.resolved,
        "an output flood must never be reported as a resolved task"
    );
    assert_failure_mentions(&verdict, "cap");
    assert!(
        verdict.verify_duration > Duration::ZERO,
        "the terminated verify run must still be accounted"
    );

    let _ = fs::remove_dir_all(&ws);
}

/// A large-but-bounded success (bigger than the display window, under the cap) must stay a success:
/// head/tail trimming is presentation, not capture incompleteness.
#[tokio::test]
async fn b07_verify_command_display_trim_does_not_fail_success() {
    let ws = temp_workspace("trim");
    let task = task_with_verify(
        "trim",
        sh("head -c 262144 /dev/zero | tr '\\0' 'x'; exit 0"),
    );

    let verdict = task.evaluate(&ws).await;
    assert!(
        verdict.resolved,
        "a bounded 256 KiB success must resolve despite display trimming: {:?}",
        verdict.failures
    );

    let _ = fs::remove_dir_all(&ws);
}

/// A task without a `verify_command` must not be charged any verify time.
///
/// B09/F07: `evaluate` now rejects a task with no oracle (no `expect_contains`, no
/// `expect_absent`, no non-empty `verify_command`) instead of resolving it vacuously, so this
/// fixture carries a real post-condition. The point of the test is unchanged: with no
/// `verify_command` the verdict is decided by the post-condition alone and `verify_duration` is
/// `Duration::ZERO`.
#[tokio::test]
async fn b07_verify_duration_is_zero_without_verify_command() {
    let ws = temp_workspace("noverify");
    fs::write(ws.join("marker.txt"), "x").unwrap();
    let mut task = task_with_verify("noverify", sh("exit 0"));
    task.verify_command = None;
    task.expect_contains = vec![("marker.txt".to_string(), "x".to_string())];

    let verdict = task.evaluate(&ws).await;
    assert!(verdict.resolved);
    assert_eq!(verdict.verify_duration, Duration::ZERO);

    let _ = fs::remove_dir_all(&ws);
}

/// Invalid (zero) limits must be rejected as a failed check without spawning anything.
#[tokio::test]
async fn b07_verify_invalid_limits_rejected_without_spawn() {
    let ws = temp_workspace("invalid");
    let task = task_with_verify("invalid", sh("touch must_not_run.txt; exit 0"));

    let verdict = task
        .evaluate_with_limits(&ws, &VerifyCommandLimits::new(Duration::ZERO, 1024))
        .await;

    assert!(!verdict.resolved);
    assert_failure_mentions(&verdict, "invalid verify_command limits");
    assert!(
        !ws.join("must_not_run.txt").exists(),
        "invalid limits must not spawn the command"
    );
    assert_eq!(verdict.verify_duration, Duration::ZERO);

    let _ = fs::remove_dir_all(&ws);
}
