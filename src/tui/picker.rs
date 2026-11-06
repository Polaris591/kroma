//! One small searchable overlay for commands, models, sessions, files and input history.
//! No separate navigation runtime: it returns ordinary TUI actions.
use super::{preferences::Language, theme::Theme};
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
    History,
}

impl Kind {
    pub fn title(self) -> &'static str {
        match self {
            Self::Commands => "Commands",
            Self::Models => "Models",
            Self::Sessions => "Sessions",
            Self::Files => "Files",
            Self::History => "History",
        }
    }
    pub fn localized_title(self, language: Language) -> &'static str {
        match self {
            Self::Commands => language.text("Commands", "命令索引"),
            Self::Models => language.text("Models", "模型"),
            Self::Sessions => language.text("Sessions", "会话"),
            Self::Files => language.text("Files", "文件"),
            Self::History => language.text("History", "输入历史"),
        }
    }
    pub fn next(self) -> Self {
        match self {
            Self::Commands => Self::Models,
            Self::Models => Self::Sessions,
            Self::Sessions => Self::Files,
            Self::Files => Self::Commands,
            Self::History => Self::History,
        }
    }
    pub fn prev(self) -> Self {
        match self {
            Self::Commands => Self::Files,
            Self::Models => Self::Commands,
            Self::Sessions => Self::Models,
            Self::Files => Self::Sessions,
            Self::History => Self::History,
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
    PrevKind,
}

pub struct Picker {
    pub kind: Kind,
    pub query: String,
    pub items: Vec<Item>,
    pub completion: bool,
    /// Optional heading for a contextual picker.
    pub(super) title: Option<String>,
    /// Keep chronological sources in their supplied order.
    pub(super) preserve_order: bool,
    selected: usize,
    area: Rect,
    list_start: usize,
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
            area: Rect::default(),
            list_start: 0,
            completion: false,
            title: None,
            preserve_order: matches!(kind, Kind::Sessions | Kind::History),
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
        if !self.preserve_order {
            items.sort_by(|(rank_a, a), (rank_b, b)| {
                rank_a.cmp(rank_b).then_with(|| a.label.cmp(&b.label))
            });
        }
        items.into_iter().map(|(_, item)| item).collect()
    }

    pub fn key(&mut self, key: KeyEvent) -> Action {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Action::Close;
        }
        match key.code {
            KeyCode::Esc => Action::Close,
            KeyCode::BackTab if !self.completion && self.kind != Kind::History => Action::PrevKind,
            KeyCode::Tab
                if key.modifiers.contains(KeyModifiers::SHIFT)
                    && !self.completion
                    && self.kind != Kind::History =>
            {
                Action::PrevKind
            }
            KeyCode::Tab if !self.completion && self.kind != Kind::History => Action::NextKind,
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
            KeyCode::Enter | KeyCode::Tab => self
                .matches()
                .get(self.selected)
                .map_or(Action::None, |item| Action::Selected(item.value.clone())),
            KeyCode::Backspace => {
                if let Some((at, _)) = self.query.grapheme_indices(true).next_back() {
                    self.query.truncate(at);
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

    /// Select only an item actually painted by the most recent draw.
    pub fn click(&mut self, column: u16, row: u16) -> Action {
        if !self.area.contains((column, row).into()) || row < self.area.y + 2 {
            return Action::None;
        }
        let index = self.list_start + usize::from(row - self.area.y - 2);
        if let Some(value) = self.matches().get(index).map(|item| item.value.clone()) {
            self.selected = index;
            Action::Selected(value)
        } else {
            Action::None
        }
    }

    pub fn draw(&mut self, frame: &mut Frame, parent: Rect, theme: Theme, language: Language) {
        self.area = Rect::default();
        if parent.width < 16 || parent.height < 3 {
            return;
        }
        let height = parent
            .height
            .min(self.matches().len().saturating_add(3).clamp(4, 10) as u16);
        let width = parent.width;
        let area = Rect::new(
            parent.x,
            parent.bottom().saturating_sub(height),
            width,
            height,
        );
        self.area = area;
        frame.render_widget(Clear, area);
        let title_line = if let Some(title) = &self.title {
            Line::styled(
                format!(" {} · Esc ", title),
                Style::default().fg(theme.accent_text),
            )
        } else if self.completion {
            Line::styled(
                language
                    .text(
                        " Commands · ↑↓ select · Tab complete · Esc close ",
                        " 命令索引 · ↑↓选择 · Tab 补全 · Esc 关闭 ",
                    )
                    .to_owned(),
                Style::default().fg(theme.accent_text),
            )
        } else if self.kind == Kind::History {
            Line::styled(
                language
                    .text(
                        " Input history · Enter recall · Esc keep draft ",
                        " 输入历史 · Enter 恢复 · Esc 保留草稿 ",
                    )
                    .to_owned(),
                Style::default().fg(theme.accent_text),
            )
        } else if self.kind == Kind::Sessions {
            Line::styled(
                language
                    .text(
                        " Sessions · ↑↓ select · Enter resume · Esc close ",
                        " 会话 · ↑↓选择 · Enter 恢复 · Esc 关闭 ",
                    )
                    .to_owned(),
                Style::default().fg(theme.accent_text),
            )
        } else if width < 52 {
            Line::styled(
                format!(" {} ", self.kind.localized_title(language)),
                Style::default().fg(theme.accent_text),
            )
        } else {
            let mut spans = vec![Span::raw(" ")];
            for (index, tab) in [Kind::Commands, Kind::Models, Kind::Sessions, Kind::Files]
                .into_iter()
                .enumerate()
            {
                if index > 0 {
                    spans.push(Span::styled(" · ", Style::default().fg(theme.faint)));
                }
                let active = tab == self.kind;
                spans.push(Span::styled(
                    tab.localized_title(language),
                    Style::default()
                        .fg(if active {
                            theme.accent_text
                        } else {
                            theme.faint
                        })
                        .add_modifier(if active {
                            Modifier::BOLD
                        } else {
                            Modifier::empty()
                        }),
                ));
            }
            spans.push(Span::styled(
                language.text("  Tab · Esc ", "  Tab 切换 · Esc "),
                Style::default().fg(theme.faint),
            ));
            Line::from(spans)
        };
        frame.render_widget(
            Block::default()
                .borders(Borders::TOP)
                .title(title_line)
                .border_style(Style::default().fg(theme.border))
                .style(Style::default().bg(theme.overlay)),
            area,
        );
        let inner = Rect::new(
            area.x + 2,
            area.y + 1,
            area.width.saturating_sub(4),
            area.height.saturating_sub(1),
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
                language.text("  No matches", "  没有匹配项"),
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
                    let name = if self.kind == Kind::Commands {
                        clip_cells(&item.label, (max_label / 2).min(30))
                    } else {
                        item.label.clone()
                    };
                    let name = clip_cells(&name, max_label);
                    let hint = clip_cells(
                        &item.hint,
                        max_label.saturating_sub(UnicodeWidthStr::width(name.as_str()) + 4),
                    );
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
                        Span::styled(name, Style::default().fg(theme.text).add_modifier(weight)),
                        Span::styled(
                            if hint.is_empty() {
                                String::new()
                            } else {
                                format!("  — {hint}")
                            },
                            Style::default().fg(theme.muted),
                        ),
                    ])
                    .style(Style::default().bg(background))
                })
                .collect()
        };
        self.list_start = start;
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
            inner.x + 2 + cursor_offset.min(usize::from(inner.width.saturating_sub(3))) as u16,
            inner.y,
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resume_picker_preserves_recent_first_order_and_arrow_navigation() {
        let mut picker = Picker::new(
            Kind::Sessions,
            vec![
                Item::new("Newest chat", "/resume new", "new"),
                Item::new("Earlier chat", "/resume old", "old"),
            ],
            "",
        );
        assert_eq!(
            picker.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Action::Selected("/resume new".into()),
        );
        picker.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(
            picker.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Action::Selected("/resume old".into()),
        );
        picker.key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(
            picker.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Action::Selected("/resume new".into()),
        );
    }
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
    #[test]
    fn dock_mouse_selection_tracks_scrolling_and_ignores_query_or_blank_rows() {
        use ratatui::{backend::TestBackend, Terminal};
        let items = (0..20)
            .map(|i| Item::new(format!("item-{i:02}"), format!("value-{i}"), "description"))
            .collect();
        let mut picker = Picker::new(Kind::Commands, items, "");
        let theme = super::super::theme::ThemeStore::load_at(None)
            .unwrap()
            .colors;
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let parent = Rect::new(1, 2, 78, 18);
        for _ in 0..9 {
            picker.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        }
        terminal
            .draw(|frame| picker.draw(frame, parent, theme, Language::En))
            .unwrap();
        assert_eq!(picker.area.x, parent.x);
        assert_eq!(picker.area.width, parent.width);
        assert_eq!(picker.area.bottom(), parent.bottom());
        assert_eq!(
            picker.click(picker.area.x + 4, picker.area.y + 2),
            Action::Selected("value-2".into())
        );
        assert_eq!(
            picker.click(picker.area.x + 4, picker.area.y + 1),
            Action::None
        );
        assert_eq!(
            picker.click(picker.area.right(), picker.area.y + 2),
            Action::None
        );
        picker.query = "missing".into();
        terminal
            .draw(|frame| picker.draw(frame, parent, theme, Language::En))
            .unwrap();
        assert_eq!(
            picker.click(picker.area.x + 4, picker.area.y + 2),
            Action::None
        );
    }

    #[test]
    fn picker_backspace_removes_complete_unicode_graphemes() {
        let mut picker = Picker::new(Kind::Files, Vec::new(), "👩‍💻e\u{301}");
        assert_eq!(
            picker.key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE)),
            Action::Changed
        );
        assert_eq!(picker.query, "👩‍💻");
        picker.key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        assert!(picker.query.is_empty());
    }
}
