//! KRV4-01: the revision manifest has one size contract, enforced on both sides.
//!
//! The defect this pins: `capture_revision` wrote whatever `to_vec_pretty` produced, while
//! `load_revision` refused anything above `MAX_REVISION_RECORD_BYTES`. A large workspace could
//! therefore produce a revision that persisted successfully and could never be read back, and
//! `list_revisions` hid it instead of complaining - so the newest revision simply appeared to
//! vanish.

use std::fs;
use std::path::PathBuf;

use kroma_core::revision::SnapshotStore;
use kroma_core::workspace::WorkspaceCap;

fn scratch(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("kroma-krv401-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).expect("scratch");
    root
}

/// KRV4-01-A1: 10k small files must round-trip, including the parent diff/recovery path.
#[test]
fn ten_thousand_file_workspace_captures_and_loads_back() {
    let root = scratch("roundtrip");
    let workspace_root = root.join("ws");
    for index in 0..10_000 {
        let dir = workspace_root.join(format!("src/module_{index:05}"));
        fs::create_dir_all(&dir).expect("dir");
        fs::write(dir.join("readme.txt"), "0123456789").expect("file");
    }
    let workspace = WorkspaceCap::new(&workspace_root).expect("workspace");
    let store = SnapshotStore::new(root.join("snapshots"));

    let captured = store
        .capture_revision(&workspace, None, None, 1)
        .expect("a 10 000-file workspace must capture");
    let loaded = store
        .load_revision(&captured.revision_id)
        .expect("a record the store accepted must load back");
    assert_eq!(
        loaded.tracked_files.len(),
        captured.tracked_files.len(),
        "every tracked file survives the round trip"
    );
    assert_eq!(loaded.revision_id, captured.revision_id);

    // A second capture against the first proves the parent link is readable too, which is what
    // diff, restore and recovery all depend on. One file changes first: the store is content
    // addressed, so an unchanged workspace would just return the same revision.
    fs::write(
        workspace_root.join("src/module_00000/readme.txt"),
        "changed",
    )
    .expect("edit");
    let second = store
        .capture_revision(&workspace, Some(&captured.revision_id), None, 2)
        .expect("a second capture");
    let loaded_second = store
        .load_revision(&second.revision_id)
        .expect("the second record loads");
    assert_eq!(
        loaded_second.parent_revision_id.as_ref(),
        Some(&captured.revision_id)
    );

    let _ = fs::remove_dir_all(&root);
}

/// KRV4-01-A2: when a record really is over the ceiling, the capture refuses - before the record
/// exists - instead of writing something the reader will reject later.
#[test]
fn an_oversized_record_is_refused_rather_than_written() {
    let root = scratch("ceiling");
    let workspace_root = root.join("ws");
    // Long paths are the cheapest way to push one manifest over the ceiling: the review's estimate
    // used ~19 characters per path, these use far more.
    for index in 0..40_000 {
        let dir = workspace_root.join(format!(
            "src/very/long/directory/name/that/exists/only/to/grow/the/manifest/module_{index:05}"
        ));
        fs::create_dir_all(&dir).expect("dir");
        fs::write(dir.join("readme.txt"), "x").expect("file");
    }
    let workspace = WorkspaceCap::new(&workspace_root).expect("workspace");
    let store = SnapshotStore::new(root.join("snapshots"));

    match store.capture_revision(&workspace, None, None, 1) {
        Err(kroma_core::revision::RevisionError::RecordTooLarge { bytes, limit }) => {
            assert!(bytes > limit, "{bytes} must exceed {limit}");
        }
        Ok(revision) => panic!(
            "a manifest over the ceiling must be refused, but {} was captured",
            revision.revision_id.as_str()
        ),
        Err(other) => panic!("expected RecordTooLarge, got {other}"),
    }

    // Nothing was left behind for a reader to choke on.
    assert!(
        store.newest_revision().expect("store readable").is_none(),
        "a refused capture must not leave a revision behind"
    );

    let _ = fs::remove_dir_all(&root);
}
