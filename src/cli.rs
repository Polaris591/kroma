//! Strict command-line parsing. Flags are never treated as prompt text.
//! Global options (`--cwd`, `--config-file`, `--language`, `--mcp`, `--no-mcp`,
//! `--quiet`, `--verbose`, `--no-color`) are accepted before or after any
//! command, and the command itself is the first non-global argument. `--` ends
//! flag parsing, so later dashes stay prompt text.
use std::{ffi::OsString, path::PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Run(RunOptions),
    Setup,
    Update { check: bool, channel: String },
    Config(Vec<String>),
    Plugins(Vec<String>),
    Sessions(Vec<String>),
    Diagnostics(Option<PathBuf>),
    TrustProject { revoke: bool },
    RecoverContext(Option<String>),
    Help(Option<String>),
    Version { build_info: bool },
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunOptions {
    pub prompt: Vec<String>,
    pub print: bool,
    pub json: bool,
    pub continue_latest: bool,
    pub session: Option<String>,
    pub new_session: bool,
    pub no_tui: bool,
    pub no_mcp: bool,
    pub mcp: Vec<String>,
    pub quiet: bool,
    pub verbose: bool,
    pub cwd: Option<PathBuf>,
    pub config_file: Option<PathBuf>,
    pub language: Option<String>,
    pub no_color: bool,
}

impl RunOptions {
    pub fn task(&self) -> String {
        self.prompt.join(" ")
    }

    pub fn one_shot(&self) -> bool {
        self.print || self.json
    }

    /// Global options parsed anywhere on the line still populate the run
    /// options, so `Command::Run` keeps every field it had when the host
    /// parsed globals a second time.
    fn inherit_globals(&mut self, globals: &GlobalOptions) {
        if globals.cwd.is_some() {
            self.cwd.clone_from(&globals.cwd);
        }
        if globals.config_file.is_some() {
            self.config_file.clone_from(&globals.config_file);
        }
        if globals.language.is_some() {
            self.language.clone_from(&globals.language);
        }
        self.mcp.extend(globals.mcp.iter().cloned());
        self.no_mcp |= globals.no_mcp;
        self.quiet |= globals.quiet;
        self.verbose |= globals.verbose;
        self.no_color |= globals.no_color;
    }
}

/// Options that every command honours. They are parsed once for the whole
/// command line and become environment variables through
/// [`GlobalOptions::environment`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GlobalOptions {
    pub cwd: Option<PathBuf>,
    pub config_file: Option<PathBuf>,
    pub language: Option<String>,
    pub mcp: Vec<String>,
    pub no_mcp: bool,
    pub quiet: bool,
    pub verbose: bool,
    pub no_color: bool,
}

