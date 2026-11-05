use anyhow::{bail, Context, Result};
use kroma::{
    agent::Agent,
    app,
    commands::{pending_text, CommandAction, CommandRegistry},
    config::Config,
    profiles,
    steering::TaskInput,
};
use std::{
    env,
    io::{self, IsTerminal, Write},
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader, Lines, Stdin};

type InputLines = Lines<BufReader<Stdin>>;

/// Set when SIGTERM asked Kroma to stop. The CLI finishes the current turn
/// (cancelling it like Ctrl+C) and then exits instead of returning to the prompt.
static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);

/// `kill` must stop the same way Ctrl+C does. The listener is created once and
/// shared, so a signal delivered between two waits is never lost, and installing
/// it never leaves the process alive with a handler nobody polls.
async fn terminate_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        use tokio::sync::{Mutex, OnceCell};
        static LISTENER: OnceCell<Option<Mutex<tokio::signal::unix::Signal>>> =
            OnceCell::const_new();
        let listener = LISTENER
            .get_or_init(|| async { signal(SignalKind::terminate()).ok().map(Mutex::new) })
            .await;
        match listener {
            Some(listener) => {
                let mut guard = listener.lock().await;
                guard.recv().await;
            }
            // Without a handler the default terminate disposition stays.
            None => std::future::pending::<()>().await,
        }
    }
    #[cfg(not(unix))]
    std::future::pending::<()>().await;
}

