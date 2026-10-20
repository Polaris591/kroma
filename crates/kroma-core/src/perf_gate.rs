//! C00-12: a performance claim may not become a default without a calibrated budget.
//!
//! The blueprint's rule is narrow and worth restating: *"未标定可做实现，不可默认晋升或宣称极致"*.
//! So this module refuses to answer "is it fast enough" until the answer has a declared subject —
//! workload, metric, method, noise and tolerance — and it treats a missing budget as a refusal
//! rather than as "no limit".
//!
//! The verdict has three outcomes on purpose. `WithinTolerance` and `Promoted` are different
//! claims, and neither is `Refused`. A gate that only knew "pass/fail" would let a regression that
//! is inside the noise band look like an improvement.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The fields a performance budget must carry before any promotion question is answerable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PerformanceBudget {
    /// The workload this budget is about (P0..P7, plus the concrete command or scenario).
    pub workload: String,
    /// The single primary metric the budget constrains.
    pub primary_metric: String,
    /// Absolute ceiling for that metric.
    pub absolute_limit: u64,
    /// Allowed relative regression against the baseline, in basis points (100 = 1%).
    pub degradation_basis_points: u32,
    /// How the number is obtained (command, sampling interval, process-tree accounting).
    pub measurement_method: String,
    /// Sample count and ordering, so a median is not mistaken for a distribution.
    pub sample_plan: String,
    /// How noise was measured, and what counts as "no change".
    pub noise_basis: String,
    /// Who approved this threshold. A number nobody signed is a number somebody guessed.
    #[serde(default)]
    pub approved_by: String,
    /// Where the number came from: the run, date and evidence path that produced it.
    #[serde(default)]
    pub source: String,
    /// When an exception to this budget expires; an exception without an end is not an exception.
    pub exception_deadline: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum BudgetError {
    Empty {
        field: &'static str,
    },
    /// A zero tolerance means any change is a regression, which is not a tolerance.
    ZeroTolerance,
}

