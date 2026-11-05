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
                (preferences::Language::En, true) => "Enterqueue",
                (preferences::Language::Zh, false) => "Enter发送",
                (preferences::Language::Zh, true) => "Enter排队",
            };
            assert!(compact.contains(action), "{compact}");
        }
    }
}
