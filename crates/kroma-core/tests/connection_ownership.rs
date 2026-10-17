//! B01: connection-scoped ownership of runs on the daemon stream.
//!
//! The multi-session daemon must not leak business events to an unauthenticated connection, and a
//! stream disconnect must cancel only the runs that stream actually started. These tests drive real
//! TCP streams through [`KromaDaemon`] so the connection lifecycle (accept → auth → dispatch → EOF)
//! is exercised end to end, with a `Notify`-gated provider instead of fixed sleeps for the "run has
//! started" signal.

use kroma_core::budget::BudgetConfig;
use kroma_core::messages::{AssistantTurn, FinishReason, ToolCall, TranscriptItem};
use kroma_core::provider::{
    FakeProvider, ModelProvider, ProviderCompletion, ProviderError, ProviderRequest,
};
use kroma_core::surface::{
    JsonRpcNotification, JsonRpcRequest, KromaDaemon, KromaRuntimeConfig, RpcDispatcher,
    SessionManager, SessionManagerConfig, SessionRuntimeFactory,
};
use kroma_core::tool::policy::PolicyMode;
use kroma_core::types::ToolCallId;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::future::Future;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;
use tokio::sync::{watch, Notify};

const TOKEN: &str = "b01-secret-token";
/// B12: a token for the remote task-control permission test.
const TASK_CONTROL_TOKEN: &str = "b12-task-control-token";
/// Bounded wait for a disconnect to propagate through the accept loop and read loop. The assertions
/// depend on the cancel decision, not on this window; it only needs to exceed a localhost EOF.
const EOF_SETTLE: Duration = Duration::from_millis(300);
/// Hard bound on every socket wait / provider-entry wait / daemon join, so a stuck task fails with a
/// stage-specific panic instead of hanging the test binary.
const IO_TIMEOUT: Duration = Duration::from_secs(5);

fn stop() -> AssistantTurn {
    AssistantTurn {
        content: Some("done".into()),
        reasoning_blocks: vec![],
        tool_calls: vec![],
        finish_reason: FinishReason::Stop,
        usage: None,
    }
}

fn completion() -> ProviderCompletion {
    ProviderCompletion {
        turn: stop(),
        request_body_bytes: 0,
        latency_ms: 0,
        ttft_ms: None,
        attempts_used: 1,
                reported_model: None,
    }
}

fn temp_root(label: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "kroma-connection-ownership-{}-{}",
        label,
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&root).unwrap();
    root
}

fn state_dir_for(base: &Path, workspace: &Path) -> PathBuf {
    let name = workspace
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "ws".to_string());
    base.join("state").join(name)
}

/// A provider that signals when a run has entered it and then blocks until released or cancelled.
/// The signal is a real `Notify` wake-up, so tests never infer "run started" from a wall clock.
#[derive(Clone)]
struct Gate {
    entered: Arc<Notify>,
    release: Arc<Notify>,
}

impl Gate {
    fn new() -> Self {
        Self {
            entered: Arc::new(Notify::new()),
            release: Arc::new(Notify::new()),
        }
    }

    /// Resolves once a run has entered the provider. `Notify::notify_one` stores a permit, so this
    /// is safe whether the run or the test wakes first. Bounded so a run that never starts fails
    /// with the calling stage instead of hanging.
    async fn wait_entered(&self, stage: &str) {
        match tokio::time::timeout(IO_TIMEOUT, self.entered.notified()).await {
            Ok(()) => {}
            Err(_) => panic!(
                "timed out after {IO_TIMEOUT:?} waiting for a run to enter the provider ({stage})"
            ),
        }
    }

    fn release(&self) {
        self.release.notify_one();
    }
}

struct GatedProvider {
    gate: Arc<Gate>,
}

impl ModelProvider for GatedProvider {
    fn complete<'a>(
        &'a self,
        _req: ProviderRequest,
        _budget: &'a BudgetConfig,
        mut cancel_rx: watch::Receiver<bool>,
    ) -> Pin<Box<dyn Future<Output = Result<ProviderCompletion, ProviderError>> + Send + 'a>> {
        let gate = Arc::clone(&self.gate);
        Box::pin(async move {
            gate.entered.notify_one();
            if *cancel_rx.borrow() {
                return Err(ProviderError::Cancelled);
            }
            tokio::select! {
                _ = gate.release.notified() => Ok(completion()),
                _ = cancel_rx.changed() => Err(ProviderError::Cancelled),
            }
        })
    }
}

type ProviderRecipe = Arc<dyn Fn(&Path) -> Arc<dyn ModelProvider> + Send + Sync>;

/// Minimal counting wrapper: records how many times a provider was asked to complete a turn, so a
/// test can prove a candidate child was (or was not) actually dispatched.
struct CountingProvider {
    calls: Arc<AtomicUsize>,
    inner: FakeProvider,
}

impl ModelProvider for CountingProvider {
    fn complete<'a>(
        &'a self,
        req: ProviderRequest,
        budget: &'a BudgetConfig,
        cancel_rx: watch::Receiver<bool>,
    ) -> Pin<Box<dyn Future<Output = Result<ProviderCompletion, ProviderError>> + Send + 'a>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.complete(req, budget, cancel_rx)
    }
}

/// B11b conservative delegation contract: an `Ask` parent refuses a `worker` subagent before any
/// candidate worktree is materialized. Matched *exactly* (not a broad `contains`) so the test can
/// only pass on the deterministic capability refusal — never on a cancel, timeout, or other error.
const ASK_WORKER_REFUSAL: &str = "policy denied: cannot spawn a `worker` subagent while the parent policy is `ask`; a worker auto-applies mutations and would exceed the parent's automatic authority. Authorize automatic writes explicitly (e.g. `--allow-write`) instead of relying on a single spawn approval";

/// Builds a runtime per session with a caller-supplied provider recipe and optional auth token.
struct FnFactory {
    base: PathBuf,
    auth_token: Option<String>,
    policy_mode: PolicyMode,
    provider: ProviderRecipe,
}

impl SessionRuntimeFactory for FnFactory {
    fn build(
        &self,
        workspace_root: &Path,
        resume_session_path: Option<&Path>,
    ) -> Result<RpcDispatcher, String> {
        let mut cfg = KromaRuntimeConfig::new(
            workspace_root,
            state_dir_for(&self.base, workspace_root),
            self.policy_mode,
        );
        cfg.resume_session_path = resume_session_path.map(Path::to_path_buf);
        if let Some(token) = &self.auth_token {
            cfg = cfg.with_auth_token(Some(token.clone()));
        }
        RpcDispatcher::new(cfg, (self.provider)(workspace_root))
    }
}

/// Registry of gates keyed by workspace, so a test can wait for the run it just started.
type Gates = Arc<Mutex<HashMap<PathBuf, Arc<Gate>>>>;

fn gated_recipe() -> (ProviderRecipe, Gates) {
    let gates: Gates = Arc::new(Mutex::new(HashMap::new()));
    let gates_for_recipe = Arc::clone(&gates);
    let recipe: ProviderRecipe = Arc::new(move |workspace: &Path| {
        let gate = gate_for(&gates_for_recipe, workspace);
        Arc::new(GatedProvider { gate }) as Arc<dyn ModelProvider>
    });
    (recipe, gates)
}

fn gate_for(gates: &Gates, workspace: &Path) -> Arc<Gate> {
    gates
        .lock()
        .unwrap()
        .entry(workspace.to_path_buf())
        .or_insert_with(|| Arc::new(Gate::new()))
        .clone()
}

fn build_manager(
    root: &Path,
    primary_ws: &Path,
    recipe: ProviderRecipe,
    token: Option<&str>,
    policy_mode: PolicyMode,
) -> SessionManager {
    let factory = Arc::new(FnFactory {
        base: root.to_path_buf(),
        auth_token: token.map(str::to_string),
        policy_mode,
        provider: recipe,
    });
    let primary = factory.build(primary_ws, None).unwrap();
    SessionManager::open(
        SessionManagerConfig {
            factory,
            max_sessions: 4,
            index_path: root.join("state").join("daemon-sessions.json"),
            default_workspace: primary_ws.to_path_buf(),
        },
        primary,
    )
    .unwrap()
}

struct RunningDaemon {
    addr: SocketAddr,
    cancel: watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
}

impl RunningDaemon {
    async fn shutdown(self) {
        let _ = self.cancel.send(true);
        match tokio::time::timeout(IO_TIMEOUT, self.task).await {
            Ok(_) => {}
            Err(_) => panic!("daemon did not shut down within {IO_TIMEOUT:?}"),
        }
    }
}

async fn start_multi(manager: SessionManager) -> RunningDaemon {
    let daemon = KromaDaemon::bind_multi("127.0.0.1:0", manager)
        .await
        .unwrap();
    spawn_daemon(daemon).await
}

async fn start_single(dispatcher: RpcDispatcher) -> RunningDaemon {
    let daemon = KromaDaemon::bind("127.0.0.1:0", dispatcher).await.unwrap();
    spawn_daemon(daemon).await
}

async fn spawn_daemon(daemon: KromaDaemon) -> RunningDaemon {
    let addr = daemon.local_addr();
    let (cancel, cancel_rx) = watch::channel(false);
    let task = tokio::spawn(async move {
        let _ = daemon.serve(cancel_rx).await;
    });
    RunningDaemon { addr, cancel, task }
}

/// One raw JSON-RPC client stream.
struct Client {
    write: OwnedWriteHalf,
    lines: tokio::io::Lines<BufReader<OwnedReadHalf>>,
    next_id: i64,
    /// Frames read while waiting for a different request id, so an out-of-order response is not
    /// lost when a second request is answered on the same connection.
    pending: Vec<Value>,
}

impl Client {
    async fn connect(addr: SocketAddr) -> Self {
        let stream = TcpStream::connect(addr).await.unwrap();
        let (read, write) = stream.into_split();
        Self {
            write,
            lines: BufReader::new(read).lines(),
            next_id: 0,
            pending: Vec::new(),
        }
    }

    /// Send a request without waiting for its response; returns the request id.
    async fn send(&mut self, method: &str, params: Value) -> i64 {
        self.next_id += 1;
        let id = self.next_id;
        let req = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        self.write
            .write_all(format!("{}\n", req).as_bytes())
            .await
            .unwrap();
        self.write.flush().await.unwrap();
        id
    }

    /// Read one raw line straight off the socket. Callers bound it, so `recv_response` can report
    /// the exact response it was waiting for rather than a generic socket timeout.
    async fn read_line(&mut self) -> Option<String> {
        self.lines.next_line().await.unwrap()
    }

    async fn recv(&mut self) -> Value {
        if !self.pending.is_empty() {
            return self.pending.remove(0);
        }
        match tokio::time::timeout(IO_TIMEOUT, self.read_line()).await {
            Ok(Some(line)) => serde_json::from_str(&line).unwrap(),
            Ok(None) => panic!("stream closed while a frame was expected"),
            Err(_) => panic!("timed out after {IO_TIMEOUT:?} waiting for a frame"),
        }
    }

