//! T12 (KR-09): discovery → activation → call → result → unmount, as a real round trip.
//!
//! The defect this pins: `discover_tools` used to print a catalog into a tool *result* while the next
//! request's `tools` array still held only the core set. A provider that enforces "you may only call
//! a declared tool" — which is what the wire protocols do — could therefore never call what it had
//! just discovered. The loop was open exactly where it mattered.

use kroma_core::budget::BudgetConfig;
use kroma_core::events::EventLogger;
use kroma_core::messages::{AssistantTurn, FinishReason, TokenUsage, ToolCall};
use kroma_core::provider::{ModelProvider, ProviderCompletion, ProviderError, ProviderRequest};
use kroma_core::session::SessionJournal;
use kroma_core::tool::activation::{
    ActivationError, ToolActivation, MAX_ACTIVE_SCHEMA_BYTES, MAX_ACTIVE_TOOLS,
};
use kroma_core::tool::definition::{RegisteredTool, ToolDefinition, ToolMetadata};
use kroma_core::tool::policy::{NonInteractiveApprovalHandler, PolicyEngine, PolicyMode};
use kroma_core::tool::registry::ToolRegistry;
use kroma_core::tool::runtime::ToolRuntime;
use kroma_core::types::EffectClass;
use kroma_core::types::ToolCallId;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

fn tool(name: &str, schema_bytes: usize) -> RegisteredTool {
    let mut parameters = serde_json::json!({"type": "object", "properties": {}});
    if schema_bytes > 0 {
        parameters["properties"]["pad"] = serde_json::json!({
            "type": "string",
            "description": "p".repeat(schema_bytes)
        });
    }
    RegisteredTool {
        definition: ToolDefinition {
            name: name.to_string(),
            description: format!("{} tool", name),
            parameters,
        },
        metadata: ToolMetadata {
            effect_class: EffectClass::ReadOnly,
            default_timeout_sec: 30,
            inline_output_cap_bytes: 4096,
        },
    }
}

fn activation_for(tools: &[RegisteredTool]) -> ToolActivation {
    let mut state = ToolActivation::new();
    for t in tools {
        state.activate(t, true).unwrap();
    }
    state
}

/// Discovery must never widen authority: a policy-denied tool cannot be activated.
#[test]
fn discover_never_activates_denied_tool() {
    let registry = ToolRegistry::new(None);
    let runtime = ToolRuntime::new(
        registry,
        PolicyEngine::new(
            PolicyMode::ReadOnly,
            Arc::new(NonInteractiveApprovalHandler),
        ),
        Arc::new(
            kroma_core::env::LocalHostEnv::new(
                std::env::temp_dir(),
                std::env::temp_dir().join("kroma-t12-state"),
            )
            .unwrap(),
        ),
        BudgetConfig::default(),
    );

    // `write` exists but is denied under ReadOnly.
    let results = runtime.activate_tools(&["write".to_string()]);
    assert_eq!(results.len(), 1);
    match &results[0].1 {
        Err(ActivationError::DeniedByPolicy(name)) => assert_eq!(name, "write"),
        other => panic!("expected a policy refusal, got {other:?}"),
    }
    assert!(
        runtime.active_tool_names().is_empty(),
        "a refused activation must not appear in the active set"
    );
    assert!(
        !runtime
            .activated_tool_definitions()
            .iter()
            .any(|d| d.name == "write"),
        "a denied tool must not gain a schema"
    );

    // An unknown name is refused as unknown, not silently ignored.
    let unknown = runtime.activate_tools(&["no_such_tool".to_string()]);
    assert!(matches!(unknown[0].1, Err(ActivationError::UnknownTool(_))));

    // A permitted, non-core tool activates and gets a schema.
    let ok = runtime.activate_tools(&["code_graph".to_string()]);
    assert!(ok[0].1.is_ok(), "code_graph should activate: {:?}", ok[0].1);
    assert!(runtime
        .activated_tool_definitions()
        .iter()
        .any(|d| d.name == "code_graph"));
}

