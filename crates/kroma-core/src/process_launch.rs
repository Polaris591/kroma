//! Shared child-process launch seam: environment policy, stdio, working directory, and
//! cross-platform process-tree containment in one place.
//!
//! The one-shot supervisor ([`crate::process_supervisor`]) and the long-lived MCP server client
//! ([`crate::mcp`]) both launch through [`ProcessLaunch`], so the credential scrub, the environment
//! policy, and the process-tree containment cannot drift between a one-shot command and a
//! long-lived server. The process-tree primitives themselves live in [`crate::process_group`]; this
//! module only sequences them and never re-implements containment, sanitization, or deadlines.

use std::io;
use std::path::PathBuf;
use std::process::Stdio;
#[cfg(unix)]
use std::os::unix::process::CommandExt;

use crate::env::build_sanitized_env;
use crate::process_group::{ChildProcessGroup, Containment};

/// `PATH` used when the host has none at all, so a bare program name still resolves. An *empty*
/// host `PATH` counts as present and is left alone.
#[cfg(unix)]
const FALLBACK_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// Execution environment sanitization and override policy shared by every supervised spawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvironmentPolicy {
    /// Strips blacklisted sensitive environment variables (API keys, secrets, tokens) and injects
    /// baseline flags (`CI=true`, `NO_COLOR=1`).
    SanitizedMinimal,
    /// Sanitized minimal baseline plus explicitly authorized key-value overrides.
    SanitizedWithCustom(Vec<(String, String)>),
    /// Sanitized host environment plus explicitly authorized overrides, *without* the `CI` /
    /// `NO_COLOR` baseline flags. Used by the supervised `bash` tool, command hooks, and long-lived
    /// MCP servers, whose inherited environment must be preserved apart from the credential scrub.
    SanitizedWithCustomNoBaseline(Vec<(String, String)>),
    /// Completely explicit environment: clears the host environment and sets only these variables.
    Explicit(Vec<(String, String)>),
}

impl EnvironmentPolicy {
    /// Apply this policy to a synchronous command.
    pub fn apply(&self, cmd: &mut std::process::Command) {
        match self {
            EnvironmentPolicy::Explicit(vars) => {
                cmd.env_clear();
                for (key, value) in vars {
                    cmd.env(key, value);
                }
            }
            EnvironmentPolicy::SanitizedMinimal => {
                apply_sanitized(cmd);
                cmd.env("CI", "true");
                cmd.env("NO_COLOR", "1");
            }
            EnvironmentPolicy::SanitizedWithCustom(overrides) => {
                apply_sanitized(cmd);
                cmd.env("CI", "true");
                cmd.env("NO_COLOR", "1");
                for (key, value) in overrides {
                    cmd.env(key, value);
                }
            }
            EnvironmentPolicy::SanitizedWithCustomNoBaseline(overrides) => {
                apply_sanitized(cmd);
                for (key, value) in overrides {
                    cmd.env(key, value);
                }
            }
        }
    }

    /// Apply this policy to a tokio command. Delegates to [`Self::apply`] through the wrapped
    /// synchronous command so the policy is spelled once.
    pub fn apply_tokio(&self, cmd: &mut tokio::process::Command) {
        self.apply(cmd.as_std_mut());
    }

    /// Whether this policy sets `PATH` itself, so the launch-time `PATH` fallback must not overwrite
    /// the caller's explicit choice. Only the Unix fallback consults it.
    #[cfg(unix)]
    fn sets_path(&self) -> bool {
        let overrides = match self {
            EnvironmentPolicy::Explicit(vars)
            | EnvironmentPolicy::SanitizedWithCustom(vars)
            | EnvironmentPolicy::SanitizedWithCustomNoBaseline(vars) => vars,
            EnvironmentPolicy::SanitizedMinimal => return false,
        };
        overrides.iter().any(|(key, _)| key == "PATH")
    }
}

fn apply_sanitized(cmd: &mut std::process::Command) {
    cmd.env_clear();
    cmd.envs(build_sanitized_env());
}

