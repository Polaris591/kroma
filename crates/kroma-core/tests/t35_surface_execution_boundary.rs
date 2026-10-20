//! R01 integration seam: the *surface* a host builds must hand the operator's execution boundary to
//! everything it starts.
//!
//! The component regressions prove the boundary travels parent → `spawn_subagent` (t09) and that a
//! durable host installs the policy it was given (durable_tasks). Neither covers the wiring this
//! crate's dispatcher owns: `KromaRuntimeConfig` → session tool runtime → child workers, repair
//! races, and a durable resume driven over RPC. Before that wiring existed the dispatcher's runtimes
//! were built with the default host policy, so a `kroma serve` session declared as `bwrap` still ran
//! its children on the host.
//!
//! Each test runs the same script twice — once with `bwrap`, once with `host` — so the assertions
//! are live: the host control must produce the outside marker and the isolated run must not.

use kroma_core::budget::BudgetConfig;
use kroma_core::messages::{AssistantTurn, FinishReason, TokenUsage, ToolCall, TranscriptItem};
use kroma_core::provider::{ModelProvider, ProviderCompletion, ProviderError, ProviderRequest};
use kroma_core::sandbox::{ExecutionMode, ExecutionPolicy, SandboxSpec, SandboxTemplate};
use kroma_core::surface::{JsonRpcRequest, KromaRuntimeConfig, RpcDispatcher};
use kroma_core::tool::policy::PolicyMode;
use kroma_core::types::ToolCallId;
use serde_json::json;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::watch;

fn scratch(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("kroma-t35-{}-{}", tag, uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    root
}

/// The boundary these tests prove cannot be faked: without a usable bwrap the isolation half would
/// silently run on the host, so an unavailable backend is a hard failure, exactly like t09/t31/t33.
fn require_bwrap() {
    if SandboxSpec::bwrap("/tmp").probe().is_err() {
        panic!("bwrap is required to prove the surface execution boundary and is not usable here");
    }
}

fn isolated() -> ExecutionPolicy {
    ExecutionPolicy::isolated(SandboxTemplate::bwrap())
}

fn usage() -> TokenUsage {
    TokenUsage {
        input_tokens: 20,
        cached_input_tokens: Some(0),
        cache_creation_input_tokens: None,
        output_tokens: 10,
        reasoning_tokens: None,
    }
}

fn tool_turn(id: &str, name: &str, args: serde_json::Value) -> AssistantTurn {
    AssistantTurn {
        content: None,
        tool_calls: vec![ToolCall {
            call_id: ToolCallId::new(id),
            name: name.to_string(),
            raw_arguments: args.to_string(),
        }],
        reasoning_blocks: Vec::new(),
        finish_reason: FinishReason::ToolCalls,
        usage: Some(usage()),
    }
}

fn stop_turn(text: &str) -> AssistantTurn {
    AssistantTurn {
        content: Some(text.to_string()),
        tool_calls: Vec::new(),
        reasoning_blocks: Vec::new(),
        finish_reason: FinishReason::Stop,
        usage: Some(usage()),
    }
}

fn script_provider<F>(f: F) -> Arc<dyn ModelProvider>
where
    F: Fn(ProviderRequest) -> AssistantTurn + Send + Sync + 'static,
{
    struct Script<F>(F);

    impl<F> ModelProvider for Script<F>
    where
        F: Fn(ProviderRequest) -> AssistantTurn + Send + Sync + 'static,
    {
        fn complete<'a>(
            &'a self,
            req: ProviderRequest,
            _budget: &'a BudgetConfig,
            _cancel_rx: watch::Receiver<bool>,
        ) -> Pin<Box<dyn Future<Output = Result<ProviderCompletion, ProviderError>> + Send + 'a>>
        {
            let turn = (self.0)(req);
            Box::pin(async move {
                Ok(ProviderCompletion {
                    turn,
                    request_body_bytes: 64,
                    latency_ms: 1,
                    ttft_ms: Some(1),
                    attempts_used: 1,
                    reported_model: None,
                })
            })
        }
    }

    Arc::new(Script(f))
}

