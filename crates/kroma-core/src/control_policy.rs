//! T27: adaptive execution policy — deterministic escalation, and a counterfactual probe that cannot
//! touch the live workspace.
//!
//! The rule the plan sets: a hard task may escalate **on evidence**, and a simple task must stay on the
//! cheap single-agent path. That rules out "the model feels unsure, so spawn more agents": every action
//! here is a pure function of observable counters, with hard caps, and a model's self-reported
//! confidence is not an input at all.
//!
//! The four actions, in evaluation order:
//!
//! | action | when |
//! | --- | --- |
//! | `Stop` | budget exhausted, the probe cap is reached while failures continue, or the same failure keeps repeating |
//! | `Probe` | enough consecutive failures, budget above the probe floor, probe cap not reached, and an isolated copy is available |
//! | `Inspect` | enough consecutive failures, but probing is not possible or not authorized |
//! | `Continue` | everything else — the single-agent baseline |
//!
//! A `Probe` is only ever a *read* of an isolated copy (see `attribution::CounterfactualProbeOutcome`):
//! it reverts and re-applies inside a throwaway worktree and reports what changed. It never edits the
//! live workspace and never triggers an automatic rollback.

use serde::{Deserialize, Serialize};

/// What the run should do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlAction {
    /// Keep going with the single agent. The default and the cheapest path.
    Continue,
    /// Gather evidence without paying for a probe: look at the failure again, widen the read.
    Inspect,
    /// Run one bounded counterfactual probe inside an isolated copy.
    Probe,
    /// Stop escalating. `StopReason` says why.
    Stop,
}

impl ControlAction {
    pub fn as_str(self) -> &'static str {
        match self {
            ControlAction::Continue => "continue",
            ControlAction::Inspect => "inspect",
            ControlAction::Probe => "probe",
            ControlAction::Stop => "stop",
        }
    }
}

/// Why escalation stopped. Every variant is a fact about the run, not a guess.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// The run's token budget is spent.
    BudgetExhausted,
    /// The probe cap was reached while failures continued.
    ProbeCapReached,
    /// The same failure signature repeated, so more of the same work would not help.
    RepeatedIdenticalFailure,
    /// No isolated copy could be made, so a probe is not allowed.
    IsolationUnavailable,
    /// The operator did not authorize probes (the default).
    ProbeNotAuthorized,
}

impl StopReason {
    pub fn as_str(self) -> &'static str {
        match self {
            StopReason::BudgetExhausted => "budget_exhausted",
            StopReason::ProbeCapReached => "probe_cap_reached",
            StopReason::RepeatedIdenticalFailure => "repeated_identical_failure",
            StopReason::IsolationUnavailable => "isolation_unavailable",
            StopReason::ProbeNotAuthorized => "probe_not_authorized",
        }
    }
}

/// Thresholds. Deliberately small integers: an operator should be able to read the whole policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ControlPolicyConfig {
    /// Consecutive failures before the run starts looking closer.
    pub inspect_after_failures: u32,
    /// Consecutive failures before a counterfactual probe becomes eligible.
    pub probe_after_failures: u32,
    /// Identical failure signatures in a row before escalating is pointless.
    pub stop_after_identical: u32,
    /// Hard cap on probes per run (mirrors `CounterfactualProbeConfig`).
    pub max_probes_per_run: u32,
    /// Tokens that must remain before a probe is worth starting: a probe that exhausts the budget
    /// mid-verification produces no evidence and burns the rest of the run.
    pub min_remaining_tokens_for_probe: u64,
}

impl Default for ControlPolicyConfig {
    fn default() -> Self {
        Self {
            inspect_after_failures: 2,
            probe_after_failures: 3,
            stop_after_identical: 4,
            // Off by default: `max_probes_per_run = 0` means "never probe".
            max_probes_per_run: 0,
            min_remaining_tokens_for_probe: 20_000,
        }
    }
}

