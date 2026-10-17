use kroma_core::agent_loop::{build_default_runtime, AgentRunConfig, AgentRunner};
use kroma_core::budget::BudgetConfig;
use kroma_core::env::LocalHostEnv;
use kroma_core::events::EventLogger;
use kroma_core::messages::FinishReason;
use kroma_core::provider::chat_completions::{
    validate_base_url, ChatCompletionsClient, ChatCompletionsConfig,
};
use kroma_core::provider::{
    ModelProvider, ProviderCompletion, ProviderError, ProviderRequest,
};
use kroma_core::tool::policy::{NonInteractiveApprovalHandler, PolicyEngine, PolicyMode};
use kroma_core::types::{RunStatus, TerminalReason};
use std::fs;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::watch;
use uuid::Uuid;

/// B16: retries are admitted before their wire request, not billed after the fact.
#[tokio::test]
async fn retry_token_allowance_blocks_transmission_and_preserves_unknown_spend() {
    use kroma_core::budget::TokenBudget;
    use kroma_core::provider::RetryBudget;
    for protocol in ["chat", "anthropic", "responses"] {
    for (cap, expected_hits, report_usage) in [(100, 1, true), (200, 2, true), (200, 2, false)] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let server_hits = Arc::clone(&hits);
        let server = tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let n = server_hits.fetch_add(1, Ordering::SeqCst);
                let mut request = Vec::new();
                loop {
                    let mut buf = [0; 4096];
                    let read = socket.read(&mut buf).await.unwrap();
                    if read == 0 { break; }
                    request.extend_from_slice(&buf[..read]);
                    if let Some(pos) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                        let header = String::from_utf8_lossy(&request[..pos]);
                        let size: usize = header.lines().find_map(|line| {
                            let (key, value) = line.split_once(':')?;
                            key.eq_ignore_ascii_case("content-length").then(|| value.trim().parse().unwrap())
                        }).unwrap_or(0);
                        if request.len() >= pos + 4 + size { break; }
                    }
                }
                let success_body = match protocol {
                    "anthropic" => r#"{"id":"fixture","type":"message","role":"assistant","content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn","usage":{"input_tokens":7,"output_tokens":3}}"#,
                    "responses" => r#"{"id":"fixture","status":"completed","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"ok"}]}],"usage":{"input_tokens":7,"output_tokens":3}}"#,
                    _ => r#"{"choices":[{"index":0,"finish_reason":"stop","message":{"role":"assistant","content":"ok"}}],"usage":{"prompt_tokens":7,"completion_tokens":3}}"#,
                };
                let mut payload: serde_json::Value = serde_json::from_str(success_body).unwrap();
                payload["model"] = serde_json::json!("returned-checkpoint");
                if !report_usage {
                    payload.as_object_mut().unwrap().remove("usage");
                }
                let success_body = payload.to_string();
                let (status, body) = if n == 0 {
                    ("503 Service Unavailable", "retry")
                } else {
                    ("200 OK", success_body.as_str())
                };
                let response = format!("HTTP/1.1 {}\r\nContent-Type: application/json\r\nRetry-After: 0\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", status, body.len(), body);
                socket.write_all(response.as_bytes()).await.unwrap();
            }
        });
        let ledger = Arc::new(TokenBudget::new(Some(cap)));
        let first = ledger.try_reserve(100).unwrap();
        let base_url = format!("http://{}", addr);
        let client: Box<dyn ModelProvider> = match protocol {
            "anthropic" => Box::new(kroma_core::provider::anthropic_messages::AnthropicMessagesClient::new(
                kroma_core::provider::anthropic_messages::AnthropicMessagesConfig {
                    base_url, api_key: "fixture".into(), model: "fixture".into(), max_retries: 2, max_tokens: 100,
                }).unwrap()),
            "responses" => Box::new(kroma_core::provider::openai_responses::OpenAiResponsesClient::new(
                kroma_core::provider::openai_responses::OpenAiResponsesConfig {
                    base_url, api_key: "fixture".into(), model: "fixture".into(), max_retries: 2, reasoning_effort: None,
                }).unwrap()),
            _ => Box::new(ChatCompletionsClient::new(ChatCompletionsConfig {
                base_url, api_key: "fixture".into(), model: "fixture".into(), max_retries: 2, reasoning_effort: None,
            }).unwrap()),
        };
        let (_tx, rx) = watch::channel(false);
        let result = client.complete(ProviderRequest {
            retry_budget: Some(RetryBudget { ledger: Arc::clone(&ledger), per_attempt: 100 }),
            ..ProviderRequest::default()
        }, &BudgetConfig::default(), rx).await;
        if cap == 100 {
            assert!(matches!(result, Err(ProviderError::RetryBudgetExhausted)), "{result:?}");
        } else {
            let completion = result.unwrap();
            assert_eq!(completion.attempts_used, 2);
            assert_eq!(completion.reported_model.as_deref(), Some("returned-checkpoint"));
        }
        // The caller's first-attempt allowance remains an unknown hold, independent of retry usage.
        drop(first);
        assert_eq!(hits.load(Ordering::SeqCst), expected_hits);
        let snapshot = ledger.accounting_snapshot();
        let unknown = if report_usage { 100 } else { 200 };
        let settled = if cap == 200 && report_usage { 10 } else { 0 };
        assert_eq!(snapshot.unknown_tokens, unknown);
        assert_eq!(snapshot.settled_tokens, settled);
        assert_eq!(snapshot.spent, unknown + settled);
        server.abort();
        let _ = server.await;
    }
    }
}

