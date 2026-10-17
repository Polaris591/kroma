//! Gap-1 (`v1.5.2`) contract for the real (provider-backed) benchmark path:
//!
//! * the provider executor drives the real `AgentRunner` and never fabricates the expected answer;
//! * a resolved task needs the model to produce the content *and* the held-out `verify_command` to
//!   pass — the harness evaluates both independently of anything the model claims;
//! * each arm runs in its own isolated workspace, and `verify_command` executes there;
//! * repetitions + randomized arm order are deterministic given a seed, and the report carries
//!   Wilson / paired-bootstrap intervals and honest small-sample caveats;
//! * self vs child token spend comes from the provider-reported usage, and a reported spend past
//!   the budget is flagged rather than hidden.

use kroma_core::bench::{
    default_strategy_pair, load_held_out_suite, load_suite, provider_origin_of,
    run_paired_benchmark, suite_fingerprint, BenchArmResult, BenchComparisonReport, BenchExecutor,
    BenchRunMetadata, BenchRunOptions, BenchStrategyConfig, BenchTask, ProviderBenchExecutor,
    ProviderBenchSettings, SyntheticBenchExecutor,
};
use kroma_core::budget::BudgetConfig;
use kroma_core::messages::{AssistantTurn, FinishReason, TokenUsage, ToolCall};
use kroma_core::provider::{
    FakeProvider, ModelProvider, ProviderCompletion, ProviderError, ProviderRequest,
};
use kroma_core::tool::policy::PolicyMode;
use kroma_core::types::ToolCallId;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;

fn temp_root(label: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "kroma-bench-contract-{}-{}",
        label,
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&root).unwrap();
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

fn write_turn(path: &str, content: &str, input: u64, output: u64) -> AssistantTurn {
    AssistantTurn {
        content: None,
        reasoning_blocks: Vec::new(),
        tool_calls: vec![ToolCall {
            call_id: ToolCallId::new("call-1"),
            name: "write".to_string(),
            raw_arguments: serde_json::json!({ "path": path, "content": content }).to_string(),
        }],
        finish_reason: FinishReason::ToolCalls,
        usage: usage(input, output),
    }
}

fn provider_executor(provider: FakeProvider, origin: Option<&str>) -> ProviderBenchExecutor {
    let settings = ProviderBenchSettings::new("fake-model", "chat-completions")
        .with_provider_origin(origin.map(|o| o.to_string()))
        .with_policy_mode(PolicyMode::AllowWrite)
        .with_max_model_turns(6)
        .with_max_run_duration(Duration::from_secs(30));
    ProviderBenchExecutor::new(Arc::new(provider), settings)
}

/// A `single_agent` arm must genuinely be one agent: `spawn_subagent` is absent from the schema
/// the model sees, and the capability envelope is recorded per result. A control arm keeps the
/// configured quota and does expose the tool.
#[tokio::test]
async fn single_agent_arm_hides_spawn_subagent_and_records_its_envelope() {
    let root = temp_root("capabilities");
    let task = BenchTask {
        id: "envelope".to_string(),
        prompt: "answer".to_string(),
        files: std::collections::BTreeMap::new(),
        expect_contains: Vec::new(),
        expect_absent: Vec::new(),
        regression_guard: false,
        verify_command: None,
    };
    let provider = FakeProvider::new(vec![stop_turn(10, 2), stop_turn(10, 2)]);
    let recorded = provider.clone();
    let settings = ProviderBenchSettings::new("fake-model", "chat-completions")
        .with_policy_mode(PolicyMode::AllowWrite)
        .with_max_subagents(8)
        .with_max_model_turns(1)
        .with_max_run_duration(Duration::from_secs(30));
    let executor = ProviderBenchExecutor::new(Arc::new(provider), settings);
    let arms = vec![
        BenchStrategyConfig::single_agent(),
        BenchStrategyConfig {
            name: "subagents_enabled".to_string(),
            projection_profile: None,
            adaptive_tournament_candidates: 0,
            max_subagents: None,
            counterfactual_probe: false,
            causal_pruning: false,
            require_verified: true,
            on_demand_tools: false,
        },
    ];

    let report = run_paired_benchmark(
        "capabilities",
        std::slice::from_ref(&task),
        &arms,
        50_000,
        &root.join("scratch"),
        &executor,
        &BenchRunOptions::new(1, 7, false),
    )
    .await;

    let single = report
        .results
        .iter()
        .find(|r| r.strategy == "single_agent")
        .unwrap();
    assert_eq!(single.max_subagents, 0);
    assert_eq!(single.children_tokens, 0);
    assert!(!single.bash_allowed);
    let multi = report
        .results
        .iter()
        .find(|r| r.strategy == "subagents_enabled")
        .unwrap();
    assert_eq!(multi.max_subagents, 8);
    assert!(!multi.bash_allowed);

    // The capability is real, not just recorded: it is absent from the schema sent to the model.
    let requests = recorded.recorded_requests();
    assert_eq!(
        requests.len(),
        2,
        "one request per arm: {:?}",
        requests.len()
    );
    let exposes_spawn = |request: &kroma_core::provider::ProviderRequest| {
        request.tools.iter().any(|t| t.name == "spawn_subagent")
    };
    assert!(
        !exposes_spawn(&requests[0]),
        "the single_agent arm must not offer spawn_subagent"
    );
    assert!(
        exposes_spawn(&requests[1]),
        "the control arm must offer spawn_subagent"
    );
    for request in &requests {
        assert!(
            request.tools.iter().any(|t| t.name == "write"),
            "both arms keep the file-write capability the task needs"
        );
    }
    let _ = std::fs::remove_dir_all(&root);
}

/// A provider that answers every request with a single bare `Stop`: the harness must report the
/// task as unresolved without inventing the expected answer.
#[tokio::test]
async fn provider_executor_never_fabricates_the_expected_answer() {
    let root = temp_root("no-fabrication");
    let task = BenchTask {
        id: "off-by-one".to_string(),
        prompt: "fix the off-by-one bug".to_string(),
        files: [(
            "src/calc.py".to_string(),
            "def total(n):\n    return sum(range(n))\n".to_string(),
        )]
        .into_iter()
        .collect(),
        expect_contains: vec![("src/calc.py".to_string(), "range(1, n + 1)".to_string())],
        expect_absent: Vec::new(),
        regression_guard: false,
        verify_command: None,
    };
    let executor = provider_executor(FakeProvider::new(vec![stop_turn(40, 10)]), None);

    let report = run_paired_benchmark(
        "no-fabrication",
        std::slice::from_ref(&task),
        &[BenchStrategyConfig::single_agent()],
        50_000,
        &root.join("scratch"),
        &executor,
        &BenchRunOptions::new(1, 7, false),
    )
    .await;

    let result = &report.results[0];
    assert!(!result.resolved, "no answer was produced: {:?}", result);
    assert_eq!(result.gate_status, "rejected");
    assert!(
        result.tool_trace.is_empty(),
        "a model that made no tool call must have an empty tool trace: {:?}",
        result.tool_trace
    );
    assert!(
        result
            .failure_trace
            .iter()
            .any(|f| f.contains("src/calc.py")),
        "the failed post-condition must be in the trace: {:?}",
        result.failure_trace
    );
    // The executor wrote only the task's initial files; the needle was never inserted.
    let written = std::fs::read_to_string(
        root.join("scratch")
            .join("single_agent")
            .join("off-by-one__r0")
            .join("src/calc.py"),
    )
    .unwrap();
    assert!(
        !written.contains("range(1, n + 1)"),
        "the executor must not write the expected answer: {}",
        written
    );
    assert_eq!(report.executor, "provider");
    assert_eq!(report.metadata.model.as_deref(), Some("fake-model"));
    let _ = std::fs::remove_dir_all(&root);
}

