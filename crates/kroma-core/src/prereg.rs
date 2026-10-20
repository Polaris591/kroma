//! M2-01: the preregistration gate in front of the one door that spends money.
//!
//! `kroma bench --executor provider` is the only command in this repository that sends paid model
//! requests. A comparison whose task cluster, model identity, quality margin, noise plan, cache
//! order, budget, stopping rule and holdout policy were not frozen beforehand is not an experiment
//! — it is a search, and any number it produces can be accused of having been picked after the
//! fact. [`check_prereg`] turns that freeze into a condition of running instead of a document
//! nobody opens.
//!
//! It mirrors `scripts/verify-prereg.py` rule for rule: the same eight required fields, dev and
//! holdout naming two existing suites, and a verifier that lives outside the workspace. It adds the
//! one rule the script cannot check, because only the runner knows it: the suite this run will
//! actually score is one the preregistration named.
//!
//! The file is read as `serde_json::Value` on purpose. This is a checklist over an
//! operator-authored document, not a schema; a typed mirror would need an edit every time a
//! preregistration grows a field, and a stale mirror accepts less than it claims.

use serde_json::Value;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// The fields every preregistration must fill in. Same list, same order as
/// `scripts/verify-prereg.py`, so the two gates cannot drift apart on what "complete" means.
pub const REQUIRED_FIELDS: [&str; 8] = [
    "task_cluster_definition",
    "model_identity_observability",
    "quality_margin",
    "noise_and_power_plan",
    "cache_order_design",
    "budget",
    "stopping_rule",
    "holdout_policy",
];

/// Which of the two preregistered suites a run is scoring.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreregSuiteRole {
    Dev,
    Holdout,
}

impl PreregSuiteRole {
    pub fn as_str(self) -> &'static str {
        match self {
            PreregSuiteRole::Dev => "dev",
            PreregSuiteRole::Holdout => "holdout",
        }
    }
}

/// R02: one host file the preregistration authorizes for read-only delivery to the held-out
/// verifier, frozen to the bytes that were authorized.
///
/// The identity is captured by [`check_prereg`] from the same document read that admits the run, so
/// the authority cannot grow afterwards: rewriting the preregistration to point at another file does
/// not change an already-admitted gate, and changing the authorized file's bytes is detected before
/// anything is mounted instead of silently substituting the oracle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifierFileIdentity {
    /// Canonical host path (absolute, regular file, no symlink in the path).
    pub path: PathBuf,
    /// Digest of the file at the moment it was authorized.
    pub sha256: String,
}

impl VerifierFileIdentity {
    /// Freeze `path` to a canonical, regular, non-symlink file and record its digest.
    ///
    /// A symlink (or a path through one) is refused: its target can change after authorization,
    /// which would deliver a file the operator never named.
    pub fn freeze(path: &Path) -> Result<Self, String> {
        if !path.is_absolute() {
            return Err(format!(
                "trusted verifier file '{}' must be an absolute path",
                path.display()
            ));
        }
        let metadata = std::fs::symlink_metadata(path).map_err(|error| {
            format!(
                "trusted verifier file '{}' is not available ({})",
                path.display(),
                error
            )
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(format!(
                "trusted verifier file '{}' must be a regular file reached without symlinks",
                path.display()
            ));
        }
        let canonical = std::fs::canonicalize(path).map_err(|error| {
            format!(
                "trusted verifier file '{}' cannot be resolved ({})",
                path.display(),
                error
            )
        })?;
        if canonical != path {
            return Err(format!(
                "trusted verifier file '{}' resolves to '{}'; pass the real path",
                path.display(),
                canonical.display()
            ));
        }
        let bytes = std::fs::read(&canonical).map_err(|error| {
            format!(
                "trusted verifier file '{}' cannot be read ({})",
                canonical.display(),
                error
            )
        })?;
        Ok(Self {
            path: canonical,
            sha256: format!("{:x}", Sha256::digest(&bytes)),
        })
    }

    /// Re-check the frozen identity immediately before the file is used.
    ///
    /// Returns `Err` when the file disappeared, stopped being a plain file, or its bytes changed:
    /// the caller must treat that as "no verdict", never as a verifier that ran and failed.
    pub fn verify_unchanged(&self) -> Result<(), String> {
        let current = Self::freeze(&self.path)?;
        if current.sha256 != self.sha256 {
            return Err(format!(
                "trusted verifier file '{}' changed after admission (sha256 {} -> {}); the oracle is not silently substituted",
                self.path.display(),
                self.sha256,
                current.sha256
            ));
        }
        Ok(())
    }
}

/// A preregistration a run was admitted under. `sha256` is the digest of the file as it was at
/// admission time; the report carries it so a number can be traced back to the document that
/// authorized it. `verifier_file` is the oracle capability the same document declared, frozen at
/// admission so later edits to the preregistration cannot widen it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreregGate {
    pub path: PathBuf,
    pub sha256: String,
    pub suite_role: PreregSuiteRole,
    pub verifier_file: Option<VerifierFileIdentity>,
}

