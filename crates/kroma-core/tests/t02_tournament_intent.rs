//! T02 (KR-30): the object a human or a hook approves must be the object that runs.
//!
//! Three things are pinned here:
//! 1. `CanonicalTournamentIntent` is stable: equal intents always produce the same digest, and
//!    changing *any* candidate-defining field changes it.
//! 2. A one-shot approval grant is bound to that digest and to the workspace revision, so it cannot
//!    be replayed for a different race and cannot survive a revision change.
//! 3. The hook really sees the canonical intent (expanded strategies, effective budget, merged
//!    protection set, resolved verification policy) and malformed RPC parameters fail *before* any
//!    approval prompt is raised.

use kroma_core::budget::BudgetConfig;
use kroma_core::extensions::{
    ExtensionManifest, HookContext, HookError, HookEvent, PolicyHook, PolicyHookDecision,
    ScopedExtension,
};
use kroma_core::subagents::{CanonicalTournamentBudget, CanonicalTournamentIntent};
use kroma_core::tool::policy::{
    ApprovalDecision, ApprovalHandler, ApprovalRequest, AuthEntry, AuthScope, PolicyEngine,
    PolicyMode,
};
use kroma_core::tool::registry::ToolRegistry;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

fn intent() -> CanonicalTournamentIntent {
    CanonicalTournamentIntent {
        task: "fix the failing add()".to_string(),
        candidates: 2,
        strategies: vec!["smallest change".to_string(), "defensive fix".to_string()],
        allowed_paths: Some(vec!["src/**".to_string()]),
        max_turns: Some(6),
        timeout_ms: 30_000,
        disallow_test_tampering: true,
        protected_paths: vec!["tests/**".to_string()],
        require_verified: true,
        verification_policy_source: "host_strict".to_string(),
        budget: CanonicalTournamentBudget::from_config(&BudgetConfig::default()),
        predictive_check: true,
    }
}

/// Equal intents serialize identically regardless of construction order, and every field that can
/// change what runs changes the digest.
#[test]
fn intent_digest_is_stable_and_changes_with_every_field() {
    let base = intent();
    assert_eq!(base.digest(), intent().digest());
    assert_eq!(
        base.canonical_json(),
        intent().canonical_json(),
        "canonical JSON must be deterministic"
    );
    // Object keys are sorted, so the payload is byte-stable.
    assert!(base.canonical_json().starts_with("{\"allowed_paths\":"));
    assert_eq!(base.digest().len(), 64);

    let mutations: Vec<(&str, CanonicalTournamentIntent)> = vec![
        (
            "task",
            CanonicalTournamentIntent {
                task: "fix something else".into(),
                ..intent()
            },
        ),
        (
            "candidates",
            CanonicalTournamentIntent {
                candidates: 3,
                ..intent()
            },
        ),
        (
            "strategies",
            CanonicalTournamentIntent {
                strategies: vec!["smallest change".into(), "different strategy".into()],
                ..intent()
            },
        ),
        (
            "allowed_paths",
            CanonicalTournamentIntent {
                allowed_paths: Some(vec!["src/lib.rs".into()]),
                ..intent()
            },
        ),
        (
            "allowed_paths_removed",
            CanonicalTournamentIntent {
                allowed_paths: None,
                ..intent()
            },
        ),
        (
            "max_turns",
            CanonicalTournamentIntent {
                max_turns: Some(7),
                ..intent()
            },
        ),
        (
            "timeout_ms",
            CanonicalTournamentIntent {
                timeout_ms: 31_000,
                ..intent()
            },
        ),
        (
            "disallow_test_tampering",
            CanonicalTournamentIntent {
                disallow_test_tampering: false,
                ..intent()
            },
        ),
        (
            "protected_paths",
            CanonicalTournamentIntent {
                protected_paths: vec!["tests/**".into(), "oracle/**".into()],
                ..intent()
            },
        ),
        (
            "require_verified",
            CanonicalTournamentIntent {
                require_verified: false,
                ..intent()
            },
        ),
        (
            "verification_policy_source",
            CanonicalTournamentIntent {
                verification_policy_source: "host_relaxed".into(),
                ..intent()
            },
        ),
        (
            "budget",
            CanonicalTournamentIntent {
                budget: CanonicalTournamentBudget {
                    max_total_tokens: Some(1_000),
                    ..CanonicalTournamentBudget::from_config(&BudgetConfig::default())
                },
                ..intent()
            },
        ),
        (
            "predictive_check",
            CanonicalTournamentIntent {
                predictive_check: false,
                ..intent()
            },
        ),
    ];
    for (field, mutated) in mutations {
        assert_ne!(
            mutated.digest(),
            base.digest(),
            "changing `{field}` must invalidate the approval digest"
        );
    }
}

