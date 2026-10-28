//! Small Unicode-aware multiline editor. No new UI framework or modal engine.
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::Value;
use std::collections::HashMap;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// Single private-use scalar per attachment. It behaves like one character
/// to the caret/backspace, but renders as a readable inline chip.
pub const ATTACHMENT_START: u32 = 0xE000;
pub const ATTACHMENT_END: u32 = 0xF8FF;

#[derive(Clone, Default)]
struct HistoryEntry {
    text: String,
    labels: HashMap<char, String>,
    attachments: Vec<Value>,
}

#[derive(Clone)]
struct EditSnapshot {
    text: String,
    caret: usize,
    labels: HashMap<char, String>,
}

#[derive(Default)]
pub struct Editor {
    text: String,
    caret: usize, // Unicode char index on a grapheme boundary, never a byte index
    attachment_labels: HashMap<char, String>,
    history: Vec<HistoryEntry>,
    history_index: Option<usize>,
    history_draft: HistoryEntry,
    undo: Vec<EditSnapshot>,
    redo: Vec<EditSnapshot>,
}

pub enum EditAction {
    None,
    Submit(String),
}

impl Editor {
    pub fn replace_all(&mut self, text: &str) {
        self.checkpoint();
        self.text = text.to_owned();
        self.caret = self.text.chars().count();
        self.history_index = None;
    }
    fn snapshot(&self) -> EditSnapshot {
        EditSnapshot {
            text: self.text.clone(),
            caret: self.caret,
            labels: self.attachment_labels.clone(),
        }
    }
    fn checkpoint(&mut self) {
        if self.undo.len() >= 100 {
            self.undo.remove(0);
        }
        self.undo.push(self.snapshot());
        self.redo.clear();
    }
    fn restore_snapshot(&mut self, snapshot: EditSnapshot) {
        self.text = snapshot.text;
        self.caret = snapshot.caret;
        self.attachment_labels = snapshot.labels;
        self.history_index = None;
    }
    pub fn undo(&mut self) {
        if let Some(previous) = self.undo.pop() {
            self.redo.push(self.snapshot());
            self.restore_snapshot(previous);
        }
    }
    pub fn redo(&mut self) {
        if let Some(next) = self.redo.pop() {
            self.undo.push(self.snapshot());
            self.restore_snapshot(next);
        }
    }
    pub fn word_before_caret(&self) -> String {
        let left = self.text.chars().take(self.caret).collect::<String>();
        if left.chars().last().is_some_and(char::is_whitespace) {
            return String::new();
        }
        left.split_whitespace().last().unwrap_or("").to_owned()
    }
    pub fn replace_word_before_caret(&mut self, with: &str) {
        let word = self.word_before_caret();
        self.checkpoint();
        let start = self.byte_index(self.caret.saturating_sub(word.chars().count()));
        let end = self.byte_index(self.caret);
        self.text.replace_range(start..end, with);
        self.caret = self.caret - word.chars().count() + with.chars().count();
    }
    pub fn text(&self) -> &str {
        &self.text
    }

    fn byte_index(&self, char_index: usize) -> usize {
        self.text
            .char_indices()
            .nth(char_index)
            .map_or(self.text.len(), |(i, _)| i)
    }

    fn previous_grapheme(&self) -> usize {
        let position = self.byte_index(self.caret);
        self.text
            .grapheme_indices(true)
            .take_while(|(index, _)| *index < position)
            .last()
            .map_or(0, |(index, _)| self.text[..index].chars().count())
    }

    fn next_grapheme(&self) -> usize {
        let position = self.byte_index(self.caret);
        self.text
            .grapheme_indices(true)
            .find(|(index, grapheme)| index + grapheme.len() > position)
            .map_or(self.text.chars().count(), |(index, grapheme)| {
                self.text[..index + grapheme.len()].chars().count()
            })
    }

