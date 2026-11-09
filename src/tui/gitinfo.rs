//! Best-effort, bounded Git observations for presentation only. Never called by draw.
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::process::Command;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Changes {
    pub files: usize,
    pub added: u64,
    pub removed: u64,
    pub untracked: usize,
}
#[derive(Clone, Debug, Default)]
pub(super) struct Info {
    pub branch: Option<String>,
    pub dirty: bool,
}
#[derive(Clone, Debug)]
pub(super) struct Snapshot {
    stats: BTreeMap<String, (u64, u64)>,
    untracked: BTreeSet<String>,
}

/// Walk to .git without invoking Git, including linked-worktree gitdir files.
fn git_dir(workdir: &Path) -> Option<PathBuf> {
    for dir in workdir.ancestors() {
        let git = dir.join(".git");
        if git.is_dir() {
            return Some(git);
        }
        if git.is_file() {
            let text = std::fs::read_to_string(&git).ok()?;
            let path = PathBuf::from(text.trim().strip_prefix("gitdir: ")?);
            return Some(if path.is_absolute() {
                path
            } else {
                dir.join(path)
            });
        }
    }
    None
}
fn branch(workdir: &Path) -> Option<String> {
    let head = std::fs::read_to_string(git_dir(workdir)?.join("HEAD")).ok()?;
    let head = head.trim();
    if let Some(reference) = head.strip_prefix("ref: refs/heads/") {
        Some(reference.into())
    } else if head.len() >= 7 && head.bytes().all(|b| b.is_ascii_hexdigit()) {
        Some(head.chars().take(7).collect())
    } else {
        None
    }
}

async fn output(workdir: &Path, args: &[&str]) -> Option<Vec<u8>> {
    output_before(workdir, args, Duration::from_millis(1500)).await
}
async fn output_before(workdir: &Path, args: &[&str], deadline: Duration) -> Option<Vec<u8>> {
    let mut command = Command::new("git");
    command
        .args(args)
        .current_dir(workdir)
        // Presentation reads must never race the agent's own git commands for
        // index.lock.
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    #[cfg(windows)]
    command.creation_flags(0x08000000); // CREATE_NO_WINDOW, matching hidden utility processes.
    let result = tokio::time::timeout(deadline, command.output())
        .await
        .ok()?
        .ok()?;
    result.status.success().then_some(result.stdout)
}

fn numstat(bytes: &[u8]) -> Option<BTreeMap<String, (u64, u64)>> {
    let text = std::str::from_utf8(bytes).ok()?;
    let mut fields = text.split_terminator('\0');
    let mut stats = BTreeMap::new();
    while let Some(record) = fields.next() {
        let mut parts = record.splitn(3, '\t');
        let added = parts.next()?;
        let removed = parts.next()?;
        let mut name = parts.next()?;
        if name.is_empty() {
            fields.next()?;
            name = fields.next()?;
        }
        // Binary changes carry no line counts, but still count as changed files.
        let counts = if added == "-" && removed == "-" {
            (0, 0)
        } else {
            (added.parse().ok()?, removed.parse().ok()?)
        };
        stats.insert(name.to_owned(), counts);
    }
    Some(stats)
}

pub(super) async fn snapshot(workdir: &Path) -> Option<Snapshot> {
    observe(workdir).await?.1
}

pub(super) async fn observe(workdir: &Path) -> Option<(Info, Option<Snapshot>)> {
    let dir = workdir.to_owned();
    let head = tokio::task::spawn_blocking(move || branch(&dir));
    let (status, stats, untracked) = tokio::join!(
        output(workdir, &["status", "--porcelain", "-z"]),
        output(workdir, &["diff", "--numstat", "-z", "HEAD", "--"]),
        output(
            workdir,
            &["ls-files", "--others", "--exclude-standard", "-z"]
        )
    );
    let info = Info {
        branch: head.await.ok().flatten(),
        dirty: !status?.is_empty(),
    };
    let snapshot = (|| {
        Some(Snapshot {
            stats: numstat(&stats?)?,
            untracked: std::str::from_utf8(&untracked?)
                .ok()?
                .split_terminator('\0')
                .map(str::to_owned)
                .collect(),
        })
    })();
    Some((info, snapshot))
}

