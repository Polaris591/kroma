//! Host-owned task control state shared by control-adapter plugins.
//!
//! The core stores control data and guards generic transitions. It never
//! interprets plugin (business) rules, reads no sidecar, starts no loops, runs
//! no commands and never treats exit status 0 as task completion.
//!
//! Public API for the host (kept intentionally small):
//! - [`TaskControlSnapshot`]: durable control record (host metadata only).
//! - [`TaskController`]: owner check, legal transitions, commit-after-persist.
//! - [`ControlUpdate`]: a *candidate* snapshot; callers persist the message and
//!   the snapshot together, then call [`TaskController::commit`]. Nothing is
//!   installed before that commit, so an unpersisted snapshot cannot leak.
//! - [`latest_control`] / [`TaskControlSnapshot::attach`] / [`strip_host_metadata`]:
//!   provider-item metadata round trip and provider projection stripping.
//! - [`ExecutionReceipt`] / [`FactIndex`]: raw facts, never model claims.
use crate::protocol::Message;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Provider-item type used for the host control record.
pub const CONTROL_ITEM_TYPE: &str = "kroma_task_control";
/// Provider-item type used for raw execution facts.
pub const EXECUTION_ITEM_TYPE: &str = "kroma_task_execution";
/// Control record version understood by this host.
pub const CONTROL_VERSION: u32 = 1;
/// Transport size bound, not a limit on task or checklist length.
pub const MAX_CONTROL_BYTES: usize = 256 * 1024;

/// Reason codes. Free-form strings are allowed, these are the shared ones.
pub mod reason {
    pub const INTERRUPTED: &str = "interrupted";
    pub const COMPLETED: &str = "completed";
    pub const CANCELLED: &str = "cancelled";
    pub const PROVIDER_ERROR: &str = "provider_error";
    pub const USER_STOP: &str = "user_stop";
    pub const BLOCKED: &str = "blocked";
    pub const SCOPE_CHANGED: &str = "scope_changed";
    pub const CARRIED_OVER: &str = "carried_over";
}

/// Bind a control task to the archive identity and the current history
/// generation. Both are host-derived; a copied or shortened history therefore
/// never inherits another scope running goal.
pub fn scope_for(archive: &std::path::Path, generation: u64) -> ControlScope {
    ControlScope::new(archive.to_string_lossy().into_owned(), generation)
}

/// The only four task states. No record means "no task".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Running,
    Paused,
    Completed,
    Cancelled,
}

impl TaskStatus {
    /// Running or paused: a task exists and may be resumed or inspected.
    pub fn is_active(self) -> bool {
        matches!(self, Self::Running | Self::Paused)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Paused => "paused",
            Self::Completed => "completed",
            Self::Cancelled => "cancelled",
        }
    }

    /// Completed or cancelled: never revived implicitly.
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Cancelled)
    }
}

/// Stable binding to one archive identity and one history generation.
/// Derived by the host, never from model input; the core only compares it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlScope {
    pub archive_key: String,
    pub generation: u64,
}

impl ControlScope {
    pub fn new(archive_key: impl Into<String>, generation: u64) -> Self {
        Self {
            archive_key: archive_key.into(),
            generation,
        }
    }
}

/// Where a registered check came from.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckSource {
    User,
    #[default]
    Derived,
}

/// Where a user-sourced requirement came from: one real user message and a
/// contiguous excerpt of its original text (whitespace is collapsed, nothing
/// else is interpreted).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceRef {
    pub message_index: usize,
    pub quote: String,
}

/// Characters of the plugin summary a host control event may carry.
pub const MAX_SUMMARY_PREVIEW_CHARS: usize = 160;

/// Upper bound for a cited user quote, matching the design's 500 characters.
pub const MAX_QUOTE_CHARS: usize = 500;

/// A registered command check contract. The host only verifies that the exact
/// command ran with a known outcome; it does not interpret what it proves.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckSpec {
    pub id: String,
    pub command: String,
    #[serde(default)]
    pub source: CheckSource,
    /// Required for user-sourced checks by the host's source validation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_ref: Option<SourceRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rationale: Option<String>,
}

impl CheckSpec {
    pub fn new(id: impl Into<String>, command: impl Into<String>, source: CheckSource) -> Self {
        Self {
            id: id.into(),
            command: command.into(),
            source,
            source_ref: None,
            rationale: None,
        }
    }
}

/// Revision and verification round that a control checkpoint was bound to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointBasis {
    pub revision: u64,
    pub verification_epoch: u64,
}

/// Authoritative control record. Host-owned fields only; plugin data stays in
/// `data` and is never interpreted by the core.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskControlSnapshot {
    pub version: u32,
    /// Informational plugin release version; data compatibility belongs to the plugin.
    #[serde(default)]
    pub plugin_version: String,
    pub owner: String,
    pub id: String,
    pub revision: u64,
    pub seq: u64,
    pub scope: ControlScope,
    pub status: TaskStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub run_id: String,
    /// Real user turns authorizing this goal and its revisions.
    #[serde(default)]
    pub source_indices: Vec<usize>,
    #[serde(default)]
    pub checks: Vec<CheckSpec>,
    #[serde(default)]
    pub verification_epoch: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint_basis: Option<CheckpointBasis>,
    #[serde(default)]
    pub data: Value,
}

impl TaskControlSnapshot {
    /// Serialize into the host metadata item (never sent to the provider).
    pub fn to_item(&self) -> Value {
        match serde_json::to_value(self) {
            Ok(Value::Object(mut map)) => {
                map.insert("type".into(), json!(CONTROL_ITEM_TYPE));
                Value::Object(map)
            }
            _ => json!({"type": CONTROL_ITEM_TYPE, "version": self.version}),
        }
    }

    /// Parse one host metadata item. Wrong type or unsupported version is an
    /// explicit error, never a silent downgrade.
    pub fn from_item(value: &Value) -> Result<Self, ControlRejection> {
        if !is_control_item(value) {
            return Err(ControlRejection::Invalid(
                "not a task control item".to_string(),
            ));
        }
        let version = value.get("version").and_then(Value::as_u64).unwrap_or(0) as u32;
        if version != CONTROL_VERSION {
            return Err(ControlRejection::UnsupportedVersion { version });
        }
        let snapshot: Self = serde_json::from_value(value.clone())
            .map_err(|error| ControlRejection::Invalid(error.to_string()))?;
        snapshot.validate()?;
        Ok(snapshot)
    }

    /// Append this record to a message's host metadata. Returns false when the
    /// record cannot be serialized (nothing is attached in that case).
    pub fn attach(&self, message: &mut Message) -> bool {
        let item = self.to_item();
        if item.get("owner").is_none() {
            return false;
        }
        message
            .provider_items
            .get_or_insert_with(Vec::new)
            .push(item);
        true
    }

    /// Ownership, version and size validation.
    pub fn validate(&self) -> Result<(), ControlRejection> {
        if self.version != CONTROL_VERSION {
            return Err(ControlRejection::UnsupportedVersion {
                version: self.version,
            });
        }
        if self.owner.trim().is_empty() {
            return Err(ControlRejection::Invalid("empty owner".to_string()));
        }
        if self.id.trim().is_empty() {
            return Err(ControlRejection::Invalid("empty task id".to_string()));
        }
        if self.run_id.trim().is_empty() {
            return Err(ControlRejection::Invalid("empty run id".to_string()));
        }
        if self.scope.archive_key.trim().is_empty() {
            return Err(ControlRejection::Invalid("empty archive key".to_string()));
        }
        let bytes = serde_json::to_vec(self)
            .map_err(|error| ControlRejection::Invalid(error.to_string()))?
            .len();
        if bytes > MAX_CONTROL_BYTES {
            return Err(ControlRejection::TooLarge {
                bytes,
                limit: MAX_CONTROL_BYTES,
            });
        }
        Ok(())
    }