struct AllowOnceApprover {
    calls: AtomicUsize,
}

impl ApprovalHandler for AllowOnceApprover {
    fn request_approval(&self, _req: &ApprovalRequest) -> ApprovalDecision {
        self.calls.fetch_add(1, Ordering::SeqCst);
        ApprovalDecision::AllowOnce
    }
    fn is_interactive(&self) -> bool {
        true
    }
}

fn tournament_scope(revision: &str) -> AuthScope {
    AuthScope {
        entry: AuthEntry::Tournament,
        tool_name: "spawn_subagent".to_string(),
        run_id: None,
        workspace_root: std::path::PathBuf::from("/ws"),
        revision: Some(kroma_core::revision::RevisionId::new(revision)),
    }
}

fn approval_request() -> ApprovalRequest {
    ApprovalRequest {
        tool_name: "spawn_subagent".to_string(),
        call_id: None,
        effect_class: kroma_core::types::EffectClass::Mutating,
        is_sensitive_path: false,
        target_path: None,
        diff_preview: None,
        command_preview: None,
        workspace_root: std::path::PathBuf::from("/ws"),
    }
}

/// A one-shot grant is bound to the digest *and* the revision it was approved for.
#[test]
fn one_shot_tournament_grant_cannot_be_replayed_or_retargeted() {
    let approver = Arc::new(AllowOnceApprover {
        calls: AtomicUsize::new(0),
    });
    let engine = PolicyEngine::new(PolicyMode::Ask, approver.clone());
    let scope = tournament_scope("r-00000000000000000000000000000001");
    let approved = intent();
    let other = CanonicalTournamentIntent {
        task: "a completely different race".to_string(),
        ..intent()
    };

    engine.prompt_approval_scoped(
        &scope,
        &approval_request(),
        Some(&approved.digest()),
        Some(std::time::Duration::from_secs(60)),
    );
    assert_eq!(approver.calls.load(Ordering::SeqCst), 1);

    // The approved digest authorizes exactly once.
    assert!(engine.consume_grant(&scope, &approved.digest()).is_ok());
    assert!(
        engine.consume_grant(&scope, &approved.digest()).is_err(),
        "a one-shot grant must not be replayable"
    );

    // A different intent has a different digest, so the grant does not cover it.
    engine.prompt_approval_scoped(
        &scope,
        &approval_request(),
        Some(&approved.digest()),
        Some(std::time::Duration::from_secs(60)),
    );
    assert!(
        engine.consume_grant(&scope, &other.digest()).is_err(),
        "an approval for one intent must not authorize a different race"
    );
    // And it still authorizes the intent it was actually granted for.
    assert!(engine.consume_grant(&scope, &approved.digest()).is_ok());

    // A revision change invalidates the approval even with the right digest.
    engine.prompt_approval_scoped(
        &scope,
        &approval_request(),
        Some(&approved.digest()),
        Some(std::time::Duration::from_secs(60)),
    );
    let advanced = tournament_scope("r-00000000000000000000000000000002");
    assert!(
        engine.consume_grant(&advanced, &approved.digest()).is_err(),
        "an approval must not survive a revision change"
    );
}

// ---------------------------------------------------------------------------------------------
// The hook sees the canonical intent
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Default)]
struct RecordingHook {
    seen: Arc<Mutex<Vec<String>>>,
}

impl PolicyHook for RecordingHook {
    fn id(&self) -> &str {
        "t02-record-intent"
    }
    fn events(&self) -> &'static [HookEvent] {
        &[HookEvent::PreToolUse]
    }
    fn evaluate(
        &self,
        _event: HookEvent,
        ctx: &HookContext,
    ) -> Result<PolicyHookDecision, HookError> {
        if let Some(raw) = &ctx.raw_arguments {
            self.seen.lock().unwrap().push(raw.clone());
        }
        Ok(PolicyHookDecision::Allow)
    }
}

fn registry_with(hook: RecordingHook) -> ToolRegistry {
    let mut manifest = ExtensionManifest::operator_trusted("t02-recorder", "test");
    manifest.capabilities.push("policy_hook".to_string());
    let registry = ToolRegistry::new(None);
    registry
        .register_extension(ScopedExtension::new(manifest).with_policy_hook(Arc::new(hook)))
        .expect("mounting the recording hook must succeed");
    registry
}

