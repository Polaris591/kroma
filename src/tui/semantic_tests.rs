//! S2 colours existing state without changing execution, content or editor geometry.
use super::*;
use ratatui::{backend::TestBackend, buffer::Buffer, Terminal};

fn ui() -> UiState {
    let mut ui = UiState::new_default(
        "model-x".into(),
        "semantic-test".into(),
        ThemeStore::load_at(None).unwrap(),
        &[],
    );
    ui.configure_status(500_000, Some("high".into()));
    ui
}
fn draw(ui: &mut UiState, width: u16, height: u16) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| ui.draw(frame)).unwrap();
    terminal.backend().buffer().clone()
}
fn neutral(color: Color) -> bool {
    matches!(color, Color::Reset) || matches!(color, Color::Rgb(r,g,b) if r==g && g==b)
}
fn assert_neutral_backgrounds_except_message_rails(ui: &UiState, buffer: &Buffer) {
    let messages = ui.message_colors();
    for (index, cell) in buffer.content().iter().enumerate() {
        if neutral(cell.bg) {
            continue;
        }
        let x = buffer.area.x + (index % usize::from(buffer.area.width)) as u16;
        let y = buffer.area.y + (index / usize::from(buffer.area.width)) as u16;
        assert_eq!(x, ui.selection_area.x + 1);
        assert!((ui.selection_area.y..ui.selection_area.bottom()).contains(&y));
        assert_eq!(cell.symbol(), " ");
        assert!(cell.bg == messages.user_rail || cell.bg == messages.assistant_rail);
    }
}
fn finish_tool(ui: &mut UiState, id: &str, command: &str, result: &str, failed: bool) {
    ui.event(UiEvent::ToolStarted {
        name: "shell".into(),
        id: id.into(),
        args: command.into(),
    });
    ui.event(UiEvent::ToolFinished {
        id: id.into(),
        result: result.into(),
        failed,
    });
}
fn line_text(line: &Line<'_>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect()
}
fn luminance(color: Color) -> f64 {
    let Color::Rgb(r, g, b) = color else {
        panic!("RGB expected")
    };
    let channel = |c: u8| {
        let s = f64::from(c) / 255.0;
        if s <= 0.04045 {
            s / 12.92
        } else {
            ((s + 0.055) / 1.055).powf(2.4)
        }
    };
    channel(r) * 0.2126 + channel(g) * 0.7152 + channel(b) * 0.0722
}

#[test]
fn semantic_palette_matches_design_and_keeps_neutral_theme_separate() {
    for (preset, expected) in [
        (
            "dark",
            [
                Color::Rgb(122, 162, 247),
                Color::Rgb(134, 217, 154),
                Color::Rgb(229, 181, 103),
                Color::Rgb(240, 120, 134),
            ],
        ),
        (
            "light",
            [
                Color::Rgb(36, 91, 180),
                Color::Rgb(44, 107, 60),
                Color::Rgb(128, 85, 14),
                Color::Rgb(170, 53, 71),
            ],
        ),
    ] {
        let mut ui = ui();
        ui.themes.select(preset).unwrap();
        let p = ui.semantic_colors();
        let base = ui.active_colors();
        assert_eq!([p.running, p.success, p.warning, p.error], expected);
        assert_eq!(p.link, p.running);
        assert_eq!(p.add, p.success);
        assert_eq!(p.remove, p.error);
        for color in [
            base.text,
            base.muted,
            base.faint,
            base.accent,
            base.accent_text,
            base.user,
            base.code,
            base.background,
            base.surface,
            base.overlay,
        ] {
            assert!(neutral(color));
        }
        for foreground in [p.running, p.success, p.warning, p.error] {
            for background in [base.background, base.surface, base.overlay] {
                let (a, b) = (luminance(foreground), luminance(background));
                assert!(
                    (a.max(b) + 0.05) / (a.min(b) + 0.05) >= 4.5,
                    "{preset}: {foreground:?} on {background:?}"
                );
            }
        }
    }
}

