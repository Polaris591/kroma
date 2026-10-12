//! Gate 4 acceptance tests: predictive post-mutation validation and candidate failure attribution.
//!
//! Each test pins one invariant that makes Gate 4 safe to ship:
//!
//! 1. The structural checker catches JSON/TOML/delimiter/conflict-marker breakage with exact
//!    1-based line/column coordinates, accepts valid files whose braces live inside strings and
//!    comments, and never rolls anything back (zero false-positive rollbacks).
//! 2. A successful `edit`/`write` attaches the advisory `<predictive_validation>` block and records
//!    a `MutationRecord` in the WAL checkpoint, linked to the Gate 1 revisions.
//! 3. A failing `WorkspaceCompletionGate` produces a ranked *candidate* attribution block that
//!    points at the exact mutation, its `source_entry_id` and its pre-change revision, emits
//!    `EventPayload::FailureAttributed`, and lets the model fix the right file on retry.
//! 4. Two edits in different files are disambiguated by file/line/symbol.
//! 5. A `--fork` rewind prunes mutations that only exist on the abandoned branch.
//! 6. The versioned evaluation corpus at `docs/GATE4_EVAL_METRICS.json` is regenerated.

use harness_core::agent_loop::{build_default_runtime, AgentRunConfig, AgentRunner};
use harness_core::attribution::{
    added_identifiers, language_name, parse_diagnostic_anchors, top_level_symbol_names,
    AttributionEngine, MutationLedger, MutatedFileSpan, PredictiveValidator,
    CANDIDATE_ATTRIBUTION_FRAMING,
};
use harness_core::budget::BudgetConfig;
use harness_core::env::{
    CompletionGate, CompletionGateContext, GateOutcome, LocalHostEnv,
};
use harness_core::events::{EventLogger, EventPayload};
use harness_core::messages::{
    AssistantTurn, FinishReason, ToolCall, TranscriptItem, UserMessage,
};
use harness_core::provider::FakeProvider;
use harness_core::revision::{RevisionId, SnapshotStore};
use harness_core::session::{FileTouch, SessionJournal};
use harness_core::tool::policy::{NonInteractiveApprovalHandler, PolicyEngine, PolicyMode};
use harness_core::types::{RunStatus, ToolCallId};
use harness_core::workspace::WorkspaceCap;
use std::fs;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::watch;
use uuid::Uuid;

fn temp_root(tag: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("harness-gate4-{}-{}", tag, Uuid::new_v4()));
    fs::create_dir_all(&path).unwrap();
    path
}

fn tool_turn(call_id: &str, name: &str, args: &str) -> AssistantTurn {
    AssistantTurn {
        content: None,
        reasoning_blocks: Vec::new(),
        tool_calls: vec![ToolCall {
            call_id: ToolCallId::new(call_id),
            name: name.to_string(),
            raw_arguments: args.to_string(),
        }],
        finish_reason: FinishReason::ToolCalls,
        usage: None,
    }
}

fn stop_turn(text: &str) -> AssistantTurn {
    AssistantTurn {
        content: Some(text.to_string()),
        reasoning_blocks: Vec::new(),
        tool_calls: Vec::new(),
        finish_reason: FinishReason::Stop,
        usage: None,
    }
}

/// A gate that rejects the first check with `feedback` and approves every later one.
struct RejectOnceGate {
    calls: AtomicUsize,
    feedback: String,
}

impl CompletionGate for RejectOnceGate {
    fn check<'a>(
        &'a self,
        _ctx: &'a CompletionGateContext,
    ) -> Pin<Box<dyn Future<Output = Result<GateOutcome, String>> + Send + 'a>> {
        Box::pin(async move {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                Ok(GateOutcome::Rejected {
                    feedback: self.feedback.clone(),
                })
            } else {
                Ok(GateOutcome::Approved)
            }
        })
    }
}

fn payloads(logger: &EventLogger) -> Vec<EventPayload> {
    logger.events().iter().map(|e| e.event.clone()).collect()
}

// ---------------------------------------------------------------------------------------------
// Test 1 — the structural checker
// ---------------------------------------------------------------------------------------------

