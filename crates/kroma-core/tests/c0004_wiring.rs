//! C00-04: the run path establishes its dispatch gate from a live lease.
//!
//! The defect this pins: `ActivationRegistry`, `CapabilityClaim` and
//! `ToolRuntime::gate_tools` were implemented and tested in isolation while nothing in production
//! ever activated a capability or called `gate_tools`. A revoke therefore withdrew nothing, because
//! no tool was gated in the first place.
//!
//! Every test here goes through a real `AgentRunner` (its `prepare_run` is the run boundary) or
//! through the runtime method that boundary calls, and asserts on the path a call actually takes:
//! the capability is declared on the live registry, the lease's claim is what dispatch is judged
//! against, and a revoke refuses the *next* call before it runs.

use kroma_core::activation::{ActivationRegistry, ActivationState};
use kroma_core::agent_loop::{build_default_runtime, AgentRunConfig, AgentRunner};
use kroma_core::budget::BudgetConfig;
use kroma_core::env::LocalHostEnv;
use kroma_core::events::EventLogger;
use kroma_core::messages::{AssistantTurn, FinishReason, TokenUsage, ToolCall, TranscriptItem};
use kroma_core::provider::{ModelProvider, ProviderCompletion, ProviderError, ProviderRequest};
use kroma_core::tool::policy::{NonInteractiveApprovalHandler, PolicyEngine, PolicyMode};
use kroma_core::tool::runtime::{ToolRuntime, RUN_DISPATCH_CAPABILITY};
use kroma_core::types::{RunStatus, TerminalReason, ToolCallId};
use std::collections::VecDeque;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::watch;

/// Bound for every awaited step: a hang must fail the test, not stall the suite.
const STEP_TIMEOUT: Duration = Duration::from_secs(60);

fn workspace(tag: &str) -> (PathBuf, PathBuf) {
    let root =
        std::env::temp_dir().join(format!("kroma-c0004-wiring-{tag}-{}", uuid::Uuid::new_v4()));
    let ws = root.join("ws");
    let state = root.join("state");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    (ws, state)
}

/// A scripted provider that withdraws the run's dispatch authority before answering a chosen
/// request — i.e. the revoke lands after the run took its lease and before the tool call it is about
/// to return is dispatched.
struct ScriptedProvider {
    turns: Mutex<VecDeque<AssistantTurn>>,
    requests: Arc<Mutex<Vec<ProviderRequest>>>,
    capabilities: Option<Arc<ActivationRegistry>>,
    /// Request index (0-based) at which to revoke; only the first such request revokes.
    revoke_at: usize,
    revoked: AtomicBool,
}

impl ScriptedProvider {
    fn new(turns: Vec<AssistantTurn>) -> Self {
        Self {
            turns: Mutex::new(turns.into()),
            requests: Arc::new(Mutex::new(Vec::new())),
            capabilities: None,
            revoke_at: usize::MAX,
            revoked: AtomicBool::new(false),
        }
    }

    fn revoking(mut self, capabilities: Arc<ActivationRegistry>, at: usize) -> Self {
        self.capabilities = Some(capabilities);
        self.revoke_at = at;
        self
    }

    fn requests(&self) -> Vec<ProviderRequest> {
        self.requests.lock().unwrap().clone()
    }
}

impl ModelProvider for ScriptedProvider {
    fn complete<'a>(
        &'a self,
        req: ProviderRequest,
        _budget: &'a BudgetConfig,
        cancel_rx: watch::Receiver<bool>,
    ) -> Pin<Box<dyn Future<Output = Result<ProviderCompletion, ProviderError>> + Send + 'a>> {
        Box::pin(async move {
            if *cancel_rx.borrow() {
                return Err(ProviderError::Cancelled);
            }
            let index = {
                let mut requests = self.requests.lock().unwrap();
                requests.push(req);
                requests.len() - 1
            };
            if index == self.revoke_at && !self.revoked.swap(true, Ordering::SeqCst) {
                self.capabilities
                    .as_ref()
                    .expect("a revoking provider carries a registry")
                    .revoke(RUN_DISPATCH_CAPABILITY);
            }
            let turn = self
                .turns
                .lock()
                .unwrap()
                .pop_front()
                .expect("one scripted turn per request");
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
        input_tokens: 10,
        cached_input_tokens: Some(0),
        cache_creation_input_tokens: None,
        output_tokens: 5,
        reasoning_tokens: None,
    })
}

