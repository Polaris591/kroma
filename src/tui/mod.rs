//! Default Kroma terminal UI. A thin host of the same Agent used by the CLI.
//! Themes are TOML data. No UI plugin runtime, RPC service or second Agent loop.
mod attachments;
mod editor;
mod links;
mod logo;
mod markdown;
mod picker;
mod theme;
mod title;
mod view;

use crate::{
    agent::Agent,
    app,
    commands::{CommandAction, CommandRegistry},
    config::Config,
    diagnostics::TuiLogGuard,
    model::ModelBackend,
    steering::TaskInput,
};
use anyhow::{Context, Result};
use attachments::{Attachment, ClipboardPaste, MAX_ATTACHMENTS};
use crossterm::{
    event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use editor::EditAction;
use futures_util::StreamExt;
use picker::{Action as PickerAction, Item as PickerItem, Kind as PickerKind, Picker};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::{io, process::Command, sync::Arc};
use theme::ThemeStore;
use tokio::sync::mpsc::{self, UnboundedReceiver};
use view::{UiEvent, UiSink, UiState};

struct TerminalScreen {
    terminal: Terminal<CrosstermBackend<io::Stdout>>,
}

impl TerminalScreen {
    fn enter() -> Result<Self> {
        enable_raw_mode().context("unable to enable terminal raw mode")?;
        if let Err(error) = execute!(
            io::stdout(),
            EnterAlternateScreen,
            EnableBracketedPaste,
            EnableMouseCapture
        ) {
            let _ = disable_raw_mode();
            return Err(error.into());
        }
        match Terminal::new(CrosstermBackend::new(io::stdout())) {
            Ok(terminal) => Ok(Self { terminal }),
            Err(error) => {
                let _ = disable_raw_mode();
                let _ = execute!(
                    io::stdout(),
                    DisableMouseCapture,
                    DisableBracketedPaste,
                    LeaveAlternateScreen
                );
                Err(error.into())
            }
        }
    }

    fn draw(&mut self, state: &mut UiState) -> Result<()> {
        self.terminal.draw(|frame| state.draw(frame))?;
        Ok(())
    }

    fn edit_prompt(&mut self, state: &mut UiState) -> Result<()> {
        if !state.attachments.is_empty() {
            anyhow::bail!("external editor is for text-only drafts; send/remove attachments first");
        }
        let editor = std::env::var("VISUAL")
            .or_else(|_| std::env::var("EDITOR"))
            .context("set VISUAL or EDITOR to use /editor")?;
        let mut args = editor.split_whitespace();
        let program = args.next().context("empty VISUAL/EDITOR")?;
        let path = std::env::temp_dir().join(format!(
            "kroma-prompt-{}-{}.md",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos()
        ));
        std::fs::write(&path, state.editor.text())?;
        disable_raw_mode()?;
        execute!(
            io::stdout(),
            DisableMouseCapture,
            DisableBracketedPaste,
            LeaveAlternateScreen
        )?;
        let outcome = Command::new(program).args(args).arg(&path).status();
        let restored = execute!(
            io::stdout(),
            EnterAlternateScreen,
            EnableBracketedPaste,
            EnableMouseCapture
        )
        .and_then(|()| enable_raw_mode());
        let contents = std::fs::read_to_string(&path);
        let _ = std::fs::remove_file(path);
        restored?;
        self.terminal.clear()?;
        let status = outcome.context("launching external editor")?;
        if !status.success() {
            anyhow::bail!("editor exited unsuccessfully: {status}");
        }
        state.editor.replace_all(&contents?);
        state.note("Draft updated from external editor");
        Ok(())
    }
}

impl Drop for TerminalScreen {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(
            io::stdout(),
            DisableMouseCapture,
            DisableBracketedPaste,
            LeaveAlternateScreen
        );
    }
}

const SLASH_COMMANDS: &[&str] = &[
    "/help",
    "/new",
    "/clear",
    "/sessions",
    "/resume",
    "/continue",
    "/rename",
    "/model",
    "/think",
    "/theme",
    "/tools",
    "/skills",
    "/usage",
    "/diff",
    "/export",
    "/history",
    "/fork",
    "/rewind",
    "/undo",
    "/pending",
    "/queue",
    "/steer",
    "/editor",
    "/exit",
    "/quit",
];

enum InputAction {
    None,
    Quit,
    Submit(String),
    Cancel,
    Edit,
}

fn attach_text(state: &mut UiState, text: &str, workdir: &std::path::Path, explicit: bool) {
    if explicit || attachments::long_paste(text) {
        if state.attachments.len() >= MAX_ATTACHMENTS {
            state.note("Maximum 8 attachments");
            return;
        }
        match Attachment::from_log(text, workdir) {
            Ok(item) => {
                let label = item.inline_label();
                let detail = item.label();
                match state.attach(item) {
                    Ok(()) => state.note(format!("Pasted 【{label}】 · {detail}")),
                    Err(error) => state.note(error),
                }
            }
            Err(error) => state.note(format!("Paste failed: {error:#}")),
        }
    } else {
        state.editor.paste(text);
    }
}

fn import_path(state: &mut UiState, text: &str, workdir: &std::path::Path) {
    let path = text.trim().trim_matches('"');
    if state.attachments.len() >= MAX_ATTACHMENTS {
        state.note("Maximum 8 attachments");
        return;
    }
    match Attachment::from_file(std::path::Path::new(path), workdir) {
        Ok(item) => {
            let label = item.inline_label();
            let detail = item.label();
            match state.attach(item) {
                Ok(()) => state.note(format!("Attached 【{label}】 · {detail}")),
                Err(error) => state.note(error),
            }
        }
        Err(error) => state.note(format!("Attachment error: {error:#}")),
    }
}

// Keep one predictable cycle; protocols may decline unsupported tiers, in
// which case the existing setting remains unchanged and the UI shows why.
const THINK_LEVELS: &[&str] = &["auto", "off", "low", "medium", "high", "xhigh", "max"];

/// The active profile's advertised levels, or the built-in set when the
/// profile does not restrict them.
fn cycle_levels(model: &dyn ModelBackend) -> Vec<String> {
    if model.thinking_levels().is_empty() {
        THINK_LEVELS
            .iter()
            .map(|level| (*level).to_owned())
            .collect()
    } else {
        model.thinking_levels().to_vec()
    }
}

fn change_thinking(state: &mut UiState, model: &dyn ModelBackend, level: &str) {
    match model.set_reasoning_level(level) {
        Ok(()) => {
            state.thinking = model.reasoning_level();
            save_model_choice(state, model);
            state.note(format!(
                "Thinking: {} · applies to the next model request",
                state.thinking.as_deref().unwrap_or(level)
            ));
        }
        Err(error) => state.note(format!("Thinking unchanged: {error:#}")),
    }
}

