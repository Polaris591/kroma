//! M1-06: the pilot's three preconditions, measured instead of asserted.
//!
//! `m106_pilot_gate.rs` pins the refusal logic with hardcoded false preconditions. This file asks the
//! product the same three questions and lets the answers decide, so "the pilot may run" is a
//! measurement rather than a claim:
//!
//! * does the run path actually wrap provider dispatches in the broker, and does it wrap only those
//!   providers that state their destination?
//! * does an admitted dispatch settle into the broker's ledger instead of vanishing?
//! * can a supervised child read the API key out of its environment?

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use kroma_core::budget::BudgetConfig;
use kroma_core::credential_broker::{
    enforce, owner_envelope, provider_declares_destination, BrokerGate, BrokerGrant,
    BrokerIdentity, BrokerState, BrokeredProvider, Deny,
};
use kroma_core::messages::{AssistantTurn, FinishReason, TokenUsage};
use kroma_core::provider::{
    DispatchBinding, ModelProvider, ProviderCompletion, ProviderError, ProviderRequest,
};
use kroma_core::tools::bash::{collect_sanitized_env, is_blacklisted_env_var};
use tokio::sync::watch;

const ENDPOINT: &str = "https://api.example.com:443";
const MODEL: &str = "gemini-3.8-flash";

fn completion(tokens: u64) -> ProviderCompletion {
    let turn = AssistantTurn {
        content: Some("done".to_string()),
        reasoning_blocks: Vec::new(),
        tool_calls: Vec::new(),
        finish_reason: FinishReason::Stop,
        usage: Some(TokenUsage {
            input_tokens: tokens,
            cached_input_tokens: None,
            cache_creation_input_tokens: None,
            output_tokens: 0,
            reasoning_tokens: None,
        }),
    };
    ProviderCompletion {
        turn,
        request_body_bytes: 1,
        latency_ms: 1,
        ttft_ms: Some(1),
        attempts_used: 1,
        reported_model: None,
    }
}

/// A provider with no socket and a binding the test states explicitly.
struct Scripted {
    binding: Option<DispatchBinding>,
    calls: Arc<AtomicUsize>,
    tokens: u64,
}

impl ModelProvider for Scripted {
    fn dispatch_binding(&self) -> Option<DispatchBinding> {
        self.binding.clone()
    }

    fn complete<'a>(
        &'a self,
        _req: ProviderRequest,
        _budget: &'a BudgetConfig,
        _cancel_rx: watch::Receiver<bool>,
    ) -> Pin<Box<dyn Future<Output = Result<ProviderCompletion, ProviderError>> + Send + 'a>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let tokens = self.tokens;
        Box::pin(async move { Ok(completion(tokens)) })
    }
}

fn scripted(
    binding: Option<DispatchBinding>,
    tokens: u64,
) -> (Arc<dyn ModelProvider>, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let provider = Arc::new(Scripted {
        binding,
        calls: Arc::clone(&calls),
        tokens,
    });
    (provider, calls)
}

fn request() -> ProviderRequest {
    ProviderRequest {
        system_prompt: "you are a test".to_string(),
        tools: Vec::new(),
        messages: Vec::new(),
        stream: false,
        cache_breakpoints: Vec::new(),
        max_output_tokens: Some(64),
        retry_budget: None,
        wire_attempts: Some(kroma_core::provider::WireAttempts::new()),
    }
}

/// M1-06-A1: the run path wraps a provider that states its destination, and does not pretend to gate
/// one that says nothing about where it sends.
#[test]
fn the_run_path_wraps_exactly_the_providers_it_can_gate() {
    let binding = DispatchBinding {
        endpoint: ENDPOINT.to_string(),
        model: MODEL.to_string(),
    };
    let (declaring, _) = scripted(Some(binding), 10);
    assert!(provider_declares_destination(&declaring));
    let wrapped = enforce(Arc::clone(&declaring), "kroma-run", u64::MAX);
    assert!(
        !Arc::ptr_eq(&wrapped, &declaring),
        "a provider that states its destination must be gated by the broker"
    );

    let (silent, _) = scripted(None, 10);
    assert!(!provider_declares_destination(&silent));
    let untouched = enforce(Arc::clone(&silent), "kroma-run", u64::MAX);
    assert!(
        Arc::ptr_eq(&untouched, &silent),
        "the broker cannot vouch for a destination it cannot see, and must not pretend to"
    );
}

