//! Golden text snapshots complement colour/hit-testing assertions in redesign_tests.
use super::*;
use ratatui::{backend::TestBackend, Terminal};

fn fixture(scene: &str, preset: &str) -> UiState {
    let mut themes = ThemeStore::load_at(None).unwrap();
    themes.select(preset).unwrap();
    // Golden snapshots must reflect the *actual* interactive default,
    // not the historical low-level renderer used by old regression tests.
    let mut ui = UiState::new_default("test-model".into(), "snapshot".into(), themes, &[]);
    ui.set_link_workdir(Path::new("D:/fixture/project"));
    ui.configure_status(500_000, Some("high".into()));
    if scene.starts_with("home") {
        ui.enter_home();
        if scene == "home-recent" {
            ui.recent_sessions = [
                ("Fix Unicode", 3600),
                ("Review tool output", 7200),
                ("Add tests", 172800),
            ]
            .into_iter()
            .map(|(title, age)| SessionEntry {
                id: format!("session-{age}"),
                name: Some(title.into()),
                modified: Some(std::time::SystemTime::now() - Duration::from_secs(age)),
            })
            .collect();
        }
        return ui;
    }
    ui.add_user("Review the change. 请检查代码。\nPreserve Unicode and indentation.");
    if scene.starts_with("tools-")
        || scene == "single-tool"
        || scene == "failure"
        || scene == "inspector"
    {
        let count = if scene == "single-tool" { 1 } else { 3 };
        for index in 0..count {
            let id = format!("call-{index}");
            ui.event(UiEvent::ToolStarted {
                name: "shell".into(),
                id: id.clone(),
                args: if index == 1 {
                    "cargo test".into()
                } else {
                    format!("echo step-{index}")
                },
            });
            if scene == "tools-running" && index == 2 {
                ui.event(UiEvent::ToolOutput {
                    id,
                    stream: "stdout".into(),
                    chunk: "test unicode_copy ... ok".into(),
                });
                continue;
            }
            let failed = (scene == "failure" || scene == "tools-failed") && index == 1;
            let interrupted = scene == "tools-interrupted" && index == 1;
            ui.event(UiEvent::ToolFinished{id,result:if interrupted{"Session interrupted. Tool result is unknown; do not assume the command succeeded or automatically repeat it.".into()}else if failed{"exit: 101\nstderr: error[E0599]: method not found".into()}else{"exit: 0\nverified".into()},failed:failed||interrupted});
        }
    } else if scene.starts_with("reasoning-") {
        ui.event(UiEvent::ReasoningFinal(
            "Inspect the existing API, then verify the change.\n\nKeep failures visible.".into(),
        ));
        ui.reasoning_expanded = scene == "reasoning-expanded";
    } else if scene == "code" {
        ui.event(UiEvent::Text("```rust\n// preserve source text\nfn greet(name: &str) {\n\tprintln!(\"Hello {name}\");\n    let count: usize = 42;\n}\n```".into()));
    } else if scene == "diff" {
        ui.event(UiEvent::Text(
            "```diff\ndiff --git a/main.rs b/main.rs\n@@ -1 +1 @@\n-old_value\n+new_value\n```"
                .into(),
        ));
    } else {
        ui.event(UiEvent::Text("Updated the implementation.\n\n- Preserved input and links\n- Verified Unicode selection".into()));
    }
    if scene == "tools-running" || scene == "running" {
        ui.running = true;
        if scene == "running" {
            ui.event(UiEvent::ModelStarted);
        }
    } else {
        ui.finish_task();
        ui.task_outcome = Some(if scene == "failure" {
            TaskOutcome::Failed
        } else if scene == "cancelled" {
            TaskOutcome::Cancelled
        } else {
            TaskOutcome::Completed
        });
    }
    ui.task_elapsed = Some(Duration::from_millis(35300));
    for entry in &mut ui.entries {
        if entry.duration.is_some() {
            entry.duration = Some(Duration::from_millis(1200));
        }
    }
    if scene == "cancelled" {
        ui.entries.push(Entry::new(
            Kind::Assistant,
            "Kroma",
            format!(
                "{} Incomplete commands may have had side effects.]",
                crate::agent::TASK_CANCELLED_PREFIX
            ),
        ));
    }
    if scene == "context75" {
        ui.event(UiEvent::ActualInput(375_000));
    }
    if scene == "context95" {
        ui.event(UiEvent::ActualInput(475_000));
    }
    if scene == "picker" {
        ui.open_command_completion("/".into());
    }
    if scene == "inspector" {
        ui.editor.paste("Unsent draft 中文");
        ui.inspect_tool("call-1");
    }
    if scene == "multi-turn" {
        for index in 1..=2 {
            ui.add_user(&format!("Follow-up {index} · 保留上下文"));
            ui.event(UiEvent::Text(format!(
                "Response {index} with a focused explanation."
            )));
        }
        ui.finish_task();
        ui.task_outcome = Some(TaskOutcome::Completed);
    }
    for (name, kind, label, value) in [
        (
            "picker-models",
            super::super::picker::Kind::Models,
            "Fast · test-model",
            "/model fast",
        ),
        (
            "picker-sessions",
            super::super::picker::Kind::Sessions,
            "Fix Unicode",
            "/resume real-session",
        ),
        (
            "picker-files",
            super::super::picker::Kind::Files,
            "src/tui/view.rs",
            "@src/tui/view.rs",
        ),
    ] {
        if scene == name {
            ui.editor.paste("Unsent draft");
            ui.picker = Some(Picker::new(
                kind,
                vec![super::super::picker::Item::new(label, value, "fixture")],
                "",
            ));
        }
    }
    if scene == "panel" {
        ui.open_plugin_panel(
            "Review\n\nA scrollable plugin panel.\nThe draft stays editable after closing.".into(),
        );
    }
    ui
}