#[test]
fn test_predictive_validator_catches_syntax_and_delimiter_errors_with_zero_false_rollbacks() {
    let validator = PredictiveValidator::new(true);

    // ---- JSON: exact line/column on a syntax break ----
    let broken_json = "{\n  \"a\": 1,\n  \"b\": [1, 2,],\n}\n";
    let report = validator.validate_content("config.json", broken_json);
    assert_eq!(report.status, "warning");
    assert_eq!(report.language, "json");
    assert_eq!(report.issue_count(), 1);
    assert_eq!(report.issues[0].kind, "json_syntax");
    assert!(report.issues[0].line >= 1 && report.issues[0].column >= 1);
    let block = report.render_block().expect("a warning block");
    assert!(block.starts_with("<predictive_validation status=\"warning\""));
    assert!(block.contains("advisory only"));
    assert!(block.ends_with("</predictive_validation>"));

    // ---- TOML: section header structure + unbalanced string ----
    assert!(validator
        .validate_content("Cargo.toml", "[package]\nname = \"x\"\n")
        .issues
        .is_empty());
    let toml_report = validator.validate_content("Cargo.toml", "[package\nname = \"x\n");
    assert!(toml_report.issues.len() >= 1);
    assert!(toml_report
        .issues
        .iter()
        .any(|issue| issue.kind == "toml_structure"));

    // ---- Rust: unclosed delimiter with the opener's coordinates ----
    let rust = "pub fn f() {\n    let v = vec![1, 2;\n}\n";
    let report = validator.validate_content("src/lib.rs", rust);
    assert!(report.issues.iter().any(|i| i.kind == "unclosed_delimiter"));
    assert!(report
        .issues
        .iter()
        .any(|i| i.line == 2 && i.kind == "unclosed_delimiter"));

    // ---- TypeScript: unclosed brace + unterminated string ----
    let ts = "export function f() {\n  const s = \"abc\n";
    let report = validator.validate_content("src/app.ts", ts);
    assert!(report.issues.iter().any(|i| i.kind == "unclosed_string"));
    assert!(report.issues.iter().any(|i| i.kind == "unclosed_delimiter"));

    // ---- Conflict markers ----
    let conflicted = "fn f() {}\n<<<<<<< HEAD\nlet a = 1;\n=======\nlet a = 2;\n>>>>>>> other\n";
    let report = validator.validate_content("src/lib.rs", conflicted);
    assert_eq!(
        report
            .issues
            .iter()
            .filter(|i| i.kind == "conflict_marker")
            .count(),
        3
    );

    // ---- Clean files whose braces live inside strings / comments / raw strings ----
    let clean_rust = "pub fn f() {\n    let s = \"} { ) ]\";\n    let r = r#\"{ \"x\" }\"#;\n    let c = '}';\n    // } } }\n    /* } { */\n    let _ = 'a';\n}\n";
    assert!(
        validator.validate_content("src/lib.rs", clean_rust).issues.is_empty(),
        "valid Rust must never warn: {:?}",
        validator.validate_content("src/lib.rs", clean_rust).issues
    );
    let clean_js = "const s = `{ \"a\": [1, 2] }`;\nconst t = ')';\nfunction f() { return `${s} ${t}`; }\n";
    assert!(validator
        .validate_content("src/app.js", clean_js)
        .issues
        .is_empty());
    let clean_py = "def f():\n    s = \"\"\"{\n    }\n    \"\"\"\n    return s  # } {\n";
    assert!(validator
        .validate_content("app.py", clean_py)
        .issues
        .is_empty());
    // Rust lifetimes are not char literals.
    assert!(validator
        .validate_content("src/lib.rs", "struct S<'a> { r: &'a str }\n")
        .issues
        .is_empty());
    // A Rust string literal may span physical lines with or without a `\` continuation, so a raw
    // newline inside one is data. Getting this wrong was a real false positive on `prompt.rs`.
    let multi_line_rust = r#"pub const S: &str = "\
line one
line two
line three";
"#;
    assert!(
        validator
            .validate_content("src/lib.rs", multi_line_rust)
            .issues
            .is_empty(),
        "a multi-line Rust string literal must not warn: {:?}",
        validator.validate_content("src/lib.rs", multi_line_rust).issues
    );

    // ---- Zero false-positive rollbacks: a validation run never mutates the file ----
    let root = temp_root("no-rollback");
    let ws = WorkspaceCap::new(&root).unwrap();
    fs::write(root.join("tricky.rs"), clean_rust).unwrap();
    let before = fs::read(root.join("tricky.rs")).unwrap();
    let report = validator.validate_file(&ws, "tricky.rs");
    assert!(report.issues.is_empty(), "{:?}", report.issues);
    assert_eq!(
        fs::read(root.join("tricky.rs")).unwrap(),
        before,
        "the predictive check must never rewrite the mutated file"
    );
    // An unsupported extension is reported as skipped, not as an error.
    let skipped = validator.validate_file(&ws, "notes.txt");
    assert_eq!(skipped.status, "skipped");
    assert!(skipped.skipped_reason.is_some());
    // `--no-predictive-check` short-circuits everything.
    let disabled = PredictiveValidator::new(false);
    assert_eq!(disabled.validate_content("src/lib.rs", rust).status, "skipped");

    let _ = fs::remove_dir_all(&root);
}

/// Regression guard for the zero-false-positive guarantee: the harness's own production sources
/// must never trip the structural checker. These files compile, so a reported issue can only be a
/// checker bug — this test is what caught the Rust multi-line string literal false positive.
#[test]
fn test_predictive_validator_never_false_positives_on_harness_sources() {
    let validator = PredictiveValidator::new(true);
    let mut stack = vec![PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src")];
    let mut checked = 0usize;
    let mut failures = Vec::new();
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            let content = fs::read_to_string(&path).unwrap();
            let report = validator.validate_content(&path.to_string_lossy(), &content);
            checked += 1;
            if !report.issues.is_empty() {
                failures.push(format!("{}: {:?}", path.display(), report.issues));
            }
        }
    }
    assert!(
        checked >= 15,
        "expected to check the whole harness-core source tree, only saw {}",
        checked
    );
    assert!(
        failures.is_empty(),
        "the structural checker reported false positives on compiling sources:\n{}",
        failures.join("\n")
    );
}

