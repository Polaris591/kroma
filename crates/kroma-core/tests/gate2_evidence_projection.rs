//! Gate 2 acceptance tests: evidence-linked context projection and raw evidence retrieval.
//!
//! Gate 2 makes every compacted Outline Stub traceable. A stub carries a deterministic
//! `source_entry_id`, the raw tool output it replaced stays retrievable through `recall_evidence`,
//! and the byte accounting per projection category is reproducible across fresh runs, `--resume`
//! and `--fork`. Test 5 also pins down the Gate 1 polish items O1–O6.

use kroma_core::agent_loop::{build_default_runtime, AgentRunConfig, AgentRunner};
use kroma_core::budget::BudgetConfig;
use kroma_core::context_projector::{
    validate_tool_call_pairing, verify_tool_call_pairing, CompactionReason, ContextProjector,
    EvidenceLink, ProjectionProfile, MAX_EVIDENCE_BYTES, MIN_COMPACTABLE_TOOL_BYTES,
};
use kroma_core::env::LocalHostEnv;
use kroma_core::events::EventLogger;
use kroma_core::messages::{
    AssistantTurn, FinishReason, ToolCall, ToolOutcome, ToolResultMessage, ToolResultMeta,
    TranscriptItem, UserMessage,
};
use kroma_core::prompt::sha256_hex64;
use kroma_core::provider::FakeProvider;
use kroma_core::revision::SnapshotStore;
use kroma_core::session::SessionJournal;
use kroma_core::tool::policy::{NonInteractiveApprovalHandler, PolicyEngine, PolicyMode};
use kroma_core::tools::recall_evidence::execute_recall_evidence;
use kroma_core::types::{ErrorCode, RunStatus, ToolCallId};
use kroma_core::workspace::WorkspaceCap;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::watch;
use uuid::Uuid;

fn temp_root(tag: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("kroma-gate2-{}-{}", tag, Uuid::new_v4()));
    fs::create_dir_all(&path).unwrap();
    path
}

/// The hidden constant lives inside a function body, so it is stripped from the Outline Stub and
/// only recoverable through `recall_evidence`.
const HIDDEN_CONSTANT: &str = "4242";
/// A second hidden constant used to prove that a re-compaction replaces stale raw evidence.
const HIDDEN_CONSTANT_V2: &str = "5151";

fn big_rust_source_with(secret: &str) -> String {
    let mut body =
        String::from("[File: src/big.rs | Lines: 1-400 of 400 | SHA256: 0123456789abcdef]\n");
    body.push_str("     1\tpub fn secret_answer() -> u32 {\n");
    for line in 2..=380 {
        body.push_str(&format!("{:>6}\t    // padding line {}\n", line, line));
    }
    body.push_str(&format!("   381\t    {}\n", secret));
    body.push_str("   382\t}\n");
    body
}

fn big_rust_source() -> String {
    big_rust_source_with(HIDDEN_CONSTANT)
}

fn big_grep_output() -> String {
    let mut body = String::new();
    for line in 1..=200 {
        body.push_str(&format!(
            "src/mod_{}.rs:{}: match line {}\n",
            line, line, line
        ));
    }
    body
}

fn big_bash_output() -> String {
    let mut body = String::from("[Command exit_code=0 | duration_ms=42]\n");
    for line in 1..=200 {
        body.push_str(&format!("test result line {}\n", line));
    }
    body
}

fn tool_result(call_id: &str, tool: &str, path: Option<&str>, body: String) -> TranscriptItem {
    TranscriptItem::ToolResult(ToolResultMessage {
        call_id: ToolCallId::new(call_id),
        tool_name: tool.into(),
        outcome: ToolOutcome::Success,
        content: Arc::from(body),
        meta: ToolResultMeta {
            relative_path: path.map(|p| p.to_string()),
            ..Default::default()
        },
    })
}

fn assistant_with(call_ids: &[(&str, &str)]) -> TranscriptItem {
    TranscriptItem::Assistant(AssistantTurn {
        content: None,
        reasoning_blocks: Vec::new(),
        tool_calls: call_ids
            .iter()
            .map(|(id, name)| ToolCall {
                call_id: ToolCallId::new(*id),
                name: (*name).to_string(),
                raw_arguments: "{}".into(),
            })
            .collect(),
        finish_reason: FinishReason::ToolCalls,
        usage: None,
    })
}

/// A transcript whose first turn reads/greps/runs three large outputs, followed by filler turns so
/// those outputs fall outside the protected tail window and must be compacted.
fn compaction_transcript() -> Vec<TranscriptItem> {
    let mut items = vec![TranscriptItem::User(UserMessage {
        content: "find the constant".into(),
    })];
    items.push(assistant_with(&[
        ("call_big", "read"),
        ("call_grep", "grep"),
        ("call_bash", "bash"),
    ]));
    items.push(tool_result(
        "call_big",
        "read",
        Some("src/big.rs"),
        big_rust_source(),
    ));
    items.push(tool_result("call_grep", "grep", None, big_grep_output()));
    items.push(tool_result("call_bash", "bash", None, big_bash_output()));
    for turn in 0..6 {
        let id = format!("call_filler_{}", turn);
        items.push(assistant_with(&[(id.as_str(), "read")]));
        items.push(tool_result(
            &id,
            "read",
            Some(&format!("src/filler_{}.rs", turn)),
            format!("     1\tpub fn filler_{}() -> usize {{ {} }}\n", turn, turn),
        ));
    }
    items
}

fn compaction_budget() -> BudgetConfig {
    let mut budget = BudgetConfig::default();
    budget.context_high_watermark_bytes = 8 * 1024;
    budget.context_low_watermark_bytes = 4 * 1024;
    budget.context_tail_window_bytes = 512;
    budget.context_min_tail_turns = 1;
    budget
}

fn stub_for<'a>(items: &'a [TranscriptItem], tool: &str) -> &'a str {
    items
        .iter()
        .find_map(|item| match item {
            TranscriptItem::ToolResult(tr) if tr.tool_name == tool => Some(&*tr.content),
            _ => None,
        })
        .expect("expected a compacted result for the tool")
}

/// Test 1: compaction embeds `source_entry_id`, records `EvidenceLink` metadata in the projector and
/// the session WAL, and reports an accurate `ContextBudgetBreakdown`.
#[test]
fn test_compaction_records_evidence_links_and_budget_breakdown() {
    let root = temp_root("links");
    let ws_dir = root.join("ws");
    fs::create_dir_all(&ws_dir).unwrap();
    let workspace = WorkspaceCap::new(&ws_dir).unwrap();
    let _ = workspace;

    let mut canonical = compaction_transcript();
    let budget = compaction_budget();
    let mut projector = ContextProjector::new();
    let projected = projector
        .project(&mut canonical, &budget, 2048, None, 8)
        .expect("projection should succeed");
    assert!(
        projected.compaction_event.is_some(),
        "the high watermark must have triggered compaction"
    );

    for tool in ["read", "grep", "bash"] {
        let stub = stub_for(&projected.messages, tool);
        assert!(
            stub.contains("source_entry_id=ev-"),
            "{} stub must embed a source_entry_id: {}",
            tool,
            stub
        );
        assert!(
            stub.contains("recall_evidence"),
            "{} stub must point at recall_evidence: {}",
            tool,
            stub
        );
    }

    let links = projector.evidence_links();
    assert_eq!(
        links.len(),
        3,
        "one evidence link per compacted output: {:?}",
        links
    );
    let read_link = links
        .iter()
        .find(|l| l.tool_name == "read")
        .expect("read evidence link");
    assert_eq!(read_link.source_entry_id, "ev-0002-call_big");
    assert_eq!(read_link.canonical_index, 2);
    assert_eq!(read_link.call_id, "call_big");
    assert_eq!(read_link.relative_path.as_deref(), Some("src/big.rs"));
    assert_eq!(read_link.original_sha256.len(), 64);
    assert_eq!(read_link.original_bytes, big_rust_source().len());
    assert_eq!(read_link.original_lines, big_rust_source().lines().count());
    assert!(read_link.stub_bytes < read_link.original_bytes);
    assert_eq!(read_link.reason, CompactionReason::StepDownArchive);
    assert!(links.iter().any(|l| l.tool_name == "grep"));
    assert!(links.iter().any(|l| l.tool_name == "bash"));

    // A later read of the same path is tagged `SupersededRead` rather than `StepDownArchive`.
    let mut superseded = vec![TranscriptItem::User(UserMessage {
        content: "t".into(),
    })];
    superseded.push(assistant_with(&[("call_r1", "read"), ("call_r2", "read")]));
    superseded.push(tool_result(
        "call_r1",
        "read",
        Some("src/same.rs"),
        big_rust_source(),
    ));
    superseded.push(tool_result(
        "call_r2",
        "read",
        Some("src/same.rs"),
        big_rust_source(),
    ));
    for turn in 0..6 {
        let id = format!("call_sup_filler_{}", turn);
        superseded.push(assistant_with(&[(id.as_str(), "read")]));
        superseded.push(tool_result(
            &id,
            "read",
            Some(&format!("src/sup_filler_{}.rs", turn)),
            format!(
                "     1\tpub fn sup_filler_{}() -> usize {{ {} }}\n",
                turn, turn
            ),
        ));
    }
    let mut sup_projector = ContextProjector::new();
    let _ = sup_projector
        .project(&mut superseded, &budget, 2048, None, 4)
        .expect("projection should succeed");
    assert!(
        sup_projector
            .evidence_links()
            .iter()
            .any(|l| l.reason == CompactionReason::SupersededRead),
        "a re-read path must be tagged SupersededRead: {:?}",
        sup_projector.evidence_links()
    );

    let stats = projector.stats();
    assert_eq!(stats.evidence_link_count, 3);
    assert_eq!(stats.budget.evidence_link_count, 3);
    assert!(stats.budget.compacted_stub_bytes > 0);
    assert!(stats.budget.recent_tail_bytes > 0);
    assert_eq!(
        stats.budget.total_projected_bytes,
        projected
            .messages
            .iter()
            .map(|i| i.estimated_bytes())
            .sum::<usize>()
    );
    assert!(stats.budget.total_projected_bytes > stats.budget.compacted_stub_bytes);
    assert!(MIN_COMPACTABLE_TOOL_BYTES < read_link.original_bytes);

    // The evidence graph round-trips through the session WAL.
    let journal = SessionJournal::create(root.join("sessions"), &ws_dir).unwrap();
    let mut journal = journal;
    for (turn, item) in compaction_transcript().into_iter().enumerate() {
        journal.append_item(turn, item).unwrap();
    }
    journal
        .sync_projected_transcript(&canonical, projector.evidence_links())
        .unwrap();
    let wal_path = journal.path().to_path_buf();
    drop(journal);
    let reopened = SessionJournal::open(&wal_path).unwrap();
    let wal_links = reopened.evidence_links();
    assert_eq!(
        wal_links.len(),
        3,
        "evidence links must persist in the WAL: {:?}",
        wal_links
    );
    assert!(wal_links
        .iter()
        .any(|l| l.source_entry_id == "ev-0002-call_big"));
    // The raw (pre-overlay) item is still addressable for retrieval after a resume.
    let active = reopened.active_transcript();
    assert!(stub_for(&active, "read").contains("source_entry_id=ev-0002-call_big"));
    let _ = fs::remove_dir_all(&root);
}

