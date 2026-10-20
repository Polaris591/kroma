//! T17 (KR-12/13/14): a frozen experiment manifest that cannot be quietly reinterpreted.
//!
//! Each test pins one way the old setup could lie: duplicate ids, an empty or unknown holdout, a
//! matrix nobody checked the size of, a lossy job directory, an effective manifest that was never
//! exported, and a development-set win being promoted as if it were a holdout result.

use kroma_core::experiment::{
    promotion_verdict, ArmKind, DecisionBasis, ExperimentManifest, JobId, ManifestError, Promotion,
    EXPERIMENT_MANIFEST_VERSION, MAX_EXPERIMENT_JOBS,
};

fn manifest_json(arms: &str, tasks: &str, extra: &str) -> String {
    format!(
        r#"{{
            "version": {EXPERIMENT_MANIFEST_VERSION},
            "id": "exp-1",
            "arms": {arms},
            "tasks": {tasks},
            "repeats": 2,
            "sandbox": "bwrap",
            "oracle": "cargo test",
            {extra}
            "max_total_tokens": 1000000
        }}"#
    )
}

fn valid_json() -> String {
    manifest_json(
        r#"[
            {"id": "base", "kind": "baseline", "model": "m-a", "price_table": "p1"},
            {"id": "cand", "kind": "candidate", "model": "m-b", "price_table": "p1"}
        ]"#,
        r#"[
            {"id": "t1", "source": "prompt 1"},
            {"id": "t2", "source": "prompt 2"},
            {"id": "t3", "source": "prompt 3"}
        ]"#,
        r#""holdout": ["t3"],"#,
    )
}

#[test]
fn duplicate_task_or_arm_id_fails_before_provider_call() {
    // Duplicate arm id.
    let dup_arm = manifest_json(
        r#"[
            {"id": "base", "kind": "baseline", "model": "m-a", "price_table": "p1"},
            {"id": "base", "kind": "candidate", "model": "m-b", "price_table": "p1"}
        ]"#,
        r#"[{"id": "t1", "source": "s"}]"#,
        "",
    );
    match ExperimentManifest::from_json(&dup_arm) {
        Err(ManifestError::DuplicateArmId(id)) => assert_eq!(id, "base"),
        other => panic!("expected DuplicateArmId, got {other:?}"),
    }

    // Duplicate task id.
    let dup_task = manifest_json(
        r#"[{"id": "base", "kind": "baseline", "model": "m", "price_table": "p"}]"#,
        r#"[{"id": "t1", "source": "a"}, {"id": "t1", "source": "b"}]"#,
        "",
    );
    match ExperimentManifest::from_json(&dup_task) {
        Err(ManifestError::DuplicateTaskId(id)) => assert_eq!(id, "t1"),
        other => panic!("expected DuplicateTaskId, got {other:?}"),
    }
}

/// A declared-but-empty holdout must never be read as "compare on everything".
#[test]
fn declared_but_empty_holdout_never_accepts() {
    let empty_holdout = manifest_json(
        r#"[{"id": "base", "kind": "baseline", "model": "m", "price_table": "p"}]"#,
        r#"[{"id": "t1", "source": "a"}, {"id": "t2", "source": "b"}]"#,
        r#""holdout": [],"#,
    );
    match ExperimentManifest::from_json(&empty_holdout) {
        Err(ManifestError::DeclaredButEmptyHoldout) => {}
        other => panic!("expected DeclaredButEmptyHoldout, got {other:?}"),
    }

    // Omitting the field entirely is the legitimate "no split" case, and it is reported as such.
    let no_holdout = manifest_json(
        r#"[{"id": "base", "kind": "baseline", "model": "m", "price_table": "p"}]"#,
        r#"[{"id": "t1", "source": "a"}, {"id": "t2", "source": "b"}]"#,
        "",
    );
    let resolved = ExperimentManifest::from_json(&no_holdout).expect("valid manifest");
    assert_eq!(resolved.decision_basis, DecisionBasis::AllTasks);
    assert_eq!(resolved.decision_basis.as_str(), "all_tasks");

    // A holdout that covers everything leaves nothing for development.
    let everything = manifest_json(
        r#"[{"id": "base", "kind": "baseline", "model": "m", "price_table": "p"}]"#,
        r#"[{"id": "t1", "source": "a"}, {"id": "t2", "source": "b"}]"#,
        r#""holdout": ["t1", "t2"],"#,
    );
    match ExperimentManifest::from_json(&everything) {
        Err(ManifestError::HoldoutOverlapsEverything) => {}
        other => panic!("expected HoldoutOverlapsEverything, got {other:?}"),
    }
}

