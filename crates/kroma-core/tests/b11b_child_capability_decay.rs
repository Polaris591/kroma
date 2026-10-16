//! B11b: a child subagent may only *decay* the parent's base-policy authority.
//!
//! The base modes form a partial order, not a scalar ladder: `AllowWrite` auto-authorizes
//! `{read-only, mutating}` and `AllowShell` auto-authorizes `{read-only, side-effect}` — neither
//! contains the other. A child run must never auto-authorize more than its parent along either
//! dimension, so a `worker` (which auto-writes in an isolated worktree that is later merged into
//! the parent workspace) is refused unless the parent mode itself auto-authorizes mutation.
//!
//! The deterministic red for this card is `worker_is_refused_when_the_parent_only_asks`: with the
//! pre-fix mapping (`allows_shell_auto() ? FullAutonomous : AllowWrite`) an `Ask` or `AllowShell`
//! parent silently promotes the worker to automatic writes it never authorized.

use kroma_core::budget::BudgetConfig;
use kroma_core::async_tasks::{AsyncTaskRecord, AsyncTaskRegistry, NewTask, TaskState};
use kroma_core::extensions::{
    HookContext, HookError, HookEvent, ObserverHook, PolicyHook, PolicyHookDecision,
    ScopedExtension, EXTENSION_API_VERSION,
};
use kroma_core::messages::{AssistantTurn, FinishReason, TokenUsage};
use kroma_core::provider::{ModelProvider, ProviderCompletion, ProviderError, ProviderRequest};
use kroma_core::subagents::{
    ParentPolicyHooks, ProviderSubagentBackend, SubagentBackend, SubagentExecutionRequest,
    SubagentRole, SubagentRunOutcome, SubagentSpec,
};
use kroma_core::tool::policy::PolicyMode;
use kroma_core::tool::registry::ToolRegistry;
use kroma_core::workspace::WorkspaceCap;
use std::fs;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::watch;
use uuid::Uuid;

const PARENT_MODES: [PolicyMode; 5] = [
    PolicyMode::ReadOnly,
    PolicyMode::Ask,
    PolicyMode::AllowWrite,
    PolicyMode::AllowShell,
    PolicyMode::FullAutonomous,
];

fn temp_root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("kroma-b11b-{}-{}", tag, Uuid::new_v4()));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn baseline_workspace(root: &Path) -> (PathBuf, WorkspaceCap) {
    let ws = root.join("ws");
    fs::create_dir_all(&ws).unwrap();
    fs::write(
        ws.join("lib.rs"),
        "pub fn add(a: i32, b: i32) -> i32 { a + b }\n",
    )
    .unwrap();
    let workspace = WorkspaceCap::new(&ws).unwrap();
    (ws, workspace)
}

/// A provider that immediately stops, so a delegation that is allowed to proceed ends promptly
/// without touching the workspace.
struct StopProvider;

impl ModelProvider for StopProvider {
    fn complete<'a>(
        &'a self,
        _req: ProviderRequest,
        _budget: &'a BudgetConfig,
        _cancel_rx: watch::Receiver<bool>,
    ) -> Pin<Box<dyn Future<Output = Result<ProviderCompletion, ProviderError>> + Send + 'a>> {
        Box::pin(async move {
            Ok(ProviderCompletion {
                turn: AssistantTurn {
                    content: Some("done".to_string()),
                    tool_calls: Vec::new(),
                    reasoning_blocks: Vec::new(),
                    finish_reason: FinishReason::Stop,
                    usage: Some(TokenUsage {
                        input_tokens: 10,
                        cached_input_tokens: None,
                        output_tokens: 2,
                        reasoning_tokens: None,
                    }),
                },
                request_body_bytes: 0,
                latency_ms: 0,
                ttft_ms: None,
                attempts_used: 1,
                reported_model: None,
            })
        })
    }
}

fn request(
    root: &Path,
    spec: SubagentSpec,
    parent_policy_mode: PolicyMode,
    workspace: WorkspaceCap,
) -> SubagentExecutionRequest {
    request_with_hooks(
        root,
        spec,
        parent_policy_mode,
        workspace,
        ParentPolicyHooks::KnownEmpty,
    )
}

