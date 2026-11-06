use super::*;
use crate::provider::ModelClient;
use ratatui::{backend::TestBackend, Terminal};

fn setup() -> (UiState, ModelClient) {
    (
        UiState::new(
            "test".into(),
            "test".into(),
            ThemeStore::load_at(None).unwrap(),
            &[],
        ),
        ModelClient::new("http://localhost/v1", None, "test".into()).unwrap(),
    )
}

fn key(ui: &mut UiState, model: &ModelClient, code: KeyCode, modifiers: KeyModifiers) {
    assert!(matches!(
        input_event(
            ui,
            Event::Key(KeyEvent::new(code, modifiers)),
            false,
            model,
            &std::env::temp_dir(),
        ),
        InputAction::None
    ));
}

#[test]
fn ctrl_o_restores_inline_tool_output_without_losing_the_draft() {
    fn screen(ui: &mut UiState) -> String {
        let mut terminal = Terminal::new(TestBackend::new(100, 32)).unwrap();
        terminal.draw(|frame| ui.draw(frame)).unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }
    let (_, model) = setup();
    let mut ui = UiState::new_default(
        "test".into(),
        "test".into(),
        ThemeStore::load_at(None).unwrap(),
        &[],
    );
    ui.add_user("Run the checks");
    ui.event(UiEvent::ToolStarted {
        name: "shell".into(),
        id: "checks".into(),
        args: "cargo test --lib".into(),
    });
    ui.event(UiEvent::ToolFinished {
        id: "checks".into(),
        result: "exit: 0\ntest result: ok. 1 passed; 0 failed\nother details\nALL_TESTS_PASSED"
            .into(),
        failed: false,
    });
    ui.editor.paste("draft to keep");
    let closed = screen(&mut ui);
    assert!(
        closed.contains("cargo test --lib"),
        "rendered screen: {closed}"
    );
    assert!(!closed.contains("ALL_TESTS_PASSED"));
    key(&mut ui, &model, KeyCode::Char('o'), KeyModifiers::CONTROL);
    assert!(ui.tools_expanded);
    assert!(!ui.has_plugin_panel());
    assert!(screen(&mut ui).contains("ALL_TESTS_PASSED"));
    assert_eq!(ui.editor.text(), "draft to keep");
    key(&mut ui, &model, KeyCode::Char('o'), KeyModifiers::CONTROL);
    assert!(!ui.tools_expanded);
    assert!(!screen(&mut ui).contains("ALL_TESTS_PASSED"));
    assert_eq!(ui.editor.text(), "draft to keep");
}

#[test]
fn multiline_paste_is_editable_and_waits_for_an_explicit_send() {
    let pasted = "任务\r\n\t中文 👩‍💻\r\n\r\n".repeat(20);
    let expected = pasted.replace("\r\n", "\n");
    for running in [false, true] {
        let (mut ui, model) = setup();
        assert!(matches!(
            input_event(
                &mut ui,
                Event::Paste(pasted.clone()),
                running,
                &model,
                &std::env::temp_dir(),
            ),
            InputAction::None
        ));
        assert_eq!(ui.editor.text(), expected);
        assert!(ui.attachments.is_empty());
        key(&mut ui, &model, KeyCode::Char('j'), KeyModifiers::CONTROL);
        let action = input_event(
            &mut ui,
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            running,
            &model,
            &std::env::temp_dir(),
        );
        assert!(matches!(action, InputAction::Submit(text) if text == format!("{expected}\n")));
    }
}

