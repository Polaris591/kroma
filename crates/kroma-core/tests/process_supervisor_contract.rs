//! Contract tests for the shared process supervisor: process-group containment, output flood caps,
//! cancellation, timeouts and Unix pid liveness probing. Windows Job-object containment runs in B15,
//! so this whole file is Unix-only.
#![cfg(unix)]

use kroma_core::budget::RunDeadline;
use kroma_core::env::ExecutionContext;
use kroma_core::process_group::ChildProcessGroup;
use kroma_core::process_supervisor::{
    EnvironmentPolicy, OneShotCommandSpec, ProcessSupervisor, TerminationReason,
};
use std::fs;
use std::time::{Duration, Instant};
use tokio::sync::watch;
use uuid::Uuid;

struct PidGuard(i32);
impl Drop for PidGuard {
    fn drop(&mut self) {
        #[cfg(unix)]
        if self.0 > 0 {
            unsafe {
                let _ = libc::kill(self.0, libc::SIGKILL);
            }
        }
    }
}

/// Restores/removes an environment variable on drop.
///
/// The sanitization tests mutate the process environment; without a drop guard a failing
/// assertion would leak a credential-shaped or non-UTF-8 variable into every later test in the
/// same process (a non-UTF-8 leak made the old `std::env::vars()` sanitizer panic in nine other
/// tests during red-testing).
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

/// Whether `pid` has terminated for the purpose of a cleanup assertion.
///
/// `ChildProcessGroup::is_process_alive` uses `kill(pid, 0)`, which reports a zombie -- a process
/// that has already terminated but has not been reaped -- as alive. The swept grandchild is reaped
/// asynchronously, so that made these assertions racy. Treat a zombie as gone: it can no longer
/// execute or hold the stdout/stderr pipes the assertions verify are closed.
#[cfg(unix)]
fn process_terminated(pid: i32) -> bool {
    if !ChildProcessGroup::is_process_alive(pid) {
        return true;
    }
    std::fs::read_to_string(format!("/proc/{}/stat", pid))
        .ok()
        .and_then(|stat| {
            stat.rsplit_once(')')
                .and_then(|(_, rest)| rest.trim().chars().next())
        })
        .map(|state| state == 'Z')
        .unwrap_or(false)
}

#[tokio::test]
async fn test_contract_normal_exit_cleans_grandchild_processes() {
    let tmp_ws = std::env::temp_dir().join(format!("kroma-ps-norm-{}", Uuid::new_v4()));
    fs::create_dir_all(&tmp_ws).unwrap();

    // Spawns a background grandchild process that sleeps 30s, prints its PID, and the parent exits 0 immediately.
    let spec = OneShotCommandSpec::new(
        vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            "sleep 30 >/dev/null 2>&1 & echo $!; exit 0".to_string(),
        ],
        &tmp_ws,
    );

    let res = ProcessSupervisor::execute_one_shot(spec, None)
        .await
        .expect("execute_one_shot should succeed");

    assert_eq!(res.exit_code, Some(0));
    assert_eq!(res.terminated_by, Some(TerminationReason::Exit));

    let bg_pid: i32 = res
        .stdout
        .lines()
        .find_map(|l| l.trim().parse::<i32>().ok())
        .expect("should find background grandchild PID in stdout");
    assert!(bg_pid > 0);
    let _guard = PidGuard(bg_pid);

    #[cfg(unix)]
    {
        // Give OS a moment to register signal
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            process_terminated(bg_pid),
            "Background grandchild PID {} must be cleaned up on normal exit, but is still alive",
            bg_pid
        );
    }

    let _ = fs::remove_dir_all(&tmp_ws);
}