/// The model writes the fix: the harness resolves the task and reports the real self/child token
/// split plus the prompt and tool-schema hashes.
#[tokio::test]
async fn provider_executor_resolves_on_a_real_write_and_accounts_tokens() {
    let root = temp_root("resolves");
    let task = BenchTask {
        id: "off-by-one".to_string(),
        prompt: "fix the off-by-one bug".to_string(),
        files: [(
            "src/calc.py".to_string(),
            "def total(n):\n    return sum(range(n))\n".to_string(),
        )]
        .into_iter()
        .collect(),
        expect_contains: vec![("src/calc.py".to_string(), "range(1, n + 1)".to_string())],
        expect_absent: Vec::new(),
        regression_guard: false,
        verify_command: None,
    };
    let executor = provider_executor(
        FakeProvider::new(vec![
            write_turn(
                "src/calc.py",
                "def total(n):\n    return sum(range(1, n + 1))\n",
                120,
                30,
            ),
            stop_turn(5, 2),
        ]),
        Some("https://api.example.com"),
    );

    let report = run_paired_benchmark(
        "resolves",
        std::slice::from_ref(&task),
        &[BenchStrategyConfig::single_agent()],
        50_000,
        &root.join("scratch"),
        &executor,
        &BenchRunOptions::new(1, 7, false),
    )
    .await;

    let result = &report.results[0];
    assert!(result.resolved, "the written answer resolves: {:?}", result);
    assert_eq!(result.gate_status, "approved");
    assert_eq!(result.self_tokens, 157, "120+30+5+2 reported input/output");
    assert_eq!(result.children_tokens, 0);
    assert_eq!(result.total_tokens, 157);
    assert!(!result.budget_exceeded);
    assert_eq!(result.max_subagents, 0, "single_agent hides spawn_subagent");
    assert!(!result.bash_allowed, "AllowWrite has no shell");
    // Trajectory diagnostics: the report must show what the model actually did.
    assert_eq!(result.turns_used, 2, "write turn + stop turn");
    assert_eq!(result.tool_calls_used, 1);
    assert_eq!(result.modified_files, vec!["src/calc.py".to_string()]);
    assert_eq!(result.final_text.as_deref(), Some("done"));
    assert!(
        result
            .tool_trace
            .iter()
            .any(|entry| entry.starts_with("write src/calc.py -> ok")),
        "the tool trace must show the write: {:?}",
        result.tool_trace
    );
    assert!(result.prompt_hash.is_some());
    assert!(result.tool_schema_hash.is_some());
    assert_eq!(result.model.as_deref(), Some("fake-model"));
    assert_eq!(
        report.metadata.provider_origin.as_deref(),
        Some("https://api.example.com")
    );
    assert!(report.metadata.prompt_hash.is_some());
    assert!(
        report.caveats().iter().any(|c| c.contains("small sample")),
        "a one-task provider report must carry the small-sample caveat: {:?}",
        report.caveats()
    );

    // Isolation: the run's state/diagnostics live *outside* the agent's capability root, so `find`
    // cannot discover the bench's own event log inside the task workspace.
    let arm_dir = root
        .join("scratch")
        .join("single_agent")
        .join("off-by-one__r0");
    assert!(
        !arm_dir.join(".kroma-bench-state").exists(),
        "bench state must not be inside the agent workspace"
    );
    assert!(
        root.join("scratch")
            .join("single_agent")
            .join("off-by-one__r0__state")
            .exists(),
        "the run state directory must exist beside the workspace"
    );
    let mut entries: Vec<String> = std::fs::read_dir(&arm_dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().to_string())
        .collect();
    entries.sort();
    assert_eq!(
        entries,
        vec!["src".to_string()],
        "the agent workspace must contain only the task's own files"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// A write denied by policy is reported as unresolved instead of silently "succeeding".
#[tokio::test]
async fn provider_executor_reports_policy_denied_writes_as_unresolved() {
    let root = temp_root("policy-denied");
    let task = BenchTask {
        id: "needs-write".to_string(),
        prompt: "write the answer".to_string(),
        files: std::collections::BTreeMap::new(),
        expect_contains: vec![("answer.txt".to_string(), "42".to_string())],
        expect_absent: Vec::new(),
        regression_guard: false,
        verify_command: None,
    };
    let settings = ProviderBenchSettings::new("fake-model", "chat-completions")
        .with_policy_mode(PolicyMode::Ask)
        .with_max_model_turns(4)
        .with_max_run_duration(Duration::from_secs(30));
    let executor = ProviderBenchExecutor::new(
        Arc::new(FakeProvider::new(vec![
            write_turn("answer.txt", "42", 10, 5),
            stop_turn(3, 1),
        ])),
        settings,
    );

    let report = run_paired_benchmark(
        "policy-denied",
        std::slice::from_ref(&task),
        &[BenchStrategyConfig::single_agent()],
        50_000,
        &root.join("scratch"),
        &executor,
        &BenchRunOptions::new(1, 7, false),
    )
    .await;

    assert!(!report.results[0].resolved);
    assert!(
        !root
            .join("scratch")
            .join("single_agent")
            .join("needs-write__r0")
            .join("answer.txt")
            .exists(),
        "a denied write must not create the file"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// Cap compliance is an axis of its own: a functionally correct run that overshoots the declared
/// cap is *not* a budget-locked success, and the report must say so separately from `resolved`.
#[tokio::test]
async fn provider_executor_flags_reported_spend_past_the_budget() {
    let root = temp_root("over-budget");
    let task = BenchTask {
        id: "expensive".to_string(),
        prompt: "answer".to_string(),
        files: std::collections::BTreeMap::new(),
        expect_contains: Vec::new(),
        expect_absent: Vec::new(),
        regression_guard: false,
        verify_command: None,
    };
    let executor = provider_executor(FakeProvider::new(vec![stop_turn(20_000, 5_000)]), None);

    let report = run_paired_benchmark(
        "over-budget",
        std::slice::from_ref(&task),
        &[BenchStrategyConfig::single_agent()],
        5_000,
        &root.join("scratch"),
        &executor,
        &BenchRunOptions::new(1, 7, false),
    )
    .await;

    let result = &report.results[0];
    assert!(!result.cap_ok, "{:?}", result);
    assert!(result.budget_exceeded);
    assert_eq!(result.budget_tokens, 5_000);
    assert_eq!(result.budget_overage, result.total_tokens - 5_000);
    assert!(result.total_tokens > 5_000);
    assert!(
        result
            .failure_trace
            .iter()
            .any(|f| f.contains("cap_violation")),
        "the overage must be recorded as a cap violation: {:?}",
        result.failure_trace
    );

    let compliance = report.cap_compliance();
    assert_eq!(compliance.runs, 1);
    assert_eq!(compliance.within_cap, 0);
    assert_eq!(compliance.over_cap, 1);
    assert_eq!(compliance.overage_tokens, result.budget_overage);
    assert!(
        report
            .caveats()
            .iter()
            .any(|c| c.contains("cap compliance")),
        "the report must carry the cap-compliance caveat: {:?}",
        report.caveats()
    );
    assert!(report.render().contains("cap compliance:"));
    let _ = std::fs::remove_dir_all(&root);
}

/// The three dimensions are independent: a run can be functionally correct (`resolved`), yet neither
/// cap-compliant nor a normal completion — and the summary must count it that way.
#[tokio::test]
async fn resolved_run_cut_off_by_the_cap_is_not_counted_as_compliant() {
    let root = temp_root("resolved-but-over");
    let task = BenchTask {
        id: "fix-me".to_string(),
        prompt: "write the answer".to_string(),
        files: std::collections::BTreeMap::new(),
        expect_contains: vec![("answer.txt".to_string(), "42".to_string())],
        expect_absent: Vec::new(),
        regression_guard: false,
        verify_command: None,
    };
    // The model answers correctly, then the reported usage blows past the cap.
    let executor = provider_executor(
        FakeProvider::new(vec![write_turn("answer.txt", "42", 30_000, 9_000)]),
        None,
    );

    let report = run_paired_benchmark(
        "resolved-but-over",
        std::slice::from_ref(&task),
        &[BenchStrategyConfig::single_agent()],
        5_000,
        &root.join("scratch"),
        &executor,
        &BenchRunOptions::new(1, 7, false),
    )
    .await;

    let result = &report.results[0];
    assert!(
        result.resolved,
        "the file was written correctly: {:?}",
        result
    );
    assert!(!result.cap_ok);
    let arm = &report.arms[0];
    assert_eq!(arm.resolved, 1);
    assert_eq!(arm.cap_ok, 0);
    assert!(arm.budget_overage_tokens > 0);
    // The model answered on its first turn, so the run still ended on its own (budget check after).
    assert!(report.cap_compliance().over_cap == 1);
    let _ = std::fs::remove_dir_all(&root);
}

/// `verify_command` runs *inside the isolated arm workspace* and its exit status is part of the
/// verdict.
#[cfg(unix)]
#[tokio::test]
async fn verify_command_runs_in_the_isolated_workspace_and_gates_resolution() {
    let root = temp_root("verify");
    let records_cwd = BenchTask {
        id: "records-cwd".to_string(),
        prompt: "just report where you are".to_string(),
        files: [("marker.txt".to_string(), "x".to_string())]
            .into_iter()
            .collect(),
        expect_contains: Vec::new(),
        expect_absent: Vec::new(),
        regression_guard: false,
        verify_command: Some(vec![
            "sh".to_string(),
            "-c".to_string(),
            "pwd > verify_cwd.txt".to_string(),
        ]),
    };
    let fails = BenchTask {
        id: "fails".to_string(),
        prompt: "nothing to do".to_string(),
        files: std::collections::BTreeMap::new(),
        expect_contains: Vec::new(),
        expect_absent: Vec::new(),
        regression_guard: false,
        verify_command: Some(vec![
            "sh".to_string(),
            "-c".to_string(),
            "echo 'held-out test failed' >&2; exit 1".to_string(),
        ]),
    };
    let executor = provider_executor(FakeProvider::new(vec![stop_turn(10, 2)]), None);

    let report = run_paired_benchmark(
        "verify",
        &[records_cwd, fails],
        &[BenchStrategyConfig::single_agent()],
        50_000,
        &root.join("scratch"),
        &executor,
        &BenchRunOptions::new(1, 7, false),
    )
    .await;

    let arm_root = root.join("scratch").join("single_agent");
    let cwd_file = arm_root.join("records-cwd__r0").join("verify_cwd.txt");
    assert!(
        cwd_file.exists(),
        "verify_command must run in the arm workspace"
    );
    let recorded = std::fs::read_to_string(&cwd_file).unwrap();
    assert_eq!(
        std::fs::canonicalize(recorded.trim()).unwrap(),
        std::fs::canonicalize(arm_root.join("records-cwd__r0")).unwrap(),
        "verify_command cwd must be the isolated arm workspace"
    );
    assert!(
        !root.join("verify_cwd.txt").exists(),
        "verify_command must not run in the bench parent directory"
    );

    let records = report
        .results
        .iter()
        .find(|r| r.task_id == "records-cwd")
        .unwrap();
    assert!(records.resolved, "{:?}", records);
    let failed = report
        .results
        .iter()
        .find(|r| r.task_id == "fails")
        .unwrap();
    assert!(!failed.resolved, "{:?}", failed);
    assert!(
        failed
            .failure_trace
            .iter()
            .any(|f| f.contains("verify_command") && f.contains("exit=1")),
        "the failing command must be reported: {:?}",
        failed.failure_trace
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// A stub executor that resolves exactly the arm named `pass` and records nothing — the *order* it
/// was invoked in is recoverable from the report because results are appended in execution order.
struct StubExecutor {
    pass_arm: &'static str,
}

impl BenchExecutor for StubExecutor {
    fn label(&self) -> &'static str {
        "stub"
    }

    fn execute<'a>(
        &'a self,
        task: &'a BenchTask,
        strategy: &'a BenchStrategyConfig,
        _budget_tokens: u64,
        _workspace_root: &'a Path,
    ) -> Pin<Box<dyn Future<Output = BenchArmResult> + Send + 'a>> {
        let resolved = strategy.name == self.pass_arm;
        Box::pin(async move {
            BenchArmResult {
                task_id: task.id.clone(),
                strategy: strategy.name.clone(),
                resolved,
                gate_status: if resolved { "approved" } else { "rejected" }.to_string(),
                self_input_tokens: 10,
                self_cached_input_tokens: 0,
                self_output_tokens: 0,
                children_input_tokens: 0,
                children_cached_input_tokens: 0,
                children_output_tokens: 0,
                reported_model: None,
                usage_basis: "unknown".to_string(),
                estimated_cost_nano_usd: None,
                cost_unknown_reason: None,
                self_tokens: 10,
                children_tokens: 0,
                total_tokens: 10,
                budget_exceeded: false,
                budget_tokens: 1_000,
                budget_overage: 0,
                cap_ok: true,
                finished_normally: true,
                wall_time_ms: 1,
                verify_time_ms: 0,
                regressions: usize::from(task.regression_guard && !resolved),
                repetition: 0,
                model: None,
                prompt_hash: None,
                tool_schema_hash: None,
                terminal_reason: None,
                failure_trace: Vec::new(),
                max_subagents: strategy.max_subagents.unwrap_or(0),
                bash_allowed: false,
                counterfactual_probe: strategy.counterfactual_probe,
                causal_pruning: strategy.causal_pruning,
                require_verified: strategy.require_verified,
                on_demand_tools: strategy.on_demand_tools,
                turns_used: 0,
                tool_calls_used: 0,
                modified_files: Vec::new(),
                final_text: None,
                tool_trace: Vec::new(),
                mechanism: None,
            }
        })
    }
}

fn stub_tasks() -> Vec<BenchTask> {
    (0..3)
        .map(|i| BenchTask {
            id: format!("task-{}", i),
            prompt: "p".to_string(),
            files: std::collections::BTreeMap::new(),
            expect_contains: Vec::new(),
            expect_absent: Vec::new(),
            regression_guard: false,
            verify_command: None,
        })
        .collect()
}

fn stub_arms() -> Vec<BenchStrategyConfig> {
    ["pass", "fail", "third", "fourth"]
        .iter()
        .map(|name| BenchStrategyConfig {
            name: name.to_string(),
            projection_profile: None,
            adaptive_tournament_candidates: 0,
            max_subagents: None,
            counterfactual_probe: false,
            causal_pruning: false,
            require_verified: true,
            on_demand_tools: false,
        })
        .collect()
}

/// Repetitions multiply the paired observations; the seeded order is reproducible; and the arm
/// order is actually shuffled instead of always following declaration order.
#[tokio::test]
async fn paired_benchmark_repeats_and_randomizes_order_deterministically() {
    let root = temp_root("order");
    let executor = StubExecutor { pass_arm: "pass" };
    let arms = stub_arms();
    let tasks = stub_tasks();
    let options = BenchRunOptions::new(6, 12345, true);

    let first = run_paired_benchmark(
        "order",
        &tasks,
        &arms,
        1_000,
        &root.join("a"),
        &executor,
        &options,
    )
    .await;
    let second = run_paired_benchmark(
        "order",
        &tasks,
        &arms,
        1_000,
        &root.join("b"),
        &executor,
        &options,
    )
    .await;

    assert_eq!(first.results.len(), tasks.len() * arms.len() * 6);
    assert_eq!(first.metadata.repetitions, 6);
    assert_eq!(first.metadata.seed, 12345);
    assert!(first.metadata.randomized_order);

    let sequence = |report: &BenchComparisonReport| {
        report
            .results
            .iter()
            .map(|r| (r.task_id.clone(), r.repetition, r.strategy.clone()))
            .collect::<Vec<_>>()
    };
    assert_eq!(sequence(&first), sequence(&second), "same seed, same order");

    // Every (task, repetition) ran every arm exactly once...
    let mut orders: std::collections::BTreeSet<Vec<String>> = std::collections::BTreeSet::new();
    for repetition in 0..6 {
        for task in &tasks {
            let order: Vec<String> = first
                .results
                .iter()
                .filter(|r| r.repetition == repetition && r.task_id == task.id)
                .map(|r| r.strategy.clone())
                .collect();
            assert_eq!(
                order.len(),
                arms.len(),
                "each arm runs once per (task, rep)"
            );
            orders.insert(order);
        }
    }
    // ...and the order is not simply the declaration order every time.
    assert!(
        orders.len() > 1,
        "randomized order produced a single ordering: {:?}",
        orders
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// Wilson intervals per arm and a paired bootstrap interval against the baseline. A perfectly
/// separated pair must have an interval that excludes zero; identical arms must include it.
#[tokio::test]
async fn paired_benchmark_reports_intervals_that_respect_separation() {
    let root = temp_root("intervals");
    let executor = StubExecutor { pass_arm: "pass" };
    let arms = stub_arms();
    let tasks = stub_tasks();
    let report = run_paired_benchmark(
        "intervals",
        &tasks,
        &arms,
        // baseline is the first declared arm = "pass"
        1_000,
        &root.join("scratch"),
        &executor,
        &BenchRunOptions::new(3, 99, false),
    )
    .await;

    let pass = report.arms.iter().find(|a| a.strategy == "pass").unwrap();
    assert_eq!(pass.pass_rate, 1.0);
    assert!(pass.pass_rate_ci_low < 1.0 && pass.pass_rate_ci_low > 0.0);
    assert_eq!(pass.pass_rate_ci_high, 1.0);

    let fail = report.arms.iter().find(|a| a.strategy == "fail").unwrap();
    assert_eq!(fail.pass_rate, 0.0);
    assert_eq!(fail.pass_rate_ci_low, 0.0);
    assert!(fail.pass_rate_ci_high > 0.0);

    // baseline "pass" vs "fail": every paired observation differs by -1.
    let delta = report
        .paired_deltas
        .iter()
        .find(|d| d.arm == "fail")
        .unwrap();
    assert_eq!(delta.baseline, "pass");
    assert_eq!(delta.delta_pass_rate, -1.0);
    assert_eq!(delta.paired_observations, tasks.len() * 3);
    assert!(delta.ci_low <= delta.ci_high);

    // Two arms with identical outcomes have no separation: the interval contains zero.
    let identical = run_paired_benchmark(
        "intervals",
        &tasks,
        &arms,
        1_000,
        &root.join("scratch2"),
        &StubExecutor { pass_arm: "third" },
        &BenchRunOptions::new(3, 99, false),
    )
    .await;
    let fourth = identical
        .paired_deltas
        .iter()
        .find(|d| d.arm == "fourth")
        .unwrap();
    assert_eq!(fourth.delta_pass_rate, 0.0);
    assert_eq!(fourth.ci_low, 0.0);
    assert_eq!(fourth.ci_high, 0.0);
    let _ = std::fs::remove_dir_all(&root);
}

/// The synthetic executor stays honestly labelled: no model metadata, an explicit caveat, and it
/// still fabricates a `fixable-` task (which is exactly why it cannot be used as capability proof).
#[tokio::test]
async fn synthetic_executor_is_labelled_and_carries_no_model_metadata() {
    let root = temp_root("synthetic");
    let report = run_paired_benchmark(
        "builtin-smoke",
        &kroma_core::bench::builtin_smoke_suite(),
        &kroma_core::bench::default_strategy_pair(),
        8_000,
        &root.join("scratch"),
        &SyntheticBenchExecutor,
        &BenchRunOptions::default(),
    )
    .await;

    assert_eq!(report.executor, "synthetic");
    assert!(report.metadata.model.is_none());
    assert!(report
        .caveats()
        .iter()
        .any(|c| c.contains("synthetic executor")));
    assert!(report.render().contains("caveat:"));
    assert!(report.results.iter().all(|r| r.model.is_none()));
    let _ = std::fs::remove_dir_all(&root);
}

fn held_out_fixture_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/bench/held_out_smoke.json")
}

#[cfg(unix)]
fn python3_available() -> bool {
    std::process::Command::new("python3")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// Materialize a task plus optional edits and evaluate it in a fresh directory.
#[cfg(unix)]
async fn evaluate_with(label: &str, task: &BenchTask, edits: &[(&str, &str)]) -> bool {
    let dir = temp_root(&format!("behaviour-{}", label));
    for (path, contents) in &task.files {
        let full = dir.join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(&full, contents).unwrap();
    }
    for (path, contents) in edits {
        let full = dir.join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(&full, contents).unwrap();
    }
    let verdict = task.evaluate(&dir).await;
    let _ = std::fs::remove_dir_all(&dir);
    verdict.resolved
}

#[cfg(unix)]
fn fixture_task<'a>(tasks: &'a [BenchTask], id: &str) -> &'a BenchTask {
    tasks
        .iter()
        .find(|t| t.id == id)
        .unwrap_or_else(|| panic!("fixture task '{}' is missing", id))
}

/// The held-out checks are behavioural: the buggy file fails, an *equivalent* fix passes, and a
/// comment containing the old string needles does not fool them.
#[cfg(unix)]
#[tokio::test]
async fn held_out_verify_commands_test_behaviour_not_spelling() {
    if !python3_available() {
        eprintln!("skipping: python3 is not available");
        return;
    }
    let tasks = load_suite(&held_out_fixture_path()).expect("the held-out suite must load");
    assert_eq!(tasks.len(), 3);

    // 1. The initial (buggy) files must fail every task.
    for task in &tasks {
        assert!(
            !evaluate_with(&format!("{}-buggy", task.id), task, &[]).await,
            "task '{}' must fail on its initial buggy file",
            task.id
        );
    }

    // 2. A comment repeating the old string needles must not satisfy the off-by-one task.
    let off_by_one = fixture_task(&tasks, "heldout-off-by-one");
    let commented = "def total(n):\n    # fixed: range(1, n + 1)\n    return sum(range(n))\n";
    assert!(
        !evaluate_with(
            "off-by-one-comment",
            off_by_one,
            &[("src/calc.py", commented)]
        )
        .await,
        "a comment must not satisfy a behavioural check"
    );

    // 3. Equivalent fixes pass even when they share no text with the original file.
    let equivalents: [(&str, &str, &str); 4] = [
        (
            "off-by-one",
            "src/calc.py",
            "def total(n):\n    total = 0\n    for value in range(n):\n        total += value + 1\n    return total\n",
        ),
        (
            "guard-map",
            "src/guard.py",
            "def ratio(a, b):\n    if not b:\n        raise ValueError('division by zero')\n    return a / b\n",
        ),
        (
            "guard-try",
            "src/guard.py",
            "def ratio(a, b):\n    try:\n        return a / b\n    except ZeroDivisionError:\n        raise ValueError('division by zero')\n",
        ),
        (
            "normalize",
            "src/text.py",
            "def normalize(s):\n    return ' '.join(s.split())\n",
        ),
    ];
    for (label, path, contents) in equivalents {
        let task_id = if label.starts_with("guard") {
            "heldout-guard-division"
        } else if label.starts_with("normalize") {
            "heldout-normalize-whitespace"
        } else {
            "heldout-off-by-one"
        };
        let task = fixture_task(&tasks, task_id);
        assert!(
            evaluate_with(label, task, &[(path, contents)]).await,
            "equivalent fix '{}' must pass {}",
            label,
            task_id
        );
    }

    // 4. A guard "fix" that still lets ZeroDivisionError escape is rejected, comment or not.
    let guard = fixture_task(&tasks, "heldout-guard-division");
    let still_broken = "def ratio(a, b):\n    # if b == 0: raise ValueError\n    return a / b\n";
    assert!(
        !evaluate_with("guard-broken", guard, &[("src/guard.py", still_broken)]).await,
        "a comment must not satisfy the guard task"
    );
}

/// The committed held-out suite loads, validates its paths/verify_command, and is small by design.
#[test]
fn held_out_suite_fixture_loads_and_validates() {
    let path = held_out_fixture_path();
    let tasks = load_suite(&path).expect("the held-out suite must load");
    assert_eq!(tasks.len(), 3);
    let ids: std::collections::BTreeSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
    assert_eq!(ids.len(), tasks.len(), "task ids must be unique");
    assert!(
        tasks.iter().all(|t| t.verify_command.is_some()),
        "every held-out task declares an independent verify_command"
    );

    // The path validator applies to a real suite exactly as it does to a hostile one.
    let hostile = path.parent().unwrap().join("hostile_escape.json");
    std::fs::write(
        &hostile,
        r#"[{"id":"escape","prompt":"p","files":{"../../escape.txt":"x"}}]"#,
    )
    .unwrap();
    assert!(load_suite(&hostile).is_err());
    let _ = std::fs::remove_file(&hostile);
}

/// The provider origin helper is what keeps credentials out of a report.
#[test]
fn provider_origin_helper_is_credential_free() {
    assert_eq!(
        provider_origin_of("https://user:secret@api.example.com/v1/chat?token=abc"),
        Some("https://api.example.com".to_string())
    );
    assert_eq!(provider_origin_of("garbage"), None);
}

/// B16: a report must be traceable to the source and binary that actually ran. A commit id alone
/// cannot describe a dirty tree, so the fingerprint carries its own source hash — and it has to
/// reach the rendered report instead of sitting in a struct nobody prints.
#[tokio::test]
async fn provider_report_carries_source_and_binary_provenance() {
    let root = temp_root("provenance");
    let task = BenchTask {
        id: "prov".to_string(),
        prompt: "answer".to_string(),
        files: std::collections::BTreeMap::new(),
        expect_contains: Vec::new(),
        expect_absent: Vec::new(),
        regression_guard: false,
        verify_command: None,
    };
    // Point provenance at a repository this test owns, so the assertions do not depend on where the
    // test binary happens to run from (and never walk an unrelated tree).
    let source_root = root.join("checkout");
    std::fs::create_dir_all(&source_root).unwrap();
    let git = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(&source_root)
            .args(args)
            .stdin(std::process::Stdio::null())
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .output()
            .expect("git is required for the provenance contract");
        assert!(output.status.success(), "git {args:?} failed");
    };
    git(&["init", "-q"]);
    std::fs::write(source_root.join("a.rs"), "fn a() {}\n").unwrap();
    git(&["add", "."]);
    git(&["commit", "-qm", "init"]);
    // An untracked file makes the tree dirty without moving the commit — the exact case a commit id
    // alone cannot describe.
    std::fs::write(source_root.join("untracked.rs"), "fn b() {}\n").unwrap();
    let settings = ProviderBenchSettings::new("fake-model", "chat-completions")
        .with_policy_mode(PolicyMode::AllowWrite)
        .with_max_model_turns(6)
        .with_max_run_duration(Duration::from_secs(30))
        .with_source_root(&source_root);
    let executor = ProviderBenchExecutor::new(
        Arc::new(FakeProvider::new(vec![stop_turn(10, 2)])),
        settings,
    );

    let report = run_paired_benchmark(
        "provenance",
        std::slice::from_ref(&task),
        &[BenchStrategyConfig::single_agent()],
        100_000,
        &root.join("scratch"),
        &executor,
        &BenchRunOptions::new(1, 11, false),
    )
    .await;

    let fingerprint = report
        .metadata
        .source_fingerprint
        .as_ref()
        .expect("the provider executor must attach a source fingerprint");
    assert!(
        fingerprint.source_sha256.is_some(),
        "the source hash is what identifies a dirty tree: {fingerprint:?}"
    );
    assert_eq!(
        fingerprint.dirty,
        Some(true),
        "an untracked file must be reported as a dirty tree: {fingerprint:?}"
    );
    assert!(fingerprint.commit.is_some(), "the commit is still recorded");
    assert!(
        fingerprint.summary().contains("tree="),
        "{}",
        fingerprint.summary()
    );
    // The standalone commit field must never disagree with the fingerprint.
    assert_eq!(fingerprint.commit, report.metadata.commit);

    let rendered = report.render();
    assert!(
        rendered.contains("source:") && rendered.contains("tree="),
        "the rendered report must show the provenance line: {rendered}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// A provider that reports a model identity and a fixed usage, so the priced path can be asserted
/// exactly instead of guessed.
struct ReportingProvider {
    model: Option<String>,
    usage: TokenUsage,
}

impl ModelProvider for ReportingProvider {
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
                    reasoning_blocks: Vec::new(),
                    tool_calls: Vec::new(),
                    finish_reason: FinishReason::Stop,
                    usage: Some(self.usage),
                },
                request_body_bytes: 8,
                latency_ms: 1,
                ttft_ms: None,
                attempts_used: 1,
                reported_model: self.model.clone(),
            })
        })
    }
}