/// Builder for a supervised child process launch.
///
/// Every field has a safe default: the sanitized minimal environment, piped stdout/stderr, null
/// stdin, no `kill_on_drop`, and real platform containment.
#[derive(Debug)]
pub struct ProcessLaunch {
    program: String,
    args: Vec<String>,
    /// Set only by [`Self::shell_invocation`]: the command must be applied as a resolved shell
    /// invocation rather than as the pre-split `args`.
    raw_command: Option<String>,
    cwd: Option<PathBuf>,
    env_policy: EnvironmentPolicy,
    stdin: Stdio,
    stdout: Stdio,
    stderr: Stdio,
    kill_on_drop: bool,
    containment: Containment,
    rlimits: Option<ResourceLimits>,
}

/// Per-process kernel resource limits applied to the child before `exec`.
///
/// These are inherited by the whole descendant tree and need no privileges, so they work as a
/// sandbox's resource ceiling where a cgroup is unavailable. They are deliberately *not* a
/// replacement for cgroups: RLIMIT_AS bounds address space (an interpreter that reserves a large
/// virtual range can fail early), and there is no portable per-tree process-count limit —
/// `RLIMIT_NPROC` counts every process of the same uid and would misfire on a busy host, so
/// process-count limits must come from `pids.max` in a cgroup instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ResourceLimits {
    /// `RLIMIT_AS`: maximum address space in bytes.
    pub address_space_bytes: Option<u64>,
    /// `RLIMIT_CPU`: maximum CPU time in seconds (SIGXCPU when exceeded).
    pub cpu_seconds: Option<u64>,
    /// `RLIMIT_FSIZE`: maximum size in bytes of any file the process may create.
    pub file_bytes: Option<u64>,
    /// `RLIMIT_NOFILE`: maximum number of open file descriptors.
    pub open_files: Option<u64>,
}

impl ResourceLimits {
    pub fn is_empty(&self) -> bool {
        self.address_space_bytes.is_none()
            && self.cpu_seconds.is_none()
            && self.file_bytes.is_none()
            && self.open_files.is_none()
    }

    /// Apply every configured limit to the current process (called in the child before `exec`).
    ///
    /// `setrlimit` is async-signal-safe, so this is safe to call from a `pre_exec` hook.
    #[cfg(unix)]
    fn apply_to_current_process(&self) -> io::Result<()> {
        fn set(resource: libc::__rlimit_resource_t, value: u64) -> io::Result<()> {
            let limit = libc::rlimit {
                rlim_cur: value as libc::rlim_t,
                rlim_max: value as libc::rlim_t,
            };
            // SAFETY: `limit` is a fully initialized rlimit for this process.
            if unsafe { libc::setrlimit(resource, &limit) } != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }
        if let Some(bytes) = self.address_space_bytes {
            set(libc::RLIMIT_AS, bytes)?;
        }
        if let Some(seconds) = self.cpu_seconds {
            set(libc::RLIMIT_CPU, seconds)?;
        }
        if let Some(bytes) = self.file_bytes {
            set(libc::RLIMIT_FSIZE, bytes)?;
        }
        if let Some(files) = self.open_files {
            set(libc::RLIMIT_NOFILE, files)?;
        }
        Ok(())
    }
}

impl ProcessLaunch {
    pub fn new(program: impl Into<String>, args: impl IntoIterator<Item = String>) -> Self {
        Self {
            program: program.into(),
            args: args.into_iter().collect(),
            raw_command: None,
            cwd: None,
            env_policy: EnvironmentPolicy::SanitizedMinimal,
            stdin: Stdio::null(),
            stdout: Stdio::piped(),
            stderr: Stdio::piped(),
            kill_on_drop: false,
            containment: Containment::Platform,
            rlimits: None,
        }
    }

