//! T17 (KR-12, KR-13, KR-14): a frozen, versioned experiment manifest.
//!
//! An experiment is a claim about a comparison, and a comparison is only meaningful if the inputs are
//! fixed before anything runs. The pieces that were missing:
//!
//! * **No versioned entry point.** Arms, models, prices, sandbox, oracle, repeats and the holdout were
//!   scattered across CLI flags, so "what exactly was compared" could only be reconstructed by reading
//!   the invocation.
//! * **No identity discipline.** Nothing stopped two arms or two tasks from sharing an id, and job
//!   directories were derived by lossy sanitization — `a/b` and `a_b` would land in the same directory
//!   and silently overwrite each other.
//! * **The holdout could not fail.** `holdout_task_ids` was consulted only while *reporting*: an id
//!   that matched nothing produced `None`, and the verdict silently fell back to "all tasks" — which
//!   is exactly the development-set promotion the holdout exists to prevent.
//!
//! This module fixes all three before a single provider call: [`ExperimentManifest::validate`] refuses
//! a broken matrix, [`ExperimentManifest::resolve`] produces the exact job list the runner would
//! schedule (so a dry run and the real run cannot disagree), and the effective manifest is exported
//! with a digest so the record can be tied to the bytes that produced it.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Bumped whenever the manifest shape changes in a way that makes an old file mean something
/// different. A manifest with an unknown version is refused rather than guessed at.
pub const EXPERIMENT_MANIFEST_VERSION: u32 = 1;

/// Hard ceiling on `arms × tasks × repeats`. A matrix larger than this is a configuration mistake or
/// an accident, and either way it must be refused before it spends money.
pub const MAX_EXPERIMENT_JOBS: usize = 10_000;

/// Which side of the comparison an arm is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArmKind {
    Baseline,
    Candidate,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArmSpec {
    pub id: String,
    pub kind: ArmKind,
    /// Model this arm runs. Required: an arm without a model cannot be priced or reproduced.
    pub model: String,
    /// Price table identifier. Required for the same reason.
    pub price_table: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskSpec {
    pub id: String,
    /// The task prompt or the path to it; opaque to the manifest.
    pub source: String,
}

/// Cost/quality accounting basis. Kept explicit because a verdict that mixes the two is not a verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionBasis {
    /// The preregistered decision is taken on the holdout split only.
    Holdout,
    /// No split was declared. Reported as such rather than dressed up as a holdout result.
    AllTasks,
}

