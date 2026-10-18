//! B10 / F12 — the multi-session daemon's session index is a durable transaction.
//!
//! The review found that the index used `create_dir_all + fs::write + rename` with no no-follow or
//! private-file boundary, no file or directory `fsync`, no read-size cap, and that it published a
//! new session into the live map *before* the index write, so a failed write could report success
//! while leaving a ghost session (and concurrent opens could all pass the capacity check and then
//! overshoot the bound). These tests pin the fixed behaviour:
//!
//! * a failed index write never reports success and never leaves a live session behind;
//! * concurrent opens reserve capacity, so the bound is never exceeded;
//! * the index is written privately (`0600`/`0700`), without following symlinks, and read bounded;
//! * the persisted generation round-trips and is advanced only by a committed write;
//! * a crash before the replace leaves the previous, consistent index recoverable.
//!
//! Every test uses its own temp root, so nothing is shared or globally perturbed.

use kroma_core::messages::{AssistantTurn, FinishReason};
use kroma_core::provider::FakeProvider;
use kroma_core::surface::{
    KromaRuntimeConfig, RpcDispatcher, SessionIndexEntry, SessionManager, SessionManagerConfig,
    SessionRuntimeFactory, JSONRPC_INTERNAL_ERROR, JSONRPC_SESSION_LIMIT, MAX_SESSION_INDEX_BYTES,
};
use kroma_core::tool::policy::PolicyMode;
use serde_json::{json, Value};
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn stop() -> AssistantTurn {
    AssistantTurn {
        content: Some("done".into()),
        reasoning_blocks: vec![],
        tool_calls: vec![],
        finish_reason: FinishReason::Stop,
        usage: None,
    }
}

fn temp_root(label: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "kroma-index-txn-{}-{}",
        label,
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&root).unwrap();
    root
}

fn index_path(base: &Path) -> PathBuf {
    base.join("state").join("daemon-sessions.json")
}

fn state_dir_for(base: &Path, workspace: &Path) -> PathBuf {
    let name = workspace
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "ws".to_string());
    base.join("state").join(name)
}

/// A factory that builds one runtime per workspace. When a [`Gate`] is attached, each `build`
/// announces that it entered and then blocks until the test releases it, so a test can observe
/// whether a second concurrent open reached the (slow) build step at all.
struct GatedFactory {
    base: PathBuf,
    gate: Option<Arc<Gate>>,
}

impl SessionRuntimeFactory for GatedFactory {
    fn build(
        &self,
        workspace_root: &Path,
        resume_session_path: Option<&Path>,
    ) -> Result<RpcDispatcher, String> {
        if let Some(gate) = &self.gate {
            gate.enter_and_wait();
        }
        let mut cfg = KromaRuntimeConfig::new(
            workspace_root,
            state_dir_for(&self.base, workspace_root),
            PolicyMode::AllowWrite,
        );
        cfg.resume_session_path = resume_session_path.map(Path::to_path_buf);
        RpcDispatcher::new(cfg, Arc::new(FakeProvider::new(vec![stop()])))
    }
}

struct Gate {
    entered_tx: std::sync::mpsc::Sender<()>,
    permits: Mutex<std::sync::mpsc::Receiver<()>>,
    entered_count: AtomicUsize,
}

impl Gate {
    fn enter_and_wait(&self) {
        self.entered_count.fetch_add(1, Ordering::SeqCst);
        let _ = self.entered_tx.send(());
        let permits = self.permits.lock().unwrap();
        let _ = permits.recv();
    }
}

fn plain_factory(base: &Path) -> Arc<GatedFactory> {
    Arc::new(GatedFactory {
        base: base.to_path_buf(),
        gate: None,
    })
}

/// A factory whose first `build` panics (to stand in for a build that dies mid-flight) and whose
/// later builds succeed. Used to prove a panicking build cannot leak a capacity reservation.
struct PanicOnceFactory {
    base: PathBuf,
    panicked: AtomicBool,
}

impl SessionRuntimeFactory for PanicOnceFactory {
    fn build(
        &self,
        workspace_root: &Path,
        resume_session_path: Option<&Path>,
    ) -> Result<RpcDispatcher, String> {
        if !self.panicked.swap(true, Ordering::SeqCst) {
            panic!("injected build panic");
        }
        let mut cfg = KromaRuntimeConfig::new(
            workspace_root,
            state_dir_for(&self.base, workspace_root),
            PolicyMode::AllowWrite,
        );
        cfg.resume_session_path = resume_session_path.map(Path::to_path_buf);
        RpcDispatcher::new(cfg, Arc::new(FakeProvider::new(vec![stop()])))
    }
}