async fn run_legacy(options: kroma::cli::RunOptions) -> Result<()> {
    let task = options.task();
    let force_cli = options.no_tui;
    let config = Config::from_env()?;
    if !options.one_shot()
        && options.prompt.is_empty()
        && !force_cli
        && options.continue_latest
        && io::stdin().is_terminal()
        && io::stdout().is_terminal()
    {
        return kroma::tui::run_with_resume(config, true).await;
    }
    let mut model_name = config.model.clone();
    let quiet = options.quiet || env::var_os("KROMA_QUIET").is_some();
    let verbose = options.verbose || env::var_os("KROMA_VERBOSE").is_some();
    let json_mode = options.json;
    let print_mode = options.one_shot();
    if verbose {
        let endpoint = reqwest::Url::parse(&config.base_url)
            .map(|mut url| {
                let _ = url.set_username("");
                let _ = url.set_password(None);
                url.set_query(None);
                url.set_fragment(None);
                url.to_string()
            })
            .unwrap_or_else(|_| config.base_url.clone());
        eprintln!(
            "[config] model={} endpoint={} protocol={} workdir={}",
            config.model,
            endpoint,
            config.api_protocol,
            config.workdir.display()
        );
    }
    if print_mode {
        let mut piped = String::new();
        if !io::stdin().is_terminal() {
            tokio::io::stdin()
                .take(8 * 1024 * 1024 + 1)
                .read_to_string(&mut piped)
                .await?;
            if piped.len() > 8 * 1024 * 1024 {
                bail!("stdin exceeds 8 MiB; pipe a file path instead");
            }
        }
        let prompt = if !piped.is_empty() {
            let referenced = if piped.len() > 16 * 1024 {
                let path = kroma::convenience::save_pipe_input(piped.as_bytes())?;
                format!(
                    "Piped input saved at {}. Read relevant portions with Shell.",
                    path.display()
                )
            } else {
                format!("Piped input:\n{piped}")
            };
            format!("{task}\n\n{referenced}")
        } else {
            task
        };
        if prompt.trim().is_empty() {
            bail!("Usage: kroma --print <task> or pipe input to --print");
        }
        let output = kroma::output::CliOutput::new();
        let runtime = if json_mode {
            app::default_runtime_with(config, |components| {
                let sink: Arc<dyn kroma::events::EventSink> =
                    Arc::new(kroma::cli_events::JsonEvents::new(output.clone()));
                components.events = Some(sink);
                Ok(())
            })
            .await?
        } else {
            app::default_runtime(config).await?
        };
        if !quiet {
            for warning in runtime
                .startup_warnings
                .iter()
                .chain(runtime.plugin_warnings.iter())
            {
                eprintln!("[startup] {warning}");
            }
        }
        let mut agent = runtime.agent;
        let prompt = kroma::convenience::file_reference(&prompt, agent.workdir())?;
        if json_mode {
            let result = drive_task(&mut agent, &prompt, None, &mut |_| {}).await;
            match result {
                Ok(Some(answer)) => {
                    output.line(&serde_json::json!({"type":"result","text":answer}).to_string())?;
                }
                Ok(None) => bail!("task cancelled"),
                Err(error) => {
                    output.line(
                        &serde_json::json!({"type":"error","message":format!("{error:#}")})
                            .to_string(),
                    )?;
                    if output.broken() {
                        return Ok(());
                    }
                    bail!("task failed");
                }
            }
            if output.broken() {
                return Ok(());
            }
        } else if !run_task(&mut agent, &prompt, None, Some(&output)).await? {
            bail!("task cancelled");
        }
        return Ok(());
    }
    if !force_cli
        && options.prompt.is_empty()
        && io::stdin().is_terminal()
        && io::stdout().is_terminal()
    {
        return kroma::tui::run(config).await;
    }
    let runtime = app::default_runtime(config).await?;
    if let Some(name) = &runtime.selected_model_name {
        model_name = name.clone();
    }
    if !quiet {
        for warning in &runtime.startup_warnings {
            eprintln!("[startup] {warning}");
        }
    }
    let mut commands =
        CommandRegistry::builtin_with_warnings(runtime.skills.list(), runtime.startup_warnings)?;
    if !quiet {
        for warning in runtime
            .plugin_warnings
            .iter()
            .chain(runtime.plugins.register_commands(&mut commands).iter())
        {
            eprintln!("[plugin] {warning}");
        }
    } else {
        let _ = runtime.plugins.register_commands(&mut commands);
    }
    let mut agent = runtime.agent;
    let line_output = kroma::output::CliOutput::new();

    if !options.prompt.is_empty() {
        if !run_task(&mut agent, &task, None, Some(&line_output)).await? {
            bail!("task cancelled");
        }
        return Ok(());
    }

    kroma::cli_println!(
        "Kroma v0.4 | model: {} | session: {} | single-pass compaction | Ctrl+C cancels | /compact | /history | /fork | /rewind | /undo | /new | /resume | /sessions | /skills | /tools | /queue | /exit",
        model_name,
        agent.session_id()
    );
    let stdin = BufReader::new(tokio::io::stdin());
    let mut lines = stdin.lines();
    let input_handle = agent.task_input();
    let mut run_followups = false;
    loop {
        if SHUTDOWN_REQUESTED.load(Ordering::SeqCst) {
            break;
        }
        let queued = if run_followups {
            input_handle.take_followup()
        } else {
            None
        };
        let from_queue = queued.is_some();
        let input = if let Some(next) = queued {
            eprintln!(
                "[queue] Running next task ({} remain).",
                input_handle.queued().len()
            );
            next
        } else {
            run_followups = false;
            kroma::cli_print!("kroma> ");
            io::stdout().flush()?;
            tokio::select! {
                input = lines.next_line() => match input? {
                    Some(text) => text,
                    None => break,
                },
                signal = tokio::signal::ctrl_c() => {
                    signal?;
                    kroma::cli_println!();
                    continue;
                },
                _ = terminate_signal() => {
                    kroma::cli_println!();
                    break;
                }
            }
        };
        let input = input.trim();
        // Queued items are always user tasks, not CLI commands. A user may
        // deliberately queue a task whose text starts with a slash.
        if from_queue {
            match run_task(&mut agent, input, Some(&mut lines), Some(&line_output)).await {
                Ok(true) => run_followups = true,
                Ok(false) => {
                    run_followups = false;
                    eprintln!("[queue] Paused after cancellation; /queue run resumes.");
                }
                Err(error) => {
                    run_followups = false;
                    eprintln!("Error: {error:#}\n[queue] Paused; /queue run resumes.");
                }
            }
            continue;
        }
        if input.is_empty() {
            continue;
        }
        if let Some(command) = input.strip_prefix('!') {
            if command.trim().is_empty() {
                eprintln!("Usage: !<shell command>");
                continue;
            }
            // Just like agent tasks and TUI !commands, dropping the future
            // must terminate the entire Shell process group on Ctrl+C.
            let shell = kroma::shell::execute_for_session(
                command.trim(),
                agent.workdir(),
                agent.archive_path(),
            );
            tokio::pin!(shell);
            let outcome = tokio::select! {
                result = &mut shell => Some(result),
                signal = tokio::signal::ctrl_c() => { signal?; None },
                _ = terminate_signal() => {
                    SHUTDOWN_REQUESTED.store(true, Ordering::SeqCst);
                    None
                },
            };
            match outcome {
                Some(Ok(output)) => {
                    kroma::cli_println!("{}", serde_json::to_string_pretty(&output)?)
                }
                Some(Err(error)) => eprintln!("Shell: {error:#}"),
                None => eprintln!("[shell] Cancelled; completed side effects are not undone."),
            }
            continue;
        }
        let command_result = {
            let command_future = commands.execute_async(input, &mut agent, &input_handle);
            tokio::pin!(command_future);
            tokio::select! {
                result = &mut command_future => Some(result),
                signal = tokio::signal::ctrl_c() => {
                    signal?;
                    None
                }
                _ = terminate_signal() => {
                    SHUTDOWN_REQUESTED.store(true, Ordering::SeqCst);
                    None
                }
            }
        };
        let Some(command_result) = command_result else {
            if input.trim_start().starts_with("/compact") {
                agent.cancel_compaction();
            }
            eprintln!("[command] Command cancelled");
            continue;
        };
        match command_result {
            Ok(Some(reply)) => {
                if !reply.output.is_empty() {
                    kroma::cli_println!("{}", reply.output);
                }
                match reply.action {
                    CommandAction::Continue => {}
                    CommandAction::Exit => break,
                    CommandAction::RunFollowups => run_followups = true,
                    CommandAction::PauseFollowups => run_followups = false,
                }
                continue;
            }
            Err(error) => {
                eprintln!("[command] {error:#}");
                continue;
            }
            Ok(None) => {} // Ordinary user input, including unregistered slash text.
        }
        let input = kroma::convenience::file_reference(input, agent.workdir())?;
        match run_task(&mut agent, &input, Some(&mut lines), Some(&line_output)).await {
            Ok(true) => run_followups = true,
            Ok(false) => {
                run_followups = false;
                if !input_handle.queued().is_empty() {
                    eprintln!("[queue] Paused after cancellation; use /queue run to continue.");
                }
            }
            Err(error) => {
                eprintln!("Error: {error:#}");
                run_followups = false;
                if !input_handle.queued().is_empty() {
                    eprintln!("[queue] Paused after error; use /queue run to continue.");
                }
            }
        }
    }
    Ok(())
}

