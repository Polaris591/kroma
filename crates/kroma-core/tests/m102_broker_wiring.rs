//! M1-02 (wiring half): the broker is the checkpoint between a provider request and the wire.
//!
//! `crates/kroma-core/src/credential_broker.rs` already decides *whether* a request may be
//! dispatched. These tests cover the other half: a provider wrapped by the broker cannot dispatch
//! without a passing grant, and every dispatch it does perform leaves one entry in the broker's
//! own ledger — reserved before the call, settled from provider-reported usage afterwards, and
//! charged as unknown liability when the usage cannot be proven.
//!
//! Every "nothing was sent" assertion here is an observation of the transport seam: the inner
//! provider's `complete` is never entered, and the wire-attempt counter the protocol clients
//! increment per transmitted HTTP attempt stays at zero. The real protocol client is used as the
//! innermost layer in the deny tests, so a dispatch would have to open a socket to
//! `api.example.com` — an unresolvable, fake authority that no test may reach.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use kroma_core::budget::BudgetConfig;
use kroma_core::credential_broker::{
    BrokerGate, BrokerGrant, BrokerIdentity, BrokerState, BrokeredProvider, Deny,
};
use kroma_core::messages::{AssistantTurn, FinishReason, TokenUsage};
use kroma_core::provider::chat_completions::{ChatCompletionsClient, ChatCompletionsConfig};
use kroma_core::provider::{
    DispatchBinding, ModelProvider, ProviderCompletion, ProviderError, ProviderRequest,
    WireAttempts,
};
use tokio::sync::watch;

const ENDPOINT: &str = "https://api.example.com:443";
const MODEL: &str = "gemini-3.8-flash";
const JOB: &str = "run-m102";

fn grant() -> BrokerGrant {
    BrokerGrant {
        job_id: JOB.to_string(),
        subject: "worker".to_string(),
        endpoints: [ENDPOINT.to_string()].into_iter().collect(),
        models: [MODEL.to_string()].into_iter().collect(),
        token_limit: 100_000,
        cost_limit_micros: 1_000_000,
        epoch: 3,
    }
}

fn identity() -> BrokerIdentity {
    BrokerIdentity {
        job_id: JOB.to_string(),
        subject: "worker".to_string(),
        epoch: 3,
    }
}

fn gate(grant: BrokerGrant) -> (Arc<BrokerGate>, Arc<Mutex<BrokerState>>) {
    let state = Arc::new(Mutex::new(BrokerState::default()));
    let gate = Arc::new(BrokerGate::new(grant, identity(), Arc::clone(&state)));
    (gate, state)
}

fn usage_turn(input: u64, output: u64) -> AssistantTurn {
    AssistantTurn {
        content: Some("done".to_string()),
        reasoning_blocks: Vec::new(),
        tool_calls: Vec::new(),
        finish_reason: FinishReason::Stop,
        usage: Some(TokenUsage {
            input_tokens: input,
            cached_input_tokens: None,
            cache_creation_input_tokens: None,
            output_tokens: output,
            reasoning_tokens: None,
        }),
    }
}

fn completion(turn: AssistantTurn) -> ProviderCompletion {
    ProviderCompletion {
        turn,
        request_body_bytes: 1,
        latency_ms: 1,
        ttft_ms: Some(1),
        attempts_used: 1,
        reported_model: None,
    }
}

/// Counts entries into the transport layer and delegates. Sitting between the broker and the real
/// protocol client is what makes "zero calls" observable without a network.
struct CountingTransport {
    inner: Arc<dyn ModelProvider>,
    calls: Arc<AtomicUsize>,
}

impl ModelProvider for CountingTransport {
    fn dispatch_binding(&self) -> Option<DispatchBinding> {
        self.inner.dispatch_binding()
    }

    fn complete<'a>(
        &'a self,
        req: ProviderRequest,
        budget: &'a BudgetConfig,
        cancel_rx: watch::Receiver<bool>,
    ) -> Pin<Box<dyn Future<Output = Result<ProviderCompletion, ProviderError>> + Send + 'a>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.complete(req, budget, cancel_rx)
    }
}

/// A provider that answers from a canned result and never touches a socket, with a binding the
/// test states explicitly — that is how a one-character difference becomes a test input.
struct ScriptedProvider {
    binding: Option<DispatchBinding>,
    result: Result<ProviderCompletion, ProviderError>,
}

