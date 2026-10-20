//! KRV4-02: the hard timeout is actually hard.
//!
//! The defect this pins: `run_bounded` waited for the child to exit and only then read the pipes,
//! with no deadline on that read and no process-group cleanup. Two shapes broke it - a child that
//! filled a pipe while the parent waited (deadlock until timeout, reported as "did not finish") and a
//! grandchild that kept the pipe open after the leader exited (drain outliving the budget).

use std::time::{Duration, Instant};

use kroma_core::process_group::Containment;
use kroma_core::sandbox::{run_bounded, run_bounded_with_containment};

/// KRV4-02-A1: a grandchild holding stdout must not extend the budget. The leader exits at once, so
/// the only thing that could keep the call alive is the inherited pipe.
#[test]
fn the_budget_covers_the_drain_after_the_leader_exits() {
    let budget = Duration::from_millis(300);
    let started = Instant::now();
    let outcome = run_bounded(
        "sh",
        &["-c".to_string(), "sleep 5 & exit 0".to_string()],
        budget,
    );
    let elapsed = started.elapsed();
    assert!(
        outcome.is_ok(),
        "the leader exited successfully: {outcome:?}"
    );
    assert!(
        elapsed < budget + Duration::from_millis(1_000),
        "the call must return near the budget, not when the grandchild ends; took {elapsed:?}"
    );
}

/// KRV4-02-A2: output larger than a pipe buffer on both channels must not be mistaken for a hang.
/// Before the fix the parent waited for exit before reading, so this deadlocked and timed out.
#[test]
fn both_channels_are_drained_while_the_child_runs() {
    let budget = Duration::from_secs(5);
    let script =
        "head -c 2000000 /dev/zero | tr '\\0' 'a'; head -c 2000000 /dev/zero | tr '\\0' 'b' >&2";
    let started = Instant::now();
    let outcome = run_bounded("sh", &["-c".to_string(), script.to_string()], budget)
        .expect("a flooding child that exits must be reported, not timed out");
    let elapsed = started.elapsed();
    assert!(
        elapsed < budget,
        "flooding both pipes must not consume the whole budget; took {elapsed:?}"
    );
    assert!(outcome.success, "the child exited 0");
    assert!(
        outcome.truncated,
        "2 MB per channel exceeds the capture ceiling and must say so"
    );
    assert!(
        outcome.stdout.len() <= 64 * 1024 && outcome.stderr.len() <= 64 * 1024,
        "retained output stays inside the ceiling: {}/{}",
        outcome.stdout.len(),
        outcome.stderr.len()
    );
    assert!(
        outcome.stdout.starts_with('a') && outcome.stderr.starts_with('b'),
        "the retained prefix is the real output, not an empty buffer"
    );
}

/// The known Windows Job Object attach failure must refuse the launch rather than fail open.
#[test]
fn containment_attach_failure_is_fail_closed() {
    let started = Instant::now();
    let program = if cfg!(windows) { "cmd" } else { "sh" };
    let args = if cfg!(windows) {
        vec!["/C".to_string(), "ping -n 60 127.0.0.1".to_string()]
    } else {
        vec!["-c".to_string(), "sleep 60".to_string()]
    };
    let err = run_bounded_with_containment(
        program,
        &args,
        Duration::from_millis(100),
        Containment::ForceAttachFailure,
    )
    .expect_err("uncontained child must never execute");
    assert!(err.contains("cannot contain"), "{err}");
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "attach failure must not block"
    );
}

#[cfg(unix)]
#[test]
fn two_inherited_pipes_share_one_drain_budget() {
    let started = Instant::now();
    let budget = Duration::from_millis(20);
    let outcome = run_bounded(
        "sh",
        &[
            "-c".to_string(),
            "(sleep 3) >/dev/null & (sleep 3) 2>/dev/null & exit 0".to_string(),
        ],
        budget,
    );
    assert!(outcome.is_ok(), "{outcome:?}");
    assert!(
        started.elapsed() < Duration::from_millis(400),
        "pipes used separate grace periods"
    );
}
