pub mod approval_ui;

use approval_ui::TtyApprovalUi;
use harness_core::agent_loop::{ build_default_runtime, AgentRunConfig, AgentRunner };
use harness_core::budget::BudgetConfig;
use harness_core::completion_gate::{configured_timeout, parse_command_argv, WorkspaceCompletionGate};
use harness_core::env::{AllowAllGate, ExecutionEnv, LocalHostEnv, SharedCompletionGate};
use harness_core::events::{ resolve_default_state_dir, EventLogger };
use harness_core::prompt::PromptBuilder;
use harness_core::session::SessionJournal;
use harness_core::provider::chat_completions::{ ChatCompletionsClient, ChatCompletionsConfig };
use harness_core::tool::policy::{ PolicyEngine, PolicyMode };
use harness_core::types::RunStatus;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::SystemTime;
use tokio::sync::watch;

#[derive(Debug, Clone)]
pub struct CliArgs {
    pub workspace: PathBuf,
    pub task: String,
    pub policy_mode: PolicyMode,
    pub no_stream: bool,
    pub strict_log: bool,
    pub tool_filter: Option<Vec<String>>,
    pub reasoning_effort: Option<String>,
    pub quiet: bool,
    pub resume: Option<PathBuf>,
    pub fork_turn: Option<usize>,
    pub verify_cmd: Option<String>,
    pub no_verify: bool,
}

pub fn parse_cli_args<I>(args: I) -> Result<CliArgs, String>
where
    I: IntoIterator<Item = String>,
{
    let mut iter = args.into_iter().peekable();
    let _bin = iter.next();

    if let Some(first) = iter.peek() {
        if first == "--help" || first == "-h" || first == "help" {
            return Err(usage_text());
        }
        if first == "--version" || first == "-V" {
            return Err(format!("harness {}", harness_core::version()));
        }
        if first == "run" {
            iter.next();
        }
    }

    let mut workspace: Option<PathBuf> = None;
    let mut task: Option<String> = None;
    let mut policy_flag: Option<String> = None;
    let mut allow_write = false;
    let mut allow_shell = false;
    let mut no_stream = false;
    let mut strict_log = false;
    let mut tool_filter: Option<Vec<String>> = None;
    let mut reasoning_effort: Option<String> = None;
    let mut quiet = false;
    let mut resume: Option<PathBuf> = None;
    let mut fork_turn: Option<usize> = None;
    let mut verify_cmd: Option<String> = None;
    let mut no_verify = false;

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--workspace" | "-w" => {
                let val = iter
                    .next()
                    .ok_or_else(|| "missing value for --workspace".to_string())?;
                workspace = Some(PathBuf::from(val));
            }
            "--task" | "-t" => {
                let val = iter
                    .next()
                    .ok_or_else(|| "missing value for --task".to_string())?;
                task = Some(val);
            }
            "--policy" => {
                let val = iter
                    .next()
                    .ok_or_else(|| "missing value for --policy".to_string())?;
                policy_flag = Some(val);
            }
            "--allow-write" => {
                allow_write = true;
            }
            "--allow-shell" => {
                allow_shell = true;
            }
            "--no-stream" => {
                no_stream = true;
            }
            "--strict-log" => {
                strict_log = true;
            }
            "--reasoning-effort" => {
                let val = iter
                    .next()
                    .ok_or_else(|| "missing value for --reasoning-effort".to_string())?;
                let trimmed = val.trim().to_string();
                if trimmed.is_empty() {
                    return Err("empty value for --reasoning-effort".to_string());
                }
                reasoning_effort = Some(trimmed);
            }
            "--quiet" | "-q" => {
                quiet = true;
            }
            "--resume" => {
                let val = iter.next().ok_or_else(|| "missing value for --resume".to_string())?;
                resume = Some(PathBuf::from(val));
            }
            "--fork" => {
                let val = iter.next().ok_or_else(|| "missing value for --fork <TURN>".to_string())?;
                fork_turn = Some(val.parse::<usize>().map_err(|_| "--fork expects a numeric turn index".to_string())?);
            }
            "--verify-cmd" => {
                let val = iter.next().ok_or_else(|| "missing value for --verify-cmd <CMD>".to_string())?;
                if val.trim().is_empty() {
                    return Err("empty value for --verify-cmd".to_string());
                }
                verify_cmd = Some(val);
            }
            "--no-verify" => {
                no_verify = true;
            }
            "--tool-filter" => {
                let val = iter
                    .next()
                    .ok_or_else(|| "missing value for --tool-filter".to_string())?;
                let list: Vec<String> = val
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                tool_filter = Some(list);
            }
            "--api-key" => {
                return Err(
                    "passing API key via CLI flag is forbidden for security; set HARNESS_API_KEY in environment (§2.1)"
                        .to_string(),
                );
            }
            "--help" | "-h" => {
                return Err(usage_text());
            }
            "--version" | "-V" => {
                return Err(format!("harness {}", harness_core::version()));
            }
            other => {
                return Err(format!("unrecognized argument '{}'\n\n{}", other, usage_text()));
            }
        }
    }

    let task = task.ok_or_else(|| "missing required --task <prompt>".to_string())?;
    if fork_turn.is_some() && resume.is_none() {
        return Err("--fork <TURN> requires --resume <SESSION_WAL>".to_string());
    }

    // Resolve `--resume` against the session store first, so `latest`, a bare session id
    // and a plain filename all work before we derive the workspace.
    let resume = match resume {
        Some(raw) => {
            let state_dir = resolve_default_state_dir();
            Some(resolve_resume_path(&raw, &state_dir, workspace.as_deref())?)
        }
        None => None,
    };

    // `--workspace` is optional when resuming: the WAL header records the workspace root.
    let workspace = match workspace {
        Some(w) => w,
        None => match resume.as_ref() {
            Some(path) => match SessionJournal::open(path) {
                Ok(journal) => journal.workspace_root().to_path_buf(),
                Err(e) => {
                    return Err(format!(
                        "failed to open session WAL {} to infer workspace: {}",
                        path.display(),
                        e
                    ))
                }
            },
            None => return Err("missing required --workspace <dir>".to_string()),
        },
    };

    let policy_mode = PolicyMode::from_flags(policy_flag.as_deref(), allow_write, allow_shell)?;

    Ok(CliArgs {
        workspace,
        task,
        policy_mode,
        no_stream,
        strict_log,
        tool_filter,
        reasoning_effort,
        quiet,
        resume,
        fork_turn,
        verify_cmd,
        no_verify,
    })
}