impl ControlPolicyConfig {
    /// The only supported way to enable probing: an explicit operator decision with a hard cap.
    pub fn with_probes(max_probes_per_run: u32) -> Self {
        Self {
            max_probes_per_run,
            ..Self::default()
        }
    }
}

/// Everything the decision depends on. No model-reported confidence, no free-form text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ControlObservation {
    pub consecutive_failures: u32,
    /// How many of those failures had the same normalised signature as the most recent one.
    pub identical_failure_streak: u32,
    pub remaining_tokens: u64,
    pub probes_used: u32,
    /// Operator authorization (`--counterfactual-probe` plus an approval seam).
    pub probe_authorized: bool,
    /// Whether an isolated copy can be created. A probe without isolation is not allowed at all.
    pub isolation_available: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ControlDecision {
    pub action: ControlAction,
    pub stop_reason: Option<StopReason>,
}

impl ControlDecision {
    fn new(action: ControlAction) -> Self {
        Self {
            action,
            stop_reason: None,
        }
    }

    fn stop(reason: StopReason) -> Self {
        Self {
            action: ControlAction::Stop,
            stop_reason: Some(reason),
        }
    }
}

/// Decide the next action.
///
/// Pure and exhaustive: the same observation always yields the same decision, which is what makes the
/// escalation auditable after the fact.
pub fn decide(config: &ControlPolicyConfig, obs: &ControlObservation) -> ControlDecision {
    // 1. Budget first: nothing else matters if there is nothing left to spend.
    if obs.remaining_tokens == 0 {
        return ControlDecision::stop(StopReason::BudgetExhausted);
    }

    // 2. Repeating the same failure is not progress, and escalating on it only multiplies cost.
    if obs.consecutive_failures >= config.inspect_after_failures
        && obs.identical_failure_streak >= config.stop_after_identical
    {
        return ControlDecision::stop(StopReason::RepeatedIdenticalFailure);
    }

    // 3. A probe is the expensive, informative action: only when the evidence warrants it.
    if obs.consecutive_failures >= config.probe_after_failures {
        if config.max_probes_per_run == 0 || !obs.probe_authorized {
            return ControlDecision::new(ControlAction::Inspect);
        }
        if obs.probes_used >= config.max_probes_per_run {
            // The cap is a ceiling on cost, not a suggestion: keep going cheaply instead.
            return ControlDecision::new(ControlAction::Inspect);
        }
        if !obs.isolation_available {
            return ControlDecision::stop(StopReason::IsolationUnavailable);
        }
        if obs.remaining_tokens < config.min_remaining_tokens_for_probe {
            // Probing now would spend the rest of the budget on the probe itself.
            return ControlDecision::new(ControlAction::Inspect);
        }
        return ControlDecision::new(ControlAction::Probe);
    }

    // 4. Cheap escalation: look before spending.
    if obs.consecutive_failures >= config.inspect_after_failures {
        return ControlDecision::new(ControlAction::Inspect);
    }

    ControlDecision::new(ControlAction::Continue)
}

/// Whether a run may evaluate multiple candidates at all.
///
/// Multi-candidate work is an **experiment**, not a default: it is enabled only by an explicit
/// experiment configuration, and there is no code path here that turns on an autonomous worker mode.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MultiCandidateGate {
    /// One candidate at a time. The production default.
    #[default]
    SingleCandidate,
    /// Several candidates, enabled by an explicit experimental configuration with a stated cap.
    Experimental { max_candidates: u32 },
}

impl MultiCandidateGate {
    /// The only constructor that produces `Experimental`: a caller must name the cap and the experiment.
    pub fn from_experiment(experiment_id: &str, max_candidates: u32) -> Option<Self> {
        if experiment_id.trim().is_empty() || max_candidates < 2 {
            return None;
        }
        Some(MultiCandidateGate::Experimental { max_candidates })
    }

    pub fn allows_many(self) -> bool {
        matches!(self, MultiCandidateGate::Experimental { .. })
    }

