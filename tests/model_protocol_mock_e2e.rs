//! Offline wire-level contract tests. No provider key, network access or
//! live model needed; exercises HTTP, SSE, Agent, Shell, WAL and APEC.
use anyhow::{Context, Result};
use kroma::{app, config::Config, provider::ModelClient};
use serde_json::{json, Value};
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    path::PathBuf,
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Copy)]
enum Wire {
    Anthropic,
    Responses,
}

impl Wire {
    fn label(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::Responses => "responses",
        }
    }
    fn route(self) -> &'static str {
        match self {
            Self::Anthropic => "/v1/messages",
            Self::Responses => "/v1/responses",
        }
    }
}

fn temp_workdir(label: &str) -> Result<PathBuf> {
    let path = std::env::temp_dir().join(format!(
        "kroma-mock-{label}-{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos(),
    ));
    std::fs::create_dir_all(&path)?;
    Ok(path)
}

fn read_http(stream: &TcpStream) -> Result<(String, Vec<String>, Value)> {
    stream.set_read_timeout(Some(Duration::from_secs(6)))?;
    let mut reader = BufReader::new(stream);
    let mut first = String::new();
    reader.read_line(&mut first)?;
    let mut headers = Vec::new();
    let mut size = 0;
    loop {
        let mut header = String::new();
        reader.read_line(&mut header)?;
        if header == "\r\n" {
            break;
        }
        if let Some(value) = header.to_ascii_lowercase().strip_prefix("content-length:") {
            size = value.trim().parse()?;
        }
        headers.push(header.trim().to_owned());
    }
    let mut body = vec![0; size];
    reader.read_exact(&mut body)?;
    let value = if body.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&body)?
    };
    Ok((first, headers, value))
}

fn http_reply(mut stream: TcpStream, body: &str, mime: &str) -> Result<()> {
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len(),
    )?;
    stream.flush()?;
    Ok(())
}

fn sse_event(name: &str, value: Value) -> String {
    format!("event: {name}\ndata: {value}\n\n")
}

