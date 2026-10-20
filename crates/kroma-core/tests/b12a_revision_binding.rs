//! B12a: an approval grant is bound to the active workspace *revision*, not merely to the
//! (mutation-stable) workspace root.
//!
//! The defect this card closes: the pre-fix `AuthScope` carried `workspace_root`, which does not
//! change when the workspace contents change. A category approval ("allow this tool for the run")
//! minted against one workspace state therefore stayed live after the run mutated the workspace,
//! and after any external/`--undo`/`revision.restore` change to the same root — silently widening
//! the operator's approval beyond the state they reviewed.
//!
//! These tests bind the scope to a real `RevisionId` and assert that a grant minted at revision A
//! is invisible at revision B. The seam-level case is the deterministic guard; the runtime case
//! drives the production `ToolRuntime` dispatch path end-to-end.

use kroma_core::agent_loop::build_default_runtime;
use kroma_core::budget::{BudgetConfig, RunDeadline};
use kroma_core::env::{ExecutionContext, LocalHostEnv};
use kroma_core::messages::{ToolCall, ToolOutcome};
use kroma_core::revision::RevisionId;
use kroma_core::tool::policy::{
    ApprovalDecision, ApprovalHandler, ApprovalRequest, AuthEntry, AuthScope, PolicyCheckResult,
    PolicyEngine, PolicyMode,
};
use kroma_core::types::{EffectClass, RunId, ToolCallId};
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::watch;
use uuid::Uuid;

const REV_A: &str = "r-11111111111111111111111111111111";
const REV_B: &str = "r-22222222222222222222222222222222";

struct CategoryApprover {
    calls: AtomicUsize,
}

impl ApprovalHandler for CategoryApprover {
    fn request_approval(&self, _req: &ApprovalRequest) -> ApprovalDecision {
        self.calls.fetch_add(1, Ordering::SeqCst);
        ApprovalDecision::AllowCategoryForRun
    }

    fn is_interactive(&self) -> bool {
        true
    }
}

fn scope_at(run_id: RunId, ws: &Path, revision: Option<&str>) -> AuthScope {
    AuthScope {
        entry: AuthEntry::Tool,
        tool_name: "write".to_string(),
        run_id: Some(run_id),
        workspace_root: ws.to_path_buf(),
        revision: revision.map(RevisionId::new),
    }
}

fn write_req(ws: &Path) -> ApprovalRequest {
    ApprovalRequest {
        tool_name: "write".to_string(),
        call_id: None,
        effect_class: EffectClass::Mutating,
        is_sensitive_path: false,
        target_path: None,
        diff_preview: None,
        command_preview: None,
        workspace_root: ws.to_path_buf(),
    }
}

fn ctx_at(run_id: RunId, rx: &watch::Receiver<bool>, revision: Option<&str>) -> ExecutionContext {
    // A live deadline makes the run identity + budget known, so an `AllowCategoryForRun` answer is
    // actually widened to a reusable grant (see `PolicyEngine::prompt_approval_scoped`).
    let deadline = RunDeadline::at(Instant::now() + Duration::from_secs(300));
    ExecutionContext::new(Some(run_id), Some(deadline), Some(rx.clone()), None, None)
        .with_revision(revision.map(RevisionId::new))
}

/// Deterministic guard: a category grant minted while the workspace was at revision A must not
/// authorize a dispatch at revision B. The `workspace_root` is identical in both scopes, so this
/// can only pass if the *revision* is what binds the grant.
#[test]
fn category_grant_is_invisible_after_the_revision_advances() {
    let engine = PolicyEngine::new(
        PolicyMode::Ask,
        Arc::new(CategoryApprover {
            calls: AtomicUsize::new(0),
        }),
    );
    let run_id = RunId::new();
    let ws = std::path::PathBuf::from("/ws");

    let at_a = scope_at(run_id, &ws, Some(REV_A));
    let decision = engine.prompt_approval_scoped(
        &at_a,
        &write_req(&ws),
        Some("digest-a"),
        Some(Duration::from_secs(60)),
    );
    assert_eq!(decision, ApprovalDecision::AllowCategoryForRun);

    // Same revision: the reusable grant is live and consumed atomically.
    assert_eq!(
        engine.evaluate_with_context(&at_a, EffectClass::Mutating, false),
        PolicyCheckResult::Allow
    );

    // Same run, same root, new revision: the operator's approval was for the old state.
    let at_b = scope_at(run_id, &ws, Some(REV_B));
    assert_eq!(
        engine.evaluate_with_context(&at_b, EffectClass::Mutating, false),
        PolicyCheckResult::Ask,
        "a category grant minted at revision A must not authorize revision B"
    );
    assert_eq!(
        engine.evaluate_with_context(&at_a, EffectClass::Mutating, false),
        PolicyCheckResult::Allow,
        "the A-revision grant is still live for its own revision (the miss above was not a fluke)"
    );

    // The reverse direction: a captured-revision grant is not visible to a revision-less scope.
    let no_rev = scope_at(run_id, &ws, None);
    assert_eq!(
        engine.evaluate_with_context(&no_rev, EffectClass::Mutating, false),
        PolicyCheckResult::Ask,
        "a grant minted at a real revision must not authorize a revision-less scope"
    );
}