fn save_model_choice(state: &mut UiState, model: &dyn ModelBackend) {
    if state.is_home() {
        // No session exists yet: keep the choice in memory until first send.
        return;
    }
    if let (Some(catalog), Some(profile)) = (&state.model_catalog, &state.model_profile) {
        if let Err(error) = catalog.set_model_choice(
            &state.session,
            crate::sessions::SessionModel {
                profile: profile.clone(),
                thinking: model.reasoning_level(),
            },
        ) {
            state.note(format!("Unable to save session model: {error:#}"));
        }
    }
}

fn picker_items(
    state: &UiState,
    kind: PickerKind,
    query: &str,
    workdir: &std::path::Path,
) -> Result<Vec<PickerItem>> {
    Ok(match kind {
        PickerKind::Commands => SLASH_COMMANDS
            .iter()
            .map(|command| PickerItem::new(*command, *command, "command"))
            .collect(),
        PickerKind::Models => crate::profiles::Profiles::load()?
            .profiles
            .iter()
            .map(|(name, profile)| {
                PickerItem::new(
                    format!("{name} · {}", profile.model),
                    format!("/model {name}"),
                    profile.api_protocol.as_deref().unwrap_or("chat"),
                )
            })
            .collect(),
        PickerKind::Sessions => state
            .model_catalog
            .as_ref()
            .context("session catalog is unavailable")?
            .list()?
            .into_iter()
            .map(|session| {
                PickerItem::new(
                    session.name.unwrap_or_else(|| session.id.clone()),
                    format!("/resume {}", session.id),
                    session.id,
                )
            })
            .collect(),
        PickerKind::Files => crate::convenience::suggestions(workdir, &format!("@{query}"))?
            .into_iter()
            .map(|path| {
                // Filesystem paths are Windows backslash-separated; the
                // query and inline @ references use portable slash syntax.
                let path = path.replace('\\', "/");
                PickerItem::new(&path, &path, "file")
            })
            .collect(),
    })
}

fn open_picker(state: &mut UiState, kind: PickerKind, query: &str, workdir: &std::path::Path) {
    match picker_items(state, kind, query, workdir) {
        Ok(items) => state.picker = Some(Picker::new(kind, items, query)),
        Err(error) => state.note(format!("{}: {error:#}", kind.title())),
    }
}

fn picker_event(state: &mut UiState, event: Event, workdir: &std::path::Path) -> InputAction {
    let action = match event {
        Event::Key(key) if key.kind != KeyEventKind::Release => {
            state.picker.as_mut().unwrap().key(key)
        }
        Event::Paste(text) => {
            let picker = state.picker.as_mut().unwrap();
            for ch in text.chars().filter(|ch| *ch != '\r' && *ch != '\n') {
                picker.key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
            }
            PickerAction::Changed
        }
        Event::Mouse(mouse) if mouse.kind == MouseEventKind::ScrollUp => state
            .picker
            .as_mut()
            .unwrap()
            .key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
        Event::Mouse(mouse) if mouse.kind == MouseEventKind::ScrollDown => state
            .picker
            .as_mut()
            .unwrap()
            .key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
        _ => PickerAction::None,
    };
    match action {
        PickerAction::Close => state.picker = None,
        PickerAction::NextKind => {
            let kind = state.picker.as_ref().unwrap().kind.next();
            open_picker(state, kind, "", workdir);
        }
        PickerAction::Changed => {
            if state.picker.as_ref().unwrap().kind == PickerKind::Files {
                let query = state.picker.as_ref().unwrap().query.clone();
                let items = picker_items(state, PickerKind::Files, &query, workdir);
                if let Ok(items) = items {
                    state.picker.as_mut().unwrap().items = items;
                }
            }
        }
        PickerAction::Selected(value) => {
            let kind = state.picker.take().unwrap().kind;
            if kind == PickerKind::Files {
                if value.ends_with('/') {
                    open_picker(state, kind, value.trim_start_matches('@'), workdir);
                } else if state.editor.word_before_caret().starts_with('@') {
                    state.editor.replace_word_before_caret(&value);
                } else {
                    let separator = if state.editor.word_before_caret().is_empty() {
                        ""
                    } else {
                        " "
                    };
                    state.editor.paste(&format!("{separator}{value}"));
                }
            } else {
                return InputAction::Submit(value);
            }
        }
        PickerAction::None => {}
    }
    InputAction::None
}

fn key_action(
    state: &mut UiState,
    key: KeyEvent,
    running: bool,
    model: &dyn ModelBackend,
) -> InputAction {
    if key.kind == KeyEventKind::Release {
        return InputAction::None;
    }
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::BackTab | KeyCode::Tab
            if key.code == KeyCode::BackTab || key.modifiers.contains(KeyModifiers::SHIFT) =>
        {
            let current = model.reasoning_level().unwrap_or_else(|| "auto".into());
            let levels = cycle_levels(model);
            let next = levels
                .iter()
                .position(|level| *level == current)
                .map(|index| levels[(index + 1) % levels.len()].clone())
                .unwrap_or_else(|| levels[0].clone());
            change_thinking(state, model, &next);
            return InputAction::None;
        }
        KeyCode::Char('c') if ctrl && running => return InputAction::Cancel,
        KeyCode::Char('c') if ctrl => {
            if state.editor.text().is_empty() && state.attachments.is_empty() && !state.attach_mode
            {
                return InputAction::Quit;
            }
            state.discard_draft();
            return InputAction::None;
        }
        KeyCode::Char('q') if ctrl && !running => return InputAction::Quit,
        KeyCode::Char('g') if ctrl && !running => return InputAction::Edit,
        KeyCode::F(2) => {
            match state.themes.cycle() {
                Ok(()) => state.note(format!(
                    "Theme: {} (saved to ~/.kroma/theme.toml)",
                    state.themes.preset()
                )),
                Err(error) => state.note(format!("Theme change failed: {error:#}")),
            }
            return InputAction::None;
        }
        KeyCode::Char('o') if ctrl => {
            state.tools_expanded = !state.tools_expanded;
            return InputAction::None;
        }
        KeyCode::Char('r') if ctrl => {
            state.reasoning_expanded = !state.reasoning_expanded;
            return InputAction::None;
        }
        KeyCode::PageUp => {
            state.scroll(state.viewport_height.max(3) as isize / 2);
            return InputAction::None;
        }
        KeyCode::End if ctrl => {
            state.scroll_to_bottom();
            return InputAction::None;
        }
        KeyCode::Tab if !running => {
            let prefix = state.editor.text().trim();
            if prefix.starts_with('/') && !prefix.contains(char::is_whitespace) {
                let matches = SLASH_COMMANDS
                    .iter()
                    .filter(|command| command.starts_with(prefix))
                    .copied()
                    .collect::<Vec<_>>();
                if matches.len() == 1 {
                    state.editor.replace_all(matches[0]);
                } else if !matches.is_empty() {
                    state.note(matches.join("  "));
                }
                return InputAction::None;
            }
        }
        KeyCode::PageDown => {
            state.scroll(-(state.viewport_height.max(3) as isize) / 2);
            return InputAction::None;
        }
        KeyCode::Esc => {
            // Esc dismisses a modal/path input, never the ordinary draft.
            // Ctrl+C remains the explicit clear/cancel/quit action.
            if state.attach_mode {
                state.attach_mode = false;
                state.attach_path.clear();
            }
            return InputAction::None;
        }
        _ => {}
    }
    let previous_history = state.editor.history_index();
    match state.editor.key(key) {
        EditAction::Submit(text) => {
            state.remember_submitted_attachments();
            InputAction::Submit(text)
        }
        EditAction::None => {
            if matches!(key.code, KeyCode::Up | KeyCode::Down) {
                state.history_navigated(previous_history);
            } else {
                state.prune_attachments();
            }
            InputAction::None
        }
    }
}