#[tokio::test]
async fn test_contract_pre_cancelled_zero_spawn() {
    let tmp_ws = std::env::temp_dir().join(format!("kroma-ps-precancel-{}", Uuid::new_v4()));
    fs::create_dir_all(&tmp_ws).unwrap();
    let marker_file = tmp_ws.join("must_not_spawn_cancel.txt");

    let (cancel_tx, cancel_rx) = watch::channel(true); // Pre-cancelled!
    let _ = cancel_tx.send(true);

    let spec = OneShotCommandSpec::new(
        vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            format!("touch {}; exit 0", marker_file.display()),
        ],
        &tmp_ws,
    );

    let res = ProcessSupervisor::execute_one_shot(spec, Some(cancel_rx))
        .await
        .expect("pre-cancelled execute_one_shot should return Ok result");

    assert_eq!(res.terminated_by, Some(TerminationReason::Cancelled));
    assert_eq!(res.exit_code, None);
    assert!(
        !marker_file.exists(),
        "Pre-cancelled command must result in ZERO spawns (marker file must not exist)"
    );

    let _ = fs::remove_dir_all(&tmp_ws);
}

#[tokio::test]
async fn test_contract_pre_deadline_exceeded_zero_spawn() {
    let tmp_ws = std::env::temp_dir().join(format!("kroma-ps-predeadline-{}", Uuid::new_v4()));
    fs::create_dir_all(&tmp_ws).unwrap();
    let marker_file = tmp_ws.join("must_not_spawn_deadline.txt");

    // A deadline in the past is already exceeded
    let deadline = RunDeadline::at(Instant::now() - Duration::from_millis(100));

    let spec = OneShotCommandSpec::new(
        vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            format!("touch {}; exit 0", marker_file.display()),
        ],
        &tmp_ws,
    )
    .with_deadline(Some(deadline));

    let res = ProcessSupervisor::execute_one_shot(spec, None)
        .await
        .expect("pre-deadline execute_one_shot should return Ok result");

    assert_eq!(res.terminated_by, Some(TerminationReason::Deadline));
    assert_eq!(res.exit_code, None);
    assert!(
        !marker_file.exists(),
        "Pre-deadline exceeded command must result in ZERO spawns (marker file must not exist)"
    );

    let _ = fs::remove_dir_all(&tmp_ws);
}

#[tokio::test]
async fn test_contract_output_flood_cap_combined_limit_terminates_group() {
    let tmp_ws = std::env::temp_dir().join(format!("kroma-ps-flood-{}", Uuid::new_v4()));
    fs::create_dir_all(&tmp_ws).unwrap();

    let flood_cap = 64 * 1024; // 64 KiB
                               // Bounded 2 MiB payload (> 64 KiB cap) ensures finite execution and prevents memory exhaustion
    let spec = OneShotCommandSpec::new(
        vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            "head -c 2097152 /dev/zero | tr '\\0' 'x'".to_string(),
        ],
        &tmp_ws,
    )
    .with_max_output_bytes(flood_cap)
    .with_timeout(Duration::from_secs(10));

    let res = ProcessSupervisor::execute_one_shot(spec, None)
        .await
        .expect("flood command should terminate with OutputFlood result");

    assert_eq!(res.terminated_by, Some(TerminationReason::OutputFlood));
    assert!(res.truncated);
    assert!(
        !res.is_success(),
        "A flooded capture must never be reported as a complete success"
    );
    assert!(
        res.stdout.len() <= flood_cap + 8192,
        "Retained output must be bounded by flood cap, got {} bytes",
        res.stdout.len()
    );

    let _ = fs::remove_dir_all(&tmp_ws);
}

#[tokio::test]
async fn test_contract_cancellation_kills_entire_process_tree() {
    let tmp_ws = std::env::temp_dir().join(format!("kroma-ps-cancel-{}", Uuid::new_v4()));
    fs::create_dir_all(&tmp_ws).unwrap();
    let pid_file = tmp_ws.join("cancel_sub_pid.txt");

    let (cancel_tx, cancel_rx) = watch::channel(false);
    let spec = OneShotCommandSpec::new(
        vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            format!("sleep 60 & echo $! > {}; wait", pid_file.display()),
        ],
        &tmp_ws,
    )
    .with_timeout(Duration::from_secs(30));

    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(150)).await;
        let _ = cancel_tx.send(true);
    });

    let res = ProcessSupervisor::execute_one_shot(spec, Some(cancel_rx))
        .await
        .expect("cancelled command should return Ok result");

    assert_eq!(res.terminated_by, Some(TerminationReason::Cancelled));

    #[cfg(unix)]
    {
        let pid_str = fs::read_to_string(&pid_file).expect("pid file should have been written");
        let sub_pid: i32 = pid_str.trim().parse().expect("valid sub pid");
        let _guard = PidGuard(sub_pid);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            process_terminated(sub_pid),
            "Cancelled command must terminate grandchild PID {}, but it is still alive",
            sub_pid
        );
    }

    let _ = fs::remove_dir_all(&tmp_ws);
}

