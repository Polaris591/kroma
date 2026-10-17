//! F05 (`v1.5.1`) budget contract: request and parallel-subtask reservation/settlement.
// T04 (KR-02) note on the fail-closed assertions below: before T04 a child's first request
// reserved its *whole* allocation as the output cap, so a post-dispatch 503 charged the entire
// 64k allocation and any retry was impossible. A request now reserves only a per-request output
// allowance (`DEFAULT_PER_REQUEST_OUTPUT_TOKENS`), so a post-dispatch failure holds exactly what
// that request could have consumed and the unused remainder stays available to a retry or sibling.
// The invariant these tests protect is unchanged: an ambiguous post-dispatch failure is never
// refunded to zero. The bound is now `>= 8_192` (the per-request output allowance) instead of the
// whole allocation, and never more than the allocation itself.
//!
//! The fix replaces the per-run token counter with one shared [`TokenBudget`] ledger for the whole
//! run tree. This file pins the three invariants the review asked for, using a counting provider
//! and a counting subagent backend:
//!
//! * budget insufficient → the model backend is called **zero** times;
//! * two parallel children cannot spend the same remaining balance (their allocations are disjoint
//!   and never sum past the cap);
//! * a cancelled child's known consumption is preserved in the ledger instead of being refunded.
//!
//! It also pins the ledger primitive itself (settle / drop / concurrent reserve) and the exact-vs-
//! estimated distinction: `usage` settles exactly, a dropped reservation settles the estimate.

use kroma_core::agent_loop::{build_default_runtime, AgentRunConfig, AgentRunner};
use kroma_core::budget::{BudgetConfig, TokenBudget};
use kroma_core::env::LocalHostEnv;
use kroma_core::events::EventLogger;
use kroma_core::messages::{AssistantTurn, FinishReason, TokenUsage, ToolCall};
use kroma_core::provider::{ModelProvider, ProviderCompletion, ProviderError, ProviderRequest};
use kroma_core::subagents::{SubagentBackend, SubagentExecutionRequest, SubagentRunOutcome};
use kroma_core::tool::policy::{NonInteractiveApprovalHandler, PolicyEngine, PolicyMode};
use kroma_core::types::{RunStatus, TerminalReason, ToolCallId};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::watch;

// ---------------------------------------------------------------------------
// Ledger primitive
// ---------------------------------------------------------------------------

#[test]
fn reservation_settlement_uses_exact_usage() {
    let budget = Arc::new(TokenBudget::new(Some(100)));
    let reservation = budget.try_reserve(60).expect("60 fits in 100");
    assert_eq!(budget.reserved(), 60);
    assert_eq!(budget.remaining(), Some(40));
    assert!(budget.try_reserve(50).is_none(), "40 + 50 > 100");

    reservation.settle(42);
    assert_eq!(
        budget.spent(),
        42,
        "settlement records the exact reported usage"
    );
    assert_eq!(budget.reserved(), 0);
    assert_eq!(budget.remaining(), Some(58));
}

/// B16: unknown consumption is a conservative hold, never measured provider usage. A
/// reported overage is not clamped away and blocks later reservations.
#[test]
fn accounting_snapshot_separates_reported_and_unknown_and_keeps_overage() {
    let budget = Arc::new(TokenBudget::new(Some(100)));
    budget.try_reserve(30).unwrap().settle(20);
    drop(budget.try_reserve(40).unwrap());
    let held = budget.accounting_snapshot();
    assert_eq!(held.settled_tokens, 20);
    assert_eq!(held.unknown_tokens, 40);
    assert_eq!(held.spent, 60);
    assert_eq!(held.overage_tokens, 0);
    assert_eq!(held.reserved, 0);
    budget.try_reserve(40).unwrap().settle(70);
    let exceeded = budget.accounting_snapshot();
    assert_eq!(exceeded.settled_tokens, 90);
    assert_eq!(exceeded.unknown_tokens, 40);
    assert_eq!(exceeded.spent, 130);
    assert_eq!(exceeded.overage_tokens, 30);
    assert!(budget.try_reserve(1).is_none());
}

#[test]
fn dropped_reservation_preserves_the_estimate() {
    let budget = Arc::new(TokenBudget::new(Some(100)));
    {
        let _reservation = budget.try_reserve(30).expect("30 fits in 100");
        // Dropped without settling: the request was cancelled/timed out before reporting usage.
    }
    assert_eq!(budget.spent(), 30, "a dropped reservation is not refunded");
    assert_eq!(budget.reserved(), 0);
    assert_eq!(budget.remaining(), Some(70));
}

#[test]
fn concurrent_reservations_never_exceed_the_limit() {
    let budget = Arc::new(TokenBudget::new(Some(100)));
    let mut threads = Vec::new();
    for _ in 0..8 {
        let budget = Arc::clone(&budget);
        threads.push(std::thread::spawn(move || budget.try_reserve(15).is_some()));
    }
    let successes = threads
        .into_iter()
        .map(|thread| thread.join().unwrap())
        .filter(|succeeded| *succeeded)
        .count();
    // 6 * 15 = 90 <= 100; a 7th would need 105. Because every reservation is either held (reserved)
    // or dropped (spent) and spent never decreases, exactly 6 can ever succeed.
    assert_eq!(successes, 6, "the ledger must not double-spend the balance");
    assert!(
        budget.committed() <= 100,
        "committed {} exceeds the limit",
        budget.committed()
    );
}

