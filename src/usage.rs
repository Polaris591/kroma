//! Foreground model usage observed from provider responses. Missing provider
//! measurements remain unknown; cached tokens are a subset of input tokens.
use crate::model::Usage;

#[derive(Default, Clone, Debug)]
pub struct UsageMeter {
    pub calls: usize,
    pub input: u64,
    pub output: u64,
    pub cached: u64,
    pub reasoning: u64,
    unknown_input: usize,
    unknown_output: usize,
    unknown_cached: usize,
    unknown_reasoning: usize,
    /// Cache hit rate only uses calls with both input and cached tokens
    /// reported. Never mix an unknown denominator into a measured ratio.
    paired_cache_input: u64,
    paired_cache_hits: u64,
}

/// Compact decimal units for counters, shared by context and API usage.
pub fn compact_tokens(value: u64) -> String {
    if value >= 1_000_000 {
        let scaled = value as f64 / 1_000_000.0;
        if value >= 10_000_000 || value % 1_000_000 == 0 {
            format!("{scaled:.0}M")
        } else {
            format!("{scaled:.1}M")
        }
    } else if value >= 100_000 || (value >= 10_000 && value % 1_000 == 0) {
        format!("{:.0}k", value as f64 / 1_000.0)
    } else if value >= 1_000 {
        format!("{:.1}k", value as f64 / 1_000.0)
    } else {
        value.to_string()
    }
}

impl UsageMeter {
    pub fn record(&mut self, usage: Option<&Usage>) {
        self.calls += 1;
        if let (Some(input), Some(cached)) =
            (usage.and_then(|u| u.input), usage.and_then(|u| u.cached))
        {
            self.paired_cache_input = self.paired_cache_input.saturating_add(input);
            self.paired_cache_hits = self.paired_cache_hits.saturating_add(cached.min(input));
        }
        if let Some(value) = usage.and_then(|u| u.input) {
            self.input = self.input.saturating_add(value);
        } else {
            self.unknown_input += 1;
        }
        if let Some(value) = usage.and_then(|u| u.output) {
            self.output = self.output.saturating_add(value);
        } else {
            self.unknown_output += 1;
        }
        if let Some(value) = usage.and_then(|u| u.cached) {
            self.cached = self.cached.saturating_add(value);
        } else {
            self.unknown_cached += 1;
        }
        if let Some(value) = usage.and_then(|u| u.reasoning) {
            self.reasoning = self.reasoning.saturating_add(value);
        } else {
            self.unknown_reasoning += 1;
        }
    }

    /// Cached input / total input on requests where *both* were reported.
    /// None means not reported (distinct from a real 0% hit rate).
    pub fn cache_hit_rate(&self) -> Option<(u64, u64, bool)> {
        (self.paired_cache_input > 0).then_some((
            self.paired_cache_hits,
            self.paired_cache_input,
            self.unknown_input > 0 || self.unknown_cached > 0,
        ))
    }

    /// Foreground counters since Kroma started (not persisted session totals).
    /// Cache reads are already part of input. The hit-rate denominator uses
    /// only requests with both input and cache usage reported.
    pub fn compact_status(&self, width: usize) -> Option<String> {
        if width == 0 {
            return None;
        }
        let counted = |tokens: u64, unknown: usize| -> String {
            if unknown == 0 {
                compact_tokens(tokens)
            } else if tokens > 0 {
                format!("≥{}", compact_tokens(tokens))
            } else {
                "—".into()
            }
        };
        let input = if self.calls == 0 {
            "—".into()
        } else {
            counted(self.input, self.unknown_input)
        };
        let output = if self.calls == 0 {
            "—".into()
        } else {
            counted(self.output, self.unknown_output)
        };
        let (cache_long, cache_short, rate, uncached) = match self.cache_hit_rate() {
            Some((hits, paired_input, partial)) => {
                let percent = hits as f64 * 100.0 / paired_input as f64;
                let mark = if partial { "~" } else { "" };
                (
                    format!(
                        "{}/{} ({mark}{percent:.0}%)",
                        compact_tokens(hits),
                        compact_tokens(paired_input)
                    ),
                    format!("{} ({mark}{percent:.0}%)", compact_tokens(hits)),
                    format!("{mark}{percent:.0}%"),
                    (!partial).then(|| compact_tokens(paired_input.saturating_sub(hits))),
                )
            }
            None if self.cached > 0 => (
                format!("≥{} (hit —)", compact_tokens(self.cached)),
                format!("≥{} (hit —)", compact_tokens(self.cached)),
                "—".into(),
                None,
            ),
            None => ("—".into(), "—".into(), "—".into(), None),
        };
        let wide = format!(
            "Tokens since launch · In {input} · Out {output} · Cache {cache_long}{}",
            uncached.map_or(String::new(), |value| format!(" · Uncached {value}"))
        );
        let regular = format!("In {input} · Out {output} · Cache {cache_long}");
        let compact = format!("In {input} · Out {output} · Cache {cache_short}");
        let narrow = format!("In {input} · Out {output} · Cache {rate}");
        let minimal = format!("In {input} · Cache {rate}");
        let cache_only = format!("Cache {rate}");
        for choice in [wide, regular, compact, narrow, minimal, cache_only] {
            if choice.chars().count() <= width {
                return Some(choice);
            }
        }
        Some(format!("Cache {rate}").chars().take(width).collect())
    }

