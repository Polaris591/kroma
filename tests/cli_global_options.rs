//! Global options parse once for every command: before or after a subcommand,
//! inline `--name=value` or separate, with strict option values. These are
//! parser and CLI usage checks only - no model, network or saved profile.
use kroma::cli::{self, Command, GlobalOptions, RunOptions};
use std::{
    ffi::OsString,
    path::PathBuf,
    process::{Command as Process, Stdio},
};

fn args(value: &[&str]) -> Vec<String> {
    value.iter().map(|item| (*item).to_owned()).collect()
}

fn run_options(command: Command) -> RunOptions {
    match command {
        Command::Run(options) => options,
        other => panic!("expected a run command, got {other:?}"),
    }
}

#[test]
fn globals_before_a_subcommand_keep_the_subcommand() {
    let (globals, command) = cli::parse_with_globals(&args(&[
        "--config-file",
        "work.json",
        "--quiet",
        "config",
        "show",
    ]))
    .expect("parse");
    assert_eq!(command, Command::Config(args(&["show"])));
    assert_eq!(globals.config_file, Some(PathBuf::from("work.json")));
    assert!(globals.quiet);
    assert_eq!(
        globals.environment(),
        vec![
            ("KROMA_CONFIG_FILE", OsString::from("work.json")),
            ("KROMA_QUIET", OsString::from("1")),
        ]
    );
}

#[test]
fn globals_after_a_subcommand_do_not_reach_its_arguments() {
    let (globals, command) =
        cli::parse_with_globals(&args(&["config", "show", "--cwd=proj", "-q", "--no-color"]))
            .expect("parse");
    assert_eq!(command, Command::Config(args(&["show"])));
    assert_eq!(globals.cwd, Some(PathBuf::from("proj")));
    assert!(globals.quiet && globals.no_color);
    assert_eq!(
        globals.environment(),
        vec![
            ("KROMA_WORKDIR", OsString::from("proj")),
            ("KROMA_QUIET", OsString::from("1")),
            ("NO_COLOR", OsString::from("1")),
        ]
    );
}

#[test]
fn inline_and_separate_global_values_agree() {
    let options = run_options(
        cli::parse(&args(&[
            "--cwd=proj",
            "--language=zh",
            "--mcp=a,b",
            "--config-file",
            "work.json",
            "task",
        ]))
        .expect("parse"),
    );
    assert_eq!(options.cwd, Some(PathBuf::from("proj")));
    assert_eq!(options.language.as_deref(), Some("zh"));
    assert_eq!(options.config_file, Some(PathBuf::from("work.json")));
    assert_eq!(options.mcp, args(&["a", "b"]));
    assert_eq!(options.prompt, args(&["task"]));
}

#[test]
fn repeated_mcp_options_accumulate_in_order() {
    let (globals, command) = cli::parse_with_globals(&args(&[
        "--mcp", "a,b", "task", "--mcp=c", "--mcp", " d , e ",
    ]))
    .expect("parse");
    assert_eq!(globals.mcp, args(&["a", "b", "c", "d", "e"]));
    assert_eq!(
        globals.environment(),
        vec![("KROMA_MCP_SERVERS", OsString::from("a,b,c,d,e"))]
    );
    let options = run_options(command);
    assert_eq!(options.task(), "task");
    assert_eq!(options.mcp, args(&["a", "b", "c", "d", "e"]));
}

#[test]
fn double_dash_keeps_global_looking_text_as_prompt() {
    let (globals, command) =
        cli::parse_with_globals(&args(&["--print", "--", "--quiet", "task"])).expect("parse");
    assert!(!globals.quiet);
    assert!(globals.environment().is_empty());
    let options = run_options(command);
    assert!(options.print);
    assert_eq!(options.prompt, args(&["--quiet", "task"]));

    let (globals, command) =
        cli::parse_with_globals(&args(&["config", "--", "--quiet"])).expect("parse");
    assert!(!globals.quiet);
    assert_eq!(command, Command::Config(args(&["--", "--quiet"])));
}

#[test]
fn an_option_value_may_not_be_another_option() {
    for line in [
        vec!["--session", "--quiet", "task"],
        vec!["--session", "-q", "task"],
        vec!["--cwd", "--quiet", "task"],
        vec!["--mcp", "--quiet", "task"],
        vec!["--language", "--no-color"],
        vec!["--config-file", "--verbose"],
    ] {
        let error = cli::parse(&args(&line)).expect_err("strict value");
        assert_eq!(error.code, 2, "{line:?}");
        assert!(
            error.message.contains("needs a value"),
            "{line:?}: {}",
            error.message
        );
    }
    let error = cli::parse(&args(&["--session", "--quiet", "task"])).expect_err("strict value");
    assert!(
        error.message.contains("--session needs a value"),
        "{}",
        error.message
    );
    assert!(!error.message.contains("task"), "{}", error.message);
}

#[test]
fn run_options_still_carry_globals_for_compatibility() {
    let options = run_options(
        cli::parse(&args(&[
            "task",
            "--quiet",
            "--verbose",
            "--no-mcp",
            "--no-color",
            "--no-tui",
        ]))
        .expect("parse"),
    );
    assert_eq!(options.prompt, args(&["task"]));
    assert!(options.quiet);
    assert!(options.verbose);
    assert!(options.no_mcp);
    assert!(options.no_color);
    assert!(options.no_tui);
    assert!(!options.one_shot());
}