// ---------------------------------------------------------------------------------------------
// Test 2 — edit/write attach the warning and record the ledger with revisions
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn test_edit_and_write_attach_predictive_warning_and_record_mutation_ledger_with_revisions() {
    let root = temp_root("ledger");
    let ws_dir = root.join("ws");
    let state_dir = root.join("state");
    fs::create_dir_all(&ws_dir).unwrap();
    fs::create_dir_all(&state_dir).unwrap();
    fs::write(ws_dir.join("src_a.rs"), "pub fn a() -> u32 {\n    1\n}\n").unwrap();

    let store = Arc::new(SnapshotStore::new(root.join("snapshots")));
    let journal = SessionJournal::create(state_dir.join("sessions"), &ws_dir).unwrap();
    let wal_path = journal.path().to_path_buf();

    let provider = Arc::new(FakeProvider::new(vec![
        // Turn 1: a clean `write` to a new file.
        tool_turn(
            "call_clean",
            "write",
            r#"{"path":"src_b.rs","content":"pub fn b() -> u32 {\n    2\n}\n"}"#,
        ),
        // Turn 2: a `write` that leaves an unclosed brace.
        tool_turn(
            "call_broken",
            "write",
            r#"{"path":"src_c.rs","content":"pub fn c() -> u32 {\n    3\n"}"#,
        ),
        stop_turn("done"),
    ]));
    let env = Arc::new(LocalHostEnv::new(&ws_dir, state_dir.clone()).unwrap());
    let policy = PolicyEngine::new(PolicyMode::AllowWrite, Arc::new(NonInteractiveApprovalHandler));
    let budget = BudgetConfig::default();
    let runtime = build_default_runtime(env, policy, budget.clone(), None);
    let cfg = AgentRunConfig::new("add two files", budget);
    let logger = EventLogger::new(cfg.run_id, Some(state_dir.clone()), false).unwrap();
    let (_tx, rx) = watch::channel(false);
    let (summary, logger) = AgentRunner::new(cfg, provider, runtime, rx)
        .with_session_journal(journal)
        .with_snapshot_store(store)
        .run(logger)
        .await;
    assert_eq!(summary.status, RunStatus::Completed, "{:?}", summary.terminal_reason);

    // The clean write carries no warning; the broken write carries the advisory block.
    let clean = summary
        .transcript
        .iter()
        .find_map(|item| match item {
            TranscriptItem::ToolResult(tr) if tr.call_id.as_str() == "call_clean" => Some(tr),
            _ => None,
        })
        .expect("the clean write result");
    assert!(
        !clean.content.contains("<predictive_validation"),
        "a clean file must not produce a warning block: {}",
        clean.content
    );
    let broken = summary
        .transcript
        .iter()
        .find_map(|item| match item {
            TranscriptItem::ToolResult(tr) if tr.call_id.as_str() == "call_broken" => Some(tr),
            _ => None,
        })
        .expect("the broken write result");
    assert!(
        broken.content.contains("<predictive_validation status=\"warning\""),
        "{}",
        broken.content
    );
    assert!(broken.content.contains("unclosed_delimiter"));

    // The mutation telemetry is attached to the tool metadata.
    let telemetry = broken.meta.mutation.clone().expect("mutation telemetry");
    assert_eq!(telemetry.spans.len(), 1);
    assert_eq!(telemetry.spans[0].relative_path, "src_c.rs");
    assert!(telemetry.spans[0].start_line >= 1);
    assert_eq!(
        telemetry.predictive.as_ref().map(|p| p.status.as_str()),
        Some("warning")
    );

    // The `PredictiveCheck` event is emitted as telemetry.
    let events = payloads(&logger);
    assert!(
        events.iter().any(|event| matches!(
            event,
            EventPayload::PredictiveCheck { status, .. } if status == "warning"
        )),
        "a warning must be logged"
    );

    // The ledger is persisted in the branch checkpoint, linked to the Gate 1 revisions.
    let journal = SessionJournal::open(&wal_path).unwrap();
    let checkpoint = journal
        .latest_active_checkpoint()
        .expect("the post-batch checkpoint");
    assert_eq!(checkpoint.mutations.len(), 2, "{:?}", checkpoint.mutations);
    let first = &checkpoint.mutations[0];
    let second = &checkpoint.mutations[1];
    assert_eq!(first.mutation_id, "mut-0001");
    assert_eq!(first.tool_name, "write");
    assert_eq!(first.call_id, "call_clean");
    assert_eq!(second.mutation_id, "mut-0002");
    assert_eq!(second.touched_ranges[0].relative_path, "src_c.rs");
    for record in &checkpoint.mutations {
        assert!(
            record.revision_before.is_some(),
            "every mutation must name its pre-change revision: {:?}",
            record
        );
        assert!(
            record.revision_after.is_some(),
            "the post-batch revision must be backfilled: {:?}",
            record
        );
        assert!(
            record
                .source_entry_id
                .as_deref()
                .is_some_and(|id| id.starts_with("ev-")),
            "every mutation must carry a Gate 2 evidence pointer: {:?}",
            record
        );
    }
    // The pre-change revision must actually exist in the snapshot store.
    let store = SnapshotStore::new(root.join("snapshots"));
    let before = first.revision_before.clone().unwrap();
    store
        .load_revision(&before)
        .expect("the pre-change revision must be restorable");
    // And `revision_after` is the revision the failing state was captured at.
    let after = first.revision_after.clone().unwrap();
    store.load_revision(&after).expect("the post-change revision");
    assert_ne!(before, after, "the mutation changed the workspace");

    let _ = fs::remove_dir_all(&root);
}