fn priced_task() -> BenchTask {
    BenchTask {
        id: "priced".to_string(),
        prompt: "answer".to_string(),
        files: std::collections::BTreeMap::new(),
        expect_contains: Vec::new(),
        expect_absent: Vec::new(),
        regression_guard: false,
        verify_command: None,
    }
}

fn fixture_price_table() -> kroma_core::usage_cost::PriceTable {
    kroma_core::usage_cost::PriceTable {
        version: "fixture-2026-10-16".to_string(),
        source: "operator fixture, not market prices".to_string(),
        // The request alias is deliberately priced too (with different rates): if the harness ever
        // priced the alias instead of the returned identity, the estimate below would change rather
        // than stay unknown, and the test would catch it.
        models: std::collections::BTreeMap::from([
            (
                "priced-model".to_string(),
                kroma_core::usage_cost::TokenRates {
                    uncached_input_nano_usd: 1_000,
                    cached_input_nano_usd: 100,
                    output_nano_usd: 3_000,
                },
            ),
            (
                "request-alias".to_string(),
                kroma_core::usage_cost::TokenRates {
                    uncached_input_nano_usd: 7_000,
                    cached_input_nano_usd: 7_000,
                    output_nano_usd: 7_000,
                },
            ),
        ]),
    }
}

/// B16: cost is computed from the *returned* identity, the cached/uncached split and reported
/// usage — and the report states it is an estimate rather than a bill.
#[tokio::test]
async fn arm_cost_uses_the_reported_identity_and_the_cached_split() {
    let root = temp_root("priced");
    let settings = ProviderBenchSettings::new("request-alias", "chat-completions")
        .with_policy_mode(PolicyMode::AllowWrite)
        .with_max_model_turns(1)
        .with_max_run_duration(Duration::from_secs(30))
        .with_price_table(Some(fixture_price_table()));
    let provider = ReportingProvider {
        model: Some("priced-model".to_string()),
        usage: TokenUsage {
            input_tokens: 1_000,
            cached_input_tokens: Some(400),
            cache_creation_input_tokens: None,
            output_tokens: 200,
            reasoning_tokens: None,
        },
    };
    let executor = ProviderBenchExecutor::new(Arc::new(provider), settings);
    let task = priced_task();

    let report = run_paired_benchmark(
        "priced",
        std::slice::from_ref(&task),
        &[BenchStrategyConfig::single_agent()],
        100_000,
        &root.join("scratch"),
        &executor,
        &BenchRunOptions::new(1, 3, false),
    )
    .await;

    let result = &report.results[0];
    assert_eq!(result.reported_model.as_deref(), Some("priced-model"));
    // 600 uncached * 1000 + 400 cached * 100 + 200 output * 3000.
    assert_eq!(
        result.estimated_cost_nano_usd,
        Some(600 * 1_000 + 400 * 100 + 200 * 3_000),
        "the cached rate must be used for cached tokens: {result:?}"
    );
    assert!(result.cost_unknown_reason.is_none(), "{result:?}");
    assert_eq!(
        report.metadata.price_table_version.as_deref(),
        Some("fixture-2026-10-16")
    );

    let rendered = report.render();
    assert!(rendered.contains("pricing:"), "{rendered}");
    assert!(rendered.contains("not an invoice"), "{rendered}");
    assert!(rendered.contains("cost:"), "{rendered}");
    let _ = std::fs::remove_dir_all(&root);
}

