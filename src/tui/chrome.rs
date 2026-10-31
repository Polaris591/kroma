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
        let (status, ink) = if self.running {
            (
                format!(
                    "● {} {}s",
                    language.text("Running", "运行中"),
                    self.task_started
                        .map_or(0, |start| start.elapsed().as_secs())
                ),
                theme.accent_text,
            )
        } else {
            match self.task_outcome {
                Some(TaskOutcome::Failed) => {
                    (language.text("✕ Failed", "✕ 失败").into(), theme.error)
                }
                Some(TaskOutcome::Cancelled) => {
                    (language.text("⊘ Cancelled", "⊘ 已取消").into(), theme.muted)
                }
                _ => (language.text("✓ Ready", "✓ 就绪").into(), theme.muted),
            }
        };
        let mut right = String::new();
        if area.width >= 50 {
            right.push_str(
                self.link_workdir
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or(""),
            );
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
            right = if area.width >= 50 {
                format!(
                    "{}  {status}",
                    ellipsize_cells(
                        self.link_workdir
                            .file_name()
                            .and_then(|name| name.to_str())
                            .unwrap_or(""),
                        16
                    )
                )
            } else {
                status.clone()
            };
        }
        if UnicodeWidthStr::width(right.as_str()) > usize::from(area.width.saturating_sub(10)) {
            right = status.clone();
        }
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
                Span::styled(format!("  {label}"), Style::default().fg(theme.muted)),
            ])),
            area,
        );
        // Project/branch are secondary; only the rightmost state has status ink.
        let prefix = right.strip_suffix(&status).unwrap_or("");
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(prefix.to_owned(), Style::default().fg(theme.muted)),
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
                language.text("Changed", "改动"),
                changes.files,
                language.text("files", "个文件"),
                changes.added,
                changes.removed
            );
        }
        if changes.untracked > 0 {
            summary.push_str(&format!(
                " · {} {} {}",
                language.text("Added", "新增"),
                changes.untracked,
                language.text("files", "个文件")
            ));
        }
        if !summary.is_empty() {
            summary.push_str(" · /diff");
        }
        summary
    }
    pub(super) fn draw_welcome_details(&self, frame: &mut Frame, area: Rect, theme: Theme) {
        if area.height < 8 {
            return;
        }
        let language = self.preferences.language;
        let branch = self.git_info.branch.as_deref().unwrap_or("");
        let context = format!(
            "{} · {}{}",
            self.model,
            self.workdir_label(),
            if branch.is_empty() {
                String::new()
            } else {
                format!(" · {branch}")
            }
        );
        let y = area
            .bottom()
            .saturating_sub(if area.height >= 25 { 6 } else { 2 });
        frame.render_widget(
            Paragraph::new(ellipsize_cells(&context, usize::from(area.width)))
                .alignment(Alignment::Center)
                .style(Style::default().fg(theme.muted)),
            Rect::new(area.x, y, area.width, 1),
        );
        if area.height >= 25 && !self.recent_sessions.is_empty() {
            for (index, (title, modified)) in self.recent_sessions.iter().take(3).enumerate() {
                let age = modified
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
                let line = ellipsize_cells(&format!("{title} · {age}"), usize::from(area.width));
                frame.render_widget(
                    Paragraph::new(line)
                        .alignment(Alignment::Center)
                        .style(Style::default().fg(theme.muted)),
                    Rect::new(area.x, y + 1 + index as u16, area.width, 1),
                );
            }
            frame.render_widget(
                Paragraph::new(language.text("/resume View all sessions", "/resume 查看全部"))
                    .alignment(Alignment::Center)
                    .style(Style::default().fg(theme.muted)),
                Rect::new(area.x, y + 4, area.width, 1),
            );
        }
    }
}