#[test]
fn unknown_holdout_id_is_configuration_error() {
    let json = manifest_json(
        r#"[{"id": "base", "kind": "baseline", "model": "m", "price_table": "p"}]"#,
        r#"[{"id": "t1", "source": "a"}, {"id": "t2", "source": "b"}]"#,
        r#""holdout": ["t9"],"#,
    );
    match ExperimentManifest::from_json(&json) {
        Err(ManifestError::UnknownHoldoutId(id)) => assert_eq!(id, "t9"),
        other => panic!("expected UnknownHoldoutId, got {other:?}"),
    }

    // Duplicated holdout ids are also refused: the split size would silently differ from the declared
    // one, which changes the estimator's n.
    let dup = manifest_json(
        r#"[{"id": "base", "kind": "baseline", "model": "m", "price_table": "p"}]"#,
        r#"[{"id": "t1", "source": "a"}, {"id": "t2", "source": "b"}, {"id": "t3", "source": "c"}]"#,
        r#""holdout": ["t3", "t3"],"#,
    );
    match ExperimentManifest::from_json(&dup) {
        Err(ManifestError::DuplicateHoldoutId(id)) => assert_eq!(id, "t3"),
        other => panic!("expected DuplicateHoldoutId, got {other:?}"),
    }
}

#[test]
fn oversized_experiment_matrix_is_refused() {
    // 200 arms x 60 tasks x 2 repeats = 24,000 jobs, over the 10,000 cap.
    let arms: Vec<String> = (0..200)
        .map(|i| {
            let kind = if i == 0 { "baseline" } else { "candidate" };
            format!(r#"{{"id": "a{i}", "kind": "{kind}", "model": "m", "price_table": "p"}}"#)
        })
        .collect();
    let tasks: Vec<String> = (0..60)
        .map(|i| format!(r#"{{"id": "t{i}", "source": "s"}}"#))
        .collect();
    let json = manifest_json(
        &format!("[{}]", arms.join(",")),
        &format!("[{}]", tasks.join(",")),
        "",
    );
    match ExperimentManifest::from_json(&json) {
        Err(ManifestError::MatrixTooLarge { jobs, limit }) => {
            assert_eq!(jobs, 200 * 60 * 2);
            assert_eq!(limit, MAX_EXPERIMENT_JOBS);
        }
        other => panic!("expected MatrixTooLarge, got {other:?}"),
    }

    // A budget below one request per job is a typo, not a budget.
    let tiny_budget = format!(
        r#"{{
            "version": {EXPERIMENT_MANIFEST_VERSION}, "id": "e",
            "arms": [{{"id": "base", "kind": "baseline", "model": "m", "price_table": "p"}}],
            "tasks": [{{"id": "t1", "source": "a"}}, {{"id": "t2", "source": "b"}}],
            "repeats": 4, "oracle": "true", "max_total_tokens": 10
        }}"#
    );
    match ExperimentManifest::from_json(&tiny_budget) {
        Err(ManifestError::BudgetTooSmallForMatrix { required, declared }) => {
            assert_eq!(required, 8 * 256);
            assert_eq!(declared, 10);
        }
        other => panic!("expected BudgetTooSmallForMatrix, got {other:?}"),
    }
}

#[test]
fn effective_manifest_is_exported_and_hashed() {
    let resolved = ExperimentManifest::from_json(&valid_json()).expect("valid manifest");
    // 2 arms x 3 tasks x 2 repeats.
    assert_eq!(resolved.jobs.len(), 12);
    assert_eq!(resolved.decision_basis, DecisionBasis::Holdout);

    let exported = resolved.effective_manifest_json();
    let reparsed = ExperimentManifest::from_json(&exported).expect("the export round-trips");
    assert_eq!(
        reparsed.digest, resolved.digest,
        "the exported manifest must hash to the same digest"
    );
    assert_eq!(reparsed.jobs, resolved.jobs);
    // The digest covers the settings that decide what runs, not just the ids.
    let mut changed = resolved.clone();
    changed.manifest.repeats = 3;
    let changed = changed.manifest.resolve().expect("valid");
    assert_ne!(changed.digest, resolved.digest);
    assert_eq!(resolved.digest.len(), 64);
}

#[test]
fn unknown_or_unused_manifest_field_is_rejected() {
    let typo = manifest_json(
        r#"[{"id": "base", "kind": "baseline", "model": "m", "price_table": "p"}]"#,
        r#"[{"id": "t1", "source": "a"}]"#,
        r#""holdout_tasks": ["t1"],"#,
    );
    match ExperimentManifest::from_json(&typo) {
        Err(ManifestError::Malformed(msg)) => {
            assert!(msg.contains("unknown field"), "unexpected message: {msg}")
        }
        other => panic!("expected Malformed for an unknown field, got {other:?}"),
    }

    // A misspelled key inside an arm is refused for the same reason.
    let bad_arm = manifest_json(
        r#"[{"id": "base", "kind": "baseline", "model": "m", "price_tabel": "p"}]"#,
        r#"[{"id": "t1", "source": "a"}]"#,
        "",
    );
    assert!(matches!(
        ExperimentManifest::from_json(&bad_arm),
        Err(ManifestError::Malformed(_))
    ));

    // An unknown version is refused rather than guessed at.
    let future = valid_json().replace(
        &format!("\"version\": {}", EXPERIMENT_MANIFEST_VERSION),
        "\"version\": 99",
    );
    match ExperimentManifest::from_json(&future) {
        Err(ManifestError::UnsupportedVersion { found, .. }) => assert_eq!(found, 99),
        other => panic!("expected UnsupportedVersion, got {other:?}"),
    }
}

/// Job directories are encoded, not sanitized: two ids that sanitize to the same string stay distinct.
#[test]
fn sanitization_collision_cannot_reuse_workspace() {
    let a = JobId {
        task_id: "a/b".to_string(),
        arm_id: "x".to_string(),
        repetition: 0,
    };
    let b = JobId {
        task_id: "a_b".to_string(),
        arm_id: "x".to_string(),
        repetition: 0,
    };
    assert_ne!(
        a.dir_name(),
        b.dir_name(),
        "two ids that sanitize identically must not share a directory"
    );
    // And the encoding is reversible in the sense that distinct ids never collide at all.
    let mut seen = std::collections::BTreeSet::new();
    for task in ["a/b", "a_b", "a.b", "a b", "a\\b"] {
        for arm in ["cand", "cand_1"] {
            for repetition in 0..3 {
                let job = JobId {
                    task_id: task.to_string(),
                    arm_id: arm.to_string(),
                    repetition,
                };
                assert!(seen.insert(job.dir_name()), "collision for {job:?}");
            }
        }
    }
}

#[test]
fn each_pair_has_exactly_one_baseline_and_candidate() {
    // Two baselines: there is no single reference point.
    let two_baselines = manifest_json(
        r#"[
            {"id": "b1", "kind": "baseline", "model": "m", "price_table": "p"},
            {"id": "b2", "kind": "baseline", "model": "m", "price_table": "p"}
        ]"#,
        r#"[{"id": "t1", "source": "a"}]"#,
        "",
    );
    match ExperimentManifest::from_json(&two_baselines) {
        Err(ManifestError::BaselineCount { found }) => assert_eq!(found, 2),
        other => panic!("expected BaselineCount, got {other:?}"),
    }

    // No baseline at all.
    let no_baseline = manifest_json(
        r#"[{"id": "c1", "kind": "candidate", "model": "m", "price_table": "p"}]"#,
        r#"[{"id": "t1", "source": "a"}]"#,
        "",
    );
    match ExperimentManifest::from_json(&no_baseline) {
        Err(ManifestError::BaselineCount { found }) => assert_eq!(found, 0),
        other => panic!("expected BaselineCount, got {other:?}"),
    }

    // A candidate without a sandbox is refused: it would race on the host.
    let unsandboxed = format!(
        r#"{{
            "version": {EXPERIMENT_MANIFEST_VERSION}, "id": "e",
            "arms": [
                {{"id": "base", "kind": "baseline", "model": "m", "price_table": "p"}},
                {{"id": "cand", "kind": "candidate", "model": "m", "price_table": "p"}}
            ],
            "tasks": [{{"id": "t1", "source": "a"}}],
            "repeats": 1, "oracle": "true"
        }}"#
    );
    assert!(matches!(
        ExperimentManifest::from_json(&unsandboxed),
        Err(ManifestError::MissingSandboxForCandidate)
    ));

    // Missing model / price table are refused, not defaulted.
    let no_model = manifest_json(
        r#"[{"id": "base", "kind": "baseline", "model": "", "price_table": "p"}]"#,
        r#"[{"id": "t1", "source": "a"}]"#,
        "",
    );
    assert!(matches!(
        ExperimentManifest::from_json(&no_model),
        Err(ManifestError::MissingModel(_))
    ));
    let no_price = manifest_json(
        r#"[{"id": "base", "kind": "baseline", "model": "m", "price_table": " "}]"#,
        r#"[{"id": "t1", "source": "a"}]"#,
        "",
    );
    assert!(matches!(
        ExperimentManifest::from_json(&no_price),
        Err(ManifestError::MissingPriceTable(_))
    ));
}

