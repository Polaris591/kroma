//! Executable, deterministic **process-crash** acceptance for the workspace-revision seam.
//!
//! The `v1.5.x` fault-injection tests (`gate1_workspace_revision.rs`,
//! `session_state_transaction.rs`) reach every crash boundary *in one process* by having the
//! production entry point return a `RecoveryRequired` value. They never actually kill a process, so
//! they cannot prove that a *fresh* process reconciles disk + snapshot store + session WAL after an
//! abrupt death. The prior CLI acceptance only `SIGKILL`ed a run while it waited for the provider.
//!
//! This file closes that gap: each scenario re-executes the test binary as a real child process,
//! lets the child reach a durable boundary, and `SIGKILL`s it there (no unwinding, no flush, no
//! cleanup). The parent then drives the **real `kroma` binary** (`CARGO_BIN_EXE_kroma`) — the
//! production `--resume` / `--undo` / `--restore-rev` / `--list-revisions` recovery path — against
//! the leftover state and asserts the documented recoverable outcome:
//!
//! * `mutation-before-capture` — a file mutated on disk with no capture: `--undo` must **refuse**
//!   (exit 3, the restore-conflict code) and leave the external bytes untouched, never silently
//!   overwrite them.
//! * `snapshot-before-wal` — the manifest is durably persisted but the WAL append died: the next
//!   process must adopt the orphaned manifest (`SnapshotStore::reconcile_with_wal`), so `--undo`
//!   then restores the pre-mutation bytes.
//! * `torn-wal-append` — a crash mid-append leaves a torn trailing line: reopening must discard
//!   the fragment (hash chain intact) and recover the last complete revision.
//! * `fork-before-leaf-move` — the durable fork intent exists, the workspace was restored, the leaf
//!   never moved: the next process must complete the fork from the intent.
//! * `restore-midway` — a restore applied some files then died: the next process must finish the
//!   recorded restore before doing anything else.
//! * `corrupt-manifest` — an unreadable manifest must **block** the mutation with exit 4 (the
//!   recovery-required code) instead of acting on a WAL it can no longer prove.
//!
//! The kill hook is test-only: it lives in this integration test and is compiled only into the test
//! binary, never into the shipped `kroma` binary, so production has no environment-triggerable kill
//! path. The child role is selected by `KROMA_CRASH_SCENARIO`; without it `crash_child_entry` is a
//! no-op so a normal `cargo test` run is unaffected.
#![cfg(unix)]

use kroma_core::events::resolve_default_state_dir;
use kroma_core::messages::{TranscriptItem, UserMessage};
use kroma_core::revision::{ForkFault, RestoreFault, RestoreResult, RevisionId, SnapshotStore};
use kroma_core::session::SessionJournal;
use kroma_core::workspace::WorkspaceCap;
use std::io::Write;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// Test name the parent re-executes to run the crash child role.
const CHILD_TEST: &str = "crash_child_entry";
/// Exit code the child uses when a scenario failed to reach its crash point (never expected).
const CHILD_NO_CRASH_EXIT: i32 = 97;
/// `EXIT_RESTORE_CONFLICT` in `kroma-cli/src/main.rs`.
const EXIT_RESTORE_CONFLICT: i32 = 3;
/// `EXIT_RESTORE_RECOVERY_REQUIRED` in `kroma-cli/src/main.rs`.
const EXIT_RESTORE_RECOVERY_REQUIRED: i32 = 4;

fn temp_root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "kroma-crash-matrix-{}-{}",
        tag,
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn user(content: &str) -> TranscriptItem {
    TranscriptItem::User(UserMessage {
        content: content.to_string(),
    })
}

// ---------------------------------------------------------------------------
// Child role: build the durable state for one scenario, then SIGKILL ourselves.
// ---------------------------------------------------------------------------

/// Re-executed by every parent scenario with `KROMA_CRASH_SCENARIO` set. Without the variable this
/// is a no-op so the ordinary test run does not crash.
#[test]
fn crash_child_entry() {
    let Ok(scenario) = std::env::var("KROMA_CRASH_SCENARIO") else {
        return;
    };
    let root = PathBuf::from(
        std::env::var("KROMA_CRASH_ROOT").expect("parent sets KROMA_CRASH_ROOT for the child"),
    );
    child_build_state(&root, &scenario);

    // A scenario that failed to reach its crash point must be visible to the parent, not silently
    // turned into a normal test pass.
    eprintln!(
        "crash child reached the end of scenario '{}' without crashing",
        scenario
    );
    std::process::exit(CHILD_NO_CRASH_EXIT);
}

