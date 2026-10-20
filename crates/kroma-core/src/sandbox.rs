//! B13: OS-level sandboxing for verification and scoring runs.
//!
//! The sandbox is an *argv translation layer*: it turns a host command into the argv that the
//! shared [`ProcessSupervisor`](crate::process_supervisor::ProcessSupervisor) launches, so every
//! sandboxed run inherits the supervisor's absolute deadlines, output flood caps, cancellation and
//! process-group cleanup. Two backends are supported:
//!
//! * [`SandboxBackend::Bwrap`] — bubblewrap, available offline. It mounts a **whitelist** view of
//!   the host toolchain read-only (`/usr`, `/bin`, `/sbin`, `/lib`, `/lib64`, plus a short list of
//!   `/etc` entries the loader and Python need), makes exactly one path writable (the workspace),
//!   gives `/tmp` a size-capped tmpfs, and unshares every namespace — network included unless the
//!   spec opts in. Everything not on the whitelist (the host `HOME`, `/root`, `/var`,
//!   `/etc/shadow`, `/etc/ssh`, ...) simply does not exist inside the sandbox. The guest
//!   *environment* is a whitelist too: `--clearenv` plus an explicit `HOME`/`TMPDIR`/`PATH`, so a
//!   host variable that the credential blacklist does not name cannot reach the untrusted program.
//! * [`SandboxBackend::Container`] — a container runtime (`docker` / `podman`) with an explicit
//!   image, for deployments that run the scorer in a container. Network is `none` by default,
//!   the root filesystem is read-only, and memory / pid / CPU caps are passed to the runtime.
//!
//! **Fail-closed, no fallback.** [`SandboxSpec::probe`] reports whether the requested backend is
//! actually usable, and the callers must refuse to run when it is not. There is deliberately no
//! code path that silently degrades to an unsandboxed host run.
//!
//! What this is *not*: an OS sandbox is not provided by path checks like `openat2`; the isolation
//! here comes from the kernel namespaces bwrap/containers create. Resource caps for the bwrap
//! backend come from an outer cgroup (bwrap itself has no memory/CPU limits) — the container
//! backend passes its caps to the runtime directly.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::process_launch::ResourceLimits;
use crate::process_supervisor::{OneShotCommandSpec, OneShotExecutionResult, ProcessSupervisor};

/// Where the workspace is mounted inside the sandbox by default.
pub const DEFAULT_GUEST_WORKSPACE: &str = "/workspace";
/// Default size cap for the sandbox `/tmp` (256 MiB).
pub const DEFAULT_TMPFS_BYTES: u64 = 256 * 1024 * 1024;
/// The `PATH` the guest sees. It names only directories the sandbox mounts itself, so it can be
/// fixed instead of inherited: a guest must not be able to read the host's layout out of `PATH`,
/// and its behaviour must not depend on where kroma happens to run. A program reachable only
/// through a [`SandboxSpec::with_ro_extra`] mount must therefore be invoked by absolute path.
pub const DEFAULT_GUEST_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
/// Default memory cap handed to a container runtime (512 MiB).
pub const DEFAULT_CONTAINER_MEMORY_BYTES: u64 = 512 * 1024 * 1024;
/// Default pid cap handed to a container runtime.
pub const DEFAULT_CONTAINER_PIDS: u64 = 128;

/// Read-only host directories every bwrap sandbox exposes. This is a whitelist: a path that is not
/// here (nor in [`BWRAP_RO_ETC_ENTRIES`] or [`SandboxSpec::ro_extra`]) is absent inside the sandbox.
pub const BWRAP_RO_HOST_DIRS: &[&str] = &["/usr", "/bin", "/sbin", "/lib", "/lib64"];

/// The `/etc` entries a toolchain needs at runtime. The whole `/etc` is deliberately not mounted:
/// it carries host secrets (`/etc/shadow`, `/etc/ssh`, ...) that a scored run must never see.
/// Missing entries are skipped (`--ro-bind-try`), so this list can name optional files.
pub const BWRAP_RO_ETC_ENTRIES: &[&str] = &[
    "ld.so.cache",
    "ld.so.conf",
    "ld.so.conf.d",
    "passwd",
    "group",
    "nsswitch.conf",
    "localtime",
    "alternatives",
    "python3",
    "python3.13",
    "ssl/openssl.cnf",
];

/// Which isolation mechanism to use. The choice is explicit; there is no automatic fallback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxBackend {
    /// bubblewrap. Offline-capable; namespace isolation with a whitelisted read-only host view.
    Bwrap,
    /// A container runtime plus the image to run. The image should be pinned by digest in
    /// production; the spec does not require a digest so local images remain usable in tests.
    Container { runtime: String, image: String },
}

/// Why a sandbox could not be used. Every variant is fail-closed: the caller must not run the
/// command on the host instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxError {
    /// `bwrap` is not installed / not on PATH.
    BwrapMissing,
    /// The container runtime or the image is not usable.
    ContainerUnavailable(String),
    /// The spec itself is invalid (empty runtime/image, missing workspace, ...).
    InvalidSpec(String),
    /// T10 (KR-16): the backend answered `--version` but could not actually isolate (namespaces,
    /// cgroups or the image's own runtime are unusable). Reported separately from "missing" because
    /// the operator action is different, and both are fail-closed.
    NamespaceUnavailable(String),
}

impl std::fmt::Display for SandboxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SandboxError::BwrapMissing => write!(
                f,
                "sandbox backend bwrap is not available; refusing to run unsandboxed"
            ),
            SandboxError::ContainerUnavailable(msg) => write!(
                f,
                "sandbox container backend is unavailable ({}); refusing to run unsandboxed",
                msg
            ),
            SandboxError::InvalidSpec(msg) => write!(f, "invalid sandbox spec: {}", msg),
            SandboxError::NamespaceUnavailable(msg) => write!(
                f,
                "sandbox backend is installed but cannot isolate ({}); refusing to run unsandboxed",
                msg
            ),
        }
    }
}

impl std::error::Error for SandboxError {}

/// Successful probes, keyed by backend + image. Failures are never cached: a transient daemon
/// problem must not permanently disable the run.
fn probe_success_cache() -> &'static std::sync::Mutex<std::collections::BTreeSet<String>> {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<std::collections::BTreeSet<String>>> =
        std::sync::OnceLock::new();
    CACHE.get_or_init(|| std::sync::Mutex::new(std::collections::BTreeSet::new()))
}

/// T10 (KR-16): outcome of one bounded CLI invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliOutcome {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
    /// KRV4-02: the retained output hit the capture ceiling. Reported rather than silent, because a
    /// caller that parses a truncated probe result would otherwise believe it saw everything.
    pub truncated: bool,
}

/// T10: the wall-clock budget for a probe or a container control command.
///
/// A probe that cannot answer within this budget is treated as *unavailable*, never as success: a
/// hung `docker image inspect` must not turn into a run that waits forever, and it must not turn
/// into "assume it works, run on the host".
pub const DEFAULT_CONTROL_BUDGET: Duration = Duration::from_secs(15);

