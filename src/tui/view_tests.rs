use super::*;
use crate::{events::RuntimeEvent, tui::runtime_events::UiSink};
use ratatui::{backend::TestBackend, Terminal};

#[test]
fn plain_animation_uses_builtin_frames_with_a_single_plugin_frame() {
    let mut ui = UiState::new_default(
        "mock".into(),
        "session".into(),
        ThemeStore::load_at(None).unwrap(),
        &[],
    );
    ui.set_plugin_presentation(vec!["ONE".into()], BTreeMap::new(), None, BTreeMap::new());
    ui.event(UiEvent::TaskStarted);
    ui.last_activity_frame = Instant::now() - Duration::from_millis(600);
    assert!(ui.tick_activity());
    assert_eq!(
        ui.activity_frame, 1,
        "Plain must not freeze on one plugin frame"
    );
    ui.last_activity_frame = Instant::now() - Duration::from_millis(600);
    assert!(ui.tick_activity());
    assert_eq!(ui.activity_frame, 2);
}

/// Opt-in local benchmark; measures complete transcript reconciliation,
/// separately from the existing cached viewport rendering benchmark.
#[test]
#[ignore = "local 3k/30k session reload performance, no provider calls"]
fn perf_full_history_reload_three_thousand_and_thirty_thousand_messages() {
    for count in [3_000, 30_000] {
        let messages = (0..count)
            .map(|i| {
                Message::new(
                    if i % 2 == 0 { "user" } else { "assistant" },
                    format!("Line {i}: {}", "long history ".repeat(8)),
                )
            })
            .collect::<Vec<_>>();
        let mut ui = UiState::new_default(
            "mock".into(),
            "history-perf".into(),
            ThemeStore::load_at(None).unwrap(),
            &[],
        );
        let started = Instant::now();
        ui.load_history(&messages);
        let elapsed = started.elapsed();
        assert_eq!(ui.entries.len(), count);
        eprintln!(
            "PERF full_history_reload messages={count} ms={:.3}",
            elapsed.as_secs_f64() * 1000.0
        );
    }
}

#[test]
fn plugin_presentation_slots_render_without_changing_model_history() {
    let mut ui = UiState::new(
        "mock".into(),
        "session".into(),
        ThemeStore::load_at(None).unwrap(),
        &[Message::new("system", "root")],
    );
    let mut labels = BTreeMap::new();
    labels.insert("example_echo".into(), "Friendly Echo".into());
    ui.set_plugin_presentation(
        vec![".".into(), "o".into()],
        labels,
        Some(PluginLayoutSpec {
            composer_max_rows: Some(6),
            ..Default::default()
        }),
        BTreeMap::from([("accent".into(), "#112233".into())]),
    );
    assert_eq!(ui.active_colors().accent, Color::Rgb(17, 34, 51));
    ui.event(UiEvent::TaskStarted);
    ui.event(UiEvent::ToolStarted {
        name: "example_echo".into(),
        id: "plugin-1".into(),
        args: "{\"text\":\"test\"}".into(),
    });
    let first = ui.activity_line(ui.themes.colors);
    assert_eq!(first.spans[0].content.as_ref(), ".");
    ui.last_activity_frame = Instant::now() - Duration::from_millis(600);
    assert!(ui.tick_activity());
    let next = ui.activity_line(ui.themes.colors);
    assert_eq!(next.spans[0].content.as_ref(), "o");
    let history = ui.history_text(ui.themes.colors, 100).to_string();
    assert!(history.contains("Friendly Echo"), "{history}");
    let cached = ui
        .history_cache
        .as_ref()
        .map(|cache| cache.text.lines.as_ptr());
    ui.last_activity_frame = Instant::now() - Duration::from_millis(600);
    assert!(ui.tick_activity());
    let next = ui.activity_line(ui.themes.colors);
    assert_eq!(next.spans[0].content.as_ref(), ".");
    assert_eq!(
        cached,
        ui.history_cache
            .as_ref()
            .map(|cache| cache.text.lines.as_ptr()),
        "animation should not re-render the transcript"
    );
    ui.set_plugin_presentation(Vec::new(), BTreeMap::new(), None, BTreeMap::new());
    assert_eq!(ui.active_colors().accent, ui.themes.colors.accent);
    assert_eq!(ui.activity_line(ui.themes.colors).spans.len(), 5);
    assert!(!ui
        .history_text(ui.themes.colors, 100)
        .to_string()
        .contains("Friendly Echo"));
}

#[test]
fn plugin_panel_overlay_scrolls_and_keeps_composer_unchanged() {
    let mut ui = UiState::new(
        "mock".into(),
        "session".into(),
        ThemeStore::load_at(None).unwrap(),
        &[],
    );
    ui.editor.paste("draft must survive");
    let body = (0..40)
        .map(|index| format!("line {index}"))
        .collect::<Vec<_>>()
        .join("\n");
    ui.open_plugin_panel(format!("Live Plugin\n\n{body}"));
    assert!(ui.has_plugin_panel());
    assert_eq!(ui.plugin_panel.as_ref().unwrap().scroll, 0);
    let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
    terminal.draw(|frame| ui.draw(frame)).unwrap();
    let all = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(all.contains("Live Plugin"));
    assert!(all.contains("line 0"));
    let cached = ui.plugin_panel.as_ref().unwrap().wrapped.lines.as_ptr();
    let initial_width = ui.plugin_panel.as_ref().unwrap().wrapped_width;
    terminal.draw(|frame| ui.draw(frame)).unwrap();
    assert_eq!(
        cached,
        ui.plugin_panel.as_ref().unwrap().wrapped.lines.as_ptr(),
        "an unchanged panel width must reuse the existing layout"
    );
    ui.scroll_plugin_panel(12);
    assert_eq!(ui.plugin_panel.as_ref().unwrap().scroll, 12);
    terminal.draw(|frame| ui.draw(frame)).unwrap();
    let scrolled = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(scrolled.contains("line 12"));
    assert_eq!(
        cached,
        ui.plugin_panel.as_ref().unwrap().wrapped.lines.as_ptr(),
        "scrolling a panel should not force reflow"
    );
    ui.scroll_plugin_panel(-8);
    assert_eq!(ui.plugin_panel.as_ref().unwrap().scroll, 4);
    let mut tiny = Terminal::new(TestBackend::new(22, 8)).unwrap();
    tiny.draw(|frame| ui.draw(frame)).unwrap();
    assert!(
        ui.plugin_panel.as_ref().unwrap().wrapped_width < initial_width,
        "resizing the terminal must invalidate and rebuild the cached layout"
    );
    let visible = tiny
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(
        visible.contains("Esc"),
        "panel must be closable on narrow terminals"
    );
    ui.close_plugin_panel();
    assert!(!ui.has_plugin_panel());
    assert_eq!(ui.editor.text(), "draft must survive");
}

#[test]
fn tool_history_shows_executed_arguments_when_plugin_rewrites_call() {
    let mut model_reply = Message::new("assistant", "");
    model_reply.tool_calls = Some(vec![crate::protocol::ToolCall {
        id: "call-1".into(),
        kind: "function".into(),
        function: crate::protocol::FunctionCall {
            name: "shell".into(),
            arguments: r#"{"command":"echo original"}"#.into(),
        },
    }]);
    let mut tool = Message::tool_with_status("call-1".into(), "exit: 0".into(), false);
    tool.provider_items = Some(vec![serde_json::json!({
        "type":"kroma_tool_execution","name":"shell",
        "arguments":r#"{"command":"echo adjusted"}"#
    })]);
    let ui = UiState::new(
        "mock".into(),
        "test".into(),
        ThemeStore::load_at(None).unwrap(),
        &[Message::new("system", "system"), model_reply, tool],
    );
    let entry = ui
        .entries
        .iter()
        .find(|entry| matches!(entry.kind, Kind::Tool))
        .unwrap();
    assert!(
        entry.body.contains("adjusted"),
        "render the command actually executed"
    );
    assert!(
        entry.details.contains("original"),
        "retain the model's original call for tracing"
    );
}

#[test]
fn usage_and_cache_restore_before_next_model_call_and_are_session_scoped() -> anyhow::Result<()> {
    use crate::sessions::SessionCatalog;
    let directory = std::env::temp_dir().join(format!(
        "kroma-usage-restore-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos()
    ));
    std::fs::create_dir_all(&directory)?;
    let default = directory.join("project.json");
    std::fs::write(&default, b"{}")?;
    let catalog = SessionCatalog::from_default(default.clone())?;
    let (other, other_path) = catalog.new_path()?;
    std::fs::write(other_path, b"{}")?;
    let mut first = UiState::new(
        "deepseek".into(),
        "default".into(),
        ThemeStore::load_at(None)?,
        &[],
    );
    first.model_catalog = Some(catalog);
    first.configure_status(120_000, Some("high".into()));
    first.event(UiEvent::Usage {
        input: Some(10_000),
        output: Some(600),
        cached: Some(8_000),
        reasoning: Some(150),
        cache_write: None,
    });
    first.event(UiEvent::ActualInput(10_000));
    first.event(UiEvent::ApecUsage {
        requests: 1,
        input: Some(1_200),
        output: Some(140),
        cached: Some(200),
        cache_write: None,
    });
    first.save_usage()?;
    drop(first);

    let mut reopened = UiState::new(
        "deepseek".into(),
        "default".into(),
        ThemeStore::load_at(None)?,
        &[],
    );
    reopened.model_catalog = Some(SessionCatalog::from_default(default.clone())?);
    reopened.configure_status(120_000, Some("high".into()));
    reopened.restore_usage()?;
    assert_eq!(reopened.usage.calls, 1);
    assert_eq!(reopened.usage.cached, 8_000);
    assert_eq!(reopened.apec_usage.calls, 1);
    assert_eq!(reopened.context_input, Some(10_000));
    let footer = reopened.usage_report();
    assert!(
        footer.contains("80%"),
        "cache ratio must be available in /usage immediately: {footer}"
    );
    assert!(
        footer.contains("10k"),
        "last request usage must be available in /usage immediately: {footer}"
    );

    reopened.session = other.clone();
    reopened.restore_usage()?;
    assert_eq!(
        reopened.usage.calls, 0,
        "session switch cannot inherit old counters"
    );
    reopened.event(UiEvent::TitleUsage {
        session_id: "default".into(),
        requests: 1,
        input: Some(100),
        output: Some(12),
        cached: Some(0),
        reasoning: None,
    });
    assert_eq!(
        reopened.title_usage.calls, 0,
        "late titles belong to their source session"
    );
    reopened.session = "default".into();
    reopened.restore_usage()?;
    assert_eq!(reopened.title_usage.calls, 1);
    assert_eq!(reopened.usage.calls, 1);
    std::fs::remove_dir_all(directory)?;
    Ok(())
}

#[test]
fn verbatim_windows_paths_are_readable_in_the_header() {
    assert_eq!(strip_verbatim_path(r"\\?\D:\Kroma"), r"D:\Kroma");
    assert_eq!(
        strip_verbatim_path(r"\\?\UNC\server\share\project"),
        r"\\server\share\project"
    );
}

#[test]
fn header_separation_and_role_body_columns_are_consistent() {
    let history = vec![
        Message::new("user", "example user line"),
        Message::new("assistant", "Example assistant response"),
    ];
    let mut ui = UiState::new(
        "test".into(),
        "session".into(),
        ThemeStore::load_at(None).unwrap(),
        &history,
    );
    let rows = rendered_text(&mut ui, 110, 30);
    assert!(
        rows[1].trim().is_empty(),
        "title needs an independent blank row"
    );
    assert_eq!(ui.selection_area.y, 2);
    for (needle, mark) in [
        ("example user line", None),
        ("Example assistant response", Some("◆")),
    ] {
        let row = rows
            .iter()
            .find(|row| row.contains(needle))
            .expect("body visible");
        assert_eq!(
            UnicodeWidthStr::width(&row[..row.find(needle).unwrap()]),
            usize::from(ui.selection_area.x) + 4
        );
        if let Some(mark) = mark {
            assert_eq!(
                UnicodeWidthStr::width(&row[..row.find(mark).unwrap()]),
                usize::from(ui.selection_area.x) + 1
            );
        }
    }
    assert!(!ui
        .selection_rows
        .iter()
        .any(|row| row.contains('▎') || row.contains('◆')));
}

#[test]
fn context_checkpoint_has_visible_activity_and_returns_to_connecting() {
    let (tx, mut rx) = super::super::event_queue::channel();
    crate::events::EventSink::on_event(&UiSink(tx), RuntimeEvent::CompactionStarted);
    let mut ui = UiState::new(
        "test".into(),
        "session".into(),
        ThemeStore::load_at(None).unwrap(),
        &[],
    );
    ui.event(UiEvent::TaskStarted);
    ui.event(rx.try_recv().unwrap());
    assert_eq!(ui.phase, "compacting");
    assert!(ui.activity_status().contains("75s"));
    assert!(ui.activity_status().contains("1 retry"));
    let rows = rendered_rows(&mut ui, 100, 28);
    assert!(rows.iter().any(|row| row.contains("75s")));
    ui.event(UiEvent::ModelStarted);
    assert_eq!(ui.phase, "waiting");
    ui.event(UiEvent::TaskFinished);
    assert_eq!(ui.phase, "ready");
}

#[test]
fn connecting_steering_stays_in_composer_not_activity_row() {
    let mut ui = UiState::new(
        "test".into(),
        "session".into(),
        ThemeStore::load_at(None).unwrap(),
        &[],
    );
    ui.event(UiEvent::TaskStarted);
    ui.event(UiEvent::ModelStarted);
    assert!(
        ui.show_editor_cursor(),
        "IME requires a composer cursor before typing"
    );
    ui.editor.paste("测试输入法");
    let rows = rendered_rows(&mut ui, 100, 28);
    let waiting = rows
        .iter()
        .position(|row| row.contains("Connecting"))
        .unwrap();
    let composer = rows
        .iter()
        .position(|row| row.contains("测试输入法"))
        .unwrap();
    assert!(waiting < composer);
    assert!(!rows[waiting].contains("测试输入法"));
}

#[test]
fn blank_history_click_does_not_reverse_a_cell_and_drag_still_selects_text() {
    let mut ui = UiState::new(
        "mock".into(),
        "session".into(),
        ThemeStore::load_at(None).unwrap(),
        &[Message::new("assistant", "SELECTABLE_TEXT")],
    );
    let mut terminal = Terminal::new(TestBackend::new(90, 28)).unwrap();
    terminal.draw(|frame| ui.draw(frame)).unwrap();

    let area = ui.selection_area;
    let blank = (area.x + area.width / 2, area.y + area.height / 2);
    assert_eq!(
        terminal.backend().buffer().cell(blank).unwrap().symbol(),
        " "
    );
    ui.select_start(blank.0, blank.1);
    assert!(ui.selected_text().is_none(), "Down is not a selection");
    terminal.draw(|frame| ui.draw(frame)).unwrap();
    assert!(
        !terminal
            .backend()
            .buffer()
            .cell(blank)
            .unwrap()
            .modifier
            .contains(Modifier::REVERSED),
        "clicking empty history must not draw a fake cursor"
    );
    ui.mouse_release();
    assert!(ui.selection_start.is_none());
    assert!(ui.selection_end.is_none());

    // A genuine drag still selects transcript text and survives mouse release.
    let (x, y) = terminal
        .backend()
        .buffer()
        .content()
        .chunks(90)
        .enumerate()
        .find_map(|(row, cells)| {
            cells
                .windows("SELECTABLE_TEXT".len())
                .position(|cells| {
                    cells.iter().map(|cell| cell.symbol()).collect::<String>() == "SELECTABLE_TEXT"
                })
                .map(|col| (col as u16, row as u16))
        })
        .expect("rendered assistant text");
    assert!(area.contains((x, y).into()));
    ui.select_start(x, y);
    ui.select_drag(x + 5, y);
    ui.mouse_release();
    assert!(ui
        .selected_text()
        .is_some_and(|text| text.contains("SELECT")));
}

