//! MCP deadline classification: one deadline must not report two different error codes.
//!
//! When a run deadline fires while an MCP call is executing, `McpHostInner::call` cancels the worker
//! and then waits a short grace window for the worker's acknowledgement. The worker acknowledges with
//! `StartedCancelled` (its select cannot tell *why* the cancel arrived), and the caller used to return
//! that verbatim. Because `mcp_error_output` maps `StartedCancelled` to `IoError` and
//! `StartedDeadline` to `ToolTimeout`, the *same* deadline produced `IoError` when the acknowledgement
//! beat the grace window and `ToolTimeout` when it did not — a scheduling race observed as a
//! one-in-ten failure of `b05_execution_context`'s deadline tests.
//!
//! The fix is at the caller's deadline branch only: a `StartedCancelled` acknowledgement received
//! *after this deadline sent the cancel* is reclassified as `StartedDeadline`. Explicit user
//! cancellation keeps its own classification (the cancel branch is untouched), which the control below
//! pins so the deadline fix cannot be mistaken for a change to cancel semantics.

use kroma_core::budget::RunDeadline;
use kroma_core::config_file::McpServerConfig;
use kroma_core::env::ExecutionContext;
use kroma_core::extensions::CustomToolContext;
use kroma_core::mcp::McpToolHost;
use kroma_core::messages::ToolOutcome;
use kroma_core::tool::registry::ToolRegistry;
use kroma_core::types::{ErrorCode, RunId};
use kroma_core::workspace::WorkspaceCap;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const SCRIPT_SERVER: &str = r#"
import sys, json, time

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
                            "serverInfo": {"name": "t37", "version": "1"}})
    elif method == "notifications/initialized":
        pass
    elif method == "tools/list":
        respond(msg["id"], {"tools": [
            {"name": "slow_tool", "description": "sleeps", "inputSchema": {"type": "object"}}
        ]})
    elif method == "tools/call":
        time.sleep(float(msg.get("params", {}).get("arguments", {}).get("delay_secs", 3.0)))
        respond(msg["id"], {"content": [{"type": "text", "text": "late"}]})
    elif "id" in msg:
        respond(msg["id"], {})
"#;

fn temp_root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("kroma-t37-{}-{}", tag, uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn dispatch(root: &Path) -> (WorkspaceCap, kroma_core::extensions::ExtensionDispatch) {
    let script = root.join("server.py");
    std::fs::write(&script, SCRIPT_SERVER).unwrap();
    let config = McpServerConfig {
        http_url: None,
        bearer_token_env: None,
        name: "t37".to_string(),
        command: "python3".to_string(),
        args: vec![script.display().to_string()],
        env: Vec::new(),
        read_only: false,
    };
    let host = McpToolHost::start(&[config], Duration::from_secs(30)).expect("host start");
    let ext = host.into_extension("t37");
    let registry = ToolRegistry::new(None);
    registry
        .register_extension(ext)
        .expect("register extension");
    let dispatch = registry
        .extension_dispatch("mcp__t37__slow_tool")
        .expect("slow_tool dispatch");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    (WorkspaceCap::new(&ws).unwrap(), dispatch)
}

/// A deadline that fires while the call is executing must always be `ToolTimeout`, whichever side of
/// the grace window the worker's acknowledgement lands on.
///
/// Each iteration uses a fresh host: an aborted call poisons its client on purpose (a late response
/// must never be matched to a later request — pinned by `b05_execution_context`), so reusing one
/// client would test the poisoning rule instead of the deadline classification.
#[test]
fn a_fired_deadline_is_always_a_tool_timeout() {
    let mut timeouts = 0;
    for iteration in 0..30 {
        let root = temp_root("deadline");
        let (workspace, slow) = dispatch(&root);
        let args = json!({"delay_secs": 3.0}).to_string();
        let run_deadline = RunDeadline::at(Instant::now() + Duration::from_millis(20));
        let exec_ctx =
            ExecutionContext::new(Some(RunId::new()), Some(run_deadline), None, None, None);
        let ctx = CustomToolContext::with_exec_context(&workspace, Some(&exec_ctx));
        let out = slow.handler.execute(&args, &ctx);
        assert_eq!(
            out.outcome,
            ToolOutcome::Error {
                code: ErrorCode::ToolTimeout
            },
            "iteration {iteration}: a fired deadline must report ToolTimeout, got {:?} ({})",
            out.outcome,
            out.content
        );
        assert!(
            out.content.contains("timed out"),
            "iteration {iteration}: the result must name the timeout, got: {}",
            out.content
        );
        timeouts += 1;
        let _ = std::fs::remove_dir_all(&root);
    }
    assert_eq!(timeouts, 30, "every iteration must classify the deadline");
}

/// Control: an explicit user cancel is not a deadline. It keeps its own classification (the call was
/// sent, so execution state is unknown) and must not be reported as a timeout.
#[test]
fn an_explicit_user_cancel_is_not_reclassified_as_a_timeout() {
    let root = temp_root("cancel");
    let (workspace, slow) = dispatch(&root);
    let args = json!({"delay_secs": 3.0}).to_string();

    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
    let exec_ctx = ExecutionContext::new(None, None, Some(cancel_rx), None, None);
    let ctx = CustomToolContext::with_exec_context(&workspace, Some(&exec_ctx));
    let sender = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(50));
        let _ = cancel_tx.send(true);
    });
    let out = slow.handler.execute(&args, &ctx);
    sender.join().unwrap();

    assert_eq!(
        out.outcome,
        ToolOutcome::Error {
            code: ErrorCode::IoError
        },
        "an explicit cancel keeps its own classification, got {:?} ({})",
        out.outcome,
        out.content
    );
    assert!(
        out.content.contains("cancelled") || out.content.contains("unknown"),
        "the cancel result must report unknown execution state, got: {}",
        out.content
    );
    let _ = std::fs::remove_dir_all(&root);
}
