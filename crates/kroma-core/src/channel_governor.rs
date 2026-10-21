//! B22: one governor for a model channel — bounded in-flight work, bounded retries with backoff, and
//! a circuit breaker.
//!
//! A channel that does not support concurrency (the DeepSeek/WorkBuddy pool is one such case) must be
//! driven through a limiter rather than being hoped about. This module provides that limiter as a
//! single object with three independent bounds:
//!
//!  * **in-flight cap** — a semaphore of `max_in_flight` permits, so a burst cannot fan out further
//!    than the policy allows, whatever the caller does;
//!  * **backoff schedule** — exponential with a hard cap, deterministic by default (jitter is opt-in and seeded), so a retry plan
//!    is reproducible in a report;
//!  * **circuit breaker** — after `failure_threshold` consecutive failures the channel is *open* for
//!    `cooldown`, during which [`ChannelGovernor::acquire`] refuses **without touching the channel**.
//!    After the cooldown exactly one probe is allowed; success closes the breaker, failure reopens it.
//!
//! What this does not claim: it does not make a serial channel concurrent, does not measure
//! throughput, and does not replace the provider's own rate limiting. It bounds *this process's*
//! fan-out and stops a failing channel from being hammered. Concurrency performance is a separate
//! experiment (and per the card, serial simulation proves scheduling logic only).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;

/// Bounds applied to one channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelPolicy {
    /// Maximum attempts (including the first) for a single logical request.
    pub max_attempts: usize,
    /// Maximum simultaneous in-flight requests on this channel.
    pub max_in_flight: usize,
    /// Backoff before the second attempt; doubles per attempt up to `max_backoff`.
    pub base_backoff: Duration,
    pub max_backoff: Duration,
    /// Consecutive failures that open the breaker.
    pub failure_threshold: usize,
    /// How long the breaker stays open before one probe is allowed.
    pub cooldown: Duration,
    /// KR-L04: optional jitter on the retry backoff.
    ///
    /// `None` (the default) keeps the retry plan exactly reproducible, which is what a report wants.
    /// Setting it spreads retries so a fleet that failed together does not retry in lockstep. The
    /// spread is derived from `(seed, attempt)`, so it is randomised in production and still
    /// pinnable by a test.
    pub jitter: Option<JitterPolicy>,
}

/// KR-L04: deterministic-but-spread retry jitter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JitterPolicy {
    /// Upper bound of the added delay, as a percentage of the computed backoff (0..=100).
    pub percent: u8,
    /// Seed for the pseudo-random sequence: same seed and same attempt always give the same delay.
    pub seed: u64,
}

impl JitterPolicy {
    pub fn new(percent: u8, seed: u64) -> Self {
        Self {
            percent: percent.min(100),
            seed,
        }
    }

    /// Fraction (0.0..1.0) of the allowed spread to add for this attempt.
    fn fraction_for_attempt(&self, attempt: usize) -> f64 {
        // xorshift64* over (seed, attempt): dependency-free, and identical for identical inputs --
        // which is exactly the property the task asks a randomised behaviour to have.
        let mut x = self.seed ^ ((attempt as u64) << 32) ^ 0x9E37_79B9_7F4A_7C15;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        let mixed = x.wrapping_mul(0x2545_F491_4F6C_DD1D);
        ((mixed >> 40) as f64) / ((1u64 << 24) as f64)
    }
}

impl Default for ChannelPolicy {
    fn default() -> Self {
        // Conservative: a channel that may not support concurrency is driven one at a time.
        Self {
            max_attempts: 3,
            max_in_flight: 1,
            base_backoff: Duration::from_millis(500),
            max_backoff: Duration::from_secs(8),
            failure_threshold: 3,
            cooldown: Duration::from_secs(30),
            jitter: None,
        }
    }
}

impl ChannelPolicy {
    /// One request at a time, three attempts, thirty-second breaker: the safe default for a pool
    /// that has rejected concurrent load.
    pub fn serial() -> Self {
        Self::default()
    }

    pub fn with_max_in_flight(mut self, max_in_flight: usize) -> Self {
        self.max_in_flight = max_in_flight.max(1);
        self
    }

