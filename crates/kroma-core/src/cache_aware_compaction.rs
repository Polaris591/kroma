//! Cache-aware, *opt-in* proactive compaction policy. Never overrides mandatory context budgets.
//!
//! A projection that changes cached prompt bytes may forfeit the discounted prefix on the next
//! call. This policy uses measured prior cached tokens, estimated reclaimable bytes, and a pessimistic
//! rewrite penalty to choose between deferring an optional compaction or pulling it forward.
//! Price-independent token equivalents are a HEURISTIC, not measured billing or cost savings.

/// Lower occupancy bound for a speculative compaction (85% of the hard high watermark).
pub const PROACTIVE_OCCUPANCY_PERCENT: u128 = 85;
/// Demand meaningful potential savings before altering a reusable cached prefix.
pub const MIN_RECLAIMABLE_BYTES: usize = 12 * 1024;
/// Assumed number of future calls to amortize one rewrite, intentionally conservative.
pub const FUTURE_CALLS: u128 = 4;
/// Relative cost of an uncached input token to a cached read; deliberately pessimistic.
pub const UNCACHED_TO_CACHED_RATIO: u128 = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionChoice {
    /// Preserve the stable prefix. May compact later at the mandatory high watermark.
    Defer,
    /// Favor compacting older tool evidence while there is room to retain the recent tail.
    Proactive,
    /// The request is above the configured hard budget; context safety is non-negotiable.
    Mandatory,
}

#[derive(Debug, Clone, Copy)]
pub struct CompactionSignals {
    pub total_request_bytes: usize,
    pub high_watermark_bytes: usize,
    pub raw_tool_bytes: usize,
    /// Only provider-reported cache reads are accepted. `None` means unmeasured, not zero.
    pub measured_cached_input_tokens: Option<u64>,
    pub hard_pressure: bool,
}

pub fn choose_compaction(signals: CompactionSignals) -> CompactionChoice {
    if signals.hard_pressure {
        return CompactionChoice::Mandatory;
    }
    // Avoid speculative writes to a stable prefix without a measured cache baseline.
    let Some(cached) = signals.measured_cached_input_tokens else {
        return CompactionChoice::Defer;
    };
    if signals.high_watermark_bytes == 0 || signals.raw_tool_bytes < MIN_RECLAIMABLE_BYTES {
        return CompactionChoice::Defer;
    }
    if (signals.total_request_bytes as u128) * 100
        < (signals.high_watermark_bytes as u128) * PROACTIVE_OCCUPANCY_PERCENT
    {
        return CompactionChoice::Defer;
    }
    // At most the *existing* large tool output can be reclaimed. Never assume future savings
    // from shrinking task/user/system content, which is authoritative and non-compactable.
    let saved_token_equiv = (signals.raw_tool_bytes as u128 / 4).saturating_mul(FUTURE_CALLS);
    // Pessimistically assume ANY rewrite invalidates all previously observed cached tokens.
    let rewrite_penalty = (cached as u128).saturating_mul(UNCACHED_TO_CACHED_RATIO - 1);
    if saved_token_equiv >= rewrite_penalty.saturating_mul(3) / 2 {
        CompactionChoice::Proactive
    } else {
        CompactionChoice::Defer
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn signals() -> CompactionSignals {
        CompactionSignals {
            total_request_bytes: 90_000,
            high_watermark_bytes: 100_000,
            raw_tool_bytes: 20_000,
            measured_cached_input_tokens: Some(0),
            hard_pressure: false,
        }
    }
    #[test]
    fn no_proactive_without_measured_usage() {
        assert_eq!(
            choose_compaction(CompactionSignals {
                measured_cached_input_tokens: None,
                ..signals()
            }),
            CompactionChoice::Defer
        );
    }
    #[test]
    fn avoids_destroying_expensive_hot_prefix() {
        assert_eq!(
            choose_compaction(CompactionSignals {
                measured_cached_input_tokens: Some(70_000),
                ..signals()
            }),
            CompactionChoice::Defer
        );
    }
    #[test]
    fn cold_prefix_can_compact_before_high_watermark() {
        assert_eq!(choose_compaction(signals()), CompactionChoice::Proactive);
    }
    #[test]
    fn budget_never_sacrificed_for_cache_hit() {
        assert_eq!(
            choose_compaction(CompactionSignals {
                measured_cached_input_tokens: None,
                hard_pressure: true,
                ..signals()
            }),
            CompactionChoice::Mandatory
        );
    }
    #[test]
    fn no_speculation_below_threshold_or_without_tool_bytes() {
        assert_eq!(
            choose_compaction(CompactionSignals {
                total_request_bytes: 80_000,
                ..signals()
            }),
            CompactionChoice::Defer
        );
        assert_eq!(
            choose_compaction(CompactionSignals {
                raw_tool_bytes: 100,
                ..signals()
            }),
            CompactionChoice::Defer
        );
    }
}