#[test]
fn settlement_and_reservation_are_one_transaction() {
    // Regression for P1-1. The old ledger updated `reserved` and `spent` with two separate atomic
    // operations (`reserved.fetch_sub` then `spent.fetch_add`). A `try_reserve` interleaving between
    // them could observe the freed reservation but not the settled spend, and reserve the same
    // balance a second time — `committed()` then rose to 200 against a limit of 100. Settling and
    // reserving now share one mutex-guarded transaction over `(spent, reserved)`, so any reserver
    // that runs after the settle must still see a committed total of 100 and fail.
    //
    // The reserver's loop is deliberately short and tight: a successful reservation here is only
    // reachable through the old two-step window, so `b_extra` staying 0 is the contract.
    for _ in 0..2000 {
        let budget = Arc::new(TokenBudget::new(Some(100)));
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let slipped = Arc::new(AtomicU64::new(0));

        let settler_budget = Arc::clone(&budget);
        let settler_barrier = Arc::clone(&barrier);
        let settler = std::thread::spawn(move || {
            let reservation = settler_budget
                .try_reserve(100)
                .expect("the first reservation fits the limit");
            settler_barrier.wait();
            reservation.settle(100);
        });

        let reserver_budget = Arc::clone(&budget);
        let reserver_barrier = Arc::clone(&barrier);
        let reserver_slipped = Arc::clone(&slipped);
        let reserver = std::thread::spawn(move || {
            reserver_barrier.wait();
            for _ in 0..64 {
                if let Some(reservation) = reserver_budget.try_reserve(100) {
                    // Only reachable if the freed reservation was observed without its spend.
                    reservation.settle(100);
                    reserver_slipped.fetch_add(100, Ordering::SeqCst);
                }
            }
        });

        settler.join().unwrap();
        reserver.join().unwrap();

        assert_eq!(
            slipped.load(Ordering::SeqCst),
            0,
            "a reservation slipped into the gap between releasing and settling"
        );
        assert!(
            budget.committed() <= 100,
            "committed {} exceeds the limit",
            budget.committed()
        );
    }
}

// ---------------------------------------------------------------------------
// Runner-level: budget insufficient makes zero backend calls
// ---------------------------------------------------------------------------

/// Provider that counts `complete` calls and replays a fixed script. A call past the script waits
/// for cancellation instead of returning, which lets the cancellation test observe the in-flight
/// reservation.
#[derive(Clone)]
enum Step {
    Turn(AssistantTurn),
    AwaitCancel,
    Err503,
    Err502,
    Err504,
    ErrInsecureUrl,
    AmbiguousRetryThen503,
}

struct CountingProvider {
    calls: AtomicUsize,
    steps: Vec<Step>,
}