fn request_with_hooks(
    root: &Path,
    spec: SubagentSpec,
    parent_policy_mode: PolicyMode,
    workspace: WorkspaceCap,
    parent_policy_hooks: ParentPolicyHooks,
) -> SubagentExecutionRequest {
    SubagentExecutionRequest {
        subagent_id: "sub-0001".to_string(),
        spec,
        workspace,
        state_dir: root.join("state"),
        workspace_skills: Vec::new(),
        parent_policy_mode,
        parent_policy_hooks,
        parent_budget: BudgetConfig::default(),
        token_budget: None,
        predictive_check: false,
        cancel_rx: watch::channel(false).1,
    }
}

async fn run(req: SubagentExecutionRequest) -> Result<SubagentRunOutcome, String> {
    let backend = ProviderSubagentBackend::new(Arc::new(StopProvider));
    backend.run_subagent(req).await
}

/// A `worker` under a parent whose mode does not auto-authorize mutation must be refused outright:
/// the operator never authorized automatic writes, and one `spawn_subagent` approval must not be
/// upgraded into a blanket write grant.
#[tokio::test]
async fn worker_is_refused_when_the_parent_only_asks() {
    for parent in [PolicyMode::Ask, PolicyMode::AllowShell] {
        let root = temp_root("ask");
        let (_ws, workspace) = baseline_workspace(&root);
        let err = run(request(
            &root,
            SubagentSpec::worker("write something"),
            parent,
            workspace,
        ))
        .await
        .expect_err("a parent that only asks for writes must not spawn an auto-writing worker");
        assert!(
            err.contains("worker") && err.contains(parent.as_str()),
            "the refusal must name the role and the parent mode, got: {err}"
        );
        assert!(
            !root.join("state").exists(),
            "the role-capability refusal must happen before the state dir is created"
        );
        let _ = fs::remove_dir_all(&root);
    }
}

/// A `worker` under a parent that auto-authorizes mutation runs with the parent's mutating
/// authority, never more: `AllowWrite` must not leak the parent's absent shell authority.
#[tokio::test]
async fn worker_inherits_writes_but_not_shell_from_allow_write() {
    let root = temp_root("allow-write");
    let (_ws, workspace) = baseline_workspace(&root);
    let outcome = run(request(
        &root,
        SubagentSpec::worker("write something"),
        PolicyMode::AllowWrite,
        workspace,
    ))
    .await
    .expect("an AllowWrite parent may spawn a worker");
    assert!(
        outcome.visible_tools.iter().any(|t| t == "write"),
        "the worker must see write, got {:?}",
        outcome.visible_tools
    );
    assert!(
        !outcome.visible_tools.iter().any(|t| t == "bash"),
        "AllowWrite must not auto-authorize shell in the child, got {:?}",
        outcome.visible_tools
    );
    let _ = fs::remove_dir_all(&root);
}

/// A `scout` is strictly read-only regardless of how permissive the parent is.
#[tokio::test]
async fn scout_stays_read_only_under_a_full_autonomous_parent() {
    let root = temp_root("scout");
    let (_ws, workspace) = baseline_workspace(&root);
    let outcome = run(request(
        &root,
        SubagentSpec::scout("explore"),
        PolicyMode::FullAutonomous,
        workspace,
    ))
    .await
    .expect("a scout is allowed under any parent");
    for forbidden in ["write", "edit", "bash", "spawn_subagent"] {
        assert!(
            !outcome.visible_tools.iter().any(|t| t == forbidden),
            "a scout must never see {forbidden}, got {:?}",
            outcome.visible_tools
        );
    }
    let _ = fs::remove_dir_all(&root);
}

/// The legacy `ReadOnly` parent -> writing worker escalation stays closed.
#[tokio::test]
async fn worker_is_refused_under_a_read_only_parent() {
    let root = temp_root("read-only");
    let (_ws, workspace) = baseline_workspace(&root);
    let err = run(request(
        &root,
        SubagentSpec::worker("write something"),
        PolicyMode::ReadOnly,
        workspace,
    ))
    .await
    .expect_err("a read-only parent must never spawn a writing worker");
    assert!(err.contains("read-only"), "got: {err}");
    let _ = fs::remove_dir_all(&root);
}

