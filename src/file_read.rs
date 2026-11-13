//! Bounded, read-only workspace file inspection tool for the agent.

use crate::{
    file_edit::resolve_existing_file,
    protocol::ToolCall,
    tools::{Tool, ToolContext, ToolFuture, ToolResult},
};
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    fs::File,
    io::{BufRead, BufReader},
    path::Path,
};

const DEFAULT_LINES: usize = 200;
const MAX_LINES: usize = 400;
const MAX_OUTPUT_BYTES: usize = 32 * 1024;
const MAX_LINE_BYTES: usize = 8 * 1024;

#[derive(Deserialize)]
struct ReadArgs {
    path: String,
    start_line: Option<usize>,
    end_line: Option<usize>,
}

pub struct FileRead;

impl Tool for FileRead {
    fn name(&self) -> &str {
        "read_file"
    }

    fn may_mutate_worktree(&self) -> bool {
        false
    }

    fn definition(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": "read_file",
                "description": "Read a line-numbered UTF-8 text slice from a file inside the working directory without running a shell command. Defaults to lines 1..200 and caps each call at 400 lines / 32 KiB. Read small focused ranges instead of whole large files.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "File path relative to the working directory, or an absolute path inside it; `..` is rejected"},
                        "start_line": {"type": "integer", "minimum": 1, "description": "1-based first line to return (default 1)"},
                        "end_line": {"type": "integer", "minimum": 1, "description": "1-based inclusive last line to return (capped at 400 lines per call)"}
                    },
                    "required": ["path"],
                    "additionalProperties": false
                }
            }
        })
    }

    fn execute<'a>(&'a self, call: &'a ToolCall, ctx: ToolContext<'a>) -> ToolFuture<'a> {
        Box::pin(async move {
            match read(call, ctx.workdir) {
                Ok(payload) => ToolResult::ok(payload.to_string()),
                Err(error) => ToolResult::error(json!({"error": format!("{error:#}")}).to_string()),
            }
        })
    }
}