/// A price table without a returned identity cannot price anything: the estimate stays unknown and
/// says why, rather than being attributed to the requested alias.
#[tokio::test]
async fn arm_cost_stays_unknown_without_a_reported_identity() {
    let root = temp_root("unpriced");
    let settings = ProviderBenchSettings::new("request-alias", "chat-completions")
        .with_policy_mode(PolicyMode::AllowWrite)
        .with_max_model_turns(1)
        .with_max_run_duration(Duration::from_secs(30))
        .with_price_table(Some(fixture_price_table()));
    let provider = ReportingProvider {
        model: None, // the provider never states what actually answered
        usage: TokenUsage {
            input_tokens: 10,
            cached_input_tokens: None,
            cache_creation_input_tokens: None,
            output_tokens: 5,
            reasoning_tokens: None,
        },
    };
    let executor = ProviderBenchExecutor::new(Arc::new(provider), settings);
    let task = priced_task();

    let report = run_paired_benchmark(
        "unpriced",
        std::slice::from_ref(&task),
        &[BenchStrategyConfig::single_agent()],
        100_000,
        &root.join("scratch"),
        &executor,
        &BenchRunOptions::new(1, 5, false),
    )
    .await;

    let result = &report.results[0];
    assert!(
        result.estimated_cost_nano_usd.is_none(),
        "an unknown identity must not be priced via the request alias: {result:?}"
    );
    let reason = result.cost_unknown_reason.as_deref().unwrap_or_default();
    assert!(
        reason.contains("identity") || reason.contains("model is unknown"),
        "the refusal must be about the missing identity, not about a missing rate - otherwise \
         pricing the request alias would still look 'unknown by accident'. got: {reason:?}"
    );
    assert!(
        !reason.contains("no exact rate"),
        "the alias IS priced in this fixture, so an unknown here must not be a rate lookup miss: {reason:?}"
    );
    assert!(report.render().contains("unpriced"), "{}", report.render());
    let _ = std::fs::remove_dir_all(&root);
}

