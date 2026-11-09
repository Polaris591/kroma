//! Model-facing file editing. A model changes code with one exact snippet
//! replacement instead of assembling heredocs or `sed` commands in the shell:
//! the edit fails loudly unless the original text occurs exactly once, and the
//! file is replaced atomically with its original line-ending style.
use crate::{
    path_display,
    protocol::ToolCall,
    tools::{Tool, ToolContext, ToolFuture, ToolResult},
};
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

static SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Deserialize)]
struct EditArgs {
    /// Path relative to the agent's working directory, or an absolute path
    /// inside it.
    path: String,
    /// Text to replace; must occur exactly once in the file.
    old: String,
    /// Replacement text; an empty string deletes the matched text.
    new: String,
}

pub struct FileEdit;

impl Tool for FileEdit {
    fn name(&self) -> &str {
        "edit_file"
    }

    fn definition(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": "edit_file",
                "description": "Replace one exact, uniquely occurring snippet in a text file inside the working directory, or create a new file by passing an empty `old`. Prefer this over shell heredoc/sed/perl when changing code: it fails without writing when `old` is missing or appears more than once, reports the line of the edit, preserves the file's CRLF or LF line endings and its mode.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "File path relative to the working directory, or an absolute path inside it; `..` is rejected"},
                        "old": {"type": "string", "description": "Exact original text to replace; must occur exactly once. Empty only when creating a new file."},
                        "new": {"type": "string", "description": "Replacement text; an empty string deletes the matched text. For a new file, the whole content."}
                    },
                    "required": ["path", "old", "new"],
                    "additionalProperties": false
                }
            }
        })
    }

    fn execute<'a>(&'a self, call: &'a ToolCall, ctx: ToolContext<'a>) -> ToolFuture<'a> {
        Box::pin(async move {
            match edit(call, ctx.workdir) {
                Ok(confirmation) => ToolResult::ok(confirmation.to_string()),
                Err(error) => ToolResult::error(json!({"error": format!("{error:#}")}).to_string()),
            }
        })
    }
}

fn edit(call: &ToolCall, workdir: &Path) -> Result<Value> {
    if call.kind != "function" || call.function.name != "edit_file" {
        bail!("unsupported tool: {}", call.function.name);
    }
    let args: EditArgs =
        serde_json::from_str(&call.function.arguments).context("invalid edit_file arguments")?;
    let (target, exists) = resolve_target(workdir, &args.path)?;
    if !exists {
        // Creating a file: an empty `old` means "no text to replace yet".
        if !args.old.is_empty() {
            bail!("file not found: {}", args.path);
        }
        let data = args.new.into_bytes();
        write_atomic(&target, &data, None)?;
        return Ok(json!({
            "path": args.path,
            "bytes": data.len(),
            "created": true,
        }));
    }
    if args.old.is_empty() {
        bail!("old must not be empty; pass the exact text to replace");
    }
    let bytes =
        fs::read(&target).with_context(|| format!("cannot read {}: file not found", args.path))?;
    let original = String::from_utf8(bytes)
        .with_context(|| format!("{} is not valid UTF-8 text", args.path))?;
    // Match on LF-normalized text so a CRLF file accepts snippets written with
    // either style, then restore the file's own endings before writing.
    let (content, old, new, crlf) = normalize(original, &args.old, &args.new);
    let occurrences = content.matches(&old).count();
    match occurrences {
        0 => bail!("old text not found in {}", args.path),
        1 => {}
        count => bail!(
            "old text appears {count} times in {}; include more surrounding context to make it unique",
            args.path
        ),
    }
    let position = content.find(&old).expect("counted occurrence");
    let line = content[..position].matches('\n').count() + 1;
    let replaced = format!(
        "{}{}{}",
        &content[..position],
        new,
        &content[position + old.len()..]
    );
    let data = if crlf {
        replaced.replace('\n', "\r\n").into_bytes()
    } else {
        replaced.into_bytes()
    };
    // A rewritten script must stay executable: reuse the file's own mode.
    #[cfg(unix)]
    let mode = {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(&target)
            .ok()
            .map(|meta| meta.permissions().mode())
    };
    #[cfg(not(unix))]
    let mode = None;
    write_atomic(&target, &data, mode)?;
    Ok(json!({
        "path": args.path,
        "bytes": data.len(),
        "line": line,
    }))
}

/// Split the file into match text and (optionally) a CRLF flag. Both the file
/// and the snippets are normalized together, so neither style can silently
/// produce mixed endings after the write.
fn normalize(original: String, old: &str, new: &str) -> (String, String, String, bool) {
    if !original.contains("\r\n") {
        return (original, old.to_owned(), new.to_owned(), false);
    }
    (
        original.replace("\r\n", "\n"),
        old.replace("\r\n", "\n"),
        new.replace("\r\n", "\n"),
        true,
    )
}

