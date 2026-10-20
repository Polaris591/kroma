//! R01 — the parent's trusted execution boundary reaches every child path.
//!
//! Two seams are pinned here, both without needing a working sandbox backend:
//!
//! * a **tournament** candidate request carries the parent's policy (the tournament is not a way to
//!   escape an isolated run), and
//! * `ProviderSubagentBackend` **installs** that policy on the child runtime, proved by a
//!   deliberately unusable backend: the child's `bash` must be *refused* (fail-closed) instead of
//!   running on the host. On the original code the child runtime defaulted to
//!   `ExecutionPolicy::host()`, so the same command ran and wrote the marker.
//!
//! The real `bwrap` end-to-end proof (parent `AgentRunner` → `spawn_subagent` → child `bash`) lives
//! in `t09_execution_boundary::worker_inherits_the_parent_execution_boundary_through_spawn_subagent`.
//! Using an unusable backend here keeps this regression meaningful on a host with no container
//! runtime, where the defect is still observable.

use kroma_core::budget::BudgetConfig;
use kroma_core::env::AllowAllGate;
use kroma_core::messages::{AssistantTurn, FinishReason, TokenUsage, ToolCall, TranscriptItem};
use kroma_core::provider::{ModelProvider, ProviderCompletion, ProviderError, ProviderRequest};
use kroma_core::revision::SnapshotStore;
use kroma_core::sandbox::{ExecutionMode, ExecutionPolicy, SandboxBackend, SandboxTemplate};
use kroma_core::subagents::{
    run_worktree_tournament, ParentPolicyHooks, ProviderSubagentBackend, SubagentBackend,
    SubagentExecutionRequest, SubagentRunOutcome, SubagentSpec, TournamentCandidate,
    TournamentSpec,
};
use kroma_core::tool::policy::PolicyMode;
use kroma_core::types::{RunStatus, TerminalReason, ToolCallId};
use kroma_core::workspace::WorkspaceCap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use tokio::sync::watch;

fn scratch(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("kroma-t32-{}-{}", tag, uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    root
}

/// A policy whose backend cannot exist: the only correct behaviour is a fail-closed refusal, which
/// makes "did the child inherit the policy?" observable without any real sandbox.
fn unusable_isolated_policy() -> ExecutionPolicy {
    ExecutionPolicy::isolated(SandboxTemplate::new(SandboxBackend::Container {
        runtime: "kroma-no-such-runtime".to_string(),
        image: "kroma-no-such-image".to_string(),
    }))
}

fn assert_same_policy(actual: &ExecutionPolicy, expected: &ExecutionPolicy, context: &str) {
    assert_eq!(
        actual.mode(),
        expected.mode(),
        "{context}: the child must run under the parent's execution mode"
    );
    assert!(
        matches!(actual.mode(), ExecutionMode::Isolated(_)),
        "{context}: the parent declared isolation"
    );
}

fn usage() -> TokenUsage {
    TokenUsage {
        input_tokens: 10,
        cached_input_tokens: Some(0),
        cache_creation_input_tokens: None,
        output_tokens: 5,
        reasoning_tokens: None,
    }
}

struct ScriptProvider<F>(F);

impl<F> ModelProvider for ScriptProvider<F>
where
    F: Fn(ProviderRequest) -> Result<AssistantTurn, ProviderError> + Send + Sync + 'static,
{
    fn complete<'a>(
        &'a self,
        req: ProviderRequest,
        _budget: &'a BudgetConfig,
        _cancel_rx: watch::Receiver<bool>,
    ) -> Pin<Box<dyn Future<Output = Result<ProviderCompletion, ProviderError>> + Send + 'a>> {
        let turn = (self.0)(req);
        Box::pin(async move {
            Ok(ProviderCompletion {
                turn: turn?,
                request_body_bytes: 64,
                latency_ms: 1,
                ttft_ms: Some(1),
                attempts_used: 1,
                reported_model: None,
            })
        })
    }
}

