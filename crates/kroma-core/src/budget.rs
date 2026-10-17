use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use thiserror::Error;

/// F04 (`v1.5.1`): the absolute wall-clock deadline of a single run, shared by every phase that
/// can block — provider requests, tool execution, approvals, verification gates and child
/// subagents. Each phase clamps its own local timeout to `min(local, remaining)` instead of
/// creating an independent budget, so the run cannot overshoot `max_run_duration` just because a
/// request happened to start near the end.
#[derive(Debug, Clone, Copy)]
pub struct RunDeadline {
    at: Instant,
}

impl RunDeadline {
    pub fn at(at: Instant) -> Self {
        Self { at }
    }

    /// Time left before the deadline; `Duration::ZERO` once it has passed.
    pub fn remaining(&self) -> Duration {
        self.at.saturating_duration_since(Instant::now())
    }

    pub fn is_exceeded(&self) -> bool {
        Instant::now() >= self.at
    }

    /// `min(local, remaining)`, floored at 1ms so a derived timeout is never zero (a zero timeout
    /// would make every request fail even when a sliver of the budget is left).
    pub fn clamp(&self, local: Duration) -> Duration {
        local.min(self.remaining()).max(Duration::from_millis(1))
    }

    /// The deadline as a [`tokio::time::Instant`] for `timeout_at` / `sleep_until`.
    pub fn tokio_instant(&self) -> tokio::time::Instant {
        tokio::time::Instant::from_std(self.at)
    }
}

/// F05 (`v1.5.1`): one shared token ledger for a whole run *tree* (the parent run plus every
/// parallel child subagent and tournament candidate it spawns).
///
/// A caller [`reserve`](Self::try_reserve)s tokens *before* it sends a request or launches a child,
/// then [`settle`](TokenReservation::settle)s the reservation with the tokens the provider actually
/// reported. Reservation and settlement are two halves of one transaction over the pair
/// `(spent, reserved)`: [`TokenBudget`] guards both counters with a single mutex, so a settlement
/// that frees a reservation and records its spend can never be observed half-applied. Two
/// concurrent subagents therefore cannot both draw the same remaining balance — the loser's
/// reservation fails and the caller must not dispatch.
///
/// **Exact vs estimated.** When a provider returns `usage`, settlement records the exact reported
/// counts. When it does not, or the request is cancelled/timed out before a response arrives, the
/// reservation is settled with the pre-dispatch *estimate* (a byte-based approximation, ~4 bytes
/// per token). That keeps cancelled/timeout spend visible instead of silently refunding it, but it
/// is not a measured token count. Without a tokenizer this ledger therefore enforces a bound on
/// *estimated* spend and must not be advertised as an exact hard cap on real provider tokens.
///
/// The mutex serializes the (short, non-blocking) counter transaction. A poisoned mutex is
/// recovered through `into_inner` because the guarded state is a pair of plain counters that a
/// panicking caller cannot leave in a partially-updated state (each transaction writes both
/// counters before releasing the lock).
#[derive(Debug)]
pub struct TokenBudget {
    limit: Option<u64>,
    state: Mutex<LedgerState>,
}

/// The two counters that together make up committed spend. Kept behind one mutex so a reservation
/// and a settlement can never interleave at an intermediate point.
#[derive(Debug, Default)]
struct LedgerState {
    spent: u64,
    reserved: u64,
    settled_tokens: u64,
    /// T04 (KR-02): settled amounts that came from the pre-dispatch estimate because the provider
    /// reported no usage. Kept apart from measured usage so no report can call them measured.
    estimated_tokens: u64,
    unknown_tokens: u64,
}

/// Atomic accounting view. Settled tokens may include caller estimates when usage was absent;
/// they are not automatically measured usage. Unknown tokens are conservative holds on dropped
/// reservations, not a claim about what the provider actually billed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TokenAccountingSnapshot {
    pub limit: Option<u64>,
    pub spent: u64,
    pub reserved: u64,
    pub settled_tokens: u64,
    /// T04 (KR-02): of `settled_tokens`, how many were the caller's estimate (no provider usage).
    pub estimated_tokens: u64,
    pub unknown_tokens: u64,
    pub overage_tokens: u64,
}