/// Test 2: `recall_evidence` returns exact line ranges and pattern matches from the raw output after
/// the projected transcript replaced it with an Outline Stub; unknown ids list the valid ones.
#[test]
fn test_recall_evidence_returns_line_ranges_and_pattern_matches() {
    let mut canonical = compaction_transcript();
    let budget = compaction_budget();
    let mut projector = ContextProjector::new();
    let _ = projector
        .project(&mut canonical, &budget, 2048, None, 8)
        .unwrap();

    // The constant is gone from the projected transcript...
    let stub = stub_for(&canonical, "read");
    assert!(stub.contains("[Compacted: read("));
    assert!(
        !stub.contains(HIDDEN_CONSTANT),
        "the stub must not leak the body: {}",
        stub
    );

    // ...but recoverable by exact line range.
    let out = execute_recall_evidence(
        &projector,
        r#"{"source_entry_id":"ev-0002-call_big","start_line":381,"end_line":382}"#,
    );
    assert!(out.outcome.is_success(), "{}", out.content);
    assert!(
        out.content
            .starts_with("[Recalled Evidence: ev-0002-call_big | Tool: read | Lines: 381-382 of"),
        "{}",
        out.content
    );
    assert!(out.content.contains(HIDDEN_CONSTANT), "{}", out.content);
    assert!(
        out.content.contains("   381\t"),
        "line numbers must be 1-based: {}",
        out.content
    );

    // Recoverable by the original call_id too.
    let by_call = execute_recall_evidence(
        &projector,
        r#"{"source_entry_id":"call_big","pattern":"4242"}"#,
    );
    assert!(by_call.outcome.is_success(), "{}", by_call.content);
    assert!(by_call.content.contains(HIDDEN_CONSTANT));
    assert!(
        by_call.content.contains("padding line 380") && by_call.content.contains("   382\t}"),
        "±2 lines of context expected: {}",
        by_call.content
    );

    // A non-matching pattern is an explicit error, not a silent empty success.
    let miss = execute_recall_evidence(
        &projector,
        r#"{"source_entry_id":"ev-0002-call_big","pattern":"no-such-token"}"#,
    );
    assert!(!miss.outcome.is_success());

    // An unknown id must list the ids that do exist.
    let unknown = execute_recall_evidence(&projector, r#"{"source_entry_id":"ev-9999-nope"}"#);
    assert!(!unknown.outcome.is_success());
    assert!(
        unknown.content.contains("ev-0002-call_big"),
        "{}",
        unknown.content
    );
    assert!(
        unknown.content.contains("ev-0003-call_grep"),
        "{}",
        unknown.content
    );
}

/// Test 3: end-to-end `AgentLoop` — a large read is compacted into an Outline Stub that strips the
/// function-body constant, the model recalls the hidden line by `source_entry_id`, and the earlier
/// projected prefix stays byte-identical.
#[tokio::test]
async fn test_agent_loop_recalls_evidence_from_an_outline_stub() {
    let root = temp_root("e2e");
    let ws_dir = root.join("ws");
    let state_dir = root.join("state");
    fs::create_dir_all(ws_dir.join("src")).unwrap();
    fs::create_dir_all(&state_dir).unwrap();
    fs::write(ws_dir.join("src/big.rs"), big_rust_source()).unwrap();
    for turn in 0..6 {
        fs::write(
            ws_dir.join(format!("src/filler_{}.rs", turn)),
            format!("pub fn filler_{}() -> usize {{ {} }}\n", turn, turn),
        )
        .unwrap();
    }

    // Drive compaction deterministically off the raw tool-output watermark: it trips as soon as a
    // second tool result exists, but the Working-Set/Tail window only lets the big read be archived
    // once it is two assistant turns old (the 4th request).
    let filler_len = format!("     1\tpub fn filler_0() -> usize {{ 0 }}\n").len();
    let mut budget = BudgetConfig::default();
    budget.max_model_turns = 16;
    budget.max_tool_calls = 32;
    budget.max_total_tool_output_bytes = big_rust_source().len() + filler_len;
    budget.context_tail_window_bytes = 512;
    budget.context_min_tail_turns = 1;
    budget.context_low_watermark_bytes = 12 * 1024;
    budget.context_high_watermark_bytes = 96 * 1024;

    let mut turns = vec![AssistantTurn {
        content: None,
        reasoning_blocks: vec![],
        tool_calls: vec![ToolCall {
            call_id: ToolCallId::new("call_big"),
            name: "read".into(),
            raw_arguments: r#"{"path":"src/big.rs"}"#.into(),
        }],
        finish_reason: FinishReason::ToolCalls,
        usage: None,
    }];
    for turn in 0..6 {
        turns.push(AssistantTurn {
            content: None,
            reasoning_blocks: vec![],
            tool_calls: vec![ToolCall {
                call_id: ToolCallId::new(format!("call_filler_{}", turn)),
                name: "read".into(),
                raw_arguments: format!(r#"{{"path":"src/filler_{}.rs"}}"#, turn),
            }],
            finish_reason: FinishReason::ToolCalls,
            usage: None,
        });
    }
    turns.push(AssistantTurn {
        content: None,
        reasoning_blocks: vec![],
        tool_calls: vec![ToolCall {
            call_id: ToolCallId::new("call_recall"),
            name: "recall_evidence".into(),
            raw_arguments: r#"{"source_entry_id":"ev-0002-call_big","pattern":"4242"}"#.into(),
        }],
        finish_reason: FinishReason::ToolCalls,
        usage: None,
    });
    turns.push(AssistantTurn {
        content: Some("recovered the constant".into()),
        reasoning_blocks: vec![],
        tool_calls: vec![],
        finish_reason: FinishReason::Stop,
        usage: None,
    });

    let provider = Arc::new(FakeProvider::new(turns));
    let env = Arc::new(LocalHostEnv::new(&ws_dir, state_dir.clone()).unwrap());
    let policy = PolicyEngine::new(
        PolicyMode::ReadOnly,
        Arc::new(NonInteractiveApprovalHandler),
    );
    let runtime = build_default_runtime(env, policy, budget.clone(), None);
    let cfg = AgentRunConfig::new("find the constant", budget);
    let logger = EventLogger::new(cfg.run_id, Some(state_dir.clone()), false).unwrap();
    let (_tx, rx) = watch::channel(false);
    let (summary, _logger) = AgentRunner::new(cfg, provider.clone(), runtime, rx)
        .run(logger)
        .await;
    assert_eq!(
        summary.status,
        RunStatus::Completed,
        "{:?}",
        summary.terminal_reason
    );

    // The read was compacted in the request the model saw...
    let bodies = provider.recorded_serialized_bodies();
    let stub_body = bodies
        .iter()
        .find(|body| body.contains("[Compacted: read("))
        .expect("the large read must have been compacted into an Outline Stub");
    assert!(
        stub_body.contains("source_entry_id=ev-0002-call_big"),
        "the stub must carry the deterministic source_entry_id"
    );
    assert!(
        !stub_body.contains(HIDDEN_CONSTANT),
        "the stub must not leak the function body constant"
    );

    // ...and the model recovered it through the evidence tool.
    let recalled = summary
        .transcript
        .iter()
        .find_map(|item| match item {
            TranscriptItem::ToolResult(tr) if tr.tool_name == "recall_evidence" => Some(tr),
            _ => None,
        })
        .expect("a recall_evidence tool result must be recorded");
    assert!(recalled.outcome.is_success(), "{}", recalled.content);
    assert!(
        recalled.content.contains(HIDDEN_CONSTANT),
        "{}",
        recalled.content
    );

    // Appending the recall call and its result never rewrites the earlier projected prefix.
    let pre_recall = &bodies[bodies.len() - 2];
    let final_body = bodies.last().unwrap();
    let prefix_marker = "call_filler_5";
    let prefix_len = pre_recall
        .rfind(prefix_marker)
        .map(|i| i + prefix_marker.len())
        .expect("the prefix tool result must be present in both requests");
    assert_eq!(
        &pre_recall[..prefix_len],
        &final_body[..prefix_len],
        "the projected prefix must stay byte-identical after the recall turn"
    );
    let _ = fs::remove_dir_all(&root);
}

/// Test 3b: the raw evidence a stub replaced survives a `--resume`, so `recall_evidence` still
/// resolves a `source_entry_id` minted in a previous process (the WAL keeps the pre-overlay item).
#[tokio::test]
async fn test_recall_evidence_survives_resume() {
    let root = temp_root("resume-evidence");
    let ws_dir = root.join("ws");
    let state_dir = root.join("state");
    fs::create_dir_all(ws_dir.join("src")).unwrap();
    fs::create_dir_all(&state_dir).unwrap();
    fs::write(ws_dir.join("src/big.rs"), big_rust_source()).unwrap();
    for turn in 0..6 {
        fs::write(
            ws_dir.join(format!("src/filler_{}.rs", turn)),
            format!("pub fn filler_{}() -> usize {{ {} }}\n", turn, turn),
        )
        .unwrap();
    }

    let filler_len = format!("     1\tpub fn filler_0() -> usize {{ 0 }}\n").len();
    let mut budget = BudgetConfig::default();
    budget.max_model_turns = 16;
    budget.max_tool_calls = 32;
    budget.max_total_tool_output_bytes = big_rust_source().len() + filler_len;
    budget.context_tail_window_bytes = 512;
    budget.context_min_tail_turns = 1;
    budget.context_low_watermark_bytes = 12 * 1024;
    budget.context_high_watermark_bytes = 96 * 1024;

    // Run 1: read the large file, then enough filler turns to archive it into an Outline Stub.
    let mut turns = vec![AssistantTurn {
        content: None,
        reasoning_blocks: vec![],
        tool_calls: vec![ToolCall {
            call_id: ToolCallId::new("call_big"),
            name: "read".into(),
            raw_arguments: r#"{"path":"src/big.rs"}"#.into(),
        }],
        finish_reason: FinishReason::ToolCalls,
        usage: None,
    }];
    for turn in 0..6 {
        turns.push(AssistantTurn {
            content: None,
            reasoning_blocks: vec![],
            tool_calls: vec![ToolCall {
                call_id: ToolCallId::new(format!("call_filler_{}", turn)),
                name: "read".into(),
                raw_arguments: format!(r#"{{"path":"src/filler_{}.rs"}}"#, turn),
            }],
            finish_reason: FinishReason::ToolCalls,
            usage: None,
        });
    }
    turns.push(AssistantTurn {
        content: Some("read everything".into()),
        reasoning_blocks: vec![],
        tool_calls: vec![],
        finish_reason: FinishReason::Stop,
        usage: None,
    });

    let journal = SessionJournal::create(state_dir.join("sessions"), &ws_dir).unwrap();
    let wal_path = journal.path().to_path_buf();
    let env = Arc::new(LocalHostEnv::new(&ws_dir, state_dir.clone()).unwrap());
    let policy = PolicyEngine::new(
        PolicyMode::ReadOnly,
        Arc::new(NonInteractiveApprovalHandler),
    );
    let runtime = build_default_runtime(env, policy, budget.clone(), None);
    let cfg = AgentRunConfig::new("read the sources", budget.clone());
    let logger = EventLogger::new(cfg.run_id, Some(state_dir.clone()), false).unwrap();
    let (_tx, rx) = watch::channel(false);
    let (first, _logger) = AgentRunner::new(cfg, Arc::new(FakeProvider::new(turns)), runtime, rx)
        .with_session_journal(journal)
        .run(logger)
        .await;
    assert_eq!(
        first.status,
        RunStatus::Completed,
        "{:?}",
        first.terminal_reason
    );
    let persisted_stub = first
        .transcript
        .iter()
        .find_map(|item| match item {
            TranscriptItem::ToolResult(tr) if tr.tool_name == "read" => Some(&*tr.content),
            _ => None,
        })
        .unwrap();
    assert!(
        persisted_stub.contains("source_entry_id=ev-0002-call_big"),
        "run 1 must have archived the read: {}",
        persisted_stub
    );

    // Run 2: resume the WAL. The stub is all that is left in the transcript, but the raw bytes are
    // still retrievable from the pre-overlay WAL entry.
    let resumed_journal = SessionJournal::open(&wal_path).unwrap();
    let resume_turns = vec![
        AssistantTurn {
            content: None,
            reasoning_blocks: vec![],
            tool_calls: vec![ToolCall {
                call_id: ToolCallId::new("call_recall"),
                name: "recall_evidence".into(),
                raw_arguments: r#"{"source_entry_id":"ev-0002-call_big","pattern":"4242"}"#.into(),
            }],
            finish_reason: FinishReason::ToolCalls,
            usage: None,
        },
        AssistantTurn {
            content: Some("recovered after resume".into()),
            reasoning_blocks: vec![],
            tool_calls: vec![],
            finish_reason: FinishReason::Stop,
            usage: None,
        },
    ];
    let env = Arc::new(LocalHostEnv::new(&ws_dir, state_dir.clone()).unwrap());
    let policy = PolicyEngine::new(
        PolicyMode::ReadOnly,
        Arc::new(NonInteractiveApprovalHandler),
    );
    let runtime = build_default_runtime(env, policy, budget.clone(), None);
    let cfg = AgentRunConfig::new("recall the constant", budget);
    let logger = EventLogger::new(cfg.run_id, Some(state_dir.clone()), false).unwrap();
    let (_tx, rx) = watch::channel(false);
    let (second, _logger) =
        AgentRunner::new(cfg, Arc::new(FakeProvider::new(resume_turns)), runtime, rx)
            .with_session_journal(resumed_journal)
            .run(logger)
            .await;
    assert_eq!(
        second.status,
        RunStatus::Completed,
        "{:?}",
        second.terminal_reason
    );
    let recalled = second
        .transcript
        .iter()
        .rev()
        .find_map(|item| match item {
            TranscriptItem::ToolResult(tr) if tr.tool_name == "recall_evidence" => Some(tr),
            _ => None,
        })
        .expect("recall_evidence result after resume");
    assert!(
        recalled.outcome.is_success(),
        "evidence must survive a resume: {}",
        recalled.content
    );
    assert!(
        recalled.content.contains(HIDDEN_CONSTANT),
        "{}",
        recalled.content
    );
    let _ = fs::remove_dir_all(&root);
}

/// Test 4: deterministic projection replay — the same canonical transcript and budget produce
/// byte-identical messages and `source_entry_id`s on a fresh run, after a `--resume`, and after a
/// `--fork` rewind.
#[test]
fn test_projection_replay_is_deterministic_across_resume_and_fork() {
    let root = temp_root("determinism");
    let ws_dir = root.join("ws");
    fs::create_dir_all(&ws_dir).unwrap();
    let budget = compaction_budget();

    let canonical_seed = compaction_transcript();
    let mut first = canonical_seed.clone();
    let mut projector_a = ContextProjector::new();
    let projected_a = projector_a
        .project(&mut first, &budget, 2048, None, 8)
        .unwrap();
    let links_a = projector_a.evidence_links().to_vec();
    assert!(!links_a.is_empty());

    // Fresh run: identical messages and identical evidence ids.
    let mut second = canonical_seed.clone();
    let mut projector_b = ContextProjector::new();
    let projected_b = projector_b
        .project(&mut second, &budget, 2048, None, 8)
        .unwrap();
    assert_eq!(projected_a.messages, projected_b.messages);
    assert_eq!(links_a, projector_b.evidence_links());

    // `--resume`: the WAL persists the compacted projection, and re-projecting it reproduces the
    // exact same messages.
    let mut journal = SessionJournal::create(root.join("sessions"), &ws_dir).unwrap();
    for (turn, item) in canonical_seed.clone().into_iter().enumerate() {
        journal.append_item(turn, item).unwrap();
    }
    journal
        .sync_projected_transcript(&first, projector_a.evidence_links())
        .unwrap();
    let wal_path = journal.path().to_path_buf();
    drop(journal);

    let resumed = SessionJournal::open(&wal_path).unwrap();
    let mut restored = resumed.active_transcript();
    let mut projector_c = ContextProjector::new();
    projector_c.restore_pinned_baseline_end(projector_a.last_pinned_baseline_end());
    let projected_c = projector_c
        .project(&mut restored, &budget, 2048, None, 8)
        .unwrap();
    assert_eq!(
        projected_c.messages, projected_a.messages,
        "a resumed projection must be byte-identical"
    );

    // `--fork`: rewinding the canonical transcript must keep the retained prefix byte-identical.
    let fork_len = 7usize;
    let mut forked = first.clone();
    forked.truncate(fork_len);
    let projected_fork = projector_a
        .project(&mut forked, &budget, 2048, None, 4)
        .unwrap();
    assert_eq!(
        projected_fork.messages,
        projected_a.messages[..fork_len].to_vec(),
        "a forked projection must preserve the surviving prefix byte-for-byte"
    );
    let _ = fs::remove_dir_all(&root);
}

/// Test 5: Gate 1 polish items O1–O6.
#[tokio::test]
async fn test_gate1_o1_o6_polish_items_are_verified() {
    // ---- O1: a `bash` command that rewrites the workspace is snapshotted ----
    let root = temp_root("o1");
    let ws_dir = root.join("ws");
    let state_dir = root.join("state");
    fs::create_dir_all(&ws_dir).unwrap();
    fs::create_dir_all(&state_dir).unwrap();
    fs::write(ws_dir.join("seed.txt"), "seed\n").unwrap();

    let store = Arc::new(SnapshotStore::new(root.join("snapshots")));
    let journal = SessionJournal::create(state_dir.join("sessions"), &ws_dir).unwrap();
    let wal_path = journal.path().to_path_buf();

    let provider = Arc::new(FakeProvider::new(vec![
        AssistantTurn {
            content: None,
            reasoning_blocks: vec![],
            tool_calls: vec![ToolCall {
                call_id: ToolCallId::new("call_bash"),
                name: "bash".into(),
                raw_arguments: r#"{"command":"printf 'made by bash\\n' > made_by_bash.txt"}"#
                    .into(),
            }],
            finish_reason: FinishReason::ToolCalls,
            usage: None,
        },
        AssistantTurn {
            content: Some("done".into()),
            reasoning_blocks: vec![],
            tool_calls: vec![],
            finish_reason: FinishReason::Stop,
            usage: None,
        },
    ]));
    let env = Arc::new(LocalHostEnv::new(&ws_dir, state_dir.clone()).unwrap());
    let policy = PolicyEngine::new(
        PolicyMode::AllowShell,
        Arc::new(NonInteractiveApprovalHandler),
    );
    let budget = BudgetConfig::default();
    let runtime = build_default_runtime(env, policy, budget.clone(), None);
    let cfg = AgentRunConfig::new("write a file through the shell", budget);
    let logger = EventLogger::new(cfg.run_id, Some(state_dir.clone()), false).unwrap();
    let (_tx, rx) = watch::channel(false);
    let (summary, _logger) = AgentRunner::new(cfg, provider, runtime, rx)
        .with_session_journal(journal)
        .with_snapshot_store(store.clone())
        .run(logger)
        .await;
    assert_eq!(
        summary.status,
        RunStatus::Completed,
        "{:?}",
        summary.terminal_reason
    );
    assert!(ws_dir.join("made_by_bash.txt").exists());

    let journal = SessionJournal::open(&wal_path).unwrap();
    let records = journal.revision_records();
    assert!(
        records
            .iter()
            .any(|r| r.changed_paths.iter().any(|p| p == "made_by_bash.txt")),
        "a bash mutation must produce a revision capture: {:?}",
        records
    );

    // ---- O2: blobs are written atomically, so no temporary file is left behind ----
    let store = SnapshotStore::new(root.join("snapshots"));
    store.ensure_layout().unwrap();
    let workspace = WorkspaceCap::new(&ws_dir).unwrap();
    let rev = store.capture_revision(&workspace, None, None, 1).unwrap();
    let blob_dir = store.root().join("blobs");
    let mut leftovers = Vec::new();
    for shard in fs::read_dir(&blob_dir).unwrap() {
        for entry in fs::read_dir(shard.unwrap().path()).unwrap() {
            let name = entry.unwrap().file_name().to_string_lossy().to_string();
            if name.contains(".tmp-") {
                leftovers.push(name);
            }
        }
    }
    assert!(
        leftovers.is_empty(),
        "atomic blob writes must leave no temp files: {:?}",
        leftovers
    );
    // Re-storing the same blob is idempotent.
    let again = store.capture_revision(&workspace, None, None, 1).unwrap();
    assert_eq!(rev.revision_id, again.revision_id);

    // ---- O3: `ensure_private_dir` refuses a symlinked store path ----
    let symlink_root = root.join("symlink-store");
    let target = root.join("symlink-target");
    fs::create_dir_all(&target).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&target, &symlink_root).unwrap();
    #[cfg(windows)]
    std::os::windows::fs::symlink_dir(&target, &symlink_root).unwrap();
    let symlink_store = SnapshotStore::new(&symlink_root);
    assert!(
        symlink_store.ensure_layout().is_err(),
        "a symlinked snapshot store must be refused, not chmod-ed through"
    );

    // A path containing `..` is lexically normalized, and only the final component is chmod-ed, so
    // pre-existing ancestors keep their own permissions.
    let dotted_root = root.join("dotted");
    fs::create_dir_all(&dotted_root).unwrap();
    let dotted_store = SnapshotStore::new(dotted_root.join("..").join("dotted").join("snapshots"));
    dotted_store.ensure_layout().unwrap();
    let blobs = dotted_root.join("snapshots").join("blobs");
    assert!(blobs.is_dir(), "`..` must be resolved lexically");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&blobs).unwrap().permissions().mode() & 0o777,
            0o700,
            "the store directory must be user-private"
        );
        assert_ne!(
            fs::metadata(&dotted_root).unwrap().permissions().mode() & 0o777,
            0o700,
            "pre-existing ancestors must keep their own permissions"
        );
    }

    // ---- O3b: a *symlinked ancestor* must be traversed, not refused ----
    //
    // Android ships `/data/data` as a symlink to `/data/user/0` (and macOS symlinks `/var`,
    // `/tmp`, `/etc`), so refusing every symlink component made the snapshot store unusable
    // there: the very first baseline capture aborted with `Permission denied`. Only the store
    // directory itself must be a real directory; OS-owned ancestors are not ours to police.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let real_parent = root.join("ancestor-real");
        fs::create_dir_all(&real_parent).unwrap();
        let linked_parent = root.join("ancestor-link");
        std::os::unix::fs::symlink(&real_parent, &linked_parent).unwrap();
        let nested_store = SnapshotStore::new(linked_parent.join("nested").join("snapshots"));
        nested_store
            .ensure_layout()
            .expect("a symlinked ancestor directory must be traversed");
        assert!(real_parent.join("nested").join("snapshots").is_dir());
        assert_eq!(
            fs::metadata(real_parent.join("nested").join("snapshots"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700,
            "the created store directory must still be user-private"
        );
    }

    // ---- O4: revision records are ordered by branch depth, not append order ----
    let o4_root = temp_root("o4");
    let o4_ws = o4_root.join("ws");
    fs::create_dir_all(&o4_ws).unwrap();
    let o4_store = SnapshotStore::new(o4_root.join("snapshots"));
    let o4_workspace = WorkspaceCap::new(&o4_ws).unwrap();
    let mut o4_journal = SessionJournal::create(o4_root.join("sessions"), &o4_ws).unwrap();
    let a = o4_journal
        .append_item(
            0,
            TranscriptItem::User(UserMessage {
                content: "a".into(),
            }),
        )
        .unwrap();
    let b = o4_journal
        .append_item(
            1,
            TranscriptItem::User(UserMessage {
                content: "b".into(),
            }),
        )
        .unwrap();
    let c = o4_journal
        .append_item(
            2,
            TranscriptItem::User(UserMessage {
                content: "c".into(),
            }),
        )
        .unwrap();
    let rev_deep = o4_store
        .capture_revision(&o4_workspace, None, None, 2)
        .unwrap()
        .revision_id;
    o4_journal
        .record_revision_capture(2, rev_deep.as_str(), None, vec![], 0)
        .unwrap();
    o4_journal.fork_at(&b).unwrap();
    let rev_shallow = o4_store
        .capture_revision(&o4_workspace, None, None, 1)
        .unwrap()
        .revision_id;
    o4_journal
        .record_revision_capture(1, rev_shallow.as_str(), None, vec![], 0)
        .unwrap();
    // Re-select the deeper leaf: both records are now on the active branch and the deeper one was
    // appended *earlier*, so append order alone would report the ancestor as "latest".
    o4_journal.fork_at(&c).unwrap();
    let ordered = o4_journal.revision_records();
    assert_eq!(ordered.len(), 2, "{:?}", ordered);
    assert_eq!(
        ordered.last().unwrap().revision_id,
        rev_deep.as_str(),
        "the deepest descendant must be reported last: {:?}",
        ordered
    );
    assert_eq!(
        o4_journal.latest_revision_id().as_deref(),
        Some(rev_deep.as_str())
    );
    let _ = (a, rev_shallow);

    // ---- O5/O6: `compare`, `newest_revision`, and the crash-before-WAL-append reconciliation ----
    let cmp = store.compare(&rev.revision_id, &rev.revision_id).unwrap();
    assert!(cmp.is_empty(), "a revision must not differ from itself");
    assert_eq!(
        store.newest_revision().unwrap().map(|r| r.revision_id),
        Some(rev.revision_id.clone())
    );

    let _ = fs::remove_dir_all(&root);
    let _ = fs::remove_dir_all(&o4_root);
}

