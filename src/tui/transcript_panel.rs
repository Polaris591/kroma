//! Optional full transcript reuses the existing scrollable overlay and input focus.
use super::*;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Default)]
pub(super) struct Search {
    pub query: String,
    pub editing: bool,
    pub matches: Vec<(usize, usize, usize)>,
    pub current: usize,
    dirty: bool,
    jump: bool,
}
impl PluginPanel {
    pub(super) fn refresh_search(&mut self, reflow: bool) {
        let Some(search) = &mut self.search else {
            return;
        };
        if reflow || search.dirty {
            search.matches.clear();
            if !search.query.is_empty() {
                for (row, line) in self.wrapped.lines.iter().enumerate() {
                    let text = line.to_string();
                    for (at, matched) in text.match_indices(&search.query) {
                        let start = UnicodeWidthStr::width(&text[..at]);
                        search
                            .matches
                            .push((row, start, start + UnicodeWidthStr::width(matched)));
                    }
                }
            }
            search.current = search.current.min(search.matches.len().saturating_sub(1));
            search.dirty = false;
        }
        if search.jump {
            if let Some((row, _, _)) = search.matches.get(search.current) {
                self.scroll = (*row).min(u16::MAX as usize) as u16;
            }
            search.jump = false;
        }
    }
}
impl UiState {
    pub(in crate::tui) fn open_full_transcript(&mut self) {
        let language = self.preferences.language;
        let mut rows = Vec::new();
        for entry in &self.entries {
            let title = match entry.kind {
                Kind::User => language.text("You", "你"),
                Kind::Assistant => "Kroma",
                Kind::Reasoning => language.text("Reasoning", "思考"),
                Kind::Tool => language.text("Tool", "工具"),
                Kind::Info => self.info_title(&entry.title),
            };
            rows.push(title.to_owned());
            match entry.kind {
                Kind::Assistant | Kind::Reasoning => {
                    if entry.body.starts_with(crate::agent::TASK_CANCELLED_PREFIX) {
                        rows.push(
                            language
                                .text(
                                    "Cancelled · incomplete commands may have had side effects",
                                    "已取消 · 未完成的命令可能已产生副作用",
                                )
                                .into(),
                        );
                    } else {
                        let document = markdown::render_document(
                            &entry.body,
                            self.active_colors(),
                            usize::MAX / 4,
                        );
                        rows.extend(document.lines.into_iter().map(|line| {
                            line.spans
                                .into_iter()
                                .filter(|span| span.content != "│ ")
                                .map(|span| span.content.into_owned())
                                .collect::<String>()
                        }));
                    }
                }
                Kind::Tool => {
                    rows.push(entry.body.clone());
                    rows.push(entry.details.clone());
                }
                Kind::User | Kind::Info => rows.push(entry.body.clone()),
            }
            rows.push(String::new());
        }
        self.open_plugin_panel(format!(
            "{}\n\n{}",
            language.text(
                "Full transcript · / search · n/N",
                "全文记录 · / 搜索 · n/N"
            ),
            rows.join("\n")
        ));
        if let Some(panel) = &mut self.plugin_panel {
            panel.search = Some(Search::default());
        }
    }
    pub(in crate::tui) fn panel_search_key(&mut self, key: KeyEvent) -> bool {
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return false;
        }
        let Some(search) = self
            .plugin_panel
            .as_mut()
            .and_then(|panel| panel.search.as_mut())
        else {
            return false;
        };
        if search.editing {
            match key.code {
                KeyCode::Esc | KeyCode::Enter => search.editing = false,
                KeyCode::Backspace => {
                    if let Some((at, _)) = search.query.grapheme_indices(true).next_back() {
                        search.query.truncate(at);
                    }
                    search.dirty = true;
                    search.jump = true;
                    search.current = 0;
                }
                KeyCode::Char(ch) => {
                    if search.query.chars().count() < 256 {
                        search.query.push(ch);
                        search.dirty = true;
                        search.jump = true;
                        search.current = 0;
                    }
                }
                _ => return false,
            }
            return true;
        }
        match key.code {
            KeyCode::Char('/') => {
                search.editing = true;
                search.query.clear();
                search.dirty = true;
                search.current = 0;
            }
            KeyCode::Char('n' | 'N') => {
                if !search.matches.is_empty() {
                    search.current = if key.code == KeyCode::Char('N') {
                        (search.current + search.matches.len() - 1) % search.matches.len()
                    } else {
                        (search.current + 1) % search.matches.len()
                    };
                    search.jump = true;
                }
            }
            _ => return false,
        }
        true
    }
}