impl TokenBudget {
    pub fn new(limit: Option<u64>) -> Self {
        Self {
            limit,
            state: Mutex::new(LedgerState::default()),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, LedgerState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The configured ceiling, or `None` when the ledger is unbounded.
    pub fn limit(&self) -> Option<u64> {
        self.limit
    }

    /// Tokens already settled as spent (exact when the provider reported usage, otherwise the
    /// pre-dispatch estimate).
    pub fn spent(&self) -> u64 {
        self.state().spent
    }

    /// Snapshot all accounting dimensions under the same lock, including actual ledger overage.
    pub fn accounting_snapshot(&self) -> TokenAccountingSnapshot {
        let state = self.state();
        TokenAccountingSnapshot {
            limit: self.limit,
            spent: state.spent,
            reserved: state.reserved,
            settled_tokens: state.settled_tokens,
            estimated_tokens: state.estimated_tokens,
            unknown_tokens: state.unknown_tokens,
            overage_tokens: self
                .limit
                .map_or(0, |limit| state.spent.saturating_sub(limit)),
        }
    }

    /// Tokens currently held by live reservations (a request or child in flight).
    pub fn reserved(&self) -> u64 {
        self.state().reserved
    }

    /// `spent + reserved` — the amount currently committed against the limit.
    pub fn committed(&self) -> u64 {
        let state = self.state();
        state.spent.saturating_add(state.reserved)
    }

    /// Tokens that may still be reserved. `None` when the ledger is unbounded.
    pub fn remaining(&self) -> Option<u64> {
        self.limit.map(|l| l.saturating_sub(self.committed()))
    }

    /// Reserve `amount` tokens as one transaction over `(spent, reserved)`. Returns `None` (without
    /// changing any counter) when the reservation would push `spent + reserved` past the limit, so a
    /// caller can stop *before* it dispatches a request or launches a child.
    pub fn try_reserve(self: &Arc<Self>, amount: u64) -> Option<TokenReservation> {
        {
            let mut state = self.state();
            if let Some(limit) = self.limit {
                let next = state.reserved.saturating_add(amount);
                if state.spent.saturating_add(next) > limit {
                    return None;
                }
            }
            state.reserved = state.reserved.saturating_add(amount);
        }
        Some(TokenReservation {
            budget: Arc::clone(self),
            amount,
            settled: false,
        })
    }

    /// Release `reserved_amount` tokens and record `actual` as spent in one mutex-guarded
    /// transaction. Both counters are updated before the guard is released, so no observer ever
    /// sees the reservation freed while the corresponding spend is still missing (the P1-1 race).
    fn settle_transaction(
        &self,
        reserved_amount: u64,
        actual: u64,
        settlement: Settlement,
    ) -> SettlementRecord {
        let mut state = self.state();
        state.reserved = state.reserved.saturating_sub(reserved_amount);
        state.spent = state.spent.saturating_add(actual);
        match settlement {
            Settlement::PossiblyDispatchedUnknown => {
                state.unknown_tokens = state.unknown_tokens.saturating_add(actual);
            }
            Settlement::Estimated { .. } => {
                state.settled_tokens = state.settled_tokens.saturating_add(actual);
                state.estimated_tokens = state.estimated_tokens.saturating_add(actual);
            }
            Settlement::NotDispatched | Settlement::CompletedUsage { .. } => {
                state.settled_tokens = state.settled_tokens.saturating_add(actual);
            }
        }
        let spent_after = state.spent;
        let limit = self.limit;
        drop(state);
        SettlementRecord {
            settlement,
            reserved: reserved_amount,
            charged: actual,
            over_reservation: actual.saturating_sub(reserved_amount),
            budget_overage: limit.map_or(0, |l| spent_after.saturating_sub(l)),
            spent_after,
            remaining_after: limit.map(|l| l.saturating_sub(spent_after)),
        }
    }
}

/// T04 (KR-02): how a reservation was closed out.
///
/// The three states exist because "the request did not consume anything" and "we cannot prove what
/// it consumed" must not be treated the same way. Only [`Settlement::NotDispatched`] returns the
/// allowance to the ledger.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Settlement {
    /// Provably never transmitted (a pre-dispatch validation failure). Consumption is exactly zero.
    NotDispatched,
    /// Transmitted, and the provider reported usage: an exact charge.
    CompletedUsage { input: u64, output: u64 },
    /// Transmitted, but the provider omitted usage. The pre-dispatch estimate is charged and
    /// labelled as an estimate — never presented as measured usage.
    Estimated { input: u64, output: u64 },
    /// Transmitted, or possibly transmitted, with unknown consumption (timeout, cancellation,
    /// dropped retry). The whole reservation is charged and tagged unknown; it is never refunded.
    PossiblyDispatchedUnknown,
}

/// T04 (KR-02): what one settlement did to the shared ledger, so callers can audit and tests can
/// assert conservation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SettlementRecord {
    pub settlement: Settlement,
    pub reserved: u64,
    pub charged: u64,
    /// Charge beyond the reservation itself (the request used more than it reserved).
    pub over_reservation: u64,
    /// Charge beyond the configured limit, once the ledger is settled.
    pub budget_overage: u64,
    pub spent_after: u64,
    pub remaining_after: Option<u64>,
}

