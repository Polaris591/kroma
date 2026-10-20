//! T29: a harness change is an experiment, and the experiment must be able to fail.
//!
//! The four gates this pins:
//! * a round declares its hypothesis, editable components, cost cap and failure criteria *before* it runs;
//! * a candidate cannot move the acceptance criteria (baseline, verifier, holdout, prices);
//! * promotion needs an improvement that is not a single-task fix, an independent regression, spending
//!   inside the declared cap, and a named owner — and even then this module only marks it for manual merge;
//! * the log keeps the rounds that produced nothing, because that cost is part of the search.

use kroma_core::harness_evolution::{
    review_promotion, EditableComponent, EvolutionRound, FrozenCriteria, PromotionBlock,
    RoundError, RoundLog, RoundOutcome,
};

fn frozen() -> FrozenCriteria {
    FrozenCriteria {
        baseline_revision: "rev-baseline".to_string(),
        verifier_sha256: "a".repeat(64),
        holdout: vec!["holdout-a".to_string(), "holdout-b".to_string()],
        price_table: "prices-v1".to_string(),
    }
}

fn round() -> EvolutionRound {
    EvolutionRound {
        id: "EXP-06-round-1".to_string(),
        hypothesis: "shorter tool descriptions reduce turns on refactor tasks".to_string(),
        editable: vec![
            EditableComponent::ToolDescriptions,
            EditableComponent::Prompt,
        ],
        cost_cap_tokens: 500_000,
        expected_gain: "fewer turns, same pass rate".to_string(),
        failure_criteria: vec![
            "any holdout task regresses".to_string(),
            "spend exceeds the cap".to_string(),
        ],
        frozen: frozen(),
    }
}

fn outcome(improved: &[&str], regressed: &[&str]) -> RoundOutcome {
    RoundOutcome {
        round_id: "EXP-06-round-1".to_string(),
        improved_tasks: improved.iter().map(|s| s.to_string()).collect(),
        regressed_tasks: regressed.iter().map(|s| s.to_string()).collect(),
        independent_regression_passed: true,
        tokens_spent: 100_000,
    }
}

#[test]
fn a_round_must_declare_hypothesis_components_cap_and_failure_criteria() {
    assert!(round().validate().is_ok());

    let mut no_hypothesis = round();
    no_hypothesis.hypothesis = "  ".to_string();
    assert_eq!(no_hypothesis.validate(), Err(RoundError::MissingHypothesis));

    let mut no_criteria = round();
    no_criteria.failure_criteria.clear();
    assert_eq!(
        no_criteria.validate(),
        Err(RoundError::MissingFailureCriteria)
    );

    let mut no_gain = round();
    no_gain.expected_gain = String::new();
    assert_eq!(no_gain.validate(), Err(RoundError::MissingExpectedGain));

    let mut no_editable = round();
    no_editable.editable.clear();
    assert_eq!(
        no_editable.validate(),
        Err(RoundError::NoEditableComponents)
    );

    let mut no_cap = round();
    no_cap.cost_cap_tokens = 0;
    assert_eq!(no_cap.validate(), Err(RoundError::ZeroCostCap));
}

#[test]
fn a_candidate_cannot_adjust_the_acceptance_criteria() {
    let round = round();

    // Unchanged: accepted.
    round
        .check_candidate(&[EditableComponent::ToolDescriptions], &frozen())
        .expect("an unchanged criteria set is valid");

    // A weaker verifier is the classic move: it makes every later number meaningless.
    let mut weaker = frozen();
    weaker.verifier_sha256 = "b".repeat(64);
    match round.check_candidate(&[EditableComponent::ToolDescriptions], &weaker) {
        Err(RoundError::AcceptanceCriteriaChanged(fields)) => {
            assert_eq!(fields, vec!["verifier_sha256"])
        }
        other => panic!("expected an acceptance-criteria refusal, got {other:?}"),
    }

    // Dropping a holdout task is the same move in a different disguise.
    let mut smaller_holdout = frozen();
    smaller_holdout.holdout.pop();
    match round.check_candidate(&[], &smaller_holdout) {
        Err(RoundError::AcceptanceCriteriaChanged(fields)) => assert_eq!(fields, vec!["holdout"]),
        other => panic!("expected a holdout refusal, got {other:?}"),
    }

    // Re-pricing changes the cost story.
    let mut cheaper = frozen();
    cheaper.price_table = "prices-discount".to_string();
    match round.check_candidate(&[], &cheaper) {
        Err(RoundError::AcceptanceCriteriaChanged(fields)) => {
            assert_eq!(fields, vec!["price_table"])
        }
        other => panic!("expected a price-table refusal, got {other:?}"),
    }

    // A different baseline is a different comparison.
    let mut other_baseline = frozen();
    other_baseline.baseline_revision = "rev-other".to_string();
    assert!(round.check_candidate(&[], &other_baseline).is_err());

    // The frozen digest is stable and order-insensitive over the holdout list.
    let mut reordered = frozen();
    reordered.holdout.reverse();
    assert_eq!(frozen().digest(), reordered.digest());
    assert_ne!(frozen().digest(), weaker.digest());
}