#[test]
fn command_output_survives_same_session_reload_without_entering_wal() {
    let history = vec![
        Message::new("user", "first question"),
        Message::new("assistant", "first answer"),
    ];
    let mut ui = UiState::new(
        "mock".into(),
        "session-a".into(),
        ThemeStore::load_at(None).unwrap(),
        &history,
    );
    ui.info("DIFF", "temporary diff output");
    let mut updated = history.clone();
    updated.push(Message::new("user", "next question"));
    updated.push(Message::new("assistant", "next answer"));
    ui.load_history(&updated);
    let info_at = ui
        .entries
        .iter()
        .position(|entry| matches!(entry.kind, Kind::Info))
        .unwrap();
    let next_at = ui
        .entries
        .iter()
        .position(|entry| entry.body == "next question")
        .unwrap();
    assert_eq!(info_at, 2, "local output remains after its original turn");
    assert!(
        info_at < next_at,
        "history rebuild must not push old information to the end"
    );
    assert!(!updated
        .iter()
        .any(|message| message.content.as_deref() == Some("temporary diff output")));
    ui.session = "session-b".into();
    ui.load_history(&[Message::new("user", "another session")]);
    assert!(!ui
        .entries
        .iter()
        .any(|entry| matches!(entry.kind, Kind::Info)));
}

#[test]
fn declarative_plugin_footer_status_fits_only_wide_terminals() {
    let mut state = UiState::new(
        "mock".into(),
        "s".into(),
        ThemeStore::load_at(None).unwrap(),
        &[],
    );
    state.plugin_statuses = vec!["Plugin ready".into()];
    let wide = state.footer_line(state.themes.colors, 160);
    assert!(wide
        .spans
        .iter()
        .any(|span| span.content.contains("Plugin ready")));
    let narrow = state.footer_line(state.themes.colors, 40);
    assert!(!narrow
        .spans
        .iter()
        .any(|span| span.content.contains("Plugin ready")));
}

#[test]
fn live_reasoning_renders_before_text_and_reconciles_without_duplicates() {
    let mut state = UiState::new(
        "test".into(),
        "demo".into(),
        ThemeStore::load_at(None).unwrap(),
        &[],
    );
    assert!(!state.reasoning_expanded);
    state.event(UiEvent::TaskStarted);
    state.event(UiEvent::ModelStarted);
    state.event(UiEvent::ReasoningDelta("Thinking UNIQUE_SENTINEL ".into()));
    assert_eq!(state.phase, "thinking");
    let mut terminal = Terminal::new(TestBackend::new(95, 28)).unwrap();
    terminal.draw(|frame| state.draw(frame)).unwrap();
    let visible = state.selection_rows.join("\n");
    assert!(visible.contains("Reasoning"));
    assert!(!visible.contains("UNIQUE_SENTINEL"));
    state.reasoning_expanded = true;
    terminal.draw(|frame| state.draw(frame)).unwrap();
    assert!(state.selection_rows.join("\n").contains("UNIQUE_SENTINEL"));

    state.event(UiEvent::ReasoningDelta("step two".into()));
    state.event(UiEvent::Text("Answer".into()));
    state.event(UiEvent::ReasoningFinal("Thinking step two".into()));
    assert_eq!(state.entries.len(), 2);
    assert!(matches!(state.entries[0].kind, Kind::Reasoning));
    assert_eq!(state.entries[0].body, "Thinking step two");
    assert!(matches!(state.entries[1].kind, Kind::Assistant));
    assert_eq!(state.entries[1].body, "Answer");

    state.event(UiEvent::ToolStarted {
        name: "shell".into(),
        id: "call".into(),
        args: "pwd".into(),
    });
    state.event(UiEvent::ModelStarted);
    state.event(UiEvent::ReasoningDelta("Next turn".into()));
    assert_eq!(state.entries.len(), 4);
    assert_eq!(state.entries[3].body, "Next turn");
}

#[test]
fn activity_stays_with_newest_message_when_reader_is_browsing_history() {
    let messages = (0..75)
        .map(|index| Message::new("assistant", format!("historical result {index}")))
        .collect::<Vec<_>>();
    let mut ui = UiState::new(
        "deepseek".into(),
        "test".into(),
        ThemeStore::load_at(None).unwrap(),
        &messages,
    );
    rendered_text(&mut ui, 80, 23);
    ui.scroll(8);
    let anchor = ui.scroll_anchor;
    ui.event(UiEvent::TaskStarted);
    ui.event(UiEvent::ModelStarted);
    let rows = rendered_text(&mut ui, 80, 23);
    assert_eq!(ui.scroll_anchor, anchor);
    assert!(rows.iter().any(|row| row.contains("Browsing history")));
    assert!(
        rows.iter().any(|row| row.contains("Connecting")),
        "the pinned status stays visible while browsing history"
    );
    ui.scroll_to_bottom();
    let rows = rendered_text(&mut ui, 80, 23);
    assert!(rows.iter().any(|row| row.contains("Connecting")));
    assert!(!rows.iter().any(|row| row.contains("Browsing history")));
}

#[test]
fn short_top_aligned_history_keeps_links_clickable() {
    let messages = vec![
        Message::new("user", "show link"),
        Message::new(
            "assistant",
            "See https://example.com/issues/123 for details",
        ),
    ];
    let mut ui = UiState::new(
        "mock".into(),
        "test".into(),
        ThemeStore::load_at(None).unwrap(),
        &messages,
    );
    rendered_text(&mut ui, 120, 38);
    assert_eq!(
        ui.selection_area.y, 2,
        "the first turn starts below a blank title separator"
    );
    let link = ui.visible_links.first().expect("link must remain visible");
    assert_eq!(
        ui.link_at(link.x, link.y),
        Some("https://example.com/issues/123")
    );
}

#[test]
fn footer_distinguishes_context_from_api_tokens_and_cached_ratio() {
    let mut ui = UiState::new(
        "deepseek".into(),
        "test".into(),
        ThemeStore::load_at(None).unwrap(),
        &[],
    );
    ui.configure_status(1_000_000, Some("max".into()));
    ui.event(UiEvent::Context { estimated: 1_600 });
    assert_eq!(ui.context_display(), "Ctx ~1.6k / 1M · ~0.2%");
    ui.event(UiEvent::Usage {
        input: Some(8_000),
        output: Some(500),
        cached: Some(6_000),
        reasoning: Some(200),
        cache_write: None,
    });
    let mut terminal = Terminal::new(TestBackend::new(130, 26)).unwrap();
    terminal.draw(|frame| ui.draw(frame)).unwrap();
    let rendered = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(rendered.contains("Context ~0.2%"));
    assert!(!rendered.contains("(1.6k/1M)"));
    assert!(!rendered.contains("Cache"));
    assert!(ui.usage_report().contains("Cache 75%"));
    assert!(ui.usage_variants()[1].contains("In/out 8.0k / 500 · Cache 75%"));
    let mut narrow = Terminal::new(TestBackend::new(40, 26)).unwrap();
    narrow.draw(|frame| ui.draw(frame)).unwrap();
    let narrow_render = narrow
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(
        narrow_render.contains("Context ~0.2%") && !narrow_render.contains("Cache"),
        "narrow terminals prioritize context; cache remains available in /usage"
    );
}

#[test]
fn background_title_and_apec_usage_cannot_replace_coding_cache_or_context() {
    let mut ui = UiState::new(
        "deepseek".into(),
        "test".into(),
        ThemeStore::load_at(None).unwrap(),
        &[],
    );
    ui.configure_status(1_000_000, Some("high".into()));
    ui.event(UiEvent::Usage {
        input: Some(1_000),
        output: Some(80),
        cached: Some(800),
        reasoning: Some(20),
        cache_write: None,
    });
    ui.event(UiEvent::ActualInput(1_000));
    let coding_footer = ui.usage.compact_status(150);
    let context = ui.context_display();
    ui.event(UiEvent::TitleUsage {
        session_id: ui.session.clone(),
        requests: 1,
        input: Some(117),
        output: Some(95),
        cached: Some(0),
        reasoning: Some(80),
    });
    ui.event(UiEvent::ApecUsage {
        requests: 2,
        input: Some(500),
        output: Some(100),
        cached: Some(200),
        cache_write: None,
    });
    assert_eq!(ui.usage.compact_status(150), coding_footer);
    assert_eq!(ui.context_display(), context);
    assert_eq!(ui.usage.calls, 1);
    assert_eq!(ui.title_usage.calls, 1);
    assert_eq!(ui.apec_usage.calls, 2);
    let report = ui.usage.report_breakdown(&ui.title_usage, &ui.apec_usage);
    assert!(report.contains("Coding: 1 call(s) | In 1000"));
    assert!(report.contains("Auto titles: 1 call(s) | In 117"));
    assert!(report.contains("APEC summaries: 2 call(s) | In 500"));
    assert!(report.contains("Combined: 4 call(s) | In 1617 | Out 275 | Cache read 1000"));
    let mut terminal = Terminal::new(TestBackend::new(120, 26)).unwrap();
    terminal.draw(|frame| ui.draw(frame)).unwrap();
    let rendered = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(!rendered.contains("Cache"));
    assert!(ui.usage_report().contains("Cache 80%"));
    assert!(rendered.contains("Context 0.1%"));
    assert!(ui.usage_variants()[1].contains("In/out 1.0k / 80 · Cache 80%"));
    assert!(!rendered.contains("Out 95") && !rendered.contains("Total In"));
}

#[test]
fn home_draws_original_brand_mark_and_clears_on_first_send() {
    let previous = [Message::new("user", "Previous conversation")];
    let mut state = UiState::new(
        "deepseek".into(),
        "default".into(),
        ThemeStore::load_at(None).unwrap(),
        &previous,
    );
    state.enter_home();
    assert!(state.is_home());
    assert_eq!(state.session_label(), "new conversation");
    let mut terminal = Terminal::new(TestBackend::new(110, 36)).unwrap();
    terminal.draw(|frame| state.draw(frame)).unwrap();
    let contents = terminal.backend().buffer().content();
    let text = contents
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(text.matches("Kroma").count() >= 2);
    assert!(text.contains("Start a task below"));
    assert!(text.contains("Ctrl+P to explore"));
    assert!(!text.contains("Previous conversation"));
    assert!(contents
        .iter()
        .any(|cell| cell.fg == ratatui::style::Color::Rgb(91, 124, 255)));
    assert!(contents
        .iter()
        .any(|cell| cell.fg == ratatui::style::Color::Rgb(255, 111, 145)));
    state.add_user("Fix the tests");
    assert!(!state.is_home());
    terminal.draw(|frame| state.draw(frame)).unwrap();
    let text = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(!text.contains("/resume history"));
    assert!(text.contains("Fix the tests"));
}

#[test]
fn home_header_shows_workdir_title_and_zero_context() {
    let mut state = UiState::new(
        "deepseek-v4.1-flash".into(),
        "default".into(),
        ThemeStore::load_at(None).unwrap(),
        &[],
    );
    state.set_link_workdir(Path::new("workspace-demo"));
    state.configure_status(500_000, Some("high".into()));
    state.enter_home();
    assert_eq!(state.terminal_title(), "Kroma — new conversation");
    let mut terminal = Terminal::new(TestBackend::new(110, 30)).unwrap();
    terminal.draw(|frame| state.draw(frame)).unwrap();
    let text = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(text.contains("◆ Kroma"));
    assert!(
        text.contains("workspace-demo"),
        "welcome details keep the workdir visible"
    );
    assert_eq!(
        text.matches("deepseek-v4.1-flash").count(),
        1,
        "model appears only in the footer"
    );
    assert!(text.contains("Context —"));
    assert!(text.contains("Start a task below"));
    state.add_user("Fix the tests");
    assert_eq!(state.terminal_title(), "● Kroma — default");
}

#[test]
fn welcome_hints_shrink_before_the_work_area_does() {
    let mut state = UiState::new(
        "deepseek".into(),
        "default".into(),
        ThemeStore::load_at(None).unwrap(),
        &[],
    );
    state.enter_home();
    let mut terminal = Terminal::new(TestBackend::new(30, 16)).unwrap();
    terminal.draw(|frame| state.draw(frame)).unwrap();
    let text = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(text.contains("Kroma"));
    assert!(state.editor_area.height >= 1);
    assert!(text.contains("Start a task"));
    assert!(!text.contains("Shift+Enter newline"));
}

#[test]
fn failure_is_explained_without_expanding_tools_and_command_is_one_line() {
    let mut state = UiState::new(
        "model".into(),
        "default".into(),
        ThemeStore::load_at(None).unwrap(),
        &[],
    );
    state.event(UiEvent::ToolStarted {
        name: "shell".into(),
        id: "build".into(),
        args: "cargo test --offline --locked ".repeat(15),
    });
    state.event(UiEvent::ToolFinished {
        id: "build".into(),
        result: "exit: 101\nstderr: error[E0599]: method not found\nlong output".into(),
        failed: true,
    });
    let lines = state.history_text(state.themes.colors, 55).lines;
    let rows = lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>();
    assert_eq!(
        rows.iter()
            .filter(|line| line.contains("Run Rust tests"))
            .count(),
        1
    );
    assert!(rows
        .iter()
        .any(|line| line.contains("exit 101") && line.contains("E0599")));
    assert!(!rows.join("\n").contains("long output"));
    for row in &rows {
        assert!(
            UnicodeWidthStr::width(row.as_str()) <= 55,
            "collapsed tool row must fit in the reading column: {row:?}"
        );
    }
    state.tools_expanded = true;
    let expanded = state.history_text(state.themes.colors, 55).lines;
    assert!(expanded
        .iter()
        .flat_map(|line| line.spans.iter())
        .any(|span| span.content.contains("long output")));
}

#[test]
fn failures_remain_visible_after_reloading_durable_tool_records() {
    let mut assistant = Message::new("assistant", "");
    assistant.tool_calls = Some(vec![crate::protocol::ToolCall {
        id: "call-1".into(),
        kind: "function".into(),
        function: crate::protocol::FunctionCall {
            name: "shell".into(),
            arguments: r#"{"command":"cargo test"}"#.into(),
        },
    }]);
    let result = Message::tool_with_status(
        "call-1".into(),
        r#"{"exit_code":101,"stdout":"","stderr":"error[E0599]: broken API"}"#.into(),
        true,
    );
    let state = UiState::new(
        "model".into(),
        "default".into(),
        ThemeStore::load_at(None).unwrap(),
        &[assistant, result],
    );
    let lines = state.history_text(state.themes.colors, 80).lines;
    assert!(
        lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .any(|span| span.content.contains("E0599")),
        "restored failure must not be a bare red cross"
    );
}

#[test]
fn widescreen_centres_the_reading_column_instead_of_gluing_it_left() {
    let mut state = UiState::new(
        "model".into(),
        "default".into(),
        ThemeStore::load_at(None).unwrap(),
        &[Message::new(
            "assistant",
            "| Column | Result |\n|---|---|\n| cargo test | passed |",
        )],
    );
    let mut terminal = Terminal::new(TestBackend::new(210, 35)).unwrap();
    terminal.draw(|frame| state.draw(frame)).unwrap();
    let area = reading_area(Rect::new(0, 0, 210, 35), false);
    assert_eq!(
        area.width, 112,
        "comfortable mode centres a 112-column reading container on widescreen terminals"
    );
    let left = i32::from(area.x);
    let right = 210 - i32::from(area.x) - i32::from(area.width);
    assert!(
        (left - right).abs() <= 1,
        "margins must be symmetric: left {left}, right {right}"
    );
    assert!(state.selection_area.width > 100);
    assert!(state.selection_rows.join("\n").contains("cargo test"));
    let small = reading_area(Rect::new(0, 0, 40, 20), false);
    assert_eq!(small.width, 38);
    assert_eq!(small.x, 1);
    // The full-width reading area fills the terminal, edge to edge.
    let wide = reading_area(Rect::new(0, 0, 210, 35), true);
    assert_eq!(wide.width, 206);
    assert_eq!(wide.x, 2);
}

