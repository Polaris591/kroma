//! Item #3 (`v1.4.0`) — Paired, budget-locked A/B benchmark runner (`kroma bench`).
//!
//! Astra's third recommendation was to stop asserting that a mechanism helps and start *measuring*
//! it. This module provides the measurement plumbing:
//!
//! * [`BenchTask`] describes one self-contained task: the initial workspace files, the prompt, and
//!   the post-conditions that decide whether the task was resolved.
//! * [`BenchStrategyConfig`] names one arm of the comparison (for example `single_agent` vs
//!   `adaptive_tournament`, or `baseline` vs `p2` projection profiles).
//! * [`BenchExecutor`] runs one arm on one task. Every arm receives the **same**
//!   `budget_tokens` total, so a comparison is a paired, budget-locked measurement rather than
//!   "the arm that spent more won".
//! * [`BenchComparisonReport`] aggregates the per-arm results into pass rate, total tokens,
//!   latency and an explicit winner.
//!
//! ## Honesty about the built-in suite
//!
//! With no `--suite` file, `kroma bench` runs [`builtin_smoke_suite`] against the
//! [`SyntheticBenchExecutor`]. That executor is **deterministic and synthetic**: it validates the
//! benchmark plumbing (budget locking, token accounting, report aggregation, paired comparison) on
//! a machine with no model endpoint, and its numbers are labelled `synthetic` in the report. Real
//! measurements require a real suite plus a provider-backed executor; the report never presents
//! synthetic numbers as evidence that a mechanism improves real task resolution.

use crate::budget::BudgetConfig;
use crate::workspace::WorkspaceCap;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

/// Marker placed in every synthetic report so its numbers can never be mistaken for real evidence.
pub const SYNTHETIC_EXECUTOR_LABEL: &str = "synthetic";

/// One self-contained benchmark task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BenchTask {
    pub id: String,
    /// Prompt handed to the agent under test.
    pub prompt: String,
    /// Initial workspace files (workspace-relative path -> contents).
    #[serde(default)]
    pub files: BTreeMap<String, String>,
    /// Post-conditions: each `(path, needle)` must be present in the file for the task to resolve.
    #[serde(default)]
    pub expect_contains: Vec<(String, String)>,
    /// Post-conditions: each path must no longer exist for the task to resolve.
    #[serde(default)]
    pub expect_absent: Vec<String>,
    /// Marks a task as a "regression" task whose failure is a hard regression signal.
    #[serde(default)]
    pub regression_guard: bool,
}

impl BenchTask {
    /// Evaluate the task's post-conditions against a materialized workspace.
    ///
    /// Every path is re-validated with [`WorkspaceCap::validate_relative_components`] before it is
    /// joined, so a task whose post-conditions escape the workspace (absolute path, `..`, drive
    /// prefix, ADS, reserved device name) can never read outside it.
    pub fn is_resolved(&self, workspace: &Path) -> bool {
        for (path, needle) in &self.expect_contains {
            let Ok(rel) = WorkspaceCap::validate_relative_components(path) else {
                return false;
            };
            match std::fs::read_to_string(workspace.join(rel)) {
                Ok(text) if text.contains(needle.as_str()) => {}
                _ => return false,
            }
        }
        for path in &self.expect_absent {
            let Ok(rel) = WorkspaceCap::validate_relative_components(path) else {
                return false;
            };
            if workspace.join(rel).exists() {
                return false;
            }
        }
        true
    }
}

/// One arm of a paired comparison.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BenchStrategyConfig {
    /// Stable arm name used in the report (for example `single_agent`).
    pub name: String,
    /// Context projection profile this arm runs with.
    #[serde(default)]
    pub projection_profile: Option<String>,
    /// Adaptive tournament candidate count for this arm (`0` = single agent).
    #[serde(default)]
    pub adaptive_tournament_candidates: usize,
}

