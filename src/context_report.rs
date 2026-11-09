//! Honest accounting for the Desktop context card.
//!
//! The ring used to show `window - available_input`, which is the output
//! reserve: a constant that never reacts to the conversation. These segments
//! are measured from the same request projection the agent sends, using the
//! crate's conservative bytes/token heuristic, so the bar moves when history
//! and tool definitions actually grow.
use serde::Serialize;

/// One coloured slice of the context window.
#[derive(Serialize)]
pub struct ContextSegment {
    pub key: &'static str,
    pub tokens: usize,
}

#[derive(Serialize)]
pub struct ContextReport {
    pub window: usize,
    pub available_input: usize,
    pub output_reserve: usize,
    pub segments: Vec<ContextSegment>,
}

impl ContextReport {
    pub fn new(window: usize, available_input: usize, output_reserve: usize) -> Self {
        Self {
            window,
            available_input,
            output_reserve,
            segments: Vec::new(),
        }
    }

    pub fn push(&mut self, key: &'static str, tokens: usize) {
        if tokens > 0 {
            self.segments.push(ContextSegment { key, tokens });
        }
    }

    /// Remaining input space inside the model window, after the measured
    /// segments and the reserved output allowance.
    pub fn free(&self) -> usize {
        let used: usize = self.segments.iter().map(|s| s.tokens).sum();
        self.window
            .saturating_sub(used)
            .saturating_sub(self.output_reserve)
    }
}