/// R01: the backend installs the request's policy on the child runtime instead of leaving the
/// `ToolRuntime::new` default (host). The child's `bash` must be refused by the unusable backend and
/// must not create the outside marker.
#[tokio::test]
async fn provider_subagent_backend_installs_the_request_execution_policy() {
    let root = scratch("backend-policy");
    let ws = root.join("ws");
    let state = root.join("state");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    let marker = root.join("outside-marker.txt");
    let marker_for_child = marker.clone();

    let child_outcomes: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let child_outcomes_c = Arc::clone(&child_outcomes);
    let turns = Arc::new(Mutex::new(0usize));
    let provider = Arc::new(ScriptProvider(move |req: ProviderRequest| {
        // Only the child runs the command; the parent is not part of this request.
        let mut turn = turns.lock().unwrap();
        if *turn == 0 {
            *turn += 1;
            Ok(AssistantTurn {
                content: None,
                tool_calls: vec![ToolCall {
                    call_id: ToolCallId::new("child_bash"),
                    name: "bash".to_string(),
                    raw_arguments: serde_json::json!({
                        "command": format!("printf pwned > {}", marker_for_child.display())
                    })
                    .to_string(),
                }],
                reasoning_blocks: Vec::new(),
                finish_reason: FinishReason::ToolCalls,
                usage: Some(usage()),
            })
        } else {
            for item in &req.messages {
                if let TranscriptItem::ToolResult(result) = item {
                    if result.tool_name == "bash" {
                        child_outcomes_c.lock().unwrap().push(format!(
                            "{:?}|{}",
                            result.outcome,
                            result.meta.execution_backend.clone().unwrap_or_default()
                        ));
                    }
                }
            }
            Ok(AssistantTurn {
                content: Some("child finished".to_string()),
                tool_calls: Vec::new(),
                reasoning_blocks: Vec::new(),
                finish_reason: FinishReason::Stop,
                usage: Some(usage()),
            })
        }
    }));

    let policy = unusable_isolated_policy();
    let backend = ProviderSubagentBackend::new(provider);
    let (cancel_tx, cancel_rx) = watch::channel(false);
    let request = SubagentExecutionRequest {
        subagent_id: "sub-0001".to_string(),
        spec: SubagentSpec::worker("run a command"),
        workspace: WorkspaceCap::new(&ws).unwrap(),
        state_dir: state.join("subagents"),
        workspace_skills: Vec::new(),
        parent_policy_mode: PolicyMode::FullAutonomous,
        parent_policy_hooks: ParentPolicyHooks::KnownEmpty,
        parent_budget: BudgetConfig::default(),
        token_budget: None,
        predictive_check: false,
        execution_policy: policy.clone(),
        cancel_rx,
    };
    let outcome = backend
        .run_subagent(request)
        .await
        .expect("the child run itself must not fail");
    let _ = cancel_tx.send(true);

    assert!(
        !marker.exists(),
        "the worker's bash ran on the host even though the request declared an isolated policy"
    );
    let observed = child_outcomes.lock().unwrap().clone();
    let _ = std::fs::remove_dir_all(&root);
    assert_eq!(
        observed.len(),
        1,
        "the child's bash result must be recorded: {observed:?}"
    );
    assert!(
        observed[0].contains("SandboxUnavailable"),
        "the child's bash must be refused by the declared backend, got {observed:?}"
    );
    assert!(
        observed[0].contains("none:refused"),
        "a refused spawn must report that no backend ran the command: {observed:?}"
    );
    assert_eq!(outcome.status, RunStatus::Completed);
}

// ---------------------------------------------------------------------------------------------
// Tournament propagation
// ---------------------------------------------------------------------------------------------

struct RecordingBackend {
    seen: Mutex<Vec<ExecutionPolicy>>,
}

impl SubagentBackend for RecordingBackend {
    fn run_subagent(
        &self,
        req: SubagentExecutionRequest,
    ) -> Pin<Box<dyn Future<Output = Result<SubagentRunOutcome, String>> + Send + '_>> {
        self.seen.lock().unwrap().push(req.execution_policy.clone());
        Box::pin(async {
            Ok(SubagentRunOutcome {
                status: RunStatus::Completed,
                terminal_reason: TerminalReason::ModelStop,
                summary: "scripted candidate completed".to_string(),
                relevant_files: Vec::new(),
                findings: Vec::new(),
                turns_used: 1,
                tool_calls_used: 0,
                tokens_used: 10,
                input_tokens: 5,
                cached_input_tokens: None,
                usage_basis: "provider_reported".to_string(),
                output_tokens: 5,
                duration_ms: 1,
                visible_tools: Vec::new(),
                request_attempts_used: 0,
                request_usage_complete: false,
                request_usage: Vec::new(),
            })
        })
    }
}

fn baseline_workspace(root: &Path) -> (WorkspaceCap, SnapshotStore) {
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(
        ws.join("lib.rs"),
        "pub fn add(a: i32, b: i32) -> i32 { a + b }\n",
    )
    .unwrap();
    (
        WorkspaceCap::new(&ws).unwrap(),
        SnapshotStore::new(root.join("store")),
    )
}

/// R01: a tournament is a delegation like any other. Every candidate request must carry the parent's
/// execution policy rather than silently defaulting to host.
#[tokio::test]
async fn tournament_candidates_receive_the_parent_execution_policy() {
    let root = scratch("tournament-policy");
    let (workspace, store) = baseline_workspace(&root);
    let policy = unusable_isolated_policy();
    let backend = Arc::new(RecordingBackend {
        seen: Mutex::new(Vec::new()),
    });

    let spec = TournamentSpec {
        candidates: vec![
            TournamentCandidate::worker("c1", "fix it"),
            TournamentCandidate::worker("c2", "fix it"),
        ],
        worktree_base: root.join("worktrees"),
        state_base: root.join("subagents"),
        allowed_write_globs: None,
        budget: BudgetConfig::default(),
        token_budget: None,
        policy_mode: PolicyMode::FullAutonomous,
        parent_policy_hooks: ParentPolicyHooks::KnownEmpty,
        workspace_skills: Vec::new(),
        predictive_check: false,
        execution_policy: policy.clone(),
        cancel_rx: watch::channel(false).1,
        timeout_ms: 5_000,
        disallow_test_tampering: false,
        protected_paths: Vec::new(),
        require_verified: false,
    };
    let gate: Arc<dyn kroma_core::env::CompletionGate> = Arc::new(AllowAllGate);
    let outcome = run_worktree_tournament(
        spec,
        &workspace,
        &store,
        Arc::clone(&backend) as Arc<dyn SubagentBackend>,
        None,
        &gate,
    )
    .await
    .expect("the tournament must run");

    let seen = backend.seen.lock().unwrap().clone();
    let _ = std::fs::remove_dir_all(&root);
    assert_eq!(seen.len(), 2, "one request per candidate");
    for (index, actual) in seen.iter().enumerate() {
        assert_same_policy(actual, &policy, &format!("candidate {}", index + 1));
    }
    assert_eq!(outcome.scores.len(), 2);
}
