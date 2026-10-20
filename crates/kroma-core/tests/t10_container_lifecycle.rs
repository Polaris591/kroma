//! T10 (KR-16, KR-17): cancellation and probing end the *work*, not just the client.
//!
//! The real-container tests run against the local Docker daemon with `alpine:3.20`. If no runtime or
//! image is available they are skipped **loudly** (a printed reason), because argv assertions are not
//! a lifecycle test — that is exactly what the task card forbids.

use kroma_core::sandbox::{
    CleanupOutcome, CliOutcome, ContainerWorkload, ExecutionClass, ExecutionPolicy, ProbeTransport,
    SandboxBackend, SandboxError, SandboxSpec, SandboxTemplate, DEFAULT_CONTROL_BUDGET,
};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const IMAGE: &str = "alpine:3.20";

fn scratch(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("kroma-t10-{}-{}", tag, uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    root
}

fn docker_ready() -> bool {
    kroma_core::sandbox::run_bounded(
        "docker",
        &[
            "image".to_string(),
            "inspect".to_string(),
            IMAGE.to_string(),
        ],
        Duration::from_secs(10),
    )
    .map(|out| out.success)
    .unwrap_or(false)
}

/// A transport driven by a closure, so "the daemon hangs" and "version works but namespaces do not"
/// can be tested without installing a broken runtime.
struct FakeTransport<F>(F);

impl<F> ProbeTransport for FakeTransport<F>
where
    F: Fn(&str, &[String]) -> Result<CliOutcome, String> + Send + Sync,
{
    fn run(&self, program: &str, args: &[String], _budget: Duration) -> Result<CliOutcome, String> {
        (self.0)(program, args)
    }
}

fn ok_out(stdout: &str) -> CliOutcome {
    CliOutcome {
        success: true,
        truncated: false,
        stdout: stdout.to_string(),
        stderr: String::new(),
    }
}

fn fail_out(stderr: &str) -> CliOutcome {
    CliOutcome {
        success: false,
        truncated: false,
        stdout: String::new(),
        stderr: stderr.to_string(),
    }
}

/// A hung daemon must not turn into a hung run: the probe is cancelled within its budget.
#[test]
fn sandbox_probe_hang_is_cancelled_within_budget() {
    // A transport that does not answer within the budget it was given: it reports "killed", which is
    // what the real transport does to a hung child.
    let transport = FakeTransport(|_program: &str, _args: &[String]| {
        Err("did not finish within 200ms and was killed".to_string())
    });
    let spec = SandboxSpec::container("docker", "some-image", "/tmp/ws");
    let started = Instant::now();
    let err = spec
        .probe_within(&transport, Duration::from_millis(200))
        .expect_err("a hanging probe must fail");
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(5),
        "the probe must be bounded by its budget, took {elapsed:?}"
    );
    assert!(
        err.to_string().contains("did not finish") || err.to_string().contains("cannot"),
        "unexpected error: {err}"
    );
}

/// The real transport really kills a hung child instead of waiting for it.
#[test]
fn run_bounded_kills_a_hung_child() {
    let started = Instant::now();
    let err =
        kroma_core::sandbox::run_bounded("sleep", &["60".to_string()], Duration::from_millis(300))
            .expect_err("a hung child must be killed");
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(5),
        "run_bounded waited {elapsed:?} instead of killing the child"
    );
    assert!(err.contains("did not finish"), "unexpected error: {err}");
}

/// `--version` succeeding says nothing about isolation: a runtime that cannot start a container is
/// reported as unavailable, not assumed usable.
#[test]
fn version_success_but_namespace_failure_is_unavailable() {
    let transport = FakeTransport(|_program: &str, args: &[String]| {
        if args.iter().any(|a| a == "--version") {
            return Ok(ok_out("Docker version 26.1.5"));
        }
        if args.first().map(String::as_str) == Some("image") {
            return Ok(ok_out("[]"));
        }
        // The smoke run is where the truth shows up.
        Ok(fail_out("operation not permitted: unshare"))
    });
    let spec = SandboxSpec::container("docker", "some-image", "/tmp/ws");
    match spec.probe_within(&transport, Duration::from_secs(1)) {
        Err(SandboxError::NamespaceUnavailable(msg)) => {
            assert!(msg.contains("cannot start a container"), "got {msg}")
        }
        other => panic!("expected NamespaceUnavailable, got {other:?}"),
    }
}

