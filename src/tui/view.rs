#[path = "chrome.rs"]
mod chrome;
#[path = "draft.rs"]
mod draft;
#[path = "inspector.rs"]
mod inspector;
#[path = "messages.rs"]
mod messages;
#[path = "navigation.rs"]
mod navigation;
#[path = "palette.rs"]
mod palette;
#[path = "plain.rs"]
mod plain;
#[path = "plugin_panel.rs"]
mod plugin_panel;
#[path = "semantic.rs"]
mod semantic;
#[path = "status.rs"]
mod status;
#[path = "tool_records.rs"]
mod tool_records;
#[path = "transcript.rs"]
mod transcript;
#[path = "transcript_panel.rs"]
mod transcript_panel;
pub(super) use chrome::{display_session_title, NoticeLevel};

use super::event_queue::EventSender;
use super::runtime_events::UiEvent;
use super::tool_display::{compact_line, ellipsize_cells, tool_arguments, tool_result_details};
use super::{
    ask::{Key as AskKey, Overlay as AskOverlay, Slot as AskSlot},
    attachments::{Attachment, MAX_ATTACHMENTS},
    editor::Editor,
    links, markdown,
    picker::Picker,
    theme::{Theme, ThemeStore},
};
use crate::{plugins::PluginLayoutSpec, protocol::Message, sessions::SessionEntry};
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, Paragraph},
    Frame,
};
use serde_json::Value;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// Keep continuation lines under the content, not underneath list bullets or
/// code gutters. Terminal cells (not UTF-8 byte lengths) determine wrapping.
fn hanging_indent(line: &Line<'_>, width: usize) -> usize {
    let prefix = line
        .spans
        .iter()
        .take(3)
        .map(|span| span.content.as_ref())
        .collect::<String>();
    let chars = prefix.chars().take(24).collect::<Vec<_>>();
    if let Some(marker) = chars
        .iter()
        .position(|ch| matches!(ch, '•' | '◦' | '▪' | '│' | '▸' | '◆' | '▍'))
    {
        if marker < 16 && chars.get(marker + 1) == Some(&' ') {
            return (marker + 2).min(width.saturating_sub(1));
        }
    }
    let leading = chars.iter().take_while(|ch| **ch == ' ').count();
    let digits = chars[leading..]
        .iter()
        .take_while(|ch| ch.is_ascii_digit())
        .count();
    let dot = leading + digits;
    if digits > 0 && chars.get(dot) == Some(&'.') && chars.get(dot + 1) == Some(&' ') {
        return (dot + 2).min(width.saturating_sub(1));
    }
    chars
        .iter()
        .take_while(|ch| **ch == ' ')
        .count()
        .min(width.saturating_sub(1))
}

fn ascii_word(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '/' | '.' | ':' | '\\')
}

/// Styled lines wrapped to display cells. Avoid Ratatui's unstable
/// Paragraph::line_count, while keeping scroll position exact.
///
/// The width policy lives in `reading_area`: this function only wraps, so a
/// line never has to be measured against two different limits.
#[cfg(test)]
fn wrap_history(text: Text<'static>, width: usize) -> (Text<'static>, Vec<usize>) {
    wrap_document(text, width)
}

fn wrap_document(text: Text<'static>, width: usize) -> (Text<'static>, Vec<usize>) {
    wrap_document_with_preformatted(text, width, &[], &[])
}

/// `preformatted` contains source-row indexes whose interior whitespace is
/// content (code blocks and tool output), not prose layout. Keeping this as
/// explicit metadata avoids inferring code from theme colours.
fn wrap_document_with_preformatted(
    text: Text<'static>,
    width: usize,
    preformatted: &[usize],
    reading_rows: &[usize],
) -> (Text<'static>, Vec<usize>) {
    wrap_document_with_reading_limit(text, width, preformatted, reading_rows, 100)
}

fn wrap_document_with_reading_limit(
    text: Text<'static>,
    width: usize,
    preformatted: &[usize],
    reading_rows: &[usize],
    reading_limit: usize,
) -> (Text<'static>, Vec<usize>) {
    let width = width.max(1);
    let mut rows = Vec::new();
    // Keep a stable source-line identity for every visual row. The absolute
    // visual row index changes when the terminal width changes.
    let mut source_lines = Vec::new();
    for (source_index, line) in text.lines.into_iter().enumerate() {
        let width = if reading_rows.binary_search(&source_index).is_ok() {
            width.min(reading_limit.max(1))
        } else {
            width
        };
        let keep_leading_whitespace = preformatted.binary_search(&source_index).is_ok();
        let mut spans = Vec::new();
        let mut columns = 0usize;
        let hanging_indent = hanging_indent(&line, width);
        for span in line.spans {
            let style = span.style;
            let mut segment = String::new();
            if span.content.is_ascii() {
                // Most coding-agent logs are ASCII. Keep the inexpensive
                // char path; full grapheme segmentation is for mixed Unicode.
                let mut chars = span.content.chars().peekable();
                let mut preceding_ascii_word = false;
                while let Some(ch) = chars.next() {
                    let ascii = ascii_word(ch);
                    if ascii && !preceding_ascii_word && columns > hanging_indent {
                        let length = 1 + chars.clone().take_while(|next| ascii_word(*next)).count();
                        if length <= width.saturating_sub(hanging_indent)
                            && columns.saturating_add(length) > width
                        {
                            if !segment.is_empty() {
                                spans.push(Span::styled(std::mem::take(&mut segment), style));
                            }
                            rows.push(Line::from(std::mem::take(&mut spans)));
                            source_lines.push(source_index);
                            columns = hanging_indent;
                            if hanging_indent > 0 {
                                spans.push(Span::raw(" ".repeat(hanging_indent)));
                            }
                        }
                    }
                    if columns > 0 && columns + 1 > width {
                        if !segment.is_empty() {
                            spans.push(Span::styled(std::mem::take(&mut segment), style));
                        }
                        rows.push(Line::from(std::mem::take(&mut spans)));
                        source_lines.push(source_index);
                        columns = hanging_indent;
                        if hanging_indent > 0 {
                            spans.push(Span::raw(" ".repeat(hanging_indent)));
                        }
                    }
                    if ch == ' ' && columns == hanging_indent && !keep_leading_whitespace {
                        continue;
                    }
                    segment.push(ch);
                    columns = columns.saturating_add(1);
                    preceding_ascii_word = ascii;
                }
            } else {
                let mut graphemes = span.content.graphemes(true).peekable();
                let mut preceding_ascii_word = false;
                while let Some(grapheme) = graphemes.next() {
                    let cells = UnicodeWidthStr::width(grapheme);
                    let ascii =
                        grapheme.len() == 1 && grapheme.chars().next().is_some_and(ascii_word);
                    // Move a short ASCII token as a unit to the next row instead
                    // of splitting `run_with_resume` or `cargo test` arbitrarily.
                    if ascii && !preceding_ascii_word && columns > hanging_indent {
                        let word_length = 1 + graphemes
                            .clone()
                            .take_while(|next| {
                                next.len() == 1 && next.chars().next().is_some_and(ascii_word)
                            })
                            .count();
                        if word_length <= width.saturating_sub(hanging_indent)
                            && columns.saturating_add(word_length) > width
                        {
                            if !segment.is_empty() {
                                spans.push(Span::styled(std::mem::take(&mut segment), style));
                            }
                            rows.push(Line::from(std::mem::take(&mut spans)));
                            source_lines.push(source_index);
                            columns = hanging_indent;
                            if hanging_indent > 0 {
                                spans.push(Span::raw(" ".repeat(hanging_indent)));
                            }
                        }
                    }
                    if columns > 0 && columns + cells > width {
                        if !segment.is_empty() {
                            spans.push(Span::styled(std::mem::take(&mut segment), style));
                        }
                        rows.push(Line::from(std::mem::take(&mut spans)));
                        source_lines.push(source_index);
                        columns = hanging_indent;
                        if hanging_indent > 0 {
                            spans.push(Span::raw(" ".repeat(hanging_indent)));
                        }
                    }
                    if grapheme == " " && columns == hanging_indent && !keep_leading_whitespace {
                        continue;
                    }
                    segment.push_str(grapheme);
                    columns = columns.saturating_add(cells);
                    preceding_ascii_word = ascii;
                }
            }
            if !segment.is_empty() {
                spans.push(Span::styled(segment, style));
            }
        }
        rows.push(Line::from(spans));
        source_lines.push(source_index);
    }
    (Text::from(rows), source_lines)
}

/// Match the Markdown parser's labeled link fragments to the wrapped
/// underline spans. This keeps the real destination through line wrapping,
/// without smuggling OSC control sequences into Ratatui's character buffer.
fn markdown_link_regions(wrapped: &Text<'_>, targets: &[(String, String)]) -> Vec<links::Region> {
    let mut regions = Vec::new();
    let mut index = 0usize;
    let mut remaining = targets.first().map(|(text, _)| text.as_str()).unwrap_or("");
    for (row, line) in wrapped.lines.iter().enumerate() {
        let mut column = 0;
        for span in &line.spans {
            let content = span.content.as_ref();
            let width = UnicodeWidthStr::width(content);
            if span.style.add_modifier.contains(Modifier::UNDERLINED) && index < targets.len() {
                if width > 0 {
                    regions.push(links::Region {
                        row,
                        start: column,
                        end: column + width,
                        target: targets[index].1.clone(),
                    });
                }
                // Whitespace at a wrapped line boundary is removed by the
                // wrapping algorithm; matching by fragment keeps the next
                // adjacent Markdown link associated with its own target.
                remaining = remaining.trim_start();
                if let Some(rest) = remaining.strip_prefix(content) {
                    remaining = rest;
                } else {
                    let chars = content.chars().count();
                    let offset = remaining
                        .char_indices()
                        .nth(chars)
                        .map_or(remaining.len(), |(at, _)| at);
                    remaining = &remaining[offset..];
                }
                if remaining.trim().is_empty() {
                    index += 1;
                    remaining = targets
                        .get(index)
                        .map(|(text, _)| text.as_str())
                        .unwrap_or("");
                }
            }
            column += width;
        }
    }
    regions
}

/// Terminal-background gutter outside the right edge of plain message
/// surfaces. Text stops at the surface edge; this visible gutter separates the
/// surface from the scrollbar and keeps the safety margin legible.
pub(super) const PLAIN_RIGHT_GUTTER: u16 = 3;

/// The TUI occupies the terminal width. Comfortable mode limits only prose
/// wrapping, never the header, tool timeline, history viewport, or composer.
/// Leaving the entire app in a 160-cell island made maximized terminals feel
/// broken and forced long commands into unreadably narrow columns.
fn reading_area(area: Rect, full_width: bool) -> Rect {
    let margin = if full_width && area.width >= 100 {
        2
    } else {
        1
    };
    let max_available = area.width.saturating_sub(margin * 2);
    let available = if full_width {
        max_available
    } else {
        max_available.min(112)
    };
    let x = if full_width {
        area.x.saturating_add(margin)
    } else {
        area.x
            .saturating_add((area.width.saturating_sub(available)) / 2)
    };
    Rect::new(x, area.y, available, area.height)
}

