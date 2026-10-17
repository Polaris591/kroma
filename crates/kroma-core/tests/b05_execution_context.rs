//! B05 / F05: Unified execution context tests.
//!
//! Verifies the 6 core invariants required by F05:
//! 1. Queued cancel does not dispatch (server never sees it, reports `NOT_EXECUTED_CANCELLED`).
//! 2. Started cancel returns timely and does not report "not executed" (reports execution status unknown / possible side effects; late responses not merged).
//! 3. Run deadline clamps MCP timeout (queue wait + call duration bounded by run deadline, not global MCP timeout).
//! 4. Half-frame poison fails closed on subsequent calls (no response misattribution).
//! 5. `spawn_blocking` abort does not wait for background thread and does not release in-flight permit early.
//! 6. Extension/MCP chain passes real execution context from caller to handler.

use kroma_core::agent_loop::{build_runtime_with_registry, AgentRunConfig, AgentRunner};
use kroma_core::budget::{BudgetConfig, RunDeadline, TokenBudget};
use kroma_core::config_file::McpServerConfig;
use kroma_core::env::{ExecutionContext, LocalHostEnv, SharedExecutionEnv};
use kroma_core::events::EventLogger;
use kroma_core::extensions::{
    CustomToolContext, CustomToolHandler, ExtensionManifest, ScopedExtension, ScopedToolDefinition,
};
use kroma_core::mcp::{
    McpClient, McpError, McpToolHost, CALL_STATE_CANCELLED_BEFORE_START, CALL_STATE_QUEUED,
    CALL_STATE_STARTED,
};
use kroma_core::messages::{
    AssistantTurn, FinishReason, ToolCall, ToolOutcome, ToolResultMeta, TranscriptItem,
};
use kroma_core::prompt::PromptBuilder;
use kroma_core::provider::{ModelProvider, ProviderCompletion, ProviderError, ProviderRequest};
use kroma_core::tool::definition::{ToolDefinition, ToolMetadata};
use kroma_core::tool::policy::{NonInteractiveApprovalHandler, PolicyEngine, PolicyMode};
use kroma_core::tool::registry::ToolRegistry;
use kroma_core::tool::runtime::ToolRuntime;
use kroma_core::tools::ToolExecutionOutput;
use kroma_core::types::{EffectClass, ErrorCode, RunId, RunStatus, TerminalReason, ToolCallId};
use kroma_core::workspace::WorkspaceCap;
use serde_json::json;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::watch;

const SCRIPT_SERVER: &str = r#"
import sys, json, time, os

def respond(msg_id, result):
    sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": msg_id, "result": result}) + "\n")
    sys.stdout.flush()

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    msg = json.loads(line)
    method = msg.get("method")
    if method == "initialize":
        respond(msg["id"], {"protocolVersion": "2024-11-05", "capabilities": {"tools": {}},
                            "serverInfo": {"name": "test-b05", "version": "1"}})
    elif method == "notifications/initialized":
        pass
    elif method == "tools/list":
        respond(msg["id"], {"tools": [
            {"name": "slow_tool", "description": "sleeps", "inputSchema": {"type": "object"}},
            {"name": "mutate_tool", "description": "mutates", "inputSchema": {"type": "object"}}
        ]})
    elif method == "tools/call":
        tool_name = msg.get("params", {}).get("name")
        args = msg.get("params", {}).get("arguments", {}) or {}
        if tool_name == "slow_tool":
            marker = args.get("started_marker")
            if marker:
                with open(marker, "w") as f:
                    f.write("started")
            delay = float(args.get("delay_secs", 2.0))
            time.sleep(delay)
            done_marker = args.get("done_marker")
            if done_marker:
                with open(done_marker, "w") as f:
                    f.write("done")
            respond(msg["id"], {"content": [{"type": "text", "text": "slow_ok"}]})
        elif tool_name == "mutate_tool":
            marker = args.get("marker")
            if marker:
                with open(marker, "w") as f:
                    f.write("mutated")
            respond(msg["id"], {"content": [{"type": "text", "text": "mutated_ok"}]})
    elif "id" in msg:
        respond(msg["id"], {})
"#;