#[test]
fn semantic_tool_prefix_colours_state_but_never_the_command() {
    let unknown="Session interrupted. Tool result is unknown; do not assume the command succeeded or automatically repeat it.";
    for (id, result, failed, kind, marker, label) in [
        ("ok", "exit: 0\nverified", false, "success", "✓", ""),
        (
            "warn",
            "exit: 0\nstderr: warning: check configuration",
            false,
            "warning",
            "!",
            "Warning",
        ),
        (
            "bad",
            "exit: 101\nstderr: assertion failed",
            true,
            "error",
            "✕",
            "Failed",
        ),
        ("unknown", unknown, true, "warning", "⊘", "Unknown"),
    ] {
        let mut ui = ui();
        finish_tool(&mut ui, id, "echo real-command", result, failed);
        let entry = ui
            .entries
            .iter()
            .find(|e| e.call_id.as_deref() == Some(id))
            .unwrap();
        let card = ui.tool_card(&[entry], ui.active_colors(), 100);
        let p = ui.semantic_colors();
        let expected = match kind {
            "success" => p.success,
            "warning" => p.warning,
            _ => p.error,
        };
        let line = &card.lines[0];
        let text = line_text(line);
        assert!(text.contains("$ echo real-command"));
        assert!(!text.contains("Done"));
        let state = line
            .spans
            .iter()
            .find(|span| span.content.trim_end() == marker)
            .unwrap();
        assert_eq!(state.style.fg, Some(expected));
        if !label.is_empty() {
            let word = line
                .spans
                .iter()
                .find(|span| span.content.contains(label))
                .unwrap();
            assert_eq!(word.style.fg, Some(expected));
        }
        assert!(line
            .spans
            .iter()
            .any(|span| span.content.contains("$ echo real-command")
                && span.style.fg.is_some_and(neutral)));
        for line in card.lines.iter().skip(1) {
            for span in &line.spans {
                assert!(span.style.fg.is_none_or(neutral));
            }
        }
    }
    let mut ui = ui();
    ui.event(UiEvent::ToolStarted {
        name: "shell".into(),
        id: "active".into(),
        args: "cargo test".into(),
    });
    let entry = ui.entries.last().unwrap();
    let card = ui.tool_card(&[entry], ui.active_colors(), 100);
    let text = line_text(&card.lines[0]);
    assert!(text.contains("$ cargo test") && text.contains("Running"));
    let state = card.lines[0]
        .spans
        .iter()
        .find(|span| span.content.contains("Running"))
        .unwrap();
    assert_eq!(state.style.fg, Some(ui.semantic_colors().running));
}

#[test]
fn semantic_individual_records_and_chevrons_survive_compact_and_expanded_views() {
    let mut ui = ui();
    finish_tool(&mut ui, "one", "echo one", "exit: 0\nok", false);
    finish_tool(&mut ui, "two", "echo two", "exit: 0\nok", false);
    let entries = ui
        .entries
        .iter()
        .filter(|e| matches!(e.kind, Kind::Tool))
        .collect::<Vec<_>>();
    for width in [40, 80, 120] {
        let card = ui.tool_card(&entries, ui.active_colors(), width);
        assert_eq!(card.rows.len(), 2);
        assert!(card.blocks.is_empty());
        assert!(card.groups.is_empty());
        assert!(!line_text(&card.lines[0]).contains("2 steps"));
        assert!(card.lines[0].width() <= width);
        assert!(!line_text(&card.lines[0]).contains("Done"));
        assert!(card.lines[0].spans.iter().any(
            |span| span.content == "✓ " && span.style.fg == Some(ui.semantic_colors().success)
        ));
        assert!(!line_text(&card.lines[0]).contains('▸'));
    }
    ui.toggle_all_tools();
    let entries = ui
        .entries
        .iter()
        .filter(|e| matches!(e.kind, Kind::Tool))
        .collect::<Vec<_>>();
    let card = ui.tool_card(&entries, ui.active_colors(), 100);
    assert!(!line_text(&card.lines[0]).contains('▾'));
    for (_, id) in &card.rows {
        assert!(id == "one" || id == "two");
    }
    assert!(card
        .lines
        .iter()
        .any(|line| line_text(line).contains("echo one")));
}

