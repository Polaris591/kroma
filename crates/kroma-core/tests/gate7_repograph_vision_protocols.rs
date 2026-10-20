//! Gate 7 (`v1.2.0`) acceptance tests: symbol dependency graph navigation, native multimodal
//! vision input, and the Anthropic Messages / OpenAI Responses wire protocols.
//!
//! Each test pins one frontier capability that makes `v1.2.0` worth shipping:
//!
//! 1. **`code_graph` / RepoGraph.** Cross-file symbol references become a weighted directed graph
//!    ranked by Personalized PageRank, so a query for a symbol name surfaces the file that defines
//!    it plus its callers and callees instead of forcing the model to grep-and-read.
//! 2. **Multimodal `read`.** PNG/JPEG/GIF/WebP files are sniffed by magic bytes, their exact pixel
//!    dimensions parsed in pure Rust, and the payload attached as base64 — while arbitrary binary
//!    files stay rejected.
//! 3. **Anthropic Messages.** `x-api-key` + `anthropic-version` + prompt-caching beta headers,
//!    `cache_control: {"type": "ephemeral"}` breakpoints on the system block, the last tool and the
//!    frozen-prefix boundary, `tool_use`/`tool_result` translation, and SSE assembly that maps
//!    `cache_read_input_tokens` into the cache-hit metric.
//! 4. **OpenAI Responses.** `instructions` + typed `input` items, `function_call` /
//!    `function_call_output` round-tripping, streaming argument assembly and
//!    `input_tokens_details.cached_tokens` accounting.

use kroma_core::messages::{
    AssistantTurn, CacheBreakpoint, CacheBreakpointMarker, FinishReason, ImageAttachment, ToolCall,
    ToolOutcome, ToolResultMessage, ToolResultMeta, TranscriptItem, UserMessage,
};
use kroma_core::provider::anthropic_messages::{
    parse_anthropic_response, serialize_anthropic_request_body, AnthropicMessagesClient,
    AnthropicMessagesConfig,
};
use kroma_core::provider::openai_responses::{
    parse_responses_response, serialize_responses_request_body, OpenAiResponsesClient,
    OpenAiResponsesConfig,
};
use kroma_core::provider::{
    build_provider, ModelProvider, ProviderError, ProviderProtocol, ProviderRequest,
    ProviderSettings,
};
use kroma_core::repo_graph::{
    execute_code_graph, extract_reference_tokens, RepoGraph, MAX_GRAPH_FILES,
};
use kroma_core::tool::definition::builtin_tool_registrations;
use kroma_core::tools::read::{base64_encode, execute_read, sniff_image};
use kroma_core::types::{EffectClass, ToolCallId};
use kroma_core::workspace::WorkspaceCap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use uuid::Uuid;

// ---------------------------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------------------------

fn temp_root(tag: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("kroma-gate7-{}-{}", tag, Uuid::new_v4()));
    fs::create_dir_all(&path).unwrap();
    path
}

fn write_file(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, content).unwrap();
}

fn write_bytes(root: &Path, rel: &str, content: &[u8]) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, content).unwrap();
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// A single-request mock HTTP server. It records the raw request text and replies with a fixed
/// response, so the test can assert on the exact wire format the client produced.
async fn spawn_mock_http_server(
    status_line: &'static str,
    extra_headers: Vec<(&'static str, String)>,
    body: String,
) -> (String, tokio::sync::mpsc::UnboundedReceiver<String>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let tx = tx.clone();
            let body = body.clone();
            let extra_headers = extra_headers.clone();
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf: Vec<u8> = Vec::new();
                let mut chunk = [0u8; 4096];
                loop {
                    let n = match sock.read(&mut chunk).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => n,
                    };
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buf[..pos]).to_string();
                        let content_length = head
                            .lines()
                            .find_map(|line| {
                                let (k, v) = line.split_once(':')?;
                                if k.eq_ignore_ascii_case("content-length") {
                                    v.trim().parse::<usize>().ok()
                                } else {
                                    None
                                }
                            })
                            .unwrap_or(0);
                        if buf.len() >= pos + 4 + content_length {
                            break;
                        }
                    }
                }
                let _ = tx.send(String::from_utf8_lossy(&buf).to_string());

                let mut response = format!("{}\r\n", status_line);
                for (name, value) in &extra_headers {
                    response.push_str(&format!("{}: {}\r\n", name, value));
                }
                response.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));
                let _ = sock.write_all(response.as_bytes()).await;
                let _ = sock.write_all(body.as_bytes()).await;
                let _ = sock.flush().await;
                let _ = sock.shutdown().await;
            });
        }
    });
    (format!("http://{}", addr), rx)
}

async fn recv_request(rx: &mut tokio::sync::mpsc::UnboundedReceiver<String>) -> String {
    tokio::time::timeout(Duration::from_secs(10), rx.recv())
        .await
        .expect("mock server received no request")
        .expect("mock server channel closed")
}

fn request_body(raw: &str) -> serde_json::Value {
    let (_, body) = raw.split_once("\r\n\r\n").expect("request had no body");
    serde_json::from_str(body).expect("request body was not valid JSON")
}

// ---------------------------------------------------------------------------------------------
// 1. RepoGraph
// ---------------------------------------------------------------------------------------------

#[test]
fn test_repo_graph_symbol_extraction_cross_file_edges_and_pagerank_ranking() {
    let root = temp_root("repograph");
    write_file(
        &root,
        "src/db.rs",
        "pub struct Session { pub id: u64 }\n\npub fn lookup_session(id: u64) -> Option<Session> {\n    if id == 0 { None } else { Some(Session { id }) }\n}\n",
    );
    write_file(
        &root,
        "src/auth.rs",
        "use crate::db::Session;\nuse crate::db::lookup_session;\n\npub fn authenticate_user(token: &str) -> Result<Session, String> {\n    let id = token.len() as u64;\n    match lookup_session(id) {\n        Some(session) => Ok(session),\n        None => Err(\"unknown token\".to_string()),\n    }\n}\n",
    );
    write_file(
        &root,
        "src/router.rs",
        "use crate::auth::authenticate_user;\n\npub fn route_request(token: &str) -> Result<u64, String> {\n    let session = authenticate_user(token)?;\n    Ok(session.id)\n}\n",
    );
    write_file(
        &root,
        "src/unrelated.rs",
        "pub fn compute_checksum(bytes: &[u8]) -> u64 {\n    let mut acc: u64 = 1469598103934665603;\n    for byte in bytes {\n        acc ^= *byte as u64;\n        acc = acc.wrapping_mul(1099511628211);\n    }\n    acc\n}\n",
    );

    let cap = WorkspaceCap::new(&root).unwrap();
    let graph = RepoGraph::build_from_workspace(&cap, MAX_GRAPH_FILES);
    assert_eq!(
        graph.total_files(),
        4,
        "all four source files must be indexed"
    );
    assert!(
        graph
            .symbols()
            .iter()
            .any(|s| s.name == "authenticate_user" && s.file_path == "src/auth.rs"),
        "authenticate_user must be extracted as a definition in src/auth.rs"
    );
    assert!(
        graph
            .symbols()
            .iter()
            .any(|s| s.name == "lookup_session" && s.file_path == "src/db.rs"),
        "lookup_session must be extracted as a definition in src/db.rs"
    );

    // Cross-file reference edges: auth -> db (lookup_session) and router -> auth (authenticate_user).
    assert!(
        graph.edges().iter().any(|e| e.from == "src/auth.rs"
            && e.to == "src/db.rs"
            && e.symbol == "lookup_session"),
        "auth.rs must depend on db.rs through lookup_session, edges={:?}",
        graph.edges()
    );
    assert!(
        graph.edges().iter().any(|e| e.from == "src/router.rs"
            && e.to == "src/auth.rs"
            && e.symbol == "authenticate_user"),
        "router.rs must depend on auth.rs through authenticate_user"
    );

    // Personalized PageRank: a query for the defining symbol must put auth.rs first and expose both
    // directions of the call chain.
    let report = graph.query(&["authenticate_user".to_string()], &[], 4);
    assert_eq!(
        report.ranked[0].path,
        "src/auth.rs",
        "the defining file must rank first, got {:?}",
        report
            .ranked
            .iter()
            .map(|r| (&r.path, r.score))
            .collect::<Vec<_>>()
    );
    assert!(
        report.ranked[0]
            .calls_into
            .contains(&"src/db.rs".to_string()),
        "auth.rs must list db.rs as a callee, got {:?}",
        report.ranked[0].calls_into
    );
    assert!(
        report.ranked[0]
            .called_by
            .contains(&"src/router.rs".to_string()),
        "auth.rs must list router.rs as a caller, got {:?}",
        report.ranked[0].called_by
    );
    assert!(
        report.ranked.iter().any(|r| r.path == "src/router.rs"),
        "router.rs must appear in the top ranks for authenticate_user"
    );
    assert!(
        report
            .ranked
            .iter()
            .all(|r| r.path != "src/unrelated.rs" || report.ranked.len() > 3),
        "unrelated.rs must not outrank the auth call chain"
    );
    assert!(
        report.ranked[0]
            .symbols
            .iter()
            .any(|s| s.name == "authenticate_user" && s.kind.as_str() == "fn"),
        "the ranked file must carry its symbol signatures"
    );

    // Cold start (no query, no focus files) reduces to degree centrality: connected files first.
    let cold = graph.query(&[], &[], 4);
    assert_eq!(cold.ranked.len(), 4);
    assert_ne!(
        cold.ranked[0].path, "src/unrelated.rs",
        "degree centrality must not rank the isolated file first"
    );

    // The kernel tool renders the same report and is registered read-only.
    let tool_out = execute_code_graph(&cap, r#"{"query":"authenticate_user","max_nodes":3}"#);
    assert!(
        tool_out.outcome.is_success(),
        "code_graph failed: {}",
        tool_out.content
    );
    assert!(
        tool_out.content.contains("rank 1: src/auth.rs"),
        "{}",
        tool_out.content
    );
    assert!(
        tool_out.content.contains("RepoGraph |"),
        "{}",
        tool_out.content
    );

    let registered = builtin_tool_registrations()
        .into_iter()
        .find(|t| t.definition.name == "code_graph")
        .expect("code_graph must be a built-in kernel tool");
    assert_eq!(registered.metadata.effect_class, EffectClass::ReadOnly);

    // `focus_files` personalisation must accept only workspace-relative paths.
    let bad = execute_code_graph(&cap, r#"{"focus_files":["../etc/passwd"]}"#);
    assert!(!bad.outcome.is_success(), "path escape must be rejected");

    fs::remove_dir_all(&root).ok();
}

// ---------------------------------------------------------------------------------------------
// 2. Multimodal read
// ---------------------------------------------------------------------------------------------

fn png_header(width: u32, height: u32, padding: usize) -> Vec<u8> {
    let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
    bytes.extend_from_slice(&13u32.to_be_bytes()); // IHDR length
    bytes.extend_from_slice(b"IHDR");
    bytes.extend_from_slice(&width.to_be_bytes());
    bytes.extend_from_slice(&height.to_be_bytes());
    bytes.extend_from_slice(&[8, 6, 0, 0, 0]); // bit depth, colour type, compression, filter, interlace
    bytes.extend_from_slice(&[0u8; 4]); // CRC placeholder
    bytes.extend(std::iter::repeat_n(0xABu8, padding));
    bytes
}

fn jpeg_with_dimensions(width: u16, height: u16) -> Vec<u8> {
    let mut bytes = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10];
    bytes.extend_from_slice(b"JFIF\0");
    bytes.extend_from_slice(&[0x01, 0x01, 0x00, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00]);
    bytes.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x11, 0x08]);
    bytes.extend_from_slice(&height.to_be_bytes());
    bytes.extend_from_slice(&width.to_be_bytes());
    bytes.extend_from_slice(&[0x03, 0x01, 0x11, 0x00, 0x02, 0x11, 0x01, 0x03, 0x11, 0x01]);
    bytes
}