impl BenchStrategyConfig {
    pub fn single_agent() -> Self {
        Self {
            name: "single_agent".to_string(),
            projection_profile: Some("p2".to_string()),
            adaptive_tournament_candidates: 0,
        }
    }

    pub fn adaptive_tournament(candidates: usize) -> Self {
        Self {
            name: format!("adaptive_tournament_{}", candidates),
            projection_profile: Some("p2".to_string()),
            adaptive_tournament_candidates: candidates.max(2),
        }
    }
}

/// Result of running one arm on one task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BenchArmResult {
    pub task_id: String,
    pub strategy: String,
    /// `true` when every post-condition held.
    pub resolved: bool,
    /// Terminal gate status (`approved` / `skipped` / `rejected` / `not_run`).
    pub gate_status: String,
    /// Tokens spent by the parent loop itself.
    pub self_tokens: u64,
    /// Tokens spent by child subagents / tournament candidates.
    pub children_tokens: u64,
    /// `self_tokens + children_tokens`, always `<= budget_tokens`.
    pub total_tokens: u64,
    pub wall_time_ms: u64,
    /// Number of regression-guard tasks this arm broke.
    pub regressions: usize,
}

/// Aggregated per-arm summary.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BenchArmSummary {
    pub strategy: String,
    pub tasks: usize,
    pub resolved: usize,
    pub pass_rate: f64,
    pub total_tokens: u64,
    pub wall_time_ms: u64,
    pub regressions: usize,
}

/// Full paired comparison report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BenchComparisonReport {
    pub suite: String,
    /// The single total budget every arm received (paired, budget-locked comparison).
    pub budget_tokens: u64,
    /// `synthetic` when the numbers came from the built-in deterministic executor.
    pub executor: String,
    pub results: Vec<BenchArmResult>,
    pub arms: Vec<BenchArmSummary>,
    /// Name of the winning arm, or `None` on a tie.
    pub winner: Option<String>,
}

impl BenchComparisonReport {
    /// Aggregate raw per-task results into per-arm summaries and pick a winner.
    ///
    /// Ranking is lexicographic: higher pass rate, then fewer regressions, then lower total tokens,
    /// then lower wall time. This makes "spent more to win" a losing strategy on ties.
    pub fn from_results(
        suite: impl Into<String>,
        budget_tokens: u64,
        executor: impl Into<String>,
        results: Vec<BenchArmResult>,
    ) -> Self {
        let mut by_arm: BTreeMap<String, Vec<&BenchArmResult>> = BTreeMap::new();
        for r in &results {
            by_arm.entry(r.strategy.clone()).or_default().push(r);
        }
        let mut arms: Vec<BenchArmSummary> = Vec::new();
        for (strategy, rs) in by_arm {
            let tasks = rs.len();
            let resolved = rs.iter().filter(|r| r.resolved).count();
            let total_tokens = rs.iter().map(|r| r.total_tokens).sum::<u64>();
            let wall_time_ms = rs.iter().map(|r| r.wall_time_ms).sum::<u64>();
            let regressions = rs.iter().map(|r| r.regressions).sum::<usize>();
            arms.push(BenchArmSummary {
                strategy,
                tasks,
                resolved,
                pass_rate: if tasks == 0 {
                    0.0
                } else {
                    resolved as f64 / tasks as f64
                },
                total_tokens,
                wall_time_ms,
                regressions,
            });
        }
        arms.sort_by(|a, b| {
            b.pass_rate
                .partial_cmp(&a.pass_rate)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.regressions.cmp(&b.regressions))
                .then_with(|| a.total_tokens.cmp(&b.total_tokens))
                .then_with(|| a.wall_time_ms.cmp(&b.wall_time_ms))
                .then_with(|| a.strategy.cmp(&b.strategy))
        });