/// Test 5b (O5/O6): a snapshot manifest written immediately before a crash that stopped the WAL
/// append is reconciled so `latest_revision_id()` still describes the workspace on disk.
#[test]
fn test_crash_before_wal_append_is_reconciled() {
    let root = temp_root("o6");
    let ws_dir = root.join("ws");
    fs::create_dir_all(&ws_dir).unwrap();
    fs::write(ws_dir.join("a.txt"), "one\n").unwrap();
    let workspace = WorkspaceCap::new(&ws_dir).unwrap();
    let store = SnapshotStore::new(root.join("snapshots"));
    let mut journal = SessionJournal::create(root.join("sessions"), &ws_dir).unwrap();
    journal
        .append_item(
            0,
            TranscriptItem::User(UserMessage {
                content: "a".into(),
            }),
        )
        .unwrap();

    // Baseline is recorded normally.
    let baseline = store.capture_revision(&workspace, None, None, 0).unwrap();
    journal
        .record_revision_capture(0, baseline.revision_id.as_str(), None, vec![], 0)
        .unwrap();

    // The mutation snapshot is written, then the process dies before the WAL append.
    fs::write(ws_dir.join("a.txt"), "two\n").unwrap();
    let orphan = store
        .capture_revision(
            &workspace,
            Some(&baseline.revision_id),
            journal.leaf_id(),
            1,
        )
        .unwrap();
    assert_ne!(orphan.revision_id, baseline.revision_id);
    assert_eq!(
        journal.latest_revision_id().as_deref(),
        Some(baseline.revision_id.as_str()),
        "pre-reconcile the WAL still names the stale revision"
    );

    // Reconciliation (the production path) adopts the manifest bound to the *current* leaf.
    assert!(journal.is_entry_on_active_branch(journal.leaf_id().unwrap()));
    let adopted = store.reconcile_with_wal(&mut journal).unwrap();
    assert_eq!(adopted.as_deref(), Some(orphan.revision_id.as_str()));
    assert_eq!(
        journal.latest_revision_id().as_deref(),
        Some(orphan.revision_id.as_str())
    );
    assert!(journal
        .revision_records()
        .iter()
        .any(|r| r.changed_paths.iter().any(|p| p == "a.txt")));
    // Reconciling twice is idempotent.
    assert_eq!(store.reconcile_with_wal(&mut journal).unwrap(), None);
    assert_eq!(journal.revision_records().len(), 2);

    // A real (non-self) comparison must report the changed path.
    assert_eq!(
        store
            .compare(&baseline.revision_id, &orphan.revision_id)
            .unwrap(),
        vec!["a.txt".to_string()]
    );
    assert!(store
        .compare(&baseline.revision_id, &baseline.revision_id)
        .unwrap()
        .is_empty());
    let _ = fs::remove_dir_all(&root);
}