#[tokio::test]
async fn rpc_hook_receives_the_real_task_scope_and_candidate_budget() {
    use kroma_core::provider::{ModelProvider, ProviderCompletion, ProviderError, ProviderRequest};
    use kroma_core::surface::{JsonRpcRequest, KromaRuntimeConfig, RpcDispatcher};
    use std::future::Future;
    use std::pin::Pin;

    struct NeverProvider;
    impl ModelProvider for NeverProvider {
        fn complete<'a>(
            &'a self,
            _req: ProviderRequest,
            _budget: &'a BudgetConfig,
            _cancel_rx: tokio::sync::watch::Receiver<bool>,
        ) -> Pin<Box<dyn Future<Output = Result<ProviderCompletion, ProviderError>> + Send + 'a>>
        {
            Box::pin(async move { Err(ProviderError::IncompleteStream("unused".to_string())) })
        }
    }

    let root = std::env::temp_dir().join(format!("kroma-t02-hook-{}", uuid::Uuid::new_v4()));
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join("lib.rs"), "pub fn add() {}\n").unwrap();

    let recorder = RecordingHook::default();
    let mut cfg = KromaRuntimeConfig::new(&ws, root.join("state"), PolicyMode::FullAutonomous);
    cfg.extensions = vec![ScopedExtension::new({
        let mut manifest = ExtensionManifest::operator_trusted("t02-recorder", "test");
        manifest.capabilities.push("policy_hook".to_string());
        manifest
    })
    .with_policy_hook(Arc::new(recorder.clone()))];
    cfg.tournament_protected_paths = vec!["oracle/**".to_string()];
    let dispatcher = RpcDispatcher::new(cfg, Arc::new(NeverProvider)).unwrap();

    let resp = dispatcher
        .handle_request(JsonRpcRequest::new(
            1,
            "session.tournament",
            serde_json::json!({
                "task": "fix add() in lib.rs",
                "candidates": 3,
                "strategies": ["only strategy A"],
                "allowed_paths": ["src/**"],
                "max_turns": 5,
                "timeout_ms": 12345,
                "protected_paths": ["tests/**"],
                "disallow_test_tampering": true,
            }),
        ))
        .await;
    // The candidates cannot run (the provider refuses), but the hook has already been consulted.
    let _ = resp;

    let seen = recorder.seen.lock().unwrap().clone();
    assert!(
        !seen.is_empty(),
        "the policy hook must be consulted before any candidate worktree is materialized"
    );
    let canonical = seen
        .iter()
        .find_map(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
        .expect("the hook must receive JSON");
    assert_eq!(canonical["task"], "fix add() in lib.rs");
    assert_eq!(canonical["candidates"], 3);
    // Strategies are expanded: the caller's one plus the defaults for the remaining candidates.
    let strategies = canonical["strategies"]
        .as_array()
        .expect("strategies array");
    assert_eq!(
        strategies.len(),
        3,
        "every candidate strategy must be approved"
    );
    assert_eq!(strategies[0], "only strategy A");
    assert_eq!(canonical["max_turns"], 5);
    assert_eq!(canonical["timeout_ms"], 12345);
    // The host-owned protection set is merged into what the hook approves.
    let protected: Vec<String> = canonical["protected_paths"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert!(protected.contains(&"oracle/**".to_string()));
    assert!(protected.contains(&"tests/**".to_string()));
    assert_eq!(canonical["disallow_test_tampering"], true);
    assert_eq!(canonical["require_verified"], true);
    assert_eq!(canonical["verification_policy_source"], "host_strict");
    assert!(
        canonical["budget"]["max_model_turns"].is_number(),
        "the effective budget must be part of the approved intent"
    );
    assert!(
        canonical.get("role").is_none(),
        "the intent must not be a fixed role=worker placeholder"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// Malformed optional parameters are rejected before any approval prompt is raised.
#[tokio::test]
async fn malformed_rpc_fields_fail_before_approval() {
    use kroma_core::provider::{ModelProvider, ProviderCompletion, ProviderError, ProviderRequest};
    use kroma_core::surface::{JsonRpcRequest, KromaRuntimeConfig, RpcDispatcher};
    use std::future::Future;
    use std::pin::Pin;

    struct NeverProvider;
    impl ModelProvider for NeverProvider {
        fn complete<'a>(
            &'a self,
            _req: ProviderRequest,
            _budget: &'a BudgetConfig,
            _cancel_rx: tokio::sync::watch::Receiver<bool>,
        ) -> Pin<Box<dyn Future<Output = Result<ProviderCompletion, ProviderError>> + Send + 'a>>
        {
            Box::pin(async move { Err(ProviderError::IncompleteStream("unused".to_string())) })
        }
    }

    let root = std::env::temp_dir().join(format!("kroma-t02-malformed-{}", uuid::Uuid::new_v4()));
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join("lib.rs"), "pub fn add() {}\n").unwrap();

    let cfg = KromaRuntimeConfig::new(&ws, root.join("state"), PolicyMode::FullAutonomous);
    let dispatcher = RpcDispatcher::new(cfg, Arc::new(NeverProvider)).unwrap();

    for (field, params) in [
        (
            "candidates as string",
            serde_json::json!({ "task": "fix", "candidates": "3" }),
        ),
        (
            "max_turns as string",
            serde_json::json!({ "task": "fix", "candidates": 2, "max_turns": "5" }),
        ),
        (
            "timeout_ms as string",
            serde_json::json!({ "task": "fix", "candidates": 2, "timeout_ms": "10" }),
        ),
        (
            "protected_paths as string",
            serde_json::json!({ "task": "fix", "candidates": 2, "protected_paths": "tests/**" }),
        ),
        (
            "strategies with a non-string",
            serde_json::json!({ "task": "fix", "candidates": 2, "strategies": ["ok", 7] }),
        ),
    ] {
        let resp = dispatcher
            .handle_request(JsonRpcRequest::new(1, "session.tournament", params))
            .await;
        let err = resp
            .error
            .unwrap_or_else(|| panic!("{field}: a mistyped parameter must be rejected"));
        assert!(
            err.message.contains("must be") || err.message.contains("invalid"),
            "{field}: expected a validation error, got: {}",
            err.message
        );
    }
    let _ = std::fs::remove_dir_all(&root);
}

/// The tool entry point consults the same hook, on the same canonical intent.
#[tokio::test]
async fn tool_hook_sees_the_canonical_intent_not_the_raw_model_arguments() {
    use kroma_core::agent_loop::{AgentRunConfig, AgentRunner};
    use kroma_core::events::EventLogger;
    use kroma_core::messages::{AssistantTurn, FinishReason, ToolCall};
    use kroma_core::provider::FakeProvider;
    use kroma_core::revision::SnapshotStore;
    use kroma_core::session::SessionJournal;
    use kroma_core::tool::policy::NonInteractiveApprovalHandler;
    use kroma_core::types::ToolCallId;

    let root = std::env::temp_dir().join(format!("kroma-t02-tool-{}", uuid::Uuid::new_v4()));
    let ws = root.join("ws");
    let state = root.join("state");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(ws.join("lib.rs"), "pub fn add() {}\n").unwrap();

    let recorder = RecordingHook::default();
    let registry = registry_with(recorder.clone());
    let env = Arc::new(kroma_core::env::LocalHostEnv::new(&ws, state.clone()).unwrap());
    let policy = PolicyEngine::new(
        PolicyMode::FullAutonomous,
        Arc::new(NonInteractiveApprovalHandler),
    );
    let budget = BudgetConfig::default();
    let runtime =
        kroma_core::agent_loop::build_runtime_with_registry(registry, env, policy, budget.clone());

    // The model only says `mode: tournament`; it never mentions strategies or a budget.
    let provider = Arc::new(FakeProvider::new(vec![
        AssistantTurn {
            content: None,
            reasoning_blocks: Vec::new(),
            tool_calls: vec![ToolCall {
                call_id: ToolCallId::new("call_race"),
                name: "spawn_subagent".to_string(),
                raw_arguments: serde_json::json!({
                    "mode": "tournament",
                    "task": "fix add()",
                    "candidates": 2,
                })
                .to_string(),
            }],
            finish_reason: FinishReason::ToolCalls,
            usage: None,
        },
        AssistantTurn {
            content: Some("done".to_string()),
            reasoning_blocks: Vec::new(),
            tool_calls: Vec::new(),
            finish_reason: FinishReason::Stop,
            usage: None,
        },
    ]));

    let journal = SessionJournal::create(state.join("sessions"), &ws).unwrap();
    let store = Arc::new(SnapshotStore::new(root.join("snapshots")));
    let cfg = AgentRunConfig::new("run a race", budget);
    let logger = EventLogger::new(cfg.run_id, Some(state.clone()), true).unwrap();
    let (_tx, rx) = tokio::sync::watch::channel(false);
    let _ = AgentRunner::new(cfg, provider, runtime, rx)
        .with_session_journal(journal)
        .with_snapshot_store(store)
        .run(logger)
        .await;

    let seen = recorder.seen.lock().unwrap().clone();
    let canonical = seen
        .iter()
        .filter_map(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
        .find(|value| value.get("strategies").is_some())
        .unwrap_or_else(|| panic!("the hook never saw the canonical intent; saw: {seen:?}"));
    assert_eq!(canonical["task"], "fix add()");
    assert_eq!(canonical["candidates"], 2);
    assert_eq!(
        canonical["strategies"].as_array().unwrap().len(),
        2,
        "the model never named a strategy; the hook must still approve the real ones"
    );
    assert_eq!(canonical["require_verified"], true);
    assert!(
        canonical["budget"]["max_model_turns"].is_number(),
        "the effective budget must be visible to the hook"
    );

    let _ = std::fs::remove_dir_all(&root);
}