fn read(call: &ToolCall, workdir: &Path) -> Result<Value> {
    if call.kind != "function" || call.function.name != "read_file" {
        bail!("unsupported tool: {}", call.function.name);
    }
    let args: ReadArgs =
        serde_json::from_str(&call.function.arguments).context("invalid read_file arguments")?;
    let start_line = args.start_line.unwrap_or(1);
    if start_line == 0 {
        bail!("start_line must be >= 1");
    }
    let requested_end = args
        .end_line
        .unwrap_or_else(|| start_line.saturating_add(DEFAULT_LINES - 1));
    if requested_end < start_line {
        bail!("end_line ({requested_end}) must be >= start_line ({start_line})");
    }
    let capped_end = requested_end.min(start_line.saturating_add(MAX_LINES - 1));

    let target = resolve_existing_file(workdir, &args.path)?;
    let file = File::open(&target).with_context(|| format!("cannot open {}", args.path))?;
    let mut reader = BufReader::new(file);
    let mut raw_line = Vec::new();
    let mut line_no = 0usize;
    let mut output = String::new();
    let mut actual_end = 0usize;
    let mut truncated = false;
    let mut has_more = false;

    loop {
        raw_line.clear();
        let bytes = reader.read_until(b'\n', &mut raw_line)?;
        if bytes == 0 {
            break;
        }
        line_no += 1;
        if line_no < start_line {
            continue;
        }
        if line_no > capped_end {
            has_more = args.end_line.is_none() || requested_end > capped_end;
            break;
        }
        if raw_line.contains(&0) {
            bail!(
                "{} appears to be binary; read_file only reads UTF-8 text",
                args.path
            );
        }
        let text = std::str::from_utf8(&raw_line)
            .with_context(|| format!("{} is not valid UTF-8 text", args.path))?;
        let trimmed = text.trim_end_matches(['\r', '\n']);
        let formatted = if trimmed.len() > MAX_LINE_BYTES {
            let mut cut = MAX_LINE_BYTES.min(trimmed.len());
            while cut > 0 && !trimmed.is_char_boundary(cut) {
                cut -= 1;
            }
            truncated = true;
            format!("{line_no}| {}...(line truncated)\n", &trimmed[..cut])
        } else {
            format!("{line_no}| {trimmed}\n")
        };
        if !output.is_empty() && output.len() + formatted.len() > MAX_OUTPUT_BYTES {
            truncated = true;
            has_more = true;
            break;
        }
        output.push_str(&formatted);
        actual_end = line_no;
        if output.len() >= MAX_OUTPUT_BYTES {
            // Check whether another line exists without allocating the rest of the file.
            raw_line.clear();
            if reader.read_until(b'\n', &mut raw_line)? > 0 {
                truncated = true;
                has_more = true;
            }
            break;
        }
    }

    if line_no < start_line && line_no > 0 {
        bail!(
            "start_line ({start_line}) exceeds total lines ({line_no}) in {}",
            args.path
        );
    }

    Ok(json!({
        "path": args.path,
        "start_line": if actual_end == 0 { 0 } else { start_line },
        "end_line": actual_end,
        "truncated": truncated || has_more,
        "content": output,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::FunctionCall;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn scratch_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kroma-read-file-{label}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
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

    fn call(args: Value) -> ToolCall {
        ToolCall {
            id: "read-call".into(),
            kind: "function".into(),
            function: FunctionCall {
                name: "read_file".into(),
                arguments: args.to_string(),
            },
        }
    }

    #[tokio::test]
    async fn reads_line_numbered_slice_and_declares_read_only() -> Result<()> {
        assert!(!FileRead.may_mutate_worktree());
        let dir = scratch_dir("slice");
        fs::write(dir.join("sample.txt"), "one\r\ntwo\r\nthree\r\nfour\r\n")?;
        let res = FileRead
            .execute(
                &call(json!({"path": "sample.txt", "start_line": 2, "end_line": 3})),
                context(&dir),
            )
            .await;
        assert!(!res.is_error, "{}", res.content);
        let parsed: Value = serde_json::from_str(&res.content)?;
        assert_eq!(parsed["start_line"], 2);
        assert_eq!(parsed["end_line"], 3);
        assert_eq!(parsed["truncated"], false);
        assert_eq!(parsed["content"], "2| two\n3| three\n");
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn rejects_traversal_outside_workspace_and_binary_files() -> Result<()> {
        let dir = scratch_dir("guard");
        let outside = scratch_dir("outside");
        fs::write(outside.join("secret.txt"), "nope")?;
        fs::write(dir.join("bin.dat"), b"\x00\x01\x02")?;

        let traversal = FileRead
            .execute(&call(json!({"path": "../secret.txt"})), context(&dir))
            .await;
        assert!(traversal.is_error);

        let outside_abs = FileRead
            .execute(
                &call(json!({"path": outside.join("secret.txt").to_string_lossy()})),
                context(&dir),
            )
            .await;
        assert!(outside_abs.is_error);

        let binary = FileRead
            .execute(&call(json!({"path": "bin.dat"})), context(&dir))
            .await;
        assert!(binary.is_error);
        assert!(binary.content.contains("binary"), "{}", binary.content);

        fs::remove_dir_all(dir)?;
        fs::remove_dir_all(outside)?;
        Ok(())
    }

    #[tokio::test]
    async fn caps_large_output_and_reports_truncated() -> Result<()> {
        let dir = scratch_dir("cap");
        let mut big = String::new();
        for i in 1..=500 {
            big.push_str(&format!("line-{i:04}-payload-content\n"));
        }
        fs::write(dir.join("big.txt"), big)?;
        let res = FileRead
            .execute(
                &call(json!({"path": "big.txt", "end_line": 500})),
                context(&dir),
            )
            .await;
        assert!(!res.is_error, "{}", res.content);
        let parsed: Value = serde_json::from_str(&res.content)?;
        assert_eq!(parsed["end_line"], MAX_LINES);
        assert_eq!(parsed["truncated"], true);
        fs::remove_dir_all(dir)?;
        Ok(())
    }
}