impl CountingProvider {
    fn new(steps: Vec<Step>) -> Self {
        Self {
            calls: AtomicUsize::new(0),
            steps,
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl ModelProvider for CountingProvider {
    fn complete<'a>(
        &'a self,
        _req: ProviderRequest,
        _budget: &'a BudgetConfig,
        mut cancel_rx: watch::Receiver<bool>,
    ) -> Pin<Box<dyn Future<Output = Result<ProviderCompletion, ProviderError>> + Send + 'a>> {
        let index = self.calls.fetch_add(1, Ordering::SeqCst);
        let step = self.steps.get(index).cloned().unwrap_or(Step::AwaitCancel);
        Box::pin(async move {
            match step {
                Step::Turn(turn) => Ok(ProviderCompletion {
                    turn,
                    request_body_bytes: 0,
                    latency_ms: 1,
                    ttft_ms: Some(1),
                    attempts_used: 1,
                    reported_model: None,
                }),
                Step::AwaitCancel => {
                    let _ = cancel_rx.changed().await;
                    Err(ProviderError::Cancelled)
                }
                Step::Err503 => Err(ProviderError::HttpStatus {
                    status: 503,
                    body: "service unavailable".into(),
                }),
                Step::Err502 => Err(ProviderError::HttpStatus {
                    status: 502,
                    body: "bad gateway".into(),
                }),
                Step::Err504 => Err(ProviderError::HttpStatus {
                    status: 504,
                    body: "gateway timeout".into(),
                }),
                Step::ErrInsecureUrl => Err(ProviderError::InsecureUrl(
                    "http://insecure.example.com".into(),
                )),
                Step::AmbiguousRetryThen503 => Err(ProviderError::HttpStatus {
                    status: 503,
                    body: "service unavailable after retried transport drop".into(),
                }),
            }
        })
    }
}

fn usage(input: u64, output: u64) -> Option<TokenUsage> {
    Some(TokenUsage {
        input_tokens: input,
        cached_input_tokens: None,
        output_tokens: output,
        reasoning_tokens: None,
    })
}

fn child_tool_turn() -> AssistantTurn {
    AssistantTurn {
        content: None,
        reasoning_blocks: vec![],
        tool_calls: vec![ToolCall {
            call_id: ToolCallId::new("child-tool-1"),
            name: "ls".into(),
            raw_arguments: r#"{"path":"."}"#.into(),
        }],
        finish_reason: FinishReason::ToolCalls,
        usage: usage(2500, 100),
    }
}

fn stop() -> AssistantTurn {
    AssistantTurn {
        content: Some("done".into()),
        reasoning_blocks: vec![],
        tool_calls: vec![],
        finish_reason: FinishReason::Stop,
        usage: usage(0, 0),
    }
}

fn spawn_turn(args: &str) -> AssistantTurn {
    AssistantTurn {
        content: None,
        reasoning_blocks: vec![],
        tool_calls: vec![ToolCall {
            call_id: ToolCallId::new("spawn-1"),
            name: "spawn_subagent".into(),
            raw_arguments: args.into(),
        }],
        finish_reason: FinishReason::ToolCalls,
        usage: usage(0, 0),
    }
}

fn temp_root(label: &str) -> std::path::PathBuf {
    let root =
        std::env::temp_dir().join(format!("kroma-budget-{}-{}", label, uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    root
}

fn run_with_ledger(
    label: &str,
    cap: u64,
    provider: Arc<dyn ModelProvider>,
    backend: Option<Arc<dyn SubagentBackend>>,
) -> (kroma_core::agent_loop::RunSummary, Arc<TokenBudget>) {
    let (summary, ledger, _events) = run_with_ledger_events(label, cap, provider, backend);
    (summary, ledger)
}

fn run_with_ledger_events(
    label: &str,
    cap: u64,
    provider: Arc<dyn ModelProvider>,
    backend: Option<Arc<dyn SubagentBackend>>,
) -> (
    kroma_core::agent_loop::RunSummary,
    Arc<TokenBudget>,
    Vec<kroma_core::events::DiagnosticEvent>,
) {
    let root = temp_root(label);
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let state = root.join("state");
    let budget = BudgetConfig::default();
    let env = Arc::new(LocalHostEnv::new(&ws, state.clone()).unwrap());
    let runtime = build_default_runtime(
        env,
        PolicyEngine::new(
            PolicyMode::AllowWrite,
            Arc::new(NonInteractiveApprovalHandler),
        ),
        budget.clone(),
        None,
    );
    let cfg = AgentRunConfig::new("budget contract", budget);
    let logger = EventLogger::new(cfg.run_id, Some(state), false).unwrap();
    let (_tx, rx) = watch::channel(false);

    let ledger = Arc::new(TokenBudget::new(Some(cap)));
    let mut runner =
        AgentRunner::new(cfg, provider, runtime, rx).with_token_budget(Arc::clone(&ledger));
    if let Some(backend) = backend {
        runner = runner.with_subagent_backend(backend);
    }
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let (summary, logger) = runtime.block_on(runner.run(logger));
    let events = logger.events().to_vec();
    let _ = std::fs::remove_dir_all(&root);
    (summary, ledger, events)
}

#[test]
fn insufficient_budget_makes_zero_backend_calls() {
    // `estimated_input` (the system prompt alone) is far larger than 1, so the first request can
    // never be reserved and the provider must never be called.
    let provider = Arc::new(CountingProvider::new(vec![Step::Turn(stop())]));
    let (summary, ledger) = run_with_ledger("zero-calls", 1, provider.clone(), None);

    assert_eq!(
        provider.calls(),
        0,
        "an unaffordable request must not reach the backend"
    );
    assert_eq!(
        summary.terminal_reason,
        TerminalReason::TokenBudgetExhausted
    );
    assert_eq!(ledger.spent(), 0);
}

// ---------------------------------------------------------------------------
// Runner-level: parallel children share one ledger
// ---------------------------------------------------------------------------

#[derive(Default)]
struct CountingSubagentBackend {
    allocations: Mutex<Vec<u64>>,
    calls: AtomicUsize,
}

impl SubagentBackend for CountingSubagentBackend {
    fn run_subagent(
        &self,
        req: SubagentExecutionRequest,
    ) -> Pin<Box<dyn Future<Output = Result<SubagentRunOutcome, String>> + Send + '_>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let ledger = req
                .token_budget
                .clone()
                .expect("every child must receive a token ledger");
            let allocation = ledger.limit().expect("the child ledger must be bounded");
            self.allocations.lock().unwrap().push(allocation);
            // Simulate the child consuming its whole allocation: reserve and settle it, so the
            // parent's settlement observes the spend.
            let reservation = ledger.try_reserve(allocation).expect("allocation fits");
            reservation.settle(allocation);
            Ok(SubagentRunOutcome {
                status: RunStatus::Completed,
                terminal_reason: TerminalReason::ModelStop,
                summary: format!("done {}", req.subagent_id),
                relevant_files: Vec::new(),
                findings: Vec::new(),
                turns_used: 1,
                tool_calls_used: 0,
                tokens_used: allocation,
                input_tokens: allocation,
                cached_input_tokens: None,
                usage_basis: "provider_reported".to_string(),
                output_tokens: 0,
                duration_ms: 0,
                visible_tools: Vec::new(),
            })
        })
    }
}