/// T04: [`TokenReservation::settle_not_dispatched`] refuses to refund a request that may have
/// reached the provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SettlementError {
    #[error("cannot refund a reservation after {wire_attempts} wire attempt(s) were transmitted")]
    AlreadyDispatched { wire_attempts: u64 },
}

/// A held [`TokenBudget`] reservation.
///
/// Call [`settle`](Self::settle) once the provider reports how many tokens the request actually
/// used.
///
/// **Policy: Provably Pre-Dispatch Failures vs. Ambiguous Post-Dispatch / Unknown Consumption:**
///
/// * **Provably Pre-Dispatch Failures:** When a request fails before any network transmission
///   takes place (such as [`crate::provider::ProviderError::InsecureUrl`] validation failure),
///   provider consumption is guaranteed to be zero. Callers settle the reservation with `0` via
///   [`settle(0)`](Self::settle) to immediately release the withheld allowance back to the ledger.
///
/// * **Ambiguous Post-Dispatch Failures & Unknown Consumption:** If a request encounters an HTTP
///   error (500, 502, 503, 504, 4xx), times out, is cancelled mid-flight, or suffers an in-flight
///   transport failure (or fails after earlier retried attempts), actual provider consumption
///   cannot be proven to be zero. To keep shared parent+child ceilings fail-closed, un-settled
///   reservations dropped via [`Drop`] are conservatively committed as spent (`self.amount`) to
///   prevent concurrent or subsequent callers from spending beyond the configured limit.
///
/// Factual observed usage metrics (reported in events and run summaries) must always remain separate
/// from conservative ledger reserves and record only measured tokens.
#[derive(Debug)]
pub struct TokenReservation {
    budget: Arc<TokenBudget>,
    amount: u64,
    settled: bool,
}

impl TokenReservation {
    /// The number of tokens reserved up front.
    pub fn amount(&self) -> u64 {
        self.amount
    }

    /// The ledger this reservation was drawn from.
    pub fn budget(&self) -> &Arc<TokenBudget> {
        &self.budget
    }

    /// Release the reservation and record `actual` tokens as spent in a single transaction, so a
    /// concurrent [`TokenBudget::try_reserve`] can never observe the freed reservation without the
    /// settled spend that replaces it.
    ///
    /// This is the legacy entry point used where the caller has already decided the basis; prefer
    /// the explicit [`settle_not_dispatched`](Self::settle_not_dispatched),
    /// [`settle_with_usage`](Self::settle_with_usage),
    /// [`settle_estimate`](Self::settle_estimate) and
    /// [`settle_unknown`](Self::settle_unknown) so the ledger knows *why*.
    pub fn settle(mut self, actual: u64) -> SettlementRecord {
        let record = self.budget.settle_transaction(
            self.amount,
            actual,
            Settlement::CompletedUsage {
                input: actual,
                output: 0,
            },
        );
        self.settled = true;
        record
    }

    /// T04 (KR-02): the request provably never reached the provider, so the allowance goes back.
    ///
    /// `wire_attempts` is the count the provider actually transmitted (0 = nothing was sent). A
    /// non-zero count is refused instead of silently refunding a request that may have been billed.
    pub fn settle_not_dispatched(
        mut self,
        wire_attempts: u64,
    ) -> Result<SettlementRecord, SettlementError> {
        if wire_attempts > 0 {
            // Leave the reservation un-settled so `Drop` charges it as unknown.
            return Err(SettlementError::AlreadyDispatched { wire_attempts });
        }
        let record = self
            .budget
            .settle_transaction(self.amount, 0, Settlement::NotDispatched);
        self.settled = true;
        Ok(record)
    }

