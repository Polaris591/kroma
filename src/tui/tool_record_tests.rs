//! Finished tool runs collapse; Ctrl+O and the group chevron restore details.
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
    matches!(c, Color::Reset) || matches!(c, Color::Rgb(r,g,b) if r <= g && g <= b && b - r <= 32)
}

#[test]
fn classic_tool_panel_shows_test_counts_build_result_and_bounded_file_output() {
    let mut ui = ui();
    finish(
        &mut ui,
        "tests",
        "cargo test --lib",
        "exit: 0\nrunning 2 tests\ntest example ... ok\ntest second ... ok\ntest result: ok. 2 passed; 0 failed; 0 ignored\nRAW_EXTRA_LOG",
        false,
    );
    finish(
        &mut ui,
        "build",
        "cargo build --release",
        "exit: 0\nCompiling kroma\nFinished `release` profile [optimized] target(s) in 4.2s",
        false,
    );
    finish(
        &mut ui,
        "files",
        "ls src",
        "exit: 0\nagent.rs\ncontext.rs\ntui\nplugins.rs",
        false,
    );
    let card = ui.tool_card(&entries(&ui), ui.active_colors(), 100);
    let excerpt = card_text(&card);
    assert!(
        excerpt.contains("2 passed; 0 failed; 0 ignored"),
        "{excerpt}"
    );
    assert!(excerpt.contains("Finished `release` profile"), "{excerpt}");
    assert!(excerpt.contains("agent.rs") && excerpt.contains("context.rs"));
    assert!(!excerpt.contains("RAW_EXTRA_LOG") && !excerpt.contains("plugins.rs"));
    assert_eq!(card.rows.len(), 3);
    assert_eq!(card.blocks.len(), 3, "one gray background for every tool");
    ui.toggle_all_tools();
    let raw = card_text(&ui.tool_card(&entries(&ui), ui.active_colors(), 100));
    assert!(raw.contains("RAW_EXTRA_LOG") && raw.contains("plugins.rs"));
    assert!(raw.contains("test second ... ok"));
}

#[test]
fn classic_tool_preview_preserves_unicode_and_does_not_invent_output() {
    let mut ui = ui();
    finish(&mut ui, "no-output", "echo quiet", "exit: 0", false);
    finish(
        &mut ui,
        "unicode",
        "rg 用户 src",
        "exit: 0\nsrc/tui/mod.rs:42: 中文🙂\nsrc/tui/view.rs:12: 用户标记",
        false,
    );
    let text = card_text(&ui.tool_card(&entries(&ui), ui.active_colors(), 80));
    assert!(text.contains("echo quiet"));
    assert!(text.contains("中文🙂"));
    assert!(text.contains("用户标记"));
    assert_eq!(
        text.matches("↳").count(),
        1,
        "a tool with no recorded stdout should not receive invented output"
    );
}

#[test]
fn detailed_records_show_real_commands_and_recoverable_failures() {
    let mut ui = ui();
    finish(
        &mut ui,
        "git",
        "git status --short",
        "exit: 128
stderr: fatal: not a git repository",
        true,
    );
    finish(
        &mut ui,
        "read",
        "cat src/main.rs",
        "exit: 0
fn main() {}",
        false,
    );
    ui.event(UiEvent::ToolStarted {
        name: "shell".into(),
        id: "tests".into(),
        args: "cargo test --lib".into(),
    });
    let card = ui.tool_card(&entries(&ui), ui.active_colors(), 100);
    let text = card_text(&card);
    assert!(text.contains("$ git status --short") && text.contains("not a git repository"));
    assert!(text.contains("$ cat src/main.rs") && text.contains("$ cargo test --lib"));
    assert!(
        text.contains("fn main()"),
        "short successful output stays visible"
    );
    assert_eq!(card.rows.len(), 3);
    assert!(card.groups.is_empty());
    assert!(card
        .lines
        .iter()
        .any(|line| line.spans.iter().any(
            |span| span.content == "◌ " && span.style.fg == Some(ui.semantic_colors().running)
        )));
    ui.toggle_all_tools();
    assert!(card_text(&ui.tool_card(&entries(&ui), ui.active_colors(), 100)).contains("fn main()"));
}

