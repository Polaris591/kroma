//! Gate 6 — Multi-Surface Runtime (`v1.0.0`) integration tests.
//!
//! Verifies:
//! 1. `test_acp_stdio_initialize_prompt_event_stream_and_evidence_recall`
//! 2. `test_rpc_approval_pause_and_resume_approve_and_deny_and_disconnect_fail_closed`
//! 3. `test_serve_tcp_daemon_loopback_security_and_cancel_and_fork`
//! 4. `test_interactive_repl_slash_commands_and_multiturn_session`
//! 5. `test_cross_surface_parity_headless_vs_rpc_produce_identical_wal_and_revisions`

use harness_core::agent_loop::{build_runtime_with_registry, AgentRunConfig, AgentRunner};
use harness_core::budget::BudgetConfig;
use harness_core::env::{AllowAllGate, ExecutionEnv, LocalHostEnv};
use harness_core::events::{EventLogger, EVENT_SCHEMA_VERSION};
use harness_core::extensions::discover_skills;
use harness_core::messages::{AssistantTurn, FinishReason, TokenUsage, ToolCall};
use harness_core::prompt::PromptBuilder;
use harness_core::provider::{
    ModelProvider, ProviderCompletion, ProviderError, ProviderRequest,
};
use harness_core::revision::SnapshotStore;
use harness_core::session::SessionJournal;
use harness_core::surface::{
    validate_daemon_listen_addr, HarnessDaemon, HarnessRuntimeConfig, InteractiveRepl,
    JsonRpcRequest, JsonRpcResponse, RpcDispatcher, JSONRPC_UNAUTHORIZED,
};
use harness_core::tool::policy::{NonInteractiveApprovalHandler, PolicyEngine, PolicyMode};
use harness_core::tool::registry::ToolRegistry;
use harness_core::types::{RunStatus, ToolCallId};
use serde_json::{json, Value};
use std::fs;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::watch;
use uuid::Uuid;

fn temp_root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("harness-gate6-{}-{}", tag, Uuid::new_v4()));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn assistant_stop(text: &str) -> AssistantTurn {
    AssistantTurn {
        content: Some(text.to_string()),
        tool_calls: Vec::new(),
        reasoning_blocks: Vec::new(),
        finish_reason: FinishReason::Stop,
        usage: Some(TokenUsage {
            input_tokens: 120,
            cached_input_tokens: Some(90),
            output_tokens: 40,
            reasoning_tokens: None,
        }),
    }
}

fn assistant_tools(calls: Vec<(&str, &str, Value)>) -> AssistantTurn {
    AssistantTurn {
        content: None,
        tool_calls: calls
            .into_iter()
            .map(|(id, name, args)| ToolCall {
                call_id: ToolCallId::new(id),
                name: name.to_string(),
                raw_arguments: args.to_string(),
            })
            .collect(),
        reasoning_blocks: Vec::new(),
        finish_reason: FinishReason::ToolCalls,
        usage: Some(TokenUsage {
            input_tokens: 160,
            cached_input_tokens: Some(120),
            output_tokens: 50,
            reasoning_tokens: None,
        }),
    }
}

struct FnProvider<F> {
    handler: F,
}

impl<F> FnProvider<F>
where
    F: Fn(ProviderRequest) -> Result<AssistantTurn, ProviderError> + Send + Sync + 'static,
{
    fn new(handler: F) -> Arc<Self> {
        Arc::new(Self { handler })
    }
}

impl<F> ModelProvider for FnProvider<F>
where
    F: Fn(ProviderRequest) -> Result<AssistantTurn, ProviderError> + Send + Sync + 'static,
{
    fn complete<'a>(
        &'a self,
        req: ProviderRequest,
        _budget: &'a BudgetConfig,
        _cancel_rx: watch::Receiver<bool>,
    ) -> Pin<Box<dyn Future<Output = Result<ProviderCompletion, ProviderError>> + Send + 'a>> {
        let turn = (self.handler)(req);
        Box::pin(async move {
            let turn = turn?;
            Ok(ProviderCompletion {
                turn,
                request_body_bytes: 256,
                latency_ms: 2,
                ttft_ms: Some(1),
                attempts_used: 1,
            })
        })
    }
}

struct SlowCancellableProvider {
    delay: Duration,
}