/// Run one control command with a hard wall-clock budget.
///
/// This is deliberately not `Command::status()`: that blocks forever when the runtime's daemon
/// hangs. The child is spawned, polled, and killed once the budget is spent, so the caller always
/// gets an answer.
pub fn run_bounded(program: &str, args: &[String], budget: Duration) -> Result<CliOutcome, String> {
    run_bounded_with_containment(
        program,
        args,
        budget,
        crate::process_group::Containment::Platform,
    )
}

/// Bounded command execution with a testable containment-attachment failure seam. The failure
/// path never proceeds with an uncontained child (including on Windows).
#[doc(hidden)]
pub fn run_bounded_with_containment(
    program: &str,
    args: &[String],
    budget: Duration,
    containment: crate::process_group::Containment,
) -> Result<CliOutcome, String> {
    /// Retained output per stream. Larger than any probe or container-control command produces;
    /// everything past it is drained and dropped so the child is never blocked by a full pipe.
    const CAPTURE_CAP: usize = 64 * 1024;
    /// How long to wait for the pipes to close once the child is gone. A grandchild holding the fd
    /// must not be able to extend the budget the caller asked for.
    const DRAIN_GRACE: Duration = Duration::from_millis(150);

    let mut command = std::process::Command::new(program);
    command
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    // The child leads its own process group, so a timeout can clear the whole tree instead of
    // leaving grandchildren holding the pipes.
    crate::process_group::ChildProcessGroup::configure_std_command(&mut command);
    let mut child = command
        .spawn()
        .map_err(|e| format!("cannot spawn {}: {}", program, e))?;
    let group =
        crate::process_group::ChildProcessGroup::try_from_std_child(&mut child, containment)
            .map_err(|e| format!("cannot contain {}: {}", program, e))?;

    // Both pipes are drained *while* the child runs. Waiting for exit first deadlocks as soon as the
    // child writes more than a pipe buffer: the child blocks on write, the parent blocks on wait, and
    // the deadline then blames a command that was working.
    let stdout = child
        .stdout
        .take()
        .map(|pipe| spawn_bounded_drain(pipe, CAPTURE_CAP));
    let stderr = child
        .stderr
        .take()
        .map(|pipe| spawn_bounded_drain(pipe, CAPTURE_CAP));

    let deadline = std::time::Instant::now() + budget;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(e) => {
                group.terminate_immediately();
                let _ = child.kill();
                return Err(format!("cannot poll {}: {}", program, e));
            }
        }
        if std::time::Instant::now() >= deadline {
            group.terminate_immediately();
            // On Windows an additional leader kill is essential: group attach failures and
            // process-tree escapes must never turn a timeout into an unbounded Child::wait().
            let _ = child.kill();
            reap_bounded(&mut child, Duration::from_millis(150));
            return Err(format!(
                "{} did not finish within {}ms and was killed",
                program,
                budget.as_millis()
            ));
        }
        std::thread::sleep(Duration::from_millis(10));
    };

    // A SINGLE absolute drain deadline across *both* pipes. The cleanup allowance is explicit
    // and shared; waiting for stderr must never restart the stdout timeout.
    let drain_deadline = deadline + DRAIN_GRACE;
    let (stdout, stdout_truncated) = collect_drain(
        stdout,
        drain_deadline.saturating_duration_since(std::time::Instant::now()),
    );
    let (stderr, stderr_truncated) = collect_drain(
        stderr,
        drain_deadline.saturating_duration_since(std::time::Instant::now()),
    );
    if stdout_truncated || stderr_truncated {
        // A descendant still holds the pipe; clear the tree so the next probe starts clean.
        group.terminate_immediately();
    } else {
        // R13: a clean leader exit is not a cleaned process tree. A control command that starts a
        // background grandchild (`sleep 30 &`) and exits 0 leaves nothing on the pipes, so the old
        // `truncated`-only sweep never ran and the descendant survived the call. Sweep the group
        // after *every* normal exit, exactly like the asynchronous supervisor's
        // `cleanup_after_exit`; a group that is already empty is left alone.
        group.sweep_alive_sync(DRAIN_GRACE);
    }

    Ok(CliOutcome {
        success: status.success(),
        stdout,
        stderr,
        truncated: stdout_truncated || stderr_truncated,
    })
}

/// Reap a killed leader without ever waiting indefinitely for process termination.
fn reap_bounded(child: &mut std::process::Child, grace: Duration) {
    let deadline = std::time::Instant::now() + grace;
    loop {
        if !matches!(child.try_wait(), Ok(None)) {
            return;
        }
        if std::time::Instant::now() >= deadline {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Read one pipe on its own thread, keeping at most `cap` bytes and draining the rest.
///
/// Draining past the cap matters as much as keeping it: a reader that stops at the cap lets the pipe
/// fill and the child block, which is the deadlock this function exists to prevent.
fn spawn_bounded_drain<R: std::io::Read + Send + 'static>(
    mut pipe: R,
    cap: usize,
) -> std::sync::mpsc::Receiver<(String, bool)> {
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut kept: Vec<u8> = Vec::with_capacity(cap.min(8 * 1024));
        let mut truncated = false;
        let mut buffer = [0u8; 8 * 1024];
        loop {
            match pipe.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => {
                    let room = cap.saturating_sub(kept.len());
                    if room == 0 {
                        truncated = true;
                    } else {
                        let take = room.min(read);
                        kept.extend_from_slice(&buffer[..take]);
                        if take < read {
                            truncated = true;
                        }
                    }
                }
                Err(_) => break,
            }
        }
        let _ = sender.send((String::from_utf8_lossy(&kept).into_owned(), truncated));
    });
    receiver
}

/// Wait at most `budget` for a drain thread to finish. Returns what was retained, or nothing if the
/// pipe is still held by something outside the process group.
fn collect_drain(
    receiver: Option<std::sync::mpsc::Receiver<(String, bool)>>,
    budget: Duration,
) -> (String, bool) {
    match receiver {
        None => (String::new(), false),
        Some(receiver) => match receiver.recv_timeout(budget) {
            Ok((text, truncated)) => (text, truncated),
            Err(_) => (String::new(), true),
        },
    }
}

/// T10 (KR-16): the transport a probe uses. Injectable so a hanging runtime can be tested without
/// installing one — the failure mode is "the daemon does not answer", not "the binary is missing".
pub trait ProbeTransport: Send + Sync {
    fn run(&self, program: &str, args: &[String], budget: Duration) -> Result<CliOutcome, String>;
}

/// The real transport: a bounded child process.
pub struct ProcessProbeTransport;

impl ProbeTransport for ProcessProbeTransport {
    fn run(&self, program: &str, args: &[String], budget: Duration) -> Result<CliOutcome, String> {
        run_bounded(program, args, budget)
    }
}

/// T10 (KR-17): the observable states of one container workload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkloadState {
    Created,
    Started,
    Stopped,
    Removed,
}

impl WorkloadState {
    pub fn as_str(self) -> &'static str {
        match self {
            WorkloadState::Created => "created",
            WorkloadState::Started => "started",
            WorkloadState::Stopped => "stopped",
            WorkloadState::Removed => "removed",
        }
    }
}

