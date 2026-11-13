//! Tool cards computed from adjacent transcript entries; nothing is persisted.
use super::*;

#[derive(Default)]
pub(super) struct ToolCard {
    pub lines: Vec<Line<'static>>,
    pub rows: Vec<(usize, String)>,
    /// Header rows that expose the read-only snapshot panel. Plain limits
    /// this to failures and unknown results; legacy retains every record.
    pub inspect: Vec<(usize, String)>,
    pub preformatted: Vec<usize>,
    pub groups: Vec<(usize, String)>,
    pub blocks: Vec<(usize, usize)>,
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
        let (label, color) = if self.plain_style {
            self.semantic_step_label(marker)
        } else {
            ("", color)
        };
        let state_text = if self.plain_style {
            format!("{marker} {label} ")
        } else {
            format!("{marker} ")
        };
        let extra = if self.plain_style {
            UnicodeWidthStr::width(label) + 1
        } else {
            0
        };
        let command = compact_line(
            &summary,
            width.saturating_sub(6 + extra + UnicodeWidthStr::width(elapsed.as_str())),
        );
        if let Some(id) = entry.call_id.as_ref() {
            tool_rows.push((lines.len(), id.clone()));
        }
        lines.push(Line::from(vec![
            Span::styled(format!("  {caret} "), Style::default().fg(theme.faint)),
            Span::styled(state_text, Style::default().fg(color)),
            Span::styled(
                command,
                Style::default().fg(if entry.failed || status == "running" {
                    theme.text
                } else {
                    theme.muted
                }),
            ),
            Span::styled(elapsed, Style::default().fg(theme.faint)),
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
                let mut spans = vec![Span::styled("│ ", Style::default().fg(theme.border))];
                if self.plain_style && diff {
                    spans.extend(self.plain_diff_spans(&output, theme));
                } else {
                    spans.push(Span::styled(output, output_style));
                }
                lines.push(body_line(Line::from(spans)));
            }
        }
    }
}

impl UiState {
    #[allow(clippy::too_many_arguments)]
    /// Append a contiguous run of tools inside an existing transcript card.
    /// Keeping row offsets here preserves expansion, inspection and code copy.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn append_tool_group(
        &self,
        tools: &[&Entry],
        theme: Theme,
        width: usize,
        lines: &mut Vec<Line<'static>>,
        tool_rows: &mut Vec<(usize, String)>,
        decor: &mut Vec<(usize, Decor)>,
        preformatted_rows: &mut Vec<usize>,
    ) {
        if tools.is_empty() {
            return;
        }
        let card = self.tool_card(
            tools,
            theme,
            width.saturating_sub(if width >= inspector::MIN_ACTION_WIDTH {
                UnicodeWidthStr::width(self.inspection_label()) + 2
            } else {
                0
            }),
        );
        let base = lines.len();
        if width >= inspector::MIN_ACTION_WIDTH {
            let inspect = if self.plain_style {
                &card.inspect
            } else {
                &card.rows
            };
            decor.extend(
                inspect
                    .iter()
                    .map(|(row, id)| (base + row, Decor::Inspect(id.clone()))),
            );
        }
        tool_rows.extend(card.rows.into_iter().map(|(row, id)| (base + row, id)));
        decor.extend(
            card.groups
                .into_iter()
                .map(|(row, id)| (base + row, Decor::Group(id))),
        );
        if !self.plain_style {
            for (start, end) in card.blocks {
                decor.extend(
                    (start..end).map(|row| (base + row, Decor::Surface(Surface::ToolRecord))),
                );
            }
        }
        preformatted_rows.extend(card.preformatted.into_iter().map(|row| base + row));
        if !self.plain_style {
            decor.extend(
                (0..card.lines.len()).map(|row| (base + row, Decor::Surface(Surface::Raised))),
            );
        }
        lines.extend(card.lines);
    }

    // Kept stable while the legacy renderer remains used by regression tests.
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
        reading_rows: &mut Vec<usize>,
    ) {
        if entry.body.trim().is_empty() {
            return;
        }
        if !*reasoning_notice_shown {
            let (heading, hint) = if self.reasoning_expanded {
                (
                    self.preferences.language.text("Reasoning", "思考内容"),
                    self.preferences
                        .language
                        .text(" · Ctrl+T to collapse", " · Ctrl+T 折叠"),
                )
            } else {
                (
                    self.preferences
                        .language
                        .text("Reasoning collapsed", "思考已折叠"),
                    " · Ctrl+T",
                )
            };
            let elapsed = entry
                .duration
                .or_else(|| entry.started.map(|start| start.elapsed()))
                .map(|duration| format!(" · {:.1}s", duration.as_secs_f64()))
                .unwrap_or_default();
            let notice = Line::from(vec![
                Span::styled("◇ ", Style::default().fg(theme.faint)),
                Span::styled(heading, Style::default().fg(theme.muted)),
                Span::styled(format!("{hint}{elapsed}"), Style::default().fg(theme.muted)),
            ]);
            lines.push(if self.plain_style {
                body_line(notice)
            } else {
                Line::from(vec![
                    Span::styled("  ◇  ", Style::default().fg(theme.faint)),
                    Span::styled(heading, Style::default().fg(theme.muted)),
                    Span::styled(format!("{hint}{elapsed}"), Style::default().fg(theme.muted)),
                ])
            });
            *reasoning_notice_shown = true;
            lines.push(Line::default());
        }
        if self.reasoning_expanded {
            let prose = width.saturating_sub(if self.plain_style { 4 } else { 5 });
            let prose = if self.plain_style {
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
            for (row, line) in rendered.into_iter().enumerate() {
                let dest = lines.len();
                if code_rows.binary_search(&row).is_ok() {
                    preformatted_rows.push(dest);
                } else {
                    reading_rows.push(dest);
                }
                let mut spans = line.spans;
                // Do not render an entire long chain of thought in italic
                // with a vertical bar on every wrapped row. Keep Markdown
                // hierarchy and give reasoning quieter ink than the answer.
                for span in &mut spans {
                    span.style = span.style.fg(theme.muted);
                }
                if spans.is_empty() {
                    if lines.last().is_some_and(|line| !line.spans.is_empty()) {
                        lines.push(Line::default());
                    }
                } else {
                    spans.insert(0, Span::raw("  "));
                    lines.push(body_line(Line::from(spans)));
                }
            }
            if lines.last().is_some_and(|line| !line.spans.is_empty()) {
                lines.push(Line::default());
            }
        }
    }
}

