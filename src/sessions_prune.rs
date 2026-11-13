//! Prune confirmed, inactive session snapshots and their owned evidence.
//! Shared catalog maps, lock files and artifacts of recoverable trashed
//! sessions are not conversations and must never be aged independently.
use anyhow::{Context, Result};
use serde::Deserialize;
use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

#[derive(Debug, Default, PartialEq, Eq)]
pub struct PruneReport {
    pub sessions: usize,
    pub files: usize,
    pub bytes: u64,
}

#[derive(Deserialize)]
struct SnapshotIdentity {
    version: u32,
    workdir: String,
    messages: Vec<serde::de::IgnoredAny>,
}

fn is_snapshot(path: &Path) -> bool {
    fs::File::open(path)
        .ok()
        .and_then(|file| {
            serde_json::from_reader::<_, SnapshotIdentity>(std::io::BufReader::new(file)).ok()
        })
        .is_some_and(|saved| {
            (1..=3).contains(&saved.version)
                && !saved.workdir.is_empty()
                && !saved.messages.is_empty()
        })
}

fn owned_files(dir: &Path, id: &str) -> Result<Vec<(PathBuf, SystemTime, u64)>> {
    let snapshot = format!("{id}.json");
    let wal = format!("{id}.json.wal");
    let outputs = format!("{id}.tool-output-");
    let mut files = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name != snapshot && name != wal && !name.starts_with(&outputs) {
            continue;
        }
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.is_file() {
            // Unknown age is not permission to delete evidence.
            if let Ok(modified) = metadata.modified() {
                files.push((entry.path(), modified, metadata.len()));
            } else {
                return Ok(Vec::new());
            }
        }
    }
    Ok(files)
}

pub fn prune(dir: &Path, older_than: Duration, now: SystemTime) -> Result<PruneReport> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(PruneReport::default())
        }
        Err(error) => return Err(error).with_context(|| format!("cannot read {}", dir.display())),
    };
    let mut snapshots = Vec::new();
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_file()
            && path.extension().is_some_and(|ext| ext == "json")
            && is_snapshot(&path)
        {
            snapshots.push(path);
        }
    }
    let mut report = PruneReport::default();
    for snapshot in snapshots {
        // Same OS lock as the Agent. Never unlink lock files: on Unix that
        // would permit separate locks on two inodes for one conversation.
        let Ok(_lock) = crate::session::SessionStore::open_at(snapshot.clone(), Path::new("."))
        else {
            continue;
        };
        if !is_snapshot(&snapshot) {
            continue;
        }
        let Some(id) = snapshot.file_stem().and_then(|id| id.to_str()) else {
            continue;
        };
        // Re-read all ages after taking the lock, not from the initial scan.
        let files = owned_files(dir, id)?;
        let Some(newest) = files.iter().map(|(_, time, _)| *time).max() else {
            continue;
        };
        if !now
            .duration_since(newest)
            .is_ok_and(|age| age >= older_than)
        {
            continue;
        }
        let mut deleted = false;
        for (path, _, bytes) in files {
            match fs::remove_file(&path) {
                Ok(()) => {
                    report.files += 1;
                    report.bytes += bytes;
                    deleted = true;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error).with_context(|| format!("cannot delete {}", path.display()))
                }
            }
        }
        report.sessions += usize::from(deleted);
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn directory(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kroma-prune-{label}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }
    fn snapshot(dir: &Path, id: &str) {
        fs::write(dir.join(format!("{id}.json")), br#"{"version":3,"workdir":"workspace","messages":[{"role":"system","content":"system"}]}"#).unwrap();
    }
    #[test]
    fn foundation_prune_preserves_catalogs_locks_and_trash_evidence() {
        let dir = directory("catalog");
        snapshot(&dir, "aa");
        fs::write(dir.join("aa.json.wal"), b"").unwrap();
        fs::write(dir.join("aa.tool-output-tool.txt"), b"owned").unwrap();
        for name in [
            "aa.names.json",
            "aa.models.json",
            "aa.usage.json",
            "aa.capabilities.json",
            "aa.last.json",
            "aa.catalog.lock",
            "bb.tool-output-tool.txt",
            "notes.txt",
        ] {
            fs::write(dir.join(name), b"{}").unwrap();
        }
        let report = prune(&dir, Duration::ZERO, SystemTime::now()).unwrap();
        assert_eq!(report.sessions, 1);
        assert_eq!(report.files, 3);
        assert!(dir.join("aa.models.json").exists());
        assert!(dir.join("aa.capabilities.json").exists());
        assert!(dir.join("aa.catalog.lock").exists());
        assert!(
            dir.join("bb.tool-output-tool.txt").exists(),
            "no live snapshot: may belong to trash"
        );
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn foundation_prune_skips_live_and_recent_sessions() {
        let dir = directory("live");
        snapshot(&dir, "aa");
        let lock =
            crate::session::SessionStore::open_at(dir.join("aa.json"), Path::new("workspace"))
                .unwrap();
        assert_eq!(
            prune(&dir, Duration::ZERO, SystemTime::now())
                .unwrap()
                .sessions,
            0
        );
        drop(lock);
        assert_eq!(
            prune(&dir, Duration::from_secs(86_400), SystemTime::now())
                .unwrap()
                .sessions,
            0
        );
        assert_eq!(
            prune(&dir, Duration::ZERO, SystemTime::now())
                .unwrap()
                .sessions,
            1
        );
        fs::remove_dir_all(dir).unwrap();
    }
}
