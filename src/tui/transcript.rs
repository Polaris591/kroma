//! Tool cards computed from adjacent transcript entries; nothing is persisted.
use super::*;

#[derive(Default)]
pub(super) struct ToolCard {
    pub lines: Vec<Line<'static>>,
    pub rows: Vec<(usize, String)>,
    pub preformatted: Vec<usize>,
    pub groups: Vec<(usize, String)>,
}

impl UiState {
    fn append_tool(&self, entry: &Entry, theme: Theme, width: usize, card: &mut ToolCard) {
        let lines = &mut card.lines;
        let tool_rows = &mut card.rows;
        let preformatted_rows = &mut card.preformatted;

        let (name, status) = entry
            .title
            .split_once(" · ")
            .unwrap_or((&entry.title, "done"));
        let warning = !entry.failed && super::super::tool_display::has_diagnostic(&entry.details);
        let (marker, color) = if interrupted(entry) {
            ("⊘", theme.warning)
        } else if entry.failed {
            ("✕", theme.error)
        } else if status == "running" {
            ("◌", theme.accent)
        } else if warning {
            ("!", theme.warning)
        } else {
            ("✓", theme.success)
        };
        // Render the operation once, not "shell + run shell command
        // + Ctrl+O" on every line. Success is quiet; failure and
        // active work carry the visual weight. Raw details remain
        // available without truncation using Ctrl+O.
        let expanded = self.tools_expanded
            || entry
                .call_id
                .as_ref()
                .is_some_and(|id| self.expanded_tool_calls.contains(id));
        let caret = if expanded { "▾" } else { "▸" };
        let elapsed = entry
            .duration
            .map(|duration| format!(" · {:.1}s", duration.as_secs_f64()))
            .unwrap_or_default();
        let summary =
            super::super::tool_display::summary(name, &entry.body, self.preferences.language);
        let summary = match self.plugin_tool_labels.get(name) {
            Some(label) if name != "shell" => format!(
                "{label} · {}",
                summary
                    .strip_prefix(name)
                    .and_then(|s| s.strip_prefix(" · "))
                    .unwrap_or(&summary)
            ),
            Some(label) => format!("{label} · {summary}"),
            None => summary,
        };
        let command = compact_line(
            &summary,
            width.saturating_sub(6 + UnicodeWidthStr::width(elapsed.as_str())),
        );
        if let Some(id) = entry.call_id.as_ref() {
            tool_rows.push((lines.len(), id.clone()));
        }
        lines.push(Line::from(vec![
            Span::styled(format!("  {caret} "), Style::default().fg(theme.border)),
            Span::styled(format!("{marker} "), Style::default().fg(color)),
            Span::styled(
                command,
                Style::default().fg(if entry.failed || status == "running" {
                    theme.text
                } else {
                    theme.muted
                }),
            ),
            Span::styled(elapsed, Style::default().fg(theme.muted)),
        ]));
        if warning && !expanded {
            lines.push(body_line(Line::styled(
                self.preferences.language.text(
                    "↳ Exit 0; diagnostic reported · Ctrl+O details",
                    "↳ 退出成功，输出含警告或错误提示 · Ctrl+O 查看详情",
                ),
                Style::default().fg(theme.warning),
            )));
        }
        if interrupted(entry) {
            lines.push(body_line(Line::styled(
                self.preferences.language.text(
                    "⊘ Interrupted · result unknown (will not rerun automatically)",
                    "⊘ 已中断 · 结果未知（不会自动重跑）",
                ),
                Style::default().fg(theme.warning),
            )));
        } else if entry.failed && !expanded {
            lines.push(body_line(Line::from(Span::styled(
                format!(
                    "↳ {}",
                    compact_line(&failure_summary(&entry.details), width.saturating_sub(5))
                ),
                Style::default().fg(theme.error),
            ))));
        }
        if status == "running" && !entry.details.is_empty() && !expanded {
            let preview = entry
                .details
                .lines()
                .rev()
                .find(|line| !line.trim().is_empty())
                .unwrap_or("");
            lines.push(body_line(Line::from(Span::styled(
                format!("↳ {}", compact_line(preview, width.saturating_sub(5))),
                Style::default().fg(theme.code),
            ))));
        }
        if expanded {
            lines.push(Line::styled(
                self.preferences
                    .language
                    .text("     ├─ Command", "     ├─ 执行命令"),
                Style::default().fg(theme.muted),
            ));
            for command in entry.body.lines() {
                let mut tab_column = 0;
                let command = markdown::expand_tabs(command, &mut tab_column);
                preformatted_rows.push(lines.len());
                lines.push(body_line(Line::from(vec![
                    Span::styled("│ ", Style::default().fg(theme.border)),
                    Span::styled(format!("$ {command}"), Style::default().fg(theme.code)),
                ])));
            }
        }
        if expanded && !entry.details.is_empty() && !interrupted(entry) {
            lines.push(Line::styled(
                self.preferences
                    .language
                    .text("     └─ Output", "     └─ 执行输出"),
                Style::default().fg(if entry.failed {
                    theme.error
                } else {
                    theme.muted
                }),
            ));
            // Treat actual unified diffs as source review, not
            // ordinary stdout. Don't recolor random + / - logs.
            let diff = name == "shell"
                && (entry.body.contains("git diff") || entry.details.contains("diff --git "));
            for output in entry.details.lines() {
                let output_style = if diff {
                    super::super::highlight::diff_style(output, theme)
                } else {
                    Style::default().fg(if entry.failed {
                        theme.error
                    } else {
                        theme.code
                    })
                };
                let mut tab_column = 0;
                let output = markdown::expand_tabs(output, &mut tab_column);
                preformatted_rows.push(lines.len());
                lines.push(body_line(Line::from(vec![
                    Span::styled("│ ", Style::default().fg(theme.border)),
                    Span::styled(output, output_style),
                ])));
            }
        }
    }
}

