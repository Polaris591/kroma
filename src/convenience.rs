//! Small user-facing commands built on existing Shell/session primitives.
use anyhow::{bail, Context, Result};
use std::{
    fs,
    path::{Path, PathBuf},
};

/// Kept while saved conversations may refer to it; users may remove logs
/// once those references are no longer needed. Never persist with umask defaults.
pub fn save_pipe_input(bytes: &[u8]) -> Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .context("HOME is unavailable; cannot privately store piped input")?;
    crate::private_file::save(&home.join(".kroma/pipe-inputs"), "stdin", "log", bytes)
}

pub fn file_reference(text: &str, workdir: &Path) -> Result<String> {
    let mut output = String::new();
    for span in text.split_inclusive(char::is_whitespace) {
        let word = span.trim_end_matches(char::is_whitespace);
        let suffix = &span[word.len()..];
        let (core, trailing) = word.split_at(
            word.trim_end_matches(|ch: char| {
                matches!(ch, ',' | ';' | ':' | ')' | ']' | '}' | '。' | '，' | '；')
            })
            .len(),
        );
        if let Some(path) = core.strip_prefix('@') {
            if !path.is_empty() {
                let candidate = workdir.join(path);
                if candidate.is_file() {
                    let full = candidate.canonicalize()?;
                    output.push_str(&format!(
                        "File {} (read with Shell as needed)",
                        crate::path_display::for_shell(&full)
                    ));
                    output.push_str(trailing);
                    output.push_str(suffix);
                    continue;
                }
            }
        }
        output.push_str(word);
        output.push_str(suffix);
    }
    Ok(output)
}

pub fn suggestions(workdir: &Path, prefix: &str) -> Result<Vec<String>> {
    let partial = prefix.strip_prefix('@').unwrap_or(prefix);
    let path = Path::new(partial);
    // A trailing separator means "enter this directory", not "prefix-match
    // its name". Path::parent() would otherwise select the parent and use the
    // directory itself as the needle.
    let entering_directory = partial.ends_with('/') || partial.ends_with('\\');
    let folder = if entering_directory {
        workdir.join(path)
    } else {
        workdir.join(path.parent().unwrap_or(Path::new("")))
    };
    let needle = if entering_directory {
        String::new()
    } else {
        path.file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned()
    };
    let mut found = Vec::new();
    if folder.is_dir() {
        for entry in fs::read_dir(&folder)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with(needle.as_str()) && !name.starts_with('.') {
                let suffix = if entry.file_type()?.is_dir() { "/" } else { "" };
                let relative = entry
                    .path()
                    .strip_prefix(workdir)
                    .unwrap_or(&entry.path())
                    .to_string_lossy()
                    .into_owned();
                found.push(format!("@{relative}{suffix}"));
            }
        }
    }
    found.sort();
    found.truncate(16);
    Ok(found)
}

