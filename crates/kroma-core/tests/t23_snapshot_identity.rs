//! T23: snapshot identity and recovery invariants — **measurement first**.
//!
//! The question the ADR has to answer: is "content identity" being used where "history event
//! identity" is meant? `RevisionId` is content-addressed, while `WorkspaceRevision` also carries
//! parent/turn/WAL metadata. When a workspace returns to earlier content (A → B → A), the second `A`
//! necessarily maps to the same content id — so either the manifest is rewritten (and the parent
//! chain loses an event), or the design keeps history somewhere else.
//!
//! These tests record what the implementation actually does. They are not a claim that undo is
//! broken; the ADR states exactly which invariants hold and which are not promised.

use kroma_core::revision::{RestoreResult, RevisionId, SnapshotStore};
use kroma_core::session::SessionJournal;
use kroma_core::workspace::WorkspaceCap;
use std::path::{Path, PathBuf};

fn scratch(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("kroma-t23-{}-{}", tag, uuid::Uuid::new_v4()));
    std::fs::create_dir_all(root.join("ws")).unwrap();
    std::fs::create_dir_all(root.join("state")).unwrap();
    root
}

fn write(ws: &Path, name: &str, contents: &str) {
    std::fs::write(ws.join(name), contents).unwrap();
}

/// A → B → A: two history events, one content address.
#[test]
fn a_b_a_keeps_every_event_but_rewrites_the_manifest_parent() {
    let root = scratch("aba");
    let ws = root.join("ws");
    let store = SnapshotStore::new(root.join("state/snapshots"));
    let cap = WorkspaceCap::new(&ws).unwrap();
    let mut journal = SessionJournal::create(root.join("state/sessions"), &ws).unwrap();

    write(&ws, "a.rs", "fn a() { 1 }\n");
    let a1 = store
        .capture_revision(&cap, None, None, 0)
        .expect("capture A");
    journal
        .record_revision_capture(
            0,
            a1.revision_id.as_str(),
            None,
            a1.changed_paths.clone(),
            1,
        )
        .unwrap();

    write(&ws, "a.rs", "fn a() { 2 }\n");
    let b = store
        .capture_revision(&cap, Some(&a1.revision_id), None, 1)
        .expect("capture B");
    journal
        .record_revision_capture(
            1,
            b.revision_id.as_str(),
            Some(a1.revision_id.as_str()),
            b.changed_paths.clone(),
            1,
        )
        .unwrap();

    // Back to the exact bytes of A.
    write(&ws, "a.rs", "fn a() { 1 }\n");
    let a2 = store
        .capture_revision(&cap, Some(&b.revision_id), None, 2)
        .expect("capture A again");
    journal
        .record_revision_capture(
            2,
            a2.revision_id.as_str(),
            Some(b.revision_id.as_str()),
            a2.changed_paths.clone(),
            1,
        )
        .unwrap();

    // Measurement 1: the same content yields the same id — this is content addressing working.
    assert_eq!(
        a1.revision_id, a2.revision_id,
        "the same content must map to the same content id"
    );

    // Measurement 2: the manifest on disk now records A's parent as B, because there is exactly one
    // manifest per content id. The A→B event (parent = A) is no longer representable in the manifest.
    let stored = store.load_revision(&a2.revision_id).expect("manifest");
    assert_eq!(
        stored.parent_revision_id.as_ref().map(RevisionId::as_str),
        Some(b.revision_id.as_str()),
        "the manifest parent was rewritten to the most recent capture"
    );
    assert_ne!(
        stored.parent_revision_id.as_ref(),
        None,
        "the second capture kept a parent, it just is not the first one"
    );

    // Measurement 3: the *session WAL* keeps both A events with their own turn and parent, which is
    // why undo targets come from the WAL and not from the manifest's parent pointer.
    let records = journal.revision_records();
    let a_events: Vec<_> = records
        .iter()
        .filter(|record| record.revision_id == a1.revision_id.as_str())
        .collect();
    assert_eq!(
        a_events.len(),
        2,
        "both A events must survive in the WAL: {records:?}"
    );
    assert_eq!(a_events[0].turn, 0);
    assert_eq!(a_events[1].turn, 2);
    assert_eq!(
        a_events[1].parent_revision_id.as_deref(),
        Some(b.revision_id.as_str())
    );

    // Measurement 4: undo targets resolve per turn, so the A→B→A history is still addressable.
    assert_eq!(
        journal.revision_at_or_before_turn(0).map(|r| r.revision_id),
        Some(a1.revision_id.as_str().to_string())
    );
    assert_eq!(
        journal.revision_at_or_before_turn(1).map(|r| r.revision_id),
        Some(b.revision_id.as_str().to_string())
    );
    assert_eq!(
        journal.revision_at_or_before_turn(2).map(|r| r.revision_id),
        Some(a1.revision_id.as_str().to_string()),
        "the third turn is the second A event"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// Two different histories that reach the same content share one content id — by design.
#[test]
fn two_paths_to_the_same_content_share_a_content_id() {
    let root = scratch("branches");
    let store_a = SnapshotStore::new(root.join("state/a"));
    let store_b = SnapshotStore::new(root.join("state/b"));
    let ws_a = root.join("ws-a");
    let ws_b = root.join("ws-b");
    std::fs::create_dir_all(&ws_a).unwrap();
    std::fs::create_dir_all(&ws_b).unwrap();
    write(&ws_a, "x.rs", "fn x() { 1 }\n");
    write(&ws_b, "x.rs", "fn x() { 1 }\n");

    let cap_a = WorkspaceCap::new(&ws_a).unwrap();
    let cap_b = WorkspaceCap::new(&ws_b).unwrap();
    let first = store_a.capture_revision(&cap_a, None, None, 0).unwrap();
    // The second store reaches the same bytes by a different route (a different parent chain).
    let other_parent = {
        write(&ws_a, "x.rs", "fn x() { 2 }\n");
        let mid = store_a
            .capture_revision(&cap_a, Some(&first.revision_id), None, 1)
            .unwrap();
        write(&ws_a, "x.rs", "fn x() { 1 }\n");
        store_a
            .capture_revision(&cap_a, Some(&mid.revision_id), None, 2)
            .unwrap()
    };
    let second = store_b.capture_revision(&cap_b, None, None, 0).unwrap();

    assert_eq!(
        first.revision_id, second.revision_id,
        "identical trees must share a content id"
    );
    assert_eq!(
        first.revision_id, other_parent.revision_id,
        "returning to the same bytes must share a content id"
    );
    // But the *manifests* differ in the history metadata they carry, and the store keeps one per
    // content id, so the last capture wins. That is the documented non-guarantee.
    let a = store_a.load_revision(&first.revision_id).unwrap();
    assert!(a.parent_revision_id.is_some(), "store A saw a parent");
    let b = store_b.load_revision(&second.revision_id).unwrap();
    assert!(b.parent_revision_id.is_none(), "store B saw no parent");

    let _ = std::fs::remove_dir_all(&root);
}

/// Reopening the store and the journal loses nothing: recovery replays committed state.
#[test]
fn crash_replay_preserves_revisions_and_undo_targets() {
    let root = scratch("replay");
    let ws = root.join("ws");
    let store_path = root.join("state/snapshots");
    let journals = root.join("state/sessions");
    let mut ids = Vec::new();
    let journal_path;
    {
        let store = SnapshotStore::new(&store_path);
        let cap = WorkspaceCap::new(&ws).unwrap();
        let mut journal = SessionJournal::create(&journals, &ws).unwrap();
        let mut parent: Option<RevisionId> = None;
        for (turn, body) in [
            (0usize, "fn v() { 0 }\n"),
            (1, "fn v() { 1 }\n"),
            (2, "fn v() { 2 }\n"),
        ] {
            write(&ws, "v.rs", body);
            let revision = store
                .capture_revision(&cap, parent.as_ref(), None, turn)
                .unwrap();
            journal
                .record_revision_capture(
                    turn,
                    revision.revision_id.as_str(),
                    parent.as_ref().map(RevisionId::as_str),
                    revision.changed_paths.clone(),
                    1,
                )
                .unwrap();
            ids.push(revision.revision_id.as_str().to_string());
            parent = Some(revision.revision_id);
        }
        journal_path = journal.path().to_path_buf();
        // Dropped here, as a crash would.
    }

    let reopened = SessionJournal::open(&journal_path).expect("reopen");
    let store = SnapshotStore::new(&store_path);
    assert_eq!(reopened.revision_records().len(), 3);
    for (turn, id) in ids.iter().enumerate() {
        assert_eq!(
            reopened
                .revision_at_or_before_turn(turn)
                .map(|r| r.revision_id),
            Some(id.clone()),
            "turn {} lost its undo target",
            turn
        );
        assert!(
            store.load_revision(&RevisionId::new(id.clone())).is_ok(),
            "revision {} is not loadable after replay",
            id
        );
    }

    let _ = std::fs::remove_dir_all(&root);
}

/// External changes are detected, not silently rolled over.
#[test]
fn external_chmod_and_rename_are_detected_before_restore() {
    let root = scratch("external");
    let ws = root.join("ws");
    let store = SnapshotStore::new(root.join("state/snapshots"));
    let cap = WorkspaceCap::new(&ws).unwrap();
    write(&ws, "a.rs", "fn a() {}\n");
    write(&ws, "b.rs", "fn b() {}\n");
    let base = store.capture_revision(&cap, None, None, 0).unwrap();

    // A rename: the tracked path disappeared and a new one appeared. Conflicts are reported as a
    // result variant (the caller decides what to tell the user), not as an error - so the invariant
    // to assert is "a conflict whose path is the renamed one", not "an Err".
    std::fs::rename(ws.join("b.rs"), ws.join("c.rs")).unwrap();
    match store
        .restore_revision(&cap, &base.revision_id, Some(&base.revision_id), false)
        .expect("a conflict is a result, not an error")
    {
        RestoreResult::Conflict {
            conflicting_paths, ..
        } => {
            assert!(
                conflicting_paths.iter().any(|c| c.path == "b.rs"),
                "the vanished path must be reported: {conflicting_paths:?}"
            );
        }
        other => panic!("expected a conflict for the renamed path, got {other:?}"),
    }
    // Nothing was written while the conflict stood.
    assert!(ws.join("c.rs").exists(), "the rename must be left alone");
    assert!(!ws.join("b.rs").exists());

    // A mode change is content change too (the content id hashes the mode).
    std::fs::rename(ws.join("c.rs"), ws.join("b.rs")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(ws.join("a.rs"), std::fs::Permissions::from_mode(0o755)).unwrap();
        match store
            .restore_revision(&cap, &base.revision_id, Some(&base.revision_id), false)
            .expect("a conflict is a result, not an error")
        {
            RestoreResult::Conflict {
                conflicting_paths, ..
            } => assert!(
                conflicting_paths.iter().any(|c| c.path == "a.rs"),
                "the chmod must be reported: {conflicting_paths:?}"
            ),
            other => panic!("expected a conflict for the chmod, got {other:?}"),
        }
        // With force it proceeds and restores the recorded mode.
        match store
            .restore_revision(&cap, &base.revision_id, Some(&base.revision_id), true)
            .expect("force-restore is not an error case")
        {
            RestoreResult::Applied { .. } => {}
            other => panic!("force-restore must apply, got {other:?}"),
        }
        let mode = std::fs::metadata(ws.join("a.rs"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o644, "restore must restore the recorded mode");
    }

    let _ = std::fs::remove_dir_all(&root);
}

/// Multiple undo steps walk the WAL events, not the (rewritten) manifest parent chain.
#[test]
fn multiple_undo_steps_follow_the_session_events() {
    let root = scratch("multi-undo");
    let ws = root.join("ws");
    let store = SnapshotStore::new(root.join("state/snapshots"));
    let cap = WorkspaceCap::new(&ws).unwrap();
    let mut journal = SessionJournal::create(root.join("state/sessions"), &ws).unwrap();

    // A → B → A, recorded as three events.
    let mut ids = Vec::new();
    let mut parent: Option<RevisionId> = None;
    for (turn, body) in [
        (0usize, "fn v() { 0 }\n"),
        (1, "fn v() { 1 }\n"),
        (2, "fn v() { 0 }\n"),
    ] {
        write(&ws, "v.rs", body);
        let revision = store
            .capture_revision(&cap, parent.as_ref(), None, turn)
            .unwrap();
        journal
            .record_revision_capture(
                turn,
                revision.revision_id.as_str(),
                parent.as_ref().map(RevisionId::as_str),
                revision.changed_paths.clone(),
                1,
            )
            .unwrap();
        ids.push(revision.revision_id.clone());
        parent = Some(revision.revision_id);
    }
    assert_eq!(
        ids[0], ids[2],
        "the first and third events share a content id"
    );

    // Undo to turn 1 (the middle state), then to turn 0.
    let target_b = journal
        .revision_at_or_before_turn(1)
        .expect("turn 1 target")
        .revision_id;
    assert_eq!(target_b, ids[1].as_str());
    // `current_expected_rev` is the revision the caller believes the workspace is at (turn 2 here).
    // Without it the guard compares live bytes against the *target*, which is the stricter reading and
    // would refuse a legitimate undo - so pass the recorded state, as the CLI does.
    let applied = store
        .restore_revision(&cap, &ids[1], Some(&ids[2]), false)
        .expect("restore to B");
    assert!(matches!(applied, RestoreResult::Applied { .. }));
    assert_eq!(
        std::fs::read_to_string(ws.join("v.rs")).unwrap(),
        "fn v() { 1 }\n"
    );

    let target_a = journal
        .revision_at_or_before_turn(0)
        .expect("turn 0 target")
        .revision_id;
    assert_eq!(target_a, ids[0].as_str());
    let applied = store
        .restore_revision(&cap, &ids[0], Some(&ids[1]), false)
        .expect("restore to A");
    assert!(matches!(applied, RestoreResult::Applied { .. }));
    assert_eq!(
        std::fs::read_to_string(ws.join("v.rs")).unwrap(),
        "fn v() { 0 }\n"
    );

    // Restoring the same target twice is idempotent.
    let again = store
        .restore_revision(&cap, &ids[0], Some(&ids[0]), false)
        .expect("restore to A again");
    assert!(matches!(again, RestoreResult::Applied { .. }));
    assert_eq!(
        std::fs::read_to_string(ws.join("v.rs")).unwrap(),
        "fn v() { 0 }\n"
    );

    let _ = std::fs::remove_dir_all(&root);
}
