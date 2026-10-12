//! Gate 1 acceptance tests: workspace revision snapshots, three-state restore, WAL linkage, and
//! crash-recovery fault injection.
//!
//! Each test is named for the behaviour it pins down, so a failure names the broken invariant
//! without needing the implementation in front of you.

use harness_core::agent_loop::{build_default_runtime, AgentRunConfig, AgentRunner};
use harness_core::budget::BudgetConfig;
use harness_core::env::LocalHostEnv;
use harness_core::events::EventLogger;
use harness_core::messages::{AssistantTurn, FinishReason, ToolCall, TranscriptItem};
use harness_core::provider::FakeProvider;
use harness_core::revision::{
    ConflictKind, RestoreFault, RestoreResult, RevisionId, SnapshotStore,
};
use harness_core::session::SessionJournal;
use harness_core::tool::policy::{NonInteractiveApprovalHandler, PolicyEngine, PolicyMode};
use harness_core::types::{RunStatus, ToolCallId};
use harness_core::workspace::WorkspaceCap;
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::watch;
use uuid::Uuid;

fn temp_root(tag: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("harness-gate1-{}-{}", tag, Uuid::new_v4()));
    fs::create_dir_all(&path).unwrap();
    path
}

fn read(path: impl AsRef<std::path::Path>) -> String {
    fs::read_to_string(path).unwrap()
}

/// Every blob payload currently in the store, so a test can prove a secret never landed in one.
fn blob_contents(store: &SnapshotStore) -> Vec<String> {
    let mut payloads = Vec::new();
    let blobs = store.root().join("blobs");
    if !blobs.exists() {
        return payloads;
    }
    for shard in fs::read_dir(&blobs).unwrap() {
        let shard = shard.unwrap();
        if !shard.path().is_dir() {
            continue;
        }
        for entry in fs::read_dir(shard.path()).unwrap() {
            let entry = entry.unwrap();
            if let Ok(text) = fs::read_to_string(entry.path()) {
                payloads.push(text);
            }
        }
    }
    payloads
}

/// Test 1: capture a baseline, capture the post-mutation revision, and restore cleanly.
#[test]
fn test_capture_then_restore_applies_modified_deleted_and_created_files() {
    let root = temp_root("apply");
    let ws_dir = root.join("ws");
    fs::create_dir_all(ws_dir.join("src")).unwrap();
    fs::write(ws_dir.join("src/a.rs"), "fn a() {}\n").unwrap();
    fs::write(ws_dir.join("src/b.rs"), "fn b() {}\n").unwrap();
    fs::write(ws_dir.join("keep.txt"), "keep\n").unwrap();
    // A non-default mode must survive the round trip.
    fs::set_permissions(ws_dir.join("src/a.rs"), fs::Permissions::from_mode(0o600)).unwrap();

    let workspace = WorkspaceCap::new(&ws_dir).unwrap();
    let store = SnapshotStore::new(root.join("snapshots"));

    let baseline = store.capture_revision(&workspace, None, None, 0).unwrap();
    assert_eq!(baseline.tracked_files.len(), 3);
    assert_eq!(
        baseline.changed_paths,
        vec!["keep.txt".to_string(), "src/a.rs".to_string(), "src/b.rs".to_string()]
    );
    assert_eq!(baseline.tracked_files["src/a.rs"].mode & 0o777, 0o600);

    // One harness turn: modify a file, delete a file, create a file.
    fs::write(ws_dir.join("src/a.rs"), "fn a() { /* v2 */ }\n").unwrap();
    fs::remove_file(ws_dir.join("src/b.rs")).unwrap();
    fs::write(ws_dir.join("src/new.rs"), "fn new() {}\n").unwrap();

    let mutated = store
        .capture_revision(&workspace, Some(&baseline.revision_id), Some("wal-entry-1"), 1)
        .unwrap();
    assert_eq!(
        mutated.parent_revision_id.as_ref().map(|r| r.as_str()),
        Some(baseline.revision_id.as_str())
    );
    assert_eq!(mutated.wal_entry_id.as_deref(), Some("wal-entry-1"));
    assert_eq!(
        mutated.changed_paths,
        vec!["src/a.rs".to_string(), "src/b.rs".to_string(), "src/new.rs".to_string()]
    );
    assert_eq!(mutated.tracked_files.len(), 3);

    let result = store
        .restore_revision(
            &workspace,
            &baseline.revision_id,
            Some(&mutated.revision_id),
            false,
        )
        .unwrap();
    match result {
        RestoreResult::Applied {
            restored_revision,
            restored_files,
            removed_files,
        } => {
            assert_eq!(restored_revision, baseline.revision_id);
            assert_eq!(
                restored_files,
                vec!["keep.txt".to_string(), "src/a.rs".to_string(), "src/b.rs".to_string()]
            );
            assert_eq!(removed_files, vec!["src/new.rs".to_string()]);
        }
        other => panic!("expected RestoreResult::Applied, got {:?}", other),
    }

    assert_eq!(read(ws_dir.join("src/a.rs")), "fn a() {}\n");
    assert_eq!(read(ws_dir.join("src/b.rs")), "fn b() {}\n");
    assert_eq!(read(ws_dir.join("keep.txt")), "keep\n");
    assert!(!ws_dir.join("src/new.rs").exists());
    assert_eq!(
        fs::metadata(ws_dir.join("src/a.rs")).unwrap().permissions().mode() & 0o777,
        0o600,
        "the tracked file mode must be restored, not reset to the process default"
    );
    assert!(!store.intent_path().exists(), "a clean restore clears its intent journal");
    let _ = fs::remove_dir_all(root);
}