impl DecisionBasis {
    pub fn as_str(self) -> &'static str {
        match self {
            DecisionBasis::Holdout => "holdout",
            DecisionBasis::AllTasks => "all_tasks",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExperimentManifest {
    pub version: u32,
    /// Experiment identity. Part of every job directory and of the effective-manifest digest.
    pub id: String,
    pub arms: Vec<ArmSpec>,
    pub tasks: Vec<TaskSpec>,
    /// Explicit holdout task ids. Empty means "no split declared" (`DecisionBasis::AllTasks`);
    /// *declaring* an empty split is an error, because it reads like a holdout and is not one.
    #[serde(default)]
    pub holdout: Vec<String>,
    pub repeats: usize,
    /// Isolation backend the arms run under (`host`, `bwrap`, `container:<runtime>:<image>`).
    #[serde(default)]
    pub sandbox: Option<String>,
    /// Grading oracle. An experiment without one cannot decide anything.
    pub oracle: String,
    /// Token ceiling for the whole matrix.
    #[serde(default)]
    pub max_total_tokens: Option<u64>,
    /// Whether the source text contained a `holdout` key at all. `serde` cannot tell an absent field
    /// from an empty array, and the difference matters: an *absent* field means "no split", a
    /// *declared but empty* one is a mistake that reads like a holdout.
    #[serde(skip)]
    pub declares_holdout: bool,
}

/// Why a manifest was refused. Every variant is raised before any provider call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestError {
    UnsupportedVersion {
        found: u32,
        supported: u32,
    },
    EmptyArms,
    EmptyTasks,
    DuplicateArmId(String),
    DuplicateTaskId(String),
    /// Exactly one baseline is required: a comparison needs a fixed reference point.
    BaselineCount {
        found: usize,
    },
    MissingModel(String),
    MissingPriceTable(String),
    RepeatsZero,
    /// `holdout` was declared (the field is present) but lists nothing.
    DeclaredButEmptyHoldout,
    UnknownHoldoutId(String),
    DuplicateHoldoutId(String),
    HoldoutOverlapsEverything,
    MissingOracle,
    MissingSandboxForCandidate,
    MatrixTooLarge {
        jobs: usize,
        limit: usize,
    },
    BudgetTooSmallForMatrix {
        required: u64,
        declared: u64,
    },
    /// The manifest is not valid JSON, or carries a field this version does not define.
    Malformed(String),
    /// The serialized manifest exceeds the parser's own ceiling.
    ManifestTooLarge {
        bytes: usize,
        limit: usize,
    },
}

impl std::fmt::Display for ManifestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ManifestError::UnsupportedVersion { found, supported } => write!(
                f,
                "experiment manifest version {} is not supported (this build implements {})",
                found, supported
            ),
            ManifestError::EmptyArms => write!(f, "the manifest declares no arms"),
            ManifestError::EmptyTasks => write!(f, "the manifest declares no tasks"),
            ManifestError::DuplicateArmId(id) => write!(f, "duplicate arm id '{}'", id),
            ManifestError::DuplicateTaskId(id) => write!(f, "duplicate task id '{}'", id),
            ManifestError::BaselineCount { found } => {
                write!(f, "exactly one baseline arm is required, found {}", found)
            }
            ManifestError::MissingModel(id) => write!(f, "arm '{}' has no model", id),
            ManifestError::MissingPriceTable(id) => {
                write!(f, "arm '{}' has no price table", id)
            }
            ManifestError::RepeatsZero => write!(f, "repeats must be at least 1"),
            ManifestError::DeclaredButEmptyHoldout => write!(
                f,
                "the holdout split is declared but empty; remove the field to compare on all tasks"
            ),
            ManifestError::UnknownHoldoutId(id) => {
                write!(f, "holdout id '{}' is not a declared task", id)
            }
            ManifestError::DuplicateHoldoutId(id) => {
                write!(f, "holdout id '{}' is listed twice", id)
            }
            ManifestError::HoldoutOverlapsEverything => write!(
                f,
                "the holdout split covers every task, so nothing is left for development"
            ),
            ManifestError::MissingOracle => write!(f, "the manifest declares no oracle"),
            ManifestError::MissingSandboxForCandidate => write!(
                f,
                "a candidate arm is declared without a sandbox; candidates must run isolated"
            ),
            ManifestError::MatrixTooLarge { jobs, limit } => write!(
                f,
                "the matrix schedules {} jobs, over the {} job limit",
                jobs, limit
            ),
            ManifestError::BudgetTooSmallForMatrix { required, declared } => write!(
                f,
                "the matrix needs at least {} tokens but the manifest declares {}",
                required, declared
            ),
            ManifestError::Malformed(msg) => write!(f, "invalid experiment manifest: {}", msg),
            ManifestError::ManifestTooLarge { bytes, limit } => write!(
                f,
                "the manifest is {} bytes, over the {} byte limit",
                bytes, limit
            ),
        }
    }
}

impl std::error::Error for ManifestError {}

/// One scheduled unit of work. The tuple is the identity: `(task, arm, repetition)`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct JobId {
    pub task_id: String,
    pub arm_id: String,
    pub repetition: usize,
}

impl JobId {
    /// Directory-safe encoding that is **not** lossy.
    ///
    /// Each component is hex-encoded, so `a/b` and `a_b` cannot collide the way a sanitizer would let
    /// them, and the separator is `_` which never appears in hex.
    pub fn dir_name(&self) -> String {
        format!(
            "{}_{}_{:04}",
            hex(&self.task_id),
            hex(&self.arm_id),
            self.repetition
        )
    }
}

fn hex(value: &str) -> String {
    let mut out = String::with_capacity(value.len() * 2);
    for byte in value.as_bytes() {
        out.push_str(&format!("{:02x}", byte));
    }
    out
}

/// A validated manifest with the job list and the identity of the whole thing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedExperiment {
    pub manifest: ExperimentManifest,
    pub jobs: Vec<JobId>,
    pub decision_basis: DecisionBasis,
    /// sha256 of the canonical JSON of the manifest as it will be executed.
    pub digest: String,
    /// Total tokens the matrix may spend at most, when declared.
    pub declared_budget: Option<u64>,
}

impl ResolvedExperiment {
    /// The exact bytes a record should store: the effective manifest, not the file the user edited.
    pub fn effective_manifest_json(&self) -> String {
        serde_json::to_string_pretty(&self.manifest).unwrap_or_else(|_| "{}".to_string())
    }