/// Real runner integration: a 503 is an ambiguous post-dispatch failure.
///
/// T04 (KR-02) changed the *allowance* here, not the fail-closed rule. Before T04 the first request
/// reserved the whole run allowance as its output cap, so the retry could not reserve anything and
/// the run stopped after one wire attempt. A request now reserves a per-request output allowance,
/// so the retry is affordable and both attempts are transmitted. What did not change: neither
/// attempt is refunded — both are charged as unknown, and the ledger ends with no reservation
/// outstanding.
#[tokio::test]
async fn runner_retry_exhaustion_retains_first_attempt_and_sends_once() {
    use kroma_core::budget::TokenBudget;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let hits = Arc::new(AtomicUsize::new(0));
    let server_hits = Arc::clone(&hits);
    let server = tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            server_hits.fetch_add(1, Ordering::SeqCst);
            let mut request = Vec::new();
            loop {
                let mut buf = [0; 4096];
                let n = socket.read(&mut buf).await.unwrap();
                if n == 0 { break; }
                request.extend_from_slice(&buf[..n]);
                if let Some(pos) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                    let header = String::from_utf8_lossy(&request[..pos]);
                    let len: usize = header.lines().find_map(|line| {
                        let (key, value) = line.split_once(':')?;
                        key.eq_ignore_ascii_case("content-length").then(|| value.trim().parse().unwrap())
                    }).unwrap_or(0);
                    if request.len() >= pos + 4 + len { break; }
                }
            }
            socket.write_all(b"HTTP/1.1 503 Service Unavailable\r\nRetry-After: 0\r\nContent-Length: 5\r\nConnection: close\r\n\r\nretry").await.unwrap();
        }
    });
    let root = std::env::temp_dir().join(format!("b16-runner-retry-{}", Uuid::new_v4()));
    let workspace = root.join("ws");
    fs::create_dir_all(&workspace).unwrap();
    let budget = BudgetConfig::default();
    let env = Arc::new(LocalHostEnv::new(&workspace, root.join("state")).unwrap());
    let runtime = build_default_runtime(env, PolicyEngine::new(
        PolicyMode::ReadOnly, Arc::new(NonInteractiveApprovalHandler)), budget.clone(), None);
    let cfg = AgentRunConfig::new("retry accounting fixture", budget);
    let logger = EventLogger::new(cfg.run_id, Some(root.join("state")), false).unwrap();
    let provider = Arc::new(ChatCompletionsClient::new(ChatCompletionsConfig {
        base_url: format!("http://{}", addr), api_key: "fixture".into(), model: "fixture".into(),
        max_retries: 2, reasoning_effort: None,
    }).unwrap());
    let ledger = Arc::new(TokenBudget::new(Some(64000)));
    let (_tx, rx) = watch::channel(false);
    let (summary, _) = AgentRunner::new(cfg, provider, runtime, rx)
        .with_token_budget(Arc::clone(&ledger)).run(logger).await;
    assert!(
        matches!(summary.terminal_reason, TerminalReason::ProviderProtocolError(_)),
        "the run ends on the provider's 503, not on a budget refusal: {:?}",
        summary.terminal_reason
    );
    assert_eq!(
        hits.load(Ordering::SeqCst),
        3,
        "one attempt plus two retries must all be affordable after a transient failure"
    );
    let accounting = ledger.accounting_snapshot();
    assert!(
        accounting.spent >= 2 * kroma_core::budget::DEFAULT_PER_REQUEST_OUTPUT_TOKENS,
        "each transmitted attempt holds at least its own output allowance: {}",
        accounting.spent
    );
    assert_eq!(
        accounting.unknown_tokens, accounting.spent,
        "an ambiguous 503 charges the reservation as unknown; it is never refunded"
    );
    assert_eq!(accounting.settled_tokens, 0);
    assert_eq!(accounting.reserved, 0, "no reservation may leak");
    assert!(accounting.spent <= 64000, "the cap still holds");
    server.abort();
    let _ = server.await;
    fs::remove_dir_all(root).unwrap();
}