#[tokio::test]
async fn test_contract_pipe_drain_timeout_prevents_grandchild_hang() {
    let tmp_ws = std::env::temp_dir().join(format!("kroma-ps-drain-{}", Uuid::new_v4()));
    fs::create_dir_all(&tmp_ws).unwrap();

    // Grandchild process inherits stdout and sleeps 30s; parent exits 0 immediately.
    let spec = OneShotCommandSpec::new(
        vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            "(sleep 30 &); echo 'LEADER_COMPLETED'; exit 0".to_string(),
        ],
        &tmp_ws,
    )
    .with_timeout(Duration::from_secs(10));

    let start = Instant::now();
    let res = ProcessSupervisor::execute_one_shot(spec, None)
        .await
        .expect("execute_one_shot should succeed");
    let elapsed = start.elapsed();

    assert!(
        elapsed < Duration::from_secs(2),
        "Pipe drain timeout must complete in < 2s, but took {:?}",
        elapsed
    );
    assert_eq!(res.exit_code, Some(0));
    assert!(res.stdout.contains("LEADER_COMPLETED"));
    assert!(
        res.truncated,
        "A drain deadline that ends before stdout reaches EOF must report truncated output, not a complete one"
    );
    assert!(
        !res.is_success(),
        "An incomplete (drain-cut) capture must never be reported as a complete success"
    );

    let _ = fs::remove_dir_all(&tmp_ws);
}

#[tokio::test]
async fn test_contract_future_drop_raii_kills_grandchild() {
    let tmp_ws = std::env::temp_dir().join(format!("kroma-ps-drop-{}", Uuid::new_v4()));
    fs::create_dir_all(&tmp_ws).unwrap();
    let pid_file = tmp_ws.join("drop_sub_pid.txt");

    let spec = OneShotCommandSpec::new(
        vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            format!("sleep 60 & echo $! > {}; sleep 60", pid_file.display()),
        ],
        &tmp_ws,
    )
    .with_timeout(Duration::from_secs(30));

    // Poll the future for 150ms and then DROP the future!
    let _ = tokio::time::timeout(
        Duration::from_millis(150),
        ProcessSupervisor::execute_one_shot(spec, None),
    )
    .await;

    #[cfg(unix)]
    {
        let pid_str = fs::read_to_string(&pid_file).expect("pid file should have been written");
        let sub_pid: i32 = pid_str.trim().parse().expect("valid sub pid");
        let _guard = PidGuard(sub_pid);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            process_terminated(sub_pid),
            "Dropping execute_one_shot future must trigger RAII cleanup of grandchild PID {}, but it is still alive",
            sub_pid
        );
    }

    let _ = fs::remove_dir_all(&tmp_ws);
}

#[tokio::test]
async fn test_contract_non_utf8_output_handling() {
    let tmp_ws = std::env::temp_dir().join(format!("kroma-ps-nonutf8-{}", Uuid::new_v4()));
    fs::create_dir_all(&tmp_ws).unwrap();

    let spec = OneShotCommandSpec::new(
        vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            "printf 'hello\\xff\\xfe\\x00world\\n'".to_string(),
        ],
        &tmp_ws,
    );

    let res = ProcessSupervisor::execute_one_shot(spec, None)
        .await
        .expect("non-utf8 output should succeed without panic");

    assert_eq!(res.exit_code, Some(0));
    assert!(res.stdout.contains("hello"));
    assert!(res.stdout.contains("world"));

    let _ = fs::remove_dir_all(&tmp_ws);
}

