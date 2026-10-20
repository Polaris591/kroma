//! B08c contract: the bench provenance read (`git -C <dir> rev-parse HEAD`) is bounded and
//! sanitized instead of the unbounded `.output()` it used to call.
//!
//! Each test drives `detect_git_commit` with a fake `git` shell script placed first on `PATH` — no
//! real repository, credential, or network hook is ever involved. They prove that a flooding or
//! stalling `git` is capped/timed out into `None`, and that only a well-formed object id reaches
//! the report. `PATH` is process-global, so the tests take a shared lock (and the suite is run with
//! `--test-threads=1`).

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use kroma_core::bench::detect_git_commit;

const SHA1: &str = "0123456789abcdef0123456789abcdef01234567";
const SHA256: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// Serializes the `PATH` mutation so the fixtures stay correct even if the runner does not pass
/// `--test-threads=1`.
static PATH_LOCK: Mutex<()> = Mutex::new(());

/// Saves `PATH` and restores it on drop, so a failing assertion cannot poison the other tests.
struct PathGuard {
    saved: Option<OsString>,
    _lock: MutexGuard<'static, ()>,
}

impl PathGuard {
    fn new() -> Self {
        let lock = PATH_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        Self {
            saved: std::env::var_os("PATH"),
            _lock: lock,
        }
    }

    /// Prepend `dir` to `PATH` for the rest of the guard's lifetime.
    fn prepend(&self, dir: &Path) {
        let mut value = dir.as_os_str().to_os_string();
        if let Some(ref old) = self.saved {
            value.push(":");
            value.push(old);
        }
        std::env::set_var("PATH", &value);
    }
}

impl Drop for PathGuard {
    fn drop(&mut self) {
        match self.saved.take() {
            Some(value) => std::env::set_var("PATH", value),
            None => std::env::remove_var("PATH"),
        }
    }
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "kroma-b08c-{}-{}-{}",
        tag,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

/// Write an executable fake `git` (a `/bin/sh` script that ignores its argv) and return its
/// directory, ready to be prepended to `PATH`.
fn fake_git(body: &str) -> PathBuf {
    let dir = temp_dir("fakegit");
    let path = dir.join("git");
    fs::write(&path, format!("#!/bin/sh\n{}\n", body)).expect("write fake git");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod fake git");
    }
    dir
}

/// The clean case: `rev-parse HEAD` prints a full SHA-1 and it is carried through unchanged.
#[tokio::test]
async fn carries_a_sha1_commit_through() {
    let git = fake_git(&format!("printf '%s\\n' {}", SHA1));
    let guard = PathGuard::new();
    guard.prepend(&git);
    let repo = temp_dir("repo");

    assert_eq!(detect_git_commit(&repo).await.as_deref(), Some(SHA1));

    drop(guard);
    let _ = fs::remove_dir_all(&git);
    let _ = fs::remove_dir_all(&repo);
}

/// A SHA-256 repository prints a 64-hex object id; it is accepted too.
#[tokio::test]
async fn carries_a_sha256_commit_through() {
    let git = fake_git(&format!("printf '%s\\n' {}", SHA256));
    let guard = PathGuard::new();
    guard.prepend(&git);
    let repo = temp_dir("repo");

    assert_eq!(detect_git_commit(&repo).await.as_deref(), Some(SHA256));

    drop(guard);
    let _ = fs::remove_dir_all(&git);
    let _ = fs::remove_dir_all(&repo);
}

/// Anything that is not a full object id — short hex, a token, a banner, empty output — is rejected
/// so it can never be attached to the report as a commit.
#[tokio::test]
async fn rejects_values_that_are_not_a_full_object_id() {
    let guard = PathGuard::new();
    let bodies = [
        "printf '%s\\n' 'deadbeef'",
        "printf '%s\\n' 'sk-live-0123456789abcdef0123456789abcdef'",
        "printf 'fatal: not a git repository\\n'",
        "printf '%s\\n' ''",
        // 39 and 41 hex characters: off by one in either direction.
        "printf '%s\\n' '0123456789abcdef0123456789abcdef0123456'",
        "printf '%s\\n' '0123456789abcdef0123456789abcdef012345678'",
    ];
    for body in bodies {
        let git = fake_git(body);
        guard.prepend(&git);
        let repo = temp_dir("repo");
        assert_eq!(
            detect_git_commit(&repo).await,
            None,
            "output must be rejected as a commit: {}",
            body
        );
        let _ = fs::remove_dir_all(&git);
        let _ = fs::remove_dir_all(&repo);
    }
    drop(guard);
}

/// A `git` that floods the pipe is stopped by the combined output cap and yields no metadata, even
/// when the flood would otherwise trim down to a valid-looking object id.
#[tokio::test]
async fn a_flooding_git_is_capped_and_yields_no_metadata() {
    // 2 MiB of newlines followed by a real object id: with no cap the leading newlines trim away
    // and the id would be accepted; the cap must stop the read first and report `None`.
    let git = fake_git(&format!(
        "head -c 2097152 /dev/zero | tr '\\0' '\\n'\nprintf '%s\\n' {}",
        SHA1
    ));
    let guard = PathGuard::new();
    guard.prepend(&git);
    let repo = temp_dir("repo");

    assert_eq!(detect_git_commit(&repo).await, None);

    drop(guard);
    let _ = fs::remove_dir_all(&git);
    let _ = fs::remove_dir_all(&repo);
}

/// A `git` that stalls is killed at the short timeout and yields no metadata. The assertion is on
/// the returned value (not a test timeout), so the red case is an explicit failure.
#[tokio::test]
async fn a_stalling_git_times_out_and_yields_no_metadata() {
    let git = fake_git(&format!("sleep 5; printf '%s\\n' {}", SHA1));
    let guard = PathGuard::new();
    guard.prepend(&git);
    let repo = temp_dir("repo");

    let started = Instant::now();
    assert_eq!(
        detect_git_commit(&repo).await,
        None,
        "a stalled git must not yield a commit"
    );
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "the read must be bounded well below the stall: {:?}",
        started.elapsed()
    );

    drop(guard);
    let _ = fs::remove_dir_all(&git);
    let _ = fs::remove_dir_all(&repo);
}