/// B17/B13: a held-out suite must not live where the graded agent can write it. Otherwise the agent
/// can rewrite the oracle and the harness grades its own replacement.
#[test]
fn a_held_out_suite_inside_the_workspace_is_refused() {
    let root = temp_root("held-out-inside");
    let workspace = root.join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let suite = workspace.join("suite.json");
    std::fs::write(
        &suite,
        r#"[{"id":"t","prompt":"p","expect_contains":[],
             "expect_absent":[],"verify_command":["true"]}]"#,
    )
    .unwrap();

    let err = load_held_out_suite(&suite, &workspace)
        .expect_err("a suite inside the graded workspace must be refused");
    assert!(
        err.contains("inside the graded workspace"),
        "the refusal must explain why: {err}"
    );

    // The same content is accepted once it lives outside the workspace.
    let outside = root.join("suite-outside.json");
    std::fs::copy(&suite, &outside).unwrap();
    let tasks = load_held_out_suite(&outside, &workspace).expect("an outside suite loads");
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].id, "t");
    let _ = std::fs::remove_dir_all(&root);
}

/// A symlink placed inside the workspace must not smuggle the suite in: the agent could repoint it,
/// so the path itself is what matters, not only where it currently resolves.
///
/// Unix-only: creating a symlink needs `std::os::unix` here. The Windows test binary must still
/// compile (`cargo check -p kroma-core --tests --target x86_64-pc-windows-gnu`); the Windows
/// equivalent is a reparse point, which the workspace path check refuses by path as well.
#[cfg(unix)]
#[test]
fn a_held_out_suite_symlinked_into_the_workspace_is_refused() {
    let root = temp_root("held-out-link");
    let workspace = root.join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let real = root.join("real-suite.json");
    std::fs::write(
        &real,
        r#"[{"id":"t","prompt":"p","expect_contains":[],
             "expect_absent":[],"verify_command":["true"]}]"#,
    )
    .unwrap();
    let link = workspace.join("suite.json");
    if std::os::unix::fs::symlink(&real, &link).is_err() {
        panic!("could not create the symlink this contract depends on");
    }
    let err = load_held_out_suite(&link, &workspace).expect_err(
        "a suite path inside the workspace must be refused even if its target is outside",
    );
    assert!(err.contains("inside the graded workspace"), "{err}");
    let _ = std::fs::remove_dir_all(&root);
}

