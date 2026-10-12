use crate::messages::ToolResultMeta;
use crate::tools::find::{load_workspace_ignore_patterns, walk_workspace_files};
use crate::tools::ls::{DEFAULT_NAV_LIMIT, MAX_NAV_OUTPUT_BYTES};
use crate::tools::ToolExecutionOutput;
use crate::types::ErrorCode;
use crate::workspace::WorkspaceCap;
use regex::RegexBuilder;
use serde::Deserialize;
use std::io::Read;
use std::time::Instant;

pub const MAX_GREP_REGEX_SIZE: usize = 2 * 1024 * 1024; // 2 MiB (ReDoS guard)
pub const MAX_GREP_SINGLE_FILE_BYTES: u64 = 2 * 1024 * 1024; // 2 MiB
pub const MAX_GREP_TOTAL_SCAN_BYTES: usize = 32 * 1024 * 1024; // 32 MiB
pub const MAX_GREP_LINE_CHARS: usize = 300;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrepArgs {
    pub pattern: String,
    pub path: Option<String>,
    pub limit: Option<usize>,
}

pub fn execute_grep(workspace: &WorkspaceCap, raw_args: &str) -> ToolExecutionOutput {
    let started = Instant::now();
    let args: GrepArgs = match serde_json::from_str(raw_args) {
        Ok(a) => a,
        Err(e) => {
            return ToolExecutionOutput::error(
                ErrorCode::InvalidArguments,
                format!("invalid JSON arguments for `grep`: {}", e),
                ToolResultMeta::default(),
            );
        }
    };

    let rel_input = args.path.as_deref().unwrap_or(".");
    let limit = args.limit.unwrap_or(DEFAULT_NAV_LIMIT).min(DEFAULT_NAV_LIMIT);
    if limit == 0 || args.pattern.is_empty() {
        return ToolExecutionOutput::error(
            ErrorCode::InvalidArguments,
            "`pattern` must be non-empty and `limit` >= 1".to_string(),
            ToolResultMeta::default(),
        );
    }

    let regex = match RegexBuilder::new(&args.pattern)
        .size_limit(MAX_GREP_REGEX_SIZE)
        .dfa_size_limit(MAX_GREP_REGEX_SIZE)
        .build()
    {
        Ok(r) => r,
        Err(e) => {
            return ToolExecutionOutput::error(
                ErrorCode::InvalidArguments,
                format!("invalid or overly complex regex '{}': {}", args.pattern, e),
                ToolResultMeta::default(),
            );
        }
    };

    let (norm_rel, _) = match workspace.resolve_existing_path(rel_input, false) {
        Ok(p) => p,
        Err(e) => {
            return ToolExecutionOutput::error(
                e.error_code(),
                e.to_string(),
                ToolResultMeta {
                    relative_path: Some(rel_input.to_string()),
                    ..Default::default()
                },
            );
        }
    };
    let rel_str = norm_rel.to_string_lossy().replace('\\', "/");

    let mut files = Vec::new();
    let mut skipped_sensitive = 0usize;
    let ignore_rules = load_workspace_ignore_patterns(workspace);

    walk_workspace_files(
        workspace,
        &norm_rel,
        &ignore_rules,
        &mut files,
        &mut skipped_sensitive,
    );

    files.sort();

    let mut out = String::new();
    let mut match_count = 0usize;
    let mut total_scanned_bytes = 0usize;
    let mut truncated = false;

    'file_loop: for rel_file in files {
        let rel_file_str = rel_file.to_string_lossy().replace('\\', "/");
        // F-02: never `fs::read` a path that was only *checked* earlier. Re-open through
        // `open_existing_no_symlink`, which enforces `openat2(RESOLVE_BENEATH |
        // RESOLVE_NO_SYMLINKS)` at open time (and `O_NONBLOCK` to avoid FIFO deadlock), then bound
        // the read with `take` so a growing file cannot exhaust memory.
        let Ok((_, mut file)) = workspace.open_existing_no_symlink(&rel_file_str) else {
            continue;
        };
        let Ok(meta) = file.metadata() else {
            continue;
        };
        if !meta.file_type().is_file() || meta.len() > MAX_GREP_SINGLE_FILE_BYTES {
            continue;
        }
        if total_scanned_bytes + (meta.len() as usize) > MAX_GREP_TOTAL_SCAN_BYTES {
            truncated = true;
            break;
        }
        let mut bytes = Vec::new();
        if file
            .by_ref()
            .take(MAX_GREP_SINGLE_FILE_BYTES + 1)
            .read_to_end(&mut bytes)
            .is_err()
        {
            continue;
        }
        if bytes.len() as u64 > MAX_GREP_SINGLE_FILE_BYTES {
            continue;
        }
        total_scanned_bytes += bytes.len();
        if bytes.contains(&0u8) {
            continue;
        }
        let Ok(text) = std::str::from_utf8(&bytes) else {
            continue;
        };
        let rel_display = rel_file_str.replace('\\', "/");

        for (idx, line) in text.lines().enumerate() {
            if regex.is_match(line) {
                if match_count >= limit {
                    truncated = true;
                    break 'file_loop;
                }
                let line_clean = line.strip_suffix('\r').unwrap_or(line);
                let display_line: String = if line_clean.chars().count() > MAX_GREP_LINE_CHARS {
                    let prefix: String = line_clean.chars().take(MAX_GREP_LINE_CHARS).collect();
                    format!("{}...", prefix)
                } else {
                    line_clean.to_string()
                };
                let entry = format!("{}:{}:{}\n", rel_display, idx + 1, display_line);
                if out.len() + entry.len() > MAX_NAV_OUTPUT_BYTES && !out.is_empty() {
                    truncated = true;
                    break 'file_loop;
                }
                out.push_str(&entry);
                match_count += 1;
            }
        }
    }

    if truncated {
        out.push_str(&format!("[Truncated after {} matches]\n", match_count));
    }
    if skipped_sensitive > 0 {
        out.push_str(&format!(
            "[Skipped {} sensitive file(s)]\n",
            skipped_sensitive
        ));
    }

    ToolExecutionOutput::success(
        out,
        ToolResultMeta {
            relative_path: Some(rel_str),
            duration_ms: started.elapsed().as_millis() as u64,
            truncated,
            ..Default::default()
        },
    )
}
