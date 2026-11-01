//! Behavioural coverage for the Calm Transcript decoration and grouping seam.
use super::*;
use ratatui::{backend::TestBackend, Terminal};

fn ui() -> UiState {
    UiState::new(
        "model".into(),
        "session".into(),
        ThemeStore::load_at(None).unwrap(),
        &[],
    )
}
fn draw(ui: &mut UiState, width: u16, height: u16) -> ratatui::buffer::Buffer {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| ui.draw(frame)).unwrap();
    terminal.backend().buffer().clone()
}
fn tool(ui: &mut UiState, id: &str, failed: bool, result: &str) {
    ui.event(UiEvent::ToolStarted {
        name: "shell".into(),
        id: id.into(),
        args: format!("echo {id}"),
    });
    ui.event(UiEvent::ToolFinished {
        id: id.into(),
        result: result.into(),
        failed,
    });
}
#[test]
fn raised_blocks_wrap_without_polluting_selection_or_links() {
    for preset in ["dark", "light", "mono"] {
        let mut ui = ui();
        ui.themes.select(preset).unwrap();
        ui.add_user(&"中文🙂 message ".repeat(15));
        ui.event(UiEvent::Text(
            "Read https://example.org/page\n\n```rust\nlet value = 42;\n```".into(),
        ));
        ui.running = false;
        let buffer = draw(&mut ui, 40, 40);
        assert!(ui
            .visible_links
            .iter()
            .any(|link| link.target == "https://example.org/page"));
        assert!(!ui
            .selection_rows
            .iter()
            .any(|row| row.contains('▎') || row.contains('◆') || row.trim() == "rust"));
        let cache = ui.history_cache.as_ref().unwrap();
        let rows = cache
            .decor
            .iter()
            .filter(|(_, item)| matches!(item, Decor::Surface(_)))
            .count();
        assert!(rows > 0, "code block retains its surface");
        assert!(
            cache
                .decor
                .iter()
                .filter(|(_, item)| matches!(item, Decor::Gutter { glyph: "▎", .. }))
                .count()
                == 1,
            "user identity is marked once, not on every wrapped row"
        );
        let user_row = ui
            .selection_rows
            .iter()
            .position(|row| row.contains("message"))
            .unwrap();
        assert_eq!(
            buffer
                .cell((
                    ui.selection_area.x + 1,
                    ui.selection_area.y + user_row as u16
                ))
                .unwrap()
                .symbol(),
            "▎"
        );
        ui.selection_start = Some((0, 0));
        ui.selection_end = Some((cache.text.lines.len() - 1, 39));
        let copied = ui.selected_text().unwrap();
        assert!(!copied.contains('▎') && !copied.contains('◆'));
    }
}
#[test]
fn completed_groups_collapse_and_failures_diagnostics_and_interruptions_stay_visible() {
    let mut ui = ui();
    tool(&mut ui, "one", false, "exit: 0\nok");
    ui.event(UiEvent::ReasoningFinal("inspect next".into()));
    tool(&mut ui, "two", false, "exit: 0\nok");
    let collapsed = ui.history_text(ui.themes.colors, 100).to_string();
    assert!(collapsed.contains("2 steps"));
    assert!(!collapsed.contains("echo one"));
    tool(&mut ui, "bad", true, "exit: 1\nstderr: permission denied");
    tool(&mut ui,"unknown",true,"Session interrupted. Tool result is unknown; do not assume the command succeeded or automatically repeat it.");
    tool(
        &mut ui,
        "warn",
        false,
        "exit: 0\nstderr: warning: check configuration",
    );
    let text = ui.history_text(ui.themes.colors, 120).to_string();
    assert!(text.contains("permission denied"));
    assert!(text.contains("result unknown"));
    assert!(text.contains("diagnostic reported"));
    assert!(text.contains("Another 2 successful steps"));
    let unknown = ui
        .entries
        .iter()
        .find(|e| e.call_id.as_deref() == Some("unknown"))
        .unwrap();
    let card = ui.tool_card(&[unknown], ui.themes.colors, 120);
    assert_eq!(card.lines[0].spans[1].content, "⊘ ");
    assert_eq!(
        card.lines[0].spans[1].style.fg,
        Some(ui.themes.colors.warning)
    );
}
#[test]
fn cancellation_is_localized_and_live_completion_has_no_duplicate_notice() {
    let message = Message::new(
        "assistant",
        format!(
            "{} Incomplete commands may have had side effects.]",
            crate::agent::TASK_CANCELLED_PREFIX
        ),
    );
    let mut ui = ui();
    ui.load_history(std::slice::from_ref(&message));
    let history = ui.history_text(ui.themes.colors, 120).to_string();
    assert!(history.contains("⊘ Cancelled"));
    assert!(!history.contains("[Task cancelled"));
    ui.task_outcome = Some(TaskOutcome::Cancelled);
    ui.info("CANCELLED", "cancelled");
    assert!(!ui
        .history_text(ui.themes.colors, 120)
        .to_string()
        .contains("Cancelled"));
    assert!(ui
        .activity_line(ui.themes.colors)
        .to_string()
        .contains("side effects are not undone"));
    assert!(message
        .content
        .unwrap()
        .starts_with(crate::agent::TASK_CANCELLED_PREFIX));
}