/// B16/B17: a report must pin the *scoring procedure*, not just the model and the source. Editing an
/// oracle changes the fingerprint, so two runs whose hashes differ are not the same benchmark.
#[test]
fn suite_fingerprint_covers_the_tasks_and_their_oracles() {
    let base = BenchTask {
        id: "t".to_string(),
        prompt: "p".to_string(),
        files: std::collections::BTreeMap::new(),
        expect_contains: Vec::new(),
        expect_absent: Vec::new(),
        regression_guard: false,
        verify_command: Some(vec!["true".to_string()]),
    };
    let first = suite_fingerprint(std::slice::from_ref(&base));
    assert_eq!(first.len(), 16, "a short, stable identifier: {first}");
    assert_eq!(
        first,
        suite_fingerprint(std::slice::from_ref(&base)),
        "the same task set must hash the same"
    );

    // Changing the held-out verifier must change the identity of the benchmark.
    let mut changed = base.clone();
    changed.verify_command = Some(vec!["false".to_string()]);
    assert_ne!(first, suite_fingerprint(std::slice::from_ref(&changed)));

    // So must changing a post-condition, or adding a task.
    let mut conditioned = base.clone();
    conditioned.expect_contains = vec![("a".to_string(), "b".to_string())];
    assert_ne!(first, suite_fingerprint(std::slice::from_ref(&conditioned)));
    assert_ne!(
        first,
        suite_fingerprint(&[base.clone(), conditioned.clone()]),
        "adding a task changes the suite identity"
    );
}