#[test]
fn only_existing_prose_files_or_valid_urls_are_clickable() {
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};
    let root = std::env::temp_dir().join(format!(
        "kroma-link-view-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/real.rs"), "fn main() {}").unwrap();

    let body = concat!(
        "Available: src/real.rs\n\n",
        "Missing: src/ghost.rs\n\n",
        "`src/real.rs` is an inline code example.\n\n",
        "```shell\ncat src/real.rs\n```\n\n",
        "[nonexistent document](./ghost.md) and https://example.org/help"
    );
    let mut ui = UiState::new(
        "mock".into(),
        "session".into(),
        ThemeStore::load_at(None).unwrap(),
        &[Message::new("assistant", body)],
    );
    ui.set_link_workdir(&root);
    ui.event(UiEvent::ToolStarted {
        name: "shell".into(),
        id: "check".into(),
        args: "cat src/real.rs".into(),
    });
    ui.event(UiEvent::ToolFinished {
        id: "check".into(),
        result: "exit: 0\nsrc/real.rs".into(),
        failed: false,
    });
    rendered_text(&mut ui, 120, 40);
    let links = &ui.visible_links;
    assert_eq!(
        links
            .iter()
            .filter(|link| link.target == "src/real.rs")
            .count(),
        1,
        "only real prose paths should be linkified: {links:?} ROWS={:?}",
        ui.selection_rows
    );
    assert!(links
        .iter()
        .any(|link| link.target == "https://example.org/help"));
    assert!(!links.iter().any(|link| link.target.contains("ghost")));
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn model_failures_are_highlighted_in_header_body_and_final_status() {
    let mut ui = UiState::new(
        "mock".into(),
        "session".into(),
        ThemeStore::load_at(None).unwrap(),
        &[],
    );
    ui.info("ERROR", "Model request failed: HTTP 429");
    ui.task_outcome = Some(TaskOutcome::Failed);
    let theme = ui.themes.colors;
    let text = ui.history_text(theme, 100);
    let heading = text
        .lines
        .iter()
        .find(|row| row.spans.iter().any(|span| span.content.contains("ERROR")))
        .unwrap();
    assert!(heading
        .spans
        .iter()
        .any(|span| span.style.fg == Some(theme.error)));
    let diagnostic = text
        .lines
        .iter()
        .find(|row| {
            row.spans
                .iter()
                .any(|span| span.content.contains("HTTP 429"))
        })
        .unwrap();
    assert!(diagnostic
        .spans
        .iter()
        .any(|span| span.style.fg == Some(theme.error)));
    let status = ui.activity_line(theme);
    assert!(status
        .spans
        .iter()
        .all(|span| span.style.fg == Some(theme.error)));
}

#[test]
fn expanded_tool_and_reasoning_have_distinct_gutters_and_sections() {
    let mut ui = UiState::new(
        "mock".into(),
        "session".into(),
        ThemeStore::load_at(None).unwrap(),
        &[],
    );
    ui.add_user("Fix the tests");
    ui.event(UiEvent::ReasoningDelta("Inspect the build results".into()));
    ui.event(UiEvent::ToolStarted {
        name: "shell".into(),
        id: "build".into(),
        args: "cargo test".into(),
    });
    ui.event(UiEvent::ToolFinished {
        id: "build".into(),
        result: "exit: 1\nstderr: error: test failed".into(),
        failed: true,
    });
    ui.reasoning_expanded = true;
    ui.tools_expanded = true;
    let doc = ui.history_text(ui.themes.colors, 130);
    let text = doc.to_string();
    assert!(text.contains("Reasoning · Ctrl+T to collapse"));
    assert!(text.contains("├─ Command"));
    assert!(text.contains("└─ Output"));
    assert!(text.contains("Inspect the build results"));
    assert!(doc.lines.iter().any(
        |line| line.to_string().contains("Inspect the build results")
            && line
                .spans
                .iter()
                .all(|span| !span.style.add_modifier.contains(Modifier::ITALIC))
    ));
    assert!(
        !text.contains("│  Inspect the build results"),
        "long reasoning must not be rendered as a guttered italic log"
    );
    assert!(text.contains("$ cargo test"));
    assert!(text.contains("error: test failed"));
}

#[test]
fn markdown_and_plain_links_hit_the_correct_screen_cells() {
    let answer = "Reports: [审查报告 SR-001](https://example.org/report) and [复现测试](./src/provider.rs).\n\nFile src/provider.rs:128 and https://rust-lang.org.";
    let mut state = UiState::new(
        "model".into(),
        "default".into(),
        ThemeStore::load_at(None).unwrap(),
        &[Message::new("assistant", answer)],
    );
    let mut terminal = Terminal::new(TestBackend::new(130, 32)).unwrap();
    terminal.draw(|frame| state.draw(frame)).unwrap();
    for (label, target) in [
        ("审查报告 SR-001", "https://example.org/report"),
        ("复现测试", "./src/provider.rs"),
        ("src/provider.rs:128", "src/provider.rs:128"),
        ("https://rust-lang.org", "https://rust-lang.org"),
    ] {
        let row = state
            .selection_rows
            .iter()
            .position(|row| row.contains(label))
            .expect("link label visible");
        let prefix = state.selection_rows[row].split_once(label).unwrap().0;
        let x = state.selection_area.x + UnicodeWidthStr::width(prefix) as u16;
        let y = state.selection_area.y + row as u16;
        assert_eq!(state.link_at(x, y), Some(target), "wrong link for {label}");
        state.hover_link(x, y);
        terminal.draw(|frame| state.draw(frame)).unwrap();
        let cells = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(cells.contains("Ctrl+Click to open"));
    }
    assert!(state
        .link_at(state.selection_area.x, state.selection_area.y)
        .is_none());
}

#[test]
fn clickable_regions_follow_scrolling_without_reusing_stale_coordinates() {
    let mut history = (0..60)
        .map(|i| Message::new("assistant", format!("History item {i}")))
        .collect::<Vec<_>>();
    history.push(Message::new(
        "assistant",
        "[latest report](https://example.org/latest)",
    ));
    let mut state = UiState::new(
        "model".into(),
        "default".into(),
        ThemeStore::load_at(None).unwrap(),
        &history,
    );
    let mut terminal = Terminal::new(TestBackend::new(88, 22)).unwrap();
    terminal.draw(|frame| state.draw(frame)).unwrap();
    let (x, y) = state.test_link_position("example.org/latest").unwrap();
    assert_eq!(state.link_at(x, y), Some("https://example.org/latest"));
    state.scroll(15);
    terminal.draw(|frame| state.draw(frame)).unwrap();
    assert_ne!(state.link_at(x, y), Some("https://example.org/latest"));
    state.scroll_to_bottom();
    terminal.draw(|frame| state.draw(frame)).unwrap();
    assert_eq!(state.link_at(x, y), Some("https://example.org/latest"));
}

#[test]
fn long_literal_url_reconstructs_full_destination_after_wrapping() {
    let url = format!("https://example.org/{}", "very-long-segment".repeat(9));
    let mut state = UiState::new(
        "model".into(),
        "default".into(),
        ThemeStore::load_at(None).unwrap(),
        &[Message::new(
            "assistant",
            format!("Check {url} for details"),
        )],
    );
    let mut terminal = Terminal::new(TestBackend::new(44, 25)).unwrap();
    terminal.draw(|frame| state.draw(frame)).unwrap();
    let partial = state
        .visible_links
        .iter()
        .find(|link| link.target.starts_with("https://example.org/"))
        .expect("start of wrapped URL should be clickable");
    assert!(partial.target.len() < url.len());
    assert_eq!(state.resolved_link(&partial.target), url);
}

#[test]
fn long_named_markdown_link_remains_clickable_across_wrapped_rows() {
    let label = "明细报告测试报告测试报告测试报告";
    let text = format!("Please review [{label}](https://example.org/full-report) now.");
    let mut state = UiState::new(
        "model".into(),
        "default".into(),
        ThemeStore::load_at(None).unwrap(),
        &[Message::new("assistant", text)],
    );
    let mut terminal = Terminal::new(TestBackend::new(34, 24)).unwrap();
    terminal.draw(|frame| state.draw(frame)).unwrap();
    let hits = state
        .visible_links
        .iter()
        .filter(|link| link.target == "https://example.org/full-report")
        .collect::<Vec<_>>();
    assert!(
        hits.len() >= 2,
        "wrapped Markdown link must have click targets on both rows"
    );
    for hit in hits {
        assert_eq!(
            state.link_at(hit.x, hit.y),
            Some("https://example.org/full-report")
        );
    }
}

#[test]
fn searchable_picker_renders_over_history_without_obscuring_editor() {
    let mut state = UiState::new(
        "deepseek".into(),
        "default".into(),
        ThemeStore::load_at(None).unwrap(),
        &[],
    );
    state.editor.paste("draft remains here");
    state.picker = Some(Picker::new(
        super::super::picker::Kind::Commands,
        vec![
            super::super::picker::Item::new("/review", "/review", "command"),
            super::super::picker::Item::new("/resume", "/resume", "command"),
        ],
        "rev",
    ));
    for (width, height) in [(40, 16), (80, 24), (160, 40)] {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| state.draw(frame)).unwrap();
        let buffer = terminal.backend().buffer();
        let lines = buffer
            .content()
            .chunks(width as usize)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>();
        let snapshot = lines.join("\n");
        assert!(
            snapshot.contains("Commands"),
            "picker disappeared at {width}: {snapshot}"
        );
        assert!(
            snapshot.contains("/review"),
            "active item missing at {width}"
        );
        assert!(
            snapshot.contains("draft remains here"),
            "composer obscured at {width}"
        );
    }
}

#[test]
fn status_keeps_model_thinking_and_context_visible_at_eighty_columns() {
    for (width, height) in [(40, 16), (80, 24), (110, 30), (160, 40)] {
        let mut state = UiState::new(
            "deepseek-v4.1-flash".into(),
            "default".into(),
            ThemeStore::load_at(None).unwrap(),
            &[],
        );
        state.configure_status(128_000, Some("high".into()));
        state.event(UiEvent::ActualInput(32_000));
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| state.draw(frame)).unwrap();
        let output = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(
            output.contains("Kroma"),
            "no app identity at {width} columns"
        );
        let expected_context = "Context 25%";
        assert!(
            output.contains(expected_context),
            "no context at {width} columns"
        );
        assert!(
            output.contains("high"),
            "no thinking level at {width} columns"
        );
        if width >= 80 {
            assert!(
                output.contains("deepseek-v4.1-flash"),
                "model must remain visible at {width} columns"
            );
        }
    }
}

#[test]
fn notification_dismisses_without_losing_important_transcript_events() {
    let mut state = UiState::new(
        "model".into(),
        "default".into(),
        ThemeStore::load_at(None).unwrap(),
        &[],
    );
    state.note("Temporary theme feedback");
    state.info("ERROR", "Last request failed with an HTTP error");
    state.dismiss_notification();
    assert!(state.notification.is_empty());
    assert!(state
        .history_text(state.themes.colors, 80)
        .lines
        .iter()
        .flat_map(|line| line.spans.iter())
        .any(|span| span.content.contains("HTTP error")));
}

#[test]
fn final_only_reasoning_inserts_before_streamed_answer() {
    let mut state = UiState::new(
        "test".into(),
        "demo".into(),
        ThemeStore::load_at(None).unwrap(),
        &[],
    );
    state.event(UiEvent::ModelStarted);
    state.event(UiEvent::Text("answer".into()));
    state.event(UiEvent::ReasoningFinal("completed reasoning".into()));
    assert_eq!(state.entries.len(), 2);
    assert!(matches!(state.entries[0].kind, Kind::Reasoning));
    assert_eq!(state.entries[0].body, "completed reasoning");
    assert_eq!(state.entries[1].body, "answer");
}

#[test]
fn mobile_review_transcript_keeps_markdown_hierarchy_visible() {
    let answer = "## 规模与结构\n\n- 17,501 行 Rust，生产代码与测试代码分层清晰。\n- 主函数 `run_with_resume` 后续仍然可以继续优化。\n  - 会话使用 JSON 快照和 WAL。\n\n## 打分\n\n| 维度 | 分数 |\n|---|---|\n| 架构设计 | 9.0 |\n| 测试质量 | 8.5 |\n\n结论：这是一份需要清晰排版的审查报告。";
    let mut state = UiState::new(
        "mock".into(),
        "review".into(),
        ThemeStore::load_at(None).unwrap(),
        &[],
    );
    state.event(UiEvent::Text(answer.into()));
    let mut terminal = Terminal::new(TestBackend::new(70, 38)).unwrap();
    terminal.draw(|frame| state.draw(frame)).unwrap();
    // Ratatui's terminal buffer includes blank continuation cells after
    // double-width CJK characters. The cached visible rows preserve the
    // original text without inserting those artificial spaces.
    let rows = &state.selection_rows;
    let section = rows
        .iter()
        .position(|line| line.contains("规模与结构"))
        .unwrap();
    assert!(
        rows[section + 1].trim().is_empty(),
        "heading should be separated from content"
    );
    assert!(
        rows.iter().any(|line| line.contains("run_with_resume")),
        "code identifier must remain readable"
    );
    assert!(
        rows.iter()
            .any(|line| line.contains("架构设计") && line.contains("9.0")),
        "score cells must share a row"
    );
    assert!(rows.iter().any(|line| line.contains("结论")));
}

#[test]
fn markdown_code_keeps_semantic_indentation_across_themes() {
    let source = "```python\nif ready:\n    run()\n    if nested:\n        finish()\n```\n\n```yaml\nroot:\n  child:\n    leaf: 中文\n```\n\n    alpha\n            beta";
    for preset in ["dark", "light", "mono"] {
        let mut store = ThemeStore::load_at(None).unwrap();
        store.select(preset).unwrap();
        let mut state = UiState::new(
            "mock".into(),
            "review".into(),
            store,
            &[Message::new("assistant", source)],
        );
        let _ = rendered_text(&mut state, 100, 32);
        let rows = state.selection_rows.clone();
        let row = |needle: &str| {
            rows.iter()
                .position(|line| line.contains(needle))
                .unwrap_or_else(|| panic!("missing {needle:?} in {preset} transcript: {rows:#?}"))
        };
        assert_eq!(rows[row("if ready:")], "      if ready:");
        assert_eq!(rows[row("run()")], "          run()");
        assert_eq!(rows[row("if nested:")], "          if nested:");
        assert_eq!(rows[row("finish()")], "              finish()");
        assert_eq!(rows[row("leaf: 中文")], "          leaf: 中文");
        assert_eq!(rows[row("alpha")], "      alpha");
        assert_eq!(rows[row("beta")], "              beta");
    }
}

