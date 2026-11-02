//! Default A / Plain renderer. No preferences, model inputs or session data change.
use super::*;

impl UiState {
    /// The interactive host's default renderer, independent of environment flags.
    /// Keep the low-level initializer available for historical regression tests.
    pub fn new_default(
        model: String,
        session: String,
        themes: ThemeStore,
        messages: &[Message],
    ) -> Self {
        let mut state = Self::new(model, session, themes, messages);
        state.plain_style = true;
        state
    }

    pub(super) fn plain_colors(&self) -> Theme {
        self.plain_palette().theme
    }

    pub(super) fn plain_footer_line(&self, colors: Theme, width: usize) -> Line<'static> {
        use crate::usage::compact_tokens;
        let language = self.preferences.language;
        let used = self.context_input.map_or_else(
            || "—".to_owned(),
            |input| {
                format!(
                    "{}{}",
                    if self.context_estimated { "~" } else { "" },
                    compact_tokens(input)
                )
            },
        );
        let capacity = if self.context_window == 0 {
            "—".to_owned()
        } else {
            compact_tokens(self.context_window as u64)
        };
        let context = format!("{used}/{capacity}");
        let last = self.usage.last_request();
        let cache = match (
            last.and_then(|usage| usage.input),
            last.and_then(|usage| usage.cached),
        ) {
            (Some(input), Some(cached)) if input > 0 && cached <= input => {
                crate::usage::UsageMeter::cache_percent(cached, input)
            }
            _ => "—".into(),
        };
        let cache_label = language.text("Cache", "缓存");
        let context_label = language.text("Context", "上下文");
        let level = self.thinking.as_deref().unwrap_or("—");
        let right_variants = [
            (
                format!("{context_label} "),
                format!(" · {cache_label} {cache}"),
            ),
            (String::new(), format!(" · {cache_label} {cache}")),
            (String::new(), format!(" · {cache}")),
        ];
        for (prefix, suffix) in &right_variants {
            let right = format!("{prefix}{context}{suffix}");
            for think in [
                format!(" · {} {level}", language.text("Think", "思考")),
                format!(" · {level}"),
            ] {
                let model_width = width.saturating_sub(
                    UnicodeWidthStr::width(right.as_str())
                        + UnicodeWidthStr::width(think.as_str())
                        + 2,
                );
                if model_width < 4 {
                    continue;
                }
                let left = format!("{}{think}", ellipsize_cells(&self.model, model_width));
                let used =
                    UnicodeWidthStr::width(left.as_str()) + UnicodeWidthStr::width(right.as_str());
                let risk = match (self.context_input, self.context_window) {
                    (Some(input), window) if window > 0 && input as f64 / window as f64 >= 0.9 => {
                        Some(self.semantic_colors().error)
                    }
                    (Some(input), window) if window > 0 && input as f64 / window as f64 >= 0.7 => {
                        Some(self.semantic_colors().warning)
                    }
                    _ => None,
                };
                return Line::from(vec![
                    Span::styled(left, Style::default().fg(colors.faint)),
                    Span::raw(" ".repeat(width.saturating_sub(used))),
                    Span::styled(prefix.clone(), Style::default().fg(colors.faint)),
                    Span::styled(
                        context.clone(),
                        if let Some(color) = risk {
                            Style::default().fg(color).add_modifier(Modifier::BOLD)
                        } else {
                            Style::default().fg(colors.faint)
                        },
                    ),
                    Span::styled(suffix.clone(), Style::default().fg(colors.faint)),
                ]);
            }
        }
        // Extremely small terminals cannot fit four values. Do not wrap,
        // fabricate readings, or let either group overwrite the other.
        Line::styled(
            ellipsize_cells(
                &format!("{} · {level} · {context} · {cache}", self.model),
                width,
            ),
            Style::default().fg(colors.faint),
        )
    }

    pub(super) fn draw_plain_welcome(&mut self, frame: &mut Frame, area: Rect, colors: Theme) {
        let x = area.x.saturating_add(4);
        let width = area.width.saturating_sub(6);
        if width < 8 || area.height < 4 {
            return;
        }
        let mut y = area.y.saturating_add(1);
        frame.render_widget(
            Paragraph::new("KROMA").style(
                Style::default()
                    .fg(colors.faint)
                    .add_modifier(Modifier::BOLD),
            ),
            Rect::new(x, y, width, 1),
        );
        y += 2;
        frame.render_widget(
            Paragraph::new(ellipsize_cells(&self.workdir_label(), usize::from(width)))
                .style(Style::default().fg(colors.muted)),
            Rect::new(x, y, width, 1),
        );
        if area.bottom().saturating_sub(y) < 5 {
            return;
        }
        y += 3;
        let language = self.preferences.language;
        if !self.recent_sessions.is_empty() {
            frame.render_widget(
                Paragraph::new(language.text("Continue working", "继续工作"))
                    .style(Style::default().fg(colors.faint)),
                Rect::new(x, y, width, 1),
            );
            y += 2;
            for entry in self.recent_sessions.iter().take(3) {
                if y >= area.bottom().saturating_sub(2) {
                    break;
                }
                let title =
                    display_session_title(entry.name.as_deref().unwrap_or(&entry.id), language);
                let hit = Rect::new(x, y, width, 1);
                let hovered = self
                    .hover_pointer
                    .is_some_and(|(column, row)| hit.contains((column, row).into()));
                frame.render_widget(
                    Paragraph::new(ellipsize_cells(&format!("› {title}"), usize::from(width)))
                        .style(Style::default().fg(if hovered {
                            colors.text
                        } else {
                            colors.muted
                        })),
                    hit,
                );
                self.recent_session_rows.push((hit, entry.id.clone()));
                y += 1;
            }
            y += 2;
        }
        if y < area.bottom() {
            frame.render_widget(
                Paragraph::new(language.text(
                    "Start a task below · Ctrl+P commands",
                    "在下方开始任务 · Ctrl+P 查看命令",
                ))
                .style(Style::default().fg(colors.faint)),
                Rect::new(x, y, width, 1),
            );
        }
    }
}