    /// Autonomous worker mode is never enabled by this module, in any configuration.
    pub fn allows_autonomous_work_mode(self) -> bool {
        false
    }
}

/// What one candidate cost and whether it helped, so losers are accounted for too.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CandidateRecord {
    pub candidate_id: String,
    /// Tokens spent producing *and* verifying this candidate, win or lose.
    pub tokens_spent: u64,
    /// `true` only when this candidate resolved the task.
    pub resolved: bool,
    /// How different this candidate is from the others (0..=100), for diversity reporting.
    pub diversity_score: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CandidateSummary {
    pub candidates: usize,
    /// Total spend including every loser: the number that decides whether the experiment was worth it.
    pub total_tokens: u64,
    /// Tokens spent on candidates that did not resolve the task.
    pub wasted_on_losers: u64,
    pub resolved: usize,
    /// Mean pairwise diversity as reported by the producers, not measured by this module.
    pub mean_diversity: u8,
}

impl CandidateSummary {
    pub fn from_records(records: &[CandidateRecord]) -> Self {
        let mut summary = CandidateSummary {
            candidates: records.len(),
            ..Self::default()
        };
        let mut diversity_sum: u32 = 0;
        for record in records {
            summary.total_tokens = summary.total_tokens.saturating_add(record.tokens_spent);
            diversity_sum = diversity_sum.saturating_add(u32::from(record.diversity_score));
            if record.resolved {
                summary.resolved += 1;
            } else {
                summary.wasted_on_losers =
                    summary.wasted_on_losers.saturating_add(record.tokens_spent);
            }
        }
        if !records.is_empty() {
            summary.mean_diversity = (diversity_sum / records.len() as u32) as u8;
        }
        summary
    }