impl ModelProvider for SlowCancellableProvider {
    fn complete<'a>(
        &'a self,
        _req: ProviderRequest,
        _budget: &'a BudgetConfig,
        mut cancel_rx: watch::Receiver<bool>,
    ) -> Pin<Box<dyn Future<Output = Result<ProviderCompletion, ProviderError>> + Send + 'a>> {
        let delay = self.delay;
        Box::pin(async move {
            tokio::select! {
                _ = tokio::time::sleep(delay) => {
                    Ok(ProviderCompletion {
                        turn: assistant_stop("completed after delay"),
                        request_body_bytes: 128,
                        latency_ms: delay.as_millis() as u64,
                        ttft_ms: Some(10),
                        attempts_used: 1,
                    })
                }
                _ = async {
                    loop {
                        if *cancel_rx.borrow() {
                            break;
                        }
                        if cancel_rx.changed().await.is_err() {
                            std::future::pending::<()>().await;
                        }
                    }
                } => {
                    Err(ProviderError::Cancelled)
                }
            }
        })
    }
}

/// Test 1: Drive the JSON-RPC `RpcDispatcher` / `acp` stdio stream through `initialize` ->
/// `session.prompt` (editing a file) -> streaming `harness/event` notifications -> `revision.list`
/// -> `evidence.recall` -> `revision.undo`, verifying workspace state and WAL consistency.
#[tokio::test]
async fn test_acp_stdio_initialize_prompt_event_stream_and_evidence_recall() {
    assert_eq!(EVENT_SCHEMA_VERSION, "1.0.0");
    assert_eq!(harness_core::version(), "1.0.0");

    let root = temp_root("acp-stdio");
    let ws = root.join("ws");
    let state = root.join("state");
    fs::create_dir_all(&ws).unwrap();
    fs::create_dir_all(&state).unwrap();
    fs::write(ws.join("app.rs"), "pub fn version() -> u32 {\n    1\n}\n").unwrap();

    let step = Arc::new(AtomicUsize::new(0));
    let step_c = Arc::clone(&step);
    let provider = FnProvider::new(move |_req| {
        let n = step_c.fetch_add(1, Ordering::SeqCst);
        match n {
            0 => Ok(assistant_tools(vec![(
                "call_edit_1",
                "edit",
                json!({
                    "path": "app.rs",
                    "edits": [{
                        "old_text": "    1\n",
                        "new_text": "    2\n"
                    }]
                }),
            )])),
            _ => Ok(assistant_stop("Updated version() to return 2.")),
        }
    });

    let cfg = HarnessRuntimeConfig::new(&ws, &state, PolicyMode::FullAutonomous)
        .with_surface("acp", "stdio");
    let dispatcher = RpcDispatcher::new(cfg, provider).unwrap();

    // Drive `serve_stream` over an in-memory duplex stream (simulating `stdin`/`stdout` in `harness acp`).
    let (client_io, server_io) = tokio::io::duplex(64 * 1024);
    let (server_read, server_write) = tokio::io::split(server_io);
    let (client_read, mut client_write) = tokio::io::split(client_io);

    let disp_server = dispatcher.clone();
    let server_task = tokio::spawn(async move {
        disp_server.serve_stream(server_read, server_write).await.unwrap();
    });

    let mut client_lines = BufReader::new(client_read).lines();

    async fn send_rpc(writer: &mut tokio::io::WriteHalf<tokio::io::DuplexStream>, req: Value) {
        let line = format!("{}\n", req);
        writer.write_all(line.as_bytes()).await.unwrap();
        writer.flush().await.unwrap();
    }

    async fn read_until_response(
        lines: &mut tokio::io::Lines<BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>>,
        expected_id: i64,
        notifications: &mut Vec<Value>,
    ) -> JsonRpcResponse {
        while let Some(raw) = lines.next_line().await.unwrap() {
            let val: Value = serde_json::from_str(&raw)
                .expect("stdout in ACP mode must contain 100% valid JSON-RPC frames");
            if val.get("id").and_then(Value::as_i64) == Some(expected_id) {
                return serde_json::from_value(val).unwrap();
            }
            notifications.push(val);
        }
        panic!("stream closed before receiving response id={}", expected_id);
    }

    let mut notifications = Vec::new();

    // 1. `initialize`
    send_rpc(
        &mut client_write,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "acp.initialize",
            "params": {}
        }),
    )
    .await;
    let init_resp = read_until_response(&mut client_lines, 1, &mut notifications).await;
    assert!(init_resp.is_ok(), "initialize failed: {:?}", init_resp.error);
    let init_val = init_resp.result.unwrap();
    assert_eq!(init_val["version"], "1.0.0");
    assert_eq!(init_val["event_schema_version"], "1.0.0");
    assert_eq!(init_val["capabilities"]["session"], true);
    assert_eq!(init_val["capabilities"]["cancel"], true);
    assert_eq!(init_val["capabilities"]["permission"], true);
    assert_eq!(init_val["capabilities"]["revisions"], true);
    assert_eq!(init_val["capabilities"]["evidence"], true);

    // 2. `session.prompt` (edits `app.rs`)
    send_rpc(
        &mut client_write,
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "session.prompt",
            "params": { "prompt": "Bump version() in app.rs to 2" }
        }),
    )
    .await;
    let prompt_resp = read_until_response(&mut client_lines, 2, &mut notifications).await;
    assert!(prompt_resp.is_ok(), "session.prompt failed: {:?}", prompt_resp.error);
    let prompt_val = prompt_resp.result.unwrap();
    assert_eq!(prompt_val["status"], "completed");
    assert_eq!(
        prompt_val["final_text"],
        "Updated version() to return 2."
    );
    assert_eq!(
        fs::read_to_string(ws.join("app.rs")).unwrap(),
        "pub fn version() -> u32 {\n    2\n}\n"
    );

    // Verify streamed `harness/event` notifications arrived with schema_version "1.0.0".
    let event_types: Vec<String> = notifications
        .iter()
        .filter(|n| n.get("method").and_then(Value::as_str) == Some("harness/event"))
        .filter_map(|n| {
            assert_eq!(n["params"]["schema_version"], "1.0.0");
            n["params"]["event_type"].as_str().map(String::from)
        })
        .collect();
    assert!(event_types.contains(&"SurfaceStarted".to_string()));
    assert!(event_types.contains(&"RunStarted".to_string()));
    assert!(event_types.contains(&"ToolStarted".to_string()));
    assert!(event_types.contains(&"ToolCompleted".to_string()));
    assert!(event_types.contains(&"RevisionCaptured".to_string()));
    assert!(event_types.contains(&"RunTerminated".to_string()));

    // 3. `revision.list`
    send_rpc(
        &mut client_write,
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "revision.list",
            "params": {}
        }),
    )
    .await;
    let rev_resp = read_until_response(&mut client_lines, 3, &mut notifications).await;
    assert!(rev_resp.is_ok());
    let revs = rev_resp.result.unwrap()["revisions"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(
        revs.len(),
        2,
        "expected baseline + post-edit revision: {:?}",
        revs
    );
    assert_eq!(revs[1]["is_current"], true);

    // 4. `evidence.recall` (by `call_edit_1` or `"latest"`)
    send_rpc(
        &mut client_write,
        json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "evidence.recall",
            "params": {
                "source_entry_id": "call_edit_1",
                "offset": 1,
                "limit": 10
            }
        }),
    )
    .await;
    let recall_resp = read_until_response(&mut client_lines, 4, &mut notifications).await;
    assert!(
        recall_resp.is_ok(),
        "evidence.recall failed: {:?}",
        recall_resp.error
    );
    let recall_val = recall_resp.result.unwrap();
    assert_eq!(recall_val["ok"], true);
    let recalled_text = recall_val["content"].as_str().unwrap();
    assert!(
        recalled_text.contains("app.rs"),
        "recalled evidence must contain the edit result: {}",
        recalled_text
    );

    // 5. `revision.undo`
    send_rpc(
        &mut client_write,
        json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "revision.undo",
            "params": {}
        }),
    )
    .await;
    let undo_resp = read_until_response(&mut client_lines, 5, &mut notifications).await;
    assert!(undo_resp.is_ok(), "revision.undo failed: {:?}", undo_resp.error);
    let undo_val = undo_resp.result.unwrap();
    assert_eq!(undo_val["status"], "applied");
    assert_eq!(
        fs::read_to_string(ws.join("app.rs")).unwrap(),
        "pub fn version() -> u32 {\n    1\n}\n",
        "revision.undo must restore app.rs to its baseline content"
    );

    // Verify WAL latest_revision_id matches the baseline revision after undo.
    let baseline_rev = revs[0]["revision_id"].as_str().unwrap();
    let journal = SessionJournal::open(dispatcher.session_path()).unwrap();
    assert_eq!(journal.latest_revision_id().as_deref(), Some(baseline_rev));
    drop(journal);

    // 6. `shutdown`
    send_rpc(
        &mut client_write,
        json!({
            "jsonrpc": "2.0",
            "id": 6,
            "method": "shutdown",
            "params": {}
        }),
    )
    .await;
    let shut_resp = read_until_response(&mut client_lines, 6, &mut notifications).await;
    assert!(shut_resp.is_ok());
    drop(client_write);
    server_task.await.unwrap();

    let _ = fs::remove_dir_all(&root);
}