impl GlobalOptions {
    /// Environment overrides these options imply. The host applies them once,
    /// before dispatch, so every command and profile loader sees the same
    /// selection.
    pub fn environment(&self) -> Vec<(&'static str, OsString)> {
        let mut environment = Vec::new();
        if let Some(cwd) = &self.cwd {
            environment.push(("KROMA_WORKDIR", cwd.clone().into_os_string()));
        }
        if let Some(path) = &self.config_file {
            environment.push(("KROMA_CONFIG_FILE", path.clone().into_os_string()));
        }
        if let Some(language) = &self.language {
            environment.push(("KROMA_LANG", OsString::from(language.as_str())));
        }
        if !self.mcp.is_empty() {
            environment.push(("KROMA_MCP_SERVERS", OsString::from(self.mcp.join(","))));
        }
        if self.no_mcp {
            environment.push(("KROMA_NO_MCP", OsString::from("1")));
        }
        if self.quiet {
            environment.push(("KROMA_QUIET", OsString::from("1")));
        }
        if self.verbose {
            environment.push(("KROMA_VERBOSE", OsString::from("1")));
        }
        if self.no_color {
            environment.push(("NO_COLOR", OsString::from("1")));
        }
        environment
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliError {
    pub message: String,
    pub code: u8,
}

impl CliError {
    fn usage(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            code: 2,
        }
    }
}

pub fn parse(args: &[String]) -> Result<Command, CliError> {
    parse_with_globals(args).map(|(_, command)| command)
}

/// Parse a whole command line. Global options are returned separately so the
/// host can apply their environment overrides once for every command.
pub fn parse_with_globals(args: &[String]) -> Result<(GlobalOptions, Command), CliError> {
    let mut globals = GlobalOptions::default();
    let first = skip_globals(args, &mut globals)?;
    match args.get(first).map(String::as_str) {
        None => {
            let mut options = RunOptions::default();
            options.inherit_globals(&globals);
            Ok((globals, Command::Run(options)))
        }
        Some(head) if is_subcommand(head) => {
            let tail = strip_globals(&args[first + 1..], &mut globals)?;
            Ok((globals, subcommand(head, &tail)?))
        }
        // Everything else is a run command: one pass over the whole line keeps
        // option values strict and preserves prompt text after `--`.
        Some(_) => parse_run(&args[first..], globals),
    }
}

fn is_subcommand(arg: &str) -> bool {
    matches!(
        arg,
        "update"
            | "config"
            | "plugins"
            | "sessions"
            | "diagnostics"
            | "trust-project"
            | "recover-context"
            | "setup"
            | "help"
            | "version"
            | "-h"
            | "--help"
            | "-V"
            | "--version"
            | "--build-info"
    )
}

fn subcommand(head: &str, args: &[String]) -> Result<Command, CliError> {
    match head {
        "update" => parse_update(args),
        "config" => Ok(Command::Config(args.to_vec())),
        "plugins" => Ok(Command::Plugins(args.to_vec())),
        "sessions" => Ok(Command::Sessions(args.to_vec())),
        "diagnostics" => parse_diagnostics(args),
        "trust-project" => parse_trust(args),
        "recover-context" => parse_recover(args),
        "setup" => match args {
            [] => Ok(Command::Setup),
            [only] if only == "-h" || only == "--help" => Ok(Command::Help(Some("setup".into()))),
            _ => Err(CliError::usage("Usage: kroma setup")),
        },
        "help" | "-h" | "--help" => Ok(Command::Help(args.first().cloned())),
        "version" | "-V" | "--version" => Ok(Command::Version { build_info: false }),
        "--build-info" => Ok(Command::Version { build_info: true }),
        other => Err(CliError::usage(format!("unknown command '{other}'"))),
    }
}

fn parse_update(args: &[String]) -> Result<Command, CliError> {
    let mut check = false;
    let mut channel = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "-h" | "--help" => return Ok(Command::Help(Some("update".into()))),
            "--check" if !check => check = true,
            "--channel" if channel.is_none() => {
                index += 1;
                channel = args.get(index).cloned();
                if channel.is_none() {
                    return Err(CliError::usage(help_for("update")));
                }
            }
            value if value.starts_with("--channel=") && channel.is_none() => {
                channel = Some(value[10..].to_owned());
            }
            _ => return Err(CliError::usage(help_for("update"))),
        }
        index += 1;
    }
    let channel = channel.unwrap_or_else(|| "latest".into());
    if !matches!(channel.as_str(), "latest" | "next") {
        return Err(CliError::usage("update channel must be latest or next"));
    }
    Ok(Command::Update { check, channel })
}

fn parse_diagnostics(args: &[String]) -> Result<Command, CliError> {
    let mut output = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "-h" | "--help" => return Ok(Command::Help(Some("diagnostics".into()))),
            "--output" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| CliError::usage("kroma diagnostics --output needs a directory"))?;
                if output.is_some() {
                    return Err(CliError::usage(
                        "kroma diagnostics --output may be specified once",
                    ));
                }
                if value.starts_with('-') && value != "-" {
                    return Err(CliError::usage(format!(
                        "kroma diagnostics --output needs a directory, got option '{value}'"
                    )));
                }
                output = Some(PathBuf::from(value));
                index += 2;
            }
            other => {
                return Err(CliError::usage(format!(
                    "unexpected diagnostics argument '{other}'\nUsage: kroma diagnostics [--output <new-directory>]"
                )))
            }
        }
    }
    Ok(Command::Diagnostics(output))
}

fn parse_trust(args: &[String]) -> Result<Command, CliError> {
    let mut revoke = false;
    for arg in args {
        match arg.as_str() {
            "--revoke" if !revoke => revoke = true,
            "-h" | "--help" => return Ok(Command::Help(Some("trust-project".into()))),
            other => {
                return Err(CliError::usage(format!(
                    "unexpected trust-project argument '{other}'\nUsage: kroma trust-project [--revoke]"
                )))
            }
        }
    }
    Ok(Command::TrustProject { revoke })
}