/// Test 2: work done outside the harness is reported, never silently clobbered.
#[test]
fn test_external_modifications_conflict_and_force_restore_overwrites() {
    let root = temp_root("conflict");
    let ws_dir = root.join("ws");
    fs::create_dir_all(ws_dir.join("src")).unwrap();
    fs::write(ws_dir.join("src/a.rs"), "v1\n").unwrap();
    fs::write(ws_dir.join("src/b.rs"), "b1\n").unwrap();
    fs::write(ws_dir.join("src/c.rs"), "c1\n").unwrap();

    let workspace = WorkspaceCap::new(&ws_dir).unwrap();
    let store = SnapshotStore::new(root.join("snapshots"));
    let baseline = store.capture_revision(&workspace, None, None, 0).unwrap();

    fs::write(ws_dir.join("src/a.rs"), "v2\n").unwrap();
    fs::remove_file(ws_dir.join("src/b.rs")).unwrap();
    fs::write(ws_dir.join("src/c.rs"), "c2\n").unwrap();
    let mutated = store
        .capture_revision(&workspace, Some(&baseline.revision_id), None, 1)
        .unwrap();

    // The user works behind the harness's back: edits `a.rs`, re-creates the deleted `b.rs`, and
    // deletes `c.rs`. A brand-new untracked file is also added, on no affected path.
    fs::write(ws_dir.join("src/a.rs"), "user edit\n").unwrap();
    fs::write(ws_dir.join("src/b.rs"), "user new\n").unwrap();
    fs::remove_file(ws_dir.join("src/c.rs")).unwrap();
    fs::write(ws_dir.join("src/untracked.rs"), "user scratch\n").unwrap();

    let conflicted = store
        .restore_revision(
            &workspace,
            &baseline.revision_id,
            Some(&mutated.revision_id),
            false,
        )
        .unwrap();
    match conflicted {
        RestoreResult::Conflict {
            target_revision,
            conflicting_paths,
        } => {
            assert_eq!(target_revision, baseline.revision_id);
            let by_path: BTreeMap<String, ConflictKind> = conflicting_paths
                .iter()
                .map(|c| (c.path.clone(), c.kind))
                .collect();
            assert_eq!(by_path.len(), 3, "got {:?}", conflicting_paths);
            assert_eq!(by_path["src/a.rs"], ConflictKind::ModifiedExternally);
            assert_eq!(by_path["src/b.rs"], ConflictKind::CreatedExternally);
            assert_eq!(by_path["src/c.rs"], ConflictKind::DeletedExternally);
            assert!(
                !by_path.contains_key("src/untracked.rs"),
                "a file that was never part of either revision is not a conflict"
            );
        }
        other => panic!("expected RestoreResult::Conflict, got {:?}", other),
    }

    assert_eq!(read(ws_dir.join("src/a.rs")), "user edit\n");
    assert_eq!(read(ws_dir.join("src/b.rs")), "user new\n");
    assert!(!ws_dir.join("src/c.rs").exists());
    assert!(!store.intent_path().exists(), "a refused restore must not journal anything");

    let forced = store
        .restore_revision(
            &workspace,
            &baseline.revision_id,
            Some(&mutated.revision_id),
            true,
        )
        .unwrap();
    assert!(
        matches!(forced, RestoreResult::Applied { .. }),
        "expected Applied with force=true, got {:?}",
        forced
    );
    assert_eq!(read(ws_dir.join("src/a.rs")), "v1\n");
    assert_eq!(read(ws_dir.join("src/b.rs")), "b1\n");
    assert_eq!(read(ws_dir.join("src/c.rs")), "c1\n");
    assert_eq!(
        read(ws_dir.join("src/untracked.rs")),
        "user scratch\n",
        "a forced restore only touches paths tracked by the target or expected revision"
    );
    let _ = fs::remove_dir_all(root);
}

