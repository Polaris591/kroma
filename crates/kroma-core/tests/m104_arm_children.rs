//! M1-04, A1 (child half) and A3 (reclaim half): a bench arm's children belong to the arm's job.
//!
//! The bench's held-out `verify_command` is spawned by `ProcessSupervisor`, not by
//! `JobScope::spawn`, so these contracts drive the real supervisor with a job registry: the child
//! must be *recorded* while it runs, terminated through its recorded process group, and *checked*
//! afterwards. A child that survives reclaim makes the arm's verdict unknown — never a clean
//! rejection. No provider, no network, no paid call: every child here is `sleep`/`true`.

use std::path::PathBuf;
use std::time::Duration;

use kroma_core::bench::{BenchTask, VerifyCommandLimits};
use kroma_core::job_scope::ChildRegistry;
use kroma_core::process_supervisor::{OneShotCommandSpec, ProcessSupervisor, TerminationReason};

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("kroma-m104c-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn task_with_verify(argv: &[&str]) -> BenchTask {
    BenchTask {
        id: "arm-children".to_string(),
        prompt: "unused in this contract".to_string(),
        files: Default::default(),
        expect_contains: Vec::new(),
        expect_absent: Vec::new(),
        regression_guard: false,
        verify_command: Some(argv.iter().map(|arg| arg.to_string()).collect()),
    }
}

/// Poll the registry (never blocking the runtime, the spawned run needs the thread) until it
/// records a child or the budget runs out.
async fn wait_until_recorded(registry: &ChildRegistry, budget: Duration) -> Vec<u32> {
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        let alive = registry.alive();
        if !alive.is_empty() || tokio::time::Instant::now() >= deadline {
            return alive;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// A1 child half: the running verify child is in the job's registry, and A3: cancelling the run
/// leaves nothing behind once the job reclaims — checked, not assumed.
#[tokio::test]
async fn the_supervisor_records_its_child_and_the_job_reclaims_it() {
    let registry = ChildRegistry::default();
    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
    let spec = OneShotCommandSpec::new(
        vec!["sleep".to_string(), "60".to_string()],
        std::env::temp_dir(),
    )
    .with_timeout(Duration::from_secs(30))
    .with_child_registry(registry.clone());
    let run =
        tokio::spawn(
            async move { ProcessSupervisor::execute_one_shot(spec, Some(cancel_rx)).await },
        );

    let alive = wait_until_recorded(&registry, Duration::from_secs(10)).await;
    assert_eq!(
        alive.len(),
        1,
        "a child the supervisor spawned for this job is recorded in the job's registry"
    );
    let pid = alive[0];
    assert!(
        std::path::Path::new(&format!("/proc/{pid}")).exists(),
        "the recorded child is the running one"
    );

    cancel_tx.send(true).unwrap();
    let result = run.await.unwrap().unwrap();
    assert_eq!(result.terminated_by, Some(TerminationReason::Cancelled));

    registry.reclaim().unwrap();
    assert!(registry.alive().is_empty());
    assert!(
        !std::path::Path::new(&format!("/proc/{pid}")).exists(),
        "cancelling and reclaiming leaves no child behind"
    );
}

/// A3: a verify command the harness stops is terminated through its recorded group, the job
/// reclaims it, and the arm keeps no verdict either way.
#[tokio::test]
async fn a_timed_out_verify_command_is_reclaimed_and_the_arm_keeps_no_verdict() {
    let workspace = scratch("timeout");
    let registry = ChildRegistry::default();
    let limits = VerifyCommandLimits::new(Duration::from_millis(200), 64 * 1024);

    let evaluation = task_with_verify(&["sleep", "60"])
        .evaluate_with_sandbox(&workspace, &limits, None, Some(&registry))
        .await;

    let reason = evaluation
        .verdict_unknown
        .expect("a stopped verify is not a verdict");
    assert!(reason.contains("timed out"), "{reason}");
    assert!(!evaluation.resolved);
    assert!(
        registry.alive().is_empty(),
        "the arm's child is gone, not merely signalled"
    );
    let _ = std::fs::remove_dir_all(&workspace);
}

/// A3: a child the recorded group sweep cannot reach is named, stays registered, and makes the
/// arm's verdict unknown instead of a clean rejection.
#[tokio::test]
async fn an_arm_whose_child_survives_reclaim_has_no_verdict() {
    let workspace = scratch("survivor");
    let registry = ChildRegistry::default();
    // A child of *this* process, so its pid is not a process group id: `kill(-pid)` finds no group
    // and the survivor stands in for a reaping failure.
    let mut survivor = std::process::Command::new("sleep")
        .arg("60")
        .spawn()
        .unwrap();
    let pid = survivor.id();
    registry.register(pid, pid as i32);

    let limits = VerifyCommandLimits::new(Duration::from_secs(5), 64 * 1024);
    let evaluation = task_with_verify(&["true"])
        .evaluate_with_sandbox(&workspace, &limits, None, Some(&registry))
        .await;

    let reason = evaluation.verdict_unknown.unwrap_or_default();
    assert!(
        reason.contains(&pid.to_string()),
        "the surviving pid is named rather than assumed gone: {reason}"
    );
    assert!(!evaluation.resolved);
    assert_eq!(
        registry.alive(),
        vec![pid],
        "a survivor stays registered; the job does not forget what it could not reap"
    );

    let _ = survivor.kill();
    let _ = survivor.wait();
    let _ = std::fs::remove_dir_all(&workspace);
}
