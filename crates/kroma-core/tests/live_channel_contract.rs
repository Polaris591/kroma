//! Live provider admission contracts. Loopback HTTP only: no model calls or credentials.
use kroma_core::budget::{BudgetConfig, TokenBudget};
use kroma_core::provider::{
    build_provider, ModelProvider, ProviderError, ProviderProtocol, ProviderRequest, ProviderSettings,
    RetryBudget, WireAttempts,
};
use kroma_core::types::TerminalReason;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, watch, Semaphore};
use tokio::task::{JoinHandle, JoinSet};

struct Fixture {
    url: String,
    hits: Arc<AtomicUsize>,
    arrivals: mpsc::Receiver<()>,
    release: Arc<Semaphore>,
    server: JoinHandle<()>,
}

impl Fixture {
    async fn new(gated: bool, status: usize) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let hits = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&hits);
        let release = Arc::new(Semaphore::new(if gated { 0 } else { 32 }));
        let permits = Arc::clone(&release);
        let (tx, arrivals) = mpsc::channel(16);
        let server = tokio::spawn(async move {
            let mut workers = JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let (mut socket, _) = accepted.unwrap();
                        let permits = Arc::clone(&permits);
                        let tx = tx.clone();
                        let observed = Arc::clone(&observed);
                        workers.spawn(async move {
                            let mut request = Vec::new();
                            loop {
                                let mut buf = [0; 2048];
                                let n = socket.read(&mut buf).await.unwrap();
                                if n == 0 { return; }
                                request.extend_from_slice(&buf[..n]);
                                assert!(request.len() < 65536, "bounded fixture requests only");
                                if let Some(pos) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                                    let head = String::from_utf8_lossy(&request[..pos]);
                                    let length: usize = head.lines().find_map(|line| {
                                        let (key, value) = line.split_once(':')?;
                                        key.eq_ignore_ascii_case("content-length")
                                            .then(|| value.trim().parse().unwrap())
                                    }).unwrap_or(0);
                                    if request.len() >= pos + 4 + length { break; }
                                }
                            }
                            observed.fetch_add(1, Ordering::SeqCst);
                            tx.send(()).await.unwrap();
                            let streaming = status == 201;
                            if !streaming { permits.acquire().await.unwrap().forget(); }
                            let request = String::from_utf8(request).unwrap();
                            let body = if status != 200 {
                                "channel unavailable"
                            } else if request.starts_with("POST /v1/messages ") {
                                r#"{"id":"fixture","model":"fixture","type":"message","role":"assistant","content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn","usage":{"input_tokens":7,"output_tokens":3}}"#
                            } else if request.starts_with("POST /v1/responses ") {
                                r#"{"id":"fixture","model":"fixture","status":"completed","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"ok"}]}],"usage":{"input_tokens":7,"output_tokens":3}}"#
                            } else {
                                r#"{"model":"fixture","choices":[{"index":0,"finish_reason":"stop","message":{"role":"assistant","content":"ok"}}],"usage":{"prompt_tokens":7,"completion_tokens":3}}"#
                            };
                            if streaming {
                                let events = if request.starts_with("POST /v1/messages ") {
                                    vec![
                                        "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"model\":\"fixture\",\"usage\":{\"input_tokens\":7,\"output_tokens\":0}}}\n\n",
                                        "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":3}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
                                    ]
                                } else if request.starts_with("POST /v1/responses ") {
                                    vec![
                                        "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"ok\"}\n\n",
                                        "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"model\":\"fixture\",\"status\":\"completed\",\"usage\":{\"input_tokens\":7,\"output_tokens\":3}}}\n\n",
                                    ]
                                } else {
                                    vec![
                                        "data: {\"model\":\"fixture\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"},\"finish_reason\":null}]}\n\n",
                                        "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":3}}\n\ndata: [DONE]\n\n",
                                    ]
                                };
                                let header = format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", events.iter().map(|e| e.len()).sum::<usize>());
                                let _ = socket.write_all(header.as_bytes()).await;
                                let _ = socket.write_all(events[0].as_bytes()).await;
                                permits.acquire().await.unwrap().forget();
                                let _ = socket.write_all(events[1].as_bytes()).await;
                                return;
                            }
                            let retry_after = if status == 429 { 120 } else { 0 };
                            let response = format!(
                                "HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nRetry-After: {retry_after}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len(),
                            );
                            let _ = socket.write_all(response.as_bytes()).await;
                        });
                    }
                    _ = workers.join_next(), if !workers.is_empty() => {}
                }
            }
        });
        Self { url, hits, arrivals, release, server }
    }

    fn client(&self, protocol: ProviderProtocol, retries: usize) -> Arc<dyn ModelProvider> {
        self.client_at(self.url.clone(), protocol, retries)
    }

    fn client_at(&self, base_url: String, protocol: ProviderProtocol, retries: usize) -> Arc<dyn ModelProvider> {
        build_provider(&ProviderSettings {
            base_url, api_key: "loopback-fixture".into(),
            model: format!("fixture-{protocol:?}"), max_retries: retries,
            reasoning_effort: None, protocol, max_tokens: 64,
        }).unwrap().1
    }

    async fn arrived(&mut self) {
        tokio::time::timeout(Duration::from_secs(2), self.arrivals.recv()).await
            .expect("a request must arrive promptly").expect("fixture stays alive");
    }

    async fn no_arrival(&mut self) {
        assert!(tokio::time::timeout(Duration::from_millis(80), self.arrivals.recv()).await.is_err(),
            "queued provider must not transmit while the channel is occupied");
    }
}

