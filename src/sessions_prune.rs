//! Delete old saved conversations: `kroma sessions prune --older-than DAYS`.
//!
//! A conversation is a group of files that share an id (`<id>.json`,
//! `<id>.json.wal`, `<id>.last.json`, `<id>.catalog.lock`). Its age is taken
//! from the newest file, so a conversation used a moment ago survives any
//! cutoff, and nothing outside that naming pattern is ever touched.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};

#[derive(Debug, Default, PartialEq, Eq)]
pub struct PruneReport {
    pub sessions: usize,
    pub files: usize,
    pub bytes: u64,
}

const SUFFIXES: [&str; 4] = [".catalog.lock", ".json.wal", ".last.json", ".json"];

fn session_id(name: &str) -> Option<&str> {
    SUFFIXES
        .iter()
        .find_map(|suffix| name.strip_suffix(suffix))
        .filter(|id| !id.is_empty())
}

/// Remove every conversation whose newest file is at least `older_than` old.
pub fn prune(dir: &Path, older_than: Duration, now: SystemTime) -> Result<PruneReport> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(PruneReport::default());
        }
        Err(error) => {
            return Err(error).with_context(|| format!("cannot read {}", dir.display()));
        }
    };
    let mut groups: BTreeMap<String, Vec<(PathBuf, SystemTime, u64)>> = BTreeMap::new();
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let Some(id) = path.file_name().and_then(|name| name.to_str()).and_then(session_id) else {
            continue;
        };
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        let modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        groups
            .entry(id.to_owned())
            .or_default()
            .push((path, modified, metadata.len()));
    }
    let mut report = PruneReport::default();
    for files in groups.into_values() {
        let newest = files
            .iter()
            .map(|(_, modified, _)| *modified)
            .max()
            .unwrap_or(SystemTime::UNIX_EPOCH);
        if !now.duration_since(newest).is_ok_and(|age| age >= older_than) {
            continue;
        }
        let mut deleted = 0;
        for (path, _, bytes) in &files {
            match fs::remove_file(path) {
                Ok(()) => {
                    report.files += 1;
                    report.bytes += bytes;
                    deleted += 1;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error).with_context(|| format!("cannot delete {}", path.display()));
                }
            }
        }
        if deleted > 0 {
            report.sessions += 1;
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str) {
        fs::write(dir.join(name), b"x").unwrap();
    }

    #[test]
    fn a_session_is_every_file_that_shares_its_id() {
        assert_eq!(session_id("00ff.json"), Some("00ff"));
        assert_eq!(session_id("00ff.json.wal"), Some("00ff"));
        assert_eq!(session_id("00ff.last.json"), Some("00ff"));
        assert_eq!(session_id("00ff.catalog.lock"), Some("00ff"));
        assert_eq!(session_id("notes.txt"), None);
        assert_eq!(session_id(".json"), None);
    }

    #[test]
    fn prune_removes_sessions_and_leaves_other_files_alone() {
        let temp = std::env::temp_dir().join(format!("kroma-prune-{}", std::process::id()));
        let _ = fs::remove_dir_all(&temp);
        fs::create_dir_all(&temp).unwrap();
        write(&temp, "aa.json");
        write(&temp, "aa.json.wal");
        write(&temp, "aa.last.json");
        write(&temp, "bb.json");
        write(&temp, "notes.txt");

        let now = SystemTime::now();
        let report = prune(&temp, Duration::from_secs(0), now).unwrap();
        assert_eq!(report.sessions, 2);
        assert_eq!(report.files, 4);
        assert!(!temp.join("aa.json").exists());
        assert!(!temp.join("bb.json").exists());
        assert!(temp.join("notes.txt").exists());
        fs::remove_dir_all(&temp).unwrap();
    }

    #[test]
    fn a_recent_session_survives_a_long_cutoff() {
        let temp = std::env::temp_dir().join(format!("kroma-keep-{}", std::process::id()));
        let _ = fs::remove_dir_all(&temp);
        fs::create_dir_all(&temp).unwrap();
        write(&temp, "cc.json");

        let report = prune(&temp, Duration::from_secs(60 * 60 * 24), SystemTime::now()).unwrap();
        assert_eq!(report, PruneReport::default());
        assert!(temp.join("cc.json").exists());
        fs::remove_dir_all(&temp).unwrap();
    }
}