/// B16: the reported identity must come from the stream too, not only from a non-streaming body.
/// Each protocol asserts: the returned model is kept even though the request used an alias; a stream
/// that changes its identity mid-flight is refused instead of attributing usage to the wrong model;
/// and a stream that never states one stays `None` rather than inheriting the request alias.
#[tokio::test]
async fn streaming_responses_report_their_own_model_identity() {
    use kroma_core::provider::{anthropic_messages, chat_completions, openai_responses};

    fn sse(events: &[(&str, &str)]) -> String {
        let mut body = String::new();
        for (name, data) in events {
            body.push_str(&format!("event: {}\ndata: {}\n\n", name, data));
        }
        body
    }

    // A server that always answers one fixed SSE body with chunked transfer encoding.
    async fn serve(body: String) -> std::net::SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let body = body.clone();
                tokio::spawn(async move {
                    let mut request = Vec::new();
                    loop {
                        let mut buf = [0; 4096];
                        let Ok(read) = socket.read(&mut buf).await else { return };
                        if read == 0 {
                            return;
                        }
                        request.extend_from_slice(&buf[..read]);
                        if let Some(pos) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                            let header = String::from_utf8_lossy(&request[..pos]);
                            let len: usize = header
                                .lines()
                                .find_map(|line| {
                                    let (key, value) = line.split_once(':')?;
                                    key.eq_ignore_ascii_case("content-length")
                                        .then(|| value.trim().parse().unwrap())
                                })
                                .unwrap_or(0);
                            if request.len() >= pos + 4 + len {
                                break;
                            }
                        }
                    }
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                });
            }
        });
        addr
    }

    let chat_ok = sse(&[
        ("", r#"{"id":"c1","model":"stream-model","choices":[{"index":0,"delta":{"content":"hi"}}]}"#),
        ("", r#"{"id":"c1","model":"stream-model","choices":[{"index":0,"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":1}}"#),
        ("", "[DONE]"),
    ]);
    let anthropic_ok = sse(&[
        ("message_start", r#"{"type":"message_start","message":{"id":"m1","model":"stream-model","role":"assistant","content":[],"usage":{"input_tokens":3,"output_tokens":0}}}"#),
        ("content_block_start", r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#),
        ("content_block_delta", r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}"#),
        ("content_block_stop", r#"{"type":"content_block_stop","index":0}"#),
        ("message_delta", r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}}"#),
        ("message_stop", r#"{"type":"message_stop"}"#),
    ]);
    let responses_ok = sse(&[
        ("response.created", r#"{"type":"response.created","response":{"id":"r1","status":"in_progress","model":"stream-model"}}"#),
        ("response.output_text.delta", r#"{"type":"response.output_text.delta","delta":"hi"}"#),
        ("response.completed", r#"{"type":"response.completed","response":{"id":"r1","status":"completed","model":"stream-model","usage":{"input_tokens":3,"output_tokens":1}}}"#),
    ]);
    // Same shape but the identity changes mid-stream: attribution must not be guessed. Chat
    // completions repeat the model on every chunk, so a contradiction is expressible there.
    let conflicting = sse(&[
        ("", r#"{"id":"c1","model":"first-model","choices":[{"index":0,"delta":{"content":"hi"}}]}"#),
        ("", r#"{"id":"c1","model":"changed-model","choices":[{"index":0,"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":1}}"#),
        ("", "[DONE]"),
    ]);

    let (_tx, rx) = watch::channel(false);

    // chat-completions
    let addr = serve(chat_ok).await;
    let client = chat_completions::ChatCompletionsClient::new(chat_completions::ChatCompletionsConfig {
        base_url: format!("http://{}", addr), api_key: "fixture".into(), model: "request-alias".into(),
        max_retries: 0, reasoning_effort: None,
    }).unwrap();
    let done = client.complete(ProviderRequest { stream: true, ..ProviderRequest::default() }, &BudgetConfig::default(), rx.clone()).await.unwrap();
    assert_eq!(done.reported_model.as_deref(), Some("stream-model"), "chat stream identity");

    // anthropic-messages
    let addr = serve(anthropic_ok).await;
    let client = anthropic_messages::AnthropicMessagesClient::new(anthropic_messages::AnthropicMessagesConfig {
        base_url: format!("http://{}", addr), api_key: "fixture".into(), model: "request-alias".into(),
        max_retries: 0, max_tokens: 64,
    }).unwrap();
    let done = client.complete(ProviderRequest { stream: true, ..ProviderRequest::default() }, &BudgetConfig::default(), rx.clone()).await.unwrap();
    assert_eq!(done.reported_model.as_deref(), Some("stream-model"), "anthropic stream identity");

    // openai-responses
    let addr = serve(responses_ok).await;
    let client = openai_responses::OpenAiResponsesClient::new(openai_responses::OpenAiResponsesConfig {
        base_url: format!("http://{}", addr), api_key: "fixture".into(), model: "request-alias".into(),
        max_retries: 0, reasoning_effort: None,
    }).unwrap();
    let done = client.complete(ProviderRequest { stream: true, ..ProviderRequest::default() }, &BudgetConfig::default(), rx).await.unwrap();
    assert_eq!(done.reported_model.as_deref(), Some("stream-model"), "responses stream identity");

    // A stream that contradicts itself is refused (attribution must not be guessed).
    let addr = serve(conflicting).await;
    let client = chat_completions::ChatCompletionsClient::new(chat_completions::ChatCompletionsConfig {
        base_url: format!("http://{}", addr), api_key: "fixture".into(), model: "request-alias".into(),
        max_retries: 0, reasoning_effort: None,
    }).unwrap();
    let (_tx2, rx2) = watch::channel(false);
    let result = client.complete(ProviderRequest { stream: true, ..ProviderRequest::default() }, &BudgetConfig::default(), rx2).await;
    match result {
        Err(err) => {
            let text = err.to_string();
            assert!(
                text.contains("model identity"),
                "the refusal must name the identity conflict, got: {text}"
            );
        }
        Ok(done) => panic!(
            "a contradictory stream must be refused, not silently attributed: {:?}",
            done.reported_model
        ),
    }
}

#[test]
fn test_base_url_security_rules() {
    // Remote HTTP rejected
    assert!(matches!(
        validate_base_url("http://api.openai.com/v1"),
        Err(ProviderError::InsecureUrl(_))
    ));
    // Userinfo rejected
    assert!(matches!(
        validate_base_url("https://user:secret@api.openai.com/v1"),
        Err(ProviderError::InsecureUrl(_))
    ));
    // Localhost HTTP allowed
    assert!(validate_base_url("http://127.0.0.1:8080/v1").is_ok());
    assert!(validate_base_url("http://localhost:9000").is_ok());
    assert!(validate_base_url("http://[::1]:8080/v1").is_ok());
    // Remote HTTPS allowed
    assert!(validate_base_url("https://api.openai.com/v1").is_ok());
    // Query and fragment rejected
    assert!(matches!(
        validate_base_url("https://api.openai.com/v1?foo=bar"),
        Err(ProviderError::InsecureUrl(_))
    ));
    assert!(matches!(
        validate_base_url("https://api.openai.com/v1#frag"),
        Err(ProviderError::InsecureUrl(_))
    ));
}

#[test]
fn test_base_url_rejects_control_characters_and_whitespace() {
    // F-04: the URL is interpolated into a hand-written HTTP request line, so any control byte or
    // whitespace anywhere in the URL must be rejected (header/request-line injection guard).
    let hostile_urls = [
        "https://api.openai.com/v1\r\nX-Injected: 1",
        "https://api.openai.com/v1\nGET / HTTP/1.1",
        "https://api.openai.com/v1 path",
        "https://api.openai.com/v1\t/tab",
        "https://api.openai.com/v1\u{7f}",
        "https://api.openai.com/v1\u{0}",
    ];
    for hostile in hostile_urls {
        assert!(
            matches!(
                validate_base_url(hostile),
                Err(ProviderError::InsecureUrl(_))
            ),
            "hostile URL must be rejected: {:?}",
            hostile
        );
    }

    // A legitimate URL with a nested base path still passes.
    assert!(validate_base_url("https://api.openai.com/v1/chat").is_ok());
}

#[tokio::test]
async fn test_chunked_sse_streaming_and_pre_chunk_disconnect_retry_and_oom_guard() {
    use kroma_core::messages::{AssistantTurn, ReasoningBlock, TranscriptItem};
    use kroma_core::provider::chat_completions::{
        serialize_openai_request_body, serialize_openai_request_body_with_effort,
    };
    use kroma_core::provider::sse::parse_usage_json;

    // 1. Verify serialize_openai_request_body omits private "reasoning_blocks" key
    let serialized = serialize_openai_request_body(
        "gpt-test",
        "sys",
        &[],
        &[TranscriptItem::Assistant(AssistantTurn {
            content: Some("hello".into()),
            reasoning_blocks: vec![ReasoningBlock {
                text: Some("step 1".into()),
                signature: Some("sig_abc".into()),
                opaque_payload: None,
            }],
            tool_calls: Vec::new(),
            finish_reason: FinishReason::Stop,
            usage: None,
        })],
        true,
    );
    assert!(!serialized.contains("\"reasoning_blocks\""));
    assert!(serialized.contains("\"reasoning_content\":\"step 1\""));
    assert!(serialized.contains("\"thinking\""));
    assert!(!serialized.contains("\"reasoning_effort\""));

    let serialized_effort =
        serialize_openai_request_body_with_effort("deepseek", "sys", &[], &[], true, Some("high"));
    assert!(serialized_effort.contains("\"reasoning_effort\":\"high\""));

    let wb_usage = serde_json::json!({
        "prompt_tokens": 1000,
        "completion_tokens": 200,
        "prompt_cache_hit_tokens": 800,
        "completion_thinking_tokens": 120
    });
    let parsed_u = parse_usage_json(&wb_usage).unwrap();
    assert_eq!(parsed_u.cached_input_tokens, Some(800));
    assert_eq!(parsed_u.reasoning_tokens, Some(120));

    // 2. Server disconnects before any business chunk on attempt 1 (Retryable),
    //    then returns Transfer-Encoding: chunked SSE on attempt 2.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let attempts = Arc::new(AtomicUsize::new(0));
    let attempts_srv = Arc::clone(&attempts);

    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            let n = attempts_srv.fetch_add(1, Ordering::SeqCst) + 1;
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf).await;
                if n == 1 {
                    // Send 200 OK chunked with only keepalive comment then disconnect before any business chunk
                    let resp = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n\
                                c\r\n: keepalive\n\r\n";
                    let _ = sock.write_all(resp.as_bytes()).await;
                } else {
                    let chunk1 = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Chunked \"}}]}\n\n";
                    let chunk2 = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"SSE OK\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
                    let wire = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n\
                         {:x}\r\n{}\r\n{:x}\r\n{}\r\n0\r\n\r\n",
                        chunk1.len(),
                        chunk1,
                        chunk2.len(),
                        chunk2
                    );
                    let _ = sock.write_all(wire.as_bytes()).await;
                }
            });
        }
    });

    let client = ChatCompletionsClient::new(ChatCompletionsConfig {
        base_url: format!("http://127.0.0.1:{}", addr.port()),
        api_key: "sk-test".into(),
        model: "gpt-test".into(),
        max_retries: 2,
        reasoning_effort: None,
    })
    .unwrap();
    let (_tx, rx) = watch::channel(false);
    let comp = client
        .complete(
            ProviderRequest {
                system_prompt: "sys".into(),
                tools: Vec::new(),
                messages: Vec::new(),
                stream: true,
                cache_breakpoints: Vec::new(),
                max_output_tokens: None,
                retry_budget: None,
                wire_attempts: None,
            },
            &BudgetConfig::default(),
            rx,
        )
        .await
        .unwrap();
    assert_eq!(comp.attempts_used, 2);
    assert_eq!(comp.turn.content.as_deref(), Some("Chunked SSE OK"));

    // 3. Oversized 7fffffff chunk size is rejected immediately without OOM
    let oom_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let oom_addr = oom_listener.local_addr().unwrap();
    tokio::spawn(async move {
        if let Ok((mut sock, _)) = oom_listener.accept().await {
            let mut buf = [0u8; 2048];
            let _ = sock.read(&mut buf).await;
            let resp = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n7fffffff\r\n";
            let _ = sock.write_all(resp.as_bytes()).await;
        }
    });
    let oom_client = ChatCompletionsClient::new(ChatCompletionsConfig {
        base_url: format!("http://127.0.0.1:{}", oom_addr.port()),
        api_key: "sk-test".into(),
        model: "gpt-test".into(),
        max_retries: 0,
        reasoning_effort: None,
    })
    .unwrap();
    let (_tx2, rx2) = watch::channel(false);
    let oom_res = oom_client
        .complete(
            ProviderRequest {
                system_prompt: "sys".into(),
                tools: Vec::new(),
                messages: Vec::new(),
                stream: true,
                cache_breakpoints: Vec::new(),
                max_output_tokens: None,
                retry_budget: None,
                wire_attempts: None,
            },
            &BudgetConfig::default(),
            rx2,
        )
        .await;
    assert!(matches!(oom_res, Err(ProviderError::IncompleteStream(_))));
}

#[tokio::test]
async fn test_mock_http_server_429_retry_after_and_redirect_block() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let hit_count = Arc::new(AtomicUsize::new(0));
    let hit_count_srv = Arc::clone(&hit_count);

    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            let count = hit_count_srv.fetch_add(1, Ordering::SeqCst) + 1;
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf).await;
                if count == 1 {
                    let resp = "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 0\r\nContent-Length: 14\r\n\r\nrate limit hit";
                    let _ = sock.write_all(resp.as_bytes()).await;
                } else {
                    let body = r#"{"id":"cmpl-1","choices":[{"index":0,"finish_reason":"stop","message":{"role":"assistant","content":"Recovered after 429","reasoning_content":"thought ok"}}],"usage":{"prompt_tokens":100,"completion_tokens":10,"prompt_tokens_details":{"cached_tokens":64}}}"#;
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                }
            });
        }
    });

    let client = ChatCompletionsClient::new(ChatCompletionsConfig {
        base_url: format!("http://127.0.0.1:{}", addr.port()),
        api_key: "sk-test-secret".into(),
        model: "gpt-test".into(),
        max_retries: 2,
        reasoning_effort: None,
    })
    .unwrap();

    // Ensure Debug output masks API key
    let dbg_str = format!(
        "{:?}",
        ChatCompletionsConfig {
            base_url: "https://api.openai.com".into(),
            api_key: "sk-live-super-secret".into(),
            model: "gpt".into(),
            max_retries: 2,
            reasoning_effort: Some("high".into()),
        }
    );
    assert!(!dbg_str.contains("sk-live-super-secret"));
    assert!(dbg_str.contains("Bearer ***"));

    let (_tx, rx) = watch::channel(false);
    let completion = client
        .complete(
            ProviderRequest {
                system_prompt: "sys".into(),
                tools: Vec::new(),
                messages: Vec::new(),
                stream: false,
                cache_breakpoints: Vec::new(),
                max_output_tokens: None,
                retry_budget: None,
                wire_attempts: None,
            },
            &BudgetConfig::default(),
            rx,
        )
        .await
        .unwrap();

    assert_eq!(completion.attempts_used, 2);
    assert_eq!(hit_count.load(Ordering::SeqCst), 2);
    assert_eq!(
        completion.turn.content.as_deref(),
        Some("Recovered after 429")
    );
    assert_eq!(completion.turn.usage.unwrap().cached_input_tokens, Some(64));

    // Verify 302 redirect is strictly blocked
    let redir_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let redir_addr = redir_listener.local_addr().unwrap();
    tokio::spawn(async move {
        if let Ok((mut sock, _)) = redir_listener.accept().await {
            let mut buf = [0u8; 2048];
            let _ = sock.read(&mut buf).await;
            let resp = "HTTP/1.1 302 Found\r\nLocation: https://evil.example.com/\r\nContent-Length: 0\r\n\r\n";
            let _ = sock.write_all(resp.as_bytes()).await;
        }
    });

    let redir_client = ChatCompletionsClient::new(ChatCompletionsConfig {
        base_url: format!("http://127.0.0.1:{}", redir_addr.port()),
        api_key: "sk-test".into(),
        model: "gpt-test".into(),
        max_retries: 2,
        reasoning_effort: None,
    })
    .unwrap();
    let (_tx2, rx2) = watch::channel(false);
    let redir_res = redir_client
        .complete(
            ProviderRequest {
                system_prompt: "sys".into(),
                tools: Vec::new(),
                messages: Vec::new(),
                stream: false,
                cache_breakpoints: Vec::new(),
                max_output_tokens: None,
                retry_budget: None,
                wire_attempts: None,
            },
            &BudgetConfig::default(),
            rx2,
        )
        .await;
    assert_eq!(redir_res.unwrap_err(), ProviderError::RedirectBlocked(302));
}