#[test]
fn long_table_cells_fall_back_and_wrap_without_overrunning_the_terminal() {
    let long = "x".repeat(120);
    let source = format!(
        "| 名称 | 状态 | 数量 | 备注 | 负责人 |\n|---|---|---|---|---|\n| 构建 | 通过 |  | {long} | 张三 |"
    );
    let mut state = UiState::new(
        "mock".into(),
        "review".into(),
        ThemeStore::load_at(None).unwrap(),
        &[Message::new("assistant", source)],
    );
    let _ = rendered_text(&mut state, 60, 24);
    let rows = state.selection_rows.clone();
    let text = rows.join("\n");
    assert!(text.contains("备注:"), "long table falls back: {rows:#?}");
    assert!(
        text.contains("xxxxxxxxxx"),
        "long cell stays visible: {rows:#?}"
    );
    assert!(
        rows.iter().all(
            |line| UnicodeWidthStr::width(line.as_str()) <= state.selection_area.width as usize
        ),
        "wrapped rows must not overrun the terminal: {rows:#?}"
    );
}

#[test]
fn test_backend_buffer_shows_code_indentation_list_numbers_and_wide_tables() {
    let source = "```python\nif ready:\n    run()\n```\n\n3. Third\n4. Fourth\n\n| A | B | C | D | E |\n|---|---|---|---|---|\n| 1 | 2 | 3 | 4 | 5 |";
    let mut state = UiState::new(
        "mock".into(),
        "review".into(),
        ThemeStore::load_at(None).unwrap(),
        &[Message::new("assistant", source)],
    );
    let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
    terminal.draw(|frame| state.draw(frame)).unwrap();
    let screen = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(screen.contains("    │     run()"), "{screen}");
    assert!(screen.contains("3. Third"), "{screen}");
    assert!(screen.contains("4. Fourth"), "{screen}");
    assert!(screen.contains("A │ B │ C │ D │ E"), "{screen}");
}

#[test]
fn code_whitespace_emoji_and_tabs_survive_the_render_pipeline() {
    let mut state = UiState::new(
        "mock".into(),
        "review".into(),
        ThemeStore::load_at(None).unwrap(),
        &[Message::new(
            "assistant",
            "```python\ndef greet():\n    \n    print(\"你好 👋\")\n```",
        )],
    );
    let _ = rendered_text(&mut state, 80, 24);
    let rows = state.selection_rows.clone();
    let blank = rows
        .iter()
        .position(|line| line == "          ")
        .expect("a whitespace-only code row keeps its four content spaces");
    assert!(
        rows[blank + 1].contains("      print(\"你好 👋\")"),
        "unicode code indentation: {rows:#?}"
    );
}

#[test]
fn code_block_blank_lines_are_not_merged_or_trimmed() {
    let mut state = UiState::new(
        "mock".into(),
        "review".into(),
        ThemeStore::load_at(None).unwrap(),
        &[Message::new(
            "assistant",
            "```text\n\nfirst\n\n\nlast\n\n\n```",
        )],
    );
    let _ = rendered_text(&mut state, 80, 24);
    let code_rows = state
        .selection_rows
        .iter()
        .filter(|line| line.starts_with("      "))
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(
        code_rows,
        vec![
            "      ",
            "      first",
            "      ",
            "      ",
            "      last",
            "      ",
            "      ",
        ]
    );
}

#[test]
fn narrow_code_wrap_preserves_indent_and_continuation_gutter() {
    let mut state = UiState::new(
        "mock".into(),
        "review".into(),
        ThemeStore::load_at(None).unwrap(),
        &[Message::new(
            "assistant",
            "```python\n        ALPHA_BETA_GAMMA_DELTA = \"0123456789abcdefghijklmnop\"\n```",
        )],
    );
    let _ = rendered_text(&mut state, 40, 30);
    let rows = state.selection_rows.clone();
    let first = rows
        .iter()
        .position(|line| line.contains("ALPHA_BETA_GAMMA_DELTA"))
        .expect("wrapped code row");
    assert!(
        rows[first].starts_with("              ALPHA_BETA_GAMMA_DELTA"),
        "source indentation must survive a narrow first row: {rows:#?}"
    );
    let reconstructed = rows[first..]
        .iter()
        .take_while(|row| !row.is_empty())
        .map(|row| {
            row.strip_prefix("      ")
                .expect("every code continuation aligns under the gutter")
        })
        .collect::<String>();
    assert_eq!(
        reconstructed, "        ALPHA_BETA_GAMMA_DELTA = \"0123456789abcdefghijklmnop\"",
        "wrapping must preserve every source space: {rows:#?}"
    );
}

#[test]
fn expanded_tool_output_keeps_leading_indentation() {
    let mut state = UiState::new(
        "mock".into(),
        "review".into(),
        ThemeStore::load_at(None).unwrap(),
        &[],
    );
    state.event(UiEvent::ToolStarted {
        name: "shell".into(),
        id: "indent".into(),
        args: "cat fixture.txt".into(),
    });
    state.event(UiEvent::ToolFinished {
        id: "indent".into(),
        result:
            "exit: 0\n    indented output\n        deeper output\n中文 👩‍💻\n    \n\ttabbed output"
                .into(),
        failed: false,
    });
    state.tools_expanded = true;
    let _ = rendered_text(&mut state, 100, 32);
    let rows = state.selection_rows.clone();
    assert!(
        rows.iter().any(|line| line == "          indented output"),
        "expanded tool stdout indentation: {rows:#?}"
    );
    assert!(
        rows.iter()
            .any(|line| line == "              deeper output"),
        "nested tool stdout indentation: {rows:#?}"
    );
    assert!(
        rows.iter().any(|line| line.contains("中文 👩‍💻")),
        "unicode tool output remains visible: {rows:#?}"
    );
    assert!(
        rows.iter().any(|line| line == "          "),
        "whitespace-only tool output row remains distinguishable: {rows:#?}"
    );
    assert!(
        rows.iter().any(|line| line == "          tabbed output"),
        "tool-output tabs use deterministic four-cell stops: {rows:#?}"
    );
    assert!(rows.iter().all(|line| !line.contains('\t')));
}

#[test]
fn copied_code_selection_keeps_leading_indentation() {
    let mut state = UiState::new(
        "mock".into(),
        "review".into(),
        ThemeStore::load_at(None).unwrap(),
        &[Message::new(
            "assistant",
            "```python\nif ready:\n    run()\n```",
        )],
    );
    let _ = rendered_text(&mut state, 80, 24);
    let rows = state.selection_rows.clone();
    let row = rows
        .iter()
        .position(|line| line.contains("run()"))
        .expect("code row");
    let content_start = rows[row].find("    run()").unwrap();
    let x = state.selection_area.x + UnicodeWidthStr::width(&rows[row][..content_start]) as u16;
    let y = state.selection_area.y + row as u16;
    state.select_start(x, y);
    state.select_drag(state.selection_area.right() - 1, y);
    state.mouse_release();
    assert_eq!(state.selected_text().as_deref(), Some("    run()"));
}

#[test]
fn ordered_list_marker_and_soft_wrap_alignment_survive_the_render_pipeline() {
    let mut state = UiState::new(
        "mock".into(),
        "review".into(),
        ThemeStore::load_at(None).unwrap(),
        &[Message::new(
            "assistant",
            "9. 第九项包含较长的中文说明，需要软换行并保持编号和正文对齐。\n10. 第十项包含较长的中文说明，需要软换行并保持编号和正文对齐。",
        )],
    );
    let _ = rendered_text(&mut state, 38, 24);
    let rows = state.selection_rows.clone();
    let first = rows
        .iter()
        .position(|line| line.contains("9. 第九项"))
        .expect("single-digit ordered item marker");
    assert!(
        rows[first + 1].starts_with("       "),
        "single-digit continuation aligns under the item text: {rows:#?}"
    );
    let tenth = rows
        .iter()
        .position(|line| line.contains("10. 第十项"))
        .expect("multi-digit ordered item marker");
    assert!(
        rows[tenth + 1].starts_with("        "),
        "two-digit continuation shifts by one cell with the marker: {rows:#?}"
    );
}

#[test]
fn five_column_table_stays_aligned_until_the_terminal_is_narrow() {
    let source = "| 名称 | 状态 | 数量 | 备注 | 负责人 |\n|---|---|---|---|---|\n| 构建 | 通过 |  | 空 | 张三 |";
    let mut state = UiState::new(
        "mock".into(),
        "review".into(),
        ThemeStore::load_at(None).unwrap(),
        &[Message::new("assistant", source)],
    );
    let _ = rendered_text(&mut state, 100, 24);
    let wide = state.selection_rows.clone();
    assert!(
        wide.iter()
            .any(|line| line.contains("名称") && line.contains("负责人")),
        "five columns fit: {wide:#?}"
    );
    assert!(
        wide.iter()
            .any(|line| line.contains("构建") && line.contains("张三")),
        "five-column data stays on one row: {wide:#?}"
    );
    let _ = rendered_text(&mut state, 30, 24);
    let narrow = state.selection_rows.clone();
    assert!(
        !narrow
            .iter()
            .any(|line| line.contains("名称") && line.contains("负责人")),
        "narrow fallback avoids an overflowing table: {narrow:#?}"
    );
    assert!(
        narrow.iter().any(|line| line.contains("负责人: 张三")),
        "narrow fallback keeps labeled values readable: {narrow:#?}"
    );
}

#[test]
fn wrapping_keeps_short_english_identifiers_intact_under_bullets() {
    let text = Text::from(vec![Line::from(vec![
        Span::raw("   "),
        Span::raw("• "),
        Span::raw("inspect run_with_resume now"),
    ])]);
    let wrapped = wrap_history(text, 25).0;
    let rows = wrapped
        .lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>();
    assert!(rows.len() >= 2, "the sample must wrap: {rows:?}");
    assert!(
        !rows[0].contains("run_"),
        "do not split the identifier: {rows:?}"
    );
    assert!(
        rows[1].contains("run_with_resume"),
        "keep the word together: {rows:?}"
    );
    assert!(
        rows[1].starts_with("     "),
        "align under bullet content: {rows:?}"
    );
}

#[test]
fn history_wrapping_keeps_zwj_emoji_as_one_visible_character() {
    let line = Line::raw(format!("{}👩‍💻z", "a".repeat(6)));
    let wrapped = wrap_history(Text::from(vec![line]), 8).0;
    let rendered = wrapped
        .lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>();
    assert_eq!(rendered[0], "aaaaaa👩‍💻");
    assert_eq!(rendered[1], "z");
}

#[test]
fn history_render_cache_is_reused_until_content_changes() {
    let messages = vec![Message::new("user", "A stable history entry")];
    let mut state = UiState::new(
        "mock".into(),
        "test".into(),
        ThemeStore::load_at(None).unwrap(),
        &messages,
    );
    let mut terminal = Terminal::new(TestBackend::new(92, 25)).unwrap();
    terminal.draw(|frame| state.draw(frame)).unwrap();
    let first = state.history_cache.as_ref().unwrap().text.lines.as_ptr();
    terminal.draw(|frame| state.draw(frame)).unwrap();
    assert_eq!(
        first,
        state.history_cache.as_ref().unwrap().text.lines.as_ptr()
    );
    state.event(UiEvent::Text("Changed".into()));
    assert!(state.history_cache.is_none());
    terminal.draw(|frame| state.draw(frame)).unwrap();
    let rendered = state
        .history_cache
        .as_ref()
        .unwrap()
        .text
        .lines
        .iter()
        .flat_map(|line| line.spans.iter().map(|span| span.content.as_ref()))
        .collect::<String>();
    assert!(rendered.contains("Changed"));
}

#[test]
fn composer_mouse_selection_is_visible_and_copies_unicode_exactly() {
    let mut ui = UiState::new(
        "mock".into(),
        "draft".into(),
        ThemeStore::load_at(None).unwrap(),
        &[],
    );
    ui.editor.paste("hello 猫 👩‍💻 world");
    let mut terminal = Terminal::new(TestBackend::new(100, 28)).unwrap();
    terminal.draw(|frame| ui.draw(frame)).unwrap();
    let area = ui.editor_area;
    assert!(area.width > 20 && area.height > 0);
    assert!(ui.start_editor_selection(area.x, area.y));
    assert!(ui.drag_editor_selection(area.x + 6, area.y));
    ui.mouse_release();
    assert_eq!(ui.editor_selected_text().as_deref(), Some("hello "));
    assert_eq!(
        ui.selected_text_at(area.x, area.y).as_deref(),
        Some("hello ")
    );
    assert!(
        ui.selected_text().is_none(),
        "transcript selection remains independent"
    );
    terminal.draw(|frame| ui.draw(frame)).unwrap();
    let selected = terminal.backend().buffer().cell((area.x, area.y)).unwrap();
    assert!(
        selected.modifier.contains(Modifier::REVERSED),
        "draft selection should visibly highlight the chosen text"
    );
    ui.clear_editor_selection();
    assert!(ui.editor_selected_text().is_none());
    assert_eq!(ui.editor.text(), "hello 猫 👩‍💻 world");
}

#[test]
fn long_draft_does_not_jump_when_drag_selection_starts() {
    let mut ui = UiState::new(
        "mock".into(),
        "draft".into(),
        ThemeStore::load_at(None).unwrap(),
        &[],
    );
    ui.editor.paste(
        &(0..20)
            .map(|i| format!("line {i:02}\n"))
            .collect::<Vec<_>>()
            .concat(),
    );
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal.draw(|frame| ui.draw(frame)).unwrap();
    let area = ui.editor_area;
    let before = ui.editor.viewport(area.width, area.height).0;
    assert!(ui.start_editor_selection(area.x, area.y));
    assert_eq!(before, ui.editor.viewport(area.width, area.height).0);
    assert!(ui.drag_editor_selection(area.x + 4, area.y + 1));
    assert_eq!(before, ui.editor.viewport(area.width, area.height).0);
    ui.mouse_release();
    assert!(ui.editor_selected_text().is_some());
}

#[test]
fn mouse_selection_copies_exact_visible_transcript_cells() {
    let history = vec![Message::new("user", "inspect hello world")];
    let mut state = UiState::new(
        "mock".into(),
        "test".into(),
        ThemeStore::load_at(None).unwrap(),
        &history,
    );
    let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
    terminal.draw(|frame| state.draw(frame)).unwrap();
    let area = state.selection_area;
    let position = state
        .selection_rows
        .iter()
        .position(|line| line.contains("inspect hello world"))
        .unwrap();
    let row = area.y + position as u16;
    let line = &state.selection_rows[position];
    let start = line.find("hello").unwrap() as u16;
    state.select_start(area.x + start, row);
    state.select_drag(area.x + start + 4, row);
    assert_eq!(state.selected_text().as_deref(), Some("hello"));
    terminal.draw(|frame| state.draw(frame)).unwrap();
    assert_eq!(state.selected_text().as_deref(), Some("hello"));
}

#[test]
fn scrollbar_thumb_can_be_dragged_between_earliest_and_latest_history() {
    let messages = (0..120)
        .map(|i| Message::new("assistant", format!("Log line {i:03}")))
        .collect::<Vec<_>>();
    let mut state = UiState::new(
        "mock".into(),
        "test".into(),
        ThemeStore::load_at(None).unwrap(),
        &messages,
    );
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal.draw(|frame| state.draw(frame)).unwrap();
    assert!(state.history_max_scroll > 0);
    let track = state.scrollbar_area;
    assert!(track.height > 5);
    let (position, thumb) = state.scrollbar_geometry();
    assert!(thumb < track.height as usize);
    assert!(state.scrollbar_press(track.x, track.y + position as u16));
    assert!(state.scrollbar_move(track.y));
    terminal.draw(|frame| state.draw(frame)).unwrap();
    assert_eq!(state.history_top_row, 0);
    assert!(state.selection_rows.join("\n").contains("Log line 000"));
    assert!(state.scrollbar_move(track.bottom() - 1));
    terminal.draw(|frame| state.draw(frame)).unwrap();
    assert_eq!(state.scroll_from_bottom, 0);
    assert!(state.selection_rows.join("\n").contains("Log line 119"));
    state.mouse_release();
    assert!(!state.scrollbar_move(track.y));
    assert!(!state.scrollbar_press(track.x.saturating_sub(3), track.y));
}

