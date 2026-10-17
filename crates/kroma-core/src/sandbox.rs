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
//!   `/etc/shadow`, `/etc/ssh`, ...) simply does not exist inside the sandbox.
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

use crate::process_launch::ResourceLimits;
use crate::process_supervisor::{OneShotCommandSpec, OneShotExecutionResult, ProcessSupervisor};

/// Where the workspace is mounted inside the sandbox by default.
pub const DEFAULT_GUEST_WORKSPACE: &str = "/workspace";
/// Default size cap for the sandbox `/tmp` (256 MiB).
pub const DEFAULT_TMPFS_BYTES: u64 = 256 * 1024 * 1024;
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
        }
    }
}

impl std::error::Error for SandboxError {}

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
            return Err(SandboxError::InvalidSpec("the program is empty".to_string()));
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
        // HOME and TMPDIR point into the sandbox tmpfs; the host's are never exposed.
        for (key, value) in [("HOME", "/tmp"), ("TMPDIR", "/tmp")] {
            argv.push("--setenv".to_string());
            argv.push(key.to_string());
            argv.push(value.to_string());
        }
        argv.push("--".to_string());
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
        let mut argv: Vec<String> = vec![runtime.to_string(), "run".to_string(), "--rm".to_string()];
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
    pub fn probe(&self) -> Result<(), SandboxError> {
        match &self.backend {
            SandboxBackend::Bwrap => {
                let ok = std::process::Command::new("bwrap")
                    .arg("--version")
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status()
                    .map(|status| status.success())
                    .unwrap_or(false);
                if ok {
                    Ok(())
                } else {
                    Err(SandboxError::BwrapMissing)
                }
            }
            SandboxBackend::Container { runtime, image } => {
                if image.trim().is_empty() {
                    return Err(SandboxError::InvalidSpec(
                        "the container image is empty".to_string(),
                    ));
                }
                let runtime_ok = std::process::Command::new(runtime)
                    .arg("--version")
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status()
                    .map(|status| status.success())
                    .unwrap_or(false);
                if !runtime_ok {
                    return Err(SandboxError::ContainerUnavailable(format!(
                        "runtime '{}' is not usable",
                        runtime
                    )));
                }
                let image_ok = std::process::Command::new(runtime)
                    .args(["image", "inspect", image])
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status()
                    .map(|status| status.success())
                    .unwrap_or(false);
                if !image_ok {
                    return Err(SandboxError::ContainerUnavailable(format!(
                        "image '{}' is not present in runtime '{}'",
                        image, runtime
                    )));
                }
                Ok(())
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