#[tokio::test]
async fn test_agent_loop_zero_side_effects_on_length_truncation_over_sse() {
    let tmp_ws = std::env::temp_dir().join(format!("kroma-ws-len-{}", Uuid::new_v4()));
    let tmp_state = std::env::temp_dir().join(format!("kroma-state-len-{}", Uuid::new_v4()));
    fs::create_dir_all(&tmp_ws).unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    // Server returns SSE stream with valid JSON arguments for `write`, but finish_reason="length"
    tokio::spawn(async move {
        if let Ok((mut sock, _)) = listener.accept().await {
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf).await;
            let sse_body = "\
data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_trunc\",\"function\":{\"name\":\"write\",\"arguments\":\"{\\\"path\\\":\\\"should_not_exist.txt\\\",\\\"content\\\":\\\"boom\\\"}\"}}]},\"finish_reason\":\"length\"}]}\n\n\
data: [DONE]\n\n";
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n{}",
                sse_body.len(),
                sse_body
            );
            let _ = sock.write_all(resp.as_bytes()).await;
        }
    });

    let provider = Arc::new(
        ChatCompletionsClient::new(ChatCompletionsConfig {
            base_url: format!("http://127.0.0.1:{}", addr.port()),
            api_key: "sk-test".into(),
            model: "gpt-test".into(),
            max_retries: 0,
            reasoning_effort: None,
        })
        .unwrap(),
    );

    let budget = BudgetConfig::default();
    let env = Arc::new(LocalHostEnv::new(&tmp_ws, tmp_state.clone()).unwrap());
    let policy = PolicyEngine::new(
        PolicyMode::FullAutonomous,
        Arc::new(NonInteractiveApprovalHandler),
    );
    let runtime = build_default_runtime(env, policy, budget.clone(), None);
    let cfg = AgentRunConfig::new("try write", budget);
    let logger = EventLogger::new(cfg.run_id, Some(tmp_state.clone()), false).unwrap();
    let (_tx, rx) = watch::channel(false);

    let (summary, _logger) = AgentRunner::new(cfg, provider, runtime, rx)
        .run(logger)
        .await;
    assert_eq!(summary.status, RunStatus::Failed);
    assert!(matches!(
        summary.terminal_reason,
        TerminalReason::ProviderProtocolError(_)
    ));
    assert_eq!(summary.tool_calls_used, 0);
    assert!(!tmp_ws.join("should_not_exist.txt").exists());

    // Ensure FinishReason wire helper is exercised
    assert_eq!(FinishReason::Length.as_wire(), "length");

    let _ = fs::remove_dir_all(&tmp_ws);
    let _ = fs::remove_dir_all(&tmp_state);
}

