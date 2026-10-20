//! C00-04: revocation — a lease that outlived its authority cannot be used for a new call.
//!
//! The defect this pins: activation handed out leases that never expired. Nothing could withdraw
//! authority, so "the capability was revoked" and "the capability is still callable" were the same
//! state, and the tool path had no idea a capability existed at all. Here a revocation is an epoch
//! (monotonic generation) rather than a flag: an authority issued before the epoch is dead forever,
//! a re-activation afterwards is a new grant, and the tool runtime refuses to dispatch *before* it
//! executes rather than reporting the refusal afterwards.

use kroma_core::activation::{
    ActivationError, ActivationRegistry, ActivationState, CapabilityClaim,
};
use kroma_core::budget::BudgetConfig;
use kroma_core::env::LocalHostEnv;
use kroma_core::messages::{ToolCall, ToolOutcome};
use kroma_core::tool::policy::{NonInteractiveApprovalHandler, PolicyEngine, PolicyMode};
use kroma_core::tool::registry::ToolRegistry;
use kroma_core::tool::runtime::ToolRuntime;
use kroma_core::types::{ErrorCode, ToolCallId};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Duration;

fn workspace(tag: &str) -> (PathBuf, PathBuf) {
    let root = std::env::temp_dir().join(format!("kroma-c0004-{tag}-{}", uuid::Uuid::new_v4()));
    let ws = root.join("ws");
    let state = root.join("state");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    (ws, state)
}

fn runtime_for(ws: &Path, state: &Path) -> ToolRuntime {
    ToolRuntime::new(
        ToolRegistry::new(None),
        PolicyEngine::new(
            PolicyMode::FullAutonomous,
            Arc::new(NonInteractiveApprovalHandler),
        ),
        Arc::new(LocalHostEnv::new(ws, state).unwrap()),
        BudgetConfig::default(),
    )
}

fn call(name: &str, args: serde_json::Value) -> ToolCall {
    ToolCall {
        call_id: ToolCallId::new(format!("call_{name}")),
        name: name.to_string(),
        raw_arguments: args.to_string(),
    }
}

/// Dispatch through the real tool path, returning the result and the policy decision label.
async fn dispatch(
    runtime: &ToolRuntime,
    name: &str,
    args: serde_json::Value,
) -> (kroma_core::messages::ToolResultMessage, String) {
    let (_tx, rx) = tokio::sync::watch::channel(false);
    let (message, decision, _hooks) = runtime.execute_call(&call(name, args), 0, rx).await;
    (message, decision)
}

fn write_args(path: &str) -> serde_json::Value {
    serde_json::json!({ "path": path, "content": "written\n" })
}

/// C00-04-①: after a revoke, the lease that was issued before it can tell that it is expired, and
/// the refusal names the capability. The revocation follows the dependency edge, so a lease chain
/// that reached the revoked capability through a parent is refused for that parent.
#[test]
fn a_revoked_capability_refuses_the_old_lease_and_names_itself() {
    let capabilities = ActivationRegistry::default();
    capabilities.declare("shell", &[], Arc::new(|| Ok(())));
    capabilities.declare("bench", &["shell"], Arc::new(|| Ok(())));

    let lease = capabilities.activate("bench").expect("bench activates");
    assert_eq!(
        lease.held().to_vec(),
        vec!["bench".to_string(), "shell".to_string()]
    );
    let claim = lease.claim();
    assert!(
        capabilities.authority(&claim).is_ok(),
        "a fresh claim carries authority"
    );
    assert!(lease.still_valid().is_ok());
    assert_eq!(capabilities.generation(), 0);

    let revoked = capabilities.revoke("shell");
    assert_eq!(
        revoked,
        vec!["shell".to_string(), "bench".to_string()],
        "the revoked id comes first, then the capabilities that depend on it"
    );
    assert_eq!(capabilities.generation(), 1);

    // Both authorities were issued for `bench`, so that is the capability they name; `shell` is
    // named by the revocation list above, which is where the cause is reported.
    let expired = capabilities
        .authority(&claim)
        .expect_err("the old claim must be refused");
    assert_eq!(expired.capability(), "bench");
    assert_eq!(expired.revoked_at(), 1);
    assert_eq!(expired.issued(), 0);
    assert!(
        expired.describe().contains("`bench`") && expired.describe().contains("revoked"),
        "the refusal names the revoked capability: {}",
        expired.describe()
    );

    let lease_expired = lease
        .still_valid()
        .expect_err("the lease covers the revoked dependency too");
    assert_eq!(lease_expired.capability(), "bench");
    assert!(
        !capabilities
            .state("bench")
            .is_some_and(|state| state == ActivationState::Active),
        "a revoked capability is not left active"
    );

    // The expired lease cannot pull the capability back up: dropping it only releases slots it
    // still owns, and the registry stays dormant.
    drop(lease);
    assert_eq!(capabilities.state("shell"), Some(ActivationState::Dormant));
    assert_eq!(capabilities.holders("shell"), Some(0));
    assert_eq!(capabilities.outstanding_leases(), 0);
}