/// T10 (KR-17): what cleanup actually achieved.
///
/// There is no `cleaned: bool` that can be set optimistically: a cleanup that could not confirm the
/// container is gone says so, keeps the id, and tells the operator how to retry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CleanupOutcome {
    /// The workload never created a container, so there is nothing to clean.
    NothingToClean,
    /// `inspect` no longer finds the container.
    Cleaned { container_id: Option<String> },
    /// Cleanup did not complete. The container id is retained so a retry is possible, and
    /// `retry_command` is the exact command an operator can run.
    RecoveryRequired {
        container_id: Option<String>,
        reason: String,
        retry_command: String,
    },
}

impl CleanupOutcome {
    pub fn is_cleaned(&self) -> bool {
        matches!(
            self,
            CleanupOutcome::Cleaned { .. } | CleanupOutcome::NothingToClean
        )
    }
}

/// T10 (KR-16/17): one container workload, addressed by a unique name and tracked by a cidfile in
/// the trusted state area.
///
/// The lifecycle is explicit — `create` → `start` → `cancel` (stop, remove, verify) — instead of a
/// foreground `run --rm`, because a foreground client is not the workload: killing it (or the
/// process that spawned it) leaves the container running in the daemon with no record of its id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerWorkload {
    runtime: String,
    image: String,
    name: String,
    cidfile: PathBuf,
}

impl ContainerWorkload {
    /// Build a workload with a unique name and a cidfile inside `state_dir` (the trusted run state
    /// area, never the workspace the model can write).
    pub fn new(
        runtime: impl Into<String>,
        image: impl Into<String>,
        name_prefix: &str,
        state_dir: &Path,
    ) -> Result<Self, SandboxError> {
        let runtime = runtime.into();
        let image = image.into();
        if runtime.trim().is_empty() || image.trim().is_empty() {
            return Err(SandboxError::InvalidSpec(
                "container runtime and image must be non-empty".to_string(),
            ));
        }
        std::fs::create_dir_all(state_dir).map_err(|e| {
            SandboxError::InvalidSpec(format!(
                "cannot create the container state directory {}: {}",
                state_dir.display(),
                e
            ))
        })?;
        let unique = format!(
            "{}-{}-{}",
            name_prefix,
            std::process::id(),
            &crate::prompt::sha256_hex64(uuid::Uuid::new_v4().as_bytes())[..12]
        );
        let name = unique.replace(
            |c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "-",
        );
        let cidfile = state_dir.join(format!("{}.cid", name));
        Ok(Self {
            runtime,
            image,
            name,
            cidfile,
        })
    }

    /// Adopt a workload that a previous process recorded (crash recovery). The name must come from
    /// the trusted state area, never from model output.
    pub fn adopt(
        runtime: impl Into<String>,
        image: impl Into<String>,
        name: impl Into<String>,
        state_dir: &Path,
    ) -> Result<Self, SandboxError> {
        let runtime = runtime.into();
        let image = image.into();
        let name = name.into();
        if runtime.trim().is_empty() || name.trim().is_empty() {
            return Err(SandboxError::InvalidSpec(
                "runtime and container name must be non-empty".to_string(),
            ));
        }
        std::fs::create_dir_all(state_dir).map_err(|e| {
            SandboxError::InvalidSpec(format!(
                "cannot create the container state directory {}: {}",
                state_dir.display(),
                e
            ))
        })?;
        let cidfile = state_dir.join(format!("{}.cid", name));
        Ok(Self {
            runtime,
            image,
            name,
            cidfile,
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn cidfile(&self) -> &Path {
        &self.cidfile
    }

    /// The container id the runtime recorded, if it recorded one.
    pub fn container_id(&self) -> Option<String> {
        std::fs::read_to_string(&self.cidfile)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }

    /// `create` argv for this workload: the same isolation flags as [`SandboxSpec::wrap_argv`], but
    /// detached from any client, named, and with the id recorded.
    pub fn create_argv(&self, spec: &SandboxSpec, program: &str, args: &[String]) -> Vec<String> {
        spec.container_create_argv(
            &self.runtime,
            &self.image,
            &self.name,
            &self.cidfile,
            program,
            args,
        )
    }

    /// Create the container (no process starts yet).
    pub fn create(
        &self,
        spec: &SandboxSpec,
        program: &str,
        args: &[String],
        transport: &dyn ProbeTransport,
        budget: Duration,
    ) -> Result<Option<String>, SandboxError> {
        let argv = self.create_argv(spec, program, args);
        let (program, rest) = argv
            .split_first()
            .ok_or_else(|| SandboxError::InvalidSpec("empty container argv".to_string()))?;
        let out = transport
            .run(program, rest, budget)
            .map_err(SandboxError::ContainerUnavailable)?;
        if !out.success || out.truncated {
            return Err(SandboxError::ContainerUnavailable(format!(
                "container create failed: {}",
                out.stderr.trim()
            )));
        }
        let id = out.stdout.trim().to_string();
        let id = if id.is_empty() {
            self.container_id()
        } else {
            Some(id)
        };
        Ok(id)
    }

    /// Start the created container.
    pub fn start(
        &self,
        transport: &dyn ProbeTransport,
        budget: Duration,
    ) -> Result<(), SandboxError> {
        let out = transport
            .run(
                &self.runtime,
                &["start".to_string(), self.name.clone()],
                budget,
            )
            .map_err(SandboxError::ContainerUnavailable)?;
        if out.success && !out.truncated {
            Ok(())
        } else {
            Err(SandboxError::ContainerUnavailable(format!(
                "container start failed: {}",
                out.stderr.trim()
            )))
        }
    }

    /// Is the container still known to the runtime?
    pub fn exists(
        &self,
        transport: &dyn ProbeTransport,
        budget: Duration,
    ) -> Result<bool, SandboxError> {
        let out = transport
            .run(
                &self.runtime,
                &[
                    "inspect".to_string(),
                    "--format".to_string(),
                    "{{.Id}}".to_string(),
                    self.name.clone(),
                ],
                budget,
            )
            .map_err(SandboxError::ContainerUnavailable)?;
        if out.truncated {
            return Err(SandboxError::ContainerUnavailable(
                "container inspect output was truncated".to_string(),
            ));
        }
        Ok(out.success)
    }

    /// Stop (if running), remove, and **verify** the container is gone.
    ///
    /// Every step is observable and every failure keeps the id: this returns
    /// [`CleanupOutcome::RecoveryRequired`] rather than pretending the workload is gone.
    pub fn cancel(&self, transport: &dyn ProbeTransport, budget: Duration) -> CleanupOutcome {
        let container_id = self.container_id();
        // Nothing was ever created: nothing to clean.
        if container_id.is_none() && !self.cidfile.exists() {
            match self.exists(transport, budget) {
                Ok(false) => return CleanupOutcome::NothingToClean,
                Ok(true) => {}
                Err(reason) => {
                    return CleanupOutcome::RecoveryRequired {
                        container_id,
                        reason: format!("cannot determine whether the workload exists: {reason}"),
                        retry_command: self.retry_command(),
                    }
                }
            }
        }
        let stop = transport.run(
            &self.runtime,
            &[
                "stop".to_string(),
                "--time".to_string(),
                "1".to_string(),
                self.name.clone(),
            ],
            budget,
        );
        if let Err(reason) = &stop {
            return CleanupOutcome::RecoveryRequired {
                container_id,
                reason: format!("container stop did not complete: {reason}"),
                retry_command: self.retry_command(),
            };
        }
        let remove = transport.run(
            &self.runtime,
            &["rm".to_string(), "-f".to_string(), self.name.clone()],
            budget,
        );
        match remove {
            Ok(out) if out.success && !out.truncated => {}
            Ok(out) => {
                return CleanupOutcome::RecoveryRequired {
                    container_id,
                    reason: format!("container remove failed: {}", out.stderr.trim()),
                    retry_command: self.retry_command(),
                }
            }
            Err(reason) => {
                return CleanupOutcome::RecoveryRequired {
                    container_id,
                    reason: format!("container remove did not complete: {reason}"),
                    retry_command: self.retry_command(),
                }
            }
        }
        // Confirm, rather than assume: `rm` returning success is not the same as the daemon having
        // no record of the workload.
        match self.exists(transport, budget) {
            Ok(false) => {
                let _ = std::fs::remove_file(&self.cidfile);
                CleanupOutcome::Cleaned { container_id }
            }
            Ok(true) => CleanupOutcome::RecoveryRequired {
                container_id,
                reason: "the runtime still reports the container after remove".to_string(),
                retry_command: self.retry_command(),
            },
            Err(reason) => CleanupOutcome::RecoveryRequired {
                container_id,
                reason: format!("cannot confirm removal: {reason}"),
                retry_command: self.retry_command(),
            },
        }
    }

    /// The exact command an operator can run to finish a failed cleanup.
    pub fn retry_command(&self) -> String {
        format!(
            "{} stop --time 1 {n} && {} rm -f {n}",
            self.runtime,
            self.runtime,
            n = self.name
        )
    }
}

/// T09 (KR-18): the trust class of a spawn. Each class has its own network and sandbox rule; the
/// run's policy answers for all of them, so "the sandbox is on" cannot mean "on for the scorer only".
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ExecutionClass {
    /// The agent's `bash` tool: untrusted model-authored commands.
    AgentBash,
    /// The suite verifier / scorer command.
    Verifier,
    /// A user-supplied hook (pre/post tool, stop hook).
    Hook,
    /// An MCP server process. Operator-installed infrastructure, not model-authored code.
    McpServer,
    /// A verification probe run against a candidate revision.
    Probe,
}

impl ExecutionClass {
    pub fn as_str(self) -> &'static str {
        match self {
            ExecutionClass::AgentBash => "agent_bash",
            ExecutionClass::Verifier => "verifier",
            ExecutionClass::Hook => "hook",
            ExecutionClass::McpServer => "mcp_server",
            ExecutionClass::Probe => "probe",
        }
    }

    pub fn all() -> [ExecutionClass; 5] {
        [
            ExecutionClass::AgentBash,
            ExecutionClass::Verifier,
            ExecutionClass::Hook,
            ExecutionClass::McpServer,
            ExecutionClass::Probe,
        ]
    }
}

/// What one class is allowed to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClassRule {
    /// Must this class run inside the sandbox? `true` means "refuse rather than run on the host".
    pub sandboxed: bool,
    /// May it reach the network? Only meaningful when sandboxed (the host has whatever the process
    /// has); an isolated class with `network: false` gets `--unshare-net`.
    pub network: bool,
}

/// The run's execution boundary.
#[derive(Debug, Clone, PartialEq)]
pub enum ExecutionMode {
    /// Explicitly unsandboxed. This is a configuration choice an operator makes, never a fallback.
    Host,
    /// Sandboxed: every class whose rule says `sandboxed` runs inside `template`, and a class that
    /// cannot be sandboxed (missing backend) is refused instead of being run on the host.
    Isolated(SandboxTemplate),
}

/// A prepared spawn: the argv to execute and the backend that will actually run it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedLaunch {
    pub program: String,
    pub args: Vec<String>,
    /// Working directory to launch in. Inside a sandbox this is the guest path the workspace is bound
    /// at, not the host path — launching in the host path would simply fail to `chdir`.
    pub cwd: Option<String>,
    /// `host`, `bwrap`, or `container:<runtime>`. This is what is *actually* used, so it can be
    /// reported and asserted instead of repeating what the configuration asked for.
    pub backend: String,
    /// T10 (KR-17): present when the backend is a container, so the caller can stop and remove the
    /// workload by name when the run is cancelled. Killing the client alone would leave it running.
    pub container: Option<ContainerWorkload>,
}

