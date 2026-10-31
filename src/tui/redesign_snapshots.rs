//! Golden text snapshots complement colour/hit-testing assertions in redesign_tests.
use super::*;
use ratatui::{backend::TestBackend, Terminal};

fn fixture(scene: &str, preset: &str) -> UiState {
    let mut themes = ThemeStore::load_at(None).unwrap();
    themes.select(preset).unwrap();
    let mut ui = UiState::new("test-model".into(), "snapshot".into(), themes, &[]);
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
            .map(|(title, age)| {
                (
                    title.into(),
                    Some(std::time::SystemTime::now() - Duration::from_secs(age)),
                )
            })
            .collect();
        }
        return ui;
    }
    ui.add_user("Review the change. 请检查代码。\nPreserve Unicode and indentation.");
    if scene.starts_with("tools-") || scene == "single-tool" || scene == "failure" {
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
    if scene == "panel" {
        ui.open_plugin_panel(
            "Review\n\nA scrollable plugin panel.\nThe draft stays editable after closing.".into(),
        );
    }
    ui
}

#[test]
fn calm_transcript_golden_matrix() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("reviews/tui-redesign/final");
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
                    std::fs::read_to_string(&path).unwrap(),
                    actual,
                    "snapshot {name}"
                );
            }
        }
    }
}