/// Create the workspace, WAL and snapshot store a scenario needs, stopping at the durable boundary.
fn child_build_state(root: &Path, scenario: &str) {
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join("a.txt"), "v1\n").unwrap();
    std::fs::write(ws.join("b.txt"), "v1\n").unwrap();
    let workspace = WorkspaceCap::new(&ws).unwrap();
    let state_dir = resolve_default_state_dir();
    let store = SnapshotStore::new(state_dir.join("snapshots"));
    let mut journal = SessionJournal::create(state_dir.join("sessions"), &ws).unwrap();

    // Baseline turn 0: transcript entry + captured revision, both recorded in the WAL.
    let leaf0 = journal.append_item(0, user("task")).unwrap();
    let v1 = store
        .capture_revision(&workspace, None, Some(&leaf0), 0)
        .unwrap();
    journal
        .record_revision_capture(
            0,
            v1.revision_id.as_str(),
            None,
            v1.changed_paths.clone(),
            v1.tracked_files.len(),
        )
        .unwrap();

    match scenario {
        // A file changed on disk, then the process died before the agent loop captured a revision.
        // The WAL still names the *previous* revision, so the divergence must be refused, not
        // absorbed.
        "mutation-before-capture" => {
            mutate_and_record_v2(&store, &mut journal, &workspace, &ws, &v1);
            // The uncaptured mutation: external bytes that no revision describes.
            std::fs::write(ws.join("a.txt"), "v3 external\n").unwrap();
        }
        // The snapshot manifest was durably written, then the process died before the WAL
        // `RevisionCaptured` append. The manifest is bound to the current leaf and unrecorded.
        "snapshot-before-wal" => {
            std::fs::write(ws.join("a.txt"), "v2\n").unwrap();
            let leaf1 = journal.append_item(1, user("turn two")).unwrap();
            let orphan = store
                .capture_revision(&workspace, Some(&v1.revision_id), Some(&leaf1), 1)
                .unwrap();
            assert_ne!(orphan.revision_id, v1.revision_id);
            assert!(
                !journal
                    .revision_records()
                    .iter()
                    .any(|r| r.revision_id == orphan.revision_id.as_str()),
                "the orphan manifest must not be in the WAL yet"
            );
        }
        // A crash in the middle of a WAL append leaves a torn trailing line.
        "torn-wal-append" => {
            mutate_and_record_v2(&store, &mut journal, &workspace, &ws, &v1);
            let wal = journal.path().to_path_buf();
            drop(journal);
            let mut file = std::fs::OpenOptions::new().append(true).open(&wal).unwrap();
            file.write_all(b"{\"seq\":99,\"prev_hash\":\"deadbeef\",\"hash\":\"torn-frag")
                .unwrap();
            file.flush().unwrap();
        }
        // A workspace-bound fork wrote its durable intent and restored the workspace, then died
        // before moving the session leaf.
        "fork-before-leaf-move" => {
            mutate_and_record_v2(&store, &mut journal, &workspace, &ws, &v1);
            let outcome = store.fork_workspace_at_turn_with_fault(
                &workspace,
                &mut journal,
                0,
                false,
                Some(ForkFault::AfterRestoreBeforeLeafMove),
            );
            assert!(
                outcome.is_err(),
                "the fault must stop the fork before the leaf moves"
            );
            assert!(
                store.pending_fork_intent().unwrap().is_some(),
                "the durable fork intent must survive the crash point"
            );
            assert_eq!(std::fs::read_to_string(ws.join("a.txt")).unwrap(), "v1\n");
        }
        // A restore applied some files then died, leaving its intent journal behind.
        "restore-midway" => {
            std::fs::write(ws.join("a.txt"), "v2\n").unwrap();
            std::fs::write(ws.join("b.txt"), "v2\n").unwrap();
            let v2 = record_v2(&store, &mut journal, &workspace, &v1);
            let outcome = store
                .restore_revision_with_fault(
                    &workspace,
                    &v1.revision_id,
                    Some(&v2),
                    true,
                    Some(RestoreFault::AfterAppliedFiles(1)),
                )
                .unwrap();
            assert!(
                matches!(outcome, RestoreResult::RecoveryRequired { .. }),
                "the fault must stop the restore midway, got {:?}",
                outcome
            );
            assert!(store.pending_restore().unwrap().is_some());
        }
        // The CLI `--undo` sequence: write the durable rewind intent, restore the workspace to the
        // previous revision, then rewind the session leaf. A crash between the restore and the leaf
        // move must be completed by the next process, not left divergent.
        "undo-before-leaf-move" => {
            let _ = mutate_and_record_v2(&store, &mut journal, &workspace, &ws, &v1);
            let outcome = store.restore_and_rewind_workspace_with_fault(
                &workspace,
                &mut journal,
                &v1.revision_id,
                0,
                false,
                Some(ForkFault::AfterRestoreBeforeLeafMove),
            );
            assert!(
                outcome.is_err(),
                "the fault must stop the rewind before the leaf moves"
            );
            assert!(
                store.pending_fork_intent().unwrap().is_some(),
                "the durable rewind intent must survive the crash point"
            );
            assert_eq!(std::fs::read_to_string(ws.join("a.txt")).unwrap(), "v1\n");
        }
        // A manifest in the store is unreadable: the next process can no longer prove the WAL
        // describes the workspace, so it must refuse to act.
        "corrupt-manifest" => {
            let v2 = mutate_and_record_v2(&store, &mut journal, &workspace, &ws, &v1);
            std::fs::write(store.revision_path(&v2), b"{ not valid json").unwrap();
        }
        other => panic!("unknown crash scenario '{}'", other),
    }

    kill_self();
}

