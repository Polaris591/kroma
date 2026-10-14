use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};
use thiserror::Error;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ConfigError {
    #[error("budget parameter `{field}` must be > 0")]
    ZeroValue { field: &'static str },
    #[error("budget parameter `{field}` overflowed during validation")]
    Overflow { field: &'static str },
    #[error("invalid watermark configuration: {reason}")]
    InvalidWatermarks { reason: &'static str },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BudgetConfig {
    pub max_model_turns: usize,
    pub max_model_request_attempts: usize,
    pub max_tool_calls: usize,
    pub max_tool_calls_per_response: usize,
    pub max_tool_argument_bytes: usize,
    pub max_run_duration: Duration,
    pub connect_timeout: Duration,
    pub idle_timeout: Duration,
    pub request_timeout: Duration,
    pub max_static_prefix_bytes: usize,
    pub context_high_watermark_bytes: usize,
    pub context_low_watermark_bytes: usize,
    pub context_tail_window_bytes: usize,
    pub context_min_tail_turns: usize,
    pub model_context_window_tokens: Option<u64>,
    pub max_total_tool_output_bytes: usize,
    pub max_tool_inline_output_bytes: usize,
    pub max_tool_inline_lines: usize,
    pub bash_head_bytes: usize,
    pub bash_tail_bytes: usize,
    pub bash_spill_bytes: usize,
    pub max_process_output_observed_bytes: usize,
    pub max_edit_file_bytes: usize,
    pub max_edit_input_bytes: usize,
    pub max_write_content_bytes: usize,
}

impl Default for BudgetConfig {
    fn default() -> Self {
        Self {
            max_model_turns: 50,
            max_model_request_attempts: 60,
            max_tool_calls: 80,
            max_tool_calls_per_response: 16,
            max_tool_argument_bytes: 1024 * 1024, // 1 MiB
            max_run_duration: Duration::from_secs(15 * 60), // 15 min
            connect_timeout: Duration::from_secs(10),
            idle_timeout: Duration::from_secs(180),
            request_timeout: Duration::from_secs(360),
            max_static_prefix_bytes: 8 * 1024,         // 8 KiB
            context_high_watermark_bytes: 1536 * 1024, // 1.5 MiB
            context_low_watermark_bytes: 768 * 1024,   // 768 KiB
            context_tail_window_bytes: 256 * 1024,     // 256 KiB
            context_min_tail_turns: 4,
            model_context_window_tokens: None,
            max_total_tool_output_bytes: 4 * 1024 * 1024, // 4 MiB
            max_tool_inline_output_bytes: 64 * 1024,      // 64 KiB
            max_tool_inline_lines: 2000,
            bash_head_bytes: 16 * 1024,                          // 16 KiB
            bash_tail_bytes: 48 * 1024,                          // 48 KiB
            bash_spill_bytes: 4 * 1024 * 1024,                   // 4 MiB
            max_process_output_observed_bytes: 16 * 1024 * 1024, // 16 MiB
            max_edit_file_bytes: 2 * 1024 * 1024,                // 2 MiB
            max_edit_input_bytes: 512 * 1024,                    // 512 KiB
            max_write_content_bytes: 1024 * 1024,                // 1 MiB
        }
    }
}

impl BudgetConfig {
    pub fn validate(&self) -> Result<(), ConfigError> {
        macro_rules! check_nonzero {
            ($val:expr, $name:literal) => {
                if $val == 0 {
                    return Err(ConfigError::ZeroValue { field: $name });
                }
            };
        }

        check_nonzero!(self.max_model_turns, "max_model_turns");
        check_nonzero!(
            self.max_model_request_attempts,
            "max_model_request_attempts"
        );
        check_nonzero!(self.max_tool_calls, "max_tool_calls");
        check_nonzero!(
            self.max_tool_calls_per_response,
            "max_tool_calls_per_response"
        );
        check_nonzero!(self.max_tool_argument_bytes, "max_tool_argument_bytes");
        check_nonzero!(self.max_run_duration.as_nanos(), "max_run_duration");
        check_nonzero!(self.connect_timeout.as_nanos(), "connect_timeout");
        check_nonzero!(self.idle_timeout.as_nanos(), "idle_timeout");
        check_nonzero!(self.request_timeout.as_nanos(), "request_timeout");
        check_nonzero!(self.max_static_prefix_bytes, "max_static_prefix_bytes");
        check_nonzero!(
            self.context_high_watermark_bytes,
            "context_high_watermark_bytes"
        );
        check_nonzero!(
            self.context_low_watermark_bytes,
            "context_low_watermark_bytes"
        );
        check_nonzero!(self.context_tail_window_bytes, "context_tail_window_bytes");
        check_nonzero!(self.context_min_tail_turns, "context_min_tail_turns");
        check_nonzero!(
            self.max_total_tool_output_bytes,
            "max_total_tool_output_bytes"
        );
        check_nonzero!(
            self.max_tool_inline_output_bytes,
            "max_tool_inline_output_bytes"
        );
        check_nonzero!(self.max_tool_inline_lines, "max_tool_inline_lines");
        check_nonzero!(self.bash_head_bytes, "bash_head_bytes");
        check_nonzero!(self.bash_tail_bytes, "bash_tail_bytes");
        check_nonzero!(self.bash_spill_bytes, "bash_spill_bytes");
        check_nonzero!(
            self.max_process_output_observed_bytes,
            "max_process_output_observed_bytes"
        );
        check_nonzero!(self.max_edit_file_bytes, "max_edit_file_bytes");
        check_nonzero!(self.max_edit_input_bytes, "max_edit_input_bytes");
        check_nonzero!(self.max_write_content_bytes, "max_write_content_bytes");

        if let Some(w_ctx) = self.model_context_window_tokens {
            if w_ctx == 0 {
                return Err(ConfigError::ZeroValue {
                    field: "model_context_window_tokens",
                });
            }
        }

        // Check duration ms conversion overflows
        if self.max_run_duration.as_millis() > u64::MAX as u128 {
            return Err(ConfigError::Overflow {
                field: "max_run_duration",
            });
        }
        if self.request_timeout.as_millis() > u64::MAX as u128 {
            return Err(ConfigError::Overflow {
                field: "request_timeout",
            });
        }

        // Check integer addition overflows
        let head_plus_tail = self
            .bash_head_bytes
            .checked_add(self.bash_tail_bytes)
            .ok_or(ConfigError::Overflow {
                field: "bash_head_bytes + bash_tail_bytes",
            })?;
        if head_plus_tail > self.max_tool_inline_output_bytes {
            return Err(ConfigError::InvalidWatermarks {
                reason: "bash_head_bytes + bash_tail_bytes exceeds max_tool_inline_output_bytes",
            });
        }

        if self
            .context_high_watermark_bytes
            .checked_add(self.max_total_tool_output_bytes)
            .is_none()
        {
            return Err(ConfigError::Overflow {
                field: "context_high_watermark_bytes + max_total_tool_output_bytes",
            });
        }

        if self.context_low_watermark_bytes >= self.context_high_watermark_bytes {
            return Err(ConfigError::InvalidWatermarks {
                reason: "context_low_watermark_bytes must be strictly less than context_high_watermark_bytes",
            });
        }

        if self.bash_spill_bytes > self.max_process_output_observed_bytes {
            return Err(ConfigError::InvalidWatermarks {
                reason: "bash_spill_bytes must be <= max_process_output_observed_bytes",
            });
        }

        Ok(())
    }

    pub fn token_watermarks(&self) -> Option<(u64, u64)> {
        self.model_context_window_tokens.map(|w| {
            let high = ((w as f64) * 0.75) as u64;
            let low = ((w as f64) * 0.45) as u64;
            (high.max(1), low.max(1))
        })
    }
}

#[derive(Debug, Clone)]
pub struct BudgetTracker {
    config: BudgetConfig,
    started_at: Instant,
    turns_used: usize,
    request_attempts_used: usize,
    tool_calls_used: usize,
    total_input_tokens: u64,
    total_cached_input_tokens: u64,
    total_output_tokens: u64,
    // F10 (`v1.4.0`): token spend of child subagents (plain fan-out + tournament candidates,
    // including failed/cancelled candidates). Tracked separately so callers can attribute cost to
    // `self` vs `children` while still seeing a combined total.
    children_input_tokens: u64,
    children_cached_input_tokens: u64,
    children_output_tokens: u64,
}

impl BudgetTracker {
    pub fn new(config: BudgetConfig) -> Result<Self, ConfigError> {
        config.validate()?;
        Ok(Self {
            config,
            started_at: Instant::now(),
            turns_used: 0,
            request_attempts_used: 0,
            tool_calls_used: 0,
            total_input_tokens: 0,
            total_cached_input_tokens: 0,
            total_output_tokens: 0,
            children_input_tokens: 0,
            children_cached_input_tokens: 0,
            children_output_tokens: 0,
        })
    }

    pub fn config(&self) -> &BudgetConfig {
        &self.config
    }

    pub fn elapsed(&self) -> Duration {
        self.started_at.elapsed()
    }

    pub fn remaining_duration(&self) -> Option<Duration> {
        self.config.max_run_duration.checked_sub(self.elapsed())
    }

    pub fn is_deadline_exceeded(&self) -> bool {
        self.elapsed() >= self.config.max_run_duration
    }

    pub fn try_begin_turn(&mut self) -> bool {
        if self.turns_used >= self.config.max_model_turns {
            return false;
        }
        self.turns_used += 1;
        true
    }

    pub fn try_record_request_attempt(&mut self) -> bool {
        if self.request_attempts_used >= self.config.max_model_request_attempts {
            return false;
        }
        self.request_attempts_used += 1;
        true
    }

    pub fn try_consume_tool_call(&mut self) -> bool {
        if self.tool_calls_used >= self.config.max_tool_calls {
            return false;
        }
        self.tool_calls_used += 1;
        true
    }

    pub fn record_token_usage(&mut self, input: u64, cached: Option<u64>, output: u64) {
        self.total_input_tokens = self.total_input_tokens.saturating_add(input);
        self.total_cached_input_tokens = self
            .total_cached_input_tokens
            .saturating_add(cached.unwrap_or(0));
        self.total_output_tokens = self.total_output_tokens.saturating_add(output);
    }

    /// F10 (`v1.4.0`): fold a child subagent's (or tournament candidate's) token spend into this
    /// tracker so the parent run summary can attribute `children` cost separately from `self` cost.
    pub fn record_children_token_usage(&mut self, input: u64, cached: Option<u64>, output: u64) {
        self.children_input_tokens = self.children_input_tokens.saturating_add(input);
        self.children_cached_input_tokens = self
            .children_cached_input_tokens
            .saturating_add(cached.unwrap_or(0));
        self.children_output_tokens = self.children_output_tokens.saturating_add(output);
    }

    pub fn turns_used(&self) -> usize {
        self.turns_used
    }

    pub fn request_attempts_used(&self) -> usize {
        self.request_attempts_used
    }

    pub fn tool_calls_used(&self) -> usize {
        self.tool_calls_used
    }

    pub fn total_input_tokens(&self) -> u64 {
        self.total_input_tokens
    }

    pub fn total_cached_input_tokens(&self) -> u64 {
        self.total_cached_input_tokens
    }

    pub fn total_output_tokens(&self) -> u64 {
        self.total_output_tokens
    }

    /// F10 (`v1.4.0`): input tokens spent by child subagents / tournament candidates.
    pub fn children_input_tokens(&self) -> u64 {
        self.children_input_tokens
    }

    /// F10 (`v1.4.0`): output tokens spent by child subagents / tournament candidates.
    pub fn children_output_tokens(&self) -> u64 {
        self.children_output_tokens
    }

    /// F10 (`v1.4.0`): total (self + children) input tokens.
    pub fn all_input_tokens(&self) -> u64 {
        self.total_input_tokens
            .saturating_add(self.children_input_tokens)
    }

    /// F10 (`v1.4.0`): total (self + children) output tokens.
    pub fn all_output_tokens(&self) -> u64 {
        self.total_output_tokens
            .saturating_add(self.children_output_tokens)
    }

    /// F10 (`v1.4.0`): total (self + children) token spend, the number used for budget decisions.
    pub fn all_total_tokens(&self) -> u64 {
        self.all_input_tokens()
            .saturating_add(self.all_output_tokens())
    }

    pub fn overall_cache_hit_ratio(&self) -> f64 {
        if self.total_input_tokens == 0 {
            0.0
        } else {
            (self.total_cached_input_tokens as f64) / (self.total_input_tokens as f64)
        }
    }
}
