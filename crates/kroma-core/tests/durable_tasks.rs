//! Capability gap #4 (`v1.5.2`): the durable async-task control plane must reflect *real* work, and
//! its `cancel`/`resume` must be executable without weakening the spawn contract.
//!
//! These tests drive the production path — the agent loop's `spawn_subagent` batch calls into
//! [`DurableTaskHost`] — and assert the durable record gets a real spend, artifact and notification,
//! that `cancel` actually stops the live child, that a task left `Running` by a crash is recovered to
//! `Suspended` and can be *resumed* (re-run) for real, and that resume admission obeys one-writer /
//! single-run / gate-and-rollback contracts.

use kroma_core::agent_loop::{
    build_runtime_with_registry, AgentRunConfig, AgentRunner, RunSummary,
};
use kroma_core::async_tasks::{
    AsyncTaskRegistry, NewTask, PendingMergeLink, TaskControlRequest, TaskState,
};
use kroma_core::budget::BudgetConfig;
use kroma_core::completion_gate::WorkspaceCompletionGate;
use kroma_core::env::{
    AllowAllGate, CompletionGate, CompletionGateContext, CompletionGateStatus, ExecutionEnv,
    GateOutcome, LocalHostEnv, SharedCompletionGate,
};
use kroma_core::events::EventLogger;
use kroma_core::messages::{AssistantTurn, FinishReason, TokenUsage, ToolCall};
use kroma_core::provider::{ModelProvider, ProviderCompletion, ProviderError, ProviderRequest};
use kroma_core::revision::{RevisionId, SnapshotStore};
use kroma_core::session::SessionJournal;
use kroma_core::subagents::{SubagentBudget, SubagentRole, SubagentSpec};
use kroma_core::task_host::DurableTaskHost;
use kroma_core::tool::policy::{NonInteractiveApprovalHandler, PolicyEngine, PolicyMode};
use kroma_core::tool::registry::ToolRegistry;
use kroma_core::types::ToolCallId;
use serde_json::json;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::watch;

fn temp_root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("kroma-tasks-{}-{}", tag, uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn usage(input: u64, output: u64) -> Option<TokenUsage> {
    Some(TokenUsage {
        input_tokens: input,
        cached_input_tokens: None,
        cache_creation_input_tokens: None,
        output_tokens: output,
        reasoning_tokens: None,
    })
}

fn stop(text: &str) -> AssistantTurn {
    AssistantTurn {
        content: Some(text.to_string()),
        reasoning_blocks: Vec::new(),
        tool_calls: Vec::new(),
        finish_reason: FinishReason::Stop,
        usage: usage(10, 5),
    }
}

fn spawn_scout(task: &str) -> AssistantTurn {
    AssistantTurn {
        content: None,
        reasoning_blocks: Vec::new(),
        tool_calls: vec![ToolCall {
            call_id: ToolCallId::new("call_scout_1"),
            name: "spawn_subagent".into(),
            raw_arguments: json!({ "role": "scout", "task": task, "max_turns": 3 }).to_string(),
        }],
        finish_reason: FinishReason::ToolCalls,
        usage: usage(10, 5),
    }
}

fn write_file(path: &str, content: &str) -> AssistantTurn {
    AssistantTurn {
        content: None,
        reasoning_blocks: Vec::new(),
        tool_calls: vec![ToolCall {
            call_id: ToolCallId::new("call_write_1"),
            name: "write".into(),
            raw_arguments: json!({ "path": path, "content": content }).to_string(),
        }],
        finish_reason: FinishReason::ToolCalls,
        usage: usage(10, 5),
    }
}

fn bash(command: &str) -> AssistantTurn {
    AssistantTurn {
        content: None,
        reasoning_blocks: Vec::new(),
        tool_calls: vec![ToolCall {
            call_id: ToolCallId::new("call_bash_1"),
            name: "bash".into(),
            raw_arguments: json!({ "command": command }).to_string(),
        }],
        finish_reason: FinishReason::ToolCalls,
        usage: usage(10, 5),
    }
}

/// What a scripted provider does for one request. `AwaitCancel` hangs until the request is
/// cancelled, which lets a test prove that a durable cancel reaches the live child. `AwaitRelease`
/// hangs until the test releases it, so a test can hold a run mid-flight while it repairs state.
#[derive(Clone)]
enum Action {
    Turn(AssistantTurn),
    AwaitCancel,
    AwaitRelease(Arc<tokio::sync::Notify>),
}

/// A provider whose reply is a function of the request, so the parent and the child can be scripted
/// apart by their system prompt (the child carries a `[SUBAGENT ROLE: ...]` banner).
struct ScriptProvider {
    script: Arc<dyn Fn(&ProviderRequest) -> Action + Send + Sync>,
}

impl ModelProvider for ScriptProvider {
    fn complete<'a>(
        &'a self,
        req: ProviderRequest,
        _budget: &'a BudgetConfig,
        mut cancel_rx: watch::Receiver<bool>,
    ) -> Pin<Box<dyn Future<Output = Result<ProviderCompletion, ProviderError>> + Send + 'a>> {
        let action = (self.script)(&req);
        Box::pin(async move {
            match action {
                Action::Turn(turn) => Ok(ProviderCompletion {
                    turn,
                    request_body_bytes: 0,
                    latency_ms: 1,
                    ttft_ms: Some(1),
                    attempts_used: 1,
                    reported_model: None,
                }),
                Action::AwaitCancel => {
                    let _ = cancel_rx.changed().await;
                    Err(ProviderError::Cancelled)
                }
                Action::AwaitRelease(release) => {
                    release.notified().await;
                    Ok(ProviderCompletion {
                        turn: stop("released after the fault was repaired"),
                        request_body_bytes: 0,
                        latency_ms: 1,
                        ttft_ms: Some(1),
                        attempts_used: 1,
                        reported_model: None,
                    })
                }
            }
        })
    }
}

fn is_child(req: &ProviderRequest) -> bool {
    req.system_prompt.contains("[SUBAGENT ROLE: scout")
        || req.system_prompt.contains("[SUBAGENT ROLE: worker")
}

/// A completion gate that always rejects, to prove a resume merge that fails verification is not
/// recorded as a success.
struct RejectGate;

impl CompletionGate for RejectGate {
    fn check<'a>(
        &'a self,
        _ctx: &'a CompletionGateContext,
    ) -> Pin<Box<dyn Future<Output = Result<GateOutcome, String>> + Send + 'a>> {
        Box::pin(async {
            Ok(GateOutcome::Rejected {
                feedback: "gate says no".to_string(),
            })
        })
    }
}

fn make_host(
    root: &std::path::Path,
    ws: &std::path::Path,
    provider: Arc<dyn ModelProvider>,
    registry: Arc<Mutex<AsyncTaskRegistry>>,
    gate: SharedCompletionGate,
) -> (Arc<DurableTaskHost>, Arc<dyn ExecutionEnv>) {
    let state = root.join("state");
    let journal = SessionJournal::create(state.join("sessions"), ws).unwrap();
    let session_path = journal.path().to_path_buf();
    drop(journal);
    make_host_bound_to(root, ws, provider, registry, gate, "sess-1", session_path)
}

/// Build a host bound to an explicit session id/WAL, so a test can drive a session switch.
fn make_host_bound_to(
    root: &std::path::Path,
    ws: &std::path::Path,
    provider: Arc<dyn ModelProvider>,
    registry: Arc<Mutex<AsyncTaskRegistry>>,
    gate: SharedCompletionGate,
    session_id: &str,
    session_path: PathBuf,
) -> (Arc<DurableTaskHost>, Arc<dyn ExecutionEnv>) {
    make_host_with_policy(
        root,
        ws,
        provider,
        registry,
        gate,
        session_id,
        session_path,
        PolicyMode::FullAutonomous,
    )
}

/// The same host under an explicit parent policy, so a test can model a process that restarted with
/// a narrower policy than the one a task was admitted under.
#[allow(clippy::too_many_arguments)]
fn make_host_with_policy(
    root: &std::path::Path,
    ws: &std::path::Path,
    provider: Arc<dyn ModelProvider>,
    registry: Arc<Mutex<AsyncTaskRegistry>>,
    gate: SharedCompletionGate,
    session_id: &str,
    session_path: PathBuf,
    parent_policy_mode: PolicyMode,
) -> (Arc<DurableTaskHost>, Arc<dyn ExecutionEnv>) {
    make_host_with_execution(
        root,
        ws,
        provider,
        registry,
        gate,
        session_id,
        session_path,
        parent_policy_mode,
        // R01: these tests run on the host explicitly, which is also the boundary they model.
        Some(kroma_core::sandbox::ExecutionPolicy::host()),
    )
}

/// The host with an explicit execution boundary (or none at all), so a test can model a surface that
/// did not attach the operator's policy.
#[allow(clippy::too_many_arguments)]
fn make_host_with_execution(
    root: &std::path::Path,
    ws: &std::path::Path,
    provider: Arc<dyn ModelProvider>,
    registry: Arc<Mutex<AsyncTaskRegistry>>,
    gate: SharedCompletionGate,
    session_id: &str,
    session_path: PathBuf,
    parent_policy_mode: PolicyMode,
    execution: Option<kroma_core::sandbox::ExecutionPolicy>,
) -> (Arc<DurableTaskHost>, Arc<dyn ExecutionEnv>) {
    let state = root.join("state");
    let env: Arc<dyn ExecutionEnv> = Arc::new(LocalHostEnv::new(ws, state.clone()).unwrap());
    let host = DurableTaskHost::new(
        registry,
        provider,
        Arc::new(SnapshotStore::new(state.join("snapshots"))),
        env.workspace().clone(),
        state.join("subagents"),
        state.join("worktrees"),
        Vec::new(),
        parent_policy_mode,
        BudgetConfig::default(),
        false,
        gate,
        session_id.to_string(),
        "owner-1".to_string(),
        session_path,
    );
    let host = match execution {
        Some(policy) => host.with_execution_policy(policy),
        None => host,
    };
    (Arc::new(host), env)
}

async fn run_once(
    env: &Arc<dyn ExecutionEnv>,
    provider: Arc<dyn ModelProvider>,
    host: Arc<DurableTaskHost>,
    task: &str,
) -> RunSummary {
    let budget = BudgetConfig::default();
    let policy = PolicyEngine::new(
        PolicyMode::FullAutonomous,
        Arc::new(NonInteractiveApprovalHandler),
    );
    let runtime = build_runtime_with_registry(
        ToolRegistry::default(),
        Arc::clone(env),
        policy,
        budget.clone(),
    );
    let run_cfg = AgentRunConfig::new(task, budget).with_durable_tasks(Some(host));
    let logger =
        EventLogger::new(run_cfg.run_id, Some(env.state_dir().to_path_buf()), false).unwrap();
    let (_tx, rx) = watch::channel(false);
    let runner = AgentRunner::new(run_cfg, provider, runtime, rx);
    runner.run(logger).await.0
}

fn open_registry(root: &std::path::Path) -> Arc<Mutex<AsyncTaskRegistry>> {
    Arc::new(Mutex::new(
        AsyncTaskRegistry::open(root.join("state/tasks/registry.json")).unwrap(),
    ))
}

/// Seed a suspended task with a real persisted spec, as a crashed process would have left it.
fn seed_suspended(
    registry: &Arc<Mutex<AsyncTaskRegistry>>,
    ws: &std::path::Path,
    task_id: &str,
    spec: &SubagentSpec,
    allocation: u64,
) {
    seed_suspended_in_session(registry, ws, task_id, spec, allocation, "sess-1", "")
}

/// Seed a suspended task that records the session WAL it was admitted against.
#[allow(clippy::too_many_arguments)]
fn seed_suspended_in_session(
    registry: &Arc<Mutex<AsyncTaskRegistry>>,
    ws: &std::path::Path,
    task_id: &str,
    spec: &SubagentSpec,
    allocation: u64,
    session_id: &str,
    session_path: &str,
) {
    let mut r = registry.lock().unwrap();
    r.register(NewTask {
        task_id: task_id.to_string(),
        session_id: session_id.to_string(),
        owner_id: "owner-1".to_string(),
        workspace_root: ws.display().to_string(),
        role: spec.role.as_str().to_string(),
        created_turn: 0,
        token_allocation: allocation,
        prompt: spec.task.clone(),
        timeout_ms: spec.budget.timeout_ms,
        spec_json: serde_json::to_string(spec).unwrap(),
        parent_policy: serde_json::to_string(&PolicyMode::FullAutonomous).unwrap(),
        parent_policy_hooks: kroma_core::subagents::ParentPolicyHooks::KnownEmpty,
        session_path: session_path.to_string(),
    })
    .unwrap();
    r.transition(task_id, TaskState::Running, 1).unwrap();
    r.suspend(task_id, 1).unwrap();
}

