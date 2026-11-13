//! Conservative token-estimation arithmetic shared by the agent and context
//! policy.
//!
//! These helpers are heuristics, not a model tokenizer. Two rates are kept on
//! purpose: content the caller still holds is counted by character class, and a
//! length-only floor of three bytes per token covers callers that already
//! discarded the bytes and any payload that is not valid UTF-8.
//!
//! The calibrated rates come from 43 measured request deltas in real sessions
//! (identical prefix, only appended messages differ): 3.85-4.65 bytes per token
//! for ASCII-heavy JSON (median 3.99) and about 0.88 token per CJK character.
//! The constants below round both towards safety, so the estimate stays above
//! the provider count on every measured payload.

/// Fixed overhead added once to a complete request estimate.
const REQUEST_OVERHEAD_TOKENS: usize = 256;

/// Length-only rate: three bytes per token even when the content is unknown.
const BYTES_PER_TOKEN: usize = 3;

/// Two tokens per seven ASCII bytes (3.5 bytes per token), slightly below the
/// measured 3.99 so dense code and punctuation stay covered.
const ASCII_TOKENS_PER_SEVEN_BYTES: usize = 2;

/// Thirteen tokens per ten CJK characters (1.3 per character), above the fitted
/// 0.88 so a Chinese-heavy payload is never underestimated.
const CJK_TOKENS_PER_TEN_CHARS: usize = 13;

/// Base conversion from serialized JSON bytes to conservative tokens. The
/// division rounds up so a partial trailing token still counts.
pub(crate) fn bytes_to_tokens(bytes: usize) -> usize {
    bytes.div_ceil(BYTES_PER_TOKEN)
}

/// Character-class estimate for a payload the caller still holds. ASCII runs at
/// the calibrated rate, CJK costs about one token per character, and every
/// other non-ASCII character counts as a full token. Falls back to the
/// length-only floor when the bytes are not valid UTF-8.
pub(crate) fn content_tokens(bytes: &[u8]) -> usize {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return bytes_to_tokens(bytes.len());
    };
    let (mut ascii, mut cjk, mut other) = (0usize, 0usize, 0usize);
    for character in text.chars() {
        let code = character as u32;
        if code < 0x80 {
            ascii += 1;
        } else if is_cjk(code) {
            cjk += 1;
        } else {
            other += 1;
        }
    }
    ascii
        .saturating_mul(ASCII_TOKENS_PER_SEVEN_BYTES)
        .div_ceil(7)
        .saturating_add(cjk.saturating_mul(CJK_TOKENS_PER_TEN_CHARS).div_ceil(10))
        .saturating_add(other)
}

fn is_cjk(code: u32) -> bool {
    matches!(
        code,
        0x3040..=0x30ff // kana and CJK punctuation
            | 0x3400..=0x4dbf // CJK extension A
            | 0x4e00..=0x9fff // CJK unified ideographs
            | 0xf900..=0xfaff // compatibility ideographs
            | 0xff00..=0xffef // fullwidth forms
            | 0x20000..=0x3ffff // supplementary ideographic planes
    )
}

/// Complete estimate for one serialized payload: the base conversion plus the
/// single fixed request overhead.
pub(crate) fn serialized_request_tokens(bytes: usize) -> usize {
    bytes_to_tokens(bytes).saturating_add(REQUEST_OVERHEAD_TOKENS)
}

/// Same, for a payload that is still available to classify.
pub(crate) fn serialized_content_tokens(payload: &[u8]) -> usize {
    content_tokens(payload).saturating_add(REQUEST_OVERHEAD_TOKENS)
}

/// Strip the fixed request overhead, for comparisons against a payload-only
/// budget. Saturates so a smaller estimate yields zero rather than wrapping.
pub(crate) fn without_request_overhead(tokens: usize) -> usize {
    tokens.saturating_sub(REQUEST_OVERHEAD_TOKENS)
}

