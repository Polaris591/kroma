//! One blocking question: the options, the toggles and a typed answer.
//! Keys are handled here so the draft editor never sees them, and the answer
//! is scoped to the single question that was asked.
use super::{preferences::Language, theme::Theme};
use crate::tools::{AskAnswer, AskRequest, Asker};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
    Frame,
};
use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
};
use tokio::sync::oneshot;

/// A waiting question and the channel that carries the answer back.
type Pending = (AskRequest, oneshot::Sender<AskAnswer>);

/// The only place a tool's question crosses into the interface. It carries the
/// request and the reply channel, so the core needs no task or event plumbing.
#[derive(Clone, Default)]
pub struct Slot {
    pending: Arc<Mutex<Option<Pending>>>,
}

impl Slot {
    pub fn new() -> Self {
        Self::default()
    }

    /// Hand a waiting question to the interface. `None` means one is already
    /// open; the caller asks again only after the person answered.
    pub fn take(&self) -> Option<Pending> {
        self.pending.lock().ok()?.take()
    }
}

impl Asker for Slot {
    fn ask<'a>(
        &'a self,
        request: AskRequest,
    ) -> Pin<Box<dyn Future<Output = Option<AskAnswer>> + Send + 'a>> {
        Box::pin(async move {
            let (answer, wait) = oneshot::channel();
            {
                // A dropped lock or an already open question means the person
                // was never asked, which is not an answer.
                let mut pending = self.pending.lock().ok()?;
                if pending.is_some() {
                    return None;
                }
                *pending = Some((request, answer));
            }
            wait.await.ok()
        })
    }
}

/// What a key did to the open question.
pub enum Key {
    /// Consumed by the question.
    Taken,
    /// Not ours; normal running-turn handling sees it (Ctrl+C stops a task).
    Pass,
    /// The person finished: send this answer and close the question.
    Closed(AskAnswer),
}

pub struct Overlay {
    question: String,
    options: Vec<String>,
    multiple: bool,
    cursor: usize,
    toggled: Vec<bool>,
    /// `Some` while the person is typing instead of choosing.
    typed: Option<String>,
    answer: Option<oneshot::Sender<AskAnswer>>,
    scroll: usize,
}

impl Overlay {
    pub fn new(request: AskRequest, answer: oneshot::Sender<AskAnswer>) -> Self {
        let toggled = vec![false; request.options.len()];
        Self {
            question: request.question,
            options: request.options,
            multiple: request.multiple,
            cursor: 0,
            toggled,
            typed: None,
            answer: Some(answer),
            scroll: 0,
        }
    }

    /// The reply channel, taken once when the question closes.
    pub fn take_answer(&mut self) -> Option<oneshot::Sender<AskAnswer>> {
        self.answer.take()
    }