    /// Launch a resolved platform shell invocation instead of a plain argv.
    ///
    /// Only for callers that resolved the shell through
    /// [`crate::process_group::resolve_shell_invocation`]: on Windows a `cmd.exe` program must
    /// receive the whole command as one verbatim `/D /S /C "<raw>"` argument, which the pre-split
    /// `args` cannot carry. `program` is the shell binary passed to [`Self::new`]. This is not a
    /// general way to route arbitrary argv through a shell, so it stays crate-internal.
    pub(crate) fn shell_invocation(mut self, raw_command: impl Into<String>) -> Self {
        self.raw_command = Some(raw_command.into());
        self
    }

    pub fn cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    pub fn env_policy(mut self, env_policy: EnvironmentPolicy) -> Self {
        self.env_policy = env_policy;
        self
    }

    pub fn stdio(mut self, stdin: Stdio, stdout: Stdio, stderr: Stdio) -> Self {
        self.stdin = stdin;
        self.stdout = stdout;
        self.stderr = stderr;
        self
    }

    pub fn kill_on_drop(mut self, kill_on_drop: bool) -> Self {
        self.kill_on_drop = kill_on_drop;
        self
    }

    /// Override how platform containment is attached. Only tests pass
    /// [`Containment::ForceAttachFailure`](crate::process_group::Containment::ForceAttachFailure).
    pub fn containment(mut self, containment: Containment) -> Self {
        self.containment = containment;
        self
    }

    /// Bound the child's kernel resources (address space, CPU time, file size, open files).
    ///
    /// The limits are installed in the forked child before `exec`, so they apply to the whole
    /// descendant tree. See [`ResourceLimits`] for what they do and do not cover.
    pub fn rlimits(mut self, limits: ResourceLimits) -> Self {
        self.rlimits = Some(limits);
        self
    }

    fn apply_common(self, cmd: &mut std::process::Command) {
        match &self.raw_command {
            Some(raw) => {
                crate::process_group::apply_shell_args(cmd, &self.program, &self.args, raw)
            }
            None => {
                cmd.args(&self.args);
            }
        }
        if let Some(ref cwd) = self.cwd {
            cmd.current_dir(cwd);
        }
        cmd.stdin(self.stdin).stdout(self.stdout).stderr(self.stderr);
        self.env_policy.apply(cmd);
        // A sanitized environment drops `PATH` when the host had none, which would stop a bare
        // program name from resolving. Mirror the one-shot supervisor's fallback so both launch
        // paths resolve commands identically. The fallback never overwrites a `PATH` the policy
        // itself set, and an *empty* host `PATH` still counts as present, exactly as before.
        #[cfg(unix)]
        if std::env::var_os("PATH").is_none() && !self.env_policy.sets_path() {
            cmd.env("PATH", FALLBACK_PATH);
        }
        #[cfg(unix)]
        if let Some(limits) = self.rlimits.filter(|limits| !limits.is_empty()) {
            // SAFETY: the closure only calls `setrlimit`, which is async-signal-safe, and it
            // neither allocates nor touches shared state.
            unsafe {
                cmd.pre_exec(move || limits.apply_to_current_process());
            }
        }
    }

    /// Spawn a tokio child and attach platform containment.
    ///
    /// Fails closed: if containment cannot be attached, the child is stopped and reaped and the
    /// error is returned, so a launch never hands back an uncontained process.
    pub fn spawn_tokio(self) -> io::Result<(tokio::process::Child, ChildProcessGroup)> {
        let containment = self.containment;
        let kill_on_drop = self.kill_on_drop;
        let mut cmd = tokio::process::Command::new(&self.program);
        self.apply_common(cmd.as_std_mut());
        cmd.kill_on_drop(kill_on_drop);
        ChildProcessGroup::configure_tokio_command(&mut cmd);
        let mut child = cmd.spawn()?;
        let group = ChildProcessGroup::try_from_tokio_child(&mut child, containment)?;
        Ok((child, group))
    }