/// Budgets are enforced, and the schema hash of what was activated is recorded.
#[test]
fn schema_activation_budget_and_cache_transition_are_logged() {
    let tools: Vec<RegisteredTool> = (0..(MAX_ACTIVE_TOOLS + 2))
        .map(|i| tool(&format!("t{}", i), 0))
        .collect();
    let mut state = ToolActivation::new();
    for t in tools.iter().take(MAX_ACTIVE_TOOLS) {
        state.activate(t, true).unwrap();
    }
    let before_digest = state.digest();
    // The count cap is a refusal, not a silent truncation.
    match state.activate(&tools[MAX_ACTIVE_TOOLS], true) {
        Err(ActivationError::TooManyActive { limit }) => assert_eq!(limit, MAX_ACTIVE_TOOLS),
        other => panic!("expected TooManyActive, got {other:?}"),
    }
    assert_eq!(state.len(), MAX_ACTIVE_TOOLS);

    // Every activation records a schema hash and byte count.
    for record in state.records() {
        assert_eq!(record.schema_hash.len(), 64);
        assert!(record.schema_bytes > 0);
    }
    // Order is monotonic and stable, so two requests can be diffed.
    let orders: Vec<u64> = state.records().iter().map(|r| r.order).collect();
    assert_eq!(orders, (0..MAX_ACTIVE_TOOLS as u64).collect::<Vec<_>>());

    // One tool whose schema alone is over the byte budget is refused.
    let huge = tool("huge", MAX_ACTIVE_SCHEMA_BYTES);
    let mut fresh = ToolActivation::new();
    match fresh.activate(&huge, true) {
        Err(ActivationError::SchemaTooLarge { .. })
        | Err(ActivationError::SchemaBudgetExceeded { .. }) => {}
        other => panic!("expected a byte-budget refusal, got {other:?}"),
    }

    // Deactivation changes the digest: the cache boundary moved.
    let mut state = activation_for(&tools[..2]);
    let with_two = state.digest();
    state.deactivate("t0");
    assert_ne!(state.digest(), with_two);
    assert_ne!(state.digest(), before_digest);
    assert!(!state.is_active("t0"));
}

/// Unmounting removes the schema *and* the dispatch: there is no second copy to go stale.
#[test]
fn unmount_removes_schema_and_dispatch() {
    let registry = ToolRegistry::new(None);
    let probe = tool("t12_probe", 0);
    let extension =
        kroma_core::extensions::ScopedExtension::new(kroma_core::extensions::ExtensionManifest {
            name: "t12-ext".to_string(),
            version: "1.0.0".to_string(),
            api_version: "1.0.0".to_string(),
            capabilities: vec!["tools".to_string()],
            trust_level: kroma_core::extensions::ExtensionTrustLevel::OperatorTrusted,
            source: "test".to_string(),
            checksum: None,
        })
        .with_tool(kroma_core::extensions::ScopedToolDefinition {
            definition: probe.definition.clone(),
            metadata: probe.metadata,
            handler: Arc::new(
                |_raw: &str, _ctx: &kroma_core::extensions::CustomToolContext<'_>| {
                    kroma_core::tools::ToolExecutionOutput::success(
                        "probe".to_string(),
                        kroma_core::messages::ToolResultMeta::default(),
                    )
                },
            ),
        });
    let id = registry.register_extension(extension).expect("register");
    let runtime = ToolRuntime::new(
        registry,
        PolicyEngine::new(
            PolicyMode::AllowWrite,
            Arc::new(NonInteractiveApprovalHandler),
        ),
        Arc::new(
            kroma_core::env::LocalHostEnv::new(
                std::env::temp_dir(),
                std::env::temp_dir().join("kroma-t12-unmount"),
            )
            .unwrap(),
        ),
        BudgetConfig::default(),
    );

    let activated = runtime.activate_tools(&["t12_probe".to_string()]);
    assert!(activated[0].1.is_ok(), "{:?}", activated[0].1);
    assert!(runtime
        .activated_tool_definitions()
        .iter()
        .any(|d| d.name == "t12_probe"));

    runtime
        .registry()
        .unregister_extension(&id)
        .expect("unmount");

    assert!(
        !runtime
            .activated_tool_definitions()
            .iter()
            .any(|d| d.name == "t12_probe"),
        "an unmounted tool must not keep a schema"
    );
    assert!(
        runtime.active_tool_names().is_empty(),
        "the stale activation must be pruned"
    );
}