// ---------------------------------------------------------------------------------------------
// Test 3 — gate failure emits ranked attribution linked to evidence and revisions
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn test_completion_gate_failure_emits_ranked_candidate_attribution_linked_to_evidence_and_revisions()
{
    let root = temp_root("attribution");
    let ws_dir = root.join("ws");
    let state_dir = root.join("state");
    fs::create_dir_all(ws_dir.join("src")).unwrap();
    fs::create_dir_all(&state_dir).unwrap();
    let a_source = "pub fn a() -> u32 {\n    1\n}\n";
    let b_lines = [
        "pub fn b() -> u32 {",
        "    let x = 1;",
        "    let y = 2;",
        "    let z = 3;",
        "    let w = 4;",
        "    let v = 5;",
        "    let u = 6;",
        "    let t = 7;",
        "    let s = 8;",
        "    let r = 9;",
        "    let q = 10;",
        "    let p = 11;",
        "    x + y",
        "}",
    ];
    let b_source = format!("{}\n", b_lines.join("\n"));
    fs::write(ws_dir.join("src/a.rs"), a_source).unwrap();
    fs::write(ws_dir.join("src/b.rs"), &b_source).unwrap();

    let store = Arc::new(SnapshotStore::new(root.join("snapshots")));
    let journal = SessionJournal::create(state_dir.join("sessions"), &ws_dir).unwrap();
    let wal_path = journal.path().to_path_buf();

    let provider = Arc::new(FakeProvider::new(vec![
        // Turn 1: a clean edit to src/a.rs.
        tool_turn(
            "call_a",
            "edit",
            r#"{"path":"src/a.rs","edits":[{"old_text":"    1","new_text":"    42"}]}"#,
        ),
        // Turn 2: the edit that introduces the compile error at src/b.rs:12.
        tool_turn(
            "call_b_break",
            "edit",
            r#"{"path":"src/b.rs","edits":[{"old_text":"    let p = 11;","new_text":"    let p = broken_symbol;"}]}"#,
        ),
        stop_turn("finished"),
        // Turn 4: the model fixes the file the attribution pointed at.
        tool_turn(
            "call_b_fix",
            "edit",
            r#"{"path":"src/b.rs","edits":[{"old_text":"    let p = broken_symbol;","new_text":"    let p = 11;"}]}"#,
        ),
        stop_turn("fixed"),
    ]));
    let env = Arc::new(LocalHostEnv::new(&ws_dir, state_dir.clone()).unwrap());
    let policy = PolicyEngine::new(PolicyMode::AllowWrite, Arc::new(NonInteractiveApprovalHandler));
    let budget = BudgetConfig::default();
    let runtime = build_default_runtime(env, policy, budget.clone(), None);
    let cfg = AgentRunConfig::new("make b compile", budget);
    let logger = EventLogger::new(cfg.run_id, Some(state_dir.clone()), false).unwrap();
    let gate: Arc<dyn CompletionGate> = Arc::new(RejectOnceGate {
        calls: AtomicUsize::new(0),
        feedback: "verification command `cargo check` exited with exit status: 101.\nChanged files: src/a.rs, src/b.rs\nSTDERR:\nerror[E0425]: cannot find value `broken_symbol` in this scope\n  --> src/b.rs:12:13\n".to_string(),
    });
    let (_tx, rx) = watch::channel(false);
    let (summary, logger) = AgentRunner::new(cfg, provider, runtime, rx)
        .with_session_journal(journal)
        .with_snapshot_store(store)
        .with_completion_gate(gate)
        .run(logger)
        .await;
    assert_eq!(summary.status, RunStatus::Completed, "{:?}", summary.terminal_reason);

    // The gate rejection prompt carries the candidate attribution block.
    let gate_message = summary
        .transcript
        .iter()
        .find_map(|item| match item {
            TranscriptItem::User(user) if user.content.contains("CompletionGate Rejected") => {
                Some(user.content.clone())
            }
            _ => None,
        })
        .expect("the gate rejection prompt");
    assert!(
        gate_message.contains("candidate_failure_attribution"),
        "{}",
        gate_message
    );
    assert!(gate_message.contains("candidate_attribution"), "{}", gate_message);
    assert!(gate_message.contains("CANDIDATES"), "{}", gate_message);
    // Top-1 is the turn-2 mutation on src/b.rs, with its evidence pointer and revisions.
    assert!(gate_message.contains("mut-0002"), "{}", gate_message);
    assert!(gate_message.contains("src/b.rs"), "{}", gate_message);
    assert!(gate_message.contains("ev-"), "{}", gate_message);
    assert!(
        gate_message.contains("recall_evidence(source_entry_id=ev-"),
        "{}",
        gate_message
    );
    assert!(gate_message.contains("--restore-rev r-"), "{}", gate_message);
    // The clean src/a.rs edit must not outrank the real cause.
    let b_position = gate_message.find("mut-0002").unwrap();
    let a_position = gate_message.find("mut-0001");
    assert!(
        a_position.is_none() || a_position.unwrap() > b_position,
        "the clean edit must not be ranked above the breaking edit:\n{}",
        gate_message
    );

    // Machine-readable telemetry.
    let events = payloads(&logger);
    let attributed = events
        .iter()
        .find_map(|event| match event {
            EventPayload::FailureAttributed {
                source,
                candidate_count,
                top_mutation_id,
                top_path,
                top_confidence,
                top_revision_before,
            } => Some((
                source.clone(),
                *candidate_count,
                top_mutation_id.clone(),
                top_path.clone(),
                *top_confidence,
                top_revision_before.clone(),
            )),
            _ => None,
        })
        .expect("a FailureAttributed event");
    assert_eq!(attributed.0, "completion_gate");
    assert!(attributed.1 >= 1);
    assert_eq!(attributed.2.as_deref(), Some("mut-0002"));
    assert_eq!(attributed.3.as_deref(), Some("src/b.rs"));
    assert!(attributed.4.unwrap() >= 0.85, "{:?}", attributed.4);
    let top_revision = attributed.5.expect("a pre-change revision");
    RevisionId::parse(&top_revision).expect("a well-formed revision id");
    SnapshotStore::new(root.join("snapshots"))
        .load_revision(&RevisionId::parse(&top_revision).unwrap())
        .expect("the named pre-change revision must be restorable");

    // The model actually repaired the attributed file, and the gate then approved.
    let repaired = fs::read_to_string(ws_dir.join("src/b.rs")).unwrap();
    assert!(repaired.contains("let p = 11;"), "{}", repaired);
    assert!(!repaired.contains("broken_symbol"), "{}", repaired);

    // The persisted ledger still links the mutation to its evidence pointer.
    let journal = SessionJournal::open(&wal_path).unwrap();
    let checkpoint = journal.latest_active_checkpoint().expect("a checkpoint");
    let breaking = checkpoint
        .mutations
        .iter()
        .find(|record| record.call_id == "call_b_break")
        .expect("the breaking mutation");
    assert!(breaking
        .source_entry_id
        .as_deref()
        .is_some_and(|id| id.starts_with("ev-")));

    let _ = fs::remove_dir_all(&root);
}

