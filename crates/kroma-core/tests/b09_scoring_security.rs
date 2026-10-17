//! B09/F07 contract: the bench post-condition verdict is a *capability-bound* read, not a path
//! string check.
//!
//! `check_post_conditions` used to join a validated relative path onto the workspace and call
//! `std::fs::read_to_string`, which follows symlinks (leaf *and* ancestor), blocks on a FIFO,
//! allocates the whole file, and treats any `metadata()` error as "absent" through `Path::exists()`.
//! A task with no oracle at all also resolved vacuously. This file pins the F07 fix:
//!
//! * a leaf or ancestor symlink that points outside the workspace never satisfies `expect_contains`;
//! * a dangling symlink (and any other non-`NotFound` error) is *present*, never "absent";
//! * a FIFO / special file is rejected without blocking the evaluator;
//! * a file past the bounded read cap is rejected instead of buffered whole;
//! * an empty oracle and an empty needle cannot fake a `resolved` verdict;
//! * `load_suite` and the direct `evaluate`/`is_resolved` API reject the same dishonest tasks.
//!
//! Every fixture is a small, network-free temp file; symlinks point only at a local sentinel, never
//! at a real secret.
#![cfg(unix)]

use kroma_core::bench::{load_suite, BenchTask};
use std::collections::BTreeMap;
use std::ffi::CString;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::time::Duration;
use uuid::Uuid;

fn temp_root(label: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("kroma-b09-{}-{}", label, Uuid::new_v4()));
    fs::create_dir_all(&root).unwrap();
    root
}

fn contains_task(id: &str, contains: &[(&str, &str)]) -> BenchTask {
    BenchTask {
        id: id.to_string(),
        prompt: "p".to_string(),
        files: BTreeMap::new(),
        expect_contains: contains
            .iter()
            .map(|(path, needle)| (path.to_string(), needle.to_string()))
            .collect(),
        expect_absent: Vec::new(),
        regression_guard: false,
        verify_command: None,
    }
}

fn absent_task(id: &str, absent: &[&str]) -> BenchTask {
    BenchTask {
        id: id.to_string(),
        prompt: "p".to_string(),
        files: BTreeMap::new(),
        expect_contains: Vec::new(),
        expect_absent: absent.iter().map(|path| path.to_string()).collect(),
        regression_guard: false,
        verify_command: None,
    }
}

/// A symlinked leaf that points at a file outside the workspace must not be followed.
#[tokio::test]
async fn leaf_symlink_to_outside_is_not_a_result() {
    let root = temp_root("leaf-symlink");
    let sentinel = root.join("outside-sentinel.txt");
    fs::write(&sentinel, "KROMA_B09_SENTINEL").unwrap();
    let ws = root.join("ws");
    fs::create_dir_all(&ws).unwrap();
    std::os::unix::fs::symlink(&sentinel, ws.join("claimed-result")).unwrap();

    let task = contains_task("leaf-symlink", &[("claimed-result", "KROMA_B09_SENTINEL")]);
    let verdict = task.evaluate(&ws).await;
    assert!(
        !verdict.resolved,
        "a symlinked leaf pointing outside the workspace must not satisfy expect_contains: {:?}",
        verdict
    );
    assert!(!task.is_resolved(&ws));
    let _ = fs::remove_dir_all(&root);
}

/// A symlinked *ancestor* directory must not be traversed to reach an outside file.
#[tokio::test]
async fn ancestor_directory_symlink_is_not_traversed() {
    let root = temp_root("ancestor-symlink");
    let outside = root.join("outside");
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("sentinel.txt"), "KROMA_B09_SENTINEL").unwrap();
    let ws = root.join("ws");
    fs::create_dir_all(&ws).unwrap();
    std::os::unix::fs::symlink(&outside, ws.join("escape")).unwrap();

    let task = contains_task(
        "ancestor-symlink",
        &[("escape/sentinel.txt", "KROMA_B09_SENTINEL")],
    );
    let verdict = task.evaluate(&ws).await;
    assert!(
        !verdict.resolved,
        "a symlinked ancestor directory must not be traversed: {:?}",
        verdict
    );
    let _ = fs::remove_dir_all(&root);
}

/// A dangling symlink still exists on disk; `expect_absent` must fail, not pass vacuously.
#[tokio::test]
async fn dangling_symlink_counts_as_present_for_expect_absent() {
    let root = temp_root("dangling");
    let ws = root.join("ws");
    fs::create_dir_all(&ws).unwrap();
    std::os::unix::fs::symlink(ws.join("never-created"), ws.join("ghost")).unwrap();

    let task = absent_task("dangling", &["ghost"]);
    let verdict = task.evaluate(&ws).await;
    assert!(
        !verdict.resolved,
        "a dangling symlink still exists and must fail expect_absent: {:?}",
        verdict
    );
    let _ = fs::remove_dir_all(&root);
}

/// A self-referential symlink makes `metadata()` fail with `ELOOP`; that is not "absent".
#[tokio::test]
async fn symlink_loop_is_not_reported_as_absent() {
    let root = temp_root("loop");
    let ws = root.join("ws");
    fs::create_dir_all(&ws).unwrap();
    std::os::unix::fs::symlink("loop", ws.join("loop")).unwrap();

    let task = absent_task("loop", &["loop"]);
    assert!(
        !task.evaluate(&ws).await.resolved,
        "a symlink loop must not be reported as absent"
    );
    let _ = fs::remove_dir_all(&root);
}