// ---------------------------------------------------------------------------------------------
// The whole loop through a real run, against a provider that only permits declared tools
// ---------------------------------------------------------------------------------------------

/// A provider that behaves like the wire protocols: calling a tool that was not declared in this
/// request is a protocol error, not something the harness quietly tolerates.
struct StrictProvider {
    turns: Mutex<std::collections::VecDeque<AssistantTurn>>,
    seen_tools: Arc<Mutex<Vec<Vec<String>>>>,
    violations: Arc<Mutex<Vec<String>>>,
}

impl ModelProvider for StrictProvider {
    fn complete<'a>(
        &'a self,
        req: ProviderRequest,
        _budget: &'a BudgetConfig,
        _cancel_rx: tokio::sync::watch::Receiver<bool>,
    ) -> Pin<Box<dyn Future<Output = Result<ProviderCompletion, ProviderError>> + Send + 'a>> {
        let declared: Vec<String> = req.tools.iter().map(|t| t.name.clone()).collect();
        self.seen_tools.lock().unwrap().push(declared.clone());
        let turn = self
            .turns
            .lock()
            .unwrap()
            .pop_front()
            .expect("scripted turn");
        for call in &turn.tool_calls {
            if !declared.iter().any(|name| name == &call.name) {
                self.violations
                    .lock()
                    .unwrap()
                    .push(format!("{} called but not declared", call.name));
            }
        }
        Box::pin(async move {
            Ok(ProviderCompletion {
                turn,
                request_body_bytes: 1,
                latency_ms: 1,
                ttft_ms: None,
                attempts_used: 1,
                reported_model: None,
            })
        })
    }
}

fn usage() -> Option<TokenUsage> {
    Some(TokenUsage {
        input_tokens: 100,
        cache_creation_input_tokens: None,
        output_tokens: 10,
        reasoning_tokens: None,
        cached_input_tokens: None,
    })
}

fn call(id: &str, name: &str, args: serde_json::Value) -> ToolCall {
    ToolCall {
        call_id: ToolCallId::new(id),
        name: name.to_string(),
        raw_arguments: args.to_string(),
    }
}