// ---------------------------------------------------------------------------------------------
// Test 4 — multi-edit disambiguation
// ---------------------------------------------------------------------------------------------

#[test]
fn test_attribution_disambiguates_multiple_edits_by_file_line_and_symbol() {
    fn span(path: &str, start: usize, end: usize, symbols: &[&str]) -> MutatedFileSpan {
        MutatedFileSpan {
            relative_path: path.to_string(),
            start_line: start,
            end_line: end,
            added_symbols: symbols.iter().map(|s| s.to_string()).collect(),
            summary: format!("edit {} lines {}-{}", path, start, end),
        }
    }

    let mut ledger = MutationLedger::new();
    ledger.record_mutation(1, "edit", "call_a", None, None, vec![span("src/a.rs", 1, 3, &[])]);
    ledger.record_mutation(
        1,
        "edit",
        "call_b",
        None,
        None,
        vec![span("src/b.rs", 10, 14, &["compute"])],
    );

    // Same turn, two files: the line range alone must pick src/b.rs.
    let report = AttributionEngine::attribute_failure(
        &ledger,
        "error[E0308]: mismatched types\n  --> src/b.rs:12:9\n",
        "completion_gate",
        3,
    );
    assert_eq!(report.framing, CANDIDATE_ATTRIBUTION_FRAMING);
    assert_eq!(report.candidates[0].mutation_id, "mut-0002");
    assert!(report.candidates[0].confidence >= 0.85);

    // A distant line with a matching symbol still selects src/b.rs (symbol band).
    let report = AttributionEngine::attribute_failure(
        &ledger,
        "error[E0425]: cannot find function `compute` in this scope\n  --> src/b.rs:900:3\n",
        "completion_gate",
        3,
    );
    assert_eq!(report.candidates[0].mutation_id, "mut-0002");
    assert!(
        (0.70..=0.85).contains(&report.candidates[0].confidence),
        "{:?}",
        report.candidates[0].confidence
    );
    assert!(report.candidates[0].rationale.contains("compute"));

    // A diagnostic in a file nobody edited must not fabricate a file-scoped candidate.
    let report = AttributionEngine::attribute_failure(
        &ledger,
        "src/vendor/legacy.c:40:5: error: implicit declaration of function 'legacy_init'\n",
        "completion_gate",
        3,
    );
    assert!(!report.anchors.is_empty());
    assert!(
        report.candidates.iter().all(|c| c.confidence <= 0.45),
        "an unrelated failure must stay in the recency band: {:?}",
        report.candidates
    );

    // Symbol hints come from the edit text, not from a full-file rescan.
    assert_eq!(
        added_identifiers("let a = old_name;", "let a = new_name;", 8),
        vec!["new_name".to_string()]
    );
    assert!(top_level_symbol_names("pub fn helper() {}\nstruct Thing;\n", 8)
        .contains(&"helper".to_string()));
    assert_eq!(language_name("src/x.tsx"), "typescript");
    assert_eq!(language_name("src/x.unknown"), "unknown");
}

// ---------------------------------------------------------------------------------------------
// Test 5 — fork pruning
// ---------------------------------------------------------------------------------------------

