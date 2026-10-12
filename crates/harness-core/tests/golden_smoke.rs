use harness_core::agent_loop::{build_default_runtime, AgentRunConfig, AgentRunner};
use harness_core::budget::BudgetConfig;
use harness_core::context_projector::verify_tool_call_pairing;
use harness_core::env::LocalHostEnv;
use harness_core::events::EventLogger;
use harness_core::messages::{AssistantTurn, FinishReason, ToolCall, ToolOutcome, TranscriptItem};
use harness_core::provider::FakeProvider;
use harness_core::tool::policy::{NonInteractiveApprovalHandler, PolicyEngine, PolicyMode};
use harness_core::types::{ErrorCode, RunStatus, TerminalReason, ToolCallId};
use std::fs;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::watch;
use uuid::Uuid;

fn setup_sample_repo() -> (std::path::PathBuf, std::path::PathBuf) {
    let root = std::env::temp_dir().join(format!("harness-golden-{}", Uuid::new_v4()));
    let ws = root.join("repo");
    let state = root.join("state");
    fs::create_dir_all(ws.join("src")).unwrap();
    fs::create_dir_all(&state).unwrap();

    fs::write(
        ws.join("src/math.rs"),
        "pub fn add(a: i32, b: i32) -> i32 {\n    a - b\n}\n\npub fn mul(a: i32, b: i32) -> i32 {\n    a * b\n}\n",
    )
    .unwrap();
    fs::write(
        ws.join("src/lib.rs"),
        "pub mod math;\n",
    )
    .unwrap();
    fs::write(ws.join(".env"), "DB_PASSWORD=secret\n").unwrap();
    (ws, state)
}

async fn run_scenario(
    ws: &Path,
    state: &Path,
    policy_mode: PolicyMode,
    budget: BudgetConfig,
    turns: Vec<AssistantTurn>,
) -> harness_core::agent_loop::RunSummary {
    let provider = Arc::new(FakeProvider::new(turns));
    let env = Arc::new(LocalHostEnv::new(ws, state.to_path_buf()).unwrap());
    let policy = PolicyEngine::new(policy_mode, Arc::new(NonInteractiveApprovalHandler));
    let runtime = build_default_runtime(env, policy, budget.clone(), None);
    let cfg = AgentRunConfig::new("golden task", budget);
    let logger = EventLogger::new(cfg.run_id, Some(state.to_path_buf()), false).unwrap();
    let (_tx, rx) = watch::channel(false);
    let (summary, _logger) = AgentRunner::new(cfg, provider, runtime, rx).run(logger).await;
    assert!(verify_tool_call_pairing(&summary.transcript));
    summary
}

