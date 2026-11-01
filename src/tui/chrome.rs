//! Session chrome and best-effort, UI-only Git observations.
use super::super::gitinfo;
use super::*;
use ratatui::layout::Alignment;

#[derive(Clone, Copy)]
pub(in crate::tui) enum NoticeLevel {
    Info,
    Warning,
    Error,
}

pub(in crate::tui) fn is_generated_session_id(value: &str) -> bool {
    let mut parts = value.split('-');
    let (Some(first), Some(second), Some(third), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    first.starts_with('s')
        && first.len() >= 10
        && first[1..].bytes().all(|b| b.is_ascii_hexdigit())
        && !second.is_empty()
        && second.bytes().all(|b| b.is_ascii_hexdigit())
        && !third.is_empty()
        && third.bytes().all(|b| b.is_ascii_hexdigit())
}

pub(in crate::tui) fn display_session_title(
    raw: &str,
    language: super::super::preferences::Language,
) -> &str {
    if raw.trim().is_empty() || raw == "default" || is_generated_session_id(raw) {
        language.text("Untitled session", "未命名会话")
    } else {
        raw
    }
}

impl UiState {
    pub(in crate::tui) fn terminal_title(&self) -> String {
        let prefix = if self.running {
            "● "
        } else {
            match self.task_outcome {
                Some(TaskOutcome::Completed) => "✓ ",
                Some(TaskOutcome::Failed) => "✕ ",
                Some(TaskOutcome::Cancelled) => "⊘ ",
                None => "",
            }
        };
        let label = self
            .session_label()
            .chars()
            .filter(|ch| !ch.is_control())
            .take(80)
            .collect::<String>();
        if label.is_empty() {
            format!("{prefix}Kroma")
        } else {
            format!("{prefix}Kroma — {label}")
        }
    }

    pub(in crate::tui) fn show_help(&mut self) {
        let content = format!(
            "{}

{}: {}",
            super::super::help::text(self.preferences.language),
            self.preferences
                .language
                .text("Working directory", "工作目录"),
            self.link_workdir.display()
        );
        self.info("HELP", content);
    }

    pub(in crate::tui) fn refresh_git(&self) {
        let Some(sender) = self.ui_sender.clone() else {
            return;
        };
        let workdir = self.link_workdir.clone();
        let revision = self.git_revision;
        let session = self.session.clone();
        tokio::spawn(async move {
            if let Some((info, _)) = gitinfo::observe(&workdir).await {
                let _ = sender.send(UiEvent::GitInfo {
                    revision,
                    session,
                    info,
                    changes: None,
                });
            }
        });
    }
    pub(in crate::tui) fn begin_git_turn(
        &self,
    ) -> tokio::task::JoinHandle<Option<gitinfo::Snapshot>> {
        let workdir = self.link_workdir.clone();
        tokio::spawn(async move { gitinfo::snapshot(&workdir).await })
    }
    pub(in crate::tui) fn finish_git_turn(
        &self,
        baseline: tokio::task::JoinHandle<Option<gitinfo::Snapshot>>,
    ) {
        let Some(sender) = self.ui_sender.clone() else {
            baseline.abort();
            return;
        };
        let workdir = self.link_workdir.clone();
        let revision = self.git_revision;
        let session = self.session.clone();
        // A baseline not captured before task completion is not a baseline.
        let ready = baseline.is_finished();
        if !ready {
            baseline.abort();
        }
        tokio::spawn(async move {
            let before = if ready {
                baseline.await.ok().flatten()
            } else {
                None
            };
            if let Some((info, after)) = gitinfo::observe(&workdir).await {
                let changes = gitinfo::changes(before.as_ref(), after.as_ref());
                let _ = sender.send(UiEvent::GitInfo {
                    revision,
                    session,
                    info,
                    changes,
                });
            }
        });
    }
    pub(super) fn draw_header(&self, frame: &mut Frame, area: Rect, theme: Theme) {
        let language = self.preferences.language;
        let (status, ink) = match self.task_outcome {
            Some(TaskOutcome::Failed) if !self.running => {
                (language.text("✕ Failed", "✕ 失败").to_owned(), theme.error)
            }
            Some(TaskOutcome::Cancelled) if !self.running => (
                language.text("⊘ Cancelled", "⊘ 已取消").to_owned(),
                theme.warning,
            ),
            _ => (String::new(), theme.faint),
        };
        let workdir_name = self
            .link_workdir
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        let show_workdir = !workdir_name.is_empty()
            && (!workdir_name.eq_ignore_ascii_case("kroma") || self.git_info.branch.is_some());
        let mut right = String::new();
        if area.width >= 50 && show_workdir {
            right.push_str(workdir_name);
            right.push_str("  ");
        }
        if area.width >= 70 {
            if let Some(branch) = &self.git_info.branch {
                right.push_str(branch);
                if self.git_info.dirty {
                    right.push('*');
                }
                right.push_str("  ");
            }
        }
        right.push_str(&status);
        if UnicodeWidthStr::width(right.as_str()) > usize::from(area.width.saturating_sub(12)) {
            right = if area.width >= 50 && show_workdir {
                format!("{}  {status}", ellipsize_cells(workdir_name, 16))
            } else {
                status.clone()
            };
        }
        if UnicodeWidthStr::width(right.as_str()) > usize::from(area.width.saturating_sub(10)) {
            right = status.clone();
        }
        let right = right.trim_end().to_owned();
        let right_width = UnicodeWidthStr::width(right.as_str()) as u16;
        let label = ellipsize_cells(
            self.session_label(),
            usize::from(area.width.saturating_sub(right_width + 11)),
        );
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("◆ ", Style::default().fg(theme.accent)),
                Span::styled(
                    "Kroma",
                    Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!(" / {label}"),
                    Style::default().fg(if self.home { theme.faint } else { theme.muted }),
                ),
            ])),
            area,
        );
        // Project/branch are secondary; only the rightmost state has status ink.
        let prefix = right.strip_suffix(&status).unwrap_or("");
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(prefix.to_owned(), Style::default().fg(theme.faint)),
                Span::styled(
                    if prefix.is_empty() {
                        right.clone()
                    } else {
                        status
                    },
                    Style::default().fg(ink),
                ),
            ]))
            .alignment(Alignment::Right),
            Rect::new(
                area.right().saturating_sub(right_width),
                area.y,
                right_width,
                1,
            ),
        );
    }
    pub(super) fn change_summary(&self) -> String {
        let Some(changes) = &self.turn_changes else {
            return String::new();
        };
        let language = self.preferences.language;
        let mut summary = String::new();
        if changes.files > 0 {
            summary = format!(
                " · {} {} {} +{} −{}",
                language.text("Workspace", "工作区变化"),
                changes.files,
                language.text("files", "个文件"),
                changes.added,
                changes.removed
            );
        }
        if changes.untracked > 0 {
            summary.push_str(&format!(
                " · {} {} {}",
                language.text("Untracked", "未跟踪"),
                changes.untracked,
                language.text("files", "个文件")
            ));
        }
        if !summary.is_empty() {
            summary.push_str(" · /diff");
        }
        summary
    }
    pub(in crate::tui) fn recent_session_at(&self, column: u16, row: u16) -> Option<&str> {
        if !self.home || self.running || self.picker.is_some() || self.plugin_panel.is_some() {
            return None;
        }
        self.recent_session_rows
            .iter()
            .find_map(|(area, id)| area.contains((column, row).into()).then_some(id.as_str()))
    }

    pub(super) fn draw_welcome_details(&mut self, frame: &mut Frame, area: Rect, theme: Theme) {
        if area.height < 4 || area.width < 8 {
            return;
        }
        let language = self.preferences.language;
        let inner = Rect::new(
            area.x + 4,
            area.y,
            area.width.saturating_sub(6),
            area.height,
        );
        let mark_rows = if area.height >= 16 && inner.width >= 24 {
            5
        } else {
            1
        };
        let recent_count = self.recent_sessions.len().min(if area.height >= 16 {
            3
        } else if area.height >= 11 {
            2
        } else {
            0
        });
        let recent_rows = if recent_count > 0 {
            recent_count as u16 + 2
        } else {
            0
        };
        let height = (mark_rows + 4 + recent_rows).min(area.height);
        let top = area
            .bottom()
            .saturating_sub(height + u16::from(area.height > height));
        super::super::logo::draw(
            frame,
            Rect::new(inner.x, top, inner.width, mark_rows),
            theme,
            self.themes.preset(),
        );
        let path_y = top + mark_rows + 1;
        if path_y < area.bottom() {
            frame.render_widget(
                Paragraph::new(ellipsize_cells(
                    &self.workdir_label(),
                    usize::from(inner.width),
                ))
                .style(Style::default().fg(theme.faint)),
                Rect::new(inner.x, path_y, inner.width, 1),
            );
        }
        if recent_count > 0 {
            let heading_y = path_y + 2;
            frame.render_widget(
                Paragraph::new(language.text("Continue working", "继续工作"))
                    .style(Style::default().fg(theme.muted)),
                Rect::new(inner.x, heading_y, inner.width, 1),
            );
            for (index, entry) in self.recent_sessions.iter().take(recent_count).enumerate() {
                let y = heading_y + 1 + index as u16;
                if y >= area.bottom() {
                    break;
                }
                let title =
                    display_session_title(entry.name.as_deref().unwrap_or(&entry.id), language);
                let age = entry
                    .modified
                    .and_then(|time| time.elapsed().ok())
                    .map(|elapsed| {
                        let mins = elapsed.as_secs() / 60;
                        if mins < 60 {
                            format!("{mins}{}", language.text("m ago", " 分钟前"))
                        } else if mins < 1440 {
                            format!("{}{}", mins / 60, language.text("h ago", " 小时前"))
                        } else {
                            format!("{}{}", mins / 1440, language.text("d ago", " 天前"))
                        }
                    })
                    .unwrap_or_else(|| "—".into());
                let age_width = UnicodeWidthStr::width(age.as_str());
                let width = usize::from(inner.width).min(64);
                let clipped = ellipsize_cells(title, width.saturating_sub(age_width + 4));
                let pad =
                    width.saturating_sub(UnicodeWidthStr::width(clipped.as_str()) + age_width + 2);
                let hit = Rect::new(inner.x, y, width as u16, 1);
                let hovered = self
                    .hover_pointer
                    .is_some_and(|(x, y)| hit.contains((x, y).into()));
                let line = Line::from(vec![
                    Span::styled(
                        "› ",
                        Style::default().fg(if hovered {
                            theme.accent_text
                        } else {
                            theme.faint
                        }),
                    ),
                    Span::styled(
                        clipped,
                        Style::default().fg(if hovered {
                            theme.accent_text
                        } else {
                            theme.text
                        }),
                    ),
                    Span::raw(" ".repeat(pad)),
                    Span::styled(age, Style::default().fg(theme.faint)),
                ]);
                frame.render_widget(Paragraph::new(line), hit);
                self.recent_session_rows.push((hit, entry.id.clone()));
            }
        }
        let hint_y = top + height - 1;
        if hint_y < area.bottom() {
            frame.render_widget(
                Paragraph::new(language.text(
                    "Start a task below · Ctrl+P to explore",
                    "在下方开始任务 · Ctrl+P 查看更多",
                ))
                .style(Style::default().fg(theme.faint)),
                Rect::new(inner.x, hint_y, inner.width, 1),
            );
        }
    }
}