#[test]
fn test_fork_rewind_prunes_future_mutations_from_ledger() {
    fn record(turn: u32, call_id: &str) -> harness_core::attribution::MutationRecord {
        harness_core::attribution::MutationRecord {
            mutation_id: format!("mut-{:04}", turn),
            turn,
            tool_name: "edit".to_string(),
            call_id: call_id.to_string(),
            source_entry_id: None,
            revision_before: None,
            revision_after: None,
            touched_ranges: vec![MutatedFileSpan {
                relative_path: format!("src/{}.rs", call_id),
                start_line: 1,
                end_line: 2,
                added_symbols: Vec::new(),
                summary: String::new(),
            }],
        }
    }

    // ---- Pure ledger pruning ----
    let mut ledger = MutationLedger::new();
    ledger.record_mutation(1, "edit", "c1", None, None, Vec::new());
    ledger.record_mutation(2, "edit", "c2", None, None, Vec::new());
    ledger.record_mutation(3, "edit", "c3", None, None, Vec::new());
    ledger.prune_after_turn(2);
    assert_eq!(
        ledger.records().iter().map(|r| r.turn).collect::<Vec<_>>(),
        vec![1, 2]
    );
    // `restore` re-seeds from a branch checkpoint and keeps ids monotonic.
    ledger.restore(vec![record(1, "c1"), record(2, "c2"), record(3, "c3")]);
    assert_eq!(ledger.len(), 3);
    let next = ledger.record_mutation(4, "edit", "c4", None, None, Vec::new());
    assert_eq!(next, "mut-0004");

    // ---- WAL branch selection ----
    let root = temp_root("fork");
    let ws_dir = root.join("ws");
    fs::create_dir_all(&ws_dir).unwrap();
    let mut journal = SessionJournal::create(root.join("sessions"), &ws_dir).unwrap();
    journal.append_item(0, TranscriptItem::User(UserMessage { content: "t0".into() })).unwrap();
    journal.append_item(1, TranscriptItem::User(UserMessage { content: "t1".into() })).unwrap();
    journal
        .checkpoint_with_revision(1, 0, Vec::new(), "", None, Vec::new(), vec![record(1, "c1")])
        .unwrap();
    journal.append_item(2, TranscriptItem::User(UserMessage { content: "t2".into() })).unwrap();
    journal
        .checkpoint_with_revision(
            2,
            0,
            vec![FileTouch { path: "src/c3.rs".into(), last_touched_turn: 3 }],
            "",
            None,
            Vec::new(),
            vec![record(1, "c1"), record(2, "c2"), record(3, "c3")],
        )
        .unwrap();
    let wal_path = journal.path().to_path_buf();
    drop(journal);

    // Before the fork the deepest checkpoint carries the whole ledger.
    {
        let journal = SessionJournal::open(&wal_path).unwrap();
        let full = journal.latest_active_checkpoint().unwrap();
        assert_eq!(full.turn, 2);
        assert_eq!(full.mutations.len(), 3);
    }

    // `--fork 1` selects the turn-1 leaf, so the turn-2 checkpoint is off-branch.
    let mut journal = SessionJournal::open(&wal_path).unwrap();
    journal.fork_at_turn(1).unwrap();
    {
        let forked = journal.latest_active_checkpoint().unwrap();
        assert_eq!(forked.turn, 1);
        assert_eq!(forked.mutations.len(), 1);
        assert_eq!(forked.mutations[0].call_id, "c1");
    }
    drop(journal);

    // Even a checkpoint that somehow carried future records is pruned by the runner's safety net.
    let mut restored = MutationLedger::new();
    restored.restore(vec![record(1, "c1"), record(2, "c2"), record(3, "c3")]);
    restored.prune_after_turn(1);
    assert_eq!(
        restored.records().iter().map(|r| r.turn).collect::<Vec<_>>(),
        vec![1]
    );

    let _ = fs::remove_dir_all(&root);
}

// ---------------------------------------------------------------------------------------------
// Test 6 — evaluation corpus + artifact
// ---------------------------------------------------------------------------------------------

struct CorpusScenario {
    name: &'static str,
    expected: Option<&'static str>,
    ledger: MutationLedger,
    failure_output: &'static str,
}

