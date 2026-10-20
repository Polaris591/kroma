//! Exercise the real runner: observation must precede dispatch and leave requests unchanged.
use kroma_core::agent_loop::{build_default_runtime, AgentRunConfig, AgentRunner};
use kroma_core::budget::BudgetConfig;
use kroma_core::env::LocalHostEnv;
use kroma_core::events::{EventLogger, EventPayload};
use kroma_core::messages::{AssistantTurn, FinishReason, TokenUsage, ToolCall};
use kroma_core::prompt::PromptBuilder;
use kroma_core::provider::{
    MessageSemantics, ModelProvider, ProviderCompletion, ProviderError, ProviderRequest,
};
use kroma_core::request_observation::RequestComposition;
use kroma_core::tool::policy::{NonInteractiveApprovalHandler, PolicyEngine, PolicyMode};
use kroma_core::types::ToolCallId;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use tokio::sync::watch;

struct FixtureProvider {
    requests: Mutex<Vec<ProviderRequest>>,
    observations: Arc<Mutex<Vec<RequestComposition>>>,
}

impl ModelProvider for FixtureProvider {
    fn message_semantics(&self) -> MessageSemantics {
        MessageSemantics::all()
    }

    fn complete<'a>(
        &'a self,
        request: ProviderRequest,
        _budget: &'a BudgetConfig,
        _cancel: watch::Receiver<bool>,
    ) -> Pin<Box<dyn Future<Output = Result<ProviderCompletion, ProviderError>> + Send + 'a>> {
        Box::pin(async move {
            let mut requests = self.requests.lock().unwrap();
            assert_eq!(
                self.observations.lock().unwrap().len(),
                requests.len() + 1,
                "composition must be recorded before the provider sees each request"
            );
            let first = requests.is_empty();
            requests.push(request);
            Ok(ProviderCompletion {
                turn: AssistantTurn {
                    content: (!first).then(|| "done".into()),
                    reasoning_blocks: Vec::new(),
                    tool_calls: if first {
                        vec![ToolCall {
                            call_id: ToolCallId::new("read-fixture"),
                            name: "read".into(),
                            raw_arguments: r#"{"path":"fixture.txt"}"#.into(),
                        }]
                    } else {
                        Vec::new()
                    },
                    finish_reason: if first {
                        FinishReason::ToolCalls
                    } else {
                        FinishReason::Stop
                    },
                    usage: Some(TokenUsage {
                        input_tokens: 10,
                        output_tokens: 5,
                        ..Default::default()
                    }),
                },
                request_body_bytes: 123,
                latency_ms: 1,
                ttft_ms: Some(1),
                attempts_used: 1,
                reported_model: Some("local-fixture".into()),
            })
        })
    }
}

