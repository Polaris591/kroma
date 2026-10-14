use crate::messages::ToolResultMeta;
use crate::sensitive_paths::SensitivePathGuard;
use crate::tools::ls::{is_skipped_dir_name, DEFAULT_NAV_LIMIT, MAX_NAV_OUTPUT_BYTES};
use crate::tools::ToolExecutionOutput;
use crate::types::ErrorCode;
use crate::workspace::WorkspaceCap;
use regex::Regex;
use serde::Deserialize;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Instant;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FindArgs {
    pub pattern: String,
    pub path: Option<String>,
    pub limit: Option<usize>,
}

pub fn execute_find(workspace: &WorkspaceCap, raw_args: &str) -> ToolExecutionOutput {
    let started = Instant::now();
    let args: FindArgs = match serde_json::from_str(raw_args) {
        Ok(a) => a,
        Err(e) => {
            return ToolExecutionOutput::error(
                ErrorCode::InvalidArguments,
                format!("invalid JSON arguments for `find`: {}", e),
                ToolResultMeta::default(),
            );
        }
    };

    let rel_input = args.path.as_deref().unwrap_or(".");
    let limit = args
        .limit
        .unwrap_or(DEFAULT_NAV_LIMIT)
        .min(DEFAULT_NAV_LIMIT);
    if limit == 0 || args.pattern.trim().is_empty() {
        return ToolExecutionOutput::error(
            ErrorCode::InvalidArguments,
            "`pattern` must be non-empty and `limit` >= 1".to_string(),
            ToolResultMeta::default(),
        );
    }

    let clean_pattern = args.pattern.strip_prefix('/').unwrap_or(&args.pattern);
    let glob_regex = match compile_glob_to_regex(clean_pattern) {
        Ok(r) => r,
        Err(e) => {
            return ToolExecutionOutput::error(
                ErrorCode::InvalidArguments,
                format!("invalid glob pattern '{}': {}", args.pattern, e),
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

    let pattern_has_slash = clean_pattern.contains('/') || args.pattern.starts_with('/');
    let mut matched: Vec<String> = files
        .into_iter()
        .filter_map(|rel_file| {
            let rel_display = rel_file.to_string_lossy().replace('\\', "/");
            let file_name = rel_file.file_name()?.to_string_lossy();
            let is_match = if pattern_has_slash {
                glob_regex.is_match(&rel_display)
            } else {
                glob_regex.is_match(&file_name) || glob_regex.is_match(&rel_display)
            };
            is_match.then_some(rel_display)
        })
        .collect();

    matched.sort();
    let total_matched = matched.len();
    let mut out = String::new();
    let mut truncated = false;
    let mut shown = 0usize;

    for (idx, item) in matched.into_iter().enumerate() {
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
            "[Truncated at {} of {} matches]\n",
            shown, total_matched
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

#[derive(Debug, Clone)]
pub struct IgnoreRule {
    pub base_dir: PathBuf,
    pub negated: bool,
    pub dir_only: bool,
    pub anchored: bool,
    pub regex: Regex,
}

/// Walk the workspace emitting **workspace-relative** file paths (F-02).
///
/// Directory traversal goes through [`WorkspaceCap::read_dir_no_symlink`], which opens each
/// directory with `openat2(RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS)` and classifies entries via
/// `fstatat(AT_SYMLINK_NOFOLLOW)`. Callers must re-open each returned path through
/// `open_existing_no_symlink` rather than trusting a pre-check, so a swap after the walk cannot
/// redirect a read outside the workspace.
pub fn walk_workspace_files(
    workspace: &WorkspaceCap,
    start_rel: &Path,
    ignore_rules: &[IgnoreRule],
    out_files: &mut Vec<PathBuf>,
    skipped_sensitive: &mut usize,
) {
    let start_rel = if start_rel.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        start_rel.to_path_buf()
    };

    if start_rel.components().any(|c| {
        matches!(c, std::path::Component::Normal(os) if is_skipped_dir_name(&os.to_string_lossy()))
    }) {
        return;
    }
    if start_rel != Path::new(".") && SensitivePathGuard::is_sensitive(&start_rel) {
        *skipped_sensitive += 1;
        return;
    }

    let mut active_rules = ignore_rules.to_vec();
    if start_rel != Path::new(".") {
        let mut rel_anc = PathBuf::new();
        for comp in start_rel.components() {
            if let std::path::Component::Normal(part) = comp {
                rel_anc.push(part);
                if rel_anc != start_rel {
                    load_dir_ignore_rules(workspace, &rel_anc, &mut active_rules);
                }
            }
        }
    }

    let start_str = start_rel.to_string_lossy().replace('\\', "/");
    match workspace.open_dir_no_symlink(&start_str) {
        Ok(_) => walk_workspace_files_inner(
            workspace,
            &start_rel,
            &active_rules,
            out_files,
            skipped_sensitive,
        ),
        Err(crate::workspace::WorkspaceError::InvalidPath { .. }) => {
            // The start path is a regular file, not a directory: emit it directly.
            if let Ok((_, file)) = workspace.open_existing_no_symlink(&start_str) {
                if file
                    .metadata()
                    .map(|m| m.file_type().is_file())
                    .unwrap_or(false)
                {
                    out_files.push(start_rel.clone());
                }
            }
        }
        Err(_) => {}
    }
}

fn walk_workspace_files_inner(
    workspace: &WorkspaceCap,
    dir_rel: &Path,
    ignore_rules: &[IgnoreRule],
    out_files: &mut Vec<PathBuf>,
    skipped_sensitive: &mut usize,
) {
    let dir_rel_str = dir_rel.to_string_lossy().replace('\\', "/");
    let Ok((_, mut entries)) = workspace.read_dir_no_symlink(&dir_rel_str) else {
        return;
    };

    let mut local_rules = ignore_rules.to_vec();
    if dir_rel != Path::new(".") {
        if is_ignored_by_rules(dir_rel, true, &local_rules) {
            return;
        }
        load_dir_ignore_rules(workspace, dir_rel, &mut local_rules);
    }

    entries.sort_by(|a, b| a.name.cmp(&b.name));

    for entry in entries {
        if is_skipped_dir_name(&entry.name) {
            continue;
        }
        // Never follow symlinks (§5.5)
        if entry.is_symlink {
            continue;
        }
        let child_rel = if dir_rel == Path::new(".") {
            PathBuf::from(&entry.name)
        } else {
            dir_rel.join(&entry.name)
        };
        if SensitivePathGuard::is_sensitive(&child_rel) {
            if entry.is_file || entry.is_dir {
                *skipped_sensitive += 1;
            }
            continue;
        }
        if is_ignored_by_rules(&child_rel, entry.is_dir, &local_rules) {
            continue;
        }
        if entry.is_dir {
            walk_workspace_files_inner(
                workspace,
                &child_rel,
                &local_rules,
                out_files,
                skipped_sensitive,
            );
        } else if entry.is_file {
            out_files.push(child_rel);
        }
    }
}

pub fn load_workspace_ignore_patterns(workspace: &WorkspaceCap) -> Vec<IgnoreRule> {
    let mut rules = Vec::new();
    load_dir_ignore_rules(workspace, Path::new(""), &mut rules);
    rules
}

fn load_dir_ignore_rules(workspace: &WorkspaceCap, dir_rel: &Path, rules: &mut Vec<IgnoreRule>) {
    for file_name in [".gitignore", ".ignore"] {
        let rel = if dir_rel.as_os_str().is_empty() {
            PathBuf::from(file_name)
        } else {
            dir_rel.join(file_name)
        };
        // F-02: ignore files are read through the same symlink-safe, FD-relative open as any other
        // workspace file, never via a raw path read.
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        let Ok((_, mut file)) = workspace.open_existing_no_symlink(&rel_str) else {
            continue;
        };
        let mut content = String::new();
        if file.read_to_string(&mut content).is_err() {
            continue;
        }
        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            let (negated, rest) = if let Some(r) = trimmed.strip_prefix('!') {
                (true, r.trim())
            } else {
                (false, trimmed)
            };
            if rest.is_empty() {
                continue;
            }
            let (dir_only, without_trailing) = if let Some(r) = rest.strip_suffix('/') {
                (true, r)
            } else {
                (false, rest)
            };
            let anchored = without_trailing.contains('/');
            let clean_pat = without_trailing
                .strip_prefix('/')
                .unwrap_or(without_trailing);
            if clean_pat.is_empty() {
                continue;
            }
            if let Ok(re) = compile_glob_to_regex(clean_pat) {
                rules.push(IgnoreRule {
                    base_dir: dir_rel.to_path_buf(),
                    negated,
                    dir_only,
                    anchored,
                    regex: re,
                });
            }
        }
    }
}

fn is_ignored_by_rules(rel: &Path, is_dir: bool, rules: &[IgnoreRule]) -> bool {
    if rules.is_empty() {
        return false;
    }
    let mut ignored = false;
    for rule in rules {
        let sub_rel = if rule.base_dir.as_os_str().is_empty() {
            rel
        } else if let Ok(s) = rel.strip_prefix(&rule.base_dir) {
            s
        } else {
            continue;
        };
        if sub_rel.as_os_str().is_empty() {
            continue;
        }
        if rule.dir_only && !is_dir {
            continue;
        }
        let sub_str = sub_rel.to_string_lossy().replace('\\', "/");
        let file_name = sub_rel
            .file_name()
            .map(|f| f.to_string_lossy().to_string())
            .unwrap_or_default();
        let matched = if rule.anchored {
            rule.regex.is_match(&sub_str)
        } else {
            rule.regex.is_match(&file_name) || rule.regex.is_match(&sub_str)
        };
        if matched {
            ignored = !rule.negated;
        }
    }
    ignored
}

pub fn compile_glob_to_regex(glob: &str) -> Result<Regex, regex::Error> {
    let mut regex_str = String::from("^");
    let chars: Vec<char> = glob.chars().collect();
    compile_glob_segment(&chars, &mut regex_str);
    regex_str.push('$');
    Regex::new(&regex_str)
}

fn compile_glob_segment(chars: &[char], regex_str: &mut String) {
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '*' && i + 1 < chars.len() && chars[i + 1] == '*' {
            if i + 2 < chars.len() && chars[i + 2] == '/' {
                regex_str.push_str("(?:.*/)?");
                i += 3;
            } else {
                regex_str.push_str(".*");
                i += 2;
            }
        } else if chars[i] == '*' {
            regex_str.push_str("[^/]*");
            i += 1;
        } else if chars[i] == '?' {
            regex_str.push_str("[^/]");
            i += 1;
        } else if chars[i] == '[' {
            if let Some(close_rel) = chars[i + 1..].iter().position(|&c| c == ']') {
                let close_idx = i + 1 + close_rel;
                regex_str.push('[');
                let mut j = i + 1;
                if j < close_idx && (chars[j] == '!' || chars[j] == '^') {
                    regex_str.push('^');
                    j += 1;
                }
                while j < close_idx {
                    if chars[j] == '\\' {
                        regex_str.push_str("\\\\");
                    } else {
                        regex_str.push(chars[j]);
                    }
                    j += 1;
                }
                regex_str.push(']');
                i = close_idx + 1;
            } else {
                regex_str.push_str("\\[");
                i += 1;
            }
        } else if chars[i] == '{' {
            if let Some(close_rel) = chars[i + 1..].iter().position(|&c| c == '}') {
                let close_idx = i + 1 + close_rel;
                let inner = &chars[i + 1..close_idx];
                regex_str.push_str("(?:");
                let mut first = true;
                for alt in inner.split(|&c| c == ',') {
                    if !first {
                        regex_str.push('|');
                    }
                    first = false;
                    compile_glob_segment(alt, regex_str);
                }
                regex_str.push(')');
                i = close_idx + 1;
            } else {
                regex_str.push_str("\\{");
                i += 1;
            }
        } else {
            let c = chars[i];
            if ".+^$()[]{}|\\".contains(c) {
                regex_str.push('\\');
            }
            regex_str.push(c);
            i += 1;
        }
    }
}
