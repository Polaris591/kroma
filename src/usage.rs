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
    /// Last completed foreground model request, separate from process totals.
    last: Option<Usage>,
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
        // A request with no usage must replace the previous request's
        // measurements; never display stale cache hits as "last".
        self.last = Some(usage.cloned().unwrap_or_default());
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

    /// Latest completed request is primary. Process totals, including cold
    /// first turns, are secondary. Neither is the current context size.
    /// Unknown provider fields stay unknown, not silently counted as zero.
    pub fn compact_status(&self, width: usize) -> Option<String> {
        if width == 0 {
            return None;
        }
        let count = |n: Option<u64>| n.map(compact_tokens).unwrap_or_else(|| "—".into());
        let last = self.last.as_ref();
        let input = count(last.and_then(|u| u.input));
        let output = count(last.and_then(|u| u.output));
        let (cache_full, cache_short, rate, fresh) =
            match (last.and_then(|u| u.input), last.and_then(|u| u.cached)) {
                (Some(input), Some(cached)) if input > 0 && cached <= input => {
                    let rate = format!("{:.0}%", cached as f64 * 100.0 / input as f64);
                    (
                        format!(
                            "{}/{} ({rate})",
                            compact_tokens(cached),
                            compact_tokens(input)
                        ),
                        format!("{} ({rate})", compact_tokens(cached)),
                        rate,
                        Some(compact_tokens(input - cached)),
                    )
                }
                (_, Some(cached)) => (
                    format!("{} (hit —)", compact_tokens(cached)),
                    compact_tokens(cached),
                    "—".into(),
                    None,
                ),
                _ => ("—".into(), "—".into(), "—".into(), None),
            };
        let total_count = |n: u64, unknown: usize| {
            if unknown == 0 {
                compact_tokens(n)
            } else if n > 0 {
                format!("≥{}", compact_tokens(n))
            } else {
                "—".into()
            }
        };
        let total_rate =
            self.cache_hit_rate()
                .map_or("—".to_owned(), |(hits, input, partial)| {
                    format!(
                        "{}{:.0}%",
                        if partial { "~" } else { "" },
                        hits as f64 * 100.0 / input as f64
                    )
                });
        let full = format!("Last In {input} · Out {output} · Cache {cache_full}");
        let with_fresh = match fresh {
            Some(fresh) => format!("{full} · Fresh {fresh}"),
            None => full.clone(),
        };
        let total = format!(
            "Total In {} · Out {} · Hit {total_rate}",
            total_count(self.input, self.unknown_input),
            total_count(self.output, self.unknown_output),
        );
        let full_total = if self.calls > 1 {
            format!("{with_fresh} · {total}")
        } else {
            with_fresh.clone()
        };
        let short_total = if self.calls > 1 {
            format!("{full} · Total hit {total_rate}")
        } else {
            full.clone()
        };
        let short = format!("Last In {input} · Out {output} · Cache {cache_short}");
        let narrow = format!("Last In {input} · Out {output} · Cache {rate}");
        let minimal = format!("Last In {input} · Cache {rate}");
        let cache_only = format!("Cache {rate}");
        for choice in [
            full_total,
            short_total,
            with_fresh,
            full,
            short,
            narrow,
            minimal,
            cache_only.clone(),
        ] {
            if choice.chars().count() <= width {
                return Some(choice);
            }
        }
        Some(cache_only.chars().take(width).collect())
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
        let latest = self
            .last
            .as_ref()
            .map_or("no completed requests".to_owned(), |u| {
                let known =
                    |n: Option<u64>| n.map_or("unknown".to_owned(), |value| value.to_string());
                let hit = match (u.input, u.cached) {
                    (Some(input), Some(cached)) if input > 0 && cached <= input => {
                        format!("{:.1}%", cached as f64 * 100.0 / input as f64)
                    }
                    _ => "unknown".into(),
                };
                format!(
                    "in={} out={} cache_read={} hit={hit}",
                    known(u.input),
                    known(u.output),
                    known(u.cached)
                )
            });
        format!(
            "Foreground usage (this Kroma process, not historical or APEC):\nLast request: {latest}\nModel calls: {}\nInput: {} tokens\nOutput: {} tokens\nReasoning: {} tokens (included in output; when reported)\nCached input: {} tokens (included in input)\nCumulative cache hit rate: {}\nAPEC summary usage is reported separately in runtime logs.",
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
        assert!(meter.compact_status(120).unwrap().contains("Cache —"));
        assert!(meter.compact_status(120).unwrap().contains("Hit ~80%"));
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
            .contains("2.0k/3.0k (67%)"));
        assert!(meter.compact_status(130).unwrap().contains("Hit ~50%"));
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
        assert!(wide.contains("Last In 8.0k · Out 500"));
        assert!(wide.contains("Cache 6.0k/8.0k (75%)"));
        assert!(wide.contains("Fresh 2.0k"));
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
    fn latest_request_is_not_confused_with_cumulative_cold_start() {
        let mut meter = UsageMeter::default();
        assert!(meter.compact_status(120).unwrap().contains("Last In —"));
        assert!(meter.compact_status(120).unwrap().contains("Cache —"));
        meter.record(Some(&Usage {
            input: Some(2_000),
            output: Some(100),
            cached: Some(0),
            ..Usage::default()
        }));
        assert!(meter
            .compact_status(130)
            .unwrap()
            .contains("Cache 0/2.0k (0%)"));
        meter.record(Some(&Usage {
            input: Some(4_000),
            output: Some(200),
            cached: Some(3_000),
            ..Usage::default()
        }));
        let footer = meter.compact_status(145).unwrap();
        assert!(footer.contains("Last In 4.0k · Out 200 · Cache 3.0k/4.0k (75%)"));
        assert!(footer.contains("Total In 6.0k · Out 300 · Hit 50%"));
        assert_eq!(meter.cache_hit_rate(), Some((3_000, 6_000, false)));
        meter.record(None);
        let unknown = meter.compact_status(120).unwrap();
        assert!(unknown.contains("Last In — · Out — · Cache —"));
        assert!(unknown.contains("Hit ~50%"));
    }

    #[test]
    fn compact_tokens_formats_million_and_sub_kilounits() {
        assert_eq!(compact_tokens(1_000_000), "1M");
        assert_eq!(compact_tokens(1_600), "1.6k");
        assert_eq!(compact_tokens(12_500), "12.5k");
        assert_eq!(compact_tokens(0), "0");
    }
}