fn input_event(
    state: &mut UiState,
    event: Event,
    running: bool,
    model: &dyn ModelBackend,
    workdir: &std::path::Path,
) -> InputAction {
    // Notifications last until a real input, not forever. Clear the previous
    // one before processing the event so new feedback remains visible.
    if matches!(&event, Event::Key(key) if key.kind != KeyEventKind::Release)
        || matches!(&event, Event::Paste(_))
        || matches!(&event, Event::Mouse(mouse) if matches!(mouse.kind,
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
            | MouseEventKind::Down(_)))
    {
        state.dismiss_notification();
    }
    if state.picker.is_some() {
        return picker_event(state, event, workdir);
    }
    match event {
        Event::Key(key)
            if !running
                && !state.attach_mode
                && key.code == KeyCode::Char('p')
                && key.modifiers.contains(KeyModifiers::CONTROL) =>
        {
            open_picker(state, PickerKind::Commands, "", workdir);
        }
        Event::Key(key)
            if matches!(key.code, KeyCode::Char('v' | 'V'))
                && key.modifiers.contains(KeyModifiers::ALT) =>
        {
            if running {
                state.note("Attach images/files after this task ends; /queue a follow-up instead");
                return InputAction::None;
            }
            match attachments::clipboard(workdir) {
                Ok(ClipboardPaste::Text(text)) => attach_text(state, &text, workdir, true),
                Ok(ClipboardPaste::Attach(mut items)) => {
                    if state.attachments.len() + items.len() > MAX_ATTACHMENTS {
                        state.note("Maximum 8 attachments");
                    } else {
                        for item in items.drain(..) {
                            let label = item.inline_label();
                            match state.attach(item) {
                                Ok(()) => state.note(format!("Attached 【{label}】")),
                                Err(error) => state.note(error),
                            }
                        }
                    }
                }
                Err(_) => {
                    state.attach_mode = true;
                    state.note("Paste a server-side file path, then Enter · Esc cancel");
                }
            }
        }
        Event::Key(key)
            if key.code == KeyCode::Tab
                && !key.modifiers.intersects(
                    KeyModifiers::SHIFT | KeyModifiers::CONTROL | KeyModifiers::ALT,
                )
                && !running
                && !state.attach_mode =>
        {
            let word = state.editor.word_before_caret();
            if word.starts_with('@') {
                match crate::convenience::suggestions(workdir, &word) {
                    Ok(matches) if matches.len() == 1 => {
                        state.editor.replace_word_before_caret(&matches[0]);
                    }
                    Ok(matches) if !matches.is_empty() => {
                        open_picker(
                            state,
                            PickerKind::Files,
                            word.trim_start_matches('@'),
                            workdir,
                        );
                    }
                    Ok(_) => state.note("No matching file"),
                    Err(error) => state.note(format!("File completion: {error:#}")),
                }
                return InputAction::None;
            }
            return key_action(state, key, running, model);
        }
        Event::Key(key) => {
            if state.attach_mode {
                match key.code {
                    KeyCode::Enter => {
                        state.attach_mode = false;
                        let path = std::mem::take(&mut state.attach_path);
                        import_path(state, &path, workdir);
                    }
                    KeyCode::Backspace => {
                        state.attach_path.pop();
                    }
                    KeyCode::Char(c)
                        if !key
                            .modifiers
                            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                    {
                        state.attach_path.push(c)
                    }
                    KeyCode::Esc => {
                        state.attach_mode = false;
                        state.attach_path.clear();
                    }
                    _ => {}
                }
                return InputAction::None;
            }
            return key_action(state, key, running, model);
        }
        Event::Paste(text) => {
            if state.attach_mode {
                state.attach_path.push_str(&text);
            } else if running && attachments::long_paste(&text) {
                state.note("Long paste needs an idle task; paste it when this task finishes");
            } else if running {
                state.editor.paste(&text);
            } else {
                attach_text(state, &text, workdir, false);
            }
        }
        Event::Mouse(mouse) => match mouse.kind {
            MouseEventKind::ScrollUp => state.scroll(3),
            MouseEventKind::ScrollDown => state.scroll(-3),
            MouseEventKind::Moved => state.hover_link(mouse.column, mouse.row),
            MouseEventKind::Up(crossterm::event::MouseButton::Left) => state.mouse_release(),
            MouseEventKind::Down(crossterm::event::MouseButton::Left) => {
                state.hover_link(mouse.column, mouse.row);
                if state.scrollbar_press(mouse.column, mouse.row) {
                    // Scrollbar owns this drag, not transcript selection.
                } else if mouse.modifiers.contains(KeyModifiers::CONTROL) {
                    if let Some(target) = state.link_at(mouse.column, mouse.row) {
                        let target = state.resolved_link(target);
                        match links::open_target(&target, workdir) {
                            Ok(()) => state.note(format!("Opened: {target}")),
                            Err(error) => state.note(format!("Open link: {error:#}")),
                        }
                    }
                } else {
                    state.select_start(mouse.column, mouse.row);
                }
            }
            MouseEventKind::Drag(crossterm::event::MouseButton::Left) => {
                if state.scrollbar_move(mouse.row) {
                    // Proportional track drag; keep selection untouched.
                } else if !mouse.modifiers.contains(KeyModifiers::CONTROL) {
                    state.select_drag(mouse.column, mouse.row);
                }
            }
            MouseEventKind::Down(crossterm::event::MouseButton::Right) => {
                if let Some(selected) = state.selected_text() {
                    match copy_selection(&selected) {
                        Ok(true) => state.note("Copied selection to desktop clipboard"),
                        Ok(false) => state.note("OSC 52 copy sent · terminal support required"),
                        Err(error) => state.note(format!("Copy failed: {error}")),
                    }
                }
            }
            _ => {}
        },
        _ => {}
    }
    InputAction::None
}