/// The fingerprint must reach the report, or a reader cannot tell two differently-scored runs apart.
#[tokio::test]
async fn report_renders_the_suite_fingerprint() {
    let root = temp_root("suite-fingerprint");
    let task = priced_task();
    let executor = provider_executor(FakeProvider::new(vec![stop_turn(10, 2)]), None);
    let report = run_paired_benchmark(
        "pinned",
        std::slice::from_ref(&task),
        &[BenchStrategyConfig::single_agent()],
        100_000,
        &root.join("scratch"),
        &executor,
        &BenchRunOptions::new(1, 13, false),
    )
    .await;

    let expected = suite_fingerprint(std::slice::from_ref(&task));
    assert_eq!(
        report.metadata.suite_sha256.as_deref(),
        Some(expected.as_str())
    );
    assert!(
        report.render().contains(&format!("oracles {expected}")),
        "the rendered report must name the oracle fingerprint: {}",
        report.render()
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// B17: thresholds must be declared before the run, and the verdict (plus the threshold that
/// produced it) must appear in the report so it cannot be rewritten afterwards.
#[tokio::test]
async fn preregistered_thresholds_produce_an_auditable_verdict() {
    use kroma_core::bench::DecisionThresholds;

    let root = temp_root("thresholds");
    // Six independent tasks keeps the verdict above the default minimum cluster count.
    let tasks: Vec<BenchTask> = (0..6)
        .map(|index| BenchTask {
            id: format!("t{index}"),
            prompt: "answer".to_string(),
            files: std::collections::BTreeMap::new(),
            expect_contains: Vec::new(),
            expect_absent: Vec::new(),
            regression_guard: false,
            verify_command: None,
        })
        .collect();
    // Explicit arm names: the stub decides by name, and `adaptive_tournament(3)` defaults to
    // `adaptive_tournament_3`, which would silently make the stub pass nothing.
    let mut candidate = BenchStrategyConfig::adaptive_tournament(3);
    candidate.name = "candidate".to_string();
    let strategies = vec![BenchStrategyConfig::single_agent(), candidate];
    let options = BenchRunOptions::new(1, 21, false).with_thresholds(DecisionThresholds {
        min_effect: 0.5,
        min_clusters: 5,
        ..DecisionThresholds::default()
    });

    // The candidate arm resolves every task while the baseline resolves none: delta = +1.
    let executor = StubExecutor {
        pass_arm: "candidate",
    };
    let report = run_paired_benchmark(
        "thresholds",
        &tasks,
        &strategies,
        10_000,
        &root.join("scratch"),
        &executor,
        &options,
    )
    .await;
    let delta = &report.paired_deltas[0];
    assert_eq!(delta.cluster_count, 6, "six independent tasks: {delta:?}");
    assert_eq!(delta.decision.as_deref(), Some("accept"), "{delta:?}");
    assert!(
        delta
            .decision_rationale
            .as_deref()
            .is_some_and(|why| why.contains("0.500")),
        "the rationale must quote the preregistered threshold: {delta:?}"
    );
    let rendered = report.render();
    assert!(
        rendered.contains("decision: candidate -> accept"),
        "{rendered}"
    );
    assert!(
        rendered.contains("tasks=6"),
        "the cluster count must be printed: {rendered}"
    );

    // Without declared thresholds the report stays descriptive: no verdict is invented.
    let descriptive = run_paired_benchmark(
        "thresholds",
        &tasks,
        &strategies,
        10_000,
        &root.join("scratch"),
        &executor,
        &BenchRunOptions::new(1, 21, false),
    )
    .await;
    assert!(descriptive.paired_deltas[0].decision.is_none());
    assert!(!descriptive.render().contains("decision:"));
    let _ = std::fs::remove_dir_all(&root);
}

/// B16: an estimated token total must never be presented as measured provider usage, and the report
/// has to say so when it happens.
#[tokio::test]
async fn estimated_usage_is_labelled_in_the_report() {
    // A provider that reports no usage at all: every number is then a byte estimate.
    struct NoUsageProvider;
    impl ModelProvider for NoUsageProvider {
        fn complete<'a>(
            &'a self,
            _req: ProviderRequest,
            _budget: &'a BudgetConfig,
            _cancel_rx: watch::Receiver<bool>,
        ) -> Pin<Box<dyn Future<Output = Result<ProviderCompletion, ProviderError>> + Send + 'a>>
        {
            Box::pin(async move {
                Ok(ProviderCompletion {
                    turn: AssistantTurn {
                        content: Some("done".to_string()),
                        reasoning_blocks: Vec::new(),
                        tool_calls: Vec::new(),
                        finish_reason: FinishReason::Stop,
                        usage: None,
                    },
                    request_body_bytes: 8,
                    latency_ms: 1,
                    ttft_ms: None,
                    attempts_used: 1,
                    reported_model: None,
                })
            })
        }
    }

    let root = temp_root("estimated-usage");
    let settings = ProviderBenchSettings::new("fixture-model", "chat-completions")
        .with_policy_mode(PolicyMode::AllowWrite)
        .with_max_model_turns(1)
        .with_max_run_duration(Duration::from_secs(30));
    let executor = ProviderBenchExecutor::new(Arc::new(NoUsageProvider), settings);
    let task = priced_task();

    let report = run_paired_benchmark(
        "estimated",
        std::slice::from_ref(&task),
        &[BenchStrategyConfig::single_agent()],
        100_000,
        &root.join("scratch"),
        &executor,
        &BenchRunOptions::new(1, 17, false),
    )
    .await;

    let result = &report.results[0];
    assert_eq!(
        result.usage_basis, "byte_estimate_not_measured",
        "a provider that reports no usage yields estimates, not measured numbers: {result:?}"
    );
    let rendered = report.render();
    assert!(
        rendered.contains("not measured usage"),
        "the report must label estimated numbers: {rendered}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// B17/B19/B22 integrity: the graded repository must not be able to choose its own evaluation
/// mechanism. The benchmark arms come from a hardcoded default pair, and every experimental switch
/// starts disabled — a workspace `.kroma.toml` cannot reach them.
#[test]
fn benchmark_arms_start_with_every_experimental_mechanism_disabled() {
    let strategies = default_strategy_pair();
    assert!(
        strategies.len() >= 2,
        "the default pair must compare two arms"
    );
    for strategy in &strategies {
        assert!(
            !strategy.counterfactual_probe,
            "arm {} starts with the counterfactual probe enabled",
            strategy.name
        );
        assert!(
            !strategy.causal_pruning,
            "arm {} starts with causal pruning enabled",
            strategy.name
        );
        assert!(
            !strategy.on_demand_tools,
            "arm {} starts with on-demand schemas enabled",
            strategy.name
        );
    }
    // The single-agent arm is the minimum: no escalation and no subagents at all.
    let single = strategies
        .iter()
        .find(|strategy| strategy.name == "single_agent")
        .expect("the default pair must include the single-agent baseline");
    assert_eq!(single.adaptive_tournament_candidates, 0);
    assert_eq!(single.max_subagents, Some(0));

    // And a freshly built settings object enables nothing either.
    let settings = ProviderBenchSettings::new("m", "chat-completions");
    assert!(settings.sandbox.is_none(), "sandboxing is opt-in per run");
    assert!(settings.price_table.is_none(), "pricing is opt-in per run");
    assert!(settings.source_root.is_none());
}

/// Resolves only for the named arm and only for the listed task ids, so a test can shape a
/// development/holdout contrast.
struct SplitExecutor {
    /// Arm that plays the baseline role.
    baseline: &'static str,
    /// Arm under test.
    candidate: &'static str,
    baseline_pass_ids: Vec<String>,
    candidate_pass_ids: Vec<String>,
}

impl BenchExecutor for SplitExecutor {
    fn label(&self) -> &'static str {
        "split"
    }

    fn execute<'a>(
        &'a self,
        task: &'a BenchTask,
        strategy: &'a BenchStrategyConfig,
        _budget_tokens: u64,
        _workspace_root: &'a Path,
    ) -> Pin<Box<dyn Future<Output = BenchArmResult> + Send + 'a>> {
        let resolved = if strategy.name == self.baseline {
            self.baseline_pass_ids.iter().any(|id| id == &task.id)
        } else if strategy.name == self.candidate {
            self.candidate_pass_ids.iter().any(|id| id == &task.id)
        } else {
            false
        };
        Box::pin(async move {
            BenchArmResult {
                task_id: task.id.clone(),
                strategy: strategy.name.clone(),
                resolved,
                gate_status: if resolved { "approved" } else { "rejected" }.to_string(),
                self_input_tokens: 1,
                self_cached_input_tokens: 0,
                self_output_tokens: 0,
                children_input_tokens: 0,
                children_cached_input_tokens: 0,
                children_output_tokens: 0,
                reported_model: None,
                usage_basis: "unknown".to_string(),
                estimated_cost_nano_usd: None,
                cost_unknown_reason: None,
                self_tokens: 1,
                children_tokens: 0,
                total_tokens: 1,
                budget_exceeded: false,
                budget_tokens: 1_000,
                budget_overage: 0,
                cap_ok: true,
                finished_normally: true,
                wall_time_ms: 1,
                verify_time_ms: 0,
                regressions: 0,
                repetition: 0,
                model: None,
                prompt_hash: None,
                tool_schema_hash: None,
                terminal_reason: Some("ModelStop".to_string()),
                failure_trace: Vec::new(),
                max_subagents: 0,
                bash_allowed: false,
                counterfactual_probe: false,
                causal_pruning: false,
                require_verified: true,
                on_demand_tools: false,
                turns_used: 1,
                tool_calls_used: 0,
                modified_files: Vec::new(),
                final_text: None,
                tool_trace: Vec::new(),
                mechanism: None,
            }
        })
    }

    fn metadata(&self) -> BenchRunMetadata {
        BenchRunMetadata::default()
    }
}

