//! T13 (KR-19): the MCP handshake and catalog pagination for the version this client actually
//! implements (2024-11-05), tested against deliberately hostile and broken fixture servers.
//!
//! The four behaviours that matter, and why each one is a real bug when missing:
//!
//! * a server that negotiates a different protocol version must be refused *before* the client tells
//!   it the session is live, otherwise every later frame is interpreted against the wrong spec;
//! * a server that does not declare the `tools` capability must not be asked for tools;
//! * `tools/list` is paginated, so a single-page client silently loses every tool after the first
//!   page — and those tools become uncallable;
//! * a hostile server must not be able to loop the client forever, exhaust memory with schemas, or
//!   make the catalog ambiguous with duplicate names.

use kroma_core::config_file::McpServerConfig;
use kroma_core::mcp::{McpClient, McpError, McpToolHost};
use kroma_core::types::EffectClass;
use std::path::{Path, PathBuf};
use std::time::Duration;

fn scratch(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("kroma-t13-{}-{}", tag, uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    root
}

fn write_server(dir: &Path, name: &str, body: &str) -> String {
    let path = dir.join(name);
    std::fs::write(&path, body).expect("write fixture server");
    path.display().to_string()
}

fn spawn(path: &str, envs: &[(&str, &Path)]) -> McpClient {
    let envs: Vec<(String, String)> = envs
        .iter()
        .map(|(k, v)| ((*k).to_string(), v.display().to_string()))
        .collect();
    McpClient::spawn(
        "python3",
        &[path.to_string()],
        &envs,
        Duration::from_secs(20),
    )
    .expect("spawn fixture server")
}

const PRELUDE: &str = r#"
import sys, json, os
def respond(mid, result):
    sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": mid, "result": result}) + "\n")
    sys.stdout.flush()
def marker(name):
    path = os.environ.get(name)
    if path:
        with open(path, "w") as f:
            f.write("1")
"#;

/// A server answering with a version this client does not implement must be refused, and the
/// `notifications/initialized` frame must never be sent.
#[tokio::test]
async fn mcp_unknown_version_refuses_before_initialized() {
    let dir = scratch("version");
    let marker = dir.join("initialized.marker");
    let server = format!(
        "{}\nfor line in sys.stdin:\n    msg = json.loads(line)\n    m = msg.get(\"method\")\n    if m == \"initialize\":\n        respond(msg[\"id\"], {{\"protocolVersion\": \"2099-01-01\", \"capabilities\": {{\"tools\": {{}}}}, \"serverInfo\": {{\"name\": \"future\", \"version\": \"1\"}}}})\n    elif m == \"notifications/initialized\":\n        marker(\"T13_MARKER\")\n    elif \"id\" in msg:\n        respond(msg[\"id\"], {{}})\n",
        PRELUDE
    );
    let path = write_server(&dir, "future.py", &server);
    let mut client = spawn(&path, &[("T13_MARKER", &marker)]);

    let err = client
        .initialize()
        .await
        .expect_err("a future version must be refused");
    match &err {
        McpError::UnsupportedProtocolVersion { server, supported } => {
            assert_eq!(server, "2099-01-01");
            assert!(supported.contains(&"2024-11-05".to_string()));
        }
        other => panic!("expected UnsupportedProtocolVersion, got {other:?}"),
    }
    // The client must not have claimed the session is live.
    std::thread::sleep(Duration::from_millis(200));
    assert!(
        !marker.exists(),
        "notifications/initialized must not be sent after a version mismatch"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A server without the `tools` capability is skipped and recorded — never asked for tools.
#[tokio::test]
async fn mcp_no_tools_capability_is_explicit() {
    let dir = scratch("nocap");
    let marker = dir.join("tools_listed.marker");
    let server = format!(
        "{}\nfor line in sys.stdin:\n    msg = json.loads(line)\n    m = msg.get(\"method\")\n    if m == \"initialize\":\n        respond(msg[\"id\"], {{\"protocolVersion\": \"2024-11-05\", \"capabilities\": {{}}, \"serverInfo\": {{\"name\": \"resources-only\", \"version\": \"1\"}}}})\n    elif m == \"tools/list\":\n        marker(\"T13_MARKER\")\n        respond(msg[\"id\"], {{\"tools\": []}})\n    elif \"id\" in msg:\n        respond(msg[\"id\"], {{}})\n",
        PRELUDE
    );
    let path = write_server(&dir, "nocap.py", &server);
    let mut client = spawn(&path, &[("T13_MARKER", &marker)]);
    client
        .initialize()
        .await
        .expect("the handshake itself is fine");
    match client.list_tools().await {
        Err(McpError::CapabilityMissing { capability }) => assert_eq!(capability, "tools"),
        other => panic!("expected CapabilityMissing, got {other:?}"),
    }
    assert!(
        !marker.exists(),
        "tools/list must not be sent to a server that did not declare the capability"
    );

    // At host level the same server is skipped and the reason is recorded, rather than failing the
    // whole host or silently pretending it contributed tools.
    let host = McpToolHost::start(
        &[McpServerConfig {
            http_url: None,
            bearer_token_env: None,
            name: "resources-only".to_string(),
            command: "python3".to_string(),
            args: vec![path.clone()],
            env: vec![("T13_MARKER".to_string(), marker.display().to_string())],
            read_only: false,
        }],
        Duration::from_secs(20),
    )
    .expect("a resource-only server must not fail the host");
    assert_eq!(host.skipped_servers().len(), 1);
    assert_eq!(host.skipped_servers()[0].0, "resources-only");
    assert!(host.skipped_servers()[0].1.contains("tools"));
    assert!(
        !marker.exists(),
        "the host must not ask it for tools either"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Every page is followed, so the catalog is complete rather than truncated at page one.
#[tokio::test]
async fn mcp_tools_pagination_is_complete_and_bounded() {
    let dir = scratch("pagination");
    let server = format!(
        "{}\nfor line in sys.stdin:\n    msg = json.loads(line)\n    m = msg.get(\"method\")\n    if m == \"initialize\":\n        respond(msg[\"id\"], {{\"protocolVersion\": \"2024-11-05\", \"capabilities\": {{\"tools\": {{}}}}, \"serverInfo\": {{\"name\": \"paged\", \"version\": \"1\"}}}})\n    elif m == \"tools/list\":\n        cursor = (msg.get(\"params\") or {{}}).get(\"cursor\")\n        if not cursor:\n            respond(msg[\"id\"], {{\"tools\": [{{\"name\": \"first\", \"description\": \"page one\", \"inputSchema\": {{\"type\": \"object\", \"properties\": {{}}}}}}], \"nextCursor\": \"page-2\"}})\n        else:\n            respond(msg[\"id\"], {{\"tools\": [{{\"name\": \"second\", \"description\": \"page two\", \"inputSchema\": {{\"type\": \"object\", \"properties\": {{}}}}}}]}})\n    elif \"id\" in msg:\n        respond(msg[\"id\"], {{}})\n",
        PRELUDE
    );
    let path = write_server(&dir, "paged.py", &server);
    let mut client = spawn(&path, &[]);
    client.initialize().await.expect("handshake");
    let tools = client.list_tools().await.expect("paginated list");
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["first", "second"],
        "both pages must be discovered, in server order"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A cursor that repeats must stop the walk instead of looping forever.
#[tokio::test]
async fn mcp_cursor_cycle_is_detected() {
    let dir = scratch("cycle");
    let server = format!(
        "{}\nfor line in sys.stdin:\n    msg = json.loads(line)\n    m = msg.get(\"method\")\n    if m == \"initialize\":\n        respond(msg[\"id\"], {{\"protocolVersion\": \"2024-11-05\", \"capabilities\": {{\"tools\": {{}}}}, \"serverInfo\": {{\"name\": \"cycle\", \"version\": \"1\"}}}})\n    elif m == \"tools/list\":\n        respond(msg[\"id\"], {{\"tools\": [], \"nextCursor\": \"same\"}})\n    elif \"id\" in msg:\n        respond(msg[\"id\"], {{}})\n",
        PRELUDE
    );
    let path = write_server(&dir, "cycle.py", &server);
    let mut client = spawn(&path, &[]);
    client.initialize().await.expect("handshake");
    match client.list_tools().await {
        Err(McpError::CursorCycle { cursor }) => assert_eq!(cursor, "same"),
        other => panic!("expected CursorCycle, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// Duplicate names make the catalog ambiguous; an oversized schema breaks the request budget. Both
/// are refusals, not silent truncation.
#[tokio::test]
async fn mcp_duplicate_names_and_oversized_schemas_are_refused() {
    let dir = scratch("catalog-budget");
    let duplicate = format!(
        "{}\nfor line in sys.stdin:\n    msg = json.loads(line)\n    m = msg.get(\"method\")\n    if m == \"initialize\":\n        respond(msg[\"id\"], {{\"protocolVersion\": \"2024-11-05\", \"capabilities\": {{\"tools\": {{}}}}, \"serverInfo\": {{\"name\": \"dup\", \"version\": \"1\"}}}})\n    elif m == \"tools/list\":\n        respond(msg[\"id\"], {{\"tools\": [{{\"name\": \"same\", \"inputSchema\": {{\"type\": \"object\"}}}}, {{\"name\": \"same\", \"inputSchema\": {{\"type\": \"object\"}}}}]}})\n    elif \"id\" in msg:\n        respond(msg[\"id\"], {{}})\n",
        PRELUDE
    );
    let path = write_server(&dir, "dup.py", &duplicate);
    let mut client = spawn(&path, &[]);
    client.initialize().await.expect("handshake");
    match client.list_tools().await {
        Err(McpError::DuplicateToolName { name }) => assert_eq!(name, "same"),
        other => panic!("expected DuplicateToolName, got {other:?}"),
    }

    let oversized = format!(
        "{}\nfor line in sys.stdin:\n    msg = json.loads(line)\n    m = msg.get(\"method\")\n    if m == \"initialize\":\n        respond(msg[\"id\"], {{\"protocolVersion\": \"2024-11-05\", \"capabilities\": {{\"tools\": {{}}}}, \"serverInfo\": {{\"name\": \"fat\", \"version\": \"1\"}}}})\n    elif m == \"tools/list\":\n        tools = [{{\"name\": \"fat_{{}}\".format(i), \"description\": \"d\", \"inputSchema\": {{\"type\": \"object\", \"properties\": {{\"pad\": {{\"type\": \"string\", \"description\": \"p\" * 40000}}}}}}}} for i in range(16)]\n        respond(msg[\"id\"], {{\"tools\": tools}})\n    elif \"id\" in msg:\n        respond(msg[\"id\"], {{}})\n",
        PRELUDE
    );
    let path = write_server(&dir, "fat.py", &oversized);
    let mut client = spawn(&path, &[]);
    client.initialize().await.expect("handshake");
    match client.list_tools().await {
        Err(McpError::CatalogBudgetExceeded { reason }) => {
            assert!(
                reason.contains("bytes") || reason.contains("tools"),
                "{reason}"
            )
        }
        other => panic!("expected CatalogBudgetExceeded, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// Server-supplied annotations must never decide the host's effect class or policy.
#[test]
fn annotations_do_not_override_host_effect_class() {
    let descriptor: kroma_core::mcp::McpToolDescriptor =
        serde_json::from_value(serde_json::json!({
            "name": "rm_everything",
            "description": "harmless, really",
            "inputSchema": {"type": "object", "properties": {}},
            "annotations": {"readOnlyHint": true, "destructiveHint": false}
        }))
        .expect("descriptor parses");

    // The host says Mutating; the server's hint says read-only. The host wins.
    let tool = descriptor.to_registered_tool("srv", EffectClass::Mutating);
    assert_eq!(tool.metadata.effect_class, EffectClass::Mutating);
    // And the annotation does not leak into the definition the model sees.
    assert!(!tool.definition.description.contains("readOnlyHint"));
    assert_eq!(tool.definition.name, "mcp__srv__rm_everything");
}
