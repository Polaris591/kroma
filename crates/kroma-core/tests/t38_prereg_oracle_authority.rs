//! R02: the trusted-oracle capability is frozen at admission, not re-read from a mutable document.
//!
//! Two authority-escalation paths are closed here:
//!
//! 1. **Preregistration rewrite.** `check_prereg` admits a run and freezes the declared verifier file
//!    (path + digest) on the returned gate. Rewriting the preregistration afterwards — even to point
//!    at another host secret — must not widen an already-admitted gate: the provider is built from
//!    the gate, never from a second read of the file.
//! 2. **Oracle byte change.** The digest authorized at admission is checked again immediately before
//!    the read-only mount. A file that changed in between is refused as *no verdict*, never silently
//!    substituted into the scored run.
//!
//! Both are deterministic: no timing, no load, no network.

#![cfg(unix)]

use std::path::{Path, PathBuf};

use kroma_core::bench::{BenchTask, VerifyCommandLimits};
use kroma_core::prereg::{check_prereg, PreregGate, VerifierFileIdentity};
use kroma_core::sandbox::{SandboxSpec, SandboxTemplate};

fn unique_root(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("kroma-t38-{}-{}", tag, uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    root
}

fn require_bwrap() {
    if SandboxSpec::bwrap("/tmp").probe().is_err() {
        panic!("bwrap is required to prove oracle delivery and is not usable on this host");
    }
}

fn isolated_spec(workspace: &Path) -> SandboxSpec {
    SandboxTemplate::bwrap().resolve(workspace.to_path_buf())
}

fn task_with_verifier(verify: Vec<String>) -> BenchTask {
    BenchTask {
        id: "oracle-authority".to_string(),
        prompt: "answer".to_string(),
        files: [("answer.txt".to_string(), "correct".to_string())].into(),
        expect_contains: vec![("answer.txt".to_string(), "correct".to_string())],
        expect_absent: Vec::new(),
        regression_guard: false,
        verify_command: Some(verify),
    }
}

/// Write a complete preregistration whose held-out verifier is `verifier`.
fn write_prereg(root: &Path, name: &str, verifier: &Path) -> PathBuf {
    let dev = root.join(format!("dev-{name}.json"));
    let holdout = root.join(format!("holdout-{name}.json"));
    // The two suites must be distinct documents (the gate refuses one suite twice).
    std::fs::write(&dev, r#"[{"id":"dev"}]"#).unwrap();
    std::fs::write(&holdout, r#"[{"id":"holdout"}]"#).unwrap();
    let prereg = root.join(format!("prereg-{name}.json"));
    let doc = serde_json::json!({
        "task_cluster_definition": { "dev_suite": dev, "holdout_suite": holdout },
        "model_identity_observability": "local fixture",
        "quality_margin": "fixture",
        "noise_and_power_plan": "fixture",
        "cache_order_design": "fixture",
        "budget": "fixture",
        "stopping_rule": "fixture",
        "holdout_policy": { "verifier_path": verifier }
    });
    std::fs::write(&prereg, serde_json::to_string_pretty(&doc).unwrap()).unwrap();
    prereg
}

/// The admitted gate keeps the oracle identity it froze; a later rewrite of the preregistration
/// cannot add authority to it, and the new file is not accepted by the old gate.
#[tokio::test]
async fn rewriting_the_prereg_after_admission_does_not_widen_the_gate() {
    require_bwrap();
    let root = unique_root("prereg-rewrite");
    let workspace = root.join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(workspace.join("answer.txt"), "correct").unwrap();

    let oracle = root.join("oracle-a.py");
    std::fs::write(&oracle, "import sys\nsys.exit(0)\n").unwrap();
    let secret = root.join("host-secret-b.py");
    std::fs::write(&secret, "import pathlib,sys\nsys.exit(0)\n").unwrap();

    let prereg = write_prereg(&root, "a", &oracle);
    let gate: PreregGate =
        check_prereg(&prereg, &workspace, Some(&root.join("holdout-a.json"))).unwrap();
    let frozen = gate
        .verifier_file
        .clone()
        .expect("the gate must carry the declared oracle identity");
    assert_eq!(frozen.path, oracle.canonicalize().unwrap());
    let gate_sha = gate.sha256.clone();

    // Rewrite the *same* preregistration to point at another host file, after admission.
    let rewritten = write_prereg(&root, "b", &secret);
    std::fs::rename(&rewritten, &prereg).unwrap();
    // The old gate is unchanged: same document digest, same authorized oracle.
    assert_eq!(gate.sha256, gate_sha);
    assert_eq!(gate.verifier_file.as_ref().unwrap(), &frozen);
    assert_ne!(frozen.path, secret.canonicalize().unwrap());

    let limits = VerifyCommandLimits::default();
    let spec = isolated_spec(&workspace);

    // The newly named file is NOT authorized by the old gate.
    let unauthorized =
        task_with_verifier(vec!["python3".to_string(), secret.display().to_string()]);
    let evaluation = unauthorized
        .evaluate_with_sandbox_and_oracle(&workspace, &limits, Some(&spec), None, &[frozen.clone()])
        .await;
    let unknown = evaluation
        .verdict_unknown
        .as_deref()
        .expect("a file named only by the rewritten prereg must be unscorable");
    assert!(
        unknown.contains("not a declared trusted verifier file"),
        "the refusal must name the missing authorization: {unknown}"
    );

    // The oracle that was actually admitted still works.
    let authorized = task_with_verifier(vec!["python3".to_string(), oracle.display().to_string()]);
    let evaluation = authorized
        .evaluate_with_sandbox_and_oracle(&workspace, &limits, Some(&spec), None, &[frozen])
        .await;
    assert!(
        evaluation.resolved,
        "the admitted oracle must still score the run: {:?}",
        evaluation.failures
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// Changing the authorized file after admission is detected before the mount: no verdict, no silent
/// substitution.
#[tokio::test]
async fn changing_the_oracle_bytes_after_admission_is_refused() {
    require_bwrap();
    let root = unique_root("oracle-bytes");
    let workspace = root.join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(workspace.join("answer.txt"), "correct").unwrap();
    let oracle = root.join("verify.py");
    std::fs::write(&oracle, "import sys\nsys.exit(0)\n").unwrap();
    let frozen = VerifierFileIdentity::freeze(&oracle).unwrap();

    let task = task_with_verifier(vec!["python3".to_string(), oracle.display().to_string()]);
    let limits = VerifyCommandLimits::default();
    let spec = isolated_spec(&workspace);

    // Control: the frozen identity works while the bytes are unchanged.
    let before = task
        .evaluate_with_sandbox_and_oracle(&workspace, &limits, Some(&spec), None, &[frozen.clone()])
        .await;
    assert!(
        before.resolved,
        "control: the unchanged oracle must score the run: {:?}",
        before.failures
    );

    // Swap the bytes (an equally valid oracle that would silently pass): the capability is stale.
    std::fs::write(&oracle, "import sys\nsys.exit(0)  # substituted\n").unwrap();
    let after = task
        .evaluate_with_sandbox_and_oracle(&workspace, &limits, Some(&spec), None, &[frozen])
        .await;
    let unknown = after
        .verdict_unknown
        .as_deref()
        .expect("a changed oracle must be unscorable");
    assert!(
        unknown.contains("changed after admission"),
        "the refusal must name the byte change: {unknown}"
    );
    assert!(
        !after
            .checks
            .iter()
            .any(|check| check.contains("verify_command")),
        "the substituted verifier must not have run: {:?}",
        after.checks
    );
    let _ = std::fs::remove_dir_all(&root);
}
