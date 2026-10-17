use crate::budget::BudgetConfig;
use crate::types::{
    EditDiagKind, EditMatchTier, EventSeq, RunId, RunStatus, TerminalReason, ToolCallId,
};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;

pub const EVENT_SCHEMA_VERSION: &str = "1.0.0";

pub type EventSink = std::sync::Arc<dyn Fn(&DiagnosticEvent) + Send + Sync>;

#[derive(Debug, Error)]
pub enum EventLogError {
    #[error("strict event logging failed: {0}")]
    StrictIoFailure(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event_type", content = "payload")]
pub enum EventPayload {
    RunStarted {
        prompt_hash: String,
        tool_schema_hash: String,
        static_prefix_bytes: usize,
        policy: String,
        budget_config: BudgetConfig,
    },
    ModelRequestStarted {
        turn: usize,
        attempt: usize,
        request_body_bytes: usize,
        message_history_bytes: usize,
    },
    ModelRequestCompleted {
        turn: usize,
        /// None for historical records; never infer measured usage from an old missing label.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage_basis: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reported_model: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        token_accounting: Option<crate::budget::TokenAccountingSnapshot>,
        input_tokens: u64,
        cached_input_tokens: Option<u64>,
        uncached_input_tokens: u64,
        output_tokens: u64,
        reasoning_tokens: Option<u64>,
        prefix_cache_hit_ratio: f64,
        latency_ms: u64,
        ttft_ms: Option<u64>,
    },
    ToolBatchReceived {
        turn: usize,
        call_count: usize,
        call_ids: Vec<ToolCallId>,
    },
    /// T03 (KR-07): the whole request would not fit the model window, so it was never sent.
    /// Recorded structurally so the operator can see the estimate, the reservation and the source
    /// of the window instead of only a terminal reason.
    ContextBudgetExceeded {
        estimated_input_tokens: u64,
        reserved_output_tokens: u64,
        window_tokens: u64,
        capability_source: String,
        tool_schema_bytes: usize,
        message_count: usize,
    },
    PolicyDecision {
        call_id: ToolCallId,
        tool_name: String,
        decision: String,
        reason: Option<String>,
    },
    ToolStarted {
        call_id: ToolCallId,
        tool_name: String,
        args_sha256: String,
        relative_path: Option<String>,
    },
    ToolCompleted {
        call_id: ToolCallId,
        tool_name: String,
        relative_path: Option<String>,
        duration_ms: u64,
        truncated: bool,
        exit_code: Option<i32>,
        outcome: String,
        edit_match_tier: Option<EditMatchTier>,
        diag_kind: Option<EditDiagKind>,
        compaction_reread_count: u64,
    },
    ContextCompacted {
        before_bytes: usize,
        after_bytes: usize,
        compacted_seq_indices: Vec<usize>,
        pinned_files: Vec<String>,
    },
    /// A `WorkspaceRevision` snapshot was captured and recorded in the session WAL (Gate 1).
    RevisionCaptured {
        turn: usize,
        revision_id: String,
        parent_revision_id: Option<String>,
        changed_paths: Vec<String>,
        tracked_file_count: usize,
        /// `baseline` for the pre-mutation capture, `mutation` for a post-tool-batch capture.
        trigger: String,
    },
    /// The workspace was rewound to a captured revision by `--undo` / `--restore-rev` / crash
    /// recovery (Gate 1 audit trail).
    RevisionRestored {
        revision_id: String,
        restored_files: usize,
        removed_files: usize,
    },
    LoopGuardTriggered {
        call_id: ToolCallId,
        tool_name: String,
        guard_kind: String,
        signature: String,
    },
    /// A Gate 3 hook was evaluated. `hook_kind` is `policy` or `observer`; `outcome` is one of
    /// `allow` | `deny` | `ok` | `failed_open` | `timeout` | `panicked`.
    HookEvaluated {
        hook_id: String,
        hook_kind: String,
        event: String,
        outcome: String,
        duration_ms: u64,
        message: Option<String>,
    },
    /// A Gate 3 extension was mounted or unmounted. `action` is `registered` or `unregistered`.
    ExtensionChanged {
        extension_id: String,
        action: String,
    },
    /// Gate 3: ancestor-directory `AGENTS.md`/`CLAUDE.md` guidance was injected into a tool result.
    SubtreeContextLoaded {
        directory: String,
        file_name: String,
        bytes: usize,
    },
    /// Gate 4: a post-mutation structural check ran. `status` is `ok` | `warning` | `skipped`.
    PredictiveCheck {
        path: String,
        status: String,
        issue_count: usize,
        duration_ms: u64,
        summary: Option<String>,
    },
    /// Gate 4: a failure was ranked against the session mutation ledger. `source` is
    /// `completion_gate` or `predict_cmd`; the report is always a *candidate* attribution.
    FailureAttributed {
        source: String,
        candidate_count: usize,
        top_mutation_id: Option<String>,
        top_path: Option<String>,
        top_confidence: Option<f64>,
        top_revision_before: Option<String>,
    },
    /// Gate 5: a bounded child subagent was spawned (`scout` or `worker`).
    SubagentSpawned {
        subagent_id: String,
        role: String,
        max_turns: usize,
        isolated_worktree: bool,
    },
    /// Gate 5: a child subagent finished (or was cancelled/timed out).
    SubagentCompleted {
        subagent_id: String,
        role: String,
        status: String,
        turns_used: usize,
        tool_calls_used: usize,
        tokens_used: u64,
        duration_ms: u64,
    },
    /// Observed tournament activity, not merely an enabled strategy flag. Candidate runs count
    /// actual child loops with at least one provider turn; zero-budget refusals are not runs.
    TournamentCompleted {
        adaptive: bool,
        candidate_runs: usize,
        verified_candidates: usize,
        merged: bool,
    },
    /// Gate 5: an isolated `worker` worktree merge was evaluated against the parent workspace.
    WorktreeMerged {
        subagent_id: String,
        status: String,
        written_files: Vec<String>,
        removed_files: Vec<String>,
        conflict_paths: Vec<String>,
        revision_after: Option<String>,
    },
    /// Gate 6: a runtime surface (`headless`, `repl`, `acp`, `serve`) started on `transport`
    /// (`stdio`, `tty`, `tcp`, `unix`, `inproc`).
    SurfaceStarted { surface: String, transport: String },
    /// Gate 6: a tool call paused awaiting operator approval on `surface`.
    ApprovalRequested {
        request_id: String,
        tool_name: String,
        surface: String,
    },
    /// Gate 6: a pending approval request was resolved (`approve_once`, `approve_session`,
    /// `deny`, `deny_timeout`, `deny_disconnect`, `deny_cancelled`).
    ApprovalResolved {
        request_id: String,
        decision: String,
    },
    RunTerminated {
        status: RunStatus,
        terminal_reason: TerminalReason,
        turns: usize,
        total_tool_calls: usize,
        total_input_tokens: u64,
        total_cached_input_tokens: u64,
        total_output_tokens: u64,
        cache_hit_ratio: f64,
        compaction_reread_count: u64,
        duration_ms: u64,
        log_degraded: bool,
        dropped_events: usize,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagnosticEvent {
    pub schema_version: String,
    pub run_id: RunId,
    pub seq: u64,
    pub timestamp_ms: u64,
    #[serde(flatten)]
    pub event: EventPayload,
}

/// Resolve the OS user state directory for Kroma runs without polluting the target workspace.
pub fn resolve_default_state_dir() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_STATE_HOME") {
        if !xdg.trim().is_empty() {
            return PathBuf::from(xdg).join("kroma").join("runs");
        }
    }
    if cfg!(target_os = "macos") {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home)
                .join("Library")
                .join("Application Support")
                .join("kroma")
                .join("runs");
        }
    }
    if cfg!(windows) {
        if let Ok(local_appdata) =
            std::env::var("LOCALAPPDATA").or_else(|_| std::env::var("APPDATA"))
        {
            if !local_appdata.trim().is_empty() {
                return PathBuf::from(local_appdata).join("kroma").join("runs");
            }
        }
        if let Ok(profile) = std::env::var("USERPROFILE") {
            if !profile.trim().is_empty() {
                return PathBuf::from(profile)
                    .join("AppData")
                    .join("Local")
                    .join("kroma")
                    .join("runs");
            }
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        if !home.trim().is_empty() {
            return PathBuf::from(home)
                .join(".local")
                .join("state")
                .join("kroma")
                .join("runs");
        }
    }
    std::env::temp_dir().join("kroma-state").join("runs")
}

pub struct EventLogger {
    run_id: RunId,
    seq: EventSeq,
    state_dir: PathBuf,
    log_path: PathBuf,
    file: Option<File>,
    strict_log: bool,
    live_stderr: bool,
    log_degraded: bool,
    dropped_events: usize,
    in_memory_events: Vec<DiagnosticEvent>,
    event_sink: Option<EventSink>,
}

impl EventLogger {
    pub fn new(
        run_id: RunId,
        state_dir_override: Option<PathBuf>,
        strict_log: bool,
    ) -> Result<Self, EventLogError> {
        let state_dir = state_dir_override.unwrap_or_else(resolve_default_state_dir);
        let log_path = state_dir.join(format!("{}.jsonl", run_id));

        let mut log_degraded = false;
        let mut file = None;

        match Self::open_log_file(&state_dir, &log_path) {
            Ok(f) => {
                file = Some(f);
            }
            Err(e) => {
                let msg = format!(
                    "failed to open diagnostic log at {}: {}",
                    log_path.display(),
                    e
                );
                if strict_log {
                    return Err(EventLogError::StrictIoFailure(msg));
                }
                eprintln!("[WARN kroma] diagnostic logging degraded: {}", msg);
                log_degraded = true;
            }
        }

        Ok(Self {
            run_id,
            seq: EventSeq::new(0),
            state_dir,
            log_path,
            file,
            strict_log,
            live_stderr: false,
            log_degraded,
            dropped_events: 0,
            in_memory_events: Vec::new(),
            event_sink: None,
        })
    }

