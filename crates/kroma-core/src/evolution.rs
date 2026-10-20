//! M4-01: a candidate pool with a bill.
//!
//! Searching for a better harness configuration is only honest if the search itself is bounded and
//! paid for. This module keeps the pool, the evaluation allowance and the spend in one place, so a
//! run cannot quietly evaluate a hundred variants and report only the winner's cost.
//!
//! Two refusals matter: a candidate whose configuration duplicates one already in the pool adds
//! nothing but cost, and an evaluation that would exceed the budget is refused *before* it runs.

use std::collections::BTreeSet;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub id: String,
    /// Canonical configuration digest: two candidates with the same digest are the same candidate.
    pub config_digest: String,
    /// Measured quality, once evaluated. `None` means "declared but not yet run".
    pub score: Option<u64>,
    pub cost_micros: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub enum PoolError {
    PoolFull { max_candidates: usize },
    DuplicateConfig { digest: String },
    UnknownCandidate { id: String },
    EvaluationBudgetExhausted { requested: u64, remaining: u64 },
    AlreadyEvaluated { id: String },
}

pub struct CandidatePool {
    max_candidates: usize,
    evaluation_budget_micros: u64,
    spent_micros: u64,
    candidates: Vec<Candidate>,
    digests: BTreeSet<String>,
    /// The baseline is kept even when a candidate beats it: a winner with nothing to compare
    /// against is not a result.
    baseline: Option<String>,
}

impl CandidatePool {
    pub fn new(max_candidates: usize, evaluation_budget_micros: u64) -> Self {
        Self {
            max_candidates,
            evaluation_budget_micros,
            spent_micros: 0,
            candidates: Vec::new(),
            digests: BTreeSet::new(),
            baseline: None,
        }
    }

    pub fn mark_baseline(&mut self, id: &str) -> Result<(), PoolError> {
        if !self.candidates.iter().any(|candidate| candidate.id == id) {
            return Err(PoolError::UnknownCandidate { id: id.to_string() });
        }
        self.baseline = Some(id.to_string());
        Ok(())
    }

    pub fn baseline(&self) -> Option<&str> {
        self.baseline.as_deref()
    }

    pub fn spent_micros(&self) -> u64 {
        self.spent_micros
    }

    pub fn candidates(&self) -> &[Candidate] {
        &self.candidates
    }

    pub fn admit(&mut self, candidate: Candidate) -> Result<(), PoolError> {
        if self.candidates.len() >= self.max_candidates {
            return Err(PoolError::PoolFull {
                max_candidates: self.max_candidates,
            });
        }
        if !self.digests.insert(candidate.config_digest.clone()) {
            return Err(PoolError::DuplicateConfig {
                digest: candidate.config_digest,
            });
        }
        self.candidates.push(candidate);
        Ok(())
    }

    /// Charge one evaluation. Refused before spending when the budget would be exceeded, so a
    /// caller cannot evaluate first and discover the overrun afterwards.
    pub fn charge(&mut self, cost_micros: u64) -> Result<(), PoolError> {
        let remaining = self
            .evaluation_budget_micros
            .saturating_sub(self.spent_micros);
        if cost_micros > remaining {
            return Err(PoolError::EvaluationBudgetExhausted {
                requested: cost_micros,
                remaining,
            });
        }
        self.spent_micros += cost_micros;
        Ok(())
    }

    pub fn record_score(
        &mut self,
        id: &str,
        score: u64,
        cost_micros: u64,
    ) -> Result<(), PoolError> {
        self.charge(cost_micros)?;
        let candidate = self
            .candidates
            .iter_mut()
            .find(|candidate| candidate.id == id)
            .ok_or_else(|| PoolError::UnknownCandidate { id: id.to_string() })?;
        if candidate.score.is_some() {
            return Err(PoolError::AlreadyEvaluated { id: id.to_string() });
        }
        candidate.score = Some(score);
        candidate.cost_micros += cost_micros;
        Ok(())
    }