#[tokio::test]
async fn test_contract_sanitized_environment_and_authorized_override() {
    let tmp_ws = std::env::temp_dir().join(format!("kroma-ps-env-{}", Uuid::new_v4()));
    fs::create_dir_all(&tmp_ws).unwrap();

    let _kroma = EnvGuard::set("KROMA_TEST_SECRET_KEY", "sensitive_val_should_be_stripped");
    let _safe = EnvGuard::set("SAFE_INHERITED_VAR", "visible_val");

    let spec = OneShotCommandSpec::new(
        vec!["/bin/sh".to_string(), "-c".to_string(), "env".to_string()],
        &tmp_ws,
    )
    .with_env_policy(EnvironmentPolicy::SanitizedWithCustom(vec![(
        "EXPLICIT_AUTHORIZED_OVERRIDE".to_string(),
        "authorized_value".to_string(),
    )]));

    let res = ProcessSupervisor::execute_one_shot(spec, None)
        .await
        .expect("env execution should succeed");

    assert_eq!(res.exit_code, Some(0));
    assert!(
        !res.stdout.contains("sensitive_val_should_be_stripped"),
        "Sensitive variable must be sanitized"
    );
    assert!(
        res.stdout
            .contains("EXPLICIT_AUTHORIZED_OVERRIDE=authorized_value"),
        "Explicit authorized override must be present"
    );
    assert!(
        res.stdout.contains("CI=true"),
        "Baseline CI=true must be present in sanitized env"
    );

    let _ = fs::remove_dir_all(&tmp_ws);
}

#[tokio::test]
async fn test_contract_execution_context_integration() {
    let tmp_ws = std::env::temp_dir().join(format!("kroma-ps-ctx-{}", Uuid::new_v4()));
    fs::create_dir_all(&tmp_ws).unwrap();

    let (cancel_tx, cancel_rx) = watch::channel(false);
    let deadline = RunDeadline::at(Instant::now() + Duration::from_secs(10));
    let ctx = ExecutionContext::new(None, Some(deadline), Some(cancel_rx), None, None);

    let spec = OneShotCommandSpec::new(
        vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            "echo 'ctx test'".to_string(),
        ],
        &tmp_ws,
    );

    let res = ProcessSupervisor::execute_one_shot_with_context(spec, &ctx)
        .await
        .expect("execute_one_shot_with_context should succeed");

    assert_eq!(res.exit_code, Some(0));
    assert!(res.stdout.contains("ctx test"));

    let _ = cancel_tx.send(true);
    let _ = fs::remove_dir_all(&tmp_ws);
}

#[tokio::test]
async fn test_contract_timeout_terminates_group() {
    let tmp_ws = std::env::temp_dir().join(format!("kroma-ps-timeout-{}", Uuid::new_v4()));
    fs::create_dir_all(&tmp_ws).unwrap();
    let pid_file = tmp_ws.join("timeout_sub_pid.txt");

    let spec = OneShotCommandSpec::new(
        vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            format!("sleep 60 & echo $! > {}; wait", pid_file.display()),
        ],
        &tmp_ws,
    )
    .with_timeout(Duration::from_millis(400));

    let start = Instant::now();
    let res = ProcessSupervisor::execute_one_shot(spec, None)
        .await
        .expect("timed-out command should return Ok result");

    assert_eq!(res.terminated_by, Some(TerminationReason::Timeout));
    assert_eq!(res.exit_code, None);
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "Timeout must terminate promptly, took {:?}",
        start.elapsed()
    );

    #[cfg(unix)]
    {
        let pid_str = fs::read_to_string(&pid_file).expect("pid file should have been written");
        let sub_pid: i32 = pid_str.trim().parse().expect("valid sub pid");
        let _guard = PidGuard(sub_pid);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            process_terminated(sub_pid),
            "Timed-out command must terminate grandchild PID {}, but it is still alive",
            sub_pid
        );
    }

    let _ = fs::remove_dir_all(&tmp_ws);
}