    fn visual_width(&self, start: usize, end: usize) -> usize {
        self.text[self.byte_index(start)..self.byte_index(end)]
            .graphemes(true)
            .map(|part| {
                part.chars()
                    .next()
                    .and_then(|ch| self.attachment_labels.get(&ch))
                    .filter(|_| part.chars().count() == 1)
                    .map_or_else(
                        || UnicodeWidthStr::width(part),
                        |label| UnicodeWidthStr::width(label.as_str()),
                    )
            })
            .sum()
    }

    fn caret_at_column(&self, start: usize, end: usize, target: usize) -> usize {
        let mut caret = start;
        let mut column = 0;
        for grapheme in self.text[self.byte_index(start)..self.byte_index(end)].graphemes(true) {
            let width = grapheme
                .chars()
                .next()
                .and_then(|ch| self.attachment_labels.get(&ch))
                .filter(|_| grapheme.chars().count() == 1)
                .map_or_else(
                    || UnicodeWidthStr::width(grapheme),
                    |label| UnicodeWidthStr::width(label.as_str()),
                );
            if column + width > target {
                break;
            }
            column += width;
            caret += grapheme.chars().count();
        }
        caret
    }

    fn insert(&mut self, input: &str) {
        self.checkpoint();
        let byte = self.byte_index(self.caret);
        self.text.insert_str(byte, input);
        self.caret += input.chars().count();
        self.history_index = None;
    }

    pub fn paste(&mut self, text: &str) {
        // Text from the OS clipboard cannot impersonate a draft's attachment.
        let clean = text
            .replace("\r\n", "\n")
            .replace('\r', "\n")
            .chars()
            .map(|c| {
                if (ATTACHMENT_START..=ATTACHMENT_END).contains(&(c as u32)) {
                    '�'
                } else {
                    c
                }
            })
            .collect::<String>();
        self.insert(&clean);
    }

    pub fn insert_attachment(&mut self, marker: char, label: String) {
        self.insert(&marker.to_string());
        self.attachment_labels
            .insert(marker, format!("【{label}】"));
    }

    #[cfg(test)]
    pub fn visible_text(&self) -> String {
        self.display_text(&self.text)
    }

    #[cfg(test)]
    fn display_text(&self, raw: &str) -> String {
        let mut result = String::new();
        for ch in raw.chars() {
            if let Some(label) = self.attachment_labels.get(&ch) {
                result.push_str(label);
            } else {
                result.push(ch);
            }
        }
        result
    }

    pub fn contains_attachment(&self, marker: char) -> bool {
        self.text.contains(marker)
    }

    pub fn history_index(&self) -> Option<usize> {
        self.history_index
    }

    pub fn history_attachments(&self) -> &[Value] {
        self.history_index
            .and_then(|index| self.history.get(index))
            .map_or(&[], |entry| entry.attachments.as_slice())
    }

    /// Bind file references to the just-sent history entry, not merely the
    /// label text. Only actual Agent requests call this (not local commands).
    pub fn remember_attachments(&mut self, attachments: Vec<Value>) {
        if let Some(entry) = self.history.last_mut() {
            entry.attachments = attachments;
        }
    }

    /// Seed editor recall on startup or session switch. The WAL remains the
    /// source of truth; no binary bytes are copied into the editor history.
    pub fn restore_history(&mut self, entries: Vec<(String, HashMap<char, String>, Vec<Value>)>) {
        self.history = entries
            .into_iter()
            .map(|(text, labels, attachments)| HistoryEntry {
                text,
                labels,
                attachments,
            })
            .collect();
        self.history_index = None;
        self.history_draft = HistoryEntry::default();
        self.undo.clear();
        self.redo.clear();
    }