fn manager_config(
    factory: Arc<GatedFactory>,
    base: &Path,
    max_sessions: usize,
    default_workspace: &Path,
) -> SessionManagerConfig {
    SessionManagerConfig {
        factory,
        max_sessions,
        index_path: index_path(base),
        default_workspace: default_workspace.to_path_buf(),
    }
}

fn open_manager(
    factory: &Arc<GatedFactory>,
    base: &Path,
    max_sessions: usize,
    workspace: &Path,
) -> SessionManager {
    let primary = factory.build(workspace, None).unwrap();
    SessionManager::open(
        manager_config(factory.clone(), base, max_sessions, workspace),
        primary,
    )
    .unwrap()
}

// ---------------------------------------------------------------------------
// F12: a failed index write must not report success or leave a ghost session
// ---------------------------------------------------------------------------

#[tokio::test]
async fn failed_index_write_reports_error_and_leaves_no_ghost_session() {
    let root = temp_root("ghost");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let factory = plain_factory(&root);
    let manager = open_manager(&factory, &root, 4, &ws);

    let before_ids = manager.session_ids();
    let before_len = manager.len();

    // Make the atomic replace fail deterministically and root-proof: `rename(tmp, target)` fails
    // with `EISDIR` when the target is a directory, no matter the process's privileges.
    let idx = index_path(&root);
    std::fs::remove_file(&idx).unwrap();
    std::fs::create_dir(&idx).unwrap();

    let failed = manager
        .open_session(Some(&ws), None)
        .expect_err("a failed index write must not be reported as success");
    assert_eq!(
        failed.code, JSONRPC_INTERNAL_ERROR,
        "the failure must be a structured internal error: {}",
        failed.message
    );
    assert_eq!(
        manager.session_ids(),
        before_ids,
        "a failed open must not leave a live session behind"
    );
    assert_eq!(
        manager.len(),
        before_len,
        "the live runtime count is unchanged"
    );

    // The daemon is still usable: once the obstruction is gone the same open succeeds.
    std::fs::remove_dir(&idx).unwrap();
    let entry = manager
        .open_session(Some(&ws), None)
        .expect("a later open must succeed");
    assert!(
        manager.session_ids().contains(&entry.session_id),
        "the successful open is published"
    );

    let _ = std::fs::remove_dir_all(root);
}

// ---------------------------------------------------------------------------
// F12: concurrent opens reserve capacity instead of racing past the bound
// ---------------------------------------------------------------------------

#[tokio::test]
async fn concurrent_opens_never_exceed_the_capacity_bound() {
    let root = temp_root("capacity");
    let ws0 = root.join("ws-0");
    let ws1 = root.join("ws-1");
    let ws2 = root.join("ws-2");
    for ws in [&ws0, &ws1, &ws2] {
        std::fs::create_dir_all(ws).unwrap();
    }

    let (entered_tx, entered_rx) = std::sync::mpsc::channel::<()>();
    let (permit_tx, permit_rx) = std::sync::mpsc::channel::<()>();
    let gate = Arc::new(Gate {
        entered_tx,
        permits: Mutex::new(permit_rx),
        entered_count: AtomicUsize::new(0),
    });
    let factory = Arc::new(GatedFactory {
        base: root.clone(),
        gate: Some(gate.clone()),
    });

    // The primary fills one of the two slots; exactly one more session may open.
    let primary = plain_factory(&root).build(&ws0, None).unwrap();
    let manager = SessionManager::open(manager_config(factory, &root, 2, &ws0), primary).unwrap();

    let first = manager.clone();
    let first_ws = ws1.clone();
    let first_open = std::thread::spawn(move || first.open_session(Some(&first_ws), None));
    entered_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("the first open must reach its build step");

    let second = manager.clone();
    let second_ws = ws2.clone();
    let second_open = std::thread::spawn(move || second.open_session(Some(&second_ws), None));

    // With the reservation fix the second open is refused before it builds, so no second `enter`
    // arrives; without it the second open passes the (lock-scoped) capacity check and blocks in
    // `build`. Either way, release the gate and join.
    let second_entered = entered_rx.recv_timeout(Duration::from_millis(750)).is_ok();
    let _ = permit_tx.send(());
    let _ = permit_tx.send(());

    let first_result = first_open.join().unwrap();
    let second_result = second_open.join().unwrap();

    assert!(
        first_result.is_ok(),
        "the reserved slot must be taken by the first open"
    );
    assert!(
        second_result.is_err(),
        "the second open must be refused at the bound"
    );
    if let Err(err) = &second_result {
        assert_eq!(err.code, JSONRPC_SESSION_LIMIT);
    }
    assert!(
        !second_entered,
        "a refused open must not spend a runtime build"
    );
    assert_eq!(
        manager.len(),
        2,
        "the daemon must never hold more sessions than its bound"
    );

    let _ = std::fs::remove_dir_all(root);
}

