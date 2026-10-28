//! One small searchable overlay for commands, model profiles, sessions and files.
//! No separate navigation runtime: it returns ordinary TUI actions.
use super::theme::Theme;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
    Frame,
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Kind {
    Commands,
    Models,
    Sessions,
    Files,
}

impl Kind {
    pub fn title(self) -> &'static str {
        match self {
            Self::Commands => "Commands",
            Self::Models => "Models",
            Self::Sessions => "Sessions",
            Self::Files => "Files",
        }
    }
    pub fn next(self) -> Self {
        match self {
            Self::Commands => Self::Models,
            Self::Models => Self::Sessions,
            Self::Sessions => Self::Files,
            Self::Files => Self::Commands,
        }
    }
}

#[derive(Clone)]
pub struct Item {
    pub label: String,
    pub value: String,
    pub hint: String,
}

impl Item {
    pub fn new(
        label: impl Into<String>,
        value: impl Into<String>,
        hint: impl Into<String>,
    ) -> Self {
        Self {
            label: label.into(),
            value: value.into(),
            hint: hint.into(),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    None,
    Changed,
    Close,
    Selected(String),
    NextKind,
}

pub struct Picker {
    pub kind: Kind,
    pub query: String,
    pub items: Vec<Item>,
    selected: usize,
}

fn clip_cells(value: &str, max_width: usize) -> String {
    if UnicodeWidthStr::width(value) <= max_width {
        return value.into();
    }
    if max_width == 0 {
        return String::new();
    }
    let mut result = String::new();
    let mut used = 0;
    for part in value.graphemes(true) {
        let cells = UnicodeWidthStr::width(part);
        if used + cells >= max_width {
            break;
        }
        result.push_str(part);
        used += cells;
    }
    result.push('…');
    result
}

fn query_tail(value: &str, max_width: usize) -> String {
    if UnicodeWidthStr::width(value) <= max_width {
        return value.into();
    }
    if max_width == 0 {
        return String::new();
    }
    let mut remaining = max_width.saturating_sub(1);
    let mut parts = Vec::new();
    for part in value.graphemes(true).rev() {
        let cells = UnicodeWidthStr::width(part);
        if cells > remaining {
            break;
        }
        remaining -= cells;
        parts.push(part);
    }
    format!("…{}", parts.into_iter().rev().collect::<String>())
}

impl Picker {
    pub fn new(kind: Kind, items: Vec<Item>, query: impl Into<String>) -> Self {
        Self {
            kind,
            items,
            query: query.into(),
            selected: 0,
        }
    }

    // Order is deterministic: exact prefix, substring, then fuzzy subsequence.
    // Never mix result order with filesystem iteration order.
    fn rank(label: &str, needle: &str) -> Option<usize> {
        let haystack = label.to_lowercase();
        let query = needle.trim().to_lowercase();
        if query.is_empty() {
            return Some(0);
        }
        if haystack.starts_with(&query) {
            return Some(0);
        }
        if haystack.contains(&query) {
            return Some(1);
        }
        let mut chars = haystack.chars();
        if query.chars().all(|c| chars.by_ref().any(|next| next == c)) {
            Some(2)
        } else {
            None
        }
    }

    fn matches(&self) -> Vec<&Item> {
        let mut items = self
            .items
            .iter()
            .filter_map(|item| {
                Self::rank(&format!("{} {}", item.label, item.hint), &self.query)
                    .map(|rank| (rank, item))
            })
            .collect::<Vec<_>>();
        items.sort_by(|(rank_a, a), (rank_b, b)| {
            rank_a.cmp(rank_b).then_with(|| a.label.cmp(&b.label))
        });
        items.into_iter().map(|(_, item)| item).collect()
    }

    pub fn key(&mut self, key: KeyEvent) -> Action {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Action::Close;
        }
        match key.code {
            KeyCode::Esc => Action::Close,
            KeyCode::Tab => Action::NextKind,
            KeyCode::Up => {
                self.selected = self.selected.saturating_sub(1);
                Action::None
            }
            KeyCode::Down => {
                self.selected = self
                    .selected
                    .saturating_add(1)
                    .min(self.matches().len().saturating_sub(1));
                Action::None
            }
            KeyCode::Enter => self
                .matches()
                .get(self.selected)
                .map_or(Action::None, |item| Action::Selected(item.value.clone())),
            KeyCode::Backspace => {
                if self.query.pop().is_some() {
                    self.selected = 0;
                    Action::Changed
                } else {
                    Action::None
                }
            }
            KeyCode::Char(ch)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.query.push(ch);
                self.selected = 0;
                Action::Changed
            }
            _ => Action::None,
        }
    }