impl ModelProvider for ScriptedProvider {
    fn dispatch_binding(&self) -> Option<DispatchBinding> {
        self.binding.clone()
    }

    fn complete<'a>(
        &'a self,
        _req: ProviderRequest,
        _budget: &'a BudgetConfig,
        _cancel_rx: watch::Receiver<bool>,
    ) -> Pin<Box<dyn Future<Output = Result<ProviderCompletion, ProviderError>> + Send + 'a>> {
        let result = self.result.clone();
        Box::pin(async move { result })
    }
}

fn real_client(model: &str) -> Arc<dyn ModelProvider> {
    Arc::new(
        ChatCompletionsClient::new(ChatCompletionsConfig {
            base_url: ENDPOINT.to_string(),
            api_key: "test-key-not-a-credential".to_string(),
            model: model.to_string(),
            max_retries: 0,
            reasoning_effort: None,
        })
        .expect("the test endpoint is a valid https base URL"),
    )
}

async fn dispatch(provider: &BrokeredProvider) -> (Result<ProviderCompletion, ProviderError>, u64) {
    let wire = WireAttempts::new();
    let req = ProviderRequest {
        system_prompt: "you are a test".to_string(),
        tools: Vec::new(),
        messages: Vec::new(),
        stream: false,
        cache_breakpoints: Vec::new(),
        max_output_tokens: Some(64),
        retry_budget: None,
        wire_attempts: Some(wire.clone()),
    };
    let (_tx, rx) = watch::channel(false);
    let result = provider.complete(req, &BudgetConfig::default(), rx).await;
    (result, wire.get() as u64)
}

fn assert_broker_denial(result: &Result<ProviderCompletion, ProviderError>) -> String {
    match result {
        Err(ProviderError::NotDispatched(inner)) => {
            assert!(
                matches!(**inner, ProviderError::BrokerDenied(_)),
                "expected a broker denial, got {inner:?}"
            );
            inner.to_string()
        }
        other => panic!("expected a pre-dispatch broker denial, got {other:?}"),
    }
}

/// M1-02-B1: a grant that names a different model by one character refuses the dispatch, and the
/// transport is never entered. The binding comes from the real protocol client, not from the test's
/// idea of it, so this also pins the model id that will actually be sent.
#[tokio::test]
async fn a_model_that_differs_by_one_character_is_never_dispatched() {
    let mut g = grant();
    g.models = ["gemini-3.8-flas".to_string()].into_iter().collect();
    let (gate, state) = gate(g);

    let calls = Arc::new(AtomicUsize::new(0));
    let transport = Arc::new(CountingTransport {
        inner: real_client(MODEL),
        calls: Arc::clone(&calls),
    });
    let provider = BrokeredProvider::new(transport, gate);

    let (result, wire_attempts) = dispatch(&provider).await;

    let denial = assert_broker_denial(&result);
    assert!(
        denial.contains("model"),
        "the denial must name the binding that failed: {denial}"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "a refused request must not reach the transport"
    );
    assert_eq!(wire_attempts, 0, "no HTTP attempt may be transmitted");
    let state = state.lock().unwrap();
    assert!(
        state.reservations.is_empty(),
        "a refused dispatch reserves nothing"
    );
    assert_eq!(state.unknown_liability_tokens, 0);
    assert_eq!(state.settled_tokens, 0);
}