    pub fn is_active(&self) -> bool {
        self.status.is_active()
    }
}

/// True when the value is the host control metadata item type.
pub fn is_control_item(value: &Value) -> bool {
    value.get("type").and_then(Value::as_str) == Some(CONTROL_ITEM_TYPE)
}

/// True when the value belongs to the host-private metadata namespace.
pub fn is_host_item(value: &Value) -> bool {
    matches!(
        value.get("type").and_then(Value::as_str),
        Some(CONTROL_ITEM_TYPE | EXECUTION_ITEM_TYPE)
    )
}

/// A snapshot found in message history.
#[derive(Debug, Clone, PartialEq)]
pub struct LoadedControl {
    /// Index of the message that carries the record in the scan input.
    pub message_index: usize,
    pub snapshot: TaskControlSnapshot,
}

/// Latest control record for `scope`, by message position (not file mtime).
///
/// Records from another scope are ignored: a copied or shortened history can
/// never revive another archive's task. A record in this scope with an
/// unsupported version is an error, not a silent skip.
pub fn latest_control(
    messages: &[Message],
    scope: &ControlScope,
) -> Result<Option<LoadedControl>, ControlRejection> {
    let mut found: Option<LoadedControl> = None;
    for (message_index, message) in messages.iter().enumerate() {
        for item in message.provider_items.as_deref().unwrap_or(&[]) {
            if !is_control_item(item) {
                continue;
            }
            let item_scope = item
                .get("scope")
                .and_then(|value| serde_json::from_value::<ControlScope>(value.clone()).ok());
            if item_scope.as_ref() != Some(scope) {
                continue;
            }
            found = Some(LoadedControl {
                message_index,
                snapshot: TaskControlSnapshot::from_item(item)?,
            });
        }
    }
    Ok(found)
}

/// Drop host-private task control/execution items from one message. Every other
/// provider item (signed reasoning, attachments, steering, tool status, ...) is
/// preserved. Apply to projection copies only, never to the durable history.
pub fn strip_host_metadata(message: &mut Message) {
    let Some(items) = message.provider_items.take() else {
        return;
    };
    let kept: Vec<Value> = items
        .into_iter()
        .filter(|item| !is_host_item(item))
        .collect();
    if !kept.is_empty() {
        message.provider_items = Some(kept);
    }
}

/// [`strip_host_metadata`] for a whole projection copy.
pub fn strip_host_metadata_all(messages: &mut [Message]) {
    for message in messages {
        strip_host_metadata(message);
    }
}

/// Why a control request was refused. Nothing is installed on refusal, so the
/// host can report the reason without touching durable state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlRejection {
    /// No control record exists for this controller instance.
    NoTask,
    /// The record belongs to another controller/plugin.
    ForeignOwner { expected: String, found: String },
    /// The record's control data version is not understood by this host.
    UnsupportedVersion { version: u32 },
    /// The record is bound to another archive identity/history generation.
    ScopeMismatch,
    /// A task is already running.
    AlreadyRunning,
    /// The transition requires a running task.
    NotRunning,
    /// The transition requires a paused task.
    NotPaused,
    /// Completed/cancelled tasks are never revived implicitly.
    Finished(TaskStatus),
    /// Plugin reply was produced for an older control state.
    StaleSeq { expected: u64, given: u64 },
    /// Plugin reply was produced for an older user revision.
    StaleRevision { expected: u64, given: u64 },
    /// Control record exceeds the size cap.
    TooLarge { bytes: usize, limit: usize },
    /// Malformed control record or plugin-provided fields.
    Invalid(String),
}

impl std::fmt::Display for ControlRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoTask => write!(f, "no task control record"),
            Self::ForeignOwner { expected, found } => {
                write!(f, "task is owned by {found}, not {expected}")
            }
            Self::UnsupportedVersion { version } => {
                write!(f, "unsupported control data version {version}")
            }
            Self::ScopeMismatch => write!(f, "task is bound to another archive/generation"),
            Self::AlreadyRunning => write!(f, "a task is already running"),
            Self::NotRunning => write!(f, "task is not running"),
            Self::NotPaused => write!(f, "task is not paused"),
            Self::Finished(status) => write!(f, "task is already {status:?}"),
            Self::StaleSeq { expected, given } => {
                write!(f, "stale control seq {given}, expected {expected}")
            }
            Self::StaleRevision { expected, given } => {
                write!(f, "stale revision {given}, expected {expected}")
            }
            Self::TooLarge { bytes, limit } => {
                write!(f, "control data is {bytes} bytes, limit {limit}")
            }
            Self::Invalid(detail) => write!(f, "invalid control data: {detail}"),
        }
    }
}

impl std::error::Error for ControlRejection {}

/// A candidate snapshot produced by a transition. The caller must persist it in
/// the same commit as the message that carries it, then call
/// [`TaskController::commit`]. The controller never installs it earlier.
#[derive(Debug, Clone, PartialEq)]
pub struct ControlUpdate {
    pub snapshot: TaskControlSnapshot,
    /// True when the caller has to persist this snapshot. False for a load that
    /// changed nothing (already paused/terminal), where no new record is needed.
    pub requires_commit: bool,
}

impl ControlUpdate {
    /// Attach the candidate record to the message committed with it.
    pub fn attach(&self, message: &mut Message) -> bool {
        self.snapshot.attach(message)
    }
}

/// Small state body: one owner, at most one current control record.
#[derive(Debug, Clone)]
pub struct TaskController {
    owner: String,
    snapshot: Option<TaskControlSnapshot>,
}

impl TaskController {
    pub fn new(owner: impl Into<String>) -> Self {
        Self {
            owner: owner.into(),
            snapshot: None,
        }
    }

    pub fn owner(&self) -> &str {
        &self.owner
    }

    /// Committed record, if any. Uncommitted candidates are not visible here.
    pub fn current(&self) -> Option<&TaskControlSnapshot> {
        self.snapshot.as_ref()
    }

    pub fn is_active(&self) -> bool {
        self.snapshot.as_ref().is_some_and(|s| s.is_active())
    }

    /// Install a snapshot after the host persisted it with its message.
    pub fn commit(&mut self, update: ControlUpdate) -> Result<(), ControlRejection> {
        if update.snapshot.owner != self.owner {
            return Err(ControlRejection::ForeignOwner {
                expected: self.owner.clone(),
                found: update.snapshot.owner,
            });
        }
        update.snapshot.validate()?;
        self.snapshot = Some(update.snapshot);
        Ok(())
    }

    /// Start a new task. Ids are minted by the host side only; a model or plugin
    /// cannot supply or reuse a task id.
    ///
    /// Allowed when no task exists, or when the previous task is paused or
    /// terminal (its record stays in history). Rejected while running.
    pub fn start(&self, scope: ControlScope) -> Result<ControlUpdate, ControlRejection> {
        if let Some(current) = &self.snapshot {
            if current.status == TaskStatus::Running {
                return Err(ControlRejection::AlreadyRunning);
            }
            if current.is_active() && current.scope != scope {
                return Err(ControlRejection::ScopeMismatch);
            }
        }
        let snapshot = TaskControlSnapshot {
            version: CONTROL_VERSION,
            plugin_version: String::new(),
            owner: self.owner.clone(),
            id: mint("task"),
            revision: 1,
            seq: 1,
            scope,
            status: TaskStatus::Running,
            reason: None,
            run_id: mint("run"),
            source_indices: Vec::new(),
            checks: Vec::new(),
            verification_epoch: 1,
            checkpoint_basis: None,
            data: Value::Null,
        };
        snapshot.validate()?;
        Ok(ControlUpdate {
            requires_commit: true,
            snapshot,
        })
    }

