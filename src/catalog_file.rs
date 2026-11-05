//! Catalog transactions: the sidecar lock protects read-modify-write across
//! processes; atomic replacement protects readers. Never unlink the lock file.
use anyhow::{Context, Result};
use std::{
    fs::{self, File},
    path::Path,
    time::Duration,
};

/// Bounded synchronous transaction. The closure must not await or re-enter.
pub(crate) fn update<T: serde::Serialize>(
    catalog: &Path,
    target: &Path,
    change: impl FnOnce() -> Result<T>,
) -> Result<()> {
    update_and_return(catalog, target, || change().map(|value| (value, ())))
}

/// Like [`update`], but also returns a value computed inside the locked
/// read-modify-write section, such as the record that was just applied.
pub(crate) fn update_and_return<T: serde::Serialize, R>(
    catalog: &Path,
    target: &Path,
    change: impl FnOnce() -> Result<(T, R)>,
) -> Result<R> {
    let _lock = acquire(&catalog.with_extension("catalog.lock"))?;
    let (value, result) = change()?;
    let parent = target.parent().context("catalog path has no parent")?;
    let temp = crate::private_file::save(
        parent,
        ".catalog",
        "tmp",
        &serde_json::to_vec_pretty(&value)?,
    )?;
    let written = crate::atomic_file::replace(&temp, target);
    if written.is_err() {
        let _ = fs::remove_file(&temp);
    }
    written?;
    #[cfg(unix)]
    File::open(parent)?.sync_all()?;
    Ok(result)
}

fn acquire(path: &Path) -> Result<File> {
    crate::file_lock::acquire(path, Duration::from_secs(5))
        .with_context(|| format!("cannot lock catalog {}", path.display()))
}
