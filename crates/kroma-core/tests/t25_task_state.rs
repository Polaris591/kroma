//! T25 (KR-31): typed task state survives compaction and resume; claims do not become facts by being
//! written down.
//!
//! Stage 1 of the plan adds structure only — no generated summaries. These tests pin the four things
//! that could go wrong with a "smart" summary layer:
//!
//! * an early user constraint being lost when the context is compacted;
//! * a path that merely appears in tool output being presented as the cause;
//! * compression being reported as free because only the compressor call was counted;
//! * an unsupported claim being promoted to a fact.

use kroma_core::budget::BudgetConfig;
use kroma_core::context_projector::ContextProjector;
use kroma_core::messages::{
    AssistantTurn, FinishReason, ToolCall, ToolOutcome, ToolResultMessage, ToolResultMeta,
    TranscriptItem, UserMessage,
};
use kroma_core::session::SessionJournal;
use kroma_core::task_state::{
    CompressionCost, EvidenceKind, EvidenceRef, FailureHypothesis, HypothesisStatus,
    SemanticCompression, TaskState,
};
use kroma_core::types::ToolCallId;
use std::sync::Arc;

fn scratch(tag: &str) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!("kroma-t25-{}-{}", tag, uuid::Uuid::new_v4()));
    std::fs::create_dir_all(root.join("ws")).unwrap();
    std::fs::create_dir_all(root.join("state")).unwrap();
    root
}

fn user(text: &str) -> TranscriptItem {
    TranscriptItem::User(UserMessage {
        content: text.into(),
    })
}

fn tool_result(call_id: &str, tool: &str, content: &str) -> TranscriptItem {
    TranscriptItem::ToolResult(ToolResultMessage {
        call_id: ToolCallId::new(call_id),
        tool_name: tool.to_string(),
        outcome: ToolOutcome::Success,
        content: Arc::from(content),
        meta: ToolResultMeta {
            relative_path: Some("src/a.rs".to_string()),
            ..Default::default()
        },
    })
}

fn assistant_with(call_id: &str, tool: &str) -> TranscriptItem {
    TranscriptItem::Assistant(AssistantTurn {
        content: None,
        reasoning_blocks: Vec::new(),
        tool_calls: vec![ToolCall {
            call_id: ToolCallId::new(call_id),
            name: tool.to_string(),
            raw_arguments: "{}".into(),
        }],
        finish_reason: FinishReason::ToolCalls,
        usage: None,
    })
}

