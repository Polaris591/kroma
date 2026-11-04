//! Automatic, local-only file checkpoints.
//!
//! Snapshots live in a separate bare repository under `~/.kroma/checkpoints`
//! and are built with `git` plumbing plus a private index, so Kroma never
//! reads or writes the user's `.git`, index, HEAD or hooks. Only real Git
//! worktrees are snapshotted; elsewhere the feature is a silent no-op.
use anyhow::{bail, Context, Result};
use std::{
    env, fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

const MAX_LIST: usize = 20;
const MAX_SESSION_HISTORY: usize = 200;
const KEEP_SESSION_HISTORY: usize = 100;

#[derive(Debug, Clone)]
pub struct Checkpoint {
    pub hash: String,
    pub timestamp: i64,
    pub subject: String,
}

#[derive(Debug, Default)]
pub struct RestoreReport {
    pub restored: usize,
    pub removed: Vec<String>,
    pub remaining: Vec<String>,
}

fn path_key(path: &Path) -> String {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in path.to_string_lossy().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

fn store_dir(root: &Path) -> Result<PathBuf> {
    let home = env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .context("HOME is unavailable; file checkpoints are disabled")?;
    let base = PathBuf::from(home).join(".kroma");
    let dir = base
        .join("checkpoints")
        .join(format!("{}.git", path_key(root)));
    fs::create_dir_all(&base).context("creating checkpoint store")?;
    fs::create_dir_all(&dir).context("creating checkpoint store")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&base, fs::Permissions::from_mode(0o700))
            .context("securing checkpoint store")?;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))
            .context("securing checkpoint store")?;
    }
    Ok(dir)
}

fn repository_root(workdir: &Path) -> Result<Option<PathBuf>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(workdir)
        .args(["rev-parse", "--show-toplevel"])
        .env("GIT_TERMINAL_PROMPT", "0")
        .output();
    let Ok(output) = output else {
        return Ok(None);
    };
    if !output.status.success() {
        return Ok(None);
    }
    let root = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if root.is_empty() {
        return Ok(None);
    }
    let root = PathBuf::from(root).canonicalize()?;
    Ok(Some(root))
}