    async fn recv_response(&mut self, id: i64, stage: &str) -> Value {
        if let Some(pos) = self
            .pending
            .iter()
            .position(|f| f.get("id").and_then(Value::as_i64) == Some(id))
        {
            return self.pending.remove(pos);
        }
        loop {
            let frame = match tokio::time::timeout(IO_TIMEOUT, self.read_line()).await {
                Ok(Some(line)) => serde_json::from_str::<Value>(&line).unwrap(),
                Ok(None) => panic!("stream closed while waiting for response id={id} ({stage})"),
                Err(_) => {
                    panic!("timed out after {IO_TIMEOUT:?} waiting for response id={id} ({stage})")
                }
            };
            if frame.get("id").and_then(Value::as_i64) == Some(id) {
                return frame;
            }
            self.pending.push(frame);
        }
    }

    async fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.send(method, params).await;
        self.recv_response(id, method).await
    }

    /// Write raw frames in a single buffer, so the read loop can process them without yielding to a
    /// spawned task (used to force cancel-before-claim ordering on a current-thread runtime).
    async fn send_raw(&mut self, frames: &str) {
        self.write.write_all(frames.as_bytes()).await.unwrap();
        self.write.flush().await.unwrap();
    }

    /// Authenticate by carrying the token on a benign request; the stream latches on success.
    async fn authenticate(&mut self, token: &str) {
        let resp = self
            .request("session.status", json!({ "auth_token": token }))
            .await;
        assert!(resp.get("error").is_none(), "auth failed: {}", resp);
    }
}