/// Local desktop clipboard is more reliable than OSC 52 in Windows Terminal;
/// remote sessions must use OSC 52 so the bytes reach the user's terminal,
/// not the SSH server's clipboard.
fn copy_selection(selected: &str) -> Result<bool> {
    let remote =
        std::env::var_os("SSH_CONNECTION").is_some() || std::env::var_os("SSH_TTY").is_some();
    if !remote {
        if let Ok(mut clipboard) = arboard::Clipboard::new() {
            if clipboard.set_text(selected.to_owned()).is_ok() {
                return Ok(true);
            }
        }
    }
    use std::io::Write;
    let encoded = base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        selected.as_bytes(),
    );
    write!(io::stdout(), "\x1b]52;c;{}\x07", encoded)?;
    io::stdout().flush()?;
    Ok(false)
}

fn show_local_command(state: &mut UiState, input: &str, model: &dyn ModelBackend) -> bool {
    if input == "/help" {
        state.info("HELP", "Enter to send; Shift+Enter or Ctrl+J to add a line.\nPgUp/PgDn or mouse wheel scroll; Ctrl+End returns to latest output.\nCtrl+P searches commands; Tab switches model/session/file categories.\nF2 changes theme; Shift+Tab cycles thinking (or /think high).\nCtx shows last request: ~ estimated, otherwise API-reported input.\nCtrl+O expands tool output; Ctrl+R shows reasoning.\n/history [count] · /sessions · /resume ID · /fork N · /rewind N · /undo\n/theme [dark|light|mono|reload] · /think [auto|off|low|medium|high|xhigh|max]\n/queue task · /skills · /tools\nWhile running: write steering text, or /queue task; Ctrl+C cancels.\nCtrl+Q quits when idle.");
        return true;
    }
    if input == "/usage" {
        state.info("USAGE", state.usage.report());
        return true;
    }
    if input == "/think"
        || input.starts_with("/think ")
        || input == "/reasoning"
        || input.starts_with("/reasoning ")
    {
        let option = input.split_once(' ').map_or("", |(_, rest)| rest.trim());
        if option.is_empty() {
            state.note(format!(
                "Thinking: {} · Shift+Tab to cycle, /think high to select",
                model.reasoning_level().as_deref().unwrap_or("n/a")
            ));
        } else {
            change_thinking(state, model, option);
        }
        return true;
    }
    if input == "/theme" || input.starts_with("/theme ") {
        let choice = input.strip_prefix("/theme").unwrap().trim();
        let outcome = match choice {
            "" => {
                state.note(format!(
                    "Theme: {} · F2 cycles · edit ~/.kroma/theme.toml then /theme reload",
                    state.themes.preset()
                ));
                return true;
            }
            "reload" => state.themes.reload(),
            name => state.themes.select(name),
        };
        match outcome {
            Ok(()) => state.note(format!("Theme: {}", state.themes.preset())),
            Err(error) => state.note(format!("Theme error: {error:#}")),
        }
        return true;
    }
    false
}

fn running_input(state: &mut UiState, task: &TaskInput, input: String, model: &dyn ModelBackend) {
    if show_local_command(state, &input, model) {
        return;
    }
    if input == "/pending" || input == "/queue list" {
        state.info("QUEUE", crate::commands::pending_text(task));
    } else if input == "/queue clear" {
        state.note(format!(
            "Cleared {} follow-up task(s)",
            task.clear_followups()
        ));
    } else if let Some(queued) = input.strip_prefix("/queue ") {
        match task.queue_followup(queued) {
            Ok(position) => state.note(format!("Follow-up task queued at #{position}")),
            Err(error) => state.note(format!("Queue error: {error:#}")),
        }
    } else if input.starts_with('/') && !input.starts_with("/steer ") {
        state
            .note("During a task: type steering text, /steer text, or /queue task; Ctrl+C cancels");
    } else {
        let instruction = input.strip_prefix("/steer ").unwrap_or(&input);
        match task.steer(instruction) {
            Ok(()) => state.note("Steering accepted; applies at next model boundary"),
            Err(error) => state.note(format!("Steering error: {error:#}")),
        }
    }
}

/// One active task, with the UI and model running concurrently in the same
/// async process. The model future borrows Agent; the renderer never does.
/// Dropping the future on Ctrl+C preserves the existing cancellation policy.
#[allow(clippy::too_many_arguments)]
async fn execute_task(
    agent: &mut Agent,
    prompt: &str,
    attachments: Vec<Attachment>,
    workdir: &std::path::Path,
    model: &dyn ModelBackend,
    state: &mut UiState,
    screen: &mut TerminalScreen,
    input: &mut EventStream,
    rx: &mut UnboundedReceiver<UiEvent>,
) -> Result<bool> {
    state.attachments = attachments.clone();
    state.add_user(prompt);
    state.finish_attachment_send();
    screen.draw(state)?;
    let task_input = agent.task_input();
    let mut ignore_text = |_: &str| {}; // TextDelta arrives through the EventSink.
    let mut ticker = tokio::time::interval(std::time::Duration::from_millis(40));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut dirty = false;
    let result = {
        let items = attachments.iter().map(Attachment::item).collect::<Vec<_>>();
        let mut future = Box::pin(agent.run_with_attachments(prompt, &items, &mut ignore_text));
        loop {
            tokio::select! {
                outcome=&mut future => break Some(outcome),
                key=input.next() => {
                    match key {
                        Some(Ok(event)) => match input_event(state,event,true,model,workdir) {
                            InputAction::Cancel => break None,
                            InputAction::Submit(text) => running_input(state,&task_input,text,model),
                            _ => {},
                        },
                        Some(Err(error)) => return Err(error.into()),
                        None => break None,
                    }
                    dirty=true;
                },
                message=rx.recv() => {
                    if let Some(message)=message {state.event(message);}
                    // Drain bursts so fast model streams do not accumulate
                    // thousands of stale intermediate UI frames.
                    while let Ok(message)=rx.try_recv() {state.event(message);}
                    dirty=true;
                },
                _=ticker.tick() => {
                    if state.auto_scroll_selection() { dirty=true; }
                    if state.tick_activity() { dirty=true; }
                    if dirty {
                        screen.draw(state)?;
                        dirty=false;
                    }
                }
            }
        }
    };
    let mut task_notice = None;
    let completed = match result {
        Some(Ok(_)) => true,
        Some(Err(error)) => {
            let message = format!("Task failed: {error:#}");
            state.note(&message);
            task_notice = Some(("ERROR", message));
            false
        }
        None => {
            agent.cancel()?;
            let message = "Task cancelled; unconfirmed commands are not replayed";
            state.note(message);
            task_notice = Some(("CANCELLED", message.to_owned()));
            false
        }
    };
    // The transcript, not live UI events, is authoritative after completion.
    while rx.try_recv().is_ok() {}
    state.load_history(agent.messages());
    state.session = agent.session_id().to_owned();
    state.refresh_session_title();
    if let Some((label, message)) = task_notice {
        // The footer notification is transient; its failure explanation is
        // still reachable by scrolling after the footer is dismissed.
        state.info(label, message);
    }
    screen.draw(state)?;
    Ok(completed)
}

