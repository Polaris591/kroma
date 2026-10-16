//! B23: the change surface a *self-repair* patch is allowed to touch.
//!
//! An offline AHE loop proposes a patch from a failure trajectory. Such a patch must be able to fix
//! ordinary code and must **never** be able to rewrite the things that judge it: the security
//! policy, the release trust root, the scorer, the held-out answers, or the evidence rules. This
//! module is the guard that enforces that boundary before a candidate is applied.
//!
//! What it is: a **path-level** guard. It answers "does this candidate touch a protected artifact?"
//! What it is not: a semantic guard. A patch that only edits an allowed file can still change
//! behaviour (that is the point of repairing), and a path-level check cannot see a build script that
//! pulls in a sibling file. Enforcement is therefore deliberately layered: this check runs before
//! applying a candidate, the patch is applied in an isolated worktree, and the result is re-verified
//! against the frozen oracle afterwards.
//!
//! Matching rules are chosen to be boring and hard to trick:
//!  * paths are compared after rejecting absolute paths and any `..` component,
//!  * comparison is case-insensitive (macOS/Windows filesystems are), and
//!  * both the exact path and its file name are matched, so a protected file cannot be reached by
//!    moving it between directories.

use std::path::{Component, Path, PathBuf};

/// Why a proposed change was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelfRepairViolation {
    /// The path is absolute or escapes the workspace with `..`.
    UnsafePath(String),
    /// The path belongs to a protected artifact.
    Protected { path: String, protected: String },
}

impl std::fmt::Display for SelfRepairViolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SelfRepairViolation::UnsafePath(path) => write!(
                f,
                "self-repair candidate uses an unsafe path '{}'",
                path
            ),
            SelfRepairViolation::Protected { path, protected } => write!(
                f,
                "self-repair candidate touches protected artifact '{}' ('{}'); a patch may not \\
                 rewrite what judges it",
                path, protected
            ),
        }
    }
}

impl std::error::Error for SelfRepairViolation {}

/// One protected artifact: a path (or file name) plus why it is protected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtectedArtifact {
    /// Exact repository-relative path, when the artifact has a fixed location.
    pub path: Option<String>,
    /// File name that is protected wherever it appears.
    pub file_name: Option<String>,
    pub reason: &'static str,
}

impl ProtectedArtifact {
    fn at(path: &str, reason: &'static str) -> Self {
        Self {
            path: Some(path.to_string()),
            file_name: None,
            reason,
        }
    }

    fn named(file_name: &str, reason: &'static str) -> Self {
        Self {
            path: None,
            file_name: Some(file_name.to_string()),
            reason,
        }
    }

    fn matches(&self, normalized: &str, file_name: &str) -> bool {
        if let Some(path) = &self.path {
            if path.eq_ignore_ascii_case(normalized) {
                return true;
            }
        }
        if let Some(name) = &self.file_name {
            if name.eq_ignore_ascii_case(file_name) {
                return true;
            }
        }
        false
    }
}

/// The artifacts no self-repair patch may touch.
///
/// Kept as data so the list is auditable: every entry says what it protects and why.
pub fn protected_artifacts() -> Vec<ProtectedArtifact> {
    vec![
        // Security policy: the code that decides what the agent may do.
        ProtectedArtifact::at(
            "crates/kroma-core/src/tool/policy.rs",
            "security policy engine",
        ),
        ProtectedArtifact::at(
            "crates/kroma-core/src/sensitive_paths.rs",
            "sensitive-path guard",
        ),
        ProtectedArtifact::at(
            "crates/kroma-core/src/secure_fs.rs",
            "no-follow filesystem primitives",
        ),
        ProtectedArtifact::at(
            "crates/kroma-core/src/sandbox.rs",
            "sandbox/isolation enforcement",
        ),
        ProtectedArtifact::at(
            "crates/kroma-core/src/provenance.rs",
            "provenance and identity evidence",
        ),
        ProtectedArtifact::at(
            "crates/kroma-core/src/self_repair_guard.rs",
            "the guard itself",
        ),
        // Release trust root: the code that decides what may be installed.
        ProtectedArtifact::at(
            "bin/kroma.js",
            "release trust root and installer",
        ),
        ProtectedArtifact::at(
            "npm/kroma-cli/bin/kroma.js",
            "release trust root and installer (npm copy)",
        ),
        ProtectedArtifact::at(
            "scripts/generate-release-manifest.js",
            "release signing tool",
        ),
        // Scorer and held-out answers.
        ProtectedArtifact::named("smoke_solutions.json", "held-out reference answers"),
        ProtectedArtifact::at(
            "crates/kroma-core/src/usage_cost.rs",
            "cost accounting used by scoring",
        ),
        // Evidence rules.
        ProtectedArtifact::at("rust-toolchain.toml", "pinned toolchain for reproducible evidence"),
    ]
}

/// The change surface for one self-repair candidate.
#[derive(Debug, Clone, Default)]
pub struct SelfRepairSurface {
    /// Repository-relative directory prefixes the patch may touch. Empty means "nothing is allowed",
    /// which is the safe default for a new integration.
    pub allowed_prefixes: Vec<String>,
    /// Extra protected artifacts for this deployment.
    pub extra_protected: Vec<ProtectedArtifact>,
}

impl SelfRepairSurface {
    /// The surface used by the offline AHE demo: ordinary source and docs, nothing else.
    pub fn source_only() -> Self {
        Self {
            allowed_prefixes: vec![
                "crates/kroma-core/src/".to_string(),
                "crates/kroma-core/tests/".to_string(),
                "crates/kroma-cli/src/".to_string(),
                "docs/".to_string(),
            ],
            extra_protected: Vec::new(),
        }
    }