pub(super) fn interrupted(entry: &Entry) -> bool {
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
        if self.plain_style {
            return self.plain_tool_records(tools, theme, width);
        }
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
        let show = expanded || running;
        let any_important = tools.iter().any(|entry| {
            entry.failed
                || interrupted(entry)
                || super::super::tool_display::has_diagnostic(&entry.details)
                || entry
                    .call_id
                    .as_ref()
                    .is_some_and(|id| self.expanded_tool_calls.contains(id))
        });
        let children_visible = show || any_important;
        {
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
            let state_text = format!("{glyph} ");
            let group_width = width;
            let duration = tools.iter().filter_map(|e| e.duration).sum::<Duration>();
            let elapsed = if tools.iter().any(|e| e.duration.is_some()) {
                format!(" · {:.1}s", duration.as_secs_f64())
            } else {
                String::new()
            };
            let language = self.preferences.language;
            let step_word = language.text("steps", "步");
            let (summary, suffix) = if children_visible {
                let failed_count = tools.iter().filter(|e| e.failed && !interrupted(e)).count();
                let title = if self.plain_style {
                    if failed_count > 0 {
                        format!(
                            "{} {step_word} · {failed_count} {}",
                            tools.len(),
                            language.text("failed", "步失败")
                        )
                    } else {
                        format!("{} {step_word}", tools.len())
                    }
                } else if running {
                    format!(
                        "{} {} {step_word}",
                        language.text("Running", "正在执行"),
                        tools.len()
                    )
                } else if failed_count > 0 {
                    format!(
                        "{} {} {step_word} · {failed_count} {}",
                        language.text("Executed", "已执行"),
                        tools.len(),
                        language.text("failed", "步失败")
                    )
                } else {
                    format!(
                        "{} {} {step_word}",
                        language.text("Executed", "已执行"),
                        tools.len()
                    )
                };
                let summary = compact_line(
                    &title,
                    group_width.saturating_sub(6 + UnicodeWidthStr::width(elapsed.as_str())),
                );
                (summary, elapsed)
            } else {
                // A finished, successful group is a status summary, not
                // an abbreviated version of the last shell command.
                let title = format!(
                    "{} {step_word} {} · Ctrl+O",
                    tools.len(),
                    language.text("completed", "已完成")
                );
                let summary = compact_line(
                    &title,
                    group_width.saturating_sub(6 + UnicodeWidthStr::width(elapsed.as_str())),
                );
                (summary, elapsed)
            };
            card.groups.push((0, key.to_owned()));
            card.lines.push(Line::from(vec![
                Span::styled(
                    format!("  {} ", if expanded { "▾" } else { "▸" }),
                    Style::default().fg(theme.faint),
                ),
                Span::styled(state_text, Style::default().fg(ink)),
                Span::styled(
                    summary,
                    Style::default().fg(if running || failed {
                        theme.text
                    } else {
                        theme.muted
                    }),
                ),
                Span::styled(suffix, Style::default().fg(theme.faint)),
            ]));
        }
        let mut hidden = 0;
        let limit = if tools.len() > 12 { tools.len() - 8 } else { 0 };
        for (index, entry) in tools.iter().enumerate() {
            let important = entry.failed
                || (self.plain_style && entry.title.ends_with(" · running"))
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
            let step_noun = if show {
                self.preferences
                    .language
                    .text(if hidden == 1 { "step" } else { "steps" }, "步")
            } else {
                self.preferences.language.text(
                    if hidden == 1 {
                        "successful step"
                    } else {
                        "successful steps"
                    },
                    "步成功",
                )
            };
            card.groups.push((card.lines.len(), key.to_owned()));
            card.lines.push(body_line(Line::styled(
                format!(
                    "… {} {hidden} {step_noun}",
                    self.preferences.language.text("Another", "另有")
                ),
                Style::default().fg(theme.faint),
            )));
        }
        card
    }
}
