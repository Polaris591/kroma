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
                        full.display()
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
    let folder = workdir.join(path.parent().unwrap_or(Path::new("")));
    let needle = path.file_name().unwrap_or_default().to_string_lossy();
    let mut found = Vec::new();
    if folder.is_dir() {
        for entry in fs::read_dir(&folder)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with(needle.as_ref()) && !name.starts_with('.') {
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

pub fn git_diff(workdir: &Path) -> Result<String> {
    let output = std::process::Command::new("git")
        .arg("diff")
        .arg("--no-ext-diff")
        .arg("HEAD")
        .current_dir(workdir)
        .output()
        .context("running git diff")?;
    if !output.status.success() {
        bail!("{}", String::from_utf8_lossy(&output.stderr));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let new_files = std::process::Command::new("git")
        .args(["ls-files", "--others", "--exclude-standard"])
        .current_dir(workdir)
        .output()
        .context("listing untracked files")?;
    if !new_files.status.success() {
        bail!("{}", String::from_utf8_lossy(&new_files.stderr));
    }
    let untracked = String::from_utf8_lossy(&new_files.stdout);
    let mut summary = String::new();
    if !untracked.is_empty() {
        summary.push_str("Untracked files (not in git diff):\n");
        summary.push_str(&untracked);
    }
    if !text.is_empty() {
        summary.push_str(&text);
    }
    Ok(if summary.is_empty() {
        "No file differences.".into()
    } else {
        summary.chars().take(12_000).collect()
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
            canonical.display()
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
