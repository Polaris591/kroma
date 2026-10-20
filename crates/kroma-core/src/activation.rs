//! C00-04: activation leases — one initialisation, shared by everyone who needs it.
//!
//! A capability that is "enabled" but never initialised, or initialised twice, is the failure
//! mode this module exists to remove. The rules it enforces are the ones the blueprint lists
//! (§5.4): single-flight initialisation, dependency order, a lease per holder so a shared
//! dependency is not torn down while someone else still uses it, and rollback that leaves no
//! lease behind when initialisation fails.
//!
//! Authority can also be withdrawn: [`ActivationRegistry::revoke`] advances a monotonic generation,
//! and every lease and claim issued before that generation is dead from then on. A holder that keeps
//! its authority across many calls (the tool dispatch gate) therefore asks the registry instead of
//! remembering a boolean, and an initialisation that was in flight when the revoke landed reports
//! [`ActivationError::Revoked`] rather than success.
//!
//! It deliberately owns no budget and no process: a lease is a refcount plus a state, not a
//! second authority over resources. Everything here is synchronous, so the invariant is readable
//! in one place.

use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// What an activation can be doing right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivationState {
    /// Never initialised, or fully released.
    Dormant,
    /// One caller is running the initialiser; others wait for its result.
    Starting,
    /// Ready and held by at least one lease.
    Active,
    /// Initialisation failed; the failure is reported to every later caller.
    Failed,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ActivationError {
    Unknown(String),
    Cycle(Vec<String>),
    InitFailed {
        id: String,
        reason: String,
    },
    /// C00-04: a revocation landed while this attempt was initialising, so the authority it would
    /// have issued was already stale. The attempt reports this instead of success.
    Revoked {
        id: String,
        generation: u64,
    },
}

pub type InitFn = Arc<dyn Fn() -> Result<(), String> + Send + Sync>;

struct Entry {
    deps: Vec<String>,
    init: InitFn,
    state: ActivationState,
    holders: usize,
    failure: Option<String>,
    /// C00-04: the registry generation at which this capability was revoked, if it ever was. An
    /// authority issued at a generation older than this one is dead; one issued at this generation
    /// or later is a fresh grant and stays usable.
    revoked_at: Option<u64>,
}

#[derive(Default)]
struct Inner {
    entries: HashMap<String, Entry>,
    /// Leases handed out minus leases dropped. A lease whose owner never drops it stays visible
    /// here, which is what makes release checkable instead of "Drop was probably called".
    outstanding: usize,
    /// C00-04: the revocation epoch. Monotonic, bumped once per [`ActivationRegistry::revoke`],
    /// and stamped on every lease and claim the registry issues.
    generation: u64,
}

pub struct ActivationRegistry {
    inner: Mutex<Inner>,
    settled: Condvar,
}

impl Default for ActivationRegistry {
    fn default() -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
            settled: Condvar::new(),
        }
    }
}

impl ActivationRegistry {
    /// Declare a capability and its dependencies. Declaring twice replaces the initialiser and
    /// keeps the current state, which is what a reload of the same capability needs.
    pub fn declare(&self, id: &str, deps: &[&str], init: InitFn) {
        let mut inner = self.inner.lock().expect("activation registry lock");
        let entry = inner
            .entries
            .entry(id.to_string())
            .or_insert_with(|| Entry {
                deps: Vec::new(),
                init: Arc::clone(&init),
                state: ActivationState::Dormant,
                holders: 0,
                failure: None,
                revoked_at: None,
            });
        entry.deps = deps.iter().map(|dep| dep.to_string()).collect();
        entry.init = init;
    }

    pub fn state(&self, id: &str) -> Option<ActivationState> {
        self.inner
            .lock()
            .expect("activation registry lock")
            .entries
            .get(id)
            .map(|entry| entry.state)
    }

    /// Leases that exist right now and have not been dropped. Zero after every lease is gone.
    pub fn outstanding_leases(&self) -> usize {
        self.inner
            .lock()
            .expect("activation registry lock")
            .outstanding
    }