fn parse_recover(args: &[String]) -> Result<Command, CliError> {
    match args {
        [] => Ok(Command::RecoverContext(None)),
        [one] if one == "-h" || one == "--help" => {
            Ok(Command::Help(Some("recover-context".into())))
        }
        [one] if !one.starts_with('-') => Ok(Command::RecoverContext(Some(one.clone()))),
        [one] => Err(CliError::usage(format!(
            "unexpected recover-context argument '{one}'\nUsage: kroma recover-context [session-id]"
        ))),
        _ => Err(CliError::usage("Usage: kroma recover-context [session-id]")),
    }
}

fn parse_run(
    args: &[String],
    mut globals: GlobalOptions,
) -> Result<(GlobalOptions, Command), CliError> {
    let mut options = RunOptions::default();
    let mut literal = false;
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if literal {
            options.prompt.push(arg.clone());
            index += 1;
            continue;
        }
        if arg == "--" {
            literal = true;
            index += 1;
            continue;
        }
        // Globals share this pass, so `--session --quiet task` fails on the
        // token meant as the session name instead of silently making the task
        // a session name.
        if take_global(args, &mut index, &mut globals)? {
            index += 1;
            continue;
        }
        match arg.as_str() {
            "-h" | "--help" => return Ok((globals, Command::Help(Some("run".into())))),
            "-V" | "--version" => {
                return Ok((globals, Command::Version { build_info: false }));
            }
            "--build-info" => {
                return Ok((globals, Command::Version { build_info: true }));
            }
            "-p" | "--print" => options.print = true,
            "--json" => {
                options.json = true;
                options.print = true;
            }
            "-c" | "--continue" => options.continue_latest = true,
            "--new" => options.new_session = true,
            "--session" => {
                options.session = Some(next_value(args, &mut index, "--session")?);
            }
            "--no-tui" | "--cli" => options.no_tui = true,
            other if other.starts_with('-') && other != "-" => {
                return Err(CliError::usage(format!(
                    "unknown option '{other}'\nRun 'kroma --help' for usage."
                )));
            }
            _ => options.prompt.push(arg.clone()),
        }
        index += 1;
    }
    if options.new_session && options.continue_latest {
        return Err(CliError::usage("--new cannot be combined with --continue"));
    }
    if options.new_session && options.session.is_some() {
        return Err(CliError::usage("--new cannot be combined with --session"));
    }
    if options.continue_latest && options.session.is_some() {
        return Err(CliError::usage(
            "--continue cannot be combined with --session",
        ));
    }
    options.inherit_globals(&globals);
    Ok((globals, Command::Run(options)))
}

#[derive(Clone, Copy)]
enum GlobalOption {
    Cwd,
    ConfigFile,
    Language,
    Mcp,
    NoMcp,
    Quiet,
    Verbose,
    NoColor,
}

impl GlobalOption {
    fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "--cwd" => Self::Cwd,
            "--config-file" => Self::ConfigFile,
            "--language" => Self::Language,
            "--mcp" => Self::Mcp,
            "--no-mcp" => Self::NoMcp,
            "-q" | "--quiet" => Self::Quiet,
            "-v" | "--verbose" => Self::Verbose,
            "--no-color" => Self::NoColor,
            _ => return None,
        })
    }
}

/// Only long options accept the inline `--name=value` form.
fn option_parts(arg: &str) -> (&str, Option<&str>) {
    match arg.split_once('=') {
        Some((name, value)) if name.starts_with("--") => (name, Some(value)),
        _ => (arg, None),
    }
}

/// Consume a global option at `args[*index]`. Returns false, without touching
/// `index`, when the token is not a global option.
fn take_global(
    args: &[String],
    index: &mut usize,
    globals: &mut GlobalOptions,
) -> Result<bool, CliError> {
    let (name, inline) = option_parts(&args[*index]);
    let Some(option) = GlobalOption::from_name(name) else {
        return Ok(false);
    };
    match option {
        GlobalOption::Quiet => set_flag(&mut globals.quiet, name, inline)?,
        GlobalOption::Verbose => set_flag(&mut globals.verbose, name, inline)?,
        GlobalOption::NoMcp => set_flag(&mut globals.no_mcp, name, inline)?,
        GlobalOption::NoColor => set_flag(&mut globals.no_color, name, inline)?,
        GlobalOption::Cwd => {
            globals.cwd = Some(PathBuf::from(global_value(args, index, name, inline)?));
        }
        GlobalOption::ConfigFile => {
            globals.config_file = Some(PathBuf::from(global_value(args, index, name, inline)?));
        }
        GlobalOption::Language => {
            let value = global_value(args, index, name, inline)?;
            if !matches!(value.as_str(), "en" | "zh") {
                return Err(CliError::usage("--language must be en or zh"));
            }
            globals.language = Some(value);
        }
        GlobalOption::Mcp => {
            let value = global_value(args, index, name, inline)?;
            let names = value
                .split(',')
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(str::to_owned)
                .collect::<Vec<_>>();
            if names.is_empty() {
                return Err(CliError::usage("--mcp needs at least one server name"));
            }
            globals.mcp.extend(names);
        }
    }
    Ok(true)
}

