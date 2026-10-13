use kroma_core::agent_loop::{build_default_runtime, AgentRunConfig, AgentRunner};
use kroma_core::budget::BudgetConfig;
use kroma_core::context_projector::{verify_tool_call_pairing, ContextProjector};
use kroma_core::env::{CompletionGate, CompletionGateContext, GateOutcome, LocalHostEnv};
use kroma_core::events::EventLogger;
use kroma_core::loop_guard::{LoopGuard, LoopGuardVerdict};
use kroma_core::messages::{
    AssistantTurn, CacheBreakpoint, FinishReason, ReasoningBlock, TokenUsage, ToolCall,
    ToolOutcome, ToolResultMessage, ToolResultMeta, TranscriptItem, UserMessage,
};
use kroma_core::provider::FakeProvider;
use kroma_core::tool::policy::{NonInteractiveApprovalHandler, PolicyEngine, PolicyMode};
use kroma_core::types::{ErrorCode, RunStatus, ToolCallId};
use std::fs;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::{mpsc, watch};
use uuid::Uuid;

#[tokio::test]
async fn test_15_turns_append_only_prefix_byte_stability() {
    let tmp_ws = std::env::temp_dir().join(format!("kroma-ws-cache-{}", Uuid::new_v4()));
    let tmp_state = std::env::temp_dir().join(format!("kroma-state-cache-{}", Uuid::new_v4()));
    fs::create_dir_all(&tmp_ws).unwrap();
    for i in 0..15 {
        fs::write(
            tmp_ws.join(format!("mod_{}.rs", i)),
            format!("pub fn func_{}() -> usize {{ {} }}\n", i, i),
        )
        .unwrap();
    }

    let mut turns = Vec::new();
    for i in 0..14 {
        turns.push(AssistantTurn {
            content: Some(format!("Turn {}", i)),
            reasoning_blocks: vec![ReasoningBlock {
                text: Some(format!("Thinking step {}", i)),
                signature: Some(format!("sig_{}", i)),
                opaque_payload: None,
            }],
            tool_calls: vec![ToolCall {
                call_id: ToolCallId::new(format!("call_{}", i)),
                name: "read".into(),
                raw_arguments: format!("{{\"path\":\"mod_{}.rs\"}}", i),
            }],
            finish_reason: FinishReason::ToolCalls,
            usage: Some(TokenUsage {
                input_tokens: 500 + (i as u64) * 100,
                cached_input_tokens: Some(400 + (i as u64) * 100),
                output_tokens: 40,
                reasoning_tokens: Some(15),
            }),
        });
    }
    turns.push(AssistantTurn {
        content: Some("All 15 turns done.".into()),
        reasoning_blocks: Vec::new(),
        tool_calls: Vec::new(),
        finish_reason: FinishReason::Stop,
        usage: Some(TokenUsage {
            input_tokens: 2000,
            cached_input_tokens: Some(1900),
            output_tokens: 20,
            reasoning_tokens: None,
        }),
    });

    let fake_provider = Arc::new(FakeProvider::new(turns));
    let budget = BudgetConfig::default();
    let env = Arc::new(LocalHostEnv::new(&tmp_ws, tmp_state.clone()).unwrap());
    let policy = PolicyEngine::new(
        PolicyMode::ReadOnly,
        Arc::new(NonInteractiveApprovalHandler),
    );
    let runtime = build_default_runtime(env, policy, budget.clone(), None);
    let cfg = AgentRunConfig::new("Inspect modules", budget);
    let logger = EventLogger::new(cfg.run_id, Some(tmp_state.clone()), false).unwrap();
    let (_tx, rx) = watch::channel(false);

    let (summary, _logger) = AgentRunner::new(cfg, fake_provider.clone(), runtime, rx)
        .run(logger)
        .await;
    assert_eq!(summary.status, RunStatus::Completed);
    assert_eq!(summary.turns_used, 15);

    let requests = fake_provider.recorded_requests();
    let bodies = fake_provider.recorded_serialized_bodies();
    assert_eq!(requests.len(), 15);
    assert_eq!(bodies.len(), 15);

    // Verify across all 15 consecutive turns:
    // 1. System prompt and Tool Schemas are 100% byte-identical
    // 2. The first N messages of Turn t are 100% byte-identical to the first N messages of Turn t+1
    for t in 0..14 {
        assert_eq!(requests[t].system_prompt, requests[t + 1].system_prompt);
        let tools_t = serde_json::to_string(&requests[t].tools).unwrap();
        let tools_next = serde_json::to_string(&requests[t + 1].tools).unwrap();
        assert_eq!(tools_t, tools_next);

        let n = requests[t].messages.len();
        assert!(requests[t + 1].messages.len() > n);
        for m_idx in 0..n {
            let m_t = serde_json::to_string(&requests[t].messages[m_idx]).unwrap();
            let m_next = serde_json::to_string(&requests[t + 1].messages[m_idx]).unwrap();
            assert_eq!(
                m_t, m_next,
                "Prefix message {} mutated between turn {} and {}",
                m_idx, t, t + 1
            );
        }
    }

    let _ = fs::remove_dir_all(&tmp_ws);
    let _ = fs::remove_dir_all(&tmp_state);
}