    pub fn with_extra_protected(mut self, artifact: ProtectedArtifact) -> Self {
        self.extra_protected.push(artifact);
        self
    }

    /// Check one candidate path list. Refuses the whole candidate on the first violation.
    pub fn check<'a, I>(&self, paths: I) -> Result<(), SelfRepairViolation>
    where
        I: IntoIterator<Item = &'a str>,
    {
        for raw in paths {
            let normalized = normalize_relative(raw)?;
            let file_name = normalized
                .rsplit('/')
                .next()
                .unwrap_or(normalized.as_str())
                .to_string();
            for artifact in protected_artifacts()
                .iter()
                .chain(self.extra_protected.iter())
            {
                if artifact.matches(&normalized, &file_name) {
                    return Err(SelfRepairViolation::Protected {
                        path: raw.to_string(),
                        protected: artifact.reason.to_string(),
                    });
                }
            }
            let allowed = self
                .allowed_prefixes
                .iter()
                .any(|prefix| normalized.starts_with(prefix.as_str()));
            if !allowed {
                return Err(SelfRepairViolation::UnsafePath(raw.to_string()));
            }
        }
        Ok(())
    }
}

/// Reject absolute paths and `..` traversal, then normalize to a `/`-separated relative path.
fn normalize_relative(raw: &str) -> Result<String, SelfRepairViolation> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(SelfRepairViolation::UnsafePath(raw.to_string()));
    }
    // Windows-style prefixes and drive letters are refused up front, so a path cannot pretend to be
    // relative on one platform and absolute on another.
    if trimmed.starts_with('/') || trimmed.contains(':') {
        return Err(SelfRepairViolation::UnsafePath(raw.to_string()));
    }
    let mut parts: Vec<String> = Vec::new();
    for component in Path::new(trimmed).components() {
        match component {
            Component::Normal(part) => parts.push(part.to_string_lossy().to_string()),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(SelfRepairViolation::UnsafePath(raw.to_string()));
            }
        }
    }
    if parts.is_empty() {
        return Err(SelfRepairViolation::UnsafePath(raw.to_string()));
    }
    Ok(parts.join("/"))
}

/// Convenience: the normalized form of a path, for logging a candidate.
pub fn normalize(path: &Path) -> Option<PathBuf> {
    normalize_relative(&path.to_string_lossy()).ok().map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_source_paths_are_allowed() {
        let surface = SelfRepairSurface::source_only();
        assert!(surface
            .check([
                "crates/kroma-core/src/agent_loop.rs",
                "crates/kroma-core/src/tools/bash.rs",
                "docs/NEXT_GEN_BLUEPRINT_v1.0.md",
            ])
            .is_ok());
    }

    /// The core promise: a self-repair patch cannot rewrite what judges it.
    #[test]
    fn protected_artifacts_are_refused_even_inside_an_allowed_prefix() {
        let surface = SelfRepairSurface::source_only();
        for path in [
            "crates/kroma-core/src/tool/policy.rs",
            "crates/kroma-core/src/sandbox.rs",
            "crates/kroma-core/src/self_repair_guard.rs",
            "crates/kroma-core/src/usage_cost.rs",
        ] {
            let err = surface
                .check([path])
                .expect_err(&format!("{path} must be refused"));
            match err {
                SelfRepairViolation::Protected { protected, .. } => {
                    assert!(!protected.is_empty(), "the refusal must name what is protected");
                }
                other => panic!("expected a protected-artifact refusal, got {other:?}"),
            }
        }
    }

    /// A protected file cannot be reached by moving it: the file name is matched wherever it is.
    #[test]
    fn a_protected_file_name_is_refused_in_any_directory() {
        let surface = SelfRepairSurface::source_only();
        assert!(surface
            .check(["crates/kroma-core/src/moved/smoke_solutions.json"])
            .is_err());
        // Case-insensitive, because macOS and Windows filesystems are.
        assert!(surface
            .check(["crates/kroma-core/src/TOOL/POLICY.RS"])
            .is_err());
    }

    /// Traversal and absolute paths are refused, so the guard cannot be walked around.
    #[test]
    fn unsafe_paths_are_refused() {
        let surface = SelfRepairSurface::source_only();
        for path in [
            "../../etc/passwd",
            "crates/../../etc/shadow",
            "/etc/shadow",
            "C:/windows/system32/cmd.exe",
            "",
        ] {
            assert!(
                surface.check([path]).is_err(),
                "{path:?} must be refused as unsafe"
            );
        }
    }

    /// Paths outside the declared surface are refused, and an empty surface allows nothing.
    #[test]
    fn changes_outside_the_declared_surface_are_refused() {
        let surface = SelfRepairSurface::source_only();
        assert!(surface.check(["crates/kroma-core/Cargo.toml"]).is_err());
        assert!(surface.check([".github/workflows/ci.yml"]).is_err());
        assert!(surface.check([".git/config"]).is_err());

        let closed = SelfRepairSurface::default();
        assert!(
            closed.check(["crates/kroma-core/src/agent_loop.rs"]).is_err(),
            "a surface with no allowed prefixes must allow nothing"
        );
    }

    /// One bad path refuses the whole candidate rather than applying the rest.
    #[test]
    fn one_violation_refuses_the_whole_candidate() {
        let surface = SelfRepairSurface::source_only();
        let candidate = [
            "crates/kroma-core/src/agent_loop.rs",
            "crates/kroma-core/src/tool/policy.rs",
        ];
        assert!(surface.check(candidate).is_err());
    }
}