fn temp_root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("kroma-b05-{}-{}", tag, uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write_server(root: &Path) -> PathBuf {
    let path = root.join("server.py");
    std::fs::write(&path, SCRIPT_SERVER).unwrap();
    path
}

fn setup_mcp_host(root: &Path, timeout: Duration) -> (ToolRegistry, String) {
    let script = write_server(root);
    let config = McpServerConfig {
        name: "b05".to_string(),
        command: "python3".to_string(),
        args: vec![script.display().to_string()],
        env: Vec::new(),
        read_only: false,
    };
    let host = McpToolHost::start(&[config], timeout).expect("host start");
    let ext = host.into_extension("b05");
    let registry = ToolRegistry::new(None);
    let id = registry
        .register_extension(ext)
        .expect("register extension");
    (registry, id.as_str().to_string())
}

/// 1. Queued cancel does not dispatch to the server and reports NotExecutedCancelled.
#[test]
fn queued_cancel_does_not_dispatch() {
    let root = temp_root("queued-cancel");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let workspace = WorkspaceCap::new(&ws).unwrap();

    let (registry, _id) = setup_mcp_host(&root, Duration::from_secs(10));
    let slow_dispatch = registry.extension_dispatch("mcp__b05__slow_tool").unwrap();
    let mutate_dispatch = registry
        .extension_dispatch("mcp__b05__mutate_tool")
        .unwrap();

    let started_marker = root.join("slow_started.marker");
    let done_marker = root.join("slow_done.marker");
    let mutate_marker = root.join("mutate.marker");

    let slow_args = json!({
        "started_marker": started_marker.display().to_string(),
        "delay_secs": 1.0,
        "done_marker": done_marker.display().to_string(),
    })
    .to_string();

    let mutate_args = json!({
        "marker": mutate_marker.display().to_string(),
    })
    .to_string();

    let (cancel_tx, cancel_rx) = watch::channel(false);
    let mutate_ctx_holder =
        ExecutionContext::new(Some(RunId::new()), None, Some(cancel_rx), None, None);

    std::thread::scope(|scope| {
        // First call occupies the MCP worker
        let h_slow = scope.spawn(|| {
            let ctx = CustomToolContext::new(&workspace);
            slow_dispatch.handler.execute(&slow_args, &ctx)
        });

        // Wait until slow call starts on the server
        for _ in 0..100 {
            if started_marker.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            started_marker.exists(),
            "slow call must have started on server"
        );

        // Cancel the mutate call before queueing it
        cancel_tx.send(true).unwrap();

        let mutate_ctx = CustomToolContext::with_exec_context(&workspace, Some(&mutate_ctx_holder));
        let mutate_out = mutate_dispatch.handler.execute(&mutate_args, &mutate_ctx);

        // Mutate call was cancelled while queued: must report NotExecutedCancelled
        assert!(
            !mutate_out.outcome.is_success(),
            "cancelled mutate call must fail"
        );
        match mutate_out.outcome {
            kroma_core::messages::ToolOutcome::Error { code } => {
                assert_eq!(
                    code,
                    ErrorCode::NotExecutedCancelled,
                    "queued cancel must report NotExecutedCancelled"
                );
            }
            _ => panic!("expected Error"),
        }

        let _ = h_slow.join().unwrap();
    });

    // Verify the server NEVER executed the mutate tool
    assert!(
        !mutate_marker.exists(),
        "queued cancelled mutation must never reach the server"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// 2. Started cancel returns timely, does NOT report "not executed", and late responses are discarded.
#[test]
fn started_cancel_returns_timely_with_unknown_side_effects() {
    let root = temp_root("started-cancel");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let workspace = WorkspaceCap::new(&ws).unwrap();

    let (registry, _id) = setup_mcp_host(&root, Duration::from_secs(10));
    let slow_dispatch = registry.extension_dispatch("mcp__b05__slow_tool").unwrap();

    let started_marker = root.join("slow_started.marker");
    let slow_args = json!({
        "started_marker": started_marker.display().to_string(),
        "delay_secs": 2.5,
    })
    .to_string();

    let (cancel_tx, cancel_rx) = watch::channel(false);
    let exec_ctx = ExecutionContext::new(Some(RunId::new()), None, Some(cancel_rx), None, None);

    let start_time = Instant::now();
    let (h_res, _) = std::thread::scope(|scope| {
        let handle = scope.spawn(|| {
            let ctx = CustomToolContext::with_exec_context(&workspace, Some(&exec_ctx));
            slow_dispatch.handler.execute(&slow_args, &ctx)
        });

        // Wait until server confirms the call has started
        for _ in 0..100 {
            if started_marker.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(started_marker.exists(), "call must have reached server");

        // Now cancel while in-flight
        cancel_tx.send(true).unwrap();
        let out = handle.join().unwrap();
        (out, ())
    });

    let elapsed = start_time.elapsed();
    assert!(
        elapsed < Duration::from_millis(800),
        "started cancel must return promptly, took {:?}",
        elapsed
    );

    // Assert it does NOT report NotExecutedCancelled
    assert!(
        !h_res.outcome.is_success(),
        "cancelled call must report error"
    );
    match h_res.outcome {
        kroma_core::messages::ToolOutcome::Error { code } => {
            assert_ne!(
                code,
                ErrorCode::NotExecutedCancelled,
                "started cancel must NOT report NotExecutedCancelled (unknown side-effects)"
            );
        }
        _ => panic!("expected Error"),
    }
    assert!(
        h_res.content.contains("side-effects may have occurred")
            || h_res.content.contains("execution state is unknown")
            || h_res.content.contains("unknown"),
        "error must explicitly report unknown execution state / possible side effects, got: {}",
        h_res.content
    );

    // Sleep to let the python server finish its 2.5s sleep and send late response
    std::thread::sleep(Duration::from_millis(2600));

    // Client was poisoned on cancel, so subsequent call fails fast and does not match the late response
    let ctx2 = CustomToolContext::new(&workspace);
    let next_out = slow_dispatch.handler.execute("{}", &ctx2);
    assert!(!next_out.outcome.is_success());
    assert!(
        next_out.content.contains("poisoned") || next_out.content.contains("IoError"),
        "subsequent call must fail closed, got: {}",
        next_out.content
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// 3. Run deadline clamps MCP timeout (queue wait + call bounded by run deadline).
#[test]
fn run_deadline_clamps_mcp_timeout() {
    let root = temp_root("deadline-clamp");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let workspace = WorkspaceCap::new(&ws).unwrap();

    // Host configured with 30s timeout
    let (registry, _id) = setup_mcp_host(&root, Duration::from_secs(30));
    let slow_dispatch = registry.extension_dispatch("mcp__b05__slow_tool").unwrap();

    let started_marker = root.join("slow_started.marker");
    let slow_args = json!({
        "started_marker": started_marker.display().to_string(),
        "delay_secs": 3.0,
    })
    .to_string();

    // Run deadline is 200ms from now
    let run_deadline = RunDeadline::at(Instant::now() + Duration::from_millis(200));
    let exec_ctx = ExecutionContext::new(Some(RunId::new()), Some(run_deadline), None, None, None);

    let started = Instant::now();
    let ctx = CustomToolContext::with_exec_context(&workspace, Some(&exec_ctx));
    let out = slow_dispatch.handler.execute(&slow_args, &ctx);
    let elapsed = started.elapsed();

    assert!(
        elapsed < Duration::from_millis(800),
        "call must be clamped by run deadline (200ms), not 30s MCP timeout, took {:?}",
        elapsed
    );
    assert!(!out.outcome.is_success(), "clamped timeout must fail");
    match out.outcome {
        kroma_core::messages::ToolOutcome::Error { code } => {
            assert_eq!(
                code,
                ErrorCode::ToolTimeout,
                "timed out call must report ToolTimeout"
            );
        }
        _ => panic!("expected Error"),
    }
    assert!(
        out.content.contains("side-effects may have occurred")
            || out.content.contains("execution state unknown"),
        "started deadline must report unknown side effects, got: {}",
        out.content
    );
    assert!(
        !out.content.contains("30s"),
        "deadline error must use effective duration, not self.timeout 30s, got: {}",
        out.content
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// 4. Half-frame poison fails closed on subsequent calls.
#[tokio::test]
async fn half_frame_poison_fails_closed() {
    let script = r#"
import sys, time
# Send incomplete frame then sleep
sys.stdout.write('{"jsonrpc":"2.0"')
sys.stdout.flush()
time.sleep(5)
"#;
    let mut client = McpClient::spawn(
        "python3",
        &["-c".to_string(), script.to_string()],
        &[],
        Duration::from_millis(200),
    )
    .expect("spawn mock");

    let init_res = client.initialize().await;
    assert!(init_res.is_err(), "incomplete frame must time out");

    // Client must be poisoned now
    let list_res = client.list_tools().await;
    match list_res {
        Err(McpError::Io(msg)) => {
            assert!(
                msg.contains("poisoned"),
                "subsequent call must report poisoned, got: {}",
                msg
            );
        }
        other => panic!("expected poisoned Io error, got: {:?}", other),
    }

    client.shutdown().await;
}

/// 5. spawn_blocking abort does not wait for background thread and retains in-flight permit.
#[tokio::test]
async fn spawn_blocking_abort_does_not_wait_and_retains_permit() {
    let root = temp_root("blocking-abort");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let env: SharedExecutionEnv = Arc::new(LocalHostEnv::new(&ws, &root).unwrap());
    let policy_engine = PolicyEngine::new(
        PolicyMode::AllowWrite,
        Arc::new(NonInteractiveApprovalHandler),
    );
    let budget = BudgetConfig::default();

    let thread_finished = Arc::new(AtomicBool::new(false));
    let tf_clone = Arc::clone(&thread_finished);

    let mut manifest = ExtensionManifest::operator_trusted("slow_ext", "test");
    manifest.capabilities.push("tools".to_string());
    let mut ext = ScopedExtension::new(manifest);

    let slow_handler: Arc<dyn CustomToolHandler> =
        Arc::new(move |_raw: &str, _ctx: &CustomToolContext<'_>| {
            std::thread::sleep(Duration::from_millis(1500));
            tf_clone.store(true, Ordering::SeqCst);
            ToolExecutionOutput::success("slow_done".to_string(), ToolResultMeta::default())
        });

    let tool_def = ToolDefinition {
        name: "slow_custom".to_string(),
        description: "slow blocking".to_string(),
        parameters: json!({"type": "object"}),
    };
    let meta = ToolMetadata {
        effect_class: EffectClass::Mutating,
        default_timeout_sec: 30,
        inline_output_cap_bytes: 64 * 1024,
    };
    ext = ext.with_tool(ScopedToolDefinition::new(tool_def, meta, slow_handler));

    let registry = ToolRegistry::new(None);
    registry.register_extension(ext).unwrap();
    let dispatch = registry.extension_dispatch("slow_custom").unwrap();
    let in_flight = Arc::clone(&dispatch.in_flight);

    let runtime = ToolRuntime::new(registry, policy_engine, env, budget);

    let (cancel_tx, cancel_rx) = watch::channel(false);
    let exec_ctx = ExecutionContext::new(Some(RunId::new()), None, Some(cancel_rx), None, None);

    let call = ToolCall {
        call_id: ToolCallId::new("call_slow"),
        name: "slow_custom".to_string(),
        raw_arguments: "{}".to_string(),
    };

    let start = Instant::now();
    let call_task =
        tokio::spawn(async move { runtime.execute_call_with_context(&call, 1, &exec_ctx).await });

    // Wait a brief moment, then cancel
    tokio::time::sleep(Duration::from_millis(50)).await;
    cancel_tx.send(true).unwrap();

    let (msg, decision, _) = call_task.await.unwrap();
    let elapsed = start.elapsed();

    // Async task returned promptly (< 300ms), NOT waiting 1500ms
    assert!(
        elapsed < Duration::from_millis(400),
        "spawn_blocking abort must not wait for the 1.5s thread, took {:?}",
        elapsed
    );

    // In-flight permit is STILL held by the running background thread!
    assert_eq!(
        in_flight.active(),
        1,
        "in-flight permit must NOT be released early while OS thread is alive"
    );
    assert!(
        !thread_finished.load(Ordering::SeqCst),
        "OS thread should still be running"
    );

    // Outcome is unknown side effects, NOT NotExecutedCancelled
    match msg.outcome {
        kroma_core::messages::ToolOutcome::Error { code } => {
            assert_ne!(
                code,
                ErrorCode::NotExecutedCancelled,
                "started blocking call must not report NotExecutedCancelled"
            );
        }
        _ => panic!("expected Error outcome"),
    }
    assert_eq!(decision, "allow");

    // Wait for the background thread to finish its sleep
    tokio::time::sleep(Duration::from_millis(1600)).await;
    assert!(
        thread_finished.load(Ordering::SeqCst),
        "OS thread must eventually finish"
    );
    assert_eq!(
        in_flight.active(),
        0,
        "in-flight permit is released once OS thread finishes"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// 6. Extension/MCP chain passes real execution context from caller to handler.
#[test]
fn extension_chain_passes_real_context() {
    let root = temp_root("real-context");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let env: SharedExecutionEnv = Arc::new(LocalHostEnv::new(&ws, &root).unwrap());
    let policy_engine = PolicyEngine::new(
        PolicyMode::AllowWrite,
        Arc::new(NonInteractiveApprovalHandler),
    );
    let budget = BudgetConfig::default();

    let expected_run_id = RunId::new();
    let expected_run_id_str = expected_run_id.to_string();
    let expected_deadline = RunDeadline::at(Instant::now() + Duration::from_secs(60));
    let (cancel_tx, cancel_rx) = watch::channel(false);
    let expected_token_budget = Arc::new(TokenBudget::new(Some(50_000)));

    let received_run_id: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let received_deadline: Arc<Mutex<Option<Duration>>> = Arc::new(Mutex::new(None));
    let received_cancelled = Arc::new(AtomicBool::new(false));
    let received_budget_limit: Arc<Mutex<Option<u64>>> = Arc::new(Mutex::new(None));

    let r_id = Arc::clone(&received_run_id);
    let r_dl = Arc::clone(&received_deadline);
    let r_c = Arc::clone(&received_cancelled);
    let r_b = Arc::clone(&received_budget_limit);

    let mut manifest = ExtensionManifest::operator_trusted("ctx_ext", "test");
    manifest.capabilities.push("tools".to_string());
    let mut ext = ScopedExtension::new(manifest);

    let inspect_handler: Arc<dyn CustomToolHandler> =
        Arc::new(move |_raw: &str, ctx: &CustomToolContext<'_>| {
            if let Some(id) = ctx.run_id() {
                *r_id.lock().unwrap() = Some(id.to_string());
            }
            if let Some(dl) = ctx.deadline() {
                *r_dl.lock().unwrap() = Some(dl.remaining());
            }
            r_c.store(ctx.is_cancelled(), Ordering::SeqCst);
            if let Some(b) = ctx.budget() {
                *r_b.lock().unwrap() = b.limit();
            }
            ToolExecutionOutput::success("inspected".to_string(), ToolResultMeta::default())
        });

    let tool_def = ToolDefinition {
        name: "inspect_ctx".to_string(),
        description: "inspects context".to_string(),
        parameters: json!({"type": "object"}),
    };
    let meta = ToolMetadata {
        effect_class: EffectClass::ReadOnly,
        default_timeout_sec: 30,
        inline_output_cap_bytes: 64 * 1024,
    };
    ext = ext.with_tool(ScopedToolDefinition::new(tool_def, meta, inspect_handler));

    let registry = ToolRegistry::new(None);
    registry.register_extension(ext).unwrap();
    let runtime = ToolRuntime::new(registry, policy_engine, env, budget);

    let exec_ctx = ExecutionContext::new(
        Some(expected_run_id),
        Some(expected_deadline),
        Some(cancel_rx),
        Some(expected_token_budget),
        None,
    );

    let call = ToolCall {
        call_id: ToolCallId::new("inspect_call"),
        name: "inspect_ctx".to_string(),
        raw_arguments: "{}".to_string(),
    };

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let (msg, _, _) = runtime.execute_call_with_context(&call, 1, &exec_ctx).await;
        assert!(msg.outcome.is_success());
    });

    assert_eq!(*received_run_id.lock().unwrap(), Some(expected_run_id_str));
    assert!(received_deadline.lock().unwrap().is_some());
    assert!(!received_cancelled.load(Ordering::SeqCst));
    assert_eq!(*received_budget_limit.lock().unwrap(), Some(50_000));

    // Now test with cancel signalled
    cancel_tx.send(true).unwrap();
    rt.block_on(async {
        let (msg, _, _) = runtime.execute_call_with_context(&call, 2, &exec_ctx).await;
        // Since it's cancelled before starting, it reports NotExecutedCancelled
        assert!(!msg.outcome.is_success());
        match msg.outcome {
            kroma_core::messages::ToolOutcome::Error { code } => {
                assert_eq!(code, ErrorCode::NotExecutedCancelled);
            }
            _ => panic!("expected Error"),
        }
    });

    let _ = std::fs::remove_dir_all(&root);
}

/// 7. Deterministic CAS gate test: Caller wins race before start -> NotExecutedCancelled.
#[test]
fn deterministic_cas_caller_wins_before_start() {
    let state = Arc::new(AtomicU8::new(CALL_STATE_QUEUED));

    // Caller checks cancel/deadline and executes CAS from QUEUED -> CANCELLED_BEFORE_START
    let caller_cas = state.compare_exchange(
        CALL_STATE_QUEUED,
        CALL_STATE_CANCELLED_BEFORE_START,
        Ordering::SeqCst,
        Ordering::SeqCst,
    );
    assert_eq!(caller_cas, Ok(CALL_STATE_QUEUED), "caller CAS must succeed");

    // Worker subsequently dequeues and attempts CAS from QUEUED -> STARTED
    let worker_cas = state.compare_exchange(
        CALL_STATE_QUEUED,
        CALL_STATE_STARTED,
        Ordering::SeqCst,
        Ordering::SeqCst,
    );
    assert_eq!(
        worker_cas,
        Err(CALL_STATE_CANCELLED_BEFORE_START),
        "worker CAS must fail because caller already marked CANCELLED_BEFORE_START"
    );
    assert_eq!(
        state.load(Ordering::SeqCst),
        CALL_STATE_CANCELLED_BEFORE_START
    );

    // Verify error mapping: caller's won race maps to NotExecutedCancelled
    let out = kroma_core::mcp::mcp_error_output("tool", &McpError::Cancelled);
    assert_eq!(
        out.outcome,
        ToolOutcome::Error {
            code: ErrorCode::NotExecutedCancelled
        }
    );
    assert!(out.content.contains("cancelled before it ran"));
}

/// 8. Deterministic CAS gate test: Worker wins race and starts -> StartedCancelled with unknown side-effects.
#[test]
fn deterministic_cas_worker_wins_and_starts_before_cancel() {
    let state = Arc::new(AtomicU8::new(CALL_STATE_QUEUED));

    // Worker dequeues and executes CAS from QUEUED -> STARTED
    let worker_cas = state.compare_exchange(
        CALL_STATE_QUEUED,
        CALL_STATE_STARTED,
        Ordering::SeqCst,
        Ordering::SeqCst,
    );
    assert_eq!(worker_cas, Ok(CALL_STATE_QUEUED), "worker CAS must succeed");

    // Caller subsequently observes cancel and attempts CAS from QUEUED -> CANCELLED_BEFORE_START
    let caller_cas = state.compare_exchange(
        CALL_STATE_QUEUED,
        CALL_STATE_CANCELLED_BEFORE_START,
        Ordering::SeqCst,
        Ordering::SeqCst,
    );
    assert_eq!(
        caller_cas,
        Err(CALL_STATE_STARTED),
        "caller CAS must fail because worker already started"
    );
    assert_eq!(state.load(Ordering::SeqCst), CALL_STATE_STARTED);

    // Caller is forbidden from returning NotExecutedCancelled; must report StartedCancelled (IoError, unknown side-effects)
    let out = kroma_core::mcp::mcp_error_output("tool", &McpError::StartedCancelled);
    assert_eq!(
        out.outcome,
        ToolOutcome::Error {
            code: ErrorCode::IoError
        }
    );
    assert!(out
        .content
        .contains("execution state unknown and side-effects may have occurred"));
}

/// 8b. Deterministic gate: Drop happens first, then mark_residual -> no residual increment.
#[test]
fn deterministic_drop_first_then_mark_residual_does_not_leak() {
    let root = temp_root("drop-first");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let (registry, _id) = setup_mcp_host(&root, Duration::from_secs(10));
    let dispatch = registry.extension_dispatch("mcp__b05__slow_tool").unwrap();
    let tracker = Arc::clone(&dispatch.in_flight);

    let guard = tracker.begin_admission().expect("admission succeeds");
    assert_eq!(tracker.active(), 1);
    assert_eq!(tracker.residual_active(), 0);

    let handle = guard.residual_handle();
    // Drop happens FIRST (OS thread finished and dropped guard)
    drop(guard);
    assert_eq!(tracker.active(), 0);
    assert_eq!(tracker.residual_active(), 0);

    // Now async task timeout / cancel fires and calls mark_residual:
    handle.mark_residual();

    // MUST NOT leak residual: drop was first, so mark_residual is a no-op
    assert_eq!(
        tracker.residual_active(),
        0,
        "mark_residual after drop must be no-op and not leak residual"
    );
    assert_eq!(tracker.active(), 0);

    // Subsequent admission must succeed!
    let next_guard = tracker.begin_admission();
    assert!(
        next_guard.is_ok(),
        "subsequent admission must succeed, but got: {:?}",
        next_guard.err()
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// 8c. Deterministic gate: mark_residual happens first, then Drop -> residual and active cleanly decremented to zero.
#[test]
fn deterministic_mark_first_then_drop_decrements_to_zero() {
    let root = temp_root("mark-first");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let (registry, _id) = setup_mcp_host(&root, Duration::from_secs(10));
    let dispatch = registry.extension_dispatch("mcp__b05__slow_tool").unwrap();
    let tracker = Arc::clone(&dispatch.in_flight);

    let guard = tracker.begin_admission().expect("admission succeeds");
    assert_eq!(tracker.active(), 1);
    assert_eq!(tracker.residual_active(), 0);

    let handle = guard.residual_handle();

    // mark_residual happens FIRST (async task cancelled / timed out while OS thread still runs)
    handle.mark_residual();
    assert_eq!(tracker.active(), 1);
    assert_eq!(tracker.residual_active(), 1);

    // New admission while residual is active must be refused
    let blocked_admission = tracker.begin_admission();
    assert!(
        matches!(
            blocked_admission,
            Err(kroma_core::extensions::AdmissionError::OverlappingResidual { residual: 1 })
        ),
        "new admission must be refused while residual is active, got: {:?}",
        blocked_admission
    );

    // Repeated mark_residual must be idempotent
    handle.mark_residual();
    assert_eq!(tracker.residual_active(), 1);

    // Now OS thread finishes and drops guard
    drop(guard);

    // Both active and residual MUST return to 0
    assert_eq!(tracker.active(), 0, "active count must return to 0");
    assert_eq!(
        tracker.residual_active(),
        0,
        "residual count must return to 0"
    );

    // Subsequent admission must succeed!
    let next_guard = tracker.begin_admission();
    assert!(
        next_guard.is_ok(),
        "subsequent admission must succeed after drop"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// 8d. Deterministic gate: panic or unwinding drops InFlightCall and cleans up counts without leak.
#[test]
fn deterministic_panic_unwind_cleans_up_without_leak() {
    let root = temp_root("panic-cleanup");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let (registry, _id) = setup_mcp_host(&root, Duration::from_secs(10));
    let dispatch = registry.extension_dispatch("mcp__b05__slow_tool").unwrap();
    let tracker = Arc::clone(&dispatch.in_flight);

    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = tracker.begin_admission().unwrap();
        assert_eq!(tracker.active(), 1);
        panic!("simulated worker panic");
    }));
    assert!(res.is_err());

    // RAII Drop MUST have run during unwinding
    assert_eq!(
        tracker.active(),
        0,
        "panic unwind must drop guard and decrement active"
    );
    assert_eq!(tracker.residual_active(), 0);

    // Test with residual marked before panic
    let res2 = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let guard = tracker.begin_admission().unwrap();
        guard.mark_residual();
        assert_eq!(tracker.residual_active(), 1);
        panic!("simulated residual worker panic");
    }));
    assert!(res2.is_err());

    // RAII Drop MUST have decremented both active and residual
    assert_eq!(tracker.active(), 0);
    assert_eq!(
        tracker.residual_active(),
        0,
        "panic unwind must decrement residual"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// 8e. Deterministic gate: mark and admit serialization under contention.
#[test]
fn deterministic_mark_and_admit_serialization() {
    let root = temp_root("mark-admit-serial");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let (registry, _id) = setup_mcp_host(&root, Duration::from_secs(10));
    let dispatch = registry.extension_dispatch("mcp__b05__slow_tool").unwrap();
    let tracker = Arc::clone(&dispatch.in_flight);

    // Call 1 enters
    let guard1 = tracker.begin_admission().unwrap();
    let handle1 = guard1.residual_handle();

    // Call 2 also enters before mark (already admitted calls run in parallel)
    let guard2 = tracker.begin_admission().unwrap();
    assert_eq!(tracker.active(), 2);
    assert_eq!(tracker.residual_active(), 0);

    // Call 1 is marked residual
    handle1.mark_residual();
    assert_eq!(tracker.residual_active(), 1);

    // While residual is active, new admission must be refused
    let adm_err = tracker.begin_admission();
    assert!(
        matches!(
            adm_err,
            Err(kroma_core::extensions::AdmissionError::OverlappingResidual { residual: 1 })
        ),
        "admission must be refused while residual is active"
    );

    // Call 2 completes and drops, but Call 1 is still residual
    drop(guard2);
    assert_eq!(tracker.active(), 1);
    assert_eq!(tracker.residual_active(), 1);

    // Still refused because Call 1 is lingering
    assert!(tracker.begin_admission().is_err());

    // Call 1 finally drops
    drop(guard1);
    assert_eq!(tracker.active(), 0);
    assert_eq!(tracker.residual_active(), 0);

    // New admission now succeeds cleanly
    let guard3 = tracker.begin_admission();
    assert!(guard3.is_ok());

    let _ = std::fs::remove_dir_all(&root);
}

/// 9. Residual blocking test: uncooperative lingering thread blocks new overlapping calls until finish.
#[tokio::test]
async fn residual_blocking_rejects_new_overlapping_calls() {
    let root = temp_root("residual-admission");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let env: SharedExecutionEnv = Arc::new(LocalHostEnv::new(&ws, &root).unwrap());
    let policy_engine = PolicyEngine::new(
        PolicyMode::AllowWrite,
        Arc::new(NonInteractiveApprovalHandler),
    );
    let budget = BudgetConfig::default();

    let thread_finished = Arc::new(AtomicBool::new(false));
    let tf_clone = Arc::clone(&thread_finished);

    let mut manifest = ExtensionManifest::operator_trusted("linger_ext", "test");
    manifest.capabilities.push("tools".to_string());
    let mut ext = ScopedExtension::new(manifest);

    let slow_handler: Arc<dyn CustomToolHandler> =
        Arc::new(move |_raw: &str, _ctx: &CustomToolContext<'_>| {
            std::thread::sleep(Duration::from_millis(800));
            tf_clone.store(true, Ordering::SeqCst);
            ToolExecutionOutput::success("done".to_string(), ToolResultMeta::default())
        });

    let tool_def = ToolDefinition {
        name: "linger_tool".to_string(),
        description: "slow linger".to_string(),
        parameters: json!({"type": "object"}),
    };
    let meta = ToolMetadata {
        effect_class: EffectClass::Mutating,
        default_timeout_sec: 30,
        inline_output_cap_bytes: 64 * 1024,
    };
    ext = ext.with_tool(ScopedToolDefinition::new(tool_def, meta, slow_handler));

    let registry = ToolRegistry::new(None);
    registry.register_extension(ext).unwrap();
    let runtime = ToolRuntime::new(registry, policy_engine, env, budget);

    let (cancel_tx, cancel_rx) = watch::channel(false);
    let exec_ctx = ExecutionContext::new(Some(RunId::new()), None, Some(cancel_rx), None, None);

    let call1 = ToolCall {
        call_id: ToolCallId::new("call_1"),
        name: "linger_tool".to_string(),
        raw_arguments: "{}".to_string(),
    };

    // Launch call 1
    let rt_clone = runtime.clone();
    let ctx_clone = exec_ctx.clone();
    let h1 = tokio::spawn(async move {
        rt_clone
            .execute_call_with_context(&call1, 1, &ctx_clone)
            .await
    });

    // Wait 50ms, then cancel call 1
    tokio::time::sleep(Duration::from_millis(50)).await;
    cancel_tx.send(true).unwrap();
    let (msg1, _, _) = h1.await.unwrap();

    // Call 1 returns promptly with unknown side-effects
    assert_eq!(
        msg1.outcome,
        ToolOutcome::Error {
            code: ErrorCode::IoError
        }
    );
    assert!(msg1.content.contains("side-effects may have occurred"));

    // Immediately attempt call 2 on the same tool: must be REFUSED because thread 1 is still residual
    let call2 = ToolCall {
        call_id: ToolCallId::new("call_2"),
        name: "linger_tool".to_string(),
        raw_arguments: "{}".to_string(),
    };
    let fresh_ctx = ExecutionContext::empty();
    let (msg2, _, _) = runtime
        .execute_call_with_context(&call2, 2, &fresh_ctx)
        .await;
    assert_eq!(
        msg2.outcome,
        ToolOutcome::Error {
            code: ErrorCode::IoError
        }
    );
    assert!(
        msg2.content.contains("residual background execution"),
        "overlapping call must be refused while residual thread runs, got: {}",
        msg2.content
    );

    // Wait for the background thread to finish
    for _ in 0..100 {
        if thread_finished.load(Ordering::SeqCst) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        thread_finished.load(Ordering::SeqCst),
        "background thread must complete"
    );

    // Give a brief moment for the drop to settle
    tokio::time::sleep(Duration::from_millis(20)).await;

    // Call 3 should now be admitted and succeed
    let call3 = ToolCall {
        call_id: ToolCallId::new("call_3"),
        name: "linger_tool".to_string(),
        raw_arguments: "{}".to_string(),
    };
    let (msg3, _, _) = runtime
        .execute_call_with_context(&call3, 3, &fresh_ctx)
        .await;
    assert_eq!(msg3.outcome, ToolOutcome::Success);
    assert_eq!(msg3.content.as_ref(), "done");

    let _ = std::fs::remove_dir_all(&root);
}

struct MockToolCallingProvider {
    turn: AssistantTurn,
}

impl ModelProvider for MockToolCallingProvider {
    fn complete<'a>(
        &'a self,
        _req: ProviderRequest,
        _budget: &'a BudgetConfig,
        _rx: watch::Receiver<bool>,
    ) -> Pin<Box<dyn Future<Output = Result<ProviderCompletion, ProviderError>> + Send + 'a>> {
        let turn = self.turn.clone();
        Box::pin(async move {
            Ok(ProviderCompletion {
                turn,
                request_body_bytes: 128,
                latency_ms: 10,
                ttft_ms: None,
                attempts_used: 1,
                reported_model: None,
            })
        })
    }
}

/// 10. AgentRunner real toolchain deadline test: run deadline clamps MCP tool execution.
#[tokio::test]
async fn agent_runner_real_toolchain_clamps_run_deadline() {
    let root = temp_root("agent-deadline");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let state = root.join("state");
    std::fs::create_dir_all(&state).unwrap();

    let (registry, _id) = setup_mcp_host(&root, Duration::from_secs(30));
    let env: SharedExecutionEnv = Arc::new(LocalHostEnv::new(&ws, &state).unwrap());
    let policy_engine = PolicyEngine::new(
        PolicyMode::AllowWrite,
        Arc::new(NonInteractiveApprovalHandler),
    );

    // Run deadline is 400ms (enough time for Python mock to start and write marker, but much less than 3s delay)
    let budget = BudgetConfig {
        max_run_duration: Duration::from_millis(400),
        max_model_turns: 3,
        ..Default::default()
    };
    let runtime = build_runtime_with_registry(registry, env, policy_engine, budget.clone());

    let started_marker = root.join("agent_slow_started.marker");
    let slow_args = json!({
        "started_marker": started_marker.display().to_string(),
        "delay_secs": 3.0,
    })
    .to_string();

    let tool_call = ToolCall {
        call_id: ToolCallId::new("mcp_call_1"),
        name: "mcp__b05__slow_tool".to_string(),
        raw_arguments: slow_args,
    };
    let assistant_turn = AssistantTurn {
        content: None,
        reasoning_blocks: Vec::new(),
        tool_calls: vec![tool_call],
        finish_reason: FinishReason::ToolCalls,
        usage: None,
    };
    let provider = Arc::new(MockToolCallingProvider {
        turn: assistant_turn,
    });

    let cfg = AgentRunConfig::new("test agent deadline", budget);
    let logger = EventLogger::new(cfg.run_id, Some(state), false).unwrap();
    let (_tx, rx) = watch::channel(false);

    let start = Instant::now();
    let (summary, _logger) = AgentRunner::new(cfg, provider, runtime, rx)
        .with_prompt_builder(PromptBuilder::default())
        .run(logger)
        .await;
    let elapsed = start.elapsed();

    // Verify run finished promptly due to run deadline (< 1000ms, NOT 3s delay or 30s timeout)
    assert!(
        elapsed < Duration::from_millis(1000),
        "run must be cut by absolute deadline (400ms), took {:?}",
        elapsed
    );
    assert_eq!(summary.status, RunStatus::BudgetExhausted);
    assert_eq!(summary.terminal_reason, TerminalReason::RunDeadlineExceeded);

    // Verify physical proof that the tool actually crossed the started gate on the server
    assert!(
        started_marker.exists(),
        "tool execution must have crossed the started gate on the server before timing out"
    );

    // Verify the tool result in the transcript
    let tool_res = summary
        .transcript
        .iter()
        .find_map(|item| match item {
            TranscriptItem::ToolResult(tr) if tr.tool_name == "mcp__b05__slow_tool" => Some(tr),
            _ => None,
        })
        .expect("transcript must record the MCP tool result");

    assert_eq!(
        tool_res.outcome,
        ToolOutcome::Error {
            code: ErrorCode::ToolTimeout
        }
    );
    assert!(
        tool_res.content.contains("side-effects may have occurred")
            || tool_res.content.contains("execution state unknown"),
        "tool result must report unknown side-effects, got: {}",
        tool_res.content
    );
    assert!(
        !tool_res.content.contains("30s"),
        "tool result must use effective duration, not 30s, got: {}",
        tool_res.content
    );

    let _ = std::fs::remove_dir_all(&root);
}