async fn wait_terminal(registry: &Arc<Mutex<AsyncTaskRegistry>>, task_id: &str) -> TaskState {
    for _ in 0..3000 {
        {
            let r = registry.lock().unwrap();
            let state = r.get(task_id).unwrap().state;
            if state.is_terminal() {
                return state;
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("task '{}' never reached a terminal state", task_id);
}

#[tokio::test]
async fn a_real_spawn_registers_settles_and_notifies() {
    let root = temp_root("settle");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join("a.txt"), "hello").unwrap();

    let registry = open_registry(&root);
    let calls = Arc::new(AtomicUsize::new(0));
    let provider: Arc<dyn ModelProvider> = Arc::new(ScriptProvider {
        script: Arc::new({
            let calls = Arc::clone(&calls);
            move |req: &ProviderRequest| {
                if is_child(req) {
                    Action::Turn(stop("scout found the answer"))
                } else if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    Action::Turn(spawn_scout("find the answer"))
                } else {
                    Action::Turn(stop("parent done"))
                }
            }
        }),
    });
    let (host, env) = make_host(
        &root,
        &ws,
        provider.clone(),
        Arc::clone(&registry),
        Arc::new(AllowAllGate),
    );

    let summary = run_once(&env, provider, host, "spawn a scout").await;
    assert!(
        summary.children_input_tokens > 0,
        "the child's spend must be attributed to the parent run"
    );

    let tasks = registry.lock().unwrap();
    let records = tasks.tasks_for_session("sess-1");
    assert_eq!(
        records.len(),
        1,
        "exactly one durable task must be recorded"
    );
    let task = records[0];
    assert_eq!(
        task.state,
        TaskState::Completed,
        "the task must record the real completion, got {:?}",
        task.state
    );
    assert!(task.token_spent > 0, "the task must record real spend");
    assert!(
        !task.artifacts.is_empty(),
        "the task must attach the child report as an artifact"
    );
    assert!(
        task.notifications.iter().any(|n| n.kind == "completed"),
        "the task must raise a completion notification"
    );
    assert_eq!(task.prompt, "find the answer");
    assert!(
        !task.spec_json.is_empty(),
        "the full spec must be persisted for resume"
    );
}

#[tokio::test]
async fn cancel_stops_the_live_child_and_marks_the_task_cancelled() {
    let root = temp_root("cancel");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join("a.txt"), "hello").unwrap();
    let registry = open_registry(&root);
    let calls = Arc::new(AtomicUsize::new(0));
    let provider: Arc<dyn ModelProvider> = Arc::new(ScriptProvider {
        script: Arc::new({
            let calls = Arc::clone(&calls);
            move |req: &ProviderRequest| {
                if is_child(req) {
                    Action::AwaitCancel
                } else if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    Action::Turn(spawn_scout("hang until cancelled"))
                } else {
                    Action::Turn(stop("parent done"))
                }
            }
        }),
    });
    let (host, env) = make_host(
        &root,
        &ws,
        provider.clone(),
        Arc::clone(&registry),
        Arc::new(AllowAllGate),
    );

    let host_for_cancel = Arc::clone(&host);
    let registry_for_poll = Arc::clone(&registry);
    let canceller = tokio::spawn(async move {
        for _ in 0..3000 {
            let id = {
                let tasks = registry_for_poll.lock().unwrap();
                tasks
                    .tasks_for_session("sess-1")
                    .into_iter()
                    .find(|t| t.state == TaskState::Running)
                    .map(|t| t.task_id.clone())
            };
            if let Some(id) = id {
                let signalled = host_for_cancel.cancel_task(&id).unwrap();
                assert!(signalled, "cancel must signal the live child");
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the child never registered as Running");
    });

    let started = std::time::Instant::now();
    let _ = run_once(&env, provider, host, "spawn a scout").await;
    canceller.await.unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "the child must be stopped by the cancel, not by a timeout"
    );

    let tasks = registry.lock().unwrap();
    let task = tasks.tasks_for_session("sess-1").remove(0);
    assert_eq!(task.state, TaskState::Cancelled);
}

#[tokio::test]
async fn restart_recovery_marks_a_started_task_unknown_and_refuses_resume() {
    let root = temp_root("recovery-unknown");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join("a.txt"), "hello").unwrap();
    let registry_path = root.join("state/tasks/registry.json");
    let spec = SubagentSpec {
        role: SubagentRole::Scout,
        task: "must not be re-run".into(),
        allowed_write_globs: None,
        budget: SubagentBudget {
            max_tokens: 200_000,
            timeout_ms: 5000,
            ..SubagentBudget::default()
        },
        revert_on_verify_failure: false,
        disallow_test_tampering: false,
        protected_paths: Vec::new(),
    };
    {
        let mut r = AsyncTaskRegistry::open(&registry_path).unwrap();
        r.register(NewTask {
            task_id: "task-1".into(),
            session_id: "sess-1".into(),
            owner_id: "owner-1".into(),
            workspace_root: ws.display().to_string(),
            role: "scout".into(),
            created_turn: 0,
            token_allocation: 200_000,
            prompt: spec.task.clone(),
            timeout_ms: spec.budget.timeout_ms,
            spec_json: serde_json::to_string(&spec).unwrap(),
            parent_policy: serde_json::to_string(&PolicyMode::FullAutonomous).unwrap(),
            parent_policy_hooks: kroma_core::subagents::ParentPolicyHooks::KnownEmpty,
            session_path: String::new(),
        })
        .unwrap();
        // A *real* admission: `begin_execution` persists the durable "started" marker before any
        // side effect, exactly like the production spawn/resume paths.
        r.begin_execution("task-1", 1).unwrap();
    }

    let registry = Arc::new(Mutex::new(AsyncTaskRegistry::open(&registry_path).unwrap()));
    {
        let mut r = registry.lock().unwrap();
        assert_eq!(r.recover_interrupted().unwrap(), 1);
        let record = r.get("task-1").unwrap();
        assert_eq!(record.state, TaskState::Suspended);
        assert!(
            record.is_recovery_blocked(),
            "a task that had started must be marked unknown, not silently resumable"
        );
        assert!(
            record
                .notifications
                .iter()
                .any(|n| n.message.contains("UNKNOWN")),
            "the notification must state the outcome is unknown"
        );
        // Idempotent: a second recovery must not erase the unknown disposition.
        assert_eq!(r.recover_interrupted().unwrap(), 0);
        assert!(r.get("task-1").unwrap().is_recovery_blocked());
    }

    let calls = Arc::new(AtomicUsize::new(0));
    let provider: Arc<dyn ModelProvider> = Arc::new(ScriptProvider {
        script: Arc::new({
            let calls = Arc::clone(&calls);
            move |_req: &ProviderRequest| {
                calls.fetch_add(1, Ordering::SeqCst);
                Action::Turn(stop("must never run"))
            }
        }),
    });
    let (host, _env) = make_host(
        &root,
        &ws,
        provider,
        Arc::clone(&registry),
        Arc::new(AllowAllGate),
    );

    let err = host
        .resume_task("task-1")
        .expect_err("a recovery-blocked task must refuse resume");
    assert!(
        err.contains("reconciled"),
        "the refusal must be structured, got: {}",
        err
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "a recovery-blocked task must not re-run the child"
    );
    // The record and its unknown disposition are preserved, never erased or auto-retried.
    let record = registry.lock().unwrap().get_cloned("task-1").unwrap();
    assert_eq!(record.state, TaskState::Suspended);
    assert!(record.is_recovery_blocked());

    // The user-visible summary reports the unknown disposition and that resume is blocked.
    let listed = registry
        .lock()
        .unwrap()
        .apply_control(&TaskControlRequest::List { session_id: None })
        .unwrap();
    let task = listed["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["task_id"].as_str() == Some("task-1"))
        .unwrap();
    assert_eq!(task["recovery"], json!("unknown_outcome"));
    assert_eq!(task["resume_blocked"], json!(true));
}

/// A task suspended *before* it was ever admitted to run (no started marker) cannot have produced
/// side effects, so it is still legitimately resumable after a restart.
#[tokio::test]
async fn restart_recovery_leaves_a_never_started_task_resumable() {
    let root = temp_root("recovery-safe");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join("a.txt"), "hello").unwrap();
    let registry = open_registry(&root);
    let spec = SubagentSpec {
        role: SubagentRole::Scout,
        task: "safe to re-run".into(),
        allowed_write_globs: None,
        budget: SubagentBudget {
            max_tokens: 200_000,
            timeout_ms: 5000,
            ..SubagentBudget::default()
        },
        revert_on_verify_failure: false,
        disallow_test_tampering: false,
        protected_paths: Vec::new(),
    };
    {
        let mut r = registry.lock().unwrap();
        r.register(NewTask {
            task_id: "task-1".into(),
            session_id: "sess-1".into(),
            owner_id: "owner-1".into(),
            workspace_root: ws.display().to_string(),
            role: "scout".into(),
            created_turn: 0,
            token_allocation: 200_000,
            prompt: spec.task.clone(),
            timeout_ms: spec.budget.timeout_ms,
            spec_json: serde_json::to_string(&spec).unwrap(),
            parent_policy: serde_json::to_string(&PolicyMode::FullAutonomous).unwrap(),
            parent_policy_hooks: kroma_core::subagents::ParentPolicyHooks::KnownEmpty,
            session_path: String::new(),
        })
        .unwrap();
        // Suspended before admission: no started marker, so no side effect is possible.
        r.transition("task-1", TaskState::Suspended, 1).unwrap();
    }

    let calls = Arc::new(AtomicUsize::new(0));
    let provider: Arc<dyn ModelProvider> = Arc::new(ScriptProvider {
        script: Arc::new({
            let calls = Arc::clone(&calls);
            move |_req: &ProviderRequest| {
                calls.fetch_add(1, Ordering::SeqCst);
                Action::Turn(stop("resumed work completed"))
            }
        }),
    });
    let (host, _env) = make_host(
        &root,
        &ws,
        provider,
        Arc::clone(&registry),
        Arc::new(AllowAllGate),
    );

    host.resume_task("task-1").unwrap();
    assert_eq!(
        wait_terminal(&registry, "task-1").await,
        TaskState::Completed,
        "a never-started task must still resume to completion"
    );
    assert!(calls.load(Ordering::SeqCst) >= 1, "the child really ran");
}

/// A registry document written by an older version has no `recovery` marker at all: a `Suspended`
/// record there cannot be proven never-started, so it must be refused rather than re-run.
#[tokio::test]
async fn legacy_suspended_record_without_a_recovery_marker_is_not_resumable() {
    let root = temp_root("legacy-recovery");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join("a.txt"), "hello").unwrap();
    let spec = SubagentSpec {
        role: SubagentRole::Scout,
        task: "must not run".into(),
        allowed_write_globs: None,
        budget: SubagentBudget {
            max_tokens: 200_000,
            timeout_ms: 5000,
            ..SubagentBudget::default()
        },
        revert_on_verify_failure: false,
        disallow_test_tampering: false,
        protected_paths: Vec::new(),
    };
    // Exactly the legacy document shape: the task has no `recovery` key.
    let doc = json!({
        "version": 1,
        "next_notification_seq": 1,
        "tasks": [{
            "task_id": "task-1",
            "session_id": "sess-1",
            "owner_id": "owner-1",
            "workspace_root": ws.display().to_string(),
            "role": "scout",
            "state": "suspended",
            "created_turn": 0,
            "updated_turn": 1,
            "token_allocation": 200000,
            "token_spent": 0,
            "artifacts": [],
            "notifications": [],
            "spec_json": serde_json::to_string(&spec).unwrap()
        }]
    });
    let dir = root.join("state/tasks");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("registry.json"),
        serde_json::to_string(&doc).unwrap(),
    )
    .unwrap();

    let registry = open_registry(&root);
    {
        let r = registry.lock().unwrap();
        let record = r.get("task-1").unwrap();
        assert!(
            record.recovery.is_none(),
            "a legacy record deserializes to no marker"
        );
        assert!(
            !record.can_resume_safely(),
            "a legacy Suspended record is not provably never-started"
        );
    }

    let calls = Arc::new(AtomicUsize::new(0));
    let provider: Arc<dyn ModelProvider> = Arc::new(ScriptProvider {
        script: Arc::new({
            let calls = Arc::clone(&calls);
            move |_req: &ProviderRequest| {
                calls.fetch_add(1, Ordering::SeqCst);
                Action::Turn(stop("must never run"))
            }
        }),
    });
    let (host, _env) = make_host(
        &root,
        &ws,
        provider,
        Arc::clone(&registry),
        Arc::new(AllowAllGate),
    );
    let err = host
        .resume_task("task-1")
        .expect_err("a legacy record must not be re-run");
    assert!(
        err.contains("reconciled"),
        "structured refusal, got: {}",
        err
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "a legacy record must not re-run the child"
    );
}

/// Pausing a live task only flips the record; the child keeps running. A resume must therefore not
/// spawn a second concurrent run for a task that already started.
#[tokio::test]
async fn a_live_paused_started_task_refuses_a_second_run() {
    let root = temp_root("live-pause");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join("a.txt"), "hello").unwrap();
    let registry = open_registry(&root);
    let spec = SubagentSpec {
        role: SubagentRole::Scout,
        task: "must not run twice".into(),
        allowed_write_globs: None,
        budget: SubagentBudget {
            max_tokens: 200_000,
            timeout_ms: 5000,
            ..SubagentBudget::default()
        },
        revert_on_verify_failure: false,
        disallow_test_tampering: false,
        protected_paths: Vec::new(),
    };
    {
        let mut r = registry.lock().unwrap();
        r.register(NewTask {
            task_id: "task-1".into(),
            session_id: "sess-1".into(),
            owner_id: "owner-1".into(),
            workspace_root: ws.display().to_string(),
            role: "scout".into(),
            created_turn: 0,
            token_allocation: 200_000,
            prompt: spec.task.clone(),
            timeout_ms: spec.budget.timeout_ms,
            spec_json: serde_json::to_string(&spec).unwrap(),
            parent_policy: serde_json::to_string(&PolicyMode::FullAutonomous).unwrap(),
            parent_policy_hooks: kroma_core::subagents::ParentPolicyHooks::KnownEmpty,
            session_path: String::new(),
        })
        .unwrap();
        // A real admission (started marker), then an operator pause that does not stop the child.
        r.begin_execution("task-1", 1).unwrap();
        r.suspend("task-1", 1).unwrap();
    }

    let calls = Arc::new(AtomicUsize::new(0));
    let provider: Arc<dyn ModelProvider> = Arc::new(ScriptProvider {
        script: Arc::new({
            let calls = Arc::clone(&calls);
            move |_req: &ProviderRequest| {
                calls.fetch_add(1, Ordering::SeqCst);
                Action::Turn(stop("must never run"))
            }
        }),
    });
    let (host, _env) = make_host(
        &root,
        &ws,
        provider,
        Arc::clone(&registry),
        Arc::new(AllowAllGate),
    );
    let err = host
        .resume_task("task-1")
        .expect_err("a live-paused started task must not spawn a second run");
    assert!(
        err.contains("reconciled"),
        "structured refusal, got: {}",
        err
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "no second provider run may start"
    );
}

/// Resume must not run while another writer holds the workspace: no backend call, structured
/// failure, and the task stays resumable.
#[tokio::test]
async fn resume_without_the_writer_lock_calls_the_backend_zero_times_and_stays_resumable() {
    let root = temp_root("resume-writer");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join("a.txt"), "hello").unwrap();
    let registry = open_registry(&root);
    let spec = SubagentSpec {
        role: SubagentRole::Scout,
        task: "resume me".into(),
        allowed_write_globs: None,
        budget: SubagentBudget {
            max_tokens: 200_000,
            timeout_ms: 5000,
            ..SubagentBudget::default()
        },
        revert_on_verify_failure: false,
        disallow_test_tampering: false,
        protected_paths: Vec::new(),
    };
    seed_suspended(&registry, &ws, "task-1", &spec, 200_000);

    let calls = Arc::new(AtomicUsize::new(0));
    let provider: Arc<dyn ModelProvider> = Arc::new(ScriptProvider {
        script: Arc::new({
            let calls = Arc::clone(&calls);
            move |_req: &ProviderRequest| {
                calls.fetch_add(1, Ordering::SeqCst);
                Action::Turn(stop("resumed"))
            }
        }),
    });
    let (host, _env) = make_host(
        &root,
        &ws,
        provider,
        Arc::clone(&registry),
        Arc::new(AllowAllGate),
    );

    // Another writer holds the workspace (as a second runtime/process would).
    let held = kroma_core::session_ownership::try_acquire_writer(&ws, "other-writer").unwrap();

    let err = host
        .resume_task("task-1")
        .expect_err("resume must fail while another writer holds the workspace");
    assert!(
        err.contains("writer") || err.contains("busy"),
        "the failure must explain the writer conflict, got: {}",
        err
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "the backend must not be called when the writer lock is unavailable"
    );
    assert_eq!(
        registry.lock().unwrap().get("task-1").unwrap().state,
        TaskState::Suspended,
        "the task must remain resumable after a refused resume"
    );

    // Releasing the writer lock lets the resume proceed.
    drop(held);
    host.resume_task("task-1").unwrap();
    assert_eq!(
        wait_terminal(&registry, "task-1").await,
        TaskState::Completed
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the backend must run exactly once after the writer is free"
    );
}