/// The partial order encoded by the decay rule: for every parent mode and role, the child's
/// automatic authority is a subset of the parent's, dimension by dimension. A scalar enum compare
/// would wrongly order `AllowWrite` and `AllowShell`.
#[test]
fn child_automatic_authority_never_exceeds_the_parent() {
    for parent in PARENT_MODES {
        for role in [SubagentRole::Scout, SubagentRole::Worker] {
            let child = kroma_core::subagents::child_policy_mode_for(parent, role);
            let Ok(child) = child else {
                continue;
            };
            assert!(
                !child.auto_mutating() || parent.auto_mutating(),
                "{parent:?}/{role:?}: child gained mutating authority"
            );
            assert!(
                !child.auto_side_effect() || parent.auto_side_effect(),
                "{parent:?}/{role:?}: child gained side-effect authority"
            );
        }
    }
}

// ---- E2: the child cannot inherit (and must not silently drop) parent policy hooks ------------

struct DenyWritePolicyHook;

impl PolicyHook for DenyWritePolicyHook {
    fn id(&self) -> &str {
        "b11b-deny-src-writes"
    }
    fn events(&self) -> &'static [HookEvent] {
        &[HookEvent::PreToolUse]
    }
    fn evaluate(&self, _event: HookEvent, _ctx: &HookContext) -> Result<PolicyHookDecision, HookError> {
        Ok(PolicyHookDecision::Deny {
            reason: "writes under src/** are denied".to_string(),
        })
    }
}

struct NoopObserverHook;

impl ObserverHook for NoopObserverHook {
    fn id(&self) -> &str {
        "b11b-observer"
    }
    fn events(&self) -> &'static [HookEvent] {
        &[HookEvent::PostToolUse]
    }
    fn observe(&self, _event: HookEvent, _ctx: &HookContext) -> Result<(), HookError> {
        Ok(())
    }
}

fn registry_with(extension: ScopedExtension, name: &str) -> ToolRegistry {
    let registry = ToolRegistry::new(None);
    registry.register_extension(extension).unwrap_or_else(|e| {
        panic!("registering the {name} extension must succeed: {e:?}")
    });
    registry
}

fn manifest_with_policy_hook(name: &str) -> kroma_core::extensions::ExtensionManifest {
    let mut manifest = kroma_core::extensions::ExtensionManifest::operator_trusted(name, "test");
    manifest.capabilities.push("policy_hook".to_string());
    manifest.capabilities.push("observer_hook".to_string());
    manifest.api_version = EXTENSION_API_VERSION.to_string();
    manifest
}

/// A real registry with a **policy** hook is `Required` (the child cannot execute it), while an
/// observer-only registry stays `KnownEmpty` — observer hooks are not policy gates and must not
/// falsely refuse a delegation.
#[test]
fn from_registry_distinguishes_policy_hooks_from_observer_hooks() {
    let policy = registry_with(
        ScopedExtension::new(manifest_with_policy_hook("deny"))
            .with_policy_hook(Arc::new(DenyWritePolicyHook)),
        "policy",
    );
    assert_eq!(
        ParentPolicyHooks::from_registry(&policy),
        ParentPolicyHooks::Required {
            hook_ids: vec!["b11b-deny-src-writes".to_string()]
        }
    );

    let observer = registry_with(
        ScopedExtension::new(manifest_with_policy_hook("observe"))
            .with_observer_hook(Arc::new(NoopObserverHook)),
        "observer",
    );
    assert_eq!(
        ParentPolicyHooks::from_registry(&observer),
        ParentPolicyHooks::KnownEmpty,
        "an observer-only registry must not refuse the delegation"
    );
}

/// A parent with a fail-closed policy hook refuses the delegation for both roles, before any state
/// directory is created.
#[tokio::test]
async fn a_parent_policy_hook_refuses_worker_and_scout_without_side_effects() {
    for spec in [SubagentSpec::worker("write"), SubagentSpec::scout("explore")] {
        let root = temp_root("hook");
        let (_ws, workspace) = baseline_workspace(&root);
        let hooks = ParentPolicyHooks::Required {
            hook_ids: vec!["b11b-deny-src-writes".to_string()],
        };
        let err = run(request_with_hooks(
            &root,
            spec,
            PolicyMode::FullAutonomous,
            workspace,
            hooks,
        ))
        .await
        .expect_err("a parent policy hook the child cannot execute must refuse the delegation");
        assert!(err.contains("unsupported delegation"), "got: {err}");
        assert!(
            !root.join("state").exists(),
            "the refusal must happen before any state dir is created"
        );
        let _ = fs::remove_dir_all(&root);
    }
}