fn git(root: &Path, store: &Path, args: &[&str]) -> Result<Output> {
    let null_config = if cfg!(windows) { "NUL" } else { "/dev/null" };
    let output = Command::new("git")
        .args([
            "--git-dir",
            &store.to_string_lossy(),
            "--work-tree",
            &root.to_string_lossy(),
            "--no-pager",
        ])
        .args(args)
        .env("GIT_DIR", store)
        .env("GIT_WORK_TREE", root)
        .env("GIT_INDEX_FILE", store.join("index"))
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_AUTHOR_NAME", "Kroma")
        .env("GIT_AUTHOR_EMAIL", "kroma@localhost")
        .env("GIT_COMMITTER_NAME", "Kroma")
        .env("GIT_COMMITTER_EMAIL", "kroma@localhost")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", null_config)
        .env("GIT_CONFIG_SYSTEM", null_config)
        .output()
        .context("running git for a file checkpoint")?;
    if !output.status.success() {
        bail!(
            "git checkpoint command failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output)
}

fn init_store(store: &Path) -> Result<()> {
    let output = Command::new("git")
        .args([
            "--git-dir",
            &store.to_string_lossy(),
            "--no-pager",
            "init",
            "--bare",
            "--quiet",
            "--template=",
        ])
        .env("GIT_DIR", store)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env(
            "GIT_CONFIG_GLOBAL",
            if cfg!(windows) { "NUL" } else { "/dev/null" },
        )
        .env(
            "GIT_CONFIG_SYSTEM",
            if cfg!(windows) { "NUL" } else { "/dev/null" },
        )
        .output()
        .context("initializing checkpoint store")?;
    if !output.status.success() {
        bail!(
            "git checkpoint init failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

fn initialized_store(root: &Path) -> Result<PathBuf> {
    let store = store_dir(root)?;
    if !store.join("HEAD").is_file() {
        init_store(&store)?;
        git(root, &store, &["config", "core.autocrlf", "false"])?;
        git(root, &store, &["config", "core.safecrlf", "false"])?;
    }
    // Older Kroma versions committed through HEAD's master branch, which
    // retained every pruned snapshot. All current refs are session refs.
    if git(
        root,
        &store,
        &["rev-parse", "-q", "--verify", "refs/heads/master"],
    )
    .is_ok()
    {
        let _ = git(root, &store, &["update-ref", "-d", "refs/heads/master"]);
    }
    Ok(store)
}

fn session_ref(session: &str) -> String {
    format!("refs/heads/session-{}", path_key(Path::new(session)))
}

fn prune_history(root: &Path, store: &Path, reference: &str) -> Result<()> {
    prune_history_to(
        root,
        store,
        reference,
        MAX_SESSION_HISTORY,
        KEEP_SESSION_HISTORY,
    )
}

fn prune_history_to(
    root: &Path,
    store: &Path,
    reference: &str,
    max: usize,
    keep: usize,
) -> Result<()> {
    let count =
        String::from_utf8_lossy(&git(root, store, &["rev-list", "--count", reference])?.stdout)
            .trim()
            .parse::<usize>()
            .unwrap_or(0);
    if count <= max {
        return Ok(());
    }
    let keep = keep.max(1);
    let list = String::from_utf8_lossy(
        &git(
            root,
            store,
            &[
                "rev-list",
                "--reverse",
                &format!("--max-count={keep}"),
                reference,
            ],
        )?
        .stdout,
    )
    .into_owned();
    let mut parent: Option<String> = None;
    for hash in list.lines().filter(|line| !line.trim().is_empty()) {
        let tree = String::from_utf8_lossy(
            &git(root, store, &["rev-parse", &format!("{hash}^{{tree}}")])?.stdout,
        )
        .trim()
        .to_owned();
        let message =
            String::from_utf8_lossy(&git(root, store, &["log", "-1", "--format=%s", hash])?.stdout)
                .trim()
                .to_owned();
        let mut commit = vec![
            "-c",
            "commit.gpgsign=false",
            "commit-tree",
            tree.as_str(),
            "-m",
            message.as_str(),
        ];
        if let Some(parent) = parent.as_deref() {
            commit.push("-p");
            commit.push(parent);
        }
        parent = Some(
            String::from_utf8_lossy(&git(root, store, &commit)?.stdout)
                .trim()
                .to_owned(),
        );
    }
    if let Some(latest) = parent {
        git(root, store, &["update-ref", reference, &latest])?;
        git(root, store, &["update-ref", "--no-deref", "HEAD", &latest])?;
    }
    // Unreachable snapshots are removed in one pass; failure to collect is
    // non-fatal because the reference itself is already bounded.
    let _ = git(root, store, &["gc", "--prune=now", "--quiet"]);
    Ok(())
}
fn lock_contention(error: &anyhow::Error) -> bool {
    let text = format!("{error:#}").to_ascii_lowercase();
    text.contains("index.lock")
        || text.contains("cannot lock ref")
        || text.contains("unable to create") && text.contains(".lock")
}

fn add_with_retry(root: &Path, store: &Path, args: &[&str]) -> Result<()> {
    let mut last = None;
    for pause in [0u64, 25, 50, 100, 200] {
        if pause > 0 {
            std::thread::sleep(std::time::Duration::from_millis(pause));
        }
        match git(root, store, args) {
            Ok(_) => return Ok(()),
            Err(error) if lock_contention(&error) => last = Some(error),
            Err(error) => return Err(error),
        }
    }
    Err(last.expect("lock retry records an error"))
}

fn snapshot_turn(root: &Path, store: &Path, session: &str, turn: u64) -> Result<Option<String>> {
    // Common build/cache directories are excluded even when a repository has
    // no .gitignore yet. Everything else follows Git's own ignore rules.
    let add_args = vec![
        "add",
        "-A",
        "--",
        ".",
        ":(exclude)target",
        ":(exclude)node_modules",
        ":(exclude)dist",
        ":(exclude)build",
        ":(exclude).next",
        ":(exclude).venv",
        ":(exclude)vendor",
        ":(exclude).kroma",
    ];
    add_with_retry(root, store, &add_args)?;
    let tree = String::from_utf8_lossy(&git(root, store, &["write-tree"])?.stdout)
        .trim()
        .to_owned();
    let reference = session_ref(session);
    let previous = git(
        root,
        store,
        &[
            "rev-parse",
            "-q",
            "--verify",
            &format!("{reference}^{{tree}}"),
        ],
    );
    if let Ok(output) = previous {
        if String::from_utf8_lossy(&output.stdout).trim() == tree {
            return Ok(None);
        }
    }
    let message = format!("turn {turn}");
    let parent = git(root, store, &["rev-parse", "-q", "--verify", &reference])
        .ok()
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .filter(|parent| !parent.is_empty());
    let mut commit = vec![
        "-c",
        "commit.gpgsign=false",
        "commit-tree",
        tree.as_str(),
        "-m",
        message.as_str(),
    ];
    if let Some(parent) = parent.as_deref() {
        commit.push("-p");
        commit.push(parent);
    }
    let hash = String::from_utf8_lossy(&git(root, store, &commit)?.stdout)
        .trim()
        .to_owned();
    git(root, store, &["update-ref", &reference, &hash])?;
    // Keep the bare store browsable with a plain `git log`; detached HEAD does
    // not create a second branch that would retain pruned history.
    git(root, store, &["update-ref", "--no-deref", "HEAD", &hash])?;
    if let Err(error) = prune_history(root, store, &reference) {
        crate::runtime_log!("[checkpoints] history prune skipped: {error:#}");
    }
    let hash = String::from_utf8_lossy(&git(root, store, &["rev-parse", &reference])?.stdout)
        .trim()
        .to_owned();
    Ok(Some(hash))
}

/// Snapshot the worktree before a turn. Returns the new commit, or `None` when
/// the tree is unchanged, the feature is disabled, or the directory is not a
/// Git worktree.
pub fn snapshot(workdir: &Path, session: &str, turn: u64) -> Result<Option<String>> {
    if env::var_os("KROMA_NO_CHECKPOINTS").is_some() {
        return Ok(None);
    }
    let Some(root) = repository_root(workdir)? else {
        return Ok(None);
    };
    let store = initialized_store(&root)?;
    snapshot_turn(&root, &store, session, turn)
}

pub fn list(workdir: &Path, session: &str) -> Result<Vec<Checkpoint>> {
    let Some(root) = repository_root(workdir)? else {
        return Ok(Vec::new());
    };
    let store = store_dir(&root)?;
    list_from(&root, &store, session)
}

fn list_from(root: &Path, store: &Path, session: &str) -> Result<Vec<Checkpoint>> {
    if !store.join("HEAD").is_file() {
        return Ok(Vec::new());
    }
    let reference = session_ref(session);
    if git(root, store, &["rev-parse", "-q", "--verify", &reference]).is_err() {
        return Ok(Vec::new());
    }
    let output = git(
        root,
        store,
        &[
            "log",
            &format!("--max-count={MAX_LIST}"),
            "--format=%H%x09%ct%x09%s",
            &reference,
            "--",
        ],
    )?;
    let mut checkpoints = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let mut parts = line.splitn(3, '\t');
        let (Some(hash), Some(timestamp), Some(subject)) =
            (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        checkpoints.push(Checkpoint {
            hash: hash.to_owned(),
            timestamp: timestamp.parse().unwrap_or(0),
            subject: subject.to_owned(),
        });
    }
    Ok(checkpoints)
}

/// Restore every file captured by a checkpoint, removing non-ignored files
/// created after it. Excluded directories and ignored files are left alone.
pub fn restore(workdir: &Path, session: &str, hash: &str) -> Result<RestoreReport> {
    let Some(root) = repository_root(workdir)? else {
        bail!("file checkpoints are only available inside a Git worktree");
    };
    let store = store_dir(&root)?;
    restore_from(&root, &store, session, hash)
}

fn restore_from(root: &Path, store: &Path, session: &str, hash: &str) -> Result<RestoreReport> {
    if !store.join("HEAD").is_file() {
        bail!("no file checkpoints exist for this project yet");
    }
    let reference = session_ref(session);
    let check = git(
        root,
        store,
        &["merge-base", "--is-ancestor", hash, &reference],
    );
    if check.is_err() {
        bail!("that checkpoint does not belong to this conversation");
    }
    let changed = git(root, store, &["diff", "--name-only", hash, "--", "."])?;
    let restored = String::from_utf8_lossy(&changed.stdout)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .count();
    git(root, store, &["checkout", hash, "--", "."])?;
    let extras = git(
        root,
        store,
        &["ls-files", "--others", "--exclude-standard", "-z"],
    )?
    .stdout;
    let mut removed = Vec::new();
    let mut remaining = Vec::new();
    for path in extras.split(|byte| *byte == 0) {
        let path = String::from_utf8_lossy(path).into_owned();
        if path.is_empty() {
            continue;
        }
        let first = path.split('/').next().unwrap_or("");
        if matches!(
            first,
            ".git"
                | "target"
                | "node_modules"
                | "dist"
                | "build"
                | ".next"
                | ".venv"
                | "vendor"
                | ".kroma"
        ) {
            continue;
        }
        match fs::remove_file(root.join(&path)) {
            Ok(()) => removed.push(path),
            Err(error) => remaining.push(format!("{path} ({error})")),
        }
    }
    Ok(RestoreReport {
        restored,
        removed,
        remaining,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "kroma-checkpoint-{label}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn snapshot_deduplicates_and_restores_a_git_worktree() -> Result<()> {
        let root = temp_dir("restore");
        let store = temp_dir("store").join("shadow.git");
        let init = Command::new("git")
            .args(["-C", &root.to_string_lossy(), "init", "-q"])
            .status()?;
        assert!(init.success());
        fs::write(root.join("notes.txt"), "first\n")?;
        init_store(&store)?;

        let first = snapshot_turn(&root, &store, "session-a", 1)?.expect("first snapshot");
        assert!(snapshot_turn(&root, &store, "session-a", 2)?.is_none());

        fs::write(root.join("notes.txt"), "second\n")?;
        let second = snapshot_turn(&root, &store, "session-a", 3)?.expect("second snapshot");
        assert_ne!(first, second);
        fs::write(
            root.join("new.txt"),
            "created later
",
        )?;
        assert_eq!(list_from(&root, &store, "session-a")?.len(), 2);

        let report = restore_from(&root, &store, "session-a", &first)?;
        assert_eq!(report.restored, 1);
        assert_eq!(report.removed, vec!["new.txt"]);
        assert!(!root.join("new.txt").exists());
        assert_eq!(fs::read_to_string(root.join("notes.txt"))?, "first\n");
        fs::remove_dir_all(root).ok();
        Ok(())
    }
}

#[cfg(test)]
mod history_tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "kroma-checkpoint-history-{label}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn count(root: &Path, store: &Path, reference: &str) -> usize {
        String::from_utf8_lossy(
            &git(root, store, &["rev-list", "--count", reference])
                .unwrap()
                .stdout,
        )
        .trim()
        .parse()
        .unwrap()
    }

    #[test]
    fn session_history_can_be_bounded_without_losing_the_latest_snapshot() -> Result<()> {
        let root = temp_dir("root");
        let store = temp_dir("store").join("shadow.git");
        let init = Command::new("git")
            .args(["-C", &root.to_string_lossy(), "init", "-q"])
            .status()?;
        assert!(init.success());
        init_store(&store)?;
        fs::write(root.join("notes.txt"), "one\n")?;
        snapshot_turn(&root, &store, "session-a", 1)?;
        fs::write(root.join("notes.txt"), "two\n")?;
        snapshot_turn(&root, &store, "session-a", 2)?;
        fs::write(root.join("notes.txt"), "three\n")?;
        let latest = snapshot_turn(&root, &store, "session-a", 3)?.unwrap();
        let reference = session_ref("session-a");
        assert_eq!(count(&root, &store, &reference), 3);
        let latest_tree = String::from_utf8_lossy(
            &git(&root, &store, &["rev-parse", &format!("{latest}^{{tree}}")])?.stdout,
        )
        .trim()
        .to_owned();

        prune_history_to(&root, &store, &reference, 2, 1)?;
        assert_eq!(count(&root, &store, &reference), 1);
        assert_eq!(
            String::from_utf8_lossy(
                &git(
                    &root,
                    &store,
                    &["rev-parse", &format!("{reference}^{{tree}}")]
                )?
                .stdout
            )
            .trim(),
            latest_tree
        );
        fs::remove_dir_all(root).ok();
        fs::remove_dir_all(store.parent().unwrap()).ok();
        Ok(())
    }
}