    /// The best evaluated candidate, ignoring unevaluated ones. Ties keep the earlier admission,
    /// so the result does not depend on iteration order.
    pub fn best(&self) -> Option<&Candidate> {
        self.candidates
            .iter()
            .filter(|candidate| candidate.score.is_some())
            .fold(None, |best: Option<&Candidate>, candidate| match best {
                None => Some(candidate),
                Some(current) if candidate.score > current.score => Some(candidate),
                Some(current) => Some(current),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(id: &str, digest: &str) -> Candidate {
        Candidate {
            id: id.to_string(),
            config_digest: digest.to_string(),
            score: None,
            cost_micros: 0,
        }
    }

    /// M4-01-A1: the pool is bounded and a duplicate configuration is refused before it costs
    /// anything.
    #[test]
    fn the_pool_is_bounded_and_deduplicated_by_configuration() {
        let mut pool = CandidatePool::new(2, 1_000);
        pool.admit(candidate("a", "digest-a")).unwrap();
        assert_eq!(
            pool.admit(candidate("a-again", "digest-a")),
            Err(PoolError::DuplicateConfig {
                digest: "digest-a".to_string()
            })
        );
        pool.admit(candidate("b", "digest-b")).unwrap();
        assert_eq!(
            pool.admit(candidate("c", "digest-c")),
            Err(PoolError::PoolFull { max_candidates: 2 })
        );
    }

    /// M4-01-A2: evaluation is charged before it runs, so an overrun is refused rather than
    /// discovered afterwards.
    #[test]
    fn evaluation_budget_is_checked_before_spending() {
        let mut pool = CandidatePool::new(4, 100);
        pool.admit(candidate("a", "digest-a")).unwrap();
        assert_eq!(
            pool.charge(150),
            Err(PoolError::EvaluationBudgetExhausted {
                requested: 150,
                remaining: 100
            })
        );
        assert_eq!(pool.spent_micros(), 0, "a refused evaluation costs nothing");
        pool.record_score("a", 10, 60).unwrap();
        assert_eq!(pool.spent_micros(), 60);
        assert_eq!(
            pool.charge(50),
            Err(PoolError::EvaluationBudgetExhausted {
                requested: 50,
                remaining: 40
            })
        );
        pool.charge(40).unwrap();
        assert_eq!(pool.spent_micros(), 100);
    }

    /// M4-01-A3: the baseline survives, an unevaluated candidate cannot win, and re-scoring is
    /// refused.
    #[test]
    fn the_baseline_survives_and_only_evaluated_candidates_win() {
        let mut pool = CandidatePool::new(4, 1_000);
        pool.admit(candidate("baseline", "digest-base")).unwrap();
        pool.admit(candidate("candidate", "digest-cand")).unwrap();
        pool.admit(candidate("unrun", "digest-unrun")).unwrap();
        pool.mark_baseline("baseline").unwrap();
        pool.record_score("baseline", 5, 10).unwrap();
        pool.record_score("candidate", 7, 10).unwrap();

        assert_eq!(pool.best().map(|c| c.id.as_str()), Some("candidate"));
        assert_eq!(
            pool.baseline(),
            Some("baseline"),
            "the baseline is kept even when beaten"
        );
        assert_eq!(
            pool.record_score("candidate", 9, 10),
            Err(PoolError::AlreadyEvaluated {
                id: "candidate".to_string()
            })
        );

        let mut unrun_only = CandidatePool::new(4, 100);
        unrun_only.admit(candidate("x", "digest-x")).unwrap();
        assert!(
            unrun_only.best().is_none(),
            "an unevaluated candidate cannot be the winner"
        );
    }
}

/// How a search spends its evaluation allowance. The three arms exist because "we searched" is not a
/// result without a comparison: a fixed order and a random order cost the same and are the controls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionStrategy {
    /// Evaluate in the order the candidates were declared.
    Fixed,
    /// Evaluate in a seeded random order.
    Random,
    /// Screen every candidate cheaply, then evaluate the most promising ones fully.
    Search,
}

/// What one screen and one full evaluation cost. Kept together because a strategy that screens
/// without paying for it is not a search, it is a free oracle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResearchCosts {
    pub screen_micros: u64,
    pub evaluate_micros: u64,
}

/// The measurement a strategy is allowed to use. Passed in, never stored: a candidate cannot reach
/// it, so it cannot rewrite what grades it.
#[derive(Clone, Copy)]
pub struct Oracle<'a> {
    pub screen: &'a dyn Fn(&Candidate) -> u64,
    pub evaluate: &'a dyn Fn(&Candidate) -> u64,
}

#[derive(Debug, PartialEq, Eq)]
pub struct StrategyOutcome {
    pub strategy: SelectionStrategy,
    pub evaluations: usize,
    pub screens: usize,
    /// Every micro spent on screening and evaluation, including the candidates that lost.
    pub spent_micros: u64,
    pub best_score: Option<u64>,
}

/// Run one selection strategy against the same allowance.
///
/// The oracle is a parameter, never part of the pool: a candidate cannot reach it, so it cannot
/// rewrite the thing that grades it or the contract that promotes it. `screen` is the cheap proxy a
/// search strategy may use; `evaluate` is the real measurement. Both are charged, which is what makes
/// "generation, embedding and evaluation all count as research cost" true rather than aspirational.
pub fn run_strategy(
    strategy: SelectionStrategy,
    candidates: &[Candidate],
    budget_micros: u64,
    costs: ResearchCosts,
    oracle: Oracle<'_>,
    seed: u64,
) -> StrategyOutcome {
    let ResearchCosts {
        screen_micros: screen_cost_micros,
        evaluate_micros: evaluate_cost_micros,
    } = costs;
    let mut spent = 0u64;
    let mut evaluations = 0usize;
    let mut screens = 0usize;
    let mut best: Option<u64> = None;

    let mut order: Vec<usize> = (0..candidates.len()).collect();
    if strategy == SelectionStrategy::Random {
        // A tiny deterministic shuffle: the point is a reproducible order, not cryptography.
        let mut state = seed | 1;
        for index in (1..order.len()).rev() {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1);
            order.swap(index, (state >> 33) as usize % (index + 1));
        }
    }
    if strategy == SelectionStrategy::Search {
        let mut screened: Vec<(u64, usize)> = Vec::new();
        for (index, candidate) in candidates.iter().enumerate() {
            if spent + screen_cost_micros > budget_micros {
                break;
            }
            spent += screen_cost_micros;
            screens += 1;
            screened.push(((oracle.screen)(candidate), index));
        }
        screened.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        order = screened.into_iter().map(|(_, index)| index).collect();
    }

    for index in order {
        if spent + evaluate_cost_micros > budget_micros {
            break;
        }
        spent += evaluate_cost_micros;
        evaluations += 1;
        let score = (oracle.evaluate)(&candidates[index]);
        best = Some(best.map_or(score, |current| current.max(score)));
    }

    StrategyOutcome {
        strategy,
        evaluations,
        screens,
        spent_micros: spent,
        best_score: best,
    }
}