/// The `Required` constraint survives a serialization round trip (as it does through the durable
/// record) and still refuses — it is never silently dropped back to an empty set.
#[tokio::test]
async fn required_hooks_round_trip_and_still_refuse() {
    let original = ParentPolicyHooks::Required {
        hook_ids: vec!["a".to_string(), "b".to_string()],
    };
    let encoded = serde_json::to_string(&original).unwrap();
    let decoded: ParentPolicyHooks = serde_json::from_str(&encoded).unwrap();
    assert_eq!(decoded, original);

    let root = temp_root("round-trip");
    let (_ws, workspace) = baseline_workspace(&root);
    let err = run(request_with_hooks(
        &root,
        SubagentSpec::worker("write"),
        PolicyMode::FullAutonomous,
        workspace,
        decoded,
    ))
    .await
    .expect_err("a round-tripped Required constraint must still refuse");
    assert!(err.contains("unsupported delegation"), "got: {err}");
    let _ = fs::remove_dir_all(&root);
}

/// `Unknown` (a legacy record with no recorded provenance) is not the same as an empty set: it
/// refuses the delegation rather than re-running the child without the parent's constraints.
#[tokio::test]
async fn unknown_provenance_refuses_the_delegation() {
    let root = temp_root("unknown");
    let (_ws, workspace) = baseline_workspace(&root);
    let err = run(request_with_hooks(
        &root,
        SubagentSpec::scout("explore"),
        PolicyMode::FullAutonomous,
        workspace,
        ParentPolicyHooks::Unknown,
    ))
    .await
    .expect_err("unknown provenance must refuse rather than assume no constraints");
    assert!(err.contains("unsupported delegation"), "got: {err}");
    let _ = fs::remove_dir_all(&root);
}

/// A durable record written before the field existed deserializes to `Unknown`, never to
/// `KnownEmpty`, so a legacy resume is refused instead of silently dropping the constraints.
#[test]
fn a_legacy_record_without_the_field_is_unknown() {
    let mut registry = AsyncTaskRegistry::in_memory();
    let record = registry
        .register(NewTask {
            task_id: "legacy-1".to_string(),
            session_id: "s1".to_string(),
            owner_id: "rt1".to_string(),
            workspace_root: "/ws".to_string(),
            role: "worker".to_string(),
            created_turn: 0,
            token_allocation: 100,
            prompt: "do the thing".to_string(),
            timeout_ms: 0,
            spec_json: "{}".to_string(),
            parent_policy: "allow-write".to_string(),
            parent_policy_hooks: ParentPolicyHooks::KnownEmpty,
            session_path: String::new(),
        })
        .unwrap();

    // Strip the field to emulate a record written before it existed.
    let mut value = serde_json::to_value(&record).unwrap();
    value
        .as_object_mut()
        .unwrap()
        .remove("parent_policy_hooks")
        .expect("the serialized record must carry the field");
    let legacy: AsyncTaskRecord = serde_json::from_value(value).unwrap();
    assert_eq!(
        legacy.parent_policy_hooks,
        ParentPolicyHooks::Unknown,
        "a legacy record must not deserialize to a no-constraints state"
    );
    assert_eq!(legacy.state, TaskState::Queued);
}

// ---- B11b F1: the tool caller checks before it materializes a worktree --------------------------

struct AllowAllPolicyHook;

impl PolicyHook for AllowAllPolicyHook {
    fn id(&self) -> &str {
        "b11b-allow-all"
    }
    fn events(&self) -> &'static [HookEvent] {
        &[HookEvent::PreToolUse]
    }
    fn evaluate(&self, _event: HookEvent, _ctx: &HookContext) -> Result<PolicyHookDecision, HookError> {
        Ok(PolicyHookDecision::Allow)
    }
}

struct SpawnThenStopProvider {
    turns: std::sync::atomic::AtomicUsize,
}

