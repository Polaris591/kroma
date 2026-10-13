use harness_core::agent_loop::{build_default_runtime, AgentRunConfig, AgentRunner};
use harness_core::budget::BudgetConfig;
use harness_core::env::LocalHostEnv;
use harness_core::events::EventLogger;
use harness_core::messages::FinishReason;
use harness_core::provider::chat_completions::{
    validate_base_url, ChatCompletionsClient, ChatCompletionsConfig,
};
use harness_core::provider::{ModelProvider, ProviderError, ProviderRequest};
use harness_core::tool::policy::{NonInteractiveApprovalHandler, PolicyEngine, PolicyMode};
use harness_core::types::{RunStatus, TerminalReason};
use std::fs;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::watch;
use uuid::Uuid;

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
            matches!(validate_base_url(hostile), Err(ProviderError::InsecureUrl(_))),
            "hostile URL must be rejected: {:?}",
            hostile
        );
    }

    // A legitimate URL with a nested base path still passes.
    assert!(validate_base_url("https://api.openai.com/v1/chat").is_ok());
}

#[tokio::test]
async fn test_chunked_sse_streaming_and_pre_chunk_disconnect_retry_and_oom_guard() {
    use harness_core::messages::{AssistantTurn, ReasoningBlock, TranscriptItem};
    use harness_core::provider::chat_completions::{
        serialize_openai_request_body, serialize_openai_request_body_with_effort,
    };
    use harness_core::provider::sse::parse_usage_json;

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

    let serialized_effort = serialize_openai_request_body_with_effort(
        "deepseek",
        "sys",
        &[],
        &[],
        true,
        Some("high"),
    );
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
    let dbg_str = format!("{:?}", ChatCompletionsConfig {
        base_url: "https://api.openai.com".into(),
        api_key: "sk-live-super-secret".into(),
        model: "gpt".into(),
        max_retries: 2,
        reasoning_effort: Some("high".into()),
    });
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
    assert_eq!(
        completion.turn.usage.unwrap().cached_input_tokens,
        Some(64)
    );

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
            },
            &BudgetConfig::default(),
            rx2,
        )
        .await;
    assert_eq!(redir_res.unwrap_err(), ProviderError::RedirectBlocked(302));
}

#[tokio::test]
async fn test_agent_loop_zero_side_effects_on_length_truncation_over_sse() {
    let tmp_ws = std::env::temp_dir().join(format!("harness-ws-len-{}", Uuid::new_v4()));
    let tmp_state = std::env::temp_dir().join(format!("harness-state-len-{}", Uuid::new_v4()));
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

    let (summary, _logger) = AgentRunner::new(cfg, provider, runtime, rx).run(logger).await;
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
            },
            &BudgetConfig::default(),
            rx,
        )
        .await;
    match smug_res {
        Err(ProviderError::Transport(msg)) => {
            assert!(msg.contains("Transfer-Encoding") && msg.contains("Content-Length"));
        }
        other => panic!("expected Transport error for TE+CL smuggling response, got {:?}", other),
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
        other => panic!("conflicting Content-Length must be rejected, got {:?}", other.map(|_| ())),
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
        other => panic!("premature EOF must be rejected, got {:?}", other.map(|_| ())),
    }
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
        other => panic!("bogus Transfer-Encoding must be rejected, got {:?}", other.map(|_| ())),
    }
}