fn corpus_scenarios() -> Vec<CorpusScenario> {
    fn span(path: &str, start: usize, end: usize, symbols: &[&str]) -> MutatedFileSpan {
        MutatedFileSpan {
            relative_path: path.to_string(),
            start_line: start,
            end_line: end,
            added_symbols: symbols.iter().map(|s| s.to_string()).collect(),
            summary: format!("edit {} lines {}-{}", path, start, end),
        }
    }
    let mut out = Vec::new();

    // 1. Rust compile error at the exact edited span.
    let mut ledger = MutationLedger::new();
    ledger.record_mutation(1, "edit", "c1", None, None, vec![span("src/a.rs", 10, 14, &[])]);
    out.push(CorpusScenario {
        name: "rust_compile_error_at_edited_span",
        expected: Some("mut-0001"),
        ledger,
        failure_output: "error[E0308]: mismatched types\n  --> src/a.rs:12:9\n",
    });

    // 2. Rust symbol rename break in a *caller* file the model never touched, alongside a later
    //    unrelated edit. A path-based engine can only recover this at rank 2 — reported honestly.
    let mut ledger = MutationLedger::new();
    ledger.record_mutation(
        1,
        "edit",
        "c1",
        None,
        None,
        vec![span("src/api.rs", 3, 5, &["compute_v2"])],
    );
    ledger.record_mutation(2, "edit", "c2", None, None, vec![span("src/unrelated.rs", 1, 2, &[])]);
    out.push(CorpusScenario {
        name: "rust_symbol_rename_break_in_caller_file",
        expected: Some("mut-0001"),
        ledger,
        failure_output:
            "error[E0425]: cannot find function `compute` in this scope\n  --> src/caller.rs:9:5\n",
    });

    // 3. JSON syntax break.
    let mut ledger = MutationLedger::new();
    ledger.record_mutation(1, "write", "c1", None, None, vec![span("config.json", 1, 9, &[])]);
    out.push(CorpusScenario {
        name: "json_syntax_break",
        expected: Some("mut-0001"),
        ledger,
        failure_output: "config.json:7:3: error: unexpected token `}`\n",
    });

    // 4. Unclosed brace in TypeScript.
    let mut ledger = MutationLedger::new();
    ledger.record_mutation(1, "write", "c1", None, None, vec![span("src/app.ts", 1, 20, &[])]);
    out.push(CorpusScenario {
        name: "unclosed_brace_typescript",
        expected: Some("mut-0001"),
        ledger,
        failure_output: "src/app.ts:12:1 - error TS1005: '}' expected.\n",
    });

    // 5. Python traceback.
    let mut ledger = MutationLedger::new();
    ledger.record_mutation(1, "edit", "c1", None, None, vec![span("app.py", 5, 9, &["compute"])]);
    out.push(CorpusScenario {
        name: "python_traceback",
        expected: Some("mut-0001"),
        ledger,
        failure_output: "Traceback (most recent call last):\n  File \"/ws/app.py\", line 7, in <module>\nNameError: name 'compute' is not defined\n",
    });

    // 6. Clean edit, no failure output at all.
    let mut ledger = MutationLedger::new();
    ledger.record_mutation(1, "edit", "c1", None, None, vec![span("src/clean.rs", 1, 3, &[])]);
    out.push(CorpusScenario {
        name: "clean_edit_no_errors",
        expected: None,
        ledger,
        failure_output: "",
    });

    // 7. Pre-existing failure in a file nobody edited.
    let mut ledger = MutationLedger::new();
    ledger.record_mutation(1, "edit", "c1", None, None, vec![span("src/x.rs", 1, 4, &[])]);
    ledger.record_mutation(2, "edit", "c2", None, None, vec![span("src/y.rs", 20, 24, &[])]);
    out.push(CorpusScenario {
        name: "pre_existing_unrelated_error",
        expected: None,
        ledger,
        failure_output: "vendor/legacy.c:40:5: error: implicit declaration of function 'legacy_init'\n",
    });

    // 8. Multi-edit turn disambiguation.
    let mut ledger = MutationLedger::new();
    ledger.record_mutation(3, "edit", "c1", None, None, vec![span("src/x.rs", 5, 8, &[])]);
    ledger.record_mutation(3, "edit", "c2", None, None, vec![span("src/y.rs", 20, 24, &[])]);
    out.push(CorpusScenario {
        name: "multi_edit_turn_disambiguation",
        expected: Some("mut-0002"),
        ledger,
        failure_output: "src/y.rs:22:3: error: expected `;`\n",
    });

    out
}