/// Test 3: an interrupted restore is always detectable and always recoverable.
#[test]
fn test_interrupted_restore_requires_recovery_then_recovers_to_a_consistent_state() {
    let root = temp_root("fault");
    let ws_dir = root.join("ws");
    fs::create_dir_all(ws_dir.join("src")).unwrap();
    for name in ["a.rs", "b.rs", "c.rs"] {
        fs::write(ws_dir.join("src").join(name), "v1\n").unwrap();
    }

    let workspace = WorkspaceCap::new(&ws_dir).unwrap();
    let store = SnapshotStore::new(root.join("snapshots"));
    let baseline = store.capture_revision(&workspace, None, None, 0).unwrap();
    for name in ["a.rs", "b.rs", "c.rs"] {
        fs::write(ws_dir.join("src").join(name), "v2\n").unwrap();
    }
    let mutated = store
        .capture_revision(&workspace, Some(&baseline.revision_id), None, 1)
        .unwrap();

    // Crash right after the intent journal was written, before any file changed.
    let early = store
        .restore_revision_with_fault(
            &workspace,
            &baseline.revision_id,
            Some(&mutated.revision_id),
            true,
            Some(RestoreFault::AfterIntentWritten),
        )
        .unwrap();
    match early {
        RestoreResult::RecoveryRequired { journal_path, reason, .. } => {
            assert!(journal_path.exists(), "the intent journal must survive the crash");
            assert!(reason.contains("fault injection"), "reason was: {}", reason);
        }
        other => panic!("expected RestoreResult::RecoveryRequired, got {:?}", other),
    }
    assert!(store.pending_restore().unwrap().is_some());
    for name in ["a.rs", "b.rs", "c.rs"] {
        assert_eq!(read(ws_dir.join("src").join(name)), "v2\n", "no file may change before the journal is cleared");
    }

    // A second restore is refused while the journal is outstanding.
    let blocked = store
        .restore_revision(
            &workspace,
            &baseline.revision_id,
            Some(&mutated.revision_id),
            true,
        )
        .unwrap();
    assert!(
        matches!(blocked, RestoreResult::RecoveryRequired { .. }),
        "expected RecoveryRequired while a journal is pending, got {:?}",
        blocked
    );

    let recovered = store.recover_pending_restore(&workspace).unwrap();
    match recovered {
        RestoreResult::Applied { restored_revision, restored_files, .. } => {
            assert_eq!(restored_revision, baseline.revision_id);
            assert_eq!(restored_files.len(), 3);
        }
        other => panic!("expected Applied after recovery, got {:?}", other),
    }
    for name in ["a.rs", "b.rs", "c.rs"] {
        assert_eq!(read(ws_dir.join("src").join(name)), "v1\n");
    }
    assert!(!store.intent_path().exists(), "recovery clears the intent journal");
    assert!(store.pending_restore().unwrap().is_none());

    // Now crash midway through the file mutations.
    let midway = store
        .restore_revision_with_fault(
            &workspace,
            &mutated.revision_id,
            Some(&baseline.revision_id),
            true,
            Some(RestoreFault::AfterAppliedFiles(1)),
        )
        .unwrap();
    match midway {
        RestoreResult::RecoveryRequired { journal_path, reason, .. } => {
            assert!(journal_path.exists());
            assert!(reason.contains("1 of 3"), "reason was: {}", reason);
        }
        other => panic!("expected RestoreResult::RecoveryRequired, got {:?}", other),
    }

    let recovered = store.recover_pending_restore(&workspace).unwrap();
    match recovered {
        RestoreResult::Applied { restored_revision, .. } => {
            assert_eq!(restored_revision, mutated.revision_id);
        }
        other => panic!("expected Applied after recovery, got {:?}", other),
    }
    for name in ["a.rs", "b.rs", "c.rs"] {
        assert_eq!(
            read(ws_dir.join("src").join(name)),
            "v2\n",
            "recovery must finish the interrupted restore, not leave a half-applied tree"
        );
    }
    assert!(!store.intent_path().exists());
    let _ = fs::remove_dir_all(root);
}