/// The backend that ran the most recent `bash` in the transcript the model is shown.
fn last_bash_backend(req: &ProviderRequest) -> Option<String> {
    req.messages.iter().rev().find_map(|item| match item {
        TranscriptItem::ToolResult(result) if result.tool_name == "bash" => {
            result.meta.execution_backend.clone()
        }
        _ => None,
    })
}

fn is_child(req: &ProviderRequest) -> bool {
    req.system_prompt.contains("[SUBAGENT ROLE:")
}

/// What one case observed. The scratch tree is removed before the assertion, so the marker state is
/// read here: an outside write that the host filesystem accepted is the defect being tested.
struct Case {
    marker_landed: bool,
    child_backend: Arc<Mutex<Option<String>>>,
}

impl Case {
    fn marker_landed(&self) -> bool {
        self.marker_landed
    }

    fn backend(&self) -> Option<String> {
        self.child_backend.lock().unwrap().clone()
    }
}

/// The child's write attempt. `mkdir -p` makes the bwrap run a real write into the sandbox tmpfs
/// (exit 0) instead of a "no such directory" artefact, so "no host marker" means isolation rather
/// than a path that happened not to exist.
fn outside_write(marker: &Path) -> String {
    let dir = marker.parent().expect("marker has a parent");
    format!(
        "mkdir -p {} && printf pwned > {}",
        dir.display(),
        marker.display()
    )
}

/// `session.prompt` → `spawn_subagent` → the worker's `bash`.
async fn prompt_case(policy: ExecutionPolicy, tag: &str) -> Case {
    let root = scratch(tag);
    let ws = root.join("ws");
    let state = root.join("state");
    std::fs::create_dir_all(ws.join("src")).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(ws.join("src/lib.rs"), "pub const VALUE: i32 = 1;\n").unwrap();
    let marker = root.join("outside-marker.txt");

    let child_backend: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let child_backend_c = Arc::clone(&child_backend);
    let parent_turns = Arc::new(AtomicUsize::new(0));
    let child_turns = Arc::new(AtomicUsize::new(0));
    let marker_for_child = marker.clone();

    let provider = script_provider(move |req: ProviderRequest| {
        if is_child(&req) {
            if child_turns.fetch_add(1, Ordering::SeqCst) == 0 {
                tool_turn(
                    "child_bash_1",
                    "bash",
                    json!({"command": outside_write(&marker_for_child)}),
                )
            } else {
                *child_backend_c.lock().unwrap() = last_bash_backend(&req);
                stop_turn("worker finished")
            }
        } else if parent_turns.fetch_add(1, Ordering::SeqCst) == 0 {
            tool_turn(
                "call_worker_1",
                "spawn_subagent",
                json!({
                    "role": "worker",
                    "task": "try to write outside the workspace",
                    "max_turns": 3
                }),
            )
        } else {
            stop_turn("worker call returned")
        }
    });

    let config = KromaRuntimeConfig::new(&ws, &state, PolicyMode::FullAutonomous)
        .with_execution_policy(policy);
    let dispatcher = RpcDispatcher::new(config, provider).unwrap();
    let response = dispatcher
        .handle_request(JsonRpcRequest::new(
            1,
            "session.prompt",
            json!({"prompt": "prove the worker boundary", "max_tokens": 200_000}),
        ))
        .await;
    assert!(
        response.is_ok(),
        "the prompt itself must run: {:?}",
        response.error
    );

    let case = Case {
        marker_landed: marker.exists(),
        child_backend,
    };
    let _ = std::fs::remove_dir_all(&root);
    case
}

/// A real dispatcher declared as `bwrap` must sandbox the worker it spawns; the host control proves
/// the same script really does write outside when the boundary is missing.
#[tokio::test]
async fn rpc_prompt_hands_the_surface_boundary_to_spawned_workers() {
    require_bwrap();
    let isolated_case = prompt_case(isolated(), "prompt-isolated").await;
    assert!(
        !isolated_case.marker_landed(),
        "the dispatcher declared bwrap but its spawned worker wrote outside the workspace on the host"
    );
    assert_eq!(
        isolated_case.backend().as_deref(),
        Some("bwrap"),
        "the worker's bash must report the backend the surface declared"
    );

    let host_case = prompt_case(ExecutionPolicy::host(), "prompt-host").await;
    assert!(
        host_case.marker_landed(),
        "control: with a host boundary the worker's write must land, or the assertion is not live"
    );
}