fn body_line(line: Line<'static>) -> Line<'static> {
    // Share the composer text column; decoration stays outside copied text.
    let mut spans = vec![Span::raw("    ")];
    spans.extend(line.spans);
    Line::from(spans)
}

#[derive(Clone, Copy)]
enum Kind {
    User,
    Assistant,
    Tool,
    Reasoning,
    Info,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AssistantPhase {
    Pending,
    Progress,
    Answer,
    Discarded,
}

#[derive(Clone)]
struct Entry {
    kind: Kind,
    assistant_phase: AssistantPhase,
    title: String,
    body: String,
    details: String,
    call_id: Option<String>,
    failed: bool,
    started: Option<Instant>,
    duration: Option<Duration>,
}

#[derive(Clone, Debug)]
struct VisibleLink {
    x: u16,
    y: u16,
    end_x: u16,
    target: String,
}

/// Styled transcript, markdown link targets, per-call disclosure rows, prose
/// rows eligible for implicit links, and source rows whose whitespace is
/// preformatted content, decoration anchors, and limited-width reading rows.
type HistoryDocument = (
    Text<'static>,
    Vec<(String, String)>,
    Vec<(usize, String)>,
    Vec<usize>,
    Vec<usize>,
    Vec<(usize, Decor)>,
    Vec<usize>,
);

#[derive(Clone, Copy)]
enum Surface {
    Raised,
    Message,
    ToolRecord,
}

#[derive(Clone)]
enum Decor {
    Surface(Surface),
    Role {
        label: &'static str,
        color: Color,
    },
    CodeLabel(markdown::CodeBlock),
    Group(String),
    Inspect(String),
    TurnStart(usize),
    Rail {
        column: u16,
        color: Color,
        first_only: bool,
    },
    Gutter {
        column: u16,
        glyph: &'static str,
        color: Color,
        first_only: bool,
    },
}

struct HistoryCache {
    width: u16,
    visible: usize,
    tools_expanded: bool,
    reasoning_expanded: bool,
    has_activity_tail: bool,
    tool_timer_second: Option<u64>,
    theme: Theme,
    text: Text<'static>,
    source_lines: Vec<usize>,
    links: Vec<links::Region>,
    decor: Vec<(usize, Decor)>,
    expanded_tool_calls: HashSet<String>,
    expanded_tool_groups: HashSet<String>,
    // Wrapped transcript row -> actual tool call ID, for per-call disclosure.
    tool_rows: Vec<(usize, String)>,
}

impl Entry {
    fn user(body: impl Into<String>) -> Self {
        Self::new(Kind::User, "YOU", body)
    }
    fn new(kind: Kind, title: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            kind,
            assistant_phase: AssistantPhase::Answer,
            title: title.into(),
            body: body.into(),
            details: String::new(),
            call_id: None,
            failed: false,
            started: None,
            duration: None,
        }
    }
}

fn excerpt(text: &str, limit: usize) -> String {
    let mut chars = text.chars();
    let short = chars.by_ref().take(limit).collect::<String>();
    if chars.next().is_some() {
        format!("{short}…")
    } else {
        short
    }
}

/// Windows canonicalization returns a verbatim `\\?\` path. That prefix is
/// useful to the filesystem but meaningless and noisy in a status header.
fn strip_verbatim_path(value: &str) -> String {
    if let Some(rest) = value.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = value.strip_prefix(r"\\?\") {
        rest.to_owned()
    } else {
        value.to_owned()
    }
}

/// Find the failure's useful diagnostic, not its full tool-result envelope.
/// Stored and live tool results already use the same tool_result_details format.
fn failure_summary(details: &str) -> String {
    let lines = details
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    if let Some(subagent_fail) = lines
        .iter()
        .copied()
        .find(|line| line.starts_with("✗ [") || line.starts_with("△ "))
    {
        return subagent_fail.to_owned();
    }
    let exit = lines.iter().find_map(|line| line.strip_prefix("exit: "));
    let message = lines
        .iter()
        .find_map(|line| {
            line.strip_prefix("stderr: ")
                .filter(|s| !s.trim().is_empty())
        })
        .or_else(|| {
            lines.iter().copied().find(|line| {
                !line.starts_with("exit: ") && {
                    let lower = line.to_ascii_lowercase();
                    lower.contains("error")
                        || lower.contains("failed")
                        || lower.contains("exception")
                        || lower.contains("not found")
                        || lower.contains("denied")
                        || lower.contains("refused")
                        || lower.contains("cannot")
                        || lower.contains("错误")
                        || lower.contains("失败")
                        || lower.contains("不存在")
                }
            })
        })
        .unwrap_or("");
    let message = message.trim().trim_start_matches("stderr: ");
    match (exit, message.is_empty()) {
        (Some(code), true) => format!("exit {code}"),
        (Some(code), false) => format!("exit {code} · {message}"),
        (None, true) => "Tool failed".into(),
        (None, false) => message.to_owned(),
    }
}

#[derive(Clone, Copy)]
pub(super) enum TaskOutcome {
    Completed,
    Failed,
    Cancelled,
}

pub struct UiState {
    #[cfg(any(windows, test))]
    pub(super) native_paste_keys: super::paste::NativePasteKeys,
    plain_style: bool,
    /// Anchor for IME composition while the composer paints a software caret.
    /// The terminal moves here *without showing* its blinking hardware cursor.
    pub(super) ime_cursor_position: Option<(u16, u16)>,
    pub(super) taskbar_enabled: bool,
    pub(super) ui_sender: Option<EventSender>,
    navigation_turn: Option<usize>,
    pending_scroll_turn: Option<usize>,
    git_revision: u64,
    git_info: super::gitinfo::Info,
    turn_changes: Option<super::gitinfo::Changes>,
    recent_sessions: Vec<SessionEntry>,
    recent_session_rows: Vec<(Rect, String)>,
    notification_level: NoticeLevel,
    received_model_output: bool,
    last_output: Instant,
    pub preferences: super::preferences::Preferences,
    pub preferences_path: Option<std::path::PathBuf>,
    pub task_outcome: Option<TaskOutcome>,
    /// Wall-clock start of the current turn; the completion row reports
    /// how long the turn took instead of a generic acknowledgement.
    task_started: Option<Instant>,
    task_elapsed: Option<Duration>,
    unread_updates: usize,
    pub editor: Editor,
    pub picker: Option<Picker>,
    /// The one open question, if a running tool is waiting for an answer.
    pub(super) ask: Option<AskOverlay>,
    /// Where a waiting tool hands its question to this interface.
    pub(super) asker: AskSlot,
    plugin_panel: Option<PluginPanel>,
    pub plugin_commands: Vec<(String, String)>,
    pub plugin_statuses: Vec<String>,
    pub plugin_widgets: Vec<crate::plugins::PluginWidget>,
    pub(super) custom_tui_slot: plugin_panel::CustomTuiSlot,
    custom_ui: Option<plugin_panel::CustomTuiOverlay>,
    pub(super) widget_action_hits: Vec<(Rect, String, String, String)>,
    pub(super) last_terminal_size: (u16, u16),
    plugin_activity_frames: Vec<String>,
    plugin_tool_labels: BTreeMap<String, String>,
    plugin_layout: Option<PluginLayoutSpec>,
    plugin_colors: BTreeMap<String, Color>,
    pub usage: crate::usage::UsageMeter,
    pub title_usage: crate::usage::UsageMeter,
    pub apec_usage: crate::usage::UsageMeter,
    pub attachments: Vec<Attachment>,
    removed_drafts: Vec<Attachment>,
    cleared_draft: Option<draft::ClearedDraft>,
    saved_draft_attachments: Option<Vec<Attachment>>,
    /// Unsent drafts are kept per session (including the welcome page).
    session_drafts: HashMap<Option<String>, draft::SessionDraft>,
    pub attach_mode: bool,
    pub attach_path: String,
    next_marker: u32,
    pub themes: ThemeStore,
    pub session: String,
    session_title: Option<String>,
    /// Plain `kroma` is a welcome page, not an implicit resumed session.
    home: bool,
    pub model: String,
    link_workdir: PathBuf,
    pub model_profile: Option<String>,
    pub model_catalog: Option<crate::sessions::SessionCatalog>,
    history_visible: usize,
    history_max_scroll: usize,
    history_cache: Option<HistoryCache>,
    selection_rows: Vec<String>,
    selection_area: Rect,
    editor_area: Rect,
    editor_selection: Option<(usize, usize)>,
    editor_selecting: bool,
    selection_start: Option<(usize, usize)>,
    selection_end: Option<(usize, usize)>,
    history_top_row: usize,
    scrollbar_drag: Option<usize>,
    scrollbar_pointer_y: Option<u16>,
    scrollbar_area: Rect,
    return_latest_area: Rect,
    selection_dragging: bool,
    selection_edge: i8,
    visible_links: Vec<VisibleLink>,
    hover_pointer: Option<(u16, u16)>,
    entries: Vec<Entry>,
    /// Temporary command output belongs to one session, not the model WAL.
    info_session: String,
    pub running: bool,
    pub notification: String,
    /// Follow-up tasks queued from the composer, shown at a glance in the footer.
    pub(super) queued: Vec<String>,
    pub scroll_from_bottom: usize,
    /// Pinned absolute top row while inspecting older output. A stream may
    /// add rows below without moving the reader's selection or context.
    scroll_anchor: Option<usize>,
    prepend_rows_before: Option<usize>,
    pub viewport_height: u16,
    pub tools_expanded: bool,
    expanded_tool_calls: HashSet<String>,
    expanded_tool_groups: HashSet<String>,
    pub reasoning_expanded: bool,
    active_assistant: Option<usize>,
    active_reasoning: Option<usize>,
    context_input: Option<u64>,
    context_window: usize,
    context_estimated: bool,
    pub thinking: Option<String>,
    /// Levels advertised by the active profile; empty means every level.
    phase: &'static str,
    phase_since: Option<Instant>,
    activity_frame: usize,
    last_activity_frame: Instant,
}

/// Marching dots: a fixed-width marker that reads as motion. A single dot
/// that grows in place looked like a blinking caret.
const ACTIVITY_FRAMES: [&str; 4] = ["●··", "·●·", "··●", "·●·"];

enum PanelPlacement {
    Centered,
    Docked,
}

struct PluginPanel {
    placement: PanelPlacement,
    search: Option<transcript_panel::Search>,
    title: String,
    body: String,
    scroll: u16,
    wrapped_width: usize,
    wrapped: Text<'static>,
}

impl UiState {
    fn active_colors(&self) -> Theme {
        if self.plain_style {
            return self.plain_colors();
        }
        let mut colors = self.themes.colors;
        for (slot, color) in &self.plugin_colors {
            match slot.as_str() {
                "accent" => colors.accent = *color,
                "background" => colors.background = *color,
                "border" => colors.border = *color,
                "text" => colors.text = *color,
                "muted" => colors.muted = *color,
                "user" => colors.user = *color,
                "tool" => colors.tool = *color,
                "code" => colors.code = *color,
                _ => {}
            }
        }
        colors
    }

    pub fn set_link_workdir(&mut self, workdir: &Path) {
        self.link_workdir = workdir.to_path_buf();
        self.history_cache = None;
    }

    /// The handle a running tool uses to hand this interface a question.
    pub(super) fn asker(&self) -> std::sync::Arc<dyn crate::tools::Asker> {
        std::sync::Arc::new(self.asker.clone())
    }

    /// Pick up a question a tool is waiting on. Answers arrive as an ordinary
    /// tool result, so nothing about the question is persisted separately.
    pub(super) fn pull_ask(&mut self) -> bool {
        if self.ask.is_some() {
            return false;
        }
        let Some((request, answer)) = self.asker.take() else {
            return false;
        };
        self.ask = Some(AskOverlay::new(request, answer));
        true
    }

    /// Route one input event to the open question. Returns whether the question
    /// consumed it; Ctrl+C keeps its run-wide meaning and falls through.
    pub(super) fn ask_event(&mut self, event: &crossterm::event::Event) -> bool {
        use crossterm::event::{Event, KeyEventKind};
        let Some(overlay) = self.ask.as_mut() else {
            return false;
        };
        let outcome = match event {
            Event::Key(key) if key.kind != KeyEventKind::Release => overlay.key(*key),
            Event::Paste(text) => {
                overlay.paste(text);
                AskKey::Taken
            }
            _ => AskKey::Taken,
        };
        match outcome {
            AskKey::Taken => true,
            AskKey::Pass => false,
            AskKey::Closed(answer) => {
                if let Some(sender) = self.ask.as_mut().and_then(AskOverlay::take_answer) {
                    let _ = sender.send(answer);
                }
                self.ask = None;
                true
            }
        }
    }

    /// Drop an unresolved question: an ended turn must not leave an overlay
    /// that can never be answered.
    pub(super) fn clear_ask(&mut self) {
        self.ask = None;
    }

    /// Low-level legacy initializer retained for historical rendering regressions.
    /// The interactive host uses `new_default`, which selects A / Plain.
    pub(super) fn new(
        model: String,
        session: String,
        themes: ThemeStore,
        messages: &[Message],
    ) -> Self {
        let mut result = Self {
            #[cfg(any(windows, test))]
            native_paste_keys: Default::default(),
            plain_style: false,
            ime_cursor_position: None,
            taskbar_enabled: false,
            ui_sender: None,
            navigation_turn: None,
            pending_scroll_turn: None,
            git_revision: 0,
            git_info: super::gitinfo::Info::default(),
            turn_changes: None,
            recent_sessions: Vec::new(),
            recent_session_rows: Vec::new(),
            notification_level: NoticeLevel::Info,
            received_model_output: false,
            last_output: Instant::now(),
            preferences: super::preferences::Preferences::default(),
            preferences_path: None,
            task_outcome: None,
            task_started: None,
            task_elapsed: None,
            unread_updates: 0,
            editor: Editor::default(),
            picker: None,
            ask: None,
            asker: AskSlot::new(),
            plugin_panel: None,
            plugin_commands: Vec::new(),
            plugin_statuses: Vec::new(),
            plugin_widgets: Vec::new(),
            custom_tui_slot: plugin_panel::CustomTuiSlot::new(),
            custom_ui: None,
            widget_action_hits: Vec::new(),
            last_terminal_size: (80, 24),
            plugin_activity_frames: Vec::new(),
            plugin_tool_labels: BTreeMap::new(),
            plugin_layout: None,
            plugin_colors: BTreeMap::new(),
            usage: crate::usage::UsageMeter::default(),
            title_usage: crate::usage::UsageMeter::default(),
            apec_usage: crate::usage::UsageMeter::default(),
            attachments: Vec::new(),
            removed_drafts: Vec::new(),
            cleared_draft: None,
            saved_draft_attachments: None,
            session_drafts: HashMap::new(),
            attach_mode: false,
            attach_path: String::new(),
            next_marker: super::editor::ATTACHMENT_START,
            themes,
            model,
            link_workdir: std::env::current_dir().unwrap_or_default(),
            model_profile: None,
            model_catalog: None,
            history_visible: 300,
            history_max_scroll: 0,
            history_cache: None,
            selection_rows: Vec::new(),
            selection_area: Rect::default(),
            editor_area: Rect::default(),
            editor_selection: None,
            editor_selecting: false,
            selection_start: None,
            selection_end: None,
            history_top_row: 0,
            scrollbar_drag: None,
            scrollbar_pointer_y: None,
            scrollbar_area: Rect::default(),
            return_latest_area: Rect::default(),
            selection_dragging: false,
            selection_edge: 0,
            visible_links: Vec::new(),
            hover_pointer: None,
            info_session: session.clone(),
            session,
            session_title: None,
            home: false,
            entries: Vec::new(),
            running: false,
            notification: String::new(),
            queued: Vec::new(),
            scroll_from_bottom: 0,
            scroll_anchor: None,
            prepend_rows_before: None,
            viewport_height: 0,
            tools_expanded: false,
            expanded_tool_calls: HashSet::new(),
            expanded_tool_groups: HashSet::new(),
            reasoning_expanded: false,
            active_assistant: None,
            active_reasoning: None,
            context_input: None,
            context_window: 0,
            context_estimated: true,
            thinking: None,
            phase: "ready",
            phase_since: None,
            activity_frame: 0,
            last_activity_frame: Instant::now(),
        };
        result.load_history(messages);
        result.restore_input_history(messages);
        result
    }

    pub fn reading_position(&self) -> Option<(usize, usize)> {
        self.scroll_anchor.map(|row| (row, self.unread_updates))
    }

    pub fn restore_reading_position(&mut self, position: Option<(usize, usize)>) {
        if let Some((row, unread)) = position {
            self.scroll_anchor = Some(row);
            self.unread_updates = unread.saturating_add(1);
        }
    }

    pub fn apply_preferences(&mut self, preferences: super::preferences::Preferences) {
        self.preferences = preferences;
        self.history_cache = None;
        self.selection_start = None;
        self.selection_end = None;
        self.mouse_release();
    }

    pub fn open_command_completion(&mut self, query: String) {
        let mut picker = Picker::new(
            super::picker::Kind::Commands,
            super::command_catalog::items_with_plugins(
                self.preferences.language,
                &self.plugin_commands,
            ),
            query,
        );
        picker.completion = true;
        self.picker = Some(picker);
    }

    fn has_activity_tail(&self) -> bool {
        if self.home {
            return false;
        }
        if self.running
            || matches!(
                self.task_outcome,
                Some(TaskOutcome::Failed | TaskOutcome::Cancelled)
            )
        {
            return true;
        }
        let start = self
            .entries
            .iter()
            .rposition(|entry| matches!(entry.kind, Kind::User))
            .unwrap_or(0);
        self.task_outcome.is_some()
            && (self.entries[start..]
                .iter()
                .any(|entry| matches!(entry.kind, Kind::Tool))
                || !self.change_summary().is_empty())
    }

    fn task_status(&self) -> String {
        let language = self.preferences.language;
        if self.running {
            return self.activity_status();
        }
        // Report how long the turn took. A bare acknowledgement costs a row
        // without telling the reader anything the transcript does not.
        let elapsed = self
            .task_elapsed
            .map(|duration| format!(" · {:.1}s", duration.as_secs_f64()))
            .unwrap_or_default();
        match self.task_outcome {
            Some(TaskOutcome::Completed) => {
                format!(
                    "{}{elapsed}{}",
                    language.text("Execution finished", "本轮执行已结束"),
                    self.change_summary()
                )
            }
            Some(TaskOutcome::Failed) => format!(
                "{}{elapsed}",
                language.text("Task failed · see error above", "任务失败 · 请查看上方错误")
            ),
            Some(TaskOutcome::Cancelled) => format!(
                "{}{elapsed}",
                language.text(
                    "Cancelled · side effects are not undone",
                    "已取消 · 已产生的操作不会撤销"
                )
            ),
            None => language.text("Ready", "就绪").into(),
        }
    }

    fn navigation_status(&self) -> String {
        let language = self.preferences.language;
        let latest = language.text("Browsing history", "正在浏览历史");
        let latest = if let Some(index) = self.navigation_turn {
            let end = index.saturating_add(1).min(self.entries.len());
            let number = self.entries[..end]
                .iter()
                .filter(|entry| matches!(entry.kind, Kind::User))
                .count();
            let count = self
                .entries
                .iter()
                .filter(|entry| matches!(entry.kind, Kind::User))
                .count();
            format!(
                "{} {number}/{count} · {latest}",
                language.text("Turn", "第")
            )
        } else {
            latest.to_owned()
        };
        if self.unread_updates > 0 {
            format!(
                "{latest} · {} {}",
                self.unread_updates,
                language.text("new updates", "条新动态")
            )
        } else {
            latest
        }
    }

    /// Mouse hit testing matches the link shown while browsing history.
    pub(in crate::tui) fn return_latest_at(&self, column: u16, row: u16) -> bool {
        self.return_latest_area.width > 0 && self.return_latest_area.contains((column, row).into())
    }

    /// Record the turn's wall-clock duration.
    ///
    /// The completion row must not depend on a `TaskFinished` event: the final
    /// drain keeps only measurement events, so the terminal event can be
    /// dropped and the row would silently lose its duration.
    pub fn finish_task(&mut self) {
        self.running = false;
        self.queued.clear();
        self.task_elapsed = self.task_started.take().map(|start| start.elapsed());
    }

    pub fn add_user(&mut self, text: &str) {
        self.home = false;
        self.git_revision = self.git_revision.wrapping_add(1);
        self.turn_changes = None;
        self.received_model_output = false;
        self.last_output = Instant::now();
        self.task_outcome = None;
        self.unread_updates = 0;
        self.dismiss_notification();
        self.mouse_release();
        self.selection_start = None;
        self.selection_end = None;
        self.history_cache = None;
        let items = self
            .attachments
            .iter()
            .map(Attachment::item)
            .collect::<Vec<_>>();
        let summary = super::attachments::display_message(text, Some(&items));
        self.entries.push(Entry::user(summary));
        self.task_started = Some(Instant::now());
        self.task_elapsed = None;
        self.active_assistant = None;
        // A new user turn must not append thinking to the previous task's
        // reasoning entry, even if TaskFinished was consumed by the final
        // event drain and only finish_task() ran.
        self.active_reasoning = None;
        self.scroll_from_bottom = 0;
        self.scroll_anchor = None;
        self.running = true;
        self.set_phase("preparing");
    }

    fn show_editor_cursor(&self) -> bool {
        // The composer accepts steering while the model is connecting.
        // Always anchor the hardware cursor here: Windows IME uses that
        // position for its candidate/composition window before the first key.
        true
    }

    /// Brief feedback is shown until the user's next interaction, not
    /// permanently in place of the footer's normal help. Important errors
    /// are additionally recorded as scrollable transcript events.
    pub fn dismiss_notification(&mut self) {
        self.notification.clear();
    }

    pub fn refresh_session_title(&mut self) {
        self.recent_sessions = self
            .model_catalog
            .as_ref()
            .and_then(|catalog| catalog.list().ok())
            .unwrap_or_default()
            .into_iter()
            .take(3)
            .collect();
        self.session_title = self.model_catalog.as_ref().and_then(|catalog| {
            catalog
                .list()
                .ok()?
                .into_iter()
                .find(|entry| entry.id == self.session)
                .and_then(|entry| entry.name)
        });
    }

    pub(super) fn session_label(&self) -> &str {
        if self.home {
            return self.preferences.language.text("new conversation", "新会话");
        }
        self.session_title
            .as_deref()
            .unwrap_or(if self.session == "default" {
                "default"
            } else {
                self.preferences
                    .language
                    .text("untitled session", "未命名会话")
            })
    }

    /// Host command output is transient UI content, not part of model history.
    /// Put long lists in the scrollable transcript instead of the tiny footer.
    fn info_title<'a>(&self, title: &'a str) -> &'a str {
        let language = self.preferences.language;
        match title {
            "HELP" => language.text(title, "帮助"),
            "USAGE" => language.text(title, "用量"),
            "COMMAND" => language.text(title, "命令结果"),
            "QUEUE" => language.text(title, "任务队列"),
            "ERROR" => language.text(title, "错误"),
            "DIFF" => language.text("Diff", "差异"),
            "CANCELLED" => language.text(title, "已取消"),
            _ => title,
        }
    }

    pub fn info(&mut self, title: &str, content: impl Into<String>) {
        self.history_cache = None;
        self.info_session = self.session.clone();
        self.entries.push(Entry::new(Kind::Info, title, content));
        // Explicit command results should be visible immediately, even when
        // the user was browsing old messages. Streaming still preserves its
        // existing scroll anchor unless the user actively jumps to latest.
        self.scroll_to_bottom();
    }

    /// Clear session-local command output after an explicit history reset.
    pub fn clear_local_info(&mut self) {
        self.entries
            .retain(|entry| !matches!(entry.kind, Kind::Info));
        self.history_cache = None;
    }

    pub fn load_history(&mut self, messages: &[Message]) {
        self.turn_changes = None;
        self.navigation_turn = None;
        self.pending_scroll_turn = None;
        // Preserve ephemeral command output at its previous transcript
        // position without writing UI-only data into the Agent journal.
        // Session switching/welcome screens intentionally clear this list.
        let mut info_markers = Vec::new();
        if !self.home && self.info_session == self.session {
            let mut durable_before = 0usize;
            for entry in &self.entries {
                if matches!(entry.kind, Kind::Info) {
                    info_markers.push((durable_before, entry.clone()));
                } else {
                    durable_before += 1;
                }
            }
        }
        // Don't let a long series of local command results grow indefinitely.
        if info_markers.len() > 64 {
            info_markers.drain(..info_markers.len() - 64);
        }
        if self.info_session != self.session {
            self.expanded_tool_groups.clear();
        }
        self.info_session = self.session.clone();
        self.task_outcome = None;
        self.unread_updates = 0;
        self.home = false;
        self.history_cache = None;
        self.mouse_release();
        self.clear_editor_selection();
        self.selection_start = None;
        self.selection_end = None;
        let durations: HashMap<_, _> = self
            .entries
            .iter()
            .filter_map(|entry| Some((entry.call_id.clone()?, (entry.started, entry.duration))))
            .collect();
        self.entries.clear();
        self.active_assistant = None;
        self.active_reasoning = None;
        self.running = false;
        self.set_phase("ready");
        // The full conversation is available for paging, but keep tool
        // result matching linear instead of rescanning older UI entries.
        let mut tool_entries = HashMap::<String, usize>::new();
        for message in messages {
            match message.role.as_str() {
                "user" => {
                    let display = super::attachments::display_message(
                        message.content.as_deref().unwrap_or(""),
                        message.provider_items.as_deref(),
                    );
                    self.entries.push(Entry::user(display));
                }
                "assistant" => {
                    if let Some(reasoning) = message.reasoning_content.as_deref() {
                        if !reasoning.trim().is_empty() {
                            self.entries
                                .push(Entry::new(Kind::Reasoning, "THINKING", reasoning));
                        }
                    }
                    if let Some(text) = message.content.as_deref() {
                        if !text.trim().is_empty() {
                            let mut entry = Entry::new(Kind::Assistant, "KROMA", text);
                            if message
                                .tool_calls
                                .as_ref()
                                .is_some_and(|calls| !calls.is_empty())
                            {
                                entry.assistant_phase = AssistantPhase::Progress;
                            }
                            self.entries.push(entry);
                        }
                    }
                    for call in message.tool_calls.as_deref().unwrap_or(&[]) {
                        let mut entry = Entry::new(
                            Kind::Tool,
                            format!("{} · running", call.function.name),
                            tool_arguments(&call.function.name, &call.function.arguments),
                        );
                        entry.call_id = Some(call.id.clone());
                        if let Some((started, duration)) = durations.get(&call.id) {
                            entry.started = *started;
                            entry.duration = *duration;
                        }
                        tool_entries.insert(call.id.clone(), self.entries.len());
                        self.entries.push(entry);
                    }
                }
                "tool" => {
                    let found = message
                        .tool_call_id
                        .as_ref()
                        .and_then(|id| tool_entries.get(id))
                        .and_then(|index| self.entries.get_mut(*index));
                    let result = message.content.as_deref().unwrap_or("");
                    if let Some(entry) = found {
                        entry.details = tool_result_details(result);
                        if let Some(effective) = message
                            .provider_items
                            .as_deref()
                            .unwrap_or(&[])
                            .iter()
                            .find(|item| {
                                item.get("type").and_then(Value::as_str)
                                    == Some("kroma_tool_execution")
                            })
                        {
                            if let Some(arguments) =
                                effective.get("arguments").and_then(Value::as_str)
                            {
                                let name = entry
                                    .title
                                    .split_once(" · ")
                                    .map_or("shell", |(name, _)| name);
                                let actual = tool_arguments(name, arguments);
                                let previous = std::mem::replace(&mut entry.body, actual);
                                entry.details = format!("Plugin adjusted tool arguments (model requested: {previous})\n{}",entry.details);
                            }
                        }
                        entry.failed = message.tool_failed();
                        entry.title = entry.title.replace(
                            " · running",
                            if entry.failed {
                                " · error"
                            } else {
                                " · done"
                            },
                        );
                    } else {
                        let mut entry = Entry::new(Kind::Tool, "tool result", "");
                        entry.details = tool_result_details(result);
                        self.entries.push(entry);
                    }
                }
                _ => {}
            }
        }
        for (offset, (before, entry)) in info_markers.into_iter().enumerate() {
            self.entries
                .insert((before + offset).min(self.entries.len()), entry);
        }
        // Keep the disclosure of a completed tool open after the runtime
        // reconciles its durable WAL messages, but not across other sessions.
        self.expanded_tool_calls
            .retain(|id| tool_entries.contains_key(id));
        self.expanded_tool_groups
            .retain(|id| tool_entries.contains_key(id));
        self.scroll_from_bottom = 0;
        self.scroll_anchor = None;
        self.prepend_rows_before = None;
        self.history_visible = 300;
    }

    pub fn is_home(&self) -> bool {
        self.home
    }

    /// Keep the previous Agent session untouched until the first real send.
    /// Only the UI enters the transient welcome state.
    pub fn enter_home(&mut self) {
        self.task_outcome = None;
        self.unread_updates = 0;
        self.picker = None;
        self.home = true;
        self.usage = crate::usage::UsageMeter::default();
        self.title_usage = crate::usage::UsageMeter::default();
        self.apec_usage = crate::usage::UsageMeter::default();
        self.info_session.clear();
        self.entries.clear();
        self.navigation_turn = None;
        self.pending_scroll_turn = None;
        self.expanded_tool_calls.clear();
        self.expanded_tool_groups.clear();
        self.session_title = None;
        self.history_cache = None;
        self.active_assistant = None;
        self.active_reasoning = None;
        self.scroll_anchor = None;
        self.scroll_from_bottom = 0;
        self.mouse_release();
        self.clear_editor_selection();
        self.selection_start = None;
        self.selection_end = None;
        self.reset_context();
    }

    pub fn event(&mut self, event: UiEvent) {
        let late_usage = !self.running
            && matches!(
                &event,
                UiEvent::Usage { .. }
                    | UiEvent::ApecUsage { .. }
                    | UiEvent::UsageBatch(_)
                    | UiEvent::ApecUsageBatch(_)
            );
        if self.scroll_anchor.is_some()
            && matches!(
                &event,
                UiEvent::Text(_)
                    | UiEvent::ToolStarted { .. }
                    | UiEvent::ToolFinished { .. }
                    | UiEvent::ToolOutput { .. }
            )
        {
            self.unread_updates = self.unread_updates.saturating_add(1);
        }
        // Usage and token updates only change the footer. Collapsed ongoing
        // reasoning is not rendered, so no Markdown reflow is required for
        // each hidden reasoning delta or its final reconciliation.
        let unchanged_history = matches!(
            &event,
            UiEvent::Usage { .. }
                | UiEvent::UsageBatch(_)
                | UiEvent::ApecUsageBatch(_)
                | UiEvent::BacklogOverflow
                | UiEvent::TitleUsage { .. }
                | UiEvent::ApecUsage { .. }
                | UiEvent::Context { .. }
                | UiEvent::ActualInput(_)
                | UiEvent::SessionTitleUpdated(_)
        ) || (!self.reasoning_expanded
            && self.active_reasoning.is_some()
            && matches!(&event, UiEvent::ReasoningFinal(_)
                | UiEvent::ReasoningDelta(_) if self.phase == "thinking"));
        if !unchanged_history {
            self.history_cache = None;
        }
        match event {
            UiEvent::Warning(message) => self.note_with_level(NoticeLevel::Warning, message),
            UiEvent::GitInfo {
                revision,
                session,
                info,
                changes,
            } => {
                if revision == self.git_revision && session == self.session {
                    self.git_info = info;
                    self.turn_changes = changes;
                }
            }
            UiEvent::SessionTitleUpdated(id) => {
                if !self.home && self.session == id {
                    self.refresh_session_title();
                }
            }
            UiEvent::Usage {
                input,
                output,
                cached,
                reasoning,
            } => {
                self.usage.record(Some(&crate::model::Usage {
                    input,
                    output,
                    cached,
                    reasoning,
                    total: None,
                    web_searches: None,
                }));
            }
            UiEvent::TitleUsage {
                session_id,
                requests,
                input,
                output,
                cached,
                reasoning,
            } => {
                let usage = crate::model::Usage {
                    input,
                    output,
                    cached,
                    reasoning,
                    total: None,
                    web_searches: None,
                };
                if let Some(catalog) = &self.model_catalog {
                    if let Err(error) =
                        catalog.add_title_usage(&session_id, requests, usage.clone())
                    {
                        crate::runtime_log!("[usage] unable to persist title usage: {error:#}");
                    }
                }
                if session_id != self.session {
                    return;
                }
                self.title_usage.record_batch(
                    requests,
                    Some(&crate::model::Usage {
                        input,
                        output,
                        cached,
                        reasoning,
                        total: None,
                        web_searches: None,
                    }),
                );
            }
            UiEvent::ApecUsage {
                requests,
                input,
                output,
                cached,
            } => {
                self.apec_usage.record_batch(
                    requests,
                    Some(&crate::model::Usage {
                        input,
                        output,
                        cached,
                        reasoning: None,
                        total: None,
                        web_searches: None,
                    }),
                );
            }
            UiEvent::UsageBatch(batch) => self.usage.absorb(&batch),
            UiEvent::ApecUsageBatch(batch) => self.apec_usage.absorb(&batch),
            UiEvent::BacklogOverflow => {
                self.note_with_level(NoticeLevel::Warning, self.preferences.language.text(
                    "Live updates paused: UI backlog limit reached. The complete transcript reloads at turn end.",
                    "实时更新已暂停：界面队列达到上限。任务结束后将恢复完整记录。",
                ));
            }
            UiEvent::TaskStarted => {
                self.running = true;
                self.task_started.get_or_insert_with(Instant::now);
                self.task_elapsed = None;
                self.set_phase("preparing");
            }
            UiEvent::TaskFinished => {
                self.active_assistant = None;
                self.active_reasoning = None;
                self.finish_task();
                self.set_phase("ready");
            }
            UiEvent::Context { estimated } => {
                // Runtime input budgets reserve output space; they must not
                // replace the total window configured for the status display.
                self.context_input = Some(estimated as u64);
                self.context_estimated = true;
            }
            UiEvent::ActualInput(tokens) => {
                self.context_input = Some(tokens);
                self.context_estimated = false;
            }
            UiEvent::CompactionStarted => {
                self.set_phase("compacting");
            }
            UiEvent::ModelStarted => {
                if let Some(index) = self.active_assistant {
                    self.entries[index].assistant_phase = AssistantPhase::Progress;
                }
                self.active_assistant = None;
                self.active_reasoning = None;
                self.set_phase("waiting");
            }
            UiEvent::ModelFinished { tools } => {
                if let Some(index) = self.active_assistant {
                    if tools > 0 {
                        self.entries[index].assistant_phase = AssistantPhase::Progress;
                    }
                }
            }
            UiEvent::FinalAnswerReady => {
                if let Some(index) = self.active_assistant {
                    self.entries[index].assistant_phase = AssistantPhase::Answer;
                }
            }
            UiEvent::ModelFailed => {
                for index in [self.active_assistant.take(), self.active_reasoning.take()]
                    .into_iter()
                    .flatten()
                {
                    self.entries[index].assistant_phase = AssistantPhase::Discarded;
                }
            }
            UiEvent::Text(delta) => {
                self.received_model_output = true;
                self.last_output = Instant::now();
                self.set_phase("streaming");
                if let Some(index) = self.active_assistant {
                    self.entries[index].body.push_str(&delta);
                } else {
                    let mut entry = Entry::new(Kind::Assistant, "KROMA", delta);
                    entry.assistant_phase = AssistantPhase::Pending;
                    self.entries.push(entry);
                    self.active_assistant = Some(self.entries.len() - 1);
                }
            }
            UiEvent::ReasoningDelta(delta) => {
                self.received_model_output = true;
                self.last_output = Instant::now();
                if delta.is_empty() {
                    return;
                }
                self.set_phase("thinking");
                if let Some(index) = self.active_reasoning {
                    self.entries[index].body.push_str(&delta);
                } else {
                    self.insert_reasoning(delta);
                }
            }
            UiEvent::ReasoningFinal(reasoning) => {
                if !reasoning.is_empty() {
                    self.received_model_output = true;
                    self.last_output = Instant::now();
                }
                if let Some(index) = self.active_reasoning {
                    // Reconcile with the durable result; do not duplicate the
                    // live deltas when the model finishes.
                    self.entries[index].body = reasoning;
                    self.entries[index].duration = self.entries[index]
                        .started
                        .take()
                        .map(|start| start.elapsed());
                } else if !reasoning.is_empty() {
                    // Fallback for model backends that only provide reasoning
                    // in the completed assistant message.
                    self.insert_reasoning(reasoning);
                }
            }
            UiEvent::ToolStarted { name, id, args } => {
                if let Some(index) = self.active_assistant {
                    self.entries[index].assistant_phase = AssistantPhase::Progress;
                }
                self.received_model_output = true;
                self.last_output = Instant::now();
                self.set_phase("tool");
                let mut entry = Entry::new(Kind::Tool, format!("{name} · running"), args);
                entry.call_id = Some(id);
                entry.started = Some(Instant::now());
                self.entries.push(entry);
                self.active_assistant = None;
                self.active_reasoning = None;
            }
            UiEvent::ToolOutput { id, stream, chunk } => {
                self.last_output = Instant::now();
                if let Some(entry) = self
                    .entries
                    .iter_mut()
                    .rev()
                    .find(|entry| entry.call_id.as_deref() == Some(&id))
                {
                    if stream == "stderr" && !entry.details.ends_with('\n') {
                        entry.details.push('\n');
                    }
                    entry.details.push_str(&chunk);
                    if entry.details.len() > 16_384 {
                        let mut start = entry.details.len() - 12_288;
                        while !entry.details.is_char_boundary(start) {
                            start += 1;
                        }
                        entry.details.drain(..start);
                        entry
                            .details
                            .insert_str(0, "[... earlier live output omitted ...]\n");
                    }
                }
            }
            UiEvent::ToolFinished { id, result, failed } => {
                self.last_output = Instant::now();
                if let Some(entry) = self
                    .entries
                    .iter_mut()
                    .rev()
                    .find(|entry| entry.call_id.as_deref() == Some(&id))
                {
                    entry.title = entry
                        .title
                        .replace(" · running", if failed { " · error" } else { " · done" });
                    entry.details = result;
                    entry.failed = failed;
                    entry.duration = entry.started.take().map(|start| start.elapsed());
                }
            }
        }
        // Background APEC usage can arrive after the foreground task ends.
        if late_usage {
            if let Err(error) = self.save_usage() {
                crate::runtime_log!("[usage] background usage persistence failed: {error:#}");
            }
        }
    }

    fn insert_reasoning(&mut self, text: String) {
        let mut entry = Entry::new(Kind::Reasoning, "THINKING", text);
        entry.started = Some(Instant::now());
        if let Some(index) = self.active_assistant {
            self.entries.insert(index, entry);
            self.active_assistant = Some(index + 1);
            self.active_reasoning = Some(index);
        } else {
            self.entries.push(entry);
            self.active_reasoning = Some(self.entries.len() - 1);
        }
    }

    pub fn scroll(&mut self, amount: isize) {
        self.navigation_turn = None;
        self.scroll_from_bottom = if amount.is_positive() {
            self.scroll_from_bottom.saturating_add(amount as usize)
        } else {
            self.scroll_from_bottom
                .saturating_sub(amount.unsigned_abs())
        };
        // A short transcript has no scrollback. Do not enter history-browsing
        // mode merely because a PageUp was pressed at the bottom.
        self.scroll_from_bottom = self.scroll_from_bottom.min(self.history_max_scroll);
        if self.scroll_from_bottom == 0 || self.history_max_scroll == 0 {
            self.scroll_anchor = None;
        } else {
            self.scroll_anchor = Some(self.history_max_scroll - self.scroll_from_bottom);
        }
        if amount > 0
            && self.entries.len() > self.history_visible
            && self.scroll_from_bottom >= self.history_max_scroll.saturating_sub(35)
        {
            self.prepend_rows_before = self
                .history_cache
                .as_ref()
                .map(|cache| cache.text.lines.len());
            self.history_visible = (self.history_visible + 200).min(self.entries.len());
            self.history_cache = None;
        }
    }

    pub fn scroll_to_bottom(&mut self) {
        self.navigation_turn = None;
        self.pending_scroll_turn = None;
        self.unread_updates = 0;
        self.scroll_anchor = None;
        self.prepend_rows_before = None;
        self.scroll_from_bottom = 0;
        if self.history_visible > 300 {
            // The live tail is visible again, so the enlarged window is pure
            // per-frame cost. Shrink before the next streamed token arrives.
            self.history_visible = 300;
            self.history_cache = None;
        }
    }

    /// The small disclosure chevron expands exactly one tool call. Clicks
    /// elsewhere still select text, and Ctrl+O continues to expand all calls.
    /// Coordinates come from the same wrapped source rows as the renderer.
    pub fn invalidate_history_cache(&mut self) {
        self.history_cache = None;
    }

    pub fn toggle_all_tools(&mut self) {
        let tool_ids = self
            .entries
            .iter()
            .filter(|e| matches!(e.kind, Kind::Tool))
            .filter_map(|e| e.call_id.as_ref())
            .collect::<Vec<_>>();
        let all_manually_open = !tool_ids.is_empty()
            && (!self.expanded_tool_calls.is_empty() || !self.expanded_tool_groups.is_empty())
            && tool_ids
                .iter()
                .all(|id| self.expanded_tool_calls.contains(*id));
        self.tools_expanded = !(self.tools_expanded || all_manually_open);
        self.expanded_tool_calls.clear();
        self.expanded_tool_groups.clear();
        self.history_cache = None;
    }

    pub fn toggle_tool_at(&mut self, column: u16, row: u16) -> bool {
        if !self.selection_area.contains((column, row).into()) {
            return false;
        }
        let view_row = (row - self.selection_area.y) as usize;
        if self
            .selection_rows
            .get(view_row)
            .is_some_and(|line| line.starts_with("  ◇  "))
        {
            self.reasoning_expanded = !self.reasoning_expanded;
            self.history_cache = None;
            self.selection_start = None;
            self.selection_end = None;
            self.mouse_release();
            return true;
        }
        let wrapped_row = self.history_top_row + view_row;
        let Some(id) = self.history_cache.as_ref().and_then(|cache| {
            cache
                .tool_rows
                .iter()
                .find(|(line, _)| *line == wrapped_row)
                .map(|(_, id)| id.clone())
                .or_else(|| {
                    cache.decor.iter().find_map(|(line, item)| match item {
                        Decor::Group(id) if *line == wrapped_row => Some(id.clone()),
                        _ => None,
                    })
                })
        }) else {
            return false;
        };
        let is_group = self.history_cache.as_ref().is_some_and(|cache| {
            cache
                .decor
                .iter()
                .any(|(line, item)| *line == wrapped_row && matches!(item, Decor::Group(_)))
        });
        // Match the clickable gutter to the chevron drawn by each renderer.
        // The default classic TUI indents the tool card more than the legacy
        // renderer; both must keep mouse hit targets aligned with the glyph.
        let chevron_x = self
            .selection_area
            .x
            .saturating_add(if self.plain_style { 6 } else { 2 });
        if !is_group
            && !(chevron_x.saturating_sub(1)..=chevron_x.saturating_add(2)).contains(&column)
        {
            return false;
        }
        if self.tools_expanded {
            for entry in &self.entries {
                if matches!(entry.kind, Kind::Tool) {
                    if let Some(call_id) = &entry.call_id {
                        self.expanded_tool_calls.insert(call_id.clone());
                        self.expanded_tool_groups.insert(call_id.clone());
                    }
                }
            }
            self.tools_expanded = false;
        }
        if is_group {
            if !self.expanded_tool_groups.insert(id.clone()) {
                self.expanded_tool_groups.remove(&id);
                if let Some(idx) = self
                    .entries
                    .iter()
                    .position(|e| e.call_id.as_deref() == Some(id.as_str()))
                {
                    for entry in &self.entries[idx..] {
                        if !matches!(entry.kind, Kind::Tool | Kind::Reasoning) {
                            break;
                        }
                        if let Some(call_id) = &entry.call_id {
                            self.expanded_tool_calls.remove(call_id);
                        }
                    }
                }
            }
        } else if !self.expanded_tool_calls.insert(id.clone()) {
            self.expanded_tool_calls.remove(&id);
        }
        self.history_cache = None;
        self.selection_start = None;
        self.selection_end = None;
        self.mouse_release();
        true
    }

    fn scroll_to_row(&mut self, row: usize) {
        let top = row.min(self.history_max_scroll);
        self.scroll_from_bottom = self.history_max_scroll - top;
        self.scroll_anchor = (self.scroll_from_bottom > 0).then_some(top);
        if self.scroll_anchor.is_none() {
            self.unread_updates = 0;
        }
    }

    fn scrollbar_geometry(&self) -> (usize, usize) {
        let height = self.scrollbar_area.height as usize;
        let total = self.history_max_scroll + self.viewport_height as usize;
        if height == 0 || total == 0 {
            return (0, height);
        }
        let thumb = (height * height / total).clamp(1, height);
        let position = self
            .history_top_row
            .saturating_mul(height - thumb)
            .checked_div(self.history_max_scroll)
            .unwrap_or(0);
        (position, thumb)
    }

    pub fn scrollbar_press(&mut self, x: u16, y: u16) -> bool {
        if !self.scrollbar_area.contains((x, y).into()) || self.history_max_scroll == 0 {
            return false;
        }
        if self.history_visible < self.entries.len() {
            // Only pay for the full history if the user explicitly drags
            // the scrollbar; ordinary streaming still caches recent entries.
            self.prepend_rows_before = self
                .history_cache
                .as_ref()
                .map(|cache| cache.text.lines.len());
            self.history_visible = self.entries.len();
            self.history_cache = None;
        }
        let (position, thumb) = self.scrollbar_geometry();
        let row = (y - self.scrollbar_area.y) as usize;
        self.scrollbar_drag = Some(if row >= position && row < position + thumb {
            row - position
        } else {
            thumb / 2
        });
        self.scrollbar_pointer_y = Some(y);
        self.scrollbar_move(y);
        true
    }

    pub fn scrollbar_move(&mut self, y: u16) -> bool {
        let Some(offset) = self.scrollbar_drag else {
            return false;
        };
        self.scrollbar_pointer_y = Some(y);
        let height = self.scrollbar_area.height as usize;
        let (_, thumb) = self.scrollbar_geometry();
        if height <= thumb {
            self.scroll_to_bottom();
            return true;
        }
        let top = (y.saturating_sub(self.scrollbar_area.y) as usize)
            .saturating_sub(offset)
            .min(height - thumb);
        self.scroll_to_row(top * self.history_max_scroll / (height - thumb));
        true
    }

    pub fn mouse_release(&mut self) {
        if self.editor_selecting {
            if let Some((start, end)) = self.editor_selection {
                if start == end {
                    self.editor.set_caret(start);
                }
            }
        }
        self.editor_selecting = false;
        self.scrollbar_drag = None;
        self.scrollbar_pointer_y = None;
        // A click is not a selection. In particular, a click on empty
        // transcript space must not leave a reversed one-cell "caret".
        if self.selection_end.is_none()
            || self.selection_start == self.selection_end
            || self.selected_text().is_none()
        {
            self.selection_start = None;
            self.selection_end = None;
        }
        self.selection_dragging = false;
        self.selection_edge = 0;
    }

    pub fn selection_needs_auto_scroll(&self) -> bool {
        self.selection_dragging && self.selection_edge != 0
    }

    pub fn auto_scroll_selection(&mut self) -> bool {
        if !self.selection_dragging || self.selection_edge == 0 {
            return false;
        }
        let direction = self.selection_edge;
        let before = self.scroll_from_bottom;
        self.scroll(if direction < 0 { 2 } else { -2 });
        if before == self.scroll_from_bottom {
            return false;
        }
        let edge_row = if direction < 0 {
            self.selection_area.y
        } else {
            self.selection_area.bottom().saturating_sub(1)
        };
        let col = self.selection_end.map_or(0, |(_, x)| x);
        let top = self
            .history_max_scroll
            .saturating_sub(self.scroll_from_bottom);
        self.selection_end = Some((top + (edge_row - self.selection_area.y) as usize, col));
        true
    }

    pub fn link_at(&self, column: u16, row: u16) -> Option<&str> {
        self.visible_links
            .iter()
            .find(|link| link.y == row && column >= link.x && column < link.end_x)
            .map(|link| link.target.as_str())
    }

    /// A literal URL may span multiple terminal rows. The first row only
    /// contains its prefix, so resolve it against the unwrapped transcript
    /// when clicked. Do not guess if two URLs share that prefix.
    pub fn resolved_link(&self, target: &str) -> String {
        if !target.starts_with("https://") && !target.starts_with("http://") {
            return target.to_owned();
        }
        let mut found = None;
        for entry in &self.entries {
            for word in entry.body.split_whitespace() {
                let word = word.trim_start_matches(['`', '"', '\'', '<', '(']);
                let word = word.trim_end_matches(['`', '"', '\'', '>', ')', ',', '.', '，', '。']);
                if word.starts_with(target)
                    && word.len() > target.len()
                    && links::normalize_target(word).is_some()
                {
                    if found.as_deref().is_some_and(|previous| previous != word) {
                        return target.to_owned();
                    }
                    found = Some(word.to_owned());
                }
            }
        }
        found.unwrap_or_else(|| target.to_owned())
    }

    #[cfg(test)]
    pub(super) fn test_link_position(&self, needle: &str) -> Option<(u16, u16)> {
        self.visible_links
            .iter()
            .find(|link| link.target.contains(needle))
            .map(|link| (link.x, link.y))
    }

    pub fn hover_link(&mut self, column: u16, row: u16) {
        self.hover_pointer = Some((column, row));
    }

    pub fn start_editor_selection(&mut self, column: u16, row: u16) -> bool {
        if self.attach_mode || !self.editor_area.contains((column, row).into()) {
            return false;
        }
        let index = self.editor.position_at(
            self.editor_area.width,
            self.editor_area.height,
            column - self.editor_area.x,
            row - self.editor_area.y,
        );
        self.editor_selection = Some((index, index));
        self.editor_selecting = true;
        // Keep the viewport anchored during dragging; mouse_release will
        // move the caret for a click without a selection.
        self.selection_start = None;
        self.selection_end = None;
        self.selection_dragging = false;
        true
    }

    pub fn drag_editor_selection(&mut self, column: u16, row: u16) -> bool {
        if !self.editor_selecting || self.editor_area.width == 0 || self.editor_area.height == 0 {
            return false;
        }
        let x = column
            .saturating_sub(self.editor_area.x)
            .min(self.editor_area.width - 1);
        let y = row
            .saturating_sub(self.editor_area.y)
            .min(self.editor_area.height - 1);
        let index = self
            .editor
            .position_at(self.editor_area.width, self.editor_area.height, x, y);
        if let Some((start, _)) = self.editor_selection {
            self.editor_selection = Some((start, index));
        }
        true
    }

    pub fn editor_selected_text(&self) -> Option<String> {
        let (a, b) = self.editor_selection?;
        self.editor.selected_display_text(a, b)
    }

    pub fn selected_text_at(&self, column: u16, row: u16) -> Option<String> {
        if self.editor_area.contains((column, row).into()) {
            self.editor_selected_text()
        } else {
            self.selected_text()
        }
    }

    pub fn paste_text(&mut self, text: &str) {
        // The Windows clipboard commonly uses CRLF. Keep pasted text as
        // logical editor lines rather than embedding stray carriage returns.
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        self.editor.paste_over(&text, self.editor_selection.take());
        self.clear_editor_selection();
        self.prune_attachments();
    }

    pub fn editor_selection_key(&mut self, key: crossterm::event::KeyEvent) -> bool {
        use crossterm::event::{KeyCode, KeyModifiers};
        if key.modifiers.contains(KeyModifiers::SHIFT)
            && !key.modifiers.contains(KeyModifiers::ALT)
            && matches!(
                key.code,
                KeyCode::Left
                    | KeyCode::Right
                    | KeyCode::Up
                    | KeyCode::Down
                    | KeyCode::Home
                    | KeyCode::End
            )
        {
            let (anchor, head) = self
                .editor_selection
                .unwrap_or((self.editor.caret(), self.editor.caret()));
            self.editor.set_caret(head);
            self.editor.key(key);
            self.editor_selection = Some((anchor, self.editor.caret()));
            self.editor_selecting = false;
            self.selection_start = None;
            self.selection_end = None;
            self.selection_dragging = false;
            return true;
        }
        if let Some((a, b)) = self.editor_selection.filter(|(a, b)| a != b) {
            let replacement = match key.code {
                KeyCode::Char('j') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    Some("\n".into())
                }
                KeyCode::Char(ch)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    Some(ch.to_string())
                }
                KeyCode::Enter
                    if key.modifiers.intersects(
                        KeyModifiers::SHIFT | KeyModifiers::CONTROL | KeyModifiers::ALT,
                    ) =>
                {
                    Some("\n".into())
                }
                KeyCode::Tab if key.modifiers.is_empty() => Some("    ".into()),
                KeyCode::Backspace | KeyCode::Delete => Some(String::new()),
                _ => None,
            };
            if let Some(text) = replacement {
                self.paste_text(&text);
                return true;
            }
            if key.modifiers.is_empty() && matches!(key.code, KeyCode::Left | KeyCode::Right) {
                self.editor.set_caret(if key.code == KeyCode::Left {
                    a.min(b)
                } else {
                    a.max(b)
                });
                self.clear_editor_selection();
                return true;
            }
            self.editor.set_caret(b);
        }
        self.clear_editor_selection();
        false
    }

    pub fn clear_editor_selection(&mut self) {
        self.editor_selection = None;
        self.editor_selecting = false;
    }

    pub fn select_start(&mut self, column: u16, row: u16) {
        // Any new click clears the previous range. Only a subsequent drag
        // creates a nonempty selection; Down itself never highlights a cell.
        self.selection_start = None;
        self.selection_end = None;
        self.selection_dragging = false;
        self.selection_edge = 0;
        if self.selection_area.contains((column, row).into()) {
            self.clear_editor_selection();
            let point = (
                self.history_top_row + (row - self.selection_area.y) as usize,
                (column - self.selection_area.x) as usize,
            );
            self.selection_start = Some(point);
            self.selection_dragging = true;
        }
    }

    pub fn select_drag(&mut self, column: u16, row: u16) {
        if self.selection_dragging {
            self.selection_edge = if row <= self.selection_area.y {
                -1
            } else if row >= self.selection_area.bottom().saturating_sub(1) {
                1
            } else {
                0
            };
            let y = row.clamp(
                self.selection_area.y,
                self.selection_area.bottom().saturating_sub(1),
            );
            let x = column.clamp(
                self.selection_area.x,
                self.selection_area.right().saturating_sub(1),
            );
            self.selection_end = Some((
                self.history_top_row + (y - self.selection_area.y) as usize,
                (x - self.selection_area.x) as usize,
            ));
        }
    }

    pub fn selected_text(&self) -> Option<String> {
        let (start, end) = (self.selection_start?, self.selection_end?);
        if start == end {
            return None;
        }
        let (start, end) = if start <= end {
            (start, end)
        } else {
            (end, start)
        };
        let history = &self.history_cache.as_ref()?.text.lines;
        let mut copied = Vec::new();
        for row in start.0..=end.0.min(history.len().saturating_sub(1)) {
            let Some(line) = history.get(row) else {
                continue;
            };
            let line = line
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>();
            let first = if row == start.0 { start.1 } else { 0 };
            let last = if row == end.0 {
                end.1 + 1
            } else {
                self.selection_area.width as usize
            };
            let mut selected = String::new();
            let mut column = 0;
            for grapheme in line.graphemes(true) {
                let width = UnicodeWidthStr::width(grapheme).max(1);
                if column < last && column + width > first {
                    selected.push_str(grapheme);
                }
                column += width;
            }
            copied.push(selected.trim_end().to_owned());
        }
        let text = copied.join("\n");
        (!text.trim().is_empty()).then_some(text)
    }

    #[cfg(test)]
    fn history_text(&self, theme: Theme, width: usize) -> Text<'static> {
        self.history_document(theme, width).0
    }

    fn history_document(&self, theme: Theme, width: usize) -> HistoryDocument {
        if self.home {
            return (
                Text::default(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
            );
        }
        let mut lines = Vec::new();
        let mut markdown_targets = Vec::new();
        let mut tool_rows = Vec::new();
        let mut plain_link_rows = Vec::new();
        let mut preformatted_rows = Vec::new();
        let mut decor = Vec::new();
        let mut reading_rows = Vec::new();
        // Legacy shows one reasoning notice per turn. Plain defers thoughts
        // into their next real tool/reply card, where Ctrl+T restores each
        // segment without creating a separate reasoning-only card.
        let mut reasoning_notice_shown = false;
        if self.entries.is_empty() && !self.running {
            lines.push(Line::default());
            lines.push(Line::from(Span::styled(
                self.preferences
                    .language
                    .text("   ◆  Ready for a task.", "   ◆  可以开始任务了。"),
                Style::default().fg(theme.muted),
            )));
            return (
                Text::from(lines),
                markdown_targets,
                tool_rows,
                plain_link_rows,
                preformatted_rows,
                decor,
                reading_rows,
            );
        }
        let visible_start = self.entries.len().saturating_sub(self.history_visible);
        let visible = self.entries[visible_start..].iter().collect::<Vec<_>>();
        let mut hidden_until = 0;
        let mut previous_was_tool = false;
        let mut plain_block: Option<(usize, messages::MessageRole)> = None;
        let mut plain_assistant_has_text = false;
        // In Plain, thinking belongs to the *next* visible model or tool
        // phase. Until one exists, only the pinned activity line displays it.
        let mut pending_reasoning: Vec<&Entry> = Vec::new();
        for (index, entry) in visible.iter().enumerate() {
            if entry.assistant_phase == AssistantPhase::Discarded || index < hidden_until {
                continue;
            }
            if matches!(entry.kind, Kind::Reasoning) && entry.body.trim().is_empty() {
                continue;
            }
            // Never create a standalone KROMA card for thinking, even
            // when Ctrl+T is expanded. Attach it when real output arrives.
            if self.plain_style && matches!(entry.kind, Kind::Reasoning) {
                pending_reasoning.push(*entry);
                continue;
            }
            if self.plain_style
                && matches!(entry.kind, Kind::Assistant)
                && entry.body.trim().is_empty()
            {
                continue;
            }
            if self.plain_style {
                let role = match entry.kind {
                    Kind::Assistant => Some(messages::MessageRole::Assistant),
                    Kind::Tool => Some(messages::MessageRole::Tools),
                    Kind::Reasoning | Kind::User | Kind::Info => None,
                };
                // Real model text and tool batches get distinct cards.
                // Pending thinking is attached only after this boundary.
                if plain_block.is_some_and(|(_, current)| Some(current) != role)
                    || (plain_assistant_has_text && matches!(entry.kind, Kind::Assistant))
                {
                    self.close_plain_block(&mut lines, &mut decor, &mut plain_block);
                    plain_assistant_has_text = false;
                }
            }
            if !self.plain_style && previous_was_tool && !matches!(entry.kind, Kind::Tool) {
                lines.push(Line::default());
            }
            previous_was_tool = matches!(entry.kind, Kind::Tool);
            let role_style = |color| Style::default().fg(color).add_modifier(Modifier::BOLD);
            match entry.kind {
                Kind::User => {
                    reasoning_notice_shown = false;
                    pending_reasoning.clear();
                    let block_start = lines.len();
                    decor.push((block_start, Decor::TurnStart(visible_start + index)));
                    if self.plain_style {
                        lines.push(Line::default());
                        decor.push((
                            lines.len(),
                            Decor::Role {
                                label: self.preferences.language.text("YOU", "你"),
                                color: self.message_colors().user_rail,
                            },
                        ));
                        lines.push(Line::default());
                    }
                    let body_start = lines.len();
                    for line in entry.body.lines() {
                        lines.push(body_line(Line::from(Span::styled(
                            line.to_owned(),
                            Style::default().fg(theme.text),
                        ))));
                    }
                    reading_rows.extend(body_start..lines.len());
                    if !self.plain_style {
                        decor.push((
                            block_start,
                            Decor::Rail {
                                column: 1,
                                color: theme.user,
                                first_only: true,
                            },
                        ));
                    } else {
                        lines.push(Line::default());
                        self.decorate_message(
                            block_start,
                            lines.len(),
                            messages::MessageRole::User,
                            &mut decor,
                        );
                    }
                    lines.push(Line::default());
                }
                Kind::Assistant => {
                    if entry.body.starts_with(crate::agent::TASK_CANCELLED_PREFIX) {
                        // The live cancellation is represented only by its completion row.
                        if !matches!(self.task_outcome, Some(TaskOutcome::Cancelled)) {
                            // The notice belongs to the turn above it: drop the
                            // block separator so it does not read as the heading
                            // of the next turn.
                            if lines.last().is_some_and(|line| line.spans.is_empty()) {
                                lines.pop();
                            }
                            lines.push(body_line(Line::styled(
                                self.preferences.language.text(
                                    "⊘ Cancelled · incomplete commands may have had side effects",
                                    "⊘ 已取消 · 未完成的命令可能已产生副作用",
                                ),
                                Style::default().fg(theme.muted),
                            )));
                        }
                        continue;
                    }
                    if self.plain_style {
                        self.begin_plain_block(
                            &mut lines,
                            &mut decor,
                            &mut plain_block,
                            messages::MessageRole::Assistant,
                        );
                        let mut reasoning_in_card = false;
                        for thought in pending_reasoning.drain(..) {
                            self.append_reasoning(
                                thought,
                                theme,
                                width,
                                &mut lines,
                                &mut preformatted_rows,
                                &mut markdown_targets,
                                &mut reasoning_in_card,
                                &mut reading_rows,
                            );
                        }
                        if lines.last().is_some_and(|line| !line.spans.is_empty()) {
                            lines.push(Line::default());
                        }
                    } else {
                        decor.push((
                            lines.len(),
                            Decor::Gutter {
                                column: 1,
                                glyph: "◆",
                                color: theme.accent,
                                first_only: true,
                            },
                        ));
                    }
                    let body_start = lines.len();
                    if !entry.body.is_empty() {
                        let prose = width.saturating_sub(if self.plain_style { 4 } else { 5 });
                        // Distinct restrained inks: almost-white headings,
                        // mist-blue links and warm inline literals. Keep the
                        // existing prose/code/background surfaces unchanged.
                        let prose_colors = if self.plain_style {
                            let dark = self.themes.preset() == "dark";
                            markdown::ProseColors {
                                heading: if dark {
                                    Color::Rgb(255, 255, 255)
                                } else {
                                    theme.text
                                },
                                code: if dark && theme.code == theme.text {
                                    Color::Rgb(255, 208, 138)
                                } else {
                                    theme.code
                                },
                                link: self.semantic_colors().link,
                            }
                        } else {
                            markdown::ProseColors::from_theme(theme)
                        };
                        let mut document =
                            markdown::render_document_with(&entry.body, theme, prose, prose_colors);
                        if self.plain_style {
                            self.color_plain_diff_blocks(&mut document, theme);
                        }
                        for block in &document.blocks {
                            let mut block = block.clone();
                            block.row += lines.len();
                            block.end += lines.len();
                            decor.push((block.row, Decor::CodeLabel(block.clone())));
                            decor.extend(
                                (block.row..block.end)
                                    .map(|row| (row, Decor::Surface(Surface::Raised))),
                            );
                        }
                        let mut rendered = document.lines;
                        let tags = document.links;
                        let code_rows = document.preformatted;
                        plain_link_rows
                            .extend(document.prose.into_iter().map(|row| lines.len() + row));
                        for tag in tags {
                            if let Some(span) = rendered
                                .get(tag.row)
                                .and_then(|row| row.spans.get(tag.span))
                            {
                                markdown_targets.push((span.content.to_string(), tag.target));
                            }
                        }
                        let base = lines.len();
                        reading_rows.extend(
                            (0..rendered.len())
                                .filter(|row| code_rows.binary_search(row).is_err())
                                .map(|row| base + row),
                        );
                        decor.extend(
                            code_rows
                                .iter()
                                .map(|row| (base + row, Decor::Surface(Surface::Raised))),
                        );
                        preformatted_rows.extend(code_rows.into_iter().map(|row| base + row));
                        if self.running
                            && self.phase == "streaming"
                            && self.active_assistant == Some(self.entries.len() - 1)
                            && std::ptr::eq(*entry, self.entries.last().unwrap())
                        {
                            if let Some(last) = rendered.last_mut() {
                                last.spans
                                    .push(Span::styled("▍", Style::default().fg(theme.accent)));
                            }
                        }
                        for line in rendered {
                            // Only prose can acquire automatic links. Never
                            // linkify inline/fenced code or raw tool output.
                            lines.push(body_line(line));
                        }
                    }
                    if !self.plain_style
                        || lines.len() == body_start
                        || lines.last().is_some_and(|line| !line.spans.is_empty())
                    {
                        lines.push(Line::default());
                    }
                    if self.plain_style {
                        plain_assistant_has_text = true;
                    }
                }
                Kind::Tool => {
                    // A tool batch may contain interleaved thinking. It ends
                    // at its last tool: trailing thinking belongs to the next
                    // actual model response, not to this tool card.
                    let boundary = visible[index..]
                        .iter()
                        .position(|next| !matches!(next.kind, Kind::Tool | Kind::Reasoning))
                        .map_or(visible.len(), |count| index + count);
                    let end = if self.plain_style {
                        index
                            + visible[index..boundary]
                                .iter()
                                .rposition(|next| matches!(next.kind, Kind::Tool))
                                .expect("a tool batch always begins with a tool")
                            + 1
                    } else {
                        boundary
                    };
                    if self.plain_style {
                        self.begin_plain_block(
                            &mut lines,
                            &mut decor,
                            &mut plain_block,
                            messages::MessageRole::Tools,
                        );
                        let mut reasoning_in_card = false;
                        if self.reasoning_expanded {
                            // Preserve the chronological order of thoughts
                            // and tools while retaining a single outer card.
                            for thought in pending_reasoning.drain(..) {
                                self.append_reasoning(
                                    thought,
                                    theme,
                                    width,
                                    &mut lines,
                                    &mut preformatted_rows,
                                    &mut markdown_targets,
                                    &mut reasoning_in_card,
                                    &mut reading_rows,
                                );
                            }
                            let mut tools = Vec::new();
                            for next in &visible[index..end] {
                                if matches!(next.kind, Kind::Tool) {
                                    tools.push(*next);
                                } else if matches!(next.kind, Kind::Reasoning) {
                                    self.append_tool_group(
                                        &tools,
                                        theme,
                                        width,
                                        &mut lines,
                                        &mut tool_rows,
                                        &mut decor,
                                        &mut preformatted_rows,
                                    );
                                    tools.clear();
                                    self.append_reasoning(
                                        next,
                                        theme,
                                        width,
                                        &mut lines,
                                        &mut preformatted_rows,
                                        &mut markdown_targets,
                                        &mut reasoning_in_card,
                                        &mut reading_rows,
                                    );
                                }
                            }
                            self.append_tool_group(
                                &tools,
                                theme,
                                width,
                                &mut lines,
                                &mut tool_rows,
                                &mut decor,
                                &mut preformatted_rows,
                            );
                        } else {
                            // Only one collapsed notice per visible tool card.
                            // Keep all consecutive tools together regardless
                            // of how many hidden reasoning steps separate them.
                            for thought in pending_reasoning.drain(..).chain(
                                visible[index..end]
                                    .iter()
                                    .copied()
                                    .filter(|next| matches!(next.kind, Kind::Reasoning)),
                            ) {
                                self.append_reasoning(
                                    thought,
                                    theme,
                                    width,
                                    &mut lines,
                                    &mut preformatted_rows,
                                    &mut markdown_targets,
                                    &mut reasoning_in_card,
                                    &mut reading_rows,
                                );
                            }
                            let tools = visible[index..end]
                                .iter()
                                .copied()
                                .filter(|next| matches!(next.kind, Kind::Tool))
                                .collect::<Vec<_>>();
                            self.append_tool_group(
                                &tools,
                                theme,
                                width,
                                &mut lines,
                                &mut tool_rows,
                                &mut decor,
                                &mut preformatted_rows,
                            );
                        }
                    } else {
                        // Legacy transcript semantics remain unchanged.
                        for next in &visible[index..end] {
                            if matches!(next.kind, Kind::Reasoning) {
                                self.append_reasoning(
                                    next,
                                    theme,
                                    width,
                                    &mut lines,
                                    &mut preformatted_rows,
                                    &mut markdown_targets,
                                    &mut reasoning_notice_shown,
                                    &mut reading_rows,
                                );
                            }
                        }
                        let tools = visible[index..end]
                            .iter()
                            .copied()
                            .filter(|next| matches!(next.kind, Kind::Tool))
                            .collect::<Vec<_>>();
                        self.append_tool_group(
                            &tools,
                            theme,
                            width,
                            &mut lines,
                            &mut tool_rows,
                            &mut decor,
                            &mut preformatted_rows,
                        );
                    }
                    lines.push(Line::default());
                    hidden_until = end;
                    previous_was_tool = false;
                }
                Kind::Reasoning => {
                    // In Plain this entry was deferred above, until a real
                    // output exists. Only the legacy renderer gets here.
                    self.append_reasoning(
                        entry,
                        theme,
                        width,
                        &mut lines,
                        &mut preformatted_rows,
                        &mut markdown_targets,
                        &mut reasoning_notice_shown,
                        &mut reading_rows,
                    );
                }
                Kind::Info => {
                    if entry.title == "CANCELLED" {
                        continue;
                    }
                    let is_error = entry.title == "ERROR";
                    // One-line command/status results are a quiet status row,
                    // not a detached heading followed by a second row.
                    if !is_error && entry.title != "DIFF" && entry.body.lines().count() == 1 {
                        let content = entry.body.trim();
                        if !content.is_empty() {
                            let localized = if entry.title == "COMMAND" {
                                content.strip_prefix("Resumed session: ").map(|id| {
                                    let label = if chrome::is_generated_session_id(id) {
                                        self.session_title
                                            .as_deref()
                                            .map(str::to_owned)
                                            .unwrap_or_else(|| {
                                                display_session_title(id, self.preferences.language)
                                                    .to_owned()
                                            })
                                    } else {
                                        id.to_owned()
                                    };
                                    format!(
                                        "{}{label}",
                                        self.preferences
                                            .language
                                            .text("Resumed session: ", "已恢复会话：")
                                    )
                                })
                            } else {
                                None
                            };
                            lines.push(Line::from(vec![
                                Span::styled("  ·  ", Style::default().fg(theme.faint)),
                                Span::styled(
                                    localized.unwrap_or_else(|| content.to_owned()),
                                    Style::default().fg(theme.muted),
                                ),
                            ]));
                        }
                        lines.push(Line::default());
                        continue;
                    }
                    lines.push(Line::from(Span::styled(
                        format!(
                            "  {}  {}",
                            if is_error { "✕" } else { "·" },
                            self.info_title(&entry.title)
                        ),
                        role_style(if is_error && self.plain_style {
                            self.semantic_colors().error
                        } else if is_error {
                            theme.error
                        } else {
                            theme.muted
                        }),
                    )));
                    for line in entry.body.lines() {
                        let localized;
                        let line = if entry.title == "COMMAND" {
                            if let Some(id) = line.strip_prefix("Resumed session: ") {
                                let label = if chrome::is_generated_session_id(id) {
                                    self.session_title.as_deref().unwrap_or_else(|| {
                                        display_session_title(id, self.preferences.language)
                                    })
                                } else {
                                    id
                                };
                                if self.preferences.language == super::preferences::Language::Zh {
                                    localized = format!("已恢复会话：{label}");
                                } else {
                                    localized = format!("Resumed session: {label}");
                                }
                                &localized
                            } else {
                                line
                            }
                        } else {
                            line
                        };
                        let style = if entry.title == "DIFF" {
                            if line.starts_with('+') && !line.starts_with("+++") {
                                Style::default().fg(theme.success)
                            } else if line.starts_with('-') && !line.starts_with("---") {
                                Style::default().fg(theme.error)
                            } else if line.starts_with("@@") {
                                Style::default().fg(theme.accent)
                            } else {
                                Style::default().fg(theme.code)
                            }
                        } else if is_error
                            && (line.starts_with("error:") || line.starts_with("fatal:"))
                        {
                            Style::default().fg(theme.error)
                        } else {
                            Style::default().fg(if is_error { theme.text } else { theme.muted })
                        };
                        if self.plain_style && entry.title == "DIFF" {
                            preformatted_rows.push(lines.len());
                            lines.push(body_line(Line::from(self.plain_diff_spans(line, theme))));
                        } else if is_error {
                            lines.push(body_line(Line::from(vec![
                                Span::styled("│ ", Style::default().fg(theme.error)),
                                Span::styled(line.to_owned(), style),
                            ])));
                        } else {
                            lines.push(body_line(Line::from(Span::styled(line.to_owned(), style))));
                        }
                    }
                    lines.push(Line::default());
                }
            }
        }
        if self.plain_style {
            self.close_plain_block(&mut lines, &mut decor, &mut plain_block);
        }
        for (row, line) in lines.iter().enumerate() {
            if let Some(span) = line.spans.get(1) {
                if (preformatted_rows.binary_search(&row).is_ok() && span.content == "│ ")
                    || span.content == "│  "
                {
                    decor.push((
                        row,
                        Decor::Gutter {
                            column: 4,
                            glyph: "│",
                            color: if span.content == "│  " {
                                theme.faint
                            } else {
                                span.style.fg.unwrap_or(theme.border)
                            },
                            first_only: true,
                        },
                    ));
                }
            }
        }
        // Reserve one stable trailing transcript row for the live model phase
        // (or the final turn duration). Only the status glyphs animate: cached
        // markdown and selection/source-line identities never reflow per tick.

        (
            Text::from(lines),
            markdown_targets,
            tool_rows,
            plain_link_rows,
            preformatted_rows,
            decor,
            reading_rows,
        )
    }

    pub fn draw(&mut self, frame: &mut Frame) {
        // A prior frame's IME anchor must not survive an overlay or resize.
        self.ime_cursor_position = None;
        self.recent_session_rows.clear();
        self.widget_action_hits.clear();
        self.return_latest_area = Rect::default();
        self.editor
            .localize_attachment_labels(self.preferences.language);
        let colors = self.active_colors();
        let message_colors = self.message_colors();
        let full_area = frame.area();
        self.last_terminal_size = (full_area.width, full_area.height);
        let previous_history_area = self.selection_area;
        let previous_editor_area = self.editor_area;
        if full_area.width < 20 || full_area.height < 8 {
            self.editor_area = Rect::default();
            // Some SSH/PTY hosts initially report 0x0 before the first
            // terminal resize event. Do not draw cells outside that buffer.
            self.viewport_height = 0;
            self.selection_area = Rect::default();
            self.scrollbar_area = Rect::default();
            self.visible_links.clear();
            self.mouse_release();
            self.clear_editor_selection();
            self.selection_start = None;
            self.selection_end = None;
            self.selection_rows.clear();
            if full_area.width > 0 && full_area.height > 0 {
                frame.render_widget(
                    Paragraph::new(
                        self.preferences
                            .language
                            .text("Resize terminal for Kroma", "请调整终端大小"),
                    ),
                    full_area,
                );
            }
            return;
        }
        frame.render_widget(
            Block::default().style(Style::default().bg(colors.background)),
            full_area,
        );
        let outer_area = reading_area(full_area, self.plain_style);
        let (area, sidebar_area) = if outer_area.width >= 96
            && self.has_widgets(crate::plugins::WidgetPlacement::Sidebar)
        {
            let cols =
                Layout::horizontal([Constraint::Min(62), Constraint::Length(32)]).split(outer_area);
            (cols[0], Some(cols[1]))
        } else {
            (outer_area, None)
        };
        let header_rows = if self.plain_style {
            u16::from(area.height >= 20)
        } else if area.height < 20 {
            1
        } else {
            2
        };
        // The model's activity follows the conversation itself instead of
        // living in a fixed bar away from its latest output. Only history
        // navigation needs a separate row above the editable composer.
        let show_navigation =
            (self.scroll_anchor.is_some() || self.navigation_turn.is_some()) && area.height >= 10;
        let editor_inset = if self.plain_style { 2 } else { 1 };
        let composer_padding = editor_inset + 1;
        let composer_min = composer_padding + 1;
        let composer_max = self
            .plugin_layout
            .as_ref()
            .and_then(|layout| layout.composer_max_rows)
            .unwrap_or(if self.plain_style { 11 } else { 10 })
            .max(composer_min)
            .min(
                area.height
                    .saturating_sub(
                        header_rows + 3 + u16::from(self.plain_style && show_navigation),
                    )
                    .max(composer_min),
            );
        // A waiting question replaces the input text, so the box is sized by
        // the question rather than by the (hidden) draft.
        let editor_rows = match &self.ask {
            Some(ask) => ask.height(area.width.saturating_sub(6)),
            None => self.editor.display_rows(area.width.saturating_sub(6)),
        };
        let composer_height = editor_rows
            .saturating_add(composer_padding)
            .clamp(composer_min, composer_max);
        let status_rows = u16::from(self.has_activity_tail() && area.height >= 12);
        let header_w_rows = if area.height >= 18 {
            self.widget_slot_height(crate::plugins::WidgetPlacement::Header, area.width)
                .min(area.height / 5)
        } else {
            0
        };
        let above_w_rows = if area.height >= 18 {
            self.widget_slot_height(crate::plugins::WidgetPlacement::AboveComposer, area.width)
                .min(area.height / 4)
        } else {
            0
        };
        let below_w_rows = if area.height >= 20 {
            self.widget_slot_height(crate::plugins::WidgetPlacement::BelowComposer, area.width)
                .min(area.height / 5)
        } else {
            0
        };
        let footer_w_rows = if area.height >= 20 {
            self.widget_slot_height(crate::plugins::WidgetPlacement::Footer, area.width)
                .min(3)
        } else {
            0
        };
        let parts = Layout::vertical([
            // A blank row separates the session heading from the first turn.
            Constraint::Length(header_rows),
            Constraint::Length(header_w_rows),
            Constraint::Min(1),
            Constraint::Length(u16::from(show_navigation)),
            // Pinned status row: the live phase, duration and queued tasks stay
            // visible while the transcript scrolls. Small terminals keep the
            // row out of the layout so the transcript does not collapse.
            Constraint::Length(status_rows),
            Constraint::Length(above_w_rows),
            Constraint::Length(composer_height),
            Constraint::Length(below_w_rows),
            Constraint::Length(footer_w_rows),
            Constraint::Length(1),
        ])
        .split(area);
        let header_slot = parts[0];
        let header_widget_slot = parts[1];
        let transcript_row = parts[2];
        let nav_row = parts[3];
        let status_row = parts[4];
        let above_widget_slot = parts[5];
        let composer = parts[6];
        let below_widget_slot = parts[7];
        let footer_widget_slot = parts[8];
        let footer_row = parts[9];
        let header_line = Rect::new(header_slot.x, header_slot.y, header_slot.width, 1);
        if !self.plain_style {
            self.draw_header(frame, header_line, colors);
        }
        self.draw_widget_slot(
            frame,
            header_widget_slot,
            crate::plugins::WidgetPlacement::Header,
            colors,
        );
        let history_slot = Rect::new(
            transcript_row.x,
            transcript_row.y,
            transcript_row.width.saturating_sub(1),
            transcript_row.height,
        );
        // The surface and its text stop before the right gutter, leaving a
        // terminal-background strip next to the scrollbar.
        let content_width = history_slot.width.saturating_sub(if self.plain_style {
            PLAIN_RIGHT_GUTTER
        } else {
            0
        });
        if previous_history_area.width > 0 && previous_history_area.width != history_slot.width {
            // Selection offsets refer to *wrapped display cells*, not raw
            // grapheme indices. Reflow invalidates them. Clear the selection
            // rather than copy unrelated text after a terminal resize.
            self.selection_start = None;
            self.selection_end = None;
            self.selection_rows.clear();
            self.mouse_release();
        }
        self.viewport_height = history_slot.height;
        let has_activity_tail = self.has_activity_tail();
        let tool_timer_second = if self.plain_style {
            self.entries
                .iter()
                .filter(|entry| {
                    matches!(entry.kind, Kind::Tool) && entry.title.ends_with(" · running")
                })
                .filter_map(|entry| entry.started)
                .min()
                .map(|start| start.elapsed().as_secs())
        } else {
            None
        };
        let cached = self.history_cache.as_ref().is_some_and(|cache| {
            cache.width == history_slot.width
                && cache.visible == self.history_visible
                && cache.tools_expanded == self.tools_expanded
                && cache.expanded_tool_calls == self.expanded_tool_calls
                && cache.expanded_tool_groups == self.expanded_tool_groups
                && cache.reasoning_expanded == self.reasoning_expanded
                && cache.has_activity_tail == has_activity_tail
                && cache.tool_timer_second == tool_timer_second
                && cache.theme == colors
        });
        let reflow_anchor = self
            .history_cache
            .as_ref()
            .filter(|cache| {
                cache.width != history_slot.width
                    && self.scroll_anchor.is_some()
                    && self.navigation_turn.is_none()
            })
            .and_then(|cache| {
                let source = *cache.source_lines.get(self.history_top_row)?;
                let first = cache.source_lines.partition_point(|&line| line < source);
                let last = cache.source_lines.partition_point(|&line| line <= source);
                Some((
                    source,
                    self.history_top_row.saturating_sub(first),
                    last - first,
                ))
            });
        if !cached {
            if self.navigation_turn.is_some() {
                self.pending_scroll_turn = self.navigation_turn;
            }
            let (
                raw,
                targets,
                raw_tool_rows,
                plain_rows,
                preformatted_rows,
                raw_decor,
                reading_rows,
            ) = self.history_document(colors, usize::from(content_width));
            let (mut text, source_lines) = wrap_document_with_reading_limit(
                raw,
                usize::from(content_width),
                &preformatted_rows,
                if self.plain_style { &[] } else { &reading_rows },
                if self.plain_style { usize::MAX } else { 100 },
            );
            let mut link_regions = markdown_link_regions(&text, &targets);
            let mut valid_paths = HashMap::<String, bool>::new();
            let mut is_available = |target: &str| {
                *valid_paths
                    .entry(target.to_owned())
                    .or_insert_with(|| links::display_target_available(target, &self.link_workdir))
            };
            link_regions.retain(|link| is_available(&link.target));
            // Only rendered prose can create implicit links. Look at those
            // source rows (not the entire transcript) and skip rows without
            // URL/path separators. This work runs on history reflow, not on
            // each animation tick; repeated file paths are resolved once.
            for source_row in plain_rows {
                let start = source_lines.partition_point(|&source| source < source_row);
                let end = source_lines.partition_point(|&source| source <= source_row);
                for row in start..end {
                    let line = &text.lines[row];
                    if !line.spans.iter().any(|span| {
                        let chunk = span.content.as_ref();
                        chunk.contains('/') || chunk.contains('\\') || chunk.contains("www.")
                    }) {
                        continue;
                    }
                    let visible = line
                        .spans
                        .iter()
                        .map(|span| span.content.as_ref())
                        .collect::<String>();
                    for link in links::detect_visible(row, &visible) {
                        if !is_available(&link.target) {
                            continue;
                        }
                        if link_regions.iter().any(|existing| {
                            existing.row == row
                                && link.start < existing.end
                                && link.end > existing.start
                        }) {
                            continue;
                        }
                        link_regions.push(link);
                    }
                }
            }
            let tool_rows = raw_tool_rows
                .into_iter()
                .filter_map(|(line, id)| {
                    let first = source_lines.partition_point(|&source| source < line);
                    (source_lines.get(first) == Some(&line)).then_some((first, id))
                })
                .collect();
            let mut decor = Vec::new();
            for (source, item) in raw_decor {
                let start = source_lines.partition_point(|&row| row < source);
                let end = source_lines.partition_point(|&row| row <= source);
                for row in start..end {
                    if !matches!(
                        &item,
                        Decor::Gutter {
                            first_only: true,
                            ..
                        } | Decor::Rail {
                            first_only: true,
                            ..
                        } | Decor::Inspect(_)
                    ) || row == start
                    {
                        decor.push((row, item.clone()));
                    }
                }
            }
            for (row, item) in &decor {
                if matches!(item, Decor::Gutter { column: 4, .. }) {
                    if let Some(span) = text.lines[*row]
                        .spans
                        .iter_mut()
                        .find(|span| span.content == "│ " || span.content == "│  ")
                    {
                        span.content = " "
                            .repeat(UnicodeWidthStr::width(span.content.as_ref()))
                            .into();
                    }
                }
            }
            self.history_cache = Some(HistoryCache {
                width: history_slot.width,
                visible: self.history_visible,
                tools_expanded: self.tools_expanded,
                reasoning_expanded: self.reasoning_expanded,
                has_activity_tail,
                tool_timer_second,
                theme: colors,
                text,
                source_lines,
                links: link_regions,
                tool_rows,
                decor,
                expanded_tool_calls: self.expanded_tool_calls.clone(),
                expanded_tool_groups: self.expanded_tool_groups.clone(),
            });
        }
        if let Some(target) = self.pending_scroll_turn.take() {
            let cache = self.history_cache.as_ref().unwrap();
            if let Some(row) = cache.decor.iter().find_map(|(row, item)| {
                matches!(item,Decor::TurnStart(index) if *index==target).then_some(*row)
            }) {
                self.history_max_scroll = cache
                    .text
                    .lines
                    .len()
                    .saturating_sub(history_slot.height as usize);
                self.scroll_to_row(row);
            }
        }
        let contents = &self.history_cache.as_ref().unwrap().text;
        let rows = contents.lines.len();
        let max_scroll = rows.saturating_sub(history_slot.height as usize);
        self.history_max_scroll = max_scroll;
        // Read naturally from the header down. The previous bottom-anchored
        // transcript put a huge blank area above the first user request on
        // large terminals; keep conversation geometry stable at any length.
        let history = history_slot;
        if let Some((source, offset, old_count)) = reflow_anchor {
            // Preserve the same *logical transcript line* when a resize
            // changes wrapping. An absolute visual row is not meaningful
            // across terminal widths. Keep roughly the same position within
            // an exceptionally long source line as well.
            let source_lines = &self.history_cache.as_ref().unwrap().source_lines;
            let start = source_lines.partition_point(|&line| line < source);
            let end = source_lines.partition_point(|&line| line <= source);
            let rebased = start
                .saturating_add(offset.saturating_mul(end.saturating_sub(start)) / old_count.max(1))
                .min(max_scroll);
            self.scroll_anchor = (rebased > 0).then_some(rebased);
            self.scroll_from_bottom = max_scroll.saturating_sub(rebased);
        }
        if let (Some(y), Some(offset)) = (self.scrollbar_pointer_y, self.scrollbar_drag) {
            let height = self.scrollbar_area.height as usize;
            let thumb = height
                .saturating_mul(height)
                .checked_div(rows)
                .map(|ratio| ratio.clamp(1, height.max(1)))
                .unwrap_or(height);
            let track = height.saturating_sub(thumb);
            let from_track_start = (y.saturating_sub(self.scrollbar_area.y) as usize)
                .saturating_sub(offset)
                .min(track);
            let top = from_track_start
                .saturating_mul(max_scroll)
                .checked_div(track)
                .unwrap_or(0);
            self.scroll_from_bottom = max_scroll.saturating_sub(top);
            self.scroll_anchor = (self.scroll_from_bottom > 0).then_some(top);
            if self.scroll_anchor.is_none() {
                self.unread_updates = 0;
            }
            // If the lazy history was expanded, existing selection rows are
            // now lower in the cached transcript.
            if let Some(previous) = self.prepend_rows_before.take() {
                let added = rows.saturating_sub(previous);
                if let Some((row, _)) = self.selection_start.as_mut() {
                    *row = row.saturating_add(added);
                }
                if let Some((row, _)) = self.selection_end.as_mut() {
                    *row = row.saturating_add(added);
                }
            }
        } else if let Some(anchor) = self.scroll_anchor {
            // The history window loads older entries by prepending rows.
            // Adjust for those once; ordinary live text only appends below.
            let prepend = self
                .prepend_rows_before
                .take()
                .map_or(0, |old_rows| rows.saturating_sub(old_rows));
            if prepend > 0 {
                if let Some((row, _)) = self.selection_start.as_mut() {
                    *row = row.saturating_add(prepend);
                }
                if let Some((row, _)) = self.selection_end.as_mut() {
                    *row = row.saturating_add(prepend);
                }
            }
            let anchored = anchor.saturating_add(prepend).min(max_scroll);
            self.scroll_anchor = (max_scroll > 0).then_some(anchored);
            self.scroll_from_bottom = max_scroll.saturating_sub(anchored);
        } else {
            self.prepend_rows_before = None;
            self.scroll_from_bottom = self.scroll_from_bottom.min(max_scroll);
        }
        let start_row = max_scroll.saturating_sub(self.scroll_from_bottom);
        self.history_top_row = start_row;
        self.selection_area = history;
        let end_row = start_row.saturating_add(history.height as usize).min(rows);
        // The track ends on the last row that carries a message surface, so the
        // bar lines up with the transcript block instead of the empty rows
        // below it.
        let bar_height = self
            .history_cache
            .as_ref()
            .and_then(|cache| {
                cache
                    .decor
                    .iter()
                    .filter(|(row, item)| {
                        matches!(item, Decor::Surface(Surface::Message) | Decor::Rail { .. })
                            && *row >= start_row
                            && *row < end_row
                    })
                    .map(|(row, _)| (row - start_row + 1) as u16)
                    .max()
            })
            .unwrap_or(history_slot.height)
            .clamp(1, history_slot.height);
        self.scrollbar_area = Rect::new(history_slot.right(), history_slot.y, 1, bar_height);
        // Borrow cached styled lines instead of cloning every span on every
        // animation/scroll frame. Selection still owns its plain-text rows.
        let viewport_lines = &contents.lines[start_row..end_row];
        self.selection_rows = viewport_lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect();

        // Click coordinates come from the same wrapped rows used for drawing,
        // not from source offsets. This remains correct after scrolling,
        // terminal resizing and Unicode double-width characters.
        let regions = self
            .history_cache
            .as_ref()
            .unwrap()
            .links
            .iter()
            .filter(|link| link.row >= start_row && link.row < end_row)
            .map(|link| links::Region {
                row: link.row - start_row,
                start: link.start,
                end: link.end,
                target: link.target.clone(),
            })
            .collect::<Vec<_>>();
        self.visible_links = regions
            .into_iter()
            .filter_map(|link| {
                let start = link.start.min(history.width as usize);
                let end = link.end.min(history.width as usize);
                (end > start).then_some(VisibleLink {
                    x: history.x + start as u16,
                    y: history.y + link.row as u16,
                    end_x: history.x + end as u16,
                    target: link.target,
                })
            })
            .collect();

        // Render already-wrapped cached lines directly into the buffer.
        // Ratatui's stable set_line API keeps per-span styles and Unicode
        // clipping without allocating/cloning a Text for each frame.
        let buffer = frame.buffer_mut();
        buffer.set_style(
            history,
            Style::default().fg(colors.text).bg(colors.background),
        );
        let decorations = &self.history_cache.as_ref().unwrap().decor;
        // Message surfaces first, then nested surfaces. A turn's rail is added
        // after its tool records, so paint order must not let it cover them.
        for (row, item) in decorations
            .iter()
            .filter(|(row, _)| (start_row..end_row).contains(row))
        {
            if matches!(item, Decor::Surface(Surface::Message)) {
                let y = history.y + (row - start_row) as u16;
                buffer.set_style(
                    self.message_surface_area(history, y),
                    Style::default().bg(message_colors.surface),
                );
            }
        }
        for (row, item) in decorations
            .iter()
            .filter(|(row, _)| (start_row..end_row).contains(row))
        {
            let y = history.y + (row - start_row) as u16;
            match item {
                Decor::Surface(Surface::Raised) => {
                    let inset = if self.plain_style {
                        4.min(history.width)
                    } else {
                        0
                    };
                    let gutter = if self.plain_style {
                        PLAIN_RIGHT_GUTTER
                    } else {
                        0
                    };
                    // Keep breathing room between a code block's panel and the
                    // transcript edge; the label needs a margin of its own.
                    let pad = if self.plain_style { 2 } else { 0 };
                    buffer.set_style(
                        Rect::new(
                            history.x + inset,
                            y,
                            history.width.saturating_sub(inset + gutter + pad),
                            1,
                        ),
                        Style::default().bg(colors.input_surface()),
                    );
                }
                Decor::Surface(Surface::ToolRecord) => {
                    // Classic B: inset the gray tool panel within the
                    // assistant rail, leaving room for its chevron and text.
                    let inset = if self.plain_style { 4 } else { 2 }.min(history.width);
                    let gutter = if self.plain_style {
                        PLAIN_RIGHT_GUTTER
                    } else {
                        0
                    };
                    // Same right padding as a code block so the inspect label
                    // never sits on the surface edge.
                    let pad = if self.plain_style { 2 } else { 0 };
                    buffer.set_style(
                        Rect::new(
                            history.x + inset,
                            y,
                            history.width.saturating_sub(inset + gutter + pad),
                            1,
                        ),
                        Style::default().bg(if self.plain_style {
                            message_colors.surface
                        } else {
                            colors.surface
                        }),
                    );
                }
                _ => {}
            }
        }
        for (row, line) in viewport_lines.iter().enumerate() {
            buffer.set_line(history.x, history.y + row as u16, line, history.width);
        }
        for (row, item) in decorations
            .iter()
            .filter(|(row, _)| (start_row..end_row).contains(row))
        {
            // A space with a coloured background stays blank in native terminal copies.
            if let Decor::Rail { column, color, .. } = item {
                if history.width > *column {
                    if let Some(cell) =
                        buffer.cell_mut((history.x + column, history.y + (row - start_row) as u16))
                    {
                        cell.set_symbol(" ").set_bg(*color);
                    }
                }
            }
            if let Decor::Gutter {
                column,
                glyph,
                color,
                ..
            } = item
            {
                if history.width > *column {
                    if let Some(cell) =
                        buffer.cell_mut((history.x + column, history.y + (row - start_row) as u16))
                    {
                        cell.set_symbol(glyph).set_fg(*color);
                    }
                }
            }
            if let Decor::Role { label, color } = item {
                buffer.set_string(
                    history.x.saturating_add(4),
                    history.y + (row - start_row) as u16,
                    ellipsize_cells(label, usize::from(history.width.saturating_sub(4))),
                    Style::default().fg(*color).add_modifier(Modifier::BOLD),
                );
            }
        }
        for (row, item) in decorations
            .iter()
            .filter(|(row, _)| (start_row..end_row).contains(row))
        {
            if let Decor::CodeLabel(block) = item {
                let label = self.code_label(block, history.width);
                let size = UnicodeWidthStr::width(label.as_str()) as u16;
                let margin = if self.plain_style {
                    PLAIN_RIGHT_GUTTER + 3
                } else {
                    1
                };
                buffer.set_string(
                    history.right().saturating_sub(size + margin),
                    history.y + (row - start_row) as u16,
                    label,
                    Style::default().fg(
                        if self.hover_pointer.is_some_and(|(x, y)| {
                            y == history.y + (row - start_row) as u16
                                && self.code_at(x, y).is_some()
                        }) {
                            colors.accent_text
                        } else {
                            colors.muted
                        },
                    ),
                );
            }
        }
        for (row, item) in decorations
            .iter()
            .filter(|(row, _)| (start_row..end_row).contains(row))
        {
            if let Decor::Inspect(_) = item {
                let label = self.inspection_label();
                let width = UnicodeWidthStr::width(label) as u16;
                // Sit inside the record's surface instead of on its edge.
                let margin = if self.plain_style {
                    PLAIN_RIGHT_GUTTER + 3
                } else {
                    1
                };
                buffer.set_string(
                    history.right().saturating_sub(width + margin),
                    history.y + (row - start_row) as u16,
                    label,
                    Style::default().fg(colors.accent_text),
                );
            }
        }
        if self.home {
            if self.plain_style {
                self.draw_plain_welcome(frame, history_slot, colors);
            } else {
                self.draw_welcome_details(frame, history_slot, colors);
            }
        }
        for link in &self.visible_links {
            let hovered = self
                .hover_pointer
                .is_some_and(|(x, y)| y == link.y && x >= link.x && x < link.end_x);
            for x in link.x..link.end_x {
                if let Some(cell) = frame.buffer_mut().cell_mut((x, link.y)) {
                    let style = if hovered {
                        Style::default()
                            .fg(if self.plain_style {
                                self.semantic_colors().link
                            } else {
                                colors.accent_text
                            })
                            .add_modifier(Modifier::UNDERLINED | Modifier::BOLD)
                    } else if self.plain_style {
                        Style::default()
                            .fg(self.semantic_colors().link)
                            .add_modifier(Modifier::UNDERLINED)
                    } else {
                        Style::default().add_modifier(Modifier::UNDERLINED)
                    };
                    cell.set_style(style);
                }
            }
        }
        if let Some((mut start, mut end)) = self
            .selection_start
            .zip(self.selection_end)
            .filter(|(start, end)| start != end)
        {
            if start > end {
                std::mem::swap(&mut start, &mut end);
            }
            for index in start.0.max(start_row)..=end.0.min(end_row.saturating_sub(1)) {
                let y = history.y + (index - start_row) as u16;
                let left = if index == start.0 {
                    history.x.saturating_add(start.1 as u16)
                } else {
                    history.x
                };
                let right = if index == end.0 {
                    history.x.saturating_add(end.1 as u16)
                } else {
                    history.right().saturating_sub(1)
                };
                if left <= right {
                    for x in left..=right {
                        if let Some(cell) = frame.buffer_mut().cell_mut((x, y)) {
                            cell.set_style(Style::default().add_modifier(Modifier::REVERSED));
                        }
                    }
                }
            }
        }

        // One unobtrusive cell-wide track, always proportional to the full
        // rendered transcript. Dragging the thumb updates the history anchor.
        if rows > history.height as usize && self.scrollbar_area.height > 0 {
            let (top, thumb) = self.scrollbar_geometry();
            for i in 0..self.scrollbar_area.height {
                if let Some(cell) = frame
                    .buffer_mut()
                    .cell_mut((self.scrollbar_area.x, self.scrollbar_area.y + i))
                {
                    let active = (i as usize) >= top && (i as usize) < top + thumb;
                    cell.set_symbol(if active { "┃" } else { "│" });
                    cell.set_style(Style::default().fg(if active {
                        colors.accent
                    } else {
                        colors.border
                    }));
                }
            }
        }

        if show_navigation {
            let area = Rect::new(
                nav_row
                    .x
                    .saturating_add(if self.plain_style { 4 } else { 2 }),
                nav_row.y,
                nav_row.width.saturating_sub(4),
                1,
            );
            let action = self
                .preferences
                .language
                .text("Return to latest", "返回最新");
            let action_width = UnicodeWidthStr::width(action);
            let mut prefix = format!("{} · ", self.navigation_status());
            if UnicodeWidthStr::width(prefix.as_str()) + action_width > area.width as usize {
                prefix = format!("{} · ", self.preferences.language.text("History", "历史"));
            }
            if UnicodeWidthStr::width(prefix.as_str()) + action_width > area.width as usize {
                prefix.clear();
            }
            let offset = UnicodeWidthStr::width(prefix.as_str()) as u16;
            self.return_latest_area = Rect::new(
                area.x + offset,
                area.y,
                (action_width as u16).min(area.width.saturating_sub(offset)),
                1,
            );
            let hovered = self
                .hover_pointer
                .is_some_and(|(x, y)| self.return_latest_at(x, y));
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled(prefix, Style::default().fg(colors.muted)),
                    Span::styled(
                        action,
                        Style::default()
                            .fg(if hovered {
                                colors.accent_text
                            } else {
                                colors.accent
                            })
                            .add_modifier(Modifier::UNDERLINED | Modifier::BOLD),
                    ),
                ])),
                area,
            );
        }
        // One quiet input surface: a divider, neutral prompt marker and
        // multiline editor. Role colours belong to message rails only.
        if status_row.height > 0 {
            let mut status = self.activity_line(colors);
            if !self.queued.is_empty() {
                status.spans.push(Span::styled("   ", Style::default()));
                for (index, item) in self.queued.iter().take(4).enumerate() {
                    if index > 0 {
                        status.spans.push(Span::styled("  ", Style::default()));
                    }
                    let preview: String = item.chars().take(28).collect();
                    status.spans.push(Span::styled(
                        format!("↳ {preview}"),
                        Style::default().fg(colors.muted),
                    ));
                }
            }
            frame.render_widget(
                Paragraph::new(status),
                Rect::new(
                    status_row
                        .x
                        .saturating_add(if self.plain_style { 4 } else { 2 }),
                    status_row.y,
                    status_row
                        .width
                        .saturating_sub(if self.plain_style { 7 } else { 4 }),
                    1,
                ),
            );
        }
        self.draw_widget_slot(
            frame,
            above_widget_slot,
            crate::plugins::WidgetPlacement::AboveComposer,
            colors,
        );
        let border = if self.attach_mode {
            colors.accent_text
        } else {
            colors.border
        };
        let mut block = Block::default()
            .style(Style::default().bg(if self.plain_style {
                colors.background
            } else {
                colors.surface
            }))
            .borders(Borders::TOP)
            .border_style(Style::default().fg(border));
        if self.attach_mode {
            block = block.title_top(Line::styled(
                self.preferences
                    .language
                    .text(" Attachment path · Esc cancel ", " 附件路径 · Esc 取消 "),
                Style::default().fg(colors.accent_text),
            ));
        }
        if !self.attach_mode && self.editor.text().contains('\n') {
            let lines = self.editor.text().split('\n').count();
            let language = self.preferences.language;
            let action = if self.running {
                language.text(
                    "Enter steer · Alt+Enter queue",
                    "Enter 引导 · Alt+Enter 排队",
                )
            } else {
                language.text("Enter send", "Enter 发送")
            };
            block = block.title_top(Line::styled(
                format!(
                    " {lines} {} · Ctrl+J {} · {action} ",
                    language.text("lines", "行"),
                    language.text("newline", "换行")
                ),
                Style::default().fg(colors.muted),
            ));
        }
        frame.render_widget(block, composer);
        let editor_inner = Rect::new(
            composer.x.saturating_add(4),
            composer.y.saturating_add(editor_inset),
            composer.width.saturating_sub(6),
            editor_rows.min(composer.height.saturating_sub(composer_padding)),
        );
        if previous_editor_area != Rect::default() && previous_editor_area != editor_inner {
            self.clear_editor_selection();
        }
        self.editor_area = editor_inner;
        let (text, cx, cy) = self
            .editor
            .viewport(editor_inner.width, editor_inner.height);
        let input = if self.attach_mode {
            Paragraph::new(if self.attach_path.is_empty() {
                self.preferences
                    .language
                    .text("Attach file path…", "粘贴附件路径…")
                    .to_owned()
            } else {
                self.attach_path.clone()
            })
            .style(Style::default().fg(colors.text))
        } else if self.editor.text().is_empty() && self.plain_style {
            Paragraph::new("")
        } else if self.editor.text().is_empty() {
            let hint = if self.running {
                self.preferences.language.text(
                    "Enter steers; Alt+Enter / Ctrl+Q queues…",
                    "Enter 引导；Alt+Enter / Ctrl+Q 排队…",
                )
            } else {
                self.preferences.language.text(
                    "Describe a task, or / for commands…",
                    "描述任务，或输入 / 查看命令…",
                )
            };
            Paragraph::new(hint).style(Style::default().fg(colors.muted))
        } else {
            Paragraph::new(text).style(Style::default().fg(colors.text))
        };
        if self.ask.is_none() {
            frame.render_widget(
                Paragraph::new("›").style(
                    Style::default()
                        .fg(colors.text)
                        .add_modifier(Modifier::BOLD),
                ),
                Rect::new(
                    composer.x.saturating_add(2),
                    composer.y.saturating_add(editor_inset),
                    1,
                    1,
                ),
            );
        }
        frame.render_widget(input, editor_inner);
        if let Some((a, b)) = self.editor_selection {
            if a != b {
                for (row, left, right) in
                    self.editor
                        .selection_cells(editor_inner.width, editor_inner.height, a, b)
                {
                    for column in left..right.min(editor_inner.width) {
                        if let Some(cell) = frame
                            .buffer_mut()
                            .cell_mut((editor_inner.x + column, editor_inner.y + row))
                        {
                            cell.set_style(Style::default().add_modifier(Modifier::REVERSED));
                        }
                    }
                }
            }
        }
        if self.show_editor_cursor()
            && self.plugin_panel.is_none()
            && self.ask.is_none()
            // A picker owns the hardware cursor for its own input field.
            && self.picker.is_none()
            && editor_inner.width > 0
            && editor_inner.height > 0
            && self.editor_selection.is_none_or(|(a, b)| a == b)
        {
            let (cx, cy) = if self.attach_mode {
                (
                    unicode_width::UnicodeWidthStr::width(self.attach_path.as_str())
                        .min(editor_inner.width.saturating_sub(1) as usize)
                        as u16,
                    0,
                )
            } else {
                (cx, cy)
            };
            // Paint a software caret and retain its position for the IME.
            // Do NOT call frame.set_cursor_position here: ratatui would show
            // the hardware cursor on every animation / elapsed-time redraw.
            let caret = (
                editor_inner
                    .x
                    .saturating_add(cx.min(editor_inner.width - 1)),
                editor_inner
                    .y
                    .saturating_add(cy.min(editor_inner.height - 1)),
            );
            if let Some(cell) = frame.buffer_mut().cell_mut(caret) {
                cell.set_style(Style::default().bg(colors.text).fg(colors.background));
            }
            self.ime_cursor_position = Some(caret);
        }
        if let Some(ask) = &mut self.ask {
            // The question replaces the input text in place: no centred popup.
            frame.render_widget(Clear, editor_inner);
            self.ime_cursor_position =
                ask.draw_in(frame, editor_inner, colors, self.preferences.language);
        }
        self.draw_widget_slot(
            frame,
            below_widget_slot,
            crate::plugins::WidgetPlacement::BelowComposer,
            colors,
        );
        self.draw_widget_slot(
            frame,
            footer_widget_slot,
            crate::plugins::WidgetPlacement::Footer,
            colors,
        );
        if let Some(side) = sidebar_area {
            let block = Block::default()
                .borders(Borders::LEFT)
                .border_style(Style::default().fg(colors.border));
            let inner = block.inner(side);
            frame.render_widget(block, side);
            self.draw_widget_slot(
                frame,
                inner,
                crate::plugins::WidgetPlacement::Sidebar,
                colors,
            );
        }
        let status_area = Rect::new(
            footer_row.x.saturating_add(2),
            footer_row.y,
            footer_row.width.saturating_sub(4),
            1,
        );
        let hovered = self.hover_pointer.and_then(|(x, y)| self.link_at(x, y));
        let footer = if !self.notification.is_empty() && self.plain_style {
            self.plain_notification_line(colors, status_area.width as usize)
        } else if !self.notification.is_empty() {
            Line::from(Span::styled(
                excerpt(&self.notification, usize::from(status_area.width)),
                Style::default().fg(match self.notification_level {
                    NoticeLevel::Info => colors.accent_text,
                    NoticeLevel::Warning => colors.warning,
                    NoticeLevel::Error => colors.error,
                }),
            ))
        } else if let Some(target) = hovered {
            Line::from(Span::styled(
                format!(
                    "Ctrl+Click to open · {}",
                    compact_line(target, (status_area.width as usize).saturating_sub(22))
                ),
                Style::default().fg(colors.muted),
            ))
        } else {
            self.footer_line(colors, status_area.width as usize)
        };
        frame.render_widget(Paragraph::new(footer), status_area);
        if let Some(picker) = &mut self.picker {
            picker.draw(
                frame,
                Rect::new(area.x, history_slot.y, area.width, history_slot.height),
                colors,
                self.preferences.language,
            );
        }
        self.draw_panel(
            frame,
            full_area,
            Rect::new(area.x, history_slot.y, area.width, history_slot.height),
            colors,
        );
        self.draw_custom_ui(frame, full_area, colors);
    }
}