    /// Incremental gain over the single-candidate baseline, in resolved tasks per 100k tokens.
    ///
    /// Reported only when something actually resolved; a run with no result has no rate, and inventing
    /// one is exactly what the plan forbids.
    pub fn resolved_per_100k_tokens(&self) -> Option<f64> {
        if self.resolved == 0 || self.total_tokens == 0 {
            return None;
        }
        Some((self.resolved as f64) * 100_000.0 / (self.total_tokens as f64))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obs(failures: u32, streak: u32, remaining: u64, used: u32) -> ControlObservation {
        ControlObservation {
            consecutive_failures: failures,
            identical_failure_streak: streak,
            remaining_tokens: remaining,
            probes_used: used,
            probe_authorized: true,
            isolation_available: true,
        }
    }

    #[test]
    fn the_single_agent_path_is_the_default() {
        let config = ControlPolicyConfig::default();
        assert_eq!(
            decide(&config, &obs(0, 0, 100_000, 0)).action,
            ControlAction::Continue
        );
        assert_eq!(
            decide(&config, &obs(1, 1, 100_000, 0)).action,
            ControlAction::Continue
        );
        // Two failures pause to look, without spending on a probe.
        assert_eq!(
            decide(&config, &obs(2, 1, 100_000, 0)).action,
            ControlAction::Inspect
        );
        // Probing is disabled by default, so even three failures stay on Inspect.
        assert_eq!(
            decide(&config, &obs(3, 1, 100_000, 0)).action,
            ControlAction::Inspect
        );
    }

    #[test]
    fn probing_requires_an_explicit_cap_and_authorization() {
        let config = ControlPolicyConfig::with_probes(1);
        assert_eq!(
            decide(&config, &obs(3, 1, 100_000, 0)).action,
            ControlAction::Probe
        );
        // Cap reached: back to Inspect, not Stop - the run can still make progress.
        let mut at_cap = obs(4, 1, 100_000, 1);
        assert_eq!(decide(&config, &at_cap).action, ControlAction::Inspect);
        // Not authorized: Inspect.
        at_cap.probe_authorized = false;
        assert_eq!(decide(&config, &at_cap).action, ControlAction::Inspect);
    }

    #[test]
    fn a_probe_never_starts_below_its_budget_floor() {
        let config = ControlPolicyConfig::with_probes(2);
        assert_eq!(
            decide(
                &config,
                &obs(3, 1, config.min_remaining_tokens_for_probe - 1, 0)
            )
            .action,
            ControlAction::Inspect
        );
        assert_eq!(
            decide(
                &config,
                &obs(3, 1, config.min_remaining_tokens_for_probe, 0)
            )
            .action,
            ControlAction::Probe
        );
    }

    #[test]
    fn a_probe_without_isolation_stops_instead_of_touching_the_workspace() {
        let config = ControlPolicyConfig::with_probes(1);
        let mut observation = obs(3, 1, 100_000, 0);
        observation.isolation_available = false;
        let decision = decide(&config, &observation);
        assert_eq!(decision.action, ControlAction::Stop);
        assert_eq!(decision.stop_reason, Some(StopReason::IsolationUnavailable));
    }

    #[test]
    fn repeating_the_same_failure_stops_escalation() {
        let config = ControlPolicyConfig::with_probes(5);
        let decision = decide(&config, &obs(6, 4, 100_000, 0));
        assert_eq!(decision.action, ControlAction::Stop);
        assert_eq!(
            decision.stop_reason,
            Some(StopReason::RepeatedIdenticalFailure)
        );
        // A *different* failure at the same count does not stop: it is new information.
        assert_eq!(
            decide(&config, &obs(6, 1, 100_000, 0)).action,
            ControlAction::Probe
        );
    }

    #[test]
    fn an_exhausted_budget_wins_over_everything_else() {
        let config = ControlPolicyConfig::with_probes(5);
        let decision = decide(&config, &obs(9, 9, 0, 0));
        assert_eq!(decision.action, ControlAction::Stop);
        assert_eq!(decision.stop_reason, Some(StopReason::BudgetExhausted));
    }

    #[test]
    fn multi_candidate_is_experimental_and_never_autonomous() {
        assert_eq!(
            MultiCandidateGate::default(),
            MultiCandidateGate::SingleCandidate
        );
        assert!(!MultiCandidateGate::default().allows_many());

        // An explicit experiment may enable it, with a cap.
        let gate = MultiCandidateGate::from_experiment("EXP-04", 3).expect("explicit experiment");
        assert!(gate.allows_many());
        assert!(!gate.allows_autonomous_work_mode());

        // Missing experiment id or a cap below two is not an experiment.
        assert!(MultiCandidateGate::from_experiment("", 3).is_none());
        assert!(MultiCandidateGate::from_experiment("EXP-04", 1).is_none());
    }

    #[test]
    fn loser_cost_is_part_of_the_total() {
        let records = vec![
            CandidateRecord {
                candidate_id: "a".to_string(),
                tokens_spent: 1_000,
                resolved: true,
                diversity_score: 40,
            },
            CandidateRecord {
                candidate_id: "b".to_string(),
                tokens_spent: 700,
                resolved: false,
                diversity_score: 60,
            },
            CandidateRecord {
                candidate_id: "c".to_string(),
                tokens_spent: 300,
                resolved: false,
                diversity_score: 80,
            },
        ];
        let summary = CandidateSummary::from_records(&records);
        assert_eq!(summary.candidates, 3);
        assert_eq!(summary.resolved, 1);
        assert_eq!(summary.total_tokens, 2_000);
        assert_eq!(summary.wasted_on_losers, 1_000);
        assert_eq!(summary.mean_diversity, 60);
        assert_eq!(summary.resolved_per_100k_tokens(), Some(50.0));

        // No result means no rate, not a zero and not a guess.
        let empty = CandidateSummary::from_records(&[CandidateRecord {
            candidate_id: "a".to_string(),
            tokens_spent: 500,
            resolved: false,
            diversity_score: 10,
        }]);
        assert_eq!(empty.resolved_per_100k_tokens(), None);
    }
}
