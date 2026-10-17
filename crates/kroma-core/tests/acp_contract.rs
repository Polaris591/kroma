//! F06 — ACP v1 adapter contract tests.
//!
//! These tests drive the `acp` stdio surface with **standard Agent Client Protocol requests** (the
//! JSON a real client such as Zed sends) and validate every agent frame against the **official ACP
//! v1 JSON Schema** vendored at `tests/fixtures/acp/schema.v1.json` (see `PROVENANCE.md`). Nothing
//! here reuses Kroma's internal method vocabulary or its own client code to prove itself.
//!
//! Covered: `initialize` (protocolVersion/agentCapabilities), `session/new` (sessionId),
//! `session/prompt` (prompt content blocks → stopReason), `session/update`, the
//! `session/request_permission` request/response round trip, `session/cancel` notification, session
//! routing, and the fact that the internal Kroma RPC is retained on the same surface.

use kroma_core::budget::BudgetConfig;
use kroma_core::messages::{AssistantTurn, FinishReason, TokenUsage, ToolCall};
use kroma_core::provider::{ModelProvider, ProviderCompletion, ProviderError, ProviderRequest};
use kroma_core::surface::{KromaRuntimeConfig, RpcDispatcher};
use kroma_core::tool::policy::PolicyMode;
use kroma_core::types::ToolCallId;
use serde_json::{json, Value};
use std::fs;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{
    AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, Lines, ReadHalf, WriteHalf,
};
use tokio::sync::watch;
use uuid::Uuid;

// ---------------------------------------------------------------------------------------------
// Official schema validation
// ---------------------------------------------------------------------------------------------

fn compile_schema(root: &str) -> jsonschema::Validator {
    let base: Value =
        serde_json::from_str(include_str!("fixtures/acp/schema.v1.json")).expect("fixture parses");
    let defs = base
        .get("$defs")
        .cloned()
        .expect("official schema exposes $defs");
    let schema = json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$defs": defs,
        "$ref": format!("#/$defs/{}", root),
    });
    jsonschema::validator_for(&schema).unwrap_or_else(|e| panic!("{} schema failed: {}", root, e))
}

#[track_caller]
fn assert_matches_schema(root: &str, instance: &Value) {
    let validator = compile_schema(root);
    let errors: Vec<String> = validator
        .iter_errors(instance)
        .map(|e| e.to_string())
        .collect();
    assert!(
        errors.is_empty(),
        "instance does not match official ACP schema #/$defs/{}: {:?}\ninstance: {}",
        root,
        errors,
        instance
    );
}

// ---------------------------------------------------------------------------------------------
// Test harness
// ---------------------------------------------------------------------------------------------

fn temp_root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("kroma-acp-{}-{}", tag, Uuid::new_v4()));
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
            input_tokens: 10,
            cached_input_tokens: Some(0),
            cache_creation_input_tokens: None,
            output_tokens: 5,
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
            input_tokens: 12,
            cached_input_tokens: Some(0),
            cache_creation_input_tokens: None,
            output_tokens: 6,
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
                request_body_bytes: 128,
                latency_ms: 1,
                ttft_ms: Some(1),
                attempts_used: 1,
                reported_model: None,
            })
        })
    }
}

struct SlowProvider {
    delay: Duration,
}

impl ModelProvider for SlowProvider {
    fn complete<'a>(
        &'a self,
        _req: ProviderRequest,
        _budget: &'a BudgetConfig,
        mut cancel_rx: watch::Receiver<bool>,
    ) -> Pin<Box<dyn Future<Output = Result<ProviderCompletion, ProviderError>> + Send + 'a>> {
        let delay = self.delay;
        Box::pin(async move {
            tokio::select! {
                _ = tokio::time::sleep(delay) => Ok(ProviderCompletion {
                    turn: assistant_stop("too late"),
                    request_body_bytes: 64,
                    latency_ms: delay.as_millis() as u64,
                    ttft_ms: Some(1),
                    attempts_used: 1,
                reported_model: None,
                }),
                _ = async {
                    loop {
                        if *cancel_rx.borrow() { break; }
                        if cancel_rx.changed().await.is_err() {
                            std::future::pending::<()>().await;
                        }
                    }
                } => Err(ProviderError::Cancelled),
            }
        })
    }
}