    /// T04 (KR-02): the provider reported usage for this request — an exact charge.
    pub fn settle_with_usage(mut self, input: u64, output: u64) -> SettlementRecord {
        let record = self.budget.settle_transaction(
            self.amount,
            input.saturating_add(output),
            Settlement::CompletedUsage { input, output },
        );
        self.settled = true;
        record
    }

    /// T04 (KR-02): the request was transmitted but the provider omitted usage, so the pre-dispatch
    /// estimate is charged and labelled as an estimate.
    pub fn settle_estimate(mut self, input: u64, output: u64) -> SettlementRecord {
        let record = self.budget.settle_transaction(
            self.amount,
            input.saturating_add(output),
            Settlement::Estimated { input, output },
        );
        self.settled = true;
        record
    }

    /// T04 (KR-02): consumption cannot be established (timeout, cancellation, dropped retry). The
    /// whole reservation is charged and tagged unknown; it is never refunded.
    pub fn settle_unknown(mut self) -> SettlementRecord {
        let record = self.budget.settle_transaction(
            self.amount,
            self.amount,
            Settlement::PossiblyDispatchedUnknown,
        );
        self.settled = true;
        record
    }
}

impl Drop for TokenReservation {
    fn drop(&mut self) {
        if !self.settled {
            self.budget.settle_transaction(
                self.amount,
                self.amount,
                Settlement::PossiblyDispatchedUnknown,
            );
        }
    }
}

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
    /// T04 (KR-02): the most output tokens a single request may claim. `None` uses
    /// [`DEFAULT_PER_REQUEST_OUTPUT_TOKENS`]. This is a *per-request* cap, distinct from the run
    /// allowance: it is what makes a retry possible after a transient failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens_per_request: Option<u64>,
    /// F05 (`v1.5.1`): optional total token ceiling (input + output) for one run *tree* — the
    /// parent run and every parallel child subagent it spawns draw from the same
    /// [`TokenBudget`]. `None` means unbounded. This is the single entry the CLI (`--max-tokens`),
    /// the config file (`max_tokens`), the RPC surface (`session.prompt.max_tokens`) and the
    /// runtime all set, so no surface can enforce a different cap than the kernel.
    ///
    /// Honesty caveat: without a tokenizer the pre-dispatch estimate is byte-based, so the cap
    /// bounds *estimated* spend, not a measured one. See [`TokenBudget`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_total_tokens: Option<u64>,
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
            max_output_tokens_per_request: None,
            max_total_tokens: None,
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
        if let Some(cap) = self.max_output_tokens_per_request {
            check_nonzero!(cap, "max_output_tokens_per_request");
        }
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

        // F05 (`v1.5.1`): a `max_total_tokens` of 0 would refuse every request, which is never a
        // useful configuration — reject it instead of silently disabling the run.
        if self.max_total_tokens == Some(0) {
            return Err(ConfigError::ZeroValue {
                field: "max_total_tokens",
            });
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

    /// F04 (`v1.5.1`): a per-request view of this budget with every timeout clamped to the
    /// remaining run time and the request-attempt cap set to `max_request_attempts`. Passing this
    /// to [`crate::provider::ModelProvider::complete`] means a provider's internal connect / idle /
    /// request deadlines and its retry loop can never outlive the parent run.
    pub fn clamped_to_deadline(
        &self,
        deadline: RunDeadline,
        max_request_attempts: usize,
    ) -> BudgetConfig {
        let mut cfg = self.clone();
        cfg.max_run_duration = deadline.clamp(cfg.max_run_duration);
        cfg.connect_timeout = deadline.clamp(cfg.connect_timeout);
        cfg.idle_timeout = deadline.clamp(cfg.idle_timeout);
        cfg.request_timeout = deadline.clamp(cfg.request_timeout);
        cfg.max_model_request_attempts = max_request_attempts.max(1);
        cfg
    }

    pub fn token_watermarks(&self) -> Option<(u64, u64)> {
        self.model_context_window_tokens.map(|w| {
            let high = ((w as f64) * 0.75) as u64;
            let low = ((w as f64) * 0.45) as u64;
            (high.max(1), low.max(1))
        })
    }
}