/// `session.tournament` → candidate worktrees → each candidate's `bash`.
async fn tournament_case(policy: ExecutionPolicy, tag: &str) -> Case {
    let root = scratch(tag);
    let ws = root.join("ws");
    let state = root.join("state");
    std::fs::create_dir_all(ws.join("src")).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(ws.join("src/lib.rs"), "pub const VALUE: i32 = 1;\n").unwrap();
    let marker = root.join("outside-marker.txt");

    let child_backend: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let child_backend_c = Arc::clone(&child_backend);
    let child_turns = Arc::new(AtomicUsize::new(0));
    let marker_for_child = marker.clone();

    let provider = script_provider(move |req: ProviderRequest| {
        if is_child(&req) {
            if child_turns.fetch_add(1, Ordering::SeqCst) % 2 == 0 {
                tool_turn(
                    "candidate_bash_1",
                    "bash",
                    json!({"command": outside_write(&marker_for_child)}),
                )
            } else {
                *child_backend_c.lock().unwrap() = last_bash_backend(&req);
                stop_turn("candidate finished")
            }
        } else {
            stop_turn("unexpected parent turn")
        }
    });

    let config = KromaRuntimeConfig::new(&ws, &state, PolicyMode::FullAutonomous)
        .with_execution_policy(policy);
    let dispatcher = RpcDispatcher::new(config, provider).unwrap();
    let response = dispatcher
        .handle_request(JsonRpcRequest::new(
            1,
            "session.tournament",
            json!({
                "task": "try to write outside the workspace",
                "candidates": 2,
                "max_turns": 3,
                "timeout_ms": 60_000
            }),
        ))
        .await;
    assert!(
        response.is_ok(),
        "the repair race itself must run: {:?}",
        response.error
    );

    let case = Case {
        marker_landed: marker.exists(),
        child_backend,
    };
    let _ = std::fs::remove_dir_all(&root);
    case
}

/// A repair race started over RPC must give its candidates the surface's boundary, not the default.
#[tokio::test]
async fn rpc_tournament_hands_the_surface_boundary_to_candidates() {
    require_bwrap();
    let isolated_case = tournament_case(isolated(), "tournament-isolated").await;
    assert!(
        !isolated_case.marker_landed(),
        "the dispatcher declared bwrap but a tournament candidate wrote outside the workspace"
    );
    assert_eq!(
        isolated_case.backend().as_deref(),
        Some("bwrap"),
        "a candidate's bash must report the backend the surface declared"
    );

    let host_case = tournament_case(ExecutionPolicy::host(), "tournament-host").await;
    assert!(
        host_case.marker_landed(),
        "control: with a host boundary a candidate's write must land"
    );
}

fn worker_spec(task: &str) -> kroma_core::subagents::SubagentSpec {
    kroma_core::subagents::SubagentSpec {
        role: kroma_core::subagents::SubagentRole::Worker,
        task: task.to_string(),
        allowed_write_globs: None,
        budget: kroma_core::subagents::SubagentBudget {
            max_tokens: 200_000,
            timeout_ms: 30_000,
            ..kroma_core::subagents::SubagentBudget::default()
        },
        revert_on_verify_failure: true,
        disallow_test_tampering: false,
        protected_paths: Vec::new(),
    }
}

/// Persist a suspended durable task the way a crashed supervisor leaves one behind.
fn seed_suspended(ws: &Path, state: &Path, session_path: &Path) {
    use kroma_core::async_tasks::{AsyncTaskRegistry, NewTask, TaskState};
    let tasks_dir = state.join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();
    let spec = worker_spec("try to write outside the workspace");
    let mut registry = AsyncTaskRegistry::open(tasks_dir.join("registry.json")).unwrap();
    registry
        .register(NewTask {
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
            parent_policy: serde_json::to_string(&PolicyMode::FullAutonomous).unwrap(),
            parent_policy_hooks: kroma_core::subagents::ParentPolicyHooks::KnownEmpty,
            session_path: session_path.display().to_string(),
        })
        .unwrap();
    registry
        .transition("task-1", TaskState::Running, 1)
        .unwrap();
    registry.suspend("task-1", 1).unwrap();
    registry.persist().unwrap();
}

