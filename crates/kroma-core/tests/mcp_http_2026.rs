//! Loopback wire-contract fixture: modern MCP metadata headers + auth-scoped schema cache.
use kroma_core::mcp_http::{McpHttpClient, MCP_HTTP_PROTOCOL};
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;
use std::time::{Duration, Instant};

fn accept_json(stream: &mut TcpStream) -> (String, Value) {
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let mut bytes = Vec::new();
    let mut buf = [0; 4096];
    loop {
        let n = stream.read(&mut buf).unwrap();
        assert!(n != 0, "client closed before HTTP request completed");
        bytes.extend_from_slice(&buf[..n]);
        assert!(bytes.len() < 256 * 1024);
        if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
            let headers = String::from_utf8(bytes[..end].to_vec()).unwrap();
            let content_length: usize = headers
                .lines()
                .find(|s| s.to_ascii_lowercase().starts_with("content-length:"))
                .unwrap()
                .split(':')
                .nth(1)
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            let complete = end + 4 + content_length;
            if bytes.len() < complete {
                continue;
            }
            let body = serde_json::from_slice(&bytes[end + 4..complete]).unwrap();
            return (headers, body);
        }
    }
}

fn reply(mut stream: TcpStream, mime: &str, body: &str) {
    let wire =
        format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        mime, body.len(), body
    );
    stream.write_all(wire.as_bytes()).unwrap();
    stream.flush().unwrap();
}

#[tokio::test]
async fn http_mcp_2026_json_catalog_and_sse_tool_response_preserves_custom_headers() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!(
        "http://127.0.0.1:{}/mcp",
        listener.local_addr().unwrap().port()
    );
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let (headers, req) = accept_json(&mut stream);
        assert!(headers.contains("POST /mcp HTTP/1.1"));
        assert!(headers.contains("MCP-Protocol-Version: 2026-07-28"));
        assert!(headers.contains("Mcp-Method: tools/list"));
        assert!(headers.contains("Accept: application/json, text/event-stream"));
        assert_eq!(
            req["params"]["_meta"]["io.modelcontextprotocol/protocolVersion"],
            MCP_HTTP_PROTOCOL
        );
        assert!(req["params"].get("cursor").is_none());
        let response = json!({"jsonrpc":"2.0","id":req["id"],"result":{
            "cacheScope":"private", "ttlMs":60000,
            "tools":[{"name":"echo","description":"echo fixture","inputSchema":{
                "type":"object", "properties":{
                    "text":{"type":"string","x-mcp-header":"Text"},
                    "meta":{"type":"object","properties":{
                        "limit":{"type":"integer","x-mcp-header":"Limit"}}}
                }
            }}]
        }});
        reply(stream, "application/json", &response.to_string());

        // The caller must reuse the TTL-bound catalog and send tools/call directly.
        let (mut stream, _) = listener.accept().unwrap();
        let (headers, req) = accept_json(&mut stream);
        assert!(headers.contains("Mcp-Method: tools/call"));
        assert!(headers.contains("Mcp-Name: echo"));
        assert!(headers.contains("Mcp-Param-Text: =?base64?5L2g5aW9?="));
        assert!(headers.contains("Mcp-Param-Limit: 2"));
        assert_eq!(req["params"]["name"], "echo");
        assert_eq!(req["params"]["arguments"]["text"], "你好");
        let event = json!({"jsonrpc":"2.0","id":req["id"],
            "result":{"content":[{"type":"text","text":"echo-success"}]}});
        reply(stream, "text/event-stream", &format!(
            ": heartbeat\n\nevent: notification\ndata: {{\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\"}}\n\ndata: {}\n\n",
            event
        ));
    });

    let mut client = McpHttpClient::new(&url, None, Duration::from_secs(3)).unwrap();
    let catalog = client.list_tools().await.unwrap();
    assert_eq!(catalog.len(), 1);
    assert_eq!(catalog[0].name, "echo");
    let result = client
        .call_tool_until(
            "echo",
            json!({"text":"你好","meta":{"limit":2}}),
            Instant::now() + Duration::from_secs(3),
        )
        .await
        .unwrap();
    assert!(result.content.contains("echo-success"));
    server.join().unwrap();
}

#[test]
fn rejects_non_loopback_plain_http_and_forged_newlines() {
    assert!(McpHttpClient::new("http://public-host/mcp", None, Duration::from_secs(1)).is_err());
    assert!(McpHttpClient::new(
        "https://public-host/mcp",
        Some("bad\nheader".into()),
        Duration::from_secs(1)
    )
    .is_err());
}