#[test]
fn conflicting_session_flags_stay_usage_errors() {
    for line in [
        vec!["--new", "--quiet", "--continue"],
        vec!["--new", "--session", "x"],
        vec!["--session", "x", "-c"],
    ] {
        let error = cli::parse(&args(&line)).expect_err("conflict");
        assert_eq!(error.code, 2, "{line:?}");
        assert!(
            error.message.contains("cannot be combined"),
            "{line:?}: {}",
            error.message
        );
    }
}

#[test]
fn unknown_and_invalid_options_stay_usage_errors() {
    let error = cli::parse(&args(&["--bogus", "task"])).expect_err("unknown option");
    assert_eq!(error.code, 2);
    assert!(
        error.message.contains("unknown option '--bogus'"),
        "{}",
        error.message
    );
    assert!(cli::parse(&args(&["--quiet=1", "task"])).is_err());
    assert!(cli::parse(&args(&["--mcp", ",", "task"])).is_err());
    assert!(cli::parse(&args(&["--language", "de", "task"])).is_err());
}

#[test]
fn help_and_version_passthrough_is_unchanged() {
    assert_eq!(
        cli::parse(&args(&["help"])).expect("help"),
        Command::Help(None)
    );
    assert_eq!(
        cli::parse(&args(&["help", "config"])).expect("help config"),
        Command::Help(Some("config".into()))
    );
    assert_eq!(cli::parse(&args(&["-h"])).expect("-h"), Command::Help(None));
    assert_eq!(
        cli::parse(&args(&["--quiet", "-h", "config"])).expect("global before help"),
        Command::Help(Some("config".into()))
    );
    assert_eq!(
        cli::parse(&args(&["task", "--help"])).expect("run help"),
        Command::Help(Some("run".into()))
    );
    assert_eq!(
        cli::parse(&args(&["setup", "--help"])).expect("setup help"),
        Command::Help(Some("setup".into()))
    );
    assert_eq!(
        cli::parse(&args(&["setup"])).expect("setup"),
        Command::Setup
    );
    assert!(cli::parse(&args(&["setup", "--quiet"])).is_ok());
    assert!(cli::parse(&args(&["setup", "--bogus"])).is_err());
    assert_eq!(
        cli::parse(&args(&["version"])).expect("version"),
        Command::Version { build_info: false }
    );
    assert_eq!(
        cli::parse(&args(&["--build-info"])).expect("build info"),
        Command::Version { build_info: true }
    );
}

#[test]
fn subcommand_arguments_survive_global_extraction() {
    assert_eq!(
        cli::parse(&args(&["plugins", "list", "--verbose"])).expect("plugins"),
        Command::Plugins(args(&["list"]))
    );
    assert_eq!(
        cli::parse(&args(&["recover-context", "s1"])).expect("recover"),
        Command::RecoverContext(Some("s1".into()))
    );
    assert_eq!(
        cli::parse(&args(&["diagnostics", "--output", "dir", "--quiet"])).expect("diagnostics"),
        Command::Diagnostics(Some(PathBuf::from("dir")))
    );
    assert_eq!(
        cli::parse(&args(&["trust-project", "--revoke"])).expect("trust"),
        Command::TrustProject { revoke: true }
    );
}

#[test]
fn environment_defaults_are_empty_without_globals() {
    assert!(GlobalOptions::default().environment().is_empty());
    assert_eq!(
        cli::parse(&args(&["task"])).expect("parse"),
        Command::Run(RunOptions {
            prompt: args(&["task"]),
            ..RunOptions::default()
        })
    );
}

fn kroma(line: &[&str]) -> std::process::Output {
    Process::new(env!("CARGO_BIN_EXE_kroma"))
        .args(line)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run the kroma binary")
}

fn stderr(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn stdout(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn cli_rejects_an_option_used_as_another_options_value() {
    let output = kroma(&["--session", "--quiet", "task"]);
    let message = stderr(&output);
    assert_eq!(output.status.code(), Some(2), "{message}");
    assert!(message.contains("--session needs a value"), "{message}");
    assert!(!message.contains("task"), "{message}");

    let output = kroma(&["--cwd", "--quiet", "task"]);
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("--cwd needs a value"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn cli_usage_errors_keep_exit_code_two() {
    let output = kroma(&["--bogus", "task"]);
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("unknown option '--bogus'"),
        "{}",
        stderr(&output)
    );

    let output = kroma(&["--language", "de", "task"]);
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("--language must be en or zh"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn cli_help_still_prints_usage_with_globals_present() {
    for line in [vec!["--help"], vec!["--quiet", "--help"]] {
        let output = kroma(&line);
        assert!(output.status.success(), "{line:?}: {}", stderr(&output));
        let text = stdout(&output);
        assert!(text.contains("Usage:"), "{line:?}: {text}");
        assert!(text.contains("--mcp"), "{line:?}: {text}");
    }
    for line in [
        vec!["help", "config"],
        vec!["--cwd", ".", "config", "--help"],
    ] {
        let output = kroma(&line);
        assert!(output.status.success(), "{line:?}: {}", stderr(&output));
        let text = stdout(&output);
        assert!(text.contains("kroma config"), "{line:?}: {text}");
    }
}