    /// Load the latest record for `scope` from history and repair an abandoned
    /// `running` record into `paused` (interrupted). Never sends a model
    /// request and never inherits facts from another scope.
    pub fn restore(
        &self,
        messages: &[Message],
        scope: &ControlScope,
    ) -> Result<Option<ControlUpdate>, ControlRejection> {
        let Some(loaded) = latest_control(messages, scope)? else {
            return Ok(None);
        };
        let mut snapshot = loaded.snapshot;
        if snapshot.source_indices.is_empty() {
            snapshot.source_indices = source_indices(messages, &snapshot.id);
        }
        if snapshot.owner != self.owner {
            return Err(ControlRejection::ForeignOwner {
                expected: self.owner.clone(),
                found: snapshot.owner,
            });
        }
        if snapshot.scope != *scope {
            return Err(ControlRejection::ScopeMismatch);
        }
        let requires_commit = snapshot.status == TaskStatus::Running;
        if requires_commit {
            snapshot.status = TaskStatus::Paused;
            snapshot.reason = Some(reason::INTERRUPTED.to_string());
            snapshot.seq += 1;
        }
        Ok(Some(ControlUpdate {
            snapshot,
            requires_commit,
        }))
    }

    /// Accept a plugin control reply for the current running task.
    ///
    /// Validates host ownership (implicit), the plugin's `seq`/`revision`, the
    /// data size and the check contract shape. Registering a *different* check
    /// plan opens a new verification round; an unchanged plan does not.
    pub fn update(
        &self,
        seq: u64,
        revision: u64,
        data: Value,
        checks: Option<Vec<CheckSpec>>,
    ) -> Result<ControlUpdate, ControlRejection> {
        let current = self.require_running()?;
        if seq != current.seq {
            return Err(ControlRejection::StaleSeq {
                expected: current.seq,
                given: seq,
            });
        }
        if revision != current.revision {
            return Err(ControlRejection::StaleRevision {
                expected: current.revision,
                given: revision,
            });
        }
        let mut snapshot = current.clone();
        if let Some(checks) = checks {
            validate_checks(&checks)?;
            snapshot.checks = checks;
        }
        snapshot.data = data;
        snapshot.seq += 1;
        snapshot.reason = None;
        snapshot.checkpoint_basis = Some(CheckpointBasis {
            revision: snapshot.revision,
            verification_epoch: snapshot.verification_epoch,
        });
        snapshot.validate()?;
        Ok(ControlUpdate {
            snapshot,
            requires_commit: true,
        })
    }

    /// A real user revision (steering or a new requirement) moves `revision`
    /// and opens a new verification round. Old declarations stop applying; no
    /// budget is reset and the status is unchanged.
    pub fn revise(&self) -> Result<ControlUpdate, ControlRejection> {
        let current = self.require_active()?;
        let mut snapshot = current.clone();
        snapshot.revision += 1;
        snapshot.seq += 1;
        snapshot.verification_epoch += 1;
        snapshot.checkpoint_basis = None;
        snapshot.reason = None;
        Ok(ControlUpdate {
            snapshot,
            requires_commit: true,
        })
    }

    /// Explicitly reopen a stopped task with a new run id and verification round.
    /// `revise` also applies a user revision carried by the resume input.
    ///
    /// Terminal tasks reopen only through this explicit user command.
    pub fn resume(&self, revise: bool) -> Result<ControlUpdate, ControlRejection> {
        let current = self.snapshot.as_ref().ok_or(ControlRejection::NoTask)?;
        match current.status {
            TaskStatus::Running => return Err(ControlRejection::NotPaused),
            TaskStatus::Paused | TaskStatus::Completed | TaskStatus::Cancelled => {}
        }
        let mut snapshot = current.clone();
        snapshot.run_id = mint("run");
        snapshot.status = TaskStatus::Running;
        snapshot.reason = None;
        snapshot.seq += 1;
        snapshot.verification_epoch += 1;
        snapshot.checkpoint_basis = None;
        if revise {
            snapshot.revision += 1;
        }
        Ok(ControlUpdate {
            snapshot,
            requires_commit: true,
        })
    }

    /// Move a running task to paused with a reason code. The caller persists
    /// the ordered pause (host-authored explanation) with this snapshot.
    pub fn pause(&self, reason: impl Into<String>) -> Result<ControlUpdate, ControlRejection> {
        let current = self.require_running()?;
        let mut snapshot = current.clone();
        snapshot.status = TaskStatus::Paused;
        snapshot.reason = Some(reason.into());
        snapshot.seq += 1;
        Ok(ControlUpdate {
            snapshot,
            requires_commit: true,
        })
    }

    /// Explicit user cancel. Allowed while running or paused.
    pub fn cancel(&self) -> Result<ControlUpdate, ControlRejection> {
        let current = self.require_active()?;
        let mut snapshot = current.clone();
        snapshot.status = TaskStatus::Cancelled;
        snapshot.reason = Some(reason::CANCELLED.to_string());
        snapshot.seq += 1;
        Ok(ControlUpdate {
            snapshot,
            requires_commit: true,
        })
    }

    /// Terminal transition to completed.
    ///
    /// The caller must only use this after its own finish gate accepted the
    /// candidate answer (revision-bound checkpoint, current-round checks, known
    /// execution facts, non-empty delivery). Those guards are layered on by the
    /// host step; this function only owns the state transition, so it must not
    /// be called because a model produced a "done" claim.
    pub fn complete(&self) -> Result<ControlUpdate, ControlRejection> {
        let current = self.require_running()?;
        let mut snapshot = current.clone();
        snapshot.status = TaskStatus::Completed;
        snapshot.reason = Some(reason::COMPLETED.to_string());
        snapshot.seq += 1;
        Ok(ControlUpdate {
            snapshot,
            requires_commit: true,
        })
    }

    fn require_running(&self) -> Result<&TaskControlSnapshot, ControlRejection> {
        let current = self.snapshot.as_ref().ok_or(ControlRejection::NoTask)?;
        match current.status {
            TaskStatus::Running => Ok(current),
            _ => Err(ControlRejection::NotRunning),
        }
    }

    fn require_active(&self) -> Result<&TaskControlSnapshot, ControlRejection> {
        let current = self.snapshot.as_ref().ok_or(ControlRejection::NoTask)?;
        if current.status.is_active() {
            Ok(current)
        } else {
            Err(ControlRejection::Finished(current.status))
        }
    }
}

fn validate_checks(checks: &[CheckSpec]) -> Result<(), ControlRejection> {
    let mut ids = std::collections::HashSet::new();
    for check in checks {
        if check.id.trim().is_empty() || check.id.len() > 32 {
            return Err(ControlRejection::Invalid("bad check id".to_string()));
        }
        if !check
            .id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            return Err(ControlRejection::Invalid("bad check id".to_string()));
        }
        if check.command.trim().is_empty() {
            return Err(ControlRejection::Invalid("empty check command".to_string()));
        }
        if !ids.insert(&check.id) {
            return Err(ControlRejection::Invalid(format!(
                "duplicate check id {}",
                check.id
            )));
        }
        validate_source_ref_shape(check.source, check.source_ref.as_ref())?;
    }
    Ok(())
}

/// Shape check only: a user-sourced contract must cite a real quote. Whether it
/// exists in this session is checked against authoritative history by
/// [`validate_check_sources`].
fn validate_source_ref_shape(
    _source: CheckSource,
    source_ref: Option<&SourceRef>,
) -> Result<(), ControlRejection> {
    match source_ref {
        None => Ok(()),
        Some(reference) => {
            let quote = collapse_whitespace(&reference.quote);
            if quote.is_empty() {
                return Err(ControlRejection::Invalid("empty source quote".to_string()));
            }
            if quote.chars().count() > MAX_QUOTE_CHARS {
                return Err(ControlRejection::Invalid(format!(
                    "source quote exceeds {MAX_QUOTE_CHARS} characters"
                )));
            }
            Ok(())
        }
    }
}

