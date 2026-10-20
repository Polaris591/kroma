//! M1-05: the ledger contract, exercised through the existing `TokenBudget`.
//!
//! The states (`planned -> reserved -> settled_exact / settled_estimated / outcome_unknown`) and
//! the API already exist in `kroma_core::budget`; this file pins the three properties the
//! blueprint requires of them, so a future change to the ledger cannot quietly start treating a
//! failed call as free, a retry as the same attempt, or an unknown settlement as a refund.

use std::sync::Arc;

use kroma_core::budget::{Settlement, TokenBudget};

/// M1-05-A1: a call that was possibly transmitted and then failed keeps its cost.
#[test]
fn a_failed_call_is_not_erased() {
    let budget = Arc::new(TokenBudget::new(Some(10_000)));

    // Provably never sent: the allowance comes back.
    let never_sent = budget.try_reserve(1_000).expect("reserve");
    let record = never_sent
        .settle_not_dispatched(0)
        .expect("never dispatched");
    assert_eq!(record.settlement, Settlement::NotDispatched);
    assert_eq!(
        budget.spent(),
        0,
        "a request that never left must not be charged"
    );

    // Possibly sent (timeout / cancellation / dropped connection): the whole reservation is kept.
    let maybe_sent = budget.try_reserve(2_000).expect("reserve");
    let record = maybe_sent.settle_unknown();
    assert_eq!(record.settlement, Settlement::PossiblyDispatchedUnknown);
    assert_eq!(record.charged, 2_000);
    let snapshot = budget.accounting_snapshot();
    assert_eq!(snapshot.spent, 2_000, "the failed attempt is still spent");
    assert_eq!(
        snapshot.unknown_tokens, 2_000,
        "and it is recorded as unknown, not exact"
    );
    assert_eq!(snapshot.reserved, 0);
}

/// M1-05-A2: a retry is a new attempt with its own reservation; it does not reuse the first one.
#[test]
fn a_retry_is_a_new_attempt() {
    let budget = Arc::new(TokenBudget::new(Some(10_000)));

    let first = budget.try_reserve(1_500).expect("first attempt reserves");
    first.settle_unknown();

    let second = budget
        .try_reserve(1_500)
        .expect("the retry reserves its own allowance");
    let record = second.settle_estimate(900, 100);
    assert_eq!(
        record.settlement,
        Settlement::Estimated {
            input: 900,
            output: 100
        }
    );

    let snapshot = budget.accounting_snapshot();
    assert_eq!(
        snapshot.spent, 2_500,
        "both attempts are charged; a retry does not replace the failed attempt's cost"
    );
    assert_eq!(
        snapshot.unknown_tokens, 1_500,
        "only the first attempt is unknown"
    );
    assert_eq!(snapshot.estimated_tokens, 1_000);
}

/// M1-05-A3: an unknown settlement keeps an upper bound, and the budget refuses to hand out the
/// allowance that liability already consumed.
#[test]
fn unknown_settlement_keeps_an_upper_bound() {
    let budget = Arc::new(TokenBudget::new(Some(5_000)));
    let attempt = budget.try_reserve(4_000).expect("reserve");
    attempt.settle_unknown();

    assert_eq!(
        budget.remaining(),
        Some(1_000),
        "the liability is deducted, not written off"
    );
    assert!(
        budget.try_reserve(2_000).is_none(),
        "a reservation beyond the remaining bound must be refused"
    );

    // The exact remaining bound is available, but it has to be settled deliberately: a reservation
    // that is simply dropped is charged as unknown, which is what the next step shows.
    let last = budget
        .try_reserve(1_000)
        .expect("the exact remaining bound fits");
    assert_eq!(budget.remaining(), Some(0));
    let snapshot = budget.accounting_snapshot();
    assert_eq!(
        snapshot.reserved, 1_000,
        "a live reservation is visible separately"
    );
    assert_eq!(
        snapshot.unknown_tokens, 4_000,
        "and it does not rewrite settled liability"
    );

    drop(last);
    let after_drop = budget.accounting_snapshot();
    assert_eq!(after_drop.reserved, 0);
    assert_eq!(
        after_drop.unknown_tokens, 5_000,
        "dropping an un-settled reservation charges it as unknown: silence is not a refund"
    );
    assert_eq!(after_drop.spent, 5_000);
    assert_eq!(
        after_drop.limit,
        Some(5_000),
        "the limit itself is not consumed"
    );
}
