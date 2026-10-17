pub mod approval_ui;

use approval_ui::TtyApprovalUi;
use kroma_core::agent_loop::{build_runtime_with_registry, AgentRunConfig, AgentRunner};
use kroma_core::budget::BudgetConfig;
use kroma_core::completion_gate::{
    configured_timeout, parse_command_argv, WorkspaceCompletionGate,
};
use kroma_core::config_file::{ConfigOverrides, ResolvedConfig};
use kroma_core::env::{AllowAllGate, ExecutionEnv, LocalHostEnv, SharedCompletionGate};
use kroma_core::events::{resolve_default_state_dir, EventLogger, EventPayload};
use kroma_core::extensions::{
    discover_skills, CommandObserverHook, CommandPolicyHook, ExtensionManifest, ScopedExtension,
    SubtreeContextTracker,
};
use kroma_core::prompt::PromptBuilder;
use kroma_core::provider::{build_provider, ProviderProtocol, ProviderSettings};
use kroma_core::revision::{
    MergeRecoveryOutcome, RestoreResult, RevisionId, SnapshotStore, WorkspaceBoundForkError,
};
use kroma_core::session::SessionJournal;
use kroma_core::surface::{
    validate_daemon_listen_addr, InteractiveRepl, KromaDaemon, KromaRuntimeConfig, RpcDispatcher,
};
use kroma_core::tool::policy::{PolicyEngine, PolicyMode};
use kroma_core::tool::registry::ToolRegistry;
use kroma_core::types::RunStatus;
use kroma_core::workspace::WorkspaceCap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::SystemTime;
use tokio::sync::watch;

/// Gate 6: explicit runtime surface selected via subcommand or CLI flag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliSurface {
    Headless,
    Repl,
    Acp,
    Serve {
        listen_addr: String,
    },
    /// Item #3 (`v1.4.0`): paired, budget-locked A/B benchmark runner.
    Bench {
        suite: Option<PathBuf>,
        budget_tokens: u64,
        json: bool,
        executor: BenchExecutorKind,
        repetitions: usize,
        seed: u64,
    },
}

/// Which [`kroma_core::bench::BenchExecutor`] a `kroma bench` run uses.
///
/// `Synthetic` is the default: it validates the report plumbing without a model endpoint and is
/// labelled `synthetic`. `Provider` is the real measurement — it drives `AgentRunner` against the
/// configured model in an isolated per-task workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BenchExecutorKind {
    Synthetic,
    Provider,
}

impl BenchExecutorKind {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "synthetic" => Ok(BenchExecutorKind::Synthetic),
            "provider" | "real" => Ok(BenchExecutorKind::Provider),
            other => Err(format!(
                "unknown --executor '{}'; expected 'synthetic' or 'provider'",
                other
            )),
        }
    }
}

#[derive(Debug, Clone)]
pub struct CliArgs {
    pub surface: CliSurface,
    pub workspace: PathBuf,
    pub task: Option<String>,
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
    /// Restore the workspace (and rewind the session leaf) to the previous turn's revision.
    pub undo: bool,
    /// Restore the workspace to a specific captured `WorkspaceRevision`.
    pub restore_rev: Option<String>,
    /// Allow `--undo` / `--restore-rev` to overwrite external modifications.
    pub force_restore: bool,
    /// List the workspace revisions captured for the session.
    pub list_revisions: bool,
    /// Gate 3: discover and print the `.agents/skills/*/SKILL.md` catalog, then exit.
    pub list_skills: bool,
    /// Gate 3: pre-activate a named `.agents/skills/<name>/SKILL.md` in the initial context.
    pub skill: Option<String>,
    /// Gate 3: command-backed fail-closed `PolicyHook` attached to `PreToolUse`.
    pub policy_hook: Option<String>,
    /// Gate 3: command-backed fail-open `ObserverHook` attached to `PostToolUse`.
    pub observer_hook: Option<String>,
    /// Gate 3: disable automatic JIT subtree `AGENTS.md`/`CLAUDE.md` loading.
    pub no_subtree_context: bool,
    /// Gate 4: disable the automatic post-`edit`/`write` structural check.
    pub no_predictive_check: bool,
    /// Gate 4: optional fast command run after a mutating batch to surface an early failure.
    pub predict_cmd: Option<String>,
    /// Gate 5: maximum number of subagent spawns per session (`0` disables `spawn_subagent`).
    pub max_subagents: usize,
    /// Capability gap #2 (`v1.5.2`): opt-in isolated counterfactual probe on repeated failures.
    pub counterfactual_probe: bool,
    /// Capability gap #5 (`v1.5.2`): when false, a repair race may merge a candidate whose
    /// verification was skipped (legacy behaviour). Strict by default.
    pub tournament_require_verified: bool,
    /// Capability gap #6 (`v1.5.2`): pin only core tool schemas and discover the rest on demand.
    pub on_demand_tools: bool,
    /// Capability gap #3 (`v1.5.2`): causal pruning of dead-end context in production runs.
    pub causal_pruning: bool,
    /// P1-4: `kroma serve` hosts a bounded multi-session control plane, routing each request to the
    /// runtime that owns the session named in it.
    pub multi_session: bool,
    /// Capability gap #6 (`v1.5.2`): trusted MCP servers from `--mcp-server NAME=COMMAND ARGS`.
    pub mcp_servers: Vec<kroma_core::config_file::McpServerConfig>,
    /// F05 (`v1.5.1`): explicit `--max-tokens` run-tree token cap (input + output).
    pub max_tokens: Option<u64>,
    /// Gate 6: headless output format (`text` or `jsonl`).
    pub output_format: String,
    /// Gate 6: optional authentication token for `kroma serve`.
    pub auth_token: Option<String>,
    /// Gate 7 (`v1.2.0`): wire protocol for the model endpoint
    /// (`auto` | `chat-completions` | `anthropic-messages` | `openai-responses`).
    pub protocol: String,
    /// `v1.3.0`: named profile from `~/.kroma/config.toml` to activate.
    pub profile: Option<String>,
    /// `v1.3.0`: true when `--protocol` was passed explicitly (so it outranks env/config).
    pub protocol_explicit: bool,
    /// `v1.3.0`: true when `--max-subagents` was passed explicitly.
    pub max_subagents_explicit: bool,
}

impl CliArgs {
    /// True when the invocation only performs a workspace-revision management action.
    pub fn is_revision_management(&self) -> bool {
        self.undo || self.restore_rev.is_some() || self.list_revisions
    }
}

pub fn parse_cli_args<I>(args: I) -> Result<CliArgs, String>
where
    I: IntoIterator<Item = String>,
{
    let mut iter = args.into_iter().peekable();
    let _bin = iter.next();

    let mut surface = CliSurface::Headless;
    let mut explicit_surface: Option<&'static str> = None;

    if let Some(first) = iter.peek() {
        if first == "--help" || first == "-h" || first == "help" {
            return Err(usage_text());
        }
        if first == "--version" || first == "-V" {
            return Err(format!("kroma {}", kroma_core::version()));
        }
        match first.as_str() {
            "run" => {
                iter.next();
                explicit_surface = Some("run");
            }
            "repl" | "tui" => {
                iter.next();
                surface = CliSurface::Repl;
                explicit_surface = Some("repl");
            }
            "acp" => {
                iter.next();
                surface = CliSurface::Acp;
                explicit_surface = Some("acp");
            }
            "serve" => {
                iter.next();
                surface = CliSurface::Serve {
                    listen_addr: "127.0.0.1:7878".to_string(),
                };
                explicit_surface = Some("serve");
            }
            "bench" => {
                iter.next();
                surface = CliSurface::Bench {
                    suite: None,
                    budget_tokens: kroma_core::bench::default_bench_budget_tokens(),
                    json: false,
                    executor: BenchExecutorKind::Synthetic,
                    repetitions: 1,
                    seed: kroma_core::bench::DEFAULT_BENCH_SEED,
                };
                explicit_surface = Some("bench");
            }
            _ => {}
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
    let mut protocol = "auto".to_string();
    let mut quiet = false;
    let mut resume: Option<PathBuf> = None;
    let mut fork_turn: Option<usize> = None;
    let mut verify_cmd: Option<String> = None;
    let mut no_verify = false;
    let mut undo = false;
    let mut restore_rev: Option<String> = None;
    let mut force_restore = false;
    let mut list_revisions = false;
    let mut list_skills = false;
    let mut skill: Option<String> = None;
    let mut policy_hook: Option<String> = None;
    let mut observer_hook: Option<String> = None;
    let mut no_subtree_context = false;
    let mut no_predictive_check = false;
    let mut predict_cmd: Option<String> = None;
    let mut max_subagents: usize = kroma_core::subagents::DEFAULT_MAX_SUBAGENT_SPAWNS;
    let mut max_subagents_explicit = false;
    // F05 (`v1.5.1`): `--max-tokens` override for the run-tree token cap.
    let mut max_tokens: Option<u64> = None;
    let mut no_subagents = false;
    let mut counterfactual_probe = false;
    let mut tournament_require_verified = true;
    let mut on_demand_tools = false;
    // Capability gap #3 (`v1.5.2`): opt-in causal pruning of dead-end context.
    let mut causal_pruning = false;
    // P1-4: bounded multi-session `kroma serve` daemon.
    let mut multi_session = false;
    // Capability gap #6 (`v1.5.2`): explicit trusted MCP servers.
    let mut mcp_servers: Vec<kroma_core::config_file::McpServerConfig> = Vec::new();
    let mut profile: Option<String> = None;
    let mut protocol_explicit = false;
    let mut output_format = "text".to_string();
    let mut auth_token: Option<String> = std::env::var("KROMA_DAEMON_TOKEN")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--workspace" | "-w" => {
                let val = iter
                    .next()
                    .ok_or_else(|| "missing value for --workspace".to_string())?;
                workspace = Some(PathBuf::from(val));
            }
            "--task" | "-t" | "--prompt" | "-p" => {
                let val = iter
                    .next()
                    .ok_or_else(|| "missing value for --task/--prompt".to_string())?;
                task = Some(val);
            }
            "--interactive" | "--repl" | "--tui" => {
                if let Some(prev) = explicit_surface {
                    if prev != "repl" {
                        return Err(format!(
                            "conflicting surface flags: cannot combine '{}' with '{}'",
                            prev, arg
                        ));
                    }
                }
                surface = CliSurface::Repl;
                explicit_surface = Some("repl");
            }
            "--acp" => {
                if let Some(prev) = explicit_surface {
                    if prev != "acp" {
                        return Err(format!(
                            "conflicting surface flags: cannot combine '{}' with '--acp'",
                            prev
                        ));
                    }
                }
                surface = CliSurface::Acp;
                explicit_surface = Some("acp");
            }
            "--serve" => {
                if let Some(prev) = explicit_surface {
                    if prev != "serve" {
                        return Err(format!(
                            "conflicting surface flags: cannot combine '{}' with '--serve'",
                            prev
                        ));
                    }
                }
                let val = iter
                    .next()
                    .ok_or_else(|| format!("missing value for {} <ADDR>", arg))?;
                let trimmed = val.trim().to_string();
                if trimmed.is_empty() {
                    return Err(format!("empty value for {}", arg));
                }
                surface = CliSurface::Serve {
                    listen_addr: trimmed,
                };
                explicit_surface = Some("serve");
            }
            "--listen" => {
                if let Some(prev) = explicit_surface {
                    if prev != "serve" {
                        return Err(format!(
                            "--listen <ADDR> is only valid with 'serve' / '--serve' (cannot combine with '{}')",
                            prev
                        ));
                    }
                }
                let val = iter
                    .next()
                    .ok_or_else(|| format!("missing value for {} <ADDR>", arg))?;
                let trimmed = val.trim().to_string();
                if trimmed.is_empty() {
                    return Err(format!("empty value for {}", arg));
                }
                surface = CliSurface::Serve {
                    listen_addr: trimmed,
                };
                explicit_surface = Some("serve");
            }
            "--auth-token" => {
                let val = iter
                    .next()
                    .ok_or_else(|| "missing value for --auth-token <TOKEN>".to_string())?;
                let trimmed = val.trim().to_string();
                if trimmed.is_empty() {
                    return Err("empty value for --auth-token".to_string());
                }
                auth_token = Some(trimmed);
            }
            "--output-format" => {
                let val = iter
                    .next()
                    .ok_or_else(|| "missing value for --output-format <text|jsonl>".to_string())?;
                match val.trim() {
                    "text" | "jsonl" => output_format = val.trim().to_string(),
                    other => {
                        return Err(format!(
                            "unknown --output-format '{}'; expected 'text' or 'jsonl'",
                            other
                        ))
                    }
                }
            }
            "--jsonl" => {
                output_format = "jsonl".to_string();
            }
            "--suite" => {
                let val = iter
                    .next()
                    .ok_or_else(|| "missing value for --suite <PATH>".to_string())?;
                if let CliSurface::Bench { suite, .. } = &mut surface {
                    *suite = Some(PathBuf::from(val));
                } else {
                    return Err("--suite is only valid for `kroma bench`".to_string());
                }
            }
            "--budget-tokens" => {
                let val = iter
                    .next()
                    .ok_or_else(|| "missing value for --budget-tokens <N>".to_string())?;
                let parsed: u64 = val
                    .trim()
                    .parse()
                    .map_err(|_| format!("invalid --budget-tokens '{}'", val))?;
                if parsed == 0 {
                    return Err("--budget-tokens must be > 0".to_string());
                }
                if let CliSurface::Bench { budget_tokens, .. } = &mut surface {
                    *budget_tokens = parsed;
                } else {
                    return Err("--budget-tokens is only valid for `kroma bench`".to_string());
                }
            }
            "--json" => {
                if let CliSurface::Bench { json, .. } = &mut surface {
                    *json = true;
                } else {
                    return Err("--json is only valid for `kroma bench`".to_string());
                }
            }
            "--executor" => {
                let val = iter.next().ok_or_else(|| {
                    "missing value for --executor <synthetic|provider>".to_string()
                })?;
                let parsed = BenchExecutorKind::parse(&val)?;
                if let CliSurface::Bench { executor, .. } = &mut surface {
                    *executor = parsed;
                } else {
                    return Err("--executor is only valid for `kroma bench`".to_string());
                }
            }
            "--repetitions" => {
                let val = iter
                    .next()
                    .ok_or_else(|| "missing value for --repetitions <N>".to_string())?;
                let parsed: usize = val
                    .trim()
                    .parse()
                    .map_err(|_| format!("invalid --repetitions '{}'", val))?;
                if parsed == 0 {
                    return Err("--repetitions must be > 0".to_string());
                }
                if let CliSurface::Bench { repetitions, .. } = &mut surface {
                    *repetitions = parsed;
                } else {
                    return Err("--repetitions is only valid for `kroma bench`".to_string());
                }
            }
            "--seed" => {
                let val = iter
                    .next()
                    .ok_or_else(|| "missing value for --seed <N>".to_string())?;
                let parsed: u64 = val
                    .trim()
                    .parse()
                    .map_err(|_| format!("invalid --seed '{}'", val))?;
                if let CliSurface::Bench { seed, .. } = &mut surface {
                    *seed = parsed;
                } else {
                    return Err("--seed is only valid for `kroma bench`".to_string());
                }
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
            "--protocol" => {
                let val = iter
                    .next()
                    .ok_or_else(|| "missing value for --protocol".to_string())?;
                // Validate here so a typo fails fast, before any credentials are read.
                let parsed = ProviderProtocol::parse(&val).map_err(|e| e.to_string())?;
                protocol = parsed.as_str().to_string();
                protocol_explicit = true;
            }
            "--profile" => {
                let val = iter
                    .next()
                    .ok_or_else(|| "missing value for --profile <NAME>".to_string())?;
                if val.trim().is_empty() {
                    return Err("empty value for --profile".to_string());
                }
                profile = Some(val);
            }
            "--quiet" | "-q" => {
                quiet = true;
            }
            "--resume" => {
                let val = iter
                    .next()
                    .ok_or_else(|| "missing value for --resume".to_string())?;
                resume = Some(PathBuf::from(val));
            }
            "--fork" => {
                let val = iter
                    .next()
                    .ok_or_else(|| "missing value for --fork <TURN>".to_string())?;
                fork_turn = Some(
                    val.parse::<usize>()
                        .map_err(|_| "--fork expects a numeric turn index".to_string())?,
                );
            }
            "--verify-cmd" => {
                let val = iter
                    .next()
                    .ok_or_else(|| "missing value for --verify-cmd <CMD>".to_string())?;
                if val.trim().is_empty() {
                    return Err("empty value for --verify-cmd".to_string());
                }
                verify_cmd = Some(val);
            }
            "--no-verify" => {
                no_verify = true;
            }
            "--undo" => {
                undo = true;
            }
            "--restore-rev" => {
                let val = iter
                    .next()
                    .ok_or_else(|| "missing value for --restore-rev <REV_ID>".to_string())?;
                if val.trim().is_empty() {
                    return Err("empty value for --restore-rev".to_string());
                }
                restore_rev = Some(val.trim().to_string());
            }
            "--force-restore" => {
                force_restore = true;
            }
            "--list-revisions" => {
                list_revisions = true;
            }
            "--list-skills" => {
                list_skills = true;
            }
            "--skill" => {
                let val = iter
                    .next()
                    .ok_or_else(|| "missing value for --skill <NAME>".to_string())?;
                if val.trim().is_empty() {
                    return Err("empty value for --skill".to_string());
                }
                skill = Some(val.trim().to_string());
            }
            "--policy-hook" => {
                let val = iter
                    .next()
                    .ok_or_else(|| "missing value for --policy-hook <CMD>".to_string())?;
                if val.trim().is_empty() {
                    return Err("empty value for --policy-hook".to_string());
                }
                policy_hook = Some(val);
            }
            "--observer-hook" => {
                let val = iter
                    .next()
                    .ok_or_else(|| "missing value for --observer-hook <CMD>".to_string())?;
                if val.trim().is_empty() {
                    return Err("empty value for --observer-hook".to_string());
                }
                observer_hook = Some(val);
            }
            "--no-subtree-context" => {
                no_subtree_context = true;
            }
            "--no-predictive-check" => {
                no_predictive_check = true;
            }
            // Capability gap #2 (`v1.5.2`): explicit operator consent for the isolated probe.
            "--counterfactual-probe" => {
                counterfactual_probe = true;
            }
            // Capability gap #5 (`v1.5.2`): relax the strict repair-race winner policy.
            "--allow-unverified-tournament" => {
                tournament_require_verified = false;
            }
            // Capability gap #6 (`v1.5.2`): expose only core tool schemas up front.
            "--on-demand-tools" => {
                on_demand_tools = true;
            }
            // Capability gap #3 (`v1.5.2`): prune dead-end context on failed branches.
            "--causal-pruning" => {
                causal_pruning = true;
            }
            // P1-4: host several sessions in one daemon and route by session id.
            "--multi-session" => {
                multi_session = true;
            }
            // Capability gap #6 (`v1.5.2`): an explicit, trusted MCP server definition.
            "--mcp-server" => {
                let val = iter.next().ok_or_else(|| {
                    "missing value for --mcp-server <NAME=COMMAND ARGS...>".to_string()
                })?;
                let (name, rest) = val
                    .split_once('=')
                    .ok_or_else(|| "--mcp-server expects NAME=COMMAND ARGS...".to_string())?;
                let name = name.trim();
                let mut parts = rest.split_whitespace();
                let command = parts.next().unwrap_or("").trim();
                if name.is_empty() || command.is_empty() {
                    return Err("--mcp-server expects NAME=COMMAND ARGS...".to_string());
                }
                mcp_servers.push(kroma_core::config_file::McpServerConfig {
                    name: name.to_string(),
                    command: command.to_string(),
                    args: parts.map(str::to_string).collect(),
                    env: Vec::new(),
                    read_only: false,
                });
            }
            "--predict-cmd" => {
                let val = iter
                    .next()
                    .ok_or_else(|| "missing value for --predict-cmd <CMD>".to_string())?;
                if val.trim().is_empty() {
                    return Err("empty value for --predict-cmd".to_string());
                }
                predict_cmd = Some(val);
            }
            "--max-subagents" => {
                let val = iter
                    .next()
                    .ok_or_else(|| "missing value for --max-subagents <N>".to_string())?;
                max_subagents = val
                    .parse::<usize>()
                    .map_err(|_| "--max-subagents expects a non-negative integer".to_string())?;
                max_subagents_explicit = true;
            }
            "--max-tokens" => {
                let val = iter
                    .next()
                    .ok_or_else(|| "missing value for --max-tokens <N>".to_string())?;
                let parsed: u64 = val
                    .trim()
                    .parse()
                    .map_err(|_| "--max-tokens expects a positive integer".to_string())?;
                if parsed == 0 {
                    return Err("--max-tokens must be > 0".to_string());
                }
                max_tokens = Some(parsed);
            }
            "--no-subagents" => {
                no_subagents = true;
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
                    "passing API key via CLI flag is forbidden for security; set KROMA_API_KEY in environment (§2.1)"
                        .to_string(),
                );
            }
            "--help" | "-h" => {
                return Err(usage_text());
            }
            "--version" | "-V" => {
                return Err(format!("kroma {}", kroma_core::version()));
            }
            other => {
                return Err(format!(
                    "unrecognized argument '{}'\n\n{}",
                    other,
                    usage_text()
                ));
            }
        }
    }

    let task = task.map(|t| t.trim().to_string()).filter(|t| !t.is_empty());
    if fork_turn.is_some() && resume.is_none() {
        return Err("--fork <TURN> requires --resume <SESSION_WAL>".to_string());
    }

    let management = undo || restore_rev.is_some() || list_revisions;
    if management {
        if resume.is_none() {
            return Err(
                "--undo, --restore-rev and --list-revisions require --resume <SESSION_WAL|latest>"
                    .to_string(),
            );
        }
        if undo && restore_rev.is_some() {
            return Err("--undo and --restore-rev are mutually exclusive".to_string());
        }
        if undo && fork_turn.is_some() {
            return Err("--undo already rewinds the session leaf; drop --fork".to_string());
        }
    } else if force_restore {
        return Err("--force-restore requires --undo or --restore-rev".to_string());
    }
    // `--list-skills` is a standalone inspection action: it needs a workspace but no task.
    // When invoked with no explicit subcommand (`kroma` or `kroma --allow-write`) and no
    // `--task` / management / inspection flag, launch the interactive REPL/TUI in the workspace
    // (defaulting to the current directory `.`), matching Claude Code / Codex / OpenCode ergonomics.
    if explicit_surface.is_none() && !management && !list_skills && task.is_none() {
        surface = CliSurface::Repl;
    }
    if surface == CliSurface::Headless && !management && !list_skills && task.is_none() {
        return Err("missing required --task <prompt>".to_string());
    }
    if task.is_some() && matches!(surface, CliSurface::Acp | CliSurface::Serve { .. }) {
        return Err(
            "--task / --prompt cannot be combined with 'acp' or 'serve' mode; send 'session.prompt' over JSON-RPC instead"
                .to_string(),
        );
    }
    if output_format == "jsonl" && surface != CliSurface::Headless {
        return Err(
            "--output-format jsonl / --jsonl is only supported in headless mode".to_string(),
        );
    }

    if let CliSurface::Serve { ref listen_addr } = surface {
        validate_daemon_listen_addr(listen_addr, auth_token.as_deref())?;
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

    // `--workspace` defaults to the session WAL's recorded workspace when resuming, or the
    // current working directory (`.`) when not explicitly specified.
    let workspace = match workspace {
        Some(w) => w,
        None => match resume.as_ref() {
            Some(path) => match SessionJournal::peek_workspace_root(path) {
                Ok(ws_root) => ws_root,
                Err(e) => {
                    return Err(format!(
                        "failed to read session WAL {} to infer workspace: {}",
                        path.display(),
                        e
                    ))
                }
            },
            None => std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        },
    };

    let policy_mode = PolicyMode::from_flags(policy_flag.as_deref(), allow_write, allow_shell)?;
    if no_subagents {
        // BUG-09 (`v1.5.1`): `--no-subagents` is an explicit operator capability disable, so it
        // must outrank any workspace-controlled `max_subagents` (a key an untrusted repo may set in
        // `.kroma.toml`). Marking it explicit routes it through `ConfigOverrides` instead of
        // falling back to `workspace.max_subagents` during `ResolvedConfig::resolve`.
        max_subagents = 0;
        max_subagents_explicit = true;
    }

    Ok(CliArgs {
        surface,
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
        undo,
        restore_rev,
        force_restore,
        list_revisions,
        list_skills,
        skill,
        policy_hook,
        observer_hook,
        no_subtree_context,
        no_predictive_check,
        predict_cmd,
        max_subagents,
        counterfactual_probe,
        tournament_require_verified,
        on_demand_tools,
        causal_pruning,
        multi_session,
        mcp_servers,
        max_tokens,
        output_format,
        auth_token,
        protocol,
        profile,
        protocol_explicit,
        max_subagents_explicit,
    })
}

