//! Offline integration against a local MCP stdio child and an HTTP server.
//! No external MCP provider, API token or LLM is needed.
use anyhow::{Context, Result};
use kroma::{
    mcp::register_configured,
    protocol::{FunctionCall, ToolCall},
    skills::{LoadSkill, SkillCatalog},
    tools::{ToolContext, ToolRegistry},
};
use serde_json::{json, Value};
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

fn test_directory(label: &str) -> Result<PathBuf> {
    let dir = std::env::temp_dir().join(format!(
        "kroma-{label}-{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    ));
    fs::create_dir_all(dir.join(".kroma"))?;
    Ok(dir)
}

fn invoke(name: &str, arguments: &str) -> ToolCall {
    ToolCall {
        id: "request1".into(),
        kind: "function".into(),
        function: FunctionCall {
            name: name.into(),
            arguments: arguments.into(),
        },
    }
}

#[cfg(unix)]
#[tokio::test]
async fn stdio_mcp_initializes_lists_and_calls_tools() -> Result<()> {
    let dir = test_directory("mcp-stdio")?;
    let fixture = dir.join("server.py");
    // A small local fixture emulates a real JSON-RPC/MCP child process. It
    // responds to initialization, notifications and paginated tool discovery.
    fs::write(
        &fixture,
        r#"import sys,json
for line in sys.stdin:
    req = json.loads(line)
    method = req.get("method")
    if "id" not in req:
        continue
    if method == "initialize":
        result = {"protocolVersion":"2025-11-25","serverInfo":{"name":"mock","version":"1.0"},"capabilities":{"tools":{}}}
    elif method == "tools/list":
        if "cursor" not in req["params"]:
            result = {"tools":[{"name":"echo","description":"Return an echo",
                      "inputSchema":{"type":"object","properties":{"text":{"type":"string"}}}}],"nextCursor":"page2"}
        else:
            result = {"tools":[{"name":"second","description":"Second tool",
                      "inputSchema":{"type":"object","properties":{}}}]}
    elif method == "tools/call":
        result = {"content":[{"type":"text","text":req["params"]["name"]+":"+str(req["params"]["arguments"].get("text",""))}],"isError":False}
    else:
        raise Exception("unknown method: "+str(method))
    print(json.dumps({"jsonrpc":"2.0","id":req["id"],"result":result}),flush=True)
"#,
    )?;
    fs::write(
        dir.join(".kroma/mcp.json"),
        serde_json::to_vec(&json!({"mcpServers":{"local":{
            "command":"python3","args":[fixture.display().to_string()]
        }}}))?,
    )?;
    let mut tools = ToolRegistry::builtin();
    let added = register_configured(&mut tools, &dir).await?;
    assert_eq!(added, 2);
    let definitions = tools.definitions();
    assert_eq!(definitions.len(), 3);
    assert!(definitions
        .iter()
        .any(|d| d["function"]["name"] == "mcp__local__echo"));
    assert!(definitions
        .iter()
        .any(|d| d["function"]["name"] == "mcp__local__second"));
    let result = tools
        .execute(
            &invoke("mcp__local__echo", r#"{"text":"hello"}"#),
            ToolContext {
                workdir: &dir,
                archive: Path::new("/tmp/test.json"),
                on_output: None,
            },
        )
        .await;
    let result: Value = serde_json::from_str(&result.content)?;
    assert_eq!(result["content"][0]["text"], "echo:hello");
    drop(tools);
    fs::remove_dir_all(dir)?;
    Ok(())
}

fn read_request(stream: &TcpStream) -> Result<(String, Vec<String>, Value)> {
    stream.set_read_timeout(Some(Duration::from_secs(4)))?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let mut size = 0;
    let mut headers = Vec::new();
    loop {
        let mut header = String::new();
        reader.read_line(&mut header)?;
        if header == "\r\n" {
            break;
        }
        if let Some(value) = header.to_ascii_lowercase().strip_prefix("content-length:") {
            size = value.trim().parse()?;
        }
        headers.push(header.trim().into());
    }
    let mut bytes = vec![0; size];
    reader.read_exact(&mut bytes)?;
    Ok((line, headers, serde_json::from_slice(&bytes)?))
}

fn reply(mut stream: TcpStream, value: Value, sse: bool) -> Result<()> {
    let body = if sse {
        format!("event: message\ndata: {value}\n\n")
    } else {
        value.to_string()
    };
    let mime = if sse {
        "text/event-stream"
    } else {
        "application/json"
    };
    write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len())?;
    stream.flush()?;
    Ok(())
}

async fn http_integration(version: &str, sse: bool) -> Result<()> {
    let dir = test_directory(if sse { "mcp-http-sse" } else { "mcp-http" })?;
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let endpoint = format!("http://{}/mcp", listener.local_addr()?);
    let modern = version == "2026-07-28";
    let versions = version.to_owned();
    let server = std::thread::spawn(move || -> Result<()> {
        let total = if modern { 2 } else { 4 };
        for step in 0..total {
            let (stream, _) = listener.accept()?;
            let (line, headers, request) = read_request(&stream)?;
            assert!(line.starts_with("POST /mcp "));
            let method = request["method"].as_str().context("method missing")?;
            assert!(headers.iter().any(|h| h
                .to_ascii_lowercase()
                .starts_with("authorization: bearer mock-token")));
            if modern {
                assert_eq!(
                    request["params"]["_meta"]["io.modelcontextprotocol/protocolVersion"],
                    versions
                );
                assert!(headers
                    .iter()
                    .any(|h| h.to_ascii_lowercase().starts_with("mcp-method:")));
            }
            match method {
                "initialize" => {
                    assert!(!modern);
                    reply(
                        stream,
                        json!({"jsonrpc":"2.0","id":request["id"],"result":{
                            "protocolVersion":"2025-11-25","capabilities":{"tools":{}},"serverInfo":{"name":"mock","version":"1"}
                        }}),
                        false,
                    )?;
                }
                "notifications/initialized" => {
                    assert!(!modern);
                    let mut stream = stream;
                    write!(
                        stream,
                        "HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    )?;
                    stream.flush()?;
                }
                "tools/list" => reply(
                    stream,
                    json!({"jsonrpc":"2.0","id":request["id"],
                    "result":{"tools":[{"name":"echo","description":"Echo input",
                    "inputSchema":{"type":"object","properties":{"text":{"type":"string"}}}}]}}),
                    sse,
                )?,
                "tools/call" => {
                    assert_eq!(request["params"]["name"], "echo");
                    assert_eq!(request["params"]["arguments"]["text"], "hello");
                    reply(
                        stream,
                        json!({"jsonrpc":"2.0","id":request["id"],"result":{
                            "content":[{"type":"text","text":"HTTP-hello"}],"isError":false
                        }}),
                        sse,
                    )?;
                }
                other => anyhow::bail!("unexpected HTTP MCP method {other}"),
            }
            let _ = step;
        }
        Ok(())
    });
    fs::write(
        dir.join(".kroma/mcp.json"),
        serde_json::to_vec(&json!({
            "mcpServers":{"remote":{"url":endpoint,"protocolVersion":version,
                "headers":{"Authorization":"Bearer mock-token"}}}
        }))?,
    )?;
    let mut tools = ToolRegistry::builtin();
    let added = register_configured(&mut tools, &dir).await?;
    assert_eq!(added, 1);
    let value = tools
        .execute(
            &invoke("mcp__remote__echo", r#"{"text":"hello"}"#),
            ToolContext {
                workdir: &dir,
                archive: Path::new("/tmp/test.json"),
                on_output: None,
            },
        )
        .await;
    let value: Value = serde_json::from_str(&value.content)?;
    assert_eq!(value["content"][0]["text"], "HTTP-hello");
    server.join().expect("HTTP MCP fixture panicked")?;
    drop(tools);
    fs::remove_dir_all(dir)?;
    Ok(())
}

#[tokio::test]
async fn streamable_http_legacy_mcp_json_and_handshake() -> Result<()> {
    http_integration("2025-11-25", false).await
}

#[tokio::test]
async fn streamable_http_modern_mcp_sse_and_inline_metadata() -> Result<()> {
    http_integration("2026-07-28", true).await
}

#[tokio::test]
async fn independent_mcp_servers_connect_concurrently_in_stable_order() -> Result<()> {
    use std::sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    };
    let dir = test_directory("mcp-parallel-startup")?;
    let active = Arc::new(AtomicUsize::new(0));
    let overlapped = Arc::new(AtomicBool::new(false));
    let mut urls = serde_json::Map::new();
    let mut servers = Vec::new();
    for name in ["alpha", "beta"] {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        urls.insert(
            name.into(),
            json!({
                "url": format!("http://{}/mcp", listener.local_addr()?),
                "protocolVersion": "2026-07-28"
            }),
        );
        let active = active.clone();
        let overlapped = overlapped.clone();
        servers.push(std::thread::spawn(move || -> Result<()> {
            let (stream, _) = listener.accept()?;
            let (_, _, request) = read_request(&stream)?;
            assert_eq!(request["method"], "tools/list");
            if active.fetch_add(1, Ordering::SeqCst) > 0 {
                overlapped.store(true, Ordering::SeqCst);
            }
            std::thread::sleep(Duration::from_millis(500));
            active.fetch_sub(1, Ordering::SeqCst);
            reply(
                stream,
                json!({"jsonrpc":"2.0","id":request["id"],
                "result":{"tools":[{"name":"ping","description":name,
                    "inputSchema":{"type":"object","properties":{}}}]}}),
                false,
            )
        }));
    }
    fs::write(
        dir.join(".kroma/mcp.json"),
        serde_json::to_vec(&json!({"mcpServers":urls}))?,
    )?;
    let mut registry = ToolRegistry::builtin();
    let registered = tokio::time::timeout(
        Duration::from_secs(5),
        register_configured(&mut registry, &dir),
    )
    .await??;
    assert_eq!(registered, 2);
    assert!(
        overlapped.load(Ordering::SeqCst),
        "server handshakes must overlap"
    );
    let names = registry
        .definitions()
        .into_iter()
        .filter_map(|t| t["function"]["name"].as_str().map(str::to_owned))
        .collect::<Vec<_>>();
    assert_eq!(
        &names[1..],
        ["mcp__alpha__ping", "mcp__beta__ping"],
        "completion order must not change the model tool schema"
    );
    for server in servers {
        server.join().expect("MCP fixture thread")?;
    }
    drop(registry);
    fs::remove_dir_all(dir)?;
    Ok(())
}

#[tokio::test]
async fn skills_expose_metadata_and_load_on_demand() -> Result<()> {
    let dir = test_directory("skills-e2e")?;
    let path = dir.join(".agents/skills/code-review");
    fs::create_dir_all(&path)?;
    fs::write(
        path.join("SKILL.md"),
        "---\nname: code-review\ndescription: Inspect Rust code quality\n---\nBODY-ONLY-ON-LOAD",
    )?;
    let skills = SkillCatalog::from_roots(&[dir.join(".agents/skills")])?;
    let metadata = skills.metadata_prompt().unwrap();
    assert!(metadata.contains("Inspect Rust code quality"));
    assert!(!metadata.contains("BODY-ONLY-ON-LOAD"));
    let mut registry = ToolRegistry::builtin();
    registry.register(std::sync::Arc::new(LoadSkill(skills)))?;
    let result = registry
        .execute(
            &invoke("load_skill", r#"{"name":"code-review"}"#),
            ToolContext {
                workdir: &dir,
                archive: Path::new("/tmp/test.json"),
                on_output: None,
            },
        )
        .await;
    let value: Value = serde_json::from_str(&result.content)?;
    assert!(value["instructions"]
        .as_str()
        .unwrap()
        .contains("BODY-ONLY-ON-LOAD"));
    assert!(value["skill_directory"]
        .as_str()
        .unwrap()
        .ends_with("code-review"));
    fs::remove_dir_all(dir)?;
    Ok(())
}
