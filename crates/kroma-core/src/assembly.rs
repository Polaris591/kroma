//! T11 (KR-08, KR-33): one assembly path, and honest accounting for experimental mechanisms.
//!
//! Two problems this module exists to prevent:
//!
//! 1. **Divergence.** CLI, RPC, bench, a worker child and a `--resume` each built their own runner, so
//!    the same configuration could produce different budgets, policies or execution boundaries
//!    depending on which door the run came through. [`RunPlan`] is derived from the resolved
//!    configuration alone, and [`RunPlan::digest`] deliberately ignores the surface: equal digests
//!    mean equal capability.
//!
//! 2. **"Enabled but zero-op".** A mechanism could be switched on in configuration, never actually
//!    run, and still show up as a win. [`FeatureLedger`] keeps `configured`, `eligible`, `executed`
//!    and `skipped(reason)` apart, and [`FeatureLedger::headline_gains`] only ever reports
//!    `executed` counts — a mechanism that was configured but never eligible contributes nothing.

use crate::config_file::ResolvedConfig;
use crate::context_projector::{ProjectionCandidate, ProjectionProfile, ProjectionSelector};
use std::collections::{BTreeMap, BTreeSet};

/// Which door a run came through. Recorded for evidence; deliberately excluded from the digest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Surface {
    Cli,
    Rpc,
    Bench,
    Worker,
    Resume,
}

impl Surface {
    pub fn as_str(self) -> &'static str {
        match self {
            Surface::Cli => "cli",
            Surface::Rpc => "rpc",
            Surface::Bench => "bench",
            Surface::Worker => "worker",
            Surface::Resume => "resume",
        }
    }

    pub fn all() -> [Surface; 5] {
        [
            Surface::Cli,
            Surface::Rpc,
            Surface::Bench,
            Surface::Worker,
            Surface::Resume,
        ]
    }
}

/// A mechanism whose activity has to be accounted for rather than assumed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Feature {
    /// Racing several candidates when the single agent stalls (`adaptive_tournament`).
    CounterfactualArm,
    /// Step-down / superseded-read / dead-end compaction of tool output.
    ContextCompaction,
    /// The revision snapshot store (`--list-revisions`, `--undo`).
    SnapshotStore,
    /// Tool discovery: the registry the run can actually call.
    ToolDiscovery,
    /// Experience candidates imported from an operator file.
    Experience,
}

impl Feature {
    pub fn as_str(self) -> &'static str {
        match self {
            Feature::CounterfactualArm => "counterfactual_arm",
            Feature::ContextCompaction => "context_compaction",
            Feature::SnapshotStore => "snapshot_store",
            Feature::ToolDiscovery => "tool_discovery",
            Feature::Experience => "experience",
        }
    }

    pub fn all() -> [Feature; 5] {
        [
            Feature::CounterfactualArm,
            Feature::ContextCompaction,
            Feature::SnapshotStore,
            Feature::ToolDiscovery,
            Feature::Experience,
        ]
    }
}

/// Why a plan cannot be assembled. Every variant is a *configuration* error: it is reported before
/// the run starts, never as a silent degradation in the middle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AssemblyError {
    /// A mechanism was requested but something it depends on is absent.
    MissingDependency {
        feature: Feature,
        requires: &'static str,
    },
    /// A value is out of range (0 where > 0 is required, an unparseable profile, ...).
    InvalidValue { key: &'static str, reason: String },
    /// A projection candidate the capability contract reserves was configured, but no algorithm in
    /// this tree implements it. Refused by name: quietly projecting with the base profile instead
    /// would let a run report an algorithm it never ran.
    UnimplementedCandidate { id: &'static str },
}

impl std::fmt::Display for AssemblyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AssemblyError::MissingDependency { feature, requires } => write!(
                f,
                "{} is configured but requires {}; refusing to start with a mechanism that cannot run",
                feature.as_str(),
                requires
            ),
            AssemblyError::InvalidValue { key, reason } => write!(f, "invalid {}: {}", key, reason),
            AssemblyError::UnimplementedCandidate { id } => write!(
                f,
                "projection candidate `{}` is declared but not implemented in this tree; refusing \
                 to run with a different projection than the one configured",
                id
            ),
        }
    }
}

impl std::error::Error for AssemblyError {}