#[test]
fn keyboard_selection_copies_raw_multiline_text_across_editor_viewports() {
    let (mut ui, model) = setup();
    let text = "👩‍💻 中文\n\tindented\n\n".repeat(20);
    ui.editor.paste(&text);
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal.draw(|frame| ui.draw(frame)).unwrap();
    let shift_ctrl = KeyModifiers::CONTROL | KeyModifiers::SHIFT;
    key(&mut ui, &model, KeyCode::Home, shift_ctrl);
    terminal.draw(|frame| ui.draw(frame)).unwrap();
    assert_eq!(ui.editor_selected_text().as_deref(), Some(text.as_str()));
    key(&mut ui, &model, KeyCode::Right, KeyModifiers::SHIFT);
    assert_eq!(
        ui.editor_selected_text().as_deref(),
        text.strip_prefix("👩‍💻")
    );
    key(&mut ui, &model, KeyCode::Home, KeyModifiers::CONTROL);
    key(&mut ui, &model, KeyCode::End, shift_ctrl);
    terminal.draw(|frame| ui.draw(frame)).unwrap();
    assert_eq!(ui.editor_selected_text().as_deref(), Some(text.as_str()));
    assert_eq!(ui.editor.text(), text);
}

#[test]
fn paste_replaces_a_multiline_selection_in_one_undo_step() {
    let (mut ui, model) = setup();
    let original = "keep\nold A\nold B\n尾巴";
    ui.editor.paste(original);
    key(&mut ui, &model, KeyCode::Home, KeyModifiers::CONTROL);
    key(&mut ui, &model, KeyCode::Down, KeyModifiers::NONE);
    key(&mut ui, &model, KeyCode::Down, KeyModifiers::SHIFT);
    key(&mut ui, &model, KeyCode::End, KeyModifiers::SHIFT);
    assert_eq!(ui.editor_selected_text().as_deref(), Some("old A\nold B"));
    input_event(
        &mut ui,
        Event::Paste("新🙂\r\n\tnew\n\nline".into()),
        false,
        &model,
        &std::env::temp_dir(),
    );
    let replaced = "keep\n新🙂\n\tnew\n\nline\n尾巴";
    assert_eq!(ui.editor.text(), replaced);
    assert!(ui.editor_selected_text().is_none());
    key(&mut ui, &model, KeyCode::Char('z'), KeyModifiers::CONTROL);
    assert_eq!(ui.editor.text(), original);
    key(&mut ui, &model, KeyCode::Char('y'), KeyModifiers::CONTROL);
    assert_eq!(ui.editor.text(), replaced);
}

#[test]
fn typing_deletion_and_newlines_replace_the_selection() {
    for (code, modifiers, replacement) in [
        (KeyCode::Char('x'), KeyModifiers::NONE, "x"),
        (KeyCode::Delete, KeyModifiers::NONE, ""),
        (KeyCode::Backspace, KeyModifiers::NONE, ""),
        (KeyCode::Enter, KeyModifiers::SHIFT, "\n"),
        (KeyCode::Enter, KeyModifiers::ALT, "\n"),
        (KeyCode::Char('j'), KeyModifiers::CONTROL, "\n"),
        (KeyCode::Tab, KeyModifiers::NONE, "    "),
    ] {
        let (mut ui, model) = setup();
        let original = "old\n中文 👩‍💻\ntext";
        ui.editor.paste(original);
        key(
            &mut ui,
            &model,
            KeyCode::Home,
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        );
        key(&mut ui, &model, code, modifiers);
        assert_eq!(ui.editor.text(), replacement, "{code:?}");
        key(&mut ui, &model, KeyCode::Char('z'), KeyModifiers::CONTROL);
        assert_eq!(ui.editor.text(), original, "one undo for {code:?}");
    }
}

#[test]
fn shift_selection_at_draft_boundaries_never_recalls_history() {
    let (mut ui, model) = setup();
    ui.editor.paste("previous message");
    assert!(matches!(
        key_action(
            &mut ui,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            false,
            &model,
        ),
        InputAction::Submit(_)
    ));
    ui.editor.paste("current\ndraft");
    key(&mut ui, &model, KeyCode::Home, KeyModifiers::CONTROL);
    key(&mut ui, &model, KeyCode::Up, KeyModifiers::SHIFT);
    assert_eq!(ui.editor.text(), "current\ndraft");
    assert_eq!(ui.editor.history_index(), None);
    key(
        &mut ui,
        &model,
        KeyCode::End,
        KeyModifiers::CONTROL | KeyModifiers::SHIFT,
    );
    key(&mut ui, &model, KeyCode::Down, KeyModifiers::SHIFT);
    assert_eq!(ui.editor.text(), "current\ndraft");
    assert_eq!(ui.editor.history_index(), None);
}