/// A failed probe refuses the launch; it never yields a host argv.
#[test]
fn probe_never_falls_back_to_host_execution() {
    let policy = ExecutionPolicy::isolated(SandboxTemplate::new(SandboxBackend::Container {
        runtime: "kroma-no-such-runtime".to_string(),
        image: "kroma-no-such-image".to_string(),
    }));
    let err = policy
        .launch_argv(
            ExecutionClass::AgentBash,
            Path::new("/tmp/ws"),
            Path::new("/tmp/state"),
            "/bin/bash",
            &["-c".to_string(), "true".to_string()],
        )
        .expect_err("an unusable backend must refuse the launch");
    assert!(
        !matches!(err, SandboxError::InvalidSpec(_)),
        "the refusal must be about the backend, got {err}"
    );
    assert!(
        err.to_string().contains("refusing to run unsandboxed"),
        "the refusal must say it is fail-closed: {err}"
    );
}

/// A cleanup that cannot confirm removal keeps the container id and says how to retry. It must never
/// report `cleaned`.
#[test]
fn cleanup_failure_retains_container_id() {
    let state = scratch("cleanup-fail");
    let workload = ContainerWorkload::new("docker", IMAGE, "kroma-test", &state).unwrap();
    std::fs::write(workload.cidfile(), "abcdef123456\n").unwrap();

    // `rm` fails, so the workload is still there.
    let transport =
        FakeTransport(
            |_program: &str, args: &[String]| match args.first().map(String::as_str) {
                Some("stop") => Ok(ok_out("")),
                Some("rm") => Ok(fail_out("container is in use")),
                Some("inspect") => Ok(ok_out("abcdef123456")),
                _ => Ok(ok_out("")),
            },
        );
    let outcome = workload.cancel(&transport, Duration::from_secs(1));
    match &outcome {
        CleanupOutcome::RecoveryRequired {
            container_id,
            reason,
            retry_command,
        } => {
            assert_eq!(container_id.as_deref(), Some("abcdef123456"));
            assert!(reason.contains("remove failed"), "got {reason}");
            assert!(
                retry_command.contains(workload.name()),
                "the retry command must address the workload: {retry_command}"
            );
        }
        other => panic!("expected RecoveryRequired, got {other:?}"),
    }
    assert!(!outcome.is_cleaned(), "a failed cleanup is not cleaned");

    // `rm` succeeds but the daemon still reports the container: also not cleaned.
    let transport =
        FakeTransport(
            |_program: &str, args: &[String]| match args.first().map(String::as_str) {
                Some("stop") | Some("rm") => Ok(ok_out("")),
                Some("inspect") => Ok(ok_out("abcdef123456")),
                _ => Ok(ok_out("")),
            },
        );
    let outcome = workload.cancel(&transport, Duration::from_secs(1));
    assert!(
        matches!(outcome, CleanupOutcome::RecoveryRequired { .. }),
        "rm success without a confirmed removal is not cleaned: {outcome:?}"
    );

    // And the happy path does report cleaned, so the assertion above is not vacuous.
    let transport =
        FakeTransport(
            |_program: &str, args: &[String]| match args.first().map(String::as_str) {
                Some("stop") | Some("rm") => Ok(ok_out("")),
                Some("inspect") => Ok(fail_out("no such container")),
                _ => Ok(ok_out("")),
            },
        );
    let outcome = workload.cancel(&transport, Duration::from_secs(1));
    assert!(outcome.is_cleaned(), "expected Cleaned, got {outcome:?}");
    let _ = std::fs::remove_dir_all(&state);
}