pub(crate) fn combine_usage_basis(left: &str, right: &str) -> &'static str {
    match (left, right) {
        ("provider_reported", "provider_reported") => "provider_reported",
        ("byte_estimate_not_measured", "byte_estimate_not_measured") => {
            "byte_estimate_not_measured"
        }
        ("unknown", "unknown") => "unknown",
        _ => "mixed",
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
    /// B16: turns whose numbers came from provider usage vs from the byte estimate.
    measured_usage_turns: usize,
    estimated_usage_turns: usize,
    child_usage_basis: Option<String>,
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
            measured_usage_turns: 0,
            estimated_usage_turns: 0,
            child_usage_basis: None,
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

    /// F04 (`v1.5.1`): the absolute deadline this tracker enforces, so phases that do not own the
    /// tracker (tool runtime, verification gate, subagents) can clamp to the same instant.
    pub fn deadline(&self) -> RunDeadline {
        RunDeadline::at(self.started_at + self.config.max_run_duration)
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

    /// Return the optimistic first-attempt slot only after the provider proves admission failed
    /// before any transmission. Cancellation after dispatch must never use this refund.
    pub(crate) fn refund_unsent_request_attempt(&mut self) {
        self.request_attempts_used = self.request_attempts_used.saturating_sub(1);
    }

    pub fn try_consume_tool_call(&mut self) -> bool {
        if self.tool_calls_used >= self.config.max_tool_calls {
            return false;
        }
        self.tool_calls_used += 1;
        true
    }

    pub fn record_token_usage(&mut self, input: u64, cached: Option<u64>, output: u64) {
        self.add_tokens(input, cached, output);
        self.measured_usage_turns = self.measured_usage_turns.saturating_add(1);
    }

    /// B16: record a *byte-based estimate* for a turn whose response carried no usage. The numbers
    /// land in the same totals (so the cap still applies), but the origin is tracked separately:
    /// a report must never present an estimate as measured usage.
    pub fn record_estimated_token_usage(&mut self, input: u64, cached: Option<u64>, output: u64) {
        self.add_tokens(input, cached, output);
        self.estimated_usage_turns = self.estimated_usage_turns.saturating_add(1);
    }

    fn add_tokens(&mut self, input: u64, cached: Option<u64>, output: u64) {
        self.total_input_tokens = self.total_input_tokens.saturating_add(input);
        self.total_cached_input_tokens = self
            .total_cached_input_tokens
            .saturating_add(cached.unwrap_or(0));
        self.total_output_tokens = self.total_output_tokens.saturating_add(output);
    }

    /// Where this run's usage numbers came from.
    ///
    /// `provider_reported` means every recorded turn carried provider usage; `mixed` means some turns
    /// were estimated; `byte_estimate_not_measured` means none reported usage; `unknown` means no
    /// turn recorded anything at all (which is not the same as zero spend).
    pub fn usage_basis(&self) -> &'static str {
        let own = match (self.measured_usage_turns, self.estimated_usage_turns) {
            (0, 0) => "unknown",
            (_, 0) => "provider_reported",
            (0, _) => "byte_estimate_not_measured",
            _ => "mixed",
        };
        self.child_usage_basis
            .as_deref()
            .map_or(own, |child| combine_usage_basis(own, child))
    }

    pub(crate) fn record_children_usage_basis(&mut self, basis: &str) {
        self.child_usage_basis = Some(self.child_usage_basis.as_deref().map_or_else(
            || basis.to_string(),
            |previous| combine_usage_basis(previous, basis).to_string(),
        ));
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

    /// B16: cached-input tokens reported by child subagents, so a cost estimate can split the
    /// cached rate from the uncached one instead of pricing every input token the same.
    pub fn children_cached_input_tokens(&self) -> u64 {
        self.children_cached_input_tokens
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

// ---------------------------------------------------------------------------------------------
// T03 (KR-03 / KR-07): model capabilities, host budget ceiling and a conservative estimator
// ---------------------------------------------------------------------------------------------

/// T03: the conservative context window assumed when neither the operator nor a capability
/// declaration gives one. Deliberately small: sending a request that is too large for the model
/// wastes a whole turn (and may be rejected by the provider), while compacting a little early only
/// costs a stub.
pub const CONSERVATIVE_CONTEXT_WINDOW_TOKENS: u64 = 32_768;

/// T04 (KR-02): the default output allowance one *request* may claim.
///
/// Before T04 the first request reserved the whole remaining run allowance as its output cap, so a
/// single transient failure left nothing for the retry. A request now reserves only what it can
/// plausibly consume; the run allowance stays available for retries and siblings. An operator can
/// raise this with [`BudgetConfig::max_output_tokens_per_request`].
pub const DEFAULT_PER_REQUEST_OUTPUT_TOKENS: u64 = 8_192;

/// T03: where a capability number came from. Reported to the operator instead of pretending every
/// number is a published spec.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapabilitySource {
    /// Explicitly configured by the operator (`model_context_window_tokens` / CLI).
    Configured,
    /// Nothing was configured and the model is not declared, so a conservative fallback applies.
    ConservativeFallback,
    /// No window is enforced at all (the operator explicitly disabled the check).
    Unbounded,
}

impl CapabilitySource {
    pub fn as_str(self) -> &'static str {
        match self {
            CapabilitySource::Configured => "configured",
            CapabilitySource::ConservativeFallback => "conservative_fallback",
            CapabilitySource::Unbounded => "unbounded",
        }
    }
}

/// T03 (KR-07): the resolved capabilities for the active model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelCapabilities {
    /// `None` only when the operator explicitly disabled the window check.
    pub context_window_tokens: Option<u64>,
    /// The provider's own output cap, when known.
    pub max_output_tokens: Option<u64>,
    pub source: CapabilitySource,
}

