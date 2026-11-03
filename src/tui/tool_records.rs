//! One visible record per tool invocation. Only that invocation's long details
//! are collapsible; adjacent calls never become a group or a hidden-step count.
//! Display sanitising removes terminal control sequences without changing the
//! stored tool result or the model-facing transcript.
use super::transcript::{interrupted, ToolCard};
use super::*;

/// Strip CSI/OSC and other escape sequences, then make remaining control bytes
/// visible. The raw entry is never modified.
fn visible_line(value: &str) -> String {
    let chars = value.chars().collect::<Vec<_>>();
    let mut out = String::with_capacity(value.len());
    let mut index = 0;
    while index < chars.len() {
        let ch = chars[index];
        if ch == '\u{1b}' {
            match chars.get(index + 1) {
                Some('[') => {
                    index += 2;
                    while index < chars.len() && !('\u{40}'..='\u{7e}').contains(&chars[index]) {
                        index += 1;
                    }
                    index = (index + 1).min(chars.len());
                }
                Some(']') => {
                    index += 2;
                    while index < chars.len() {
                        if chars[index] == '\u{7}' {
                            index += 1;
                            break;
                        }
                        if chars[index] == '\u{1b}' && chars.get(index + 1) == Some(&'\\') {
                            index += 2;
                            break;
                        }
                        index += 1;
                    }
                }
                Some(_) => index += 2,
                None => index += 1,
            }
            continue;
        }
        if ch == '\r' {
            index += 1;
            continue;
        }
        if ch == '\t' {
            out.push_str("    ");
        } else if ch.is_control() {
            out.push('�');
        } else {
            out.push(ch);
        }
        index += 1;
    }
    out
}

fn clip_line(value: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let value = visible_line(value);
    if UnicodeWidthStr::width(value.as_str()) <= width {
        return value;
    }
    let mut out = String::new();
    let mut used = 0;
    for part in value.graphemes(true) {
        let cells = UnicodeWidthStr::width(part);
        if used + cells > width - 1 {
            break;
        }
        out.push_str(part);
        used += cells;
    }
    out.push('…');
    out
}

/// Split a display command only on unquoted `&&` / `;`. This is not a shell
/// parser; it exists so quoted arguments keep their separators and the quiet
/// summary never shows a fragment as a fake command.
fn shell_segments(command: &str) -> Vec<&str> {
    let bytes = command.as_bytes();
    let mut segments = Vec::new();
    let mut start = 0;
    let mut index = 0;
    let mut quote = None;
    let mut escaped = false;
    while index < bytes.len() {
        let byte = bytes[index];
        if escaped {
            escaped = false;
            index += 1;
            continue;
        }
        if let Some(open) = quote {
            if open == b'"' && byte == b'\\' {
                escaped = true;
            } else if byte == open {
                quote = None;
            }
            index += 1;
            continue;
        }
        match byte {
            b'\\' => {
                escaped = true;
                index += 1;
            }
            b'\'' | b'"' => {
                quote = Some(byte);
                index += 1;
            }
            b';' => {
                segments.push(&command[start..index]);
                start = index + 1;
                index += 1;
            }
            b'&' if bytes.get(index + 1) == Some(&b'&') => {
                segments.push(&command[start..index]);
                index += 2;
                start = index;
            }
            _ => index += 1,
        }
    }
    segments.push(&command[start..]);
    segments
}

