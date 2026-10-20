//! M1-04: one job, one place to work, one sealed result to score.
//!
//! Three properties that are easy to claim and easy to lose:
//!
//! * **A job owns its directory.** `JobScope::open` creates a directory named after the job and
//!   refuses to reuse one, so two jobs cannot share scratch space by accident.
//! * **Cancellation reaps what it started.** Every child a job runs is recorded in its
//!   [`ChildRegistry`] — both the ones spawned through [`JobScope::spawn`] and the ones a launch
//!   path spawns while working for the job (the bench `verify_command` goes through
//!   [`crate::process_supervisor`] and registers itself). `cancel` refuses new spawns, terminates
//!   the recorded groups and then *checks* that the pids are gone rather than assuming a signal was
//!   enough; whatever survives is named as [`ScopeError::StillAlive`].
//! * **Scoring sees sealed bytes.** [`seal_workspace`] digests the files the oracle reads, copies
//!   the tree out of the job's workspace, and re-hashes the copy against those digests, so what is
//!   scored is the captured bytes and not whatever the job wrote afterwards. `ScoreInput` only
//!   accepts a `SealedArtifact` and re-hashes it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::process_group::ChildProcessGroup;

#[derive(Debug, PartialEq, Eq)]
pub enum ScopeError {
    /// The job already has a directory (or the seal's destination name is taken): isolation per job
    /// is not negotiable.
    AlreadyExists {
        job_id: String,
    },
    /// The scope was cancelled; nothing new may start.
    Cancelled,
    /// A recorded child is still alive after being asked to stop.
    StillAlive {
        pids: Vec<u32>,
    },
    /// The bytes no longer match the sealed digest.
    Tampered,
    Io(String),
}

/// How long [`ChildRegistry::reclaim`] waits for a signalled group to disappear before reporting
/// what is left as alive.
const RECLAIM_POLL_BUDGET: Duration = Duration::from_millis(250);

/// Poll step inside the reclaim budget.
const RECLAIM_POLL_STEP: Duration = Duration::from_millis(5);

/// One child a job started, identified by leader pid *and* the group it was placed in.
///
/// `started` is the kernel start time read from `/proc/<pid>/stat` at registration; it is what keeps
/// a recycled pid from being mistaken for the child we launched (see [`Self::holds_pid`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RegisteredChild {
    pid: u32,
    pgid: i32,
    started: Option<u64>,
}

impl RegisteredChild {
    /// The pid still belongs to the process registered here: `/proc/<pid>` exists and, when a start
    /// time was readable at registration, it matches. A pid recycled by an unrelated process is
    /// therefore *not* ours.
    ///
    /// A zombie also holds its pid. That is deliberate: while the pid is held no other process can
    /// lead a group with that id, so signalling the recorded group cannot reach an unrelated one.
    fn holds_pid(&self) -> bool {
        matches!(proc_stat(self.pid), Some(stat) if self.matches(&stat))
    }

    /// Still *executing*: the pid is ours and it is not a zombie.
    ///
    /// A zombie is a stopped process whose parent has not waited yet — it cannot write, talk or
    /// hold a pipe, so it is not a child that survived. Recorded limit: on a host without `/proc`
    /// every probe answers `false`, so reaping is the caller's to verify there.
    fn is_running(&self) -> bool {
        matches!(proc_stat(self.pid), Some(stat) if self.matches(&stat) && stat.state != 'Z')
    }

    /// Whether the kernel start time still matches the one recorded at registration.
    fn matches(&self, stat: &ProcStat) -> bool {
        match self.started {
            Some(recorded) => recorded == stat.started,
            // Nothing to compare against: report "ours" rather than assume a live child died.
            None => true,
        }
    }
}

/// The children one job (one bench arm) started, shared by the scope and by the launch paths that
/// actually spawn them.
///
/// The handle is an `Arc`, so a spawn site deep in the supervisor can record its child without
/// owning the job, and the job keeps one list instead of asking each launch path what it started.
/// It is in-memory by design: the registry is reaping state, not evidence, so it is never persisted
/// and never reaches a prompt or a report.
#[derive(Debug, Clone, Default)]
pub struct ChildRegistry {
    entries: Arc<Mutex<BTreeMap<u32, RegisteredChild>>>,
}