#[test]
fn context_thresholds_unknown_usage_and_notice_severity_are_distinct() {
    let mut ui = ui();
    ui.configure_status(100_000, Some("high".into()));
    assert!(ui
        .footer_line(ui.themes.colors, 120)
        .to_string()
        .contains("Context —"));
    for (input, color) in [
        (69_000, ui.themes.colors.muted),
        (75_000, ui.themes.colors.warning),
        (95_000, ui.themes.colors.error),
    ] {
        ui.event(UiEvent::ActualInput(input));
        let line = ui.footer_line(ui.themes.colors, 120);
        let context = line
            .spans
            .iter()
            .find(|span| span.content.starts_with("Context "))
            .unwrap();
        assert_eq!(context.style.fg, Some(color));
        assert_eq!(
            context.content.as_ref(),
            format!("Context {}%", input / 1000)
        );
        assert!(ui.context_display().contains("/ 100k"));
    }
    for (level, color) in [
        (NoticeLevel::Info, ui.themes.colors.accent_text),
        (NoticeLevel::Warning, ui.themes.colors.warning),
        (NoticeLevel::Error, ui.themes.colors.error),
    ] {
        ui.note_with_level(level, "notice");
        let buffer = draw(&mut ui, 80, 24);
        let cell = buffer
            .content
            .iter()
            .find(|cell| cell.symbol() == "n" && cell.fg == color);
        assert!(cell.is_some(), "notice severity colour missing");
    }
}
#[test]
fn waiting_phase_tracks_model_output_and_idle_hint_uses_last_output() {
    let mut ui = ui();
    ui.add_user("task");
    ui.event(UiEvent::ModelStarted);
    assert!(ui.activity_status().starts_with("Connecting"));
    ui.event(UiEvent::Text("answer".into()));
    ui.event(UiEvent::ModelStarted);
    assert!(ui.activity_status().starts_with("Waiting for model"));
    ui.last_output = Instant::now() - Duration::from_secs(9);
    assert!(ui
        .activity_line(ui.themes.colors)
        .to_string()
        .contains("Ctrl+C"));
    ui.event(UiEvent::Text("next".into()));
    assert!(!ui
        .activity_line(ui.themes.colors)
        .to_string()
        .contains("Ctrl+C"));
}
#[test]
fn late_git_results_are_scoped_to_session_and_turn_and_summary_is_success_only() {
    use crate::tui::gitinfo::{Changes, Info};
    let mut ui = ui();
    ui.add_user("task");
    let revision = ui.git_revision;
    ui.event(UiEvent::GitInfo {
        revision: revision - 1,
        session: ui.session.clone(),
        info: Info::default(),
        changes: Some(Changes {
            files: 99,
            ..Changes::default()
        }),
    });
    assert!(ui.turn_changes.is_none());
    ui.event(UiEvent::GitInfo {
        revision,
        session: ui.session.clone(),
        info: Info {
            branch: Some("feature/tui".into()),
            dirty: true,
        },
        changes: Some(Changes {
            files: 3,
            added: 42,
            removed: 7,
            untracked: 2,
        }),
    });
    ui.finish_task();
    ui.task_outcome = Some(TaskOutcome::Completed);
    assert!(ui
        .task_status()
        .contains("Workspace 3 files +42 −7 · Untracked 2 files · /diff"));
    ui.task_outcome = Some(TaskOutcome::Failed);
    assert!(!ui.task_status().contains("/diff"));
    ui.task_outcome = Some(TaskOutcome::Cancelled);
    assert!(!ui.task_status().contains("/diff"));
    ui.load_history(&[]);
    assert!(ui.turn_changes.is_none());
}