    /// C00-05-A1: the release probe. Reports what the registry actually holds, so "released" is
    /// a measurement rather than an assumption about destructors.
    pub fn probe_released(&self, id: &str) -> ReleaseProbe {
        let inner = self.inner.lock().expect("activation registry lock");
        match inner.entries.get(id) {
            None => ReleaseProbe {
                known: false,
                state: None,
                holders: 0,
                released: false,
            },
            Some(entry) => ReleaseProbe {
                known: true,
                state: Some(entry.state),
                holders: entry.holders,
                released: entry.holders == 0 && entry.state != ActivationState::Active,
            },
        }
    }

    pub fn holders(&self, id: &str) -> Option<usize> {
        self.inner
            .lock()
            .expect("activation registry lock")
            .entries
            .get(id)
            .map(|entry| entry.holders)
    }

    /// The revocation epoch as it stands now. Bumped by every [`ActivationRegistry::revoke`].
    pub fn generation(&self) -> u64 {
        self.inner
            .lock()
            .expect("activation registry lock")
            .generation
    }

    /// C00-04: revoke `id` and every capability that depends on it, transitively — a lease chain
    /// that reached a capability through it loses that authority too. Returns the revoked ids,
    /// `id` first and the dependents in ASCII order; an unknown id revokes nothing and returns an
    /// empty list without advancing the epoch.
    ///
    /// Revocation ends authority, it does not tear anything down: holders are dropped to zero (the
    /// capability is no longer active) and a recorded failure is cleared, so the capability must be
    /// initialised afresh at the new generation. Leases already issued keep their old generation and
    /// therefore stay dead even after that re-activation; an initialisation that is in flight when
    /// the revoke lands reports [`ActivationError::Revoked`] instead of success.
    pub fn revoke(&self, id: &str) -> Vec<String> {
        let mut inner = self.inner.lock().expect("activation registry lock");
        if !inner.entries.contains_key(id) {
            return Vec::new();
        }

        let mut dependents: Vec<String> = Vec::new();
        let mut frontier = vec![id.to_string()];
        while let Some(revoked) = frontier.pop() {
            for (name, entry) in inner.entries.iter() {
                if entry.deps.iter().any(|dep| dep == &revoked) && !dependents.contains(name) {
                    dependents.push(name.clone());
                    frontier.push(name.clone());
                }
            }
        }
        dependents.sort();
        let mut revoked = vec![id.to_string()];
        revoked.extend(dependents);

        let generation = inner.generation + 1;
        inner.generation = generation;
        for revoked_id in &revoked {
            if let Some(entry) = inner.entries.get_mut(revoked_id) {
                entry.revoked_at = Some(generation);
                entry.holders = 0;
                entry.failure = None;
                // An attempt that is initialising right now owns the `Starting` state: leaving it
                // there keeps waiting callers parked until that attempt settles, so a revoke can
                // never make two initialisers run at once.
                if entry.state != ActivationState::Starting {
                    entry.state = ActivationState::Dormant;
                }
            }
        }
        self.settled.notify_all();
        revoked
    }

    /// C00-04: whether `claim` still carries dispatch authority. `Ok` means the capability it names
    /// has not been revoked since the claim was issued; `Err` names the capability and the epoch
    /// that ended it.
    pub fn authority(&self, claim: &CapabilityClaim) -> Result<(), Revoked> {
        let inner = self.inner.lock().expect("activation registry lock");
        match inner.entries.get(&claim.capability) {
            // There is no undeclare, so a claim minted by a lease always names a live entry; an
            // entry that is gone has no epoch left to violate.
            None => Ok(()),
            Some(entry) => match entry.revoked_at {
                Some(revoked_at) if revoked_at > claim.generation => Err(Revoked {
                    capability: claim.capability.clone(),
                    issued: claim.generation,
                    revoked_at,
                }),
                _ => Ok(()),
            },
        }
    }

