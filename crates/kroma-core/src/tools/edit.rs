use crate::messages::ToolResultMeta;
use crate::prompt::{sha256_hex16, sha256_hex64};
use crate::tools::write::atomic_write_at;
use crate::tools::ToolExecutionOutput;
use crate::types::{EditDiagKind, EditMatchTier, ErrorCode};
use crate::workspace::{InodeMutationLock, WorkspaceCap};
use serde::Deserialize;
use std::time::Instant;

pub const MAX_EDITS_PER_CALL: usize = 16;
pub const MAX_DIAGNOSTIC_FILE_BYTES: usize = 256 * 1024; // 256 KiB

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EditItem {
    pub old_text: String,
    pub new_text: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EditArgs {
    pub path: String,
    pub edits: Vec<EditItem>,
    pub expected_sha256: Option<String>,
}

#[derive(Debug, Clone)]
struct ValidatedSpan {
    edit_index: usize,
    byte_start: usize,
    byte_end: usize,
    start_line: usize,
    end_line: usize,
    replacement_bytes: Vec<u8>,
    tier: EditMatchTier,
}

#[derive(Debug, Clone)]
struct EditItemDiag {
    error_code: Option<ErrorCode>,
    diag_kind: Option<EditDiagKind>,
    detail: String,
}

#[derive(Debug, Clone)]
struct RawLineSlice<'a> {
    content_no_eol: &'a str,
    trimmed_end: &'a str,
    byte_start: usize,
    byte_end_no_eol: usize,
    byte_end_with_eol: usize,
    eol: &'a str,
}