#[test]
fn rejected_large_paste_and_empty_paste_keep_the_existing_selection() {
    let (mut ui, model) = setup();
    ui.editor.paste("keep\nthis draft");
    key(
        &mut ui,
        &model,
        KeyCode::Home,
        KeyModifiers::CONTROL | KeyModifiers::SHIFT,
    );
    for pasted in ["x".repeat(4097), String::new()] {
        input_event(
            &mut ui,
            Event::Paste(pasted),
            true,
            &model,
            &std::env::temp_dir(),
        );
        assert_eq!(ui.editor.text(), "keep\nthis draft");
        assert_eq!(
            ui.editor_selected_text().as_deref(),
            Some("keep\nthis draft")
        );
        assert!(ui.attachments.is_empty());
    }
}

#[test]
fn clipboard_text_shortcuts_are_explicit_and_ignore_release_events() {
    for (code, modifiers, expected) in [
        (KeyCode::Char('v'), KeyModifiers::CONTROL, true),
        (
            KeyCode::Char('V'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            true,
        ),
        (KeyCode::Insert, KeyModifiers::SHIFT, true),
        (KeyCode::Insert, KeyModifiers::CONTROL, false),
        (KeyCode::Char('v'), KeyModifiers::ALT, false),
        (KeyCode::Char('v'), KeyModifiers::NONE, false),
    ] {
        let mut event = KeyEvent::new(code, modifiers);
        assert_eq!(text_paste_shortcut(&event), expected, "{event:?}");
        event.kind = KeyEventKind::Release;
        assert!(!text_paste_shortcut(&event));
    }
}

#[test]
fn clipboard_multiline_text_uses_the_same_editable_paste_path() {
    let (mut ui, _) = setup();
    let text = "line\n\t中文\n".repeat(20);
    attach_text(&mut ui, &text, &std::env::temp_dir());
    assert_eq!(ui.editor.text(), text);
    assert!(ui.attachments.is_empty());
    let (displayed, _, _) = ui.editor.viewport(80, 100);
    assert!(displayed.contains("    中文"));
    assert_eq!(
        ui.editor
            .selected_display_text(0, text.chars().count())
            .as_deref(),
        Some(text.as_str())
    );
}

#[test]
fn large_attachment_paste_replaces_selection_and_preserves_undo_file_ownership() {
    let (mut ui, model) = setup();
    let dir = std::env::temp_dir().join(format!(
        "kroma-multiline-paste-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    ui.editor.paste("before ");
    let old = Attachment::from_log("original attachment", &dir).unwrap();
    let old_path = old.path.clone();
    ui.attach(old).unwrap();
    ui.editor.paste(" after");
    let original = ui.editor.text().to_owned();
    key(
        &mut ui,
        &model,
        KeyCode::Home,
        KeyModifiers::CONTROL | KeyModifiers::SHIFT,
    );
    let text = "large log output\n".repeat(400);
    input_event(&mut ui, Event::Paste(text.clone()), false, &model, &dir);
    assert_eq!(ui.attachments.len(), 1);
    let new_path = ui.attachments[0].path.clone();
    assert_ne!(new_path, old_path);
    assert_eq!(std::fs::read_to_string(&new_path).unwrap(), text);
    assert_eq!(ui.editor.text().chars().count(), 1);
    key(&mut ui, &model, KeyCode::Char('z'), KeyModifiers::CONTROL);
    assert_eq!(ui.editor.text(), original);
    assert_eq!(ui.attachments.len(), 1);
    assert_eq!(ui.attachments[0].path, old_path);
    assert!(new_path.exists());
    key(&mut ui, &model, KeyCode::Char('y'), KeyModifiers::CONTROL);
    assert_eq!(ui.attachments.len(), 1);
    assert_eq!(ui.attachments[0].path, new_path);
    ui.discard_draft();
    assert!(!old_path.exists());
    assert!(!new_path.exists());
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn ctrl_q_preserves_unsent_drafts_and_running_messages_choose_the_right_lane() {
    let (mut ui, model) = setup();
    let dir = std::env::temp_dir();
    let q = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL);
    let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
    let queue_enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT);

    ui.editor.paste("unsent draft");
    assert!(matches!(
        input_event(&mut ui, Event::Key(q), false, &model, &dir),
        InputAction::None
    ));
    assert_eq!(ui.editor.text(), "unsent draft");

    assert!(matches!(
        input_event(&mut ui, Event::Key(q), true, &model, &dir),
        InputAction::Queue(text) if text == "unsent draft"
    ));
    assert!(ui.editor.text().is_empty());

    ui.editor.paste("interrupt current task");
    assert!(matches!(
        input_event(&mut ui, Event::Key(enter), true, &model, &dir),
        InputAction::Submit(text) if text == "interrupt current task"
    ));
    ui.editor.paste("run next after finishing");
    assert!(matches!(
        input_event(&mut ui, Event::Key(queue_enter), true, &model, &dir),
        InputAction::Queue(text) if text == "run next after finishing"
    ));

    ui.editor.paste("keep");
    assert!(matches!(
        input_event(&mut ui, Event::Key(queue_enter), false, &model, &dir),
        InputAction::None
    ));
    assert_eq!(ui.editor.text(), "keep\n");
}

#[test]
fn input_history_recall_undo_restores_the_original_draft() {
    let (mut ui, model) = setup();
    ui.editor
        .restore_history(vec![("saved prompt".into(), Default::default(), vec![])]);
    ui.editor.paste("draft not sent");
    key(&mut ui, &model, KeyCode::Char('r'), KeyModifiers::CONTROL);
    key(&mut ui, &model, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(ui.editor.text(), "saved prompt");
    key(&mut ui, &model, KeyCode::Char('z'), KeyModifiers::CONTROL);
    assert_eq!(ui.editor.text(), "draft not sent");
    assert!(ui.editor.history_index().is_none());
    key(&mut ui, &model, KeyCode::Char('y'), KeyModifiers::CONTROL);
    assert_eq!(ui.editor.text(), "saved prompt");
}

#[test]
fn session_draft_switches_preserve_text_and_attachment_ownership() {
    let (mut ui, _model) = setup();
    let dir = std::env::temp_dir().join(format!("kroma-draft-switch-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    ui.editor.paste("session one");
    let item = Attachment::from_log("preserved evidence", &dir).unwrap();
    let path = item.path.clone();
    ui.attach(item).unwrap();

    ui.switch_session_draft("session-two", false);
    ui.session = "session-two".into();
    assert!(ui.editor.text().is_empty());
    assert!(ui.attachments.is_empty());
    assert!(
        path.exists(),
        "switching must not delete the draft attachment"
    );

    ui.editor.paste("session two");
    ui.switch_session_draft("test", false);
    ui.session = "test".into();
    assert!(ui.editor.text().starts_with("session one"));
    assert_eq!(ui.attachments.len(), 1);
    assert_eq!(ui.attachments[0].path, path);

    ui.switch_session_draft("test", true);
    ui.enter_home();
    assert!(ui.editor.text().is_empty());
    ui.editor.paste("welcome draft");
    ui.switch_session_draft("test", false);
    ui.load_history(&[]);
    assert!(ui.editor.text().starts_with("session one"));
    ui.switch_session_draft("test", true);
    ui.enter_home();
    assert_eq!(ui.editor.text(), "welcome draft");

    ui.switch_session_draft("test", false);
    ui.load_history(&[]);
    ui.discard_draft();
    assert!(!path.exists(), "discard explicitly releases its draft copy");
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn parked_draft_attachments_are_cleaned_when_tui_exits() {
    let dir =
        std::env::temp_dir().join(format!("kroma-parked-draft-cleanup-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path;
    {
        let (mut ui, _) = setup();
        let item = Attachment::from_log("draft-only evidence", &dir).unwrap();
        path = item.path.clone();
        ui.attach(item).unwrap();
        ui.switch_session_draft("other-session", false);
        assert!(path.exists());
        ui.session = "other-session".into();
    }
    assert!(!path.exists());
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn help_and_thinking_alias_are_local_and_do_not_append_transcript_entries() {
    let (mut ui, model) = setup();
    ui.editor.paste("keep draft");
    assert!(show_local_command(&mut ui, "/help", &model));
    assert!(ui.has_plugin_panel());
    assert_eq!(ui.editor.text(), "keep draft");
    ui.close_plugin_panel();
    assert!(show_local_command(&mut ui, "/hotkeys", &model));
    assert!(ui.has_plugin_panel());
    ui.close_plugin_panel();
    assert!(show_local_command(&mut ui, "/thinking", &model));
    assert!(show_local_command(&mut ui, "/thinking auto", &model));
}

#[test]
fn familiar_shortcuts_select_model_recall_history_and_keep_draft() {
    let (mut ui, model) = setup();
    let dir = std::env::temp_dir();

    for prompt in ["first prompt", "second prompt"] {
        ui.editor.paste(prompt);
        assert!(matches!(
            input_event(
                &mut ui,
                Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
                false,
                &model,
                &dir
            ),
            InputAction::Submit(_)
        ));
    }
    ui.editor.paste("unsent draft");
    key(&mut ui, &model, KeyCode::Char('r'), KeyModifiers::CONTROL);
    assert_eq!(ui.picker.as_ref().unwrap().kind, PickerKind::History);
    for ch in "first".chars() {
        key(&mut ui, &model, KeyCode::Char(ch), KeyModifiers::NONE);
    }
    key(&mut ui, &model, KeyCode::Enter, KeyModifiers::NONE);
    assert!(ui.picker.is_none());
    assert_eq!(ui.editor.text(), "first prompt", "recall must not send");

    key(&mut ui, &model, KeyCode::Down, KeyModifiers::NONE);
    assert_eq!(ui.editor.text(), "second prompt");
    key(&mut ui, &model, KeyCode::Down, KeyModifiers::NONE);
    assert_eq!(ui.editor.text(), "unsent draft");

    key(&mut ui, &model, KeyCode::Char('r'), KeyModifiers::CONTROL);
    key(&mut ui, &model, KeyCode::Esc, KeyModifiers::NONE);
    assert!(ui.picker.is_none());
    assert_eq!(ui.editor.text(), "unsent draft", "Esc keeps the draft");

    key(&mut ui, &model, KeyCode::Char('l'), KeyModifiers::CONTROL);
    assert_eq!(ui.picker.as_ref().unwrap().kind, PickerKind::Models);
    key(&mut ui, &model, KeyCode::Esc, KeyModifiers::NONE);
    assert_eq!(ui.editor.text(), "unsent draft");
}

#[test]
fn history_search_renders_the_recalled_entries_own_attachment_labels() {
    let (mut ui, model) = setup();
    let marker = char::from_u32(editor::ATTACHMENT_START).unwrap();
    let labels = std::collections::HashMap::from([(marker, "【source.txt】".into())]);
    ui.editor
        .restore_history(vec![(format!("inspect {marker}"), labels, vec![])]);
    ui.editor.paste("unsent draft");
    key(&mut ui, &model, KeyCode::Char('r'), KeyModifiers::CONTROL);
    assert!(ui.picker.as_ref().unwrap().items[0]
        .label
        .contains("【source.txt】"));
    key(&mut ui, &model, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(ui.editor.text(), format!("inspect {marker}"));
    key(&mut ui, &model, KeyCode::Down, KeyModifiers::NONE);
    assert_eq!(ui.editor.text(), "unsent draft");
}

#[test]
fn escape_and_ctrl_t_respect_modals_and_running_state() {
    let (mut ui, model) = setup();
    let dir = std::env::temp_dir();
    ui.editor.paste("draft survives");

    key(&mut ui, &model, KeyCode::Char('t'), KeyModifiers::CONTROL);
    assert!(ui.reasoning_expanded);
    key(&mut ui, &model, KeyCode::Char('t'), KeyModifiers::CONTROL);
    assert!(!ui.reasoning_expanded);

    key(&mut ui, &model, KeyCode::Char('t'), KeyModifiers::ALT);
    assert!(ui.has_plugin_panel(), "full transcript is still accessible");
    assert!(matches!(
        input_event(
            &mut ui,
            Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            true,
            &model,
            &dir
        ),
        InputAction::None
    ));
    assert!(!ui.has_plugin_panel(), "first Esc closes a modal");
    assert!(matches!(
        input_event(
            &mut ui,
            Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            true,
            &model,
            &dir
        ),
        InputAction::Cancel
    ));
    assert_eq!(ui.editor.text(), "draft survives");
}

#[test]
fn ctrl_d_and_readline_word_motion_have_consistent_semantics() {
    let (mut ui, model) = setup();
    let dir = std::env::temp_dir();
    assert!(matches!(
        input_event(
            &mut ui,
            Event::Key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL)),
            false,
            &model,
            &dir
        ),
        InputAction::Quit
    ));
    ui.editor.paste("alpha beta");
    key(&mut ui, &model, KeyCode::Char('a'), KeyModifiers::CONTROL);
    key(&mut ui, &model, KeyCode::Char('d'), KeyModifiers::CONTROL);
    assert_eq!(ui.editor.text(), "lpha beta");
    key(&mut ui, &model, KeyCode::Char('f'), KeyModifiers::ALT);
    assert_eq!(ui.editor.caret(), 5);
    key(&mut ui, &model, KeyCode::Char('b'), KeyModifiers::ALT);
    assert_eq!(ui.editor.caret(), 0);
    key(&mut ui, &model, KeyCode::Char('e'), KeyModifiers::CONTROL);
    assert_eq!(ui.editor.text(), "lpha beta");
    ui.editor.clear();
    key(&mut ui, &model, KeyCode::Char('?'), KeyModifiers::NONE);
    assert!(ui.has_plugin_panel(), "? on an empty draft opens help");
    key(&mut ui, &model, KeyCode::Esc, KeyModifiers::NONE);
    assert!(!ui.has_plugin_panel());
    assert!(ui.editor.text().is_empty());
}

#[test]
fn multiline_composer_explains_newline_and_send_or_queue_in_both_languages() {
    for language in [preferences::Language::En, preferences::Language::Zh] {
        let (mut ui, _) = setup();
        ui.preferences.language = language;
        ui.editor.paste("first\nsecond\n");
        let mut terminal = Terminal::new(TestBackend::new(100, 28)).unwrap();
        for running in [false, true] {
            ui.running = running;
            terminal.draw(|frame| ui.draw(frame)).unwrap();
            let rendered: String = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|cell| cell.symbol())
                .collect();
            let compact = rendered.replace(' ', "");
            assert!(compact.contains("Ctrl+J"));
            assert!(compact.contains(language.text("3lines", "3行")));
            let action = match (language, running) {
                (preferences::Language::En, false) => "Entersend",
                (preferences::Language::En, true) => "Entersteer",
                (preferences::Language::Zh, false) => "Enter发送",
                (preferences::Language::Zh, true) => "Enter引导",
            };
            assert!(compact.contains(action), "{compact}");
        }
    }
}
