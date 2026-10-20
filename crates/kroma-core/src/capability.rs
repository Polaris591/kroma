//! T20 (KR-34): the capability manifest — what is implemented, what is wired, and what has been
//! actually exercised.
//!
//! The problem this solves is documentation drift: a README sentence like "sandboxed execution" reads
//! the same whether the sandbox is wired into the agent's shell, tested only against a fake, or merely
//! designed. Each capability here carries five separate facts, and the manifest is checked against the
//! repository (not against itself):
//!
//! * `implemented` — the code exists.
//! * `wired` — the surfaces that actually call it (`cli`, `rpc`, `bench`, `worker`, `resume`).
//! * `contract_tested` — named regression tests exist and are present in the tree.
//! * `live_tested` — it was exercised against the real thing (a real container runtime, a real
//!   provider), not only a fixture.
//! * `default_on` — whether it applies without configuration.
//!
//! An unimplemented capability cannot claim `default_on`, and a capability tested only against fakes
//! cannot claim `live_tested`.

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// Surfaces a capability can be wired into. Kept as data so a claim like "wired: cli, rpc" is
/// checkable instead of prose.
pub const KNOWN_SURFACES: &[&str] = &["cli", "rpc", "bench", "worker", "resume"];

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityEvidence {
    /// Test names that pin the contract. Each must exist in the tree.
    #[serde(default)]
    pub tests: Vec<String>,
    /// What was exercised for real, when `live_tested` is true.
    #[serde(default)]
    pub live: Vec<String>,
    /// The task that produced this capability (for navigation).
    #[serde(default)]
    pub task: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Capability {
    pub id: String,
    pub summary: String,
    pub implemented: bool,
    #[serde(default)]
    pub wired: Vec<String>,
    pub contract_tested: bool,
    pub live_tested: bool,
    pub default_on: bool,
    /// CLI flags this capability documents, if any. Checked against the argument parser.
    #[serde(default)]
    pub cli_flags: Vec<String>,
    #[serde(default)]
    pub evidence: CapabilityEvidence,
    /// Known limits, stated where the capability is claimed.
    #[serde(default)]
    pub limits: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityManifest {
    pub schema_version: u32,
    /// Version of the source tree this manifest describes.
    pub package_version: String,
    /// Capabilities whose job is to prevent something, not to save something. C00-12: a dormancy
    /// tax may never be lowered by deleting one of these, so the manifest names them and validation
    /// refuses a manifest where one has disappeared or stopped being wired.
    #[serde(default)]
    pub safety_capabilities: Vec<String>,
    pub capabilities: Vec<Capability>,
}

/// A documented claim that does not match the code or the evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestProblem {
    UnsupportedSchema(u32),
    DuplicateId(String),
    UnknownSurface {
        id: String,
        surface: String,
    },
    DefaultOnWithoutImplementation(String),
    DefaultOnWithoutWiring(String),
    ContractTestedWithoutTests(String),
    LiveTestedWithoutEvidence(String),
    MissingEvidence {
        id: String,
        test: String,
    },
    /// A capability the manifest itself calls safety-critical is gone, unimplemented or unwired.
    SafetyCapabilityMissing(String),
}

impl std::fmt::Display for ManifestProblem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ManifestProblem::UnsupportedSchema(v) => {
                write!(f, "capability manifest schema {} is not supported", v)
            }
            ManifestProblem::DuplicateId(id) => write!(f, "duplicate capability id '{}'", id),
            ManifestProblem::UnknownSurface { id, surface } => write!(
                f,
                "capability '{}' claims an unknown surface '{}'",
                id, surface
            ),
            ManifestProblem::DefaultOnWithoutImplementation(id) => write!(
                f,
                "capability '{}' claims default_on without being implemented",
                id
            ),
            ManifestProblem::DefaultOnWithoutWiring(id) => write!(
                f,
                "capability '{}' claims default_on without being wired into any surface",
                id
            ),
            ManifestProblem::ContractTestedWithoutTests(id) => write!(
                f,
                "capability '{}' claims contract_tested but names no test",
                id
            ),
            ManifestProblem::LiveTestedWithoutEvidence(id) => write!(
                f,
                "capability '{}' claims live_tested but names no live evidence",
                id
            ),
            ManifestProblem::MissingEvidence { id, test } => write!(
                f,
                "capability '{}' cites test '{}' which is not in the tree",
                id, test
            ),
            ManifestProblem::SafetyCapabilityMissing(id) => write!(
                f,
                "safety capability '{}' is missing, unimplemented or no longer wired - the dormancy \
                 tax may not be lowered by removing a safety capability",
                id
            ),
        }
    }
}

impl std::error::Error for ManifestProblem {}

pub const CAPABILITY_SCHEMA_VERSION: u32 = 1;

// Trusted safety baseline: the editable manifest MUST NOT define its own minimum.
// Changing this list requires an explicit code review and fresh regression evidence.
pub const REQUIRED_SAFETY_CAPABILITIES: &[&str] = &[
    "host_verification_policy",
    "host_budget_ceiling",
    "request_budget_lifecycle",
    "bounded_io",
    "wal_capacity",
    "windows_writer_ownership",
    "execution_boundary",
    "container_lifecycle",
    "mcp_handshake",
    "provenance_identity",
    "ahe_isolation",
];