/// C00-04-②: a gated tool is dispatchable while its claim is current and is refused — without
/// running — once the capability is revoked. The refusal is structured and names the capability, so
/// it is not a silent fall through to the bare registry.
#[tokio::test]
async fn a_revoked_capability_is_not_dispatchable() {
    let (ws, state) = workspace("dispatch");
    let capabilities = Arc::new(ActivationRegistry::default());
    capabilities.declare("workspace-write", &[], Arc::new(|| Ok(())));

    let runtime = runtime_for(&ws, &state).with_capabilities(Arc::clone(&capabilities));
    let lease = capabilities
        .activate("workspace-write")
        .expect("capability activates");
    runtime.gate_tools(lease.claim(), &["write"]);

    let (allowed, decision) = dispatch(&runtime, "write", write_args("before.txt")).await;
    assert!(
        allowed.outcome.is_success(),
        "a current claim still dispatches: {} ({decision})",
        allowed.content
    );
    assert_eq!(decision, "allow");
    assert!(ws.join("before.txt").exists());

    assert_eq!(
        capabilities.revoke("workspace-write"),
        vec!["workspace-write".to_string()]
    );

    let (denied, decision) = dispatch(&runtime, "write", write_args("after.txt")).await;
    assert!(
        !denied.outcome.is_success(),
        "a revoked tool must be refused"
    );
    assert_eq!(decision, "deny_capability_revoked");
    assert_eq!(
        denied.outcome,
        ToolOutcome::Error {
            code: ErrorCode::PolicyDenied
        }
    );
    assert!(
        denied.content.contains("workspace-write") && denied.content.contains("revoked"),
        "the refusal names the capability and the reason: {}",
        denied.content
    );
    assert!(
        !ws.join("after.txt").exists(),
        "the gate refuses before dispatch: the tool must not have run"
    );

    // An ungated tool is untouched by the gate; it is not a blanket block.
    let (untouched, decision) = dispatch(
        &runtime,
        "read",
        serde_json::json!({ "path": "before.txt" }),
    )
    .await;
    assert!(
        untouched.outcome.is_success(),
        "only the gated tool is withdrawn: {} ({decision})",
        untouched.content
    );

    let _ = std::fs::remove_dir_all(ws.parent().unwrap());
}

/// C00-04-③: re-activation after a revoke issues a new generation and works; the authority issued
/// before the revoke stays dead, even after the new generation is running.
#[tokio::test]
async fn reactivation_issues_a_new_generation_and_the_old_claim_stays_dead() {
    let (ws, state) = workspace("reactivate");
    let capabilities = Arc::new(ActivationRegistry::default());
    let initialisations = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&initialisations);
    capabilities.declare(
        "workspace-write",
        &[],
        Arc::new(move || {
            counted.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }),
    );

    let runtime = runtime_for(&ws, &state).with_capabilities(Arc::clone(&capabilities));
    let first = capabilities
        .activate("workspace-write")
        .expect("capability activates");
    let first_claim: CapabilityClaim = first.claim();
    runtime.gate_tools(first_claim.clone(), &["write"]);
    let first_generation = first.generation();

    capabilities.revoke("workspace-write");

    let second = capabilities
        .activate("workspace-write")
        .expect("a revoked capability can be activated again");
    assert!(
        second.generation() > first_generation,
        "re-activation must issue a new generation ({} -> {})",
        first_generation,
        second.generation()
    );
    assert_eq!(
        initialisations.load(Ordering::SeqCst),
        2,
        "revocation invalidates the state, so the initialiser runs again"
    );
    assert!(second.still_valid().is_ok());
    assert!(
        first.still_valid().is_err(),
        "the pre-revocation lease stays dead after the new activation"
    );
    assert!(capabilities.authority(&first_claim).is_err());

    // Dropping the expired lease must not release the new generation's hold.
    drop(first);
    assert_eq!(
        capabilities.holders("workspace-write"),
        Some(1),
        "the stale lease owns no holder slot in the new generation"
    );
    assert_eq!(
        capabilities.state("workspace-write"),
        Some(ActivationState::Active)
    );

    // The gate still holds the pre-revocation claim: refused even though the capability is active.
    let (denied, decision) = dispatch(&runtime, "write", write_args("a.txt")).await;
    assert!(!denied.outcome.is_success());
    assert_eq!(decision, "deny_capability_revoked");
    assert!(!ws.join("a.txt").exists());

    // A claim from the new lease restores dispatch.
    runtime.gate_tools(second.claim(), &["write"]);
    let (allowed, decision) = dispatch(&runtime, "write", write_args("a.txt")).await;
    assert!(
        allowed.outcome.is_success(),
        "the new authority dispatches: {} ({decision})",
        allowed.content
    );
    assert!(ws.join("a.txt").exists());

    drop(second);
    let _ = std::fs::remove_dir_all(ws.parent().unwrap());
}