/// Environment facts that are not configuration: whether a service could actually be constructed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanEnvironment {
    /// The revision snapshot store. `true` is the default because the store is core and always
    /// constructed; a caller that knows otherwise must say so.
    pub snapshot_store_available: bool,
    /// Operator-supplied experience candidates, when one was given.
    pub experience_file: Option<String>,
}

impl Default for PlanEnvironment {
    fn default() -> Self {
        Self {
            snapshot_store_available: true,
            experience_file: None,
        }
    }
}

/// The budget half of a plan.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PlanBudget {
    pub max_total_tokens: Option<u64>,
    pub max_subagents: Option<usize>,
    pub model: Option<String>,
    pub allowed_models: Option<Vec<String>>,
}

/// The policy half of a plan.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PlanPolicy {
    pub mode: Option<String>,
    pub verify_command: Option<String>,
    pub mcp_server_count: usize,
    pub execution_backend: Option<String>,
}

/// Everything a run needs that is decided by configuration alone, identical across surfaces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunPlan {
    /// Which door this instance came through. Excluded from [`Self::digest`].
    pub surface: Surface,
    pub budget: PlanBudget,
    pub policy: PlanPolicy,
    /// The profile the run will actually project with, canonicalised. An unset `projection_profile`
    /// resolves to [`ProjectionProfile::default`] (`p2`) — the same value the runtime uses — so an
    /// unset profile and an explicit `p2` are one plan, digest included.
    pub projection_profile: Option<String>,
    pub adaptive_tournament: Option<usize>,
    pub features: BTreeMap<Feature, FeatureActivity>,
}

/// Static activity of one mechanism in a plan: whether it is on, and whether it *can* run.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FeatureActivity {
    pub configured: bool,
    pub eligible: bool,
    /// Empty when eligible; otherwise the reason it is not.
    pub reason: String,
}

impl RunPlan {
    /// Assemble a plan from the resolved configuration.
    ///
    /// Dependencies are checked here, at configuration time, so a run never starts with a mechanism
    /// that cannot possibly execute. Rules:
    ///
    /// | feature | requires |
    /// | --- | --- |
    /// | `counterfactual_arm` | an isolated execution backend, and a positive candidate count |
    /// | `context_compaction` | a projection profile that compacts (`p1`/`p2`/`p3`; unset = `p2`) |
    /// | `snapshot_store` | nothing (the revision store is core) |
    /// | `tool_discovery` | nothing (the registry is core) |
    /// | `experience` | an operator-supplied candidate file (not representable in config today) |
    ///
    /// Naming a `ctx.*` projection candidate additionally requires an implementation of that
    /// candidate, and this tree has none, so naming one is a configuration error rather than a run
    /// that silently projects with the base profile.
    pub fn from_resolved(config: &ResolvedConfig, surface: Surface) -> Result<Self, AssemblyError> {
        Self::from_resolved_with_env(config, surface, &PlanEnvironment::default())
    }