/// The resolved execution policy of one run (T09, KR-18).
///
/// The table below is the whole contract; there is no per-call override, so no model-supplied
/// parameter can move a class into or out of the sandbox:
///
/// | class | isolated mode | network |
/// | --- | --- | --- |
/// | `agent_bash` | sandboxed | denied |
/// | `verifier` | sandboxed | denied |
/// | `hook` | sandboxed | denied |
/// | `probe` | sandboxed | denied |
/// | `mcp_server` | host | allowed |
///
/// `mcp_server` is deliberately not sandboxed: an MCP server is operator-installed infrastructure
/// that usually needs the network and its own toolchain, and pretending to sandbox it would only
/// break it. In host mode every class runs on the host — that is an explicit operator decision
/// ([`ExecutionMode::Host`]), and [`Self::launch_argv`] still reports `host` as the backend so the
/// difference is visible in the evidence rather than implied.
#[derive(Debug, Clone, PartialEq)]
pub struct ExecutionPolicy {
    mode: ExecutionMode,
    rules: Vec<(ExecutionClass, ClassRule)>,
}

impl ExecutionPolicy {
    /// Explicitly unsandboxed. Not a fallback: the caller asked for this.
    pub fn host() -> Self {
        Self {
            mode: ExecutionMode::Host,
            rules: ExecutionClass::all()
                .into_iter()
                .map(|class| {
                    (
                        class,
                        ClassRule {
                            sandboxed: false,
                            network: true,
                        },
                    )
                })
                .collect(),
        }
    }

    /// Sandboxed with the default class table above.
    pub fn isolated(template: SandboxTemplate) -> Self {
        Self {
            mode: ExecutionMode::Isolated(template),
            rules: vec![
                (
                    ExecutionClass::AgentBash,
                    ClassRule {
                        sandboxed: true,
                        network: false,
                    },
                ),
                (
                    ExecutionClass::Verifier,
                    ClassRule {
                        sandboxed: true,
                        network: false,
                    },
                ),
                (
                    ExecutionClass::Hook,
                    ClassRule {
                        sandboxed: true,
                        network: false,
                    },
                ),
                (
                    ExecutionClass::Probe,
                    ClassRule {
                        sandboxed: true,
                        network: false,
                    },
                ),
                (
                    ExecutionClass::McpServer,
                    ClassRule {
                        sandboxed: false,
                        network: true,
                    },
                ),
            ],
        }
    }

    /// Narrow one class' rule (used by configuration, never by model input).
    pub fn with_rule(mut self, class: ExecutionClass, rule: ClassRule) -> Self {
        for entry in &mut self.rules {
            if entry.0 == class {
                entry.1 = rule;
                return self;
            }
        }
        self.rules.push((class, rule));
        self
    }