/// Collapse every whitespace run to one space. Used for exact, non-semantic
/// quote matching; no stemming, case folding or paraphrase acceptance.
pub fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Host-side id minting. Plugins and models never supply these.
fn mint(prefix: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos() as u64)
        .unwrap_or(0);
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}-{nanos:x}-{counter:x}")
}

/// Raw execution fact captured by the host before any result rewriting.
///
/// Only host-recorded values are trusted: a command string reported by the
/// model, a rewritten result text or "no error seen" are not execution facts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionReceipt {
    pub call_id: String,
    pub tool: String,
    /// Exact command actually executed (already decoded), if the tool has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    pub workdir: String,
    /// Raw error bit from the tool adapter.
    pub is_error: bool,
    /// Shell exit status when it was observed; `None` means unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    pub goal_id: String,
    pub revision: u64,
    pub verification_epoch: u64,
    /// Registered check this call matched, if any (assigned by the host step).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check_id: Option<String>,
}

/// What is known about an execution. `Unknown` never counts as passing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReceiptOutcome {
    Succeeded,
    Failed,
    Unknown,
}

impl ExecutionReceipt {
    /// A shell execution fact. `exit_code == None` (killed, timed out, spawn
    /// failure) stays unknown instead of being guessed.
    pub fn shell(
        call_id: impl Into<String>,
        command: impl Into<String>,
        workdir: impl Into<String>,
        is_error: bool,
        exit_code: Option<i32>,
    ) -> Self {
        Self {
            call_id: call_id.into(),
            tool: "shell".to_string(),
            command: Some(command.into()),
            workdir: workdir.into(),
            is_error,
            exit_code,
            goal_id: String::new(),
            revision: 0,
            verification_epoch: 0,
            check_id: None,
        }
    }

    /// Bind the fact to the task state that was active when it ran.
    pub fn with_control(
        mut self,
        goal_id: impl Into<String>,
        revision: u64,
        verification_epoch: u64,
    ) -> Self {
        self.goal_id = goal_id.into();
        self.revision = revision;
        self.verification_epoch = verification_epoch;
        self
    }

    pub fn outcome(&self) -> ReceiptOutcome {
        if self.is_error {
            return ReceiptOutcome::Failed;
        }
        match self.exit_code {
            Some(0) => ReceiptOutcome::Succeeded,
            Some(_) => ReceiptOutcome::Failed,
            None => ReceiptOutcome::Unknown,
        }
    }

    pub fn to_item(&self) -> Value {
        match serde_json::to_value(self) {
            Ok(Value::Object(mut map)) => {
                map.insert("type".into(), json!(EXECUTION_ITEM_TYPE));
                Value::Object(map)
            }
            _ => json!({"type": EXECUTION_ITEM_TYPE, "call_id": self.call_id}),
        }
    }

    pub fn from_item(value: &Value) -> Option<Self> {
        if value.get("type").and_then(Value::as_str) != Some(EXECUTION_ITEM_TYPE) {
            return None;
        }
        serde_json::from_value(value.clone()).ok()
    }

    /// Store the fact in the same message the tool result is committed with.
    pub fn attach(&self, message: &mut Message) -> bool {
        let item = self.to_item();
        if item.get("call_id").is_none() {
            return false;
        }
        message
            .provider_items
            .get_or_insert_with(Vec::new)
            .push(item);
        true
    }

    /// First execution fact on a message, if any.
    pub fn from_message(message: &Message) -> Option<Self> {
        message
            .provider_items
            .as_deref()
            .unwrap_or(&[])
            .iter()
            .find_map(Self::from_item)
    }

    /// All facts recorded for `goal_id`, in message order.
    pub fn collect(messages: &[Message], goal_id: &str) -> Vec<Self> {
        let mut found: Vec<Self> = Vec::new();
        for message in messages {
            for item in message.provider_items.as_deref().unwrap_or(&[]) {
                if let Some(receipt) = Self::from_item(item) {
                    if receipt.goal_id == goal_id {
                        found.push(receipt);
                    }
                }
            }
        }
        found
    }
}

/// One real user message the controller plugin may cite. `message_index` is a
/// position in the authoritative history, never a model-supplied value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserSource {
    pub message_index: usize,
    pub content: String,
}

/// Provider-item type marking a host-authored reference, not a user instruction.
pub const CONTROL_NOTE_ITEM_TYPE: &str = "kroma_control_reference";

/// Strip reserved host fields at external input boundaries, before persistence.
pub fn strip_incoming_host_metadata(message: &mut Message) {
    strip_host_metadata(message);
    if let Some(items) = message.provider_items.as_mut() {
        items.retain(|item| item["type"] != CONTROL_NOTE_ITEM_TYPE);
        if items.is_empty() {
            message.provider_items = None;
        }
    }
}

fn source_indices(messages: &[Message], goal_id: &str) -> Vec<usize> {
    messages
        .iter()
        .enumerate()
        .filter(|(_, message)| {
            is_actual_user_message(message)
                && message
                    .provider_items
                    .as_deref()
                    .unwrap_or(&[])
                    .iter()
                    .any(|item| is_control_item(item) && item["id"] == goal_id)
        })
        .map(|(index, _)| index)
        .collect()
}

/// A branch may carry the latest active requirement, never an older one hidden
/// behind a completed or cancelled record.
pub fn suspended_origin(messages: &[Message]) -> Option<TaskControlSnapshot> {
    let mut latest = messages
        .iter()
        .rev()
        .flat_map(|message| {
            message
                .provider_items
                .as_deref()
                .unwrap_or(&[])
                .iter()
                .rev()
        })
        .find_map(|item| TaskControlSnapshot::from_item(item).ok())?;
    if !latest.is_active() {
        return None;
    }
    if latest.source_indices.is_empty() {
        latest.source_indices = source_indices(messages, &latest.id);
    }
    let origin = *latest.source_indices.first()?;
    messages
        .get(origin)
        .filter(|message| is_actual_user_message(message))?;
    Some(latest)
}
/// True for a host-authored control-reference message.
pub fn is_control_note(message: &Message) -> bool {
    message
        .provider_items
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .any(|item| item.get("type").and_then(Value::as_str) == Some(CONTROL_NOTE_ITEM_TYPE))
}

/// True only for a message the human actually wrote. Host control notes also
/// use `role = "user"` as transport, so task signals, recall anchors and
/// trusted user quotes must test this instead of comparing the role alone.
pub fn is_actual_user_message(message: &Message) -> bool {
    message.role == "user" && !is_control_note(message)
}

/// Real user messages with their authoritative positions. No silent truncation.
pub fn user_sources(messages: &[Message]) -> Vec<UserSource> {
    messages
        .iter()
        .enumerate()
        .filter(|(_, message)| is_actual_user_message(message))
        .map(|(message_index, message)| UserSource {
            message_index,
            content: message.content.clone().unwrap_or_default(),
        })
        .collect()
}

/// Only this goal's user turns, selected directly rather than rescanning history
/// before every tool call. Legacy source positions are recovered once on load.
pub fn goal_sources(
    snapshot: Option<&TaskControlSnapshot>,
    messages: &[Message],
) -> Vec<UserSource> {
    snapshot
        .into_iter()
        .flat_map(|snapshot| &snapshot.source_indices)
        .filter_map(|&message_index| {
            let message = messages
                .get(message_index)
                .filter(|message| is_actual_user_message(message))?;
            Some(UserSource {
                message_index,
                content: message.content.clone().unwrap_or_default(),
            })
        })
        .collect()
}

