//! F03 / F10 (`v1.5.1`) session state-transaction contract, plus runtime session ownership.
//!
//! * F03 — `session.fork` is workspace-bound by default: it restores the workspace to the revision
//!   that was current at the forked turn and only then moves the session leaf. External edits make
//!   the fork fail closed instead of being silently overwritten, and an interrupted restore makes
//!   it fail with a recovery-required error.
//! * F10 — a WAL reconciliation failure or an unrecovered restore blocks new mutations; read-only
//!   status degrades with a reason instead of proceeding.
//! * Session ownership — one writer per workspace across runtimes, sessions routed by id, and
//!   different workspaces running independently.
//!
//! These are the fixed versions of the review reproductions (which asserted the buggy behaviour).

use kroma_core::budget::BudgetConfig;
use kroma_core::messages::{AssistantTurn, FinishReason, ToolCall};
use kroma_core::provider::{
    FakeProvider, ModelProvider, ProviderCompletion, ProviderError, ProviderRequest,
};
use kroma_core::revision::{
    ForkFault, ForkRecoveryOutcome, RestoreFault, RevisionId, SnapshotStore,
};
use kroma_core::session::SessionJournal;
use kroma_core::surface::{KromaRuntimeConfig, RpcDispatcher};
use kroma_core::tool::policy::PolicyMode;
use kroma_core::types::ToolCallId;
use kroma_core::workspace::WorkspaceCap;
use serde_json::{json, Value};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;

const JSONRPC_RESTORE_CONFLICT: i64 = -32003;
const JSONRPC_RECOVERY_REQUIRED: i64 = -32004;
const JSONRPC_WORKSPACE_BUSY: i64 = -32005;
const JSONRPC_SESSION_NOT_OWNED: i64 = -32006;

fn stop() -> AssistantTurn {
    AssistantTurn {
        content: Some("done".into()),
        reasoning_blocks: vec![],
        tool_calls: vec![],
        finish_reason: FinishReason::Stop,
        usage: None,
    }
}

fn write_tool(path: &str, content: &str) -> AssistantTurn {
    AssistantTurn {
        content: None,
        reasoning_blocks: vec![],
        tool_calls: vec![ToolCall {
            call_id: ToolCallId::new("one"),
            name: "write".into(),
            raw_arguments: json!({ "path": path, "content": content }).to_string(),
        }],
        finish_reason: FinishReason::ToolCalls,
        usage: None,
    }
}

fn temp_root(label: &str) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!(
        "kroma-state-txn-{}-{}",
        label,
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&root).unwrap();
    root
}

async fn rpc(disp: &RpcDispatcher, method: &str, params: Value) -> Value {
    let req = serde_json::from_value(json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": method,
        "params": params,
    }))
    .unwrap();
    serde_json::to_value(disp.handle_request(req).await).unwrap()
}

fn err_code(resp: &Value) -> Option<i64> {
    resp.get("error")
        .and_then(|e| e.get("code"))
        .and_then(Value::as_i64)
}

/// A provider that only returns after `delay`, so a run holds its workspace writer lease long
/// enough for a second runtime to contend for the same workspace.
struct SlowProvider {
    delay: Duration,
}

impl ModelProvider for SlowProvider {
    fn complete<'a>(
        &'a self,
        _req: ProviderRequest,
        _budget: &'a BudgetConfig,
        _rx: watch::Receiver<bool>,
    ) -> Pin<Box<dyn Future<Output = Result<ProviderCompletion, ProviderError>> + Send + 'a>> {
        let delay = self.delay;
        Box::pin(async move {
            tokio::time::sleep(delay).await;
            Ok(ProviderCompletion {
                turn: stop(),
                request_body_bytes: 0,
                latency_ms: delay.as_millis() as u64,
                ttft_ms: None,
                attempts_used: 1,
                reported_model: None,
            })
        })
    }
}