/// Returns false when the user cancelled this task. Interactive mode continues.
async fn run_task(
    agent: &mut Agent,
    input: &str,
    lines: Option<&mut InputLines>,
    output: Option<&Arc<kroma::output::CliOutput>>,
) -> Result<bool> {
    let mut saw_text = false;
    let mut ends_with_newline = true;
    let result = drive_task(agent, input, lines, &mut |text| {
        saw_text = true;
        ends_with_newline = text.ends_with('\n');
        if let Some(output) = output {
            let _ = output.text(text);
        } else {
            kroma::cli_print!("{text}");
            let _ = io::stdout().flush();
        }
    })
    .await;
    if saw_text && !ends_with_newline {
        if let Some(output) = output {
            let _ = output.text(
                "
",
            );
        } else {
            kroma::cli_println!();
        }
    }
    if output.is_some_and(|output| output.broken()) {
        return Ok(true);
    }
    match result? {
        Some(answer) => {
            if !saw_text {
                if let Some(output) = output {
                    output.line(&answer)?;
                } else {
                    kroma::cli_println!("{answer}");
                }
            }
            Ok(true)
        }
        None => {
            eprintln!("[task] Cancelled. The current model request and shell were stopped.");
            Ok(false)
        }
    }
}

/// Shared CLI/JSON driver. Drop the running future (and its process-group
/// guard) before recording cancellation. JSON events come from Agent::cancel.
async fn drive_task(
    agent: &mut Agent,
    input: &str,
    mut lines: Option<&mut InputLines>,
    on_text: &mut (impl FnMut(&str) + Send),
) -> Result<Option<String>> {
    let input_handle = agent.task_input();
    let mut stdin_closed = false;
    let result = {
        let mut task = Box::pin(agent.run(input, on_text));
        loop {
            // `biased` ordering is load-bearing: the task future must be polled
            // before stdin. Agent::run activates the steering inbox on its first
            // poll, so polling stdin first would let already-buffered piped input
            // (for example `/steer <text>`) reach TaskInput::steer before any
            // task is active, and that instruction would be rejected and lost.
            tokio::select! {
                biased;
                signal = tokio::signal::ctrl_c() => {
                    signal?;
                    break None;
                }
                _ = terminate_signal() => {
                    eprintln!("[task] Stopped by SIGTERM.");
                    SHUTDOWN_REQUESTED.store(true, Ordering::SeqCst);
                    break None;
                }
                result = &mut task => break Some(result),
                next = async {
                    match lines.as_mut() {
                        Some(lines) => lines.next_line().await,
                        None => Ok(None),
                    }
                }, if lines.is_some() && !stdin_closed => match next? {
                    Some(text) => handle_running_input(&input_handle, text.trim()),
                    None => stdin_closed = true,
                },
            }
        }
    };
    match result {
        Some(result) => result.map(Some),
        None => {
            if let Err(error) = agent.cancel() {
                eprintln!("[task] Cancelled, but the session could not record it: {error:#}");
            }
            Ok(None)
        }
    }
}