impl ChildRegistry {
    /// Record a running child: `pgid` is the group its launch placed it in, and is what reclaim
    /// signals.
    pub fn register(&self, pid: u32, pgid: i32) {
        self.entries().insert(
            pid,
            RegisteredChild {
                pid,
                pgid,
                started: proc_start_time(pid),
            },
        );
    }

    /// Stop tracking `pid` (the child was reaped by its owner).
    pub fn deregister(&self, pid: u32) {
        self.entries().remove(&pid);
    }

    /// The registered pids that are still running.
    pub fn alive(&self) -> Vec<u32> {
        self.entries()
            .values()
            .filter(|child| child.is_running())
            .map(|child| child.pid)
            .collect()
    }

    /// Terminate every registered child that still holds its pid, then check instead of assuming.
    ///
    /// Each one is signalled through the same [`ChildProcessGroup`] sweep the launch paths use
    /// (`kill(-pgid, SIGKILL)`), polled for up to [`RECLAIM_POLL_BUDGET`] and then re-checked.
    /// Children that stopped are dropped from the registry; a child still *running* stays
    /// registered *and* is returned as [`ScopeError::StillAlive`], so a caller can record "no
    /// verdict" instead of a clean rejection.
    ///
    /// Signalling is limited to entries that provably still hold their pid, so a recycled pid's
    /// group can never be hit. The blocking poll is bounded, and both callers (the arm's
    /// end-of-verify reclaim and `JobScope::cancel`) have nothing else to do while a child dies.
    pub fn reclaim(&self) -> Result<(), ScopeError> {
        let doomed: Vec<RegisteredChild> = self
            .entries()
            .values()
            .copied()
            .filter(|child| child.holds_pid())
            .collect();
        for child in &doomed {
            ChildProcessGroup::from_pgid(child.pgid).terminate_immediately();
        }
        let deadline = Instant::now() + RECLAIM_POLL_BUDGET;
        loop {
            let mut entries = self.entries();
            entries.retain(|_, child| child.is_running());
            let alive: Vec<u32> = entries.keys().copied().collect();
            drop(entries);
            if alive.is_empty() {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(ScopeError::StillAlive { pids: alive });
            }
            std::thread::sleep(RECLAIM_POLL_STEP);
        }
    }

    fn entries(&self) -> MutexGuard<'_, BTreeMap<u32, RegisteredChild>> {
        // A panic while the registry lock was held cannot corrupt a map of pids: keep reaping
        // rather than propagating the panic into a cancel path.
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// One job's working area and the children it started.
pub struct JobScope {
    job_id: String,
    root: PathBuf,
    children: Vec<Child>,
    registry: ChildRegistry,
    cancelled: bool,
    keep: bool,
}

impl JobScope {
    pub fn open(root: impl AsRef<Path>, job_id: &str) -> Result<Self, ScopeError> {
        let root = root.as_ref().join(job_id);
        if root.exists() {
            return Err(ScopeError::AlreadyExists {
                job_id: job_id.to_string(),
            });
        }
        std::fs::create_dir_all(&root).map_err(|error| ScopeError::Io(error.to_string()))?;
        Ok(Self {
            job_id: job_id.to_string(),
            root,
            children: Vec::new(),
            registry: ChildRegistry::default(),
            cancelled: false,
            keep: false,
        })
    }

    pub fn path(&self) -> &Path {
        &self.root
    }

    pub fn job_id(&self) -> &str {
        &self.job_id
    }

    /// The job's [`ChildRegistry`], for launch paths that spawn outside the scope.
    pub fn child_registry(&self) -> &ChildRegistry {
        &self.registry
    }

    /// Hand the directory to the caller instead of deleting it on drop.
    ///
    /// The bench keeps each arm's tree on disk: it is the run's evidence, and the report's paths
    /// already point at it. Everything else about the scope (exclusive ownership, child reaping)
    /// still holds while it lives.
    pub fn keep(mut self) {
        self.keep = true;
    }

    /// Start a child inside this job. Refused once the scope is cancelled.
    ///
    /// The child is put in its own process group through the shared containment seam and recorded
    /// with that group, so cancellation can signal the tree and not just the leader.
    pub fn spawn(&mut self, mut command: Command) -> Result<u32, ScopeError> {
        if self.cancelled {
            return Err(ScopeError::Cancelled);
        }
        ChildProcessGroup::configure_std_command(&mut command);
        let child = command
            .current_dir(&self.root)
            .spawn()
            .map_err(|error| ScopeError::Io(error.to_string()))?;
        let pid = child.id();
        self.registry.register(pid, pid as i32);
        self.children.push(child);
        Ok(pid)
    }

