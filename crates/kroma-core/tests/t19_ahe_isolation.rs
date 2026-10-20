//! T19 (KR-20/21/22): the self-repair experimenter must not be able to grade itself.
//!
//! What was wrong: the baseline was evaluated **in the source tree**, so a verifier that wrote to its
//! working directory mutated the very workspace the experiment promises to leave alone; the candidate
//! copy was removed by an explicit call, so any early return leaked it; and every successful
//! prediction collapsed into one `Accepted`, which meant "I predicted this task would still fail and
//! it does" counted as a repair.

use kroma_core::ahe::{
    evaluate_repair, frozen_evaluator_identity, FileChange, Prediction, RepairCandidate,
    RepairDecision,
};
use kroma_core::bench::{BenchTask, VerifyCommandLimits};
use kroma_core::self_repair_guard::{protected_artifacts, SelfRepairSurface, SelfRepairViolation};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Serializes the tests in this file: the temp-tree leak check scans `/tmp` for `kroma-ahe-*`, and a
/// concurrent experiment in another thread would look like a leak.
static AHE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn ahe_guard() -> tokio::sync::MutexGuard<'static, ()> {
    AHE_LOCK.lock().await
}

fn scratch(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("kroma-t19-{}-{}", tag, uuid::Uuid::new_v4()));
    std::fs::create_dir_all(root.join("src")).unwrap();
    root
}

fn workspace_files(root: &Path) {
    std::fs::write(
        root.join("src/calc.py"),
        "def total(n):\n    return sum(range(n))\n",
    )
    .unwrap();
    std::fs::write(root.join("src/keep.py"), "def ok():\n    return True\n").unwrap();
}

fn surface() -> SelfRepairSurface {
    SelfRepairSurface {
        allowed_prefixes: vec!["src/".to_string()],
        extra_protected: Vec::new(),
    }
}

fn limits() -> VerifyCommandLimits {
    VerifyCommandLimits::new(Duration::from_secs(20), 64 * 1024)
}

fn task(id: &str, verify: &str) -> BenchTask {
    BenchTask {
        id: id.to_string(),
        prompt: "p".to_string(),
        files: BTreeMap::new(),
        expect_contains: Vec::new(),
        expect_absent: Vec::new(),
        regression_guard: false,
        verify_command: Some(vec![
            "python3".to_string(),
            "-c".to_string(),
            verify.to_string(),
        ]),
    }
}

fn suite() -> Vec<BenchTask> {
    vec![
        task(
            "sum",
            "import sys; sys.path.insert(0,'src')\nfrom calc import total\nassert total(3)==6, total(3)",
        ),
        task(
            "keep",
            "import sys; sys.path.insert(0,'src')\nfrom keep import ok\nassert ok() is True",
        ),
    ]
}

fn fixing_candidate() -> RepairCandidate {
    RepairCandidate {
        hypothesis: "total() is off by one".to_string(),
        prediction: Prediction {
            task_id: "sum".to_string(),
            expected_resolved: true,
        },
        changes: vec![FileChange {
            path: "src/calc.py".to_string(),
            contents: "def total(n):\n    return sum(range(1, n + 1))\n".to_string(),
        }],
    }
}

fn temp_ahe_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with("kroma-ahe-") {
                dirs.push(entry.path());
            }
        }
    }
    dirs
}

