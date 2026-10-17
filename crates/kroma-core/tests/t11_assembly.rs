//! T11 (KR-08, KR-33): one assembly path for every door, and accounting that cannot invent a gain.
//!
//! The claim being tested is narrow and checkable: for one resolved configuration, the CLI, RPC,
//! bench, worker and resume doors must produce the *same* plan (same budget, policy, execution
//! boundary, feature eligibility), and a mechanism that is configured but cannot run must be a
//! configuration error at start rather than a run that quietly does nothing.

use kroma_core::assembly::{
    eligible_features, AssemblyError, Feature, FeatureLedger, PlanEnvironment, RunPlan, Surface,
};
use kroma_core::config_file::{ConfigOverrides, KromaConfigFile, ResolvedConfig};
use std::path::PathBuf;

fn resolve(user_toml: &str, workspace_toml: &str) -> ResolvedConfig {
    let user = KromaConfigFile::parse_str(user_toml).expect("user config");
    let workspace = KromaConfigFile::parse_str(workspace_toml).expect("workspace config");
    ResolvedConfig::resolve(
        &user,
        &workspace,
        &Default::default(),
        &ConfigOverrides::default(),
        None,
        PathBuf::from("/tmp/ws/.kroma.toml"),
    )
}

/// The matrix the task card names: every door, one digest.
#[test]
fn feature_matrix_covers_cli_rpc_bench_worker_resume() {
    let config = resolve(
        "model = \"m\"\nmax_tokens = 200000\nmax_subagents = 4\npolicy = \"workspace_write\"\nprojection_profile = \"p2\"\nexecution_sandbox = \"bwrap\"\nadaptive_tournament = 2\n",
        "",
    );

    let mut digests = std::collections::BTreeSet::new();
    let mut eligible_sets = std::collections::BTreeSet::new();
    for surface in Surface::all() {
        let plan = RunPlan::from_resolved(&config, surface).expect("plan");
        assert_eq!(plan.surface, surface, "the plan must record its door");
        digests.insert(plan.digest());
        eligible_sets.insert(
            eligible_features(&plan)
                .into_iter()
                .map(Feature::as_str)
                .collect::<Vec<_>>()
                .join(","),
        );
    }
    assert_eq!(
        digests.len(),
        1,
        "the same configuration produced {} different plans across surfaces",
        digests.len()
    );
    assert_eq!(
        eligible_sets.len(),
        1,
        "surfaces disagreed on which features are eligible"
    );
    let eligible = eligible_sets.into_iter().next().unwrap();
    assert!(
        eligible.contains("counterfactual_arm")
            && eligible.contains("context_compaction")
            && eligible.contains("snapshot_store"),
        "expected the configured mechanisms to be eligible, got {eligible}"
    );
}

/// Same name, sharper claim: policy fields specifically must not differ per surface.
#[test]
fn same_run_plan_has_same_policy_across_all_surfaces() {
    let config = resolve(
        "policy = \"read_only\"\nverify_command = \"cargo test\"\nexecution_sandbox = \"bwrap\"\nmax_tokens = 5000\n",
        "",
    );
    let baseline = RunPlan::from_resolved(&config, Surface::Cli).expect("plan");
    for surface in [
        Surface::Rpc,
        Surface::Bench,
        Surface::Worker,
        Surface::Resume,
    ] {
        let plan = RunPlan::from_resolved(&config, surface).expect("plan");
        assert_eq!(
            plan.policy, baseline.policy,
            "policy differs for {surface:?}"
        );
        assert_eq!(
            plan.budget, baseline.budget,
            "budget differs for {surface:?}"
        );
        assert_eq!(
            plan.digest(),
            baseline.digest(),
            "digest differs for {surface:?}"
        );
    }

    // The digest must actually depend on the configuration, or the equality above is vacuous.
    let other = resolve("policy = \"read_only\"\nmax_tokens = 5001\n", "");
    assert_ne!(
        RunPlan::from_resolved(&other, Surface::Cli)
            .unwrap()
            .digest(),
        baseline.digest(),
        "a different max_tokens must change the plan digest"
    );
}

