//! C00-12: calibrated performance gates.
//!
//! Three specs, one test each: a threshold with no approver or source is refused; a measurement
//! whose own samples are wider than the tolerance is reported INCONCLUSIVE rather than as
//! non-inferior; and the dormancy tax may not be lowered by deleting a safety capability.

use kroma_core::capability::CapabilityManifest;
use kroma_core::perf_gate::{promote, Measurement, PerformanceBudget, Refusal, Verdict};

fn budget() -> PerformanceBudget {
    PerformanceBudget {
        workload: "P0 startup".to_string(),
        primary_metric: "startup_ms".to_string(),
        absolute_limit: 200,
        degradation_basis_points: 1500,
        measurement_method: "spawn and time to first response".to_string(),
        sample_plan: "5 alternating runs, median".to_string(),
        noise_basis: "within-arm spread measured at 8 027 KB".to_string(),
        exception_deadline: None,
        approved_by: "owner (2026-10-19)".to_string(),
        source: "evidence/e01/exposure-cost.json".to_string(),
    }
}

/// C00-12-A1: a threshold nobody signed and nobody can trace is not a threshold.
#[test]
fn a_threshold_without_an_approver_or_source_is_refused() {
    let mut unsigned = budget();
    unsigned.approved_by = "  ".to_string();
    assert_eq!(
        promote(
            Some(&unsigned),
            Some(100),
            &Measurement::new("startup_ms", vec![100])
        ),
        Verdict::Refused(Refusal::Uncalibrated {
            field: "approved_by"
        })
    );

    let mut unsourced = budget();
    unsourced.source = String::new();
    assert_eq!(
        promote(
            Some(&unsourced),
            Some(100),
            &Measurement::new("startup_ms", vec![100])
        ),
        Verdict::Refused(Refusal::Uncalibrated { field: "source" })
    );

    assert!(
        budget().validate().is_ok(),
        "a signed and sourced budget passes"
    );
}

/// C00-12-A2: noise decides. When the candidate's own samples are wider than the tolerance, the
/// answer is INCONCLUSIVE, never "within tolerance".
#[test]
fn a_noise_dominated_measurement_is_inconclusive_not_non_inferior() {
    let budget = budget();
    // Samples spanning 50-150 ms around a median of 100: 10 000 basis points of spread against a
    // 1 500 basis point tolerance. No comparison here can see a 15% change.
    let noisy = Measurement::new("startup_ms", vec![50, 75, 100, 125, 150]);
    assert_eq!(noisy.spread_basis_points(), 10_000);
    assert_eq!(
        promote(Some(&budget), Some(100), &noisy),
        Verdict::Inconclusive {
            spread_basis_points: 10_000,
            tolerance_basis_points: 1_500
        }
    );

    // A single sample cannot be noisy, and pretending it is would make every one-shot comparison
    // inconclusive.
    let one_shot = Measurement::new("startup_ms", vec![100]);
    assert_eq!(one_shot.spread_basis_points(), 0);
    assert_eq!(
        promote(Some(&budget), Some(100), &one_shot),
        Verdict::WithinTolerance {
            delta_basis_points: 0
        }
    );
}

/// C00-12-A3: the dormancy tax may not be lowered by removing a safety capability.
#[test]
fn a_manifest_may_not_drop_a_safety_capability() {
    let text = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../docs/capabilities.json"
    ))
    .expect("the shipped manifest");
    let manifest = CapabilityManifest::from_json(&text).expect("valid manifest");
    assert!(
        manifest.validate().is_ok(),
        "the shipped manifest keeps every safety capability implemented and wired"
    );
    assert!(
        !manifest.safety_capabilities.is_empty(),
        "a manifest that names no safety capability cannot enforce this rule"
    );

    let mut trimmed = manifest.clone();
    trimmed
        .capabilities
        .retain(|c| c.id != trimmed.safety_capabilities[0]);
    let problem = trimmed.validate().expect_err("dropping one must fail");
    // Mutate BOTH lists: older versions let an editable manifest delete the guard and its
    // self-declared required entry together, bypassing the safety contract.
    trimmed.safety_capabilities.remove(0);
    assert!(
        trimmed.validate().is_err(),
        "two-list deletion cannot weaken the code-owned safety baseline"
    );
    assert!(
        format!("{problem}").contains("may not be lowered by removing"),
        "{problem}"
    );
}

/// C00-12-A4: the shipped thresholds are consumable by the gate, not prose. Every budget in
/// `docs/engineering/PERF_BUDGETS.json` parses, validates, and drives a verdict - so "the threshold
/// has an approver and a source" is checked by the same code that would refuse an unapproved one.
#[test]
fn the_shipped_budgets_are_consumable_by_the_gate() {
    let text = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../docs/engineering/PERF_BUDGETS.json"
    ))
    .expect("the shipped budgets");
    let document: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
    let budgets = document["budgets"].as_array().expect("a budgets array");
    assert!(!budgets.is_empty(), "a gate with no budgets gates nothing");

    for entry in budgets {
        let budget: PerformanceBudget =
            serde_json::from_value(entry.clone()).expect("each budget deserialises");
        budget
            .validate()
            .expect("each shipped budget is signed and sourced");
        let sample = Measurement::new(budget.primary_metric.clone(), vec![1, 1, 1]);
        let verdict = promote(Some(&budget), Some(1), &sample);
        assert!(
            matches!(
                verdict,
                Verdict::WithinTolerance { .. } | Verdict::Promoted { .. }
            ),
            "{}: a stable sample at the baseline must not be refused, got {verdict:?}",
            budget.workload
        );
    }
}
