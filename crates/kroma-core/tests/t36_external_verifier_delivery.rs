//! R02: the held-out verifier must be deliverable into an isolated run **without** moving the oracle
//! into the agent-writable workspace.
//!
//! The scoring contract is that the verifier is independent of the work being scored. Two failures
//! break it:
//!
//! * the verifier cannot reach its own files from inside the sandbox, so it exits non-zero and the
//!   harness records a *functional* rejection (`VerifyFailure::Failed`, `verdict_unknown = None`);
//! * the "fix" is to copy the oracle into the workspace, which the agent can then rewrite.
//!
//! These tests pin the safe behaviour: host files named by the verifier argv are delivered read-only
//! at explicit guest paths, the verifier really runs and resolves the task, it cannot rewrite itself,
//! and a verifier whose files cannot be delivered is *unscorable* (no verdict) rather than a candidate
//! failure. The host controls prove each assertion is live.

#![cfg(unix)]

use std::path::{Path, PathBuf};

use kroma_core::bench::{
    BenchExecutor, BenchStrategyConfig, BenchTask, ProviderBenchExecutor, ProviderBenchSettings,
    VerifyCommandLimits,
};
use kroma_core::messages::{AssistantTurn, FinishReason, TokenUsage, ToolCall};
use kroma_core::provider::FakeProvider;
use kroma_core::sandbox::{ExecutionPolicy, SandboxSpec, SandboxTemplate};
use kroma_core::tool::policy::PolicyMode;
use kroma_core::types::ToolCallId;
use std::sync::Arc;
use std::time::Duration;

fn unique_root(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("kroma-t36-{}-{}", tag, uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).expect("create scratch root");
    root
}

/// Freeze a host file the way the operator/CLI does, for tests that hold a valid oracle.
fn identity(path: &Path) -> kroma_core::prereg::VerifierFileIdentity {
    kroma_core::prereg::VerifierFileIdentity::freeze(path).expect("freeze trusted verifier file")
}

fn require_bwrap() {
    if SandboxSpec::bwrap("/tmp").probe().is_err() {
        panic!("bwrap is required to prove verifier delivery and is not usable on this host");
    }
}

/// The concrete spec the bench builds for an isolated verifier: the policy's template resolved
/// against the scored workspace.
fn isolated_spec(workspace: &Path) -> SandboxSpec {
    SandboxTemplate::bwrap().resolve(workspace.to_path_buf())
}

fn task_with_verifier(verify: Vec<String>) -> BenchTask {
    BenchTask {
        id: "oracle".to_string(),
        prompt: "answer".to_string(),
        files: [("answer.txt".to_string(), "correct".to_string())].into(),
        expect_contains: vec![("answer.txt".to_string(), "correct".to_string())],
        expect_absent: Vec::new(),
        regression_guard: false,
        verify_command: Some(verify),
    }
}

/// The verifier script itself: it resolves only if it is *read-only*, so a writable delivery fails
/// the run instead of passing silently. It never writes into the workspace.
const ORACLE: &str = r#"
import pathlib, sys
me = pathlib.Path(__file__)
try:
    me.write_text(me.read_text() + "\n# tampered\n")
    writable = True
except OSError:
    writable = False
sys.exit(0 if not writable else 3)
"#;