#[tokio::test]
async fn test_http_smuggling_dual_te_cl_rejected_and_chunked_error_body_decoded() {
    // 1. Response containing both Transfer-Encoding: chunked and Content-Length must be rejected
    let smug_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let smug_addr = smug_listener.local_addr().unwrap();
    tokio::spawn(async move {
        if let Ok((mut sock, _)) = smug_listener.accept().await {
            let mut buf = [0u8; 2048];
            let _ = sock.read(&mut buf).await;
            let resp = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\nContent-Length: 42\r\n\r\n0\r\n\r\n";
            let _ = sock.write_all(resp.as_bytes()).await;
        }
    });

    let smug_client = ChatCompletionsClient::new(ChatCompletionsConfig {
        base_url: format!("http://127.0.0.1:{}", smug_addr.port()),
        api_key: "sk-test".into(),
        model: "gpt-test".into(),
        max_retries: 2,
        reasoning_effort: None,
    })
    .unwrap();
    let (_tx, rx) = watch::channel(false);
    let smug_res = smug_client
        .complete(
            ProviderRequest {
                system_prompt: "sys".into(),
                tools: Vec::new(),
                messages: Vec::new(),
                stream: false,
                cache_breakpoints: Vec::new(),
                max_output_tokens: None,
                retry_budget: None,
                wire_attempts: None,
            },
            &BudgetConfig::default(),
            rx,
        )
        .await;
    match smug_res {
        Err(ProviderError::Transport(msg)) => {
            assert!(msg.contains("Transfer-Encoding") && msg.contains("Content-Length"));
        }
        other => panic!(
            "expected Transport error for TE+CL smuggling response, got {:?}",
            other
        ),
    }

    // 2. Non-2xx error response with Transfer-Encoding: chunked decodes the chunked error body
    let err_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let err_addr = err_listener.local_addr().unwrap();
    tokio::spawn(async move {
        if let Ok((mut sock, _)) = err_listener.accept().await {
            let mut buf = [0u8; 2048];
            let _ = sock.read(&mut buf).await;
            let err_json = r#"{"error":{"message":"invalid model parameter"}}"#;
            let resp = format!(
                "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n{}\r\n0\r\n\r\n",
                err_json.len(),
                err_json
            );
            let _ = sock.write_all(resp.as_bytes()).await;
        }
    });

    let err_client = ChatCompletionsClient::new(ChatCompletionsConfig {
        base_url: format!("http://127.0.0.1:{}", err_addr.port()),
        api_key: "sk-test".into(),
        model: "gpt-test".into(),
        max_retries: 0,
        reasoning_effort: None,
    })
    .unwrap();
    let (_tx2, rx2) = watch::channel(false);
    let err_res = err_client
        .complete(
            ProviderRequest {
                system_prompt: "sys".into(),
                tools: Vec::new(),
                messages: Vec::new(),
                stream: true,
                cache_breakpoints: Vec::new(),
                max_output_tokens: None,
                retry_budget: None,
                wire_attempts: None,
            },
            &BudgetConfig::default(),
            rx2,
        )
        .await;
    assert_eq!(
        err_res.unwrap_err(),
        ProviderError::HttpStatus {
            status: 400,
            body: r#"{"error":{"message":"invalid model parameter"}}"#.to_string(),
        }
    );
}