    pub fn key(&mut self, key: KeyEvent) -> Key {
        // Ctrl+C keeps its existing meaning: stop the whole turn.
        if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c')) {
            return Key::Pass;
        }
        if self.typed.is_some() {
            return self.typed_key(key);
        }
        match key.code {
            KeyCode::Esc => Key::Closed(AskAnswer::Dismissed),
            KeyCode::Up => {
                self.move_by(-1);
                Key::Taken
            }
            KeyCode::Down => {
                self.move_by(1);
                Key::Taken
            }
            KeyCode::Char(' ') if self.multiple => {
                self.toggle(self.cursor);
                Key::Taken
            }
            KeyCode::Enter => {
                if self.cursor == self.options.len() {
                    self.typed = Some(String::new());
                    return Key::Taken;
                }
                if self.multiple {
                    let picked = self.picked();
                    if picked.is_empty() {
                        return Key::Taken;
                    }
                    return Key::Closed(self.answered(picked, None));
                }
                let choice = self.options[self.cursor].clone();
                Key::Closed(self.answered(vec![choice], None))
            }
            KeyCode::Char(digit) => match digit.to_digit(10) {
                Some(index) if index >= 1 && index as usize <= self.options.len() => {
                    let index = index as usize - 1;
                    if self.multiple {
                        self.cursor = index;
                        self.toggle(index);
                        Key::Taken
                    } else {
                        let choice = self.options[index].clone();
                        Key::Closed(self.answered(vec![choice], None))
                    }
                }
                _ => Key::Taken,
            },
            _ => Key::Taken,
        }
    }

    /// A pasted block only belongs to the answer being typed.
    pub fn paste(&mut self, text: &str) {
        if let Some(buffer) = self.typed.as_mut() {
            buffer.push_str(text.trim_matches(['\r', '\n']));
        }
    }

    fn typed_key(&mut self, key: KeyEvent) -> Key {
        match key.code {
            KeyCode::Esc => {
                self.typed = None;
                Key::Taken
            }
            KeyCode::Enter => {
                let text = self.typed.take().unwrap_or_default();
                if text.trim().is_empty() {
                    return Key::Taken;
                }
                Key::Closed(self.answered(self.picked(), Some(text)))
            }
            KeyCode::Backspace => {
                if let Some(buffer) = self.typed.as_mut() {
                    buffer.pop();
                }
                Key::Taken
            }
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                if let Some(buffer) = self.typed.as_mut() {
                    buffer.push(character);
                }
                Key::Taken
            }
            _ => Key::Taken,
        }
    }

    fn answered(&mut self, picked: Vec<String>, typed: Option<String>) -> AskAnswer {
        AskAnswer::Answered { picked, typed }
    }

    fn picked(&self) -> Vec<String> {
        self.options
            .iter()
            .zip(&self.toggled)
            .filter(|(_, chosen)| **chosen)
            .map(|(option, _)| option.clone())
            .collect()
    }

    fn toggle(&mut self, index: usize) {
        if let Some(chosen) = self.toggled.get_mut(index) {
            *chosen = !*chosen;
        }
    }

    fn row_count(&self) -> usize {
        self.options.len() + 1
    }

    fn move_by(&mut self, delta: isize) {
        const WINDOW: usize = 8;
        let rows = self.row_count() as isize;
        self.cursor = (self.cursor as isize + delta).rem_euclid(rows) as usize;
        if self.cursor < self.scroll {
            self.scroll = self.cursor;
        } else if self.cursor >= self.scroll + WINDOW {
            self.scroll = self.cursor + 1 - WINDOW;
        }
    }

    pub fn draw(&mut self, frame: &mut Frame, parent: Rect, theme: Theme, language: Language) {
        if parent.width < 20 || parent.height < 5 {
            // A waiting question must never be invisible: show one clipped
            // line until the terminal is large enough for the full box.
            let row = Rect::new(parent.x, parent.y, parent.width, 1);
            let line = Line::from(Span::styled(
                clip(&self.question, usize::from(parent.width).saturating_sub(2)),
                Style::default()
                    .fg(theme.accent_text)
                    .add_modifier(Modifier::BOLD),
            ));
            frame.render_widget(Clear, row);
            frame.render_widget(Paragraph::new(line), row);
            return;
        }
        const WINDOW: usize = 8;
        let width = parent.width.saturating_sub(4).clamp(20, 72);
        let inner_width = width.saturating_sub(4) as usize;
        let title = language.text("Question", "请选择");
        let mut lines: Vec<Line> = Vec::new();
        for row in wrap(&self.question, inner_width) {
            lines.push(Line::from(Span::styled(
                row,
                Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
            )));
        }
        lines.push(Line::default());
        let start = self.scroll.min(self.options.len());
        let end = (start + WINDOW).min(self.row_count());
        let mut typed_row = None;
        for index in start..end {
            if index == self.options.len() {
                typed_row = Some(lines.len());
            }
            lines.push(self.row_line(index, inner_width, theme, language));
        }
        lines.push(Line::default());
        let hint = if self.typed.is_some() {
            language.text("Enter submits · Esc goes back", "Enter 提交 · Esc 返回")
        } else if self.multiple {
            language.text(
                "↑↓ move · Space toggles · Enter confirms · Esc closes",
                "↑↓ 移动 · 空格打勾 · Enter 确认 · Esc 关闭",
            )
        } else {
            language.text(
                "↑↓ move · number picks · Enter confirms · Esc closes",
                "↑↓ 移动 · 数字直选 · Enter 确认 · Esc 关闭",
            )
        };
        lines.push(Line::from(Span::styled(
            hint,
            Style::default().fg(theme.muted),
        )));
        let height = (lines.len() as u16 + 2).min(parent.height);
        let area = Rect::new(
            parent.x + parent.width.saturating_sub(width) / 2,
            parent.y + parent.height.saturating_sub(height) / 2,
            width,
            height,
        );
        frame.render_widget(Clear, area);
        let block = Block::default()
            .borders(Borders::ALL)
            .title(Span::styled(
                format!(" {title} "),
                Style::default().fg(theme.accent_text),
            ))
            .style(Style::default().bg(theme.overlay));
        frame.render_widget(
            Paragraph::new(lines)
                .block(block)
                .wrap(Wrap { trim: false }),
            area,
        );
        // Typing needs a real caret position so an IME candidate window opens
        // next to the text, exactly like the composer.
        if let (Some(buffer), Some(row)) = (self.typed.as_ref(), typed_row) {
            let y = area.y.saturating_add(1 + row as u16);
            let column = 3
                + clipped_width(buffer, inner_width)
                + u16::from(self.multiple) * 4
                + u16::from(self.options.len() > 9) * 3;
            if y + 1 < area.bottom() {
                frame.set_cursor_position((area.x.saturating_add(column).min(area.right() - 2), y));
            }
        }
    }

    fn row_line(
        &self,
        index: usize,
        inner_width: usize,
        theme: Theme,
        language: Language,
    ) -> Line<'static> {
        let active = index == self.cursor;
        let marker = if active { "› " } else { "  " };
        let label = if index == self.options.len() {
            match &self.typed {
                Some(buffer) => buffer.clone(),
                None => language
                    .text("Other (type your own)", "其他（自己输入）")
                    .to_owned(),
            }
        } else {
            self.options[index].clone()
        };
        let check = if self.multiple {
            if self.toggled.get(index).copied().unwrap_or(false) {
                "[x] "
            } else {
                "[ ] "
            }
        } else {
            ""
        };
        let number = if index < self.options.len() {
            format!("{}. ", index + 1)
        } else {
            String::new()
        };
        let background = if active {
            Theme::blend(theme.accent, theme.overlay, 0.04)
        } else {
            theme.overlay
        };
        let weight = if active {
            Modifier::BOLD
        } else {
            Modifier::empty()
        };
        Line::from(vec![
            Span::styled(marker, Style::default().fg(theme.accent_text)),
            Span::styled(
                clip(
                    &format!("{check}{number}{label}"),
                    inner_width.saturating_sub(2),
                ),
                Style::default().fg(theme.text).add_modifier(weight),
            ),
        ])
        .style(Style::default().bg(background))
    }
}

