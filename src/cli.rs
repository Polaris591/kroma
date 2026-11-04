//! Strict command-line parsing. Flags are never treated as prompt text.
//! Subcommands are recognized only as the first argument; `--` ends flags.
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Run(RunOptions),
    Setup,
    Config(Vec<String>),
    Plugins(Vec<String>),
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
    let Some(first) = args.first().map(String::as_str) else {
        return Ok(Command::Run(RunOptions::default()));
    };
    match first {
        "config" => Ok(Command::Config(args[1..].to_vec())),
        "plugins" => Ok(Command::Plugins(args[1..].to_vec())),
        "diagnostics" => parse_diagnostics(&args[1..]),
        "trust-project" => parse_trust(&args[1..]),
        "recover-context" => parse_recover(&args[1..]),
        "setup" => {
            if args.len() > 1 && args[1] != "--help" && args[1] != "-h" {
                Err(CliError::usage("Usage: kroma setup"))
            } else if args.len() > 1 {
                Ok(Command::Help(Some("setup".into())))
            } else {
                Ok(Command::Setup)
            }
        }
        "help" => Ok(Command::Help(args.get(1).cloned())),
        "version" => Ok(Command::Version { build_info: false }),
        "-h" | "--help" => Ok(Command::Help(args.get(1).cloned())),
        "-V" | "--version" => Ok(Command::Version { build_info: false }),
        "--build-info" => Ok(Command::Version { build_info: true }),
        _ => parse_run(args),
    }
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

fn parse_run(args: &[String]) -> Result<Command, CliError> {
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
        match arg.as_str() {
            "--" => literal = true,
            "-h" | "--help" => return Ok(Command::Help(Some("run".into()))),
            "-V" | "--version" => {
                return Ok(Command::Version { build_info: false });
            }
            "--build-info" => {
                return Ok(Command::Version { build_info: true });
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
            "--no-mcp" => options.no_mcp = true,
            "--mcp" => {
                let value = next_value(args, &mut index, "--mcp")?;
                let names = value
                    .split(',')
                    .map(str::trim)
                    .filter(|v| !v.is_empty())
                    .collect::<Vec<_>>();
                if names.is_empty() {
                    return Err(CliError::usage("--mcp needs at least one server name"));
                }
                options.mcp.extend(names.into_iter().map(str::to_owned));
            }
            "-q" | "--quiet" => options.quiet = true,
            "-v" | "--verbose" => options.verbose = true,
            "--cwd" => {
                options.cwd = Some(PathBuf::from(next_value(args, &mut index, "--cwd")?));
            }
            "--config-file" => {
                options.config_file = Some(PathBuf::from(next_value(
                    args,
                    &mut index,
                    "--config-file",
                )?));
            }
            "--language" => {
                let value = next_value(args, &mut index, "--language")?;
                if !matches!(value.as_str(), "en" | "zh") {
                    return Err(CliError::usage("--language must be en or zh"));
                }
                options.language = Some(value);
            }
            "--no-color" => options.no_color = true,
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
    Ok(Command::Run(options))
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

pub fn usage() -> &'static str {
    "Kroma - a minimal Rust AI agent harness\n\n\
Usage:\n  kroma                         Interactive TUI (new conversation)\n  kroma [TASK]                  Run one task in the line CLI\n  kroma --print TASK            Stream a one-shot answer\n  kroma --json TASK             Emit one JSON object per line\n  kroma --continue              Resume the latest saved conversation\n  kroma --session NAME TASK     Use a named saved conversation\n  kroma setup                   Configure a model interactively\n  kroma config ...              Manage saved profiles\n  kroma plugins ...             Manage process plugins\n  kroma diagnostics ...         Write a local diagnostics bundle\n  kroma trust-project           Approve this project's MCP config\n  kroma recover-context         Reset custom context memory\n  kroma help [COMMAND]          Show help\n  kroma version                 Show version\n\n\
Options:\n  -p, --print             One-shot plain text\n      --json              JSONL events (implies --print)\n  -c, --continue          Resume the most recent conversation\n      --session <name>    Use or create a named conversation\n      --new               Force a fresh conversation\n      --no-mcp            Skip MCP servers for this run\n      --mcp <names>       Enable only named MCP servers (comma separated)\n  -q, --quiet             Suppress optional runtime diagnostics\n  -v, --verbose           Print extra runtime diagnostics\n      --cwd <path>        Project directory\n      --config-file <p>   Alternate profile JSON path\n      --language <en|zh>  TUI language\n      --no-color          Disable colour\n      --no-tui, --cli     Use the line CLI for interactive input\n  -h, --help              Show help\n  -V, --version           Show version\n      --build-info        Version plus executable path\n\n\
Use '--' before prompt text that starts with a dash: kroma -- --bogus\n\
Docs: docs/quickstart.md - docs/security.md"
}

pub fn help_for(topic: &str) -> &'static str {
    match topic {
        "config" => "Usage: kroma config [list|show|set <name>|use <name>|delete <name>|test <name>]\n\nProfiles live in ~/.kroma/config.json (or KROMA_CONFIG_FILE).\nRun 'kroma setup' for a guided first-run flow.",
        "plugins" => "Usage: kroma plugins [list|enable <id>|disable <id>|install <path>|check <path>|uninstall <id>|config <id>|set <id> <key> <value>|reset <id> <key>]\n\nInstalled plugins are disabled until enabled; restart or /plugins reload after enabling.",
        "diagnostics" => "Usage: kroma diagnostics [--output <new-directory>]\n\nWrites a local-only allowlisted bundle. No upload, model request or plugin process.",
        "trust-project" => "Usage: kroma trust-project [--revoke]\n\nApproves or revokes this project's .kroma/mcp.json. The approval is bound to its content.",
        "recover-context" => "Usage: kroma recover-context [session-id]\n\nResets unavailable custom context memory while preserving raw history.",
        "setup" => "Usage: kroma setup\n\nGuided model setup: provider, model, key, optional connection test, then save.",
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
