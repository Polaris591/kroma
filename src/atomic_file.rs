//! Durable replacement of a written file, shared by profiles, the session
//! catalog and session snapshots. A reader must never observe a half-written
//! file, and a crash must leave either the old or the new content.
use anyhow::{Context, Result};
use std::path::Path;

/// Replace `target` with the already-synced `temp` file in one step.
///
/// On Windows this goes through `MoveFileExW` so the swap is write-through and
/// tolerates a transient sharing violation; on Unix a same-directory `rename`
/// is already atomic. Callers fsync the temporary file first.
pub(crate) fn replace(temp: &Path, target: &Path) -> Result<()> {
    #[cfg(windows)]
    crate::winsys::replace_file(temp, target).with_context(|| {
        format!(
            "failed to atomically replace {} (temporary file {})",
            target.display(),
            temp.display()
        )
    })?;
    #[cfg(not(windows))]
    std::fs::rename(temp, target)
        .with_context(|| format!("failed to atomically replace {}", target.display()))?;
    Ok(())
}
