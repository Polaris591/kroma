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
    pub output_nano_usd: u64,
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
            let model = model.filter(|m| !m.trim().is_empty()).ok_or("reported model is unknown")?;
            let rate = self.models.get(model).ok_or("no exact rate for reported model")?;
            let usage = usage.ok_or("provider usage is unknown")?;
            let cached = match usage.cached_input_tokens {
                Some(cached) if cached <= usage.input_tokens => cached,
                Some(_) => return Err("cached tokens exceed total input tokens"),
                None if usage.input_tokens == 0 => 0,
                None if rate.cached_input_nano_usd == rate.uncached_input_nano_usd => 0,
                None => return Err("cache split is unknown and rates differ"),
            };
            // reasoning_tokens are a subset of output for the supported usage schema; do not
            // charge them twice. Provider-specific separate billing needs a different rate schema.
            let input = (usage.input_tokens - cached).checked_mul(rate.uncached_input_nano_usd);
            let cached = cached.checked_mul(rate.cached_input_nano_usd);
            let output = usage.output_tokens.checked_mul(rate.output_nano_usd);
            input.and_then(|a| cached.and_then(|b| a.checked_add(b)))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn table() -> PriceTable {
        PriceTable {
            version: "fixture-v1".into(), source: "fixture-not-market-prices".into(),
            models: BTreeMap::from([("returned-model".into(), TokenRates {
                uncached_input_nano_usd: 1000, cached_input_nano_usd: 100,
                output_nano_usd: 3000,
            })]),
        }
    }

    #[test]
    fn cache_split_and_reasoning_are_not_double_charged() {
        let result = table().estimate(Some("returned-model"), Some(TokenUsage {
            input_tokens: 100, cached_input_tokens: Some(40), output_tokens: 10,
            reasoning_tokens: Some(8),
        }));
        assert_eq!(result.estimated_nano_usd, Some(94000));
        assert_eq!(result.table_version, "fixture-v1");
        assert!(result.unknown_reason.is_none());
    }

    #[test]
    fn absent_identity_usage_or_cache_split_is_not_free() {
        let usage = TokenUsage { input_tokens: 1, ..TokenUsage::default() };
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
        let mut usage = TokenUsage { input_tokens: 1, cached_input_tokens: Some(2),
            ..TokenUsage::default() };
        assert!(table().estimate(Some("returned-model"), Some(usage)).estimated_nano_usd.is_none());
        usage.cached_input_tokens = Some(0);
        usage.output_tokens = u64::MAX;
        assert!(table().estimate(Some("returned-model"), Some(usage)).estimated_nano_usd.is_none());
        assert_eq!(table().estimate(Some("returned-model"), Some(TokenUsage::default())).estimated_nano_usd, Some(0));
    }
}