/// Resolve a `--resume` argument into a concrete session WAL path.
///
/// Accepts:
/// - an existing path, used verbatim;
/// - the literal `latest`, selecting the newest `*.wal.jsonl` under the session store
///   (restricted to the current workspace root when one is provided);
/// - a session UUID / file name, matched exactly or by unique filename prefix under the store.
fn resolve_resume_path(
    raw: &std::path::Path,
    state_dir: &std::path::Path,
    workspace: Option<&std::path::Path>,
) -> Result<PathBuf, String> {
    let sessions_dir = state_dir.join("sessions");
    let raw_str = raw.to_string_lossy().to_string();

    if raw_str == "latest" {
        let entries = std::fs::read_dir(&sessions_dir).map_err(|e| {
            format!(
                "cannot read session store {}: {}",
                sessions_dir.display(),
                e
            )
        })?;
        let ws_canon = workspace.map(|w| w.canonicalize().unwrap_or_else(|_| w.to_path_buf()));
        let mut newest: Option<(SystemTime, PathBuf)> = None;
        for entry in entries.flatten() {
            let path = entry.path();
            let is_wal = path
                .file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.ends_with(".wal.jsonl"))
                .unwrap_or(false);
            if !is_wal {
                continue;
            }
            if let Some(ws) = ws_canon.as_deref() {
                match SessionJournal::open(&path) {
                    Ok(journal) if journal.workspace_root() == ws => {}
                    _ => continue,
                }
            }
            let modified = match entry.metadata().and_then(|m| m.modified()) {
                Ok(t) => t,
                Err(_) => continue,
            };
            if newest.as_ref().map(|(t, _)| modified > *t).unwrap_or(true) {
                newest = Some((modified, path));
            }
        }
        return newest.map(|(_, p)| p).ok_or_else(|| {
            format!(
                "no session WAL found for --resume latest in {}",
                sessions_dir.display()
            )
        });
    }

    if raw.exists() {
        validate_resume_path_candidate(raw)?;
        return Ok(raw.to_path_buf());
    }

    // A bare session id / filename resolves against the session store.
    let direct = sessions_dir.join(raw);
    if direct.exists() {
        return Ok(direct);
    }
    if let Ok(entries) = std::fs::read_dir(&sessions_dir) {
        let mut matches: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.starts_with(raw_str.as_str()))
                    .unwrap_or(false)
            })
            .collect();
        matches.sort();
        if matches.len() == 1 {
            let only = matches.remove(0);
            validate_resume_path_candidate(&only)?;
            return Ok(only);
        }
    }

    if raw.exists() {
        validate_resume_path_candidate(raw)?;
    }
    Ok(raw.to_path_buf())
}