#[test]
fn test_multimodal_read_png_jpeg_header_dimensions_and_base64_attachment() {
    let root = temp_root("vision");
    let png = png_header(1280, 720, 64);
    write_bytes(&root, "shot.png", &png);
    let jpeg = jpeg_with_dimensions(640, 480);
    write_bytes(&root, "mockup.jpg", &jpeg);
    // A file that claims to be a PNG but is arbitrary binary must never become an attachment.
    write_bytes(&root, "fake.png", &[0x00, 0x01, 0x02, 0x03]);
    write_bytes(&root, "blob.bin", &[0x00, 0x01, 0x02]);

    let cap = WorkspaceCap::new(&root).unwrap();

    let out = execute_read(&cap, r#"{"path":"shot.png"}"#);
    assert!(out.outcome.is_success(), "png read failed: {}", out.content);
    assert_eq!(out.image_attachments.len(), 1);
    let attachment = &out.image_attachments[0];
    assert_eq!(attachment.mime_type, "image/png");
    assert_eq!(attachment.width, Some(1280));
    assert_eq!(attachment.height, Some(720));
    assert_eq!(attachment.byte_len, png.len());
    assert_eq!(&*attachment.base64_data, base64_encode(&png));
    assert!(out.content.contains("image/png"), "{}", out.content);
    assert!(out.content.contains("1280x720"), "{}", out.content);

    let jpeg_out = execute_read(&cap, r#"{"path":"mockup.jpg"}"#);
    assert!(jpeg_out.outcome.is_success(), "{}", jpeg_out.content);
    let jpeg_attachment = &jpeg_out.image_attachments[0];
    assert_eq!(jpeg_attachment.mime_type, "image/jpeg");
    assert_eq!(jpeg_attachment.width, Some(640));
    assert_eq!(jpeg_attachment.height, Some(480));

    // Header parsing must be exact and independent of the payload length.
    assert_eq!(sniff_image(&png).unwrap().dimensions, (1280, 720));
    assert_eq!(sniff_image(&jpeg).unwrap().dimensions, (640, 480));
    let gif = {
        let mut b = b"GIF89a".to_vec();
        b.extend_from_slice(&320u16.to_le_bytes());
        b.extend_from_slice(&240u16.to_le_bytes());
        b.extend_from_slice(&[0u8; 16]);
        b
    };
    assert_eq!(sniff_image(&gif).unwrap().dimensions, (320, 240));
    let webp = {
        let mut b = b"RIFF".to_vec();
        b.extend_from_slice(&[0, 0, 0, 0]);
        b.extend_from_slice(b"WEBP");
        b.extend_from_slice(b"VP8X");
        b.extend_from_slice(&[10, 0, 0, 0]);
        // 1 flags byte + 3 reserved bytes.
        b.extend_from_slice(&[0u8; 4]);
        let w = 1023u32; // width-1
        let h = 767u32; // height-1
        b.extend_from_slice(&[w as u8, (w >> 8) as u8, (w >> 16) as u8]);
        b.extend_from_slice(&[h as u8, (h >> 8) as u8, (h >> 16) as u8]);
        b
    };
    assert_eq!(sniff_image(&webp).unwrap().dimensions, (1024, 768));

    // Arbitrary binary stays rejected, including a binary file masquerading as an image.
    let binary = execute_read(&cap, r#"{"path":"blob.bin"}"#);
    assert!(!binary.outcome.is_success());
    assert!(binary.image_attachments.is_empty());

    let masquerade = execute_read(&cap, r#"{"path":"fake.png"}"#);
    assert!(
        !masquerade.outcome.is_success(),
        "a .png whose magic bytes are not PNG must be rejected as binary"
    );
    assert!(masquerade.image_attachments.is_empty());

    // Attachments travel with the ToolResultMessage and are counted in the context budget.
    let msg = ToolResultMessage {
        call_id: ToolCallId::new("call_img"),
        tool_name: "read".to_string(),
        outcome: ToolOutcome::Success,
        content: Arc::from(out.content.as_str()),
        meta: ToolResultMeta {
            relative_path: Some("shot.png".to_string()),
            image_attachments: out.image_attachments.clone(),
            ..Default::default()
        },
    };
    assert_eq!(msg.image_attachments().len(), 1);
    assert_eq!(msg.image_attachment_bytes(), attachment.encoded_len());
    let item = TranscriptItem::ToolResult(msg);
    assert!(item.estimated_bytes() > attachment.encoded_len());

    // The OpenAI-compatible serializer must emit a multimodal `image_url` content part for the
    // attachment instead of flattening it into text.
    let wire = kroma_core::provider::chat_completions::serialize_openai_request_body(
        "m",
        "sys",
        &[],
        std::slice::from_ref(&item),
        false,
    );
    let wire_json: serde_json::Value = serde_json::from_str(&wire).unwrap();
    let tool_message = &wire_json["messages"][1];
    assert_eq!(tool_message["role"], "tool");
    assert_eq!(tool_message["content"][0]["type"], "text");
    assert_eq!(tool_message["content"][1]["type"], "image_url");
    assert!(tool_message["content"][1]["image_url"]["url"]
        .as_str()
        .unwrap()
        .starts_with("data:image/png;base64,"));

    // The Anthropic serializer uses its own `image` block shape.
    let anthropic_wire = kroma_core::provider::anthropic_messages::serialize_anthropic_request_body(
        "m",
        "sys",
        &[],
        std::slice::from_ref(&item),
        &[],
        false,
        1024,
    );
    let anthropic_json: serde_json::Value = serde_json::from_str(&anthropic_wire).unwrap();
    assert_eq!(
        anthropic_json["messages"][0]["content"][0]["content"][1]["type"],
        "image"
    );
    assert_eq!(
        anthropic_json["messages"][0]["content"][0]["content"][1]["source"]["type"],
        "base64"
    );

    // Base64 must be standard, padded RFC 4648.
    assert_eq!(base64_encode(b"Man"), "TWFu");
    assert_eq!(base64_encode(b"Ma"), "TWE=");
    assert_eq!(base64_encode(b"M"), "TQ==");

    fs::remove_dir_all(&root).ok();
}

// ---------------------------------------------------------------------------------------------
// 3. Anthropic Messages
// ---------------------------------------------------------------------------------------------

fn anthropic_sse_body() -> String {
    let events = [
        (
            "message_start",
            r#"{"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","content":[],"usage":{"input_tokens":100,"output_tokens":1,"cache_read_input_tokens":1024,"cache_creation_input_tokens":50}}}"#,
        ),
        (
            "content_block_start",
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        ),
        (
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Checking auth"}}"#,
        ),
        (
            "content_block_stop",
            r#"{"type":"content_block_stop","index":0}"#,
        ),
        (
            "content_block_start",
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"read"}}"#,
        ),
        (
            "content_block_delta",
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"path\":"}}"#,
        ),
        (
            "content_block_delta",
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"\"auth.rs\"}"}}"#,
        ),
        (
            "content_block_stop",
            r#"{"type":"content_block_stop","index":1}"#,
        ),
        (
            "message_delta",
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":42}}"#,
        ),
        ("message_stop", r#"{"type":"message_stop"}"#),
    ];
    let mut body = String::new();
    for (name, data) in events {
        body.push_str(&format!("event: {}\ndata: {}\n\n", name, data));
    }
    body
}