/// A verifier that writes to its working directory must not reach the source tree: the baseline runs
/// in a frozen copy, not in place.
#[tokio::test]
async fn ahe_baseline_verifier_cannot_mutate_source_root() {
    let _guard = ahe_guard().await;
    let root = scratch("baseline-write");
    workspace_files(&root);
    let marker = root.join("src/calc.py");
    let before = std::fs::read_to_string(&marker).unwrap();

    // Both tasks write into their cwd; under the old code the baseline wrote into the source tree.
    let mut tasks = suite();
    tasks.push(task(
        "writer",
        "open('pwned.txt','w').write('x')\nopen('src/calc.py','w').write('# clobbered')\nprint('ok')",
    ));

    let outcome = evaluate_repair(&fixing_candidate(), &surface(), &root, &tasks, &limits())
        .await
        .expect("the candidate is allowed");
    assert!(
        outcome.source_untouched,
        "the source tree changed: {}",
        outcome.rationale
    );
    assert_eq!(
        std::fs::read_to_string(&marker).unwrap(),
        before,
        "a verifier wrote into the source tree"
    );
    assert!(
        !root.join("pwned.txt").exists(),
        "a verifier created a file in the source tree"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// Baseline and candidate start from identical copies of the frozen source.
#[tokio::test]
async fn ahe_candidate_and_baseline_start_identical() {
    let _guard = ahe_guard().await;
    let root = scratch("identical-start");
    workspace_files(&root);
    let log = root
        .parent()
        .unwrap()
        .join(format!("kroma-t19-digests-{}.log", uuid::Uuid::new_v4()));
    // Each evaluation appends the digest of an untouched file. If the two runs started from different
    // bytes, the two lines would differ.
    let digest_task = task(
        "digest",
        &format!(
            "import hashlib, pathlib\n\
             data = pathlib.Path('src/keep.py').read_bytes()\n\
             open({:?}, 'a').write(hashlib.sha256(data).hexdigest() + '\\n')",
            log.display().to_string()
        ),
    );
    let mut tasks = suite();
    tasks.push(digest_task);

    evaluate_repair(&fixing_candidate(), &surface(), &root, &tasks, &limits())
        .await
        .expect("the candidate is allowed");

    let lines: Vec<String> = std::fs::read_to_string(&log)
        .expect("the digest log")
        .lines()
        .map(str::to_string)
        .collect();
    assert_eq!(
        lines.len(),
        2,
        "expected one line per evaluation: {lines:?}"
    );
    assert_eq!(
        lines[0], lines[1],
        "baseline and candidate did not start from identical bytes"
    );
    let _ = std::fs::remove_file(&log);
    let _ = std::fs::remove_dir_all(&root);
}

/// The judge is outside the candidate's write scope, even inside an allowed prefix.
#[tokio::test]
async fn allowed_source_change_cannot_replace_frozen_judge() {
    let _guard = ahe_guard().await;
    let root = scratch("frozen-judge");
    workspace_files(&root);

    // A prefix that would allow everything, so only the protected list can refuse these.
    let permissive = SelfRepairSurface {
        allowed_prefixes: vec![String::new()],
        extra_protected: Vec::new(),
    };
    for path in [
        "crates/kroma-core/src/bench.rs",
        "crates/kroma-core/src/ahe.rs",
        "crates/kroma-core/src/completion_gate.rs",
        "crates/kroma-core/src/experiment.rs",
        "crates/kroma-core/tests/gate5_controlled_subagents.rs",
        "smoke_solutions.json",
    ] {
        let candidate = RepairCandidate {
            hypothesis: "make the judge kinder".to_string(),
            prediction: Prediction {
                task_id: "sum".to_string(),
                expected_resolved: true,
            },
            changes: vec![FileChange {
                path: path.to_string(),
                contents: "// kinder judge".to_string(),
            }],
        };
        let err = evaluate_repair(&candidate, &permissive, &root, &suite(), &limits())
            .await
            .expect_err("a candidate may not touch the judge");
        match err {
            SelfRepairViolation::Protected { path: refused, .. } => {
                assert_eq!(refused, path, "the refusal must name the artifact");
            }
            other => panic!("expected Protected for {path}, got {other:?}"),
        }
    }
    // The source tree was never touched by a refused candidate.
    assert_eq!(
        std::fs::read_to_string(root.join("src/calc.py")).unwrap(),
        "def total(n):\n    return sum(range(n))\n"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// Predicting that a failing task *stays* failing is a true prediction and not a repair.
#[tokio::test]
async fn predicting_existing_failure_is_not_promotion() {
    let _guard = ahe_guard().await;
    let root = scratch("predict-fail");
    workspace_files(&root);
    // The task genuinely fails; the candidate predicts exactly that and changes nothing useful.
    let candidate = RepairCandidate {
        hypothesis: "this stays broken".to_string(),
        prediction: Prediction {
            task_id: "sum".to_string(),
            expected_resolved: false,
        },
        changes: vec![FileChange {
            path: "src/calc.py".to_string(),
            contents: "def total(n):\n    return sum(range(n))  # still wrong\n".to_string(),
        }],
    };
    let outcome = evaluate_repair(&candidate, &surface(), &root, &suite(), &limits())
        .await
        .expect("the candidate is allowed");

    assert!(
        outcome.verdict.prediction_confirmed,
        "the prediction did hold"
    );
    assert!(
        !outcome.verdict.improvement_accepted,
        "a confirmed failure is not an improvement"
    );
    assert!(
        !outcome.verdict.promotion_approved,
        "nothing may self-promote"
    );
    assert_eq!(outcome.decision, RepairDecision::Rejected);
    assert_eq!(outcome.verdict.as_str(), "prediction_confirmed_only");
    assert!(outcome.rationale.contains("not an improvement"));
    let _ = std::fs::remove_dir_all(&root);
}

/// A candidate that changes a file but improves nothing stays experimental.
#[tokio::test]
async fn zero_effect_candidate_remains_experimental() {
    let _guard = ahe_guard().await;
    let root = scratch("zero-effect");
    workspace_files(&root);
    // Rewrites an unrelated file: the predicted task still fails, nothing improves.
    let candidate = RepairCandidate {
        hypothesis: "a comment will fix it".to_string(),
        prediction: Prediction {
            task_id: "sum".to_string(),
            expected_resolved: true,
        },
        changes: vec![FileChange {
            path: "src/keep.py".to_string(),
            contents: "def ok():\n    return True  # unrelated\n".to_string(),
        }],
    };
    let outcome = evaluate_repair(&candidate, &surface(), &root, &suite(), &limits())
        .await
        .expect("the candidate is allowed");
    assert!(!outcome.verdict.prediction_confirmed);
    assert!(!outcome.verdict.improvement_accepted);
    assert!(!outcome.verdict.promotion_approved);
    assert!(outcome.improvements.is_empty());
    assert_eq!(outcome.decision, RepairDecision::Rejected);
    let _ = std::fs::remove_dir_all(&root);
}

/// A genuine repair is accepted as a *candidate for review*, never promoted automatically.
#[tokio::test]
async fn a_real_repair_is_accepted_but_never_promoted() {
    let _guard = ahe_guard().await;
    let root = scratch("real-repair");
    workspace_files(&root);
    let outcome = evaluate_repair(&fixing_candidate(), &surface(), &root, &suite(), &limits())
        .await
        .expect("the candidate is allowed");
    assert!(outcome.verdict.prediction_confirmed);
    assert!(outcome.verdict.experiment_valid);
    assert!(outcome.verdict.improvement_accepted);
    assert!(
        !outcome.verdict.promotion_approved,
        "promotion requires human approval after an independent regression"
    );
    assert_eq!(outcome.verdict.as_str(), "improvement_accepted");
    assert_eq!(outcome.improvements, vec!["sum".to_string()]);
    assert!(outcome.regressions.is_empty());
    // The source tree is still the buggy original.
    assert_eq!(
        std::fs::read_to_string(root.join("src/calc.py")).unwrap(),
        "def total(n):\n    return sum(range(n))\n"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// Changing the judge invalidates the comparison identity.
#[test]
fn changing_evaluator_invalidates_comparison_identity() {
    let base = suite();
    let identity = frozen_evaluator_identity(&base, &limits());
    assert_eq!(identity.len(), 64);
    // Same suite, same limits: same identity.
    assert_eq!(frozen_evaluator_identity(&suite(), &limits()), identity);

    // A different verification command is a different judge.
    let mut changed = suite();
    changed[0].verify_command = Some(vec!["true".to_string()]);
    assert_ne!(frozen_evaluator_identity(&changed, &limits()), identity);

    // A different limit is a different judge too.
    let other_limits = VerifyCommandLimits::new(Duration::from_secs(5), 64 * 1024);
    assert_ne!(frozen_evaluator_identity(&base, &other_limits), identity);

    // A different post-condition is a different judge.
    let mut changed = suite();
    changed[1].expect_absent = vec!["src/gone.txt".to_string()];
    assert_ne!(frozen_evaluator_identity(&changed, &limits()), identity);
}

/// Every protected artifact the task card names is actually on the list.
#[test]
fn the_judge_and_its_evidence_are_protected() {
    let protected = protected_artifacts();
    let paths: Vec<&str> = protected
        .iter()
        .filter_map(|artifact| artifact.path.as_deref())
        .collect();
    let prefixes: Vec<&str> = protected
        .iter()
        .filter_map(|artifact| artifact.path_prefix.as_deref())
        .collect();
    for required in [
        "crates/kroma-core/src/bench.rs",
        "crates/kroma-core/src/completion_gate.rs",
        "crates/kroma-core/src/ahe.rs",
        "crates/kroma-core/src/experiment.rs",
        "crates/kroma-core/src/usage_cost.rs",
        "crates/kroma-core/src/tool/policy.rs",
    ] {
        assert!(
            paths.contains(&required),
            "{} is not protected; a candidate could rewrite the rule that judges it",
            required
        );
    }
    assert!(
        prefixes
            .iter()
            .any(|p| p.starts_with("crates/kroma-core/tests/")),
        "the trusted test suite must be protected as a subtree"
    );
    // Every entry explains itself: an unexplained protection is unauditable.
    for artifact in &protected {
        assert!(!artifact.reason.is_empty(), "an artifact has no reason");
    }
}

/// An exception during the experiment leaves no copy behind.
#[tokio::test]
async fn ahe_apply_failure_and_cancel_clean_temp() {
    let _guard = ahe_guard().await;
    let root = scratch("apply-failure");
    workspace_files(&root);
    let before = temp_ahe_dirs();

    // `src` is a directory, so writing it as a file must fail after the copies were made.
    let candidate = RepairCandidate {
        hypothesis: "write over a directory".to_string(),
        prediction: Prediction {
            task_id: "sum".to_string(),
            expected_resolved: true,
        },
        changes: vec![FileChange {
            path: "src".to_string(),
            contents: "not a directory".to_string(),
        }],
    };
    let err = evaluate_repair(&candidate, &surface(), &root, &suite(), &limits())
        .await
        .expect_err("writing over a directory must fail");
    assert!(matches!(err, SelfRepairViolation::UnsafePath(_)));

    // Both copies are gone: the failure happened after they were created.
    let after = temp_ahe_dirs();
    let leaked: Vec<&PathBuf> = after.iter().filter(|dir| !before.contains(dir)).collect();
    assert!(
        leaked.is_empty(),
        "the failed experiment leaked {} temp tree(s): {leaked:?}",
        leaked.len()
    );

    // A normal run leaves nothing either.
    let outcome = evaluate_repair(&fixing_candidate(), &surface(), &root, &suite(), &limits())
        .await
        .expect("the candidate is allowed");
    assert!(outcome.verdict.improvement_accepted);
    let after_success = temp_ahe_dirs();
    assert!(
        after_success.iter().all(|dir| before.contains(dir)),
        "a successful run leaked a temp tree"
    );
    let _ = std::fs::remove_dir_all(&root);
}