// ---------------------------------------------------------------------------------------------
// F01 — an unauthenticated connection is not subscribed to business events.
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn unauthenticated_connection_receives_no_business_notifications() {
    let root = temp_root("f01-unauth");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let (recipe, _gates) = gated_recipe();
    let manager = build_manager(&root, &ws, recipe, Some(TOKEN), PolicyMode::AllowWrite);
    let session = manager.default_session_id().unwrap();
    let running = start_multi(manager.clone()).await;

    let mut client = Client::connect(running.addr).await;
    // Prove the stream loop is live and has processed this connection: the request is refused.
    let refused = client.request("session.status", json!({})).await;
    assert_eq!(
        refused["error"]["code"],
        json!(-32001),
        "an unauthenticated request must be refused: {}",
        refused
    );

    // A business event emitted on the session must not reach this connection.
    manager
        .runtime_for(&session)
        .unwrap()
        .emit_notification(JsonRpcNotification::new(
            "review/sensitive",
            json!({ "marker": "UNAUTHENTICATED_EVENT_LEAK" }),
        ));

    let leaked = tokio::time::timeout(Duration::from_millis(400), client.recv()).await;
    assert!(
        leaked.is_err(),
        "unauthenticated connection received a business event: {:?}",
        leaked
    );

    running.shutdown().await;
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn authenticated_connection_receives_business_notifications() {
    let root = temp_root("f01-auth");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let (recipe, _gates) = gated_recipe();
    let manager = build_manager(&root, &ws, recipe, Some(TOKEN), PolicyMode::AllowWrite);
    let session = manager.default_session_id().unwrap();
    let running = start_multi(manager.clone()).await;

    let mut client = Client::connect(running.addr).await;
    client.authenticate(TOKEN).await;

    // Once authenticated, the same event must arrive (the fix must not over-gate).
    manager
        .runtime_for(&session)
        .unwrap()
        .emit_notification(JsonRpcNotification::new(
            "review/sensitive",
            json!({ "marker": "AUTHENTICATED_EVENT" }),
        ));
    let frame = tokio::time::timeout(Duration::from_millis(400), client.recv())
        .await
        .expect("an authenticated connection must receive the business event");
    assert_eq!(frame["method"], json!("review/sensitive"));
    assert_eq!(frame["params"]["marker"], json!("AUTHENTICATED_EVENT"));

    running.shutdown().await;
    let _ = std::fs::remove_dir_all(root);
}

// ---------------------------------------------------------------------------------------------
// F02 — a disconnect cancels only the runs the disconnecting connection owns.
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn unauthenticated_disconnect_does_not_cancel_a_run() {
    let root = temp_root("f02-unauth");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let (recipe, gates) = gated_recipe();
    let gate = gate_for(&gates, &ws);
    let manager = build_manager(&root, &ws, recipe, Some(TOKEN), PolicyMode::AllowWrite);
    let session = manager.default_session_id().unwrap();
    let running = start_multi(manager).await;

    let mut owner = Client::connect(running.addr).await;
    owner.authenticate(TOKEN).await;
    let prompt_id = owner
        .send(
            "session.prompt",
            json!({ "prompt": "slow", "session_id": session }),
        )
        .await;
    gate.wait_entered("owner run start").await;

    // A second connection that never authenticates and then goes away.
    let stranger = Client::connect(running.addr).await;
    drop(stranger);
    tokio::time::sleep(EOF_SETTLE).await;

    gate.release();
    let resp = owner
        .recv_response(prompt_id, "owner prompt response")
        .await;
    assert!(resp.get("error").is_none(), "owner run failed: {}", resp);
    assert_ne!(
        resp["result"]["status"],
        json!("cancelled"),
        "an unauthenticated EOF cancelled the run: {}",
        resp
    );

    running.shutdown().await;
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn authenticated_observer_disconnect_does_not_cancel_a_run() {
    let root = temp_root("f02-observer");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let (recipe, gates) = gated_recipe();
    let gate = gate_for(&gates, &ws);
    let manager = build_manager(&root, &ws, recipe, Some(TOKEN), PolicyMode::AllowWrite);
    let session = manager.default_session_id().unwrap();
    let running = start_multi(manager).await;

    let mut owner = Client::connect(running.addr).await;
    owner.authenticate(TOKEN).await;
    let prompt_id = owner
        .send(
            "session.prompt",
            json!({ "prompt": "slow", "session_id": session }),
        )
        .await;
    gate.wait_entered("owner run start").await;

    // An authenticated read-only observer that never starts a run.
    let mut observer = Client::connect(running.addr).await;
    observer.authenticate(TOKEN).await;
    let _ = observer
        .request("session.status", json!({ "session_id": session }))
        .await;
    drop(observer);
    tokio::time::sleep(EOF_SETTLE).await;

    gate.release();
    let resp = owner
        .recv_response(prompt_id, "owner prompt response")
        .await;
    assert!(resp.get("error").is_none(), "owner run failed: {}", resp);
    assert_ne!(
        resp["result"]["status"],
        json!("cancelled"),
        "an authenticated observer's EOF cancelled another connection's run: {}",
        resp
    );

    running.shutdown().await;
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn owner_disconnect_cancels_only_its_own_run() {
    let root = temp_root("f02-owner");
    let ws_a = root.join("ws-a");
    let ws_b = root.join("ws-b");
    std::fs::create_dir_all(&ws_a).unwrap();
    std::fs::create_dir_all(&ws_b).unwrap();
    let (recipe, gates) = gated_recipe();
    let gate_a = gate_for(&gates, &ws_a);
    let gate_b = gate_for(&gates, &ws_b);
    let manager = build_manager(&root, &ws_a, recipe, Some(TOKEN), PolicyMode::AllowWrite);
    let session_a = manager.default_session_id().unwrap();
    let session_b = manager.open_session(Some(&ws_b), None).unwrap().session_id;
    let running = start_multi(manager).await;

    let mut conn_a = Client::connect(running.addr).await;
    conn_a.authenticate(TOKEN).await;
    let _prompt_a = conn_a
        .send(
            "session.prompt",
            json!({ "prompt": "slow A", "session_id": session_a }),
        )
        .await;
    gate_a.wait_entered("session A run start").await;

    let mut conn_b = Client::connect(running.addr).await;
    conn_b.authenticate(TOKEN).await;
    let prompt_b = conn_b
        .send(
            "session.prompt",
            json!({ "prompt": "slow B", "session_id": session_b }),
        )
        .await;
    gate_b.wait_entered("session B run start").await;

    // Connection A owns session A's run; closing it must cancel that run and nothing else. Poll
    // (bounded) for the cancel to land instead of assuming a fixed propagation delay.
    drop(conn_a);
    let deadline = tokio::time::Instant::now() + IO_TIMEOUT;
    loop {
        let status_a = conn_b
            .request("session.status", json!({ "session_id": session_a }))
            .await;
        if status_a["result"]["in_flight"] == json!(false) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "connection A's run was not cancelled after its EOF: {}",
            status_a
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    gate_b.release();
    let resp_b = conn_b
        .recv_response(prompt_b, "session B prompt response")
        .await;
    assert!(
        resp_b.get("error").is_none(),
        "session B failed: {}",
        resp_b
    );
    assert_ne!(
        resp_b["result"]["status"],
        json!("cancelled"),
        "connection A's EOF cancelled session B's run: {}",
        resp_b
    );

    running.shutdown().await;
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn busy_requester_disconnect_does_not_cancel_the_owner_run() {
    let root = temp_root("f02-busy");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let (recipe, gates) = gated_recipe();
    let gate = gate_for(&gates, &ws);
    let manager = build_manager(&root, &ws, recipe, Some(TOKEN), PolicyMode::AllowWrite);
    let session = manager.default_session_id().unwrap();
    let running = start_multi(manager).await;

    let mut owner = Client::connect(running.addr).await;
    owner.authenticate(TOKEN).await;
    let prompt_id = owner
        .send(
            "session.prompt",
            json!({ "prompt": "slow", "session_id": session }),
        )
        .await;
    gate.wait_entered("owner run start").await;

    // A second controller races the busy session: it must be refused and must not claim ownership.
    let mut contender = Client::connect(running.addr).await;
    contender.authenticate(TOKEN).await;
    let busy = contender
        .request(
            "session.prompt",
            json!({ "prompt": "busy", "session_id": session }),
        )
        .await;
    assert!(
        busy.get("error").is_some(),
        "a prompt on a busy session must be refused: {}",
        busy
    );
    drop(contender);
    tokio::time::sleep(EOF_SETTLE).await;

    gate.release();
    let resp = owner
        .recv_response(prompt_id, "owner prompt response")
        .await;
    assert!(resp.get("error").is_none(), "owner run failed: {}", resp);
    assert_ne!(
        resp["result"]["status"],
        json!("cancelled"),
        "a refused (busy) requester's EOF cancelled the owner's run: {}",
        resp
    );

    running.shutdown().await;
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn completed_connection_disconnect_does_not_cancel_a_new_run() {
    let root = temp_root("f02-completed");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let (recipe, gates) = gated_recipe();
    let gate = gate_for(&gates, &ws);
    let manager = build_manager(&root, &ws, recipe, Some(TOKEN), PolicyMode::AllowWrite);
    let session = manager.default_session_id().unwrap();
    let running = start_multi(manager).await;

    // Connection 1 runs a prompt to completion, then stays connected as a plain observer.
    let mut old = Client::connect(running.addr).await;
    old.authenticate(TOKEN).await;
    let first = old
        .send(
            "session.prompt",
            json!({ "prompt": "first", "session_id": session }),
        )
        .await;
    gate.wait_entered("owner run start").await;
    gate.release();
    let first_resp = old.recv_response(first, "first prompt response").await;
    assert!(first_resp.get("error").is_none(), "{}", first_resp);

    // Connection 2 starts a new run on the same session.
    let mut current = Client::connect(running.addr).await;
    current.authenticate(TOKEN).await;
    let second = current
        .send(
            "session.prompt",
            json!({ "prompt": "second", "session_id": session }),
        )
        .await;
    gate.wait_entered("owner run start").await;

    // The old connection's EOF must not touch the new run.
    drop(old);
    tokio::time::sleep(EOF_SETTLE).await;
    gate.release();
    let second_resp = current
        .recv_response(second, "second prompt response")
        .await;
    assert!(
        second_resp.get("error").is_none(),
        "current run failed: {}",
        second_resp
    );
    assert_ne!(
        second_resp["result"]["status"],
        json!("cancelled"),
        "a completed connection's EOF cancelled a newer run: {}",
        second_resp
    );

    running.shutdown().await;
    let _ = std::fs::remove_dir_all(root);
}

// ---------------------------------------------------------------------------------------------
// Single-dispatcher stream: an observer's EOF must not kill another connection's run.
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn single_dispatcher_observer_disconnect_does_not_cancel_the_run() {
    let root = temp_root("f02-single");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let gate = Arc::new(Gate::new());
    let cfg = KromaRuntimeConfig::new(&ws, root.join("state"), PolicyMode::AllowWrite)
        .with_surface("serve", "tcp")
        .with_auth_token(Some(TOKEN.to_string()));
    let dispatcher = RpcDispatcher::new(
        cfg,
        Arc::new(GatedProvider {
            gate: Arc::clone(&gate),
        }),
    )
    .unwrap();
    let running = start_single(dispatcher).await;

    let mut owner = Client::connect(running.addr).await;
    owner.authenticate(TOKEN).await;
    let prompt_id = owner
        .send("session.prompt", json!({ "prompt": "slow" }))
        .await;
    gate.wait_entered("owner run start").await;

    let mut observer = Client::connect(running.addr).await;
    observer.authenticate(TOKEN).await;
    drop(observer);
    tokio::time::sleep(EOF_SETTLE).await;

    gate.release();
    let resp = owner
        .recv_response(prompt_id, "owner prompt response")
        .await;
    assert!(resp.get("error").is_none(), "owner run failed: {}", resp);
    assert_ne!(
        resp["result"]["status"],
        json!("cancelled"),
        "single-dispatcher observer EOF cancelled the owner's run: {}",
        resp
    );

    running.shutdown().await;
    let _ = std::fs::remove_dir_all(root);
}

// ---------------------------------------------------------------------------------------------
// Startup race: a connection that closes while its prompt is still starting must not leak a run.
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn connection_closed_during_prompt_start_leaves_no_in_flight_run() {
    let root = temp_root("f02-start-race");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let (recipe, _gates) = gated_recipe();
    let manager = build_manager(&root, &ws, recipe, Some(TOKEN), PolicyMode::AllowWrite);
    let session = manager.default_session_id().unwrap();
    let running = start_multi(manager).await;

    // Send a prompt and drop the connection immediately, without waiting for a response.
    let mut flaky = Client::connect(running.addr).await;
    flaky.authenticate(TOKEN).await;
    let _ = flaky
        .send(
            "session.prompt",
            json!({ "prompt": "race", "session_id": session }),
        )
        .await;
    drop(flaky);
    tokio::time::sleep(EOF_SETTLE).await;

    // The session must not be left holding a run (which would block the next prompt as busy).
    let mut probe = Client::connect(running.addr).await;
    probe.authenticate(TOKEN).await;
    let status = probe
        .request("session.status", json!({ "session_id": session }))
        .await;
    assert_eq!(
        status["result"]["in_flight"],
        json!(false),
        "a run leaked past its closed connection: {}",
        status
    );

    running.shutdown().await;
    let _ = std::fs::remove_dir_all(root);
}

// ---------------------------------------------------------------------------------------------
// Cancel admission: an explicit cancel ordered after the prompt frame must reach the run even when
// it arrives before the spawned prompt task has claimed its cancel channel.
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn explicit_cancel_after_prompt_frame_is_not_lost() {
    let root = temp_root("cancel-before-claim");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let (recipe, gates) = gated_recipe();
    let gate = gate_for(&gates, &ws);
    let manager = build_manager(&root, &ws, recipe, Some(TOKEN), PolicyMode::AllowWrite);
    let session = manager.default_session_id().unwrap();
    let running = start_multi(manager).await;

    let mut client = Client::connect(running.addr).await;
    client.authenticate(TOKEN).await;

    // One write: the prompt frame, then the cancel frame. The read loop must route the cancel to
    // the prompt request that was registered by the prompt frame.
    let prompt = json!({
        "jsonrpc": "2.0", "id": 101, "method": "session.prompt",
        "params": { "prompt": "slow", "session_id": session }
    });
    let cancel = json!({
        "jsonrpc": "2.0", "id": 102, "method": "session.cancel",
        "params": { "session_id": session }
    });
    client.send_raw(&format!("{prompt}\n{cancel}\n")).await;

    // Release the gated provider in case the run did start; a cancelled run returns without it.
    gate.release();
    let resp = client
        .recv_response(101, "prompt response after cancel")
        .await;
    assert_eq!(
        resp["result"]["status"],
        json!("cancelled"),
        "explicit cancel ordered after the prompt frame was lost: {}",
        resp
    );

    running.shutdown().await;
    let _ = std::fs::remove_dir_all(root);
}

// ---------------------------------------------------------------------------------------------
// ACP ownership cleanup: a completed owner run must release its stream's writer task, otherwise
// every ACP run leaks one daemon connection permit.
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn acp_owner_run_completion_releases_its_connection_permit() {
    let root = temp_root("acp-sink-cleanup");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let cfg = KromaRuntimeConfig::new(&ws, root.join("state"), PolicyMode::AllowWrite)
        .with_surface("serve", "tcp")
        .with_acp_protocol(true)
        .with_auth_token(Some(TOKEN.to_string()));
    let dispatcher = RpcDispatcher::new(cfg, Arc::new(FakeProvider::new(vec![stop()]))).unwrap();
    let running = start_single(dispatcher).await;

    for i in 0..(kroma_core::surface::MAX_DAEMON_CONNECTIONS + 2) {
        let mut client = Client::connect(running.addr).await;
        client.authenticate(TOKEN).await;
        let id = client
            .send("session.prompt", json!({ "prompt": format!("turn {i}") }))
            .await;
        let resp = client.recv_response(id, "acp owner prompt response").await;
        assert!(resp.get("error").is_none(), "cycle {i} failed: {}", resp);
        drop(client);
    }

    // A fresh connection must still be served: permits must not have leaked.
    let mut probe = Client::connect(running.addr).await;
    probe.authenticate(TOKEN).await;
    let status = probe.request("session.status", json!({})).await;
    assert!(
        status.get("error").is_none(),
        "completed ACP owner runs leaked connection permits: {}",
        status
    );

    running.shutdown().await;
    let _ = std::fs::remove_dir_all(root);
}

// ---------------------------------------------------------------------------------------------
// Tournament ownership. Two complementary cases, because B11b made an `Ask` tournament
// deterministically refuse its `worker` candidates:
//   1. approval survival (an `Ask` parent): observer EOF must not cancel the pending approval; the
//      operation only stops at the deterministic capability refusal;
//   2. legitimate success (an `AllowWrite` parent, no approval): observer EOF during an *active*
//      tournament must not cancel the owner, and a real candidate completes with worktrees cleaned.
// Together they cover "approval survives" + "legitimate task succeeds"; case 1 alone is a
// conservative B11b migration, not "behaviour unchanged".
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn observer_disconnect_preserves_tournament_approval_until_capability_refusal() {
    let root = temp_root("f02-tournament");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let child_calls = Arc::new(AtomicUsize::new(0));
    let calls_for_recipe = Arc::clone(&child_calls);
    let recipe: ProviderRecipe = Arc::new(move |_ws: &Path| {
        Arc::new(CountingProvider {
            calls: Arc::clone(&calls_for_recipe),
            inner: FakeProvider::new(vec![stop(), stop(), stop()]),
        }) as Arc<dyn ModelProvider>
    });
    let manager = build_manager(&root, &ws, recipe, Some(TOKEN), PolicyMode::Ask);
    let session = manager.default_session_id().unwrap();
    let running = start_multi(manager).await;

    let mut owner = Client::connect(running.addr).await;
    owner.authenticate(TOKEN).await;
    let tournament_id = owner
        .send(
            "session.tournament",
            json!({ "task": "do something", "candidates": 2, "session_id": session }),
        )
        .await;

    // Wait for the preflight approval request. `recv` is internally bounded, so a missing event
    // fails fast; an error response for the tournament id fails immediately instead of hanging.
    let request_id = loop {
        let frame = owner.recv().await;
        if frame.get("id").and_then(Value::as_i64) == Some(tournament_id) {
            panic!("session.tournament was rejected before the approval prompt: {frame}");
        }
        if frame.get("method").and_then(Value::as_str) == Some("approval/requested") {
            break frame["params"]["request_id"]
                .as_str()
                .expect("approval request carries an id")
                .to_string();
        }
    };

    // An authenticated observer connects and leaves while the owner is mid-approval.
    let mut observer = Client::connect(running.addr).await;
    observer.authenticate(TOKEN).await;
    drop(observer);
    tokio::time::sleep(EOF_SETTLE).await;

    // The approval must survive the observer's EOF: responding must be accepted, not denied.
    let approve_id = owner
        .send(
            "approval.respond",
            json!({ "request_id": request_id, "decision": "approve_once", "auth_token": TOKEN }),
        )
        .await;
    let approve_resp = owner
        .recv_response(approve_id, "approval.respond response")
        .await;
    assert!(
        approve_resp.get("error").is_none(),
        "observer EOF cancelled the pending approval: {}",
        approve_resp
    );

    // The tournament then stops at the *deterministic* B11b capability refusal (an `Ask` parent
    // cannot delegate an auto-mutating `worker`), never at a cancel/timeout/other error. The exact
    // text is matched so a generic error cannot satisfy this.
    let resp = owner
        .recv_response(tournament_id, "tournament response")
        .await;
    let err = resp
        .get("error")
        .unwrap_or_else(|| panic!("Ask parent must refuse the worker tournament, got: {resp}"));
    assert_eq!(
        err.get("message").and_then(Value::as_str),
        Some(ASK_WORKER_REFUSAL),
        "expected the exact capability refusal, got: {resp}"
    );
    assert_eq!(
        child_calls.load(Ordering::SeqCst),
        0,
        "a refused delegation must not dispatch any candidate child"
    );

    // The owner's connection/run was not cancelled: it still serves requests.
    let status = owner
        .request("session.status", json!({ "session_id": session }))
        .await;
    assert!(
        status.get("error").is_none(),
        "observer EOF cancelled the owner's connection/run: {status}"
    );

    running.shutdown().await;
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn observer_disconnect_during_active_tournament_does_not_cancel_a_succeeding_run() {
    let root = temp_root("f02-tournament-ok");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let gate = Arc::new(Gate::new());
    let gate_for_recipe = Arc::clone(&gate);
    let recipe: ProviderRecipe = Arc::new(move |_ws: &Path| {
        Arc::new(GatedProvider {
            gate: Arc::clone(&gate_for_recipe),
        }) as Arc<dyn ModelProvider>
    });
    let manager = build_manager(&root, &ws, recipe, Some(TOKEN), PolicyMode::AllowWrite);
    let session = manager.default_session_id().unwrap();
    let running = start_multi(manager).await;

    let mut owner = Client::connect(running.addr).await;
    owner.authenticate(TOKEN).await;
    let tournament_id = owner
        .send(
            "session.tournament",
            json!({ "task": "do something", "candidates": 2, "session_id": session }),
        )
        .await;

    // The tournament is now *active*: a candidate child is inside the provider. Under `AllowWrite`
    // there is no approval prompt, so this is the complementary case to the `Ask` test above.
    gate.wait_entered("tournament candidate").await;

    // An authenticated observer connects and leaves mid-tournament.
    let mut observer = Client::connect(running.addr).await;
    observer.authenticate(TOKEN).await;
    drop(observer);
    tokio::time::sleep(EOF_SETTLE).await;

    // Release every candidate. `Notify::notify_one` wakes one waiter and stores a permit for a
    // candidate that enters after this point, so repeated calls cover concurrent or sequential runs.
    for _ in 0..4 {
        gate.release();
    }

    let resp = owner
        .recv_response(tournament_id, "tournament response")
        .await;
    assert!(
        resp.get("error").is_none(),
        "observer EOF cancelled the owner's active tournament: {resp}"
    );
    let result = &resp["result"];
    let candidates = result["candidates"]
        .as_array()
        .expect("tournament result carries candidates");
    assert_eq!(candidates.len(), 2, "both candidates must run: {resp}");
    for candidate in candidates {
        assert_eq!(
            candidate["status"], "completed",
            "observer EOF must not cancel a candidate: {resp}"
        );
    }
    assert_eq!(
        result["all_worktrees_cleaned_up"], true,
        "every tournament worktree must be removed: {resp}"
    );
    // The worktree directory itself is left empty on disk.
    let worktrees = state_dir_for(&root, &ws).join("worktrees");
    if worktrees.exists() {
        let leftover: Vec<_> = std::fs::read_dir(&worktrees).unwrap().collect();
        assert!(leftover.is_empty(), "leftover worktrees: {leftover:?}");
    }

    // The completion event is broadcast to the owner's stream, proving the run reached its normal
    // end rather than being cancelled.
    let completed_event = owner.pending.iter().find(|f| {
        f.get("method").and_then(Value::as_str) == Some("kroma/event")
            && f["params"]["event"].as_str() == Some("tournament_completed")
    });
    assert!(
        completed_event.is_some(),
        "owner did not receive a tournament_completed event: pending={:?}",
        owner.pending
    );

    running.shutdown().await;
    let _ = std::fs::remove_dir_all(root);
}

// ---------------------------------------------------------------------------------------------
// Finding 1: single-dispatcher non-ACP Ask owner EOF does not permanently fail-close approval broker.
// A subsequent connection must be able to receive approval requests, approve them, and complete runs.
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn single_dispatcher_non_acp_ask_owner_disconnect_permits_subsequent_approval() {
    let root = temp_root("f01-reopen");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let gate = Arc::new(Gate::new());
    let cfg = KromaRuntimeConfig::new(&ws, root.join("state"), PolicyMode::Ask)
        .with_surface("serve", "tcp")
        .with_auth_token(Some(TOKEN.to_string()));

    struct F1Provider {
        gate: Arc<Gate>,
    }
    impl ModelProvider for F1Provider {
        fn complete<'a>(
            &'a self,
            req: ProviderRequest,
            _budget: &'a BudgetConfig,
            mut cancel_rx: watch::Receiver<bool>,
        ) -> Pin<Box<dyn Future<Output = Result<ProviderCompletion, ProviderError>> + Send + 'a>> {
            let gate = Arc::clone(&self.gate);
            Box::pin(async move {
                let last_user_prompt = req.messages.iter().rev().find_map(|m| match m {
                    TranscriptItem::User(u) => Some(u.content.as_str()),
                    _ => None,
                });
                if last_user_prompt == Some("slow_run_a") {
                    gate.entered.notify_one();
                    if *cancel_rx.borrow() {
                        return Err(ProviderError::Cancelled);
                    }
                    tokio::select! {
                        _ = gate.release.notified() => Ok(completion()),
                        _ = cancel_rx.changed() => Err(ProviderError::Cancelled),
                    }
                } else if req.messages.iter().any(|m| matches!(m, TranscriptItem::ToolResult(_))) {
                    Ok(completion())
                } else {
                    let turn = AssistantTurn {
                        content: None,
                        reasoning_blocks: vec![],
                        tool_calls: vec![ToolCall {
                            call_id: ToolCallId::new("call_f1_write"),
                            name: "write".to_string(),
                            raw_arguments: json!({ "path": "approved_f1.txt", "content": "f1_ok" }).to_string(),
                        }],
                        finish_reason: FinishReason::ToolCalls,
                        usage: None,
                    };
                    Ok(ProviderCompletion {
                        turn,
                        request_body_bytes: 0,
                        latency_ms: 0,
                        ttft_ms: None,
                        attempts_used: 1,
                reported_model: None,
                    })
                }
            })
        }
    }

    let dispatcher = RpcDispatcher::new(cfg, Arc::new(F1Provider { gate: Arc::clone(&gate) })).unwrap();
    let running = start_single(dispatcher).await;

    // 1. Connection A starts an in-flight run
    let mut conn_a = Client::connect(running.addr).await;
    conn_a.authenticate(TOKEN).await;
    let _prompt_a = conn_a.send("session.prompt", json!({ "prompt": "slow_run_a" })).await;
    gate.wait_entered("conn_a run start").await;

    // 2. Connection A disconnects while in-flight (triggers end_connection_run -> broker.close())
    drop(conn_a);
    tokio::time::sleep(EOF_SETTLE).await;
    gate.release();

    // 3. Connection B connects, authenticates, and starts a prompt requiring approval
    let mut conn_b = Client::connect(running.addr).await;
    conn_b.authenticate(TOKEN).await;
    let prompt_b = conn_b.send("session.prompt", json!({ "prompt": "run_b_needs_approval" })).await;

    // 4. B receives approval/requested
    let request_id = loop {
        let frame = conn_b.recv().await;
        if frame.get("id").and_then(Value::as_i64) == Some(prompt_b) {
            panic!("session.prompt for B finished before approval was received: {frame}");
        }
        if frame.get("method").and_then(Value::as_str) == Some("approval/requested") {
            assert_eq!(frame["params"]["tool_name"], "write");
            break frame["params"]["request_id"]
                .as_str()
                .expect("request_id")
                .to_string();
        }
    };

    // 5. B approves
    let respond_id = conn_b.send("approval.respond", json!({
        "request_id": request_id,
        "decision": "approve_once",
        "auth_token": TOKEN
    })).await;
    let respond_resp = conn_b.recv_response(respond_id, "approval.respond").await;
    assert!(respond_resp.get("error").is_none(), "approval.respond failed: {respond_resp}");

    // 6. Run B finishes successfully and tool writes file
    let resp_b = conn_b.recv_response(prompt_b, "conn_b prompt response").await;
    assert!(resp_b.get("error").is_none(), "prompt B failed: {resp_b}");
    assert_eq!(resp_b["result"]["status"], json!("completed"), "{resp_b}");
    assert!(ws.join("approved_f1.txt").exists(), "approved write tool was executed");

    running.shutdown().await;
    let _ = std::fs::remove_dir_all(root);
}

// ---------------------------------------------------------------------------------------------
// Finding 4: ACP session/request_permission response requires an authenticated connection that
// owns the exact active run. Stranger and observer forged responses are rejected; owner succeeds.
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn acp_permission_response_requires_authenticated_owner() {
    let root = temp_root("f04-acp-auth");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let cfg = KromaRuntimeConfig::new(&ws, root.join("state"), PolicyMode::Ask)
        .with_surface("serve", "tcp")
        .with_acp_protocol(true)
        .with_auth_token(Some(TOKEN.to_string()));

    struct F4Provider;
    impl ModelProvider for F4Provider {
        fn complete<'a>(
            &'a self,
            req: ProviderRequest,
            _budget: &'a BudgetConfig,
            _cancel_rx: watch::Receiver<bool>,
        ) -> Pin<Box<dyn Future<Output = Result<ProviderCompletion, ProviderError>> + Send + 'a>> {
            Box::pin(async move {
                if req.messages.iter().any(|m| matches!(m, TranscriptItem::ToolResult(_))) {
                    Ok(completion())
                } else {
                    let turn = AssistantTurn {
                        content: None,
                        reasoning_blocks: vec![],
                        tool_calls: vec![ToolCall {
                            call_id: ToolCallId::new("call_f4_write"),
                            name: "write".to_string(),
                            raw_arguments: json!({ "path": "f4_done.txt", "content": "f4_ok" }).to_string(),
                        }],
                        finish_reason: FinishReason::ToolCalls,
                        usage: None,
                    };
                    Ok(ProviderCompletion {
                        turn,
                        request_body_bytes: 0,
                        latency_ms: 0,
                        ttft_ms: None,
                        attempts_used: 1,
                reported_model: None,
                    })
                }
            })
        }
    }

    let dispatcher = RpcDispatcher::new(cfg, Arc::new(F4Provider)).unwrap();
    let running = start_single(dispatcher).await;

    // 1. Owner connection authenticates and initiates the run
    let mut owner = Client::connect(running.addr).await;
    owner.authenticate(TOKEN).await;
    let prompt_id = owner.send("session.prompt", json!({ "prompt": "trigger_acp" })).await;

    // 2. Owner waits for session/request_permission
    let req_id = loop {
        let frame = owner.recv().await;
        if frame.get("id").and_then(Value::as_i64) == Some(prompt_id) {
            panic!("prompt completed before permission was requested: {frame}");
        }
        if frame.get("method").and_then(Value::as_str) == Some("session/request_permission") {
            break frame["id"].as_str().expect("acp permission request id").to_string();
        }
    };

    // 3. Stranger (unauthenticated) sends forged rejection: must be rejected with -32001
    let mut stranger = Client::connect(running.addr).await;
    let forged_response_stranger = json!({
        "jsonrpc": "2.0",
        "id": req_id,
        "result": { "outcome": { "outcome": "selected", "optionId": "reject_once" } }
    });
    stranger.send_raw(&format!("{forged_response_stranger}\n")).await;
    let stranger_line = tokio::time::timeout(Duration::from_millis(500), stranger.read_line())
        .await
        .expect("unauthorized stranger response must be answered immediately")
        .expect("stream closed unexpectedly");
    let stranger_resp: Value = serde_json::from_str(&stranger_line).unwrap();
    assert_eq!(
        stranger_resp["error"]["code"],
        json!(-32001),
        "unauthenticated permission response must be rejected with UNAUTHORIZED: {stranger_resp}"
    );
    drop(stranger);

    // 4. Observer (authenticated, but does NOT own the active run) sends forged rejection: must be rejected with -32600
    let mut observer = Client::connect(running.addr).await;
    observer.authenticate(TOKEN).await;
    let forged_response_observer = json!({
        "jsonrpc": "2.0",
        "id": req_id,
        "result": { "outcome": { "outcome": "selected", "optionId": "reject_once" } }
    });
    observer.send_raw(&format!("{forged_response_observer}\n")).await;
    let observer_line = tokio::time::timeout(Duration::from_millis(500), observer.read_line())
        .await
        .expect("wrong-owner observer response must be answered immediately")
        .expect("stream closed unexpectedly");
    let observer_resp: Value = serde_json::from_str(&observer_line).unwrap();
    assert_eq!(
        observer_resp["error"]["code"],
        json!(-32600),
        "wrong-owner permission response must be rejected with INVALID_REQUEST: {observer_resp}"
    );
    drop(observer);

    // 5. True owner sends response: must succeed and allow run to complete
    let legitimate_response = json!({
        "jsonrpc": "2.0",
        "id": req_id,
        "result": { "outcome": { "outcome": "selected", "optionId": "allow_once" } }
    });
    owner.send_raw(&format!("{legitimate_response}\n")).await;

    let resp = owner.recv_response(prompt_id, "acp owner prompt response").await;
    assert!(resp.get("error").is_none(), "owner run failed: {resp}");
    assert_eq!(resp["result"]["status"], json!("completed"), "{resp}");
    assert!(ws.join("f4_done.txt").exists(), "approved write was executed");

    running.shutdown().await;
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn b02_ab_omitted_session_id_does_not_cross_route() {
    let root = temp_root("b02-ab-route");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let (recipe, _gates) = gated_recipe();
    let manager = build_manager(&root, &ws, recipe, Some(TOKEN), PolicyMode::AllowWrite);
    let daemon = start_multi(manager).await;

    let mut client_a = Client::connect(daemon.addr).await;
    client_a.authenticate(TOKEN).await;
    let initial_status = client_a.request("session.status", json!({})).await;
    let primary_session_id = initial_status["result"]["session_id"]
        .as_str()
        .unwrap()
        .to_string();

    let mut client_b = Client::connect(daemon.addr).await;
    client_b.authenticate(TOKEN).await;
    let new_resp = client_b.request("session.new", json!({})).await;
    let new_session_id = new_resp["result"]["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(primary_session_id, new_session_id);

    // Client A sends another request omitting session_id.
    // In connection-private routing, Client A must stay on primary_session_id,
    // not contaminated by Client B's session.new.
    let a_status_after = client_a.request("session.status", json!({})).await;
    let a_routed_session_id = a_status_after["result"]["session_id"].as_str().unwrap();
    assert_eq!(
        a_routed_session_id, primary_session_id,
        "Client A was cross-routed to Client B's session!"
    );

    daemon.shutdown().await;
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn b02_idle_connection_eof_reclaims_stream_cleanly() {
    let root = temp_root("b02-idle-eof");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let (recipe, _gates) = gated_recipe();
    let manager = build_manager(&root, &ws, recipe, Some(TOKEN), PolicyMode::AllowWrite);

    let (client_io, server_io) = tokio::io::duplex(4096);
    let (s_read, s_write) = tokio::io::split(server_io);
    let (c_read, mut c_write) = tokio::io::split(client_io);

    let mgr = manager.clone();
    let server_task = tokio::spawn(async move {
        mgr.serve_stream(s_read, s_write).await
    });

    let mut client_lines = BufReader::new(c_read).lines();

    // Authenticate client
    let auth_req = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "session.status",
        "params": { "auth_token": TOKEN }
    });
    c_write
        .write_all(format!("{auth_req}\n").as_bytes())
        .await
        .unwrap();
    c_write.flush().await.unwrap();

    let resp_line = client_lines
        .next_line()
        .await
        .unwrap()
        .expect("auth response");
    let resp: Value = serde_json::from_str(&resp_line).unwrap();
    assert!(resp.get("result").is_some());
    assert_eq!(
        manager.primary().subscriber_count(),
        1,
        "authenticated client should be subscribed"
    );

    // Send EOF from client
    drop(c_write);
    drop(client_lines);

    // Give the server task enough yields to process EOF and finish.
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }

    let finished = server_task.is_finished();
    if !finished {
        server_task.abort();
    }
    assert!(
        finished,
        "serve_stream hung on idle EOF (subscriber tasks leaked out_tx)"
    );
    assert_eq!(
        manager.primary().subscriber_count(),
        0,
        "disconnected client must have its subscriber unregistered"
    );

    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn b02_session_new_failure_does_not_contaminate_connection_default() {
    let root = temp_root("b02-new-fail");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let (recipe, _gates) = gated_recipe();
    let manager = build_manager(&root, &ws, recipe, Some(TOKEN), PolicyMode::AllowWrite);
    let daemon = start_multi(manager).await;

    let mut client = Client::connect(daemon.addr).await;
    client.authenticate(TOKEN).await;

    let initial_status = client.request("session.status", json!({})).await;
    let primary_session_id = initial_status["result"]["session_id"]
        .as_str()
        .unwrap()
        .to_string();

    // Fill up to max_sessions (max_sessions = 4 in build_manager)
    for _ in 0..3 {
        let resp = client.request("session.new", json!({})).await;
        assert!(resp.get("result").is_some(), "session.new should succeed");
    }
    let status_before_overflow = client.request("session.status", json!({})).await;
    let valid_session_id = status_before_overflow["result"]["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(primary_session_id, valid_session_id);

    // Now attempt 5th session.new, which must fail with JSONRPC_SESSION_LIMIT (-32007)
    let fail_resp = client.request("session.new", json!({})).await;
    assert!(fail_resp.get("error").is_some());
    assert_eq!(fail_resp["error"]["code"], json!(-32007));

    // The connection's default session must remain valid_session_id and not be corrupted
    let status_after = client.request("session.status", json!({})).await;
    assert_eq!(
        status_after["result"]["session_id"].as_str().unwrap(),
        valid_session_id,
        "failed session.new corrupted connection default session"
    );

    daemon.shutdown().await;
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn b02_session_close_falls_back_to_primary_session() {
    let root = temp_root("b02-close-fallback");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let (recipe, _gates) = gated_recipe();
    let manager = build_manager(&root, &ws, recipe, Some(TOKEN), PolicyMode::AllowWrite);
    let daemon = start_multi(manager).await;

    let mut client = Client::connect(daemon.addr).await;
    client.authenticate(TOKEN).await;

    let initial_status = client.request("session.status", json!({})).await;
    let primary_session_id = initial_status["result"]["session_id"]
        .as_str()
        .unwrap()
        .to_string();

    // Create session 2
    let new_resp = client.request("session.new", json!({})).await;
    let s2_id = new_resp["result"]["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(primary_session_id, s2_id);

    let s2_status = client.request("session.status", json!({})).await;
    assert_eq!(s2_status["result"]["session_id"].as_str().unwrap(), s2_id);

    // Close session 2
    let close_resp = client
        .request("session.close", json!({ "session_id": s2_id }))
        .await;
    assert!(close_resp.get("result").is_some());

    // Omitting session_id on subsequent request falls back to primary session
    let fallback_status = client.request("session.status", json!({})).await;
    assert_eq!(
        fallback_status["result"]["session_id"].as_str().unwrap(),
        primary_session_id,
        "closing current default session did not fall back to primary session"
    );

    daemon.shutdown().await;
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn b02_null_params_normalizes_to_target_session() {
    let root = temp_root("b02-null-params");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let (recipe, _gates) = gated_recipe();
    let manager = build_manager(&root, &ws, recipe, Some(TOKEN), PolicyMode::AllowWrite);
    let daemon = start_multi(manager).await;

    // Connect raw TCP socket so we can send exact JSON-RPC frames with null params
    let mut stream = tokio::net::TcpStream::connect(daemon.addr).await.unwrap();
    let (r, mut w) = stream.split();
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let mut reader = BufReader::new(r);

    // Auth
    w.write_all(format!("{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"auth\",\"params\":{{\"token\":\"{}\"}}}}\n", TOKEN).as_bytes()).await.unwrap();
    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();

    // Create session 2 on this raw connection
    w.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"session.new\",\"params\":{}}\n").await.unwrap();
    line.clear();
    reader.read_line(&mut line).await.unwrap();
    let s2_created: Value = serde_json::from_str(&line).unwrap();
    let raw_s2_id = s2_created["result"]["session_id"].as_str().unwrap().to_string();

    // Now send params: null for session.status
    let raw_null = r#"{"jsonrpc":"2.0","id":42,"method":"session.status","params":null}"#;
    w.write_all(format!("{}\n", raw_null).as_bytes()).await.unwrap();
    line.clear();
    reader.read_line(&mut line).await.unwrap();
    let status_resp: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(
        status_resp["result"]["session_id"].as_str().unwrap(),
        raw_s2_id,
        "params: null should normalize to connection default session instead of falling back to primary"
    );

    daemon.shutdown().await;
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn b02_session_close_primary_rejected() {
    let root = temp_root("b02-close-primary-rejected");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let (recipe, _gates) = gated_recipe();
    let manager = build_manager(&root, &ws, recipe, Some(TOKEN), PolicyMode::AllowWrite);
    let daemon = start_multi(manager).await;

    let mut client = Client::connect(daemon.addr).await;
    client.authenticate(TOKEN).await;

    let initial_status = client.request("session.status", json!({})).await;
    let primary_id = initial_status["result"]["session_id"].as_str().unwrap().to_string();

    // Attempting to close the primary session explicitly must be rejected with an error
    let close_resp = client.request("session.close", json!({ "session_id": primary_id })).await;
    assert!(
        close_resp.get("error").is_some(),
        "closing daemon primary session must be rejected with an error"
    );

    daemon.shutdown().await;
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn b02_invalid_params_rejected_deterministically() {
    let root = temp_root("b02-invalid-params");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let (recipe, _gates) = gated_recipe();
    let manager = build_manager(&root, &ws, recipe, Some(TOKEN), PolicyMode::AllowWrite);
    let daemon = start_multi(manager).await;

    let mut stream = tokio::net::TcpStream::connect(daemon.addr).await.unwrap();
    let (r, mut w) = stream.split();
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let mut reader = BufReader::new(r);

    // Auth
    w.write_all(format!("{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"auth\",\"params\":{{\"token\":\"{}\"}}}}\n", TOKEN).as_bytes()).await.unwrap();
    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();

    // Send array params
    let raw_arr = r#"{"jsonrpc":"2.0","id":99,"method":"session.status","params":[1,2,3]}"#;
    w.write_all(format!("{}\n", raw_arr).as_bytes()).await.unwrap();
    line.clear();
    reader.read_line(&mut line).await.unwrap();
    let resp: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(
        resp["error"]["code"].as_i64(),
        Some(-32602),
        "array params should be rejected with invalid params (-32602)"
    );

    daemon.shutdown().await;
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn b02_auth_three_failures_fail_closes_connection() {
    let root = temp_root("b02-auth-failures");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let (recipe, _gates) = gated_recipe();
    let manager = build_manager(&root, &ws, recipe, Some(TOKEN), PolicyMode::AllowWrite);
    let daemon = start_multi(manager).await;

    let mut stream = tokio::net::TcpStream::connect(daemon.addr).await.unwrap();
    let (r, mut w) = stream.split();
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let mut reader = BufReader::new(r);

    // Send first 2 invalid auth tokens
    for i in 1..=2 {
        let req = format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":{},\"method\":\"session.status\",\"params\":{{\"auth_token\":\"wrong-{}\"}}}}\n",
            i, i
        );
        w.write_all(req.as_bytes()).await.unwrap();
        let mut line = String::new();
        let n = reader.read_line(&mut line).await.unwrap();
        assert!(n > 0, "server should respond to attempt {}", i);
        let resp: Value = serde_json::from_str(&line).unwrap();
        assert!(resp.get("error").is_some());
    }

    // 3rd failure triggers immediate fail-closed termination
    let req3 = "{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"session.status\",\"params\":{\"auth_token\":\"wrong-3\"}}\n";
    w.write_all(req3.as_bytes()).await.unwrap();

    // Read remaining bytes until stream terminates: must reach EOF promptly
    let mut total_lines = 0;
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).await.unwrap();
        if n == 0 {
            break;
        }
        total_lines += 1;
    }
    assert!(total_lines <= 1, "connection terminated on 3rd failure");

    daemon.shutdown().await;
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn b02_active_run_connection_is_not_killed_by_idle_check() {
    let root = temp_root("b02-active-run-idle");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let (recipe, gates) = gated_recipe();
    let gate = gate_for(&gates, &ws);
    let manager = build_manager(&root, &ws, recipe, Some(TOKEN), PolicyMode::AllowWrite);
    let daemon = start_multi(manager).await;

    let mut client = Client::connect(daemon.addr).await;
    client.authenticate(TOKEN).await;

    // Start a prompt that is gated (stays active in flight)
    let prompt_id = client
        .send(
            "session.prompt",
            json!({ "prompt": "slow" }),
        )
        .await;
    gate.wait_entered("active run started").await;

    // While task is active in flight, release gate
    gate.release();

    // The response must arrive cleanly
    let resp = client.recv_response(prompt_id, "active run response").await;
    assert!(resp.get("error").is_none());
    assert_ne!(resp["result"]["status"], json!("cancelled"));

    daemon.shutdown().await;
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn b02_half_frame_preserved_across_run_join_cancellation() {
    let root = temp_root("b02-half-frame");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let (recipe, gates) = gated_recipe();
    let gate = gate_for(&gates, &ws);
    let manager = build_manager(&root, &ws, recipe, Some(TOKEN), PolicyMode::AllowWrite);

    let (client_io, server_io) = tokio::io::duplex(4096);
    let (s_read, s_write) = tokio::io::split(server_io);
    let (c_read, mut c_write) = tokio::io::split(client_io);

    let mgr = manager.clone();
    let server_task = tokio::spawn(async move {
        mgr.serve_stream(s_read, s_write).await
    });

    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let mut client_lines = BufReader::new(c_read).lines();

    // 1. Authenticate client
    c_write
        .write_all(format!("{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"session.status\",\"params\":{{\"auth_token\":\"{}\"}}}}\n", TOKEN).as_bytes())
        .await
        .unwrap();
    c_write.flush().await.unwrap();
    let auth_resp = client_lines.next_line().await.unwrap().unwrap();
    let parsed: Value = serde_json::from_str(&auth_resp).unwrap();
    assert!(parsed.get("result").is_some());

    // 2. Start a prompt that enters the gate (so it runs asynchronously in background)
    c_write
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":10,\"method\":\"session.prompt\",\"params\":{\"prompt\":\"slow\"}}\n")
        .await
        .unwrap();
    c_write.flush().await.unwrap();
    gate.wait_entered("prompt started and entered gate").await;

    // 3. While prompt is running in background, send HALF of the next request (no newline!)
    let half_req = b"{\"jsonrpc\":\"2.0\",\"id\":20,\"method\":\"session.";
    c_write.write_all(half_req).await.unwrap();
    c_write.flush().await.unwrap();

    // Give server opportunity to read into its buffer
    tokio::task::yield_now().await;

    // 4. Release the gate so the background prompt finishes and joins
    gate.release();

    async fn read_until_id<R: AsyncBufReadExt + Unpin>(lines: &mut tokio::io::Lines<R>, id: i64) -> Value {
        loop {
            let line = lines.next_line().await.unwrap().expect("stream closed unexpectedly");
            let v: Value = serde_json::from_str(&line).unwrap();
            if v.get("id").and_then(Value::as_i64) == Some(id) {
                return v;
            }
            if v.get("id").is_none() && v.get("error").is_some() {
                panic!("received unexpected JSON-RPC parse error indicating prefix frame loss: {}", v);
            }
        }
    }

    // Read prompt completion response (id=10)
    let prompt_resp = read_until_id(&mut client_lines, 10).await;
    assert_eq!(prompt_resp["id"].as_i64(), Some(10));

    // 5. Send the second half of the request with newline
    let second_half = b"status\",\"params\":{}}\n";
    c_write.write_all(second_half).await.unwrap();
    c_write.flush().await.unwrap();

    // 6. Read the response for request id=20: must NOT be an invalid JSON-RPC error!
    let next_resp = read_until_id(&mut client_lines, 20).await;
    assert_eq!(
        next_resp["id"].as_i64(),
        Some(20),
        "request id 20 must be parsed completely after run join cancellation; got: {}",
        next_resp
    );
    assert!(
        next_resp.get("result").is_some(),
        "half-frame was corrupted or truncated: {}",
        next_resp
    );

    drop(c_write);
    drop(client_lines);
    let _ = server_task.await;
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn b02_auth_absolute_deadline_enforces_timeout_even_with_slow_partial_frames() {
    let root = temp_root("b02-auth-deadline-slow");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let (recipe, _gates) = gated_recipe();
    let manager = build_manager(&root, &ws, recipe, Some(TOKEN), PolicyMode::AllowWrite);

    let (client_io, server_io) = tokio::io::duplex(4096);
    let (s_read, s_write) = tokio::io::split(server_io);
    let (c_read, mut c_write) = tokio::io::split(client_io);

    use kroma_core::surface::TransportConfig;
    let config = TransportConfig {
        auth_handshake_timeout: Duration::from_millis(80),
        ..Default::default()
    };

    let mgr = manager.clone();
    let server_task = tokio::spawn(async move {
        mgr.serve_stream_with_config(s_read, s_write, config).await
    });

    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let mut client_lines = BufReader::new(c_read).lines();

    // Send partial frame without completing it
    c_write.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1").await.unwrap();
    c_write.flush().await.unwrap();

    // Sleep past auth_handshake_timeout (80ms)
    tokio::time::sleep(Duration::from_millis(120)).await;

    // Server must send UNAUTHORIZED terminal error response and then close the connection
    let first = client_lines
        .next_line()
        .await
        .expect("read timeout")
        .expect("must receive terminal auth error response");
    let resp: Value = serde_json::from_str(&first).expect("valid JSON error response");
    assert_eq!(resp["error"]["code"].as_i64(), Some(-32001), "expected JSONRPC_UNAUTHORIZED");
    assert!(
        resp["error"]["message"].as_str().unwrap().contains("authentication handshake timed out"),
        "error message must mention auth handshake timeout: {}",
        resp
    );

    let second = client_lines.next_line().await.expect("read timeout");
    assert!(second.is_none(), "stream must be closed after terminal auth error");

    let _ = server_task.await;
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn b02_idle_timeout_terminates_quiet_connection_without_active_run() {
    let root = temp_root("b02-idle-timeout-quiet");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let (recipe, _gates) = gated_recipe();
    let manager = build_manager(&root, &ws, recipe, Some(TOKEN), PolicyMode::AllowWrite);

    // Client stays quiet past idle_timeout (80ms)
    let (client_io, server_io) = tokio::io::duplex(4096);
    let (s_read, s_write) = tokio::io::split(server_io);
    let (c_read, mut c_write) = tokio::io::split(client_io);

    use kroma_core::surface::TransportConfig;
    let config = TransportConfig {
        idle_timeout: Duration::from_millis(80),
        ..Default::default()
    };

    let mgr = manager.clone();
    let server_task = tokio::spawn(async move {
        mgr.serve_stream_with_config(s_read, s_write, config).await
    });

    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let mut client_lines = BufReader::new(c_read).lines();

    // Authenticate client
    c_write
        .write_all(format!("{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"session.status\",\"params\":{{\"auth_token\":\"{}\"}}}}\n", TOKEN).as_bytes())
        .await
        .unwrap();
    c_write.flush().await.unwrap();
    let auth_resp = client_lines.next_line().await.unwrap().unwrap();
    assert!(auth_resp.contains("result"));

    // Client stays quiet past idle_timeout (80ms)
    tokio::time::sleep(Duration::from_millis(120)).await;

    // Server must terminate idle connection
    let eof = client_lines.next_line().await.unwrap();
    assert!(eof.is_none(), "idle connection without active run must be terminated on idle timeout");

    let _ = server_task.await;
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn b02_idle_timeout_immune_during_active_run_with_injected_short_timeout() {
    let root = temp_root("b02-idle-immune-active");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let (recipe, gates) = gated_recipe();
    let gate = gate_for(&gates, &ws);
    let manager = build_manager(&root, &ws, recipe, Some(TOKEN), PolicyMode::AllowWrite);

    let (client_io, server_io) = tokio::io::duplex(4096);
    let (s_read, s_write) = tokio::io::split(server_io);
    let (c_read, mut c_write) = tokio::io::split(client_io);

    use kroma_core::surface::TransportConfig;
    let config = TransportConfig {
        idle_timeout: Duration::from_millis(80),
        ..Default::default()
    };

    let mgr = manager.clone();
    let server_task = tokio::spawn(async move {
        mgr.serve_stream_with_config(s_read, s_write, config).await
    });

    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let mut client_lines = BufReader::new(c_read).lines();

    // Authenticate
    c_write
        .write_all(format!("{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"session.status\",\"params\":{{\"auth_token\":\"{}\"}}}}\n", TOKEN).as_bytes())
        .await
        .unwrap();
    c_write.flush().await.unwrap();
    let _auth_resp = client_lines.next_line().await.unwrap().unwrap();

    // Start gated prompt
    c_write
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":10,\"method\":\"session.prompt\",\"params\":{\"prompt\":\"slow\"}}\n")
        .await
        .unwrap();
    c_write.flush().await.unwrap();
    gate.wait_entered("active run entered gate").await;

    // Hold run active in gate for 140ms (> 80ms idle timeout!)
    tokio::time::sleep(Duration::from_millis(140)).await;

    // Release gate
    gate.release();

    // Connection must NOT have been killed by idle timeout; response arrives cleanly
    loop {
        let line = client_lines.next_line().await.unwrap().expect("stream must not be closed during active run");
        let v: Value = serde_json::from_str(&line).unwrap();
        if v.get("id").and_then(Value::as_i64) == Some(10) {
            assert!(v.get("result").is_some());
            assert_ne!(v["result"]["status"], json!("cancelled"));
            break;
        }
    }

    drop(c_write);
    drop(client_lines);
    let _ = server_task.await;
    let _ = std::fs::remove_dir_all(root);
}

// ---------------------------------------------------------------------------------------------
// B11: observer cancel control boundary. A `session.cancel` that carries a daemon connection
// context is admitted only from the connection that owns the *exact* current active run; every
// other authenticated connection is refused with the stable `not_owner` error and the owner's work
// (including a pending approval) is left untouched.
// ---------------------------------------------------------------------------------------------

/// B11: the stable protocol error a refused remote cancel must carry (`JSONRPC_SESSION_NOT_OWNED`).
const NOT_OWNER: i64 = -32006;
/// The auth gate's terminal error for a cancel from a connection that never authenticated.
const UNAUTHORIZED: i64 = -32001;

fn assert_not_owner(resp: &Value, stage: &str) {
    assert_eq!(
        resp["error"]["code"],
        json!(NOT_OWNER),
        "{stage}: expected the stable not_owner error, got: {resp}"
    );
    let message = resp["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.starts_with("not_owner"),
        "{stage}: the refusal must be stable and self-describing, got: {resp}"
    );
}

/// B11: asks for one mutating tool call, so an `Ask` policy raises a real approval before the run
/// can complete.
struct ApprovalProvider;

impl ModelProvider for ApprovalProvider {
    fn complete<'a>(
        &'a self,
        req: ProviderRequest,
        _budget: &'a BudgetConfig,
        _cancel_rx: watch::Receiver<bool>,
    ) -> Pin<Box<dyn Future<Output = Result<ProviderCompletion, ProviderError>> + Send + 'a>> {
        Box::pin(async move {
            if req
                .messages
                .iter()
                .any(|m| matches!(m, TranscriptItem::ToolResult(_)))
            {
                Ok(completion())
            } else {
                Ok(ProviderCompletion {
                    turn: AssistantTurn {
                        content: None,
                        reasoning_blocks: vec![],
                        tool_calls: vec![ToolCall {
                            call_id: ToolCallId::new("call_b11_write"),
                            name: "write".to_string(),
                            raw_arguments: json!({ "path": "b11_approved.txt", "content": "ok" })
                                .to_string(),
                        }],
                        finish_reason: FinishReason::ToolCalls,
                        usage: None,
                    },
                    request_body_bytes: 0,
                    latency_ms: 0,
                    ttft_ms: None,
                    attempts_used: 1,
                reported_model: None,
                })
            }
        })
    }
}

#[tokio::test]
async fn observer_cancel_is_refused_and_the_owners_approval_survives() {
    let root = temp_root("b11-observer-approval");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let cfg = KromaRuntimeConfig::new(&ws, root.join("state"), PolicyMode::Ask)
        .with_surface("serve", "tcp")
        .with_auth_token(Some(TOKEN.to_string()));
    let dispatcher = RpcDispatcher::new(cfg, Arc::new(ApprovalProvider)).unwrap();
    let running = start_single(dispatcher).await;

    let mut owner = Client::connect(running.addr).await;
    owner.authenticate(TOKEN).await;
    let prompt_id = owner
        .send("session.prompt", json!({ "prompt": "write something" }))
        .await;

    // The owner's run is blocked on a real approval.
    let request_id = loop {
        let frame = owner.recv().await;
        if frame.get("id").and_then(Value::as_i64) == Some(prompt_id) {
            panic!("owner run finished before the approval was raised: {frame}");
        }
        if frame.get("method").and_then(Value::as_str) == Some("approval/requested") {
            break frame["params"]["request_id"]
                .as_str()
                .expect("approval request carries an id")
                .to_string();
        }
    };

    // An authenticated observer tries to cancel the owner's run.
    let mut observer = Client::connect(running.addr).await;
    observer.authenticate(TOKEN).await;
    let cancel_id = observer.send("session.cancel", json!({})).await;
    let cancel_resp = observer
        .recv_response(cancel_id, "observer session.cancel")
        .await;
    assert_not_owner(&cancel_resp, "observer session.cancel");

    // The refusal changed nothing: the owner's pending approval is still answerable.
    let approve_id = owner
        .send(
            "approval.respond",
            json!({ "request_id": request_id, "decision": "approve_once", "auth_token": TOKEN }),
        )
        .await;
    let approve_resp = owner.recv_response(approve_id, "approval.respond").await;
    assert!(
        approve_resp.get("error").is_none(),
        "observer cancel cleared the owner's pending approval: {approve_resp}"
    );

    let resp = owner.recv_response(prompt_id, "owner prompt response").await;
    assert!(resp.get("error").is_none(), "owner run failed: {resp}");
    assert_eq!(
        resp["result"]["status"],
        json!("completed"),
        "observer cancel stopped the owner's run: {resp}"
    );
    assert!(
        ws.join("b11_approved.txt").exists(),
        "the approved write did not run"
    );

    running.shutdown().await;
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn observer_cancel_is_refused_for_every_request_alias() {
    let root = temp_root("b11-alias");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let (recipe, gates) = gated_recipe();
    let gate = gate_for(&gates, &ws);
    let manager = build_manager(&root, &ws, recipe, Some(TOKEN), PolicyMode::AllowWrite);
    let session = manager.default_session_id().unwrap();
    let running = start_multi(manager).await;

    let mut owner = Client::connect(running.addr).await;
    owner.authenticate(TOKEN).await;
    let prompt_id = owner
        .send(
            "session.prompt",
            json!({ "prompt": "slow", "session_id": session }),
        )
        .await;
    gate.wait_entered("owner run start").await;

    // The observer is authenticated but owns nothing; every spelling must be refused.
    let mut observer = Client::connect(running.addr).await;
    observer.authenticate(TOKEN).await;
    for method in ["session.cancel", "session/cancel", "acp.cancel"] {
        let id = observer.send(method, json!({ "session_id": session })).await;
        let resp = observer.recv_response(id, method).await;
        assert_not_owner(&resp, method);
    }

    gate.release();
    let resp = owner.recv_response(prompt_id, "owner prompt response").await;
    assert!(resp.get("error").is_none(), "owner run failed: {resp}");
    assert_ne!(
        resp["result"]["status"],
        json!("cancelled"),
        "a refused observer cancel still stopped the owner's run: {resp}"
    );

    running.shutdown().await;
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn owner_cancel_reaches_its_own_run() {
    let root = temp_root("b11-owner-cancel");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let gate = Arc::new(Gate::new());
    let cfg = KromaRuntimeConfig::new(&ws, root.join("state"), PolicyMode::AllowWrite)
        .with_surface("serve", "tcp")
        .with_auth_token(Some(TOKEN.to_string()));
    let dispatcher = RpcDispatcher::new(
        cfg,
        Arc::new(GatedProvider {
            gate: Arc::clone(&gate),
        }),
    )
    .unwrap();
    let running = start_single(dispatcher).await;

    let mut owner = Client::connect(running.addr).await;
    owner.authenticate(TOKEN).await;
    let prompt_id = owner
        .send("session.prompt", json!({ "prompt": "slow" }))
        .await;
    gate.wait_entered("owner run start").await;

    let cancel_id = owner.send("session.cancel", json!({})).await;
    let cancel_resp = owner
        .recv_response(cancel_id, "owner session.cancel")
        .await;
    assert!(cancel_resp.get("error").is_none(), "{cancel_resp}");
    assert_eq!(
        cancel_resp["result"]["cancelled"],
        json!(true),
        "the owner's own cancel must be admitted: {cancel_resp}"
    );
    assert_eq!(cancel_resp["result"]["was_in_flight"], json!(true), "{cancel_resp}");

    let resp = owner.recv_response(prompt_id, "owner prompt response").await;
    assert_eq!(
        resp["result"]["status"],
        json!("cancelled"),
        "the owner's cancel did not reach its own run: {resp}"
    );

    running.shutdown().await;
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn previous_owner_cannot_cancel_a_newer_connections_run() {
    let root = temp_root("b11-stale-owner");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let (recipe, gates) = gated_recipe();
    let gate = gate_for(&gates, &ws);
    let manager = build_manager(&root, &ws, recipe, Some(TOKEN), PolicyMode::AllowWrite);
    let session = manager.default_session_id().unwrap();
    let running = start_multi(manager).await;

    // The first connection starts and cancels its own run.
    let mut first = Client::connect(running.addr).await;
    first.authenticate(TOKEN).await;
    let first_prompt = first
        .send(
            "session.prompt",
            json!({ "prompt": "slow", "session_id": session }),
        )
        .await;
    gate.wait_entered("first run start").await;
    let first_cancel = first
        .send("session.cancel", json!({ "session_id": session }))
        .await;
    let first_cancel_resp = first.recv_response(first_cancel, "first cancel").await;
    assert!(
        first_cancel_resp.get("error").is_none(),
        "{first_cancel_resp}"
    );
    let first_done = first
        .recv_response(first_prompt, "first prompt response")
        .await;
    assert_eq!(
        first_done["result"]["status"],
        json!("cancelled"),
        "{first_done}"
    );

    // A second connection starts a fresh run on the same session.
    let mut second = Client::connect(running.addr).await;
    second.authenticate(TOKEN).await;
    let second_prompt = second
        .send(
            "session.prompt",
            json!({ "prompt": "slow", "session_id": session }),
        )
        .await;
    gate.wait_entered("second run start").await;

    // The previous owner can no longer stop anything.
    let stale_cancel = first
        .send("session.cancel", json!({ "session_id": session }))
        .await;
    let stale_resp = first.recv_response(stale_cancel, "stale owner cancel").await;
    assert_not_owner(&stale_resp, "stale owner cancel");

    gate.release();
    let second_done = second
        .recv_response(second_prompt, "second prompt response")
        .await;
    assert!(second_done.get("error").is_none(), "{second_done}");
    assert_eq!(
        second_done["result"]["status"],
        json!("completed"),
        "the previous owner stopped a newer connection's run: {second_done}"
    );

    running.shutdown().await;
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn acp_observer_cancel_notification_does_not_stop_the_owner_run() {
    let root = temp_root("b11-acp-notify");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let gate = Arc::new(Gate::new());
    let cfg = KromaRuntimeConfig::new(&ws, root.join("state"), PolicyMode::AllowWrite)
        .with_surface("serve", "tcp")
        .with_acp_protocol(true)
        .with_auth_token(Some(TOKEN.to_string()));
    let dispatcher = RpcDispatcher::new(
        cfg,
        Arc::new(GatedProvider {
            gate: Arc::clone(&gate),
        }),
    )
    .unwrap();
    let running = start_single(dispatcher).await;

    let mut owner = Client::connect(running.addr).await;
    owner.authenticate(TOKEN).await;
    let status = owner.request("session.status", json!({})).await;
    let session = status["result"]["session_id"]
        .as_str()
        .expect("session id")
        .to_string();

    let prompt_id = owner
        .send("session.prompt", json!({ "prompt": "slow" }))
        .await;
    gate.wait_entered("owner run start").await;

    // ACP `session/cancel` is a notification (no id): a non-owner's is ignored, never answered.
    let mut observer = Client::connect(running.addr).await;
    observer.authenticate(TOKEN).await;
    let notif = json!({
        "jsonrpc": "2.0",
        "method": "session/cancel",
        "params": { "sessionId": session }
    });
    observer.send_raw(&format!("{notif}\n")).await;
    // Barrier: this request is only answered after the notification ahead of it was processed.
    let probe = observer.request("session.status", json!({})).await;
    assert!(probe.get("error").is_none(), "observer probe failed: {probe}");

    gate.release();
    let resp = owner.recv_response(prompt_id, "owner prompt response").await;
    assert!(resp.get("error").is_none(), "owner run failed: {resp}");
    assert_ne!(
        resp["result"]["status"],
        json!("cancelled"),
        "an ACP observer notification cancelled the owner's run: {resp}"
    );

    running.shutdown().await;
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn unauthenticated_cancel_is_refused_and_the_owner_run_survives() {
    let root = temp_root("b11-unauth-cancel");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let gate = Arc::new(Gate::new());
    let cfg = KromaRuntimeConfig::new(&ws, root.join("state"), PolicyMode::AllowWrite)
        .with_surface("serve", "tcp")
        .with_auth_token(Some(TOKEN.to_string()));
    let dispatcher = RpcDispatcher::new(
        cfg,
        Arc::new(GatedProvider {
            gate: Arc::clone(&gate),
        }),
    )
    .unwrap();
    let running = start_single(dispatcher).await;

    let mut owner = Client::connect(running.addr).await;
    owner.authenticate(TOKEN).await;
    let prompt_id = owner
        .send("session.prompt", json!({ "prompt": "slow" }))
        .await;
    gate.wait_entered("owner run start").await;

    // A connection that never authenticated is stopped by the auth gate before any cancel logic.
    let mut stranger = Client::connect(running.addr).await;
    let cancel_id = stranger.send("session.cancel", json!({})).await;
    let cancel_resp = stranger
        .recv_response(cancel_id, "stranger session.cancel")
        .await;
    assert_eq!(
        cancel_resp["error"]["code"],
        json!(UNAUTHORIZED),
        "an unauthenticated cancel must be refused by the auth gate: {cancel_resp}"
    );

    gate.release();
    let resp = owner.recv_response(prompt_id, "owner prompt response").await;
    assert!(resp.get("error").is_none(), "owner run failed: {resp}");
    assert_ne!(
        resp["result"]["status"],
        json!("cancelled"),
        "an unauthenticated cancel stopped the owner's run: {resp}"
    );

    running.shutdown().await;
    let _ = std::fs::remove_dir_all(root);
}

// ---------------------------------------------------------------------------------------------
// B12: remote `tasks.control` permission.
// ---------------------------------------------------------------------------------------------

/// B12: durable task control over the network is refused — there is no persistent authority that
/// can safely claim a task, so every mutating `tasks.control` action is rejected on the remote
/// stream while read-only paths and the trusted in-process control entry keep working.
#[tokio::test]
async fn remote_task_control_mutations_are_refused_but_reads_and_local_control_work() {
    let root = temp_root("task-control");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let state_dir = root.join("state");
    // Seed a real durable task so the record can be asserted untouched.
    let registry_path = state_dir.join("tasks").join("registry.json");
    std::fs::create_dir_all(registry_path.parent().unwrap()).unwrap();
    std::fs::write(
        &registry_path,
        serde_json::to_string(&json!({
            "version": 1,
            "next_notification_seq": 1,
            "tasks": [{
                "task_id": "task-1",
                "session_id": "sess-1",
                "owner_id": "owner-1",
                "workspace_root": ws.display().to_string(),
                "role": "scout",
                "state": "suspended",
                "created_turn": 0,
                "updated_turn": 1,
                "token_allocation": 1000,
                "token_spent": 0,
                "artifacts": [],
                "notifications": []
            }, {
                "task_id": "task-2",
                "session_id": "sess-1",
                "owner_id": "owner-1",
                "workspace_root": ws.display().to_string(),
                "role": "worker",
                "state": "completed",
                "created_turn": 0,
                "updated_turn": 2,
                "token_allocation": 1000,
                "token_spent": 7,
                "artifacts": [],
                "notifications": [],
                "verification": { "gate": "post-merge", "passed": true, "revision": "rev-1" }
            }]
        }))
        .unwrap(),
    )
    .unwrap();

    let cfg = KromaRuntimeConfig::new(&ws, state_dir.clone(), PolicyMode::AllowWrite)
        .with_auth_token(Some(TASK_CONTROL_TOKEN.to_string()));
    let dispatcher = RpcDispatcher::new(cfg, Arc::new(FakeProvider::new(vec![stop()]))).unwrap();
    let local = dispatcher.clone();
    let running = start_single(dispatcher).await;

    // An authenticated *remote* connection must be refused for every mutating action.
    let mut client = Client::connect(running.addr).await;
    client.authenticate(TASK_CONTROL_TOKEN).await;
    let mutating: [(&str, Value); 8] = [
        ("pause", json!({ "action": "pause", "task_id": "task-1" })),
        ("resume", json!({ "action": "resume", "task_id": "task-1" })),
        ("cancel", json!({ "action": "cancel", "task_id": "task-1" })),
        (
            "challenge",
            json!({ "action": "challenge", "task_id": "task-1", "reason": "no" }),
        ),
        (
            "resolve_challenge",
            json!({ "action": "resolve_challenge", "task_id": "task-1", "accept": true }),
        ),
        (
            "acknowledge",
            json!({ "action": "acknowledge", "task_id": "task-1" }),
        ),
        (
            "accept",
            json!({ "action": "accept", "task_id": "task-1" }),
        ),
        (
            "reconcile",
            json!({ "action": "reconcile", "task_id": "task-1",
                    "decision": { "kind": "acknowledged_unknown" }, "reason": "test" }),
        ),
    ];
    for (label, params) in mutating {
        let resp = client.request("tasks.control", params).await;
        let err = resp
            .get("error")
            .unwrap_or_else(|| panic!("remote '{label}' must be refused, got: {resp}"));
        assert_eq!(err["code"], json!(-32006), "remote '{label}': {resp}");
    }

    // Read-only paths are not harmed.
    let list = client
        .request("tasks.list", json!({ "session_id": "sess-1" }))
        .await;
    assert_eq!(
        list["result"]["count"],
        json!(2),
        "tasks.list must still work: {list}"
    );
    let list2 = client
        .request(
            "tasks.control",
            json!({ "action": "list", "session_id": "sess-1" }),
        )
        .await;
    assert_eq!(
        list2["result"]["count"],
        json!(2),
        "tasks.control list must still work: {list2}"
    );

    // The durable record is untouched by the refused mutations.
    let after = std::fs::read_to_string(&registry_path).unwrap();
    assert!(
        after.contains("\"suspended\""),
        "the record must be unchanged: {after}"
    );
    assert!(
        !after.contains("\"cancelled\""),
        "the record must not have been cancelled: {after}"
    );

    // An unauthenticated connection cannot mutate either.
    let mut anon = Client::connect(running.addr).await;
    let resp = anon
        .request(
            "tasks.control",
            json!({ "action": "cancel", "task_id": "task-1" }),
        )
        .await;
    assert!(
        resp.get("error").is_some(),
        "an unauthenticated control must be refused: {resp}"
    );

    // The trusted in-process control entry still mutates.
    let local_resp = local
        .handle_request(JsonRpcRequest::new(
            99,
            "tasks.control",
            json!({ "action": "cancel", "task_id": "task-1" }),
        ))
        .await;
    let local_val = serde_json::to_value(&local_resp).unwrap();
    assert!(
        local_val.get("error").is_none(),
        "local control must still work: {local_val}"
    );

    // The trusted in-process entry can mint a real acceptance for a completed, verified record.
    let accept_resp = local
        .handle_request(JsonRpcRequest::new(
            100,
            "tasks.control",
            json!({ "action": "accept", "task_id": "task-2", "evidence_revision": "rev-1" }),
        ))
        .await;
    let accept_val = serde_json::to_value(&accept_resp).unwrap();
    assert!(
        accept_val.get("error").is_none(),
        "local accept must still work: {accept_val}"
    );
    assert_eq!(accept_val["result"]["accepted"], json!(true));
    assert_eq!(accept_val["result"]["accepted_by"], json!("local-host"));
    assert_eq!(accept_val["result"]["evidence_revision"], json!("rev-1"));

    running.shutdown().await;
    let _ = std::fs::remove_dir_all(root);
}

/// B12: the multi-session daemon stream refuses every mutating `tasks.control` action too (the
/// same shared routing), while read-only control still works.
#[tokio::test]
async fn multi_session_remote_task_control_is_refused() {
    let root = temp_root("task-control-multi");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let (recipe, _gates) = gated_recipe();
    let manager = build_manager(
        &root,
        &ws,
        recipe,
        Some(TASK_CONTROL_TOKEN),
        PolicyMode::AllowWrite,
    );
    let running = start_multi(manager).await;

    let mut client = Client::connect(running.addr).await;
    client.authenticate(TASK_CONTROL_TOKEN).await;

    let mutating: [(&str, Value); 8] = [
        ("pause", json!({ "action": "pause", "task_id": "task-1" })),
        ("resume", json!({ "action": "resume", "task_id": "task-1" })),
        ("cancel", json!({ "action": "cancel", "task_id": "task-1" })),
        (
            "challenge",
            json!({ "action": "challenge", "task_id": "task-1", "reason": "no" }),
        ),
        (
            "resolve_challenge",
            json!({ "action": "resolve_challenge", "task_id": "task-1", "accept": true }),
        ),
        (
            "acknowledge",
            json!({ "action": "acknowledge", "task_id": "task-1" }),
        ),
        (
            "accept",
            json!({ "action": "accept", "task_id": "task-1" }),
        ),
        (
            "reconcile",
            json!({ "action": "reconcile", "task_id": "task-1",
                    "decision": { "kind": "acknowledged_unknown" }, "reason": "test" }),
        ),
    ];
    for (label, params) in mutating {
        let resp = client.request("tasks.control", params).await;
        let err = resp
            .get("error")
            .unwrap_or_else(|| panic!("multi remote '{label}' must be refused, got: {resp}"));
        assert_eq!(err["code"], json!(-32006), "multi remote '{label}': {resp}");
    }
    let list = client.request("tasks.list", json!({})).await;
    assert!(
        list.get("error").is_none(),
        "multi tasks.list must still work: {list}"
    );

    running.shutdown().await;
    let _ = std::fs::remove_dir_all(root);
}