impl Drop for UiState {
    fn drop(&mut self) {
        if self.taskbar_enabled {
            super::terminal_feedback::clear_progress(&mut std::io::stdout());
        }
        self.discard_draft();
        for parked in self.session_drafts.values() {
            parked.discard_copies();
        }
    }
}

#[cfg(test)]
#[path = "view_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "redesign_tests.rs"]
mod redesign_tests;

#[cfg(test)]
#[path = "redesign_snapshots.rs"]
mod redesign_snapshots;

#[cfg(test)]
#[path = "graphite_tests.rs"]
mod graphite_tests;

#[cfg(test)]
#[path = "plain_tests.rs"]
mod plain_tests;

#[cfg(test)]
#[path = "semantic_tests.rs"]
mod semantic_tests;

#[cfg(test)]
#[path = "message_tests.rs"]
mod message_tests;

#[cfg(test)]
#[path = "tool_record_tests.rs"]
mod tool_record_tests;

#[cfg(test)]
mod hardening_tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};

    fn state() -> UiState {
        UiState::new(
            "mock".into(),
            "hardening".into(),
            ThemeStore::load_at(None).unwrap(),
            &[Message::new("system", "root")],
        )
    }

    #[test]
    fn navigation_status_is_safe_after_home_clears_entries() {
        let mut ui = state();
        ui.navigation_turn = Some(0);
        ui.enter_home();
        assert!(ui.navigation_turn.is_none());
        assert!(ui.pending_scroll_turn.is_none());
        let _ = ui.navigation_status();
    }

    #[test]
    fn scrolling_a_short_transcript_does_not_enter_browse_mode() {
        let mut ui = state();
        ui.history_max_scroll = 0;
        ui.scroll(20);
        assert!(ui.scroll_anchor.is_none());
        assert_eq!(ui.scroll_from_bottom, 0);
    }

    #[test]
    fn resize_clears_editor_selection_without_touching_the_draft() {
        let mut ui = state();
        ui.editor.paste("keep this draft");
        let mut wide = Terminal::new(TestBackend::new(100, 24)).unwrap();
        wide.draw(|frame| ui.draw(frame)).unwrap();
        assert!(ui.start_editor_selection(ui.editor_area.x + 1, ui.editor_area.y));
        assert!(ui.editor_selection.is_some());
        let mut narrow = Terminal::new(TestBackend::new(45, 24)).unwrap();
        narrow.draw(|frame| ui.draw(frame)).unwrap();
        assert!(ui.editor_selection.is_none());
        assert_eq!(ui.editor.text(), "keep this draft");
    }

    #[test]
    fn tiny_terminal_clears_history_selection_instead_of_restoring_it() {
        let mut ui = UiState::new(
            "mock".into(),
            "hardening".into(),
            ThemeStore::load_at(None).unwrap(),
            &[
                Message::new("assistant", "one"),
                Message::new("assistant", "two"),
            ],
        );
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| ui.draw(frame)).unwrap();
        ui.select_start(ui.selection_area.x + 2, ui.selection_area.y);
        ui.select_drag(ui.selection_area.x + 5, ui.selection_area.y);
        assert!(ui.selected_text().is_some());
        let mut tiny = Terminal::new(TestBackend::new(12, 6)).unwrap();
        tiny.draw(|frame| ui.draw(frame)).unwrap();
        assert!(ui.selection_start.is_none());
        assert!(ui.selection_end.is_none());
        let mut normal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        normal.draw(|frame| ui.draw(frame)).unwrap();
        assert!(ui.selected_text().is_none());
    }
}
