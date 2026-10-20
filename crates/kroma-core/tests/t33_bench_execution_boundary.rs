//! R02 / R05 / R06: the provider bench arm must actually run under the configuration the report
//! says it ran under.
//!
//! Each test drives the real [`ProviderBenchExecutor`] over a real per-arm `JobScope`, the way
//! `run_paired_benchmark` does, and observes the effect from outside the arm:
//!
//! * R02 — the arm's `bash`, its development gate and its held-out verifier all resolve the one
//!   operator-declared `ExecutionPolicy`. A host control proves each probe can really write the
//!   outside marker, so the isolated assertion is not vacuous.
//! * R05 — every arm dispatches against its own broker ledger, and the experiment ceiling is
//!   computed from the number of planned arms instead of being one arm's budget.
//! * R06 — the filtered registry handed to the agent is the one the runtime uses: an excluded tool
//!   is neither in the recorded schema nor executable.
//!
//! The isolation tests need `bwrap`; a host without it cannot prove the claim, so they fail loudly
//! rather than skipping.

#![cfg(unix)]

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use kroma_core::bench::{
    experiment_envelope, BenchExecutor, BenchStrategyConfig, BenchTask, ProviderBenchExecutor,
    ProviderBenchSettings,
};
use kroma_core::budget::BudgetConfig;
use kroma_core::completion_gate::WorkspaceCompletionGate;
use kroma_core::credential_broker;
use kroma_core::messages::{AssistantTurn, FinishReason, TokenUsage, ToolCall};
use kroma_core::provider::{
    DispatchBinding, FakeProvider, ModelProvider, ProviderCompletion, ProviderError,
    ProviderRequest,
};
use kroma_core::sandbox::{ExecutionPolicy, SandboxSpec, SandboxTemplate};
use kroma_core::tool::policy::PolicyMode;
use kroma_core::types::ToolCallId;
use tokio::sync::watch;