/// BUG-05: `redacted_thinking` blocks carry an encrypted payload in `data`, and a `thinking`
/// block's signature can be split across several `signature_delta` events.
fn anthropic_reasoning_sse_body() -> String {
    let events = [
        (
            "message_start",
            r#"{"type":"message_start","message":{"id":"msg_2","type":"message","role":"assistant","content":[],"usage":{"input_tokens":10,"output_tokens":1}}}"#,
        ),
        (
            "content_block_start",
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"redacted_thinking","data":"ENCRYPTED_PAYLOAD"}}"#,
        ),
        (
            "content_block_stop",
            r#"{"type":"content_block_stop","index":0}"#,
        ),
        (
            "content_block_start",
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"thinking","thinking":""}}"#,
        ),
        (
            "content_block_delta",
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"thinking_delta","thinking":"step 1"}}"#,
        ),
        (
            "content_block_delta",
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"signature_delta","signature":"AAA"}}"#,
        ),
        (
            "content_block_delta",
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"signature_delta","signature":"BBB"}}"#,
        ),
        (
            "content_block_stop",
            r#"{"type":"content_block_stop","index":1}"#,
        ),
        (
            "message_delta",
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":3}}"#,
        ),
        ("message_stop", r#"{"type":"message_stop"}"#),
    ];
    let mut body = String::new();
    for (name, data) in events {
        body.push_str(&format!("event: {}\ndata: {}\n\n", name, data));
    }
    body
}

#[tokio::test]
async fn test_anthropic_stream_assembles_redacted_thinking_and_concatenates_split_signatures() {
    let (base_url, mut rx) = spawn_mock_http_server(
        "HTTP/1.1 200 OK",
        vec![("Content-Type", "text/event-stream".to_string())],
        anthropic_reasoning_sse_body(),
    )
    .await;

    let client = AnthropicMessagesClient::new(AnthropicMessagesConfig {
        base_url,
        api_key: "sk-ant-test-key".to_string(),
        model: "claude-sonnet-4-5".to_string(),
        max_retries: 0,
        max_tokens: 4096,
    })
    .unwrap();

    let req = ProviderRequest {
        system_prompt: "sys".to_string(),
        tools: Vec::new(),
        messages: vec![TranscriptItem::User(UserMessage {
            content: "hi".to_string(),
        })],
        stream: true,
        cache_breakpoints: Vec::new(),
        max_output_tokens: None,
        retry_budget: None,
        wire_attempts: None,
    };
    let (_tx, cancel_rx) = watch::channel(false);
    let completion = client
        .complete(req, &kroma_core::budget::BudgetConfig::default(), cancel_rx)
        .await
        .expect("anthropic reasoning stream should succeed");
    let _ = recv_request(&mut rx).await;

    let turn = completion.turn;
    assert_eq!(
        turn.reasoning_blocks.len(),
        2,
        "both reasoning blocks must survive: {:?}",
        turn.reasoning_blocks
    );
    let redacted = &turn.reasoning_blocks[0];
    assert_eq!(
        redacted.opaque_payload.as_deref(),
        Some("ENCRYPTED_PAYLOAD")
    );
    assert!(redacted.text.is_none());
    let thinking = &turn.reasoning_blocks[1];
    assert_eq!(thinking.text.as_deref(), Some("step 1"));
    assert_eq!(
        thinking.signature.as_deref(),
        Some("AAABBB"),
        "a signature split across events must be concatenated, not overwritten"
    );
    assert_eq!(turn.finish_reason, FinishReason::Stop);
}

