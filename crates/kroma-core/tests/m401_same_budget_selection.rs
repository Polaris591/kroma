//! M4-01: fixed, random and search compared under the same evaluation allowance.
//!
//! The spec's first clause is the one without evidence before this file: "固定/随机/搜索同预算对照".
//! A search that is not compared against the two controls cannot claim to have searched well, and a
//! comparison that lets one arm spend more is not a comparison.

use kroma_core::evolution::{run_strategy, Candidate, Oracle, ResearchCosts, SelectionStrategy};

const SCREEN_COST: u64 = 1_000;
const EVALUATE_COST: u64 = 5_000;
const BUDGET: u64 = 40_000;

fn candidates(count: u64) -> Vec<Candidate> {
    (0..count)
        .map(|index| Candidate {
            id: format!("cand-{index:02}"),
            config_digest: format!("digest-{index:02}"),
            // Deliberately misleading: a declared score must never be trusted over the oracle.
            score: Some(999 - index),
            cost_micros: 0,
        })
        .collect()
}

/// Quality rises with the index; the cheap proxy correlates with it, which is what a search strategy
/// is allowed to exploit. The oracle is the only thing that decides a score.
fn quality(index: u64) -> u64 {
    index
}

fn run(strategy: SelectionStrategy) -> kroma_core::evolution::StrategyOutcome {
    let pool = candidates(20);
    let score = |candidate: &Candidate| {
        let index: u64 = candidate
            .id
            .trim_start_matches("cand-")
            .parse()
            .unwrap_or(0);
        quality(index)
    };
    run_strategy(
        strategy,
        &pool,
        BUDGET,
        ResearchCosts {
            screen_micros: SCREEN_COST,
            evaluate_micros: EVALUATE_COST,
        },
        Oracle {
            screen: &score,
            evaluate: &score,
        },
        7,
    )
}

/// M4-01-A1: the same allowance produces a real comparison, and search finds at least as good a
/// candidate as the two controls without spending more.
#[test]
fn the_three_strategies_are_compared_under_one_budget() {
    let fixed = run(SelectionStrategy::Fixed);
    let random = run(SelectionStrategy::Random);
    let search = run(SelectionStrategy::Search);

    for outcome in [&fixed, &random, &search] {
        assert!(
            outcome.spent_micros <= BUDGET,
            "{:?} spent {} of {BUDGET}",
            outcome.strategy,
            outcome.spent_micros
        );
        assert!(
            outcome.evaluations > 0,
            "{:?} evaluated nothing",
            outcome.strategy
        );
    }

    assert_eq!(
        search.best_score,
        Some(19),
        "with a proxy that orders the candidates correctly, search must find the best one"
    );
    assert!(
        search.best_score >= fixed.best_score && search.best_score >= random.best_score,
        "search must not lose to the controls it is compared against"
    );
    assert!(
        search.screens > 0,
        "the search arm pays for screening; that is where its advantage comes from"
    );
}

/// M4-01-A2: screening and evaluation are both charged. The reported spend is the sum of what the
/// strategy actually did, so a losing candidate's cost cannot disappear from the total.
#[test]
fn research_cost_counts_screens_and_evaluations() {
    for strategy in [
        SelectionStrategy::Fixed,
        SelectionStrategy::Random,
        SelectionStrategy::Search,
    ] {
        let outcome = run(strategy);
        let expected =
            outcome.screens as u64 * SCREEN_COST + outcome.evaluations as u64 * EVALUATE_COST;
        assert_eq!(
            outcome.spent_micros, expected,
            "{strategy:?}: every screen and every evaluation must be billed"
        );
    }
}

/// M4-01-A3: a candidate cannot grade itself. The declared `score` is ignored and only the oracle's
/// answer counts, so nothing in the pool can rewrite the measurement or the promotion contract.
#[test]
fn a_declared_score_cannot_override_the_oracle() {
    let pool = candidates(3);
    assert_eq!(
        pool[0].score,
        Some(999),
        "the fixture declares an inflated score"
    );
    let zero = |_: &Candidate| 0u64;
    let outcome = run_strategy(
        SelectionStrategy::Fixed,
        &pool,
        BUDGET,
        ResearchCosts {
            screen_micros: SCREEN_COST,
            evaluate_micros: EVALUATE_COST,
        },
        Oracle {
            screen: &zero,
            evaluate: &zero,
        },
        7,
    );
    assert_eq!(
        outcome.best_score,
        Some(0),
        "the oracle said zero for every candidate, so the best is zero regardless of what they claim"
    );
}