/// Test 2: In `PolicyMode::Ask`, verify that a mutating tool pauses and emits `approval/requested`,
/// resumes and succeeds when `approval.respond { decision: "approve_once" }` arrives, is denied
/// when `decision: "deny"` arrives, and fails closed if the approval channel closes/times out.
#[tokio::test]
async fn test_rpc_approval_pause_and_resume_approve_and_deny_and_disconnect_fail_closed() {
    let root = temp_root("rpc-approval");
    let ws = root.join("ws");
    let state = root.join("state");
    fs::create_dir_all(&ws).unwrap();
    fs::create_dir_all(&state).unwrap();

    let step = Arc::new(AtomicUsize::new(0));
    let step_c = Arc::clone(&step);
    let provider = FnProvider::new(move |_req| {
        let n = step_c.fetch_add(1, Ordering::SeqCst);
        match n {
            // Sub-case A: first prompt writes `approved.txt` (will be approved once).
            0 => Ok(assistant_tools(vec![(
                "call_write_ok",
                "write",
                json!({ "path": "approved.txt", "content": "approved content\n" }),
            )])),
            1 => Ok(assistant_stop("Approved write done.")),
            // Sub-case B: second prompt writes `denied.txt` (will be denied).
            2 => Ok(assistant_tools(vec![(
                "call_write_deny",
                "write",
                json!({ "path": "denied.txt", "content": "should not exist\n" }),
            )])),
            3 => Ok(assistant_stop("Write was denied.")),
            // Sub-case C: third prompt writes `disconnected.txt` (client closes channel -> fail-closed).
            4 => Ok(assistant_tools(vec![(
                "call_write_disc",
                "write",
                json!({ "path": "disconnected.txt", "content": "never written\n" }),
            )])),
            _ => Ok(assistant_stop("Disconnected write denied.")),
        }
    });

    let cfg = HarnessRuntimeConfig::new(&ws, &state, PolicyMode::Ask)
        .with_surface("acp", "stdio")
        .with_approval_timeout(Duration::from_secs(5));
    let dispatcher = RpcDispatcher::new(cfg, provider).unwrap();
    let mut note_rx = dispatcher.subscribe_notifications();

    // --- Sub-case A: Approve once ---
    let disp_a = dispatcher.clone();
    let prompt_a = tokio::spawn(async move {
        disp_a
            .handle_request(JsonRpcRequest::new(
                10,
                "session.prompt",
                json!({ "prompt": "Create approved.txt" }),
            ))
            .await
    });

    // Wait for `approval/requested` notification.
    let req_id_a = loop {
        let note = tokio::time::timeout(Duration::from_secs(3), note_rx.recv())
            .await
            .expect("timed out waiting for approval/requested")
            .unwrap();
        if note.method == "approval/requested" {
            assert_eq!(note.params["tool_name"], "write");
            break note.params["request_id"].as_str().unwrap().to_string();
        }
    };

    // Inspect via `approval.pending`.
    let pending_resp = dispatcher
        .handle_request(JsonRpcRequest::new(11, "approval.pending", json!({})))
        .await;
    let pending_arr = pending_resp.result.unwrap()["pending"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(pending_arr.len(), 1);
    assert_eq!(pending_arr[0]["request_id"], req_id_a);

    // Respond with `approve_once`.
    let respond_a = dispatcher
        .handle_request(JsonRpcRequest::new(
            12,
            "approval.respond",
            json!({ "request_id": req_id_a, "decision": "approve_once" }),
        ))
        .await;
    assert!(respond_a.is_ok());
    assert_eq!(respond_a.result.unwrap()["decision"], "approve_once");

    let res_a = prompt_a.await.unwrap();
    assert!(res_a.is_ok());
    assert_eq!(
        fs::read_to_string(ws.join("approved.txt")).unwrap(),
        "approved content\n"
    );

    // --- Sub-case B: Deny ---
    let disp_b = dispatcher.clone();
    let prompt_b = tokio::spawn(async move {
        disp_b
            .handle_request(JsonRpcRequest::new(
                20,
                "session.prompt",
                json!({ "prompt": "Create denied.txt" }),
            ))
            .await
    });

    let req_id_b = loop {
        let note = tokio::time::timeout(Duration::from_secs(3), note_rx.recv())
            .await
            .expect("timed out waiting for second approval/requested")
            .unwrap();
        if note.method == "approval/requested" {
            break note.params["request_id"].as_str().unwrap().to_string();
        }
    };

    let respond_b = dispatcher
        .handle_request(JsonRpcRequest::new(
            21,
            "approval.respond",
            json!({ "request_id": req_id_b, "decision": "deny" }),
        ))
        .await;
    assert!(respond_b.is_ok());
    let res_b = prompt_b.await.unwrap();
    assert!(res_b.is_ok());
    assert!(
        !ws.join("denied.txt").exists(),
        "denied write must not create denied.txt"
    );

    // --- Sub-case C: Client disconnect / channel close fails closed ---
    let disp_c = dispatcher.clone();
    let prompt_c = tokio::spawn(async move {
        disp_c
            .handle_request(JsonRpcRequest::new(
                30,
                "session.prompt",
                json!({ "prompt": "Create disconnected.txt" }),
            ))
            .await
    });

    loop {
        let note = tokio::time::timeout(Duration::from_secs(3), note_rx.recv())
            .await
            .expect("timed out waiting for third approval/requested")
            .unwrap();
        if note.method == "approval/requested" {
            break;
        }
    }

    // Simulate client disconnect by closing the approval broker without approving.
    dispatcher.approval_broker().close();

    let res_c = prompt_c.await.unwrap();
    assert!(res_c.is_ok());
    assert!(
        !ws.join("disconnected.txt").exists(),
        "disconnect during approval must fail closed and never write disconnected.txt"
    );

    let _ = fs::remove_dir_all(&root);
}

/// Test 3: Verify `harness serve` rejects non-loopback bind without an auth token, accepts loopback
/// TCP JSON-RPC client requests, supports `session.cancel` on an in-flight run, and supports
/// `session.fork`.
#[tokio::test]
async fn test_serve_tcp_daemon_loopback_security_and_cancel_and_fork() {
    // 1. Non-loopback bind without an auth token is strictly rejected.
    let err = validate_daemon_listen_addr("0.0.0.0:9100", None).unwrap_err();
    assert!(err.contains("non-loopback"));
    let err_empty = validate_daemon_listen_addr("0.0.0.0:9100", Some("   ")).unwrap_err();
    assert!(err_empty.contains("non-loopback"));
    // Non-loopback with a non-empty auth token is allowed.
    assert!(validate_daemon_listen_addr("0.0.0.0:9100", Some("tok-secret")).is_ok());
    // Loopback without a token is allowed.
    assert!(validate_daemon_listen_addr("127.0.0.1:0", None).is_ok());

    let root = temp_root("serve-daemon");
    let ws = root.join("ws");
    let state = root.join("state");
    fs::create_dir_all(&ws).unwrap();
    fs::create_dir_all(&state).unwrap();
    fs::write(ws.join("item.txt"), "initial\n").unwrap();

    // First test auth-token enforcement on a TCP daemon.
    {
        let provider_auth = FnProvider::new(|_| Ok(assistant_stop("ok")));
        let cfg_auth = HarnessRuntimeConfig::new(&ws, state.join("auth-state"), PolicyMode::ReadOnly)
            .with_surface("serve", "tcp")
            .with_auth_token(Some("super-secret-token".to_string()));
        let disp_auth = RpcDispatcher::new(cfg_auth, provider_auth).unwrap();
        let daemon_auth = HarnessDaemon::bind("127.0.0.1:0", disp_auth).await.unwrap();
        let addr_auth = daemon_auth.local_addr();
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let handle_auth = tokio::spawn(async move {
            let _ = daemon_auth.serve(cancel_rx).await;
        });

        let stream = TcpStream::connect(addr_auth).await.unwrap();
        let (r, mut w) = stream.into_split();
        let mut lines = BufReader::new(r).lines();

        // Request without token must fail with JSONRPC_UNAUTHORIZED.
        w.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n")
            .await
            .unwrap();
        let unauth_line = lines.next_line().await.unwrap().unwrap();
        let unauth_resp: JsonRpcResponse = serde_json::from_str(&unauth_line).unwrap();
        assert_eq!(unauth_resp.error.unwrap().code, JSONRPC_UNAUTHORIZED);

        // Request with valid token succeeds.
        w.write_all(
            b"{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"initialize\",\"params\":{\"auth_token\":\"super-secret-token\"}}\n",
        )
        .await
        .unwrap();
        let ok_line = lines.next_line().await.unwrap().unwrap();
        let ok_resp: JsonRpcResponse = serde_json::from_str(&ok_line).unwrap();
        assert!(ok_resp.is_ok());

        let _ = cancel_tx.send(true);
        drop(w);
        let _ = handle_auth.await;
    }

    // Now test loopback TCP daemon: multi-turn prompts, `session.fork`, and `session.cancel` on an
    // in-flight run.
    let call_idx = Arc::new(AtomicUsize::new(0));
    let call_idx_c = Arc::clone(&call_idx);
    let provider = FnProvider::new(move |_req| {
        let idx = call_idx_c.fetch_add(1, Ordering::SeqCst);
        match idx {
            0 => Ok(assistant_stop("Turn 1 complete.")),
            1 => Ok(assistant_stop("Turn 2 complete.")),
            _ => Ok(assistant_stop("Post-fork turn complete.")),
        }
    });

    let cfg = HarnessRuntimeConfig::new(&ws, &state, PolicyMode::FullAutonomous)
        .with_surface("serve", "tcp");
    let dispatcher = RpcDispatcher::new(cfg, provider).unwrap();
    let daemon = HarnessDaemon::bind("127.0.0.1:0", dispatcher.clone())
        .await
        .unwrap();
    let addr = daemon.local_addr();
    let (daemon_cancel_tx, daemon_cancel_rx) = watch::channel(false);
    let daemon_task = tokio::spawn(async move {
        daemon.serve(daemon_cancel_rx).await.unwrap();
    });

    let stream = TcpStream::connect(addr).await.unwrap();
    let (r, mut w) = stream.into_split();
    let mut lines = BufReader::new(r).lines();

    async fn tcp_rpc(
        w: &mut tokio::net::tcp::OwnedWriteHalf,
        lines: &mut tokio::io::Lines<BufReader<tokio::net::tcp::OwnedReadHalf>>,
        id: i64,
        method: &str,
        params: Value,
    ) -> JsonRpcResponse {
        let req = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        w.write_all(format!("{}\n", req).as_bytes()).await.unwrap();
        w.flush().await.unwrap();
        while let Some(line) = lines.next_line().await.unwrap() {
            let v: Value = serde_json::from_str(&line).unwrap();
            if v.get("id").and_then(Value::as_i64) == Some(id) {
                return serde_json::from_value(v).unwrap();
            }
        }
        panic!("TCP stream closed before response id={}", id);
    }

    let r1 = tcp_rpc(&mut w, &mut lines, 1, "session.prompt", json!({ "prompt": "First prompt" })).await;
    assert_eq!(r1.result.unwrap()["status"], "completed");

    let r2 = tcp_rpc(&mut w, &mut lines, 2, "session.prompt", json!({ "prompt": "Second prompt" })).await;
    assert_eq!(r2.result.unwrap()["status"], "completed");

    let st_before = tcp_rpc(&mut w, &mut lines, 3, "session.status", json!({})).await;
    let turns_before = st_before.result.unwrap()["turn_count"].as_u64().unwrap();
    assert!(turns_before >= 2);

    // Fork back to turn 1 over TCP JSON-RPC.
    let fork_resp = tcp_rpc(&mut w, &mut lines, 4, "session.fork", json!({ "turn": 1 })).await;
    assert!(fork_resp.is_ok(), "session.fork failed: {:?}", fork_resp.error);
    assert_eq!(fork_resp.result.unwrap()["turn_count"], 1);

    drop(w);
    let _ = daemon_cancel_tx.send(true);
    daemon_task.await.unwrap();

    // Test `session.cancel` on an in-flight slow run over TCP JSON-RPC.
    let slow_provider = Arc::new(SlowCancellableProvider {
        delay: Duration::from_secs(10),
    });
    let slow_cfg = HarnessRuntimeConfig::new(&ws, state.join("slow"), PolicyMode::ReadOnly)
        .with_surface("serve", "tcp");
    let slow_disp = RpcDispatcher::new(slow_cfg, slow_provider).unwrap();
    let slow_daemon = HarnessDaemon::bind("127.0.0.1:0", slow_disp).await.unwrap();
    let slow_addr = slow_daemon.local_addr();
    let (slow_cancel_tx, slow_cancel_rx) = watch::channel(false);
    let slow_task = tokio::spawn(async move {
        slow_daemon.serve(slow_cancel_rx).await.unwrap();
    });

    let slow_stream = TcpStream::connect(slow_addr).await.unwrap();
    let (sr, mut sw) = slow_stream.into_split();
    let mut slines = BufReader::new(sr).lines();

    // Start long prompt (id=100) without waiting for it to finish, then send `session.cancel` (id=101).
    sw.write_all(
        b"{\"jsonrpc\":\"2.0\",\"id\":100,\"method\":\"session.prompt\",\"params\":{\"prompt\":\"Long task\"}}\n",
    )
    .await
    .unwrap();
    sw.flush().await.unwrap();

    tokio::time::sleep(Duration::from_millis(60)).await;
    sw.write_all(
        b"{\"jsonrpc\":\"2.0\",\"id\":101,\"method\":\"session.cancel\",\"params\":{}}\n",
    )
    .await
    .unwrap();
    sw.flush().await.unwrap();

    let mut got_cancel_ack = false;
    let mut got_prompt_cancelled = false;
    while let Some(line) = tokio::time::timeout(Duration::from_secs(3), slines.next_line())
        .await
        .expect("timed out waiting for cancelled prompt response")
        .unwrap()
    {
        let v: Value = serde_json::from_str(&line).unwrap();
        if v.get("id").and_then(Value::as_i64) == Some(101) {
            assert_eq!(v["result"]["cancelled"], true);
            got_cancel_ack = true;
        } else if v.get("id").and_then(Value::as_i64) == Some(100) {
            assert_eq!(v["result"]["status"], "cancelled");
            got_prompt_cancelled = true;
        }
        if got_cancel_ack && got_prompt_cancelled {
            break;
        }
    }
    assert!(got_cancel_ack && got_prompt_cancelled);

    drop(sw);
    let _ = slow_cancel_tx.send(true);
    slow_task.await.unwrap();

    let _ = fs::remove_dir_all(&root);
}

/// Test 4: Drive the interactive REPL session handler through multi-turn prompts and
/// `/status`, `/revisions`, `/skills`, `/undo`, `/quit`.
#[tokio::test]
async fn test_interactive_repl_slash_commands_and_multiturn_session() {
    let root = temp_root("repl");
    let ws = root.join("ws");
    let state = root.join("state");
    let skill_dir = ws.join(".agents/skills/rust-style");
    fs::create_dir_all(&skill_dir).unwrap();
    fs::create_dir_all(&state).unwrap();
    fs::write(ws.join("note.txt"), "line 1\n").unwrap();
    fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: rust-style\ndescription: Rust formatting rules\n---\nUse rustfmt.\n",
    )
    .unwrap();

    let turn_counter = Arc::new(AtomicUsize::new(0));
    let turn_counter_c = Arc::clone(&turn_counter);
    let provider = FnProvider::new(move |_req| {
        let n = turn_counter_c.fetch_add(1, Ordering::SeqCst);
        match n {
            0 => Ok(assistant_tools(vec![(
                "call_repl_edit",
                "edit",
                json!({
                    "path": "note.txt",
                    "edits": [{
                        "old_text": "line 1\n",
                        "new_text": "line 1\nline 2\n"
                    }]
                }),
            )])),
            1 => Ok(assistant_stop("Added line 2 to note.txt.")),
            _ => Ok(assistant_stop("Inspected note.txt.")),
        }
    });

    let cfg = HarnessRuntimeConfig::new(&ws, &state, PolicyMode::FullAutonomous)
        .with_surface("repl", "tty");
    let dispatcher = RpcDispatcher::new(cfg, provider).unwrap();
    let repl = InteractiveRepl::new(dispatcher);

    let banner = repl.render_status_banner().await;
    assert!(banner.contains("Harness v1.0.0 Interactive REPL"));
    assert!(banner.contains("policy: allow-write+allow-shell"));

    // 1. `/help`
    let help = repl.handle_line("/help").await.unwrap();
    assert!(help.output.contains("/status"));
    assert!(help.output.contains("/undo"));
    assert!(!help.should_exit);

    // 2. `/skills` and `/skills rust-style`
    let skills_list = repl.handle_line("/skills").await.unwrap();
    assert!(skills_list.output.contains("rust-style"));
    let skills_act = repl.handle_line("/skills rust-style").await.unwrap();
    assert!(skills_act.output.contains("activated skill 'rust-style'"));
    assert!(skills_act.output.contains("Use rustfmt."));

    // 3. Multi-turn prompt 1 (edits `note.txt`)
    let p1 = repl.handle_line("Add line 2 to note.txt").await.unwrap();
    assert!(p1.output.contains("Added line 2 to note.txt."));
    assert!(p1.output.contains("status=completed"));
    assert_eq!(
        fs::read_to_string(ws.join("note.txt")).unwrap(),
        "line 1\nline 2\n"
    );

    // 4. Multi-turn prompt 2 (follow-up question in the same session)
    let p2 = repl.handle_line("Confirm the change").await.unwrap();
    assert!(p2.output.contains("Inspected note.txt."));

    // 5. `/status`
    let st = repl.handle_line("/status").await.unwrap();
    assert!(
        st.output.contains("turn_count: 3"),
        "expected turn_count: 3 in status output: {}",
        st.output
    );
    assert!(st.output.contains("mutations: 1"));

    // 6. `/revisions`
    let revs = repl.handle_line("/revisions").await.unwrap();
    assert!(
        revs.output.contains("revisions (3 captured):"),
        "unexpected /revisions output: {}",
        revs.output
    );
    assert!(revs.output.contains("note.txt"));

    // 7. `/undo`
    let undo = repl.handle_line("/undo").await.unwrap();
    assert!(undo.output.contains("restored workspace to"));
    assert_eq!(
        fs::read_to_string(ws.join("note.txt")).unwrap(),
        "line 1\n",
        "/undo in REPL must restore note.txt"
    );

    // 8. `/quit`
    let quit = repl.handle_line("/quit").await.unwrap();
    assert!(quit.should_exit);

    let _ = fs::remove_dir_all(&root);
}