fn mock_stream(wire: Wire, step: usize) -> String {
    match wire {
        Wire::Anthropic => {
            let mut s = sse_event(
                "message_start",
                json!({"type":"message_start","message":{"usage":{"input_tokens":40,"output_tokens":0}}}),
            );
            if step == 0 {
                s += &sse_event(
                    "content_block_start",
                    json!({"type":"content_block_start",
                    "index":0,"content_block":{"type":"thinking","thinking":""}}),
                );
                s += &sse_event(
                    "content_block_delta",
                    json!({"type":"content_block_delta",
                    "index":0,"delta":{"type":"thinking_delta","thinking":"hidden"}}),
                );
                s += &sse_event(
                    "content_block_delta",
                    json!({"type":"content_block_delta",
                    "index":0,"delta":{"type":"signature_delta","signature":"signed-opaque"}}),
                );
                s += &sse_event(
                    "content_block_stop",
                    json!({"type":"content_block_stop","index":0}),
                );
                s += &sse_event(
                    "content_block_start",
                    json!({"type":"content_block_start",
                    "index":1,"content_block":{"type":"tool_use","id":"call_mock","name":"shell","input":{}}}),
                );
                s += &sse_event(
                    "content_block_delta",
                    json!({"type":"content_block_delta",
                    "index":1,"delta":{"type":"input_json_delta","partial_json":"{\"command\":\"printf mock-ok\"}"}}),
                );
                s += &sse_event(
                    "content_block_stop",
                    json!({"type":"content_block_stop","index":1}),
                );
                s += &sse_event(
                    "message_delta",
                    json!({"type":"message_delta",
                    "delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":30}}),
                );
            } else {
                let text = if step == 1 {
                    "confirmed tool"
                } else {
                    "resumed history"
                };
                s += &sse_event(
                    "content_block_start",
                    json!({"type":"content_block_start",
                    "index":0,"content_block":{"type":"text","text":""}}),
                );
                s += &sse_event(
                    "content_block_delta",
                    json!({"type":"content_block_delta",
                    "index":0,"delta":{"type":"text_delta","text":text}}),
                );
                s += &sse_event(
                    "message_delta",
                    json!({"type":"message_delta",
                    "delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":5}}),
                );
            }
            s + &sse_event("message_stop", json!({"type":"message_stop"}))
        }
        Wire::Responses => {
            let mut s = String::new();
            if step == 0 {
                s += &sse_event(
                    "response.output_item.added",
                    json!({"type":"response.output_item.added",
                    "output_index":0,"item":{"type":"reasoning","id":"rs_mock","encrypted_content":"encrypted-opaque"}}),
                );
                s += &sse_event(
                    "response.output_item.done",
                    json!({"type":"response.output_item.done",
                    "output_index":0,"item":{"type":"reasoning","id":"rs_mock","encrypted_content":"encrypted-opaque"}}),
                );
                s += &sse_event(
                    "response.output_item.added",
                    json!({"type":"response.output_item.added",
                    "output_index":1,"item":{"type":"function_call","call_id":"call_mock",
                        "name":"shell","arguments":""}}),
                );
                s += &sse_event(
                    "response.function_call_arguments.delta",
                    json!({"type":"response.function_call_arguments.delta",
                    "output_index":1,"delta":"{\"command\":\"printf "}),
                );
                s += &sse_event(
                    "response.function_call_arguments.delta",
                    json!({"type":"response.function_call_arguments.delta",
                    "output_index":1,"delta":"mock-ok\"}"}),
                );
                s += &sse_event(
                    "response.output_item.done",
                    json!({"type":"response.output_item.done",
                    "output_index":1,"item":{"type":"function_call","call_id":"call_mock","name":"shell",
                        "arguments":"{\"command\":\"printf mock-ok\"}"}}),
                );
            } else {
                let text = if step == 1 {
                    "confirmed tool"
                } else {
                    "resumed history"
                };
                s += &sse_event(
                    "response.output_item.added",
                    json!({"type":"response.output_item.added",
                    "output_index":0,"item":{"type":"message","content":[]}}),
                );
                s += &sse_event(
                    "response.output_text.delta",
                    json!({"type":"response.output_text.delta","delta":text}),
                );
                s += &sse_event(
                    "response.output_item.done",
                    json!({"type":"response.output_item.done",
                    "output_index":0,"item":{"type":"message","content":[{"type":"output_text","text":text}]}}),
                );
            }
            s + &sse_event(
                "response.completed",
                json!({"type":"response.completed",
                "response":{"status":"completed","usage":{"input_tokens":40,"output_tokens":12,"total_tokens":52}}}),
            )
        }
    }
}

fn assert_second_round(wire: Wire, req: &Value) {
    match wire {
        Wire::Anthropic => {
            let msgs = req["messages"].as_array().unwrap();
            let assistant = msgs
                .iter()
                .find(|m| m["role"] == "assistant" && m["content"].is_array())
                .unwrap();
            let blocks = assistant["content"].as_array().unwrap();
            assert!(blocks
                .iter()
                .any(|b| b["type"] == "thinking" && b["signature"] == "signed-opaque"));
            assert!(blocks
                .iter()
                .any(|b| b["type"] == "tool_use" && b["id"] == "call_mock"));
            let tool = msgs
                .iter()
                .flat_map(|m| m["content"].as_array().into_iter().flatten())
                .find(|b| b["type"] == "tool_result")
                .unwrap();
            assert_eq!(tool["tool_use_id"], "call_mock");
            assert!(tool["content"].as_str().unwrap().contains("mock-ok"));
        }
        Wire::Responses => {
            let msgs = req["input"].as_array().unwrap();
            assert!(msgs
                .iter()
                .any(|m| m["type"] == "reasoning" && m["encrypted_content"] == "encrypted-opaque"));
            assert!(msgs
                .iter()
                .any(|m| m["type"] == "function_call" && m["call_id"] == "call_mock"));
            let result = msgs
                .iter()
                .find(|m| m["type"] == "function_call_output")
                .unwrap();
            assert_eq!(result["call_id"], "call_mock");
            assert!(result["output"].as_str().unwrap().contains("mock-ok"));
        }
    }
}

