//! The default Plain style is exercised through the real renderer, not HTML mocks.
use super::*;
use ratatui::{backend::TestBackend, buffer::Buffer, Terminal};

fn ui() -> UiState {
    let mut ui = UiState::new_default(
        "model-x".into(),
        "plain-test".into(),
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
fn text(buffer: &Buffer) -> String {
    buffer
        .content()
        .chunks(usize::from(buffer.area.width).max(1))
        .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}
fn tool(ui: &mut UiState, id: &str, result: &str, failed: bool) {
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
fn measured(ui: &mut UiState) {
    ui.event(UiEvent::Usage {
        input: Some(12_000),
        output: Some(300),
        cached: Some(9_600),
        reasoning: None,
    });
    ui.event(UiEvent::ActualInput(12_000));
}
fn neutral(color: Color) -> bool {
    matches!(color, Color::Reset) || matches!(color, Color::Rgb(r,g,b) if r==g && g==b)
}

#[test]
fn plain_is_the_default_host_style_without_mutating_saved_theme_or_preferences() {
    let themes = ThemeStore::load_at(None).unwrap();
    let colors = themes.colors;
    let mut ui = UiState::new_default("model".into(), "test".into(), themes, &[]);
    assert!(ui.plain_style);
    let rendered = text(&draw(&mut ui, 80, 24));
    assert!(!rendered.contains("Kroma /"));
    assert!(rendered.contains("Cache —"));
    assert_eq!(ui.themes.colors, colors);
    assert_eq!(
        ui.preferences,
        super::super::preferences::Preferences::default()
    );
    assert!(ui.preferences_path.is_none());
}

#[test]
fn default_tui_entry_has_no_preview_environment_gate() {
    let host = include_str!("mod.rs");
    assert!(host.contains("let mut state = UiState::new_default("));
    assert!(!host.contains("KROMA_TUI_STYLE"));
}

#[test]
fn plain_keeps_code_and_overlays_neutral_except_semantic_diff_markers() {
    for preset in ["dark", "light", "mono"] {
        let mut ui = ui();
        ui.themes.select(preset).unwrap();
        ui.set_plugin_presentation(
            vec!["COLOURFUL".into()],
            BTreeMap::new(),
            None,
            BTreeMap::from([("accent".into(), "#ff0000".into())]),
        );
        ui.add_user("Check Unicode 中文🙂");
        ui.event(UiEvent::Text(
            "```rust\n// comment\nlet value: usize = 42;\n```\n\n```diff\n-old\n+new\n```".into(),
        ));
        ui.finish_task();
        for panel in [false, true] {
            if panel {
                ui.open_plugin_panel("Details\n\nPlain overlay".into());
            }
            let buffer = draw(&mut ui, 100, 35);
            for (index, cell) in buffer.content().iter().enumerate() {
                let messages = ui.message_colors();
                let role_rail = index % usize::from(buffer.area.width)
                    == usize::from(ui.selection_area.x + 1)
                    && cell.symbol() == " "
                    && (cell.bg == messages.user_rail || cell.bg == messages.assistant_rail);
                if !neutral(cell.fg) {
                    let palette = ui.semantic_colors();
                    let diff = (cell.symbol() == "+" && cell.fg == palette.add)
                        || (cell.symbol() == "-" && cell.fg == palette.remove);
                    let role_label =
                        cell.fg == messages.user_rail || cell.fg == messages.assistant_rail;
                    assert!(
                        diff || role_rail || role_label,
                        "non-semantic text got colour: {}",
                        cell.symbol()
                    );
                }
                assert!(
                    neutral(cell.bg) || role_rail,
                    "{preset} background {:?}",
                    cell.bg
                );
            }
        }
    }
}

#[test]
fn plain_uses_the_full_workspace_for_prose_and_code() {
    let mut ui = ui();
    ui.add_user("Review the code");
    ui.event(UiEvent::Text(format!(
        "{}\n\n```text\n{}\n```",
        "readable prose ".repeat(60),
        "z".repeat(130)
    )));
    draw(&mut ui, 160, 40);
    assert!(ui.selection_area.width > 140);
    let cache = ui.history_cache.as_ref().unwrap();
    let prose = cache
        .text
        .lines
        .iter()
        .filter(|line| line.to_string().contains("readable"));
    assert!(prose.clone().count() > 1);
    let widest = prose.map(Line::width).max().unwrap_or(0);
    assert!(
        widest > 120,
        "prose must fill the available width, got {widest}"
    );
    assert!(cache
        .text
        .lines
        .iter()
        .any(|line| line.to_string().contains(&"z".repeat(100))));
}

#[test]
fn plain_layout_setting_keeps_the_full_width_reading_area() {
    let mut ui = ui();
    ui.add_user("Review the paragraph");
    ui.event(UiEvent::Text("readable prose ".repeat(60)));
    draw(&mut ui, 160, 40);
    let area = ui.selection_area;
    let comfortable = ui
        .history_cache
        .as_ref()
        .unwrap()
        .text
        .lines
        .iter()
        .filter(|line| line.to_string().contains("readable"))
        .map(Line::width)
        .max()
        .unwrap();
    assert!(comfortable > 120);
    ui.apply_preferences(super::super::preferences::Preferences {
        full_width: true,
        ..ui.preferences
    });
    draw(&mut ui, 160, 40);
    assert_eq!(ui.selection_area, area);
    let wide = ui
        .history_cache
        .as_ref()
        .unwrap()
        .text
        .lines
        .iter()
        .filter(|line| line.to_string().contains("readable"))
        .map(Line::width)
        .max()
        .unwrap();
    assert!(wide > 88);
}

#[test]
fn plain_composer_has_one_divider_and_no_raised_surface_or_empty_placeholder() {
    let mut ui = ui();
    ui.add_user("hello");
    let buffer = draw(&mut ui, 80, 24);
    let rendered = text(&buffer);
    assert!(
        !rendered.contains("Describe a task") && !rendered.contains("Enter queues the next task")
    );
    assert!(!rendered.contains('┌') && !rendered.contains('└'));
    assert!(rendered.contains('─') && rendered.contains('›'));
    assert_eq!(ui.editor_area.height, 1);
    // The caret is a software block on the first cell; the rest of the
    // composer keeps the terminal background.
    assert_eq!(
        buffer
            .cell((ui.editor_area.x, ui.editor_area.y))
            .unwrap()
            .bg,
        ui.active_colors().text
    );
    assert_eq!(
        buffer
            .cell((ui.editor_area.right() - 1, ui.editor_area.y))
            .unwrap()
            .bg,
        ui.active_colors().background
    );
    assert_eq!(ui.editor_area.y, 21);
    ui.running = true;
    assert!(!text(&draw(&mut ui, 80, 24)).contains("Enter queues the next task"));
}

#[test]
fn plain_hardware_cursor_stays_in_the_real_unicode_editor_geometry() {
    for draft in [
        "",
        "hello",
        "中文🙂 e\u{301}",
        "第一行\n第二行",
        &"wrapped text ".repeat(30),
    ] {
        let mut ui = ui();
        ui.editor.paste(draft);
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| ui.draw(frame)).unwrap();
        let area = ui.editor_area;
        let (_, x, y) = ui.editor.viewport(area.width, area.height);
        let cell = terminal
            .backend()
            .buffer()
            .cell((
                area.x + x.min(area.width - 1),
                area.y + y.min(area.height - 1),
            ))
            .unwrap();
        assert_eq!(cell.bg, ui.active_colors().text);
        assert!(area.y + y.min(area.height - 1) < 23);
        assert!(area.height <= 8);
    }
}

#[test]
fn plain_labels_are_decoration_and_do_not_pollute_copy_or_link_targets() {
    let mut ui = ui();
    ui.add_user("hello 中文");
    ui.event(UiEvent::Text(
        "See https://example.org/plain\n\n```rust\nlet value = 42;\n```".into(),
    ));
    let rendered = text(&draw(&mut ui, 100, 35));
    assert!(rendered.contains("YOU") && rendered.contains("KROMA"));
    assert!(ui
        .visible_links
        .iter()
        .any(|link| link.target == "https://example.org/plain"));
    let cache = ui.history_cache.as_ref().unwrap();
    ui.selection_start = Some((0, 0));
    ui.selection_end = Some((cache.text.lines.len() - 1, 99));
    let copied = ui.selected_text().unwrap();
    assert!(copied.contains("hello") && copied.contains("let value"));
    assert!(!copied.contains("YOU") && !copied.contains("KROMA"));
}

#[test]
fn plain_footer_preserves_measurements_unknowns_estimates_and_foreground_scope() {
    let mut ui = ui();
    let line = ui.footer_line(ui.active_colors(), 100).to_string();
    assert!(line.contains("—/500k") && line.contains("Cache —"));
    assert!(!line.contains("Cache 0%"));
    measured(&mut ui);
    let expected = ui.footer_line(ui.active_colors(), 100).to_string();
    assert!(expected.contains("12k/500k") && expected.contains("Cache 80%"));
    ui.event(UiEvent::TitleUsage {
        session_id: ui.session.clone(),
        requests: 1,
        input: Some(100),
        output: Some(50),
        cached: Some(0),
        reasoning: None,
    });
    ui.event(UiEvent::ApecUsage {
        requests: 1,
        input: Some(100),
        output: Some(50),
        cached: Some(0),
    });
    assert_eq!(
        ui.footer_line(ui.active_colors(), 100).to_string(),
        expected
    );
    ui.reset_context();
    ui.event(UiEvent::Context { estimated: 12_000 });
    assert!(ui
        .footer_line(ui.active_colors(), 100)
        .to_string()
        .contains("~12k/500k"));
    ui.event(UiEvent::Usage {
        input: Some(100),
        output: None,
        cached: Some(0),
        reasoning: None,
    });
    assert!(ui
        .footer_line(ui.active_colors(), 100)
        .to_string()
        .contains("Cache 0%"));
    ui.event(UiEvent::Usage {
        input: None,
        output: None,
        cached: None,
        reasoning: None,
    });
    assert!(ui
        .footer_line(ui.active_colors(), 100)
        .to_string()
        .contains("Cache —"));
}

#[test]
fn plain_footer_fits_long_names_without_overlapping_groups() {
    let mut ui = ui();
    ui.model = "provider/extremely-long-model-profile-with-a-version".into();
    measured(&mut ui);
    for width in 1..180 {
        let line = ui.footer_line(ui.active_colors(), width);
        assert!(line.width() <= width, "footer width {width}: {}", line);
        if width >= 40 {
            let text = line.to_string();
            assert!(text.contains("high") && text.contains("12k/500k") && text.contains("80%"));
        }
    }
}

#[test]
fn plain_keeps_each_tool_visible_with_live_output_and_failure_reasons() {
    let mut ui = ui();
    ui.add_user("Check tool steps");
    tool(&mut ui, "quiet-one", "exit: 0\nok", false);
    tool(&mut ui, "quiet-two", "exit: 0\nok", false);
    ui.event(UiEvent::ToolStarted {
        name: "shell".into(),
        id: "live".into(),
        args: "cargo test".into(),
    });
    ui.event(UiEvent::ToolOutput {
        id: "live".into(),
        stream: "stdout".into(),
        chunk: "test resume_empty ... ok".into(),
    });
    let running = ui.history_text(ui.active_colors(), 100).to_string();
    assert!(running.contains("resume_empty"));
    assert!(running.contains("echo quiet-one") && running.contains("echo quiet-two"));
    assert!(!running.contains("Another") && !running.contains("3 steps"));
    ui.event(UiEvent::ToolFinished {
        id: "live".into(),
        result: "exit: 101\nstderr: empty checkpoint".into(),
        failed: true,
    });
    let failed = ui.history_text(ui.active_colors(), 100).to_string();
    assert!(failed.contains("empty checkpoint"));
    ui.toggle_all_tools();
    assert!(ui
        .history_text(ui.active_colors(), 100)
        .to_string()
        .contains("echo quiet-one"));
}

#[test]
fn plain_history_remains_pinned_while_streaming_and_survives_resize() {
    let mut ui = ui();
    ui.add_user("Long answer");
    ui.event(UiEvent::Text(
        (0..150)
            .map(|i| format!("line-{i:03} stable content\n"))
            .collect(),
    ));
    draw(&mut ui, 80, 24);
    ui.scroll_anchor = Some(3);
    draw(&mut ui, 80, 24);
    let anchor = ui.scroll_anchor;
    let top = ui.selection_rows[0].clone();
    ui.event(UiEvent::Text("new streaming output\n".into()));
    draw(&mut ui, 80, 24);
    assert_eq!(ui.scroll_anchor, anchor);
    assert_eq!(ui.selection_rows[0], top);
    draw(&mut ui, 100, 30);
    assert!(ui.scroll_anchor.is_some());
    ui.scroll_to_bottom();
    draw(&mut ui, 100, 30);
    assert!(ui.scroll_anchor.is_none());
}

#[test]
fn plain_panels_and_recent_session_hit_regions_keep_drafts_intact() {
    let mut ui = ui();
    ui.enter_home();
    ui.recent_sessions.push(SessionEntry {
        id: "real-session".into(),
        name: Some("Review changes".into()),
        modified: None,
    });
    draw(&mut ui, 80, 24);
    let hit = ui.recent_session_rows[0].0;
    assert_eq!(ui.recent_session_at(hit.x, hit.y), Some("real-session"));
    ui.add_user("Inspect");
    tool(&mut ui, "record", "exit: 0\nverified", false);
    ui.editor.paste("unfinished 中文 draft");
    ui.open_command_completion("/".into());
    draw(&mut ui, 80, 24);
    ui.picker = None;
    ui.inspect_tool("record");
    draw(&mut ui, 80, 24);
    ui.close_plugin_panel();
    assert_eq!(ui.editor.text(), "unfinished 中文 draft");
}

#[test]
fn plain_activity_keeps_stall_and_cancellation_feedback_without_visual_noise() {
    let mut ui = ui();
    ui.set_plugin_presentation(vec!["NOISY".into()], BTreeMap::new(), None, BTreeMap::new());
    ui.running = true;
    ui.event(UiEvent::ToolStarted {
        name: "shell".into(),
        id: "live".into(),
        args: "cargo test".into(),
    });
    let active = ui.activity_line(ui.active_colors()).to_string();
    assert!(active.contains("Running"));
    assert!(!active.contains("Run Rust tests"));
    assert!(!active.contains("NOISY") && !active.contains('▲'));
    ui.last_output = Instant::now() - Duration::from_secs(9);
    assert!(ui
        .activity_line(ui.active_colors())
        .to_string()
        .contains("Ctrl+C"));
    ui.finish_task();
    ui.task_outcome = Some(TaskOutcome::Cancelled);
    let cancelled = ui.activity_line(ui.active_colors()).to_string();
    assert!(cancelled.contains('⊘') && cancelled.contains("side effects are not undone"));
}

#[test]
fn plain_handles_small_and_initially_zero_sized_terminals() {
    for (width, height) in [(0, 0), (1, 1), (19, 7), (20, 8), (40, 10), (80, 16)] {
        let mut ui = ui();
        ui.editor.paste(&"multi line\n".repeat(30));
        draw(&mut ui, width, height);
        if width >= 20 && height >= 8 {
            assert!(ui.editor_area.right() <= width && ui.editor_area.bottom() <= height);
        }
    }
}

fn fixture(scene: &str) -> UiState {
    let mut ui = ui();
    ui.preferences.language = super::super::preferences::Language::Zh;
    if scene == "home" {
        ui.enter_home();
        return ui;
    }
    if scene == "connecting" {
        ui.add_user("你好，先看一下项目结构。");
        ui.event(UiEvent::ModelStarted);
        ui.running = true;
        ui.phase = "waiting";
        ui.received_model_output = false;
        ui.phase_since = Some(std::time::Instant::now() - Duration::from_secs(3));
        measured(&mut ui);
        return ui;
    }
    if scene == "conversation" {
        ui.preferences.language = super::super::preferences::Language::En;
        ui.add_user("你好");
        ui.event(UiEvent::Text("你好！我是 Kroma，可以帮你处理代码相关的工作，例如：\n\n- 阅读、理解、修改项目代码\n- 调试报错、修复 bug\n- 编写补充测试\n- 代码审查与重构建议\n\n当前工作目录是 `D:/Kroma`。有什么想做的吗？".into()));
        ui.finish_task();
        ui.task_outcome = Some(TaskOutcome::Completed);
        measured(&mut ui);
        return ui;
    }
    ui.add_user("检查会话恢复流程，修复空检查点边界，并补充回归测试。");
    ui.event(UiEvent::Text(
        "我会先确认恢复路径与测试范围，再做最小修改。".into(),
    ));
    tool(&mut ui, "read", "exit: 0\nverified", false);
    tool(&mut ui, "search", "exit: 0\n3 matching call sites", false);
    ui.event(UiEvent::ToolStarted {
        name: "shell".into(),
        id: "tests".into(),
        args: "cargo test --lib session::tests".into(),
    });
    if scene == "running" {
        ui.running = true;
        ui.event(UiEvent::ToolOutput {
            id: "tests".into(),
            stream: "stdout".into(),
            chunk: "test resume_empty ... ok".into(),
        });
    } else {
        let failed = scene == "failure";
        ui.event(UiEvent::ToolFinished {
            id: "tests".into(),
            result: if failed {
                "exit: 101\nstderr: assertion failed: empty checkpoint"
            } else if scene == "warning" {
                "exit: 0\nstderr: warning: verify checkpoint compatibility"
            } else {
                "exit: 0\n12 passed; 0 failed"
            }
            .into(),
            failed,
        });
        ui.finish_task();
        ui.task_outcome = Some(if failed {
            TaskOutcome::Failed
        } else {
            TaskOutcome::Completed
        });
        ui.task_elapsed = Some(Duration::from_millis(8400));
        ui.event(UiEvent::Text(
            if failed {
                "空检查点分支仍有异常，当前验证未通过。"
            } else if scene == "warning" {
                "验证已结束，但仍需复核检查点兼容性提示。"
            } else {
                "已修复恢复边界，并补充回归测试。"
            }
            .into(),
        ));
    }
    if scene == "code" {
        ui.event(UiEvent::Text("\n\n```rust\nfn restore(checkpoint: Option<Snapshot>) {\n    let state = checkpoint.unwrap_or_default();\n    replay_after(state);\n}\n```".into()));
    }
    if scene == "diff" {
        ui.event(UiEvent::Text("\n\n```diff\ndiff --git a/session.rs b/session.rs\n--- a/session.rs\n+++ b/session.rs\n@@ -1 +1 @@\n-let state = checkpoint.unwrap();\n+let state = checkpoint.unwrap_or_default();\n```".into()));
    }
    for entry in &mut ui.entries {
        if entry.duration.is_some() {
            entry.duration = Some(Duration::from_millis(1200));
        }
    }
    if ui.running {
        ui.phase_since = Some(Instant::now() - Duration::from_secs(3));
    }
    measured(&mut ui);
    if scene == "failure" {
        ui.event(UiEvent::ActualInput(475_000));
    }
    if scene == "warning" {
        ui.event(UiEvent::ActualInput(390_000));
    }
    ui.editor
        .paste("再检查一下取消操作是否会影响已完成的修改。");
    if scene == "picker" {
        ui.open_command_completion("/".into());
    }
    if scene == "inspector" {
        ui.inspect_tool("tests");
    }
    ui
}

#[test]
#[ignore = "Writes offline screenshots of the real renderer; no model or credentials needed"]
fn export_plain_preview() {
    use std::fmt::Write;
    fn escape(text: &str) -> String {
        text.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
    }
    fn rgb(color: Color, fallback: u8) -> (u8, u8, u8) {
        match color {
            Color::Rgb(r, g, b) => (r, g, b),
            _ => (fallback, fallback, fallback),
        }
    }
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("reviews/tui-plain-runtime");
    std::fs::create_dir_all(&root).unwrap();
    let mut html = String::from("<!doctype html><html lang=zh><meta charset=utf-8><title>Kroma A / Real renderer</title><style>body{background:#111;color:#ddd;font:14px system-ui;margin:30px}h1{font-weight:500}p,summary{color:#aaa}summary{cursor:pointer;padding:12px 0}.frame{overflow:auto}pre{font:13px/1.5 'Cascadia Mono',Consolas,'Microsoft YaHei',monospace;white-space:pre;margin:0;width:max-content}span{display:inline-block}.rail-user,.rail-assistant{position:relative;color:transparent!important}.rail-user::before,.rail-assistant::before{content:'';position:absolute;top:0;bottom:0;background:var(--rail)}.rail-user::before{left:0;width:100%}.rail-assistant::before{left:0;width:100%}details{margin:20px 0;border-top:1px solid #444}</style><h1>A · 留白 / 消息背景与角色竖线（S2）</h1><p>由真实 UiState + TestBackend 离线生成，不是网页重画布局。未调用模型。展开查看各状态；ANSI 文件可在对应尺寸的真彩终端重放。</p>");
    for preset in ["dark", "light"] {
        for (width, height) in [(120, 36), (80, 24)] {
            for scene in [
                "conversation",
                "connecting",
                "short",
                "home",
                "running",
                "failure",
                "warning",
                "diff",
                "code",
                "picker",
                "inspector",
            ] {
                let mut ui = fixture(scene);
                ui.themes.select(preset).unwrap();
                let buffer = draw(&mut ui, width, height);
                let name = format!("{preset}-{scene}-{width}x{height}");
                write!(
                    html,
                    "<details{}><summary>{name}</summary><div class=frame><pre>",
                    if preset == "dark" && width == 120 && scene == "conversation" {
                        " open"
                    } else {
                        ""
                    }
                )
                .unwrap();
                let mut ansi = String::from("\x1b[0m\x1b[2J\x1b[H\x1b[?25l");
                let mut plain = String::new();
                for y in 0..height {
                    let mut x = 0;
                    while x < width {
                        let cell = buffer.cell((x, y)).unwrap();
                        let columns = UnicodeWidthStr::width(cell.symbol())
                            .max(1)
                            .min(usize::from(width - x));
                        let (r, g, b) = rgb(cell.fg, 229);
                        let (br, bg, bb) = rgb(cell.bg, 13);
                        let bold = cell.modifier.contains(Modifier::BOLD);
                        let messages = ui.message_colors();
                        let rail_class = if x == ui.selection_area.x + 1
                            && cell.symbol() == " "
                            && cell.bg == messages.user_rail
                        {
                            "rail-user"
                        } else if x == ui.selection_area.x + 1
                            && cell.symbol() == " "
                            && cell.bg == messages.assistant_rail
                        {
                            "rail-assistant"
                        } else {
                            ""
                        };
                        write!(html,"<span class=\"{rail_class}\" style=\"width:{columns}ch;--rail:#{br:02x}{bg:02x}{bb:02x};color:#{r:02x}{g:02x}{b:02x};background:#{br:02x}{bg:02x}{bb:02x};font-weight:{}\">{}</span>",if bold {700}else{400},escape(cell.symbol())).unwrap();
                        write!(
                            ansi,
                            "\x1b[{};{}H\x1b[0;38;2;{r};{g};{b};48;2;{br};{bg};{bb}{}m{}",
                            y + 1,
                            x + 1,
                            if bold { ";1" } else { "" },
                            cell.symbol()
                        )
                        .unwrap();
                        plain.push_str(cell.symbol());
                        x += columns as u16;
                    }
                    html.push('\n');
                    plain.push('\n');
                }
                html.push_str("</pre></div></details>");
                ansi.push_str("\x1b[0m\x1b[?25h");
                std::fs::write(root.join(format!("{name}.ansi")), ansi).unwrap();
                std::fs::write(root.join(format!("{name}.txt")), plain).unwrap();
            }
        }
    }
    html.push_str("</html>");
    std::fs::write(root.join("index.html"), html).unwrap();
    crate::cli_println!(
        "Plain renderer gallery: {}",
        root.join("index.html").display()
    );
}

fn relative_luminance(color: Color) -> f64 {
    let Color::Rgb(r, g, b) = color else {
        panic!("contrast requires an RGB colour, got {color:?}");
    };
    let linear = |value: u8| {
        let value = f64::from(value) / 255.0;
        if value <= 0.04045 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * linear(r) + 0.7152 * linear(g) + 0.0722 * linear(b)
}

fn contrast(a: Color, b: Color) -> f64 {
    let (a, b) = (relative_luminance(a), relative_luminance(b));
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

#[test]
fn plain_palette_is_the_single_source_and_keeps_message_contrast() {
    for preset in ["dark", "light"] {
        let mut ui = ui();
        ui.themes.select(preset).unwrap();
        let palette = ui.plain_palette();
        assert_eq!(ui.plain_colors(), palette.theme);
        assert_eq!(ui.message_colors(), palette.message);
        assert_eq!(ui.semantic_colors(), palette.semantic);
        let background = palette.message.surface;
        for (name, ink) in [
            ("text", palette.theme.text),
            ("muted", palette.theme.muted),
            ("faint", palette.theme.faint),
            ("user rail", palette.message.user_rail),
            ("assistant rail", palette.message.assistant_rail),
            ("running", palette.semantic.running),
            ("success", palette.semantic.success),
            ("warning", palette.semantic.warning),
            ("error", palette.semantic.error),
        ] {
            assert!(
                contrast(ink, background) >= 4.5,
                "{preset} {name} on message surface: {:.2}",
                contrast(ink, background)
            );
        }
    }
}

#[test]
fn plain_mono_palette_defers_every_colour_to_the_terminal() {
    let mut ui = ui();
    ui.themes.select("mono").unwrap();
    let palette = ui.plain_palette();
    for color in [
        palette.theme.background,
        palette.theme.surface,
        palette.theme.text,
        palette.theme.muted,
        palette.theme.border,
        palette.message.surface,
        palette.message.user_rail,
        palette.message.assistant_rail,
        palette.semantic.running,
        palette.semantic.success,
        palette.semantic.error,
    ] {
        assert_eq!(color, Color::Reset);
    }
}