    /// [`Self::from_resolved`] with the environment facts a caller may know better than the config
    /// file (today: whether the revision snapshot store could be constructed). Kept explicit so a
    /// missing dependency is a *stated* input, never a guess.
    pub fn from_resolved_with_env(
        config: &ResolvedConfig,
        surface: Surface,
        env: &PlanEnvironment,
    ) -> Result<Self, AssemblyError> {
        let isolated = matches!(
            config.execution_sandbox.as_deref(),
            Some(v) if !v.eq_ignore_ascii_case("host")
        );
        let mut features: BTreeMap<Feature, FeatureActivity> = BTreeMap::new();

        // Counterfactual arm: an experimental mechanism that must actually run somewhere isolated,
        // otherwise it is "enabled but zero-op".
        let candidates = config.adaptive_tournament.unwrap_or(0);
        if candidates > 0 {
            if !env.snapshot_store_available {
                return Err(AssemblyError::MissingDependency {
                    feature: Feature::CounterfactualArm,
                    requires: "the revision snapshot store",
                });
            }
            if !isolated {
                return Err(AssemblyError::MissingDependency {
                    feature: Feature::CounterfactualArm,
                    requires:
                        "an isolated execution backend (execution_sandbox = bwrap or container:...)",
                });
            }
            features.insert(
                Feature::CounterfactualArm,
                FeatureActivity {
                    configured: true,
                    eligible: true,
                    reason: String::new(),
                },
            );
        } else {
            features.insert(
                Feature::CounterfactualArm,
                FeatureActivity {
                    configured: false,
                    eligible: false,
                    reason: "adaptive_tournament is 0 or unset".to_string(),
                },
            );
        }

        // The plan must name the projection the runtime will use: an unset `projection_profile`
        // means `ProjectionProfile::default()` (`p2`), not `baseline`. Parsing through the one
        // selector also canonicalises case and whitespace, so equal plans have equal digests, and a
        // `ctx.*` candidate is resolved in its own namespace instead of being reinterpreted as a
        // `p1`/`p2`/`p3` name.
        let selector = config
            .projection_profile
            .as_deref()
            .map(ProjectionSelector::parse)
            .transpose()
            .map_err(|reason| AssemblyError::InvalidValue {
                key: "projection_profile",
                reason,
            })?;
        // A candidate adds a stage over its base profile, so the run must name the candidate it
        // will actually execute; naming the base would let a report claim an algorithm it never
        // ran. Candidates with no algorithm in this tree are still refused by name.
        let (profile, candidate_id) = match selector {
            None => (ProjectionProfile::default(), None),
            Some(ProjectionSelector::Legacy(profile)) => (profile, None),
            Some(ProjectionSelector::Candidate(candidate @ ProjectionCandidate::TaskStateV1)) => {
                (candidate.base_profile(), Some(candidate.id()))
            }
            Some(ProjectionSelector::Candidate(candidate)) => {
                return Err(AssemblyError::UnimplementedCandidate { id: candidate.id() })
            }
        };
        if profile.compacts() {
            features.insert(
                Feature::ContextCompaction,
                FeatureActivity {
                    configured: true,
                    eligible: true,
                    reason: String::new(),
                },
            );
        } else {
            features.insert(
                Feature::ContextCompaction,
                FeatureActivity {
                    configured: false,
                    eligible: false,
                    reason: format!("projection profile is {}", profile.as_str()),
                },
            );
        }

        features.insert(
            Feature::SnapshotStore,
            FeatureActivity {
                configured: true,
                eligible: true,
                reason: String::new(),
            },
        );
        features.insert(
            Feature::ToolDiscovery,
            FeatureActivity {
                configured: true,
                eligible: true,
                reason: String::new(),
            },
        );
        match &env.experience_file {
            Some(path) if !path.trim().is_empty() => {
                features.insert(
                    Feature::Experience,
                    FeatureActivity {
                        configured: true,
                        eligible: true,
                        reason: String::new(),
                    },
                );
            }
            _ => {
                features.insert(
                    Feature::Experience,
                    FeatureActivity {
                        configured: false,
                        eligible: false,
                        reason: "no operator experience file was supplied".to_string(),
                    },
                );
            }
        }

        Ok(Self {
            surface,
            budget: PlanBudget {
                max_total_tokens: config.max_tokens,
                max_subagents: config.max_subagents,
                model: config.model.clone(),
                allowed_models: config.allowed_models.clone(),
            },
            policy: PlanPolicy {
                mode: config.policy.clone(),
                verify_command: config.verify_command.clone(),
                mcp_server_count: config.mcp_servers.len(),
                execution_backend: config.execution_sandbox.clone(),
            },
            projection_profile: Some(
                candidate_id
                    .map(str::to_string)
                    .unwrap_or_else(|| profile.as_str().to_string()),
            ),
            adaptive_tournament: config.adaptive_tournament,
            features,
        })
    }

    /// A stable digest of everything except the surface and the runtime counters.
    ///
    /// Two runs of the same configuration through different doors must produce the same digest; that
    /// equality is the actual claim "the same config gets the same base capabilities".
    pub fn digest(&self) -> String {
        let mut canonical = String::new();
        canonical.push_str(&format!(
            "budget:tokens={:?};subagents={:?};model={:?};allow={:?}\n",
            self.budget.max_total_tokens,
            self.budget.max_subagents,
            self.budget.model,
            self.budget.allowed_models
        ));
        canonical.push_str(&format!(
            "policy:mode={:?};verify={:?};mcp={};exec={:?}\n",
            self.policy.mode,
            self.policy.verify_command,
            self.policy.mcp_server_count,
            self.policy.execution_backend
        ));
        canonical.push_str(&format!(
            "projection:{:?};tournament:{:?}\n",
            self.projection_profile, self.adaptive_tournament
        ));
        for (feature, activity) in &self.features {
            canonical.push_str(&format!(
                "feature:{};configured={};eligible={};reason={}\n",
                feature.as_str(),
                activity.configured,
                activity.eligible,
                activity.reason
            ));
        }
        crate::prompt::sha256_hex64(canonical.as_bytes())
    }