/// Break the question into rows that fit the box: it is the one part a person
/// must read in full, so it is never clipped to a single line.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(8);
    let mut rows = Vec::new();
    for source in text.lines() {
        let mut row = String::new();
        for word in source.split_whitespace() {
            let projected = row.chars().count() + 1 + word.chars().count();
            if projected > width && !row.is_empty() {
                rows.push(std::mem::take(&mut row));
            }
            if !row.is_empty() {
                row.push(' ');
            }
            row.push_str(word);
        }
        rows.push(row);
        if rows.len() >= 6 {
            break;
        }
    }
    if rows.is_empty() {
        rows.push(String::new());
    }
    rows.truncate(6);
    rows
}

fn clip(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_owned();
    }
    let mut clipped: String = text.chars().take(width.saturating_sub(1)).collect();
    clipped.push('…');
    clipped
}

fn clipped_width(text: &str, width: usize) -> u16 {
    text.chars()
        .count()
        .min(width.saturating_sub(2))
        .min(u16::MAX as usize) as u16
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{FunctionCall, ToolCall};
    use crate::tools::{AskTool, Tool, ToolContext};
    use crossterm::event::KeyCode;
    use std::path::Path;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn overlay(multiple: bool) -> Overlay {
        let (sender, _wait) = oneshot::channel();
        Overlay::new(
            AskRequest {
                question: "Which database?".into(),
                options: vec!["Postgres".into(), "SQLite".into(), "MySQL".into()],
                multiple,
            },
            sender,
        )
    }

    fn closed(outcome: Key) -> AskAnswer {
        match outcome {
            Key::Closed(answer) => answer,
            _ => panic!("expected the question to close"),
        }
    }

    #[test]
    fn one_key_or_one_enter_settles_a_single_choice() {
        let mut question = overlay(false);
        assert_eq!(
            closed(question.key(key(KeyCode::Enter))),
            AskAnswer::Answered {
                picked: vec!["Postgres".into()],
                typed: None
            }
        );
        let mut question = overlay(false);
        assert_eq!(
            closed(question.key(key(KeyCode::Char('3')))),
            AskAnswer::Answered {
                picked: vec!["MySQL".into()],
                typed: None
            }
        );
    }

    #[test]
    fn space_toggles_and_enter_confirms_in_option_order() {
        let mut question = overlay(true);
        assert!(matches!(question.key(key(KeyCode::Enter)), Key::Taken));
        assert!(matches!(question.key(key(KeyCode::Char(' '))), Key::Taken));
        assert!(matches!(question.key(key(KeyCode::Down)), Key::Taken));
        assert!(matches!(question.key(key(KeyCode::Down)), Key::Taken));
        assert!(matches!(question.key(key(KeyCode::Char(' '))), Key::Taken));
        assert_eq!(
            closed(question.key(key(KeyCode::Enter))),
            AskAnswer::Answered {
                picked: vec!["Postgres".into(), "MySQL".into()],
                typed: None
            }
        );
    }

    #[test]
    fn esc_closes_without_an_answer_and_ctrl_c_still_stops_the_turn() {
        let mut question = overlay(false);
        assert_eq!(
            closed(question.key(key(KeyCode::Esc))),
            AskAnswer::Dismissed
        );
        let mut question = overlay(false);
        assert!(matches!(
            question.key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Key::Pass
        ));
    }

    #[test]
    fn a_typed_answer_replaces_the_choices() {
        let mut question = overlay(false);
        for _ in 0..3 {
            assert!(matches!(question.key(key(KeyCode::Down)), Key::Taken));
        }
        assert!(matches!(question.key(key(KeyCode::Enter)), Key::Taken));
        for character in ["s", "t", "a", "g", "i", "n", "g"] {
            assert!(matches!(
                question.key(key(KeyCode::Char(character.chars().next().unwrap()))),
                Key::Taken
            ));
        }
        assert_eq!(
            closed(question.key(key(KeyCode::Enter))),
            AskAnswer::Answered {
                picked: Vec::new(),
                typed: Some("staging".into())
            }
        );
        // Esc while typing goes back to the list instead of closing.
        let mut question = overlay(false);
        for _ in 0..3 {
            assert!(matches!(question.key(key(KeyCode::Down)), Key::Taken));
        }
        assert!(matches!(question.key(key(KeyCode::Enter)), Key::Taken));
        assert!(question.typed.is_some());
        assert!(matches!(question.key(key(KeyCode::Esc)), Key::Taken));
        assert!(question.typed.is_none());
        assert!(matches!(question.key(key(KeyCode::Enter)), Key::Taken));
        assert!(question.typed.is_some());
    }

    fn context<'a>(ask: Option<&'a dyn Asker>) -> ToolContext<'a> {
        ToolContext {
            model_connection: None,
            user_message: None,
            user_turn: None,
            user_images: None,
            previous_user_images: None,
            workdir: Path::new("."),
            archive: Path::new("kroma-ask-test.json"),
            on_output: None,
            ask,
        }
    }

    fn call() -> ToolCall {
        ToolCall {
            id: "call-ask".into(),
            kind: "function".into(),
            function: FunctionCall {
                name: "ask".into(),
                arguments: r#"{"question":"Which database?","options":["Postgres","SQLite"]}"#
                    .into(),
            },
        }
    }

    #[tokio::test]
    async fn a_tool_waits_until_the_interface_answers() {
        let slot = Slot::new();
        let asker: Arc<dyn Asker> = Arc::new(slot.clone());
        let tool = AskTool::new("ask", "Ask the user to choose");
        let call = call();
        let mut pending = Box::pin(tool.execute(&call, context(Some(asker.as_ref()))));
        tokio::select! {
            result = &mut pending => panic!("the tool answered itself: {}", result.content),
            _ = tokio::time::sleep(std::time::Duration::from_millis(20)) => {}
        }
        let (request, answer) = slot.take().expect("the question reached the interface");
        assert_eq!(request.question, "Which database?");
        answer
            .send(AskAnswer::Answered {
                picked: vec!["SQLite".into()],
                typed: None,
            })
            .expect("the waiting tool accepts one answer");
        let result = pending.await;
        assert!(!result.is_error);
        assert_eq!(result.content, "User selected: SQLite");
    }

    #[tokio::test]
    async fn a_question_nobody_can_show_ends_with_a_failure_not_a_wait() {
        let slot = Slot::new();
        let asker: Arc<dyn Asker> = Arc::new(slot.clone());
        let tool = AskTool::new("ask", "Ask the user to choose");
        let call = call();
        let mut pending = Box::pin(tool.execute(&call, context(Some(asker.as_ref()))));
        tokio::select! {
            result = &mut pending => panic!("the tool answered itself: {}", result.content),
            _ = tokio::time::sleep(std::time::Duration::from_millis(20)) => {}
        }
        drop(slot.take().expect("the question reached the interface"));
        let result = pending.await;
        assert!(result.is_error);
        assert!(result.content.contains("could not be shown"));
    }
}