#[test]
fn turn_navigation_survives_five_turns_reflow_and_preserves_the_draft() {
    let mut ui = ui();
    for index in 0..5 {
        ui.add_user(&format!("turn {index} {}", "中文 long request ".repeat(8)));
        ui.event(UiEvent::Text("answer line\n\n".repeat(16)));
        ui.finish_task();
    }
    ui.editor.paste("unsent draft");
    draw(&mut ui, 80, 24);
    for width in [80, 40, 100, 160] {
        ui.scroll_to_bottom();
        draw(&mut ui, width, 24);
        for expected in (0..5).rev() {
            ui.jump_turn(true);
            draw(&mut ui, width, 24);
            let index = ui.navigation_turn.expect("turn navigation active");
            assert!(ui.entries[index]
                .body
                .starts_with(&format!("turn {expected} ")));
            let cache = ui.history_cache.as_ref().unwrap();
            let row = cache
                .decor
                .iter()
                .find_map(|(row, item)| {
                    matches!(item,Decor::TurnStart(turn) if *turn==index).then_some(*row)
                })
                .unwrap();
            assert_eq!(ui.history_top_row, row.min(ui.history_max_scroll));
        }
        for expected in 1..5 {
            ui.jump_turn(false);
            draw(&mut ui, width, 24);
            assert!(ui.entries[ui.navigation_turn.unwrap()]
                .body
                .starts_with(&format!("turn {expected} ")));
        }
        let turn = ui.navigation_turn;
        draw(&mut ui, if width == 40 { 100 } else { 40 }, 24);
        assert_eq!(ui.navigation_turn, turn);
    }
    assert_eq!(ui.editor.text(), "unsent draft");
    ui.attach_mode = true;
    let turn = ui.navigation_turn;
    ui.jump_turn(true);
    assert_eq!(ui.navigation_turn, turn);
}
#[test]
fn code_copy_hits_only_its_label_and_returns_original_tabs_blanks_and_unicode() {
    let source = "\tfn main() {\n\n\tprintln!(\"你好🙂\");\n}\n";
    let mut ui = ui();
    ui.entries.push(Entry::new(
        Kind::Assistant,
        "Kroma",
        format!("```rust\n{source}```"),
    ));
    for width in [40, 80, 100, 160] {
        draw(&mut ui, width, 24);
        let cache = ui.history_cache.as_ref().unwrap();
        let (row, _) = cache
            .decor
            .iter()
            .find(|(_, item)| matches!(item, Decor::CodeLabel(_)))
            .unwrap();
        let y = ui.selection_area.y + (*row - ui.history_top_row) as u16;
        let x = ui.selection_area.right() - 2;
        assert_eq!(ui.code_at(x, y), Some(source));
        assert!(ui.code_at(ui.selection_area.x, y).is_none());
        assert!(ui.code_at(x, y + 1).is_none());
        assert!(!ui
            .selection_rows
            .iter()
            .any(|row| row.contains("Copy") || row.trim() == "rust"));
    }
}

#[test]
fn full_transcript_search_expands_every_step_and_never_edits_the_draft() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let mut ui = ui();
    ui.editor.paste("draft");
    for index in 0..20 {
        tool(
            &mut ui,
            &format!("step-{index}"),
            false,
            &format!("exit: 0\nevidence {index}"),
        );
    }
    ui.event(UiEvent::ReasoningFinal("reasoning evidence".into()));
    ui.open_full_transcript();
    draw(&mut ui, 80, 24);
    assert!(ui
        .plugin_panel
        .as_ref()
        .unwrap()
        .body
        .contains("echo step-0"));
    assert!(ui
        .plugin_panel
        .as_ref()
        .unwrap()
        .body
        .contains("echo step-19"));
    assert!(ui
        .plugin_panel
        .as_ref()
        .unwrap()
        .body
        .contains("reasoning evidence"));
    for ch in "/evidence".chars() {
        assert!(ui.panel_search_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)));
        draw(&mut ui, 80, 24);
    }
    assert_eq!(
        ui.plugin_panel
            .as_ref()
            .unwrap()
            .search
            .as_ref()
            .unwrap()
            .matches
            .len(),
        21
    );
    ui.panel_search_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    ui.panel_search_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
    draw(&mut ui, 80, 24);
    assert_eq!(
        ui.plugin_panel
            .as_ref()
            .unwrap()
            .search
            .as_ref()
            .unwrap()
            .current,
        1
    );
    ui.panel_search_key(KeyEvent::new(KeyCode::Char('N'), KeyModifiers::SHIFT));
    draw(&mut ui, 40, 16);
    assert_eq!(
        ui.plugin_panel
            .as_ref()
            .unwrap()
            .search
            .as_ref()
            .unwrap()
            .current,
        0
    );
    assert_eq!(ui.editor.text(), "draft");
}

