//! Private, create-new storage for user-supplied attachments and piped logs.
use anyhow::{bail, Result};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

static SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub(crate) fn save(folder: &Path, prefix: &str, suffix: &str, bytes: &[u8]) -> Result<PathBuf> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(folder)?;
    // Never redirect a private store through a pre-existing symlink.
    if !fs::symlink_metadata(folder)?.file_type().is_dir() {
        bail!(
            "private storage must be a real directory: {}",
            folder.display()
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(folder, fs::Permissions::from_mode(0o700))?;
    }
    let time = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let path = folder.join(format!(
        "{prefix}-{}-{time:x}-{sequence:x}.{suffix}",
        std::process::id()
    ));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&path)?;
    if let Err(error) = file.write_all(bytes).and_then(|_| file.sync_all()) {
        drop(file);
        let _ = fs::remove_file(&path);
        return Err(error.into());
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_storage_is_unique_and_rejects_symlink_directory() -> Result<()> {
        let root = std::env::temp_dir().join(format!(
            "kroma-private-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
        ));
        let folder = root.join("nested/private");
        let first = save(&folder, "stdin", "log", b"one")?;
        let second = save(&folder, "stdin", "log", b"two")?;
        assert_ne!(first, second);
        assert_eq!(fs::read(&first)?, b"one");
        #[cfg(unix)]
        {
            use std::os::unix::fs::{symlink, PermissionsExt};
            assert_eq!(fs::metadata(&first)?.permissions().mode() & 0o777, 0o600);
            assert_eq!(fs::metadata(&folder)?.permissions().mode() & 0o777, 0o700);
            symlink(&folder, root.join("linked"))?;
            assert!(save(&root.join("linked"), "stdin", "log", b"no").is_err());
        }
        fs::remove_dir_all(root)?;
        Ok(())
    }
}
