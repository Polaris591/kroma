//! M4-02: experience that is revised and withdrawn, never rewritten, and governed by scope.
//!
//! The store already exists (`kroma_core::experience`); these tests pin the three properties the
//! blueprint requires, so a later change cannot start overwriting evidence or letting a
//! repository-sourced lesson become global knowledge.

use kroma_core::experience::{
    ExperienceError, ExperienceScope, ExperienceSource, ExperienceStatus, ExperienceStore,
    TrustedActor,
};

fn owner() -> TrustedActor {
    TrustedActor::new("owner")
}

/// M4-02-A1: a counterexample attaches to the record instead of replacing its evidence, and
/// revocation keeps the original readable.
#[test]
fn a_counterexample_is_added_not_overwritten() {
    let mut store = ExperienceStore::new();
    let id = store
        .propose(
            "merge-gate",
            ExperienceScope::Repo,
            "merged tree differs from the branch",
            "rerun the gate on the merged tree",
            "gate passed on branch, failed after merge (run-17)",
            "rev-a",
            ExperienceSource::Operator,
        )
        .unwrap();
    store.accept(&id, &owner()).unwrap();
    store
        .with_counterexample(
            &id,
            "a 2026-10 run passed on the merged tree",
            Some(u64::MAX),
        )
        .unwrap();

    let (evidence, counterexample) = {
        let record = store.get(&id).expect("record survives the counterexample");
        (record.evidence.clone(), record.counterexample.clone())
    };
    assert_eq!(
        evidence,
        "gate passed on branch, failed after merge (run-17)"
    );
    assert!(counterexample
        .as_deref()
        .is_some_and(|text| text.contains("passed on the merged tree")));

    store
        .revoke(&id, "counterexample reproduced twice")
        .unwrap();
    let revoked = store.get(&id).expect("a revoked record is not deleted");
    assert_eq!(revoked.status, ExperienceStatus::Revoked);
    assert_eq!(
        revoked.revoked_reason.as_deref(),
        Some("counterexample reproduced twice")
    );
    assert_eq!(
        revoked.evidence, evidence,
        "the original evidence is intact"
    );
}

/// M4-02-A2: scope is governed by where the text came from. A repository lesson stops at repo
/// scope, and a model's guess cannot leave the run that produced it.
#[test]
fn scope_limits_follow_the_source() {
    let mut store = ExperienceStore::new();
    let repo = store
        .propose(
            "repo-lesson",
            ExperienceScope::Repo,
            "the workspace pins an old toolchain",
            "read rust-toolchain.toml first",
            "observed twice",
            "rev-a",
            ExperienceSource::Repository {
                path: "README.md".to_string(),
            },
        )
        .unwrap();
    store.accept(&repo, &owner()).unwrap();
    assert!(
        matches!(
            store.promote(&repo, ExperienceScope::Global, &owner()),
            Err(ExperienceError::UntrustedSource { .. })
        ),
        "repository text cannot become global knowledge"
    );

    let model = store
        .propose(
            "model-lesson",
            ExperienceScope::Task,
            "the failing test looks flaky",
            "retry once",
            "model guess",
            "rev-a",
            ExperienceSource::Model {
                run_id: "run-1".to_string(),
            },
        )
        .unwrap();
    store.accept(&model, &owner()).unwrap();
    assert!(
        matches!(
            store.promote(&model, ExperienceScope::Repo, &owner()),
            Err(ExperienceError::UntrustedSource { .. })
        ),
        "a model's guess cannot become repository knowledge"
    );
}

/// M4-02-A3: retrieval follows the scope ordering in both directions, and revocation hides a
/// record from retrieval without deleting it. Acceptance still requires evidence.
#[test]
fn scope_ordering_revocation_and_evidence() {
    let mut store = ExperienceStore::new();
    let unsupported = store
        .propose(
            "no-evidence",
            ExperienceScope::Repo,
            "p",
            "a",
            "   ",
            "rev-a",
            ExperienceSource::Operator,
        )
        .unwrap();
    assert!(matches!(
        store.accept(&unsupported, &owner()),
        Err(ExperienceError::MissingEvidence(_))
    ));

    let repo = store
        .propose(
            "repo-lesson",
            ExperienceScope::Repo,
            "p",
            "a",
            "evidence",
            "rev-a",
            ExperienceSource::Operator,
        )
        .unwrap();
    let task = store
        .propose(
            "task-lesson",
            ExperienceScope::Task,
            "p",
            "a",
            "evidence",
            "rev-a",
            ExperienceSource::Operator,
        )
        .unwrap();
    let global = store
        .propose(
            "global-lesson",
            ExperienceScope::Global,
            "p",
            "a",
            "evidence",
            "rev-a",
            ExperienceSource::Operator,
        )
        .unwrap();
    for id in [&repo, &task, &global] {
        store.accept(id, &owner()).unwrap();
    }

    let repo_view: Vec<&str> = store
        .retrieve(ExperienceScope::Repo, 10, 0)
        .iter()
        .map(|record| record.key.as_str())
        .collect();
    assert!(repo_view.contains(&"repo-lesson"));
    assert!(
        repo_view.contains(&"task-lesson"),
        "a narrower scope applies inside a wider one"
    );
    assert!(
        !repo_view.contains(&"global-lesson"),
        "a wider scope must not surface inside a narrower one"
    );

    store.revoke(&repo, "no longer true").unwrap();
    assert!(
        !store
            .retrieve(ExperienceScope::Repo, 10, 0)
            .iter()
            .any(|record| record.key == "repo-lesson"),
        "a revoked record is not retrieved"
    );
    assert!(store.get(&repo).is_some(), "but it stays in the record");
    assert_eq!(store.len(), 4, "nothing was deleted");
}