#[test]
fn a_candidate_cannot_edit_undeclared_or_forbidden_components() {
    let round = round();
    // Editing a component the round did not declare.
    match round.check_candidate(&[EditableComponent::ProjectionThresholds], &frozen()) {
        Err(RoundError::ComponentNotDeclared(name)) => {
            assert!(name.contains("ProjectionThresholds"))
        }
        other => panic!("expected ComponentNotDeclared, got {other:?}"),
    }

    // The judge and the security chain are never editable by any round.
    for forbidden in [
        "crates/kroma-core/src/bench.rs",
        "crates/kroma-core/src/completion_gate.rs",
        "crates/kroma-core/src/ahe.rs",
        "crates/kroma-core/src/experiment.rs",
        "crates/kroma-core/src/tool/policy.rs",
        "crates/kroma-core/src/sandbox.rs",
        "crates/kroma-core/src/usage_cost.rs",
        "bin/kroma.js",
        "crates/kroma-core/tests/gate5_controlled_subagents.rs",
    ] {
        assert!(
            EvolutionRound::is_forbidden_component(forbidden),
            "{forbidden} must be forbidden for every round"
        );
    }
    assert!(!EvolutionRound::is_forbidden_component(
        "crates/kroma-core/src/prompt.rs"
    ));
}

#[test]
fn promotion_needs_independent_regression_trustworthy_gain_and_owner() {
    let round = round();

    // The full pass: two tasks improved, nothing regressed, regression run, inside the cap, owner named.
    let good = review_promotion(&round, &outcome(&["task-a", "task-b"], &[]), Some("owner"));
    assert!(good.promoted, "blocks: {:?}", good.blocks_as_strings());
    assert!(
        good.requires_manual_merge,
        "a promoted round still needs a human merge; this module never merges"
    );

    // No improvement.
    let none = review_promotion(&round, &outcome(&[], &[]), Some("owner"));
    assert!(!none.promoted);
    assert_eq!(none.blocks_as_strings(), vec!["no_improvement"]);

    // A gain from a single task is a task fix, not a harness improvement.
    let single = review_promotion(&round, &outcome(&["task-a"], &[]), Some("owner"));
    assert!(!single.promoted);
    assert_eq!(single.blocks_as_strings(), vec!["single_task_gain"]);

    // Any regression blocks, whatever else is true.
    let regressed = review_promotion(
        &round,
        &outcome(&["task-a", "task-b"], &["task-c"]),
        Some("owner"),
    );
    assert!(!regressed.promoted);
    assert!(regressed.blocks.contains(&PromotionBlock::Regression));

    // No independent regression.
    let mut no_regression_run = outcome(&["task-a", "task-b"], &[]);
    no_regression_run.independent_regression_passed = false;
    let unverified = review_promotion(&round, &no_regression_run, Some("owner"));
    assert_eq!(
        unverified.blocks_as_strings(),
        vec!["independent_regression_not_run"]
    );

    // Over the declared cap: the round did not run inside its own limits.
    let mut expensive = outcome(&["task-a", "task-b"], &[]);
    expensive.tokens_spent = round.cost_cap_tokens + 1;
    assert_eq!(
        review_promotion(&round, &expensive, Some("owner")).blocks_as_strings(),
        vec!["cost_cap_exceeded"]
    );

    // No owner: promotion is a human decision.
    assert_eq!(
        review_promotion(&round, &outcome(&["task-a", "task-b"], &[]), None).blocks_as_strings(),
        vec!["owner_approval_missing"]
    );
    assert_eq!(
        review_promotion(&round, &outcome(&["task-a", "task-b"], &[]), Some("  "))
            .blocks_as_strings(),
        vec!["owner_approval_missing"]
    );
}

#[test]
fn the_round_log_keeps_the_rounds_that_produced_nothing() {
    let mut log = RoundLog::default();
    log.record(outcome(&["task-a", "task-b"], &[])); // win
    log.record(outcome(&[], &[])); // nothing
    log.record(outcome(&["task-c"], &["task-d"])); // regression

    assert_eq!(log.rounds.len(), 3);
    assert_eq!(log.total_tokens(), 300_000);
    let negative = log.negative_rounds();
    assert_eq!(negative.len(), 2, "no-improvement and regression both stay");
    assert!(negative.iter().any(|r| r.improved_tasks.is_empty()));
    assert!(negative.iter().any(|r| !r.regressed_tasks.is_empty()));
    // The net improvement of the regressing round is visible, not hidden.
    assert_eq!(log.rounds[2].net_improvements(), 0);
}