/// Run one prompt that rewrites `a.txt` from `before` to `after`, returning the dispatcher, the
/// workspace path and the `after` revision id.
async fn dispatcher_with_edit(
    root: &std::path::Path,
) -> (RpcDispatcher, std::path::PathBuf, String) {
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join("a.txt"), "before").unwrap();
    let cfg = KromaRuntimeConfig::new(&ws, root.join("state"), PolicyMode::AllowWrite);
    let disp = RpcDispatcher::new(
        cfg,
        Arc::new(FakeProvider::new(vec![
            write_tool("a.txt", "after"),
            stop(),
        ])),
    )
    .unwrap();
    let resp = rpc(&disp, "session.prompt", json!({ "prompt": "change a.txt" })).await;
    assert!(resp.get("error").is_none(), "prompt failed: {}", resp);
    let after_rev = resp["result"]["latest_revision_id"]
        .as_str()
        .expect("prompt response carries the captured revision")
        .to_string();
    (disp, ws, after_rev)
}

fn store_for(root: &std::path::Path) -> SnapshotStore {
    SnapshotStore::new(root.join("state").join("snapshots"))
}

// ---------------------------------------------------------------------------
// F03: workspace-bound fork
// ---------------------------------------------------------------------------

#[tokio::test]
async fn fork_at_turn_zero_restores_workspace_bytes_and_wal() {
    let root = temp_root("fork-restore");
    let (disp, ws, after_rev) = dispatcher_with_edit(&root).await;
    assert_eq!(std::fs::read_to_string(ws.join("a.txt")).unwrap(), "after");

    let fork = rpc(&disp, "session.fork", json!({ "turn": 0 })).await;
    assert!(fork.get("error").is_none(), "fork failed: {}", fork);
    assert_eq!(fork["result"]["mode"], "workspace");
    assert_eq!(fork["result"]["restored_workspace"], true);
    assert_eq!(
        std::fs::read_to_string(ws.join("a.txt")).unwrap(),
        "before",
        "workspace-bound fork must restore the bytes current at turn 0"
    );

    // The WAL's current revision now names the restored bytes, not the abandoned `after` state.
    let journal = SessionJournal::open(disp.session_path()).unwrap();
    let latest = journal.latest_revision_id().expect("restored revision");
    assert_eq!(
        fork["result"]["latest_revision_id"].as_str(),
        Some(latest.as_str())
    );
    let manifest = store_for(&root)
        .load_revision(&RevisionId::new(latest.clone()))
        .unwrap();
    assert_eq!(
        manifest.tracked_files["a.txt"].sha256,
        kroma_core::prompt::sha256_hex64(b"before"),
        "the current revision must hash the restored content"
    );
    // The abandoned branch's revision is retained in the store as its backup.
    assert!(
        store_for(&root)
            .load_revision(&RevisionId::new(after_rev))
            .is_ok(),
        "the pre-fork revision must survive the fork"
    );

    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn workspace_bound_fork_refuses_to_overwrite_an_external_edit() {
    let root = temp_root("fork-conflict");
    let (disp, ws, _after_rev) = dispatcher_with_edit(&root).await;
    let before = rpc(&disp, "session.status", json!({})).await;
    let turns_before = before["result"]["turn_count"].as_u64().unwrap();
    // The user edits the file outside Kroma after the run.
    std::fs::write(ws.join("a.txt"), "external").unwrap();

    let fork = rpc(&disp, "session.fork", json!({ "turn": 0 })).await;
    assert_eq!(
        err_code(&fork),
        Some(JSONRPC_RESTORE_CONFLICT),
        "fork must refuse an external modification: {}",
        fork
    );
    assert!(
        fork["error"]["data"]["conflicts"]
            .as_array()
            .map(|c| c.iter().any(|e| e["path"] == "a.txt"))
            .unwrap_or(false),
        "conflict must name the externally modified path: {}",
        fork
    );
    assert_eq!(
        std::fs::read_to_string(ws.join("a.txt")).unwrap(),
        "external",
        "a refused fork must not touch the workspace"
    );
    // The session leaf did not move.
    let after = rpc(&disp, "session.status", json!({})).await;
    assert_eq!(
        after["result"]["turn_count"].as_u64().unwrap(),
        turns_before,
        "a refused fork must not move the leaf"
    );

    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn conversation_only_fork_leaves_the_workspace_untouched() {
    let root = temp_root("fork-conversation");
    let (disp, ws, _after_rev) = dispatcher_with_edit(&root).await;

    let fork = rpc(
        &disp,
        "session.fork",
        json!({ "turn": 0, "mode": "conversation" }),
    )
    .await;
    assert!(fork.get("error").is_none(), "fork failed: {}", fork);
    assert_eq!(fork["result"]["mode"], "conversation");
    assert_eq!(fork["result"]["restored_workspace"], false);
    assert_eq!(
        std::fs::read_to_string(ws.join("a.txt")).unwrap(),
        "after",
        "conversation-only fork must not restore files"
    );
    let journal = SessionJournal::open(disp.session_path()).unwrap();
    assert_eq!(journal.max_active_turn(), 0);

    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn force_fork_overwrites_an_external_edit_explicitly() {
    let root = temp_root("fork-force");
    let (disp, ws, _after_rev) = dispatcher_with_edit(&root).await;
    std::fs::write(ws.join("a.txt"), "external").unwrap();

    let fork = rpc(&disp, "session.fork", json!({ "turn": 0, "force": true })).await;
    assert!(fork.get("error").is_none(), "forced fork failed: {}", fork);
    assert_eq!(std::fs::read_to_string(ws.join("a.txt")).unwrap(), "before");

    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn fork_at_a_turn_without_a_revision_fails_without_touching_the_workspace() {
    let root = temp_root("fork-no-revision");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    // A session that was created but never prompted has no revision history at all.
    let cfg = KromaRuntimeConfig::new(&ws, root.join("state"), PolicyMode::AllowWrite);
    let disp = RpcDispatcher::new(cfg, Arc::new(FakeProvider::new(vec![]))).unwrap();

    let fork = rpc(&disp, "session.fork", json!({ "turn": 0 })).await;
    assert_eq!(
        err_code(&fork),
        Some(-32602),
        "a fork with no revision must fail with an invalid-params error: {}",
        fork
    );

    let _ = std::fs::remove_dir_all(root);
}

// ---------------------------------------------------------------------------
// F03 follow-up (P1-2): durable fork intent closes the restore→leaf-move crash window
// ---------------------------------------------------------------------------

#[tokio::test]
async fn interrupted_fork_is_completed_from_its_durable_intent() {
    let root = temp_root("fork-intent");
    let (disp, ws, after_rev) = dispatcher_with_edit(&root).await;
    let store = store_for(&root);
    let workspace = WorkspaceCap::new(&ws).unwrap();
    let mut journal = SessionJournal::open(disp.session_path()).unwrap();
    let records = journal.revision_records();
    assert_eq!(records.len(), 2, "baseline plus the edited revision");
    let baseline = records[0].revision_id.clone();
    let expected_leaf = journal.entry_id_at_or_before_turn(0).unwrap();
    assert_eq!(
        journal.latest_revision_id().as_deref(),
        Some(after_rev.as_str())
    );

    // Crash exactly between the restore (phase 1) and the leaf move (phase 2).
    let crashed = store.fork_workspace_at_turn_with_fault(
        &workspace,
        &mut journal,
        0,
        false,
        Some(ForkFault::AfterRestoreBeforeLeafMove),
    );
    assert!(crashed.is_err(), "the injected crash aborts phase 2");
    assert_eq!(
        std::fs::read_to_string(ws.join("a.txt")).unwrap(),
        "before",
        "phase 1 restored the file"
    );
    assert_eq!(
        journal.latest_revision_id().as_deref(),
        Some(after_rev.as_str()),
        "phase 2 has not run yet"
    );
    assert!(
        store.pending_fork_intent().unwrap().is_some(),
        "the crash must leave a durable fork intent"
    );
    drop(journal);

    // The next entry point (the exact call the dispatcher makes) finishes phase 2 idempotently.
    let mut journal = SessionJournal::open(disp.session_path()).unwrap();
    match store
        .recover_pending_fork(&workspace, &mut journal)
        .unwrap()
    {
        ForkRecoveryOutcome::Completed(outcome) => {
            assert_eq!(outcome.forked_at_turn, 0);
            assert_eq!(outcome.target_revision.as_str(), baseline.as_str());
        }
        other => panic!("expected the fork to be completed, got {:?}", other),
    }
    assert_eq!(
        journal.leaf_id(),
        Some(expected_leaf.as_str()),
        "the leaf moves to the forked turn"
    );
    assert_eq!(
        journal.latest_revision_id().as_deref(),
        Some(baseline.as_str()),
        "the restored revision is re-bound in the WAL"
    );
    assert_eq!(std::fs::read_to_string(ws.join("a.txt")).unwrap(), "before");
    assert!(
        store.pending_fork_intent().unwrap().is_none(),
        "the intent is cleared after completion"
    );

    // Recovery is idempotent.
    assert_eq!(
        store
            .recover_pending_fork(&workspace, &mut journal)
            .unwrap(),
        ForkRecoveryOutcome::NothingPending
    );

    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn resumed_runtime_completes_an_interrupted_fork() {
    let root = temp_root("fork-intent-resume");
    let (disp, ws, after_rev) = dispatcher_with_edit(&root).await;
    let store = store_for(&root);
    let workspace = WorkspaceCap::new(&ws).unwrap();
    let mut journal = SessionJournal::open(disp.session_path()).unwrap();
    let baseline = journal.revision_records()[0].revision_id.clone();
    let session_path = disp.session_path();
    let _ = store.fork_workspace_at_turn_with_fault(
        &workspace,
        &mut journal,
        0,
        false,
        Some(ForkFault::AfterRestoreBeforeLeafMove),
    );
    assert_eq!(
        journal.latest_revision_id().as_deref(),
        Some(after_rev.as_str()),
        "the crash leaves the leaf on the pre-fork branch"
    );
    drop(journal);
    drop(disp);

    // Resuming the session runs the production recovery path during construction.
    let mut cfg = KromaRuntimeConfig::new(&ws, root.join("state"), PolicyMode::AllowWrite);
    cfg.resume_session_path = Some(session_path);
    let resumed = RpcDispatcher::new(cfg, Arc::new(FakeProvider::new(vec![stop()]))).unwrap();

    let journal = SessionJournal::open(resumed.session_path()).unwrap();
    assert_eq!(
        journal.latest_revision_id().as_deref(),
        Some(baseline.as_str()),
        "resumed construction completes the interrupted fork's phase 2"
    );
    assert_eq!(std::fs::read_to_string(ws.join("a.txt")).unwrap(), "before");
    assert!(store.pending_fork_intent().unwrap().is_none());

    let _ = std::fs::remove_dir_all(root);
}

// ---------------------------------------------------------------------------
// F10: reconcile failures and interrupted restores gate mutations
// ---------------------------------------------------------------------------

#[tokio::test]
async fn corrupt_manifest_blocks_mutations_and_degrades_status() {
    let root = temp_root("corrupt-manifest");
    let (disp, _ws, _after_rev) = dispatcher_with_edit(&root).await;

    let revisions_dir = root.join("state").join("snapshots").join("revisions");
    std::fs::create_dir_all(&revisions_dir).unwrap();
    std::fs::write(
        revisions_dir.join("r-00000000000000000000000000000000.json"),
        b"this is not a revision manifest",
    )
    .unwrap();

    let prompt = rpc(&disp, "session.prompt", json!({ "prompt": "keep going" })).await;
    assert_eq!(
        err_code(&prompt),
        Some(JSONRPC_RECOVERY_REQUIRED),
        "a corrupt manifest must block a new mutation: {}",
        prompt
    );

    let status = rpc(&disp, "session.status", json!({})).await;
    assert_eq!(
        status["result"]["recovery"]["degraded"], true,
        "status must report the degraded recovery state: {}",
        status
    );
    assert!(
        status["result"]["recovery"]["reason"]
            .as_str()
            .unwrap()
            .contains("unreadable"),
        "status must explain why recovery is degraded: {}",
        status
    );

    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn interrupted_restore_blocks_new_mutation_until_recovered() {
    let root = temp_root("interrupted-restore");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join("a.txt"), "before").unwrap();
    let cfg = KromaRuntimeConfig::new(&ws, root.join("state"), PolicyMode::AllowWrite);
    let disp = RpcDispatcher::new(
        cfg,
        Arc::new(FakeProvider::new(vec![
            write_tool("a.txt", "after"),
            stop(),
            stop(),
        ])),
    )
    .unwrap();
    let resp = rpc(&disp, "session.prompt", json!({ "prompt": "change" })).await;
    assert!(resp.get("error").is_none(), "prompt failed: {}", resp);

    // Start a restore back to the baseline and crash immediately after the intent is journaled.
    let store = store_for(&root);
    let workspace = WorkspaceCap::new(&ws).unwrap();
    let journal = SessionJournal::open(disp.session_path()).unwrap();
    let records = journal.revision_records();
    let baseline = RevisionId::new(records[0].revision_id.clone());
    let current = RevisionId::new(records.last().unwrap().revision_id.clone());
    let outcome = store
        .restore_revision_with_fault(
            &workspace,
            &baseline,
            Some(&current),
            false,
            Some(RestoreFault::AfterIntentWritten),
        )
        .unwrap();
    assert!(matches!(
        outcome,
        kroma_core::revision::RestoreResult::RecoveryRequired { .. }
    ));
    assert!(store.pending_restore().unwrap().is_some());
    drop(journal);

    // A new mutation is refused while the workspace may be half-applied.
    let blocked = rpc(&disp, "session.prompt", json!({ "prompt": "run anyway" })).await;
    assert_eq!(
        err_code(&blocked),
        Some(JSONRPC_RECOVERY_REQUIRED),
        "an interrupted restore must block a new mutation: {}",
        blocked
    );
    let blocked_fork = rpc(&disp, "session.fork", json!({ "turn": 0 })).await;
    assert_eq!(err_code(&blocked_fork), Some(JSONRPC_RECOVERY_REQUIRED));

    // Recovering finishes the journaled restore; the run then proceeds again.
    let recovered = store.recover_pending_restore(&workspace).unwrap();
    assert!(matches!(
        recovered,
        kroma_core::revision::RestoreResult::Applied { .. }
    ));
    assert_eq!(std::fs::read_to_string(ws.join("a.txt")).unwrap(), "before");

    let _ = std::fs::remove_dir_all(root);
}

// ---------------------------------------------------------------------------
// Runtime session ownership and workspace writer routing
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_second_runtime_cannot_write_the_same_workspace() {
    let root = temp_root("writer-exclusive");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();

    let cfg_a = KromaRuntimeConfig::new(&ws, root.join("state-a"), PolicyMode::AllowWrite);
    let disp_a = RpcDispatcher::new(
        cfg_a,
        Arc::new(SlowProvider {
            delay: Duration::from_millis(400),
        }),
    )
    .unwrap();
    let cfg_b = KromaRuntimeConfig::new(&ws, root.join("state-b"), PolicyMode::AllowWrite);
    let disp_b = RpcDispatcher::new(cfg_b, Arc::new(FakeProvider::new(vec![stop()]))).unwrap();

    let held = disp_a.clone();
    let run = tokio::spawn(async move {
        rpc(
            &held,
            "session.prompt",
            json!({ "prompt": "hold the workspace" }),
        )
        .await
    });
    tokio::time::sleep(Duration::from_millis(100)).await;

    let contended = rpc(&disp_b, "session.prompt", json!({ "prompt": "also write" })).await;
    assert_eq!(
        err_code(&contended),
        Some(JSONRPC_WORKSPACE_BUSY),
        "a second runtime must not write the same workspace: {}",
        contended
    );

    let first = run.await.unwrap();
    assert!(
        first.get("error").is_none(),
        "the first runtime must still finish: {}",
        first
    );

    // The lease is released when the run returns, so the second runtime can now proceed.
    let retry = rpc(
        &disp_b,
        "session.prompt",
        json!({ "prompt": "now it is free" }),
    )
    .await;
    assert!(
        retry.get("error").is_none(),
        "lease must be released: {}",
        retry
    );

    let _ = std::fs::remove_dir_all(root);
}

/// P1-4: the writer lease is enforced across *processes*, not just runtimes in one process. A
/// held advisory lock on the workspace lock file (what another `kroma serve` process owns) makes a
/// prompt fail with the same workspace-busy error and names the competing holder.
#[cfg(unix)]
#[tokio::test]
async fn a_competing_process_holding_the_lock_blocks_a_run() {
    use std::io::Write;
    use std::os::unix::io::AsRawFd;

    let root = temp_root("writer-xproc");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let cfg = KromaRuntimeConfig::new(&ws, root.join("state"), PolicyMode::AllowWrite);
    let disp = RpcDispatcher::new(cfg, Arc::new(FakeProvider::new(vec![stop()]))).unwrap();

    // Stand in for a second process by holding the shared advisory lock on the workspace.
    let lock_path = kroma_core::session_ownership::workspace_lock_path(&ws);
    std::fs::create_dir_all(lock_path.parent().unwrap()).unwrap();
    let mut competitor = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .unwrap();
    // SAFETY: `competitor` owns a valid open descriptor for the duration of the call.
    let rc = unsafe { libc::flock(competitor.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    assert_eq!(rc, 0, "the simulated other process must take the lock");
    writeln!(competitor, "serve-pid-999\t999").unwrap();

    let blocked = rpc(&disp, "session.prompt", json!({ "prompt": "write anyway" })).await;
    assert_eq!(
        err_code(&blocked),
        Some(JSONRPC_WORKSPACE_BUSY),
        "a competing process must block the run: {}",
        blocked
    );
    assert!(
        blocked["error"]["data"]["current_owner"]
            .as_str()
            .unwrap_or_default()
            .contains("serve-pid-999"),
        "the busy error must name the other process: {}",
        blocked
    );

    // Once the other process exits (lock released), the run proceeds.
    drop(competitor);
    let retry = rpc(&disp, "session.prompt", json!({ "prompt": "now free" })).await;
    assert!(
        retry.get("error").is_none(),
        "the workspace must be free after the other process exits: {}",
        retry
    );

    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn different_workspaces_run_independently() {
    let root = temp_root("writer-independent");
    let ws_a = root.join("ws-a");
    let ws_b = root.join("ws-b");
    std::fs::create_dir_all(&ws_a).unwrap();
    std::fs::create_dir_all(&ws_b).unwrap();

    let slow = Duration::from_millis(250);
    let cfg_a = KromaRuntimeConfig::new(&ws_a, root.join("state-a"), PolicyMode::AllowWrite);
    let disp_a = RpcDispatcher::new(cfg_a, Arc::new(SlowProvider { delay: slow })).unwrap();
    let cfg_b = KromaRuntimeConfig::new(&ws_b, root.join("state-b"), PolicyMode::AllowWrite);
    let disp_b = RpcDispatcher::new(cfg_b, Arc::new(SlowProvider { delay: slow })).unwrap();

    let a = disp_a.clone();
    let b = disp_b.clone();
    let run_a =
        tokio::spawn(async move { rpc(&a, "session.prompt", json!({ "prompt": "a" })).await });
    let run_b =
        tokio::spawn(async move { rpc(&b, "session.prompt", json!({ "prompt": "b" })).await });
    let res_a = run_a.await.unwrap();
    let res_b = run_b.await.unwrap();
    assert!(
        res_a.get("error").is_none(),
        "workspace A failed: {}",
        res_a
    );
    assert!(
        res_b.get("error").is_none(),
        "workspace B failed: {}",
        res_b
    );

    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn session_route_and_misrouting() {
    let root = temp_root("session-route");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let cfg = KromaRuntimeConfig::new(&ws, root.join("state"), PolicyMode::AllowWrite);
    let disp = RpcDispatcher::new(cfg, Arc::new(FakeProvider::new(vec![stop()]))).unwrap();

    let ownership = disp.ownership();
    let route = kroma_core::session_ownership::lookup_session_route(&ownership.session_id)
        .expect("the owning runtime must be registered in the routing table");
    assert_eq!(route.owner_id, ownership.dispatcher_id);
    assert_eq!(
        route.workspace_root,
        std::fs::canonicalize(&ws).unwrap(),
        "the route must name the canonical workspace root"
    );
    assert!(kroma_core::session_ownership::sessions_for_workspace(&ws)
        .iter()
        .any(|r| r.session_id == ownership.session_id));

    // A request that names a different session is rejected instead of mutating this one.
    let misrouted = rpc(
        &disp,
        "session.prompt",
        json!({ "prompt": "wrong session", "session_id": "not-this-session" }),
    )
    .await;
    assert_eq!(
        err_code(&misrouted),
        Some(JSONRPC_SESSION_NOT_OWNED),
        "a misrouted request must be rejected: {}",
        misrouted
    );

    let _ = std::fs::remove_dir_all(root);
}

/// KR-T03: a WAL written by an older/incompatible format must be refused explicitly.
///
/// The acceptance says "no silent irreversible format damage; a downgrade that cannot be recovered
/// must be refused up front". The reader verifies four things per line (valid JSON, contiguous seq,
/// prev_hash chain, hash) and the header (seq 0, zero prev_hash). This pins the refusal for the
/// shape an older writer would produce, so a future format change cannot quietly reinterpret old
/// bytes as new ones.
#[test]
fn an_older_wal_format_is_refused_rather_than_reinterpreted() {
    use kroma_core::session::{SessionError, SessionJournal};

    let root = std::env::temp_dir().join(format!("kroma-t03-wal-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();

    // (a) header missing the hash field entirely -- what an older writer could emit.
    let old = root.join("old-format.wal.jsonl");
    std::fs::write(
        &old,
        "{\"seq\":0,\"prev_hash\":\"0000000000000000000000000000000000000000000000000000000000000000\"}\n",
    )
    .unwrap();
    let err = match SessionJournal::open(&old) {
        Ok(_) => panic!("an incomplete header must be refused"),
        Err(e) => e,
    };
    assert!(
        matches!(err, SessionError::Invalid(_)),
        "expected an explicit Invalid refusal, got {err:?}"
    );

    // (b) header that parses but has the wrong sequence -- a reordered/merged file.
    let bad_seq = root.join("bad-seq.wal.jsonl");
    std::fs::write(
        &bad_seq,
        "{\"seq\":7,\"prev_hash\":\"0000000000000000000000000000000000000000000000000000000000000000\",\"payload\":{},\"hash\":\"00\"}\n",
    )
    .unwrap();
    let err = match SessionJournal::open(&bad_seq) {
        Ok(_) => panic!("a wrong header seq must be refused"),
        Err(e) => e,
    };
    assert!(
        matches!(err, SessionError::Invalid(_)),
        "expected an explicit Invalid refusal, got {err:?}"
    );

    // (c) not a WAL at all.
    let garbage = root.join("garbage.wal.jsonl");
    std::fs::write(&garbage, "not json at all\n").unwrap();
    let err = match SessionJournal::open(&garbage) {
        Ok(_) => panic!("garbage must be refused"),
        Err(e) => e,
    };
    assert!(matches!(err, SessionError::Invalid(_)));

    let _ = std::fs::remove_dir_all(&root);
}