impl PerformanceBudget {
    pub fn validate(&self) -> Result<(), BudgetError> {
        // C00-12: a threshold with no approver and no source is not a threshold, it is a guess that
        // happens to be written down.
        if self.approved_by.trim().is_empty() {
            return Err(BudgetError::Empty {
                field: "approved_by",
            });
        }
        if self.source.trim().is_empty() {
            return Err(BudgetError::Empty { field: "source" });
        }
        let fields: [(&'static str, &str); 5] = [
            ("workload", &self.workload),
            ("primary_metric", &self.primary_metric),
            ("measurement_method", &self.measurement_method),
            ("sample_plan", &self.sample_plan),
            ("noise_basis", &self.noise_basis),
        ];
        if let Some((field, _)) = fields.iter().find(|(_, value)| value.trim().is_empty()) {
            return Err(BudgetError::Empty { field });
        }
        if self.absolute_limit == 0 {
            return Err(BudgetError::Empty {
                field: "absolute_limit",
            });
        }
        if self.degradation_basis_points == 0 {
            return Err(BudgetError::ZeroTolerance);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// The samples were too spread out to tell the candidate from the baseline. This is a result:
    /// it says "no evidence either way", which is what noise-dominated measurements are allowed to
    /// say. It is deliberately not `WithinTolerance`, because that would claim equality.
    Inconclusive {
        spread_basis_points: u32,
        tolerance_basis_points: u32,
    },
    /// Better than the baseline by more than the tolerance.
    Promoted {
        improvement_basis_points: i64,
    },
    /// Different, but inside the declared tolerance: no claim either way.
    WithinTolerance {
        delta_basis_points: i64,
    },
    Refused(Refusal),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// No budget, so no question to answer.
    NoBudget,
    Uncalibrated {
        field: &'static str,
    },
    /// Above the absolute ceiling, regardless of how it compares to the baseline.
    AbsoluteLimitExceeded {
        measured: u64,
        limit: u64,
    },
    Regressed {
        delta_basis_points: i64,
        allowed_basis_points: u32,
    },
}

/// One measurement of the primary metric.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Measurement {
    pub metric: String,
    pub value: u64,
    /// Per-sample values, kept so a caller can see the spread the median hides.
    pub samples: Vec<u64>,
}

impl Measurement {
    pub fn new(metric: impl Into<String>, samples: Vec<u64>) -> Self {
        let mut ordered = samples.clone();
        ordered.sort_unstable();
        let value = if ordered.is_empty() {
            0
        } else {
            ordered[ordered.len() / 2]
        };
        Self {
            metric: metric.into(),
            value,
            samples,
        }
    }
}

impl Measurement {
    /// How wide the samples are relative to their median, in basis points. Zero for a single sample:
    /// one number cannot be noisy, and pretending otherwise would make every single-run comparison
    /// inconclusive.
    pub fn spread_basis_points(&self) -> u32 {
        let (Some(min), Some(max)) = (self.samples.iter().min(), self.samples.iter().max()) else {
            return 0;
        };
        let median = {
            let mut sorted = self.samples.clone();
            sorted.sort_unstable();
            sorted[sorted.len() / 2]
        };
        if median == 0 {
            return u32::MAX;
        }
        (((max - min) * 10_000) / median).min(u32::MAX as u64) as u32
    }
}

/// Decide whether a candidate may become the default.
///
/// `baseline` is the value the candidate is compared against. A candidate with no baseline cannot
/// be promoted, only reported.
pub fn promote(
    budget: Option<&PerformanceBudget>,
    baseline: Option<u64>,
    candidate: &Measurement,
) -> Verdict {
    let Some(budget) = budget else {
        return Verdict::Refused(Refusal::NoBudget);
    };
    if let Err(error) = budget.validate() {
        let field = match error {
            BudgetError::Empty { field } => field,
            BudgetError::ZeroTolerance => "degradation_basis_points",
        };
        return Verdict::Refused(Refusal::Uncalibrated { field });
    }
    if candidate.metric != budget.primary_metric {
        return Verdict::Refused(Refusal::Uncalibrated {
            field: "primary_metric",
        });
    }
    if candidate.value > budget.absolute_limit {
        return Verdict::Refused(Refusal::AbsoluteLimitExceeded {
            measured: candidate.value,
            limit: budget.absolute_limit,
        });
    }
    let Some(baseline) = baseline else {
        return Verdict::Refused(Refusal::NoBudget);
    };
    if baseline == 0 {
        return Verdict::Refused(Refusal::Uncalibrated { field: "baseline" });
    }

    // Positive delta means "slower than baseline" for time-like metrics.
    let delta = ((candidate.value as i64 - baseline as i64) * 10_000) / baseline as i64;
    let allowed = budget.degradation_basis_points as i64;
    if delta > allowed {
        return Verdict::Refused(Refusal::Regressed {
            delta_basis_points: delta,
            allowed_basis_points: budget.degradation_basis_points,
        });
    }
    // C00-12: before comparing, ask whether the measurement can distinguish anything at all. The
    // spread of the candidate's own samples is the cheapest honest proxy: when it is wider than the
    // tolerance, a difference inside that tolerance is not observable.
    let spread = candidate.spread_basis_points();
    if spread > budget.degradation_basis_points {
        return Verdict::Inconclusive {
            spread_basis_points: spread,
            tolerance_basis_points: budget.degradation_basis_points,
        };
    }
    if delta < -allowed {
        return Verdict::Promoted {
            improvement_basis_points: -delta,
        };
    }
    Verdict::WithinTolerance {
        delta_basis_points: delta,
    }
}

/// Group measurements by workload, which is how a report should be read: a metric name alone does
/// not say what was measured.
pub fn by_workload(measurements: Vec<(String, Measurement)>) -> BTreeMap<String, Measurement> {
    measurements.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn budget() -> PerformanceBudget {
        PerformanceBudget {
            workload: "P2 workspace.graph over this tree".to_string(),
            primary_metric: "graph_ms".to_string(),
            absolute_limit: 500,
            degradation_basis_points: 500,
            measurement_method: "kroma acp + workspace.graph, 3 warm repeats, 10ms /proc sampling"
                .to_string(),
            sample_plan: "3 warm repeats, fixed order, median reported with min/max".to_string(),
            noise_basis: "observed run-to-run spread of 12% on an idle host".to_string(),
            exception_deadline: None,
            approved_by: "owner (fixture)".to_string(),
            source: "evidence/fixture".to_string(),
        }
    }

    fn measurement(value: u64) -> Measurement {
        Measurement::new("graph_ms", vec![value, value, value])
    }

    /// C00-12-A1: an incomplete budget is refused by name, including the case of "tolerance 0",
    /// which is not a tolerance at all.
    #[test]
    fn an_uncalibrated_budget_is_refused_by_field() {
        let mut incomplete = budget();
        incomplete.measurement_method = "  ".to_string();
        assert_eq!(
            promote(Some(&incomplete), Some(100), &measurement(100)),
            Verdict::Refused(Refusal::Uncalibrated {
                field: "measurement_method"
            })
        );

        let mut zero = budget();
        zero.degradation_basis_points = 0;
        assert_eq!(
            promote(Some(&zero), Some(100), &measurement(100)),
            Verdict::Refused(Refusal::Uncalibrated {
                field: "degradation_basis_points"
            })
        );

        let mut no_noise = budget();
        no_noise.noise_basis = String::new();
        assert_eq!(
            promote(Some(&no_noise), Some(100), &measurement(100)),
            Verdict::Refused(Refusal::Uncalibrated {
                field: "noise_basis"
            })
        );
    }

    /// C00-12-A2: without a budget there is no question to answer, so nothing is promoted by
    /// default.
    #[test]
    fn no_budget_and_no_baseline_promote_nothing() {
        assert_eq!(
            promote(None, Some(100), &measurement(80)),
            Verdict::Refused(Refusal::NoBudget)
        );
        assert_eq!(
            promote(Some(&budget()), None, &measurement(80)),
            Verdict::Refused(Refusal::NoBudget)
        );
        assert_eq!(
            promote(Some(&budget()), Some(0), &measurement(80)),
            Verdict::Refused(Refusal::Uncalibrated { field: "baseline" })
        );
    }

    /// C00-12-A3 (negative): a regression beyond tolerance is refused, one inside it makes no
    /// claim, and only a real improvement is promoted. An absolute ceiling refuses regardless of
    /// the baseline.
    #[test]
    fn regression_and_tolerance_are_distinguished() {
        assert_eq!(
            promote(Some(&budget()), Some(100), &measurement(120)),
            Verdict::Refused(Refusal::Regressed {
                delta_basis_points: 2000,
                allowed_basis_points: 500,
            })
        );
        assert_eq!(
            promote(Some(&budget()), Some(100), &measurement(102)),
            Verdict::WithinTolerance {
                delta_basis_points: 200
            }
        );
        assert_eq!(
            promote(Some(&budget()), Some(100), &measurement(90)),
            Verdict::Promoted {
                improvement_basis_points: 1000
            }
        );
        assert_eq!(
            promote(Some(&budget()), Some(100), &measurement(600)),
            Verdict::Refused(Refusal::AbsoluteLimitExceeded {
                measured: 600,
                limit: 500
            }),
            "the absolute ceiling applies even when the baseline is worse"
        );
    }
}