/// F-04: a response carrying two *different* `Content-Length` values is an ambiguous framing and
/// must be rejected, not silently trusted.
#[tokio::test]
async fn test_duplicate_conflicting_content_length_is_rejected() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        if let Ok((mut sock, _)) = listener.accept().await {
            let mut buf = [0u8; 2048];
            let _ = sock.read(&mut buf).await;
            let resp = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 10\r\nContent-Length: 20\r\n\r\n0123456789";
            let _ = sock.write_all(resp.as_bytes()).await;
        }
    });

    let client = ChatCompletionsClient::new(ChatCompletionsConfig {
        base_url: format!("http://127.0.0.1:{}", addr.port()),
        api_key: "sk-test".into(),
        model: "gpt-test".into(),
        max_retries: 0,
        reasoning_effort: None,
    })
    .unwrap();
    let (_tx, rx) = watch::channel(false);
    let res = client
        .complete(
            ProviderRequest {
                system_prompt: "sys".into(),
                tools: Vec::new(),
                messages: Vec::new(),
                stream: false,
                cache_breakpoints: Vec::new(),
                max_output_tokens: None,
                retry_budget: None,
                wire_attempts: None,
            },
            &BudgetConfig::default(),
            rx,
        )
        .await;
    match res {
        Err(ProviderError::Transport(msg)) => {
            assert!(
                msg.contains("conflicting Content-Length"),
                "unexpected transport error: {}",
                msg
            );
        }
        other => panic!(
            "conflicting Content-Length must be rejected, got {:?}",
            other.map(|_| ())
        ),
    }
}

/// F-04: a declared `Content-Length` that is not fully delivered (peer closes early) is a premature
/// EOF and must be rejected, never treated as a complete body.
#[tokio::test]
async fn test_premature_eof_short_body_is_rejected() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        if let Ok((mut sock, _)) = listener.accept().await {
            let mut buf = [0u8; 2048];
            let _ = sock.read(&mut buf).await;
            // Promise 500 bytes, deliver 50, then drop the connection.
            let resp = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 500\r\n\r\n0123456789012345678901234567890123456789012345678901234567";
            let _ = sock.write_all(resp.as_bytes()).await;
            // socket dropped here -> peer closes early
        }
    });

    let client = ChatCompletionsClient::new(ChatCompletionsConfig {
        base_url: format!("http://127.0.0.1:{}", addr.port()),
        api_key: "sk-test".into(),
        model: "gpt-test".into(),
        max_retries: 0,
        reasoning_effort: None,
    })
    .unwrap();
    let (_tx, rx) = watch::channel(false);
    let res = client
        .complete(
            ProviderRequest {
                system_prompt: "sys".into(),
                tools: Vec::new(),
                messages: Vec::new(),
                stream: false,
                cache_breakpoints: Vec::new(),
                max_output_tokens: None,
                retry_budget: None,
                wire_attempts: None,
            },
            &BudgetConfig::default(),
            rx,
        )
        .await;
    match res {
        Err(ProviderError::IncompleteStream(msg)) => {
            assert!(
                msg.contains("prematurely"),
                "unexpected incomplete-stream error: {}",
                msg
            );
        }
        other => panic!(
            "premature EOF must be rejected, got {:?}",
            other.map(|_| ())
        ),
    }
}