#[tokio::test]
async fn test_anthropic_messages_protocol_cache_control_breakpoints_and_sse_stream() {
    let (base_url, mut rx) = spawn_mock_http_server(
        "HTTP/1.1 200 OK",
        vec![("Content-Type", "text/event-stream".to_string())],
        anthropic_sse_body(),
    )
    .await;

    let client = AnthropicMessagesClient::new(AnthropicMessagesConfig {
        base_url,
        api_key: "sk-ant-test-key".to_string(),
        model: "claude-sonnet-4-5".to_string(),
        max_retries: 0,
        max_tokens: 4096,
    })
    .unwrap();

    let tools = builtin_tool_registrations()
        .into_iter()
        .map(|t| t.definition)
        .take(3)
        .collect::<Vec<_>>();

    let messages = vec![
        TranscriptItem::User(UserMessage {
            content: "Audit the auth path".to_string(),
        }),
        TranscriptItem::Assistant(AssistantTurn {
            content: Some("Reading auth.rs".to_string()),
            reasoning_blocks: Vec::new(),
            tool_calls: vec![kroma_core::messages::ToolCall {
                call_id: ToolCallId::new("toolu_prev"),
                name: "read".to_string(),
                raw_arguments: r#"{"path":"auth.rs"}"#.to_string(),
            }],
            finish_reason: FinishReason::ToolCalls,
            usage: None,
        }),
        TranscriptItem::ToolResult(ToolResultMessage {
            call_id: ToolCallId::new("toolu_prev"),
            tool_name: "read".to_string(),
            outcome: ToolOutcome::Success,
            content: Arc::from("     1\tpub fn authenticate_user() {}"),
            meta: ToolResultMeta::default(),
        }),
    ];

    let req = ProviderRequest {
        system_prompt: "You are the kroma kernel.".to_string(),
        tools: tools.clone(),
        messages,
        stream: true,
        cache_breakpoints: vec![CacheBreakpointMarker {
            breakpoint: CacheBreakpoint::PinnedBaselineEnd,
            message_index: 0,
        }],
        max_output_tokens: None,
        retry_budget: None,
        wire_attempts: None,
    };

    let (_tx, cancel_rx) = watch::channel(false);
    let completion = client
        .complete(req, &kroma_core::budget::BudgetConfig::default(), cancel_rx)
        .await
        .expect("anthropic stream should succeed");

    let raw = recv_request(&mut rx).await;
    assert!(
        raw.starts_with("POST /v1/messages HTTP/1.1"),
        "unexpected request line: {}",
        raw.lines().next().unwrap_or("")
    );
    assert!(raw.contains("x-api-key: sk-ant-test-key"), "{}", raw);
    assert!(raw.contains("anthropic-version: 2023-06-01"), "{}", raw);
    assert!(
        raw.contains("anthropic-beta: prompt-caching-2024-07-31"),
        "{}",
        raw
    );

    let body = request_body(&raw);
    assert_eq!(body["model"], "claude-sonnet-4-5");
    assert_eq!(body["max_tokens"], 4096);
    assert_eq!(body["stream"], true);

    // Breakpoint 1: the static system block.
    assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
    // Breakpoint 2: the last tool definition only.
    let wire_tools = body["tools"].as_array().unwrap();
    assert_eq!(wire_tools.len(), tools.len());
    assert!(wire_tools[0].get("cache_control").is_none());
    assert_eq!(
        wire_tools.last().unwrap()["cache_control"]["type"],
        "ephemeral"
    );
    // Breakpoint 3: the frozen-prefix boundary message.
    assert_eq!(
        body["messages"][0]["content"][0]["cache_control"]["type"],
        "ephemeral"
    );

    // `tool_use` / `tool_result` translation.
    assert_eq!(body["messages"][1]["content"][1]["type"], "tool_use");
    assert_eq!(body["messages"][1]["content"][1]["id"], "toolu_prev");
    assert_eq!(body["messages"][2]["content"][0]["type"], "tool_result");
    assert_eq!(
        body["messages"][2]["content"][0]["tool_use_id"],
        "toolu_prev"
    );

    // SSE assembly.
    let turn = completion.turn;
    assert_eq!(turn.content.as_deref(), Some("Checking auth"));
    assert_eq!(turn.finish_reason, FinishReason::ToolCalls);
    assert_eq!(turn.tool_calls.len(), 1);
    assert_eq!(turn.tool_calls[0].name, "read");
    assert_eq!(turn.tool_calls[0].raw_arguments, r#"{"path":"auth.rs"}"#);
    let usage = turn.usage.expect("usage must be parsed");
    assert_eq!(usage.cached_input_tokens, Some(1024));
    assert_eq!(usage.input_tokens, 100 + 1024 + 50);
    assert_eq!(usage.output_tokens, 42);
    assert!((usage.prefix_cache_hit_ratio() - 0.8713).abs() < 0.01);

    // Non-streaming parsing shares the same translation rules.
    let parsed = parse_anthropic_response(
        r#"{"type":"message","role":"assistant","content":[{"type":"text","text":"ok"},{"type":"tool_use","id":"toolu_9","name":"grep","input":{"pattern":"openat2"}}],"stop_reason":"tool_use","usage":{"input_tokens":10,"output_tokens":5,"cache_read_input_tokens":90}}"#,
    )
    .unwrap();
    assert_eq!(parsed.content.as_deref(), Some("ok"));
    assert_eq!(parsed.tool_calls[0].name, "grep");
    assert_eq!(
        parsed.tool_calls[0].raw_arguments,
        r#"{"pattern":"openat2"}"#
    );
    assert_eq!(parsed.finish_reason, FinishReason::ToolCalls);
    assert_eq!(parsed.usage.unwrap().input_tokens, 100);

    // The serializer is deterministic for identical input.
    let a = serialize_anthropic_request_body("m", "sys", &tools, &[], &[], true, 1024);
    let b = serialize_anthropic_request_body("m", "sys", &tools, &[], &[], true, 1024);
    assert_eq!(a, b);
}

// ---------------------------------------------------------------------------------------------
// 4. OpenAI Responses
// ---------------------------------------------------------------------------------------------

fn responses_sse_body() -> String {
    let events = [
        (
            "response.created",
            r#"{"type":"response.created","response":{"id":"resp_1","status":"in_progress"}}"#,
        ),
        (
            "response.output_item.added",
            r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","call_id":"call_1","name":"grep","arguments":""}}"#,
        ),
        (
            "response.function_call_arguments.delta",
            r#"{"type":"response.function_call_arguments.delta","output_index":0,"delta":"{\"pattern\":"}"#,
        ),
        (
            "response.function_call_arguments.delta",
            r#"{"type":"response.function_call_arguments.delta","output_index":0,"delta":"\"openat2\"}"}"#,
        ),
        (
            "response.function_call_arguments.done",
            r#"{"type":"response.function_call_arguments.done","output_index":0}"#,
        ),
        (
            "response.output_text.delta",
            r#"{"type":"response.output_text.delta","delta":"Searching"}"#,
        ),
        (
            "response.completed",
            r#"{"type":"response.completed","response":{"id":"resp_1","status":"completed","usage":{"input_tokens":500,"output_tokens":20,"input_tokens_details":{"cached_tokens":384}}}}"#,
        ),
    ];
    let mut body = String::new();
    for (name, data) in events {
        body.push_str(&format!("event: {}\ndata: {}\n\n", name, data));
    }
    body
}

#[tokio::test]
async fn test_openai_responses_protocol_roundtrip_and_function_call() {
    let (base_url, mut rx) = spawn_mock_http_server(
        "HTTP/1.1 200 OK",
        vec![("Content-Type", "text/event-stream".to_string())],
        responses_sse_body(),
    )
    .await;

    let client = OpenAiResponsesClient::new(OpenAiResponsesConfig {
        base_url,
        api_key: "sk-openai-test".to_string(),
        model: "gpt-5.2".to_string(),
        max_retries: 0,
        reasoning_effort: Some("medium".to_string()),
    })
    .unwrap();

    let tools = builtin_tool_registrations()
        .into_iter()
        .map(|t| t.definition)
        .take(2)
        .collect::<Vec<_>>();

    let messages = vec![
        TranscriptItem::User(UserMessage {
            content: "Find the sandbox".to_string(),
        }),
        TranscriptItem::Assistant(AssistantTurn {
            content: None,
            reasoning_blocks: Vec::new(),
            tool_calls: vec![kroma_core::messages::ToolCall {
                call_id: ToolCallId::new("call_prev"),
                name: "read".to_string(),
                raw_arguments: r#"{"path":"src/lib.rs"}"#.to_string(),
            }],
            finish_reason: FinishReason::ToolCalls,
            usage: None,
        }),
        TranscriptItem::ToolResult(ToolResultMessage {
            call_id: ToolCallId::new("call_prev"),
            tool_name: "read".to_string(),
            outcome: ToolOutcome::Success,
            content: Arc::from("     1\tfn main() {}"),
            meta: ToolResultMeta::default(),
        }),
    ];

    let req = ProviderRequest {
        system_prompt: "You are the kroma kernel.".to_string(),
        tools: tools.clone(),
        messages,
        stream: true,
        cache_breakpoints: Vec::new(),
        max_output_tokens: None,
        retry_budget: None,
        wire_attempts: None,
    };

    let (_tx, cancel_rx) = watch::channel(false);
    let completion = client
        .complete(req, &kroma_core::budget::BudgetConfig::default(), cancel_rx)
        .await
        .expect("responses stream should succeed");

    let raw = recv_request(&mut rx).await;
    assert!(
        raw.starts_with("POST /v1/responses HTTP/1.1"),
        "unexpected request line: {}",
        raw.lines().next().unwrap_or("")
    );
    assert!(
        raw.contains("Authorization: Bearer sk-openai-test"),
        "{}",
        raw
    );

    let body = request_body(&raw);
    assert_eq!(body["model"], "gpt-5.2");
    assert_eq!(body["instructions"], "You are the kroma kernel.");
    assert_eq!(body["stream"], true);
    assert_eq!(body["store"], false);
    assert_eq!(body["reasoning"]["effort"], "medium");

    let input = body["input"].as_array().unwrap();
    assert_eq!(input[0]["role"], "user");
    assert_eq!(input[0]["content"][0]["type"], "input_text");
    assert_eq!(input[1]["type"], "function_call");
    assert_eq!(input[1]["call_id"], "call_prev");
    assert_eq!(input[1]["name"], "read");
    assert_eq!(input[2]["type"], "function_call_output");
    assert_eq!(input[2]["call_id"], "call_prev");
    assert_eq!(body["tools"][0]["type"], "function");
    assert!(body["tools"][0].get("name").is_some());

    let turn = completion.turn;
    assert_eq!(turn.content.as_deref(), Some("Searching"));
    assert_eq!(turn.finish_reason, FinishReason::ToolCalls);
    assert_eq!(turn.tool_calls.len(), 1);
    assert_eq!(turn.tool_calls[0].name, "grep");
    assert_eq!(turn.tool_calls[0].raw_arguments, r#"{"pattern":"openat2"}"#);
    let usage = turn.usage.expect("usage must be parsed");
    assert_eq!(usage.cached_input_tokens, Some(384));
    assert_eq!(usage.input_tokens, 500);

    let parsed = parse_responses_response(
        r#"{"status":"completed","output":[{"type":"message","content":[{"type":"output_text","text":"done"}]},{"type":"function_call","call_id":"call_9","name":"ls","arguments":"{}"}],"usage":{"input_tokens":12,"output_tokens":3,"input_tokens_details":{"cached_tokens":8}}}"#,
    )
    .unwrap();
    assert_eq!(parsed.content.as_deref(), Some("done"));
    assert_eq!(parsed.tool_calls[0].call_id.as_str(), "call_9");
    assert_eq!(parsed.usage.unwrap().cached_input_tokens, Some(8));

    let serialized = serialize_responses_request_body("m", "s", &tools, &[], true, None);
    assert!(serialized.contains("\"instructions\":\"s\""));
    assert!(!serialized.contains("reasoning"));
}

#[test]
fn test_protocol_selection_auto_detection_and_provider_factory() {
    assert_eq!(
        ProviderProtocol::parse("auto").unwrap(),
        ProviderProtocol::Auto
    );
    assert_eq!(
        ProviderProtocol::parse("anthropic").unwrap(),
        ProviderProtocol::AnthropicMessages
    );
    assert_eq!(
        ProviderProtocol::parse("openai-responses").unwrap(),
        ProviderProtocol::OpenAiResponses
    );
    assert!(ProviderProtocol::parse("nope").is_err());

    assert_eq!(
        ProviderProtocol::detect("https://api.anthropic.com", "claude-sonnet-4-5"),
        ProviderProtocol::AnthropicMessages
    );
    assert_eq!(
        ProviderProtocol::detect("https://api.openai.com/v1", "claude-opus-4-6"),
        ProviderProtocol::AnthropicMessages
    );
    assert_eq!(
        ProviderProtocol::detect("https://api.openai.com/v1", "gpt-5.2"),
        ProviderProtocol::ChatCompletions
    );
    assert_eq!(
        ProviderProtocol::detect("https://api.openai.com/v1/responses", "gpt-5.2"),
        ProviderProtocol::OpenAiResponses
    );

    // The factory must build every protocol and report which one it resolved.
    for (protocol, model, expected) in [
        (
            ProviderProtocol::ChatCompletions,
            "gpt-4o",
            ProviderProtocol::ChatCompletions,
        ),
        (
            ProviderProtocol::AnthropicMessages,
            "claude-sonnet-4-5",
            ProviderProtocol::AnthropicMessages,
        ),
        (
            ProviderProtocol::OpenAiResponses,
            "gpt-5.2",
            ProviderProtocol::OpenAiResponses,
        ),
        (
            ProviderProtocol::Auto,
            "claude-sonnet-4-5",
            ProviderProtocol::AnthropicMessages,
        ),
    ] {
        let settings = ProviderSettings {
            base_url: "https://api.example.com/v1".to_string(),
            api_key: "sk-test".to_string(),
            model: model.to_string(),
            max_retries: 0,
            reasoning_effort: None,
            protocol,
            max_tokens: 0,
        };
        let (resolved, _provider): (ProviderProtocol, Arc<dyn ModelProvider>) =
            build_provider(&settings).expect("factory must build every protocol");
        assert_eq!(resolved, expected);
    }
}

// ---------------------------------------------------------------------------------------------
// 5. v1.2.0 post-review regressions
//
// Each test pins a defect the 3-way review of `v1.2.0` found: a multimodal image that escaped
// Step-Down Compaction (and could abort the run), and truncated SSE streams that were accepted as
// complete turns in both new protocols.
// ---------------------------------------------------------------------------------------------

fn sse_body(events: &[(&str, &str)]) -> String {
    let mut body = String::new();
    for (name, data) in events {
        body.push_str(&format!("event: {}\ndata: {}\n\n", name, data));
    }
    body
}

fn single_user_request() -> ProviderRequest {
    ProviderRequest {
        system_prompt: "You are the kroma kernel.".to_string(),
        tools: Vec::new(),
        messages: vec![TranscriptItem::User(UserMessage {
            content: "hello".to_string(),
        })],
        stream: true,
        cache_breakpoints: Vec::new(),
        max_output_tokens: None,
        retry_budget: None,
        wire_attempts: None,
    }
}

// ---- RepoGraph: whitespace + CJK identifiers, deduplicated neighbours ------------------------

#[test]
fn test_repo_graph_multi_space_cjk_identifiers_and_deduplicated_neighbour_lists() {
    let root = temp_root("repograph-cjk");
    // Extra whitespace after `const` / `type` used to make `rest[name.len()..]` slice inside a
    // multi-byte CJK codepoint and panic (`byte index is not a char boundary`).
    write_file(
        &root,
        "src/a.ts",
        "const  函数 = () => {\n  共享符号();\n  另一个共享();\n};\n",
    );
    write_file(
        &root,
        "src/b.ts",
        "export function 共享符号() { return 1; }\nexport function 另一个共享() { return 2; }\n",
    );
    write_file(
        &root,
        "src/c.go",
        "package main\n\ntype  名称 struct {\n\tID int\n}\n",
    );

    let cap = WorkspaceCap::new(&root).unwrap();
    let graph = RepoGraph::build_from_workspace(&cap, MAX_GRAPH_FILES);
    assert_eq!(graph.total_files(), 3);

    let find = |name: &str, path: &str| {
        graph
            .symbols()
            .iter()
            .find(|s| s.name == name && s.file_path == path)
            .unwrap_or_else(|| {
                panic!(
                    "{} must be extracted as a definition in {}; got {:?}",
                    name,
                    path,
                    graph
                        .symbols()
                        .iter()
                        .map(|s| (&s.name, &s.file_path, s.kind.as_str()))
                        .collect::<Vec<_>>()
                )
            })
    };
    assert_eq!(find("函数", "src/a.ts").kind.as_str(), "fn");
    assert_eq!(find("共享符号", "src/b.ts").kind.as_str(), "fn");
    assert_eq!(find("另一个共享", "src/b.ts").kind.as_str(), "fn");
    assert_eq!(find("名称", "src/c.go").kind.as_str(), "struct");

    // a.ts references two symbols that both live in b.ts: one file pair, two raw edges. The
    // neighbour lists must aggregate them into a single entry per file.
    assert_eq!(
        graph
            .edges()
            .iter()
            .filter(|e| e.from == "src/a.ts" && e.to == "src/b.ts")
            .count(),
        2,
        "a.ts must carry one edge per referenced b.ts symbol"
    );

    let report = graph.query(&["共享符号".to_string()], &[], 3);
    let a = report
        .ranked
        .iter()
        .find(|r| r.path == "src/a.ts")
        .expect("src/a.ts must be ranked");
    assert_eq!(
        a.calls_into.iter().filter(|p| *p == "src/b.ts").count(),
        1,
        "calls_into must list each neighbour file once, got {:?}",
        a.calls_into
    );
    let b = report
        .ranked
        .iter()
        .find(|r| r.path == "src/b.ts")
        .expect("src/b.ts must be ranked");
    assert_eq!(
        b.called_by.iter().filter(|p| *p == "src/a.ts").count(),
        1,
        "called_by must list each neighbour file once, got {:?}",
        b.called_by
    );

    fs::remove_dir_all(&root).ok();
}

// ---- Context projection: image attachments must be compactable and never abort a run ---------

/// ~192 KiB of base64: far above `MIN_COMPACTABLE_TOOL_BYTES` and far above the test budget.
fn large_image_attachment() -> ImageAttachment {
    ImageAttachment {
        mime_type: "image/png".to_string(),
        base64_data: Arc::from("QUJDREVGR0g".repeat(24 * 1024).as_str()),
        width: Some(1280),
        height: Some(720),
        byte_len: 144 * 1024,
    }
}

fn assistant_turn_with_call(call_id: &str, args: &str, text: &str) -> AssistantTurn {
    AssistantTurn {
        content: Some(text.to_string()),
        reasoning_blocks: Vec::new(),
        tool_calls: vec![ToolCall {
            call_id: ToolCallId::new(call_id),
            name: "read".to_string(),
            raw_arguments: args.to_string(),
        }],
        finish_reason: FinishReason::ToolCalls,
        usage: None,
    }
}

fn image_read_result(call_id: &str, path: &str, image: ImageAttachment) -> ToolResultMessage {
    ToolResultMessage {
        call_id: ToolCallId::new(call_id),
        tool_name: "read".to_string(),
        outcome: ToolOutcome::Success,
        content: Arc::from(format!(
            "Read image file {} [image/png, 1280x720, 147456 bytes]\n",
            path
        )),
        meta: ToolResultMeta {
            relative_path: Some(path.to_string()),
            image_attachments: vec![image],
            ..Default::default()
        },
    }
}

fn text_read_result(call_id: &str, path: &str, body: &str) -> ToolResultMessage {
    ToolResultMessage {
        call_id: ToolCallId::new(call_id),
        tool_name: "read".to_string(),
        outcome: ToolOutcome::Success,
        content: Arc::from(body),
        meta: ToolResultMeta {
            relative_path: Some(path.to_string()),
            ..Default::default()
        },
    }
}

fn plain_assistant(text: &str) -> AssistantTurn {
    AssistantTurn {
        content: Some(text.to_string()),
        reasoning_blocks: Vec::new(),
        tool_calls: Vec::new(),
        finish_reason: FinishReason::Stop,
        usage: None,
    }
}

/// A budget small enough that one 192 KiB attachment always exceeds the high watermark.
fn tight_budget() -> kroma_core::budget::BudgetConfig {
    kroma_core::budget::BudgetConfig {
        context_high_watermark_bytes: 64 * 1024,
        context_low_watermark_bytes: 16 * 1024,
        context_tail_window_bytes: 512,
        context_min_tail_turns: 1,
        ..Default::default()
    }
}

#[test]
fn test_step_down_compaction_evicts_stale_image_attachments_and_reclaims_budget() {
    use kroma_core::context_projector::{ContextProjector, ProjectionProfile};

    let image = large_image_attachment();
    let image_bytes = image.encoded_len();

    let mut canonical = vec![
        TranscriptItem::User(UserMessage {
            content: "look at the screenshot".to_string(),
        }),
        TranscriptItem::Assistant(assistant_turn_with_call(
            "call_img",
            r#"{"path":"shot.png"}"#,
            "reading the screenshot",
        )),
        TranscriptItem::ToolResult(image_read_result("call_img", "shot.png", image.clone())),
        TranscriptItem::Assistant(assistant_turn_with_call(
            "call_txt",
            r#"{"path":"notes.md"}"#,
            "reading notes",
        )),
        TranscriptItem::ToolResult(text_read_result("call_txt", "notes.md", "notes body")),
        TranscriptItem::Assistant(plain_assistant("done")),
        TranscriptItem::User(UserMessage {
            content: "carry on".to_string(),
        }),
    ];
    let before: usize = canonical.iter().map(TranscriptItem::estimated_bytes).sum();

    let mut projector = ContextProjector::new().with_profile(ProjectionProfile::P2);
    let projected = projector
        .project(&mut canonical, &tight_budget(), 0, None, 3)
        .expect("compaction must keep the run alive");

    let compacted = match &projected.messages[2] {
        TranscriptItem::ToolResult(tr) => tr,
        other => panic!("expected a tool result at index 2, got {:?}", other),
    };
    assert!(
        compacted.meta.image_attachments.is_empty(),
        "a stale image in the compactable middle region must drop its base64 payload"
    );
    assert!(
        compacted.content.contains("source_entry_id="),
        "the Outline Stub must keep the evidence pointer: {}",
        compacted.content
    );
    match &canonical[2] {
        TranscriptItem::ToolResult(tr) => assert!(
            tr.meta.image_attachments.is_empty(),
            "the canonical transcript must be cleared in lockstep so the payload is really freed"
        ),
        other => panic!("expected a tool result, got {:?}", other),
    }

    assert!(
        before - projected.message_history_bytes >= image_bytes - 1024,
        "compaction must reclaim the image payload: before={} after={} image_bytes={}",
        before,
        projected.message_history_bytes,
        image_bytes
    );
    assert!(projector.stats().budget.compacted_stub_bytes > 0);
    assert!(projector.stats().budget.tool_output_bytes < image_bytes);
}

#[test]
fn test_multimodal_read_never_aborts_the_run_with_context_budget_exceeded() {
    use kroma_core::context_projector::{ContextProjector, ProjectionProfile};

    // The image sits in the protected tail window, so no text compaction can reach it.
    let mut canonical = vec![
        TranscriptItem::User(UserMessage {
            content: "look at the screenshot".to_string(),
        }),
        TranscriptItem::Assistant(assistant_turn_with_call(
            "call_txt",
            r#"{"path":"notes.md"}"#,
            "reading notes",
        )),
        TranscriptItem::ToolResult(text_read_result("call_txt", "notes.md", "notes body")),
        TranscriptItem::Assistant(assistant_turn_with_call(
            "call_img",
            r#"{"path":"shot.png"}"#,
            "reading the screenshot",
        )),
        TranscriptItem::ToolResult(image_read_result(
            "call_img",
            "shot.png",
            large_image_attachment(),
        )),
        TranscriptItem::User(UserMessage {
            content: "carry on".to_string(),
        }),
    ];

    let mut projector = ContextProjector::new().with_profile(ProjectionProfile::P2);
    let projected = projector
        .project(canonical.as_mut_slice(), &tight_budget(), 0, None, 2)
        .expect("a valid multimodal read must never abort the run with ContextBudgetExceeded");

    let remaining: usize = projected
        .messages
        .iter()
        .map(|item| match item {
            TranscriptItem::ToolResult(tr) => tr.image_attachment_bytes(),
            _ => 0,
        })
        .sum();
    assert_eq!(
        remaining, 0,
        "an image that cannot fit the budget must be evicted rather than failing the projection"
    );
    assert!(
        projected.message_history_bytes <= 64 * 1024,
        "the projection must end below the high watermark, got {}",
        projected.message_history_bytes
    );
}

// ---- Protocol: truncated SSE streams must be rejected -----------------------------------------

#[tokio::test]
async fn test_anthropic_truncated_sse_streams_are_rejected() {
    let truncated_events: &[(&str, &str)] = &[
        (
            "message_start",
            r#"{"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","content":[],"usage":{"input_tokens":10,"output_tokens":1}}}"#,
        ),
        (
            "content_block_start",
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_1","name":"bash"}}"#,
        ),
        (
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"cmd\":"}}"#,
        ),
        // No `content_block_stop`, no `message_delta`, no `message_stop`: the proxy cut the stream.
    ];
    let (base_url, _rx) = spawn_mock_http_server(
        "HTTP/1.1 200 OK",
        vec![("Content-Type", "text/event-stream".to_string())],
        sse_body(truncated_events),
    )
    .await;

    let client = AnthropicMessagesClient::new(AnthropicMessagesConfig {
        base_url,
        api_key: "sk-ant-test-key".to_string(),
        model: "claude-sonnet-4-5".to_string(),
        max_retries: 0,
        max_tokens: 4096,
    })
    .unwrap();

    let (_tx, cancel_rx) = watch::channel(false);
    let err = client
        .complete(
            single_user_request(),
            &kroma_core::budget::BudgetConfig::default(),
            cancel_rx,
        )
        .await
        .expect_err("a stream without message_stop must not be accepted as a complete turn");
    assert!(
        matches!(err, ProviderError::IncompleteStream(_)),
        "expected IncompleteStream, got {:?}",
        err
    );

    // A stream that does send `message_stop` but leaves a `tool_use` block open must also fail:
    // the partial JSON would otherwise be executed as a tool call.
    let unclosed_events: &[(&str, &str)] = &[
        (
            "message_start",
            r#"{"type":"message_start","message":{"id":"msg_2","type":"message","role":"assistant","content":[],"usage":{"input_tokens":10,"output_tokens":1}}}"#,
        ),
        (
            "content_block_start",
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_2","name":"bash"}}"#,
        ),
        (
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"cmd\":\"ls\"}"}}"#,
        ),
        (
            "message_delta",
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":5}}"#,
        ),
        ("message_stop", r#"{"type":"message_stop"}"#),
    ];
    let (base_url, _rx) = spawn_mock_http_server(
        "HTTP/1.1 200 OK",
        vec![("Content-Type", "text/event-stream".to_string())],
        sse_body(unclosed_events),
    )
    .await;
    let client = AnthropicMessagesClient::new(AnthropicMessagesConfig {
        base_url,
        api_key: "sk-ant-test-key".to_string(),
        model: "claude-sonnet-4-5".to_string(),
        max_retries: 0,
        max_tokens: 4096,
    })
    .unwrap();
    let (_tx, cancel_rx) = watch::channel(false);
    let err = client
        .complete(
            single_user_request(),
            &kroma_core::budget::BudgetConfig::default(),
            cancel_rx,
        )
        .await
        .expect_err("an unclosed tool_use block must not be accepted");
    assert!(
        matches!(err, ProviderError::IncompleteStream(_)),
        "expected IncompleteStream, got {:?}",
        err
    );
}