/// Old sessions persisted DeepSeek's "enabled" mode as an effort. Migrate
/// it only at the persistence boundary; it is not a new user-selectable tier.
fn restore_reasoning(model: &dyn ModelBackend, saved: &str) -> Option<String> {
    if ["KROMA_REASONING_EFFORT", "KROMA_DEEPSEEK_THINKING"]
        .iter()
        .any(|key| std::env::var_os(key).is_some())
    {
        return None;
    }
    let level = if saved == "enabled" { "auto" } else { saved };
    model.set_reasoning_level(level).err().map(|error| {
        format!("Saved thinking '{saved}' unavailable ({error:#}); keeping configured effort")
    })
}

/// Manual Shell is not an Agent task, but must still keep raw-terminal
/// input responsive. Dropping this future invokes the same process guard.
async fn execute_local_shell(
    screen: &mut TerminalScreen,
    state: &mut UiState,
    input: &mut EventStream,
    command: &str,
    workdir: &std::path::Path,
    archive: &std::path::Path,
) -> Result<()> {
    state.note("Shell running · Ctrl+C / Esc cancels");
    screen.draw(state)?;
    let result = {
        let mut future = Box::pin(crate::shell::execute_for_session(command, workdir, archive));
        loop {
            tokio::select! {
                result = &mut future => break Some(result),
                event = input.next() => match event {
                    Some(Ok(Event::Key(key))) if key.kind != KeyEventKind::Release &&
                        (key.code == KeyCode::Esc ||
                            (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))) => break None,
                    Some(Ok(Event::Resize(_, _))) => screen.draw(state)?,
                    Some(Err(error)) => return Err(error.into()),
                    None => break None,
                    _ => {},
                },
            }
        }
    };
    match result {
        Some(Ok(result)) => state.info("SHELL", serde_json::to_string_pretty(&result)?),
        Some(Err(error)) => state.note(format!("Shell: {error:#}")),
        None => state.note("Shell cancelled; completed side effects are not undone"),
    }
    screen.draw(state)?;
    Ok(())
}

pub async fn run(config: Config) -> Result<()> {
    run_with_resume(config, false).await
}

