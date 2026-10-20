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

/// C00-03: an unset `projection_profile` means the runtime default (`p2`), not `baseline`.
///
/// The plan has to describe the profile the run will actually use, so an unset value must produce
/// the *same* plan — same eligibility, same profile name, same digest — as an explicit `p2`.
/// `baseline` still turns compaction off, and `p1`/`p3` keep their own identity.
#[test]
fn unset_projection_profile_is_the_default_p2() {
    let unset = resolve("", "");
    assert!(
        unset.projection_profile.is_none(),
        "the fixture must leave projection_profile unset"
    );
    let implicit = RunPlan::from_resolved(&unset, Surface::Cli).expect("plan");
    let explicit =
        RunPlan::from_resolved(&resolve("projection_profile = \"p2\"\n", ""), Surface::Cli)
            .expect("plan");

    assert_eq!(
        implicit.projection_profile, explicit.projection_profile,
        "an unset profile must resolve to the same profile as an explicit p2"
    );
    assert_eq!(
        implicit.features[&Feature::ContextCompaction],
        explicit.features[&Feature::ContextCompaction],
        "an unset profile must be exactly as eligible for compaction as an explicit p2"
    );
    assert!(
        implicit.features[&Feature::ContextCompaction].eligible,
        "the runtime default (p2) compacts, so the plan must say so"
    );
    assert_eq!(
        implicit.digest(),
        explicit.digest(),
        "unset and an explicit default p2 are one plan; the digest must not disagree"
    );

    // The same plan written differently (case/whitespace) is still the same plan.
    let shouting = RunPlan::from_resolved(
        &resolve("projection_profile = \" P2 \"\n", ""),
        Surface::Cli,
    )
    .expect("plan");
    assert_eq!(shouting.digest(), explicit.digest());

    // `baseline` still disables compaction, and the reason names it.
    let baseline = RunPlan::from_resolved(
        &resolve("projection_profile = \"baseline\"\n", ""),
        Surface::Cli,
    )
    .expect("plan");
    assert!(!baseline.features[&Feature::ContextCompaction].eligible);
    assert!(
        baseline.features[&Feature::ContextCompaction]
            .reason
            .contains("baseline"),
        "the reason must name the profile that disabled compaction: {:?}",
        baseline.features[&Feature::ContextCompaction].reason
    );
    assert_ne!(baseline.digest(), implicit.digest());

    // `p1` and `p3` both compact but stay distinct plans.
    let mut seen = std::collections::BTreeSet::new();
    for name in ["p1", "p3"] {
        let plan = RunPlan::from_resolved(
            &resolve(&format!("projection_profile = \"{name}\"\n"), ""),
            Surface::Cli,
        )
        .expect("plan");
        assert!(
            plan.features[&Feature::ContextCompaction].eligible,
            "{name} compacts and must be eligible"
        );
        assert_eq!(plan.projection_profile.as_deref(), Some(name));
        assert_ne!(
            plan.digest(),
            explicit.digest(),
            "{name} must not collapse into the p2 plan"
        );
        seen.insert(plan.digest());
    }
    assert_eq!(seen.len(), 2, "p1 and p3 must remain distinct plans");
}

/// C00-03: a new projection algorithm cannot be smuggled in by overloading `p1`/`p2`/`p3`.
///
/// The only profile IDs that resolve to a legacy profile are the four legacy names. The IDs the
/// capability contract reserves for future algorithms are rejected by the config parser *and* by the
/// plan gate (a `ResolvedConfig` can also come from `KROMA_PROJECTION_PROFILE`, which the parser
/// never sees), so adding one is a visible change rather than a silent reinterpretation of an old
/// profile.
#[test]
fn reserved_new_policy_ids_are_rejected_not_aliased_to_legacy_profiles() {
    // M3-02 shipped an algorithm for `ctx.taskstate.v1`, so it is no longer reserved; the other
    // two still are.
    KromaConfigFile::parse_str("projection_profile = \"ctx.taskstate.v1\"\n")
        .expect("an implemented candidate parses");

    // `CAPABILITY_CONTRACT.json` → `reserved_new_policy_ids`.
    for id in ["ctx.semantic-extractive.v1", "ctx.guideline-optimized.v1"] {
        let parse_err = KromaConfigFile::parse_str(&format!("projection_profile = \"{id}\"\n"))
            .expect_err("the config parser must refuse a reserved policy id");
        assert!(
            parse_err.contains(id),
            "the parser error must name the rejected id {id}: {parse_err}"
        );

        let mut config = resolve("", "");
        config.projection_profile = Some(id.to_string());
        match RunPlan::from_resolved(&config, Surface::Cli) {
            Err(AssemblyError::UnimplementedCandidate { id: refused }) => assert_eq!(refused, id),
            other => panic!("{id} must be rejected, got {other:?}"),
        }
    }

    // No reserved id becomes a plan: an unset value still resolves to the documented default, so a
    // future algorithm arrives as an explicit new name, not as the new meaning of `p2`.
    let unset = RunPlan::from_resolved(&resolve("", ""), Surface::Cli).expect("plan");
    assert_eq!(unset.projection_profile.as_deref(), Some("p2"));
}