#[tokio::test]
async fn test_openai_responses_truncated_sse_streams_are_rejected() {
    // (a) The stream ends before `response.completed`.
    let no_completed: &[(&str, &str)] = &[
        (
            "response.created",
            r#"{"type":"response.created","response":{"id":"resp_1","status":"in_progress"}}"#,
        ),
        (
            "response.output_text.delta",
            r#"{"type":"response.output_text.delta","delta":"Searching"}"#,
        ),
    ];
    let (base_url, _rx) = spawn_mock_http_server(
        "HTTP/1.1 200 OK",
        vec![("Content-Type", "text/event-stream".to_string())],
        sse_body(no_completed),
    )
    .await;
    let client = OpenAiResponsesClient::new(OpenAiResponsesConfig {
        base_url,
        api_key: "sk-openai-test".to_string(),
        model: "gpt-5.2".to_string(),
        max_retries: 0,
        reasoning_effort: None,
    })
    .unwrap();
    let (_tx, cancel_rx) = watch::channel(false);
    let err = client
        .complete(
            single_user_request(),
            &kroma_core::budget::BudgetConfig::default(),
            cancel_rx,
        )
        .await
        .expect_err("a stream without response.completed must not be accepted");
    assert!(
        matches!(err, ProviderError::IncompleteStream(_)),
        "expected IncompleteStream, got {:?}",
        err
    );

    // (b) `response.completed` arrives but the function call never finished streaming its
    // arguments: promoting it with `{}` would execute a tool the model never fully specified.
    let unclosed_call: &[(&str, &str)] = &[
        (
            "response.created",
            r#"{"type":"response.created","response":{"id":"resp_2","status":"in_progress"}}"#,
        ),
        (
            "response.output_item.added",
            r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","call_id":"call_1","name":"bash","arguments":""}}"#,
        ),
        (
            "response.completed",
            r#"{"type":"response.completed","response":{"id":"resp_2","status":"completed","usage":{"input_tokens":10,"output_tokens":1}}}"#,
        ),
    ];
    let (base_url, _rx) = spawn_mock_http_server(
        "HTTP/1.1 200 OK",
        vec![("Content-Type", "text/event-stream".to_string())],
        sse_body(unclosed_call),
    )
    .await;
    let client = OpenAiResponsesClient::new(OpenAiResponsesConfig {
        base_url,
        api_key: "sk-openai-test".to_string(),
        model: "gpt-5.2".to_string(),
        max_retries: 0,
        reasoning_effort: None,
    })
    .unwrap();
    let (_tx, cancel_rx) = watch::channel(false);
    let err = client
        .complete(
            single_user_request(),
            &kroma_core::budget::BudgetConfig::default(),
            cancel_rx,
        )
        .await
        .expect_err("an unclosed function_call must not be promoted into a `{}` call");
    assert!(
        matches!(err, ProviderError::IncompleteStream(_)),
        "expected IncompleteStream, got {:?}",
        err
    );
}

