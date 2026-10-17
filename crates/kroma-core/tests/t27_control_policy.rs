//! T27: the control policy's `Probe` action must be the *isolated* probe, and nothing else.
//!
//! The policy decides from counters only (no model confidence). This test closes the loop: when the
//! policy says `Probe`, the only entry point used is
//! `AttributionEngine::verify_candidate_in_isolated_worktree`, which materialises a throwaway worktree.
//! The assertions are that the probe never sees the parent path, that the parent's bytes are untouched
//! afterwards, and that the worktree is gone — plus the negative: an unauthorized policy decision never
//! invokes the probe at all.

use kroma_core::attribution::{
    AttributionCandidate, AttributionEngine, MutatedFileSpan, MutationLedger,
};
use kroma_core::control_policy::{
    decide, ControlAction, ControlObservation, ControlPolicyConfig, StopReason,
};
use kroma_core::revision::SnapshotStore;
use kroma_core::workspace::WorkspaceCap;
use std::cell::{Cell, RefCell};
use std::path::PathBuf;

fn scratch(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("kroma-t27-{}-{}", tag, uuid::Uuid::new_v4()));
    std::fs::create_dir_all(root.join("ws")).unwrap();
    root
}

fn observation(failures: u32, authorized: bool, isolation: bool) -> ControlObservation {
    ControlObservation {
        consecutive_failures: failures,
        identical_failure_streak: 1,
        remaining_tokens: 200_000,
        probes_used: 0,
        probe_authorized: authorized,
        isolation_available: isolation,
    }
}

/// A `Probe` decision runs on a copy and leaves the live workspace byte-identical.
#[test]
fn a_probe_action_runs_only_on_an_isolated_copy() {
    let root = scratch("probe");
    let ws_dir = root.join("ws");
    let ws = WorkspaceCap::new(&ws_dir).unwrap();
    let store = SnapshotStore::new(root.join("snapshots"));

    std::fs::write(ws_dir.join("lib.rs"), "pub fn compute() -> u32 { 1 }\n").unwrap();
    let rev_before = store
        .capture_revision(&ws, None, None, 0)
        .unwrap()
        .revision_id;
    std::fs::write(ws_dir.join("lib.rs"), "pub fn compute_v2() -> u32 { 1 }\n").unwrap();
    let rev_after = store
        .capture_revision(&ws, Some(&rev_before), None, 1)
        .unwrap()
        .revision_id;

    let mut ledger = MutationLedger::new();
    ledger.record_mutation(
        1,
        "edit",
        "call_rename",
        Some("ev-0001-call_rename".to_string()),
        Some(rev_before.clone()),
        vec![MutatedFileSpan {
            relative_path: "lib.rs".to_string(),
            start_line: 1,
            end_line: 1,
            added_symbols: vec!["compute_v2".to_string()],
            removed_symbols: vec!["compute".to_string()],
            summary: "edit lib.rs lines 1-1".to_string(),
        }],
    );
    ledger.backfill_revision_after(1, Some(rev_after.clone()));
    let mut report = AttributionEngine::attribute_failure(
        &ledger,
        "error[E0425]: cannot find function `compute` in this scope\n  --> caller.rs:4:5\n",
        "completion_gate",
        3,
    );

    // 1. The policy authorizes a probe at three consecutive failures with an explicit cap.
    let config = ControlPolicyConfig::with_probes(1);
    let decision = decide(&config, &observation(3, true, true));
    assert_eq!(decision.action, ControlAction::Probe, "{decision:?}");
    assert!(decision.stop_reason.is_none());

    // 2. The only probe entry point is the isolated one, so the decision can be executed as decided.
    let parent_root = ws.root().to_path_buf();
    let worktree_root = root.join("wt-t27");
    let probed_root = RefCell::new(PathBuf::new());
    let existed_during_probe = Cell::new(false);
    let verification = AttributionEngine::verify_candidate_in_isolated_worktree(
        &store,
        &ws,
        Some(&rev_after),
        &mut report.candidates[0],
        &rev_after,
        worktree_root.clone(),
        |probe_ws| {
            *probed_root.borrow_mut() = probe_ws.root().to_path_buf();
            existed_during_probe.set(worktree_root.exists());
            true
        },
    )
    .expect("isolated verification succeeds");

    assert_ne!(
        *probed_root.borrow(),
        parent_root,
        "the probe must run against a copy, not the live workspace"
    );
    assert!(
        existed_during_probe.get(),
        "the probe ran without a materialised worktree"
    );
    assert!(verification.failure_resolved_on_revert);
    assert_eq!(
        std::fs::read_to_string(ws_dir.join("lib.rs")).unwrap(),
        "pub fn compute_v2() -> u32 { 1 }\n",
        "the live workspace must be byte-identical after the probe"
    );
    assert!(!worktree_root.exists(), "the worktree must be cleaned up");

    // 3. The executed probe is recorded as evidence, not just inferred.
    assert!(
        report.candidates[0]
            .rationale
            .contains("verified in isolated worktree"),
        "the probe outcome must be recorded: {}",
        report.candidates[0].rationale
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// An unauthorized decision never reaches the probe, and a missing isolation stops instead.
#[test]
fn unauthorized_or_unisolated_probes_are_never_executed() {
    let config = ControlPolicyConfig::with_probes(2);

    let unauthorized = decide(&config, &observation(5, false, true));
    assert_eq!(unauthorized.action, ControlAction::Inspect);
    assert!(unauthorized.stop_reason.is_none());

    let unisolated = decide(&config, &observation(5, true, false));
    assert_eq!(unisolated.action, ControlAction::Stop);
    assert_eq!(
        unisolated.stop_reason,
        Some(StopReason::IsolationUnavailable)
    );

    // Probing disabled entirely (the production default): Inspect, never Probe.
    let disabled = ControlPolicyConfig::default();
    assert_eq!(
        decide(&disabled, &observation(9, true, true)).action,
        ControlAction::Inspect
    );

    // A budget that cannot cover the probe keeps it at Inspect rather than half-running it.
    let poor = ControlObservation {
        remaining_tokens: config.min_remaining_tokens_for_probe - 1,
        ..observation(9, true, true)
    };
    assert_eq!(decide(&config, &poor).action, ControlAction::Inspect);
}

/// A candidate that never resolved still costs, and the summary says so.
#[test]
fn every_candidate_is_accounted_including_the_losers() {
    use kroma_core::control_policy::{CandidateRecord, CandidateSummary};
    let records = [
        CandidateRecord {
            candidate_id: "winner".to_string(),
            tokens_spent: 4_000,
            resolved: true,
            diversity_score: 30,
        },
        CandidateRecord {
            candidate_id: "loser-a".to_string(),
            tokens_spent: 5_000,
            resolved: false,
            diversity_score: 70,
        },
    ];
    let summary = CandidateSummary::from_records(&records);
    assert_eq!(summary.total_tokens, 9_000);
    assert_eq!(
        summary.wasted_on_losers, 5_000,
        "the losing candidate cost more than the winner; the total must show it"
    );
    assert_eq!(
        summary.resolved_per_100k_tokens(),
        Some(100_000.0 / 9_000.0 * 1.0)
    );
    let _ = AttributionCandidate {
        mutation_id: String::new(),
        turn: 0,
        tool_name: String::new(),
        relative_path: None,
        line_range: None,
        source_entry_id: None,
        revision_before: None,
        revision_after: None,
        confidence: 0.0,
        rationale: String::new(),
    };
}
