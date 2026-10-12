//! `recall_evidence` — read-only recovery of the raw tool output behind a compacted Outline Stub.
//!
//! Context Projection replaces large, superseded tool results with small Outline Stubs so the
//! provider request stays inside budget. Each stub carries a deterministic `source_entry_id`, and
//! the raw bytes stay addressable through [`ContextProjector`]'s evidence store (which is also
//! re-seeded from the session WAL on `--resume`). This tool lets the model pull back exact line
//! ranges — or lines matching a pattern — without ever re-reading the workspace file.
//!
//! It is pure: it never touches the filesystem, so it is safe under `--policy read-only`.

use crate::context_projector::ContextProjector;
use crate::messages::ToolResultMeta;
use crate::tool::output_buffer::floor_char_boundary;
use crate::tools::ToolExecutionOutput;
use crate::types::ErrorCode;
use serde::Deserialize;

/// Hard cap on lines returned by a single recall (matches the tool contract).
pub const MAX_RECALL_LINES: usize = 200;
/// Hard cap on bytes returned by a single recall.
pub const MAX_RECALL_BYTES: usize = 32 * 1024;
/// Lines of context kept around each `pattern` match.
const PATTERN_CONTEXT_LINES: usize = 2;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecallEvidenceArgs {
    /// Exact `source_entry_id` (or the originating tool `call_id`) of a compacted tool result.
    pub source_entry_id: String,
    /// 1-based first line to return (default 1).
    pub start_line: Option<usize>,
    /// 1-based last line to return (capped at `start_line + 199`).
    pub end_line: Option<usize>,
    /// Case-insensitive substring filter; returns matching lines with ±2 lines of context.
    pub pattern: Option<String>,
}

pub fn execute_recall_evidence(
    projector: &ContextProjector,
    raw_args: &str,
) -> ToolExecutionOutput {
    let args: RecallEvidenceArgs = match serde_json::from_str(raw_args) {
        Ok(a) => a,
        Err(e) => {
            return ToolExecutionOutput::error(
                ErrorCode::InvalidArguments,
                format!("invalid JSON arguments for `recall_evidence`: {}", e),
                ToolResultMeta::default(),
            );
        }
    };

    let link = match projector.evidence_link(&args.source_entry_id) {
        Some(link) => link.clone(),
        None => {
            let mut available: Vec<String> = projector
                .evidence_links()
                .iter()
                .map(|l| l.source_entry_id.clone())
                .collect();
            available.sort();
            let listing = if available.is_empty() {
                "none (no tool output has been compacted yet)".to_string()
            } else {
                available.join(", ")
            };
            return ToolExecutionOutput::error(
                ErrorCode::InvalidArguments,
                format!(
                    "unknown source_entry_id '{}'. Available compacted evidence ids: {}",
                    args.source_entry_id, listing
                ),
                ToolResultMeta::default(),
            );
        }
    };

    let Some(raw) = projector.evidence_lookup(&args.source_entry_id) else {
        return ToolExecutionOutput::error(
            ErrorCode::InvalidArguments,
            format!(
                "evidence '{}' is indexed but its raw bytes are no longer retained",
                args.source_entry_id
            ),
            ToolResultMeta::default(),
        );
    };

    let lines: Vec<&str> = raw.lines().collect();
    let total = lines.len();
    let sha16: String = link.original_sha256.chars().take(16).collect();

    if let Some(pattern) = args.pattern.as_deref().filter(|p| !p.is_empty()) {
        let needle = pattern.to_lowercase();
        let mut wanted: Vec<usize> = Vec::new();
        for (idx, line) in lines.iter().enumerate() {
            if line.to_lowercase().contains(&needle) {
                let from = idx.saturating_sub(PATTERN_CONTEXT_LINES);
                let to = (idx + PATTERN_CONTEXT_LINES).min(total.saturating_sub(1));
                for candidate in from..=to {
                    if !wanted.contains(&candidate) {
                        wanted.push(candidate);
                    }
                }
            }
        }
        wanted.sort_unstable();
        if wanted.is_empty() {
            return ToolExecutionOutput::error(
                ErrorCode::InvalidArguments,
                format!(
                    "pattern '{}' did not match any of the {} line(s) of evidence {}",
                    pattern, total, args.source_entry_id
                ),
                ToolResultMeta::default(),
            );
        }
        let truncated = wanted.len() > MAX_RECALL_LINES;
        wanted.truncate(MAX_RECALL_LINES);
        let first = wanted[0] + 1;
        let last = *wanted.last().unwrap() + 1;
        let body = render_lines(&lines, &wanted);
        return finish(
            &args.source_entry_id,
            &link.tool_name,
            (first, last),
            total,
            &sha16,
            body,
            truncated,
        );
    }

    if total == 0 {
        return finish(
            &args.source_entry_id,
            &link.tool_name,
            (0, 0),
            total,
            &sha16,
            "[evidence is empty]\n".to_string(),
            false,
        );
    }
    let start = args.start_line.unwrap_or(1).max(1);
    if start > total {
        return ToolExecutionOutput::error(
            ErrorCode::InvalidArguments,
            format!(
                "start_line {} exceeds the {} line(s) of evidence {}",
                start, total, args.source_entry_id
            ),
            ToolResultMeta::default(),
        );
    }
    let max_end = (start + MAX_RECALL_LINES - 1).min(total);
    let end = args.end_line.unwrap_or(max_end).clamp(start, max_end);
    let selected: Vec<usize> = (start - 1..end).collect();
    let body = render_lines(&lines, &selected);
    let truncated = end < total;
    finish(
        &args.source_entry_id,
        &link.tool_name,
        (start, end),
        total,
        &sha16,
        body,
        truncated,
    )
}

fn render_lines(lines: &[&str], selected: &[usize]) -> String {
    let mut body = String::new();
    let mut previous: Option<usize> = None;
    for &idx in selected {
        if let Some(prev) = previous {
            if idx != prev + 1 {
                body.push_str("...\n");
            }
        }
        if body.len() >= MAX_RECALL_BYTES {
            body.push_str("[recall truncated at the 32 KiB cap]\n");
            break;
        }
        body.push_str(&format!("{:>5}\t{}\n", idx + 1, lines[idx]));
        previous = Some(idx);
    }
    if body.is_empty() {
        body.push_str("[no lines selected]\n");
    }
    body
}

#[allow(clippy::too_many_arguments)]
fn finish(
    source_entry_id: &str,
    tool_name: &str,
    (start, end): (usize, usize),
    total: usize,
    sha16: &str,
    mut body: String,
    truncated: bool,
) -> ToolExecutionOutput {
    let header = format!(
        "[Recalled Evidence: {} | Tool: {} | Lines: {}-{} of {} | SHA256: {}]\n",
        source_entry_id, tool_name, start, end, total, sha16
    );
    if truncated {
        body.push_str(&format!(
            "[More evidence available. Call recall_evidence with start_line={} to continue.]\n",
            end + 1
        ));
    }
    let mut content = String::with_capacity(header.len() + body.len());
    content.push_str(&header);
    content.push_str(&body);
    // Keep the response inside the tool's inline cap even for pathological single lines.
    if content.len() > MAX_RECALL_BYTES + header.len() {
        let cut = floor_char_boundary(&content, MAX_RECALL_BYTES + header.len());
        content.truncate(cut);
        content.push_str("\n[recall truncated at the 32 KiB cap]\n");
    }
    ToolExecutionOutput::success(
        content,
        ToolResultMeta {
            relative_path: None,
            lines_range: Some((start, end)),
            ..Default::default()
        },
    )
}