/// Two concurrent resumes of the same task admit exactly one run; a cancel hits only that run.
#[tokio::test]
async fn two_concurrent_resumes_run_the_backend_once_and_cancel_hits_only_that_run() {
    let root = temp_root("resume-concurrent");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join("a.txt"), "hello").unwrap();
    let registry = open_registry(&root);
    let spec = SubagentSpec {
        role: SubagentRole::Scout,
        task: "resume once".into(),
        allowed_write_globs: None,
        budget: SubagentBudget {
            max_tokens: 200_000,
            timeout_ms: 20_000,
            ..SubagentBudget::default()
        },
        revert_on_verify_failure: false,
        disallow_test_tampering: false,
        protected_paths: Vec::new(),
    };
    seed_suspended(&registry, &ws, "task-1", &spec, 200_000);

    let calls = Arc::new(AtomicUsize::new(0));
    let provider: Arc<dyn ModelProvider> = Arc::new(ScriptProvider {
        script: Arc::new({
            let calls = Arc::clone(&calls);
            move |_req: &ProviderRequest| {
                calls.fetch_add(1, Ordering::SeqCst);
                Action::AwaitCancel
            }
        }),
    });
    let (host, _env) = make_host(
        &root,
        &ws,
        provider,
        Arc::clone(&registry),
        Arc::new(AllowAllGate),
    );

    let h1 = Arc::clone(&host);
    let h2 = Arc::clone(&host);
    let (r1, r2) = tokio::join!(
        tokio::spawn(async move { h1.resume_task("task-1") }),
        tokio::spawn(async move { h2.resume_task("task-1") }),
    );
    let outcomes = [r1.unwrap(), r2.unwrap()];
    let accepted = outcomes.iter().filter(|r| r.is_ok()).count();
    let refused = outcomes.iter().filter(|r| r.is_err()).count();
    assert_eq!(
        accepted, 1,
        "exactly one resume must be admitted: {:?}",
        outcomes
    );
    assert_eq!(
        refused, 1,
        "the second resume must be refused: {:?}",
        outcomes
    );

    // Let the admitted run reach the backend exactly once.
    for _ in 0..300 {
        if calls.load(Ordering::SeqCst) >= 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1, "the backend must run once");

    // Cancel that single run; it stops and the count does not grow.
    let signalled = host.cancel_task("task-1").unwrap();
    assert!(signalled, "cancel must reach the admitted run");
    assert_eq!(
        wait_terminal(&registry, "task-1").await,
        TaskState::Cancelled
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1, "no extra backend run");
}

/// A task that is already `Running` (a live child) must refuse a second resume instead of
/// overwriting its canceller and spawning a duplicate backend run.
#[tokio::test]
async fn resume_of_a_running_task_is_refused_and_runs_no_backend() {
    let root = temp_root("resume-running");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join("a.txt"), "hello").unwrap();
    let registry = open_registry(&root);
    let calls = Arc::new(AtomicUsize::new(0));
    let provider: Arc<dyn ModelProvider> = Arc::new(ScriptProvider {
        script: Arc::new({
            let calls = Arc::clone(&calls);
            move |_req: &ProviderRequest| {
                calls.fetch_add(1, Ordering::SeqCst);
                Action::Turn(stop("ran"))
            }
        }),
    });
    let (host, _env) = make_host(
        &root,
        &ws,
        provider,
        Arc::clone(&registry),
        Arc::new(AllowAllGate),
    );
    let spec = SubagentSpec {
        role: SubagentRole::Scout,
        task: "already running".into(),
        allowed_write_globs: None,
        budget: SubagentBudget {
            max_tokens: 200_000,
            timeout_ms: 5000,
            ..SubagentBudget::default()
        },
        revert_on_verify_failure: false,
        disallow_test_tampering: false,
        protected_paths: Vec::new(),
    };
    let (cancel_tx, _cancel_rx) = watch::channel(false);
    host.register_child(
        &kroma_core::task_host::ChildTaskSpec {
            task_id: "task-1".to_string(),
            spec,
            allocation: 200_000,
            turn: 0,
            parent_policy_mode: PolicyMode::FullAutonomous,
            parent_policy_hooks: kroma_core::subagents::ParentPolicyHooks::KnownEmpty,
        },
        cancel_tx,
    )
    .unwrap();
    assert_eq!(
        registry.lock().unwrap().get("task-1").unwrap().state,
        TaskState::Running
    );

    let err = host
        .resume_task("task-1")
        .expect_err("a running task must not be resumed");
    assert!(
        err.contains("live run") || err.contains("already running"),
        "the refusal must explain the live run, got: {}",
        err
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "resuming a running task must not call the backend"
    );
    // The original run's canceller is still the live one and still works.
    assert!(host.cancel_task("task-1").unwrap());
}

/// A worker resume whose merge fails post-merge verification is recorded as `Failed`, not
/// `Completed` — an unverified result must never masquerade as success.
#[tokio::test]
async fn worker_resume_that_fails_the_completion_gate_is_not_completed() {
    let root = temp_root("resume-gate");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join("a.txt"), "hello").unwrap();
    let registry = open_registry(&root);
    let spec = SubagentSpec {
        role: SubagentRole::Worker,
        task: "write out.txt".into(),
        allowed_write_globs: None,
        budget: SubagentBudget {
            max_tokens: 200_000,
            timeout_ms: 5000,
            ..SubagentBudget::default()
        },
        revert_on_verify_failure: true,
        disallow_test_tampering: false,
        protected_paths: Vec::new(),
    };
    seed_suspended(&registry, &ws, "task-1", &spec, 200_000);

    let calls = Arc::new(AtomicUsize::new(0));
    let provider: Arc<dyn ModelProvider> = Arc::new(ScriptProvider {
        script: Arc::new({
            let calls = Arc::clone(&calls);
            move |_req: &ProviderRequest| {
                if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    Action::Turn(write_file("out.txt", "worker wrote this"))
                } else {
                    Action::Turn(stop("worker done"))
                }
            }
        }),
    });
    let (host, _env) = make_host(
        &root,
        &ws,
        provider,
        Arc::clone(&registry),
        Arc::new(RejectGate),
    );

    host.resume_task("task-1").unwrap();
    let state = wait_terminal(&registry, "task-1").await;
    assert_eq!(
        state,
        TaskState::Failed,
        "a gate-rejected worker merge must not be Completed"
    );
    let r = registry.lock().unwrap();
    let task = r.get("task-1").unwrap();
    assert!(
        task.notifications
            .iter()
            .any(|n| n.message.contains("verification")),
        "the failure must explain the verification rejection: {:?}",
        task.notifications
    );
    // The rejected change was rolled back out of the parent workspace.
    assert!(
        !ws.join("out.txt").exists(),
        "a reverted worker merge must leave the parent workspace unchanged"
    );
}

/// A successful worker resume merges its isolated delta, records the merged revision in the session
/// WAL (so `--undo` can revert it), and marks the task `Completed` only after the gate approved.
#[tokio::test]
async fn worker_resume_records_the_merge_in_the_session_wal() {
    let root = temp_root("resume-wal");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join("a.txt"), "hello").unwrap();
    let registry = open_registry(&root);
    let spec = SubagentSpec {
        role: SubagentRole::Worker,
        task: "write out.txt".into(),
        allowed_write_globs: None,
        budget: SubagentBudget {
            max_tokens: 200_000,
            timeout_ms: 5000,
            ..SubagentBudget::default()
        },
        revert_on_verify_failure: true,
        disallow_test_tampering: false,
        protected_paths: Vec::new(),
    };
    seed_suspended(&registry, &ws, "task-1", &spec, 200_000);

    let calls = Arc::new(AtomicUsize::new(0));
    let provider: Arc<dyn ModelProvider> = Arc::new(ScriptProvider {
        script: Arc::new({
            let calls = Arc::clone(&calls);
            move |_req: &ProviderRequest| {
                if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    Action::Turn(write_file("out.txt", "merged by resume"))
                } else {
                    Action::Turn(stop("worker done"))
                }
            }
        }),
    });
    let (host, _env) = make_host(
        &root,
        &ws,
        provider,
        Arc::clone(&registry),
        Arc::new(AllowAllGate),
    );

    host.resume_task("task-1").unwrap();
    assert_eq!(
        wait_terminal(&registry, "task-1").await,
        TaskState::Completed
    );
    assert_eq!(
        std::fs::read_to_string(ws.join("out.txt")).unwrap(),
        "merged by resume",
        "the approved worker delta must be merged into the parent workspace"
    );

    // The merged revision is recorded in the session WAL, so `--undo` can revert it.
    let journal = SessionJournal::open(host.session_path()).unwrap();
    let records = journal.revision_records();
    assert!(
        records
            .iter()
            .any(|r| r.changed_paths.iter().any(|p| p == "out.txt")),
        "the merge must be recorded in the session WAL, got {:?}",
        records
            .iter()
            .map(|r| (&r.revision_id, &r.changed_paths))
            .collect::<Vec<_>>()
    );
}

/// A completion gate that is *armed* (it would run a real command) and always approves, so a worker
/// merge records real verification evidence bound to the merged revision.
struct ArmedApproveGate;

impl CompletionGate for ArmedApproveGate {
    fn check<'a>(
        &'a self,
        _ctx: &'a CompletionGateContext,
    ) -> Pin<Box<dyn Future<Output = Result<GateOutcome, String>> + Send + 'a>> {
        Box::pin(async { Ok(GateOutcome::Approved) })
    }

    fn status(&self) -> CompletionGateStatus {
        CompletionGateStatus::Command {
            command: vec!["true".to_string()],
        }
    }
}

/// Seed a suspended worker that writes `out.txt` and drive a real resume with the given gate.
async fn resume_a_worker_that_writes_out_txt(
    tag: &str,
    gate: SharedCompletionGate,
) -> (Arc<Mutex<AsyncTaskRegistry>>, Arc<DurableTaskHost>) {
    let root = temp_root(tag);
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join("a.txt"), "hello").unwrap();
    let registry = open_registry(&root);
    let spec = SubagentSpec {
        role: SubagentRole::Worker,
        task: "write out.txt".into(),
        allowed_write_globs: None,
        budget: SubagentBudget {
            max_tokens: 200_000,
            timeout_ms: 5000,
            ..SubagentBudget::default()
        },
        revert_on_verify_failure: true,
        disallow_test_tampering: false,
        protected_paths: Vec::new(),
    };
    seed_suspended(&registry, &ws, "task-1", &spec, 200_000);

    let calls = Arc::new(AtomicUsize::new(0));
    let provider: Arc<dyn ModelProvider> = Arc::new(ScriptProvider {
        script: Arc::new({
            let calls = Arc::clone(&calls);
            move |_req: &ProviderRequest| {
                if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    Action::Turn(write_file("out.txt", "merged by resume"))
                } else {
                    Action::Turn(stop("worker done"))
                }
            }
        }),
    });
    let (host, _env) = make_host(&root, &ws, provider, Arc::clone(&registry), gate);
    host.resume_task("task-1").unwrap();
    (registry, host)
}

/// B12: a real worker resume whose armed post-merge gate approved records verification evidence
/// bound to the merged revision, but execution completion is **not** acceptance.
#[tokio::test]
async fn completed_worker_resume_is_verified_but_not_accepted() {
    let (registry, host) =
        resume_a_worker_that_writes_out_txt("accept-boundary", Arc::new(ArmedApproveGate)).await;
    assert_eq!(
        wait_terminal(&registry, "task-1").await,
        TaskState::Completed
    );

    let merged = SessionJournal::open(host.session_path())
        .unwrap()
        .revision_records()
        .into_iter()
        .find_map(|r| {
            r.changed_paths
                .iter()
                .any(|p| p == "out.txt")
                .then(|| r.revision_id.to_string())
        })
        .expect("the merge must be recorded in the session WAL");

    let record = registry.lock().unwrap().get_cloned("task-1").unwrap();
    let verification = record
        .verification
        .as_ref()
        .expect("an armed gate that approved must record verification evidence");
    assert!(
        verification.passed,
        "an approved armed gate must record passed=true"
    );
    assert_eq!(
        verification.revision.as_deref(),
        Some(merged.as_str()),
        "verification must be bound to the merged revision"
    );
    assert!(
        !record.is_accepted(),
        "a completed worker must not be accepted without an explicit trusted decision"
    );

    // The user-visible summary reports verified=true and accepted=false — never a false "accepted".
    let listed = registry
        .lock()
        .unwrap()
        .apply_control(&TaskControlRequest::List { session_id: None })
        .unwrap();
    let task = listed["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["task_id"].as_str() == Some("task-1"))
        .unwrap();
    assert_eq!(task["verified"], json!(true));
    assert_eq!(task["accepted"], json!(false));
}

/// B12: a disabled gate never records verification — the summary must say *unknown* (`null`), not a
/// fake `verified`, and the task is still not accepted.
#[tokio::test]
async fn a_disabled_gate_records_unknown_verification_not_verified() {
    let (registry, _host) =
        resume_a_worker_that_writes_out_txt("accept-unknown", Arc::new(AllowAllGate)).await;
    assert_eq!(
        wait_terminal(&registry, "task-1").await,
        TaskState::Completed
    );
    let record = registry.lock().unwrap().get_cloned("task-1").unwrap();
    assert!(
        record.verification.is_none(),
        "a disabled gate must record unknown verification, never a fake pass"
    );
    assert!(!record.is_accepted());

    let listed = registry
        .lock()
        .unwrap()
        .apply_control(&TaskControlRequest::List { session_id: None })
        .unwrap();
    let task = listed["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["task_id"].as_str() == Some("task-1"))
        .unwrap();
    assert_eq!(task["verified"], serde_json::Value::Null);
    assert_eq!(task["accepted"], json!(false));
}

/// A worker resume must honour the persisted `allowed_write_globs`: a delta outside the scope is a
/// conflict (not silently merged), so the task ends `Failed` and the parent workspace is untouched.
#[tokio::test]
async fn worker_resume_respects_the_persisted_write_scope() {
    let root = temp_root("resume-scope");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join("a.txt"), "hello").unwrap();
    let registry = open_registry(&root);
    let spec = SubagentSpec {
        role: SubagentRole::Worker,
        task: "write outside the scope".into(),
        allowed_write_globs: Some(vec!["src/**".to_string()]),
        budget: SubagentBudget {
            max_tokens: 200_000,
            timeout_ms: 5000,
            ..SubagentBudget::default()
        },
        revert_on_verify_failure: true,
        disallow_test_tampering: false,
        protected_paths: Vec::new(),
    };
    seed_suspended(&registry, &ws, "task-1", &spec, 200_000);

    let calls = Arc::new(AtomicUsize::new(0));
    let provider: Arc<dyn ModelProvider> = Arc::new(ScriptProvider {
        script: Arc::new({
            let calls = Arc::clone(&calls);
            move |_req: &ProviderRequest| {
                if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    // `bash` bypasses the write-tool scope hook, so only the merge-time
                    // `allowed_write_globs` check can stop this delta reaching the parent.
                    Action::Turn(bash("echo outside-scope > out.txt"))
                } else {
                    Action::Turn(stop("worker done"))
                }
            }
        }),
    });
    let (host, _env) = make_host(
        &root,
        &ws,
        provider,
        Arc::clone(&registry),
        Arc::new(AllowAllGate),
    );

    host.resume_task("task-1").unwrap();
    assert_eq!(wait_terminal(&registry, "task-1").await, TaskState::Failed);
    assert!(
        !ws.join("out.txt").exists(),
        "an out-of-scope worker delta must not be merged"
    );
}