#[tokio::test]
async fn strict_provider_can_call_newly_discovered_tool() {
    use kroma_core::agent_loop::{build_default_runtime, AgentRunConfig, AgentRunner};

    let root = std::env::temp_dir().join(format!("kroma-t12-loop-{}", uuid::Uuid::new_v4()));
    let ws = root.join("ws");
    let state = root.join("state");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(ws.join("lib.rs"), "pub fn a() {}\n").unwrap();

    let turns: std::collections::VecDeque<AssistantTurn> = vec![
        // 1. discover + activate a tool that is not in the core set.
        AssistantTurn {
            content: None,
            reasoning_blocks: Vec::new(),
            tool_calls: vec![call(
                "call_discover",
                "discover_tools",
                serde_json::json!({"query": "code_graph", "activate": ["code_graph"]}),
            )],
            finish_reason: FinishReason::ToolCalls,
            usage: usage(),
        },
        // 2. call it. This is only legal if activation really reached the request's tool list.
        AssistantTurn {
            content: None,
            reasoning_blocks: Vec::new(),
            tool_calls: vec![call(
                "call_graph",
                "code_graph",
                serde_json::json!({"action": "summary"}),
            )],
            finish_reason: FinishReason::ToolCalls,
            usage: usage(),
        },
        // 3. unload it again.
        AssistantTurn {
            content: None,
            reasoning_blocks: Vec::new(),
            tool_calls: vec![call(
                "call_unload",
                "discover_tools",
                serde_json::json!({"deactivate": ["code_graph"]}),
            )],
            finish_reason: FinishReason::ToolCalls,
            usage: usage(),
        },
        AssistantTurn {
            content: Some("done".to_string()),
            reasoning_blocks: Vec::new(),
            tool_calls: Vec::new(),
            finish_reason: FinishReason::Stop,
            usage: usage(),
        },
    ]
    .into_iter()
    .collect();

    let seen_tools = Arc::new(Mutex::new(Vec::new()));
    let violations = Arc::new(Mutex::new(Vec::new()));
    let budget = BudgetConfig::default();
    let env = Arc::new(kroma_core::env::LocalHostEnv::new(&ws, state.clone()).unwrap());
    let policy = PolicyEngine::new(
        PolicyMode::AllowWrite,
        Arc::new(NonInteractiveApprovalHandler),
    );
    let runtime = build_default_runtime(env, policy, budget.clone(), None);
    let cfg = AgentRunConfig::new("discover then call", budget).with_on_demand_tools(true);
    let logger = EventLogger::new(cfg.run_id, Some(state.clone()), false).unwrap();
    let journal = SessionJournal::create(state.join("sessions"), &ws).unwrap();
    let (_tx, rx) = tokio::sync::watch::channel(false);
    let (summary, _logger) = AgentRunner::new(
        cfg,
        Arc::new(StrictProvider {
            turns: Mutex::new(turns),
            seen_tools: Arc::clone(&seen_tools),
            violations: Arc::clone(&violations),
        }),
        runtime,
        rx,
    )
    .with_session_journal(journal)
    .with_snapshot_store(Arc::new(kroma_core::revision::SnapshotStore::new(
        root.join("snapshots"),
    )))
    .run(logger)
    .await;

    let seen = seen_tools.lock().unwrap().clone();
    assert!(
        violations.lock().unwrap().is_empty(),
        "the model called an undeclared tool: {:?}",
        violations.lock().unwrap()
    );
    assert!(
        seen.len() >= 3,
        "expected at least three requests, got {}",
        seen.len()
    );
    assert!(
        !seen[0].iter().any(|n| n == "code_graph"),
        "on-demand mode must not pin the schema up front: {:?}",
        seen[0]
    );
    assert!(
        seen[1].iter().any(|n| n == "code_graph"),
        "activation must reach the next request's tool list: {:?}",
        seen[1]
    );
    // `seen[2]` is the request that *carries* the deactivate call, so it was built while the tool was
    // still active. The request after it must no longer offer the schema.
    assert!(
        seen[2].iter().any(|n| n == "code_graph"),
        "the deactivating request still has the schema it is about to drop: {:?}",
        seen[2]
    );
    assert!(
        !seen[3].iter().any(|n| n == "code_graph"),
        "deactivation must remove the schema from the following request: {:?}",
        seen[3]
    );
    assert_eq!(summary.turns_used, 4);

    let transcript = summary.transcript;
    let discover_result = transcript
        .iter()
        .find_map(|item| match item {
            kroma_core::messages::TranscriptItem::ToolResult(tr)
                if tr.tool_name == "discover_tools" =>
            {
                Some(tr.content.to_string())
            }
            _ => None,
        })
        .expect("the discovery result must be in the transcript");
    assert!(
        discover_result.contains("activated code_graph"),
        "activation must be reported with its schema identity: {discover_result}"
    );
    assert!(
        discover_result.contains("digest"),
        "the active-set digest must be reported: {discover_result}"
    );

    let _ = std::fs::remove_dir_all(&root);
}