fn print_pending(input: &TaskInput) {
    eprintln!("{}", pending_text(input));
}

fn handle_running_input(handle: &TaskInput, text: &str) {
    if text.is_empty() {
        return;
    }
    if matches!(text, "/pending" | "/queue list") {
        print_pending(handle);
    } else if text == "/queue clear" {
        eprintln!(
            "[queue] Cleared {} pending task(s).",
            handle.clear_followups()
        );
    } else if text == "/queue run" {
        eprintln!("[queue] Already running; pending tasks will execute after this one.");
    } else if let Some(task) = text.strip_prefix("/queue ") {
        match handle.queue_followup(task) {
            Ok(position) => eprintln!("[queue] Added follow-up task #{position}."),
            Err(error) => eprintln!("[queue] {error:#}"),
        }
    } else if text == "/queue" || text == "/steer" {
        eprintln!("Usage: /steer <instruction> or /queue <task>.");
    } else if matches!(
        text,
        "/exit"
            | "/quit"
            | "/new"
            | "/clear"
            | "/sessions"
            | "/undo"
            | "/history"
            | "/fork"
            | "/rewind"
            | "/checkpoints"
            | "/restore-files"
    ) || text.starts_with("/resume ")
        || text.starts_with("/history ")
        || text.starts_with("/fork ")
        || text.starts_with("/rewind ")
        || text.starts_with("/restore-files ")
    {
        eprintln!("[task] Session commands are available after the current task. Ctrl+C cancels.");
    } else {
        let instruction = text.strip_prefix("/steer ").unwrap_or(text);
        match handle.steer(instruction) {
            Ok(()) => eprintln!("[steer] Accepted; will apply at the next model turn."),
            Err(error) => eprintln!("[steer] {error:#}"),
        }
    }
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (globals, command) = match kroma::cli::parse_with_globals(&args) {
        Ok(parsed) => parsed,
        Err(error) => {
            eprintln!("Error: {}", error.message);
            return std::process::ExitCode::from(error.code);
        }
    };
    // Globals belong to every command, so their environment overrides are
    // applied once here instead of inside `run`.
    for (key, value) in globals.environment() {
        env::set_var(key, value);
    }
    match dispatch(command).await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{}", friendly_error(&error));
            std::process::ExitCode::from(exit_code(&error))
        }
    }
}

