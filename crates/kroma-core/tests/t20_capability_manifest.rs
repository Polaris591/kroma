//! T20 (KR-34): the capability manifest is checked against the repository, not against itself.
//!
//! Each assertion here is a way documentation drifts from reality: a cited test that does not exist, a
//! documented flag the CLI never parsed, a version that disagrees with the package files, an old gate
//! report standing in for current evidence, or a capability claiming `live_tested` with nothing behind
//! it.

use kroma_core::capability::{CapabilityManifest, ManifestProblem};
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    // The test binary runs with the crate directory as cwd (`crates/kroma-core`).
    let cwd = std::env::current_dir().expect("cwd");
    if cwd.join("docs/capabilities.json").exists() {
        return cwd;
    }
    let parent = cwd.parent().and_then(|p| p.parent()).map(Path::to_path_buf);
    parent.unwrap_or(cwd)
}

fn manifest() -> CapabilityManifest {
    let root = repo_root();
    let text = std::fs::read_to_string(root.join("docs/capabilities.json"))
        .expect("docs/capabilities.json must exist");
    CapabilityManifest::from_json(&text).expect("the manifest must parse")
}

/// Every concatenated test source, so a cited name can be checked against the tree.
fn test_sources() -> String {
    let dir = repo_root().join("crates/kroma-core/tests");
    let mut text = String::new();
    let mut stack = vec![dir];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().map(|e| e == "rs").unwrap_or(false) {
                if let Ok(contents) = std::fs::read_to_string(&path) {
                    text.push_str(&contents);
                    text.push('\n');
                }
            }
        }
    }
    // Unit tests inside the library count too.
    let src = repo_root().join("crates/kroma-core/src");
    let mut stack = vec![src];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().map(|e| e == "rs").unwrap_or(false) {
                if let Ok(contents) = std::fs::read_to_string(&path) {
                    text.push_str(&contents);
                    text.push('\n');
                }
            }
        }
    }
    text
}

/// Every claim is internally consistent and points at a test that exists.
#[test]
fn capability_claim_has_current_evidence_reference() {
    let manifest = manifest();
    manifest
        .validate()
        .expect("the manifest must be self-consistent");
    manifest
        .validate_evidence(&test_sources())
        .expect("every cited test must exist in the tree");

    // The manifest must actually cover the work: a file that silently shrinks is a documentation
    // regression, not a smaller claim.
    assert!(
        manifest.capabilities.len() >= 18,
        "the manifest lists only {} capabilities",
        manifest.capabilities.len()
    );

    // Every capability is traceable to a task, and every task id looks like one.
    for capability in &manifest.capabilities {
        let task = capability
            .evidence
            .task
            .as_deref()
            .unwrap_or_else(|| panic!("capability '{}' cites no task", capability.id));
        assert!(
            task.starts_with('T') && task.len() >= 3,
            "capability '{}' cites a malformed task id '{}'",
            capability.id,
            task
        );
        assert!(
            !capability.summary.trim().is_empty(),
            "capability '{}' has no summary",
            capability.id
        );
    }

    // A capability that is not wired anywhere must not claim to be on by default.
    for capability in &manifest.capabilities {
        if capability.wired.is_empty() {
            assert!(
                !capability.default_on,
                "capability '{}' claims default_on without being wired",
                capability.id
            );
        }
    }
}

/// Every flag the documentation mentions is a flag the CLI actually parses.
#[test]
fn documentation_commands_match_cli_help() {
    let root = repo_root();
    let main_rs = std::fs::read_to_string(root.join("crates/kroma-cli/src/main.rs"))
        .expect("the CLI source must exist");
    let manifest = manifest();
    let mut checked = 0usize;
    for capability in &manifest.capabilities {
        for flag in &capability.cli_flags {
            assert!(
                flag.starts_with("--"),
                "capability '{}' documents '{}', which is not a flag",
                capability.id,
                flag
            );
            let quoted = format!("\"{}\"", flag);
            assert!(
                main_rs.contains(&quoted),
                "capability '{}' documents '{}' but the CLI parser never matches it",
                capability.id,
                flag
            );
            checked += 1;
        }
    }
    assert!(checked >= 5, "expected the manifest to document CLI flags");
}

/// A report from an older version is not evidence for this one.
#[test]
fn old_report_cannot_satisfy_new_release_gate() {
    use kroma_core::capability::CapabilityManifest as M;
    assert!(M::evidence_is_current("1.5.0", "1.5.0"));
    assert!(
        !M::evidence_is_current("1.4.0", "1.5.0"),
        "an old gate report must not satisfy a new release gate"
    );
    assert!(!M::evidence_is_current("", "1.5.0"));
    assert!(!M::evidence_is_current("1.5.0", ""));
    // Whitespace is not a version difference, but it is also not a match on the raw string.
    assert!(M::evidence_is_current(" 1.5.0 ", "1.5.0"));
}

/// The manifest's version agrees with the package files it claims to describe.
#[test]
fn package_versions_and_release_manifest_agree() {
    let root = repo_root();
    let manifest = manifest();

    let cargo = std::fs::read_to_string(root.join("Cargo.toml")).expect("workspace Cargo.toml");
    let cargo_version = cargo
        .lines()
        .find_map(|line| {
            let line = line.trim();
            line.strip_prefix("version = ")
                .map(|v| v.trim().trim_matches('"').to_string())
        })
        .expect("a workspace version");
    assert_eq!(
        manifest.package_version, cargo_version,
        "the capability manifest describes a different version than the workspace"
    );

    let package_json = std::fs::read_to_string(root.join("package.json")).expect("package.json");
    let value: serde_json::Value =
        serde_json::from_str(&package_json).expect("package.json parses");
    assert_eq!(
        value["version"].as_str(),
        Some(manifest.package_version.as_str()),
        "package.json and the capability manifest disagree on the version"
    );

    let npm_pkg = root.join("npm/kroma-cli/package.json");
    if npm_pkg.exists() {
        let text = std::fs::read_to_string(&npm_pkg).expect("npm package.json");
        let value: serde_json::Value = serde_json::from_str(&text).expect("npm package parses");
        assert_eq!(
            value["version"].as_str(),
            Some(manifest.package_version.as_str()),
            "the npm package and the capability manifest disagree on the version"
        );
    }
}