async fn exercise(wire: Wire) -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let addr = listener.local_addr()?;
    let server = thread::spawn(move || -> Result<()> {
        let mut generation = 0;
        let mut discoveries = 0;
        let mut summary = 0;
        for _ in 0..6 {
            let (stream, _) = listener.accept()?;
            let (line, headers, request) = read_http(&stream)?;
            if line.starts_with("GET /v1/models/mock ") {
                discoveries += 1;
                if matches!(wire, Wire::Anthropic) {
                    assert!(headers
                        .iter()
                        .any(|h| h.to_ascii_lowercase().starts_with("x-api-key: mock-key")));
                }
                let metadata = json!({"id":"mock","max_input_tokens":65536,"max_tokens":8192,
                    "capabilities":{"thinking":{"types":{"adaptive":{"supported":true}}}}});
                http_reply(stream, &metadata.to_string(), "application/json")?;
                continue;
            }
            assert!(
                line.starts_with(&format!("POST {} ", wire.route())),
                "unexpected route: {line}"
            );
            assert_eq!(request["model"], "mock");
            if matches!(wire, Wire::Anthropic) {
                assert!(headers
                    .iter()
                    .any(|h| h.to_ascii_lowercase().starts_with("anthropic-version:")));
            }
            if request["stream"] == false {
                summary += 1;
                match wire {
                    Wire::Anthropic => {
                        assert_eq!(request["system"], "summarize safely");
                        assert!(request["messages"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .any(|m| m["role"] == "user" && m["content"] == "notes"));
                        http_reply(stream,&json!({"stop_reason":"end_turn","content":[
                            {"type":"text","text":"summary ok"}],"usage":{"input_tokens":8,"output_tokens":3}}).to_string(),"application/json")?;
                    }
                    Wire::Responses => {
                        assert!(request["input"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .any(|m| m["role"] == "system" && m["content"] == "summarize safely"));
                        http_reply(stream,&json!({"status":"completed","output":[
                            {"type":"message","content":[{"type":"output_text","text":"summary ok"}]}],
                            "usage":{"input_tokens":8,"output_tokens":3}}).to_string(),"application/json")?;
                    }
                }
                continue;
            }
            assert_eq!(request["stream"], true);
            assert_eq!(
                request["max_output_tokens"]
                    .as_u64()
                    .or_else(|| request["max_tokens"].as_u64()),
                Some(4096)
            );
            if generation == 0 {
                match wire {
                    Wire::Anthropic => {
                        assert_eq!(request["tools"][0]["name"], "shell");
                        assert_eq!(request["thinking"]["type"], "adaptive");
                    }
                    Wire::Responses => {
                        assert_eq!(request["tools"][0]["type"], "function");
                        assert_eq!(request["store"], false);
                        assert_eq!(request["include"][0], "reasoning.encrypted_content");
                    }
                }
            } else {
                assert_second_round(wire, &request);
            }
            let data = mock_stream(wire, generation);
            http_reply(stream, &data, "text/event-stream")?;
            generation += 1;
        }
        assert_eq!(generation, 3);
        assert_eq!(discoveries, 2);
        assert_eq!(summary, 1);
        Ok(())
    });

    let dir = temp_workdir(wire.label())?;
    let config = || Config {
        base_url: format!("http://{addr}/v1"),
        api_key: Some("mock-key".into()),
        model: "mock".into(),
        workdir: dir.clone(),
        deepseek_thinking: None,
        api_protocol: wire.label().into(),
        reasoning_effort: "auto".into(),
        max_output_tokens: None,
        context_window: None,
        thinking_levels: Vec::new(),
        selected_profile: None,
    };
    let mut agent = app::default_agent(config()).await?;
    let mut shown = String::new();
    assert_eq!(
        agent
            .run("run a test command", &mut |s| shown.push_str(s))
            .await?,
        "confirmed tool"
    );
    assert!(shown.contains("confirmed tool"));
    assert!(!shown.contains("hidden"));
    drop(agent);
    let mut resumed = app::default_agent(config()).await?;
    assert_eq!(
        resumed.run("continue", &mut |_| {}).await?,
        "resumed history"
    );
    drop(resumed);

    let adapter = ModelClient::new(
        &format!("http://{addr}/v1"),
        Some("mock-key".into()),
        "mock".into(),
    )?
    .with_protocol(wire.label())?;
    assert_eq!(
        adapter
            .text_completion("summarize safely", "notes", 512)
            .await?
            .text,
        "summary ok"
    );
    server.join().expect("mock server thread panicked")?;
    std::fs::remove_dir_all(&dir)?;
    Ok(())
}

#[tokio::test]
async fn anthropic_full_agent_tool_loop_restart_and_summary_without_api() -> Result<()> {
    exercise(Wire::Anthropic)
        .await
        .context("Anthropic mock protocol integration")
}

#[tokio::test]
async fn responses_full_agent_tool_loop_restart_and_summary_without_api() -> Result<()> {
    exercise(Wire::Responses)
        .await
        .context("Responses mock protocol integration")
}

#[tokio::test]
async fn responses_summary_rejection_falls_back_once_and_remembers_choice() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let addr = listener.local_addr()?;
    let server = thread::spawn(move || -> Result<Vec<Value>> {
        let mut received = Vec::new();
        for step in 0..3 {
            let (mut stream, _) = listener.accept()?;
            let (line, _, request) = read_http(&stream)?;
            assert!(line.starts_with("POST /v1/responses "));
            received.push(request);
            if step == 0 {
                let body = r#"{"error":{"message":"unknown field reasoning.summary"}}"#;
                write!(stream, "HTTP/1.1 400 Bad Request\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())?;
                stream.flush()?;
            } else {
                let data = sse_event(
                    "response.completed",
                    json!({
                        "type":"response.completed","response":{"status":"completed",
                        "output":[{"type":"message","content":[{"type":"output_text","text":"ok"}]}],
                        "usage":{"input_tokens":9,"output_tokens":2}}
                    }),
                );
                // Streaming adapter consumes explicit text deltas.
                let payload = format!(
                    "{}{}",
                    sse_event(
                        "response.output_text.delta",
                        json!({"type":"response.output_text.delta","delta":"ok"})
                    ),
                    data
                );
                http_reply(stream, &payload, "text/event-stream")?;
            }
        }
        Ok(received)
    });
    let client = ModelClient::new(&format!("http://{addr}/v1"), None, "mock".into())?
        .with_protocol("responses")?
        .with_reasoning("high".into(), None);
    let messages = [kroma::protocol::Message::new("user", "test")];
    for _ in 0..2 {
        let answer = client
            .complete_with_tools(&messages, &[], &mut |_| {})
            .await?;
        assert_eq!(answer.message.content.as_deref(), Some("ok"));
    }
    let requests = server.join().expect("mock server panicked")?;
    assert_eq!(requests.len(), 3, "only the first request may retry");
    assert_eq!(requests[0]["reasoning"]["summary"], "auto");
    assert!(requests[1]["reasoning"].get("summary").is_none());
    assert!(requests[2]["reasoning"].get("summary").is_none());
    assert!(requests
        .iter()
        .all(|request| request["reasoning"]["effort"] == "high"));
    Ok(())
}

#[tokio::test]
async fn responses_partial_stream_error_never_replays_the_request() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let address = listener.local_addr()?;
    let server = thread::spawn(move || -> Result<()> {
        let (stream, _) = listener.accept()?;
        let (_, _, request) = read_http(&stream)?;
        assert_eq!(request["reasoning"]["summary"], "auto");
        let body = format!(
            "{}{}",
            sse_event(
                "response.output_text.delta",
                json!({"type":"response.output_text.delta","delta":"partial"})
            ),
            sse_event(
                "error",
                json!({"type":"error","message":"unsupported reasoning.summary"})
            )
        );
        http_reply(stream, &body, "text/event-stream")?;
        std::thread::sleep(Duration::from_millis(180));
        listener.set_nonblocking(true)?;
        assert!(
            listener.accept().is_err(),
            "the provider must not retry after any streaming data"
        );
        Ok(())
    });
    let client = ModelClient::new(&format!("http://{address}/v1"), None, "mock".into())?
        .with_protocol("responses")?;
    let mut visible = String::new();
    let failure = client
        .complete_with_tools(
            &[kroma::protocol::Message::new("user", "task")],
            &[],
            &mut |s| visible.push_str(s),
        )
        .await;
    assert!(failure.is_err());
    assert_eq!(visible, "partial");
    server.join().expect("mock server panicked")?;
    Ok(())
}