/// F-04 / transport: a chunked response that delivers one data chunk and then closes *without* the
/// trailing CRLF and *without* the terminating 0-size chunk was previously accepted as a complete
/// body, because the CRLF read's timeout/EOF was swallowed. It must be reported as an incomplete
/// stream. A well-formed chunked response must still succeed.
#[tokio::test]
async fn test_chunked_missing_trailing_crlf_is_incomplete_but_wellformed_succeeds() {
    let body = r#"{"id":"cmpl-1","choices":[{"index":0,"finish_reason":"stop","message":{"role":"assistant","content":"hi"}}],"usage":{"prompt_tokens":1,"completion_tokens":1}}"#;

    // (a) One chunk, no trailing CRLF, no 0 chunk, then disconnect.
    let broken = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let broken_addr = broken.local_addr().unwrap();
    let broken_body = body.to_string();
    tokio::spawn(async move {
        if let Ok((mut sock, _)) = broken.accept().await {
            let mut buf = [0u8; 2048];
            let _ = sock.read(&mut buf).await;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n{}",
                broken_body.len(),
                broken_body
            );
            let _ = sock.write_all(resp.as_bytes()).await;
        }
    });
    let broken_client = ChatCompletionsClient::new(ChatCompletionsConfig {
        base_url: format!("http://127.0.0.1:{}", broken_addr.port()),
        api_key: "sk-test".into(),
        model: "gpt-test".into(),
        max_retries: 0,
        reasoning_effort: None,
    })
    .unwrap();
    let (_tx, rx) = watch::channel(false);
    let broken_res = broken_client
        .complete(
            ProviderRequest {
                system_prompt: "sys".into(),
                tools: Vec::new(),
                messages: Vec::new(),
                stream: false,
                cache_breakpoints: Vec::new(),
                max_output_tokens: None,
                retry_budget: None,
                wire_attempts: None,
            },
            &BudgetConfig::default(),
            rx,
        )
        .await;
    match broken_res {
        Err(ProviderError::IncompleteStream(msg)) => {
            assert!(msg.contains("CRLF"), "unexpected error: {}", msg);
        }
        other => panic!(
            "a chunked body missing its terminator must be incomplete, got {:?}",
            other.map(|_| ())
        ),
    }

    // (b) A well-formed chunked response (data chunk + CRLF + 0 chunk + CRLF) still succeeds.
    let good = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let good_addr = good.local_addr().unwrap();
    let good_body = body.to_string();
    tokio::spawn(async move {
        if let Ok((mut sock, _)) = good.accept().await {
            let mut buf = [0u8; 2048];
            let _ = sock.read(&mut buf).await;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n{}\r\n0\r\n\r\n",
                good_body.len(),
                good_body
            );
            let _ = sock.write_all(resp.as_bytes()).await;
        }
    });
    let good_client = ChatCompletionsClient::new(ChatCompletionsConfig {
        base_url: format!("http://127.0.0.1:{}", good_addr.port()),
        api_key: "sk-test".into(),
        model: "gpt-test".into(),
        max_retries: 0,
        reasoning_effort: None,
    })
    .unwrap();
    let (_tx2, rx2) = watch::channel(false);
    let good_res = good_client
        .complete(
            ProviderRequest {
                system_prompt: "sys".into(),
                tools: Vec::new(),
                messages: Vec::new(),
                stream: false,
                cache_breakpoints: Vec::new(),
                max_output_tokens: None,
                retry_budget: None,
                wire_attempts: None,
            },
            &BudgetConfig::default(),
            rx2,
        )
        .await;
    assert!(
        good_res.is_ok(),
        "a well-formed chunked body must still succeed: {:?}",
        good_res.err()
    );
}

/// F-04: an unknown or multi-valued `Transfer-Encoding` (e.g. `gzip, bogus`) must be rejected
/// rather than mis-parsed as chunked.
#[tokio::test]
async fn test_bogus_transfer_encoding_is_rejected() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        if let Ok((mut sock, _)) = listener.accept().await {
            let mut buf = [0u8; 2048];
            let _ = sock.read(&mut buf).await;
            let resp = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: gzip, bogus\r\n\r\n";
            let _ = sock.write_all(resp.as_bytes()).await;
        }
    });

    let client = ChatCompletionsClient::new(ChatCompletionsConfig {
        base_url: format!("http://127.0.0.1:{}", addr.port()),
        api_key: "sk-test".into(),
        model: "gpt-test".into(),
        max_retries: 0,
        reasoning_effort: None,
    })
    .unwrap();
    let (_tx, rx) = watch::channel(false);
    let res = client
        .complete(
            ProviderRequest {
                system_prompt: "sys".into(),
                tools: Vec::new(),
                messages: Vec::new(),
                stream: false,
                cache_breakpoints: Vec::new(),
                max_output_tokens: None,
                retry_budget: None,
                wire_attempts: None,
            },
            &BudgetConfig::default(),
            rx,
        )
        .await;
    match res {
        Err(ProviderError::Transport(msg)) => {
            assert!(
                msg.contains("Transfer-Encoding"),
                "unexpected transport error: {}",
                msg
            );
        }
        other => panic!(
            "bogus Transfer-Encoding must be rejected, got {:?}",
            other.map(|_| ())
        ),
    }
}

/// B16: a *failed* sample must still report how many requests it actually transmitted. Before this,
/// only the logical turn was counted, so a call that retried three times and failed looked like it
/// had never reached the provider.
#[tokio::test]
async fn failed_provider_call_reports_its_real_wire_attempts() {
    use kroma_core::provider::WireAttempts;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let hits = Arc::new(AtomicUsize::new(0));
    let server_hits = Arc::clone(&hits);
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            server_hits.fetch_add(1, Ordering::SeqCst);
            let mut request = Vec::new();
            loop {
                let mut buf = [0; 4096];
                let Ok(read) = socket.read(&mut buf).await else { return };
                if read == 0 {
                    return;
                }
                request.extend_from_slice(&buf[..read]);
                if let Some(pos) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                    let header = String::from_utf8_lossy(&request[..pos]);
                    let len: usize = header
                        .lines()
                        .find_map(|line| {
                            let (key, value) = line.split_once(':')?;
                            key.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse().unwrap())
                        })
                        .unwrap_or(0);
                    if request.len() >= pos + 4 + len {
                        break;
                    }
                }
            }
            let _ = socket
                .write_all(b"HTTP/1.1 503 Service Unavailable\r\nRetry-After: 0\r\nContent-Length: 5\r\nConnection: close\r\n\r\nretry")
                .await;
        }
    });

    let client = ChatCompletionsClient::new(ChatCompletionsConfig {
        base_url: format!("http://{}", addr),
        api_key: "fixture".into(),
        model: "fixture-model".into(),
        max_retries: 2,
        reasoning_effort: None,
    })
    .unwrap();
    let (_tx, rx) = watch::channel(false);
    let attempts = WireAttempts::new();
    let result = client
        .complete(
            ProviderRequest {
                stream: false,
                wire_attempts: Some(attempts.clone()),
                ..ProviderRequest::default()
            },
            &BudgetConfig::default(),
            rx,
        )
        .await;
    assert!(result.is_err(), "an always-503 server must fail the call");
    assert_eq!(attempts.get(), 3, "max_retries=2 means three transmitted attempts");
    assert_eq!(hits.load(Ordering::SeqCst), 3, "the server side must agree");
}

