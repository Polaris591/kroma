//! R07 / R09 / R14: evaluation authorization and verdict semantics.
//!
//! * R07 — a decorator must forward the provider's declared message semantics, or the runner
//!   silently stops sending the cache breakpoints an Anthropic adapter supports.
//! * R09 — a paired comparison where one side has no verdict is not evidence. The unknown side is
//!   not a functional failure, and the comparison cannot `accept` on the pairs that remain.
//! * R14 — the broker's private-address gate normalizes IPv6 (unique-local, link-local) and
//!   IPv4-mapped IPv6 addresses instead of letting a second spelling through.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use kroma_core::agent_loop::{build_runtime_with_registry, AgentRunConfig, AgentRunner};
use kroma_core::bench::{
    BenchArmResult, BenchComparisonReport, BenchRunMetadata, DecisionThresholds,
};
use kroma_core::budget::BudgetConfig;
use kroma_core::credential_broker::{
    authorize, enforce, BrokerGrant, BrokerRequest, BrokerState, Deny,
};
use kroma_core::env::LocalHostEnv;
use kroma_core::events::EventLogger;
use kroma_core::messages::{AssistantTurn, FinishReason, TokenUsage};
use kroma_core::prompt::PromptBuilder;
use kroma_core::provider::{
    DispatchBinding, MessageSemantics, ModelProvider, ProviderCompletion, ProviderError,
    ProviderRequest,
};
use kroma_core::tool::policy::{NonInteractiveApprovalHandler, PolicyEngine, PolicyMode};
use kroma_core::tool::registry::ToolRegistry;
use tokio::sync::watch;