impl ModelProvider for SpawnThenStopProvider {
    fn complete<'a>(
        &'a self,
        _req: ProviderRequest,
        _budget: &'a BudgetConfig,
        _cancel_rx: watch::Receiver<bool>,
    ) -> Pin<Box<dyn Future<Output = Result<ProviderCompletion, ProviderError>> + Send + 'a>> {
        Box::pin(async move {
            let n = self
                .turns
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let turn = if n == 0 {
                AssistantTurn {
                    content: None,
                    tool_calls: vec![kroma_core::messages::ToolCall {
                        call_id: kroma_core::types::ToolCallId::new("call-spawn"),
                        name: "spawn_subagent".to_string(),
                        raw_arguments:
                            serde_json::json!({"role": "worker", "task": "write something"})
                                .to_string(),
                    }],
                    reasoning_blocks: Vec::new(),
                    finish_reason: FinishReason::ToolCalls,
                    usage: Some(TokenUsage {
                        input_tokens: 10,
                        cached_input_tokens: None,
                        output_tokens: 2,
                        reasoning_tokens: None,
                    }),
                }
            } else {
                AssistantTurn {
                    content: Some("done".to_string()),
                    tool_calls: Vec::new(),
                    reasoning_blocks: Vec::new(),
                    finish_reason: FinishReason::Stop,
                    usage: Some(TokenUsage {
                        input_tokens: 10,
                        cached_input_tokens: None,
                        output_tokens: 2,
                        reasoning_tokens: None,
                    }),
                }
            };
            Ok(ProviderCompletion {
                turn,
                request_body_bytes: 0,
                latency_ms: 0,
                ttft_ms: None,
                attempts_used: 1,
                reported_model: None,
            })
        })
    }
}

/// B11b F1: the real `spawn_subagent` caller refuses a parent whose policy hooks the child cannot
/// execute *before* it materializes a worker worktree or creates a child state dir.
#[tokio::test]
async fn agent_loop_tool_caller_refuses_before_materializing_a_worktree() {
    use kroma_core::agent_loop::{build_runtime_with_registry, AgentRunConfig, AgentRunner};
    use kroma_core::env::LocalHostEnv;
    use kroma_core::events::EventLogger;
    use kroma_core::messages::TranscriptItem;
    use kroma_core::tool::policy::{NonInteractiveApprovalHandler, PolicyEngine};

    let root = temp_root("caller-hook");
    let ws = root.join("ws");
    let state = root.join("state");
    fs::create_dir_all(&ws).unwrap();
    fs::write(ws.join("a.txt"), "hello").unwrap();

    let registry = registry_with(
        ScopedExtension::new(manifest_with_policy_hook("allow-all"))
            .with_policy_hook(Arc::new(AllowAllPolicyHook)),
        "policy",
    );
    let env = Arc::new(LocalHostEnv::new(&ws, &state).unwrap());
    let runtime = build_runtime_with_registry(
        registry,
        env,
        PolicyEngine::new(
            PolicyMode::AllowWrite,
            Arc::new(NonInteractiveApprovalHandler),
        ),
        BudgetConfig::default(),
    );
    let (_tx, rx) = watch::channel(false);
    let cfg = AgentRunConfig::new("B11b caller preflight", BudgetConfig::default());
    let logger = EventLogger::new(cfg.run_id, Some(state.clone()), false).unwrap();
    let provider = Arc::new(SpawnThenStopProvider {
        turns: std::sync::atomic::AtomicUsize::new(0),
    });
    let (summary, _) = AgentRunner::new(cfg, provider, runtime, rx).run(logger).await;

    let spawn = summary
        .transcript
        .iter()
        .find_map(|i| match i {
            TranscriptItem::ToolResult(tr) if tr.tool_name == "spawn_subagent" => Some(tr),
            _ => None,
        })
        .expect("the spawn_subagent call must be recorded");
    assert!(!spawn.outcome.is_success());
    assert!(
        spawn.content.contains("unsupported delegation"),
        "got: {}",
        spawn.content
    );
    assert!(
        !state.join("worktrees").exists(),
        "the refused delegation must not materialize a worktree"
    );
    assert!(!ws.join("out.txt").exists());
    let _ = fs::remove_dir_all(&root);
}