/// B12 seam: a resumed worker whose parent workspace changed the *same path* after the worktree was
/// materialized must be refused at the shared merge chokepoint (`compute_merge_delta`), not silently
/// merged. This pins the resume-path half of the "parent revision changed -> re-validate before
/// merge" invariant, which was only exercised on the first-run (`gate5`) and tournament (`gate8`)
/// paths.
#[tokio::test]
async fn worker_resume_refuses_a_parent_path_changed_after_materialize() {
    let root = temp_root("resume-parent-conflict");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join("shared.txt"), "base\n").unwrap();
    let registry = open_registry(&root);

    let spec = SubagentSpec {
        role: SubagentRole::Worker,
        task: "rewrite shared.txt".into(),
        allowed_write_globs: None,
        budget: SubagentBudget {
            max_tokens: 200_000,
            timeout_ms: 5000,
            ..SubagentBudget::default()
        },
        revert_on_verify_failure: true,
        disallow_test_tampering: false,
        protected_paths: Vec::new(),
    };
    // Seed with a legal mutating parent policy (`AllowWrite`) and known-empty hooks, so the only
    // thing that can fail the resume is the merge conflict itself (not authorization/budget/refusal).
    {
        let mut r = registry.lock().unwrap();
        r.register(NewTask {
            task_id: "task-1".to_string(),
            session_id: "sess-1".to_string(),
            owner_id: "owner-1".to_string(),
            workspace_root: ws.display().to_string(),
            role: spec.role.as_str().to_string(),
            created_turn: 0,
            token_allocation: 200_000,
            prompt: spec.task.clone(),
            timeout_ms: spec.budget.timeout_ms,
            spec_json: serde_json::to_string(&spec).unwrap(),
            parent_policy: serde_json::to_string(&PolicyMode::AllowWrite).unwrap(),
            parent_policy_hooks: kroma_core::subagents::ParentPolicyHooks::KnownEmpty,
            session_path: String::new(),
        })
        .unwrap();
        r.transition("task-1", TaskState::Running, 1).unwrap();
        r.suspend("task-1", 1).unwrap();
    }

    let calls = Arc::new(AtomicUsize::new(0));
    let child_started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let provider: Arc<dyn ModelProvider> = Arc::new(ScriptProvider {
        script: Arc::new({
            let calls = Arc::clone(&calls);
            let child_started = Arc::clone(&child_started);
            let release = Arc::clone(&release);
            move |_req: &ProviderRequest| match calls.fetch_add(1, Ordering::SeqCst) {
                0 => {
                    child_started.notify_one();
                    Action::Turn(write_file("shared.txt", "worker\n"))
                }
                _ => Action::AwaitRelease(Arc::clone(&release)),
            }
        }),
    });
    let (host, _env) = make_host(
        &root,
        &ws,
        provider,
        Arc::clone(&registry),
        Arc::new(AllowAllGate),
    );

    host.resume_task("task-1").unwrap();

    // The worktree is materialized before the child's first provider call, so once the child starts
    // its base revision is fixed. Change the same path in the parent *after* that point, then let the
    // child finish and attempt its merge.
    child_started.notified().await;
    std::fs::write(ws.join("shared.txt"), "parent modified\n").unwrap();
    release.notify_one();

    assert_eq!(
        wait_terminal(&registry, "task-1").await,
        TaskState::Failed,
        "a parent path changed after materialize must fail the resume, not merge"
    );
    assert!(
        calls.load(Ordering::SeqCst) >= 1,
        "the resumed worker must actually have run (provider executed)"
    );
    // The parent's post-materialize bytes survive: the worker delta was never applied.
    assert_eq!(
        std::fs::read_to_string(ws.join("shared.txt")).unwrap(),
        "parent modified\n",
        "the conflicting worker delta must not overwrite the parent"
    );
    let record = registry.lock().unwrap().get_cloned("task-1").unwrap();
    let summary = record
        .notifications
        .last()
        .map(|n| n.message.clone())
        .unwrap_or_default();
    assert!(
        summary.contains("merge conflict")
            && summary.contains("shared.txt")
            && summary.contains("parent workspace modified"),
        "the failure must be the parent-path merge conflict, got: {}",
        summary
    );
    assert!(
        record.pending_merge.is_none(),
        "a refused merge must not strand a pending merge intent"
    );
    let journal = SessionJournal::open(host.session_path()).unwrap();
    assert!(
        journal.revision_records().is_empty(),
        "a refused merge must not link a revision into the session WAL"
    );
}

/// B12: when the parent workspace changes on a path the worker did *not* touch (an un-snapshotted
/// external write that does not advance the snapshot id), the post-merge gate must re-verify the
/// **merged parent** content, not the worker's isolated worktree. The gate passes only if the
/// parent's concurrent change is visible where the gate actually runs.
#[tokio::test]
async fn a_parent_non_conflicting_change_is_re_verified_on_the_merged_workspace() {
    let root = temp_root("reverify-parent");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join("a.txt"), "hello").unwrap();
    std::fs::write(ws.join("parent.txt"), "before\n").unwrap();
    let registry = open_registry(&root);
    let spec = SubagentSpec {
        role: SubagentRole::Worker,
        task: "write child.txt".into(),
        allowed_write_globs: None,
        budget: SubagentBudget {
            max_tokens: 200_000,
            timeout_ms: 5000,
            ..SubagentBudget::default()
        },
        revert_on_verify_failure: true,
        disallow_test_tampering: false,
        protected_paths: Vec::new(),
    };
    seed_suspended(&registry, &ws, "task-1", &spec, 200_000);

    let calls = Arc::new(AtomicUsize::new(0));
    let child_started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let provider: Arc<dyn ModelProvider> = Arc::new(ScriptProvider {
        script: Arc::new({
            let calls = Arc::clone(&calls);
            let child_started = Arc::clone(&child_started);
            let release = Arc::clone(&release);
            move |_req: &ProviderRequest| match calls.fetch_add(1, Ordering::SeqCst) {
                0 => {
                    child_started.notify_one();
                    Action::Turn(write_file("child.txt", "worker\n"))
                }
                _ => Action::AwaitRelease(Arc::clone(&release)),
            }
        }),
    });

    // A real armed `WorkspaceCompletionGate` whose command inspects the workspace it runs in.
    let gate: SharedCompletionGate = Arc::new(WorkspaceCompletionGate::new(
        Some(vec![
            "sh".to_string(),
            "-c".to_string(),
            "grep -q PARENT_MARKER parent.txt".to_string(),
        ]),
        Duration::from_secs(10),
    ));
    let (host, _env) = make_host(&root, &ws, provider, Arc::clone(&registry), gate);

    host.resume_task("task-1").unwrap();
    child_started.notified().await;
    // External, un-snapshotted, non-conflicting parent write: the snapshot id does NOT advance.
    std::fs::write(ws.join("parent.txt"), "PARENT_MARKER\n").unwrap();
    release.notify_one();

    assert_eq!(
        wait_terminal(&registry, "task-1").await,
        TaskState::Completed
    );
    let record = registry.lock().unwrap().get_cloned("task-1").unwrap();
    let verification = record
        .verification
        .as_ref()
        .expect("the armed gate must have run on the merged workspace");
    assert!(verification.passed);
    assert_eq!(
        std::fs::read_to_string(ws.join("parent.txt")).unwrap(),
        "PARENT_MARKER\n",
        "the parent's concurrent change must be present in the merged workspace"
    );
    assert_eq!(
        std::fs::read_to_string(ws.join("child.txt")).unwrap(),
        "worker\n",
        "the worker delta must be merged too"
    );
    // No WAL revision existed at resume time: the parent snapshot identity is honestly unknown and
    // is never faked into a `Some`.
    assert!(verification.parent_snapshot_revision.is_none());
    assert!(!record.is_accepted());
}

/// B12: with a disabled gate, a parent change must not be reported as verified/accepted.
#[tokio::test]
async fn a_parent_change_with_a_disabled_gate_is_not_reported_verified() {
    let root = temp_root("reverify-disabled");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join("a.txt"), "hello").unwrap();
    std::fs::write(ws.join("parent.txt"), "before\n").unwrap();
    let registry = open_registry(&root);
    let spec = SubagentSpec {
        role: SubagentRole::Worker,
        task: "write child.txt".into(),
        allowed_write_globs: None,
        budget: SubagentBudget {
            max_tokens: 200_000,
            timeout_ms: 5000,
            ..SubagentBudget::default()
        },
        revert_on_verify_failure: true,
        disallow_test_tampering: false,
        protected_paths: Vec::new(),
    };
    seed_suspended(&registry, &ws, "task-1", &spec, 200_000);

    let calls = Arc::new(AtomicUsize::new(0));
    let child_started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let provider: Arc<dyn ModelProvider> = Arc::new(ScriptProvider {
        script: Arc::new({
            let calls = Arc::clone(&calls);
            let child_started = Arc::clone(&child_started);
            let release = Arc::clone(&release);
            move |_req: &ProviderRequest| match calls.fetch_add(1, Ordering::SeqCst) {
                0 => {
                    child_started.notify_one();
                    Action::Turn(write_file("child.txt", "worker\n"))
                }
                _ => Action::AwaitRelease(Arc::clone(&release)),
            }
        }),
    });
    let (host, _env) = make_host(
        &root,
        &ws,
        provider,
        Arc::clone(&registry),
        Arc::new(AllowAllGate),
    );

    host.resume_task("task-1").unwrap();
    child_started.notified().await;
    std::fs::write(ws.join("parent.txt"), "PARENT_MARKER\n").unwrap();
    release.notify_one();

    assert_eq!(
        wait_terminal(&registry, "task-1").await,
        TaskState::Completed
    );
    let record = registry.lock().unwrap().get_cloned("task-1").unwrap();
    assert!(
        record.verification.is_none(),
        "a disabled gate must not report the parent-changed merge as verified"
    );
    assert!(
        !record.is_accepted(),
        "a disabled gate must not report the merge as accepted"
    );
}