    pub fn mode(&self) -> &ExecutionMode {
        &self.mode
    }

    /// The path the **guest** sees for this run's workspace.
    ///
    /// Host mode is the host path itself; isolated mode is the sandbox's own mount point
    /// ([`SandboxSpec::workspace_guest`], `/workspace` by default), which is where the same directory
    /// appears *inside* the sandbox. A command built for the guest must use this view: passing the
    /// host path to a sandboxed interpreter names a directory the guest does not have, and mapping
    /// the host's trusted state directory into the guest to compensate would hand the agent the
    /// runtime's private files.
    ///
    /// Pure: it resolves the template's mount point and never probes the backend.
    pub fn guest_workspace_root(&self, host_workspace: &std::path::Path) -> String {
        match &self.mode {
            ExecutionMode::Host => host_workspace.display().to_string(),
            ExecutionMode::Isolated(template) => {
                template
                    .resolve(host_workspace.to_path_buf())
                    .workspace_guest
            }
        }
    }

    pub fn rule(&self, class: ExecutionClass) -> ClassRule {
        self.rules
            .iter()
            .find(|(c, _)| *c == class)
            .map(|(_, r)| *r)
            .unwrap_or(ClassRule {
                sandboxed: false,
                network: true,
            })
    }

    /// Does this class have to run inside the sandbox?
    pub fn requires_sandbox(&self, class: ExecutionClass) -> bool {
        matches!(self.mode, ExecutionMode::Isolated(_)) && self.rule(class).sandboxed
    }

    /// Prepare a spawn for `class`.
    ///
    /// Fail-closed: when the class requires the sandbox and the backend is unavailable, this
    /// returns [`SandboxError`] and the caller must not run the command. There is no branch that
    /// silently produces a host argv for a class that requires isolation.
    pub fn launch_argv(
        &self,
        class: ExecutionClass,
        workspace: &std::path::Path,
        state_dir: &std::path::Path,
        program: &str,
        args: &[String],
    ) -> Result<PreparedLaunch, SandboxError> {
        if !self.requires_sandbox(class) {
            return Ok(PreparedLaunch {
                program: program.to_string(),
                args: args.to_vec(),
                cwd: None,
                backend: "host".to_string(),
                container: None,
            });
        }
        let ExecutionMode::Isolated(template) = &self.mode else {
            // Unreachable: `requires_sandbox` is false in host mode. Kept explicit so a future mode
            // cannot quietly fall through to the host.
            return Err(SandboxError::InvalidSpec(
                "isolated mode required but the policy is not isolated".to_string(),
            ));
        };
        let mut spec = template.resolve(workspace.to_path_buf());
        spec = spec.with_network(self.rule(class).network);
        // Probe first: `wrap_argv` is pure and would happily emit a bwrap argv even when bwrap is
        // not installed, which is exactly the silent-degradation path this policy must not have.
        spec.probe()?;
        let (argv, backend, container) = match &spec.backend {
            SandboxBackend::Bwrap => (spec.wrap_argv(program, args)?, "bwrap".to_string(), None),
            SandboxBackend::Container { runtime, image } => {
                // T10 (KR-17): a container the run cannot address by name is a workload it cannot
                // cancel. The name and cidfile live in the trusted state area, never the workspace.
                let workload =
                    ContainerWorkload::new(runtime.clone(), image.clone(), "kroma", state_dir)?;
                let argv = spec.wrap_container_named(
                    runtime,
                    image,
                    workload.name(),
                    workload.cidfile(),
                    program,
                    args,
                );
                (argv, format!("container:{}", runtime), Some(workload))
            }
        };
        let mut argv = argv.into_iter();
        let program = argv.next().ok_or_else(|| {
            SandboxError::InvalidSpec("the sandbox produced an empty argv".to_string())
        })?;
        Ok(PreparedLaunch {
            program,
            args: argv.collect(),
            cwd: Some(spec.workspace_guest.clone()),
            backend,
            container,
        })
    }
}

/// R02: one trusted host file delivered read-only at a fixed guest path.
///
/// A held-out verifier is not part of the scored workspace and must never be moved into it: the agent
/// can write there, and an agent-writable oracle is not an oracle. [`SandboxSpec::ro_extra`] mounts a
/// path at the *same* host path, which cannot deliver a file under `/tmp` (the guest `/tmp` is a fresh
/// tmpfs), so verifier files get their own guest namespace and their own guest path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadOnlyMount {
    /// Absolute host path of a regular file that exists when the sandbox is built.
    pub host: PathBuf,
    /// Absolute guest path, free of `.`/`..` components. Its parent is created inside the sandbox,
    /// never on the host.
    pub guest: String,
}

impl ReadOnlyMount {
    pub fn new(host: impl Into<PathBuf>, guest: impl Into<String>) -> Result<Self, String> {
        let host = host.into();
        let guest = guest.into();
        if !host.is_absolute() {
            return Err(format!(
                "read-only mount source '{}' must be absolute",
                host.display()
            ));
        }
        if !guest.starts_with('/') {
            return Err(format!(
                "read-only mount target '{}' must be absolute",
                guest
            ));
        }
        if guest
            .split('/')
            .any(|component| component == "." || component == "..")
        {
            return Err(format!(
                "read-only mount target '{}' must not contain '.' or '..'",
                guest
            ));
        }
        Ok(Self { host, guest })
    }
}

/// Everything the sandbox needs to translate a command. Construct with [`SandboxSpec::bwrap`] or
/// [`SandboxSpec::container`] and refine with the `with_*` builders.
#[derive(Debug, Clone, PartialEq)]
pub struct SandboxSpec {
    pub backend: SandboxBackend,
    /// The host directory that becomes the only writable view inside the sandbox.
    pub workspace_host: PathBuf,
    /// Where the workspace appears inside the sandbox.
    pub workspace_guest: String,
    /// Allow network egress. Defaults to `false`; the launch flag is `--unshare-net` /
    /// `--network none` unless this is explicitly set.
    pub network: bool,
    /// Size cap for the sandbox `/tmp`.
    pub tmpfs_bytes: u64,
    /// Container memory cap (bwrap ignores this: its caps come from an outer cgroup).
    pub memory_bytes: Option<u64>,
    /// Container pid cap.
    pub pids_max: Option<u64>,
    /// Container CPU cap in cores.
    pub cpu_cores: Option<f64>,
    /// Extra read-only host paths exposed inside the sandbox (a toolchain outside `/usr`).
    pub ro_extra: Vec<PathBuf>,
    /// Trusted files delivered read-only at explicit guest paths (the held-out verifier's own files).
    /// Distinct from `ro_extra`, which keeps the host path and would be shadowed by the guest tmpfs.
    pub read_only_mounts: Vec<ReadOnlyMount>,
    /// Per-process kernel limits for the bwrap backend, which has no built-in quotas. The container
    /// backend ignores these because the runtime already enforces `--memory` / `--cpus`; using both
    /// would give two different, confusing ceilings.
    pub rlimits: Option<ResourceLimits>,
}

