//! Read-only working-tree review for the Desktop. It never writes, stages,
//! resets, reverts or attributes existing changes to an Agent task.
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::{
    io::Read,
    path::{Component, Path},
    process::{Command, Stdio},
};

const MAX_STATUS_BYTES: usize = 256 * 1024;
const MAX_DIFF_BYTES: usize = 192 * 1024;
const MAX_ENTRIES: usize = 200;

/// Run git without a shell or visible Windows console, and bound output before
/// it reaches the app. Do not block a UI/agent runtime mutex while doing I/O.
fn git(project: &Path, args: &[&str], max_bytes: usize) -> Result<(Vec<u8>, bool)> {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(project)
        .arg("-c")
        .arg("core.quotepath=false")
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }

    let mut process = command.spawn().context("Git is not available")?;
    let mut stdout = process.stdout.take().context("Git stdout unavailable")?;
    let mut bytes = Vec::new();
    stdout
        .by_ref()
        .take((max_bytes + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > max_bytes {
        let _ = process.kill();
        let _ = process.wait();
        bytes.truncate(max_bytes);
        return Ok((bytes, true));
    }
    let result = process.wait()?;
    if !result.success() {
        bail!("Git command failed ({result})");
    }
    Ok((bytes, false))
}

fn status_entries(bytes: &[u8]) -> Vec<Value> {
    let items: Vec<&[u8]> = bytes
        .split(|byte| *byte == 0)
        .filter(|s| !s.is_empty())
        .collect();
    let mut result = Vec::new();
    let mut index = 0;
    while index < items.len() {
        let entry = items[index];
        index += 1;
        if entry.len() < 4 || entry[2] != b' ' {
            continue;
        }
        let code = &entry[..2];
        // Porcelain v1 -z emits destination then source for renames/copies.
        if code.iter().any(|byte| matches!(*byte, b'R' | b'C')) && index < items.len() {
            index += 1;
        }
        let untracked = code == b"??";
        let staged = code[0] != b' ' && code[0] != b'?';
        let path = String::from_utf8_lossy(&entry[3..]).into_owned();
        let status = if untracked {
            "untracked"
        } else if code.contains(&b'D') {
            "deleted"
        } else if code.contains(&b'R') {
            "renamed"
        } else if code.contains(&b'A') {
            "added"
        } else if code.contains(&b'U') {
            "conflict"
        } else {
            "modified"
        };
        result.push(json!({
            "path": path,
            "status": status,
            "staged": staged,
            "untracked": untracked,
        }));
    }
    result
}

fn repository_root(project: &Path) -> Result<String> {
    let (bytes, truncated) = git(project, &["rev-parse", "--show-toplevel"], 8192)?;
    if truncated {
        bail!("Repository path is unexpectedly long");
    }
    let root = String::from_utf8(bytes)?.trim().to_owned();
    if root.is_empty() {
        bail!("Git repository is not available");
    }
    Ok(root)
}

pub fn overview(project: &Path) -> Result<Value> {
    let root = match repository_root(project) {
        Ok(root) => root,
        Err(_) => {
            return Ok(json!({
                "is_repo":false,
                "root":null,
                "entries":[],
                "truncated":false,
                "message":"此目录不是 Git 仓库，暂时无法查看版本差异。",
            }));
        }
    };
    let (bytes, output_truncated) = git(
        Path::new(&root),
        &["status", "--porcelain=v1", "-z", "--untracked-files=normal"],
        MAX_STATUS_BYTES,
    )?;
    let mut entries = status_entries(&bytes);
    let total = entries.len();
    let truncated = output_truncated || total > MAX_ENTRIES;
    entries.truncate(MAX_ENTRIES);
    Ok(json!({
        "is_repo":true,
        "root":root,
        "entries":entries,
        "truncated":truncated,
        "message":null,
    }))
}

fn is_relative_file_path(path: &str) -> bool {
    let normalized = path.replace('\\', "/");
    !normalized.is_empty()
        && !normalized.contains('\0')
        && !normalized.starts_with('/')
        && !normalized.starts_with("//")
        && !normalized.contains(":/")
        && Path::new(&normalized)
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
}

pub fn file_diff(project: &Path, relative_path: &str) -> Result<Value> {
    if !is_relative_file_path(relative_path) {
        bail!("Invalid Git file path");
    }
    let overview = overview(project)?;
    let root = overview["root"].as_str().context("Not a Git repository")?;
    let entry = overview["entries"]
        .as_array()
        .and_then(|entries| {
            entries
                .iter()
                .find(|entry| entry["path"].as_str() == Some(relative_path))
        })
        .context("File is no longer in the working-tree change list")?;
    if entry["untracked"] == true {
        // A new file has no diff yet, so show its own content: reading it is the
        // same thing the agent already did when it wrote the file.
        let root = Path::new(root);
        let path = root.join(relative_path);
        let text = std::fs::read(&path)
            .ok()
            .map(|bytes| {
                let bytes = if bytes.len() > MAX_DIFF_BYTES {
                    bytes[..MAX_DIFF_BYTES].to_vec()
                } else {
                    bytes
                };
                String::from_utf8_lossy(&bytes).into_owned()
            })
            .unwrap_or_default();
        if text.contains('\0') {
            return Ok(json!({
                "path":relative_path,
                "diff":"",
                "truncated":false,
                "message":"二进制文件不显示内容预览。",
            }));
        }
        let mut diff = String::new();
        for line in text.lines() {
            diff.push('+');
            diff.push_str(line);
            diff.push('\n');
        }
        return Ok(json!({
            "path":relative_path,
            "diff":diff,
            "truncated":text.len() >= MAX_DIFF_BYTES,
            "message":if diff.is_empty(){Some("这是一个空文件。")}else{None},
        }));
    }
    let root = Path::new(root);
    let args = [
        "diff",
        "--no-ext-diff",
        "--no-color",
        "--unified=3",
        "--",
        relative_path,
    ];
    let (working, truncated_work) = git(root, &args, MAX_DIFF_BYTES)?;
    let cached_args = [
        "diff",
        "--cached",
        "--no-ext-diff",
        "--no-color",
        "--unified=3",
        "--",
        relative_path,
    ];
    let (staged, truncated_staged) = git(root, &cached_args, MAX_DIFF_BYTES)?;
    let mut result = String::new();
    if !staged.is_empty() {
        result.push_str("已暂存的变更\n");
        result.push_str(&String::from_utf8_lossy(&staged));
    }
    if !working.is_empty() {
        if !result.is_empty() {
            result.push_str("\n\n");
        }
        result.push_str("尚未暂存的变更\n");
        result.push_str(&String::from_utf8_lossy(&working));
    }
    Ok(json!({
        "path":relative_path,
        "diff":result,
        "truncated":truncated_work || truncated_staged,
        "message":if result.is_empty(){Some("此文件没有可显示的文本差异。")}else{None},
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handles_spaces_unicode_and_renames_without_inventing_extra_files() {
        let result = status_entries(
            b" M src/main.rs\0?? notes/new.txt\0R  newer.rs\0older.rs\0 M \xE4\xB8\xAD\xE6\x96\x87.txt\0",
        );
        assert_eq!(result.len(), 4);
        assert_eq!(result[0]["path"], "src/main.rs");
        assert_eq!(result[1]["status"], "untracked");
        assert_eq!(result[2]["path"], "newer.rs");
        assert_eq!(result[2]["status"], "renamed");
        assert_eq!(result[3]["path"], "中文.txt");
    }

    #[test]
    fn rejects_absolute_parent_and_empty_paths() {
        assert!(!is_relative_file_path("../outside"));
        assert!(!is_relative_file_path(""));
        assert!(!is_relative_file_path("C:/outside"));
        assert!(!is_relative_file_path("dir/../../outside"));
        assert!(!is_relative_file_path("dir\\\\..\\\\outside"));
        assert!(is_relative_file_path("src/agent.rs"));
        assert!(is_relative_file_path("中文 文件.txt"));
    }
}
