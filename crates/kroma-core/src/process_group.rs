//! Cross-platform process group & Windows Job Object process-tree containment.
//!
//! On Unix, child processes are placed in a dedicated process group (`setpgid(0, 0)`) and swept
//! via `kill(-pgid, SIGTERM)` followed by `SIGKILL`.
//! On Windows, child processes are spawned with `CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW` and
//! assigned to an anonymous kernel Job Object configured with
//! `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_BREAKAWAY_OK`, then terminated atomically
//! via `TerminateJobObject` + `CloseHandle` so no grandchild process can outlive the command or
//! hold inherited stdout/stderr pipes open.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::Duration;
#[cfg(unix)]
use std::time::Instant;

#[cfg(windows)]
use std::os::windows::io::AsRawHandle;
#[cfg(windows)]
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
#[cfg(windows)]
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_BREAKAWAY_OK, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
#[cfg(windows)]
use windows_sys::Win32::System::Threading::{CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW};

/// Windows kernel Job Object handle with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`.
#[cfg(windows)]
#[derive(Debug)]
pub struct WindowsJobObject {
    handle: HANDLE,
}

#[cfg(windows)]
// Safety: Win32 job object HANDLE is process-wide and thread-safe for Assign/Terminate/Close.
unsafe impl Send for WindowsJobObject {}
#[cfg(windows)]
unsafe impl Sync for WindowsJobObject {}

#[cfg(windows)]
impl WindowsJobObject {
    pub fn new() -> std::io::Result<Self> {
        let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if handle.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        info.BasicLimitInformation.LimitFlags =
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_BREAKAWAY_OK;
        let ok = unsafe {
            SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const std::ffi::c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if ok == 0 {
            let err = std::io::Error::last_os_error();
            unsafe {
                let _ = CloseHandle(handle);
            }
            return Err(err);
        }
        Ok(Self { handle })
    }

    pub fn assign_raw_process(&self, process_handle: HANDLE) -> std::io::Result<()> {
        if process_handle.is_null() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "null process handle",
            ));
        }
        let ok = unsafe { AssignProcessToJobObject(self.handle, process_handle) };
        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }

    pub fn terminate(&self, exit_code: u32) {
        unsafe {
            let _ = TerminateJobObject(self.handle, exit_code);
        }
    }
}

#[cfg(windows)]
impl Drop for WindowsJobObject {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            unsafe {
                let _ = CloseHandle(self.handle);
            }
        }
    }
}

/// Platform containment handle attached to a spawned child: a Windows Job Object, or `()` on Unix
/// where containment is the process group set at spawn time.
#[cfg(windows)]
pub(crate) type PlatformContainment = WindowsJobObject;
#[cfg(not(windows))]
pub(crate) type PlatformContainment = ();

/// How a launch attaches platform process-tree containment to an already-spawned child.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Containment {
    /// Real platform containment: Unix process group (`setpgid`); Windows Job Object
    /// (`CreateJobObject` + `AssignProcessToJobObject`).
    #[default]
    Platform,
    /// Test-only seam: force the containment attach to fail so the fail-closed stop+reap contract
    /// can be exercised on every platform. Never set in production.
    #[doc(hidden)]
    ForceAttachFailure,
}

/// Cross-platform handle representing a spawned child's process group (Unix) or Job Object (Windows).
#[derive(Debug)]
pub struct ChildProcessGroup {
    pgid: i32,
    #[cfg(windows)]
    job: Option<WindowsJobObject>,
}

impl ChildProcessGroup {
    pub fn configure_tokio_command(cmd: &mut tokio::process::Command) {
        #[cfg(unix)]
        {
            cmd.process_group(0);
        }
        #[cfg(windows)]
        {
            cmd.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
        }
    }

