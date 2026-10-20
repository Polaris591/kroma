//! C00-09: the gateway every nested call has to pass, and the bounds it enforces.
//!
//! Kroma has no script runtime yet, and the blueprint is explicit that a full JS VM is a later
//! decision rather than a requirement. What it does require is that *whatever* runs a script
//! cannot reach a tool except through the same authorization, budget, cancellation and audit path
//! a direct call uses. This module is that chokepoint's accounting half: depth, call count, result
//! bytes, wall clock and cancellation.
//!
//! Two deliberate choices:
//!
//! * A call that starts before cancellation keeps its result; a call that starts after it is
//!   refused. Silently dropping a callback that already began would hide work that may have had a
//!   side effect.
//! * An oversized result is an error, never a silent truncation: a truncated tool result looks
//!   like a complete one to the model, which is worse than a failure.

use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NestedLimits {
    /// How deep nested calls may go. Depth 0 is the top-level call the model made.
    pub max_depth: usize,
    /// Total nested calls allowed for the run.
    pub max_calls: usize,
    /// Largest single result a nested call may return.
    pub max_result_bytes: usize,
    /// Wall clock allowed for the whole script.
    pub deadline: Duration,
    /// How many nested calls may be in flight at once.
    pub max_concurrent: usize,
    /// CPU seconds the kernel allows the nested program before it is killed. A script that spins must
    /// stop at a limit, not at whatever the wall clock happens to catch.
    pub max_cpu_seconds: u64,
    /// Address space the nested program may allocate, in bytes. Enforced by the kernel, so a runaway
    /// allocation is killed rather than allowed to pressure the host.
    pub max_memory_bytes: u64,
}

impl NestedLimits {
    /// The limits one run gets: the tool-call allowance is the budget's, depth is one (a nested call
    /// may not itself nest), and the kernel bounds are fixed so a runaway program is killed rather
    /// than allowed to pressure the host.
    pub fn for_run(budget: &crate::budget::BudgetConfig) -> NestedLimits {
        NestedLimits {
            max_depth: 1,
            max_calls: budget.max_tool_calls.max(1),
            max_result_bytes: 64 * 1024,
            deadline: budget.max_run_duration,
            max_concurrent: 1,
            max_cpu_seconds: 30,
            max_memory_bytes: 1024 * 1024 * 1024,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum NestedError {
    DepthExceeded {
        depth: usize,
        max_depth: usize,
    },
    CallBudgetExhausted {
        max_calls: usize,
    },
    ResultTooLarge {
        bytes: usize,
        max_result_bytes: usize,
    },
    DeadlineExceeded,
    ConcurrencyExceeded {
        max_concurrent: usize,
    },
    /// The script was cancelled and this call started afterwards.
    Cancelled,
}

pub struct NestedGateway {
    limits: NestedLimits,
    /// A mutex rather than a cell: the gateway is shared by the whole run, and the codemode executor
    /// calls it from a task that is not guaranteed to be on the same thread.
    state: std::sync::Mutex<State>,
}

struct State {
    started: Instant,
    calls: usize,
    in_flight: usize,
    cancelled: bool,
}

impl NestedGateway {
    pub fn new(limits: NestedLimits) -> Self {
        Self {
            limits,
            state: std::sync::Mutex::new(State {
                started: Instant::now(),
                calls: 0,
                in_flight: 0,
                cancelled: false,
            }),
        }
    }

    /// Stop accepting new nested calls. Calls already in flight keep their tickets.
    pub fn cancel(&self) {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .cancelled = true;
    }

    pub fn is_cancelled(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .cancelled
    }

    pub fn calls_started(&self) -> usize {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).calls
    }

    pub fn in_flight(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .in_flight
    }

    /// Admit one nested call and return its ticket id.
    ///
    /// This is the form an executor needs: the codemode path admits a call, awaits the program, then
    /// settles it, and cannot hold a borrow of the gateway across that await.
    pub fn begin(&self, depth: usize) -> Result<u64, NestedError> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        Self::admit(&self.limits, &mut state, depth)
    }

    /// Settle a call admitted by [`Self::begin`], rejecting a result over the bound.
    pub fn settle(&self, id: u64, bytes: usize) -> Result<(), NestedError> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let _ = id;
        state.in_flight = state.in_flight.saturating_sub(1);
        drop(state);
        if bytes > self.limits.max_result_bytes {
            return Err(NestedError::ResultTooLarge {
                bytes,
                max_result_bytes: self.limits.max_result_bytes,
            });
        }
        Ok(())
    }