    pub fn draw(&self, frame: &mut Frame, parent: Rect, theme: Theme) {
        if parent.width < 16 || parent.height < 3 {
            return;
        }
        let height = parent.height.min(11);
        let width = parent.width.min(72);
        let area = Rect::new(
            parent.x + (parent.width - width) / 2,
            parent.bottom().saturating_sub(height),
            width,
            height,
        );
        frame.render_widget(Clear, area);
        frame.render_widget(
            Block::default()
                .borders(Borders::ALL)
                .title(if width < 48 {
                    format!(" {} ", self.kind.title())
                } else {
                    format!(" {} · Tab: switch · Esc: close ", self.kind.title())
                })
                .border_style(Style::default().fg(theme.accent))
                .style(Style::default().bg(theme.input_surface())),
            area,
        );
        let inner = Rect::new(
            area.x + 2,
            area.y + 1,
            area.width.saturating_sub(4),
            area.height.saturating_sub(2),
        );
        if inner.height == 0 {
            return;
        }
        let matches = self.matches();
        let title = Line::from(vec![
            Span::styled("› ", Style::default().fg(theme.accent)),
            Span::styled(
                query_tail(&self.query, inner.width.saturating_sub(2) as usize),
                Style::default().fg(theme.text),
            ),
        ]);
        frame.render_widget(
            Paragraph::new(title),
            Rect::new(inner.x, inner.y, inner.width, 1),
        );
        let list_height = inner.height.saturating_sub(1) as usize;
        let start = self.selected.saturating_sub(list_height.saturating_sub(1));
        let lines = if matches.is_empty() {
            vec![Line::styled(
                "  No matches",
                Style::default().fg(theme.muted),
            )]
        } else {
            matches
                .iter()
                .enumerate()
                .skip(start)
                .take(list_height)
                .map(|(index, item)| {
                    let active = index == self.selected;
                    let marker = if active { "› " } else { "  " };
                    let max_label = inner.width.saturating_sub(2) as usize;
                    let label = clip_cells(&item.label, max_label);
                    Line::from(vec![
                        Span::styled(marker, Style::default().fg(theme.accent)),
                        Span::styled(
                            label,
                            Style::default()
                                .fg(if active { theme.accent } else { theme.text })
                                .add_modifier(if active {
                                    Modifier::BOLD
                                } else {
                                    Modifier::empty()
                                }),
                        ),
                    ])
                })
                .collect()
        };
        frame.render_widget(
            Paragraph::new(lines),
            Rect::new(
                inner.x,
                inner.y + 1,
                inner.width,
                inner.height.saturating_sub(1),
            ),
        );
        let cursor_offset = UnicodeWidthStr::width(self.query.as_str());
        frame.set_cursor_position((
            inner.x + 2 + (cursor_offset as u16).min(inner.width.saturating_sub(3)),
            inner.y,
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn one_fuzzy_picker_preserves_stable_selection_and_category_switching() {
        let mut picker = Picker::new(
            Kind::Commands,
            vec![
                Item::new("/resume", "/resume", "session"),
                Item::new("/rewind", "/rewind", "history"),
                Item::new("/model", "/model", "model"),
            ],
            "",
        );
        for ch in "rsm".chars() {
            assert_eq!(
                picker.key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)),
                Action::Changed
            );
        }
        assert_eq!(
            picker.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Action::Selected("/resume".into())
        );
        assert_eq!(
            picker.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
            Action::NextKind
        );
        assert_eq!(picker.kind.next(), Kind::Models);
        assert_eq!(
            picker.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            Action::Close
        );
    }
}