impl Drop for Fixture {
    fn drop(&mut self) { self.server.abort(); } // Dropping the JoinSet aborts fixture socket workers.
}

#[tokio::test]
async fn separately_built_protocol_clients_share_capacity_until_the_response_finishes() {
    for protocol in [ProviderProtocol::ChatCompletions, ProviderProtocol::AnthropicMessages,
                     ProviderProtocol::OpenAiResponses] {
        let mut fixture = Fixture::new(true, 200).await;
        let first = fixture.client(protocol, 0);
        let second = fixture.client(ProviderProtocol::ChatCompletions, 0);
        let (_cancel, rx) = watch::channel(false);
        let first_rx = rx.clone();
        let first = tokio::spawn(async move {
            first.complete(ProviderRequest::default(), &BudgetConfig::default(), first_rx).await
        });
        fixture.arrived().await;
        let second = tokio::spawn(async move {
            second.complete(ProviderRequest::default(), &BudgetConfig::default(), rx).await
        });
        fixture.no_arrival().await;
        assert_eq!(fixture.hits.load(Ordering::SeqCst), 1);
        fixture.release.add_permits(1);
        assert_eq!(first.await.unwrap().unwrap().attempts_used, 1);
        fixture.arrived().await;
        fixture.release.add_permits(1);
        assert_eq!(second.await.unwrap().unwrap().attempts_used, 1);
    }
}

#[tokio::test]
async fn streaming_admission_is_held_until_the_last_event_for_every_protocol() {
    for protocol in [ProviderProtocol::ChatCompletions, ProviderProtocol::AnthropicMessages,
                     ProviderProtocol::OpenAiResponses] {
        let mut fixture = Fixture::new(true, 201).await;
        let first = fixture.client(protocol, 0);
        let second = fixture.client(protocol, 0);
        let (_tx, rx) = watch::channel(false);
        let first_rx = rx.clone();
        let first = tokio::spawn(async move {
            first.complete(ProviderRequest { stream: true, ..Default::default() },
                &BudgetConfig::default(), first_rx).await
        });
        fixture.arrived().await;
        let second = tokio::spawn(async move {
            second.complete(ProviderRequest { stream: true, ..Default::default() },
                &BudgetConfig::default(), rx).await
        });
        fixture.no_arrival().await;
        fixture.release.add_permits(1);
        first.await.unwrap().unwrap();
        fixture.arrived().await;
        fixture.release.add_permits(1);
        second.await.unwrap().unwrap();
    }
}

#[tokio::test]
async fn explicit_protocol_paths_and_aliases_cannot_create_another_channel() {
    let mut fixture = Fixture::new(true, 200).await;
    let first = fixture.client(ProviderProtocol::ChatCompletions, 0);
    let second = fixture.client_at(format!("{}/v1/messages", fixture.url.replace("127.0.0.1", "localhost")),
        ProviderProtocol::AnthropicMessages, 0);
    let (_tx, rx) = watch::channel(false);
    let first_rx = rx.clone();
    let first = tokio::spawn(async move {
        first.complete(ProviderRequest::default(), &BudgetConfig::default(), first_rx).await
    });
    fixture.arrived().await;
    let second = tokio::spawn(async move {
        second.complete(ProviderRequest::default(), &BudgetConfig::default(), rx).await
    });
    fixture.no_arrival().await;
    fixture.release.add_permits(1);
    first.await.unwrap().unwrap();
    fixture.arrived().await;
    fixture.release.add_permits(1);
    second.await.unwrap().unwrap();
}