    pub fn configure_std_command(cmd: &mut std::process::Command) {
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
        }
    }

    /// Best-effort containment for a spawned child.
    ///
    /// On Windows this is **fail-open**: if the Job Object cannot be created or the child cannot be
    /// assigned to it, the returned handle carries no containment and every sweep/terminate call
    /// becomes a silent no-op. Every production spawn (the shared launch seam, `bash`, the
    /// completion gate, command hooks) uses [`Self::try_from_tokio_child`] /
    /// [`Self::try_from_std_child`] instead; only tests still build a handle this way.
    pub fn from_tokio_child(child: &tokio::process::Child) -> Self {
        let pgid = child.id().unwrap_or(0) as i32;
        #[cfg(windows)]
        {
            let job = WindowsJobObject::new().ok().and_then(|j| {
                let raw = child.raw_handle()?;
                j.assign_raw_process(raw as HANDLE).ok().map(|()| j)
            });
            return Self { pgid, job };
        }
        #[cfg(not(windows))]
        Self { pgid }
    }

    /// Best-effort containment for a synchronously spawned child. Fail-open on Windows; see
    /// [`Self::from_tokio_child`].
    pub fn from_std_child(child: &std::process::Child) -> Self {
        let pgid = child.id() as i32;
        #[cfg(windows)]
        {
            let job = WindowsJobObject::new().ok().and_then(|j| {
                j.assign_raw_process(child.as_raw_handle() as HANDLE)
                    .ok()
                    .map(|()| j)
            });
            return Self { pgid, job };
        }
        #[cfg(not(windows))]
        Self { pgid }
    }

    /// Attach containment to a spawned child, failing closed.
    ///
    /// On Windows, if the Job Object cannot be created or the child cannot be assigned to it, the
    /// child is stopped and reaped before the error is returned, so a caller never receives an
    /// uncontained process. On Unix containment is the process group already set at spawn time and
    /// this always succeeds.
    ///
    /// Window race (recorded, not solved here): a child can execute and spawn descendants between
    /// `spawn` and the job assignment, and those descendants are not retroactively contained. Fully
    /// closing that window needs a suspended spawn / job-constrained creation, tracked as B15.
    pub fn try_from_tokio_child(
        child: &mut tokio::process::Child,
        containment: Containment,
    ) -> std::io::Result<Self> {
        let pgid = child.id().unwrap_or(0) as i32;
        match Self::try_attach(pgid, containment, || attach_job_tokio(child)) {
            Ok(group) => Ok(group),
            Err(err) => {
                // Fail closed: a child we could not contain must not keep running unsupervised.
                let _ = child.start_kill();
                reap_tokio_child(child);
                Err(err)
            }
        }
    }

    /// Attach containment to a synchronously spawned child, failing closed (stop + reap on
    /// Windows failure). See [`Self::try_from_tokio_child`].
    pub fn try_from_std_child(
        child: &mut std::process::Child,
        containment: Containment,
    ) -> std::io::Result<Self> {
        let pgid = child.id() as i32;
        match Self::try_attach(pgid, containment, || attach_job_std(child)) {
            Ok(group) => Ok(group),
            Err(err) => {
                let _ = child.kill();
                let _ = child.wait();
                Err(err)
            }
        }
    }

    /// Shared attach core: run `attach` unless the caller asked to force a failure.
    fn try_attach<F>(pgid: i32, containment: Containment, attach: F) -> std::io::Result<Self>
    where
        F: FnOnce() -> std::io::Result<PlatformContainment>,
    {
        if containment == Containment::ForceAttachFailure {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                "forced containment failure (test seam)",
            ));
        }
        #[cfg(windows)]
        {
            let job = attach()?;
            Ok(Self { pgid, job: Some(job) })
        }
        #[cfg(not(windows))]
        {
            attach()?;
            Ok(Self { pgid })
        }
    }

    pub fn pgid(&self) -> i32 {
        self.pgid
    }

    /// Check whether a specific PID is currently alive.
    pub fn is_process_alive(pid: i32) -> bool {
        #[cfg(unix)]
        {
            pid > 0 && unsafe { libc::kill(pid, 0) == 0 }
        }
        #[cfg(not(unix))]
        {
            // Windows liveness is not a POSIX `kill(0)` probe, and this layer owns no process
            // handle for an arbitrary pid; report only "not provably alive" and leave containment
            // to the Job Object above.
            let _ = pid;
            false
        }
    }

    /// Immediately terminate the entire process group / Job Object.
    pub fn terminate_immediately(&self) {
        #[cfg(unix)]
        if self.pgid > 1 {
            unsafe {
                let _ = libc::kill(-self.pgid, libc::SIGKILL);
            }
        }
        #[cfg(windows)]
        if let Some(ref job) = self.job {
            job.terminate(1);
        }
    }

    /// Synchronously sweep the process group / Job Object with a short grace period.
    pub fn sweep_sync(&self, grace: Duration) {
        #[cfg(unix)]
        if self.pgid > 1 {
            unsafe {
                let _ = libc::kill(-self.pgid, libc::SIGTERM);
            }
            std::thread::sleep(grace);
            unsafe {
                let _ = libc::kill(-self.pgid, libc::SIGKILL);
            }
        }
        #[cfg(windows)]
        {
            let _ = grace;
            if let Some(ref job) = self.job {
                job.terminate(1);
            }
        }
    }

    /// Asynchronously terminate the process group / Job Object on timeout, cancellation, or flood cap.
    pub async fn terminate_gracefully(
        &self,
        child: &mut tokio::process::Child,
        grace_period: Duration,
    ) {
        #[cfg(unix)]
        if self.pgid > 1 {
            unsafe {
                let _ = libc::kill(-self.pgid, libc::SIGTERM);
            }
        }

        if tokio::time::timeout(grace_period, child.wait())
            .await
            .is_err()
        {
            self.terminate_immediately();
            let _ = child.kill().await;
            let _ = child.wait().await;
        } else {
            self.terminate_immediately();
        }
    }

    /// Sweep any lingering background descendants after the direct child leader has exited.
    pub async fn cleanup_after_exit(&self) {
        #[cfg(unix)]
        if self.pgid > 1 {
            let alive = unsafe { libc::kill(-self.pgid, 0) == 0 };
            if alive {
                unsafe {
                    let _ = libc::kill(-self.pgid, libc::SIGTERM);
                }
                let deadline = Instant::now() + Duration::from_millis(100);
                while Instant::now() < deadline {
                    if unsafe { libc::kill(-self.pgid, 0) != 0 } {
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                if unsafe { libc::kill(-self.pgid, 0) == 0 } {
                    unsafe {
                        let _ = libc::kill(-self.pgid, libc::SIGKILL);
                    }
                    for _ in 0..5 {
                        if unsafe { libc::kill(-self.pgid, 0) != 0 } {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                }
            }
        }
        #[cfg(windows)]
        if let Some(ref job) = self.job {
            job.terminate(0);
        }
    }
}

/// Bounded synchronous reap of a tokio child after `start_kill` on the fail-closed path. Only runs
/// when containment could not be attached, so it never blocks the steady state.
fn reap_tokio_child(child: &mut tokio::process::Child) {
    let deadline = std::time::Instant::now() + Duration::from_millis(200);
    while std::time::Instant::now() < deadline {
        if matches!(child.try_wait(), Ok(Some(_))) {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Attach a Windows Job Object to an already-spawned tokio child. Unix is a no-op.
#[cfg(windows)]
fn attach_job_tokio(child: &tokio::process::Child) -> std::io::Result<PlatformContainment> {
    let job = WindowsJobObject::new()?;
    let raw = child.raw_handle().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::Other,
            "child handle unavailable for job assignment",
        )
    })?;
    job.assign_raw_process(raw as HANDLE)?;
    Ok(job)
}

#[cfg(not(windows))]
fn attach_job_tokio(_child: &tokio::process::Child) -> std::io::Result<PlatformContainment> {
    Ok(())
}

/// Attach a Windows Job Object to an already-spawned synchronous child. Unix is a no-op.
#[cfg(windows)]
fn attach_job_std(child: &std::process::Child) -> std::io::Result<PlatformContainment> {
    let job = WindowsJobObject::new()?;
    job.assign_raw_process(child.as_raw_handle() as HANDLE)?;
    Ok(job)
}

#[cfg(not(windows))]
fn attach_job_std(_child: &std::process::Child) -> std::io::Result<PlatformContainment> {
    Ok(())
}

/// Resolve the shell binary and argument vector used by `bash` on the current host.
pub fn resolve_shell_invocation(command: &str) -> (String, Vec<String>) {
    #[cfg(unix)]
    {
        if let Ok(custom) = std::env::var("KROMA_SHELL") {
            let trimmed = custom.trim();
            if !trimmed.is_empty() {
                return format_shell_args(trimmed, command);
            }
        }
        let shell_bin = if Path::new("/bin/bash").exists() {
            "/bin/bash"
        } else {
            "/bin/sh"
        };
        (
            shell_bin.to_string(),
            vec!["-c".to_string(), command.to_string()],
        )
    }
    #[cfg(windows)]
    {
        let kroma_shell = std::env::var("KROMA_SHELL").ok();
        let path_env = std::env::var_os("PATH");
        let comspec = std::env::var("ComSpec")
            .or_else(|_| std::env::var("COMSPEC"))
            .ok();
        resolve_windows_shell_with_env(
            command,
            kroma_shell.as_deref(),
            path_env.as_deref(),
            comspec.as_deref(),
            |p| p.is_file(),
        )
    }
}

/// Deterministic, unit-testable Windows shell resolution:
/// 1. `KROMA_SHELL` environment override
/// 2. Git Bash (`bash.exe` on `PATH` or `C:\Program Files\Git\bin\bash.exe`)
/// 3. PowerShell (`pwsh.exe` or `powershell.exe` on `PATH` with `-NoLogo -NoProfile -NonInteractive -Command`)
/// 4. `cmd.exe /D /S /C`
pub fn resolve_windows_shell_with_env<F>(
    command: &str,
    kroma_shell: Option<&str>,
    path_env: Option<&OsStr>,
    comspec: Option<&str>,
    file_exists: F,
) -> (String, Vec<String>)
where
    F: Fn(&Path) -> bool,
{
    if let Some(raw_shell) = kroma_shell {
        let trimmed = raw_shell.trim();
        if !trimmed.is_empty() {
            return format_shell_args(trimmed, command);
        }
    }

    // 1. Look for bash.exe on PATH or standard Git for Windows locations
    if let Some(found) = find_executable_in_path("bash.exe", path_env, &file_exists) {
        return (
            found.to_string_lossy().to_string(),
            vec!["-c".to_string(), command.to_string()],
        );
    }
    for candidate in [
        r"C:\Program Files\Git\bin\bash.exe",
        r"C:\Program Files\Git\usr\bin\bash.exe",
    ] {
        if file_exists(Path::new(candidate)) {
            return (
                candidate.to_string(),
                vec!["-c".to_string(), command.to_string()],
            );
        }
    }

    // 2. Look for pwsh.exe or powershell.exe on PATH
    for ps_bin in ["pwsh.exe", "powershell.exe"] {
        if let Some(found) = find_executable_in_path(ps_bin, path_env, &file_exists) {
            return format_shell_args(&found.to_string_lossy(), command);
        }
    }

    // 3. Fallback to ComSpec / cmd.exe
    let cmd_bin = comspec
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("cmd.exe");
    format_shell_args(cmd_bin, command)
}

fn find_executable_in_path<F>(
    exe_name: &str,
    path_env: Option<&OsStr>,
    file_exists: &F,
) -> Option<PathBuf>
where
    F: Fn(&Path) -> bool,
{
    let raw = path_env?;
    let text = raw.to_string_lossy();
    let sep = if text.contains(';') { ';' } else { ':' };
    for dir in text.split(sep) {
        let clean = dir.trim().trim_matches('"');
        if clean.is_empty() {
            continue;
        }
        let candidate = if clean.contains('\\') {
            PathBuf::from(format!(
                "{}\\{}",
                clean.trim_end_matches(['/', '\\']),
                exe_name
            ))
        } else {
            Path::new(clean).join(exe_name)
        };
        if file_exists(&candidate) {
            return Some(candidate);
        }
    }
    None
}

fn format_shell_args(shell_bin: &str, command: &str) -> (String, Vec<String>) {
    let leaf = shell_bin
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(shell_bin)
        .to_ascii_lowercase();
    if matches!(
        leaf.as_str(),
        "pwsh" | "pwsh.exe" | "powershell" | "powershell.exe"
    ) {
        (
            shell_bin.to_string(),
            vec![
                "-NoLogo".to_string(),
                "-NoProfile".to_string(),
                "-NonInteractive".to_string(),
                "-Command".to_string(),
                command.to_string(),
            ],
        )
    } else if is_cmd_exe(&leaf) {
        (
            shell_bin.to_string(),
            vec![
                "/D".to_string(),
                "/S".to_string(),
                "/C".to_string(),
                command.to_string(),
            ],
        )
    } else {
        (
            shell_bin.to_string(),
            vec!["-c".to_string(), command.to_string()],
        )
    }
}

/// True when `shell_bin` refers to `cmd` or `cmd.exe`.
pub fn is_cmd_exe(shell_bin: &str) -> bool {
    let leaf = shell_bin
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(shell_bin)
        .to_ascii_lowercase();
    matches!(leaf.as_str(), "cmd" | "cmd.exe")
}

/// Attach a resolved shell invocation to a synchronous command.
///
/// On Windows with `cmd.exe`, the whole raw command must be passed as one verbatim
/// `/D /S /C "<raw_command>"` argument so inner double quotes are never mangled by MSVCRT `\"`
/// escaping. Every other shell and every other platform uses the pre-split `shell_args` unchanged;
/// no additional quoting is added here.
pub(crate) fn apply_shell_args(
    cmd: &mut std::process::Command,
    shell_bin: &str,
    shell_args: &[String],
    raw_command: &str,
) {
    #[cfg(windows)]
    if is_cmd_exe(shell_bin) {
        use std::os::windows::process::CommandExt;
        cmd.raw_arg(format!("/D /S /C \"{}\"", raw_command));
        return;
    }
    let _ = (shell_bin, raw_command);
    cmd.args(shell_args);
}

/// Attach `shell_args` (or on Windows with `cmd.exe`, a verbatim `/D /S /C "<raw_command>"` raw
/// argument) so inner double quotes are never mangled by MSVCRT `\"` escaping.
pub fn apply_shell_args_tokio(
    cmd: &mut tokio::process::Command,
    shell_bin: &str,
    shell_args: &[String],
    raw_command: &str,
) {
    apply_shell_args(cmd.as_std_mut(), shell_bin, shell_args, raw_command);
}
