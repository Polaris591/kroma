//! Conservative token-estimation arithmetic shared by the agent and context
//! policy.
//!
//! These helpers are heuristics, not a model tokenizer: serialized request
//! bytes convert at three bytes per token, rounded up, and one complete
//! request estimate carries a single fixed overhead. Callers add their own
//! image, expanded-log and measured-prefix allowances on top; those
//! allowances are deliberately never folded into the base conversion.

/// Fixed overhead added once to a complete request estimate.
const REQUEST_OVERHEAD_TOKENS: usize = 256;

/// Bytes-per-token divisor of the conservative JSON heuristic.
const BYTES_PER_TOKEN: usize = 3;

/// Base conversion from serialized JSON bytes to conservative tokens. The
/// division rounds up so a partial trailing token still counts.
pub(crate) fn bytes_to_tokens(bytes: usize) -> usize {
    bytes.div_ceil(BYTES_PER_TOKEN)
}

/// Complete estimate for one serialized payload: the base conversion plus the
/// single fixed request overhead.
pub(crate) fn serialized_request_tokens(bytes: usize) -> usize {
    bytes_to_tokens(bytes).saturating_add(REQUEST_OVERHEAD_TOKENS)
}

/// Strip the fixed request overhead, for comparisons against a payload-only
/// budget. Saturates so a smaller estimate yields zero rather than wrapping.
pub(crate) fn without_request_overhead(tokens: usize) -> usize {
    tokens.saturating_sub(REQUEST_OVERHEAD_TOKENS)
}

/// Extend a measured prefix with the bounded 25% margin applied to an appended
/// raw suffix, whose JSON is often denser than ordinary prose.
pub(crate) fn with_measured_margin(base: usize) -> usize {
    base.saturating_add(base.div_ceil(4))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_conversion_rounds_up_and_handles_empty_input() {
        assert_eq!(bytes_to_tokens(0), 0);
        assert_eq!(bytes_to_tokens(1), 1);
        assert_eq!(bytes_to_tokens(2), 1);
        assert_eq!(bytes_to_tokens(3), 1);
        assert_eq!(bytes_to_tokens(4), 2);
        assert_eq!(bytes_to_tokens(6), 2);
        assert_eq!(bytes_to_tokens(7), 3);
        assert_eq!(serialized_request_tokens(0), REQUEST_OVERHEAD_TOKENS);
        assert_eq!(serialized_request_tokens(3), REQUEST_OVERHEAD_TOKENS + 1);
        assert_eq!(serialized_request_tokens(4), REQUEST_OVERHEAD_TOKENS + 2);
    }

    #[test]
    fn measured_margin_rounds_the_quarter_up() {
        assert_eq!(with_measured_margin(0), 0);
        assert_eq!(with_measured_margin(1), 2);
        assert_eq!(with_measured_margin(3), 4);
        assert_eq!(with_measured_margin(4), 5);
        assert_eq!(with_measured_margin(8), 10);
        assert_eq!(with_measured_margin(9), 12);
    }

    #[test]
    fn complete_estimate_adds_the_fixed_overhead_exactly_once() {
        // Splitting one payload into exact thirds must not multiply the
        // overhead, and the base conversion stays a single ceiling.
        assert_eq!(
            serialized_request_tokens(6),
            bytes_to_tokens(3)
                .saturating_add(bytes_to_tokens(3))
                .saturating_add(REQUEST_OVERHEAD_TOKENS)
        );
        assert_ne!(
            serialized_request_tokens(6),
            serialized_request_tokens(3).saturating_add(serialized_request_tokens(3))
        );
        assert_eq!(
            without_request_overhead(serialized_request_tokens(3_000)),
            bytes_to_tokens(3_000)
        );
        assert_eq!(without_request_overhead(0), 0);
        assert_eq!(without_request_overhead(REQUEST_OVERHEAD_TOKENS), 0);
    }

    #[test]
    fn near_usize_max_estimates_saturate_instead_of_overflowing() {
        assert_eq!(
            bytes_to_tokens(usize::MAX),
            usize::MAX / BYTES_PER_TOKEN + usize::from(usize::MAX % BYTES_PER_TOKEN != 0)
        );
        // The base conversion leaves room for the fixed overhead; only the
        // margin can saturate at the top of the range.
        assert_eq!(
            serialized_request_tokens(usize::MAX),
            bytes_to_tokens(usize::MAX) + REQUEST_OVERHEAD_TOKENS
        );
        assert_eq!(with_measured_margin(usize::MAX), usize::MAX);
        assert_eq!(with_measured_margin(usize::MAX - 1), usize::MAX);
        assert_eq!(
            without_request_overhead(usize::MAX),
            usize::MAX - REQUEST_OVERHEAD_TOKENS
        );
    }
}
