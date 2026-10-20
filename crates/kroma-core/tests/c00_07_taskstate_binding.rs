//! C00-07: the semantic [`TaskState`] is derived from the canonical transcript at the request
//! boundary and stamped onto the existing session checkpoint.
//!
//! The stamp is *provenance*, not a verdict. It names the WAL leaf, the events that supplied the
//! facts, and the workspace revision the process had observed at that moment. It deliberately
//! carries no `valid`/`verified` flag: Kroma only advances a revision when it captures one itself,
//! so an edit made outside the run advances nothing and a boolean here would be a claim the process
//! cannot check.
//!
//! These tests are named for the two behaviours that make the wiring traceable at all:
//!
//! * the production request path (FakeProvider, no model call) actually writes a binding whose
//!   digest and source entry ids reproduce from the transcript prefix it names;
//! * a checkpoint written without a binding still loads (the field is additive), and a revision
//!   change is *visible* as a different observed revision without any old fact being re-declared
//!   valid.

use kroma_core::agent_loop::{build_default_runtime, AgentRunConfig, AgentRunner};
use kroma_core::budget::BudgetConfig;
use kroma_core::env::LocalHostEnv;
use kroma_core::events::EventLogger;
use kroma_core::messages::{AssistantTurn, FinishReason, ToolCall, TranscriptItem, UserMessage};
use kroma_core::provider::FakeProvider;
use kroma_core::revision::SnapshotStore;
use kroma_core::session::{SessionCheckpoint, SessionJournal};
use kroma_core::task_state::TaskState;
use kroma_core::tool::policy::{NonInteractiveApprovalHandler, PolicyEngine, PolicyMode};
use kroma_core::types::{RunStatus, ToolCallId};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::watch;
use uuid::Uuid;

const TASK: &str = "Fix the failing test. Do not modify anything under tests/. Verify with cargo test -p kroma-core.";

fn temp_root(tag: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("kroma-c0007-{}-{}", tag, Uuid::new_v4()));
    fs::create_dir_all(&path).unwrap();
    path
}

/// Every `checkpoint` payload's `data` object, in WAL order. Read from the raw file so the test sees
/// exactly the bytes a reader/resume would see, not a private in-memory accessor.
fn checkpoints(wal_path: &Path) -> Vec<Value> {
    fs::read_to_string(wal_path)
        .unwrap()
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|line| line["payload"]["kind"] == "checkpoint")
        .map(|line| line["payload"]["data"].clone())
        .collect()
}

fn bindings(checkpoints: &[Value]) -> Vec<Value> {
    checkpoints
        .iter()
        .filter(|checkpoint| !checkpoint["task_state"].is_null())
        .map(|checkpoint| checkpoint["task_state"].clone())
        .collect()
}

fn write_turn() -> AssistantTurn {
    AssistantTurn {
        content: None,
        reasoning_blocks: vec![],
        tool_calls: vec![ToolCall {
            call_id: ToolCallId::new("call_write"),
            name: "write".into(),
            raw_arguments: r#"{"path":"src/app.rs","content":"fn main() { println!(\"hi\"); }\n"}"#
                .into(),
        }],
        finish_reason: FinishReason::ToolCalls,
        usage: None,
    }
}

