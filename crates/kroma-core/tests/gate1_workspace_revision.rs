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
    ConflictKind, RestoreFault, RestoreResult, RevisionId, SnapshotStore,
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
        vec!["keep.txt".to_string(), "src/a.rs".to_string(), "src/b.rs".to_string()]
    );
    assert_eq!(baseline.tracked_files["src/a.rs"].mode & 0o777, 0o600);

    // One kroma turn: modify a file, delete a file, create a file.
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
        get_file_mode(ws_dir.join("src/a.rs")),
        0o600,
        "the tracked file mode must be restored, not reset to the process default"
    );
    assert!(!store.intent_path().exists(), "a clean restore clears its intent journal");
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
    create_symlink_file(outside_dir.join("secret.txt"), ws_dir.join("escape.txt"));

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
    create_symlink_file(outside_dir.join("secret.txt"), ws_dir.join("real.txt"));

    let refused = store
        .restore_revision(&workspace, &baseline.revision_id, Some(&baseline.revision_id), false)
        .unwrap();
    match refused {
        RestoreResult::Conflict { conflicting_paths, .. } => {
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
        .append_item(0, TranscriptItem::User(UserMessage { content: "task".into() }))
        .unwrap();
    let r0 = capture_and_record(&store, &mut journal, &workspace, None, 0);

    let mut parent = r0;
    for turn in 1..=3usize {
        fs::write(ws_dir.join("f.txt"), format!("v{}\n", turn)).unwrap();
        journal
            .append_item(turn, TranscriptItem::User(UserMessage { content: format!("turn {}", turn) }))
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
            journal.revision_records().last().map(|r| r.revision_id.as_str()),
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
        .append_item(0, TranscriptItem::User(UserMessage { content: "task".into() }))
        .unwrap();
    let r0 = capture_and_record(&store, &mut journal, &workspace, None, 0);
    fs::write(ws_dir.join("f.txt"), "v1\n").unwrap();
    journal
        .append_item(1, TranscriptItem::User(UserMessage { content: "turn 1".into() }))
        .unwrap();
    let r1 = capture_and_record(&store, &mut journal, &workspace, Some(&r0), 1);
    fs::write(ws_dir.join("f.txt"), "v2\n").unwrap();
    journal
        .append_item(2, TranscriptItem::User(UserMessage { content: "turn 2".into() }))
        .unwrap();
    let r2 = capture_and_record(&store, &mut journal, &workspace, Some(&r1), 2);
    assert_eq!(journal.revision_records().len(), 3);

    // Fork back to turn 1 and mutate on the new branch.
    journal.fork_at_turn(1).unwrap();
    fs::write(ws_dir.join("f.txt"), "v2-branch\n").unwrap();
    journal
        .append_item(2, TranscriptItem::User(UserMessage { content: "turn 2 branch".into() }))
        .unwrap();
    let r2b = capture_and_record(&store, &mut journal, &workspace, Some(&r1), 2);
    let records = journal.revision_records();
    assert_eq!(records.len(), 3, "the abandoned branch's revision must be filtered out");
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
        .restore_revision(&workspace, &baseline.revision_id, Some(&mutated.revision_id), false)
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
        .restore_revision(&workspace, &file_rev.revision_id, Some(&dir_rev.revision_id), false)
        .unwrap();
    assert!(
        matches!(back_to_file, RestoreResult::Applied { .. }),
        "directory -> file must not wedge: {:?}",
        back_to_file
    );
    assert!(fs::symlink_metadata(ws_dir.join("foo")).unwrap().file_type().is_file());
    assert_eq!(read(ws_dir.join("foo")), "foo v1\n");
    assert!(!ws_dir.join("foo/bar.txt").exists());

    // And forward again to the directory revision.
    let back_to_dir = store
        .restore_revision(&workspace, &dir_rev.revision_id, Some(&file_rev.revision_id), false)
        .unwrap();
    assert!(
        matches!(back_to_dir, RestoreResult::Applied { .. }),
        "file -> directory must not wedge: {:?}",
        back_to_dir
    );
    assert!(fs::symlink_metadata(ws_dir.join("foo")).unwrap().file_type().is_dir());
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
        .restore_revision(&workspace, &baseline.revision_id, Some(&mutated.revision_id), false)
        .unwrap();
    match mode_conflict {
        RestoreResult::Conflict { conflicting_paths, .. } => {
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
        .restore_revision(&workspace, &baseline.revision_id, Some(&mutated.revision_id), false)
        .unwrap();
    match link_conflict {
        RestoreResult::Conflict { conflicting_paths, .. } => {
            assert_eq!(conflicting_paths.len(), 1, "got {:?}", conflicting_paths);
            assert_eq!(conflicting_paths[0].path, "g.rs");
            assert_eq!(conflicting_paths[0].kind, ConflictKind::CreatedExternally);
        }
        other => panic!("expected Conflict for a created symlink, got {:?}", other),
    }
    assert!(
        fs::symlink_metadata(ws_dir.join("g.rs")).unwrap().file_type().is_symlink(),
        "a refused restore must not touch the user's symlink"
    );
    assert_eq!(read(outside_dir.join("secret.txt")), "OUTSIDE_SECRET\n");

    // External symlink swapped onto a tracked path that the expected revision has -> ModifiedExternally.
    fs::remove_file(ws_dir.join("x.rs")).unwrap();
    create_symlink_file(outside_dir.join("secret.txt"), ws_dir.join("x.rs"));
    let swap_conflict = store
        .restore_revision(&workspace, &baseline.revision_id, Some(&mutated.revision_id), false)
        .unwrap();
    match swap_conflict {
        RestoreResult::Conflict { conflicting_paths, .. } => {
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
        .restore_revision(&workspace, &grown.revision_id, Some(&baseline.revision_id), false)
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
        .restore_revision(&workspace, &baseline.revision_id, Some(&mutated.revision_id), false)
        .unwrap();
    assert!(
        matches!(refused, RestoreResult::Conflict { .. }),
        "an obstructing directory must be a conflict without force: {:?}",
        refused
    );
    assert!(ws_dir.join("foo/nested/inner.txt").exists());

    let forced = store
        .restore_revision(&workspace, &baseline.revision_id, Some(&mutated.revision_id), true)
        .unwrap();
    assert!(
        matches!(forced, RestoreResult::Applied { .. }),
        "force must clear the obstructing directory instead of wedging: {:?}",
        forced
    );
    assert!(fs::symlink_metadata(ws_dir.join("foo")).unwrap().file_type().is_file());
    assert_eq!(read(ws_dir.join("foo")), "foo v1\n");
    assert!(!ws_dir.join("foo/nested").exists());

    // External symlink parked on the tracked path: force must unlink the link, not its target.
    fs::remove_file(ws_dir.join("foo")).unwrap();
    create_symlink_file(outside_dir.join("secret.txt"), ws_dir.join("foo"));
    let forced_link = store
        .restore_revision(&workspace, &baseline.revision_id, Some(&mutated.revision_id), true)
        .unwrap();
    assert!(
        matches!(forced_link, RestoreResult::Applied { .. }),
        "force must replace the symlink itself: {:?}",
        forced_link
    );
    assert!(fs::symlink_metadata(ws_dir.join("foo")).unwrap().file_type().is_file());
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
    let policy = PolicyEngine::new(PolicyMode::AllowWrite, Arc::new(NonInteractiveApprovalHandler));
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
    assert_eq!(summary.status, RunStatus::Completed, "{:?}", summary.terminal_reason);
    assert_eq!(read(ws_dir.join("src/app.rs")), "fn main() { println!(\"hi\"); }\n");

    let resumed = SessionJournal::open(&wal_path).unwrap();
    let pre_run_leaf = resumed.leaf_id().map(|s| s.to_string());
    assert!(pre_run_leaf.is_some(), "run 1 must have left an active leaf");
    let pre_run_turn = resumed.max_active_turn();

    // Run 2 (resume): write "bye" on the same session.
    let env = Arc::new(LocalHostEnv::new(&ws_dir, state_dir.clone()).unwrap());
    let policy = PolicyEngine::new(PolicyMode::AllowWrite, Arc::new(NonInteractiveApprovalHandler));
    let runtime = build_default_runtime(env, policy, budget.clone(), None);
    let cfg = AgentRunConfig::new("write bye", budget.clone());
    let logger = EventLogger::new(cfg.run_id, Some(state_dir.clone()), false).unwrap();
    let (_tx, rx) = watch::channel(false);
    let (summary, _logger) = AgentRunner::new(
        cfg,
        scripted_write_provider("call_bye", "src/app.rs", "fn main() { println!(\"bye\"); }\n"),
        runtime,
        rx,
    )
    .with_session_journal(resumed)
    .with_snapshot_store(store.clone())
    .run(logger)
    .await;
    assert_eq!(summary.status, RunStatus::Completed, "{:?}", summary.terminal_reason);
    assert_eq!(read(ws_dir.join("src/app.rs")), "fn main() { println!(\"bye\"); }\n");

    let mut journal = SessionJournal::open(&wal_path).unwrap();
    let baseline_records: Vec<_> = journal
        .revision_records()
        .into_iter()
        .filter(|r| r.turn == pre_run_turn)
        .collect();
    assert!(
        baseline_records.iter().any(|r| r.leaf_id.as_deref() == pre_run_leaf.as_deref()),
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
