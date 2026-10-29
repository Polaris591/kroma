use super::{
    attachments::{Attachment, MAX_ATTACHMENTS},
    editor::Editor,
    links, markdown,
    picker::Picker,
    theme::{Theme, ThemeStore},
};
use crate::{events::RuntimeEvent, protocol::Message};
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Paragraph},
    Frame,
};
use serde_json::Value;
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};
use tokio::sync::mpsc::UnboundedSender;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

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
        .position(|ch| matches!(ch, '•' | '◦' | '▪' | '│' | '▸' | '◆'))
    {
        if marker < 16 && chars.get(marker + 1) == Some(&' ') {
            return (marker + 2).min(width.saturating_sub(1));
        }
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
    let width = width.max(1);
    let mut rows = Vec::new();
    // Keep a stable source-line identity for every visual row. The absolute
    // visual row index changes when the terminal width changes.
    let mut source_lines = Vec::new();
    for (source_index, line) in text.lines.into_iter().enumerate() {
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
                    if ch == ' ' && columns == hanging_indent {
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
                    if grapheme == " " && columns == hanging_indent {
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

/// The reading column.
///
/// `/layout wide` fills the terminal. The default comfortable layout centres a
/// column of prose width: a text block glued to the left edge of a wide
/// terminal leaves one large empty margin, and long paragraphs are harder to
/// follow when the line has no visible right boundary. Centring keeps both
/// margins equal without lengthening the line; narrow terminals still use
/// their full width.
fn reading_area(area: Rect, full_width: bool) -> Rect {
    let margin = if area.width >= 100 {
        2
    } else if area.width >= 55 {
        1
    } else {
        0
    };
    let available = area.width.saturating_sub(margin * 2);
    let width = if full_width {
        available
    } else {
        (super::tool_display::prose_width(usize::from(available)) as u16).min(available)
    };
    Rect::new(
        area.x
            .saturating_add(margin)
            .saturating_add(available.saturating_sub(width) / 2),
        area.y,
        width,
        area.height,
    )
}

fn body_line(line: Line<'static>) -> Line<'static> {
    // Bodies hang under the role label, so every message shares one text
    // column instead of starting a cell left of its own title.
    let mut spans = vec![Span::raw("     ")];
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

struct Entry {
    kind: Kind,
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

struct HistoryCache {
    width: u16,
    visible: usize,
    tools_expanded: bool,
    reasoning_expanded: bool,
    has_activity_tail: bool,
    theme: Theme,
    text: Text<'static>,
    source_lines: Vec<usize>,
    links: Vec<links::Region>,
}

impl Entry {
    fn user(body: impl Into<String>) -> Self {
        Self::new(Kind::User, "YOU", body)
    }
    fn new(kind: Kind, title: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            kind,
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

pub enum UiEvent {
    TaskStarted,
    ModelStarted,
    Usage {
        input: Option<u64>,
        output: Option<u64>,
        cached: Option<u64>,
        reasoning: Option<u64>,
    },
    TitleUsage {
        requests: usize,
        input: Option<u64>,
        output: Option<u64>,
        cached: Option<u64>,
        reasoning: Option<u64>,
    },
    ApecUsage {
        requests: usize,
        input: Option<u64>,
        output: Option<u64>,
        cached: Option<u64>,
    },
    Context {
        estimated: usize,
    },
    ActualInput(u64),
    Text(String),
    ReasoningDelta(String),
    ReasoningFinal(String),
    ToolStarted {
        name: String,
        id: String,
        args: String,
    },
    ToolFinished {
        id: String,
        result: String,
        failed: bool,
    },
    ToolOutput {
        id: String,
        stream: String,
        chunk: String,
    },
    SessionTitleUpdated(String),
    TaskFinished,
}

pub struct UiSink(pub UnboundedSender<UiEvent>);

impl crate::events::EventSink for UiSink {
    fn on_event(&self, event: RuntimeEvent<'_>) {
        let message = match event {
            RuntimeEvent::TaskStarted => Some(UiEvent::TaskStarted),
            RuntimeEvent::ModelStarted { .. } => Some(UiEvent::ModelStarted),
            RuntimeEvent::ContextPrepared {
                estimated_input_tokens,
                ..
            } => Some(UiEvent::Context {
                estimated: estimated_input_tokens,
            }),
            RuntimeEvent::TextDelta(delta) => Some(UiEvent::Text(delta.to_owned())),
            RuntimeEvent::ReasoningDelta(delta) => Some(UiEvent::ReasoningDelta(delta.to_owned())),
            RuntimeEvent::ModelFinished {
                input_tokens,
                output_tokens,
                reasoning_tokens,
                cached_tokens,
                reasoning,
                ..
            } => {
                let _ = self.0.send(UiEvent::Usage {
                    input: input_tokens,
                    output: output_tokens,
                    cached: cached_tokens,
                    reasoning: reasoning_tokens,
                });
                if let Some(tokens) = input_tokens {
                    let _ = self.0.send(UiEvent::ActualInput(tokens));
                }
                if let Some(text) = reasoning.filter(|s| !s.is_empty()) {
                    let _ = self.0.send(UiEvent::ReasoningFinal(text.to_owned()));
                }
                None
            }
            RuntimeEvent::CompactionUsage {
                requests,
                input_tokens,
                output_tokens,
                cached_tokens,
            } => Some(UiEvent::ApecUsage {
                requests,
                input: input_tokens,
                output: output_tokens,
                cached: cached_tokens,
            }),
            RuntimeEvent::ToolStarted {
                name,
                id,
                arguments,
            } => Some(UiEvent::ToolStarted {
                name: name.to_owned(),
                id: id.to_owned(),
                args: tool_arguments(name, arguments),
            }),
            RuntimeEvent::ToolFinished {
                id,
                result,
                is_error,
                ..
            } => Some(UiEvent::ToolFinished {
                id: id.to_owned(),
                result: tool_result_details(result),
                failed: is_error,
            }),
            RuntimeEvent::ToolOutput { id, stream, chunk } => Some(UiEvent::ToolOutput {
                id: id.to_owned(),
                stream: stream.to_owned(),
                chunk: chunk.to_owned(),
            }),
            RuntimeEvent::TaskFinished { .. } => Some(UiEvent::TaskFinished),
            _ => None,
        };
        if let Some(message) = message {
            let _ = self.0.send(message);
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

/// Shorten text by terminal cell width rather than UTF-8 bytes or chars.
/// A long command must not take over the conversation's reading column.
fn ellipsize_cells(value: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if UnicodeWidthStr::width(value) <= width {
        return value.to_owned();
    }
    let mut output = String::new();
    let mut used = 0;
    for ch in value.chars() {
        let cells = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + cells > width - 1 {
            break;
        }
        output.push(ch);
        used += cells;
    }
    output.push('…');
    output
}

fn compact_line(value: &str, width: usize) -> String {
    ellipsize_cells(
        &value.split_whitespace().collect::<Vec<_>>().join(" "),
        width,
    )
}

/// Find the failure's useful diagnostic, not its full tool-result envelope.
/// Stored and live tool results already use the same tool_result_details format.
fn failure_summary(details: &str) -> String {
    let lines = details
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
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
                }
            })
        })
        .or_else(|| {
            lines
                .iter()
                .copied()
                .find(|line| !line.starts_with("exit: "))
        })
        .unwrap_or("Tool failed");
    let message = message.trim().trim_start_matches("stderr: ");
    match exit {
        Some(code) => format!("exit {code} · {message}"),
        None => message.to_owned(),
    }
}

fn tool_arguments(name: &str, raw: &str) -> String {
    let value: Value = match serde_json::from_str(raw) {
        Ok(value) => value,
        Err(_) => return raw.to_owned(),
    };
    if name == "shell" {
        return value["command"]
            .as_str()
            .map_or_else(|| raw.to_owned(), str::to_owned);
    }
    value.to_string()
}

fn tool_result_details(raw: &str) -> String {
    let Ok(value) = serde_json::from_str::<Value>(raw) else {
        return raw.to_owned();
    };
    if let Some(error) = value.get("error") {
        return error.as_str().unwrap_or(raw).to_owned();
    }
    let mut lines = Vec::new();
    if let Some(code) = value.get("exit_code") {
        lines.push(format!("exit: {}", code));
    }
    if let Some(stdout) = value.get("stdout").and_then(Value::as_str) {
        if !stdout.trim().is_empty() {
            lines.push(stdout.to_owned());
        }
    }
    if let Some(stderr) = value.get("stderr").and_then(Value::as_str) {
        if !stderr.trim().is_empty() {
            lines.push(format!("stderr: {stderr}"));
        }
    }
    if let Some(content) = value.get("content").and_then(Value::as_array) {
        for item in content.iter() {
            if let Some(text) = item.get("text").and_then(Value::as_str) {
                lines.push(text.to_owned());
            }
        }
    }
    for key in ["stdout_file", "stderr_file"] {
        if let Some(path) = value.get(key).and_then(Value::as_str) {
            lines.push(format!("{key}: {path}"));
        }
    }
    if lines.is_empty() {
        return raw.to_owned();
    }
    lines.join(
        "
",
    )
}

#[derive(Clone, Copy)]
pub(super) enum TaskOutcome {
    Completed,
    Failed,
    Cancelled,
}

pub struct UiState {
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
    pub usage: crate::usage::UsageMeter,
    pub title_usage: crate::usage::UsageMeter,
    pub apec_usage: crate::usage::UsageMeter,
    pub attachments: Vec<Attachment>,
    removed_drafts: Vec<Attachment>,
    saved_draft_attachments: Option<Vec<Attachment>>,
    pub attach_mode: bool,
    pub attach_path: String,
    next_marker: u32,
    pub themes: ThemeStore,
    pub session: String,
    session_title: Option<String>,
    /// Plain `kroma` is a welcome page, not an implicit resumed session.
    home: bool,
    pub model: String,
    pub model_profile: Option<String>,
    pub model_catalog: Option<crate::sessions::SessionCatalog>,
    history_visible: usize,
    history_max_scroll: usize,
    history_cache: Option<HistoryCache>,
    selection_rows: Vec<String>,
    selection_area: Rect,
    selection_start: Option<(usize, usize)>,
    selection_end: Option<(usize, usize)>,
    history_top_row: usize,
    scrollbar_drag: Option<usize>,
    scrollbar_pointer_y: Option<u16>,
    scrollbar_area: Rect,
    selection_dragging: bool,
    selection_edge: i8,
    visible_links: Vec<VisibleLink>,
    hover_pointer: Option<(u16, u16)>,
    entries: Vec<Entry>,
    pub running: bool,
    pub notification: String,
    pub scroll_from_bottom: usize,
    /// Pinned absolute top row while inspecting older output. A stream may
    /// add rows below without moving the reader's selection or context.
    scroll_anchor: Option<usize>,
    prepend_rows_before: Option<usize>,
    pub viewport_height: u16,
    pub tools_expanded: bool,
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

// Three original Kroma logo wedges, lit in sequence. No generic spinner.
const BRAND_PULSE_FRAMES: usize = 6;

impl UiState {
    pub fn new(model: String, session: String, themes: ThemeStore, messages: &[Message]) -> Self {
        let mut result = Self {
            preferences: super::preferences::Preferences::default(),
            preferences_path: None,
            task_outcome: None,
            task_started: None,
            task_elapsed: None,
            unread_updates: 0,
            editor: Editor::default(),
            picker: None,
            usage: crate::usage::UsageMeter::default(),
            title_usage: crate::usage::UsageMeter::default(),
            apec_usage: crate::usage::UsageMeter::default(),
            attachments: Vec::new(),
            removed_drafts: Vec::new(),
            saved_draft_attachments: None,
            attach_mode: false,
            attach_path: String::new(),
            next_marker: super::editor::ATTACHMENT_START,
            themes,
            model,
            model_profile: None,
            model_catalog: None,
            history_visible: 300,
            history_max_scroll: 0,
            history_cache: None,
            selection_rows: Vec::new(),
            selection_area: Rect::default(),
            selection_start: None,
            selection_end: None,
            history_top_row: 0,
            scrollbar_drag: None,
            scrollbar_pointer_y: None,
            scrollbar_area: Rect::default(),
            selection_dragging: false,
            selection_edge: 0,
            visible_links: Vec::new(),
            hover_pointer: None,
            session,
            session_title: None,
            home: false,
            entries: Vec::new(),
            running: false,
            notification: String::new(),
            scroll_from_bottom: 0,
            scroll_anchor: None,
            prepend_rows_before: None,
            viewport_height: 0,
            tools_expanded: false,
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
            super::command_catalog::items(self.preferences.language),
            query,
        );
        picker.completion = true;
        self.picker = Some(picker);
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
                format!("{}{elapsed}", language.text("Completed", "已完成"))
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
        let latest = language.text(
            "Browsing history · Ctrl+End: latest",
            "正在浏览历史 · Ctrl+End 返回最新",
        );
        if self.unread_updates > 0 {
            format!(
                "{latest} · {} {}",
                self.unread_updates,
                language.text("new updates", "条新动态")
            )
        } else {
            latest.into()
        }
    }

    pub fn usage_report(&self) -> String {
        if self.preferences.language == super::preferences::Language::En {
            return format!(
                "{}\n{}",
                self.context_display(),
                self.usage
                    .report_breakdown(&self.title_usage, &self.apec_usage)
            );
        }
        let context = self.context_display().replace("Ctx", "上下文");
        let mut rows = vec![context, "本次进程的用量（不含启动前的历史请求）：".into()];
        for (label, meter) in [
            ("编程模型", &self.usage),
            ("自动标题", &self.title_usage),
            ("APEC 摘要", &self.apec_usage),
        ] {
            rows.push(
                meter
                    .summary_line(label)
                    .replace("call(s)", "次请求")
                    .replace("In ", "输入 ")
                    .replace("Out ", "输出 ")
                    .replace("Cache read ", "缓存读取 ")
                    .replace("Hit ", "命中率 ")
                    .replace("Reasoning ", "推理 "),
            );
        }
        rows.push("缓存读取属于输入的一部分；推理属于输出的一部分。".into());
        rows.push("— 表示未报告；≥ 表示部分请求未报告；~ 表示估算或部分测量。".into());
        rows.push("APEC 仅统计已完成的摘要；未完成或失败的请求可能产生未报告的费用。".into());
        rows.join("\n")
    }

    /// The three measurement widths, longest first. Kept separate so callers
    /// can ask whether a variant fits *whole* before spending the row on it.
    fn usage_variants(&self) -> [String; 3] {
        use crate::usage::compact_tokens;
        let language = self.preferences.language;
        let last = self.usage.last_request();
        let unknown = language.text("unreported", "未报告");
        let count =
            |value: Option<u64>| value.map(compact_tokens).unwrap_or_else(|| unknown.into());
        let input = count(last.and_then(|usage| usage.input));
        let output = count(last.and_then(|usage| usage.output));
        let cache = match (last.and_then(|u| u.input), last.and_then(|u| u.cached)) {
            (Some(input), Some(cached)) if input > 0 && cached <= input => {
                format!("{:.0}%", cached as f64 * 100.0 / input as f64)
            }
            _ => unknown.into(),
        };
        [
            format!(
                "{} {input} / {} {output} · {} {cache}",
                language.text("Last input", "上次输入"),
                language.text("output", "输出"),
                language.text("Cache", "缓存命中")
            ),
            format!(
                "{} {input} / {output} · {} {cache}",
                language.text("In/out", "输入/输出"),
                language.text("Cache", "缓存")
            ),
            format!("{} {cache}", language.text("Cache", "缓存")),
        ]
    }

    /// A measurement string that fits `width` without truncation, if one exists.
    #[cfg(test)]
    fn usage_status_fitting(&self, width: usize) -> Option<String> {
        self.usage_variants()
            .into_iter()
            .find(|value| UnicodeWidthStr::width(value.as_str()) <= width)
    }

    #[cfg(test)]
    fn usage_status(&self, width: usize) -> String {
        self.usage_status_fitting(width)
            .unwrap_or_else(|| ellipsize_cells(&self.usage_variants()[2], width))
    }
    /// One quiet footer. Usage and cache readings are secondary to the
    /// conversation, so fit them *whole* rather than truncating numbers or
    /// repeating command hints on every frame. /usage retains the breakdown.
    fn footer_line(&self, colors: Theme, width: usize) -> Line<'static> {
        let language = self.preferences.language;
        let context = self
            .compact_context()
            .replace("Ctx", language.text("Context", "上下文"));
        let thinking = format!(
            "{} {}",
            language.text("Think", "思考"),
            self.thinking.as_deref().unwrap_or("—")
        );
        let metadata = format!("{} · {thinking} · {context}", self.model);
        let compact_metadata = format!("{} · {context}", self.model);
        let fallback = format!("{thinking} · {context}");
        let mut candidates = Vec::new();
        if self.usage.calls > 0 {
            let variants = self.usage_variants();
            // On wide terminals show input/output and cached percentage.
            // On narrower ones keep model, context and cache before detail.
            candidates.push(format!("{metadata} · {}", variants[1]));
            candidates.push(format!("{metadata} · {}", variants[2]));
            candidates.push(format!("{compact_metadata} · {}", variants[2]));
            candidates.push(format!("{context} · {}", variants[2]));
        } else {
            candidates.push(metadata);
            candidates.push(fallback);
            candidates.push(compact_metadata);
        }
        // Very narrow terminal: preserve context first, and report cache as
        // unknown instead of inventing zero when it has not been measured.
        candidates.push(context);
        let chosen = candidates
            .into_iter()
            .find(|candidate| UnicodeWidthStr::width(candidate.as_str()) <= width)
            .unwrap_or_else(|| ellipsize_cells(&self.footer_status(width), width));
        let mut spans = Vec::new();
        for (index, section) in chosen.split(" · ").enumerate() {
            if index > 0 {
                spans.push(Span::styled(" · ", Style::default().fg(colors.muted)));
            }
            let ink = if section.starts_with("Cache ") || section.starts_with("缓存 ") {
                colors.accent
            } else {
                colors.muted
            };
            spans.push(Span::styled(section.to_owned(), Style::default().fg(ink)));
        }
        Line::from(spans)
    }

    /// Record the turn's wall-clock duration.
    ///
    /// The completion row must not depend on a `TaskFinished` event: the final
    /// drain keeps only measurement events, so the terminal event can be
    /// dropped and the row would silently lose its duration.
    pub fn finish_task(&mut self) {
        self.running = false;
        self.task_elapsed = self.task_started.take().map(|start| start.elapsed());
    }

    pub fn add_user(&mut self, text: &str) {
        self.home = false;
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
        self.scroll_from_bottom = 0;
        self.scroll_anchor = None;
        self.running = true;
        self.set_phase("preparing");
    }

    pub fn attach(&mut self, mut item: Attachment) -> Result<(), String> {
        if self.attachments.len() >= MAX_ATTACHMENTS {
            Self::discard_copy(&item);
            return Err(format!("Maximum {MAX_ATTACHMENTS} attachments"));
        }
        while self.next_marker <= super::editor::ATTACHMENT_END
            && self
                .editor
                .contains_attachment(char::from_u32(self.next_marker).unwrap())
        {
            self.next_marker += 1;
        }
        let marker = char::from_u32(self.next_marker)
            .filter(|_| self.next_marker <= super::editor::ATTACHMENT_END)
            .ok_or_else(|| {
                Self::discard_copy(&item);
                "Too many draft insertions; clear or send to reset".to_string()
            })?;
        self.next_marker += 1;
        let label = item.inline_label();
        item.marker = Some(marker);
        self.editor.insert_attachment(marker, label);
        self.attachments.push(item);
        Ok(())
    }

    pub fn finish_attachment_send(&mut self) {
        // The submitted WAL owns these file refs from here onward.
        self.attachments.clear();
        for item in self.removed_drafts.drain(..) {
            Self::discard_copy(&item);
        }
        if let Some(draft) = self.saved_draft_attachments.take() {
            for item in draft {
                Self::discard_copy(&item);
            }
        }
        self.next_marker = super::editor::ATTACHMENT_START;
    }

    fn discard_copy(item: &Attachment) {
        if item.draft_copy {
            let _ = std::fs::remove_file(&item.path);
        }
    }

    /// The editor stores complete historical input including its opaque
    /// attachment references. No media is loaded just to show a past prompt.
    pub fn restore_input_history(&mut self, messages: &[Message]) {
        let entries = messages
            .iter()
            .filter(|m| m.role == "user")
            .rev()
            .take(120)
            .collect::<Vec<_>>();
        let mut records = Vec::with_capacity(entries.len());
        for message in entries.into_iter().rev() {
            let mut text = message.content.clone().unwrap_or_default();
            let mut labels = HashMap::new();
            let mut parts = message.provider_items.clone().unwrap_or_default();
            let mut candidate = super::editor::ATTACHMENT_START;
            for part in &mut parts {
                let Some(item) = Attachment::from_item(part) else {
                    continue;
                };
                let marker = if let Some(ch) = item.marker {
                    ch
                } else {
                    while candidate <= super::editor::ATTACHMENT_END
                        && char::from_u32(candidate).is_some_and(|ch| text.contains(ch))
                    {
                        candidate += 1;
                    }
                    // Old or malformed sessions may contain more attachments
                    // than the editor's private-use marker range. Leave any
                    // excess attachment unmarked (legacy list format) rather
                    // than assigning a non-marker Unicode scalar or panicking.
                    let Some(ch) = char::from_u32(candidate)
                        .filter(|_| candidate <= super::editor::ATTACHMENT_END)
                    else {
                        continue;
                    };
                    candidate += 1;
                    // Older list-style sessions had no inline position.
                    text.push(ch);
                    part["marker"] = serde_json::json!(ch.to_string());
                    ch
                };
                labels.insert(marker, format!("【{}】", item.inline_label()));
            }
            if !text.is_empty() {
                records.push((text, labels, parts));
            }
        }
        self.editor.restore_history(records);
    }

    pub fn history_navigated(&mut self, previous: Option<usize>) {
        let current = self.editor.history_index();
        if current == previous {
            return;
        }
        if previous.is_none() && current.is_some() {
            // Editing a recalled line starts a new draft. The editor replaces
            // its earlier saved draft on the next Up; keep file ownership in
            // sync with that simple history behavior.
            if let Some(abandoned) = self.saved_draft_attachments.take() {
                for item in abandoned {
                    Self::discard_copy(&item);
                }
            }
            self.saved_draft_attachments = Some(std::mem::take(&mut self.attachments));
        }
        self.attachments = if current.is_some() {
            self.editor
                .history_attachments()
                .iter()
                .filter_map(Attachment::from_item)
                .collect()
        } else {
            self.saved_draft_attachments.take().unwrap_or_default()
        };
    }

    pub fn remember_submitted_attachments(&mut self) {
        self.editor
            .remember_attachments(self.attachments.iter().map(Attachment::item).collect());
    }

    pub fn prune_attachments(&mut self) {
        let mut index = 0;
        while index < self.removed_drafts.len() {
            if self.removed_drafts[index]
                .marker
                .is_some_and(|marker| self.editor.contains_attachment(marker))
            {
                self.attachments.push(self.removed_drafts.remove(index));
            } else {
                index += 1;
            }
        }
        self.attachments.retain(|item| {
            if let Some(marker) = item.marker {
                if !self.editor.contains_attachment(marker) {
                    self.removed_drafts.push(item.clone());
                    return false;
                }
            }
            true
        });
    }

    pub fn discard_draft(&mut self) {
        self.editor.clear();
        self.attach_mode = false;
        self.attach_path.clear();
        self.next_marker = super::editor::ATTACHMENT_START;
        for item in self.attachments.drain(..) {
            Self::discard_copy(&item);
        }
        for item in self.removed_drafts.drain(..) {
            Self::discard_copy(&item);
        }
        if let Some(draft) = self.saved_draft_attachments.take() {
            for item in draft {
                Self::discard_copy(&item);
            }
        }
    }

    pub fn configure_status(&mut self, window: usize, thinking: Option<String>) {
        self.context_window = window;
        self.thinking = thinking;
    }

    pub fn reset_context(&mut self) {
        self.context_input = None;
        self.context_estimated = true;
    }

    pub fn context_display(&self) -> String {
        use crate::usage::compact_tokens;
        let window = self.context_window as u64;
        if window == 0 {
            return "Ctx —".into();
        }
        match self.context_input {
            Some(input) => {
                let percent = Self::context_percent(input, window);
                let marker = if self.context_estimated { "~" } else { "" };
                format!(
                    "Ctx {marker}{} / {} · {marker}{percent}%",
                    compact_tokens(input),
                    compact_tokens(window)
                )
            }
            None => format!("Ctx — / {}", compact_tokens(window)),
        }
    }

    fn context_percent(input: u64, window: u64) -> String {
        let percent = input as f64 * 100.0 / window as f64;
        if percent > 0.0 && percent < 0.1 {
            format!("{percent:.2}")
        } else if percent < 1.0 {
            format!("{percent:.1}")
        } else {
            format!("{percent:.0}")
        }
    }

    fn compact_context(&self) -> String {
        match (self.context_input, self.context_window) {
            (Some(input), window) if window > 0 => {
                let percent = Self::context_percent(input, window as u64);
                format!(
                    "Ctx {}{percent}%",
                    if self.context_estimated { "~" } else { "" }
                )
            }
            _ => "Ctx —".into(),
        }
    }

    fn footer_status(&self, width: usize) -> String {
        let language = self.preferences.language;
        let think = format!(
            "{} {}",
            language.text("Reasoning", "推理"),
            self.thinking.as_deref().unwrap_or("—")
        );
        let context = self
            .compact_context()
            .replace("Ctx", language.text("Context", "上下文"));
        let separator = " · ";
        let metadata_width = UnicodeWidthStr::width(think.as_str())
            + UnicodeWidthStr::width(context.as_str())
            + 2 * UnicodeWidthStr::width(separator);
        if width >= 46 && metadata_width + 8 <= width {
            let model = ellipsize_cells(&self.model, (width - metadata_width).min(24));
            return format!("{model}{separator}{think}{separator}{context}");
        }
        if width >= 30 {
            let level = self.thinking.as_deref().unwrap_or("—");
            let context = self
                .compact_context()
                .replace("Ctx", language.text("Context", "上下文"));
            let right = format!("{level} · {context}");
            let available = width.saturating_sub(UnicodeWidthStr::width(right.as_str()) + 3);
            return format!("{} · {right}", ellipsize_cells(&self.model, available));
        }
        ellipsize_cells(&format!("{} · {}", self.model, context), width)
    }

    pub fn note(&mut self, text: impl Into<String>) {
        self.notification = text.into();
        if self.preferences.language == super::preferences::Language::Zh {
            self.notification = self
                .notification
                .replace("【Clipboard image】", "【剪贴板图片】");
        }
    }

    fn set_phase(&mut self, phase: &'static str) {
        if self.phase != phase {
            self.phase = phase;
            self.phase_since = (phase != "ready").then(Instant::now);
        }
    }

    /// The task loop already wakes every 40ms; paint about five frames per
    /// second while active, without reflowing history or polling the provider.
    pub fn tick_activity(&mut self) -> bool {
        if !self.running || self.last_activity_frame.elapsed() < Duration::from_millis(180) {
            return false;
        }
        self.last_activity_frame = Instant::now();
        self.activity_frame = (self.activity_frame + 1) % BRAND_PULSE_FRAMES;
        true
    }

    fn activity_status(&self) -> String {
        if !self.running {
            return self.preferences.language.text("Ready", "就绪").into();
        }
        let phase = match self.phase {
            "thinking" => self.preferences.language.text("Thinking", "正在思考"),
            "waiting" => self.preferences.language.text("Connecting", "正在连接模型"),
            "streaming" => self.preferences.language.text("Writing", "正在生成回复"),
            "tool" => self
                .preferences
                .language
                .text("Running tool", "正在执行工具"),
            _ => self.preferences.language.text("Preparing", "正在准备"),
        };
        let elapsed = self
            .phase_since
            .map_or(0, |start| start.elapsed().as_secs());
        format!("{phase} · {elapsed}s")
    }

    fn activity_line(&self, colors: Theme) -> Line<'static> {
        if !self.running {
            // The outcome marker matches the transcript markers above, so a
            // finished turn is recognisable without reading the words.
            let (marker, color) = match self.task_outcome {
                Some(TaskOutcome::Completed) => ("✓", colors.user),
                Some(TaskOutcome::Failed) => ("✕", colors.error),
                Some(TaskOutcome::Cancelled) => ("⊘", colors.muted),
                None => ("·", colors.muted),
            };
            return Line::from(vec![
                Span::styled(format!("{marker}  "), Style::default().fg(color)),
                Span::styled(self.task_status(), Style::default().fg(colors.muted)),
            ]);
        }
        let mut spans = Vec::with_capacity(5);
        for (index, ink) in [colors.accent, colors.text, colors.tool]
            .into_iter()
            .enumerate()
        {
            let active = index == self.activity_frame / 2;
            let faded = match (ink, colors.background) {
                (ratatui::style::Color::Rgb(r, g, b), ratatui::style::Color::Rgb(br, bg, bb)) => {
                    // Blend with the surface rather than abruptly flashing on/off.
                    ratatui::style::Color::Rgb(
                        ((u16::from(r) + 3 * u16::from(br)) / 4) as u8,
                        ((u16::from(g) + 3 * u16::from(bg)) / 4) as u8,
                        ((u16::from(b) + 3 * u16::from(bb)) / 4) as u8,
                    )
                }
                _ => ink,
            };
            spans.push(Span::styled(
                "▲",
                Style::default()
                    .fg(if active { ink } else { faded })
                    .add_modifier(if active {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
            ));
        }
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            self.activity_status(),
            Style::default().fg(colors.text),
        ));
        Line::from(spans)
    }

    fn show_editor_cursor(&self) -> bool {
        // An empty Steer/Queue composer remains usable, but its hardware
        // caret should not blink while the model is doing the active work.
        // The first typed character brings the caret back immediately.
        !self.running || !self.editor.text().is_empty() || self.attach_mode
    }

    /// Brief feedback is shown until the user's next interaction, not
    /// permanently in place of the footer's normal help. Important errors
    /// are additionally recorded as scrollable transcript events.
    pub fn dismiss_notification(&mut self) {
        self.notification.clear();
    }

    pub fn refresh_session_title(&mut self) {
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
            "CANCELLED" => language.text(title, "已取消"),
            _ => title,
        }
    }

    pub fn info(&mut self, title: &str, content: impl Into<String>) {
        self.history_cache = None;
        self.entries.push(Entry::new(Kind::Info, title, content));
        // Explicit command results should be visible immediately, even when
        // the user was browsing old messages. Streaming still preserves its
        // existing scroll anchor unless the user actively jumps to latest.
        self.scroll_to_bottom();
    }

    pub fn load_history(&mut self, messages: &[Message]) {
        self.task_outcome = None;
        self.unread_updates = 0;
        self.home = false;
        self.history_cache = None;
        self.mouse_release();
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
                            self.entries
                                .push(Entry::new(Kind::Assistant, "KROMA", text));
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
        self.entries.clear();
        self.session_title = None;
        self.history_cache = None;
        self.active_assistant = None;
        self.active_reasoning = None;
        self.scroll_anchor = None;
        self.scroll_from_bottom = 0;
        self.mouse_release();
        self.selection_start = None;
        self.selection_end = None;
        self.reset_context();
    }

    pub fn event(&mut self, event: UiEvent) {
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
                }));
            }
            UiEvent::TitleUsage {
                requests,
                input,
                output,
                cached,
                reasoning,
            } => {
                self.title_usage.record_batch(
                    requests,
                    Some(&crate::model::Usage {
                        input,
                        output,
                        cached,
                        reasoning,
                        total: None,
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
                    }),
                );
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
            UiEvent::ModelStarted => {
                self.active_assistant = None;
                self.active_reasoning = None;
                self.set_phase("waiting");
            }
            UiEvent::Text(delta) => {
                self.set_phase("streaming");
                if let Some(index) = self.active_assistant {
                    self.entries[index].body.push_str(&delta);
                } else {
                    self.entries
                        .push(Entry::new(Kind::Assistant, "KROMA", delta));
                    self.active_assistant = Some(self.entries.len() - 1);
                }
            }
            UiEvent::ReasoningDelta(delta) => {
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
                if let Some(index) = self.active_reasoning {
                    // Reconcile with the durable result; do not duplicate the
                    // live deltas when the model finishes.
                    self.entries[index].body = reasoning;
                } else if !reasoning.is_empty() {
                    // Fallback for model backends that only provide reasoning
                    // in the completed assistant message.
                    self.insert_reasoning(reasoning);
                }
            }
            UiEvent::ToolStarted { name, id, args } => {
                self.set_phase("tool");
                let mut entry = Entry::new(Kind::Tool, format!("{name} · running"), args);
                entry.call_id = Some(id);
                entry.started = Some(Instant::now());
                self.entries.push(entry);
                self.active_assistant = None;
                self.active_reasoning = None;
            }
            UiEvent::ToolOutput { id, stream, chunk } => {
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
    }

    fn insert_reasoning(&mut self, text: String) {
        let entry = Entry::new(Kind::Reasoning, "THINKING", text);
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
        self.scroll_from_bottom = if amount.is_positive() {
            self.scroll_from_bottom.saturating_add(amount as usize)
        } else {
            self.scroll_from_bottom
                .saturating_sub(amount.unsigned_abs())
        };
        if self.scroll_from_bottom == 0 {
            self.scroll_anchor = None;
        } else {
            self.scroll_anchor = Some(
                self.history_max_scroll
                    .saturating_sub(self.scroll_from_bottom.min(self.history_max_scroll)),
            );
        }
        if amount > 0
            && self.history_visible < self.entries.len()
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
        self.unread_updates = 0;
        self.scroll_anchor = None;
        self.prepend_rows_before = None;
        self.scroll_from_bottom = 0;
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
        self.scrollbar_drag = None;
        self.scrollbar_pointer_y = None;
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

    pub fn select_start(&mut self, column: u16, row: u16) {
        if self.selection_area.contains((column, row).into()) {
            let point = (
                self.history_top_row + (row - self.selection_area.y) as usize,
                (column - self.selection_area.x) as usize,
            );
            self.selection_start = Some(point);
            self.selection_end = Some(point);
            self.selection_dragging = true;
            self.selection_edge = 0;
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

    fn history_document(
        &self,
        theme: Theme,
        width: usize,
    ) -> (Text<'static>, Vec<(String, String)>) {
        if self.home {
            return (Text::default(), Vec::new());
        }
        let mut lines = Vec::new();
        let mut markdown_targets = Vec::new();
        if self.entries.is_empty() && !self.running {
            lines.push(Line::default());
            lines.push(Line::from(Span::styled(
                self.preferences
                    .language
                    .text("   ◆  Ready for a task.", "   ◆  可以开始任务了。"),
                Style::default().fg(theme.muted),
            )));
            return (Text::from(lines), markdown_targets);
        }
        for entry in self.entries.iter().rev().take(self.history_visible).rev() {
            let role_style = |color| Style::default().fg(color).add_modifier(Modifier::BOLD);
            match entry.kind {
                Kind::User => {
                    lines.push(Line::from(vec![
                        Span::styled("  ▎", Style::default().fg(theme.user)),
                        Span::styled(
                            self.preferences.language.text("  You", "  你"),
                            role_style(theme.user),
                        ),
                    ]));
                    for line in entry.body.lines() {
                        lines.push(body_line(Line::from(Span::styled(
                            line.to_owned(),
                            Style::default().fg(theme.text),
                        ))));
                    }
                    lines.push(Line::default());
                }
                Kind::Assistant => {
                    lines.push(Line::from(vec![
                        Span::styled("  ◆", Style::default().fg(theme.accent)),
                        // The label shares the mark's accent: a turn is found by
                        // colour, not by reading a small bold word.
                        Span::styled("  Kroma", role_style(theme.accent)),
                    ]));
                    if !entry.body.is_empty() {
                        let (mut rendered, tags) = markdown::render_with_links(
                            &entry.body,
                            theme,
                            width.saturating_sub(5),
                        );
                        for tag in tags {
                            if let Some(span) = rendered
                                .get(tag.row)
                                .and_then(|row| row.spans.get(tag.span))
                            {
                                markdown_targets.push((span.content.to_string(), tag.target));
                            }
                        }
                        if self.running
                            && self.phase == "streaming"
                            && self.active_assistant == Some(self.entries.len() - 1)
                            && std::ptr::eq(entry, self.entries.last().unwrap())
                        {
                            if let Some(last) = rendered.last_mut() {
                                last.spans
                                    .push(Span::styled("▍", Style::default().fg(theme.accent)));
                            }
                        }
                        for line in rendered {
                            lines.push(body_line(line));
                        }
                    }
                    lines.push(Line::default());
                }
                Kind::Tool => {
                    let (name, status) = entry
                        .title
                        .split_once(" · ")
                        .unwrap_or((&entry.title, "done"));
                    let warning =
                        !entry.failed && super::tool_display::has_diagnostic(&entry.details);
                    let (marker, color) = if entry.failed {
                        ("✕", theme.error)
                    } else if warning {
                        ("!", theme.tool)
                    } else if status == "running" {
                        ("◌", theme.tool)
                    } else {
                        ("✓", theme.user)
                    };
                    // A tool is activity, not a separate article. In collapsed
                    // mode show only one purposeful line, without the same
                    // Ctrl+O instruction repeated for every call.
                    let display_name = compact_line(name, width.saturating_sub(13).min(24));
                    let summary_width = width;
                    let command_width = summary_width
                        .saturating_sub(5 + UnicodeWidthStr::width(display_name.as_str()) + 2);
                    let elapsed = entry
                        .duration
                        .map(|duration| format!(" · {:.1}s", duration.as_secs_f64()))
                        .unwrap_or_default();
                    let summary =
                        super::tool_display::summary(name, &entry.body, self.preferences.language);
                    let command =
                        compact_line(summary, command_width.saturating_sub(elapsed.len()));
                    lines.push(Line::from(vec![
                        Span::styled(format!("  {marker}  "), Style::default().fg(color)),
                        Span::styled(display_name, role_style(theme.tool)),
                        Span::styled(
                            format!("  {command}{elapsed}"),
                            Style::default().fg(theme.muted),
                        ),
                    ]));
                    if warning && !self.tools_expanded {
                        lines.push(body_line(Line::styled(
                            self.preferences.language.text(
                                "↳ Exit 0; diagnostic reported · Ctrl+O details",
                                "↳ 退出成功，输出含警告或错误提示 · Ctrl+O 查看详情",
                            ),
                            Style::default().fg(theme.tool),
                        )));
                    }
                    if entry.failed && !self.tools_expanded {
                        lines.push(body_line(Line::from(Span::styled(
                            format!(
                                "↳ {}",
                                compact_line(
                                    &failure_summary(&entry.details),
                                    width.saturating_sub(5)
                                )
                            ),
                            Style::default().fg(theme.error),
                        ))));
                    }
                    if status == "running" && !entry.details.is_empty() && !self.tools_expanded {
                        let preview = entry
                            .details
                            .lines()
                            .rev()
                            .find(|line| !line.trim().is_empty())
                            .unwrap_or("");
                        lines.push(body_line(Line::from(Span::styled(
                            format!("↳ {}", compact_line(preview, width.saturating_sub(5))),
                            Style::default().fg(theme.code),
                        ))));
                    }
                    if self.tools_expanded {
                        for command in entry.body.lines() {
                            lines.push(body_line(Line::styled(
                                format!("$ {command}"),
                                Style::default().fg(theme.code),
                            )));
                        }
                    }
                    if self.tools_expanded && !entry.details.is_empty() {
                        for output in entry.details.lines() {
                            lines.push(body_line(Line::from(vec![
                                Span::styled("│ ", Style::default().fg(theme.border)),
                                Span::styled(
                                    output.to_owned(),
                                    Style::default().fg(if entry.failed {
                                        theme.error
                                    } else {
                                        theme.code
                                    }),
                                ),
                            ])));
                        }
                    }
                    // Group neighboring tool calls; separate once the answer
                    // resumes, using spacing instead of an enclosing box.
                }
                Kind::Reasoning => {
                    if entry.body.trim().is_empty() {
                        continue;
                    }
                    lines.push(Line::from(Span::styled(
                        self.preferences
                            .language
                            .text("  ◇  Reasoning · Ctrl+R", "  ◇  推理摘要 · Ctrl+R"),
                        Style::default().fg(theme.muted),
                    )));
                    if self.reasoning_expanded {
                        let (rendered, tags) = markdown::render_with_links(
                            &entry.body,
                            theme,
                            width.saturating_sub(5),
                        );
                        for tag in tags {
                            if let Some(span) = rendered
                                .get(tag.row)
                                .and_then(|row| row.spans.get(tag.span))
                            {
                                markdown_targets.push((span.content.to_string(), tag.target));
                            }
                        }
                        for line in rendered {
                            lines.push(body_line(line));
                        }
                    }
                    // Give reasoning the same one-row separation every other block
                    // gets, so the collapsed line is not read as a header for the
                    // answer below it.
                    lines.push(Line::default());
                }
                Kind::Info => {
                    lines.push(Line::from(Span::styled(
                        format!("  ·  {}", self.info_title(&entry.title)),
                        role_style(theme.muted),
                    )));
                    for line in entry.body.lines() {
                        let localized;
                        let line = if entry.title == "COMMAND"
                            && self.preferences.language == super::preferences::Language::Zh
                        {
                            if let Some(id) = line.strip_prefix("Resumed session: ") {
                                localized = format!("已恢复会话：{id}");
                                &localized
                            } else {
                                line
                            }
                        } else {
                            line
                        };
                        let style = if line.starts_with('+') && !line.starts_with("+++") {
                            Style::default().fg(theme.user)
                        } else if line.starts_with('-') && !line.starts_with("---") {
                            Style::default().fg(theme.error)
                        } else if line.starts_with("@@") {
                            Style::default().fg(theme.accent)
                        } else {
                            Style::default().fg(theme.text)
                        };
                        lines.push(body_line(Line::from(Span::styled(line.to_owned(), style))));
                    }
                    lines.push(Line::default());
                }
            }
        }
        // Reserve one stable trailing transcript row for the live model phase
        // (or the final turn duration). Only the status glyphs animate: cached
        // markdown and selection/source-line identities never reflow per tick.
        if self.running || self.task_outcome.is_some() {
            lines.push(Line::default());
        }
        (Text::from(lines), markdown_targets)
    }

    pub fn draw(&mut self, frame: &mut Frame) {
        self.editor
            .localize_attachment_labels(self.preferences.language);
        let colors = self.themes.colors;
        let full_area = frame.area();
        if full_area.width < 20 || full_area.height < 8 {
            // Some SSH/PTY hosts initially report 0x0 before the first
            // terminal resize event. Do not draw cells outside that buffer.
            self.viewport_height = 0;
            self.selection_area = Rect::default();
            self.scrollbar_area = Rect::default();
            self.visible_links.clear();
            self.mouse_release();
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
        let area = reading_area(full_area, self.preferences.full_width);
        let editor_rows = self.editor.display_rows(area.width.saturating_sub(6));
        let composer_height = editor_rows.saturating_add(2).clamp(3, 10);
        // The model's activity follows the conversation itself instead of
        // living in a fixed bar away from its latest output. Only history
        // navigation needs a separate row above the editable composer.
        let show_navigation = self.scroll_anchor.is_some() && area.height >= 10;
        let layout = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(u16::from(show_navigation)),
            Constraint::Length(composer_height),
            Constraint::Length(1),
        ])
        .split(area);
        let header_line = Rect::new(layout[0].x, layout[0].y, layout[0].width, 1);
        let label = ellipsize_cells(
            self.session_label(),
            (header_line.width as usize).saturating_sub(12),
        );
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("◆ ", Style::default().fg(colors.accent)),
                Span::styled(
                    "Kroma",
                    Style::default()
                        .fg(colors.text)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!("  /  {label}"), Style::default().fg(colors.muted)),
            ])),
            Rect::new(header_line.x, header_line.y, header_line.width, 1),
        );
        let history_slot = Rect::new(
            layout[1].x.saturating_add(1),
            layout[1].y,
            layout[1].width.saturating_sub(2),
            layout[1].height,
        );
        if self.selection_area.width > 0 && self.selection_area.width != history_slot.width {
            // Selection offsets refer to *wrapped display cells*, not raw
            // grapheme indices. Reflow invalidates them. Clear the selection
            // rather than copy unrelated text after a terminal resize.
            self.selection_start = None;
            self.selection_end = None;
            self.mouse_release();
        }
        self.viewport_height = history_slot.height;
        let has_activity_tail = !self.home && (self.running || self.task_outcome.is_some());
        let cached = self.history_cache.as_ref().is_some_and(|cache| {
            cache.width == history_slot.width
                && cache.visible == self.history_visible
                && cache.tools_expanded == self.tools_expanded
                && cache.reasoning_expanded == self.reasoning_expanded
                && cache.has_activity_tail == has_activity_tail
                && cache.theme == colors
        });
        let reflow_anchor = self
            .history_cache
            .as_ref()
            .filter(|cache| cache.width != history_slot.width && self.scroll_anchor.is_some())
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
            let (raw, targets) = self.history_document(colors, usize::from(history_slot.width));
            let (text, source_lines) = wrap_document(raw, usize::from(history_slot.width));
            let link_regions = markdown_link_regions(&text, &targets);
            self.history_cache = Some(HistoryCache {
                width: history_slot.width,
                visible: self.history_visible,
                tools_expanded: self.tools_expanded,
                reasoning_expanded: self.reasoning_expanded,
                has_activity_tail,
                theme: colors,
                text,
                source_lines,
                links: link_regions,
            });
        }
        let contents = &self.history_cache.as_ref().unwrap().text;
        let rows = contents.lines.len();
        let max_scroll = rows.saturating_sub(history_slot.height as usize);
        self.history_max_scroll = max_scroll;
        // For short conversations, bring the newest response near the input.
        // Empty space is left *above* the dialogue, not between it and the
        // composer. Keep the full viewport for welcome art and long history.
        let history = if !self.home && rows > 0 && rows < history_slot.height as usize {
            Rect::new(
                history_slot.x,
                history_slot.bottom().saturating_sub(rows as u16),
                history_slot.width,
                rows as u16,
            )
        } else {
            history_slot
        };
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
            self.scroll_anchor = Some(anchored);
            self.scroll_from_bottom = max_scroll.saturating_sub(anchored);
        } else {
            self.prepend_rows_before = None;
            self.scroll_from_bottom = self.scroll_from_bottom.min(max_scroll);
        }
        let start_row = max_scroll.saturating_sub(self.scroll_from_bottom);
        self.history_top_row = start_row;
        self.selection_area = history;
        self.scrollbar_area =
            Rect::new(history_slot.right(), history_slot.y, 1, history_slot.height);
        let end_row = start_row.saturating_add(history.height as usize).min(rows);
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
        let mut regions = self
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
        for (row, line) in self.selection_rows.iter().enumerate() {
            for link in links::detect_visible(row, line) {
                if !regions.iter().any(|existing| {
                    existing.row == row && link.start < existing.end && link.end > existing.start
                }) {
                    regions.push(link);
                }
            }
        }
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
        for (row, line) in viewport_lines.iter().enumerate() {
            buffer.set_line(history.x, history.y + row as u16, line, history.width);
        }
        // The activity belongs to the most recent message, not a floating
        // header/composer bar. Only paint the last (reserved) transcript row.
        // Timer ticks never rebuild wrapped Markdown or move scroll anchors.
        if has_activity_tail && rows > 0 {
            let tail = rows - 1;
            if (start_row..end_row).contains(&tail) {
                frame.render_widget(
                    Paragraph::new(self.activity_line(colors)),
                    Rect::new(
                        history.x.saturating_add(2),
                        history.y + (tail - start_row) as u16,
                        history.width.saturating_sub(4),
                        1,
                    ),
                );
            }
        }
        if self.home {
            super::logo::draw(
                frame,
                history_slot,
                colors,
                self.themes.preset(),
                self.preferences.language,
            );
        }
        for link in &self.visible_links {
            let hovered = self
                .hover_pointer
                .is_some_and(|(x, y)| y == link.y && x >= link.x && x < link.end_x);
            for x in link.x..link.end_x {
                if let Some(cell) = frame.buffer_mut().cell_mut((x, link.y)) {
                    let style = if hovered {
                        Style::default()
                            .fg(colors.accent)
                            .add_modifier(Modifier::UNDERLINED | Modifier::BOLD)
                    } else {
                        Style::default().add_modifier(Modifier::UNDERLINED)
                    };
                    cell.set_style(style);
                }
            }
        }
        if let (Some(mut start), Some(mut end)) = (self.selection_start, self.selection_end) {
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
            frame.render_widget(
                Paragraph::new(Line::styled(
                    self.navigation_status(),
                    Style::default().fg(colors.accent),
                )),
                Rect::new(
                    layout[2].x.saturating_add(2),
                    layout[2].y,
                    layout[2].width.saturating_sub(4),
                    1,
                ),
            );
        }
        // One quiet input surface: a divider, a colored prompt marker and a
        // multiline editor. No nested rectangular conversation/editor boxes.
        let composer = layout[3];
        frame.render_widget(
            Block::default()
                .style(Style::default().bg(colors.input_surface()))
                .borders(Borders::TOP)
                .border_style(Style::default().fg(colors.border)),
            composer,
        );
        let editor_inner = Rect::new(
            composer.x.saturating_add(4),
            composer.y.saturating_add(1),
            composer.width.saturating_sub(6),
            editor_rows.min(composer.height.saturating_sub(1)),
        );
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
        } else if self.editor.text().is_empty() {
            let hint = if self.running {
                self.preferences.language.text(
                    "Steer this task, or /queue another…",
                    "输入引导指令，或用 /queue 排队…",
                )
            } else {
                self.preferences.language.text(
                    "Ask anything, or describe a coding task…",
                    "输入问题或编程任务，输入 / 查看命令…",
                )
            };
            Paragraph::new(hint).style(Style::default().fg(colors.muted))
        } else {
            Paragraph::new(text).style(Style::default().fg(colors.text))
        };
        frame.render_widget(
            Paragraph::new("›").style(
                Style::default()
                    .fg(colors.accent)
                    .add_modifier(Modifier::BOLD),
            ),
            Rect::new(
                composer.x.saturating_add(2),
                composer.y.saturating_add(1),
                1,
                1,
            ),
        );
        frame.render_widget(input, editor_inner);
        if self.show_editor_cursor() && editor_inner.width > 0 && editor_inner.height > 0 {
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
            frame.set_cursor_position((
                editor_inner
                    .x
                    .saturating_add(cx.min(editor_inner.width - 1)),
                editor_inner
                    .y
                    .saturating_add(cy.min(editor_inner.height - 1)),
            ));
        }
        let status_area = Rect::new(
            layout[4].x.saturating_add(2),
            layout[4].y,
            layout[4].width.saturating_sub(4),
            1,
        );
        let hovered = self.hover_pointer.and_then(|(x, y)| self.link_at(x, y));
        let footer = if !self.notification.is_empty() {
            Line::from(Span::styled(
                excerpt(&self.notification, usize::from(status_area.width)),
                Style::default().fg(colors.accent),
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
        if let Some(picker) = &self.picker {
            picker.draw(frame, history_slot, colors, self.preferences.language);
        }
    }
}

impl Drop for UiState {
    fn drop(&mut self) {
        self.discard_draft();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};

    #[test]
    fn live_reasoning_renders_before_text_and_reconciles_without_duplicates() {
        let mut state = UiState::new(
            "test".into(),
            "demo".into(),
            ThemeStore::load_at(None).unwrap(),
            &[],
        );
        assert!(!state.reasoning_expanded);
        state.event(UiEvent::TaskStarted);
        state.event(UiEvent::ModelStarted);
        state.event(UiEvent::ReasoningDelta("Thinking UNIQUE_SENTINEL ".into()));
        assert_eq!(state.phase, "thinking");
        let mut terminal = Terminal::new(TestBackend::new(95, 28)).unwrap();
        terminal.draw(|frame| state.draw(frame)).unwrap();
        let visible = state.selection_rows.join("\n");
        assert!(visible.contains("Reasoning"));
        assert!(!visible.contains("UNIQUE_SENTINEL"));
        state.reasoning_expanded = true;
        terminal.draw(|frame| state.draw(frame)).unwrap();
        assert!(state.selection_rows.join("\n").contains("UNIQUE_SENTINEL"));

        state.event(UiEvent::ReasoningDelta("step two".into()));
        state.event(UiEvent::Text("Answer".into()));
        state.event(UiEvent::ReasoningFinal("Thinking step two".into()));
        assert_eq!(state.entries.len(), 2);
        assert!(matches!(state.entries[0].kind, Kind::Reasoning));
        assert_eq!(state.entries[0].body, "Thinking step two");
        assert!(matches!(state.entries[1].kind, Kind::Assistant));
        assert_eq!(state.entries[1].body, "Answer");

        state.event(UiEvent::ToolStarted {
            name: "shell".into(),
            id: "call".into(),
            args: "pwd".into(),
        });
        state.event(UiEvent::ModelStarted);
        state.event(UiEvent::ReasoningDelta("Next turn".into()));
        assert_eq!(state.entries.len(), 4);
        assert_eq!(state.entries[3].body, "Next turn");
    }

    #[test]
    fn branded_activity_animates_without_reflow_or_disabling_steering() {
        let mut ui = UiState::new(
            "deepseek".into(),
            "test".into(),
            ThemeStore::load_at(None).unwrap(),
            &[],
        );
        ui.event(UiEvent::TaskStarted);
        ui.event(UiEvent::ModelStarted);
        let mut terminal = Terminal::new(TestBackend::new(90, 26)).unwrap();
        terminal.draw(|frame| ui.draw(frame)).unwrap();
        assert!(ui.activity_status().starts_with("Connecting"));
        assert!(
            !ui.show_editor_cursor(),
            "empty composer must not blink while running"
        );
        let cached = ui.history_cache.as_ref().unwrap().text.lines.as_ptr();
        let first = ui.activity_line(ui.themes.colors);
        for _ in 0..2 {
            ui.last_activity_frame = Instant::now() - Duration::from_millis(200);
            assert!(
                ui.tick_activity(),
                "animation must continue without model data"
            );
        }
        let second = ui.activity_line(ui.themes.colors);
        assert_ne!(first.spans[0].style.fg, second.spans[0].style.fg);
        assert_ne!(first.spans[1].style.fg, second.spans[1].style.fg);
        terminal.draw(|frame| ui.draw(frame)).unwrap();
        assert_eq!(
            cached,
            ui.history_cache.as_ref().unwrap().text.lines.as_ptr(),
            "animation must not rebuild the transcript"
        );
        assert!(!ui.selection_rows.join("\n").contains("Waiting for model"));
        let rows = terminal
            .backend()
            .buffer()
            .content()
            .chunks(90)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>();
        let status_row = rows
            .iter()
            .position(|row| row.contains("Connecting"))
            .unwrap();
        let editor_row = rows
            .iter()
            .position(|row| row.contains("Steer this task"))
            .unwrap();
        assert_eq!(
            status_row + 2,
            editor_row,
            "activity must sit directly above composer"
        );
        assert!(
            !rows[0].contains("Connecting"),
            "no distant header duplicate"
        );
        assert_eq!(
            ui.selection_area.bottom() as usize - 1,
            status_row,
            "activity belongs to the reserved trailing transcript line"
        );
        assert!(
            ui.history_cache
                .as_ref()
                .unwrap()
                .text
                .lines
                .last()
                .unwrap()
                .spans
                .is_empty(),
            "animation must never be written into persistent transcript lines"
        );

        ui.event(UiEvent::ReasoningDelta("step 1".into()));
        terminal.draw(|frame| ui.draw(frame)).unwrap();
        assert!(ui.selection_rows.join("\n").contains("Reasoning"));
        ui.phase_since = Some(Instant::now() - Duration::from_secs(9));
        assert!(ui.activity_status().contains("Thinking · 9s"));
        ui.editor.paste("steer: check tests");
        assert!(
            ui.show_editor_cursor(),
            "typing while running restores the caret"
        );

        ui.event(UiEvent::TaskFinished);
        assert!(!ui.tick_activity());
        assert_eq!(ui.activity_status(), "Ready");
        assert!(ui.show_editor_cursor());
    }

    #[test]
    fn short_chat_hugs_composer_and_live_status_follows_the_latest_message() {
        let messages = vec![
            Message::new("user", "hello"),
            Message::new("assistant", "The answer is visible"),
        ];
        let mut ui = UiState::new(
            "deepseek".into(),
            "test".into(),
            ThemeStore::load_at(None).unwrap(),
            &messages,
        );
        let rows = rendered_text(&mut ui, 100, 36);
        let header = rows.iter().position(|row| row.contains("Kroma")).unwrap();
        let answer = rows
            .iter()
            .position(|row| row.contains("The answer is visible"))
            .unwrap();
        let editor = rows
            .iter()
            .position(|row| row.contains("Ask anything"))
            .unwrap();
        assert_eq!(header, 0);
        assert!(answer > 20, "short transcript belongs near the composer");
        assert!(
            editor - answer <= 5,
            "do not leave a tall empty void between latest reply and input"
        );
        assert_eq!(
            ui.history_top_row, 0,
            "logical history coordinates stay unchanged"
        );

        ui.add_user("run tests");
        ui.event(UiEvent::ModelStarted);
        let rows = rendered_text(&mut ui, 100, 36);
        let status = rows
            .iter()
            .position(|row| row.contains("Connecting"))
            .unwrap();
        let latest_user = rows
            .iter()
            .position(|row| row.contains("run tests"))
            .unwrap();
        let editor = rows
            .iter()
            .position(|row| row.contains("Steer this task"))
            .unwrap();
        assert!(latest_user < status && status < editor);
        assert!(
            editor - status <= 3,
            "live activity belongs to latest message"
        );
        assert!(
            !rows[0].contains("Connecting"),
            "no detached header activity"
        );

        ui.event(UiEvent::ToolStarted {
            name: "shell".into(),
            id: "tool1".into(),
            args: "cargo test".into(),
        });
        let rows = rendered_text(&mut ui, 100, 36);
        let tool_status = rows
            .iter()
            .position(|row| row.contains("Running tool"))
            .unwrap();
        let editor = rows
            .iter()
            .position(|row| row.contains("Steer this task"))
            .unwrap();
        assert!(editor - tool_status <= 3);
        let cache = ui.history_cache.as_ref().unwrap().text.lines.as_ptr();
        ui.last_activity_frame = Instant::now() - Duration::from_millis(200);
        assert!(ui.tick_activity());
        rendered_text(&mut ui, 100, 36);
        assert_eq!(
            cache,
            ui.history_cache.as_ref().unwrap().text.lines.as_ptr(),
            "animation cannot rewrap transcript on every frame"
        );
        ui.finish_task();
        ui.task_outcome = Some(TaskOutcome::Completed);
        let rows = rendered_text(&mut ui, 100, 36);
        let completed = rows
            .iter()
            .position(|row| row.contains("Completed"))
            .unwrap();
        assert!(rows[completed].contains('✓'));
        assert!(
            completed
                < rows
                    .iter()
                    .position(|row| row.contains("Ask anything"))
                    .unwrap()
        );
    }

    #[test]
    fn activity_stays_with_newest_message_when_reader_is_browsing_history() {
        let messages = (0..75)
            .map(|index| Message::new("assistant", format!("historical result {index}")))
            .collect::<Vec<_>>();
        let mut ui = UiState::new(
            "deepseek".into(),
            "test".into(),
            ThemeStore::load_at(None).unwrap(),
            &messages,
        );
        rendered_text(&mut ui, 80, 23);
        ui.scroll(8);
        let anchor = ui.scroll_anchor;
        ui.event(UiEvent::TaskStarted);
        ui.event(UiEvent::ModelStarted);
        let rows = rendered_text(&mut ui, 80, 23);
        assert_eq!(ui.scroll_anchor, anchor);
        assert!(rows.iter().any(|row| row.contains("Browsing history")));
        assert!(
            !rows.iter().any(|row| row.contains("Connecting")),
            "the status is not a floating bar above the composer while browsing"
        );
        ui.scroll_to_bottom();
        let rows = rendered_text(&mut ui, 80, 23);
        assert!(rows.iter().any(|row| row.contains("Connecting")));
        assert!(!rows.iter().any(|row| row.contains("Browsing history")));
    }

    #[test]
    fn short_bottom_aligned_history_keeps_links_clickable() {
        let messages = vec![
            Message::new("user", "show link"),
            Message::new(
                "assistant",
                "See https://example.com/issues/123 for details",
            ),
        ];
        let mut ui = UiState::new(
            "mock".into(),
            "test".into(),
            ThemeStore::load_at(None).unwrap(),
            &messages,
        );
        rendered_text(&mut ui, 120, 38);
        assert!(ui.selection_area.y > 10, "short transcript moves down");
        let link = ui.visible_links.first().expect("link must remain visible");
        assert_eq!(
            ui.link_at(link.x, link.y),
            Some("https://example.com/issues/123")
        );
    }

    #[test]
    fn footer_distinguishes_context_from_api_tokens_and_cached_ratio() {
        let mut ui = UiState::new(
            "deepseek".into(),
            "test".into(),
            ThemeStore::load_at(None).unwrap(),
            &[],
        );
        ui.configure_status(1_000_000, Some("max".into()));
        ui.event(UiEvent::Context { estimated: 1_600 });
        assert_eq!(ui.context_display(), "Ctx ~1.6k / 1M · ~0.2%");
        ui.event(UiEvent::Usage {
            input: Some(8_000),
            output: Some(500),
            cached: Some(6_000),
            reasoning: Some(200),
        });
        let mut terminal = Terminal::new(TestBackend::new(130, 26)).unwrap();
        terminal.draw(|frame| ui.draw(frame)).unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("Context ~0.2%"));
        assert!(rendered.contains("In/out 8.0k / 500 · Cache 75%"));
        let mut narrow = Terminal::new(TestBackend::new(40, 26)).unwrap();
        narrow.draw(|frame| ui.draw(frame)).unwrap();
        let narrow_render = narrow
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(
            narrow_render.contains("Cache") && narrow_render.contains("75%"),
            "cache read and rate must remain visible on narrow terminals"
        );
    }

    #[test]
    fn background_title_and_apec_usage_cannot_replace_coding_cache_or_context() {
        let mut ui = UiState::new(
            "deepseek".into(),
            "test".into(),
            ThemeStore::load_at(None).unwrap(),
            &[],
        );
        ui.configure_status(1_000_000, Some("high".into()));
        ui.event(UiEvent::Usage {
            input: Some(1_000),
            output: Some(80),
            cached: Some(800),
            reasoning: Some(20),
        });
        ui.event(UiEvent::ActualInput(1_000));
        let coding_footer = ui.usage.compact_status(150);
        let context = ui.context_display();
        ui.event(UiEvent::TitleUsage {
            requests: 1,
            input: Some(117),
            output: Some(95),
            cached: Some(0),
            reasoning: Some(80),
        });
        ui.event(UiEvent::ApecUsage {
            requests: 2,
            input: Some(500),
            output: Some(100),
            cached: Some(200),
        });
        assert_eq!(ui.usage.compact_status(150), coding_footer);
        assert_eq!(ui.context_display(), context);
        assert_eq!(ui.usage.calls, 1);
        assert_eq!(ui.title_usage.calls, 1);
        assert_eq!(ui.apec_usage.calls, 2);
        let report = ui.usage.report_breakdown(&ui.title_usage, &ui.apec_usage);
        assert!(report.contains("Coding: 1 call(s) | In 1000"));
        assert!(report.contains("Auto titles: 1 call(s) | In 117"));
        assert!(report.contains("APEC summaries: 2 call(s) | In 500"));
        assert!(report.contains("Combined: 4 call(s) | In 1617 | Out 275 | Cache read 1000"));
        let mut terminal = Terminal::new(TestBackend::new(120, 26)).unwrap();
        terminal.draw(|frame| ui.draw(frame)).unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("In/out 1.0k / 80 · Cache 80%"));
        assert!(!rendered.contains("Out 95") && !rendered.contains("Total In"));
    }

    #[test]
    fn home_draws_original_brand_mark_and_clears_on_first_send() {
        let previous = [Message::new("user", "Previous conversation")];
        let mut state = UiState::new(
            "deepseek".into(),
            "default".into(),
            ThemeStore::load_at(None).unwrap(),
            &previous,
        );
        state.enter_home();
        assert!(state.is_home());
        assert_eq!(state.session_label(), "new conversation");
        let mut terminal = Terminal::new(TestBackend::new(110, 36)).unwrap();
        terminal.draw(|frame| state.draw(frame)).unwrap();
        let contents = terminal.backend().buffer().content();
        let text = contents
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains("K R O M A"));
        assert!(text.contains("/resume for history"));
        assert!(!text.contains("Previous conversation"));
        assert!(contents
            .iter()
            .any(|cell| cell.fg == ratatui::style::Color::Rgb(91, 124, 255)));
        assert!(contents
            .iter()
            .any(|cell| cell.fg == ratatui::style::Color::Rgb(255, 111, 145)));
        state.add_user("Fix the tests");
        assert!(!state.is_home());
        terminal.draw(|frame| state.draw(frame)).unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(!text.contains("/resume for history"));
        assert!(text.contains("Fix the tests"));
    }

    #[test]
    fn failure_is_explained_without_expanding_tools_and_command_is_one_line() {
        let mut state = UiState::new(
            "model".into(),
            "default".into(),
            ThemeStore::load_at(None).unwrap(),
            &[],
        );
        state.event(UiEvent::ToolStarted {
            name: "shell".into(),
            id: "build".into(),
            args: "cargo test --offline --locked ".repeat(15),
        });
        state.event(UiEvent::ToolFinished {
            id: "build".into(),
            result: "exit: 101\nstderr: error[E0599]: method not found\nlong output".into(),
            failed: true,
        });
        let lines = state.history_text(state.themes.colors, 55).lines;
        let rows = lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();
        assert_eq!(
            rows.iter()
                .filter(|line| line.contains("Run Rust tests"))
                .count(),
            1
        );
        assert!(rows
            .iter()
            .any(|line| line.contains("exit 101") && line.contains("E0599")));
        assert!(!rows.join("\n").contains("long output"));
        for row in &rows {
            assert!(
                UnicodeWidthStr::width(row.as_str()) <= 55,
                "collapsed tool row must fit in the reading column: {row:?}"
            );
        }
        state.tools_expanded = true;
        let expanded = state.history_text(state.themes.colors, 55).lines;
        assert!(expanded
            .iter()
            .flat_map(|line| line.spans.iter())
            .any(|span| span.content.contains("long output")));
    }

    #[test]
    fn failures_remain_visible_after_reloading_durable_tool_records() {
        let mut assistant = Message::new("assistant", "");
        assistant.tool_calls = Some(vec![crate::protocol::ToolCall {
            id: "call-1".into(),
            kind: "function".into(),
            function: crate::protocol::FunctionCall {
                name: "shell".into(),
                arguments: r#"{"command":"cargo test"}"#.into(),
            },
        }]);
        let result = Message::tool_with_status(
            "call-1".into(),
            r#"{"exit_code":101,"stdout":"","stderr":"error[E0599]: broken API"}"#.into(),
            true,
        );
        let state = UiState::new(
            "model".into(),
            "default".into(),
            ThemeStore::load_at(None).unwrap(),
            &[assistant, result],
        );
        let lines = state.history_text(state.themes.colors, 80).lines;
        assert!(
            lines
                .iter()
                .flat_map(|line| line.spans.iter())
                .any(|span| span.content.contains("E0599")),
            "restored failure must not be a bare red cross"
        );
    }

    #[test]
    fn widescreen_centres_the_reading_column_instead_of_gluing_it_left() {
        let mut state = UiState::new(
            "model".into(),
            "default".into(),
            ThemeStore::load_at(None).unwrap(),
            &[Message::new(
                "assistant",
                "| Column | Result |\n|---|---|\n| cargo test | passed |",
            )],
        );
        let mut terminal = Terminal::new(TestBackend::new(210, 35)).unwrap();
        terminal.draw(|frame| state.draw(frame)).unwrap();
        let area = reading_area(Rect::new(0, 0, 210, 35), false);
        assert!(
            area.width >= 150,
            "a maximized terminal still uses a wide column, got {}",
            area.width
        );
        let left = i32::from(area.x);
        let right = 210 - i32::from(area.x) - i32::from(area.width);
        assert!(
            (left - right).abs() <= 1,
            "margins must be symmetric: left {left}, right {right}"
        );
        assert!(state.selection_area.width > 100);
        assert!(state.selection_rows.join("\n").contains("cargo test"));
        let small = reading_area(Rect::new(0, 0, 40, 20), false);
        assert_eq!(small.width, 40);
        assert_eq!(small.x, 0);
        // `/layout wide` still fills the terminal, edge to edge.
        let wide = reading_area(Rect::new(0, 0, 210, 35), true);
        assert_eq!(wide.width, 206);
        assert_eq!(wide.x, 2);
    }

    #[test]
    fn markdown_and_plain_links_hit_the_correct_screen_cells() {
        let answer = "Reports: [审查报告 SR-001](https://example.org/report) and [复现测试](./reviews/tests.md).\n\nFile src/provider.rs:128 and https://rust-lang.org.";
        let mut state = UiState::new(
            "model".into(),
            "default".into(),
            ThemeStore::load_at(None).unwrap(),
            &[Message::new("assistant", answer)],
        );
        let mut terminal = Terminal::new(TestBackend::new(130, 32)).unwrap();
        terminal.draw(|frame| state.draw(frame)).unwrap();
        for (label, target) in [
            ("审查报告 SR-001", "https://example.org/report"),
            ("复现测试", "./reviews/tests.md"),
            ("src/provider.rs:128", "src/provider.rs:128"),
            ("https://rust-lang.org", "https://rust-lang.org"),
        ] {
            let row = state
                .selection_rows
                .iter()
                .position(|row| row.contains(label))
                .expect("link label visible");
            let prefix = state.selection_rows[row].split_once(label).unwrap().0;
            let x = state.selection_area.x + UnicodeWidthStr::width(prefix) as u16;
            let y = state.selection_area.y + row as u16;
            assert_eq!(state.link_at(x, y), Some(target), "wrong link for {label}");
            state.hover_link(x, y);
            terminal.draw(|frame| state.draw(frame)).unwrap();
            let cells = terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>();
            assert!(cells.contains("Ctrl+Click to open"));
        }
        assert!(state
            .link_at(state.selection_area.x, state.selection_area.y)
            .is_none());
    }

    #[test]
    fn clickable_regions_follow_scrolling_without_reusing_stale_coordinates() {
        let mut history = (0..60)
            .map(|i| Message::new("assistant", format!("History item {i}")))
            .collect::<Vec<_>>();
        history.push(Message::new(
            "assistant",
            "[latest report](https://example.org/latest)",
        ));
        let mut state = UiState::new(
            "model".into(),
            "default".into(),
            ThemeStore::load_at(None).unwrap(),
            &history,
        );
        let mut terminal = Terminal::new(TestBackend::new(88, 22)).unwrap();
        terminal.draw(|frame| state.draw(frame)).unwrap();
        let (x, y) = state.test_link_position("example.org/latest").unwrap();
        assert_eq!(state.link_at(x, y), Some("https://example.org/latest"));
        state.scroll(15);
        terminal.draw(|frame| state.draw(frame)).unwrap();
        assert_ne!(state.link_at(x, y), Some("https://example.org/latest"));
        state.scroll_to_bottom();
        terminal.draw(|frame| state.draw(frame)).unwrap();
        assert_eq!(state.link_at(x, y), Some("https://example.org/latest"));
    }

    #[test]
    fn long_literal_url_reconstructs_full_destination_after_wrapping() {
        let url = format!("https://example.org/{}", "very-long-segment".repeat(9));
        let mut state = UiState::new(
            "model".into(),
            "default".into(),
            ThemeStore::load_at(None).unwrap(),
            &[Message::new(
                "assistant",
                format!("Check {url} for details"),
            )],
        );
        let mut terminal = Terminal::new(TestBackend::new(44, 25)).unwrap();
        terminal.draw(|frame| state.draw(frame)).unwrap();
        let partial = state
            .visible_links
            .iter()
            .find(|link| link.target.starts_with("https://example.org/"))
            .expect("start of wrapped URL should be clickable");
        assert!(partial.target.len() < url.len());
        assert_eq!(state.resolved_link(&partial.target), url);
    }

    #[test]
    fn long_named_markdown_link_remains_clickable_across_wrapped_rows() {
        let label = "明细报告测试报告测试报告测试报告";
        let text = format!("Please review [{label}](https://example.org/full-report) now.");
        let mut state = UiState::new(
            "model".into(),
            "default".into(),
            ThemeStore::load_at(None).unwrap(),
            &[Message::new("assistant", text)],
        );
        let mut terminal = Terminal::new(TestBackend::new(34, 24)).unwrap();
        terminal.draw(|frame| state.draw(frame)).unwrap();
        let hits = state
            .visible_links
            .iter()
            .filter(|link| link.target == "https://example.org/full-report")
            .collect::<Vec<_>>();
        assert!(
            hits.len() >= 2,
            "wrapped Markdown link must have click targets on both rows"
        );
        for hit in hits {
            assert_eq!(
                state.link_at(hit.x, hit.y),
                Some("https://example.org/full-report")
            );
        }
    }

    #[test]
    fn searchable_picker_renders_over_history_without_obscuring_editor() {
        let mut state = UiState::new(
            "deepseek".into(),
            "default".into(),
            ThemeStore::load_at(None).unwrap(),
            &[],
        );
        state.editor.paste("draft remains here");
        state.picker = Some(Picker::new(
            super::super::picker::Kind::Commands,
            vec![
                super::super::picker::Item::new("/review", "/review", "command"),
                super::super::picker::Item::new("/resume", "/resume", "command"),
            ],
            "rev",
        ));
        for (width, height) in [(40, 16), (80, 24), (160, 40)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|frame| state.draw(frame)).unwrap();
            let buffer = terminal.backend().buffer();
            let lines = buffer
                .content()
                .chunks(width as usize)
                .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
                .collect::<Vec<_>>();
            let snapshot = lines.join("\n");
            assert!(
                snapshot.contains("Commands"),
                "picker disappeared at {width}: {snapshot}"
            );
            assert!(
                snapshot.contains("/review"),
                "active item missing at {width}"
            );
            assert!(
                snapshot.contains("draft remains here"),
                "composer obscured at {width}"
            );
        }
    }

    #[test]
    fn status_keeps_model_thinking_and_context_visible_at_eighty_columns() {
        for (width, height) in [(40, 16), (80, 24), (110, 30), (160, 40)] {
            let mut state = UiState::new(
                "deepseek-v4.1-flash".into(),
                "default".into(),
                ThemeStore::load_at(None).unwrap(),
                &[],
            );
            state.configure_status(128_000, Some("high".into()));
            state.event(UiEvent::ActualInput(32_000));
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|frame| state.draw(frame)).unwrap();
            let output = terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>();
            assert!(
                output.contains("Kroma"),
                "no app identity at {width} columns"
            );
            let expected_context = "Context 25%";
            assert!(
                output.contains(expected_context),
                "no context at {width} columns"
            );
            assert!(
                output.contains("high"),
                "no thinking level at {width} columns"
            );
            if width >= 80 {
                assert!(
                    output.contains("deepseek-v4.1-flash"),
                    "model must remain visible at {width} columns"
                );
            }
        }
    }

    #[test]
    fn notification_dismisses_without_losing_important_transcript_events() {
        let mut state = UiState::new(
            "model".into(),
            "default".into(),
            ThemeStore::load_at(None).unwrap(),
            &[],
        );
        state.note("Temporary theme feedback");
        state.info("ERROR", "Last request failed with an HTTP error");
        state.dismiss_notification();
        assert!(state.notification.is_empty());
        assert!(state
            .history_text(state.themes.colors, 80)
            .lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .any(|span| span.content.contains("HTTP error")));
    }

    #[test]
    fn final_only_reasoning_inserts_before_streamed_answer() {
        let mut state = UiState::new(
            "test".into(),
            "demo".into(),
            ThemeStore::load_at(None).unwrap(),
            &[],
        );
        state.event(UiEvent::ModelStarted);
        state.event(UiEvent::Text("answer".into()));
        state.event(UiEvent::ReasoningFinal("completed reasoning".into()));
        assert_eq!(state.entries.len(), 2);
        assert!(matches!(state.entries[0].kind, Kind::Reasoning));
        assert_eq!(state.entries[0].body, "completed reasoning");
        assert_eq!(state.entries[1].body, "answer");
    }

    #[test]
    fn mobile_review_transcript_keeps_markdown_hierarchy_visible() {
        let answer = "## 规模与结构\n\n- 17,501 行 Rust，生产代码与测试代码分层清晰。\n- 主函数 `run_with_resume` 后续仍然可以继续优化。\n  - 会话使用 JSON 快照和 WAL。\n\n## 打分\n\n| 维度 | 分数 |\n|---|---|\n| 架构设计 | 9.0 |\n| 测试质量 | 8.5 |\n\n结论：这是一份需要清晰排版的审查报告。";
        let mut state = UiState::new(
            "mock".into(),
            "review".into(),
            ThemeStore::load_at(None).unwrap(),
            &[],
        );
        state.event(UiEvent::Text(answer.into()));
        let mut terminal = Terminal::new(TestBackend::new(70, 38)).unwrap();
        terminal.draw(|frame| state.draw(frame)).unwrap();
        // Ratatui's terminal buffer includes blank continuation cells after
        // double-width CJK characters. The cached visible rows preserve the
        // original text without inserting those artificial spaces.
        let rows = &state.selection_rows;
        let section = rows
            .iter()
            .position(|line| line.contains("规模与结构"))
            .unwrap();
        assert!(
            rows[section + 1].trim().is_empty(),
            "heading should be separated from content"
        );
        assert!(
            rows.iter().any(|line| line.contains("run_with_resume")),
            "code identifier must remain readable"
        );
        assert!(
            rows.iter()
                .any(|line| line.contains("架构设计") && line.contains("9.0")),
            "score cells must share a row"
        );
        assert!(rows.iter().any(|line| line.contains("结论")));
    }

    #[test]
    fn wrapping_keeps_short_english_identifiers_intact_under_bullets() {
        let text = Text::from(vec![Line::from(vec![
            Span::raw("   "),
            Span::raw("• "),
            Span::raw("inspect run_with_resume now"),
        ])]);
        let wrapped = wrap_history(text, 25).0;
        let rows = wrapped
            .lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();
        assert!(rows.len() >= 2, "the sample must wrap: {rows:?}");
        assert!(
            !rows[0].contains("run_"),
            "do not split the identifier: {rows:?}"
        );
        assert!(
            rows[1].contains("run_with_resume"),
            "keep the word together: {rows:?}"
        );
        assert!(
            rows[1].starts_with("     "),
            "align under bullet content: {rows:?}"
        );
    }

    #[test]
    fn history_wrapping_keeps_zwj_emoji_as_one_visible_character() {
        let line = Line::raw(format!("{}👩‍💻z", "a".repeat(6)));
        let wrapped = wrap_history(Text::from(vec![line]), 8).0;
        let rendered = wrapped
            .lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();
        assert_eq!(rendered[0], "aaaaaa👩‍💻");
        assert_eq!(rendered[1], "z");
    }

    #[test]
    fn history_render_cache_is_reused_until_content_changes() {
        let messages = vec![Message::new("user", "A stable history entry")];
        let mut state = UiState::new(
            "mock".into(),
            "test".into(),
            ThemeStore::load_at(None).unwrap(),
            &messages,
        );
        let mut terminal = Terminal::new(TestBackend::new(92, 25)).unwrap();
        terminal.draw(|frame| state.draw(frame)).unwrap();
        let first = state.history_cache.as_ref().unwrap().text.lines.as_ptr();
        terminal.draw(|frame| state.draw(frame)).unwrap();
        assert_eq!(
            first,
            state.history_cache.as_ref().unwrap().text.lines.as_ptr()
        );
        state.event(UiEvent::Text("Changed".into()));
        assert!(state.history_cache.is_none());
        terminal.draw(|frame| state.draw(frame)).unwrap();
        let rendered = state
            .history_cache
            .as_ref()
            .unwrap()
            .text
            .lines
            .iter()
            .flat_map(|line| line.spans.iter().map(|span| span.content.as_ref()))
            .collect::<String>();
        assert!(rendered.contains("Changed"));
    }

    #[test]
    fn mouse_selection_copies_exact_visible_transcript_cells() {
        let history = vec![Message::new("user", "inspect hello world")];
        let mut state = UiState::new(
            "mock".into(),
            "test".into(),
            ThemeStore::load_at(None).unwrap(),
            &history,
        );
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal.draw(|frame| state.draw(frame)).unwrap();
        let area = state.selection_area;
        let position = state
            .selection_rows
            .iter()
            .position(|line| line.contains("inspect hello world"))
            .unwrap();
        let row = area.y + position as u16;
        let line = &state.selection_rows[position];
        let start = line.find("hello").unwrap() as u16;
        state.select_start(area.x + start, row);
        state.select_drag(area.x + start + 4, row);
        assert_eq!(state.selected_text().as_deref(), Some("hello"));
        terminal.draw(|frame| state.draw(frame)).unwrap();
        assert_eq!(state.selected_text().as_deref(), Some("hello"));
    }

    #[test]
    fn scrollbar_thumb_can_be_dragged_between_earliest_and_latest_history() {
        let messages = (0..120)
            .map(|i| Message::new("assistant", format!("Log line {i:03}")))
            .collect::<Vec<_>>();
        let mut state = UiState::new(
            "mock".into(),
            "test".into(),
            ThemeStore::load_at(None).unwrap(),
            &messages,
        );
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| state.draw(frame)).unwrap();
        assert!(state.history_max_scroll > 0);
        let track = state.scrollbar_area;
        assert!(track.height > 5);
        let (position, thumb) = state.scrollbar_geometry();
        assert!(thumb < track.height as usize);
        assert!(state.scrollbar_press(track.x, track.y + position as u16));
        assert!(state.scrollbar_move(track.y));
        terminal.draw(|frame| state.draw(frame)).unwrap();
        assert_eq!(state.history_top_row, 0);
        assert!(state.selection_rows.join("\n").contains("Log line 000"));
        assert!(state.scrollbar_move(track.bottom() - 1));
        terminal.draw(|frame| state.draw(frame)).unwrap();
        assert_eq!(state.scroll_from_bottom, 0);
        assert!(state.selection_rows.join("\n").contains("Log line 119"));
        state.mouse_release();
        assert!(!state.scrollbar_move(track.y));
        assert!(!state.scrollbar_press(track.x.saturating_sub(3), track.y));
    }

    #[test]
    fn scrollbar_can_reach_entries_older_than_initial_lazy_history_window() {
        let messages = (0..440)
            .map(|i| Message::new("assistant", format!("Timeline {i:03}")))
            .collect::<Vec<_>>();
        let mut state = UiState::new(
            "mock".into(),
            "test".into(),
            ThemeStore::load_at(None).unwrap(),
            &messages,
        );
        let mut terminal = Terminal::new(TestBackend::new(90, 24)).unwrap();
        terminal.draw(|frame| state.draw(frame)).unwrap();
        assert_eq!(state.history_visible, 300);
        let track = state.scrollbar_area;
        assert!(state.scrollbar_press(track.x, track.y));
        terminal.draw(|frame| state.draw(frame)).unwrap();
        assert_eq!(state.history_visible, messages.len());
        assert_eq!(state.history_top_row, 0);
        assert!(state.selection_rows.join("\n").contains("Timeline 000"));
        state.mouse_release();
        state.scroll_to_bottom();
        terminal.draw(|frame| state.draw(frame)).unwrap();
        assert!(state.selection_rows.join("\n").contains("Timeline 439"));
    }

    #[test]
    fn drag_selection_autoscrolls_and_copies_across_multiple_viewports() {
        let messages = (0..80)
            .map(|i| Message::new("assistant", format!("Excerpt {i:03}")))
            .collect::<Vec<_>>();
        let mut state = UiState::new(
            "mock".into(),
            "test".into(),
            ThemeStore::load_at(None).unwrap(),
            &messages,
        );
        let mut terminal = Terminal::new(TestBackend::new(80, 18)).unwrap();
        terminal.draw(|frame| state.draw(frame)).unwrap();
        state.scroll(state.history_max_scroll as isize);
        terminal.draw(|frame| state.draw(frame)).unwrap();
        assert_eq!(state.history_top_row, 0);
        let area = state.selection_area;
        state.select_start(area.x + 3, area.y + 2);
        state.select_drag(area.x + 4, area.bottom() - 1);
        assert!(state.selection_needs_auto_scroll());
        let first_end = state.selection_end.unwrap().0;
        for _ in 0..40 {
            assert!(state.auto_scroll_selection());
            terminal.draw(|frame| state.draw(frame)).unwrap();
        }
        assert!(state.selection_end.unwrap().0 > first_end + 15);
        assert!(state.selected_text().unwrap().contains("Excerpt 012"));
        assert!(state.selected_text().unwrap().contains("Excerpt 025"));
        state.mouse_release();
        assert!(!state.selection_needs_auto_scroll());
        assert!(!state.auto_scroll_selection());
        assert!(state.selected_text().unwrap().contains("Excerpt 025"));
    }

    #[test]
    fn selection_can_auto_scroll_upward_and_keeps_earlier_text() {
        let messages = (0..80)
            .map(|i| Message::new("assistant", format!("Earlier {i:03}")))
            .collect::<Vec<_>>();
        let mut state = UiState::new(
            "mock".into(),
            "test".into(),
            ThemeStore::load_at(None).unwrap(),
            &messages,
        );
        let mut terminal = Terminal::new(TestBackend::new(80, 18)).unwrap();
        terminal.draw(|frame| state.draw(frame)).unwrap();
        let area = state.selection_area;
        state.select_start(area.x + 2, area.bottom() - 3);
        let start_row = state.selection_start.unwrap().0;
        state.select_drag(area.x + 2, area.y);
        assert!(state.selection_needs_auto_scroll());
        for _ in 0..25 {
            assert!(state.auto_scroll_selection());
            terminal.draw(|frame| state.draw(frame)).unwrap();
        }
        assert!(state.selection_end.unwrap().0 < start_row - 20);
        assert!(state.selected_text().unwrap().lines().count() > 20);
        state.mouse_release();
        assert!(!state.auto_scroll_selection());
    }

    #[test]
    fn scrollbar_drag_does_not_replace_existing_selection() {
        let messages = (0..45)
            .map(|i| Message::new("assistant", format!("Record {i:03}")))
            .collect::<Vec<_>>();
        let mut state = UiState::new(
            "mock".into(),
            "test".into(),
            ThemeStore::load_at(None).unwrap(),
            &messages,
        );
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        terminal.draw(|frame| state.draw(frame)).unwrap();
        let area = state.selection_area;
        let row = state
            .selection_rows
            .iter()
            .position(|line| line.contains("Record 044"))
            .unwrap();
        state.select_start(area.x + 3, area.y + row as u16);
        state.select_drag(area.x + 8, area.y + row as u16);
        let selected = state.selected_text().unwrap();
        state.mouse_release();
        let x = state.scrollbar_area.x;
        let y = state.scrollbar_area.y;
        assert!(state.scrollbar_press(x, y));
        terminal.draw(|frame| state.draw(frame)).unwrap();
        assert_eq!(state.selected_text().as_deref(), Some(selected.as_str()));
    }
    #[test]
    fn scrolling_pins_visible_history_while_streaming_then_returns_to_latest() {
        let messages = (0..60)
            .map(|i| Message::new("assistant", format!("existing line {i:03}")))
            .collect::<Vec<_>>();
        let mut state = UiState::new(
            "test".into(),
            "default".into(),
            ThemeStore::load_at(None).unwrap(),
            &messages,
        );
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| state.draw(frame)).unwrap();
        state.scroll(12);
        terminal.draw(|frame| state.draw(frame)).unwrap();
        let first_visible = state.selection_rows.join("\n");
        for _ in 0..100 {
            state.event(UiEvent::Text(" streamed output\n".into()));
            terminal.draw(|frame| state.draw(frame)).unwrap();
        }
        assert_eq!(
            first_visible,
            state.selection_rows.join("\n"),
            "new output must not move the reading viewport"
        );
        state.scroll_to_bottom();
        terminal.draw(|frame| state.draw(frame)).unwrap();
        assert!(state.selection_rows.join("\n").contains("streamed output"));
        assert!(state.scroll_anchor.is_none());
    }

    #[test]
    fn copy_selection_never_breaks_a_combined_emoji_cluster() {
        let mut state = UiState::new(
            "test".into(),
            "default".into(),
            ThemeStore::load_at(None).unwrap(),
            &[Message::new("user", "👩‍💻")],
        );
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| state.draw(frame)).unwrap();
        let row = state
            .selection_rows
            .iter()
            .position(|line| line.contains("👩‍💻"))
            .unwrap();
        let text = &state.selection_rows[row];
        let cells = text.split_once("👩‍💻").unwrap().0;
        let x = state.selection_area.x + UnicodeWidthStr::width(cells) as u16;
        let y = state.selection_area.y + row as u16;
        state.select_start(x, y);
        state.select_drag(x + 1, y);
        assert_eq!(state.selected_text().as_deref(), Some("👩‍💻"));
    }

    #[test]
    fn borrowed_viewport_renders_the_same_cells_as_original_paragraph() {
        let messages = vec![
            Message::new("user", "你好，检查 Unicode 和 emoji"),
            Message::new(
                "assistant",
                "## Results\n\n**Passed**: ✅ 3 tests\n\n- Cargo checks\n- 宽字符和组合字形 e\u{301}\n\n`fn main() {}`",
            ),
        ];
        let mut state = UiState::new(
            "mock".into(),
            "test".into(),
            ThemeStore::load_at(None).unwrap(),
            &messages,
        );
        let mut actual = Terminal::new(TestBackend::new(88, 25)).unwrap();
        actual.draw(|frame| state.draw(frame)).unwrap();
        let area = state.selection_area;
        let end = (state.history_top_row + area.height as usize)
            .min(state.history_cache.as_ref().unwrap().text.lines.len());
        let original_lines =
            state.history_cache.as_ref().unwrap().text.lines[state.history_top_row..end].to_vec();
        let theme = state.themes.colors;
        let mut reference = Terminal::new(TestBackend::new(88, 25)).unwrap();
        reference
            .draw(|frame| {
                frame.render_widget(
                    Block::default().style(Style::default().bg(theme.background)),
                    frame.area(),
                );
                frame.render_widget(
                    Paragraph::new(Text::from(original_lines))
                        .style(Style::default().fg(theme.text).bg(theme.background)),
                    area,
                );
            })
            .unwrap();
        for y in area.y..area.bottom() {
            for x in area.x..area.right() {
                let real = actual.backend().buffer().cell((x, y)).unwrap();
                let previous = reference.backend().buffer().cell((x, y)).unwrap();
                assert_eq!(
                    (real.symbol(), real.fg, real.bg, real.modifier),
                    (
                        previous.symbol(),
                        previous.fg,
                        previous.bg,
                        previous.modifier
                    ),
                    "viewport cell ({x},{y}) changed"
                );
            }
        }
    }

    #[test]
    #[ignore = "TUI draw microbenchmark; run manually for frame timings"]
    fn bench_streaming_frame_latency_three_hundred_and_three_thousand_messages() {
        use std::time::Instant;
        for count in [300, 3000] {
            let messages = (0..count).map(|i| Message::new("assistant",
                format!("### Task {i}\n\nA short description, a path `src/provider.rs` and a result.\n\n- note one\n- note two")))
                .collect::<Vec<_>>();
            let mut state = UiState::new(
                "model".into(),
                "default".into(),
                ThemeStore::load_at(None).unwrap(),
                &messages,
            );
            let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
            terminal.draw(|frame| state.draw(frame)).unwrap();
            let mut samples = Vec::new();
            for _ in 0..60 {
                let start = Instant::now();
                state.event(UiEvent::Text(" partial streaming response".into()));
                terminal.draw(|frame| state.draw(frame)).unwrap();
                samples.push(start.elapsed().as_micros());
            }
            samples.sort_unstable();
            eprintln!(
                "TUI frames messages={count} median={}us p95={}us",
                samples[30], samples[56]
            );
        }
    }

    #[test]
    fn older_history_is_reachable_without_rendering_it_all_initially() {
        let messages: Vec<_> = (0..700)
            .map(|i| Message::new("user", format!("history item {i}")))
            .collect();
        let mut state = UiState::new(
            "mock".into(),
            "test".into(),
            ThemeStore::load_at(None).unwrap(),
            &messages,
        );
        assert_eq!(state.entries.len(), 700);
        assert_eq!(state.history_visible, 300);
        for _ in 0..3 {
            state.scroll(50);
        }
        assert_eq!(state.history_visible, 700);
        let rendered = state
            .history_text(state.themes.colors, 80)
            .lines
            .iter()
            .flat_map(|line| line.spans.iter().map(|span| span.content.as_ref()))
            .collect::<String>();
        assert!(rendered.contains("history item 0"));
    }
    #[test]
    fn zero_and_tiny_terminal_draws_are_safe() {
        let themes = ThemeStore::load_at(None).unwrap();
        let mut ui = UiState::new("model".into(), "test".into(), themes, &[]);
        for (width, height) in [(0, 0), (0, 10), (12, 0), (1, 1), (15, 7), (19, 20)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|frame| ui.draw(frame)).unwrap();
            assert_eq!(ui.viewport_height, 0);
            assert!(ui.visible_links.is_empty());
        }
        let mut normal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        normal.draw(|frame| ui.draw(frame)).unwrap();
        assert!(
            ui.viewport_height > 0,
            "normal layout must recover after resize"
        );
    }

    #[test]
    fn command_output_is_visible_after_scrolling_old_history() {
        let messages = (0..90)
            .map(|i| Message::new("assistant", format!("Historical item {i:03}")))
            .collect::<Vec<_>>();
        let mut ui = UiState::new(
            "mock".into(),
            "review".into(),
            ThemeStore::load_at(None).unwrap(),
            &messages,
        );
        let mut terminal = Terminal::new(TestBackend::new(82, 24)).unwrap();
        terminal.draw(|frame| ui.draw(frame)).unwrap();
        ui.scroll(45);
        terminal.draw(|frame| ui.draw(frame)).unwrap();
        assert!(ui.scroll_anchor.is_some());
        ui.info("Result", "FRESH_INFORMATION_AFTER_SCROLL");
        terminal.draw(|frame| ui.draw(frame)).unwrap();
        assert!(
            ui.selection_rows
                .join("\n")
                .contains("FRESH_INFORMATION_AFTER_SCROLL"),
            "new command output must be visible instead of remaining pinned to old history"
        );
    }

    #[test]
    fn resize_preserves_logical_history_line_instead_of_old_wrapped_row() {
        let messages = (0..85)
            .map(|i| {
                Message::new(
                    "user",
                    format!("HISTORY_{i:03} {}", "long_words ".repeat(17)),
                )
            })
            .collect::<Vec<_>>();
        let mut ui = UiState::new(
            "mock".into(),
            "review".into(),
            ThemeStore::load_at(None).unwrap(),
            &messages,
        );
        let mut wide = Terminal::new(TestBackend::new(115, 25)).unwrap();
        wide.draw(|frame| ui.draw(frame)).unwrap();
        ui.scroll(96);
        wide.draw(|frame| ui.draw(frame)).unwrap();
        let original_source = ui.history_cache.as_ref().unwrap().source_lines[ui.history_top_row];
        assert!(original_source > 10);
        let mut narrow = Terminal::new(TestBackend::new(53, 25)).unwrap();
        narrow.draw(|frame| ui.draw(frame)).unwrap();
        let narrow_source = ui.history_cache.as_ref().unwrap().source_lines[ui.history_top_row];
        assert_eq!(
            narrow_source, original_source,
            "resize should preserve the source line"
        );
        wide.draw(|frame| ui.draw(frame)).unwrap();
        let restored_source = ui.history_cache.as_ref().unwrap().source_lines[ui.history_top_row];
        assert_eq!(
            restored_source, original_source,
            "round-trip reflow should keep context"
        );
    }

    #[test]
    fn legacy_attachment_markers_stop_at_private_use_boundary() {
        let mut user = Message::new("user", "old session with too many attachments");
        user.provider_items = Some(
            (0..6_402)
                .map(|i| {
                    serde_json::json!({
                        "type":"kroma_attachment","kind":"log",
                        "path":format!("/missing/{i}.log"),"name":format!("log-{i}"),
                    })
                })
                .collect(),
        );
        let mut ui = UiState::new(
            "mock".into(),
            "review".into(),
            ThemeStore::load_at(None).unwrap(),
            &[user],
        );
        // Older, unmarked attachments must remain in restored history even
        // when the 6400 private-use marker slots have been exhausted.
        assert_eq!(ui.entries.len(), 1);
        ui.editor.key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Up,
            crossterm::event::KeyModifiers::NONE,
        ));
        let parts = ui.editor.history_attachments();
        assert_eq!(parts.len(), 6_402);
        assert_eq!(
            parts
                .iter()
                .filter(|part| crate::attachments::marker(part).is_some())
                .count(),
            6_400
        );
        assert!(parts[6_400]["marker"].is_null());
    }

    #[test]
    fn resize_clears_invalid_selection_without_copying_wrong_text() {
        let answer = format!("{} TARGET {}", "prefix".repeat(7), "suffix".repeat(12));
        let mut ui = UiState::new(
            "mock".into(),
            "review".into(),
            ThemeStore::load_at(None).unwrap(),
            &[Message::new("assistant", answer)],
        );
        let mut wide = Terminal::new(TestBackend::new(110, 24)).unwrap();
        wide.draw(|frame| ui.draw(frame)).unwrap();
        let row = ui
            .selection_rows
            .iter()
            .position(|s| s.contains("TARGET"))
            .unwrap();
        let prefix = ui.selection_rows[row].split_once("TARGET").unwrap().0;
        let x = ui.selection_area.x + UnicodeWidthStr::width(prefix) as u16;
        let y = ui.selection_area.y + row as u16;
        ui.select_start(x, y);
        ui.select_drag(x + 5, y);
        ui.mouse_release();
        assert_eq!(ui.selected_text().as_deref(), Some("TARGET"));
        let mut narrow = Terminal::new(TestBackend::new(40, 24)).unwrap();
        narrow.draw(|frame| ui.draw(frame)).unwrap();
        assert!(ui.selection_start.is_none() && ui.selection_end.is_none());
        assert!(
            ui.selected_text().is_none(),
            "a terminal resize must clear old wrapped coordinates instead of copying wrong text"
        );
        let row = ui
            .selection_rows
            .iter()
            .position(|s| s.contains("TARGET"))
            .unwrap();
        let prefix = ui.selection_rows[row].split_once("TARGET").unwrap().0;
        let x = ui.selection_area.x + UnicodeWidthStr::width(prefix) as u16;
        let y = ui.selection_area.y + row as u16;
        ui.select_start(x, y);
        ui.select_drag(x + 5, y);
        ui.mouse_release();
        assert_eq!(ui.selected_text().as_deref(), Some("TARGET"));
    }

    #[test]
    fn narrow_terminal_draw_does_not_panic() {
        let mut store = ThemeStore::load_at(None).unwrap();
        store.select("mono").unwrap();
        let mut ui = UiState::new("mock-model".into(), "test".into(), store, &[]);
        ui.add_user("**Markdown** 你好\n- list");
        ui.event(UiEvent::ToolStarted {
            name: "shell".into(),
            id: "x".into(),
            args: "pwd".into(),
        });
        let mut terminal = Terminal::new(TestBackend::new(30, 12)).unwrap();
        terminal.draw(|frame| ui.draw(frame)).unwrap();
        assert!(ui.viewport_height >= 3);
        let text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains("Kroma"));
        assert!(
            !text.contains("Conversation"),
            "avoid a full-screen chat box"
        );
    }

    #[test]
    fn conversation_is_readable_without_competing_boxes() {
        let store = ThemeStore::load_at(None).unwrap();
        let mut ui = UiState::new("demo-model".into(), "demo".into(), store, &[]);
        ui.add_user("Check project and report tests");
        ui.event(UiEvent::ToolStarted {
            name: "shell".into(),
            id: "tool-1".into(),
            args: "cargo test --locked".into(),
        });
        ui.event(UiEvent::ToolFinished {
            id: "tool-1".into(),
            result: "exit: 0\nall tests passed".into(),
            failed: false,
        });
        ui.event(UiEvent::Text(
            "## Tests passed\nAll checks succeeded.".into(),
        ));
        ui.running = false;
        ui.tools_expanded = true;
        let mut terminal = Terminal::new(TestBackend::new(110, 32)).unwrap();
        terminal.draw(|frame| ui.draw(frame)).unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        for expected in ["Kroma", "cargo test", "Tests passed", "all tests passed"] {
            assert!(text.contains(expected), "missing {expected}");
        }
        assert!(
            !text.contains("┌ Conversation"),
            "transcript should breathe"
        );
    }

    #[test]
    fn context_display_uses_total_window_not_reserved_input_budget() {
        use crate::events::EventSink;

        let theme = ThemeStore::load_at(None).unwrap();
        let mut state = UiState::new("mock".into(), "default".into(), theme, &[]);
        assert_eq!(state.context_display(), "Ctx —");
        state.configure_status(500_000, Some("high".into()));
        assert_eq!(state.context_display(), "Ctx — / 500k");

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let sink = UiSink(tx);
        sink.on_event(RuntimeEvent::ContextPrepared {
            messages: 2,
            estimated_input_tokens: 125_000,
            available_input_tokens: 372_000,
        });
        state.event(rx.try_recv().unwrap());
        assert_eq!(state.context_display(), "Ctx ~125k / 500k · ~25%");

        sink.on_event(RuntimeEvent::ModelFinished {
            request: 1,
            tools: 0,
            reasoning: None,
            input_tokens: Some(100_000),
            output_tokens: Some(10_000),
            cached_tokens: Some(80_000),
            reasoning_tokens: Some(3_000),
        });
        while let Ok(event) = rx.try_recv() {
            state.event(event);
        }
        assert_eq!(state.context_display(), "Ctx 100k / 500k · 20%");
        sink.on_event(RuntimeEvent::CompactionUsage {
            requests: 2,
            input_tokens: Some(1_000),
            output_tokens: Some(200),
            cached_tokens: Some(800),
        });
        while let Ok(event) = rx.try_recv() {
            state.event(event);
        }
        assert_eq!(state.apec_usage.calls, 2);
        assert_eq!(state.usage.calls, 1);
        assert_eq!(state.context_display(), "Ctx 100k / 500k · 20%");
        // Show current request usage, not cumulative billed tokens.
        state.event(UiEvent::ActualInput(150_000));
        assert_eq!(state.context_display(), "Ctx 150k / 500k · 30%");
        state.reset_context();
        assert_eq!(state.context_display(), "Ctx — / 500k");

        // A model/session switch updates the total window without inventing usage.
        state.configure_status(200_000, None);
        assert_eq!(state.context_display(), "Ctx — / 200k");
        sink.on_event(RuntimeEvent::ContextPrepared {
            messages: 2,
            estimated_input_tokens: 50_000,
            available_input_tokens: 168_000,
        });
        state.event(rx.try_recv().unwrap());
        assert_eq!(state.context_display(), "Ctx ~50k / 200k · ~25%");
    }

    #[test]
    fn context_status_uses_total_window_in_wide_and_narrow_terminals() {
        let theme = ThemeStore::load_at(None).unwrap();
        let mut state = UiState::new("mock".into(), "default".into(), theme, &[]);
        state.configure_status(500_000, Some("high".into()));
        state.event(UiEvent::ActualInput(100_000));
        for (width, expected) in [(110, "Context 20%"), (40, "Context 20%")] {
            let mut terminal = Terminal::new(TestBackend::new(width, 24)).unwrap();
            terminal.draw(|frame| state.draw(frame)).unwrap();
            let text = terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>();
            assert!(text.contains(expected), "missing {expected}: {text}");
        }
    }

    #[test]
    fn mixed_attachments_keep_numbered_order_after_history_restore() {
        let mut message = Message::new("user", "Compare these files in sequence");
        message.provider_items = Some(vec![
            serde_json::json!({"type":"kroma_attachment","kind":"image","name":"before.png"}),
            serde_json::json!({"type":"kroma_attachment","kind":"log","name":"Pasted text"}),
            serde_json::json!({"type":"kroma_attachment","kind":"image","name":"after.png"}),
        ]);
        let ui = UiState::new(
            "mock".into(),
            "test".into(),
            ThemeStore::load_at(None).unwrap(),
            &[message],
        );
        let content = ui
            .history_text(ui.themes.colors, 80)
            .lines
            .iter()
            .flat_map(|line| line.spans.iter().map(|span| span.content.as_ref()))
            .collect::<String>();
        let first = content.find("#1 ▧ before.png").expect("image 1 label");
        let second = content.find("#2 ▤ Pasted text").expect("log 2 label");
        let third = content.find("#3 ▧ after.png").expect("image 3 label");
        assert!(first < second && second < third);
    }

    fn rendered_rows(ui: &mut UiState, width: u16, height: u16) -> Vec<String> {
        rendered_text(ui, width, height)
            .iter()
            .map(|row| row.chars().filter(|ch| *ch != ' ').collect())
            .collect()
    }

    /// Cell-accurate rows, keeping spaces: alignment and separation are the
    /// behaviour under test for layout work.
    fn rendered_text(ui: &mut UiState, width: u16, height: u16) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| ui.draw(frame)).unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .chunks(width as usize)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect()
    }

    #[test]
    fn bilingual_chrome_picker_descriptions_and_unknown_usage_render_at_multiple_sizes() {
        let mut ui = UiState::new(
            "mock".into(),
            "test".into(),
            ThemeStore::load_at(None).unwrap(),
            &[],
        );
        ui.apply_preferences(super::super::preferences::Preferences {
            language: super::super::preferences::Language::Zh,
            full_width: false,
        });
        ui.configure_status(500_000, Some("high".into()));
        ui.event(UiEvent::Context { estimated: 100_000 });
        ui.event(UiEvent::Usage {
            input: None,
            output: None,
            cached: None,
            reasoning: None,
        });
        for width in [30, 40, 80, 160] {
            let output = rendered_rows(&mut ui, width, 26).join("\n");
            assert!(
                output.contains("上下文"),
                "localized footer missing at {width}: {output}"
            );
            assert!(
                output.contains("未报告"),
                "unknown must not become 0 at {width}: {output}"
            );
            assert!(
                UnicodeWidthStr::width(ui.usage_status((width - 8) as usize).as_str())
                    <= (width - 8) as usize
            );
        }
        ui.open_command_completion("/language".into());
        let output = rendered_rows(&mut ui, 100, 26).join("\n");
        assert!(output.contains("命令索引"));
        assert!(output.contains("切换界面语言并保存"));
        ui.picker = None;
        ui.enter_home();
        let output = rendered_rows(&mut ui, 100, 30).join("\n");
        assert!(output.contains("开始新会话"));
        ui.apply_preferences(super::super::preferences::Preferences::default());
        let output = rendered_rows(&mut ui, 100, 30).join("\n");
        assert!(output.contains("Startaconversation"));
    }

    #[test]
    fn wrapping_keeps_source_line_identity_within_the_reading_column() {
        let text = Text::from(vec![
            Line::raw("word ".repeat(45)),
            Line::raw(format!("│ {}", "x".repeat(150))),
        ]);
        let (wrapped, sources) = wrap_document(text, 180);
        assert_eq!(wrapped.lines.len(), sources.len());
        assert_eq!(*sources.last().unwrap(), 1);
        for line in &wrapped.lines {
            assert!(UnicodeWidthStr::width(line.to_string().as_str()) <= 180);
        }
        let (narrow, sources) = wrap_document(Text::from(vec![Line::raw("word ".repeat(45))]), 60);
        assert!(narrow.lines.len() > 1, "the column width is the wrap width");
        assert!(sources.iter().all(|source| *source == 0));
    }

    #[test]
    fn history_navigation_reports_unseen_updates_without_moving_and_survives_completion() {
        let messages: Vec<_> = (0..60)
            .map(|i| Message::new("assistant", format!("history line {i}")))
            .collect();
        let mut ui = UiState::new(
            "mock".into(),
            "test".into(),
            ThemeStore::load_at(None).unwrap(),
            &messages,
        );
        rendered_rows(&mut ui, 80, 24);
        ui.scroll(12);
        rendered_rows(&mut ui, 80, 24);
        let anchor = ui.scroll_anchor;
        ui.event(UiEvent::Text("new streaming result".into()));
        let output = rendered_rows(&mut ui, 80, 24).join("\n");
        assert_eq!(anchor, ui.scroll_anchor);
        assert!(output.contains("Browsinghistory"));
        assert!(ui.navigation_status().contains("new updates"));
        let reading = ui.reading_position();
        ui.load_history(&messages);
        ui.task_outcome = Some(TaskOutcome::Completed);
        ui.restore_reading_position(reading);
        rendered_rows(&mut ui, 80, 24);
        assert_eq!(anchor, ui.scroll_anchor);
        ui.scroll_to_bottom();
        assert_eq!(ui.unread_updates, 0);
        assert!(ui.scroll_anchor.is_none());
        assert!(rendered_rows(&mut ui, 80, 24)
            .join("\n")
            .contains("Completed"));
        ui.task_outcome = Some(TaskOutcome::Failed);
        assert!(ui.task_status().contains("failed"));
        ui.task_outcome = Some(TaskOutcome::Cancelled);
        assert!(ui.task_status().contains("Cancelled"));
        ui.enter_home();
        assert!(ui.task_outcome.is_none());
    }

    #[test]
    fn collapsed_tool_summary_preserves_full_arguments_archive_reference_and_stderr_warning() {
        let mut ui = UiState::new(
            "mock".into(),
            "test".into(),
            ThemeStore::load_at(None).unwrap(),
            &[],
        );
        let command = format!("cargo test {} UNIQUE_COMMAND_TAIL", "--locked ".repeat(80));
        let raw = serde_json::json!({"command":command}).to_string();
        assert_eq!(tool_arguments("shell", &raw), command);
        let result = tool_result_details(&serde_json::json!({"exit_code":0, "stdout":"line\n".repeat(400), "stderr":"warning: diagnostic", "stdout_file":"/tmp/kroma-complete.log"}).to_string());
        assert!(result.contains("/tmp/kroma-complete.log"));
        ui.event(UiEvent::ToolStarted {
            name: "shell".into(),
            id: "1".into(),
            args: command,
        });
        ui.event(UiEvent::ToolFinished {
            id: "1".into(),
            result,
            failed: false,
        });
        let collapsed = ui.history_text(ui.themes.colors, 160).to_string();
        assert!(collapsed.contains("Run Rust tests"));
        assert!(collapsed.contains("diagnostic reported"));
        assert!(!collapsed.contains("UNIQUE_COMMAND_TAIL"));
        assert!(
            !ui.entries[0].failed,
            "stderr warnings do not change exit status"
        );
        assert!(ui.entries[0].duration.is_some());
        ui.tools_expanded = true;
        let expanded = ui.history_text(ui.themes.colors, 160).to_string();
        assert!(expanded.contains("UNIQUE_COMMAND_TAIL"));
        assert!(expanded.contains("/tmp/kroma-complete.log"));
    }

    #[test]
    fn screenshot_regressions_compact_tools_localized_labels_and_single_history_hint() {
        let mut ui = UiState::new(
            "mock".into(),
            "test".into(),
            ThemeStore::load_at(None).unwrap(),
            &[],
        );
        ui.apply_preferences(super::super::preferences::Preferences {
            language: super::super::preferences::Language::Zh,
            full_width: false,
        });
        ui.event(UiEvent::ToolStarted {
            name: "shell".into(),
            id: "build".into(),
            args: format!(
                "cd /project && cargo build --release {}",
                " --locked".repeat(100)
            ),
        });
        ui.event(UiEvent::ToolFinished {
            id: "build".into(),
            result: "exit 0\nstderr:    Compiling kroma\n    Finished release profile".into(),
            failed: false,
        });
        for width in [40, 80, 180, 240] {
            let raw = ui.history_text(ui.themes.colors, width);
            let (wrapped, _) = wrap_document(raw, width);
            let rows: Vec<_> = wrapped
                .lines
                .iter()
                .map(ToString::to_string)
                .filter(|row| !row.trim().is_empty())
                .collect();
            assert_eq!(
                rows.len(),
                1,
                "collapsed successful tool must occupy one row at {width}: {rows:?}"
            );
            assert!(rows[0].contains('✓'));
            assert!(!rows[0].contains("警告"));
        }
        ui.info("COMMAND", "Resumed session: abc123");
        ui.editor.insert_attachment(
            char::from_u32(super::super::editor::ATTACHMENT_START).unwrap(),
            "Clipboard image".into(),
        );
        ui.note("已添加 【Clipboard image】");
        let output = rendered_rows(&mut ui, 100, 26).join("\n");
        assert!(output.contains("已恢复会话：abc123"));
        assert!(output.contains("剪贴板图片"));
        assert!(!output.contains("Clipboardimage"));
        assert!(ui.notification.contains("剪贴板图片"));
        for i in 0..30 {
            ui.info("COMMAND", format!("old line {i}"));
        }
        ui.dismiss_notification();
        rendered_rows(&mut ui, 100, 26);
        ui.scroll(10);
        let output = rendered_rows(&mut ui, 100, 26).join("\n");
        assert_eq!(
            output.matches("Ctrl+End").count(),
            1,
            "history navigation should appear once: {output}"
        );
    }

    #[test]
    fn reasoning_block_is_separated_from_the_answer_that_follows_it() {
        let mut ui = UiState::new(
            "mock".into(),
            "test".into(),
            ThemeStore::load_at(None).unwrap(),
            &[],
        );
        ui.add_user("why");
        ui.event(UiEvent::ReasoningFinal("private chain of thought".into()));
        ui.event(UiEvent::Text("the answer".into()));
        ui.running = false;
        let rows = rendered_text(&mut ui, 80, 24);
        let reasoning = rows
            .iter()
            .position(|row| row.contains("Reasoning · Ctrl+R"))
            .expect("collapsed reasoning line");
        assert!(
            rows[reasoning + 1].trim().is_empty(),
            "reasoning must not be glued to the answer header: {:?}",
            &rows[reasoning..reasoning + 3]
        );
        assert!(
            rows[reasoning + 2].contains("Kroma"),
            "the answer header follows the separator: {:?}",
            &rows[reasoning..reasoning + 3]
        );
    }

    #[test]
    fn footer_is_one_compact_row_without_repeated_command_hints() {
        let mut ui = UiState::new(
            "deepseek".into(),
            "test".into(),
            ThemeStore::load_at(None).unwrap(),
            &[],
        );
        ui.configure_status(1_000_000, Some("auto".into()));
        ui.preferences.language = super::super::preferences::Language::Zh;
        ui.event(UiEvent::ActualInput(1000));
        ui.event(UiEvent::Usage {
            input: Some(1500),
            output: Some(8),
            cached: Some(1305),
            reasoning: None,
        });
        ui.add_user("你好");
        ui.event(UiEvent::Text("answer".into()));
        ui.event(UiEvent::TaskFinished);
        let rows = rendered_text(&mut ui, 120, 30);
        let footer = rows.last().expect("footer row");
        let visible: String = footer.chars().filter(|ch| *ch != ' ').collect();
        assert!(visible.contains("deepseek"), "{visible:?}");
        assert!(visible.contains("上下文0.1%"), "{visible:?}");
        assert!(visible.contains("缓存87%"), "{visible:?}");
        assert!(visible.contains("输入/输出1.5k/8"), "{visible:?}");
        assert!(
            !visible.contains("/usage") && !visible.contains("/help"),
            "use the slash picker for commands instead of a second help bar"
        );
        let first_line = rows
            .iter()
            .filter(|row| {
                row.chars()
                    .filter(|ch| *ch != ' ')
                    .collect::<String>()
                    .contains("上下文")
            })
            .count();
        assert_eq!(first_line, 1, "context information belongs to only one row");
        let column = reading_area(Rect::new(0, 0, 120, 30), false);
        let leading = footer.chars().take_while(|ch| *ch == ' ').count();
        assert!(
            leading >= column.x as usize,
            "footer must align with the reading column"
        );
    }

    #[test]
    fn completion_row_reports_the_outcome_and_how_long_the_turn_took() {
        let mut ui = UiState::new(
            "mock".into(),
            "test".into(),
            ThemeStore::load_at(None).unwrap(),
            &[],
        );
        ui.add_user("task");
        // The real loop takes the duration when the task future resolves: the
        // final event drain keeps only measurements, so a TaskFinished event
        // may never reach the UI.
        ui.finish_task();
        ui.task_outcome = Some(TaskOutcome::Completed);
        let rows = rendered_text(&mut ui, 80, 24);
        let completed = rows
            .iter()
            .find(|row| row.contains('✓'))
            .expect("completion marker");
        assert!(completed.contains("Completed · "), "{completed:?}");
        assert!(
            completed.trim_end().ends_with('s'),
            "the row reports how long the turn took: {completed:?}"
        );
        assert!(
            !rows.concat().contains("review results above"),
            "the completion row carries information, not an acknowledgement"
        );
        ui.task_elapsed = Some(Duration::from_millis(8_340));
        ui.task_outcome = Some(TaskOutcome::Failed);
        let rows = rendered_text(&mut ui, 80, 24);
        assert!(
            rows.iter()
                .any(|row| row.contains('✕') && row.contains("8.3s")),
            "failure keeps its marker and duration"
        );
        ui.task_outcome = Some(TaskOutcome::Cancelled);
        let rows = rendered_text(&mut ui, 80, 24);
        assert!(
            rows.iter()
                .any(|row| row.contains('⊘') && row.contains("8.3s")),
            "cancellation keeps its marker and duration"
        );
    }
}