    /// Features that are configured but cannot run, with the reason. Non-empty here means the plan
    /// should have been rejected; it is exposed so a caller cannot accidentally ignore it.
    pub fn ineligible(&self) -> Vec<(Feature, &str)> {
        self.features
            .iter()
            .filter(|(_, a)| a.configured && !a.eligible)
            .map(|(f, a)| (*f, a.reason.as_str()))
            .collect()
    }
}

/// Runtime accounting for one feature in one run.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FeatureCounters {
    /// Times the mechanism was turned on by configuration.
    pub configured: u64,
    /// Times the preconditions for running it were actually met.
    pub eligible: u64,
    /// Times it actually ran and produced a result.
    pub executed: u64,
    /// Times it was eligible but skipped, with the reason and how often.
    pub skipped: BTreeMap<String, u64>,
}

impl FeatureCounters {
    pub fn record_configured(&mut self) {
        self.configured += 1;
    }

    pub fn record_eligible(&mut self) {
        self.eligible += 1;
    }

    pub fn record_executed(&mut self) {
        self.executed += 1;
    }

    pub fn record_skipped(&mut self, reason: impl Into<String>) {
        *self.skipped.entry(reason.into()).or_insert(0) += 1;
    }
}

/// Per-run accounting for every feature.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FeatureLedger {
    counters: BTreeMap<Feature, FeatureCounters>,
}

impl FeatureLedger {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn counters(&self, feature: Feature) -> Option<&FeatureCounters> {
        self.counters.get(&feature)
    }

    fn entry(&mut self, feature: Feature) -> &mut FeatureCounters {
        self.counters.entry(feature).or_default()
    }

    pub fn record_configured(&mut self, feature: Feature) {
        self.entry(feature).record_configured();
    }

    pub fn record_eligible(&mut self, feature: Feature) {
        self.entry(feature).record_eligible();
    }

    pub fn record_executed(&mut self, feature: Feature) {
        self.entry(feature).record_executed();
    }

    pub fn record_skipped(&mut self, feature: Feature, reason: impl Into<String>) {
        self.entry(feature).record_skipped(reason);
    }

    /// The only numbers a report may present as a gain.
    ///
    /// A feature that was configured but never eligible, or eligible but never executed, does not
    /// appear here at all — "configured" is not "ran", and an unexecuted mechanism has no effect to
    /// report. Skipped counts are returned separately so they can be shown as *no* result rather than
    /// folded into one.
    pub fn headline_gains(&self) -> Vec<(Feature, u64)> {
        self.counters
            .iter()
            .filter(|(_, c)| c.executed > 0)
            .map(|(f, c)| (*f, c.executed))
            .collect()
    }

    /// Features that were configured but produced nothing, with why. This is what a report must say
    /// instead of inventing a gain.
    pub fn zero_execution_report(&self) -> Vec<(Feature, &FeatureCounters)> {
        self.counters
            .iter()
            .filter(|(_, c)| c.executed == 0)
            .map(|(f, c)| (*f, c))
            .collect()
    }

    /// A one-line-per-feature summary for evidence logs.
    pub fn summary(&self) -> String {
        let mut out = String::new();
        for (feature, c) in &self.counters {
            let skipped: Vec<String> = c
                .skipped
                .iter()
                .map(|(reason, n)| format!("{}x {}", n, reason))
                .collect();
            out.push_str(&format!(
                "{}: configured={} eligible={} executed={} skipped=[{}]\n",
                feature.as_str(),
                c.configured,
                c.eligible,
                c.executed,
                skipped.join(", ")
            ));
        }
        out
    }
}

/// The set of features a plan says are eligible; used by the matrix test to prove every surface
/// agrees and by callers to seed the ledger.
pub fn eligible_features(plan: &RunPlan) -> BTreeSet<Feature> {
    plan.features
        .iter()
        .filter(|(_, a)| a.eligible)
        .map(|(f, _)| *f)
        .collect()
}