/// Bytes of `git diff` read before the command is reported as truncated.
const MAX_DIFF_BYTES: usize = 96 * 1024;
/// Bytes of `git` diagnostics kept for error reporting.
const MAX_GIT_STDERR_BYTES: usize = 16 * 1024;
/// Characters shown inline; the full read stays bounded separately.
const MAX_DIFF_DISPLAY_CHARS: usize = 12_000;
/// A read-only observation must not hang the host forever.
const GIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Run one read-only git command with bounded pipes and a deadline. Dropping
/// the caller kills the child instead of leaving git behind a closed command,
/// and a chatty stderr can never block stdout.
async fn run_git(workdir: &Path, args: &[&str]) -> Result<String> {
    use tokio::io::AsyncReadExt;
    let mut command = tokio::process::Command::new("git");
    command
        .args(args)
        .current_dir(workdir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let mut child = command.spawn().context("running git")?;
    let mut stdout = child.stdout.take().context("git stdout unavailable")?;
    let mut stderr = child.stderr.take().context("git stderr unavailable")?;
    let mut out_reader = (&mut stdout).take(MAX_DIFF_BYTES as u64 + 1);
    let mut err_reader = (&mut stderr).take(MAX_GIT_STDERR_BYTES as u64);
    let read = async {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let (out_result, err_result) = tokio::join!(
            out_reader.read_to_end(&mut out),
            err_reader.read_to_end(&mut err),
        );
        out_result?;
        err_result?;
        Ok::<(Vec<u8>, Vec<u8>), std::io::Error>((out, err))
    };
    let (out, err) = match tokio::time::timeout(GIT_TIMEOUT, read).await {
        Ok(result) => result.context("reading git output")?,
        Err(_) => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            bail!(
                "git {} did not finish within {} seconds",
                args.join(" "),
                GIT_TIMEOUT.as_secs()
            );
        }
    };
    let truncated = out.len() > MAX_DIFF_BYTES;
    let mut text = String::from_utf8_lossy(&out[..out.len().min(MAX_DIFF_BYTES)]).into_owned();
    let status = if truncated {
        // Stop git rather than wait for a writer blocked on an unread pipe.
        let _ = child.start_kill();
        None
    } else {
        Some(child.wait().await.context("waiting for git")?)
    };
    if let Some(status) = status {
        if !status.success() {
            bail!("{}", String::from_utf8_lossy(&err).trim().to_owned());
        }
    }
    if truncated {
        text.push_str(&format!(
            "\n[... git output truncated at {MAX_DIFF_BYTES} bytes ...]"
        ));
    }
    Ok(text)
}

