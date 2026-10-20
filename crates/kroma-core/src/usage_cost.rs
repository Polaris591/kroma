//! Versioned, operator-supplied price estimates; never an upstream invoice or account balance.
//! Rates use integer nano-USD per token (equivalently milli-USD per million tokens), avoiding
//! floating-point rounding. Missing usage/identity/rate remains unknown, not a free request.
use crate::messages::TokenUsage;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TokenRates {
    pub uncached_input_nano_usd: u64,
    pub cached_input_nano_usd: u64,
    /// T18 (KR-15): tokens *written into* the prefix cache. Providers charge a premium for these
    /// (Anthropic: 1.25× the base input rate), so folding them into the uncached rate under-charges
    /// every warm-up turn. `None` means the operator table does not say, and a turn that actually
    /// wrote to the cache is then unknown rather than silently priced at the wrong rate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation_input_nano_usd: Option<u64>,
    pub output_nano_usd: u64,
}

/// T18 (KR-15): where the token counts came from. A byte estimate is not a provider measurement, and
/// a cost built on one must never be presented as measured.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageBasis {
    /// The provider reported usage for this attempt.
    Measured,
    /// The host estimated the tokens (no usage in the response). Not a measurement.
    Estimated,
    /// Nothing usable: the attempt failed before usage was known.
    Unknown,
}

