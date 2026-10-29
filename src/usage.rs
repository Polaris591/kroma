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
    /// The last measured request, including missing provider fields.
    pub(crate) fn last_request(&self) -> Option<&Usage> {
        self.last.as_ref()
    }

    pub fn record(&mut self, usage: Option<&Usage>) {
        self.record_batch(1, usage);
    }

    /// Summaries can be aggregates of several provider attempts. Their token
    /// fields are combined measurements, not one artificial model request.
    pub fn record_batch(&mut self, requests: usize, usage: Option<&Usage>) {
        if requests == 0 {
            return;
        }
        self.calls = self.calls.saturating_add(requests);
        // A missing measurement must never leave a stale "last" value.
        self.last = (requests == 1).then(|| usage.cloned().unwrap_or_default());
        if let (Some(input), Some(cached)) =
            (usage.and_then(|u| u.input), usage.and_then(|u| u.cached))
        {
            self.paired_cache_input = self.paired_cache_input.saturating_add(input);
            self.paired_cache_hits = self.paired_cache_hits.saturating_add(cached.min(input));
        }
        if let Some(value) = usage.and_then(|u| u.input) {
            self.input = self.input.saturating_add(value);
        } else {
            self.unknown_input += requests;
        }
        if let Some(value) = usage.and_then(|u| u.output) {
            self.output = self.output.saturating_add(value);
        } else {
            self.unknown_output += requests;
        }
        if let Some(value) = usage.and_then(|u| u.cached) {
            self.cached = self.cached.saturating_add(value);
        } else {
            self.unknown_cached += requests;
        }
        if let Some(value) = usage.and_then(|u| u.reasoning) {
            self.reasoning = self.reasoning.saturating_add(value);
        } else {
            self.unknown_reasoning += requests;
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

    /// Do not display a rounded 100% unless upstream actually reported
    /// every input token as cached. A 99.96% reading is still below 100%.
    pub(crate) fn cache_percent(cached: u64, input: u64) -> String {
        if input == 0 {
            return "—".into();
        }
        if cached >= input {
            return "100%".into();
        }
        let rate = cached as f64 * 100.0 / input as f64;
        if rate >= 99.95 {
            "<100%".into()
        } else if (rate - rate.round()).abs() < 0.00001 {
            format!("{rate:.0}%")
        } else {
            format!("{rate:.1}%")
        }
    }

    /// The quiet footer describes only the latest foreground coding request.
    /// Title generation and APEC are separate expenses, visible in /usage.
    /// Missing cache fields are unknown; a reported zero is exactly 0%.
    pub fn compact_status(&self, width: usize) -> Option<String> {
        if width == 0 {
            return None;
        }
        let last = self.last.as_ref();
        let count = |n: Option<u64>| n.map(compact_tokens).unwrap_or_else(|| "—".into());
        let input = count(last.and_then(|u| u.input));
        let output = count(last.and_then(|u| u.output));
        let (rate, detail) = match (last.and_then(|u| u.input), last.and_then(|u| u.cached)) {
            (Some(input), Some(cached)) if input > 0 && cached <= input => (
                Self::cache_percent(cached, input),
                Some(format!(
                    "{}/{}",
                    compact_tokens(cached),
                    compact_tokens(input)
                )),
            ),
            _ => ("—".into(), None),
        };
        let normal = format!("Last In {input} · Out {output} · Cache {rate}");
        let detailed = detail.map_or(normal.clone(), |detail| format!("{normal} ({detail})"));
        let narrow = format!("In {input} · Out {output} · Cache {rate}");
        let minimal = format!("In {input} · Cache {rate}");
        let cache_only = format!("Cache {rate}");
        for variant in [detailed, normal, narrow, minimal, cache_only.clone()] {
            if variant.chars().count() <= width {
                return Some(variant);
            }
        }
        Some(cache_only.chars().take(width).collect())
    }

    pub(crate) fn summary_line(&self, label: &str) -> String {
        let measured = |value: u64, unknown: usize| {
            if self.calls == 0 {
                "—".to_owned()
            } else if unknown == 0 {
                value.to_string()
            } else if value > 0 {
                format!("≥{value}")
            } else {
                "—".to_owned()
            }
        };
        let cache_rate =
            self.cache_hit_rate()
                .map_or("—".to_owned(), |(cached, input, partial)| {
                    let percent =
                        if cached < input && (cached as f64 * 100.0 / input as f64) >= 99.95 {
                            "<100%".to_owned()
                        } else {
                            format!("{:.1}%", cached as f64 * 100.0 / input as f64)
                        };
                    format!("{}{percent}", if partial { "~" } else { "" })
                });
        format!(
            "{label}: {} call(s) | In {} | Out {} | Cache read {} | Hit {} | Reasoning {}",
            self.calls,
            measured(self.input, self.unknown_input),
            measured(self.output, self.unknown_output),
            measured(self.cached, self.unknown_cached),
            cache_rate,
            measured(self.reasoning, self.unknown_reasoning),
        )
    }

    fn merge(&mut self, other: &Self) {
        self.calls = self.calls.saturating_add(other.calls);
        self.input = self.input.saturating_add(other.input);
        self.output = self.output.saturating_add(other.output);
        self.cached = self.cached.saturating_add(other.cached);
        self.reasoning = self.reasoning.saturating_add(other.reasoning);
        self.unknown_input = self.unknown_input.saturating_add(other.unknown_input);
        self.unknown_output = self.unknown_output.saturating_add(other.unknown_output);
        self.unknown_cached = self.unknown_cached.saturating_add(other.unknown_cached);
        self.unknown_reasoning = self
            .unknown_reasoning
            .saturating_add(other.unknown_reasoning);
        self.paired_cache_input = self
            .paired_cache_input
            .saturating_add(other.paired_cache_input);
        self.paired_cache_hits = self
            .paired_cache_hits
            .saturating_add(other.paired_cache_hits);
    }

    pub fn report_breakdown(&self, title: &Self, apec: &Self) -> String {
        let mut total = Self::default();
        for meter in [self, title, apec] {
            total.merge(meter);
        }
        [
            "Measured usage (this Kroma process only):".to_owned(),
            self.summary_line("Coding"),
            title.summary_line("Auto titles"),
            apec.summary_line("APEC summaries"),
            total.summary_line("Combined"),
            "Last coding request: ".to_owned()
                + &self.compact_status(120).unwrap_or_default(),
            "Cache read tokens are part of input; reasoning tokens are part of output.".into(),
            "— = unavailable; ≥ = some requests unreported; ~ = cache ratio based on measured requests only.".into(),
            "APEC counts finalized summaries; unfinished or failed attempts may have unreported costs.".into(),
        ]
        .join("\n")
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
        assert!(meter
            .report_breakdown(&UsageMeter::default(), &UsageMeter::default())
            .contains("Hit ~80.0%"));
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
            .contains("Cache 66.7% (2.0k/3.0k)"));
        assert!(meter
            .report_breakdown(&UsageMeter::default(), &UsageMeter::default())
            .contains("Hit ~50.0%"));
        assert_eq!(meter.input, 5_000);
        assert_eq!(meter.output, 310);
        assert!(meter
            .report()
            .contains("50.0% (2000 cached / 4000 input, reported requests only)"));
    }

    #[test]
    fn cache_percent_never_rounds_a_partial_hit_to_exactly_one_hundred() {
        assert_eq!(UsageMeter::cache_percent(9_960, 10_000), "99.6%");
        assert_eq!(UsageMeter::cache_percent(9_999, 10_000), "<100%");
        assert_eq!(UsageMeter::cache_percent(10_000, 10_000), "100%");
        assert_eq!(UsageMeter::cache_percent(0, 10_000), "0%");
        assert_eq!(UsageMeter::cache_percent(0, 0), "—");
        let mut usage = UsageMeter::default();
        usage.record(Some(&Usage {
            input: Some(10_000),
            cached: Some(9_960),
            ..Usage::default()
        }));
        assert!(usage.compact_status(120).unwrap().contains("Cache 99.6%"));
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
        assert!(wide.contains("Cache 75% (6.0k/8.0k)"));
        assert!(!wide.contains("Fresh") && !wide.contains("Total"));
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
            .contains("Cache 0% (0/1.0k)"));
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
            .contains("Cache 0% (0/2.0k)"));
        meter.record(Some(&Usage {
            input: Some(4_000),
            output: Some(200),
            cached: Some(3_000),
            ..Usage::default()
        }));
        let footer = meter.compact_status(145).unwrap();
        assert!(footer.contains("Last In 4.0k · Out 200 · Cache 75% (3.0k/4.0k)"));
        assert!(!footer.contains("Total"));
        assert!(meter
            .report_breakdown(&UsageMeter::default(), &UsageMeter::default())
            .contains("Combined: 2 call(s) | In 6000 | Out 300 | Cache read 3000 | Hit 50.0%"));
        assert_eq!(meter.cache_hit_rate(), Some((3_000, 6_000, false)));
        meter.record(None);
        let unknown = meter.compact_status(120).unwrap();
        assert!(unknown.contains("Last In — · Out — · Cache —"));
        assert!(meter
            .report_breakdown(&UsageMeter::default(), &UsageMeter::default())
            .contains("Hit ~50.0%"));
    }

    #[test]
    fn separate_categories_sum_measured_usage_without_mixing_last_coding_request() {
        let mut coding = UsageMeter::default();
        coding.record(Some(&Usage {
            input: Some(1_000),
            output: Some(80),
            cached: Some(800),
            ..Usage::default()
        }));
        let last_before = coding.compact_status(100);
        let mut titles = UsageMeter::default();
        titles.record_batch(
            1,
            Some(&Usage {
                input: Some(117),
                output: Some(95),
                cached: Some(0),
                ..Usage::default()
            }),
        );
        let mut apec = UsageMeter::default();
        apec.record_batch(
            2,
            Some(&Usage {
                input: Some(500),
                output: Some(70),
                cached: Some(100),
                ..Usage::default()
            }),
        );
        assert_eq!(coding.compact_status(100), last_before);
        let report = coding.report_breakdown(&titles, &apec);
        assert!(
            report.contains("Coding: 1 call(s) | In 1000 | Out 80 | Cache read 800 | Hit 80.0%")
        );
        assert!(
            report.contains("Auto titles: 1 call(s) | In 117 | Out 95 | Cache read 0 | Hit 0.0%")
        );
        assert!(report
            .contains("APEC summaries: 2 call(s) | In 500 | Out 70 | Cache read 100 | Hit 20.0%"));
        assert!(report.contains("Combined: 4 call(s) | In 1617 | Out 245 | Cache read 900"));
        titles.record_batch(1, None);
        assert_eq!(titles.calls, 2);
        let partial = coding.report_breakdown(&titles, &apec);
        assert!(partial
            .contains("Auto titles: 2 call(s) | In ≥117 | Out ≥95 | Cache read — | Hit ~0.0%"));
        assert!(partial.contains("Combined: 5 call(s) | In ≥1617 | Out ≥245 | Cache read ≥900"));
    }

    #[test]
    fn compact_tokens_formats_million_and_sub_kilounits() {
        assert_eq!(compact_tokens(1_000_000), "1M");
        assert_eq!(compact_tokens(1_600), "1.6k");
        assert_eq!(compact_tokens(12_500), "12.5k");
        assert_eq!(compact_tokens(0), "0");
    }
}
