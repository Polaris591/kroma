//! Default Kroma terminal UI. A thin host of the same Agent used by the CLI.
//! Themes are TOML data. No UI plugin runtime, RPC service or second Agent loop.
mod attachments;
mod editor;
mod markdown;
mod theme;
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
        KeyCode::Tab if !running => {
            let prefix = state.editor.text().trim();
            if prefix.starts_with('/') && !prefix.contains(char::is_whitespace) {
                let matches = [
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
                ]
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
            if state.attach_mode {
                state.attach_mode = false;
                state.attach_path.clear();
            } else {
                state.discard_draft();
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
    match event {
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
                        state.info("FILES", matches.join("  ·  "));
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
            MouseEventKind::Down(crossterm::event::MouseButton::Left) => {
                state.select_start(mouse.column, mouse.row)
            }
            MouseEventKind::Drag(crossterm::event::MouseButton::Left) => {
                state.select_drag(mouse.column, mouse.row)
            }
            MouseEventKind::Down(crossterm::event::MouseButton::Right) => {
                if let Some(selected) = state.selected_text() {
                    // OSC 52 copies to the terminal's clipboard, including
                    // supported remote/SSH terminals. No host-side X11 or
                    // platform-specific clipboard dependency is required.
                    let encoded = base64::Engine::encode(
                        &base64::engine::general_purpose::STANDARD,
                        selected.as_bytes(),
                    );
                    use std::io::Write;
                    let _ = write!(io::stdout(), "\x1b]52;c;{}\x07", encoded);
                    let _ = io::stdout().flush();
                    state.note("Selection copied (if terminal supports OSC 52)");
                }
            }
            _ => {}
        },
        _ => {}
    }
    InputAction::None
}

fn show_local_command(state: &mut UiState, input: &str, model: &dyn ModelBackend) -> bool {
    if input == "/help" {
        state.info("HELP", "Enter to send; Shift+Enter or Ctrl+J to add a line.\nPgUp/PgDn or mouse wheel scroll; F2 changes theme.\nShift+Tab cycles thinking (or /think high).\nCtx shows last request: ~ estimated, otherwise API-reported input.\nCtrl+O expands tool output; Ctrl+R shows reasoning.\n/history [count] · /sessions · /resume ID · /fork N · /rewind N · /undo\n/theme [dark|light|mono|reload] · /think [auto|off|low|medium|high|xhigh|max]\n/queue task · /skills · /tools\nWhile running: write steering text, or /queue task; Ctrl+C cancels.\nCtrl+Q quits when idle.");
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
                    if dirty {
                        screen.draw(state)?;
                        dirty=false;
                    }
                }
            }
        }
    };
    let completed = match result {
        Some(Ok(_)) => true,
        Some(Err(error)) => {
            state.note(format!("Task failed: {error:#}"));
            false
        }
        None => {
            agent.cancel()?;
            state.note("Task cancelled; unconfirmed commands are not replayed");
            false
        }
    };
    // The transcript, not live UI events, is authoritative after completion.
    while rx.try_recv().is_ok() {}
    state.load_history(agent.messages());
    state.session = agent.session_id().to_owned();
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

/// Continue the most recently modified conversation only on explicit request.
/// A plain `kroma` still opens the default session as before.
pub async fn run_with_resume(mut config: Config, resume_recent: bool) -> Result<()> {
    let _quiet = TuiLogGuard::enter();
    let catalog = crate::sessions::SessionCatalog::new(&config.workdir)?;
    let target_session = if resume_recent {
        catalog.recent()?
    } else {
        None
    };
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
    let saved = if allow_restore && catalog.resolve(target_id).is_ok() {
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
    let observer = Arc::new(UiSink(tx));
    let mut model_handle: Option<Arc<dyn ModelBackend>> = None;
    let runtime = app::default_runtime_with(config, |components| {
        components.events = Some(observer);
        model_handle = Some(components.model.clone());
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
    state.model_profile = initial_profile;
    state.configure_status(agent.context_window_tokens(), model.reasoning_level());
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
                    match crate::profiles::Profiles::load() {
                        Ok(profiles) => {
                            let active = state.model_profile.as_deref();
                            state.info(
                                "MODELS",
                                profiles
                                    .profiles
                                    .iter()
                                    .map(|(name, p)| {
                                        format!(
                                            "{} {name} · {} · {}",
                                            if active == Some(name.as_str()) {
                                                "*"
                                            } else {
                                                " "
                                            },
                                            p.model,
                                            p.api_protocol.as_deref().unwrap_or("chat")
                                        )
                                    })
                                    .collect::<Vec<_>>()
                                    .join("\n"),
                            );
                        }
                        Err(error) => state.note(format!("Models: {error:#}")),
                    }
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