    pub fn take(&mut self) -> Option<String> {
        if self.text.trim().is_empty() {
            return None;
        }
        let text = std::mem::take(&mut self.text);
        self.caret = 0;
        self.history_index = None;
        self.history_draft = HistoryEntry::default();
        let labels = std::mem::take(&mut self.attachment_labels);
        self.attachment_labels.clear();
        if self
            .history
            .last()
            .is_none_or(|last| last.text != text || last.labels != labels || !labels.is_empty())
        {
            self.history.push(HistoryEntry {
                text: text.clone(),
                labels,
                attachments: Vec::new(),
            });
        }
        Some(text)
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.attachment_labels.clear();
        self.caret = 0;
        self.history_index = None;
        self.undo.clear();
        self.redo.clear();
    }

    fn line_start(&self) -> usize {
        self.text
            .chars()
            .take(self.caret)
            .collect::<String>()
            .rfind('\n')
            .map_or(0, |byte| {
                self.text[..self.byte_index(self.caret)][..=byte]
                    .chars()
                    .count()
            })
    }

    fn line_end(&self) -> usize {
        let chars: Vec<char> = self.text.chars().collect();
        (self.caret..chars.len())
            .find(|&i| chars[i] == '\n')
            .unwrap_or(chars.len())
    }

    fn move_vertical(&mut self, up: bool) {
        let chars: Vec<char> = self.text.chars().collect();
        let start = self.line_start();
        let column = self.visual_width(start, self.caret);
        if up {
            if start == 0 {
                self.history_step(true);
                return;
            }
            let previous_end = start - 1;
            let previous_start = chars[..previous_end]
                .iter()
                .rposition(|&c| c == '\n')
                .map_or(0, |i| i + 1);
            self.caret = self.caret_at_column(previous_start, previous_end, column);
        } else {
            let end = self.line_end();
            if end == chars.len() {
                self.history_step(false);
                return;
            }
            let next_start = end + 1;
            let next_end = chars[next_start..]
                .iter()
                .position(|&c| c == '\n')
                .map_or(chars.len(), |i| next_start + i);
            self.caret = self.caret_at_column(next_start, next_end, column);
        }
    }

    fn history_step(&mut self, previous: bool) {
        if previous {
            if self.history.is_empty() {
                return;
            }
            let next = self
                .history_index
                .map_or(self.history.len() - 1, |i| i.saturating_sub(1));
            if self.history_index.is_none() {
                self.history_draft = HistoryEntry {
                    text: self.text.clone(),
                    labels: self.attachment_labels.clone(),
                    attachments: Vec::new(),
                };
            }
            self.history_index = Some(next);
            self.text = self.history[next].text.clone();
            self.attachment_labels = self.history[next].labels.clone();
        } else if let Some(index) = self.history_index {
            if index + 1 == self.history.len() {
                self.history_index = None;
                self.text = self.history_draft.text.clone();
                self.attachment_labels = self.history_draft.labels.clone();
            } else {
                self.history_index = Some(index + 1);
                self.text = self.history[index + 1].text.clone();
                self.attachment_labels = self.history[index + 1].labels.clone();
            }
        }
        self.caret = self.text.chars().count();
    }

