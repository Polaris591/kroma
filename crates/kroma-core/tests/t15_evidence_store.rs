//! T15 (KR-11): memory eviction is not deletion, and "gone" is not the same as "never existed".
//!
//! Four contracts, each of which the old single-level map could not honour:
//! 1. An evicted entry is still recoverable through its trusted artifact reference.
//! 2. A missing, corrupt or unreadable artifact reports which of those it is.
//! 3. A fork's sibling branch cannot resolve this branch's evidence.
//! 4. Evidence is the bytes that were captured, never the workspace file as it looks now.

use kroma_core::context_projector::{CompactionReason, ContextProjector, EvidenceLink};
use kroma_core::evidence_store::{ArtifactKey, ArtifactStatus, EvidenceStore, MAX_ARTIFACT_BYTES};
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;

fn scratch(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("kroma-t15-{}-{}", tag, uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    root
}

fn link(id: &str, index: usize, raw: &str) -> EvidenceLink {
    EvidenceLink {
        source_entry_id: id.to_string(),
        canonical_index: index,
        call_id: format!("call_{}", id),
        tool_name: "read".to_string(),
        relative_path: Some("src/a.rs".to_string()),
        original_sha256: kroma_core::prompt::sha256_hex64(raw.as_bytes()),
        original_bytes: raw.len(),
        original_lines: raw.lines().count(),
        stub_bytes: 128,
        reason: CompactionReason::StepDownArchive,
    }
}

/// The headline contract: after the in-memory cap forces an eviction, the bytes are still there.
#[test]
fn evicted_text_can_be_loaded_by_trusted_artifact_ref() {
    let root = scratch("evict");
    let store = Arc::new(EvidenceStore::new(root.join("evidence")));

    // Two entries of ~9 MiB each: together they exceed the 16 MiB in-memory cap, so the older one is
    // evicted (oldest-first by transcript position).
    let older = "a".repeat(9 * 1024 * 1024);
    let newer = "b".repeat(9 * 1024 * 1024);
    let mut projector =
        ContextProjector::new().with_evidence_store(Arc::clone(&store), "session-1", "leaf-1");
    projector.register_evidence(
        link("ev-0001-call_old", 1, &older),
        Arc::from(older.as_str()),
    );
    projector.register_evidence(
        link("ev-0002-call_new", 2, &newer),
        Arc::from(newer.as_str()),
    );

    // The older entry was evicted from memory...
    assert!(
        projector.evidence_status("ev-0001-call_old").is_some(),
        "the older entry must no longer be served from memory"
    );
    // ...but it is still available through the artifact.
    match projector.evidence_status("ev-0001-call_old") {
        Some(ArtifactStatus::Available { bytes, .. }) => {
            assert_eq!(bytes, older.len() as u64)
        }
        other => panic!("expected the evicted artifact to be Available, got {other:?}"),
    }
    let recovered = projector
        .evidence_lookup("ev-0001-call_old")
        .expect("an evicted entry must still resolve");
    assert_eq!(recovered.len(), older.len());
    assert!(recovered.starts_with("aaa"));

    // The newer entry never left memory, so it is a plain hit.
    assert!(projector.evidence_status("ev-0002-call_new").is_none());
    assert!(projector.evidence_lookup("ev-0002-call_new").is_some());

    let _ = std::fs::remove_dir_all(&root);
}

/// Missing, corrupt and unreadable artifacts are three different answers.
#[test]
fn expired_or_corrupt_artifact_returns_explicit_status() {
    let root = scratch("status");
    let store = EvidenceStore::new(root.join("evidence"));
    let raw = "line one\nline two\n";
    let key = ArtifactKey::new(
        "s",
        "b",
        "ev-0001-call_x",
        kroma_core::prompt::sha256_hex64(raw.as_bytes()),
    );

    // Never written.
    assert_eq!(store.get(&key), ArtifactStatus::Gone);

    // Written, then readable.
    store.put(&key, raw).expect("put");
    match store.get(&key) {
        ArtifactStatus::Available { text, bytes } => {
            assert_eq!(text, raw);
            assert_eq!(bytes, raw.len() as u64);
        }
        other => panic!("expected Available, got {other:?}"),
    }

    // Corrupted in place: the bytes no longer hash to the key.
    let path = store.root().join(key.relative_path());
    std::fs::write(&path, "tampered\n").unwrap();
    match store.get(&key) {
        ArtifactStatus::Corrupt { reason } => assert!(reason.contains("hashes to"), "{reason}"),
        other => panic!("expected Corrupt, got {other:?}"),
    }

    // Restored but the directory is no longer private: the bytes cannot be trusted.
    store.put(&key, raw).expect("put again");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            path.parent().unwrap(),
            std::fs::Permissions::from_mode(0o777),
        )
        .unwrap();
        match store.get(&key) {
            ArtifactStatus::Forbidden { reason } => {
                assert!(reason.contains("not private"), "{reason}")
            }
            other => panic!("expected Forbidden, got {other:?}"),
        }
        std::fs::set_permissions(
            path.parent().unwrap(),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
    }

    // Deleted by the user: an explicit Gone, not a silent miss.
    assert_eq!(store.remove(&key), ArtifactStatus::Gone);
    assert_eq!(store.get(&key), ArtifactStatus::Gone);

    // An oversized artifact is refused at write time rather than stored and truncated later.
    let big = "x".repeat((MAX_ARTIFACT_BYTES + 1) as usize);
    let big_key = ArtifactKey::new(
        "s",
        "b",
        "ev-0002-call_big",
        kroma_core::prompt::sha256_hex64(big.as_bytes()),
    );
    assert!(store.put(&big_key, &big).is_err());
    assert_eq!(store.get(&big_key), ArtifactStatus::Gone);

    let _ = std::fs::remove_dir_all(&root);
}