    /// Take a lease, initialising on first use.
    pub fn activate(&self, id: &str) -> Result<ActivationLease<'_>, ActivationError> {
        let (held, generation) = self.acquire(id, &mut Vec::new())?;
        self.inner
            .lock()
            .expect("activation registry lock")
            .outstanding += 1;
        Ok(ActivationLease {
            registry: self,
            held,
            generation,
        })
    }

    /// Acquire `id` and everything it depends on, returning every id this call now holds.
    ///
    /// The returned ids are the lease: the caller releases them when it drops, which is why a
    /// dependency stays active while any parent still holds it.
    fn acquire(
        &self,
        id: &str,
        path: &mut Vec<String>,
    ) -> Result<(Vec<String>, u64), ActivationError> {
        if path.iter().any(|seen| seen == id) {
            let mut cycle = path.clone();
            cycle.push(id.to_string());
            return Err(ActivationError::Cycle(cycle));
        }

        let mut inner = self.inner.lock().expect("activation registry lock");
        loop {
            let entry = inner
                .entries
                .get_mut(id)
                .ok_or_else(|| ActivationError::Unknown(id.to_string()))?;
            match entry.state {
                ActivationState::Active => {
                    entry.holders += 1;
                    return Ok((vec![id.to_string()], inner.generation));
                }
                ActivationState::Starting => {
                    inner = self.settled.wait(inner).expect("activation registry wait");
                }
                ActivationState::Failed => {
                    return Err(ActivationError::InitFailed {
                        id: id.to_string(),
                        reason: entry
                            .failure
                            .clone()
                            .unwrap_or_else(|| "initialisation failed".to_string()),
                    });
                }
                ActivationState::Dormant => break,
            }
        }

        let (deps, init, attempt) = {
            let entry = inner
                .entries
                .get_mut(id)
                .expect("entry present after state check");
            entry.state = ActivationState::Starting;
            entry.failure = None;
            (
                entry.deps.clone(),
                Arc::clone(&entry.init),
                inner.generation,
            )
        };
        drop(inner);

        // Dependencies first, in declared order. Their ids join this call's lease, so a failed
        // initialisation can release exactly what it took and nothing more.
        path.push(id.to_string());
        let mut held: Vec<String> = vec![id.to_string()];
        let mut outcome: Result<(), ActivationError> = Ok(());
        for dep in &deps {
            match self.acquire(dep, path) {
                Ok((chain, _)) => held.extend(chain),
                Err(error) => {
                    outcome = Err(error);
                    break;
                }
            }
        }
        if outcome.is_ok() {
            outcome = init().map_err(|reason| ActivationError::InitFailed {
                id: id.to_string(),
                reason,
            });
        }
        path.pop();

        let mut inner = self.inner.lock().expect("activation registry lock");
        let entry = inner
            .entries
            .get_mut(id)
            .expect("entry present after initialisation");
        // A revoke that landed while the initialiser was running outranks the result: the authority
        // this attempt would issue belongs to a generation that no longer exists, so reporting
        // success here would be "revoked and still answered".
        if let Some(revoked_at) = entry.revoked_at.filter(|revoked_at| *revoked_at > attempt) {
            entry.state = ActivationState::Dormant;
            entry.holders = 0;
            entry.failure = None;
            self.settled.notify_all();
            drop(inner);
            for held_id in held.iter().filter(|held_id| *held_id != id) {
                self.release(held_id, attempt);
            }
            return Err(ActivationError::Revoked {
                id: id.to_string(),
                generation: revoked_at,
            });
        }
        match outcome {
            Ok(()) => {
                entry.state = ActivationState::Active;
                entry.holders = 1;
                let generation = inner.generation;
                self.settled.notify_all();
                Ok((held, generation))
            }
            Err(error) => {
                entry.state = ActivationState::Failed;
                entry.holders = 0;
                entry.failure = Some(error.describe());
                self.settled.notify_all();
                drop(inner);
                // Rollback: release the dependencies this attempt took, keep the failure.
                for held_id in held.iter().filter(|held_id| *held_id != id) {
                    self.release(held_id, attempt);
                }
                Err(error)
            }
        }
    }

    /// Drop one holder slot, but only if `generation` still owns one: a revoke has already zeroed
    /// the count for the ids it revoked, so a stale lease dropping must not decrement (and thereby
    /// tear down) the holder slots of the generation that replaced it.
    fn release(&self, id: &str, generation: u64) {
        let mut inner = self.inner.lock().expect("activation registry lock");
        if let Some(entry) = inner.entries.get_mut(id) {
            if entry
                .revoked_at
                .is_none_or(|revoked_at| revoked_at <= generation)
            {
                entry.holders = entry.holders.saturating_sub(1);
                if entry.holders == 0 && entry.state == ActivationState::Active {
                    entry.state = ActivationState::Dormant;
                }
            }
        }
        self.settled.notify_all();
    }
}