// ---------------------------------------------------------------------------
// F12: no-follow, bounded read, private file
// ---------------------------------------------------------------------------

#[tokio::test]
async fn oversized_index_is_refused_instead_of_read_into_memory() {
    let root = temp_root("oversized");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let idx = index_path(&root);
    std::fs::create_dir_all(idx.parent().unwrap()).unwrap();

    // A syntactically valid *legacy* (bare-array) document that is still far over the cap: the old
    // code read and parsed it happily; the fixed code refuses it before reading the body.
    let filler = "w".repeat(400);
    let entries: Vec<Value> = (0..4000)
        .map(|i| {
            json!({
                "session_id": format!("session-{i:06}"),
                "workspace_root": filler,
                "session_path": filler,
            })
        })
        .collect();
    let bytes = serde_json::to_vec(&entries).unwrap();
    assert!(
        bytes.len() as u64 > MAX_SESSION_INDEX_BYTES,
        "the fixture must actually exceed the cap"
    );
    std::fs::write(&idx, &bytes).unwrap();

    let factory = plain_factory(&root);
    let primary = factory.build(&ws, None).unwrap();
    let result = SessionManager::open(manager_config(factory, &root, 4, &ws), primary);
    assert!(
        result.is_err(),
        "an oversized index must be refused, not loaded"
    );

    let _ = std::fs::remove_dir_all(root);
}

#[cfg(unix)]
#[tokio::test]
async fn symlinked_index_is_refused_and_its_target_is_untouched() {
    let root = temp_root("symlink");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let idx = index_path(&root);
    std::fs::create_dir_all(idx.parent().unwrap()).unwrap();

    // A valid (empty) legacy index behind a symlink: the old `fs::read` followed it and started the
    // daemon; the no-follow read must refuse it and leave the target alone.
    let victim = root.join("victim.json");
    std::fs::write(&victim, b"[]").unwrap();
    std::os::unix::fs::symlink(&victim, &idx).unwrap();

    let factory = plain_factory(&root);
    let primary = factory.build(&ws, None).unwrap();
    let result = SessionManager::open(manager_config(factory, &root, 4, &ws), primary);
    assert!(
        result.is_err(),
        "a symlinked index must be refused, never followed"
    );
    assert_eq!(
        std::fs::read(&victim).unwrap(),
        b"[]",
        "the symlink target must not be read or written through"
    );

    let _ = std::fs::remove_dir_all(root);
}

#[cfg(unix)]
#[tokio::test]
async fn index_and_its_directory_are_private_with_no_temp_leftovers() {
    use std::os::unix::fs::PermissionsExt;

    let root = temp_root("private");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let factory = plain_factory(&root);
    let manager = open_manager(&factory, &root, 4, &ws);
    manager.open_session(Some(&ws), None).unwrap();

    let idx = index_path(&root);
    let file_mode = std::fs::metadata(&idx).unwrap().permissions().mode() & 0o777;
    assert_eq!(
        file_mode, 0o600,
        "the index must be a private file, got {file_mode:o}"
    );
    let dir_mode = std::fs::metadata(idx.parent().unwrap())
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(
        dir_mode, 0o700,
        "the index directory must be private, got {dir_mode:o}"
    );

    let leftovers: Vec<String> = std::fs::read_dir(idx.parent().unwrap())
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|name| name.contains(".tmp-"))
        .collect();
    assert!(
        leftovers.is_empty(),
        "a committed write must not leave temp files: {leftovers:?}"
    );

    let _ = std::fs::remove_dir_all(root);
}

// ---------------------------------------------------------------------------
// F12: generation round-trips, and recovery is consistent across a restart
// ---------------------------------------------------------------------------