/// A fork's sibling branch has a different key, so it cannot read this branch's evidence.
#[test]
fn fork_does_not_cross_read_sibling_evidence() {
    let root = scratch("fork");
    let store = EvidenceStore::new(root.join("evidence"));
    let raw = "secret from the parent branch\n";
    let sha = kroma_core::prompt::sha256_hex64(raw.as_bytes());

    let parent = ArtifactKey::new("s", "leaf-parent", "ev-0001-call_x", &sha);
    store.put(&parent, raw).expect("put");

    // Same session, same event, same content — different branch.
    let sibling = ArtifactKey::new("s", "leaf-sibling", "ev-0001-call_x", &sha);
    assert_eq!(
        store.get(&sibling),
        ArtifactStatus::Gone,
        "a sibling branch must not resolve the parent branch's artifact"
    );
    assert!(store.get(&parent).is_available());

    // A different session is also isolated.
    let other_session = ArtifactKey::new("s2", "leaf-parent", "ev-0001-call_x", &sha);
    assert_eq!(store.get(&other_session), ArtifactStatus::Gone);

    let _ = std::fs::remove_dir_all(&root);
}

/// Evidence is the captured bytes; the workspace file having the same name is irrelevant.
#[test]
fn historical_hash_differs_from_current_file() {
    let root = scratch("historical");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let file = ws.join("a.rs");
    std::fs::write(&file, "fn old_version() {}\n").unwrap();
    let captured = "fn old_version() {}\n";
    let store = EvidenceStore::new(root.join("evidence"));
    let key = ArtifactKey::new(
        "s",
        "b",
        "ev-0001-call_x",
        kroma_core::prompt::sha256_hex64(captured.as_bytes()),
    );
    store.put(&key, captured).expect("put");

    // The file is edited afterwards.
    std::fs::write(&file, "fn new_version() {}\n").unwrap();
    let current = std::fs::read_to_string(&file).unwrap();
    let current_hash = kroma_core::prompt::sha256_hex64(current.as_bytes());
    assert_ne!(current_hash, key.sha256);

    // Reading the historical evidence returns the *old* bytes, not the file as it is now.
    match store.get(&key) {
        ArtifactStatus::Available { text, .. } => assert_eq!(text, captured),
        other => panic!("expected the historical bytes, got {other:?}"),
    }

    // A key built from the current content has no artifact: the store never falls back to the
    // workspace file, so the same name cannot impersonate old evidence.
    let current_key = ArtifactKey::new("s", "b", "ev-0001-call_x", &current_hash);
    assert_eq!(store.get(&current_key), ArtifactStatus::Gone);

    let _ = std::fs::remove_dir_all(&root);
}

/// GC deletes only what no retained session references, and reports what it could not delete.
#[test]
fn gc_keeps_referenced_artifacts_and_reports_failures() {
    let root = scratch("gc");
    let store = EvidenceStore::new(root.join("evidence"));
    let mut keys = Vec::new();
    for i in 0..4 {
        let raw = format!("evidence {i}\n");
        let key = ArtifactKey::new(
            "s",
            "b",
            format!("ev-000{i}-call_x"),
            kroma_core::prompt::sha256_hex64(raw.as_bytes()),
        );
        store.put(&key, &raw).expect("put");
        keys.push((key, raw));
    }

    // Only the first two are still referenced.
    let live: BTreeSet<ArtifactKey> = keys[..2].iter().map(|(k, _)| k.clone()).collect();
    let report = store.gc(&live);
    assert_eq!(report.kept, 2);
    assert_eq!(report.removed, 2);
    assert!(report.bytes_freed > 0);
    assert!(report.failed.is_empty(), "{:?}", report.failed);

    assert!(store.get(&keys[0].0).is_available());
    assert!(store.get(&keys[1].0).is_available());
    assert_eq!(store.get(&keys[2].0), ArtifactStatus::Gone);
    assert_eq!(store.get(&keys[3].0), ArtifactStatus::Gone);

    // GC is idempotent: a second pass finds nothing to remove and still reports honestly.
    let report = store.gc(&live);
    assert_eq!(report.removed, 0);
    assert_eq!(report.kept, 2);

    let _ = std::fs::remove_dir_all(&root);
}