fn call(id: &str, name: &str, args: serde_json::Value) -> AssistantTurn {
    AssistantTurn {
        content: None,
        reasoning_blocks: Vec::new(),
        tool_calls: vec![ToolCall {
            call_id: ToolCallId::new(id),
            name: name.to_string(),
            raw_arguments: args.to_string(),
        }],
        finish_reason: FinishReason::ToolCalls,
        usage: usage(),
    }
}

fn stop() -> AssistantTurn {
    AssistantTurn {
        content: Some("done".to_string()),
        reasoning_blocks: Vec::new(),
        tool_calls: Vec::new(),
        finish_reason: FinishReason::Stop,
        usage: usage(),
    }
}

fn runtime_for(ws: &Path, state: &Path, capabilities: Arc<ActivationRegistry>) -> ToolRuntime {
    build_default_runtime(
        Arc::new(LocalHostEnv::new(ws, state).unwrap()),
        PolicyEngine::new(
            PolicyMode::FullAutonomous,
            Arc::new(NonInteractiveApprovalHandler),
        ),
        BudgetConfig::default(),
        None,
    )
    .with_capabilities(capabilities)
}

/// Run one scripted session through the real agent loop.
async fn run(
    provider: Arc<ScriptedProvider>,
    runtime: ToolRuntime,
    state: &Path,
) -> kroma_core::agent_loop::RunSummary {
    let budget = BudgetConfig::default();
    let cfg = AgentRunConfig::new("c00-04 wiring", budget);
    let logger = EventLogger::new(cfg.run_id, Some(state.to_path_buf()), false).unwrap();
    let (_tx, rx) = watch::channel(false);
    let (summary, _logger) = tokio::time::timeout(
        STEP_TIMEOUT,
        AgentRunner::new(cfg, provider, runtime, rx).run(logger),
    )
    .await
    .expect("the run must not hang");
    summary
}

/// The content of the tool result this run returned for `tool`, as the next request carried it.
fn tool_result(requests: &[ProviderRequest], tool: &str) -> String {
    requests
        .iter()
        .flat_map(|request| request.messages.iter())
        .filter_map(|item| match item {
            TranscriptItem::ToolResult(result) if result.tool_name == tool => {
                Some(result.content.to_string())
            }
            _ => None,
        })
        .next_back()
        .unwrap_or_else(|| panic!("no tool result for `{tool}`"))
}

fn write_turns() -> Vec<AssistantTurn> {
    vec![
        call(
            "call_write",
            "write",
            serde_json::json!({ "path": "note.txt", "content": "written\n" }),
        ),
        stop(),
    ]
}

/// C00-04-①: the run path declares and activates the capability itself, so the dispatch it allows
/// is dispatch the registry granted — not dispatch that happened because nothing was gated.
#[tokio::test]
async fn the_run_binds_a_live_lease_and_dispatches_under_it() {
    let (ws, state) = workspace("issued");
    let capabilities = Arc::new(ActivationRegistry::default());
    let provider = Arc::new(ScriptedProvider::new(write_turns()));

    let summary = run(
        Arc::clone(&provider),
        runtime_for(&ws, &state, Arc::clone(&capabilities)),
        &state,
    )
    .await;

    assert_eq!(summary.terminal_reason, TerminalReason::ModelStop);
    let written = ws.join("note.txt");
    assert!(
        written.exists(),
        "an unrevoked authority must dispatch normally: {}",
        tool_result(&provider.requests(), "write")
    );

    // The gate was established from a lease the registry issued: the capability is declared, it was
    // activated (its initialiser ran), and no holder is left behind once the lease was released.
    assert!(
        capabilities.state(RUN_DISPATCH_CAPABILITY).is_some(),
        "the run declares its dispatch capability on the live registry"
    );
    assert_eq!(
        capabilities.holders(RUN_DISPATCH_CAPABILITY),
        Some(0),
        "a run keeps the claim, not the lease: nothing is left holding the capability"
    );
    assert_eq!(
        capabilities.outstanding_leases(),
        0,
        "no lease escaped the run boundary"
    );

    let _ = std::fs::remove_dir_all(ws.parent().unwrap());
}

