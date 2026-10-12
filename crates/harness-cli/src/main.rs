pub mod approval_ui;

use approval_ui::TtyApprovalUi;
use harness_core::agent_loop::{ build_runtime_with_registry, AgentRunConfig, AgentRunner };
use harness_core::budget::BudgetConfig;
use harness_core::completion_gate::{configured_timeout, parse_command_argv, WorkspaceCompletionGate};
use harness_core::env::{AllowAllGate, ExecutionEnv, LocalHostEnv, SharedCompletionGate};
use harness_core::events::{ resolve_default_state_dir, EventLogger, EventPayload };
use harness_core::prompt::PromptBuilder;
use harness_core::revision::{RestoreResult, RevisionId, SnapshotStore};
use harness_core::session::SessionJournal;
use harness_core::workspace::WorkspaceCap;
use harness_core::provider::chat_completions::{ ChatCompletionsClient, ChatCompletionsConfig };
use harness_core::extensions::{
    discover_skills, CommandObserverHook, CommandPolicyHook, ExtensionManifest, ScopedExtension,
    SubtreeContextTracker,
};
use harness_core::tool::policy::{ PolicyEngine, PolicyMode };
use harness_core::tool::registry::ToolRegistry;
use harness_core::types::RunStatus;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::SystemTime;
use tokio::sync::watch;