/// B12: when the session WAL already names a real parent snapshot revision (one advanced past the
/// worker's materialized base), the resume must record that id as the parent snapshot identity it
/// re-verified against, and the merged revision's parent chain must point at it. The parent's
/// non-conflicting change is applied *after* the worker materializes its base, so the gate can only
/// pass by running on the merged parent content.
#[tokio::test]
async fn a_parent_snapshot_advanced_before_resume_is_recorded_and_re_verified() {
    let root = temp_root("reverify-some-id");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join("a.txt"), "hello").unwrap();
    std::fs::write(ws.join("parent.txt"), "before\n").unwrap();
    let registry = open_registry(&root);
    let spec = SubagentSpec {
        role: SubagentRole::Worker,
        task: "write child.txt".into(),
        allowed_write_globs: None,
        budget: SubagentBudget {
            max_tokens: 200_000,
            timeout_ms: 5000,
            ..SubagentBudget::default()
        },
        revert_on_verify_failure: true,
        disallow_test_tampering: false,
        protected_paths: Vec::new(),
    };
    seed_suspended(&registry, &ws, "task-1", &spec, 200_000);

    let calls = Arc::new(AtomicUsize::new(0));
    let child_started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let provider: Arc<dyn ModelProvider> = Arc::new(ScriptProvider {
        script: Arc::new({
            let calls = Arc::clone(&calls);
            let child_started = Arc::clone(&child_started);
            let release = Arc::clone(&release);
            move |_req: &ProviderRequest| match calls.fetch_add(1, Ordering::SeqCst) {
                0 => {
                    child_started.notify_one();
                    Action::Turn(write_file("child.txt", "worker\n"))
                }
                _ => Action::AwaitRelease(Arc::clone(&release)),
            }
        }),
    });
    // A real armed gate that checks BOTH the parent's new marker and the worker's delta in the
    // workspace it runs in.
    let gate: SharedCompletionGate = Arc::new(WorkspaceCompletionGate::new(
        Some(vec![
            "sh".to_string(),
            "-c".to_string(),
            "grep -q PARENT_MARKER parent.txt && grep -q worker child.txt".to_string(),
        ]),
        Duration::from_secs(10),
    ));
    let (host, env) = make_host(&root, &ws, provider, Arc::clone(&registry), gate);

    // Real parent snapshot revisions, captured and recorded through the production APIs (no
    // hand-forged revision strings).
    let store = SnapshotStore::new(root.join("state/snapshots"));
    let r0 = store
        .capture_revision(env.workspace(), None, None, 1)
        .unwrap();
    {
        let mut j = SessionJournal::open(host.session_path()).unwrap();
        j.record_revision_capture(
            1,
            r0.revision_id.as_str(),
            None,
            vec!["a.txt".to_string(), "parent.txt".to_string()],
            2,
        )
        .unwrap();
    }
    // Advance the parent on a non-conflicting path and capture a NEW parent snapshot id.
    std::fs::write(ws.join("parent.txt"), "PARENT_MARKER\n").unwrap();
    let r1 = store
        .capture_revision(env.workspace(), Some(&r0.revision_id), None, 2)
        .unwrap();
    {
        let mut j = SessionJournal::open(host.session_path()).unwrap();
        j.record_revision_capture(
            2,
            r1.revision_id.as_str(),
            Some(r0.revision_id.as_str()),
            vec!["parent.txt".to_string()],
            2,
        )
        .unwrap();
    }
    assert_ne!(r0.revision_id, r1.revision_id);
    // Put the pre-change content back so the worker's materialized base does not already carry the
    // parent's new marker; the parent re-applies it *after* materialization.
    std::fs::write(ws.join("parent.txt"), "before\n").unwrap();

    host.resume_task("task-1").unwrap();
    child_started.notified().await;
    std::fs::write(ws.join("parent.txt"), "PARENT_MARKER\n").unwrap();
    release.notify_one();

    assert_eq!(
        wait_terminal(&registry, "task-1").await,
        TaskState::Completed
    );
    assert!(
        calls.load(Ordering::SeqCst) >= 1,
        "the provider must actually have run"
    );

    let record = registry.lock().unwrap().get_cloned("task-1").unwrap();
    let verification = record
        .verification
        .as_ref()
        .expect("the armed gate must have run on the merged workspace");
    assert!(verification.passed, "the gate must have run and approved");
    assert_eq!(
        verification.parent_snapshot_revision.as_deref(),
        Some(r1.revision_id.as_str()),
        "the recorded parent snapshot id must be the real advanced one"
    );
    assert_ne!(
        verification.parent_snapshot_revision.as_deref(),
        Some(r0.revision_id.as_str()),
        "it must be the NEW id, not the baseline"
    );
    assert!(!record.is_accepted());

    // Exactly one merge is linked, and it is the revision the gate verified; its parent chain
    // reaches the new parent snapshot id.
    let journal = SessionJournal::open(host.session_path()).unwrap();
    let merge_records: Vec<_> = journal
        .revision_records()
        .into_iter()
        .filter(|r| r.changed_paths.iter().any(|p| p == "child.txt"))
        .collect();
    assert_eq!(
        merge_records.len(),
        1,
        "exactly one merge must be linked (no duplicate)"
    );
    let merged_id = merge_records[0].revision_id.clone();
    assert_eq!(verification.revision.as_deref(), Some(merged_id.as_str()));

    let merged_rev = store
        .load_revision(&RevisionId::parse(&merged_id).unwrap())
        .unwrap();
    assert_eq!(
        merged_rev.parent_revision_id.as_ref(),
        Some(&r1.revision_id),
        "the merged revision's parent must be the new parent snapshot id"
    );
}

/// A worker spec that writes `out.txt`, for the session/WAL-linkage regressions.
fn worker_spec(task: &str) -> SubagentSpec {
    SubagentSpec {
        role: SubagentRole::Worker,
        task: task.into(),
        allowed_write_globs: None,
        budget: SubagentBudget {
            max_tokens: 200_000,
            timeout_ms: 5000,
            ..SubagentBudget::default()
        },
        revert_on_verify_failure: true,
        disallow_test_tampering: false,
        protected_paths: Vec::new(),
    }
}

/// A worker whose scripted provider writes one file, then stops.
fn worker_provider(
    calls: Arc<AtomicUsize>,
    path: &'static str,
    body: &'static str,
) -> Arc<dyn ModelProvider> {
    Arc::new(ScriptProvider {
        script: Arc::new(move |_req: &ProviderRequest| {
            if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                Action::Turn(write_file(path, body))
            } else {
                Action::Turn(stop("worker done"))
            }
        }),
    })
}

/// A declared `report` artifact must have durable bytes: the full summary is stored privately with a
/// matching digest *before* the finish metadata is committed, and it can be read back after a
/// restart. The registry notification stays bounded at 512 bytes, so the report file is the only
/// place the full bytes exist.
#[tokio::test]
async fn report_artifact_carries_durable_bytes_that_survive_a_restart() {
    let root = temp_root("report");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join("a.txt"), "hello").unwrap();
    let registry = open_registry(&root);
    let expected = format!("{}END-MARKER", "R".repeat(700));
    let calls = Arc::new(AtomicUsize::new(0));
    let provider: Arc<dyn ModelProvider> = Arc::new(ScriptProvider {
        script: Arc::new({
            let calls = Arc::clone(&calls);
            let expected = expected.clone();
            move |req: &ProviderRequest| {
                if is_child(req) {
                    Action::Turn(stop(&expected))
                } else if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    Action::Turn(spawn_scout("produce a long report"))
                } else {
                    Action::Turn(stop("parent done"))
                }
            }
        }),
    });
    let (host, env) = make_host(
        &root,
        &ws,
        provider.clone(),
        Arc::clone(&registry),
        Arc::new(AllowAllGate),
    );
    run_once(&env, provider.clone(), Arc::clone(&host), "spawn a scout").await;

    let (task_id, artifact, notification) = {
        let tasks = registry.lock().unwrap();
        let task = tasks.tasks_for_session("sess-1").remove(0);
        let artifact = task
            .artifacts
            .iter()
            .find(|a| a.kind == "report")
            .expect("the task must carry a report artifact")
            .clone();
        let notification = task
            .notifications
            .iter()
            .find(|n| n.kind == "completed")
            .expect("the task must notify completion")
            .message
            .clone();
        (task.task_id.clone(), artifact, notification)
    };
    assert!(
        artifact.bytes > 512,
        "the report must hold the full summary, got {} bytes",
        artifact.bytes
    );
    assert!(
        artifact.location.starts_with("file:reports/"),
        "the artifact must name durable bytes, got {}",
        artifact.location
    );
    assert_eq!(
        artifact.sha256.as_deref(),
        Some(kroma_core::prompt::sha256_hex64(expected.as_bytes()).as_str()),
        "the recorded digest must be the digest of the stored bytes"
    );
    assert!(
        notification.len() <= 512 && !notification.contains("END-MARKER"),
        "the registry notification is bounded and does not carry the full report: {}",
        notification.len()
    );
    assert_eq!(
        host.read_report(&task_id).unwrap(),
        expected,
        "the report must resolve to the full bytes before a restart"
    );

    // A restart: reopen the registry and build a fresh host over the same state directory.
    let reopened = Arc::new(Mutex::new(
        AsyncTaskRegistry::open(root.join("state/tasks/registry.json")).unwrap(),
    ));
    let (host2, _env2) = make_host(&root, &ws, provider, reopened, Arc::new(AllowAllGate));
    assert_eq!(
        host2.read_report(&task_id).unwrap(),
        expected,
        "the durable report must still resolve, with a matching hash, after a restart"
    );
}

/// Switching sessions must not redirect an already-admitted task's WAL: the record keeps the session
/// context captured at its own admission, while a *new* admission uses the current binding.
#[tokio::test]
async fn a_session_switch_does_not_redirect_an_admitted_tasks_wal() {
    let root = temp_root("session-switch");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join("a.txt"), "hello").unwrap();
    let sessions = root.join("state/sessions");
    let journal_a = SessionJournal::create(&sessions, &ws).unwrap();
    let path_a = journal_a.path().to_path_buf();
    drop(journal_a);
    let journal_b = SessionJournal::create(&sessions, &ws).unwrap();
    let path_b = journal_b.path().to_path_buf();
    drop(journal_b);

    let registry = open_registry(&root);
    let spec = worker_spec("write out.txt");
    seed_suspended_in_session(
        &registry,
        &ws,
        "task-A",
        &spec,
        200_000,
        "sess-A",
        path_a.to_str().unwrap(),
    );

    let calls = Arc::new(AtomicUsize::new(0));
    let provider = worker_provider(Arc::clone(&calls), "out.txt", "merged by A");
    let (host, _env) = make_host_bound_to(
        &root,
        &ws,
        provider,
        Arc::clone(&registry),
        Arc::new(AllowAllGate),
        "sess-A",
        path_a.clone(),
    );

    // The dispatcher switches to session B before the task is resumed.
    host.rebind_session("sess-B".to_string(), "owner-1".to_string(), path_b.clone());
    assert_eq!(host.session_path(), path_b, "new admissions bind to B");

    // A new admission uses the current binding...
    let (cancel_tx, _cancel_rx) = watch::channel(false);
    host.register_child(
        &kroma_core::task_host::ChildTaskSpec {
            task_id: "task-B".to_string(),
            spec: worker_spec("new work"),
            allocation: 1,
            turn: 0,
            parent_policy_mode: PolicyMode::FullAutonomous,
            parent_policy_hooks: kroma_core::subagents::ParentPolicyHooks::KnownEmpty,
        },
        cancel_tx,
    )
    .unwrap();
    {
        let tasks = registry.lock().unwrap();
        assert_eq!(
            tasks.get("task-B").unwrap().session_path,
            path_b.display().to_string(),
            "a new admission must record the current session WAL"
        );
        // ...while the already-admitted task keeps its own.
        assert_eq!(
            tasks.get("task-A").unwrap().session_path,
            path_a.display().to_string(),
            "an admitted task must keep the session context captured at its admission"
        );
    }

    // Resuming A links its merge into A's WAL, not the current session's.
    host.resume_task("task-A").unwrap();
    assert_eq!(
        wait_terminal(&registry, "task-A").await,
        TaskState::Completed
    );
    let records_a = SessionJournal::open(&path_a).unwrap().revision_records();
    assert!(
        records_a
            .iter()
            .any(|r| r.changed_paths.iter().any(|p| p == "out.txt")),
        "the merge must be linked into the admitting session's WAL, got {:?}",
        records_a
            .iter()
            .map(|r| (&r.revision_id, &r.changed_paths))
            .collect::<Vec<_>>()
    );
    let records_b = SessionJournal::open(&path_b).unwrap().revision_records();
    assert!(
        records_b.is_empty(),
        "the current session's WAL must not be touched by another session's task, got {:?}",
        records_b.iter().map(|r| &r.revision_id).collect::<Vec<_>>()
    );
}

/// A worker resume whose session WAL is locked must be refused *before* the parent workspace is
/// mutated, and must leave the task resumable — never a silent `Completed` with a lost undo link.
#[tokio::test]
async fn worker_resume_with_a_locked_session_wal_is_refused_before_merging() {
    let root = temp_root("wal-locked");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join("a.txt"), "hello").unwrap();
    let journal = SessionJournal::create(root.join("state/sessions"), &ws).unwrap();
    let session_path = journal.path().to_path_buf();
    drop(journal);

    let registry = open_registry(&root);
    let spec = worker_spec("write out.txt");
    seed_suspended_in_session(
        &registry,
        &ws,
        "task-1",
        &spec,
        200_000,
        "sess-1",
        session_path.to_str().unwrap(),
    );

    let calls = Arc::new(AtomicUsize::new(0));
    let provider = worker_provider(Arc::clone(&calls), "out.txt", "must not land");
    let (host, _env) = make_host_bound_to(
        &root,
        &ws,
        provider,
        Arc::clone(&registry),
        Arc::new(AllowAllGate),
        "sess-1",
        session_path.clone(),
    );

    // A concurrent conversation holds the WAL.
    let held = SessionJournal::open(&session_path).unwrap();
    let err = host
        .resume_task("task-1")
        .expect_err("a locked session WAL must refuse the resume");
    assert!(
        err.contains("WAL") || err.contains("unavailable"),
        "the refusal must explain the WAL conflict, got: {}",
        err
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "the backend must not run when the WAL link cannot be taken"
    );
    assert!(
        !ws.join("out.txt").exists(),
        "a refused resume must not mutate the parent workspace"
    );
    assert_eq!(
        registry.lock().unwrap().get("task-1").unwrap().state,
        TaskState::Suspended,
        "the task must remain resumable after a refused resume"
    );

    // Releasing the WAL lets the resume proceed and link the merge.
    drop(held);
    host.resume_task("task-1").unwrap();
    assert_eq!(
        wait_terminal(&registry, "task-1").await,
        TaskState::Completed
    );
    assert_eq!(
        std::fs::read_to_string(ws.join("out.txt")).unwrap(),
        "must not land"
    );
    let records = SessionJournal::open(&session_path)
        .unwrap()
        .revision_records();
    assert!(
        records
            .iter()
            .any(|r| r.changed_paths.iter().any(|p| p == "out.txt")),
        "the successful resume must link its merge into the WAL"
    );
}

/// A persistence failure during settlement or cancel must be propagated, not reported as success:
/// no phantom completion, no charged spend, no fake terminal notification, and the live canceller
/// binding is still released safely.
#[tokio::test]
async fn finish_and_cancel_persistence_faults_are_propagated() {
    let root = temp_root("settle-fault");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join("a.txt"), "hello").unwrap();
    let registry = open_registry(&root);

    let spec = SubagentSpec {
        role: SubagentRole::Scout,
        task: "settle me".into(),
        allowed_write_globs: None,
        budget: SubagentBudget {
            max_tokens: 200_000,
            timeout_ms: 5000,
            ..SubagentBudget::default()
        },
        revert_on_verify_failure: false,
        disallow_test_tampering: false,
        protected_paths: Vec::new(),
    };
    let (host, _env) = make_host(
        &root,
        &ws,
        Arc::new(ScriptProvider {
            script: Arc::new(|_req: &ProviderRequest| Action::Turn(stop("unused"))),
        }),
        Arc::clone(&registry),
        Arc::new(AllowAllGate),
    );
    for task_id in ["task-1", "task-2"] {
        let (cancel_tx, _cancel_rx) = watch::channel(false);
        host.register_child(
            &kroma_core::task_host::ChildTaskSpec {
                task_id: task_id.to_string(),
                spec: spec.clone(),
                allocation: 100,
                turn: 0,
                parent_policy_mode: PolicyMode::FullAutonomous,
                parent_policy_hooks: kroma_core::subagents::ParentPolicyHooks::KnownEmpty,
            },
            cancel_tx,
        )
        .unwrap();
    }

    // Break the registry backing store without touching the private report directory, so every
    // registry commit now fails before anything is visible (a real, permission-independent fault).
    let tasks_dir = root.join("state/tasks");
    std::fs::remove_dir_all(&tasks_dir).unwrap();
    std::fs::write(&tasks_dir, b"not a directory").unwrap();

    let err = host
        .finish_child(
            "task-1",
            &kroma_core::task_host::ChildFinish {
                state: TaskState::Completed,
                spent: 40,
                summary: "the child really finished".into(),
                keep_pending_merge: false,
                verification: None,
            },
        )
        .expect_err("a failed settlement commit must be an error");
    assert!(!err.is_empty(), "the failure must carry a reason");
    {
        let tasks = registry.lock().unwrap();
        let task = tasks.get("task-1").unwrap();
        assert_eq!(task.state, TaskState::Running, "no phantom completion");
        assert_eq!(task.token_spent, 0, "no phantom spend");
        assert!(
            !task.notifications.iter().any(|n| n.kind == "completed"),
            "no fake terminal notification: {:?}",
            task.notifications
        );
    }
    assert!(
        !host.has_live_run("task-1"),
        "the canceller binding must be released once the run is over"
    );

    let err = host
        .cancel_task("task-2")
        .expect_err("a failed durable cancel must be an error");
    assert!(!err.is_empty(), "the failure must carry a reason");
    assert_eq!(
        registry.lock().unwrap().get("task-2").unwrap().state,
        TaskState::Running,
        "a failed durable cancel must not claim the task is cancelled"
    );
    assert!(
        host.has_live_run("task-2"),
        "a failed durable cancel must keep the live binding so the child can still be stopped"
    );
}