/// Extend a measured prefix with the residual margin applied to an appended raw
/// suffix. Ten percent: the class-aware base already covers dense JSON, so this
/// only absorbs tokenizer differences between models.
pub(crate) fn with_measured_margin(base: usize) -> usize {
    base.saturating_add(base.div_ceil(10))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn length_only_floor_rounds_up_and_handles_empty_input() {
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
    fn ascii_content_uses_the_calibrated_rate() {
        assert_eq!(content_tokens(b""), 0);
        assert_eq!(content_tokens(b"a"), 1);
        assert_eq!(content_tokens(&[b'a'; 7]), 2);
        assert_eq!(content_tokens(&[b'a'; 3_500]), 1_000);
        assert!(content_tokens(&[b'a'; 3_500]) < bytes_to_tokens(3_500));
    }

    #[test]
    fn cjk_costs_about_one_token_per_character() {
        let chinese = "中文上下文估算".repeat(10);
        assert_eq!(chinese.chars().count(), 70);
        assert_eq!(content_tokens(chinese.as_bytes()), 91);
        // Three UTF-8 bytes per character is far below one token per character,
        // so the ASCII rate alone would undercount this badly.
        assert!(content_tokens(chinese.as_bytes()) > bytes_to_tokens(chinese.len()));
    }

    #[test]
    fn invalid_utf8_falls_back_to_the_length_floor() {
        assert_eq!(content_tokens(&[0xff, 0xfe, 0xfd]), bytes_to_tokens(3));
    }

    #[test]
    fn measured_payloads_are_never_underestimated() {
        // (ascii bytes, CJK characters, provider tokens) from real request deltas.
        for (ascii, cjk, provider) in [
            (140_606usize, 9_023usize, 43_622usize),
            (140_437, 9_050, 43_514),
            (145, 9, 42),
        ] {
            let mut text = String::with_capacity(ascii + cjk * 3);
            text.push_str(&"a".repeat(ascii));
            text.push_str(&"中".repeat(cjk));
            let estimate = content_tokens(text.as_bytes());
            assert!(
                estimate >= provider,
                "ascii={ascii} cjk={cjk} estimate={estimate} provider={provider}"
            );
            assert!(
                estimate * 2 <= provider * 3,
                "ascii={ascii} cjk={cjk} estimate={estimate} provider={provider}"
            );
        }
    }

    #[test]
    fn multilingual_and_dense_payload_boundaries_stay_conservative() {
        // Recorded boundaries for payload classes the estimator must not
        // undercount. These are classification regressions, not tokenizer
        // claims: the provider count is still the only real measurement.
        struct Case {
            name: &'static str,
            text: String,
            class: Class,
        }
        #[derive(PartialEq, Eq, Debug)]
        enum Class {
            Ascii,
            Cjk,
            Other,
        }
        let cases = vec![
            Case {
                name: "dense rust source",
                text: "fn main(){let x=vec![1,2,3];for i in &x{if *i>1{panic!(\"{i}\");}}}"
                    .repeat(50),
                class: Class::Ascii,
            },
            Case {
                name: "cjk prose",
                text: "这是中文上下文估算边界样本。".repeat(60),
                class: Class::Cjk,
            },
            Case {
                name: "japanese kana and kanji",
                text: "この文章は日本語の境界サンプルです。".repeat(60),
                class: Class::Cjk,
            },
            Case {
                name: "emoji",
                text: "🐍🚀🧪🔥".repeat(80),
                class: Class::Other,
            },
            Case {
                name: "mixed multilingual",
                text: "deploy 部署 🚀 ok 確認\n".repeat(80),
                class: Class::Ascii,
            },
            Case {
                name: "dense json",
                text: "{\"a\":1,\"b\":[2,3],\"c\":{\"d\":\"e\"}}".repeat(60),
                class: Class::Ascii,
            },
        ];
        for case in cases {
            let bytes = case.text.as_bytes();
            let characters = case.text.chars().count();
            let estimate = content_tokens(bytes);
            assert!(estimate > 0, "{}: estimate must be positive", case.name);
            match case.class {
                Class::Ascii => assert!(
                    estimate < characters,
                    "{}: ASCII runs at about two tokens per seven bytes",
                    case.name
                ),
                // CJK and other non-ASCII characters cost about one token each,
                // which is far above the three-bytes-per-token floor.
                Class::Cjk | Class::Other => assert!(
                    estimate >= characters,
                    "{}: non-ASCII text must never be undercounted (estimate {estimate}, chars {characters})",
                    case.name
                ),
            }
            assert!(
                estimate >= bytes_to_tokens(bytes.len() / 2),
                "{}: the class-aware rate must stay above half the length floor",
                case.name
            );
            // Growth is monotone: appending more text never lowers the estimate.
            let mut longer = case.text.clone();
            longer.push_str(&case.text);
            assert!(content_tokens(longer.as_bytes()) > estimate);
        }
    }

    #[test]
    fn measured_margin_rounds_the_tenth_up() {
        assert_eq!(with_measured_margin(0), 0);
        assert_eq!(with_measured_margin(1), 2);
        assert_eq!(with_measured_margin(3), 4);
        assert_eq!(with_measured_margin(4), 5);
        assert_eq!(with_measured_margin(8), 9);
        assert_eq!(with_measured_margin(9), 10);
        assert_eq!(with_measured_margin(10), 11);
    }

    #[test]
    fn complete_estimate_adds_the_fixed_overhead_exactly_once() {
        let payload = b"aaaaaa";
        assert_eq!(
            serialized_content_tokens(payload),
            content_tokens(payload).saturating_add(REQUEST_OVERHEAD_TOKENS)
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
        assert_eq!(
            serialized_request_tokens(usize::MAX),
            bytes_to_tokens(usize::MAX) + REQUEST_OVERHEAD_TOKENS
        );
        assert_eq!(with_measured_margin(usize::MAX), usize::MAX);
        let huge = vec![b'a'; 1_000_000];
        assert!(content_tokens(&huge) > 0);
    }
}