    pub fn with_failure_threshold(mut self, threshold: usize) -> Self {
        self.failure_threshold = threshold.max(1);
        self
    }

    pub fn with_cooldown(mut self, cooldown: Duration) -> Self {
        self.cooldown = cooldown;
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreakerState {
    Closed,
    /// Refusing until this instant; exactly one probe is allowed after it.
    Open {
        until: Instant,
    },
    /// One probe is in flight; further callers wait for its verdict.
    HalfOpen,
}

impl BreakerState {
    pub fn as_str(self) -> &'static str {
        match self {
            BreakerState::Closed => "closed",
            BreakerState::Open { .. } => "open",
            BreakerState::HalfOpen => "half_open",
        }
    }
}

/// Why a caller was not allowed to use the channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GovernorError {
    /// The breaker is open: nothing was sent, and the caller may retry after `retry_after`.
    ChannelOpen { retry_after: Duration },
}

impl std::fmt::Display for GovernorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GovernorError::ChannelOpen { retry_after } => write!(
                f,
                "channel breaker is open after repeated failures; retry in {:?}",
                retry_after
            ),
        }
    }
}

impl std::error::Error for GovernorError {}

/// What the governor currently looks like, for a report or a status line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GovernorSnapshot {
    pub state: &'static str,
    pub in_flight: usize,
    pub consecutive_failures: usize,
    pub retry_after: Option<Duration>,
}

#[derive(Debug, Default)]
struct Inner {
    consecutive_failures: usize,
    breaker: Option<BreakerState>,
}

/// See the module documentation. Cheap to share: wrap in `Arc` and hand the same governor to every
/// caller of one channel.
#[derive(Debug)]
pub struct ChannelGovernor {
    policy: ChannelPolicy,
    permits: Semaphore,
    in_flight: AtomicUsize,
    inner: Mutex<Inner>,
}

impl ChannelGovernor {
    pub fn new(mut policy: ChannelPolicy) -> Self {
        policy.max_in_flight = policy.max_in_flight.max(1);
        policy.max_attempts = policy.max_attempts.max(1);
        Self {
            permits: Semaphore::new(policy.max_in_flight),
            in_flight: AtomicUsize::new(0),
            inner: Mutex::new(Inner::default()),
            policy,
        }
    }