#[test]
fn test_responses_non_streaming_failed_status_is_an_error() {
    let err = parse_responses_response(r#"{"status":"failed","output":[]}"#)
        .expect_err("a failed response must not be reported as a successful turn");
    assert!(
        matches!(err, ProviderError::Transport(_)),
        "expected Transport, got {:?}",
        err
    );
    let incomplete = parse_responses_response(
        r#"{"status":"incomplete","output":[{"type":"message","content":[{"type":"output_text","text":"partial"}]}]}"#,
    )
    .unwrap();
    assert_eq!(incomplete.finish_reason, FinishReason::Length);
}

#[test]
fn test_anthropic_wire_messages_alternate_roles_and_lead_with_thinking() {
    use kroma_core::messages::ReasoningBlock;

    let messages = vec![
        TranscriptItem::User(UserMessage {
            content: "audit auth".to_string(),
        }),
        TranscriptItem::Assistant(AssistantTurn {
            content: Some("thinking out loud".to_string()),
            reasoning_blocks: vec![ReasoningBlock {
                text: Some("consider the sandbox".to_string()),
                signature: Some("sig-1".to_string()),
                opaque_payload: None,
            }],
            tool_calls: vec![ToolCall {
                call_id: ToolCallId::new("toolu_1"),
                name: "read".to_string(),
                raw_arguments: r#"{"path":"auth.rs"}"#.to_string(),
            }],
            finish_reason: FinishReason::ToolCalls,
            usage: None,
        }),
        TranscriptItem::ToolResult(ToolResultMessage {
            call_id: ToolCallId::new("toolu_1"),
            tool_name: "read".to_string(),
            outcome: ToolOutcome::Success,
            content: Arc::from("fn authenticate() {}"),
            meta: ToolResultMeta::default(),
        }),
        // A steering user message immediately after the tool result: emitting it as its own wire
        // message would put two `user` messages back to back and Anthropic rejects that.
        TranscriptItem::User(UserMessage {
            content: "now check the router".to_string(),
        }),
    ];

    let wire = serialize_anthropic_request_body("m", "sys", &[], &messages, &[], false, 1024);
    let json: serde_json::Value = serde_json::from_str(&wire).unwrap();
    let wire_messages = json["messages"].as_array().unwrap();

    let roles: Vec<&str> = wire_messages
        .iter()
        .map(|m| m["role"].as_str().unwrap())
        .collect();
    assert_eq!(roles, vec!["user", "assistant", "user"]);

    // Rule 2: thinking leads the assistant turn.
    let assistant_blocks = wire_messages[1]["content"].as_array().unwrap();
    assert_eq!(assistant_blocks[0]["type"], "thinking");
    assert_eq!(assistant_blocks[1]["type"], "text");
    assert_eq!(assistant_blocks[2]["type"], "tool_use");

    // Rule 1 + tool_result ordering: the merged user turn keeps `tool_result` blocks first.
    let merged = wire_messages[2]["content"].as_array().unwrap();
    assert_eq!(merged[0]["type"], "tool_result");
    assert_eq!(merged[0]["tool_use_id"], "toolu_1");
    assert_eq!(merged[1]["type"], "text");
    assert_eq!(merged[1]["text"], "now check the router");

    // Rule 3: an empty system prompt must omit the `system` field rather than send an empty block.
    let blank = serialize_anthropic_request_body("m", "   ", &[], &messages, &[], false, 1024);
    let blank_json: serde_json::Value = serde_json::from_str(&blank).unwrap();
    assert!(blank_json.get("system").is_none(), "{}", blank);

    // The `cache_control` prefix breakpoint indexes the transcript, so it must be translated onto
    // the merged wire message that actually carries item 1's blocks.
    let marked = serialize_anthropic_request_body(
        "m",
        "sys",
        &[],
        &messages,
        &[CacheBreakpointMarker {
            breakpoint: CacheBreakpoint::PinnedBaselineEnd,
            message_index: 1,
        }],
        false,
        1024,
    );
    let marked_json: serde_json::Value = serde_json::from_str(&marked).unwrap();
    let marked_blocks = marked_json["messages"][1]["content"].as_array().unwrap();
    assert_eq!(
        marked_blocks.last().unwrap()["cache_control"]["type"],
        "ephemeral",
        "the breakpoint must land on the assistant wire message, not drift forward"
    );
}

// =============================================================================================
// v1.4.0 — Astra F09 (Responses encrypted reasoning round-trip) + B1 (incremental RepoGraph)
// =============================================================================================

use kroma_core::messages::ReasoningBlock;
use kroma_core::repo_graph::{EdgeKind, RepoGraphCache};
use kroma_core::subagents::{default_test_protected_globs, effective_protected_globs};

#[test]
fn test_f09_responses_encrypted_content_round_trip_non_streaming() {
    // A non-streaming reasoning item carrying only `encrypted_content` (no readable summary) must
    // be preserved on the turn...
    let body = r#"{
        "id": "resp_1",
        "status": "completed",
        "output": [
            {"type": "reasoning", "encrypted_content": "OPAQUE-BLOB-123"},
            {"type": "message", "content": [{"type": "output_text", "text": "hi"}]}
        ],
        "usage": {"input_tokens": 10, "output_tokens": 2}
    }"#;
    let turn = parse_responses_response(body).unwrap();
    assert_eq!(turn.reasoning_blocks.len(), 1);
    assert_eq!(
        turn.reasoning_blocks[0].opaque_payload.as_deref(),
        Some("OPAQUE-BLOB-123")
    );
    assert!(turn.reasoning_blocks[0].text.is_none());

    // ...and replayed verbatim on the next request, even though `text` is empty.
    let messages = vec![
        TranscriptItem::User(UserMessage {
            content: "do a thing".to_string(),
        }),
        TranscriptItem::Assistant(turn),
        TranscriptItem::ToolResult(ToolResultMessage {
            call_id: ToolCallId::new("call_1"),
            tool_name: "read".to_string(),
            outcome: ToolOutcome::Success,
            content: Arc::from("ok"),
            meta: ToolResultMeta::default(),
        }),
    ];
    let wire = serialize_responses_request_body("gpt-5.2", "sys", &[], &messages, false, None);
    let v: serde_json::Value = serde_json::from_str(&wire).unwrap();
    let reasoning = v["input"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item.get("type").and_then(|t| t.as_str()) == Some("reasoning"))
        .expect("reasoning item must be replayed");
    assert_eq!(
        reasoning.get("encrypted_content").and_then(|c| c.as_str()),
        Some("OPAQUE-BLOB-123")
    );
}

