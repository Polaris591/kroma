use anyhow::{bail, Result};
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
    sync::Arc,
};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader, Lines, Stdin};

type InputLines = Lines<BufReader<Stdin>>;

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.as_slice() == ["--version"]
        || args.as_slice() == ["-V"]
        || args.as_slice() == ["--build-info"]
    {
        println!(
            "kroma {} ({})",
            env!("CARGO_PKG_VERSION"),
            env!("KROMA_BUILD_REVISION")
        );
        if args.as_slice() == ["--build-info"] {
            println!("executable: {}", std::env::current_exe()?.display());
        }
        return Ok(());
    }
    if args
        .first()
        .is_some_and(|arg| arg == "--help" || arg == "-h")
    {
        print_help();
        return Ok(());
    }
    if args.first().is_some_and(|arg| arg == "config") {
        return profiles::cli(&args[1..]);
    }
    if args.first().is_some_and(|arg| arg == "plugins") {
        return kroma::plugins::PluginCatalog::cli(&args[1..]);
    }
    if args.first().is_some_and(|arg| arg == "trust-project") {
        let revoke = args.iter().any(|arg| arg == "--revoke");
        if args.len() > 2 || (args.len() == 2 && args[1] != "--revoke") {
            bail!("usage: kroma trust-project [--revoke]");
        }
        let dir = env::var("KROMA_WORKDIR")
            .map(PathBuf::from)
            .or_else(|_| env::current_dir())?
            .canonicalize()?;
        if revoke {
            kroma::mcp::revoke_project_mcp(&dir)?;
            println!(
                "Revoked project MCP trust for {}. The project's .kroma/mcp.json will not load until approved again.",
                dir.display()
            );
        } else {
            let config = kroma::mcp::trust_project_mcp(&dir)?;
            println!(
                "Trusted project MCP config {}. Re-run this command after editing the file.",
                config.display()
            );
        }
        return Ok(());
    }
    if args.first().is_some_and(|arg| arg == "recover-context") {
        if args.len() > 2 {
            bail!("usage: kroma recover-context [session-id]");
        }
        let dir = env::var("KROMA_WORKDIR")
            .map(PathBuf::from)
            .or_else(|_| env::current_dir())?
            .canonicalize()?;
        let session = args.get(1).map_or("default", String::as_str);
        if Agent::recover_builtin_context(&dir, session)? {
            println!("Recovered session '{session}' using built-in context; complete original history preserved.");
        } else {
            println!("Session '{session}' already uses built-in context; no change.");
        }
        return Ok(());
    }
    let config = Config::from_env()?;
    if args.as_slice() == ["--continue"] {
        if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
            bail!("kroma --continue requires an interactive terminal; use kroma --cli and /continue otherwise");
        }
        return kroma::tui::run_with_resume(config, true).await;
    }
    let mut model_name = config.model.clone();
    let json_mode = args.iter().any(|arg| arg == "--json");
    let print_mode = json_mode || args.iter().any(|arg| arg == "--print");
    if print_mode {
        let task = args
            .iter()
            .filter(|arg| !matches!(arg.as_str(), "--json" | "--print"))
            .cloned()
            .collect::<Vec<_>>()
            .join(" ");
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
        let runtime = if json_mode {
            app::default_runtime_with(config, |components| {
                let sink: Arc<dyn kroma::events::EventSink> =
                    Arc::new(kroma::cli_events::JsonEvents);
                components.events = Some(sink);
                Ok(())
            })
            .await?
        } else {
            app::default_runtime(config).await?
        };
        for warning in runtime
            .startup_warnings
            .iter()
            .chain(runtime.plugin_warnings.iter())
        {
            eprintln!("[startup] {warning}");
        }
        let mut agent = runtime.agent;
        agent.set_background_compaction(false);
        let prompt = kroma::convenience::file_reference(&prompt, agent.workdir())?;
        if json_mode {
            let result = drive_task(&mut agent, &prompt, None, &mut |_| {}).await;
            match result {
                Ok(Some(answer)) => {
                    println!("{}", serde_json::json!({"type":"result","text":answer}));
                }
                Ok(None) => bail!("task cancelled"),
                Err(error) => {
                    println!(
                        "{}",
                        serde_json::json!({"type":"error","message":format!("{error:#}")})
                    );
                    bail!("task failed");
                }
            }
        } else if !run_task(&mut agent, &prompt, None).await? {
            bail!("task cancelled");
        }
        return Ok(());
    }
    let force_cli = args.as_slice() == ["--no-tui"] || args.as_slice() == ["--cli"];
    if args.is_empty() && io::stdin().is_terminal() && io::stdout().is_terminal() {
        return kroma::tui::run(config).await;
    }
    let runtime = app::default_runtime(config).await?;
    if let Some(name) = &runtime.selected_model_name {
        model_name = name.clone();
    }
    for warning in &runtime.startup_warnings {
        eprintln!("[startup] {warning}");
    }
    let mut commands =
        CommandRegistry::builtin_with_warnings(runtime.skills.list(), runtime.startup_warnings)?;
    for warning in runtime
        .plugin_warnings
        .iter()
        .chain(runtime.plugins.register_commands(&mut commands).iter())
    {
        eprintln!("[plugin] {warning}");
    }
    let mut agent = runtime.agent;

    if !args.is_empty() && !force_cli {
        agent.set_background_compaction(false);
        if !run_task(&mut agent, &args.join(" "), None).await? {
            bail!("task cancelled");
        }
        return Ok(());
    }
    if !io::stdin().is_terminal() {
        // A piped CLI session has no future interactive turn to collect a
        // speculative background summary; hard-limit compaction still runs.
        agent.set_background_compaction(false);
    }

    println!(
        "Kroma v0.4 | model: {} | session: {} | TRC + APEC | Ctrl+C cancels | /history | /fork | /rewind | /undo | /new | /resume | /sessions | /skills | /tools | /queue | /exit",
        model_name,
        agent.session_id()
    );
    let stdin = BufReader::new(tokio::io::stdin());
    let mut lines = stdin.lines();
    let input_handle = agent.task_input();
    let mut run_followups = false;
    loop {
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
            print!("kroma> ");
            io::stdout().flush()?;
            tokio::select! {
                input = lines.next_line() => match input? {
                    Some(text) => text,
                    None => break,
                },
                signal = tokio::signal::ctrl_c() => {
                    signal?;
                    println!();
                    continue;
                },
            }
        };
        let input = input.trim();
        // Queued items are always user tasks, not CLI commands. A user may
        // deliberately queue a task whose text starts with a slash.
        if from_queue {
            match run_task(&mut agent, input, Some(&mut lines)).await {
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
            };
            match outcome {
                Some(Ok(output)) => println!("{}", serde_json::to_string_pretty(&output)?),
                Some(Err(error)) => eprintln!("Shell: {error:#}"),
                None => eprintln!("[shell] Cancelled; completed side effects are not undone."),
            }
            continue;
        }
        let command_result = {
            let command_future = commands.execute_async(input, &mut agent, &input_handle);
            tokio::pin!(command_future);
            tokio::select! {
                result = &mut command_future => result,
                signal = tokio::signal::ctrl_c() => {
                    signal?;
                    eprintln!("[plugin] Command cancelled");
                    continue;
                }
            }
        };
        match command_result {
            Ok(Some(reply)) => {
                if !reply.output.is_empty() {
                    println!("{}", reply.output);
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
        match run_task(&mut agent, &input, Some(&mut lines)).await {
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
async fn run_task(agent: &mut Agent, input: &str, lines: Option<&mut InputLines>) -> Result<bool> {
    let mut saw_text = false;
    let mut ends_with_newline = true;
    let result = drive_task(agent, input, lines, &mut |text| {
        saw_text = true;
        ends_with_newline = text.ends_with('\n');
        print!("{text}");
        let _ = io::stdout().flush();
    })
    .await;
    if saw_text && !ends_with_newline {
        println!();
    }
    match result? {
        Some(answer) => {
            if !saw_text {
                println!("{answer}");
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
            tokio::select! {
                biased;
                signal = tokio::signal::ctrl_c() => {
                    signal?;
                    break None;
                }
                next = async {
                    match lines.as_mut() {
                        Some(lines) => lines.next_line().await,
                        None => Ok(None),
                    }
                }, if lines.is_some() && !stdin_closed => match next? {
                    Some(text) => handle_running_input(&input_handle, text.trim()),
                    None => stdin_closed = true,
                },
                result = &mut task => break Some(result),
            }
        }
    };
    match result {
        Some(result) => result.map(Some),
        None => {
            agent.cancel()?;
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
    ) || text.starts_with("/resume ")
        || text.starts_with("/history ")
        || text.starts_with("/fork ")
        || text.starts_with("/rewind ")
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

fn print_help() {
    println!("Interactive TTY: default Ratatui TUI. --no-tui (or --cli): original line CLI. F2: switch theme; /theme reload: reread ~/.kroma/theme.toml.");
    println!(
        "Kroma v0.4\n\nUsage:\n  kroma [task text]\n  kroma              Interactive session\n  kroma config set <name>  Save API settings (key is entered privately)\n  kroma plugins list       List installed plugins (no model connection needed)\n  kroma plugins install <dir>  Install local plugin directory (disabled)\n  kroma plugins enable <id>   Enable for next startup\n  kroma plugins disable <id>  Disable for next startup\n  kroma plugins set <id> <key> <value>  Set plugin configuration\n  kroma plugins config <id>  Show plugin settings\n  kroma config list        List saved profiles\n  kroma config use <name>  Switch the default profile\n  kroma config show        Show effective settings without secrets\n  kroma recover-context [session-id]  Reset unavailable custom context memory while preserving raw history
  kroma trust-project [--revoke]  Approve or revoke this project's .kroma/mcp.json\n\nControls:\n  Ctrl+C             Cancel active model request or shell task\n  /new               Start a separate saved conversation\n  /sessions          List saved conversations for this workdir\n  /resume <id>       Switch to a saved conversation (or default)\n  /clear             Clear only the active conversation\n  /exit              Quit\n\nOptional environment:\n  KROMA_MODEL                  Model ID (required without a saved profile)\n  KROMA_BASE_URL               OpenAI-compatible API base\n  KROMA_API_KEY                API key, overrides the selected profile\n  KROMA_PROVIDER_PROFILE       Select a saved profile by name\n  KROMA_CONFIG_FILE            Use an alternate JSON profiles path\n  KROMA_WORKDIR                Shell working directory (default current directory)\n  KROMA_PLUGIN_DIR             Override user plugin directory\n  KROMA_SESSION_FILE           Override default session JSON path (named sessions share its directory)\n  KROMA_DEEPSEEK_THINKING      enabled, high or max; opt-in for DeepSeek-compatible endpoints\n  KROMA_PROFILE                Set to 1 for extra task profiling\n  KROMA_CONTEXT_WINDOW         Model context size in tokens (default 128000)\n  KROMA_OUTPUT_RESERVE_TOKENS  Tokens reserved for response (default min(4096, context/8))
  KROMA_TRUST_PROJECT_MCP      1 approves this project's .kroma/mcp.json for one run
  KROMA_WEB_ALLOW_PRIVATE      1 lets the web plugin fetch loopback/private addresses"
    );
    println!("\nAdditional model settings:\n  KROMA_API_PROTOCOL           chat (default), responses or anthropic\n  KROMA_REASONING_EFFORT       auto (default), off, low, medium, high, xhigh or max\n  KROMA_MAX_OUTPUT_TOKENS      Override requested output maximum; upstream cap takes priority\n  KROMA_CONTEXT_WINDOW         Override upstream model context capacity\n\nKROMA_BASE_URL is the provider's API base URL, such as https://api.anthropic.com/v1.");
    println!("\nInstruction files:\n  ~/.kroma/AGENTS.md           Global instructions for all projects\n  <workdir>/AGENTS.md          Instructions for the current project\n  Files reload at each new task; the original session transcript is unchanged.\n\nWhile a task runs:\n  type text or /steer <text>    Steer the active task at the next model turn\n  /queue <task>                Schedule a separate task after the current task\n  /queue list | /pending       Inspect pending tasks\n  /queue clear                Remove pending follow-ups\n  /queue run                  Resume a paused follow-up queue\n  Ctrl+C                      Cancel current task and pause queued tasks");
    println!("\nMCP: ~/.kroma/mcp.json and <workdir>/.kroma/mcp.json (or KROMA_MCP_CONFIG).\nProject MCP configs are disabled until approved with 'kroma trust-project' (or KROMA_TRUST_PROJECT_MCP=1 for one run).\nSkills: ~/.agents/skills, ~/.kroma/skills, <workdir>/.agents/skills, <workdir>/.kroma/skills.\nInteractive: /tools lists Shell, MCP and plugin tools; /skills lists discovered skill metadata; /plugins manages plugins.");
    println!("\nHistory: /history [count] lists completed checkpoints; /fork [number] starts a separate session; /rewind <number> replaces the current session after preserving its old future; /undo rewinds one turn. /resume <session-id> restores a saved future. These commands never revert files or git commits.");
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