/// Resolve a model-supplied path inside the working directory. An absolute path
/// is accepted when it already points inside the workspace; symbolic links are
/// resolved so a link cannot redirect the write to an outside file. Returns the
/// resolved path and whether the file already exists.
fn resolve_target(workdir: &Path, raw: &str) -> Result<(PathBuf, bool)> {
    if raw.trim().is_empty() {
        bail!("path must not be empty");
    }
    let requested = Path::new(raw);
    if requested
        .components()
        .any(|part| matches!(part, Component::ParentDir))
    {
        bail!("path must not leave the working directory: {raw}");
    }
    let joined = if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        workdir.join(requested)
    };
    let root = workdir
        .canonicalize()
        .unwrap_or_else(|_| workdir.to_path_buf());
    match joined.canonicalize() {
        Ok(target) => {
            if path_display::strip_prefix_equivalent(&target, &root).is_none() {
                bail!("path resolves outside the working directory: {raw}");
            }
            if !target.is_file() {
                bail!("not a regular file: {raw}");
            }
            Ok((target, true))
        }
        Err(_) if joined.exists() => bail!("not a regular file: {raw}"),
        Err(_) => {
            // A new file: its existing directory is what must stay inside the
            // workspace, including through a link.
            let parent = joined.parent().context("path has no parent directory")?;
            let parent = parent
                .canonicalize()
                .with_context(|| format!("directory not found: {}", parent.display()))?;
            if path_display::strip_prefix_equivalent(&parent, &root).is_none() {
                bail!("path resolves outside the working directory: {raw}");
            }
            let name = joined.file_name().context("path has no file name")?;
            Ok((parent.join(name), false))
        }
    }
}