fn set_flag(target: &mut bool, name: &str, inline: Option<&str>) -> Result<(), CliError> {
    if let Some(value) = inline {
        return Err(CliError::usage(format!(
            "{name} does not take a value, got '{value}'"
        )));
    }
    *target = true;
    Ok(())
}

fn global_value(
    args: &[String],
    index: &mut usize,
    name: &str,
    inline: Option<&str>,
) -> Result<String, CliError> {
    let value = match inline {
        Some(value) => value.to_owned(),
        None => next_value(args, index, name)?,
    };
    if value.is_empty() || (value.starts_with('-') && value != "-") {
        return Err(CliError::usage(format!(
            "{name} needs a value, got '{value}'"
        )));
    }
    Ok(value)
}

fn next_value(args: &[String], index: &mut usize, flag: &str) -> Result<String, CliError> {
    *index += 1;
    let value = args
        .get(*index)
        .cloned()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| CliError::usage(format!("{flag} needs a value")))?;
    if value.starts_with('-') && value != "-" {
        return Err(CliError::usage(format!(
            "{flag} needs a value, got option '{value}'"
        )));
    }
    Ok(value)
}

/// Index of the first token that is not a global option. `--` counts as a
/// token, so a leading literal separator still starts a run command.
fn skip_globals(args: &[String], globals: &mut GlobalOptions) -> Result<usize, CliError> {
    let mut index = 0;
    while index < args.len() && take_global(args, &mut index, globals)? {
        index += 1;
    }
    Ok(index)
}

/// Remove global options from a subcommand's arguments, keeping everything
/// after `--` verbatim.
fn strip_globals(args: &[String], globals: &mut GlobalOptions) -> Result<Vec<String>, CliError> {
    let mut kept = Vec::with_capacity(args.len());
    let mut index = 0;
    while index < args.len() {
        if args[index] == "--" {
            kept.extend(args[index..].iter().cloned());
            break;
        }
        if take_global(args, &mut index, globals)? {
            index += 1;
            continue;
        }
        kept.push(args[index].clone());
        index += 1;
    }
    Ok(kept)
}

pub fn usage() -> &'static str {
    "Kroma - a minimal Rust AI agent harness\n\n\
Usage:\n  kroma                         Interactive TUI (new conversation)\n  kroma [TASK]                  Run one task in the line CLI\n  kroma --print TASK            Stream a one-shot answer\n  kroma --json TASK             Emit one JSON object per line\n  kroma --continue              Resume the latest saved conversation\n  kroma --session NAME TASK     Use a named saved conversation\n  kroma setup                   Configure a model interactively\n  kroma config ...              Manage saved profiles\n  kroma plugins ...             Manage process plugins\n  kroma diagnostics ...         Write a local diagnostics bundle\n  kroma trust-project           Approve this project's MCP config\n  kroma recover-context         Reset custom context memory\n  kroma help [COMMAND]          Show help\n  kroma version                 Show version\n\n\
Options:\n  -p, --print             One-shot plain text\n      --json              JSONL events (implies --print)\n  -c, --continue          Resume the most recent conversation\n      --session <name>    Use or create a named conversation\n      --new               Force a fresh conversation\n      --no-mcp            Skip MCP servers for this run\n      --mcp <names>       Enable only named MCP servers (comma separated)\n  -q, --quiet             Suppress optional runtime diagnostics\n  -v, --verbose           Print extra runtime diagnostics\n      --cwd <path>        Project directory\n      --config-file <p>   Alternate profile JSON path\n      --language <en|zh>  TUI language\n      --no-color          Disable colour\n      --no-tui, --cli     Use the line CLI for interactive input\n  -h, --help              Show help\n  -V, --version           Show version\n      --build-info        Version plus executable path\n\n\
Global options accept --name=value and may appear before or after any command.\n\
Use '--' before prompt text that starts with a dash: kroma -- --bogus\n\
Docs: docs/quickstart.md - docs/security.md"
}