impl CapabilityManifest {
    pub fn from_json(text: &str) -> Result<Self, String> {
        serde_json::from_str(text).map_err(|e| e.to_string())
    }

    /// The product gate: [`Self::validate_structure`] plus [`Self::validate_safety_baseline`].
    ///
    /// `docs/capabilities.json` is validated through this entry point, so a manifest can never be
    /// green by dropping the trusted safety list and can never be green by contradicting itself.
    pub fn validate(&self) -> Result<(), ManifestProblem> {
        self.validate_structure()?;
        self.validate_safety_baseline()
    }

    /// Internal consistency: no claim that contradicts another claim.
    ///
    /// This layer deliberately says nothing about [`REQUIRED_SAFETY_CAPABILITIES`]. It is the part a
    /// structural fixture can satisfy with a single capability, so a test about duplicate ids or an
    /// unknown surface does not have to restate the product's whole safety baseline to reach its own
    /// assertion. Anything that gates a release must call [`Self::validate`] instead.
    pub fn validate_structure(&self) -> Result<(), ManifestProblem> {
        if self.schema_version != CAPABILITY_SCHEMA_VERSION {
            return Err(ManifestProblem::UnsupportedSchema(self.schema_version));
        }
        let mut seen: BTreeSet<&str> = BTreeSet::new();
        for capability in &self.capabilities {
            if !seen.insert(capability.id.as_str()) {
                return Err(ManifestProblem::DuplicateId(capability.id.clone()));
            }
            for surface in &capability.wired {
                if !KNOWN_SURFACES.contains(&surface.as_str()) {
                    return Err(ManifestProblem::UnknownSurface {
                        id: capability.id.clone(),
                        surface: surface.clone(),
                    });
                }
            }
            if capability.default_on && !capability.implemented {
                return Err(ManifestProblem::DefaultOnWithoutImplementation(
                    capability.id.clone(),
                ));
            }
            if capability.default_on && capability.wired.is_empty() {
                return Err(ManifestProblem::DefaultOnWithoutWiring(
                    capability.id.clone(),
                ));
            }
            if capability.contract_tested && capability.evidence.tests.is_empty() {
                return Err(ManifestProblem::ContractTestedWithoutTests(
                    capability.id.clone(),
                ));
            }
            if capability.live_tested && capability.evidence.live.is_empty() {
                return Err(ManifestProblem::LiveTestedWithoutEvidence(
                    capability.id.clone(),
                ));
            }
        }
        Ok(())
    }

    /// The trusted safety baseline: the compiled-in minimum is present, implemented and wired, and
    /// every capability the manifest itself calls safety-critical satisfies the same rule.
    ///
    /// [`REQUIRED_SAFETY_CAPABILITIES`] is compiled in on purpose: the editable manifest must not be
    /// able to lower the dormancy tax by deleting the list that measures it.
    pub fn validate_safety_baseline(&self) -> Result<(), ManifestProblem> {
        for &required in REQUIRED_SAFETY_CAPABILITIES {
            if !self
                .safety_capabilities
                .iter()
                .any(|id| id.as_str() == required)
            {
                return Err(ManifestProblem::SafetyCapabilityMissing(
                    required.to_string(),
                ));
            }
        }
        for safety in &self.safety_capabilities {
            match self.capabilities.iter().find(|c| &c.id == safety) {
                // Not defined at all is the strongest form of removal, so it gets the same verdict
                // and the same message rather than a separate "unknown id" complaint.
                None => return Err(ManifestProblem::SafetyCapabilityMissing(safety.clone())),
                Some(capability) if !capability.implemented || capability.wired.is_empty() => {
                    return Err(ManifestProblem::SafetyCapabilityMissing(safety.clone()))
                }
                Some(_) => {}
            }
        }
        Ok(())
    }

    /// Check every cited test name against the repository's test sources.
    ///
    /// `sources` is the concatenated text of the test files (the caller reads them), so this stays a
    /// pure function and the check is "the name appears in a test file", not "someone wrote it down".
    pub fn validate_evidence(&self, sources: &str) -> Result<(), ManifestProblem> {
        for capability in &self.capabilities {
            for test in &capability.evidence.tests {
                if !sources.contains(test.as_str()) {
                    return Err(ManifestProblem::MissingEvidence {
                        id: capability.id.clone(),
                        test: test.clone(),
                    });
                }
            }
        }
        Ok(())
    }

    /// Is a report from `report_version` usable as evidence for `current_version`?
    ///
    /// No. An old gate report describes a different binary; treating it as current evidence is how a
    /// release gate passes on work that was never re-run.
    pub fn evidence_is_current(report_version: &str, current_version: &str) -> bool {
        !report_version.trim().is_empty()
            && !current_version.trim().is_empty()
            && report_version.trim() == current_version.trim()
    }
}