impl ModelCapabilities {
    /// T03: resolve the window from the operator's configuration, falling back conservatively.
    ///
    /// Kroma deliberately keeps **no model-name table**: a nickname in one account is not a public
    /// specification, and a wrong table is worse than an honest fallback.
    pub fn resolve(
        configured_window: Option<u64>,
        configured_output: Option<u64>,
    ) -> ModelCapabilities {
        match configured_window {
            Some(0) => ModelCapabilities {
                context_window_tokens: None,
                max_output_tokens: configured_output,
                source: CapabilitySource::Unbounded,
            },
            Some(window) => ModelCapabilities {
                context_window_tokens: Some(window),
                max_output_tokens: configured_output,
                source: CapabilitySource::Configured,
            },
            None => ModelCapabilities {
                context_window_tokens: Some(CONSERVATIVE_CONTEXT_WINDOW_TOKENS),
                max_output_tokens: configured_output,
                source: CapabilitySource::ConservativeFallback,
            },
        }
    }

    /// T03: output tokens that must be reserved on top of the input estimate.
    pub fn reserved_output_tokens(&self, requested: Option<u64>) -> u64 {
        let cap = self.max_output_tokens.unwrap_or(u64::MAX);
        requested.unwrap_or(0).min(cap)
    }

    /// T04 (KR-02): the output allowance one request may claim.
    ///
    /// Bounded by the operator's per-request cap and by the provider's own cap. `run_remaining` is
    /// the *run* allowance; the result is never larger than it, but it is also never the whole of
    /// it unless the operator asked for that.
    pub fn per_request_output_cap(&self, host_cap: Option<u64>, run_remaining: Option<u64>) -> u64 {
        let mut cap = host_cap.unwrap_or(DEFAULT_PER_REQUEST_OUTPUT_TOKENS);
        if let Some(provider_cap) = self.max_output_tokens {
            cap = cap.min(provider_cap);
        }
        if let Some(remaining) = run_remaining {
            cap = cap.min(remaining);
        }
        cap
    }
}

/// T03 (KR-03): the operator-owned ceiling a workspace preference may lower but never raise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HostBudgetCeiling {
    pub max_total_tokens: Option<u64>,
    pub max_subagents: Option<usize>,
    pub context_window_tokens: Option<u64>,
}

/// T03: result of applying an untrusted preference to a host ceiling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CeilingOutcome<T> {
    pub effective: T,
    /// `true` when the preference asked for more than the host allows and was clamped.
    pub clamped: bool,
}