/// Plain `kroma` shows the welcome screen. Explicit --continue restores the
/// latest saved session; only the first real send creates a new conversation.
pub async fn run_with_resume(mut config: Config, resume_recent: bool) -> Result<()> {
    let _quiet = TuiLogGuard::enter();
    let catalog = crate::sessions::SessionCatalog::new(&config.workdir)?;
    let target_session = if resume_recent {
        catalog.recent()?
    } else {
        None
    };
    let show_home = target_session.is_none();
    let target_id = target_session.as_deref().unwrap_or("default");
    // Explicit environment/profile choices win over a previous TUI session.
    let allow_restore = [
        "KROMA_PROVIDER_PROFILE",
        "KROMA_MODEL",
        "KROMA_BASE_URL",
        "KROMA_API_KEY",
        "KROMA_API_PROTOCOL",
    ]
    .iter()
    .all(|key| std::env::var_os(key).is_none());
    let saved = if allow_restore && resume_recent && catalog.resolve(target_id).is_ok() {
        catalog.model_choice(target_id)?
    } else {
        None
    };
    let mut restored = false;
    let mut restore_warning = None;
    if let Some(choice) = saved.as_ref() {
        let profiles = crate::profiles::Profiles::load()?;
        if let Some(profile) = profiles.profiles.get(&choice.profile) {
            config = Config::from_profile(profile, config.workdir, Some(choice.profile.clone()))?;
            restored = true;
        } else {
            restore_warning = Some(format!(
                "Saved model profile '{}' is unavailable; using configured default",
                choice.profile
            ));
        }
    }
    let model_name = config.model.clone();
    let initial_profile = config.selected_profile.clone();
    let workdir = config.workdir.clone();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let observer = Arc::new(UiSink(tx.clone()));
    let mut model_handle: Option<Arc<dyn ModelBackend>> = None;
    let runtime = app::default_runtime_with(config, |components| {
        components.events = Some(observer);
        model_handle = Some(components.model());
        components.defer_initial_session = show_home;
        Ok(())
    })
    .await?;
    let mut model = model_handle.expect("default runtime model was initialized");
    if restored {
        if let Some(level) = saved.as_ref().and_then(|choice| choice.thinking.as_deref()) {
            restore_warning = restore_reasoning(model.as_ref(), level);
        }
    }
    let commands = CommandRegistry::builtin_with_warnings(
        runtime.skills.list(),
        runtime.startup_warnings.clone(),
    )?;
    let mut agent = runtime.agent;
    if let Some(target) = target_session.as_deref().filter(|id| *id != "default") {
        agent.resume_session(target)?;
    }
    let (themes, theme_warning) = match ThemeStore::load() {
        Ok(themes) => (themes, None),
        Err(error) => (
            ThemeStore::fallback(),
            Some(format!(
                "Invalid theme.toml ({error:#}); using dark. Fix the file and /theme reload."
            )),
        ),
    };
    let mut state = UiState::new(
        model_name,
        agent.session_id().to_owned(),
        themes,
        agent.messages(),
    );
    state.model_catalog = Some(catalog);
    state.refresh_session_title();
    state.model_profile = initial_profile;
    state.configure_status(agent.context_window_tokens(), model.reasoning_level());
    if show_home {
        state.enter_home();
    }
    save_model_choice(&mut state, model.as_ref());
    if let Some(warning) = restore_warning {
        state.note(warning);
    }
    if let Some(warning) = theme_warning {
        state.note(warning);
    }
    if !runtime.startup_warnings.is_empty() {
        state.note(format!(
            "{} MCP server(s) unavailable · /tools for details",
            runtime.startup_warnings.len()
        ));
    }
    let mut screen = TerminalScreen::enter()?;
    let mut input = EventStream::new();
    let task_input = agent.task_input();
    let mut run_followups = false;
    let mut selection_tick = tokio::time::interval(std::time::Duration::from_millis(55));
    selection_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        if run_followups {
            if let Some(next) = task_input.take_followup() {
                if !execute_task(
                    &mut agent,
                    &next,
                    Vec::new(),
                    &workdir,
                    model.as_ref(),
                    &mut state,
                    &mut screen,
                    &mut input,
                    &mut rx,
                )
                .await?
                {
                    run_followups = false;
                }
                continue;
            }
            run_followups = false;
        }
        screen.draw(&mut state)?;
        let action = tokio::select! {
            event=input.next() => match event {
                Some(Ok(Event::Key(key))) if key.code==KeyCode::Char('g')
                    && key.modifiers.contains(KeyModifiers::CONTROL) => {
                    if let Err(error)=screen.edit_prompt(&mut state) {
                        state.note(format!("Editor: {error:#}"));
                    }
                    InputAction::None
                },
                Some(Ok(event)) => input_event(&mut state,event,false,model.as_ref(),&workdir),
                Some(Err(error)) => return Err(error.into()),
                None => return Ok(()),
            },
            message=rx.recv() => {
                if let Some(message)=message {state.event(message);}
                InputAction::None
            },
            _=selection_tick.tick(), if state.selection_needs_auto_scroll() => {
                state.auto_scroll_selection();
                InputAction::None
            },
        };
        match action {
            InputAction::Quit => break,
            InputAction::Edit => {
                if let Err(error) = screen.edit_prompt(&mut state) {
                    state.note(format!("Editor: {error:#}"));
                }
            }
            InputAction::Submit(text) => {
                if text == "/editor" {
                    if let Err(error) = screen.edit_prompt(&mut state) {
                        state.note(format!("Editor: {error:#}"));
                    }
                    continue;
                }
                if text == "/model" {
                    open_picker(&mut state, PickerKind::Models, "", &workdir);
                    continue;
                }
                if text == "/sessions" || text == "/resume" {
                    open_picker(&mut state, PickerKind::Sessions, "", &workdir);
                    continue;
                }
                if text == "/new" {
                    // Do not create an empty archived session. The next
                    // actual user request allocates the session ID.
                    if let Err(error) = agent.enter_welcome() {
                        state.note(format!("Unable to return to welcome: {error:#}"));
                        continue;
                    }
                    state.discard_draft();
                    state.enter_home();
                    continue;
                }
                if let Some(name) = text.strip_prefix("/model ") {
                    let selected = crate::profiles::Profiles::load().and_then(|profiles| {
                        profiles
                            .profiles
                            .get(name.trim())
                            .cloned()
                            .context("Unknown saved model profile")
                    });
                    match selected {
                        Ok(profile) => match app::model_from_profile(&profile).await {
                            Ok((next, budget)) => match agent.change_model(next.clone(), budget) {
                                Ok(()) => {
                                    model = next;
                                    state.model = profile.model.clone();
                                    state.model_profile = Some(name.trim().to_owned());
                                    save_model_choice(&mut state, model.as_ref());
                                    state.configure_status(
                                        agent.context_window_tokens(),
                                        model.reasoning_level(),
                                    );
                                    state.reset_context();
                                    state
                                        .note(format!("Model: {} · profile {name}", profile.model));
                                }
                                Err(error) => state.note(format!("Model switch: {error:#}")),
                            },
                            Err(error) => state.note(format!("Model switch: {error:#}")),
                        },
                        Err(error) => state.note(format!("Model: {error:#}")),
                    }
                    continue;
                }
                if let Some(command) = text.strip_prefix('!') {
                    let command = command.trim();
                    if command.is_empty() {
                        state.note("Usage: !<shell command>");
                        continue;
                    }
                    execute_local_shell(
                        &mut screen,
                        &mut state,
                        &mut input,
                        command,
                        &workdir,
                        agent.archive_path(),
                    )
                    .await?;
                    continue;
                }
                let text = match crate::convenience::file_reference(&text, &workdir) {
                    Ok(rewritten) => rewritten,
                    Err(error) => {
                        state.note(format!("File reference: {error:#}"));
                        continue;
                    }
                };
                if show_local_command(&mut state, &text, model.as_ref()) {
                    continue;
                }
                match commands.execute(&text, &mut agent, &task_input) {
                    Ok(Some(reply)) => {
                        let before = state.session.clone();
                        state.session = agent.session_id().to_owned();
                        if state.session != before {
                            let saved = state
                                .model_catalog
                                .as_ref()
                                .context("session catalog missing")?
                                .model_choice(&state.session)?;
                            if let Some(choice) = saved {
                                if state.model_profile.as_deref() != Some(&choice.profile) {
                                    let profiles = crate::profiles::Profiles::load()?;
                                    if let Some(profile) = profiles.profiles.get(&choice.profile) {
                                        match app::model_from_profile(profile).await {
                                            Ok((next, budget)) => {
                                                match agent.change_model(next.clone(), budget) {
                                                    Ok(()) => {
                                                        model = next;
                                                        state.model = profile.model.clone();
                                                        state.model_profile =
                                                            Some(choice.profile.clone());
                                                    }
                                                    Err(error) => state
                                                        .note(format!("Session model: {error:#}")),
                                                }
                                            }
                                            Err(error) => {
                                                state.note(format!("Session model: {error:#}"))
                                            }
                                        }
                                    } else {
                                        state.note(format!(
                                            "Saved model profile '{}' is unavailable",
                                            choice.profile
                                        ));
                                    }
                                }
                                if state.model_profile.as_deref() == Some(&choice.profile) {
                                    if let Some(level) = choice.thinking.as_deref() {
                                        if let Some(warning) =
                                            restore_reasoning(model.as_ref(), level)
                                        {
                                            state.note(warning);
                                        }
                                    }
                                }
                            } else {
                                save_model_choice(&mut state, model.as_ref());
                            }
                            state.configure_status(
                                agent.context_window_tokens(),
                                model.reasoning_level(),
                            );
                        }
                        state.load_history(agent.messages());
                        state.refresh_session_title();
                        if state.session != before
                            || matches!(text.as_str(), "/clear" | "/undo")
                            || text.starts_with("/rewind ")
                        {
                            state.reset_context();
                            state.discard_draft();
                            state.restore_input_history(agent.messages());
                        }
                        if !reply.output.is_empty() {
                            state.info("COMMAND", reply.output);
                        }
                        match reply.action {
                            CommandAction::Exit => break,
                            CommandAction::RunFollowups => run_followups = true,
                            CommandAction::PauseFollowups => run_followups = false,
                            CommandAction::Continue => {}
                        }
                    }
                    Ok(None) => {
                        let first_prompt = if state.is_home() {
                            match agent.new_session() {
                                Ok(_) => {
                                    // Exit the transient welcome state before
                                    // persisting this conversation's model.
                                    state.load_history(agent.messages());
                                    state.session = agent.session_id().to_owned();
                                    state.refresh_session_title();
                                    save_model_choice(&mut state, model.as_ref());
                                    Some(text.clone())
                                }
                                Err(error) => {
                                    state.note(format!("Unable to create session: {error:#}"));
                                    continue;
                                }
                            }
                        } else {
                            None
                        };
                        let attachments = std::mem::take(&mut state.attachments);
                        run_followups = execute_task(
                            &mut agent,
                            &text,
                            attachments,
                            &workdir,
                            model.as_ref(),
                            &mut state,
                            &mut screen,
                            &mut input,
                            &mut rx,
                        )
                        .await?;
                        if let Some(first_prompt) = first_prompt {
                            if run_followups {
                                let assistant_text = agent
                                    .messages()
                                    .iter()
                                    .rev()
                                    .find(|message| message.role == "assistant")
                                    .and_then(|message| message.content.as_deref())
                                    .unwrap_or_default()
                                    .to_owned();
                                title::schedule(
                                    model.clone(),
                                    state
                                        .model_catalog
                                        .as_ref()
                                        .context("session catalog missing")?
                                        .default_path()
                                        .to_path_buf(),
                                    agent.session_id().to_owned(),
                                    first_prompt,
                                    assistant_text,
                                    tx.clone(),
                                );
                            }
                        }
                    }
                    Err(error) => state.note(format!("Command error: {error:#}")),
                }
            }
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ModelClient;
    use crossterm::event::KeyModifiers;

    fn setup() -> (UiState, ModelClient) {
        let model = ModelClient::new("http://localhost/v1", None, "test".into()).unwrap();
        let ui = UiState::new(
            "test".into(),
            "test".into(),
            ThemeStore::load_at(None).unwrap(),
            &[],
        );
        (ui, model)
    }

    #[test]
    fn tab_completes_builtin_slash_command() {
        let (mut ui, model) = setup();
        ui.editor.replace_all("/exi");
        let action = input_event(
            &mut ui,
            Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
            false,
            &model,
            std::path::Path::new("."),
        );
        assert!(matches!(action, InputAction::None));
        assert_eq!(ui.editor.text(), "/exit");
    }

    #[test]
    fn ctrl_click_on_missing_file_reports_failure_without_selecting_text() {
        use crossterm::event::{MouseButton, MouseEvent};
        use ratatui::{backend::TestBackend, Terminal};
        let (mut ui, model) = setup();
        ui.event(UiEvent::Text(
            "Read src/nonexistent-kroma-file.rs:42".into(),
        ));
        let mut terminal = Terminal::new(TestBackend::new(110, 27)).unwrap();
        terminal.draw(|frame| ui.draw(frame)).unwrap();
        let (x, y) = ui
            .test_link_position("nonexistent-kroma-file")
            .expect("file path should be clickable");
        let mouse = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: x,
            row: y,
            modifiers: KeyModifiers::CONTROL,
        };
        let result = input_event(
            &mut ui,
            Event::Mouse(mouse),
            false,
            &model,
            std::env::temp_dir().as_path(),
        );
        assert!(matches!(result, InputAction::None));
        assert!(
            ui.notification.contains("file not found"),
            "error feedback required"
        );
        assert!(
            ui.selected_text().is_none(),
            "Ctrl+click must not select text"
        );
    }

    #[test]
    fn common_picker_filters_commands_without_overwriting_unsent_draft() {
        let (mut ui, model) = setup();
        let dir = std::env::temp_dir();
        ui.editor.paste("keep this unsent note");
        let trigger = KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL);
        input_event(&mut ui, Event::Key(trigger), false, &model, &dir);
        assert_eq!(ui.picker.as_ref().unwrap().kind, PickerKind::Commands);
        for ch in "usage".chars() {
            input_event(
                &mut ui,
                Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)),
                false,
                &model,
                &dir,
            );
        }
        let result = input_event(
            &mut ui,
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            false,
            &model,
            &dir,
        );
        assert!(matches!(result, InputAction::Submit(text) if text == "/usage"));
        assert!(ui.picker.is_none());
        assert_eq!(ui.editor.text(), "keep this unsent note");
        input_event(&mut ui, Event::Key(trigger), false, &model, &dir);
        let tab = KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE);
        input_event(&mut ui, Event::Key(tab), false, &model, &dir);
        assert_eq!(ui.picker.as_ref().unwrap().kind, PickerKind::Models);
        input_event(
            &mut ui,
            Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            false,
            &model,
            &dir,
        );
        assert!(ui.picker.is_none());
        assert_eq!(ui.editor.text(), "keep this unsent note");
    }

    #[test]
    fn file_search_picker_replaces_only_the_file_word() -> Result<()> {
        let (mut ui, model) = setup();
        let dir = std::env::temp_dir().join(format!("kroma-picker-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("src"))?;
        for name in ["agent.rs", "app.rs"] {
            std::fs::write(dir.join("src").join(name), "// fixture")?;
        }
        ui.editor.paste("Review @src/a");
        input_event(
            &mut ui,
            Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
            false,
            &model,
            &dir,
        );
        assert_eq!(ui.picker.as_ref().unwrap().kind, PickerKind::Files);
        input_event(
            &mut ui,
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            false,
            &model,
            &dir,
        );
        assert!(ui.editor.text().starts_with("Review @src/"));
        assert!(ui.editor.text().ends_with(".rs"));
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    fn named_sessions_are_searchable_and_switch_preserves_the_draft() -> Result<()> {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let root = std::env::temp_dir().join(format!("kroma-picker-sessions-{nonce}"));
        std::fs::create_dir_all(&root)?;
        let catalog = crate::sessions::SessionCatalog::from_default(root.join("session.json"))?;
        let (id, path) = catalog.new_path()?;
        std::fs::write(path, "{}")?;
        catalog.name_session(&id, "Review Kroma")?;
        let (mut state, model) = setup();
        state.editor.paste("my unsent task");
        state.model_catalog = Some(catalog);
        state.session = id.clone();
        state.refresh_session_title();
        assert_eq!(state.session_label(), "Review Kroma");
        open_picker(&mut state, PickerKind::Sessions, "", &root);
        for ch in "Review".chars() {
            input_event(
                &mut state,
                Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)),
                false,
                &model,
                &root,
            );
        }
        let chosen = input_event(
            &mut state,
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            false,
            &model,
            &root,
        );
        assert!(matches!(chosen, InputAction::Submit(ref command)
            if command == &format!("/resume {id}")));
        assert_eq!(state.editor.text(), "my unsent task");
        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn thinking_picker_uses_current_backend_after_model_switch() {
        let (mut ui, _) = setup();
        let old = ModelClient::new("http://localhost/v1", None, "old".into())
            .unwrap()
            .with_reasoning("low".into(), None)
            .with_thinking_levels(vec!["low".into()]);
        let new = ModelClient::new("http://localhost/v1", None, "new".into())
            .unwrap()
            .with_reasoning("high".into(), None)
            .with_thinking_levels(vec!["high".into(), "xhigh".into()]);
        let key = KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT);
        key_action(&mut ui, key, false, &old);
        assert_eq!(old.reasoning_level().as_deref(), Some("low"));
        key_action(&mut ui, key, false, &new);
        assert_eq!(new.reasoning_level().as_deref(), Some("xhigh"));
        key_action(&mut ui, key, false, &new);
        assert_eq!(new.reasoning_level().as_deref(), Some("high"));
    }

    #[test]
    fn control_c_clears_draft_then_exits_or_cancels() {
        let (mut ui, model) = setup();
        let c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(matches!(
            key_action(&mut ui, c, false, &model),
            InputAction::Quit
        ));
        ui.editor.paste("unsubmitted");
        assert!(matches!(
            key_action(&mut ui, c, false, &model),
            InputAction::None
        ));
        assert!(ui.editor.text().is_empty());
        assert!(matches!(
            key_action(&mut ui, c, false, &model),
            InputAction::Quit
        ));
        ui.editor.paste("steer");
        assert!(matches!(
            key_action(&mut ui, c, true, &model),
            InputAction::Cancel
        ));
        assert_eq!(ui.editor.text(), "steer");
    }

    #[test]
    fn escape_preserves_draft_and_dismisses_transient_feedback() {
        let (mut ui, model) = setup();
        let dir = std::env::temp_dir();
        ui.editor.paste("a long unsent draft");
        ui.note("Old notification");
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        assert!(matches!(
            input_event(&mut ui, Event::Key(esc), false, &model, &dir),
            InputAction::None
        ));
        assert_eq!(ui.editor.text(), "a long unsent draft");
        assert!(ui.notification.is_empty());
        ui.attach_mode = true;
        ui.attach_path = "temporary-path".into();
        input_event(&mut ui, Event::Key(esc), false, &model, &dir);
        assert!(!ui.attach_mode);
        assert!(ui.attach_path.is_empty());
        assert_eq!(ui.editor.text(), "a long unsent draft");
        // Ctrl+C remains the explicit clear operation when idle.
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        input_event(&mut ui, Event::Key(ctrl_c), false, &model, &dir);
        assert!(ui.editor.text().is_empty());
    }

    #[test]
    fn active_task_does_not_accept_unroutable_media() {
        let (mut ui, model) = setup();
        let dir = std::env::temp_dir();
        assert!(matches!(
            input_event(
                &mut ui,
                Event::Key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::ALT)),
                true,
                &model,
                &dir
            ),
            InputAction::None
        ));
        assert!(ui.attachments.is_empty());
        assert!(ui.notification.contains("after this task"));
        let log = "long error\n".repeat(20);
        input_event(&mut ui, Event::Paste(log), true, &model, &dir);
        assert!(ui.attachments.is_empty());
    }

    #[test]
    fn recalled_media_is_real_and_discarding_draft_never_deletes_sent_file() {
        use std::time::{SystemTime, UNIX_EPOCH};
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("kroma-recall-draft-{stamp}"));
        std::fs::create_dir_all(&dir).unwrap();
        let original = dir.join("original.png");
        std::fs::write(&original, b"persistent historical image").unwrap();
        let marker = char::from_u32(super::editor::ATTACHMENT_START).unwrap();
        let mut message = crate::protocol::Message::new("user", format!("Show {marker}"));
        message.provider_items = Some(vec![serde_json::json!({
            "type":"kroma_attachment","kind":"image","name":"original.png",
            "path":original,"mime":"image/png","bytes":27,"marker":marker.to_string()
        })]);
        let model = ModelClient::new("http://localhost/v1", None, "test".into()).unwrap();
        let mut ui = UiState::new(
            "test".into(),
            "test".into(),
            ThemeStore::load_at(None).unwrap(),
            &[message],
        );
        let up = KeyEvent::new(KeyCode::Up, KeyModifiers::NONE);
        assert!(matches!(
            key_action(&mut ui, up, false, &model),
            InputAction::None
        ));
        assert!(ui.editor.visible_text().contains("【original.png】"));
        assert_eq!(ui.attachments.len(), 1);
        assert!(!ui.attachments[0].draft_copy);
        let c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(matches!(
            key_action(&mut ui, c, false, &model),
            InputAction::None
        ));
        assert!(
            original.exists(),
            "discarding a recalled chip must not delete durable source"
        );
        assert!(matches!(
            key_action(&mut ui, up, false, &model),
            InputAction::None
        ));
        ui.editor
            .key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        ui.prune_attachments();
        assert!(ui.attachments.is_empty());
        assert!(
            original.exists(),
            "removing a recalled chip must not delete historic source"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn undo_restores_pending_attachment_and_its_file() {
        let (mut ui, model) = setup();
        let dir =
            std::env::temp_dir().join(format!("kroma-undo-attachment-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("image.png");
        std::fs::write(&path, b"\x89PNG\r\n\x1a\nimage").unwrap();
        let item = Attachment::from_file(&path, &dir).unwrap();
        let stored = item.path.clone();
        ui.attach(item).unwrap();
        let backspace = KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE);
        let undo = KeyEvent::new(KeyCode::Char('z'), KeyModifiers::CONTROL);
        assert!(matches!(
            key_action(&mut ui, backspace, false, &model),
            InputAction::None
        ));
        assert!(ui.attachments.is_empty());
        assert!(stored.exists());
        assert!(matches!(
            key_action(&mut ui, undo, false, &model),
            InputAction::None
        ));
        assert_eq!(ui.attachments.len(), 1);
        assert_eq!(ui.attachments[0].path, stored);
        ui.discard_draft();
        assert!(!stored.exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn long_paste_is_an_attachment_and_its_content_is_not_in_ui() {
        let (mut ui, model) = setup();
        let dir = std::env::temp_dir().join(format!("kroma-tui-paste-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let text = "line of error output\n".repeat(300);
        assert!(matches!(
            input_event(&mut ui, Event::Paste(text.clone()), false, &model, &dir),
            InputAction::None
        ));
        assert!(!ui.editor.text().is_empty());
        assert!(ui.editor.visible_text().contains("【"));
        assert_eq!(ui.attachments.len(), 1);
        assert_eq!(
            std::fs::read_to_string(&ui.attachments[0].path).unwrap(),
            text
        );
        let projected =
            crate::attachments::content(ui.editor.text(), &[ui.attachments[0].item()], "chat")
                .unwrap();
        assert!(!projected.to_string().contains("line of error output"));
        assert!(projected.to_string().contains("shell"));
        let c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(matches!(
            key_action(&mut ui, c, false, &model),
            InputAction::None
        ));
        assert!(ui.attachments.is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn image_is_projected_into_real_multimodal_provider_content() {
        let dir = std::env::temp_dir().join(format!("kroma-tui-image-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let image = arboard::ImageData {
            width: 1,
            height: 1,
            bytes: std::borrow::Cow::Borrowed(&[255, 0, 0, 255]),
        };
        let item = attachments::Attachment::from_pixels(image, &dir)
            .unwrap()
            .item();
        for protocol in ["chat", "responses", "anthropic"] {
            let content =
                crate::attachments::content("Describe this", [item.clone()].as_slice(), protocol)
                    .unwrap();
            assert!(content.to_string().contains("base64"));
            assert!(content.to_string().contains("image"));
            assert!(!content.to_string().contains("paste-"));
        }
        std::fs::remove_file(item["path"].as_str().unwrap()).unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }
}