        let winner = match (arms.first(), arms.get(1)) {
            (Some(a), Some(b))
                if a.pass_rate == b.pass_rate
                    && a.regressions == b.regressions
                    && a.total_tokens == b.total_tokens =>
            {
                None
            }
            (Some(a), _) => Some(a.strategy.clone()),
            (None, _) => None,
        };

        Self {
            suite: suite.into(),
            budget_tokens,
            executor: executor.into(),
            results,
            arms,
            winner,
        }
    }

    /// Human-readable paired comparison report.
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("suite:    {}\n", self.suite));
        out.push_str(&format!("executor: {}\n", self.executor));
        out.push_str(&format!(
            "budget:   {} tokens per arm per task (paired, budget-locked)\n",
            self.budget_tokens
        ));
        if self.executor == SYNTHETIC_EXECUTOR_LABEL {
            out.push_str(
                "note:     synthetic executor — validates the benchmark plumbing only; these numbers\n          are NOT evidence that a mechanism improves real task resolution.\n",
            );
        }
        out.push('\n');
        out.push_str(&format!(
            "{:<26} {:>6} {:>8} {:>9} {:>11} {:>12} {:>8}\n",
            "arm", "tasks", "pass", "pass_rate", "total_tok", "wall_ms", "regress"
        ));
        for arm in &self.arms {
            out.push_str(&format!(
                "{:<26} {:>6} {:>8} {:>9.1}% {:>11} {:>12} {:>8}\n",
                arm.strategy,
                arm.tasks,
                arm.resolved,
                arm.pass_rate * 100.0,
                arm.total_tokens,
                arm.wall_time_ms,
                arm.regressions,
            ));
        }
        out.push('\n');
        match &self.winner {
            Some(name) => out.push_str(&format!("winner: {}\n", name)),
            None => out.push_str("winner: <tie>\n"),
        }
        out
    }
}

/// Executes one arm on one task under a hard total token budget.
///
/// Implementors must never let `self_tokens + children_tokens` exceed `budget_tokens`.
pub trait BenchExecutor: Send + Sync {
    fn label(&self) -> &'static str;

    fn execute(
        &self,
        task: &BenchTask,
        strategy: &BenchStrategyConfig,
        budget_tokens: u64,
        workspace_root: &Path,
    ) -> BenchArmResult;
}

/// Deterministic synthetic executor used by the built-in smoke suite.
///
/// It exercises the *plumbing* — budget locking, self/children token accounting, post-condition
/// evaluation and report aggregation — without a model endpoint. Tasks whose `id` starts with
/// `fixable-` are resolved by the single agent; tasks whose `id` starts with `stall-` require the
/// adaptive tournament arm to resolve, which mirrors the real escalation story.
pub struct SyntheticBenchExecutor;