/// KR-K02: a terminal SSE response ends the call, even when the server keeps the connection open.
///
/// The transport spec allows the server to hold the connection after delivering the response. The
/// client used to read the body to EOF first, so such a server stalled every call until the
/// deadline. This fixture sends the terminal frame with no `Content-Length` (so the body would only
/// end when the socket closes) and then holds the socket for five seconds.
#[tokio::test]
async fn http_mcp_2026_sse_terminal_response_ends_the_call_without_connection_close() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!(
        "http://127.0.0.1:{}/mcp",
        listener.local_addr().unwrap().port()
    );
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let (_headers, req) = accept_json(&mut stream);
        let response = json!({"jsonrpc":"2.0","id":req["id"],"result":{
            "cacheScope":"private", "ttlMs":60000,
            "tools":[{"name":"echo","description":"echo fixture","inputSchema":{
                "type":"object","properties":{"text":{"type":"string"}}}}]
        }});
        // Deliberately no Content-Length and no Connection: close.
        let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n";
        stream.write_all(head.as_bytes()).unwrap();
        stream
            .write_all(format!(": heartbeat\n\ndata: {}\n\n", response).as_bytes())
            .unwrap();
        stream.flush().unwrap();
        // Hold the socket well past the assertion window.
        thread::sleep(Duration::from_secs(5));
    });

    let mut client = McpHttpClient::new(&url, None, Duration::from_secs(20)).unwrap();
    let started = Instant::now();
    let catalog = client
        .list_tools()
        .await
        .expect("the terminal SSE frame must resolve the call");
    let elapsed = started.elapsed();

    assert_eq!(catalog.len(), 1);
    assert_eq!(catalog[0].name, "echo");
    assert!(
        elapsed < Duration::from_secs(2),
        "the call must end on the terminal frame, not on connection close (took {elapsed:?})"
    );
    drop(server);
}

/// KR-L01: how much of a request's cost is protocol plumbing, and how much is not.
///
/// The acceptance asks to "confirm the optimizable share". This measures the loopback protocol cost
/// (connect + request write + response read + parse) with the same client used against real
/// endpoints, so it can be compared against the live canary's 3336 ms ttft on the same machine.
/// Whatever is left over is network + provider, which no client-side change can remove.
#[tokio::test]
async fn local_loopback_protocol_cost_is_measured() {
    const REPEATS: usize = 5;
    let mut samples = Vec::new();

    for _ in 0..REPEATS {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!(
            "http://127.0.0.1:{}/mcp",
            listener.local_addr().unwrap().port()
        );
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let (_headers, req) = accept_json(&mut stream);
            let response = json!({"jsonrpc":"2.0","id":req["id"],"result":{
                "cacheScope":"private","ttlMs":60000,
                "tools":[{"name":"echo","description":"e","inputSchema":{"type":"object"}}]
            }});
            reply(stream, "application/json", &response.to_string());
        });

        let mut client = McpHttpClient::new(&url, None, Duration::from_secs(10)).unwrap();
        let started = Instant::now();
        let catalog = client
            .list_tools()
            .await
            .expect("loopback call must succeed");
        samples.push(started.elapsed());
        assert_eq!(catalog.len(), 1);
        server.join().unwrap();
    }

    samples.sort();
    let median = samples[REPEATS / 2];
    println!(
        "KR-L01 loopback protocol cost: median={:?} min={:?} max={:?} (n={})",
        median,
        samples[0],
        samples[REPEATS - 1],
        REPEATS
    );
    // Loopback plumbing is sub-millisecond to a few ms. If this ever reaches tens of ms, the client
    // is doing real work per request and that is worth optimizing; against the live canary's
    // 3336 ms ttft it is noise, so the optimizable share of end-to-end latency is not here.
    assert!(
        median < Duration::from_millis(50),
        "loopback protocol cost should be small, got {median:?}"
    );
}

/// KR-K04: an MRTR / InputRequired response is refused, not silently accepted.
///
/// The task's rule is "only claim support when the complete state machine and real interop pass; do not
/// treat field acceptance as protocol implementation". Kroma does not implement MRTR, and the adapter
/// says so by refusing a response whose `resultType` is `input_required` instead of discarding the
/// input request and returning a result the model would misread as final.
#[tokio::test]
async fn mcp_http_2026_refuses_an_input_required_response() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!(
        "http://127.0.0.1:{}/mcp",
        listener.local_addr().unwrap().port()
    );
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let (_headers, req) = accept_json(&mut stream);
        let response = json!({
            "jsonrpc": "2.0",
            "id": req["id"],
            "result": {
                "resultType": "input_required",
                "inputRequests": [{"kind": "text", "prompt": "approve?"}]
            }
        });
        reply(stream, "application/json", &response.to_string());
    });

    let mut client = McpHttpClient::new(&url, None, Duration::from_secs(10)).unwrap();
    let error = client
        .list_tools()
        .await
        .expect_err("an input_required response must be refused");
    let text = format!("{error:?}");
    assert!(
        text.contains("MRTR") || text.contains("InputRequired") || text.contains("input_required"),
        "the refusal must name what is unsupported, got: {text}"
    );
    server.join().unwrap();
}
