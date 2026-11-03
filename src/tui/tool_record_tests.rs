//! Independent execution records: calls remain visible; only per-call details fold.
use super::*;
use ratatui::{backend::TestBackend, buffer::Buffer, Terminal};

fn ui() -> UiState {
    UiState::new_default(
        "model-x".into(),
        "record-test".into(),
        ThemeStore::load_at(None).unwrap(),
        &[],
    )
}
fn draw(ui: &mut UiState, w: u16, h: u16) -> Buffer {
    let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
    t.draw(|f| ui.draw(f)).unwrap();
    t.backend().buffer().clone()
}
fn finish(ui: &mut UiState, id: &str, command: &str, result: &str, failed: bool) {
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
fn entries(ui: &UiState) -> Vec<&Entry> {
    ui.entries
        .iter()
        .filter(|e| matches!(e.kind, Kind::Tool))
        .collect()
}
fn card_text(card: &transcript::ToolCard) -> String {
    card.lines
        .iter()
        .map(Line::to_string)
        .collect::<Vec<_>>()
        .join("\n")
}
fn neutral(c: Color) -> bool {
    matches!(c, Color::Reset) || matches!(c,Color::Rgb(r,g,b) if r==g&&g==b)
}

#[test]
fn every_call_remains_visible_even_beyond_the_old_group_limit() {
    let mut ui = ui();
    for i in 0..24 {
        finish(
            &mut ui,
            &format!("id-{i}"),
            &format!("echo visible-{i:02}"),
            "exit: 0\nok",
            false,
        );
    }
    let card = ui.tool_card(&entries(&ui), ui.active_colors(), 100);
    assert_eq!(card.rows.len(), 24);
    assert_eq!(card.lines.len(), 24);
    assert!(card.blocks.is_empty());
    assert!(card.groups.is_empty());
    let text = card_text(&card);
    for i in 0..24 {
        assert!(text.contains(&format!("$ echo visible-{i:02}")));
    }
    assert!(!text.contains("Another") && !text.contains("steps"));
    assert_eq!(
        card.rows
            .iter()
            .map(|(_, id)| id.clone())
            .collect::<Vec<_>>(),
        (0..24).map(|i| format!("id-{i}")).collect::<Vec<_>>()
    );
}

#[test]
fn each_successful_record_is_one_quiet_command_line() {
    let mut ui = ui();
    finish(
        &mut ui,
        "read",
        "ls -la",
        "exit: 0\nsrc/\nCargo.toml\nREADME.md",
        false,
    );
    let entry = ui
        .entries
        .iter_mut()
        .find(|entry| matches!(entry.kind, Kind::Tool))
        .unwrap();
    entry.duration = Some(Duration::from_millis(700));
    let card = ui.tool_card(&entries(&ui), ui.active_colors(), 100);
    let text = card_text(&card);
    assert!(text.contains("$ ls -la"));
    assert!(!text.contains("0.7s"));
    assert!(!text.contains("Done") && !text.contains("Shell"));
    assert!(!text.contains("src/") && !text.contains("README.md"));
    assert_eq!(card.lines.len(), 1, "a successful call occupies one row");
    assert!(card.lines[0]
        .spans
        .iter()
        .any(|s| s.content == "✓ " && s.style.fg == Some(ui.semantic_colors().success)));
}

#[test]
fn successful_calls_are_not_hidden_when_another_call_fails() {
    let mut ui = ui();
    finish(&mut ui, "ok", "pwd", "exit: 0\n/project", false);
    finish(
        &mut ui,
        "bad",
        "git status",
        "exit: 128\nstderr: fatal: not a git repository",
        true,
    );
    let card = ui.tool_card(&entries(&ui), ui.active_colors(), 100);
    let text = card_text(&card);
    assert_eq!(card.rows.len(), 2);
    assert!(
        text.contains("$ pwd")
            && text.contains("$ git status")
            && text.contains("not a git repository")
    );
    let state = |row: usize| {
        card.lines[row]
            .spans
            .iter()
            .find(|span| span.style.fg.is_some_and(|color| !neutral(color)))
            .and_then(|span| span.style.fg)
    };
    assert_eq!(state(card.rows[0].0), Some(ui.semantic_colors().success));
    assert_eq!(state(card.rows[1].0), Some(ui.semantic_colors().error));
}

#[test]
fn live_preview_shows_the_latest_lines_without_losing_the_record() {
    let mut ui = ui();
    ui.event(UiEvent::ToolStarted {
        name: "shell".into(),
        id: "live".into(),
        args: "cargo test".into(),
    });
    ui.event(UiEvent::ToolOutput {
        id: "live".into(),
        stream: "stdout".into(),
        chunk: "old-1\nold-2\ncurrent-3\ncurrent-4\ncurrent-5\ncurrent-6\ncurrent-7\ncurrent-8\ncurrent-9\ncurrent-10\n".into(),
    });
    let card = ui.tool_card(&entries(&ui), ui.active_colors(), 100);
    let text = card_text(&card);
    assert!(text.contains("Running") && text.contains("current-3") && text.contains("current-10"));
    assert!(!text.contains("old-1") && !text.contains("old-2"));
    assert_eq!(card.rows.len(), 1);
}

#[test]
fn expanding_one_call_does_not_expand_its_neighbours_or_clear_the_draft() {
    let mut ui = ui();
    ui.add_user("Check tools");
    for id in ["one", "two"] {
        finish(
            &mut ui,
            id,
            &format!("echo {id}"),
            &format!("exit: 0\n{id}-first\n{id}-hidden"),
            false,
        );
    }
    ui.finish_task();
    ui.editor.paste("unsent 中文 draft");
    draw(&mut ui, 100, 60);
    let global = ui
        .history_cache
        .as_ref()
        .unwrap()
        .tool_rows
        .iter()
        .find(|(_, id)| id == "one")
        .unwrap()
        .0;
    let y = ui.selection_area.y + (global - ui.history_top_row) as u16;
    assert!(ui.toggle_tool_at(ui.selection_area.x + 2, y));
    assert!(ui.expanded_tool_calls.contains("one") && !ui.expanded_tool_calls.contains("two"));
    assert!(ui.expanded_tool_groups.is_empty());
    let text = card_text(&ui.tool_card(&entries(&ui), ui.active_colors(), 100));
    assert!(text.contains("one-hidden") && !text.contains("two-hidden"));
    assert_eq!(ui.editor.text(), "unsent 中文 draft");
}

#[test]
fn successful_records_stay_flat_and_failures_get_a_panel() {
    let mut success_ui = ui();
    success_ui.add_user("Inspect");
    finish(&mut success_ui, "one", "echo one", "exit: 0\none", false);
    finish(&mut success_ui, "two", "echo two", "exit: 0\ntwo", false);
    success_ui.finish_task();
    let b = draw(&mut success_ui, 100, 60);
    let cache = success_ui.history_cache.as_ref().unwrap();
    assert!(!cache
        .decor
        .iter()
        .any(|(_, d)| matches!(d, Decor::Surface(Surface::ToolRecord))));
    for (row, _) in &cache.tool_rows {
        let y = success_ui.selection_area.y + (*row - success_ui.history_top_row) as u16;
        assert_eq!(
            b.cell((success_ui.selection_area.x + 2, y)).unwrap().bg,
            success_ui.message_colors().surface
        );
    }

    let mut failed_ui = ui();
    failed_ui.add_user("Inspect");
    finish(
        &mut failed_ui,
        "bad",
        "echo bad",
        "exit: 1\nfailed diagnostic",
        true,
    );
    failed_ui.finish_task();
    let b = draw(&mut failed_ui, 100, 60);
    let cache = failed_ui.history_cache.as_ref().unwrap();
    let panel = cache
        .decor
        .iter()
        .find_map(|(row, d)| matches!(d, Decor::Surface(Surface::ToolRecord)).then_some(*row))
        .expect("failed records keep an attention panel");
    let y = failed_ui.selection_area.y + (panel - failed_ui.history_top_row) as u16;
    assert_eq!(
        b.cell((failed_ui.selection_area.x + 2, y)).unwrap().bg,
        failed_ui.active_colors().surface
    );
}

#[test]
fn mcp_records_show_real_names_and_arguments_not_a_fake_shell_command() {
    let mut ui = ui();
    ui.event(UiEvent::ToolStarted {
        name: "mcp_search".into(),
        id: "mcp".into(),
        args: "{\"query\":\"中文 test\"}".into(),
    });
    ui.event(UiEvent::ToolFinished {
        id: "mcp".into(),
        result: "two matches".into(),
        failed: false,
    });
    let card = ui.tool_card(&entries(&ui), ui.active_colors(), 100);
    let text = card_text(&card);
    assert!(text.contains("mcp_search") && text.contains("中文 test"));
    assert!(!text.contains("$ "));
    assert_eq!(card.lines.len(), 1);
}

#[test]
fn interrupted_results_stay_unknown_and_never_become_successful_output() {
    let mut ui = ui();
    finish(&mut ui,"unknown","external-command","Session interrupted. Tool result is unknown; do not assume the command succeeded or automatically repeat it.",true);
    let card = ui.tool_card(&entries(&ui), ui.active_colors(), 100);
    let text = card_text(&card);
    assert!(text.contains("Unknown") && text.contains("will not rerun automatically"));
    assert!(!text.contains("Done"));
    assert!(text.contains("Unknown"));
    assert!(card.lines[0]
        .spans
        .iter()
        .any(|span| span.content.contains("Unknown")
            && span.style.fg == Some(ui.semantic_colors().warning)));
}

#[test]
fn expanded_diff_only_colours_markers_and_plain_logs_remain_plain() {
    let mut ui = ui();
    finish(
        &mut ui,
        "diff",
        "git diff",
        "exit: 0\ndiff --git a/a b/a\n--- a/a\n+++ b/a\n@@ -1 +1 @@\n-old\n+new",
        false,
    );
    ui.toggle_all_tools();
    let card = ui.tool_card(&entries(&ui), ui.active_colors(), 100);
    for line in card.lines.iter().skip(1) {
        for s in &line.spans {
            if s.style.fg.is_some_and(|c| !neutral(c)) {
                assert!(s.content == "+" || s.content == "-");
            }
        }
    }
    let mut ordinary = self::ui();
    finish(
        &mut ordinary,
        "log",
        "echo log",
        "exit: 0\n+normal log\n-normal log",
        false,
    );
    ordinary.toggle_all_tools();
    let card = ordinary.tool_card(&entries(&ordinary), ordinary.active_colors(), 100);
    for l in card.lines.iter().skip(1) {
        assert!(l.spans.iter().all(|s| s.style.fg.is_none_or(neutral)));
    }
}

#[test]
fn headers_fit_small_widths_and_unicode_previews_never_split_graphemes() {
    let mut ui = ui();
    finish(
        &mut ui,
        "unicode",
        "echo 中文👩‍💻 e\u{301}",
        "exit: 0\n中文👩‍💻 e\u{301} unicode output",
        false,
    );
    for width in [24, 40, 80, 120] {
        let card = ui.tool_card(&entries(&ui), ui.active_colors(), width);
        assert!(card.lines[0].width() <= width);
        for l in &card.lines {
            let text = l.to_string();
            assert!(!text.ends_with('\u{200d}'));
        }
    }
}

#[test]
fn control_bytes_are_visible_not_emitted_and_raw_data_stays_unchanged() {
    let mut ui = ui();
    let raw = "exit: 0\n\u{1b}]52;c;payload\u{7}";
    finish(&mut ui, "safe", "printf test", raw, false);
    ui.toggle_all_tools();
    let card = ui.tool_card(&entries(&ui), ui.active_colors(), 100);
    let text = card_text(&card);
    assert!(!text.contains('\u{1b}') && !text.contains('\u{7}'));
    assert!(
        !text.contains("payload"),
        "the escape payload must be stripped"
    );
    assert!(text.contains("printf test"));
    let entry = entries(&ui)[0];
    assert_eq!(entry.details, raw);
}

#[test]
fn long_details_are_still_available_and_tiny_windows_do_not_panic() {
    let mut ui = ui();
    let output = (0..40)
        .map(|i| format!("line-{i:02}\n"))
        .collect::<String>();
    finish(
        &mut ui,
        "many",
        "printf multiline",
        &format!("exit: 0\n{output}"),
        false,
    );
    let short = card_text(&ui.tool_card(&entries(&ui), ui.active_colors(), 100));
    assert!(!short.contains("line-39"));
    ui.toggle_all_tools();
    assert!(card_text(&ui.tool_card(&entries(&ui), ui.active_colors(), 100)).contains("line-39"));
    for (w, h) in [(20, 8), (40, 16), (80, 24)] {
        draw(&mut ui, w, h);
        assert!(ui.editor_area.right() <= w && ui.editor_area.bottom() <= h);
    }
}

#[test]
fn only_failed_or_unknown_records_expose_the_snapshot_action() {
    let mut ui = ui();
    finish(&mut ui, "ok", "echo ok", "exit: 0\nok", false);
    finish(&mut ui, "bad", "echo bad", "exit: 1\nfailed", true);
    let entries = entries(&ui);
    let ok = ui.tool_card(&[entries[0]], ui.active_colors(), 100);
    let bad = ui.tool_card(&[entries[1]], ui.active_colors(), 100);
    assert!(ok.inspect.is_empty());
    assert_eq!(bad.inspect.len(), 1);
    assert_eq!(bad.inspect[0].1, "bad");
}

#[test]
fn ansi_csi_colours_are_stripped_without_touching_raw_output() {
    let mut ui = ui();
    let raw = "exit: 0\n\u{1b}[31mred\u{1b}[0m and \u{1b}[1;32mgreen\u{1b}[0m";
    finish(&mut ui, "ansi", "printf colour", raw, false);
    ui.toggle_all_tools();
    let card = ui.tool_card(&entries(&ui), ui.active_colors(), 100);
    let text = card_text(&card);
    assert!(text.contains("red and green"));
    assert!(!text.contains('\u{1b}') && !text.contains("[31m") && !text.contains("[0m"));
    assert_eq!(entries(&ui)[0].details, raw);
}

#[test]
fn failed_preview_prefers_the_diagnostic_over_unrelated_stdout() {
    let mut ui = ui();
    finish(
        &mut ui,
        "git",
        "cd /d/Kroma; ls -la; git status",
        "exit: 128\ntotal 16\nstderr: fatal: not a git repository",
        true,
    );
    let card = ui.tool_card(&entries(&ui), ui.active_colors(), 100);
    let text = card_text(&card);
    assert!(text.contains("not a git repository"));
    assert!(!text.contains("total 16"));
    assert!(text.contains("$ cd /d/Kroma; ls -la; git status"));
}

#[test]
fn successful_chains_drop_scaffolding_but_expansion_keeps_the_exact_command() {
    let mut ui = ui();
    finish(
        &mut ui,
        "chain",
        "cd /repo && echo \"===\" && ls && git status",
        "exit: 0\nok",
        false,
    );
    let card = ui.tool_card(&entries(&ui), ui.active_colors(), 100);
    let text = card_text(&card);
    assert!(text.contains("$ ls · git status"));
    assert!(!text.contains("echo") && !text.contains("cd "));
    ui.toggle_all_tools();
    let expanded = card_text(&ui.tool_card(&entries(&ui), ui.active_colors(), 100));
    assert!(expanded.contains("cd /repo && echo \"===\" && ls && git status"));
}

#[test]
fn redirected_echo_side_effects_are_not_folded_away() {
    let mut ui = ui();
    finish(
        &mut ui,
        "redirect",
        "echo overwritten > config.toml && cargo test",
        "exit: 0\nok",
        false,
    );
    let text = card_text(&ui.tool_card(&entries(&ui), ui.active_colors(), 100));
    assert!(
        text.contains("$ echo overwritten > config.toml · cargo test"),
        "the file write must survive the summary: {text}"
    );
    ui.toggle_all_tools();
    let expanded = card_text(&ui.tool_card(&entries(&ui), ui.active_colors(), 100));
    assert!(expanded.contains("echo overwritten > config.toml && cargo test"));
}

#[test]
fn substitution_echoes_stay_visible_while_literal_separators_fold() {
    let mut ui = ui();
    finish(
        &mut ui,
        "substitution",
        "cd /repo && echo $(cat version) && cargo test",
        "exit: 0\nok",
        false,
    );
    finish(
        &mut ui,
        "literal",
        "cd /repo && echo '===' && cargo test",
        "exit: 0\nok",
        false,
    );
    let entries = entries(&ui);
    let substitution = card_text(&ui.tool_card(&[entries[0]], ui.active_colors(), 100));
    assert!(substitution.contains("$ echo $(cat version) · cargo test"));
    let literal = card_text(&ui.tool_card(&[entries[1]], ui.active_colors(), 100));
    assert!(literal.contains("$ cargo test") && !literal.contains("echo"));
}

#[test]
fn successful_rows_have_no_disclosure_badge_and_keep_right_padding() {
    let mut ui = ui();
    finish(&mut ui, "short", "pwd", "exit: 0\n/repo", false);
    finish(
        &mut ui,
        "long",
        "cd /repo && echo \"===\" && cat Cargo.toml && git status --short",
        "exit: 0\nok",
        false,
    );
    let width = 100;
    let card = ui.tool_card(&entries(&ui), ui.active_colors(), width);
    let text = card_text(&card);
    assert!(!text.contains('▸') && !text.contains('▾'));
    for line in &card.lines {
        assert!(line.width() <= width - 2);
    }
}