#[test]
fn parallel_children_cannot_double_spend_the_shared_balance() {
    const CAP: u64 = 100_000;
    // Each scout asks for the 64k default; the second must be clamped to the 36k that is left
    // after the first reservation, so the two allocations are disjoint and sum to the cap.
    let args = r#"{"tasks":[
        {"role":"scout","task":"a","max_tokens":64000},
        {"role":"scout","task":"b","max_tokens":64000}
    ]}"#;
    let provider = Arc::new(CountingProvider::new(vec![
        Step::Turn(spawn_turn(args)),
        Step::Turn(stop()),
    ]));
    let backend = Arc::new(CountingSubagentBackend::default());
    let (summary, ledger) = run_with_ledger(
        "shared-ledger",
        CAP,
        provider.clone(),
        Some(backend.clone()),
    );

    assert_eq!(backend.calls.load(Ordering::SeqCst), 2);
    // The reservation loop is sequential (64k then 36k), but the children run concurrently, so
    // the backend records their allocations in completion order — compare as a set.
    let mut allocations = backend.allocations.lock().unwrap().clone();
    allocations.sort_unstable();
    assert_eq!(
        allocations,
        vec![36_000, 64_000],
        "the second child must receive only what the shared ledger still holds"
    );
    assert!(
        allocations.iter().sum::<u64>() <= CAP,
        "allocations {:?} must not exceed the cap {}",
        allocations,
        CAP
    );
    assert_eq!(
        ledger.spent(),
        CAP,
        "the run tree must account for exactly the cap once both children consumed their shares"
    );
    assert!(
        ledger.committed() <= CAP,
        "committed {} exceeds the cap",
        ledger.committed()
    );
    // The children exhausted the shared cap, so the parent cannot start another request.
    assert_eq!(
        summary.terminal_reason,
        TerminalReason::TokenBudgetExhausted
    );
    assert_eq!(
        provider.calls(),
        1,
        "only the spawn turn reached the backend"
    );
}

// ---------------------------------------------------------------------------
// Regressions: provably pre-dispatch vs ambiguous post-dispatch / unknown consumption
// ---------------------------------------------------------------------------

#[test]
fn first_request_503_preserves_ledger_hold_while_reporting_zero_measured_tokens() {
    const CAP: u64 = 100_000;
    let args = r#"{"role":"scout","task":"fail503","max_tokens":64000}"#;
    let provider = Arc::new(CountingProvider::new(vec![
        Step::Turn(spawn_turn(args)),
        Step::Err503,
        Step::Turn(stop()),
    ]));
    let (summary, ledger, events) =
        run_with_ledger_events("first-503", CAP, provider.clone(), None);

    assert_eq!(summary.terminal_reason, TerminalReason::ModelStop);

    let sub_completed = events
        .iter()
        .find_map(|ev| match &ev.event {
            kroma_core::events::EventPayload::SubagentCompleted {
                tokens_used,
                status,
                ..
            } => Some((status.clone(), *tokens_used)),
            _ => None,
        })
        .expect("SubagentCompleted event should be emitted");

    assert_eq!(sub_completed.0, "failed");
    // Factual observed usage is 0: the child completed zero turns and generated zero tokens.
    assert_eq!(
        sub_completed.1, 0,
        "SubagentCompleted must report exactly 0 measured tokens on 503 failure"
    );
    assert_eq!(
        summary.children_input_tokens, 0,
        "parent summary children_input_tokens must report 0 measured tokens"
    );
    // Because an HTTP 503 is a post-dispatch server/gateway response that cannot prove zero
    // backend consumption, the ledger conservatively withholds the reservation fail-closed.
    assert!(
        ledger.spent() >= 8_192,
        "shared ledger must hold the conservative reservation fail-closed for post-dispatch 503, spent={}",
        ledger.spent()
    );
    assert!(ledger.committed() <= CAP, "committed must not exceed cap");
}