    pub fn policy(&self) -> &ChannelPolicy {
        &self.policy
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|error| error.into_inner())
    }

    /// Ask permission for one attempt.
    ///
    /// Refuses while the breaker is open — deliberately *before* waiting on the semaphore, so an open
    /// breaker costs nothing and touches no channel state. After the cooldown exactly one probe is
    /// admitted; other callers are refused until that probe reports its verdict.
    pub async fn acquire(&self) -> Result<ChannelPermit<'_>, GovernorError> {
        self.check_breaker(false)?;
        let permit = self
            .permits
            .acquire()
            .await
            .expect("the governor's semaphore is never closed");
        // Health may have changed while this caller waited. Do not mark a half-open probe until
        // capacity is actually owned: dropping a waiting future must not strand the breaker.
        let probe = self.check_breaker(true)?;
        permit.forget();
        self.in_flight.fetch_add(1, Ordering::SeqCst);
        Ok(ChannelPermit {
            governor: self,
            probe,
        })
    }

    /// Non-blocking variant, for callers that prefer to shed load instead of queueing.
    pub fn try_acquire(&self) -> Result<ChannelPermit<'_>, GovernorError> {
        self.check_breaker(false)?;
        let permit = self
            .permits
            .try_acquire()
            .map_err(|_| GovernorError::ChannelOpen {
                retry_after: Duration::ZERO,
            })?;
        let probe = self.check_breaker(true)?;
        permit.forget();
        self.in_flight.fetch_add(1, Ordering::SeqCst);
        Ok(ChannelPermit {
            governor: self,
            probe,
        })
    }

    fn check_breaker(&self, admit: bool) -> Result<bool, GovernorError> {
        let mut inner = self.lock();
        match inner.breaker {
            Some(BreakerState::Open { until }) => {
                let now = Instant::now();
                if now < until {
                    return Err(GovernorError::ChannelOpen {
                        retry_after: until.saturating_duration_since(now),
                    });
                }
                if admit {
                    inner.breaker = Some(BreakerState::HalfOpen);
                }
                Ok(true)
            }
            Some(BreakerState::HalfOpen) => Err(GovernorError::ChannelOpen {
                retry_after: self.policy.cooldown,
            }),
            _ => Ok(false),
        }
    }

    /// Report that an attempt succeeded: the breaker closes and the failure streak resets.
    pub fn record_success(&self) {
        let mut inner = self.lock();
        inner.consecutive_failures = 0;
        inner.breaker = Some(BreakerState::Closed);
    }

    /// Report that an attempt failed. A failed probe reopens the breaker immediately; otherwise the
    /// breaker opens once the streak reaches the threshold.
    pub fn record_failure(&self) {
        let mut inner = self.lock();
        inner.consecutive_failures = inner.consecutive_failures.saturating_add(1);
        let probing = matches!(inner.breaker, Some(BreakerState::HalfOpen));
        if probing || inner.consecutive_failures >= self.policy.failure_threshold.max(1) {
            inner.breaker = Some(BreakerState::Open {
                until: Instant::now() + self.policy.cooldown,
            });
        }
    }

    /// Backoff before `attempt` (1-based; the first attempt is not delayed).
    ///
    /// Deterministic on purpose: a report should be able to state the exact retry plan.
    pub fn backoff_for_attempt(&self, attempt: usize) -> Duration {
        let base = self.deterministic_backoff(attempt);
        match self.policy.jitter {
            // Default: exactly reproducible, so a report can state the retry plan.
            None => base,
            Some(jitter) => {
                // Spread on top of the deterministic plan. Same seed and attempt always produce the
                // same delay, so enabling jitter does not make a run unreproducible.
                let spread = base.mul_f64(
                    jitter.fraction_for_attempt(attempt) * (f64::from(jitter.percent) / 100.0),
                );
                (base + spread).min(self.policy.max_backoff)
            }
        }
    }

    /// The un-jittered exponential plan.
    fn deterministic_backoff(&self, attempt: usize) -> Duration {
        if attempt <= 1 {
            return Duration::ZERO;
        }
        let steps = (attempt - 2).min(16) as u32;
        let scaled = self
            .policy
            .base_backoff
            .checked_mul(1u32 << steps)
            .unwrap_or(self.policy.max_backoff);
        scaled.min(self.policy.max_backoff)
    }

    pub fn snapshot(&self) -> GovernorSnapshot {
        let inner = self.lock();
        let (state, retry_after) = match inner.breaker {
            Some(BreakerState::Open { until }) => {
                let now = Instant::now();
                let remaining = until.saturating_duration_since(now);
                ("open", Some(remaining))
            }
            Some(BreakerState::HalfOpen) => ("half_open", None),
            _ => ("closed", None),
        };
        GovernorSnapshot {
            state,
            in_flight: self.in_flight.load(Ordering::SeqCst),
            consecutive_failures: inner.consecutive_failures,
            retry_after,
        }
    }
}

/// Permission for one attempt. Dropping it returns the permit and decrements the in-flight count, so
/// a panicking or cancelled caller cannot leak channel capacity.
#[derive(Debug)]
pub struct ChannelPermit<'a> {
    governor: &'a ChannelGovernor,
    probe: bool,
}

