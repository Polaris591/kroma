//! OS sidecar locks: hold the file for the transaction and never unlink it.
use std::{
    fs::{File, OpenOptions},
    path::Path,
    time::{Duration, Instant},
};
pub(crate) fn acquire(path: &Path, timeout: Duration) -> std::io::Result<File> {
    let started = Instant::now();
    loop {
        match try_acquire(path) {
            Ok(file) => return Ok(file),
            Err(error) if contended(&error) && started.elapsed() < timeout => {
                std::thread::sleep(Duration::from_millis(5))
            }
            Err(error) => return Err(error),
        }
    }
}
pub(crate) fn contended(error: &std::io::Error) -> bool {
    #[cfg(windows)]
    {
        matches!(error.raw_os_error(), Some(32 | 33))
    }
    #[cfg(not(windows))]
    {
        error.kind() == std::io::ErrorKind::WouldBlock
    }
}
pub(crate) fn try_acquire(path: &Path) -> std::io::Result<File> {
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn independent_opens_contend_and_release_without_truncation() -> std::io::Result<()> {
        let path = std::env::temp_dir().join(format!(
            "kroma-lock-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path, b"retained")?;
        let first = acquire(&path, Duration::ZERO)?;
        let error = acquire(&path, Duration::from_millis(10)).unwrap_err();
        assert!(contended(&error));
        drop(first);
        drop(acquire(&path, Duration::ZERO)?);
        assert_eq!(std::fs::read(&path)?, b"retained");
        std::fs::remove_file(path)?;
        Ok(())
    }
}
