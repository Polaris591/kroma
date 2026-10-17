//! T04 (KR-02): the reservation lifecycle for one request, its retries, and the siblings that share
//! the same ledger.
//!
//! The rules under test:
//! 1. A request reserves a *per-request* allowance, not the whole run allowance, so a transient
//!    failure still leaves budget for the retry.
//! 2. A settlement is one of three states — provably not dispatched (refund), transmitted with
//!    reported usage (exact charge), transmitted with unknown consumption (conservative charge that
//!    is never refunded).
//! 3. Parent, children and retries draw from one ledger: nothing is double-counted, nothing leaks,
//!    and when the balance is gone the number of dispatches is zero.
//! 4. Real usage above the reservation is charged and audited, never truncated to fit the budget.

use kroma_core::budget::{
    BudgetConfig, ModelCapabilities, Settlement, TokenBudget, DEFAULT_PER_REQUEST_OUTPUT_TOKENS,
};
use kroma_core::provider::{ProviderError, RetryBudget};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

fn cap_ledger(limit: u64) -> Arc<TokenBudget> {
    Arc::new(TokenBudget::new(Some(limit)))
}

/// T04: a finite run whose first attempt fails transiently must still be able to afford the retry,
/// and the run must report two wire attempts.
#[test]
fn finite_budget_transient_then_success_has_two_wire_attempts() {
    let capabilities = ModelCapabilities::resolve(Some(128_000), None);
    let estimate = 1_500u64;
    let per_request = capabilities.per_request_output_cap(None, Some(40_000));
    assert_eq!(per_request, DEFAULT_PER_REQUEST_OUTPUT_TOKENS);
    let allowance = estimate + per_request;

    // --- the T04 lifecycle: two attempts, the first one's consumption unknown ---
    let ledger = cap_ledger(40_000);
    let wire = Arc::new(AtomicU64::new(0));

    let first = ledger.try_reserve(allowance).expect("first attempt fits");
    wire.fetch_add(1, Ordering::SeqCst);
    // Transmitted, then failed in a way that cannot prove zero consumption.
    let first_record = first.settle_unknown();
    assert_eq!(
        first_record.settlement,
        Settlement::PossiblyDispatchedUnknown
    );
    assert_eq!(first_record.charged, allowance);

    // The retry claims its own allowance from the same ledger — this is what the pre-T04 code could
    // not do, because the first request had reserved the whole remaining run allowance.
    let retry = ledger
        .try_reserve(allowance)
        .expect("a transient failure must leave room for the retry");
    wire.fetch_add(1, Ordering::SeqCst);
    let retry_record = retry.settle_with_usage(1_200, 300);
    assert_eq!(
        retry_record.settlement,
        Settlement::CompletedUsage {
            input: 1_200,
            output: 300
        }
    );
    assert_eq!(retry_record.charged, 1_500);

    assert_eq!(wire.load(Ordering::SeqCst), 2, "two wire attempts");
    let snapshot = ledger.accounting_snapshot();
    assert_eq!(snapshot.spent, allowance + 1_500);
    assert_eq!(snapshot.reserved, 0, "no reservation may leak");
    assert_eq!(snapshot.unknown_tokens, allowance);
    assert!(snapshot.spent <= 40_000, "the cap still holds");

    // --- the defect this replaces: reserving the whole remaining allowance ---
    let wide = cap_ledger(40_000);
    let whole_run = 40_000u64;
    let first = wide.try_reserve(whole_run).expect("fits");
    drop(first); // unknown consumption
    assert!(
        wide.try_reserve(whole_run).is_none(),
        "reserving the whole run allowance up front is exactly what made the retry impossible"
    );
}

