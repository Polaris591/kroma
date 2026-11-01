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
                _ => (language.text("✓ Ready", "✓ 就绪").into(), theme.faint),
            }
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
                    format!("  {label}"),
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
        let reserve = if area.height >= 22 && !self.recent_sessions.is_empty() {
            6
        } else if area.height >= 25 {
            4
        } else {
            2
        };
        let logo_area_height = area.height.saturating_sub(reserve);
        let mark_height = if logo_area_height >= 20 && area.width >= 34 {
            12
        } else if logo_area_height >= 14 && area.width >= 26 {
            8
        } else {
            0
        };
        let block_height = if mark_height > 0 { mark_height + 4 } else { 3 };
        let logo_bottom = area.y + logo_area_height.saturating_sub(block_height) / 2 + block_height;
        let max_y = area.bottom().saturating_sub(
            if area.height >= 22 && !self.recent_sessions.is_empty() {
                5
            } else {
                2
            },
        );
        let y = (logo_bottom + 1).min(max_y);
        frame.render_widget(
            Paragraph::new(ellipsize_cells(&context, usize::from(area.width)))
                .alignment(Alignment::Center)
                .style(Style::default().fg(theme.faint)),
            Rect::new(area.x, y, area.width, 1),
        );
        if area.height >= 22 && !self.recent_sessions.is_empty() {
            let col_width = usize::from(area.width.saturating_sub(4)).min(52);
            for (index, (raw_title, modified)) in self.recent_sessions.iter().take(3).enumerate() {
                let title = display_session_title(raw_title, language);
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
                let line = if col_width >= 24 {
                    let age_width = UnicodeWidthStr::width(age.as_str());
                    let max_title = col_width.saturating_sub(age_width + 2);
                    let clipped = ellipsize_cells(title, max_title);
                    let used = UnicodeWidthStr::width(clipped.as_str()) + age_width;
                    let pad = col_width.saturating_sub(used).max(2);
                    Line::from(vec![
                        Span::styled(clipped, Style::default().fg(theme.muted)),
                        Span::raw(" ".repeat(pad)),
                        Span::styled(age, Style::default().fg(theme.faint)),
                    ])
                } else {
                    Line::styled(
                        ellipsize_cells(&format!("{title} · {age}"), usize::from(area.width)),
                        Style::default().fg(theme.muted),
                    )
                };
                frame.render_widget(
                    Paragraph::new(line).alignment(Alignment::Center),
                    Rect::new(area.x, y + 2 + index as u16, area.width, 1),
                );
            }
        }
    }
}