#[test]
fn test_f09_responses_serialization_replays_text_and_opaque_together() {
    let turn = AssistantTurn {
        content: Some("thinking out loud".to_string()),
        reasoning_blocks: vec![ReasoningBlock {
            text: Some("summary".to_string()),
            signature: None,
            opaque_payload: Some("OPAQUE-2".to_string()),
        }],
        tool_calls: Vec::new(),
        finish_reason: FinishReason::Stop,
        usage: None,
    };
    let messages = vec![TranscriptItem::Assistant(turn)];
    let wire = serialize_responses_request_body("m", "sys", &[], &messages, false, None);
    let v: serde_json::Value = serde_json::from_str(&wire).unwrap();
    let reasoning = v["input"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item.get("type").and_then(|t| t.as_str()) == Some("reasoning"))
        .unwrap();
    assert_eq!(
        reasoning.get("encrypted_content").and_then(|c| c.as_str()),
        Some("OPAQUE-2")
    );
    assert!(reasoning.get("summary").is_some());
}

#[tokio::test]
async fn test_f09_responses_sse_captures_encrypted_reasoning_item() {
    let events = [
        (
            "response.created",
            r#"{"type":"response.created","response":{"id":"resp_1","status":"in_progress"}}"#,
        ),
        (
            "response.output_item.done",
            r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"reasoning","encrypted_content":"SSE-OPAQUE-9"}}"#,
        ),
        (
            "response.output_text.delta",
            r#"{"type":"response.output_text.delta","delta":"done"}"#,
        ),
        (
            "response.completed",
            r#"{"type":"response.completed","response":{"id":"resp_1","status":"completed","usage":{"input_tokens":10,"output_tokens":2}}}"#,
        ),
    ];
    let mut body = String::new();
    for (name, data) in events {
        body.push_str(&format!("event: {}\ndata: {}\n\n", name, data));
    }

    let (base_url, _rx) = spawn_mock_http_server(
        "HTTP/1.1 200 OK",
        vec![("Content-Type", "text/event-stream".to_string())],
        body,
    )
    .await;
    let client = OpenAiResponsesClient::new(OpenAiResponsesConfig {
        base_url,
        api_key: "sk-openai-test".to_string(),
        model: "gpt-5.2".to_string(),
        max_retries: 0,
        reasoning_effort: None,
    })
    .unwrap();
    let req = ProviderRequest {
        system_prompt: "sys".to_string(),
        tools: Vec::new(),
        messages: vec![TranscriptItem::User(UserMessage {
            content: "hello".to_string(),
        })],
        stream: true,
        cache_breakpoints: Vec::new(),
        max_output_tokens: None,
        retry_budget: None,
        wire_attempts: None,
    };
    let (_tx, cancel_rx) = watch::channel(false);
    let completion = client
        .complete(req, &kroma_core::budget::BudgetConfig::default(), cancel_rx)
        .await
        .expect("responses stream should succeed");
    assert!(
        completion
            .turn
            .reasoning_blocks
            .iter()
            .any(|r| r.opaque_payload.as_deref() == Some("SSE-OPAQUE-9")),
        "streamed encrypted reasoning must be captured: {:?}",
        completion.turn.reasoning_blocks
    );
}

#[test]
fn test_b1_repo_graph_incremental_cache_and_typed_edges() {
    let root = temp_root("b1-repograph");
    let ws_dir = root.join("ws");
    fs::create_dir_all(ws_dir.join("src")).unwrap();
    fs::write(
        ws_dir.join("src/lib.rs"),
        "use crate::auth::Session;\n\npub fn login() -> Session { Session::new() }\n",
    )
    .unwrap();
    fs::write(
        ws_dir.join("src/auth.rs"),
        "pub struct Session { id: u32 }\n\nimpl Session { pub fn new() -> Self { Self { id: 1 } } }\n",
    )
    .unwrap();
    let cap = WorkspaceCap::new(&ws_dir).unwrap();

    let mut cache = RepoGraphCache::new();
    let first = RepoGraph::build_from_workspace_cached(&cap, MAX_GRAPH_FILES, &mut cache);
    assert_eq!(first.cache_hits, 0);
    assert!(!cache.is_empty());
    assert!(cache.content_sha256("src/lib.rs").is_some());

    // Second build with unchanged files: everything is served from the cache.
    let second = RepoGraph::build_from_workspace_cached(&cap, MAX_GRAPH_FILES, &mut cache);
    assert!(
        second.cache_hits >= first.total_files(),
        "expected cache hits for unchanged files, got {} of {}",
        second.cache_hits,
        first.total_files()
    );

    // Typed edges: `Session` is a type defined in auth.rs and referenced from lib.rs.
    assert!(
        second
            .edges()
            .iter()
            .any(|e| matches!(e.kind, EdgeKind::Import | EdgeKind::TypeOrSymbol)),
        "expected a typed edge, got {:?}",
        second.edges()
    );
}

/// BUG-06: the production entry point (`build_from_workspace`, used by `code_graph` and
/// `/workspace/graph`) must reuse the per-workspace cache instead of building a throwaway one, and
/// must invalidate by content rather than trusting `(len, mtime)` alone.
#[test]
fn test_bug06_production_graph_build_reuses_workspace_cache_by_content() {
    let root = temp_root("bug06-graph-cache");
    let ws_dir = root.join("ws");
    fs::create_dir_all(ws_dir.join("src")).unwrap();
    // Same byte length for both versions, so a stale `(len, mtime)`-only key would go unnoticed.
    fs::write(
        ws_dir.join("src/lib.rs"),
        "pub fn alpha_fn() {}\npub fn beta_fn() { alpha_fn(); }\n",
    )
    .unwrap();
    let cap = WorkspaceCap::new(&ws_dir).unwrap();

    let first = RepoGraph::build_from_workspace(&cap, MAX_GRAPH_FILES);
    assert_eq!(first.cache_hits, 0, "first build parses everything");
    let second = RepoGraph::build_from_workspace(&cap, MAX_GRAPH_FILES);
    assert!(
        second.cache_hits > 0,
        "the production path must reuse the persistent cache"
    );
    assert_eq!(second.total_symbols(), first.total_symbols());
    assert_eq!(second.total_edges(), first.total_edges());

    // Changing the content (same length) must not serve the stale parse.
    fs::write(
        ws_dir.join("src/lib.rs"),
        "pub fn gamma_fn() {}\npub fn delta_fn() { gamma_fn(); }\n",
    )
    .unwrap();
    let third = RepoGraph::build_from_workspace(&cap, MAX_GRAPH_FILES);
    assert!(third.symbols().iter().any(|s| s.name == "gamma_fn"));
    assert!(!third.symbols().iter().any(|s| s.name == "alpha_fn"));

    let _ = fs::remove_dir_all(&root);
}