/// Root-level read-only context for command, control-tool and finish requests.
/// It carries no host-private execution facts and no credentials.
pub fn control_context(snapshot: Option<&TaskControlSnapshot>, sources: &[UserSource]) -> Value {
    let (status, seq, id, revision, checks, data) = match snapshot {
        Some(snapshot) => (
            serde_json::to_value(snapshot.status).unwrap_or_else(|_| json!("paused")),
            json!(snapshot.seq),
            json!(snapshot.id),
            json!(snapshot.revision),
            json!(snapshot.checks),
            if snapshot.data.is_object() {
                snapshot.data.clone()
            } else {
                json!({})
            },
        ),
        None => (
            json!("none"),
            json!(0),
            Value::Null,
            Value::Null,
            json!([]),
            json!({}),
        ),
    };
    let mut context = json!({
        "status": status,
        "seq": seq,
        "checks": checks,
        "data": data,
        "user_sources": sources,
    });
    if let Some(map) = context.as_object_mut() {
        if let Some(id) = id.as_str() {
            map.insert("id".into(), json!(id));
        }
        if let Some(revision) = revision.as_u64() {
            map.insert("revision".into(), json!(revision));
        }
    }
    context
}

/// Verify that every cited user source really exists in the authoritative
/// history: same message position, same role, and the quote is contiguous in
/// that message after whitespace collapsing. No semantic matching.
pub fn validate_check_sources(
    checks: &[CheckSpec],
    sources: &[UserSource],
) -> Result<(), ControlRejection> {
    for check in checks {
        let Some(reference) = check.source_ref.as_ref() else {
            continue;
        };
        let source = sources
            .iter()
            .find(|source| source.message_index == reference.message_index)
            .ok_or_else(|| {
                ControlRejection::Invalid(format!(
                    "check {} cites message {} which is not a real user message",
                    check.id, reference.message_index
                ))
            })?;
        let quote = collapse_whitespace(&reference.quote);
        if !collapse_whitespace(&source.content).contains(&quote) {
            return Err(ControlRejection::Invalid(format!(
                "check {} cites text that is not in user message {}",
                check.id, reference.message_index
            )));
        }
    }
    Ok(())
}

/// Bounded raw-execution index for the current task. Facts are only ever
/// appended by the host; nothing here is derived from model-supplied text.
#[derive(Debug, Clone, Default)]
pub struct FactIndex {
    receipts: Vec<ExecutionReceipt>,
}

/// Registered-check evaluation for the current verification round.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CheckStatus {
    /// Registered checks that ran with a known success in this round.
    pub passed: Vec<String>,
    /// Registered checks without a passing execution in this round.
    pub missing: Vec<String>,
}

impl CheckStatus {
    pub fn all_passed(&self) -> bool {
        self.missing.is_empty()
    }
}

impl FactIndex {
    pub fn from_history(messages: &[Message], goal_id: &str) -> Self {
        let mut index = Self::default();
        for receipt in ExecutionReceipt::collect(messages, goal_id) {
            index.record(receipt);
        }
        index
    }

    /// Keep every fact for the current goal so long tasks do not lose references.
    pub fn record(&mut self, receipt: ExecutionReceipt) {
        self.receipts.push(receipt);
    }

    pub fn len(&self) -> usize {
        self.receipts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.receipts.is_empty()
    }

    /// Exact-match classification of a shell execution against the current
    /// registered plan. Command and workdir must match verbatim; shell syntax
    /// is never parsed or normalized.
    pub fn classify(
        snapshot: &TaskControlSnapshot,
        command: Option<&str>,
        workdir: &str,
    ) -> Option<String> {
        if workdir.is_empty() {
            return None;
        }
        let command = command?;
        snapshot
            .checks
            .iter()
            .find(|check| check.command == command)
            .map(|check| check.id.clone())
    }

    /// Current-round evaluation: the latest execution of a registered check
    /// must be a known success. A later non-check execution opens a new round
    /// (`verification_epoch`), so it never appears among this round's receipts.
    pub fn status(&self, snapshot: &TaskControlSnapshot, workdir: &str) -> CheckStatus {
        let mut status = CheckStatus::default();
        for check in &snapshot.checks {
            let mut latest_succeeded = false;
            for receipt in &self.receipts {
                if receipt.goal_id != snapshot.id
                    || receipt.revision != snapshot.revision
                    || receipt.verification_epoch != snapshot.verification_epoch
                    || receipt.command.as_deref() != Some(check.command.as_str())
                    || receipt.workdir != workdir
                {
                    continue;
                }
                // Ordinary executions open a new round instead of living in
                // this one, so only registered-check receipts are considered.
                latest_succeeded = receipt.outcome() == ReceiptOutcome::Succeeded;
            }
            if latest_succeeded {
                status.passed.push(check.id.clone());
            } else {
                status.missing.push(check.id.clone());
            }
        }
        status
    }

    /// True when a tool call id was actually executed and recorded.
    pub fn has_call(&self, call_id: &str, goal_id: &str, revision: u64) -> bool {
        self.receipts.iter().any(|receipt| {
            receipt.call_id == call_id && receipt.goal_id == goal_id && receipt.revision == revision
        })
    }
}

/// Facts handed to `before_task_finish`. Every field is an explicit boolean or
/// count; no field can be omitted and then read as a pass.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FinishFacts {
    pub checks_satisfied: bool,
    pub missing: Vec<String>,
    pub checkpoint_basis_valid: bool,
}

impl FinishFacts {
    pub fn to_value(&self) -> Value {
        json!({
            "checks_satisfied": self.checks_satisfied,
            "missing": self.missing,
            "checkpoint_basis_valid": self.checkpoint_basis_valid,
        })
    }

    /// Host-side blockers evaluated independently of the plugin decision.
    pub fn host_blockers(&self, has_content: bool) -> Vec<String> {
        let mut blockers = Vec::new();
        if !self.checkpoint_basis_valid {
            blockers.push("checkpoint_basis".to_string());
        }
        if !self.checks_satisfied {
            blockers.extend(self.missing.iter().map(|id| format!("check:{id}")));
        }
        if !has_content {
            blockers.push("delivery".to_string());
        }
        blockers
    }
}

/// Evaluate the candidate finish from host facts only.
pub fn finish_facts(
    snapshot: &TaskControlSnapshot,
    facts: &FactIndex,
    workdir: &str,
) -> FinishFacts {
    let status = facts.status(snapshot, workdir);
    let basis_valid = snapshot.checkpoint_basis
        == Some(CheckpointBasis {
            revision: snapshot.revision,
            verification_epoch: snapshot.verification_epoch,
        });
    FinishFacts {
        checks_satisfied: status.all_passed(),
        missing: status.missing,
        checkpoint_basis_valid: basis_valid,
    }
}

/// What a plugin may propose through root-level `control`. `completed` is not
/// representable: only the host finish gate installs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlOp {
    Start,
    Resume,
    Update,
}

impl ControlOp {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Resume => "resume",
            Self::Update => "update",
        }
    }
}

/// One validated control proposal from the owner plugin.
#[derive(Debug, Clone, PartialEq)]
pub struct ControlProposal {
    pub op: ControlOp,
    pub base_seq: u64,
    pub id: Option<String>,
    pub revision: Option<u64>,
    pub data: Value,
    pub checks: Option<Vec<CheckSpec>>,
    /// Execution ids cited by the plugin; checked against host records before commit.
    pub evidence: Vec<String>,
}