impl ActivationError {
    /// The message recorded on the failed entry, so a later caller sees why it is not available.
    fn describe(&self) -> String {
        match self {
            ActivationError::Unknown(missing) => format!("unknown dependency `{missing}`"),
            ActivationError::Cycle(cycle) => format!("dependency cycle {}", cycle.join(" -> ")),
            ActivationError::InitFailed { reason, .. } => reason.clone(),
            ActivationError::Revoked { id, generation } => {
                format!("capability `{id}` was revoked at generation {generation} while activating")
            }
        }
    }
}

/// Why an issued authority no longer permits a call. Names the capability, because a refusal that
/// does not say which capability was revoked is not actionable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Revoked {
    capability: String,
    issued: u64,
    revoked_at: u64,
}

impl Revoked {
    pub fn capability(&self) -> &str {
        &self.capability
    }

    /// The generation the authority was issued at.
    pub fn issued(&self) -> u64 {
        self.issued
    }

    /// The generation at which the capability was revoked.
    pub fn revoked_at(&self) -> u64 {
        self.revoked_at
    }

    pub fn describe(&self) -> String {
        format!(
            "capability `{}` was revoked at generation {}; authority issued at generation {} is no longer valid",
            self.capability, self.revoked_at, self.issued
        )
    }
}

impl std::fmt::Display for Revoked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.describe())
    }
}

/// C00-04: the authority a lease carries, as plain data.
///
/// An [`ActivationLease`] borrows the registry and so cannot outlive the call that took it. A caller
/// that has to keep asking "may I still dispatch?" across many later calls — the tool runtime, over
/// the turns of one run — keeps the claim instead. It is not a second grant: it is the capability
/// the lease named plus the generation it was issued at, and [`ActivationRegistry::authority`]
/// decides whether that is still current.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityClaim {
    capability: String,
    generation: u64,
}

impl CapabilityClaim {
    pub fn capability(&self) -> &str {
        &self.capability
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }
}

/// One holder's claim on an activated capability and everything it depends on.
pub struct ActivationLease<'a> {
    registry: &'a ActivationRegistry,
    held: Vec<String>,
    /// The revocation epoch observed when this authority was issued.
    generation: u64,
}

impl std::fmt::Debug for ActivationLease<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "ActivationLease({}, generation {})",
            self.held[0], self.generation
        )
    }
}

impl ActivationLease<'_> {
    pub fn id(&self) -> &str {
        &self.held[0]
    }

    pub fn held(&self) -> &[String] {
        &self.held
    }

    /// The revocation epoch this lease was issued at.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The capability this lease's authority is for, as plain data that can outlive the lease.
    pub fn claim(&self) -> CapabilityClaim {
        CapabilityClaim {
            capability: self.held[0].clone(),
            generation: self.generation,
        }
    }

    /// C00-04: whether this lease may still be used for a new call. Covers everything the lease
    /// holds, so a revoked dependency expires the parent lease too.
    pub fn still_valid(&self) -> Result<(), Revoked> {
        let inner = self
            .registry
            .inner
            .lock()
            .expect("activation registry lock");
        for id in &self.held {
            if let Some(entry) = inner.entries.get(id) {
                if let Some(revoked_at) = entry.revoked_at.filter(|r| *r > self.generation) {
                    return Err(Revoked {
                        capability: id.clone(),
                        issued: self.generation,
                        revoked_at,
                    });
                }
            }
        }
        Ok(())
    }
}

