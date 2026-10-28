//! Styled Markdown for a terminal. Keep the parser separate from Agent and
//! leave layout/wrapping to Ratatui rather than maintaining our own renderer.
use super::theme::Theme;
use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};
use unicode_width::UnicodeWidthStr;

fn blank_after(lines: &mut Vec<Line<'static>>) {
    if lines.last().is_some_and(|line| !line.spans.is_empty()) {
        lines.push(Line::default());
    }
}

#[derive(Default)]
struct TableContent {
    header: Vec<String>,
    rows: Vec<Vec<String>>,
    current: Vec<String>,
    cell: String,
}

fn padded_cell(cell: &str, width: usize) -> String {
    format!(
        "{cell}{}",
        " ".repeat(width.saturating_sub(UnicodeWidthStr::width(cell)))
    )
}

/// Markdown tables need aligned display cells, not a series of inline pipes.
/// On narrow screens, fall back to compact labeled rows rather than breaking
/// unrelated columns across several lines.
fn render_table(lines: &mut Vec<Line<'static>>, table: TableContent, theme: Theme, width: usize) {
    let columns = table
        .header
        .len()
        .max(table.rows.iter().map(Vec::len).max().unwrap_or(0));
    if columns == 0 {
        return;
    }
    let widths: Vec<_> = (0..columns)
        .map(|column| {
            table
                .header
                .get(column)
                .into_iter()
                .chain(table.rows.iter().filter_map(|row| row.get(column)))
                .map(|cell| UnicodeWidthStr::width(cell.as_str()))
                .max()
                .unwrap_or(0)
        })
        .collect();
    let fit =
        columns <= 4 && widths.iter().sum::<usize>() + 3 * columns.saturating_sub(1) + 2 <= width;
    if fit {
        for (index, row) in std::iter::once(&table.header)
            .chain(table.rows.iter())
            .enumerate()
        {
            let mut spans = vec![Span::raw("  ")];
            for (column, cell_width) in widths.iter().enumerate().take(columns) {
                if column > 0 {
                    spans.push(Span::styled(" │ ", Style::default().fg(theme.border)));
                }
                let cell = row.get(column).map(String::as_str).unwrap_or("");
                let style = if index == 0 {
                    Style::default()
                        .fg(theme.accent)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(theme.text)
                };
                spans.push(Span::styled(padded_cell(cell, *cell_width), style));
            }
            lines.push(Line::from(spans));
            if index == 0 {
                lines.push(Line::from(Span::styled(
                    format!(
                        "  {}",
                        "─".repeat(widths.iter().sum::<usize>() + 3 * columns.saturating_sub(1))
                    ),
                    Style::default().fg(theme.border),
                )));
            }
        }
    } else {
        for row in table.rows {
            let title = row.first().map(String::as_str).unwrap_or("");
            lines.push(Line::from(vec![
                Span::styled("  • ", Style::default().fg(theme.accent)),
                Span::styled(
                    title.to_owned(),
                    Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
                ),
            ]));
            for (index, cell) in row.iter().enumerate().skip(1) {
                let key = table
                    .header
                    .get(index)
                    .map(String::as_str)
                    .unwrap_or("Value");
                lines.push(Line::from(vec![
                    Span::styled(format!("    {key}: "), Style::default().fg(theme.muted)),
                    Span::styled(cell.to_owned(), Style::default().fg(theme.text)),
                ]));
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct LinkSpan {
    pub row: usize,
    pub span: usize,
    pub target: String,
}

#[cfg(test)]
pub fn render_with_width(source: &str, theme: Theme, width: usize) -> Vec<Line<'static>> {
    render_with_links(source, theme, width).0
}

/// Keep the link's real destination alongside its rendered label. A terminal
/// hyperlink may display `审查报告` instead of an actual URL; linkify-by-text
/// alone cannot open it, especially after line wrapping.
pub fn render_with_links(
    source: &str,
    theme: Theme,
    width: usize,
) -> (Vec<Line<'static>>, Vec<LinkSpan>) {
    let mut lines = Vec::new();
    let mut link_spans = Vec::new();
    let mut current_link: Option<String> = None;
    let mut spans = Vec::new();
    let mut styles = vec![Style::default().fg(theme.text)];
    let mut list_depth = 0usize;
    let mut in_code = false;
    let mut code_language: Option<String> = None;
    let mut last_blank = false;
    let mut table: Option<TableContent> = None;

    let emit_line =
        |lines: &mut Vec<Line<'static>>, spans: &mut Vec<Span<'static>>, last_blank: &mut bool| {
            let blank = spans.is_empty();
            if !blank || !*last_blank {
                lines.push(Line::from(std::mem::take(spans)));
            }
            *last_blank = blank;
        };
    for event in Parser::new_ext(
        source,
        Options::ENABLE_TABLES | Options::ENABLE_TASKLISTS | Options::ENABLE_STRIKETHROUGH,
    ) {
        if let Some(table_content) = table.as_mut() {
            match event {
                Event::Start(Tag::TableCell) => table_content.cell.clear(),
                Event::End(TagEnd::TableCell) => table_content
                    .current
                    .push(table_content.cell.trim().to_owned()),
                Event::End(TagEnd::TableHead) => {
                    table_content.header = std::mem::take(&mut table_content.current)
                }
                Event::End(TagEnd::TableRow) => table_content
                    .rows
                    .push(std::mem::take(&mut table_content.current)),
                Event::Text(value) | Event::Code(value) => table_content.cell.push_str(&value),
                Event::SoftBreak | Event::HardBreak => table_content.cell.push(' '),
                Event::End(TagEnd::Table) => {
                    render_table(&mut lines, table.take().unwrap(), theme, width);
                    blank_after(&mut lines);
                }
                _ => {}
            }
            continue;
        }
        match event {
            Event::Start(tag) => match tag {
                Tag::Table(_) => {
                    if !spans.is_empty() {
                        emit_line(&mut lines, &mut spans, &mut last_blank);
                    }
                    blank_after(&mut lines);
                    table = Some(TableContent::default());
                }
                Tag::Heading { level, .. } => {
                    if !spans.is_empty() {
                        emit_line(&mut lines, &mut spans, &mut last_blank);
                    }
                    blank_after(&mut lines);
                    let marker = match level {
                        pulldown_cmark::HeadingLevel::H1 => "◆ ",
                        pulldown_cmark::HeadingLevel::H2 => "▸ ",
                        _ => "• ",
                    };
                    spans.push(Span::styled(marker, Style::default().fg(theme.accent)));
                    styles.push(
                        Style::default()
                            .fg(theme.accent)
                            .add_modifier(Modifier::BOLD),
                    );
                }
                Tag::Strong => styles.push(styles.last().unwrap().add_modifier(Modifier::BOLD)),
                Tag::Emphasis => styles.push(styles.last().unwrap().add_modifier(Modifier::ITALIC)),
                Tag::Strikethrough => {
                    styles.push(styles.last().unwrap().add_modifier(Modifier::CROSSED_OUT))
                }
                Tag::Link { dest_url, .. } => {
                    current_link = Some(dest_url.into_string());
                    styles.push(
                        Style::default()
                            .fg(theme.accent)
                            .add_modifier(Modifier::UNDERLINED),
                    );
                }
                Tag::CodeBlock(kind) => {
                    if !spans.is_empty() {
                        emit_line(&mut lines, &mut spans, &mut last_blank);
                    }
                    blank_after(&mut lines);
                    in_code = true;
                    code_language = match kind {
                        CodeBlockKind::Fenced(info) => info
                            .split_whitespace()
                            .next()
                            .filter(|s| !s.is_empty())
                            .map(str::to_ascii_lowercase),
                        CodeBlockKind::Indented => None,
                    };
                    if let Some(language) = code_language.as_deref() {
                        lines.push(Line::from(Span::styled(
                            format!("  {language}"),
                            Style::default().fg(theme.muted),
                        )));
                    }
                    styles.push(Style::default().fg(theme.code));
                }
                Tag::List(_) => {
                    if list_depth == 0 && !spans.is_empty() {
                        emit_line(&mut lines, &mut spans, &mut last_blank);
                    }
                    list_depth += 1;
                }
                Tag::Item => {
                    if !spans.is_empty() {
                        emit_line(&mut lines, &mut spans, &mut last_blank);
                    }
                    let indent = "  ".repeat(list_depth.saturating_sub(1).min(8));
                    let marker = match list_depth {
                        1 => "•",
                        2 => "◦",
                        _ => "▪",
                    };
                    spans.push(Span::styled(
                        format!("{indent}{marker} "),
                        Style::default().fg(theme.accent),
                    ));
                }
                Tag::BlockQuote(_) => {
                    if !spans.is_empty() {
                        emit_line(&mut lines, &mut spans, &mut last_blank);
                    }
                    blank_after(&mut lines);
                    spans.push(Span::styled("│ ", Style::default().fg(theme.muted)));
                    styles.push(
                        Style::default()
                            .fg(theme.muted)
                            .add_modifier(Modifier::ITALIC),
                    );
                }
                Tag::TableCell if !spans.is_empty() => {
                    spans.push(Span::styled(" │ ", Style::default().fg(theme.border)));
                }
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Heading(_) | TagEnd::BlockQuote(_) => {
                    styles.pop();
                    emit_line(&mut lines, &mut spans, &mut last_blank);
                    blank_after(&mut lines);
                }
                TagEnd::CodeBlock => {
                    in_code = false;
                    code_language = None;
                    styles.pop();
                    if !spans.is_empty() {
                        emit_line(&mut lines, &mut spans, &mut last_blank);
                    }
                    blank_after(&mut lines);
                }
                TagEnd::Strong | TagEnd::Emphasis | TagEnd::Strikethrough => {
                    styles.pop();
                }
                TagEnd::Link => {
                    current_link = None;
                    styles.pop();
                }
                TagEnd::Paragraph | TagEnd::Item => {
                    if !spans.is_empty() {
                        emit_line(&mut lines, &mut spans, &mut last_blank);
                    }
                    if matches!(tag, TagEnd::Paragraph) && list_depth == 0 {
                        blank_after(&mut lines);
                    }
                }
                TagEnd::List(_) => {
                    list_depth = list_depth.saturating_sub(1);
                    if list_depth == 0 {
                        blank_after(&mut lines);
                    }
                }
                _ => {}
            },
            Event::Text(value) | Event::Html(value) | Event::InlineHtml(value) => {
                let style = *styles.last().unwrap();
                for (i, chunk) in value.split('\n').enumerate() {
                    if i > 0 {
                        emit_line(&mut lines, &mut spans, &mut last_blank);
                    }
                    if !chunk.is_empty() {
                        if in_code && spans.is_empty() {
                            spans.push(Span::styled("│ ", Style::default().fg(theme.border)));
                        }
                        let code_style = if in_code && code_language.as_deref() == Some("diff") {
                            match chunk.chars().next() {
                                Some('+') => Style::default().fg(theme.user),
                                Some('-') => Style::default().fg(theme.error),
                                Some('@') => Style::default().fg(theme.accent),
                                _ => style,
                            }
                        } else if in_code
                            && (chunk.trim_start().starts_with("//")
                                || chunk.trim_start().starts_with('#'))
                        {
                            Style::default().fg(theme.muted)
                        } else {
                            style
                        };
                        if let Some(target) = &current_link {
                            link_spans.push(LinkSpan {
                                row: lines.len(),
                                span: spans.len(),
                                target: target.clone(),
                            });
                        }
                        spans.push(Span::styled(chunk.to_owned(), code_style));
                        last_blank = false;
                    }
                }
            }
            Event::Code(value) => {
                if let Some(target) = &current_link {
                    link_spans.push(LinkSpan {
                        row: lines.len(),
                        span: spans.len(),
                        target: target.clone(),
                    });
                }
                let style = if current_link.is_some() {
                    Style::default()
                        .fg(theme.accent)
                        .add_modifier(Modifier::UNDERLINED)
                } else {
                    Style::default().fg(theme.code)
                };
                spans.push(Span::styled(value.into_string(), style));
            }
            Event::SoftBreak => spans.push(Span::raw(" ")),
            Event::HardBreak => emit_line(&mut lines, &mut spans, &mut last_blank),
            Event::Rule => {
                emit_line(&mut lines, &mut spans, &mut last_blank);
                lines.push(Line::from(Span::styled(
                    "────────────────",
                    Style::default().fg(theme.border),
                )));
            }
            Event::TaskListMarker(checked) => spans.push(Span::styled(
                if checked { "[x] " } else { "[ ] " },
                Style::default().fg(theme.accent),
            )),
            _ => {}
        }
    }
    if !spans.is_empty() {
        lines.push(Line::from(spans));
    }
    while lines.len() > 1 && lines.last().is_some_and(|line| line.spans.is_empty()) {
        lines.pop();
    }
    if lines.is_empty() {
        lines.push(Line::default());
    }
    (lines, link_spans)
}

#[cfg(test)]
mod tests {
    use super::super::theme::{ThemeSettings, ThemeStore};
    use super::*;

    #[test]
    fn headings_paragraphs_nested_lists_and_tables_have_distinct_rows() {
        let theme = ThemeStore::load_at(None).unwrap().colors;
        let source = "## 规模与结构\n\n第一段说明。\n\n第二段说明。\n\n- 父项\n  - 子项\n\n| 维度 | 分数 |\n|---|---|\n| 架构设计 | 9.0 |\n| 测试质量 | 8.5 |\n\n结论。";
        let rows = render_with_width(source, theme, 56);
        let text = rows
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();
        let heading = text
            .iter()
            .position(|line| line.contains("规模与结构"))
            .unwrap();
        assert!(
            text[heading + 1].is_empty(),
            "heading should have breathing room"
        );
        let first = text
            .iter()
            .position(|line| line.contains("第一段说明"))
            .unwrap();
        let second = text
            .iter()
            .position(|line| line.contains("第二段说明"))
            .unwrap();
        assert!(
            text[first + 1..second].iter().any(String::is_empty),
            "paragraphs should not run together"
        );
        assert!(text.iter().any(|line| line.contains("• 父项")));
        assert!(text.iter().any(|line| line.contains("  ◦ 子项")));
        let table = text
            .iter()
            .position(|line| line.contains("维度") && line.contains("分数"))
            .unwrap();
        assert!(
            text[table + 1].contains('─'),
            "a table needs a visible header separator"
        );
        assert!(text[table + 2].contains("架构设计") && text[table + 2].contains("9.0"));
        assert!(text[table + 3].contains("测试质量") && text[table + 3].contains("8.5"));
        assert!(text.iter().any(|line| line.contains("结论")));
    }

    #[test]
    fn named_markdown_links_keep_their_targets_when_display_text_differs() {
        let theme = ThemeStore::load_at(None).unwrap().colors;
        let (rows, tags) = render_with_links(
            "Read [审查报告 SR-001](https://example.org/reviews/SR-001) and [复现测试](./reviews/tests.md).",
            theme, 100,
        );
        assert_eq!(tags.len(), 2);
        assert_eq!(tags[0].target, "https://example.org/reviews/SR-001");
        assert_eq!(tags[1].target, "./reviews/tests.md");
        assert_eq!(
            rows[tags[0].row].spans[tags[0].span].content,
            "审查报告 SR-001"
        );
        assert!(rows[tags[0].row].spans[tags[0].span]
            .style
            .add_modifier
            .contains(Modifier::UNDERLINED));
    }

    #[test]
    fn fenced_code_and_diff_have_language_labels_and_semantic_rows() {
        let theme = ThemeStore::load_at(None).unwrap().colors;
        let markdown = "Before\n\n```rust\nfn greet() {}\n// comment\n```\n\n```diff\n@@ -1,1 +1,1 @@\n- old\n+ new\n unchanged\n```";
        let rendered = render_with_width(markdown, theme, 80);
        let lines = rendered
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();
        assert!(lines.iter().any(|line| line.trim() == "rust"));
        assert!(lines.iter().any(|line| line.trim() == "diff"));
        assert!(lines.iter().any(|line| line.contains("│ fn greet()")));
        assert!(lines.iter().any(|line| line.contains("│ + new")));
        let added = rendered
            .iter()
            .flat_map(|line| line.spans.iter())
            .find(|span| span.content.contains("+ new"))
            .unwrap();
        let removed = rendered
            .iter()
            .flat_map(|line| line.spans.iter())
            .find(|span| span.content.contains("- old"))
            .unwrap();
        assert_eq!(added.style.fg, Some(theme.user));
        assert_eq!(removed.style.fg, Some(theme.error));
    }

    #[test]
    fn narrow_tables_fall_back_to_labeled_rows() {
        let theme = ThemeStore::load_at(None).unwrap().colors;
        let rows = render_with_width(
            "| 维度 | 分数 |\n|---|---|\n| 非常长的工程化与仓库卫生 | 6.5 |",
            theme,
            20,
        );
        let text = rows
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();
        assert!(text
            .iter()
            .any(|line| line.contains("非常长的工程化与仓库卫生")));
        assert!(text.iter().any(|line| line.contains("分数: 6.5")));
    }
    #[test]
    fn handles_heading_lists_code_and_bold_without_literal_markup() {
        let _ = ThemeSettings::default();
        let theme = Theme {
            background: ratatui::style::Color::Reset,
            text: ratatui::style::Color::White,
            muted: ratatui::style::Color::Gray,
            accent: ratatui::style::Color::Cyan,
            border: ratatui::style::Color::Gray,
            user: ratatui::style::Color::Green,
            tool: ratatui::style::Color::Yellow,
            code: ratatui::style::Color::Blue,
            error: ratatui::style::Color::Red,
        };
        let lines = render_with_width(
            "# 标题\n\n- **你好**\n- `code`\n\n```rust\nlet a=1;\n```",
            theme,
            80,
        );
        let flat = lines
            .iter()
            .flat_map(|line| line.spans.iter().map(|span| span.content.as_ref()))
            .collect::<String>();
        assert!(flat.contains("标题") && flat.contains("你好") && flat.contains("let a=1"));
        assert!(!flat.contains("**你好**"));
    }

    #[test]
    fn code_gutter_ends_at_fence() {
        let theme = Theme {
            background: ratatui::style::Color::Reset,
            text: ratatui::style::Color::White,
            muted: ratatui::style::Color::Gray,
            accent: ratatui::style::Color::Cyan,
            border: ratatui::style::Color::Gray,
            user: ratatui::style::Color::Green,
            tool: ratatui::style::Color::Yellow,
            code: ratatui::style::Color::Blue,
            error: ratatui::style::Color::Red,
        };
        let lines = render_with_width("Before\n\n```sh\necho ok\n```\n\nAfter", theme, 80);
        let last = lines
            .last()
            .unwrap()
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert_eq!(last, "After");
    }
}
