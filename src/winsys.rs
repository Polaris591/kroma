//! Windows operations unavailable through safe std/Tokio APIs.
//! FFI stays here; owned handles close on all error paths. Shell startup fails
//! closed if job assignment fails: an uncontained process is never resumed.
use std::{
    io,
    os::windows::{
        ffi::OsStrExt,
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
    },
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
};
use windows_sys::Win32::{
    Foundation::{HANDLE, INVALID_HANDLE_VALUE},
    Storage::FileSystem::{MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH},
    System::{
        Console::{
            GetConsoleMode, GetStdHandle, SetConsoleMode, ENABLE_ECHO_INPUT, STD_INPUT_HANDLE,
        },
        Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
        },
        JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
            SetInformationJobObject, TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        },
        Threading::{OpenThread, ResumeThread, CREATE_SUSPENDED, THREAD_SUSPEND_RESUME},
    },
};

fn usable(handle: HANDLE) -> bool {
    !handle.is_null() && handle != INVALID_HANDLE_VALUE
}

/// Owns the process tree until successful shell completion.
pub(crate) struct ChildTreeGuard {
    job: Option<OwnedHandle>,
    armed: bool,
}

/// Jobs from successful commands stay alive for the lifetime of this Kroma
/// process. Closing the guard handle would otherwise enforce
/// KILL_ON_JOB_CLOSE immediately, even though a Unix shell command that starts
/// a background process has already succeeded. Windows does not allow that
/// flag to be cleared after creation. Keeping one handle means background work
/// outlives its shell command but not Kroma; a force-killed Kroma still closes
/// every handle and terminates contained descendants.
static DISARMED_JOBS: OnceLock<Mutex<Vec<OwnedHandle>>> = OnceLock::new();

fn retain_disarmed_job(job: OwnedHandle) {
    const MAX_RETAINED_JOBS: usize = 128;
    let jobs = DISARMED_JOBS.get_or_init(|| Mutex::new(Vec::new()));
    if let Ok(mut jobs) = jobs.lock() {
        jobs.push(job);
        if jobs.len() > MAX_RETAINED_JOBS {
            // Dropping the oldest job ends only very old background work.
            // This bounds handle growth without changing the newest command's
            // lifecycle.
            jobs.remove(0);
        }
    }
}