/// A transcript that reads `src/same.rs` twice with distinct bodies, optionally followed by a
/// *failed* third read of the same path.
fn superseded_read_transcript(
    secret1: &str,
    secret2: &str,
    failed_reread: bool,
) -> Vec<TranscriptItem> {
    let mut items = vec![TranscriptItem::User(UserMessage {
        content: "reread a file".into(),
    })];
    items.push(assistant_with(&[("call_r1", "read"), ("call_r2", "read")]));
    items.push(tool_result(
        "call_r1",
        "read",
        Some("src/same.rs"),
        big_rust_source_with(secret1),
    ));
    items.push(tool_result(
        "call_r2",
        "read",
        Some("src/same.rs"),
        big_rust_source_with(secret2),
    ));
    for turn in 0..6 {
        push_read_turn(&mut items, "sup", turn);
    }
    if failed_reread {
        items.push(assistant_with(&[("call_r3", "read")]));
        items.push(TranscriptItem::ToolResult(ToolResultMessage {
            call_id: ToolCallId::new("call_r3"),
            tool_name: "read".into(),
            outcome: ToolOutcome::Error {
                code: ErrorCode::IoError,
            },
            content: Arc::from("read failed"),
            meta: ToolResultMeta {
                relative_path: Some("src/same.rs".into()),
                ..Default::default()
            },
        }));
    }
    items
}

/// `SupersededRead` recall: when a file is read twice, the *first* read is archived with
/// `CompactionReason::SupersededRead`, and `recall_evidence` must still return that first read's
/// exact bytes — never the newer read's.
#[test]
fn test_superseded_read_recall_returns_the_earlier_read_bytes() {
    const V1: &str = "v1_secret_marker";
    const V2: &str = "v2_secret_marker";
    let budget = compaction_budget();

    let mut items = superseded_read_transcript(V1, V2, false);
    let mut projector = ContextProjector::new();
    let _ = projector
        .project(&mut items, &budget, 2048, None, 8)
        .unwrap();

    let r1 = projector
        .evidence_links()
        .iter()
        .find(|l| l.call_id == "call_r1")
        .expect("the first read must be archived")
        .clone();
    assert_eq!(r1.reason, CompactionReason::SupersededRead);
    let r2 = projector
        .evidence_links()
        .iter()
        .find(|l| l.call_id == "call_r2")
        .expect("the second read must be archived")
        .clone();
    assert_eq!(r2.reason, CompactionReason::StepDownArchive);
    assert_ne!(r1.source_entry_id, r2.source_entry_id);

    let first = execute_recall_evidence(
        &projector,
        &format!(
            r#"{{"source_entry_id":"{}","pattern":"{}"}}"#,
            r1.source_entry_id, V1
        ),
    );
    assert!(first.outcome.is_success(), "{}", first.content);
    assert!(first.content.contains(V1), "{}", first.content);
    assert!(!first.content.contains(V2), "{}", first.content);

    let second = execute_recall_evidence(
        &projector,
        &format!(
            r#"{{"source_entry_id":"{}","pattern":"{}"}}"#,
            r2.source_entry_id, V2
        ),
    );
    assert!(second.outcome.is_success(), "{}", second.content);
    assert!(second.content.contains(V2), "{}", second.content);
    assert!(!second.content.contains(V1), "{}", second.content);

    // A *failed* later read must not mark an earlier successful read as superseded.
    let mut with_failed_reread = superseded_read_transcript(V1, V2, true);
    let mut projector = ContextProjector::new();
    let _ = projector
        .project(&mut with_failed_reread, &budget, 2048, None, 8)
        .unwrap();
    let r2_again = projector
        .evidence_links()
        .iter()
        .find(|l| l.call_id == "call_r2")
        .expect("the second read must still be archived");
    assert_eq!(
        r2_again.reason,
        CompactionReason::StepDownArchive,
        "a failed later read must not supersede a successful one"
    );
}

// ---------------------------------------------------------------------------------------------
// Post-Gemini-audit Gate 2 regressions (v0.10.0 final): branch isolation, resume bookkeeping,
// UTF-8 / byte-cap recall pagination, and the four-profile A/B evaluation record.
// ---------------------------------------------------------------------------------------------

/// A successful `read` tool result carrying an explicit line window.
fn paged_read(call_id: &str, path: &str, start: usize, end: usize, body: String) -> TranscriptItem {
    TranscriptItem::ToolResult(ToolResultMessage {
        call_id: ToolCallId::new(call_id),
        tool_name: "read".into(),
        outcome: ToolOutcome::Success,
        content: Arc::from(body),
        meta: ToolResultMeta {
            relative_path: Some(path.to_string()),
            lines_range: Some((start, end)),
            ..Default::default()
        },
    })
}

/// BUG-07: paging through one file (1-200, then 201-400, then 401-600) must not be mistaken for a
/// later read superseding the earlier one, and must not inflate the reread-regret counter per page.
#[test]
fn test_bug07_paged_reads_are_not_superseded_and_do_not_inflate_reread_regret() {
    let budget = compaction_budget();
    let mut items = vec![TranscriptItem::User(UserMessage {
        content: "page through a file".into(),
    })];
    items.push(assistant_with(&[
        ("call_p1", "read"),
        ("call_p2", "read"),
        ("call_p3", "read"),
    ]));
    items.push(paged_read(
        "call_p1",
        "src/paged.rs",
        1,
        200,
        big_rust_source_with("page1_marker"),
    ));
    items.push(paged_read(
        "call_p2",
        "src/paged.rs",
        201,
        400,
        big_rust_source_with("page2_marker"),
    ));
    items.push(paged_read(
        "call_p3",
        "src/paged.rs",
        401,
        600,
        big_rust_source_with("page3_marker"),
    ));
    for turn in 0..6 {
        push_read_turn(&mut items, "tail", turn);
    }
    let mut projector = ContextProjector::new();
    let _ = projector
        .project(&mut items, &budget, 2048, None, 8)
        .unwrap();

    let paged_reasons: Vec<CompactionReason> = projector
        .evidence_links()
        .iter()
        .filter(|link| link.call_id.starts_with("call_p"))
        .map(|link| link.reason)
        .collect();
    assert!(
        !paged_reasons.is_empty(),
        "paginated reads should have been archived"
    );
    assert!(
        paged_reasons
            .iter()
            .all(|reason| *reason != CompactionReason::SupersededRead),
        "paging must not be misread as supersession: {:?}",
        paged_reasons
    );

    // Re-reading the file after compaction is regret, counted once per compaction — not once per
    // page.
    let before = projector.compaction_reread_count();
    projector.notify_file_read("src/paged.rs");
    projector.notify_file_read("src/paged.rs");
    projector.notify_file_read("src/paged.rs");
    assert_eq!(
        projector.compaction_reread_count(),
        before + 1,
        "three paged reads must not each inflate the reread-regret counter"
    );
}