impl UiState {
    pub(super) fn draw_panel(&mut self, frame: &mut Frame, full_area: Rect, colors: Theme) {
        if let Some(panel) = &mut self.plugin_panel {
            let width = full_area.width.saturating_sub(2).min(100);
            let height = full_area.height.saturating_sub(2).min(26);
            if width >= 16 && height >= 4 {
                let area = Rect::new(
                    full_area.x + (full_area.width - width) / 2,
                    full_area.y + (full_area.height - height) / 2,
                    width,
                    height,
                );
                frame.render_widget(Clear, area);
                let inner = Rect::new(
                    area.x + 1,
                    area.y + 1,
                    area.width.saturating_sub(2),
                    area.height.saturating_sub(2),
                );
                let reflow = panel.wrapped_width != usize::from(inner.width);
                if reflow {
                    let (wrapped, _) =
                        wrap_document(Text::raw(panel.body.clone()), usize::from(inner.width));
                    panel.wrapped = wrapped;
                    panel.wrapped_width = usize::from(inner.width);
                }
                panel.refresh_search(reflow);

                let heading = if width < 44 {
                    format!(
                        " {} · Esc ",
                        ellipsize_cells(&panel.title, width.saturating_sub(10) as usize)
                    )
                } else {
                    format!(
                        " {} · ↑↓/PgUp/PgDn · Esc {} ",
                        ellipsize_cells(&panel.title, width.saturating_sub(28) as usize),
                        self.preferences.language.text("Close", "关闭")
                    )
                };
                let heading = if let Some(search) = &panel.search {
                    format!(
                        "{} /{} · {}/{} ",
                        ellipsize_cells(&heading, width.saturating_sub(28) as usize),
                        search.query,
                        if search.matches.is_empty() {
                            0
                        } else {
                            search.current + 1
                        },
                        search.matches.len()
                    )
                } else {
                    heading
                };
                let block = Block::default()
                    .borders(Borders::ALL)
                    .title(Line::styled(
                        heading,
                        Style::default().fg(colors.accent_text),
                    ))
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(colors.border))
                    .style(Style::default().bg(colors.overlay));
                frame.render_widget(block, area);
                let max_scroll = panel
                    .wrapped
                    .lines
                    .len()
                    .saturating_sub(inner.height as usize)
                    .min(u16::MAX as usize) as u16;
                panel.scroll = panel.scroll.min(max_scroll);
                // Clone only the visible rows; never rewrap or copy the
                // entire panel on the 180ms activity-frame redraw.
                let visible = panel
                    .wrapped
                    .lines
                    .iter()
                    .skip(usize::from(panel.scroll))
                    .take(usize::from(inner.height))
                    .cloned()
                    .collect::<Vec<_>>();
                frame.render_widget(
                    Paragraph::new(visible)
                        .style(Style::default().fg(colors.text).bg(colors.overlay)),
                    inner,
                );
                if let Some(search) = &panel.search {
                    for (index, (row, start, end)) in search.matches.iter().enumerate() {
                        if *row < usize::from(panel.scroll)
                            || *row >= usize::from(panel.scroll) + usize::from(inner.height)
                        {
                            continue;
                        }
                        let y = inner.y + (*row - usize::from(panel.scroll)) as u16;
                        for x in *start..(*end).min(usize::from(inner.width)) {
                            if let Some(cell) = frame.buffer_mut().cell_mut((inner.x + x as u16, y))
                            {
                                cell.set_style(
                                    Style::default()
                                        .fg(colors.text)
                                        .bg(Theme::blend(colors.accent, colors.overlay, 0.35))
                                        .add_modifier(if index == search.current {
                                            Modifier::BOLD | Modifier::UNDERLINED
                                        } else {
                                            Modifier::empty()
                                        }),
                                );
                            }
                        }
                    }
                }
            }
        }
    }
}