/// M1-06-A2: an admitted dispatch leaves one settlement behind. The ledger is the same object that
/// gated the request, so a paid call cannot happen outside the accounting.
#[tokio::test]
async fn an_admitted_dispatch_settles_into_the_broker_ledger() {
    let state = Arc::new(Mutex::new(BrokerState::default()));
    let grant = BrokerGrant {
        job_id: "kroma-run".to_string(),
        subject: "kroma-run".to_string(),
        endpoints: [ENDPOINT.to_string()].into_iter().collect(),
        models: [MODEL.to_string()].into_iter().collect(),
        token_limit: 100_000,
        cost_limit_micros: 0,
        epoch: 0,
    };
    let identity = BrokerIdentity {
        job_id: grant.job_id.clone(),
        subject: grant.subject.clone(),
        epoch: grant.epoch,
    };
    let gate = Arc::new(BrokerGate::new(grant, identity, Arc::clone(&state)));

    let binding = DispatchBinding {
        endpoint: ENDPOINT.to_string(),
        model: MODEL.to_string(),
    };
    let (inner, calls) = scripted(Some(binding), 120);
    let provider = BrokeredProvider::new(inner, gate);

    let (_, rx) = watch::channel(false);
    let budget = BudgetConfig::default();
    let result = provider.complete(request(), &budget, rx).await;
    assert!(result.is_ok(), "the granted destination must be admitted");
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    let state = state.lock().expect("state");
    assert_eq!(state.reserved_tokens(), 0, "the reservation was settled");
    assert_eq!(
        state.settled_tokens, 120,
        "the provider's reported usage reached the ledger that gated the call"
    );
}

/// M1-06-A3: a supervised child cannot read a key out of its environment, whatever the key is called.
#[test]
fn the_api_key_never_reaches_a_supervised_child() {
    let names = [
        "KROMA_API_KEY",
        "OPENAI_API_KEY",
        "SOMEVENDOR_API_KEY",
        "MY_SECRET_KEY",
    ];
    for name in names {
        assert!(
            is_blacklisted_env_var(name),
            "{name} must be scrubbed before a child is launched"
        );
    }
    // The scrub is by name, so an unset variable is enough to prove the collector drops the shape.
    std::env::set_var("SOMEVENDOR_API_KEY", "not-a-real-key");
    let visible: Vec<String> = collect_sanitized_env()
        .into_iter()
        .map(|(key, _)| key.to_string_lossy().to_string())
        .collect();
    std::env::remove_var("SOMEVENDOR_API_KEY");
    assert!(
        !visible.iter().any(|key| key == "SOMEVENDOR_API_KEY"),
        "the collector must not hand this name to a child"
    );
}

/// M1-06-A4: a paid run does not start without a declared spend envelope. There is no default and
/// `Some(0)` is not a small envelope - it is a run that could never make one request.
#[test]
fn a_paid_run_without_an_owner_envelope_does_not_start() {
    assert_eq!(owner_envelope(None), Err(Deny::NoOwnerEnvelope));
    assert_eq!(owner_envelope(Some(0)), Err(Deny::NoOwnerEnvelope));
    assert_eq!(owner_envelope(Some(1_000)), Ok(1_000));
}

/// M1-06-A5: the envelope is enforced at the dispatch, not only declared. A request larger than the
/// remaining envelope is refused before anything is sent.
#[test]
fn a_request_larger_than_the_envelope_is_refused_before_dispatch() {
    let state = Arc::new(Mutex::new(BrokerState::default()));
    let grant = BrokerGrant {
        job_id: "kroma-run".to_string(),
        subject: "kroma-run".to_string(),
        endpoints: [ENDPOINT.to_string()].into_iter().collect(),
        models: [MODEL.to_string()].into_iter().collect(),
        token_limit: owner_envelope(Some(100)).expect("a declared envelope"),
        cost_limit_micros: 0,
        epoch: 0,
    };
    let identity = BrokerIdentity {
        job_id: grant.job_id.clone(),
        subject: grant.subject.clone(),
        epoch: grant.epoch,
    };
    let gate = BrokerGate::new(grant, identity, state);
    let binding = DispatchBinding {
        endpoint: ENDPOINT.to_string(),
        model: MODEL.to_string(),
    };
    match gate.admit(Some(&binding), 1_000) {
        Err(Deny::BudgetExceeded {
            requested,
            remaining,
        }) => {
            assert_eq!(requested, 1_000);
            assert_eq!(remaining, 100);
        }
        other => panic!("expected the envelope to refuse the claim, got {other:?}"),
    }
}