impl BenchExecutor for SyntheticBenchExecutor {
    fn label(&self) -> &'static str {
        SYNTHETIC_EXECUTOR_LABEL
    }

    fn execute(
        &self,
        task: &BenchTask,
        strategy: &BenchStrategyConfig,
        budget_tokens: u64,
        workspace_root: &Path,
    ) -> BenchArmResult {
        let started = std::time::Instant::now();
        let _ = std::fs::create_dir_all(workspace_root);

        let uses_tournament = strategy.adaptive_tournament_candidates >= 2;
        let needs_tournament = task.id.starts_with("stall-");
        let single_agent_resolves = task.id.starts_with("fixable-");

        // Materialize the initial workspace for a faithful post-condition check.
        for (path, contents) in &task.files {
            let full = workspace_root.join(path);
            if let Some(parent) = full.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(&full, contents);
        }

        let attempt_applies_fix = if uses_tournament {
            single_agent_resolves || needs_tournament
        } else {
            single_agent_resolves
        };
        if attempt_applies_fix {
            for (path, needle) in &task.expect_contains {
                let full = workspace_root.join(path);
                if let Some(parent) = full.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let mut text = std::fs::read_to_string(&full).unwrap_or_default();
                if !text.contains(needle.as_str()) {
                    text.push_str(needle);
                    text.push('\n');
                }
                let _ = std::fs::write(&full, text);
            }
            for path in &task.expect_absent {
                let _ = std::fs::remove_file(workspace_root.join(path));
            }
        }

        let resolved = task.is_resolved(workspace_root);

        // Deterministic, budget-locked synthetic token accounting: the single agent spends a fixed
        // per-task amount; a tournament arm spends more *children* tokens but still respects the
        // same total budget.
        let per_task_self = 900u64;
        let per_candidate_children = 250u64;
        let candidates = if uses_tournament {
            strategy.adaptive_tournament_candidates as u64
        } else {
            0
        };
        let mut self_tokens = per_task_self;
        let mut children_tokens = candidates * per_candidate_children;
        // Hard budget lock: never report more than the shared per-arm budget.
        if self_tokens + children_tokens > budget_tokens {
            let over = self_tokens + children_tokens - budget_tokens;
            let cut = over.min(children_tokens);
            children_tokens -= cut;
            let remaining = over - cut;
            if remaining > 0 {
                self_tokens = self_tokens.saturating_sub(remaining);
            }
        }

        let gate_status = if resolved { "approved" } else { "rejected" }.to_string();
        let regressions = usize::from(task.regression_guard && !resolved);

        BenchArmResult {
            task_id: task.id.clone(),
            strategy: strategy.name.clone(),
            resolved,
            gate_status,
            self_tokens,
            children_tokens,
            total_tokens: self_tokens + children_tokens,
            wall_time_ms: started.elapsed().as_millis() as u64,
            regressions,
        }
    }
}

/// The built-in self-contained smoke suite (no `--suite` file required).
pub fn builtin_smoke_suite() -> Vec<BenchTask> {
    let mut tasks = Vec::new();
    for i in 1..=3 {
        tasks.push(BenchTask {
            id: format!("fixable-{}", i),
            prompt: format!("Fix the off-by-one bug in module {}.", i),
            files: BTreeMap::from([(
                format!("src/mod{}.rs", i),
                "fn compute() -> i32 { 0 }\n".to_string(),
            )]),
            expect_contains: vec![(format!("src/mod{}.rs", i), "fixed".to_string())],
            expect_absent: Vec::new(),
            regression_guard: false,
        });
    }
    for i in 1..=3 {
        tasks.push(BenchTask {
            id: format!("stall-{}", i),
            prompt: format!(
                "The verification command keeps failing after the first fix attempt ({}).",
                i
            ),
            files: BTreeMap::from([(
                format!("src/stall{}.rs", i),
                "fn compute() -> i32 { 0 }\n".to_string(),
            )]),
            expect_contains: vec![(format!("src/stall{}.rs", i), "fixed".to_string())],
            expect_absent: Vec::new(),
            regression_guard: true,
        });
    }
    tasks
}

/// The default arm pair used when a suite does not declare its own arms.
pub fn default_strategy_pair() -> Vec<BenchStrategyConfig> {
    vec![
        BenchStrategyConfig::single_agent(),
        BenchStrategyConfig::adaptive_tournament(3),
    ]
}

/// Load a benchmark suite from a JSON file.
///
/// Every workspace-relative path in `files`, `expect_contains` and `expect_absent` is validated up
/// front (task id + field name in the error) so a hostile suite cannot make an executor write or
/// read outside the scratch workspace. Failing here — before any arm runs — beats discovering the
/// escape at the first `fs::write`/`read_to_string`.
pub fn load_suite(path: &Path) -> Result<Vec<BenchTask>, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read benchmark suite {}: {}", path.display(), e))?;
    let tasks: Vec<BenchTask> = serde_json::from_str(&text)
        .map_err(|e| format!("invalid benchmark suite {}: {}", path.display(), e))?;
    for task in &tasks {
        validate_task_paths(task)
            .map_err(|e| format!("invalid benchmark suite {}: {}", path.display(), e))?;
    }
    Ok(tasks)
}