/// F-01: a `--resume` path must be an existing, non-symlink regular file. The deeper WAL-header
/// and hash-chain validation happens in `SessionJournal::open`; this only rejects obviously wrong
/// inputs (symlinks, directories, devices) before any open-for-write occurs.
fn validate_resume_path_candidate(path: &std::path::Path) -> Result<(), String> {
    let meta = std::fs::symlink_metadata(path)
        .map_err(|e| format!("cannot stat --resume path {}: {}", path.display(), e))?;
    if meta.file_type().is_symlink() {
        return Err(format!(
            "--resume path {} is a symlink; refusing to open it as a session WAL",
            path.display()
        ));
    }
    if !meta.file_type().is_file() {
        return Err(format!(
            "--resume path {} is not a regular file",
            path.display()
        ));
    }
    // F-01: reject an oversized explicit path before any open-for-write can occur.
    if meta.len() > harness_core::session::MAX_WAL_FILE_BYTES {
        return Err(format!(
            "--resume path {} is {} bytes, exceeding the {} byte WAL cap",
            path.display(),
            meta.len(),
            harness_core::session::MAX_WAL_FILE_BYTES
        ));
    }
    Ok(())
}

/// How the CompletionGate should be constructed for this run.
#[derive(Debug, PartialEq, Eq)]
pub enum CompletionGateChoice {
    /// `--no-verify` or a read-only policy: never spawn a verification process.
    Disabled,
    /// An explicitly authorized command from `--verify-cmd` or `HARNESS_VERIFY_COMMAND`.
    Explicit(Vec<String>),
    /// Auto-detect from project manifests; `allow_auto` mirrors `--allow-shell` authorization.
    AutoDetect { allow_auto: bool },
}

/// Decide how the CompletionGate should run without spawning anything. Auto-detected project test
/// scripts are untrusted code, so they are only run when `--allow-shell` was explicitly granted;
/// an explicit `--verify-cmd` / `HARNESS_VERIFY_COMMAND` is itself the operator's authorization.
fn choose_completion_gate(
    cli: &CliArgs,
    env_verify_cmd: Option<String>,
) -> Result<CompletionGateChoice, String> {
    if cli.no_verify || cli.policy_mode == PolicyMode::ReadOnly {
        return Ok(CompletionGateChoice::Disabled);
    }
    if let Some(raw) = cli.verify_cmd.clone().or(env_verify_cmd) {
        let argv = parse_command_argv(&raw)?;
        if argv.is_empty() {
            return Err("verification command is empty".to_string());
        }
        return Ok(CompletionGateChoice::Explicit(argv));
    }
    Ok(CompletionGateChoice::AutoDetect { allow_auto: cli.policy_mode.allows_shell_auto() })
}

