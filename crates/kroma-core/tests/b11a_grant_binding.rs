//! B11a: approval grants are bound to the exact authorization scope (entry point, tool, run,
//! workspace) and, for one-shot / sensitive approvals, to the exact argument digest.
//!
//! The deterministic red for this card is `category_approval_does_not_leak_across_runs`: the
//! pre-fix `PolicyEngine` cached a category approval by tool name only, so a grant minted in run A
//! authorized run B on the same engine without any fresh prompt.

use kroma_core::agent_loop::build_default_runtime;
use kroma_core::budget::BudgetConfig;
use kroma_core::env::{ExecutionContext, LocalHostEnv};
use kroma_core::messages::{ToolCall, ToolOutcome};
use kroma_core::tool::policy::{
    canonical_arguments_digest, ApprovalDecision, ApprovalHandler, ApprovalRequest, AuthEntry,
    AuthScope, GrantRejection, PolicyCheckResult, PolicyEngine, PolicyMode,
};
use kroma_core::types::{EffectClass, RunId, ToolCallId};
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use uuid::Uuid;

struct CategoryApprover {
    decision: ApprovalDecision,
    calls: AtomicUsize,
}

impl ApprovalHandler for CategoryApprover {
    fn request_approval(&self, _req: &ApprovalRequest) -> ApprovalDecision {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.decision
    }

    fn is_interactive(&self) -> bool {
        true
    }
}

fn ctx_for(run_id: RunId, rx: &watch::Receiver<bool>) -> ExecutionContext {
    ExecutionContext::new(Some(run_id), None, Some(rx.clone()), None, None)
}