#[test]
fn test_watermark_step_down_compaction_working_set_pinning_and_outline_stub() {
    let mut budget = BudgetConfig::default();
    budget.context_high_watermark_bytes = 24 * 1024; // 24 KiB high watermark for test
    budget.context_low_watermark_bytes = 12 * 1024;  // 12 KiB low watermark
    budget.context_tail_window_bytes = 2 * 1024;     // 2 KiB tail window
    budget.context_min_tail_turns = 1;

    let mut projector = ContextProjector::new();
    let mut canonical: Vec<TranscriptItem> = vec![TranscriptItem::User(UserMessage {
        content: "Refactor parser and lexer".into(),
    })];

    // Turn 1: Read cold file `src/cold.rs` (8 KiB with top-level symbols)
    let mut cold_src = String::from("[File: src/cold.rs | Lines: 1-200 of 200 | SHA256: a1b2c3d4e5f60718]\n");
    cold_src.push_str("    1\tpub struct ColdParser {\n   12\tpub fn parse_cold_expr() {\n   45\tenum ColdToken {\n");
    cold_src.push_str(&"   50\t// padding line for cold file\n".repeat(260));
    canonical.push(TranscriptItem::Assistant(AssistantTurn {
        content: None,
        reasoning_blocks: Vec::new(),
        tool_calls: vec![ToolCall {
            call_id: ToolCallId::new("call_cold_read"),
            name: "read".into(),
            raw_arguments: "{\"path\":\"src/cold.rs\"}".into(),
        }],
        finish_reason: FinishReason::ToolCalls,
        usage: None,
    }));
    canonical.push(TranscriptItem::ToolResult(ToolResultMessage {
        call_id: ToolCallId::new("call_cold_read"),
        tool_name: "read".into(),
        outcome: ToolOutcome::Success,
        content: Arc::from(cold_src),
        meta: ToolResultMeta {
            relative_path: Some("src/cold.rs".into()),
            content_sha256: Some("a1b2c3d4e5f60718".into()),
            lines_range: Some((1, 200)),
            ..Default::default()
        },
    }));

    // Turn 2: Read & Edit hot file `src/hot.rs` (8 KiB) -> Working-Set Pinning must exempt its latest read!
    let mut hot_src = String::from("[File: src/hot.rs | Lines: 1-200 of 200 | SHA256: 1122334455667788]\n");
    hot_src.push_str("    1\tpub fn hot_active_function() {}\n");
    hot_src.push_str(&"   10\t// hot file active content\n".repeat(260));
    canonical.push(TranscriptItem::Assistant(AssistantTurn {
        content: None,
        reasoning_blocks: Vec::new(),
        tool_calls: vec![
            ToolCall {
                call_id: ToolCallId::new("call_hot_read"),
                name: "read".into(),
                raw_arguments: "{\"path\":\"src/hot.rs\"}".into(),
            },
            ToolCall {
                call_id: ToolCallId::new("call_hot_edit"),
                name: "edit".into(),
                raw_arguments: "{\"path\":\"src/hot.rs\"}".into(),
            },
        ],
        finish_reason: FinishReason::ToolCalls,
        usage: None,
    }));
    canonical.push(TranscriptItem::ToolResult(ToolResultMessage {
        call_id: ToolCallId::new("call_hot_read"),
        tool_name: "read".into(),
        outcome: ToolOutcome::Success,
        content: Arc::from(hot_src.clone()),
        meta: ToolResultMeta {
            relative_path: Some("src/hot.rs".into()),
            content_sha256: Some("1122334455667788".into()),
            lines_range: Some((1, 200)),
            ..Default::default()
        },
    }));
    canonical.push(TranscriptItem::ToolResult(ToolResultMessage {
        call_id: ToolCallId::new("call_hot_edit"),
        tool_name: "edit".into(),
        outcome: ToolOutcome::Success,
        content: Arc::from("[Edited: src/hot.rs]"),
        meta: ToolResultMeta {
            relative_path: Some("src/hot.rs".into()),
            content_sha256: Some("9988776655443322".into()),
            modified_file: true,
            ..Default::default()
        },
    }));

    // Turn 3: Read another cold file `src/ cold2.rs` (10 KiB) + Turn 4 tail
    let mut cold2_src = String::from("[File: src/cold2.rs | Lines: 1-250 of 250 | SHA256: deadbeefcafebabe]\n");
    cold2_src.push_str("    5\tpub struct LexerState {\n");
    cold2_src.push_str(&"   20\t// padding line for cold2\n".repeat(320));
    canonical.push(TranscriptItem::Assistant(AssistantTurn {
        content: None,
        reasoning_blocks: Vec::new(),
        tool_calls: vec![ToolCall {
            call_id: ToolCallId::new("call_cold2_read"),
            name: "read".into(),
            raw_arguments: "{\"path\":\"src/cold2.rs\"}".into(),
        }],
        finish_reason: FinishReason::ToolCalls,
        usage: None,
    }));
    canonical.push(TranscriptItem::ToolResult(ToolResultMessage {
        call_id: ToolCallId::new("call_cold2_read"),
        tool_name: "read".into(),
        outcome: ToolOutcome::Success,
        content: Arc::from(cold2_src),
        meta: ToolResultMeta {
            relative_path: Some("src/cold2.rs".into()),
            content_sha256: Some("deadbeefcafebabe".into()),
            lines_range: Some((1, 250)),
            ..Default::default()
        },
    }));

    // Turn 4 (protected tail turn)
    canonical.push(TranscriptItem::Assistant(AssistantTurn {
        content: None,
        reasoning_blocks: Vec::new(),
        tool_calls: vec![ToolCall {
            call_id: ToolCallId::new("call_tail"),
            name: "ls".into(),
            raw_arguments: "{}".into(),
        }],
        finish_reason: FinishReason::ToolCalls,
        usage: None,
    }));
    canonical.push(TranscriptItem::ToolResult(ToolResultMessage {
        call_id: ToolCallId::new("call_tail"),
        tool_name: "ls".into(),
        outcome: ToolOutcome::Success,
        content: Arc::from("src/\nCargo.toml\n"),
        meta: ToolResultMeta::default(),
    }));

    let proj = projector
        .project(&mut canonical, &budget, 1000, None, 4)
        .expect("compaction should succeed");

    assert!(proj.compaction_event.is_some());
    assert!(verify_tool_call_pairing(&proj.messages));

    // Verify 4 CacheBreakpoints are annotated
    assert_eq!(proj.breakpoints.len(), 4);
    assert_eq!(proj.breakpoints[0].breakpoint, CacheBreakpoint::ToolsEnd);
    assert_eq!(proj.breakpoints[1].breakpoint, CacheBreakpoint::SystemEnd);
    assert_eq!(
        proj.breakpoints[2].breakpoint,
        CacheBreakpoint::PinnedBaselineEnd
    );
    assert_eq!(
        proj.breakpoints[3].breakpoint,
        CacheBreakpoint::TailWindowStart
    );

    // Verify cold file was compacted into an Outline Stub with 16-char sha256 and top-level symbol line numbers
    let TranscriptItem::ToolResult(ref cold_tr) = proj.messages[2] else {
        panic!("expected ToolResult at index 2");
    };
    assert!(cold_tr.content.starts_with("[Compacted: read(src/cold.rs"));
    assert!(cold_tr.content.contains("sha256=a1b2c3d4e5f60718"));
    assert!(cold_tr.content.contains("L1 struct ColdParser"));
    assert!(cold_tr.content.contains("L12 fn parse_cold_expr"));
    assert!(cold_tr.content.contains("L45 enum ColdToken"));

    // Verify Working-Set Pinning: `src/hot.rs` (modified in Turn 2, within last 8 turns) was NOT compacted!
    let TranscriptItem::ToolResult(ref hot_tr) = proj.messages[4] else {
        panic!("expected ToolResult at index 4");
    };
    assert_eq!(hot_tr.content.as_ref(), hot_src.as_str());

    // Verify next turn below watermark is 100% Append-Only (no further compaction)
    canonical.push(TranscriptItem::User(UserMessage {
        content: "Next small question".into(),
    }));
    let proj2 = projector
        .project(&mut canonical, &budget, 1000, None, 5)
        .unwrap();
    assert!(proj2.compaction_event.is_none());
    for i in 0..proj.messages.len() {
        assert_eq!(proj.messages[i], proj2.messages[i]);
    }

    // Verify compaction_reread_count increments if a compacted file is re-read, and does NOT double-count on repeat reads
    assert_eq!(projector.compaction_reread_count(), 0);
    projector.notify_file_read("src/hot.rs");
    assert_eq!(projector.compaction_reread_count(), 0);
    projector.notify_file_read("src/cold.rs");
    assert_eq!(projector.compaction_reread_count(), 1);
    projector.notify_file_read("src/cold.rs");
    assert_eq!(projector.compaction_reread_count(), 1);

    // Verify verify_tool_call_pairing rejects orphan ToolResultMessage after UserMessage
    let mut broken = proj.messages.clone();
    broken.push(TranscriptItem::User(UserMessage {
        content: "hi".into(),
    }));
    broken.push(TranscriptItem::ToolResult(ToolResultMessage {
        call_id: ToolCallId::new("orphan_id"),
        tool_name: "read".into(),
        outcome: ToolOutcome::Success,
        content: Arc::from("orphan"),
        meta: ToolResultMeta::default(),
    }));
    assert!(!verify_tool_call_pairing(&broken));

    // The still-pinned `src/hot.rs` read (index 4) must stay OUTSIDE the frozen baseline: freezing it
    // into `PinnedBaselineEnd` would turn its 8-turn Working-Set Pinning exemption into a permanent,
    // never-compactable prefix token (see docs/CODE_REVIEW_FIX_LIST.md §2).
    let baseline_len_after_comp1 = proj.breakpoints[2].message_index;
    assert_eq!(baseline_len_after_comp1, 4);
    for t in 0..10 {
        let mut big = String::from("[File: src/extra.rs | Lines: 1-100 of 100 | SHA256: 0000111122223333]\n");
        big.push_str(&format!("   10\t// turn {} extra data\n", t).repeat(120));
        canonical.push(TranscriptItem::Assistant(AssistantTurn {
            content: None,
            reasoning_blocks: Vec::new(),
            tool_calls: vec![ToolCall {
                call_id: ToolCallId::new(format!("extra_{}", t)),
                name: "read".into(),
                raw_arguments: "{\"path\":\"src/extra.rs\"}".into(),
            }],
            finish_reason: FinishReason::ToolCalls,
            usage: None,
        }));
        canonical.push(TranscriptItem::ToolResult(ToolResultMessage {
            call_id: ToolCallId::new(format!("extra_{}", t)),
            tool_name: "read".into(),
            outcome: ToolOutcome::Success,
            content: Arc::from(big),
            meta: ToolResultMeta {
                relative_path: Some("src/extra.rs".into()),
                content_sha256: Some("0000111122223333".into()),
                lines_range: Some((1, 100)),
                ..Default::default()
            },
        }));
    }
    let proj3 = projector
        .project(&mut canonical, &budget, 1000, None, 15)
        .expect("second compaction should succeed");
    assert!(proj3.compaction_event.is_some());
    // The frozen baseline prefix below `PinnedBaselineEnd` must never be re-mutated by a later pass.
    for i in 0..baseline_len_after_comp1 {
        assert_eq!(
            proj.messages[i], proj3.messages[i],
            "Pinned baseline item {} was mutated during second compaction!",
            i
        );
    }
    // Once the 8-turn Working-Set Pinning window has expired (turn 15 vs. last edit in turn 2), the
    // previously exempted hot read is reclaimed by the second compaction instead of lingering forever.
    let TranscriptItem::ToolResult(ref hot_after_comp2) = proj3.messages[4] else {
        panic!("expected ToolResult at index 4");
    };
    assert!(
        hot_after_comp2.content.starts_with("[Compacted: read(src/hot.rs"),
        "src/hot.rs read must be reclaimed once its pin window expires, got: {}",
        &hot_after_comp2.content[..hot_after_comp2.content.len().min(80)]
    );
}

