//! M2-01 contract: a paid benchmark run is only admitted under a preregistration.
//!
//! These tests cover the gate itself (which rules it enforces, and how it names the one that
//! failed) and the audit trail (the report records the digest of the document that admitted the
//! run, plus whether the suite scored was the holdout). The CLI wiring — `--prereg` being required
//! for `--executor provider` — is covered where the flag lives.

use kroma_core::bench::{
    builtin_smoke_suite, default_strategy_pair, run_paired_benchmark, BenchRunOptions,
    SyntheticBenchExecutor,
};
use kroma_core::prereg::{check_prereg, PreregGate, PreregSuiteRole};
use sha2::Digest;
use std::path::{Path, PathBuf};

fn temp_root(label: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "kroma-prereg-gate-{}-{}",
        label,
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&root).unwrap();
    root
}

fn write_suite(path: &Path, tasks: &str) {
    std::fs::write(path, format!("[{}]", tasks)).unwrap();
}

/// The eight fields `scripts/verify-prereg.py` requires, filled in. Individual tests remove or
/// repoint one of them to prove the gate names that rule.
fn complete_prereg(dev: &str, holdout: &str, verifier: &str) -> serde_json::Value {
    serde_json::json!({
        "format": "kroma.prereg/1",
        "experiment": "E00",
        "status": "PREREGISTERED_NOT_RUN",
        "task_cluster_definition": {
            "cluster": "fixture cluster",
            "independence": "task is the sampling unit",
            "dev_suite": dev,
            "holdout_suite": holdout
        },
        "model_identity_observability": { "requested": "fixture-model" },
        "quality_margin": {
            "primary_metric": "quality_pass_rate",
            "delta": "5 percentage points",
            "rule": "non-inferiority on quality"
        },
        "noise_and_power_plan": { "plan": "12 paired runs", "interval": "95% cluster bootstrap" },
        "cache_order_design": { "order": "randomized within blocks", "warmup": "one discarded" },
        "budget": { "plan": "12 runs, 1.2M tokens" },
        "stopping_rule": "stop at 12 runs",
        "holdout_policy": { "queries": 1, "verifier_path": verifier }
    })
}