/// A crash between a worker merge and its WAL append must be replayed from the durable,
/// session-bound merge intent, and the replay must be idempotent (no duplicate WAL line).
#[tokio::test]
async fn a_pending_merge_intent_is_replayed_idempotently() {
    let root = temp_root("merge-replay");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join("out.txt"), "merged by resume").unwrap();
    let journal = SessionJournal::create(root.join("state/sessions"), &ws).unwrap();
    let session_path = journal.path().to_path_buf();
    drop(journal);

    let store = SnapshotStore::new(root.join("state/snapshots"));
    let env: Arc<dyn ExecutionEnv> = Arc::new(LocalHostEnv::new(&ws, root.join("state")).unwrap());
    let merged = store
        .capture_revision(env.workspace(), None, None, 3)
        .unwrap();
    let revision_id = merged.revision_id.as_str().to_string();

    let registry = open_registry(&root);
    let seed_intent = |registry: &Arc<Mutex<AsyncTaskRegistry>>| {
        let mut r = registry.lock().unwrap();
        r.set_pending_merge(
            "task-1",
            PendingMergeLink {
                session_id: "sess-1".into(),
                session_path: session_path.display().to_string(),
                turn: 3,
                parent_revision: None,
                merged_revision: Some(revision_id.clone()),
                changed_paths: merged.changed_paths.clone(),
                tracked_files: merged.tracked_files.len(),
            },
        )
        .unwrap();
    };
    {
        let mut r = registry.lock().unwrap();
        r.register(NewTask {
            task_id: "task-1".into(),
            session_id: "sess-1".into(),
            owner_id: "owner-1".into(),
            workspace_root: ws.display().to_string(),
            role: "worker".into(),
            created_turn: 0,
            token_allocation: 100,
            prompt: "p".into(),
            timeout_ms: 0,
            spec_json: "{}".into(),
            parent_policy: "allow-write".into(),
            parent_policy_hooks: kroma_core::subagents::ParentPolicyHooks::KnownEmpty,
            session_path: session_path.display().to_string(),
        })
        .unwrap();
        r.transition("task-1", TaskState::Running, 1).unwrap();
    }
    seed_intent(&registry);

    let (host, _env) = make_host_bound_to(
        &root,
        &ws,
        Arc::new(ScriptProvider {
            script: Arc::new(|_req: &ProviderRequest| Action::Turn(stop("unused"))),
        }),
        Arc::clone(&registry),
        Arc::new(AllowAllGate),
        "sess-1",
        session_path.clone(),
    );

    let report = host.replay_pending_merges().unwrap();
    assert_eq!(report.linked, 1);
    assert!(
        report.unresolved.is_empty(),
        "a replayable intent must not be reported unresolved"
    );
    assert!(
        registry.lock().unwrap().pending_merges().is_empty(),
        "a replayed intent must be cleared"
    );
    let count = |path: &std::path::Path| {
        SessionJournal::open(path)
            .unwrap()
            .revision_records()
            .iter()
            .filter(|r| r.revision_id == revision_id)
            .count()
    };
    assert_eq!(count(&session_path), 1, "the merge must be linked once");

    // A second crash leaves the same intent again; the replay must not duplicate the WAL line.
    seed_intent(&registry);
    host.replay_pending_merges().unwrap();
    assert_eq!(
        count(&session_path),
        1,
        "replaying an already-recorded revision must not append a duplicate"
    );
}

/// A completion gate that approves, but first sabotages the task registry so every later registry
/// commit fails before anything is visible. It lets a test inject a failure *after* the merge has
/// already been applied and captured (the gate runs after the merge's own journal is cleared).
struct SabotageRegistryGate {
    tasks_dir: PathBuf,
    sabotage: Arc<AtomicBool>,
}

impl CompletionGate for SabotageRegistryGate {
    fn check<'a>(
        &'a self,
        _ctx: &'a CompletionGateContext,
    ) -> Pin<Box<dyn Future<Output = Result<GateOutcome, String>> + Send + 'a>> {
        Box::pin(async move {
            if !self.sabotage.swap(true, Ordering::SeqCst) {
                let backup = self.tasks_dir.with_extension("sabotaged");
                let _ = std::fs::remove_dir_all(&backup);
                std::fs::rename(&self.tasks_dir, &backup).expect("move the registry dir aside");
                std::fs::write(&self.tasks_dir, b"not a directory")
                    .expect("block the registry dir");
            }
            Ok(GateOutcome::Approved)
        })
    }
}

/// A scout spec for id-reuse regressions.
fn scout_spec(task: &str) -> SubagentSpec {
    SubagentSpec {
        role: SubagentRole::Scout,
        task: task.into(),
        allowed_write_globs: None,
        budget: SubagentBudget {
            max_tokens: 200_000,
            timeout_ms: 5000,
            ..SubagentBudget::default()
        },
        revert_on_verify_failure: false,
        disallow_test_tampering: false,
        protected_paths: Vec::new(),
    }
}

/// P1-a: a capacity event must never evict the only durable copy of an unreplayed merge link.
#[tokio::test]
async fn capacity_pressure_keeps_a_pending_merge_record_and_still_admits() {
    let root = temp_root("capacity");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join("a.txt"), "hello").unwrap();
    let registry = open_registry(&root);
    {
        let mut r = registry.lock().unwrap();
        let fill = |r: &mut AsyncTaskRegistry, id: &str, turn: u32| {
            r.register(NewTask {
                task_id: id.to_string(),
                session_id: "sess-1".to_string(),
                owner_id: "owner-1".to_string(),
                workspace_root: ws.display().to_string(),
                role: "scout".to_string(),
                created_turn: 0,
                token_allocation: 10,
                prompt: "p".to_string(),
                timeout_ms: 0,
                spec_json: "{}".to_string(),
                parent_policy: "allow-write".to_string(),
                parent_policy_hooks: kroma_core::subagents::ParentPolicyHooks::KnownEmpty,
                session_path: String::new(),
            })
            .unwrap();
            r.transition(id, TaskState::Running, turn).unwrap();
            r.fail(id, "done", turn).unwrap();
        };
        // The protected record is the oldest terminal record, so a naive prune would evict it first.
        fill(&mut r, "protected", 0);
        r.set_pending_merge(
            "protected",
            PendingMergeLink {
                session_id: "sess-1".into(),
                session_path: "/w/session.wal.jsonl".into(),
                turn: 0,
                parent_revision: None,
                merged_revision: Some("r-abc".into()),
                changed_paths: vec!["out.txt".into()],
                tracked_files: 1,
            },
        )
        .unwrap();
        for i in 0..(kroma_core::async_tasks::MAX_TASKS - 1) {
            fill(&mut r, &format!("filler-{:03}", i), 1);
        }
        assert_eq!(r.len(), kroma_core::async_tasks::MAX_TASKS);
    }

    let (host, _env) = make_host(
        &root,
        &ws,
        Arc::new(ScriptProvider {
            script: Arc::new(|_req: &ProviderRequest| Action::Turn(stop("unused"))),
        }),
        Arc::clone(&registry),
        Arc::new(AllowAllGate),
    );
    let (cancel_tx, _cancel_rx) = watch::channel(false);
    let durable_id = host
        .register_child(
            &kroma_core::task_host::ChildTaskSpec {
                task_id: "new-child".to_string(),
                spec: scout_spec("new work"),
                allocation: 10,
                turn: 0,
                parent_policy_mode: PolicyMode::FullAutonomous,
                parent_policy_hooks: kroma_core::subagents::ParentPolicyHooks::KnownEmpty,
            },
            cancel_tx,
        )
        .expect("capacity pressure with an evictable terminal record must admit the child");
    assert_eq!(durable_id, "new-child");

    let r = registry.lock().unwrap();
    assert!(
        r.get("protected").is_some(),
        "a capacity prune must never evict a record with a pending merge intent"
    );
    assert_eq!(r.pending_merges().len(), 1);
    assert!(
        r.get("new-child").is_some(),
        "the new admission must be recorded"
    );
}

/// P1-b: an intent that carries no merged revision can never be replayed, so it must be reported and
/// cleared instead of being skipped forever as a permanent orphan.
#[tokio::test]
async fn an_intent_without_a_merged_revision_is_reported_and_cleared_not_orphaned() {
    let root = temp_root("none-intent");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join("a.txt"), "hello").unwrap();
    let journal = SessionJournal::create(root.join("state/sessions"), &ws).unwrap();
    let session_path = journal.path().to_path_buf();
    drop(journal);
    let registry = open_registry(&root);
    {
        let mut r = registry.lock().unwrap();
        r.register(NewTask {
            task_id: "task-1".to_string(),
            session_id: "sess-1".to_string(),
            owner_id: "owner-1".to_string(),
            workspace_root: ws.display().to_string(),
            role: "worker".to_string(),
            created_turn: 0,
            token_allocation: 10,
            prompt: "p".to_string(),
            timeout_ms: 0,
            spec_json: "{}".to_string(),
            parent_policy: "allow-write".to_string(),
            parent_policy_hooks: kroma_core::subagents::ParentPolicyHooks::KnownEmpty,
            session_path: session_path.display().to_string(),
        })
        .unwrap();
        r.transition("task-1", TaskState::Running, 1).unwrap();
        r.fail("task-1", "link failed", 1).unwrap();
        r.set_pending_merge(
            "task-1",
            PendingMergeLink {
                session_id: "sess-1".into(),
                session_path: session_path.display().to_string(),
                turn: 1,
                parent_revision: None,
                merged_revision: None,
                changed_paths: Vec::new(),
                tracked_files: 0,
            },
        )
        .unwrap();
    }
    let (host, _env) = make_host_bound_to(
        &root,
        &ws,
        Arc::new(ScriptProvider {
            script: Arc::new(|_req: &ProviderRequest| Action::Turn(stop("unused"))),
        }),
        Arc::clone(&registry),
        Arc::new(AllowAllGate),
        "sess-1",
        session_path,
    );

    let report = host.replay_pending_merges().unwrap();
    assert_eq!(report.linked, 0);
    assert_eq!(
        report.unresolved,
        vec!["task-1".to_string()],
        "an unreplayable intent must be reported, not silently skipped"
    );
    assert!(
        registry.lock().unwrap().pending_merges().is_empty(),
        "the unreplayable intent must not be left as a permanent orphan"
    );
    // A second replay finds nothing to do.
    let again = host.replay_pending_merges().unwrap();
    assert_eq!(again.linked, 0);
    assert!(again.unresolved.is_empty());
}

/// P1-b: a completed merge whose link-intent update fails must not claim a pending replay and must
/// not leave an orphan intent behind. The failure is injected *after* the merge is applied (from the
/// completion gate), which is exactly the window the review identified.
#[tokio::test]
async fn a_failed_link_intent_update_never_claims_replay_and_leaves_no_orphan() {
    let root = temp_root("link-intent-fail");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join("a.txt"), "hello").unwrap();
    let journal = SessionJournal::create(root.join("state/sessions"), &ws).unwrap();
    let session_path = journal.path().to_path_buf();
    drop(journal);
    let registry = open_registry(&root);
    let spec = worker_spec("write out.txt");
    seed_suspended_in_session(
        &registry,
        &ws,
        "task-1",
        &spec,
        200_000,
        "sess-1",
        session_path.to_str().unwrap(),
    );

    let calls = Arc::new(AtomicUsize::new(0));
    let provider = worker_provider(Arc::clone(&calls), "out.txt", "merged then link fails");
    let gate: SharedCompletionGate = Arc::new(SabotageRegistryGate {
        tasks_dir: root.join("state/tasks"),
        sabotage: Arc::new(AtomicBool::new(false)),
    });
    let (host, _env) = make_host_bound_to(
        &root,
        &ws,
        provider,
        Arc::clone(&registry),
        gate,
        "sess-1",
        session_path.clone(),
    );

    host.resume_task("task-1").unwrap();
    for _ in 0..600 {
        if host.resuming_count() == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(host.resuming_count(), 0, "the resumed run must finish");

    // The merge itself landed (that is the point: the link failed after the merge).
    assert_eq!(
        std::fs::read_to_string(ws.join("out.txt")).unwrap(),
        "merged then link fails"
    );
    {
        let r = registry.lock().unwrap();
        let task = r.get("task-1").unwrap();
        assert_ne!(
            task.state,
            TaskState::Completed,
            "a merge whose session WAL link failed must never be Completed"
        );
        let intent = task
            .pending_merge
            .as_ref()
            .expect("the pre-merge intent is the only durable trace left");
        assert!(
            intent.merged_revision.is_none(),
            "the post-merge update failed, so the intent cannot name a revision"
        );
        assert!(
            !task
                .notifications
                .iter()
                .any(|n| n.message.contains("pending for replay")),
            "no notification may claim a replay is pending: {:?}",
            task.notifications
        );
    }

    // Restore the registry: the unreplayable intent must be reported and cleared, not orphaned.
    let backup = root.join("state/tasks.sabotaged");
    std::fs::remove_file(root.join("state/tasks")).unwrap();
    std::fs::rename(&backup, root.join("state/tasks")).unwrap();
    let report = host.replay_pending_merges().unwrap();
    assert_eq!(report.linked, 0);
    assert_eq!(report.unresolved, vec!["task-1".to_string()]);
    assert!(
        registry.lock().unwrap().pending_merges().is_empty(),
        "no permanent orphan may be left behind"
    );
}