/// Mutate `a.txt` to `v2`, capture and WAL-record the revision exactly as the agent loop does, and
/// return its id.
fn mutate_and_record_v2(
    store: &SnapshotStore,
    journal: &mut SessionJournal,
    workspace: &WorkspaceCap,
    ws: &Path,
    v1: &kroma_core::revision::WorkspaceRevision,
) -> RevisionId {
    std::fs::write(ws.join("a.txt"), "v2\n").unwrap();
    record_v2(store, journal, workspace, v1)
}

/// Capture and WAL-record the current workspace as the turn-1 revision on top of `v1`.
fn record_v2(
    store: &SnapshotStore,
    journal: &mut SessionJournal,
    workspace: &WorkspaceCap,
    v1: &kroma_core::revision::WorkspaceRevision,
) -> RevisionId {
    let leaf1 = journal.append_item(1, user("turn two")).unwrap();
    let v2 = store
        .capture_revision(workspace, Some(&v1.revision_id), Some(&leaf1), 1)
        .unwrap();
    journal
        .record_revision_capture(
            1,
            v2.revision_id.as_str(),
            Some(v1.revision_id.as_str()),
            v2.changed_paths.clone(),
            v2.tracked_files.len(),
        )
        .unwrap();
    v2.revision_id
}

/// Hard, uncatchable death with no unwinding, no buffered write flush and no destructor cleanup —
/// the same end state a `SIGKILL`ed production process leaves.
fn kill_self() -> ! {
    unsafe {
        libc::kill(libc::getpid(), libc::SIGKILL);
    }
    // Unreachable on Unix; abort if the signal somehow did not land.
    std::process::abort()
}

// ---------------------------------------------------------------------------
// Parent role: run a child to its crash point, then recover with the real CLI.
// ---------------------------------------------------------------------------

/// Spawn the test binary as the crash child for `scenario`, assert it died by `SIGKILL`, and return
/// the scenario root.
fn crash_at(scenario: &str) -> PathBuf {
    let root = temp_root(scenario);
    std::fs::create_dir_all(root.join("xdg")).unwrap();
    let exe = std::env::current_exe().unwrap();
    let status = Command::new(exe)
        .arg(CHILD_TEST)
        .arg("--exact")
        .arg("--nocapture")
        .arg("--test-threads=1")
        .env("KROMA_CRASH_SCENARIO", scenario)
        .env("KROMA_CRASH_ROOT", &root)
        .env("XDG_STATE_HOME", root.join("xdg"))
        .env("HOME", root.join("home"))
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .status()
        .expect("re-exec the test binary as the crash child");
    assert_eq!(
        status.signal(),
        Some(libc::SIGKILL),
        "child for '{}' must die by SIGKILL, got {:?}",
        scenario,
        status
    );
    assert_eq!(
        status.code(),
        None,
        "child for '{}' exited normally instead of crashing",
        scenario
    );
    root
}