fn split_tasks(count: usize) -> Vec<BenchTask> {
    (0..count)
        .map(|index| BenchTask {
            id: format!("t{index}"),
            prompt: "p".to_string(),
            files: std::collections::BTreeMap::new(),
            expect_contains: Vec::new(),
            expect_absent: Vec::new(),
            regression_guard: false,
            verify_command: None,
        })
        .collect()
}

/// B17: a verdict must rest on held-out evidence. The same results must therefore be judged
/// differently depending on which split was declared *before* the run — and a development-set
/// improvement must not become an accepted mechanism.
#[tokio::test]
async fn the_verdict_is_taken_on_the_holdout_split_when_one_is_declared() {
    use kroma_core::bench::DecisionThresholds;

    let root = temp_root("holdout-split");
    let tasks = split_tasks(12);
    let dev: Vec<String> = (0..6).map(|index| format!("t{index}")).collect();
    let holdout: Vec<String> = (6..12).map(|index| format!("t{index}")).collect();

    let mut candidate = BenchStrategyConfig::single_agent();
    candidate.name = "candidate".to_string();
    let strategies = vec![BenchStrategyConfig::single_agent(), candidate];
    let thresholds = DecisionThresholds {
        min_effect: 0.5,
        min_clusters: 5,
        ..DecisionThresholds::default()
    };

    // The arm improves every development task while the baseline keeps the held-out ones: the
    // development set looks like progress, the held-out set shows a regression.
    let executor = SplitExecutor {
        baseline: "single_agent",
        candidate: "candidate",
        baseline_pass_ids: holdout.clone(),
        candidate_pass_ids: dev.clone(),
    };
    let report = run_paired_benchmark(
        "split",
        &tasks,
        &strategies,
        10_000,
        &root.join("scratch"),
        &executor,
        &BenchRunOptions::new(1, 31, false)
            .with_thresholds(thresholds.clone())
            .with_holdout_task_ids(holdout.clone()),
    )
    .await;

    let delta = &report.paired_deltas[0];
    let holdout_stats = delta.holdout.as_ref().expect("holdout statistics");
    assert_eq!(holdout_stats.cluster_count, 6, "{delta:?}");
    assert_eq!(holdout_stats.delta_pass_rate, -1.0, "{delta:?}");
    assert_eq!(delta.decision.as_deref(), Some("reject"), "{delta:?}");
    assert_eq!(delta.decision_basis.as_deref(), Some("holdout"));
    assert!(
        delta
            .decision_rationale
            .as_deref()
            .is_some_and(|why| why.starts_with("[holdout]")),
        "the rationale must say which split was judged: {delta:?}"
    );
    assert!(report.render().contains("holdout:"), "{}", report.render());

    // The same data with no declared split is judged on every task, where the gains and losses
    // cancel: the point is that the *declared basis* decides, not the numbers alone.
    let executor = SplitExecutor {
        baseline: "single_agent",
        candidate: "candidate",
        baseline_pass_ids: holdout.clone(),
        candidate_pass_ids: dev.clone(),
    };
    let undeclared = run_paired_benchmark(
        "split",
        &tasks,
        &strategies,
        10_000,
        &root.join("scratch"),
        &executor,
        &BenchRunOptions::new(1, 31, false).with_thresholds(thresholds.clone()),
    )
    .await;
    let delta = &undeclared.paired_deltas[0];
    assert!(delta.holdout.is_none(), "no split was declared: {delta:?}");
    assert_eq!(delta.decision_basis.as_deref(), Some("all tasks"));
    assert_eq!(
        delta.decision.as_deref(),
        Some("inconclusive"),
        "cancelling gains and losses is not evidence of an effect: {delta:?}"
    );

    // A genuine held-out improvement is accepted.
    let executor = SplitExecutor {
        baseline: "single_agent",
        candidate: "candidate",
        baseline_pass_ids: Vec::new(),
        candidate_pass_ids: holdout.clone(),
    };
    let real = run_paired_benchmark(
        "split",
        &tasks,
        &strategies,
        10_000,
        &root.join("scratch"),
        &executor,
        &BenchRunOptions::new(1, 31, false)
            .with_thresholds(thresholds)
            .with_holdout_task_ids(holdout),
    )
    .await;
    let delta = &real.paired_deltas[0];
    assert_eq!(delta.decision.as_deref(), Some("accept"), "{delta:?}");
    assert_eq!(delta.decision_basis.as_deref(), Some("holdout"));
    let _ = std::fs::remove_dir_all(&root);
}