#[test]
fn scrollbar_can_reach_entries_older_than_initial_lazy_history_window() {
    let messages = (0..440)
        .map(|i| Message::new("assistant", format!("Timeline {i:03}")))
        .collect::<Vec<_>>();
    let mut state = UiState::new(
        "mock".into(),
        "test".into(),
        ThemeStore::load_at(None).unwrap(),
        &messages,
    );
    let mut terminal = Terminal::new(TestBackend::new(90, 24)).unwrap();
    terminal.draw(|frame| state.draw(frame)).unwrap();
    assert_eq!(state.history_visible, 300);
    let track = state.scrollbar_area;
    assert!(state.scrollbar_press(track.x, track.y));
    terminal.draw(|frame| state.draw(frame)).unwrap();
    assert_eq!(state.history_visible, messages.len());
    assert_eq!(state.history_top_row, 0);
    assert!(state.selection_rows.join("\n").contains("Timeline 000"));
    state.mouse_release();
    state.scroll_to_bottom();
    terminal.draw(|frame| state.draw(frame)).unwrap();
    assert!(state.selection_rows.join("\n").contains("Timeline 439"));
}

#[test]
fn drag_selection_autoscrolls_and_copies_across_multiple_viewports() {
    let messages = (0..80)
        .map(|i| Message::new("assistant", format!("Excerpt {i:03}")))
        .collect::<Vec<_>>();
    let mut state = UiState::new(
        "mock".into(),
        "test".into(),
        ThemeStore::load_at(None).unwrap(),
        &messages,
    );
    let mut terminal = Terminal::new(TestBackend::new(80, 18)).unwrap();
    terminal.draw(|frame| state.draw(frame)).unwrap();
    state.scroll(state.history_max_scroll as isize);
    terminal.draw(|frame| state.draw(frame)).unwrap();
    assert_eq!(state.history_top_row, 0);
    let area = state.selection_area;
    state.select_start(area.x + 3, area.y + 2);
    state.select_drag(area.x + 4, area.bottom() - 1);
    assert!(state.selection_needs_auto_scroll());
    let first_end = state.selection_end.unwrap().0;
    for _ in 0..40 {
        assert!(state.auto_scroll_selection());
        terminal.draw(|frame| state.draw(frame)).unwrap();
    }
    assert!(state.selection_end.unwrap().0 > first_end + 15);
    assert!(state.selected_text().unwrap().contains("Excerpt 012"));
    assert!(state.selected_text().unwrap().contains("Excerpt 025"));
    state.mouse_release();
    assert!(!state.selection_needs_auto_scroll());
    assert!(!state.auto_scroll_selection());
    assert!(state.selected_text().unwrap().contains("Excerpt 025"));
}

#[test]
fn selection_can_auto_scroll_upward_and_keeps_earlier_text() {
    let messages = (0..80)
        .map(|i| Message::new("assistant", format!("Earlier {i:03}")))
        .collect::<Vec<_>>();
    let mut state = UiState::new(
        "mock".into(),
        "test".into(),
        ThemeStore::load_at(None).unwrap(),
        &messages,
    );
    let mut terminal = Terminal::new(TestBackend::new(80, 18)).unwrap();
    terminal.draw(|frame| state.draw(frame)).unwrap();
    let area = state.selection_area;
    state.select_start(area.x + 2, area.bottom() - 3);
    let start_row = state.selection_start.unwrap().0;
    state.select_drag(area.x + 2, area.y);
    assert!(state.selection_needs_auto_scroll());
    for _ in 0..25 {
        assert!(state.auto_scroll_selection());
        terminal.draw(|frame| state.draw(frame)).unwrap();
    }
    assert!(state.selection_end.unwrap().0 < start_row - 20);
    assert!(state.selected_text().unwrap().lines().count() > 20);
    state.mouse_release();
    assert!(!state.auto_scroll_selection());
}

#[test]
fn scrollbar_drag_does_not_replace_existing_selection() {
    let messages = (0..45)
        .map(|i| Message::new("assistant", format!("Record {i:03}")))
        .collect::<Vec<_>>();
    let mut state = UiState::new(
        "mock".into(),
        "test".into(),
        ThemeStore::load_at(None).unwrap(),
        &messages,
    );
    let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
    terminal.draw(|frame| state.draw(frame)).unwrap();
    let area = state.selection_area;
    let row = state
        .selection_rows
        .iter()
        .position(|line| line.contains("Record 044"))
        .unwrap();
    state.select_start(area.x + 3, area.y + row as u16);
    state.select_drag(area.x + 8, area.y + row as u16);
    let selected = state.selected_text().unwrap();
    state.mouse_release();
    let x = state.scrollbar_area.x;
    let y = state.scrollbar_area.y;
    assert!(state.scrollbar_press(x, y));
    terminal.draw(|frame| state.draw(frame)).unwrap();
    assert_eq!(state.selected_text().as_deref(), Some(selected.as_str()));
}
#[test]
fn scrolling_pins_visible_history_while_streaming_then_returns_to_latest() {
    let messages = (0..60)
        .map(|i| Message::new("assistant", format!("existing line {i:03}")))
        .collect::<Vec<_>>();
    let mut state = UiState::new(
        "test".into(),
        "default".into(),
        ThemeStore::load_at(None).unwrap(),
        &messages,
    );
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal.draw(|frame| state.draw(frame)).unwrap();
    state.scroll(12);
    terminal.draw(|frame| state.draw(frame)).unwrap();
    let first_visible = state.selection_rows.join("\n");
    for _ in 0..100 {
        state.event(UiEvent::Text(" streamed output\n".into()));
        terminal.draw(|frame| state.draw(frame)).unwrap();
    }
    assert_eq!(
        first_visible,
        state.selection_rows.join("\n"),
        "new output must not move the reading viewport"
    );
    state.scroll_to_bottom();
    terminal.draw(|frame| state.draw(frame)).unwrap();
    assert!(state.selection_rows.join("\n").contains("streamed output"));
    assert!(state.scroll_anchor.is_none());
}

#[test]
fn copy_selection_never_breaks_a_combined_emoji_cluster() {
    let mut state = UiState::new(
        "test".into(),
        "default".into(),
        ThemeStore::load_at(None).unwrap(),
        &[Message::new("user", "👩‍💻")],
    );
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal.draw(|frame| state.draw(frame)).unwrap();
    let row = state
        .selection_rows
        .iter()
        .position(|line| line.contains("👩‍💻"))
        .unwrap();
    let text = &state.selection_rows[row];
    let cells = text.split_once("👩‍💻").unwrap().0;
    let x = state.selection_area.x + UnicodeWidthStr::width(cells) as u16;
    let y = state.selection_area.y + row as u16;
    state.select_start(x, y);
    state.select_drag(x + 1, y);
    assert_eq!(state.selected_text().as_deref(), Some("👩‍💻"));
}

#[test]
fn borrowed_viewport_renders_the_same_cells_as_original_paragraph() {
    let messages = vec![
        Message::new("user", "你好，检查 Unicode 和 emoji"),
        Message::new(
            "assistant",
            "## Results\n\n**Passed**: ✅ 3 tests\n\n- Cargo checks\n- 宽字符和组合字形 e\u{301}\n\n`fn main() {}`",
        ),
    ];
    let mut state = UiState::new(
        "mock".into(),
        "test".into(),
        ThemeStore::load_at(None).unwrap(),
        &messages,
    );
    let mut actual = Terminal::new(TestBackend::new(88, 25)).unwrap();
    actual.draw(|frame| state.draw(frame)).unwrap();
    let area = state.selection_area;
    let end = (state.history_top_row + area.height as usize)
        .min(state.history_cache.as_ref().unwrap().text.lines.len());
    let original_lines =
        state.history_cache.as_ref().unwrap().text.lines[state.history_top_row..end].to_vec();
    let theme = state.themes.colors;
    let mut reference = Terminal::new(TestBackend::new(88, 25)).unwrap();
    reference
        .draw(|frame| {
            frame.render_widget(
                Block::default().style(Style::default().bg(theme.background)),
                frame.area(),
            );
            frame.render_widget(
                Paragraph::new(Text::from(original_lines))
                    .style(Style::default().fg(theme.text).bg(theme.background)),
                area,
            );
            let cache = state.history_cache.as_ref().unwrap();
            for (row, item) in &cache.decor {
                if *row < state.history_top_row || *row >= end {
                    continue;
                }
                let y = area.y + (*row - state.history_top_row) as u16;
                for x in area.x..area.right() {
                    let cell = frame.buffer_mut().cell_mut((x, y)).unwrap();
                    match item {
                        Decor::Surface(_) => {
                            cell.set_bg(theme.surface);
                        }
                        Decor::Rail { column, color, .. } if x == area.x + column => {
                            cell.set_symbol(" ").set_bg(*color);
                        }
                        Decor::Gutter {
                            column,
                            glyph,
                            color,
                            ..
                        } if x == area.x + column => {
                            cell.set_symbol(glyph).set_fg(*color);
                        }
                        _ => {}
                    }
                }
            }
        })
        .unwrap();
    for y in area.y..area.bottom() {
        for x in area.x..area.right() {
            let real = actual.backend().buffer().cell((x, y)).unwrap();
            let previous = reference.backend().buffer().cell((x, y)).unwrap();
            assert_eq!(
                (real.symbol(), real.fg, real.bg, real.modifier),
                (
                    previous.symbol(),
                    previous.fg,
                    previous.bg,
                    previous.modifier
                ),
                "viewport cell ({x},{y}) changed"
            );
        }
    }
}

#[test]
#[ignore = "TUI draw microbenchmark; run manually for frame timings"]
fn bench_streaming_frame_latency_three_hundred_and_three_thousand_messages() {
    use std::time::Instant;

    fn percentile(samples: &[u128], percent: usize) -> u128 {
        let index = (samples.len() * percent / 100).min(samples.len() - 1);
        samples[index]
    }

    for count in [300, 3000] {
        let messages = (0..count).map(|i| Message::new("assistant",
            format!("### Task {i}\n\nA short description, a path `src/provider.rs` and a result.\n\n- note one\n- note two")))
            .collect::<Vec<_>>();
        let mut state = UiState::new(
            "model".into(),
            "default".into(),
            ThemeStore::load_at(None).unwrap(),
            &messages,
        );
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| state.draw(frame)).unwrap();
        let mut samples = Vec::new();
        for _ in 0..60 {
            let start = Instant::now();
            state.event(UiEvent::Text(" partial streaming response".into()));
            terminal.draw(|frame| state.draw(frame)).unwrap();
            samples.push(start.elapsed().as_micros());
        }
        samples.sort_unstable();
        eprintln!(
            "TUI scenario=streaming-window messages={count} terminal=80x24 frames=60 median={}us p95={}us",
            percentile(&samples, 50),
            percentile(&samples, 95)
        );
    }

    let messages = (0..3000)
        .map(|i| {
            Message::new(
                "assistant",
                format!("### Task {i}\n\nA short description and `src/provider.rs`.\n\n- note one\n- note two"),
            )
        })
        .collect::<Vec<_>>();
    let mut state = UiState::new(
        "model".into(),
        "default".into(),
        ThemeStore::load_at(None).unwrap(),
        &messages,
    );
    state.history_visible = messages.len();
    let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
    let mut samples = Vec::new();
    for _ in 0..8 {
        state.history_cache = None;
        let start = Instant::now();
        terminal.draw(|frame| state.draw(frame)).unwrap();
        samples.push(start.elapsed().as_micros());
    }
    samples.sort_unstable();
    eprintln!(
        "TUI scenario=all-history messages=3000 terminal=100x30 forced-cache-rebuilds=8 median={}us p95={}us",
        percentile(&samples, 50),
        percentile(&samples, 95)
    );

    let details = (0..2400)
        .map(|i| format!("    indented output {i} 中文 👩‍💻\n        deeper {i}\n"))
        .collect::<Vec<_>>()
        .concat();
    let detail_line_count = details.lines().count();
    assert_eq!(detail_line_count, 4800);
    let mut state = UiState::new(
        "model".into(),
        "default".into(),
        ThemeStore::load_at(None).unwrap(),
        &[],
    );
    state.event(UiEvent::ToolStarted {
        name: "shell".into(),
        id: "bench-output".into(),
        args: "cat large.log".into(),
    });
    state.event(UiEvent::ToolFinished {
        id: "bench-output".into(),
        result: details,
        failed: false,
    });
    state.tools_expanded = true;
    let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
    let mut samples = Vec::new();
    for _ in 0..12 {
        state.history_cache = None;
        let start = Instant::now();
        terminal.draw(|frame| state.draw(frame)).unwrap();
        samples.push(start.elapsed().as_micros());
    }
    samples.sort_unstable();
    eprintln!(
        "TUI scenario=expanded-tool-output lines={detail_line_count} terminal=100x30 forced-cache-rebuilds=12 median={}us p95={}us",
        percentile(&samples, 50),
        percentile(&samples, 95)
    );

    let code = (0..900)
        .map(|i| {
            if i % 3 == 0 {
                format!("    let value_{i} = \"中文 👩‍💻\";")
            } else if i % 3 == 1 {
                format!("        nested_call_{i}();")
            } else {
                format!("// line {i}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    let mut state = UiState::new(
        "model".into(),
        "default".into(),
        ThemeStore::load_at(None).unwrap(),
        &[Message::new("assistant", format!("```rust\n{code}\n```"))],
    );
    let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
    let mut samples = Vec::new();
    for _ in 0..12 {
        state.history_cache = None;
        let start = Instant::now();
        terminal.draw(|frame| state.draw(frame)).unwrap();
        samples.push(start.elapsed().as_micros());
    }
    samples.sort_unstable();
    eprintln!(
        "TUI scenario=long-code-block lines=900 terminal=100x30 forced-cache-rebuilds=12 median={}us p95={}us",
        percentile(&samples, 50),
        percentile(&samples, 95)
    );
}

#[test]
fn older_history_is_reachable_without_rendering_it_all_initially() {
    let messages: Vec<_> = (0..700)
        .map(|i| Message::new("user", format!("history item {i}")))
        .collect();
    let mut state = UiState::new(
        "mock".into(),
        "test".into(),
        ThemeStore::load_at(None).unwrap(),
        &messages,
    );
    assert_eq!(state.entries.len(), 700);
    assert_eq!(state.history_visible, 300);
    for _ in 0..3 {
        state.scroll(50);
    }
    assert_eq!(state.history_visible, 700);
    let rendered = state
        .history_text(state.themes.colors, 80)
        .lines
        .iter()
        .flat_map(|line| line.spans.iter().map(|span| span.content.as_ref()))
        .collect::<String>();
    assert!(rendered.contains("history item 0"));
}
#[test]
fn zero_and_tiny_terminal_draws_are_safe() {
    let themes = ThemeStore::load_at(None).unwrap();
    let mut ui = UiState::new("model".into(), "test".into(), themes, &[]);
    for (width, height) in [(0, 0), (0, 10), (12, 0), (1, 1), (15, 7), (19, 20)] {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| ui.draw(frame)).unwrap();
        assert_eq!(ui.viewport_height, 0);
        assert!(ui.visible_links.is_empty());
    }
    let mut normal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    normal.draw(|frame| ui.draw(frame)).unwrap();
    assert!(
        ui.viewport_height > 0,
        "normal layout must recover after resize"
    );
}

#[test]
fn command_output_is_visible_after_scrolling_old_history() {
    let messages = (0..90)
        .map(|i| Message::new("assistant", format!("Historical item {i:03}")))
        .collect::<Vec<_>>();
    let mut ui = UiState::new(
        "mock".into(),
        "review".into(),
        ThemeStore::load_at(None).unwrap(),
        &messages,
    );
    let mut terminal = Terminal::new(TestBackend::new(82, 24)).unwrap();
    terminal.draw(|frame| ui.draw(frame)).unwrap();
    ui.scroll(45);
    terminal.draw(|frame| ui.draw(frame)).unwrap();
    assert!(ui.scroll_anchor.is_some());
    ui.info("Result", "FRESH_INFORMATION_AFTER_SCROLL");
    terminal.draw(|frame| ui.draw(frame)).unwrap();
    assert!(
        ui.selection_rows
            .join("\n")
            .contains("FRESH_INFORMATION_AFTER_SCROLL"),
        "new command output must be visible instead of remaining pinned to old history"
    );
}

#[test]
fn resize_preserves_logical_history_line_instead_of_old_wrapped_row() {
    let messages = (0..85)
        .map(|i| {
            Message::new(
                "user",
                format!("HISTORY_{i:03} {}", "long_words ".repeat(17)),
            )
        })
        .collect::<Vec<_>>();
    let mut ui = UiState::new(
        "mock".into(),
        "review".into(),
        ThemeStore::load_at(None).unwrap(),
        &messages,
    );
    let mut wide = Terminal::new(TestBackend::new(115, 25)).unwrap();
    wide.draw(|frame| ui.draw(frame)).unwrap();
    ui.scroll(96);
    wide.draw(|frame| ui.draw(frame)).unwrap();
    let original_source = ui.history_cache.as_ref().unwrap().source_lines[ui.history_top_row];
    assert!(original_source > 10);
    let mut narrow = Terminal::new(TestBackend::new(53, 25)).unwrap();
    narrow.draw(|frame| ui.draw(frame)).unwrap();
    let narrow_source = ui.history_cache.as_ref().unwrap().source_lines[ui.history_top_row];
    assert_eq!(
        narrow_source, original_source,
        "resize should preserve the source line"
    );
    wide.draw(|frame| ui.draw(frame)).unwrap();
    let restored_source = ui.history_cache.as_ref().unwrap().source_lines[ui.history_top_row];
    assert_eq!(
        restored_source, original_source,
        "round-trip reflow should keep context"
    );
}

#[test]
fn legacy_attachment_markers_stop_at_private_use_boundary() {
    let mut user = Message::new("user", "old session with too many attachments");
    user.provider_items = Some(
        (0..6_402)
            .map(|i| {
                serde_json::json!({
                    "type":"kroma_attachment","kind":"log",
                    "path":format!("/missing/{i}.log"),"name":format!("log-{i}"),
                })
            })
            .collect(),
    );
    let mut ui = UiState::new(
        "mock".into(),
        "review".into(),
        ThemeStore::load_at(None).unwrap(),
        &[user],
    );
    // Older, unmarked attachments must remain in restored history even
    // when the 6400 private-use marker slots have been exhausted.
    assert_eq!(ui.entries.len(), 1);
    ui.editor.key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Up,
        crossterm::event::KeyModifiers::NONE,
    ));
    let parts = ui.editor.history_attachments();
    assert_eq!(parts.len(), 6_402);
    assert_eq!(
        parts
            .iter()
            .filter(|part| crate::attachments::marker(part).is_some())
            .count(),
        6_400
    );
    assert!(parts[6_400]["marker"].is_null());
}