/// The single session WAL the child created under the scenario's state directory.
fn find_wal(root: &Path) -> PathBuf {
    let sessions = root.join("xdg").join("kroma").join("runs").join("sessions");
    let mut wals: Vec<PathBuf> = std::fs::read_dir(&sessions)
        .expect("child must have created a sessions dir")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.to_string_lossy().ends_with(".wal.jsonl"))
        .collect();
    assert_eq!(wals.len(), 1, "expected exactly one session WAL");
    wals.pop().unwrap()
}

/// The baseline revision id recorded by the child, read from the WAL.
fn baseline_revision(root: &Path) -> String {
    let journal = SessionJournal::open(find_wal(root)).unwrap();
    journal
        .revision_records()
        .first()
        .map(|r| r.revision_id.clone())
        .expect("the child always records a baseline revision")
}

/// Drive the real `kroma` recovery CLI against the crashed state.
fn run_cli(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_kroma"))
        .arg("run")
        .arg("--workspace")
        .arg(root.join("ws"))
        .arg("--quiet")
        .args(args)
        .env("XDG_STATE_HOME", root.join("xdg"))
        .env("HOME", root.join("home"))
        .output()
        .expect("run the kroma recovery CLI")
}

fn stdout_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn read_ws(root: &Path, name: &str) -> String {
    std::fs::read_to_string(root.join("ws").join(name)).unwrap()
}

