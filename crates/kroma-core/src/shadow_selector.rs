//! KR-S02: an offline / shadow strategy selector.
//!
//! The task's rule is that a selector only enters canary when it beats the fixed strategy on a
//! held-out set, and that the simple strategy wins by default otherwise. This module is therefore
//! deliberately **shadow-only**: it reads task features, records what it *would* have chosen, and
//! never changes what the run does. A caller has to opt in to reading the advice.

use serde::{Deserialize, Serialize};

/// Coarse task features the selector is allowed to look at. Kept small on purpose: a selector that
/// needs the model's own output to decide is not a selector, it is a second agent.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskFeatures {
    /// Number of files the task prompt mentions by path.
    pub mentioned_files: usize,
    /// Whether the prompt asks for a plan/design rather than an edit.
    pub planning_task: bool,
    /// Whether the prompt names a test command or a failing test.
    pub has_verification: bool,
}

/// What the shadow selector would have picked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ShadowChoice {
    /// The fixed strategy: what the run actually uses.
    Fixed,
    /// The selector's suggestion (recorded, never applied without a promoted canary).
    Suggested(String),
}

/// One shadow decision, kept for offline comparison against the fixed strategy.
///
/// The three fields the taskbook asks for are all present: what the selector *recommended*, what the
/// run *actually* used, and the selector's estimate of what the other choice would have cost.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShadowRecord {
    pub features: TaskFeatures,
    pub choice: ShadowChoice,
    /// The strategy the run actually used (the fixed one, unless a canary was promoted).
    pub actual: String,
    /// The selector's own estimate of the alternative's cost in tokens; `None` when it has no basis
    /// to estimate, which is the honest answer rather than a fabricated number.
    pub counterfactual_estimate: Option<i64>,
    /// Always false in this build: the record is advice, not control flow.
    pub applied: bool,
}

/// Decide, without applying. The rules are intentionally boring: they exist so the offline
/// comparison has something to compare against, not because they are believed to be better.
pub fn observe(features: TaskFeatures, actual: &str) -> ShadowRecord {
    let choice = if features.planning_task && features.mentioned_files >= 3 {
        ShadowChoice::Suggested("p3".to_string())
    } else if features.mentioned_files >= 5 && features.has_verification {
        ShadowChoice::Suggested("p2".to_string())
    } else {
        ShadowChoice::Fixed
    };
    ShadowRecord {
        features,
        choice,
        actual: actual.to_string(),
        // No basis to price the alternative without running it, so this stays None rather than
        // inventing a delta.
        counterfactual_estimate: None,
        applied: false,
    }
}

/// A selector may only be promoted by a held-out win; this is the gate the taskbook describes.
/// `dev_gain` and `holdout_gain` are relative token deltas (negative = cheaper).
pub fn promotion_verdict(dev_gain: f64, holdout_gain: f64) -> &'static str {
    if holdout_gain < -0.05 && dev_gain < -0.05 {
        "promote"
    } else if holdout_gain > 0.0 {
        "reject: costs more on held-out"
    } else {
        "keep shadow: no held-out net gain"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_shadow_record_never_claims_to_have_applied() {
        let rec = observe(
            TaskFeatures {
                mentioned_files: 6,
                planning_task: true,
                has_verification: true,
            },
            "p2",
        );
        assert_eq!(
            rec.actual, "p2",
            "the record must say what the run actually used"
        );
        assert!(
            !rec.applied,
            "shadow advice must never be reported as applied"
        );
        assert_eq!(rec.choice, ShadowChoice::Suggested("p3".to_string()));
    }

    #[test]
    fn a_small_edit_task_gets_the_fixed_strategy() {
        let rec = observe(
            TaskFeatures {
                mentioned_files: 1,
                planning_task: false,
                has_verification: true,
            },
            "p2",
        );
        assert_eq!(rec.choice, ShadowChoice::Fixed);
    }

    #[test]
    fn promotion_requires_a_held_out_win() {
        assert_eq!(promotion_verdict(-0.10, -0.08), "promote");
        assert!(promotion_verdict(-0.20, 0.02).starts_with("reject"));
        assert!(promotion_verdict(-0.10, -0.01).starts_with("keep shadow"));
    }
}