impl UiState {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn append_reasoning(
        &self,
        entry: &Entry,
        theme: Theme,
        width: usize,
        lines: &mut Vec<Line<'static>>,
        preformatted_rows: &mut Vec<usize>,
        markdown_targets: &mut Vec<(String, String)>,
        reasoning_notice_shown: &mut bool,
    ) {
        if entry.body.trim().is_empty() {
            return;
        }
        if !*reasoning_notice_shown {
            let label = if self.reasoning_expanded {
                self.preferences.language.text(
                    "  ◇  Reasoning · Ctrl+R to collapse",
                    "  ◇  思考内容 · Ctrl+R 折叠",
                )
            } else {
                self.preferences.language.text(
                    "  ◇  Reasoning collapsed · Ctrl+R",
                    "  ◇  思考已折叠 · Ctrl+R",
                )
            };
            let elapsed = entry
                .duration
                .or_else(|| entry.started.map(|start| start.elapsed()))
                .map(|duration| format!(" · {:.1}s", duration.as_secs_f64()))
                .unwrap_or_default();
            lines.push(Line::styled(
                format!("{label}{elapsed}"),
                Style::default().fg(theme.muted),
            ));
            *reasoning_notice_shown = true;
            lines.push(Line::default());
        }
        if self.reasoning_expanded {
            let prose = width.saturating_sub(5);
            let prose = if self.full_width_prose() {
                prose
            } else {
                super::super::tool_display::prose_width(prose)
            };
            let (rendered, tags, code_rows) =
                markdown::render_with_links(&entry.body, theme, prose);
            for tag in tags {
                if let Some(span) = rendered
                    .get(tag.row)
                    .and_then(|row| row.spans.get(tag.span))
                {
                    markdown_targets.push((span.content.to_string(), tag.target));
                }
            }
            let base = lines.len();
            preformatted_rows.extend(code_rows.into_iter().map(|row| base + row));
            for line in rendered {
                let mut spans = line.spans;
                for span in &mut spans {
                    span.style = span.style.fg(theme.muted).add_modifier(Modifier::ITALIC);
                }
                spans.insert(0, Span::styled("│  ", Style::default().fg(theme.faint)));
                lines.push(body_line(Line::from(spans)));
            }
            lines.push(Line::default());
        }
    }
}

fn interrupted(entry: &Entry) -> bool {
    entry
        .details
        .contains("Session interrupted. Tool result is unknown;")
}

