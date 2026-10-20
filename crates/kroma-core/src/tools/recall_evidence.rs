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
use crate::evidence_store::ArtifactStatus;
use crate::messages::ToolResultMeta;
use crate::tool::output_buffer::floor_char_boundary;
use crate::tools::ToolExecutionOutput;
use crate::types::ErrorCode;
use serde::Deserialize;

/// Hard cap on lines returned by a single recall (matches the tool contract).
pub const MAX_RECALL_LINES: usize = 200;
/// Hard cap on the numbered-line body returned by a single recall. The header and the continuation
/// footer are added around this body, so the response stays inside the tool's inline cap.
pub const MAX_RECALL_BYTES: usize = 32 * 1024;
/// Lines of context kept around each `pattern` match.
const PATTERN_CONTEXT_LINES: usize = 2;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecallEvidenceArgs {
    /// Exact `source_entry_id` (or the originating tool `call_id`) of a compacted tool result.
    pub source_entry_id: String,
    /// 1-based first line to return (default 1). Bounds the `pattern` scan as well.
    pub start_line: Option<usize>,
    /// 1-based last line to return (capped at `start_line + 199`). Clamps `pattern` matches and
    /// their ±2 context lines to the same window.
    pub end_line: Option<usize>,
    /// Case-insensitive substring filter; returns matching lines with ±2 lines of context, clamped
    /// to the `start_line..=end_line` window when one is given.
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
        // T15 (KR-11): say *why*. "No longer retained", "gone", "corrupt" and "forbidden" are
        // different facts, and collapsing them into one message would make a damaged store look like
        // a retention policy. C00-07: an artifact GC collected is answered as *unavailable*, and a
        // layer that has no answer says the content is unknown — neither is "empty" or "no match".
        let detail = match projector.evidence_status(&args.source_entry_id) {
            Some(ArtifactStatus::Gone) => {
                "the stored artifact is unavailable (collected by GC, deleted, or never written); \
                 its content is unknown, not empty"
                    .to_string()
            }
            Some(status) => status.describe(),
            None => "its raw bytes are no longer retained and no evidence layer can answer for \
                     their content (unknown, not empty)"
                .to_string(),
        };
        return ToolExecutionOutput::error(
            ErrorCode::InvalidArguments,
            format!(
                "evidence '{}' is indexed but {}",
                args.source_entry_id, detail
            ),
            ToolResultMeta::default(),
        );
    };

    let lines: Vec<&str> = raw.lines().collect();
    let total = lines.len();
    let sha16: String = link.original_sha256.chars().take(16).collect();

    if total == 0 {
        return finish(
            &args.source_entry_id,
            &link.tool_name,
            (0, 0),
            total,
            &sha16,
            RenderedLines {
                body: "[evidence is empty]\n".to_string(),
                last_line: 0,
                hit_byte_cap: false,
            },
            false,
        );
    }

    // `start_line` bounds the requested window in both the range and the `pattern` modes, and
    // `end_line` clamps it (capped at `start_line + MAX_RECALL_LINES - 1`).
    let window_start = args.start_line.unwrap_or(1).max(1);
    if window_start > total {
        return ToolExecutionOutput::error(
            ErrorCode::InvalidArguments,
            format!(
                "start_line {} exceeds the {} line(s) of evidence {}",
                window_start, total, args.source_entry_id
            ),
            ToolResultMeta::default(),
        );
    }
    let window_end = args.end_line.map(|end| end.clamp(window_start, total));

    if let Some(pattern) = args.pattern.as_deref().filter(|p| !p.is_empty()) {
        let needle = pattern.to_lowercase();
        let mut wanted: Vec<usize> = Vec::new();
        for (idx, line) in lines.iter().enumerate() {
            let line_no = idx + 1;
            if line_no < window_start || window_end.is_some_and(|end| line_no > end) {
                continue;
            }
            if line.to_lowercase().contains(&needle) {
                let from = idx.saturating_sub(PATTERN_CONTEXT_LINES);
                let to = (idx + PATTERN_CONTEXT_LINES).min(total - 1);
                for candidate in from..=to {
                    // Context lines are clamped to the requested window too, so following the
                    // continuation hint cannot re-serve lines outside it.
                    let candidate_no = candidate + 1;
                    if candidate_no < window_start
                        || window_end.is_some_and(|end| candidate_no > end)
                    {
                        continue;
                    }
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
        let line_truncated = wanted.len() > MAX_RECALL_LINES;
        wanted.truncate(MAX_RECALL_LINES);
        let first = wanted[0] + 1;
        let rendered = render_lines(&lines, &wanted);
        let last = rendered.last_line;
        // The header, the meta and the continuation hint must all name the lines actually
        // rendered — never the requested range — or the model skips the unrendered remainder.
        let truncated = line_truncated || rendered.hit_byte_cap || last < total;
        return finish(
            &args.source_entry_id,
            &link.tool_name,
            (first, last),
            total,
            &sha16,
            rendered,
            truncated,
        );
    }

    let max_end = (window_start + MAX_RECALL_LINES - 1).min(total);
    let end = window_end.unwrap_or(max_end).clamp(window_start, max_end);
    let selected: Vec<usize> = (window_start - 1..end).collect();
    let rendered = render_lines(&lines, &selected);
    let last = rendered.last_line;
    let truncated = end < total || rendered.hit_byte_cap;
    finish(
        &args.source_entry_id,
        &link.tool_name,
        (window_start, last),
        total,
        &sha16,
        rendered,
        truncated,
    )
}

/// The outcome of rendering a line selection, carrying the last line number actually emitted so
/// callers can report a truthful `Lines: X-Y of Z` header and continuation hint.
struct RenderedLines {
    body: String,
    /// 1-based number of the last line included in `body` (`0` when nothing was rendered).
    last_line: usize,
    /// True when the 32 KiB inline cap stopped the render early.
    hit_byte_cap: bool,
}

fn render_lines(lines: &[&str], selected: &[usize]) -> RenderedLines {
    let mut body = String::new();
    let mut previous: Option<usize> = None;
    let mut last_line = 0usize;
    let mut hit_byte_cap = false;
    for &idx in selected {
        let separator = match previous {
            Some(prev) if idx != prev + 1 => "...\n",
            _ => "",
        };
        let line = format!("{:>5}\t{}\n", idx + 1, lines[idx]);
        // Check the byte cap *before* appending, so a line that would overshoot is never added and
        // the reported last line stays truthful. The first line is always attempted so a single
        // oversized line still surfaces its prefix instead of an empty body.
        if !body.is_empty() && body.len() + separator.len() + line.len() > MAX_RECALL_BYTES {
            hit_byte_cap = true;
            break;
        }
        body.push_str(separator);
        body.push_str(&line);
        previous = Some(idx);
        last_line = idx + 1;
    }
    if body.is_empty() {
        body.push_str("[no lines selected]\n");
    }
    RenderedLines {
        body,
        last_line,
        hit_byte_cap,
    }
}

#[allow(clippy::too_many_arguments)]
fn finish(
    source_entry_id: &str,
    tool_name: &str,
    (start, end): (usize, usize),
    total: usize,
    sha16: &str,
    rendered: RenderedLines,
    truncated: bool,
) -> ToolExecutionOutput {
    let header = format!(
        "[Recalled Evidence: {} | Tool: {} | Lines: {}-{} of {} | SHA256: {}]\n",
        source_entry_id, tool_name, start, end, total, sha16
    );
    let mut body = rendered.body;
    // Clamp the body to the inline cap *before* adding the footer, so the continuation hint can
    // never be sliced off by the outer truncation below.
    if body.len() > MAX_RECALL_BYTES {
        let cut = floor_char_boundary(&body, MAX_RECALL_BYTES);
        body.truncate(cut);
        body.push_str("\n[recall truncated at the 32 KiB cap]\n");
    }
    if truncated {
        body.push_str(&format!(
            "[More evidence available. Call recall_evidence with start_line={} to continue.]\n",
            end + 1
        ));
    }
    let mut content = String::with_capacity(header.len() + body.len());
    content.push_str(&header);
    content.push_str(&body);
    ToolExecutionOutput::success(
        content,
        ToolResultMeta {
            relative_path: None,
            lines_range: Some((start, end)),
            ..Default::default()
        },
    )
}