fn build_completion_gate(choice: CompletionGateChoice) -> SharedCompletionGate {
    match choice {
        CompletionGateChoice::Disabled => Arc::new(AllowAllGate),
        CompletionGateChoice::Explicit(argv) => {
            Arc::new(WorkspaceCompletionGate::new(Some(argv), configured_timeout()))
        }
        CompletionGateChoice::AutoDetect { allow_auto } => Arc::new(
            WorkspaceCompletionGate::with_auto_detection(None, configured_timeout(), allow_auto),
        ),
    }
}

pub fn usage_text() -> String {
    "\
Usage: harness run --workspace <DIR> --task <TASK> [OPTIONS]

Options:
  --workspace <DIR>         Target repository workspace directory
  --task <TASK>             Coding or analysis task description
  --policy <MODE>           Host policy: read-only | ask (default: ask)
  --allow-write             Auto-allow workspace file mutations (edit/write)
  --allow-shell             Auto-allow supervised shell commands (bash)
  --reasoning-effort <LVL>  Reasoning effort level (e.g. low, medium, high)
  --quiet, -q               Suppress live progress output on stderr
  --no-stream               Fallback to non-streaming JSON requests
  --strict-log              Fail-closed if diagnostic JSONL event logging fails
  --resume <WAL|latest>     Resume a session WAL, session id, or the latest session (latest)
  --fork <TURN>             Branch from a prior turn (requires --resume)
  --verify-cmd <CMD>        Explicit completion-check command (overrides HARNESS_VERIFY_COMMAND)
  --no-verify               Disable the completion verification gate entirely
  --tool-filter <CSV>       Benchmarking ablation filter (e.g. read,edit,write,bash)

Environment Credentials:
  HARNESS_API_KEY           Provider API key (stripped from child shell environments)
  HARNESS_BASE_URL          OpenAI-compatible base URL (HTTPS required for remote hosts)
  HARNESS_MODEL             Model identifier
  HARNESS_REASONING_EFFORT  Reasoning effort fallback when --reasoning-effort is not set
  HARNESS_VERIFY_COMMAND   Optional argv-style completion check, e.g. \"cargo test --workspace\"
  HARNESS_VERIFY_TIMEOUT_SECS  Completion check timeout (default 180)
  HARNESS_GATE_MAX_RETRIES Maximum completion-gate rejections (default 3)

Security Notice:
  Sensitive path guard blocks accidental access to .env/keys via file tools;
  granting --allow-shell executes commands under the current OS user without an OS container sandbox."
        .to_string()
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = match parse_cli_args(std::env::args()) {
        Ok(c) => c,
        Err(msg) => {
            if msg.starts_with("Usage:") || msg.starts_with("harness ") {
                println!("{}", msg);
                return ExitCode::SUCCESS;
            }
            eprintln!("error: {}", msg);
            return ExitCode::from(2);
        }
    };

    let api_key = match std::env::var("HARNESS_API_KEY") {
        Ok(k) if !k.trim().is_empty() => k,
        _ => {
            eprintln!("error: HARNESS_API_KEY environment variable is required");
            return ExitCode::from(2);
        }
    };
    let base_url = std::env::var("HARNESS_BASE_URL")
        .unwrap_or_else(|_| "https://api.openai.com".to_string());
    let model = std::env::var("HARNESS_MODEL").unwrap_or_else(|_| "gpt-4o".to_string());
    let reasoning_effort = cli.reasoning_effort.clone().or_else(|| {
        std::env::var("HARNESS_REASONING_EFFORT")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    });

    let provider = match ChatCompletionsClient::new(ChatCompletionsConfig {
        base_url,
        api_key,
        model,
        max_retries: 2,
        reasoning_effort,
    }) {
        Ok(c) => Arc::new(c),
        Err(e) => {
            eprintln!("error: invalid provider configuration: {}", e);
            return ExitCode::from(2);
        }
    };

    let budget = BudgetConfig::default();
    let state_dir = resolve_default_state_dir();
    let env = match LocalHostEnv::new(&cli.workspace, state_dir.clone()) {
        Ok(e) => Arc::new(e),
        Err(e) => {
            eprintln!("error: failed to initialize workspace capability: {}", e);
            return ExitCode::from(2);
        }
    };

    // Freeze the workspace orientation block (AGENTS.md/CLAUDE.md + sorted top-level entries)
    // into the static system prefix before `env` is moved into the tool runtime.
    let prompt_builder = PromptBuilder::default().with_workspace_context(env.workspace());

    let session = if let Some(path) = cli.resume.as_ref() {
        let mut journal = match SessionJournal::open(path) {
            Ok(j) => j,
            Err(e) => {
                eprintln!("error: failed to resume session WAL: {}", e);
                return ExitCode::from(2);
            }
        };
        if journal.workspace_root() != env.workspace().root() {
            eprintln!("error: session WAL belongs to workspace {}, not {}", journal.workspace_root().display(), env.workspace().root().display());
            return ExitCode::from(2);
        }
        if let Some(turn) = cli.fork_turn {
            if let Err(e) = journal.fork_at_turn(turn) {
                eprintln!("error: failed to fork session at turn {}: {}", turn, e);
                return ExitCode::from(2);
            }
        }
        journal
    } else {
        match SessionJournal::create(state_dir.join("sessions"), env.workspace().root()) {
            Ok(j) => j,
            Err(e) => {
                eprintln!("error: failed to create session WAL: {}", e);
                return ExitCode::from(1);
            }
        }
    };
    if !cli.quiet {
        eprintln!("[harness] session WAL: {}", session.path().display());
    }

    let gate_choice = match choose_completion_gate(&cli, std::env::var("HARNESS_VERIFY_COMMAND").ok()) {
        Ok(choice) => choice,
        Err(e) => {
            eprintln!("error: {}", e);
            return ExitCode::from(2);
        }
    };

    let mut run_cfg = AgentRunConfig::new(cli.task, budget.clone());
    run_cfg.stream = !cli.no_stream;
    run_cfg.strict_log = cli.strict_log;

    let mut logger = match EventLogger::new(run_cfg.run_id, Some(state_dir), cli.strict_log) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("error: {}", e);
            return ExitCode::from(1);
        }
    };
    logger.set_live_stderr(!cli.quiet);

    let approval_ui = Arc::new(TtyApprovalUi::new());
    let policy_engine = PolicyEngine::new(cli.policy_mode, approval_ui);

    let runtime = build_default_runtime(env, policy_engine, budget, cli.tool_filter);

    let (cancel_tx, cancel_rx) = watch::channel(false);
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            // F-06: unblock a synchronous TTY approval prompt so Ctrl-C actually cancels.
            crate::approval_ui::APPROVAL_CANCELLED.store(true, std::sync::atomic::Ordering::SeqCst);
            let _ = cancel_tx.send(true);
            if tokio::signal::ctrl_c().await.is_ok() {
                std::process::exit(130);
            }
        }
    });

    let max_gate_retries = std::env::var("HARNESS_GATE_MAX_RETRIES")
        .ok().and_then(|s| s.parse::<usize>().ok()).filter(|n| *n > 0).unwrap_or(3);
    let runner = AgentRunner::new(run_cfg, provider, runtime, cancel_rx)
        .with_prompt_builder(prompt_builder)
        .with_session_journal(session)
        .with_completion_gate(build_completion_gate(gate_choice))
        .with_completion_gate_max_retries(max_gate_retries);
    let (summary, _logger) = runner.run(logger).await;

    if let Some(ref text) = summary.final_assistant_text {
        println!("{}", text);
    }

    if summary.log_degraded {
        eprintln!(
            "[WARN harness] run completed with degraded diagnostic logging ({} events dropped)",
            summary.dropped_events
        );
    }

    match summary.status {
        RunStatus::Completed => ExitCode::SUCCESS,
        RunStatus::Cancelled => ExitCode::from(130),
        other => {
            eprintln!(
                "[harness] run terminated with status {:?}: {:?}",
                other, summary.terminal_reason
            );
            ExitCode::from(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use approval_ui::TtyApprovalUi;
    use harness_core::tool::policy::{ApprovalDecision, ApprovalHandler, ApprovalRequest};
    use harness_core::types::EffectClass;

    #[test]
    fn test_cli_flags_and_policy_modes() {
        let ro = parse_cli_args(vec![
            "harness".into(),
            "run".into(),
            "--workspace".into(),
            "./repo".into(),
            "--task".into(),
            "analyze".into(),
            "--policy".into(),
            "read-only".into(),
        ])
        .unwrap();
        assert_eq!(ro.policy_mode, PolicyMode::ReadOnly);

        let ask_default = parse_cli_args(vec![
            "harness".into(),
            "run".into(),
            "--workspace".into(),
            "./repo".into(),
            "--task".into(),
            "fix bug".into(),
        ])
        .unwrap();
        assert_eq!(ask_default.policy_mode, PolicyMode::Ask);

        let allow_w = parse_cli_args(vec![
            "harness".into(),
            "run".into(),
            "--workspace".into(),
            "./repo".into(),
            "--task".into(),
            "refactor".into(),
            "--allow-write".into(),
        ])
        .unwrap();
        assert_eq!(allow_w.policy_mode, PolicyMode::AllowWrite);

        let full_auto = parse_cli_args(vec![
            "harness".into(),
            "run".into(),
            "--workspace".into(),
            "./repo".into(),
            "--task".into(),
            "test".into(),
            "--allow-write".into(),
            "--allow-shell".into(),
            "--no-stream".into(),
            "--strict-log".into(),
            "--tool-filter".into(),
            "read,edit,write,bash".into(),
        ])
        .unwrap();
        assert_eq!(full_auto.policy_mode, PolicyMode::FullAutonomous);
        assert!(full_auto.no_stream);
        assert!(full_auto.strict_log);
        assert_eq!(
            full_auto.tool_filter,
            Some(vec![
                "read".into(),
                "edit".into(),
                "write".into(),
                "bash".into()
            ])
        );
        assert_eq!(full_auto.reasoning_effort, None);
        assert!(!full_auto.quiet);

        let with_effort_quiet = parse_cli_args(vec![
            "harness".into(),
            "run".into(),
            "--workspace".into(),
            "./repo".into(),
            "--task".into(),
            "think hard".into(),
            "--reasoning-effort".into(),
            "high".into(),
            "--quiet".into(),
        ])
        .unwrap();
        assert_eq!(with_effort_quiet.reasoning_effort.as_deref(), Some("high"));
        assert!(with_effort_quiet.quiet);

        let resumed = parse_cli_args(vec![
            "harness".into(), "run".into(), "--workspace".into(), "./repo".into(),
            "--task".into(), "continue".into(), "--resume".into(), "session.wal.jsonl".into(),
            "--fork".into(), "10".into(),
        ]).unwrap();
        assert_eq!(resumed.resume.as_deref(), Some(std::path::Path::new("session.wal.jsonl")));
        assert_eq!(resumed.fork_turn, Some(10));
        assert!(parse_cli_args(vec![
            "harness".into(), "run".into(), "--workspace".into(), "./repo".into(),
            "--task".into(), "continue".into(), "--fork".into(), "10".into(),
        ]).unwrap_err().contains("requires --resume"));

        // Passing invalid --policy with --allow-write must be rejected
        let bad_policy = parse_cli_args(vec![
            "harness".into(),
            "run".into(),
            "--workspace".into(),
            "./repo".into(),
            "--task".into(),
            "x".into(),
            "--policy".into(),
            "typo".into(),
            "--allow-write".into(),
        ]);
        assert!(bad_policy.is_err());

        // Passing --version after `run` must work
        let ver_res = parse_cli_args(vec!["harness".into(), "run".into(), "--version".into()]);
        assert_eq!(
            ver_res.unwrap_err(),
            format!("harness {}", harness_core::version())
        );

        // Passing --api-key on CLI must be rejected (§2.1)
        let bad_key = parse_cli_args(vec![
            "harness".into(),
            "run".into(),
            "--workspace".into(),
            "./repo".into(),
            "--task".into(),
            "x".into(),
            "--api-key".into(),
            "sk-123".into(),
        ]);
        assert!(bad_key.is_err());

        // Non-TTY approval UI must immediately deny without blocking
        let ui = TtyApprovalUi::with_tty_override(false);
        let req = ApprovalRequest {
            tool_name: "bash".into(),
            effect_class: EffectClass::SideEffect,
            is_sensitive_path: false,
            target_path: None,
            diff_preview: None,
            command_preview: Some("cargo test".into()),
            workspace_root: PathBuf::from("/tmp"),
        };
        assert_eq!(ui.request_approval(&req), ApprovalDecision::Deny);
    }

    fn base_cli(extra: Vec<String>) -> Vec<String> {
        let mut args = vec![
            "harness".into(),
            "run".into(),
            "--workspace".into(),
            "./repo".into(),
            "--task".into(),
            "task".into(),
        ];
        args.extend(extra);
        args
    }

    #[test]
    fn test_verify_cmd_and_no_verify_flags_parse() {
        let explicit = parse_cli_args(base_cli(vec![
            "--verify-cmd".into(),
            "sh -c \"cargo test --workspace\"".into(),
        ]))
        .unwrap();
        assert_eq!(explicit.verify_cmd.as_deref(), Some("sh -c \"cargo test --workspace\""));
        assert!(!explicit.no_verify);

        let disabled = parse_cli_args(base_cli(vec!["--no-verify".into()])).unwrap();
        assert!(disabled.no_verify);
        assert!(disabled.verify_cmd.is_none());

        // Empty or missing values must be rejected rather than silently ignored.
        assert!(parse_cli_args(base_cli(vec!["--verify-cmd".into(), "   ".into()])).is_err());
        assert!(parse_cli_args(base_cli(vec!["--verify-cmd".into()])).is_err());
    }

    #[test]
    fn test_completion_gate_selection_honors_shell_authorization() {
        // An explicit --verify-cmd runs even without --allow-shell.
        let explicit = parse_cli_args(base_cli(vec![
            "--verify-cmd".into(),
            "sh -c \"cargo test\"".into(),
        ]))
        .unwrap();
        assert_eq!(
            choose_completion_gate(&explicit, None).unwrap(),
            CompletionGateChoice::Explicit(vec!["sh".into(), "-c".into(), "cargo test".into()])
        );

        // HARNESS_VERIFY_COMMAND is treated as explicit authorization too.
        let no_cmd = parse_cli_args(base_cli(vec![])).unwrap();
        assert_eq!(
            choose_completion_gate(&no_cmd, Some("cargo test --workspace".into())).unwrap(),
            CompletionGateChoice::Explicit(vec!["cargo".into(), "test".into(), "--workspace".into()])
        );

        // Without an explicit command and without --allow-shell, auto-detection must stay off.
        assert_eq!(
            choose_completion_gate(&no_cmd, None).unwrap(),
            CompletionGateChoice::AutoDetect { allow_auto: false }
        );

        // --allow-shell authorizes auto-detected project verification.
        let shell = parse_cli_args(base_cli(vec!["--allow-shell".into()])).unwrap();
        assert_eq!(
            choose_completion_gate(&shell, None).unwrap(),
            CompletionGateChoice::AutoDetect { allow_auto: true }
        );

        // --no-verify wins over an otherwise-authorized explicit command.
        let disabled = parse_cli_args(base_cli(vec![
            "--allow-shell".into(),
            "--no-verify".into(),
        ]))
        .unwrap();
        assert_eq!(
            choose_completion_gate(&disabled, Some("cargo test".into())).unwrap(),
            CompletionGateChoice::Disabled
        );

        // Read-only policy never enables verification.
        let ro = parse_cli_args(base_cli(vec!["--policy".into(), "read-only".into()])).unwrap();
        assert_eq!(choose_completion_gate(&ro, None).unwrap(), CompletionGateChoice::Disabled);
    }
}