/// Build the CLI-override layer from parsed arguments. Precedence is CLI flags > `KROMA_*` env vars
/// > workspace `.kroma.toml` (benign keys only) > `~/.kroma/config.toml`.
///
/// BUG-09 (`v1.5.1`): `--no-subagents` marks [`CliArgs::max_subagents_explicit`], so its explicit
/// `0` is carried here as an override and cannot be outranked by a workspace-controlled
/// `max_subagents`.
fn config_overrides_for(cli: &CliArgs) -> ConfigOverrides {
    ConfigOverrides {
        profile: cli.profile.clone(),
        model: None,
        base_url: None,
        api_key: None,
        protocol: if cli.protocol_explicit {
            Some(cli.protocol.clone())
        } else {
            None
        },
        reasoning_effort: cli.reasoning_effort.clone(),
        verify_command: cli.verify_cmd.clone(),
        max_subagents: if cli.max_subagents_explicit {
            Some(cli.max_subagents)
        } else {
            None
        },
        // F05 (`v1.5.1`): `--max-tokens` is the CLI layer of the unified token cap.
        max_tokens: cli.max_tokens,
        // B2 (`v1.4.0`): a `--projection-profile` CLI flag is not exposed yet; the value comes from
        // `config.toml` / `.kroma.toml` / `KROMA_PROJECTION_PROFILE` / `/config set`.
        projection_profile: None,
        // Item #3 (`v1.4.0`): adaptive tournament escalation comes from config, not a CLI flag.
        adaptive_tournament: None,
        // Capability gap #6 (`v1.5.2`): explicit trusted MCP servers.
        mcp_servers: cli.mcp_servers.clone(),
    }
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
                match SessionJournal::peek_workspace_root(&path) {
                    Ok(ws_root) if ws_root == ws => {}
                    _ => continue,
                }
            } else if SessionJournal::peek_workspace_root(&path).is_err() {
                continue;
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

    // A bare session id / filename resolves against the session store (reject any path traversal).
    if !raw.components().any(|c| {
        matches!(
            c,
            std::path::Component::ParentDir | std::path::Component::RootDir
        )
    }) {
        let direct = sessions_dir.join(raw);
        if direct.exists() {
            validate_resume_path_candidate(&direct)?;
            return Ok(direct);
        }
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
    if meta.len() > kroma_core::session::MAX_WAL_FILE_BYTES {
        return Err(format!(
            "--resume path {} is {} bytes, exceeding the {} byte WAL cap",
            path.display(),
            meta.len(),
            kroma_core::session::MAX_WAL_FILE_BYTES
        ));
    }
    Ok(())
}

/// How the CompletionGate should be constructed for this run.
#[derive(Debug, PartialEq, Eq)]
pub enum CompletionGateChoice {
    /// `--no-verify` or a read-only policy: never spawn a verification process.
    Disabled,
    /// An explicitly authorized command from `--verify-cmd` or `KROMA_VERIFY_COMMAND`.
    Explicit(Vec<String>),
    /// Auto-detect from project manifests; `allow_auto` mirrors `--allow-shell` authorization.
    AutoDetect { allow_auto: bool },
}

/// Decide how the CompletionGate should run without spawning anything. Auto-detected project test
/// scripts are untrusted code, so they are only run when `--allow-shell` was explicitly granted;
/// an explicit `--verify-cmd` / `KROMA_VERIFY_COMMAND` is itself the operator's authorization.
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
    Ok(CompletionGateChoice::AutoDetect {
        allow_auto: cli.policy_mode.allows_shell_auto(),
    })
}

fn build_completion_gate(choice: CompletionGateChoice) -> SharedCompletionGate {
    match choice {
        CompletionGateChoice::Disabled => Arc::new(AllowAllGate),
        CompletionGateChoice::Explicit(argv) => Arc::new(WorkspaceCompletionGate::new(
            Some(argv),
            configured_timeout(),
        )),
        CompletionGateChoice::AutoDetect { allow_auto } => Arc::new(
            WorkspaceCompletionGate::with_auto_detection(None, configured_timeout(), allow_auto),
        ),
    }
}

/// Build the runtime configuration shared by every session a surface hosts.
///
/// The initial dispatcher and each additional session opened by the multi-session daemon go through
/// this one function, so a session can never get a different policy, budget, gate or tool filter
/// than the daemon's startup session.
#[allow(clippy::too_many_arguments)]
fn build_surface_runtime_config(
    cli: &CliArgs,
    resolved_config: &ResolvedConfig,
    budget: BudgetConfig,
    gate: SharedCompletionGate,
    max_gate_retries: usize,
    surface_name: &str,
    transport_name: &str,
    workspace_root: PathBuf,
    state_dir: PathBuf,
    resume_session_path: Option<PathBuf>,
    provider_factory: Arc<dyn kroma_core::surface::ProviderFactory>,
) -> Result<KromaRuntimeConfig, String> {
    let mut rt_cfg = KromaRuntimeConfig::new(workspace_root, state_dir, cli.policy_mode)
        .with_surface(surface_name, transport_name)
        // F06: the `acp` stdio surface speaks the official Agent Client Protocol v1.
        .with_acp_protocol(matches!(cli.surface, CliSurface::Acp))
        .with_auth_token(cli.auth_token.clone())
        .with_provider_factory(provider_factory)
        .with_resolved_config(resolved_config.clone())
        .with_completion_gate(gate);
    rt_cfg.budget = budget;
    rt_cfg.stream = !cli.no_stream;
    rt_cfg.strict_log = cli.strict_log;
    rt_cfg.quiet = cli.quiet;
    rt_cfg.tool_filter = cli.tool_filter.clone();
    rt_cfg.resume_session_path = resume_session_path;
    rt_cfg.fork_turn = cli.fork_turn;
    rt_cfg.completion_gate_max_retries = max_gate_retries;
    rt_cfg.predictive_check = !cli.no_predictive_check;
    rt_cfg.subtree_context_enabled = !cli.no_subtree_context;
    rt_cfg.max_subagents = resolved_config.max_subagents.unwrap_or(cli.max_subagents);
    // Item #3 (`v1.4.0`): adaptive tournament escalation candidate count.
    rt_cfg.adaptive_tournament_candidates = resolved_config.adaptive_tournament.unwrap_or(0);
    // Capability gaps #2/#3/#5/#6 (`v1.5.2`): frontier opt-ins.
    rt_cfg.counterfactual_probe = cli.counterfactual_probe;
    rt_cfg.tournament_require_verified = cli.tournament_require_verified;
    rt_cfg.on_demand_tools = cli.on_demand_tools;
    rt_cfg.causal_pruning = cli.causal_pruning;
    rt_cfg.initial_skill = cli.skill.clone();
    if let Some(raw_pred) = cli.predict_cmd.as_deref() {
        if cli.policy_mode != PolicyMode::ReadOnly && !cli.no_predictive_check {
            let argv = parse_command_argv(raw_pred)?;
            if argv.is_empty() {
                return Err("empty value for --predict-cmd".to_string());
            }
            rt_cfg.predict_cmd = Some(Arc::new(WorkspaceCompletionGate::with_auto_detection(
                Some(argv),
                configured_timeout(),
                false,
            )));
        }
    }
    Ok(rt_cfg)
}

/// Capability gap #6 (`v1.5.2`): mount trusted MCP servers into a headless tool registry.
///
/// Uses the exact same core loader as the `RpcDispatcher`, so the headless `kroma run` path and the
/// REPL/ACP/serve surfaces cannot disagree about whether `--mcp-server` / `[mcp.<name>]` is loaded.
fn mount_trusted_mcp(
    registry: &mut ToolRegistry,
    servers: &[kroma_core::config_file::McpServerConfig],
    quiet: bool,
) -> Result<Option<usize>, String> {
    match kroma_core::mcp::trusted_mcp_extension(servers) {
        Ok(Some((ext, tool_count))) => {
            registry
                .register_extension(ext)
                .map_err(|e| format!("failed to mount MCP tools: {}", e))?;
            if !quiet {
                eprintln!(
                    "[kroma] mounted {} MCP tool(s) from {} trusted server(s)",
                    tool_count,
                    servers.len()
                );
            }
            Ok(Some(tool_count))
        }
        Ok(None) => Ok(None),
        Err(e) => Err(format!("failed to start a configured MCP server: {}", e)),
    }
}

/// Exit code returned when a restore would overwrite work done outside the kroma.
const EXIT_RESTORE_CONFLICT: u8 = 3;
/// Exit code returned when an interrupted restore left the workspace needing recovery.
const EXIT_RESTORE_RECOVERY_REQUIRED: u8 = 4;