impl ChildTreeGuard {
    /// Spawn suspended, assign the job, then resume the initial thread. There
    /// is no user-code execution window outside the job. Tokio does not expose
    /// the initial thread handle, so obtain it from a thread snapshot while the
    /// process is still suspended (it cannot have created other threads yet).
    pub(crate) fn spawn(
        command: &mut tokio::process::Command,
    ) -> io::Result<(tokio::process::Child, Self)> {
        // SAFETY: null security/name pointers request default, unnamed ownership.
        let raw = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if !usable(raw) {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: CreateJobObjectW returned a fresh owned handle, wrapped once.
        let job = unsafe { OwnedHandle::from_raw_handle(raw) };
        let guard = Self {
            job: Some(job),
            armed: true,
        };
        // If the owning Kroma process is force-killed (for example when its
        // parent plugin is cancelled), dropping OS handles must also kill
        // the contained PowerShell/descendants. Rust Drop won't run then.
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let set = unsafe {
            SetInformationJobObject(
                guard
                    .job
                    .as_ref()
                    .expect("fresh job handle")
                    .as_raw_handle(),
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if set == 0 {
            return Err(io::Error::last_os_error());
        }
        command.creation_flags(CREATE_SUSPENDED).kill_on_drop(true);
        let mut child = command.spawn()?;
        let setup = (|| {
            let process = child
                .raw_handle()
                .ok_or_else(|| io::Error::other("shell handle unavailable"))?;
            // SAFETY: both handles remain owned/live throughout this call; the
            // child was created by this process with assignment access.
            if unsafe {
                AssignProcessToJobObject(
                    guard
                        .job
                        .as_ref()
                        .expect("fresh job handle")
                        .as_raw_handle(),
                    process,
                )
            } == 0
            {
                return Err(io::Error::last_os_error());
            }
            resume_initial_thread(
                child
                    .id()
                    .ok_or_else(|| io::Error::other("shell PID unavailable"))?,
            )
        })();
        if let Err(error) = setup {
            // In particular, terminate an unassigned suspended child on failure.
            // kill_on_drop remains a second guard, and Tokio owns reaping.
            let _ = child.start_kill();
            return Err(error);
        }
        Ok((child, guard))
    }

    pub(crate) fn disarm(&mut self) {
        self.armed = false;
        // Transfer the job handle to the process-lifetime registry instead of
        // closing it. KILL_ON_JOB_CLOSE remains set (Windows ignores attempts
        // to clear it), so closing the last handle would kill background work.
        if let Some(job) = self.job.take() {
            retain_disarmed_job(job);
        }
    }

    pub(crate) fn terminate(&self) {
        if self.armed {
            if let Some(job) = &self.job {
                // SAFETY: job is exclusively owned and live; no user memory passed.
                unsafe { TerminateJobObject(job.as_raw_handle(), 1) };
            }
        }
    }
}

fn resume_initial_thread(pid: u32) -> io::Result<()> {
    // SAFETY: snapshot flags require no pointers; returned handle is owned.
    let raw = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if !usable(raw) {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fresh valid snapshot handle, transferred exactly once.
    let snapshot = unsafe { OwnedHandle::from_raw_handle(raw) };
    let mut entry = THREADENTRY32 {
        dwSize: u32::try_from(std::mem::size_of::<THREADENTRY32>())
            .expect("THREADENTRY32 fits DWORD"),
        ..Default::default()
    };
    // SAFETY: entry is writable and has the required structure size; snapshot is live.
    let mut found = unsafe { Thread32First(snapshot.as_raw_handle(), &mut entry) } != 0;
    while found {
        if entry.th32OwnerProcessID == pid {
            // SAFETY: no pointers; requests only resume rights to the enumerated thread.
            let raw = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID) };
            if !usable(raw) {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: fresh valid thread handle, owned by this scope.
            let thread = unsafe { OwnedHandle::from_raw_handle(raw) };
            // SAFETY: live thread handle has resume access; child has not run yet.
            if unsafe { ResumeThread(thread.as_raw_handle()) } == u32::MAX {
                return Err(io::Error::last_os_error());
            }
            return Ok(());
        }
        entry.dwSize =
            u32::try_from(std::mem::size_of::<THREADENTRY32>()).expect("THREADENTRY32 fits DWORD");
        // SAFETY: same live snapshot and writable, size-initialized structure.
        found = unsafe { Thread32Next(snapshot.as_raw_handle(), &mut entry) } != 0;
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "suspended shell thread not found",
    ))
}

impl Drop for ChildTreeGuard {
    fn drop(&mut self) {
        if self.armed {
            if let Some(job) = &self.job {
                // SAFETY: job is exclusively owned and live; no user memory passed.
                // Termination is nonblocking; the option then closes the job.
                unsafe { TerminateJobObject(job.as_raw_handle(), 1) };
            }
        }
    }
}

/// Hides console echo while a secret is typed. `Drop` restores the original
/// mode even when the caller panics or the prompt is interrupted.
pub(crate) struct HiddenConsoleInput {
    console: HANDLE,
    previous: u32,
}

impl HiddenConsoleInput {
    pub(crate) fn enter() -> io::Result<Self> {
        // SAFETY: constant identifies the borrowed process stdin handle.
        let console = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
        if !usable(console) {
            return Err(io::Error::last_os_error());
        }
        let mut previous = 0u32;
        // SAFETY: validated handle; previous is writable for the duration.
        if unsafe { GetConsoleMode(console, &mut previous) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: handle is live; mode derives from GetConsoleMode.
        if unsafe { SetConsoleMode(console, previous & !ENABLE_ECHO_INPUT) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { console, previous })
    }
}

impl Drop for HiddenConsoleInput {
    fn drop(&mut self) {
        // SAFETY: borrowed stdin remains live; restore the mode read on entry.
        unsafe { SetConsoleMode(self.console, self.previous) };
    }
}

/// Replace `target` with the already-synced `temp` file.
///
/// `MoveFileExW` is used instead of `std::fs::rename` because it supports
/// `MOVEFILE_WRITE_THROUGH`, which is the Windows equivalent of the directory
/// fsync Unix needs before the rename is durable. Sharing violations from an
/// antivirus or indexer scanning the fresh file are retried briefly.
pub(crate) fn replace_file(temp: &Path, target: &Path) -> io::Result<()> {
    let source = wide(temp)?;
    let destination = wide(target)?;
    let flags = MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH;
    let mut attempt = 0u64;
    loop {
        // SAFETY: both owned vectors are NUL-terminated, NUL-free paths and
        // remain alive for the call. Flags are documented MoveFileEx flags.
        if unsafe { MoveFileExW(source.as_ptr(), destination.as_ptr(), flags) } != 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        // 5 = ERROR_ACCESS_DENIED, 32 = ERROR_SHARING_VIOLATION.
        let retriable = matches!(error.raw_os_error(), Some(5) | Some(32));
        if !retriable || attempt >= 5 {
            return Err(error);
        }
        attempt += 1;
        std::thread::sleep(std::time::Duration::from_millis(20 * attempt));
    }
}

fn wide(path: &Path) -> io::Result<Vec<u16>> {
    let mut units: Vec<u16> = path.as_os_str().encode_wide().collect();
    if units.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "path contains a NUL byte",
        ));
    }
    units.push(0);
    Ok(units)
}

