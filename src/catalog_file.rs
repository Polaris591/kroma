//! Catalog transactions: the sidecar lock protects read-modify-write across
//! processes; atomic replacement protects readers. Never unlink the lock file.
use anyhow::{Context, Result};
use std::{
    fs::{self, File, OpenOptions},
    path::Path,
    time::{Duration, Instant},
};

/// Bounded synchronous transaction. The closure must not await or re-enter.
pub(crate) fn update<T: serde::Serialize>(
    catalog: &Path,
    target: &Path,
    change: impl FnOnce() -> Result<T>,
) -> Result<()> {
    let _lock = acquire(&catalog.with_extension("catalog.lock"))?;
    let value = change()?;
    let parent = target.parent().context("catalog path has no parent")?;
    let temp = crate::private_file::save(
        parent,
        ".catalog",
        "tmp",
        &serde_json::to_vec_pretty(&value)?,
    )?;
    let result = crate::atomic_file::replace(&temp, target);
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result?;
    #[cfg(unix)]
    File::open(parent)?.sync_all()?;
    Ok(())
}

fn acquire(path: &Path) -> Result<File> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match try_acquire(path) {
            Ok(file) => return Ok(file),
            Err(error) if contended(&error) && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(5))
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("cannot lock catalog {}", path.display()))
            }
        }
    }
}

fn contended(error: &std::io::Error) -> bool {
    #[cfg(windows)]
    {
        error.raw_os_error() == Some(32)
    }
    #[cfg(not(windows))]
    {
        error.kind() == std::io::ErrorKind::WouldBlock
    }
}

fn try_acquire(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.share_mode(0);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path)?;
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        // SAFETY: owned live descriptor, no pointers. Closing this independently
        // opened descriptor releases the lock, including on error paths.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(file)
}