async fn dispatch(command: kroma::cli::Command) -> Result<()> {
    use kroma::cli::{self, Command};
    match command {
        Command::Help(topic) => {
            kroma::cli_println!("{}", cli::help_for(topic.as_deref().unwrap_or("run")));
            Ok(())
        }
        Command::Version { build_info } => {
            kroma::cli_println!(
                "kroma {} ({})",
                env!("CARGO_PKG_VERSION"),
                env!("KROMA_BUILD_REVISION")
            );
            if build_info {
                kroma::cli_println!("executable: {}", std::env::current_exe()?.display());
            }
            Ok(())
        }
        Command::Update { check, channel } => kroma::update::run(check, &channel).await,
        Command::Setup => kroma::setup::run().await,
        Command::Config(args) => {
            if wants_help(&args) {
                kroma::cli_println!("{}", cli::help_for("config"));
                return Ok(());
            }
            if let [cmd, name] = args.as_slice() {
                if cmd == "test" {
                    return test_profile(name).await;
                }
            }
            profiles::cli(&args)
        }
        Command::Plugins(args) => {
            if wants_help(&args) {
                kroma::cli_println!("{}", cli::help_for("plugins"));
                return Ok(());
            }
            kroma::plugins::PluginCatalog::cli(&args)
        }
        Command::Diagnostics(output) => {
            let path = kroma::diagnostics_bundle::generate(output)?;
            kroma::cli_println!(
                "Diagnostics written to {} (local only; no upload, model request or plugin process).",
                path.display()
            );
            Ok(())
        }
        Command::TrustProject { revoke } => {
            let dir = env::var("KROMA_WORKDIR")
                .map(PathBuf::from)
                .or_else(|_| env::current_dir())?
                .canonicalize()?;
            if revoke {
                kroma::mcp::revoke_project_mcp(&dir)?;
                kroma::cli_println!(
                    "Revoked project MCP trust for {}. The project's .kroma/mcp.json will not load until approved again.",
                    dir.display()
                );
            } else {
                let config = kroma::mcp::trust_project_mcp(&dir)?;
                kroma::cli_println!(
                    "Trusted project MCP config {}. Re-run this command after editing the file.",
                    config.display()
                );
            }
            Ok(())
        }
        Command::RecoverContext(session) => {
            let dir = env::var("KROMA_WORKDIR")
                .map(PathBuf::from)
                .or_else(|_| env::current_dir())?
                .canonicalize()?;
            let session = session.unwrap_or_else(|| "default".into());
            if Agent::recover_builtin_context(&dir, &session)? {
                kroma::cli_println!("Recovered session '{session}' using built-in context; complete original history preserved.");
            } else {
                kroma::cli_println!(
                    "Session '{session}' already uses built-in context; no change."
                );
            }
            Ok(())
        }
        Command::Run(options) => {
            if let Err(error) = Config::from_env() {
                let interactive = options.prompt.is_empty()
                    && !options.one_shot()
                    && io::stdin().is_terminal()
                    && io::stdout().is_terminal();
                if interactive && is_missing_model(&error) {
                    kroma::setup::run().await?;
                } else {
                    return Err(error);
                }
            }
            prepare_one_shot_session(&options)?;
            run_legacy(options).await
        }
    }
}

fn wants_help(args: &[String]) -> bool {
    args.iter().any(|arg| arg == "-h" || arg == "--help")
}

fn prepare_one_shot_session(options: &kroma::cli::RunOptions) -> Result<()> {
    if options.new_session {
        // A stale TUI target must not turn --new back into a resume.
        env::remove_var("KROMA_TUI_SESSION");
    }
    if !options.one_shot() && options.session.is_none() {
        let line_host = options.no_tui || !io::stdin().is_terminal() || !io::stdout().is_terminal();
        let wants_line_mode = line_host || !options.prompt.is_empty();
        if options.new_session && wants_line_mode {
            // Create the fresh path below; do not let the line host reuse default.
        } else if !options.continue_latest
            || !wants_line_mode
            || (options.prompt.is_empty() && !line_host)
        {
            return Ok(());
        }
    }
    let explicit_env = env::var_os("KROMA_SESSION_FILE");
    if explicit_env.is_some()
        && options.session.is_none()
        && !options.new_session
        && !options.continue_latest
    {
        return Ok(());
    }
    let workdir = env::var("KROMA_WORKDIR")
        .map(PathBuf::from)
        .or_else(|_| env::current_dir())?
        .canonicalize()
        .context("project path does not exist")?;
    let catalog = kroma::sessions::SessionCatalog::new(&workdir)?;
    let path = if let Some(name) = &options.session {
        match catalog.resolve_name(name) {
            Ok(id) if id != "default" => catalog.resolve(&id)?,
            Ok(_) => catalog.path_for("default")?,
            Err(error) if catalog.title_matches(name)? => return Err(error),
            Err(_) => catalog.path_for(name)?,
        }
    } else if options.continue_latest {
        let id = catalog
            .recent()?
            .ok_or_else(|| anyhow::anyhow!("no saved conversation; run a task first"))?;
        catalog.resolve(&id)?
    } else {
        catalog.new_path()?.1
    };
    env::set_var("KROMA_SESSION_FILE", &path);
    if !options.one_shot() {
        if let Some(name) = &options.session {
            env::set_var("KROMA_TUI_SESSION", name);
        }
    }
    Ok(())
}