#[test]
fn test_gate4_eval_corpus_metrics_and_artifact_generation() {
    let scenarios = corpus_scenarios();
    assert!(scenarios.len() >= 8, "the corpus needs at least 8 scenarios");

    let validator = PredictiveValidator::new(true);
    let mut labeled = 0usize;
    let mut top1_hits = 0usize;
    let mut top2_hits = 0usize;
    let mut results = Vec::new();

    for scenario in &scenarios {
        let report = AttributionEngine::attribute_failure(
            &scenario.ledger,
            scenario.failure_output,
            "completion_gate",
            3,
        );
        assert_eq!(report.framing, CANDIDATE_ATTRIBUTION_FRAMING);
        let ranked: Vec<String> = report
            .candidates
            .iter()
            .map(|candidate| candidate.mutation_id.clone())
            .collect();
        let top1 = ranked.first().cloned();
        let top2 = ranked.iter().take(2).cloned().collect::<Vec<_>>();
        let (top1_hit, top2_hit) = match scenario.expected {
            Some(expected) => {
                labeled += 1;
                let t1 = top1.as_deref() == Some(expected);
                let t2 = top2.iter().any(|id| id == expected);
                if t1 {
                    top1_hits += 1;
                }
                if t2 {
                    top2_hits += 1;
                }
                (Some(t1), Some(t2))
            }
            None => (None, None),
        };
        results.push(serde_json::json!({
            "scenario": scenario.name,
            "expected_mutation_id": scenario.expected,
            "anchors": report.anchors.len(),
            "candidates": report.candidates.iter().map(|candidate| serde_json::json!({
                "mutation_id": candidate.mutation_id,
                "turn": candidate.turn,
                "tool_name": candidate.tool_name,
                "relative_path": candidate.relative_path,
                "line_range": candidate.line_range,
                "confidence": candidate.confidence,
                "rationale": candidate.rationale,
            })).collect::<Vec<_>>(),
            "top1_mutation_id": top1,
            "top1_hit": top1_hit,
            "top2_hit": top2_hit,
        }));
    }

    assert_eq!(labeled, 6, "six scenarios carry a known injection point");
    assert!(
        top1_hits >= 5,
        "top-1 precision regressed: {}/{}",
        top1_hits,
        labeled
    );
    assert_eq!(
        top2_hits, labeled,
        "every injected fault must be recovered within the top 2 candidates"
    );
    let top1_precision = top1_hits as f64 / labeled as f64;
    let top2_recall = top2_hits as f64 / labeled as f64;

    // ---- false-positive rollback probe ----
    // Valid-but-tricky files must produce no warning, and validating them must never rewrite them.
    let probe_root = temp_root("fp-probe");
    let probe_ws = WorkspaceCap::new(&probe_root).unwrap();
    let probes: [(&str, &str); 5] = [
        ("a.rs", "pub fn f<'a>(x: &'a str) -> &'a str {\n    let s = \"} { ) ]\";\n    let r = r#\"{ \"x\" }\"#;\n    let c = '}';\n    // } } }\n    /* } { */\n    x\n}\n"),
        ("b.js", "const t = `{ [ } ]`;\nfunction f() { return `${t}`; }\n"),
        ("c.ts", "type T = { a: string };\nconst v: T = { a: '{' };\n"),
        ("d.py", "def f():\n    s = \"\"\"{\n}\n\"\"\"\n    return s  # } {\n"),
        ("e.go", "package main\n\nfunc main() {\n\tb := `raw { }`\n\t_ = b\n}\n"),
    ];
    let mut probe_warnings = 0usize;
    let mut rolled_back = 0usize;
    let mut validation_total_us = 0u128;
    let mut validation_calls = 0usize;
    for (name, content) in probes {
        let path = probe_root.join(name);
        fs::write(&path, content).unwrap();
        let before = fs::read(&path).unwrap();
        let started = std::time::Instant::now();
        let report = validator.validate_file(&probe_ws, name);
        validation_total_us += started.elapsed().as_micros();
        validation_calls += 1;
        if !report.issues.is_empty() {
            probe_warnings += 1;
        }
        if fs::read(&path).unwrap() != before {
            rolled_back += 1;
        }
    }
    // A deliberately broken file must warn (proving the probe set is not vacuous) and still be
    // left untouched.
    fs::write(probe_root.join("broken.rs"), "pub fn f() {\n").unwrap();
    let before = fs::read(probe_root.join("broken.rs")).unwrap();
    let started = std::time::Instant::now();
    let report = validator.validate_file(&probe_ws, "broken.rs");
    validation_total_us += started.elapsed().as_micros();
    validation_calls += 1;
    assert!(!report.issues.is_empty());
    if fs::read(probe_root.join("broken.rs")).unwrap() != before {
        rolled_back += 1;
    }

    assert_eq!(probe_warnings, 0, "valid files must never produce a warning");
    assert_eq!(rolled_back, 0, "nothing may ever be rolled back");
    let false_positive_rollback_rate = rolled_back as f64 / (probe_warnings + rolled_back + 1) as f64;
    assert_eq!(false_positive_rollback_rate, 0.0);
    let mean_validation_overhead_ms =
        validation_total_us as f64 / validation_calls as f64 / 1000.0;

    let artifact = serde_json::json!({
        "artifact": "gate4-predictive-validation-and-attribution-eval",
        "harness_version": env!("CARGO_PKG_VERSION"),
        "generated_by": "crates/harness-core/tests/gate4_predictive_attribution.rs::test_gate4_eval_corpus_metrics_and_artifact_generation",
        "blueprint": "docs/v1_blueprint/IMPLEMENTATION_GATES_AND_EVAL.md#gate-4--predictive-validation",
        "framing": CANDIDATE_ATTRIBUTION_FRAMING,
        "metrics": {
            "labeled_scenarios": labeled,
            "total_scenarios": scenarios.len(),
            "top1_attribution_precision": top1_precision,
            "top2_attribution_recall": top2_recall,
            "false_positive_rollback_rate": false_positive_rollback_rate,
            "false_positive_probe_files": probes.len(),
            "false_positive_warnings": probe_warnings,
            "rolled_back_files": rolled_back,
            "mean_validation_overhead_ms": mean_validation_overhead_ms,
            "validation_calls": validation_calls,
        },
        "results": results,
        "notes": [
            "Metrics are measured on a synthetic corpus with known injection points; the blueprint requires synthetic ground truth for attribution precision/recall.",
            "Attribution is always emitted as `candidate_attribution`; no run claims root-cause certainty and nothing is auto-rolled back on a warning.",
            "rust_symbol_rename_break_in_caller_file is the one scenario where top-1 is not the injected fault: the failing anchor is in a file the model never edited, so the engine falls back to recency and the rename is recovered at rank 2. Raising that to rank 1 needs the reproduce/revert evidence step the blueprint describes, not a bigger heuristic.",
            "mean_validation_overhead_ms covers only the in-process structural check (no process spawn).",
            "false_positive_rollback_rate is structurally 0.0: the predictive check is advisory and never mutates the workspace."
        ],
    });
    let metrics_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/GATE4_EVAL_METRICS.json");
    fs::write(&metrics_path, serde_json::to_string_pretty(&artifact).unwrap())
        .unwrap_or_else(|e| panic!("failed to write {}: {}", metrics_path.display(), e));
    eprintln!("[gate4-eval] wrote {}", metrics_path.display());

    // The evidence graph parses the diagnostic anchors for every non-empty failure output.
    for scenario in &scenarios {
        if scenario.failure_output.is_empty() {
            continue;
        }
        assert!(
            !parse_diagnostic_anchors(scenario.failure_output).is_empty(),
            "{}: the corpus failure output must be parseable",
            scenario.name
        );
    }

    let _ = fs::remove_dir_all(&probe_root);
}