/// A workspace with a dev and a holdout suite, a verifier *outside* it, and a preregistration that
/// names all three. Returns (workspace, outside, prereg path).
fn fixture(
    label: &str,
    experiment: impl Fn(&Path, &Path) -> serde_json::Value,
) -> (PathBuf, PathBuf, PathBuf) {
    let workspace = temp_root(label);
    let outside = temp_root(&format!("{}-outside", label));
    std::fs::create_dir_all(workspace.join("experiments")).unwrap();
    write_suite(&workspace.join("experiments/dev.json"), r#"{"id":"dev"}"#);
    write_suite(
        &workspace.join("experiments/holdout.json"),
        r#"{"id":"holdout"}"#,
    );
    std::fs::write(outside.join("verifier.py"), "print('fixture')\n").unwrap();

    let doc = experiment(&workspace, &outside);
    let prereg = workspace.join("prereg.json");
    std::fs::write(&prereg, serde_json::to_vec_pretty(&doc).unwrap()).unwrap();
    (workspace, outside, prereg)
}

/// The shape a real run has: the preregistration records the suite inside the repository, while the
/// run scores a byte-identical copy outside it (a suite inside the graded workspace is refused by
/// the loader). The gate must accept that and label it a holdout run.
#[test]
fn bench_prereg_gate_admits_a_complete_preregistration_on_the_named_holdout() {
    let (workspace, outside, prereg) = fixture("admit", |workspace, outside| {
        complete_prereg(
            workspace.join("experiments/dev.json").to_str().unwrap(),
            workspace.join("experiments/holdout.json").to_str().unwrap(),
            outside.join("verifier.py").to_str().unwrap(),
        )
    });
    let holdout_copy = outside.join("suite-holdout.json");
    std::fs::copy(workspace.join("experiments/holdout.json"), &holdout_copy).unwrap();

    let gate = check_prereg(&prereg, &workspace, Some(&holdout_copy))
        .expect("a complete preregistration on its own holdout must be admitted");
    assert_eq!(gate.suite_role, PreregSuiteRole::Holdout);
    assert_eq!(gate.path, prereg);
    let expected = format!(
        "{:x}",
        sha2::Sha256::digest(std::fs::read(&prereg).unwrap())
    );
    assert_eq!(gate.sha256, expected, "the report carries the file digest");

    // The dev suite is admitted too, and labelled dev: the run is allowed, the claim is not.
    let dev_copy = outside.join("suite-dev.json");
    std::fs::copy(workspace.join("experiments/dev.json"), &dev_copy).unwrap();
    let dev_gate = check_prereg(&prereg, &workspace, Some(&dev_copy)).unwrap();
    assert_eq!(dev_gate.suite_role, PreregSuiteRole::Dev);

    for path in [workspace, outside] {
        let _ = std::fs::remove_dir_all(path);
    }
}

/// Deleting one of the eight fields is the difference between an experiment and a search. The error
/// has to name the field, because "invalid preregistration" tells the operator nothing.
#[test]
fn bench_prereg_gate_names_the_required_field_that_is_missing() {
    let (workspace, outside, prereg) = fixture("missing-field", |workspace, outside| {
        let mut doc = complete_prereg(
            workspace.join("experiments/dev.json").to_str().unwrap(),
            workspace.join("experiments/holdout.json").to_str().unwrap(),
            outside.join("verifier.py").to_str().unwrap(),
        );
        doc.as_object_mut().unwrap().remove("quality_margin");
        doc
    });
    let holdout_copy = outside.join("suite-holdout.json");
    std::fs::copy(workspace.join("experiments/holdout.json"), &holdout_copy).unwrap();

    let problems = check_prereg(&prereg, &workspace, Some(&holdout_copy))
        .expect_err("a preregistration without a quality margin must be refused");
    assert_eq!(
        problems.len(),
        1,
        "one rule broken, one problem: {:?}",
        problems
    );
    assert!(
        problems[0].contains("quality_margin"),
        "the problem must name the field: {}",
        problems[0]
    );

    for path in [workspace, outside] {
        let _ = std::fs::remove_dir_all(path);
    }
}

/// The point of a holdout is that it is the frozen one. A suite that is neither of the two suites
/// the preregistration names is a different experiment wearing the same command line.
#[test]
fn bench_prereg_gate_refuses_a_suite_the_preregistration_does_not_name() {
    let (workspace, outside, prereg) = fixture("unregistered", |workspace, outside| {
        complete_prereg(
            workspace.join("experiments/dev.json").to_str().unwrap(),
            workspace.join("experiments/holdout.json").to_str().unwrap(),
            outside.join("verifier.py").to_str().unwrap(),
        )
    });
    let edited = outside.join("suite-holdout-edited.json");
    write_suite(&edited, r#"{"id":"holdout"},{"id":"extra"}"#);

    let problems = check_prereg(&prereg, &workspace, Some(&edited))
        .expect_err("an unregistered suite must be refused");
    assert_eq!(problems.len(), 1, "{:?}", problems);
    assert!(
        problems[0].contains("neither the preregistered dev suite"),
        "the problem must say which suites were named: {}",
        problems[0]
    );

    // The built-in smoke suite is not a preregistered suite either.
    let problems = check_prereg(&prereg, &workspace, None)
        .expect_err("the built-in suite must be refused for a gated run");
    assert!(
        problems[0].contains("built-in smoke suite"),
        "{:?}",
        problems
    );

    for path in [workspace, outside] {
        let _ = std::fs::remove_dir_all(path);
    }
}

/// A verifier the graded agent can edit is the oracle marking its own homework.
#[test]
fn bench_prereg_gate_refuses_a_verifier_inside_the_workspace() {
    let (workspace, outside, prereg) = fixture("inside-verifier", |workspace, _| {
        let inside = workspace.join("verifier.py");
        std::fs::write(&inside, "print('easy')\n").unwrap();
        complete_prereg(
            workspace.join("experiments/dev.json").to_str().unwrap(),
            workspace.join("experiments/holdout.json").to_str().unwrap(),
            inside.to_str().unwrap(),
        )
    });
    let holdout_copy = outside.join("suite-holdout.json");
    std::fs::copy(workspace.join("experiments/holdout.json"), &holdout_copy).unwrap();

    let problems = check_prereg(&prereg, &workspace, Some(&holdout_copy))
        .expect_err("an in-workspace verifier must be refused");
    assert!(
        problems
            .iter()
            .any(|problem| problem.contains("outside the workspace")),
        "{:?}",
        problems
    );

    for path in [workspace, outside] {
        let _ = std::fs::remove_dir_all(path);
    }
}

/// Dev and holdout pointing at one suite means the confirmatory run re-scores the development set.
#[test]
fn bench_prereg_gate_refuses_dev_and_holdout_naming_one_suite() {
    let (workspace, outside, prereg) = fixture("same-suite", |workspace, outside| {
        let only_path = workspace.join("experiments/holdout.json");
        let only = only_path.to_str().unwrap();
        complete_prereg(only, only, outside.join("verifier.py").to_str().unwrap())
    });
    let holdout_copy = outside.join("suite-holdout.json");
    std::fs::copy(workspace.join("experiments/holdout.json"), &holdout_copy).unwrap();

    let problems = check_prereg(&prereg, &workspace, Some(&holdout_copy))
        .expect_err("one suite cannot be both dev and holdout");
    assert!(
        problems
            .iter()
            .any(|problem| problem.contains("must be different suites")),
        "{:?}",
        problems
    );

    for path in [workspace, outside] {
        let _ = std::fs::remove_dir_all(path);
    }
}

/// The audit trail: a report must say which document admitted the run and whether the numbers came
/// from the holdout. Both facts survive serialization, so a later reader does not have to trust the
/// console line the operator saw at the time.
#[tokio::test]
async fn bench_report_records_the_preregistration_it_was_admitted_under() {
    let root = temp_root("metadata");
    let gate = PreregGate {
        path: PathBuf::from("/fixtures/E02.json"),
        sha256: "a".repeat(64),
        suite_role: PreregSuiteRole::Holdout,
        // R02: this report fixture declares no external oracle; the field is explicit so a missing
        // capability can never be mistaken for a granted one.
        verifier_file: None,
    };
    let options = BenchRunOptions::new(1, 7, false).with_prereg(gate.clone());
    let report = run_paired_benchmark(
        "prereg-metadata",
        &builtin_smoke_suite(),
        &default_strategy_pair(),
        1_000,
        &root.join("scratch"),
        &SyntheticBenchExecutor,
        &options,
    )
    .await;

    assert_eq!(report.metadata.prereg_sha256, Some(gate.sha256.clone()));
    assert_eq!(
        report.metadata.prereg_suite_role.as_deref(),
        Some("holdout")
    );
    let rendered = report.render();
    assert!(
        rendered.contains(&format!("prereg:  {} (holdout suite)", gate.sha256)),
        "{rendered}"
    );
    let serialized = serde_json::to_value(&report).unwrap();
    assert_eq!(
        serialized["metadata"]["prereg_sha256"],
        serde_json::json!(gate.sha256)
    );
    assert_eq!(
        serialized["metadata"]["prereg_suite_role"],
        serde_json::json!("holdout")
    );

    // A run that was not gated (the synthetic plumbing check) records nothing, rather than a
    // placeholder that could be mistaken for an admission.
    let plain = BenchRunOptions::new(1, 7, false);
    let ungated = run_paired_benchmark(
        "prereg-metadata-plain",
        &builtin_smoke_suite(),
        &default_strategy_pair(),
        1_000,
        &root.join("scratch-plain"),
        &SyntheticBenchExecutor,
        &plain,
    )
    .await;
    assert_eq!(ungated.metadata.prereg_sha256, None);
    assert_eq!(ungated.metadata.prereg_suite_role, None);
    assert!(!ungated.render().contains("prereg:"));

    let _ = std::fs::remove_dir_all(&root);
}