/// T04: only a provably pre-dispatch failure refunds; an ambiguous dispatch does not.
#[test]
fn not_dispatched_refunds_but_unknown_dispatched_does_not() {
    let ledger = cap_ledger(10_000);

    let refunded = ledger.try_reserve(5_000).unwrap();
    let record = refunded
        .settle_not_dispatched(0)
        .expect("nothing was transmitted, so the allowance returns");
    assert_eq!(record.settlement, Settlement::NotDispatched);
    assert_eq!(record.charged, 0);
    assert_eq!(ledger.spent(), 0);
    assert_eq!(ledger.reserved(), 0);
    assert_eq!(ledger.remaining(), Some(10_000));

    // One wire attempt was in fact transmitted: the refund is refused and the whole reservation is
    // charged as unknown by `Drop`.
    let ambiguous = ledger.try_reserve(5_000).unwrap();
    let err = ambiguous
        .settle_not_dispatched(1)
        .expect_err("a transmitted request must not be refunded");
    assert!(matches!(
        err,
        kroma_core::budget::SettlementError::AlreadyDispatched { wire_attempts: 1 }
    ));
    let snapshot = ledger.accounting_snapshot();
    assert_eq!(snapshot.spent, 5_000);
    assert_eq!(snapshot.unknown_tokens, 5_000);
    assert_eq!(snapshot.reserved, 0);
    assert_eq!(ledger.remaining(), Some(5_000));
}

/// T04: an explicit "unknown" settlement charges the reservation and is labelled, never presented as
/// measured usage.
#[test]
fn unknown_and_estimated_settlements_keep_their_basis() {
    let ledger = cap_ledger(20_000);

    let unknown = ledger.try_reserve(2_000).unwrap().settle_unknown();
    assert_eq!(unknown.settlement, Settlement::PossiblyDispatchedUnknown);
    assert_eq!(unknown.charged, 2_000);

    let estimated = ledger
        .try_reserve(3_000)
        .unwrap()
        .settle_estimate(2_000, 400);
    assert_eq!(
        estimated.settlement,
        Settlement::Estimated {
            input: 2_000,
            output: 400
        }
    );

    let exact = ledger
        .try_reserve(3_000)
        .unwrap()
        .settle_with_usage(2_500, 100);
    assert_eq!(
        exact.settlement,
        Settlement::CompletedUsage {
            input: 2_500,
            output: 100
        }
    );

    let snapshot = ledger.accounting_snapshot();
    assert_eq!(snapshot.spent, 2_000 + 2_400 + 2_600);
    assert_eq!(snapshot.reserved, 0);
    assert_eq!(snapshot.unknown_tokens, 2_000, "unknown is tracked apart");
    assert_eq!(
        snapshot.estimated_tokens, 2_400,
        "estimates are tracked apart"
    );
    assert_eq!(
        snapshot.settled_tokens + snapshot.unknown_tokens,
        snapshot.spent,
        "every charged token is accounted for exactly once, either as a settlement or as unknown"
    );
    assert_eq!(snapshot.overage_tokens, 0);
}

/// T04: usage above the reservation is charged and reported as overage, never truncated to fit.
#[test]
fn real_usage_above_the_reservation_is_charged_and_audited() {
    let ledger = cap_ledger(10_000);
    let reservation = ledger.try_reserve(2_000).unwrap();
    let record = reservation.settle_with_usage(9_000, 4_000);
    assert_eq!(record.charged, 13_000);
    assert_eq!(record.over_reservation, 11_000);
    assert_eq!(record.budget_overage, 3_000);
    assert_eq!(record.spent_after, 13_000);
    assert_eq!(record.remaining_after, Some(0));

    let snapshot = ledger.accounting_snapshot();
    assert_eq!(
        snapshot.spent, 13_000,
        "the number is not clamped to the limit"
    );
    assert_eq!(snapshot.overage_tokens, 3_000);

    // Nothing left: the next request cannot be dispatched at all.
    assert!(
        ledger.try_reserve(1).is_none(),
        "zero dispatches when nothing remains"
    );
}

