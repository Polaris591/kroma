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
