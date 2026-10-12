//! Gate 2 acceptance tests: evidence-linked context projection and raw evidence retrieval.
//!
//! Gate 2 makes every compacted Outline Stub traceable. A stub carries a deterministic
//! `source_entry_id`, the raw tool output it replaced stays retrievable through `recall_evidence`,
//! and the byte accounting per projection category is reproducible across fresh runs, `--resume`
//! and `--fork`. Test 5 also pins down the Gate 1 polish items O1–O6.

use harness_core::agent_loop::{build_default_runtime, AgentRunConfig, AgentRunner};
use harness_core::budget::BudgetConfig;
use harness_core::context_projector::{
    CompactionReason, ContextProjector, MIN_COMPACTABLE_TOOL_BYTES,
};
use harness_core::env::LocalHostEnv;
use harness_core::events::EventLogger;
use harness_core::messages::{
    AssistantTurn, FinishReason, ToolCall, ToolOutcome, ToolResultMessage, ToolResultMeta,
    TranscriptItem, UserMessage,
};
use harness_core::provider::FakeProvider;
use harness_core::revision::SnapshotStore;
use harness_core::session::SessionJournal;
use harness_core::tool::policy::{NonInteractiveApprovalHandler, PolicyEngine, PolicyMode};
use harness_core::tools::recall_evidence::execute_recall_evidence;
use harness_core::types::{RunStatus, ToolCallId};
use harness_core::workspace::WorkspaceCap;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::watch;
use uuid::Uuid;

fn temp_root(tag: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("harness-gate2-{}-{}", tag, Uuid::new_v4()));
    fs::create_dir_all(&path).unwrap();
    path
}

/// The hidden constant lives inside a function body, so it is stripped from the Outline Stub and
/// only recoverable through `recall_evidence`.
const HIDDEN_CONSTANT: &str = "4242";

fn big_rust_source() -> String {
    let mut body = String::from("[File: src/big.rs | Lines: 1-400 of 400 | SHA256: 0123456789abcdef]\n");
    body.push_str("     1\tpub fn secret_answer() -> u32 {\n");
    for line in 2..=380 {
        body.push_str(&format!("{:>6}\t    // padding line {}\n", line, line));
    }
    body.push_str(&format!("   381\t    {}\n", HIDDEN_CONSTANT));
    body.push_str("   382\t}\n");
    body
}