#[test]
fn semantic_activity_colours_only_state_and_keeps_cancellation_neutral() {
    let mut ui = ui();
    ui.running = true;
    ui.event(UiEvent::ToolStarted {
        name: "shell".into(),
        id: "active".into(),
        args: "cargo test".into(),
    });
    let active = ui.activity_line(ui.active_colors());
    assert!(active.spans[0].content.contains("Running"));
    assert_eq!(active.spans[0].style.fg, Some(ui.semantic_colors().running));
    for span in active.spans.iter().skip(1) {
        assert!(span.style.fg.is_some_and(neutral));
    }
    for outcome in [
        TaskOutcome::Completed,
        TaskOutcome::Failed,
        TaskOutcome::Cancelled,
    ] {
        ui.running = false;
        ui.task_outcome = Some(outcome);
        ui.task_elapsed = Some(Duration::from_secs(8));
        let line = ui.activity_line(ui.active_colors());
        let expected = match outcome {
            TaskOutcome::Completed => ui.semantic_colors().success,
            TaskOutcome::Failed => ui.semantic_colors().error,
            TaskOutcome::Cancelled => ui.active_colors().muted,
        };
        assert_eq!(line.spans[0].style.fg, Some(expected));
        for span in line.spans.iter().skip(1) {
            assert!(span.style.fg.is_some_and(neutral));
        }
    }
}

#[test]
fn semantic_context_thresholds_do_not_colour_cache_model_or_unknowns() {
    let mut ui = ui();
    ui.event(UiEvent::Usage {
        input: Some(100),
        output: None,
        cached: Some(0),
        reasoning: None,
    });
    for (input, role) in [
        (0, "neutral"),
        (349_999, "neutral"),
        (350_000, "warning"),
        (449_999, "warning"),
        (450_000, "error"),
    ] {
        ui.event(UiEvent::ActualInput(input));
        let line = ui.footer_line(ui.active_colors(), 120);
        let value = line
            .spans
            .iter()
            .find(|s| s.content.contains("/500k"))
            .unwrap();
        let expected = match role {
            "warning" => ui.semantic_colors().warning,
            "error" => ui.semantic_colors().error,
            _ => ui.active_colors().faint,
        };
        assert_eq!(value.style.fg, Some(expected));
        for span in &line.spans {
            if !span.content.contains("/500k") {
                assert!(span.style.fg.is_none_or(neutral));
            }
        }
        assert!(line.to_string().contains("Cache 0%"));
    }
    ui.reset_context();
    let line = ui.footer_line(ui.active_colors(), 120);
    assert!(line.to_string().contains("—/500k"));
    assert!(line.spans.iter().all(|s| s.style.fg.is_none_or(neutral)));
}

#[test]
fn semantic_link_colour_is_hover_only_and_does_not_spread_to_input() {
    let mut ui = ui();
    ui.add_user("Check link");
    ui.event(UiEvent::Text(
        "See [documentation](https://example.org/plain).".into(),
    ));
    ui.finish_task();
    let buffer = draw(&mut ui, 100, 30);
    let link = ui.visible_links[0].clone();
    assert_eq!(
        buffer.cell((link.x, link.y)).unwrap().fg,
        ui.active_colors().muted
    );
    ui.hover_pointer = Some((link.x, link.y));
    let hovered = draw(&mut ui, 100, 30);
    assert_eq!(
        hovered.cell((link.x, link.y)).unwrap().fg,
        ui.semantic_colors().link
    );
    assert!(hovered
        .cell((link.x, link.y))
        .unwrap()
        .modifier
        .contains(Modifier::UNDERLINED));
    assert!(neutral(
        hovered
            .cell((ui.editor_area.x - 2, ui.editor_area.y))
            .unwrap()
            .fg
    ));
    assert_eq!(
        ui.link_at(link.x, link.y),
        Some("https://example.org/plain")
    );
}