/// Whether a segment redirects, pipes, substitutes, expands, backgrounds or
/// otherwise acts beyond printing literal text. Quoting is tracked so a quoted
/// `;` or `&&` stays inert, while `$` and backticks still expand inside double
/// quotes. An unbalanced quote or a dangling escape counts as a side effect:
/// the segment is then shown verbatim instead of folded away.
fn has_shell_side_effect(segment: &str) -> bool {
    let bytes = segment.as_bytes();
    let mut index = 0;
    let mut quote = None;
    let mut escaped = false;
    while index < bytes.len() {
        let byte = bytes[index];
        if escaped {
            escaped = false;
            index += 1;
            continue;
        }
        if let Some(open) = quote {
            if open == b'"' && byte == b'\\' {
                escaped = true;
            } else if byte == open {
                quote = None;
            } else if open == b'"' && (byte == b'$' || byte == b'`') {
                return true;
            }
            index += 1;
            continue;
        }
        match byte {
            b'\\' => escaped = true,
            b'\'' | b'"' => quote = Some(byte),
            b'>' | b'<' | b'|' | b'&' | b';' | b'$' | b'`' | b'(' | b')' | b'{' | b'}' | b'*'
            | b'?' | b'[' | b']' | b'#' | b'!' | b'\n' => return true,
            _ => {}
        }
        index += 1;
    }
    quote.is_some() || escaped
}

/// The text after `word` when a segment starts with it. `echo"x"` matches, so
/// the quoting style of the separator does not change how it is classified.
fn scaffolding_rest<'a>(segment: &'a str, word: &str) -> Option<&'a str> {
    let end = segment
        .find(|ch: char| ch.is_whitespace() || ch == '"' || ch == '\'')
        .unwrap_or(segment.len());
    (&segment[..end] == word).then(|| segment[end..].trim())
}

/// `echo` is display-only scaffolding while it just prints literal text. A
/// redirection, pipeline, substitution, expansion, subshell or comment makes it
/// a real command, so it stays visible exactly as written.
fn is_separator_echo(segment: &str) -> bool {
    scaffolding_rest(segment, "echo").is_some_and(|rest| !has_shell_side_effect(rest))
}

/// One directory argument: either a single quoted run or a run without
/// whitespace or stray quotes. `cd a b` is not one target.
fn is_literal_cd_target(target: &str) -> bool {
    if let Some(quote @ ('"' | '\'')) = target.chars().next() {
        let inner = target
            .strip_prefix(quote)
            .and_then(|rest| rest.strip_suffix(quote))
            .unwrap_or("");
        return !inner.is_empty() && !inner.contains(quote);
    }
    !target.is_empty() && !target.contains(char::is_whitespace) && !target.contains(['"', '\''])
}

/// A leading `cd /repo` only sets the working directory of the commands that
/// follow, so it can fold away. `cd` with an expansion, redirection, comment or
/// ambiguous target keeps its exact text; a `cd` in the middle of a chain is
/// part of that chain and is never removed.
fn is_safe_cd_preamble(segment: &str) -> bool {
    scaffolding_rest(segment, "cd")
        .is_some_and(|target| is_literal_cd_target(target) && !has_shell_side_effect(target))
}

/// Remove shell scaffolding from a successful call: a leading `cd`, literal
/// separator echoes, and empty segments. Anything that can change files or
/// state keeps its exact text. The exact original command remains available
/// when the record is expanded.
fn compact_success_command(command: &str) -> String {
    let mut parts = Vec::new();
    let mut command_shown = false;
    for piece in shell_segments(command) {
        let piece = piece.trim();
        if piece.is_empty() || is_separator_echo(piece) {
            continue;
        }
        if !command_shown && is_safe_cd_preamble(piece) {
            continue;
        }
        command_shown = true;
        parts.push(piece);
    }
    if parts.is_empty() {
        command.lines().next().unwrap_or("").trim().to_owned()
    } else {
        parts.join(" · ")
    }
}

impl UiState {
    pub(super) fn plain_tool_records(
        &self,
        tools: &[&Entry],
        theme: Theme,
        width: usize,
    ) -> ToolCard {
        let mut card = ToolCard::default();
        for entry in tools {
            let start = card.lines.len();
            self.append_plain_record(entry, theme, width, &mut card);
            // Successful calls stay on the shared message surface; only active,
            // failed, unknown or diagnostic records get a panel tint.
            let warning = !entry.failed
                && !entry.title.ends_with(" · running")
                && super::super::tool_display::has_diagnostic(&entry.details);
            if entry.failed || entry.title.ends_with(" · running") || warning || interrupted(entry)
            {
                card.blocks.push((start, card.lines.len()));
            }
        }
        card
    }