/// P1-c: a cancelled worker resume must leave the parent workspace and the session WAL untouched.
#[tokio::test]
async fn a_cancelled_worker_resume_leaves_the_parent_untouched() {
    let root = temp_root("cancel-worker");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join("a.txt"), "hello").unwrap();
    let journal = SessionJournal::create(root.join("state/sessions"), &ws).unwrap();
    let session_path = journal.path().to_path_buf();
    drop(journal);
    let registry = open_registry(&root);
    let spec = worker_spec("write out.txt");
    seed_suspended_in_session(
        &registry,
        &ws,
        "task-1",
        &spec,
        200_000,
        "sess-1",
        session_path.to_str().unwrap(),
    );

    // The first call writes the worker's delta, the second hangs until the resume is cancelled.
    let calls = Arc::new(AtomicUsize::new(0));
    let provider: Arc<dyn ModelProvider> = Arc::new(ScriptProvider {
        script: Arc::new({
            let calls = Arc::clone(&calls);
            move |_req: &ProviderRequest| {
                if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    Action::Turn(write_file("out.txt", "must not be merged"))
                } else {
                    Action::AwaitCancel
                }
            }
        }),
    });
    let (host, _env) = make_host_bound_to(
        &root,
        &ws,
        provider,
        Arc::clone(&registry),
        Arc::new(AllowAllGate),
        "sess-1",
        session_path.clone(),
    );

    host.resume_task("task-1").unwrap();
    for _ in 0..600 {
        if calls.load(Ordering::SeqCst) >= 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        calls.load(Ordering::SeqCst) >= 1,
        "the resumed worker must reach the backend"
    );
    assert!(
        host.cancel_task("task-1").unwrap(),
        "the live resumed run must be signalled"
    );
    for _ in 0..600 {
        if host.resuming_count() == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(host.resuming_count(), 0, "the cancelled run must finish");

    assert_eq!(
        registry.lock().unwrap().get("task-1").unwrap().state,
        TaskState::Cancelled
    );
    assert!(
        !ws.join("out.txt").exists(),
        "a cancelled worker delta must not be merged into the parent"
    );
    assert!(
        SessionJournal::open(&session_path)
            .unwrap()
            .revision_records()
            .is_empty(),
        "a cancelled worker resume must not link anything into the session WAL"
    );
}

/// P1-d: a finished record's id is never reused. A later admission with the same requested id is
/// recorded under a derived id, so the finished record keeps its own report bytes and spend.
#[tokio::test]
async fn two_admissions_reusing_a_task_id_keep_report_bytes_and_spend_separate() {
    let root = temp_root("reuse-id");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join("a.txt"), "hello").unwrap();
    let registry = open_registry(&root);
    let (host, _env) = make_host(
        &root,
        &ws,
        Arc::new(ScriptProvider {
            script: Arc::new(|_req: &ProviderRequest| Action::Turn(stop("unused"))),
        }),
        Arc::clone(&registry),
        Arc::new(AllowAllGate),
    );

    let (tx_a, _rx_a) = watch::channel(false);
    let id_a = host
        .register_child(
            &kroma_core::task_host::ChildTaskSpec {
                task_id: "sub-0001".to_string(),
                spec: scout_spec("first run"),
                allocation: 100,
                turn: 0,
                parent_policy_mode: PolicyMode::FullAutonomous,
                parent_policy_hooks: kroma_core::subagents::ParentPolicyHooks::KnownEmpty,
            },
            tx_a,
        )
        .unwrap();
    assert_eq!(id_a, "sub-0001");
    host.finish_child(
        &id_a,
        &kroma_core::task_host::ChildFinish {
            state: TaskState::Completed,
            spent: 11,
            summary: "report A".into(),
            keep_pending_merge: false,
            verification: None,
        },
    )
    .unwrap();

    // A new session restarts the caller's id counter, so it asks for the same durable id again.
    let (tx_b, _rx_b) = watch::channel(false);
    let id_b = host
        .register_child(
            &kroma_core::task_host::ChildTaskSpec {
                task_id: "sub-0001".to_string(),
                spec: scout_spec("second run"),
                allocation: 100,
                turn: 0,
                parent_policy_mode: PolicyMode::FullAutonomous,
                parent_policy_hooks: kroma_core::subagents::ParentPolicyHooks::KnownEmpty,
            },
            tx_b,
        )
        .unwrap();
    assert_ne!(
        id_b, "sub-0001",
        "a finished record's id must never be reused for a new admission"
    );
    host.finish_child(
        &id_b,
        &kroma_core::task_host::ChildFinish {
            state: TaskState::Completed,
            spent: 22,
            summary: "report B".into(),
            keep_pending_merge: false,
            verification: None,
        },
    )
    .unwrap();

    {
        let r = registry.lock().unwrap();
        let a = r.get("sub-0001").expect("the first record must survive");
        let b = r.get(&id_b).expect("the second record must be recorded");
        assert_eq!(
            a.token_spent, 11,
            "the first record's spend must be unchanged"
        );
        assert_eq!(b.token_spent, 22, "the second record's spend is its own");
        assert_eq!(a.artifacts.len(), 1);
        assert_eq!(b.artifacts.len(), 1);
    }
    assert_eq!(host.read_report("sub-0001").unwrap(), "report A");
    assert_eq!(host.read_report(&id_b).unwrap(), "report B");

    // A record that has not finished owns its identity outright: no derived id is handed out.
    let (tx_c, _rx_c) = watch::channel(false);
    let id_live = host
        .register_child(
            &kroma_core::task_host::ChildTaskSpec {
                task_id: "sub-0002".to_string(),
                spec: scout_spec("live run"),
                allocation: 100,
                turn: 0,
                parent_policy_mode: PolicyMode::FullAutonomous,
                parent_policy_hooks: kroma_core::subagents::ParentPolicyHooks::KnownEmpty,
            },
            tx_c,
        )
        .unwrap();
    assert_eq!(id_live, "sub-0002");
    let (tx_d, _rx_d) = watch::channel(false);
    let refusal = host
        .register_child(
            &kroma_core::task_host::ChildTaskSpec {
                task_id: "sub-0002".to_string(),
                spec: scout_spec("duplicate live run"),
                allocation: 100,
                turn: 0,
                parent_policy_mode: PolicyMode::FullAutonomous,
                parent_policy_hooks: kroma_core::subagents::ParentPolicyHooks::KnownEmpty,
            },
            tx_d,
        )
        .expect_err("a non-terminal record's id must never be reused");
    assert!(
        refusal.contains("not terminal") || refusal.contains("refusing to reuse"),
        "the refusal must explain the live record, got: {}",
        refusal
    );
}

/// P2-1: a pending merge intent whose session WAL cannot be opened must not abandon the other
/// intents in the same startup. The failing intent is reported (and retained so the next startup
/// retries it); every later intent is still replayed and cleared.
#[tokio::test]
async fn a_locked_wal_does_not_abandon_the_other_pending_merge_intents() {
    let root = temp_root("replay-continue");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join("a.txt"), "hello").unwrap();
    let sessions = root.join("state/sessions");
    let journal_a = SessionJournal::create(&sessions, &ws).unwrap();
    let path_a = journal_a.path().to_path_buf();
    drop(journal_a);
    let journal_b = SessionJournal::create(&sessions, &ws).unwrap();
    let path_b = journal_b.path().to_path_buf();
    drop(journal_b);

    let revision_id = "r-merged".to_string();
    let registry = open_registry(&root);
    {
        let mut r = registry.lock().unwrap();
        for (task_id, session_path) in [("task-a", &path_a), ("task-b", &path_b)] {
            r.register(NewTask {
                task_id: task_id.to_string(),
                session_id: "sess-1".to_string(),
                owner_id: "owner-1".to_string(),
                workspace_root: ws.display().to_string(),
                role: "worker".to_string(),
                created_turn: 0,
                token_allocation: 100,
                prompt: "p".to_string(),
                timeout_ms: 0,
                spec_json: "{}".to_string(),
                parent_policy: "allow-write".to_string(),
                parent_policy_hooks: kroma_core::subagents::ParentPolicyHooks::KnownEmpty,
                session_path: session_path.display().to_string(),
            })
            .unwrap();
            r.transition(task_id, TaskState::Running, 1).unwrap();
            r.fail(task_id, "link failed", 1).unwrap();
            r.set_pending_merge(
                task_id,
                PendingMergeLink {
                    session_id: "sess-1".into(),
                    session_path: session_path.display().to_string(),
                    turn: 1,
                    parent_revision: None,
                    merged_revision: Some(revision_id.clone()),
                    changed_paths: vec!["out.txt".into()],
                    tracked_files: 1,
                },
            )
            .unwrap();
        }
    }

    let (host, _env) = make_host_bound_to(
        &root,
        &ws,
        Arc::new(ScriptProvider {
            script: Arc::new(|_req: &ProviderRequest| Action::Turn(stop("unused"))),
        }),
        Arc::clone(&registry),
        Arc::new(AllowAllGate),
        "sess-1",
        path_a.clone(),
    );

    // A concurrent conversation holds the first intent's WAL.
    let held = SessionJournal::open(&path_a).unwrap();
    let report = host
        .replay_pending_merges()
        .expect("a per-intent WAL failure must not fail the whole replay");
    assert_eq!(
        report.linked, 1,
        "the second intent must still be replayed, got {:?}",
        report
    );
    assert_eq!(
        report.failed.len(),
        1,
        "the unopenable intent must be reported, got {:?}",
        report
    );
    assert_eq!(report.failed[0].0, "task-a");
    assert!(
        !report.failed[0].1.is_empty(),
        "the failure must carry a reason"
    );
    assert!(report.unresolved.is_empty());
    {
        let r = registry.lock().unwrap();
        assert_eq!(
            r.pending_merges()
                .into_iter()
                .map(|(id, _)| id)
                .collect::<Vec<_>>(),
            vec!["task-a".to_string()],
            "only the failed intent may remain pending"
        );
    }
    let has_revision = |path: &std::path::Path| {
        SessionJournal::open(path)
            .unwrap()
            .revision_records()
            .iter()
            .any(|r| r.revision_id == revision_id)
    };
    assert!(has_revision(&path_b), "the second intent must be linked");
    assert!(
        !std::fs::read_to_string(&path_a)
            .unwrap()
            .contains(&revision_id),
        "the locked WAL must not be linked"
    );

    // Releasing the WAL lets the retained intent be replayed on the next startup.
    drop(held);
    let again = host.replay_pending_merges().unwrap();
    assert_eq!(again.linked, 1);
    assert!(again.failed.is_empty(), "got {:?}", again);
    assert!(
        registry.lock().unwrap().pending_merges().is_empty(),
        "the retained intent must be cleared once it is replayed"
    );
    assert!(has_revision(&path_a));
}

/// P2-2: a `PersistUncertain` on the resume transition means the `Running` state is already visible
/// on disk and in memory (only the directory fsync could not be confirmed), so the admission must
/// continue instead of stranding a durable `Running` record with no run that refuses every later
/// resume. The run is admitted exactly once.
#[cfg(unix)]
#[tokio::test]
async fn a_persist_uncertain_resume_transition_still_admits_the_run_exactly_once() {
    let root = temp_root("resume-uncertain");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join("a.txt"), "hello").unwrap();
    let registry = open_registry(&root);
    let spec = scout_spec("resume through an uncertain commit");
    seed_suspended(&registry, &ws, "task-1", &spec, 200_000);

    let calls = Arc::new(AtomicUsize::new(0));
    let release = Arc::new(tokio::sync::Notify::new());
    let provider: Arc<dyn ModelProvider> = Arc::new(ScriptProvider {
        script: Arc::new({
            let calls = Arc::clone(&calls);
            let release = Arc::clone(&release);
            move |_req: &ProviderRequest| {
                calls.fetch_add(1, Ordering::SeqCst);
                Action::AwaitRelease(Arc::clone(&release))
            }
        }),
    });
    let (host, _env) = make_host(
        &root,
        &ws,
        provider,
        Arc::clone(&registry),
        Arc::new(AllowAllGate),
    );

    // Force a real post-rename directory-sync failure: the registry's directory becomes a symlink,
    // so the rename still lands but `sync_dir` refuses to follow the symlinked leaf. That is exactly
    // the `PersistUncertain` case (visible on disk, not provably durable).
    let tasks_dir = root.join("state/tasks");
    let tasks_real = root.join("state/tasks-real");
    std::fs::rename(&tasks_dir, &tasks_real).unwrap();
    std::os::unix::fs::symlink(&tasks_real, &tasks_dir).unwrap();

    host.resume_task("task-1")
        .expect("a PersistUncertain resume transition must be treated as applied");
    assert!(
        host.has_live_run("task-1"),
        "the admission must bind the canceller even when the transition was uncertain"
    );
    assert_eq!(
        registry.lock().unwrap().get("task-1").unwrap().state,
        TaskState::Running,
        "the durable state must match the visible Running transition"
    );
    // A second resume must be refused: exactly one run may be admitted.
    let second = host
        .resume_task("task-1")
        .expect_err("a second resume must be refused while the admitted run is live");
    assert!(
        second.contains("live run") || second.contains("already"),
        "the refusal must explain the live run, got: {}",
        second
    );

    for _ in 0..600 {
        if calls.load(Ordering::SeqCst) >= 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the backend must be admitted exactly once"
    );

    // Repair the directory and let the admitted run finish normally.
    std::fs::remove_file(&tasks_dir).unwrap();
    std::fs::rename(&tasks_real, &tasks_dir).unwrap();
    release.notify_one();
    assert_eq!(
        wait_terminal(&registry, "task-1").await,
        TaskState::Completed,
        "the admitted run must settle normally once the commit path is repaired"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "still exactly one backend run"
    );
}