#[test]
fn resize_clears_invalid_selection_without_copying_wrong_text() {
    let answer = format!("{} TARGET {}", "prefix".repeat(7), "suffix".repeat(12));
    let mut ui = UiState::new(
        "mock".into(),
        "review".into(),
        ThemeStore::load_at(None).unwrap(),
        &[Message::new("assistant", answer)],
    );
    let mut wide = Terminal::new(TestBackend::new(110, 24)).unwrap();
    wide.draw(|frame| ui.draw(frame)).unwrap();
    let row = ui
        .selection_rows
        .iter()
        .position(|s| s.contains("TARGET"))
        .unwrap();
    let prefix = ui.selection_rows[row].split_once("TARGET").unwrap().0;
    let x = ui.selection_area.x + UnicodeWidthStr::width(prefix) as u16;
    let y = ui.selection_area.y + row as u16;
    ui.select_start(x, y);
    ui.select_drag(x + 5, y);
    ui.mouse_release();
    assert_eq!(ui.selected_text().as_deref(), Some("TARGET"));
    let mut narrow = Terminal::new(TestBackend::new(40, 24)).unwrap();
    narrow.draw(|frame| ui.draw(frame)).unwrap();
    assert!(ui.selection_start.is_none() && ui.selection_end.is_none());
    assert!(
        ui.selected_text().is_none(),
        "a terminal resize must clear old wrapped coordinates instead of copying wrong text"
    );
    let row = ui
        .selection_rows
        .iter()
        .position(|s| s.contains("TARGET"))
        .unwrap();
    let prefix = ui.selection_rows[row].split_once("TARGET").unwrap().0;
    let x = ui.selection_area.x + UnicodeWidthStr::width(prefix) as u16;
    let y = ui.selection_area.y + row as u16;
    ui.select_start(x, y);
    ui.select_drag(x + 5, y);
    ui.mouse_release();
    assert_eq!(ui.selected_text().as_deref(), Some("TARGET"));
}

#[test]
fn narrow_terminal_draw_does_not_panic() {
    let mut store = ThemeStore::load_at(None).unwrap();
    store.select("mono").unwrap();
    let mut ui = UiState::new("mock-model".into(), "test".into(), store, &[]);
    ui.add_user("**Markdown** 你好\n- list");
    ui.event(UiEvent::ToolStarted {
        name: "shell".into(),
        id: "x".into(),
        args: "pwd".into(),
    });
    let mut terminal = Terminal::new(TestBackend::new(30, 12)).unwrap();
    terminal.draw(|frame| ui.draw(frame)).unwrap();
    assert!(ui.viewport_height >= 3);
    let text = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(text.contains("Kroma"));
    assert!(
        !text.contains("Conversation"),
        "avoid a full-screen chat box"
    );
}

#[test]
fn conversation_is_readable_without_competing_boxes() {
    let store = ThemeStore::load_at(None).unwrap();
    let mut ui = UiState::new("demo-model".into(), "demo".into(), store, &[]);
    ui.add_user("Check project and report tests");
    ui.event(UiEvent::ToolStarted {
        name: "shell".into(),
        id: "tool-1".into(),
        args: "cargo test --locked".into(),
    });
    ui.event(UiEvent::ToolFinished {
        id: "tool-1".into(),
        result: "exit: 0\nall tests passed".into(),
        failed: false,
    });
    ui.event(UiEvent::Text(
        "## Tests passed\nAll checks succeeded.".into(),
    ));
    ui.running = false;
    ui.tools_expanded = true;
    let mut terminal = Terminal::new(TestBackend::new(110, 32)).unwrap();
    terminal.draw(|frame| ui.draw(frame)).unwrap();
    let text = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    for expected in ["Kroma", "cargo test", "Tests passed", "all tests passed"] {
        assert!(text.contains(expected), "missing {expected}");
    }
    assert!(
        !text.contains("┌ Conversation"),
        "transcript should breathe"
    );
}

#[test]
fn context_display_uses_total_window_not_reserved_input_budget() {
    use crate::events::EventSink;

    let theme = ThemeStore::load_at(None).unwrap();
    let mut state = UiState::new("mock".into(), "default".into(), theme, &[]);
    assert_eq!(state.context_display(), "Ctx —");
    state.configure_status(500_000, Some("high".into()));
    assert_eq!(state.context_display(), "Ctx — / 500k");

    let (tx, mut rx) = super::super::event_queue::channel();
    let sink = UiSink(tx);
    sink.on_event(RuntimeEvent::ContextPrepared {
        messages: 2,
        estimated_input_tokens: 125_000,
        available_input_tokens: 372_000,
    });
    state.event(rx.try_recv().unwrap());
    assert_eq!(state.context_display(), "Ctx ~125k / 500k · ~25%");

    sink.on_event(RuntimeEvent::ModelFinished {
        request: 1,
        tools: 0,
        reasoning: None,
        input_tokens: Some(100_000),
        output_tokens: Some(10_000),
        cached_tokens: Some(80_000),
        reasoning_tokens: Some(3_000),
        cache_write_tokens: None,
        cache_write_1h_tokens: None,
    });
    while let Ok(event) = rx.try_recv() {
        state.event(event);
    }
    assert_eq!(state.context_display(), "Ctx 100k / 500k · 20%");
    sink.on_event(RuntimeEvent::CompactionUsage {
        requests: 2,
        input_tokens: Some(1_000),
        output_tokens: Some(200),
        cached_tokens: Some(800),
        cache_write_tokens: None,
    });
    while let Ok(event) = rx.try_recv() {
        state.event(event);
    }
    assert_eq!(state.apec_usage.calls, 2);
    assert_eq!(state.usage.calls, 1);
    assert_eq!(state.context_display(), "Ctx 100k / 500k · 20%");
    // Show current request usage, not cumulative billed tokens.
    state.event(UiEvent::ActualInput(150_000));
    assert_eq!(state.context_display(), "Ctx 150k / 500k · 30%");
    state.reset_context();
    assert_eq!(state.context_display(), "Ctx — / 500k");

    // A model/session switch updates the total window without inventing usage.
    state.configure_status(200_000, None);
    assert_eq!(state.context_display(), "Ctx — / 200k");
    sink.on_event(RuntimeEvent::ContextPrepared {
        messages: 2,
        estimated_input_tokens: 50_000,
        available_input_tokens: 168_000,
    });
    state.event(rx.try_recv().unwrap());
    assert_eq!(state.context_display(), "Ctx ~50k / 200k · ~25%");
}

#[test]
fn context_status_uses_total_window_in_wide_and_narrow_terminals() {
    let theme = ThemeStore::load_at(None).unwrap();
    let mut state = UiState::new("mock".into(), "default".into(), theme, &[]);
    state.configure_status(500_000, Some("high".into()));
    state.event(UiEvent::ActualInput(100_000));
    assert!(state.usage_report().contains("100k / 500k"));
    for (width, expected) in [(110, "Context 20%"), (40, "Context 20%")] {
        let mut terminal = Terminal::new(TestBackend::new(width, 24)).unwrap();
        terminal.draw(|frame| state.draw(frame)).unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains(expected), "missing {expected}: {text}");
    }
}

#[test]
fn mixed_attachments_keep_numbered_order_after_history_restore() {
    let mut message = Message::new("user", "Compare these files in sequence");
    message.provider_items = Some(vec![
        serde_json::json!({"type":"kroma_attachment","kind":"image","name":"before.png"}),
        serde_json::json!({"type":"kroma_attachment","kind":"log","name":"Pasted text"}),
        serde_json::json!({"type":"kroma_attachment","kind":"image","name":"after.png"}),
    ]);
    let ui = UiState::new(
        "mock".into(),
        "test".into(),
        ThemeStore::load_at(None).unwrap(),
        &[message],
    );
    let content = ui
        .history_text(ui.themes.colors, 80)
        .lines
        .iter()
        .flat_map(|line| line.spans.iter().map(|span| span.content.as_ref()))
        .collect::<String>();
    let first = content.find("#1 ▧ before.png").expect("image 1 label");
    let second = content.find("#2 ▤ Pasted text").expect("log 2 label");
    let third = content.find("#3 ▧ after.png").expect("image 3 label");
    assert!(first < second && second < third);
}

#[test]
fn redesign_snapshot_matrix() {
    let root = std::env::var_os("KROMA_SNAPSHOT_DIR").map(PathBuf::from);
    if let Some(root) = &root {
        std::fs::create_dir_all(root).unwrap();
    }
    for (width, height) in [(40, 16), (80, 24), (100, 30), (160, 40)] {
        for scene in ["home", "conversation", "failure", "cancelled"] {
            let mut ui = UiState::new(
                "test-model".into(),
                "test-session".into(),
                ThemeStore::load_at(None).unwrap(),
                &[],
            );
            ui.preferences.language = super::super::preferences::Language::En;
            ui.home = scene == "home";
            if !ui.home {
                ui.entries
                    .push(Entry::user("Review this change. 检查代码。"));
                ui.entries.push(Entry::new(
                    Kind::Assistant,
                    "Kroma",
                    "Updated the implementation.

```rust
fn main() { crate::cli_println!(\"hello\"); }
```",
                ));
                if scene == "failure" {
                    ui.info("ERROR", "Build failed: E0599");
                }
                ui.task_outcome = Some(match scene {
                    "failure" => TaskOutcome::Failed,
                    "cancelled" => TaskOutcome::Cancelled,
                    _ => TaskOutcome::Completed,
                });
                ui.task_elapsed = Some(Duration::from_millis(35300));
            }
            let rows = rendered_text(&mut ui, width, height);
            assert_eq!(rows.len(), usize::from(height));
            if let Some(root) = &root {
                std::fs::write(
                    root.join(format!("{scene}-{width}x{height}.txt")),
                    rows.join(
                        "
",
                    ),
                )
                .unwrap();
            }
        }
    }
}

fn rendered_rows(ui: &mut UiState, width: u16, height: u16) -> Vec<String> {
    rendered_text(ui, width, height)
        .iter()
        .map(|row| row.chars().filter(|ch| *ch != ' ').collect())
        .collect()
}

/// Cell-accurate rows, keeping spaces: alignment and separation are the
/// behaviour under test for layout work.
fn rendered_text(ui: &mut UiState, width: u16, height: u16) -> Vec<String> {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| ui.draw(frame)).unwrap();
    terminal
        .backend()
        .buffer()
        .content()
        .chunks(width as usize)
        .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
        .collect()
}

#[test]
fn bilingual_chrome_picker_descriptions_and_unknown_usage_render_at_multiple_sizes() {
    let mut ui = UiState::new(
        "mock".into(),
        "test".into(),
        ThemeStore::load_at(None).unwrap(),
        &[],
    );
    ui.apply_preferences(super::super::preferences::Preferences {
        language: super::super::preferences::Language::Zh,
    });
    ui.configure_status(500_000, Some("high".into()));
    ui.event(UiEvent::Context { estimated: 100_000 });
    ui.event(UiEvent::Usage {
        input: None,
        output: None,
        cached: None,
        reasoning: None,
        cache_write: None,
    });
    for width in [30, 40, 80, 160] {
        let output = rendered_rows(&mut ui, width, 26).join("\n");
        assert!(
            output.contains("上下文"),
            "localized footer missing at {width}: {output}"
        );
        assert!(
            ui.usage_report().contains("未报告"),
            "unknown must not become 0 at {width}: {output}"
        );
        assert!(
            UnicodeWidthStr::width(ui.usage_status((width - 8) as usize).as_str())
                <= (width - 8) as usize
        );
    }
    ui.open_command_completion("/language".into());
    let output = rendered_rows(&mut ui, 100, 26).join("\n");
    assert!(output.contains("命令索引"));
    assert!(output.contains("切换界面语言并保存"));
    ui.picker = None;
    ui.enter_home();
    let output = rendered_rows(&mut ui, 100, 30).join("\n");
    assert!(output.contains("在下方开始任务"));
    assert!(output.contains("Ctrl+P查看更多"));
    ui.apply_preferences(super::super::preferences::Preferences::default());
    let output = rendered_rows(&mut ui, 100, 30).join("\n");
    assert!(output.contains("Startataskbelow"));
}

#[test]
fn wrapping_keeps_source_line_identity_within_the_reading_column() {
    let text = Text::from(vec![
        Line::raw("word ".repeat(45)),
        Line::raw(format!("│ {}", "x".repeat(150))),
    ]);
    let (wrapped, sources) = wrap_document(text, 180);
    assert_eq!(wrapped.lines.len(), sources.len());
    assert_eq!(*sources.last().unwrap(), 1);
    for line in &wrapped.lines {
        assert!(UnicodeWidthStr::width(line.to_string().as_str()) <= 180);
    }
    let (narrow, sources) = wrap_document(Text::from(vec![Line::raw("word ".repeat(45))]), 60);
    assert!(narrow.lines.len() > 1, "the column width is the wrap width");
    assert!(sources.iter().all(|source| *source == 0));
}

