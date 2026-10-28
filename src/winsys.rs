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
        JobObjects::{AssignProcessToJobObject, CreateJobObjectW, TerminateJobObject},
        Threading::{OpenThread, ResumeThread, CREATE_SUSPENDED, THREAD_SUSPEND_RESUME},
    },
};

fn usable(handle: HANDLE) -> bool {
    !handle.is_null() && handle != INVALID_HANDLE_VALUE
}

/// Owns the process tree until successful shell completion.
pub(crate) struct ChildTreeGuard {
    job: OwnedHandle,
    armed: bool,
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
        let guard = Self { job, armed: true };
        command.creation_flags(CREATE_SUSPENDED).kill_on_drop(true);
        let mut child = command.spawn()?;
        let setup = (|| {
            let process = child
                .raw_handle()
                .ok_or_else(|| io::Error::other("shell handle unavailable"))?;
            // SAFETY: both handles remain owned/live throughout this call; the
            // child was created by this process with assignment access.
            if unsafe { AssignProcessToJobObject(guard.job.as_raw_handle(), process) } == 0 {
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
            // SAFETY: job is exclusively owned and live; no user memory passed.
            // Termination is nonblocking; OwnedHandle then closes the job.
            unsafe { TerminateJobObject(self.job.as_raw_handle(), 1) };
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