/// Test 5: Verify that running the same task via Headless `AgentRunner` vs. JSON-RPC
/// `session.prompt` produces identical workspace revisions and WAL structure.
#[tokio::test]
async fn test_cross_surface_parity_headless_vs_rpc_produce_identical_wal_and_revisions() {
    let root = temp_root("parity");
    let ws_headless = root.join("ws-headless");
    let state_headless = root.join("state-headless");
    let ws_rpc = root.join("ws-rpc");
    let state_rpc = root.join("state-rpc");

    for dir in [&ws_headless, &state_headless, &ws_rpc, &state_rpc] {
        fs::create_dir_all(dir).unwrap();
    }
    fs::write(ws_headless.join("lib.rs"), "pub const X: i32 = 10;\n").unwrap();
    fs::write(ws_rpc.join("lib.rs"), "pub const X: i32 = 10;\n").unwrap();

    let make_provider = || {
        let step = Arc::new(AtomicUsize::new(0));
        FnProvider::new(move |_req| {
            let n = step.fetch_add(1, Ordering::SeqCst);
            match n {
                0 => Ok(assistant_tools(vec![(
                    "call_parity_1",
                    "edit",
                    json!({
                        "path": "lib.rs",
                        "edits": [{
                            "old_text": "10",
                            "new_text": "42"
                        }]
                    }),
                )])),
                _ => Ok(assistant_stop("Updated X to 42.")),
            }
        })
    };

    // --- Surface 1: Headless `AgentRunner` ---
    let env_h = Arc::new(LocalHostEnv::new(&ws_headless, state_headless.clone()).unwrap());
    let prompt_h = PromptBuilder::default().with_workspace_context(env_h.workspace());
    let skills_h = discover_skills(env_h.workspace());
    let journal_h = SessionJournal::create(state_headless.join("sessions"), env_h.workspace().root()).unwrap();
    let wal_path_h = journal_h.path().to_path_buf();
    let store_h = Arc::new(SnapshotStore::new(state_headless.join("snapshots")));
    let policy_h = PolicyEngine::new(
        PolicyMode::FullAutonomous,
        Arc::new(NonInteractiveApprovalHandler),
    );
    let reg_h = ToolRegistry::new(None).with_workspace_skills(skills_h);
    let rt_h = build_runtime_with_registry(reg_h, env_h, policy_h, BudgetConfig::default());
    let (_tx_h, rx_h) = watch::channel(false);
    let cfg_h = AgentRunConfig::new("Set X to 42", BudgetConfig::default());
    let logger_h = EventLogger::new(cfg_h.run_id, Some(state_headless.clone()), false).unwrap();

    let runner_h = AgentRunner::new(cfg_h, make_provider(), rt_h, rx_h)
        .with_prompt_builder(prompt_h)
        .with_session_journal(journal_h)
        .with_snapshot_store(store_h)
        .with_completion_gate(Arc::new(AllowAllGate));
    let (summary_h, _) = runner_h.run(logger_h).await;
    assert_eq!(summary_h.status, RunStatus::Completed);

    // --- Surface 2: JSON-RPC `RpcDispatcher` (`session.prompt`) ---
    let cfg_rpc = HarnessRuntimeConfig::new(&ws_rpc, &state_rpc, PolicyMode::FullAutonomous)
        .with_surface("acp", "stdio");
    let dispatcher = RpcDispatcher::new(cfg_rpc, make_provider()).unwrap();
    let resp_rpc = dispatcher
        .handle_request(JsonRpcRequest::new(
            1,
            "session.prompt",
            json!({ "prompt": "Set X to 42" }),
        ))
        .await;
    assert!(resp_rpc.is_ok());
    let val_rpc = resp_rpc.result.unwrap();
    assert_eq!(val_rpc["status"], "completed");

    // Compare workspace files, captured revision IDs, and WAL structure!
    assert_eq!(
        fs::read_to_string(ws_headless.join("lib.rs")).unwrap(),
        fs::read_to_string(ws_rpc.join("lib.rs")).unwrap()
    );

    let j_headless = SessionJournal::open(&wal_path_h).unwrap();
    let j_rpc = SessionJournal::open(dispatcher.session_path()).unwrap();

    // Content-addressed revision IDs must be 100% identical across Headless and RPC surfaces!
    let revs_h: Vec<String> = j_headless
        .revision_records()
        .into_iter()
        .map(|r| r.revision_id)
        .collect();
    let revs_rpc: Vec<String> = j_rpc
        .revision_records()
        .into_iter()
        .map(|r| r.revision_id)
        .collect();
    assert_eq!(revs_h.len(), 2);
    assert_eq!(
        revs_h, revs_rpc,
        "Headless and RPC surfaces must produce identical content-addressed WorkspaceRevision chains"
    );

    // Active WAL transcript entries and turn numbers must match 1:1.
    let entries_h = j_headless.active_entries();
    let entries_rpc = j_rpc.active_entries();
    assert_eq!(entries_h.len(), entries_rpc.len());
    for (eh, er) in entries_h.iter().zip(entries_rpc.iter()) {
        assert_eq!(eh.turn, er.turn);
        assert_eq!(eh.item, er.item);
    }

    let _ = fs::remove_dir_all(&root);
}
