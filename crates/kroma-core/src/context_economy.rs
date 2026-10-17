//! B21: cache/pruning economics, measured rather than assumed.
//!
//! The card's acceptance line is specific: a comparison must not present a *text reduction* as a
//! *cost reduction*. Those are different claims, and only one of them can be observed without a
//! price table and provider-reported usage. This module therefore:
//!
//!  * records what a run actually spent (uncached input, cached input, output, TTFT, wall time) and
//!    how much prompt text it carried,
//!  * reports every delta separately — tokens, cost, latency, text bytes — instead of collapsing them
//!    into one "saving",
//!  * refuses to claim a saving when the cost is unknown, and says so explicitly when the text shrank
//!    while the cost did not.
//!
//! Nothing here calls a model: it compares recorded numbers, so the accounting can be verified
//! offline. Whether summarization *does* save money in practice is a separate experiment (blocked on
//! budget in the card conclusions).

use crate::messages::TokenUsage;
use crate::usage_cost::PriceTable;
use serde::{Deserialize, Serialize};

/// What one run cost and carried, as reported by the run itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunEconomy {
    /// Arm label (`rules-only`, `rules+summary`, ...).
    pub label: String,
    /// Identity the provider actually returned; required before any cost claim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reported_model: Option<String>,
    pub uncached_input_tokens: u64,
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
    /// Prompt bytes sent (the thing summarization changes).
    pub prompt_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttft_ms: Option<u64>,
    pub wall_ms: u64,
}

impl RunEconomy {
    fn usage(&self) -> TokenUsage {
        TokenUsage {
            input_tokens: self
                .uncached_input_tokens
                .saturating_add(self.cached_input_tokens),
            cached_input_tokens: Some(self.cached_input_tokens),
            cache_creation_input_tokens: None,
            output_tokens: self.output_tokens,
            reasoning_tokens: None,
        }
    }

    /// Estimated cost, or `None` when the model identity or a rate is missing.
    fn cost(&self, table: Option<&PriceTable>) -> Option<u64> {
        table?.estimate(self.reported_model.as_deref(), Some(self.usage())).estimated_nano_usd
    }
}

/// Which claim the numbers actually support.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EconomyVerdict {
    /// Estimated cost fell, and the estimate is complete.
    Cheaper,
    /// Estimated cost did not fall.
    NotCheaper,
    /// The cost cannot be established (no price table, unknown identity, unknown usage): no saving
    /// may be claimed.
    CostUnknown,
    /// The prompt text shrank but the estimated cost did not. Named explicitly because this is the
    /// mistake the acceptance criterion forbids.
    TextShrankCostNotLower,
}

/// A comparison between two recorded runs, every dimension kept separate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EconomyComparison {
    pub baseline: String,
    pub candidate: String,
    /// Candidate − baseline, in tokens. Negative means the candidate used fewer.
    pub token_delta: i64,
    pub cached_input_delta: i64,
    pub output_delta: i64,
    /// Candidate − baseline prompt bytes. Negative means less text was sent.
    pub text_delta_bytes: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_delta_nano_usd: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttft_delta_ms: Option<i64>,
    pub wall_delta_ms: i64,
    pub verdict: EconomyVerdict,
    pub rationale: String,
}

impl EconomyComparison {
    /// `true` only for a claim the numbers support.
    pub fn claims_saving(&self) -> bool {
        self.verdict == EconomyVerdict::Cheaper
    }

    /// One line for a report.
    pub fn summary(&self) -> String {
        let cost = match self.cost_delta_nano_usd {
            Some(delta) => format!("{:+} nano-USD", delta),
            None => "cost unknown".to_string(),
        };
        let ttft = match self.ttft_delta_ms {
            Some(delta) => format!("{:+}ms", delta),
            None => "ttft unknown".to_string(),
        };
        format!(
            "{} vs {}: tokens {:+}, text {:+}B, {}, {}, wall {:+}ms -> {} ({})",
            self.candidate,
            self.baseline,
            self.token_delta,
            self.text_delta_bytes,
            cost,
            ttft,
            self.wall_delta_ms,
            match self.verdict {
                EconomyVerdict::Cheaper => "cheaper",
                EconomyVerdict::NotCheaper => "not cheaper",
                EconomyVerdict::CostUnknown => "cost unknown",
                EconomyVerdict::TextShrankCostNotLower => "text shrank, cost did not fall",
            },
            self.rationale
        )
    }
}

