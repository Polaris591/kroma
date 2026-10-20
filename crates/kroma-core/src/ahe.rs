//! B23: an offline, deterministic self-repair loop.
//!
//! The loop under test is: a failure trajectory yields **one hypothesis with a falsifiable
//! prediction**; a candidate patch fixes it; the patch is applied to an **isolated copy**, checked
//! against the contract (the held-out tasks), and only then accepted, rejected or rolled back.
//!
//! What this module deliberately does *not* do: propose hypotheses with a model, or hot-apply
//! anything to the live tree. The model-driven hypothesis search needs an experiment budget (marked
//! blocked in the card conclusions); this module is the part that must be correct *before* such a
//! search is worth running:
//!
//!  * a candidate may not touch the security policy, the trust root, the scorer or the held-out
//!    answers ([`crate::self_repair_guard`]) — otherwise the repair rewrites its own judge,
//!  * the patch is applied to a copy, never to the tree the failure came from,
//!  * the prediction is checked against the observed result, so a hypothesis that "explains" a
//!    failure without predicting anything is rejected rather than accepted,
//!  * a regression in any task that used to pass rejects the candidate, and
//!  * the operator sees the decision and the evidence; nothing is hot-applied automatically.

use crate::bench::{BenchTask, VerifyCommandLimits};
use crate::self_repair_guard::{SelfRepairSurface, SelfRepairViolation};
use std::path::{Path, PathBuf};

/// One file the candidate rewrites, relative to the workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChange {
    pub path: String,
    pub contents: String,
}

/// What the hypothesis claims will happen. A hypothesis without a prediction cannot be falsified, so
/// the loop refuses to run one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prediction {
    /// Task the hypothesis is about.
    pub task_id: String,
    /// Whether that task is expected to pass after the repair.
    pub expected_resolved: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepairCandidate {
    /// The single hypothesis, in prose, for the operator's record.
    pub hypothesis: String,
    pub prediction: Prediction,
    pub changes: Vec<FileChange>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepairDecision {
    Accepted,
    Rejected,
    /// The candidate was refused before it ran (unsafe path, protected artifact, empty patch).
    Refused,
}

/// T19 (KR-22): the four levels the single `Accepted` used to collapse.
///
/// * `prediction_confirmed` — the stated hypothesis predicted what actually happened. This says
///   nothing about whether the change is good: predicting "this task still fails" is a true
///   prediction and a useless repair.
/// * `experiment_valid` — the comparison itself is sound: both sides ran from identical frozen
///   copies, the judge did not change, and the source tree was not touched.
/// * `improvement_accepted` — a *real* improvement: valid, confirmed, no regressions, at least one
///   task improved, and the prediction was that the task would start passing.
/// * `promotion_approved` — always `false` in this version. Promotion is a human decision taken after
///   an independent full regression; the experimenter only ever produces a patch and a report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RepairVerdict {
    pub prediction_confirmed: bool,
    pub experiment_valid: bool,
    pub improvement_accepted: bool,
    pub promotion_approved: bool,
}