/// The constraint the user stated first must survive heavy compaction and a resume.
#[test]
fn early_user_constraint_survives_compaction_and_resume() {
    let root = scratch("constraint");
    let ws = root.join("ws");
    std::fs::write(ws.join("lib.rs"), "pub fn a() {}\n").unwrap();

    let mut transcript = vec![user(
        "Fix the failing test. Do not modify anything under tests/. Verify with cargo test -p kroma-core.",
    )];
    // Pad with enough tool output to force compaction of the early turns.
    for i in 0..8 {
        transcript.push(assistant_with(&format!("c{i}"), "read"));
        transcript.push(tool_result(
            &format!("c{i}"),
            "read",
            &format!("src/module{i}.rs line\n").repeat(400),
        ));
    }

    let budget = BudgetConfig {
        context_high_watermark_bytes: 32 * 1024,
        context_low_watermark_bytes: 16 * 1024,
        context_tail_window_bytes: 512,
        context_min_tail_turns: 1,
        max_total_tool_output_bytes: 8 * 1024 * 1024,
        ..Default::default()
    };
    let mut projector = ContextProjector::new();
    let projected = projector
        .project(&mut transcript, &budget, 0, None, 40)
        .unwrap();
    assert!(
        projected.compaction_event.is_some(),
        "the fixture must actually compact, otherwise this test proves nothing"
    );

    // The state is derived from the *canonical* transcript, so compaction cannot remove it.
    let state = TaskState::derive(&transcript);
    assert_eq!(
        state.constraints.len(),
        1,
        "expected exactly one constraint: {state:?}"
    );
    let constraint = &state.constraints[0];
    assert!(
        constraint
            .statement
            .contains("Do not modify anything under tests/"),
        "the constraint must be kept verbatim: {}",
        constraint.statement
    );
    assert_eq!(constraint.provenance.canonical_index, 0);
    assert!(constraint.provenance.quote.contains("Do not modify"));
    assert!(
        state.constraint_lines()[0].contains("user:"),
        "the compacted view must show the user's own words"
    );

    // Resume: the canonical transcript comes back from the WAL, so the state is identical.
    let mut journal = SessionJournal::create(root.join("state/sessions"), &ws).unwrap();
    for (turn, item) in transcript.iter().cloned().enumerate() {
        journal.append_item(turn, item).unwrap();
    }
    let path = journal.path().to_path_buf();
    drop(journal);
    let reopened = SessionJournal::open(&path).expect("reopen");
    let restored = TaskState::derive(&reopened.active_transcript());
    assert_eq!(
        restored.constraints, state.constraints,
        "a resume must not lose or rewrite the user's constraint"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// A path that appears in tool output is a mention, not a cause.
#[test]
fn path_mentions_do_not_claim_causal_truth() {
    let transcript = vec![
        user("why does the build fail?"),
        assistant_with("c1", "read"),
        tool_result(
            "c1",
            "read",
            "error in src/parser.rs: unexpected token\nsee also tests/parse_test.rs\n",
        ),
    ];
    let state = TaskState::derive(&transcript);
    assert!(
        state
            .path_mentions
            .iter()
            .any(|m| m.path == "src/parser.rs"),
        "the path should be recorded: {:?}",
        state.path_mentions
    );
    for mention in &state.path_mentions {
        assert!(
            mention.mention_only,
            "a lexical path match must be tagged mention_only, not presented as a cause"
        );
    }
    // Nothing in the derivation turns a mention into a hypothesis.
    assert!(state.failure_hypotheses.is_empty());
    assert!(state.supported_facts().is_empty());
}

/// An unsupported claim is never promoted to a fact, no matter how often it is repeated.
#[test]
fn unsupported_summary_claim_is_not_promoted_to_fact() {
    let mut state = TaskState::default();
    let mut hypothesis = FailureHypothesis::new("the failure is a race in the scheduler");
    assert_eq!(hypothesis.status, HypothesisStatus::Unverified);
    assert!(!hypothesis.is_fact(), "a fresh hypothesis is not a fact");

    // Writing it down again does not help.
    state.add_hypothesis(hypothesis.clone());
    assert!(state.supported_facts().is_empty());

    // "Support" without evidence keeps it unverified.
    hypothesis.status = HypothesisStatus::Supported;
    assert!(
        !hypothesis.is_fact(),
        "supported with no evidence is still not a fact"
    );

    // Only supported *with* an evidence reference becomes a fact.
    hypothesis.support(EvidenceRef {
        kind: EvidenceKind::ToolResult,
        reference: "12".to_string(),
    });
    assert!(hypothesis.is_fact());

    // Contradiction wins: a contradicted claim is not a fact even with evidence.
    let mut contradicted = FailureHypothesis::new("the parser is slow");
    contradicted.support(EvidenceRef {
        kind: EvidenceKind::Verification,
        reference: "cargo test".to_string(),
    });
    contradicted.contradict(EvidenceRef {
        kind: EvidenceKind::ToolResult,
        reference: "13".to_string(),
    });
    assert_eq!(contradicted.status, HypothesisStatus::Contradicted);
    assert!(!contradicted.is_fact());
}

/// Compression cost includes the compressor *and* the rereads/cache rebuilds it causes.
#[test]
fn summary_cost_includes_compressor_and_rereads() {
    // The default configuration has no compressor at all.
    assert_eq!(
        SemanticCompression::default(),
        SemanticCompression::Disabled
    );
    let structural = CompressionCost::structural_only();
    assert_eq!(structural.total_tokens(), 0);
    assert!(structural.compressor_disabled);
    structural.validate().expect("a structural step is valid");

    // A compressor call that causes a reread and a cache rebuild is not free.
    let real = CompressionCost {
        compressor_input_tokens: 2_000,
        compressor_output_tokens: 300,
        reread_bytes: 64 * 1024,
        cache_rebuilds: 1,
        compressor_disabled: false,
    };
    assert_eq!(real.total_tokens(), 2_300);
    let described = real.describe();
    assert!(described.contains("2300 tokens"), "{described}");
    assert!(described.contains("reread=65536"), "{described}");
    assert!(described.contains("cache_rebuilds=1"), "{described}");

    // A disabled compressor that still bills is a bug, not a cheaper variant.
    let inconsistent = CompressionCost {
        compressor_input_tokens: 10,
        compressor_disabled: true,
        ..CompressionCost::default()
    };
    assert!(inconsistent.validate().is_err());

    // The default stays off until holdout evidence exists (see ADR-0002).
    assert_ne!(
        SemanticCompression::Disabled,
        SemanticCompression::SchemaConstrained
    );
}