fn stop_turn() -> AssistantTurn {
    AssistantTurn {
        content: Some("done".to_string()),
        reasoning_blocks: Vec::new(),
        tool_calls: Vec::new(),
        finish_reason: FinishReason::Stop,
        usage: Some(TokenUsage {
            input_tokens: 10,
            cached_input_tokens: None,
            cache_creation_input_tokens: None,
            output_tokens: 2,
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

/// Records the exact request the runner built, and declares the semantics an Anthropic adapter
/// would declare. A broker wrapper that drops `message_semantics` changes what the runner sends.
struct RecordingProvider {
    semantics: MessageSemantics,
    binding: Option<DispatchBinding>,
    requests: Arc<Mutex<Vec<ProviderRequest>>>,
}

impl ModelProvider for RecordingProvider {
    fn dispatch_binding(&self) -> Option<DispatchBinding> {
        self.binding.clone()
    }

    fn message_semantics(&self) -> MessageSemantics {
        self.semantics
    }

    fn complete<'a>(
        &'a self,
        req: ProviderRequest,
        _budget: &'a BudgetConfig,
        _cancel_rx: watch::Receiver<bool>,
    ) -> Pin<Box<dyn Future<Output = Result<ProviderCompletion, ProviderError>> + Send + 'a>> {
        self.requests.lock().unwrap().push(req);
        Box::pin(async move { Ok(completion(stop_turn())) })
    }
}

fn binding() -> DispatchBinding {
    DispatchBinding {
        endpoint: "https://api.example.com:443".to_string(),
        model: "fake-model".to_string(),
    }
}

/// R07: the decorator reports exactly what the wrapped adapter declares.
#[test]
fn brokered_provider_forwards_the_inner_message_semantics() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let inner: Arc<dyn ModelProvider> = Arc::new(RecordingProvider {
        semantics: MessageSemantics::all(),
        binding: Some(binding()),
        requests,
    });
    let brokered = enforce(inner, "kroma-run", 100_000);
    assert_eq!(
        brokered.message_semantics(),
        MessageSemantics::all(),
        "wrapping a provider must not erase its declared wire capabilities"
    );
}

/// R07, end to end: the real Anthropic adapter declares prompt-cache breakpoints, and the runner
/// only sends them when the provider it holds still says so after the broker wrapped it.
#[tokio::test]
async fn the_broker_keeps_cache_breakpoints_reachable_through_the_runner() {
    use kroma_core::provider::anthropic_messages::{
        AnthropicMessagesClient, AnthropicMessagesConfig,
    };

    // Adapter -> broker: the declaration survives the decorator.
    let anthropic = AnthropicMessagesClient::new(AnthropicMessagesConfig {
        base_url: "https://api.example.com".to_string(),
        api_key: "fixture".to_string(),
        model: "claude-fixture".to_string(),
        max_retries: 0,
        max_tokens: 1024,
    })
    .expect("valid base url");
    let anthropic: Arc<dyn ModelProvider> = Arc::new(anthropic);
    assert!(
        anthropic.message_semantics().cache_breakpoints,
        "the control requires an adapter that declares cache breakpoints"
    );
    let brokered_anthropic = enforce(Arc::clone(&anthropic), "kroma-run", 100_000);
    assert!(
        brokered_anthropic.message_semantics().cache_breakpoints,
        "broker-wrapping the Anthropic client must keep its cache-breakpoint capability"
    );

    // Broker -> runner: the request the runner actually builds carries the projector's markers.
    let root = std::env::temp_dir().join(format!("kroma-t34-runner-{}", uuid::Uuid::new_v4()));
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let state = root.join("state");
    std::fs::create_dir_all(&state).unwrap();

    let requests = Arc::new(Mutex::new(Vec::new()));
    let inner: Arc<dyn ModelProvider> = Arc::new(RecordingProvider {
        semantics: MessageSemantics::all(),
        binding: Some(binding()),
        requests: Arc::clone(&requests),
    });
    let provider = enforce(inner, "kroma-run", 100_000);

    let env = Arc::new(LocalHostEnv::new(&ws, &state).unwrap());
    let policy = PolicyEngine::new(
        PolicyMode::AllowWrite,
        Arc::new(NonInteractiveApprovalHandler),
    );
    let budget = BudgetConfig {
        max_model_turns: 1,
        max_run_duration: Duration::from_secs(30),
        ..BudgetConfig::default()
    };
    let runtime = build_runtime_with_registry(ToolRegistry::new(None), env, policy, budget.clone());
    let cfg = AgentRunConfig::new("reply with done", budget);
    let logger = EventLogger::new(cfg.run_id, Some(state), false).unwrap();
    let (_tx, rx) = watch::channel(false);
    let _ = AgentRunner::new(cfg, provider, runtime, rx)
        .with_prompt_builder(PromptBuilder::default())
        .run(logger)
        .await;

    let recorded = requests.lock().unwrap().clone();
    assert_eq!(
        recorded.len(),
        1,
        "the runner must have dispatched one request"
    );
    assert!(
        !recorded[0].cache_breakpoints.is_empty(),
        "the broker erased the adapter's cache-breakpoint capability, so the runner dropped them"
    );
    let _ = std::fs::remove_dir_all(&root);
}

fn arm_result(
    task_id: &str,
    strategy: &str,
    resolved: bool,
    verdict_unknown: Option<&str>,
) -> BenchArmResult {
    BenchArmResult {
        task_id: task_id.to_string(),
        strategy: strategy.to_string(),
        resolved,
        gate_status: if resolved { "approved" } else { "rejected" }.to_string(),
        self_input_tokens: 10,
        self_cached_input_tokens: 0,
        self_output_tokens: 0,
        children_input_tokens: 0,
        children_cached_input_tokens: 0,
        children_output_tokens: 0,
        reported_model: None,
        usage_basis: "provider_reported".to_string(),
        estimated_cost_nano_usd: None,
        cost_unknown_reason: None,
        request_usage: Vec::new(),
        request_attempts_used: 1,
        request_usage_complete: true,
        child_request_usage: Vec::new(),
        child_usage_complete: true,
        self_tokens: 10,
        children_tokens: 0,
        total_tokens: 10,
        budget_exceeded: false,
        budget_tokens: 1_000,
        budget_overage: 0,
        cap_ok: true,
        finished_normally: true,
        wall_time_ms: 1,
        verify_time_ms: 0,
        regressions: 0,
        cache_hit_ratio: None,
        ttft_ms_mean: None,
        ttft_ms_max: None,
        ttft_ms_samples: 0,
        repetition: 0,
        model: Some("fake-model".to_string()),
        prompt_hash: Some("prompt".to_string()),
        tool_schema_hash: Some("tools".to_string()),
        terminal_reason: Some("ModelStop".to_string()),
        failure_trace: Vec::new(),
        verdict_unknown: verdict_unknown.map(str::to_string),
        max_subagents: 0,
        bash_allowed: false,
        exposed_tools: vec!["read".to_string()],
        counterfactual_probe: false,
        causal_pruning: false,
        require_verified: true,
        on_demand_tools: false,
        turns_used: 1,
        route_decisions: Vec::new(),
        tool_calls_used: 0,
        modified_files: Vec::new(),
        final_text: None,
        tool_trace: Vec::new(),
        mechanism: None,
    }
}

fn report_with(results: Vec<BenchArmResult>) -> BenchComparisonReport {
    let thresholds = DecisionThresholds {
        min_effect: 0.0,
        min_clusters: 5,
        max_regression_failures: Some(0),
        max_cost_increase_nano_usd: None,
    };
    BenchComparisonReport::from_results(
        "t34",
        1_000,
        "provider",
        results,
        BenchRunMetadata::default(),
        Some("baseline".to_string()),
        Some(&thresholds),
        &[],
    )
}

/// R09: five baseline runs with no verdict and five candidate runs with a real verdict must not
/// become `accept`. The same shape *without* the unknown verdicts does accept, which is the
/// control that keeps this test honest.
#[test]
fn a_pair_with_an_unknown_verdict_cannot_accept() {
    let mut results = Vec::new();
    for index in 0..5 {
        let id = format!("task-{index}");
        results.push(arm_result(
            &id,
            "baseline",
            false,
            Some("harness could not seal the workspace"),
        ));
        results.push(arm_result(&id, "candidate", true, None));
    }
    let report = report_with(results);
    let delta = report
        .paired_deltas
        .iter()
        .find(|delta| delta.arm == "candidate")
        .expect("the candidate arm is compared");
    assert_eq!(
        delta.decision.as_deref(),
        Some("inconclusive"),
        "an unknown baseline is missing evidence, not a failure: {delta:?}"
    );
    assert_eq!(delta.unknown_verdict_pairs, 5, "{delta:?}");
    assert_eq!(delta.paired_observations, 0, "{delta:?}");
    assert!(
        delta
            .decision_rationale
            .as_deref()
            .is_some_and(|why| why.contains("no verdict")),
        "{delta:?}"
    );

    // Control: the identical pass/fail pattern with a real verdict on both sides is exactly the
    // situation that used to accept, so the refusal above is caused by the missing verdict.
    let mut known = Vec::new();
    for index in 0..5 {
        let id = format!("task-{index}");
        known.push(arm_result(&id, "baseline", false, None));
        known.push(arm_result(&id, "candidate", true, None));
    }
    let control = report_with(known);
    let control_delta = control
        .paired_deltas
        .iter()
        .find(|delta| delta.arm == "candidate")
        .expect("the candidate arm is compared");
    assert_eq!(
        control_delta.decision.as_deref(),
        Some("accept"),
        "{control_delta:?}"
    );
}

/// R14: an allowlisted endpoint is still refused when its address is private in any spelling.
#[test]
fn ipv6_and_mapped_private_endpoints_are_refused_even_when_allowlisted() {
    let state = BrokerState::default();
    for endpoint in [
        "https://[::ffff:169.254.169.254]:443",
        "https://[fc00::1]:443",
        "https://[fe80::1]:443",
        "https://[::ffff:127.0.0.1]:443",
    ] {
        let grant = BrokerGrant {
            job_id: "job-1".to_string(),
            subject: "worker".to_string(),
            endpoints: [endpoint.to_string()].into_iter().collect(),
            models: ["m".to_string()].into_iter().collect(),
            token_limit: 1_000,
            cost_limit_micros: 0,
            epoch: 0,
        };
        let request = BrokerRequest {
            job_id: "job-1".to_string(),
            subject: "worker".to_string(),
            endpoint: endpoint.to_string(),
            model: "m".to_string(),
            max_tokens: 10,
            follows_redirect_to: None,
            epoch: 0,
        };
        assert!(
            matches!(
                authorize(&grant, &request, &state),
                Err(Deny::PrivateAddress { .. })
            ),
            "{endpoint} must be refused as a private target"
        );
    }

    // A public address in the same spelling is still admitted.
    let endpoint = "https://[2606:4700:4700::1111]:443";
    let grant = BrokerGrant {
        job_id: "job-1".to_string(),
        subject: "worker".to_string(),
        endpoints: [endpoint.to_string()].into_iter().collect(),
        models: ["m".to_string()].into_iter().collect(),
        token_limit: 1_000,
        cost_limit_micros: 0,
        epoch: 0,
    };
    let request = BrokerRequest {
        job_id: "job-1".to_string(),
        subject: "worker".to_string(),
        endpoint: endpoint.to_string(),
        model: "m".to_string(),
        max_tokens: 10,
        follows_redirect_to: None,
        epoch: 0,
    };
    assert_eq!(authorize(&grant, &request, &state), Ok(()));
}
