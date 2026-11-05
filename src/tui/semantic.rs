//! S2 semantic foregrounds are separate from the neutral theme/accent palette.
//! Colour conveys existing state; it never decides a tool or task outcome.
use super::palette::SemanticColors;
use super::*;

impl UiState {
    pub(super) fn semantic_colors(&self) -> SemanticColors {
        self.plain_palette().semantic
    }

    pub(super) fn semantic_step_label(&self, marker: &str) -> (&'static str, Color) {
        let p = self.semantic_colors();
        let language = self.preferences.language;
        match marker {
            "◌" => (language.text("Running", "执行中"), p.running),
            "✓" => (language.text("Done", "完成"), p.success),
            "!" => (language.text("Warning", "警告"), p.warning),
            "⊘" => (language.text("Unknown", "结果未知"), p.warning),
            _ => (language.text("Failed", "失败"), p.error),
        }
    }

    pub(super) fn plain_activity_line(&self, colors: Theme) -> Line<'static> {
        let p = self.semantic_colors();
        let language = self.preferences.language;
        if self.running {
            let label = match self.phase {
                "compacting" => language.text(
                    "Compacting (360s per attempt, up to 3 retries)",
                    "压缩中（单次 360 秒，最多重试 3 次）",
                ),
                "thinking" => language.text("Thinking", "思考中"),
                "waiting" if !self.received_model_output => language.text("Connecting", "连接中"),
                "waiting" => language.text("Waiting", "等待中"),
                "streaming" => language.text("Writing", "生成中"),
                "tool" => language.text("Running", "执行中"),
                _ => language.text("Preparing", "准备中"),
            };
            // The active tool record already names the operation; keep this
            // line about the model phase and elapsed time only.
            let elapsed = self
                .phase_since
                .map_or(0, |start| start.elapsed().as_secs());
            let details = format!(" · {elapsed}s");
            let mut spans = vec![
                Span::styled(
                    format!("{} {label}", self.activity_glyph()),
                    Style::default().fg(p.running),
                ),
                Span::styled(details, Style::default().fg(colors.muted)),
            ];
            if self.last_output.elapsed() >= Duration::from_secs(8) {
                spans.push(Span::styled(
                    language.text(" · Ctrl+C to interrupt", " · Ctrl+C 中断"),
                    Style::default().fg(colors.faint),
                ));
            }
            return Line::from(spans);
        }
        let elapsed = self
            .task_elapsed
            .map(|d| format!(" · {:.1}s", d.as_secs_f64()))
            .unwrap_or_default();
        let (label, color, details) = match self.task_outcome {
            Some(TaskOutcome::Completed) => (
                language.text("✓ Completed", "✓ 完成"),
                p.success,
                format!("{elapsed}{}", self.change_summary()),
            ),
            Some(TaskOutcome::Failed) => (
                language.text("✕ Failed", "✕ 失败"),
                p.error,
                format!(
                    "{elapsed}{}",
                    language.text(" · see error above", " · 请查看上方错误")
                ),
            ),
            Some(TaskOutcome::Cancelled) => (
                language.text("⊘ Cancelled", "⊘ 已取消"),
                colors.muted,
                format!(
                    "{elapsed}{}",
                    language.text(" · side effects are not undone", " · 已产生的操作不会撤销")
                ),
            ),
            None => (
                language.text("· Ready", "· 就绪"),
                colors.muted,
                String::new(),
            ),
        };
        Line::from(vec![
            Span::styled(label, Style::default().fg(color)),
            Span::styled(details, Style::default().fg(colors.muted)),
        ])
    }

    pub(super) fn plain_notification_line(&self, colors: Theme, width: usize) -> Line<'static> {
        let p = self.semantic_colors();
        let language = self.preferences.language;
        let (prefix, color) = match self.notification_level {
            NoticeLevel::Info => ("", colors.muted),
            NoticeLevel::Warning => (language.text("! Warning · ", "! 警告 · "), p.warning),
            NoticeLevel::Error => (language.text("✕ Error · ", "✕ 错误 · "), p.error),
        };
        let prefix = ellipsize_cells(prefix, width);
        let remaining = width.saturating_sub(UnicodeWidthStr::width(prefix.as_str()));
        let message = self.notification.replace(['\n', '\r'], " ");
        Line::from(vec![
            Span::styled(prefix, Style::default().fg(color)),
            Span::styled(
                ellipsize_cells(&message, remaining),
                Style::default().fg(colors.muted),
            ),
        ])
    }

    /// Only called for a known diff source, never arbitrary stdout or prose.
    /// Spans concatenate to the original expanded line, with no coloured fill.
    pub(super) fn plain_diff_spans(&self, line: &str, theme: Theme) -> Vec<Span<'static>> {
        let p = self.semantic_colors();
        let marker = if line.starts_with('+') && !line.starts_with("+++") {
            Some(p.add)
        } else if line.starts_with('-') && !line.starts_with("---") {
            Some(p.remove)
        } else {
            None
        };
        if let Some(color) = marker {
            vec![
                Span::styled(line[..1].to_owned(), Style::default().fg(color)),
                Span::styled(line[1..].to_owned(), Style::default().fg(theme.code)),
            ]
        } else {
            vec![Span::styled(
                line.to_owned(),
                super::super::highlight::diff_style(line, theme),
            )]
        }
    }

    pub(super) fn color_plain_diff_blocks(&self, document: &mut markdown::Document, theme: Theme) {
        for block in &document.blocks {
            if !matches!(block.language.as_str(), "diff" | "patch") {
                continue;
            }
            for row in (block.row + 1)..block.end {
                let Some(line) = document.lines.get_mut(row) else {
                    continue;
                };
                if line.spans.first().is_none_or(|span| span.content != "│ ") {
                    continue;
                }
                let source = line
                    .spans
                    .iter()
                    .skip(1)
                    .map(|span| span.content.as_ref())
                    .collect::<String>();
                let gutter = line.spans[0].clone();
                line.spans = std::iter::once(gutter)
                    .chain(self.plain_diff_spans(&source, theme))
                    .collect();
            }
        }
    }
}