fn scope(entry: AuthEntry, run_id: RunId, ws: &Path) -> AuthScope {
    AuthScope {
        entry,
        tool_name: "write".to_string(),
        run_id: Some(run_id),
        workspace_root: ws.to_path_buf(),
        // B12a: a reusable category grant now requires a real workspace revision identity, so
        // these scope tests bind one (the subject under test is run/workspace/entry/digest, not the
        // revision — that boundary has its own `b12a_revision_binding` tests).
        revision: Some(kroma_core::revision::RevisionId::new(
            "r-00000000000000000000000000000000",
        )),
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

/// A category approval ("allow this tool for the current run") must not leak into a *different*
/// run: a second run on the same engine must be re-gated, not silently allowed.
#[tokio::test]
async fn category_approval_does_not_leak_across_runs() {
    let tmp_root = std::env::temp_dir().join(format!("kroma-b11a-{}", Uuid::new_v4()));
    let ws_dir = tmp_root.join("ws");
    let state_dir = tmp_root.join("state");
    fs::create_dir_all(&ws_dir).unwrap();

    let approver = Arc::new(CategoryApprover {
        decision: ApprovalDecision::AllowCategoryForRun,
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

    let call_a = ToolCall {
        call_id: ToolCallId::new("a1"),
        name: "write".into(),
        raw_arguments: r#"{"path":"a.txt","content":"A"}"#.into(),
    };
    let (out_a, label_a, _) = runtime
        .execute_call_with_context(&call_a, 0, &ctx_for(RunId::new(), &rx))
        .await;
    assert_eq!(label_a, "ask_approved", "first run must prompt and be approved");
    assert_eq!(out_a.outcome, ToolOutcome::Success);

    let call_b = ToolCall {
        call_id: ToolCallId::new("b1"),
        name: "write".into(),
        raw_arguments: r#"{"path":"b.txt","content":"B"}"#.into(),
    };
    let (_out_b, label_b, _) = runtime
        .execute_call_with_context(&call_b, 0, &ctx_for(RunId::new(), &rx))
        .await;
    assert_eq!(
        label_b, "ask_approved",
        "a category grant from run A must not authorize run B without a fresh prompt"
    );
    assert_eq!(approver.calls.load(Ordering::SeqCst), 2);

    let _ = fs::remove_dir_all(&tmp_root);
}

/// A category grant is scoped to the entry point that raised it: a `write` approved through the
/// kernel tool dispatcher must not authorize the same tool name through another authority path.
#[test]
fn category_grant_is_scoped_to_the_entry_point() {
    let ws = std::path::PathBuf::from("/ws");
    let run = RunId::new();
    let approver = Arc::new(CategoryApprover {
        decision: ApprovalDecision::AllowCategoryForRun,
        calls: AtomicUsize::new(0),
    });
    let engine = PolicyEngine::new(PolicyMode::Ask, approver);
    let tool_scope = scope(AuthEntry::Tool, run, &ws);

    let decision = engine.prompt_approval_scoped(
        &tool_scope,
        &write_req(&ws),
        Some("digest-a"),
        Some(Duration::from_secs(60)),
    );
    assert_eq!(decision, ApprovalDecision::AllowCategoryForRun);

    // Same run + tool + workspace, but a different entry point: not authorized.
    let tournament_scope = scope(AuthEntry::Tournament, run, &ws);
    assert_eq!(
        engine.evaluate_with_context(&tournament_scope, EffectClass::Mutating, false),
        PolicyCheckResult::Ask,
        "a grant from the tool entry must not authorize the tournament entry"
    );
    // The entry that owns the grant still sees it.
    assert_eq!(
        engine.evaluate_with_context(&tool_scope, EffectClass::Mutating, false),
        PolicyCheckResult::Allow,
    );
}

/// A category grant is scoped to the run and workspace that minted it, and expires with the run.
#[test]
fn category_grant_is_scoped_to_run_and_workspace() {
    let ws = std::path::PathBuf::from("/ws");
    let other_ws = std::path::PathBuf::from("/other");
    let run = RunId::new();
    let engine = PolicyEngine::new(
        PolicyMode::Ask,
        Arc::new(CategoryApprover {
            decision: ApprovalDecision::AllowCategoryForRun,
            calls: AtomicUsize::new(0),
        }),
    );
    let s = scope(AuthEntry::Tool, run, &ws);
    engine.prompt_approval_scoped(
        &s,
        &write_req(&ws),
        Some("d"),
        Some(Duration::from_secs(60)),
    );

    // A different run, and a different workspace, both re-ask.
    let other_run = scope(AuthEntry::Tool, RunId::new(), &ws);
    let other_workspace = scope(AuthEntry::Tool, run, &other_ws);
    assert_eq!(
        engine.evaluate_with_context(&other_run, EffectClass::Mutating, false),
        PolicyCheckResult::Ask
    );
    assert_eq!(
        engine.evaluate_with_context(&other_workspace, EffectClass::Mutating, false),
        PolicyCheckResult::Ask
    );

    // `invalidate_run` retires the grant precisely for that run.
    engine.invalidate_run(run);
    assert_eq!(
        engine.evaluate_with_context(&s, EffectClass::Mutating, false),
        PolicyCheckResult::Ask
    );
}

/// A one-shot approval is bound to the exact argument digest: the credential for arguments A must
/// be rejected for arguments B, and must not be replayable.
#[test]
fn one_shot_grant_is_digest_bound_and_single_use() {
    let ws = std::path::PathBuf::from("/ws");
    let run = RunId::new();
    let engine = PolicyEngine::new(
        PolicyMode::Ask,
        Arc::new(CategoryApprover {
            decision: ApprovalDecision::AllowOnce,
            calls: AtomicUsize::new(0),
        }),
    );
    let s = scope(AuthEntry::Tool, run, &ws);
    let decision = engine.prompt_approval_scoped(
        &s,
        &write_req(&ws),
        Some("digest-a"),
        Some(Duration::from_secs(60)),
    );
    assert_eq!(decision, ApprovalDecision::AllowOnce);

    // A different argument digest is not authorized by the approval for `digest-a`.
    assert_eq!(
        engine.consume_grant(&s, "digest-b"),
        Err(GrantRejection::DigestMismatch)
    );
    // The exact digest is authorized exactly once.
    assert_eq!(engine.consume_grant(&s, "digest-a"), Ok(()));
    // Replay is rejected (the grant was retired after its single use).
    assert_eq!(
        engine.consume_grant(&s, "digest-a"),
        Err(GrantRejection::Missing)
    );
}

/// A category grant is consumed atomically and re-asks once its use budget is exhausted.
#[test]
fn category_grant_use_budget_is_exhausted_exactly_once() {
    let ws = std::path::PathBuf::from("/ws");
    let run = RunId::new();
    let engine = PolicyEngine::new(
        PolicyMode::Ask,
        Arc::new(CategoryApprover {
            decision: ApprovalDecision::AllowCategoryForRun,
            calls: AtomicUsize::new(0),
        }),
    );
    let s = scope(AuthEntry::Tool, run, &ws);
    engine.prompt_approval_scoped(
        &s,
        &write_req(&ws),
        Some("d"),
        Some(Duration::from_secs(60)),
    );

    for i in 0..kroma_core::tool::policy::CATEGORY_GRANT_MAX_USES {
        assert_eq!(
            engine.consume_grant(&s, &format!("args-{i}")),
            Ok(()),
            "use {i} must be within budget"
        );
    }
    // The budget is exactly spent: the grant is retired and re-asks.
    assert_eq!(
        engine.consume_grant(&s, "args-final"),
        Err(GrantRejection::Missing)
    );
    assert_eq!(
        engine.evaluate_with_context(&s, EffectClass::Mutating, false),
        PolicyCheckResult::Ask,
        "an exhausted category grant must re-ask, not silently allow"
    );
}

/// An expired grant is rejected even if its scope and digest match.
#[test]
fn expired_grant_is_rejected() {
    let ws = std::path::PathBuf::from("/ws");
    let run = RunId::new();
    let engine = PolicyEngine::new(
        PolicyMode::Ask,
        Arc::new(CategoryApprover {
            decision: ApprovalDecision::AllowOnce,
            calls: AtomicUsize::new(0),
        }),
    );
    let s = scope(AuthEntry::Tool, run, &ws);

    // A zero remaining budget mints a grant that is already expired.
    engine.prompt_approval_scoped(&s, &write_req(&ws), Some("digest-a"), Some(Duration::ZERO));
    assert_eq!(
        engine.consume_grant(&s, "digest-a"),
        Err(GrantRejection::Expired)
    );
}

/// Without a run id + live deadline, a category answer must degrade to a single call rather than
/// become an unbounded cross-request grant.
#[test]
fn category_answer_without_run_metadata_degrades_to_one_shot() {
    let ws = std::path::PathBuf::from("/ws");
    let engine = PolicyEngine::new(
        PolicyMode::Ask,
        Arc::new(CategoryApprover {
            decision: ApprovalDecision::AllowCategoryForRun,
            calls: AtomicUsize::new(0),
        }),
    );
    let no_run = AuthScope {
        entry: AuthEntry::Tool,
        tool_name: "write".to_string(),
        run_id: None,
        workspace_root: ws.clone(),
        revision: None,
    };
    let decision = engine.prompt_approval_scoped(
        &no_run,
        &write_req(&ws),
        Some("digest-a"),
        Some(Duration::from_secs(60)),
    );
    assert_eq!(decision, ApprovalDecision::AllowOnce);
    assert_eq!(engine.consume_grant(&no_run, "digest-a"), Ok(()));
    assert_eq!(
        engine.consume_grant(&no_run, "digest-a"),
        Err(GrantRejection::Missing),
        "a degraded category answer must not be reusable"
    );
}

/// The canonical digest is order-independent for objects, preserves array order, and rejects
/// invalid JSON rather than hashing raw bytes.
#[test]
fn canonical_digest_normalizes_object_order_and_rejects_invalid_json() {
    let a = canonical_arguments_digest(r#"{"b":1,"a":[2,1]}"#).unwrap();
    let b = canonical_arguments_digest(r#"{"a":[2,1],"b":1}"#).unwrap();
    let c = canonical_arguments_digest(r#"{"a":[1,2],"b":1}"#).unwrap();
    assert_eq!(a, b, "object key order must not change the digest");
    assert_ne!(a, c, "array order must be preserved");
    assert!(
        canonical_arguments_digest("not json").is_none(),
        "invalid JSON must be rejected, not hashed as raw bytes"
    );
}

/// F1 (deterministic instance gate): a category decision that authorizes a dispatch must spend one
/// use *at the decision boundary itself*, not only via a later, separate `consume_grant`. With the
/// pre-fix source `evaluate_with_context` is a pure read, so the 33rd decision still returns
/// `Allow` and the 32-use budget is only a soft limit.
#[test]
fn category_grant_use_budget_is_enforced_at_decision_time() {
    let ws = std::path::PathBuf::from("/ws");
    let run = RunId::new();
    let engine = PolicyEngine::new(
        PolicyMode::Ask,
        Arc::new(CategoryApprover {
            decision: ApprovalDecision::AllowCategoryForRun,
            calls: AtomicUsize::new(0),
        }),
    );
    let s = scope(AuthEntry::Tool, run, &ws);
    engine.prompt_approval_scoped(
        &s,
        &write_req(&ws),
        Some("d"),
        Some(Duration::from_secs(60)),
    );
    for i in 0..kroma_core::tool::policy::CATEGORY_GRANT_MAX_USES {
        assert_eq!(
            engine.evaluate_with_context(&s, EffectClass::Mutating, false),
            PolicyCheckResult::Allow,
            "decision {i} must be within the {}-use budget",
            kroma_core::tool::policy::CATEGORY_GRANT_MAX_USES
        );
    }
    assert_eq!(
        engine.evaluate_with_context(&s, EffectClass::Mutating, false),
        PolicyCheckResult::Ask,
        "the decision after the use budget is spent must re-ask, not silently allow"
    );
}