#[tokio::test]
async fn test_golden_smoke_16_end_to_end_scenarios() {
    // Scenario 1: Read-only repository navigation (ls -> find -> grep -> read -> stop)
    {
        let (ws, state) = setup_sample_repo();
        let summary = run_scenario(
            &ws,
            &state,
            PolicyMode::ReadOnly,
            BudgetConfig::default(),
            vec![
                AssistantTurn {
                    content: None,
                    reasoning_blocks: Vec::new(),
                    tool_calls: vec![
                        ToolCall {
                            call_id: ToolCallId::new("c1"),
                            name: "ls".into(),
                            raw_arguments: r#"{"path":"."}"#.into(),
                        },
                        ToolCall {
                            call_id: ToolCallId::new("c2"),
                            name: "find".into(),
                            raw_arguments: r#"{"pattern":"*.rs"}"#.into(),
                        },
                        ToolCall {
                            call_id: ToolCallId::new("c3"),
                            name: "grep".into(),
                            raw_arguments: r#"{"pattern":"pub fn add"}"#.into(),
                        },
                        ToolCall {
                            call_id: ToolCallId::new("c4"),
                            name: "read".into(),
                            raw_arguments: r#"{"path":"src/math.rs"}"#.into(),
                        },
                    ],
                    finish_reason: FinishReason::ToolCalls,
                    usage: None,
                },
                AssistantTurn {
                    content: Some("Found bug in add()".into()),
                    reasoning_blocks: Vec::new(),
                    tool_calls: Vec::new(),
                    finish_reason: FinishReason::Stop,
                    usage: None,
                },
            ],
        )
        .await;
        assert_eq!(summary.status, RunStatus::Completed);
        assert_eq!(summary.tool_calls_used, 4);
        let _ = fs::remove_dir_all(ws.parent().unwrap());
    }

    // Scenario 2: Edit indentation mismatch -> 1-turn self-healing recovery
    {
        let (ws, state) = setup_sample_repo();
        let summary = run_scenario(
            &ws,
            &state,
            PolicyMode::AllowWrite,
            BudgetConfig::default(),
            vec![
                // Turn 1: wrong 2-space indent
                AssistantTurn {
                    content: None,
                    reasoning_blocks: Vec::new(),
                    tool_calls: vec![ToolCall {
                        call_id: ToolCallId::new("e1"),
                        name: "edit".into(),
                        raw_arguments: r#"{"path":"src/math.rs","edits":[{"old_text":"pub fn add(a: i32, b: i32) -> i32 {\n  a - b\n}","new_text":"pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}"}]}"#.into(),
                    }],
                    finish_reason: FinishReason::ToolCalls,
                    usage: None,
                },
                // Turn 2: self-heals using 4-space indent from diagnostic
                AssistantTurn {
                    content: None,
                    reasoning_blocks: Vec::new(),
                    tool_calls: vec![ToolCall {
                        call_id: ToolCallId::new("e2"),
                        name: "edit".into(),
                        raw_arguments: r#"{"path":"src/math.rs","edits":[{"old_text":"pub fn add(a: i32, b: i32) -> i32 {\n    a - b\n}","new_text":"pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}"}]}"#.into(),
                    }],
                    finish_reason: FinishReason::ToolCalls,
                    usage: None,
                },
                AssistantTurn {
                    content: Some("Fixed add()".into()),
                    reasoning_blocks: Vec::new(),
                    tool_calls: Vec::new(),
                    finish_reason: FinishReason::Stop,
                    usage: None,
                },
            ],
        )
        .await;
        assert_eq!(summary.status, RunStatus::Completed);
        assert!(fs::read_to_string(ws.join("src/math.rs"))
            .unwrap()
            .contains("a + b"));
        let _ = fs::remove_dir_all(ws.parent().unwrap());
    }

    // Scenario 3: Multi-edit atomic replacement in single snapshot
    {
        let (ws, state) = setup_sample_repo();
        let summary = run_scenario(
            &ws,
            &state,
            PolicyMode::AllowWrite,
            BudgetConfig::default(),
            vec![
                AssistantTurn {
                    content: None,
                    reasoning_blocks: Vec::new(),
                    tool_calls: vec![ToolCall {
                        call_id: ToolCallId::new("e_multi"),
                        name: "edit".into(),
                        raw_arguments: r#"{"path":"src/math.rs","edits":[{"old_text":"a - b","new_text":"a + b"},{"old_text":"a * b","new_text":"a.saturating_mul(b)"}]}"#.into(),
                    }],
                    finish_reason: FinishReason::ToolCalls,
                    usage: None,
                },
                AssistantTurn {
                    content: Some("Updated both functions".into()),
                    reasoning_blocks: Vec::new(),
                    tool_calls: Vec::new(),
                    finish_reason: FinishReason::Stop,
                    usage: None,
                },
            ],
        )
        .await;
        assert_eq!(summary.status, RunStatus::Completed);
        let updated = fs::read_to_string(ws.join("src/math.rs")).unwrap();
        assert!(updated.contains("a + b") && updated.contains("a.saturating_mul(b)"));
        let _ = fs::remove_dir_all(ws.parent().unwrap());
    }

    // Scenario 4: Mid-batch tool budget exhaustion synthesizes NOT_EXECUTED_BUDGET_EXHAUSTED for remaining call_ids
    {
        let (ws, state) = setup_sample_repo();
        let mut budget = BudgetConfig::default();
        budget.max_tool_calls = 2;
        let summary = run_scenario(
            &ws,
            &state,
            PolicyMode::ReadOnly,
            budget,
            vec![AssistantTurn {
                content: None,
                reasoning_blocks: Vec::new(),
                tool_calls: vec![
                    ToolCall {
                        call_id: ToolCallId::new("b1"),
                        name: "ls".into(),
                        raw_arguments: "{}".into(),
                    },
                    ToolCall {
                        call_id: ToolCallId::new("b2"),
                        name: "find".into(),
                        raw_arguments: r#"{"pattern":"*.rs"}"#.into(),
                    },
                    ToolCall {
                        call_id: ToolCallId::new("b3_unexecuted"),
                        name: "read".into(),
                        raw_arguments: r#"{"path":"src/lib.rs"}"#.into(),
                    },
                ],
                finish_reason: FinishReason::ToolCalls,
                usage: None,
            }],
        )
        .await;
        assert_eq!(summary.status, RunStatus::BudgetExhausted);
        assert_eq!(summary.terminal_reason, TerminalReason::MaxToolCalls);
        let TranscriptItem::ToolResult(ref synth) = summary.transcript.last().unwrap() else {
            panic!("expected synthetic ToolResult");
        };
        assert_eq!(synth.call_id.as_str(), "b3_unexecuted");
        assert_eq!(
            synth.outcome,
            ToolOutcome::Error {
                code: ErrorCode::NotExecutedBudgetExhausted
            }
        );
        let _ = fs::remove_dir_all(ws.parent().unwrap());
    }

    // Scenario 5: Mid-batch cancellation synthesizes NOT_EXECUTED_CANCELLED for all remaining call_ids
    {
        let (ws, state) = setup_sample_repo();
        let budget = BudgetConfig::default();
        let turns = vec![AssistantTurn {
            content: None,
            reasoning_blocks: Vec::new(),
            tool_calls: vec![
                ToolCall {
                    call_id: ToolCallId::new("bash_slow"),
                    name: "bash".into(),
                    raw_arguments: r#"{"command":"sleep 5"}"#.into(),
                },
                ToolCall {
                    call_id: ToolCallId::new("after_cancel"),
                    name: "read".into(),
                    raw_arguments: r#"{"path":"src/lib.rs"}"#.into(),
                },
            ],
            finish_reason: FinishReason::ToolCalls,
            usage: None,
        }];
        let provider = Arc::new(FakeProvider::new(turns));
        let env = Arc::new(LocalHostEnv::new(&ws, state.clone()).unwrap());
        let policy = PolicyEngine::new(
            PolicyMode::FullAutonomous,
            Arc::new(NonInteractiveApprovalHandler),
        );
        let runtime = build_default_runtime(env, policy, budget.clone(), None)
            .with_bash_grace_period( std::time::Duration::from_millis(50) );
        let cfg = AgentRunConfig::new("cancel mid batch", budget);
        let logger = EventLogger::new(cfg.run_id, Some(state), false).unwrap();
        let (tx, rx) = watch::channel(false);
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            let _ = tx.send(true);
        });
        let (summary, _) = AgentRunner::new(cfg, provider, runtime, rx).run(logger).await;
        assert_eq!(summary.status, RunStatus::Cancelled);
        assert_eq!(summary.terminal_reason, TerminalReason::UserCancelled);
        assert!(verify_tool_call_pairing(&summary.transcript));
        let TranscriptItem::ToolResult(ref last_tr) = summary.transcript.last().unwrap() else {
            panic!("expected synthetic ToolResult");
        };
        assert_eq!(last_tr.call_id.as_str(), "after_cancel");
        assert_eq!(
            last_tr.outcome,
            ToolOutcome::Error {
                code: ErrorCode::NotExecutedCancelled
            }
        );
        let _ = fs::remove_dir_all(ws.parent().unwrap());
    }

    // Scenario 6: MaxTurns budget exhaustion terminates cleanly at turn boundary
    {
        let (ws, state) = setup_sample_repo();
        let mut budget = BudgetConfig::default();
        budget.max_model_turns = 1;
        let summary = run_scenario(
            &ws,
            &state,
            PolicyMode::FullAutonomous,
            budget,
            vec![
                AssistantTurn {
                    content: None,
                    reasoning_blocks: Vec::new(),
                    tool_calls: vec![ToolCall {
                        call_id: ToolCallId::new("c_t1"),
                        name: "ls".into(),
                        raw_arguments: "{}".into(),
                    }],
                    finish_reason: FinishReason::ToolCalls,
                    usage: None,
                },
                AssistantTurn {
                    content: Some("unreachable".into()),
                    reasoning_blocks: Vec::new(),
                    tool_calls: Vec::new(),
                    finish_reason: FinishReason::Stop,
                    usage: None,
                },
            ],
        )
        .await;
        assert_eq!(summary.status, RunStatus::BudgetExhausted);
        assert_eq!(summary.terminal_reason, TerminalReason::MaxTurns);
        assert_eq!(summary.turns_used, 1);
        let _ = fs::remove_dir_all(ws.parent().unwrap());
    }

    // Scenario 7: ReadOnly policy blocks `write` with POLICY_DENIED and zero disk mutation
    {
        let (ws, state) = setup_sample_repo();
        let summary = run_scenario(
            &ws,
            &state,
            PolicyMode::ReadOnly,
            BudgetConfig::default(),
            vec![
                AssistantTurn {
                    content: None,
                    reasoning_blocks: Vec::new(),
                    tool_calls: vec![ToolCall {
                        call_id: ToolCallId::new("c_ro_deny"),
                        name: "write".into(),
                        raw_arguments: r#"{"path":"a.txt","content":"x"}"#.into(),
                    }],
                    finish_reason: FinishReason::ToolCalls,
                    usage: None,
                },
                AssistantTurn {
                    content: Some("Understood read-only policy".into()),
                    reasoning_blocks: Vec::new(),
                    tool_calls: Vec::new(),
                    finish_reason: FinishReason::Stop,
                    usage: None,
                },
            ],
        )
        .await;
        assert_eq!(summary.status, RunStatus::Completed);
        assert!(!ws.join("a.txt").exists());
        let TranscriptItem::ToolResult(ref tr) = summary.transcript[2] else {
            panic!("expected ToolResult at transcript[2]");
        };
        assert_eq!(
            tr.outcome,
            ToolOutcome::Error {
                code: ErrorCode::PolicyDenied
            }
        );
        let _ = fs::remove_dir_all(ws.parent().unwrap());
    }

    // Scenario 8: Sensitive path `.env` (and symlink to `.env`) blocked with SENSITIVE_PATH_BLOCKED, model recovers via `.env.example`
    {
        let (ws, state) = setup_sample_repo();
        fs::write(ws.join(".env.example"), "DB_PASSWORD=placeholder\n").unwrap();
        std::os::unix::fs::symlink(ws.join(".env"), ws.join("env_link")).unwrap();
        let summary = run_scenario(
            &ws,
            &state,
            PolicyMode::FullAutonomous,
            BudgetConfig::default(),
            vec![
                AssistantTurn {
                    content: None,
                    reasoning_blocks: Vec::new(),
                    tool_calls: vec![
                        ToolCall {
                            call_id: ToolCallId::new("s8_env"),
                            name: "read".into(),
                            raw_arguments: r#"{"path":".env"}"#.into(),
                        },
                        ToolCall {
                            call_id: ToolCallId::new("s8_link"),
                            name: "read".into(),
                            raw_arguments: r#"{"path":"env_link"}"#.into(),
                        },
                    ],
                    finish_reason: FinishReason::ToolCalls,
                    usage: None,
                },
                AssistantTurn {
                    content: None,
                    reasoning_blocks: Vec::new(),
                    tool_calls: vec![ToolCall {
                        call_id: ToolCallId::new("s8_example"),
                        name: "read".into(),
                        raw_arguments: r#"{"path":".env.example"}"#.into(),
                    }],
                    finish_reason: FinishReason::ToolCalls,
                    usage: None,
                },
                AssistantTurn {
                    content: Some("Used .env.example template safely.".into()),
                    reasoning_blocks: Vec::new(),
                    tool_calls: Vec::new(),
                    finish_reason: FinishReason::Stop,
                    usage: None,
                },
            ],
        )
        .await;
        assert_eq!(summary.status, RunStatus::Completed);
        for idx in [2usize, 3usize] {
            let TranscriptItem::ToolResult(ref tr) = summary.transcript[idx] else {
                panic!("expected ToolResult");
            };
            assert_eq!(
                tr.outcome,
                ToolOutcome::Error {
                    code: ErrorCode::SensitivePathBlocked
                }
            );
        }
        let TranscriptItem::ToolResult(ref ok_tr) = summary.transcript[5] else {
            panic!("expected ToolResult");
        };
        assert_eq!(ok_tr.outcome, ToolOutcome::Success);
        assert!(ok_tr.content.contains("DB_PASSWORD=placeholder"));
        let _ = fs::remove_dir_all(ws.parent().unwrap());
    }

    // Scenario 9: Line-number prefix pollution (`   1\t...`) in `edit` triggers LineNumberPrefix diagnostic and self-heals
    {
        let (ws, state) = setup_sample_repo();
        let summary = run_scenario(
            &ws,
            &state,
            PolicyMode::AllowWrite,
            BudgetConfig::default(),
            vec![
                AssistantTurn {
                    content: None,
                    reasoning_blocks: Vec::new(),
                    tool_calls: vec![ToolCall {
                        call_id: ToolCallId::new("s9_bad"),
                        name: "edit".into(),
                        raw_arguments: r#"{"path":"src/math.rs","edits":[{"old_text":"    1\tpub fn add(a: i32, b: i32) -> i32 {\n    2\t    a - b","new_text":"pub fn add(a: i32, b: i32) -> i32 {\n    a + b"}]}"#.into(),
                    }],
                    finish_reason: FinishReason::ToolCalls,
                    usage: None,
                },
                AssistantTurn {
                    content: None,
                    reasoning_blocks: Vec::new(),
                    tool_calls: vec![ToolCall {
                        call_id: ToolCallId::new("s9_good"),
                        name: "edit".into(),
                        raw_arguments: r#"{"path":"src/math.rs","edits":[{"old_text":"pub fn add(a: i32, b: i32) -> i32 {\n    a - b","new_text":"pub fn add(a: i32, b: i32) -> i32 {\n    a + b"}]}"#.into(),
                    }],
                    finish_reason: FinishReason::ToolCalls,
                    usage: None,
                },
                AssistantTurn {
                    content: Some("Recovered from line number prefix.".into()),
                    reasoning_blocks: Vec::new(),
                    tool_calls: Vec::new(),
                    finish_reason: FinishReason::Stop,
                    usage: None,
                },
            ],
        )
        .await;
        assert_eq!(summary.status, RunStatus::Completed);
        let TranscriptItem::ToolResult(ref err_tr) = summary.transcript[2] else {
            panic!("expected ToolResult");
        };
        assert_eq!(
            err_tr.meta.diag_kind,
            Some(harness_core::types::EditDiagKind::LineNumberPrefix)
        );
        assert!(fs::read_to_string(ws.join("src/math.rs")).unwrap().contains("a + b"));
        let _ = fs::remove_dir_all(ws.parent().unwrap());
    }

    // Scenario 10: Closest-candidate diagnostic on fuzzy `old_text` mismatch guides 1-turn recovery
    {
        let (ws, state) = setup_sample_repo();
        let summary = run_scenario(
            &ws,
            &state,
            PolicyMode::AllowWrite,
            BudgetConfig::default(),
            vec![
                AssistantTurn {
                    content: None,
                    reasoning_blocks: Vec::new(),
                    tool_calls: vec![ToolCall {
                        call_id: ToolCallId::new("s10_fuzzy"),
                        name: "edit".into(),
                        raw_arguments: r#"{"path":"src/math.rs","edits":[{"old_text":"pub fn mul(a: i32, b: i32) -> i32 {\n    a / b\n}","new_text":"pub fn mul(a: i32, b: i32) -> i32 {\n    a.wrapping_mul(b)\n}"}]}"#.into(),
                    }],
                    finish_reason: FinishReason::ToolCalls,
                    usage: None,
                },
                AssistantTurn {
                    content: None,
                    reasoning_blocks: Vec::new(),
                    tool_calls: vec![ToolCall {
                        call_id: ToolCallId::new("s10_exact"),
                        name: "edit".into(),
                        raw_arguments: r#"{"path":"src/math.rs","edits":[{"old_text":"pub fn mul(a: i32, b: i32) -> i32 {\n    a * b\n}","new_text":"pub fn mul(a: i32, b: i32) -> i32 {\n    a.wrapping_mul(b)\n}"}]}"#.into(),
                    }],
                    finish_reason: FinishReason::ToolCalls,
                    usage: None,
                },
                AssistantTurn {
                    content: Some("Updated mul()".into()),
                    reasoning_blocks: Vec::new(),
                    tool_calls: Vec::new(),
                    finish_reason: FinishReason::Stop,
                    usage: None,
                },
            ],
        )
        .await;
        assert_eq!(summary.status, RunStatus::Completed);
        let TranscriptItem::ToolResult(ref err_tr) = summary.transcript[2] else {
            panic!("expected ToolResult");
        };
        assert_eq!(
            err_tr.meta.diag_kind,
            Some(harness_core::types::EditDiagKind::ClosestCandidate)
        );
        assert!(fs::read_to_string(ws.join("src/math.rs"))
            .unwrap()
            .contains("a.wrapping_mul(b)"));
        let _ = fs::remove_dir_all(ws.parent().unwrap());
    }

    // Scenario 11: LoopGuard blocks 3rd consecutive identical call (including `./` path alias) with REPEATED_CALL_BLOCKED
    {
        let (ws, state) = setup_sample_repo();
        let summary = run_scenario(
            &ws,
            &state,
            PolicyMode::ReadOnly,
            BudgetConfig::default(),
            vec![
                AssistantTurn {
                    content: None,
                    reasoning_blocks: Vec::new(),
                    tool_calls: vec![
                        ToolCall {
                            call_id: ToolCallId::new("r1"),
                            name: "read".into(),
                            raw_arguments: r#"{"path":"src/lib.rs"}"#.into(),
                        },
                        ToolCall {
                            call_id: ToolCallId::new("r2"),
                            name: "read".into(),
                            raw_arguments: r#"{"path":"./src/lib.rs"}"#.into(),
                        },
                        ToolCall {
                            call_id: ToolCallId::new("r3_blocked"),
                            name: "read".into(),
                            raw_arguments: r#"{"path":"src//lib.rs"}"#.into(),
                        },
                    ],
                    finish_reason: FinishReason::ToolCalls,
                    usage: None,
                },
                AssistantTurn {
                    content: Some("Stopped repeating".into()),
                    reasoning_blocks: Vec::new(),
                    tool_calls: Vec::new(),
                    finish_reason: FinishReason::Stop,
                    usage: None,
                },
            ],
        )
        .await;
        assert_eq!(summary.status, RunStatus::Completed);
        let TranscriptItem::ToolResult(ref r3_tr) = summary.transcript[4] else {
            panic!("expected 3rd ToolResult");
        };
        assert_eq!(
            r3_tr.outcome,
            ToolOutcome::Error {
                code: ErrorCode::RepeatedCallBlocked
            }
        );
        let _ = fs::remove_dir_all(ws.parent().unwrap());
    }

    // Scenario 12: Period-2 Ping-Pong allows 5th call (`read`) and blocks 6th call (`[A,B,A,B,A]+B`) with PING_PONG_CYCLE_BLOCKED
    {
        let (ws, state) = setup_sample_repo();
        let mut turns = Vec::new();
        for cycle in 1..=3 {
            turns.push(AssistantTurn {
                content: None,
                reasoning_blocks: Vec::new(),
                tool_calls: vec![
                    ToolCall {
                        call_id: ToolCallId::new(format!("pp_r_{}", cycle)),
                        name: "read".into(),
                        raw_arguments: r#"{"path":"src/lib.rs"}"#.into(),
                    },
                    ToolCall {
                        call_id: ToolCallId::new(format!("pp_g_{}", cycle)),
                        name: "grep".into(),
                        raw_arguments: r#"{"pattern":"pub mod"}"#.into(),
                    },
                ],
                finish_reason: FinishReason::ToolCalls,
                usage: None,
            });
        }
        turns.push(AssistantTurn {
            content: Some("Broke out of ping-pong".into()),
            reasoning_blocks: Vec::new(),
            tool_calls: Vec::new(),
            finish_reason: FinishReason::Stop,
            usage: None,
        });
        let summary = run_scenario(&ws, &state, PolicyMode::ReadOnly, BudgetConfig::default(), turns).await;
        assert_eq!(summary.status, RunStatus::Completed);
        // 5th call (pp_r_3) must be Success; 6th call (pp_g_3) must be PingPongCycleBlocked!
        let fifth = summary.transcript.iter().find_map(|i| match i {
            TranscriptItem::ToolResult(tr) if tr.call_id.as_str() == "pp_r_3" => Some(tr),
            _ => None,
        }).unwrap();
        assert_eq!(fifth.outcome, ToolOutcome::Success);

        let sixth = summary.transcript.iter().find_map(|i| match i {
            TranscriptItem::ToolResult(tr) if tr.call_id.as_str() == "pp_g_3" => Some(tr),
            _ => None,
        }).unwrap();
        assert_eq!(
            sixth.outcome,
            ToolOutcome::Error {
                code: ErrorCode::PingPongCycleBlocked
            }
        );
        let _ = fs::remove_dir_all(ws.parent().unwrap());
    }

    // Scenario 13: Per-file 3 failed edits trip EDIT_FAILURE_STREAK_BLOCKED (even with interleaved reads before 3), then `read` unlocks
    {
        let (ws, state) = setup_sample_repo();
        let summary = run_scenario(
            &ws,
            &state,
            PolicyMode::AllowWrite,
            BudgetConfig::default(),
            vec![
                AssistantTurn {
                    content: None,
                    reasoning_blocks: Vec::new(),
                    tool_calls: vec![
                        ToolCall {
                            call_id: ToolCallId::new("ef1"),
                            name: "edit".into(),
                            raw_arguments: r#"{"path":"src/math.rs","edits":[{"old_text":"nonexistent_1","new_text":"x"}]}"#.into(),
                        },
                        ToolCall {
                            call_id: ToolCallId::new("er1"),
                            name: "read".into(),
                            raw_arguments: r#"{"path":"src/math.rs"}"#.into(),
                        },
                        ToolCall {
                            call_id: ToolCallId::new("ef2"),
                            name: "edit".into(),
                            raw_arguments: r#"{"path":"./src/math.rs","edits":[{"old_text":"nonexistent_2","new_text":"x"}]}"#.into(),
                        },
                        ToolCall {
                            call_id: ToolCallId::new("ef3"),
                            name: "edit".into(),
                            raw_arguments: r#"{"path":"src/math.rs","edits":[{"old_text":"nonexistent_3","new_text":"x"}]}"#.into(),
                        },
                        // 4th edit without read after streak=3 -> blocked by EditFailureStreakBlocked!
                        ToolCall {
                            call_id: ToolCallId::new("ef4_blocked"),
                            name: "edit".into(),
                            raw_arguments: r#"{"path":"src/math.rs","edits":[{"old_text":"a - b","new_text":"a + b"}]}"#.into(),
                        },
                    ],
                    finish_reason: FinishReason::ToolCalls,
                    usage: None,
                },
                // Turn 2: `read` unlocks circuit breaker, then `edit` succeeds
                AssistantTurn {
                    content: None,
                    reasoning_blocks: Vec::new(),
                    tool_calls: vec![
                        ToolCall {
                            call_id: ToolCallId::new("er_unlock"),
                            name: "read".into(),
                            raw_arguments: r#"{"path":"src/math.rs"}"#.into(),
                        },
                        ToolCall {
                            call_id: ToolCallId::new("ef5_ok"),
                            name: "edit".into(),
                            raw_arguments: r#"{"path":"src/math.rs","edits":[{"old_text":"a - b","new_text":"a + b"}]}"#.into(),
                        },
                    ],
                    finish_reason: FinishReason::ToolCalls,
                    usage: None,
                },
                AssistantTurn {
                    content: Some("Unlocked and edited".into()),
                    reasoning_blocks: Vec::new(),
                    tool_calls: Vec::new(),
                    finish_reason: FinishReason::Stop,
                    usage: None,
                },
            ],
        )
        .await;
        assert_eq!(summary.status, RunStatus::Completed);
        let ef4 = summary.transcript.iter().find_map(|i| match i {
            TranscriptItem::ToolResult(tr) if tr.call_id.as_str() == "ef4_blocked" => Some(tr),
            _ => None,
        }).unwrap();
        assert_eq!(
            ef4.outcome,
            ToolOutcome::Error {
                code: ErrorCode::EditFailureStreakBlocked
            }
        );
        let ef5 = summary.transcript.iter().find_map(|i| match i {
            TranscriptItem::ToolResult(tr) if tr.call_id.as_str() == "ef5_ok" => Some(tr),
            _ => None,
        }).unwrap();
        assert_eq!(ef5.outcome, ToolOutcome::Success);
        let _ = fs::remove_dir_all(ws.parent().unwrap());
    }

    // Scenario 14: `write` no-clobber (`if_not_exists=true`) rejection + `write` new file + `bash` verification
    {
        let (ws, state) = setup_sample_repo();
        let summary = run_scenario(
            &ws,
            &state,
            PolicyMode::FullAutonomous,
            BudgetConfig::default(),
            vec![
                AssistantTurn {
                    content: None,
                    reasoning_blocks: Vec::new(),
                    tool_calls: vec![
                        ToolCall {
                            call_id: ToolCallId::new("w_clash"),
                            name: "write".into(),
                            raw_arguments: r#"{"path":"src/lib.rs","content":"clobber","if_not_exists":true}"#.into(),
                        },
                        ToolCall {
                            call_id: ToolCallId::new("w_new"),
                            name: "write".into(),
                            raw_arguments: r#"{"path":"src/extra.rs","content":"pub const MAGIC: i32 = 77;\n","if_not_exists":true}"#.into(),
                        },
                        ToolCall {
                            call_id: ToolCallId::new("b_verify"),
                            name: "bash".into(),
                            raw_arguments: r#"{"command":"grep MAGIC src/extra.rs"}"#.into(),
                        },
                    ],
                    finish_reason: FinishReason::ToolCalls,
                    usage: None,
                },
                AssistantTurn {
                    content: Some("Verified MAGIC".into()),
                    reasoning_blocks: Vec::new(),
                    tool_calls: Vec::new(),
                    finish_reason: FinishReason::Stop,
                    usage: None,
                },
            ],
        )
        .await;
        assert_eq!(summary.status, RunStatus::Completed);
        assert_eq!(fs::read_to_string(ws.join("src/lib.rs")).unwrap(), "pub mod math;\n");
        let w_clash = summary.transcript.iter().find_map(|i| match i {
            TranscriptItem::ToolResult(tr) if tr.call_id.as_str() == "w_clash" => Some(tr),
            _ => None,
        }).unwrap();
        assert_eq!(
            w_clash.outcome,
            ToolOutcome::Error {
                code: ErrorCode::FileAlreadyExists
            }
        );
        let b_verify = summary.transcript.iter().find_map(|i| match i {
            TranscriptItem::ToolResult(tr) if tr.call_id.as_str() == "b_verify" => Some(tr),
            _ => None,
        }).unwrap();
        assert_eq!(b_verify.outcome, ToolOutcome::Success);
        assert!(b_verify.content.contains("MAGIC"));
        let _ = fs::remove_dir_all(ws.parent().unwrap());
    }

    // Scenario 15: Path escape (`../escape.txt`) blocked with PATH_ESCAPE_BLOCKED, model recovers with valid path
    {
        let (ws, state) = setup_sample_repo();
        let summary = run_scenario(
            &ws,
            &state,
            PolicyMode::AllowWrite,
            BudgetConfig::default(),
            vec![
                AssistantTurn {
                    content: None,
                    reasoning_blocks: Vec::new(),
                    tool_calls: vec![
                        ToolCall {
                            call_id: ToolCallId::new("esc_w"),
                            name: "write".into(),
                            raw_arguments: r#"{"path":"../escape.txt","content":"bad"}"#.into(),
                        },
                        ToolCall {
                            call_id: ToolCallId::new("valid_w"),
                            name: "write".into(),
                            raw_arguments: r#"{"path":"src/ok.txt","content":"safe\n"}"#.into(),
                        },
                    ],
                    finish_reason: FinishReason::ToolCalls,
                    usage: None,
                },
                AssistantTurn {
                    content: Some("Wrote safe file".into()),
                    reasoning_blocks: Vec::new(),
                    tool_calls: Vec::new(),
                    finish_reason: FinishReason::Stop,
                    usage: None,
                },
            ],
        )
        .await;
        assert_eq!(summary.status, RunStatus::Completed);
        let esc_tr = summary.transcript.iter().find_map(|i| match i {
            TranscriptItem::ToolResult(tr) if tr.call_id.as_str() == "esc_w" => Some(tr),
            _ => None,
        }).unwrap();
        assert_eq!(
            esc_tr.outcome,
            ToolOutcome::Error {
                code: ErrorCode::PathEscapeBlocked
            }
        );
        assert!(ws.join("src/ok.txt").exists());
        let _ = fs::remove_dir_all(ws.parent().unwrap());
    }

    // Scenario 16: Interactive approval TOCTOU detects new file created during approval wait (`pre_approval_sha = None` -> `now_sha = Some`)
    // and generates real Unified Diff preview
    {
        use harness_core::tool::policy::{ApprovalDecision, ApprovalHandler, ApprovalRequest};

        struct RacingApprovalHandler {
            ws: std::path::PathBuf,
            captured_diff: std::sync::Mutex<Option<String>>,
        }
        impl ApprovalHandler for RacingApprovalHandler {
            fn is_interactive(&self) -> bool {
                true
            }
            fn request_approval(&self, req: &ApprovalRequest) -> ApprovalDecision {
                *self.captured_diff.lock().unwrap() = req.diff_preview.clone();
                // Simulate external process creating the target file while user is reviewing approval prompt!
                fs::write(self.ws.join("src/new_race.rs"), "external concurrent content\n").unwrap();
                ApprovalDecision::AllowOnce
            }
        }

        let (ws, state) = setup_sample_repo();
        let handler = Arc::new(RacingApprovalHandler {
            ws: ws.clone(),
            captured_diff: std::sync::Mutex::new(None),
        });
        let budget = BudgetConfig::default();
        let env = Arc::new(LocalHostEnv::new(&ws, state.clone()).unwrap());
        let policy = PolicyEngine::new(PolicyMode::Ask, handler.clone());
        let runtime = build_default_runtime(env, policy, budget.clone(), None);
        let provider = Arc::new(FakeProvider::new(vec![
            AssistantTurn {
                content: None,
                reasoning_blocks: Vec::new(),
                tool_calls: vec![ToolCall {
                    call_id: ToolCallId::new("race_write"),
                    name: "write".into(),
                    raw_arguments: r#"{"path":"src/new_race.rs","content":"pub fn mine() {}\n"}"#.into(),
                }],
                finish_reason: FinishReason::ToolCalls,
                usage: None,
            },
            AssistantTurn {
                content: Some("Aborted write due to TOCTOU".into()),
                reasoning_blocks: Vec::new(),
                tool_calls: Vec::new(),
                finish_reason: FinishReason::Stop,
                usage: None,
            },
        ]));
        let cfg = AgentRunConfig::new("race test", budget);
        let logger = EventLogger::new(cfg.run_id, Some(state), false).unwrap();
        let (_tx, rx) = watch::channel(false);
        let (summary, _) = AgentRunner::new(cfg, provider, runtime, rx).run(logger).await;

        assert_eq!(summary.status, RunStatus::Completed);
        let diff_str = handler.captured_diff.lock().unwrap().clone().unwrap();
        assert!(diff_str.contains("--- a/src/new_race.rs") && diff_str.contains("+pub fn mine() {}"));

        let race_tr = summary.transcript.iter().find_map(|i| match i {
            TranscriptItem::ToolResult(tr) if tr.call_id.as_str() == "race_write" => Some(tr),
            _ => None,
        }).unwrap();
        assert_eq!(
            race_tr.outcome,
            ToolOutcome::Error {
                code: ErrorCode::StaleFile
            }
        );
        // External content must NOT have been overwritten!
        assert_eq!(
            fs::read_to_string(ws.join("src/new_race.rs")).unwrap(),
            "external concurrent content\n"
        );
        let _ = fs::remove_dir_all(ws.parent().unwrap());
    }
}
