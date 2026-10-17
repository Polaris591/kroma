//! Capability gap #6 (`v1.5.2`): MCP servers are loaded from *trusted* config into real tool
//! dispatch, and an untrusted workspace config can never spawn one.
//!
//! The roundtrip uses a real MCP stdio server (a small Python process) so the test exercises the
//! production path: config → spawn → `initialize` → `tools/list` → register as a kernel tool →
//! model calls it → `tools/call` → the result reaches the model.

use kroma_core::config_file::{ConfigOverrides, KromaConfigFile, McpServerConfig, ResolvedConfig};
use kroma_core::messages::{AssistantTurn, FinishReason, TokenUsage, ToolCall, TranscriptItem};
use kroma_core::provider::{ModelProvider, ProviderCompletion, ProviderError, ProviderRequest};
use kroma_core::surface::{JsonRpcRequest, KromaRuntimeConfig, RpcDispatcher};
use kroma_core::tool::policy::PolicyMode;
use kroma_core::types::ToolCallId;
use serde_json::json;
use std::collections::BTreeMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::watch;

const MOCK_SERVER: &str = r#"
import sys, json

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
                            "serverInfo": {"name": "mock", "version": "1"}})
    elif method == "notifications/initialized":
        pass
    elif method == "tools/list":
        respond(msg["id"], {"tools": [{"name": "echo", "description": "echo text",
                                       "inputSchema": {"type": "object",
                                                       "properties": {"text": {"type": "string"}},
                                                       "required": ["text"]}}]})
    elif method == "tools/call":
        args = msg.get("params", {}).get("arguments", {}) or {}
        respond(msg["id"], {"content": [{"type": "text", "text": "MCP_ECHO:" + str(args.get("text", ""))}]})
    elif "id" in msg:
        respond(msg["id"], {})
"#;

fn temp_root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("kroma-mcp-{}-{}", tag, uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write_mock_server(root: &Path) -> PathBuf {
    let path = root.join("mock_mcp_server.py");
    std::fs::write(&path, MOCK_SERVER).unwrap();
    path
}

fn usage() -> Option<TokenUsage> {
    Some(TokenUsage {
        input_tokens: 10,
        cached_input_tokens: None,
        cache_creation_input_tokens: None,
        output_tokens: 5,
        reasoning_tokens: None,
    })
}

fn stop(text: &str) -> AssistantTurn {
    AssistantTurn {
        content: Some(text.to_string()),
        reasoning_blocks: Vec::new(),
        tool_calls: Vec::new(),
        finish_reason: FinishReason::Stop,
        usage: usage(),
    }
}

/// Provider that records the tools offered on the first call, invokes the MCP tool, then records the
/// tool result on the second call.
struct ScriptProvider {
    calls: AtomicUsize,
    seen_tools: Mutex<Vec<String>>,
    tool_results: Mutex<Vec<String>>,
}

impl ModelProvider for ScriptProvider {
    fn complete<'a>(
        &'a self,
        req: ProviderRequest,
        _budget: &'a kroma_core::budget::BudgetConfig,
        _cancel_rx: watch::Receiver<bool>,
    ) -> Pin<Box<dyn Future<Output = Result<ProviderCompletion, ProviderError>> + Send + 'a>> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        if call == 0 {
            *self.seen_tools.lock().unwrap() = req.tools.iter().map(|t| t.name.clone()).collect();
        }
        let results: Vec<String> = req
            .messages
            .iter()
            .filter_map(|m| match m {
                TranscriptItem::ToolResult(tr) => Some(tr.content.to_string()),
                _ => None,
            })
            .collect();
        if !results.is_empty() {
            *self.tool_results.lock().unwrap() = results;
        }
        let turn = if call == 0 {
            AssistantTurn {
                content: None,
                reasoning_blocks: Vec::new(),
                tool_calls: vec![ToolCall {
                    call_id: ToolCallId::new("mcp_call_1"),
                    name: "mcp__mock__echo".into(),
                    raw_arguments: json!({ "text": "hello-mcp" }).to_string(),
                }],
                finish_reason: FinishReason::ToolCalls,
                usage: usage(),
            }
        } else {
            stop("done")
        };
        Box::pin(async move {
            Ok(ProviderCompletion {
                turn,
                request_body_bytes: 0,
                latency_ms: 1,
                ttft_ms: Some(1),
                attempts_used: 1,
                reported_model: None,
            })
        })
    }
}