/// A scope with no real revision can never widen a category answer into a reusable grant: it
/// degrades to a single `Once` call, so two unrelated `None` scopes cannot share category reuse.
#[test]
fn a_revisionless_scope_degrades_to_a_single_call() {
    let engine = PolicyEngine::new(
        PolicyMode::Ask,
        Arc::new(CategoryApprover {
            calls: AtomicUsize::new(0),
        }),
    );
    let run_id = RunId::new();
    let ws = std::path::PathBuf::from("/ws");

    let no_rev = scope_at(run_id, &ws, None);
    let decision = engine.prompt_approval_scoped(
        &no_rev,
        &write_req(&ws),
        Some("digest-a"),
        Some(Duration::from_secs(60)),
    );
    assert_eq!(
        decision,
        ApprovalDecision::AllowOnce,
        "without a real revision a category answer must degrade to a single call"
    );
    // No reusable category grant was minted, so a second decision does not hit one.
    assert_eq!(
        engine.evaluate_with_context(&no_rev, EffectClass::Mutating, false),
        PolicyCheckResult::Ask
    );
    // The degraded `Once` grant is still exactly one use for this call.
    assert_eq!(engine.consume_grant(&no_rev, "digest-a"), Ok(()));
    assert_eq!(
        engine.consume_grant(&no_rev, "digest-a"),
        Err(kroma_core::tool::policy::GrantRejection::Missing)
    );
}

/// End-to-end through the production dispatch path: the same run's second `write` after the
/// revision advanced is re-prompted instead of being silently covered by the first approval.
#[tokio::test]
async fn runtime_re_prompts_a_write_after_the_revision_advances() {
    let tmp_root = std::env::temp_dir().join(format!("kroma-b12a-{}", Uuid::new_v4()));
    let ws_dir = tmp_root.join("ws");
    let state_dir = tmp_root.join("state");
    fs::create_dir_all(&ws_dir).unwrap();

    let approver = Arc::new(CategoryApprover {
        calls: AtomicUsize::new(0),
    });
    let env = Arc::new(LocalHostEnv::new(&ws_dir, &state_dir).unwrap());
    let runtime = build_default_runtime(
        env,
        PolicyEngine::new(PolicyMode::Ask, approver.clone()),
        BudgetConfig::default(),
        None,
    );
    let (_tx, rx) = watch::channel(false);
    let run_id = RunId::new();

    let call_a = ToolCall {
        call_id: ToolCallId::new("a1"),
        name: "write".into(),
        raw_arguments: r#"{"path":"a.txt","content":"A"}"#.into(),
    };
    let (out_a, label_a, _) = runtime
        .execute_call_with_context(&call_a, 0, &ctx_at(run_id, &rx, Some(REV_A)))
        .await;
    assert_eq!(label_a, "ask_approved");
    assert_eq!(out_a.outcome, ToolOutcome::Success);
    assert_eq!(approver.calls.load(Ordering::SeqCst), 1);

    // Same revision: the widened category grant is reused without a second prompt (control — the
    // test below is about the revision boundary, not about category grants being single-use).
    let call_a2 = ToolCall {
        call_id: ToolCallId::new("a2"),
        name: "write".into(),
        raw_arguments: r#"{"path":"a2.txt","content":"A2"}"#.into(),
    };
    let (_out_a2, label_a2, _) = runtime
        .execute_call_with_context(&call_a2, 0, &ctx_at(run_id, &rx, Some(REV_A)))
        .await;
    assert_eq!(label_a2, "allow");
    assert_eq!(approver.calls.load(Ordering::SeqCst), 1);

    // The run mutated the workspace, so its active revision is now B.
    let call_b = ToolCall {
        call_id: ToolCallId::new("b1"),
        name: "write".into(),
        raw_arguments: r#"{"path":"b.txt","content":"B"}"#.into(),
    };
    let (out_b, label_b, _) = runtime
        .execute_call_with_context(&call_b, 0, &ctx_at(run_id, &rx, Some(REV_B)))
        .await;
    assert_eq!(
        label_b, "ask_approved",
        "the second write must be re-prompted once the revision advanced"
    );
    assert_eq!(out_b.outcome, ToolOutcome::Success);
    assert_eq!(
        approver.calls.load(Ordering::SeqCst),
        2,
        "a category approval for revision A must not cover revision B"
    );

    let _ = fs::remove_dir_all(&tmp_root);
}
