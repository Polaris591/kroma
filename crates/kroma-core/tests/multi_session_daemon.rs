//! Bounded multi-session daemon control plane (P1-4).
//!
//! A `kroma serve` daemon hosts several independent runtimes and routes each request to the runtime
//! that owns the session named in the request. These tests pin the three behaviours the review
//! required: two sessions never cross state, a cancel only affects its target session, and a daemon
//! restart recovers the sessions it was hosting.

use kroma_core::budget::BudgetConfig;
use kroma_core::messages::{AssistantTurn, FinishReason, ToolCall};
use kroma_core::provider::{
    FakeProvider, ModelProvider, ProviderCompletion, ProviderError, ProviderRequest,
};
use kroma_core::surface::{
    KromaRuntimeConfig, RpcDispatcher, SessionIndexEntry, SessionManager, SessionManagerConfig,
    SessionRuntimeFactory,
};
use kroma_core::tool::policy::PolicyMode;
use kroma_core::types::ToolCallId;
use serde_json::{json, Value};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;

fn stop() -> AssistantTurn {
    AssistantTurn {
        content: Some("done".into()),
        reasoning_blocks: vec![],
        tool_calls: vec![],
        finish_reason: FinishReason::Stop,
        usage: None,
    }
}

fn write_marker(marker: &str) -> AssistantTurn {
    AssistantTurn {
        content: None,
        reasoning_blocks: vec![],
        tool_calls: vec![ToolCall {
            call_id: ToolCallId::new("call_1"),
            name: "write".into(),
            raw_arguments: json!({ "path": "marker.txt", "content": marker }).to_string(),
        }],
        finish_reason: FinishReason::ToolCalls,
        usage: None,
    }
}

fn temp_root(label: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "kroma-multi-session-{}-{}",
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

/// Recipe that produces one provider for a workspace.
type ProviderRecipe = Arc<dyn Fn(&Path) -> Arc<dyn ModelProvider> + Send + Sync>;

/// Builds a runtime per session with a caller-supplied provider recipe.
struct FnFactory {
    base: PathBuf,
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
            PolicyMode::AllowWrite,
        );
        cfg.resume_session_path = resume_session_path.map(Path::to_path_buf);
        RpcDispatcher::new(cfg, (self.provider)(workspace_root))
    }
}

async fn rpc(manager: &SessionManager, method: &str, params: Value) -> Value {
    let req = serde_json::from_value(json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": method,
        "params": params,
    }))
    .unwrap();
    serde_json::to_value(manager.handle_request(req).await).unwrap()
}

fn index_path(base: &Path) -> PathBuf {
    base.join("state").join("daemon-sessions.json")
}