    fn append_plain_record(&self, entry: &Entry, theme: Theme, width: usize, card: &mut ToolCard) {
        let language = self.preferences.language;
        let (name, state) = entry
            .title
            .split_once(" · ")
            .unwrap_or((&entry.title, "done"));
        let unknown = interrupted(entry);
        let running = state == "running";
        let warning =
            !running && !entry.failed && super::super::tool_display::has_diagnostic(&entry.details);
        let marker = if unknown {
            "⊘"
        } else if entry.failed {
            "✕"
        } else if running {
            "◌"
        } else if warning {
            "!"
        } else {
            "✓"
        };
        let (label, ink) = self.semantic_step_label(marker);
        let expanded = self.tools_expanded
            || entry
                .call_id
                .as_ref()
                .is_some_and(|id| self.expanded_tool_calls.contains(id));
        // Successful shell calls are one quiet line. Drop the `cd … && echo`
        // scaffolding that made every record look like a shell transcript;
        // expanding the record still shows the exact original command.
        let summary = if name == "shell" {
            let mut commands = entry.body.lines().filter(|line| !line.trim().is_empty());
            let first = commands.next().unwrap_or("");
            let more_lines = commands.next().is_some();
            let line = if marker == "✓" {
                compact_success_command(first)
            } else {
                first.to_owned()
            };
            if line.is_empty() {
                "$".to_owned()
            } else if more_lines {
                format!("$ {line} …")
            } else {
                format!("$ {line}")
            }
        } else {
            self.tool_summary(entry)
        };
        let duration = entry.duration.or_else(|| {
            running
                .then(|| entry.started.map(|start| start.elapsed()))
                .flatten()
        });
        // Sub-second successes stay silent; active and failed records keep
        // their timing because it explains what the reader is waiting on.
        let show_elapsed = duration.is_some_and(|duration| {
            if marker == "✓" {
                duration.as_secs_f64() >= 1.0
            } else {
                duration.as_secs_f64() >= 0.1
            }
        });
        let elapsed = if show_elapsed {
            duration
                .map(|duration| format!(" · {:.1}s", duration.as_secs_f64()))
                .unwrap_or_default()
        } else {
            String::new()
        };
        // Keep the colored state word for attention states, not for success.
        let state = if marker == "✓" {
            String::new()
        } else {
            format!("{label} ")
        };
        // Keep the tool text clear of the surface edge, but do not paint a
        // disclosure badge. The row remains expandable from its state marker
        // and through Ctrl+O.
        let prefix_width = 4
            + 2
            + UnicodeWidthStr::width(state.as_str())
            + UnicodeWidthStr::width(elapsed.as_str());
        let summary = clip_line(&summary, width.saturating_sub(2 + prefix_width));
        if entry.failed || unknown {
            if let Some(id) = &entry.call_id {
                card.inspect.push((card.lines.len(), id.clone()));
            }
        }
        if let Some(id) = &entry.call_id {
            card.rows.push((card.lines.len(), id.clone()));
        }
        card.lines.push(Line::from(vec![
            Span::raw("    "),
            Span::styled(format!("{marker} "), Style::default().fg(ink)),
            Span::styled(state, Style::default().fg(ink)),
            Span::styled(
                summary,
                Style::default().fg(if marker == "✓" {
                    theme.muted
                } else {
                    theme.text
                }),
            ),
            Span::styled(elapsed, Style::default().fg(theme.faint)),
        ]));

        if expanded {
            card.lines.push(body_line(Line::styled(
                language.text("Command", "执行命令"),
                Style::default().fg(theme.muted),
            )));
            for command in entry.body.lines() {
                self.record_command_line(command, name, theme, None, card);
            }
        }
        if unknown {
            card.lines.push(body_line(Line::styled(
                language.text(
                    "Result unknown; will not rerun automatically",
                    "结果未知，不会自动重新执行",
                ),
                Style::default().fg(theme.muted),
            )));
            return;
        }
        if expanded {
            card.lines.push(body_line(Line::styled(
                language.text("Output", "执行输出"),
                Style::default().fg(theme.muted),
            )));
            if entry.details.is_empty() {
                card.lines.push(body_line(Line::styled(
                    language.text("No output recorded", "暂无已记录输出"),
                    Style::default().fg(theme.faint),
                )));
            }
            let diff = name == "shell"
                && (entry.body.contains("git diff") || entry.details.contains("diff --git "));
            for output in entry.details.lines() {
                let mut column = 0;
                let output = visible_line(&markdown::expand_tabs(output, &mut column));
                card.preformatted.push(card.lines.len());
                let spans = if diff {
                    self.plain_diff_spans(&output, theme)
                } else {
                    vec![Span::styled(output, Style::default().fg(theme.code))]
                };
                card.lines.push(body_line(Line::from(spans)));
            }
            return;
        }
        // Successful calls stay at two rows. Running, failed and diagnostic
        // calls expose a bounded preview of their existing captured output.
        if !running && !entry.failed && !warning {
            return;
        }
        let limit = if running { 8 } else { 2 };
        let source = entry
            .details
            .lines()
            .map(str::trim_end)
            .filter(|line| !line.trim().is_empty() && !line.trim_start().starts_with("exit: "))
            .map(|line| line.strip_prefix("stderr: ").unwrap_or(line));
        let mut candidates = if running {
            source.rev().take(limit + 1).collect::<Vec<_>>()
        } else {
            source.take(limit + 1).collect::<Vec<_>>()
        };
        if running {
            candidates.reverse();
        }
        let mut preview: Vec<String> = Vec::new();
        if running {
            let selected = &candidates[candidates.len().saturating_sub(limit)..];
            for &line in selected {
                if preview.len() >= limit {
                    break;
                }
                if !preview.iter().any(|shown| shown.contains(line)) {
                    preview.push(line.to_owned());
                }
            }
        } else if entry.failed {
            preview.push(failure_summary(&entry.details));
            if let Some(diagnostic) = candidates.iter().find(|line| {
                super::super::tool_display::has_diagnostic(line)
                    || line.to_ascii_lowercase().contains("fatal:")
                    || line.to_ascii_lowercase().contains("error")
            }) {
                if !preview.iter().any(|shown| shown.contains(*diagnostic)) {
                    preview.push((*diagnostic).to_owned());
                }
            }
        } else if warning {
            if let Some(diagnostic) = entry
                .details
                .lines()
                .find(|line| super::super::tool_display::has_diagnostic(line))
            {
                preview.push(
                    diagnostic
                        .strip_prefix("stderr: ")
                        .unwrap_or(diagnostic)
                        .to_owned(),
                );
            }
        }
        if running && preview.is_empty() {
            preview.push(language.text("Waiting for output…", "等待工具输出…").into());
        }
        let omitted = candidates.len() > preview.len();
        let preview_len = preview.len();
        for (index, line) in preview.into_iter().enumerate() {
            let suffix = if omitted && index + 1 == preview_len && !running {
                " …"
            } else {
                ""
            };
            let mut column = 0;
            let line = markdown::expand_tabs(&line, &mut column);
            let line = if running && omitted && index == 0 {
                format!("… {line}")
            } else {
                line
            };
            let max = width.saturating_sub(6 + UnicodeWidthStr::width(suffix));
            card.preformatted.push(card.lines.len());
            card.lines.push(body_line(Line::from(vec![
                Span::styled("↳ ", Style::default().fg(theme.faint)),
                Span::styled(
                    format!("{}{suffix}", clip_line(&line, max)),
                    Style::default().fg(theme.muted),
                ),
            ])));
        }
    }