/// Append the Gate 1 `RevisionRestored` audit event to the run log for a completed restore (O6).
fn emit_revision_restored(
    state_dir: &Path,
    strict_log: bool,
    revision_id: &RevisionId,
    restored_files: usize,
    removed_files: usize,
) {
    let run_id = kroma_core::types::RunId::new();
    let Ok(mut logger) = EventLogger::new(run_id, Some(state_dir.to_path_buf()), strict_log) else {
        return;
    };
    let _ = logger.log(EventPayload::RevisionRestored {
        revision_id: revision_id.as_str().to_string(),
        restored_files,
        removed_files,
    });
}

/// Reconcile a snapshot manifest that was durably written immediately before a crash that stopped
/// the WAL `RevisionCaptured` append (O6). Without this the WAL would describe an older workspace
/// state and a following `--undo` would misreport the live files as external modifications.
///
/// Delegates to [`SnapshotStore::reconcile_with_wal`] so the CLI and the kroma-core acceptance
/// tests exercise exactly the same production reconciliation path.
///
/// The error is returned, not swallowed: a failed reconciliation means the session WAL may not
/// describe the recorded workspace history, so the caller must refuse to start a mutation on top
/// of it (F10).
fn reconcile_store_with_wal(
    workspace: &WorkspaceCap,
    journal: &mut SessionJournal,
    store: &SnapshotStore,
) -> Result<(), kroma_core::revision::RevisionError> {
    use kroma_core::revision::ForkRecoveryOutcome;
    store.reconcile_with_wal(journal).map(|_| ())?;
    // F03 follow-up (P1-2): an interrupted workspace-bound fork must be finished (or safely
    // abandoned) before a mutation runs on top of it, so the leaf and the restored files converge.
    if let ForkRecoveryOutcome::Abandoned {
        forked_at_turn,
        target_revision,
        conflicting_paths,
    } = store.recover_pending_fork(workspace, journal)?
    {
        eprintln!(
            "[kroma] abandoned an interrupted fork to turn {} (revision {}): {} path(s) changed on disk since the fork began",
            forked_at_turn,
            target_revision,
            conflicting_paths.len()
        );
    }
    Ok(())
}

fn snapshot_store_for(state_dir: &Path) -> SnapshotStore {
    SnapshotStore::new(state_dir.join("snapshots"))
}

/// F03: apply `--fork <TURN>` to a resumed session the same way `session.fork` does over RPC —
/// restore the workspace to the revision current at that turn, then move the session leaf. Returns
/// a descriptive error instead of touching the workspace when an external edit or an interrupted
/// restore makes the fork unsafe.
fn fork_resumed_session(
    store: &SnapshotStore,
    workspace: &WorkspaceCap,
    journal: &mut SessionJournal,
    turn: usize,
) -> Result<kroma_core::revision::WorkspaceBoundForkOutcome, String> {
    store
        .fork_workspace_at_turn(workspace, journal, turn, false)
        .map_err(|e| e.to_string())
}

/// Gate 3: `--list-skills` discovers the workspace skill catalog and exits without a provider call.
/// Item #3 (`v1.4.0`) / gap-1 (`v1.5.2`): `kroma bench [--suite <PATH>] [--executor <synthetic|provider>]
/// [--budget-tokens <N>] [--repetitions <N>] [--seed <N>] [--json]`.
///
/// Runs a paired, budget-locked A/B comparison. With no `--suite`, the built-in smoke suite and the
/// deterministic synthetic executor are used so the command works out of the box; the report marks
/// those numbers as `synthetic`. `--executor provider` runs the real `AgentRunner` against the
/// resolved provider configuration; it prints the model/protocol/endpoint origin but never the
/// API key, and every arm runs in its own isolated scratch directory under the system temp dir.
async fn run_bench(cli: &CliArgs) -> ExitCode {
    use kroma_core::bench::{
        default_strategy_pair, load_held_out_suite, run_paired_benchmark, BenchExecutor,
        BenchRunOptions, SyntheticBenchExecutor, SYNTHETIC_EXECUTOR_LABEL,
    };

    let CliSurface::Bench {
        suite,
        budget_tokens,
        json,
        executor,
        repetitions,
        seed,
    } = &cli.surface
    else {
        eprintln!("error: internal: run_bench called for a non-bench surface");
        return ExitCode::from(2);
    };

    let (suite_name, tasks) = match suite {
        // A suite inside the workspace under test would be editable by the graded agent, so the
        // held-out loader refuses that location instead of silently grading a rewritten oracle.
        Some(path) => match load_held_out_suite(path, &cli.workspace) {
            Ok(tasks) => (
                path.file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("suite")
                    .to_string(),
                tasks,
            ),
            Err(e) => {
                eprintln!("error: {}", e);
                return ExitCode::from(2);
            }
        },
        None => (
            "builtin-smoke".to_string(),
            kroma_core::bench::builtin_smoke_suite(),
        ),
    };
    if tasks.is_empty() {
        eprintln!("error: benchmark suite is empty");
        return ExitCode::from(2);
    }

    let options = BenchRunOptions::new(*repetitions, *seed, true);
    let executor_impl: Box<dyn BenchExecutor> = match executor {
        BenchExecutorKind::Synthetic => Box::new(SyntheticBenchExecutor),
        BenchExecutorKind::Provider => match build_provider_bench_executor(cli).await {
            Ok(executor) => Box::new(executor),
            Err(e) => {
                eprintln!("error: {}", e);
                return ExitCode::from(2);
            }
        },
    };

    // The real executor drives an agent that can read and write its whole workspace, so its scratch
    // tree lives outside the operator's repository. The synthetic executor only writes the files a
    // task declared, and keeps the historical in-workspace location.
    let scratch = match executor {
        BenchExecutorKind::Synthetic => cli
            .workspace
            .join(".kroma-bench")
            .join(format!("run-{}", std::process::id())),
        BenchExecutorKind::Provider => std::env::temp_dir().join(format!(
            "kroma-bench-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        )),
    };

    let report = run_paired_benchmark(
        &suite_name,
        &tasks,
        &default_strategy_pair(),
        *budget_tokens,
        &scratch,
        executor_impl.as_ref(),
        &options,
    )
    .await;
    let _ = std::fs::remove_dir_all(&scratch);

    if *json {
        match serde_json::to_string_pretty(&report) {
            Ok(s) => println!("{}", s),
            Err(e) => {
                eprintln!("error: failed to serialize bench report: {}", e);
                return ExitCode::from(2);
            }
        }
    } else {
        print!("{}", report.render());
    }
    if !cli.quiet && report.executor == SYNTHETIC_EXECUTOR_LABEL {
        eprintln!(
            "[kroma] note: built-in smoke suite uses the synthetic executor (plumbing check only)"
        );
    }
    ExitCode::SUCCESS
}

/// Build the real, provider-backed bench executor from the same resolved configuration a normal
/// run uses. Returns an operator-facing error (exit 2) when no credential is configured; the API
/// key itself is never printed, serialized or attached to the report.
///
/// The provenance commit is read through the bounded one-shot supervisor, so this is `async`.
async fn build_provider_bench_executor(
    cli: &CliArgs,
) -> Result<kroma_core::bench::ProviderBenchExecutor, String> {
    use kroma_core::bench::{
        detect_git_commit, provider_origin_of, ProviderBenchExecutor, ProviderBenchSettings,
    };

    let overrides = config_overrides_for(cli);
    let resolved = ResolvedConfig::load(&cli.workspace, &overrides)?;
    let api_key = resolved
        .api_key
        .clone()
        .map(|k| k.trim().to_string())
        .filter(|k| !k.is_empty())
        .ok_or_else(|| {
            "`kroma bench --executor provider` requires an API key; set KROMA_API_KEY, add api_key \
             to ~/.kroma/config.toml, or pass --profile"
                .to_string()
        })?;
    let base_url = resolved
        .base_url
        .clone()
        .unwrap_or_else(|| "https://api.openai.com".to_string());
    let model = resolved
        .model
        .clone()
        .unwrap_or_else(|| "gpt-4o".to_string());
    let protocol = match resolved.protocol.as_deref() {
        Some(raw) if !raw.trim().is_empty() => ProviderProtocol::parse(raw)
            .map_err(|e| format!("invalid protocol '{}': {}", raw, e))?,
        _ => ProviderProtocol::Auto,
    };

    let provider_settings = ProviderSettings {
        base_url: base_url.clone(),
        api_key,
        model: model.clone(),
        max_retries: 2,
        reasoning_effort: resolved.reasoning_effort.clone(),
        protocol,
        max_tokens: 0,
    };
    let (resolved_protocol, provider) = build_provider(&provider_settings)
        .map_err(|e| format!("invalid provider configuration: {}", e))?;

    if !cli.quiet {
        eprintln!(
            "[kroma] bench executor: provider model={} protocol={}",
            model,
            resolved_protocol.as_str()
        );
        if let Some(origin) = provider_origin_of(&base_url) {
            eprintln!("[kroma] bench endpoint: {}", origin);
        }
        if cli.policy_mode == PolicyMode::Ask {
            eprintln!(
                "[kroma] note: policy 'ask' is non-interactive during `kroma bench`; file writes \
                 will be denied. Pass --allow-write (or --policy allow-write) to let the agent edit \
                 the isolated task workspace."
            );
        }
        if !cli.policy_mode.allows_shell_auto() {
            eprintln!(
                "[kroma] note: bash is not enabled for this bench run (policy '{}'); every arm \
                 keeps the same file tools, so a task only resolvable with a shell needs \
                 --allow-shell.",
                cli.policy_mode.as_str()
            );
        }
    }

    let bench_settings = ProviderBenchSettings::new(model, resolved_protocol.as_str().to_string())
        .with_provider_origin(provider_origin_of(&base_url))
        .with_commit(
            detect_git_commit(&std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
                .await,
        )
        .with_policy_mode(cli.policy_mode)
        .with_max_subagents(resolved.max_subagents.unwrap_or(cli.max_subagents));
    // Bench arms need the same development gate as a normal run or adaptive escalation can never
    // observe verification failures. The task's held-out final scorer remains entirely separate.
    let gate_choice = choose_completion_gate(cli, resolved.verify_command.clone())?;
    Ok(ProviderBenchExecutor::new(provider, bench_settings)
        .with_development_gate(build_completion_gate(gate_choice)))
}

fn run_list_skills(cli: &CliArgs) -> ExitCode {
    let workspace = match WorkspaceCap::new(&cli.workspace) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("error: failed to initialize workspace capability: {}", e);
            return ExitCode::from(2);
        }
    };
    let skills = discover_skills(&workspace);
    if skills.is_empty() {
        println!(
            "no skills found; add a definition at {}/<name>/SKILL.md",
            kroma_core::extensions::SKILL_DIR
        );
        return ExitCode::SUCCESS;
    }
    println!(
        "skills for {} ({} discovered):",
        cli.workspace.display(),
        skills.len()
    );
    for skill in &skills {
        println!("  {}\n      path: {}", skill.name, skill.path);
        if !skill.description.is_empty() {
            println!("      {}", skill.description);
        }
    }
    ExitCode::SUCCESS
}

/// Gate 3: build the command-backed hooks declared on the CLI as a single operator-trusted
/// extension. Mounting them through the same registry as any other extension means they are
/// removed by exactly the same `unregister_extension` path.
fn build_cli_extension(cli: &CliArgs) -> Result<Option<ScopedExtension>, String> {
    if cli.policy_hook.is_none() && cli.observer_hook.is_none() {
        return Ok(None);
    }
    let mut manifest =
        ExtensionManifest::operator_trusted("cli-hooks", "--policy-hook/--observer-hook");
    let mut ext = ScopedExtension::new(manifest.clone());
    if let Some(raw) = cli.policy_hook.as_deref() {
        let argv = parse_command_argv(raw)?;
        if argv.is_empty() {
            return Err("empty --policy-hook command".to_string());
        }
        manifest.capabilities.push("policy_hook".to_string());
        ext.policy_hooks
            .push(Arc::new(CommandPolicyHook::new("cli-policy-hook", argv)));
    }
    if let Some(raw) = cli.observer_hook.as_deref() {
        let argv = parse_command_argv(raw)?;
        if argv.is_empty() {
            return Err("empty --observer-hook command".to_string());
        }
        manifest.capabilities.push("observer_hook".to_string());
        ext.observer_hooks.push(Arc::new(CommandObserverHook::new(
            "cli-observer-hook",
            argv,
        )));
    }
    ext.manifest = manifest;
    Ok(Some(ext))
}

/// Bind a completed `--restore-rev` into the session WAL so `latest_revision_id()` and
/// `--list-revisions` describe the workspace that is actually on disk. Without this, a later
/// `--undo` would treat the freshly restored files as external modifications and refuse.
fn record_restored_revision(
    journal: &mut SessionJournal,
    store: &SnapshotStore,
    restored_revision: &RevisionId,
    restored_files: &[String],
    removed_files: &[String],
    expected: Option<&RevisionId>,
) -> Result<(), String> {
    let (changed_paths, tracked_file_count) = match store.load_revision(restored_revision) {
        Ok(manifest) => (manifest.changed_paths.clone(), manifest.tracked_files.len()),
        Err(_) => {
            let mut paths: Vec<String> = restored_files
                .iter()
                .chain(removed_files.iter())
                .cloned()
                .collect();
            paths.sort();
            paths.dedup();
            (paths, restored_files.len())
        }
    };
    let turn = journal.max_active_turn();
    journal
        .record_revision_capture(
            turn,
            restored_revision.as_str(),
            expected.map(|e| e.as_str()),
            changed_paths,
            tracked_file_count,
        )
        .map_err(|e| e.to_string())
}

/// Execute `--list-revisions` / `--undo` / `--restore-rev` without contacting a model provider.
fn run_revision_management(cli: &CliArgs, state_dir: &Path) -> ExitCode {
    let Some(session_path) = cli.resume.as_ref() else {
        eprintln!("error: --undo, --restore-rev and --list-revisions require --resume <SESSION_WAL|latest>");
        return ExitCode::from(2);
    };
    let mut journal = match SessionJournal::open(session_path) {
        Ok(j) => j,
        Err(e) => {
            eprintln!("error: failed to open session WAL: {}", e);
            return ExitCode::from(2);
        }
    };
    let workspace = match WorkspaceCap::new(&cli.workspace) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("error: failed to initialize workspace capability: {}", e);
            return ExitCode::from(2);
        }
    };
    let store = snapshot_store_for(state_dir);
    // O6: a crash between the snapshot write and the WAL append leaves the store one revision ahead
    // of the WAL; reconcile before reading `latest_revision_id()` so the two agree. F10: a failed
    // reconciliation aborts the management operation instead of acting on a possibly inconsistent
    // WAL/snapshot pair.
    if let Err(e) = reconcile_store_with_wal(&workspace, &mut journal, &store) {
        eprintln!(
            "error: failed to reconcile session WAL with the snapshot store: {}",
            e
        );
        return ExitCode::from(EXIT_RESTORE_RECOVERY_REQUIRED);
    }

    if cli.list_revisions {
        let records = journal.revision_records();
        if records.is_empty() {
            println!("no workspace revisions recorded for this session");
            return ExitCode::SUCCESS;
        }
        let current = journal.latest_revision_id();
        println!(
            "workspace revisions for session {} ({} captured):",
            journal.session_id(),
            records.len()
        );
        // Only the newest record whose revision matches the live workspace is "current"; an older
        // record can carry the same content address (e.g. after `--restore-rev` re-records it).
        let current_index = records
            .iter()
            .rposition(|record| Some(&record.revision_id) == current.as_ref());
        for (index, record) in records.iter().enumerate() {
            let marker = if Some(index) == current_index {
                "  <- current"
            } else {
                ""
            };
            println!(
                "  turn {:>4}  {}  files={} changed={}{}",
                record.turn,
                record.revision_id,
                record.tracked_file_count,
                record.changed_paths.len(),
                marker
            );
            for path in &record.changed_paths {
                println!("        {}", path);
            }
        }
        println!("snapshot store: {}", store.root().display());
        return ExitCode::SUCCESS;
    }

    let (target, expected, rewind_turn) = if cli.undo {
        let records = journal.revision_records();
        let Some(current) = records.last() else {
            eprintln!("error: no workspace revision recorded for this session");
            return ExitCode::from(2);
        };
        // Skip any trailing capture that describes the same workspace content, so `--undo` means
        // "go back to the last revision the workspace actually differed from".
        let Some(previous) = records
            .iter()
            .rev()
            .find(|record| record.revision_id != current.revision_id)
        else {
            eprintln!(
                "error: nothing to undo; the workspace is unchanged since revision {}",
                current.revision_id
            );
            return ExitCode::from(2);
        };
        match (
            RevisionId::parse(&previous.revision_id),
            RevisionId::parse(&current.revision_id),
        ) {
            (Ok(target), Ok(expected)) => (target, Some(expected), Some(previous.turn)),
            (Err(e), _) | (_, Err(e)) => {
                eprintln!("error: {}", e);
                return ExitCode::from(2);
            }
        }
    } else {
        let raw = cli.restore_rev.as_deref().unwrap_or_default();
        let target = match RevisionId::parse(raw) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("error: {}", e);
                return ExitCode::from(2);
            }
        };
        let expected = journal
            .latest_revision_id()
            .and_then(|id| RevisionId::parse(&id).ok());
        (target, expected, None)
    };

    // F8: an interrupted worktree→parent merge leaves the parent workspace half-updated and a merge
    // intent journal behind. `--undo` / `--restore-rev` must heal it first: restoring on top of a
    // half-merged tree (especially with `--force-restore`) would otherwise leave the merge journal
    // to be replayed on the next normal startup, clobbering the revision the user just restored and
    // deleting merge-created paths the restored revision legitimately contains.
    match store.recover_pending_merge(&workspace) {
        Ok(MergeRecoveryOutcome::NothingPending) => {}
        Ok(MergeRecoveryOutcome::RolledBack {
            pre_merge_revision, ..
        }) => {
            println!(
                "recovered an interrupted subagent merge; rolled the workspace back to {}",
                pre_merge_revision
            );
        }
        Ok(MergeRecoveryOutcome::AlreadyMerged { merged_revision }) => {
            println!(
                "cleared a stale subagent merge journal; revision {} had already been merged",
                merged_revision
            );
        }
        Err(e) => {
            eprintln!(
                "error: failed to recover an interrupted subagent merge: {}",
                e
            );
            return ExitCode::from(EXIT_RESTORE_RECOVERY_REQUIRED);
        }
    }

    // An interrupted restore leaves its intent journal behind. Finishing that restore is the only
    // safe action: applying a *different* target on top of a half-applied tree would compound it.
    //
    // `--undo` restores the workspace *and* rewinds the session leaf. That pair is made
    // crash-atomic by the same durable fork intent `--fork` uses: a crash between the restore and
    // the leaf move is completed by `recover_pending_fork` on the next start instead of leaving the
    // WAL naming a revision the workspace is no longer on (which would make the retry refuse with a
    // misleading "changed outside the kroma" conflict).
    let mut rewound_in_store = false;
    let result = match store.pending_restore() {
        Ok(Some(intent)) if intent.target_revision == target => {
            println!(
                "resuming an interrupted restore of {} from {}",
                target,
                store.intent_path().display()
            );
            store.recover_pending_restore(&workspace)
        }
        Ok(Some(intent)) => {
            eprintln!(
                "error: an interrupted restore of {} must be completed before restoring {}",
                intent.target_revision, target
            );
            eprintln!("  intent journal: {}", store.intent_path().display());
            eprintln!(
                "  re-run with --restore-rev {} to finish it",
                intent.target_revision
            );
            return ExitCode::from(EXIT_RESTORE_RECOVERY_REQUIRED);
        }
        Ok(None) => match rewind_turn {
            Some(turn) => match store.restore_and_rewind_workspace(
                &workspace,
                &mut journal,
                &target,
                turn,
                cli.force_restore,
            ) {
                Ok(outcome) => {
                    rewound_in_store = true;
                    Ok(RestoreResult::Applied {
                        restored_revision: outcome.target_revision,
                        restored_files: outcome.restored_files,
                        removed_files: outcome.removed_files,
                    })
                }
                Err(WorkspaceBoundForkError::Conflict {
                    target_revision,
                    conflicting_paths,
                    ..
                }) => Ok(RestoreResult::Conflict {
                    target_revision,
                    conflicting_paths,
                }),
                Err(WorkspaceBoundForkError::RecoveryRequired {
                    target_revision,
                    journal_path,
                    reason,
                    ..
                }) => Ok(RestoreResult::RecoveryRequired {
                    target_revision,
                    journal_path,
                    reason,
                }),
                Err(e) => Err(kroma_core::revision::RevisionError::InvalidStore(format!(
                    "failed to restore and rewind the session: {}",
                    e
                ))),
            },
            None => {
                store.restore_revision(&workspace, &target, expected.as_ref(), cli.force_restore)
            }
        },
        Err(e) => Err(e),
    };

    let result = match result {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: restore failed: {}", e);
            return ExitCode::from(2);
        }
    };

    match result {
        RestoreResult::Applied {
            restored_revision,
            restored_files,
            removed_files,
        } => {
            println!(
                "restored workspace to {} ({} file(s) written, {} removed)",
                restored_revision,
                restored_files.len(),
                removed_files.len()
            );
            for path in &restored_files {
                println!("  restored {}", path);
            }
            for path in &removed_files {
                println!("  removed  {}", path);
            }
            emit_revision_restored(
                state_dir,
                cli.strict_log,
                &restored_revision,
                restored_files.len(),
                removed_files.len(),
            );
            if let Some(turn) = rewind_turn.filter(|_| !rewound_in_store) {
                match journal.fork_at_turn(turn) {
                    Ok(leaf) => println!(
                        "rewound session transcript to turn {} (leaf {})",
                        turn, leaf
                    ),
                    Err(e) => {
                        eprintln!(
                            "error: workspace restored but the session leaf could not be rewound: {}",
                            e
                        );
                        return ExitCode::from(1);
                    }
                }
                // The rewound leaf can be the very leaf the *undone* revision was bound to (that
                // happens whenever `current` was recorded at a turn <= `previous.turn`, e.g. after
                // a `--restore-rev` or a `--resume` baseline capture). Then `revision_records()`
                // still ends with `current` and `latest_revision_id()` would keep naming the undone
                // revision, making the next `--undo` / `--restore-rev` fail with a false
                // `ModifiedExternally` conflict. Re-bind the restored revision so the WAL always
                // describes the workspace that is actually on disk.
                if journal.latest_revision_id().as_deref() != Some(restored_revision.as_str()) {
                    if let Err(e) = record_restored_revision(
                        &mut journal,
                        &store,
                        &restored_revision,
                        &restored_files,
                        &removed_files,
                        expected.as_ref(),
                    ) {
                        eprintln!(
                            "error: workspace restored to {} but the session WAL could not record it: {}",
                            restored_revision, e
                        );
                        return ExitCode::from(1);
                    }
                }
            } else if rewind_turn.is_none() {
                if let Err(e) = record_restored_revision(
                    &mut journal,
                    &store,
                    &restored_revision,
                    &restored_files,
                    &removed_files,
                    expected.as_ref(),
                ) {
                    eprintln!(
                        "error: workspace restored to {} but the session WAL could not record it: {}",
                        restored_revision, e
                    );
                    return ExitCode::from(1);
                }
            }
            ExitCode::SUCCESS
        }
        RestoreResult::Conflict {
            target_revision,
            conflicting_paths,
        } => {
            eprintln!(
                "error: refusing to restore {}; {} path(s) changed outside the kroma (pass --force-restore to overwrite)",
                target_revision,
                conflicting_paths.len()
            );
            for conflict in &conflicting_paths {
                eprintln!("  {:?} {}", conflict.kind, conflict.path);
            }
            ExitCode::from(EXIT_RESTORE_CONFLICT)
        }
        RestoreResult::RecoveryRequired {
            target_revision,
            journal_path,
            reason,
        } => {
            eprintln!(
                "error: restore of {} did not complete; the workspace needs recovery",
                target_revision
            );
            eprintln!("  intent journal: {}", journal_path.display());
            eprintln!("  reason: {}", reason);
            eprintln!(
                "  re-run with --restore-rev {} to finish the restore",
                target_revision
            );
            ExitCode::from(EXIT_RESTORE_RECOVERY_REQUIRED)
        }
    }
}