#[tokio::test]
async fn trusted_config_mcp_server_is_dispatchable() {
    let root = temp_root("roundtrip");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let server = write_mock_server(&root);

    let resolved = ResolvedConfig {
        mcp_servers: vec![McpServerConfig {
            name: "mock".to_string(),
            command: "python3".to_string(),
            args: vec![server.display().to_string()],
            env: Vec::new(),
            read_only: false,
        }],
        ..ResolvedConfig::default()
    };

    let provider = Arc::new(ScriptProvider {
        calls: AtomicUsize::new(0),
        seen_tools: Mutex::new(Vec::new()),
        tool_results: Mutex::new(Vec::new()),
    });
    let cfg = KromaRuntimeConfig::new(&ws, root.join("state"), PolicyMode::FullAutonomous)
        .with_resolved_config(resolved);
    let dispatcher = RpcDispatcher::new(cfg, provider.clone()).unwrap();

    let resp = dispatcher
        .handle_request(JsonRpcRequest::new(
            1,
            "session.prompt",
            json!({ "prompt": "please call the echo tool" }),
        ))
        .await;
    assert!(
        serde_json::to_value(&resp).unwrap().get("error").is_none(),
        "prompt failed: {:?}",
        resp
    );

    let seen = provider.seen_tools.lock().unwrap().clone();
    assert!(
        seen.iter().any(|t| t == "mcp__mock__echo"),
        "the MCP tool must be visible to the model, got {:?}",
        seen
    );
    let results = provider.tool_results.lock().unwrap().clone();
    assert!(
        results.iter().any(|r| r.contains("MCP_ECHO:hello-mcp")),
        "the MCP tools/call round trip must return the server's text, got {:?}",
        results
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn an_untrusted_workspace_config_cannot_declare_mcp_servers() {
    // The workspace file parses the section, but the trust guard must drop it.
    let workspace =
        KromaConfigFile::parse_str("[mcp.evil]\ncommand = \"python3\"\nargs = \"/tmp/evil.py\"\n")
            .unwrap();
    assert!(
        workspace.mcp_servers.contains_key("evil"),
        "the parser must read the section so the guard can see it"
    );

    let resolved = ResolvedConfig::resolve(
        &KromaConfigFile::default(),
        &workspace,
        &BTreeMap::new(),
        &ConfigOverrides::default(),
        None,
        PathBuf::from("/ws/.kroma.toml"),
    );
    assert!(
        resolved.mcp_servers.is_empty(),
        "a workspace config must never contribute an MCP server, got {:?}",
        resolved.mcp_servers
    );
    assert!(
        resolved
            .rejected_workspace_keys
            .contains(&"mcp".to_string()),
        "the workspace guard must report the rejected mcp section: {:?}",
        resolved.rejected_workspace_keys
    );

    // A trusted user config does contribute it.
    let user = KromaConfigFile::parse_str(
        "[mcp.good]\ncommand = \"python3\"\nargs = \"/tmp/good.py\"\nread_only = true\n",
    )
    .unwrap();
    let resolved = ResolvedConfig::resolve(
        &user,
        &KromaConfigFile::default(),
        &BTreeMap::new(),
        &ConfigOverrides::default(),
        None,
        PathBuf::from("/ws/.kroma.toml"),
    );
    assert_eq!(resolved.mcp_servers.len(), 1);
    assert_eq!(resolved.mcp_servers[0].name, "good");
    assert!(resolved.mcp_servers[0].read_only);
    assert_eq!(
        resolved.mcp_servers[0].args,
        vec!["/tmp/good.py".to_string()]
    );
}

/// A wedged MCP server must not hang startup: discovery is bounded and the child is reaped.
#[test]
fn a_wedged_mcp_server_times_out_and_is_cleaned_up() {
    use kroma_core::mcp::{McpError, McpToolHost};

    let root = temp_root("wedged");
    let server = root.join("wedged.py");
    std::fs::write(
        &server,
        "import sys, time\nfor _ in sys.stdin:\n    time.sleep(30)\n",
    )
    .unwrap();
    let config = McpServerConfig {
        name: "wedged".to_string(),
        command: "python3".to_string(),
        args: vec![server.display().to_string()],
        env: Vec::new(),
        read_only: false,
    };

    let started = std::time::Instant::now();
    let result = McpToolHost::start(&[config], std::time::Duration::from_millis(300));
    let err = result.err().expect("a wedged server must fail discovery");
    assert!(
        matches!(err, McpError::Timeout(_)),
        "expected a bounded timeout, got {:?}",
        err
    );
    assert!(
        started.elapsed() < std::time::Duration::from_secs(20),
        "discovery must be bounded, took {:?}",
        started.elapsed()
    );

    // The child is killed by the host's cleanup; give it a moment and assert it is gone.
    std::thread::sleep(std::time::Duration::from_millis(300));
    let _ = std::fs::remove_dir_all(&root);
}

// ---------------------------------------------------------------------------------------------
// Finding 8 — MCP call lifetime: an abandoned call must never run later, and shutdown must bound
// the process lifetime and reap the child even when the server stops reading.
// ---------------------------------------------------------------------------------------------

/// Waits for a file to appear (bounded), so a test can order two calls deterministically.
#[cfg(unix)]
fn wait_for_file(path: &Path) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        if path.exists() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    panic!("file never appeared: {}", path.display());
}

/// Reads a PID a test server reported at startup (bounded).
#[cfg(unix)]
fn wait_for_pid(path: &Path) -> i32 {
    wait_for_file(path);
    std::fs::read_to_string(path)
        .expect("pid file")
        .trim()
        .parse()
        .expect("pid file must hold a pid")
}

/// True while `pid` still exists (including as an unreaped zombie). After the parent reaps it,
/// `kill(pid, 0)` fails with `ESRCH`.
#[cfg(unix)]
fn process_alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

const QUEUED_MUTATION_SERVER: &str = r#"
import sys, json, time

def respond(mid, result):
    sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": mid, "result": result}) + "\n")
    sys.stdout.flush()

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    msg = json.loads(line)
    method = msg.get("method")
    if method == "initialize":
        respond(msg["id"], {"protocolVersion": "2024-11-05", "capabilities": {"tools": {}},
                            "serverInfo": {"name": "srv", "version": "1"}})
    elif method == "notifications/initialized":
        pass
    elif method == "tools/list":
        respond(msg["id"], {"tools": [
            {"name": "slow", "description": "slow",
             "inputSchema": {"type": "object", "properties": {"payload": {"type": "string"}}}},
            {"name": "mutate", "description": "mutate",
             "inputSchema": {"type": "object", "properties": {"path": {"type": "string"}}}},
        ]})
        # Stop reading for a while: the next (large) request write blocks in the pipe.
        time.sleep(0.8)
    elif method == "tools/call":
        params = msg.get("params", {}) or {}
        name = params.get("name")
        args = params.get("arguments", {}) or {}
        if name == "mutate":
            with open(args.get("path", "/tmp/mutated"), "w") as f:
                f.write("mutated")
            respond(msg["id"], {"content": [{"type": "text", "text": "mutated"}]})
        else:
            respond(msg["id"], {"content": [{"type": "text", "text": "slow-done"}]})
    elif "id" in msg:
        respond(msg["id"], {})
"#;

#[cfg(unix)]
const BLOCKING_SERVER: &str = r#"
import sys, json, os, time

pid_file = os.environ.get("KROMA_TEST_PID_FILE")
if pid_file:
    with open(pid_file, "w") as f:
        f.write(str(os.getpid()))

def respond(mid, result):
    sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": mid, "result": result}) + "\n")
    sys.stdout.flush()

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    msg = json.loads(line)
    method = msg.get("method")
    if method == "initialize":
        respond(msg["id"], {"protocolVersion": "2024-11-05", "capabilities": {"tools": {}},
                            "serverInfo": {"name": "srv", "version": "1"}})
    elif method == "notifications/initialized":
        pass
    elif method == "tools/list":
        respond(msg["id"], {"tools": [
            {"name": "slow", "description": "slow",
             "inputSchema": {"type": "object", "properties": {}}},
        ]})
    elif method == "tools/call":
        # Hold the call open forever so the test can cancel/unmount it.
        time.sleep(300)
    elif "id" in msg:
        respond(msg["id"], {})
"#;

#[cfg(unix)]
const STOPS_READING_SERVER: &str = r#"
import sys, json, os, time

pid_file = os.environ.get("KROMA_TEST_PID_FILE")
if pid_file:
    with open(pid_file, "w") as f:
        f.write(str(os.getpid()))

def respond(mid, result):
    sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": mid, "result": result}) + "\n")
    sys.stdout.flush()

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    msg = json.loads(line)
    method = msg.get("method")
    if method == "initialize":
        respond(msg["id"], {"protocolVersion": "2024-11-05", "capabilities": {"tools": {}},
                            "serverInfo": {"name": "srv", "version": "1"}})
    elif method == "notifications/initialized":
        pass
    elif method == "tools/list":
        respond(msg["id"], {"tools": [
            {"name": "big", "description": "big",
             "inputSchema": {"type": "object", "properties": {"payload": {"type": "string"}}}},
        ]})
        # Stop reading stdin entirely: the next (large) request write blocks in the pipe.
        time.sleep(300)
    elif "id" in msg:
        respond(msg["id"], {})
"#;

fn mcp_host_with_script(
    root: &Path,
    tag: &str,
    script: &str,
    timeout: std::time::Duration,
    env: Vec<(String, String)>,
) -> (kroma_core::tool::registry::ToolRegistry, String) {
    let server = root.join(format!("{}_server.py", tag));
    std::fs::write(&server, script).unwrap();
    let config = McpServerConfig {
        name: "srv".to_string(),
        command: "python3".to_string(),
        args: vec![server.display().to_string()],
        env,
        read_only: false,
    };
    let host = kroma_core::mcp::McpToolHost::start(&[config], timeout).expect("start MCP host");
    let ext = host.into_extension("mcp");
    let registry = kroma_core::tool::registry::ToolRegistry::new(None);
    let id = registry.register_extension(ext).unwrap();
    (registry, id.as_str().to_string())
}

/// A first call whose write stalls (the server stops reading) must time out, and the mutating call
/// queued behind it must never reach the server after its caller gave up.
#[test]
fn a_timed_out_call_never_executes_a_queued_mutation() {
    use kroma_core::extensions::CustomToolContext;
    use kroma_core::workspace::WorkspaceCap;

    let root = temp_root("queue-expiry");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let marker = root.join("mutated.marker");

    let (registry, _id) = mcp_host_with_script(
        &root,
        "queue",
        QUEUED_MUTATION_SERVER,
        std::time::Duration::from_millis(400),
        Vec::new(),
    );
    let slow = registry.extension_dispatch("mcp__srv__slow").unwrap();
    let mutate = registry.extension_dispatch("mcp__srv__mutate").unwrap();
    let workspace = WorkspaceCap::new(&ws).unwrap();

    // 300 KiB exceeds any pipe buffer, and the server stops reading, so the write blocks until its
    // deadline fires.
    let big = json!({ "payload": "x".repeat(300 * 1024) }).to_string();
    let started = std::time::Instant::now();
    let (slow_out, mutate_out) = std::thread::scope(|scope| {
        let slow_handle = scope.spawn(|| {
            let ctx = CustomToolContext::new(&workspace);
            slow.handler.execute(&big, &ctx)
        });
        // Let the slow call reach the queue first, then queue the mutation behind it.
        std::thread::sleep(std::time::Duration::from_millis(50));
        let ctx = CustomToolContext::new(&workspace);
        let args = json!({ "path": marker.display().to_string() }).to_string();
        let mutate_out = mutate.handler.execute(&args, &ctx);
        let slow_out = slow_handle.join().unwrap();
        (slow_out, mutate_out)
    });

    assert!(
        started.elapsed() < std::time::Duration::from_secs(3),
        "the stalled write must be bounded by the request timeout, took {:?}",
        started.elapsed()
    );
    assert!(
        !slow_out.outcome.is_success(),
        "the slow call must report the timeout, got {:?}",
        slow_out.outcome
    );
    assert!(
        !mutate_out.outcome.is_success(),
        "the queued mutating call must be rejected, got {:?}",
        mutate_out.outcome
    );
    // Give a worker that ignored the deadline time to run the queued mutation, so the assertion
    // below is about the server never seeing it, not about the test finishing first.
    std::thread::sleep(std::time::Duration::from_millis(1200));
    assert!(
        !marker.exists(),
        "the queued mutation must never have reached the server"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// Cancelling/unmounting while a call is in flight must return promptly and reap the child.
/// The pid probe and process-group reaping are Unix semantics; Windows containment is B15.
#[cfg(unix)]
#[test]
fn unmount_during_a_call_is_bounded_and_reaps_the_child() {
    use kroma_core::extensions::CustomToolContext;
    use kroma_core::workspace::WorkspaceCap;

    let root = temp_root("unmount-reap");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let pid_file = root.join("server.pid");

    let (registry, id) = mcp_host_with_script(
        &root,
        "blocking",
        BLOCKING_SERVER,
        std::time::Duration::from_secs(30),
        vec![(
            "KROMA_TEST_PID_FILE".to_string(),
            pid_file.display().to_string(),
        )],
    );
    let dispatch = registry.extension_dispatch("mcp__srv__slow").unwrap();
    let workspace = WorkspaceCap::new(&ws).unwrap();
    let pid = wait_for_pid(&pid_file);
    let ext_id = kroma_core::extensions::ExtensionId::new(id);

    let started = std::time::Instant::now();
    let (report, out) = std::thread::scope(|scope| {
        let handle = scope.spawn(|| {
            let _guard = dispatch.in_flight.begin();
            let ctx = CustomToolContext {
                workspace: &workspace,
                in_flight: Some(Arc::clone(&dispatch.in_flight)),
                exec_context: None,
            };
            dispatch.handler.execute("{}", &ctx)
        });
        // Let the call reach the server, then unmount: this cancels the call and tears the host down.
        std::thread::sleep(std::time::Duration::from_millis(200));
        let report = registry.unregister_extension(&ext_id).unwrap();
        let out = handle.join().unwrap();
        (report, out)
    });

    assert!(
        started.elapsed() < std::time::Duration::from_secs(5),
        "unmount must be bounded, took {:?}",
        started.elapsed()
    );
    assert!(
        !out.outcome.is_success(),
        "the cancelled call must report an error, got {:?}",
        out.outcome
    );
    assert_eq!(
        report.extension_id.as_str(),
        "mcp",
        "the unmount report must name the extension"
    );
    assert!(
        !process_alive(pid),
        "the MCP child (pid {}) must be reaped on unmount",
        pid
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// A server that stops reading a large request must not block shutdown: the write is bounded by the
/// request timeout and the child is reaped when the host is torn down. Unix-only for the same
/// reason as the reap test above.
#[cfg(unix)]
#[test]
fn a_server_that_stops_reading_still_shuts_down() {
    use kroma_core::extensions::{CustomToolContext, ExtensionId};
    use kroma_core::workspace::WorkspaceCap;

    let root = temp_root("stops-reading");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let pid_file = root.join("server.pid");

    let (registry, id) = mcp_host_with_script(
        &root,
        "stops",
        STOPS_READING_SERVER,
        std::time::Duration::from_millis(500),
        vec![(
            "KROMA_TEST_PID_FILE".to_string(),
            pid_file.display().to_string(),
        )],
    );
    let dispatch = registry.extension_dispatch("mcp__srv__big").unwrap();
    let workspace = WorkspaceCap::new(&ws).unwrap();
    let pid = wait_for_pid(&pid_file);

    // 300 KiB exceeds any pipe buffer, so the write blocks until its deadline fires.
    let big = json!({ "payload": "x".repeat(300 * 1024) }).to_string();
    let call_started = std::time::Instant::now();
    let out = {
        let _guard = dispatch.in_flight.begin();
        let ctx = CustomToolContext {
            workspace: &workspace,
            in_flight: Some(Arc::clone(&dispatch.in_flight)),
            exec_context: None,
        };
        dispatch.handler.execute(&big, &ctx)
    };
    assert!(
        !out.outcome.is_success(),
        "a write-blocked call must time out, got {:?}",
        out.outcome
    );
    assert!(
        call_started.elapsed() < std::time::Duration::from_secs(3),
        "the write must be bounded by the request timeout, took {:?}",
        call_started.elapsed()
    );

    let shutdown_started = std::time::Instant::now();
    let _ = registry
        .unregister_extension(&ExtensionId::new(id))
        .expect("unmount must not error");
    assert!(
        shutdown_started.elapsed() < std::time::Duration::from_secs(3),
        "shutdown must be bounded even when the server stopped reading, took {:?}",
        shutdown_started.elapsed()
    );
    assert!(
        !process_alive(pid),
        "the wedged child (pid {}) must be reaped on shutdown",
        pid
    );

    let _ = std::fs::remove_dir_all(&root);
}

// ---------------------------------------------------------------------------------------------
// B04: F04 (Environment sanitization) & F06 (isError handling & invalid arguments)
// ---------------------------------------------------------------------------------------------

const ENV_CHECK_SERVER: &str = r#"
import sys, json, os

def respond(mid, result):
    sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": mid, "result": result}) + "\n")
    sys.stdout.flush()

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    msg = json.loads(line)
    method = msg.get("method")
    if method == "initialize":
        respond(msg["id"], {"protocolVersion": "2024-11-05", "capabilities": {"tools": {}},
                            "serverInfo": {"name": "srv", "version": "1"}})
    elif method == "notifications/initialized":
        pass
    elif method == "tools/list":
        respond(msg["id"], {"tools": [
            {"name": "check_env", "description": "dump env keys",
             "inputSchema": {"type": "object", "properties": {}}},
        ]})
    elif method == "tools/call":
        keys = list(os.environ.keys())
        explicit_val = os.environ.get("AUTHORIZED_MCP_KEY", "")
        has_path = "PATH" in os.environ
        payload = {
            "keys": keys,
            "has_path": has_path,
            "explicit_val": explicit_val,
        }
        respond(msg["id"], {"content": [{"type": "text", "text": json.dumps(payload)}]})
    elif "id" in msg:
        respond(msg["id"], {})
"#;

#[test]
fn mcp_server_inherits_sanitized_env_and_receives_explicit_env() {
    use kroma_core::extensions::CustomToolContext;
    use kroma_core::workspace::WorkspaceCap;

    std::env::set_var("SENTINEL_API_KEY", "unauthorized_sentinel_token");
    std::env::set_var("KROMA_TEST_SECRET", "leak_canary");

    let root = temp_root("env-sanitize");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();

    let (registry, _id) = mcp_host_with_script(
        &root,
        "env_check",
        ENV_CHECK_SERVER,
        std::time::Duration::from_secs(5),
        vec![("AUTHORIZED_MCP_KEY".to_string(), "explicitly_granted".to_string())],
    );

    let dispatch = registry.extension_dispatch("mcp__srv__check_env").unwrap();
    let workspace = WorkspaceCap::new(&ws).unwrap();
    let _guard = dispatch.in_flight.begin();
    let ctx = CustomToolContext {
        workspace: &workspace,
        in_flight: Some(Arc::clone(&dispatch.in_flight)),
        exec_context: None,
    };
    let out = dispatch.handler.execute("{}", &ctx);
    assert!(out.outcome.is_success(), "env check must succeed: {:?}", out.outcome);

    let parsed: serde_json::Value = serde_json::from_str(out.content.trim()).expect("valid JSON response");
    let keys: Vec<String> = parsed["keys"].as_array().unwrap().iter().map(|v| v.as_str().unwrap().to_string()).collect();

    assert!(
        !keys.iter().any(|k| k == "SENTINEL_API_KEY"),
        "SENTINEL_API_KEY must not leak to MCP server, found in env: {:?}",
        keys
    );
    assert!(
        !keys.iter().any(|k| k.starts_with("KROMA_")),
        "KROMA_* must not leak to MCP server, found in env: {:?}",
        keys
    );
    assert_eq!(
        parsed["explicit_val"].as_str().unwrap(),
        "explicitly_granted",
        "explicitly configured env var must be passed to MCP server"
    );
    assert!(
        parsed["has_path"].as_bool().unwrap(),
        "system PATH must be retained in sanitized env"
    );

    let _ = std::fs::remove_dir_all(&root);
}

const IS_ERROR_SERVER: &str = r#"
import sys, json

def respond(mid, result):
    sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": mid, "result": result}) + "\n")
    sys.stdout.flush()

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    msg = json.loads(line)
    method = msg.get("method")
    if method == "initialize":
        respond(msg["id"], {"protocolVersion": "2024-11-05", "capabilities": {"tools": {}},
                            "serverInfo": {"name": "srv", "version": "1"}})
    elif method == "notifications/initialized":
        pass
    elif method == "tools/list":
        respond(msg["id"], {"tools": [
            {"name": "fail_tool", "description": "reports isError: true",
             "inputSchema": {"type": "object", "properties": {}}},
            {"name": "success_tool", "description": "reports isError: false",
             "inputSchema": {"type": "object", "properties": {}}},
            {"name": "default_tool", "description": "omits isError",
             "inputSchema": {"type": "object", "properties": {}}},
            {"name": "success_structured_tool", "description": "reports isError: false with content and structuredContent",
             "inputSchema": {"type": "object", "properties": {}}},
            {"name": "structured_only_tool", "description": "reports isError: false with structuredContent only",
             "inputSchema": {"type": "object", "properties": {}}},
            {"name": "structured_only_fail_tool", "description": "reports isError: true with structuredContent only",
             "inputSchema": {"type": "object", "properties": {}}},
        ]})
    elif method == "tools/call":
        name = msg.get("params", {}).get("name")
        if name == "fail_tool":
            respond(msg["id"], {
                "isError": True,
                "content": [{"type": "text", "text": "structured failure details"}],
                "structuredContent": {"err_code": 404}
            })
        elif name == "success_tool":
            respond(msg["id"], {
                "isError": False,
                "content": [{"type": "text", "text": "normal success"}]
            })
        elif name == "success_structured_tool":
            respond(msg["id"], {
                "isError": False,
                "content": [{"type": "text", "text": "structured success details"}],
                "structuredContent": {"diag": "healthy", "items": 3}
            })
        elif name == "structured_only_tool":
            respond(msg["id"], {
                "isError": False,
                "structuredContent": {"only_data": "value123"}
            })
        elif name == "structured_only_fail_tool":
            respond(msg["id"], {
                "isError": True,
                "structuredContent": {"error_code": 500, "reason": "no content field"}
            })
        else:
            respond(msg["id"], {
                "content": [{"type": "text", "text": "default success"}]
            })
    elif "id" in msg:
        respond(msg["id"], {})
"#;

#[test]
fn mcp_tool_is_error_returns_tool_failure_and_preserves_content() {
    use kroma_core::extensions::CustomToolContext;
    use kroma_core::types::ErrorCode;
    use kroma_core::workspace::WorkspaceCap;

    let root = temp_root("is-error");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();

    let (registry, _id) = mcp_host_with_script(
        &root,
        "is_error",
        IS_ERROR_SERVER,
        std::time::Duration::from_secs(5),
        Vec::new(),
    );
    let workspace = WorkspaceCap::new(&ws).unwrap();

    // 1. fail_tool with isError: true
    let fail_dispatch = registry.extension_dispatch("mcp__srv__fail_tool").unwrap();
    let _guard1 = fail_dispatch.in_flight.begin();
    let ctx1 = CustomToolContext {
        workspace: &workspace,
        in_flight: Some(Arc::clone(&fail_dispatch.in_flight)),
        exec_context: None,
    };
    let fail_out = fail_dispatch.handler.execute("{}", &ctx1);
    assert!(
        !fail_out.outcome.is_success(),
        "isError: true must report tool failure, got: {:?}",
        fail_out.outcome
    );
    match fail_out.outcome {
        kroma_core::messages::ToolOutcome::Error { code } => {
            assert_eq!(code, ErrorCode::IoError);
        }
        _ => panic!("expected ToolOutcome::Error"),
    }
    assert!(
        fail_out.content.contains("structured failure details"),
        "error content must be preserved, got: {}",
        fail_out.content
    );
    assert!(
        fail_out.content.contains("[structuredContent]\n{\"err_code\":404}"),
        "structuredContent must be appended with JSON tag on failure, got: {}",
        fail_out.content
    );

    // 2. success_tool with isError: false
    let succ_dispatch = registry.extension_dispatch("mcp__srv__success_tool").unwrap();
    let _guard2 = succ_dispatch.in_flight.begin();
    let ctx2 = CustomToolContext {
        workspace: &workspace,
        in_flight: Some(Arc::clone(&succ_dispatch.in_flight)),
        exec_context: None,
    };
    let succ_out = succ_dispatch.handler.execute("{}", &ctx2);
    assert!(
        succ_out.outcome.is_success(),
        "isError: false must report tool success, got: {:?}",
        succ_out.outcome
    );
    assert!(succ_out.content.contains("normal success"));

    // 3. default_tool with omitted isError
    let def_dispatch = registry.extension_dispatch("mcp__srv__default_tool").unwrap();
    let _guard3 = def_dispatch.in_flight.begin();
    let ctx3 = CustomToolContext {
        workspace: &workspace,
        in_flight: Some(Arc::clone(&def_dispatch.in_flight)),
        exec_context: None,
    };
    let def_out = def_dispatch.handler.execute("{}", &ctx3);
    assert!(
        def_out.outcome.is_success(),
        "omitted isError must report tool success, got: {:?}",
        def_out.outcome
    );
    assert!(def_out.content.contains("default success"));

    // 4. success_structured_tool with isError: false, content and structuredContent
    let succ_struct_dispatch = registry
        .extension_dispatch("mcp__srv__success_structured_tool")
        .unwrap();
    let _guard4 = succ_struct_dispatch.in_flight.begin();
    let ctx4 = CustomToolContext {
        workspace: &workspace,
        in_flight: Some(Arc::clone(&succ_struct_dispatch.in_flight)),
        exec_context: None,
    };
    let succ_struct_out = succ_struct_dispatch.handler.execute("{}", &ctx4);
    assert!(
        succ_struct_out.outcome.is_success(),
        "success with structuredContent must succeed, got: {:?}",
        succ_struct_out.outcome
    );
    assert!(
        succ_struct_out.content.contains("structured success details"),
        "success content must be preserved, got: {}",
        succ_struct_out.content
    );
    assert!(
        succ_struct_out
            .content
            .contains("[structuredContent]\n{\"diag\":\"healthy\",\"items\":3}"),
        "structuredContent must be appended with JSON tag on success, got: {}",
        succ_struct_out.content
    );

    // 5. structured_only_tool with isError: false and structuredContent only (no content field)
    let struct_only_dispatch = registry
        .extension_dispatch("mcp__srv__structured_only_tool")
        .unwrap();
    let _guard5 = struct_only_dispatch.in_flight.begin();
    let ctx5 = CustomToolContext {
        workspace: &workspace,
        in_flight: Some(Arc::clone(&struct_only_dispatch.in_flight)),
        exec_context: None,
    };
    let struct_only_out = struct_only_dispatch.handler.execute("{}", &ctx5);
    assert!(
        struct_only_out.outcome.is_success(),
        "structured-only tool must succeed, got: {:?}",
        struct_only_out.outcome
    );
    assert!(
        struct_only_out
            .content
            .contains("[structuredContent]\n{\"only_data\":\"value123\"}"),
        "structured-only tool must preserve structuredContent, got: {}",
        struct_only_out.content
    );
    assert!(
        !struct_only_out.content.contains("\"structuredContent\":"),
        "structured-only tool must not duplicate/double encode raw result, got: {}",
        struct_only_out.content
    );

    // 6. structured_only_fail_tool with isError: true and structuredContent only (no content field)
    let struct_fail_dispatch = registry
        .extension_dispatch("mcp__srv__structured_only_fail_tool")
        .unwrap();
    let _guard6 = struct_fail_dispatch.in_flight.begin();
    let ctx6 = CustomToolContext {
        workspace: &workspace,
        in_flight: Some(Arc::clone(&struct_fail_dispatch.in_flight)),
        exec_context: None,
    };
    let struct_fail_out = struct_fail_dispatch.handler.execute("{}", &ctx6);
    assert!(
        !struct_fail_out.outcome.is_success(),
        "structured-only failure must report tool failure, got: {:?}",
        struct_fail_out.outcome
    );
    assert!(
        struct_fail_out.content.contains("[mcp tool error]"),
        "failure prefix must be present, got: {}",
        struct_fail_out.content
    );
    assert!(
        struct_fail_out
            .content
            .contains("[structuredContent]\n{\"error_code\":500,\"reason\":\"no content field\"}"),
        "structured-only failure must preserve structuredContent, got: {}",
        struct_fail_out.content
    );
    assert!(
        !struct_fail_out.content.contains("\"structuredContent\":"),
        "structured-only failure must not duplicate/double encode raw result, got: {}",
        struct_fail_out.content
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn mcp_tool_invalid_arguments_rejected_without_server_dispatch() {
    use kroma_core::extensions::CustomToolContext;
    use kroma_core::types::ErrorCode;
    use kroma_core::workspace::WorkspaceCap;

    let root = temp_root("invalid-args");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();

    let (registry, _id) = mcp_host_with_script(
        &root,
        "is_error",
        IS_ERROR_SERVER,
        std::time::Duration::from_secs(5),
        Vec::new(),
    );
    let workspace = WorkspaceCap::new(&ws).unwrap();
    let dispatch = registry.extension_dispatch("mcp__srv__success_tool").unwrap();

    // 1. Malformed JSON
    let _guard1 = dispatch.in_flight.begin();
    let ctx1 = CustomToolContext {
        workspace: &workspace,
        in_flight: Some(Arc::clone(&dispatch.in_flight)),
        exec_context: None,
    };
    let out1 = dispatch.handler.execute("{invalid_json", &ctx1);
    assert!(
        !out1.outcome.is_success(),
        "malformed JSON arguments must error, got: {:?}",
        out1.outcome
    );
    match out1.outcome {
        kroma_core::messages::ToolOutcome::Error { code } => {
            assert_eq!(code, ErrorCode::InvalidArguments);
        }
        _ => panic!("expected ToolOutcome::Error"),
    }

    // 2. Non-object JSON
    let _guard2 = dispatch.in_flight.begin();
    let ctx2 = CustomToolContext {
        workspace: &workspace,
        in_flight: Some(Arc::clone(&dispatch.in_flight)),
        exec_context: None,
    };
    let out2 = dispatch.handler.execute("[1, 2, 3]", &ctx2);
    assert!(
        !out2.outcome.is_success(),
        "non-object JSON arguments must error, got: {:?}",
        out2.outcome
    );
    match out2.outcome {
        kroma_core::messages::ToolOutcome::Error { code } => {
            assert_eq!(code, ErrorCode::InvalidArguments);
        }
        _ => panic!("expected ToolOutcome::Error"),
    }

    let _ = std::fs::remove_dir_all(&root);
}