    /// Stop accepting work, terminate what the job started, and verify it is gone.
    ///
    /// Two kinds of child are in one registry: the ones this scope spawned, and the ones a launch
    /// path spawned while working for the job. This scope's own children are killed *and waited* so
    /// they are reaped, not left as zombies; the registry then terminates the rest through their
    /// recorded process groups and checks the pids instead of assuming a signal was enough.
    pub fn cancel(&mut self) -> Result<(), ScopeError> {
        self.cancelled = true;
        for child in &mut self.children {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.children.clear();
        self.registry.reclaim()
    }
}

impl Drop for JobScope {
    fn drop(&mut self) {
        for child in &mut self.children {
            let _ = child.kill();
            let _ = child.wait();
        }
        if !self.keep {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }
}

/// A frozen copy of a job's workspace: the tree scoring is allowed to read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SealedWorkspace {
    root: PathBuf,
}

impl SealedWorkspace {
    pub fn path(&self) -> &Path {
        &self.root
    }
}

/// Freeze `workspace` into `dest` and return the copy that may be scored.
///
/// The digest of every path the oracle reads is taken from the live workspace *first*, then the
/// tree is copied, then each copy is read back through [`ScoreInput`] against that digest. That
/// order is the whole point: verifying a copy against a digest taken before it proves the copy
/// holds the sealed bytes instead of merely being self-consistent, and a writer that raced the copy
/// is reported as [`ScopeError::Tampered`] rather than judged.
///
/// `dest` must not exist. Symlinks and special files are not copied: the oracle refuses them, and
/// following one would score bytes from outside the workspace.
pub fn seal_workspace(
    workspace: &Path,
    dest: &Path,
    scored: &[String],
) -> Result<SealedWorkspace, ScopeError> {
    let job_id = workspace
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let digests = sealed_digests(workspace, scored)?;
    copy_workspace(workspace, dest)?;
    for (path, sha256) in digests {
        let bytes = match std::fs::read(dest.join(&path)) {
            Ok(bytes) => bytes,
            // Absent at the seal and absent in the copy: the oracle reports the missing file.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(ScopeError::Io(error.to_string())),
        };
        ScoreInput::new(SealedArtifact {
            job_id: job_id.clone(),
            sha256,
            bytes,
        })?;
    }
    Ok(SealedWorkspace {
        root: dest.to_path_buf(),
    })
}

/// sha256 of every regular file the oracle reads, taken before the copy is made.
fn sealed_digests(
    workspace: &Path,
    scored: &[String],
) -> Result<Vec<(String, String)>, ScopeError> {
    let mut digests = Vec::with_capacity(scored.len());
    for path in scored {
        let full = workspace.join(path);
        match std::fs::symlink_metadata(&full) {
            Ok(meta) if meta.is_file() => {
                let bytes = std::fs::read(&full).map_err(|e| ScopeError::Io(e.to_string()))?;
                digests.push((path.clone(), sha256_hex(&bytes)));
            }
            // A symlink or a special file is not sealed bytes, and a missing file is the oracle's
            // business rather than the seal's.
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(ScopeError::Io(error.to_string())),
        }
    }
    Ok(digests)
}

fn copy_workspace(from: &Path, to: &Path) -> Result<(), ScopeError> {
    if to.exists() {
        return Err(ScopeError::AlreadyExists {
            job_id: to.display().to_string(),
        });
    }
    std::fs::create_dir_all(to).map_err(|error| ScopeError::Io(error.to_string()))?;
    copy_entries(from, to)
}

fn copy_entries(from: &Path, to: &Path) -> Result<(), ScopeError> {
    let entries = std::fs::read_dir(from).map_err(|error| ScopeError::Io(error.to_string()))?;
    for entry in entries {
        let entry = entry.map_err(|error| ScopeError::Io(error.to_string()))?;
        let file_type = entry
            .file_type()
            .map_err(|error| ScopeError::Io(error.to_string()))?;
        let target = to.join(entry.file_name());
        if file_type.is_dir() {
            std::fs::create_dir_all(&target).map_err(|error| ScopeError::Io(error.to_string()))?;
            copy_entries(&entry.path(), &target)?;
        } else if file_type.is_file() {
            std::fs::copy(entry.path(), &target)
                .map_err(|error| ScopeError::Io(error.to_string()))?;
        }
    }
    Ok(())
}

/// Kernel state and start time of `pid`, read from `/proc/<pid>/stat`.
struct ProcStat {
    /// One of the `/proc` state letters (`R`, `S`, `D`, `Z`, ...).
    state: char,
    /// Field 22: start time in clock ticks since boot.
    started: u64,
}

/// Read `/proc/<pid>/stat`; `None` when the process is gone or `/proc` is unreadable.
///
/// The `comm` field is parenthesised and may itself contain spaces, `)` or newlines, so the fields
/// are split after the *last* `)` — field 3 (state) follows it, and `starttime` is index 19 there.
fn proc_stat(pid: u32) -> Option<ProcStat> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let mut fields = stat.rsplit_once(')')?.1.split_whitespace();
    let state = fields.next()?.chars().next()?;
    let started = fields.nth(18)?.parse().ok()?;
    Some(ProcStat { state, started })
}

/// Kernel start time of `pid`, recorded at registration so a recycled pid is not mistaken for ours.
fn proc_start_time(pid: u32) -> Option<u64> {
    proc_stat(pid).map(|stat| stat.started)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SealedArtifact {
    pub job_id: String,
    pub sha256: String,
    pub bytes: Vec<u8>,
}

/// The only thing a scorer accepts. There is deliberately no `From<String>`: a scorer that can be
/// handed unsealed bytes will eventually be handed unsealed bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScoreInput(SealedArtifact);

impl ScoreInput {
    /// Re-hash the material against its recorded digest before scoring it.
    pub fn new(artifact: SealedArtifact) -> Result<Self, ScopeError> {
        if sha256_hex(&artifact.bytes) != artifact.sha256 {
            return Err(ScopeError::Tampered);
        }
        Ok(ScoreInput(artifact))
    }

    pub fn job_id(&self) -> &str {
        &self.0.job_id
    }

    pub fn bytes(&self) -> &[u8] {
        &self.0.bytes
    }

    /// Score the sealed bytes. Kept trivial on purpose: the interesting decision is what is
    /// allowed in, not what the scorer computes.
    pub fn score(&self) -> usize {
        self.0.bytes.len()
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    crate::prompt::sha256_hex64(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("kroma-m104-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// M1-04-A1: each job gets its own directory, and an existing one is never reused.
    #[test]
    fn jobs_do_not_share_a_working_directory() {
        let root = scratch("isolation");
        let a = JobScope::open(&root, "job-a").unwrap();
        let b = JobScope::open(&root, "job-b").unwrap();
        assert_ne!(a.path(), b.path());
        assert!(a.path().ends_with("job-a") && b.path().ends_with("job-b"));

        assert_eq!(
            JobScope::open(&root, "job-a").err(),
            Some(ScopeError::AlreadyExists {
                job_id: "job-a".to_string()
            }),
            "a job directory is owned by exactly one job"
        );
        let a_path = a.path().to_path_buf();
        drop(a);
        assert!(!a_path.exists(), "dropping the scope removes its directory");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// M1-04-A2: cancellation refuses new work and reaps what the job started.
    #[test]
    fn cancellation_refuses_new_work_and_reaps_children() {
        let root = scratch("cancel");
        let mut scope = JobScope::open(&root, "job-cancel").unwrap();

        let mut first = Command::new("sleep");
        first.arg("30");
        let pid = scope.spawn(first).unwrap();

        scope.cancel().unwrap();
        assert!(
            !Path::new(&format!("/proc/{pid}")).exists(),
            "the child must be gone after cancel, not merely signalled"
        );

        assert_eq!(
            scope.spawn(Command::new("true")).err(),
            Some(ScopeError::Cancelled),
            "a cancelled job starts nothing new"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A recycled pid is not our child: the start time recorded at registration decides.
    #[test]
    fn a_recycled_pid_is_not_mistaken_for_a_recorded_child() {
        let registry = ChildRegistry::default();
        let me = std::process::id();
        registry.register(me, me as i32);
        assert_eq!(
            registry.alive(),
            vec![me],
            "our own process is provably alive"
        );

        // Same pid, a start time that cannot be ours (the kernel never reports 0).
        registry.entries().insert(
            me,
            RegisteredChild {
                pid: me,
                pgid: 0,
                started: Some(0),
            },
        );
        assert!(
            registry.alive().is_empty(),
            "a pid whose start time differs is a recycled pid, not a child of this job"
        );
        registry.deregister(me);
        assert!(registry.alive().is_empty());
    }

    /// Reclaim drops a group it killed, and never reports a reaped child as alive.
    #[test]
    fn reclaim_terminates_and_forgets_a_recorded_group() {
        let root = scratch("reclaim");
        let mut scope = JobScope::open(&root, "job-reclaim").unwrap();
        let mut sleeper = Command::new("sleep");
        sleeper.arg("30");
        let pid = scope.spawn(sleeper).unwrap();
        assert_eq!(scope.child_registry().alive(), vec![pid]);

        scope.child_registry().reclaim().unwrap();
        assert!(scope.child_registry().alive().is_empty());
        assert!(scope.cancel().is_ok(), "nothing is left to report");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The registry is what `cancel` reports on: a child the group sweep cannot reach is named,
    /// not assumed gone.
    #[test]
    fn cancel_names_a_child_that_cannot_be_reclaimed() {
        let root = scratch("cancel-foreign");
        let mut scope = JobScope::open(&root, "job-foreign").unwrap();
        // A child of *this* process, so its pid is not a group id: `kill(-pid)` finds no group and
        // the survivor stands in for a child whose reaping failed.
        let mut orphan = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let pid = orphan.id();
        scope.child_registry().register(pid, pid as i32);

        match scope.cancel() {
            Err(ScopeError::StillAlive { pids }) => {
                assert_eq!(pids, vec![pid], "the survivor is named")
            }
            other => panic!("expected StillAlive, got {:?}", other),
        }
        let _ = orphan.kill();
        let _ = orphan.wait();
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The bench keeps each arm's directory as the run's evidence: `keep` hands it back instead of
    /// deleting it on drop.
    #[test]
    fn a_kept_workspace_outlives_its_scope() {
        let root = scratch("keep");
        let scope = JobScope::open(&root, "job-keep").unwrap();
        let path = scope.path().to_path_buf();
        std::fs::write(path.join("src.rs"), b"fn f() {}\n").unwrap();

        scope.keep();

        assert!(
            path.join("src.rs").exists(),
            "a kept workspace is not deleted with its scope"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// M1-04-A3: scoring reads the sealed snapshot, and refuses bytes that changed after sealing.
    #[test]
    fn scoring_only_sees_sealed_bytes_and_detects_tampering() {
        let root = scratch("seal");
        let scope = JobScope::open(&root, "job-seal").unwrap();
        std::fs::write(scope.path().join("report.json"), b"{\"jobs\": 24}").unwrap();
        let scored = vec!["report.json".to_string()];

        // The sealed copy is not inside the job's workspace: the job has no path to it.
        let sealed = seal_workspace(scope.path(), &root.join("job-seal__sealed"), &scored).unwrap();
        assert!(!sealed.path().starts_with(scope.path()));
        assert_eq!(
            std::fs::read(sealed.path().join("report.json")).unwrap(),
            b"{\"jobs\": 24}"
        );

        // A candidate that keeps writing after the seal changes its own workspace...
        std::fs::write(scope.path().join("report.json"), b"{\"jobs\": 999}").unwrap();

        // ...and the judged bytes are the sealed ones.
        let input = ScoreInput::new(SealedArtifact {
            job_id: "job-seal".to_string(),
            sha256: sha256_hex(b"{\"jobs\": 24}"),
            bytes: std::fs::read(sealed.path().join("report.json")).unwrap(),
        })
        .expect("the sealed bytes are scorable");
        assert_eq!(input.job_id(), "job-seal");
        assert_eq!(input.score(), 12);

        // A doctored artifact is refused rather than scored.
        let doctored = SealedArtifact {
            job_id: "job-seal".to_string(),
            sha256: sha256_hex(b"{\"jobs\": 24}"),
            bytes: b"{\"jobs\": 999}".to_vec(),
        };
        assert_eq!(ScoreInput::new(doctored).err(), Some(ScopeError::Tampered));

        let _ = std::fs::remove_dir_all(&root);
    }
}
