use super::*;

impl UiState {
    pub fn save_usage(&self) -> anyhow::Result<()> {
        if self.home {
            return Ok(());
        }
        if let Some(catalog) = &self.model_catalog {
            catalog.set_usage(
                &self.session,
                crate::sessions::SessionUsage {
                    coding: self.usage.clone(),
                    title: self.title_usage.clone(),
                    apec: self.apec_usage.clone(),
                    context_input: self.context_input,
                    context_estimated: self.context_estimated,
                },
            )?;
        }
        Ok(())
    }

    pub fn restore_usage(&mut self) -> anyhow::Result<()> {
        let snapshot = match &self.model_catalog {
            Some(catalog) if !self.home => catalog.usage(&self.session)?,
            _ => crate::sessions::SessionUsage::default(),
        };
        self.usage = snapshot.coding;
        self.title_usage = snapshot.title;
        self.apec_usage = snapshot.apec;
        self.context_input = snapshot.context_input;
        self.context_estimated = snapshot.context_estimated;
        Ok(())
    }

    pub fn usage_report(&self) -> String {
        let language = self.preferences.language;
        let mut rows = vec![
            self.context_display()
                .replace("Ctx", language.text("Context", "上下文")),
            self.usage_variants()[0].clone(),
        ];
        if language == crate::tui::preferences::Language::En {
            rows.push(
                self.usage
                    .report_breakdown(&self.title_usage, &self.apec_usage),
            );
        } else {
            rows.push("本会话已记录用量（跨重启保存；旧版未记录的请求不可恢复）：".into());
            for (label, meter) in [
                ("编程模型", &self.usage),
                ("自动标题", &self.title_usage),
                ("APEC 摘要", &self.apec_usage),
            ] {
                rows.push(
                    meter
                        .summary_line(label)
                        .replace("call(s)", "次请求")
                        .replace("In ", "输入 ")
                        .replace("Out ", "输出 ")
                        .replace("Cache read ", "缓存读取 ")
                        .replace("Hit ", "命中率 ")
                        .replace("Reasoning ", "推理 "),
                );
            }
            rows.push("缓存读取属于输入的一部分；推理属于输出的一部分。".into());
            rows.push("— 表示未报告；≥ 表示部分请求未报告；~ 表示估算或部分测量。".into());
            rows.push("APEC 仅统计已完成的摘要；未完成或失败的请求可能产生未报告的费用。".into());
        }
        rows.extend(self.plugin_statuses.iter().cloned());
        rows.join("\n")
    }

    /// The three measurement widths, longest first. Kept separate so callers
    /// can ask whether a variant fits *whole* before spending the row on it.
    pub(in crate::tui) fn usage_variants(&self) -> [String; 3] {
        use crate::usage::compact_tokens;
        let language = self.preferences.language;
        let last = self.usage.last_request();
        let unknown = language.text("unreported", "未报告");
        let count =
            |value: Option<u64>| value.map(compact_tokens).unwrap_or_else(|| unknown.into());
        let input = count(last.and_then(|usage| usage.input));
        let output = count(last.and_then(|usage| usage.output));
        let cache = match (last.and_then(|u| u.input), last.and_then(|u| u.cached)) {
            (Some(input), Some(cached)) if input > 0 && cached <= input => {
                crate::usage::UsageMeter::cache_percent(cached, input)
            }
            _ => unknown.into(),
        };
        [
            format!(
                "{} {input} / {} {output} · {} {cache}",
                language.text("Last input", "上次输入"),
                language.text("output", "输出"),
                language.text("Cache", "缓存命中")
            ),
            format!(
                "{} {input} / {output} · {} {cache}",
                language.text("In/out", "输入/输出"),
                language.text("Cache", "缓存")
            ),
            format!("{} {cache}", language.text("Cache", "缓存")),
        ]
    }