    /// Admit one nested call at `depth`.
    ///
    /// Checks run in the order that matters: cancellation first (nothing new starts), then depth,
    /// then the call allowance, then concurrency, then the deadline.
    pub fn begin_call(&self, depth: usize) -> Result<CallTicket<'_>, NestedError> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let id = Self::admit(&self.limits, &mut state, depth)?;
        drop(state);
        Ok(CallTicket { gateway: self, id })
    }

    /// The admission rules, in the order that matters: cancellation first (nothing new starts), then
    /// depth, then the call allowance, then concurrency, then the deadline.
    fn admit(limits: &NestedLimits, state: &mut State, depth: usize) -> Result<u64, NestedError> {
        if state.cancelled {
            return Err(NestedError::Cancelled);
        }
        if depth > limits.max_depth {
            return Err(NestedError::DepthExceeded {
                depth,
                max_depth: limits.max_depth,
            });
        }
        if state.calls >= limits.max_calls {
            return Err(NestedError::CallBudgetExhausted {
                max_calls: limits.max_calls,
            });
        }
        if state.in_flight >= limits.max_concurrent {
            return Err(NestedError::ConcurrencyExceeded {
                max_concurrent: limits.max_concurrent,
            });
        }
        if state.started.elapsed() > limits.deadline {
            return Err(NestedError::DeadlineExceeded);
        }
        state.calls += 1;
        state.in_flight += 1;
        Ok(state.calls as u64)
    }
}

/// One admitted nested call. Finishing it is explicit so the in-flight count cannot leak.
pub struct CallTicket<'a> {
    gateway: &'a NestedGateway,
    id: u64,
}

impl CallTicket<'_> {
    /// Complete the call with a result of `bytes` bytes.
    pub fn finish(self, bytes: usize) -> Result<(), NestedError> {
        self.gateway.settle(self.id, bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits() -> NestedLimits {
        NestedLimits {
            max_depth: 3,
            max_calls: 4,
            max_result_bytes: 1024,
            deadline: Duration::from_secs(5),
            max_concurrent: 2,
            max_cpu_seconds: 30,
            max_memory_bytes: 1024 * 1024 * 1024,
        }
    }

    /// C00-09: depth is bounded, and the refusal names the limit it hit.
    #[test]
    fn nesting_depth_is_bounded() {
        let gateway = NestedGateway::new(limits());
        for depth in 0..=3 {
            let ticket = gateway.begin_call(depth).expect("depth within the bound");
            ticket.finish(10).expect("small result");
        }
        assert_eq!(
            gateway.begin_call(4).err(),
            Some(NestedError::DepthExceeded {
                depth: 4,
                max_depth: 3
            })
        );
    }

    /// The call allowance and the concurrency allowance are separate limits: finishing calls does
    /// not restore the total budget, but it does free a concurrency slot.
    #[test]
    fn call_allowance_and_concurrency_are_enforced_separately() {
        let gateway = NestedGateway::new(limits());
        let first = gateway.begin_call(0).expect("first");
        let second = gateway.begin_call(0).expect("second");
        assert_eq!(
            gateway.begin_call(0).err(),
            Some(NestedError::ConcurrencyExceeded { max_concurrent: 2 })
        );
        first.finish(1).unwrap();
        second.finish(1).unwrap();

        let third = gateway.begin_call(0).expect("third");
        let fourth = gateway.begin_call(0).expect("fourth");
        third.finish(1).unwrap();
        fourth.finish(1).unwrap();
        assert_eq!(gateway.calls_started(), 4);
        assert_eq!(
            gateway.begin_call(0).err(),
            Some(NestedError::CallBudgetExhausted { max_calls: 4 }),
            "the total allowance is not restored by finishing"
        );
    }

    /// An oversized result is refused rather than truncated, and the slot is still released.
    #[test]
    fn oversized_results_fail_instead_of_truncating() {
        let gateway = NestedGateway::new(limits());
        let ticket = gateway.begin_call(1).expect("admitted");
        assert_eq!(
            ticket.finish(1025),
            Err(NestedError::ResultTooLarge {
                bytes: 1025,
                max_result_bytes: 1024
            })
        );
        assert_eq!(
            gateway.in_flight(),
            0,
            "a refused result must not leak an in-flight slot"
        );
        let ticket = gateway.begin_call(1).expect("slot released");
        assert_eq!(ticket.finish(1024), Ok(()), "exactly the bound is allowed");
    }

    /// C00-09: after cancellation no new nested call starts, and a call already in flight is
    /// still allowed to finish so its side effect is accounted for.
    #[test]
    fn cancellation_refuses_new_calls_but_lets_in_flight_ones_finish() {
        let gateway = NestedGateway::new(limits());
        let in_flight = gateway.begin_call(0).expect("admitted before cancellation");
        gateway.cancel();
        assert_eq!(gateway.begin_call(1).err(), Some(NestedError::Cancelled));
        assert_eq!(
            in_flight.finish(10),
            Ok(()),
            "work that already started keeps its result instead of being silently dropped"
        );
        assert_eq!(gateway.in_flight(), 0);
    }

    /// The deadline applies to the whole script, not to each call.
    #[test]
    fn the_deadline_covers_the_whole_script() {
        let gateway = NestedGateway::new(NestedLimits {
            deadline: Duration::from_millis(20),
            ..limits()
        });
        std::thread::sleep(Duration::from_millis(30));
        assert_eq!(
            gateway.begin_call(0).err(),
            Some(NestedError::DeadlineExceeded)
        );
    }
}