impl ControlProposal {
    /// Strict decode: unknown fields, a missing op, a non-object data payload
    /// or an injected identity field are rejected as a whole.
    pub fn parse(value: &Value) -> Result<Self, ControlRejection> {
        let object = value
            .as_object()
            .ok_or_else(|| ControlRejection::Invalid("control must be a JSON object".into()))?;
        for key in object.keys() {
            match key.as_str() {
                "op" | "base_seq" | "id" | "revision" | "data" | "checks" | "evidence" => {}
                other => {
                    return Err(ControlRejection::Invalid(format!(
                        "unknown control field '{other}'"
                    )))
                }
            }
        }
        let op = match object.get("op").and_then(Value::as_str) {
            Some("start") => ControlOp::Start,
            Some("resume") => ControlOp::Resume,
            Some("update") => ControlOp::Update,
            Some(other) => {
                return Err(ControlRejection::Invalid(format!(
                    "unsupported control op '{other}'"
                )))
            }
            None => return Err(ControlRejection::Invalid("control needs op".into())),
        };
        let base_seq = object.get("base_seq").and_then(Value::as_u64).unwrap_or(0);
        let id = match object.get("id") {
            None | Some(Value::Null) => None,
            Some(Value::String(id)) => Some(id.clone()),
            Some(_) => {
                return Err(ControlRejection::Invalid(
                    "control id must be a string".into(),
                ))
            }
        };
        let revision = match object.get("revision") {
            None | Some(Value::Null) => None,
            Some(value) => Some(value.as_u64().ok_or_else(|| {
                ControlRejection::Invalid("control revision must be a number".into())
            })?),
        };
        let data = match object.get("data") {
            Some(value) if value.is_object() => value.clone(),
            Some(_) => {
                return Err(ControlRejection::Invalid(
                    "control data must be a JSON object".into(),
                ))
            }
            None => Value::Null,
        };
        if op != ControlOp::Start && data.is_null() {
            return Err(ControlRejection::Invalid(
                "control data is required for resume/update".into(),
            ));
        }
        let checks = match object.get("checks") {
            None | Some(Value::Null) => None,
            Some(value) => Some(
                serde_json::from_value::<Vec<CheckSpec>>(value.clone()).map_err(|error| {
                    ControlRejection::Invalid(format!("invalid control checks: {error}"))
                })?,
            ),
        };
        let evidence = object
            .get("evidence")
            .map(|value| serde_json::from_value::<Vec<String>>(value.clone()))
            .transpose()
            .map_err(|error| ControlRejection::Invalid(format!("invalid evidence: {error}")))?
            .unwrap_or_default();
        Ok(Self {
            op,
            base_seq,
            id,
            revision,
            data,
            checks,
            evidence,
        })
    }
}