/// The schema refuses the contradictions a hand-written manifest invites.
#[test]
fn manifest_rejects_contradictory_claims() {
    // Each case supplies the whole capability body, so no field is repeated.
    let parse = |body: &str| {
        let json = format!(
            r#"{{"schema_version": 1, "package_version": "1.5.0", "capabilities": [{{ {body} }}]}}"#
        );
        CapabilityManifest::from_json(&json).map(|m| m.validate())
    };

    // Not implemented but on by default.
    assert!(matches!(
        parse(
            r#""id": "x", "summary": "s", "implemented": false, "wired": ["cli"], "contract_tested": false, "live_tested": false, "default_on": true"#
        ),
        Ok(Err(ManifestProblem::DefaultOnWithoutImplementation(_)))
    ));
    // On by default but wired nowhere.
    assert!(matches!(
        parse(
            r#""id": "x", "summary": "s", "implemented": true, "contract_tested": false, "live_tested": false, "default_on": true"#
        ),
        Ok(Err(ManifestProblem::DefaultOnWithoutWiring(_)))
    ));
    // Contract-tested with no test named.
    assert!(matches!(
        parse(
            r#""id": "x", "summary": "s", "implemented": true, "wired": ["cli"], "contract_tested": true, "live_tested": false, "default_on": false"#
        ),
        Ok(Err(ManifestProblem::ContractTestedWithoutTests(_)))
    ));
    // Live-tested with no live evidence.
    assert!(matches!(
        parse(
            r#""id": "x", "summary": "s", "implemented": true, "wired": ["cli"], "contract_tested": false, "live_tested": true, "default_on": false"#
        ),
        Ok(Err(ManifestProblem::LiveTestedWithoutEvidence(_)))
    ));
    // Unknown surface.
    assert!(matches!(
        parse(
            r#""id": "x", "summary": "s", "implemented": true, "wired": ["browser"], "contract_tested": false, "live_tested": false, "default_on": false"#
        ),
        Ok(Err(ManifestProblem::UnknownSurface { .. }))
    ));
    // Unknown field.
    assert!(parse(r#""id": "x", "summary": "s", "implemented": true, "wired": ["cli"], "contract_tested": false, "live_tested": false, "default_on": false, "marketing": "best""#).is_err());
    // A valid one passes.
    assert!(matches!(
        parse(
            r#""id": "x", "summary": "s", "implemented": true, "wired": ["cli"], "contract_tested": false, "live_tested": false, "default_on": false"#
        ),
        Ok(Ok(()))
    ));
}

/// A cited test that is not in the tree is a documentation defect.
#[test]
fn missing_cited_test_is_reported() {
    // Built at runtime so the name cannot be found by the scan that looks for it in this very file.
    let absent = format!("absent_{}_test", uuid::Uuid::new_v4().simple());
    let json = format!(
        r#"{{
        "schema_version": 1,
        "package_version": "1.5.0",
        "capabilities": [{{
            "id": "x", "summary": "s", "implemented": true, "wired": ["cli"],
            "contract_tested": true, "live_tested": false, "default_on": false,
            "evidence": {{"tests": ["{}"]}}
        }}]
    }}"#,
        absent
    );
    let manifest = CapabilityManifest::from_json(&json).expect("parses");
    manifest.validate().expect("internally consistent");
    match manifest.validate_evidence(&test_sources()) {
        Err(ManifestProblem::MissingEvidence { id, test }) => {
            assert_eq!(id, "x");
            assert_eq!(test, absent);
        }
        other => panic!("expected MissingEvidence, got {other:?}"),
    }
}

/// The human-readable table and the machine-readable manifest describe the same set.
#[test]
fn capability_doc_and_manifest_do_not_drift() {
    let root = repo_root();
    let doc = std::fs::read_to_string(root.join("docs/CAPABILITIES.md"))
        .expect("docs/CAPABILITIES.md must exist");
    let manifest = manifest();

    // One table row per capability, counted by the task column (`| T..`).
    let rows = doc
        .lines()
        .filter(|line| line.starts_with("| ") && line.contains("| T"))
        .count();
    assert_eq!(
        rows,
        manifest.capabilities.len(),
        "docs/CAPABILITIES.md has {rows} rows but the manifest lists {} capabilities",
        manifest.capabilities.len()
    );

    // Every task cited by the manifest appears in the doc, and vice versa.
    for capability in &manifest.capabilities {
        let task = capability.evidence.task.as_deref().unwrap();
        assert!(
            doc.contains(&format!("| {}", task)),
            "task {} ({}) is missing from docs/CAPABILITIES.md",
            task,
            capability.id
        );
    }

    // The doc must state the two limits that are easiest to overstate.
    assert!(
        doc.contains("NOT_RUN"),
        "the doc must say which live checks were not run"
    );
    assert!(
        doc.contains("不是 OS 安全边界") || doc.contains("not an OS security boundary"),
        "the doc must not imply a worktree is a security boundary"
    );
    assert!(
        doc.contains("许可证"),
        "the licence decision must be left to the owner, not silently assumed"
    );
}