pub async fn execute_edit(
    workspace: &WorkspaceCap,
    inode_locks: &InodeMutationLock,
    raw_args: &str,
    max_edit_file_bytes: usize,
    max_edit_input_bytes: usize,
) -> ToolExecutionOutput {
    let started = Instant::now();
    let args: EditArgs = match serde_json::from_str(raw_args) {
        Ok(a) => a,
        Err(e) => {
            return ToolExecutionOutput::error(
                ErrorCode::InvalidArguments,
                format!("invalid JSON arguments for `edit`: {}", e),
                ToolResultMeta::default(),
            );
        }
    };

    if args.edits.is_empty() || args.edits.len() > MAX_EDITS_PER_CALL {
        return ToolExecutionOutput::error(
            ErrorCode::InvalidArguments,
            format!(
                "`edits` must contain between 1 and {} items, got {}",
                MAX_EDITS_PER_CALL,
                args.edits.len()
            ),
            ToolResultMeta {
                relative_path: Some(args.path),
                ..Default::default()
            },
        );
    }

    let total_input_bytes: usize = args
        .edits
        .iter()
        .map(|e| e.old_text.len() + e.new_text.len())
        .sum();
    if total_input_bytes > max_edit_input_bytes {
        return ToolExecutionOutput::error(
            ErrorCode::InvalidArguments,
            format!(
                "total `edits` input size ({} bytes) exceeds limit ({} bytes)",
                total_input_bytes, max_edit_input_bytes
            ),
            ToolResultMeta {
                relative_path: Some(args.path),
                ..Default::default()
            },
        );
    }

    // Pre-check for `[line truncated]` literal marker (§5.2)
    for (idx, item) in args.edits.iter().enumerate() {
        if item.old_text.contains("[line truncated]") || item.new_text.contains("[line truncated]")
        {
            return ToolExecutionOutput::error(
                ErrorCode::LineTruncatedMarker,
                format!(
                    "edits[{}] contains literal '[line truncated]' marker from truncated `read` output. \
                    Narrow your single-line replacement range and do not include '[line truncated]'.",
                    idx
                ),
                ToolResultMeta {
                    relative_path: Some(args.path),
                    diag_kind: Some(EditDiagKind::LineTruncatedMarker),
                    ..Default::default()
                },
            );
        }
        if item.old_text.is_empty() {
            return ToolExecutionOutput::error(
                ErrorCode::InvalidArguments,
                format!("edits[{}].old_text must not be empty", idx),
                ToolResultMeta {
                    relative_path: Some(args.path),
                    ..Default::default()
                },
            );
        }
    }

    // Resolve path and forbid symlink leaf
    let (norm_rel, _abs_path) = match workspace.resolve_existing_path(&args.path, true) {
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
    let rel_str = norm_rel.to_string_lossy().replace('\\', "/");

    let (parent_dir, file_name) = match workspace.open_parent_dir_no_symlink(&norm_rel, false) {
        Ok(res) => res,
        Err(e) => {
            return ToolExecutionOutput::error(
                e.error_code(),
                e.to_string(),
                ToolResultMeta {
                    relative_path: Some(rel_str),
                    ..Default::default()
                },
            );
        }
    };

    // F-07: key the mutation lock off the already-open parent directory handle + leaf file name
    // (matching `execute_write`), and acquire the lock BEFORE opening the target file so
    // concurrent `write`/`edit` calls on the same path are strictly serialized.
    let (_lock_key, _guard) = match inode_locks
        .acquire_for_file(&parent_dir, Some(file_name.as_os_str()))
        .await
    {
        Ok(g) => g,
        Err(e) => {
            return ToolExecutionOutput::error(
                ErrorCode::IoError,
                format!("failed to acquire inode lock on '{}': {}", rel_str, e),
                ToolResultMeta {
                    relative_path: Some(rel_str),
                    ..Default::default()
                },
            );
        }
    };

    // Open existing file via race-free open_existing_no_symlink while holding the parent+leaf lock
    let (_, mut file) = match workspace.open_existing_no_symlink(&args.path) {
        Ok(f) => f,
        Err(e) => {
            return ToolExecutionOutput::error(
                e.error_code(),
                e.to_string(),
                ToolResultMeta {
                    relative_path: Some(rel_str),
                    ..Default::default()
                },
            );
        }
    };

    let sym_meta = match file.metadata() {
        Ok(m) => m,
        Err(e) => {
            return ToolExecutionOutput::error(
                ErrorCode::FileNotFound,
                format!("failed to stat '{}': {}", rel_str, e),
                ToolResultMeta {
                    relative_path: Some(rel_str),
                    ..Default::default()
                },
            );
        }
    };
    if !sym_meta.file_type().is_file() {
        return ToolExecutionOutput::error(
            ErrorCode::InvalidArguments,
            format!("'{}' is not a regular file", rel_str),
            ToolResultMeta {
                relative_path: Some(rel_str),
                ..Default::default()
            },
        );
    }
    if sym_meta.len() > max_edit_file_bytes as u64 {
        return ToolExecutionOutput::error(
            ErrorCode::InvalidArguments,
            format!(
                "file '{}' size ({} bytes) exceeds max_edit_file_bytes ({} bytes)",
                rel_str,
                sym_meta.len(),
                max_edit_file_bytes
            ),
            ToolResultMeta {
                relative_path: Some(rel_str),
                ..Default::default()
            },
        );
    }

    // T06 (KR-06): the `sym_meta` length check above is a friendly pre-check, not the boundary. The
    // read streams at most `max_edit_file_bytes + 1` bytes from the verified handle, so a file that
    // grows after the check (or a sparse/hostile file) cannot allocate past the limit.
    let raw_bytes = match crate::secure_fs::read_bounded_handle(
        &mut file,
        &rel_str,
        max_edit_file_bytes as u64,
        crate::secure_fs::Overflow::Refuse,
    ) {
        Ok(read) => read.bytes,
        Err(crate::secure_fs::SecureFsError::TooLarge { .. }) => {
            return ToolExecutionOutput::error(
                ErrorCode::InvalidArguments,
                format!(
                    "file '{}' grew beyond max_edit_file_bytes ({} bytes) while being read",
                    rel_str, max_edit_file_bytes
                ),
                ToolResultMeta {
                    relative_path: Some(rel_str),
                    ..Default::default()
                },
            )
        }
        Err(e) => {
            return ToolExecutionOutput::error(
                ErrorCode::IoError,
                format!("failed to read '{}': {}", rel_str, e),
                ToolResultMeta {
                    relative_path: Some(rel_str),
                    ..Default::default()
                },
            )
        }
    };

    if raw_bytes.contains(&0u8) {
        return ToolExecutionOutput::error(
            ErrorCode::BinaryOrInvalidUtf8,
            format!("file '{}' contains NUL bytes (binary)", rel_str),
            ToolResultMeta {
                relative_path: Some(rel_str),
                ..Default::default()
            },
        );
    }

    let raw_str = match std::str::from_utf8(&raw_bytes) {
        Ok(s) => s,
        Err(_) => {
            return ToolExecutionOutput::error(
                ErrorCode::BinaryOrInvalidUtf8,
                format!("file '{}' is not valid UTF-8", rel_str),
                ToolResultMeta {
                    relative_path: Some(rel_str),
                    ..Default::default()
                },
            );
        }
    };

    let current_sha64 = sha256_hex64(&raw_bytes);
    let current_sha16 = current_sha64[..16].to_string();

    let has_bom = raw_str.starts_with('\u{FEFF}');
    let body_raw = if has_bom { &raw_str[3..] } else { raw_str };
    let file_lines = parse_raw_lines(body_raw);
    let crlf_count = file_lines.iter().filter(|l| l.eol == "\r\n").count();
    let lf_count = file_lines.iter().filter(|l| l.eol == "\n").count();
    let dominant_eol = if crlf_count >= lf_count && crlf_count > 0 {
        "\r\n"
    } else {
        "\n"
    };

    // Unified 16-char / 64-char expected_sha256 CAS check (§5.2 V6-04)
    if let Some(ref expected) = args.expected_sha256 {
        let exp_norm = expected.trim().to_ascii_lowercase();
        if exp_norm.len() != 16 && exp_norm.len() != 64 {
            return ToolExecutionOutput::error(
                ErrorCode::InvalidArguments,
                format!(
                    "`expected_sha256` must be 16 or 64 hex characters, got '{}'",
                    expected
                ),
                ToolResultMeta {
                    relative_path: Some(rel_str),
                    content_sha256: Some(current_sha16),
                    ..Default::default()
                },
            );
        }
        if !current_sha64.starts_with(&exp_norm) {
            let snippet = build_stale_file_snippet(&file_lines, &args.edits[0].old_text);
            return ToolExecutionOutput::error(
                ErrorCode::StaleFile,
                format!(
                    "expected SHA256 {}, but current file '{}' has SHA256: {}.\n\
                    [Current file snippet (SHA256: {})]:\n{}",
                    exp_norm, rel_str, current_sha16, current_sha16, snippet
                ),
                ToolResultMeta {
                    relative_path: Some(rel_str),
                    content_sha256: Some(current_sha16),
                    diag_kind: Some(EditDiagKind::StaleFile),
                    ..Default::default()
                },
            );
        }
    }

    // Validate all edits against the same raw snapshot
    let mut validated_spans: Vec<ValidatedSpan> = Vec::with_capacity(args.edits.len());
    let mut diagnostics: Vec<EditItemDiag> = Vec::with_capacity(args.edits.len());
    let mut any_failed = false;

    for (idx, item) in args.edits.iter().enumerate() {
        let old_norm = normalize_crlf(&item.old_text);
        let new_norm = normalize_crlf(&item.new_text);
        let (span, diag) = match_one_edit(
            idx,
            &old_norm,
            &new_norm,
            body_raw,
            &file_lines,
            dominant_eol,
        );
        match span {
            Some(span) => validated_spans.push(span),
            None => any_failed = true,
        }
        diagnostics.push(diag);
    }

    if any_failed {
        let primary_code = diagnostics
            .iter()
            .find_map(|d| d.error_code)
            .unwrap_or(ErrorCode::MatchNotFound);
        let primary_diag = diagnostics.iter().find_map(|d| d.diag_kind);
        let summary = diagnostics
            .iter()
            .map(|d| d.detail.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        return ToolExecutionOutput::error(
            primary_code,
            format!(
                "All-or-None edit validation failed for '{}' (SHA256: {}):\n{}",
                rel_str, current_sha16, summary
            ),
            ToolResultMeta {
                relative_path: Some(rel_str),
                content_sha256: Some(current_sha16),
                duration_ms: started.elapsed().as_millis() as u64,
                diag_kind: primary_diag,
                ..Default::default()
            },
        );
    }

    // Check for overlapping edit spans
    let mut sorted_spans = validated_spans.clone();
    sorted_spans.sort_by_key(|s| (s.byte_start, s.byte_end));
    for w in sorted_spans.windows(2) {
        if w[0].byte_end > w[1].byte_start {
            return ToolExecutionOutput::error(
                ErrorCode::OverlappingEdits,
                format!(
                    "edits[{}] (lines {}-{}) overlaps with edits[{}] (lines {}-{}); merge overlapping edits into a single item",
                    w[0].edit_index,
                    w[0].start_line,
                    w[0].end_line,
                    w[1].edit_index,
                    w[1].start_line,
                    w[1].end_line
                ),
                ToolResultMeta {
                    relative_path: Some(rel_str),
                    content_sha256: Some(current_sha16),
                    diag_kind: Some(EditDiagKind::OverlappingEdits),
                    ..Default::default()
                },
            );
        }
    }

    // Apply replacements from back to front on the raw body bytes
    let mut new_body_bytes = body_raw.as_bytes().to_vec();
    for span in sorted_spans.iter().rev() {
        new_body_bytes.splice(
            span.byte_start..span.byte_end,
            span.replacement_bytes.iter().copied(),
        );
    }

    let mut final_bytes = Vec::with_capacity(new_body_bytes.len() + if has_bom { 3 } else { 0 });
    if has_bom {
        final_bytes.extend_from_slice("\u{FEFF}".as_bytes());
    }
    final_bytes.extend_from_slice(&new_body_bytes);

    #[cfg(unix)]
    let mode = {
        use std::os::unix::fs::PermissionsExt;
        sym_meta.permissions().mode()
    };
    #[cfg(windows)]
    let mode = if sym_meta.permissions().readonly() {
        0o444
    } else {
        0o644
    };
    if let Err((code, msg)) = atomic_write_at(&parent_dir, &file_name, &final_bytes, mode, false) {
        return ToolExecutionOutput::error(
            code,
            msg,
            ToolResultMeta {
                relative_path: Some(rel_str),
                ..Default::default()
            },
        );
    }

    let new_sha16 = sha256_hex16(&final_bytes);
    let min_line = sorted_spans.iter().map(|s| s.start_line).min().unwrap_or(1);
    let max_line = sorted_spans.iter().map(|s| s.end_line).max().unwrap_or(1);
    let highest_tier = if sorted_spans
        .iter()
        .any(|s| s.tier == EditMatchTier::Tier2TrailingWhitespace)
    {
        EditMatchTier::Tier2TrailingWhitespace
    } else {
        EditMatchTier::Tier1Exact
    };

    let new_body_str = String::from_utf8_lossy(&new_body_bytes);
    let diff_preview = build_unified_diff(&rel_str, body_raw, &new_body_str, min_line, max_line);

    ToolExecutionOutput::success(
        format!(
            "[Edited: {} | Applied: {} edit(s) at lines {}-{} | SHA256: {}]\n{}",
            rel_str,
            sorted_spans.len(),
            min_line,
            max_line,
            new_sha16,
            diff_preview
        ),
        ToolResultMeta {
            relative_path: Some(rel_str),
            content_sha256: Some(new_sha16),
            lines_range: Some((min_line, max_line)),
            duration_ms: started.elapsed().as_millis() as u64,
            edit_match_tier: Some(highest_tier),
            modified_file: true,
            ..Default::default()
        },
    )
}

/// `MATCH_NOT_UNIQUE` diagnostic for an `old_text` that matched more than once.
///
/// Both tiers produce exactly this message, so it lives here rather than being spelled out twice.
fn not_unique_diag(idx: usize, line_nums: &[usize]) -> EditItemDiag {
    EditItemDiag {
        error_code: Some(ErrorCode::MatchNotUnique),
        diag_kind: Some(EditDiagKind::MatchNotUnique),
        detail: format!(
            "edits[{}]: MATCH_NOT_UNIQUE (Matched at lines {:?}. Include 2-3 surrounding unique lines in old_text to disambiguate.)",
            idx, line_nums
        ),
    }
}

/// Match one `edits[]` item against the raw snapshot and describe the outcome.
///
/// Pure: it only reads the parsed snapshot plus the normalized old/new text, so the whole Tier 1 /
/// Tier 2 / mismatch-diagnostic policy is unit-testable without a workspace, a mutation lock or an
/// open file handle. `Some(span)` means the item matched and is ready to apply; the diagnostic is
/// always returned because the caller reports it on both paths.
fn match_one_edit(
    idx: usize,
    old_norm: &str,
    new_norm: &str,
    body_raw: &str,
    file_lines: &[RawLineSlice<'_>],
    dominant_eol: &str,
) -> (Option<ValidatedSpan>, EditItemDiag) {
    // `execute_edit` rejects an empty `old_text` earlier (InvalidArguments), so this is currently
    // unreachable in production -- but this is the pure matching boundary and an empty needle walks
    // the substring search past the end of the buffer (a panic, not an error). Guard it here so the
    // precondition is enforced where it is relied on, and so a future caller cannot reintroduce it.
    if old_norm.is_empty() {
        return (
            None,
            EditItemDiag {
                error_code: Some(ErrorCode::InvalidArguments),
                diag_kind: None,
                detail: format!("edits[{}]: old_text must not be empty", idx),
            },
        );
    }

    let tier2_matches = find_tier2_trailing_ws_matches(file_lines, old_norm);
    // Tier 1: Exact match after CRLF/CR -> LF normalization (only accept as unique if Tier 2
    // does not reveal additional trailing-whitespace-equivalent duplicates elsewhere in the file).
    let tier1_matches = find_tier1_exact_matches(body_raw, file_lines, old_norm);
    if tier1_matches.len() == 1 && tier2_matches.len() <= 1 {
        let (byte_start, byte_end, start_line, end_line) = tier1_matches[0];
        let replacement_str = apply_eol_style(new_norm, dominant_eol);
        return (
            Some(ValidatedSpan {
                edit_index: idx,
                byte_start,
                byte_end,
                start_line,
                end_line,
                replacement_bytes: replacement_str.into_bytes(),
                tier: EditMatchTier::Tier1Exact,
            }),
            EditItemDiag {
                error_code: None,
                diag_kind: None,
                detail: format!("edits[{}]: OK (lines {}-{})", idx, start_line, end_line),
            },
        );
    } else if tier1_matches.len() > 1 {
        let line_nums: Vec<usize> = tier1_matches.iter().map(|m| m.2).collect();
        return (None, not_unique_diag(idx, &line_nums));
    }

    // Tier 2: Trailing horizontal whitespace tolerant match (preserving unchanged lines, indentation, BOM & CRLF)
    if tier2_matches.len() == 1 {
        let (byte_start, byte_end, start_line, end_line) = tier2_matches[0];
        let replacement_str = apply_eol_style(new_norm, dominant_eol);
        return (
            Some(ValidatedSpan {
                edit_index: idx,
                byte_start,
                byte_end,
                start_line,
                end_line,
                replacement_bytes: replacement_str.into_bytes(),
                tier: EditMatchTier::Tier2TrailingWhitespace,
            }),
            EditItemDiag {
                error_code: None,
                diag_kind: None,
                detail: format!(
                    "edits[{}]: OK via Tier 2 trailing-whitespace match (lines {}-{})",
                    idx, start_line, end_line
                ),
            },
        );
    }
    if tier2_matches.len() > 1 {
        let line_nums: Vec<usize> = tier2_matches.iter().map(|m| m.2).collect();
        return (None, not_unique_diag(idx, &line_nums));
    }

    // Neither Tier 1 nor Tier 2 matched: run actionable mismatch diagnostics (§5.2)
    let (diag_kind, diag_hint) = diagnose_mismatch(body_raw, file_lines, old_norm);
    (
        None,
        EditItemDiag {
            error_code: Some(ErrorCode::MatchNotFound),
            diag_kind: Some(diag_kind),
            detail: format!("edits[{}]: MATCH_NOT_FOUND\n{}", idx, diag_hint),
        },
    )
}

fn parse_raw_lines(body: &str) -> Vec<RawLineSlice<'_>> {
    if body.is_empty() {
        return Vec::new();
    }
    let mut lines = Vec::new();
    let bytes = body.as_bytes();
    let mut start = 0;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\n' {
            let (end_no_eol, eol) = if i > start && bytes[i - 1] == b'\r' {
                (i - 1, "\r\n")
            } else {
                (i, "\n")
            };
            let content_no_eol = &body[start..end_no_eol];
            let trimmed_end = content_no_eol.trim_end_matches([' ', '\t']);
            lines.push(RawLineSlice {
                content_no_eol,
                trimmed_end,
                byte_start: start,
                byte_end_no_eol: end_no_eol,
                byte_end_with_eol: i + 1,
                eol,
            });
            start = i + 1;
        }
        i += 1;
    }
    if start < bytes.len() {
        let content_no_eol = &body[start..bytes.len()];
        let trimmed_end = content_no_eol.trim_end_matches([' ', '\t']);
        lines.push(RawLineSlice {
            content_no_eol,
            trimmed_end,
            byte_start: start,
            byte_end_no_eol: bytes.len(),
            byte_end_with_eol: bytes.len(),
            eol: "",
        });
    }
    lines
}