/// P2-3: an over-cap report must never block settlement. It is stored truncated and explicitly
/// marked, and the artifact carries the digest/length of exactly the stored bytes plus the true
/// original size in a separate field, so a reference can never claim bytes that were not stored.
#[tokio::test]
async fn an_over_cap_report_is_truncated_and_marked_without_blocking_settlement() {
    let root = temp_root("report-cap");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join("a.txt"), "hello").unwrap();
    let registry = open_registry(&root);
    let (host, _env) = make_host(
        &root,
        &ws,
        Arc::new(ScriptProvider {
            script: Arc::new(|_req: &ProviderRequest| Action::Turn(stop("unused"))),
        }),
        Arc::clone(&registry),
        Arc::new(AllowAllGate),
    );

    let (cancel_tx, _cancel_rx) = watch::channel(false);
    let task_id = host
        .register_child(
            &kroma_core::task_host::ChildTaskSpec {
                task_id: "sub-cap".to_string(),
                spec: scout_spec("produce an over-cap report"),
                allocation: 100,
                turn: 0,
                parent_policy_mode: PolicyMode::FullAutonomous,
                parent_policy_hooks: kroma_core::subagents::ParentPolicyHooks::KnownEmpty,
            },
            cancel_tx,
        )
        .unwrap();

    let original = format!(
        "{}END-OF-ORIGINAL",
        "R".repeat(kroma_core::task_host::MAX_REPORT_BYTES + 4096)
    );
    host.finish_child(
        &task_id,
        &kroma_core::task_host::ChildFinish {
            state: TaskState::Completed,
            spent: 7,
            summary: original.clone(),
            keep_pending_merge: false,
            verification: None,
        },
    )
    .expect("an over-cap report must never block settlement");

    let artifact = {
        let r = registry.lock().unwrap();
        let task = r.get(&task_id).unwrap();
        assert_eq!(task.state, TaskState::Completed, "the task must settle");
        assert!(
            task.notifications.iter().any(|n| n.kind == "completed"),
            "the settlement notification must be raised"
        );
        task.artifacts
            .iter()
            .find(|a| a.kind == "report")
            .expect("the task must carry a report artifact")
            .clone()
    };
    assert!(
        artifact.bytes <= kroma_core::task_host::MAX_REPORT_BYTES as u64,
        "the stored report must stay under the cap, got {} bytes",
        artifact.bytes
    );
    assert_eq!(
        artifact.original_bytes,
        Some(original.len() as u64),
        "the reference must record the true original size separately"
    );

    // `read_report` verifies the digest and the stored length against the reference.
    let stored = host.read_report(&task_id).unwrap();
    assert_eq!(
        stored.len() as u64,
        artifact.bytes,
        "the readable report must be exactly the stored bytes"
    );
    assert_eq!(
        artifact.sha256.as_deref(),
        Some(kroma_core::prompt::sha256_hex64(stored.as_bytes()).as_str()),
        "the digest must cover exactly the stored bytes"
    );
    assert!(
        stored.contains("report truncated"),
        "the truncation must be visible in the stored report: {}",
        &stored[stored.len().saturating_sub(200)..]
    );
    assert!(
        stored.contains(&original.len().to_string()),
        "the notice must name the true original byte count"
    );
    assert!(
        !stored.contains("END-OF-ORIGINAL"),
        "a truncated report must not claim to contain the original tail"
    );

    // The bytes on disk are exactly the bytes the reference promises.
    let on_disk = std::fs::read(root.join("state/subagents/reports/sub-cap.report")).unwrap();
    assert_eq!(
        on_disk,
        stored.as_bytes(),
        "the reference must match the bytes actually stored"
    );
}

/// B11b: a durable record whose parent policy-hook constraints are `Required` or `Unknown` must
/// refuse the resume before materializing a worktree or running the child, and settle as an
/// explicit `Failed` with the record preserved — never as a silent success.
#[tokio::test]
async fn resume_of_a_task_with_parent_policy_hooks_is_refused_without_side_effects() {
    for hooks in [
        kroma_core::subagents::ParentPolicyHooks::Required {
            hook_ids: vec!["b11b-deny-src-writes".to_string()],
        },
        kroma_core::subagents::ParentPolicyHooks::Unknown,
    ] {
        let root = temp_root("resume-hooks");
        let ws = root.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(ws.join("a.txt"), "hello").unwrap();
        let registry = open_registry(&root);
        let spec = SubagentSpec {
            role: SubagentRole::Worker,
            task: "write out.txt".into(),
            allowed_write_globs: None,
            budget: SubagentBudget {
                max_tokens: 200_000,
                timeout_ms: 5000,
                ..SubagentBudget::default()
            },
            revert_on_verify_failure: false,
            disallow_test_tampering: false,
            protected_paths: Vec::new(),
        };
        {
            let mut r = registry.lock().unwrap();
            r.register(NewTask {
                task_id: "task-1".into(),
                session_id: "sess-1".into(),
                owner_id: "owner-1".into(),
                workspace_root: ws.display().to_string(),
                role: "worker".into(),
                created_turn: 0,
                token_allocation: 200_000,
                prompt: spec.task.clone(),
                timeout_ms: 5000,
                spec_json: serde_json::to_string(&spec).unwrap(),
                parent_policy: serde_json::to_string(&PolicyMode::FullAutonomous).unwrap(),
                parent_policy_hooks: hooks.clone(),
                session_path: String::new(),
            })
            .unwrap();
            r.transition("task-1", TaskState::Running, 1).unwrap();
            r.suspend("task-1", 1).unwrap();
        }

        let calls = Arc::new(AtomicUsize::new(0));
        let provider: Arc<dyn ModelProvider> = Arc::new(ScriptProvider {
            script: Arc::new({
                let calls = Arc::clone(&calls);
                move |_req: &ProviderRequest| {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Action::Turn(stop("the constrained child must never run"))
                }
            }),
        });
        let (host, _env) = make_host(
            &root,
            &ws,
            provider,
            Arc::clone(&registry),
            Arc::new(RejectGate),
        );

        host.resume_task("task-1").unwrap();
        let state = wait_terminal(&registry, "task-1").await;
        assert_eq!(
            state,
            TaskState::Failed,
            "a constrained resume must settle as an explicit failure, not Completed"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "the child provider must never be called"
        );
        assert!(
            !ws.join("out.txt").exists(),
            "the parent workspace must be untouched"
        );
        assert!(
            !root
                .join("state")
                .join("worktrees")
                .join("resume-task-1")
                .exists(),
            "the refused resume must not materialize a worktree"
        );
        let r = registry.lock().unwrap();
        let task = r
            .get("task-1")
            .expect("the durable record must be preserved");
        assert!(
            task.notifications
                .iter()
                .any(|n| n.message.contains("unsupported delegation")),
            "the failure must name the unsupported delegation: {:?}",
            task.notifications
        );
        drop(r);
        let _ = std::fs::remove_dir_all(&root);
    }
}

/// C00-10-A2: a resume replays a task admitted under an *earlier* process policy, so the recorded
/// mode is only an upper bound — it must be re-validated against the policy this process runs under
/// now. A worker admitted under `FullAutonomous` must not be re-run with automatic writes by a
/// process that restarted as `ReadOnly`, and the refusal must leave the task resumable.
#[tokio::test]
async fn resume_revalidates_a_worker_against_the_current_policy() {
    let root = temp_root("resume-revalidate");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join("a.txt"), "hello").unwrap();
    let journal = SessionJournal::create(root.join("state/sessions"), &ws).unwrap();
    let session_path = journal.path().to_path_buf();
    drop(journal);

    let registry = open_registry(&root);
    let spec = worker_spec("write out.txt");
    // What a crash leaves behind: a worker admitted under `FullAutonomous`, suspended before it ran.
    seed_suspended_in_session(
        &registry,
        &ws,
        "task-1",
        &spec,
        200_000,
        "sess-1",
        session_path.to_str().unwrap(),
    );
    {
        let r = registry.lock().unwrap();
        let record = r.get("task-1").unwrap();
        assert_eq!(record.state, TaskState::Suspended);
        assert_eq!(
            serde_json::from_str::<PolicyMode>(&record.parent_policy).unwrap(),
            PolicyMode::FullAutonomous,
            "the admission must have recorded the wider policy"
        );
    }

    let calls = Arc::new(AtomicUsize::new(0));
    // The process restarted under `--policy read-only`, on the same registry and session WAL.
    let (host, _env) = make_host_with_policy(
        &root,
        &ws,
        worker_provider(Arc::clone(&calls), "out.txt", "must not land"),
        Arc::clone(&registry),
        Arc::new(AllowAllGate),
        "sess-1",
        session_path.clone(),
        PolicyMode::ReadOnly,
    );

    let err = host
        .resume_task("task-1")
        .expect_err("a worker admitted under a wider policy must not resume under read-only");
    let expected_hint = format!(
        "admitted under policy `{}` but this process is running under `{}`",
        PolicyMode::FullAutonomous.as_str(),
        PolicyMode::ReadOnly.as_str()
    );
    assert!(
        err.contains(&expected_hint),
        "the refusal must name both the admission policy and the current one, got: {}",
        err
    );
    assert!(
        err.contains("--allow-write"),
        "the refusal must say how to re-authorize, got: {}",
        err
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "the backend must not run when the current policy no longer authorizes the role"
    );
    assert!(
        !ws.join("out.txt").exists(),
        "a refused resume must not mutate the parent workspace"
    );
    assert!(
        !root.join("state/worktrees/resume-task-1").exists(),
        "the refusal must happen before a worktree is materialized"
    );
    assert_eq!(
        registry.lock().unwrap().get("task-1").unwrap().state,
        TaskState::Suspended,
        "a policy refusal must leave the task resumable, not Failed/Running"
    );

    // Re-authorizing automatic writes is enough: the same record then resumes and merges.
    let (host, _env) = make_host_with_policy(
        &root,
        &ws,
        worker_provider(Arc::clone(&calls), "out.txt", "must not land"),
        Arc::clone(&registry),
        Arc::new(AllowAllGate),
        "sess-1",
        session_path,
        PolicyMode::AllowWrite,
    );
    host.resume_task("task-1").unwrap();
    assert_eq!(
        wait_terminal(&registry, "task-1").await,
        TaskState::Completed
    );
    assert_eq!(
        std::fs::read_to_string(ws.join("out.txt")).unwrap(),
        "must not land"
    );
}

// ---------------------------------------------------------------------------------------------
// R01: a durable resume runs inside the process's trusted execution boundary
// ---------------------------------------------------------------------------------------------

/// A resume without a trusted execution policy is refused while the task is still resumable.
///
/// Defaulting to host here is exactly the defect: the process may have been started with
/// `execution_sandbox = bwrap`, and a resume that silently rebuilds the child runtime with the
/// `ToolRuntime::new` default would hand it the user's filesystem and network.
#[tokio::test]
async fn resume_without_an_execution_policy_is_refused_and_runs_nothing() {
    let root = temp_root("resume-no-policy");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join("a.txt"), "hello").unwrap();
    let registry = open_registry(&root);
    let spec = worker_spec("write out.txt");
    seed_suspended(&registry, &ws, "task-1", &spec, 200_000);

    let calls = Arc::new(AtomicUsize::new(0));
    let provider = worker_provider(Arc::clone(&calls), "out.txt", "must not land");
    let (host, _env) = make_host_with_execution(
        &root,
        &ws,
        provider,
        Arc::clone(&registry),
        Arc::new(AllowAllGate),
        "sess-1",
        root.join("state/sessions/seed.wal"),
        PolicyMode::AllowWrite,
        // The surface never attached the operator's boundary.
        None,
    );

    let error = host
        .resume_task("task-1")
        .expect_err("a resume without a trusted execution policy must be refused");
    assert!(
        error.contains("no trusted execution policy"),
        "the refusal must name the missing policy: {error}"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "no backend request may be made"
    );
    assert!(
        !ws.join("out.txt").exists(),
        "the refused resume must not touch the parent workspace"
    );
    let state = registry.lock().unwrap().get("task-1").unwrap().state;
    assert!(
        !state.is_terminal(),
        "the task must stay resumable, not be consumed as a failure: {state:?}"
    );
}

/// A resume that *has* a policy installs it on the resumed child runtime. The backend is deliberately
/// unusable, so the only safe outcome is a fail-closed refusal of the child's `bash`; on the original
/// code the child defaulted to host and wrote the marker.
#[tokio::test]
async fn resumed_child_runs_under_the_attached_execution_policy() {
    let root = temp_root("resume-policy");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join("a.txt"), "hello").unwrap();
    let marker = root.join("outside-marker.txt");
    let registry = open_registry(&root);
    let spec = worker_spec("run a command");
    seed_suspended(&registry, &ws, "task-1", &spec, 200_000);

    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let seen_c = Arc::clone(&seen);
    let marker_c = marker.clone();
    let turns = Arc::new(AtomicUsize::new(0));
    let provider: Arc<dyn ModelProvider> = Arc::new(ScriptProvider {
        script: Arc::new(move |req: &ProviderRequest| {
            if turns.fetch_add(1, Ordering::SeqCst) == 0 {
                Action::Turn(bash(&format!("printf pwned > {}", marker_c.display())))
            } else {
                for item in &req.messages {
                    if let kroma_core::messages::TranscriptItem::ToolResult(result) = item {
                        if result.tool_name == "bash" {
                            seen_c.lock().unwrap().push(format!(
                                "{:?}|{}",
                                result.outcome,
                                result.meta.execution_backend.clone().unwrap_or_default()
                            ));
                        }
                    }
                }
                Action::Turn(stop("resumed child finished"))
            }
        }),
    });

    let policy = kroma_core::sandbox::ExecutionPolicy::isolated(
        kroma_core::sandbox::SandboxTemplate::new(kroma_core::sandbox::SandboxBackend::Container {
            runtime: "kroma-no-such-runtime".to_string(),
            image: "kroma-no-such-image".to_string(),
        }),
    );
    // A worker resume records its merge in the session WAL, so the WAL must exist before admission.
    let journal = SessionJournal::create(root.join("state/sessions"), &ws).unwrap();
    let session_path = journal.path().to_path_buf();
    drop(journal);
    let (host, _env) = make_host_with_execution(
        &root,
        &ws,
        provider,
        Arc::clone(&registry),
        Arc::new(AllowAllGate),
        "sess-1",
        session_path,
        // `bash` must be dispatchable inside the child, so the probe fails on the *boundary* and
        // not on the approval policy.
        PolicyMode::FullAutonomous,
        Some(policy),
    );
    host.resume_task("task-1")
        .expect("a resume with an attached policy is admitted");
    let _ = wait_terminal(&registry, "task-1").await;

    let observed = seen.lock().unwrap().clone();
    assert!(
        !marker.exists(),
        "the resumed child's bash ran on the host even though the host attached an isolated policy"
    );
    assert_eq!(
        observed.len(),
        1,
        "the resumed child's bash result must be recorded: {observed:?}"
    );
    assert!(
        observed[0].contains("SandboxUnavailable") && observed[0].contains("none:refused"),
        "the resumed child's bash must be refused by the attached backend, got {observed:?}"
    );
}