#[test]
fn completed_runs_keep_one_quiet_row_per_call_and_expand_without_loss() {
    let mut ui = ui();
    for i in 0..24 {
        finish(
            &mut ui,
            &format!("id-{i}"),
            &format!("echo visible-{i:02}"),
            "exit: 0
RAW_SUCCESS_OUTPUT",
            false,
        );
    }
    let card = ui.tool_card(&entries(&ui), ui.active_colors(), 100);
    assert_eq!(card.rows.len(), 24);
    assert_eq!(
        card.lines.len(),
        48,
        "each completed call has a command and result"
    );
    assert!(card.groups.is_empty() && card.blocks.len() == 24);
    assert!(card_text(&card).contains("visible-00") && card_text(&card).contains("visible-23"));
    assert!(card_text(&card).contains("RAW_SUCCESS_OUTPUT"));
    ui.toggle_all_tools();
    let expanded = ui.tool_card(&entries(&ui), ui.active_colors(), 100);
    assert_eq!(expanded.rows.len(), 24);
    assert!(card_text(&expanded).contains("RAW_SUCCESS_OUTPUT"));
    assert_eq!(
        expanded
            .rows
            .iter()
            .map(|(_, id)| id.clone())
            .collect::<Vec<_>>(),
        (0..24).map(|i| format!("id-{i}")).collect::<Vec<_>>()
    );
}

#[test]
fn successful_tool_shows_short_key_result_with_original_log_expandable() {
    let mut ui = ui();
    finish(
        &mut ui,
        "read",
        "ls -la",
        "exit: 0
src/
Cargo.toml
README.md",
        false,
    );
    ui.entries
        .iter_mut()
        .find(|e| matches!(e.kind, Kind::Tool))
        .unwrap()
        .duration = Some(Duration::from_millis(700));
    let card = ui.tool_card(&entries(&ui), ui.active_colors(), 100);
    let text = card_text(&card);
    assert!(text.contains("$ ls -la") && !text.contains("0.7s"));
    assert!(text.contains("src/") && text.contains("Cargo.toml"));
    assert!(!text.contains("README.md"), "long results stay bounded");
    assert!(text.contains("(+1 lines)"));
    assert_eq!(card.lines.len(), 2);
    assert!(card.lines[0]
        .spans
        .iter()
        .any(|span| span.content == "✓ " && span.style.fg == Some(ui.semantic_colors().success)));
    ui.toggle_all_tools();
    let expanded = card_text(&ui.tool_card(&entries(&ui), ui.active_colors(), 100));
    assert!(
        expanded.contains("$ ls -la")
            && expanded.contains("src/")
            && expanded.contains("README.md")
    );
}

#[test]
fn successful_calls_are_not_hidden_when_another_call_fails() {
    let mut ui = ui();
    finish(
        &mut ui,
        "ok",
        "pwd",
        "exit: 0
/project",
        false,
    );
    finish(
        &mut ui,
        "bad",
        "git status",
        "exit: 128
stderr: fatal: not a git repository",
        true,
    );
    let card = ui.tool_card(&entries(&ui), ui.active_colors(), 100);
    let text = card_text(&card);
    assert_eq!(card.rows.len(), 2);
    assert!(text.contains("$ pwd") && text.contains("$ git status"));
    assert!(text.contains("not a git repository") && text.contains("/project"));
    assert!(card.lines.iter().any(|line| line
        .spans
        .iter()
        .any(|span| span.content == "✕ " && span.style.fg == Some(ui.semantic_colors().error))));
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
    assert!(text.contains("$ cargo test") && text.contains("current-10"));
    assert!(!text.contains("current-3") && !text.contains("old-1"));
    assert_eq!(card.rows.len(), 1, "one live tool record");
    ui.toggle_all_tools();
    let full = card_text(&ui.tool_card(&entries(&ui), ui.active_colors(), 100));
    assert!(full.contains("current-3") && full.contains("current-10"));
    assert!(
        full.contains("old-1"),
        "full mode shows original captured logs"
    );
}