/// C00-04-②: a revoke that lands after the run took its lease refuses the next call *before* it
/// runs, and the refusal names the capability and the epoch. This is the acceptance point that was
/// missing in production: nothing was gated, so a revoke withdrew nothing.
#[tokio::test]
async fn a_revoke_after_the_lease_refuses_the_next_call_before_it_runs() {
    let (ws, state) = workspace("revoked");
    let capabilities = Arc::new(ActivationRegistry::default());
    // The revoke lands while answering the first request, i.e. after `prepare_run` took the run's
    // lease and before the `write` call it returns can be dispatched.
    let provider =
        Arc::new(ScriptedProvider::new(write_turns()).revoking(Arc::clone(&capabilities), 0));

    let summary = run(
        Arc::clone(&provider),
        runtime_for(&ws, &state, Arc::clone(&capabilities)),
        &state,
    )
    .await;

    // The run itself still finishes: revocation ends authority, it does not abort the session.
    assert_eq!(summary.terminal_reason, TerminalReason::ModelStop);
    assert_eq!(summary.status, RunStatus::Completed);

    assert!(
        !ws.join("note.txt").exists(),
        "the gate refuses before dispatch: the tool must not have run"
    );

    let refusal = tool_result(&provider.requests(), "write");
    assert!(
        refusal.contains("NOT_DISPATCHED_CAPABILITY_REVOKED"),
        "the refusal must name itself: {refusal}"
    );
    assert!(
        refusal.contains(RUN_DISPATCH_CAPABILITY) && refusal.contains("revoked"),
        "the refusal must name the capability and the reason: {refusal}"
    );
    assert_eq!(
        capabilities.generation(),
        1,
        "exactly one epoch bump happened, so the run was refused by that revoke"
    );

    let _ = std::fs::remove_dir_all(ws.parent().unwrap());
}

/// C00-04-③: the two tools the loop dispatches itself are behind the same gate. They are not routed
/// through `execute_call_inner`, so the loop asks the authority explicitly and lets a withdrawn call
/// fall through to the runtime, which refuses it by name like any other tool.
#[tokio::test]
async fn the_tools_the_loop_dispatches_itself_are_gated_too() {
    let (ws, state) = workspace("bypass");
    std::fs::write(ws.join("note.txt"), "content\n").unwrap();
    let capabilities = Arc::new(ActivationRegistry::default());
    let turns = vec![
        call(
            "call_recall",
            "recall_evidence",
            serde_json::json!({ "source_entry_id": "nothing" }),
        ),
        call("call_spawn", "spawn_subagent", serde_json::json!({})),
        stop(),
    ];
    let provider = Arc::new(ScriptedProvider::new(turns).revoking(Arc::clone(&capabilities), 0));

    let summary = run(
        Arc::clone(&provider),
        runtime_for(&ws, &state, Arc::clone(&capabilities)),
        &state,
    )
    .await;
    assert_eq!(summary.terminal_reason, TerminalReason::ModelStop);

    for tool in ["recall_evidence", "spawn_subagent"] {
        let result = tool_result(&provider.requests(), tool);
        assert!(
            result.contains("NOT_DISPATCHED_CAPABILITY_REVOKED")
                && result.contains(RUN_DISPATCH_CAPABILITY),
            "a revoked run must not dispatch `{tool}` on the loop's own path: {result}"
        );
    }
    assert!(
        !summary
            .transcript
            .iter()
            .any(|item| matches!(item, TranscriptItem::ToolResult(result)
                if result.tool_name == "spawn_subagent" && result.outcome.is_success())),
        "a refused `spawn_subagent` must not have spawned anything"
    );

    let _ = std::fs::remove_dir_all(ws.parent().unwrap());
}