impl Drop for ActivationLease<'_> {
    fn drop(&mut self) {
        for id in &self.held {
            self.registry.release(id, self.generation);
        }
        let mut inner = self
            .registry
            .inner
            .lock()
            .expect("activation registry lock");
        inner.outstanding = inner.outstanding.saturating_sub(1);
    }
}

/// What the registry reports about a capability right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReleaseProbe {
    pub known: bool,
    pub state: Option<ActivationState>,
    pub holders: usize,
    pub released: bool,
}

/// C00-05: how often one run may move between capabilities.
///
/// Three separate brakes, because they prevent three different failures: `min_dwell` stops a
/// capability being torn down before it has done any work, `cooldown` stops Plan↔Simple style
/// oscillation, and `max_switches` bounds the whole run so a policy cannot thrash for an hour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SwitchPolicy {
    pub min_dwell: Duration,
    pub cooldown: Duration,
    pub max_switches: usize,
}

#[derive(Debug, PartialEq, Eq)]
pub enum SwitchError {
    /// The current capability has not been active long enough to be replaced.
    MinDwell {
        remaining_ms: u64,
    },
    /// The previous switch is too recent.
    Cooldown {
        remaining_ms: u64,
    },
    /// The run has spent its switching allowance.
    CapReached {
        max: usize,
    },
    Activation(ActivationError),
}

/// Enforces a [`SwitchPolicy`]. It does not own leases: the caller keeps the old lease alive until
/// the new one is acquired, then drops it, so a failed switch never leaves the run with nothing.
pub struct CapabilityScheduler<'a> {
    registry: &'a ActivationRegistry,
    policy: SwitchPolicy,
    switches: usize,
    last_switch: Option<Instant>,
    current: Option<String>,
}

impl<'a> CapabilityScheduler<'a> {
    pub fn new(registry: &'a ActivationRegistry, policy: SwitchPolicy) -> Self {
        Self {
            registry,
            policy,
            switches: 0,
            last_switch: None,
            current: None,
        }
    }

    pub fn switches(&self) -> usize {
        self.switches
    }

    pub fn current(&self) -> Option<&str> {
        self.current.as_deref()
    }

    /// Move to `id`, returning a lease for it. Re-requesting the current capability is not a
    /// switch and costs nothing.
    pub fn switch_to(&mut self, id: &str) -> Result<ActivationLease<'a>, SwitchError> {
        if self.current.as_deref() == Some(id) {
            return self.registry.activate(id).map_err(SwitchError::Activation);
        }
        if let Some(last) = self.last_switch {
            let elapsed = last.elapsed();
            if elapsed < self.policy.min_dwell {
                return Err(SwitchError::MinDwell {
                    remaining_ms: (self.policy.min_dwell - elapsed).as_millis() as u64,
                });
            }
            if elapsed < self.policy.cooldown {
                return Err(SwitchError::Cooldown {
                    remaining_ms: (self.policy.cooldown - elapsed).as_millis() as u64,
                });
            }
        }
        if self.switches >= self.policy.max_switches {
            return Err(SwitchError::CapReached {
                max: self.policy.max_switches,
            });
        }
        // Acquire before recording: a failed activation must not consume a switch.
        let lease = self
            .registry
            .activate(id)
            .map_err(SwitchError::Activation)?;
        self.switches += 1;
        self.last_switch = Some(Instant::now());
        self.current = Some(id.to_string());
        Ok(lease)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Barrier;
    use std::thread;

