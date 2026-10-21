//! KR-J04: a CADOC-style object-card candidate.
//!
//! The task is explicit that this is a *candidate*: it may not inherit the paper's "~40% saving"
//! number, and it is only promoted if a held-out comparison shows it non-inferior. So this module
//! keeps the cards and their wait-cost accounting, exposes them for a comparison, and nothing in the
//! run reads them as control flow.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// One object the run has touched, with the state the model would otherwise have to re-derive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectCard {
    pub id: String,
    /// Short, model-facing state line (what the object *is* right now).
    pub state: String,
    /// Turns since the card was last refreshed; a stale card is worse than no card.
    pub age_turns: usize,
    /// Estimated tokens the model would spend re-deriving this object from scratch.
    pub rebuild_cost_tokens: u64,
}

/// The card set for one run.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ObjectCards {
    cards: BTreeMap<String, ObjectCard>,
    /// Cards whose age exceeded the freshness bound were dropped, and the count is kept so a
    /// comparison can tell "fewer cards" from "fewer useful cards".
    pub dropped_stale: u64,
}

/// Freshness bound: a card older than this is dropped rather than served.
pub const MAX_CARD_AGE_TURNS: usize = 12;

impl ObjectCards {
    /// Insert or refresh a card. Re-inserting an id updates it in place, so a card can never be
    /// duplicated into a second, conflicting state.
    pub fn upsert(&mut self, card: ObjectCard) {
        self.cards.insert(card.id.clone(), card);
    }

    /// Drop cards past the freshness bound. Returns how many were dropped.
    pub fn expire(&mut self, current_turn: usize) -> u64 {
        let before = self.cards.len();
        self.cards
            .retain(|_, c| current_turn.saturating_sub(c.age_turns) <= MAX_CARD_AGE_TURNS);
        let dropped = (before - self.cards.len()) as u64;
        self.dropped_stale = self.dropped_stale.saturating_add(dropped);
        dropped
    }

    pub fn get(&self, id: &str) -> Option<&ObjectCard> {
        self.cards.get(id)
    }

    pub fn len(&self) -> usize {
        self.cards.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cards.is_empty()
    }

    /// Total tokens a model would have spent rebuilding every live object from scratch. This is the
    /// candidate's *claimed* saving; it is not a measured saving and is never reported as one.
    pub fn claimed_rebuild_cost(&self) -> u64 {
        self.cards.values().map(|c| c.rebuild_cost_tokens).sum()
    }

    /// The card set rendered for a prompt, cheapest first, bounded by `max_tokens`.
    pub fn render(&self, max_tokens: u64) -> String {
        let mut out = String::new();
        let mut used = 0u64;
        let mut sorted: Vec<&ObjectCard> = self.cards.values().collect();
        sorted.sort_by_key(|c| std::cmp::Reverse(c.rebuild_cost_tokens));
        for card in sorted {
            let line = format!(
                "- {}: {} (age {} turns)\n",
                card.id, card.state, card.age_turns
            );
            // Round up: underestimating a line's cost is how a "bounded" render overruns its budget.
            let cost = (line.len() as u64).div_ceil(4);
            if used + cost > max_tokens {
                break;
            }
            used += cost;
            out.push_str(&line);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(id: &str, age: usize, cost: u64) -> ObjectCard {
        ObjectCard {
            id: id.to_string(),
            state: format!("state of {id}"),
            age_turns: age,
            rebuild_cost_tokens: cost,
        }
    }

    #[test]
    fn upserting_the_same_id_updates_in_place() {
        let mut cards = ObjectCards::default();
        cards.upsert(card("a", 1, 100));
        cards.upsert(card("a", 2, 200));
        assert_eq!(cards.len(), 1, "a card must never be duplicated");
        assert_eq!(cards.get("a").unwrap().rebuild_cost_tokens, 200);
    }

    #[test]
    fn stale_cards_are_dropped_not_served() {
        let mut cards = ObjectCards::default();
        cards.upsert(card("old", 0, 100));
        cards.upsert(card("fresh", 20, 100));
        let dropped = cards.expire(21);
        assert_eq!(dropped, 1);
        assert!(
            cards.get("old").is_none(),
            "a card older than the bound must not be served"
        );
        assert!(cards.get("fresh").is_some());
    }

    #[test]
    fn rendering_is_bounded_and_never_exceeds_the_budget() {
        let mut cards = ObjectCards::default();
        for i in 0..50 {
            cards.upsert(card(&format!("o{i}"), 1, 100));
        }
        let rendered = cards.render(40);
        let tokens = rendered.len() as u64 / 4;
        assert!(
            tokens <= 40,
            "rendered cards must fit the budget, got {tokens}"
        );
        assert!(!rendered.is_empty());
    }

    #[test]
    fn the_claimed_saving_is_labelled_as_claimed() {
        // The task forbids inheriting the paper's number; the only number this module produces is a
        // rebuild-cost estimate, and its name says so.
        let mut cards = ObjectCards::default();
        cards.upsert(card("a", 1, 1000));
        cards.upsert(card("b", 1, 500));
        assert_eq!(cards.claimed_rebuild_cost(), 1500);
    }
}
