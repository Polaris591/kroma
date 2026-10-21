//! KR-J03: a shadow economic planner, replacing the fixed "4 attempts / 8 turns" heuristic.
//!
//! The task's acceptance is threefold: the decision must be explainable, a hard window must not be
//! bypassable for economic reasons, and the default may only change once there is evidence. This
//! module therefore produces a **recommendation** (keep or commit, with the reason and a token
//! interval) and never mutates the projector. The hard-window check (`KR-B04`, which runs before
//! dispatch) is upstream of it and stays authoritative.

use serde::{Deserialize, Serialize};

/// What the planner would do with one tool result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EconomyAction {
    /// Leave the bytes inline: they are cheap enough relative to the remaining budget.
    Keep,
    /// Replace the bytes with a stub: the expected saving outweighs the expected re-read cost.
    Commit,
}

/// Why the planner chose that action. Every decision carries one; there is no silent branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EconomyReason {
    /// The item is below the minimum compactable size, so compaction cannot pay.
    TooSmall,
    /// The item is inside the pinned working set (recent turns are likely to be re-read).
    Pinned,
    /// Keeping it would push the run over the hard window; the window always wins.
    HardWindow,
    /// The expected saving exceeds the expected re-read cost by the configured margin.
    PaysToCommit,
    /// The saving is real but too small to be distinguishable from noise.
    Marginal,
}

/// The planner's configuration. Defaults are the shipped heuristic's behaviour, so switching the
/// planner on is not a behaviour change until someone edits these numbers with evidence.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct EconomyConfig {
    /// Items smaller than this are never worth stubbing.
    pub min_bytes: usize,
    /// Recent turns kept inline regardless of size.
    pub pin_turns: usize,
    /// Expected number of future reads of a compacted item (used for the cost estimate).
    pub expected_rereads: f64,
    /// A saving must exceed this multiple of the expected re-read cost to be committed.
    pub margin: f64,
}

impl Default for EconomyConfig {
    fn default() -> Self {
        Self {
            min_bytes: 1024,
            pin_turns: 8,
            expected_rereads: 0.5,
            margin: 1.5,
        }
    }
}

/// One explainable decision.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EconomyDecision {
    pub action: EconomyAction,
    pub reason: EconomyReason,
    /// Estimated tokens saved if committed, and the interval we are willing to claim.
    pub saving_tokens: i64,
    pub interval_low: i64,
    pub interval_high: i64,
    /// False in this build: the decision is advice, the projector is unchanged.
    pub applied: bool,
}

/// Decide, with an explicit reason and an interval, and never apply.
///
/// `turns_since` is how many turns ago the item entered the transcript; `window_remaining` is the
/// bytes left before the hard window (0 means the window is already full, which forces a commit
/// regardless of economics -- that is the "window is not bypassable" rule).
pub fn plan(
    cfg: &EconomyConfig,
    item_bytes: usize,
    turns_since: usize,
    window_remaining: usize,
) -> EconomyDecision {
    let saving = (item_bytes as f64 * 0.9) as i64; // a stub keeps ~10% of the bytes
    let reread_cost = (item_bytes as f64 * cfg.expected_rereads) as i64;
    let net = saving - reread_cost;
    let interval_low = (net as f64 * 0.6) as i64;
    let interval_high = (net as f64 * 1.4) as i64;

    let (action, reason) = if item_bytes > window_remaining {
        // The hard window wins: no economic argument may keep bytes that do not fit.
        (EconomyAction::Commit, EconomyReason::HardWindow)
    } else if item_bytes < cfg.min_bytes {
        (EconomyAction::Keep, EconomyReason::TooSmall)
    } else if turns_since < cfg.pin_turns {
        (EconomyAction::Keep, EconomyReason::Pinned)
    } else if (saving as f64) > (reread_cost as f64 * cfg.margin) {
        (EconomyAction::Commit, EconomyReason::PaysToCommit)
    } else {
        (EconomyAction::Keep, EconomyReason::Marginal)
    };

    EconomyDecision {
        action,
        reason,
        saving_tokens: saving,
        interval_low,
        interval_high,
        applied: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_hard_window_beats_every_economic_argument() {
        // A pinned, tiny item that does not fit the remaining window must still be committed.
        let cfg = EconomyConfig::default();
        let d = plan(&cfg, 500, 0, 100);
        assert_eq!(d.action, EconomyAction::Commit);
        assert_eq!(d.reason, EconomyReason::HardWindow);
    }

    #[test]
    fn a_pinned_item_is_kept_even_when_large() {
        let cfg = EconomyConfig::default();
        let d = plan(&cfg, 100_000, 1, 10_000_000);
        assert_eq!(d.action, EconomyAction::Keep);
        assert_eq!(d.reason, EconomyReason::Pinned);
    }

    #[test]
    fn a_large_old_item_pays_to_commit_and_says_why() {
        let cfg = EconomyConfig::default();
        let d = plan(&cfg, 100_000, 50, 10_000_000);
        assert_eq!(d.action, EconomyAction::Commit);
        assert_eq!(d.reason, EconomyReason::PaysToCommit);
        // The interval bounds the *net* saving (saving minus expected re-read cost), not the gross
        // saving: claiming a range around the gross number would overstate what committing buys.
        let net = d.saving_tokens - (100_000f64 * cfg.expected_rereads) as i64;
        assert!(
            d.interval_low <= net && net <= d.interval_high,
            "interval must contain the net saving"
        );
        assert!(
            !d.applied,
            "the planner must not claim to have applied anything"
        );
    }

    #[test]
    fn a_tiny_item_is_never_worth_stubbing() {
        let cfg = EconomyConfig::default();
        let d = plan(&cfg, 100, 50, 10_000_000);
        assert_eq!(d.action, EconomyAction::Keep);
        assert_eq!(d.reason, EconomyReason::TooSmall);
    }

    #[test]
    fn every_decision_carries_a_reason_and_an_interval() {
        let cfg = EconomyConfig::default();
        for (bytes, turns, window) in [
            (100, 0, 1_000),
            (50_000, 1, 1_000_000),
            (50_000, 50, 1_000_000),
        ] {
            let d = plan(&cfg, bytes, turns, window);
            assert!(
                d.interval_low <= d.interval_high,
                "interval must be ordered"
            );
        }
    }
}