    pub fn baseline_arm(&self) -> &ArmSpec {
        self.manifest
            .arms
            .iter()
            .find(|arm| arm.kind == ArmKind::Baseline)
            .expect("validated manifests always have exactly one baseline")
    }

    /// Is `task_id` in the holdout split?
    pub fn is_holdout(&self, task_id: &str) -> bool {
        self.manifest.holdout.iter().any(|id| id == task_id)
    }

    /// The jobs a dry run prints. Identical to what the runner schedules, because both come from here.
    pub fn job_lines(&self) -> Vec<String> {
        self.jobs.iter().map(|job| job.dir_name()).collect()
    }
}

impl ExperimentManifest {
    /// Parse and validate in one step. Unknown fields are refused by `deny_unknown_fields`, so a typo
    /// in a manifest is an error instead of a silently ignored setting.
    pub fn from_json(text: &str) -> Result<ResolvedExperiment, ManifestError> {
        if text.len() > crate::config_file::MAX_WORKSPACE_CONFIG_BYTES as usize {
            return Err(ManifestError::ManifestTooLarge {
                bytes: text.len(),
                limit: crate::config_file::MAX_WORKSPACE_CONFIG_BYTES as usize,
            });
        }
        let declares_holdout = text.contains("\"holdout\"");
        let mut manifest: ExperimentManifest =
            serde_json::from_str(text).map_err(|e| ManifestError::Malformed(e.to_string()))?;
        manifest.declares_holdout = declares_holdout;
        Self::resolve(manifest)
    }

    pub fn validate(&self) -> Result<(), ManifestError> {
        if self.version != EXPERIMENT_MANIFEST_VERSION {
            return Err(ManifestError::UnsupportedVersion {
                found: self.version,
                supported: EXPERIMENT_MANIFEST_VERSION,
            });
        }
        if self.arms.is_empty() {
            return Err(ManifestError::EmptyArms);
        }
        if self.tasks.is_empty() {
            return Err(ManifestError::EmptyTasks);
        }
        if self.oracle.trim().is_empty() {
            return Err(ManifestError::MissingOracle);
        }
        if self.repeats == 0 {
            return Err(ManifestError::RepeatsZero);
        }

        let mut arm_ids: BTreeSet<&str> = BTreeSet::new();
        let mut baselines = 0usize;
        let mut has_candidate = false;
        for arm in &self.arms {
            if arm.id.trim().is_empty() {
                return Err(ManifestError::DuplicateArmId(String::new()));
            }
            if !arm_ids.insert(arm.id.as_str()) {
                return Err(ManifestError::DuplicateArmId(arm.id.clone()));
            }
            if arm.model.trim().is_empty() {
                return Err(ManifestError::MissingModel(arm.id.clone()));
            }
            if arm.price_table.trim().is_empty() {
                return Err(ManifestError::MissingPriceTable(arm.id.clone()));
            }
            match arm.kind {
                ArmKind::Baseline => baselines += 1,
                ArmKind::Candidate => has_candidate = true,
            }
        }
        if baselines != 1 {
            return Err(ManifestError::BaselineCount { found: baselines });
        }
        if has_candidate && self.sandbox.is_none() {
            return Err(ManifestError::MissingSandboxForCandidate);
        }

        let mut task_ids: BTreeSet<&str> = BTreeSet::new();
        for task in &self.tasks {
            if !task_ids.insert(task.id.as_str()) {
                return Err(ManifestError::DuplicateTaskId(task.id.clone()));
            }
        }

        // The holdout: declared means non-empty, every id must exist, no duplicates, and something has
        // to be left for development. Silently falling back to "all tasks" is the bug this replaces.
        let mut seen_holdout: BTreeSet<&str> = BTreeSet::new();
        for id in &self.holdout {
            if !task_ids.contains(id.as_str()) {
                return Err(ManifestError::UnknownHoldoutId(id.clone()));
            }
            if !seen_holdout.insert(id.as_str()) {
                return Err(ManifestError::DuplicateHoldoutId(id.clone()));
            }
        }
        if self.holdout.is_empty() && self.declares_holdout_field() {
            return Err(ManifestError::DeclaredButEmptyHoldout);
        }
        if !self.holdout.is_empty() && self.holdout.len() == self.tasks.len() {
            return Err(ManifestError::HoldoutOverlapsEverything);
        }

        let jobs = self.matrix_size();
        if jobs > MAX_EXPERIMENT_JOBS {
            return Err(ManifestError::MatrixTooLarge {
                jobs,
                limit: MAX_EXPERIMENT_JOBS,
            });
        }
        if let Some(budget) = self.max_total_tokens {
            // A rough floor: every job needs at least one request of a few hundred tokens. A budget
            // below that is not a budget, it is a typo.
            let required = (jobs as u64).saturating_mul(MIN_TOKENS_PER_JOB);
            if budget < required {
                return Err(ManifestError::BudgetTooSmallForMatrix {
                    required,
                    declared: budget,
                });
            }
        }
        Ok(())
    }