fn read_turn(call_id: &str, path: &str) -> AssistantTurn {
    AssistantTurn {
        content: None,
        reasoning_blocks: vec![],
        tool_calls: vec![ToolCall {
            call_id: ToolCallId::new(call_id),
            name: "read".into(),
            raw_arguments: format!(r#"{{"path":"{path}"}}"#),
        }],
        finish_reason: FinishReason::ToolCalls,
        usage: None,
    }
}

fn stop_turn() -> AssistantTurn {
    AssistantTurn {
        content: Some("done".into()),
        reasoning_blocks: vec![],
        tool_calls: vec![],
        finish_reason: FinishReason::Stop,
        usage: None,
    }
}

struct RunFixture {
    ws: PathBuf,
    state_dir: PathBuf,
    wal_path: PathBuf,
}

/// One `write` tool call followed by a stop, so the run crosses two request boundaries (and, with a
/// snapshot store, one workspace mutation).
async fn run_write_then_stop(root: &Path, with_snapshot_store: bool) -> RunFixture {
    let ws = root.join("ws");
    let state_dir = root.join("state");
    fs::create_dir_all(ws.join("src")).unwrap();
    fs::create_dir_all(&state_dir).unwrap();
    fs::write(ws.join("src/app.rs"), "fn main() {}\n").unwrap();

    let journal = SessionJournal::create(state_dir.join("sessions"), &ws).unwrap();
    let wal_path = journal.path().to_path_buf();
    let env = Arc::new(LocalHostEnv::new(&ws, state_dir.clone()).unwrap());
    let policy = PolicyEngine::new(
        PolicyMode::AllowWrite,
        Arc::new(NonInteractiveApprovalHandler),
    );
    let budget = BudgetConfig::default();
    let runtime = build_default_runtime(env, policy, budget.clone(), None);
    let cfg = AgentRunConfig::new(TASK, budget);
    let logger = EventLogger::new(cfg.run_id, Some(state_dir.clone()), false).unwrap();
    let (_tx, rx) = watch::channel(false);
    let mut runner = AgentRunner::new(
        cfg,
        Arc::new(FakeProvider::new(vec![write_turn(), stop_turn()])),
        runtime,
        rx,
    )
    .with_session_journal(journal);
    if with_snapshot_store {
        runner = runner.with_snapshot_store(Arc::new(SnapshotStore::new(root.join("snapshots"))));
    }
    let (summary, _logger) = runner.run(logger).await;
    assert_eq!(
        summary.status,
        RunStatus::Completed,
        "{:?}",
        summary.terminal_reason
    );
    RunFixture {
        ws,
        state_dir,
        wal_path,
    }
}

/// The production request path records a binding, and the digest plus source entry ids reproduce
/// exactly from the transcript prefix the binding names.
#[tokio::test]
async fn request_path_records_a_binding_that_reproduces_from_its_named_leaf() {
    let root = temp_root("request");
    let fixture = run_write_then_stop(&root, false).await;

    let journal = SessionJournal::open(&fixture.wal_path).unwrap();
    let entries = journal.active_entries();
    let entry_ids = journal.active_entry_ids();
    assert_eq!(
        entry_ids.len(),
        entries.len(),
        "the id-only accessor must describe the same active path as active_entries()"
    );
    let items: Vec<TranscriptItem> = entries.into_iter().map(|entry| entry.item).collect();

    let recorded = checkpoints(&fixture.wal_path);
    let recorded = bindings(&recorded);
    assert!(
        !recorded.is_empty(),
        "the request path must persist a task-state binding; checkpoints carried none"
    );
    let binding = recorded.last().unwrap();

    let transcript_len = binding["transcript_len"].as_u64().unwrap() as usize;
    assert!(
        transcript_len > 0 && transcript_len <= items.len(),
        "the binding must name a prefix of the canonical transcript: {transcript_len} of {}",
        items.len()
    );
    // The derivation happened at the request boundary, so the prefix — not the whole transcript — is
    // what the digest has to reproduce from.
    let derived =
        TaskState::derive_with_source_ids(&items[..transcript_len], &entry_ids[..transcript_len]);
    assert_eq!(
        derived.constraints.len(),
        1,
        "the fixture must produce a constraint, otherwise the digest proves nothing: {derived:?}"
    );
    assert_eq!(
        derived.verification.len(),
        1,
        "the fixture must produce a verification item: {derived:?}"
    );
    let expected_digest = derived
        .digest()
        .expect("a derived state must be digestible");
    assert_eq!(
        binding["state_digest"].as_str(),
        Some(expected_digest.as_str()),
        "the recorded digest must be the digest of the state derived from the named leaf"
    );
    assert_eq!(
        binding["constraint_count"].as_u64(),
        Some(derived.constraints.len() as u64)
    );
    assert_eq!(
        binding["verification_count"].as_u64(),
        Some(derived.verification.len() as u64)
    );

    // Provenance points at the event the fact came from, and the leaf identity is on the active path.
    assert_eq!(
        binding["source_entry_ids"],
        serde_json::json!([entry_ids[0]]),
        "the user turn is the source of both the constraint and the verification item"
    );
    assert_eq!(
        binding["leaf_entry_id"],
        serde_json::json!(entry_ids[transcript_len - 1])
    );
    assert!(entry_ids.contains(&binding["leaf_entry_id"].as_str().unwrap().to_string()));

    // No snapshot store was attached, so no revision was observed: recorded as null, never guessed.
    assert!(
        binding["derived_at_observed_revision"].is_null(),
        "without a captured revision the binding must say so: {binding}"
    );

    let _ = fs::remove_dir_all(&root);
}

/// A checkpoint written without a binding keeps the pre-C00-07 on-disk shape and still loads; once
/// the workspace revision advances, the binding names a different observed revision without any old
/// fact being re-declared valid.
#[tokio::test]
async fn legacy_checkpoint_without_a_binding_loads_and_a_revision_change_stays_visible() {
    let root = temp_root("legacy");
    let ws = root.join("ws");
    let state_dir = root.join("state");
    fs::create_dir_all(&ws).unwrap();
    fs::create_dir_all(&state_dir).unwrap();

    // (1) Old WAL shape: a checkpoint with no binding must serialize without the new key, because
    // the WAL hash chain covers the serialized payload — an added key would invalidate every
    // earlier line on read.
    let mut journal = SessionJournal::create(state_dir.join("sessions"), &ws).unwrap();
    let legacy_wal = journal.path().to_path_buf();
    journal
        .append_item(
            0,
            TranscriptItem::User(UserMessage {
                content: "do not modify tests/".into(),
            }),
        )
        .unwrap();
    journal.checkpoint(0, 0, Vec::new(), "legacy-hash").unwrap();
    drop(journal);

    let raw = fs::read_to_string(&legacy_wal).unwrap();
    assert!(
        !raw.contains("task_state"),
        "a checkpoint without a binding must keep the legacy bytes: {raw}"
    );
    let reopened = SessionJournal::open(&legacy_wal).expect("a legacy WAL must still load");
    assert!(
        reopened
            .latest_active_checkpoint()
            .unwrap()
            .task_state
            .is_none(),
        "an absent field must read as 'no binding', not as an empty one"
    );
    // The struct-level half of the same compatibility promise.
    let legacy_json = serde_json::json!({
        "leaf_id": null,
        "turn": 0,
        "pinned_baseline_end": 0,
        "last_touched": [],
        "reasoning_blocks_sha256": "",
    });
    let legacy_checkpoint: SessionCheckpoint = serde_json::from_value(legacy_json).unwrap();
    assert!(legacy_checkpoint.task_state.is_none());

    // (2) The revision advances inside the run (baseline, then the `write` batch). The earlier
    // binding keeps naming the revision it observed; the later one names the new revision.
    let fixture = run_write_then_stop(&root, true).await;
    let journal = SessionJournal::open(&fixture.wal_path).unwrap();
    let latest_revision = journal
        .latest_revision_id()
        .expect("the snapshot store must have captured revisions");

    let recorded = checkpoints(&fixture.wal_path);
    let recorded = bindings(&recorded);
    let observed: Vec<Option<String>> = recorded
        .iter()
        .map(|binding| {
            binding["derived_at_observed_revision"]
                .as_str()
                .map(str::to_string)
        })
        .collect();
    let first = observed
        .first()
        .cloned()
        .flatten()
        .expect("the first request must record the observed baseline revision");
    let last = observed
        .last()
        .cloned()
        .flatten()
        .expect("the last request must record the observed revision");
    assert_ne!(
        first, last,
        "a revision change must be visible in the binding, not silently absorbed: {observed:?}"
    );
    assert_eq!(
        last, latest_revision,
        "the last binding must name the revision the run ended on"
    );
    assert_eq!(
        observed.first().cloned().flatten(),
        Some(first),
        "an earlier binding must keep naming the revision it observed; rewriting it to the new \
         revision would claim the older facts were re-checked"
    );

    // The record never equates "derived at revision A" with "current at revision B": at least one
    // checkpoint carries a newer `workspace_revision_id` than the revision its binding observed.
    assert!(
        recorded_checkpoint_pairs(&fixture.wal_path).iter().any(
            |(checkpoint_revision, observed_revision)| {
                checkpoint_revision.as_deref() != observed_revision.as_deref()
            }
        ),
        "the checkpoint's revision and the binding's observed revision must be independent fields"
    );

    // No validity verdict was smuggled in: the stamp is provenance only.
    for binding in &recorded {
        for key in binding.as_object().unwrap().keys() {
            let lowered = key.to_ascii_lowercase();
            assert!(
                !lowered.contains("valid") && !lowered.contains("verified"),
                "a binding must not carry a validity/verification claim: {key}"
            );
        }
    }

    let _ = fs::remove_dir_all(&root);
    let _ = (fixture.ws, fixture.state_dir);
}

/// `(checkpoint.workspace_revision_id, binding.derived_at_observed_revision)` for every checkpoint
/// that carries a binding, in WAL order.
fn recorded_checkpoint_pairs(wal_path: &Path) -> Vec<(Option<String>, Option<String>)> {
    checkpoints(wal_path)
        .into_iter()
        .filter(|checkpoint| !checkpoint["task_state"].is_null())
        .map(|checkpoint| {
            (
                checkpoint["workspace_revision_id"]
                    .as_str()
                    .map(str::to_string),
                checkpoint["task_state"]["derived_at_observed_revision"]
                    .as_str()
                    .map(str::to_string),
            )
        })
        .collect()
}

/// The binding must name the transcript a reader will actually replay. Once a request compacts, the
/// projection overlay makes the stubbed items canonical, so a digest derived before projection can
/// never be recomputed from the leaf it points at.
#[tokio::test]
async fn binding_is_derived_from_the_transcript_the_compaction_overlay_makes_canonical() {
    let root = temp_root("compaction");
    let ws = root.join("ws");
    let state_dir = root.join("state");
    fs::create_dir_all(ws.join("src")).unwrap();
    fs::create_dir_all(&state_dir).unwrap();
    // Large enough to cross the watermark, and full of path-like tokens so the raw tool output
    // contributes path mentions that compaction drops.
    let mut big = String::new();
    for i in 0..2000 {
        big.push_str(&format!(
            "src/module{i}.rs: filler line for compaction padding padding padding padding\n"
        ));
    }
    // Three distinct paths: identical `read` calls in a row are stopped by the loop guard before the
    // context can grow, and three results are what push the request over the watermark.
    for name in ["a.rs", "b.rs", "c.rs"] {
        fs::write(ws.join("src").join(name), &big).unwrap();
    }

    let journal = SessionJournal::create(state_dir.join("sessions"), &ws).unwrap();
    let wal_path = journal.path().to_path_buf();
    let env = Arc::new(LocalHostEnv::new(&ws, state_dir.clone()).unwrap());
    let policy = PolicyEngine::new(
        PolicyMode::AllowWrite,
        Arc::new(NonInteractiveApprovalHandler),
    );
    let budget = BudgetConfig {
        // Sized from the measured fixture: the static prefix is ~10 KiB and one `read` result is
        // capped at ~64 KiB. Turns 2 and 3 fit; turn 4 does not, and by then the first read result
        // is outside the protected tail window, so it is the one that gets compacted.
        context_high_watermark_bytes: 170 * 1024,
        context_low_watermark_bytes: 160 * 1024,
        context_tail_window_bytes: 512,
        context_min_tail_turns: 1,
        max_total_tool_output_bytes: 8 * 1024 * 1024,
        ..Default::default()
    };
    let runtime = build_default_runtime(env, policy, budget.clone(), None);
    let cfg = AgentRunConfig::new(TASK, budget);
    let logger = EventLogger::new(cfg.run_id, Some(state_dir.clone()), false).unwrap();
    let (_tx, rx) = watch::channel(false);
    let provider = Arc::new(FakeProvider::new(vec![
        read_turn("call_read_1", "src/a.rs"),
        read_turn("call_read_2", "src/b.rs"),
        read_turn("call_read_3", "src/c.rs"),
        stop_turn(),
    ]));
    let (summary, _logger) = AgentRunner::new(cfg, provider, runtime, rx)
        .with_session_journal(journal)
        .run(logger)
        .await;
    assert_eq!(
        summary.status,
        RunStatus::Completed,
        "{:?}",
        summary.terminal_reason
    );

    let journal = SessionJournal::open(&wal_path).unwrap();
    // The fixture must actually compact. A persisted `ProjectionUpdate` is the overlay that makes a
    // stub canonical, so without one this test would prove nothing about post-projection derivation.
    let raw_wal = fs::read_to_string(&wal_path).unwrap();
    assert!(
        raw_wal.contains(r#""kind":"projection_update""#),
        "the fixture must trigger real compaction, not just a small watermark"
    );

    let entry_ids = journal.active_entry_ids();
    let canonical: Vec<TranscriptItem> = journal
        .active_entries()
        .into_iter()
        .map(|entry| entry.item)
        .collect();
    let recorded = bindings(&checkpoints(&wal_path));
    let binding = recorded
        .last()
        .expect("the request path must persist a task-state binding");
    let transcript_len = binding["transcript_len"].as_u64().unwrap() as usize;
    assert!(transcript_len > 0 && transcript_len <= canonical.len());

    // A reader replays the leaf and re-derives: that digest must be the recorded one.
    let derived = TaskState::derive_with_source_ids(
        &canonical[..transcript_len],
        &entry_ids[..transcript_len],
    );
    assert_eq!(
        derived.constraints.len(),
        1,
        "the user constraint must survive compaction: {derived:?}"
    );
    assert_eq!(derived.verification.len(), 1);
    let canonical_digest = derived
        .digest()
        .expect("a derived state must be digestible");
    assert_eq!(
        binding["state_digest"].as_str(),
        Some(canonical_digest.as_str()),
        "the digest must reproduce from the canonical (post-overlay) leaf prefix"
    );
    assert_eq!(
        binding["source_entry_ids"],
        serde_json::json!([entry_ids[0]]),
        "the constraint's source event is the user turn, which compaction never rewrites"
    );

    // The raw bytes are gone from the canonical transcript. The binding therefore must not describe
    // the pre-compaction text: that derivation is a different state, and it is what the old
    // pre-projection position recorded.
    let raw: Vec<TranscriptItem> = entry_ids[..transcript_len]
        .iter()
        .map(|id| journal.raw_entry_item(id).unwrap().clone())
        .collect();
    let raw_derived = TaskState::derive_with_source_ids(&raw, &entry_ids[..transcript_len]);
    assert!(
        !raw_derived.path_mentions.is_empty(),
        "the raw tool output must mention paths, otherwise the fixture proves nothing"
    );
    assert!(
        raw_derived.path_mentions.len() > derived.path_mentions.len(),
        "compaction must have dropped path mentions: raw {}, canonical {}",
        raw_derived.path_mentions.len(),
        derived.path_mentions.len()
    );
    assert_ne!(
        raw_derived.digest().unwrap(),
        canonical_digest,
        "a pre-projection digest is not the digest a reader can recompute from the leaf"
    );

    let _ = fs::remove_dir_all(&root);
}

/// Every `task_state_facts_collected` payload in event-stream order, read from the raw log file so
/// the test sees exactly the bytes a reviewer would grep.
fn facts_collected_events(log_path: &Path) -> Vec<Value> {
    fs::read_to_string(log_path)
        .unwrap()
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|event| event["event_type"] == "TaskStateFactsCollected")
        .map(|event| event["payload"].clone())
        .collect()
}

fn keys_of(payload: &Value, field: &str) -> Vec<String> {
    payload[field]
        .as_array()
        .unwrap_or_else(|| panic!("{field} must be an array: {payload}"))
        .iter()
        .map(|key| key.as_str().expect("keys are strings").to_string())
        .collect()
}

/// C00-07: in the production request path a fact a still-live constraint names is pinned and
/// survives compaction, an unreferenced fact is collected, and the GC report lands in the event
/// stream as keys, not as file content.
///
/// The fixture copies the sizing of `binding_is_derived_from_the_transcript_the_compaction_overlay
/// _makes_canonical`: three ~64 KiB `read` results, so the fourth request compacts the *first* one.
/// The first result's path mentions are what disappear from the leaf; the constraint that names
/// them is a user turn, which compaction never rewrites.
#[tokio::test]
async fn pinned_fact_survives_compaction_in_the_request_path_and_gc_is_auditable() {
    const PIN_TASK: &str =
        "Fix the failing test. Do not modify keep/. Verify with cargo test -p kroma-core.";
    let root = temp_root("pins");
    let ws = root.join("ws");
    let state_dir = root.join("state");
    fs::create_dir_all(ws.join("keep")).unwrap();
    fs::create_dir_all(ws.join("other")).unwrap();
    fs::create_dir_all(&state_dir).unwrap();

    // Files whose *paths* the tool output mentions. `keep/` is the directory the user constraint
    // names, so mentions under it are referenced; `other/` is not named by anything.
    fs::write(ws.join("keep/target.rs"), "pub fn target() {}\n").unwrap();
    fs::write(ws.join("other/drop.rs"), "pub fn drop_it() {}\n").unwrap();
    let filler = |token: &str| {
        let mut body = String::new();
        for _ in 0..2000 {
            body.push_str(&format!(
                "{token}: filler line for compaction padding padding padding padding\n"
            ));
        }
        body
    };
    // Read targets are themselves real files (their paths show up in the read header).
    fs::write(ws.join("keep/big_a.rs"), filler("keep/target.rs")).unwrap();
    fs::write(ws.join("other/big_b.rs"), filler("other/drop.rs")).unwrap();
    fs::write(ws.join("other/big_c.rs"), filler("other/second.rs")).unwrap();

    let journal = SessionJournal::create(state_dir.join("sessions"), &ws).unwrap();
    let wal_path = journal.path().to_path_buf();
    let env = Arc::new(LocalHostEnv::new(&ws, state_dir.clone()).unwrap());
    let policy = PolicyEngine::new(
        PolicyMode::AllowWrite,
        Arc::new(NonInteractiveApprovalHandler),
    );
    let budget = BudgetConfig {
        context_high_watermark_bytes: 170 * 1024,
        context_low_watermark_bytes: 160 * 1024,
        context_tail_window_bytes: 512,
        context_min_tail_turns: 1,
        max_total_tool_output_bytes: 8 * 1024 * 1024,
        ..Default::default()
    };
    let runtime = build_default_runtime(env, policy, budget.clone(), None);
    let cfg = AgentRunConfig::new(PIN_TASK, budget);
    let logger = EventLogger::new(cfg.run_id, Some(state_dir.clone()), false).unwrap();
    let log_path = logger.log_path().to_path_buf();
    let (_tx, rx) = watch::channel(false);
    let provider = Arc::new(FakeProvider::new(vec![
        read_turn("call_read_1", "keep/big_a.rs"),
        read_turn("call_read_2", "other/big_b.rs"),
        read_turn("call_read_3", "other/big_c.rs"),
        stop_turn(),
    ]));
    let (summary, _logger) = AgentRunner::new(cfg, provider, runtime, rx)
        .with_session_journal(journal)
        .run(logger)
        .await;
    assert_eq!(
        summary.status,
        RunStatus::Completed,
        "{:?}",
        summary.terminal_reason
    );

    // The fixture must compact, otherwise nothing about survival across compaction is proven.
    let raw_wal = fs::read_to_string(&wal_path).unwrap();
    assert!(
        raw_wal.contains(r#""kind":"projection_update""#),
        "the fixture must trigger real compaction"
    );

    let journal = SessionJournal::open(&wal_path).unwrap();
    let entry_ids = journal.active_entry_ids();
    let canonical: Vec<TranscriptItem> = journal
        .active_entries()
        .into_iter()
        .map(|entry| entry.item)
        .collect();
    let binding = bindings(&checkpoints(&wal_path))
        .pop()
        .expect("the request path must persist a task-state binding");
    let transcript_len = binding["transcript_len"].as_u64().unwrap() as usize;
    let derived = TaskState::derive_with_source_ids(
        &canonical[..transcript_len],
        &entry_ids[..transcript_len],
    );
    let raw: Vec<TranscriptItem> = entry_ids[..transcript_len]
        .iter()
        .map(|id| journal.raw_entry_item(id).unwrap().clone())
        .collect();
    let raw_derived = TaskState::derive_with_source_ids(&raw, &entry_ids[..transcript_len]);

    // The mention that is gone from the leaf, and only gone there.
    let mentions = |state: &TaskState| -> Vec<String> {
        state
            .path_mentions
            .iter()
            .map(|mention| mention.path.clone())
            .collect()
    };
    assert!(
        mentions(&raw_derived).contains(&"keep/target.rs".to_string()),
        "the raw tool output must mention the constrained path: {:?}",
        mentions(&raw_derived)
    );
    assert!(
        !mentions(&derived).contains(&"keep/target.rs".to_string()),
        "compaction must have dropped that mention, otherwise the pin proves nothing: {:?}",
        mentions(&derived)
    );

    // The GC report is readable from the event stream, keys only.
    let reports = facts_collected_events(&log_path);
    assert!(
        !reports.is_empty(),
        "a request that derives task state must record what GC kept and dropped"
    );
    for report in &reports {
        let dropped = keys_of(report, "dropped");
        let retained = keys_of(report, "retained");
        assert!(
            dropped.iter().all(|key| !retained.contains(key)),
            "a key cannot be both dropped and retained: {report}"
        );
        assert!(
            !report.to_string().contains("fn "),
            "the report must carry keys, not file content: {report}"
        );
    }

    // An unreferenced fact is collected while its mention is visible.
    assert!(
        reports
            .iter()
            .any(|report| keys_of(report, "dropped").contains(&"other/drop.rs".to_string())),
        "an unreferenced fact must be dropped: {reports:?}"
    );

    // The pinned fact outlives the mention of its path.
    let last = reports.last().unwrap();
    let retained = keys_of(last, "retained");
    let dropped = keys_of(last, "dropped");
    assert!(
        retained.contains(&"keep/target.rs".to_string()),
        "the constrained fact must be retained after compaction removed its mention: {last}"
    );
    assert!(
        !dropped.contains(&"keep/target.rs".to_string()),
        "the constrained fact must never be dropped: {last}"
    );

    // Consequence, stated rather than hidden: a retained fact is part of the stamped state, so the
    // digest no longer reproduces from the transcript alone (LIFECYCLE_C0007 "still open" item 5).
    assert_ne!(
        binding["state_digest"].as_str(),
        derived.digest().as_deref(),
        "the retained fact must be visible in the stamped state, not only in the event"
    );

    let _ = fs::remove_dir_all(&root);
}

/// Every `(event_type, payload)` in the run's event log, in emission order, so a test can line a
/// boundary's invalidation verdict up against the GC report written just before it.
fn event_stream(log_path: &Path) -> Vec<(String, Value)> {
    fs::read_to_string(log_path)
        .unwrap()
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .map(|event| {
            (
                event["event_type"].as_str().unwrap().to_string(),
                event["payload"].clone(),
            )
        })
        .filter(|(kind, _)| kind.starts_with("TaskState"))
        .collect()
}

fn keys_in(payload: &Value, field: &str) -> Vec<String> {
    payload[field]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|key| key.as_str().unwrap().to_string())
        .collect()
}

/// C00-07-A: a fact records the revision it was observed at. When that revision changes, the fact is
/// invalidated at the request boundary — it is not re-stated against the new bytes — and it becomes a
/// fact again only after a *later* tool result observes the path.
///
/// The fixture is the production request path (FakeProvider, no model call): `read` observes
/// `src/app.rs`, `write` changes it, a second `read` observes it again.
#[tokio::test]
async fn request_boundary_invalidates_a_fact_whose_subject_changed_and_reestablishes_it_on_observation(
) {
    let root = temp_root("freshness");
    let ws = root.join("ws");
    let state_dir = root.join("state");
    fs::create_dir_all(ws.join("src")).unwrap();
    fs::create_dir_all(&state_dir).unwrap();
    // The content the first `read` observes; `write_turn` replaces it.
    fs::write(ws.join("src/app.rs"), "fn main() {}\n").unwrap();

    let journal = SessionJournal::create(state_dir.join("sessions"), &ws).unwrap();
    let env = Arc::new(LocalHostEnv::new(&ws, state_dir.clone()).unwrap());
    let policy = PolicyEngine::new(
        PolicyMode::AllowWrite,
        Arc::new(NonInteractiveApprovalHandler),
    );
    let budget = BudgetConfig::default();
    let runtime = build_default_runtime(env, policy, budget.clone(), None);
    let cfg = AgentRunConfig::new(TASK, budget);
    let logger = EventLogger::new(cfg.run_id, Some(state_dir.clone()), false).unwrap();
    let log_path = logger.log_path().to_path_buf();
    let (_tx, rx) = watch::channel(false);
    let provider = Arc::new(FakeProvider::new(vec![
        read_turn("call_read_1", "src/app.rs"),
        write_turn(),
        read_turn("call_read_2", "src/app.rs"),
        stop_turn(),
    ]));
    let (summary, _logger) = AgentRunner::new(cfg, provider, runtime, rx)
        .with_session_journal(journal)
        .run(logger)
        .await;
    assert_eq!(
        summary.status,
        RunStatus::Completed,
        "{:?}",
        summary.terminal_reason
    );

    let events = event_stream(&log_path);
    let reports: Vec<&Value> = events
        .iter()
        .filter(|(kind, _)| kind == "TaskStateFactsCollected")
        .map(|(_, payload)| payload)
        .collect();
    let invalidations: Vec<(usize, &Value)> = events
        .iter()
        .enumerate()
        .filter(|(_, (kind, _))| kind == "TaskStateFactsInvalidated")
        .map(|(index, (_, payload))| (index, payload))
        .collect();
    assert!(
        !invalidations.is_empty(),
        "a source change must invalidate the fact that recorded the old revision; the event stream \
         carried no invalidation: {events:?}"
    );

    for (index, payload) in &invalidations {
        let stale = keys_in(payload, "stale");
        let missing = keys_in(payload, "missing");
        let moved = keys_in(payload, "environment_moved");
        assert!(
            !stale.contains(&"src/app.rs".to_string())
                || !missing.contains(&"src/app.rs".to_string()),
            "a key cannot be in two verdict classes: {payload}"
        );
        assert!(moved.is_empty(), "no environment was recorded: {payload}");
        // The GC report written for the same boundary must not present the invalidated fact as a
        // fact: it is neither retained nor collected, because there is nothing left to collect.
        let boundary_report = events[..*index]
            .iter()
            .rev()
            .find(|(kind, _)| kind == "TaskStateFactsCollected")
            .map(|(_, payload)| payload)
            .expect("every boundary writes a GC report");
        assert!(
            !keys_of(boundary_report, "retained").contains(&"src/app.rs".to_string()),
            "an invalidated fact must not be presented as a fact in the same boundary: \
             {boundary_report}"
        );
        assert!(
            !keys_of(boundary_report, "dropped").contains(&"src/app.rs".to_string()),
            "the invalidated key must not be re-stated and then garbage-collected instead: \
             {boundary_report}"
        );
    }
    let stale_total: Vec<String> = invalidations
        .iter()
        .flat_map(|(_, payload)| keys_in(payload, "stale"))
        .collect();
    assert_eq!(
        stale_total,
        vec!["src/app.rs".to_string()],
        "exactly the changed subject must be reported stale, once: {events:?}"
    );

    // Before the change the path was a fact: it was bound when the first `read` result landed. Without
    // this the invalidation above would prove nothing.
    assert!(
        reports
            .iter()
            .any(|report| keys_of(report, "dropped").contains(&"src/app.rs".to_string())),
        "the path must have been a fact before it changed: {reports:?}"
    );
    // And after the second `read` observed the new bytes it is a fact again, which is why it is
    // collectable (nothing references `src/app.rs`) rather than still invalid.
    let last = reports.last().unwrap();
    assert!(
        keys_of(last, "dropped").contains(&"src/app.rs".to_string()),
        "a later observation must re-establish the fact, not leave it invalidated forever: {last}"
    );

    let _ = fs::remove_dir_all(&root);
}