/// Write `data` through a synced same-directory temporary file so a reader or a
/// crash sees either the old or the new content, never a partial file.
fn write_atomic(target: &Path, data: &[u8], mode: Option<u32>) -> Result<()> {
    #[cfg(not(unix))]
    let _ = mode;
    let parent = target
        .parent()
        .context("edited file has no parent directory")?;
    let name = target.file_name().unwrap_or_default().to_string_lossy();
    let temp = parent.join(format!(
        ".{name}.{}-{}.edit.tmp",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let write = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .with_context(|| format!("cannot create temporary file {}", temp.display()))?;
        file.write_all(data)?;
        file.sync_all()?;
        drop(file);
        #[cfg(unix)]
        if let Some(mode) = mode {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&temp, fs::Permissions::from_mode(mode))?;
        }
        crate::atomic_file::replace(&temp, target)
    })();
    if write.is_err() {
        let _ = fs::remove_file(&temp);
    }
    write?;
    #[cfg(unix)]
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::FunctionCall;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn scratch_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kroma-edit-file-{label}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn call(path: &str, old: &str, new: &str) -> ToolCall {
        ToolCall {
            id: "edit-call".into(),
            kind: "function".into(),
            function: FunctionCall {
                name: "edit_file".into(),
                arguments: json!({"path": path, "old": old, "new": new}).to_string(),
            },
        }
    }

    fn context(dir: &Path) -> ToolContext<'_> {
        ToolContext {
            model_connection: None,
            user_message: None,
            user_turn: None,
            user_images: None,
            previous_user_images: None,
            workdir: dir,
            archive: dir,
            on_output: None,
            ask: None,
        }
    }

    async fn run(dir: &Path, path: &str, old: &str, new: &str) -> ToolResult {
        FileEdit.execute(&call(path, old, new), context(dir)).await
    }

    #[tokio::test]
    async fn replaces_unique_text_and_reports_line() -> Result<()> {
        let dir = scratch_dir("unique");
        fs::write(dir.join("code.rs"), "alpha\nbeta\ngamma\n")?;
        let result = run(&dir, "code.rs", "beta", "BETA").await;
        assert!(!result.is_error, "{}", result.content);
        let report: Value = serde_json::from_str(&result.content)?;
        assert_eq!(report["line"], 2);
        assert_eq!(
            fs::read_to_string(dir.join("code.rs"))?,
            "alpha\nBETA\ngamma\n"
        );
        assert_eq!(report["bytes"], 17);
        assert!(
            !result.content.contains("BETA\n"),
            "confirmation must not echo the file: {}",
            result.content
        );
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn empty_replacement_deletes_the_matched_text() -> Result<()> {
        let dir = scratch_dir("delete");
        fs::write(dir.join("code.rs"), "keep\ndrop\nkeep\n")?;
        let result = run(&dir, "code.rs", "drop\n", "").await;
        assert!(!result.is_error, "{}", result.content);
        assert_eq!(fs::read_to_string(dir.join("code.rs"))?, "keep\nkeep\n");
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn missing_old_text_is_rejected_and_the_file_is_untouched() -> Result<()> {
        let dir = scratch_dir("missing");
        fs::write(dir.join("code.rs"), "alpha\n")?;
        let result = run(&dir, "code.rs", "absent", "new").await;
        assert!(result.is_error);
        assert!(result.content.contains("not found"), "{}", result.content);
        assert_eq!(fs::read_to_string(dir.join("code.rs"))?, "alpha\n");
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn repeated_old_text_reports_the_count() -> Result<()> {
        let dir = scratch_dir("repeated");
        fs::write(dir.join("code.rs"), "x = 1\nx = 1\nx = 1\n")?;
        let result = run(&dir, "code.rs", "x = 1", "x = 2").await;
        assert!(result.is_error);
        assert!(result.content.contains("3 times"), "{}", result.content);
        assert_eq!(
            fs::read_to_string(dir.join("code.rs"))?,
            "x = 1\nx = 1\nx = 1\n"
        );
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn empty_old_text_is_rejected() -> Result<()> {
        let dir = scratch_dir("empty-old");
        fs::write(dir.join("code.rs"), "alpha\n")?;
        let result = run(&dir, "code.rs", "", "new").await;
        assert!(result.is_error);
        assert!(
            result.content.contains("must not be empty"),
            "{}",
            result.content
        );
        assert_eq!(fs::read_to_string(dir.join("code.rs"))?, "alpha\n");
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn unknown_file_is_reported() -> Result<()> {
        let dir = scratch_dir("unknown");
        let result = run(&dir, "absent.rs", "alpha", "beta").await;
        assert!(result.is_error);
        assert!(
            result.content.contains("file not found"),
            "{}",
            result.content
        );
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn paths_outside_the_working_directory_are_rejected() -> Result<()> {
        let dir = scratch_dir("escape");
        let workdir = dir.join("work");
        fs::create_dir_all(&workdir)?;
        let outside = dir.join("outside.txt");
        fs::write(&outside, "secret\n")?;
        let parent = run(&workdir, "../outside.txt", "secret", "leaked").await;
        assert!(parent.is_error);
        assert!(
            parent.content.contains("leave the working directory"),
            "{}",
            parent.content
        );
        let absolute = run(&workdir, &outside.to_string_lossy(), "secret", "leaked").await;
        assert!(absolute.is_error);
        assert!(
            absolute.content.contains("outside the working directory"),
            "{}",
            absolute.content
        );
        assert_eq!(fs::read_to_string(&outside)?, "secret\n");
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn an_in_workspace_absolute_path_and_a_new_file_are_supported() -> Result<()> {
        let dir = scratch_dir("inside-absolute");
        fs::write(dir.join("code.rs"), "fn main() {}\n")?;
        let absolute = dir.join("code.rs");
        let edited = run(
            &dir,
            &absolute.to_string_lossy(),
            "fn main() {}",
            "fn main() { run(); }",
        )
        .await;
        assert!(!edited.is_error, "{}", edited.content);
        assert_eq!(
            fs::read_to_string(dir.join("code.rs"))?,
            "fn main() { run(); }\n"
        );
        let created = run(&dir, "src/new.rs", "", "pub fn new() {}\n").await;
        assert!(created.is_error, "a missing directory is reported");
        fs::create_dir_all(dir.join("src"))?;
        let created = run(&dir, "src/new.rs", "", "pub fn new() {}\n").await;
        assert!(!created.is_error, "{}", created.content);
        assert!(
            created.content.contains("\"created\":true"),
            "{}",
            created.content
        );
        assert_eq!(
            fs::read_to_string(dir.join("src/new.rs"))?,
            "pub fn new() {}\n"
        );
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn crlf_line_endings_are_preserved() -> Result<()> {
        let dir = scratch_dir("crlf");
        fs::write(dir.join("code.rs"), "fn main() {\r\n    old();\r\n}\r\n")?;
        let result = run(&dir, "code.rs", "    old();", "    new();").await;
        assert!(!result.is_error, "{}", result.content);
        let report: Value = serde_json::from_str(&result.content)?;
        assert_eq!(report["line"], 2);
        let written = String::from_utf8(fs::read(dir.join("code.rs"))?)?;
        assert_eq!(written, "fn main() {\r\n    new();\r\n}\r\n");
        assert_eq!(
            written.matches('\n').count(),
            written.matches("\r\n").count()
        );
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn an_executable_script_keeps_its_mode() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch_dir("mode");
        let script = dir.join("run.sh");
        fs::write(&script, "#!/bin/sh\necho old\n")?;
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755))?;
        let result = run(&dir, "run.sh", "echo old", "echo new").await;
        assert!(!result.is_error, "{}", result.content);
        let mode = fs::metadata(&script)?.permissions().mode() & 0o777;
        assert_eq!(mode, 0o755);
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn atomic_write_leaves_no_temporary_file() -> Result<()> {
        let dir = scratch_dir("atomic");
        fs::write(dir.join("code.rs"), "alpha\n")?;
        let result = run(&dir, "code.rs", "alpha", "beta").await;
        assert!(!result.is_error, "{}", result.content);
        let entries: Vec<String> = fs::read_dir(&dir)?
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(entries, vec!["code.rs".to_owned()]);
        fs::remove_dir_all(dir)?;
        Ok(())
    }
}