struct AcpStream {
    write: WriteHalf<DuplexStream>,
    lines: Lines<BufReader<ReadHalf<DuplexStream>>>,
    seen: Vec<Value>,
}

impl AcpStream {
    async fn spawn(dispatcher: RpcDispatcher) -> Self {
        let (client_io, server_io) = tokio::io::duplex(256 * 1024);
        let (server_read, server_write) = tokio::io::split(server_io);
        let (client_read, client_write) = tokio::io::split(client_io);
        tokio::spawn(async move {
            dispatcher
                .serve_stream(server_read, server_write)
                .await
                .unwrap();
        });
        Self {
            write: client_write,
            lines: BufReader::new(client_read).lines(),
            seen: Vec::new(),
        }
    }

    async fn send(&mut self, frame: Value) {
        let line = format!("{}\n", frame);
        self.write.write_all(line.as_bytes()).await.unwrap();
        self.write.flush().await.unwrap();
    }

    async fn next_frame(&mut self) -> Value {
        match self.lines.next_line().await.unwrap() {
            Some(line) => serde_json::from_str(&line)
                .unwrap_or_else(|e| panic!("ACP stream emitted invalid JSON ({}): {}", e, line)),
            None => panic!("ACP stdio stream closed unexpectedly"),
        }
    }

    /// Read frames until the response with `id` arrives; every intermediate frame is recorded.
    async fn response(&mut self, id: i64) -> Value {
        loop {
            let frame = self.next_frame().await;
            if frame.get("id").and_then(Value::as_i64) == Some(id) {
                return frame;
            }
            self.seen.push(frame);
        }
    }

    fn take_seen(&mut self) -> Vec<Value> {
        std::mem::take(&mut self.seen)
    }
}

async fn initialize_handshake(client: &mut AcpStream) {
    client
        .send(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": 1,
                "clientCapabilities": { "fs": { "readTextFile": true, "writeTextFile": true } },
                "clientInfo": { "name": "contract-test-client", "version": "1.0.0" }
            }
        }))
        .await;
    let resp = client.response(1).await;
    assert!(resp.get("error").is_none(), "initialize failed: {:?}", resp);
    let result = resp.get("result").cloned().unwrap();
    assert_matches_schema("InitializeResponse", &result);
    assert_eq!(result["protocolVersion"], json!(1));
    assert!(
        result["protocolVersion"].is_u64(),
        "protocolVersion must be an integer, got {}",
        result["protocolVersion"]
    );
    assert!(
        result.get("protocol_version").is_none(),
        "the adapter must not leak the internal string version field"
    );
    assert_eq!(result["agentCapabilities"]["loadSession"], json!(false));
    assert_eq!(
        result["agentCapabilities"]["promptCapabilities"]["image"],
        json!(false)
    );
    assert_eq!(result["agentInfo"]["name"], json!("kroma"));
}

async fn new_session(client: &mut AcpStream, cwd: &str, id: i64) -> String {
    client
        .send(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "session/new",
            "params": { "cwd": cwd, "mcpServers": [] }
        }))
        .await;
    let resp = client.response(id).await;
    assert!(
        resp.get("error").is_none(),
        "session/new failed: {:?}",
        resp
    );
    let result = resp.get("result").cloned().unwrap();
    assert_matches_schema("NewSessionResponse", &result);
    result["sessionId"].as_str().unwrap().to_string()
}

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn acp_initialize_negotiates_protocol_version_and_capabilities() {
    let root = temp_root("initialize");
    let ws = root.join("ws");
    fs::create_dir_all(&ws).unwrap();
    let provider = FnProvider::new(|_| Ok(assistant_stop("noop")));
    let cfg = KromaRuntimeConfig::new(&ws, root.join("state"), PolicyMode::FullAutonomous)
        .with_surface("acp", "stdio")
        .with_acp_protocol(true);
    let dispatcher = RpcDispatcher::new(cfg, provider).unwrap();
    let mut client = AcpStream::spawn(dispatcher).await;

    initialize_handshake(&mut client).await;

    // The internal Kroma RPC is retained on the same surface and still returns its own shape.
    client
        .send(json!({ "jsonrpc": "2.0", "id": 99, "method": "acp.initialize", "params": {} }))
        .await;
    let internal = client.response(99).await;
    assert!(internal.get("error").is_none(), "{:?}", internal);
    let internal_result = internal["result"].clone();
    assert_eq!(internal_result["protocol_version"], json!("1.0"));
    assert_eq!(internal_result["capabilities"]["session"], json!(true));
}