/// T04: parent, children and retries share one limit; nothing is double counted or lost.
#[test]
fn parallel_children_and_retry_share_one_limit() {
    const LIMIT: u64 = 30_000;
    const CHILD_ALLOCATION: u64 = 10_000;
    let ledger = cap_ledger(LIMIT);
    let children = 3;

    let handles: Vec<_> = (0..children)
        .map(|idx| {
            let ledger = Arc::clone(&ledger);
            std::thread::spawn(move || {
                let allocation = ledger
                    .try_reserve(CHILD_ALLOCATION)
                    .expect("each child reserves its allocation up front");
                // Each child spends a per-request allowance; the first child fails transiently and
                // retries from its own allocation ledger.
                let child_ledger = Arc::new(TokenBudget::new(Some(CHILD_ALLOCATION)));
                let per_request = 4_000u64;
                let first = child_ledger.try_reserve(per_request).unwrap();
                if idx == 0 {
                    first.settle_unknown();
                    let retry = child_ledger
                        .try_reserve(per_request)
                        .expect("retry fits inside the child allocation");
                    retry.settle_with_usage(1_000, 500);
                } else {
                    first.settle_with_usage(1_000, 500);
                }
                let spent = child_ledger.spent();
                allocation.settle(spent);
                spent
            })
        })
        .collect();

    let mut total = 0u64;
    for handle in handles {
        total += handle.join().unwrap();
        // At every join point the ledger is consistent: committed never exceeds the limit.
        assert!(
            ledger.committed() <= LIMIT,
            "committed {} exceeded the shared limit {LIMIT}",
            ledger.committed()
        );
    }

    let snapshot = ledger.accounting_snapshot();
    assert_eq!(snapshot.reserved, 0, "no child reservation may leak");
    assert_eq!(
        snapshot.spent, total,
        "spend equals the sum of what children settled"
    );
    // The transient child charged its failed attempt as unknown plus its retry's exact usage; the
    // others charged exactly their usage. Nothing is counted twice.
    assert_eq!(snapshot.spent, 4_000 + 1_500 + 2 * 1_500);
    assert!(snapshot.spent <= LIMIT);

    // The balance is genuinely shared, not per-child: it fits exactly two more allocations and then
    // refuses, instead of handing every child its own private ceiling.
    let a = ledger
        .try_reserve(CHILD_ALLOCATION)
        .expect("one more allocation fits");
    let b = ledger
        .try_reserve(CHILD_ALLOCATION)
        .expect("a second allocation fits");
    assert!(
        ledger.try_reserve(CHILD_ALLOCATION).is_none(),
        "a third allocation would exceed the shared limit"
    );
    drop(a);
    drop(b);
    assert_eq!(ledger.reserved(), 0);
}

/// T04: the retry allowance comes from the shared ledger, so a retry competes with siblings for the
/// same balance instead of getting a private line of credit.
#[test]
fn retry_allowance_competes_with_siblings_for_the_same_balance() {
    let ledger = cap_ledger(12_000);
    let retry_budget = RetryBudget {
        ledger: Arc::clone(&ledger),
        per_attempt: 6_000,
    };

    // A sibling takes half the balance first.
    let sibling = ledger.try_reserve(6_000).unwrap();
    let retry = retry_budget.reserve().expect("one retry still fits");
    assert_eq!(ledger.reserved(), 12_000);
    assert!(
        matches!(
            retry_budget.reserve(),
            Err(ProviderError::RetryBudgetExhausted)
        ),
        "a second retry must be refused, not granted out of thin air"
    );

    retry.settle_with_usage(1_000, 100);
    sibling.settle_with_usage(500, 50);
    let snapshot = ledger.accounting_snapshot();
    assert_eq!(snapshot.reserved, 0);
    assert_eq!(snapshot.spent, 1_650);
}

/// T04: a zero per-attempt allowance is refused rather than silently sending a free retry.
#[test]
fn zero_retry_allowance_is_refused() {
    let ledger = cap_ledger(10_000);
    let retry_budget = RetryBudget {
        ledger,
        per_attempt: 0,
    };
    assert!(matches!(
        retry_budget.reserve(),
        Err(ProviderError::RetryBudgetExhausted)
    ));
}