/// Validate every workspace-relative path a [`BenchTask`] names, rejecting absolute paths, parent
/// traversal, Windows drive/UNC prefixes, reserved device names and NTFS Alternate Data Streams.
fn validate_task_paths(task: &BenchTask) -> Result<(), String> {
    for path in task.files.keys() {
        WorkspaceCap::validate_relative_components(path).map_err(|e| {
            format!(
                "task '{}': invalid path in 'files' '{}': {}",
                task.id, path, e
            )
        })?;
    }
    for (path, _) in &task.expect_contains {
        WorkspaceCap::validate_relative_components(path).map_err(|e| {
            format!(
                "task '{}': invalid path in 'expect_contains' '{}': {}",
                task.id, path, e
            )
        })?;
    }
    for path in &task.expect_absent {
        WorkspaceCap::validate_relative_components(path).map_err(|e| {
            format!(
                "task '{}': invalid path in 'expect_absent' '{}': {}",
                task.id, path, e
            )
        })?;
    }
    Ok(())
}

/// Run a paired, budget-locked comparison of `strategies` over `tasks`.
///
/// Each arm runs in its own scratch directory under `workspace_root` and receives the identical
/// `budget_tokens`, so the comparison is paired by construction.
pub fn run_paired_benchmark(
    suite_name: &str,
    tasks: &[BenchTask],
    strategies: &[BenchStrategyConfig],
    budget_tokens: u64,
    workspace_root: &Path,
    executor: &dyn BenchExecutor,
) -> BenchComparisonReport {
    let mut results = Vec::new();
    for strategy in strategies {
        for task in tasks {
            let arm_root = workspace_root
                .join(sanitize_arm_name(&strategy.name))
                .join(sanitize_arm_name(&task.id));
            let _ = std::fs::remove_dir_all(&arm_root);
            let _ = std::fs::create_dir_all(&arm_root);
            results.push(executor.execute(task, strategy, budget_tokens, &arm_root));
        }
    }
    BenchComparisonReport::from_results(suite_name, budget_tokens, executor.label(), results)
}