pub fn help_for(topic: &str) -> &'static str {
    match topic {
        "config" => "Usage: kroma config [list|show|set <name>|use <name>|delete <name>|test <name>]\n\nProfiles live in ~/.kroma/config.json (or KROMA_CONFIG_FILE).\nRun 'kroma setup' for a guided first-run flow.",
        "sessions" => "Usage: kroma sessions prune --older-than DAYS

Removes saved conversations whose newest file is older than DAYS (minimum 1).
A conversation used recently always survives any cutoff; nothing else in the directory is touched.",
        "plugins" => "Usage: kroma plugins [list|enable <id>|disable <id>|install <path>|check <path>|uninstall <id>|config <id>|set <id> <key> <value>|reset <id> <key>]\n\nInstalled plugins are disabled until enabled; restart or /plugins reload after enabling.\nPlugins with a [settings] table reject unknown keys; 'set' converts the value to the type of that key's manifest default (bool, integer, string array, string).",
        "diagnostics" => "Usage: kroma diagnostics [--output <new-directory>]\n\nWrites a local-only allowlisted bundle. No upload, model request or plugin process.",
        "trust-project" => "Usage: kroma trust-project [--revoke]\n\nApproves or revokes this project's .kroma/mcp.json. The approval is bound to its content.",
        "recover-context" => "Usage: kroma recover-context [session-id]\n\nResets unavailable custom context memory while preserving raw history.",
        "setup" => "Usage: kroma setup\n\nGuided model setup: provider, model, key, optional connection test, then save.",
        "update" => "Usage: kroma update [--check] [--channel latest|next]

Explicit npm version check/update. npm installations update through npm; Cargo/source binaries only give manual instructions. No startup network requests.",
        "run" => usage(),
        _ => usage(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(value: &[&str]) -> Vec<String> {
        value.iter().map(|v| (*v).to_owned()).collect()
    }

    #[test]
    fn flags_are_never_prompt_text() {
        let Command::Run(options) = parse(&args(&["--print", "hello"])).unwrap() else {
            panic!("expected run");
        };
        assert_eq!(options.task(), "hello");
        assert!(options.print);
        assert!(!options.prompt.contains(&"--print".to_owned()));
    }

    #[test]
    fn unknown_flags_are_usage_errors() {
        let error = parse(&args(&["--bogus"])).unwrap_err();
        assert_eq!(error.code, 2);
        assert!(error.message.contains("unknown option"));
    }

    #[test]
    fn help_and_version_do_not_require_a_prompt() {
        assert_eq!(parse(&args(&["help"])).unwrap(), Command::Help(None));
        assert_eq!(
            parse(&args(&["version"])).unwrap(),
            Command::Version { build_info: false }
        );
        assert_eq!(
            parse(&args(&["--version"])).unwrap(),
            Command::Version { build_info: false }
        );
    }

    #[test]
    fn options_can_follow_the_prompt() {
        let Command::Run(options) = parse(&args(&["hello", "--json"])).unwrap() else {
            panic!("expected run");
        };
        assert_eq!(options.task(), "hello");
        assert!(options.json);
        assert!(options.print);
    }

    #[test]
    fn option_values_cannot_be_another_option() {
        assert!(parse(&args(&["--session", "--print"])).is_err());
        assert!(parse(&args(&["--mcp", "--print"])).is_err());
        assert!(parse(&args(&["--config-file", "--print"])).is_err());
        assert!(parse(&args(&["--mcp", ","])).is_err());
    }

    #[test]
    fn double_dash_allows_literal_dash_text() {
        let Command::Run(options) = parse(&args(&["--", "--bogus"])).unwrap() else {
            panic!("expected run");
        };
        assert_eq!(options.task(), "--bogus");
    }

    #[test]
    fn conflicting_session_flags_are_rejected() {
        assert!(parse(&args(&["--new", "--continue"])).is_err());
        assert!(parse(&args(&["--session", "x", "--continue"])).is_err());
    }

    #[test]
    fn combined_cli_and_print_do_not_leak_the_flag() {
        let Command::Run(options) = parse(&args(&["--cli", "--print", "hello"])).unwrap() else {
            panic!("expected run");
        };
        assert_eq!(options.task(), "hello");
        assert!(options.no_tui);
        assert!(options.print);
    }
}