impl SandboxSpec {
    /// A bubblewrap sandbox over `workspace_host`, with the network unshared and container-style
    /// limits left unset (bwrap caps come from the outer cgroup).
    pub fn bwrap(workspace_host: impl Into<PathBuf>) -> Self {
        Self {
            backend: SandboxBackend::Bwrap,
            workspace_host: workspace_host.into(),
            workspace_guest: DEFAULT_GUEST_WORKSPACE.to_string(),
            network: false,
            tmpfs_bytes: DEFAULT_TMPFS_BYTES,
            memory_bytes: None,
            pids_max: None,
            cpu_cores: None,
            ro_extra: Vec::new(),
            read_only_mounts: Vec::new(),
            rlimits: None,
        }
    }

    /// A container sandbox over `workspace_host`, with fail-closed default caps.
    pub fn container(
        runtime: impl Into<String>,
        image: impl Into<String>,
        workspace_host: impl Into<PathBuf>,
    ) -> Self {
        Self {
            backend: SandboxBackend::Container {
                runtime: runtime.into(),
                image: image.into(),
            },
            workspace_host: workspace_host.into(),
            workspace_guest: DEFAULT_GUEST_WORKSPACE.to_string(),
            network: false,
            tmpfs_bytes: DEFAULT_TMPFS_BYTES,
            memory_bytes: Some(DEFAULT_CONTAINER_MEMORY_BYTES),
            pids_max: Some(DEFAULT_CONTAINER_PIDS),
            cpu_cores: Some(1.0),
            ro_extra: Vec::new(),
            read_only_mounts: Vec::new(),
            rlimits: None,
        }
    }

    pub fn with_network(mut self, allow: bool) -> Self {
        self.network = allow;
        self
    }

    pub fn with_tmpfs_bytes(mut self, bytes: u64) -> Self {
        self.tmpfs_bytes = bytes;
        self
    }

    pub fn with_ro_extra(mut self, path: impl Into<PathBuf>) -> Self {
        self.ro_extra.push(path.into());
        self
    }

    /// R02: deliver `mount.host` read-only at `mount.guest` inside the sandbox.
    pub fn with_read_only_mount(mut self, mount: ReadOnlyMount) -> Self {
        self.read_only_mounts.push(mount);
        self
    }

    /// Bound the sandboxed process tree's kernel resources (bwrap backend).
    pub fn with_rlimits(mut self, limits: ResourceLimits) -> Self {
        self.rlimits = Some(limits);
        self
    }

    pub fn with_container_limits(
        mut self,
        memory_bytes: Option<u64>,
        pids_max: Option<u64>,
        cpu_cores: Option<f64>,
    ) -> Self {
        self.memory_bytes = memory_bytes;
        self.pids_max = pids_max;
        self.cpu_cores = cpu_cores;
        self
    }

    /// `true` when the spec would let the sandbox reach the network.
    pub fn network_allowed(&self) -> bool {
        self.network
    }

    /// Translate a host program + args into the sandboxed argv. Pure: it resolves nothing on the
    /// host beyond the whitelist membership of paths, and it never produces an unsandboxed argv.
    pub fn wrap_argv(&self, program: &str, args: &[String]) -> Result<Vec<String>, SandboxError> {
        if self.workspace_host.as_os_str().is_empty() {
            return Err(SandboxError::InvalidSpec(
                "the workspace host path is empty".to_string(),
            ));
        }
        if program.trim().is_empty() {
            return Err(SandboxError::InvalidSpec(
                "the program is empty".to_string(),
            ));
        }
        match &self.backend {
            SandboxBackend::Bwrap => Ok(self.wrap_bwrap(program, args)),
            SandboxBackend::Container { runtime, image } => {
                if runtime.trim().is_empty() {
                    return Err(SandboxError::InvalidSpec(
                        "the container runtime is empty".to_string(),
                    ));
                }
                if image.trim().is_empty() {
                    return Err(SandboxError::InvalidSpec(
                        "the container image is empty".to_string(),
                    ));
                }
                Ok(self.wrap_container(runtime, image, program, args))
            }
        }
    }

    fn wrap_bwrap(&self, program: &str, args: &[String]) -> Vec<String> {
        let mut argv: Vec<String> = vec!["bwrap".to_string()];
        let ro = |argv: &mut Vec<String>, path: &Path| {
            argv.push("--ro-bind-try".to_string());
            argv.push(path.display().to_string());
            argv.push(path.display().to_string());
        };
        for dir in BWRAP_RO_HOST_DIRS {
            ro(&mut argv, Path::new(dir));
        }
        for entry in BWRAP_RO_ETC_ENTRIES {
            ro(&mut argv, &Path::new("/etc").join(entry));
        }
        for extra in &self.ro_extra {
            ro(&mut argv, extra);
        }
        // R02: trusted verifier files, at explicit guest paths. `--ro-bind` (not `-try`) so a file
        // that vanished between validation and launch aborts the sandbox instead of silently
        // producing a guest that cannot find its oracle.
        let mut created_dirs: Vec<String> = Vec::new();
        for mount in &self.read_only_mounts {
            if let Some(parent) = Path::new(&mount.guest).parent() {
                let parent = parent.display().to_string();
                if !parent.is_empty() && !created_dirs.contains(&parent) {
                    argv.push("--dir".to_string());
                    argv.push(parent.clone());
                    created_dirs.push(parent);
                }
            }
            argv.push("--ro-bind".to_string());
            argv.push(mount.host.display().to_string());
            argv.push(mount.guest.clone());
        }
        // A size-capped tmpfs so a runaway scorer cannot fill the host disk through /tmp.
        argv.push("--size".to_string());
        argv.push(self.tmpfs_bytes.to_string());
        argv.push("--tmpfs".to_string());
        argv.push("/tmp".to_string());
        argv.push("--proc".to_string());
        argv.push("/proc".to_string());
        argv.push("--dev".to_string());
        argv.push("/dev".to_string());
        // Exactly one writable path: the workspace.
        argv.push("--bind".to_string());
        argv.push(self.workspace_host.display().to_string());
        argv.push(self.workspace_guest.clone());
        argv.push("--chdir".to_string());
        argv.push(self.workspace_guest.clone());
        argv.push("--unshare-all".to_string());
        if self.network {
            argv.push("--share-net".to_string());
        }
        argv.push("--die-with-parent".to_string());
        argv.push("--new-session".to_string());
        // The guest environment is built here, not inherited. The launcher's environment is only
        // *sanitized* (`SanitizedMinimal` = the host's variables minus a blacklist of named
        // credentials), so anything the blacklist does not recognise — a DSN, a service token
        // under an unfamiliar name, `SSH_AUTH_SOCK` — would otherwise be readable by the untrusted
        // program. `--clearenv` must come before every `--setenv`: bwrap applies `--setenv` to the
        // environment current at that point in the argv. bwrap itself keeps `PWD`.
        argv.push("--clearenv".to_string());
        // HOME/TMPDIR/PATH are the host-free base: HOME and TMPDIR point into the sandbox tmpfs
        // (the host's are never exposed), and PATH names only mounted directories so the guest runs
        // in the default C locale without the host's layout. `CI`/`NO_COLOR` are the same baseline
        // the launch policy sets for every supervised child, restated here because the clear would
        // otherwise drop them: a scored run must behave the same on a laptop and on a CI runner.
        for (key, value) in [
            ("HOME", "/tmp"),
            ("TMPDIR", "/tmp"),
            ("PATH", DEFAULT_GUEST_PATH),
            ("CI", "true"),
            ("NO_COLOR", "1"),
        ] {
            argv.push("--setenv".to_string());
            argv.push(key.to_string());
            argv.push(value.to_string());
        }
        argv.push("--".to_string());
        argv.push(program.to_string());
        argv.extend(args.iter().cloned());
        argv
    }