/// Compare two runs. `table` is the operator's price table; without it no cost claim is made.
pub fn compare(
    baseline: &RunEconomy,
    candidate: &RunEconomy,
    table: Option<&PriceTable>,
) -> EconomyComparison {
    let token_delta = delta(
        baseline
            .uncached_input_tokens
            .saturating_add(baseline.cached_input_tokens),
        candidate
            .uncached_input_tokens
            .saturating_add(candidate.cached_input_tokens),
    );
    let cached_input_delta = delta(baseline.cached_input_tokens, candidate.cached_input_tokens);
    let output_delta = delta(baseline.output_tokens, candidate.output_tokens);
    let text_delta_bytes = delta(baseline.prompt_bytes, candidate.prompt_bytes);
    let cost_delta_nano_usd = match (baseline.cost(table), candidate.cost(table)) {
        (Some(before), Some(after)) => Some(delta(before, after)),
        _ => None,
    };
    let ttft_delta_ms = match (baseline.ttft_ms, candidate.ttft_ms) {
        (Some(before), Some(after)) => Some(delta(before, after)),
        _ => None,
    };
    let wall_delta_ms = delta(baseline.wall_ms, candidate.wall_ms);

    let (verdict, rationale) = match cost_delta_nano_usd {
        None => (
            EconomyVerdict::CostUnknown,
            if text_delta_bytes < 0 {
                "the prompt text shrank, but without a complete price estimate no cost saving is claimed"
                    .to_string()
            } else {
                "no complete price estimate is available, so no cost saving is claimed".to_string()
            },
        ),
        Some(cost_delta) if cost_delta < 0 => (
            EconomyVerdict::Cheaper,
            format!("estimated cost fell by {} nano-USD", -cost_delta),
        ),
        Some(_) if text_delta_bytes < 0 => (
            EconomyVerdict::TextShrankCostNotLower,
            "the prompt text shrank but the estimated cost did not fall".to_string(),
        ),
        Some(cost_delta) => (
            EconomyVerdict::NotCheaper,
            format!("estimated cost rose by {} nano-USD", cost_delta),
        ),
    };

    EconomyComparison {
        baseline: baseline.label.clone(),
        candidate: candidate.label.clone(),
        token_delta,
        cached_input_delta,
        output_delta,
        text_delta_bytes,
        cost_delta_nano_usd,
        ttft_delta_ms,
        wall_delta_ms,
        verdict,
        rationale,
    }
}

fn delta(before: u64, after: u64) -> i64 {
    after as i64 - before as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage_cost::{PriceTable, TokenRates};
    use std::collections::BTreeMap;

    fn table() -> PriceTable {
        PriceTable {
            version: "fixture-v1".to_string(),
            source: "operator fixture, not market prices".to_string(),
            models: BTreeMap::from([(
                "returned-model".to_string(),
                TokenRates {
                    uncached_input_nano_usd: 1_000,
                    cached_input_nano_usd: 100,
                    cache_creation_input_nano_usd: None,
                    output_nano_usd: 3_000,
                },
            )]),
        }
    }

    fn run(label: &str, uncached: u64, cached: u64, output: u64, bytes: u64) -> RunEconomy {
        RunEconomy {
            label: label.to_string(),
            reported_model: Some("returned-model".to_string()),
            uncached_input_tokens: uncached,
            cached_input_tokens: cached,
            output_tokens: output,
            prompt_bytes: bytes,
            ttft_ms: Some(120),
            wall_ms: 900,
        }
    }

    /// The acceptance criterion: less text with a higher bill must not be reported as a saving.
    #[test]
    fn a_smaller_prompt_with_a_higher_cost_is_not_a_saving() {
        let baseline = run("rules-only", 1_000, 0, 100, 40_000);
        // Far less text, but the summarizer spent more output tokens than the text saved.
        let candidate = run("rules+summary", 200, 0, 5_000, 4_000);
        let comparison = compare(&baseline, &candidate, Some(&table()));

        assert!(comparison.text_delta_bytes < 0, "the text did shrink");
        assert!(comparison.cost_delta_nano_usd.unwrap() > 0, "the cost rose");
        assert_eq!(comparison.verdict, EconomyVerdict::TextShrankCostNotLower);
        assert!(!comparison.claims_saving());
        assert!(comparison.summary().contains("text shrank"));
    }

    /// A genuine saving is reported as one — including the case where the text barely moves but the
    /// cached share rises (cached input is cheaper).
    #[test]
    fn a_real_cost_reduction_is_reported_as_a_saving() {
        let baseline = run("rules-only", 10_000, 0, 100, 40_000);
        let candidate = run("rules+cache", 1_000, 9_000, 100, 40_000);
        let comparison = compare(&baseline, &candidate, Some(&table()));
        assert_eq!(comparison.verdict, EconomyVerdict::Cheaper);
        assert!(comparison.claims_saving());
        assert!(comparison.cost_delta_nano_usd.unwrap() < 0);
        assert_eq!(comparison.cached_input_delta, 9_000);
    }

    /// Without a price table (or without the returned identity) the honest answer is "unknown", and
    /// the text delta is still reported — as text, not as money.
    #[test]
    fn an_unknown_cost_is_reported_as_unknown_not_as_a_saving() {
        let baseline = run("rules-only", 1_000, 0, 100, 40_000);
        let candidate = run("rules+summary", 500, 0, 100, 20_000);

        let no_table = compare(&baseline, &candidate, None);
        assert_eq!(no_table.verdict, EconomyVerdict::CostUnknown);
        assert!(no_table.cost_delta_nano_usd.is_none());
        assert!(no_table.text_delta_bytes < 0);
        assert!(no_table.rationale.contains("no cost saving is claimed"));
        assert!(!no_table.claims_saving());

        let mut unknown_model = candidate.clone();
        unknown_model.reported_model = None;
        let no_identity = compare(&baseline, &unknown_model, Some(&table()));
        assert_eq!(no_identity.verdict, EconomyVerdict::CostUnknown);
        assert!(!no_identity.claims_saving());
    }

    /// Every dimension is reported separately; latency is never inferred from tokens.
    #[test]
    fn dimensions_are_reported_separately() {
        let baseline = run("rules-only", 1_000, 0, 100, 40_000);
        let mut candidate = run("rules+summary", 500, 0, 100, 20_000);
        candidate.ttft_ms = None; // the provider did not report it
        candidate.wall_ms = 1_400;

        let comparison = compare(&baseline, &candidate, Some(&table()));
        assert!(comparison.ttft_delta_ms.is_none(), "unknown stays unknown");
        assert_eq!(comparison.wall_delta_ms, 500);
        assert_eq!(comparison.token_delta, -500);
        assert_eq!(comparison.output_delta, 0);
        assert!(comparison.summary().contains("ttft unknown"));
    }
}