    fn counter(init: &Arc<AtomicUsize>) -> InitFn {
        let init = Arc::clone(init);
        Arc::new(move || {
            init.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }

    /// C00-04-A1: eight callers, one initialisation. Everyone else waits for that one result.
    #[test]
    fn initialisation_runs_once_for_concurrent_callers() {
        let registry = Arc::new(ActivationRegistry::default());
        let calls = Arc::new(AtomicUsize::new(0));
        registry.declare("cap", &[], counter(&calls));

        let barrier = Arc::new(Barrier::new(8));
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let registry = Arc::clone(&registry);
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    let lease = registry.activate("cap").expect("activation succeeds");
                    assert_eq!(lease.id(), "cap");
                    thread::sleep(std::time::Duration::from_millis(20));
                })
            })
            .collect();
        for handle in handles {
            handle.join().expect("thread joins");
        }

        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "the initialiser must run once"
        );
        assert_eq!(registry.state("cap"), Some(ActivationState::Dormant));
        assert_eq!(registry.holders("cap"), Some(0));
    }

    /// C00-04-A2: a shared dependency stays active while any holder remains.
    #[test]
    fn a_shared_dependency_outlives_the_first_release() {
        let registry = ActivationRegistry::default();
        let dep_calls = Arc::new(AtomicUsize::new(0));
        registry.declare("dep", &[], counter(&dep_calls));
        registry.declare("a", &["dep"], Arc::new(|| Ok(())));
        registry.declare("b", &["dep"], Arc::new(|| Ok(())));

        let a = registry.activate("a").expect("a activates");
        let b = registry.activate("b").expect("b activates");
        assert_eq!(registry.holders("dep"), Some(2));

        drop(a);
        assert_eq!(
            registry.state("dep"),
            Some(ActivationState::Active),
            "b still holds the dependency"
        );
        drop(b);
        assert_eq!(registry.state("dep"), Some(ActivationState::Dormant));
        assert_eq!(
            dep_calls.load(Ordering::SeqCst),
            1,
            "the dependency initialised once"
        );
    }

    /// C00-04-A3: a failed initialisation leaves no lease behind, and reports the same failure
    /// to later callers instead of retrying silently.
    #[test]
    fn a_failed_activation_rolls_back_and_stays_failed() {
        let registry = ActivationRegistry::default();
        let dep_calls = Arc::new(AtomicUsize::new(0));
        registry.declare("dep", &[], counter(&dep_calls));
        registry.declare("broken", &["dep"], Arc::new(|| Err("boom".to_string())));

        let error = registry
            .activate("broken")
            .expect_err("activation must fail");
        assert_eq!(
            error,
            ActivationError::InitFailed {
                id: "broken".to_string(),
                reason: "boom".to_string()
            }
        );
        assert_eq!(registry.state("broken"), Some(ActivationState::Failed));
        assert_eq!(
            registry.holders("broken"),
            Some(0),
            "a failed attempt holds nothing"
        );
        assert_eq!(
            registry.state("dep"),
            Some(ActivationState::Dormant),
            "the dependency lease taken by the failed attempt is released"
        );
        assert_eq!(registry.holders("dep"), Some(0));

        let again = registry.activate("broken").expect_err("still failed");
        assert_eq!(
            again,
            ActivationError::InitFailed {
                id: "broken".to_string(),
                reason: "boom".to_string()
            }
        );
        assert_eq!(
            dep_calls.load(Ordering::SeqCst),
            1,
            "no silent retry of the initialiser"
        );
    }

    /// A dependency cycle is refused instead of recursing, and an unknown dependency is named.
    #[test]
    fn cycles_and_unknown_dependencies_are_refused() {
        let registry = ActivationRegistry::default();
        registry.declare("a", &["b"], Arc::new(|| Ok(())));
        registry.declare("b", &["a"], Arc::new(|| Ok(())));
        match registry.activate("a") {
            Err(ActivationError::Cycle(cycle)) => assert_eq!(cycle, vec!["a", "b", "a"]),
            other => panic!("expected a cycle error, got {other:?}"),
        }

        let registry = ActivationRegistry::default();
        registry.declare("needs", &["missing"], Arc::new(|| Ok(())));
        let error = registry.activate("needs").expect_err("unknown dependency");
        assert_eq!(
            error,
            ActivationError::Unknown("missing".to_string()),
            "the error names the dependency that does not exist"
        );
        assert_eq!(registry.state("needs"), Some(ActivationState::Failed));
        assert_eq!(registry.holders("needs"), Some(0));
    }

    /// C00-05-A1: release is measured, not assumed. A dropped lease leaves nothing behind; a
    /// lease the caller forgets stays visible.
    #[test]
    fn release_is_measured_by_the_registry_not_by_drop_being_called() {
        let registry = ActivationRegistry::default();
        registry.declare("cap", &[], Arc::new(|| Ok(())));

        let lease = registry.activate("cap").expect("activates");
        assert_eq!(registry.outstanding_leases(), 1);
        assert!(!registry.probe_released("cap").released);
        drop(lease);
        assert_eq!(registry.outstanding_leases(), 0);
        let probe = registry.probe_released("cap");
        assert!(
            probe.released,
            "holders and state must both say released: {probe:?}"
        );
        assert_eq!(probe.holders, 0);
        assert_eq!(probe.state, Some(ActivationState::Dormant));

        // A leaked lease is visible: the probe does not pretend the capability is free.
        let leaked = registry.activate("cap").expect("activates again");
        std::mem::forget(leaked);
        assert_eq!(registry.outstanding_leases(), 1);
        assert!(!registry.probe_released("cap").released);
    }

    /// C00-05-A2: min-dwell and cooldown are separate brakes with separate messages.
    #[test]
    fn dwell_and_cooldown_block_early_switches() {
        let registry = ActivationRegistry::default();
        registry.declare("a", &[], Arc::new(|| Ok(())));
        registry.declare("b", &[], Arc::new(|| Ok(())));

        let mut scheduler = CapabilityScheduler::new(
            &registry,
            SwitchPolicy {
                min_dwell: Duration::from_millis(40),
                cooldown: Duration::from_millis(80),
                max_switches: 10,
            },
        );
        let first = scheduler.switch_to("a").expect("first switch");
        match scheduler.switch_to("b") {
            Err(SwitchError::MinDwell { remaining_ms }) => assert!(remaining_ms > 0),
            other => panic!("expected min-dwell refusal, got {other:?}"),
        }
        std::thread::sleep(Duration::from_millis(50));
        match scheduler.switch_to("b") {
            Err(SwitchError::Cooldown { remaining_ms }) => assert!(remaining_ms > 0),
            other => panic!("expected cooldown refusal, got {other:?}"),
        }
        std::thread::sleep(Duration::from_millis(50));
        let second = scheduler.switch_to("b").expect("switch after both brakes");
        assert_eq!(scheduler.current(), Some("b"));
        assert_eq!(scheduler.switches(), 2);
        drop((first, second));
    }

    /// C00-05-A3: the per-run switch allowance is finite, and a failed switch does not spend one.
    #[test]
    fn the_run_switch_cap_is_enforced() {
        let registry = ActivationRegistry::default();
        for id in ["a", "b", "c", "d", "broken"] {
            let init: InitFn = if id == "broken" {
                Arc::new(|| Err("no".to_string()))
            } else {
                Arc::new(|| Ok(()))
            };
            registry.declare(id, &[], init);
        }
        let mut scheduler = CapabilityScheduler::new(
            &registry,
            SwitchPolicy {
                min_dwell: Duration::ZERO,
                cooldown: Duration::ZERO,
                max_switches: 3,
            },
        );
        let first = scheduler.switch_to("a").expect("first");
        let second = scheduler.switch_to("b").expect("second");

        // A failed activation reports the failure and consumes no allowance.
        match scheduler.switch_to("broken") {
            Err(SwitchError::Activation(ActivationError::InitFailed { id, .. })) => {
                assert_eq!(id, "broken")
            }
            other => panic!("expected the activation failure, got {other:?}"),
        }
        assert_eq!(scheduler.switches(), 2, "a failed switch is not a switch");

        let third = scheduler
            .switch_to("c")
            .expect("third switch fits the allowance");
        assert_eq!(scheduler.switches(), 3);
        match scheduler.switch_to("d") {
            Err(SwitchError::CapReached { max }) => assert_eq!(max, 3),
            other => panic!("expected the cap, got {other:?}"),
        }
        drop((first, second, third));
    }
}