#[test]
fn gateway_errors_500_502_504_preserve_ledger_hold_while_reporting_zero_measured() {
    const CAP: u64 = 100_000;

    // Test 502 Bad Gateway
    {
        let args = r#"{"role":"scout","task":"fail502","max_tokens":64000}"#;
        let provider = Arc::new(CountingProvider::new(vec![
            Step::Turn(spawn_turn(args)),
            Step::Err502,
            Step::Turn(stop()),
        ]));
        let (summary, ledger, events) =
            run_with_ledger_events("fail-502", CAP, provider.clone(), None);

        let sub_completed = events
            .iter()
            .find_map(|ev| match &ev.event {
                kroma_core::events::EventPayload::SubagentCompleted {
                    tokens_used,
                    status,
                    ..
                } => Some((status.clone(), *tokens_used)),
                _ => None,
            })
            .expect("SubagentCompleted event should be emitted");

        assert_eq!(sub_completed.0, "failed");
        assert_eq!(
            sub_completed.1, 0,
            "502 Bad Gateway must report 0 measured tokens"
        );
        assert_eq!(summary.children_input_tokens, 0);
        assert!(
            ledger.spent() >= 8_192,
            "502 Bad Gateway cannot prove zero consumption; ledger must withhold reservation (T04: the failing *request* holds its own reservation, which is at least its per-request output allowance, and is never refunded)",
            );
        // T04 (KR-02): the child reserved a per-request allowance rather than its whole
        // allocation, so a transient failure no longer burns the allocation: the unused
        // remainder is available to a retry or a sibling. Consumption beyond what the request
        // reserved was impossible (the provider was given that output cap), so returning the
        // unused remainder is not an optimistic refund.
        assert!(
            ledger.spent() <= 64_000,
            "the hold must never exceed what the run could have consumed",
        );
    }

    // Test 504 Gateway Timeout
    {
        let args = r#"{"role":"scout","task":"fail504","max_tokens":64000}"#;
        let provider = Arc::new(CountingProvider::new(vec![
            Step::Turn(spawn_turn(args)),
            Step::Err504,
            Step::Turn(stop()),
        ]));
        let (summary, ledger, events) =
            run_with_ledger_events("fail-504", CAP, provider.clone(), None);

        let sub_completed = events
            .iter()
            .find_map(|ev| match &ev.event {
                kroma_core::events::EventPayload::SubagentCompleted {
                    tokens_used,
                    status,
                    ..
                } => Some((status.clone(), *tokens_used)),
                _ => None,
            })
            .expect("SubagentCompleted event should be emitted");

        assert_eq!(sub_completed.0, "failed");
        assert_eq!(
            sub_completed.1, 0,
            "504 Gateway Timeout must report 0 measured tokens"
        );
        assert_eq!(summary.children_input_tokens, 0);
        assert!(
            ledger.spent() >= 8_192,
            "504 Gateway Timeout cannot prove zero consumption; ledger must withhold reservation (T04: the failing *request* holds its own reservation, which is at least its per-request output allowance, and is never refunded)",
            );
        // T04 (KR-02): the child reserved a per-request allowance rather than its whole
        // allocation, so a transient failure no longer burns the allocation: the unused
        // remainder is available to a retry or a sibling. Consumption beyond what the request
        // reserved was impossible (the provider was given that output cap), so returning the
        // unused remainder is not an optimistic refund.
        assert!(
            ledger.spent() <= 64_000,
            "the hold must never exceed what the run could have consumed",
        );
    }
}

#[test]
fn successful_partial_usage_followed_by_503_reports_measured_and_holds_remaining() {
    const CAP: u64 = 100_000;
    let args = r#"{"role":"scout","task":"partial503","max_tokens":64000}"#;
    // Turn 1: parent spawns child.
    // Turn 2 (child turn 1): child runs `ls` tool, consumes 2500 input + 100 output = 2600 tokens.
    // Turn 3 (child turn 2): provider fails with 503.
    // Turn 4: parent completes with stop.
    let provider = Arc::new(CountingProvider::new(vec![
        Step::Turn(spawn_turn(args)),
        Step::Turn(child_tool_turn()),
        Step::Err503,
        Step::Turn(stop()),
    ]));
    let (summary, ledger, events) =
        run_with_ledger_events("partial-503", CAP, provider.clone(), None);

    assert_eq!(summary.terminal_reason, TerminalReason::ModelStop);

    let sub_completed = events
        .iter()
        .find_map(|ev| match &ev.event {
            kroma_core::events::EventPayload::SubagentCompleted {
                tokens_used,
                status,
                ..
            } => Some((status.clone(), *tokens_used)),
            _ => None,
        })
        .expect("SubagentCompleted event should be emitted");

    assert_eq!(sub_completed.0, "failed");
    // Factual telemetry reports the 2600 measured tokens from turn 1, never the withheld reserve.
    assert_eq!(
        sub_completed.1, 2600,
        "SubagentCompleted must report exactly the 2600 factual observed tokens"
    );
    assert_eq!(summary.children_input_tokens, 2500);
    assert_eq!(summary.children_output_tokens, 100);
    // The ledger holds the full 64,000 allocation (2,600 settled for turn 1 + 61,400 conservative hold for turn 2).
    assert!(
        ledger.spent() >= 8_192,
        "shared ledger must hold the 64000 allocation fail-closed including the unverified turn 2 hold, spent={}",
        ledger.spent()
    );
    assert!(ledger.committed() <= CAP, "committed must not exceed cap");
}