#[tokio::test]
async fn rpc_surface_uses_the_host_selected_candidate_prompt() {
    use kroma_core::prompt::{PromptProfile, EFFICIENT_CANDIDATE_SYSTEM_PROMPT};
    use kroma_core::surface::{JsonRpcRequest, KromaRuntimeConfig, RpcDispatcher};
    struct Capture(Mutex<Vec<String>>);
    impl ModelProvider for Capture {
        fn complete<'a>(
            &'a self,
            request: ProviderRequest,
            _budget: &'a BudgetConfig,
            _cancel: watch::Receiver<bool>,
        ) -> Pin<Box<dyn Future<Output = Result<ProviderCompletion, ProviderError>> + Send + 'a>>
        {
            self.0.lock().unwrap().push(request.system_prompt);
            Box::pin(async {
                Ok(ProviderCompletion {
                    turn: AssistantTurn {
                        content: Some("done".into()),
                        reasoning_blocks: Vec::new(),
                        tool_calls: Vec::new(),
                        finish_reason: FinishReason::Stop,
                        usage: Some(TokenUsage {
                            input_tokens: 10,
                            output_tokens: 2,
                            ..Default::default()
                        }),
                    },
                    request_body_bytes: 123,
                    latency_ms: 1,
                    ttft_ms: Some(1),
                    attempts_used: 1,
                    reported_model: Some("local-fixture".into()),
                })
            })
        }
    }
    let root = std::env::temp_dir().join(format!("kroma-profile-rpc-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(root.join("ws")).unwrap();
    let mut config =
        KromaRuntimeConfig::new(root.join("ws"), root.join("state"), PolicyMode::ReadOnly);
    config.prompt_profile = PromptProfile::EfficientCandidate;
    let provider = Arc::new(Capture(Mutex::new(Vec::new())));
    let dispatcher = RpcDispatcher::new(config, provider.clone()).unwrap();
    let response = dispatcher
        .handle_request(JsonRpcRequest::new(
            1,
            "session.prompt",
            serde_json::json!({"prompt": "fixture", "max_tokens": 50_000}),
        ))
        .await;
    assert!(response.is_ok(), "{:?}", response.error);
    {
        let prompts = provider.0.lock().unwrap();
        assert_eq!(prompts.len(), 1);
        assert!(prompts[0].starts_with(EFFICIENT_CANDIDATE_SYSTEM_PROMPT));
    }

    // Exercise the actual bench executor seam, not just CLI argument parsing.
    use kroma_core::bench::{
        BenchExecutor, BenchStrategyConfig, BenchTask, ProviderBenchExecutor, ProviderBenchSettings,
    };
    let bench_provider = Arc::new(Capture(Mutex::new(Vec::new())));
    let executor = ProviderBenchExecutor::new(
        bench_provider.clone(),
        ProviderBenchSettings::new("local-fixture", "chat-completions"),
    )
    .with_prompt_builder(PromptBuilder::efficient_candidate());
    let scope = kroma_core::job_scope::JobScope::open(&root, "bench-profile").unwrap();
    let task = BenchTask {
        id: "profile-fixture".into(),
        prompt: "fixture".into(),
        files: Default::default(),
        expect_contains: Vec::new(),
        expect_absent: Vec::new(),
        regression_guard: false,
        verify_command: None,
    };
    let _result = executor
        .execute(&task, &BenchStrategyConfig::single_agent(), 50_000, &scope)
        .await;
    let bench_prompts = bench_provider.0.lock().unwrap();
    assert_eq!(bench_prompts.len(), 1);
    assert_eq!(bench_prompts[0], EFFICIENT_CANDIDATE_SYSTEM_PROMPT);
    drop(scope);
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn runner_observes_each_request_without_injecting_statistics_into_model_context() {
    for prompt in [
        PromptBuilder::default(),
        PromptBuilder::efficient_candidate(),
    ] {
        let root = std::env::temp_dir().join(format!("kroma-composition-{}", uuid::Uuid::new_v4()));
        let workspace = root.join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(workspace.join("fixture.txt"), "PRIVATE_FILE_CONTENT").unwrap();
        let budget = BudgetConfig::default();
        let environment = Arc::new(LocalHostEnv::new(&workspace, root.join("state")).unwrap());
        let runtime = build_default_runtime(
            environment,
            PolicyEngine::new(
                PolicyMode::ReadOnly,
                Arc::new(NonInteractiveApprovalHandler),
            ),
            budget.clone(),
            None,
        );
        let config = AgentRunConfig::new("read fixture.txt", budget);
        let observations = Arc::new(Mutex::new(Vec::new()));
        let provider = Arc::new(FixtureProvider {
            requests: Mutex::new(Vec::new()),
            observations: observations.clone(),
        });
        let mut logger = EventLogger::new(config.run_id, Some(root.join("events")), true).unwrap();
        let capture = observations.clone();
        logger.set_event_sink(Arc::new(move |event| {
            if let EventPayload::RequestCompositionObserved { composition, .. } = &event.event {
                let encoded = serde_json::to_string(composition).unwrap();
                assert!(!encoded.contains("PRIVATE_FILE_CONTENT"));
                capture.lock().unwrap().push(composition.clone());
            }
        }));
        let (_sender, receiver) = watch::channel(false);
        let (summary, _) = AgentRunner::new(config, provider.clone(), runtime, receiver)
            .with_prompt_builder(prompt.clone())
            .run(logger)
            .await;
        assert!(
            summary.terminal_reason == kroma_core::types::TerminalReason::ModelStop,
            "{:?}",
            summary.terminal_reason
        );
        let requests = provider.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].system_prompt, prompt.system_prompt());
        assert_eq!(requests[0].messages.len(), 1);
        assert_eq!(requests[1].messages.len(), 3);
        for request in requests.iter() {
            let content = serde_json::to_string(&request.messages).unwrap();
            assert!(!content.contains("RequestCompositionObserved"));
            assert!(!content.contains("matching_history_prefix_messages"));
        }
        let observations = observations.lock().unwrap();
        assert_eq!(observations[1].system_unchanged, Some(true));
        assert_eq!(observations[1].tools_unchanged, Some(true));
        assert_eq!(observations[1].matching_history_prefix_messages, Some(1));
        assert!(observations[1].tool_result_utf8_bytes >= "PRIVATE_FILE_CONTENT".len());
        std::fs::remove_dir_all(root).unwrap();
    }
}