/// Any non-`NotFound` error (ENOTDIR here; a real EACCES takes the same arm) must not be "absent".
#[tokio::test]
async fn non_notfound_error_is_not_absence() {
    let root = temp_root("enotdir");
    let ws = root.join("ws");
    fs::create_dir_all(&ws).unwrap();
    fs::write(ws.join("regular.txt"), "x").unwrap();

    let task = absent_task("enotdir", &["regular.txt/child"]);
    assert!(
        !task.evaluate(&ws).await.resolved,
        "an unresolvable path is not proof of absence"
    );
    let _ = fs::remove_dir_all(&root);
}

/// A FIFO post-condition must be rejected without ever blocking the evaluator.
///
/// Safety net only: opening a FIFO read-only blocks until a writer appears, so the pre-fix path
/// would hang rather than return a wrong verdict. This is a regression guard, not the F07 red
/// evidence.
#[test]
fn fifo_post_condition_is_rejected_without_blocking() {
    let root = temp_root("fifo");
    let ws = root.join("ws");
    fs::create_dir_all(&ws).unwrap();
    let fifo = ws.join("pipe");
    let c_path = CString::new(fifo.as_os_str().as_bytes()).unwrap();
    assert_eq!(
        unsafe { libc::mkfifo(c_path.as_ptr(), 0o644) },
        0,
        "mkfifo failed"
    );

    let task = contains_task("fifo", &[("pipe", "x")]);
    let (tx, rx) = std::sync::mpsc::channel();
    let ws_for_thread = ws.clone();
    let worker = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let resolved = runtime.block_on(task.evaluate(&ws_for_thread)).resolved;
        let _ = tx.send(resolved);
    });
    let resolved = rx
        .recv_timeout(Duration::from_secs(20))
        .expect("a FIFO post-condition must not block the evaluator");
    assert!(!resolved, "a FIFO must never satisfy expect_contains");
    worker.join().unwrap();
    let _ = fs::remove_dir_all(&root);
}

/// The post-condition read is bounded: an oversized artefact is rejected, a small one still passes.
#[tokio::test]
async fn oversized_post_condition_file_is_rejected() {
    let root = temp_root("oversized");
    let ws = root.join("ws");
    fs::create_dir_all(&ws).unwrap();
    // 2 MiB of filler with the needle past the 1 MiB read cap: the pre-fix read buffered the whole
    // file and found it; the fix caps the read and rejects the artefact.
    let mut body = "a".repeat(2 * 1024 * 1024);
    body.push_str("KROMA_B09_NEEDLE");
    fs::write(ws.join("big.txt"), body).unwrap();

    let task = contains_task("oversized", &[("big.txt", "KROMA_B09_NEEDLE")]);
    assert!(
        !task.evaluate(&ws).await.resolved,
        "a file past the read cap must be rejected, not buffered whole"
    );

    // A file inside the cap still resolves, so the cap is a bound, not a blanket rejection.
    fs::write(ws.join("small.txt"), "prefix KROMA_B09_NEEDLE suffix").unwrap();
    let small = contains_task("small", &[("small.txt", "KROMA_B09_NEEDLE")]);
    assert!(
        small.evaluate(&ws).await.resolved,
        "a file inside the read cap must still resolve"
    );
    let _ = fs::remove_dir_all(&root);
}

/// A task with no post-condition and no held-out command has no oracle and must not resolve.
#[tokio::test]
async fn empty_oracle_cannot_resolve() {
    let root = temp_root("empty-oracle");
    let ws = root.join("ws");
    fs::create_dir_all(&ws).unwrap();
    let task = BenchTask {
        id: "vacuous".to_string(),
        prompt: "p".to_string(),
        files: BTreeMap::new(),
        expect_contains: Vec::new(),
        expect_absent: Vec::new(),
        regression_guard: false,
        verify_command: None,
    };
    let verdict = task.evaluate(&ws).await;
    assert!(
        !verdict.resolved,
        "a task with no oracle must not resolve: {:?}",
        verdict
    );
    assert!(!task.is_resolved(&ws));
    let _ = fs::remove_dir_all(&root);
}

/// An empty needle is true for every file; it must be rejected instead of faking a pass.
#[tokio::test]
async fn empty_needle_is_not_trivially_true() {
    let root = temp_root("empty-needle");
    let ws = root.join("ws");
    fs::create_dir_all(&ws).unwrap();
    fs::write(ws.join("answer.txt"), "anything at all").unwrap();

    let task = contains_task("empty-needle", &[("answer.txt", "")]);
    assert!(
        !task.evaluate(&ws).await.resolved,
        "an empty needle must not be trivially satisfied"
    );
    let _ = fs::remove_dir_all(&root);
}

/// `load_suite` and the direct API reject the same vacuous tasks; a legal suite still loads.
#[test]
fn suite_loader_and_direct_api_reject_the_same_vacuous_tasks() {
    let root = temp_root("suite-consistency");
    let cases: [(&str, &str); 2] = [
        ("no-oracle", r#"[{"id":"vacuous","prompt":"p"}]"#),
        (
            "empty-needle",
            r#"[{"id":"empty-needle","prompt":"p","expect_contains":[["a.txt",""]]}]"#,
        ),
    ];
    for (label, body) in cases {
        let path = root.join(format!("{}.json", label));
        fs::write(&path, body).unwrap();
        let err = load_suite(&path).expect_err("a vacuous task must be rejected");
        assert!(err.contains("task '"), "error must name the task: {}", err);
    }

    let legal = root.join("legal.json");
    fs::write(
        &legal,
        r#"[{"id":"ok","prompt":"p","expect_contains":[["a.txt","needle"]]}]"#,
    )
    .unwrap();
    let tasks = load_suite(&legal).expect("a legal suite must load");
    assert_eq!(tasks.len(), 1);
    let _ = fs::remove_dir_all(&root);
}