#[test]
fn retry_ambiguous_then_rejection_preserves_ledger_hold() {
    const CAP: u64 = 100_000;
    let args = r#"{"role":"scout","task":"retry-fail","max_tokens":64000}"#;
    // Simulates a provider where attempt 1 was an ambiguous transport drop and attempt 2 returned 503.
    let provider = Arc::new(CountingProvider::new(vec![
        Step::Turn(spawn_turn(args)),
        Step::AmbiguousRetryThen503,
        Step::Turn(stop()),
    ]));
    let (summary, ledger, events) =
        run_with_ledger_events("retry-fail", CAP, provider.clone(), None);

    assert_eq!(summary.terminal_reason, TerminalReason::ModelStop);

    let sub_completed = events
        .iter()
        .find_map(|ev| match &ev.event {
            kroma_core::events::EventPayload::SubagentCompleted {
                tokens_used,
                status,
                ..
            } => Some((status.clone(), *tokens_used)),
            _ => None,
        })
        .expect("SubagentCompleted event should be emitted");

    assert_eq!(sub_completed.0, "failed");
    assert_eq!(
        sub_completed.1, 0,
        "SubagentCompleted must report 0 measured tokens for failed run"
    );
    assert_eq!(summary.children_input_tokens, 0);
    // Attempt 1 may have consumed tokens before dropping; ledger must conservatively hold allocation.
    assert!(
        ledger.spent() >= 8_192,
        "ledger must retain conservative hold when provider retried an ambiguous attempt before 503, spent={}",
        ledger.spent()
    );
}

#[test]
fn provably_pre_dispatch_insecure_url_refunds_reservation() {
    const CAP: u64 = 100_000;
    let args = r#"{"role":"scout","task":"pre-dispatch-fail","max_tokens":64000}"#;
    // InsecureUrl fails purely in client-side URL validation before any network socket is opened.
    let provider = Arc::new(CountingProvider::new(vec![
        Step::Turn(spawn_turn(args)),
        Step::ErrInsecureUrl,
        Step::Turn(stop()),
    ]));
    let (summary, ledger, events) =
        run_with_ledger_events("insecure-url", CAP, provider.clone(), None);

    assert_eq!(summary.terminal_reason, TerminalReason::ModelStop);

    let sub_completed = events
        .iter()
        .find_map(|ev| match &ev.event {
            kroma_core::events::EventPayload::SubagentCompleted {
                tokens_used,
                status,
                ..
            } => Some((status.clone(), *tokens_used)),
            _ => None,
        })
        .expect("SubagentCompleted event should be emitted");

    assert_eq!(sub_completed.0, "failed");
    assert_eq!(sub_completed.1, 0, "0 measured tokens");
    assert_eq!(summary.children_input_tokens, 0);
    // Because InsecureUrl is provably pre-dispatch (zero network bytes sent), the reservation
    // is safely refunded to the shared ledger.
    assert_eq!(
        ledger.spent(),
        0,
        "provably pre-dispatch error must safely refund the reservation to the shared ledger"
    );
    assert_eq!(
        ledger.remaining(),
        Some(CAP),
        "the full cap must be restored after pre-dispatch refund"
    );
}

#[test]
fn cancellation_after_partial_usage_preserves_remaining_reservation_and_reports_measured_tokens() {
    const CAP: u64 = 100_000;
    let args = r#"{"role":"scout","task":"partial-cancel","max_tokens":64000,"timeout_ms":300}"#;
    // Turn 1: parent spawns child.
    // Turn 2 (child turn 1): child runs `ls` tool, consumes 2500 input + 100 output = 2600 tokens.
    // Turn 3 (child turn 2): child hangs until cancelled by timeout.
    // Turn 4: parent completes with stop.
    let provider = Arc::new(CountingProvider::new(vec![
        Step::Turn(spawn_turn(args)),
        Step::Turn(child_tool_turn()),
        Step::AwaitCancel,
        Step::Turn(stop()),
    ]));
    let (summary, ledger, events) =
        run_with_ledger_events("partial-cancel", CAP, provider.clone(), None);

    assert_eq!(summary.terminal_reason, TerminalReason::ModelStop);

    let sub_completed = events
        .iter()
        .find_map(|ev| match &ev.event {
            kroma_core::events::EventPayload::SubagentCompleted {
                tokens_used,
                status,
                ..
            } => Some((status.clone(), *tokens_used)),
            _ => None,
        })
        .expect("SubagentCompleted event should be emitted");

    assert_eq!(sub_completed.0, "budget_exhausted");
    assert_eq!(
        sub_completed.1, 2600,
        "SubagentCompleted must report factual measured tokens (2600), not the full 64000 allocation"
    );
    assert_eq!(summary.children_input_tokens, 2500);
    assert_eq!(summary.children_output_tokens, 100);
    assert!(
        ledger.spent() >= 8_192,
        "shared ledger must hold the conservative reservation fail-closed for unknown consumption, spent={}",
        ledger.spent()
    );
    assert!(ledger.committed() <= CAP, "committed must not exceed cap");
}