#[test]
fn history_navigation_reports_unseen_updates_without_moving_and_survives_completion() {
    let messages: Vec<_> = (0..60)
        .map(|i| Message::new("assistant", format!("history line {i}")))
        .collect();
    let mut ui = UiState::new(
        "mock".into(),
        "test".into(),
        ThemeStore::load_at(None).unwrap(),
        &messages,
    );
    rendered_rows(&mut ui, 80, 24);
    ui.scroll(12);
    rendered_rows(&mut ui, 80, 24);
    let anchor = ui.scroll_anchor;
    ui.event(UiEvent::Text("new streaming result".into()));
    let output = rendered_rows(&mut ui, 80, 24).join("\n");
    assert_eq!(anchor, ui.scroll_anchor);
    assert!(output.contains("Browsinghistory"));
    assert!(ui.navigation_status().contains("new updates"));
    let reading = ui.reading_position();
    ui.load_history(&messages);
    ui.task_outcome = Some(TaskOutcome::Completed);
    ui.restore_reading_position(reading);
    rendered_rows(&mut ui, 80, 24);
    assert_eq!(anchor, ui.scroll_anchor);
    ui.scroll_to_bottom();
    assert_eq!(ui.unread_updates, 0);
    assert!(ui.scroll_anchor.is_none());
    assert!(!rendered_rows(&mut ui, 80, 24)
        .join("\n")
        .contains("Executionfinished"));
    assert!(
        !ui.has_activity_tail(),
        "ordinary answers end without a redundant completion row"
    );
    ui.task_outcome = Some(TaskOutcome::Failed);
    assert!(ui.task_status().contains("failed"));
    ui.task_outcome = Some(TaskOutcome::Cancelled);
    assert!(ui.task_status().contains("Cancelled"));
    ui.enter_home();
    assert!(ui.task_outcome.is_none());
}

#[test]
fn collapsed_tool_summary_preserves_full_arguments_archive_reference_and_stderr_warning() {
    let mut ui = UiState::new(
        "mock".into(),
        "test".into(),
        ThemeStore::load_at(None).unwrap(),
        &[],
    );
    let command = format!("cargo test {} UNIQUE_COMMAND_TAIL", "--locked ".repeat(80));
    let raw = serde_json::json!({"command":command}).to_string();
    assert_eq!(tool_arguments("shell", &raw), command);
    let result = tool_result_details(&serde_json::json!({"exit_code":0, "stdout":"line\n".repeat(400), "stderr":"warning: diagnostic", "stdout_file":"/tmp/kroma-complete.log"}).to_string());
    assert!(result.contains("/tmp/kroma-complete.log"));
    ui.event(UiEvent::ToolStarted {
        name: "shell".into(),
        id: "1".into(),
        args: command,
    });
    ui.event(UiEvent::ToolFinished {
        id: "1".into(),
        result,
        failed: false,
    });
    let collapsed = ui.history_text(ui.themes.colors, 160).to_string();
    assert!(collapsed.contains("Run Rust tests"));
    assert!(collapsed.contains("diagnostic reported"));
    assert!(!collapsed.contains("UNIQUE_COMMAND_TAIL"));
    assert!(
        !ui.entries[0].failed,
        "stderr warnings do not change exit status"
    );
    assert!(ui.entries[0].duration.is_some());
    ui.tools_expanded = true;
    let expanded = ui.history_text(ui.themes.colors, 160).to_string();
    assert!(expanded.contains("UNIQUE_COMMAND_TAIL"));
    assert!(expanded.contains("/tmp/kroma-complete.log"));
}

#[test]
fn screenshot_regressions_compact_tools_localized_labels_and_single_history_hint() {
    let mut ui = UiState::new(
        "mock".into(),
        "test".into(),
        ThemeStore::load_at(None).unwrap(),
        &[],
    );
    ui.apply_preferences(super::super::preferences::Preferences {
        language: super::super::preferences::Language::Zh,
    });
    ui.event(UiEvent::ToolStarted {
        name: "shell".into(),
        id: "build".into(),
        args: format!(
            "cd /project && cargo build --release {}",
            " --locked".repeat(100)
        ),
    });
    ui.event(UiEvent::ToolFinished {
        id: "build".into(),
        result: "exit 0\nstderr:    Compiling kroma\n    Finished release profile".into(),
        failed: false,
    });
    for width in [40, 80, 180, 240] {
        let raw = ui.history_text(ui.themes.colors, width);
        let (wrapped, _) = wrap_document(raw, width);
        let rows: Vec<_> = wrapped
            .lines
            .iter()
            .map(ToString::to_string)
            .filter(|row| !row.trim().is_empty())
            .collect();
        assert_eq!(
            rows.len(),
            1,
            "collapsed successful tool must occupy one row at {width}: {rows:?}"
        );
        assert!(rows[0].contains('✓'));
        assert!(!rows[0].contains("警告"));
    }
    ui.info("COMMAND", "Resumed session: abc123");
    ui.editor.insert_attachment(
        char::from_u32(super::super::editor::ATTACHMENT_START).unwrap(),
        "Clipboard image".into(),
        None,
    );
    ui.note("已添加 【Clipboard image】");
    let output = rendered_rows(&mut ui, 100, 26).join("\n");
    assert!(output.contains("已恢复会话：abc123"));
    assert!(output.contains("剪贴板图片"));
    assert!(!output.contains("Clipboardimage"));
    assert!(ui.notification.contains("剪贴板图片"));
    for i in 0..30 {
        ui.info("COMMAND", format!("old line {i}"));
    }
    ui.dismiss_notification();
    rendered_rows(&mut ui, 100, 26);
    ui.scroll(10);
    let output = rendered_rows(&mut ui, 100, 26).join("\n");
    assert_eq!(
        output.matches("返回最新").count(),
        1,
        "the clickable return-to-latest action should appear once: {output}"
    );
}

#[test]
fn reasoning_block_is_separated_from_the_answer_that_follows_it() {
    let mut ui = UiState::new(
        "mock".into(),
        "test".into(),
        ThemeStore::load_at(None).unwrap(),
        &[],
    );
    ui.add_user("why");
    ui.event(UiEvent::ReasoningFinal("private chain of thought".into()));
    ui.event(UiEvent::Text("the answer".into()));
    ui.running = false;
    let rows = rendered_text(&mut ui, 80, 24);
    let reasoning = rows
        .iter()
        .position(|row| row.contains("Reasoning collapsed · Ctrl+T"))
        .expect("collapsed reasoning line");
    assert!(
        rows[reasoning + 1].trim().is_empty(),
        "reasoning must not be glued to the answer header: {:?}",
        &rows[reasoning..reasoning + 3]
    );
    assert!(
        rows[reasoning + 2].contains("◆") && rows[reasoning + 2].contains("the answer"),
        "the answer header follows the separator: {:?}",
        &rows[reasoning..reasoning + 3]
    );
}

#[test]
fn svg_task_layout_is_readable_across_narrow_and_wide_chinese_terminals() {
    let mut ui = UiState::new(
        "deepseek".into(),
        "test".into(),
        ThemeStore::load_at(None).unwrap(),
        &[],
    );
    ui.apply_preferences(super::super::preferences::Preferences {
        language: super::super::preferences::Language::Zh,
    });
    ui.add_user("画一个蹦蹦跳跳的动态 SVG");
    ui.event(UiEvent::ReasoningDelta("先检查 Node 是否可用".into()));
    ui.event(UiEvent::ToolStarted {
        name: "shell".into(),
        id: "check-node".into(),
        args: "node -v".into(),
    });
    ui.event(UiEvent::ToolFinished {
        id: "check-node".into(),
        result: "exit: 0\nv22.0".into(),
        failed: false,
    });
    ui.event(UiEvent::ModelStarted);
    ui.event(UiEvent::ReasoningDelta("写入动画文件".into()));
    ui.event(UiEvent::ToolStarted {
        name: "shell".into(),
        id: "draw-svg".into(),
        args: "node -e \"require('fs').writeFileSync('bouncing-cat.svg','<svg/>')\"".into(),
    });
    ui.event(UiEvent::ToolFinished {
        id: "draw-svg".into(),
        result: "exit: 49\n-1.18-windows.xml".into(),
        failed: true,
    });
    ui.event(UiEvent::Text("Node 可用，正在处理 SVG。".into()));
    ui.running = false;
    for width in [40, 80, 120, 184, 240] {
        let rows = rendered_text(&mut ui, width, 42);
        let stripped = rows
            .iter()
            .map(|row| row.chars().filter(|ch| *ch != ' ').collect::<String>())
            .collect::<Vec<_>>();
        let text = stripped.join("\n");
        let first_user = stripped
            .iter()
            .position(|row| row.contains("蹦蹦跳跳"))
            .unwrap();
        assert!(
            first_user < 6,
            "user prompt should start near the header at {width}"
        );
        assert_eq!(
            text.matches("思考已折叠").count(),
            1,
            "only one collapsed reasoning header at {width}"
        );
        assert!(
            text.contains("另有1步") && text.contains("bouncing-cat.svg"),
            "real tool operations must be visible at {width}: {text}"
        );
        assert!(!text.contains("执行Shell命令"));
        assert!(
            text.contains("exit49"),
            "failure remains visible at {width}"
        );
    }
}

#[test]
fn failed_tools_prefer_actionable_diagnostics_over_unrelated_stdout() {
    assert_eq!(failure_summary("exit: 49\n-1.18-windows.xml"), "exit 49");
    assert_eq!(
        failure_summary("exit: 101\nstderr: error[E0599]: method not found"),
        "exit 101 · error[E0599]: method not found"
    );
    assert_eq!(
        failure_summary("exit: 1\nThe file was not found"),
        "exit 1 · The file was not found"
    );
    assert_eq!(failure_summary("connection refused"), "connection refused");
}

#[test]
fn expanded_git_diff_uses_semantic_add_remove_colors() {
    let mut ui = UiState::new(
        "mock".into(),
        "test".into(),
        ThemeStore::load_at(None).unwrap(),
        &[],
    );
    ui.event(UiEvent::ToolStarted {
        name: "shell".into(),
        id: "diff".into(),
        args: "git diff -- src/main.rs".into(),
    });
    ui.event(UiEvent::ToolFinished {
        id: "diff".into(),
        result:
            "exit: 0\ndiff --git a/src/main.rs b/src/main.rs\n@@ -1 +1 @@\n-old\n+new\n unchanged"
                .into(),
        failed: false,
    });
    ui.tools_expanded = true;
    let text = ui.history_text(ui.themes.colors, 105);
    let ink = |needle: &str| {
        text.lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .find(|span| span.content == needle)
            .and_then(|span| span.style.fg)
    };
    assert_eq!(ink("+new"), Some(ui.themes.colors.success));
    assert_eq!(ink("-old"), Some(ui.themes.colors.error));
    assert_eq!(ink("@@ -1 +1 @@"), Some(ui.themes.colors.muted));
    assert_eq!(ink(" unchanged"), Some(ui.themes.colors.code));
}

#[test]
fn detailed_tool_result_remains_visible_and_original_log_is_expandable() {
    let mut ui = UiState::new_default(
        "mock".into(),
        "tool-log-panel".into(),
        ThemeStore::load_at(None).unwrap(),
        &[],
    );
    ui.add_user("Inspect files");
    ui.event(UiEvent::ToolStarted {
        name: "shell".into(),
        id: "older".into(),
        args: "echo older-turn".into(),
    });
    ui.event(UiEvent::ToolFinished {
        id: "older".into(),
        result: "exit: 0\\nprevious".into(),
        failed: false,
    });
    ui.add_user("Investigate failing test");
    ui.event(UiEvent::ToolStarted {
        name: "shell".into(),
        id: "current".into(),
        args: "cargo test --lib".into(),
    });
    ui.event(UiEvent::ToolFinished {
        id: "current".into(),
        result: "exit: 101\\nstderr: assertion failed".into(),
        failed: true,
    });
    ui.editor.paste("unfinished user draft");
    let history = ui.history_text(ui.active_colors(), 100).to_string();
    assert!(history.contains("cargo test") && history.contains("assertion failed"));
    ui.toggle_all_tools();
    let expanded = ui.history_text(ui.active_colors(), 100).to_string();
    assert!(expanded.contains("cargo test --lib"));
    assert!(expanded.contains("assertion failed"));
    assert!(!ui.has_plugin_panel());
    assert_eq!(ui.editor.text(), "unfinished user draft");
}

#[test]
fn reasoning_is_collapsed_by_default_and_complete_when_expanded() {
    let mut ui = UiState::new_default(
        "mock".into(),
        "reasoning-preview".into(),
        ThemeStore::load_at(None).unwrap(),
        &[],
    );
    ui.add_user("Investigate the failure");
    let content = (0..18)
        .map(|n| format!("reasoning line {n:02}"))
        .collect::<Vec<_>>()
        .join("\n\n");
    ui.event(UiEvent::TaskStarted);
    ui.event(UiEvent::ReasoningDelta(content.clone()));
    ui.reasoning_expanded = true;
    let pending = ui.history_text(ui.active_colors(), 100).to_string();
    assert!(
        !pending.contains("reasoning line") && !pending.contains("KROMA"),
        "thinking-only output must not create a response card"
    );
    assert!(ui.activity_status().starts_with("Thinking"));
    ui.event(UiEvent::Text("The investigation is complete.".into()));
    let document = ui.history_text(ui.active_colors(), 100);
    let visible = document.to_string();
    assert!(visible.contains("reasoning line 17"));
    assert!(visible.contains("reasoning line 00"));
    assert!(!visible.contains("earlier reasoning"));
    for row in &document.lines {
        if row.to_string().contains("reasoning line") {
            assert!(row
                .spans
                .iter()
                .all(|s| !s.style.add_modifier.contains(Modifier::ITALIC)));
        }
    }
    ui.open_full_transcript();
    assert!(ui
        .plugin_panel
        .as_ref()
        .unwrap()
        .body
        .contains("reasoning line 00"));
    assert!(ui
        .plugin_panel
        .as_ref()
        .unwrap()
        .body
        .contains("reasoning line 17"));
}