#[test]
fn group_hidden_success_hint_is_clickable_and_disclosure_does_not_leak_sessions() {
    let mut ui = ui();
    tool(&mut ui, "success", false, "exit: 0\nok");
    tool(&mut ui, "failed", true, "exit: 1\nstderr: denied");
    draw(&mut ui, 100, 30);
    let row = ui
        .selection_rows
        .iter()
        .position(|row| row.contains("successful step"))
        .unwrap();
    assert!(ui.toggle_tool_at(ui.selection_area.x + 6, ui.selection_area.y + row as u16));
    assert!(ui.expanded_tool_groups.contains("success"));
    ui.session = "another-session".into();
    ui.load_history(&[]);
    assert!(ui.expanded_tool_groups.is_empty());
}
#[test]
fn selection_of_whole_code_and_tool_cards_excludes_decoration_gutters_and_labels() {
    let mut ui = ui();
    ui.entries.push(Entry::new(
        Kind::Assistant,
        "Kroma",
        "```rust\n    let count = 42;\n```",
    ));
    tool(&mut ui, "tool", false, "exit: 0\n    actual output");
    ui.tools_expanded = true;
    draw(&mut ui, 100, 40);
    let cache = ui.history_cache.as_ref().unwrap();
    ui.selection_start = Some((0, 0));
    ui.selection_end = Some((cache.text.lines.len() - 1, 99));
    let copied = ui.selected_text().unwrap();
    for decoration in ["│", "▎", "◆", "⧉", "Copy", "rust"] {
        assert!(
            !copied.contains(decoration),
            "copied decoration {decoration}: {copied}"
        );
    }
    assert!(copied.contains("    let count = 42;"));
    assert!(copied.contains("    actual output"));
    assert!(ui.selection_rows.iter().all(|row| !row.contains('│')));
}

#[test]
fn obsidian_refinements_deduplicate_group_headers_humanize_session_ids_and_toggle_reasoning() {
    let mut ui = ui();
    ui.set_link_workdir(Path::new("D:/Kroma"));
    ui.add_user("你好");
    ui.event(UiEvent::ReasoningFinal("hidden chain of thought".into()));
    tool(
        &mut ui,
        "fail-1",
        true,
        "exit: 128
fatal: not a git repository",
    );
    tool(
        &mut ui,
        "ok-2",
        false,
        "exit: 0
clean",
    );
    let buffer = draw(&mut ui, 100, 30);
    let header: String = (0..100)
        .map(|x| buffer.cell((x, 0)).unwrap().symbol())
        .collect();
    assert_eq!(
        header.matches("Kroma").count(),
        1,
        "do not repeat Kroma workdir in header: {header}"
    );
    let text = ui.history_text(ui.themes.colors, 100).to_string();
    assert!(text.contains("Executed 2 steps · 1 failed"));
    assert_eq!(
        text.matches("echo fail-1").count(),
        1,
        "failed child command must not be duplicated on parent row"
    );
    assert!(text.contains("Another 1 successful step"));
    assert!(!text.contains("1 successful steps"));
    let reasoning_row = ui
        .selection_rows
        .iter()
        .position(|row| row.contains("Reasoning collapsed"))
        .unwrap();
    assert!(ui.toggle_tool_at(
        ui.selection_area.x + 4,
        ui.selection_area.y + reasoning_row as u16
    ));
    assert!(ui.reasoning_expanded);
    assert_eq!(
        super::chrome::display_session_title(
            "s18e3af53f0efa0fc-0-2398",
            super::super::preferences::Language::En
        ),
        "Untitled session"
    );
    assert_eq!(
        super::chrome::display_session_title(
            "s18e3af53f0efa0fc-0-2398",
            super::super::preferences::Language::Zh
        ),
        "未命名会话"
    );
}