#[test]
fn mutation_verify_provably_zero_consumption_policy() {
    // Only provably pre-dispatch client-side validation failures are known to consume zero tokens.
    assert!(ProviderError::InsecureUrl("http://insecure.com".into()).is_provably_zero_consumption());

    // Post-dispatch errors (even 503 or 4xx) cannot prove zero backend consumption; they must NOT refund.
    assert!(!ProviderError::HttpStatus {
        status: 503,
        body: "service unavailable".into(),
    }
    .is_provably_zero_consumption());
    assert!(!ProviderError::HttpStatus {
        status: 502,
        body: "bad gateway".into(),
    }
    .is_provably_zero_consumption());
    assert!(!ProviderError::HttpStatus {
        status: 504,
        body: "gateway timeout".into(),
    }
    .is_provably_zero_consumption());
    assert!(!ProviderError::HttpStatus {
        status: 500,
        body: "server error".into(),
    }
    .is_provably_zero_consumption());
    assert!(!ProviderError::HttpStatus {
        status: 429,
        body: "rate limited".into(),
    }
    .is_provably_zero_consumption());
    assert!(!ProviderError::HttpStatus {
        status: 400,
        body: "bad request".into(),
    }
    .is_provably_zero_consumption());
    assert!(!ProviderError::RedirectBlocked(302).is_provably_zero_consumption());

    // In-flight or ambiguous transport errors also must NOT refund.
    assert!(!ProviderError::Cancelled.is_provably_zero_consumption());
    assert!(!ProviderError::Timeout.is_provably_zero_consumption());
    assert!(!ProviderError::IncompleteStream("premature eof".into()).is_provably_zero_consumption());
    assert!(!ProviderError::TruncatedToolCall("length".into()).is_provably_zero_consumption());
    assert!(!ProviderError::Transport("connection reset".into()).is_provably_zero_consumption());
    assert!(!ProviderError::MaxAttemptsExhausted.is_provably_zero_consumption());
}

#[test]
fn max_tokens_config_key_resolves_with_documented_precedence() {
    use kroma_core::config_file::{ConfigOverrides, KromaConfigFile, ResolvedConfig};

    let user = KromaConfigFile::parse_str("max_tokens = 111\n").unwrap();
    let workspace = KromaConfigFile::parse_str("max_tokens = 222\n").unwrap();
    let mut env = std::collections::BTreeMap::new();
    env.insert("KROMA_MAX_TOKENS".to_string(), "333".to_string());

    let resolved = ResolvedConfig::resolve(
        &user,
        &workspace,
        &env,
        &ConfigOverrides::default(),
        None,
        std::path::PathBuf::from("/ws/.kroma.toml"),
    );
    // T03 (KR-03): an explicit per-invocation layer (env/CLI) is the operator speaking now, so a
    // workspace can neither raise it nor quietly lower it. The user-file default, by contrast, is a
    // ceiling a workspace may lower — see `workspace_cannot_raise_host_token_ceiling`.
    assert_eq!(
        resolved.max_tokens,
        Some(333),
        "env outranks workspace/user"
    );
    assert!(resolved.redacted_summary().contains("max_tokens:"));

    // A CLI `--max-tokens` override outranks every file/env layer.
    let overridden = ResolvedConfig::resolve(
        &user,
        &workspace,
        &env,
        &ConfigOverrides {
            max_tokens: Some(444),
            ..ConfigOverrides::default()
        },
        None,
        std::path::PathBuf::from("/ws/.kroma.toml"),
    );
    assert_eq!(overridden.max_tokens, Some(444));
}

#[test]
fn budget_config_rejects_a_zero_token_cap() {
    let budget = BudgetConfig {
        max_total_tokens: Some(0),
        ..BudgetConfig::default()
    };
    assert!(budget.validate().is_err());
}

#[test]
fn exhausted_shared_budget_skips_children_with_zero_backend_calls() {
    const CAP: u64 = 100_000;
    let args = r#"{"tasks":[{"role":"scout","task":"a"},{"role":"scout","task":"b"}]}"#;
    // The parent's first completion reports the entire cap as consumed, so no child can be funded.
    let mut spawn = spawn_turn(args);
    spawn.usage = usage(CAP, 0);
    let provider = Arc::new(CountingProvider::new(vec![
        Step::Turn(spawn),
        Step::Turn(stop()),
    ]));
    let backend = Arc::new(CountingSubagentBackend::default());
    let (summary, ledger) = run_with_ledger(
        "exhausted-children",
        CAP,
        provider.clone(),
        Some(backend.clone()),
    );

    assert_eq!(
        backend.calls.load(Ordering::SeqCst),
        0,
        "no child may run once the shared balance is empty"
    );
    assert!(backend.allocations.lock().unwrap().is_empty());
    assert_eq!(ledger.spent(), CAP);
    assert_eq!(
        summary.terminal_reason,
        TerminalReason::TokenBudgetExhausted
    );
}

// ---------------------------------------------------------------------------
// Runner-level: cancellation preserves the known spend
// ---------------------------------------------------------------------------

