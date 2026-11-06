//! Automatic, local-only file checkpoints.
//!
//! Snapshots live in a separate bare repository under `~/.kroma/checkpoints`
//! and are built with `git` plumbing plus a private index, so Kroma never
//! reads or writes the user's `.git`, index, HEAD or hooks. Only real Git
//! worktrees are snapshotted; elsewhere the feature is a silent no-op.
//!
//! Every snapshot/list/restore/prune transaction for one project is serialized
//! by an owner-only lock file in the private store. The store is shared by all
//! sessions of that project, so the lock also protects the private index from
//! concurrent Kroma processes.
use anyhow::{bail, Context, Result};
use std::{
    collections::HashSet,
    env, fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

const MAX_LIST: usize = 20;
const MAX_SESSION_HISTORY: usize = 200;
const KEEP_SESSION_HISTORY: usize = 100;
const LOCK_TIMEOUT: Duration = Duration::from_secs(30);
const LOCK_FILE: &str = "kroma-transaction.lock";
const EXCLUDED_DIRS: &[&str] = &[
    "target",
    "node_modules",
    "dist",
    "build",
    ".next",
    ".venv",
    "vendor",
    ".kroma",
];

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

fn store_base() -> Result<PathBuf> {
    let home = env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .context("HOME is unavailable; file checkpoints are disabled")?;
    Ok(PathBuf::from(home).join(".kroma"))
}

fn store_dir(root: &Path) -> Result<PathBuf> {
    let base = store_base()?;
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

fn repository_root(workdir: &Path, cancel: Option<Arc<AtomicBool>>) -> Result<Option<PathBuf>> {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(workdir)
        .args(["rev-parse", "--show-toplevel"])
        .env("GIT_TERMINAL_PROMPT", "0")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
        .env_remove("GIT_NAMESPACE")
        .env_remove("GIT_PREFIX");
    let output = if let Some(cancel) = cancel {
        let repo = Repo::new(workdir, &env::temp_dir(), Some(cancel));
        repo.check_cancelled()?;
        match repo.run_command(command, None) {
            Ok(output) => output,
            Err(error) if repo.cancelled() => return Err(error),
            Err(_) => return Ok(None),
        }
    } else {
        let Ok(output) = command.output() else {
            return Ok(None);
        };
        output
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

struct Repo {
    root: PathBuf,
    store: PathBuf,
    cancel: Option<Arc<AtomicBool>>,
}

impl Repo {
    fn new(root: &Path, store: &Path, cancel: Option<Arc<AtomicBool>>) -> Self {
        Self {
            root: root.to_path_buf(),
            store: store.to_path_buf(),
            cancel,
        }
    }

    fn cancelled(&self) -> bool {
        self.cancel
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::SeqCst))
    }

    fn check_cancelled(&self) -> Result<()> {
        if self.cancelled() {
            bail!("file checkpoint cancelled");
        }
        Ok(())
    }

    fn git(&self, args: &[&str]) -> Result<Output> {
        self.git_with_input(args, None)
    }

    fn git_with_input(&self, args: &[&str], input: Option<&[u8]>) -> Result<Output> {
        self.check_cancelled()?;
        let null_config = if cfg!(windows) { "NUL" } else { "/dev/null" };
        let store = self.store.to_string_lossy().into_owned();
        let root = self.root.to_string_lossy().into_owned();
        let index = self.store.join("index").to_string_lossy().into_owned();
        let mut command = Command::new("git");
        command
            .current_dir(&self.root)
            .args([
                "--git-dir",
                &store,
                "--work-tree",
                &root,
                "--no-pager",
                "-c",
                "gc.auto=0",
            ])
            .args(args)
            .env("GIT_DIR", &store)
            .env("GIT_WORK_TREE", &root)
            .env("GIT_INDEX_FILE", &index)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_AUTHOR_NAME", "Kroma")
            .env("GIT_AUTHOR_EMAIL", "kroma@localhost")
            .env("GIT_COMMITTER_NAME", "Kroma")
            .env("GIT_COMMITTER_EMAIL", "kroma@localhost")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", null_config)
            .env("GIT_CONFIG_SYSTEM", null_config)
            .env_remove("GIT_COMMON_DIR")
            .env_remove("GIT_OBJECT_DIRECTORY")
            .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
            .env_remove("GIT_NAMESPACE")
            .env_remove("GIT_PREFIX");
        self.run_command(command, input)
    }

    fn bare_git(&self, args: &[&str]) -> Result<Output> {
        self.check_cancelled()?;
        let null_config = if cfg!(windows) { "NUL" } else { "/dev/null" };
        let store = self.store.to_string_lossy().into_owned();
        let mut command = Command::new("git");
        command
            .args(["--git-dir", &store, "--no-pager", "-c", "gc.auto=0"])
            .args(args)
            .env("GIT_DIR", &store)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", null_config)
            .env("GIT_CONFIG_SYSTEM", null_config)
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_COMMON_DIR")
            .env_remove("GIT_OBJECT_DIRECTORY")
            .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
            .env_remove("GIT_NAMESPACE")
            .env_remove("GIT_PREFIX");
        self.run_command(command, None)
    }

    fn run_command(&self, mut command: Command, input: Option<&[u8]>) -> Result<Output> {
        // Ordinary list/restore operations need no polling or scratch output.
        if self.cancel.is_none() && input.is_none() {
            return checked_output(
                command
                    .output()
                    .context("running git for a file checkpoint")?,
            );
        }
        fs::create_dir_all(&self.store)?;
        let mut scratch = ScratchFiles(Vec::new());
        let (stdout_file, stdout_path) = scratch.create(&self.store, "out")?;
        let (stderr_file, stderr_path) = scratch.create(&self.store, "err")?;
        command
            .stdout(stdout_file)
            .stderr(stderr_file)
            .stdin(Stdio::null());
        if let Some(bytes) = input {
            let (mut file, path) = scratch.create(&self.store, "in")?;
            file.write_all(bytes)?;
            drop(file);
            command.stdin(fs::File::open(path)?);
        }
        let status = if self.cancel.is_none() {
            command.status()?
        } else {
            // Git transactions are synchronous and may also run from tests or
            // external hosts without a Tokio runtime. tokio::process::Command
            // needs a reactor even for a polling try_wait loop; supply a small
            // temporary one only in that case. The ordinary Agent path runs
            // inside spawn_blocking and reuses its existing runtime handle.
            let fallback_runtime = if tokio::runtime::Handle::try_current().is_err() {
                Some(
                    tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .context("initializing cancellable Git process reactor")?,
                )
            } else {
                None
            };
            let _reactor_guard = fallback_runtime.as_ref().map(|runtime| runtime.enter());
            let mut command = tokio::process::Command::from(command);
            command.kill_on_drop(true);
            #[cfg(unix)]
            {
                use std::os::unix::process::CommandExt;
                command.as_std_mut().process_group(0);
            }
            #[cfg(windows)]
            let (mut child, _tree) = crate::winsys::ChildTreeGuard::spawn(&mut command)?;
            #[cfg(not(windows))]
            let mut child = command.spawn()?;
            loop {
                if self.cancelled() {
                    #[cfg(windows)]
                    _tree.terminate();
                    #[cfg(unix)]
                    if let Some(pid) = child.id() {
                        // The child owns a fresh process group, including Git helpers.
                        unsafe {
                            libc::kill(-(pid as i32), libc::SIGKILL);
                        }
                    }
                    let _ = child.start_kill();
                    while child.try_wait()?.is_none() {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    bail!("file checkpoint cancelled");
                }
                match child.try_wait()? {
                    Some(status) => break status,
                    None => std::thread::sleep(Duration::from_millis(5)),
                }
            }
        };
        checked_output(Output {
            status,
            stdout: fs::read(stdout_path)?,
            stderr: fs::read(stderr_path)?,
        })
    }
}

fn checked_output(output: Output) -> Result<Output> {
    if !output.status.success() {
        bail!(
            "git checkpoint command failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output)
}

struct ScratchFiles(Vec<PathBuf>);
impl ScratchFiles {
    fn create(&mut self, store: &Path, tag: &str) -> Result<(fs::File, PathBuf)> {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = store.join(format!(
            ".kroma-{tag}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options
            .open(&path)
            .context("creating git checkpoint scratch file")?;
        self.0.push(path.clone());
        Ok((file, path))
    }
}
impl Drop for ScratchFiles {
    fn drop(&mut self) {
        for path in &self.0 {
            let _ = fs::remove_file(path);
        }
    }
}

fn init_store(repo: &Repo) -> Result<()> {
    repo.bare_git(&["init", "--bare", "--quiet", "--template="])?;
    Ok(())
}

fn initialized_store(repo: &Repo) -> Result<()> {
    let store = &repo.store;
    if !store.join("HEAD").is_file() {
        repo.check_cancelled()?;
        init_store(repo)?;
        repo.git(&["config", "core.autocrlf", "false"])?;
        repo.git(&["config", "core.safecrlf", "false"])?;
    }
    // Older Kroma versions committed through HEAD's master branch, which
    // retained every pruned snapshot. All current refs are session refs.
    if repo
        .git(&["rev-parse", "-q", "--verify", "refs/heads/master"])
        .is_ok()
    {
        let _ = repo.git(&["update-ref", "-d", "refs/heads/master"]);
    }
    Ok(())
}

fn session_ref(session: &str) -> String {
    format!("refs/heads/session-{}", path_key(Path::new(session)))
}

fn pathspecs() -> Vec<String> {
    let mut specs = vec![".".to_owned()];
    specs.extend(EXCLUDED_DIRS.iter().map(|dir| format!(":(exclude){dir}")));
    specs
}

fn is_excluded(path: &Path) -> bool {
    path.components()
        .next()
        .and_then(|part| part.as_os_str().to_str())
        .is_some_and(|first| EXCLUDED_DIRS.contains(&first))
}

fn prune_history(repo: &Repo, reference: &str) -> Result<()> {
    prune_history_to(repo, reference, MAX_SESSION_HISTORY, KEEP_SESSION_HISTORY)
}

fn prune_history_to(repo: &Repo, reference: &str, max: usize, keep: usize) -> Result<()> {
    let count = String::from_utf8_lossy(&repo.git(&["rev-list", "--count", reference])?.stdout)
        .trim()
        .parse::<usize>()
        .unwrap_or(0);
    if count <= max {
        return Ok(());
    }
    let keep = keep.max(1);
    let list = String::from_utf8_lossy(
        &repo
            .git(&[
                "rev-list",
                "--reverse",
                &format!("--max-count={keep}"),
                reference,
            ])?
            .stdout,
    )
    .into_owned();
    let mut parent: Option<String> = None;
    for hash in list.lines().filter(|line| !line.trim().is_empty()) {
        repo.check_cancelled()?;
        let tree = String::from_utf8_lossy(
            &repo
                .git(&["rev-parse", &format!("{hash}^{{tree}}")])?
                .stdout,
        )
        .trim()
        .to_owned();
        let message =
            String::from_utf8_lossy(&repo.git(&["log", "-1", "--format=%s", hash])?.stdout)
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
            String::from_utf8_lossy(&repo.git(&commit)?.stdout)
                .trim()
                .to_owned(),
        );
    }
    if let Some(latest) = parent {
        repo.check_cancelled()?;
        repo.git(&["update-ref", reference, &latest])?;
        repo.git(&["update-ref", "--no-deref", "HEAD", &latest])?;
    }
    // Unreachable snapshots are removed in one pass; failure to collect is
    // non-fatal because the reference itself is already bounded.
    let _ = repo.git(&["gc", "--prune=now", "--quiet"]);
    Ok(())
}

fn snapshot_turn(repo: &Repo, session: &str, turn: u64) -> Result<Option<String>> {
    let specs = pathspecs();
    let mut add_args = vec!["add", "-A", "--"];
    add_args.extend(specs.iter().map(String::as_str));
    repo.git(&add_args)?;
    let tree = String::from_utf8_lossy(&repo.git(&["write-tree"])?.stdout)
        .trim()
        .to_owned();
    let reference = session_ref(session);
    let previous = repo.git(&[
        "rev-parse",
        "-q",
        "--verify",
        &format!("{reference}^{{tree}}"),
    ]);
    if let Ok(output) = previous {
        if String::from_utf8_lossy(&output.stdout).trim() == tree {
            return Ok(None);
        }
    }
    repo.check_cancelled()?;
    let parent = repo
        .git(&["rev-parse", "-q", "--verify", &reference])
        .ok()
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .filter(|parent| !parent.is_empty());
    // Agent's in-memory turn counter resets on process restart. Persisted Git
    // ancestry is authoritative for display numbering, so a restarted session
    // never labels two different commits "turn 1".
    let previous_turn = parent.as_deref().and_then(|hash| {
        repo.git(&["show", "-s", "--format=%s", hash])
            .ok()
            .and_then(|output| {
                String::from_utf8_lossy(&output.stdout)
                    .trim()
                    .strip_prefix("turn ")
                    .and_then(|number| number.parse::<u64>().ok())
            })
    });
    let turn = previous_turn.map_or(turn, |last| turn.max(last.saturating_add(1)));
    let message = format!("turn {turn}");
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
    let hash = String::from_utf8_lossy(&repo.git(&commit)?.stdout)
        .trim()
        .to_owned();
    repo.check_cancelled()?;
    repo.git(&["update-ref", &reference, &hash])?;
    // Keep the bare store browsable with a plain `git log`; detached HEAD does
    // not create a second branch that would retain pruned history.
    repo.git(&["update-ref", "--no-deref", "HEAD", &hash])?;
    if let Err(error) = prune_history(repo, &reference) {
        if repo.cancelled() {
            return Err(error);
        }
        crate::runtime_log!("[checkpoints] history prune skipped: {error:#}");
    }
    let hash = String::from_utf8_lossy(&repo.git(&["rev-parse", &reference])?.stdout)
        .trim()
        .to_owned();
    Ok(Some(hash))
}

fn lock_store(store: &Path, cancel: Option<&AtomicBool>) -> Result<fs::File> {
    let path = store.join(LOCK_FILE);
    let deadline = Instant::now() + LOCK_TIMEOUT;
    loop {
        if cancel.is_some_and(|flag| flag.load(Ordering::SeqCst)) {
            bail!("file checkpoint cancelled");
        }
        match crate::file_lock::try_acquire(&path) {
            Ok(file) => return Ok(file),
            Err(error) if crate::file_lock::contended(&error) => {
                if Instant::now() >= deadline {
                    bail!("timed out waiting for another Kroma checkpoint transaction");
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => return Err(error).with_context(|| format!("locking {}", path.display())),
        }
    }
}

fn run_transaction<T>(
    root: &Path,
    store: &Path,
    cancel: Option<Arc<AtomicBool>>,
    action: impl FnOnce(&Repo) -> Result<T>,
) -> Result<T> {
    let _lock = lock_store(store, cancel.as_deref())?;
    let repo = Repo::new(root, store, cancel);
    let result = action(&repo);
    if repo.cancelled() {
        remove_git_locks(store)?;
    }
    result
}

fn remove_git_locks(store: &Path) -> Result<()> {
    for entry in fs::read_dir(store)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            if entry.file_name() != "objects" {
                remove_git_locks(&path)?;
            }
        } else if (path.extension().is_some_and(|ext| ext == "lock")
            || entry.file_name() == "gc.pid")
            && entry.file_name() != LOCK_FILE
        {
            fs::remove_file(path)?;
        }
    }
    Ok(())
}

fn snapshot_inner(
    workdir: &Path,
    session: &str,
    turn: u64,
    cancel: Option<Arc<AtomicBool>>,
) -> Result<Option<String>> {
    if env::var_os("KROMA_NO_CHECKPOINTS").is_some() {
        return Ok(None);
    }
    let Some(root) = repository_root(workdir, cancel.clone())? else {
        return Ok(None);
    };
    let store = store_dir(&root)?;
    run_transaction(&root, &store, cancel, |repo| {
        initialized_store(repo)?;
        snapshot_turn(repo, session, turn)
    })
}

/// Snapshot the worktree before a turn. Returns the new commit, or `None` when
/// the tree is unchanged, the feature is disabled, or the directory is not a
/// Git worktree.
pub fn snapshot(workdir: &Path, session: &str, turn: u64) -> Result<Option<String>> {
    snapshot_inner(workdir, session, turn, None)
}

/// Cancellable snapshot used by an agent task. Dropping the task future sets
/// the flag; the blocking transaction then kills Git and releases its lock.
pub(crate) fn snapshot_cancellable(
    workdir: &Path,
    session: &str,
    turn: u64,
    cancelled: Arc<AtomicBool>,
) -> Result<Option<String>> {
    snapshot_inner(workdir, session, turn, Some(cancelled))
}

pub fn list(workdir: &Path, session: &str) -> Result<Vec<Checkpoint>> {
    let Some(root) = repository_root(workdir, None)? else {
        return Ok(Vec::new());
    };
    let store = store_dir(&root)?;
    run_transaction(&root, &store, None, |repo| list_from(repo, session))
}

fn list_from(repo: &Repo, session: &str) -> Result<Vec<Checkpoint>> {
    if !repo.store.join("HEAD").is_file() {
        return Ok(Vec::new());
    }
    let reference = session_ref(session);
    if repo
        .git(&["rev-parse", "-q", "--verify", &reference])
        .is_err()
    {
        return Ok(Vec::new());
    }
    let output = repo.git(&[
        "log",
        &format!("--max-count={MAX_LIST}"),
        "--format=%H%x09%ct%x09%s",
        &reference,
        "--",
    ])?;
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
/// created or first captured after it. Excluded directories and ignored files
/// are left alone.
pub fn restore(workdir: &Path, session: &str, hash: &str) -> Result<RestoreReport> {
    let Some(root) = repository_root(workdir, None)? else {
        bail!("file checkpoints are only available inside a Git worktree");
    };
    let store = store_dir(&root)?;
    run_transaction(&root, &store, None, |repo| {
        restore_from(repo, session, hash)
    })
}

fn split_paths(bytes: &[u8]) -> impl Iterator<Item = &[u8]> {
    bytes
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
}

fn path_from_bytes(bytes: &[u8]) -> PathBuf {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        PathBuf::from(std::ffi::OsStr::from_bytes(bytes))
    }
    #[cfg(not(unix))]
    {
        PathBuf::from(String::from_utf8_lossy(bytes).into_owned())
    }
}

fn restore_from(repo: &Repo, session: &str, hash: &str) -> Result<RestoreReport> {
    if !repo.store.join("HEAD").is_file() {
        bail!("no file checkpoints exist for this project yet");
    }
    let reference = session_ref(session);
    if repo
        .git(&["merge-base", "--is-ancestor", hash, &reference])
        .is_err()
    {
        bail!("that checkpoint does not belong to this conversation");
    }
    let specs = pathspecs();
    let mut diff_args = vec!["diff", "--name-only", "-z", hash, "--"];
    diff_args.extend(specs.iter().map(String::as_str));
    let changed = repo.git(&diff_args)?.stdout;
    let changed: HashSet<Vec<u8>> = split_paths(&changed).map(<[u8]>::to_vec).collect();
    let mut restored = 0;
    let captured_bytes = repo
        .git(&["ls-tree", "-r", "-z", "--name-only", hash])?
        .stdout;
    let captured: HashSet<Vec<u8>> = split_paths(&captured_bytes).map(<[u8]>::to_vec).collect();
    let ignored = repo
        .git(&[
            "ls-files",
            "-z",
            "--cached",
            "--ignored",
            "--exclude-standard",
        ])?
        .stdout;
    let ignored: HashSet<Vec<u8>> = split_paths(&ignored).map(<[u8]>::to_vec).collect();
    let current = repo
        .git(&[
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ])?
        .stdout;
    let current_paths: HashSet<Vec<u8>> = split_paths(&current).map(<[u8]>::to_vec).collect();
    let mut removed = Vec::new();
    let mut remaining = Vec::new();
    for bytes in split_paths(&current) {
        if captured.contains(bytes) {
            continue;
        }
        let path = path_from_bytes(bytes);
        if is_excluded(&path) || ignored.contains(bytes) {
            continue;
        }
        if !safe_ancestors(&repo.root, &path) {
            remaining.push(format!("{} (linked ancestor preserved)", path.display()));
            continue;
        }
        let complete = repo.root.join(&path);
        if fs::symlink_metadata(&complete).is_err() {
            continue;
        }
        let report = path.to_string_lossy().into_owned();
        match fs::remove_file(&complete) {
            Ok(()) => {
                removed.push(report);
                let mut parent = complete.parent();
                while let Some(dir) = parent.filter(|dir| *dir != repo.root) {
                    if fs::remove_dir(dir).is_err() {
                        break;
                    }
                    parent = dir.parent();
                }
            }
            Err(error) => remaining.push(format!("{report} ({error})")),
        }
    }
    // Reset only our private index. Removing extras first handles file/directory
    // transitions; checkout-index refuses nonempty (including ignored) blockers.
    repo.git(&["read-tree", hash])?;
    let mut checkout = Vec::new();
    for bytes in split_paths(&captured_bytes) {
        let path = path_from_bytes(bytes);
        if is_excluded(&path) || ignored.contains(bytes) {
            continue;
        }
        if !safe_ancestors(&repo.root, &path) {
            remaining.push(format!("{} (linked ancestor preserved)", path.display()));
            continue;
        }
        let complete = repo.root.join(&path);
        if !current_paths.contains(bytes) && complete.exists() {
            continue;
        }
        if complete.is_dir() {
            let _ = fs::remove_dir(&complete);
        }
        restored += usize::from(changed.contains(bytes));
        checkout.extend_from_slice(bytes);
        checkout.push(0);
    }
    if !checkout.is_empty() {
        repo.git_with_input(
            &["checkout-index", "--force", "-z", "--stdin"],
            Some(&checkout),
        )?;
    }
    Ok(RestoreReport {
        restored,
        removed,
        remaining,
    })
}

fn safe_ancestors(root: &Path, path: &Path) -> bool {
    let mut complete = root.to_path_buf();
    let parts: Vec<_> = path.components().collect();
    for part in parts.iter().take(parts.len().saturating_sub(1)) {
        if !matches!(part, std::path::Component::Normal(_)) {
            return false;
        }
        complete.push(part.as_os_str());
        if fs::symlink_metadata(&complete).is_ok_and(|meta| {
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                meta.file_attributes() & 0x400 != 0
            }
            #[cfg(not(windows))]
            {
                meta.is_symlink()
            }
        }) {
            return false;
        }
    }
    true
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

    fn initialized(label: &str) -> Result<(PathBuf, Repo)> {
        let root = temp_dir(label);
        let store = temp_dir(&format!("{label}-store")).join("shadow.git");
        let init = Command::new("git")
            .args(["-C", &root.to_string_lossy(), "init", "-q"])
            .status()?;
        assert!(init.success());
        let repo = Repo::new(&root, &store, None);
        init_store(&repo)?;
        Ok((root, repo))
    }

    fn cleanup(root: &Path, repo: &Repo) {
        let _ = fs::remove_dir_all(root);
        if let Some(parent) = repo.store.parent() {
            let _ = fs::remove_dir_all(parent);
        }
    }

    #[tokio::test]
    #[ignore = "local filesystem benchmark; run in release mode with one test thread"]
    async fn perf_file_checkpoint_first_unchanged_and_delta() -> Result<()> {
        let (root, repo) = initialized("perf")?;
        for index in 0..1000 {
            fs::write(root.join(format!("file-{index:04}.txt")), "x".repeat(1024))?;
        }
        fs::write(
            root.join(".gitignore"),
            "ignored/
",
        )?;
        fs::create_dir(root.join("ignored"))?;
        fs::write(root.join("ignored/cache"), "not captured")?;
        let original_index = fs::read(root.join(".git/index")).ok();
        let mut first = Vec::new();
        let mut unchanged = Vec::new();
        let mut delta = Vec::new();
        for index in 0..43 {
            if index >= 22 {
                fs::write(root.join("file-0000.txt"), format!("edit-{index}"))?;
            }
            let flag = Arc::new(AtomicBool::new(false));
            let start = Instant::now();
            assert_eq!(
                repository_root(&root, Some(flag.clone()))?,
                Some(root.canonicalize()?)
            );
            let captured = run_transaction(&root, &repo.store, Some(flag), |repo| {
                initialized_store(repo)?;
                snapshot_turn(repo, "perf", index)
            })?;
            let elapsed = start.elapsed();
            if index == 0 {
                first.push(elapsed);
                assert!(captured.is_some());
            } else if index < 22 {
                unchanged.push(elapsed);
                assert!(captured.is_none());
            } else {
                delta.push(elapsed);
                assert!(captured.is_some());
            }
        }
        assert_eq!(fs::read(root.join(".git/index")).ok(), original_index);
        let fixture = serde_json::json!({"files":1000,"bytes_per_file":1024,"cancellable":true,"includes_root_discovery":true,"store_preinitialized":true});
        crate::performance::report("checkpoint.first_capture", &first, fixture.clone());
        crate::performance::report("checkpoint.unchanged", &unchanged, fixture.clone());
        crate::performance::report("checkpoint.one_file_delta", &delta, fixture);
        cleanup(&root, &repo);
        Ok(())
    }

    #[test]
    fn checkpoint_child_helper() {
        let Some(dir) = env::var_os("KROMA_CHECKPOINT_CHILD_HELPER") else {
            return;
        };
        let dir = PathBuf::from(dir);
        if env::var_os("KROMA_CHECKPOINT_DESCENDANT").is_some() {
            let mut pulse = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(dir.join("pulse"))
                .unwrap();
            loop {
                pulse.write_all(b".").unwrap();
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        fs::write(dir.join("index.lock"), "interrupted Git lock").unwrap();
        fs::write(dir.join("gc.pid"), "interrupted Git GC").unwrap();
        let mut child = Command::new(env::current_exe().unwrap())
            .args([
                "--exact",
                "checkpoints::tests::checkpoint_child_helper",
                "--nocapture",
            ])
            .env("KROMA_CHECKPOINT_DESCENDANT", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let _ = child.wait();
    }

    #[tokio::test]
    async fn cancelling_running_checkpoint_kills_descendants_and_cleans_private_files() -> Result<()>
    {
        let (root, repo) = initialized("active-cancel")?;
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_root = root.clone();
        let store = repo.store.clone();
        let flag = cancel.clone();
        let worker = tokio::task::spawn_blocking(move || {
            run_transaction(&worker_root, &store, Some(flag), |repo| {
                let mut command = Command::new(env::current_exe()?);
                command
                    .args([
                        "--exact",
                        "checkpoints::tests::checkpoint_child_helper",
                        "--nocapture",
                    ])
                    .env("KROMA_CHECKPOINT_CHILD_HELPER", &repo.store);
                repo.run_command(command, None)
            })
        });
        let deadline = Instant::now() + Duration::from_secs(10);
        while !repo.store.join("pulse").exists() && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let started = repo.store.join("pulse").exists();
        cancel.store(true, Ordering::SeqCst);
        assert!(tokio::time::timeout(Duration::from_secs(5), worker)
            .await??
            .is_err());
        assert!(started, "checkpoint child never started");
        assert!(!repo.store.join("index.lock").exists());
        assert!(!repo.store.join("gc.pid").exists());
        assert!(!fs::read_dir(&repo.store)?.any(|entry| entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".kroma-")));
        let length = fs::metadata(repo.store.join("pulse"))?.len();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            fs::metadata(repo.store.join("pulse"))?.len(),
            length,
            "descendant survived cancellation"
        );
        fs::write(root.join("next"), "next transaction")?;
        assert!(
            run_transaction(&root, &repo.store, None, |repo| snapshot_turn(
                repo, "next", 1
            ))?
            .is_some()
        );
        cleanup(&root, &repo);
        Ok(())
    }

    #[test]
    fn restore_handles_empty_trees_and_file_directory_transitions() -> Result<()> {
        let (root, repo) = initialized("types")?;
        let empty = snapshot_turn(&repo, "types", 1)?.unwrap();
        fs::write(root.join("item"), "file")?;
        let file = snapshot_turn(&repo, "types", 2)?.unwrap();
        fs::remove_file(root.join("item"))?;
        fs::create_dir(root.join("item"))?;
        fs::write(root.join("item/child"), "nested")?;
        let directory = snapshot_turn(&repo, "types", 3)?.unwrap();
        restore_from(&repo, "types", &file)?;
        assert_eq!(fs::read_to_string(root.join("item"))?, "file");
        restore_from(&repo, "types", &directory)?;
        assert_eq!(fs::read_to_string(root.join("item/child"))?, "nested");
        restore_from(&repo, "types", &empty)?;
        assert!(!root.join("item").exists());
        cleanup(&root, &repo);
        Ok(())
    }

    #[test]
    fn restore_preserves_files_that_became_ignored_after_being_captured() -> Result<()> {
        let (root, repo) = initialized("new-ignore")?;
        fs::write(root.join("keep"), "initial")?;
        let old = snapshot_turn(&repo, "ignored", 1)?.unwrap();
        fs::write(root.join("later"), "captured later")?;
        snapshot_turn(&repo, "ignored", 2)?;
        fs::write(
            root.join(".gitignore"),
            "later
",
        )?;
        fs::write(root.join("later"), "private ignored edits")?;
        restore_from(&repo, "ignored", &old)?;
        assert_eq!(
            fs::read_to_string(root.join("later"))?,
            "private ignored edits"
        );
        cleanup(&root, &repo);
        Ok(())
    }

    #[test]
    fn restoring_never_removes_files_through_symlink_or_junction_ancestors() -> Result<()> {
        let (root, repo) = initialized("linked-ancestor")?;
        let outside = temp_dir("outside");
        let old = snapshot_turn(&repo, "links", 1)?.unwrap();
        fs::create_dir(root.join("linked"))?;
        fs::write(root.join("linked/victim"), "captured later")?;
        snapshot_turn(&repo, "links", 2)?;
        fs::remove_dir_all(root.join("linked"))?;
        fs::write(outside.join("victim"), "outside must survive")?;
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, root.join("linked"))?;
        #[cfg(windows)]
        {
            let result = Command::new("cmd")
                .args(["/C", "mklink", "/J"])
                .arg(root.join("linked"))
                .arg(&outside)
                .output()?;
            assert!(result.status.success(), "{result:?}");
        }
        restore_from(&repo, "links", &old)?;
        assert_eq!(
            fs::read_to_string(outside.join("victim"))?,
            "outside must survive"
        );
        #[cfg(windows)]
        fs::remove_dir(root.join("linked"))?;
        #[cfg(unix)]
        if root.join("linked").exists() {
            fs::remove_file(root.join("linked"))?;
        }
        cleanup(&root, &repo);
        fs::remove_dir_all(outside)?;
        Ok(())
    }

    #[test]
    fn snapshot_deduplicates_and_restores_a_git_worktree() -> Result<()> {
        let (root, repo) = initialized("restore")?;
        fs::write(root.join("notes.txt"), "first\n")?;
        let first = snapshot_turn(&repo, "session-a", 1)?.expect("first snapshot");
        assert!(snapshot_turn(&repo, "session-a", 2)?.is_none());
        fs::write(root.join("notes.txt"), "second\n")?;
        let second = snapshot_turn(&repo, "session-a", 3)?.expect("second snapshot");
        assert_ne!(first, second);
        fs::write(root.join("new.txt"), "created later\n")?;
        assert_eq!(list_from(&repo, "session-a")?.len(), 2);

        let report = restore_from(&repo, "session-a", &first)?;
        assert_eq!(report.restored, 1);
        assert_eq!(report.removed, vec!["new.txt"]);
        assert!(!root.join("new.txt").exists());
        assert_eq!(fs::read_to_string(root.join("notes.txt"))?, "first\n");
        cleanup(&root, &repo);
        Ok(())
    }

    #[test]
    fn snapshots_continue_turn_numbers_after_agent_restart() -> Result<()> {
        let (root, repo) = initialized("restart-number")?;
        fs::write(root.join("notes.txt"), "before restart\n")?;
        let first = snapshot_turn(&repo, "restart-session", 1)?.unwrap();
        fs::write(root.join("notes.txt"), "after restart\n")?;
        let second = snapshot_turn(&repo, "restart-session", 1)?.unwrap();
        assert_ne!(first, second);
        let subject =
            String::from_utf8_lossy(&repo.git(&["show", "-s", "--format=%s", &second])?.stdout)
                .trim()
                .to_owned();
        assert_eq!(subject, "turn 2");
        cleanup(&root, &repo);
        Ok(())
    }

    #[test]
    fn restore_removes_files_first_captured_by_a_later_snapshot() -> Result<()> {
        let (root, repo) = initialized("later")?;
        fs::write(root.join("base.txt"), "base\n")?;
        let first = snapshot_turn(&repo, "session-a", 1)?.expect("first snapshot");
        fs::write(root.join("later.txt"), "captured later\n")?;
        snapshot_turn(&repo, "session-a", 2)?.expect("second snapshot");
        assert!(root.join("later.txt").is_file());

        let report = restore_from(&repo, "session-a", &first)?;
        assert_eq!(report.removed, vec!["later.txt"]);
        assert!(!root.join("later.txt").exists());
        assert_eq!(fs::read_to_string(root.join("base.txt"))?, "base\n");
        cleanup(&root, &repo);
        Ok(())
    }

    #[test]
    fn restore_preserves_ignored_and_excluded_files() -> Result<()> {
        let (root, repo) = initialized("ignored")?;
        fs::write(root.join(".gitignore"), "*.log\n")?;
        fs::write(root.join("keep.txt"), "old\n")?;
        let first = snapshot_turn(&repo, "session-a", 1)?.expect("first snapshot");

        fs::write(root.join("ignored.log"), "ignored\n")?;
        fs::create_dir_all(root.join("target"))?;
        fs::write(root.join("target/generated.bin"), "build\n")?;
        fs::create_dir_all(root.join("node_modules/pkg"))?;
        fs::write(root.join("node_modules/pkg/index.js"), "dependency\n")?;
        fs::write(root.join("keep.txt"), "changed\n")?;

        let report = restore_from(&repo, "session-a", &first)?;
        assert_eq!(report.restored, 1);
        assert!(report.removed.is_empty(), "{:?}", report.removed);
        assert_eq!(fs::read_to_string(root.join("ignored.log"))?, "ignored\n");
        assert_eq!(
            fs::read_to_string(root.join("target/generated.bin"))?,
            "build\n"
        );
        assert_eq!(
            fs::read_to_string(root.join("node_modules/pkg/index.js"))?,
            "dependency\n"
        );
        assert_eq!(fs::read_to_string(root.join("keep.txt"))?, "old\n");
        cleanup(&root, &repo);
        Ok(())
    }

    #[test]
    fn snapshots_and_restores_leave_the_project_git_metadata_alone() -> Result<()> {
        let (root, repo) = initialized("metadata")?;
        fs::write(root.join("tracked.txt"), "base\n")?;
        let add = Command::new("git")
            .args(["-C", &root.to_string_lossy(), "add", "tracked.txt"])
            .status()?;
        assert!(add.success());
        let index = fs::read(root.join(".git/index"))?;
        let head = fs::read(root.join(".git/HEAD"))?;

        let first = snapshot_turn(&repo, "session-a", 1)?.expect("first snapshot");
        fs::write(root.join("tracked.txt"), "changed\n")?;
        snapshot_turn(&repo, "session-a", 2)?.expect("second snapshot");
        restore_from(&repo, "session-a", &first)?;

        assert_eq!(fs::read(root.join(".git/index"))?, index);
        assert_eq!(fs::read(root.join(".git/HEAD"))?, head);
        cleanup(&root, &repo);
        Ok(())
    }

    #[test]
    fn restore_handles_spaced_and_unicode_filenames() -> Result<()> {
        let (root, repo) = initialized("names")?;
        fs::write(root.join("file with space.txt"), "first\n")?;
        fs::write(root.join("naïve.txt"), "unicode\n")?;
        let first = snapshot_turn(&repo, "session-a", 1)?.expect("first snapshot");

        fs::write(root.join("file with space.txt"), "second\n")?;
        fs::write(root.join("new file.txt"), "created later\n")?;
        let report = restore_from(&repo, "session-a", &first)?;
        assert_eq!(report.removed, vec!["new file.txt"]);
        assert_eq!(
            fs::read_to_string(root.join("file with space.txt"))?,
            "first\n"
        );
        assert_eq!(fs::read_to_string(root.join("naïve.txt"))?, "unicode\n");
        assert!(!root.join("new file.txt").exists());
        cleanup(&root, &repo);
        Ok(())
    }

    #[test]
    fn concurrent_transactions_are_serialized_across_sessions() -> Result<()> {
        let (root, repo) = initialized("concurrent")?;
        fs::write(root.join("base.txt"), "base\n")?;
        let mut workers = Vec::new();
        for worker in 0..6 {
            let root = root.clone();
            let store = repo.store.clone();
            workers.push(std::thread::spawn(move || -> Result<()> {
                for turn in 1..=3 {
                    fs::write(
                        root.join(format!("worker-{worker}.txt")),
                        format!("{worker}-{turn}\n"),
                    )?;
                    run_transaction(&root, &store, None, |repo| {
                        initialized_store(repo)?;
                        snapshot_turn(repo, &format!("session-{worker}"), turn)?;
                        Ok(())
                    })?;
                }
                Ok(())
            }));
        }
        for worker in workers {
            worker.join().expect("checkpoint worker panicked")?;
        }
        for worker in 0..6 {
            let checkpoints = run_transaction(&root, &repo.store, None, |repo| {
                list_from(repo, &format!("session-{worker}"))
            })?;
            assert!(!checkpoints.is_empty(), "session-{worker} lost snapshots");
        }
        cleanup(&root, &repo);
        Ok(())
    }

    // The cancellable path uses Tokio's process reaper, so test it inside a runtime.
    #[tokio::test]
    async fn cancelled_snapshot_stops_before_ref_mutation_and_releases_the_lock() -> Result<()> {
        let (root, repo) = initialized("cancel")?;
        fs::write(root.join("base.txt"), "base\n")?;
        let cancel = Arc::new(AtomicBool::new(false));
        let cancelled = Arc::clone(&cancel);
        let result = run_transaction(&root, &repo.store, Some(cancel), |repo| {
            initialized_store(repo)?;
            cancelled.store(true, Ordering::SeqCst);
            snapshot_turn(repo, "session-cancelled", 1)
        });
        assert!(result.is_err(), "cancelled snapshot must stop");
        assert!(repo
            .git(&[
                "rev-parse",
                "-q",
                "--verify",
                &session_ref("session-cancelled")
            ])
            .is_err());
        // The transaction lock must be released even though the snapshot stopped.
        let hash = run_transaction(&root, &repo.store, None, |repo| {
            snapshot_turn(repo, "session-cancelled", 1)
        })?;
        assert!(hash.is_some());
        cleanup(&root, &repo);
        Ok(())
    }

    #[test]
    fn session_history_can_be_bounded_without_losing_the_latest_snapshot() -> Result<()> {
        let (root, repo) = initialized("history")?;
        fs::write(root.join("notes.txt"), "one\n")?;
        snapshot_turn(&repo, "session-a", 1)?;
        fs::write(root.join("notes.txt"), "two\n")?;
        snapshot_turn(&repo, "session-a", 2)?;
        fs::write(root.join("notes.txt"), "three\n")?;
        let latest = snapshot_turn(&repo, "session-a", 3)?.unwrap();
        let reference = session_ref("session-a");
        let count = |reference: &str| -> usize {
            String::from_utf8_lossy(
                &repo
                    .git(&["rev-list", "--count", reference])
                    .unwrap()
                    .stdout,
            )
            .trim()
            .parse()
            .unwrap()
        };
        assert_eq!(count(&reference), 3);
        let latest_tree = String::from_utf8_lossy(
            &repo
                .git(&["rev-parse", &format!("{latest}^{{tree}}")])?
                .stdout,
        )
        .trim()
        .to_owned();

        prune_history_to(&repo, &reference, 2, 1)?;
        assert_eq!(count(&reference), 1);
        assert_eq!(
            String::from_utf8_lossy(
                &repo
                    .git(&["rev-parse", &format!("{reference}^{{tree}}")])?
                    .stdout
            )
            .trim(),
            latest_tree
        );
        cleanup(&root, &repo);
        Ok(())
    }
}
