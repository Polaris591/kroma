//! Gate 1 acceptance tests: workspace revision snapshots, three-state restore, WAL linkage, and
//! crash-recovery fault injection.
//!
//! Each test is named for the behaviour it pins down, so a failure names the broken invariant
//! without needing the implementation in front of you.

use kroma_core::agent_loop::{build_default_runtime, AgentRunConfig, AgentRunner};
use kroma_core::budget::BudgetConfig;
use kroma_core::env::LocalHostEnv;
use kroma_core::events::EventLogger;
use kroma_core::messages::{AssistantTurn, FinishReason, ToolCall, TranscriptItem, UserMessage};
use kroma_core::provider::FakeProvider;
use kroma_core::revision::{
    ConflictKind, ForkIntent, ForkRecoveryOutcome, RestoreFault, RestoreResult, RevisionId,
    SnapshotStore,
};
use kroma_core::session::SessionJournal;
use kroma_core::tool::policy::{NonInteractiveApprovalHandler, PolicyEngine, PolicyMode};
use kroma_core::types::{RunStatus, ToolCallId};
use kroma_core::workspace::WorkspaceCap;
use std::collections::BTreeMap;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::watch;
use uuid::Uuid;

fn create_symlink_file(src: impl AsRef<Path>, dst: impl AsRef<Path>) {
    #[cfg(unix)]
    std::os::unix::fs::symlink(src, dst).unwrap();
    #[cfg(windows)]
    std::os::windows::fs::symlink_file(src, dst).unwrap();
}

fn set_file_mode(path: impl AsRef<Path>, mode: u32) {
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    #[cfg(windows)]
    {
        let mut perms = fs::metadata(path.as_ref()).unwrap().permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        perms.set_readonly((mode & 0o222) == 0);
        fs::set_permissions(path, perms).unwrap();
    }
}

fn get_file_mode(path: impl AsRef<Path>) -> u32 {
    #[cfg(unix)]
    {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }
    #[cfg(windows)]
    {
        if fs::metadata(path).unwrap().permissions().readonly() {
            0o444
        } else {
            0o600
        }
    }
}

fn temp_root(tag: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("kroma-gate1-{}-{}", tag, Uuid::new_v4()));
    fs::create_dir_all(&path).unwrap();
    path
}

fn read(path: impl AsRef<std::path::Path>) -> String {
    fs::read_to_string(path).unwrap()
}

/// Capture the current workspace and record it on the active session branch.
fn capture_and_record(
    store: &SnapshotStore,
    journal: &mut SessionJournal,
    workspace: &WorkspaceCap,
    parent: Option<&RevisionId>,
    turn: usize,
) -> RevisionId {
    let revision = store
        .capture_revision(workspace, parent, None, turn)
        .unwrap();
    journal
        .record_revision_capture(
            turn,
            revision.revision_id.as_str(),
            parent.map(|p| p.as_str()),
            revision.changed_paths.clone(),
            revision.tracked_files.len(),
        )
        .unwrap();
    revision.revision_id
}

/// The exact selection `kroma --undo` performs: restore the previous revision the workspace
/// actually differed from, then rewind the transcript leaf to that turn.
fn undo_once(
    store: &SnapshotStore,
    journal: &mut SessionJournal,
    workspace: &WorkspaceCap,
) -> RestoreResult {
    let records = journal.revision_records();
    let current = records.last().expect("a revision must be recorded").clone();
    let previous = records
        .iter()
        .rev()
        .find(|record| record.revision_id != current.revision_id)
        .expect("an earlier revision must exist")
        .clone();
    let target = RevisionId::parse(&previous.revision_id).unwrap();
    let expected = RevisionId::parse(&current.revision_id).unwrap();
    let result = store
        .restore_revision(workspace, &target, Some(&expected), false)
        .unwrap();
    if matches!(result, RestoreResult::Applied { .. }) {
        journal.fork_at_turn(previous.turn).unwrap();
    }
    result
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
    set_file_mode(ws_dir.join("src/a.rs"), 0o600);

    let workspace = WorkspaceCap::new(&ws_dir).unwrap();
    let store = SnapshotStore::new(root.join("snapshots"));

    let baseline = store.capture_revision(&workspace, None, None, 0).unwrap();
    assert_eq!(baseline.tracked_files.len(), 3);
    assert_eq!(
        baseline.changed_paths,
        vec![
            "keep.txt".to_string(),
            "src/a.rs".to_string(),
            "src/b.rs".to_string()
        ]
    );
    assert_eq!(baseline.tracked_files["src/a.rs"].mode & 0o777, 0o600);

    // One kroma turn: modify a file, delete a file, create a file.
    fs::write(ws_dir.join("src/a.rs"), "fn a() { /* v2 */ }\n").unwrap();
    fs::remove_file(ws_dir.join("src/b.rs")).unwrap();
    fs::write(ws_dir.join("src/new.rs"), "fn new() {}\n").unwrap();

    let mutated = store
        .capture_revision(
            &workspace,
            Some(&baseline.revision_id),
            Some("wal-entry-1"),
            1,
        )
        .unwrap();
    assert_eq!(
        mutated.parent_revision_id.as_ref().map(|r| r.as_str()),
        Some(baseline.revision_id.as_str())
    );
    assert_eq!(mutated.wal_entry_id.as_deref(), Some("wal-entry-1"));
    assert_eq!(
        mutated.changed_paths,
        vec![
            "src/a.rs".to_string(),
            "src/b.rs".to_string(),
            "src/new.rs".to_string()
        ]
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
                vec![
                    "keep.txt".to_string(),
                    "src/a.rs".to_string(),
                    "src/b.rs".to_string()
                ]
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
        get_file_mode(ws_dir.join("src/a.rs")),
        0o600,
        "the tracked file mode must be restored, not reset to the process default"
    );
    assert!(
        !store.intent_path().exists(),
        "a clean restore clears its intent journal"
    );
    let _ = fs::remove_dir_all(root);
}