/// C00-03 / M3-02: a `ctx.*` candidate is resolved in its own namespace on every door.
///
/// Two failure modes are pinned here. Reporting a declared candidate as "unknown" hides the
/// difference between a typo and an algorithm that is contract-reserved but not shipped yet; and
/// resolving an implemented candidate to `p2` would start a run whose report names an algorithm it
/// never ran. `ctx.taskstate.v1` has an algorithm now, so it assembles and keeps its own name; the
/// other two are still refused by name.
#[test]
fn projection_candidates_are_named_and_unimplemented_ones_are_refused() {
    // `CAPABILITY_CONTRACT.json` → `reserved_new_policy_ids`, the same three the projector declares.
    for surface in Surface::all() {
        let mut config = resolve("", "");
        config.projection_profile = Some("ctx.taskstate.v1".to_string());
        let plan = RunPlan::from_resolved(&config, surface)
            .unwrap_or_else(|e| panic!("ctx.taskstate.v1 must assemble on {surface:?}: {e}"));
        assert_eq!(
            plan.projection_profile.as_deref(),
            Some("ctx.taskstate.v1"),
            "the plan must name the candidate it will run, not the profile it composes onto"
        );
    }

    for id in ["ctx.semantic-extractive.v1", "ctx.guideline-optimized.v1"] {
        for surface in Surface::all() {
            let mut config = resolve("", "");
            config.projection_profile = Some(id.to_string());
            let error = RunPlan::from_resolved(&config, surface)
                .err()
                .unwrap_or_else(|| panic!("{id} must not assemble a plan on {surface:?}"));
            let message = error.to_string();
            assert!(
                message.contains(id),
                "the refusal must name the configured candidate {id}: {message}"
            );
            assert!(
                message.contains("not implemented"),
                "the refusal must say the candidate has no implementation, not merely that it is \
                 unexpected: {message}"
            );
            assert!(
                !message.contains("unknown"),
                "a declared candidate must not be reported as an unknown id: {message}"
            );
        }
    }

    // An id in no namespace is still refused as unknown, so the two refusals stay distinguishable.
    let mut typo = resolve("", "");
    typo.projection_profile = Some("ctx.taskstate.v9".to_string());
    let message = RunPlan::from_resolved(&typo, Surface::Cli)
        .expect_err("an undeclared ctx.* id must be refused")
        .to_string();
    assert!(
        message.contains("ctx.taskstate.v9") && message.contains("unknown"),
        "an undeclared id must be reported as unknown and named: {message}"
    );
}

/// C00-03: the same normalized configuration is one plan, whichever layer supplied it and whichever
/// door assembled it.
///
/// `projection_profile` is writable from the user global file, from a workspace `.kroma.toml` and
/// from `KROMA_PROJECTION_PROFILE` / a programmatic override, and the three keep their raw spelling
/// (`"P2"`, `" p2 "`, `"p2"`). The plan has to canonicalise all of them — and an unset value, which
/// the runtime reads as the default `p2` — into one digest, or "the same config" would mean five
/// different capability sets depending on the door.
#[test]
fn same_normalized_config_is_one_plan_across_layers_and_surfaces() {
    let from_user_file = resolve("projection_profile = \"P2\"\n", "");
    let from_workspace_file = resolve("", "projection_profile = \" p2 \"\n");
    let from_override = ResolvedConfig::resolve(
        &KromaConfigFile::parse_str("").expect("user config"),
        &KromaConfigFile::parse_str("").expect("workspace config"),
        &Default::default(),
        &ConfigOverrides {
            projection_profile: Some("P2".to_string()),
            ..Default::default()
        },
        None,
        PathBuf::from("/tmp/ws/.kroma.toml"),
    );
    let unset = resolve("", "");
    assert!(
        unset.projection_profile.is_none(),
        "fixture must leave it unset"
    );
    // The layers keep the raw spelling; canonicalising it is the plan's job, not theirs.
    assert_eq!(from_user_file.projection_profile.as_deref(), Some("P2"));
    assert_eq!(
        from_workspace_file.projection_profile.as_deref(),
        Some(" p2 ")
    );

    let mut digests = std::collections::BTreeSet::new();
    for (layer, config) in [
        ("user file", &from_user_file),
        ("workspace file", &from_workspace_file),
        ("override", &from_override),
        ("unset", &unset),
    ] {
        for surface in Surface::all() {
            let plan = RunPlan::from_resolved(config, surface)
                .unwrap_or_else(|e| panic!("{layer}/{surface:?} must assemble: {e}"));
            assert_eq!(
                plan.projection_profile.as_deref(),
                Some("p2"),
                "{layer}/{surface:?} must canonicalise to the one default profile"
            );
            assert!(
                plan.features[&Feature::ContextCompaction].eligible,
                "{layer}/{surface:?} must plan the default profile's compaction"
            );
            digests.insert(plan.digest());
        }
    }
    assert_eq!(
        digests.len(),
        1,
        "one normalized configuration produced {} different plans",
        digests.len()
    );
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