impl Drop for ChannelPermit<'_> {
    fn drop(&mut self) {
        if self.probe {
            let mut inner = self.governor.lock();
            if matches!(inner.breaker, Some(BreakerState::HalfOpen)) {
                // Cancellation is not a successful health check. Release the probing state but
                // keep the channel fail-closed until a fresh cooldown/probe can establish health.
                inner.breaker = Some(BreakerState::Open {
                    until: Instant::now() + self.governor.policy.cooldown,
                });
            }
        }
        self.governor.in_flight.fetch_sub(1, Ordering::SeqCst);
        self.governor.permits.add_permits(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_queued_caller_rechecks_the_breaker_after_capacity_returns() {
        let governor = ChannelGovernor::new(ChannelPolicy::serial().with_failure_threshold(1));
        let first = governor.acquire().await.expect("first request admitted");
        let mut queued = Box::pin(governor.acquire());
        // Poll the actual acquire future before opening the breaker, with no sleeps or tasks.
        assert!(
            tokio::time::timeout(Duration::from_millis(20), queued.as_mut())
                .await
                .is_err()
        );
        governor.record_failure();
        drop(first);
        assert!(
            queued.await.is_err(),
            "a queued caller must not bypass a newly opened breaker"
        );
        assert_eq!(governor.snapshot().in_flight, 0);
    }

    #[tokio::test]
    async fn dropping_a_half_open_probe_does_not_wedge_the_channel() {
        let governor = ChannelGovernor::new(
            ChannelPolicy::serial()
                .with_failure_threshold(1)
                .with_cooldown(Duration::ZERO),
        );
        governor.record_failure();
        let probe = governor.acquire().await.expect("cooldown elapsed");
        assert_eq!(governor.snapshot().state, "half_open");
        drop(probe); // Cancellation/future-drop supplies no health verdict.
        assert_ne!(
            governor.snapshot().state,
            "half_open",
            "a dropped probe must not strand the breaker"
        );
        assert!(
            governor.try_acquire().is_ok(),
            "a later probe must remain possible"
        );
    }

    #[test]
    fn in_flight_never_exceeds_the_policy() {
        let governor = ChannelGovernor::new(ChannelPolicy::serial().with_max_in_flight(2));
        let first = governor.try_acquire().expect("first permit");
        let second = governor.try_acquire().expect("second permit");
        assert_eq!(governor.snapshot().in_flight, 2);
        assert!(
            governor.try_acquire().is_err(),
            "a third permit must not be handed out"
        );

        drop(first);
        assert_eq!(governor.snapshot().in_flight, 1);
        let third = governor.try_acquire().expect("capacity was returned");
        drop(third);
        drop(second);
        assert_eq!(governor.snapshot().in_flight, 0);
    }

    /// The point of the breaker: a failing channel is not hammered. Refusals happen without the
    /// caller ever reaching the channel.
    #[test]
    fn a_failure_storm_opens_the_breaker_before_any_further_call() {
        let governor = ChannelGovernor::new(
            ChannelPolicy::serial()
                .with_failure_threshold(3)
                .with_cooldown(Duration::from_secs(5)),
        );
        for _ in 0..3 {
            governor.record_failure();
        }
        let snapshot = governor.snapshot();
        assert_eq!(snapshot.state, "open");
        assert_eq!(snapshot.consecutive_failures, 3);
        assert!(snapshot.retry_after.unwrap() > Duration::ZERO);

        // Bound first: the permit borrows the governor, so the temporary must not outlive it.
        let outcome = governor.try_acquire();
        match outcome {
            Err(GovernorError::ChannelOpen { retry_after }) => {
                assert!(
                    retry_after > Duration::ZERO,
                    "the caller is told when to retry"
                );
            }
            Ok(permit) => {
                drop(permit);
                panic!("expected an open-breaker refusal, but a permit was handed out");
            }
        }
    }

    #[tokio::test]
    async fn a_success_resets_the_failure_streak() {
        let governor = ChannelGovernor::new(ChannelPolicy::serial().with_failure_threshold(3));
        governor.record_failure();
        governor.record_failure();
        governor.record_success();
        governor.record_failure();
        governor.record_failure();
        assert_eq!(
            governor.snapshot().state,
            "closed",
            "failures must be *consecutive* to open the breaker"
        );
    }

    #[tokio::test]
    async fn the_cooldown_admits_exactly_one_probe_and_success_closes() {
        let governor = ChannelGovernor::new(
            ChannelPolicy::serial()
                .with_failure_threshold(1)
                .with_cooldown(Duration::from_millis(60)),
        );
        governor.record_failure();
        assert_eq!(governor.snapshot().state, "open");

        tokio::time::sleep(Duration::from_millis(80)).await;
        let probe = governor
            .acquire()
            .await
            .expect("after the cooldown one probe is allowed");
        assert_eq!(governor.snapshot().state, "half_open");
        assert!(
            governor.try_acquire().is_err(),
            "a second caller must wait for the probe's verdict"
        );

        governor.record_success();
        assert_eq!(governor.snapshot().state, "closed");
        drop(probe);
        assert!(
            governor.try_acquire().is_ok(),
            "the channel is usable again"
        );
    }

    #[tokio::test]
    async fn a_failed_probe_reopens_the_breaker() {
        let governor = ChannelGovernor::new(
            ChannelPolicy::serial()
                .with_failure_threshold(1)
                .with_cooldown(Duration::from_millis(60)),
        );
        governor.record_failure();
        tokio::time::sleep(Duration::from_millis(80)).await;
        let probe = governor.acquire().await.expect("probe admitted");
        governor.record_failure();
        assert_eq!(governor.snapshot().state, "open");
        assert!(governor.snapshot().retry_after.unwrap() > Duration::ZERO);
        drop(probe);
    }

    /// The retry plan must be reproducible from the report, so the schedule is exact.
    #[test]
    fn backoff_is_exponential_and_capped() {
        let mut policy = ChannelPolicy::serial();
        policy.base_backoff = Duration::from_millis(10);
        policy.max_backoff = Duration::from_millis(40);
        policy.max_attempts = 6;
        let governor = ChannelGovernor::new(policy);

        assert_eq!(governor.backoff_for_attempt(1), Duration::ZERO);
        assert_eq!(governor.backoff_for_attempt(2), Duration::from_millis(10));
        assert_eq!(governor.backoff_for_attempt(3), Duration::from_millis(20));
        assert_eq!(governor.backoff_for_attempt(4), Duration::from_millis(40));
        assert_eq!(
            governor.backoff_for_attempt(5),
            Duration::from_millis(40),
            "the cap holds however many attempts are configured"
        );
        assert_eq!(governor.backoff_for_attempt(9), Duration::from_millis(40));
    }

    /// A dropped permit returns capacity: cancellation must not leak the channel's allowance.
    #[test]
    fn a_dropped_permit_returns_capacity() {
        let governor = ChannelGovernor::new(ChannelPolicy::serial());
        {
            let _permit = governor.try_acquire().expect("the single permit");
            assert!(governor.try_acquire().is_err());
        }
        assert_eq!(governor.snapshot().in_flight, 0);
        assert!(governor.try_acquire().is_ok(), "capacity came back on drop");
    }

    /// KR-L04: seeded jitter must be reproducible and must actually spread.
    #[test]
    fn seeded_jitter_is_reproducible_and_spreads() {
        let base = ChannelPolicy {
            base_backoff: Duration::from_millis(100),
            max_backoff: Duration::from_secs(10),
            ..Default::default()
        };
        let plain = ChannelGovernor::new(base.clone());
        let jittered = ChannelPolicy {
            jitter: Some(JitterPolicy::new(50, 42)),
            ..base.clone()
        };

        let first = ChannelGovernor::new(jittered.clone()).backoff_for_attempt(3);
        let again = ChannelGovernor::new(jittered.clone()).backoff_for_attempt(3);
        assert_eq!(
            first, again,
            "the same seed and attempt must always produce the same delay"
        );

        let unjittered = plain.backoff_for_attempt(3);
        assert!(
            first >= unjittered,
            "jitter may only add delay: {first:?} < {unjittered:?}"
        );
        let ceiling = unjittered.mul_f64(1.5) + Duration::from_millis(2);
        assert!(
            first <= ceiling,
            "50% jitter must stay within its bound: {first:?} > {ceiling:?}"
        );

        let other_seed = ChannelGovernor::new(ChannelPolicy {
            jitter: Some(JitterPolicy::new(50, 7)),
            ..base
        });
        let differs = (2..10).any(|attempt| {
            other_seed.backoff_for_attempt(attempt)
                != ChannelGovernor::new(jittered.clone()).backoff_for_attempt(attempt)
        });
        assert!(differs, "different seeds must spread differently");
    }
}