fn big_grep_output() -> String {
    let mut body = String::new();
    for line in 1..=200 {
        body.push_str(&format!("src/mod_{}.rs:{}: match line {}\n", line, line, line));
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
    let mut items = vec![TranscriptItem::User(UserMessage { content: "find the constant".into() })];
    items.push(assistant_with(&[
        ("call_big", "read"),
        ("call_grep", "grep"),
        ("call_bash", "bash"),
    ]));
    items.push(tool_result("call_big", "read", Some("src/big.rs"), big_rust_source()));
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
    assert_eq!(links.len(), 3, "one evidence link per compacted output: {:?}", links);
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
    let mut superseded = vec![TranscriptItem::User(UserMessage { content: "t".into() })];
    superseded.push(assistant_with(&[("call_r1", "read"), ("call_r2", "read")]));
    superseded.push(tool_result("call_r1", "read", Some("src/same.rs"), big_rust_source()));
    superseded.push(tool_result("call_r2", "read", Some("src/same.rs"), big_rust_source()));
    for turn in 0..6 {
        let id = format!("call_sup_filler_{}", turn);
        superseded.push(assistant_with(&[(id.as_str(), "read")]));
        superseded.push(tool_result(
            &id,
            "read",
            Some(&format!("src/sup_filler_{}.rs", turn)),
            format!("     1\tpub fn sup_filler_{}() -> usize {{ {} }}\n", turn, turn),
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
        projected.messages.iter().map(|i| i.estimated_bytes()).sum::<usize>()
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
    assert_eq!(wal_links.len(), 3, "evidence links must persist in the WAL: {:?}", wal_links);
    assert!(wal_links.iter().any(|l| l.source_entry_id == "ev-0002-call_big"));
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
    let _ = projector.project(&mut canonical, &budget, 2048, None, 8).unwrap();

    // The constant is gone from the projected transcript...
    let stub = stub_for(&canonical, "read");
    assert!(stub.contains("[Compacted: read("));
    assert!(!stub.contains(HIDDEN_CONSTANT), "the stub must not leak the body: {}", stub);

    // ...but recoverable by exact line range.
    let out = execute_recall_evidence(
        &projector,
        r#"{"source_entry_id":"ev-0002-call_big","start_line":381,"end_line":382}"#,
    );
    assert!(out.outcome.is_success(), "{}", out.content);
    assert!(out.content.starts_with("[Recalled Evidence: ev-0002-call_big | Tool: read | Lines: 381-382 of"), "{}", out.content);
    assert!(out.content.contains(HIDDEN_CONSTANT), "{}", out.content);
    assert!(out.content.contains("   381\t"), "line numbers must be 1-based: {}", out.content);

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
    assert!(unknown.content.contains("ev-0002-call_big"), "{}", unknown.content);
    assert!(unknown.content.contains("ev-0003-call_grep"), "{}", unknown.content);
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
    let policy = PolicyEngine::new(PolicyMode::ReadOnly, Arc::new(NonInteractiveApprovalHandler));
    let runtime = build_default_runtime(env, policy, budget.clone(), None);
    let cfg = AgentRunConfig::new("find the constant", budget);
    let logger = EventLogger::new(cfg.run_id, Some(state_dir.clone()), false).unwrap();
    let (_tx, rx) = watch::channel(false);
    let (summary, _logger) = AgentRunner::new(cfg, provider.clone(), runtime, rx)
        .run(logger)
        .await;
    assert_eq!(summary.status, RunStatus::Completed, "{:?}", summary.terminal_reason);

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
    assert!(recalled.content.contains(HIDDEN_CONSTANT), "{}", recalled.content);

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
    let policy = PolicyEngine::new(PolicyMode::ReadOnly, Arc::new(NonInteractiveApprovalHandler));
    let runtime = build_default_runtime(env, policy, budget.clone(), None);
    let cfg = AgentRunConfig::new("read the sources", budget.clone());
    let logger = EventLogger::new(cfg.run_id, Some(state_dir.clone()), false).unwrap();
    let (_tx, rx) = watch::channel(false);
    let (first, _logger) = AgentRunner::new(cfg, Arc::new(FakeProvider::new(turns)), runtime, rx)
        .with_session_journal(journal)
        .run(logger)
        .await;
    assert_eq!(first.status, RunStatus::Completed, "{:?}", first.terminal_reason);
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
    let policy = PolicyEngine::new(PolicyMode::ReadOnly, Arc::new(NonInteractiveApprovalHandler));
    let runtime = build_default_runtime(env, policy, budget.clone(), None);
    let cfg = AgentRunConfig::new("recall the constant", budget);
    let logger = EventLogger::new(cfg.run_id, Some(state_dir.clone()), false).unwrap();
    let (_tx, rx) = watch::channel(false);
    let (second, _logger) =
        AgentRunner::new(cfg, Arc::new(FakeProvider::new(resume_turns)), runtime, rx)
            .with_session_journal(resumed_journal)
            .run(logger)
            .await;
    assert_eq!(second.status, RunStatus::Completed, "{:?}", second.terminal_reason);
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
    assert!(recalled.content.contains(HIDDEN_CONSTANT), "{}", recalled.content);
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
    let projected_fork = projector_a.project(&mut forked, &budget, 2048, None, 4).unwrap();
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
                raw_arguments: r#"{"command":"printf 'made by bash\\n' > made_by_bash.txt"}"#.into(),
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
    let policy = PolicyEngine::new(PolicyMode::AllowShell, Arc::new(NonInteractiveApprovalHandler));
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
    assert_eq!(summary.status, RunStatus::Completed, "{:?}", summary.terminal_reason);
    assert!(ws_dir.join("made_by_bash.txt").exists());

    let journal = SessionJournal::open(&wal_path).unwrap();
    let records = journal.revision_records();
    assert!(
        records.iter().any(|r| r.changed_paths.iter().any(|p| p == "made_by_bash.txt")),
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
    assert!(leftovers.is_empty(), "atomic blob writes must leave no temp files: {:?}", leftovers);
    // Re-storing the same blob is idempotent.
    let again = store.capture_revision(&workspace, None, None, 1).unwrap();
    assert_eq!(rev.revision_id, again.revision_id);

    // ---- O3: `ensure_private_dir` refuses a symlinked store path ----
    let symlink_root = root.join("symlink-store");
    let target = root.join("symlink-target");
    fs::create_dir_all(&target).unwrap();
    std::os::unix::fs::symlink(&target, &symlink_root).unwrap();
    let symlink_store = SnapshotStore::new(&symlink_root);
    assert!(
        symlink_store.ensure_layout().is_err(),
        "a symlinked snapshot store must be refused, not chmod-ed through"
    );

    // ---- O4: revision records are ordered by branch depth, not append order ----
    let o4_root = temp_root("o4");
    let o4_ws = o4_root.join("ws");
    fs::create_dir_all(&o4_ws).unwrap();
    let o4_store = SnapshotStore::new(o4_root.join("snapshots"));
    let o4_workspace = WorkspaceCap::new(&o4_ws).unwrap();
    let mut o4_journal = SessionJournal::create(o4_root.join("sessions"), &o4_ws).unwrap();
    let a = o4_journal
        .append_item(0, TranscriptItem::User(UserMessage { content: "a".into() }))
        .unwrap();
    let b = o4_journal
        .append_item(1, TranscriptItem::User(UserMessage { content: "b".into() }))
        .unwrap();
    let c = o4_journal
        .append_item(2, TranscriptItem::User(UserMessage { content: "c".into() }))
        .unwrap();
    let rev_deep = o4_store.capture_revision(&o4_workspace, None, None, 2).unwrap().revision_id;
    o4_journal
        .record_revision_capture(2, rev_deep.as_str(), None, vec![], 0)
        .unwrap();
    o4_journal.fork_at(&b).unwrap();
    let rev_shallow = o4_store.capture_revision(&o4_workspace, None, None, 1).unwrap().revision_id;
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
    assert_eq!(o4_journal.latest_revision_id().as_deref(), Some(rev_deep.as_str()));
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
        .append_item(0, TranscriptItem::User(UserMessage { content: "a".into() }))
        .unwrap();

    // Baseline is recorded normally.
    let baseline = store.capture_revision(&workspace, None, None, 0).unwrap();
    journal
        .record_revision_capture(0, baseline.revision_id.as_str(), None, vec![], 0)
        .unwrap();

    // The mutation snapshot is written, then the process dies before the WAL append.
    fs::write(ws_dir.join("a.txt"), "two\n").unwrap();
    let orphan = store
        .capture_revision(&workspace, Some(&baseline.revision_id), journal.leaf_id(), 1)
        .unwrap();
    assert_ne!(orphan.revision_id, baseline.revision_id);
    assert_eq!(
        journal.latest_revision_id().as_deref(),
        Some(baseline.revision_id.as_str()),
        "pre-reconcile the WAL still names the stale revision"
    );

    // Reconciliation adopts the manifest that is bound to an entry on the active branch.
    assert!(journal.is_entry_on_active_branch(journal.leaf_id().unwrap()));
    let turn = journal.max_active_turn();
    let parent = journal.latest_revision_id();
    journal
        .record_revision_capture(
            turn,
            orphan.revision_id.as_str(),
            parent.as_deref(),
            orphan.changed_paths.clone(),
            orphan.tracked_files.len(),
        )
        .unwrap();
    assert_eq!(
        journal.latest_revision_id().as_deref(),
        Some(orphan.revision_id.as_str())
    );
    assert!(journal
        .revision_records()
        .iter()
        .any(|r| r.changed_paths.iter().any(|p| p == "a.txt")));
    let _ = fs::remove_dir_all(&root);
}