#[tokio::test]
async fn runner_queued_cancellation_refunds_the_unsent_reservation() {
    use kroma_core::agent_loop::{build_default_runtime, AgentRunConfig, AgentRunner};
    use kroma_core::env::LocalHostEnv;
    use kroma_core::events::EventLogger;
    use kroma_core::tool::policy::{NonInteractiveApprovalHandler, PolicyEngine, PolicyMode};
    let mut fixture = Fixture::new(true, 200).await;
    let first_client = fixture.client(ProviderProtocol::ChatCompletions, 0);
    let queued_client = fixture.client(ProviderProtocol::AnthropicMessages, 0);
    let (_tx, first_rx) = watch::channel(false);
    let first = tokio::spawn(async move {
        first_client.complete(ProviderRequest::default(), &BudgetConfig::default(), first_rx).await
    });
    fixture.arrived().await;
    let root = std::env::temp_dir().join(format!("b22-runner-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(root.join("ws")).unwrap();
    let budget = BudgetConfig::default();
    let env = Arc::new(LocalHostEnv::new(root.join("ws"), root.join("state")).unwrap());
    let runtime = build_default_runtime(env, PolicyEngine::new(PolicyMode::ReadOnly,
        Arc::new(NonInteractiveApprovalHandler)), budget.clone(), None);
    let cfg = AgentRunConfig::new("queued cancellation", budget);
    let logger = EventLogger::new(cfg.run_id, Some(root.join("state")), false).unwrap();
    let ledger = Arc::new(TokenBudget::new(Some(100_000)));
    let (cancel, rx) = watch::channel(false);
    let runner = AgentRunner::new(cfg, queued_client, runtime, rx).with_token_budget(Arc::clone(&ledger));
    let mut running = Box::pin(runner.run(logger));
    assert!(tokio::time::timeout(Duration::from_millis(40), running.as_mut()).await.is_err());
    assert!(ledger.accounting_snapshot().reserved > 0, "runner reached provider admission");
    cancel.send(true).unwrap();
    let (summary, _) = tokio::time::timeout(Duration::from_secs(2), running).await.unwrap();
    assert_eq!(summary.terminal_reason, TerminalReason::UserCancelled);
    assert_eq!(summary.request_attempts_used, 0, "an unsent reservation is not a wire attempt");
    assert_eq!(ledger.accounting_snapshot().spent, 0);
    assert_eq!(ledger.accounting_snapshot().reserved, 0);
    assert_eq!(fixture.hits.load(Ordering::SeqCst), 1);
    fixture.release.add_permits(1);
    first.await.unwrap().unwrap();
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn cancellation_while_queued_sends_nothing_and_reports_zero_consumption() {
    let mut fixture = Fixture::new(true, 200).await;
    let first_client = fixture.client(ProviderProtocol::ChatCompletions, 0);
    let queued_client = fixture.client(ProviderProtocol::AnthropicMessages, 0);
    let (_first_cancel, first_rx) = watch::channel(false);
    let first = tokio::spawn(async move {
        first_client.complete(ProviderRequest::default(), &BudgetConfig::default(), first_rx).await
    });
    fixture.arrived().await;
    let (cancel, rx) = watch::channel(false);
    let wire = WireAttempts::new();
    let request = ProviderRequest { wire_attempts: Some(wire.clone()), ..Default::default() };
    let budget = BudgetConfig::default();
    let mut queued = Box::pin(queued_client.complete(request, &budget, rx));
    assert!(tokio::time::timeout(Duration::from_millis(30), queued.as_mut()).await.is_err());
    cancel.send(true).unwrap();
    let error = tokio::time::timeout(Duration::from_secs(1), queued).await.unwrap().unwrap_err();
    assert!(matches!(error.clone().into_terminal_reason(), TerminalReason::UserCancelled));
    assert!(error.is_provably_zero_consumption(), "a queued cancellation is not unknown usage: {error}");
    assert_eq!(wire.get(), 0);
    assert_eq!(fixture.hits.load(Ordering::SeqCst), 1);
    fixture.release.add_permits(1);
    first.await.unwrap().unwrap();
    let (_tx, rx) = watch::channel(false);
    fixture.release.add_permits(1);
    queued_client.complete(ProviderRequest::default(), &BudgetConfig::default(), rx).await.unwrap();
}

#[tokio::test]
async fn a_queued_request_deadline_expires_before_transmission() {
    let mut fixture = Fixture::new(true, 200).await;
    let first_client = fixture.client(ProviderProtocol::ChatCompletions, 0);
    let queued_client = fixture.client(ProviderProtocol::OpenAiResponses, 0);
    let (_tx, rx) = watch::channel(false);
    let first_rx = rx.clone();
    let first = tokio::spawn(async move {
        first_client.complete(ProviderRequest::default(), &BudgetConfig::default(), first_rx).await
    });
    fixture.arrived().await;
    let wire = WireAttempts::new();
    let budget = BudgetConfig { request_timeout: Duration::from_millis(60), ..Default::default() };
    let error = tokio::time::timeout(Duration::from_secs(1), queued_client.complete(
        ProviderRequest { wire_attempts: Some(wire.clone()), ..Default::default() }, &budget, rx,
    )).await.unwrap().unwrap_err();
    assert!(error.is_provably_zero_consumption(), "admission timed out before dispatch: {error}");
    assert_eq!(wire.get(), 0);
    assert_eq!(fixture.hits.load(Ordering::SeqCst), 1);
    fixture.release.add_permits(1);
    first.await.unwrap().unwrap();
}

#[tokio::test]
async fn repeated_channel_failures_open_a_breaker_shared_across_protocols() {
    let fixture = Fixture::new(false, 503).await;
    let clients = [fixture.client(ProviderProtocol::ChatCompletions, 0),
                   fixture.client(ProviderProtocol::AnthropicMessages, 0),
                   fixture.client(ProviderProtocol::OpenAiResponses, 0)];
    let (_tx, rx) = watch::channel(false);
    for client in &clients {
        let error = client.complete(ProviderRequest::default(), &BudgetConfig::default(), rx.clone()).await.unwrap_err();
        assert!(matches!(error, ProviderError::HttpStatus { status: 503, .. }));
    }
    let wire = WireAttempts::new();
    let error = clients[0].complete(ProviderRequest {
        wire_attempts: Some(wire.clone()), ..Default::default()
    }, &BudgetConfig::default(), rx).await.unwrap_err();
    assert!(error.is_provably_zero_consumption(), "the open breaker must refuse before dispatch: {error}");
    assert_eq!(wire.get(), 0);
    assert_eq!(fixture.hits.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn a_long_retry_after_is_refused_instead_of_retrying_before_the_server_allows() {
    let fixture = Fixture::new(false, 429).await;
    let client = fixture.client(ProviderProtocol::ChatCompletions, 2);
    let (_tx, rx) = watch::channel(false);
    let wire = WireAttempts::new();
    let error = tokio::time::timeout(Duration::from_secs(1), client.complete(ProviderRequest {
        wire_attempts: Some(wire.clone()), ..Default::default()
    }, &BudgetConfig::default(), rx)).await.expect("do not sleep for an unbounded server delay").unwrap_err();
    assert!(matches!(error, ProviderError::ChannelUnavailable { retry_after_ms: 120_000 }));
    assert!(!error.is_provably_zero_consumption(), "the rate-limited attempt was already transmitted");
    assert_eq!(wire.get(), 1);
    assert_eq!(fixture.hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_breaker_refusal_after_a_sent_attempt_does_not_refund_its_unknown_spend() {
    let fixture = Fixture::new(false, 503).await;
    let single = fixture.client(ProviderProtocol::ChatCompletions, 0);
    let retrying = fixture.client(ProviderProtocol::OpenAiResponses, 2);
    let (_tx, rx) = watch::channel(false);
    for _ in 0..2 {
        single.complete(ProviderRequest::default(), &BudgetConfig::default(), rx.clone()).await.unwrap_err();
    }
    let ledger = Arc::new(TokenBudget::new(Some(200)));
    let first_reservation = ledger.try_reserve(100).unwrap();
    let wire = WireAttempts::new();
    let error = retrying.complete(ProviderRequest {
        retry_budget: Some(RetryBudget { ledger: Arc::clone(&ledger), per_attempt: 100 }),
        wire_attempts: Some(wire.clone()), ..Default::default()
    }, &BudgetConfig::default(), rx).await.unwrap_err();
    assert!(!error.is_provably_zero_consumption(), "one attempt was already sent");
    assert_eq!(wire.get(), 1, "the breaker must prevent the retry, not merely observe it");
    assert_eq!(fixture.hits.load(Ordering::SeqCst), 3);
    drop(first_reservation);
    assert_eq!(ledger.accounting_snapshot().unknown_tokens, 100);
}