    fn record_command_line(
        &self,
        command: &str,
        name: &str,
        theme: Theme,
        max: Option<usize>,
        card: &mut ToolCard,
    ) {
        let mut column = 0;
        let command = markdown::expand_tabs(command, &mut column);
        let prefix = if name == "shell" { "$ " } else { "args: " };
        let text = visible_line(&format!("{prefix}{command}"));
        let text = max.map_or_else(|| text.clone(), |max| clip_line(&text, max));
        card.preformatted.push(card.lines.len());
        card.lines.push(body_line(Line::from(Span::styled(
            text,
            Style::default().fg(theme.code),
        ))));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn success_summary_does_not_split_inside_quotes_or_escapes() {
        assert_eq!(
            compact_success_command("cd /repo && echo \"===\" && ls && git status"),
            "ls · git status"
        );
        assert_eq!(
            compact_success_command("printf '%s && %s' a b && git status"),
            "printf '%s && %s' a b · git status"
        );
        assert_eq!(
            compact_success_command("echo \"a; b\" && ls"),
            "ls",
            "a quoted semicolon must not create a fake command fragment"
        );
        assert_eq!(
            compact_success_command(r"echo a\;b && ls"),
            "ls",
            "an escaped separator must not split the command"
        );
        assert_eq!(
            compact_success_command("echo \"a; b\""),
            "echo \"a; b\"",
            "a command made only of scaffolding stays readable"
        );
    }

    #[test]
    fn redirected_echo_keeps_its_exact_segment() {
        assert_eq!(
            compact_success_command("echo overwritten > config.toml && cargo test"),
            "echo overwritten > config.toml · cargo test"
        );
        assert_eq!(
            compact_success_command("cd /repo && echo note >> log.md && cargo test"),
            "echo note >> log.md · cargo test"
        );
        assert_eq!(
            compact_success_command("echo skipped | tee log.txt && cargo test"),
            "echo skipped | tee log.txt · cargo test"
        );
        assert_eq!(
            compact_success_command("echo x >&2 && cargo test"),
            "echo x >&2 · cargo test"
        );
    }

    #[test]
    fn substituting_echo_keeps_its_exact_segment() {
        assert_eq!(
            compact_success_command("echo $(date) && cargo test"),
            "echo $(date) · cargo test"
        );
        assert_eq!(
            compact_success_command("echo `date` && cargo test"),
            "echo `date` · cargo test"
        );
        assert_eq!(
            compact_success_command("echo \"built $(date)\" && cargo test"),
            "echo \"built $(date)\" · cargo test"
        );
        assert_eq!(
            compact_success_command("echo $HOME && cargo test"),
            "echo $HOME · cargo test"
        );
    }

    #[test]
    fn quoted_literal_separators_still_fold_away() {
        assert_eq!(
            compact_success_command("cd \"/d/My Repo\" && echo '===' && cargo test"),
            "cargo test"
        );
        assert_eq!(compact_success_command("echo 'a && b' && ls"), "ls");
        assert_eq!(compact_success_command("echo \"a; b\" && ls"), "ls");
    }

    #[test]
    fn safe_scaffolding_still_compacts() {
        assert_eq!(
            compact_success_command("cd /repo && echo \"===\" && ls -la"),
            "ls -la"
        );
        assert_eq!(
            compact_success_command("cd /repo && cargo test"),
            "cargo test"
        );
        assert_eq!(compact_success_command("pwd"), "pwd");
    }

    #[test]
    fn unsafe_cd_stays_visible() {
        assert_eq!(
            compact_success_command("cd $BUILD && cargo test"),
            "cd $BUILD · cargo test"
        );
        assert_eq!(
            compact_success_command("cd \"$(mktemp -d)\" && cargo test"),
            "cd \"$(mktemp -d)\" · cargo test"
        );
        assert_eq!(
            compact_success_command("cargo build && cd /tmp && make"),
            "cargo build · cd /tmp · make"
        );
        assert_eq!(
            compact_success_command("cd /repo"),
            "cd /repo",
            "a record made only of scaffolding stays readable"
        );
    }
}