/// Test 2: work done outside the kroma is reported, never silently clobbered.
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

    // The user works behind the kroma's back: edits `a.rs`, re-creates the deleted `b.rs`, and
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
    assert!(
        !store.intent_path().exists(),
        "a refused restore must not journal anything"
    );

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
        RestoreResult::RecoveryRequired {
            journal_path,
            reason,
            ..
        } => {
            assert!(
                journal_path.exists(),
                "the intent journal must survive the crash"
            );
            assert!(reason.contains("fault injection"), "reason was: {}", reason);
        }
        other => panic!("expected RestoreResult::RecoveryRequired, got {:?}", other),
    }
    assert!(store.pending_restore().unwrap().is_some());
    for name in ["a.rs", "b.rs", "c.rs"] {
        assert_eq!(
            read(ws_dir.join("src").join(name)),
            "v2\n",
            "no file may change before the journal is cleared"
        );
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
        RestoreResult::Applied {
            restored_revision,
            restored_files,
            ..
        } => {
            assert_eq!(restored_revision, baseline.revision_id);
            assert_eq!(restored_files.len(), 3);
        }
        other => panic!("expected Applied after recovery, got {:?}", other),
    }
    for name in ["a.rs", "b.rs", "c.rs"] {
        assert_eq!(read(ws_dir.join("src").join(name)), "v1\n");
    }
    assert!(
        !store.intent_path().exists(),
        "recovery clears the intent journal"
    );
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
        RestoreResult::RecoveryRequired {
            journal_path,
            reason,
            ..
        } => {
            assert!(journal_path.exists());
            assert!(reason.contains("1 of 3"), "reason was: {}", reason);
        }
        other => panic!("expected RestoreResult::RecoveryRequired, got {:?}", other),
    }

    let recovered = store.recover_pending_restore(&workspace).unwrap();
    match recovered {
        RestoreResult::Applied {
            restored_revision, ..
        } => {
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
                raw_arguments:
                    r#"{"path":"src/app.rs","content":"fn main() { println!(\"hi\"); }\n"}"#.into(),
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
        PolicyMode::AllowWrite,
        Arc::new(NonInteractiveApprovalHandler),
    );
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

    assert_eq!(
        summary.status,
        RunStatus::Completed,
        "{:?}",
        summary.terminal_reason
    );
    assert_eq!(
        read(ws_dir.join("src/app.rs")),
        "fn main() { println!(\"hi\"); }\n"
    );

    let mut journal = SessionJournal::open(&wal_path).unwrap();
    let records = journal.revision_records();
    assert_eq!(
        records.len(),
        2,
        "expected a baseline and one mutation revision: {:?}",
        records
    );
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
    assert_eq!(
        transcript.len(),
        1,
        "the rewound transcript keeps only the original task"
    );
    assert!(
        matches!(&transcript[0], TranscriptItem::User(u) if u.content == "update the greeting")
    );
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
    create_symlink_file(outside_dir.join("secret.txt"), ws_dir.join("escape.txt"));

    let workspace = WorkspaceCap::new(&ws_dir).unwrap();
    let store = SnapshotStore::new(root.join("snapshots"));
    let baseline = store.capture_revision(&workspace, None, None, 0).unwrap();

    assert!(baseline.tracked_files.contains_key("real.txt"));
    for excluded in [
        ".env",
        "escape.txt",
        "node_modules/dep.js",
        "target/build.bin",
    ] {
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
    assert!(
        !blobs.is_empty(),
        "the tracked file must have produced a blob"
    );
    assert!(
        blobs
            .iter()
            .all(|body| !body.contains("SUPER_SECRET_VALUE")),
        "a sensitive file's contents must never reach the snapshot store"
    );

    // Swap the tracked file for a symlink pointing outside the workspace.
    fs::remove_file(ws_dir.join("real.txt")).unwrap();
    create_symlink_file(outside_dir.join("secret.txt"), ws_dir.join("real.txt"));

    let refused = store
        .restore_revision(
            &workspace,
            &baseline.revision_id,
            Some(&baseline.revision_id),
            false,
        )
        .unwrap();
    match refused {
        RestoreResult::Conflict {
            conflicting_paths, ..
        } => {
            assert_eq!(conflicting_paths.len(), 1, "got {:?}", conflicting_paths);
            assert_eq!(conflicting_paths[0].path, "real.txt");
            // A symlink parked on a tracked path is a modification of that path (it is neither the
            // tracked regular file nor an absence of it), so it must be reported as such.
            assert_eq!(conflicting_paths[0].kind, ConflictKind::ModifiedExternally);
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

/// Test 6: `--undo` is repeatable across three mutating turns (T3 -> T2 -> T1 -> T0), rewinding
/// both the workspace files and the session branch at each step.
#[test]
fn test_sequential_undo_across_three_mutating_turns_rewinds_to_the_baseline() {
    let root = temp_root("sequential-undo");
    let ws_dir = root.join("ws");
    fs::create_dir_all(&ws_dir).unwrap();
    fs::write(ws_dir.join("f.txt"), "v0\n").unwrap();

    let workspace = WorkspaceCap::new(&ws_dir).unwrap();
    let store = SnapshotStore::new(root.join("snapshots"));
    let mut journal = SessionJournal::create(root.join("sessions"), &ws_dir).unwrap();

    journal
        .append_item(
            0,
            TranscriptItem::User(UserMessage {
                content: "task".into(),
            }),
        )
        .unwrap();
    let r0 = capture_and_record(&store, &mut journal, &workspace, None, 0);

    let mut parent = r0;
    for turn in 1..=3usize {
        fs::write(ws_dir.join("f.txt"), format!("v{}\n", turn)).unwrap();
        journal
            .append_item(
                turn,
                TranscriptItem::User(UserMessage {
                    content: format!("turn {}", turn),
                }),
            )
            .unwrap();
        parent = capture_and_record(&store, &mut journal, &workspace, Some(&parent), turn);
    }
    assert_eq!(journal.revision_records().len(), 4);

    for (remaining, expected_body) in [(3usize, "v2\n"), (2, "v1\n"), (1, "v0\n")] {
        let result = undo_once(&store, &mut journal, &workspace);
        assert!(
            matches!(result, RestoreResult::Applied { .. }),
            "undo must apply cleanly: {:?}",
            result
        );
        assert_eq!(read(ws_dir.join("f.txt")), expected_body);
        assert_eq!(
            journal.revision_records().len(),
            remaining,
            "each undo must drop exactly the abandoned branch's revision"
        );
        assert_eq!(
            journal.latest_revision_id().as_deref(),
            journal
                .revision_records()
                .last()
                .map(|r| r.revision_id.as_str()),
            "latest_revision_id must track the active branch"
        );
    }
    assert_eq!(read(ws_dir.join("f.txt")), "v0\n");
    assert_eq!(journal.revision_records().len(), 1);
    let _ = fs::remove_dir_all(root);
}

/// Test 7: forking at an earlier turn, mutating on the new branch, and restoring a revision that
/// lives on the abandoned branch — revisions are content-addressed, so cross-branch restore works.
#[test]
fn test_fork_then_new_mutation_and_cross_branch_restore() {
    let root = temp_root("fork-branch");
    let ws_dir = root.join("ws");
    fs::create_dir_all(&ws_dir).unwrap();
    fs::write(ws_dir.join("f.txt"), "v0\n").unwrap();

    let workspace = WorkspaceCap::new(&ws_dir).unwrap();
    let store = SnapshotStore::new(root.join("snapshots"));
    let mut journal = SessionJournal::create(root.join("sessions"), &ws_dir).unwrap();

    journal
        .append_item(
            0,
            TranscriptItem::User(UserMessage {
                content: "task".into(),
            }),
        )
        .unwrap();
    let r0 = capture_and_record(&store, &mut journal, &workspace, None, 0);
    fs::write(ws_dir.join("f.txt"), "v1\n").unwrap();
    journal
        .append_item(
            1,
            TranscriptItem::User(UserMessage {
                content: "turn 1".into(),
            }),
        )
        .unwrap();
    let r1 = capture_and_record(&store, &mut journal, &workspace, Some(&r0), 1);
    fs::write(ws_dir.join("f.txt"), "v2\n").unwrap();
    journal
        .append_item(
            2,
            TranscriptItem::User(UserMessage {
                content: "turn 2".into(),
            }),
        )
        .unwrap();
    let r2 = capture_and_record(&store, &mut journal, &workspace, Some(&r1), 2);
    assert_eq!(journal.revision_records().len(), 3);

    // Fork back to turn 1 and mutate on the new branch.
    journal.fork_at_turn(1).unwrap();
    fs::write(ws_dir.join("f.txt"), "v2-branch\n").unwrap();
    journal
        .append_item(
            2,
            TranscriptItem::User(UserMessage {
                content: "turn 2 branch".into(),
            }),
        )
        .unwrap();
    let r2b = capture_and_record(&store, &mut journal, &workspace, Some(&r1), 2);
    let records = journal.revision_records();
    assert_eq!(
        records.len(),
        3,
        "the abandoned branch's revision must be filtered out"
    );
    assert!(!records.iter().any(|r| r.revision_id == r2.as_str()));
    assert_eq!(journal.latest_revision_id().as_deref(), Some(r2b.as_str()));

    // Restore a revision captured on the abandoned branch.
    let restored = store
        .restore_revision(&workspace, &r2, Some(&r2b), false)
        .unwrap();
    assert!(
        matches!(restored, RestoreResult::Applied { .. }),
        "a revision from another branch must still restore: {:?}",
        restored
    );
    assert_eq!(read(ws_dir.join("f.txt")), "v2\n");
    let _ = fs::remove_dir_all(root);
}

/// Test 8: undoing a file created inside a brand-new nested directory tree removes the file and
/// prunes the now-empty `a`, `a/b`, `a/b/c` directories.
#[test]
fn test_nested_new_directory_tree_is_pruned_on_undo() {
    let root = temp_root("nested");
    let ws_dir = root.join("ws");
    fs::create_dir_all(&ws_dir).unwrap();
    fs::write(ws_dir.join("keep.txt"), "keep\n").unwrap();

    let workspace = WorkspaceCap::new(&ws_dir).unwrap();
    let store = SnapshotStore::new(root.join("snapshots"));
    let baseline = store.capture_revision(&workspace, None, None, 0).unwrap();

    fs::create_dir_all(ws_dir.join("a/b/c")).unwrap();
    fs::write(ws_dir.join("a/b/c/new.rs"), "fn new() {}\n").unwrap();
    let mutated = store
        .capture_revision(&workspace, Some(&baseline.revision_id), None, 1)
        .unwrap();

    let restored = store
        .restore_revision(
            &workspace,
            &baseline.revision_id,
            Some(&mutated.revision_id),
            false,
        )
        .unwrap();
    match restored {
        RestoreResult::Applied { removed_files, .. } => {
            assert_eq!(removed_files, vec!["a/b/c/new.rs".to_string()]);
        }
        other => panic!("expected Applied, got {:?}", other),
    }
    assert!(!ws_dir.join("a/b/c/new.rs").exists());
    for dir in ["a/b/c", "a/b", "a"] {
        assert!(
            !ws_dir.join(dir).exists(),
            "the newly created directory {} must be pruned when it becomes empty",
            dir
        );
    }
    assert_eq!(read(ws_dir.join("keep.txt")), "keep\n");
    let _ = fs::remove_dir_all(root);
}

/// Test 9: a path that changed kind between revisions (file <-> directory) restores cleanly in
/// both directions instead of wedging in `RecoveryRequired`.
#[test]
fn test_file_and_directory_kind_swap_restores_in_both_directions() {
    let root = temp_root("kind-swap");
    let ws_dir = root.join("ws");
    fs::create_dir_all(&ws_dir).unwrap();
    fs::write(ws_dir.join("foo"), "foo v1\n").unwrap();

    let workspace = WorkspaceCap::new(&ws_dir).unwrap();
    let store = SnapshotStore::new(root.join("snapshots"));
    let file_rev = store.capture_revision(&workspace, None, None, 0).unwrap();

    // file -> directory
    fs::remove_file(ws_dir.join("foo")).unwrap();
    fs::create_dir_all(ws_dir.join("foo")).unwrap();
    fs::write(ws_dir.join("foo/bar.txt"), "bar\n").unwrap();
    let dir_rev = store
        .capture_revision(&workspace, Some(&file_rev.revision_id), None, 1)
        .unwrap();

    // Undo back to the file revision.
    let back_to_file = store
        .restore_revision(
            &workspace,
            &file_rev.revision_id,
            Some(&dir_rev.revision_id),
            false,
        )
        .unwrap();
    assert!(
        matches!(back_to_file, RestoreResult::Applied { .. }),
        "directory -> file must not wedge: {:?}",
        back_to_file
    );
    assert!(fs::symlink_metadata(ws_dir.join("foo"))
        .unwrap()
        .file_type()
        .is_file());
    assert_eq!(read(ws_dir.join("foo")), "foo v1\n");
    assert!(!ws_dir.join("foo/bar.txt").exists());

    // And forward again to the directory revision.
    let back_to_dir = store
        .restore_revision(
            &workspace,
            &dir_rev.revision_id,
            Some(&file_rev.revision_id),
            false,
        )
        .unwrap();
    assert!(
        matches!(back_to_dir, RestoreResult::Applied { .. }),
        "file -> directory must not wedge: {:?}",
        back_to_dir
    );
    assert!(fs::symlink_metadata(ws_dir.join("foo"))
        .unwrap()
        .file_type()
        .is_dir());
    assert_eq!(read(ws_dir.join("foo/bar.txt")), "bar\n");
    let _ = fs::remove_dir_all(root);
}

/// Test 10: an external symlink or permission change on a tracked path is a conflict, and a symlink
/// created at a path the target wants is never silently replaced.
#[test]
fn test_external_symlink_and_mode_change_are_conflicts() {
    let root = temp_root("external-kind");
    let ws_dir = root.join("ws");
    let outside_dir = root.join("outside");
    fs::create_dir_all(&ws_dir).unwrap();
    fs::create_dir_all(&outside_dir).unwrap();
    fs::write(outside_dir.join("secret.txt"), "OUTSIDE_SECRET\n").unwrap();
    fs::write(ws_dir.join("x.rs"), "x v1\n").unwrap();
    fs::write(ws_dir.join("g.rs"), "g v1\n").unwrap();
    set_file_mode(ws_dir.join("x.rs"), 0o644);

    let workspace = WorkspaceCap::new(&ws_dir).unwrap();
    let store = SnapshotStore::new(root.join("snapshots"));
    let baseline = store.capture_revision(&workspace, None, None, 0).unwrap();

    fs::write(ws_dir.join("x.rs"), "x v2\n").unwrap();
    fs::remove_file(ws_dir.join("g.rs")).unwrap();
    let mutated = store
        .capture_revision(&workspace, Some(&baseline.revision_id), None, 1)
        .unwrap();

    // External chmod: same bytes, different mode -> ModifiedExternally.
    set_file_mode(ws_dir.join("x.rs"), 0o600);
    let mode_conflict = store
        .restore_revision(
            &workspace,
            &baseline.revision_id,
            Some(&mutated.revision_id),
            false,
        )
        .unwrap();
    match mode_conflict {
        RestoreResult::Conflict {
            conflicting_paths, ..
        } => {
            assert_eq!(conflicting_paths.len(), 1, "got {:?}", conflicting_paths);
            assert_eq!(conflicting_paths[0].path, "x.rs");
            assert_eq!(conflicting_paths[0].kind, ConflictKind::ModifiedExternally);
        }
        other => panic!("expected Conflict for a mode change, got {:?}", other),
    }
    set_file_mode(ws_dir.join("x.rs"), 0o644);

    // External symlink at a path the baseline wants to restore (only in target).
    create_symlink_file(outside_dir.join("secret.txt"), ws_dir.join("g.rs"));
    let link_conflict = store
        .restore_revision(
            &workspace,
            &baseline.revision_id,
            Some(&mutated.revision_id),
            false,
        )
        .unwrap();
    match link_conflict {
        RestoreResult::Conflict {
            conflicting_paths, ..
        } => {
            assert_eq!(conflicting_paths.len(), 1, "got {:?}", conflicting_paths);
            assert_eq!(conflicting_paths[0].path, "g.rs");
            assert_eq!(conflicting_paths[0].kind, ConflictKind::CreatedExternally);
        }
        other => panic!("expected Conflict for a created symlink, got {:?}", other),
    }
    assert!(
        fs::symlink_metadata(ws_dir.join("g.rs"))
            .unwrap()
            .file_type()
            .is_symlink(),
        "a refused restore must not touch the user's symlink"
    );
    assert_eq!(read(outside_dir.join("secret.txt")), "OUTSIDE_SECRET\n");

    // External symlink swapped onto a tracked path that the expected revision has -> ModifiedExternally.
    fs::remove_file(ws_dir.join("x.rs")).unwrap();
    create_symlink_file(outside_dir.join("secret.txt"), ws_dir.join("x.rs"));
    let swap_conflict = store
        .restore_revision(
            &workspace,
            &baseline.revision_id,
            Some(&mutated.revision_id),
            false,
        )
        .unwrap();
    match swap_conflict {
        RestoreResult::Conflict {
            conflicting_paths, ..
        } => {
            let by_path: BTreeMap<&str, ConflictKind> = conflicting_paths
                .iter()
                .map(|c| (c.path.as_str(), c.kind))
                .collect();
            assert_eq!(by_path["x.rs"], ConflictKind::ModifiedExternally);
            assert_eq!(by_path["g.rs"], ConflictKind::CreatedExternally);
        }
        other => panic!("expected Conflict for a swapped symlink, got {:?}", other),
    }
    assert_eq!(read(outside_dir.join("secret.txt")), "OUTSIDE_SECRET\n");
    let _ = fs::remove_dir_all(root);
}

/// Test 11: a file the target revision deliberately skipped (grew past the snapshot cap) is never
/// deleted when restoring to that revision.
#[test]
fn test_skipped_large_file_is_preserved_on_restore() {
    let root = temp_root("skipped-large");
    let ws_dir = root.join("ws");
    fs::create_dir_all(&ws_dir).unwrap();
    fs::write(ws_dir.join("small.txt"), "small\n").unwrap();
    fs::write(ws_dir.join("big.bin"), "a".repeat(1024)).unwrap();

    let workspace = WorkspaceCap::new(&ws_dir).unwrap();
    let store = SnapshotStore::new(root.join("snapshots"));
    let baseline = store.capture_revision(&workspace, None, None, 0).unwrap();
    assert!(baseline.tracked_files.contains_key("big.bin"));

    let big_body = "b".repeat(5 * 1024 * 1024);
    fs::write(ws_dir.join("big.bin"), &big_body).unwrap();
    let grown = store
        .capture_revision(&workspace, Some(&baseline.revision_id), None, 1)
        .unwrap();
    assert!(
        grown.skipped_paths.iter().any(|p| p == "big.bin"),
        "the oversized file must be recorded as skipped: {:?}",
        grown.skipped_paths
    );
    assert!(!grown.tracked_files.contains_key("big.bin"));

    // Restoring to the grown revision must not delete the file it could not track.
    let restored = store
        .restore_revision(
            &workspace,
            &grown.revision_id,
            Some(&baseline.revision_id),
            false,
        )
        .unwrap();
    match restored {
        RestoreResult::Applied { removed_files, .. } => {
            assert!(
                !removed_files.iter().any(|p| p == "big.bin"),
                "a skipped path must never be removed: {:?}",
                removed_files
            );
        }
        other => panic!("expected Applied, got {:?}", other),
    }
    assert_eq!(read(ws_dir.join("big.bin")).len(), big_body.len());
    assert_eq!(read(ws_dir.join("small.txt")), "small\n");
    let _ = fs::remove_dir_all(root);
}

/// Test 13: `--force-restore` clears an obstructing symlink or directory before writing, without
/// following the symlink and without wedging in `RecoveryRequired`.
#[test]
fn test_force_restore_clears_obstructing_symlink_and_directory() {
    let root = temp_root("force-obstruction");
    let ws_dir = root.join("ws");
    let outside_dir = root.join("outside");
    fs::create_dir_all(&ws_dir).unwrap();
    fs::create_dir_all(&outside_dir).unwrap();
    fs::write(outside_dir.join("secret.txt"), "OUTSIDE_SECRET\n").unwrap();
    fs::write(ws_dir.join("foo"), "foo v1\n").unwrap();

    let workspace = WorkspaceCap::new(&ws_dir).unwrap();
    let store = SnapshotStore::new(root.join("snapshots"));
    let baseline = store.capture_revision(&workspace, None, None, 0).unwrap();
    fs::write(ws_dir.join("foo"), "foo v2\n").unwrap();
    let mutated = store
        .capture_revision(&workspace, Some(&baseline.revision_id), None, 1)
        .unwrap();

    // External non-empty directory parked on the tracked path.
    fs::remove_file(ws_dir.join("foo")).unwrap();
    fs::create_dir_all(ws_dir.join("foo/nested")).unwrap();
    fs::write(ws_dir.join("foo/nested/inner.txt"), "user data\n").unwrap();

    let refused = store
        .restore_revision(
            &workspace,
            &baseline.revision_id,
            Some(&mutated.revision_id),
            false,
        )
        .unwrap();
    assert!(
        matches!(refused, RestoreResult::Conflict { .. }),
        "an obstructing directory must be a conflict without force: {:?}",
        refused
    );
    assert!(ws_dir.join("foo/nested/inner.txt").exists());

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
        "force must clear the obstructing directory instead of wedging: {:?}",
        forced
    );
    assert!(fs::symlink_metadata(ws_dir.join("foo"))
        .unwrap()
        .file_type()
        .is_file());
    assert_eq!(read(ws_dir.join("foo")), "foo v1\n");
    assert!(!ws_dir.join("foo/nested").exists());

    // External symlink parked on the tracked path: force must unlink the link, not its target.
    fs::remove_file(ws_dir.join("foo")).unwrap();
    create_symlink_file(outside_dir.join("secret.txt"), ws_dir.join("foo"));
    let forced_link = store
        .restore_revision(
            &workspace,
            &baseline.revision_id,
            Some(&mutated.revision_id),
            true,
        )
        .unwrap();
    assert!(
        matches!(forced_link, RestoreResult::Applied { .. }),
        "force must replace the symlink itself: {:?}",
        forced_link
    );
    assert!(fs::symlink_metadata(ws_dir.join("foo"))
        .unwrap()
        .file_type()
        .is_file());
    assert_eq!(read(ws_dir.join("foo")), "foo v1\n");
    assert_eq!(read(outside_dir.join("secret.txt")), "OUTSIDE_SECRET\n");
    let _ = fs::remove_dir_all(root);
}

fn scripted_write_provider(call_id: &str, path: &str, content: &str) -> Arc<FakeProvider> {
    Arc::new(FakeProvider::new(vec![
        AssistantTurn {
            content: None,
            reasoning_blocks: vec![],
            tool_calls: vec![ToolCall {
                call_id: ToolCallId::new(call_id),
                name: "write".into(),
                raw_arguments: serde_json::json!({ "path": path, "content": content }).to_string(),
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
    ]))
}

/// Test 12: a resumed run captures its baseline before appending the new task message, so a
/// subsequent `--undo` rewinds to a leaf that still carries the baseline revision rather than
/// evicting it from the active branch.
#[tokio::test]
async fn test_resumed_run_baseline_survives_undo_rewind() {
    let root = temp_root("resume-undo");
    let ws_dir = root.join("ws");
    let state_dir = root.join("state");
    fs::create_dir_all(ws_dir.join("src")).unwrap();
    fs::create_dir_all(&state_dir).unwrap();
    fs::write(ws_dir.join("src/app.rs"), "fn main() {}\n").unwrap();

    let store = Arc::new(SnapshotStore::new(root.join("snapshots")));
    let journal = SessionJournal::create(state_dir.join("sessions"), &ws_dir).unwrap();
    let wal_path = journal.path().to_path_buf();
    let budget = BudgetConfig::default();

    // Run 1 (fresh session): write "hi".
    let env = Arc::new(LocalHostEnv::new(&ws_dir, state_dir.clone()).unwrap());
    let policy = PolicyEngine::new(
        PolicyMode::AllowWrite,
        Arc::new(NonInteractiveApprovalHandler),
    );
    let runtime = build_default_runtime(env, policy, budget.clone(), None);
    let cfg = AgentRunConfig::new("write hi", budget.clone());
    let logger = EventLogger::new(cfg.run_id, Some(state_dir.clone()), false).unwrap();
    let (_tx, rx) = watch::channel(false);
    let (summary, _logger) = AgentRunner::new(
        cfg,
        scripted_write_provider("call_hi", "src/app.rs", "fn main() { println!(\"hi\"); }\n"),
        runtime,
        rx,
    )
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
    assert_eq!(
        read(ws_dir.join("src/app.rs")),
        "fn main() { println!(\"hi\"); }\n"
    );

    let resumed = SessionJournal::open(&wal_path).unwrap();
    let pre_run_leaf = resumed.leaf_id().map(|s| s.to_string());
    assert!(
        pre_run_leaf.is_some(),
        "run 1 must have left an active leaf"
    );
    let pre_run_turn = resumed.max_active_turn();

    // Run 2 (resume): write "bye" on the same session.
    let env = Arc::new(LocalHostEnv::new(&ws_dir, state_dir.clone()).unwrap());
    let policy = PolicyEngine::new(
        PolicyMode::AllowWrite,
        Arc::new(NonInteractiveApprovalHandler),
    );
    let runtime = build_default_runtime(env, policy, budget.clone(), None);
    let cfg = AgentRunConfig::new("write bye", budget.clone());
    let logger = EventLogger::new(cfg.run_id, Some(state_dir.clone()), false).unwrap();
    let (_tx, rx) = watch::channel(false);
    let (summary, _logger) = AgentRunner::new(
        cfg,
        scripted_write_provider(
            "call_bye",
            "src/app.rs",
            "fn main() { println!(\"bye\"); }\n",
        ),
        runtime,
        rx,
    )
    .with_session_journal(resumed)
    .with_snapshot_store(store.clone())
    .run(logger)
    .await;
    assert_eq!(
        summary.status,
        RunStatus::Completed,
        "{:?}",
        summary.terminal_reason
    );
    assert_eq!(
        read(ws_dir.join("src/app.rs")),
        "fn main() { println!(\"bye\"); }\n"
    );

    let mut journal = SessionJournal::open(&wal_path).unwrap();
    let baseline_records: Vec<_> = journal
        .revision_records()
        .into_iter()
        .filter(|r| r.turn == pre_run_turn)
        .collect();
    assert!(
        baseline_records
            .iter()
            .any(|r| r.leaf_id.as_deref() == pre_run_leaf.as_deref()),
        "the resumed baseline must be bound to the pre-run leaf: {:?}",
        baseline_records
    );

    // `--undo` rewinds to `pre_run_turn`; the resumed baseline must survive the rewind.
    journal.fork_at_turn(pre_run_turn).unwrap();
    let after_rewind = journal.revision_records();
    assert!(
        after_rewind
            .iter()
            .any(|r| r.turn == pre_run_turn && r.leaf_id.as_deref() == pre_run_leaf.as_deref()),
        "the resumed baseline record must stay on the active branch after the undo rewind: {:?}",
        after_rewind
    );
    assert_eq!(
        journal.latest_revision_id().as_deref(),
        after_rewind.last().map(|r| r.revision_id.as_str()),
        "latest_revision_id must describe the rewound branch"
    );
    let _ = fs::remove_dir_all(root);
}

// ---- F03 (v1.5.1): blob paths must reject corrupt content hashes, never panic ----------------

#[test]
fn test_blob_path_rejects_non_hex_content_hash_without_panicking() {
    let root = temp_root("f3-blob-hash");
    let store = SnapshotStore::new(root.join("snapshots"));
    // An on-disk revision manifest can carry a short / non-ASCII / path-escaping hash. `blob_path`
    // used to panic on `&sha256[..2]` and could point outside `blobs/`; it must now return
    // `InvalidStore` for both entry points.
    for bad in [
        "",
        "a",
        "zz",
        "../../escape",
        "r-0123456789abcdef0123456789abcdef",
    ] {
        match store.blob_path(bad) {
            Err(kroma_core::revision::RevisionError::InvalidStore(_)) => {}
            other => panic!(
                "expected InvalidStore from blob_path({:?}), got {:?}",
                bad, other
            ),
        }
        match store.read_blob(bad) {
            Err(kroma_core::revision::RevisionError::InvalidStore(_)) => {}
            other => panic!(
                "expected InvalidStore from read_blob({:?}), got {:?}",
                bad, other
            ),
        }
    }
    // A valid 64-hex hash still resolves beneath `blobs/`.
    let good = "a".repeat(64);
    let path = store.blob_path(&good).unwrap();
    assert!(path.starts_with(root.join("snapshots").join("blobs")));
    let _ = fs::remove_dir_all(&root);
}

// ---- F08 (v1.5.1): startup pre-check rolls back an interrupted merge -------------------------

/// The merge journal a real merge writes: the intent plus the content and permission bits it is
/// about to put on each written path, taken from the store's own scan of the live files. The fixture
/// writes it *after* the half-applied state exists, because the recorded mode has to be the file's
/// real mode; a real merge records the same evidence from its worktree before touching the parent.
fn write_merge_journal(
    store: &SnapshotStore,
    workspace: &WorkspaceCap,
    intent: kroma_core::revision::MergeIntent,
    applied: &[&str],
) {
    let (tracked, _) = store.scan_workspace_to_blobs(workspace).unwrap();
    let applied_files: Vec<kroma_core::revision::MergeAppliedFile> = applied
        .iter()
        .map(|path| {
            let entry = tracked
                .get(*path)
                .unwrap_or_else(|| panic!("fixture: '{}' must be a live tracked file", path));
            kroma_core::revision::MergeAppliedFile {
                path: (*path).to_string(),
                sha256: entry.sha256.clone(),
                mode: Some(entry.mode),
            }
        })
        .collect();
    store
        .write_merge_intent_with_applied(&intent, &applied_files)
        .unwrap();
}

#[test]
fn test_startup_recovery_rolls_back_an_interrupted_merge() {
    let root = temp_root("f8-merge-recovery");
    let ws_dir = root.join("ws");
    fs::create_dir_all(&ws_dir).unwrap();
    fs::write(ws_dir.join("keep.rs"), "v1\n").unwrap();
    fs::write(ws_dir.join("edit.rs"), "v1\n").unwrap();
    let workspace = WorkspaceCap::new(&ws_dir).unwrap();
    let store = SnapshotStore::new(root.join("snapshots"));
    let pre = store.capture_revision(&workspace, None, None, 0).unwrap();

    // Simulate a merge interrupted midway: the durable journal is present (recording the content and
    // mode the merge wrote), one path was overwritten, one was removed, and a brand-new file was
    // created.
    fs::write(ws_dir.join("edit.rs"), "half-merged\n").unwrap();
    fs::write(ws_dir.join("new.rs"), "new\n").unwrap();
    fs::remove_file(ws_dir.join("keep.rs")).unwrap();
    write_merge_journal(
        &store,
        &workspace,
        kroma_core::revision::MergeIntent {
            version: 1,
            pre_merge_revision: pre.revision_id.clone(),
            created_at_ms: 0,
            written_files: vec!["edit.rs".to_string(), "new.rs".to_string()],
            removed_files: vec!["keep.rs".to_string()],
        },
        &["edit.rs", "new.rs"],
    );

    let outcome = store.recover_pending_merge(&workspace).unwrap();
    match outcome {
        kroma_core::revision::MergeRecoveryOutcome::RolledBack {
            pre_merge_revision, ..
        } => assert_eq!(pre_merge_revision, pre.revision_id),
        other => panic!("expected RolledBack, got {:?}", other),
    }
    assert_eq!(
        read(ws_dir.join("edit.rs")),
        "v1\n",
        "an overwritten file must be restored"
    );
    assert_eq!(
        read(ws_dir.join("keep.rs")),
        "v1\n",
        "a removed file must be restored"
    );
    assert!(
        !ws_dir.join("new.rs").exists(),
        "a file the merge newly created must be removed"
    );
    assert!(store.pending_merge_intent().unwrap().is_none());
    assert!(store.pending_restore().unwrap().is_none());

    // The pre-check is idempotent: a second startup is a no-op.
    assert_eq!(
        store.recover_pending_merge(&workspace).unwrap(),
        kroma_core::revision::MergeRecoveryOutcome::NothingPending
    );
    let _ = fs::remove_dir_all(&root);
}

/// F08 follow-up: the first rollback of an interrupted merge must not overwrite work the user did
/// while Kroma was down. The journal records the content the merge wrote, so the rollback can prove
/// that `edit.rs` and `new.rs` hold its own output; once `new.rs` holds something else, the whole
/// rollback stops with `RecoveryRequired`, leaves every path in its half-applied state (no partial
/// rollback) and keeps the journal. Restoring the merge's own content makes the same recovery
/// succeed.
#[test]
fn test_merge_recovery_does_not_roll_back_over_an_edit_made_while_kroma_was_down() {
    let root = temp_root("f8-merge-user-edit-while-down");
    let ws_dir = root.join("ws");
    fs::create_dir_all(&ws_dir).unwrap();
    fs::write(ws_dir.join("keep.rs"), "v1\n").unwrap();
    fs::write(ws_dir.join("edit.rs"), "v1\n").unwrap();
    let workspace = WorkspaceCap::new(&ws_dir).unwrap();
    let store = SnapshotStore::new(root.join("snapshots"));
    let pre = store.capture_revision(&workspace, None, None, 0).unwrap();

    // The merge crashed midway: one write landed, the removal landed, the second write is pending.
    // The journal still records both writes (a real merge journals its whole delta), and the modes
    // are pinned so the proof is not at the mercy of the test process' umask.
    fs::write(ws_dir.join("edit.rs"), "half-merged\n").unwrap();
    set_file_mode(ws_dir.join("edit.rs"), 0o644);
    fs::remove_file(ws_dir.join("keep.rs")).unwrap();
    let applied = |path: &str, content: &str| kroma_core::revision::MergeAppliedFile {
        path: path.to_string(),
        sha256: kroma_core::prompt::sha256_hex64(content.as_bytes()),
        mode: Some(0o644),
    };
    store
        .write_merge_intent_with_applied(
            &kroma_core::revision::MergeIntent {
                version: 1,
                pre_merge_revision: pre.revision_id.clone(),
                created_at_ms: 0,
                written_files: vec!["edit.rs".to_string(), "new.rs".to_string()],
                removed_files: vec!["keep.rs".to_string()],
            },
            &[
                applied("edit.rs", "half-merged\n"),
                applied("new.rs", "new\n"),
            ],
        )
        .unwrap();
    // While Kroma was down the user rewrote the path the merge had created.
    fs::write(ws_dir.join("new.rs"), "user file\n").unwrap();

    let err = store
        .recover_pending_merge(&workspace)
        .expect_err("an edit made while Kroma was down must not be overwritten by the rollback");
    match err {
        kroma_core::revision::RevisionError::RecoveryRequired { reason, .. } => {
            assert!(
                reason.contains("'new.rs'") && !reason.contains("'edit.rs'"),
                "only the unprovable path may be reported: {}",
                reason
            );
        }
        other => panic!("expected RecoveryRequired, got {:?}", other),
    }
    assert_eq!(
        read(ws_dir.join("new.rs")),
        "user file\n",
        "the user's own content must survive"
    );
    assert_eq!(
        read(ws_dir.join("edit.rs")),
        "half-merged\n",
        "a refused rollback must not apply any of its writes"
    );
    assert!(
        !ws_dir.join("keep.rs").exists(),
        "a refused rollback must not restore a removal either"
    );
    assert!(
        store.pending_merge_intent().unwrap().is_some(),
        "the merge journal must survive so recovery can be retried"
    );
    assert!(
        store.pending_restore().unwrap().is_none(),
        "no restore intent may be persisted for a rollback that cannot be applied"
    );

    // The user puts the merge's own output back, which makes every path provable again.
    fs::write(ws_dir.join("new.rs"), "new\n").unwrap();
    set_file_mode(ws_dir.join("new.rs"), 0o644);
    let outcome = store.recover_pending_merge(&workspace).unwrap();
    assert!(
        matches!(
            outcome,
            kroma_core::revision::MergeRecoveryOutcome::RolledBack { .. }
        ),
        "a fully provable interrupted merge must still roll back, got {:?}",
        outcome
    );
    assert_eq!(read(ws_dir.join("edit.rs")), "v1\n");
    assert_eq!(read(ws_dir.join("keep.rs")), "v1\n");
    assert!(!ws_dir.join("new.rs").exists());
    assert!(store.pending_merge_intent().unwrap().is_none());
    assert!(store.pending_restore().unwrap().is_none());
    assert_eq!(
        store.recover_pending_merge(&workspace).unwrap(),
        kroma_core::revision::MergeRecoveryOutcome::NothingPending
    );
    let _ = fs::remove_dir_all(&root);
}

/// F08 follow-up: the same protection covers permissions. The journal records the content *and* the
/// mode the merge wrote, so a path whose bytes are still the merge's output but whose mode the user
/// changed while Kroma was down is not rolled back — that would silently restore the pre-merge mode
/// over the user's change. The refusal leaves every path in its half-applied state and keeps the
/// journal; restoring the recorded mode makes the same recovery succeed.
#[cfg(unix)]
#[test]
fn test_merge_recovery_does_not_roll_back_over_a_chmod_made_while_kroma_was_down() {
    let root = temp_root("f8-merge-user-chmod-while-down");
    let ws_dir = root.join("ws");
    fs::create_dir_all(&ws_dir).unwrap();
    fs::write(ws_dir.join("edit.rs"), "v1\n").unwrap();
    fs::write(ws_dir.join("keep.rs"), "v1\n").unwrap();
    let workspace = WorkspaceCap::new(&ws_dir).unwrap();
    let store = SnapshotStore::new(root.join("snapshots"));
    let pre = store.capture_revision(&workspace, None, None, 0).unwrap();

    // The merge half-applied: `edit.rs` and `new.rs` hold its content with the mode it wrote, and
    // `keep.rs` was removed.
    fs::write(ws_dir.join("edit.rs"), "half-merged\n").unwrap();
    set_file_mode(ws_dir.join("edit.rs"), 0o644);
    fs::remove_file(ws_dir.join("keep.rs")).unwrap();
    fs::write(ws_dir.join("new.rs"), "new\n").unwrap();
    set_file_mode(ws_dir.join("new.rs"), 0o644);
    write_merge_journal(
        &store,
        &workspace,
        kroma_core::revision::MergeIntent {
            version: 1,
            pre_merge_revision: pre.revision_id.clone(),
            created_at_ms: 0,
            written_files: vec!["edit.rs".to_string(), "new.rs".to_string()],
            removed_files: vec!["keep.rs".to_string()],
        },
        &["edit.rs", "new.rs"],
    );

    // While Kroma was down the user changed only the permission bits of the merge's output.
    set_file_mode(ws_dir.join("edit.rs"), 0o600);

    let err = store
        .recover_pending_merge(&workspace)
        .expect_err("a chmod made while Kroma was down must not be overwritten by the rollback");
    match err {
        kroma_core::revision::RevisionError::RecoveryRequired { reason, .. } => {
            assert!(
                reason.contains("'edit.rs'") && !reason.contains("'new.rs'"),
                "only the unprovable path may be reported: {}",
                reason
            );
        }
        other => panic!("expected RecoveryRequired, got {:?}", other),
    }
    assert_eq!(
        fs::metadata(ws_dir.join("edit.rs"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600,
        "the user's permission change must survive"
    );
    assert_eq!(
        read(ws_dir.join("edit.rs")),
        "half-merged\n",
        "a refused rollback must not apply any of its writes"
    );
    assert!(
        !ws_dir.join("keep.rs").exists(),
        "a refused rollback must not restore a removal either"
    );
    assert!(
        store.pending_merge_intent().unwrap().is_some(),
        "the merge journal must survive so recovery can be retried"
    );
    assert!(
        store.pending_restore().unwrap().is_none(),
        "no restore intent may be persisted for a rollback that cannot be applied"
    );

    // Putting the recorded mode back makes the path provable again.
    set_file_mode(ws_dir.join("edit.rs"), 0o644);
    let outcome = store.recover_pending_merge(&workspace).unwrap();
    assert!(
        matches!(
            outcome,
            kroma_core::revision::MergeRecoveryOutcome::RolledBack { .. }
        ),
        "a fully provable interrupted merge must still roll back, got {:?}",
        outcome
    );
    assert_eq!(read(ws_dir.join("edit.rs")), "v1\n");
    assert_eq!(read(ws_dir.join("keep.rs")), "v1\n");
    assert!(!ws_dir.join("new.rs").exists());
    assert!(store.pending_merge_intent().unwrap().is_none());
    assert!(store.pending_restore().unwrap().is_none());
    let _ = fs::remove_dir_all(&root);
}

/// F08 follow-up: a journal written before the merge recorded the content it writes proves only the
/// pre-merge state. Once such a merge has written a path, the rollback cannot tell that content from
/// an edit made while Kroma was down, so it is reported instead of overwritten — the compatibility
/// limit of every pre-existing journal. A path still holding the pre-merge state needs no proof and
/// the rollback still completes.
#[test]
fn test_merge_recovery_requires_confirmation_for_a_journal_without_applied_evidence() {
    let root = temp_root("f8-merge-legacy-journal");
    let ws_dir = root.join("ws");
    fs::create_dir_all(&ws_dir).unwrap();
    fs::write(ws_dir.join("edit.rs"), "v1\n").unwrap();
    let workspace = WorkspaceCap::new(&ws_dir).unwrap();
    let store = SnapshotStore::new(root.join("snapshots"));
    let pre = store.capture_revision(&workspace, None, None, 0).unwrap();

    store
        .write_merge_intent(&kroma_core::revision::MergeIntent {
            version: 1,
            pre_merge_revision: pre.revision_id.clone(),
            created_at_ms: 0,
            written_files: vec!["edit.rs".to_string()],
            removed_files: Vec::new(),
        })
        .unwrap();
    fs::write(ws_dir.join("edit.rs"), "half-merged\n").unwrap();

    let err = store
        .recover_pending_merge(&workspace)
        .expect_err("a journal without applied evidence cannot prove the merge's own write");
    match err {
        kroma_core::revision::RevisionError::RecoveryRequired { reason, .. } => {
            assert!(
                reason.contains("'edit.rs'"),
                "reason must name the unprovable path: {}",
                reason
            );
        }
        other => panic!("expected RecoveryRequired, got {:?}", other),
    }
    assert_eq!(read(ws_dir.join("edit.rs")), "half-merged\n");
    assert!(
        store.pending_merge_intent().unwrap().is_some(),
        "the merge journal must survive so recovery can be retried"
    );
    assert!(
        store.pending_restore().unwrap().is_none(),
        "no restore intent may be persisted for a rollback that cannot be applied"
    );

    // The user puts the pre-merge content back: that state is provable, so the rollback completes.
    fs::write(ws_dir.join("edit.rs"), "v1\n").unwrap();
    let outcome = store.recover_pending_merge(&workspace).unwrap();
    assert!(
        matches!(
            outcome,
            kroma_core::revision::MergeRecoveryOutcome::RolledBack { .. }
        ),
        "a legacy journal whose paths are all at the pre-merge state must still clear, got {:?}",
        outcome
    );
    assert_eq!(read(ws_dir.join("edit.rs")), "v1\n");
    assert!(store.pending_merge_intent().unwrap().is_none());
    assert!(store.pending_restore().unwrap().is_none());
    let _ = fs::remove_dir_all(&root);
}

/// F08 follow-up: the applied-content evidence lives in the merge intent journal itself, so a reader
/// gets the intent and the evidence from the same durable record. A journal written without it stays
/// readable and reports no evidence instead of a partial or stale one.
#[test]
fn test_merge_intent_journal_round_trips_the_applied_content_evidence() {
    let root = temp_root("f8-merge-journal-round-trip");
    let store = SnapshotStore::new(root.join("snapshots"));
    let pre = RevisionId::new("r-0123456789abcdef0123456789abcdef");
    let intent = kroma_core::revision::MergeIntent {
        version: 1,
        pre_merge_revision: pre.clone(),
        created_at_ms: 7,
        written_files: vec!["lib.rs".to_string()],
        removed_files: vec!["gone.rs".to_string()],
    };
    let sha = kroma_core::prompt::sha256_hex64(b"merged\n");

    store
        .write_merge_intent_with_applied(
            &intent,
            &[kroma_core::revision::MergeAppliedFile {
                path: "lib.rs".to_string(),
                sha256: sha.clone(),
                mode: Some(0o644),
            }],
        )
        .unwrap();
    let journal = store.pending_merge_journal().unwrap().unwrap();
    assert_eq!(journal.intent, intent);
    assert_eq!(
        journal.applied_files,
        vec![kroma_core::revision::MergeAppliedFile {
            path: "lib.rs".to_string(),
            sha256: sha,
            mode: Some(0o644),
        }],
        "the applied evidence must survive the round trip next to its intent"
    );
    assert_eq!(
        store.pending_merge_intent().unwrap(),
        Some(intent.clone()),
        "reading the intent must not drop the evidence section"
    );
    assert_eq!(
        store.pending_merge_journal().unwrap().unwrap(),
        journal,
        "a second read must return the same journal"
    );

    // An entry recorded before mode evidence existed keeps `mode: None` — the journal must not
    // invent permission bits it never saw.
    store
        .write_merge_intent_with_applied(
            &intent,
            &[kroma_core::revision::MergeAppliedFile {
                path: "lib.rs".to_string(),
                sha256: "b".repeat(64),
                mode: None,
            }],
        )
        .unwrap();
    let without_mode = store.pending_merge_journal().unwrap().unwrap();
    assert_eq!(without_mode.applied_files.len(), 1);
    assert_eq!(without_mode.applied_files[0].mode, None);

    // A journal written without evidence stays readable and proves nothing about the merge's writes.
    store.write_merge_intent(&intent).unwrap();
    let legacy = store.pending_merge_journal().unwrap().unwrap();
    assert_eq!(legacy.intent, intent);
    assert!(legacy.applied_files.is_empty());
    assert_eq!(store.pending_merge_intent().unwrap(), Some(intent));
    let _ = fs::remove_dir_all(&root);
}

/// F08: recovery must schedule every removal before every write, deepest first. A merge that turned
/// the plain file `x` into the directory `x/` holding `x/y` and then crashed before writing `x/y`
/// leaves the journal `[Write("x/y")]` + `[Remove("x")]`. Lexicographic order emits `Write("x")`
/// first, which then makes `Remove("x/y")` fail with `ENOTDIR` after the intent is already
/// persisted — a permanent `EXIT_RESTORE_RECOVERY_REQUIRED` wedge.
#[test]
fn test_startup_merge_recovery_orders_removals_before_writes_for_a_file_to_dir_swap() {
    let root = temp_root("f8-merge-file-dir-order");
    let ws_dir = root.join("ws");
    fs::create_dir_all(&ws_dir).unwrap();
    fs::write(ws_dir.join("x"), "file-v1\n").unwrap();
    let workspace = WorkspaceCap::new(&ws_dir).unwrap();
    let store = SnapshotStore::new(root.join("snapshots"));
    let pre = store.capture_revision(&workspace, None, None, 0).unwrap();

    store
        .write_merge_intent(&kroma_core::revision::MergeIntent {
            version: 1,
            pre_merge_revision: pre.revision_id.clone(),
            created_at_ms: 0,
            written_files: vec!["x/y".to_string()],
            removed_files: vec!["x".to_string()],
        })
        .unwrap();
    // The merge removed the file `x` and created the directory `x`, then crashed before writing
    // `x/y`.
    fs::remove_file(ws_dir.join("x")).unwrap();
    fs::create_dir(ws_dir.join("x")).unwrap();

    let outcome = store.recover_pending_merge(&workspace).unwrap();
    assert!(
        matches!(
            outcome,
            kroma_core::revision::MergeRecoveryOutcome::RolledBack { .. }
        ),
        "expected a clean rollback, got {:?}",
        outcome
    );
    assert!(
        ws_dir.join("x").is_file(),
        "x must be restored as a plain file, not left as a directory"
    );
    assert_eq!(read(ws_dir.join("x")), "file-v1\n");
    assert!(store.pending_merge_intent().unwrap().is_none());
    assert!(store.pending_restore().unwrap().is_none());
    let _ = fs::remove_dir_all(&root);
}

/// F08: a merge-created path the user has since replaced with a directory must survive the
/// rollback. Rollback removals are non-forced, so a non-regular entry is external work and is left
/// in place instead of being recursively deleted.
#[test]
fn test_startup_merge_recovery_keeps_a_user_directory_that_replaced_a_merged_file() {
    let root = temp_root("f8-merge-non-forced-remove");
    let ws_dir = root.join("ws");
    fs::create_dir_all(&ws_dir).unwrap();
    fs::write(ws_dir.join("keep.rs"), "keep\n").unwrap();
    let workspace = WorkspaceCap::new(&ws_dir).unwrap();
    let store = SnapshotStore::new(root.join("snapshots"));
    let pre = store.capture_revision(&workspace, None, None, 0).unwrap();

    store
        .write_merge_intent(&kroma_core::revision::MergeIntent {
            version: 1,
            pre_merge_revision: pre.revision_id.clone(),
            created_at_ms: 0,
            written_files: vec!["new.rs".to_string()],
            removed_files: Vec::new(),
        })
        .unwrap();
    // The merge created `new.rs`; the user then replaced it with their own directory.
    fs::create_dir(ws_dir.join("new.rs")).unwrap();
    fs::write(ws_dir.join("new.rs").join("user.txt"), "mine\n").unwrap();

    let outcome = store.recover_pending_merge(&workspace).unwrap();
    assert!(
        matches!(
            outcome,
            kroma_core::revision::MergeRecoveryOutcome::RolledBack { .. }
        ),
        "expected a clean rollback, got {:?}",
        outcome
    );
    assert!(
        ws_dir.join("new.rs").is_dir(),
        "a user directory on a merge-created path must not be recursively deleted"
    );
    assert_eq!(read(ws_dir.join("new.rs").join("user.txt")), "mine\n");
    let _ = fs::remove_dir_all(&root);
}

/// F03/F08: a crash between capturing the merged revision and clearing the journal leaves a stale
/// journal even though the merge landed. Recovery must detect the completed merge and clear the
/// journal instead of silently rolling the merge back.
#[test]
fn test_startup_merge_recovery_detects_a_completed_merge_and_keeps_it() {
    let root = temp_root("f8-merge-already-landed");
    let ws_dir = root.join("ws");
    fs::create_dir_all(&ws_dir).unwrap();
    fs::write(ws_dir.join("keep.rs"), "v1\n").unwrap();
    fs::write(ws_dir.join("edit.rs"), "v1\n").unwrap();
    let workspace = WorkspaceCap::new(&ws_dir).unwrap();
    let store = SnapshotStore::new(root.join("snapshots"));
    let pre = store.capture_revision(&workspace, None, None, 0).unwrap();

    store
        .write_merge_intent(&kroma_core::revision::MergeIntent {
            version: 1,
            pre_merge_revision: pre.revision_id.clone(),
            created_at_ms: 0,
            written_files: vec!["edit.rs".to_string()],
            removed_files: vec!["keep.rs".to_string()],
        })
        .unwrap();
    // The merge applied fully and durably captured its merged manifest (unlinked, parented on the
    // pre-merge revision), then crashed before the journal was cleared.
    fs::write(ws_dir.join("edit.rs"), "merged\n").unwrap();
    fs::remove_file(ws_dir.join("keep.rs")).unwrap();
    let merged = store
        .capture_revision(&workspace, Some(&pre.revision_id), None, 0)
        .unwrap();

    let outcome = store.recover_pending_merge(&workspace).unwrap();
    assert_eq!(
        outcome,
        kroma_core::revision::MergeRecoveryOutcome::AlreadyMerged {
            merged_revision: merged.revision_id.clone()
        }
    );
    assert_eq!(
        read(ws_dir.join("edit.rs")),
        "merged\n",
        "a completed merge must not be rolled back"
    );
    assert!(
        !ws_dir.join("keep.rs").exists(),
        "a completed merge must not be rolled back"
    );
    assert!(store.pending_merge_intent().unwrap().is_none());
    assert!(store.pending_restore().unwrap().is_none());

    // Idempotent: the stale journal is gone, so the next startup has nothing to do.
    assert_eq!(
        store.recover_pending_merge(&workspace).unwrap(),
        kroma_core::revision::MergeRecoveryOutcome::NothingPending
    );
    let _ = fs::remove_dir_all(&root);
}

/// F03/F08: a WAL-linked capture with the same parent is not the merge's unlinked merged manifest,
/// so it must not be mistaken for a completed merge; the stale journal is still rolled back.
#[test]
fn test_startup_merge_recovery_rolls_back_when_only_a_wal_linked_revision_matches() {
    let root = temp_root("f8-merge-wal-linked");
    let ws_dir = root.join("ws");
    fs::create_dir_all(&ws_dir).unwrap();
    fs::write(ws_dir.join("keep.rs"), "v1\n").unwrap();
    fs::write(ws_dir.join("edit.rs"), "v1\n").unwrap();
    let workspace = WorkspaceCap::new(&ws_dir).unwrap();
    let store = SnapshotStore::new(root.join("snapshots"));
    let pre = store.capture_revision(&workspace, None, None, 0).unwrap();

    fs::write(ws_dir.join("edit.rs"), "merged\n").unwrap();
    fs::remove_file(ws_dir.join("keep.rs")).unwrap();
    write_merge_journal(
        &store,
        &workspace,
        kroma_core::revision::MergeIntent {
            version: 1,
            pre_merge_revision: pre.revision_id.clone(),
            created_at_ms: 0,
            written_files: vec!["edit.rs".to_string()],
            removed_files: vec!["keep.rs".to_string()],
        },
        &["edit.rs"],
    );
    store
        .capture_revision(&workspace, Some(&pre.revision_id), Some("wal-1"), 0)
        .unwrap();

    let outcome = store.recover_pending_merge(&workspace).unwrap();
    assert!(
        matches!(
            outcome,
            kroma_core::revision::MergeRecoveryOutcome::RolledBack { .. }
        ),
        "a WAL-linked revision is not the merge's own capture; got {:?}",
        outcome
    );
    assert_eq!(read(ws_dir.join("edit.rs")), "v1\n");
    assert_eq!(read(ws_dir.join("keep.rs")), "v1\n");
    let _ = fs::remove_dir_all(&root);
}

/// F03/F08: the completed-merge proof requires the matching revision to cover *every* written path;
/// a manifest missing one is not proof, so the stale journal is still rolled back.
#[test]
fn test_startup_merge_recovery_rolls_back_when_the_matching_revision_misses_a_written_path() {
    let root = temp_root("f8-merge-missing-written");
    let ws_dir = root.join("ws");
    fs::create_dir_all(&ws_dir).unwrap();
    fs::write(ws_dir.join("edit.rs"), "v1\n").unwrap();
    let workspace = WorkspaceCap::new(&ws_dir).unwrap();
    let store = SnapshotStore::new(root.join("snapshots"));
    let pre = store.capture_revision(&workspace, None, None, 0).unwrap();

    fs::write(ws_dir.join("edit.rs"), "half-merged\n").unwrap();
    write_merge_journal(
        &store,
        &workspace,
        kroma_core::revision::MergeIntent {
            version: 1,
            pre_merge_revision: pre.revision_id.clone(),
            created_at_ms: 0,
            written_files: vec!["edit.rs".to_string(), "never-created.rs".to_string()],
            removed_files: Vec::new(),
        },
        &["edit.rs"],
    );
    store
        .capture_revision(&workspace, Some(&pre.revision_id), None, 0)
        .unwrap();

    let outcome = store.recover_pending_merge(&workspace).unwrap();
    assert!(
        matches!(
            outcome,
            kroma_core::revision::MergeRecoveryOutcome::RolledBack { .. }
        ),
        "a manifest missing a written path is not proof of completion; got {:?}",
        outcome
    );
    assert_eq!(read(ws_dir.join("edit.rs")), "v1\n");
    let _ = fs::remove_dir_all(&root);
}

/// F08: a matching merged manifest is not proof on its own. A post-merge gate rejection with
/// `revert_on_verify_failure == false` keeps the merged revision M in the store while the user puts
/// the workspace back by hand; a later merge (same content-addressed pre-merge revision) then
/// crashes half-applied. Recovery must roll the half-merged workspace back instead of trusting M
/// and clearing the journal, which would strand the workspace in a state no startup can repair.
#[test]
fn test_startup_merge_recovery_rolls_back_a_half_merged_workspace_despite_a_matching_merged_manifest(
) {
    let root = temp_root("f8-merge-half-merged-with-matching-manifest");
    let ws_dir = root.join("ws");
    fs::create_dir_all(&ws_dir).unwrap();
    fs::write(ws_dir.join("edit.rs"), "v1\n").unwrap();
    fs::write(ws_dir.join("keep.rs"), "v1\n").unwrap();
    let workspace = WorkspaceCap::new(&ws_dir).unwrap();
    let store = SnapshotStore::new(root.join("snapshots"));
    let pre = store.capture_revision(&workspace, None, None, 0).unwrap();

    let intent = || kroma_core::revision::MergeIntent {
        version: 1,
        pre_merge_revision: pre.revision_id.clone(),
        created_at_ms: 0,
        written_files: vec!["edit.rs".to_string()],
        removed_files: vec!["keep.rs".to_string()],
    };

    // A first merge lands fully and captures the merged manifest M (unlinked, parented on `pre`),
    // which the post-merge gate rejection deliberately keeps in the store.
    fs::write(ws_dir.join("edit.rs"), "merged\n").unwrap();
    fs::remove_file(ws_dir.join("keep.rs")).unwrap();
    write_merge_journal(&store, &workspace, intent(), &["edit.rs"]);
    store
        .capture_revision(&workspace, Some(&pre.revision_id), None, 0)
        .unwrap();

    // The user then reverts the workspace to S0 by hand.
    fs::write(ws_dir.join("edit.rs"), "v1\n").unwrap();
    fs::write(ws_dir.join("keep.rs"), "v1\n").unwrap();

    // A second merge (same content-addressed pre-merge revision) writes a fresh journal and crashes
    // halfway, leaving `edit.rs` half-written.
    fs::write(ws_dir.join("edit.rs"), "half-merged\n").unwrap();
    write_merge_journal(&store, &workspace, intent(), &["edit.rs"]);

    let outcome = store.recover_pending_merge(&workspace).unwrap();
    assert!(
        matches!(
            outcome,
            kroma_core::revision::MergeRecoveryOutcome::RolledBack { .. }
        ),
        "a half-merged workspace must be rolled back, not trusted from a stale manifest; got {:?}",
        outcome
    );
    assert_eq!(read(ws_dir.join("edit.rs")), "v1\n");
    assert_eq!(read(ws_dir.join("keep.rs")), "v1\n");
    assert!(store.pending_merge_intent().unwrap().is_none());
    assert!(store.pending_restore().unwrap().is_none());
    let _ = fs::remove_dir_all(&root);
}

/// F08: a deletions-only merge records `written_files == []`, so the "every written path is tracked"
/// proof is vacuously true and matches any unlinked revision with the same parent. The removed path
/// must actually be absent for the merge to count as landed — and once the user has put their own
/// file back on that path, the rollback cannot prove the bytes are the merge's to delete.
#[test]
fn test_startup_merge_recovery_requires_confirmation_when_a_removed_path_came_back_by_hand() {
    let root = temp_root("f8-merge-deletions-only");
    let ws_dir = root.join("ws");
    fs::create_dir_all(&ws_dir).unwrap();
    fs::write(ws_dir.join("gone.rs"), "v1\n").unwrap();
    let workspace = WorkspaceCap::new(&ws_dir).unwrap();
    let store = SnapshotStore::new(root.join("snapshots"));
    let pre = store.capture_revision(&workspace, None, None, 0).unwrap();

    let intent = || kroma_core::revision::MergeIntent {
        version: 1,
        pre_merge_revision: pre.revision_id.clone(),
        created_at_ms: 0,
        written_files: Vec::new(),
        removed_files: vec!["gone.rs".to_string()],
    };

    // The first merge deleted `gone.rs` and captured its merged manifest (no tracked files).
    store.write_merge_intent(&intent()).unwrap();
    fs::remove_file(ws_dir.join("gone.rs")).unwrap();
    store
        .capture_revision(&workspace, Some(&pre.revision_id), None, 0)
        .unwrap();

    // The user restores `gone.rs` by hand, then a second merge crashes before deleting it again.
    fs::write(ws_dir.join("gone.rs"), "user-restored\n").unwrap();
    store.write_merge_intent(&intent()).unwrap();

    let err = store
        .recover_pending_merge(&workspace)
        .expect_err("a hand-restored removed path must not be treated as landed or rolled back");
    match err {
        kroma_core::revision::RevisionError::RecoveryRequired { reason, .. } => {
            assert!(
                reason.contains("'gone.rs'"),
                "reason must name the unprovable path: {}",
                reason
            );
        }
        other => panic!("expected RecoveryRequired, got {:?}", other),
    }
    assert_eq!(
        read(ws_dir.join("gone.rs")),
        "user-restored\n",
        "a file the user restored by hand must not be overwritten"
    );
    assert!(
        store.pending_merge_intent().unwrap().is_some(),
        "the merge journal must survive so recovery can be retried"
    );
    assert!(
        store.pending_restore().unwrap().is_none(),
        "no restore intent may be persisted for a rollback that cannot be applied"
    );
    let _ = fs::remove_dir_all(&root);
}

/// F08: a plain file on an ancestor component of a write path makes `open_parent_dir_no_symlink`
/// fail with `ENOTDIR` (`SymlinkBlocked`) after the journal was already persisted, wedging every
/// future startup. It must be detected up front as `RecoveryRequired` without persisting a restore
/// intent.
#[test]
fn test_startup_merge_recovery_reports_recovery_required_when_a_plain_file_blocks_a_write_path() {
    let root = temp_root("f8-merge-write-path-file-ancestor");
    let ws_dir = root.join("ws");
    fs::create_dir_all(ws_dir.join("d")).unwrap();
    fs::write(ws_dir.join("d").join("a"), "v1\n").unwrap();
    let workspace = WorkspaceCap::new(&ws_dir).unwrap();
    let store = SnapshotStore::new(root.join("snapshots"));
    let pre = store.capture_revision(&workspace, None, None, 0).unwrap();

    store
        .write_merge_intent(&kroma_core::revision::MergeIntent {
            version: 1,
            pre_merge_revision: pre.revision_id.clone(),
            created_at_ms: 0,
            written_files: vec!["d/a".to_string()],
            removed_files: Vec::new(),
        })
        .unwrap();
    // The merge half-wrote `d/a`; the user then replaced the directory `d` with a plain file.
    fs::remove_file(ws_dir.join("d").join("a")).unwrap();
    fs::remove_dir(ws_dir.join("d")).unwrap();
    fs::write(ws_dir.join("d"), "user file\n").unwrap();

    let err = store
        .recover_pending_merge(&workspace)
        .expect_err("a plain file blocking a write path must be an actionable failure");
    match err {
        kroma_core::revision::RevisionError::RecoveryRequired { reason, .. } => {
            assert!(
                reason.contains("'d/a'") && reason.contains("'d'"),
                "reason must name the blocked path and the blocking component: {}",
                reason
            );
        }
        other => panic!("expected RecoveryRequired, got {:?}", other),
    }
    assert!(
        store.pending_restore().unwrap().is_none(),
        "no restore intent may be persisted for a rollback that cannot be applied"
    );
    assert!(
        store.pending_merge_intent().unwrap().is_some(),
        "the merge journal must survive so recovery can be retried"
    );
    assert_eq!(
        read(ws_dir.join("d")),
        "user file\n",
        "the user's plain file must be left untouched"
    );
    let _ = fs::remove_dir_all(&root);
}

/// F08: the legal `directory -> file` rollback — the merge wrote the file `d` and the rollback must
/// restore `d/a` — leaves a plain file at the ancestor `d`, but `d` is scheduled for removal, so it
/// is unlinked first and the write lands. This exception must keep working.
#[test]
fn test_startup_merge_recovery_allows_a_plain_file_ancestor_that_is_scheduled_for_removal() {
    let root = temp_root("f8-merge-write-path-file-ancestor-removed");
    let ws_dir = root.join("ws");
    fs::create_dir_all(ws_dir.join("d")).unwrap();
    fs::write(ws_dir.join("d").join("a"), "v1\n").unwrap();
    let workspace = WorkspaceCap::new(&ws_dir).unwrap();
    let store = SnapshotStore::new(root.join("snapshots"));
    let pre = store.capture_revision(&workspace, None, None, 0).unwrap();

    // The merge removed the directory `d` (its file `d/a`) and wrote the file `d`, then crashed.
    fs::remove_file(ws_dir.join("d").join("a")).unwrap();
    fs::remove_dir(ws_dir.join("d")).unwrap();
    fs::write(ws_dir.join("d"), "merged\n").unwrap();
    write_merge_journal(
        &store,
        &workspace,
        kroma_core::revision::MergeIntent {
            version: 1,
            pre_merge_revision: pre.revision_id.clone(),
            created_at_ms: 0,
            written_files: vec!["d".to_string()],
            removed_files: vec!["d/a".to_string()],
        },
        &["d"],
    );

    let outcome = store.recover_pending_merge(&workspace).unwrap();
    assert!(
        matches!(
            outcome,
            kroma_core::revision::MergeRecoveryOutcome::RolledBack { .. }
        ),
        "expected a clean rollback, got {:?}",
        outcome
    );
    assert!(
        ws_dir.join("d").is_dir(),
        "d must be restored as a directory"
    );
    assert_eq!(read(ws_dir.join("d").join("a")), "v1\n");
    assert!(store.pending_merge_intent().unwrap().is_none());
    assert!(store.pending_restore().unwrap().is_none());
    let _ = fs::remove_dir_all(&root);
}

/// F08: a merge that replaced a directory with a file is rolled back by removing the file and
/// restoring the directory's children. If the user parks a symlink on the directory path, the
/// non-forced removal is a silent no-op and the forced write would hit `SymlinkBlocked` after the
/// journal was persisted, wedging every startup. Recovery must fail with an actionable
/// `RecoveryRequired` (path + reason) instead of looping on the obstruction.
#[cfg(unix)]
#[test]
fn test_startup_merge_recovery_reports_recovery_required_when_a_symlink_blocks_a_restored_directory(
) {
    let root = temp_root("f8-merge-symlink-blocks-dir-restore");
    let ws_dir = root.join("ws");
    fs::create_dir_all(ws_dir.join("p")).unwrap();
    fs::write(ws_dir.join("p").join("y"), "v1\n").unwrap();
    let workspace = WorkspaceCap::new(&ws_dir).unwrap();
    let store = SnapshotStore::new(root.join("snapshots"));
    let pre = store.capture_revision(&workspace, None, None, 0).unwrap();

    store
        .write_merge_intent(&kroma_core::revision::MergeIntent {
            version: 1,
            pre_merge_revision: pre.revision_id.clone(),
            created_at_ms: 0,
            written_files: vec!["p".to_string()],
            removed_files: vec!["p/y".to_string()],
        })
        .unwrap();
    // The merge replaced the directory `p` with the file `p`, then crashed.
    fs::remove_file(ws_dir.join("p").join("y")).unwrap();
    fs::remove_dir(ws_dir.join("p")).unwrap();
    fs::write(ws_dir.join("p"), "merged\n").unwrap();
    // The user then parks a symlink on `p` before recovery.
    fs::remove_file(ws_dir.join("p")).unwrap();
    let elsewhere = root.join("elsewhere");
    fs::create_dir_all(&elsewhere).unwrap();
    std::os::unix::fs::symlink(&elsewhere, ws_dir.join("p")).unwrap();

    let err = store
        .recover_pending_merge(&workspace)
        .expect_err("a symlink blocking the restored directory must be an actionable failure");
    match err {
        kroma_core::revision::RevisionError::RecoveryRequired { reason, .. } => {
            assert!(
                reason.contains("'p'"),
                "reason must name the blocking path: {}",
                reason
            );
        }
        other => panic!("expected RecoveryRequired, got {:?}", other),
    }
    assert!(
        store.pending_merge_intent().unwrap().is_some(),
        "the merge journal must survive so recovery can be retried"
    );
    assert!(
        store.pending_restore().unwrap().is_none(),
        "no restore intent may be persisted for a rollback that cannot be applied"
    );

    // Once the user removes the symlink, the same recovery succeeds.
    fs::remove_file(ws_dir.join("p")).unwrap();
    let outcome = store.recover_pending_merge(&workspace).unwrap();
    assert!(
        matches!(
            outcome,
            kroma_core::revision::MergeRecoveryOutcome::RolledBack { .. }
        ),
        "expected a clean rollback after the obstruction is removed, got {:?}",
        outcome
    );
    assert_eq!(read(ws_dir.join("p").join("y")), "v1\n");
    let _ = fs::remove_dir_all(&root);
}

/// F08: a merge that *modified* a tracked file records a `write` operation. If the user replaces
/// that file with a directory before the interrupted merge is rolled back, the forced write would
/// recursively delete the directory — external work the merge never created. Recovery must refuse
/// with an actionable `RecoveryRequired` and leave the directory and its contents intact.
#[test]
fn test_startup_merge_recovery_refuses_to_delete_a_user_directory_on_a_modified_write_path() {
    let root = temp_root("f8-merge-write-path-user-directory");
    let ws_dir = root.join("ws");
    fs::create_dir_all(&ws_dir).unwrap();
    fs::write(ws_dir.join("edit.rs"), "v1\n").unwrap();
    let workspace = WorkspaceCap::new(&ws_dir).unwrap();
    let store = SnapshotStore::new(root.join("snapshots"));
    let pre = store.capture_revision(&workspace, None, None, 0).unwrap();

    // The merge half-wrote `edit.rs` (mode pinned so the recorded evidence is umask-independent),
    // then the user replaced it with their own directory.
    fs::write(ws_dir.join("edit.rs"), "half-merged\n").unwrap();
    set_file_mode(ws_dir.join("edit.rs"), 0o644);
    write_merge_journal(
        &store,
        &workspace,
        kroma_core::revision::MergeIntent {
            version: 1,
            pre_merge_revision: pre.revision_id.clone(),
            created_at_ms: 0,
            written_files: vec!["edit.rs".to_string()],
            removed_files: Vec::new(),
        },
        &["edit.rs"],
    );
    fs::remove_file(ws_dir.join("edit.rs")).unwrap();
    fs::create_dir(ws_dir.join("edit.rs")).unwrap();
    fs::write(ws_dir.join("edit.rs").join("user.txt"), "mine\n").unwrap();

    let err = store
        .recover_pending_merge(&workspace)
        .expect_err("a user directory on a modified write path must be an actionable failure");
    match err {
        kroma_core::revision::RevisionError::RecoveryRequired { reason, .. } => {
            assert!(
                reason.contains("'edit.rs'"),
                "reason must name the blocking path: {}",
                reason
            );
        }
        other => panic!("expected RecoveryRequired, got {:?}", other),
    }
    assert!(
        ws_dir.join("edit.rs").is_dir(),
        "a user directory on a modified write path must not be recursively deleted"
    );
    assert_eq!(read(ws_dir.join("edit.rs").join("user.txt")), "mine\n");
    assert!(
        store.pending_restore().unwrap().is_none(),
        "no restore intent may be persisted for a rollback that cannot be applied"
    );
    assert!(
        store.pending_merge_intent().unwrap().is_some(),
        "the merge journal must survive so recovery can be retried"
    );

    // Once the user clears the obstruction and puts the merge's half-written file back, the same
    // recovery succeeds and restores the pre-merge content. A path the user left absent stays
    // unprovable: recovery cannot tell a deletion by the merge from one made while Kroma was down.
    fs::remove_file(ws_dir.join("edit.rs").join("user.txt")).unwrap();
    fs::remove_dir(ws_dir.join("edit.rs")).unwrap();
    fs::write(ws_dir.join("edit.rs"), "half-merged\n").unwrap();
    set_file_mode(ws_dir.join("edit.rs"), 0o644);
    let outcome = store.recover_pending_merge(&workspace).unwrap();
    assert!(
        matches!(
            outcome,
            kroma_core::revision::MergeRecoveryOutcome::RolledBack { .. }
        ),
        "expected a clean rollback after the obstruction is removed, got {:?}",
        outcome
    );
    assert_eq!(read(ws_dir.join("edit.rs")), "v1\n");
    let _ = fs::remove_dir_all(&root);
}

/// F08: a merge that replaced the file `d` with the directory `d` is rolled back by restoring the
/// file `d`. The old "any removal lives below `d`" check let the forced write recursively delete an
/// untracked user file the user had dropped into the live directory. Recovery must enumerate the
/// real leaves and refuse with `RecoveryRequired`, leaving the user's file intact.
#[test]
fn test_startup_merge_recovery_refuses_to_delete_an_untracked_user_file_inside_a_restored_directory(
) {
    let root = temp_root("f8-merge-dir-user-file");
    let ws_dir = root.join("ws");
    fs::create_dir_all(&ws_dir).unwrap();
    fs::write(ws_dir.join("d"), "file-v1\n").unwrap();
    let workspace = WorkspaceCap::new(&ws_dir).unwrap();
    let store = SnapshotStore::new(root.join("snapshots"));
    let pre = store.capture_revision(&workspace, None, None, 0).unwrap();

    // The merge removed the file `d` and created the directory `d` with its own file `d/b`; the
    // user then dropped an untracked file into it.
    fs::remove_file(ws_dir.join("d")).unwrap();
    fs::create_dir(ws_dir.join("d")).unwrap();
    fs::write(ws_dir.join("d").join("b"), "merged\n").unwrap();
    fs::write(ws_dir.join("d").join("user.txt"), "mine\n").unwrap();
    write_merge_journal(
        &store,
        &workspace,
        kroma_core::revision::MergeIntent {
            version: 1,
            pre_merge_revision: pre.revision_id.clone(),
            created_at_ms: 0,
            written_files: vec!["d/b".to_string()],
            removed_files: vec!["d".to_string()],
        },
        &["d/b"],
    );

    let err = store.recover_pending_merge(&workspace).expect_err(
        "an untracked file inside the restored directory must be an actionable failure",
    );
    match err {
        kroma_core::revision::RevisionError::RecoveryRequired { reason, .. } => {
            assert!(
                reason.contains("'d'") && reason.contains("user.txt"),
                "reason must name the write path and the untracked leaf: {}",
                reason
            );
        }
        other => panic!("expected RecoveryRequired, got {:?}", other),
    }
    assert!(
        ws_dir.join("d").join("user.txt").exists(),
        "the user's untracked file must survive"
    );
    assert_eq!(read(ws_dir.join("d").join("user.txt")), "mine\n");
    assert!(
        store.pending_restore().unwrap().is_none(),
        "no restore intent may be persisted for a rollback that cannot be applied"
    );
    assert!(
        store.pending_merge_intent().unwrap().is_some(),
        "the merge journal must survive so recovery can be retried"
    );

    // Once the user moves their file out, the same recovery succeeds and restores the file `d`.
    fs::remove_file(ws_dir.join("d").join("user.txt")).unwrap();
    let outcome = store.recover_pending_merge(&workspace).unwrap();
    assert!(
        matches!(
            outcome,
            kroma_core::revision::MergeRecoveryOutcome::RolledBack { .. }
        ),
        "expected a clean rollback after the untracked file is removed, got {:?}",
        outcome
    );
    assert!(ws_dir.join("d").is_file(), "d must be restored as a file");
    assert_eq!(read(ws_dir.join("d")), "file-v1\n");
    let _ = fs::remove_dir_all(&root);
}

/// F08: a symlink on a non-touched ancestor of a write path makes `open_parent_dir_no_symlink` fail
/// (`SymlinkBlocked`). Because the rollback journal is persisted before the writes are applied, the
/// failure would recur on every future startup and wedge recovery. It must be detected up front and
/// reported as `RecoveryRequired` without persisting a restore intent.
#[cfg(unix)]
#[test]
fn test_startup_merge_recovery_reports_recovery_required_when_a_symlink_blocks_a_modified_write_path(
) {
    let root = temp_root("f8-merge-write-path-symlink-ancestor");
    let ws_dir = root.join("ws");
    fs::create_dir_all(ws_dir.join("p")).unwrap();
    fs::write(ws_dir.join("p").join("edit.rs"), "v1\n").unwrap();
    let workspace = WorkspaceCap::new(&ws_dir).unwrap();
    let store = SnapshotStore::new(root.join("snapshots"));
    let pre = store.capture_revision(&workspace, None, None, 0).unwrap();

    store
        .write_merge_intent(&kroma_core::revision::MergeIntent {
            version: 1,
            pre_merge_revision: pre.revision_id.clone(),
            created_at_ms: 0,
            written_files: vec!["p/edit.rs".to_string()],
            removed_files: Vec::new(),
        })
        .unwrap();
    // The merge half-wrote `p/edit.rs`, then the user replaced the directory `p` with a symlink.
    let elsewhere = root.join("elsewhere");
    fs::create_dir_all(&elsewhere).unwrap();
    fs::remove_file(ws_dir.join("p").join("edit.rs")).unwrap();
    fs::remove_dir(ws_dir.join("p")).unwrap();
    std::os::unix::fs::symlink(&elsewhere, ws_dir.join("p")).unwrap();

    let err = store
        .recover_pending_merge(&workspace)
        .expect_err("a symlink blocking a write path must be an actionable failure");
    match err {
        kroma_core::revision::RevisionError::RecoveryRequired { reason, .. } => {
            assert!(
                reason.contains("'p/edit.rs'") && reason.contains("'p'"),
                "reason must name the blocked path and the blocking component: {}",
                reason
            );
        }
        other => panic!("expected RecoveryRequired, got {:?}", other),
    }
    assert!(
        store.pending_restore().unwrap().is_none(),
        "the journal must not be persisted for a rollback that cannot be applied"
    );
    assert!(
        store.pending_merge_intent().unwrap().is_some(),
        "the merge journal must survive so recovery can be retried"
    );
    let _ = fs::remove_dir_all(&root);
}

/// F03: a restore journal written before the per-operation `force` flag existed recorded only the
/// intent-level force. A forced restore's removal must stay forced; defaulting the missing flag to
/// `false` would silently no-op on an obstructing symlink while still reporting the path as removed.
#[cfg(unix)]
#[test]
fn test_restore_recovery_forces_a_removal_from_a_journal_without_the_per_operation_force_flag() {
    let root = temp_root("f3-legacy-remove-force-default");
    let ws_dir = root.join("ws");
    fs::create_dir_all(&ws_dir).unwrap();
    fs::write(ws_dir.join("keep.rs"), "v1\n").unwrap();
    let workspace = WorkspaceCap::new(&ws_dir).unwrap();
    let store = SnapshotStore::new(root.join("snapshots"));
    let target = store.capture_revision(&workspace, None, None, 0).unwrap();

    // A journal from the previous binary: `force` lived only on the intent, so the `remove`
    // operation has no per-operation flag.
    let legacy = serde_json::json!({
        "version": 1,
        "target_revision": target.revision_id.as_str(),
        "parent_revision": null,
        "created_at_ms": 0,
        "force": true,
        "operations": [ { "op": "remove", "path": "merged.rs" } ]
    });
    fs::write(store.intent_path(), serde_json::to_vec(&legacy).unwrap()).unwrap();

    // The merge-created file was replaced by a user symlink before recovery.
    let elsewhere = root.join("elsewhere");
    fs::create_dir_all(&elsewhere).unwrap();
    create_symlink_file(&elsewhere, ws_dir.join("merged.rs"));

    // C00-10: a journal with no recorded parent cannot prove that symlink was not put there while
    // Kroma was down, so the *default* replay refuses it and keeps the journal instead of deleting
    // unknown external state. (The intent's own `force` describes the original restore; it is not
    // standing authorization for work that appeared after the crash.)
    let refused = store.recover_pending_restore(&workspace).unwrap();
    assert!(
        matches!(refused, RestoreResult::Conflict { .. }),
        "an unprovable removal must be refused by default, got {:?}",
        refused
    );
    assert!(
        fs::symlink_metadata(ws_dir.join("merged.rs")).is_ok(),
        "a refused replay must not delete the external entry"
    );
    assert!(store.pending_restore().unwrap().is_some());

    // The legacy per-op default is still what governs *how* the authorized replay applies it: with
    // this call's explicit force the forced removal lands instead of silently degrading to a no-op.
    let result = store
        .recover_pending_restore_with_force(&workspace, true)
        .unwrap();
    match result {
        RestoreResult::Applied { removed_files, .. } => {
            assert_eq!(removed_files, vec!["merged.rs".to_string()]);
        }
        other => panic!("expected Applied, got {:?}", other),
    }
    assert!(
        fs::symlink_metadata(ws_dir.join("merged.rs")).is_err(),
        "a forced removal from a legacy journal must not degrade to a silent no-op"
    );
    let _ = fs::remove_dir_all(&root);
}

/// F03: the other half of the legacy-journal default. An *unforced* restore (`force == false`) must
/// keep its removals non-forced: defaulting the absent per-op flag to `true` must not silently
/// upgrade a no-op removal into a recursive delete of a user directory parked on the managed path.
#[test]
fn test_restore_recovery_keeps_a_user_directory_for_a_legacy_unforced_removal() {
    let root = temp_root("f3-legacy-remove-unforced");
    let ws_dir = root.join("ws");
    fs::create_dir_all(&ws_dir).unwrap();
    fs::write(ws_dir.join("keep.rs"), "v1\n").unwrap();
    let workspace = WorkspaceCap::new(&ws_dir).unwrap();
    let store = SnapshotStore::new(root.join("snapshots"));
    let target = store.capture_revision(&workspace, None, None, 0).unwrap();

    // A journal from the previous binary: `force` lived only on the intent (here `false`, an
    // ordinary `--restore-rev`), so the `remove` operation has no per-operation flag.
    let legacy = serde_json::json!({
        "version": 1,
        "target_revision": target.revision_id.as_str(),
        "parent_revision": null,
        "created_at_ms": 0,
        "force": false,
        "operations": [ { "op": "remove", "path": "merged.rs" } ]
    });
    fs::write(store.intent_path(), serde_json::to_vec(&legacy).unwrap()).unwrap();

    // The user replaced the merge-created file with their own directory before recovery.
    fs::create_dir(ws_dir.join("merged.rs")).unwrap();
    fs::write(ws_dir.join("merged.rs").join("user.txt"), "mine\n").unwrap();

    let result = store.recover_pending_restore(&workspace).unwrap();
    assert!(
        matches!(result, RestoreResult::Applied { .. }),
        "an unforced legacy removal is a no-op, not a failure: {:?}",
        result
    );
    assert!(
        ws_dir.join("merged.rs").is_dir(),
        "an unforced legacy removal must not recursively delete a user directory"
    );
    assert_eq!(read(ws_dir.join("merged.rs").join("user.txt")), "mine\n");
    let _ = fs::remove_dir_all(&root);
}

/// C00-10: an intent that cannot prove a path's pre-restore state — a legacy journal with no
/// `parent_revision`, or one whose parent manifest has since been collected — must not fall back to
/// blind replay. Unknown pre-state is not permission to write: the default replay refuses, changes
/// nothing on disk and keeps the journal; only an explicit `force` on this call proceeds.
#[test]
fn test_restore_recovery_with_unknown_prestate_refuses_to_overwrite_a_user_edit() {
    let root = temp_root("recovery-unknown-prestate");
    let ws_dir = root.join("ws");
    fs::create_dir_all(&ws_dir).unwrap();
    fs::write(ws_dir.join("a.rs"), "v1\n").unwrap();
    let workspace = WorkspaceCap::new(&ws_dir).unwrap();
    let store = SnapshotStore::new(root.join("snapshots"));
    let target = store.capture_revision(&workspace, None, None, 0).unwrap();
    let entry = target.tracked_files.get("a.rs").unwrap().clone();

    // Two journals that carry no provable pre-state: a legacy one without a parent, and one whose
    // parent revision is no longer in the store (collected while the workspace was idle).
    for (label, parent_revision) in [
        ("no recorded parent", serde_json::Value::Null),
        (
            "parent manifest collected",
            serde_json::Value::String(format!("r-{}", "0".repeat(32))),
        ),
    ] {
        // The user's content at the path the journal intends to write, and a path the journal would
        // create from scratch: nothing may be touched while the conflict stands.
        fs::write(ws_dir.join("a.rs"), "user edit\n").unwrap();
        let _ = fs::remove_file(ws_dir.join("created.rs"));
        let journal = serde_json::json!({
            "version": 1,
            "target_revision": target.revision_id.as_str(),
            "parent_revision": parent_revision,
            "created_at_ms": 0,
            // The original operation was forced; that is not authorization for a later edit.
            "force": true,
            "operations": [
                { "op": "write", "path": "a.rs", "sha256": entry.sha256, "mode": entry.mode },
                { "op": "write", "path": "created.rs", "sha256": entry.sha256, "mode": entry.mode }
            ]
        });
        fs::write(store.intent_path(), serde_json::to_vec(&journal).unwrap()).unwrap();

        let refused = store.recover_pending_restore(&workspace).unwrap();
        match refused {
            RestoreResult::Conflict {
                conflicting_paths, ..
            } => {
                assert_eq!(
                    conflicting_paths
                        .iter()
                        .map(|c| c.path.as_str())
                        .collect::<Vec<_>>(),
                    vec!["a.rs"],
                    "{}: only the unprovable path conflicts",
                    label
                );
            }
            other => panic!(
                "{}: unknown pre-state must be refused by default, got {:?}",
                label, other
            ),
        }
        assert_eq!(
            read(ws_dir.join("a.rs")),
            "user edit\n",
            "{}: the default replay must not write",
            label
        );
        assert!(
            fs::symlink_metadata(ws_dir.join("created.rs")).is_err(),
            "{}: a refused journal must not be partially applied",
            label
        );
        assert!(
            store.pending_restore().unwrap().is_some(),
            "{}: the journal must be kept for an explicit decision",
            label
        );

        let forced = store
            .recover_pending_restore_with_force(&workspace, true)
            .unwrap();
        assert!(
            matches!(forced, RestoreResult::Applied { .. }),
            "{}: this call's explicit force must finish it, got {:?}",
            label,
            forced
        );
        assert_eq!(read(ws_dir.join("a.rs")), "v1\n", "{}", label);
        assert_eq!(read(ws_dir.join("created.rs")), "v1\n", "{}", label);
        assert!(store.pending_restore().unwrap().is_none(), "{}", label);
    }
    let _ = fs::remove_dir_all(&root);
}

/// C00-10: a fork that crashed mid-restore leaves its own restore journal. The fork intent proves
/// only that the journal belongs to this fork — not that a path still holds what the journal
/// recorded. A file the user edited while Kroma was down therefore stops the automatic completion:
/// the fork is abandoned, the workspace is left exactly as the user left it, and the half-restore
/// journal stays on disk for an explicit decision.
#[test]
fn test_fork_recovery_does_not_overwrite_a_user_edit_made_while_kroma_was_down() {
    let root = temp_root("fork-recovery-live-edit");
    let ws_dir = root.join("ws");
    fs::create_dir_all(&ws_dir).unwrap();
    fs::write(ws_dir.join("a.rs"), "v1\n").unwrap();
    fs::write(ws_dir.join("b.rs"), "b1\n").unwrap();

    let workspace = WorkspaceCap::new(&ws_dir).unwrap();
    let store = SnapshotStore::new(root.join("snapshots"));
    let mut journal = SessionJournal::create(root.join("sessions"), &ws_dir).unwrap();
    journal
        .append_item(
            0,
            TranscriptItem::User(UserMessage {
                content: "start".to_string(),
            }),
        )
        .unwrap();
    let baseline = capture_and_record(&store, &mut journal, &workspace, None, 0);
    journal
        .append_item(
            1,
            TranscriptItem::User(UserMessage {
                content: "edit".to_string(),
            }),
        )
        .unwrap();
    fs::write(ws_dir.join("a.rs"), "v2\n").unwrap();
    fs::write(ws_dir.join("b.rs"), "b2\n").unwrap();
    let mutated = capture_and_record(&store, &mut journal, &workspace, Some(&baseline), 1);

    // A fork to turn 0 that crashed after both its fork intent and its restore intent were durable.
    store
        .write_fork_intent(&ForkIntent {
            version: 1,
            session_id: journal.session_id().to_string(),
            workspace_root: workspace.root().to_string_lossy().into_owned(),
            forked_at_turn: 0,
            target_revision: baseline.clone(),
            expected_revision: Some(mutated.clone()),
            created_at_ms: 0,
        })
        .unwrap();
    let crashed = store
        .restore_revision_with_fault(
            &workspace,
            &baseline,
            Some(&mutated),
            false,
            Some(RestoreFault::AfterIntentWritten),
        )
        .unwrap();
    assert!(matches!(crashed, RestoreResult::RecoveryRequired { .. }));
    assert!(store.pending_restore().unwrap().is_some());

    // While Kroma was down the user edited one of the files the fork's journal would write.
    fs::write(ws_dir.join("a.rs"), "user edit\n").unwrap();

    match store
        .recover_pending_fork(&workspace, &mut journal)
        .unwrap()
    {
        ForkRecoveryOutcome::Abandoned {
            forked_at_turn,
            target_revision,
            conflicting_paths,
        } => {
            assert_eq!(forked_at_turn, 0);
            assert_eq!(target_revision.as_str(), baseline.as_str());
            assert_eq!(
                conflicting_paths
                    .iter()
                    .map(|c| c.path.as_str())
                    .collect::<Vec<_>>(),
                vec!["a.rs"],
                "only the path edited while Kroma was down conflicts"
            );
        }
        other => panic!(
            "the fork must be abandoned instead of overwriting the user's edit, got {:?}",
            other
        ),
    }
    assert_eq!(
        read(ws_dir.join("a.rs")),
        "user edit\n",
        "the user's edit must survive the abandoned fork"
    );
    assert_eq!(
        read(ws_dir.join("b.rs")),
        "b2\n",
        "an abandoned fork must not half-apply its journal"
    );
    assert!(
        store.pending_fork_intent().unwrap().is_none(),
        "the abandoned fork intent is cleared"
    );
    assert!(
        store.pending_restore().unwrap().is_some(),
        "the half-restore journal stays so recovery still needs an explicit decision"
    );
    assert_eq!(
        journal.latest_revision_id().as_deref(),
        Some(mutated.as_str()),
        "the session leaf never moved"
    );
    let _ = fs::remove_dir_all(&root);
}

/// C00-10 (§8.6): an interrupted restore is replayed on recovery, but a file the user edited
/// *after* the crash is external work the journal never saw. Replaying the write would silently
/// overwrite it, so recovery must report the conflict and keep the journal; only an explicit
/// forced replay overwrites it.
#[test]
fn test_restore_recovery_refuses_to_overwrite_a_file_edited_after_the_crash() {
    let root = temp_root("recovery-live-edit-write");
    let ws_dir = root.join("ws");
    fs::create_dir_all(ws_dir.join("src")).unwrap();
    fs::write(ws_dir.join("src/a.rs"), "v1\n").unwrap();
    fs::write(ws_dir.join("src/b.rs"), "b1\n").unwrap();

    let workspace = WorkspaceCap::new(&ws_dir).unwrap();
    let store = SnapshotStore::new(root.join("snapshots"));
    let baseline = store.capture_revision(&workspace, None, None, 0).unwrap();
    fs::write(ws_dir.join("src/a.rs"), "v2\n").unwrap();
    fs::write(ws_dir.join("src/b.rs"), "b2\n").unwrap();
    let mutated = store
        .capture_revision(&workspace, Some(&baseline.revision_id), None, 1)
        .unwrap();

    // Crash right after the intent was journaled, before any file was touched.
    let crashed = store
        .restore_revision_with_fault(
            &workspace,
            &baseline.revision_id,
            Some(&mutated.revision_id),
            false,
            Some(RestoreFault::AfterIntentWritten),
        )
        .unwrap();
    assert!(matches!(crashed, RestoreResult::RecoveryRequired { .. }));

    // While Kroma was down the user edited one of the files the journal intends to write.
    fs::write(ws_dir.join("src/a.rs"), "user edit\n").unwrap();

    let recovered = store.recover_pending_restore(&workspace).unwrap();
    match recovered {
        RestoreResult::Conflict {
            conflicting_paths, ..
        } => {
            assert_eq!(
                conflicting_paths
                    .iter()
                    .map(|c| c.path.as_str())
                    .collect::<Vec<_>>(),
                vec!["src/a.rs"],
                "only the path edited after the crash conflicts"
            );
        }
        other => panic!(
            "recovery must refuse to overwrite a post-crash edit, got {:?}",
            other
        ),
    }
    assert_eq!(
        read(ws_dir.join("src/a.rs")),
        "user edit\n",
        "the user's edit must survive a refused recovery"
    );
    assert_eq!(
        read(ws_dir.join("src/b.rs")),
        "b2\n",
        "a refused recovery must not half-apply the journal"
    );
    assert!(
        store.pending_restore().unwrap().is_some(),
        "the intent must survive a refused recovery so it can still be finished or forced"
    );

    // An explicit forced replay is the operator acknowledging the overwrite.
    let forced = store
        .recover_pending_restore_with_force(&workspace, true)
        .unwrap();
    assert!(
        matches!(forced, RestoreResult::Applied { .. }),
        "a forced replay must finish the interrupted restore, got {:?}",
        forced
    );
    assert_eq!(read(ws_dir.join("src/a.rs")), "v1\n");
    assert_eq!(read(ws_dir.join("src/b.rs")), "b1\n");
    assert!(store.pending_restore().unwrap().is_none());
    let _ = fs::remove_dir_all(&root);
}

/// C00-10 (§8.6): the removal half of the same contract. A file the user created on a path the
/// interrupted restore intends to delete is not the content the parent revision recorded, so the
/// replay must not delete it silently.
#[test]
fn test_restore_recovery_refuses_to_delete_a_file_created_after_the_crash() {
    let root = temp_root("recovery-live-edit-remove");
    let ws_dir = root.join("ws");
    fs::create_dir_all(&ws_dir).unwrap();
    fs::write(ws_dir.join("keep.rs"), "k1\n").unwrap();
    fs::write(ws_dir.join("gone.rs"), "g1\n").unwrap();

    let workspace = WorkspaceCap::new(&ws_dir).unwrap();
    let store = SnapshotStore::new(root.join("snapshots"));
    let baseline = store.capture_revision(&workspace, None, None, 0).unwrap();
    fs::remove_file(ws_dir.join("gone.rs")).unwrap();
    let target = store
        .capture_revision(&workspace, Some(&baseline.revision_id), None, 1)
        .unwrap();

    // Back to the baseline state, so the only difference the restore has to apply is the removal.
    fs::write(ws_dir.join("gone.rs"), "g1\n").unwrap();

    // Restoring `target` must remove `gone.rs`; crash right after the intent was journaled.
    let crashed = store
        .restore_revision_with_fault(
            &workspace,
            &target.revision_id,
            Some(&baseline.revision_id),
            false,
            Some(RestoreFault::AfterIntentWritten),
        )
        .unwrap();
    assert!(matches!(crashed, RestoreResult::RecoveryRequired { .. }));

    // While Kroma was down the user recreated the path with their own content.
    fs::write(ws_dir.join("gone.rs"), "mine\n").unwrap();

    let recovered = store.recover_pending_restore(&workspace).unwrap();
    match recovered {
        RestoreResult::Conflict {
            conflicting_paths, ..
        } => {
            assert_eq!(
                conflicting_paths
                    .iter()
                    .map(|c| c.path.as_str())
                    .collect::<Vec<_>>(),
                vec!["gone.rs"]
            );
        }
        other => panic!(
            "recovery must refuse to delete a post-crash file, got {:?}",
            other
        ),
    }
    assert_eq!(
        read(ws_dir.join("gone.rs")),
        "mine\n",
        "the user's file must survive a refused recovery"
    );
    assert!(store.pending_restore().unwrap().is_some());

    let forced = store
        .recover_pending_restore_with_force(&workspace, true)
        .unwrap();
    assert!(
        matches!(forced, RestoreResult::Applied { .. }),
        "a forced replay must finish the interrupted restore, got {:?}",
        forced
    );
    assert!(
        fs::symlink_metadata(ws_dir.join("gone.rs")).is_err(),
        "a forced replay applies the recorded removal"
    );
    assert!(store.pending_restore().unwrap().is_none());
    let _ = fs::remove_dir_all(&root);
}

// ---- F1 (v1.5.1): session store creation refuses a symlinked leaf ---------------------------

#[cfg(unix)]
#[test]
fn test_session_create_refuses_symlinked_session_dir() {
    let root = temp_root("f1-session-symlink");
    let real = root.join("real");
    fs::create_dir_all(&real).unwrap();
    let link = root.join("sessions");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let ws = root.join("ws");
    fs::create_dir_all(&ws).unwrap();

    let err = match SessionJournal::create(&link, &ws) {
        Ok(_) => panic!("a symlinked session directory must be refused, not chmod-ed through"),
        Err(e) => e,
    };
    let msg = format!("{}", err);
    assert!(
        msg.contains("not a real directory"),
        "unexpected error message: {}",
        msg
    );
    let _ = fs::remove_dir_all(&root);
}

// ---- F2 (v1.5.1): store fallback tolerates symlinked ancestors, refuses symlinked leaf -------

#[cfg(unix)]
#[test]
fn test_store_layout_tolerates_symlinked_ancestor_but_refuses_symlinked_leaf() {
    let root = temp_root("f2-store-symlink");
    let real = root.join("real");
    fs::create_dir_all(&real).unwrap();
    let link = root.join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();

    // A symlinked *ancestor* (macOS `/var`, Android `/data/data`) must still work: the fallback
    // creates the store and only constrains the final component.
    let nested = SnapshotStore::new(link.join("snapshots"));
    nested
        .ensure_layout()
        .expect("a symlinked ancestor must be tolerated");
    assert!(real.join("snapshots").is_dir());

    // A symlinked *leaf* must be refused rather than chmod-ed through.
    let leaf = SnapshotStore::new(root.join("link"));
    let err = leaf
        .ensure_layout()
        .expect_err("a symlinked store root must be refused");
    assert!(
        format!("{}", err).contains("not a real directory"),
        "unexpected error: {}",
        err
    );
    let _ = fs::remove_dir_all(&root);
}