#[test]
fn clicking_a_tool_record_expands_only_that_call_without_clearing_draft() {
    let mut ui = ui();
    ui.add_user("Check tools");
    for id in ["one", "two"] {
        finish(
            &mut ui,
            id,
            &format!("echo {id}"),
            &format!(
                "exit: 0
{id}-hidden"
            ),
            false,
        );
    }
    ui.finish_task();
    ui.editor.paste("unsent 中文 draft");
    draw(&mut ui, 100, 60);
    let row = ui.history_cache.as_ref().unwrap().tool_rows[0].0;
    let y = ui.selection_area.y + (row - ui.history_top_row) as u16;
    assert!(ui.toggle_tool_at(ui.selection_area.x + 6, y));
    assert!(!ui.has_plugin_panel());
    assert!(ui.expanded_tool_calls.contains("one") && !ui.expanded_tool_calls.contains("two"));
    draw(&mut ui, 100, 60);
    let text = ui.history_text(ui.active_colors(), 100).to_string();
    assert!(
        text.contains("one-hidden") && text.contains("two-hidden"),
        "both brief results remain visible, independent of expansion"
    );
    assert_eq!(ui.editor.text(), "unsent 中文 draft");
    assert!(!ui.has_plugin_panel());
    assert_eq!(ui.editor.text(), "unsent 中文 draft");
}