#[tokio::test]
async fn two_sessions_in_one_daemon_do_not_cross_state() {
    let root = temp_root("cross-state");
    let ws_a = root.join("ws-a");
    let ws_b = root.join("ws-b");
    std::fs::create_dir_all(&ws_a).unwrap();
    std::fs::create_dir_all(&ws_b).unwrap();

    let factory = Arc::new(FnFactory {
        base: root.clone(),
        provider: Arc::new(|workspace: &Path| {
            let label = workspace
                .file_name()
                .map(|n| n.to_string_lossy().trim_start_matches("ws-").to_string())
                .unwrap_or_else(|| "x".to_string());
            Arc::new(FakeProvider::new(vec![write_marker(&label), stop()]))
                as Arc<dyn ModelProvider>
        }),
    });
    let primary = factory.build(&ws_a, None).unwrap();
    let manager = SessionManager::open(
        SessionManagerConfig {
            factory: factory.clone(),
            max_sessions: 4,
            index_path: index_path(&root),
            default_workspace: ws_a.clone(),
        },
        primary,
    )
    .unwrap();

    let session_a = manager.default_session_id().unwrap();
    let entry_b = manager
        .open_session(Some(&ws_b), None)
        .expect("a second session opens");
    assert_eq!(manager.len(), 2, "the daemon holds two runtimes");
    assert_ne!(session_a, entry_b.session_id);

    // Prompt session A (by id) into its own workspace.
    let a = rpc(
        &manager,
        "session.prompt",
        json!({ "prompt": "write the A marker", "session_id": session_a }),
    )
    .await;
    assert!(a.get("error").is_none(), "session A prompt failed: {}", a);

    // Prompt session B (by id) into the other workspace.
    let b = rpc(
        &manager,
        "session.prompt",
        json!({ "prompt": "write the B marker", "session_id": entry_b.session_id }),
    )
    .await;
    assert!(b.get("error").is_none(), "session B prompt failed: {}", b);

    assert_eq!(
        std::fs::read_to_string(ws_a.join("marker.txt")).unwrap(),
        "a",
        "session A only touched its own workspace"
    );
    assert_eq!(
        std::fs::read_to_string(ws_b.join("marker.txt")).unwrap(),
        "b",
        "session B only touched its own workspace"
    );
    assert!(
        !ws_a.join("marker.txt").metadata().unwrap().is_dir()
            && !ws_b.join("marker.txt").metadata().unwrap().is_dir()
    );

    // Each session reports its own turn count; state did not bleed between runtimes.
    let status_a = rpc(
        &manager,
        "session.status",
        json!({ "session_id": session_a }),
    )
    .await;
    let status_b = rpc(
        &manager,
        "session.status",
        json!({ "session_id": entry_b.session_id }),
    )
    .await;
    assert!(status_a.get("error").is_none(), "{}", status_a);
    assert!(status_b.get("error").is_none(), "{}", status_b);
    assert_eq!(
        status_a["result"]["workspace_root"],
        json!(ws_a.display().to_string())
    );
    assert_eq!(
        status_b["result"]["workspace_root"],
        json!(ws_b.display().to_string())
    );

    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn a_request_for_an_unowned_session_is_refused() {
    let root = temp_root("unowned");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let factory = Arc::new(FnFactory {
        base: root.clone(),
        provider: Arc::new(|_workspace: &Path| {
            Arc::new(FakeProvider::new(vec![stop()])) as Arc<dyn ModelProvider>
        }),
    });
    let primary = factory.build(&ws, None).unwrap();
    let manager = SessionManager::open(
        SessionManagerConfig {
            factory: factory.clone(),
            max_sessions: 2,
            index_path: index_path(&root),
            default_workspace: ws.clone(),
        },
        primary,
    )
    .unwrap();

    let refused = rpc(
        &manager,
        "session.prompt",
        json!({ "prompt": "hijack", "session_id": "not-this-daemon" }),
    )
    .await;
    assert_eq!(
        refused["error"]["code"],
        json!(-32006),
        "a foreign session must be refused with session-not-owned: {}",
        refused
    );

    // Capacity is enforced: the second session fills the bound, a third is refused.
    let _ = manager.open_session(None, None).unwrap();
    let third = manager.open_session(None, None);
    assert!(third.is_err(), "the session bound must be enforced");

    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn parallel_cancels_affect_only_the_target_session() {
    let root = temp_root("cancel-isolation");
    let ws_a = root.join("ws-a");
    let ws_b = root.join("ws-b");
    std::fs::create_dir_all(&ws_a).unwrap();
    std::fs::create_dir_all(&ws_b).unwrap();

    // A provider that only returns after `delay`, so a run holds its session busy.
    struct SlowProvider {
        delay: Duration,
    }
    impl ModelProvider for SlowProvider {
        fn complete<'a>(
            &'a self,
            _req: ProviderRequest,
            _budget: &'a BudgetConfig,
            mut cancel_rx: watch::Receiver<bool>,
        ) -> Pin<Box<dyn Future<Output = Result<ProviderCompletion, ProviderError>> + Send + 'a>>
        {
            let delay = self.delay;
            Box::pin(async move {
                tokio::select! {
                    _ = tokio::time::sleep(delay) => Ok(ProviderCompletion {
                        turn: stop(),
                        request_body_bytes: 0,
                        latency_ms: delay.as_millis() as u64,
                        ttft_ms: None,
                        attempts_used: 1,
                reported_model: None,
                    }),
                    _ = cancel_rx.changed() => Err(ProviderError::Cancelled),
                }
            })
        }
    }
    let factory = Arc::new(FnFactory {
        base: root.clone(),
        provider: Arc::new(|_workspace: &Path| {
            Arc::new(SlowProvider {
                delay: Duration::from_millis(600),
            }) as Arc<dyn ModelProvider>
        }),
    });
    let primary = factory.build(&ws_a, None).unwrap();
    let manager = SessionManager::open(
        SessionManagerConfig {
            factory: factory.clone(),
            max_sessions: 4,
            index_path: index_path(&root),
            default_workspace: ws_a.clone(),
        },
        primary,
    )
    .unwrap();
    let session_a = manager.default_session_id().unwrap();
    let session_b = manager.open_session(Some(&ws_b), None).unwrap().session_id;

    let manager_a = manager.clone();
    let session_a_for_run = session_a.clone();
    let run_a = tokio::spawn(async move {
        rpc(
            &manager_a,
            "session.prompt",
            json!({ "prompt": "slow A", "session_id": session_a_for_run }),
        )
        .await
    });
    let manager_b = manager.clone();
    let session_b_for_run = session_b.clone();
    let run_b = tokio::spawn(async move {
        rpc(
            &manager_b,
            "session.prompt",
            json!({ "prompt": "slow B", "session_id": session_b_for_run }),
        )
        .await
    });

    // Let both prompts register their cancellation channel, then cancel only A.
    tokio::time::sleep(Duration::from_millis(150)).await;
    let cancelled = rpc(
        &manager,
        "session.cancel",
        json!({ "session_id": session_a }),
    )
    .await;
    assert!(
        cancelled.get("error").is_none(),
        "cancel failed: {}",
        cancelled
    );

    let result_a = run_a.await.unwrap();
    assert!(
        result_a.get("error").is_none(),
        "cancelled session A should still return a result: {}",
        result_a
    );
    assert_eq!(
        result_a["result"]["status"],
        json!("cancelled"),
        "cancelling A must cancel A: {}",
        result_a
    );

    let result_b = run_b.await.unwrap();
    assert!(
        result_b.get("error").is_none(),
        "session B failed: {}",
        result_b
    );
    assert_ne!(
        result_b["result"]["status"],
        json!("cancelled"),
        "cancelling A must not cancel B: {}",
        result_b
    );

    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn daemon_restart_recovers_hosted_sessions() {
    let root = temp_root("restart");
    let ws_a = root.join("ws-a");
    let ws_b = root.join("ws-b");
    std::fs::create_dir_all(&ws_a).unwrap();
    std::fs::create_dir_all(&ws_b).unwrap();
    let factory = Arc::new(FnFactory {
        base: root.clone(),
        provider: Arc::new(|_workspace: &Path| {
            Arc::new(FakeProvider::new(vec![write_marker("b"), stop()])) as Arc<dyn ModelProvider>
        }),
    });

    let primary = factory.build(&ws_a, None).unwrap();
    let manager = SessionManager::open(
        SessionManagerConfig {
            factory: factory.clone(),
            max_sessions: 4,
            index_path: index_path(&root),
            default_workspace: ws_a.clone(),
        },
        primary,
    )
    .unwrap();
    let session_b: SessionIndexEntry = manager.open_session(Some(&ws_b), None).unwrap();
    // Give session B a turn so its WAL has content to recover.
    let prompt = rpc(
        &manager,
        "session.prompt",
        json!({ "prompt": "write the B marker", "session_id": session_b.session_id }),
    )
    .await;
    assert!(prompt.get("error").is_none(), "{}", prompt);
    let sessions_before = manager.session_ids();
    drop(manager);

    // A new daemon process starts from the same index and must recover session B.
    let primary2 = factory.build(&ws_b, None).unwrap();
    let manager2 = SessionManager::open(
        SessionManagerConfig {
            factory: factory.clone(),
            max_sessions: 4,
            index_path: index_path(&root),
            default_workspace: ws_b.clone(),
        },
        primary2,
    )
    .unwrap();

    for session_id in &sessions_before {
        assert!(
            manager2.session_ids().contains(session_id),
            "session {} must be recovered after restart; recovered {:?}",
            session_id,
            manager2.session_ids()
        );
    }

    // The recovered session is usable: its history is intact.
    let status = rpc(
        &manager2,
        "session.status",
        json!({ "session_id": session_b.session_id }),
    )
    .await;
    assert!(status.get("error").is_none(), "{}", status);
    assert_eq!(
        status["result"]["workspace_root"],
        json!(ws_b.display().to_string())
    );
    assert!(
        status["result"]["turn_count"].as_u64().unwrap_or(0) >= 1,
        "the recovered session keeps its turn history: {}",
        status
    );

    let _ = std::fs::remove_dir_all(root);
}

/// End-to-end: the `kroma serve` daemon (via `KromaDaemon::bind_multi`) really routes a stream's
/// requests to the runtime that owns each named session, and refuses a foreign session id.
#[tokio::test]
async fn serve_daemon_routes_requests_by_session_id() {
    use kroma_core::surface::KromaDaemon;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::TcpStream;

    let root = temp_root("serve-route");
    let ws_a = root.join("ws-a");
    let ws_b = root.join("ws-b");
    std::fs::create_dir_all(&ws_a).unwrap();
    std::fs::create_dir_all(&ws_b).unwrap();
    let factory = Arc::new(FnFactory {
        base: root.clone(),
        provider: Arc::new(|workspace: &Path| {
            let label = workspace
                .file_name()
                .map(|n| n.to_string_lossy().trim_start_matches("ws-").to_string())
                .unwrap_or_else(|| "x".to_string());
            Arc::new(FakeProvider::new(vec![write_marker(&label), stop()]))
                as Arc<dyn ModelProvider>
        }),
    });
    let primary = factory.build(&ws_a, None).unwrap();
    let primary_session = primary.session_id();
    let manager = SessionManager::open(
        SessionManagerConfig {
            factory: factory.clone(),
            max_sessions: 4,
            index_path: index_path(&root),
            default_workspace: ws_a.clone(),
        },
        primary,
    )
    .unwrap();
    let daemon = KromaDaemon::bind_multi("127.0.0.1:0", manager)
        .await
        .unwrap();
    let addr = daemon.local_addr();
    let (cancel_tx, cancel_rx) = watch::channel(false);
    let serve_task = tokio::spawn(async move { daemon.serve(cancel_rx).await });

    let stream = TcpStream::connect(addr).await.unwrap();
    let (r, mut w) = stream.into_split();
    let mut lines = BufReader::new(r).lines();

    async fn tcp_rpc(
        w: &mut tokio::net::tcp::OwnedWriteHalf,
        lines: &mut tokio::io::Lines<BufReader<tokio::net::tcp::OwnedReadHalf>>,
        id: i64,
        method: &str,
        params: Value,
    ) -> Value {
        let req = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        w.write_all(format!("{}\n", req).as_bytes()).await.unwrap();
        w.flush().await.unwrap();
        loop {
            let line = lines.next_line().await.unwrap().expect("stream open");
            let v: Value = serde_json::from_str(&line).unwrap();
            if v.get("id").and_then(Value::as_i64) == Some(id) {
                return v;
            }
        }
    }

    // A new session in the other workspace; the daemon opens a second runtime.
    let opened = tcp_rpc(
        &mut w,
        &mut lines,
        1,
        "session.new",
        json!({ "workspace": ws_b.display().to_string() }),
    )
    .await;
    let session_b = opened["result"]["session_id"]
        .as_str()
        .expect("session.new returns a session id")
        .to_string();
    assert_ne!(session_b, primary_session);

    // Route a prompt to session A explicitly.
    let a = tcp_rpc(
        &mut w,
        &mut lines,
        2,
        "session.prompt",
        json!({ "prompt": "a", "session_id": primary_session }),
    )
    .await;
    assert!(a.get("error").is_none(), "{}", a);

    // Route a prompt to session B explicitly.
    let b = tcp_rpc(
        &mut w,
        &mut lines,
        3,
        "session.prompt",
        json!({ "prompt": "b", "session_id": session_b }),
    )
    .await;
    assert!(b.get("error").is_none(), "{}", b);

    assert_eq!(
        std::fs::read_to_string(ws_a.join("marker.txt")).unwrap(),
        "a"
    );
    assert_eq!(
        std::fs::read_to_string(ws_b.join("marker.txt")).unwrap(),
        "b"
    );

    // A request naming a session this daemon does not own is refused.
    let foreign = tcp_rpc(
        &mut w,
        &mut lines,
        4,
        "session.prompt",
        json!({ "prompt": "foreign", "session_id": "not-here" }),
    )
    .await;
    assert_eq!(foreign["error"]["code"], json!(-32006), "{}", foreign);

    let _ = cancel_tx.send(true);
    let _ = serve_task.await;
    let _ = std::fs::remove_dir_all(root);
}