    pub fn set_live_stderr(&mut self, live_stderr: bool) {
        self.live_stderr = live_stderr;
    }

    pub fn set_event_sink(&mut self, sink: EventSink) {
        self.event_sink = Some(sink);
    }

    pub fn live_stderr(&self) -> bool {
        self.live_stderr
    }

    fn open_log_file(state_dir: &Path, log_path: &Path) -> std::io::Result<File> {
        fs::create_dir_all(state_dir)?;
        let mut opts = OpenOptions::new();
        opts.create(true).append(true);
        crate::secure_fs::apply_no_follow(&mut opts);
        #[cfg(unix)]
        {
            // `fchmod` the descriptor we actually opened: a path-based `chmod` would follow a
            // symlink swapped onto the log path and restrict an arbitrary target instead.
            opts.mode(0o600);
            let f = opts.open(log_path)?;
            f.set_permissions(fs::Permissions::from_mode(0o600))?;
            Ok(f)
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};
            opts.share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE);
            opts.open(log_path)
        }
    }

    pub fn log(&mut self, payload: EventPayload) -> Result<EventSeq, EventLogError> {
        if self.live_stderr {
            match &payload {
                EventPayload::RunStarted {
                    static_prefix_bytes,
                    policy,
                    ..
                } => {
                    eprintln!(
                        "🚀 [kroma] Run {} started | policy={} | static_prefix={}B",
                        self.run_id, policy, static_prefix_bytes
                    );
                }
                EventPayload::ModelRequestStarted {
                    turn,
                    attempt,
                    request_body_bytes,
                    message_history_bytes,
                } => {
                    eprintln!(
                        "⏳ [turn {}] requesting model (attempt {}, history={}B, body={}B)...",
                        turn, attempt, message_history_bytes, request_body_bytes
                    );
                }
                EventPayload::ModelRequestCompleted {
                    turn,
                    input_tokens,
                    cached_input_tokens,
                    output_tokens,
                    reasoning_tokens,
                    prefix_cache_hit_ratio,
                    latency_ms,
                    ttft_ms,
                    ..
                } => {
                    let ttft = match ttft_ms {
                        Some(ms) => format!("{}ms", ms),
                        None => "n/a".to_string(),
                    };
                    let cached = match cached_input_tokens {
                        Some(v) => *v,
                        None => 0,
                    };
                    let think = match reasoning_tokens {
                        Some(v) => *v,
                        None => 0,
                    };
                    eprintln!(
                        "🧠 [turn {}] model {}ms | ttft={} | in={} (cached={}, hit={:.1}%) out={} think={}",
                        turn,
                        latency_ms,
                        ttft,
                        input_tokens,
                        cached,
                        prefix_cache_hit_ratio * 100.0,
                        output_tokens,
                        think
                    );
                }
                EventPayload::ToolCompleted {
                    tool_name,
                    relative_path,
                    duration_ms,
                    exit_code,
                    outcome,
                    edit_match_tier,
                    ..
                } => {
                    let path = match relative_path {
                        Some(p) => p.as_str(),
                        None => "-",
                    };
                    let exit = match exit_code {
                        Some(c) => c.to_string(),
                        None => "-".to_string(),
                    };
                    let tier = match edit_match_tier {
                        Some(t) => format!("{:?}", t),
                        None => "-".to_string(),
                    };
                    eprintln!(
                        "🔧 [tool:{}] path=\"{}\" -> {} ({}ms, exit={}, tier={})",
                        tool_name, path, outcome, duration_ms, exit, tier
                    );
                }
                EventPayload::ContextCompacted {
                    before_bytes,
                    after_bytes,
                    pinned_files,
                    ..
                } => {
                    eprintln!(
                        "📦 [compact] {}B -> {}B (pinned={:?})",
                        before_bytes, after_bytes, pinned_files
                    );
                }
                EventPayload::RevisionCaptured {
                    turn,
                    revision_id,
                    changed_paths,
                    tracked_file_count,
                    trigger,
                    ..
                } => {
                    eprintln!(
                        "📸 [revision] turn={} rev={} trigger={} files={} changed={}",
                        turn,
                        revision_id,
                        trigger,
                        tracked_file_count,
                        changed_paths.len()
                    );
                }
                EventPayload::RevisionRestored {
                    revision_id,
                    restored_files,
                    removed_files,
                } => {
                    eprintln!(
                        "⏪ [revision] restored {} ({} written, {} removed)",
                        revision_id, restored_files, removed_files
                    );
                }
                EventPayload::HookEvaluated {
                    hook_id,
                    hook_kind,
                    event,
                    outcome,
                    duration_ms,
                    message,
                } => {
                    if outcome != "allow" && outcome != "ok" {
                        eprintln!(
                            "🪝 [{}-hook:{}] {} -> {} ({}ms){}",
                            hook_kind,
                            hook_id,
                            event,
                            outcome,
                            duration_ms,
                            message
                                .as_ref()
                                .map(|m| format!(" | {}", m))
                                .unwrap_or_default()
                        );
                    }
                }
                EventPayload::ExtensionChanged {
                    extension_id,
                    action,
                } => {
                    eprintln!("🧩 [extension] {} {}", action, extension_id);
                }
                EventPayload::SubtreeContextLoaded {
                    directory,
                    file_name,
                    bytes,
                } => {
                    eprintln!(
                        "📄 [subtree-context] {}/{} ({}B)",
                        directory, file_name, bytes
                    );
                }
                EventPayload::SubagentSpawned {
                    subagent_id,
                    role,
                    max_turns,
                    isolated_worktree,
                } => {
                    eprintln!(
                        "🤖 [subagent:{}] spawned role={} max_turns={} isolated={}",
                        subagent_id, role, max_turns, isolated_worktree
                    );
                }
                EventPayload::SubagentCompleted {
                    subagent_id,
                    role,
                    status,
                    turns_used,
                    tool_calls_used,
                    tokens_used,
                    duration_ms,
                } => {
                    eprintln!(
                        "🤖 [subagent:{}] {} role={} ({}ms, turns={}, tools={}, tokens={})",
                        subagent_id,
                        status,
                        role,
                        duration_ms,
                        turns_used,
                        tool_calls_used,
                        tokens_used
                    );
                }
                EventPayload::WorktreeMerged {
                    subagent_id,
                    status,
                    written_files,
                    removed_files,
                    conflict_paths,
                    ..
                } => {
                    eprintln!(
                        "🌿 [worktree:{}] merge={} written={} removed={} conflicts={}",
                        subagent_id,
                        status,
                        written_files.len(),
                        removed_files.len(),
                        conflict_paths.len()
                    );
                }
                EventPayload::SurfaceStarted { surface, transport } => {
                    eprintln!(
                        "🌐 [surface] started surface={} transport={}",
                        surface, transport
                    );
                }
                EventPayload::ApprovalRequested {
                    request_id,
                    tool_name,
                    surface,
                } => {
                    eprintln!(
                        "🔐 [approval:{}] requested tool={} surface={}",
                        request_id, tool_name, surface
                    );
                }
                EventPayload::ApprovalResolved {
                    request_id,
                    decision,
                } => {
                    eprintln!("🔐 [approval:{}] resolved -> {}", request_id, decision);
                }
                EventPayload::RunTerminated {
                    status,
                    terminal_reason,
                    turns,
                    total_tool_calls,
                    total_input_tokens,
                    total_cached_input_tokens,
                    total_output_tokens,
                    cache_hit_ratio,
                    duration_ms,
                    ..
                } => {
                    eprintln!(
                        "🏁 [kroma] {:?} ({:?}) in {}ms | turns={} tools={} | in={} (cached={}, hit={:.1}%) out={}",
                        status,
                        terminal_reason,
                        duration_ms,
                        turns,
                        total_tool_calls,
                        total_input_tokens,
                        total_cached_input_tokens,
                        cache_hit_ratio * 100.0,
                        total_output_tokens
                    );
                }
                _ => {}
            }
        }

        let seq = self.seq.advance();
        let timestamp_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);

        let event = DiagnosticEvent {
            schema_version: EVENT_SCHEMA_VERSION.to_string(),
            run_id: self.run_id,
            seq: seq.0,
            timestamp_ms,
            event: payload,
        };

        if let Some(ref sink) = self.event_sink {
            sink(&event);
        }

        if self.in_memory_events.len() < 4096 {
            self.in_memory_events.push(event.clone());
        }

        let line = match serde_json::to_string(&event) {
            Ok(s) => format!("{}\n", s),
            Err(e) => {
                return self.handle_write_error(format!("serialization error: {}", e), seq);
            }
        };

        if let Some(ref mut f) = self.file {
            if let Err(e) = f.write_all(line.as_bytes()).and_then(|_| f.flush()) {
                self.file = None;
                return self.handle_write_error(
                    format!("write error on {}: {}", self.log_path.display(), e),
                    seq,
                );
            }
        } else {
            return self.handle_write_error(
                format!("log file {} unavailable", self.log_path.display()),
                seq,
            );
        }

        Ok(seq)
    }

    fn handle_write_error(
        &mut self,
        msg: String,
        seq: EventSeq,
    ) -> Result<EventSeq, EventLogError> {
        let was_degraded = self.log_degraded;
        self.log_degraded = true;
        self.dropped_events += 1;
        if self.strict_log {
            return Err(EventLogError::StrictIoFailure(msg));
        }
        if !was_degraded {
            eprintln!(
                "[WARN kroma] diagnostic event seq={} not persisted (log_degraded=true): {}",
                seq.0, msg
            );
        }
        Ok(seq)
    }

    pub fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    pub fn log_path(&self) -> &Path {
        &self.log_path
    }

    pub fn is_degraded(&self) -> bool {
        self.log_degraded
    }

    pub fn dropped_events(&self) -> usize {
        self.dropped_events
    }

    pub fn events(&self) -> &[DiagnosticEvent] {
        &self.in_memory_events
    }
}
