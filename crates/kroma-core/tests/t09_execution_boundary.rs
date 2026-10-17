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