impl HostBudgetCeiling {
    /// A workspace may lower a cap; asking for more is clamped to the host value and reported.
    pub fn clamp_tokens(&self, preference: Option<u64>) -> CeilingOutcome<Option<u64>> {
        match (self.max_total_tokens, preference) {
            (Some(host), Some(pref)) if pref > host => CeilingOutcome {
                effective: Some(host),
                clamped: true,
            },
            (Some(host), Some(pref)) => CeilingOutcome {
                effective: Some(pref.min(host)),
                clamped: false,
            },
            (Some(host), None) => CeilingOutcome {
                effective: Some(host),
                clamped: false,
            },
            (None, pref) => CeilingOutcome {
                effective: pref,
                clamped: false,
            },
        }
    }

    /// Same rule for concurrency: a repository cannot grant itself more parallel subagents.
    pub fn clamp_subagents(&self, preference: Option<usize>) -> CeilingOutcome<Option<usize>> {
        match (self.max_subagents, preference) {
            (Some(host), Some(pref)) if pref > host => CeilingOutcome {
                effective: Some(host),
                clamped: true,
            },
            (Some(host), Some(pref)) => CeilingOutcome {
                effective: Some(pref.min(host)),
                clamped: false,
            },
            (Some(host), None) => CeilingOutcome {
                effective: Some(host),
                clamped: false,
            },
            (None, pref) => CeilingOutcome {
                effective: pref,
                clamped: false,
            },
        }
    }

    /// Same rule for the model context window.
    pub fn clamp_window(&self, preference: Option<u64>) -> CeilingOutcome<Option<u64>> {
        match (self.context_window_tokens, preference) {
            (Some(host), Some(pref)) if pref > host => CeilingOutcome {
                effective: Some(host),
                clamped: true,
            },
            (Some(host), Some(pref)) => CeilingOutcome {
                effective: Some(pref.min(host)),
                clamped: false,
            },
            (Some(host), None) => CeilingOutcome {
                effective: Some(host),
                clamped: false,
            },
            (None, pref) => CeilingOutcome {
                effective: pref,
                clamped: false,
            },
        }
    }
}

/// T03 (KR-03): conservative token estimate for text.
///
/// ASCII is estimated at 4 bytes per token. Every non-ASCII byte is charged at 1 token per 2 bytes,
/// because CJK text is roughly one token per character (3 UTF-8 bytes) and `bytes / 4` would
/// *under*-estimate it. This is an estimate, not a tokenizer, and it is documented as such.
/// T03 (KR-03): conservative token bound for a *byte count* we do not have the text of.
///
/// Three bytes per token is the safe direction for both scripts Kroma sees in practice: UTF-8 CJK
/// is 3 bytes per character and one token per character, so the bound is tight there; ASCII is 4
/// bytes per token, so the bound over-estimates by a third. Over-estimating is the point — the
/// number is used to decide whether a request may be sent at all.
pub fn conservative_bytes_tokens(bytes: usize) -> u64 {
    ((bytes as u64).saturating_add(2)) / 3
}

pub fn conservative_text_tokens(text: &str) -> u64 {
    let mut ascii = 0u64;
    let mut non_ascii = 0u64;
    for byte in text.bytes() {
        if byte.is_ascii() {
            ascii += 1;
        } else {
            non_ascii += 1;
        }
    }
    // `+3`/`+1` round up so a short string never estimates to zero.
    ascii.saturating_add(3) / 4 + non_ascii.saturating_add(1) / 2
}

/// T03 (KR-07): conservative estimate for one whole request: text plus image payloads.
///
/// Image bytes are charged at 4 bytes per token; a base64 image is far larger than the vision
/// tokens it bills, so this errs on the safe side. `serialization_overhead_bytes` covers JSON
/// framing, tool-call scaffolding and role markers that are not part of any single string.
pub fn conservative_request_tokens(
    system_prompt: &str,
    items: &[crate::messages::TranscriptItem],
    tool_schema_bytes: usize,
    serialization_overhead_bytes: usize,
) -> u64 {
    let mut total = conservative_text_tokens(system_prompt);
    for item in items {
        total = total.saturating_add(item.estimated_tokens());
    }
    total
        .saturating_add((tool_schema_bytes as u64).saturating_add(3) / 4)
        .saturating_add((serialization_overhead_bytes as u64).saturating_add(3) / 4)
}