/// A held-out verifier outside the workspace runs inside the sandbox, resolves the task, and cannot
/// rewrite itself. The host control runs the same script where the write succeeds, so the read-only
/// assertion cannot pass vacuously.
#[tokio::test]
async fn an_external_verifier_runs_read_only_inside_the_sandbox() {
    require_bwrap();
    let root = unique_root("external-oracle");
    let workspace = root.join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(workspace.join("answer.txt"), "correct").unwrap();
    let oracle_dir = root.join("oracle");
    std::fs::create_dir_all(&oracle_dir).unwrap();
    let oracle = oracle_dir.join("verify.py");
    std::fs::write(&oracle, ORACLE).unwrap();
    let before = std::fs::read(&oracle).unwrap();

    let task = task_with_verifier(vec!["python3".to_string(), oracle.display().to_string()]);
    let limits = VerifyCommandLimits::default();
    let spec = isolated_spec(&workspace);

    let isolated = task
        .evaluate_with_sandbox_and_oracle(
            &workspace,
            &limits,
            Some(&spec),
            None,
            &[identity(&oracle)],
        )
        .await;
    assert!(
        isolated.resolved,
        "the delivered external verifier must run and resolve the task: {:?}",
        isolated.failures
    );
    assert_eq!(
        isolated.verdict_unknown, None,
        "a delivered verifier is a real verdict, not an unknown: {:?}",
        isolated.failures
    );
    assert_eq!(
        std::fs::read(&oracle).unwrap(),
        before,
        "the verifier was mounted writable and rewrote its own host file"
    );

    // Control: on the host the same script *can* rewrite itself, so it exits 3 and the task does not
    // resolve. That proves the read-only property above is what made the isolated run pass.
    let host = task
        .evaluate_with_sandbox(&workspace, &limits, None, None)
        .await;
    assert!(
        !host.resolved,
        "control: a writable oracle must be able to rewrite itself: {:?}",
        host.failures
    );
    assert_eq!(
        host.verdict_unknown, None,
        "control: a verifier that ran and failed is a functional failure, not unknown"
    );
    assert_ne!(
        std::fs::read(&oracle).unwrap(),
        before,
        "control: the host run must have modified the oracle"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// A verifier whose files cannot be delivered measures nothing: the arm is unscorable, not failed.
/// The host control shows the same missing path is a *functional* failure there, which is exactly the
/// classification this test forbids for the isolated run.
#[tokio::test]
async fn an_undeliverable_verifier_is_unscorable_not_a_candidate_failure() {
    require_bwrap();
    let root = unique_root("missing-oracle");
    let workspace = root.join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(workspace.join("answer.txt"), "correct").unwrap();
    let missing = root.join("oracle").join("does-not-exist.py");

    let task = task_with_verifier(vec!["python3".to_string(), missing.display().to_string()]);
    let limits = VerifyCommandLimits::default();
    let spec = isolated_spec(&workspace);

    // The operator declared the oracle at admission; it is gone by dispatch time.
    std::fs::create_dir_all(missing.parent().unwrap()).unwrap();
    std::fs::write(&missing, "import sys\nsys.exit(0)\n").unwrap();
    let declared = identity(&missing);
    std::fs::remove_file(&missing).unwrap();
    let isolated = task
        .evaluate_with_sandbox_and_oracle(&workspace, &limits, Some(&spec), None, &[declared])
        .await;
    let unknown = isolated
        .verdict_unknown
        .as_deref()
        .expect("an undeliverable oracle must record an unknown verdict");
    assert!(
        unknown.contains("not available") || unknown.contains("no verdict"),
        "the unknown verdict must name the delivery failure: {unknown}"
    );
    assert!(
        isolated
            .failures
            .iter()
            .all(|failure| { !failure.contains("exit=") || failure.contains("not available") }),
        "an undeliverable oracle must not be reported as the verifier exiting non-zero: {:?}",
        isolated.failures
    );

    // Control: with no sandbox the command really runs, Python exits 2, and the harness classifies a
    // functional failure (no unknown). That is the misclassification the isolated path must not have.
    let host = task
        .evaluate_with_sandbox(&workspace, &limits, None, None)
        .await;
    assert!(!host.resolved);
    assert_eq!(
        host.verdict_unknown, None,
        "control: the host run cannot see the delivery problem, so it records a functional failure"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// Nothing is delivered implicitly: a host file the verifier argv names but the operator did not
/// declare is refused (unscorable) and the verifier never runs. `/etc/shadow` is the sharpest case —
/// "deliver whatever the argv names" would be a host-secret exfiltration path, and a blacklist is not
/// authorization either.
#[tokio::test]
async fn an_undeclared_host_file_is_refused_and_never_delivered() {
    require_bwrap();
    assert!(
        std::fs::read_to_string("/etc/shadow").is_ok(),
        "the control requires a host-readable /etc/shadow"
    );
    let root = unique_root("undeclared-oracle");
    let workspace = root.join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(workspace.join("answer.txt"), "correct").unwrap();
    // Written inside the scored workspace (the guest cwd) so the host can observe that it ran.
    let marker = workspace.join("verifier-ran.marker");
    let oracle_dir = root.join("oracle");
    std::fs::create_dir_all(&oracle_dir).unwrap();
    let oracle = oracle_dir.join("verify.py");
    let script = "import pathlib,sys\npathlib.Path('verifier-ran.marker').write_text('ran')\nsys.exit(0 if pathlib.Path('/etc/shadow').exists() else 1)\n";
    std::fs::write(&oracle, script).unwrap();
    let task = task_with_verifier(vec!["python3".to_string(), oracle.display().to_string()]);
    let limits = VerifyCommandLimits::default();
    let spec = isolated_spec(&workspace);

    // No trusted oracle declaration at all: the verifier may not receive its own script either.
    let isolated = task
        .evaluate_with_sandbox(&workspace, &limits, Some(&spec), None)
        .await;
    let unknown = isolated
        .verdict_unknown
        .as_deref()
        .expect("an undeclared oracle must be unscorable");
    assert!(
        unknown.contains("not a declared trusted verifier file"),
        "the refusal must name the missing authorization: {unknown}"
    );
    assert!(
        !marker.exists(),
        "the verifier must not run when its files were not authorized"
    );

    // Control: declaring it makes the same task scorable, and the declared file is the only thing
    // delivered — the verifier still cannot read /etc/shadow, which was never declared.
    let declared = task
        .evaluate_with_sandbox_and_oracle(
            &workspace,
            &limits,
            Some(&spec),
            None,
            &[identity(&oracle)],
        )
        .await;
    assert!(
        marker.exists(),
        "control: a declared oracle must run: {:?}",
        declared.failures
    );
    assert!(
        !declared.resolved,
        "control: /etc/shadow was never declared, so the verifier must not read it: {:?}",
        declared.checks
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// The argv itself never authorizes a mount. An absolute **data/secret** path is refused unless the
/// operator declared that exact file, and declaring some other file does not unlock it. `cat
/// /etc/shadow` is the sharpest case: it must stay denied, exactly as it is for the b13 contract.
#[tokio::test]
async fn an_absolute_data_path_is_never_mounted_by_the_argv_alone() {
    require_bwrap();
    assert!(
        std::fs::read_to_string("/etc/shadow").is_ok(),
        "the control requires a host-readable /etc/shadow"
    );
    let root = unique_root("data-path-oracle");
    let workspace = root.join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(workspace.join("answer.txt"), "correct").unwrap();
    let task = task_with_verifier(vec!["cat".to_string(), "/etc/shadow".to_string()]);
    let limits = VerifyCommandLimits::default();
    let spec = isolated_spec(&workspace);

    // 1. Nothing declared: the data path is refused, nothing is mounted, the verifier never runs.
    let undeclared = task
        .evaluate_with_sandbox(&workspace, &limits, Some(&spec), None)
        .await;
    let unknown = undeclared
        .verdict_unknown
        .as_deref()
        .expect("an undeclared data path must be unscorable");
    assert!(
        unknown.contains("/etc/shadow") && unknown.contains("not a declared trusted verifier file"),
        "the refusal must name the data path and the missing authorization: {unknown}"
    );
    assert!(
        !undeclared
            .checks
            .iter()
            .any(|check| check.contains("verify_command")),
        "the verifier must not have run: {:?}",
        undeclared.checks
    );

    // 2. A *different* file declared: authorization is per path, so the secret is still refused.
    let decoy = root.join("decoy.py");
    std::fs::write(&decoy, "print('decoy')\n").unwrap();
    let decoy_declared = task
        .evaluate_with_sandbox_and_oracle(
            &workspace,
            &limits,
            Some(&spec),
            None,
            &[identity(&decoy)],
        )
        .await;
    let unknown = decoy_declared
        .verdict_unknown
        .as_deref()
        .expect("declaring another file must not authorize the secret");
    assert!(
        unknown.contains("/etc/shadow"),
        "the refusal must still name the secret path: {unknown}"
    );
    assert!(
        !decoy_declared
            .checks
            .iter()
            .any(|check| check.contains("verify_command")),
        "the verifier must not have run: {:?}",
        decoy_declared.checks
    );

    // 3. Control: on the host the same command reads the secret, so this is isolation + delivery
    //    authorization, not a command that cannot work.
    let host = task
        .evaluate_with_sandbox(&workspace, &limits, None, None)
        .await;
    assert!(
        host.resolved,
        "control: the host run must read /etc/shadow: {:?}",
        host.failures
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// A data path outside the system roots is refused too, and a symlink alias to a secret is refused
/// even when the *alias* is what a task names: authorization is the operator's declaration, never the
/// argv, and it never follows a link into a file nobody declared.
#[tokio::test]
async fn a_data_path_outside_the_system_roots_and_a_symlink_alias_are_refused() {
    require_bwrap();
    let root = unique_root("tmp-data-oracle");
    let workspace = root.join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(workspace.join("answer.txt"), "correct").unwrap();
    let secret = root.join("host-secret.txt");
    std::fs::write(&secret, "host-secret").unwrap();
    let alias = root.join("alias.py");
    std::os::unix::fs::symlink(&secret, &alias).unwrap();
    let limits = VerifyCommandLimits::default();
    let spec = isolated_spec(&workspace);

    // (a) an unauthorized /tmp data path: no mount, no verdict.
    let data_task = task_with_verifier(vec!["cat".to_string(), secret.display().to_string()]);
    let data = data_task
        .evaluate_with_sandbox(&workspace, &limits, Some(&spec), None)
        .await;
    let unknown = data
        .verdict_unknown
        .as_deref()
        .expect("an unauthorized data path must be unscorable");
    assert!(
        unknown.contains("not a declared trusted verifier file"),
        "the refusal must name the missing authorization: {unknown}"
    );

    // (b) a symlink alias to the same secret: authorization refuses it outright, and a file that is
    // replaced by a symlink *after* admission is caught at dispatch time.
    let refused = kroma_core::prereg::VerifierFileIdentity::freeze(&alias)
        .expect_err("a symlink must never be frozen as a trusted verifier file");
    assert!(
        refused.contains("symlink") || refused.contains("regular file"),
        "the refusal must name the symlink rule: {refused}"
    );
    let real = root.join("oracle.py");
    std::fs::write(&real, "import sys\nsys.exit(0)\n").unwrap();
    let frozen = identity(&real);
    std::fs::remove_file(&real).unwrap();
    std::os::unix::fs::symlink(&secret, &real).unwrap();
    let alias_task = task_with_verifier(vec!["cat".to_string(), real.display().to_string()]);
    let aliased = alias_task
        .evaluate_with_sandbox_and_oracle(&workspace, &limits, Some(&spec), None, &[frozen])
        .await;
    let unknown = aliased
        .verdict_unknown
        .as_deref()
        .expect("a post-admission symlink must be unscorable");
    assert!(
        unknown.contains("symlink") || unknown.contains("regular file"),
        "the refusal must name the symlink rule: {unknown}"
    );
    assert!(
        !aliased
            .checks
            .iter()
            .any(|check| check.contains("verify_command")),
        "the verifier must not have run: {:?}",
        aliased.checks
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// A declared path that is a symlink is refused: its target can change between authorization and
/// launch, which would deliver a file the operator never named.
#[tokio::test]
async fn a_symlinked_trusted_file_is_refused() {
    require_bwrap();
    let root = unique_root("symlink-oracle");
    let workspace = root.join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(workspace.join("answer.txt"), "correct").unwrap();
    let secret = root.join("secret.txt");
    std::fs::write(&secret, "host-secret").unwrap();
    let link = root.join("verify.py");
    std::os::unix::fs::symlink(&secret, &link).unwrap();
    let task = task_with_verifier(vec!["cat".to_string(), link.display().to_string()]);
    let spec = isolated_spec(&workspace);

    // Authorization refuses the symlink itself: the capability is frozen to a real file.
    let refused = kroma_core::prereg::VerifierFileIdentity::freeze(&link)
        .expect_err("a symlink must never be frozen as a trusted verifier file");
    assert!(
        refused.contains("symlink") || refused.contains("regular file"),
        "the refusal must name the symlink rule: {refused}"
    );
    // And the same link declared as the *target* still cannot be used to reach another file.
    let frozen = identity(&secret);
    let evaluation = task
        .evaluate_with_sandbox_and_oracle(
            &workspace,
            &VerifyCommandLimits::default(),
            Some(&spec),
            None,
            &[frozen],
        )
        .await;
    assert!(
        evaluation.resolved,
        "the declared target file itself is a valid oracle: {:?}",
        evaluation.failures
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// A verifier that lives inside the scored workspace The arm's agent (and its development
/// The oracle mounts belong to the held-out verifier only: the arm's agent (and its development
/// gate) must never see them — `/kroma-oracle` exists only inside the verifier's sandbox.
#[tokio::test]
async fn the_agent_never_sees_the_oracle_mount() {
    require_bwrap();
    let root = unique_root("agent-oracle-visibility");
    let workspace = root.join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let oracle = root.join("verify.py");
    std::fs::write(&oracle, "import sys\nsys.exit(0)\n").unwrap();

    fn stop() -> AssistantTurn {
        AssistantTurn {
            content: Some("done".to_string()),
            reasoning_blocks: Vec::new(),
            tool_calls: Vec::new(),
            finish_reason: FinishReason::Stop,
            usage: Some(TokenUsage {
                input_tokens: 10,
                cached_input_tokens: None,
                cache_creation_input_tokens: None,
                output_tokens: 2,
                reasoning_tokens: None,
            }),
        }
    }
    fn bash(command: &str) -> AssistantTurn {
        AssistantTurn {
            content: None,
            reasoning_blocks: Vec::new(),
            tool_calls: vec![ToolCall {
                call_id: ToolCallId::new("call-1"),
                name: "bash".to_string(),
                raw_arguments: serde_json::json!({ "command": command }).to_string(),
            }],
            finish_reason: FinishReason::ToolCalls,
            usage: Some(TokenUsage {
                input_tokens: 10,
                cached_input_tokens: None,
                cache_creation_input_tokens: None,
                output_tokens: 2,
                reasoning_tokens: None,
            }),
        }
    }

    let task = task_with_verifier(vec!["python3".to_string(), oracle.display().to_string()]);
    let settings = ProviderBenchSettings::new("fake-model", "chat-completions")
        .with_policy_mode(PolicyMode::FullAutonomous)
        .with_max_model_turns(2)
        .with_max_run_duration(Duration::from_secs(60))
        .with_execution_policy(ExecutionPolicy::isolated(SandboxTemplate::bwrap()))
        .with_trusted_verifier_file(identity(&oracle));
    let provider = FakeProvider::new(vec![bash("/kroma-oracle/0-verify.py"), stop()]);
    let executor = ProviderBenchExecutor::new(Arc::new(provider), settings);
    let strategy = BenchStrategyConfig::single_agent();
    let scope = kroma_core::job_scope::JobScope::open(&root, "arm").unwrap();
    let result = executor.execute(&task, &strategy, 100_000, &scope).await;
    scope.keep();

    assert!(
        result.resolved,
        "the declared oracle must still score the arm: {:?}",
        result.failure_trace
    );
    assert!(
        result
            .tool_trace
            .iter()
            .any(|trace| trace.contains("bash") && trace.contains("error")),
        "the agent must not be able to execute the oracle path, got {:?}",
        result.tool_trace
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// A verifier that lives inside the scored workspace is still addressed through the guest workspace
/// view, so an in-workspace oracle keeps working (it is the operator's choice, not a delivery rule).
#[tokio::test]
async fn an_in_workspace_verifier_is_addressed_through_the_guest_view() {
    require_bwrap();
    let root = unique_root("workspace-oracle");
    let workspace = root.join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(workspace.join("answer.txt"), "correct").unwrap();
    std::fs::write(
        workspace.join("verify.py"),
        "import pathlib,sys\nsys.exit(0 if pathlib.Path('answer.txt').read_text()=='correct' else 1)\n",
    )
    .unwrap();

    let task = task_with_verifier(vec![
        "python3".to_string(),
        workspace.join("verify.py").display().to_string(),
    ]);
    let spec = isolated_spec(&workspace);
    let evaluation = task
        .evaluate_with_sandbox(
            &workspace,
            &VerifyCommandLimits::default(),
            Some(&spec),
            None,
        )
        .await;
    assert!(
        evaluation.resolved,
        "an in-workspace verifier must be reached through the guest workspace view: {:?}",
        evaluation.failures
    );
    let _ = std::fs::remove_dir_all(&root);
}