/// Describe the working tree without blocking the host: a slow repository or a
/// stuck git process ends in a clear message instead of a frozen interface.
pub async fn git_diff(workdir: &Path) -> Result<String> {
    // A repository without commits has no HEAD; compare the worktree instead
    // of failing every /diff in a freshly initialized project.
    let has_head = run_git(workdir, &["rev-parse", "--verify", "HEAD"])
        .await
        .is_ok();
    let diff = if has_head {
        run_git(workdir, &["diff", "--no-ext-diff", "HEAD"]).await?
    } else {
        run_git(workdir, &["diff", "--no-ext-diff"]).await?
    };
    let untracked = run_git(workdir, &["ls-files", "--others", "--exclude-standard"]).await?;
    let mut summary = String::new();
    if !untracked.trim().is_empty() {
        summary.push_str("Untracked files (not in git diff):\n");
        summary.push_str(&untracked);
    }
    summary.push_str(&diff);
    let summary = summary.trim_end();
    if summary.is_empty() {
        return Ok("No file differences.".into());
    }
    Ok(if summary.chars().count() > MAX_DIFF_DISPLAY_CHARS {
        let shown: String = summary.chars().take(MAX_DIFF_DISPLAY_CHARS).collect();
        format!("{shown}\n[... truncated for display ...]")
    } else {
        summary.to_owned()
    })
}
pub fn export_session(
    messages: &[crate::protocol::Message],
    workdir: &Path,
    name: &str,
) -> Result<PathBuf> {
    if name.contains('/') || name.contains('\\') || name.contains("..") || name.is_empty() {
        bail!("export name must be a simple filename");
    }
    let path = workdir.join(name);
    if path.exists() {
        bail!("export file already exists: {}", path.display());
    }
    let text = if name.ends_with(".json") {
        serde_json::to_string_pretty(messages)?
    } else if name.ends_with(".md") {
        messages
            .iter()
            .filter(|m| m.role == "user" || m.role == "assistant")
            .map(|m| format!("### {}\n\n{}\n", m.role, m.content.as_deref().unwrap_or("")))
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        bail!("export filename must end with .md or .json")
    };
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create_new(true);
    let mut f = opts.open(&path)?;
    use std::io::Write;
    f.write_all(text.as_bytes())?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn scratch(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kroma-convenience-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn git_diff_reports_untracked_files_asynchronously() {
        let dir = scratch("diff");
        let initialized = std::process::Command::new("git")
            .arg("init")
            .arg("--quiet")
            .current_dir(&dir)
            .status();
        if initialized.map(|status| !status.success()).unwrap_or(true) {
            fs::remove_dir_all(&dir).ok();
            return;
        }
        fs::write(dir.join("new-file.txt"), "hello\n").unwrap();
        let diff = git_diff(&dir).await.expect("diff");
        assert!(diff.contains("Untracked files"), "{diff}");
        assert!(diff.contains("new-file.txt"), "{diff}");
        fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn git_diff_outside_a_repository_reports_the_git_error() {
        let dir = scratch("no-repo");
        let error = git_diff(&dir).await.expect_err("git must refuse");
        assert!(!format!("{error:#}").trim().is_empty());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn file_references_keep_newlines_and_missing_names_unchanged() -> Result<()> {
        let root = std::env::temp_dir().join(format!("kroma-file-ref-{}", std::process::id()));
        fs::create_dir_all(root.join("src"))?;
        fs::write(root.join("src/agent.rs"), "// code")?;
        let input = "请检查 @src/agent.rs\n第二行 @missing.rs  保留空格\n还有 @src/agent.rs，";
        let projected = file_reference(input, &root)?;
        // The projection embeds the canonical path, which uses `\` separators
        // on Windows.
        let canonical = root.join("src/agent.rs").canonicalize()?;
        assert!(projected.contains(&format!(
            "{} (read with Shell as needed)\n",
            crate::path_display::for_shell(&canonical)
        )));
        assert!(projected.contains("第二行 @missing.rs  保留空格\n"));
        assert!(projected.ends_with("(read with Shell as needed)，"));
        assert_eq!(
            suggestions(&root, "@src/age")?,
            vec![format!("@src{}agent.rs", std::path::MAIN_SEPARATOR)]
        );
        fs::remove_dir_all(root)?;
        Ok(())
    }
    #[test]
    fn suggestions_enter_directories_and_filter_hidden_or_excess_candidates() -> Result<()> {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let root = std::env::temp_dir().join(format!("kroma-suggestions-{nonce}"));
        fs::create_dir_all(root.join("src"))?;
        fs::write(root.join("src/agent.rs"), "// code")?;
        fs::write(root.join("srcfile.txt"), "sibling")?;
        fs::write(root.join(".hidden"), "hidden")?;
        for i in 0..18 {
            fs::write(root.join(format!("bulk-{i:02}.txt")), "fixture")?;
        }
        let entered = suggestions(&root, "@src/")?;
        assert_eq!(entered.len(), 1, "only files inside src: {entered:?}");
        assert_eq!(entered[0].replace('\\', "/"), "@src/agent.rs");
        let all = suggestions(&root, "@")?;
        assert_eq!(all.len(), 16, "suggestions remain capped: {all:?}");
        assert!(
            all.iter().all(|item| !item.contains(".hidden")),
            "hidden entries stay filtered: {all:?}"
        );
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn export_refuses_overwrite_and_writes_chat_without_tools() -> Result<()> {
        let root = std::env::temp_dir().join(format!("kroma-export-{}", std::process::id()));
        fs::create_dir_all(&root)?;
        let messages = vec![
            crate::protocol::Message::new("user", "hello"),
            crate::protocol::Message::new("tool", "private tool output"),
            crate::protocol::Message::new("assistant", "world"),
        ];
        let path = export_session(&messages, &root, "session.md")?;
        let content = fs::read_to_string(path)?;
        assert!(content.contains("hello") && content.contains("world"));
        assert!(!content.contains("private tool output"));
        assert!(export_session(&messages, &root, "session.md").is_err());
        assert!(export_session(&messages, &root, "../escape.md").is_err());
        fs::remove_dir_all(root)?;
        Ok(())
    }
}