    /// T10 (KR-17): `run` form that is *trackable*: same isolation flags, plus a unique `--name` and
    /// a `--cidfile` in the trusted state area, so cancelling can stop and remove the workload even
    /// if the client process is killed.
    pub fn wrap_container_named(
        &self,
        runtime: &str,
        image: &str,
        name: &str,
        cidfile: &Path,
        program: &str,
        args: &[String],
    ) -> Vec<String> {
        let mut argv = self.container_create_argv(runtime, image, name, cidfile, program, args);
        // `create` -> `run`: the rest of the argv is identical, which is the point — a container the
        // run can track is configured exactly like one it cannot.
        if argv.len() > 1 {
            argv[1] = "run".to_string();
        }
        argv
    }

    /// T10 (KR-17): `create` form of [`Self::wrap_container`].
    ///
    /// Same isolation flags, but the container is *named*, its id is written to a cidfile, and it is
    /// not tied to a foreground client. `--rm` is deliberately absent: removal is an explicit,
    /// observable step ([`ContainerWorkload::cancel`]) so a failed cleanup can be reported instead of
    /// vanishing with the client.
    pub fn container_create_argv(
        &self,
        runtime: &str,
        image: &str,
        name: &str,
        cidfile: &Path,
        program: &str,
        args: &[String],
    ) -> Vec<String> {
        let mut argv: Vec<String> = vec![
            runtime.to_string(),
            "create".to_string(),
            "--name".to_string(),
            name.to_string(),
            "--cidfile".to_string(),
            cidfile.display().to_string(),
            "--init".to_string(),
        ];
        argv.push("--network".to_string());
        argv.push(if self.network { "bridge" } else { "none" }.to_string());
        argv.push("--read-only".to_string());
        argv.push("--tmpfs".to_string());
        argv.push(format!("/tmp:rw,size={},mode=1777", self.tmpfs_bytes));
        if let Some(memory) = self.memory_bytes {
            argv.push("--memory".to_string());
            argv.push(memory.to_string());
        }
        if let Some(pids) = self.pids_max {
            argv.push("--pids-limit".to_string());
            argv.push(pids.to_string());
        }
        if let Some(cores) = self.cpu_cores {
            argv.push("--cpus".to_string());
            argv.push(cores.to_string());
        }
        argv.push("-v".to_string());
        argv.push(format!(
            "{}:{}:rw",
            self.workspace_host.display(),
            self.workspace_guest
        ));
        // Extra paths (a toolchain, an oracle fixture) are mounted read-only: a container must not be
        // able to rewrite the thing it is being graded against.
        for extra in &self.ro_extra {
            argv.push("--mount".to_string());
            argv.push(format!(
                "type=bind,source={},target={},readonly",
                extra.display(),
                extra.display()
            ));
        }
        // R02: trusted verifier files at explicit guest paths (the image must already have the parent
        // directory; the container backend is not executable on every host).
        for mount in &self.read_only_mounts {
            argv.push("--mount".to_string());
            argv.push(format!(
                "type=bind,source={},target={},readonly",
                mount.host.display(),
                mount.guest
            ));
        }
        argv.push("-w".to_string());
        argv.push(self.workspace_guest.clone());
        argv.push("--env".to_string());
        argv.push("HOME=/tmp".to_string());
        argv.push("--env".to_string());
        argv.push("TMPDIR=/tmp".to_string());
        argv.push(image.to_string());
        argv.push(program.to_string());
        argv.extend(args.iter().cloned());
        argv
    }

    fn wrap_container(
        &self,
        runtime: &str,
        image: &str,
        program: &str,
        args: &[String],
    ) -> Vec<String> {
        let mut argv: Vec<String> =
            vec![runtime.to_string(), "run".to_string(), "--rm".to_string()];
        argv.push("--init".to_string());
        argv.push("--network".to_string());
        argv.push(if self.network { "bridge" } else { "none" }.to_string());
        // The container's own root filesystem is read-only; only the workspace mount is writable.
        argv.push("--read-only".to_string());
        argv.push("--tmpfs".to_string());
        argv.push(format!("/tmp:rw,size={},mode=1777", self.tmpfs_bytes));
        if let Some(memory) = self.memory_bytes {
            argv.push("--memory".to_string());
            argv.push(memory.to_string());
        }
        if let Some(pids) = self.pids_max {
            argv.push("--pids-limit".to_string());
            argv.push(pids.to_string());
        }
        if let Some(cores) = self.cpu_cores {
            argv.push("--cpus".to_string());
            argv.push(cores.to_string());
        }
        argv.push("-v".to_string());
        argv.push(format!(
            "{}:{}:rw",
            self.workspace_host.display(),
            self.workspace_guest
        ));
        argv.push("-w".to_string());
        argv.push(self.workspace_guest.clone());
        argv.push("--env".to_string());
        argv.push("HOME=/tmp".to_string());
        argv.push("--env".to_string());
        argv.push("TMPDIR=/tmp".to_string());
        argv.push(image.to_string());
        argv.push(program.to_string());
        argv.extend(args.iter().cloned());
        argv
    }

    /// Check that the requested backend is actually usable on this host.
    ///
    /// Fail-closed: an unusable backend is an error the caller must surface — never a reason to run
    /// the command on the bare host. For containers this also checks that the image exists locally,
    /// so a missing image cannot silently degrade into "runs anyway".
    /// Probe the backend, cached: a successful probe is remembered for the life of the process, so a
    /// per-spawn check stays cheap while a failure is always re-tested (a transient daemon hiccup
    /// must not permanently disable a run).
    pub fn probe(&self) -> Result<(), SandboxError> {
        self.probe_within(&ProcessProbeTransport, DEFAULT_CONTROL_BUDGET)
    }

    /// T10 (KR-16): bounded probe with an injectable transport.
    ///
    /// Three separate questions, all bounded by `budget`:
    /// 1. is the binary there and does it answer (`--version`);
    /// 2. is the image/namespace usable (a real no-op run, not just an inspect);
    /// 3. does any of it hang — a hung daemon is "unavailable", never "assume it works".
    pub fn probe_within(
        &self,
        transport: &dyn ProbeTransport,
        budget: Duration,
    ) -> Result<(), SandboxError> {
        let key = self.probe_cache_key();
        if probe_success_cache()
            .lock()
            .map(|cache| cache.contains(&key))
            .unwrap_or(false)
        {
            return Ok(());
        }
        self.probe_uncached(transport, budget)?;
        if let Ok(mut cache) = probe_success_cache().lock() {
            cache.insert(key);
        }
        Ok(())
    }

    fn probe_cache_key(&self) -> String {
        match &self.backend {
            SandboxBackend::Bwrap => "bwrap".to_string(),
            SandboxBackend::Container { runtime, image } => {
                format!("container:{}:{}", runtime, image)
            }
        }
    }

