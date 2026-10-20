//! M3-01: routing that cannot oscillate.
//!
//! A router that switches on a raw threshold flaps: the signal sits on the boundary, every turn
//! crosses it, and the run pays the switch cost forever. Three brakes, same as the capability
//! scheduler, plus a rule this module treats as non-negotiable: when confidence is low the router
//! falls back to the simplest strategy instead of guessing an expensive one.
//!
//! The decision is a function of one observation plus the router's own history, so it is testable
//! without a model: `observe` returns the strategy in force and whether this call changed it.

use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strategy {
    Simple,
    Plan,
    Scout,
}

impl Strategy {
    pub fn as_str(self) -> &'static str {
        match self {
            Strategy::Simple => "simple",
            Strategy::Plan => "plan",
            Strategy::Scout => "scout",
        }
    }
}

impl RouteReason {
    pub fn as_str(self) -> &'static str {
        match self {
            RouteReason::Held => "held",
            RouteReason::LowConfidence => "low_confidence",
            RouteReason::MinDwell => "min_dwell",
            RouteReason::SwitchCap => "switch_cap",
            RouteReason::Escalated => "escalated",
            RouteReason::DeEscalated => "de_escalated",
        }
    }
}

/// One routing decision, kept so a run can be audited afterwards instead of trusted.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RouteDecision {
    pub turn: usize,
    pub strategy: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RoutingPolicy {
    /// Complexity above this escalates to `Plan` (only if it is `up_threshold` away from the
    /// current band, which is what makes the pair a hysteresis).
    pub up_threshold: f32,
    /// Complexity below this de-escalates back to `Simple`.
    pub down_threshold: f32,
    /// Below this confidence the router refuses to escalate at all.
    pub low_confidence: f32,
    /// Minimum time a strategy stays in force.
    pub min_dwell: Duration,
    /// Hard cap on switches for one run.
    pub max_switches: usize,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RoutingSignal {
    pub complexity: f32,
    pub confidence: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteReason {
    Held,
    LowConfidence,
    MinDwell,
    SwitchCap,
    Escalated,
    DeEscalated,
}

pub struct Router {
    current: Strategy,
    policy: RoutingPolicy,
    switches: usize,
    last_switch: Option<Instant>,
}

impl Router {
    pub fn new(policy: RoutingPolicy) -> Self {
        Self {
            current: Strategy::Simple,
            policy,
            switches: 0,
            last_switch: None,
        }
    }

    pub fn current(&self) -> Strategy {
        self.current
    }

    pub fn switches(&self) -> usize {
        self.switches
    }

    /// What the router does with one observation.
    ///
    /// Order: low confidence wins over everything (it is the expensive-guess guard), then the
    /// dwell and cap brakes, then the two thresholds.
    pub fn observe(&mut self, signal: RoutingSignal) -> (Strategy, RouteReason) {
        if signal.confidence < self.policy.low_confidence {
            // Falling back to the cheap strategy is still a switch, so it obeys the brakes.
            if self.current == Strategy::Simple {
                return (self.current, RouteReason::LowConfidence);
            }
            return self.try_set(Strategy::Simple, RouteReason::LowConfidence);
        }

        let target = if signal.complexity >= self.policy.up_threshold {
            Strategy::Plan
        } else if signal.complexity < self.policy.down_threshold {
            Strategy::Simple
        } else {
            // Inside the band: hold whatever is in force. This is the hysteresis.
            return (self.current, RouteReason::Held);
        };

        if target == self.current {
            return (self.current, RouteReason::Held);
        }
        let reason = match target {
            Strategy::Plan => RouteReason::Escalated,
            _ => RouteReason::DeEscalated,
        };
        self.try_set(target, reason)
    }

    fn try_set(&mut self, target: Strategy, reason: RouteReason) -> (Strategy, RouteReason) {
        if let Some(last) = self.last_switch {
            if last.elapsed() < self.policy.min_dwell {
                return (self.current, RouteReason::MinDwell);
            }
        }
        if self.switches >= self.policy.max_switches {
            return (self.current, RouteReason::SwitchCap);
        }
        self.current = target;
        self.switches += 1;
        self.last_switch = Some(Instant::now());
        (self.current, reason)
    }
}

impl RoutingPolicy {
    /// The policy a run gets when it only says "route": a wide band, no dwell, three switches.
    /// Chosen so the hysteresis does the damping rather than a timer, which a turn-based loop
    /// cannot honour anyway.
    pub fn default_policy() -> Self {
        Self {
            up_threshold: 0.7,
            down_threshold: 0.3,
            low_confidence: 0.4,
            min_dwell: Duration::ZERO,
            max_switches: 3,
        }
    }
}

/// Turn the turn's own observables into a routing signal.
///
/// Both inputs are measurements, not guesses: `result_bytes` is what the tools actually returned
/// and `tool_errors` counts results that came back as errors. A turn with no tool calls at all is
/// treated as maximum confidence - there is nothing to doubt.
pub fn signal(tool_calls: usize, tool_errors: usize, result_bytes: usize) -> RoutingSignal {
    let confidence = if tool_calls == 0 {
        1.0
    } else {
        (tool_calls - tool_errors.min(tool_calls)) as f32 / tool_calls as f32
    };
    let complexity = (result_bytes as f32 / 16_384.0).min(1.0);
    RoutingSignal {
        complexity,
        confidence,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> RoutingPolicy {
        RoutingPolicy {
            up_threshold: 0.7,
            down_threshold: 0.3,
            low_confidence: 0.4,
            min_dwell: Duration::ZERO,
            max_switches: 10,
        }
    }

    /// M3-01-A1: the band between the thresholds holds the current strategy, so a signal that
    /// wobbles around one number does not flap.
    #[test]
    fn the_band_between_thresholds_holds() {
        let mut router = Router::new(policy());
        assert_eq!(
            router
                .observe(RoutingSignal {
                    complexity: 0.8,
                    confidence: 0.9
                })
                .1,
            RouteReason::Escalated
        );
        assert_eq!(router.current(), Strategy::Plan);

        for complexity in [0.69, 0.65, 0.5, 0.31, 0.7] {
            let (strategy, reason) = router.observe(RoutingSignal {
                complexity,
                confidence: 0.9,
            });
            assert_eq!(
                reason,
                RouteReason::Held,
                "complexity {complexity} is inside the band"
            );
            assert_eq!(strategy, Strategy::Plan);
        }
        assert_eq!(router.switches(), 1, "one escalation, no flapping");

        assert_eq!(
            router
                .observe(RoutingSignal {
                    complexity: 0.1,
                    confidence: 0.9
                })
                .1,
            RouteReason::DeEscalated
        );
        assert_eq!(router.current(), Strategy::Simple);
    }

    /// M3-01-A2: low confidence never escalates - it falls back to the cheapest strategy.
    #[test]
    fn low_confidence_falls_back_instead_of_guessing() {
        let mut router = Router::new(policy());
        router.observe(RoutingSignal {
            complexity: 0.9,
            confidence: 0.9,
        });
        assert_eq!(router.current(), Strategy::Plan);

        let (strategy, reason) = router.observe(RoutingSignal {
            complexity: 0.95,
            confidence: 0.2,
        });
        assert_eq!(reason, RouteReason::LowConfidence);
        assert_eq!(
            strategy,
            Strategy::Simple,
            "an uncertain router takes the cheap path"
        );

        let (strategy, reason) = router.observe(RoutingSignal {
            complexity: 0.99,
            confidence: 0.1,
        });
        assert_eq!(
            (strategy, reason),
            (Strategy::Simple, RouteReason::LowConfidence)
        );
    }

    /// M3-01-A3: dwell and the switch cap are separate brakes, and neither is bypassed by a
    /// low-confidence fallback.
    #[test]
    fn dwell_and_switch_cap_bound_the_run() {
        let mut slow = policy();
        slow.min_dwell = Duration::from_millis(40);
        let mut router = Router::new(slow);
        assert_eq!(
            router
                .observe(RoutingSignal {
                    complexity: 0.9,
                    confidence: 0.9
                })
                .1,
            RouteReason::Escalated
        );

        // Inside the dwell window nothing moves, including the low-confidence fallback.
        assert_eq!(
            router
                .observe(RoutingSignal {
                    complexity: 0.0,
                    confidence: 0.1
                })
                .1,
            RouteReason::MinDwell
        );
        assert_eq!(router.current(), Strategy::Plan);
        std::thread::sleep(Duration::from_millis(45));
        assert_eq!(
            router
                .observe(RoutingSignal {
                    complexity: 0.0,
                    confidence: 0.1
                })
                .1,
            RouteReason::LowConfidence
        );
        assert_eq!(router.current(), Strategy::Simple);

        // A fresh policy with no dwell: this assertion is about the cap and nothing else.
        let mut capped = Router::new(RoutingPolicy {
            max_switches: 1,
            ..policy()
        });
        assert_eq!(
            capped
                .observe(RoutingSignal {
                    complexity: 0.9,
                    confidence: 0.9
                })
                .1,
            RouteReason::Escalated
        );
        assert_eq!(
            capped
                .observe(RoutingSignal {
                    complexity: 0.0,
                    confidence: 0.9
                })
                .1,
            RouteReason::SwitchCap
        );
        assert_eq!(
            capped.current(),
            Strategy::Plan,
            "the cap holds the strategy in force"
        );
    }

    /// E08-A1: the signal is read off measurements, so its edges are pinned: a turn with no tool
    /// calls cannot look uncertain, and a turn whose results exceed 16 KiB is maximally complex.
    #[test]
    fn the_signal_comes_from_measured_counts() {
        let idle = signal(0, 0, 0);
        assert_eq!(idle.confidence, 1.0, "nothing was tried, nothing failed");
        assert_eq!(idle.complexity, 0.0);

        let mostly_fine = signal(4, 1, 8_192);
        assert_eq!(mostly_fine.confidence, 0.75);
        assert_eq!(mostly_fine.complexity, 0.5);

        let huge = signal(2, 2, 100_000);
        assert_eq!(huge.confidence, 0.0);
        assert_eq!(
            huge.complexity, 1.0,
            "complexity saturates, it does not run past 1"
        );
    }

    /// E08-A2: a real run feeds `signal` into `observe` once per turn. Complexity that wobbles
    /// around the up-threshold must produce one switch, not one per turn.
    #[test]
    fn a_wobbling_run_switches_once_not_once_per_turn() {
        let mut router = Router::new(policy());
        let turns = [0.9, 0.5, 0.72, 0.65, 0.95, 0.4];
        let mut switches = 0;
        for (index, complexity) in turns.iter().enumerate() {
            let before = router.switches();
            let (strategy, _) = router.observe(signal(3, 0, (complexity * 16_384.0) as usize));
            if router.switches() != before {
                switches += 1;
                assert!(index < 2, "switched again on turn {index}: {strategy:?}");
            }
        }
        assert_eq!(switches, 1, "the band absorbed the wobble");
        assert_eq!(router.current(), Strategy::Plan);
    }

    /// E08-A3: low confidence beats high complexity. A run that keeps failing its tools must not
    /// escalate on the strength of a large transcript.
    #[test]
    fn low_confidence_refuses_the_expensive_strategy() {
        let mut router = Router::new(policy());
        let (strategy, reason) = router.observe(signal(4, 3, 60_000));
        assert_eq!(strategy, Strategy::Simple);
        assert_eq!(reason, RouteReason::LowConfidence);
        assert_eq!(router.switches(), 0, "refusing to escalate is not a switch");
    }
}