#[test]
fn one_reasoning_notice_per_user_turn_preserves_every_expanded_segment() {
    let mut ui = UiState::new(
        "mock".into(),
        "test".into(),
        ThemeStore::load_at(None).unwrap(),
        &[],
    );
    ui.add_user("draw an animated SVG");
    ui.event(UiEvent::ReasoningDelta(
        "I should inspect the environment".into(),
    ));
    ui.event(UiEvent::ToolStarted {
        name: "shell".into(),
        id: "node-check".into(),
        args: "node -v".into(),
    });
    ui.event(UiEvent::ToolFinished {
        id: "node-check".into(),
        result: "exit: 0\nv22.0.0".into(),
        failed: false,
    });
    ui.event(UiEvent::ModelStarted);
    ui.event(UiEvent::ReasoningDelta("Now create the SVG drawing".into()));
    ui.event(UiEvent::ToolStarted {
        name: "shell".into(),
        id: "create".into(),
        args: "node -e \"require('fs').writeFileSync('cat.svg','<svg/>')\"".into(),
    });
    ui.event(UiEvent::ToolFinished {
        id: "create".into(),
        result: "exit: 0\ncat.svg".into(),
        failed: false,
    });
    ui.event(UiEvent::ModelStarted);
    ui.event(UiEvent::ReasoningFinal("Now check the SVG file".into()));
    ui.running = false;

    let collapsed = ui.history_text(ui.themes.colors, 120).to_string();
    assert_eq!(
        collapsed.matches("Reasoning collapsed · Ctrl+T").count(),
        1,
        "three thinking segments must not produce three repeated banners"
    );
    assert!(!collapsed.contains("I should inspect"));
    assert!(!collapsed.contains("Now create"));
    assert!(!collapsed.contains("Check Node version"));
    ui.expanded_tool_groups.insert("node-check".into());
    assert!(ui
        .history_text(ui.themes.colors, 120)
        .to_string()
        .contains("Check Node version"));
    assert!(collapsed.contains("2 steps completed"));
    assert!(ui
        .history_text(ui.themes.colors, 120)
        .to_string()
        .contains("Write · cat.svg"));
    assert!(!collapsed.contains("Run shell command"));

    ui.reasoning_expanded = true;
    let expanded = ui.history_text(ui.themes.colors, 120).to_string();
    assert_eq!(
        expanded.matches("Reasoning · Ctrl+T to collapse").count(),
        1
    );
    assert!(
        expanded.contains("I should inspect"),
        "expanded reasoning is complete"
    );
    assert!(expanded.contains("Now create"));
    assert!(expanded.contains("Now check the SVG"));
    assert!(!expanded.contains("earlier reasoning"));
    ui.open_full_transcript();
    assert!(ui.has_plugin_panel());
    assert!(
        ui.plugin_panel
            .as_ref()
            .unwrap()
            .body
            .contains("I should inspect"),
        "searchable full transcript retains every original reasoning segment"
    );
    ui.close_plugin_panel();
    ui.add_user("make it bounce higher");
    ui.event(UiEvent::ReasoningDelta("Adjust the path".into()));
    ui.running = false;
    let next = ui.history_text(ui.themes.colors, 120).to_string();
    assert_eq!(next.matches("Reasoning · Ctrl+T to collapse").count(), 2);
}

#[test]
fn long_successful_tool_bursts_collapse_without_hiding_failures() {
    let mut ui = UiState::new(
        "mock".into(),
        "test".into(),
        ThemeStore::load_at(None).unwrap(),
        &[],
    );
    ui.add_user("inspect the repository");
    for i in 0..18 {
        let id = format!("call-{i}");
        ui.event(UiEvent::ToolStarted {
            name: "shell".into(),
            id: id.clone(),
            args: format!("rg unique-pattern-{i} src"),
        });
        ui.event(UiEvent::ToolFinished {
            id,
            result: "exit: 0\none match".into(),
            failed: false,
        });
    }
    ui.event(UiEvent::ToolStarted {
        name: "shell".into(),
        id: "failed".into(),
        args: "rg unresolved_pattern src".into(),
    });
    ui.event(UiEvent::ToolFinished {
        id: "failed".into(),
        result: "exit: 1\nnot found".into(),
        failed: true,
    });
    ui.event(UiEvent::ToolStarted {
        name: "shell".into(),
        id: "active".into(),
        args: "cargo test".into(),
    });
    let collapsed = ui.history_text(ui.themes.colors, 120).to_string();
    assert!(collapsed.contains("20 steps"));
    assert!(collapsed.contains("Another 12 steps"));
    assert!(!collapsed.contains("unique-pattern-0"));
    assert!(collapsed.contains("unique-pattern-17"));
    assert!(collapsed.contains("unresolved_pattern"));
    assert!(collapsed.contains("exit 1"));
    assert!(collapsed.contains("Run Rust tests"));
    ui.tools_expanded = true;
    let full = ui.history_text(ui.themes.colors, 120).to_string();
    assert!(full.contains("Another 12 steps"));
    assert!(!full.contains("unique-pattern-0"));
    assert!(full.contains("unique-pattern-17"));
}

#[test]
fn single_tool_chevron_expands_only_its_own_output() {
    let mut ui = UiState::new(
        "mock".into(),
        "test".into(),
        ThemeStore::load_at(None).unwrap(),
        &[],
    );
    for (id, command, result) in [
        ("one", "node -v", "exit: 0\nfirst tool detail 22.0.0"),
        (
            "two",
            "node render.js",
            "exit: 0\nsecond tool detail cat.svg",
        ),
    ] {
        ui.event(UiEvent::ToolStarted {
            name: "shell".into(),
            id: id.into(),
            args: command.into(),
        });
        ui.event(UiEvent::ToolFinished {
            id: id.into(),
            result: result.into(),
            failed: false,
        });
    }
    let initial = rendered_text(&mut ui, 100, 30);
    assert!(!initial.iter().any(|row| row.contains("Check Node version")));
    let group_row = initial
        .iter()
        .position(|row| row.contains("2 steps"))
        .unwrap();
    assert!(ui.toggle_tool_at(ui.selection_area.x + 2, group_row as u16));
    let rows = rendered_text(&mut ui, 100, 30);
    let row = rows
        .iter()
        .position(|text| text.contains("Check Node version"))
        .expect("first tool row");
    assert_eq!(ui.selection_area.y, 2);
    let marker_x = ui.selection_area.x + 2;
    assert!(!ui.toggle_tool_at(marker_x + 12, row as u16));
    assert!(ui.toggle_tool_at(marker_x, row as u16));
    let after = rendered_text(&mut ui, 100, 30).join("\n");
    assert!(after.contains("first tool detail"));
    assert!(!after.contains("second tool detail"));
    assert!(after.contains("▾"), "clicked tool displays an open chevron");
    assert!(after.contains("▸"), "other tool remains collapsed");
    assert!(!ui.toggle_tool_at(marker_x, 0), "header is not a tool");
    ui.tools_expanded = true;
    let all = rendered_text(&mut ui, 100, 30).join("\n");
    assert!(
        all.contains("second tool detail"),
        "Ctrl+O still exposes all"
    );
    ui.tools_expanded = false;
    rendered_text(&mut ui, 100, 30);
    assert!(ui.toggle_tool_at(marker_x, row as u16));
    let collapsed = rendered_text(&mut ui, 100, 30).join("\n");
    assert!(!collapsed.contains("first tool detail"));
}

#[test]
fn footer_is_one_compact_row_without_repeated_command_hints() {
    let mut ui = UiState::new(
        "deepseek".into(),
        "test".into(),
        ThemeStore::load_at(None).unwrap(),
        &[],
    );
    ui.configure_status(1_000_000, Some("auto".into()));
    ui.preferences.language = super::super::preferences::Language::Zh;
    ui.event(UiEvent::ActualInput(1000));
    ui.event(UiEvent::Usage {
        input: Some(1500),
        output: Some(8),
        cached: Some(1305),
        reasoning: None,
        cache_write: None,
    });
    ui.add_user("你好");
    ui.event(UiEvent::Text("answer".into()));
    ui.event(UiEvent::TaskFinished);
    let rows = rendered_text(&mut ui, 120, 30);
    let footer = rows.last().expect("footer row");
    let visible: String = footer.chars().filter(|ch| *ch != ' ').collect();
    assert!(visible.contains("deepseek"), "{visible:?}");
    assert!(visible.contains("上下文0.1%"), "{visible:?}");
    assert!(!visible.contains("缓存"), "{visible:?}");
    assert!(ui.usage_report().contains("缓存命中 87%"));
    assert!(ui.usage_variants()[1].contains("输入/输出 1.5k / 8"));
    assert!(
        !visible.contains("/usage") && !visible.contains("/help"),
        "use the slash picker for commands instead of a second help bar"
    );
    let first_line = rows
        .iter()
        .filter(|row| {
            row.chars()
                .filter(|ch| *ch != ' ')
                .collect::<String>()
                .contains("上下文")
        })
        .count();
    assert_eq!(first_line, 1, "context information belongs to only one row");
    let column = reading_area(Rect::new(0, 0, 120, 30), false);
    let leading = footer.chars().take_while(|ch| *ch == ' ').count();
    assert!(
        leading >= column.x as usize,
        "footer must align with the reading column"
    );
}

#[test]
fn completion_row_reports_the_outcome_and_how_long_the_turn_took() {
    let mut ui = UiState::new(
        "mock".into(),
        "test".into(),
        ThemeStore::load_at(None).unwrap(),
        &[],
    );
    ui.add_user("task");
    // The real loop takes the duration when the task future resolves: the
    // final event drain keeps only measurements, so a TaskFinished event
    // may never reach the UI.
    ui.event(UiEvent::ToolStarted {
        name: "shell".into(),
        id: "done".into(),
        args: "echo done".into(),
    });
    ui.event(UiEvent::ToolFinished {
        id: "done".into(),
        result: "exit: 0".into(),
        failed: false,
    });
    ui.finish_task();
    ui.task_outcome = Some(TaskOutcome::Completed);
    let rows = rendered_text(&mut ui, 80, 24);
    let completed = rows
        .iter()
        .find(|row| row.contains("Execution finished"))
        .expect("completion marker");
    assert!(completed.contains("Execution finished · "), "{completed:?}");
    assert!(
        completed.trim_end().ends_with('s'),
        "the row reports how long the turn took: {completed:?}"
    );
    assert!(
        !rows.concat().contains("review results above"),
        "the completion row carries information, not an acknowledgement"
    );
    ui.task_elapsed = Some(Duration::from_millis(8_340));
    ui.task_outcome = Some(TaskOutcome::Failed);
    let rows = rendered_text(&mut ui, 80, 24);
    assert!(
        rows.iter()
            .any(|row| row.contains('✕') && row.contains("8.3s")),
        "failure keeps its marker and duration"
    );
    ui.task_outcome = Some(TaskOutcome::Cancelled);
    let rows = rendered_text(&mut ui, 80, 24);
    assert!(
        rows.iter()
            .any(|row| row.contains('⊘') && row.contains("8.3s")),
        "cancellation keeps its marker and duration"
    );
}

#[test]
fn overloaded_live_view_recovers_complete_tool_results_from_authoritative_history() {
    let (tx, mut rx) = super::super::event_queue::channel();
    let mut ui = UiState::new(
        "test".into(),
        "default".into(),
        ThemeStore::load_at(None).unwrap(),
        &[],
    );
    tx.send(UiEvent::TaskStarted).unwrap();
    tx.send(UiEvent::ToolStarted {
        name: "shell".into(),
        id: "call-1".into(),
        args: "echo hello".into(),
    })
    .unwrap();
    tx.send(UiEvent::Text("x".repeat(3 * 1024 * 1024))).unwrap();
    tx.send(UiEvent::ToolFinished {
        id: "call-1".into(),
        result: "hello".into(),
        failed: false,
    })
    .unwrap();
    tx.send(UiEvent::TaskFinished).unwrap();
    while let Ok(event) = rx.try_recv() {
        ui.event(event);
    }
    assert!(!ui.running);
    assert!(ui.notification.contains("Live updates paused"));
    let mut assistant = Message::new("assistant", "");
    assistant.tool_calls = Some(vec![crate::protocol::ToolCall {
        id: "call-1".into(),
        kind: "function".into(),
        function: crate::protocol::FunctionCall {
            name: "shell".into(),
            arguments: r#"{"command":"echo hello"}"#.into(),
        },
    }]);
    ui.load_history(&[
        Message::new("user", "test"),
        assistant,
        Message::tool_with_status("call-1".into(), "hello".into(), false),
        Message::new("assistant", "完整回复🦀"),
    ]);
    let tool = ui
        .entries
        .iter()
        .find(|entry| entry.call_id.as_deref() == Some("call-1"))
        .unwrap();
    assert!(!tool.title.contains("running"));
    assert_eq!(tool.details, "hello");
    assert!(ui.entries.iter().any(|entry| entry.body == "完整回复🦀"));
}

#[test]
fn plugin_widgets_footer_status_and_custom_tui_overlay_render_and_hit_test() {
    use crate::plugins::{
        CustomTuiChannel, CustomTuiClientEvent, CustomTuiFrameUpdate, CustomTuiSpan, CustomTuiSpec,
        PluginUiAction, PluginWidget, UiScope, WidgetPlacement,
    };
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use std::sync::{mpsc, Arc};

    let mut ui = UiState::new_default(
        "mock".into(),
        "session".into(),
        ThemeStore::load_at(None).unwrap(),
        &[],
    );
    ui.plugin_statuses = vec!["UI Lab · 计数 2".into()];

    ui.plugin_widgets = vec![PluginWidget {
        plugin_id: "ui-lab".into(),
        id: "hud".into(),
        placement: WidgetPlacement::Sidebar,
        title: "UI Lab".into(),
        blocks: crate::panel::parse(r#"[{"type":"progress","label":"任务","value":2,"max":4}]"#),
        text: "任务 50%".into(),
        actions: vec![PluginUiAction {
            id: "inc".into(),
            label: "+1 计数".into(),
            primary: true,
            danger: false,
        }],
        scope: UiScope::Session,
    }];

    let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
    terminal.draw(|frame| ui.draw(frame)).unwrap();
    let buffer_text = |buf: &ratatui::buffer::Buffer| -> String {
        let mut out = String::new();
        for y in 0..buf.area.height {
            let mut x = 0u16;
            while x < buf.area.width {
                let sym = buf.cell((x, y)).unwrap().symbol();
                out.push_str(sym);
                let w = unicode_width::UnicodeWidthStr::width(sym).max(1) as u16;
                x += w;
            }
            out.push('\n');
        }
        out
    };
    let screen = buffer_text(terminal.backend().buffer());
    assert!(
        screen.contains("UI Lab · 计数 2"),
        "footer status visible: {screen}"
    );
    assert!(screen.contains("UI Lab"), "sidebar widget rendered");
    assert!(screen.contains("[+1 计数]"), "action button rendered");

    // Verify exact hit-testing on the rendered button coordinates.
    let (rect, plugin_id, widget_id, action_id) = ui.widget_action_hits[0].clone();
    assert_eq!(
        ui.widget_action_at(rect.x, rect.y),
        Some((plugin_id, widget_id, action_id))
    );

    // Custom TUI overlay renders styled lines and exits cleanly on Ctrl+C.
    let (client_tx, client_rx) = mpsc::channel();
    let (frame_tx, frame_rx) = mpsc::channel();
    let wake = Arc::new(tokio::sync::Notify::new());
    let host = ui.custom_tui_host();
    assert!(host.open(CustomTuiChannel {
        spec: CustomTuiSpec {
            plugin_id: "ui-lab".into(),
            title: "自定义画布".into(),
            width: Some(48),
            height: Some(12),
            fullscreen: false,
        },
        client_tx,
        frame_rx,
        wake,
    }));
    assert!(ui.pull_custom_ui());
    assert!(matches!(
        client_rx.try_recv().unwrap(),
        CustomTuiClientEvent::Render { .. }
    ));
    frame_tx
        .send(CustomTuiFrameUpdate {
            lines: vec![vec![CustomTuiSpan {
                text: "自定义画布内容".into(),
                fg: Some("accent".into()),
                bg: None,
                bold: true,
                dim: false,
                reverse: false,
            }]],
            cursor: Some((2, 0)),
            done: false,
            result: None,
        })
        .unwrap();
    assert!(ui.poll_custom_ui());
    terminal.draw(|frame| ui.draw(frame)).unwrap();
    let overlay_screen = buffer_text(terminal.backend().buffer());
    assert!(overlay_screen.contains("自定义画布"));
    assert!(overlay_screen.contains("自定义画布内容"));

    assert!(ui.custom_ui_event(&Event::Key(KeyEvent::new(
        KeyCode::Char('c'),
        KeyModifiers::CONTROL,
    ))));
    assert!(!ui.has_custom_ui());
    assert!(matches!(
        client_rx.try_recv().unwrap(),
        CustomTuiClientEvent::Cancel
    ));
}