/// BUG-07: the two cases that *do* supersede an earlier read must still be detected — a later read
/// whose window covers the earlier one, and a successful `edit` between two reads.
#[test]
fn test_bug07_covering_read_and_intervening_edit_still_supersede() {
    let budget = compaction_budget();

    // (a) A later read covering the earlier window supersedes it.
    let mut items = vec![TranscriptItem::User(UserMessage {
        content: "cover".into(),
    })];
    items.push(assistant_with(&[("call_a", "read"), ("call_b", "read")]));
    items.push(paged_read(
        "call_a",
        "src/cover.rs",
        1,
        200,
        big_rust_source_with("early_marker"),
    ));
    items.push(paged_read(
        "call_b",
        "src/cover.rs",
        1,
        400,
        big_rust_source_with("wide_marker"),
    ));
    for turn in 0..6 {
        push_read_turn(&mut items, "tail", turn);
    }
    let mut projector = ContextProjector::new();
    let _ = projector
        .project(&mut items, &budget, 2048, None, 8)
        .unwrap();
    let a = projector
        .evidence_links()
        .iter()
        .find(|link| link.call_id == "call_a")
        .expect("the earlier read must be archived");
    assert_eq!(a.reason, CompactionReason::SupersededRead);

    // (b) A successful `edit` between reads supersedes the earlier read.
    let mut items = vec![TranscriptItem::User(UserMessage {
        content: "edit".into(),
    })];
    items.push(assistant_with(&[("call_e1", "read"), ("call_ed", "edit")]));
    items.push(paged_read(
        "call_e1",
        "src/edited.rs",
        1,
        200,
        big_rust_source_with("before_edit"),
    ));
    items.push(TranscriptItem::ToolResult(ToolResultMessage {
        call_id: ToolCallId::new("call_ed"),
        tool_name: "edit".into(),
        outcome: ToolOutcome::Success,
        content: Arc::from("edited"),
        meta: ToolResultMeta {
            relative_path: Some("src/edited.rs".into()),
            ..Default::default()
        },
    }));
    for turn in 0..6 {
        push_read_turn(&mut items, "tail", turn);
    }
    let mut projector = ContextProjector::new();
    let _ = projector
        .project(&mut items, &budget, 2048, None, 8)
        .unwrap();
    let e1 = projector
        .evidence_links()
        .iter()
        .find(|link| link.call_id == "call_e1")
        .expect("the pre-edit read must be archived");
    assert_eq!(e1.reason, CompactionReason::SupersededRead);
}