impl UiState {
    pub(super) fn tool_summary(&self, entry: &Entry) -> String {
        let name = entry
            .title
            .split_once(" · ")
            .map_or(entry.title.as_str(), |(name, _)| name);
        let summary =
            super::super::tool_display::summary(name, &entry.body, self.preferences.language);
        match self.plugin_tool_labels.get(name) {
            Some(label) if name != "shell" => format!(
                "{label} · {}",
                summary
                    .strip_prefix(name)
                    .and_then(|s| s.strip_prefix(" · "))
                    .unwrap_or(&summary)
            ),
            Some(label) => format!("{label} · {summary}"),
            None => summary,
        }
    }

    pub(super) fn tool_card(&self, tools: &[&Entry], theme: Theme, width: usize) -> ToolCard {
        let mut card = ToolCard::default();
        let Some(first) = tools.first() else {
            return card;
        };
        if tools.len() == 1 {
            self.append_tool(first, theme, width, &mut card);
            return card;
        }
        let key = first.call_id.as_deref().unwrap_or("");
        let running = tools.iter().any(|e| e.title.ends_with(" · running"));
        let failed = tools.iter().any(|e| e.failed && !interrupted(e));
        let warning = tools
            .iter()
            .any(|e| interrupted(e) || super::super::tool_display::has_diagnostic(&e.details));
        let expanded = self.tools_expanded || self.expanded_tool_groups.contains(key);
        if tools.len() > 1 {
            let caption = tools
                .iter()
                .find(|e| e.title.ends_with(" · running"))
                .or_else(|| tools.iter().find(|e| e.failed && !interrupted(e)))
                .copied()
                .unwrap_or(tools[tools.len() - 1]);
            let (glyph, ink) = if running {
                ("◌", theme.accent)
            } else if failed {
                ("✕", theme.error)
            } else if tools.iter().any(|e| interrupted(e)) {
                ("⊘", theme.warning)
            } else if warning {
                ("!", theme.warning)
            } else {
                ("✓", theme.success)
            };
            let duration = tools.iter().filter_map(|e| e.duration).sum::<Duration>();
            let elapsed = if tools.iter().any(|e| e.duration.is_some()) {
                format!(" · {:.1}s", duration.as_secs_f64())
            } else {
                String::new()
            };
            let suffix = format!(
                " · {} {}{elapsed}",
                tools.len(),
                self.preferences.language.text("steps", "步")
            );
            let summary = compact_line(
                &self.tool_summary(caption),
                width.saturating_sub(6 + UnicodeWidthStr::width(suffix.as_str())),
            );
            card.groups.push((0, key.to_owned()));
            card.lines.push(Line::from(vec![
                Span::styled(
                    format!("  {} ", if expanded { "▾" } else { "▸" }),
                    Style::default().fg(theme.faint),
                ),
                Span::styled(format!("{glyph} "), Style::default().fg(ink)),
                Span::styled(
                    summary,
                    Style::default().fg(if running || failed {
                        theme.text
                    } else {
                        theme.muted
                    }),
                ),
                Span::styled(suffix, Style::default().fg(theme.muted)),
            ]));
        }
        let show = expanded || running || tools.len() == 1;
        let mut hidden = 0;
        let limit = if tools.len() > 12 { tools.len() - 8 } else { 0 };
        for (index, entry) in tools.iter().enumerate() {
            let important = entry.failed
                || interrupted(entry)
                || super::super::tool_display::has_diagnostic(&entry.details)
                || entry
                    .call_id
                    .as_ref()
                    .is_some_and(|id| self.expanded_tool_calls.contains(id));
            if (show && index >= limit) || important {
                self.append_tool(entry, theme, width, &mut card);
            } else {
                hidden += 1;
            }
        }
        if hidden > 0 && (show || failed || warning) {
            card.groups.push((card.lines.len(), key.to_owned()));
            card.lines.push(body_line(Line::styled(
                format!(
                    "… {} {hidden} {}",
                    self.preferences.language.text("Another", "另有"),
                    self.preferences.language.text(
                        if show { "steps" } else { "successful steps" },
                        if show { "步" } else { "步成功" }
                    )
                ),
                Style::default().fg(theme.muted),
            )));
        }
        card
    }
}
