//! T14 (KR-10): the cache breakpoint must follow the growing history, and the cache accounting must
//! tell a write apart from a read.
//!
//! The defect being pinned: the wire breakpoint was always taken from `PinnedBaselineEnd` — the run's
//! frozen baseline, computed once and never moved. Everything the session appended afterwards was
//! therefore re-sent as uncached input on every turn, which is the opposite of what a prefix cache is
//! for. The second half is bookkeeping: `cache_creation_input_tokens` was folded into `input_tokens`
//! and then reported as *uncached*, so a cold cache (all writes, no reads) looked like a normal turn
//! and a warm cache's writes were invisible.

use kroma_core::budget::BudgetConfig;
use kroma_core::context_projector::ContextProjector;
use kroma_core::messages::{
    AssistantTurn, CacheBreakpoint, FinishReason, TokenUsage, ToolCall, ToolOutcome,
    ToolResultMessage, ToolResultMeta, TranscriptItem, UserMessage,
};
use kroma_core::provider::anthropic_messages::serialize_anthropic_request_body;
use kroma_core::tool::definition::ToolDefinition;
use kroma_core::types::ToolCallId;
use std::sync::Arc;

fn turn(i: usize) -> Vec<TranscriptItem> {
    vec![
        TranscriptItem::User(UserMessage {
            content: format!("question {i}"),
        }),
        TranscriptItem::Assistant(AssistantTurn {
            content: Some(format!("answer {i}")),
            reasoning_blocks: Vec::new(),
            tool_calls: Vec::new(),
            finish_reason: FinishReason::Stop,
            usage: None,
        }),
    ]
}

fn transcript(turns: usize) -> Vec<TranscriptItem> {
    let mut items = Vec::new();
    for i in 0..turns {
        items.extend(turn(i));
    }
    items
}

fn budget() -> BudgetConfig {
    BudgetConfig {
        // Generous watermarks: this test is about the breakpoint, not about compaction.
        context_high_watermark_bytes: 8 * 1024 * 1024,
        context_low_watermark_bytes: 4 * 1024 * 1024,
        // A small tail window, so the stable prefix exists at all: with a huge window the whole
        // transcript is "tail" and the breakpoint would legitimately sit at 0.
        context_tail_window_bytes: 120,
        context_min_tail_turns: 1,
        ..Default::default()
    }
}

fn breakpoint_of(
    projected: &kroma_core::context_projector::ProjectedContext,
    kind: CacheBreakpoint,
) -> usize {
    projected
        .breakpoints
        .iter()
        .find(|m| m.breakpoint == kind)
        .unwrap_or_else(|| panic!("missing {kind:?} breakpoint"))
        .message_index
}

/// The breakpoint advances with the history instead of sitting at the frozen baseline.
#[test]
fn pre_compaction_history_breakpoint_advances() {
    let short = {
        let mut items = transcript(6);
        let mut projector = ContextProjector::new();
        let projected = projector
            .project(&mut items, &budget(), 0, None, 1)
            .unwrap();
        (
            breakpoint_of(&projected, CacheBreakpoint::PinnedBaselineEnd),
            breakpoint_of(&projected, CacheBreakpoint::TailWindowStart),
        )
    };
    let long = {
        let mut items = transcript(24);
        let mut projector = ContextProjector::new();
        let projected = projector
            .project(&mut items, &budget(), 0, None, 1)
            .unwrap();
        (
            breakpoint_of(&projected, CacheBreakpoint::PinnedBaselineEnd),
            breakpoint_of(&projected, CacheBreakpoint::TailWindowStart),
        )
    };

    assert!(
        long.1 > short.1,
        "the tail-window breakpoint must advance with the history: {} -> {}",
        short.1,
        long.1
    );
    // The baseline is a floor, never the ceiling.
    assert!(
        long.0 <= long.1,
        "the pinned baseline must not sit past the advancing breakpoint"
    );

    // And the wire layer picks the advanced one: the furthest marker wins.
    let mut items = transcript(24);
    let mut projector = ContextProjector::new();
    let projected = projector
        .project(&mut items, &budget(), 0, None, 1)
        .unwrap();
    let body = serialize_anthropic_request_body(
        "m",
        "system",
        &[],
        &projected.messages,
        &projected.breakpoints,
        false,
        1024,
    );
    let value: serde_json::Value = serde_json::from_str(&body).unwrap();
    let cached_indices: Vec<usize> = value["messages"]
        .as_array()
        .unwrap()
        .iter()
        .enumerate()
        .filter(|(_, m)| m["content"].to_string().contains("cache_control"))
        .map(|(i, _)| i)
        .collect();
    assert!(
        cached_indices.len() >= 2,
        "expected a prefix and a tail cache marker, got {cached_indices:?}"
    );
    let last = value["messages"].as_array().unwrap().len() - 1;
    assert_eq!(
        *cached_indices.last().unwrap(),
        last,
        "the newest message must carry the tail marker"
    );
    assert!(
        cached_indices[0] > 1,
        "the prefix marker must have advanced past the early messages: {cached_indices:?}"
    );
}