#[test]
fn test_loop_guard_repeated_ping_pong_and_edit_streak() {
    let mut guard = LoopGuard::new();

    // 1. Consecutive 3 identical calls (canonical JSON key-order insensitive AND `./` path normalized)
    assert!(matches!(
        guard.check_and_record("read", r#"{"path":"./src/a.rs","offset":1}"#, Some("./src/a.rs")),
        LoopGuardVerdict::Allow { .. }
    ));
    assert!(matches!(
        guard.check_and_record("read", r#"{"offset": 1, "path": "src/a.rs"}"#, Some("src/a.rs")),
        LoopGuardVerdict::Allow { .. }
    ));
    let third = guard.check_and_record("read", r#"{"path":"src//./a.rs","offset":1}"#, Some("src/a.rs"));
    assert!(matches!(
        third,
        LoopGuardVerdict::Block {
            code: ErrorCode::RepeatedCallBlocked,
            ..
        }
    ));

    // 2. Period-2 Ping-Pong oscillation (A, B) -> (A, B) -> (A, B)
    // 5th call (A) is allowed so normal read-edit-read-edit-read recovery works;
    // 6th call (B completing 3rd full cycle [A,B,A,B,A]+B) is blocked!
    let mut pp_guard = LoopGuard::new();
    let call_a = ("read", r#"{"path":"x.rs"}"#);
    let call_b = ("edit", r#"{"path":"x.rs","edits":[{"old_text":"a","new_text":"b"}]}"#);
    // Cycle 1
    assert!(matches!(
        pp_guard.check_and_record(call_a.0, call_a.1, Some("x.rs")),
        LoopGuardVerdict::Allow { .. }
    ));
    assert!(matches!(
        pp_guard.check_and_record(call_b.0, call_b.1, Some("x.rs")),
        LoopGuardVerdict::Allow { .. }
    ));
    // Cycle 2
    assert!(matches!(
        pp_guard.check_and_record(call_a.0, call_a.1, Some("x.rs")),
        LoopGuardVerdict::Allow { .. }
    ));
    assert!(matches!(
        pp_guard.check_and_record(call_b.0, call_b.1, Some("x.rs")),
        LoopGuardVerdict::Allow { .. }
    ));
    // Cycle 3: 5th call (call_a = read) is ALLOWED!
    assert!(matches!(
        pp_guard.check_and_record(call_a.0, call_a.1, Some("x.rs")),
        LoopGuardVerdict::Allow { .. }
    ));
    // 6th call (call_b = edit completing 3rd cycle) is BLOCKED!
    let cycle3_sixth = pp_guard.check_and_record(call_b.0, call_b.1, Some("x.rs"));
    assert!(matches!(
        cycle3_sixth,
        LoopGuardVerdict::Block {
            code: ErrorCode::PingPongCycleBlocked,
            ..
        }
    ));

    // 3. Per-file consecutive 3 edit failures circuit breaker (interleaved `read` before 3 does not reset streak)
    let mut streak_guard = LoopGuard::new();
    assert_eq!(streak_guard.record_file_tool_outcome("edit", "./foo.rs", false), 1);
    assert_eq!(streak_guard.record_file_tool_outcome("read", "foo.rs", true), 1);
    assert_eq!(streak_guard.record_file_tool_outcome("edit", "foo.rs", false), 2);
    assert_eq!(streak_guard.record_file_tool_outcome("read", "./foo.rs", true), 2);
    assert_eq!(streak_guard.record_file_tool_outcome("edit", "foo.rs", false), 3);

    // 4th edit without an intervening `read` after reaching 3 is blocked by EditFailureStreakBlocked
    let blocked_edit = streak_guard.check_and_record(
        "edit",
        r#"{"path":"./foo.rs","edits":[{"old_text":"different","new_text":"text"}]}"#,
        Some("./foo.rs"),
    );
    assert!(matches!(
        blocked_edit,
        LoopGuardVerdict::Block {
            code: ErrorCode::EditFailureStreakBlocked,
            ..
        }
    ));

    // Calling `read` on `foo.rs` AFTER streak >= 3 unlocks the circuit breaker
    assert_eq!(streak_guard.record_file_tool_outcome("read", "foo.rs", true), 0);
    assert!(matches!(
        streak_guard.check_and_record(
            "edit",
            r#"{"path":"foo.rs","edits":[{"old_text":"different2","new_text":"text"}]}"#,
            Some("foo.rs"),
        ),
        LoopGuardVerdict::Allow { .. }
    ));
}

struct RejectOnceGate {
    calls: AtomicUsize,
}

impl CompletionGate for RejectOnceGate {
    fn check<'a>(
        &'a self,
        _ctx: &'a CompletionGateContext,
    ) -> Pin<Box<dyn Future<Output = Result<GateOutcome, String>> + Send + 'a>> {
        Box::pin(async move {
            let c = self.calls.fetch_add(1, Ordering::SeqCst);
            if c == 0 {
                Ok(GateOutcome::Rejected {
                    feedback: "cargo check failed: missing semicolon".into(),
                })
            } else {
                Ok(GateOutcome::Approved)
            }
        })
    }
}

#[tokio::test]
async fn test_readonly_parallel_batch_ordering_steering_and_completion_gate() {
    let tmp_ws = std::env::temp_dir().join(format!("kroma-ws-ro-{}", Uuid::new_v4()));
    let tmp_state = std::env::temp_dir().join(format!("kroma-state-ro-{}", Uuid::new_v4()));
    fs::create_dir_all(&tmp_ws).unwrap();

    for i in 0..10 {
        fs::write(tmp_ws.join(format!("f_{}.txt", i)), format!("file_content_{}\n", i)).unwrap();
    }

    // Turn 1: 10 concurrent read-only calls in a single batch + 1 write call
    let mut batch_calls: Vec<ToolCall> = (0..10)
        .map(|i| ToolCall {
            call_id: ToolCallId::new(format!("ro_call_{}", i)),
            name: "read".into(),
            raw_arguments: format!("{{\"path\":\"f_{}.txt\"}}", i),
        })
        .collect();
    batch_calls.push(ToolCall {
        call_id: ToolCallId::new("mut_write_1"),
        name: "write".into(),
        raw_arguments: "{\"path\":\"out.txt\",\"content\":\"written\"}".into(),
    });

    let turns = vec![
        AssistantTurn {
            content: None,
            reasoning_blocks: Vec::new(),
            tool_calls: batch_calls,
            finish_reason: FinishReason::ToolCalls,
            usage: None,
        },
        // Turn 2: Model attempts to stop -> CompletionGate rejects once
        AssistantTurn {
            content: Some("Trying to stop".into()),
            reasoning_blocks: Vec::new(),
            tool_calls: Vec::new(),
            finish_reason: FinishReason::Stop,
            usage: None,
        },
        // Turn 3: Model stops after gate feedback -> CompletionGate approves
        AssistantTurn {
            content: Some("Fixed and verified.".into()),
            reasoning_blocks: Vec::new(),
            tool_calls: Vec::new(),
            finish_reason: FinishReason::Stop,
            usage: None,
        },
    ];

    let provider = Arc::new(FakeProvider::new(turns));
    let budget = BudgetConfig::default();
    let env = Arc::new(LocalHostEnv::new(&tmp_ws, tmp_state.clone()).unwrap());
    let policy = PolicyEngine::new(
        PolicyMode::AllowWrite,
        Arc::new(NonInteractiveApprovalHandler),
    );
    let runtime = build_default_runtime(env, policy, budget.clone(), None);
    let cfg = AgentRunConfig::new("Read 10 files in parallel and write out.txt", budget);
    let logger = EventLogger::new(cfg.run_id, Some(tmp_state.clone()), false).unwrap();
    let (_tx, rx) = watch::channel(false);

    let (steer_tx, steer_rx) = mpsc::unbounded_channel();
    steer_tx
        .send("Mid-run steering instruction: double check formatting".into())
        .unwrap();

    let gate = Arc::new(RejectOnceGate {
        calls: AtomicUsize::new(0),
    });

    let (summary, _logger) = AgentRunner::new(cfg, provider, runtime, rx)
        .with_completion_gate(gate)
        .with_steer_receiver(steer_rx)
        .run(logger)
        .await;

    assert_eq!(summary.status, RunStatus::Completed);
    assert_eq!(summary.turns_used, 3);
    assert_eq!(summary.modified_files, vec!["out.txt".to_string()]);
    assert!(verify_tool_call_pairing(&summary.transcript));

    // Verify all 10 parallel read-only results were committed in exact index order 0..10
    for i in 0..10 {
        let TranscriptItem::ToolResult(ref tr) = summary.transcript[2 + i] else {
            panic!("expected ToolResult at transcript[{}]", 2 + i);
        };
        assert_eq!(tr.call_id.as_str(), format!("ro_call_{}", i));
        assert!(tr.content.contains(&format!("file_content_{}", i)));
    }
    let TranscriptItem::ToolResult(ref write_tr) = summary.transcript[12] else {
        panic!("expected write ToolResult at transcript[12]");
    };
    assert_eq!(write_tr.call_id.as_str(), "mut_write_1");

    // Verify mid-run steering message was injected right after the tool batch closed
    let TranscriptItem::User(ref steer_msg) = summary.transcript[13] else {
        panic!("expected steered UserMessage at transcript[13]");
    };
    assert!(steer_msg.content.contains("Mid-run steering instruction"));

    let _ = fs::remove_dir_all(&tmp_ws);
    let _ = fs::remove_dir_all(&tmp_state);
}