pub fn usage_text() -> String {
    "Usage: kroma [OPTIONS]                                  (launch interactive TUI/REPL in current dir)\n\
       kroma --task <TASK> [--workspace <DIR>] [OPTIONS]\n\
       kroma run [--workspace <DIR>] --task <TASK> [OPTIONS]\n\
       kroma repl [--workspace <DIR>] [OPTIONS]\n\
       kroma acp [--workspace <DIR>] [OPTIONS]\n\
       kroma serve [--workspace <DIR>] [--listen 127.0.0.1:<PORT>] [--auth-token <TOKEN>] [OPTIONS]\n\
       kroma bench [--suite <PATH>] [--executor <synthetic|provider>] [--budget-tokens <N>]\n\
         [--repetitions <N>] [--seed <N>] [--json]\n\n\
Surfaces (Gate 6):\n\
  (default with no args)    Interactive multi-turn terminal REPL/TUI in current directory\n\
  run                       Headless single-run CLI mode\n\
  repl | tui | --interactive Interactive multi-turn terminal REPL/TUI\n\
  acp | --acp               JSON-RPC 2.0 Agent Client Protocol server over stdio\n\
  serve | --serve <ADDR>    JSON-RPC 2.0 daemon over loopback TCP / Unix socket (or authenticated TCP)\n\
  --listen <ADDR>           Listen endpoint for `kroma serve` (default: 127.0.0.1:7878)\n\
  --auth-token <TOKEN>      Required when binding `kroma serve` to a non-loopback address.\n\
                            Connection is plaintext (no TLS) and auth token travels in\n\
                            JSON-RPC parameters, so it must only be used on a trusted network\n\
  --output-format <FMT>     Headless output format: text | jsonl (default: text)\n\
  --jsonl                   Shorthand for --output-format jsonl\n\
  bench                     Paired, budget-locked A/B benchmark (built-in smoke suite by default)\n\
  --suite <PATH>            Benchmark suite JSON for `kroma bench`\n\
  --executor <KIND>         `synthetic` (default, plumbing only) | `provider` (real AgentRunner run)\n\
  --budget-tokens <N>       Shared per-arm run-tree token budget for `kroma bench`\n\
  --repetitions <N>         Repeat each paired comparison N times (default: 1)\n\
  --seed <N>                Seed for the randomized arm order and the bootstrap intervals\n\
  --json                    Emit the `kroma bench` report as JSON\n\n\
Options:\n\
  --workspace <DIR>         Target repository workspace directory (default: current directory `.`)\n\
  --task <TASK>             Coding or analysis task description"
        .to_string()
        + "
  --policy <MODE>           Host policy: read-only | ask (default: ask)
  --allow-write             Auto-allow workspace file mutations (edit/write)
  --allow-shell             Auto-allow supervised shell commands (bash)
  --reasoning-effort <LVL>  Reasoning effort level (e.g. off, low, medium, high, xhigh)
  --protocol <NAME>         Wire protocol: auto | chat-completions | anthropic-messages | openai-responses
  --profile <NAME>          Activate a [profiles.<name>] entry from ~/.kroma/config.toml
  --quiet, -q               Suppress live progress output on stderr
  --no-stream               Fallback to non-streaming JSON requests
  --strict-log              Fail-closed if diagnostic JSONL event logging fails
  --resume <WAL|latest>     Resume a session WAL, session id, or the latest session (latest)
  --fork <TURN>             Branch from a prior turn (requires --resume)
  --verify-cmd <CMD>        Explicit completion-check command (overrides KROMA_VERIFY_COMMAND)
  --no-verify               Disable the completion verification gate entirely
  --tool-filter <CSV>       Benchmarking ablation filter (e.g. read,edit,write,bash)

Extensions / Hooks / Skills (Gate 3):
  --list-skills             Discover and print .agents/skills/*/SKILL.md, then exit
  --skill <NAME>            Pre-activate a named skill in the initial untrusted context
  --policy-hook <CMD>       Fail-closed PreToolUse policy hook (non-zero exit or timeout denies)
  --observer-hook <CMD>     Fail-open PostToolUse observer hook (failures are logged, never block)
  --no-subtree-context      Disable automatic JIT subtree AGENTS.md/CLAUDE.md loading

Predictive Validation / Attribution (Gate 4):
  --no-predictive-check     Disable the post-edit/write structural (syntax/delimiter) check
  --predict-cmd <CMD>       Fast command run after a mutating batch to surface failures early

Controlled Subagents / Isolated Worktrees (Gate 5):
  --max-subagents <N>       Maximum subagent spawns per session (default: 8, 0 disables)
  --max-tokens <N>          Shared run-tree token cap (input + output) for the run and its subagents
  --no-subagents            Disable subagent spawning (max_subagents = 0)

Frontier mechanisms (v1.5.2, all opt-in):
  --counterfactual-probe    On repeated verification failure, probe top candidate(s) in an isolated
                            worktree (bounded, approved; never rolls back your workspace)
  --allow-unverified-tournament
                            Let a repair race merge a candidate whose verification was skipped
                            (strict verification is the default)
  --on-demand-tools         Pin only core tool schemas; fetch the rest via `discover_tools`
  --causal-pruning          Replace abandoned-branch tool output with evidence-linked dead-end
                            stubs so it does not crowd the context window
  --multi-session           `kroma serve`: host several sessions in one daemon and route each
                            request to the runtime owning the session named in it
  --mcp-server NAME=CMD ARGS
                            Spawn a trusted MCP stdio server and expose its tools (repeatable)

Workspace Revision Management (requires --resume):
  --undo                    Restore the workspace and session leaf to the previous turn's revision
  --restore-rev <REV_ID>    Restore the workspace to a specific captured revision
  --force-restore           Overwrite external modifications during --undo / --restore-rev
  --list-revisions          List the workspace revisions captured for the session

Environment Credentials:
  KROMA_API_KEY           Provider API key (stripped from child shell environments)
  KROMA_BASE_URL          OpenAI-compatible base URL (HTTPS required for remote hosts)
  KROMA_MODEL             Model identifier
  KROMA_PROTOCOL          Wire protocol override (auto | chat-completions | anthropic-messages | openai-responses)
  KROMA_REASONING_EFFORT  Reasoning effort fallback when --reasoning-effort is not set
  KROMA_VERIFY_COMMAND   Optional argv-style completion check, e.g. \"cargo test --workspace\"
  KROMA_VERIFY_TIMEOUT_SECS  Completion check timeout (default 180)
  KROMA_GATE_MAX_RETRIES Maximum completion-gate rejections (default 3)

Security Notice:
  Sensitive path guard blocks accidental access to .env/keys via file tools;
  granting --allow-shell executes commands under the current OS user without an OS container sandbox.

Exit codes:
  0 success, 1 run failure, 2 invalid invocation, 3 restore conflict, 4 restore recovery required."
}

/// `v1.3.0`: rebuilds the provider client when `/model`, `/thinking` or `/protocol` hot-swaps the
/// active configuration, so the operator never has to restart `kroma`.
struct CliProviderFactory {
    base_url: String,
    api_key: String,
    default_model: String,
    reasoning_effort: Option<String>,
}

impl kroma_core::surface::ProviderFactory for CliProviderFactory {
    fn build(
        &self,
        model: &str,
        protocol: &str,
        reasoning_effort: Option<&str>,
    ) -> Result<
        (
            kroma_core::surface::ProviderSwap,
            Arc<dyn kroma_core::provider::ModelProvider>,
        ),
        String,
    > {
        let parsed = ProviderProtocol::parse(protocol).map_err(|e| e.to_string())?;
        let effective_model = if model.trim().is_empty() {
            self.default_model.clone()
        } else {
            model.trim().to_string()
        };
        let settings = ProviderSettings {
            base_url: self.base_url.clone(),
            api_key: self.api_key.clone(),
            model: effective_model.clone(),
            max_retries: 2,
            reasoning_effort: reasoning_effort
                .map(|r| r.to_string())
                .or_else(|| self.reasoning_effort.clone()),
            protocol: parsed,
            max_tokens: 0,
        };
        let (resolved, provider) = build_provider(&settings)
            .map_err(|e| format!("invalid provider configuration: {}", e))?;
        Ok((
            kroma_core::surface::ProviderSwap {
                protocol: resolved.as_str().to_string(),
                model: effective_model,
            },
            provider,
        ))
    }
}

/// P1-4: builds an independent runtime for each session the multi-session `kroma serve` daemon
/// hosts. It reuses [`build_surface_runtime_config`] so every session gets the same policy, budget,
/// gate and tool filter as the daemon's startup session, and rebuilds the CLI extension (hooks,
/// skills, MCP stanzas) per runtime.
struct CliSessionRuntimeFactory {
    cli: CliArgs,
    resolved_config: ResolvedConfig,
    budget: BudgetConfig,
    gate: SharedCompletionGate,
    max_gate_retries: usize,
    surface_name: String,
    transport_name: String,
    state_dir: PathBuf,
    provider_factory: Arc<dyn kroma_core::surface::ProviderFactory>,
}

impl kroma_core::surface::SessionRuntimeFactory for CliSessionRuntimeFactory {
    fn build(
        &self,
        workspace_root: &Path,
        resume_session_path: Option<&Path>,
    ) -> Result<RpcDispatcher, String> {
        let (_, provider) = self.provider_factory.build("", "auto", None)?;
        let mut cfg = build_surface_runtime_config(
            &self.cli,
            &self.resolved_config,
            self.budget.clone(),
            self.gate.clone(),
            self.max_gate_retries,
            &self.surface_name,
            &self.transport_name,
            workspace_root.to_path_buf(),
            self.state_dir.clone(),
            resume_session_path.map(Path::to_path_buf),
            self.provider_factory.clone(),
        )?;
        if let Some(ext) = build_cli_extension(&self.cli)? {
            cfg.extensions.push(ext);
        }
        RpcDispatcher::new(cfg, provider)
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = match parse_cli_args(std::env::args()) {
        Ok(c) => c,
        Err(msg) => {
            if msg.starts_with("Usage:") || msg.starts_with("kroma ") {
                println!("{}", msg);
                return ExitCode::SUCCESS;
            }
            eprintln!("error: {}", msg);
            return ExitCode::from(2);
        }
    };

    let state_dir = resolve_default_state_dir();

    // Item #3 (`v1.4.0`): `kroma bench` is a self-contained measurement command. With the default
    // `--executor synthetic` it never contacts a provider; `--executor provider` loads the same
    // resolved provider configuration as a normal run (and fails with exit 2 when no key is set).
    if matches!(cli.surface, CliSurface::Bench { .. }) {
        return run_bench(&cli).await;
    }

    // Gate 1: revision management actions never contact a provider and never need an API key.
    if cli.is_revision_management() {
        return run_revision_management(&cli, &state_dir);
    }

    // Gate 3: skill discovery is a standalone inspection action.
    if cli.list_skills {
        return run_list_skills(&cli);
    }

    // Gate 3: fail fast on an unknown `--skill` before requiring provider credentials, so the
    // operator gets an actionable list instead of an unrelated API-key error.
    if let Some(ref skill_name) = cli.skill {
        match WorkspaceCap::new(&cli.workspace) {
            Ok(cap) => {
                let catalog = discover_skills(&cap);
                if !catalog.iter().any(|s| s.name == *skill_name) {
                    let discovered = if catalog.is_empty() {
                        "<none>".to_string()
                    } else {
                        catalog
                            .iter()
                            .map(|s| s.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    };
                    eprintln!(
                        "error: --skill unknown skill '{}'; discovered: {}",
                        skill_name, discovered
                    );
                    return ExitCode::from(2);
                }
            }
            Err(e) => {
                eprintln!("error: failed to initialize workspace capability: {}", e);
                return ExitCode::from(2);
            }
        }
    }

    // v1.3.0: layered configuration. Precedence is CLI flags > KROMA_* env vars >
    // workspace `.kroma.toml` (benign keys only) > `~/.kroma/config.toml`. A workspace file can
    // never contribute credentials, endpoints, hooks or verification commands.
    let overrides = config_overrides_for(&cli);
    let resolved_config = match ResolvedConfig::load(&cli.workspace, &overrides) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {}", e);
            return ExitCode::from(2);
        }
    };
    if resolved_config.workspace_guard_triggered() && !cli.quiet {
        // P5 (`v1.5.0`): the path and rejected key names come from an untrusted workspace file, so
        // they are escaped before reaching stderr. `sanitize_diagnostic_label` also escapes tabs
        // and newlines, which `sanitize_terminal_text` deliberately preserves for banner content.
        let path = resolved_config
            .workspace_config_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| ".kroma.toml".to_string());
        let keys = resolved_config
            .rejected_workspace_keys
            .iter()
            .map(|k| kroma_core::surface::sanitize_diagnostic_label(k))
            .collect::<Vec<_>>()
            .join(", ");
        eprintln!(
            "[kroma] ignoring untrusted workspace keys in {}: {}",
            kroma_core::surface::sanitize_diagnostic_label(&path),
            keys
        );
    }

    let api_key = match resolved_config.api_key.clone() {
        Some(k) if !k.trim().is_empty() => k,
        _ => {
            if matches!(cli.surface, CliSurface::Repl) {
                // Interactive REPL allows zero-config startup: user can configure via `/config set api_key <key>` inside REPL.
                "unconfigured".to_string()
            } else {
                eprintln!(
                    "error: no API key configured; set KROMA_API_KEY, add api_key to ~/.kroma/config.toml, or pass --profile"
                );
                return ExitCode::from(2);
            }
        }
    };
    let base_url = resolved_config
        .base_url
        .clone()
        .unwrap_or_else(|| "https://api.openai.com".to_string());
    let model = resolved_config
        .model
        .clone()
        .unwrap_or_else(|| "gpt-4o".to_string());
    let reasoning_effort = resolved_config.reasoning_effort.clone();

    // Gate 7: one settings struct feeds all three wire protocols. `--protocol` / `KROMA_PROTOCOL`
    // override auto-detection from the base URL and model id.
    let protocol = match resolved_config.protocol.as_deref() {
        Some(raw) if !raw.trim().is_empty() => match ProviderProtocol::parse(raw) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("error: invalid protocol '{}': {}", raw, e);
                return ExitCode::from(2);
            }
        },
        _ => ProviderProtocol::Auto,
    };
    let provider_settings = ProviderSettings {
        base_url: base_url.clone(),
        api_key: api_key.clone(),
        model: model.clone(),
        max_retries: 2,
        reasoning_effort: reasoning_effort.clone(),
        protocol,
        max_tokens: 0,
    };
    let (resolved_protocol, provider) = match build_provider(&provider_settings) {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("error: invalid provider configuration: {}", e);
            return ExitCode::from(2);
        }
    };
    if !cli.quiet && resolved_protocol != ProviderProtocol::ChatCompletions {
        eprintln!("[kroma] provider protocol: {}", resolved_protocol.as_str());
    }

    let budget = BudgetConfig {
        // F05 (`v1.5.1`): the resolved `--max-tokens` / `KROMA_MAX_TOKENS` / `max_tokens` value is
        // the single run-tree token cap shared by the parent run and every child subagent.
        max_total_tokens: resolved_config.max_tokens,
        ..BudgetConfig::default()
    };
    // The resolved config already encodes the documented precedence (CLI `--verify-cmd` >
    // `KROMA_VERIFY_COMMAND` > `~/.kroma/config.toml`), so passing it here is what makes a
    // `verify_command` written to the config file actually take effect in headless mode.
    let gate_choice = match choose_completion_gate(&cli, resolved_config.verify_command.clone()) {
        Ok(choice) => choice,
        Err(e) => {
            eprintln!("error: {}", e);
            return ExitCode::from(2);
        }
    };
    let max_gate_retries = std::env::var("KROMA_GATE_MAX_RETRIES")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(3);

    // Gate 6: multi-surface dispatch (`repl`, `acp`, `serve`) through the unified `RpcDispatcher`.
    if cli.surface != CliSurface::Headless {
        let (surface_name, transport_name) = match &cli.surface {
            CliSurface::Repl => ("repl", "tty"),
            CliSurface::Acp => ("acp", "stdio"),
            CliSurface::Serve { listen_addr } => {
                if kroma_core::surface::is_unix_listen_spec(listen_addr) {
                    ("serve", "unix")
                } else {
                    ("serve", "tcp")
                }
            }
            CliSurface::Headless => ("headless", "stdio"),
            CliSurface::Bench { .. } => ("bench", "local"),
        };
        let provider_factory: Arc<dyn kroma_core::surface::ProviderFactory> =
            Arc::new(CliProviderFactory {
                base_url: base_url.clone(),
                api_key: api_key.clone(),
                default_model: model.clone(),
                reasoning_effort: reasoning_effort.clone(),
            });
        let gate = build_completion_gate(gate_choice);
        let mut rt_cfg = match build_surface_runtime_config(
            &cli,
            &resolved_config,
            budget.clone(),
            gate.clone(),
            max_gate_retries,
            surface_name,
            transport_name,
            cli.workspace.clone(),
            state_dir.clone(),
            cli.resume.clone(),
            provider_factory.clone(),
        ) {
            Ok(cfg) => cfg,
            Err(e) => {
                eprintln!("error: {}", e);
                return ExitCode::from(2);
            }
        };
        match build_cli_extension(&cli) {
            Ok(Some(ext)) => rt_cfg.extensions.push(ext),
            Ok(None) => {}
            Err(e) => {
                eprintln!("error: {}", e);
                return ExitCode::from(2);
            }
        }

        let dispatcher = match RpcDispatcher::new(rt_cfg, provider) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("error: {}", e);
                return ExitCode::from(2);
            }
        };

        let (cancel_tx, cancel_rx) = watch::channel(false);
        let disp_for_ctrlc = dispatcher.clone();
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                // P2 (`v1.5.0`): the first `^C` must cancel the run in flight, not just signal a
                // watch channel the REPL branch never reads. `cancel_active_run` fires the
                // dispatcher's real `active_cancel` watch (observed by provider/tool waits) and
                // denies any pending approval; a second `^C` remains the hard exit.
                let _ = cancel_tx.send(true);
                disp_for_ctrlc.cancel_active_run();
                if tokio::signal::ctrl_c().await.is_ok() {
                    std::process::exit(130);
                }
            }
        });

        let res = match &cli.surface {
            CliSurface::Repl => {
                let repl = InteractiveRepl::new(dispatcher);
                let stdin = tokio::io::BufReader::new(tokio::io::stdin());
                let stdout = tokio::io::stdout();
                repl.run_io_with_initial_prompt(stdin, stdout, cli.task.as_deref())
                    .await
            }
            CliSurface::Acp => {
                dispatcher
                    .serve_stream(tokio::io::stdin(), tokio::io::stdout())
                    .await
            }
            CliSurface::Bench { .. } => Err(
                "kroma bench runs before the agent surfaces; this branch is unreachable"
                    .to_string(),
            ),
            CliSurface::Serve { listen_addr } => {
                if cli.multi_session {
                    // P1-4: bounded multi-session daemon. The startup dispatcher is the first
                    // session; `session.new` opens additional runtimes and later requests route by
                    // the session id they name. The cross-process workspace writer lock still
                    // serializes sessions bound to the same workspace.
                    let factory = Arc::new(CliSessionRuntimeFactory {
                        cli: cli.clone(),
                        resolved_config: resolved_config.clone(),
                        budget: budget.clone(),
                        gate: gate.clone(),
                        max_gate_retries,
                        surface_name: surface_name.to_string(),
                        transport_name: transport_name.to_string(),
                        state_dir: state_dir.clone(),
                        provider_factory: provider_factory.clone(),
                    });
                    let manager = match kroma_core::surface::SessionManager::open(
                        kroma_core::surface::SessionManagerConfig {
                            factory,
                            max_sessions: kroma_core::surface::DEFAULT_MAX_DAEMON_SESSIONS,
                            index_path: state_dir.join("sessions").join("daemon-sessions.json"),
                            default_workspace: cli.workspace.clone(),
                        },
                        dispatcher,
                    ) {
                        Ok(manager) => manager,
                        Err(e) => {
                            eprintln!("error: failed to open the session manager: {}", e);
                            return ExitCode::from(2);
                        }
                    };
                    if !cli.quiet {
                        eprintln!(
                            "[kroma] multi-session daemon hosting {} session(s)",
                            manager.len()
                        );
                    }
                    match KromaDaemon::bind_multi(listen_addr, manager).await {
                        Ok(daemon) => {
                            if !cli.quiet {
                                eprintln!(
                                    "[kroma] daemon listening on {}",
                                    daemon.local_endpoint()
                                );
                            }
                            if let Some(warning) = daemon.non_loopback_warning() {
                                eprintln!("[kroma] WARNING: {}", warning);
                            }
                            daemon.serve(cancel_rx).await
                        }
                        Err(e) => Err(e),
                    }
                } else {
                    match KromaDaemon::bind(listen_addr, dispatcher).await {
                        Ok(daemon) => {
                            if !cli.quiet {
                                eprintln!(
                                    "[kroma] daemon listening on {}",
                                    daemon.local_endpoint()
                                );
                            }
                            if let Some(warning) = daemon.non_loopback_warning() {
                                eprintln!("[kroma] WARNING: {}", warning);
                            }
                            daemon.serve(cancel_rx).await
                        }
                        Err(e) => Err(e),
                    }
                }
            }
            CliSurface::Headless => unreachable!(),
        };

        return match res {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("error: {}", e);
                ExitCode::from(1)
            }
        };
    }

    let env = match LocalHostEnv::new(&cli.workspace, state_dir.clone()) {
        Ok(e) => Arc::new(e),
        Err(e) => {
            eprintln!("error: failed to initialize workspace capability: {}", e);
            return ExitCode::from(2);
        }
    };

    // F8: a crash midway through a worktree→parent merge leaves a merge intent journal behind and
    // the parent workspace half-updated. Roll it back to the recorded pre-merge revision before any
    // new run starts, exactly like the restore-intent pre-check below handles an interrupted
    // restore.
    match snapshot_store_for(&state_dir).recover_pending_merge(env.workspace()) {
        Ok(MergeRecoveryOutcome::NothingPending) => {}
        Ok(MergeRecoveryOutcome::RolledBack {
            pre_merge_revision, ..
        }) => {
            if !cli.quiet {
                eprintln!(
                    "[kroma] recovered an interrupted subagent merge; rolled the workspace back to {}",
                    pre_merge_revision
                );
            }
        }
        Ok(MergeRecoveryOutcome::AlreadyMerged { merged_revision }) => {
            if !cli.quiet {
                eprintln!(
                    "[kroma] cleared a stale subagent merge journal; revision {} had already been merged",
                    merged_revision
                );
            }
        }
        Err(e) => {
            eprintln!(
                "error: failed to recover an interrupted subagent merge: {}",
                e
            );
            return ExitCode::from(EXIT_RESTORE_RECOVERY_REQUIRED);
        }
    }

    // Freeze the workspace orientation block (AGENTS.md/CLAUDE.md + sorted top-level entries)
    // into the static system prefix before `env` is moved into the tool runtime.
    let mut prompt_builder = PromptBuilder::default().with_workspace_context(env.workspace());
    if let Some(ref skill_name) = cli.skill {
        prompt_builder = match prompt_builder.with_activated_skill(env.workspace(), skill_name) {
            Ok(pb) => pb,
            Err(e) => {
                eprintln!("error: --skill {}", e);
                return ExitCode::from(2);
            }
        };
    }
    // Gate 3: the workspace skill catalog is discovered once and shared by `activate_skill`.
    let workspace_skills = discover_skills(env.workspace());

    let session = if let Some(path) = cli.resume.as_ref() {
        let mut journal = match SessionJournal::open(path) {
            Ok(j) => j,
            Err(e) => {
                eprintln!("error: failed to resume session WAL: {}", e);
                return ExitCode::from(2);
            }
        };
        if journal.workspace_root() != env.workspace().root() {
            eprintln!(
                "error: session WAL belongs to workspace {}, not {}",
                journal.workspace_root().display(),
                env.workspace().root().display()
            );
            return ExitCode::from(2);
        }
        // O6: a crash between the snapshot write and the WAL append leaves the store one revision
        // ahead of the WAL. Reconcile before `--fork` so a resumed run never starts from a stale
        // `latest_revision_id()`.
        let store_for_resume = snapshot_store_for(&state_dir);
        if let Err(e) = reconcile_store_with_wal(env.workspace(), &mut journal, &store_for_resume) {
            eprintln!(
                "error: failed to reconcile session WAL with the snapshot store: {}",
                e
            );
            return ExitCode::from(EXIT_RESTORE_RECOVERY_REQUIRED);
        }
        // F03: `--fork` is workspace-bound, exactly like `session.fork` over RPC: restore the
        // workspace to the revision current at that turn, then move the session leaf. External
        // edits make the fork fail closed instead of being overwritten.
        if let Some(turn) = cli.fork_turn {
            match fork_resumed_session(&store_for_resume, env.workspace(), &mut journal, turn) {
                Ok(outcome) => {
                    if !cli.quiet {
                        eprintln!(
                            "[kroma] forked workspace at turn {} (restored {} file(s), removed {} file(s), revision {})",
                            outcome.forked_at_turn,
                            outcome.restored_files.len(),
                            outcome.removed_files.len(),
                            outcome.target_revision
                        );
                    }
                }
                Err(e) => {
                    eprintln!("error: failed to fork workspace at turn {}: {}", turn, e);
                    return ExitCode::from(EXIT_RESTORE_RECOVERY_REQUIRED);
                }
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
        eprintln!("[kroma] session WAL: {}", session.path().display());
    }

    // F10: an interrupted restore leaves the workspace half-applied. Mutating it now would layer a
    // new run on top of that; refuse until the restore is recovered, exactly like the RPC surface.
    {
        let store = snapshot_store_for(&state_dir);
        match store.pending_restore() {
            Ok(Some(intent)) => {
                eprintln!(
                    "error: an interrupted restore of {} must be recovered before a new run (intent journal: {})",
                    intent.target_revision,
                    store.intent_path().display()
                );
                return ExitCode::from(EXIT_RESTORE_RECOVERY_REQUIRED);
            }
            Ok(None) => {}
            Err(e) => {
                eprintln!("error: failed to read the restore intent journal: {}", e);
                return ExitCode::from(EXIT_RESTORE_RECOVERY_REQUIRED);
            }
        }
    }

    let mut run_cfg = AgentRunConfig::new(cli.task.clone().unwrap_or_default(), budget.clone());
    run_cfg.stream = !cli.no_stream;
    run_cfg.strict_log = cli.strict_log;
    // B2 (`v1.4.0`): apply the configured context projection profile.
    run_cfg.projection_profile = resolved_config
        .projection_profile
        .as_deref()
        .and_then(|v| kroma_core::context_projector::ProjectionProfile::parse(v).ok());
    // Capability gap #6 (`v1.5.2`): on-demand tool schema mode.
    run_cfg.on_demand_tools = cli.on_demand_tools;
    // Capability gap #3 (`v1.5.2`): causal pruning of dead-end context.
    run_cfg.causal_pruning = cli.causal_pruning;

    let mut logger = match EventLogger::new(run_cfg.run_id, Some(state_dir.clone()), cli.strict_log)
    {
        Ok(l) => l,
        Err(e) => {
            eprintln!("error: {}", e);
            return ExitCode::from(1);
        }
    };
    logger.set_live_stderr(!cli.quiet && cli.output_format != "jsonl");
    if cli.output_format == "jsonl" {
        logger.set_event_sink(Arc::new(|ev| {
            if let Ok(line) = serde_json::to_string(ev) {
                println!("{}", line);
            }
        }));
    }
    let _ = logger.log(EventPayload::SurfaceStarted {
        surface: "headless".to_string(),
        transport: cli.output_format.clone(),
    });

    let approval_ui = Arc::new(TtyApprovalUi::new());
    let policy_engine = PolicyEngine::new(cli.policy_mode, approval_ui);

    // Gate 3: the registry carries the reversible extension mount points, the workspace skill
    // catalog, and the CLI-declared hooks.
    // BUG-01 (`v1.5.1`): the headless path must honour the same resolved `max_subagents` as the
    // REPL/serve path (`resolved_config` already merges CLI > env > workspace > user file).
    let max_subagents = resolved_config.max_subagents.unwrap_or(cli.max_subagents);
    let mut registry =
        ToolRegistry::new(cli.tool_filter.clone()).with_workspace_skills(workspace_skills);
    if max_subagents == 0 {
        registry = registry.without_subagents();
    }
    match build_cli_extension(&cli) {
        Ok(Some(ext)) => {
            let name = ext.manifest.name.clone();
            match registry.register_extension(ext) {
                Ok(id) => {
                    let _ = logger.log(EventPayload::ExtensionChanged {
                        extension_id: id.as_str().to_string(),
                        action: "registered".to_string(),
                    });
                    if !cli.quiet {
                        eprintln!("[kroma] mounted extension: {}", name);
                    }
                }
                Err(e) => {
                    eprintln!("error: failed to mount hook extension: {}", e);
                    return ExitCode::from(2);
                }
            }
        }
        Ok(None) => {}
        Err(e) => {
            eprintln!("error: {}", e);
            return ExitCode::from(2);
        }
    }

    // Capability gap #6 (`v1.5.2`): the headless path must load trusted MCP servers through the
    // exact same helper the dispatcher uses, so `--mcp-server` / `[mcp.<name>]` is never accepted
    // here but silently ignored.
    match mount_trusted_mcp(&mut registry, &resolved_config.mcp_servers, cli.quiet) {
        Ok(Some(tool_count)) => {
            let _ = logger.log(EventPayload::ExtensionChanged {
                extension_id: "mcp".to_string(),
                action: "registered".to_string(),
            });
            let _ = tool_count;
        }
        Ok(None) => {}
        Err(e) => {
            eprintln!("error: {}", e);
            return ExitCode::from(2);
        }
    }

    // Gate 3: subtree JIT context. `--fork <TURN>` has already rewound the session leaf to a turn
    // `<= TURN`, so `latest_active_checkpoint()` returns the checkpoint on the *rewound* branch: the
    // directories whose guidance is still present in that branch's transcript must be restored for
    // a fork exactly as for a plain resume, or they would be re-injected on touch.
    let mut subtree_tracker = SubtreeContextTracker::new(!cli.no_subtree_context);
    if let Some(checkpoint) = session.latest_active_checkpoint() {
        subtree_tracker.restore_loaded_dirs(checkpoint.subtree_dirs.clone());
    }

    // T09 (KR-18): the run's execution boundary is resolved once, here, and installed on the runtime
    // that performs the real spawns. An unrecognised value already failed at config load; this is
    // the one place that turns the string into a policy, so no surface can disagree.
    let execution_policy = match kroma_core::config_file::execution_policy_from_config(
        resolved_config.execution_sandbox.as_deref(),
    ) {
        Ok(policy) => policy,
        Err(e) => {
            eprintln!("error: {}", e);
            return ExitCode::from(2);
        }
    };
    let runtime = build_runtime_with_registry(registry, env, policy_engine, budget)
        .with_predictive_check(!cli.no_predictive_check)
        .with_execution_policy(execution_policy);

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

    let snapshot_store = Arc::new(snapshot_store_for(&state_dir));
    if !cli.quiet {
        eprintln!(
            "[kroma] workspace revisions: {}",
            snapshot_store.root().display()
        );
    }
    let runner = AgentRunner::new(run_cfg, provider, runtime, cancel_rx)
        .with_prompt_builder(prompt_builder)
        .with_session_journal(session)
        .with_snapshot_store(snapshot_store)
        .with_subtree_context(subtree_tracker)
        .with_completion_gate(build_completion_gate(gate_choice))
        .with_completion_gate_max_retries(max_gate_retries)
        .with_max_subagents(max_subagents)
        // Item #3 (`v1.4.0`): adaptive tournament escalation on repeated verification failure.
        .with_adaptive_tournament_candidates(resolved_config.adaptive_tournament.unwrap_or(0))
        // Capability gap #5 (`v1.5.2`): strict repair-race verification policy.
        .with_tournament_require_verified(cli.tournament_require_verified);
    // Gate 4: `--predict-cmd` is an explicit operator authorization, exactly like `--verify-cmd`,
    // so it runs without needing `--allow-shell`. It is skipped when `--policy read-only` or
    // `--no-predictive-check` is active.
    let runner = match cli.predict_cmd.as_deref() {
        Some(_) if cli.policy_mode == PolicyMode::ReadOnly || cli.no_predictive_check => runner,
        Some(raw) => match parse_command_argv(raw) {
            Ok(argv) if !argv.is_empty() => {
                if !cli.quiet {
                    eprintln!("[kroma] predictive check: {}", argv.join(" "));
                }
                runner.with_predictive_command(Arc::new(
                    WorkspaceCompletionGate::with_auto_detection(
                        Some(argv),
                        configured_timeout(),
                        false,
                    ),
                ))
            }
            Ok(_) => {
                eprintln!("error: empty value for --predict-cmd");
                return ExitCode::from(2);
            }
            Err(e) => {
                eprintln!("error: {}", e);
                return ExitCode::from(2);
            }
        },
        None => runner,
    };
    // Capability gap #2 (`v1.5.2`): `--counterfactual-probe` is the operator's consent for the
    // isolated probe; without it the default deny keeps the probe off.
    let runner = if cli.counterfactual_probe {
        if !cli.quiet {
            eprintln!("[kroma] counterfactual probe: enabled (isolated worktree; bounded)");
        }
        runner.with_counterfactual_probe(
            kroma_core::attribution::CounterfactualProbeConfig::enabled_with(2),
            std::sync::Arc::new(kroma_core::attribution::AllowProbes),
        )
    } else {
        runner
    };
    let (summary, _logger) = runner.run(logger).await;

    if let Some(ref text) = summary.final_assistant_text {
        println!("{}", text);
    }

    if summary.log_degraded {
        eprintln!(
            "[WARN kroma] run completed with degraded diagnostic logging ({} events dropped)",
            summary.dropped_events
        );
    }

    match summary.status {
        RunStatus::Completed => ExitCode::SUCCESS,
        RunStatus::Cancelled => ExitCode::from(130),
        other => {
            eprintln!(
                "[kroma] run terminated with status {:?}: {:?}",
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
    use kroma_core::tool::policy::{ApprovalDecision, ApprovalHandler, ApprovalRequest};
    use kroma_core::types::EffectClass;

    #[test]
    fn test_cli_flags_and_policy_modes() {
        let ro = parse_cli_args(vec![
            "kroma".into(),
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
            "kroma".into(),
            "run".into(),
            "--workspace".into(),
            "./repo".into(),
            "--task".into(),
            "fix bug".into(),
        ])
        .unwrap();
        assert_eq!(ask_default.policy_mode, PolicyMode::Ask);

        let allow_w = parse_cli_args(vec![
            "kroma".into(),
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
            "kroma".into(),
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
            "kroma".into(),
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
            "kroma".into(),
            "run".into(),
            "--workspace".into(),
            "./repo".into(),
            "--task".into(),
            "continue".into(),
            "--resume".into(),
            "session.wal.jsonl".into(),
            "--fork".into(),
            "10".into(),
        ])
        .unwrap();
        assert_eq!(
            resumed.resume.as_deref(),
            Some(std::path::Path::new("session.wal.jsonl"))
        );
        assert_eq!(resumed.fork_turn, Some(10));
        assert!(parse_cli_args(vec![
            "kroma".into(),
            "run".into(),
            "--workspace".into(),
            "./repo".into(),
            "--task".into(),
            "continue".into(),
            "--fork".into(),
            "10".into(),
        ])
        .unwrap_err()
        .contains("requires --resume"));

        // Passing invalid --policy with --allow-write must be rejected
        let bad_policy = parse_cli_args(vec![
            "kroma".into(),
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
        let ver_res = parse_cli_args(vec!["kroma".into(), "run".into(), "--version".into()]);
        assert_eq!(
            ver_res.unwrap_err(),
            format!("kroma {}", kroma_core::version())
        );

        // Passing --api-key on CLI must be rejected (§2.1)
        let bad_key = parse_cli_args(vec![
            "kroma".into(),
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
            call_id: None,
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
            "kroma".into(),
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
        assert_eq!(
            explicit.verify_cmd.as_deref(),
            Some("sh -c \"cargo test --workspace\"")
        );
        assert!(!explicit.no_verify);

        let disabled = parse_cli_args(base_cli(vec!["--no-verify".into()])).unwrap();
        assert!(disabled.no_verify);
        assert!(disabled.verify_cmd.is_none());

        // Empty or missing values must be rejected rather than silently ignored.
        assert!(parse_cli_args(base_cli(vec!["--verify-cmd".into(), "   ".into()])).is_err());
        assert!(parse_cli_args(base_cli(vec!["--verify-cmd".into()])).is_err());
    }

    #[test]
    fn test_workspace_revision_management_flags_parse() {
        // `--list-revisions` and `--undo` are standalone actions: no --task required.
        let listed = parse_cli_args(vec![
            "kroma".into(),
            "run".into(),
            "--workspace".into(),
            "./repo".into(),
            "--resume".into(),
            "session.wal.jsonl".into(),
            "--list-revisions".into(),
        ])
        .unwrap();
        assert!(listed.list_revisions);
        assert!(listed.task.is_none());
        assert!(listed.is_revision_management());

        let undo = parse_cli_args(vec![
            "kroma".into(),
            "run".into(),
            "--workspace".into(),
            "./repo".into(),
            "--resume".into(),
            "session.wal.jsonl".into(),
            "--undo".into(),
            "--force-restore".into(),
        ])
        .unwrap();
        assert!(undo.undo);
        assert!(undo.force_restore);

        let restore = parse_cli_args(vec![
            "kroma".into(),
            "run".into(),
            "--workspace".into(),
            "./repo".into(),
            "--resume".into(),
            "session.wal.jsonl".into(),
            "--restore-rev".into(),
            "r-0123456789abcdef0123456789abcdef".into(),
        ])
        .unwrap();
        assert_eq!(
            restore.restore_rev.as_deref(),
            Some("r-0123456789abcdef0123456789abcdef")
        );

        // A normal run still requires --task.
        assert!(parse_cli_args(vec![
            "kroma".into(),
            "run".into(),
            "--workspace".into(),
            "./repo".into(),
        ])
        .unwrap_err()
        .contains("--task"));

        // Management actions need a session to act on.
        assert!(parse_cli_args(vec![
            "kroma".into(),
            "run".into(),
            "--workspace".into(),
            "./repo".into(),
            "--undo".into(),
        ])
        .unwrap_err()
        .contains("require --resume"));

        // Conflicting or dangling combinations are rejected rather than guessed at.
        assert!(parse_cli_args(vec![
            "kroma".into(),
            "run".into(),
            "--workspace".into(),
            "./repo".into(),
            "--resume".into(),
            "session.wal.jsonl".into(),
            "--undo".into(),
            "--restore-rev".into(),
            "r-0123456789abcdef0123456789abcdef".into(),
        ])
        .unwrap_err()
        .contains("mutually exclusive"));
        assert!(parse_cli_args(vec![
            "kroma".into(),
            "run".into(),
            "--workspace".into(),
            "./repo".into(),
            "--resume".into(),
            "session.wal.jsonl".into(),
            "--undo".into(),
            "--fork".into(),
            "2".into(),
        ])
        .unwrap_err()
        .contains("drop --fork"));
        assert!(parse_cli_args(base_cli(vec!["--force-restore".into()]))
            .unwrap_err()
            .contains("--force-restore requires"));
        assert!(parse_cli_args(base_cli(vec!["--restore-rev".into(), "   ".into()])).is_err());
        assert!(parse_cli_args(base_cli(vec!["--restore-rev".into()])).is_err());
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

        // KROMA_VERIFY_COMMAND is treated as explicit authorization too.
        let no_cmd = parse_cli_args(base_cli(vec![])).unwrap();
        assert_eq!(
            choose_completion_gate(&no_cmd, Some("cargo test --workspace".into())).unwrap(),
            CompletionGateChoice::Explicit(vec![
                "cargo".into(),
                "test".into(),
                "--workspace".into()
            ])
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
        let disabled =
            parse_cli_args(base_cli(vec!["--allow-shell".into(), "--no-verify".into()])).unwrap();
        assert_eq!(
            choose_completion_gate(&disabled, Some("cargo test".into())).unwrap(),
            CompletionGateChoice::Disabled
        );

        // Read-only policy never enables verification.
        let ro = parse_cli_args(base_cli(vec!["--policy".into(), "read-only".into()])).unwrap();
        assert_eq!(
            choose_completion_gate(&ro, None).unwrap(),
            CompletionGateChoice::Disabled
        );
    }

    /// `--restore-rev` must bind the restored revision into the WAL, so a following `--undo`
    /// compares against the revision that is actually on disk instead of reporting a false conflict.
    #[test]
    fn test_restore_rev_records_revision_so_a_following_undo_stays_in_sync() {
        use kroma_core::messages::{TranscriptItem, UserMessage};
        use std::fs;

        let root =
            std::env::temp_dir().join(format!("kroma-cli-restore-rev-{}", uuid::Uuid::new_v4()));
        let ws_dir = root.join("ws");
        fs::create_dir_all(&ws_dir).unwrap();
        fs::write(ws_dir.join("f.txt"), "v0\n").unwrap();

        let workspace = WorkspaceCap::new(&ws_dir).unwrap();
        let store = SnapshotStore::new(root.join("snapshots"));
        let mut journal = SessionJournal::create(root.join("sessions"), &ws_dir).unwrap();

        journal
            .append_item(
                0,
                TranscriptItem::User(UserMessage {
                    content: "task".into(),
                }),
            )
            .unwrap();
        let r0 = store.capture_revision(&workspace, None, None, 0).unwrap();
        journal
            .record_revision_capture(
                0,
                r0.revision_id.as_str(),
                None,
                r0.changed_paths.clone(),
                r0.tracked_files.len(),
            )
            .unwrap();

        fs::write(ws_dir.join("f.txt"), "v1\n").unwrap();
        journal
            .append_item(
                1,
                TranscriptItem::User(UserMessage {
                    content: "turn 1".into(),
                }),
            )
            .unwrap();
        let r1 = store
            .capture_revision(&workspace, Some(&r0.revision_id), None, 1)
            .unwrap();
        journal
            .record_revision_capture(
                1,
                r1.revision_id.as_str(),
                Some(r0.revision_id.as_str()),
                r1.changed_paths.clone(),
                r1.tracked_files.len(),
            )
            .unwrap();

        // `--restore-rev r0`: the expected revision is the session's latest recorded one.
        let restored = store
            .restore_revision(&workspace, &r0.revision_id, Some(&r1.revision_id), false)
            .unwrap();
        let (restored_files, removed_files) = match &restored {
            RestoreResult::Applied {
                restored_files,
                removed_files,
                ..
            } => (restored_files.clone(), removed_files.clone()),
            other => panic!("expected Applied, got {:?}", other),
        };
        record_restored_revision(
            &mut journal,
            &store,
            &r0.revision_id,
            &restored_files,
            &removed_files,
            Some(&r1.revision_id),
        )
        .unwrap();
        assert_eq!(
            journal.latest_revision_id().as_deref(),
            Some(r0.revision_id.as_str()),
            "--restore-rev must leave the WAL describing the restored workspace"
        );

        // A following `--undo` now compares r1 against the on-disk r0 and must apply cleanly.
        let records = journal.revision_records();
        let current = records.last().unwrap().clone();
        let previous = records
            .iter()
            .rev()
            .find(|record| record.revision_id != current.revision_id)
            .unwrap()
            .clone();
        assert_eq!(current.revision_id, r0.revision_id.as_str());
        assert_eq!(previous.revision_id, r1.revision_id.as_str());
        let undo = store
            .restore_revision(
                &workspace,
                &RevisionId::parse(&previous.revision_id).unwrap(),
                Some(&RevisionId::parse(&current.revision_id).unwrap()),
                false,
            )
            .unwrap();
        assert!(
            matches!(undo, RestoreResult::Applied { .. }),
            "--undo after --restore-rev must not report a false conflict: {:?}",
            undo
        );
        assert_eq!(fs::read_to_string(ws_dir.join("f.txt")).unwrap(), "v1\n");
        let _ = fs::remove_dir_all(root);
    }

    /// A temp workspace + session WAL with two recorded revisions `r0` (S0) and `r1` (S1), plus a
    /// merge journal parented on `r1` and a half-applied merge (`edit.rs` half-written). `--undo`
    /// and `--restore-rev r0` both target `r0` with `r1` as the expected revision.
    fn pending_merge_revision_management_fixture(
        tag: &str,
    ) -> (PathBuf, PathBuf, SnapshotStore, RevisionId, PathBuf) {
        use kroma_core::messages::{TranscriptItem, UserMessage};
        use std::fs;

        let root = std::env::temp_dir().join(format!("kroma-cli-{}-{}", tag, uuid::Uuid::new_v4()));
        let ws_dir = root.join("ws");
        fs::create_dir_all(&ws_dir).unwrap();
        fs::write(ws_dir.join("edit.rs"), "v1\n").unwrap();
        fs::write(ws_dir.join("keep.rs"), "v1\n").unwrap();

        let workspace = WorkspaceCap::new(&ws_dir).unwrap();
        let store = SnapshotStore::new(root.join("snapshots"));
        let mut journal = SessionJournal::create(root.join("sessions"), &ws_dir).unwrap();
        journal
            .append_item(
                0,
                TranscriptItem::User(UserMessage {
                    content: "task".into(),
                }),
            )
            .unwrap();
        let r0 = store.capture_revision(&workspace, None, None, 0).unwrap();
        journal
            .record_revision_capture(
                0,
                r0.revision_id.as_str(),
                None,
                r0.changed_paths.clone(),
                r0.tracked_files.len(),
            )
            .unwrap();

        fs::write(ws_dir.join("edit.rs"), "v2\n").unwrap();
        journal
            .append_item(
                1,
                TranscriptItem::User(UserMessage {
                    content: "turn 1".into(),
                }),
            )
            .unwrap();
        let r1 = store
            .capture_revision(&workspace, Some(&r0.revision_id), None, 1)
            .unwrap();
        journal
            .record_revision_capture(
                1,
                r1.revision_id.as_str(),
                Some(r0.revision_id.as_str()),
                r1.changed_paths.clone(),
                r1.tracked_files.len(),
            )
            .unwrap();

        // A merge parented on r1 was interrupted halfway: its journal is on disk and `edit.rs` is
        // half-written.
        store
            .write_merge_intent(&kroma_core::revision::MergeIntent {
                version: 1,
                pre_merge_revision: r1.revision_id.clone(),
                created_at_ms: 0,
                written_files: vec!["edit.rs".to_string()],
                removed_files: vec!["keep.rs".to_string()],
            })
            .unwrap();
        fs::write(ws_dir.join("edit.rs"), "half-merged\n").unwrap();

        let session_path = journal.path().to_path_buf();
        (root, ws_dir, store, r0.revision_id, session_path)
    }

    /// F8: `--restore-rev` must run the merge pre-check before restoring, so a half-merged workspace
    /// with a pending merge journal is healed (and the journal cleared) instead of making the
    /// restore conflict — or, with `--force-restore`, leaving the journal to be replayed later.
    #[test]
    fn test_restore_rev_recovers_a_pending_merge_before_restoring() {
        use std::fs;

        let (root, ws_dir, store, r0, session_path) =
            pending_merge_revision_management_fixture("restore-rev-merge-precheck");
        let cli = parse_cli_args(vec![
            "kroma".into(),
            "run".into(),
            "--workspace".into(),
            ws_dir.to_string_lossy().into_owned(),
            "--resume".into(),
            session_path.to_string_lossy().into_owned(),
            "--restore-rev".into(),
            r0.as_str().to_string(),
        ])
        .unwrap();

        let _ = run_revision_management(&cli, &root);

        assert!(
            store.pending_merge_intent().unwrap().is_none(),
            "--restore-rev must clear the merge journal before restoring"
        );
        assert_eq!(
            fs::read_to_string(ws_dir.join("edit.rs")).unwrap(),
            "v1\n",
            "the half-merged workspace must be healed before the restore"
        );
        let _ = fs::remove_dir_all(root);
    }

    /// F8: `--undo` shares the same pre-check call site, so a half-merged workspace is healed before
    /// the undo instead of making the undo conflict on the half-merge.
    #[test]
    fn test_undo_recovers_a_pending_merge_before_restoring() {
        use std::fs;

        let (root, ws_dir, store, _r0, session_path) =
            pending_merge_revision_management_fixture("undo-merge-precheck");
        let cli = parse_cli_args(vec![
            "kroma".into(),
            "run".into(),
            "--workspace".into(),
            ws_dir.to_string_lossy().into_owned(),
            "--resume".into(),
            session_path.to_string_lossy().into_owned(),
            "--undo".into(),
        ])
        .unwrap();

        let _ = run_revision_management(&cli, &root);

        assert!(
            store.pending_merge_intent().unwrap().is_none(),
            "--undo must clear the merge journal before restoring"
        );
        assert_eq!(
            fs::read_to_string(ws_dir.join("edit.rs")).unwrap(),
            "v1\n",
            "the half-merged workspace must be healed before the undo"
        );
        let _ = fs::remove_dir_all(root);
    }

    /// Gate 2 audit (Finding 1): when the *undone* revision is bound to the same leaf the rewind
    /// selects — which happens after a `--restore-rev` re-records it at the current leaf — `--undo`
    /// must re-bind the restored revision into the WAL. Otherwise `latest_revision_id()` keeps
    /// naming the undone revision and the next `--restore-rev` fails with a false
    /// `ModifiedExternally` conflict.
    /// F03: the CLI `--fork` path is workspace-bound, exactly like `session.fork` over RPC. It only
    /// moves the session leaf after the workspace restore succeeds, and refuses to overwrite an
    /// external edit.
    #[test]
    fn test_cli_fork_restores_workspace_and_moves_the_leaf() {
        use kroma_core::messages::{TranscriptItem, UserMessage};
        use std::fs;

        let root = std::env::temp_dir().join(format!("kroma-cli-fork-{}", uuid::Uuid::new_v4()));
        let ws_dir = root.join("ws");
        let state_dir = root.join("state");
        fs::create_dir_all(&ws_dir).unwrap();
        fs::create_dir_all(&state_dir).unwrap();
        fs::write(ws_dir.join("f.txt"), "v0\n").unwrap();

        let workspace = WorkspaceCap::new(&ws_dir).unwrap();
        let store = SnapshotStore::new(state_dir.join("snapshots"));
        let mut journal = SessionJournal::create(state_dir.join("sessions"), &ws_dir).unwrap();
        journal
            .append_item(
                0,
                TranscriptItem::User(UserMessage {
                    content: "task".into(),
                }),
            )
            .unwrap();
        let r0 = store.capture_revision(&workspace, None, None, 0).unwrap();
        journal
            .record_revision_capture(
                0,
                r0.revision_id.as_str(),
                None,
                r0.changed_paths.clone(),
                r0.tracked_files.len(),
            )
            .unwrap();

        fs::write(ws_dir.join("f.txt"), "v1\n").unwrap();
        journal
            .append_item(
                1,
                TranscriptItem::User(UserMessage {
                    content: "turn 1".into(),
                }),
            )
            .unwrap();
        let r1 = store
            .capture_revision(&workspace, Some(&r0.revision_id), None, 1)
            .unwrap();
        journal
            .record_revision_capture(
                1,
                r1.revision_id.as_str(),
                Some(r0.revision_id.as_str()),
                r1.changed_paths.clone(),
                r1.tracked_files.len(),
            )
            .unwrap();

        let outcome = fork_resumed_session(&store, &workspace, &mut journal, 0).unwrap();
        assert_eq!(outcome.forked_at_turn, 0);
        assert_eq!(outcome.target_revision, r0.revision_id);
        assert_eq!(fs::read_to_string(ws_dir.join("f.txt")).unwrap(), "v0\n");
        assert_eq!(
            journal.latest_revision_id().as_deref(),
            Some(r0.revision_id.as_str())
        );
        assert_eq!(journal.max_active_turn(), 0);

        // An external edit makes the CLI fork refuse instead of overwriting the user's work.
        fs::write(ws_dir.join("f.txt"), "external\n").unwrap();
        let err = fork_resumed_session(&store, &workspace, &mut journal, 0).unwrap_err();
        assert!(
            err.contains("refusing to fork"),
            "unexpected fork error: {}",
            err
        );
        assert_eq!(
            fs::read_to_string(ws_dir.join("f.txt")).unwrap(),
            "external\n"
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn test_undo_rebinds_the_wal_when_the_undone_revision_shares_the_rewind_leaf() {
        use kroma_core::messages::{TranscriptItem, UserMessage};
        use std::fs;

        let root =
            std::env::temp_dir().join(format!("kroma-cli-undo-leaf-{}", uuid::Uuid::new_v4()));
        let ws_dir = root.join("ws");
        let state_dir = root.join("state");
        fs::create_dir_all(&ws_dir).unwrap();
        fs::create_dir_all(&state_dir).unwrap();
        fs::write(ws_dir.join("f.txt"), "v0\n").unwrap();

        let workspace = WorkspaceCap::new(&ws_dir).unwrap();
        let store = SnapshotStore::new(state_dir.join("snapshots"));
        let mut journal = SessionJournal::create(state_dir.join("sessions"), &ws_dir).unwrap();
        journal
            .append_item(
                0,
                TranscriptItem::User(UserMessage {
                    content: "task".into(),
                }),
            )
            .unwrap();
        let r0 = store.capture_revision(&workspace, None, None, 0).unwrap();
        journal
            .record_revision_capture(
                0,
                r0.revision_id.as_str(),
                None,
                r0.changed_paths.clone(),
                r0.tracked_files.len(),
            )
            .unwrap();

        fs::write(ws_dir.join("f.txt"), "v1\n").unwrap();
        journal
            .append_item(
                1,
                TranscriptItem::User(UserMessage {
                    content: "turn 1".into(),
                }),
            )
            .unwrap();
        let r1 = store
            .capture_revision(&workspace, Some(&r0.revision_id), None, 1)
            .unwrap();
        journal
            .record_revision_capture(
                1,
                r1.revision_id.as_str(),
                Some(r0.revision_id.as_str()),
                r1.changed_paths.clone(),
                r1.tracked_files.len(),
            )
            .unwrap();

        // `--restore-rev r0` re-records r0 at the *current* leaf, i.e. the same turn-1 entry that
        // `--undo` will rewind to.
        let restored = store
            .restore_revision(&workspace, &r0.revision_id, Some(&r1.revision_id), false)
            .unwrap();
        let (restored_files, removed_files) = match &restored {
            RestoreResult::Applied {
                restored_files,
                removed_files,
                ..
            } => (restored_files.clone(), removed_files.clone()),
            other => panic!("expected Applied, got {:?}", other),
        };
        record_restored_revision(
            &mut journal,
            &store,
            &r0.revision_id,
            &restored_files,
            &removed_files,
            Some(&r1.revision_id),
        )
        .unwrap();
        let wal_path = journal.path().to_path_buf();
        drop(journal);

        // `--undo` from here: `current` (r0) and `previous` (r1) share turn 1 and the same leaf.
        let undo_cli = parse_cli_args(vec![
            "kroma".into(),
            "run".into(),
            "--workspace".into(),
            ws_dir.display().to_string(),
            "--resume".into(),
            wal_path.display().to_string(),
            "--undo".into(),
        ])
        .unwrap();
        let _ = run_revision_management(&undo_cli, &state_dir);
        assert_eq!(
            fs::read_to_string(ws_dir.join("f.txt")).unwrap(),
            "v1\n",
            "--undo must restore the previous revision's content"
        );
        let journal = SessionJournal::open(&wal_path).unwrap();
        assert_eq!(
            journal.latest_revision_id().as_deref(),
            Some(r1.revision_id.as_str()),
            "--undo must leave the WAL describing the restored workspace"
        );
        drop(journal);

        // With the WAL in sync, a following `--restore-rev r0` must apply without a false conflict.
        let restore_cli = parse_cli_args(vec![
            "kroma".into(),
            "run".into(),
            "--workspace".into(),
            ws_dir.display().to_string(),
            "--resume".into(),
            wal_path.display().to_string(),
            "--restore-rev".into(),
            r0.revision_id.as_str().to_string(),
        ])
        .unwrap();
        let _ = run_revision_management(&restore_cli, &state_dir);
        assert_eq!(fs::read_to_string(ws_dir.join("f.txt")).unwrap(), "v0\n");
        let _ = fs::remove_dir_all(root);
    }

    /// O5/O6: a snapshot manifest written immediately before a crash that stopped the WAL append is
    /// reconciled on the next `--resume`, and a completed restore emits the `RevisionRestored`
    /// audit event.
    #[test]
    fn test_reconcile_orphan_snapshot_and_emit_revision_restored_event() {
        use kroma_core::messages::{TranscriptItem, UserMessage};
        use std::fs;

        let root =
            std::env::temp_dir().join(format!("kroma-cli-reconcile-{}", uuid::Uuid::new_v4()));
        let ws_dir = root.join("ws");
        let state_dir = root.join("state");
        fs::create_dir_all(&ws_dir).unwrap();
        fs::create_dir_all(&state_dir).unwrap();
        fs::write(ws_dir.join("f.txt"), "v0\n").unwrap();

        let workspace = WorkspaceCap::new(&ws_dir).unwrap();
        let store = SnapshotStore::new(root.join("snapshots"));
        let mut journal = SessionJournal::create(root.join("sessions"), &ws_dir).unwrap();
        journal
            .append_item(
                0,
                TranscriptItem::User(UserMessage {
                    content: "task".into(),
                }),
            )
            .unwrap();
        let baseline = store.capture_revision(&workspace, None, None, 0).unwrap();
        journal
            .record_revision_capture(
                0,
                baseline.revision_id.as_str(),
                None,
                baseline.changed_paths.clone(),
                baseline.tracked_files.len(),
            )
            .unwrap();

        // Snapshot written, then the process dies before the WAL `RevisionCaptured` append.
        fs::write(ws_dir.join("f.txt"), "v1\n").unwrap();
        let orphan = store
            .capture_revision(
                &workspace,
                Some(&baseline.revision_id),
                journal.leaf_id(),
                1,
            )
            .unwrap();
        assert_ne!(orphan.revision_id, baseline.revision_id);
        assert_eq!(
            journal.latest_revision_id().as_deref(),
            Some(baseline.revision_id.as_str()),
            "the WAL still names the stale revision before reconciliation"
        );

        reconcile_store_with_wal(&workspace, &mut journal, &store).unwrap();
        assert_eq!(
            journal.latest_revision_id().as_deref(),
            Some(orphan.revision_id.as_str()),
            "reconciliation must adopt the orphaned manifest"
        );
        assert_eq!(
            journal.revision_records().len(),
            2,
            "reconciliation must not append a duplicate"
        );
        // Reconciling twice is idempotent.
        reconcile_store_with_wal(&workspace, &mut journal, &store).unwrap();
        assert_eq!(journal.revision_records().len(), 2);

        // A completed restore emits the Gate 1 audit event.
        emit_revision_restored(&state_dir, false, &baseline.revision_id, 1, 0);
        let logged = fs::read_dir(&state_dir)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| fs::read_to_string(entry.path()).unwrap_or_default())
            .any(|body| body.contains("\"RevisionRestored\"") && body.contains("restored_files"));
        assert!(
            logged,
            "a completed restore must be recorded in the run event log"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn test_subagent_cli_flags_parse() {
        let default_cli = parse_cli_args(vec![
            "kroma".into(),
            "run".into(),
            "--workspace".into(),
            "./repo".into(),
            "--task".into(),
            "check".into(),
        ])
        .unwrap();
        assert_eq!(
            default_cli.max_subagents,
            kroma_core::subagents::DEFAULT_MAX_SUBAGENT_SPAWNS
        );

        let custom_max = parse_cli_args(vec![
            "kroma".into(),
            "run".into(),
            "--workspace".into(),
            "./repo".into(),
            "--task".into(),
            "check".into(),
            "--max-subagents".into(),
            "3".into(),
        ])
        .unwrap();
        assert_eq!(custom_max.max_subagents, 3);

        let disabled = parse_cli_args(vec![
            "kroma".into(),
            "run".into(),
            "--workspace".into(),
            "./repo".into(),
            "--task".into(),
            "check".into(),
            "--no-subagents".into(),
        ])
        .unwrap();
        assert_eq!(disabled.max_subagents, 0);

        assert!(parse_cli_args(vec![
            "kroma".into(),
            "run".into(),
            "--workspace".into(),
            "./repo".into(),
            "--task".into(),
            "check".into(),
            "--max-subagents".into(),
            "not-a-number".into(),
        ])
        .is_err());
    }

    #[test]
    fn test_no_subagents_outranks_workspace_config() {
        // BUG-09 (`v1.5.1`): `--no-subagents` is an explicit operator capability disable. A
        // workspace `.kroma.toml` (untrusted repo input) may set `max_subagents`, but it must not
        // silently re-enable subagents behind the operator's back.
        let workspace_cfg =
            kroma_core::config_file::KromaConfigFile::parse_str("max_subagents = 5")
                .expect("workspace config parses");
        let user_cfg = kroma_core::config_file::KromaConfigFile::default();
        let env = std::collections::BTreeMap::new();
        let workspace_path = PathBuf::from(".kroma.toml");

        let resolve = |cli: &CliArgs| {
            let overrides = config_overrides_for(cli);
            ResolvedConfig::resolve(
                &user_cfg,
                &workspace_cfg,
                &env,
                &overrides,
                None,
                workspace_path.clone(),
            )
        };

        let disabled = parse_cli_args(vec![
            "kroma".into(),
            "run".into(),
            "--workspace".into(),
            "./repo".into(),
            "--task".into(),
            "check".into(),
            "--no-subagents".into(),
        ])
        .unwrap();
        let disabled_resolved = resolve(&disabled);
        assert_eq!(
            disabled_resolved
                .max_subagents
                .unwrap_or(disabled.max_subagents),
            0,
            "--no-subagents must not be overridden by a workspace-controlled max_subagents"
        );

        // Normal path: without the flag the workspace file's value still applies.
        let default_cli = parse_cli_args(vec![
            "kroma".into(),
            "run".into(),
            "--workspace".into(),
            "./repo".into(),
            "--task".into(),
            "check".into(),
        ])
        .unwrap();
        let default_resolved = resolve(&default_cli);
        assert_eq!(
            default_resolved
                .max_subagents
                .unwrap_or(default_cli.max_subagents),
            5,
            "without --no-subagents the workspace max_subagents must still apply"
        );

        // An explicit --max-subagents still outranks the workspace file.
        let explicit = parse_cli_args(vec![
            "kroma".into(),
            "run".into(),
            "--workspace".into(),
            "./repo".into(),
            "--task".into(),
            "check".into(),
            "--max-subagents".into(),
            "3".into(),
        ])
        .unwrap();
        let explicit_resolved = resolve(&explicit);
        assert_eq!(
            explicit_resolved
                .max_subagents
                .unwrap_or(explicit.max_subagents),
            3,
        );
    }

    #[test]
    fn test_gate6_multi_surface_cli_flags_and_serve_security() {
        let repl = parse_cli_args(vec![
            "kroma".into(),
            "repl".into(),
            "--workspace".into(),
            "./repo".into(),
        ])
        .unwrap();
        assert_eq!(repl.surface, CliSurface::Repl);
        assert!(repl.task.is_none());

        let tui = parse_cli_args(vec![
            "kroma".into(),
            "--workspace".into(),
            "./repo".into(),
            "--interactive".into(),
        ])
        .unwrap();
        assert_eq!(tui.surface, CliSurface::Repl);

        let acp = parse_cli_args(vec![
            "kroma".into(),
            "acp".into(),
            "--workspace".into(),
            "./repo".into(),
        ])
        .unwrap();
        assert_eq!(acp.surface, CliSurface::Acp);

        let serve_loopback = parse_cli_args(vec![
            "kroma".into(),
            "serve".into(),
            "--workspace".into(),
            "./repo".into(),
            "--listen".into(),
            "127.0.0.1:0".into(),
        ])
        .unwrap();
        assert_eq!(
            serve_loopback.surface,
            CliSurface::Serve {
                listen_addr: "127.0.0.1:0".into()
            }
        );

        // Non-loopback bind without --auth-token must be rejected.
        let non_loopback_err = parse_cli_args(vec![
            "kroma".into(),
            "serve".into(),
            "--workspace".into(),
            "./repo".into(),
            "--listen".into(),
            "0.0.0.0:9000".into(),
        ])
        .unwrap_err();
        assert!(non_loopback_err.contains("non-loopback"));

        // Non-loopback bind with --auth-token is accepted.
        let serve_authed = parse_cli_args(vec![
            "kroma".into(),
            "serve".into(),
            "--workspace".into(),
            "./repo".into(),
            "--listen".into(),
            "0.0.0.0:9000".into(),
            "--auth-token".into(),
            "secret-token-123".into(),
        ])
        .unwrap();
        assert_eq!(serve_authed.auth_token.as_deref(), Some("secret-token-123"));

        let jsonl = parse_cli_args(vec![
            "kroma".into(),
            "run".into(),
            "--workspace".into(),
            "./repo".into(),
            "--prompt".into(),
            "inspect".into(),
            "--output-format".into(),
            "jsonl".into(),
        ])
        .unwrap();
        assert_eq!(jsonl.output_format, "jsonl");
        assert_eq!(jsonl.task.as_deref(), Some("inspect"));

        // Conflicting surface flags must be rejected.
        assert!(parse_cli_args(vec![
            "kroma".into(),
            "repl".into(),
            "--workspace".into(),
            "./repo".into(),
            "--listen".into(),
            "127.0.0.1:7878".into(),
        ])
        .is_err());

        // --prompt with acp/serve must be rejected, while --prompt with repl is accepted as initial turn.
        assert!(parse_cli_args(vec![
            "kroma".into(),
            "acp".into(),
            "--workspace".into(),
            "./repo".into(),
            "--prompt".into(),
            "hello".into(),
        ])
        .is_err());
        let repl_with_prompt = parse_cli_args(vec![
            "kroma".into(),
            "repl".into(),
            "--workspace".into(),
            "./repo".into(),
            "--prompt".into(),
            "initial turn".into(),
        ])
        .unwrap();
        assert_eq!(repl_with_prompt.surface, CliSurface::Repl);
        assert_eq!(repl_with_prompt.task.as_deref(), Some("initial turn"));

        // Unix domain socket --listen is accepted without requiring --auth-token.
        let serve_unix = parse_cli_args(vec![
            "kroma".into(),
            "serve".into(),
            "--workspace".into(),
            "./repo".into(),
            "--listen".into(),
            "unix:///tmp/kroma-test.sock".into(),
        ])
        .unwrap();
        assert_eq!(
            serve_unix.surface,
            CliSurface::Serve {
                listen_addr: "unix:///tmp/kroma-test.sock".into()
            }
        );

        // Zero-arg `kroma` (and `kroma --allow-write`) defaults to interactive REPL in the
        // current working directory, while `kroma --task "..."` runs Headless in current directory.
        let zero_arg = parse_cli_args(vec!["kroma".into()]).unwrap();
        assert_eq!(zero_arg.surface, CliSurface::Repl);
        assert!(zero_arg.task.is_none());
        assert_eq!(
            zero_arg.workspace,
            std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
        );

        let flag_only = parse_cli_args(vec!["kroma".into(), "--allow-write".into()]).unwrap();
        assert_eq!(flag_only.surface, CliSurface::Repl);
        assert_eq!(flag_only.policy_mode, PolicyMode::AllowWrite);

        let task_only =
            parse_cli_args(vec!["kroma".into(), "--task".into(), "quick fix".into()]).unwrap();
        assert_eq!(task_only.surface, CliSurface::Headless);
        assert_eq!(task_only.task.as_deref(), Some("quick fix"));
    }

    #[test]
    fn test_serve_help_text_and_non_loopback_warning() {
        let help = usage_text();
        assert!(help.contains("--auth-token <TOKEN>"));
        assert!(help.contains("plaintext (no TLS)"));
        assert!(help.contains("JSON-RPC parameters"));
        assert!(help.contains("trusted network"));

        let non_loopback_ep = kroma_core::surface::validate_daemon_listen_endpoint(
            "0.0.0.0:9000",
            Some("super-secret-auth-token"),
        )
        .expect("non-loopback with auth token should validate");
        assert!(!non_loopback_ep.is_loopback());
        let warning = non_loopback_ep.non_loopback_warning();
        assert!(warning.is_some());
        let text = warning.unwrap();
        assert!(text.contains("plaintext (no TLS)"));
        assert!(text.contains("JSON-RPC parameters"));
        assert!(text.contains("trusted network"));
        assert!(!text.contains("super-secret-auth-token"));

        let loopback_ep =
            kroma_core::surface::validate_daemon_listen_endpoint("127.0.0.1:7878", None)
                .expect("loopback without token should validate");
        assert!(loopback_ep.is_loopback());
        assert_eq!(loopback_ep.non_loopback_warning(), None);

        let unix_ep =
            kroma_core::surface::validate_daemon_listen_endpoint("unix:///tmp/test.sock", None)
                .expect("unix endpoint should validate");
        assert!(unix_ep.is_loopback());
        assert_eq!(unix_ep.non_loopback_warning(), None);
    }

    #[test]
    fn test_max_tokens_flag_feeds_the_unified_token_cap() {
        let parsed = parse_cli_args(vec![
            "kroma".into(),
            "run".into(),
            "--task".into(),
            "fix".into(),
            "--max-tokens".into(),
            "12345".into(),
        ])
        .unwrap();
        assert_eq!(parsed.max_tokens, Some(12345));
        assert_eq!(config_overrides_for(&parsed).max_tokens, Some(12345));

        // A zero cap is rejected instead of silently disabling every request.
        let err = parse_cli_args(vec![
            "kroma".into(),
            "run".into(),
            "--task".into(),
            "fix".into(),
            "--max-tokens".into(),
            "0".into(),
        ])
        .unwrap_err();
        assert!(err.contains("--max-tokens must be > 0"), "got: {}", err);

        // Absent the flag, the override stays unset so config/env can supply the cap.
        let no_flag = parse_cli_args(vec![
            "kroma".into(),
            "run".into(),
            "--task".into(),
            "fix".into(),
        ])
        .unwrap();
        assert_eq!(no_flag.max_tokens, None);
        assert_eq!(config_overrides_for(&no_flag).max_tokens, None);
    }

    #[test]
    fn test_causal_pruning_flag_is_opt_in() {
        // Capability gap #3 (`v1.5.2`): causal pruning has a production entry point (`--causal-pruning`
        // on both the headless and `serve` paths) and stays off unless the operator asks for it.
        let parsed = parse_cli_args(vec![
            "kroma".into(),
            "run".into(),
            "--task".into(),
            "fix".into(),
            "--causal-pruning".into(),
        ])
        .unwrap();
        assert!(parsed.causal_pruning, "the flag must enable causal pruning");

        let default = parse_cli_args(vec![
            "kroma".into(),
            "run".into(),
            "--task".into(),
            "fix".into(),
        ])
        .unwrap();
        assert!(
            !default.causal_pruning,
            "causal pruning must be opt-in, not a silent default"
        );
    }

    #[test]
    fn test_mcp_server_flag_parses_into_a_trusted_config() {
        // Capability gap #6 (`v1.5.2`): `--mcp-server NAME=CMD ARGS` is the explicit, trusted way to
        // add an MCP server; a malformed value is rejected instead of silently ignored.
        let parsed = parse_cli_args(vec![
            "kroma".into(),
            "serve".into(),
            "--mcp-server".into(),
            "mock=python3 /tmp/server.py --flag".into(),
        ])
        .unwrap();
        assert_eq!(parsed.mcp_servers.len(), 1);
        assert_eq!(parsed.mcp_servers[0].name, "mock");
        assert_eq!(parsed.mcp_servers[0].command, "python3");
        assert_eq!(
            parsed.mcp_servers[0].args,
            vec!["/tmp/server.py".to_string(), "--flag".to_string()]
        );
        assert!(!parsed.mcp_servers[0].read_only);
        assert_eq!(config_overrides_for(&parsed).mcp_servers.len(), 1);

        assert!(
            parse_cli_args(vec![
                "kroma".into(),
                "serve".into(),
                "--mcp-server".into(),
                "no-equals".into(),
            ])
            .is_err(),
            "a malformed --mcp-server value must be rejected"
        );
    }

    /// Capability gap #6 (`v1.5.2`): the headless `kroma run` path must actually mount a trusted
    /// MCP server (not merely accept the flag). This drives the same `mount_trusted_mcp` helper the
    /// headless path calls, against a real MCP stdio server.
    #[tokio::test]
    async fn test_headless_mounts_a_trusted_mcp_server() {
        const MOCK_SERVER: &str = r#"
import sys, json

def respond(msg_id, result):
    sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": msg_id, "result": result}) + "\n")
    sys.stdout.flush()

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    msg = json.loads(line)
    method = msg.get("method")
    if method == "initialize":
        respond(msg["id"], {"protocolVersion": "2024-11-05", "capabilities": {},
                            "serverInfo": {"name": "mock", "version": "1"}})
    elif method == "notifications/initialized":
        pass
    elif method == "tools/list":
        respond(msg["id"], {"tools": [{"name": "echo", "description": "echo",
                                       "inputSchema": {"type": "object"}}]})
    elif method == "tools/call":
        respond(msg["id"], {"content": [{"type": "text", "text": "ok"}]})
    elif "id" in msg:
        respond(msg["id"], {})
"#;
        let dir = std::env::temp_dir().join(format!("kroma-cli-mcp-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let server = dir.join("mock_mcp.py");
        std::fs::write(&server, MOCK_SERVER).unwrap();

        let servers = vec![kroma_core::config_file::McpServerConfig {
            name: "mock".to_string(),
            command: "python3".to_string(),
            args: vec![server.display().to_string()],
            env: Vec::new(),
            read_only: false,
        }];
        let mut registry = ToolRegistry::new(None);
        let mounted = mount_trusted_mcp(&mut registry, &servers, true).unwrap();
        assert_eq!(mounted, Some(1), "the mock server exposes one tool");

        let policy = PolicyEngine::new(
            PolicyMode::FullAutonomous,
            Arc::new(kroma_core::tool::policy::NonInteractiveApprovalHandler),
        );
        let names: Vec<String> = registry
            .visible_definitions_for_engine(&policy)
            .into_iter()
            .map(|d| d.name)
            .collect();
        assert!(
            names.iter().any(|n| n == "mcp__mock__echo"),
            "the headless registry must expose the MCP tool, got {:?}",
            names
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}

// Removed 2026-10-14: a `BionicTlsPad` thread_local and two helpers used to sit here, added by
// 56fb681 ("enforce TLS segment p_align >= 64 for Android ARM64 Bionic"). They never did
// anything -- nothing called either helper, so the thread_local and both functions were stripped,
// and the released android binary has no PT_TLS segment at all whose alignment could be wrong.
// Their only observable effect was an unguarded `#[link(name = "ssl")]`, which forced every
// target to link OpenSSL and left the Windows release build broken for three months.
//
// If a bionic TLS-alignment problem does show up, confirm it against the built artifact
// (`readelf -lW kroma | grep TLS`; p_align must be >= 64) instead of adding padding the linker
// may never emit. CI checks the android artifact for exactly that condition.