#[tokio::test]
async fn test_contract_midflight_deadline_terminates_group() {
    let tmp_ws = std::env::temp_dir().join(format!("kroma-ps-middeadline-{}", Uuid::new_v4()));
    fs::create_dir_all(&tmp_ws).unwrap();
    let pid_file = tmp_ws.join("deadline_sub_pid.txt");

    // The run deadline lands while the command is still running (the command timeout is far away).
    let deadline = RunDeadline::at(Instant::now() + Duration::from_millis(400));
    let spec = OneShotCommandSpec::new(
        vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            format!("sleep 60 & echo $! > {}; wait", pid_file.display()),
        ],
        &tmp_ws,
    )
    .with_timeout(Duration::from_secs(30))
    .with_deadline(Some(deadline));

    let start = Instant::now();
    let res = ProcessSupervisor::execute_one_shot(spec, None)
        .await
        .expect("deadline-terminated command should return Ok result");

    assert_eq!(res.terminated_by, Some(TerminationReason::Deadline));
    assert_eq!(res.exit_code, None);
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "Deadline must terminate promptly, took {:?}",
        start.elapsed()
    );

    #[cfg(unix)]
    {
        let pid_str = fs::read_to_string(&pid_file).expect("pid file should have been written");
        let sub_pid: i32 = pid_str.trim().parse().expect("valid sub pid");
        let _guard = PidGuard(sub_pid);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            process_terminated(sub_pid),
            "Deadline-terminated command must kill grandchild PID {}, but it is still alive",
            sub_pid
        );
    }

    let _ = fs::remove_dir_all(&tmp_ws);
}

#[tokio::test]
async fn test_contract_stderr_flood_cap_terminates_group() {
    let tmp_ws = std::env::temp_dir().join(format!("kroma-ps-errflood-{}", Uuid::new_v4()));
    fs::create_dir_all(&tmp_ws).unwrap();

    let flood_cap = 64 * 1024; // 64 KiB
    let spec = OneShotCommandSpec::new(
        vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            "head -c 2097152 /dev/zero | tr '\\0' 'e' 1>&2".to_string(),
        ],
        &tmp_ws,
    )
    .with_max_output_bytes(flood_cap)
    .with_timeout(Duration::from_secs(10));

    let res = ProcessSupervisor::execute_one_shot(spec, None)
        .await
        .expect("stderr flood should terminate with OutputFlood result");

    assert_eq!(res.terminated_by, Some(TerminationReason::OutputFlood));
    assert!(res.truncated);
    assert!(
        res.stderr.len() <= flood_cap + 8192,
        "Retained stderr must be bounded by the flood cap, got {} bytes",
        res.stderr.len()
    );

    let _ = fs::remove_dir_all(&tmp_ws);
}

#[tokio::test]
async fn test_contract_combined_cap_trips_when_each_stream_is_under_it() {
    let tmp_ws = std::env::temp_dir().join(format!("kroma-ps-combined-{}", Uuid::new_v4()));
    fs::create_dir_all(&tmp_ws).unwrap();

    let flood_cap = 64 * 1024; // 64 KiB combined
    let per_stream = 40 * 1024; // 40 KiB each: under the cap alone, over it together
    let spec = OneShotCommandSpec::new(
        vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            format!(
                "head -c {per_stream} /dev/zero | tr '\\0' 'x'; head -c {per_stream} /dev/zero | tr '\\0' 'y' 1>&2"
            ),
        ],
        &tmp_ws,
    )
    .with_max_output_bytes(flood_cap)
    .with_timeout(Duration::from_secs(10));

    let res = ProcessSupervisor::execute_one_shot(spec, None)
        .await
        .expect("combined flood should terminate with OutputFlood result");

    assert_eq!(res.terminated_by, Some(TerminationReason::OutputFlood));
    assert!(res.truncated);
    assert!(
        res.stdout.len() <= per_stream,
        "stdout alone is under the cap and must not be head/tail truncated, got {} bytes",
        res.stdout.len()
    );
    assert!(
        res.stderr.len() <= per_stream,
        "stderr alone is under the cap and must not be head/tail truncated, got {} bytes",
        res.stderr.len()
    );

    let _ = fs::remove_dir_all(&tmp_ws);
}

