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

    pub fn from_tokio_child(child: &tokio::process::Child) -> Self {
        let pgid = child.id().unwrap_or(0) as i32;
        #[cfg(windows)]
        {
            let job = WindowsJobObject::new().ok().and_then(|j| {
                if let Some(raw) = child.raw_handle() {
                    let _ = j.assign_raw_process(raw as HANDLE);
                    Some(j)
                } else {
                    None
                }
            });
            return Self { pgid, job };
        }
        #[cfg(not(windows))]
        Self { pgid }
    }

    pub fn from_std_child(child: &std::process::Child) -> Self {
        let pgid = child.id() as i32;
        #[cfg(windows)]
        {
            let job = WindowsJobObject::new().ok().map(|j| {
                let _ = j.assign_raw_process(child.as_raw_handle() as HANDLE);
                j
            });
            return Self { pgid, job };
        }
        #[cfg(not(windows))]
        Self { pgid }
    }

    pub fn pgid(&self) -> i32 {
        self.pgid
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
        #[cfg(windows)]
        if let Some(ref job) = self.job {
            job.terminate(1);
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

/// Resolve the shell binary and argument vector used by `bash` on the current host.
pub fn resolve_shell_invocation(command: &str) -> (String, Vec<String>) {
    #[cfg(unix)]
    {
        if let Ok(custom) = std::env::var("HARNESS_SHELL") {
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
        let harness_shell = std::env::var("HARNESS_SHELL").ok();
        let path_env = std::env::var_os("PATH");
        let comspec = std::env::var("ComSpec")
            .or_else(|_| std::env::var("COMSPEC"))
            .ok();
        resolve_windows_shell_with_env(
            command,
            harness_shell.as_deref(),
            path_env.as_deref(),
            comspec.as_deref(),
            |p| p.is_file(),
        )
    }
}

/// Deterministic, unit-testable Windows shell resolution:
/// 1. `HARNESS_SHELL` environment override
/// 2. Git Bash (`bash.exe` on `PATH` or `C:\Program Files\Git\bin\bash.exe`)
/// 3. PowerShell (`pwsh.exe` or `powershell.exe` on `PATH` with `-NoLogo -NoProfile -NonInteractive -Command`)
/// 4. `cmd.exe /D /S /C`
pub fn resolve_windows_shell_with_env<F>(
    command: &str,
    harness_shell: Option<&str>,
    path_env: Option<&OsStr>,
    comspec: Option<&str>,
    file_exists: F,
) -> (String, Vec<String>)
where
    F: Fn(&Path) -> bool,
{
    if let Some(raw_shell) = harness_shell {
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
    } else if matches!(leaf.as_str(), "cmd" | "cmd.exe") {
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