#[test]
#[ignore = "Legacy golden images encode pre-phase message layout; regenerate after the card redesign"]
fn calm_transcript_golden_matrix() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("reviews/tui-graphite/final");
    let update = std::env::var_os("KROMA_UPDATE_SNAPSHOTS").is_some();
    if update {
        std::fs::create_dir_all(&root).unwrap();
    }
    for preset in ["dark", "light", "mono"] {
        for (width, height) in [(40, 16), (80, 24), (100, 30), (160, 40)] {
            for scene in [
                "home",
                "home-recent",
                "conversation",
                "failure",
                "cancelled",
                "single-tool",
                "tools-success",
                "tools-failed",
                "tools-running",
                "tools-interrupted",
                "reasoning-collapsed",
                "reasoning-expanded",
                "code",
                "diff",
                "context75",
                "context95",
                "picker",
                "panel",
                "running",
                "idle",
                "multi-turn",
                "inspector",
                "picker-models",
                "picker-sessions",
                "picker-files",
            ] {
                let mut ui = fixture(scene, preset);
                let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                terminal.draw(|frame| ui.draw(frame)).unwrap();
                let actual = terminal
                    .backend()
                    .buffer()
                    .content()
                    .chunks(usize::from(width))
                    .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
                    .collect::<Vec<_>>()
                    .join("\n");
                let name = format!("{preset}-{scene}-{width}x{height}.txt");
                let path = root.join(&name);
                if update {
                    std::fs::write(&path, &actual).unwrap();
                }
                assert_eq!(
                    std::fs::read_to_string(&path)
                        .unwrap()
                        .replace("\r\n", "\n"),
                    actual,
                    "snapshot {name}"
                );
            }
        }
    }
}

#[test]
#[ignore = "Graphite visual artifact lane: run with --ignored --nocapture"]
fn export_graphite_visual_gallery() {
    use std::fmt::Write;
    fn rgb(color: Color, fallback: (u8, u8, u8)) -> (u8, u8, u8) {
        if let Color::Rgb(r, g, b) = color {
            (r, g, b)
        } else {
            fallback
        }
    }
    fn escape(text: &str) -> String {
        text.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
    }
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("reviews/tui-graphite/gallery");
    std::fs::create_dir_all(&root).unwrap();
    let mut html = String::from("<!doctype html><html lang=zh><meta charset=utf-8><title>Kroma Graphite</title><style>body{background:#101114;color:#e5e3df;font:15px system-ui;margin:32px}pre{font:13px/1.5 'Cascadia Mono',Consolas,monospace;white-space:pre;margin:0}summary{cursor:pointer;margin:20px 0 8px}.frame{overflow:auto;padding:12px;background:#17191e}span{display:inline-block}a{color:#9aaeff}</style><h1>Kroma Graphite</h1><p>TestBackend 彩色样板，不是实时终端截图；mono 的原生终端颜色在此使用深色模拟。ANSI 文件可在真彩色终端中重放。</p>");
    for preset in ["dark", "light", "mono"] {
        let scenes: &[&str] = if preset == "dark" {
            &[
                "home-recent",
                "multi-turn",
                "tools-failed",
                "running",
                "picker",
                "inspector",
            ]
        } else {
            &["home-recent", "picker", "inspector"]
        };
        for (width, height) in [(80, 24), (160, 40)] {
            for scene in scenes {
                let mut ui = fixture(scene, preset);
                let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                terminal.draw(|frame| ui.draw(frame)).unwrap();
                let name = format!("{preset}-{scene}-{width}x{height}");
                write!(
                    html,
                    "<details><summary>{name}</summary><div class=frame><pre>"
                )
                .unwrap();
                let mut ansi = String::from("\x1b[0m\x1b[2J\x1b[H\x1b[?25l");
                for y in 0..height {
                    let mut x = 0;
                    while x < width {
                        let cell = terminal.backend().buffer().cell((x, y)).unwrap();
                        let (r, g, b) = rgb(cell.fg, (229, 227, 223));
                        let (br, bg, bb) = rgb(cell.bg, (16, 17, 20));
                        let columns = UnicodeWidthStr::width(cell.symbol())
                            .max(1)
                            .min(usize::from(width - x));
                        let bold = cell.modifier.contains(Modifier::BOLD);
                        let italic = cell.modifier.contains(Modifier::ITALIC);
                        write!(html,"<span style=\"width:{columns}ch;color:#{r:02x}{g:02x}{b:02x};background:#{br:02x}{bg:02x}{bb:02x};font-weight:{};font-style:{}\">{}</span>",if bold {700} else {400},if italic {"italic"} else {"normal"},escape(cell.symbol())).unwrap();
                        write!(
                            ansi,
                            "\x1b[{};{}H\x1b[0;38;2;{r};{g};{b};48;2;{br};{bg};{bb}{}{}m{}",
                            y + 1,
                            x + 1,
                            if bold { ";1" } else { "" },
                            if italic { ";3" } else { "" },
                            cell.symbol()
                        )
                        .unwrap();
                        x += columns as u16;
                    }
                    html.push('\n');
                }
                html.push_str("</pre></div></details>");
                std::fs::write(root.join(format!("{name}.ansi")), ansi).unwrap();
            }
        }
    }
    html.push_str("</html>");
    std::fs::write(root.join("index.html"), html).unwrap();
    crate::cli_println!(
        "Graphite visual gallery: {}",
        root.join("index.html").display()
    );
}