/// A mechanism whose dependency is missing fails at configuration time.
#[test]
fn missing_feature_dependency_fails_at_start() {
    // Counterfactual arm on the host: it would race candidates outside any boundary.
    let config = resolve("adaptive_tournament = 2\n", "");
    match RunPlan::from_resolved(&config, Surface::Cli) {
        Err(AssemblyError::MissingDependency { feature, requires }) => {
            assert_eq!(feature, Feature::CounterfactualArm);
            assert!(
                requires.contains("isolated execution backend"),
                "unexpected requirement: {requires}"
            );
        }
        other => panic!("expected a missing-dependency error, got {other:?}"),
    }

    // The same configuration with an isolated backend is accepted.
    let ok = resolve(
        "adaptive_tournament = 2\nexecution_sandbox = \"bwrap\"\n",
        "",
    );
    assert!(RunPlan::from_resolved(&ok, Surface::Cli).is_ok());

    // An unknown projection profile is a configuration error, not a silent baseline. The file parser
    // already refuses `p9`, so this mutates the resolved value directly: the plan is a second,
    // independent gate (a caller can build a `ResolvedConfig` without going through the parser).
    let mut bad = resolve("", "");
    bad.projection_profile = Some("p9".to_string());
    match RunPlan::from_resolved(&bad, Surface::Cli) {
        Err(AssemblyError::InvalidValue { key, .. }) => assert_eq!(key, "projection_profile"),
        other => panic!("expected an invalid-value error, got {other:?}"),
    }
}

/// The counterfactual arm also needs the snapshot store it probes against.
#[test]
fn missing_snapshot_dependency_is_configuration_error() {
    let config = resolve(
        "adaptive_tournament = 3\nexecution_sandbox = \"bwrap\"\n",
        "",
    );
    let env = PlanEnvironment {
        snapshot_store_available: false,
        experience_file: None,
    };
    match RunPlan::from_resolved_with_env(&config, Surface::Bench, &env) {
        Err(AssemblyError::MissingDependency { feature, requires }) => {
            assert_eq!(feature, Feature::CounterfactualArm);
            assert_eq!(requires, "the revision snapshot store");
        }
        other => panic!("expected a snapshot-dependency error, got {other:?}"),
    }
    // With the store available the same configuration assembles.
    assert!(
        RunPlan::from_resolved_with_env(&config, Surface::Bench, &PlanEnvironment::default())
            .is_ok()
    );

    // Experience is only eligible when an operator file was actually supplied.
    let with_file = PlanEnvironment {
        snapshot_store_available: true,
        experience_file: Some("/etc/kroma/experience.json".to_string()),
    };
    let plan = RunPlan::from_resolved_with_env(&config, Surface::Cli, &with_file).unwrap();
    assert!(plan.features[&Feature::Experience].eligible);
    let without = RunPlan::from_resolved(&config, Surface::Cli).unwrap();
    assert!(!without.features[&Feature::Experience].eligible);
}

/// Configured is not executed. A report may only present `executed` counts as a gain.
#[test]
fn zero_eligible_events_is_reported_not_invented_gain() {
    let mut ledger = FeatureLedger::new();
    // Configured, never eligible: e.g. a counterfactual arm whose candidates were all rejected
    // before any probe ran.
    ledger.record_configured(Feature::CounterfactualArm);
    ledger.record_skipped(
        Feature::CounterfactualArm,
        "no candidate reached the probe stage",
    );

    assert!(
        ledger.headline_gains().is_empty(),
        "a configured-but-unexecuted feature must not appear as a gain: {:?}",
        ledger.headline_gains()
    );
    let zero = ledger.zero_execution_report();
    assert_eq!(zero.len(), 1);
    assert_eq!(zero[0].0, Feature::CounterfactualArm);
    assert_eq!(
        zero[0]
            .1
            .skipped
            .get("no candidate reached the probe stage"),
        Some(&1)
    );
    assert!(ledger.summary().contains("executed=0"));

    // Eligible and executed is a gain; eligible and skipped is still not.
    ledger.record_eligible(Feature::ContextCompaction);
    ledger.record_executed(Feature::ContextCompaction);
    ledger.record_eligible(Feature::Experience);
    ledger.record_skipped(Feature::Experience, "operator file rejected at import");
    let gains = ledger.headline_gains();
    assert_eq!(gains, vec![(Feature::ContextCompaction, 1)]);
    assert!(
        !gains.iter().any(|(f, _)| *f == Feature::Experience),
        "an eligible-but-skipped feature is not a gain"
    );
    // executed can never exceed eligible in a well-formed ledger, and the counters keep them apart.
    let compaction = ledger.counters(Feature::ContextCompaction).unwrap();
    assert_eq!(
        (
            compaction.configured,
            compaction.eligible,
            compaction.executed
        ),
        (0, 1, 1)
    );
}
