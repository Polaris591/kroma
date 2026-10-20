//! T29: constrained offline harness evolution.
//!
//! The rule the plan sets is narrow on purpose: harness changes are proposed offline, evaluated against
//! a **frozen** baseline/verifier/holdout/price table, and promoted only by a human after an independent
//! regression. Nothing here merges, deploys, or edits the security/budget/signing chain.
//!
//! This module is the experiment *bookkeeping* that makes those limits checkable:
//!
//! * [`EvolutionRound`] — the declaration a round must make before it runs: hypothesis, editable
//!   components, cost cap, expected gain, and the criteria that would make it a failure. A round with no
//!   failure criterion is refused, because an experiment that cannot fail cannot teach anything.
//! * [`FrozenCriteria`] — baseline revision, verifier identity, holdout ids and price-table id. A
//!   candidate that changes any of them is rejected outright: adjusting the acceptance criteria is the
//!   one move that makes every later number meaningless.
//! * [`PromotionReview`] — the human decision, with the four conditions made explicit: an accepted
//!   improvement, a gain that does not come from a single task, an independent regression that passed,
//!   and a named owner.
//! * [`RoundLog`] — every round, including the ones that showed no improvement or regressed. A log that
//!   only keeps wins is a brochure, not a record.

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// Components a candidate is allowed to edit. Anything not listed is out of scope by default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EditableComponent {
    /// Prompt wording inside the assembly path.
    Prompt,
    /// Tool descriptions visible to the model.
    ToolDescriptions,
    /// Context projection thresholds (step-down watermarks).
    ProjectionThresholds,
    /// Which tools are pinned versus discovered on demand.
    ToolDiscovery,
}

/// Criteria that must not move between the baseline run and the candidate run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrozenCriteria {
    /// Revision of the baseline the candidate is compared against.
    pub baseline_revision: String,
    /// Identity of the verifier (T19's `frozen_evaluator_identity` digest).
    pub verifier_sha256: String,
    /// Holdout task ids. Never adjusted by a candidate.
    pub holdout: Vec<String>,
    /// Price table id used for cost accounting.
    pub price_table: String,
}

impl FrozenCriteria {
    pub fn digest(&self) -> String {
        let mut holdout = self.holdout.clone();
        holdout.sort();
        let canonical = format!(
            "baseline={}\nverifier={}\nholdout={}\nprice={}\n",
            self.baseline_revision,
            self.verifier_sha256,
            holdout.join(","),
            self.price_table
        );
        crate::prompt::sha256_hex64(canonical.as_bytes())
    }

    /// Difference between the frozen criteria and what a candidate actually used.
    ///
    /// Returns the changed field names. A non-empty list means the comparison is meaningless, so the
    /// caller must refuse the candidate rather than record a number.
    pub fn differences(&self, other: &FrozenCriteria) -> Vec<&'static str> {
        let mut changed = Vec::new();
        if self.baseline_revision != other.baseline_revision {
            changed.push("baseline_revision");
        }
        if self.verifier_sha256 != other.verifier_sha256 {
            changed.push("verifier_sha256");
        }
        let a: BTreeSet<&String> = self.holdout.iter().collect();
        let b: BTreeSet<&String> = other.holdout.iter().collect();
        if a != b {
            changed.push("holdout");
        }
        if self.price_table != other.price_table {
            changed.push("price_table");
        }
        changed
    }
}