/// Test 4: end-to-end `AgentLoop` + `SessionJournal` — the revision id is persisted in the WAL and
/// `--undo` rewinds both the transcript leaf and the workspace files.
#[tokio::test]
async fn test_agent_loop_persists_revision_ids_and_undo_rewinds_wal_and_workspace() {
    let root = temp_root("e2e");
    let ws_dir = root.join("ws");
    let state_dir = root.join("state");
    fs::create_dir_all(ws_dir.join("src")).unwrap();
    fs::create_dir_all(&state_dir).unwrap();
    fs::write(ws_dir.join("src/app.rs"), "fn main() {}\n").unwrap();

    let store = Arc::new(SnapshotStore::new(root.join("snapshots")));
    let journal = SessionJournal::create(state_dir.join("sessions"), &ws_dir).unwrap();
    let wal_path = journal.path().to_path_buf();

    let provider = Arc::new(FakeProvider::new(vec![
        AssistantTurn {
            content: None,
            reasoning_blocks: vec![],
            tool_calls: vec![ToolCall {
                call_id: ToolCallId::new("call_write"),
                name: "write".into(),
                raw_arguments: r#"{"path":"src/app.rs","content":"fn main() { println!(\"hi\"); }\n"}"#.into(),
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
    let policy = PolicyEngine::new(PolicyMode::AllowWrite, Arc::new(NonInteractiveApprovalHandler));
    let budget = BudgetConfig::default();
    let runtime = build_default_runtime(env, policy, budget.clone(), None);
    let cfg = AgentRunConfig::new("update the greeting", budget);
    let logger = EventLogger::new(cfg.run_id, Some(state_dir.clone()), false).unwrap();
    let (_tx, rx) = watch::channel(false);
    let (summary, _logger) = AgentRunner::new(cfg, provider, runtime, rx)
        .with_session_journal(journal)
        .with_snapshot_store(store.clone())
        .run(logger)
        .await;

    assert_eq!(summary.status, RunStatus::Completed, "{:?}", summary.terminal_reason);
    assert_eq!(read(ws_dir.join("src/app.rs")), "fn main() { println!(\"hi\"); }\n");

    let mut journal = SessionJournal::open(&wal_path).unwrap();
    let records = journal.revision_records();
    assert_eq!(records.len(), 2, "expected a baseline and one mutation revision: {:?}", records);
    assert_ne!(records[0].revision_id, records[1].revision_id);
    assert_eq!(records[0].turn, 0);
    assert_eq!(records[1].turn, 1);
    assert!(records[1].leaf_id.is_some());
    assert_eq!(records[1].changed_paths, vec!["src/app.rs".to_string()]);
    assert_eq!(
        journal.latest_revision_id().as_deref(),
        Some(records[1].revision_id.as_str())
    );

    // The turn checkpoint carries the revision id too (the `serde(default)` field is Gate 1's
    // backward-compatible WAL extension).
    let checkpoint = journal
        .latest_active_checkpoint()
        .cloned()
        .expect("a checkpoint must have been persisted");
    assert_eq!(
        checkpoint.workspace_revision_id.as_deref(),
        Some(records[1].revision_id.as_str())
    );

    // `--undo`: restore the previous revision and rewind the session leaf to that turn.
    let target = RevisionId::parse(&records[0].revision_id).unwrap();
    let expected = RevisionId::parse(&records[1].revision_id).unwrap();
    let workspace = WorkspaceCap::new(&ws_dir).unwrap();
    let restored = store
        .restore_revision(&workspace, &target, Some(&expected), false)
        .unwrap();
    assert!(
        matches!(restored, RestoreResult::Applied { .. }),
        "undo must apply cleanly: {:?}",
        restored
    );
    assert_eq!(read(ws_dir.join("src/app.rs")), "fn main() {}\n");
    journal.fork_at_turn(records[0].turn).unwrap();
    drop(journal);

    let reopened = SessionJournal::open(&wal_path).unwrap();
    let transcript = reopened.active_transcript();
    assert_eq!(transcript.len(), 1, "the rewound transcript keeps only the original task");
    assert!(matches!(&transcript[0], TranscriptItem::User(u) if u.content == "update the greeting"));
    assert_eq!(
        reopened.revision_records().len(),
        1,
        "only the baseline revision remains on the rewound branch"
    );
    let _ = fs::remove_dir_all(root);
}

/// Test 5: symlinks, sensitive files, and build output are never captured, and a symlink swapped
/// over a tracked path is refused rather than followed.
#[test]
fn test_symlinks_and_sensitive_files_are_never_captured_or_overwritten() {
    let root = temp_root("exclude");
    let ws_dir = root.join("ws");
    let outside_dir = root.join("outside");
    fs::create_dir_all(ws_dir.join("node_modules")).unwrap();
    fs::create_dir_all(ws_dir.join("target")).unwrap();
    fs::create_dir_all(&outside_dir).unwrap();

    fs::write(ws_dir.join("real.txt"), "real v1\n").unwrap();
    fs::write(ws_dir.join(".env"), "DB_PASSWORD=SUPER_SECRET_VALUE\n").unwrap();
    fs::write(ws_dir.join("node_modules/dep.js"), "dep\n").unwrap();
    fs::write(ws_dir.join("target/build.bin"), "bin\n").unwrap();
    fs::write(outside_dir.join("secret.txt"), "OUTSIDE_SECRET\n").unwrap();
    std::os::unix::fs::symlink(outside_dir.join("secret.txt"), ws_dir.join("escape.txt")).unwrap();

    let workspace = WorkspaceCap::new(&ws_dir).unwrap();
    let store = SnapshotStore::new(root.join("snapshots"));
    let baseline = store.capture_revision(&workspace, None, None, 0).unwrap();

    assert!(baseline.tracked_files.contains_key("real.txt"));
    for excluded in [".env", "escape.txt", "node_modules/dep.js", "target/build.bin"] {
        assert!(
            !baseline.tracked_files.contains_key(excluded),
            "{} must never be tracked: {:?}",
            excluded,
            baseline.tracked_files.keys().collect::<Vec<_>>()
        );
    }
    for skipped in [".env", "escape.txt", "node_modules", "target"] {
        assert!(
            baseline.skipped_paths.iter().any(|p| p == skipped),
            "{} must be reported in skipped_paths: {:?}",
            skipped,
            baseline.skipped_paths
        );
    }
    let blobs = blob_contents(&store);
    assert!(!blobs.is_empty(), "the tracked file must have produced a blob");
    assert!(
        blobs.iter().all(|body| !body.contains("SUPER_SECRET_VALUE")),
        "a sensitive file's contents must never reach the snapshot store"
    );

    // Swap the tracked file for a symlink pointing outside the workspace.
    fs::remove_file(ws_dir.join("real.txt")).unwrap();
    std::os::unix::fs::symlink(outside_dir.join("secret.txt"), ws_dir.join("real.txt")).unwrap();

    let refused = store
        .restore_revision(&workspace, &baseline.revision_id, Some(&baseline.revision_id), false)
        .unwrap();
    match refused {
        RestoreResult::Conflict { conflicting_paths, .. } => {
            assert_eq!(conflicting_paths.len(), 1, "got {:?}", conflicting_paths);
            assert_eq!(conflicting_paths[0].path, "real.txt");
            assert_eq!(conflicting_paths[0].kind, ConflictKind::DeletedExternally);
        }
        other => panic!("expected RestoreResult::Conflict, got {:?}", other),
    }
    assert!(
        fs::symlink_metadata(ws_dir.join("real.txt"))
            .unwrap()
            .file_type()
            .is_symlink(),
        "the restore must not replace the symlink"
    );
    assert_eq!(read(outside_dir.join("secret.txt")), "OUTSIDE_SECRET\n");
    let _ = fs::remove_dir_all(root);
}