/// M1-02-B2: the endpoint is bound to the character as well. One character of the host is enough
/// to make the destination a different endpoint.
#[tokio::test]
async fn an_endpoint_that_differs_by_one_character_is_never_dispatched() {
    let mut g = grant();
    g.endpoints = ["https://api.example.con:443".to_string()]
        .into_iter()
        .collect();
    let (gate, state) = gate(g);

    let calls = Arc::new(AtomicUsize::new(0));
    let transport = Arc::new(CountingTransport {
        inner: real_client(MODEL),
        calls: Arc::clone(&calls),
    });
    let provider = BrokeredProvider::new(transport, gate);

    let (result, wire_attempts) = dispatch(&provider).await;

    let denial = assert_broker_denial(&result);
    assert!(
        denial.contains("endpoint"),
        "the denial must name the binding that failed: {denial}"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(wire_attempts, 0);
    let state = state.lock().unwrap();
    assert!(state.reservations.is_empty());
    assert_eq!(state.unknown_liability_tokens, 0);
}

/// M1-02-B3: a provider that cannot state its endpoint and model cannot be authorized. "Unknown"
/// is refused, not treated as "unconstrained".
#[tokio::test]
async fn a_provider_without_a_dispatch_binding_is_refused() {
    let (gate, state) = gate(grant());
    let calls = Arc::new(AtomicUsize::new(0));
    let transport = Arc::new(CountingTransport {
        inner: Arc::new(ScriptedProvider {
            binding: None,
            result: Ok(completion(usage_turn(1, 1))),
        }),
        calls: Arc::clone(&calls),
    });
    let provider = BrokeredProvider::new(transport, gate);

    let (result, wire_attempts) = dispatch(&provider).await;

    let denial = assert_broker_denial(&result);
    assert!(denial.contains("binding"), "unexpected denial: {denial}");
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(wire_attempts, 0);
    assert!(state.lock().unwrap().reservations.is_empty());
}

/// M1-02-B4: an authorized dispatch reserves before the call and settles with the usage the
/// provider reported — the reservation is released, the spend is recorded, and nothing is left as
/// liability.
#[tokio::test]
async fn a_granted_dispatch_reserves_before_and_settles_reported_usage() {
    let (gate, state) = gate(grant());
    let calls = Arc::new(AtomicUsize::new(0));
    let transport = Arc::new(CountingTransport {
        inner: Arc::new(ScriptedProvider {
            binding: Some(DispatchBinding {
                endpoint: ENDPOINT.to_string(),
                model: MODEL.to_string(),
            }),
            result: Ok(completion(usage_turn(120, 30))),
        }),
        calls: Arc::clone(&calls),
    });
    let provider = BrokeredProvider::new(transport, gate);

    let (result, _wire_attempts) = dispatch(&provider).await;

    assert!(
        result.is_ok(),
        "the granted dispatch must succeed: {result:?}"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let state = state.lock().unwrap();
    assert!(
        state.reservations.is_empty(),
        "a settled dispatch holds no reservation"
    );
    assert_eq!(
        state.settled_tokens, 150,
        "the ledger records the provider-reported input + output tokens"
    );
    assert_eq!(state.unknown_liability_tokens, 0);
}

/// M1-02-B5: a completion without reported usage cannot prove what was billed, so the reservation
/// becomes unknown liability instead of a refund.
#[tokio::test]
async fn a_completion_without_reported_usage_becomes_unknown_liability() {
    let (gate, state) = gate(grant());
    let mut turn = usage_turn(0, 0);
    turn.usage = None;
    let provider = BrokeredProvider::new(
        Arc::new(ScriptedProvider {
            binding: Some(DispatchBinding {
                endpoint: ENDPOINT.to_string(),
                model: MODEL.to_string(),
            }),
            result: Ok(completion(turn)),
        }),
        gate,
    );

    let (result, _) = dispatch(&provider).await;
    assert!(result.is_ok());

    let state = state.lock().unwrap();
    assert!(state.reservations.is_empty());
    assert_eq!(state.settled_tokens, 0);
    assert!(
        state.unknown_liability_tokens > 0,
        "an unknown settlement keeps the worst case as liability, never zero"
    );
}

/// M1-02-B6: a dispatch that failed after transmission is not refunded: the provider may have
/// billed it.
#[tokio::test]
async fn a_failed_dispatch_is_not_refunded() {
    let (gate, state) = gate(grant());
    let provider = BrokeredProvider::new(
        Arc::new(ScriptedProvider {
            binding: Some(DispatchBinding {
                endpoint: ENDPOINT.to_string(),
                model: MODEL.to_string(),
            }),
            result: Err(ProviderError::HttpStatus {
                status: 500,
                body: "upstream error".to_string(),
            }),
        }),
        gate,
    );

    let (result, _) = dispatch(&provider).await;
    assert!(result.is_err());

    let state = state.lock().unwrap();
    assert!(state.reservations.is_empty());
    assert_eq!(state.settled_tokens, 0);
    assert!(state.unknown_liability_tokens > 0);
}

/// M1-02-B7: only a request that provably never left the process releases its reservation. The
/// existing zero-consumption classification decides that, so the broker does not invent a second
/// rule.
#[tokio::test]
async fn a_provably_undispatched_failure_releases_the_reservation() {
    let (gate, state) = gate(grant());
    let provider = BrokeredProvider::new(
        Arc::new(ScriptedProvider {
            binding: Some(DispatchBinding {
                endpoint: ENDPOINT.to_string(),
                model: MODEL.to_string(),
            }),
            result: Err(ProviderError::NotDispatched(Box::new(
                ProviderError::InsecureUrl("rejected before any socket".to_string()),
            ))),
        }),
        gate,
    );

    let (result, wire_attempts) = dispatch(&provider).await;
    assert!(result.is_err());
    assert_eq!(wire_attempts, 0);

    let state = state.lock().unwrap();
    assert!(
        state.reservations.is_empty(),
        "a request that never left the process holds no reservation"
    );
    assert_eq!(state.settled_tokens, 0);
    assert_eq!(
        state.unknown_liability_tokens, 0,
        "zero consumption is not liability"
    );
}

/// M1-02-B8: the envelope depletes across dispatches. One settled dispatch consumes allowance, so
/// the next request that no longer fits is refused before dispatch.
#[tokio::test]
async fn a_settled_dispatch_consumes_the_envelope_for_the_next_one() {
    let binding = Some(DispatchBinding {
        endpoint: ENDPOINT.to_string(),
        model: MODEL.to_string(),
    });

    // Learn what one dispatch claims from the ledger itself instead of restating the estimator: a
    // completion without reported usage moves its whole reservation into liability.
    let (probe_gate, probe_state) = gate(grant());
    let probe = BrokeredProvider::new(
        Arc::new(ScriptedProvider {
            binding: binding.clone(),
            result: Ok(completion({
                let mut turn = usage_turn(0, 0);
                turn.usage = None;
                turn
            })),
        }),
        probe_gate,
    );
    assert!(dispatch(&probe).await.0.is_ok());
    let claim = probe_state.lock().unwrap().unknown_liability_tokens;
    assert!(claim > 0, "a dispatch must claim something");

    // A grant that covers exactly one such dispatch, one token short of covering a second one after
    // the first has really spent 100.
    let mut tight = grant();
    tight.token_limit = claim.saturating_add(99);
    let (gate, state) = gate(tight);
    let calls = Arc::new(AtomicUsize::new(0));
    let transport = Arc::new(CountingTransport {
        inner: Arc::new(ScriptedProvider {
            binding: binding.clone(),
            result: Ok(completion(usage_turn(90, 10))),
        }),
        calls: Arc::clone(&calls),
    });
    let provider = BrokeredProvider::new(transport, gate);

    let (first, _) = dispatch(&provider).await;
    assert!(first.is_ok(), "the first dispatch fits the envelope");
    assert_eq!(state.lock().unwrap().settled_tokens, 100);

    let (second, wire_attempts) = dispatch(&provider).await;
    let denial = assert_broker_denial(&second);
    assert!(denial.contains("budget"), "unexpected denial: {denial}");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "only the first dispatch reached the transport"
    );
    assert_eq!(wire_attempts, 0);
    assert!(state.lock().unwrap().reservations.is_empty());
}

/// M1-02-B9: the refusal is reported as a pre-dispatch error, so the caller's own ledger refunds
/// the allowance it reserved for this attempt. `is_provably_zero_consumption` is the existing
/// contract for exactly that.
#[tokio::test]
async fn a_broker_denial_claims_zero_consumption() {
    let mut g = grant();
    g.models = ["something-else".to_string()].into_iter().collect();
    let (gate, _state) = gate(g);
    let provider = BrokeredProvider::new(real_client(MODEL), gate);

    let (result, _) = dispatch(&provider).await;
    let error = result.expect_err("a denied request must not succeed");
    assert!(
        error.is_provably_zero_consumption(),
        "a denial happens before any transmission: {error:?}"
    );
    assert!(matches!(
        error,
        ProviderError::NotDispatched(ref inner) if matches!(**inner, ProviderError::BrokerDenied(_))
    ));
    // The wire-level denial reason is the broker's, not a socket error: nothing was connected.
    assert!(Deny::ModelNotAllowed.to_string().contains("model"));
}