    pub fn key(&mut self, key: KeyEvent) -> EditAction {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('z') if ctrl => self.undo(),
            KeyCode::Char('y') if ctrl => self.redo(),
            KeyCode::Enter
                if key.modifiers.intersects(
                    KeyModifiers::SHIFT | KeyModifiers::ALT | KeyModifiers::CONTROL,
                ) =>
            {
                self.insert("\n")
            }
            KeyCode::Enter => return self.take().map_or(EditAction::None, EditAction::Submit),
            KeyCode::Char('j') if ctrl => self.insert("\n"),
            KeyCode::Char('a') if ctrl => self.caret = self.line_start(),
            KeyCode::Char('e') if ctrl => self.caret = self.line_end(),
            KeyCode::Char('u') if ctrl => {
                self.checkpoint();
                let start = self.line_start();
                let begin = self.byte_index(start);
                let end = self.byte_index(self.caret);
                self.text.replace_range(begin..end, "");
                self.caret = start;
            }
            KeyCode::Char('k') if ctrl => {
                self.checkpoint();
                let end = self.line_end();
                let begin = self.byte_index(self.caret);
                self.text.replace_range(begin..self.byte_index(end), "");
            }
            KeyCode::Char(c) if !ctrl && !key.modifiers.contains(KeyModifiers::ALT) => {
                self.insert(&c.to_string())
            }
            KeyCode::Tab => self.insert("    "),
            KeyCode::Backspace if self.caret > 0 => {
                self.checkpoint();
                let previous = self.previous_grapheme();
                self.text
                    .replace_range(self.byte_index(previous)..self.byte_index(self.caret), "");
                self.caret = previous;
            }
            KeyCode::Delete if self.caret < self.text.chars().count() => {
                self.checkpoint();
                self.text.replace_range(
                    self.byte_index(self.caret)..self.byte_index(self.next_grapheme()),
                    "",
                );
            }
            KeyCode::Left => self.caret = self.previous_grapheme(),
            KeyCode::Right => self.caret = self.next_grapheme(),
            KeyCode::Home => self.caret = self.line_start(),
            KeyCode::End => self.caret = self.line_end(),
            KeyCode::Up => self.move_vertical(true),
            KeyCode::Down => self.move_vertical(false),
            _ => {}
        }
        EditAction::None
    }

    /// Render only the portion of the editor surrounding its cursor, including
    /// soft-wrapped lines. Width is measured in terminal display cells.
    pub fn viewport(&self, width: u16, height: u16) -> (String, u16, u16) {
        let width = usize::from(width.max(1));
        let height = usize::from(height.max(1));
        let mut rows = vec![String::new()];
        let mut col = 0usize;
        let mut cursor = (0usize, 0usize);
        let mut char_index = 0;
        for grapheme in self.text.graphemes(true) {
            if grapheme == "\n" {
                if char_index == self.caret {
                    cursor = (rows.len() - 1, col);
                }
                rows.push(String::new());
                col = 0;
                char_index += 1;
                continue;
            }
            let label = grapheme
                .chars()
                .next()
                .and_then(|ch| self.attachment_labels.get(&ch))
                .filter(|_| grapheme.chars().count() == 1);
            let displayed = label.map_or(grapheme, String::as_str);
            let cells = UnicodeWidthStr::width(displayed);
            if col + cells > width && col > 0 {
                rows.push(String::new());
                col = 0;
            }
            if char_index == self.caret {
                cursor = (rows.len() - 1, col);
            }
            rows.last_mut().unwrap().push_str(displayed);
            col += cells;
            char_index += grapheme.chars().count();
        }
        if self.caret == self.text.chars().count() {
            cursor = (rows.len() - 1, col);
        }
        // A cursor at column=width belongs on the next terminal row.
        if cursor.1 >= width {
            rows.push(String::new());
            cursor = (rows.len() - 1, 0);
        }
        let start = cursor.0.saturating_sub(height - 1);
        let end = rows.len().min(start + height);
        (
            rows[start..end].join("\n"),
            cursor.1 as u16,
            (cursor.0 - start) as u16,
        )
    }

    pub fn display_rows(&self, width: u16) -> u16 {
        let (visible, _, _) = self.viewport(width, u16::MAX);
        visible.split('\n').count().clamp(1, 8) as u16
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn grapheme_navigation_and_deletion_keep_emoji_and_combining_marks_intact() {
        let mut editor = Editor::default();
        editor.paste("a e\u{301} 👩‍💻 你好");
        editor.key(key(KeyCode::Home));
        editor.key(key(KeyCode::Right));
        editor.key(key(KeyCode::Right));
        editor.key(key(KeyCode::Delete));
        assert_eq!(
            editor.text(),
            "a  👩‍💻 你好",
            "combining mark and letter delete together"
        );
        editor.key(key(KeyCode::Right));
        editor.key(key(KeyCode::Backspace));
        assert_eq!(editor.text(), "a 👩‍💻 你好");
        editor.key(key(KeyCode::Delete));
        assert_eq!(editor.text(), "a  你好", "ZWJ emoji deletes as one unit");
        editor.key(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::CONTROL));
        assert!(editor.text().contains("👩‍💻"), "undo restores full grapheme");
        let (text, col, row) = editor.viewport(12, 4);
        assert!(text.contains("👩‍💻"));
        assert!(col < 12 && row < 4);
    }

    #[test]
    fn unicode_vertical_motion_matches_cell_columns_not_codepoints() {
        let mut editor = Editor::default();
        editor.paste("中文ab\nabcde");
        editor.key(key(KeyCode::Home));
        for _ in 0..3 {
            editor.key(key(KeyCode::Right));
        }
        editor.key(key(KeyCode::Up));
        editor.key(key(KeyCode::Char('!')));
        assert_eq!(editor.text(), "中!文ab\nabcde");
    }

    #[test]
    fn unicode_edit_history_and_multiline() {
        let mut editor = Editor::default();
        editor.paste("你好🙂\r\nworld");
        editor.key(key(KeyCode::Left));
        editor.key(key(KeyCode::Backspace));
        assert_eq!(editor.text(), "你好🙂\nword");
        let (lines, _, _) = editor.viewport(20, 5);
        assert!(lines.contains("你好🙂"));
        let EditAction::Submit(s) = editor.key(key(KeyCode::Enter)) else {
            panic!("submit");
        };
        assert_eq!(s, "你好🙂\nword");
        editor.key(key(KeyCode::Up));
        assert_eq!(editor.text(), s);
    }

    #[test]
    fn wrapped_cursor_stays_visible_and_blank_is_not_sent() {
        let mut editor = Editor::default();
        assert!(matches!(editor.key(key(KeyCode::Enter)), EditAction::None));
        editor.paste("abc中文def");
        let (_, col, row) = editor.viewport(4, 2);
        assert!(col < 4 && row < 2);
    }

    #[test]
    fn inline_attachment_is_atomic_and_keeps_text_order() {
        let mut editor = Editor::default();
        let token = char::from_u32(ATTACHMENT_START).unwrap();
        editor.paste("你看看怎么样");
        for _ in 0..3 {
            editor.key(key(KeyCode::Left));
        }
        editor.insert_attachment(token, "foo.jpg".into());
        assert_eq!(editor.visible_text(), "你看看【foo.jpg】怎么样");
        let (rendered, _, _) = editor.viewport(60, 3);
        assert_eq!(rendered, editor.visible_text());
        editor.key(key(KeyCode::Backspace));
        assert_eq!(editor.text(), "你看看怎么样");
        assert!(!editor.contains_attachment(token));
        editor.paste(&token.to_string());
        assert!(
            editor.text().contains('�'),
            "cannot spoof an attachment token"
        );
    }

    #[test]
    fn undo_and_redo_restore_attachment_as_one_edit_and_path_completion() {
        let mut editor = Editor::default();
        editor.paste("看一下");
        let token = char::from_u32(ATTACHMENT_START).unwrap();
        editor.insert_attachment(token, "file.png".into());
        editor.key(key(KeyCode::Backspace));
        assert_eq!(editor.text(), "看一下");
        editor.key(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::CONTROL));
        assert!(editor.visible_text().contains("【file.png】"));
        editor.key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::CONTROL));
        assert!(!editor.visible_text().contains("【file.png】"));
        editor.clear();
        editor.paste("Review @src/ag");
        assert_eq!(editor.word_before_caret(), "@src/ag");
        editor.replace_word_before_caret("@src/agent.rs");
        assert_eq!(editor.text(), "Review @src/agent.rs");
        editor.key(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::CONTROL));
        assert_eq!(editor.text(), "Review @src/ag");
    }
}