fn cleanup(root: &Path) {
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn crash_before_capture_refuses_to_overwrite_external_bytes() {
    let root = crash_at("mutation-before-capture");
    let wal = find_wal(&root);
    assert_eq!(
        read_ws(&root, "a.txt"),
        "v3 external\n",
        "the uncaptured mutation must be on disk after the crash"
    );

    // `--undo` must fail closed: the live bytes differ from the revision the WAL names.
    let undo = run_cli(&root, &["--resume", wal.to_str().unwrap(), "--undo"]);
    assert_eq!(
        undo.status.code(),
        Some(EXIT_RESTORE_CONFLICT),
        "expected the restore-conflict exit code; stderr: {}",
        stderr_of(&undo)
    );
    assert_eq!(
        read_ws(&root, "a.txt"),
        "v3 external\n",
        "a refused undo must not touch the external bytes"
    );

    // The WAL still describes `v2` as current: nothing was silently adopted.
    let listed = run_cli(
        &root,
        &["--resume", wal.to_str().unwrap(), "--list-revisions"],
    );
    assert_eq!(listed.status.code(), Some(0), "{}", stderr_of(&listed));
    assert_eq!(
        stdout_of(&listed).matches("<- current").count(),
        1,
        "exactly one revision is current"
    );
    cleanup(&root);
}

#[test]
fn crash_between_snapshot_and_wal_adopts_the_orphan_manifest() {
    let root = crash_at("snapshot-before-wal");
    let wal = find_wal(&root);
    assert_eq!(read_ws(&root, "a.txt"), "v2\n");

    // Reconciliation must adopt the orphaned manifest before `--undo` reads the leaf.
    let listed = run_cli(
        &root,
        &["--resume", wal.to_str().unwrap(), "--list-revisions"],
    );
    assert_eq!(listed.status.code(), Some(0), "{}", stderr_of(&listed));
    let listing = stdout_of(&listed);
    assert_eq!(
        listing.matches("<- current").count(),
        1,
        "one current revision after adoption:\n{}",
        listing
    );

    // With the orphan adopted, `--undo` restores the pre-mutation bytes.
    let undo = run_cli(&root, &["--resume", wal.to_str().unwrap(), "--undo"]);
    assert_eq!(undo.status.code(), Some(0), "{}", stderr_of(&undo));
    assert_eq!(
        read_ws(&root, "a.txt"),
        "v1\n",
        "undo after reconciliation restores the baseline bytes"
    );
    cleanup(&root);
}

#[test]
fn crash_mid_wal_append_discards_the_torn_line() {
    let root = crash_at("torn-wal-append");
    let wal = find_wal(&root);

    // Recovery discards the torn fragment and keeps the last complete revision.
    let undo = run_cli(&root, &["--resume", wal.to_str().unwrap(), "--undo"]);
    assert_eq!(undo.status.code(), Some(0), "{}", stderr_of(&undo));
    assert_eq!(read_ws(&root, "a.txt"), "v1\n");
    let bytes = std::fs::read(&wal).unwrap();
    assert!(
        !String::from_utf8_lossy(&bytes).contains("torn-frag"),
        "the torn trailing fragment must be truncated on recovery"
    );
    cleanup(&root);
}

#[test]
fn crash_before_leaf_move_completes_the_fork_from_its_intent() {
    let root = crash_at("fork-before-leaf-move");
    let wal = find_wal(&root);

    // The fork is completed from the durable intent: the leaf moves and the workspace is on the
    // forked (turn 0) revision.
    let listed = run_cli(
        &root,
        &["--resume", wal.to_str().unwrap(), "--list-revisions"],
    );
    assert_eq!(listed.status.code(), Some(0), "{}", stderr_of(&listed));
    let listing = stdout_of(&listed);
    assert_eq!(
        listing.matches("<- current").count(),
        1,
        "one current revision after fork recovery:\n{}",
        listing
    );
    assert_eq!(read_ws(&root, "a.txt"), "v1\n");
    cleanup(&root);
}

#[test]
fn crash_mid_restore_finishes_the_recorded_restore() {
    let root = crash_at("restore-midway");
    let wal = find_wal(&root);
    // The crash left a partially applied tree: one file restored, one not.
    assert_eq!(read_ws(&root, "a.txt"), "v1\n");
    assert_eq!(read_ws(&root, "b.txt"), "v2\n");
    let target = baseline_revision(&root);

    // Re-running the same restore target finishes the recorded intent.
    let restored = run_cli(
        &root,
        &["--resume", wal.to_str().unwrap(), "--restore-rev", &target],
    );
    assert_eq!(restored.status.code(), Some(0), "{}", stderr_of(&restored));
    assert_eq!(read_ws(&root, "a.txt"), "v1\n");
    assert_eq!(read_ws(&root, "b.txt"), "v1\n");
    cleanup(&root);
}

#[test]
fn crash_mid_undo_completes_the_rewind_from_its_intent() {
    let root = crash_at("undo-before-leaf-move");
    let wal = find_wal(&root);
    assert_eq!(read_ws(&root, "a.txt"), "v1\n");

    // The next process completes the rewind from the durable intent: the leaf moves back and the
    // WAL names the restored revision, instead of refusing with a false external-modification
    // conflict while the WAL still names the undone revision.
    let listed = run_cli(
        &root,
        &["--resume", wal.to_str().unwrap(), "--list-revisions"],
    );
    assert_eq!(listed.status.code(), Some(0), "{}", stderr_of(&listed));
    let listing = stdout_of(&listed);
    assert_eq!(
        listing.matches("<- current").count(),
        1,
        "one current revision after undo recovery:\n{}",
        listing
    );
    assert_eq!(read_ws(&root, "a.txt"), "v1\n");

    // The rewind is complete: retrying `--undo` has nothing left to undo (it must not march the
    // workspace back to the undone `v2`).
    let retry = run_cli(&root, &["--resume", wal.to_str().unwrap(), "--undo"]);
    assert_eq!(retry.status.code(), Some(2), "{}", stderr_of(&retry));
    assert_eq!(read_ws(&root, "a.txt"), "v1\n");
    cleanup(&root);
}

#[test]
fn crash_with_a_corrupt_manifest_blocks_the_next_mutation() {
    let root = crash_at("corrupt-manifest");
    let wal = find_wal(&root);

    // An unreadable manifest means the WAL can no longer be proven consistent: refuse (exit 4).
    let listed = run_cli(
        &root,
        &["--resume", wal.to_str().unwrap(), "--list-revisions"],
    );
    assert_eq!(
        listed.status.code(),
        Some(EXIT_RESTORE_RECOVERY_REQUIRED),
        "expected the recovery-required exit code; stderr: {}",
        stderr_of(&listed)
    );
    cleanup(&root);
}

/// Directly exercise the *real* `kroma --undo` writing its durable rewind intent: start the CLI,
/// wait until the intent file appears on disk, then `SIGKILL` it mid-rewind and prove the next
/// process finishes the rewind instead of leaving the WAL and workspace divergent. Many
/// megabyte-sized blobs (each under the snapshot per-file cap) make the restore window long enough
/// to interrupt reliably.
#[test]
fn real_cli_undo_crash_is_completed_by_the_next_process() {
    const FILES: usize = 96;
    const FILE_BYTES: usize = 1024 * 1024;
    let root = temp_root("real-cli-undo-crash");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join("a.txt"), "v1\n").unwrap();
    for index in 0..FILES {
        std::fs::write(
            ws.join(format!("f{:03}.dat", index)),
            vec![b'x'; FILE_BYTES],
        )
        .unwrap();
    }
    let workspace = WorkspaceCap::new(&ws).unwrap();
    let xdg = root.join("xdg");
    std::fs::create_dir_all(&xdg).unwrap();
    let state_dir = xdg.join("kroma").join("runs");
    let store = SnapshotStore::new(state_dir.join("snapshots"));
    let mut journal = SessionJournal::create(state_dir.join("sessions"), &ws).unwrap();
    let leaf0 = journal.append_item(0, user("task")).unwrap();
    let v1 = store
        .capture_revision(&workspace, None, Some(&leaf0), 0)
        .unwrap();
    journal
        .record_revision_capture(
            0,
            v1.revision_id.as_str(),
            None,
            v1.changed_paths.clone(),
            v1.tracked_files.len(),
        )
        .unwrap();
    // v2 rewrites every blob, so the `--undo` restore has to rewrite them all again.
    for index in 0..FILES {
        std::fs::write(
            ws.join(format!("f{:03}.dat", index)),
            vec![b'y'; FILE_BYTES],
        )
        .unwrap();
    }
    let leaf1 = journal.append_item(1, user("turn two")).unwrap();
    let v2 = store
        .capture_revision(&workspace, Some(&v1.revision_id), Some(&leaf1), 1)
        .unwrap();
    assert_ne!(
        v2.revision_id, v1.revision_id,
        "v2 must differ from v1 for there to be something to undo"
    );
    journal
        .record_revision_capture(
            1,
            v2.revision_id.as_str(),
            Some(v1.revision_id.as_str()),
            v2.changed_paths.clone(),
            v2.tracked_files.len(),
        )
        .unwrap();
    let wal = journal.path().to_path_buf();
    drop(journal);

    let intent_path = store.fork_intent_path();
    let mut child = Command::new(env!("CARGO_BIN_EXE_kroma"))
        .arg("run")
        .arg("--workspace")
        .arg(&ws)
        .arg("--quiet")
        .arg("--resume")
        .arg(&wal)
        .arg("--undo")
        .env("XDG_STATE_HOME", &xdg)
        .env("HOME", root.join("home"))
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn the real kroma CLI");

    // Wait for the durable rewind intent, then kill the CLI while it is still restoring.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while !intent_path.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "the CLI never wrote its durable rewind intent"
        );
        if let Some(status) = child.try_wait().unwrap() {
            panic!(
                "the CLI finished (status {:?}) before the rewind could be interrupted",
                status
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    unsafe {
        libc::kill(child.id() as i32, libc::SIGKILL);
    }
    let status = child.wait().unwrap();
    assert_eq!(status.signal(), Some(libc::SIGKILL));
    assert!(
        intent_path.exists(),
        "the rewind intent must survive the crash"
    );

    // The next process completes the rewind from the intent and clears it.
    let listed = run_cli(
        &root,
        &["--resume", wal.to_str().unwrap(), "--list-revisions"],
    );
    assert_eq!(listed.status.code(), Some(0), "{}", stderr_of(&listed));
    assert_eq!(
        stdout_of(&listed).matches("<- current").count(),
        1,
        "one current revision after recovery:\n{}",
        stdout_of(&listed)
    );
    assert!(
        !intent_path.exists(),
        "recovery must clear the rewind intent"
    );
    assert_eq!(read_ws(&root, "a.txt"), "v1\n");
    cleanup(&root);
}