#[derive(Debug, Clone)]
pub struct CliArgs {
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
            "--predict-cmd" => {
                let val = iter
                    .next()
                    .ok_or_else(|| "missing value for --predict-cmd <CMD>".to_string())?;
                if val.trim().is_empty() {
                    return Err("empty value for --predict-cmd".to_string());
                }
                predict_cmd = Some(val);
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
    if !management && !list_skills && task.is_none() {
        return Err("missing required --task <prompt>".to_string());
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
    if !raw
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir | std::path::Component::RootDir))
    {
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

/// Exit code returned when a restore would overwrite work done outside the harness.
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
    let run_id = harness_core::types::RunId::new();
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
/// Delegates to [`SnapshotStore::reconcile_with_wal`] so the CLI and the harness-core acceptance
/// tests exercise exactly the same production reconciliation path.
fn reconcile_store_with_wal(journal: &mut SessionJournal, store: &SnapshotStore) {
    let _ = store.reconcile_with_wal(journal);
}

fn snapshot_store_for(state_dir: &Path) -> SnapshotStore {
    SnapshotStore::new(state_dir.join("snapshots"))
}

/// Gate 3: `--list-skills` discovers the workspace skill catalog and exits without a provider call.
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
            harness_core::extensions::SKILL_DIR
        );
        return ExitCode::SUCCESS;
    }
    println!("skills for {} ({} discovered):", cli.workspace.display(), skills.len());
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
    let mut manifest = ExtensionManifest::operator_trusted("cli-hooks", "--policy-hook/--observer-hook");
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
        ext.observer_hooks
            .push(Arc::new(CommandObserverHook::new("cli-observer-hook", argv)));
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
    // of the WAL; reconcile before reading `latest_revision_id()` so the two agree.
    reconcile_store_with_wal(&mut journal, &store);

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

    // An interrupted restore leaves its intent journal behind. Finishing that restore is the only
    // safe action: applying a *different* target on top of a half-applied tree would compound it.
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
        Ok(None) => store.restore_revision(&workspace, &target, expected.as_ref(), cli.force_restore),
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
            if let Some(turn) = rewind_turn {
                match journal.fork_at_turn(turn) {
                    Ok(leaf) => println!("rewound session transcript to turn {} (leaf {})", turn, leaf),
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
                if journal.latest_revision_id().as_deref()
                    != Some(restored_revision.as_str())
                {
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
            } else if let Err(e) = record_restored_revision(
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
            ExitCode::SUCCESS
        }
        RestoreResult::Conflict {
            target_revision,
            conflicting_paths,
        } => {
            eprintln!(
                "error: refusing to restore {}; {} path(s) changed outside the harness (pass --force-restore to overwrite)",
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

Extensions / Hooks / Skills (Gate 3):
  --list-skills             Discover and print .agents/skills/*/SKILL.md, then exit
  --skill <NAME>            Pre-activate a named skill in the initial untrusted context
  --policy-hook <CMD>       Fail-closed PreToolUse policy hook (non-zero exit or timeout denies)
  --observer-hook <CMD>     Fail-open PostToolUse observer hook (failures are logged, never block)
  --no-subtree-context      Disable automatic JIT subtree AGENTS.md/CLAUDE.md loading

Predictive Validation / Attribution (Gate 4):
  --no-predictive-check     Disable the post-edit/write structural (syntax/delimiter) check
  --predict-cmd <CMD>       Fast command run after a mutating batch to surface failures early

Workspace Revision Management (requires --resume):
  --undo                    Restore the workspace and session leaf to the previous turn's revision
  --restore-rev <REV_ID>    Restore the workspace to a specific captured revision
  --force-restore           Overwrite external modifications during --undo / --restore-rev
  --list-revisions          List the workspace revisions captured for the session

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
  granting --allow-shell executes commands under the current OS user without an OS container sandbox.

Exit codes:
  0 success, 1 run failure, 2 invalid invocation, 3 restore conflict, 4 restore recovery required."
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

    let state_dir = resolve_default_state_dir();

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
    let env = match LocalHostEnv::new(&cli.workspace, state_dir.clone()) {
        Ok(e) => Arc::new(e),
        Err(e) => {
            eprintln!("error: failed to initialize workspace capability: {}", e);
            return ExitCode::from(2);
        }
    };

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
            eprintln!("error: session WAL belongs to workspace {}, not {}", journal.workspace_root().display(), env.workspace().root().display());
            return ExitCode::from(2);
        }
        // O6: a crash between the snapshot write and the WAL append leaves the store one revision
        // ahead of the WAL. Reconcile before `--fork` so a resumed run never starts from a stale
        // `latest_revision_id()`.
        reconcile_store_with_wal(&mut journal, &snapshot_store_for(&state_dir));
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

    let mut run_cfg = AgentRunConfig::new(cli.task.clone().unwrap_or_default(), budget.clone());
    run_cfg.stream = !cli.no_stream;
    run_cfg.strict_log = cli.strict_log;

    let mut logger = match EventLogger::new(run_cfg.run_id, Some(state_dir.clone()), cli.strict_log) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("error: {}", e);
            return ExitCode::from(1);
        }
    };
    logger.set_live_stderr(!cli.quiet);

    let approval_ui = Arc::new(TtyApprovalUi::new());
    let policy_engine = PolicyEngine::new(cli.policy_mode, approval_ui);

    // Gate 3: the registry carries the reversible extension mount points, the workspace skill
    // catalog, and the CLI-declared hooks.
    let registry = ToolRegistry::new(cli.tool_filter.clone()).with_workspace_skills(workspace_skills);
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
                        eprintln!("[harness] mounted extension: {}", name);
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

    // Gate 3: subtree JIT context. `--fork <TURN>` has already rewound the session leaf to a turn
    // `<= TURN`, so `latest_active_checkpoint()` returns the checkpoint on the *rewound* branch: the
    // directories whose guidance is still present in that branch's transcript must be restored for
    // a fork exactly as for a plain resume, or they would be re-injected on touch.
    let mut subtree_tracker = SubtreeContextTracker::new(!cli.no_subtree_context);
    if let Some(checkpoint) = session.latest_active_checkpoint() {
        subtree_tracker.restore_loaded_dirs(checkpoint.subtree_dirs.clone());
    }

    let runtime = build_runtime_with_registry(registry, env, policy_engine, budget)
        .with_predictive_check(!cli.no_predictive_check);

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
    let snapshot_store = Arc::new(snapshot_store_for(&state_dir));
    if !cli.quiet {
        eprintln!("[harness] workspace revisions: {}", snapshot_store.root().display());
    }
    let runner = AgentRunner::new(run_cfg, provider, runtime, cancel_rx)
        .with_prompt_builder(prompt_builder)
        .with_session_journal(session)
        .with_snapshot_store(snapshot_store)
        .with_subtree_context(subtree_tracker)
        .with_completion_gate(build_completion_gate(gate_choice))
        .with_completion_gate_max_retries(max_gate_retries);
    // Gate 4: `--predict-cmd` is an explicit operator authorization, exactly like `--verify-cmd`,
    // so it runs without needing `--allow-shell`.
    let runner = match cli.predict_cmd.as_deref() {
        Some(raw) => match parse_command_argv(raw) {
            Ok(argv) if !argv.is_empty() => {
                if !cli.quiet {
                    eprintln!("[harness] predictive check: {}", argv.join(" "));
                }
                runner.with_predictive_command(Arc::new(WorkspaceCompletionGate::with_auto_detection(
                    Some(argv),
                    configured_timeout(),
                    false,
                )))
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
    fn test_workspace_revision_management_flags_parse() {
        // `--list-revisions` and `--undo` are standalone actions: no --task required.
        let listed = parse_cli_args(vec![
            "harness".into(), "run".into(), "--workspace".into(), "./repo".into(),
            "--resume".into(), "session.wal.jsonl".into(), "--list-revisions".into(),
        ])
        .unwrap();
        assert!(listed.list_revisions);
        assert!(listed.task.is_none());
        assert!(listed.is_revision_management());

        let undo = parse_cli_args(vec![
            "harness".into(), "run".into(), "--workspace".into(), "./repo".into(),
            "--resume".into(), "session.wal.jsonl".into(), "--undo".into(),
            "--force-restore".into(),
        ])
        .unwrap();
        assert!(undo.undo);
        assert!(undo.force_restore);

        let restore = parse_cli_args(vec![
            "harness".into(), "run".into(), "--workspace".into(), "./repo".into(),
            "--resume".into(), "session.wal.jsonl".into(),
            "--restore-rev".into(), "r-0123456789abcdef0123456789abcdef".into(),
        ])
        .unwrap();
        assert_eq!(
            restore.restore_rev.as_deref(),
            Some("r-0123456789abcdef0123456789abcdef")
        );

        // A normal run still requires --task.
        assert!(parse_cli_args(vec![
            "harness".into(), "run".into(), "--workspace".into(), "./repo".into(),
        ])
        .unwrap_err()
        .contains("--task"));

        // Management actions need a session to act on.
        assert!(parse_cli_args(vec![
            "harness".into(), "run".into(), "--workspace".into(), "./repo".into(),
            "--undo".into(),
        ])
        .unwrap_err()
        .contains("require --resume"));

        // Conflicting or dangling combinations are rejected rather than guessed at.
        assert!(parse_cli_args(vec![
            "harness".into(), "run".into(), "--workspace".into(), "./repo".into(),
            "--resume".into(), "session.wal.jsonl".into(), "--undo".into(),
            "--restore-rev".into(), "r-0123456789abcdef0123456789abcdef".into(),
        ])
        .unwrap_err()
        .contains("mutually exclusive"));
        assert!(parse_cli_args(vec![
            "harness".into(), "run".into(), "--workspace".into(), "./repo".into(),
            "--resume".into(), "session.wal.jsonl".into(), "--undo".into(), "--fork".into(), "2".into(),
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

    /// `--restore-rev` must bind the restored revision into the WAL, so a following `--undo`
    /// compares against the revision that is actually on disk instead of reporting a false conflict.
    #[test]
    fn test_restore_rev_records_revision_so_a_following_undo_stays_in_sync() {
        use harness_core::messages::{TranscriptItem, UserMessage};
        use std::fs;

        let root = std::env::temp_dir().join(format!(
            "harness-cli-restore-rev-{}",
            uuid::Uuid::new_v4()
        ));
        let ws_dir = root.join("ws");
        fs::create_dir_all(&ws_dir).unwrap();
        fs::write(ws_dir.join("f.txt"), "v0\n").unwrap();

        let workspace = WorkspaceCap::new(&ws_dir).unwrap();
        let store = SnapshotStore::new(root.join("snapshots"));
        let mut journal = SessionJournal::create(root.join("sessions"), &ws_dir).unwrap();

        journal
            .append_item(0, TranscriptItem::User(UserMessage { content: "task".into() }))
            .unwrap();
        let r0 = store.capture_revision(&workspace, None, None, 0).unwrap();
        journal
            .record_revision_capture(0, r0.revision_id.as_str(), None, r0.changed_paths.clone(), r0.tracked_files.len())
            .unwrap();

        fs::write(ws_dir.join("f.txt"), "v1\n").unwrap();
        journal
            .append_item(1, TranscriptItem::User(UserMessage { content: "turn 1".into() }))
            .unwrap();
        let r1 = store
            .capture_revision(&workspace, Some(&r0.revision_id), None, 1)
            .unwrap();
        journal
            .record_revision_capture(1, r1.revision_id.as_str(), Some(r0.revision_id.as_str()), r1.changed_paths.clone(), r1.tracked_files.len())
            .unwrap();

        // `--restore-rev r0`: the expected revision is the session's latest recorded one.
        let restored = store
            .restore_revision(&workspace, &r0.revision_id, Some(&r1.revision_id), false)
            .unwrap();
        let (restored_files, removed_files) = match &restored {
            RestoreResult::Applied { restored_files, removed_files, .. } => {
                (restored_files.clone(), removed_files.clone())
            }
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

    /// Gate 2 audit (Finding 1): when the *undone* revision is bound to the same leaf the rewind
    /// selects — which happens after a `--restore-rev` re-records it at the current leaf — `--undo`
    /// must re-bind the restored revision into the WAL. Otherwise `latest_revision_id()` keeps
    /// naming the undone revision and the next `--restore-rev` fails with a false
    /// `ModifiedExternally` conflict.
    #[test]
    fn test_undo_rebinds_the_wal_when_the_undone_revision_shares_the_rewind_leaf() {
        use harness_core::messages::{TranscriptItem, UserMessage};
        use std::fs;

        let root = std::env::temp_dir().join(format!(
            "harness-cli-undo-leaf-{}",
            uuid::Uuid::new_v4()
        ));
        let ws_dir = root.join("ws");
        let state_dir = root.join("state");
        fs::create_dir_all(&ws_dir).unwrap();
        fs::create_dir_all(&state_dir).unwrap();
        fs::write(ws_dir.join("f.txt"), "v0\n").unwrap();

        let workspace = WorkspaceCap::new(&ws_dir).unwrap();
        let store = SnapshotStore::new(state_dir.join("snapshots"));
        let mut journal = SessionJournal::create(state_dir.join("sessions"), &ws_dir).unwrap();
        journal
            .append_item(0, TranscriptItem::User(UserMessage { content: "task".into() }))
            .unwrap();
        let r0 = store.capture_revision(&workspace, None, None, 0).unwrap();
        journal
            .record_revision_capture(0, r0.revision_id.as_str(), None, r0.changed_paths.clone(), r0.tracked_files.len())
            .unwrap();

        fs::write(ws_dir.join("f.txt"), "v1\n").unwrap();
        journal
            .append_item(1, TranscriptItem::User(UserMessage { content: "turn 1".into() }))
            .unwrap();
        let r1 = store
            .capture_revision(&workspace, Some(&r0.revision_id), None, 1)
            .unwrap();
        journal
            .record_revision_capture(1, r1.revision_id.as_str(), Some(r0.revision_id.as_str()), r1.changed_paths.clone(), r1.tracked_files.len())
            .unwrap();

        // `--restore-rev r0` re-records r0 at the *current* leaf, i.e. the same turn-1 entry that
        // `--undo` will rewind to.
        let restored = store
            .restore_revision(&workspace, &r0.revision_id, Some(&r1.revision_id), false)
            .unwrap();
        let (restored_files, removed_files) = match &restored {
            RestoreResult::Applied { restored_files, removed_files, .. } => {
                (restored_files.clone(), removed_files.clone())
            }
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
            "harness".into(), "run".into(),
            "--workspace".into(), ws_dir.display().to_string(),
            "--resume".into(), wal_path.display().to_string(),
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
            "harness".into(), "run".into(),
            "--workspace".into(), ws_dir.display().to_string(),
            "--resume".into(), wal_path.display().to_string(),
            "--restore-rev".into(), r0.revision_id.as_str().to_string(),
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
        use harness_core::messages::{TranscriptItem, UserMessage};
        use std::fs;

        let root = std::env::temp_dir().join(format!(
            "harness-cli-reconcile-{}",
            uuid::Uuid::new_v4()
        ));
        let ws_dir = root.join("ws");
        let state_dir = root.join("state");
        fs::create_dir_all(&ws_dir).unwrap();
        fs::create_dir_all(&state_dir).unwrap();
        fs::write(ws_dir.join("f.txt"), "v0\n").unwrap();

        let workspace = WorkspaceCap::new(&ws_dir).unwrap();
        let store = SnapshotStore::new(root.join("snapshots"));
        let mut journal = SessionJournal::create(root.join("sessions"), &ws_dir).unwrap();
        journal
            .append_item(0, TranscriptItem::User(UserMessage { content: "task".into() }))
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

        reconcile_store_with_wal(&mut journal, &store);
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
        reconcile_store_with_wal(&mut journal, &store);
        assert_eq!(journal.revision_records().len(), 2);

        // A completed restore emits the Gate 1 audit event.
        emit_revision_restored(&state_dir, false, &baseline.revision_id, 1, 0);
        let logged = fs::read_dir(&state_dir)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| fs::read_to_string(entry.path()).unwrap_or_default())
            .any(|body| body.contains("\"RevisionRestored\"") && body.contains("restored_files"));
        assert!(logged, "a completed restore must be recorded in the run event log");
        let _ = fs::remove_dir_all(root);
    }
}