fn is_missing_model(error: &anyhow::Error) -> bool {
    format!("{error:#}")
        .contains("KROMA_MODEL, an enabled model plugin or a saved profile is required")
}

fn friendly_error(error: &anyhow::Error) -> String {
    let text = format!("{error:#}");
    let hint = if text.contains("401") || text.contains("403") {
        Some("Hint: check this profile's API key with `kroma config test <name>` or `kroma setup`.")
    } else if text.contains("404") && (text.contains("model") || text.contains("Model")) {
        Some("Hint: check the model ID with `kroma setup` or `kroma config show`.")
    } else if text.contains("error sending request")
        || text.contains("connection refused")
        || text.contains("dns error")
    {
        Some(
            "Hint: the model endpoint is unreachable; check KROMA_BASE_URL or `kroma config show`.",
        )
    } else if text.contains("session already in use") {
        Some(
            "Hint: use `--new` for a separate run, or `--session <name>` for another conversation.",
        )
    } else if text.contains("context near hard limit") {
        Some("Hint: reduce MCP tools (--no-mcp), shorten the task, or use a larger model.")
    } else if text.contains("KROMA_MODEL, an enabled model plugin or a saved profile is required") {
        Some("Hint: run `kroma setup` (interactive) or set KROMA_MODEL, KROMA_BASE_URL and KROMA_API_KEY.")
    } else {
        None
    };
    match hint {
        Some(hint) => format!("Error: {text}\n{hint}"),
        None => format!("Error: {text}"),
    }
}

fn exit_code(error: &anyhow::Error) -> u8 {
    let text = format!("{error:#}");
    if text.contains("401") || text.contains("403") {
        4
    } else if text.contains("KROMA_MODEL, an enabled model plugin or a saved profile is required") {
        3
    } else if text.contains("context near hard limit") {
        6
    } else if text.contains("model returned HTTP") || text.contains("request failed") {
        5
    } else {
        1
    }
}

async fn test_profile(name: &str) -> Result<()> {
    let profiles = profiles::Profiles::load()?;
    let profile = profiles
        .profiles
        .get(name)
        .cloned()
        .with_context(|| format!("unknown profile '{name}'; use `kroma config list`"))?;
    let started = std::time::Instant::now();
    let (model, _, _) = app::model_from_profile(&profile).await?;
    let result = model
        .generate_short_text("You are a connectivity test. Reply with OK.", "ping", 16)
        .await?;
    kroma::cli_println!(
        "Profile '{name}' connected in {:?}: {}",
        started.elapsed(),
        result.text.trim()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_input_routes_to_steering_or_follow_up_instead_of_mixing() {
        let inbox = TaskInput::default();
        let task = inbox.queue_followup("second turn").unwrap();
        assert_eq!(task, 1);
        // The steering handle activates only while Agent::run is polling;
        // live commands received outside a task are rejected.
        handle_running_input(&inbox, "/steer wrong-time");
        assert_eq!(inbox.pending_steering(), 0);
        handle_running_input(&inbox, "/queue third turn");
        assert_eq!(inbox.queued(), vec!["second turn", "third turn"]);
        handle_running_input(&inbox, "/queue list");
        handle_running_input(&inbox, "/queue clear");
        assert!(inbox.queued().is_empty());
    }
}