/// What a round declares before it runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvolutionRound {
    pub id: String,
    /// The falsifiable statement this round tests.
    pub hypothesis: String,
    /// Components the candidate may touch.
    pub editable: Vec<EditableComponent>,
    /// Token ceiling for the whole round.
    pub cost_cap_tokens: u64,
    /// What improvement would look like, stated before the result is known.
    pub expected_gain: String,
    /// What would make this round a failure. Required: an experiment that cannot fail is not one.
    pub failure_criteria: Vec<String>,
    /// The frozen criteria the candidate must not change.
    pub frozen: FrozenCriteria,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoundError {
    MissingHypothesis,
    MissingFailureCriteria,
    MissingExpectedGain,
    NoEditableComponents,
    ZeroCostCap,
    /// The candidate changed the acceptance criteria.
    AcceptanceCriteriaChanged(Vec<&'static str>),
    /// The candidate edited a component outside its declaration.
    ComponentNotDeclared(String),
    /// A component that is never editable (the judge, the security chain) was touched.
    ForbiddenComponent(String),
}

impl std::fmt::Display for RoundError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RoundError::MissingHypothesis => write!(f, "a round must state a falsifiable hypothesis"),
            RoundError::MissingFailureCriteria => {
                write!(f, "a round must state what would make it a failure")
            }
            RoundError::MissingExpectedGain => write!(f, "a round must state the expected gain"),
            RoundError::NoEditableComponents => write!(f, "a round must declare what it may edit"),
            RoundError::ZeroCostCap => write!(f, "a round must declare a non-zero cost cap"),
            RoundError::AcceptanceCriteriaChanged(fields) => write!(
                f,
                "the candidate changed the acceptance criteria ({}); the comparison is invalid",
                fields.join(", ")
            ),
            RoundError::ComponentNotDeclared(component) => {
                write!(f, "the candidate edited '{}', which this round did not declare", component)
            }
            RoundError::ForbiddenComponent(component) => write!(
                f,
                "'{}' can never be edited by a candidate (judge / security / budget / signing chain)",
                component
            ),
        }
    }
}

impl std::error::Error for RoundError {}

impl EvolutionRound {
    pub fn validate(&self) -> Result<(), RoundError> {
        if self.hypothesis.trim().is_empty() {
            return Err(RoundError::MissingHypothesis);
        }
        if self.failure_criteria.is_empty() {
            return Err(RoundError::MissingFailureCriteria);
        }
        if self.expected_gain.trim().is_empty() {
            return Err(RoundError::MissingExpectedGain);
        }
        if self.editable.is_empty() {
            return Err(RoundError::NoEditableComponents);
        }
        if self.cost_cap_tokens == 0 {
            return Err(RoundError::ZeroCostCap);
        }
        Ok(())
    }

    /// Check a candidate's reported change set against this round's declaration and frozen criteria.
    ///
    /// `changed_components` are the components the candidate actually edited; `observed` is the frozen
    /// criteria the candidate was evaluated under.
    pub fn check_candidate(
        &self,
        changed_components: &[EditableComponent],
        observed: &FrozenCriteria,
    ) -> Result<(), RoundError> {
        self.validate()?;
        let changed = self.frozen.differences(observed);
        if !changed.is_empty() {
            return Err(RoundError::AcceptanceCriteriaChanged(changed));
        }
        for component in changed_components {
            if !self.editable.contains(component) {
                return Err(RoundError::ComponentNotDeclared(format!("{component:?}")));
            }
        }
        Ok(())
    }

    /// Components that may **never** be edited by a candidate, whatever a round declares.
    pub fn is_forbidden_component(name: &str) -> bool {
        let forbidden = [
            "bench", // scorer + statistics
            "completion_gate",
            "ahe",
            "experiment",
            "policy",
            "sandbox",
            "budget",
            "usage_cost",
            "provenance",
            "self_repair_guard",
            "bin/kroma.js", // signing chain
            "tests/",       // trusted tests
        ];
        forbidden.iter().any(|f| name.contains(f))
    }
}

/// The outcome of one round, kept whether it helped or not.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoundOutcome {
    pub round_id: String,
    /// Distinct task ids that improved. A gain from one task is not a harness improvement.
    pub improved_tasks: Vec<String>,
    pub regressed_tasks: Vec<String>,
    /// Whether the change survived an independent regression run.
    pub independent_regression_passed: bool,
    /// Tokens actually spent, compared against the declared cap.
    pub tokens_spent: u64,
}