/// Admit a provider run under `prereg_path`, or list every rule it breaks.
///
/// `workspace_root` resolves the relative suite paths a preregistration records (they were written
/// relative to the repository the experiment runs in). `suite` is the suite the run will actually
/// use; `None` means the built-in smoke suite, which is not a preregistered suite.
///
/// The returned list is empty only when every rule holds — the caller must treat a non-empty list
/// as "do not run", never as a warning.
/// R02: the held-out verifier the preregistration declares (`holdout_policy.verifier_path`).
///
/// This is the operator's explicit authorization for an external oracle file: the preregistration is
/// the signed experiment definition, so the path it names is the only host file an isolated held-out
/// verifier may be given read-only access to. Nothing is inferred from the verifier argv.
pub fn declared_verifier_path(doc: &Value) -> Option<PathBuf> {
    let raw = doc
        .get("holdout_policy")
        .and_then(|block| block.get("verifier_path"))
        .and_then(Value::as_str)?
        .trim();
    if raw.is_empty() {
        return None;
    }
    let path = PathBuf::from(raw);
    path.is_absolute().then_some(path)
}

pub fn check_prereg(
    prereg_path: &Path,
    workspace_root: &Path,
    suite: Option<&Path>,
) -> Result<PreregGate, Vec<String>> {
    let shown = prereg_path.display().to_string();
    let bytes = std::fs::read(prereg_path)
        .map_err(|e| vec![format!("prereg {}: unreadable ({})", shown, e)])?;
    let doc: Value = match serde_json::from_slice(&bytes) {
        Ok(doc) => doc,
        Err(e) => return Err(vec![format!("prereg {}: not JSON ({})", shown, e)]),
    };
    if !doc.is_object() {
        return Err(vec![format!("prereg {}: must be a JSON object", shown)]);
    }

    let mut problems = Vec::new();
    for field in REQUIRED_FIELDS {
        if !is_filled(doc.get(field)) {
            problems.push(format!(
                "prereg {}: missing required field `{}`",
                shown, field
            ));
        }
    }

    let cluster = doc.get("task_cluster_definition");
    let dev = resolve_prereg_suite(
        workspace_root,
        field_str(cluster, "dev_suite"),
        "dev_suite",
        &shown,
        &mut problems,
    );
    let holdout = resolve_prereg_suite(
        workspace_root,
        field_str(cluster, "holdout_suite"),
        "holdout_suite",
        &shown,
        &mut problems,
    );
    if let (Some(dev), Some(holdout)) = (&dev, &holdout) {
        // Two paths pointing at one file is the same suite twice: no independent replication.
        if dev.canonical == holdout.canonical || dev.sha256 == holdout.sha256 {
            problems.push(format!(
                "prereg {}: dev_suite and holdout_suite must be different suites, both name {}",
                shown,
                dev.canonical.display()
            ));
        }
    }

    let verifier = doc
        .get("holdout_policy")
        .and_then(|block| block.get("verifier_path"))
        .and_then(Value::as_str)
        .unwrap_or("");
    if let Err(problem) = check_verifier(verifier, workspace_root, &shown) {
        problems.push(problem);
    }

    let suite_role = match (suite, &dev, &holdout) {
        (None, _, _) => {
            problems.push(format!(
                "prereg {}: the built-in smoke suite is not a preregistered suite; name the dev or \
                 holdout suite with --suite <PATH>",
                shown
            ));
            None
        }
        (Some(suite), dev, holdout) => match suite_role_of(suite, dev, holdout) {
            Ok(role) => Some(role),
            Err(problem) => {
                problems.push(format!("prereg {}: {}", shown, problem));
                None
            }
        },
    };

    // R02: the oracle capability comes from this document, not from a later re-read. A declared
    // path that cannot be frozen (missing, symlink, directory) is an admission problem.
    let verifier_file = match declared_verifier_path(&doc) {
        Some(path) => match VerifierFileIdentity::freeze(&path) {
            Ok(identity) => Some(identity),
            Err(problem) => {
                problems.push(format!("prereg {}: {}", shown, problem));
                None
            }
        },
        None => None,
    };

    match (problems.is_empty(), suite_role) {
        (true, Some(suite_role)) => Ok(PreregGate {
            path: prereg_path.to_path_buf(),
            sha256: format!("{:x}", Sha256::digest(&bytes)),
            suite_role,
            verifier_file,
        }),
        (false, _) => Err(problems),
        // Unreachable by construction: a missing role always pushes a problem. Kept as a refusal
        // rather than a panic so a future edit to the branches above cannot admit a run by mistake.
        (true, None) => Err(vec![format!(
            "prereg {}: no suite role was determined for this run",
            shown
        )]),
    }
}

/// The name and digest of one preregistered suite, resolved and checked to exist as readable JSON.
struct PreregSuite {
    canonical: PathBuf,
    sha256: String,
}