#[test]
fn semantic_diff_transform_is_lossless_and_colours_markers_not_code() {
    let ui = ui();
    let base = ui.active_colors();
    let source="```diff\ndiff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1 +1 @@\n-\t旧值🙂\n+\t新值 e\u{301}\n\n```\n\n```rust\nlet value = \"+not a diff\";\n```";
    let mut document = markdown::render_document(source, base, 100);
    let before = document.lines.iter().map(line_text).collect::<Vec<_>>();
    let raw = document
        .blocks
        .iter()
        .map(|b| b.raw.clone())
        .collect::<Vec<_>>();
    ui.color_plain_diff_blocks(&mut document, base);
    assert_eq!(
        document.lines.iter().map(line_text).collect::<Vec<_>>(),
        before
    );
    assert_eq!(
        document
            .blocks
            .iter()
            .map(|b| b.raw.clone())
            .collect::<Vec<_>>(),
        raw
    );
    let mut markers = 0;
    for line in &document.lines {
        for span in &line.spans {
            if span.style.fg.is_some_and(|c| !neutral(c)) {
                assert!(span.content == "+" || span.content == "-");
                assert!(span.style.bg.is_none());
                markers += 1;
            }
        }
    }
    assert_eq!(markers, 2);
}

#[test]
fn semantic_tool_diffs_do_not_recolour_arbitrary_plus_minus_logs() {
    let mut ui = ui();
    finish_tool(
        &mut ui,
        "log",
        "echo progress",
        "exit: 0\n+ ordinary progress\n- ordinary progress",
        false,
    );
    finish_tool(
        &mut ui,
        "diff",
        "git diff",
        "exit: 0\ndiff --git a/a b/a\n--- a/a\n+++ b/a\n@@ -1 +1 @@\n-old\n+new",
        false,
    );
    ui.toggle_all_tools();
    for id in ["log", "diff"] {
        let entry = ui
            .entries
            .iter()
            .find(|e| e.call_id.as_deref() == Some(id))
            .unwrap();
        let card = ui.tool_card(&[entry], ui.active_colors(), 100);
        let lines = card.lines.iter().filter(|line| {
            line_text(line).contains("ordinary progress")
                || line_text(line).contains("-old")
                || line_text(line).contains("+new")
        });
        for line in lines {
            for span in &line.spans {
                if id == "log" {
                    assert!(span.style.fg.is_none_or(neutral));
                } else if span.style.fg.is_some_and(|c| !neutral(c)) {
                    assert!(span.content == "+" || span.content == "-");
                }
            }
        }
    }
}

#[test]
fn semantic_diff_command_output_is_typed_and_plain_info_stays_neutral() {
    let mut ui = ui();
    ui.info("DIFF", "@@ -1 +1 @@\n-old\n+new");
    let document = ui.history_text(ui.active_colors(), 100);
    assert!(document
        .lines
        .iter()
        .flat_map(|line| &line.spans)
        .any(|s| s.content == "+" && s.style.fg == Some(ui.semantic_colors().add)));
    let mut ordinary = self::ui();
    ordinary.info("COMMAND", "+not a diff\n-not a diff");
    for line in ordinary.history_text(ordinary.active_colors(), 100).lines {
        for span in line.spans {
            assert!(span.style.fg.is_none_or(neutral));
        }
    }
}