impl RoundOutcome {
    pub fn net_improvements(&self) -> i64 {
        self.improved_tasks.len() as i64 - self.regressed_tasks.len() as i64
    }
}

/// Why a round was not promoted, so the record says why rather than just "no".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromotionBlock {
    NoImprovement,
    /// Every improvement came from the same single task.
    SingleTaskGain,
    Regression,
    IndependentRegressionNotRun,
    CostCapExceeded,
    OwnerApprovalMissing,
}

impl PromotionBlock {
    pub fn as_str(&self) -> &'static str {
        match self {
            PromotionBlock::NoImprovement => "no_improvement",
            PromotionBlock::SingleTaskGain => "single_task_gain",
            PromotionBlock::Regression => "regression",
            PromotionBlock::IndependentRegressionNotRun => "independent_regression_not_run",
            PromotionBlock::CostCapExceeded => "cost_cap_exceeded",
            PromotionBlock::OwnerApprovalMissing => "owner_approval_missing",
        }
    }
}

/// The promotion decision for one round.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromotionReview {
    /// `true` only when every gate below passed and an owner approved.
    pub promoted: bool,
    /// One label per blocking reason.
    pub blocks: Vec<PromotionBlock>,
    /// The patch is promoted to a *reviewed* change; it is never merged or deployed by this module.
    pub requires_manual_merge: bool,
}

impl PromotionReview {
    pub fn blocks_as_strings(&self) -> Vec<&'static str> {
        self.blocks.iter().map(PromotionBlock::as_str).collect()
    }
}

/// Decide whether a round may be promoted.
///
/// Deliberately conservative and explicit:
/// * no improvement, or any regression at all → blocked;
/// * an improvement that comes from a single task → blocked (that is a task fix, not a harness gain);
/// * no independent regression → blocked;
/// * spending over the declared cap → blocked (the round did not run inside its own limits);
/// * no named owner → blocked, because promotion is a human decision.
///
/// `requires_manual_merge` is always `true` when promoted, and this module never merges.
pub fn review_promotion(
    round: &EvolutionRound,
    outcome: &RoundOutcome,
    owner: Option<&str>,
) -> PromotionReview {
    let mut blocks = Vec::new();
    if outcome.regressed_tasks.iter().any(|t| !t.is_empty()) {
        blocks.push(PromotionBlock::Regression);
    }
    if outcome.improved_tasks.len() < 2 {
        // Zero improvements and single-task gains are different reasons, and the record should say which.
        if outcome.improved_tasks.len() == 1 {
            blocks.push(PromotionBlock::SingleTaskGain);
        } else {
            blocks.push(PromotionBlock::NoImprovement);
        }
    }
    if !outcome.independent_regression_passed {
        blocks.push(PromotionBlock::IndependentRegressionNotRun);
    }
    if outcome.tokens_spent > round.cost_cap_tokens {
        blocks.push(PromotionBlock::CostCapExceeded);
    }
    if owner.map(|o| o.trim().is_empty()).unwrap_or(true) {
        blocks.push(PromotionBlock::OwnerApprovalMissing);
    }
    let promoted = blocks.is_empty();
    PromotionReview {
        promoted,
        blocks,
        requires_manual_merge: promoted,
    }
}

/// Every round, including the ones that failed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoundLog {
    #[serde(default)]
    pub rounds: Vec<RoundOutcome>,
}

impl RoundLog {
    pub fn record(&mut self, outcome: RoundOutcome) {
        self.rounds.push(outcome);
    }

    /// Rounds that produced no improvement or a regression. These must be kept and reviewed; a log that
    /// only lists wins hides the cost of the search.
    pub fn negative_rounds(&self) -> Vec<&RoundOutcome> {
        self.rounds
            .iter()
            .filter(|r| r.improved_tasks.is_empty() || !r.regressed_tasks.is_empty())
            .collect()
    }

    pub fn total_tokens(&self) -> u64 {
        self.rounds.iter().map(|r| r.tokens_spent).sum()
    }
}