    /// A measurement string that fits `width` without truncation, if one exists.
    #[cfg(test)]
    pub(in crate::tui) fn usage_status_fitting(&self, width: usize) -> Option<String> {
        self.usage_variants()
            .into_iter()
            .find(|value| UnicodeWidthStr::width(value.as_str()) <= width)
    }

    #[cfg(test)]
    pub(in crate::tui) fn usage_status(&self, width: usize) -> String {
        self.usage_status_fitting(width)
            .unwrap_or_else(|| ellipsize_cells(&self.usage_variants()[2], width))
    }
    /// One quiet footer. Usage and cache readings are secondary to the
    /// conversation. Detailed usage stays in /usage, not in a second status row.
    pub(in crate::tui) fn footer_line(&self, colors: Theme, width: usize) -> Line<'static> {
        if self.plain_style {
            return self.plain_footer_line(colors, width);
        }
        let language = self.preferences.language;
        let context = self
            .compact_context()
            .replace("Ctx", language.text("Context", "上下文"));
        let thinking = format!(
            "{} {}",
            language.text("Think", "思考"),
            self.thinking.as_deref().unwrap_or("—")
        );
        let metadata = format!("{} · {thinking} · {context}", self.model);
        let mut chosen = if UnicodeWidthStr::width(metadata.as_str()) <= width {
            metadata
        } else {
            self.footer_status(width)
        };
        if width >= 110 && !self.plugin_statuses.is_empty() {
            let expanded = format!("{chosen} · {}", self.plugin_statuses.join(" · "));
            if UnicodeWidthStr::width(expanded.as_str()) <= width {
                chosen = expanded;
            }
        }
        let mut spans = Vec::new();
        for (index, section) in chosen.split(" · ").enumerate() {
            if index > 0 {
                spans.push(Span::styled(" · ", Style::default().fg(colors.faint)));
            }
            let ink = if section.starts_with("Cache ") || section.starts_with("缓存 ") {
                colors.faint
            } else if section.starts_with("Context ") || section.starts_with("上下文 ") {
                match (self.context_input, self.context_window) {
                    (Some(input), window) if window > 0 && input as f64 / window as f64 >= 0.9 => {
                        colors.error
                    }
                    (Some(input), window) if window > 0 && input as f64 / window as f64 >= 0.7 => {
                        colors.warning
                    }
                    _ => colors.muted,
                }
            } else {
                colors.muted
            };
            spans.push(Span::styled(section.to_owned(), Style::default().fg(ink)));
        }
        let hint = self
            .preferences
            .language
            .text("Ctrl+P Commands", "Ctrl+P 命令");
        let used = spans
            .iter()
            .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
            .sum::<usize>();
        let hint_width = UnicodeWidthStr::width(hint);
        if width >= 100 && used + hint_width + 3 <= width && self.notification.is_empty() {
            spans.push(Span::raw(" ".repeat(width - used - hint_width)));
            spans.push(Span::styled(hint, Style::default().fg(colors.muted)));
        }
        Line::from(spans)
    }

    pub fn configure_status(&mut self, window: usize, thinking: Option<String>) {
        self.context_window = window;
        self.thinking = thinking;
    }

    pub fn reset_context(&mut self) {
        self.context_input = None;
        self.context_estimated = true;
    }

    pub(in crate::tui) fn workdir_label(&self) -> String {
        let display = strip_verbatim_path(&self.link_workdir.display().to_string());
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from)
            .map(|path| strip_verbatim_path(&path.display().to_string()));
        if let Some(home) = home {
            let separator = std::path::MAIN_SEPARATOR;
            if display == home {
                return "~".into();
            }
            if let Some(rest) = display.strip_prefix(&format!("{home}{separator}")) {
                return format!("~{separator}{rest}");
            }
        }
        display
    }

    pub fn context_display(&self) -> String {
        use crate::usage::compact_tokens;
        let window = self.context_window as u64;
        if window == 0 {
            return "Ctx —".into();
        }
        match self.context_input {
            Some(input) => {
                let percent = Self::context_percent(input, window);
                let marker = if self.context_estimated { "~" } else { "" };
                format!(
                    "Ctx {marker}{} / {} · {marker}{percent}%",
                    compact_tokens(input),
                    compact_tokens(window)
                )
            }
            None => format!("Ctx — / {}", compact_tokens(window)),
        }
    }

    pub(in crate::tui) fn context_percent(input: u64, window: u64) -> String {
        let percent = input as f64 * 100.0 / window as f64;
        if percent > 0.0 && percent < 0.1 {
            format!("{percent:.2}")
        } else if percent < 1.0 {
            format!("{percent:.1}")
        } else {
            format!("{percent:.0}")
        }
    }

    pub(in crate::tui) fn compact_context(&self) -> String {
        match (self.context_input, self.context_window) {
            (Some(input), window) if window > 0 => {
                let percent = Self::context_percent(input, window as u64);
                format!(
                    "Ctx {}{percent}%",
                    if self.context_estimated { "~" } else { "" }
                )
            }
            _ => "Ctx —".into(),
        }
    }

    pub(in crate::tui) fn footer_status(&self, width: usize) -> String {
        let language = self.preferences.language;
        let think = format!(
            "{} {}",
            language.text("Reasoning", "推理"),
            self.thinking.as_deref().unwrap_or("—")
        );
        let context = self
            .compact_context()
            .replace("Ctx", language.text("Context", "上下文"));
        let separator = " · ";
        let metadata_width = UnicodeWidthStr::width(think.as_str())
            + UnicodeWidthStr::width(context.as_str())
            + 2 * UnicodeWidthStr::width(separator);
        let queued = if self.queued.is_empty() {
            String::new()
        } else {
            format!("{separator}⏸ {}", self.queued.len())
        };
        if width >= 46 && metadata_width + 8 <= width {
            let model = ellipsize_cells(&self.model, (width - metadata_width).min(24));
            return format!("{model}{separator}{think}{separator}{context}{queued}");
        }
        if width >= 30 {
            let level = self.thinking.as_deref().unwrap_or("—");
            let context = self
                .compact_context()
                .replace("Ctx", language.text("Context", "上下文"));
            let right = format!("{level} · {context}");
            let available = width.saturating_sub(UnicodeWidthStr::width(right.as_str()) + 3);
            return format!("{} · {right}", ellipsize_cells(&self.model, available));
        }
        ellipsize_cells(&format!("{} · {}{queued}", self.model, context), width)
    }

    pub fn note(&mut self, text: impl Into<String>) {
        self.note_with_level(NoticeLevel::Info, text);
    }

    pub(in crate::tui) fn note_with_level(&mut self, level: NoticeLevel, text: impl Into<String>) {
        self.notification_level = level;
        self.notification = text.into();
        if self.preferences.language == crate::tui::preferences::Language::Zh {
            self.notification = self
                .notification
                .replace("【Clipboard image】", "【剪贴板图片】");
        }
    }

    pub(in crate::tui) fn set_phase(&mut self, phase: &'static str) {
        if self.phase != phase {
            self.phase = phase;
            self.phase_since = (phase != "ready").then(Instant::now);
        }
    }

    /// The task loop already wakes every 40ms; paint about two frames per
    /// second while active, without reflowing history or polling the provider.
    pub fn tick_activity(&mut self) -> bool {
        // Keep the pulse calm; the composer draws a software caret and the
        // terminal's hardware cursor remains hidden during these redraws.
        if !self.running || self.last_activity_frame.elapsed() < Duration::from_millis(500) {
            return false;
        }
        self.last_activity_frame = Instant::now();
        let frame_count = if self.plain_style || self.plugin_activity_frames.is_empty() {
            ACTIVITY_FRAMES.len()
        } else {
            self.plugin_activity_frames.len()
        };
        self.activity_frame = (self.activity_frame + 1) % frame_count;
        true
    }

    /// The animated glyph for the live activity row. Plugin frames decorate
    /// the logo only; the plain status keeps Kroma's own pulse so third-party
    /// text never leaks into the transcript.
    pub(in crate::tui) fn activity_glyph(&self) -> &'static str {
        ACTIVITY_FRAMES[self.activity_frame % ACTIVITY_FRAMES.len()]
    }

    pub(in crate::tui) fn activity_status(&self) -> String {
        if !self.running {
            return self.preferences.language.text("Ready", "就绪").into();
        }
        let phase = match self.phase {
            "compacting" => self.preferences.language.text(
                "Compacting context (75s per attempt, up to 1 retry)",
                "正在压缩上下文（单次 75 秒，最多重试 1 次）",
            ),
            "thinking" => self.preferences.language.text("Thinking", "正在思考"),
            "waiting" if !self.received_model_output => {
                self.preferences.language.text("Connecting", "正在连接模型")
            }
            "waiting" => self
                .preferences
                .language
                .text("Waiting for model", "等待模型响应"),
            "streaming" => self.preferences.language.text("Writing", "正在生成回复"),
            "tool" => self
                .preferences
                .language
                .text("Running tool", "正在执行工具"),
            _ => self.preferences.language.text("Preparing", "正在准备"),
        };
        let elapsed = self
            .phase_since
            .map_or(0, |start| start.elapsed().as_secs());
        let phase = if self.phase == "tool" {
            self.entries
                .iter()
                .rev()
                .find(|entry| {
                    matches!(entry.kind, Kind::Tool) && entry.title.ends_with(" · running")
                })
                .map(|entry| {
                    if self.plain_style {
                        self.tool_summary(entry)
                    } else {
                        format!(
                            "{} {}",
                            self.preferences.language.text("Running", "正在运行"),
                            self.tool_summary(entry)
                        )
                    }
                })
                .unwrap_or_else(|| phase.to_owned())
        } else {
            phase.to_owned()
        };
        format!("{phase} · {elapsed}s")
    }

    pub(in crate::tui) fn activity_line(&self, colors: Theme) -> Line<'static> {
        if self.plain_style {
            return self.plain_activity_line(colors);
        }
        if !self.running {
            // The outcome marker matches the transcript markers above, so a
            // finished turn is recognisable without reading the words.
            let (marker, color) = match self.task_outcome {
                Some(TaskOutcome::Completed) => ("·", colors.faint),
                Some(TaskOutcome::Failed) => ("✕", colors.error),
                Some(TaskOutcome::Cancelled) => ("⊘", colors.muted),
                None => ("·", colors.muted),
            };
            return Line::from(vec![
                Span::styled(format!("{marker}  "), Style::default().fg(color)),
                Span::styled(
                    self.task_status(),
                    Style::default().fg(
                        if matches!(self.task_outcome, Some(TaskOutcome::Failed)) {
                            colors.error
                        } else {
                            colors.muted
                        },
                    ),
                ),
            ]);
        }
        if let Some(frame) = self.plugin_activity_frames.get(self.activity_frame) {
            return Line::from(vec![
                Span::styled(frame.clone(), Style::default().fg(colors.accent)),
                Span::raw("  "),
                Span::styled(self.activity_status(), Style::default().fg(colors.text)),
            ]);
        }
        let mut spans = Vec::with_capacity(5);
        for (index, ink) in [colors.accent, colors.text, colors.user]
            .into_iter()
            .enumerate()
        {
            let active = index == self.activity_frame / 2;
            let faded = Theme::blend(ink, colors.background, 0.18);
            spans.push(Span::styled(
                "▲",
                Style::default()
                    .fg(if active { ink } else { faded })
                    .add_modifier(if active {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
            ));
        }
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            self.activity_status(),
            Style::default().fg(colors.muted),
        ));
        if self.last_output.elapsed() >= Duration::from_secs(8) {
            spans.push(Span::styled(
                self.preferences
                    .language
                    .text(" · Ctrl+C to interrupt", " · Ctrl+C 中断"),
                Style::default().fg(colors.faint),
            ));
        }
        Line::from(spans)
    }
}