/// P2: the persistent per-workspace `RepoGraphCache` must be pruned on every build — a path that
/// disappears (or is no longer indexed) must not keep its entry for the whole process lifetime.
/// The pruning already exists via `RepoGraphCache::retain_paths`, called at the end of every build;
/// this test locks that behaviour so a future change cannot silently reintroduce unbounded growth.
#[test]
fn test_repo_graph_cache_prunes_paths_that_are_no_longer_indexed() {
    let root = temp_root("repograph-cache-prune");
    let ws_dir = root.join("ws");
    fs::create_dir_all(ws_dir.join("src")).unwrap();
    fs::write(ws_dir.join("src/a.rs"), "pub fn a() {}\n").unwrap();
    fs::write(ws_dir.join("src/b.rs"), "pub fn b() {}\n").unwrap();
    let cap = WorkspaceCap::new(&ws_dir).unwrap();
    let cache = cap.repo_graph_cache().clone();

    let _ = RepoGraph::build_from_workspace(&cap, MAX_GRAPH_FILES);
    assert_eq!(
        cache.lock().unwrap().len(),
        2,
        "both files must be indexed and cached"
    );

    // Deleting a file must drop its cache entry on the next build.
    fs::remove_file(ws_dir.join("src/b.rs")).unwrap();
    let _ = RepoGraph::build_from_workspace(&cap, MAX_GRAPH_FILES);
    {
        let c = cache.lock().unwrap();
        assert_eq!(c.len(), 1, "the deleted file's cache entry must be pruned");
        assert!(c.content_sha256("src/a.rs").is_some());
    }

    // The surviving unchanged file is still served from the cache, so pruning did not break reuse.
    let rebuilt = RepoGraph::build_from_workspace(&cap, MAX_GRAPH_FILES);
    assert!(rebuilt.cache_hits >= 1, "reuse must survive pruning");

    let _ = fs::remove_dir_all(&root);
}

/// C00-11: the cache fingerprint is `(path, len, mtime_ns, content_sha256)`. Without the content
/// leg, an edit that keeps the byte length and restores the mtime would be served from the stale
/// parse — the exact case `len`/`mtime` alone cannot see. This pins that leg: same path, same
/// length, same mtime, different bytes must rebuild.
#[test]
fn test_c0011_cache_rebuilds_when_bytes_change_under_same_length_and_mtime() {
    let root = temp_root("c0011-sha-leg");
    let ws_dir = root.join("ws");
    fs::create_dir_all(ws_dir.join("src")).unwrap();
    let path = ws_dir.join("src/ledger.rs");

    let before = "pub fn alpha_entry() -> usize { 1 }\n";
    let after = before.replace("alpha", "beta_");
    assert_eq!(
        before.len(),
        after.len(),
        "payloads must differ only in bytes"
    );

    fs::write(&path, before).unwrap();
    let mtime = fs::metadata(&path).unwrap().modified().unwrap();

    let cap = WorkspaceCap::new(&ws_dir).unwrap();
    let first = RepoGraph::build_from_workspace(&cap, MAX_GRAPH_FILES);
    assert!(first.symbols().iter().any(|s| s.name == "alpha_entry"));

    fs::write(&path, after).unwrap();
    {
        let file = fs::File::options().write(true).open(&path).unwrap();
        file.set_times(fs::FileTimes::new().set_modified(mtime))
            .unwrap();
    }

    // The premise: neither length nor mtime can tell these two revisions apart.
    let meta = fs::metadata(&path).unwrap();
    assert_eq!(meta.len(), before.len() as u64, "length must be unchanged");
    assert_eq!(meta.modified().unwrap(), mtime, "mtime must be restored");

    let second = RepoGraph::build_from_workspace(&cap, MAX_GRAPH_FILES);
    assert!(
        second.symbols().iter().any(|s| s.name == "beta__entry"),
        "changed bytes behind an identical (len, mtime) must be re-parsed"
    );
    assert!(
        !second.symbols().iter().any(|s| s.name == "alpha_entry"),
        "the stale parse must not survive"
    );

    let _ = fs::remove_dir_all(&root);
}

/// C00-11: a navigation query must reflect a file that was added or renamed since the previous
/// build. `code_graph` and `/workspace/graph` share one per-workspace parse cache, so this drives
/// the production entry point (`build_from_workspace`) three times against a mutating workspace;
/// builds two and three are warm, which is the only configuration in which a stale graph could be
/// served at all.
#[test]
fn test_c0011_navigation_query_tracks_added_and_renamed_files() {
    let root = temp_root("c0011-nav-cache");
    let ws_dir = root.join("ws");
    fs::create_dir_all(ws_dir.join("src")).unwrap();
    fs::write(
        ws_dir.join("src/ledger.rs"),
        "pub struct LedgerEntry { pub id: u32 }\n\npub fn ledger_apply() -> usize { 1 }\n",
    )
    .unwrap();
    fs::write(
        ws_dir.join("src/main.rs"),
        "use crate::ledger::ledger_apply;\n\npub fn run_ledger() -> usize { ledger_apply() }\n",
    )
    .unwrap();
    let cap = WorkspaceCap::new(&ws_dir).unwrap();
    let cache = cap.repo_graph_cache().clone();

    // 1. Baseline: the defining file is indexed and ranked for its own symbol.
    let first = RepoGraph::build_from_workspace(&cap, MAX_GRAPH_FILES);
    assert!(first.files().iter().any(|f| f == "src/ledger.rs"));
    let report = first.query(&["ledger_apply".to_string()], &[], 3);
    assert_eq!(
        report.ranked[0].path, "src/ledger.rs",
        "baseline query must rank the defining file first"
    );

    // 2. A new file must be indexed by a warm build, and a query must reach it.
    fs::write(
        ws_dir.join("src/audit.rs"),
        "pub fn audit_emit() -> usize { 2 }\n",
    )
    .unwrap();
    let second = RepoGraph::build_from_workspace(&cap, MAX_GRAPH_FILES);
    assert!(
        second.cache_hits > 0,
        "the second build must reuse the per-workspace cache"
    );
    assert!(
        second.files().iter().any(|f| f == "src/audit.rs"),
        "a file added since the previous build is missing from the graph: {:?}",
        second.files()
    );
    let report = second.query(&["audit_emit".to_string()], &[], 3);
    assert_eq!(
        report.ranked[0].path, "src/audit.rs",
        "a query must reach the added file"
    );

    // 3. A rename keeps the bytes identical, so only the path key can distinguish old from new.
    fs::rename(ws_dir.join("src/ledger.rs"), ws_dir.join("src/journal.rs")).unwrap();
    let third = RepoGraph::build_from_workspace(&cap, MAX_GRAPH_FILES);
    assert!(
        third.cache_hits > 0,
        "the third build must reuse the per-workspace cache"
    );
    assert!(
        !third.files().iter().any(|f| f == "src/ledger.rs"),
        "the renamed-away path is still indexed: {:?}",
        third.files()
    );
    assert!(
        third.files().iter().any(|f| f == "src/journal.rs"),
        "the renamed file is missing from the graph: {:?}",
        third.files()
    );
    assert!(
        !third
            .symbols()
            .iter()
            .any(|s| s.file_path == "src/ledger.rs"),
        "a symbol still points at the renamed-away path"
    );
    let report = third.query(&["ledger_apply".to_string()], &[], 3);
    assert_eq!(
        report.ranked[0].path, "src/journal.rs",
        "a query must follow the rename, not the stale path"
    );
    {
        let guard = cache.lock().unwrap();
        assert!(
            guard.content_sha256("src/ledger.rs").is_none(),
            "the renamed-away path kept a cache entry"
        );
        assert!(
            guard.content_sha256("src/journal.rs").is_some(),
            "the renamed file was not cached under its new path"
        );
    }

    let _ = fs::remove_dir_all(&root);
}

/// BUG-06: a Rust lifetime (`'a`, `'static`) is not a string opener. Before the fix the rest of the
/// line was erased, dropping the call/identifier tokens the graph is built from. Single-quoted
/// Python / JS strings must still be stripped.
#[test]
fn test_bug06_reference_tokens_survive_rust_lifetimes_and_still_strip_python_js_strings() {
    // Each line carries exactly one `'`, so the old code erased everything after it.
    let rust_src = "fn check<T>(x: T) -> usize where T: 'static { count_items(x) }\n\
                    fn f(x: &'a str) -> usize { compute_len(x) }\n";
    let tokens = extract_reference_tokens("src/lib.rs", rust_src);
    assert!(tokens.contains("count_items"), "tokens: {:?}", tokens);
    assert!(tokens.contains("compute_len"), "tokens: {:?}", tokens);

    // A real char literal is still stripped, and the code after it is preserved.
    let with_char = "fn g() -> char { let c = 'x'; marker_fn() }\n";
    let tokens = extract_reference_tokens("src/lib.rs", with_char);
    assert!(tokens.contains("marker_fn"), "tokens: {:?}", tokens);

    // Python single-quoted strings are still removed.
    let py = "def f():\n    call_marker('hidden_py_symbol')\n    after_fn()\n";
    let tokens = extract_reference_tokens("a.py", py);
    assert!(tokens.contains("after_fn"), "tokens: {:?}", tokens);
    assert!(!tokens.contains("hidden_py_symbol"), "tokens: {:?}", tokens);

    // JS single-quoted strings are still removed.
    let js = "const x = 'hidden_js_symbol';\njs_after();\n";
    let tokens = extract_reference_tokens("a.js", js);
    assert!(tokens.contains("js_after"), "tokens: {:?}", tokens);
    assert!(!tokens.contains("hidden_js_symbol"), "tokens: {:?}", tokens);
}

#[test]
fn test_item3_default_test_protected_globs_cover_common_layouts() {
    let globs = effective_protected_globs(true, &[]);
    assert!(globs.contains(&"**/tests/**".to_string()));
    assert!(!default_test_protected_globs().is_empty());
    // Explicit globs are honoured even without the default set.
    let explicit = effective_protected_globs(false, &["docs/**".to_string()]);
    assert_eq!(explicit, vec!["docs/**".to_string()]);
}