impl RepairVerdict {
    pub fn as_str(&self) -> &'static str {
        if self.promotion_approved {
            "promotion_approved"
        } else if self.improvement_accepted {
            "improvement_accepted"
        } else if self.prediction_confirmed && self.experiment_valid {
            "prediction_confirmed_only"
        } else if self.experiment_valid {
            "experiment_valid_no_improvement"
        } else {
            "invalid_experiment"
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepairOutcome {
    pub decision: RepairDecision,
    /// T19: the split verdict. `decision` is kept as a coarse summary for existing callers and is
    /// derived from `verdict.improvement_accepted`.
    pub verdict: RepairVerdict,
    /// T19: identity of the judge (tasks + limits) that produced this comparison. Two outcomes with
    /// different values were judged by different rules and cannot be compared.
    pub evaluator_sha256: String,
    /// T19: `true` when the source tree's content hash is unchanged after the experiment.
    pub source_untouched: bool,
    pub rationale: String,
    /// `Some(true/false)` when the prediction could be checked; `None` when the run never happened.
    pub prediction_held: Option<bool>,
    /// Task ids that passed before and failed after the candidate: a regression.
    pub regressions: Vec<String>,
    /// Task ids whose verdict improved.
    pub improvements: Vec<String>,
}

impl RepairOutcome {
    fn refused(reason: impl Into<String>) -> Self {
        Self {
            decision: RepairDecision::Refused,
            verdict: RepairVerdict::default(),
            evaluator_sha256: String::new(),
            source_untouched: true,
            rationale: reason.into(),
            prediction_held: None,
            regressions: Vec::new(),
            improvements: Vec::new(),
        }
    }
}

/// T19 (KR-20): identity of the judge.
///
/// Everything that decides the verdict is hashed: task ids, prompts, post-conditions, verification
/// commands and the limits they run under. If any of it changes, the same candidate is no longer
/// being compared against the same judge, and [`compare_outcomes`] refuses to treat the two runs as
/// one experiment.
pub fn frozen_evaluator_identity(tasks: &[BenchTask], limits: &VerifyCommandLimits) -> String {
    let mut canonical = String::new();
    canonical.push_str(&format!(
        "limits:{}:{}\n",
        limits.timeout.as_millis(),
        limits.max_output_bytes
    ));
    for task in tasks {
        canonical.push_str(&format!("task:{}\n", task.id));
        canonical.push_str(&format!("prompt:{}\n", task.prompt));
        for (path, needle) in &task.expect_contains {
            canonical.push_str(&format!("contains:{path}:{needle}\n"));
        }
        for path in &task.expect_absent {
            canonical.push_str(&format!("absent:{path}\n"));
        }
        canonical.push_str(&format!("regression_guard:{}\n", task.regression_guard));
        if let Some(command) = &task.verify_command {
            canonical.push_str(&format!("verify:{}\n", command.join("\u{1f}")));
        }
    }
    crate::prompt::sha256_hex64(canonical.as_bytes())
}

/// T19: can two outcomes be compared at all?
///
/// Returns `None` when the judge changed between them — the same candidate measured under different
/// rules is not a before/after pair, and treating it as one is how a comparison quietly becomes
/// meaningless.
pub fn compare_outcomes(before: &RepairOutcome, after: &RepairOutcome) -> Option<f64> {
    if before.evaluator_sha256 != after.evaluator_sha256 {
        return None;
    }
    if !before.source_untouched || !after.source_untouched {
        return None;
    }
    let before_improved = before.improvements.len() as f64 - before.regressions.len() as f64;
    let after_improved = after.improvements.len() as f64 - after.regressions.len() as f64;
    Some(after_improved - before_improved)
}

/// Run the offline repair loop for one candidate.
///
/// `source_root` is never modified: the candidate is applied to a fresh copy, the held-out tasks are
/// evaluated there, and the copy is removed afterwards (`--rollback` and "accept" differ only in the
/// decision recorded, never in whether the original tree was touched).
pub async fn evaluate_repair(
    candidate: &RepairCandidate,
    surface: &SelfRepairSurface,
    source_root: &Path,
    tasks: &[BenchTask],
    limits: &VerifyCommandLimits,
) -> Result<RepairOutcome, SelfRepairViolation> {
    if candidate.changes.is_empty() {
        return Ok(RepairOutcome::refused("the candidate changes no files"));
    }
    if candidate.prediction.task_id.trim().is_empty() {
        return Ok(RepairOutcome::refused(
            "the hypothesis states no prediction, so it cannot be falsified",
        ));
    }
    // 1. Change surface: refuse before anything runs, so a refused candidate has no side effects.
    surface.check(candidate.changes.iter().map(|change| change.path.as_str()))?;
    if !tasks
        .iter()
        .any(|task| task.id == candidate.prediction.task_id)
    {
        return Ok(RepairOutcome::refused(format!(
            "the prediction names unknown task '{}'",
            candidate.prediction.task_id
        )));
    }

    // T19 (KR-20): identity of the judge, captured before anything runs, so a comparison can be tied
    // to the rules that produced it.
    let evaluator_sha256 = frozen_evaluator_identity(tasks, limits);
    let source_before = crate::provenance::fingerprint_explicit(source_root, None).source_sha256;

    // 2. Baseline and candidate each run in their own frozen copy. The source tree is never an
    // evaluation root: a verifier that writes (or a task that deletes) would otherwise mutate the
    // workspace the experiment is supposed to leave alone.
    let baseline_tree = TempTree::create_from(source_root)?;
    let candidate_tree = TempTree::create_from(source_root)?;

    // 3. Before: the contract's verdicts on an untouched copy.
    let before = evaluate_all(baseline_tree.path(), tasks, limits).await;

    // 4. Apply the candidate to its own copy.
    for change in &candidate.changes {
        let target = candidate_tree.path().join(&change.path);
        if let Some(parent) = target.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        std::fs::write(&target, &change.contents).map_err(|error| {
            SelfRepairViolation::UnsafePath(format!("{}: {error}", change.path))
        })?;
    }

    // 5. After: the same contract on the patched copy. Both trees are removed by `Drop` on every
    // exit path, including the early returns above.
    let after = evaluate_all(candidate_tree.path(), tasks, limits).await;
    drop(baseline_tree);
    drop(candidate_tree);

    let source_after = crate::provenance::fingerprint_explicit(source_root, None).source_sha256;
    let source_untouched = source_before == source_after;

    let mut regressions = Vec::new();
    let mut improvements = Vec::new();
    for ((id, was), (_, now)) in before.iter().zip(after.iter()) {
        match (was, now) {
            (true, false) => regressions.push(id.clone()),
            (false, true) => improvements.push(id.clone()),
            _ => {}
        }
    }

    // 6. The prediction must actually hold: a hypothesis that explains nothing is rejected.
    let predicted_after = after
        .iter()
        .find(|(id, _)| id == &candidate.prediction.task_id)
        .map(|(_, resolved)| *resolved)
        .unwrap_or(false);
    let prediction_held = predicted_after == candidate.prediction.expected_resolved;

    // 7. T19 (KR-22): split the verdict. A confirmed prediction is not an improvement, and an
    // improvement is not a promotion.
    let experiment_valid = source_untouched;
    let improvement_accepted = experiment_valid
        && prediction_held
        && regressions.is_empty()
        && !improvements.is_empty()
        // Predicting that a failing task *stays* failing is a true prediction and a useless repair.
        && candidate.prediction.expected_resolved;
    let verdict = RepairVerdict {
        prediction_confirmed: prediction_held,
        experiment_valid,
        improvement_accepted,
        // Promotion is a human decision after an independent full regression. The experimenter never
        // approves its own promotion.
        promotion_approved: false,
    };

    let rationale = if !source_untouched {
        format!(
            "the source tree changed during the experiment ({} -> {}); the comparison is invalid",
            source_before.as_deref().unwrap_or("unknown"),
            source_after.as_deref().unwrap_or("unknown")
        )
    } else if !prediction_held {
        format!(
            "the prediction did not hold: task '{}' was expected resolved={} but is resolved={}",
            candidate.prediction.task_id, candidate.prediction.expected_resolved, predicted_after
        )
    } else if !regressions.is_empty() {
        format!(
            "the prediction held but {} previously passing task(s) regressed: {}",
            regressions.len(),
            regressions.join(", ")
        )
    } else if !candidate.prediction.expected_resolved {
        format!(
            "the prediction held (task '{}' still fails) but a confirmed failure is not an improvement",
            candidate.prediction.task_id
        )
    } else if improvements.is_empty() {
        format!(
            "the prediction held but nothing improved ({} improved, {} regressed): the candidate stays experimental",
            improvements.len(),
            regressions.len()
        )
    } else {
        format!(
            "the prediction held, no task regressed and {} task(s) improved; the patch is a candidate for human review",
            improvements.len()
        )
    };

    Ok(RepairOutcome {
        decision: if improvement_accepted {
            RepairDecision::Accepted
        } else {
            RepairDecision::Rejected
        },
        verdict,
        evaluator_sha256,
        source_untouched,
        rationale,
        prediction_held: Some(prediction_held),
        regressions,
        improvements,
    })
}

/// T19 (KR-20): a temporary copy of the source tree that removes itself on every exit path.
///
/// The earlier code removed the copy with an explicit call after the evaluation, so any early return
/// (a failed write, a refusal, a panic) leaked a full workspace copy. `Drop` makes cleanup
/// unconditional.
struct TempTree {
    path: PathBuf,
}

impl TempTree {
    fn create_from(source: &Path) -> Result<Self, SelfRepairViolation> {
        let path = std::env::temp_dir().join(format!("kroma-ahe-{}", uuid::Uuid::new_v4()));
        copy_dir(source, &path).map_err(|error| {
            let _ = std::fs::remove_dir_all(&path);
            SelfRepairViolation::UnsafePath(format!("isolated copy failed: {error}"))
        })?;
        Ok(Self { path })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempTree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// `(task id, resolved)` for every task, in the suite's order.
async fn evaluate_all(
    root: &Path,
    tasks: &[BenchTask],
    limits: &VerifyCommandLimits,
) -> Vec<(String, bool)> {
    let mut verdicts = Vec::with_capacity(tasks.len());
    for task in tasks {
        let evaluation = task.evaluate_with_limits(root, limits).await;
        verdicts.push((task.id.clone(), evaluation.resolved));
    }
    verdicts
}

fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let target = to.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else if file_type.is_file() {
            std::fs::copy(entry.path(), &target)?;
        }
        // Symlinks and special files are deliberately not copied: a repair candidate has no business
        // reaching outside the workspace, and copying a FIFO would block.
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// The demo workspace keeps its modules under `src/`, so the demo surface allows exactly that.
    /// The guard itself is unchanged: a patch outside this prefix is still refused (see the test for
    /// a protected path, which is refused by name regardless of the prefix).
    fn demo_surface() -> SelfRepairSurface {
        SelfRepairSurface {
            allowed_prefixes: vec!["src/".to_string()],
            extra_protected: Vec::new(),
        }
    }

    fn limits() -> VerifyCommandLimits {
        VerifyCommandLimits::new(std::time::Duration::from_secs(20), 64 * 1024)
    }

    /// A workspace with one buggy module and a second module that must keep working.
    fn workspace(label: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("kroma-ahe-src-{label}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("src/calc.py"),
            "def total(n):\n    return sum(range(n))\n",
        )
        .unwrap();
        std::fs::write(root.join("src/keep.py"), "def ok():\n    return True\n").unwrap();
        root
    }

    fn tasks() -> Vec<BenchTask> {
        let task = |id: &str, verify: &str| BenchTask {
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
        };
        vec![
            task(
                "sum",
                "import sys; sys.path.insert(0,'src')\nfrom calc import total\nassert total(3)==6, total(3)\nprint('ok')",
            ),
            task(
                "keep",
                "import sys; sys.path.insert(0,'src')\nfrom keep import ok\nassert ok() is True\nprint('ok')",
            ),
        ]
    }

    fn fix_change() -> FileChange {
        FileChange {
            path: "src/calc.py".to_string(),
            contents: "def total(n):\n    return sum(range(1, n + 1))\n".to_string(),
        }
    }

    /// The happy path: a patch that fixes the predicted task is accepted, and the tree the failure
    /// came from is untouched afterwards.
    #[tokio::test]
    async fn a_working_repair_is_accepted_without_touching_the_source_tree() {
        let root = workspace("accept");
        let original = std::fs::read_to_string(root.join("src/calc.py")).unwrap();
        let candidate = RepairCandidate {
            hypothesis: "total() sums 0..n instead of 1..=n".to_string(),
            prediction: Prediction {
                task_id: "sum".to_string(),
                expected_resolved: true,
            },
            changes: vec![fix_change()],
        };

        let outcome = evaluate_repair(&candidate, &demo_surface(), &root, &tasks(), &limits())
            .await
            .expect("the candidate is allowed");

        assert_eq!(outcome.decision, RepairDecision::Accepted, "{outcome:?}");
        assert_eq!(outcome.prediction_held, Some(true));
        assert!(outcome.regressions.is_empty());
        assert_eq!(outcome.improvements, vec!["sum".to_string()]);
        assert_eq!(
            std::fs::read_to_string(root.join("src/calc.py")).unwrap(),
            original,
            "the source tree must never be modified by a repair evaluation"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A candidate that touches a protected artifact is refused before anything runs.
    #[tokio::test]
    async fn a_candidate_touching_the_judge_is_refused_before_it_runs() {
        let root = workspace("protected");
        let candidate = RepairCandidate {
            hypothesis: "skip the gate".to_string(),
            prediction: Prediction {
                task_id: "sum".to_string(),
                expected_resolved: true,
            },
            changes: vec![
                fix_change(),
                FileChange {
                    path: "crates/kroma-core/src/tool/policy.rs".to_string(),
                    contents: "// weakened".to_string(),
                },
            ],
        };

        let error = evaluate_repair(&candidate, &demo_surface(), &root, &tasks(), &limits())
            .await
            .expect_err("a patch touching the security policy must be refused");
        assert!(
            matches!(error, SelfRepairViolation::Protected { .. }),
            "{error:?}"
        );
        assert!(
            !root.join("crates/kroma-core/src/tool/policy.rs").exists(),
            "a refused candidate must not write anything"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A hypothesis whose prediction does not hold is rejected, not accepted with a note.
    #[tokio::test]
    async fn a_candidate_whose_prediction_does_not_hold_is_rejected() {
        let root = workspace("prediction");
        let candidate = RepairCandidate {
            hypothesis: "the failure is caused by something else entirely".to_string(),
            prediction: Prediction {
                task_id: "sum".to_string(),
                expected_resolved: true,
            },
            // A change that does not fix the task.
            changes: vec![FileChange {
                path: "src/calc.py".to_string(),
                contents: "def total(n):\n    return 0\n".to_string(),
            }],
        };

        let outcome = evaluate_repair(&candidate, &demo_surface(), &root, &tasks(), &limits())
            .await
            .expect("the candidate is allowed");

        assert_eq!(outcome.decision, RepairDecision::Rejected);
        assert_eq!(outcome.prediction_held, Some(false));
        assert!(
            outcome.rationale.contains("prediction did not hold"),
            "{outcome:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Fixing the target while breaking another task is a regression, so it is rejected.
    #[tokio::test]
    async fn a_repair_that_regresses_another_task_is_rejected() {
        let root = workspace("regression");
        let candidate = RepairCandidate {
            hypothesis: "fix the sum by rewriting the sibling module".to_string(),
            prediction: Prediction {
                task_id: "sum".to_string(),
                expected_resolved: true,
            },
            changes: vec![
                fix_change(),
                FileChange {
                    path: "src/keep.py".to_string(),
                    contents: "def ok():\n    return False\n".to_string(),
                },
            ],
        };

        let outcome = evaluate_repair(&candidate, &demo_surface(), &root, &tasks(), &limits())
            .await
            .expect("the candidate is allowed");

        assert_eq!(outcome.decision, RepairDecision::Rejected);
        assert_eq!(outcome.regressions, vec!["keep".to_string()]);
        assert!(outcome.rationale.contains("regressed"), "{outcome:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A prediction about a task that does not exist cannot be checked, so the loop refuses it.
    #[tokio::test]
    async fn a_prediction_about_an_unknown_task_is_refused() {
        let root = workspace("unknown");
        let candidate = RepairCandidate {
            hypothesis: "h".to_string(),
            prediction: Prediction {
                task_id: "does-not-exist".to_string(),
                expected_resolved: true,
            },
            changes: vec![fix_change()],
        };
        let outcome = evaluate_repair(&candidate, &demo_surface(), &root, &tasks(), &limits())
            .await
            .expect("allowed surface");
        assert_eq!(outcome.decision, RepairDecision::Refused);
        assert!(outcome.rationale.contains("unknown task"), "{outcome:?}");
        let _ = std::fs::remove_dir_all(&root);
    }
}