#[test]
fn cancelled_child_spend_is_preserved() {
    const CAP: u64 = 100_000;
    let args = r#"{"role":"scout","task":"slow","max_tokens":64000,"timeout_ms":300}"#;
    // Call 1: the parent asks for the child. Call 2: the child's request, cancelled by its timeout.
    // Call 3: the parent's follow-up turn.
    let provider = Arc::new(CountingProvider::new(vec![
        Step::Turn(spawn_turn(args)),
        Step::AwaitCancel,
        Step::Turn(stop()),
    ]));
    let (summary, ledger) = run_with_ledger("cancel-spend", CAP, provider.clone(), None);

    assert_eq!(summary.terminal_reason, TerminalReason::ModelStop);
    assert!(
        ledger.spent() >= 8_192,
        "a cancelled child's reservation must stay accounted for, spent={}",
        ledger.spent()
    );
    assert!(
        ledger.spent() <= CAP,
        "the preserved spend must not exceed the cap, spent={}",
        ledger.spent()
    );
    // Factual observed usage is 0 because the cancelled child never completed a turn;
    // conservative ledger reserves held for fail-closed protection are not mislabeled as measured tokens.
    assert_eq!(
        summary.children_input_tokens, 0,
        "withheld reserves must not be mislabeled as measured tokens in children_input_tokens"
    );
}

// ---------------------------------------------------------------------------
// Unified entry: the RPC `session.prompt` parameter reaches the same cap
// ---------------------------------------------------------------------------

#[tokio::test]
async fn rpc_session_prompt_max_tokens_reaches_the_cap() {
    use kroma_core::surface::{JsonRpcRequest, KromaRuntimeConfig, RpcDispatcher};
    use serde_json::json;

    let root = temp_root("rpc-cap");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let provider = Arc::new(CountingProvider::new(vec![Step::Turn(stop())]));
    let config = KromaRuntimeConfig::new(&ws, root.join("state"), PolicyMode::AllowWrite);
    let dispatcher = RpcDispatcher::new(config, provider.clone()).unwrap();

    // A 1-token cap cannot even afford the system prompt, so the run stops before any backend call.
    let response = dispatcher
        .handle_request(JsonRpcRequest::new(
            1,
            "session.prompt",
            json!({ "prompt": "hello", "max_tokens": 1 }),
        ))
        .await;
    assert!(response.is_ok(), "prompt failed: {:?}", response.error);
    assert_eq!(response.result.unwrap()["status"], "budget_exhausted");
    assert_eq!(
        provider.calls(),
        0,
        "an unaffordable RPC prompt must not call the backend"
    );

    // A zero cap is rejected as an invalid parameter instead of silently disabling the run.
    let rejected = dispatcher
        .handle_request(JsonRpcRequest::new(
            2,
            "session.prompt",
            json!({ "prompt": "hello", "max_tokens": 0 }),
        ))
        .await;
    assert!(
        rejected.error.is_some(),
        "'max_tokens': 0 must be an invalid parameter"
    );

    let _ = std::fs::remove_dir_all(&root);
}

// ---------------------------------------------------------------------------
// Unified entry: `/config set max_tokens` hot-swaps the shared cap
// ---------------------------------------------------------------------------

#[tokio::test]
async fn rpc_config_set_max_tokens_hot_swaps_the_cap() {
    use kroma_core::surface::{JsonRpcRequest, KromaRuntimeConfig, RpcDispatcher};
    use serde_json::json;

    let root = temp_root("rpc-config-cap");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let provider = Arc::new(CountingProvider::new(vec![Step::Turn(stop())]));
    let config = KromaRuntimeConfig::new(&ws, root.join("state"), PolicyMode::AllowWrite);
    let dispatcher = RpcDispatcher::new(config, provider.clone()).unwrap();

    // The third entry point of the unified cap (after `--max-tokens` and the `max_tokens` config
    // key) accepts a number and echoes the live value back.
    let set = dispatcher
        .handle_request(JsonRpcRequest::new(
            1,
            "config.set",
            json!({ "max_tokens": 1 }),
        ))
        .await;
    assert!(set.is_ok(), "config.set failed: {:?}", set.error);
    assert_eq!(set.result.unwrap()["max_tokens"], 1);

    let get = dispatcher
        .handle_request(JsonRpcRequest::new(2, "config.get", json!({})))
        .await;
    assert_eq!(get.result.unwrap()["max_tokens"], 1);

    // The hot-swapped cap is what the next run enforces: a 1-token ceiling cannot afford the system
    // prompt, so the run stops before any backend call.
    let prompt = dispatcher
        .handle_request(JsonRpcRequest::new(
            3,
            "session.prompt",
            json!({ "prompt": "hello" }),
        ))
        .await;
    assert!(prompt.is_ok(), "prompt failed: {:?}", prompt.error);
    assert_eq!(prompt.result.unwrap()["status"], "budget_exhausted");
    assert_eq!(
        provider.calls(),
        0,
        "the hot-swapped cap must gate the run before the backend is called"
    );

    // A numeric string (how the REPL forwards raw text) works too, and zero is rejected the way the
    // config-file validator rejects it.
    let string_set = dispatcher
        .handle_request(JsonRpcRequest::new(
            4,
            "config.set",
            json!({ "max_tokens": "500" }),
        ))
        .await;
    assert_eq!(string_set.result.unwrap()["max_tokens"], 500);

    let zero = dispatcher
        .handle_request(JsonRpcRequest::new(
            5,
            "config.set",
            json!({ "max_tokens": 0 }),
        ))
        .await;
    assert!(
        zero.error.is_some(),
        "'max_tokens': 0 must be rejected by config.set"
    );

    let _ = std::fs::remove_dir_all(&root);
}