/// A development-set win is not a promotion.
#[test]
fn development_only_gain_does_not_promote() {
    // Great on development, negative on the holdout: reject.
    assert_eq!(
        promotion_verdict(
            DecisionBasis::Holdout,
            Some((0.40, 0.30, 0.50)),
            Some((-0.05, -0.12, 0.02)),
        ),
        Promotion::Reject
    );
    // Great on development, holdout indistinguishable from zero: not a promotion either.
    assert_eq!(
        promotion_verdict(
            DecisionBasis::Holdout,
            Some((0.40, 0.30, 0.50)),
            Some((0.01, -0.02, 0.04)),
        ),
        Promotion::Inconclusive
    );
    // A real holdout win promotes, regardless of development.
    assert_eq!(
        promotion_verdict(
            DecisionBasis::Holdout,
            Some((0.01, -0.01, 0.02)),
            Some((0.08, 0.03, 0.13)),
        ),
        Promotion::Promote
    );
    // No holdout declared: nothing can be promoted.
    assert_eq!(
        promotion_verdict(DecisionBasis::AllTasks, Some((0.5, 0.4, 0.6)), None),
        Promotion::CannotPromoteWithoutHoldout
    );
}

/// The cost/quality split is stated, not implied.
#[test]
fn cost_and_quality_split_basis_is_explicit() {
    let resolved = ExperimentManifest::from_json(&valid_json()).expect("valid manifest");
    assert_eq!(resolved.decision_basis, DecisionBasis::Holdout);
    assert_eq!(resolved.decision_basis.as_str(), "holdout");
    assert!(resolved.is_holdout("t3"));
    assert!(!resolved.is_holdout("t1"));
    // The baseline is identifiable without scanning arms by hand.
    assert_eq!(resolved.baseline_arm().kind, ArmKind::Baseline);
    assert_eq!(resolved.baseline_arm().id, "base");
    // Every job carries an arm and a repetition, so a cost comparison can pair them.
    for job in &resolved.jobs {
        assert!(!job.arm_id.is_empty());
        assert!(job.repetition < resolved.manifest.repeats);
        assert!(resolved.manifest.tasks.iter().any(|t| t.id == job.task_id));
    }
}
