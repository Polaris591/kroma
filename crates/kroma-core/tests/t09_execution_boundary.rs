//! T09 (KR-18, KR-33): the run's execution boundary covers the *real* tool actions, not just the
//! final scoring step, and the boundary is reported as what actually ran.
//!
//! Three properties are pinned:
//! 1. In an isolated run the agent's `bash` really is sandboxed — it cannot write outside the
//!    workspace and cannot reach the network — and the result records the backend that ran it.
//! 2. When isolation is required and the backend is unavailable, the command is **refused**; there
//!    is no silent host fallback and no side effect.
//! 3. The class table is explicit: which spawn class is sandboxed and whether it may use the network
//!    is decided by policy, never by a model-supplied parameter.

use kroma_core::budget::BudgetConfig;
use kroma_core::sandbox::{
    ClassRule, ExecutionClass, ExecutionMode, ExecutionPolicy, SandboxBackend, SandboxTemplate,
};
use kroma_core::tools::bash::execute_bash;
use kroma_core::types::ErrorCode;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::sync::watch;

fn scratch(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("kroma-t09-{}-{}", tag, uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    root
}

async fn run_bash(
    ws: &Path,
    command: &str,
    policy: &ExecutionPolicy,
) -> kroma_core::tools::ToolExecutionOutput {
    let state = ws.join(".kroma-state");
    std::fs::create_dir_all(&state).unwrap();
    let (_, rx) = watch::channel(false);
    execute_bash(
        ws,
        &state,
        &kroma_core::types::ToolCallId::new("call_bash"),
        &serde_json::json!({"command": command}).to_string(),
        &BudgetConfig::default(),
        rx,
        Duration::from_secs(2),
        policy,
    )
    .await
}

/// The class table is the contract: no per-call override exists, so a model cannot move a class.
#[test]
fn execution_class_matrix_is_explicit() {
    let isolated = ExecutionPolicy::isolated(SandboxTemplate::bwrap());
    for class in [
        ExecutionClass::AgentBash,
        ExecutionClass::Verifier,
        ExecutionClass::Hook,
        ExecutionClass::Probe,
    ] {
        assert!(
            isolated.requires_sandbox(class),
            "{} must be sandboxed in an isolated run",
            class.as_str()
        );
        assert!(
            !isolated.rule(class).network,
            "{} must not get the network",
            class.as_str()
        );
    }
    // An MCP server is operator-installed infrastructure: host + network, stated explicitly.
    assert!(!isolated.requires_sandbox(ExecutionClass::McpServer));
    assert!(isolated.rule(ExecutionClass::McpServer).network);
    assert!(matches!(isolated.mode(), ExecutionMode::Isolated(_)));

    // Host mode is an explicit decision, and it says so for every class.
    let host = ExecutionPolicy::host();
    for class in ExecutionClass::all() {
        assert!(!host.requires_sandbox(class));
        assert!(host.rule(class).network);
    }
    assert_eq!(host.mode(), &ExecutionMode::Host);

    // Narrowing is possible, and only in the safe direction is it allowed to widen: an explicit
    // rule replaces the class default.
    let narrowed = ExecutionPolicy::isolated(SandboxTemplate::bwrap()).with_rule(
        ExecutionClass::Hook,
        ClassRule {
            sandboxed: false,
            network: true,
        },
    );
    assert!(!narrowed.requires_sandbox(ExecutionClass::Hook));
    assert!(narrowed.requires_sandbox(ExecutionClass::AgentBash));
}

/// The regression the task card names: isolation covers the agent's `bash`, not only the scorer.
#[tokio::test]
async fn sandbox_required_covers_agent_bash_not_only_scorer() {
    let root = scratch("bash-isolated");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let policy = ExecutionPolicy::isolated(SandboxTemplate::bwrap());

    let inside = run_bash(&ws, "printf ok > inside.txt", &policy).await;
    assert_eq!(
        inside.outcome,
        kroma_core::messages::ToolOutcome::Success,
        "a write inside the workspace must work: {:?}",
        inside.content
    );
    assert_eq!(
        inside.meta.execution_backend.as_deref(),
        Some("bwrap"),
        "the result must report the backend that actually ran the command"
    );
    assert!(
        ws.join("inside.txt").exists(),
        "the sandboxed write must land in the workspace"
    );

    // The parent of the workspace is not bound inside the sandbox. `..` from the guest workspace is
    // the sandbox's own ephemeral root, so the command may well "succeed" there — what matters is
    // that nothing appears on the host, because that root is a private mount namespace that dies
    // with the process. (Asserting the command *fails* would be asserting the wrong property.)
    let escape = run_bash(&ws, "printf escaped > ../escaped.txt", &policy).await;
    assert!(
        !root.join("escaped.txt").exists(),
        "the sandbox must not create files outside the workspace on the host (exit: {:?})",
        escape.outcome
    );
    assert!(
        !Path::new("/escaped.txt").exists(),
        "the escape must not reach the host root either"
    );

    // An absolute path to the parent directory is refused for the same reason.
    let absolute = run_bash(
        &ws,
        &format!("printf escaped > {}/absolute.txt", root.display()),
        &policy,
    )
    .await;
    assert_ne!(absolute.outcome, kroma_core::messages::ToolOutcome::Success);
    assert!(!root.join("absolute.txt").exists());

    let _ = std::fs::remove_dir_all(&root);
}

/// Isolation also covers the network, which is what makes the policy worth anything.
#[tokio::test]
async fn isolated_bash_has_no_network() {
    let root = scratch("bash-net");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let policy = ExecutionPolicy::isolated(SandboxTemplate::bwrap());

    // `/dev/tcp` is a bash builtin: no external tool needed, and it fails without a network stack.
    let out = run_bash(
        &ws,
        "exec 3<>/dev/tcp/1.1.1.1/80 && printf reachable",
        &policy,
    )
    .await;
    assert_ne!(
        out.outcome,
        kroma_core::messages::ToolOutcome::Success,
        "an isolated class must not reach the network: {:?}",
        out.content
    );
    assert_eq!(out.meta.execution_backend.as_deref(), Some("bwrap"));

    // The same command on an explicitly host policy is the control: it is not the command that is
    // broken, it is the boundary that works.
    let host = ExecutionPolicy::host();
    let control = run_bash(&ws, "printf reachable", &host).await;
    assert_eq!(control.outcome, kroma_core::messages::ToolOutcome::Success);
    assert_eq!(control.meta.execution_backend.as_deref(), Some("host"));

    let _ = std::fs::remove_dir_all(&root);
}

/// No backend means the command is refused — never run on the host instead.
#[tokio::test]
async fn no_backend_means_refuse_required_execution() {
    let root = scratch("bash-nobackend");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    // A container runtime that cannot exist: the policy must refuse, not fall back.
    let policy = ExecutionPolicy::isolated(SandboxTemplate::new(SandboxBackend::Container {
        runtime: "kroma-no-such-runtime".to_string(),
        image: "kroma-no-such-image".to_string(),
    }));

    let out = run_bash(&ws, "printf ran > side-effect.txt", &policy).await;
    assert_eq!(
        out.outcome,
        kroma_core::messages::ToolOutcome::Error {
            code: ErrorCode::SandboxUnavailable
        },
        "expected a fail-closed refusal, got {:?} / {:?}",
        out.outcome,
        out.content
    );
    assert_eq!(
        out.meta.execution_backend.as_deref(),
        Some("none:refused"),
        "a refused spawn must not claim a backend"
    );
    assert!(
        !ws.join("side-effect.txt").exists(),
        "the refused command must not have run anywhere"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// The reported backend is the one that ran, so a host run cannot masquerade as isolated.
#[tokio::test]
async fn host_policy_reports_the_host_backend() {
    let root = scratch("bash-host");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let out = run_bash(&ws, "printf host > host.txt", &ExecutionPolicy::host()).await;
    assert_eq!(out.outcome, kroma_core::messages::ToolOutcome::Success);
    assert_eq!(out.meta.execution_backend.as_deref(), Some("host"));
    assert!(ws.join("host.txt").exists());
    let _ = std::fs::remove_dir_all(&root);
}

/// A worker reaching for the parent through an absolute path is refused inside the boundary; the
/// worktree/CAS view is not an OS security boundary on its own, so this is enforced by the sandbox.
#[tokio::test]
async fn worker_cannot_write_parent_via_absolute_path() {
    let root = scratch("worker-parent");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let parent_file = root.join("parent.txt");
    std::fs::write(&parent_file, b"original").unwrap();

    let policy = ExecutionPolicy::isolated(SandboxTemplate::bwrap());
    let out = run_bash(
        &ws,
        &format!("printf overwritten > {}", parent_file.display()),
        &policy,
    )
    .await;
    assert_ne!(out.outcome, kroma_core::messages::ToolOutcome::Success);
    assert_eq!(
        std::fs::read(&parent_file).unwrap(),
        b"original",
        "the parent workspace must be untouched"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// T09 (KR-18): the boundary is a security decision, so only the operator may declare it. A
/// workspace that sets it is refused (reported), not honoured.
#[test]
fn workspace_cannot_declare_the_execution_boundary() {
    use kroma_core::config_file::{ConfigOverrides, KromaConfigFile, ResolvedConfig};
    let user = KromaConfigFile::parse_str("model = \"m\"\n").unwrap();
    let workspace = KromaConfigFile::parse_str("execution_sandbox = \"host\"\n").unwrap();
    let resolved = ResolvedConfig::resolve(
        &user,
        &workspace,
        &Default::default(),
        &ConfigOverrides::default(),
        None,
        PathBuf::from("/tmp/ws/.kroma.toml"),
    );
    assert_eq!(
        resolved.execution_sandbox, None,
        "a workspace value must not decide the boundary"
    );
    assert!(
        resolved
            .rejected_workspace_keys
            .contains(&"execution_sandbox".to_string()),
        "the refusal must be visible, got {:?}",
        resolved.rejected_workspace_keys
    );

    // The operator layer is honoured.
    let user = KromaConfigFile::parse_str("execution_sandbox = \"bwrap\"\n").unwrap();
    let resolved = ResolvedConfig::resolve(
        &user,
        &KromaConfigFile::default(),
        &Default::default(),
        &ConfigOverrides::default(),
        None,
        PathBuf::from("/tmp/ws/.kroma.toml"),
    );
    assert_eq!(resolved.execution_sandbox.as_deref(), Some("bwrap"));
}

/// An unrecognised boundary fails at configuration time instead of silently running on the host.
#[test]
fn invalid_execution_boundary_fails_at_start() {
    use kroma_core::config_file::{execution_policy_from_config, validate_execution_sandbox};
    for good in ["host", "bwrap", "container:docker:kroma:latest"] {
        validate_execution_sandbox("execution_sandbox", good)
            .unwrap_or_else(|e| panic!("{good} should be valid: {e}"));
    }
    for bad in ["docker", "sandbox", "container:", "container:docker", ""] {
        assert!(
            validate_execution_sandbox("execution_sandbox", bad).is_err(),
            "{bad:?} must be refused"
        );
    }

    // The mapping is explicit and total over the accepted values.
    assert_eq!(
        execution_policy_from_config(None).unwrap().mode(),
        &ExecutionMode::Host
    );
    assert_eq!(
        execution_policy_from_config(Some("host")).unwrap().mode(),
        &ExecutionMode::Host
    );
    assert!(matches!(
        execution_policy_from_config(Some("bwrap")).unwrap().mode(),
        ExecutionMode::Isolated(_)
    ));
    let container = execution_policy_from_config(Some("container:docker:img:1")).unwrap();
    match container.mode() {
        ExecutionMode::Isolated(template) => assert_eq!(
            &template.backend,
            &SandboxBackend::Container {
                runtime: "docker".to_string(),
                image: "img:1".to_string()
            }
        ),
        other => panic!("expected an isolated container policy, got {other:?}"),
    }
}

/// R01: a real parent run that declares `bwrap` must hand that boundary to the worker it spawns.
///
/// This exercises the production chain — `AgentRunner` → `spawn_subagent` →
/// `ProviderSubagentBackend` → child `AgentRunner` → `bash` — instead of calling `run_bash`
/// directly. The reported defect lived exactly in the gap between those two: only the parent
/// runtime was configured, the child runtime was re-created with `ExecutionPolicy::host()`, and the
/// worker could write anywhere the OS user could. The child's own `bash` result has to name
/// `bwrap`, and the outside marker must not exist.
#[tokio::test]
async fn worker_inherits_the_parent_execution_boundary_through_spawn_subagent() {
    use kroma_core::agent_loop::{build_runtime_with_registry, AgentRunConfig, AgentRunner};
    use kroma_core::env::{ExecutionEnv, LocalHostEnv};
    use kroma_core::events::EventLogger;
    use kroma_core::messages::{AssistantTurn, FinishReason, TokenUsage, ToolCall, TranscriptItem};
    use kroma_core::provider::{ModelProvider, ProviderCompletion, ProviderError, ProviderRequest};
    use kroma_core::revision::SnapshotStore;
    use kroma_core::session::SessionJournal;
    use kroma_core::tool::policy::{NonInteractiveApprovalHandler, PolicyEngine, PolicyMode};
    use kroma_core::tool::registry::ToolRegistry;
    use kroma_core::types::ToolCallId;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::{Arc, Mutex};
    use tokio::sync::watch;

    if kroma_core::sandbox::SandboxSpec::bwrap("/tmp")
        .probe()
        .is_err()
    {
        panic!(
            "bwrap is required to prove the parent→worker boundary and is not usable on this host"
        );
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
        ) -> Pin<Box<dyn Future<Output = Result<ProviderCompletion, ProviderError>> + Send + 'a>>
        {
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

    let root = scratch("parent-child-policy");
    let ws = root.join("ws");
    let state = root.join("state");
    std::fs::create_dir_all(ws.join("src")).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(ws.join("src/lib.rs"), "pub const VALUE: i32 = 1;\n").unwrap();
    let outside_marker = root.join("outside-marker.txt");
    assert!(!outside_marker.exists());

    let child_backend: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let child_backend_c = Arc::clone(&child_backend);
    let parent_turns = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let child_turns = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let parent_turns_c = Arc::clone(&parent_turns);
    let child_turns_c = Arc::clone(&child_turns);
    let marker_for_child = outside_marker.clone();

    let provider = Arc::new(ScriptProvider(move |req: ProviderRequest| {
        if req.system_prompt.contains("[SUBAGENT ROLE: worker") {
            let turn = child_turns_c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if turn == 0 {
                Ok(tool_turn(
                    "child_bash_1",
                    "bash",
                    serde_json::json!({
                        "command": format!("printf pwned > {}", marker_for_child.display())
                    }),
                ))
            } else {
                // The child's transcript holds the bash result: record the backend that ran it so
                // the assertion fails loudly if the child fell back to the host.
                let backend = req.messages.iter().find_map(|item| match item {
                    TranscriptItem::ToolResult(result) if result.tool_name == "bash" => {
                        result.meta.execution_backend.clone()
                    }
                    _ => None,
                });
                *child_backend_c.lock().unwrap() = backend;
                Ok(stop_turn("worker finished"))
            }
        } else {
            let turn = parent_turns_c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if turn == 0 {
                Ok(tool_turn(
                    "call_worker_1",
                    "spawn_subagent",
                    serde_json::json!({
                        "role": "worker",
                        "task": "try to write outside the workspace",
                        "max_turns": 3
                    }),
                ))
            } else {
                Ok(stop_turn("worker call returned"))
            }
        }
    }));

    let env = Arc::new(LocalHostEnv::new(&ws, state.clone()).unwrap());
    let policy_engine = PolicyEngine::new(
        PolicyMode::FullAutonomous,
        Arc::new(NonInteractiveApprovalHandler),
    );
    let execution_policy = ExecutionPolicy::isolated(SandboxTemplate::bwrap());
    let runtime = build_runtime_with_registry(
        ToolRegistry::default(),
        env.clone(),
        policy_engine,
        BudgetConfig::default(),
    )
    // The parent run declares the boundary. Everything below is the product path.
    .with_execution_policy(execution_policy.clone());
    let journal = SessionJournal::create(state.join("sessions"), env.workspace().root()).unwrap();
    let store = Arc::new(SnapshotStore::new(state.join("snapshots")));
    let (_cancel_tx, cancel_rx) = watch::channel(false);
    let cfg = AgentRunConfig::new(
        "prove the worker inherits the parent boundary",
        BudgetConfig::default(),
    );
    let logger = EventLogger::new(cfg.run_id, Some(state.clone()), false).unwrap();
    let runner = AgentRunner::new(cfg, provider, runtime, cancel_rx)
        .with_session_journal(journal)
        .with_snapshot_store(store)
        .with_max_subagents(2);

    let (summary, _logger) = runner.run(logger).await;
    let _ = std::fs::remove_dir_all(&root);

    assert!(
        !outside_marker.exists(),
        "the parent declared bwrap but the worker created a host file outside its workspace"
    );
    assert_eq!(
        child_backend.lock().unwrap().as_deref(),
        Some("bwrap"),
        "the worker's bash must report the sandbox backend the parent declared (terminal: {:?})",
        summary.terminal_reason
    );
    assert_eq!(
        summary.status,
        kroma_core::types::RunStatus::Completed,
        "the parent run itself must complete: {:?}",
        summary.terminal_reason
    );
}
