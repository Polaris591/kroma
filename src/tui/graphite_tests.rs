//! Graphite layout, hit testing, and read-only inspection regressions.
use super::*;
use ratatui::{backend::TestBackend, Terminal};

fn state() -> UiState {
    let mut ui = UiState::new(
        "test-model".into(),
        "test".into(),
        ThemeStore::load_at(None).unwrap(),
        &[],
    );
    ui.set_link_workdir(Path::new("D:/fixture/project"));
    ui.configure_status(500_000, Some("high".into()));
    ui
}

fn draw(ui: &mut UiState, width: u16, height: u16) -> ratatui::buffer::Buffer {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| ui.draw(frame)).unwrap();
    terminal.backend().buffer().clone()
}

fn record(ui: &mut UiState, id: &str, failed: bool) {
    ui.event(UiEvent::ToolStarted {
        name: "shell".into(),
        id: id.into(),
        args: format!("echo {id}"),
    });
    ui.event(UiEvent::ToolFinished {
        id: id.into(),
        result: if failed {
            "exit: 1\nstderr: permission denied".into()
        } else {
            "exit: 0\n    original 中文 output\n\narchive: output.log".into()
        },
        failed,
    });
}

#[test]
fn graphite_input_and_prose_share_a_column_without_message_boxes() {
    for preset in ["dark", "light", "mono"] {
        for (width, height) in [(40, 16), (80, 24), (100, 30), (160, 40)] {
            let mut ui = state();
            ui.themes.select(preset).unwrap();
            ui.add_user("User request\nsecond source line");
            ui.event(UiEvent::Text("Assistant response".into()));
            ui.editor.paste("Draft");
            let buffer = draw(&mut ui, width, height);
            assert_eq!(ui.selection_area.x + 4, ui.editor_area.x);
            let user = ui
                .selection_rows
                .iter()
                .position(|row| row.contains("User request"))
                .unwrap();
            let cell = buffer
                .cell((ui.selection_area.x + 4, ui.selection_area.y + user as u16))
                .unwrap();
            assert_eq!(cell.bg, ui.themes.colors.background);
            assert!(!cell.modifier.contains(Modifier::BOLD));
            let marks = ui
                .history_cache
                .as_ref()
                .unwrap()
                .decor
                .iter()
                .filter(|(_, item)| matches!(item, Decor::Rail { .. }))
                .count();
            assert_eq!(marks, 1);
            let rail = buffer
                .cell((ui.selection_area.x + 1, ui.selection_area.y + user as u16))
                .unwrap();
            assert_eq!(rail.symbol(), " ");
            assert_eq!(rail.bg, ui.themes.colors.user);
            assert!(!buffer
                .content()
                .iter()
                .any(|cell| matches!(cell.symbol(), "╭" | "╰" | "╮" | "╯")));
        }
    }
}

#[test]
fn recent_sessions_use_ids_and_reflow_without_stale_hit_regions() {
    let mut ui = state();
    ui.enter_home();
    ui.recent_sessions = vec![
        SessionEntry {
            id: "real-session-1".into(),
            name: Some("Duplicate title".into()),
            modified: None,
        },
        SessionEntry {
            id: "real-session-2".into(),
            name: Some("Duplicate title".into()),
            modified: None,
        },
        SessionEntry {
            id: "real-session-3".into(),
            name: Some("Third".into()),
            modified: None,
        },
    ];
    draw(&mut ui, 160, 40);
    assert_eq!(ui.recent_session_rows.len(), 3);
    let first = ui.recent_session_rows[0].0;
    let second = ui.recent_session_rows[1].0;
    assert_eq!(
        ui.recent_session_at(first.x, first.y),
        Some("real-session-1")
    );
    assert_eq!(
        ui.recent_session_at(second.x, second.y),
        Some("real-session-2")
    );
    ui.open_command_completion("/".into());
    assert!(ui.recent_session_at(first.x, first.y).is_none());
    ui.picker = None;
    draw(&mut ui, 40, 16);
    assert_eq!(ui.recent_session_rows.len(), 2);
    assert!(ui.recent_session_at(first.x, first.y).is_none());
    draw(&mut ui, 0, 0);
    assert!(ui.recent_session_rows.is_empty());
    ui.add_user("start");
    draw(&mut ui, 80, 24);
    assert!(ui.recent_session_rows.is_empty());
}

#[test]
fn plain_answers_end_quietly_but_failures_and_tool_durations_remain() {
    let mut ui = state();
    ui.add_user("hello");
    ui.event(UiEvent::Text("hello back".into()));
    ui.finish_task();
    ui.task_outcome = Some(TaskOutcome::Completed);
    assert!(!ui.has_activity_tail());
    record(&mut ui, "actual-call", false);
    assert!(ui.has_activity_tail());
    ui.add_user("next plain answer");
    ui.finish_task();
    ui.task_outcome = Some(TaskOutcome::Completed);
    assert!(
        !ui.has_activity_tail(),
        "old tool calls must not add a status to a new plain answer"
    );
    for outcome in [TaskOutcome::Failed, TaskOutcome::Cancelled] {
        ui.task_outcome = Some(outcome);
        assert!(ui.has_activity_tail());
    }
}