/// BUG-07: the raw-evidence store is capped. When it overflows, the oldest entries are evicted and
/// `recall_evidence` returns an understandable error rather than panicking or returning empty.
#[test]
fn test_bug07_evidence_cap_evicts_oldest_and_recall_reports_unretained_bytes() {
    let mut projector = ContextProjector::new();
    let big = "x".repeat(MAX_EVIDENCE_BYTES / 2 + 1024);
    let make_link =
        |source_entry_id: &str, canonical_index: usize, call_id: &str, path: &str| EvidenceLink {
            source_entry_id: source_entry_id.to_string(),
            canonical_index,
            call_id: call_id.to_string(),
            tool_name: "read".to_string(),
            relative_path: Some(path.to_string()),
            original_sha256: sha256_hex64(big.as_bytes()),
            original_bytes: big.len(),
            original_lines: 1,
            stub_bytes: 10,
            reason: CompactionReason::StepDownArchive,
        };
    projector.register_evidence(
        make_link("ev-0000-old", 0, "call_old", "src/old.rs"),
        Arc::from(big.as_str()),
    );
    projector.register_evidence(
        make_link("ev-0001-new", 1, "call_new", "src/new.rs"),
        Arc::from(big.as_str()),
    );

    assert!(
        projector.evidence_lookup("ev-0000-old").is_none(),
        "the oldest entry's raw bytes must be evicted"
    );
    assert!(
        projector.evidence_lookup("ev-0001-new").is_some(),
        "the newest entry's raw bytes must be retained"
    );
    assert!(
        projector.evidence_link("ev-0000-old").is_some(),
        "the evicted entry must stay addressable"
    );

    let out = execute_recall_evidence(&projector, r#"{"source_entry_id":"ev-0000-old"}"#);
    assert!(!out.outcome.is_success(), "{}", out.content);
    assert!(
        out.content.contains("no longer retained"),
        "expected an understandable 'bytes gone' error, got: {}",
        out.content
    );
}

const FIRST_BRANCH_SECRET: &str = "FIRST_BRANCH_SECRET";
const ABANDONED_BRANCH_SECRET: &str = "ABANDONED_BRANCH_SECRET";

/// A 383-line "read" body whose function-body secret differs per branch.
fn branch_big_source(secret: &str) -> String {
    big_rust_source_with(secret)
}

fn small_read(call_id: &str, path: &str) -> TranscriptItem {
    tool_result(
        call_id,
        "read",
        Some(path),
        format!(
            "     1\tpub fn {}() -> usize {{ 0 }}\n",
            path.replace(['/', '.'], "_")
        ),
    )
}

fn push_read_turn(items: &mut Vec<TranscriptItem>, prefix: &str, turn: usize) {
    let id = format!("call_{}_{}", prefix, turn);
    items.push(assistant_with(&[(id.as_str(), "read")]));
    items.push(small_read(&id, &format!("src/{}_{}.rs", prefix, turn)));
}

fn append_both(
    journal: &mut SessionJournal,
    items: &mut Vec<TranscriptItem>,
    turn: usize,
    item: TranscriptItem,
) -> String {
    items.push(item.clone());
    journal.append_item(turn, item).unwrap()
}

/// Gate 2 branch isolation: a compaction overlay recorded on an abandoned branch must never leak
/// onto the live branch, an ancestor stub must stay resolvable after `--fork` + `--resume`, and the
/// same canonical index must be re-compactable on the new branch.
#[test]
fn test_fork_isolates_evidence_links_across_branches() {
    let root = temp_root("fork-isolation");
    let ws_dir = root.join("ws");
    fs::create_dir_all(&ws_dir).unwrap();
    let budget = compaction_budget();

    let mut journal = SessionJournal::create(root.join("sessions"), &ws_dir).unwrap();
    let mut items: Vec<TranscriptItem> = Vec::new();
    let mut ids: Vec<String> = Vec::new();

    // Branch A, first half: one large read that must be archived before the fork point.
    ids.push(append_both(
        &mut journal,
        &mut items,
        0,
        TranscriptItem::User(UserMessage {
            content: "task".into(),
        }),
    ));
    ids.push(append_both(
        &mut journal,
        &mut items,
        1,
        assistant_with(&[("call_big1", "read")]),
    ));
    ids.push(append_both(
        &mut journal,
        &mut items,
        2,
        tool_result(
            "call_big1",
            "read",
            Some("src/first.rs"),
            branch_big_source(FIRST_BRANCH_SECRET),
        ),
    ));
    for turn in 0..6 {
        let id = format!("call_f{}", turn);
        ids.push(append_both(
            &mut journal,
            &mut items,
            2,
            assistant_with(&[(id.as_str(), "read")]),
        ));
        ids.push(append_both(
            &mut journal,
            &mut items,
            2,
            small_read(&id, &format!("src/f{}.rs", turn)),
        ));
    }
    assert_eq!(items.len(), 15);
    // Sync #1 records the index-2 overlay at the leaf that is still an ancestor after the fork.
    let mut projector_a = ContextProjector::new();
    let _ = projector_a
        .project(&mut items, &budget, 2048, None, 8)
        .unwrap();
    journal
        .sync_projected_transcript(&items, projector_a.evidence_links())
        .unwrap();
    assert!(projector_a
        .evidence_links()
        .iter()
        .any(|l| l.source_entry_id == "ev-0002-call_big1"));

    // Branch A, second half: a second large read archived at a *deeper* leaf.
    ids.push(append_both(
        &mut journal,
        &mut items,
        3,
        assistant_with(&[("call_big2", "read")]),
    ));
    ids.push(append_both(
        &mut journal,
        &mut items,
        4,
        tool_result(
            "call_big2",
            "read",
            Some("src/second.rs"),
            branch_big_source(ABANDONED_BRANCH_SECRET),
        ),
    ));
    for turn in 0..3 {
        let id = format!("call_g{}", turn);
        ids.push(append_both(
            &mut journal,
            &mut items,
            4,
            assistant_with(&[(id.as_str(), "read")]),
        ));
        ids.push(append_both(
            &mut journal,
            &mut items,
            4,
            small_read(&id, &format!("src/g{}.rs", turn)),
        ));
    }
    assert_eq!(items.len(), 23);
    let _ = projector_a
        .project(&mut items, &budget, 2048, None, 12)
        .unwrap();
    journal
        .sync_projected_transcript(&items, projector_a.evidence_links())
        .unwrap();
    assert!(
        projector_a
            .evidence_links()
            .iter()
            .any(|l| l.source_entry_id == "ev-0016-call_big2"),
        "the second half must have archived index 16: {:?}",
        projector_a.evidence_links()
    );
    // Fork onto the entry at index 20: it keeps the first half *and* the second large read, while
    // abandoning the last two entries (and the deeper overlay that archived index 16).
    let fork_point = ids[20].clone();
    let wal_path = journal.path().to_path_buf();
    drop(journal);

    // Re-open and rewind onto branch B: everything appended after `fork_point` is abandoned.
    let mut reopened = SessionJournal::open(&wal_path).unwrap();
    reopened.fork_at(&fork_point).unwrap();
    let branch_b_len = reopened.active_entries().len();
    assert_eq!(branch_b_len, 21);

    let links = reopened.evidence_links();
    assert_eq!(
        links.len(),
        1,
        "only the ancestor stub's link may survive the rewind: {:?}",
        links
    );
    assert_eq!(links[0].source_entry_id, "ev-0002-call_big1");

    let mut projector = ContextProjector::new();
    reopened.seed_projector_evidence(&mut projector);
    // The ancestor stub still resolves to the exact branch-A raw lines.
    let ancestor = execute_recall_evidence(
        &projector,
        r#"{"source_entry_id":"ev-0002-call_big1","pattern":"FIRST_BRANCH_SECRET"}"#,
    );
    assert!(ancestor.outcome.is_success(), "{}", ancestor.content);
    assert!(
        ancestor.content.contains(FIRST_BRANCH_SECRET),
        "{}",
        ancestor.content
    );
    // The abandoned branch's evidence id must be rejected.
    let abandoned =
        execute_recall_evidence(&projector, r#"{"source_entry_id":"ev-0016-call_big2"}"#);
    assert!(
        !abandoned.outcome.is_success(),
        "an abandoned branch's source_entry_id must not resolve: {}",
        abandoned.content
    );
    assert!(
        abandoned.content.contains("unknown source_entry_id"),
        "{}",
        abandoned.content
    );

    // The new branch re-compacts canonical index 16 with the same call_id, minting the same
    // deterministic id. That must register and resolve cleanly on the live branch.
    let mut branch_b = reopened.active_transcript();
    let mut projector_b = ContextProjector::new();
    reopened.seed_projector_evidence(&mut projector_b);
    let _ = projector_b
        .project(&mut branch_b, &budget, 2048, None, 10)
        .unwrap();
    assert!(
        projector_b
            .evidence_links()
            .iter()
            .any(|l| l.source_entry_id == "ev-0016-call_big2"),
        "branch B must be able to compact index 16 afresh: {:?}",
        projector_b.evidence_links()
    );
    let branch_b_recall = execute_recall_evidence(
        &projector_b,
        r#"{"source_entry_id":"ev-0016-call_big2","pattern":"ABANDONED_BRANCH_SECRET"}"#,
    );
    assert!(
        branch_b_recall.outcome.is_success(),
        "{}",
        branch_b_recall.content
    );
    assert!(branch_b_recall.content.contains(ABANDONED_BRANCH_SECRET));
    let _ = fs::remove_dir_all(&root);
}

/// Gate 2 `--fork` state hygiene: rewinding the canonical transcript must prune the rewound items'
/// evidence (link, raw bytes and `call_id` index), so a fresh compaction at the same canonical index
/// with the same `call_id` registers cleanly instead of being dropped as a duplicate.
#[test]
fn test_fork_rewind_prunes_evidence_and_allows_recompaction_at_the_same_index() {
    let budget = compaction_budget();
    let mut canonical = compaction_transcript();
    let mut projector = ContextProjector::new();
    let _ = projector
        .project(&mut canonical, &budget, 2048, None, 8)
        .unwrap();
    assert!(projector.evidence_lookup("ev-0002-call_big").is_some());
    assert!(projector.evidence_lookup("call_big").is_some());

    // `--fork` back to a 1-item prefix: index 2 is now past the tail and its evidence must go.
    let mut forked = canonical[..1].to_vec();
    let _ = projector
        .project(&mut forked, &budget, 2048, None, 1)
        .unwrap();
    assert!(
        !projector
            .evidence_links()
            .iter()
            .any(|l| l.source_entry_id == "ev-0002-call_big"),
        "a rewound item's evidence link must be pruned: {:?}",
        projector.evidence_links()
    );
    assert!(
        projector.evidence_lookup("ev-0002-call_big").is_none()
            && projector.evidence_lookup("call_big").is_none(),
        "the rewound raw bytes and call_id index must be pruned too"
    );

    // Re-grow the same (index, call_id) pair on the new branch with *different* content: the same
    // deterministic `source_entry_id` must register the new bytes rather than being dropped.
    let mut regrown = canonical[..1].to_vec();
    regrown.push(assistant_with(&[("call_big", "read")]));
    regrown.push(tool_result(
        "call_big",
        "read",
        Some("src/big.rs"),
        big_rust_source_with(HIDDEN_CONSTANT_V2),
    ));
    for turn in 0..6 {
        push_read_turn(&mut regrown, "re", turn);
    }
    let _ = projector
        .project(&mut regrown, &budget, 2048, None, 8)
        .unwrap();
    assert_eq!(
        projector
            .evidence_links()
            .iter()
            .filter(|l| l.source_entry_id == "ev-0002-call_big")
            .count(),
        1,
        "re-compaction at the same index must register exactly one link: {:?}",
        projector.evidence_links()
    );
    let fresh = execute_recall_evidence(
        &projector,
        r#"{"source_entry_id":"ev-0002-call_big","pattern":"5151"}"#,
    );
    assert!(fresh.outcome.is_success(), "{}", fresh.content);
    assert!(fresh.content.contains(HIDDEN_CONSTANT_V2));
    let stale = execute_recall_evidence(
        &projector,
        r#"{"source_entry_id":"ev-0002-call_big","pattern":"4242"}"#,
    );
    assert!(
        !stale.outcome.is_success(),
        "the rewound branch's raw bytes must not survive: {}",
        stale.content
    );
}

/// Gate 2 resume accounting: re-seeding compacted stubs from the WAL must also restore the
/// `compacted_indices` / `compacted_file_paths` bookkeeping. Otherwise `--resume` under-reports
/// `ContextBudgetBreakdown.compacted_stub_bytes` (pre-compacted stubs get misattributed to the tail
/// or to the pinned working set) and reread-regret accounting goes blind.
#[test]
fn test_resume_restores_compacted_indices_and_budget_breakdown() {
    let root = temp_root("resume-indices");
    let ws_dir = root.join("ws");
    fs::create_dir_all(&ws_dir).unwrap();
    let budget = compaction_budget();

    let mut canonical = compaction_transcript();
    let mut projector = ContextProjector::new();
    let _ = projector
        .project(&mut canonical, &budget, 2048, None, 8)
        .unwrap();
    let fresh_breakdown = projector.stats().budget.clone();
    assert!(fresh_breakdown.compacted_stub_bytes > 0);

    let mut journal = SessionJournal::create(root.join("sessions"), &ws_dir).unwrap();
    for (turn, item) in compaction_transcript().into_iter().enumerate() {
        journal.append_item(turn, item).unwrap();
    }
    journal
        .sync_projected_transcript(&canonical, projector.evidence_links())
        .unwrap();
    let wal_path = journal.path().to_path_buf();
    drop(journal);

    let resumed = SessionJournal::open(&wal_path).unwrap();
    let mut restored = resumed.active_transcript();
    let mut resumed_projector = ContextProjector::new();
    resumed_projector.restore_pinned_baseline_end(projector.last_pinned_baseline_end());
    resumed.seed_projector_evidence(&mut resumed_projector);
    let resumed_projection = resumed_projector
        .project(&mut restored, &budget, 2048, None, 8)
        .unwrap();

    let resumed_breakdown = resumed_projector.stats().budget.clone();
    assert!(
        resumed_breakdown.compacted_stub_bytes > 0,
        "a resumed projection must still attribute the archived stubs: {:?}",
        resumed_breakdown
    );
    assert_eq!(
        resumed_breakdown.compacted_stub_bytes,
        fresh_breakdown.compacted_stub_bytes
    );
    assert_eq!(resumed_breakdown.evidence_link_count, 3);
    assert_eq!(
        resumed_breakdown.total_projected_bytes,
        resumed_projection
            .messages
            .iter()
            .map(|i| i.estimated_bytes())
            .sum::<usize>()
    );

    // Byte + estimated-token component reporting is present and internally consistent.
    assert_eq!(resumed_breakdown.system_prefix_bytes, 2048);
    assert_eq!(resumed_breakdown.tokens.system_prefix_tokens, 512);
    assert_eq!(
        resumed_breakdown.tokens.history_tokens,
        resumed_breakdown.history_bytes / 4
    );
    assert_eq!(
        resumed_breakdown.tokens.total_tokens,
        (resumed_breakdown.system_prefix_bytes
            + resumed_breakdown.tool_schema_bytes
            + resumed_breakdown.history_bytes)
            / 4
    );
    assert!(resumed_breakdown.tool_output_bytes > 0);
    assert!(resumed_breakdown.tool_output_bytes <= resumed_breakdown.history_bytes);
    let stats = resumed_projector.stats();
    assert_eq!(stats.tokens, resumed_breakdown.tokens);
    assert_eq!(stats.recall_hit_count, 0);
    assert_eq!(stats.recall_miss_count, 0);
    let _ = fs::remove_dir_all(&root);
}

fn register_raw(projector: &mut ContextProjector, id: &str, raw: &str) {
    projector.register_evidence(
        EvidenceLink {
            source_entry_id: id.to_string(),
            canonical_index: 1,
            call_id: format!("call_{}", id),
            tool_name: "read".to_string(),
            relative_path: Some("src/evidence.txt".to_string()),
            original_sha256: sha256_hex64(raw.as_bytes()),
            original_bytes: raw.len(),
            original_lines: raw.lines().count(),
            stub_bytes: 32,
            reason: CompactionReason::StepDownArchive,
        },
        Arc::from(raw),
    );
}

/// Parse the `Lines: <first>-<last> of <total>` field back out of a recall header.
fn recalled_line_range(content: &str) -> (usize, usize, usize) {
    let header_end = content.find(']').expect("recall header");
    let header = &content[..header_end];
    let marker = "Lines: ";
    let start = header.find(marker).expect("Lines field") + marker.len();
    let rest = &header[start..];
    let end = rest.find(" |").unwrap_or(rest.len());
    let field = &rest[..end];
    let numbers: Vec<usize> = field
        .split(|c: char| !c.is_ascii_digit())
        .filter(|s| !s.is_empty())
        .map(|s| s.parse().unwrap())
        .collect();
    assert_eq!(
        numbers.len(),
        3,
        "expected first-last-of-triple in '{}'",
        field
    );
    (numbers[0], numbers[1], numbers[2])
}

/// `recall_evidence` must handle multi-byte UTF-8 (CJK + emoji + Unicode case folding) and must
/// report the *actually rendered* last line when the 32 KiB inline cap clips the response.
#[test]
fn test_recall_evidence_utf8_patterns_and_byte_cap_line_clipping() {
    let mut projector = ContextProjector::new();

    // ---- multi-byte pattern matching ----
    let cjk = "第一行 中文内容 甲\n第二行 🎉🚀 emoji\n第三行 ΣΟΦΟΣ greek\n第四行 中文内容 乙\n";
    register_raw(&mut projector, "ev-0001-cjk", cjk);

    let hit = execute_recall_evidence(
        &projector,
        r#"{"source_entry_id":"ev-0001-cjk","pattern":"中文内容"}"#,
    );
    assert!(hit.outcome.is_success(), "{}", hit.content);
    assert!(hit.content.contains("中文内容 甲") && hit.content.contains("中文内容 乙"));
    assert!(hit.content.contains("🎉🚀"));
    assert!(
        hit.content.contains("    1\t第一行"),
        "1-based numbering: {}",
        hit.content
    );

    let emoji = execute_recall_evidence(
        &projector,
        r#"{"source_entry_id":"ev-0001-cjk","pattern":"🎉"}"#,
    );
    assert!(emoji.outcome.is_success(), "{}", emoji.content);
    assert!(emoji.content.contains("第二行"));

    // Unicode case folding: mixed-case Greek must fold to match the uppercase line. (`str::to_lowercase`
    // applies the context-sensitive final-sigma rule, so the pattern avoids a word-final sigma.)
    let greek = execute_recall_evidence(
        &projector,
        r#"{"source_entry_id":"ev-0001-cjk","pattern":"σΟφ"}"#,
    );
    assert!(greek.outcome.is_success(), "{}", greek.content);
    assert!(greek.content.contains("ΣΟΦΟΣ"));

    // `start_line`/`end_line` bound the pattern window (and its ±2 context lines).
    let windowed = execute_recall_evidence(
        &projector,
        r#"{"source_entry_id":"ev-0001-cjk","pattern":"中文内容","start_line":3,"end_line":4}"#,
    );
    assert!(windowed.outcome.is_success(), "{}", windowed.content);
    assert!(
        windowed.content.contains("中文内容 乙"),
        "{}",
        windowed.content
    );
    assert!(
        !windowed.content.contains("中文内容 甲"),
        "the requested window must exclude earlier matches: {}",
        windowed.content
    );
    assert_eq!(recalled_line_range(&windowed.content), (3, 4, 4));

    // ---- 32 KiB byte-cap clipping with ASCII lines ----
    let long_line = "x".repeat(300);
    let mut ascii_big = String::new();
    for line in 1..=200 {
        ascii_big.push_str(&format!("{} #{}\n", long_line, line));
    }
    register_raw(&mut projector, "ev-0002-long", &ascii_big);

    let clipped = execute_recall_evidence(
        &projector,
        r#"{"source_entry_id":"ev-0002-long","start_line":1,"end_line":200}"#,
    );
    assert!(clipped.outcome.is_success(), "{}", clipped.content);
    let (first, last, total) = recalled_line_range(&clipped.content);
    assert_eq!((first, total), (1, 200));
    assert!(
        last < 200,
        "the 32 KiB cap must clip the requested range: {}",
        clipped.content
    );
    assert_eq!(clipped.meta.lines_range, Some((first, last)));
    assert!(
        clipped
            .content
            .contains(&format!("start_line={} to continue", last + 1)),
        "the continuation hint must name the next *unrendered* line: {}",
        clipped.content
    );
    assert!(
        clipped
            .content
            .contains(&format!("{} #{}\n", long_line, last)),
        "the reported last line must actually be rendered"
    );
    assert!(
        !clipped
            .content
            .contains(&format!("{} #{}\n", long_line, last + 1)),
        "no line past the reported last line may be rendered"
    );

    // Following the continuation hint makes progress instead of re-serving the same window.
    let continued = execute_recall_evidence(
        &projector,
        &format!(
            r#"{{"source_entry_id":"ev-0002-long","start_line":{}}}"#,
            last + 1
        ),
    );
    assert!(continued.outcome.is_success(), "{}", continued.content);
    assert!(continued
        .content
        .contains(&format!("{} #{}\n", long_line, last + 1)));

    // ---- 32 KiB cap with 3-byte CJK lines: no char-boundary panic, footer preserved ----
    let mut cjk_big = String::new();
    for line in 1..=400 {
        cjk_big.push_str(&format!("{} #{}\n", "汉字测试内容".repeat(25), line));
    }
    register_raw(&mut projector, "ev-0003-cjkbig", &cjk_big);
    let cjk_clipped = execute_recall_evidence(
        &projector,
        r#"{"source_entry_id":"ev-0003-cjkbig","start_line":1,"end_line":200}"#,
    );
    assert!(cjk_clipped.outcome.is_success(), "{}", cjk_clipped.content);
    let (_, cjk_last, cjk_total) = recalled_line_range(&cjk_clipped.content);
    assert_eq!(cjk_total, 400);
    assert!(cjk_last < 200, "{}", cjk_clipped.content);
    assert!(
        cjk_clipped
            .content
            .contains(&format!("start_line={} to continue", cjk_last + 1)),
        "the footer must survive the byte cap for multi-byte content: {}",
        cjk_clipped.content
    );
    assert!(cjk_clipped.content.len() < 40 * 1024);

    // ---- A single line larger than the cap must still surface its prefix ----
    let one_huge = format!("{}\n", "z".repeat(40 * 1024));
    register_raw(&mut projector, "ev-0004-huge", &one_huge);
    let huge = execute_recall_evidence(&projector, r#"{"source_entry_id":"ev-0004-huge"}"#);
    assert!(huge.outcome.is_success(), "{}", huge.content);
    assert!(
        huge.content
            .contains("[recall truncated at the 32 KiB cap]"),
        "{}",
        huge.content
    );
    assert!(huge.content.len() < 40 * 1024, "len={}", huge.content.len());
}

// ---------------------------------------------------------------------------------------------
// Gate 2 A/B: the four projection profiles over the five blueprint task archetypes.
// ---------------------------------------------------------------------------------------------

fn eval_budget() -> BudgetConfig {
    let mut budget = BudgetConfig::default();
    budget.context_high_watermark_bytes = 16 * 1024;
    budget.context_low_watermark_bytes = 12 * 1024;
    budget.context_tail_window_bytes = 512;
    budget.context_min_tail_turns = 1;
    budget
}

/// A ~1.6 KiB read body, just above `MIN_COMPACTABLE_TOOL_BYTES`, so compaction granularity is fine
/// enough for the stricter `P3` target to be observably different from `P2`.
fn medium_source(tag: usize) -> String {
    let mut body = format!(
        "[File: src/med_{}.rs | Lines: 1-40 of 40 | SHA256: 00000000000000aa]\n",
        tag
    );
    body.push_str("     1\tpub fn medium() -> usize {\n");
    while body.len() < 1600 {
        body.push_str("     2\t    // padding\n");
    }
    body.push_str("    40\t}\n");
    body
}

/// Archetype 1 — long-file navigation: read/grep three large outputs, then work on.
fn archetype_long_file_nav() -> Vec<TranscriptItem> {
    let mut items = vec![TranscriptItem::User(UserMessage {
        content: "navigate a long file".into(),
    })];
    items.push(assistant_with(&[
        ("nav_read_a", "read"),
        ("nav_read_b", "read"),
        ("nav_grep", "grep"),
    ]));
    items.push(tool_result(
        "nav_read_a",
        "read",
        Some("src/a.rs"),
        big_rust_source(),
    ));
    items.push(tool_result(
        "nav_read_b",
        "read",
        Some("src/b.rs"),
        branch_big_source(HIDDEN_CONSTANT_V2),
    ));
    items.push(tool_result("nav_grep", "grep", None, big_grep_output()));
    for turn in 0..6 {
        push_read_turn(&mut items, "nav", turn);
    }
    items
}

/// Archetype 2 — cross-file bug fix: read three files (two large), edit the small one, then continue.
/// The edited file is deliberately the *small* one so its Working-Set Pinning exemption cannot hide a
/// large read from the A/B comparison.
fn archetype_cross_file_bug_fix() -> Vec<TranscriptItem> {
    let mut items = vec![TranscriptItem::User(UserMessage {
        content: "fix a cross-file bug".into(),
    })];
    items.push(assistant_with(&[
        ("x_read_a", "read"),
        ("x_read_b", "read"),
    ]));
    items.push(small_read("x_read_a", "src/a.rs"));
    items.push(tool_result(
        "x_read_b",
        "read",
        Some("src/b.rs"),
        big_rust_source(),
    ));
    items.push(assistant_with(&[("x_read_c", "read")]));
    items.push(tool_result(
        "x_read_c",
        "read",
        Some("src/c.rs"),
        branch_big_source(HIDDEN_CONSTANT_V2),
    ));
    items.push(assistant_with(&[("x_edit_a", "edit")]));
    items.push(TranscriptItem::ToolResult(ToolResultMessage {
        call_id: ToolCallId::new("x_edit_a"),
        tool_name: "edit".into(),
        outcome: ToolOutcome::Success,
        content: Arc::from("[Edited: src/a.rs | 1 hunk applied]"),
        meta: ToolResultMeta {
            relative_path: Some("src/a.rs".into()),
            modified_file: true,
            ..Default::default()
        },
    }));
    for turn in 0..6 {
        push_read_turn(&mut items, "x", turn);
    }
    items
}

/// Archetype 3 — repeated-error recovery: two failing `bash` runs plus one large read, interleaved
/// with an edit of a *different*, small file.
fn archetype_repeated_error_recovery() -> Vec<TranscriptItem> {
    fn failed_bash(call_id: &str, marker: &str) -> TranscriptItem {
        let mut body = String::from("[Command exit_code=101 | duration_ms=830]\n");
        for line in 1..=200 {
            body.push_str(&format!("{} error: unresolved name #{}\n", marker, line));
        }
        TranscriptItem::ToolResult(ToolResultMessage {
            call_id: ToolCallId::new(call_id),
            tool_name: "bash".into(),
            outcome: ToolOutcome::Error {
                code: ErrorCode::IoError,
            },
            content: Arc::from(body),
            meta: ToolResultMeta {
                exit_code: Some(101),
                ..Default::default()
            },
        })
    }
    let mut items = vec![TranscriptItem::User(UserMessage {
        content: "make the build pass".into(),
    })];
    items.push(assistant_with(&[("e_bash_1", "bash")]));
    items.push(failed_bash("e_bash_1", "first"));
    items.push(assistant_with(&[("e_read_1", "read")]));
    items.push(tool_result(
        "e_read_1",
        "read",
        Some("src/lib.rs"),
        big_rust_source(),
    ));
    items.push(assistant_with(&[("e_edit_1", "edit")]));
    items.push(TranscriptItem::ToolResult(ToolResultMessage {
        call_id: ToolCallId::new("e_edit_1"),
        tool_name: "edit".into(),
        outcome: ToolOutcome::Success,
        content: Arc::from("[Edited: src/main.rs | 1 hunk applied]"),
        meta: ToolResultMeta {
            relative_path: Some("src/main.rs".into()),
            modified_file: true,
            ..Default::default()
        },
    }));
    items.push(assistant_with(&[("e_bash_2", "bash")]));
    items.push(failed_bash("e_bash_2", "second"));
    for turn in 0..6 {
        push_read_turn(&mut items, "e", turn);
    }
    items
}

/// Archetype 4 — over budget: 16 medium reads (fine compaction granularity).
fn archetype_over_budget() -> Vec<TranscriptItem> {
    let mut items = vec![TranscriptItem::User(UserMessage {
        content: "summarise a large tree".into(),
    })];
    for turn in 0..8 {
        let first = format!("ob_{}_0", turn);
        let second = format!("ob_{}_1", turn);
        items.push(assistant_with(&[
            (first.as_str(), "read"),
            (second.as_str(), "read"),
        ]));
        items.push(tool_result(
            &first,
            "read",
            Some(&format!("src/ob_{}_0.rs", turn)),
            medium_source(turn * 2),
        ));
        items.push(tool_result(
            &second,
            "read",
            Some(&format!("src/ob_{}_1.rs", turn)),
            medium_source(turn * 2 + 1),
        ));
    }
    for turn in 0..6 {
        push_read_turn(&mut items, "obf", turn);
    }
    items
}

/// Archetype 5 — multi-turn: ten turns with four large reads spread across them.
fn archetype_multi_turn() -> Vec<TranscriptItem> {
    let mut items = vec![TranscriptItem::User(UserMessage {
        content: "ten turns of work".into(),
    })];
    for turn in 0..10 {
        let id = format!("mt_{}", turn);
        items.push(assistant_with(&[(id.as_str(), "read")]));
        let body = if turn % 3 == 0 {
            big_rust_source()
        } else {
            format!("     1\tpub fn mt_{}() -> usize {{ {} }}\n", turn, turn)
        };
        items.push(tool_result(
            &id,
            "read",
            Some(&format!("src/mt_{}.rs", turn)),
            body,
        ));
    }
    for turn in 0..6 {
        push_read_turn(&mut items, "mtf", turn);
    }
    items
}

/// Gate 2 exit criterion: the four projection profiles over the five blueprint task archetypes,
/// with the tool-call/result pairing invariant asserted at every compaction boundary. Also emits the
/// versioned machine-readable evaluation record at `docs/GATE2_EVAL_METRICS.json`.
#[test]
fn test_projection_profiles_ab_metrics_and_tool_pairing_invariant() {
    let budget = eval_budget();
    let profiles = [
        ProjectionProfile::Baseline,
        ProjectionProfile::P1,
        ProjectionProfile::P2,
        ProjectionProfile::P3,
    ];
    let archetypes: [(&str, fn() -> Vec<TranscriptItem>); 5] = [
        ("long_file_nav", archetype_long_file_nav),
        ("cross_file_bug_fix", archetype_cross_file_bug_fix),
        ("repeated_error_recovery", archetype_repeated_error_recovery),
        ("over_budget", archetype_over_budget),
        ("multi_turn", archetype_multi_turn),
    ];

    let mut pairing_checks = 0usize;
    let mut results: Vec<serde_json::Value> = Vec::new();
    let mut by_key: std::collections::BTreeMap<(String, String), (usize, usize)> =
        std::collections::BTreeMap::new();

    for (name, build) in archetypes {
        let canonical = build();
        for profile in profiles {
            let mut items = canonical.clone();
            let mut projector = ContextProjector::new().with_profile(profile);
            let projected = projector
                .project(&mut items, &budget, 2048, None, 8)
                .unwrap_or_else(|e| panic!("{} / {}: {:?}", name, profile.as_str(), e));

            // Hard constraint: 1:1 tool-call/result pairing must survive compaction.
            validate_tool_call_pairing(&projected.messages)
                .unwrap_or_else(|e| panic!("{} / {}: {}", name, profile.as_str(), e));
            assert!(verify_tool_call_pairing(&projected.messages));
            pairing_checks += 1;

            let stats = projector.stats();
            let breakdown = &stats.budget;
            assert_eq!(
                breakdown.total_projected_bytes,
                projected
                    .messages
                    .iter()
                    .map(|i| i.estimated_bytes())
                    .sum::<usize>(),
                "{} / {}: byte accounting must cover every projected item",
                name,
                profile.as_str()
            );
            assert_eq!(breakdown.history_bytes, breakdown.total_projected_bytes);
            assert_eq!(breakdown.tokens.history_tokens, breakdown.history_bytes / 4);
            assert_eq!(stats.tokens, breakdown.tokens);

            match profile {
                ProjectionProfile::Baseline => {
                    assert_eq!(
                        breakdown.compacted_stub_bytes, 0,
                        "{}: the baseline profile must not compact",
                        name
                    );
                    assert_eq!(breakdown.evidence_link_count, 0);
                }
                ProjectionProfile::P1 => {
                    assert_eq!(
                        breakdown.evidence_link_count, 0,
                        "{}: P1 keeps watermarks + pinning but no evidence graph",
                        name
                    );
                    assert!(
                        breakdown.compacted_stub_bytes > 0,
                        "{}: P1 must still compact an over-budget transcript: {:?}",
                        name,
                        breakdown
                    );
                }
                ProjectionProfile::P2 | ProjectionProfile::P3 => {
                    assert!(
                        breakdown.evidence_link_count > 0,
                        "{} / {}: the evidence graph must record the archived stubs: {:?}",
                        name,
                        profile.as_str(),
                        breakdown
                    );
                    assert!(breakdown.compacted_stub_bytes > 0);
                }
            }

            by_key.insert(
                (name.to_string(), profile.as_str().to_string()),
                (
                    breakdown.total_projected_bytes,
                    breakdown.evidence_link_count,
                ),
            );
            results.push(serde_json::json!({
                "archetype": name,
                "profile": profile.as_str(),
                "total_projected_bytes": breakdown.total_projected_bytes,
                "history_bytes": breakdown.history_bytes,
                "tool_output_bytes": breakdown.tool_output_bytes,
                "pinned_working_set_bytes": breakdown.pinned_working_set_bytes,
                "compacted_stub_bytes": breakdown.compacted_stub_bytes,
                "recent_tail_bytes": breakdown.recent_tail_bytes,
                "evidence_link_count": breakdown.evidence_link_count,
                "estimated_tokens": breakdown.tokens.total_tokens,
                "estimated_history_tokens": breakdown.tokens.history_tokens,
                "recall_hit_count": breakdown.recall_hit_count,
                "recall_miss_count": breakdown.recall_miss_count,
                "tool_call_pairing": "ok",
            }));
        }

        let key = |profile: &str| (name.to_string(), profile.to_string());
        let (p2_bytes, p2_links) = by_key[&key("p2")];
        let (p3_bytes, p3_links) = by_key[&key("p3")];
        assert!(
            p3_bytes <= p2_bytes,
            "{}: the stricter P3 target must never project more than P2 ({} vs {})",
            name,
            p3_bytes,
            p2_bytes
        );
        assert!(
            p3_links >= p2_links,
            "{}: P3 keeps P2's evidence graph and may archive more stubs under its stricter target ({} vs {})",
            name,
            p3_links,
            p2_links
        );
    }

    // The fine-grained over-budget archetype must show a strict P3 < P2 reduction.
    let ob = |profile: &str| by_key[&("over_budget".to_string(), profile.to_string())].0;
    assert!(
        ob("p3") < ob("p2"),
        "the P3 stricter target must measurably reduce the over-budget projection (p2={} p3={})",
        ob("p2"),
        ob("p3")
    );

    // The Working-Set Pinning bucket must be exercised by at least one archetype: the archetypes
    // that edit a small file right before the tail window keep that file's read + edit pinned.
    let pinned_probe = {
        let mut items = archetype_repeated_error_recovery();
        let mut projector = ContextProjector::new().with_profile(ProjectionProfile::P2);
        let _ = projector
            .project(&mut items, &budget, 2048, None, 8)
            .unwrap();
        projector.stats().budget.pinned_working_set_bytes
    };
    assert!(
        pinned_probe > 0,
        "the pinned working-set bucket must be non-zero for a recently edited file"
    );

    assert_eq!(pairing_checks, 20, "4 profiles x 5 archetypes");

    let artifact = serde_json::json!({
        "artifact": "gate2-context-projection-eval",
        "kroma_version": env!("CARGO_PKG_VERSION"),
        "generated_by": "crates/kroma-core/tests/gate2_evidence_projection.rs::test_projection_profiles_ab_metrics_and_tool_pairing_invariant",
        "blueprint": "docs/v1_blueprint/IMPLEMENTATION_GATES_AND_EVAL.md#gate-2--context-projection-ab",
        "profiles": profiles.iter().map(|p| p.as_str()).collect::<Vec<_>>(),
        "task_archetypes": archetypes.iter().map(|(name, _)| *name).collect::<Vec<_>>(),
        "hard_constraints": {
            "tool_call_result_pairing": "100% pass",
            "pairing_checks": pairing_checks,
            "tool_call_pairing_validator": "kroma_core::context_projector::validate_tool_call_pairing",
        },
        "results": results,
        "notes": [
            "estimated_tokens = bytes / 4 (byte-based estimate; provider-reported usage remains the cost authority)",
            "baseline = no compaction (A/B reference); p1 = watermarks + working-set pinning; p2 = p1 + source_entry_id evidence graph (default); p3 = p2 + 25% stricter low-watermark target (experimental, default off)"
        ],
    });
    let metrics_path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/GATE2_EVAL_METRICS.json");
    fs::write(
        &metrics_path,
        serde_json::to_string_pretty(&artifact).unwrap(),
    )
    .unwrap_or_else(|e| panic!("failed to write {}: {}", metrics_path.display(), e));
    eprintln!("[gate2-eval] wrote {}", metrics_path.display());
}

/// Gate 2 evidence-overlay selection must mirror `active_entries()`: when two projection overlays
/// exist for the same entry, the one recorded at the *deepest* leaf wins regardless of WAL append
/// order. WAL-order dedup would let a shallower (earlier) overlay shadow the deeper one after a
/// `--fork` re-selects the descendant leaf.
#[test]
fn test_evidence_links_select_the_deepest_overlay_like_active_entries() {
    fn link_for(id: &str, canonical_index: usize, call_id: &str) -> EvidenceLink {
        EvidenceLink {
            source_entry_id: id.to_string(),
            canonical_index,
            call_id: call_id.to_string(),
            tool_name: "read".to_string(),
            relative_path: Some("src/x.rs".to_string()),
            original_sha256: sha256_hex64(b"raw"),
            original_bytes: 4,
            original_lines: 1,
            stub_bytes: 32,
            reason: CompactionReason::StepDownArchive,
        }
    }
    fn stub(id: &str) -> TranscriptItem {
        TranscriptItem::ToolResult(ToolResultMessage {
            call_id: ToolCallId::new("call_x"),
            tool_name: "read".into(),
            outcome: ToolOutcome::Success,
            content: Arc::from(format!(
                "[Compacted: read(src/x.rs) | source_entry_id={} | call recall_evidence(source_entry_id={}) | Outline: none]",
                id, id
            )),
            meta: ToolResultMeta {
                relative_path: Some("src/x.rs".into()),
                ..Default::default()
            },
        })
    }

    let root = temp_root("overlay-depth");
    let ws_dir = root.join("ws");
    fs::create_dir_all(&ws_dir).unwrap();
    let mut journal = SessionJournal::create(root.join("sessions"), &ws_dir).unwrap();

    let a = journal
        .append_item(
            0,
            TranscriptItem::User(UserMessage {
                content: "a".into(),
            }),
        )
        .unwrap();
    let b = journal
        .append_item(
            0,
            tool_result("call_x", "read", Some("src/x.rs"), big_rust_source()),
        )
        .unwrap();

    // 1. A *shallow* overlay for `b`, recorded at `b` itself (earliest in WAL order).
    let mut shallow = journal.active_transcript();
    shallow[1] = stub("ev-shallow");
    journal
        .sync_projected_transcript(&shallow, &[link_for("ev-shallow", 1, "call_x")])
        .unwrap();

    // 2. Append `c`, then a *deeper* overlay for `b` recorded at `c`.
    let c = journal
        .append_item(
            0,
            TranscriptItem::User(UserMessage {
                content: "c".into(),
            }),
        )
        .unwrap();
    let mut deep = journal.active_transcript();
    deep[1] = stub("ev-deep");
    journal
        .sync_projected_transcript(&deep, &[link_for("ev-deep", 1, "call_x")])
        .unwrap();
    let wal_path = journal.path().to_path_buf();
    drop(journal);

    // Re-select the deepest leaf: both overlays are on the active path, and the deeper one must win.
    let mut reopened = SessionJournal::open(&wal_path).unwrap();
    reopened.fork_at(&c).unwrap();
    assert_eq!(reopened.active_entries().len(), 3);
    let links = reopened.evidence_links();
    assert_eq!(links.len(), 1, "one link per entry: {:?}", links);
    assert_eq!(
        links[0].source_entry_id, "ev-deep",
        "the deepest overlay must win regardless of WAL append order: {:?}",
        links
    );
    assert!(reopened.active_transcript().iter().any(
        |item| matches!(item, TranscriptItem::ToolResult(tr) if tr.content.contains("ev-deep"))
    ));

    // Re-selecting the shallower leaf exposes only the shallow overlay.
    reopened.fork_at(&b).unwrap();
    assert_eq!(reopened.active_entries().len(), 2);
    let shallow_links = reopened.evidence_links();
    assert_eq!(shallow_links.len(), 1, "{:?}", shallow_links);
    assert_eq!(shallow_links[0].source_entry_id, "ev-shallow");
    let _ = (a, b, root);
}
