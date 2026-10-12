use crate::messages::ToolResultMeta;
use crate::sensitive_paths::SensitivePathGuard;
use crate::tools::ToolExecutionOutput;
use crate::types::ErrorCode;
use crate::workspace::WorkspaceCap;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::time::Instant;

pub const DEFAULT_NAV_LIMIT: usize = 200;
pub const MAX_NAV_OUTPUT_BYTES: usize = 32 * 1024; // 32 KiB

pub const BUILTIN_SKIPPED_DIRS: &[&str] = &[".git", "target", "node_modules", "dist", ".venv"];

pub fn is_skipped_dir_name(name: &str) -> bool {
    BUILTIN_SKIPPED_DIRS.contains(&name)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LsArgs {
    pub path: Option<String>,
    pub limit: Option<usize>,
}

pub fn execute_ls(workspace: &WorkspaceCap, raw_args: &str) -> ToolExecutionOutput {
    let started = Instant::now();
    let args: LsArgs = match serde_json::from_str(raw_args) {
        Ok(a) => a,
        Err(e) => {
            return ToolExecutionOutput::error(
                ErrorCode::InvalidArguments,
                format!("invalid JSON arguments for `ls`: {}", e),
                ToolResultMeta::default(),
            );
        }
    };

    let rel_input = args.path.as_deref().unwrap_or(".");
    let limit = args.limit.unwrap_or(DEFAULT_NAV_LIMIT).min(DEFAULT_NAV_LIMIT);
    if limit == 0 {
        return ToolExecutionOutput::error(
            ErrorCode::InvalidArguments,
            "`limit` must be >= 1".to_string(),
            ToolResultMeta::default(),
        );
    }

    // F-02: list the directory from a race-free `openat2(RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS)`
    // handle and classify entries with `fstatat(AT_SYMLINK_NOFOLLOW)`, so a directory swapped for a
    // symlink after a path check cannot redirect the listing outside the workspace.
    let (norm_rel, dir_entries) = match workspace.read_dir_no_symlink(rel_input) {
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
    let rel_str = norm_rel.to_string_lossy().to_string();

    if norm_rel.components().any(|c| {
        matches!(c, std::path::Component::Normal(os) if is_skipped_dir_name(&os.to_string_lossy()))
    }) {
        return ToolExecutionOutput::success(
            String::new(),
            ToolResultMeta {
                relative_path: Some(rel_str),
                duration_ms: started.elapsed().as_millis() as u64,
                truncated: false,
                ..Default::default()
            },
        );
    }

    let mut entries = Vec::new();
    let mut skipped_sensitive = 0usize;

    for entry in dir_entries {
        let file_name = entry.name;
        if is_skipped_dir_name(&file_name) {
            continue;
        }

        let child_rel = if norm_rel == Path::new(".") {
            PathBuf::from(&file_name)
        } else {
            norm_rel.join(&file_name)
        };

        if SensitivePathGuard::is_sensitive(&child_rel) {
            skipped_sensitive += 1;
            continue;
        }

        // Do not follow symlinks
        if entry.is_symlink {
            continue;
        }

        if entry.is_dir {
            entries.push(format!("{}/", file_name));
        } else if entry.is_file {
            entries.push(file_name);
        }
    }

    entries.sort();
    let total_entries = entries.len();
    let mut out = String::new();
    let mut truncated = false;
    let mut shown = 0usize;

    for (idx, item) in entries.into_iter().enumerate() {
        if idx >= limit || out.len() + item.len() + 1 > MAX_NAV_OUTPUT_BYTES {
            truncated = true;
            break;
        }
        out.push_str(&item);
        out.push('\n');
        shown += 1;
    }

    if truncated {
        out.push_str(&format!(
            "[Truncated at {} of {} entries]\n",
            shown, total_entries
        ));
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