#[test]
fn semantic_notices_colour_only_the_prefix_and_fit_narrow_rows() {
    let mut ui = ui();
    for level in [NoticeLevel::Info, NoticeLevel::Warning, NoticeLevel::Error] {
        ui.note_with_level(level, "中文诊断 with ordinary body text\nsecond line");
        for width in [0, 8, 14, 40, 80] {
            let line = ui.plain_notification_line(ui.active_colors(), width);
            assert!(line.width() <= width);
            assert!(line.spans[1].style.fg.is_some_and(neutral));
            let expected = match level {
                NoticeLevel::Info => ui.active_colors().muted,
                NoticeLevel::Warning => ui.semantic_colors().warning,
                NoticeLevel::Error => ui.semantic_colors().error,
            };
            assert_eq!(line.spans[0].style.fg, Some(expected));
        }
    }
}

#[test]
fn semantic_mono_respects_native_foreground_and_background() {
    let mut ui = ui();
    ui.themes.select("mono").unwrap();
    ui.add_user("native terminal");
    ui.event(UiEvent::Text(
        "```diff\n-old\n+new\n```\n\nhttps://example.org/".into(),
    ));
    finish_tool(&mut ui, "failure", "cargo test", "exit: 1\nfailed", true);
    ui.event(UiEvent::ActualInput(475_000));
    let buffer = draw(&mut ui, 100, 35);
    for cell in buffer.content() {
        assert_eq!(cell.fg, Color::Reset);
        assert_eq!(cell.bg, Color::Reset);
    }
    assert_eq!(ui.semantic_colors().running, Color::Reset);
}

#[test]
fn semantic_error_info_colours_the_heading_not_diagnostic_body() {
    let mut ui = ui();
    ui.info("ERROR", "ordinary diagnostic text\n+not a diff");
    let document = ui.history_text(ui.active_colors(), 100);
    assert!(document.lines[0].spans[0].content.contains("ERROR"));
    assert_eq!(
        document.lines[0].spans[0].style.fg,
        Some(ui.semantic_colors().error)
    );
    for line in document.lines.iter().skip(1) {
        for span in &line.spans {
            assert!(span.style.fg.is_none_or(neutral));
        }
    }
}

#[test]
fn semantic_long_statuses_and_risk_readings_survive_small_terminals() {
    for (width, height) in [(20, 8), (40, 16), (80, 24), (160, 42)] {
        let mut ui = ui();
        ui.add_user("Check a long task");
        finish_tool(
            &mut ui,
            "warning",
            "echo warning",
            "exit: 0\nstderr: warning: unknown result",
            false,
        );
        ui.event(UiEvent::ToolStarted {
            name: "shell".into(),
            id: "active".into(),
            args: "cargo test --workspace --all-targets -- --nocapture".into(),
        });
        ui.event(UiEvent::ActualInput(475_000));
        ui.editor.paste(&"中文 draft line\n".repeat(12));
        let buffer = draw(&mut ui, width, height);
        assert!(ui.editor_area.right() <= width && ui.editor_area.bottom() <= height);
        assert_neutral_backgrounds_except_message_rails(&ui, &buffer);
    }
}

#[test]
fn semantic_render_keeps_non_rail_backgrounds_neutral_and_editor_geometry_stable() {
    for preset in ["dark", "light"] {
        let mut ui = ui();
        ui.themes.select(preset).unwrap();
        ui.add_user("Check tools");
        finish_tool(&mut ui, "good", "echo good", "exit: 0\nok", false);
        finish_tool(&mut ui, "bad", "cargo test", "exit: 1\nfailed", true);
        ui.editor.paste("中文🙂 unfinished draft");
        let buffer = draw(&mut ui, 80, 24);
        let area = ui.editor_area;
        assert!(area.bottom() < 24);
        assert_neutral_backgrounds_except_message_rails(&ui, &buffer);
        for y in area.y..area.bottom() {
            for x in area.x..area.right() {
                assert!(neutral(buffer.cell((x, y)).unwrap().fg));
            }
        }
        ui.toggle_all_tools();
        draw(&mut ui, 80, 24);
        assert_eq!(ui.editor_area, area);
    }
}