/// `false` for every shape an operator could leave behind while meaning "not written yet": absent,
/// null, empty string, empty list, empty object.
fn is_filled(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::String(text)) => !text.trim().is_empty(),
        Some(Value::Array(items)) => !items.is_empty(),
        Some(Value::Object(map)) => !map.is_empty(),
        Some(_) => true,
    }
}

fn field_str<'a>(block: Option<&'a Value>, key: &str) -> Option<&'a str> {
    block
        .and_then(|block| block.get(key))
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
}

/// Resolve a suite path recorded in the preregistration and prove it is a suite on disk.
///
/// Relative paths are resolved against the workspace root, the way the operator wrote them.
fn resolve_prereg_suite(
    workspace_root: &Path,
    raw: Option<&str>,
    field: &str,
    shown: &str,
    problems: &mut Vec<String>,
) -> Option<PreregSuite> {
    let raw = match raw {
        Some(raw) => raw,
        None => {
            problems.push(format!(
                "prereg {}: missing `task_cluster_definition.{}`",
                shown, field
            ));
            return None;
        }
    };
    let path = if Path::new(raw).is_absolute() {
        PathBuf::from(raw)
    } else {
        workspace_root.join(raw)
    };
    let canonical = match std::fs::canonicalize(&path) {
        Ok(canonical) => canonical,
        Err(e) => {
            problems.push(format!(
                "prereg {}: {} does not exist ({}: {})",
                shown, field, raw, e
            ));
            return None;
        }
    };
    match std::fs::read(&canonical) {
        Ok(bytes) => match serde_json::from_slice::<Value>(&bytes) {
            Ok(_) => Some(PreregSuite {
                canonical,
                sha256: format!("{:x}", Sha256::digest(&bytes)),
            }),
            Err(e) => {
                problems.push(format!(
                    "prereg {}: {} is not readable JSON ({}: {})",
                    shown, field, raw, e
                ));
                None
            }
        },
        Err(e) => {
            problems.push(format!(
                "prereg {}: {} is unreadable ({}: {})",
                shown, field, raw, e
            ));
            None
        }
    }
}

/// The verifier decides whether a task passed, so it must be a real file outside the graded
/// workspace: a verifier the agent can edit is the oracle handing out its own answers.
fn check_verifier(raw: &str, workspace_root: &Path, shown: &str) -> Result<(), String> {
    if raw.trim().is_empty() {
        return Err(format!(
            "prereg {}: missing `holdout_policy.verifier_path`",
            shown
        ));
    }
    let path = Path::new(raw);
    if !path.is_absolute() {
        return Err(format!(
            "prereg {}: verifier_path must be an absolute path (got `{}`)",
            shown, raw
        ));
    }
    if !path.is_file() {
        return Err(format!(
            "prereg {}: verifier_path does not exist ({})",
            shown, raw
        ));
    }
    let verifier = std::fs::canonicalize(path).map_err(|e| {
        format!(
            "prereg {}: cannot resolve verifier_path {}: {}",
            shown, raw, e
        )
    })?;
    if let Ok(workspace) = std::fs::canonicalize(workspace_root) {
        if verifier.starts_with(&workspace) {
            return Err(format!(
                "prereg {}: the verifier must live outside the workspace ({} is inside {})",
                shown,
                verifier.display(),
                workspace.display()
            ));
        }
    }
    Ok(())
}

/// Which preregistered suite the run is scoring.
///
/// A suite is identified by its bytes, not by its path. The preregistration records the path the
/// operator froze (inside the repository), while a real run must point at a copy outside the graded
/// workspace — `load_held_out_suite` refuses an in-workspace suite. Same bytes is therefore the only
/// identity that holds for both, and it is the stricter one: editing the suite changes the digest
/// and the run is refused instead of quietly scoring something nobody froze.
fn suite_role_of(
    suite: &Path,
    dev: &Option<PreregSuite>,
    holdout: &Option<PreregSuite>,
) -> Result<PreregSuiteRole, String> {
    let canonical = std::fs::canonicalize(suite)
        .map_err(|e| format!("cannot resolve the suite to run {}: {}", suite.display(), e))?;
    let bytes = std::fs::read(&canonical)
        .map_err(|e| format!("cannot read the suite to run {}: {}", suite.display(), e))?;
    let digest = format!("{:x}", Sha256::digest(&bytes));
    // Dev is tested first: when both entries happen to hold identical bytes, the run is labelled
    // dev, so a holdout hit is only ever claimed against the holdout's own content.
    if dev.as_ref().is_some_and(|dev| dev.sha256 == digest) {
        return Ok(PreregSuiteRole::Dev);
    }
    if holdout
        .as_ref()
        .is_some_and(|holdout| holdout.sha256 == digest)
    {
        return Ok(PreregSuiteRole::Holdout);
    }
    let named = |entry: &Option<PreregSuite>| match entry {
        Some(entry) => entry.canonical.display().to_string(),
        None => "unreadable".to_string(),
    };
    Err(format!(
        "the suite to run {} is neither the preregistered dev suite ({}) nor the holdout suite \
         ({}); a run may only score a suite the preregistration named",
        canonical.display(),
        named(dev),
        named(holdout)
    ))
}