/// A heartbeat written inside the container must stop when the run is cancelled, and the container
/// must be gone from the daemon — not merely detached from a client.
#[test]
fn cancel_leaves_no_live_container_or_grandchild() {
    if !docker_ready() {
        eprintln!(
            "SKIP cancel_leaves_no_live_container_or_grandchild: docker or {} is unavailable; \
             this test is the lifecycle gate and must run where a container runtime exists",
            IMAGE
        );
        return;
    }
    let root = scratch("cancel");
    let ws = root.join("ws");
    let state = root.join("state");
    std::fs::create_dir_all(&ws).unwrap();
    let spec = SandboxSpec::container("docker", IMAGE, &ws);
    let workload = ContainerWorkload::new("docker", IMAGE, "kroma-test", &state).unwrap();
    let transport = kroma_core::sandbox::ProcessProbeTransport;

    workload
        .create(
            &spec,
            "sh",
            &[
                "-c".to_string(),
                "while true; do date +%s%N >> /workspace/heartbeat.txt; sleep 0.2; done"
                    .to_string(),
            ],
            &transport,
            Duration::from_secs(30),
        )
        .expect("create");
    workload
        .start(&transport, Duration::from_secs(30))
        .expect("start");

    let heartbeat = ws.join("heartbeat.txt");
    let deadline = Instant::now() + Duration::from_secs(20);
    while !heartbeat.exists() {
        assert!(
            Instant::now() < deadline,
            "the container never wrote a heartbeat"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    let before = std::fs::metadata(&heartbeat).unwrap().len();
    assert!(before > 0, "the heartbeat must have content");

    let outcome = workload.cancel(&transport, Duration::from_secs(30));
    assert!(
        outcome.is_cleaned(),
        "cancel must confirm the cleanup, got {outcome:?}"
    );

    let stopped_at = std::fs::metadata(&heartbeat).unwrap().len();
    std::thread::sleep(Duration::from_millis(1500));
    let after = std::fs::metadata(&heartbeat).unwrap().len();
    assert_eq!(
        stopped_at, after,
        "the heartbeat kept growing after cancel: the work did not stop"
    );
    assert!(
        !workload
            .exists(&transport, Duration::from_secs(10))
            .unwrap_or(true),
        "the container must not exist after cancel"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// Killing the client process does not end the daemon's workload — which is exactly why cleanup is
/// addressed by name. This test demonstrates the defect and then the fix.
#[test]
fn kill_cli_still_cleans_daemon_workload() {
    if !docker_ready() {
        eprintln!(
            "SKIP kill_cli_still_cleans_daemon_workload: docker or {} is unavailable",
            IMAGE
        );
        return;
    }
    let root = scratch("killcli");
    let state = root.join("state");
    std::fs::create_dir_all(&state).unwrap();
    let name = format!("kroma-killtest-{}", std::process::id());
    let mut client = std::process::Command::new("docker")
        .args(["run", "--name", &name, IMAGE, "sleep", "300"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn the docker run client");
    std::thread::sleep(Duration::from_millis(1500));

    // Kill the client the way a supervisor kills a timed-out process group.
    client.kill().expect("kill the client");
    let _ = client.wait();

    let workload = ContainerWorkload::adopt("docker", IMAGE, &name, &state).unwrap();
    let transport = kroma_core::sandbox::ProcessProbeTransport;
    assert!(
        workload
            .exists(&transport, Duration::from_secs(10))
            .unwrap(),
        "the daemon workload outlives its client (this is the reason cleanup is by name)"
    );

    let outcome = workload.cancel(&transport, Duration::from_secs(30));
    assert!(
        outcome.is_cleaned(),
        "expected a confirmed cleanup: {outcome:?}"
    );
    assert!(
        !workload
            .exists(&transport, Duration::from_secs(10))
            .unwrap(),
        "the daemon workload must be gone"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// A read-only mount (the grading oracle) cannot be modified from inside the container.
#[test]
fn container_cannot_modify_oracle_mount() {
    if !docker_ready() {
        eprintln!(
            "SKIP container_cannot_modify_oracle_mount: docker or {} is unavailable",
            IMAGE
        );
        return;
    }
    let root = scratch("oracle");
    let ws = root.join("ws");
    let oracle = root.join("oracle");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::create_dir_all(&oracle).unwrap();
    let oracle_file = oracle.join("expected.txt");
    std::fs::write(&oracle_file, b"oracle").unwrap();

    let mut spec = SandboxSpec::container("docker", IMAGE, &ws);
    spec = spec.with_ro_extra(&oracle);
    let out = kroma_core::sandbox::run_bounded(
        "docker",
        &spec
            .wrap_argv(
                "sh",
                &[
                    "-c".to_string(),
                    format!("printf hacked > {}/expected.txt", oracle.display()),
                ],
            )
            .unwrap(),
        Duration::from_secs(60),
    )
    .expect("run");
    assert!(
        !out.success,
        "writing to the read-only oracle mount must fail, stdout={} stderr={}",
        out.stdout, out.stderr
    );
    assert_eq!(
        std::fs::read(&oracle_file).unwrap(),
        b"oracle",
        "the oracle file must be untouched"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// Two workloads never collide on a name or a cidfile, so cleanup can never hit the wrong container.
#[test]
fn workload_names_and_cidfiles_are_unique() {
    let state = scratch("unique");
    let mut names = BTreeSet::new();
    let mut cidfiles = BTreeSet::new();
    let seen = Mutex::new(0usize);
    for _ in 0..32 {
        let workload = ContainerWorkload::new("docker", IMAGE, "kroma-test", &state).unwrap();
        assert!(names.insert(workload.name().to_string()), "duplicate name");
        assert!(
            cidfiles.insert(workload.cidfile().to_path_buf()),
            "duplicate cidfile"
        );
        *seen.lock().unwrap() += 1;
    }
    assert_eq!(*seen.lock().unwrap(), 32);
    assert_eq!(DEFAULT_CONTROL_BUDGET, Duration::from_secs(15));
    let _ = std::fs::remove_dir_all(&state);
}