    /// `true` when the JSON the user wrote contained a `holdout` key. The deserializer cannot tell an
    /// absent field from an empty array, so the distinction is made by re-reading the raw text.
    fn declares_holdout_field(&self) -> bool {
        self.declares_holdout
    }

    fn matrix_size(&self) -> usize {
        self.arms
            .len()
            .saturating_mul(self.tasks.len())
            .saturating_mul(self.repeats)
    }

    pub fn resolve(self) -> Result<ResolvedExperiment, ManifestError> {
        self.validate()?;
        let mut jobs = Vec::with_capacity(self.matrix_size());
        for task in &self.tasks {
            for arm in &self.arms {
                for repetition in 0..self.repeats {
                    jobs.push(JobId {
                        task_id: task.id.clone(),
                        arm_id: arm.id.clone(),
                        repetition,
                    });
                }
            }
        }
        // Identity of the whole matrix: unique job directories, no accidental duplicates.
        let mut seen: BTreeMap<String, usize> = BTreeMap::new();
        for job in &jobs {
            let dir = job.dir_name();
            if seen.insert(dir.clone(), seen.len()).is_some() {
                return Err(ManifestError::DuplicateTaskId(format!(
                    "job directory '{}' is produced by two jobs",
                    dir
                )));
            }
        }
        let decision_basis = if self.holdout.is_empty() {
            DecisionBasis::AllTasks
        } else {
            DecisionBasis::Holdout
        };
        let canonical = serde_json::to_string(&self).unwrap_or_default();
        let digest = crate::prompt::sha256_hex64(canonical.as_bytes());
        let declared_budget = self.max_total_tokens;
        Ok(ResolvedExperiment {
            manifest: self,
            jobs,
            decision_basis,
            digest,
            declared_budget,
        })
    }
}

/// Floor used only to catch an obviously wrong budget (a matrix of N jobs cannot cost fewer than N
/// requests).
const MIN_TOKENS_PER_JOB: u64 = 256;

/// How a candidate becomes the new baseline.
///
/// The rule is deliberately blunt: the **holdout** decides. A candidate that looks better on the
/// development split and worse on the holdout is not promoted, no matter how good the development
/// number is — that is the entire purpose of holding data out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Promotion {
    Promote,
    Reject,
    /// The experiment had no holdout split, so it cannot promote anything. Reported instead of being
    /// treated as a win on "all tasks".
    CannotPromoteWithoutHoldout,
    /// The holdout delta is within the interval, i.e. not distinguishable from zero.
    Inconclusive,
}

impl Promotion {
    pub fn as_str(self) -> &'static str {
        match self {
            Promotion::Promote => "promote",
            Promotion::Reject => "reject",
            Promotion::CannotPromoteWithoutHoldout => "cannot_promote_without_holdout",
            Promotion::Inconclusive => "inconclusive",
        }
    }
}

/// Decide promotion from the two splits.
///
/// `holdout` is `(mean_delta, ci_low, ci_high)` for the candidate minus the baseline.
pub fn promotion_verdict(
    basis: DecisionBasis,
    development: Option<(f64, f64, f64)>,
    holdout: Option<(f64, f64, f64)>,
) -> Promotion {
    if basis == DecisionBasis::AllTasks {
        return Promotion::CannotPromoteWithoutHoldout;
    }
    let Some((mean, low, _high)) = holdout else {
        return Promotion::Inconclusive;
    };
    // Development numbers are recorded but never decide. They are accepted so the caller must pass
    // them (and so a report can show them side by side) — not so they can override the holdout.
    let _ = development;
    if mean > 0.0 && low > 0.0 {
        Promotion::Promote
    } else if mean < 0.0 {
        Promotion::Reject
    } else {
        Promotion::Inconclusive
    }
}