/// A missing/failed baseline is unknown, never an invented zero-change summary.
pub(super) fn changes(before: Option<&Snapshot>, after: Option<&Snapshot>) -> Option<Changes> {
    let (before, after) = (before?, after?);
    let mut changes = Changes::default();
    for path in before
        .stats
        .keys()
        .chain(after.stats.keys())
        .collect::<BTreeSet<_>>()
    {
        let a = before.stats.get(path).copied().unwrap_or_default();
        let b = after.stats.get(path).copied().unwrap_or_default();
        if before.stats.get(path) != after.stats.get(path) {
            changes.files += 1;
            changes.added += b.0.saturating_sub(a.0) + a.1.saturating_sub(b.1);
            changes.removed += b.1.saturating_sub(a.1) + a.0.saturating_sub(b.0);
        }
    }
    changes.untracked = after.untracked.difference(&before.untracked).count();
    Some(changes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, process::Command as Git};
    #[test]
    fn reads_branch_worktree_file_and_detached_head() {
        let dir = std::env::temp_dir().join(format!("kroma-git-head-{}", std::process::id()));
        fs::create_dir_all(dir.join("meta")).unwrap();
        fs::create_dir_all(dir.join("work")).unwrap();
        fs::write(dir.join("work/.git"), "gitdir: ../meta\n").unwrap();
        fs::write(dir.join("meta/HEAD"), "ref: refs/heads/feature/tui\n").unwrap();
        assert_eq!(branch(&dir.join("work")).as_deref(), Some("feature/tui"));
        fs::write(dir.join("meta/HEAD"), "0123456789abcdef\n").unwrap();
        assert_eq!(branch(&dir.join("work")).as_deref(), Some("0123456"));
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn parses_nul_paths_renames_binary_and_missing_baseline() {
        let stats = numstat(b"3\t1\tname with space\0-\t-\tbinary\0").unwrap();
        assert_eq!(stats["name with space"], (3, 1));
        assert_eq!(numstat(b"2\t0\t\0old\0new\0").unwrap()["new"], (2, 0));
        assert!(numstat(b"invalid").is_none());
        assert!(changes(None, None).is_none());
    }
    #[tokio::test]
    async fn repository_snapshot_reports_only_the_turn_delta() {
        if Git::new("git").arg("--version").output().is_err() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("kroma-git-delta-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        for args in [
            vec!["init", "-q"],
            vec!["config", "user.name", "test"],
            vec!["config", "user.email", "test@example.org"],
        ] {
            assert!(Git::new("git")
                .args(args)
                .current_dir(&dir)
                .status()
                .unwrap()
                .success());
        }
        fs::write(dir.join("file"), "first\n").unwrap();
        for args in [vec!["add", "file"], vec!["commit", "-qm", "base"]] {
            assert!(Git::new("git")
                .args(args)
                .current_dir(&dir)
                .status()
                .unwrap()
                .success());
        }
        fs::write(dir.join("file"), "first\npre-existing\n").unwrap();
        let before = snapshot(&dir).await.unwrap();
        fs::write(dir.join("file"), "first\npre-existing\nnew\n").unwrap();
        fs::write(dir.join("untracked"), "new").unwrap();
        let (info, after) = observe(&dir).await.unwrap();
        let after = after.unwrap();
        assert_eq!(
            changes(Some(&before), Some(&after)),
            Some(Changes {
                files: 1,
                added: 1,
                removed: 0,
                untracked: 1
            })
        );
        assert!(info.dirty);
        assert!(snapshot(&dir.join("missing")).await.is_none());
        assert!(changes(None, Some(&after)).is_none());
        fs::remove_dir_all(dir).unwrap();
    }
    #[tokio::test]
    async fn command_timeout_is_unknown_not_zero_changes() {
        if Git::new("git").arg("--version").output().is_err() {
            return;
        }
        let dir = std::env::temp_dir();
        let value = output_before(
            &dir,
            &["-c", "alias.pause=!sleep 2", "pause"],
            Duration::from_millis(20),
        )
        .await;
        assert!(value.is_none());
        assert!(changes(None, None).is_none());
    }
    #[tokio::test]
    async fn unborn_repository_keeps_branch_status_without_fabricated_diff_counts() {
        if Git::new("git").arg("--version").output().is_err() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("kroma-git-unborn-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        assert!(Git::new("git")
            .args(["init", "-q", "-b", "tui-test"])
            .current_dir(&dir)
            .status()
            .unwrap()
            .success());
        let (info, snapshot) = observe(&dir).await.unwrap();
        assert_eq!(info.branch.as_deref(), Some("tui-test"));
        assert!(snapshot.is_none());
        fs::remove_dir_all(dir).unwrap();
    }
}
