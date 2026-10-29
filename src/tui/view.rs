use super::{
    attachments::{Attachment, MAX_ATTACHMENTS},
    editor::Editor,
    links, markdown,
    picker::Picker,
    theme::{Theme, ThemeStore},
};
use crate::{events::RuntimeEvent, protocol::Message};
use ratatui::{
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Paragraph},
    Frame,
};
use serde_json::Value;
use std::collections::HashMap;
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
fn wrap_history(text: Text<'static>, width: usize) -> Text<'static> {
    let width = width.max(1);
    let mut rows = Vec::new();
    for line in text.lines {
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
    }
    Text::from(rows)
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

/// Fill the available terminal width. Long tool commands, Markdown tables,
/// code and diffs are workspace content, not newspaper paragraphs: the old
/// 100-column limit wasted most of a maximized terminal window.
fn reading_area(area: Rect) -> Rect {
    let margin = if area.width >= 100 {
        2
    } else if area.width >= 55 {
        1
    } else {
        0
    };
    let width = area.width.saturating_sub(margin * 2);
    Rect::new(area.x + margin, area.y, width, area.height)
}

fn body_line(line: Line<'static>) -> Line<'static> {
    let mut spans = vec![Span::raw("   ")];
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
    theme: Theme,
    text: Text<'static>,
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
                result: short_tool_result(result),
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
/// Stored and live tool results already use the same short_tool_result format.
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
        Err(_) => return excerpt(raw, 240),
    };
    if name == "shell" {
        return value["command"]
            .as_str()
            .map_or_else(|| excerpt(raw, 240), |s| excerpt(s, 240));
    }
    excerpt(&value.to_string(), 240)
}

fn short_tool_result(raw: &str) -> String {
    let Ok(value) = serde_json::from_str::<Value>(raw) else {
        return excerpt(raw, 1200);
    };
    if let Some(error) = value.get("error") {
        return excerpt(error.as_str().unwrap_or(raw), 1200);
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
        for item in content.iter().take(4) {
            if let Some(text) = item.get("text").and_then(Value::as_str) {
                lines.push(text.to_owned());
            }
        }
    }
    if lines.is_empty() {
        return excerpt(raw, 1200);
    }
    excerpt(&lines.join("\n"), 1200)
}

pub struct UiState {
    pub editor: Editor,
    pub picker: Option<Picker>,
    pub usage: crate::usage::UsageMeter,
    pub attachments: Vec<Attachment>,
    removed_drafts: Vec<Attachment>,
    saved_draft_attachments: Option<Vec<Attachment>>,
    pub attach_mode: bool,
    pub attach_path: String,
    next_marker: u32,
    pub themes: ThemeStore,
    pub session: String,
    session_title: Option<String>,
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
}

impl UiState {
    pub fn new(model: String, session: String, themes: ThemeStore, messages: &[Message]) -> Self {
        let mut result = Self {
            editor: Editor::default(),
            picker: None,
            usage: crate::usage::UsageMeter::default(),
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
        };
        result.load_history(messages);
        result.restore_input_history(messages);
        result
    }

    pub fn add_user(&mut self, text: &str) {
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
        self.active_assistant = None;
        self.scroll_from_bottom = 0;
        self.scroll_anchor = None;
        self.running = true;
        self.phase = "preparing";
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
                    while char::from_u32(candidate).is_some_and(|ch| text.contains(ch)) {
                        candidate += 1;
                    }
                    let ch = char::from_u32(candidate).unwrap();
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
        fn compact(value: u64) -> String {
            if value >= 10_000 {
                format!("{:.0}k", value as f64 / 1000.0)
            } else if value >= 1_000 {
                format!("{:.1}k", value as f64 / 1000.0)
            } else {
                value.to_string()
            }
        }
        let window = self.context_window as u64;
        if window == 0 {
            return "Ctx —".into();
        }
        match self.context_input {
            Some(input) => {
                let percent = (input as f64 * 100.0 / window as f64).round() as u64;
                let marker = if self.context_estimated { "~" } else { "" };
                format!(
                    "Ctx {marker}{} / {} · {marker}{percent}%",
                    compact(input),
                    compact(window)
                )
            }
            None => format!("Ctx — / {}", compact(window)),
        }
    }

    fn compact_context(&self) -> String {
        match (self.context_input, self.context_window) {
            (Some(input), window) if window > 0 => {
                let percent = (input as f64 * 100.0 / window as f64).round() as u64;
                format!("Ctx {percent}%")
            }
            _ => "Ctx —".into(),
        }
    }

    fn footer_status(&self, width: usize) -> String {
        let think = format!("Think {}", self.thinking.as_deref().unwrap_or("n/a"));
        let context = if width >= 60 {
            self.context_display()
        } else {
            self.compact_context()
        };
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
            let context = self.compact_context();
            let right = format!("{level} · {context}");
            let available = width.saturating_sub(UnicodeWidthStr::width(right.as_str()) + 3);
            return format!("{} · {right}", ellipsize_cells(&self.model, available));
        }
        ellipsize_cells(
            &format!("{} · {}", self.model, self.compact_context()),
            width,
        )
    }

    pub fn note(&mut self, text: impl Into<String>) {
        self.notification = text.into();
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
        self.session_title
            .as_deref()
            .unwrap_or(if self.session == "default" {
                "default"
            } else {
                "untitled session"
            })
    }

    /// Host command output is transient UI content, not part of model history.
    /// Put long lists in the scrollable transcript instead of the tiny footer.
    pub fn info(&mut self, title: &str, content: impl Into<String>) {
        self.history_cache = None;
        self.entries.push(Entry::new(Kind::Info, title, content));
        self.scroll_from_bottom = 0;
    }

    pub fn load_history(&mut self, messages: &[Message]) {
        self.history_cache = None;
        self.mouse_release();
        self.selection_start = None;
        self.selection_end = None;
        self.entries.clear();
        self.active_assistant = None;
        self.active_reasoning = None;
        self.running = false;
        self.phase = "ready";
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
                        entry.details = short_tool_result(result);
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
                        entry.details = short_tool_result(result);
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

    pub fn event(&mut self, event: UiEvent) {
        // Usage and token updates only change the footer. Collapsed ongoing
        // reasoning is not rendered, so no Markdown reflow is required for
        // each hidden reasoning delta or its final reconciliation.
        let unchanged_history = matches!(
            &event,
            UiEvent::Usage { .. } | UiEvent::Context { .. } | UiEvent::ActualInput(_)
        ) || (!self.reasoning_expanded
            && self.active_reasoning.is_some()
            && matches!(&event, UiEvent::ReasoningFinal(_)
                | UiEvent::ReasoningDelta(_) if self.phase == "thinking"));
        if !unchanged_history {
            self.history_cache = None;
        }
        match event {
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
            UiEvent::TaskStarted => {
                self.running = true;
                self.phase = "preparing";
            }
            UiEvent::TaskFinished => {
                self.running = false;
                self.active_assistant = None;
                self.active_reasoning = None;
                self.phase = "ready";
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
                self.phase = "waiting";
            }
            UiEvent::Text(delta) => {
                self.phase = "streaming";
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
                self.phase = "thinking";
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
                self.phase = "tool";
                let mut entry = Entry::new(Kind::Tool, format!("{name} · running"), args);
                entry.call_id = Some(id);
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
        self.scroll_anchor = None;
        self.prepend_rows_before = None;
        self.scroll_from_bottom = 0;
    }

    fn scroll_to_row(&mut self, row: usize) {
        let top = row.min(self.history_max_scroll);
        self.scroll_from_bottom = self.history_max_scroll - top;
        self.scroll_anchor = (self.scroll_from_bottom > 0).then_some(top);
    }

    fn scrollbar_geometry(&self) -> (usize, usize) {
        let height = self.scrollbar_area.height as usize;
        let total = self.history_max_scroll + self.viewport_height as usize;
        if height == 0 || total == 0 {
            return (0, height);
        }
        let thumb = (height * height / total).clamp(1, height);
        let position = if self.history_max_scroll == 0 {
            0
        } else {
            self.history_top_row * (height - thumb) / self.history_max_scroll
        };
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
        let mut lines = Vec::new();
        let mut markdown_targets = Vec::new();
        if self.entries.is_empty() {
            lines.push(Line::default());
            lines.push(Line::from(Span::styled(
                "   ◆  Ready for a task.",
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
                        Span::styled("  You", role_style(theme.user)),
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
                        Span::styled("  Kroma", role_style(theme.text)),
                    ]));
                    if !entry.body.is_empty() {
                        let (mut rendered, tags) = markdown::render_with_links(
                            &entry.body,
                            theme,
                            width.saturating_sub(3),
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
                    let (marker, color) = if entry.failed {
                        ("✕", theme.error)
                    } else if status == "running" {
                        ("◌", theme.tool)
                    } else {
                        ("✓", theme.user)
                    };
                    // A tool is activity, not a separate article. In collapsed
                    // mode show only one purposeful line, without the same
                    // Ctrl+O instruction repeated for every call.
                    let display_name = compact_line(name, width.saturating_sub(13).min(24));
                    let command_width =
                        width.saturating_sub(5 + UnicodeWidthStr::width(display_name.as_str()) + 2);
                    let command = compact_line(&entry.body, command_width);
                    lines.push(Line::from(vec![
                        Span::styled(format!("  {marker}  "), Style::default().fg(color)),
                        Span::styled(display_name, role_style(theme.tool)),
                        Span::styled(format!("  {command}"), Style::default().fg(theme.muted)),
                    ]));
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
                    let streaming = self.running
                        && self.phase == "thinking"
                        && self
                            .active_reasoning
                            .is_some_and(|index| std::ptr::eq(entry, &self.entries[index]));
                    let label = if streaming {
                        "  ◌  Thinking…"
                    } else {
                        "  ◇  Reasoning"
                    };
                    lines.push(Line::from(Span::styled(
                        label,
                        Style::default().fg(theme.muted),
                    )));
                    if self.reasoning_expanded {
                        let (rendered, tags) = markdown::render_with_links(
                            &entry.body,
                            theme,
                            width.saturating_sub(3),
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
                }
                Kind::Info => {
                    lines.push(Line::from(Span::styled(
                        format!("  ·  {}", entry.title),
                        role_style(theme.muted),
                    )));
                    for line in entry.body.lines() {
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
        if self.running && matches!(self.phase, "preparing" | "waiting") {
            lines.push(Line::from(Span::styled(
                if self.phase == "waiting" {
                    "  ◌  Waiting for model…"
                } else {
                    "  ◌  Preparing context…"
                },
                Style::default().fg(theme.muted),
            )));
        }
        (Text::from(lines), markdown_targets)
    }

    pub fn draw(&mut self, frame: &mut Frame) {
        let colors = self.themes.colors;
        let full_area = frame.area();
        frame.render_widget(
            Block::default().style(Style::default().bg(colors.background)),
            full_area,
        );
        let area = reading_area(full_area);
        let editor_rows = self.editor.display_rows(area.width.saturating_sub(6));
        let composer_height = editor_rows.saturating_add(2).clamp(3, 10);
        let layout = Layout::vertical([
            Constraint::Length(2),
            Constraint::Min(1),
            Constraint::Length(composer_height),
            Constraint::Length(2),
        ])
        .split(area);
        let (status, status_color) = if self.running {
            (
                match self.phase {
                    "streaming" => "● writing",
                    "thinking" => "● thinking",
                    "waiting" => "● waiting",
                    "tool" => "● running tool",
                    _ => "● working",
                },
                colors.tool,
            )
        } else {
            ("● ready", colors.user)
        };
        // Header: identity and activity only. Model, Think and Context sit
        // together in one footer row, including the common 80-column case.
        let header_line = Rect::new(layout[0].x, layout[0].y, layout[0].width, 1);
        let activity_width = UnicodeWidthStr::width(status);
        let show_activity = header_line.width as usize >= activity_width + 25;
        let left_width = (header_line.width as usize).saturating_sub(if show_activity {
            activity_width + 2
        } else {
            0
        });
        let label = ellipsize_cells(self.session_label(), left_width.saturating_sub(12));
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
            Rect::new(
                header_line.x,
                header_line.y,
                left_width.min(u16::MAX as usize) as u16,
                1,
            ),
        );
        if show_activity {
            frame.render_widget(
                Paragraph::new(status)
                    .style(Style::default().fg(status_color))
                    .alignment(Alignment::Right),
                Rect::new(
                    header_line.right().saturating_sub(activity_width as u16),
                    header_line.y,
                    activity_width as u16,
                    1,
                ),
            );
        }
        frame.render_widget(
            Paragraph::new("─".repeat(usize::from(layout[0].width)))
                .style(Style::default().fg(colors.border)),
            Rect::new(
                layout[0].x,
                layout[0].y.saturating_add(1),
                layout[0].width,
                1,
            ),
        );

        let history = Rect::new(
            layout[1].x.saturating_add(1),
            layout[1].y,
            layout[1].width.saturating_sub(2),
            layout[1].height,
        );
        self.viewport_height = history.height;
        let cached = self.history_cache.as_ref().is_some_and(|cache| {
            cache.width == history.width
                && cache.visible == self.history_visible
                && cache.tools_expanded == self.tools_expanded
                && cache.reasoning_expanded == self.reasoning_expanded
                && cache.theme == colors
        });
        if !cached {
            let (raw, targets) = self.history_document(colors, usize::from(history.width));
            let text = wrap_history(raw, usize::from(history.width));
            let link_regions = markdown_link_regions(&text, &targets);
            self.history_cache = Some(HistoryCache {
                width: history.width,
                visible: self.history_visible,
                tools_expanded: self.tools_expanded,
                reasoning_expanded: self.reasoning_expanded,
                theme: colors,
                text,
                links: link_regions,
            });
        }
        let contents = &self.history_cache.as_ref().unwrap().text;
        let rows = contents.lines.len();
        let max_scroll = rows.saturating_sub(history.height as usize);
        self.history_max_scroll = max_scroll;
        if let (Some(y), Some(offset)) = (self.scrollbar_pointer_y, self.scrollbar_drag) {
            let height = self.scrollbar_area.height as usize;
            let thumb = if rows == 0 {
                height
            } else {
                (height.saturating_mul(height) / rows).clamp(1, height.max(1))
            };
            let track = height.saturating_sub(thumb);
            let from_track_start = (y.saturating_sub(self.scrollbar_area.y) as usize)
                .saturating_sub(offset)
                .min(track);
            let top = if track > 0 {
                from_track_start * max_scroll / track
            } else {
                0
            };
            self.scroll_from_bottom = max_scroll.saturating_sub(top);
            self.scroll_anchor = (self.scroll_from_bottom > 0).then_some(top);
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
        self.scrollbar_area = Rect::new(history.right(), history.y, 1, history.height);
        let end_row = start_row.saturating_add(history.height as usize).min(rows);
        let viewport_lines = contents.lines[start_row..end_row].to_vec();
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

        // Copy/render just the viewport, not the entire cached transcript.
        // This also avoids u16::MAX clipping on very long conversations.
        frame.render_widget(
            Paragraph::new(Text::from(viewport_lines))
                .style(Style::default().fg(colors.text).bg(colors.background)),
            history,
        );
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

        // One quiet input surface: a divider, a colored prompt marker and a
        // multiline editor. No nested rectangular conversation/editor boxes.
        let composer = layout[2];
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
                "Attach file path…".to_owned()
            } else {
                self.attach_path.clone()
            })
            .style(Style::default().fg(colors.text))
        } else if self.editor.text().is_empty() {
            let hint = if self.running {
                "Steer this task, or /queue another…"
            } else {
                "Ask anything, or describe a coding task…"
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
        if editor_inner.width > 0 && editor_inner.height > 0 {
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
            layout[3].x.saturating_add(2),
            layout[3].y,
            layout[3].width.saturating_sub(4),
            1,
        );
        let status_line = self.footer_status(status_area.width as usize);
        frame.render_widget(
            Paragraph::new(status_line).style(Style::default().fg(colors.muted)),
            status_area,
        );
        let hovered = self.hover_pointer.and_then(|(x, y)| self.link_at(x, y));
        let footer = if !self.notification.is_empty() {
            excerpt(&self.notification, usize::from(status_area.width))
        } else if let Some(target) = hovered {
            format!(
                "Ctrl+Click to open · {}",
                compact_line(target, (status_area.width as usize).saturating_sub(22))
            )
        } else if self.scroll_anchor.is_some() {
            "↑ Reading earlier output · Ctrl+End to latest".into()
        } else if self.running {
            "Enter steer · Ctrl+C cancel · /queue follow-up".into()
        } else if status_area.width < 50 {
            "Enter send · /help".into()
        } else {
            "Enter send · /help · Ctrl+R reasoning · Ctrl+O tools".into()
        };
        frame.render_widget(
            Paragraph::new(footer).style(Style::default().fg(if self.notification.is_empty() {
                colors.muted
            } else {
                colors.accent
            })),
            Rect::new(
                status_area.x,
                status_area.y.saturating_add(1),
                status_area.width,
                1,
            ),
        );
        if let Some(picker) = &self.picker {
            picker.draw(frame, history, colors);
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
        assert!(visible.contains("Thinking…"));
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
                .filter(|line| line.contains("cargo test"))
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
    fn widescreen_uses_available_terminal_width_instead_of_a_hundred_columns() {
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
        assert!(
            state.selection_area.width >= 200,
            "maximized terminals should not be artificially limited to 100 columns"
        );
        assert!(state.selection_rows.join("\n").contains("cargo test"));
        let small = reading_area(Rect::new(0, 0, 40, 20));
        assert_eq!(small.width, 40);
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
            let expected_context = if width >= 80 {
                "Ctx 32k / 128k · 25%"
            } else {
                "Ctx 25%"
            };
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
        let wrapped = wrap_history(text, 25);
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
        let wrapped = wrap_history(Text::from(vec![line]), 8);
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
        for (width, expected) in [(110, "Ctx 100k / 500k · 20%"), (40, "Ctx 20%")] {
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
}