#[test]
fn inspector_is_bounded_read_only_searchable_and_uses_current_call_identity() {
    let mut ui = state();
    ui.add_user("inspect work");
    record(&mut ui, "first", false);
    record(&mut ui, "second", true);
    ui.editor.paste("unsent 中文 draft");
    ui.editor.key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Left,
        crossterm::event::KeyModifiers::NONE,
    ));
    let caret_before = ui.editor.viewport(60, 3);
    let entries_before = ui.entries.len();
    ui.tools_expanded = true;
    draw(&mut ui, 100, 30);
    let action = ui
        .history_cache
        .as_ref()
        .unwrap()
        .decor
        .iter()
        .find_map(|(row, item)| match item {
            Decor::Inspect(id) if id == "second" => Some(*row),
            _ => None,
        })
        .unwrap();
    let y = ui.selection_area.y + (action - ui.history_top_row) as u16;
    let x = ui.selection_area.right() - 2;
    assert_eq!(ui.inspection_at(x, y), Some("second"));
    assert!(ui.inspection_at(ui.selection_area.x + 8, y).is_none());
    assert!(!ui.selection_rows.join("\n").contains("[view]"));
    ui.inspect_tool("second");
    let panel = ui.plugin_panel.as_ref().unwrap();
    assert!(matches!(panel.placement, PanelPlacement::Docked));
    assert!(panel.body.contains("echo second"));
    assert!(panel.body.contains("permission denied"));
    assert!(!panel.body.contains("echo first"));
    for (width, height) in [(20, 8), (40, 16), (80, 24), (100, 30), (160, 40)] {
        let buffer = draw(&mut ui, width, height);
        assert!(ui.editor_area.height > 0);
        assert_eq!(ui.entries.len(), entries_before);
        assert!(buffer
            .content()
            .iter()
            .any(|cell| cell.bg == ui.themes.colors.overlay));
    }
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    assert!(ui.panel_search_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE)));
    for ch in "permission".chars() {
        assert!(ui.panel_search_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)));
    }
    draw(&mut ui, 100, 30);
    assert!(!ui
        .plugin_panel
        .as_ref()
        .unwrap()
        .search
        .as_ref()
        .unwrap()
        .matches
        .is_empty());
    ui.close_plugin_panel();
    assert_eq!(ui.editor.text(), "unsent 中文 draft");
    assert_eq!(ui.editor.viewport(60, 3), caret_before);
    ui.inspect_tool("first");
    assert!(ui
        .plugin_panel
        .as_ref()
        .unwrap()
        .body
        .contains("archive: output.log"));
    ui.close_plugin_panel();
    draw(&mut ui, 40, 16);
    assert!(!ui
        .history_cache
        .as_ref()
        .unwrap()
        .decor
        .iter()
        .any(|(_, item)| matches!(item, Decor::Inspect(_))));
    assert!(ui
        .history_text(ui.themes.colors, 37)
        .to_string()
        .contains("permission denied"));
}

#[test]
fn execution_picker_preserves_draft_and_invalid_ids_never_open_another_call() {
    let mut ui = state();
    ui.editor.paste("draft");
    ui.browse_execution();
    assert!(ui.picker.is_none());
    record(&mut ui, "call-1", false);
    record(&mut ui, "call-2", true);
    ui.browse_execution();
    let picker = ui.picker.as_mut().unwrap();
    assert!(picker
        .title
        .as_deref()
        .unwrap()
        .contains("Execution records"));
    assert_eq!(
        picker.key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Enter,
            crossterm::event::KeyModifiers::NONE
        )),
        super::super::picker::Action::Selected("/inspect call-2".into())
    );
    ui.picker = None;
    ui.inspect_tool("missing");
    assert!(ui.plugin_panel.is_none());
    assert_eq!(ui.editor.text(), "draft");
}

#[test]
fn comfortable_prose_caps_at_96_cells_but_code_uses_more_space() {
    let mut ui = state();
    ui.event(UiEvent::Text(format!(
        "{}\n\n```text\n{}\n```",
        "prose `inline` words ".repeat(20),
        "C".repeat(140)
    )));
    draw(&mut ui, 160, 40);
    let prose = ui.selection_rows.iter().filter(|row| row.contains("prose"));
    assert!(prose.clone().count() > 1);
    assert!(prose
        .clone()
        .all(|row| UnicodeWidthStr::width(row.as_str()) <= 100));
    assert!(ui
        .selection_rows
        .iter()
        .any(|row| row.contains("CCCC") && UnicodeWidthStr::width(row.as_str()) > 100));
}