impl UsageBasis {
    pub fn as_str(self) -> &'static str {
        match self {
            UsageBasis::Measured => "measured",
            UsageBasis::Estimated => "estimated",
            UsageBasis::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PriceTable {
    pub version: String,
    pub source: String,
    /// Exact provider-reported model identity, not a requested alias guessed to be a checkpoint.
    pub models: BTreeMap<String, TokenRates>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CostEstimate {
    pub table_version: String,
    pub price_source: String,
    pub reported_model: Option<String>,
    pub usage: Option<TokenUsage>,
    /// None is unknown. A measured zero is Some(0), never inferred from a missing response.
    pub estimated_nano_usd: Option<u64>,
    pub unknown_reason: Option<String>,
    pub basis: String,
}

impl PriceTable {
    /// No implicit alias matching, cached-token guessing, or invented default price.
    pub fn estimate(&self, model: Option<&str>, usage: Option<TokenUsage>) -> CostEstimate {
        let mut estimate = CostEstimate {
            table_version: self.version.clone(),
            price_source: self.source.clone(),
            reported_model: model.map(str::to_string),
            usage,
            estimated_nano_usd: None,
            unknown_reason: None,
            basis: "provider-reported usage × operator price table; not an invoice".to_string(),
        };
        let result = (|| -> Result<u64, &str> {
            if self.version.trim().is_empty() || self.source.trim().is_empty() {
                return Err("price table version and source are required");
            }
            let model = model
                .filter(|m| !m.trim().is_empty())
                .ok_or("reported model is unknown")?;
            let rate = self
                .models
                .get(model)
                .ok_or("no exact rate for reported model")?;
            let usage = usage.ok_or("provider usage is unknown")?;
            let cached = match usage.cached_input_tokens {
                Some(cached) if cached <= usage.input_tokens => cached,
                Some(_) => return Err("cached tokens exceed total input tokens"),
                None if usage.input_tokens == 0 => 0,
                None if rate.cached_input_nano_usd == rate.uncached_input_nano_usd => 0,
                None => return Err("cache split is unknown and rates differ"),
            };
            // T18: cache writes are their own line item. A turn that wrote to the cache and a table
            // that does not price writes is unknown — the alternative is charging the write at the
            // read rate (too cheap) or the uncached rate (also wrong).
            if rate.cache_creation_input_nano_usd.is_some()
                && usage.cache_creation_input_tokens.is_none()
            {
                return Err("cache-write usage unknown for a write-priced model");
            }
            let created = usage.cache_creation_input_tokens.unwrap_or(0);
            if created > 0 && rate.cache_creation_input_nano_usd.is_none() {
                return Err("no cache-creation rate for reported model");
            }
            if cached.saturating_add(created) > usage.input_tokens {
                return Err("cached + cache-creation tokens exceed total input tokens");
            }
            // reasoning_tokens are a subset of output for the supported usage schema; do not
            // charge them twice. Provider-specific separate billing needs a different rate schema.
            let input =
                (usage.input_tokens - cached - created).checked_mul(rate.uncached_input_nano_usd);
            let cached = cached.checked_mul(rate.cached_input_nano_usd);
            let created = created.checked_mul(rate.cache_creation_input_nano_usd.unwrap_or(0));
            let output = usage.output_tokens.checked_mul(rate.output_nano_usd);
            input
                .and_then(|a| cached.and_then(|b| a.checked_add(b)))
                .and_then(|a| created.and_then(|b| a.checked_add(b)))
                .and_then(|a| output.and_then(|b| a.checked_add(b)))
                .ok_or("cost arithmetic overflow")
        })();
        match result {
            Ok(cost) => estimate.estimated_nano_usd = Some(cost),
            Err(reason) => estimate.unknown_reason = Some(reason.to_string()),
        }
        estimate
    }
}

/// T18 (KR-15): one transmitted request attempt, priced on its own.
///
/// The earlier shape priced a whole run with one model name, which is wrong the moment a retry lands
/// on a different checkpoint or a child runs a different model: the last reported model would price
/// every token. Here each attempt carries the model *it* reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptCost {
    pub attempt: usize,
    pub requested_model: Option<String>,
    pub reported_model: Option<String>,
    pub basis: UsageBasis,
    /// Attempt number this one retried, when it was a retry.
    pub retry_of: Option<usize>,
    pub estimate: CostEstimate,
}

impl AttemptCost {
    pub fn priced_nano_usd(&self) -> Option<u64> {
        self.estimate.estimated_nano_usd
    }
}

/// T18: the per-attempt cost ledger for one run (parent and children together).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CostLedger {
    attempts: Vec<AttemptCost>,
}

/// The only numbers a report may show, with the incompleteness made explicit.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CostSummary {
    pub attempts: usize,
    /// Attempts with a measured basis and a known price.
    pub measured_attempts: usize,
    /// Attempts priced from a host estimate rather than provider usage.
    pub estimated_attempts: usize,
    /// Attempts that could not be priced at all.
    pub unknown_attempts: usize,
    /// Sum over measured attempts only.
    pub measured_nano_usd: u64,
    /// Sum over estimated attempts only. Never added to `measured_nano_usd`.
    pub estimated_nano_usd: u64,
    /// `true` when any attempt was estimated or unknown: the total is not a complete measured cost.
    pub incomplete: bool,
    pub unknown_reasons: Vec<String>,
    /// Measured spend per reported model, so a mixed-model run cannot be collapsed to one number.
    pub by_model_nano_usd: BTreeMap<String, u64>,
}

impl CostSummary {
    /// A single figure for reports, and the reason it may be understated.
    pub fn total_nano_usd(&self) -> Option<u64> {
        if self.incomplete {
            None
        } else {
            Some(self.measured_nano_usd)
        }
    }
}

impl CostLedger {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn attempts(&self) -> &[AttemptCost] {
        &self.attempts
    }

    /// Price and record one attempt.
    ///
    /// `reported` is the model the provider said it used; `requested` is what the run asked for. The
    /// price always follows `reported` (falling back to `requested` only when the provider said
    /// nothing), because an alias does not determine the rate.
    pub fn record_attempt(
        &mut self,
        requested_model: Option<&str>,
        reported_model: Option<&str>,
        usage: Option<TokenUsage>,
        basis: UsageBasis,
        retry_of: Option<usize>,
        table: &PriceTable,
    ) -> usize {
        let attempt = self.attempts.len();
        let priced_model = reported_model.or(requested_model);
        // An estimated basis is priced against the same table, but the basis stays visible so the
        // summary can keep it out of the measured total.
        let estimate = table.estimate(priced_model, usage);
        self.attempts.push(AttemptCost {
            attempt,
            requested_model: requested_model.map(str::to_string),
            reported_model: reported_model.map(str::to_string),
            basis,
            retry_of,
            estimate,
        });
        attempt
    }