impl ControlUpdate {
    /// Attach plugin-owned data (and an optional full check plan) to a freshly
    /// started task. Host identity, ids and budgets stay untouched.
    pub fn with_data(
        mut self,
        data: Value,
        checks: Option<Vec<CheckSpec>>,
    ) -> Result<Self, ControlRejection> {
        if !data.is_object() {
            return Err(ControlRejection::Invalid(
                "control data must be a JSON object".into(),
            ));
        }
        if let Some(checks) = checks {
            validate_checks(&checks)?;
            self.snapshot.checks = checks;
        }
        self.snapshot.data = data;
        self.snapshot.validate()?;
        Ok(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope(generation: u64) -> ControlScope {
        ControlScope::new("archive-key", generation)
    }

    fn history_with(snapshot: &TaskControlSnapshot) -> Vec<Message> {
        let mut message = Message::new("user", "/goal fix login");
        assert!(snapshot.attach(&mut message));
        vec![message]
    }

    #[test]
    fn start_mints_host_ids_and_never_installs_before_commit() {
        let controller = TaskController::new("goal");
        let update = controller.start(scope(7)).unwrap();
        assert_eq!(update.snapshot.owner, "goal");
        assert_eq!(update.snapshot.version, CONTROL_VERSION);
        assert_eq!(update.snapshot.revision, 1);
        assert_eq!(update.snapshot.seq, 1);
        assert_eq!(update.snapshot.verification_epoch, 1);
        assert_eq!(update.snapshot.status, TaskStatus::Running);
        assert_eq!(update.snapshot.scope, scope(7));
        assert!(update.snapshot.id.starts_with("task-"));
        assert!(update.snapshot.run_id.starts_with("run-"));
        assert!(update.requires_commit);
        assert!(
            controller.current().is_none(),
            "a candidate snapshot is not installed before the host commits it"
        );
    }

    #[test]
    fn running_task_blocks_restart_and_cross_scope_takeover() {
        let mut controller = TaskController::new("goal");
        let first = controller.start(scope(7)).unwrap();
        let first_id = first.snapshot.id.clone();
        controller.commit(first).unwrap();
        assert_eq!(
            controller.start(scope(7)).unwrap_err(),
            ControlRejection::AlreadyRunning
        );
        assert_eq!(
            controller.start(scope(8)).unwrap_err(),
            ControlRejection::AlreadyRunning
        );

        let paused = controller.pause(reason::BLOCKED).unwrap();
        controller.commit(paused).unwrap();
        assert_eq!(
            controller.start(scope(8)).unwrap_err(),
            ControlRejection::ScopeMismatch
        );
        let next = controller.start(scope(7)).unwrap();
        assert_ne!(next.snapshot.id, first_id, "a new goal gets a new host id");
        assert_eq!(next.snapshot.reason, None);
    }

    #[test]
    fn metadata_is_scope_and_position_based_and_strips_cleanly() {
        let first = TaskController::new("goal")
            .start(scope(7))
            .unwrap()
            .snapshot;
        let newer = TaskController::new("goal")
            .start(scope(7))
            .unwrap()
            .snapshot;
        assert_ne!(first.id, newer.id);

        let mut early = Message::new("user", "/goal one");
        early.provider_items = Some(vec![
            json!({"type": "kroma_steering"}),
            json!({"type": "kroma_attachment", "kind": "log", "path": "build.log"}),
        ]);
        assert!(first.attach(&mut early));
        let mut late = Message::new("assistant", "progress");
        assert!(newer.attach(&mut late));

        let history = vec![early, late];
        let found = latest_control(&history, &scope(7)).unwrap().unwrap();
        assert_eq!(found.message_index, 1, "latest message position wins");
        assert_eq!(found.snapshot, newer);
        assert_eq!(latest_control(&history, &scope(8)).unwrap(), None);

        let mut foreign = Message::new("user", "/goal two");
        let mut other_scope = newer.clone();
        other_scope.scope = scope(8);
        assert!(other_scope.attach(&mut foreign));
        assert!(latest_control(&[foreign], &scope(7)).unwrap().is_none());

        let mut stripped = history[0].clone();
        strip_host_metadata(&mut stripped);
        let kept = stripped.provider_items.as_deref().unwrap();
        assert_eq!(kept.len(), 2, "unrelated provider metadata is preserved");
        assert!(kept.iter().all(|item| !is_host_item(item)));

        let mut only = Message::new("assistant", "x");
        assert!(newer.attach(&mut only));
        strip_host_metadata(&mut only);
        assert_eq!(only.provider_items, None);
    }

    #[test]
    fn restore_repairs_interrupted_running_task() {
        let started = TaskController::new("goal")
            .start(scope(7))
            .unwrap()
            .snapshot;
        let history = history_with(&started);
        let restored = TaskController::new("goal")
            .restore(&history, &scope(7))
            .unwrap()
            .unwrap();
        assert!(restored.requires_commit);
        assert_eq!(restored.snapshot.status, TaskStatus::Paused);
        assert_eq!(
            restored.snapshot.reason.as_deref(),
            Some(reason::INTERRUPTED)
        );
        assert_eq!(restored.snapshot.id, started.id);
        assert_eq!(restored.snapshot.revision, started.revision);
        assert_eq!(restored.snapshot.seq, started.seq + 1);

        let mut controller = TaskController::new("goal");
        assert!(controller.current().is_none());
        controller.commit(restored).unwrap();
        let resumed = controller.resume(false).unwrap();
        assert_eq!(resumed.snapshot.status, TaskStatus::Running);
        assert!(TaskController::new("goal")
            .restore(&[], &scope(7))
            .unwrap()
            .is_none());
    }

    #[test]
    fn restore_of_paused_record_requires_no_new_commit() {
        let mut controller = TaskController::new("goal");
        let start = controller.start(scope(7)).unwrap();
        controller.commit(start).unwrap();
        let paused = controller.pause(reason::BLOCKED).unwrap();
        controller.commit(paused).unwrap();
        let history = history_with(controller.current().unwrap());
        let reloaded = TaskController::new("goal")
            .restore(&history, &scope(7))
            .unwrap()
            .unwrap();
        assert!(!reloaded.requires_commit);
        let mut expected = controller.current().unwrap().clone();
        expected.source_indices = vec![0];
        assert_eq!(reloaded.snapshot, expected);
    }

    #[test]
    fn foreign_owner_and_unsupported_version_are_rejected() {
        let snapshot = TaskController::new("goal")
            .start(scope(7))
            .unwrap()
            .snapshot;
        let history = history_with(&snapshot);
        assert_eq!(
            TaskController::new("other")
                .restore(&history, &scope(7))
                .unwrap_err(),
            ControlRejection::ForeignOwner {
                expected: "other".into(),
                found: "goal".into(),
            }
        );
        let mut controller = TaskController::new("other");
        assert!(matches!(
            controller.commit(ControlUpdate {
                snapshot: snapshot.clone(),
                requires_commit: true,
            }),
            Err(ControlRejection::ForeignOwner { .. })
        ));

        let mut item = snapshot.to_item();
        item["version"] = json!(99);
        let mut message = Message::new("user", "/goal");
        message.provider_items = Some(vec![item]);
        assert_eq!(
            TaskController::new("goal")
                .restore(&[message], &scope(7))
                .unwrap_err(),
            ControlRejection::UnsupportedVersion { version: 99 }
        );
    }

    #[test]
    fn explicit_resume_reopens_stopped_tasks_without_automatic_replay() {
        let mut controller = TaskController::new("goal");
        let start = controller.start(scope(7)).unwrap();
        let start_run = start.snapshot.run_id.clone();
        let value = start.snapshot.to_item();
        controller
            .commit(ControlUpdate {
                snapshot: TaskControlSnapshot::from_item(&value).unwrap(),
                requires_commit: true,
            })
            .unwrap();
        assert_eq!(
            controller.resume(false).unwrap_err(),
            ControlRejection::NotPaused
        );

        let paused = controller.pause(reason::BLOCKED).unwrap();
        controller.commit(paused).unwrap();
        let resumed = controller.resume(true).unwrap();
        assert_eq!(resumed.snapshot.status, TaskStatus::Running);
        assert_eq!(
            resumed.snapshot.revision, 2,
            "resume input applies a revision"
        );
        assert_eq!(resumed.snapshot.reason, None);
        assert_ne!(resumed.snapshot.run_id, start_run);
        assert!(resumed.snapshot.checkpoint_basis.is_none());
        assert!(resumed.snapshot.verification_epoch > 1);
        controller.commit(resumed).unwrap();

        let done = controller.complete().unwrap();
        controller.commit(done).unwrap();
        assert_eq!(controller.current().unwrap().status, TaskStatus::Completed);
        assert_eq!(
            controller.resume(false).unwrap().snapshot.status,
            TaskStatus::Running
        );
        assert_eq!(
            controller.revise().unwrap_err(),
            ControlRejection::Finished(TaskStatus::Completed)
        );
        assert_eq!(
            controller.pause(reason::BLOCKED).unwrap_err(),
            ControlRejection::NotRunning
        );
        assert_eq!(
            controller.cancel().unwrap_err(),
            ControlRejection::Finished(TaskStatus::Completed)
        );
    }

    #[test]
    fn cancel_from_paused_is_terminal() {
        let mut controller = TaskController::new("goal");
        let start = controller.start(scope(7)).unwrap();
        controller.commit(start).unwrap();
        let paused = controller.pause(reason::USER_STOP).unwrap();
        controller.commit(paused).unwrap();
        let cancelled = controller.cancel().unwrap();
        assert_eq!(cancelled.snapshot.status, TaskStatus::Cancelled);
        assert_eq!(
            cancelled.snapshot.reason.as_deref(),
            Some(reason::CANCELLED)
        );
        controller.commit(cancelled).unwrap();
        assert_eq!(
            controller.resume(false).unwrap().snapshot.status,
            TaskStatus::Running
        );
    }

    #[test]
    fn revise_moves_revision_and_opens_a_round() {
        let mut controller = TaskController::new("goal");
        let start = controller.start(scope(7)).unwrap();
        controller.commit(start).unwrap();
        let revised = controller.revise().unwrap();
        assert_eq!(revised.snapshot.revision, 2);
        assert_eq!(revised.snapshot.seq, 2);
        assert_eq!(revised.snapshot.verification_epoch, 2);
        assert_eq!(revised.snapshot.status, TaskStatus::Running);
        assert!(revised.snapshot.checkpoint_basis.is_none());
        controller.commit(revised).unwrap();
        let paused = controller.pause(reason::BLOCKED).unwrap();
        controller.commit(paused).unwrap();
        let revised = controller.revise().unwrap();
        assert_eq!(revised.snapshot.status, TaskStatus::Paused);
        assert_eq!(revised.snapshot.revision, 3);
    }

    #[test]
    fn update_checks_seq_revision_plan_and_size() {
        let mut controller = TaskController::new("goal");
        let start = controller.start(scope(7)).unwrap();
        let task_id = start.snapshot.id.clone();
        controller.commit(start).unwrap();
        let checks = vec![CheckSpec::new("C1", "cargo test", CheckSource::Derived)];
        let first = controller
            .update(1, 1, json!({"summary": "s"}), Some(checks.clone()))
            .unwrap();
        assert_eq!(first.snapshot.id, task_id);
        assert_eq!(first.snapshot.seq, 2);
        assert_eq!(
            first.snapshot.verification_epoch, 1,
            "registering a plan keeps actual execution facts"
        );
        assert_eq!(
            first.snapshot.checkpoint_basis,
            Some(CheckpointBasis {
                revision: 1,
                verification_epoch: 1,
            })
        );
        assert_eq!(
            TaskControlSnapshot::from_item(&first.snapshot.to_item()).unwrap(),
            first.snapshot
        );
        controller.commit(first).unwrap();

        assert_eq!(
            controller.update(1, 1, json!({}), None).unwrap_err(),
            ControlRejection::StaleSeq {
                expected: 2,
                given: 1
            }
        );
        assert_eq!(
            controller.update(2, 5, json!({}), None).unwrap_err(),
            ControlRejection::StaleRevision {
                expected: 1,
                given: 5
            }
        );
        let same_plan = controller
            .update(2, 1, json!({"summary": "t"}), Some(checks))
            .unwrap();
        assert_eq!(
            same_plan.snapshot.verification_epoch, 1,
            "an unchanged plan keeps the round"
        );

        let duplicate = vec![
            CheckSpec::new("C1", "cargo test", CheckSource::Derived),
            CheckSpec::new("C1", "cargo build", CheckSource::User),
        ];
        assert!(matches!(
            controller
                .update(2, 1, json!({}), Some(duplicate))
                .unwrap_err(),
            ControlRejection::Invalid(_)
        ));
        let oversized = json!({"blob": "x".repeat(MAX_CONTROL_BYTES)});
        assert!(matches!(
            controller.update(2, 1, oversized, None).unwrap_err(),
            ControlRejection::TooLarge { .. }
        ));
        let accepted = controller
            .update(2, 1, json!({"summary": "u"}), None)
            .unwrap();
        assert_eq!(accepted.snapshot.seq, 3);
        assert!(accepted.requires_commit);
    }

    #[test]
    fn execution_receipts_keep_raw_facts_and_stay_host_private() {
        let passed = ExecutionReceipt::shell("call-1", "cargo test", "/repo", false, Some(0))
            .with_control("task-1", 1, 2);
        assert_eq!(passed.outcome(), ReceiptOutcome::Succeeded);
        assert_eq!(
            ExecutionReceipt::shell("c2", "cargo test", "/repo", false, None).outcome(),
            ReceiptOutcome::Unknown
        );
        assert_eq!(
            ExecutionReceipt::shell("c3", "cargo test", "/repo", false, Some(101)).outcome(),
            ReceiptOutcome::Failed
        );
        assert_eq!(
            ExecutionReceipt::shell("c4", "cargo test", "/repo", true, Some(0)).outcome(),
            ReceiptOutcome::Failed
        );

        let mut message = Message::tool("call-1".into(), "ok".into());
        assert!(passed.attach(&mut message));
        assert_eq!(
            ExecutionReceipt::from_message(&message),
            Some(passed.clone())
        );
        assert_eq!(
            ExecutionReceipt::collect(&[message.clone()], "task-1"),
            vec![passed]
        );
        assert!(ExecutionReceipt::collect(&[message.clone()], "task-2").is_empty());

        let mut projected = message;
        strip_host_metadata(&mut projected);
        assert_eq!(projected.provider_items, None);
        assert!(ExecutionReceipt::from_message(&projected).is_none());
    }
    fn running_snapshot() -> (TaskController, TaskControlSnapshot) {
        let mut controller = TaskController::new("goal");
        let started = controller
            .start(scope(1))
            .expect("start is allowed without a task");
        controller.commit(started).expect("commit");
        let update = controller
            .update(
                1,
                1,
                json!({"summary": "work", "claim": "working"}),
                Some(vec![CheckSpec::new("T1", "echo ok", CheckSource::Derived)]),
            )
            .expect("register one check");
        let snapshot = update.snapshot.clone();
        controller.commit(update).expect("commit");
        (controller, snapshot)
    }

    #[test]
    fn user_sources_exclude_host_notes_and_keep_real_positions() {
        let mut history = vec![
            Message::new("system", "system prompt"),
            Message::new("user", "/goal fix login"),
        ];
        let mut note = Message::new("user", "[任务控制参考] keep going");
        note.provider_items = Some(vec![json!({"type": CONTROL_NOTE_ITEM_TYPE})]);
        history.push(note);
        history.push(Message::new("assistant", "working"));
        history.push(Message::new("user", "add a regression test"));

        let sources = user_sources(&history);
        assert_eq!(
            sources,
            vec![
                UserSource {
                    message_index: 1,
                    content: "/goal fix login".into()
                },
                UserSource {
                    message_index: 4,
                    content: "add a regression test".into()
                }
            ]
        );
        assert!(is_control_note(&history[2]));
        assert!(!is_control_note(&history[1]));
    }

    #[test]
    fn check_sources_must_quote_the_real_user_message() {
        let sources = vec![UserSource {
            message_index: 3,
            content: "修复登录失败，补上回归测试".into(),
        }];
        let mut good = CheckSpec::new("T1", "npm test", CheckSource::User);
        good.source_ref = Some(SourceRef {
            message_index: 3,
            quote: "补上回归测试".into(),
        });
        assert!(validate_checks(&[good.clone()]).is_ok());
        assert!(validate_check_sources(&[good], &sources).is_ok());

        let mut wrong_quote = CheckSpec::new("T1", "npm test", CheckSource::User);
        wrong_quote.source_ref = Some(SourceRef {
            message_index: 3,
            quote: "补上单元测试".into(),
        });
        assert!(validate_check_sources(&[wrong_quote], &sources).is_err());

        let mut wrong_index = CheckSpec::new("T1", "npm test", CheckSource::User);
        wrong_index.source_ref = Some(SourceRef {
            message_index: 9,
            quote: "补上回归测试".into(),
        });
        assert!(validate_check_sources(&[wrong_index], &sources).is_err());

        // Source references are optional, but supplied references are checked.
        let missing = CheckSpec::new("T1", "npm test", CheckSource::User);
        assert!(
            validate_checks(&[missing]).is_ok(),
            "source annotations do not force a citation form"
        );
    }

    #[test]
    fn control_proposals_are_strict_and_cannot_forge_identity() {
        let start = json!({"op": "start", "base_seq": 0, "data": {"summary": "x"}});
        let parsed = ControlProposal::parse(&start).expect("valid start");
        assert_eq!(parsed.op, ControlOp::Start);
        assert_eq!(parsed.base_seq, 0);

        for bad in [
            json!({"op": "complete", "data": {}}),
            json!({"op": "start", "owner": "evil", "data": {}}),
            json!({"op": "start", "status": "completed", "data": {}}),
            json!({"op": "start", "data": [1, 2]}),
            json!({"op": "update", "base_seq": 1}),
            json!({"base_seq": 1, "data": {}}),
            json!({"op": "update", "base_seq": 1, "id": 3, "data": {}}),
            json!({"op": "update", "base_seq": 1, "revision": "2", "data": {}}),
        ] {
            assert!(ControlProposal::parse(&bad).is_err(), "must reject {bad}");
        }
    }

    #[test]
    fn a_passing_check_only_counts_in_the_round_it_ran() {
        let (_controller, snapshot) = running_snapshot();
        let mut facts = FactIndex::default();
        let mut receipt = ExecutionReceipt::shell("call-1", "echo ok", "work", false, Some(0))
            .with_control(
                snapshot.id.clone(),
                snapshot.revision,
                snapshot.verification_epoch,
            );
        receipt.check_id = Some("T1".into());
        facts.record(receipt);
        let status = facts.status(&snapshot, "work");
        assert_eq!(status.passed, vec!["T1".to_string()]);
        let good = finish_facts(&snapshot, &facts, "work");
        assert!(good.checks_satisfied);
        assert!(good.checkpoint_basis_valid);
        assert!(good.host_blockers(true).is_empty());

        // A failing or unknown execution never counts as passing.
        let mut failed = facts.clone();
        let mut receipt = ExecutionReceipt::shell("call-2", "echo ok", "work", false, Some(1))
            .with_control(
                snapshot.id.clone(),
                snapshot.revision,
                snapshot.verification_epoch,
            );
        receipt.check_id = Some("T1".into());
        failed.record(receipt);
        assert!(!finish_facts(&snapshot, &failed, "work").checks_satisfied);

        // A new verification round invalidates the old pass.
        let mut next_round = snapshot.clone();
        next_round.verification_epoch += 1;
        let stale = finish_facts(&next_round, &facts, "work");
        assert!(!stale.checks_satisfied);
        assert!(!stale.checkpoint_basis_valid);
        assert_eq!(stale.host_blockers(true).len(), 2);
    }
    fn start_record(id: &str) -> Value {
        let controller = TaskController::new("goal");
        let mut update = controller
            .start(ControlScope::new("sessions/a.json", 2))
            .unwrap();
        update.snapshot.id = id.to_string();
        update
            .snapshot
            .to_item()
            .as_object()
            .cloned()
            .expect("control item")
            .into()
    }

    #[test]
    fn a_branched_history_keeps_only_a_requirement_with_its_real_user_turn() {
        let mut user = Message::new("user", "/goal 修复登录失败");
        user.provider_items = Some(vec![start_record("task-origin")]);
        let history = vec![
            Message::new("system", "prompt"),
            user.clone(),
            Message::new("assistant", "working"),
        ];
        let origin = suspended_origin(&history).expect("the user turn is still present");
        assert_eq!(origin.id, "task-origin");
        assert!(origin.is_active());

        // The same record on a host note is not the user's requirement.
        let mut note = Message::new("user", "[任务控制参考] 继续");
        note.provider_items = Some(vec![
            json!({"type": CONTROL_NOTE_ITEM_TYPE, "source": "host"}),
            start_record("task-note"),
        ]);
        assert!(suspended_origin(&[history[0].clone(), note, history[2].clone()]).is_none());

        // A truncated prefix has no requirement to carry over.
        assert!(suspended_origin(&history[2..]).is_none());

        // Terminal records are never carried into a new generation.
        let mut done = start_record("task-done");
        done["status"] = json!("completed");
        let mut message = Message::new("user", "/goal 修复登录失败");
        message.provider_items = Some(vec![done]);
        assert!(suspended_origin(&[message]).is_none());
    }
}
