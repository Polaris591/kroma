//! Message identity stays aligned through wrapping, streaming and selection.
use super::*;
use messages::MessageRole;
use ratatui::{backend::TestBackend, buffer::Buffer, Terminal};

fn ui() -> UiState {
    UiState::new_default(
        "model-x".into(),
        "message-test".into(),
        ThemeStore::load_at(None).unwrap(),
        &[],
    )
}
fn draw(ui: &mut UiState, width: u16, height: u16) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| ui.draw(frame)).unwrap();
    terminal.backend().buffer().clone()
}
fn rails(ui: &UiState, role: MessageRole) -> Vec<usize> {
    let p = ui.message_colors();
    let expected = match role {
        MessageRole::User => p.user_rail,
        MessageRole::Assistant => p.assistant_rail,
    };
    let mut rows = ui
        .history_cache
        .as_ref()
        .unwrap()
        .decor
        .iter()
        .filter_map(|(row, item)| {
            matches!(item,Decor::Rail {column:1,color,first_only:false} if *color==expected)
                .then_some(*row)
        })
        .collect::<Vec<_>>();
    rows.sort_unstable();
    rows.dedup();
    rows
}
fn visible_y(ui: &UiState, row: usize) -> Option<u16> {
    (row >= ui.history_top_row && row < ui.history_top_row + usize::from(ui.selection_area.height))
        .then(|| ui.selection_area.y + (row - ui.history_top_row) as u16)
}
fn luminance(color: Color) -> f64 {
    let Color::Rgb(r, g, b) = color else {
        panic!("RGB required")
    };
    let f = |c: u8| {
        let c = f64::from(c) / 255.0;
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * f(r) + 0.7152 * f(g) + 0.0722 * f(b)
}
fn contrast(a: Color, b: Color) -> f64 {
    let (a, b) = (luminance(a), luminance(b));
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

#[test]
fn both_speakers_use_blank_background_rails_and_the_same_text_column() {
    let mut ui = ui();
    ui.add_user("user aligned");
    ui.event(UiEvent::Text("assistant aligned".into()));
    ui.finish_task();
    let buffer = draw(&mut ui, 120, 40);
    for role in [MessageRole::User, MessageRole::Assistant] {
        for row in rails(&ui, role) {
            let y = visible_y(&ui, row).unwrap();
            assert_eq!(
                buffer.cell((ui.selection_area.x + 1, y)).unwrap().symbol(),
                " "
            );
        }
    }
    for word in ["user aligned", "assistant aligned"] {
        let row = ui
            .history_cache
            .as_ref()
            .unwrap()
            .text
            .lines
            .iter()
            .position(|line| line.to_string().contains(word))
            .unwrap();
        let y = visible_y(&ui, row).unwrap();
        assert_eq!(
            buffer.cell((ui.selection_area.x + 4, y)).unwrap().symbol(),
            &word[..1]
        );
    }
}

#[test]
fn surface_has_one_padding_row_each_side_and_a_continuous_rail() {
    let mut ui = ui();
    ui.add_user("hello user");
    ui.event(UiEvent::Text("hello assistant".into()));
    ui.finish_task();
    let buffer = draw(&mut ui, 120, 40);
    let rows = rails(&ui, MessageRole::User);
    assert_eq!(rows.len(), 4);
    assert!(rows.windows(2).all(|p| p[1] == p[0] + 1));
    let cache = ui.history_cache.as_ref().unwrap();
    assert!(cache.text.lines[rows[0]].spans.is_empty());
    assert!(cache.text.lines[*rows.last().unwrap()].spans.is_empty());
    for &row in &rows {
        let y = visible_y(&ui, row).unwrap();
        let area = ui.message_surface_area(ui.selection_area, y);
        for x in area.x + 1..area.right() {
            assert_eq!(buffer.cell((x, y)).unwrap().bg, ui.message_colors().surface);
        }
        assert_eq!(
            buffer.cell((area.x, y)).unwrap().bg,
            ui.message_colors().user_rail
        );
    }
    // One terminal-background row separates a user turn from the reply.
    let gap = rows.last().unwrap() + 1;
    let y = visible_y(&ui, gap).unwrap();
    assert_eq!(
        buffer.cell((ui.selection_area.x + 1, y)).unwrap().bg,
        ui.active_colors().background
    );
    assert_eq!(rails(&ui, MessageRole::Assistant)[0], gap + 1);
}

#[test]
fn assistant_rail_covers_paragraphs_code_and_all_soft_wraps() {
    let mut ui = ui();
    ui.add_user("Review");
    ui.event(UiEvent::Text(format!("{}\n\n- first item\n- second item\n\n```rust\nlet value = 42;\n\nprintln!(\"中文🙂\");\n```","Long reply 中文🙂 ".repeat(18))));
    ui.finish_task();
    let buffer = draw(&mut ui, 40, 65);
    let rows = rails(&ui, MessageRole::Assistant);
    assert!(rows.len() > 10);
    assert!(rows.windows(2).all(|p| p[1] == p[0] + 1));
    for row in rows {
        if let Some(y) = visible_y(&ui, row) {
            let cell = buffer.cell((ui.selection_area.x + 1, y)).unwrap();
            assert_eq!(cell.symbol(), " ");
            assert_eq!(cell.bg, ui.message_colors().assistant_rail);
        }
    }
    assert_ne!(
        ui.message_colors().assistant_rail,
        ui.message_colors().user_rail
    );
    assert_ne!(
        ui.message_colors().assistant_rail,
        ui.semantic_colors().success
    );
}

#[test]
fn classic_b_uses_a_user_card_but_keeps_assistant_prose_on_terminal_background() {
    let mut ui = ui();
    ui.add_user("user card");
    ui.event(UiEvent::Text("assistant prose".into()));
    ui.finish_task();
    let buffer = draw(&mut ui, 100, 40);
    let cache = ui.history_cache.as_ref().unwrap();
    let prose_row = cache
        .text
        .lines
        .iter()
        .position(|line| line.to_string().contains("assistant prose"))
        .unwrap();
    let y = visible_y(&ui, prose_row).unwrap();
    assert_eq!(
        buffer.cell((ui.selection_area.x + 5, y)).unwrap().bg,
        ui.active_colors().background,
        "assistant prose is not another giant gray card"
    );
    assert!(cache
        .decor
        .iter()
        .any(|(row, item)| *row == prose_row && matches!(item, Decor::Rail { .. })));
    assert!(!cache
        .decor
        .iter()
        .any(|(row, item)| *row == prose_row && matches!(item, Decor::Surface(Surface::Message))));
    let user = rails(&ui, MessageRole::User);
    let user_y = visible_y(&ui, user[1]).unwrap();
    assert_eq!(
        buffer.cell((ui.selection_area.x + 5, user_y)).unwrap().bg,
        ui.message_colors().surface
    );

    ui.event(UiEvent::ToolStarted {
        name: "shell".into(),
        id: "test-tool".into(),
        args: "cargo test --lib".into(),
    });
    ui.event(UiEvent::ToolFinished {
        id: "test-tool".into(),
        result: "exit: 0\ntest result: ok. 2 passed; 0 failed".into(),
        failed: false,
    });
    let buffer = draw(&mut ui, 100, 40);
    let row = ui.history_cache.as_ref().unwrap().tool_rows[0].0;
    let y = visible_y(&ui, row).unwrap();
    assert_eq!(
        buffer.cell((ui.selection_area.x + 5, y)).unwrap().bg,
        ui.active_colors().surface,
        "tool calls have a separate, restrained gray surface"
    );
}

#[test]
fn user_background_and_rails_follow_unicode_wraps_after_resize() {
    let mut ui = ui();
    let original = "中文🙂 e\u{301} text ".repeat(25).trim_end().to_owned();
    ui.add_user(&original);
    ui.event(UiEvent::Text("Reply".into()));
    ui.finish_task();
    draw(&mut ui, 120, 50);
    let wide = rails(&ui, MessageRole::User).len();
    let buffer = draw(&mut ui, 40, 65);
    let rows = rails(&ui, MessageRole::User);
    assert!(rows.len() > wide);
    for row in rows {
        let y = visible_y(&ui, row).unwrap();
        let area = ui.message_surface_area(ui.selection_area, y);
        assert_eq!(
            buffer.cell((area.x + 1, y)).unwrap().bg,
            ui.message_colors().surface
        );
        assert_eq!(
            buffer.cell((area.right(), y)).unwrap().bg,
            ui.active_colors().background
        );
    }
    assert_eq!(ui.entries[0].body, original);
}

#[test]
fn role_decoration_does_not_pollute_copy_links_or_code_source() {
    let mut ui = ui();
    ui.add_user("hello 中文🙂");
    let code = "\tlet value = 42;\n\n";
    ui.event(UiEvent::Text(format!(
        "See [docs](https://example.org/messages).\n\n```rust\n{code}```"
    )));
    ui.finish_task();
    draw(&mut ui, 100, 45);
    assert!(ui
        .visible_links
        .iter()
        .any(|l| l.target == "https://example.org/messages"));
    let cache = ui.history_cache.as_ref().unwrap();
    assert!(cache
        .decor
        .iter()
        .any(|(_, d)| matches!(d,Decor::CodeLabel(b) if b.raw==code)));
    ui.selection_start = Some((0, 0));
    ui.selection_end = Some((cache.text.lines.len() - 1, 99));
    let copied = ui.selected_text().unwrap();
    assert!(copied.contains("hello 中文🙂") && copied.contains("let value = 42;"));
    assert!(
        !copied.contains('▎')
            && !copied.contains('│')
            && !copied.contains("YOU")
            && !copied.contains("KROMA")
    );
}

// Skip the continuation cells of wide graphemes, as a terminal copy does.
fn native_row(buffer: &Buffer, area: Rect, y: u16) -> String {
    let mut text = String::new();
    let mut x = area.x;
    while x < area.right() {
        let symbol = buffer.cell((x, y)).unwrap().symbol();
        text.push_str(symbol);
        x += UnicodeWidthStr::width(symbol).max(1) as u16;
    }
    text
}

#[test]
fn native_terminal_copy_sees_spaces_instead_of_message_rails() {
    for preset in ["dark", "light", "mono"] {
        for plain in [false, true] {
            let mut ui = ui();
            ui.plain_style = plain;
            ui.themes.select(preset).unwrap();
            ui.add_user("user 中文🙂\nsecond user line");
            ui.event(UiEvent::Text("assistant 中文🙂\n\nsecond paragraph".into()));
            ui.finish_task();
            let buffer = draw(&mut ui, 60, 40);
            // Native terminal selection reads displayed cells, not selected_text().
            let native = (ui.selection_area.y..ui.selection_area.bottom())
                .map(|y| native_row(&buffer, ui.selection_area, y))
                .collect::<Vec<_>>()
                .join("\n");
            assert!(native.contains("user 中文🙂"));
            assert!(native.contains("assistant 中文🙂"));
            assert!(!native.contains('▎'), "{preset}, plain={plain}: {native}");
            let mut count = 0;
            for (row, item) in &ui.history_cache.as_ref().unwrap().decor {
                if let Decor::Rail { column, color, .. } = item {
                    let y = visible_y(&ui, *row).unwrap();
                    let cell = buffer.cell((ui.selection_area.x + column, y)).unwrap();
                    assert_eq!(cell.symbol(), " ");
                    assert_eq!(cell.bg, *color);
                    count += 1;
                }
            }
            assert!(count > 0);
        }
    }
}

#[test]
fn copy_preserves_rail_characters_that_belong_to_the_message() {
    let mut ui = ui();
    let original = "literal ▎ in source 中文🙂";
    ui.add_user(original);
    ui.finish_task();
    let buffer = draw(&mut ui, 80, 30);
    let cache = ui.history_cache.as_ref().unwrap();
    let row = cache
        .text
        .lines
        .iter()
        .position(|line| line.to_string().contains(original))
        .unwrap();
    ui.selection_start = Some((row, 4));
    ui.selection_end = Some((row, 79));
    assert_eq!(ui.selected_text().unwrap(), original);
    let y = visible_y(&ui, row).unwrap();
    let native = native_row(&buffer, ui.selection_area, y);
    assert!(native.contains(original));
    assert_eq!(native.matches('▎').count(), 1);
}

#[test]
fn streaming_extends_rails_without_moving_pinned_history() {
    let mut ui = ui();
    ui.add_user("Long stream");
    ui.event(UiEvent::Text(
        (0..120)
            .map(|i| format!("line-{i:03} stable reply\n"))
            .collect::<Vec<_>>()
            .concat(),
    ));
    draw(&mut ui, 80, 24);
    ui.scroll(20);
    draw(&mut ui, 80, 24);
    let anchor = ui.scroll_anchor;
    let top = ui.selection_rows[0].clone();
    let count = rails(&ui, MessageRole::Assistant).len();
    ui.event(UiEvent::Text("new streamed row\n".repeat(6)));
    draw(&mut ui, 80, 24);
    assert_eq!(ui.scroll_anchor, anchor);
    assert_eq!(ui.selection_rows[0], top);
    assert!(rails(&ui, MessageRole::Assistant).len() > count);
    ui.scroll_to_bottom();
    let buffer = draw(&mut ui, 80, 24);
    for row in rails(&ui, MessageRole::Assistant) {
        if let Some(y) = visible_y(&ui, row) {
            assert_eq!(
                buffer.cell((ui.selection_area.x + 1, y)).unwrap().symbol(),
                " "
            );
        }
    }
}

#[test]
fn message_surfaces_leave_a_visible_terminal_right_gutter() {
    for width in [40, 80, 160] {
        let mut ui = ui();
        ui.add_user("Short request");
        ui.event(UiEvent::Text("Short reply".into()));
        ui.finish_task();
        let buffer = draw(&mut ui, width, 40);
        for role in [MessageRole::User, MessageRole::Assistant] {
            for row in rails(&ui, role) {
                let y = visible_y(&ui, row).unwrap();
                let area = ui.message_surface_area(ui.selection_area, y);
                assert_eq!(area.width, ui.selection_area.width - 1 - PLAIN_RIGHT_GUTTER);
                assert_eq!(area.right(), ui.selection_area.right() - PLAIN_RIGHT_GUTTER);
                let expected = match role {
                    MessageRole::User => ui.message_colors().surface,
                    MessageRole::Assistant => ui.active_colors().background,
                };
                for x in area.x + 1..area.right() {
                    assert_eq!(buffer.cell((x, y)).unwrap().bg, expected);
                }
                for x in area.right()..ui.selection_area.right() {
                    assert_eq!(
                        buffer.cell((x, y)).unwrap().bg,
                        ui.active_colors().background
                    );
                }
            }
        }
        for x in 0..width {
            assert_eq!(
                buffer.cell((x, 39)).unwrap().bg,
                ui.active_colors().background
            );
        }
        let before = ui.message_surface_area(ui.selection_area, ui.selection_area.y);
        ui.apply_preferences(super::super::preferences::Preferences {
            full_width: true,
            ..ui.preferences
        });
        draw(&mut ui, width, 40);
        assert_eq!(
            ui.message_surface_area(ui.selection_area, ui.selection_area.y),
            before
        );
    }
}

#[test]
fn message_colours_are_readable_and_native_mono_is_respected() {
    for preset in ["dark", "light"] {
        let mut ui = ui();
        ui.themes.select(preset).unwrap();
        let p = ui.message_colors();
        let b = ui.active_colors();
        assert!(contrast(b.text, p.surface) >= 4.5);
        assert!(contrast(b.faint, p.surface) >= if preset == "dark" { 2.8 } else { 4.5 });
        assert!(contrast(p.user_rail, p.surface) >= 3.0);
        assert!(contrast(p.assistant_rail, p.surface) >= 3.0);
        if preset == "dark" {
            assert!(luminance(p.surface) > luminance(b.background));
        }
    }
    let mut ui = ui();
    ui.themes.select("mono").unwrap();
    ui.add_user("Native message");
    ui.event(UiEvent::Text("Native reply".into()));
    ui.finish_task();
    for c in draw(&mut ui, 80, 30).content() {
        assert_eq!(c.fg, Color::Reset);
        assert_eq!(c.bg, Color::Reset);
    }
}

#[test]
fn small_windows_and_editor_geometry_remain_safe() {
    for (w, h) in [(20, 8), (40, 16), (80, 24)] {
        let mut ui = ui();
        ui.add_user(&"long user 中文🙂 ".repeat(20));
        ui.event(UiEvent::Text("assistant text ".repeat(30)));
        ui.editor.paste("unsent draft");
        draw(&mut ui, w, h);
        assert!(ui.editor_area.right() <= w && ui.editor_area.bottom() <= h);
        assert!(
            ui.message_surface_area(ui.selection_area, ui.selection_area.y)
                .right()
                <= ui.selection_area.right()
        );
        assert_eq!(ui.editor.text(), "unsent draft");
    }
}

#[test]
fn one_assistant_turn_keeps_one_label_and_one_rail_through_tools_and_text() {
    let mut ui = ui();
    ui.add_user("Inspect the project");
    ui.event(UiEvent::Text("I will inspect the files.".into()));
    ui.event(UiEvent::ToolStarted {
        name: "shell".into(),
        id: "tool".into(),
        args: "ls".into(),
    });
    ui.event(UiEvent::ToolFinished {
        id: "tool".into(),
        result: "exit: 0\nsrc".into(),
        failed: false,
    });
    ui.event(UiEvent::Text("The source directory is present.".into()));
    ui.finish_task();
    draw(&mut ui, 120, 50);
    let (labels, rows, tool_header, body) = {
        let cache = ui.history_cache.as_ref().unwrap();
        let labels = cache
            .decor
            .iter()
            .filter(|(_, item)| matches!(item, Decor::Role { label: "KROMA", .. }))
            .count();
        let body = cache
            .text
            .lines
            .iter()
            .position(|line| line.to_string().contains("I will inspect"))
            .unwrap();
        (
            labels,
            rails(&ui, MessageRole::Assistant),
            cache
                .tool_rows
                .iter()
                .map(|(row, _)| *row)
                .next()
                .expect("tool record row"),
            body,
        )
    };
    assert_eq!(labels, 1, "one assistant turn has one KROMA label");
    assert!(
        rows.windows(2).all(|pair| pair[1] == pair[0] + 1),
        "rail is continuous"
    );
    assert!(
        rows.contains(&tool_header),
        "tool activity stays inside the assistant rail"
    );
    let tool_row = ui.selection_area.y + (tool_header - ui.history_top_row) as u16;
    let body_cell = ui.selection_area.y + (body - ui.history_top_row) as u16;
    let buffer = draw(&mut ui, 120, 50);
    assert_eq!(
        buffer
            .cell((ui.selection_area.x + 1, tool_row))
            .unwrap()
            .symbol(),
        " "
    );
    assert_eq!(
        buffer.cell((ui.selection_area.x + 1, tool_row)).unwrap().bg,
        ui.message_colors().assistant_rail
    );
    // Tool state markers are inset inside a gray panel, prose is not.
    assert_eq!(
        buffer
            .cell((ui.selection_area.x + 4, body_cell))
            .unwrap()
            .symbol(),
        "I"
    );
    assert_eq!(
        buffer
            .cell((ui.selection_area.x + 8, tool_row))
            .unwrap()
            .symbol(),
        "✓"
    );
}

#[test]
fn identity_colours_are_distinct_from_execution_state_colours() {
    let mut ui = ui();
    let p = ui.message_colors();
    let s = ui.semantic_colors();
    assert_ne!(p.user_rail, s.running);
    assert_ne!(p.assistant_rail, s.success);
    assert_ne!(p.user_rail, p.assistant_rail);
    for preset in ["dark", "light"] {
        ui.themes.select(preset).unwrap();
        let p = ui.message_colors();
        let base = ui.active_colors();
        assert!(contrast(p.user_rail, base.surface) >= 4.5);
        assert!(contrast(p.assistant_rail, base.surface) >= 4.5);
    }
}

#[test]
fn completion_status_is_pinned_plain_text_above_the_composer() {
    let mut ui = ui();
    ui.add_user("Run a tool");
    ui.event(UiEvent::Text("Starting the check.".into()));
    ui.event(UiEvent::ToolStarted {
        name: "shell".into(),
        id: "live".into(),
        args: "cargo test".into(),
    });
    ui.running = true;
    let buffer = draw(&mut ui, 100, 40);
    let y = (0..40u16)
        .find(|&y| {
            (0..100u16)
                .map(|x| buffer.cell((x, y)).unwrap().symbol())
                .collect::<String>()
                .contains("Running")
        })
        .expect("pinned status row");
    // The status is plain text: no message surface and no rail.
    assert_eq!(
        buffer.cell((0, y)).unwrap().bg,
        ui.active_colors().background
    );
    assert_ne!(buffer.cell((0, y)).unwrap().symbol(), "▎");
    let text = (0..100u16)
        .map(|x| buffer.cell((x, y)).unwrap().symbol())
        .collect::<String>();
    assert!(
        text.contains("Running") || text.contains("执行中"),
        "{text}"
    );
}

#[test]
fn consecutive_turns_are_separated_by_a_terminal_background_row() {
    let mut ui = ui();
    ui.add_user("First question");
    ui.event(UiEvent::Text("First answer".into()));
    ui.finish_task();
    ui.add_user("Second question");
    ui.event(UiEvent::Text("Second answer".into()));
    ui.finish_task();
    draw(&mut ui, 100, 50);
    let user_rows = rails(&ui, MessageRole::User);
    let assistant_rows = rails(&ui, MessageRole::Assistant);
    let second_user = user_rows
        .windows(2)
        .find(|pair| pair[1] > pair[0] + 1)
        .map(|pair| pair[1])
        .unwrap();
    let last_assistant = *assistant_rows
        .iter()
        .filter(|row| **row < second_user)
        .max()
        .unwrap();
    let boundary = last_assistant + 1;
    assert!(boundary < second_user);
    let buffer = draw(&mut ui, 100, 50);
    let y = ui.selection_area.y + (boundary - ui.history_top_row) as u16;
    assert_eq!(
        buffer.cell((ui.selection_area.x + 1, y)).unwrap().bg,
        ui.active_colors().background
    );
    assert_eq!(
        buffer.cell((ui.selection_area.x + 1, y)).unwrap().symbol(),
        " "
    );
    let cache = ui.history_cache.as_ref().unwrap();
    assert!(cache.text.lines[boundary].spans.is_empty());
    assert!(!cache.decor.iter().any(|(row, item)| *row == boundary
        && matches!(
            item,
            Decor::Surface(Surface::Message) | Decor::Rail { column: 1, .. }
        )));
}

#[test]
fn plain_content_stops_before_a_terminal_right_gutter() {
    let mut ui = ui();
    ui.add_user(&"long user request with ascii text ".repeat(20));
    ui.event(UiEvent::Text(
        "long assistant answer with ascii text ".repeat(40),
    ));
    ui.finish_task();
    let buffer = draw(&mut ui, 100, 50);
    let history = ui.selection_area;
    let cache = ui.history_cache.as_ref().unwrap();
    let mut checked = 0;
    for (row, item) in &cache.decor {
        if !matches!(item, Decor::Surface(Surface::Message))
            || *row < ui.history_top_row
            || *row >= ui.history_top_row + usize::from(history.height)
        {
            continue;
        }
        let y = history.y + (*row - ui.history_top_row) as u16;
        let surface = ui.message_surface_area(history, y);
        assert_eq!(surface.right(), history.right() - PLAIN_RIGHT_GUTTER);
        for x in surface.right()..history.right() {
            assert_eq!(
                buffer.cell((x, y)).unwrap().bg,
                ui.active_colors().background,
                "the right gutter must be terminal background at {x},{y}"
            );
            assert_eq!(
                buffer.cell((x, y)).unwrap().symbol(),
                " ",
                "content must not enter the right gutter at {x},{y}"
            );
        }
        checked += 1;
    }
    assert!(checked > 4);
}

#[test]
fn plain_code_copy_label_stays_left_of_the_terminal_gutter() {
    let mut ui = ui();
    ui.add_user("Show code");
    ui.event(UiEvent::Text("```rust\nfn main() {}\n```".into()));
    ui.finish_task();
    draw(&mut ui, 100, 40);
    let cache = ui.history_cache.as_ref().unwrap();
    let (row, block) = cache
        .decor
        .iter()
        .find_map(|(row, item)| match item {
            Decor::CodeLabel(block) => Some((*row, block)),
            _ => None,
        })
        .unwrap();
    assert!(row >= ui.history_top_row);
    let y = ui.selection_area.y + (row - ui.history_top_row) as u16;
    let label = ui.code_label(block, ui.selection_area.width);
    let size = UnicodeWidthStr::width(label.as_str()) as u16;
    let end = ui.selection_area.right() - PLAIN_RIGHT_GUTTER;
    assert!(ui.code_at(end - size, y).is_some());
    assert!(ui.code_at(end, y).is_none());
}

#[test]
fn connecting_after_a_user_turn_starts_its_own_assistant_block() {
    let mut ui = ui();
    ui.add_user("你好");
    ui.event(UiEvent::ModelStarted);
    ui.running = true;
    ui.phase = "waiting";
    ui.received_model_output = false;
    ui.phase_since = Some(std::time::Instant::now());
    let buffer = draw(&mut ui, 100, 30);
    let history = ui.selection_area;
    let status_y = (0..30u16)
        .find(|&y| {
            (0..100u16)
                .map(|x| buffer.cell((x, y)).unwrap().symbol())
                .collect::<String>()
                .contains("Connecting")
        })
        .expect("activity line should be visible");

    // The pinned activity is plain text on the terminal background.
    assert_eq!(
        buffer.cell((0, status_y)).unwrap().bg,
        ui.active_colors().background
    );
    // The last decorated user row ends before the separator.
    // The status strip owns the last surface row; the user block ends before it.
    let tail = u16::MAX as usize;
    let last_user = ui
        .history_cache
        .as_ref()
        .unwrap()
        .decor
        .iter()
        .filter_map(|(row, item)| {
            (matches!(item, Decor::Surface(Surface::Message)) && *row < tail).then_some(*row)
        })
        .max()
        .unwrap();
    let last_user_y = history.y + (last_user - ui.history_top_row) as u16;
    assert!(last_user_y + 1 < status_y);
    assert_eq!(
        buffer.cell((history.x + 1, last_user_y)).unwrap().bg,
        ui.message_colors().user_rail
    );
}

#[test]
fn short_system_messages_are_compact_without_hiding_error_details() {
    let mut ui = ui();
    ui.preferences.language = super::super::preferences::Language::Zh;
    ui.info("COMMAND", "Resumed session: project-work");
    let document = ui.history_document(ui.active_colors(), 100);
    let visible = document
        .0
        .lines
        .iter()
        .filter(|line| !line.to_string().trim().is_empty())
        .collect::<Vec<_>>();
    assert_eq!(visible.len(), 1, "brief status is a single row");
    assert!(visible[0].to_string().contains("已恢复会话：project-work"));
    assert!(!visible[0].to_string().contains("命令结果"));
    assert!(visible[0]
        .spans
        .iter()
        .all(|span| span.style.fg == Some(ui.active_colors().muted)
            || span.style.fg == Some(ui.active_colors().faint)));

    ui.info("ERROR", "Something failed\nDetails to keep");
    let output = ui.history_document(ui.active_colors(), 100).0.to_string();
    assert!(output.contains("Something failed"));
    assert!(output.contains("Details to keep"));
}

#[test]
fn plain_prose_restores_semantic_markdown_without_recoloring_inline_code() {
    let mut ui = ui();
    ui.add_user("q");
    ui.event(UiEvent::Text(
        "# 标题\n\n**重要** 正文 `代码` [文档](https://example.org)\n\n- 条目\n\n> 引用内容\n\n| 维度 | 分数 |\n|---|---|\n| 质量 | 好 |\n".into(),
    ));
    let semantic = ui.semantic_colors();
    let document = ui.history_document(ui.active_colors(), 80);
    let mut heading = false;
    let mut code = false;
    let mut link = false;
    let mut bullet = false;
    let mut emphasized = false;
    let mut table_heading = false;
    let mut quote = false;
    for line in &document.0.lines {
        for span in &line.spans {
            if span.content.contains("标题")
                && span.style.fg == Some(ui.active_colors().text)
                && span.style.add_modifier.contains(Modifier::BOLD)
            {
                heading = true;
            }
            if span.content.contains("代码")
                && span.style.fg == Some(ui.active_colors().code)
                && span.style.bg == Some(ui.active_colors().overlay)
            {
                code = true;
            }
            if span.content.contains("文档")
                && span.style.fg == Some(semantic.link)
                && span.style.add_modifier.contains(Modifier::UNDERLINED)
            {
                link = true;
            }
            if span.content.contains('•') && span.style.fg == Some(ui.active_colors().muted) {
                bullet = true;
            }
            if span.content.contains("重要")
                && span.style.fg == Some(ui.active_colors().text)
                && span.style.add_modifier.contains(Modifier::BOLD)
            {
                emphasized = true;
            }
            if span.content.contains("维度")
                && span.style.fg == Some(ui.active_colors().text)
                && span.style.add_modifier.contains(Modifier::BOLD)
            {
                table_heading = true;
            }
            if span.content.contains("引用内容")
                && span.style.fg == Some(ui.active_colors().muted)
                && !span.style.add_modifier.contains(Modifier::ITALIC)
            {
                quote = true;
            }
        }
    }
    assert!(heading, "headings are bright and bold, not link-colored");
    assert!(link, "links are identifiable even without hovering");
    assert!(bullet, "list structure uses a muted marker");
    assert!(emphasized, "strong text has visibly stronger hierarchy");
    assert!(table_heading, "table headings are bold and neutral");
    assert!(quote, "blockquotes are muted without an italic text wall");
    assert!(
        code,
        "inline code retains neutral ink and a distinct background"
    );
}

#[test]
fn cancelled_notice_sits_directly_under_its_turn() {
    let mut ui = ui();
    ui.add_user("你好");
    ui.entries.push(Entry::new(
        Kind::Assistant,
        "Kroma",
        format!(
            "{} Incomplete commands may have had side effects.]",
            crate::agent::TASK_CANCELLED_PREFIX
        ),
    ));
    ui.add_user("下一个问题");
    draw(&mut ui, 100, 24);
    let cache = ui.history_cache.as_ref().unwrap();
    let notice = cache
        .text
        .lines
        .iter()
        .position(|line| {
            line.spans
                .iter()
                .any(|span| span.content.contains("已取消") || span.content.contains("Cancelled"))
        })
        .expect("cancellation notice");
    assert!(
        cache
            .decor
            .iter()
            .any(|(row, decor)| *row == notice - 1 && matches!(decor, Decor::Rail { .. })),
        "no blank separator between the cancelled turn and its notice"
    );
    assert!(
        cache.text.lines[notice + 1].spans.is_empty(),
        "the next turn keeps its separator"
    );
}