// ---------------------------------------------------------------------------------------------
// The per-request cap, observed through a real run
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn per_request_output_does_not_consume_entire_run_allowance() {
    use kroma_core::agent_loop::{build_default_runtime, AgentRunConfig, AgentRunner};
    use kroma_core::events::EventLogger;
    use kroma_core::messages::{AssistantTurn, FinishReason};
    use kroma_core::provider::{ModelProvider, ProviderCompletion, ProviderError, ProviderRequest};
    use kroma_core::revision::SnapshotStore;
    use kroma_core::session::SessionJournal;
    use kroma_core::tool::policy::{NonInteractiveApprovalHandler, PolicyEngine, PolicyMode};
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::Mutex;

    struct CapturingProvider {
        turns: Mutex<std::collections::VecDeque<AssistantTurn>>,
        seen_output_caps: Arc<Mutex<Vec<Option<u64>>>>,
    }

    impl ModelProvider for CapturingProvider {
        fn complete<'a>(
            &'a self,
            req: ProviderRequest,
            _budget: &'a BudgetConfig,
            _cancel_rx: tokio::sync::watch::Receiver<bool>,
        ) -> Pin<Box<dyn Future<Output = Result<ProviderCompletion, ProviderError>> + Send + 'a>>
        {
            self.seen_output_caps
                .lock()
                .unwrap()
                .push(req.max_output_tokens);
            let turn = self
                .turns
                .lock()
                .unwrap()
                .pop_front()
                .expect("scripted turn");
            Box::pin(async move {
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

    let root = std::env::temp_dir().join(format!("kroma-t04-cap-{}", uuid::Uuid::new_v4()));
    let ws = root.join("ws");
    let state = root.join("state");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(ws.join("lib.rs"), "pub fn a() {}\n").unwrap();

    const RUN_ALLOWANCE: u64 = 200_000;
    let budget = BudgetConfig {
        max_total_tokens: Some(RUN_ALLOWANCE),
        ..Default::default()
    };
    let caps = Arc::new(Mutex::new(Vec::new()));
    let turns: std::collections::VecDeque<AssistantTurn> = vec![
        AssistantTurn {
            content: Some("first".to_string()),
            reasoning_blocks: Vec::new(),
            tool_calls: Vec::new(),
            finish_reason: FinishReason::Stop,
            usage: Some(kroma_core::messages::TokenUsage {
                input_tokens: 900,
                output_tokens: 100,
                reasoning_tokens: None,
                cached_input_tokens: None,
            }),
        },
        AssistantTurn {
            content: Some("second".to_string()),
            reasoning_blocks: Vec::new(),
            tool_calls: Vec::new(),
            finish_reason: FinishReason::Stop,
            usage: Some(kroma_core::messages::TokenUsage {
                input_tokens: 900,
                output_tokens: 100,
                reasoning_tokens: None,
                cached_input_tokens: None,
            }),
        },
    ]
    .into_iter()
    .collect();

    let env = Arc::new(kroma_core::env::LocalHostEnv::new(&ws, state.clone()).unwrap());
    let policy = PolicyEngine::new(
        PolicyMode::ReadOnly,
        Arc::new(NonInteractiveApprovalHandler),
    );
    let runtime = build_default_runtime(env, policy, budget.clone(), None);
    let cfg = AgentRunConfig::new("do one thing", budget);
    let logger = EventLogger::new(cfg.run_id, Some(state.clone()), false).unwrap();
    let journal = SessionJournal::create(state.join("sessions"), &ws).unwrap();
    let (_tx, rx) = tokio::sync::watch::channel(false);
    let (summary, _logger) = AgentRunner::new(
        cfg,
        Arc::new(CapturingProvider {
            turns: Mutex::new(turns),
            seen_output_caps: Arc::clone(&caps),
        }),
        runtime,
        rx,
    )
    .with_session_journal(journal)
    .with_snapshot_store(Arc::new(SnapshotStore::new(root.join("snapshots"))))
    .run(logger)
    .await;

    let caps = caps.lock().unwrap().clone();
    assert!(
        !caps.is_empty(),
        "the run must have sent a request: {:?}",
        summary.terminal_reason
    );
    for cap in &caps {
        let cap = cap.expect("a bounded run must send an explicit per-request output cap");
        assert!(
            cap <= DEFAULT_PER_REQUEST_OUTPUT_TOKENS,
            "a single request claimed {cap} tokens of the {RUN_ALLOWANCE}-token run allowance"
        );
    }

    let _ = std::fs::remove_dir_all(&root);
}