fn normalize_crlf(s: &str) -> String {
    s.replace("\r\n", "\n").replace('\r', "\n")
}

fn apply_eol_style(lf_str: &str, eol: &str) -> String {
    if eol == "\r\n" {
        lf_str.replace('\n', "\r\n")
    } else {
        lf_str.to_string()
    }
}

/// Find exact matches after CRLF->LF normalization and map them back to exact byte offsets and 1-indexed line numbers in `body_raw`.
fn find_tier1_exact_matches(
    body_raw: &str,
    file_lines: &[RawLineSlice<'_>],
    old_norm: &str,
) -> Vec<(usize, usize, usize, usize)> {
    // Build LF-normalized body along with a mapping from normalized byte offset -> raw byte offset
    let raw_bytes = body_raw.as_bytes();
    let mut norm_bytes = Vec::with_capacity(raw_bytes.len());
    let mut norm_to_raw = Vec::with_capacity(raw_bytes.len() + 1);

    let mut i = 0;
    while i < raw_bytes.len() {
        if raw_bytes[i] == b'\r' && i + 1 < raw_bytes.len() && raw_bytes[i + 1] == b'\n' {
            norm_to_raw.push(i);
            norm_bytes.push(b'\n');
            i += 2;
        } else {
            norm_to_raw.push(i);
            norm_bytes.push(raw_bytes[i]);
            i += 1;
        }
    }
    norm_to_raw.push(raw_bytes.len());

    let norm_str = unsafe { std::str::from_utf8_unchecked(&norm_bytes) };
    let mut matches = Vec::new();
    let mut search_from = 0;
    while let Some(rel_pos) = norm_str[search_from..].find(old_norm) {
        let norm_start = search_from + rel_pos;
        let norm_end = norm_start + old_norm.len();

        // If `old_norm` starts at a line boundary and stops right before trailing `[ \t]+` at EOL,
        // defer to Tier 2 so the whole line (including its trailing whitespace) is replaced cleanly.
        let starts_at_line_bol = norm_start == 0 || norm_bytes[norm_start - 1] == b'\n';
        let mut rem_idx = norm_end;
        while rem_idx < norm_bytes.len()
            && (norm_bytes[rem_idx] == b' ' || norm_bytes[rem_idx] == b'\t')
        {
            rem_idx += 1;
        }
        let has_unmatched_trailing_ws_at_eol = starts_at_line_bol
            && rem_idx > norm_end
            && (rem_idx == norm_bytes.len() || norm_bytes[rem_idx] == b'\n');

        if !has_unmatched_trailing_ws_at_eol {
            let raw_start = norm_to_raw[norm_start];
            let raw_end = norm_to_raw[norm_end];

            let start_line = byte_offset_to_line_num(file_lines, raw_start);
            let end_line = byte_offset_to_line_num(file_lines, raw_end.saturating_sub(1));
            matches.push((raw_start, raw_end, start_line, end_line));
        }

        search_from = norm_start + 1;
        while search_from < norm_str.len() && !norm_str.is_char_boundary(search_from) {
            search_from += 1;
        }
    }
    matches
}

fn byte_offset_to_line_num(file_lines: &[RawLineSlice<'_>], byte_offset: usize) -> usize {
    for (idx, line) in file_lines.iter().enumerate() {
        if byte_offset >= line.byte_start
            && byte_offset < line.byte_end_with_eol.max(line.byte_start + 1)
        {
            return idx + 1;
        }
    }
    file_lines.len().max(1)
}

/// Tier 2: Line-sequence match ignoring ONLY trailing spaces/tabs (`[ \t]+$`) on each line.
fn find_tier2_trailing_ws_matches(
    file_lines: &[RawLineSlice<'_>],
    old_norm: &str,
) -> Vec<(usize, usize, usize, usize)> {
    let old_has_trailing_nl = old_norm.ends_with('\n');
    let old_trimmed_nl = old_norm.strip_suffix('\n').unwrap_or(old_norm);
    let old_lines: Vec<&str> = old_trimmed_nl
        .split('\n')
        .map(|l| l.trim_end_matches([' ', '\t']))
        .collect();

    if old_lines.is_empty() || old_lines.len() > file_lines.len() {
        return Vec::new();
    }

    let k = old_lines.len();
    let mut matches = Vec::new();
    for start_idx in 0..=(file_lines.len() - k) {
        let window_matches =
            (0..k).all(|offset| file_lines[start_idx + offset].trimmed_end == old_lines[offset]);
        if window_matches {
            let first_line = &file_lines[start_idx];
            let last_line = &file_lines[start_idx + k - 1];
            let byte_start = first_line.byte_start;
            let byte_end = if old_has_trailing_nl {
                last_line.byte_end_with_eol
            } else {
                last_line.byte_end_no_eol
            };
            matches.push((byte_start, byte_end, start_idx + 1, start_idx + k));
        }
    }
    matches
}

/// Actionable mismatch diagnostics (§5.2):
/// 1. Line number prefix pollution (`   42\t` or `42: `)
/// 2. Indentation mismatch (with exact 10-line file preview)
/// 3. Closest candidate line window (>= 50% similarity with +/- 3 context lines)
fn diagnose_mismatch(
    body_raw: &str,
    file_lines: &[RawLineSlice<'_>],
    old_norm: &str,
) -> (EditDiagKind, String) {
    let old_trimmed_nl = old_norm.strip_suffix('\n').unwrap_or(old_norm);
    let old_lines: Vec<&str> = old_trimmed_nl.split('\n').collect();

    // 1. Check for line number prefix pollution from `read` or `grep` output
    let non_empty_old: Vec<&str> = old_lines
        .iter()
        .copied()
        .filter(|l| !l.trim().is_empty())
        .collect();
    if !non_empty_old.is_empty() && non_empty_old.iter().all(|l| has_line_number_prefix(l)) {
        return (
            EditDiagKind::LineNumberPrefix,
            "[Hint: old_text appears to include line number prefixes from read output. Strip the leading line numbers and tab before retrying.]".to_string(),
        );
    }

    if body_raw.len() > MAX_DIAGNOSTIC_FILE_BYTES || file_lines.is_empty() {
        return (
            EditDiagKind::ClosestCandidate,
            "[Hint: Call `read` around the target section to verify exact line contents.]"
                .to_string(),
        );
    }

    // 2. Check for leading indentation mismatch (ignoring leading & trailing horizontal whitespace)
    let k = old_lines.len();
    if k <= file_lines.len() {
        let old_stripped: Vec<&str> = old_lines
            .iter()
            .map(|l| l.trim_matches([' ', '\t']))
            .collect();
        let mut indent_matches = Vec::new();
        for start_idx in 0..=(file_lines.len() - k) {
            let ok = (0..k).all(|off| {
                file_lines[start_idx + off]
                    .content_no_eol
                    .trim_matches([' ', '\t'])
                    == old_stripped[off]
            });
            if ok {
                indent_matches.push(start_idx);
            }
        }

        if let Some(&start_idx) = indent_matches.first() {
            let start_line = start_idx + 1;
            let end_line = start_idx + k;
            let file_indent = describe_indentation(
                file_lines[start_idx..start_idx + k]
                    .iter()
                    .map(|l| l.content_no_eol),
            );
            let old_indent = describe_indentation(old_lines.iter().copied());
            let ctx_start = start_line.saturating_sub(1).max(1);
            let ctx_end = (end_line + 1)
                .min(file_lines.len())
                .max(ctx_start + 9)
                .min(file_lines.len());
            let snippet = format_line_range(file_lines, ctx_start, ctx_end);
            return (
                EditDiagKind::IndentationMismatch,
                format!(
                    "[Indentation mismatch at lines {}-{}: file uses {}, whereas old_text used {}. Exact file lines {}-{} are shown below:]\n{}",
                    start_line, end_line, file_indent, old_indent, ctx_start, ctx_end, snippet
                ),
            );
        }
    }

    // 3. Find closest candidate window with similarity >= 50%
    let win_len = k.min(file_lines.len()).max(1);
    let mut best_score = 0.0f64;
    let mut best_start_idx = 0usize;

    for start_idx in 0..=(file_lines.len() - win_len) {
        let mut total_sim = 0.0;
        for off in 0..win_len {
            let f_line = file_lines[start_idx + off].content_no_eol.trim();
            let o_line = old_lines[off].trim();
            total_sim += line_similarity(f_line, o_line);
        }
        let avg_sim = total_sim / (win_len as f64);
        if avg_sim > best_score {
            best_score = avg_sim;
            best_start_idx = start_idx;
        }
    }

    if best_score >= 0.50 {
        let cand_start = best_start_idx + 1;
        let cand_end = best_start_idx + win_len;
        let ctx_start = cand_start.saturating_sub(3).max(1);
        let ctx_end = (cand_end + 3).min(file_lines.len());
        let snippet = format_line_range(file_lines, ctx_start, ctx_end);
        return (
            EditDiagKind::ClosestCandidate,
            format!(
                "[Closest candidate at lines {}-{} (similarity {:.0}%). Exact file lines {}-{} are shown below:]\n{}",
                cand_start,
                cand_end,
                best_score * 100.0,
                ctx_start,
                ctx_end,
                snippet
            ),
        );
    }

    let preview_end = file_lines.len().min(15);
    let snippet = format_line_range(file_lines, 1, preview_end);
    (
        EditDiagKind::ClosestCandidate,
        format!(
            "[No close match found (best similarity {:.0}%). File preview lines 1-{}:]\n{}",
            best_score * 100.0,
            preview_end,
            snippet
        ),
    )
}

fn has_line_number_prefix(line: &str) -> bool {
    let trimmed = line.trim_start_matches(' ');
    let digits_len = trimmed.bytes().take_while(|b| b.is_ascii_digit()).count();
    if digits_len == 0 || digits_len > 7 {
        return false;
    }
    let after_digits = &trimmed[digits_len..];
    after_digits.starts_with('\t')
        || after_digits.starts_with(": ")
        || after_digits.starts_with(" | ")
}

fn describe_indentation<'a, I>(lines: I) -> String
where
    I: Iterator<Item = &'a str>,
{
    for line in lines {
        if line.trim().is_empty() {
            continue;
        }
        let leading: String = line
            .chars()
            .take_while(|&c| c == ' ' || c == '\t')
            .collect();
        if leading.is_empty() {
            return "0-space indent".to_string();
        }
        if leading.contains('\t') {
            return format!(
                "{}-tab indent",
                leading.chars().filter(|&c| c == '\t').count()
            );
        }
        return format!("{}-space indent", leading.len());
    }
    "0-space indent".to_string()
}

fn line_similarity(a: &str, b: &str) -> f64 {
    if a == b {
        return 1.0;
    }
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    // Character bigram Sørensen–Dice coefficient
    let a_chars: Vec<char> = a.chars().collect();
    let b_chars: Vec<char> = b.chars().collect();
    if a_chars.len() == 1 || b_chars.len() == 1 {
        return if a_chars[0] == b_chars[0] { 0.5 } else { 0.0 };
    }
    let mut a_bigrams: Vec<(char, char)> = a_chars.windows(2).map(|w| (w[0], w[1])).collect();
    let mut b_bigrams: Vec<(char, char)> = b_chars.windows(2).map(|w| (w[0], w[1])).collect();
    a_bigrams.sort_unstable();
    b_bigrams.sort_unstable();

    let mut matches = 0usize;
    let (mut i, mut j) = (0, 0);
    while i < a_bigrams.len() && j < b_bigrams.len() {
        match a_bigrams[i].cmp(&b_bigrams[j]) {
            std::cmp::Ordering::Equal => {
                matches += 1;
                i += 1;
                j += 1;
            }
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
        }
    }
    (2.0 * matches as f64) / ((a_bigrams.len() + b_bigrams.len()) as f64)
}

fn format_line_range(
    file_lines: &[RawLineSlice<'_>],
    start_line: usize,
    end_line: usize,
) -> String {
    if file_lines.is_empty() || start_line == 0 || start_line > file_lines.len() {
        return String::new();
    }
    let s = start_line - 1;
    let e = end_line.min(file_lines.len());
    let mut out = String::new();
    for (idx, line) in file_lines.iter().enumerate().take(e).skip(s) {
        use std::fmt::Write;
        let line_str = line.content_no_eol;
        if line_str.len() > 2048 {
            let mut cut = 2048;
            while cut > 0 && !line_str.is_char_boundary(cut) {
                cut -= 1;
            }
            let _ = writeln!(
                &mut out,
                "{:>5}\t{}[line truncated]",
                idx + 1,
                &line_str[..cut]
            );
        } else {
            let _ = writeln!(&mut out, "{:>5}\t{}", idx + 1, line_str);
        }
    }
    out
}

fn build_stale_file_snippet(file_lines: &[RawLineSlice<'_>], first_old_text: &str) -> String {
    if file_lines.is_empty() {
        return "(empty file)".to_string();
    }
    let target_first = first_old_text
        .lines()
        .find(|l| !l.trim().is_empty())
        .map(|l| l.trim())
        .unwrap_or("");
    if !target_first.is_empty() {
        if let Some((idx, _)) = file_lines.iter().enumerate().max_by(|(_, a), (_, b)| {
            line_similarity(a.content_no_eol.trim(), target_first)
                .partial_cmp(&line_similarity(b.content_no_eol.trim(), target_first))
                .unwrap_or(std::cmp::Ordering::Equal)
        }) {
            let start = (idx + 1).saturating_sub(3).max(1);
            let end = (idx + 6).min(file_lines.len());
            return format_line_range(file_lines, start, end);
        }
    }
    format_line_range(file_lines, 1, file_lines.len().min(15))
}

fn build_unified_diff(
    rel_path: &str,
    old_body: &str,
    new_body: &str,
    min_line: usize,
    max_line: usize,
) -> String {
    let old_lines: Vec<&str> = old_body
        .lines()
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .collect();
    let new_lines: Vec<&str> = new_body
        .lines()
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .collect();

    let ctx_start = min_line.saturating_sub(3).max(1);
    let old_ctx_end = (max_line + 3).min(old_lines.len());
    let delta = new_lines.len() as isize - old_lines.len() as isize;
    let new_ctx_end = ((max_line as isize + delta + 3).max(1) as usize).min(new_lines.len());

    let mut diff = format!(
        "--- a/{}\n+++ b/{}\n@@ -{},{} +{},{} @@\n",
        rel_path,
        rel_path,
        ctx_start,
        old_ctx_end.saturating_sub(ctx_start - 1),
        ctx_start,
        new_ctx_end.saturating_sub(ctx_start - 1)
    );

    for l in old_lines
        .iter()
        .skip(ctx_start.saturating_sub(1))
        .take(old_ctx_end.saturating_sub(ctx_start - 1))
    {
        diff.push('-');
        diff.push_str(l);
        diff.push('\n');
    }
    for l in new_lines
        .iter()
        .skip(ctx_start.saturating_sub(1))
        .take(new_ctx_end.saturating_sub(ctx_start - 1))
    {
        diff.push('+');
        diff.push_str(l);
        diff.push('\n');
    }
    diff
}

#[cfg(test)]
mod match_tests {
    use super::*;

    /// Run the pure matcher over a raw body, the way `execute_edit` does after reading the file.
    fn run<'a>(
        body: &'a str,
        old: &str,
        new: &str,
        dominant_eol: &str,
    ) -> (Option<ValidatedSpan>, EditItemDiag, Vec<RawLineSlice<'a>>) {
        let file_lines = parse_raw_lines(body);
        let (span, diag) = match_one_edit(
            0,
            &normalize_crlf(old),
            &normalize_crlf(new),
            body,
            &file_lines,
            dominant_eol,
        );
        (span, diag, file_lines)
    }

    #[test]
    fn tier1_exact_match_covers_exactly_the_old_text() {
        let body = "a\nb\nc\n";
        let (span, diag, _) = run(body, "b\n", "B\n", "\n");
        let span = span.expect("a unique exact match must be applied");
        assert_eq!(span.tier, EditMatchTier::Tier1Exact);
        assert_eq!((span.start_line, span.end_line), (2, 2));
        assert_eq!(
            &body[span.byte_start..span.byte_end],
            "b\n",
            "the span must cover exactly the matched old text"
        );
        assert!(diag.error_code.is_none() && diag.diag_kind.is_none());
        assert!(diag.detail.contains("OK"), "got {}", diag.detail);
    }

    #[test]
    fn tier1_wins_when_the_exact_match_is_unique() {
        let (span, _, _) = run("let x = 1;\n", "let x = 1;\n", "let x = 2;\n", "\n");
        assert_eq!(
            span.expect("exact match").tier,
            EditMatchTier::Tier1Exact,
            "an exact hit must be reported as Tier 1, not Tier 2"
        );
    }

    #[test]
    fn a_crlf_body_matches_an_lf_old_text_and_the_span_keeps_the_crlf() {
        let body = "a\r\nb\r\n";
        let (span, _, _) = run(body, "b\n", "B\n", "\r\n");
        let span = span.expect("CRLF/CR -> LF normalization must let this match");
        assert_eq!(span.tier, EditMatchTier::Tier1Exact);
        assert_eq!(
            &body[span.byte_start..span.byte_end],
            "b\r\n",
            "the span must cover the raw CRLF bytes, not the normalized ones"
        );
    }

    #[test]
    fn replacement_is_written_back_in_the_dominant_eol_style() {
        let (span, _, _) = run("a\r\nb\r\n", "b\n", "B\n", "\r\n");
        assert_eq!(
            span.expect("match").replacement_bytes,
            b"B\r\n",
            "a CRLF file must not be rewritten with a bare LF"
        );
        let (span, _, _) = run("a\nb\n", "b\n", "B\n", "\n");
        assert_eq!(span.expect("match").replacement_bytes, b"B\n");
    }

    #[test]
    fn tier2_tolerates_trailing_whitespace_on_the_file_line() {
        let body = "let x = 1;   \n";
        let (span, diag, _) = run(body, "let x = 1;\n", "let x = 2;\n", "\n");
        let span = span.expect("trailing whitespace must be tolerated at Tier 2");
        assert_eq!(span.tier, EditMatchTier::Tier2TrailingWhitespace);
        assert!(diag.detail.contains("Tier 2"), "got {}", diag.detail);
    }

    #[test]
    fn an_ambiguous_old_text_is_refused_as_not_unique() {
        let (span, diag, _) = run("x\nx\n", "x\n", "y\n", "\n");
        assert!(span.is_none(), "an ambiguous edit must not be applied");
        assert_eq!(diag.error_code, Some(ErrorCode::MatchNotUnique));
        assert_eq!(diag.diag_kind, Some(EditDiagKind::MatchNotUnique));
        assert!(
            diag.detail.contains("MATCH_NOT_UNIQUE"),
            "got {}",
            diag.detail
        );
    }

    #[test]
    fn a_tier1_unique_hit_is_still_refused_when_tier2_reveals_duplicates() {
        // `x\n` appears exactly once byte-for-byte, but a second line differs only by trailing
        // whitespace. Accepting the Tier 1 hit would silently edit the wrong one of two
        // equivalent lines, so the documented rule refuses it.
        let (span, diag, _) = run("x   \nx\n", "x\n", "y\n", "\n");
        assert!(
            span.is_none(),
            "a Tier 1 hit with a Tier 2 duplicate elsewhere must not be applied"
        );
        assert_eq!(diag.error_code, Some(ErrorCode::MatchNotUnique));
    }

    #[test]
    fn a_missing_old_text_reports_match_not_found_with_a_diagnostic_kind() {
        let (span, diag, _) = run("a\n", "zzz\n", "y\n", "\n");
        assert!(span.is_none());
        assert_eq!(diag.error_code, Some(ErrorCode::MatchNotFound));
        assert!(
            diag.diag_kind.is_some(),
            "the mismatch must carry an actionable diagnostic kind"
        );
        assert!(
            diag.detail.contains("MATCH_NOT_FOUND"),
            "got {}",
            diag.detail
        );
    }

    #[test]
    fn an_empty_old_text_is_refused_instead_of_panicking() {
        // The substring search would run off the end of the buffer for an empty needle, so the pure
        // boundary must reject it rather than panic. `execute_edit` also pre-rejects it.
        let (span, diag, _) = run("a\n", "", "y\n", "\n");
        assert!(span.is_none(), "an empty old_text must not match");
        assert_eq!(diag.error_code, Some(ErrorCode::InvalidArguments));
        assert!(
            diag.detail.contains("must not be empty"),
            "got {}",
            diag.detail
        );
    }
}