    pub fn report(&self) -> String {
        let value = |count: u64, missing: usize| -> String {
            if missing == 0 {
                count.to_string()
            } else {
                format!("{count} measured (+{missing} unknown request(s))")
            }
        };
        let cache_rate = self.cache_hit_rate().map_or(
            "unknown (provider did not report both cache and input tokens)".to_owned(),
            |(cached, input, partial)| {
                format!(
                    "{:.1}% ({} cached / {} input{}); cached tokens are part of input",
                    cached as f64 * 100.0 / input as f64,
                    cached,
                    input,
                    if partial {
                        ", reported requests only"
                    } else {
                        ""
                    }
                )
            },
        );
        format!(
            "Foreground usage (this Kroma process, not historical or APEC):\nModel calls: {}\nInput: {} tokens\nOutput: {} tokens\nReasoning: {} tokens (included in output; when reported)\nCached input: {} tokens (included in input)\nCache hit rate: {}\nAPEC summary usage is reported separately in runtime logs.",
            self.calls,value(self.input,self.unknown_input),
            value(self.output,self.unknown_output),
            value(self.reasoning,self.unknown_reasoning),
            value(self.cached,self.unknown_cached),
            cache_rate,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn missing_tokens_are_not_zero_and_cached_is_not_double_counted() {
        let mut meter = UsageMeter::default();
        meter.record(Some(&Usage {
            input: Some(100),
            output: Some(25),
            cached: Some(80),
            total: None,
            reasoning: Some(8),
        }));
        meter.record(None);
        assert_eq!(meter.calls, 2);
        assert!(meter
            .report()
            .contains("100 measured (+1 unknown request(s))"));
        assert!(meter.report().contains("80 measured"));
        assert!(meter.report().contains("8 measured"));
        assert_eq!(meter.cache_hit_rate(), Some((80, 100, true)));
        assert!(meter
            .compact_status(120)
            .unwrap()
            .contains("Cache 80/100 (~80%)"));
    }

    #[test]
    fn cache_hit_rate_distinguishes_zero_from_unreported_and_uses_paired_calls() {
        let mut meter = UsageMeter::default();
        assert_eq!(meter.cache_hit_rate(), None);
        meter.record(Some(&Usage {
            input: Some(1_000),
            output: Some(90),
            cached: None,
            ..Usage::default()
        }));
        assert_eq!(meter.cache_hit_rate(), None);
        assert!(meter.compact_status(100).unwrap().contains("Cache —"));
        meter.record(Some(&Usage {
            input: Some(1_000),
            output: Some(70),
            cached: Some(0),
            ..Usage::default()
        }));
        assert_eq!(meter.cache_hit_rate(), Some((0, 1_000, true)));
        meter.record(Some(&Usage {
            input: Some(3_000),
            output: Some(150),
            cached: Some(2_000),
            ..Usage::default()
        }));
        assert_eq!(meter.cache_hit_rate(), Some((2_000, 4_000, true)));
        assert!(meter
            .compact_status(120)
            .unwrap()
            .contains("2.0k/4.0k (~50%)"));
        assert_eq!(meter.input, 5_000);
        assert_eq!(meter.output, 310);
        assert!(meter
            .report()
            .contains("50.0% (2000 cached / 4000 input, reported requests only)"));
    }

    #[test]
    fn compact_status_shows_read_uncached_and_not_reported_separately() {
        let mut meter = UsageMeter::default();
        assert!(meter.compact_status(100).unwrap().contains("Cache —"));
        meter.record(Some(&Usage {
            input: Some(8_000),
            output: Some(500),
            cached: Some(6_000),
            ..Usage::default()
        }));
        let wide = meter.compact_status(130).unwrap();
        assert!(wide.contains("Tokens since launch"));
        assert!(wide.contains("Cache 6.0k/8.0k (75%)"));
        assert!(wide.contains("Uncached 2.0k"));
        assert!(meter.compact_status(40).unwrap().contains("75%"));
        let mut zero = UsageMeter::default();
        zero.record(Some(&Usage {
            input: Some(1_000),
            cached: Some(0),
            ..Usage::default()
        }));
        assert!(zero
            .compact_status(130)
            .unwrap()
            .contains("Cache 0/1.0k (0%)"));
        assert!(meter.compact_status(0).is_none());
    }

    #[test]
    fn compact_tokens_formats_million_and_sub_kilounits() {
        assert_eq!(compact_tokens(1_000_000), "1M");
        assert_eq!(compact_tokens(1_600), "1.6k");
        assert_eq!(compact_tokens(12_500), "12.5k");
        assert_eq!(compact_tokens(0), "0");
    }
}
