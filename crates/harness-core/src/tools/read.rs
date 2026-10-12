use crate::messages::ToolResultMeta;
use crate::prompt::hex_encode;
use crate::tool::output_buffer::floor_char_boundary;
use crate::tools::ToolExecutionOutput;
use crate::types::ErrorCode;
use crate::workspace::WorkspaceCap;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::io::{BufReader, Read};
use std::time::Instant;

pub const MAX_READ_SCAN_BYTES: usize = 8 * 1024 * 1024; // 8 MiB
pub const MAX_SHA256_FILE_BYTES: u64 = 4 * 1024 * 1024; // 4 MiB
pub const MAX_INLINE_OUTPUT_BYTES: usize = 64 * 1024; // 64 KiB
pub const MAX_READ_LINES: usize = 2000;
pub const MAX_SINGLE_LINE_BYTES: usize = 2048; // 2 KiB

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadArgs {
    pub path: String,
    pub offset: Option<usize>,
    pub limit: Option<usize>,
}

pub fn execute_read(workspace: &WorkspaceCap, raw_args: &str) -> ToolExecutionOutput {
    let started = Instant::now();
    let args: ReadArgs = match serde_json::from_str(raw_args) {
        Ok(a) => a,
        Err(e) => {
            return ToolExecutionOutput::error(
                ErrorCode::InvalidArguments,
                format!("invalid JSON arguments for `read`: {}", e),
                ToolResultMeta::default(),
            );
        }
    };

    let offset = args.offset.unwrap_or(1);
    if offset == 0 {
        return ToolExecutionOutput::error(
            ErrorCode::InvalidArguments,
            "`offset` is 1-indexed and must be >= 1".to_string(),
            ToolResultMeta {
                relative_path: Some(args.path),
                ..Default::default()
            },
        );
    }

    let limit = args.limit.unwrap_or(MAX_READ_LINES);
    if limit == 0 {
        return ToolExecutionOutput::error(
            ErrorCode::InvalidArguments,
            "`limit` must be >= 1".to_string(),
            ToolResultMeta {
                relative_path: Some(args.path),
                ..Default::default()
            },
        );
    }
    let effective_limit = limit.min(MAX_READ_LINES);

    let (norm_rel, file) = match workspace.open_existing_no_symlink(&args.path) {
        Ok(p) => p,
        Err(e) => {
            return ToolExecutionOutput::error(
                e.error_code(),
                e.to_string(),
                ToolResultMeta {
                    relative_path: Some(args.path),
                    ..Default::default()
                },
            );
        }
    };
    let rel_str = norm_rel.to_string_lossy().to_string();

    let meta = match file.metadata() {
        Ok(m) => m,
        Err(e) => {
            return ToolExecutionOutput::error(
                ErrorCode::IoError,
                format!("failed to stat '{}': {}", rel_str, e),
                ToolResultMeta {
                    relative_path: Some(rel_str),
                    ..Default::default()
                },
            );
        }
    };

    if !meta.file_type().is_file() {
        return ToolExecutionOutput::error(
            ErrorCode::InvalidArguments,
            format!("'{}' is not a regular file", rel_str),
            ToolResultMeta {
                relative_path: Some(rel_str),
                ..Default::default()
            },
        );
    }

    let file_size = meta.len();
    let compute_hash = file_size <= MAX_SHA256_FILE_BYTES;
    let mut hasher = compute_hash.then(Sha256::new);

    // Bounded streaming read up to MAX_READ_SCAN_BYTES (8 MiB)
    let mut reader = BufReader::with_capacity(16 * 1024, file);
    let mut raw_bytes = Vec::with_capacity((file_size as usize).min(64 * 1024));
    let mut chunk = [0u8; 8192];
    let mut scanned_bytes: usize = 0;
    let mut hit_scan_cap = false;

    loop {
        match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                let slice = &chunk[..n];
                // Check for NUL byte (binary file)
                if slice.contains(&0u8) {
                    return ToolExecutionOutput::error(
                        ErrorCode::BinaryOrInvalidUtf8,
                        format!("file '{}' contains NUL bytes and appears to be binary", rel_str),
                        ToolResultMeta {
                            relative_path: Some(rel_str),
                            duration_ms: started.elapsed().as_millis() as u64,
                            ..Default::default()
                        },
                    );
                }
                if let Some(ref mut h) = hasher {
                    h.update(slice);
                }
                let remaining_scan = MAX_READ_SCAN_BYTES.saturating_sub(scanned_bytes);
                let take_n = n.min(remaining_scan);
                raw_bytes.extend_from_slice(&slice[..take_n]);
                scanned_bytes += take_n;
                if scanned_bytes >= MAX_READ_SCAN_BYTES {
                    if take_n < n || file_size > MAX_READ_SCAN_BYTES as u64 {
                        hit_scan_cap = true;
                        hasher = None;
                        break;
                    }
                    let mut probe = [0u8; 1];
                    match reader.read(&mut probe) {
                        Ok(0) => break,
                        Ok(_) => {
                            hit_scan_cap = true;
                            hasher = None;
                            break;
                        }
                        Err(e) => {
                            return ToolExecutionOutput::error(
                                ErrorCode::IoError,
                                format!("error reading '{}': {}", rel_str, e),
                                ToolResultMeta {
                                    relative_path: Some(rel_str),
                                    ..Default::default()
                                },
                            );
                        }
                    }
                }
            }
            Err(e) => {
                return ToolExecutionOutput::error(
                    ErrorCode::IoError,
                    format!("error reading '{}': {}", rel_str, e),
                    ToolResultMeta {
                        relative_path: Some(rel_str),
                        ..Default::default()
                    },
                );
            }
        }
    }

    // Validate UTF-8 (if cut at 8 MiB scan boundary, check valid prefix)
    let text = match std::str::from_utf8(&raw_bytes) {
        Ok(s) => s,
        Err(e) => {
            if hit_scan_cap && e.error_len().is_none() {
                unsafe { std::str::from_utf8_unchecked(&raw_bytes[..e.valid_up_to()]) }
            } else {
                return ToolExecutionOutput::error(
                    ErrorCode::BinaryOrInvalidUtf8,
                    format!("file '{}' is not valid UTF-8", rel_str),
                    ToolResultMeta {
                        relative_path: Some(rel_str),
                        duration_ms: started.elapsed().as_millis() as u64,
                        ..Default::default()
                    },
                );
            }
        }
    };

    let sha256_display = if let Some(h) = hasher {
        let digest = h.finalize();
        let hex = hex_encode(&digest);
        hex[..16].to_string()
    } else {
        "skipped_large_file".to_string()
    };

    // Strip optional UTF-8 BOM for line display while keeping SHA256 over raw file bytes
    let body_text = text.strip_prefix('\u{FEFF}').unwrap_or(text);

    let start_line = offset;
    let mut total_lines = 0usize;
    let mut formatted_body = String::new();
    let mut end_line = start_line.saturating_sub(1);
    let mut truncated_by_cap = false;

    if !body_text.is_empty() {
        let trimmed_body = body_text.strip_suffix('\n').unwrap_or(body_text);
        for raw_line in trimmed_body.split('\n') {
            total_lines += 1;
            let line_num = total_lines;
            if line_num < start_line || truncated_by_cap {
                continue;
            }
            if line_num - start_line >= effective_limit {
                truncated_by_cap = true;
                continue;
            }
            let line_content = raw_line.strip_suffix('\r').unwrap_or(raw_line);
            let formatted_line = if line_content.len() > MAX_SINGLE_LINE_BYTES {
                let cut = floor_char_boundary(line_content, MAX_SINGLE_LINE_BYTES);
                format!("{:>5}\t{}[line truncated]\n", line_num, &line_content[..cut])
            } else {
                format!("{:>5}\t{}\n", line_num, line_content)
            };

            if formatted_body.len() + formatted_line.len() > MAX_INLINE_OUTPUT_BYTES
                && !formatted_body.is_empty()
            {
                truncated_by_cap = true;
                continue;
            }
            formatted_body.push_str(&formatted_line);
            end_line = line_num;
        }
    }

    let more_lines_remain = end_line < total_lines;
    let truncated = truncated_by_cap || more_lines_remain || hit_scan_cap;

    let (display_start, display_end) = if total_lines == 0 {
        (0, 0)
    } else if start_line > total_lines {
        (total_lines, total_lines)
    } else {
        (start_line, end_line)
    };

    let mut output = format!(
        "[File: {} | Lines: {}-{} of {} | SHA256: {}]\n",
        rel_str, display_start, display_end, total_lines, sha256_display
    );
    output.push_str(&formatted_body);

    if start_line > total_lines && total_lines > 0 {
        output.push_str(&format!(
            "[Requested offset={} exceeds file length ({} lines).]\n",
            start_line, total_lines
        ));
    } else if more_lines_remain {
        output.push_str(&format!(
            "[Truncated at line {}. Call read with offset={} to continue.]\n",
            display_end,
            display_end + 1
        ));
    } else if hit_scan_cap {
        output.push_str(&format!(
            "[Scan limit ({} MiB) reached at line {}. Use grep to locate lines beyond the scan cap.]\n",
            MAX_READ_SCAN_BYTES / (1024 * 1024),
            display_end
        ));
    }

    let content_sha256 = (sha256_display != "skipped_large_file").then_some(sha256_display);

    ToolExecutionOutput::success(
        output,
        ToolResultMeta {
            relative_path: Some(rel_str),
            content_sha256,
            lines_range: Some((display_start, display_end)),
            duration_ms: started.elapsed().as_millis() as u64,
            truncated,
            ..Default::default()
        },
    )
}