    /// Spawn a synchronous child and attach platform containment. Used by the synchronous command
    /// hook path; `kill_on_drop` does not apply here.
    pub fn spawn_std(self) -> io::Result<(std::process::Child, ChildProcessGroup)> {
        let containment = self.containment;
        let mut cmd = std::process::Command::new(&self.program);
        self.apply_common(&mut cmd);
        ChildProcessGroup::configure_std_command(&mut cmd);
        let mut child = cmd.spawn()?;
        let group = ChildProcessGroup::try_from_std_child(&mut child, containment)?;
        Ok((child, group))
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// Reads the `PATH` the launch would hand the child.
    fn launched_path(launch: ProcessLaunch) -> Option<String> {
        let mut cmd = std::process::Command::new("/bin/true");
        launch.apply_common(&mut cmd);
        cmd.get_envs()
            .find(|(key, _)| *key == "PATH")
            .and_then(|(_, value)| value)
            .map(|value| value.to_string_lossy().into_owned())
    }

    /// The launch-time `PATH` fallback only fills a *missing* host `PATH`, and never replaces a
    /// `PATH` the environment policy set itself. An empty host `PATH` counts as present.
    ///
    /// This test manipulates the process `PATH`, so it must run single-threaded (`--test-threads=1`,
    /// which is how the spawn fixtures are run anyway).
    #[test]
    fn path_fallback_fills_only_a_missing_host_path() {
        let saved = std::env::var_os("PATH");

        std::env::remove_var("PATH");
        assert_eq!(
            launched_path(
                ProcessLaunch::new("/bin/true", Vec::<String>::new()).env_policy(
                    EnvironmentPolicy::Explicit(vec![("PATH".to_string(), "/policy/bin".to_string())])
                )
            )
            .as_deref(),
            Some("/policy/bin"),
            "an explicit policy PATH must never be overwritten by the fallback"
        );
        assert_eq!(
            launched_path(
                ProcessLaunch::new("/bin/true", Vec::<String>::new()).env_policy(
                    EnvironmentPolicy::SanitizedWithCustomNoBaseline(vec![(
                        "PATH".to_string(),
                        "/custom/bin".to_string(),
                    )])
                )
            )
            .as_deref(),
            Some("/custom/bin"),
            "a sanitized policy PATH override must never be overwritten by the fallback"
        );
        assert_eq!(
            launched_path(
                ProcessLaunch::new("/bin/true", Vec::<String>::new())
                    .env_policy(EnvironmentPolicy::SanitizedWithCustomNoBaseline(Vec::new()))
            )
            .as_deref(),
            Some(FALLBACK_PATH),
            "a missing host PATH is filled so bare program names still resolve"
        );

        std::env::set_var("PATH", "");
        assert_eq!(
            launched_path(
                ProcessLaunch::new("/bin/true", Vec::<String>::new())
                    .env_policy(EnvironmentPolicy::SanitizedWithCustomNoBaseline(Vec::new()))
            )
            .as_deref(),
            Some(""),
            "an empty host PATH is present, not missing, and must be left alone"
        );

        match saved {
            Some(value) => std::env::set_var("PATH", value),
            None => std::env::remove_var("PATH"),
        }
    }

    /// The predicate behind the fallback, spelled out for every policy variant.
    #[test]
    fn only_policies_carrying_path_suppress_the_fallback() {
        assert!(!EnvironmentPolicy::SanitizedMinimal.sets_path());
        assert!(!EnvironmentPolicy::Explicit(Vec::new()).sets_path());
        assert!(!EnvironmentPolicy::SanitizedWithCustom(vec![("CI".to_string(), "true".to_string())])
            .sets_path());
        assert!(
            EnvironmentPolicy::Explicit(vec![("PATH".to_string(), "/x".to_string())]).sets_path()
        );
        assert!(EnvironmentPolicy::SanitizedWithCustom(vec![(
            "PATH".to_string(),
            "/x".to_string()
        )])
        .sets_path());
        assert!(EnvironmentPolicy::SanitizedWithCustomNoBaseline(vec![(
            "PATH".to_string(),
            "/x".to_string()
        )])
        .sets_path());
    }
}
