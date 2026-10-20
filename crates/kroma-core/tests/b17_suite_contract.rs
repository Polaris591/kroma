//! B17: the smoke suite must be usable offline, without a model and without spending anything.
//!
//! An oracle is only worth having if it is *discriminating*: it must reject the task's initial
//! (buggy) state and accept the intended fix. This contract proves that for every task in the
//! suite by materialising both states in a scratch workspace and running the real held-out
//! `verify_command`. It also pins the structural properties the scoring harness relies on:
//! unique ids, a non-vacuous oracle per task, and path-safe task inputs.
//!
//! The reference solutions live in a separate held-out file, which is exactly the split the B17 card
//! requires: the answers are not part of the task material handed to the worker.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use kroma_core::bench::{load_held_out_suite, BenchTask, VerifyCommandLimits};

const SUITE: &str = "tests/fixtures/bench/smoke_suite.json";
const SOLUTIONS: &str = "tests/fixtures/bench/smoke_solutions.json";

fn fixture(path: &str) -> PathBuf {
    // `CARGO_MANIFEST_DIR` is crates/kroma-core, so the fixture paths stay stable wherever the test
    // binary runs from.
    Path::new(env!("CARGO_MANIFEST_DIR")).join(path)
}

fn temp_workspace(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("kroma-b17-{}-{}", label, uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write_files(root: &Path, files: &BTreeMap<String, String>) {
    for (rel, contents) in files {
        let full = root.join(rel);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, contents).unwrap();
    }
}

fn solutions() -> BTreeMap<String, BTreeMap<String, String>> {
    let text = std::fs::read_to_string(fixture(SOLUTIONS)).expect("solutions fixture");
    serde_json::from_str(&text).expect("solutions JSON")
}

fn limits() -> VerifyCommandLimits {
    VerifyCommandLimits::new(Duration::from_secs(30), 256 * 1024)
}

/// The suite lives in the repository, never inside a graded workspace, so the held-out loader must
/// accept it.
fn load() -> Vec<BenchTask> {
    let workspace = temp_workspace("guard");
    let tasks = load_held_out_suite(&fixture(SUITE), &workspace).expect("suite must load");
    let _ = std::fs::remove_dir_all(&workspace);
    tasks
}

#[test]
fn every_task_has_a_unique_id_and_a_non_vacuous_oracle() {
    let tasks = load();
    // The plan asks for a 20-30 task smoke set. Pinning the floor here stops the suite from
    // silently shrinking back to a handful of examples.
    assert!(
        tasks.len() >= 20,
        "the smoke suite must hold at least 20 tasks, found {}",
        tasks.len()
    );
    let mut seen = std::collections::BTreeSet::new();
    for task in &tasks {
        assert!(
            seen.insert(task.id.clone()),
            "duplicate task id {}",
            task.id
        );
        let has_static = !task.expect_contains.is_empty() || !task.expect_absent.is_empty();
        let has_command = task
            .verify_command
            .as_ref()
            .is_some_and(|argv| !argv.is_empty());
        assert!(
            has_static || has_command,
            "task {} has no oracle at all, so it would resolve for free",
            task.id
        );
        assert!(
            !task.prompt.trim().is_empty(),
            "task {} has no prompt",
            task.id
        );
    }
}

#[test]
fn every_task_declares_a_reference_solution() {
    let tasks = load();
    let solutions = solutions();
    for task in &tasks {
        assert!(
            solutions.contains_key(&task.id),
            "task {} has no reference solution, so its oracle is unverified",
            task.id
        );
    }
    assert_eq!(
        solutions.len(),
        tasks.len(),
        "the solutions file and the suite must describe the same tasks"
    );
}

/// The discriminating-oracle contract: initial state fails, intended fix passes.
#[tokio::test]
async fn each_oracle_rejects_the_initial_state_and_accepts_its_solution() {
    let tasks = load();
    let solutions = solutions();
    let mut checked = 0usize;
    for task in &tasks {
        let solution = solutions
            .get(&task.id)
            .unwrap_or_else(|| panic!("missing solution for {}", task.id));

        // 1. The task as shipped must not already satisfy its oracle.
        let buggy = temp_workspace(&format!("buggy-{}", task.id));
        write_files(&buggy, &task.files);
        let initial = task.evaluate_with_limits(&buggy, &limits()).await;
        assert!(
            !initial.resolved,
            "task {} resolves before any work is done: {:?}",
            task.id, initial.checks
        );

        // 2. The reference solution must satisfy it.
        let fixed = temp_workspace(&format!("fixed-{}", task.id));
        write_files(&fixed, &task.files);
        write_files(&fixed, solution);
        let solved = task.evaluate_with_limits(&fixed, &limits()).await;
        assert!(
            solved.resolved,
            "task {} rejects its own reference solution: {:?}",
            task.id, solved.failures
        );

        let _ = std::fs::remove_dir_all(&buggy);
        let _ = std::fs::remove_dir_all(&fixed);
        checked += 1;
    }
    assert_eq!(checked, tasks.len(), "every task must be checked");
}

/// Deleting the implementation must not pass either: an empty module breaks the oracle's import,
/// and a permissive stub fails its assertions.
#[tokio::test]
async fn deleting_the_implementation_does_not_pass() {
    let tasks = load();
    let victim = tasks
        .iter()
        .find(|task| task.id == "delete-oracle-is-not-a-pass")
        .expect("the suite must keep the anti-deletion task");

    for (label, contents) in [
        ("gutted", ""),
        ("stub", "def check(value):\n    return True\n"),
    ] {
        let workspace = temp_workspace(label);
        write_files(&workspace, &victim.files);
        std::fs::write(workspace.join("src/check.py"), contents).unwrap();
        let outcome = victim.evaluate_with_limits(&workspace, &limits()).await;
        assert!(
            !outcome.resolved,
            "the {label} implementation must not satisfy the oracle: {:?}",
            outcome.checks
        );
        let _ = std::fs::remove_dir_all(&workspace);
    }
}
