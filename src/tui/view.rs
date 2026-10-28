use super::{
    attachments::{Attachment, MAX_ATTACHMENTS},
    editor::Editor,
    markdown,
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
use unicode_width::UnicodeWidthChar;

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
            let mut chars = span.content.chars().peekable();
            let mut preceding_ascii_word = false;
            while let Some(ch) = chars.next() {
                let cells = UnicodeWidthChar::width(ch).unwrap_or(0);
                // Move a short ASCII token as a unit to the next row instead
                // of splitting `run_with_resume` or `cargo test` arbitrarily.
                if ascii_word(ch) && !preceding_ascii_word && columns > hanging_indent {
                    let word_length =
                        1 + chars.clone().take_while(|next| ascii_word(*next)).count();
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
                if ch == ' ' && columns == hanging_indent {
                    continue;
                }
                segment.push(ch);
                columns = columns.saturating_add(cells);
                preceding_ascii_word = ascii_word(ch);
            }
            if !segment.is_empty() {
                spans.push(Span::styled(segment, style));
            }
        }
        rows.push(Line::from(spans));
    }
    Text::from(rows)
}

/// A comfortable reading measure on large terminals, while leaving narrow
/// terminals almost full-width. No independent layout engine is necessary.
fn reading_area(area: Rect) -> Rect {
    let margin = if area.width >= 100 {
        4
    } else if area.width >= 55 {
        2
    } else {
        1
    };
    let width = area.width.saturating_sub(margin * 2).min(100);
    Rect::new(
        area.x + (area.width - width) / 2,
        area.y,
        width,
        area.height,
    )
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

struct HistoryCache {
    width: u16,
    visible: usize,
    tools_expanded: bool,
    reasoning_expanded: bool,
    theme: Theme,
    text: Text<'static>,
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
    pub usage: crate::usage::UsageMeter,
    pub attachments: Vec<Attachment>,
    removed_drafts: Vec<Attachment>,
    saved_draft_attachments: Option<Vec<Attachment>>,
    pub attach_mode: bool,
    pub attach_path: String,
    next_marker: u32,
    pub themes: ThemeStore,
    pub session: String,
    pub model: String,
    pub model_profile: Option<String>,
    pub model_catalog: Option<crate::sessions::SessionCatalog>,
    history_visible: usize,
    history_max_scroll: usize,
    history_cache: Option<HistoryCache>,
    selection_rows: Vec<String>,
    selection_area: Rect,
    selection_start: Option<(u16, u16)>,
    selection_end: Option<(u16, u16)>,
    entries: Vec<Entry>,
    pub running: bool,
    pub notification: String,
    pub scroll_from_bottom: usize,
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
            session,
            entries: Vec::new(),
            running: false,
            notification: String::new(),
            scroll_from_bottom: 0,
            viewport_height: 0,
            tools_expanded: false,
            reasoning_expanded: true,
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

    pub fn note(&mut self, text: impl Into<String>) {
        self.notification = text.into();
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
        self.history_visible = 300;
    }

    pub fn event(&mut self, event: UiEvent) {
        self.history_cache = None;
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
        self.selection_start = None;
        self.selection_end = None;
        self.scroll_from_bottom = if amount.is_positive() {
            self.scroll_from_bottom.saturating_add(amount as usize)
        } else {
            self.scroll_from_bottom
                .saturating_sub(amount.unsigned_abs())
        };
        if amount > 0
            && self.history_visible < self.entries.len()
            && self.scroll_from_bottom >= self.history_max_scroll.saturating_sub(35)
        {
            self.history_visible = (self.history_visible + 200).min(self.entries.len());
            self.history_cache = None;
        }
    }

    pub fn select_start(&mut self, column: u16, row: u16) {
        if self.selection_area.contains((column, row).into()) {
            self.selection_start = Some((column, row));
            self.selection_end = Some((column, row));
        }
    }

    pub fn select_drag(&mut self, column: u16, row: u16) {
        if self.selection_start.is_some() {
            self.selection_end = Some((column, row));
        }
    }

    pub fn selected_text(&self) -> Option<String> {
        let (start, end) = (self.selection_start?, self.selection_end?);
        let (start, end) = if (start.1, start.0) <= (end.1, end.0) {
            (start, end)
        } else {
            (end, start)
        };
        if !self.selection_area.contains(start.into()) {
            return None;
        }
        let area = self.selection_area;
        let mut copied = Vec::new();
        for y in start.1..=end.1.min(area.bottom().saturating_sub(1)) {
            let Some(line) = self.selection_rows.get((y - area.y) as usize) else {
                continue;
            };
            let first = if y == start.1 {
                start.0.saturating_sub(area.x) as usize
            } else {
                0
            };
            let last = if y == end.1 {
                end.0.saturating_sub(area.x) as usize + 1
            } else {
                area.width as usize
            };
            let mut selected = String::new();
            let mut column = 0;
            for ch in line.chars() {
                let width = UnicodeWidthChar::width(ch).unwrap_or(0).max(1);
                if column < last && column + width > first {
                    selected.push(ch);
                }
                column += width;
            }
            copied.push(selected.trim_end().to_owned());
        }
        let text = copied.join("\n");
        (!text.trim().is_empty()).then_some(text)
    }

    fn history_text(&self, theme: Theme, width: usize) -> Text<'static> {
        let mut lines = Vec::new();
        if self.entries.is_empty() {
            lines.extend([Line::default(), Line::default(), Line::default()]);
            lines.push(Line::from(Span::styled(
                "   ◆  Ready when you are.",
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::from(Span::styled(
                "      A small, focused workspace for big ideas.",
                Style::default().fg(theme.text),
            )));
            lines.push(Line::default());
            lines.push(Line::from(Span::styled(
                "      Try  ·  Review this project",
                Style::default().fg(theme.muted),
            )));
            lines.push(Line::from(Span::styled(
                "           Find and fix a failing test",
                Style::default().fg(theme.muted),
            )));
            return Text::from(lines);
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
                        let mut rendered = markdown::render_with_width(
                            &entry.body,
                            theme,
                            width.saturating_sub(3),
                        );
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
                    lines.push(Line::from(vec![
                        Span::styled(format!("  {marker}  "), Style::default().fg(color)),
                        Span::styled(name.to_owned(), role_style(theme.tool)),
                        Span::styled(
                            format!("  {}", entry.body.replace('\n', " ")),
                            Style::default().fg(theme.muted),
                        ),
                    ]));
                    if status == "running" && !entry.details.is_empty() && !self.tools_expanded {
                        let preview = entry
                            .details
                            .lines()
                            .rev()
                            .find(|line| !line.trim().is_empty())
                            .unwrap_or("");
                        lines.push(body_line(Line::from(Span::styled(
                            format!("↳ {}", excerpt(preview, 150)),
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
                    let label = if self.reasoning_expanded {
                        if streaming {
                            "  ◌  Thinking…  ·  Ctrl+R to hide"
                        } else {
                            "  ◇  Reasoning  ·  Ctrl+R to hide"
                        }
                    } else if streaming {
                        "  ◌  Thinking…  ·  Ctrl+R to expand"
                    } else {
                        "  ◇  Reasoning  ·  Ctrl+R to expand"
                    };
                    lines.push(Line::from(Span::styled(
                        label,
                        Style::default().fg(theme.muted),
                    )));
                    if self.reasoning_expanded {
                        for line in
                            markdown::render_with_width(&entry.body, theme, width.saturating_sub(3))
                        {
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
                        lines.push(body_line(Line::from(Span::styled(
                            line.to_owned(),
                            Style::default().fg(theme.text),
                        ))));
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
        Text::from(lines)
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
        // Brand, context and state have a single well-defined hierarchy.
        let header_left = Line::from(vec![
            Span::styled("◆ ", Style::default().fg(colors.accent)),
            Span::styled(
                "Kroma",
                Style::default()
                    .fg(colors.text)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("   /  {}", excerpt(&self.session, 20)),
                Style::default().fg(colors.muted),
            ),
        ]);
        let header_line = Rect::new(layout[0].x, layout[0].y, layout[0].width, 1);
        frame.render_widget(Paragraph::new(header_left), header_line);
        // Hide secondary metadata on tight terminals instead of letting it
        // overlap the app name.
        if layout[0].width >= 55 {
            let mut right = Vec::new();
            if layout[0].width >= 85 {
                right.push(Span::styled(
                    format!("{}  ·  ", excerpt(&self.model, 22)),
                    Style::default().fg(colors.muted),
                ));
            }
            right.push(Span::styled(status, Style::default().fg(status_color)));
            frame.render_widget(
                Paragraph::new(Line::from(right)).alignment(Alignment::Right),
                header_line,
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
            self.history_cache = Some(HistoryCache {
                width: history.width,
                visible: self.history_visible,
                tools_expanded: self.tools_expanded,
                reasoning_expanded: self.reasoning_expanded,
                theme: colors,
                text: wrap_history(
                    self.history_text(colors, usize::from(history.width)),
                    usize::from(history.width),
                ),
            });
        }
        let contents = self.history_cache.as_ref().unwrap().text.clone();
        let rows = contents.lines.len();
        let max_scroll = rows.saturating_sub(history.height as usize);
        self.history_max_scroll = max_scroll;
        self.scroll_from_bottom = self.scroll_from_bottom.min(max_scroll);
        let start_row = max_scroll.saturating_sub(self.scroll_from_bottom);
        self.selection_area = history;
        self.selection_rows = contents
            .lines
            .iter()
            .skip(start_row)
            .take(history.height as usize)
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect();
        let mut paragraph =
            Paragraph::new(contents).style(Style::default().fg(colors.text).bg(colors.background));
        paragraph = paragraph.scroll((
            max_scroll
                .saturating_sub(self.scroll_from_bottom)
                .min(u16::MAX as usize) as u16,
            0,
        ));
        frame.render_widget(paragraph, history);
        if let (Some(mut start), Some(mut end)) = (self.selection_start, self.selection_end) {
            if (start.1, start.0) > (end.1, end.0) {
                std::mem::swap(&mut start, &mut end);
            }
            for y in start.1.max(history.y)..=end.1.min(history.bottom().saturating_sub(1)) {
                let left = if y == start.1 {
                    start.0.max(history.x)
                } else {
                    history.x
                };
                let right = if y == end.1 {
                    end.0.min(history.right().saturating_sub(1))
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
        let ctx = self.context_display();
        let level = self.thinking.as_deref().unwrap_or("n/a");
        let reasoning = if status_area.width < 50 {
            format!("Think {level}")
        } else {
            format!("Think {level}  ⇧Tab")
        };
        let left = if status_area.width < 39 {
            match self.context_input {
                Some(input) if self.context_window > 0 => format!(
                    "Ctx {}%",
                    (input as f64 * 100.0 / self.context_window as f64).round() as u64
                ),
                _ => "Ctx —".into(),
            }
        } else {
            ctx
        };
        let gap = if status_area.width >= 50 {
            "    "
        } else {
            "  "
        };
        let status_line = format!("{left}{gap}{reasoning}");
        frame.render_widget(
            Paragraph::new(status_line).style(Style::default().fg(colors.muted)),
            status_area,
        );
        let footer = if !self.notification.is_empty() {
            excerpt(&self.notification, usize::from(status_area.width))
        } else if self.running {
            "Enter steer · Ctrl+C cancel · /queue follow-up".into()
        } else if status_area.width < 50 {
            "Enter send · /help · F2 theme".into()
        } else {
            "Enter send · Shift+Enter newline · /help · F2 theme".into()
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
        assert!(state.reasoning_expanded);
        state.event(UiEvent::TaskStarted);
        state.event(UiEvent::ModelStarted);
        state.event(UiEvent::ReasoningDelta("Thinking ".into()));
        assert_eq!(state.phase, "thinking");
        let mut terminal = Terminal::new(TestBackend::new(95, 28)).unwrap();
        terminal.draw(|frame| state.draw(frame)).unwrap();
        let visible = state.selection_rows.join("\n");
        assert!(visible.contains("Thinking…"));
        assert!(visible.contains("Thinking"));

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