fn sanitize_arm_name(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// The default total budget (tokens) for one arm of a `kroma bench` run.
pub fn default_bench_budget_tokens() -> u64 {
    BudgetConfig::default().max_model_turns as u64 * 2_000
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_paired_benchmark_is_budget_locked_and_picks_a_winner() {
        let dir = std::env::temp_dir().join(format!("kroma-bench-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let tasks = builtin_smoke_suite();
        let strategies = default_strategy_pair();
        let budget = 8_000u64;
        let report = run_paired_benchmark(
            "builtin-smoke",
            &tasks,
            &strategies,
            budget,
            &dir,
            &SyntheticBenchExecutor,
        );

        // Every arm respected the shared budget.
        for r in &report.results {
            assert!(
                r.total_tokens <= budget,
                "arm {} task {} exceeded budget: {} > {}",
                r.strategy,
                r.task_id,
                r.total_tokens,
                budget
            );
        }
        assert_eq!(report.arms.len(), 2);
        // The tournament arm resolves the stall tasks, so it wins on pass rate.
        assert_eq!(report.winner.as_deref(), Some("adaptive_tournament_3"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_synthetic_executor_label_is_marked() {
        let dir = std::env::temp_dir().join(format!("kroma-bench2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let report = run_paired_benchmark(
            "builtin-smoke",
            &builtin_smoke_suite(),
            &default_strategy_pair(),
            8_000,
            &dir,
            &SyntheticBenchExecutor,
        );
        assert_eq!(report.executor, SYNTHETIC_EXECUTOR_LABEL);
        assert!(report.render().contains("synthetic executor"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A hostile suite must not be able to smuggle a workspace-escaping path through any of the
    /// three path-bearing fields; `load_suite` rejects it up front and names the task + field.
    #[test]
    fn test_load_suite_rejects_escaping_paths_in_every_field() {
        let dir = std::env::temp_dir().join(format!("kroma-bench-suite-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let cases: [(&str, &str); 6] = [
            (
                "files",
                r#"[{"id":"t1","prompt":"p","files":{"../escape.txt":"x"}}]"#,
            ),
            (
                "files",
                r#"[{"id":"t1","prompt":"p","files":{"/tmp/abs.txt":"x"}}]"#,
            ),
            (
                "expect_contains",
                r#"[{"id":"t2","prompt":"p","expect_contains":[["../escape.txt","x"]]}]"#,
            ),
            (
                "expect_contains",
                r#"[{"id":"t2","prompt":"p","expect_contains":[["/tmp/abs.txt","x"]]}]"#,
            ),
            (
                "expect_absent",
                r#"[{"id":"t3","prompt":"p","expect_absent":["../escape.txt"]}]"#,
            ),
            (
                "expect_absent",
                r#"[{"id":"t3","prompt":"p","expect_absent":["/tmp/abs.txt"]}]"#,
            ),
        ];
        for (index, (field, body)) in cases.iter().enumerate() {
            let path = dir.join(format!("suite-{}.json", index));
            std::fs::write(&path, body).unwrap();
            let err = load_suite(&path).expect_err("an escaping path must be rejected");
            assert!(err.contains("task '"), "error must name the task: {}", err);
            assert!(err.contains(field), "error must name the field: {}", err);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A legal suite still loads, materializes into its per-arm scratch directory and resolves.
    #[test]
    fn test_load_suite_accepts_legal_paths_and_synthetic_arm_materializes_and_resolves() {
        let dir = std::env::temp_dir().join(format!("kroma-bench-legal-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let suite_path = dir.join("suite.json");
        std::fs::write(
            &suite_path,
            r#"[{"id":"fixable-legal","prompt":"p","files":{"src/mod.rs":"fn compute() -> i32 { 0 }\n"},"expect_contains":[["src/mod.rs","fixed"]],"expect_absent":["gone.txt"]}]"#,
        )
        .unwrap();
        let tasks = load_suite(&suite_path).expect("a legal suite must load");
        assert_eq!(tasks.len(), 1);
        let workspace_root = dir.join("scratch");
        let report = run_paired_benchmark(
            "legal",
            &tasks,
            &[BenchStrategyConfig::single_agent()],
            8_000,
            &workspace_root,
            &SyntheticBenchExecutor,
        );
        assert_eq!(report.results.len(), 1);
        assert!(
            report.results[0].resolved,
            "the synthetic fixable task must resolve"
        );
        assert!(
            workspace_root
                .join("single_agent")
                .join("fixable-legal")
                .join("src/mod.rs")
                .exists(),
            "the legal task file must be materialized inside the arm scratch directory"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `is_resolved` is defense in depth: even a task built in code (bypassing `load_suite`) cannot
    /// use an escaping post-condition path to read outside the workspace.
    #[test]
    fn test_is_resolved_rejects_escaping_post_condition_paths() {
        let dir = std::env::temp_dir().join(format!("kroma-bench-resolve-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Plant a file outside the workspace that an escaping `expect_contains` would otherwise read.
        std::fs::write(dir.join("escape.txt"), "needle").unwrap();
        let workspace = dir.join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        let task = BenchTask {
            id: "hostile".to_string(),
            prompt: "p".to_string(),
            files: std::collections::BTreeMap::new(),
            expect_contains: vec![("../escape.txt".to_string(), "needle".to_string())],
            expect_absent: vec!["/tmp/kroma-should-not-exist".to_string()],
            regression_guard: false,
        };
        assert!(
            !task.is_resolved(&workspace),
            "an escaping post-condition must never count as resolved"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