#[tokio::test]
async fn generation_is_advanced_only_by_a_committed_write() {
    let root = temp_root("generation");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let factory = plain_factory(&root);
    let manager = open_manager(&factory, &root, 4, &ws);
    let entry_b: SessionIndexEntry = manager.open_session(Some(&ws), None).unwrap();
    let idx = index_path(&root);

    let first: Value = serde_json::from_slice(&std::fs::read(&idx).unwrap()).unwrap();
    let generation_first = first["generation"]
        .as_u64()
        .expect("a generation is persisted");
    assert!(generation_first >= 1);
    assert!(first["entries"].is_array());
    drop(manager);

    // A restart recovers the persisted sessions and writes the next generation.
    let manager2 = open_manager(&factory, &root, 4, &ws);
    assert!(
        manager2.session_ids().contains(&entry_b.session_id),
        "session B must be recovered after a restart"
    );
    let second: Value = serde_json::from_slice(&std::fs::read(&idx).unwrap()).unwrap();
    let generation_second = second["generation"].as_u64().unwrap();
    assert!(
        generation_second > generation_first,
        "a committed restart write advances the generation ({generation_first} -> {generation_second})"
    );

    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn a_leftover_temp_file_does_not_corrupt_recovery() {
    let root = temp_root("crash-before-replace");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let factory = plain_factory(&root);
    let manager = open_manager(&factory, &root, 4, &ws);
    let entry_b: SessionIndexEntry = manager.open_session(Some(&ws), None).unwrap();
    let idx = index_path(&root);
    drop(manager);

    // A crash between the temp write and the rename leaves the temp file and the old index.
    let stale = idx
        .parent()
        .unwrap()
        .join(".daemon-sessions.json.tmp-stale");
    std::fs::write(&stale, b"{ this is not a valid index").unwrap();

    let manager2 = open_manager(&factory, &root, 4, &ws);
    assert!(
        manager2.session_ids().contains(&entry_b.session_id),
        "the committed index must survive a crash before the replace"
    );
    let doc: Value = serde_json::from_slice(&std::fs::read(&idx).unwrap()).unwrap();
    assert!(
        doc["entries"].is_array(),
        "the stale temp file's garbage must never become the index"
    );

    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn legacy_bare_array_index_is_still_accepted_and_upgraded() {
    let root = temp_root("legacy");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let idx = index_path(&root);
    std::fs::create_dir_all(idx.parent().unwrap()).unwrap();
    // The pre-generation format was a bare array.
    std::fs::write(&idx, b"[]").unwrap();

    let factory = plain_factory(&root);
    let _manager = open_manager(&factory, &root, 4, &ws);

    let doc: Value = serde_json::from_slice(&std::fs::read(&idx).unwrap()).unwrap();
    assert!(
        doc["generation"].as_u64().unwrap() >= 1,
        "a legacy index is upgraded to the generation document: {doc}"
    );
    assert!(doc["entries"].is_array());

    let _ = std::fs::remove_dir_all(root);
}

// ---------------------------------------------------------------------------
// F2: the capacity reservation is released even when a build panics
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_panicking_build_releases_its_capacity_reservation() {
    let root = temp_root("build-panic");
    let ws0 = root.join("ws-0");
    let ws1 = root.join("ws-1");
    let ws2 = root.join("ws-2");
    let ws3 = root.join("ws-3");
    for ws in [&ws0, &ws1, &ws2, &ws3] {
        std::fs::create_dir_all(ws).unwrap();
    }

    let primary = plain_factory(&root).build(&ws0, None).unwrap();
    let factory = Arc::new(PanicOnceFactory {
        base: root.clone(),
        panicked: AtomicBool::new(false),
    });
    let manager = SessionManager::open(
        SessionManagerConfig {
            factory,
            max_sessions: 3,
            index_path: index_path(&root),
            default_workspace: ws0.clone(),
        },
        primary,
    )
    .unwrap();

    // The first build panics; the reservation must be released during unwinding.
    let caught =
        std::panic::catch_unwind(AssertUnwindSafe(|| manager.open_session(Some(&ws1), None)));
    assert!(caught.is_err(), "the injected build panic must propagate");
    assert_eq!(
        manager.len(),
        1,
        "a panicking build must publish no session"
    );

    // Both remaining slots are still free: the leaked reservation would have refused these.
    manager
        .open_session(Some(&ws1), None)
        .expect("the released slot must be reusable");
    manager
        .open_session(Some(&ws2), None)
        .expect("the second free slot must be usable");
    assert_eq!(manager.len(), 3, "the bound still counts live sessions");

    // Other sessions were not wrongly decremented: the bound is still exactly enforced.
    let refused = manager
        .open_session(Some(&ws3), None)
        .expect_err("the bound must still be enforced");
    assert_eq!(refused.code, JSONRPC_SESSION_LIMIT);

    let _ = std::fs::remove_dir_all(root);
}