    fn probe_uncached(
        &self,
        transport: &dyn ProbeTransport,
        budget: Duration,
    ) -> Result<(), SandboxError> {
        let version = |program: &str| transport.run(program, &["--version".to_string()], budget);
        match &self.backend {
            SandboxBackend::Bwrap => {
                match version("bwrap") {
                    Ok(out) if out.success && !out.truncated => {}
                    Ok(_) => return Err(SandboxError::BwrapMissing),
                    Err(_) => return Err(SandboxError::BwrapMissing),
                }
                // `--version` succeeding says nothing about whether user namespaces work (they are
                // commonly disabled in containers). Run a real, tiny namespace.
                let smoke = transport.run(
                    "bwrap",
                    &[
                        "--ro-bind".to_string(),
                        "/".to_string(),
                        "/".to_string(),
                        "--dev".to_string(),
                        "/dev".to_string(),
                        "--unshare-net".to_string(),
                        "/bin/true".to_string(),
                    ],
                    budget,
                );
                match smoke {
                    Ok(out) if out.success && !out.truncated => Ok(()),
                    Ok(out) => Err(SandboxError::NamespaceUnavailable(format!(
                        "bwrap cannot create a namespace: {}",
                        out.stderr.trim()
                    ))),
                    Err(e) => Err(SandboxError::NamespaceUnavailable(format!(
                        "bwrap namespace probe did not finish: {}",
                        e
                    ))),
                }
            }
            SandboxBackend::Container { runtime, image } => {
                if image.trim().is_empty() {
                    return Err(SandboxError::InvalidSpec(
                        "the container image is empty".to_string(),
                    ));
                }
                match version(runtime) {
                    Ok(out) if out.success && !out.truncated => {}
                    Ok(_) => {
                        return Err(SandboxError::ContainerUnavailable(format!(
                            "runtime '{}' is not usable",
                            runtime
                        )))
                    }
                    Err(e) => {
                        return Err(SandboxError::ContainerUnavailable(format!(
                            "runtime '{}' did not answer --version: {}",
                            runtime, e
                        )))
                    }
                }
                let inspect = transport.run(
                    runtime,
                    &["image".to_string(), "inspect".to_string(), image.clone()],
                    budget,
                );
                match inspect {
                    Ok(out) if out.success && !out.truncated => {}
                    Ok(_) => {
                        return Err(SandboxError::ContainerUnavailable(format!(
                            "image '{}' is not present in runtime '{}'",
                            image, runtime
                        )))
                    }
                    Err(e) => {
                        return Err(SandboxError::ContainerUnavailable(format!(
                            "image inspect did not finish: {}",
                            e
                        )))
                    }
                }
                // The image can exist while the runtime cannot actually start a container (no
                // namespaces, no cgroups delegation, a broken storage driver). One no-op run is the
                // only honest check.
                let smoke = transport.run(
                    runtime,
                    &[
                        "run".to_string(),
                        "--rm".to_string(),
                        "--network".to_string(),
                        "none".to_string(),
                        image.clone(),
                        "/bin/true".to_string(),
                    ],
                    budget,
                );
                match smoke {
                    Ok(out) if out.success && !out.truncated => Ok(()),
                    Ok(out) => Err(SandboxError::NamespaceUnavailable(format!(
                        "runtime '{}' cannot start a container: {}",
                        runtime,
                        out.stderr.trim()
                    ))),
                    Err(e) => Err(SandboxError::NamespaceUnavailable(format!(
                        "container namespace probe did not finish: {}",
                        e
                    ))),
                }
            }
        }
    }
}

/// A sandbox recipe whose workspace is resolved per run.
///
/// A caller that does not know the workspace path up front (the benchmark harness creates one
/// isolated workspace per arm) configures this instead of a [`SandboxSpec`], and resolves it once
/// the real path exists. Keeping the two types separate is what makes it impossible to accidentally
/// sandbox a run against the wrong directory: a spec always names a concrete workspace.
#[derive(Debug, Clone, PartialEq)]
pub struct SandboxTemplate {
    pub backend: SandboxBackend,
    pub network: bool,
    pub tmpfs_bytes: u64,
    pub rlimits: Option<ResourceLimits>,
    pub ro_extra: Vec<PathBuf>,
}

impl SandboxTemplate {
    /// The default recipe for a backend: network off, default tmpfs, no extra mounts.
    pub fn new(backend: SandboxBackend) -> Self {
        Self {
            backend,
            network: false,
            tmpfs_bytes: DEFAULT_TMPFS_BYTES,
            rlimits: None,
            ro_extra: Vec::new(),
        }
    }

    /// bubblewrap with the hardened defaults used by the verification host.
    pub fn bwrap() -> Self {
        Self::new(SandboxBackend::Bwrap)
    }

    pub fn with_network(mut self, allow: bool) -> Self {
        self.network = allow;
        self
    }

    pub fn with_tmpfs_bytes(mut self, bytes: u64) -> Self {
        self.tmpfs_bytes = bytes;
        self
    }

    pub fn with_rlimits(mut self, limits: ResourceLimits) -> Self {
        self.rlimits = Some(limits);
        self
    }

    pub fn with_ro_extra(mut self, path: impl Into<PathBuf>) -> Self {
        self.ro_extra.push(path.into());
        self
    }

    /// Bind the recipe to one concrete workspace.
    pub fn resolve(&self, workspace: impl Into<PathBuf>) -> SandboxSpec {
        let mut spec = match &self.backend {
            SandboxBackend::Bwrap => SandboxSpec::bwrap(workspace),
            SandboxBackend::Container { runtime, image } => {
                SandboxSpec::container(runtime.clone(), image.clone(), workspace)
            }
        };
        spec.network = self.network;
        spec.tmpfs_bytes = self.tmpfs_bytes;
        spec.rlimits = self.rlimits;
        spec.ro_extra = self.ro_extra.clone();
        spec
    }
}

/// Run a command inside a sandbox through the shared [`ProcessSupervisor`].
///
/// The supervisor owns the deadline, the output flood cap, cancellation and process-group cleanup;
/// the sandbox only decides *where* the command runs. A failed [`SandboxSpec::probe`] is returned
/// as an error and **nothing is executed**.
pub async fn execute_sandboxed(
    spec: &SandboxSpec,
    program: &str,
    args: &[String],
    timeout: std::time::Duration,
) -> Result<OneShotExecutionResult, String> {
    spec.probe().map_err(|e| e.to_string())?;
    let argv = spec.wrap_argv(program, args).map_err(|e| e.to_string())?;
    let mut command = OneShotCommandSpec::new(argv, spec.workspace_host.clone())
        .with_timeout(timeout)
        .with_env_policy(crate::process_launch::EnvironmentPolicy::SanitizedMinimal);
    if let Some(limits) = spec
        .rlimits
        .filter(|limits| !limits.is_empty() && spec.backend == SandboxBackend::Bwrap)
    {
        command = command.with_rlimits(limits);
    }
    ProcessSupervisor::execute_one_shot(command, None)
        .await
        .map_err(|e| e.to_string())
}