#[tokio::test]
async fn acp_session_prompt_accepts_content_blocks_and_streams_updates() {
    let root = temp_root("prompt");
    let ws = root.join("ws");
    fs::create_dir_all(&ws).unwrap();
    let step = Arc::new(AtomicUsize::new(0));
    let step_c = Arc::clone(&step);
    let provider = FnProvider::new(move |_| {
        let n = step_c.fetch_add(1, Ordering::SeqCst);
        match n {
            0 => Ok(assistant_tools(vec![(
                "call_write_1",
                "write",
                json!({ "path": "note.txt", "content": "hello from acp\n" }),
            )])),
            _ => Ok(assistant_stop("Wrote note.txt.")),
        }
    });
    let cfg = KromaRuntimeConfig::new(&ws, root.join("state"), PolicyMode::FullAutonomous)
        .with_surface("acp", "stdio")
        .with_acp_protocol(true);
    let dispatcher = RpcDispatcher::new(cfg, provider).unwrap();
    let mut client = AcpStream::spawn(dispatcher).await;

    initialize_handshake(&mut client).await;
    let session_id = new_session(&mut client, ws.to_str().unwrap(), 2).await;

    client
        .send(json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "session/prompt",
            "params": {
                "sessionId": session_id,
                "prompt": [
                    { "type": "text", "text": "Please write a note." },
                    { "type": "resource_link", "name": "spec", "uri": "file:///spec.md" }
                ]
            }
        }))
        .await;
    let resp = client.response(3).await;
    assert!(
        resp.get("error").is_none(),
        "session/prompt failed: {:?}",
        resp
    );
    let result = resp["result"].clone();
    assert_matches_schema("PromptResponse", &result);
    assert_eq!(result["stopReason"], json!("end_turn"));

    // The write tool actually ran.
    assert_eq!(
        fs::read_to_string(ws.join("note.txt")).unwrap(),
        "hello from acp\n"
    );

    // Every streamed frame must be an official `session/update`, and the set must cover the tool
    // lifecycle plus the final assistant message.
    let frames = client.take_seen();
    let mut variants: Vec<String> = Vec::new();
    for frame in &frames {
        assert_eq!(
            frame["method"],
            json!("session/update"),
            "unexpected frame: {}",
            frame
        );
        let params = frame["params"].clone();
        assert_matches_schema("SessionNotification", &params);
        assert_eq!(params["sessionId"], json!(session_id));
        variants.push(
            params["update"]["sessionUpdate"]
                .as_str()
                .unwrap()
                .to_string(),
        );
    }
    assert!(
        variants.contains(&"tool_call".to_string()),
        "variants: {:?}",
        variants
    );
    assert!(
        variants.contains(&"tool_call_update".to_string()),
        "variants: {:?}",
        variants
    );
    assert!(
        variants.contains(&"agent_message_chunk".to_string()),
        "variants: {:?}",
        variants
    );
    let message = frames
        .iter()
        .find(|f| f["params"]["update"]["sessionUpdate"] == json!("agent_message_chunk"))
        .unwrap();
    assert_eq!(
        message["params"]["update"]["content"]["text"],
        json!("Wrote note.txt.")
    );
}

