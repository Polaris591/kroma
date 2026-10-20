//! M3-02: `ctx.taskstate.v1` — project context around the task state, not the transcript.
//!
//! The candidate is a *stage* over `P2`, not a new profile: it renders the derived [`TaskState`]
//! (goal, constraints, verification items, facts) into one bounded block, so a run can carry the
//! task itself instead of replaying the exploration that produced it. The card's three acceptance
//! lines are enforced here rather than promised in prose:
//!
//! * a fact whose binding is not [`Freshness::Fresh`] is **dropped**, never softened — replaying a
//!   stale fact as context hands the run a constraint that no longer holds (C00-07);
//! * a sensitive fact contributes its **key only**; the value never reaches the block;
//! * the block reports the tokens it costs, so the caller charges the **same run's** ledger. A
//!   projection measured against a different budget is not a cheaper projection.

use crate::evidence_lifecycle::Freshness;
use crate::task_state::TaskState;
use std::collections::{BTreeMap, BTreeSet};

/// The id an operator selects this candidate by. It never replaces a `p*` name.
pub const CANDIDATE_ID: &str = "ctx.taskstate.v1";

/// One rendered task-state block, with what it cost and what it refused to carry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectedTaskState {
    pub text: String,
    /// Tokens this block adds to the request, so the caller can charge the run it belongs to.
    pub charged_tokens: u64,
    pub kept_facts: usize,
    /// Facts dropped because their binding was not fresh.
    pub dropped_facts: usize,
    /// Facts rendered as a key with `[redacted]` instead of their value.
    pub redacted_facts: usize,
}

/// Render the task state, bounded by `max_bytes`.
pub fn project(
    state: &TaskState,
    freshness: &BTreeMap<String, Freshness>,
    sensitive: &BTreeSet<String>,
    max_bytes: usize,
) -> ProjectedTaskState {
    let mut lines: Vec<String> = Vec::new();
    if let Some(goal) = &state.goal {
        lines.push(format!("[task] {}", goal.quote.trim()));
    }
    for constraint in &state.constraints {
        lines.push(format!("[constraint] {}", constraint.statement.trim()));
    }
    for item in &state.verification {
        lines.push(format!("[verify] {}", item.statement.trim()));
    }

    let mut kept_facts = 0;
    let mut dropped_facts = 0;
    let mut redacted_facts = 0;
    for (key, binding) in &state.facts {
        // A fact with no recorded observation is not fresh; it is unknown, and unknown is not
        // something a run may act on.
        let fresh = matches!(freshness.get(key), Some(Freshness::Fresh));
        if !fresh {
            dropped_facts += 1;
            continue;
        }
        kept_facts += 1;
        if sensitive.contains(key) {
            redacted_facts += 1;
            lines.push(format!("[fact] {}: [redacted]", key));
        } else {
            lines.push(format!("[fact] {}: {}", key, binding.fact.trim()));
        }
    }

    // Bounded: drop trailing lines rather than truncate one mid-sentence, so the block never ends
    // in a half-true statement.
    let mut text = String::new();
    for line in lines {
        let added = line.len() + 1;
        if text.len() + added > max_bytes {
            break;
        }
        text.push_str(&line);
        text.push('\n');
    }

    ProjectedTaskState {
        charged_tokens: text.len().div_ceil(4) as u64,
        text,
        kept_facts,
        dropped_facts,
        redacted_facts,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evidence_lifecycle::FactBinding;
    use crate::task_state::{Constraint, Provenance};

    fn provenance(quote: &str) -> Provenance {
        Provenance {
            canonical_index: 0,
            entry_id: None,
            quote: quote.to_string(),
        }
    }

    fn state_with_facts(keys: &[(&str, &str)]) -> TaskState {
        let mut state = TaskState {
            goal: Some(provenance("make the parser reject an empty name")),
            constraints: vec![Constraint {
                statement: "do not modify tests/".to_string(),
                provenance: provenance("don't touch the tests"),
            }],
            ..TaskState::default()
        };
        for (key, fact) in keys {
            state
                .facts
                .insert((*key).to_string(), FactBinding::new(*fact));
        }
        state
    }

    #[test]
    fn stale_and_missing_facts_never_reach_the_block() {
        let state = state_with_facts(&[("port", "8080"), ("flag", "enabled")]);
        let freshness = BTreeMap::from([
            ("port".to_string(), Freshness::Fresh),
            (
                "flag".to_string(),
                Freshness::Stale {
                    changed: vec!["src/main.rs".to_string()],
                },
            ),
        ]);
        let out = project(&state, &freshness, &BTreeSet::new(), 4096);
        assert!(out.text.contains("8080"));
        assert!(
            !out.text.contains("enabled"),
            "a stale fact must not be replayed"
        );
        assert_eq!(out.kept_facts, 1);
        assert_eq!(out.dropped_facts, 1);

        // A fact with no observation at all is dropped too: unknown is not fresh.
        let unobserved = state_with_facts(&[("port", "8080")]);
        let out = project(&unobserved, &BTreeMap::new(), &BTreeSet::new(), 4096);
        assert_eq!(out.kept_facts, 0);
        assert_eq!(out.dropped_facts, 1);
    }

    #[test]
    fn a_sensitive_fact_contributes_its_key_and_nothing_else() {
        let state = state_with_facts(&[("token", "sk-live-1234567890")]);
        let freshness = BTreeMap::from([("token".to_string(), Freshness::Fresh)]);
        let sensitive = BTreeSet::from(["token".to_string()]);
        let out = project(&state, &freshness, &sensitive, 4096);
        assert!(out.text.contains("token: [redacted]"));
        assert!(
            !out.text.contains("sk-live"),
            "the value must not be rendered"
        );
        assert_eq!(out.redacted_facts, 1);
    }

    #[test]
    fn the_block_is_bounded_and_reports_what_it_costs() {
        let state = state_with_facts(&[("port", "8080")]);
        let freshness = BTreeMap::from([("port".to_string(), Freshness::Fresh)]);
        let full = project(&state, &freshness, &BTreeSet::new(), 4096);
        assert_eq!(full.charged_tokens, full.text.len().div_ceil(4) as u64);
        assert!(full.text.contains("[task]"));
        assert!(full.text.contains("[constraint]"));
        // Same input, same block: the projection cannot vary run to run.
        assert_eq!(full, project(&state, &freshness, &BTreeSet::new(), 4096));

        let tiny = project(&state, &freshness, &BTreeSet::new(), 12);
        assert!(tiny.text.len() <= 12);
        assert!(tiny.charged_tokens < full.charged_tokens);
    }
}