/// The same guarantee at the run level: a failed run's summary must count the attempts that were
/// transmitted, not just the one logical turn it was working on.
#[tokio::test]
async fn failed_run_summary_counts_every_transmitted_attempt() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let hits = Arc::new(AtomicUsize::new(0));
    let server_hits = Arc::clone(&hits);
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            server_hits.fetch_add(1, Ordering::SeqCst);
            let mut request = Vec::new();
            loop {
                let mut buf = [0; 4096];
                let Ok(read) = socket.read(&mut buf).await else { return };
                if read == 0 {
                    return;
                }
                request.extend_from_slice(&buf[..read]);
                if let Some(pos) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                    let header = String::from_utf8_lossy(&request[..pos]);
                    let len: usize = header
                        .lines()
                        .find_map(|line| {
                            let (key, value) = line.split_once(':')?;
                            key.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse().unwrap())
                        })
                        .unwrap_or(0);
                    if request.len() >= pos + 4 + len {
                        break;
                    }
                }
            }
            let _ = socket
                .write_all(b"HTTP/1.1 503 Service Unavailable\r\nRetry-After: 0\r\nContent-Length: 5\r\nConnection: close\r\n\r\nretry")
                .await;
        }
    });

    let root = std::env::temp_dir().join(format!("b16-attempts-{}", Uuid::new_v4()));
    let workspace = root.join("ws");
    fs::create_dir_all(&workspace).unwrap();
    let budget = BudgetConfig::default();
    let env = Arc::new(LocalHostEnv::new(&workspace, root.join("state")).unwrap());
    let runtime = build_default_runtime(
        env,
        PolicyEngine::new(PolicyMode::ReadOnly, Arc::new(NonInteractiveApprovalHandler)),
        budget.clone(),
        None,
    );
    let cfg = AgentRunConfig::new("attempt accounting", budget);
    let logger = EventLogger::new(cfg.run_id, Some(root.join("state")), false).unwrap();
    let provider = Arc::new(
        ChatCompletionsClient::new(ChatCompletionsConfig {
            base_url: format!("http://{}", addr),
            api_key: "fixture".into(),
            model: "fixture-model".into(),
            max_retries: 2,
            reasoning_effort: None,
        })
        .unwrap(),
    );
    // No token ledger: an unbounded run must be allowed to retry, so the attempt count is what is
    // under test here rather than the retry budget admission.
    let (_tx, rx) = watch::channel(false);
    let (summary, _logger) = AgentRunner::new(cfg, provider, runtime, rx).run(logger).await;

    assert_eq!(summary.status, RunStatus::Failed);
    assert_eq!(
        summary.request_attempts_used, 3,
        "the summary must report the three transmitted attempts, not just the logical turn"
    );
    assert_eq!(hits.load(Ordering::SeqCst), 3);
    let _ = fs::remove_dir_all(root);
}

/// A third-party provider may report `attempts_used` without adopting the shared `WireAttempts`
/// counter. The contract is then: report it honestly, and the runner still accounts for it.
struct AttemptsReportingProvider {
    reported_attempts: usize,
}

impl ModelProvider for AttemptsReportingProvider {
    fn complete<'a>(
        &'a self,
        _req: ProviderRequest,
        _budget: &'a BudgetConfig,
        _cancel_rx: watch::Receiver<bool>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<ProviderCompletion, ProviderError>> + Send + 'a>,
    > {
        Box::pin(async move {
            Ok(ProviderCompletion {
                turn: kroma_core::messages::AssistantTurn {
                    content: Some("done".to_string()),
                    reasoning_blocks: Vec::new(),
                    tool_calls: Vec::new(),
                    finish_reason: kroma_core::messages::FinishReason::Stop,
                    usage: Some(kroma_core::messages::TokenUsage {
                        input_tokens: 10,
                        cached_input_tokens: None,
                        output_tokens: 2,
                        reasoning_tokens: None,
                    }),
                },
                request_body_bytes: 8,
                latency_ms: 1,
                ttft_ms: None,
                // Deliberately no WireAttempts counter: only the self-reported number is available.
                attempts_used: self.reported_attempts,
                reported_model: Some("third-party-model".to_string()),
            })
        })
    }
}

/// Without the shared counter the runner must trust (and record) the provider's own attempt count,
/// so a retrying third-party provider cannot look like it never retried.
#[tokio::test]
async fn a_provider_without_the_counter_still_reports_its_attempts() {
    let root = std::env::temp_dir().join(format!("b16-provider-attempts-{}", Uuid::new_v4()));
    let workspace = root.join("ws");
    fs::create_dir_all(&workspace).unwrap();
    let budget = BudgetConfig::default();
    let env = Arc::new(LocalHostEnv::new(&workspace, root.join("state")).unwrap());
    let runtime = build_default_runtime(
        env,
        PolicyEngine::new(PolicyMode::ReadOnly, Arc::new(NonInteractiveApprovalHandler)),
        budget.clone(),
        None,
    );
    let cfg = AgentRunConfig::new("third party attempts", budget);
    let logger = EventLogger::new(cfg.run_id, Some(root.join("state")), false).unwrap();
    let provider = Arc::new(AttemptsReportingProvider {
        reported_attempts: 3,
    });
    let (_tx, rx) = watch::channel(false);
    let (summary, _logger) = AgentRunner::new(cfg, provider, runtime, rx).run(logger).await;

    assert_eq!(
        summary.request_attempts_used, 3,
        "the self-reported attempt count must be honoured when no counter is provided"
    );
    let _ = fs::remove_dir_all(root);
}