#[test]
fn all_tool_records_share_one_subtle_gray_surface() {
    let mut success_ui = ui();
    success_ui.add_user("Inspect");
    finish(&mut success_ui, "one", "echo one", "exit: 0\none", false);
    finish(&mut success_ui, "two", "echo two", "exit: 0\ntwo", false);
    success_ui.finish_task();
    let b = draw(&mut success_ui, 100, 60);
    let cache = success_ui.history_cache.as_ref().unwrap();
    assert!(cache
        .decor
        .iter()
        .any(|(_, d)| matches!(d, Decor::Surface(Surface::ToolRecord))));
    for (row, _) in &cache.tool_rows {
        let y = success_ui.selection_area.y + (*row - success_ui.history_top_row) as u16;
        assert_eq!(
            b.cell((success_ui.selection_area.x + 4, y)).unwrap().bg,
            success_ui.message_colors().surface
        );
        assert_eq!(
            b.cell((success_ui.selection_area.x + 3, y)).unwrap().bg,
            success_ui.active_colors().background,
            "the gray panel stays inset from the assistant rail"
        );
        assert_eq!(
            b.cell((success_ui.selection_area.x + 6, y))
                .unwrap()
                .symbol(),
            "▸",
            "the disclosure chevron is visibly separated from the rail"
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
    draw(&mut failed_ui, 100, 60);
    assert!(failed_ui
        .history_cache
        .as_ref()
        .unwrap()
        .decor
        .iter()
        .any(|(_, d)| matches!(d, Decor::Surface(Surface::ToolRecord))));
    failed_ui.toggle_all_tools();
    let b = draw(&mut failed_ui, 100, 60);
    let cache = failed_ui.history_cache.as_ref().unwrap();
    let panel = cache
        .decor
        .iter()
        .find_map(|(row, d)| matches!(d, Decor::Surface(Surface::ToolRecord)).then_some(*row))
        .expect("full mode keeps failure diagnostics in a tinted panel");
    let y = failed_ui.selection_area.y + (panel - failed_ui.history_top_row) as u16;
    assert_eq!(
        b.cell((failed_ui.selection_area.x + 4, y)).unwrap().bg,
        failed_ui.message_colors().surface
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
    assert!(text.contains("mcp_search"));
    assert!(text.contains("中文 test") && !text.contains("$ "));
    assert_eq!(card.lines.len(), 2);
    assert!(text.contains("two matches"));
    ui.toggle_all_tools();
    let full = card_text(&ui.tool_card(&entries(&ui), ui.active_colors(), 100));
    assert!(full.contains("mcp_search") && full.contains("中文 test"));
    assert!(!full.contains("$ mcp_search"));
}

#[test]
fn interrupted_results_stay_unknown_and_never_become_successful_output() {
    let mut ui = ui();
    finish(&mut ui,"unknown","external-command","Session interrupted. Tool result is unknown; do not assume the command succeeded or automatically repeat it.",true);
    let card = ui.tool_card(&entries(&ui), ui.active_colors(), 100);
    let text = card_text(&card);
    assert!(text.contains("Unknown"));
    assert!(text.contains("will not rerun automatically"));
    assert!(card.lines[0]
        .spans
        .iter()
        .any(|span| span.content == "⊘ " && span.style.fg == Some(ui.semantic_colors().warning)));
    ui.toggle_all_tools();
    let full = card_text(&ui.tool_card(&entries(&ui), ui.active_colors(), 100));
    assert!(full.contains("Result unknown; will not rerun automatically"));
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
    for line in card.lines.iter().skip(2) {
        for s in &line.spans {
            if s.style.fg.is_some_and(|c| !neutral(c)) {
                assert!(
                    s.content == "+" || s.content == "-" || s.content == "✓ ",
                    "unexpected diff colour: {:?}",
                    s.content
                );
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
    for l in card.lines.iter().skip(2) {
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
        .collect::<Vec<_>>()
        .concat();
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
    let original_entries = entries(&ui);
    let ok = ui.tool_card(&[original_entries[0]], ui.active_colors(), 100);
    let bad = ui.tool_card(&[original_entries[1]], ui.active_colors(), 100);
    assert!(
        ok.inspect.is_empty() && bad.inspect.len() == 1,
        "failure inspection is available without expanding"
    );
    drop(original_entries);
    ui.toggle_all_tools();
    let expanded_entries = entries(&ui);
    let bad = ui.tool_card(&[expanded_entries[1]], ui.active_colors(), 100);
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
    assert!(text.contains("Failed"));
    assert!(text.contains("not a git repository") && !text.contains("total 16"));
    ui.toggle_all_tools();
    let full = card_text(&ui.tool_card(&entries(&ui), ui.active_colors(), 100));
    assert!(full.contains("not a git repository") && full.contains("total 16"));
    assert!(full.contains("cd /d/Kroma; ls -la; git status"));
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
    assert!(text.contains("git status"));
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
    assert!(text.contains("$ echo overwritten"));
    assert!(
        text.contains("config.toml"),
        "command side effects remain visible"
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
    let collapsed = card_text(&ui.tool_card(&entries(&ui), ui.active_colors(), 100));
    assert!(collapsed.contains("$(cat version)"));
    ui.toggle_all_tools();
    let entries = entries(&ui);
    let substitution = card_text(&ui.tool_card(&[entries[0]], ui.active_colors(), 100));
    assert!(substitution.contains("echo $(cat version) && cargo test"));
    let literal = card_text(&ui.tool_card(&[entries[1]], ui.active_colors(), 100));
    assert!(literal.contains("echo '===' && cargo test"));
}

#[test]
fn successful_rows_have_disclosure_and_keep_right_padding() {
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
    assert!(text.contains('▸') && text.contains("$ pwd"));
    assert!(text.contains("Cargo.toml") && text.contains("git status"));
    for line in &card.lines {
        assert!(line.width() <= width - 2);
    }
}

#[test]
fn subagent_live_and_completed_records_render_clean_dashboard_not_raw_json() {
    let mut ui = ui();
    let raw_args = serde_json::json!({
        "tasks": [
            {"task": "检查 src/session.rs 恢复逻辑"},
            {"task": "运行集成测试", "cwd": "../kroma-tests"}
        ]
    })
    .to_string();
    ui.event(UiEvent::ToolStarted {
        name: "subagent".into(),
        id: "sub-1".into(),
        args: tool_arguments("subagent", &raw_args),
    });
    ui.event(UiEvent::ToolOutput {
        id: "sub-1".into(),
        stream: "stdout".into(),
        chunk: concat!(
            "▷ [1/2] 检查 src/session.rs 恢复逻辑
",
            "▷ [2/2] 运行集成测试
",
            "1▶ 2▶ · 5s/600s · in 0 / out 0
",
            "[1/2] 检查 src/session.rs… 0:05 ▶ model turn 1 · 0 tok
",
            "[2/2] 运行集成测试 0:05 ▶ starting · 0 tok
",
            "✓ [1/2]   8.2s · in 3.1k / out 90 · \"恢复逻辑正常\"
",
            "1✓ 2▶ · 10s/600s · in 3.1k / out 90
",
            "[2/2] 运行集成测试 0:10 ▶ shell 0:04 · 1.2k tok
"
        )
        .into(),
    });
    let live_text = card_text(&ui.tool_card(&entries(&ui), ui.active_colors(), 120));
    assert!(
        !live_text.contains("{\"tasks\""),
        "header must not dump raw JSON: {live_text}"
    );
    assert!(live_text.contains("subagent") && live_text.contains("Running"));
    assert!(live_text.contains("1✓ 2▶") && live_text.contains("src/session.rs"));
    ui.toggle_all_tools();
    let live_full = card_text(&ui.tool_card(&entries(&ui), ui.active_colors(), 120));
    assert!(live_full.contains("检查 src/session.rs") && live_full.contains("运行集成测试"));
    assert!(live_full.contains("1✓ 2▶ · 10s/600s"));
    ui.toggle_all_tools();

    let raw_result = serde_json::json!({
        "schema_version": 1,
        "run_id": "20260405-abcd1234",
        "status": "partial",
        "results": [
            {
                "index": 0,
                "status": "completed",
                "exit_code": 0,
                "output": "恢复逻辑正常
    WAL 顺序已验证",
                "output_truncated": false,
                "duration_ms": 8200,
                "usage": {"requests": 1, "input": 3100, "output": 90, "cached": 0, "reasoning": 0},
                "artifacts": "/tmp/runs/0"
            },
            {
                "index": 1,
                "status": "failed",
                "exit_code": null,
                "output": "已保留的片段",
                "output_truncated": false,
                "partial": true,
                "error": {"code": "CHILD_TIMEOUT", "message": "child timed out after 300s"},
                "duration_ms": 300000,
                "usage": {"requests": 1, "input": 1200, "output": 20, "cached": 0, "reasoning": 0},
                "artifacts": "/tmp/runs/1"
            }
        ]
    })
    .to_string();
    ui.event(UiEvent::ToolFinished {
        id: "sub-1".into(),
        result: tool_result_details(&raw_result),
        failed: true,
    });
    let collapsed = card_text(&ui.tool_card(&entries(&ui), ui.active_colors(), 120));
    assert!(!collapsed.contains("\"schema_version\"") && !collapsed.contains("\"status\":"));
    assert!(collapsed.contains("Failed"));
    assert!(collapsed.contains("CHILD_TIMEOUT"));

    ui.toggle_all_tools();
    let expanded = card_text(&ui.tool_card(&entries(&ui), ui.active_colors(), 120));
    assert!(expanded.contains("[1/2] 检查 src/session.rs 恢复逻辑"));
    assert!(expanded.contains("[2/2] 运行集成测试 (../kroma-tests)"));
    assert!(expanded.contains("── [1/2] ✓ completed · 8.2s"));
    assert!(expanded.contains("WAL 顺序已验证"));
    assert!(expanded.contains("── [2/2] ✗ failed (CHILD_TIMEOUT) · 300.0s"));
    assert!(expanded.contains("partial answer:"));
    assert!(expanded.contains("已保留的片段"));
    assert!(expanded.contains("artifacts: /tmp/runs/1"));
}