/// C00-04-④: an authority that cannot be issued is fail-closed. The capability here is declared by
/// its owner with a real initialiser, and the revoke lands *inside* that initialisation, so the run
/// is issued no claim at all — every dispatch is then refused with the reason rather than running
/// ungated.
#[tokio::test]
async fn an_authority_that_cannot_be_issued_refuses_every_dispatch() {
    let (ws, state) = workspace("not-issued");
    let capabilities = Arc::new(ActivationRegistry::default());
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let release = Arc::new(std::sync::Barrier::new(2));
    let held_open = Arc::clone(&release);
    capabilities.declare(
        RUN_DISPATCH_CAPABILITY,
        &[],
        Arc::new(move || {
            entered_tx.send(()).expect("initialiser signals entry");
            held_open.wait();
            Ok(())
        }),
    );
    let runtime = runtime_for(&ws, &state, Arc::clone(&capabilities));

    let binding = {
        let capabilities = Arc::clone(&capabilities);
        let binding_runtime = runtime.clone();
        std::thread::spawn(move || {
            let outcome = binding_runtime.bind_dispatch_authority();
            drop(capabilities);
            outcome
        })
    };
    entered_rx
        .recv()
        .expect("the run's activation must have started before the revoke");
    assert_eq!(
        capabilities.revoke(RUN_DISPATCH_CAPABILITY),
        vec![RUN_DISPATCH_CAPABILITY.to_string()]
    );
    release.wait();

    let refused = binding
        .join()
        .expect("the binding thread joins")
        .expect_err("an activation that outlived a revoke must issue no authority");
    assert!(
        refused.contains(RUN_DISPATCH_CAPABILITY) && refused.contains("revoked"),
        "the reason names the capability and the epoch: {refused}"
    );
    assert_eq!(
        capabilities.state(RUN_DISPATCH_CAPABILITY),
        Some(ActivationState::Dormant),
        "the revoked attempt left the capability dormant and issued nothing"
    );

    // Fail-closed: the call is refused with the same reason, not dispatched.
    let gate = runtime
        .dispatch_authority("read")
        .expect_err("a run with no authority refuses every dispatch");
    assert_eq!(gate, refused);

    let (_tx, rx) = watch::channel(false);
    let (message, decision, _hooks) = tokio::time::timeout(
        STEP_TIMEOUT,
        runtime.execute_call(
            &ToolCall {
                call_id: ToolCallId::new("call_read"),
                name: "read".to_string(),
                raw_arguments: serde_json::json!({ "path": "note.txt" }).to_string(),
            },
            0,
            rx,
        ),
    )
    .await
    .expect("dispatch must not hang");
    assert!(!message.outcome.is_success());
    assert_eq!(decision, "deny_capability_revoked");
    assert!(
        message
            .content
            .contains("NOT_DISPATCHED_CAPABILITY_REVOKED"),
        "even an ungated-looking tool is refused when no authority was issued: {}",
        message.content
    );

    let _ = std::fs::remove_dir_all(ws.parent().unwrap());
}

/// C00-04-⑤: the boundary this wiring does *not* decide. Issuance is per binding, so a revoke ends
/// the runs that hold the authority and a later binding issues its own — who may issue, and whether
/// a revoke is final for the process, is an owner decision. Pinned here so the limit is visible
/// rather than discovered.
#[tokio::test]
async fn a_later_run_issues_its_own_authority_after_a_revoke() {
    let (ws, state) = workspace("reissue");
    let capabilities = Arc::new(ActivationRegistry::default());
    let runtime = runtime_for(&ws, &state, Arc::clone(&capabilities));

    let first = runtime.bind_dispatch_authority().expect("first issuance");
    assert!(runtime.dispatch_authority("read").is_ok());
    capabilities.revoke(RUN_DISPATCH_CAPABILITY);
    assert!(
        runtime.dispatch_authority("read").is_err(),
        "the authority issued before the revoke stays dead"
    );

    let second = runtime
        .bind_dispatch_authority()
        .expect("a new binding issues a new generation");
    assert!(
        second > first,
        "re-activation is a new generation ({first} -> {second})"
    );
    assert!(runtime.dispatch_authority("read").is_ok());

    let _ = std::fs::remove_dir_all(ws.parent().unwrap());
}