/// C00-04-④a: a revoke that lands while the initialiser is running cannot be answered with success.
/// The attempt reports revocation, leaves the capability dormant, and hands out no lease.
#[test]
fn revoking_during_an_in_flight_initialisation_reports_revocation() {
    let capabilities = Arc::new(ActivationRegistry::default());
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let release = Arc::new(Barrier::new(2));
    let held_open = Arc::clone(&release);
    capabilities.declare(
        "slow",
        &[],
        Arc::new(move || {
            entered_tx.send(()).expect("initialiser signals entry");
            held_open.wait();
            Ok(())
        }),
    );

    let background = Arc::clone(&capabilities);
    let attempt = thread::spawn(move || {
        let outcome = background
            .activate("slow")
            .map(|lease| lease.id().to_string());
        (outcome.is_ok(), outcome.err())
    });

    entered_rx
        .recv()
        .expect("the initialiser must have started before the revoke");
    assert_eq!(capabilities.revoke("slow"), vec!["slow".to_string()]);
    release.wait();

    let (succeeded, error) = attempt.join().expect("the activating thread joins");
    assert!(
        !succeeded,
        "an activation that outlived a revoke must not report success"
    );
    match error {
        Some(ActivationError::Revoked { id, generation }) => {
            assert_eq!(id, "slow");
            assert_eq!(generation, capabilities.generation());
        }
        other => panic!("expected a revocation error, got {other:?}"),
    }
    assert_eq!(capabilities.state("slow"), Some(ActivationState::Dormant));
    assert_eq!(capabilities.holders("slow"), Some(0));
    assert_eq!(
        capabilities.outstanding_leases(),
        0,
        "no lease escaped the revoked attempt"
    );
}

/// C00-04-④b: revoke against calls that are already running. Every call that starts after the
/// revoke returned is refused, so "revoked" and "returned success" never overlap; calls that were
/// already inside the gate when the revoke landed are the in-flight boundary, not a violation.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_dispatch_never_succeeds_after_a_revoke_returns() {
    let (ws, state) = workspace("race");
    std::fs::write(ws.join("note.txt"), "content\n").unwrap();

    let capabilities = Arc::new(ActivationRegistry::default());
    capabilities.declare("workspace-read", &[], Arc::new(|| Ok(())));

    let runtime = Arc::new(runtime_for(&ws, &state).with_capabilities(Arc::clone(&capabilities)));
    let lease = capabilities
        .activate("workspace-read")
        .expect("capability activates");
    runtime.gate_tools(lease.claim(), &["read"]);

    let revoked = Arc::new(AtomicBool::new(false));
    let successes = Arc::new(AtomicUsize::new(0));
    let refused_after_flag = Arc::new(AtomicUsize::new(0));
    let violations = Arc::new(AtomicUsize::new(0));

    let mut workers = Vec::new();
    for _ in 0..8 {
        let runtime = Arc::clone(&runtime);
        let revoked = Arc::clone(&revoked);
        let successes = Arc::clone(&successes);
        let refused_after_flag = Arc::clone(&refused_after_flag);
        let violations = Arc::clone(&violations);
        workers.push(tokio::spawn(async move {
            // Call until the revoke lands, then keep calling so the post-revoke path is exercised.
            let mut seen_flag = 0;
            loop {
                // Read the flag before the call: a call that starts after the revoke returned must
                // be refused. Calls in flight when the revoke lands are the boundary, not a bug.
                let started_after_flag = revoked.load(Ordering::SeqCst);
                let (message, decision) =
                    dispatch(&runtime, "read", serde_json::json!({ "path": "note.txt" })).await;
                if message.outcome.is_success() {
                    successes.fetch_add(1, Ordering::SeqCst);
                    if started_after_flag {
                        violations.fetch_add(1, Ordering::SeqCst);
                    }
                } else {
                    assert_eq!(
                        decision, "deny_capability_revoked",
                        "the only refusal on this path is the revocation gate: {}",
                        message.content
                    );
                    if started_after_flag {
                        refused_after_flag.fetch_add(1, Ordering::SeqCst);
                    }
                }
                if revoked.load(Ordering::SeqCst) {
                    seen_flag += 1;
                    if seen_flag >= 4 {
                        break;
                    }
                }
                // `read` completes without yielding, so without this the dispatching tasks would
                // starve the timer that fires the revoke.
                tokio::task::yield_now().await;
            }
        }));
    }

    tokio::time::sleep(Duration::from_millis(20)).await;
    capabilities.revoke("workspace-read");
    revoked.store(true, Ordering::SeqCst);

    for worker in workers {
        worker.await.expect("the dispatching task joins");
    }

    assert_eq!(
        violations.load(Ordering::SeqCst),
        0,
        "no call may succeed once the revoke has returned"
    );
    assert!(
        successes.load(Ordering::SeqCst) > 0,
        "the gate must have let calls through before the revoke, otherwise this proves nothing"
    );
    assert!(
        refused_after_flag.load(Ordering::SeqCst) >= 8 * 3,
        "every worker must have been refused by the gate once the revoke returned, saw {}",
        refused_after_flag.load(Ordering::SeqCst)
    );

    let (message, decision) =
        dispatch(&runtime, "read", serde_json::json!({ "path": "note.txt" })).await;
    assert!(!message.outcome.is_success());
    assert_eq!(decision, "deny_capability_revoked");

    let _ = std::fs::remove_dir_all(ws.parent().unwrap());
}