/// Projecting the same transcript twice is byte-identical, so a cache built on it is reusable.
#[test]
fn prefix_bytes_stable_between_compactions() {
    let mut items = transcript(12);
    let mut a = ContextProjector::new();
    let projected_a = a
        .project(&mut items.clone(), &budget(), 0, None, 1)
        .unwrap();
    let mut b = ContextProjector::new();
    let projected_b = b.project(&mut items, &budget(), 0, None, 1).unwrap();

    assert_eq!(projected_a.breakpoints, projected_b.breakpoints);
    assert_eq!(projected_a.messages.len(), projected_b.messages.len());
    for (i, (x, y)) in projected_a
        .messages
        .iter()
        .zip(projected_b.messages.iter())
        .enumerate()
    {
        assert_eq!(
            serde_json::to_string(x).unwrap(),
            serde_json::to_string(y).unwrap(),
            "message {i} differs between two projections of the same transcript"
        );
    }

    // Appending turns must not rewrite the existing prefix: that is what makes the cached prefix
    // reusable across turns.
    let mut longer = transcript(18);
    let mut c = ContextProjector::new();
    let projected_c = c.project(&mut longer, &budget(), 0, None, 1).unwrap();
    let n = projected_a.messages.len();
    for i in 0..n {
        assert_eq!(
            serde_json::to_string(&projected_a.messages[i]).unwrap(),
            serde_json::to_string(&projected_c.messages[i]).unwrap(),
            "appending turns rewrote prefix message {i}"
        );
    }
}

/// A tool batch is merged into one wire message; a breakpoint that indexes the transcript must be
/// translated, not used directly.
#[test]
fn wire_role_merge_preserves_boundary_mapping() {
    let mut items = vec![
        TranscriptItem::User(UserMessage {
            content: "run the tools".into(),
        }),
        TranscriptItem::Assistant(AssistantTurn {
            content: Some("calling".into()),
            reasoning_blocks: Vec::new(),
            tool_calls: vec![
                ToolCall {
                    call_id: ToolCallId::new("c1"),
                    name: "read".to_string(),
                    raw_arguments: "{}".into(),
                },
                ToolCall {
                    call_id: ToolCallId::new("c2"),
                    name: "read".to_string(),
                    raw_arguments: "{}".into(),
                },
            ],
            finish_reason: FinishReason::ToolCalls,
            usage: None,
        }),
    ];
    for id in ["c1", "c2"] {
        items.push(TranscriptItem::ToolResult(ToolResultMessage {
            call_id: ToolCallId::new(id),
            tool_name: "read".to_string(),
            outcome: ToolOutcome::Success,
            content: Arc::from("line\n"),
            meta: ToolResultMeta::default(),
        }));
    }
    items.extend(turn(9));

    let mut projector = ContextProjector::new();
    let projected = projector
        .project(&mut items, &budget(), 0, None, 1)
        .unwrap();
    let body = serialize_anthropic_request_body(
        "m",
        "system",
        &[ToolDefinition {
            name: "read".to_string(),
            description: "read".to_string(),
            parameters: serde_json::json!({"type": "object", "properties": {}}),
        }],
        &projected.messages,
        &projected.breakpoints,
        false,
        1024,
    );
    let value: serde_json::Value = serde_json::from_str(&body).unwrap();
    // Anthropic caps cache breakpoints at four: system, tools, prefix, tail.
    let marker_count = body.matches("cache_control").count();
    assert_eq!(
        marker_count, 4,
        "expected exactly four cache breakpoints (system, tools, prefix, tail), got {marker_count}"
    );
    // The tool results must have been merged into the assistant's turn, not left as their own
    // messages: an off-by-one here would put the marker on the wrong message.
    let roles: Vec<&str> = value["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["role"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(
        roles[0], "user",
        "the first message is the user turn, got {roles:?}"
    );
    assert_eq!(roles[1], "assistant");
    // Two tool results, one merged assistant message: the next message is a user turn again.
    assert_eq!(
        roles[2], "user",
        "tool results must merge into one turn, got {roles:?}"
    );
}

/// Cache writes are not cache hits, and neither is uncached input.
#[test]
fn cache_hit_measurement_separates_creation_from_read() {
    // A warm turn: 600 read, 300 written, 100 uncached.
    let warm = TokenUsage {
        input_tokens: 1000,
        cached_input_tokens: Some(600),
        cache_creation_input_tokens: Some(300),
        output_tokens: 50,
        reasoning_tokens: None,
    };
    assert_eq!(warm.uncached_input_tokens(), 100);
    assert_eq!(warm.cache_creation_tokens(), 300);
    assert!((warm.prefix_cache_hit_ratio() - 0.6).abs() < 1e-9);

    // A cold turn: everything is a write. The hit ratio must be zero, not one, and the write must not
    // be reported as uncached input.
    let cold = TokenUsage {
        input_tokens: 1000,
        cached_input_tokens: Some(0),
        cache_creation_input_tokens: Some(1000),
        output_tokens: 50,
        reasoning_tokens: None,
    };
    assert_eq!(cold.uncached_input_tokens(), 0);
    assert_eq!(cold.prefix_cache_hit_ratio(), 0.0);

    // A provider that reports neither split: everything is uncached, and nothing is invented.
    let plain = TokenUsage {
        input_tokens: 1000,
        cached_input_tokens: None,
        cache_creation_input_tokens: None,
        output_tokens: 50,
        reasoning_tokens: None,
    };
    assert_eq!(plain.uncached_input_tokens(), 1000);
    assert_eq!(plain.cache_creation_tokens(), 0);
    assert_eq!(plain.prefix_cache_hit_ratio(), 0.0);
}
