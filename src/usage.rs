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
}

impl UsageMeter {
    pub fn record(&mut self, usage: Option<&Usage>) {
        self.calls += 1;
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

    pub fn report(&self) -> String {
        let value = |count: u64, missing: usize| -> String {
            if missing == 0 {
                count.to_string()
            } else {
                format!("{count} measured (+{missing} unknown request(s))")
            }
        };
        format!(
            "Foreground usage (this Kroma process, not historical or APEC):\nModel calls: {}\nInput: {} tokens\nOutput: {} tokens\nReasoning: {} tokens (included in output; when reported)\nCached input: {} tokens (included in input)\nAPEC summary usage is reported separately in runtime logs.",
            self.calls,value(self.input,self.unknown_input),
            value(self.output,self.unknown_output),
            value(self.reasoning,self.unknown_reasoning),
            value(self.cached,self.unknown_cached),
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
    }
}