/// `tasks.control resume` → the resumed child's `bash`.
async fn resume_case(policy: ExecutionPolicy, tag: &str) -> Case {
    let root = scratch(tag);
    let ws = root.join("ws");
    let state = root.join("state");
    std::fs::create_dir_all(ws.join("src")).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(ws.join("src/lib.rs"), "pub const VALUE: i32 = 1;\n").unwrap();
    let marker = root.join("outside-marker.txt");

    // A worker resume records its merge in the session WAL, so the WAL must exist before admission.
    let journal = kroma_core::session::SessionJournal::create(state.join("sessions"), &ws).unwrap();
    let session_path = journal.path().to_path_buf();
    drop(journal);
    seed_suspended(&ws, &state, &session_path);

    let child_backend: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let child_backend_c = Arc::clone(&child_backend);
    let child_turns = Arc::new(AtomicUsize::new(0));
    let child_turns_c = Arc::clone(&child_turns);
    let marker_for_child = marker.clone();

    let provider = script_provider(move |req: ProviderRequest| {
        assert!(
            is_child(&req),
            "only the resumed worker should call the provider"
        );
        if child_turns_c.fetch_add(1, Ordering::SeqCst) == 0 {
            tool_turn(
                "resumed_bash_1",
                "bash",
                json!({"command": outside_write(&marker_for_child)}),
            )
        } else {
            *child_backend_c.lock().unwrap() = last_bash_backend(&req);
            stop_turn("resumed worker finished")
        }
    });

    let config = KromaRuntimeConfig::new(&ws, &state, PolicyMode::FullAutonomous)
        .with_execution_policy(policy);
    let dispatcher = RpcDispatcher::new(config, provider).unwrap();
    let response = dispatcher
        .handle_request(JsonRpcRequest::new(
            1,
            "tasks.control",
            json!({"action": "resume", "task_id": "task-1"}),
        ))
        .await;
    assert!(
        response.is_ok(),
        "a resume with an attached policy must be admitted: {:?}",
        response.error
    );

    // The resume runs in the background; wait for the worker's second turn (its transcript now holds
    // the bash result) rather than assuming a duration.
    for _ in 0..600 {
        if child_turns.load(Ordering::SeqCst) >= 2 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    let case = Case {
        marker_landed: marker.exists(),
        child_backend,
    };
    let _ = std::fs::remove_dir_all(&root);
    case
}

/// A durable task resumed through the dispatcher must run its child under the surface's boundary.
#[tokio::test]
async fn rpc_resume_hands_the_surface_boundary_to_the_resumed_child() {
    require_bwrap();
    let isolated_case = resume_case(isolated(), "resume-isolated").await;
    assert!(
        !isolated_case.marker_landed(),
        "the dispatcher declared bwrap but a resumed durable child wrote outside the workspace"
    );
    assert_eq!(
        isolated_case.backend().as_deref(),
        Some("bwrap"),
        "the resumed child's bash must report the backend the surface declared"
    );

    let host_case = resume_case(ExecutionPolicy::host(), "resume-host").await;
    assert!(
        host_case.marker_landed(),
        "control: with a host boundary the resumed child's write must land"
    );
}

/// The default a surface gets when it installs nothing is still the host boundary — the wiring makes
/// the operator's value reach the runtime, it does not change what "unset" means.
#[test]
fn an_unconfigured_surface_still_defaults_to_the_host_boundary() {
    let root = scratch("default-policy");
    let config =
        KromaRuntimeConfig::new(root.join("ws"), root.join("state"), PolicyMode::AllowWrite);
    assert_eq!(config.execution_policy.mode(), &ExecutionMode::Host);
    let _ = std::fs::remove_dir_all(&root);
}