#[tokio::test]
async fn acp_rejects_non_conformant_requests() {
    let root = temp_root("reject");
    let ws = root.join("ws");
    fs::create_dir_all(&ws).unwrap();
    let provider = FnProvider::new(|_| Ok(assistant_stop("noop")));
    let cfg = KromaRuntimeConfig::new(&ws, root.join("state"), PolicyMode::FullAutonomous)
        .with_surface("acp", "stdio")
        .with_acp_protocol(true);
    let dispatcher = RpcDispatcher::new(cfg, provider).unwrap();
    let mut client = AcpStream::spawn(dispatcher).await;

    // initialize without the required protocolVersion.
    client
        .send(json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {} }))
        .await;
    let resp = client.response(1).await;
    assert_eq!(resp["error"]["code"], json!(-32602), "{}", resp);
    assert_matches_schema("Error", &resp["error"]);

    initialize_handshake(&mut client).await;

    // session/new without `mcpServers` (required by the schema).
    client
        .send(json!({
            "jsonrpc": "2.0", "id": 2, "method": "session/new",
            "params": { "cwd": ws.to_str().unwrap() }
        }))
        .await;
    let resp = client.response(2).await;
    assert_eq!(resp["error"]["code"], json!(-32602), "{}", resp);

    // A relative cwd is rejected.
    client
        .send(json!({
            "jsonrpc": "2.0", "id": 21, "method": "session/new",
            "params": { "cwd": "relative/path", "mcpServers": [] }
        }))
        .await;
    let resp = client.response(21).await;
    assert_eq!(resp["error"]["code"], json!(-32602), "{}", resp);

    // A cwd that is not this runtime's workspace is refused rather than silently ignored.
    let other = temp_root("other-workspace");
    client
        .send(json!({
            "jsonrpc": "2.0", "id": 22, "method": "session/new",
            "params": { "cwd": other.to_str().unwrap(), "mcpServers": [] }
        }))
        .await;
    let resp = client.response(22).await;
    assert_eq!(resp["error"]["code"], json!(-32602), "{}", resp);

    let session_id = new_session(&mut client, ws.to_str().unwrap(), 3).await;

    // Legacy string prompt is not a valid ACP prompt.
    client
        .send(json!({
            "jsonrpc": "2.0", "id": 4, "method": "session/prompt",
            "params": { "sessionId": session_id, "prompt": "hello" }
        }))
        .await;
    let resp = client.response(4).await;
    assert_eq!(resp["error"]["code"], json!(-32602), "{}", resp);
    assert_matches_schema("Error", &resp["error"]);

    // Image prompt blocks are rejected because the capability was not advertised.
    client
        .send(json!({
            "jsonrpc": "2.0", "id": 5, "method": "session/prompt",
            "params": { "sessionId": session_id, "prompt": [{ "type": "image", "data": "x", "mimeType": "image/png" }] }
        }))
        .await;
    let resp = client.response(5).await;
    assert_eq!(resp["error"]["code"], json!(-32602), "{}", resp);

    // A misrouted sessionId is refused instead of mutating another session.
    client
        .send(json!({
            "jsonrpc": "2.0", "id": 6, "method": "session/prompt",
            "params": { "sessionId": "00000000-0000-0000-0000-000000000000", "prompt": [{ "type": "text", "text": "hi" }] }
        }))
        .await;
    let resp = client.response(6).await;
    assert_eq!(resp["error"]["code"], json!(-32602), "{}", resp);
}