/// Directories to prepend to the child's `PATH` so a non-login Git Bash still
/// finds its own coreutils and `git.exe`.
pub(crate) fn bash_tool_dirs(bash: &Path) -> Vec<PathBuf> {
    let Some(bin) = bash.parent() else {
        return Vec::new();
    };
    let mut dirs = vec![bin.to_path_buf()];
    let usr_bin = bin.parent().map(|root| root.join("usr").join("bin"));
    if usr_bin.as_ref().is_some_and(|dir| dir.is_dir()) {
        dirs.extend(usr_bin);
    }
    dirs
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(label: &str) -> PathBuf {
        // cmd.exe treats parentheses as syntax, so keep the path plain.
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "kroma-winsys-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A successfully completed shell may intentionally start a background
    /// process. Disarming must clear kill-on-close, not just skip the explicit
    /// termination call.
    #[tokio::test]
    async fn successful_shell_can_leave_a_background_descendant() {
        let dir = scratch("background");
        let worker = dir.join("background.ps1");
        std::fs::write(
            &worker,
            "Set-Content -LiteralPath 'started.txt' -Value 'started'\r\nStart-Sleep -Milliseconds 1500\r\nSet-Content -LiteralPath 'survived.txt' -Value 'alive'\r\n",
        )
        .unwrap();
        let parent = dir.join("parent.ps1");
        std::fs::write(
            &parent,
            format!(
                "Start-Process -FilePath powershell -ArgumentList '-NoProfile','-NonInteractive','-ExecutionPolicy','Bypass','-File','{}' -WindowStyle Hidden\r\nStart-Sleep -Milliseconds 700\r\n",
                worker.display()
            ),
        )
        .unwrap();
        let mut command = tokio::process::Command::new("powershell");
        command
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
            ])
            .arg(&parent)
            .current_dir(&dir)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let (mut child, mut guard) = ChildTreeGuard::spawn(&mut command).unwrap();
        let status = child.wait().await.unwrap();
        assert!(status.success());
        assert!(
            dir.join("started.txt").is_file(),
            "background descendant never started inside the job"
        );
        guard.disarm();
        drop(guard);
        tokio::time::sleep(std::time::Duration::from_secs(4)).await;
        assert!(
            dir.join("survived.txt").is_file(),
            "a successful shell's background descendant was killed on job close"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The grandchild announces itself first, so a surviving marker proves
    /// containment failed rather than that the descendant never ran.
    #[tokio::test]
    async fn cancelling_a_shell_terminates_its_descendants() {
        let dir = scratch("job");
        // A batch file keeps quoting out of the test; it starts a real
        // grandchild, then stays alive until the guard is dropped.
        let script = dir.join("tree.bat");
        let mut text = String::new();
        for line in [
            r#"start /b cmd /c "echo grandchild> grandchild.txt & ping -n 4 127.0.0.1 >nul & echo survived> survived.txt""#,
            "ping -n 10 127.0.0.1 >nul",
        ] {
            text.push_str(line);
            text.push(char::from(13));
            text.push(char::from(10));
        }
        std::fs::write(&script, text).unwrap();
        let mut command = tokio::process::Command::new("cmd");
        command
            .arg("/c")
            // cmd treats parentheses in a path as syntax, so quote it.
            .raw_arg(format!("\"{}\"", script.display()))
            .current_dir(&dir)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let (mut child, guard) = ChildTreeGuard::spawn(&mut command).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !dir.join("grandchild.txt").is_file() {
            assert!(
                std::time::Instant::now() < deadline,
                "grandchild never started"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        // Dropping an armed guard models Ctrl+C before the shell completes.
        drop(guard);
        let _ = child.start_kill();
        let _ = child.wait().await;
        tokio::time::sleep(std::time::Duration::from_secs(4)).await;
        assert!(
            !dir.join("survived.txt").exists(),
            "descendant escaped the job"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn abrupt_owner_test_helper() {
        let Ok(folder) = std::env::var("KROMA_JOB_OWNER_HELPER") else {
            return;
        };
        let dir = PathBuf::from(folder);
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            let mut command = tokio::process::Command::new("cmd");
            command
                .arg("/c")
                .raw_arg(format!("\"{}\"", dir.join("tree.bat").display()))
                .current_dir(&dir)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            let (_shell, _guard) = ChildTreeGuard::spawn(&mut command).unwrap();
            std::fs::write(dir.join("owner-ready.txt"), "ready").unwrap();
            tokio::time::sleep(std::time::Duration::from_secs(25)).await;
        });
    }

    #[tokio::test]
    async fn force_killing_job_owner_terminates_its_shell_descendants() {
        let dir = scratch("abrupt-owner");
        let script = dir.join("tree.bat");
        let lines = [
            r#"start /b cmd /c "echo grandchild> grandchild.txt & ping -n 4 127.0.0.1 >nul & echo survived> survived.txt""#,
            "ping -n 10 127.0.0.1 >nul",
        ].join("\r\n") + "\r\n";
        std::fs::write(&script, lines).unwrap();
        let mut owner = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "winsys::tests::abrupt_owner_test_helper"])
            .env("KROMA_JOB_OWNER_HELPER", &dir)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !(dir.join("owner-ready.txt").is_file() && dir.join("grandchild.txt").is_file()) {
            if std::time::Instant::now() >= deadline {
                let _ = owner.kill();
                panic!("job owner's child tree never became ready");
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        owner.kill().unwrap();
        let _ = owner.wait();
        tokio::time::sleep(std::time::Duration::from_secs(4)).await;
        assert!(
            !dir.join("survived.txt").exists(),
            "force-killed job owner left its Shell descendant running"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn replace_file_overwrites_an_existing_target() {
        let dir = scratch("replace");
        let target = dir.join("target.json");
        let temp = dir.join("temp.json");
        std::fs::write(&target, b"old").unwrap();
        std::fs::write(&temp, b"new").unwrap();
        replace_file(&temp, &target).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"new");
        assert!(!temp.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn wide_rejects_an_embedded_nul() {
        use std::os::windows::ffi::OsStringExt;
        let path = PathBuf::from(std::ffi::OsString::from_wide(&[97, 0, 98]));
        assert_eq!(wide(&path).unwrap_err().kind(), io::ErrorKind::InvalidInput);
    }
}