    pub fn summary(&self) -> CostSummary {
        let mut summary = CostSummary {
            attempts: self.attempts.len(),
            ..CostSummary::default()
        };
        for attempt in &self.attempts {
            match (attempt.basis, attempt.priced_nano_usd()) {
                (UsageBasis::Measured, Some(cost)) => {
                    summary.measured_attempts += 1;
                    summary.measured_nano_usd = summary.measured_nano_usd.saturating_add(cost);
                    let model = attempt
                        .reported_model
                        .clone()
                        .or_else(|| attempt.requested_model.clone())
                        .unwrap_or_else(|| "<unknown>".to_string());
                    let slot = summary.by_model_nano_usd.entry(model).or_insert(0);
                    *slot = slot.saturating_add(cost);
                }
                (UsageBasis::Estimated, Some(cost)) => {
                    summary.estimated_attempts += 1;
                    summary.estimated_nano_usd = summary.estimated_nano_usd.saturating_add(cost);
                    summary.incomplete = true;
                    summary.unknown_reasons.push(format!(
                        "attempt {} was priced from a host estimate",
                        attempt.attempt
                    ));
                }
                (_, _) => {
                    summary.unknown_attempts += 1;
                    summary.incomplete = true;
                    summary.unknown_reasons.push(format!(
                        "attempt {} is unpriced: {}",
                        attempt.attempt,
                        attempt
                            .estimate
                            .unknown_reason
                            .clone()
                            .unwrap_or_else(|| "no usage was recorded".to_string())
                    ));
                }
            }
        }
        summary
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table() -> PriceTable {
        PriceTable {
            version: "fixture-v1".into(),
            source: "fixture-not-market-prices".into(),
            models: BTreeMap::from([(
                "returned-model".into(),
                TokenRates {
                    uncached_input_nano_usd: 1000,
                    cached_input_nano_usd: 100,
                    cache_creation_input_nano_usd: None,
                    output_nano_usd: 3000,
                },
            )]),
        }
    }

    #[test]
    fn cache_split_and_reasoning_are_not_double_charged() {
        let result = table().estimate(
            Some("returned-model"),
            Some(TokenUsage {
                input_tokens: 100,
                cached_input_tokens: Some(40),
                output_tokens: 10,
                reasoning_tokens: Some(8),
                cache_creation_input_tokens: None,
            }),
        );
        assert_eq!(result.estimated_nano_usd, Some(94000));
        assert_eq!(result.table_version, "fixture-v1");
        assert!(result.unknown_reason.is_none());
    }

    #[test]
    fn absent_identity_usage_or_cache_split_is_not_free() {
        let usage = TokenUsage {
            input_tokens: 1,
            ..TokenUsage::default()
        };
        for result in [
            table().estimate(None, Some(usage)),
            table().estimate(Some("requested-alias"), Some(usage)),
            table().estimate(Some("returned-model"), None),
            table().estimate(Some("returned-model"), Some(usage)),
        ] {
            assert_eq!(result.estimated_nano_usd, None);
            assert!(result.unknown_reason.is_some());
        }
    }

    #[test]
    fn invalid_cache_or_overflow_stays_unknown_but_measured_zero_is_zero() {
        let mut usage = TokenUsage {
            input_tokens: 1,
            cached_input_tokens: Some(2),
            ..TokenUsage::default()
        };
        assert!(table()
            .estimate(Some("returned-model"), Some(usage))
            .estimated_nano_usd
            .is_none());
        usage.cached_input_tokens = Some(0);
        usage.output_tokens = u64::MAX;
        assert!(table()
            .estimate(Some("returned-model"), Some(usage))
            .estimated_nano_usd
            .is_none());
        assert_eq!(
            table()
                .estimate(Some("returned-model"), Some(TokenUsage::default()))
                .estimated_nano_usd,
            Some(0)
        );
    }
}