fn unique_root(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("kroma-t33-{}-{}", tag, uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).expect("create scratch root");
    root
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

fn stop_turn(input: u64, output: u64) -> AssistantTurn {
    AssistantTurn {
        content: Some("done".to_string()),
        reasoning_blocks: Vec::new(),
        tool_calls: Vec::new(),
        finish_reason: FinishReason::Stop,
        usage: usage(input, output),
    }
}

fn tool_turn(name: &str, arguments: serde_json::Value) -> AssistantTurn {
    AssistantTurn {
        content: None,
        reasoning_blocks: Vec::new(),
        tool_calls: vec![ToolCall {
            call_id: ToolCallId::new("call-1"),
            name: name.to_string(),
            raw_arguments: arguments.to_string(),
        }],
        finish_reason: FinishReason::ToolCalls,
        usage: usage(10, 2),
    }
}

fn bash_turn(command: &str) -> AssistantTurn {
    tool_turn("bash", serde_json::json!({ "command": command }))
}

fn write_turn(path: &str, content: &str) -> AssistantTurn {
    tool_turn(
        "write",
        serde_json::json!({ "path": path, "content": content }),
    )
}

fn task(id: &str, verify_command: Option<Vec<String>>) -> BenchTask {
    BenchTask {
        id: id.to_string(),
        prompt: "write the answer file".to_string(),
        files: Default::default(),
        expect_contains: vec![("answer.txt".to_string(), "correct".to_string())],
        expect_absent: Vec::new(),
        regression_guard: false,
        verify_command,
    }
}

fn isolated_bwrap() -> ExecutionPolicy {
    ExecutionPolicy::isolated(SandboxTemplate::bwrap())
}

/// The isolation tests need a real sandbox backend; a host without one cannot prove the claim.
fn require_bwrap() {
    if SandboxSpec::bwrap("/tmp").probe().is_err() {
        panic!("bwrap is required to prove the R02 bench boundary and is not usable on this host");
    }
}

fn settings(policy: ExecutionPolicy) -> ProviderBenchSettings {
    ProviderBenchSettings::new("fake-model", "chat-completions")
        .with_policy_mode(PolicyMode::FullAutonomous)
        .with_max_model_turns(3)
        .with_max_run_duration(Duration::from_secs(60))
        .with_execution_policy(policy)
}

async fn run_arm(
    root: &Path,
    executor: &ProviderBenchExecutor,
    task: &BenchTask,
    strategy: &BenchStrategyConfig,
    job: &str,
) -> kroma_core::bench::BenchArmResult {
    let scope = kroma_core::job_scope::JobScope::open(root, job).expect("open arm scope");
    let result = executor.execute(task, strategy, 100_000, &scope).await;
    scope.keep();
    result
}

/// R02: the arm's `bash` runs inside the operator's execution policy. The host control writes the
/// marker, the isolated arm cannot.
#[tokio::test]
async fn the_arm_agent_bash_runs_inside_the_execution_policy() {
    require_bwrap();
    let root = unique_root("agent-bash");
    let marker = root.join("outside-agent-marker.txt");
    let command = format!("printf host-write > '{}'", marker.display());
    let task = task("agent-bash", None);
    let strategy = BenchStrategyConfig::single_agent();

    // Control: with the historical host policy the same call writes the host marker.
    let host_root = root.join("host");
    std::fs::create_dir_all(&host_root).unwrap();
    let host_provider = FakeProvider::new(vec![bash_turn(&command), stop_turn(10, 2)]);
    let host = ProviderBenchExecutor::new(
        Arc::new(host_provider),
        settings(ExecutionPolicy::host()).with_max_model_turns(2),
    );
    let host_result = run_arm(&host_root, &host, &task, &strategy, "host").await;
    assert!(
        marker.exists(),
        "the control arm must be able to write the host marker, or the isolation assertion is \
         vacuous: {host_result:?}"
    );
    std::fs::remove_file(&marker).unwrap();

    // The same arm under bwrap: the only writable view is the arm workspace.
    let isolated_root = root.join("isolated");
    std::fs::create_dir_all(&isolated_root).unwrap();
    let isolated_provider = FakeProvider::new(vec![bash_turn(&command), stop_turn(10, 2)]);
    let isolated =
        ProviderBenchExecutor::new(Arc::new(isolated_provider), settings(isolated_bwrap()));
    let isolated_result = run_arm(&isolated_root, &isolated, &task, &strategy, "isolated").await;
    assert!(
        !marker.exists(),
        "the bench agent wrote outside its workspace despite execution_sandbox=bwrap: \
         {isolated_result:?}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// R02: the development gate (the operator-authorized `verify_command` shown to the agent) runs
/// under the same policy. A gate that stayed on the host would leak the very boundary the arm
/// claims to measure.
#[tokio::test]
async fn the_arm_development_gate_runs_inside_the_execution_policy() {
    require_bwrap();
    let root = unique_root("dev-gate");
    let marker = root.join("outside-gate-marker.txt");
    let gate_command = vec![
        "python3".to_string(),
        "-c".to_string(),
        format!("open({:?}, 'w').write('gate-host-write')", marker),
    ];
    let task = task("dev-gate", None);
    let strategy = BenchStrategyConfig::single_agent();

    let host_root = root.join("host");
    std::fs::create_dir_all(&host_root).unwrap();
    let host_provider = FakeProvider::new(vec![
        write_turn("answer.txt", "correct"),
        stop_turn(10, 2),
        stop_turn(10, 2),
    ]);
    let host =
        ProviderBenchExecutor::new(Arc::new(host_provider), settings(ExecutionPolicy::host()))
            .with_development_gate(Arc::new(WorkspaceCompletionGate::new(
                Some(gate_command.clone()),
                Duration::from_secs(30),
            )));
    let host_result = run_arm(&host_root, &host, &task, &strategy, "host").await;
    assert!(
        marker.exists(),
        "the control development gate must be able to write the host marker: {host_result:?}"
    );
    std::fs::remove_file(&marker).unwrap();

    let isolated_root = root.join("isolated");
    std::fs::create_dir_all(&isolated_root).unwrap();
    let isolated_provider = FakeProvider::new(vec![
        write_turn("answer.txt", "correct"),
        stop_turn(10, 2),
        stop_turn(10, 2),
    ]);
    let isolated =
        ProviderBenchExecutor::new(Arc::new(isolated_provider), settings(isolated_bwrap()))
            .with_development_gate(Arc::new(
                WorkspaceCompletionGate::new(Some(gate_command.clone()), Duration::from_secs(30))
                    .with_execution_policy(isolated_bwrap()),
            ));
    let isolated_result = run_arm(&isolated_root, &isolated, &task, &strategy, "isolated").await;
    assert!(
        !marker.exists(),
        "the bench development gate wrote outside the arm workspace: {isolated_result:?}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// R02: the held-out verifier is the scorer, so it must run under the same policy even when the
/// legacy scorer-only `sandbox` setting was never passed.
#[tokio::test]
async fn the_held_out_verifier_runs_inside_the_execution_policy() {
    require_bwrap();
    assert!(
        std::fs::read_to_string("/etc/shadow").is_ok(),
        "the control requires a host-readable /etc/shadow"
    );
    let root = unique_root("verifier");
    let marker = root.join("outside-verifier-marker.txt");
    let verify = vec![
        "sh".to_string(),
        "-c".to_string(),
        format!(
            "cat /etc/shadow && printf verifier-host-write > '{}'",
            marker.display()
        ),
    ];
    let task = task("verifier", Some(verify));
    let strategy = BenchStrategyConfig::single_agent();

    let host_root = root.join("host");
    std::fs::create_dir_all(&host_root).unwrap();
    let host_provider =
        FakeProvider::new(vec![write_turn("answer.txt", "correct"), stop_turn(10, 2)]);
    let host =
        ProviderBenchExecutor::new(Arc::new(host_provider), settings(ExecutionPolicy::host()));
    let host_result = run_arm(&host_root, &host, &task, &strategy, "host").await;
    assert!(
        host_result.resolved,
        "the control verifier must resolve on the host: {host_result:?}"
    );
    assert!(
        marker.exists(),
        "the control verifier must write the host marker: {host_result:?}"
    );
    std::fs::remove_file(&marker).unwrap();

    let isolated_root = root.join("isolated");
    std::fs::create_dir_all(&isolated_root).unwrap();
    let isolated_provider =
        FakeProvider::new(vec![write_turn("answer.txt", "correct"), stop_turn(10, 2)]);
    let isolated =
        ProviderBenchExecutor::new(Arc::new(isolated_provider), settings(isolated_bwrap()));
    let isolated_result = run_arm(&isolated_root, &isolated, &task, &strategy, "isolated").await;
    assert!(
        !isolated_result.resolved,
        "a verifier that cannot read the host secret must not resolve the arm: {isolated_result:?}"
    );
    assert!(
        !marker.exists(),
        "the held-out verifier wrote outside the sealed workspace: {isolated_result:?}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// R06: the arm's declared `tool_filter` reaches the runtime. The excluded `write` is neither in
/// the recorded exposure nor executable, while the unfiltered control does write the file.
#[tokio::test]
async fn the_filtered_registry_is_the_one_the_arm_runs_with() {
    let root = unique_root("tool-filter");
    let task = task("tool-filter", None);

    let filtered = BenchStrategyConfig {
        name: "read_only_tools".to_string(),
        tool_filter: Some(vec!["read".to_string()]),
        ..BenchStrategyConfig::single_agent()
    };
    let filtered_root = root.join("filtered");
    std::fs::create_dir_all(&filtered_root).unwrap();
    let provider = FakeProvider::new(vec![write_turn("answer.txt", "correct"), stop_turn(10, 2)]);
    let executor =
        ProviderBenchExecutor::new(Arc::new(provider), settings(ExecutionPolicy::host()));
    let filtered_result = run_arm(&filtered_root, &executor, &task, &filtered, "filtered").await;
    assert_eq!(
        filtered_result.exposed_tools,
        vec!["read".to_string()],
        "the arm must expose exactly its filtered tool set: {filtered_result:?}"
    );
    assert!(
        !filtered_root.join("filtered").join("answer.txt").exists(),
        "an excluded tool must not execute: {filtered_result:?}"
    );

    // Control: without the filter the same call lands, so the assertion above is about the filter.
    let unfiltered = BenchStrategyConfig::single_agent();
    let unfiltered_root = root.join("unfiltered");
    std::fs::create_dir_all(&unfiltered_root).unwrap();
    let provider = FakeProvider::new(vec![write_turn("answer.txt", "correct"), stop_turn(10, 2)]);
    let executor =
        ProviderBenchExecutor::new(Arc::new(provider), settings(ExecutionPolicy::host()));
    let unfiltered_result = run_arm(
        &unfiltered_root,
        &executor,
        &task,
        &unfiltered,
        "unfiltered",
    )
    .await;
    assert!(
        unfiltered_result.exposed_tools.iter().any(|t| t == "write"),
        "the control arm must expose write: {unfiltered_result:?}"
    );
    assert!(
        unfiltered_root
            .join("unfiltered")
            .join("answer.txt")
            .exists(),
        "the control write must land: {unfiltered_result:?}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// A provider with a stated dispatch binding, so the broker can gate it, whose turns carry a
/// chosen provider-reported usage. It never opens a socket.
struct BoundFakeProvider {
    inner: FakeProvider,
    binding: DispatchBinding,
}

impl ModelProvider for BoundFakeProvider {
    fn dispatch_binding(&self) -> Option<DispatchBinding> {
        Some(self.binding.clone())
    }

    fn complete<'a>(
        &'a self,
        req: ProviderRequest,
        budget: &'a BudgetConfig,
        cancel_rx: watch::Receiver<bool>,
    ) -> Pin<Box<dyn Future<Output = Result<ProviderCompletion, ProviderError>> + Send + 'a>> {
        self.inner.complete(req, budget, cancel_rx)
    }
}

fn bound_provider(turns: Vec<AssistantTurn>) -> Arc<dyn ModelProvider> {
    Arc::new(BoundFakeProvider {
        inner: FakeProvider::new(turns),
        binding: DispatchBinding {
            endpoint: "https://api.example.com:443".to_string(),
            model: "fake-model".to_string(),
        },
    })
}

/// R05: the experiment ceiling covers every planned arm, and each arm dispatches against its own
/// ledger. The control wraps the same provider with one arm's budget — the historical behaviour —
/// and the second arm is denied before its first request.
#[tokio::test]
async fn each_arm_gets_its_own_broker_allowance_and_the_ceiling_covers_all_arms() {
    const PER_ARM: u64 = 20_000;
    const FIRST_ARM_USAGE: u64 = 19_000;
    let task = task("budget", None);
    let strategy = BenchStrategyConfig::single_agent();

    let root = unique_root("budget-fixed");
    // The owner's experiment ceiling: two planned arms × the per-arm budget.
    let total = experiment_envelope(PER_ARM, 2, None).expect("the planned arms fit the ceiling");
    assert_eq!(total, PER_ARM * 2);
    let shared = bound_provider(vec![
        stop_turn(FIRST_ARM_USAGE, 0),
        stop_turn(FIRST_ARM_USAGE, 0),
    ]);
    let provider = credential_broker::enforce(shared, "kroma-bench", total);
    let executor = ProviderBenchExecutor::new(
        provider,
        ProviderBenchSettings::new("fake-model", "chat-completions")
            .with_policy_mode(PolicyMode::FullAutonomous)
            .with_max_model_turns(1)
            .with_max_run_duration(Duration::from_secs(30)),
    );
    let first = run_arm(&root, &executor, &task, &strategy, "arm-a").await;
    assert_eq!(first.total_tokens, FIRST_ARM_USAGE);
    assert_eq!(first.request_attempts_used, 1, "{first:?}");

    let second = run_arm(&root, &executor, &task, &strategy, "arm-b").await;
    assert_eq!(
        second.request_attempts_used, 1,
        "the second arm must dispatch its first request from its own allowance: {second:?}"
    );
    assert_eq!(second.total_tokens, FIRST_ARM_USAGE, "{second:?}");

    // Control: one arm's budget as the *whole* experiment ceiling is the old wiring. The first arm
    // spends almost all of it, so the second arm's first dispatch is refused.
    let control_root = unique_root("budget-control");
    let control_shared = bound_provider(vec![
        stop_turn(FIRST_ARM_USAGE, 0),
        stop_turn(FIRST_ARM_USAGE, 0),
    ]);
    let control_provider = credential_broker::enforce(control_shared, "kroma-bench", PER_ARM);
    let control = ProviderBenchExecutor::new(
        control_provider,
        ProviderBenchSettings::new("fake-model", "chat-completions")
            .with_policy_mode(PolicyMode::FullAutonomous)
            .with_max_model_turns(1)
            .with_max_run_duration(Duration::from_secs(30)),
    );
    let control_first = run_arm(&control_root, &control, &task, &strategy, "arm-a").await;
    assert_eq!(control_first.request_attempts_used, 1, "{control_first:?}");
    let control_second = run_arm(&control_root, &control, &task, &strategy, "arm-b").await;
    assert_eq!(
        control_second.request_attempts_used, 0,
        "the shared single-arm ledger is expected to deny the second arm: {control_second:?}"
    );
    assert!(
        !control_second.finished_normally,
        "the denied arm must not look like a clean measurement: {control_second:?}"
    );
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&control_root);
}

/// R05: a declared experiment ceiling that cannot cover the planned arms is refused before the
/// first request, and arithmetic that would overflow is refused too.
#[test]
fn an_insufficient_experiment_ceiling_is_refused_before_any_arm() {
    let refused = experiment_envelope(30_000, 2, Some(30_000))
        .expect_err("one arm's budget cannot cover two arms");
    assert!(
        refused.contains("cannot cover") && refused.contains("60000"),
        "{refused}"
    );
    assert_eq!(
        experiment_envelope(30_000, 2, Some(60_000)).unwrap(),
        60_000
    );
    assert_eq!(
        experiment_envelope(30_000, 2, Some(90_000)).unwrap(),
        90_000
    );
    assert!(experiment_envelope(0, 1, None).is_err());
    assert!(experiment_envelope(1, 0, None).is_err());
    assert!(experiment_envelope(u64::MAX, 2, None).is_err());
}