#[tokio::test]
async fn test_contract_head_tail_bounds_output_below_flood_cap() {
    let tmp_ws = std::env::temp_dir().join(format!("kroma-ps-headtail-{}", Uuid::new_v4()));
    fs::create_dir_all(&tmp_ws).unwrap();

    // 100 KiB of output is far below the 1 MiB flood cap, so only the default 16 KiB + 48 KiB
    // display window (not the flood cap) can trim what is returned.
    let spec = OneShotCommandSpec::new(
        vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            "head -c 102400 /dev/zero | tr '\\0' 'z'".to_string(),
        ],
        &tmp_ws,
    )
    .with_max_output_bytes(1024 * 1024)
    .with_timeout(Duration::from_secs(10));

    let res = ProcessSupervisor::execute_one_shot(spec, None)
        .await
        .expect("head/tail bounded command should succeed");

    assert_eq!(res.exit_code, Some(0));
    assert_eq!(res.terminated_by, Some(TerminationReason::Exit));
    assert!(
        res.output_trimmed,
        "Output beyond the display window must set output_trimmed"
    );
    assert!(
        !res.truncated,
        "Display-only trimming must not mark the capture incomplete"
    );
    assert!(
        res.stdout.len() <= 16 * 1024 + 48 * 1024 + 512,
        "Retained stdout must be bounded by the display window, got {} bytes",
        res.stdout.len()
    );
    assert!(
        res.stdout.contains("bytes omitted"),
        "Trimmed output must mark the omitted span"
    );
    assert!(
        res.is_success(),
        "Display-only head/tail trimming must not turn a successful exit 0 into a failure"
    );

    let _ = fs::remove_dir_all(&tmp_ws);
}

#[cfg(unix)]
#[tokio::test]
async fn test_contract_non_utf8_environment_does_not_panic() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let tmp_ws = std::env::temp_dir().join(format!("kroma-ps-nonutf8env-{}", Uuid::new_v4()));
    fs::create_dir_all(&tmp_ws).unwrap();

    // A host environment containing non-UTF-8 key/value bytes used to make sanitization panic
    // (`std::env::vars()`); `vars_os` must carry it through without panicking.
    let key = OsString::from_vec(b"KROMA_TEST_NON_UTF8_\xff".to_vec());
    let value = OsString::from_vec(b"value_\xfe\xff".to_vec());
    let _guard = EnvGuard::set(key, value);

    let spec = OneShotCommandSpec::new(
        vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            "exit 0".to_string(),
        ],
        &tmp_ws,
    );

    let res = ProcessSupervisor::execute_one_shot(spec, None)
        .await
        .expect("non-UTF-8 host env must not panic sanitization");

    assert_eq!(res.exit_code, Some(0));

    let _ = fs::remove_dir_all(&tmp_ws);
}

#[tokio::test]
async fn test_contract_sanitized_env_strips_kroma_keeps_git_author() {
    let tmp_ws = std::env::temp_dir().join(format!("kroma-ps-envshare-{}", Uuid::new_v4()));
    fs::create_dir_all(&tmp_ws).unwrap();

    let _kroma = EnvGuard::set("KROMA_TEST_HOME", "/tmp/should_be_stripped");
    let _author = EnvGuard::set("GIT_AUTHOR_NAME", "Ada Lovelace");
    let _email = EnvGuard::set("GIT_AUTHOR_EMAIL", "ada@example.com");

    let spec = OneShotCommandSpec::new(
        vec!["/bin/sh".to_string(), "-c".to_string(), "env".to_string()],
        &tmp_ws,
    );

    let res = ProcessSupervisor::execute_one_shot(spec, None)
        .await
        .expect("env execution should succeed");

    assert_eq!(res.exit_code, Some(0));
    assert!(
        res.stdout.contains("GIT_AUTHOR_NAME=Ada Lovelace"),
        "GIT_AUTHOR_NAME contains no credential marker and must survive sanitization"
    );
    assert!(
        res.stdout.contains("GIT_AUTHOR_EMAIL=ada@example.com"),
        "GIT_AUTHOR_EMAIL contains no credential marker and must survive sanitization"
    );
    assert!(
        !res.stdout.contains("KROMA_TEST_HOME="),
        "KROMA_* variables must be stripped by the shared blacklist"
    );

    let _ = fs::remove_dir_all(&tmp_ws);
}