#[tokio::test]
async fn acp_permission_request_is_standard_and_gates_the_tool() {
    let root = temp_root("permission");
    let ws = root.join("ws");
    fs::create_dir_all(&ws).unwrap();
    let step = Arc::new(AtomicUsize::new(0));
    let step_c = Arc::clone(&step);
    let provider = FnProvider::new(move |_| {
        let n = step_c.fetch_add(1, Ordering::SeqCst);
        match n {
            0 => Ok(assistant_tools(vec![(
                "call_write_1",
                "write",
                json!({ "path": "gated.txt", "content": "approved\n" }),
            )])),
            _ => Ok(assistant_stop("done")),
        }
    });
    let cfg = KromaRuntimeConfig::new(&ws, root.join("state"), PolicyMode::Ask)
        .with_surface("acp", "stdio")
        .with_acp_protocol(true);
    let dispatcher = RpcDispatcher::new(cfg, provider).unwrap();
    let mut client = AcpStream::spawn(dispatcher).await;

    initialize_handshake(&mut client).await;
    let session_id = new_session(&mut client, ws.to_str().unwrap(), 2).await;

    client
        .send(json!({
            "jsonrpc": "2.0", "id": 3, "method": "session/prompt",
            "params": { "sessionId": session_id, "prompt": [{ "type": "text", "text": "write gated.txt" }] }
        }))
        .await;

    // Read the agent's `session/request_permission` request, validate it, and answer it.
    let mut permission_id: Option<Value> = None;
    let mut permission_tool_call_id: Option<String> = None;
    let prompt_response = loop {
        let frame = client.next_frame().await;
        if frame.get("method").and_then(Value::as_str) == Some("session/request_permission") {
            let params = frame["params"].clone();
            assert_matches_schema("RequestPermissionRequest", &params);
            assert_eq!(params["sessionId"], json!(session_id));
            assert!(params["toolCall"]["toolCallId"].is_string());
            permission_tool_call_id = params["toolCall"]["toolCallId"].as_str().map(String::from);
            let option_ids: Vec<String> = params["options"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|o| o["optionId"].as_str().map(String::from))
                .collect();
            assert!(option_ids.contains(&"allow_once".to_string()));
            assert!(option_ids.contains(&"reject_once".to_string()));
            permission_id = frame.get("id").cloned();
            client
                .send(json!({
                    "jsonrpc": "2.0",
                    "id": frame["id"].clone(),
                    "result": { "outcome": { "outcome": "selected", "optionId": "allow_once" } }
                }))
                .await;
            continue;
        }
        if frame.get("id").and_then(Value::as_i64) == Some(3) {
            break frame;
        }
        client.seen.push(frame);
    };

    assert!(permission_id.is_some(), "no permission request was issued");
    // P2-1: the permission request's `toolCallId` must name a `tool_call` the client already
    // received, so an id-matching client can associate the prompt with the pending call.
    let announced: Vec<String> = client
        .take_seen()
        .iter()
        .filter_map(|frame| {
            if frame.get("method").and_then(Value::as_str) != Some("session/update") {
                return None;
            }
            let update = &frame["params"]["update"];
            if update["sessionUpdate"] != json!("tool_call") {
                return None;
            }
            update["toolCallId"].as_str().map(String::from)
        })
        .collect();
    let permission_tool_call_id = permission_tool_call_id.expect("permission names a tool call");
    // P2-1: the permission request must reuse the kernel tool-call id (the provider's `call_write_1`),
    // not a synthetic approval number, so a client can match it to the tool call it tracks.
    assert_eq!(
        permission_tool_call_id, "call_write_1",
        "the permission request must carry the kernel toolCallId"
    );
    assert!(
        announced.contains(&permission_tool_call_id),
        "permission toolCallId {permission_tool_call_id:?} must be an already-announced tool_call id; announced {announced:?}"
    );
    let result = prompt_response["result"].clone();
    assert_matches_schema("PromptResponse", &result);
    assert_eq!(result["stopReason"], json!("end_turn"));
    assert_eq!(
        fs::read_to_string(ws.join("gated.txt")).unwrap(),
        "approved\n"
    );
}

#[tokio::test]
async fn acp_cancel_notification_yields_cancelled_stop_reason() {
    let root = temp_root("cancel");
    let ws = root.join("ws");
    fs::create_dir_all(&ws).unwrap();
    let provider: Arc<dyn ModelProvider> = Arc::new(SlowProvider {
        delay: Duration::from_secs(30),
    });
    let cfg = KromaRuntimeConfig::new(&ws, root.join("state"), PolicyMode::FullAutonomous)
        .with_surface("acp", "stdio")
        .with_acp_protocol(true);
    let dispatcher = RpcDispatcher::new(cfg, provider).unwrap();
    let mut client = AcpStream::spawn(dispatcher).await;

    initialize_handshake(&mut client).await;
    let session_id = new_session(&mut client, ws.to_str().unwrap(), 2).await;

    client
        .send(json!({
            "jsonrpc": "2.0", "id": 3, "method": "session/prompt",
            "params": { "sessionId": session_id, "prompt": [{ "type": "text", "text": "long task" }] }
        }))
        .await;
    // Let the prompt register its cancellation channel, then cancel (a notification, so no id).
    tokio::time::sleep(Duration::from_millis(150)).await;
    client
        .send(json!({
            "jsonrpc": "2.0",
            "method": "session/cancel",
            "params": { "sessionId": session_id }
        }))
        .await;

    let resp = client.response(3).await;
    assert!(
        resp.get("error").is_none(),
        "cancelled prompt errored: {:?}",
        resp
    );
    let result = resp["result"].clone();
    assert_matches_schema("PromptResponse", &result);
    assert_eq!(result["stopReason"], json!("cancelled"));
}
