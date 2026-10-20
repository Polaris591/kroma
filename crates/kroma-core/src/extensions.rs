//! Gate 3 — reversible scoped extensions, dual-class hooks, subtree JIT context, and skills.
//!
//! Three invariants define this module:
//!
//! 1. **Reversibility.** Registering an extension adds tools/hooks/skills; unregistering it must
//!    restore the registry byte-for-byte. Extension tools can never shadow a kernel tool, and they
//!    remain subject to the kernel `PolicyEngine` and `--tool-filter` exactly like built-ins.
//! 2. **Fail-closed policy, fail-open observation.** A `PolicyHook` may only ever *restrict*: a
//!    denial, an error, a panic, a non-zero exit or a timeout blocks the action. An `ObserverHook`
//!    is telemetry only: any failure is recorded and the action proceeds.
//! 3. **Additive context.** Subtree `AGENTS.md`/`CLAUDE.md` guidance and activated skills are
//!    appended to *new* tool results or the initial prefix — never to an already-sent turn — so the
//!    frozen prefix KV cache of earlier turns stays intact.

use crate::budget::{RunDeadline, TokenBudget};
use crate::env::ExecutionContext;
use crate::messages::ToolResultMeta;
use crate::prompt::{escape_fence_attribute, fence_untrusted};
use crate::tool::definition::{RegisteredTool, ToolDefinition, ToolMetadata};
use crate::tool::output_buffer::floor_char_boundary;
use crate::tool::policy::PolicyEngine;
use crate::tools::ToolExecutionOutput;
use crate::types::{EffectClass, ErrorCode, RunId};
use crate::workspace::{WorkspaceCap, WorkspaceError};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::io::Read;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Tools the kernel owns. An extension may never register, replace or remove one of these, so a
/// project-supplied extension cannot widen its own authority by shadowing `bash` or `read`.
pub const RESERVED_KERNEL_TOOLS: &[&str] = &[
    "activate_skill",
    "bash",
    "code_graph",
    "codemode",
    "discover_tools",
    "edit",
    "find",
    "grep",
    "ls",
    "read",
    "recall_evidence",
    "spawn_subagent",
    "write",
];

/// Default wall-clock budget for a single hook evaluation.
pub const DEFAULT_HOOK_TIMEOUT: Duration = Duration::from_millis(5_000);
/// Directory holding on-demand skill definitions, relative to the workspace root.
pub const SKILL_DIR: &str = ".agents/skills";
/// Additional compatibility skill directory (the Claude Code layout), scanned after
/// [`SKILL_DIR`]. A name collision is resolved in favour of `.agents/skills`.
pub const CLAUDE_SKILL_DIR: &str = ".claude/skills";
/// Skill directories scanned by [`discover_skills`], in precedence order.
pub const SKILL_DIRS: &[&str] = &[SKILL_DIR, CLAUDE_SKILL_DIR];
/// Hard cap on the number of activation `paths` globs retained from one `SKILL.md`.
pub const MAX_SKILL_PATHS: usize = 32;
/// Hard cap on a single `SKILL.md` read from disk.
pub const SKILL_FILE_MAX_BYTES: usize = 32 * 1024;
/// Hard cap on the skill body rendered into the model context.
pub const SKILL_BODY_MAX_BYTES: usize = 16 * 1024;
/// Hard cap on the guidance read from a single ancestor directory (`AGENTS.md`/`CLAUDE.md`).
pub const SUBTREE_DOC_MAX_BYTES: usize = 8 * 1024;
/// Hard cap on the total JIT subtree guidance injected across one run.
pub const SUBTREE_TOTAL_MAX_BYTES: usize = 32 * 1024;
/// Hard cap on the bytes captured from a command hook's stdout/stderr. The reader is started
/// before the child is awaited, so a noisy hook can never fill an OS pipe buffer and deadlock.
const HOOK_OUTPUT_LIMIT_BYTES: usize = 4096;
/// Bounded post-exit drain window. `CommandHookRunner::run` must never block forever on a pipe a
/// grandchild process still holds open, so the reader results are collected with a deadline.
const HOOK_DRAIN_DEADLINE: Duration = Duration::from_millis(200);
/// Grace between `SIGTERM` and `SIGKILL` when sweeping a hook's process group.
const HOOK_KILL_GRACE: Duration = Duration::from_millis(20);
/// How long `unregister_extension` waits for in-flight calls to observe the cancellation signal
/// before reporting them as cancelled-but-not-drained.
const UNMOUNT_DRAIN_WINDOW: Duration = Duration::from_millis(250);

// ---------------------------------------------------------------------------------------------
// Extension registration
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ExtensionId(pub String);

impl ExtensionId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ExtensionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Where an extension came from, and therefore how much it is trusted. A project-scoped extension
/// carries no additional OS privilege merely by being loaded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionTrustLevel {
    /// Shipped by the operator on the command line (e.g. `--policy-hook`).
    OperatorTrusted,
    /// Discovered inside the target repository; untrusted project data.
    ProjectUntrusted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtensionManifest {
    pub name: String,
    pub version: String,
    /// Version of the extension API this extension was written against.
    pub api_version: String,
    /// Declared capabilities, e.g. `["tools", "policy_hook"]`.
    pub capabilities: Vec<String>,
    pub trust_level: ExtensionTrustLevel,
    /// Where the extension was loaded from (CLI flag, workspace-relative path, ...).
    pub source: String,
    /// Optional content digest of the extension payload, for re-trust decisions.
    pub checksum: Option<String>,
}

impl ExtensionManifest {
    pub fn operator_trusted(name: impl Into<String>, source: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            version: "0.1.0".to_string(),
            api_version: EXTENSION_API_VERSION.to_string(),
            capabilities: Vec::new(),
            trust_level: ExtensionTrustLevel::OperatorTrusted,
            source: source.into(),
            checksum: None,
        }
    }
}

/// Extension API version understood by this build.
pub const EXTENSION_API_VERSION: &str = "1.0.0";

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum ExtensionError {
    #[error("extension '{0}' is already registered")]
    DuplicateExtension(String),
    #[error("extension '{0}' is not registered")]
    UnknownExtension(String),
    #[error("tool name '{0}' is reserved by the kernel and cannot be shadowed by an extension")]
    ReservedKernelTool(String),
    #[error("tool name '{0}' is already provided by another mounted extension")]
    ToolNameTaken(String),
    #[error("extension '{extension}' declares tool '{tool}' twice")]
    DuplicateTool { extension: String, tool: String },
    #[error("skill name '{0}' is already registered")]
    DuplicateSkill(String),
    #[error(
        "extension '{extension}' targets unsupported API version '{found}' (expected {expected})"
    )]
    UnsupportedApiVersion {
        extension: String,
        found: String,
        expected: String,
    },
    #[error("extension '{extension}' contributes '{capability}' without declaring it in manifest.capabilities")]
    UndeclaredCapability {
        extension: String,
        capability: String,
    },
    #[error("extension '{extension}' is project-untrusted and may not register '{capability}'")]
    UntrustedCapability {
        extension: String,
        capability: String,
    },
    #[error(
        "extension '{extension}' source checksum changed (trusted {expected}, found {found}); re-trust it explicitly before mounting"
    )]
    ChecksumMismatch {
        extension: String,
        expected: String,
        found: String,
    },
    #[error("invalid extension manifest: {0}")]
    InvalidManifest(String),
}

/// Skill names are used as fence attributes and catalog keys, so they are restricted to a
/// conservative charset that cannot forge prompt structure.
pub fn is_valid_skill_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// Host-side context handed to an extension tool handler. Deliberately narrow: a handler gets the
/// capability-guarded workspace and nothing else, so loading an extension cannot widen OS access.
pub struct CustomToolContext<'a> {
    pub workspace: &'a WorkspaceCap,
    /// Set when the call was dispatched by a mounted extension. A long-running handler may poll
    /// [`CustomToolContext::is_cancelled`] to stop early when the extension is unmounted mid-call.
    pub in_flight: Option<Arc<InFlightTracker>>,
    /// F05: Unified execution context of the enclosing run.
    pub exec_context: Option<&'a ExecutionContext>,
}

impl<'a> CustomToolContext<'a> {
    pub fn new(workspace: &'a WorkspaceCap) -> Self {
        Self {
            workspace,
            in_flight: None,
            exec_context: None,
        }
    }

    pub fn with_exec_context(
        workspace: &'a WorkspaceCap,
        exec_context: Option<&'a ExecutionContext>,
    ) -> Self {
        Self {
            workspace,
            in_flight: None,
            exec_context,
        }
    }

    /// True once the owning extension was unmounted OR the enclosing run was cancelled;
    /// the handler should return promptly.
    pub fn is_cancelled(&self) -> bool {
        if self
            .in_flight
            .as_ref()
            .is_some_and(|tracker| tracker.is_cancelled())
        {
            return true;
        }
        if self.exec_context.is_some_and(|ec| ec.is_cancelled()) {
            return true;
        }
        false
    }

    pub fn deadline(&self) -> Option<RunDeadline> {
        self.exec_context.and_then(|ec| ec.deadline)
    }

    pub fn run_id(&self) -> Option<RunId> {
        self.exec_context.and_then(|ec| ec.run_id)
    }

    pub fn budget(&self) -> Option<&Arc<TokenBudget>> {
        self.exec_context.and_then(|ec| ec.budget.as_ref())
    }

    pub fn policy(&self) -> Option<&Arc<PolicyEngine>> {
        self.exec_context.and_then(|ec| ec.policy.as_ref())
    }
}

/// Lifecycle state of one extension tool call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallLifecycle {
    Active,
    Residual,
    Dropped,
}

#[derive(Debug)]
struct TrackerState {
    active: usize,
    residual: usize,
    calls: HashMap<u64, CallLifecycle>,
    next_call_id: u64,
}

/// Default concurrent call limit per mounted extension tool.
pub const DEFAULT_MAX_CONCURRENT_CALLS_PER_EXTENSION: usize = 8;

/// Errors when admitting a call into an extension's in-flight execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdmissionError {
    Unmounted,
    OverlappingResidual { residual: usize },
    CapacityExceeded { active: usize, limit: usize },
}

/// Per-extension in-flight call accounting.
///
/// Shared between the registry (which owns the mount lifecycle) and every dispatch of an extension
/// tool, so `unregister_extension` can stop new calls, signal cancellation to the calls already
/// running, and report exactly what it could not wait for or undo.
///
/// Also enforces bounded admission and refuses new overlapping calls while a residual background
/// execution from a cancelled/timed-out blocking call is still active.
pub struct InFlightTracker {
    extension_id: ExtensionId,
    state: Mutex<TrackerState>,
    max_concurrent: usize,
    cancelled: AtomicBool,
    unmounted: AtomicBool,
    side_effects: Mutex<Vec<String>>,
}

impl fmt::Debug for InFlightTracker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (active, residual) = {
            let s = self.state.lock().unwrap_or_else(|e| e.into_inner());
            (s.active, s.residual)
        };
        f.debug_struct("InFlightTracker")
            .field("extension_id", &self.extension_id)
            .field("active", &active)
            .field("residual", &residual)
            .field("max_concurrent", &self.max_concurrent)
            .field("cancelled", &self.cancelled.load(Ordering::SeqCst))
            .finish()
    }
}

impl InFlightTracker {
    fn new(extension_id: ExtensionId) -> Self {
        Self {
            extension_id,
            state: Mutex::new(TrackerState {
                active: 0,
                residual: 0,
                calls: HashMap::new(),
                next_call_id: 1,
            }),
            max_concurrent: DEFAULT_MAX_CONCURRENT_CALLS_PER_EXTENSION,
            cancelled: AtomicBool::new(false),
            unmounted: AtomicBool::new(false),
            side_effects: Mutex::new(Vec::new()),
        }
    }

    pub fn extension_id(&self) -> &ExtensionId {
        &self.extension_id
    }

    /// Attempt to admit a new call subject to mount lifecycle, residual blocking exclusion,
    /// and concurrent capacity limits.
    ///
    /// Atomic admission: checks unmounted, residual, and capacity, and transitions new call
    /// to `Active` under the tracker's internal lock.
    pub fn begin_admission(self: &Arc<Self>) -> Result<InFlightCall, AdmissionError> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if self.unmounted.load(Ordering::SeqCst) {
            return Err(AdmissionError::Unmounted);
        }
        if state.residual > 0 {
            return Err(AdmissionError::OverlappingResidual {
                residual: state.residual,
            });
        }
        if state.active >= self.max_concurrent {
            return Err(AdmissionError::CapacityExceeded {
                active: state.active,
                limit: self.max_concurrent,
            });
        }
        let call_id = state.next_call_id;
        state.next_call_id = state.next_call_id.wrapping_add(1).max(1);
        state.active += 1;
        state.calls.insert(call_id, CallLifecycle::Active);
        drop(state);

        Ok(InFlightCall {
            tracker: Arc::clone(self),
            call_id,
        })
    }

    /// Register a new in-flight call (legacy compatibility).
    pub fn begin(self: &Arc<Self>) -> Option<InFlightCall> {
        self.begin_admission().ok()
    }

    /// Mark an active call as residual under the tracker's internal lock.
    /// If the call was already dropped, this is a no-op and never increments residual.
    pub fn mark_call_residual(&self, call_id: u64) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(lifecycle) = state.calls.get_mut(&call_id) {
            if *lifecycle == CallLifecycle::Active {
                *lifecycle = CallLifecycle::Residual;
                state.residual += 1;
            }
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    pub fn is_unmounted(&self) -> bool {
        self.unmounted.load(Ordering::SeqCst)
    }

    /// Record that a `Mutating`/`SideEffect` call completed. These are the effects an unmount
    /// cannot roll back, so they are reported in the [`UnmountReport`].
    pub fn record_side_effect(&self, tool_name: &str) {
        if let Ok(mut guard) = self.side_effects.lock() {
            guard.push(tool_name.to_string());
        }
    }

    pub fn active(&self) -> usize {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).active
    }

    pub fn residual_active(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .residual
    }

    fn side_effects_snapshot(&self) -> Vec<String> {
        self.side_effects
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }
}

/// Handle held by the caller to mark an in-flight blocking call as residual upon cancel/deadline.
#[derive(Clone, Debug)]
pub struct ResidualHandle {
    tracker: Arc<InFlightTracker>,
    call_id: u64,
}

impl ResidualHandle {
    pub fn mark_residual(&self) {
        self.tracker.mark_call_residual(self.call_id);
    }

    pub fn call_id(&self) -> u64 {
        self.call_id
    }
}

/// RAII guard for one in-flight extension call; dropping it ends the call's accounting.
#[derive(Debug)]
pub struct InFlightCall {
    tracker: Arc<InFlightTracker>,
    call_id: u64,
}

impl InFlightCall {
    pub fn call_id(&self) -> u64 {
        self.call_id
    }

    pub fn residual_handle(&self) -> ResidualHandle {
        ResidualHandle {
            tracker: Arc::clone(&self.tracker),
            call_id: self.call_id,
        }
    }

    pub fn mark_residual(&self) {
        self.tracker.mark_call_residual(self.call_id);
    }
}

impl Drop for InFlightCall {
    fn drop(&mut self) {
        let mut state = self.tracker.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(lifecycle) = state.calls.remove(&self.call_id) {
            match lifecycle {
                CallLifecycle::Active => {
                    state.active = state.active.saturating_sub(1);
                }
                CallLifecycle::Residual => {
                    state.active = state.active.saturating_sub(1);
                    state.residual = state.residual.saturating_sub(1);
                }
                CallLifecycle::Dropped => {}
            }
        }
    }
}

/// Result of unmounting an extension: what was cancelled, what drained in time, and which effects
/// were already committed and therefore cannot be reversed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnmountReport {
    pub extension_id: ExtensionId,
    /// In-flight calls that were still running when the drain window expired.
    pub cancelled_in_flight: usize,
    /// In-flight calls that observed the cancellation signal and finished within the drain window.
    pub drained_in_flight: usize,
    /// `Mutating`/`SideEffect` extension tool calls that completed before the unmount.
    pub irreversible_side_effects: Vec<String>,
}

/// A resolved extension tool dispatch: the handler plus the in-flight bookkeeping the runtime needs
/// to gate the call against a concurrent unmount.
#[derive(Clone)]
pub struct ExtensionDispatch {
    pub extension_id: ExtensionId,
    pub tool_name: String,
    pub effect_class: EffectClass,
    pub handler: Arc<dyn CustomToolHandler>,
    pub in_flight: Arc<InFlightTracker>,
}

/// Execution contract for an extension-provided tool.
pub trait CustomToolHandler: Send + Sync {
    fn execute(&self, raw_arguments: &str, ctx: &CustomToolContext<'_>) -> ToolExecutionOutput;
}

impl<F> CustomToolHandler for F
where
    F: Fn(&str, &CustomToolContext<'_>) -> ToolExecutionOutput + Send + Sync,
{
    fn execute(&self, raw_arguments: &str, ctx: &CustomToolContext<'_>) -> ToolExecutionOutput {
        self(raw_arguments, ctx)
    }
}

/// A model-visible tool contributed by an extension, together with its host-side handler.
pub struct ScopedToolDefinition {
    pub definition: ToolDefinition,
    pub metadata: ToolMetadata,
    pub handler: Arc<dyn CustomToolHandler>,
}

impl fmt::Debug for ScopedToolDefinition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ScopedToolDefinition")
            .field("name", &self.definition.name)
            .field("effect_class", &self.metadata.effect_class)
            .finish()
    }
}

impl ScopedToolDefinition {
    pub fn new(
        definition: ToolDefinition,
        metadata: ToolMetadata,
        handler: Arc<dyn CustomToolHandler>,
    ) -> Self {
        Self {
            definition,
            metadata,
            handler,
        }
    }
}

/// Everything a single extension contributes. All of it is removed again by
/// [`ToolRegistry::unregister_extension`].
pub struct ScopedExtension {
    pub manifest: ExtensionManifest,
    pub tools: Vec<ScopedToolDefinition>,
    pub policy_hooks: Vec<Arc<dyn PolicyHook>>,
    pub observer_hooks: Vec<Arc<dyn ObserverHook>>,
    pub skills: Vec<SkillEntry>,
    /// Optional teardown callback, invoked once when the extension is unmounted.
    pub cleanup: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl ScopedExtension {
    pub fn new(manifest: ExtensionManifest) -> Self {
        Self {
            manifest,
            tools: Vec::new(),
            policy_hooks: Vec::new(),
            observer_hooks: Vec::new(),
            skills: Vec::new(),
            cleanup: None,
        }
    }

    pub fn with_tool(mut self, tool: ScopedToolDefinition) -> Self {
        self.tools.push(tool);
        self
    }

    pub fn with_policy_hook(mut self, hook: Arc<dyn PolicyHook>) -> Self {
        self.policy_hooks.push(hook);
        self
    }

    pub fn with_observer_hook(mut self, hook: Arc<dyn ObserverHook>) -> Self {
        self.observer_hooks.push(hook);
        self
    }

    pub fn with_skill(mut self, skill: SkillEntry) -> Self {
        self.skills.push(skill);
        self
    }

    pub fn with_cleanup(mut self, cleanup: Arc<dyn Fn() + Send + Sync>) -> Self {
        self.cleanup = Some(cleanup);
        self
    }
}

/// A mounted extension. Dropping it does not unmount; the registry owns the lifecycle so unmount
/// is always explicit and observable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionHandle {
    pub id: ExtensionId,
    pub manifest: ExtensionManifest,
}

#[derive(Clone)]
struct RegisteredExtensionTool {
    definition: ToolDefinition,
    metadata: ToolMetadata,
    handler: Arc<dyn CustomToolHandler>,
    extension_id: ExtensionId,
    in_flight: Arc<InFlightTracker>,
}

impl fmt::Debug for RegisteredExtensionTool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RegisteredExtensionTool")
            .field("name", &self.definition.name)
            .field("extension_id", &self.extension_id)
            .finish()
    }
}

/// Shared, interior-mutable extension registry state.
///
/// `ToolRegistry` clones share one `ExtensionState`, so every clone observes the same mount set and
/// an `unregister_extension` is immediately visible to all in-flight clones.
#[derive(Default)]
pub struct ExtensionState {
    tools: BTreeMap<String, RegisteredExtensionTool>,
    policy_hooks: Vec<(ExtensionId, Arc<dyn PolicyHook>)>,
    observer_hooks: Vec<(ExtensionId, Arc<dyn ObserverHook>)>,
    skills: BTreeMap<String, SkillEntry>,
    manifests: BTreeMap<ExtensionId, ExtensionManifest>,
    /// Trusted source checksums, keyed by extension id. Deliberately survives an unmount so a
    /// changed extension source must be re-trusted before it can be mounted again.
    trusted_checksums: BTreeMap<String, String>,
    /// Per-extension teardown callbacks, run once on unmount.
    cleanups: BTreeMap<String, Vec<Arc<dyn Fn() + Send + Sync>>>,
}

impl fmt::Debug for ExtensionState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ExtensionState")
            .field("tools", &self.tools.keys().collect::<Vec<_>>())
            .field("policy_hooks", &self.policy_hooks.len())
            .field("observer_hooks", &self.observer_hooks.len())
            .field("skills", &self.skills.keys().collect::<Vec<_>>())
            .field("extensions", &self.manifests.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl ExtensionState {
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
            && self.policy_hooks.is_empty()
            && self.observer_hooks.is_empty()
            && self.skills.is_empty()
            && self.manifests.is_empty()
    }

    pub fn extension_ids(&self) -> Vec<ExtensionId> {
        self.manifests.keys().cloned().collect()
    }

    pub fn extension_tools(&self) -> Vec<RegisteredTool> {
        self.tools
            .values()
            .map(|tool| RegisteredTool {
                definition: tool.definition.clone(),
                metadata: tool.metadata.clone(),
            })
            .collect()
    }

    pub fn dispatch_for(&self, name: &str) -> Option<ExtensionDispatch> {
        self.tools.get(name).map(|tool| ExtensionDispatch {
            extension_id: tool.extension_id.clone(),
            tool_name: tool.definition.name.clone(),
            effect_class: tool.metadata.effect_class,
            handler: Arc::clone(&tool.handler),
            in_flight: Arc::clone(&tool.in_flight),
        })
    }

    /// Re-trust an extension whose source checksum changed. Operator-only: a project-supplied
    /// extension can never grant itself this.
    pub fn retrust_checksum(&mut self, id: &ExtensionId, checksum: impl Into<String>) {
        self.trusted_checksums.insert(id.0.clone(), checksum.into());
    }

    pub fn trusted_checksum(&self, id: &ExtensionId) -> Option<&str> {
        self.trusted_checksums.get(&id.0).map(|s| s.as_str())
    }

    pub fn policy_hooks(&self) -> Vec<Arc<dyn PolicyHook>> {
        self.policy_hooks
            .iter()
            .map(|(_, hook)| Arc::clone(hook))
            .collect()
    }

    pub fn observer_hooks(&self) -> Vec<Arc<dyn ObserverHook>> {
        self.observer_hooks
            .iter()
            .map(|(_, hook)| Arc::clone(hook))
            .collect()
    }

    pub fn extension_skills(&self) -> Vec<SkillEntry> {
        self.skills.values().cloned().collect()
    }

    /// Mount an extension. Validation happens before any mutation, so a rejected mount leaves the
    /// registry exactly as it was (the "init failure rolls back" invariant).
    pub fn register(&mut self, ext: ScopedExtension) -> Result<ExtensionId, ExtensionError> {
        if ext.manifest.api_version != EXTENSION_API_VERSION {
            return Err(ExtensionError::UnsupportedApiVersion {
                extension: ext.manifest.name.clone(),
                found: ext.manifest.api_version.clone(),
                expected: EXTENSION_API_VERSION.to_string(),
            });
        }
        if ext.manifest.name.trim().is_empty() {
            return Err(ExtensionError::InvalidManifest(
                "extension name must not be empty".to_string(),
            ));
        }
        let id = ExtensionId::new(ext.manifest.name.clone());
        if self.manifests.contains_key(&id) {
            return Err(ExtensionError::DuplicateExtension(id.0));
        }

        // Capability matrix: every contributed surface must be declared in `manifest.capabilities`,
        // so an extension cannot silently acquire a capability it never advertised.
        let declared: BTreeSet<&str> = ext
            .manifest
            .capabilities
            .iter()
            .map(|capability| capability.as_str())
            .collect();
        let require_capability = |capability: &str| -> Result<(), ExtensionError> {
            if declared.contains(capability) {
                Ok(())
            } else {
                Err(ExtensionError::UndeclaredCapability {
                    extension: id.0.clone(),
                    capability: capability.to_string(),
                })
            }
        };
        if !ext.tools.is_empty() {
            require_capability("tools")?;
        }
        if !ext.policy_hooks.is_empty() {
            require_capability("policy_hook")?;
        }
        if !ext.observer_hooks.is_empty() {
            require_capability("observer_hook")?;
        }
        if !ext.skills.is_empty() {
            require_capability("skills")?;
        }

        // Trust boundary: a project-discovered extension may only ever contribute read-only tools,
        // telemetry hooks and skills. Gating tool use (`PolicyHook`) or mutating the workspace
        // requires explicit operator trust.
        if ext.manifest.trust_level == ExtensionTrustLevel::ProjectUntrusted {
            if !ext.policy_hooks.is_empty() {
                return Err(ExtensionError::UntrustedCapability {
                    extension: id.0.clone(),
                    capability: "policy_hook".to_string(),
                });
            }
            for tool in &ext.tools {
                if tool.metadata.effect_class != EffectClass::ReadOnly {
                    return Err(ExtensionError::UntrustedCapability {
                        extension: id.0.clone(),
                        capability: format!(
                            "tool '{}' ({:?})",
                            tool.definition.name, tool.metadata.effect_class
                        ),
                    });
                }
            }
        }

        // Source checksum re-trust: once a digest is pinned, a different digest is refused until an
        // operator explicitly re-trusts the extension.
        if let Some(checksum) = ext.manifest.checksum.as_deref() {
            if let Some(pinned) = self.trusted_checksums.get(&id.0) {
                if pinned != checksum {
                    return Err(ExtensionError::ChecksumMismatch {
                        extension: id.0.clone(),
                        expected: pinned.clone(),
                        found: checksum.to_string(),
                    });
                }
            }
        }

        // Validate the whole contribution first: no partial mount on failure.
        let mut staged: Vec<(String, RegisteredExtensionTool)> = Vec::new();
        let mut seen: BTreeSet<String> = BTreeSet::new();
        for tool in ext.tools {
            let name = tool.definition.name.clone();
            if RESERVED_KERNEL_TOOLS.contains(&name.as_str()) {
                return Err(ExtensionError::ReservedKernelTool(name));
            }
            if self.tools.contains_key(&name) {
                return Err(ExtensionError::ToolNameTaken(name));
            }
            if !seen.insert(name.clone()) {
                return Err(ExtensionError::DuplicateTool {
                    extension: id.0.clone(),
                    tool: name,
                });
            }
            staged.push((
                name.clone(),
                RegisteredExtensionTool {
                    definition: tool.definition,
                    metadata: tool.metadata,
                    handler: tool.handler,
                    extension_id: id.clone(),
                    in_flight: Arc::new(InFlightTracker::new(id.clone())),
                },
            ));
        }
        let mut seen_skills: BTreeSet<String> = BTreeSet::new();
        for skill in &ext.skills {
            let name = skill.name.trim();
            if name.is_empty() {
                return Err(ExtensionError::InvalidManifest(
                    "skill name must not be empty".to_string(),
                ));
            }
            if !is_valid_skill_name(name) {
                return Err(ExtensionError::InvalidManifest(format!(
                    "invalid skill name '{}': expected characters [A-Za-z0-9._-]",
                    name
                )));
            }
            // A collision would overwrite another extension's (or a workspace) skill and break the
            // byte-for-byte reversibility guarantee, so it is rejected before anything is committed.
            if !seen_skills.insert(name.to_string()) || self.skills.contains_key(name) {
                return Err(ExtensionError::DuplicateSkill(name.to_string()));
            }
        }

        // Commit.
        for (name, tool) in staged {
            self.tools.insert(name, tool);
        }
        for hook in ext.policy_hooks {
            self.policy_hooks.push((id.clone(), hook));
        }
        for hook in ext.observer_hooks {
            self.observer_hooks.push((id.clone(), hook));
        }
        for skill in ext.skills {
            let mut skill = skill;
            // Tag contributed skills with their owner so `unregister` can remove exactly them.
            skill.extension_id = Some(id.clone());
            self.skills.insert(skill.name.clone(), skill);
        }
        if let Some(checksum) = ext.manifest.checksum.clone() {
            self.trusted_checksums.insert(id.0.clone(), checksum);
        }
        if let Some(cleanup) = ext.cleanup {
            self.cleanups.entry(id.0.clone()).or_default().push(cleanup);
        }
        self.manifests.insert(id.clone(), ext.manifest);
        Ok(id)
    }

    /// Unmount an extension and restore every surface to its pre-mount state.
    ///
    /// New dispatches stop immediately, in-flight calls are signalled and given a bounded drain
    /// window, extension cleanup callbacks run, and the report records what could not be drained or
    /// reversed. The trusted-checksum pin is intentionally kept so a changed source needs re-trust.
    pub fn unregister(&mut self, id: &ExtensionId) -> Result<UnmountReport, ExtensionError> {
        if !self.manifests.contains_key(id) {
            return Err(ExtensionError::UnknownExtension(id.0.clone()));
        }

        // 1. Stop new calls and signal cancellation to every in-flight call of this extension.
        let trackers: Vec<Arc<InFlightTracker>> = self
            .tools
            .values()
            .filter(|tool| &tool.extension_id == id)
            .map(|tool| Arc::clone(&tool.in_flight))
            .collect();
        let in_flight_before: usize = trackers.iter().map(|tracker| tracker.active()).sum();
        for tracker in &trackers {
            tracker.unmounted.store(true, Ordering::SeqCst);
            tracker.cancelled.store(true, Ordering::SeqCst);
        }

        // 2. Bounded drain window: a cooperative handler observes the signal and returns promptly.
        let deadline = Instant::now() + UNMOUNT_DRAIN_WINDOW;
        while trackers.iter().any(|tracker| tracker.active() > 0) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        let remaining: usize = trackers.iter().map(|tracker| tracker.active()).sum();
        let mut irreversible_side_effects: Vec<String> = trackers
            .iter()
            .flat_map(|tracker| tracker.side_effects_snapshot())
            .collect();
        irreversible_side_effects.sort();
        irreversible_side_effects.dedup();

        // 3. Run teardown callbacks in reverse registration order before removing the surfaces.
        if let Some(callbacks) = self.cleanups.remove(&id.0) {
            for callback in callbacks.into_iter().rev() {
                callback();
            }
        }

        // 4. Remove every contributed surface.
        self.tools.retain(|_, tool| &tool.extension_id != id);
        self.policy_hooks.retain(|(owner, _)| owner != id);
        self.observer_hooks.retain(|(owner, _)| owner != id);
        self.skills
            .retain(|_, skill| skill.extension_id.as_ref() != Some(id));
        self.manifests.remove(id);

        Ok(UnmountReport {
            extension_id: id.clone(),
            cancelled_in_flight: remaining,
            drained_in_flight: in_flight_before.saturating_sub(remaining),
            irreversible_side_effects,
        })
    }
}

// ---------------------------------------------------------------------------------------------
// Dual-class hook engine
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookEvent {
    PreToolUse,
    PostToolUse,
    PreCompletion,
    TurnCompleted,
    RunCompleted,
}

impl HookEvent {
    pub fn as_str(&self) -> &'static str {
        match self {
            HookEvent::PreToolUse => "pre_tool_use",
            HookEvent::PostToolUse => "post_tool_use",
            HookEvent::PreCompletion => "pre_completion",
            HookEvent::TurnCompleted => "turn_completed",
            HookEvent::RunCompleted => "run_completed",
        }
    }

    /// Whether a `PolicyHook` is allowed to gate this event. `PolicyHook` only ever gates
    /// tool use and completion; the remaining events are observation-only.
    pub fn is_policy_gated(&self) -> bool {
        matches!(self, HookEvent::PreToolUse | HookEvent::PreCompletion)
    }
}

/// Everything a hook may inspect. Contains no credentials and no host paths outside the workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookContext {
    pub turn: usize,
    pub tool_name: Option<String>,
    pub call_id: Option<String>,
    pub raw_arguments: Option<String>,
    pub relative_path: Option<String>,
    /// `"success"` or `"error"` for post-tool-use events.
    pub outcome: Option<String>,
    pub workspace_root: PathBuf,
}

impl HookContext {
    pub fn new(turn: usize, workspace_root: impl Into<PathBuf>) -> Self {
        Self {
            turn,
            tool_name: None,
            call_id: None,
            raw_arguments: None,
            relative_path: None,
            outcome: None,
            workspace_root: workspace_root.into(),
        }
    }

    pub fn for_tool(
        turn: usize,
        workspace_root: impl Into<PathBuf>,
        tool_name: &str,
        call_id: &str,
        raw_arguments: &str,
    ) -> Self {
        Self {
            turn,
            tool_name: Some(tool_name.to_string()),
            call_id: Some(call_id.to_string()),
            raw_arguments: Some(raw_arguments.to_string()),
            relative_path: crate::tool::runtime::ToolRuntime::extract_relative_path(raw_arguments),
            outcome: None,
            workspace_root: workspace_root.into(),
        }
    }

    /// Serialize to the JSON document handed to a command hook on stdin.
    pub fn to_json(&self) -> String {
        serde_json::json!({
            "turn": self.turn,
            "tool_name": self.tool_name,
            "call_id": self.call_id,
            "raw_arguments": self.raw_arguments,
            "relative_path": self.relative_path,
            "outcome": self.outcome,
            "workspace_root": self.workspace_root.display().to_string(),
        })
        .to_string()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "decision")]
pub enum PolicyHookDecision {
    Allow,
    Deny { reason: String },
}

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum HookError {
    #[error("hook '{0}' failed: {1}")]
    Failed(String, String),
    #[error("hook '{0}' timed out after {1} ms")]
    Timeout(String, u64),
    #[error("hook '{0}' panicked")]
    Panicked(String),
}

/// Fail-closed hook. It may only ever *restrict* an action: a `Deny`, an error, a panic, a non-zero
/// exit or a timeout all block. It can never turn a kernel `PolicyEngine` denial into an allow,
/// because the kernel policy is evaluated independently and after this hook runs.
pub trait PolicyHook: Send + Sync {
    fn id(&self) -> &str;
    fn events(&self) -> &'static [HookEvent];
    fn evaluate(
        &self,
        event: HookEvent,
        ctx: &HookContext,
    ) -> Result<PolicyHookDecision, HookError>;
}

/// Fail-open hook. Telemetry only: any failure is recorded and the action proceeds.
pub trait ObserverHook: Send + Sync {
    fn id(&self) -> &str;
    fn events(&self) -> &'static [HookEvent];
    fn observe(&self, event: HookEvent, ctx: &HookContext) -> Result<(), HookError>;
}

/// One hook evaluation, logged as `EventPayload::HookEvaluated`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookEvaluation {
    pub hook_id: String,
    /// `"policy"` or `"observer"`.
    pub hook_kind: String,
    pub event: String,
    /// `allow` | `deny` | `ok` | `failed_open` | `timeout` | `panicked`.
    pub outcome: String,
    pub duration_ms: u64,
    pub message: Option<String>,
    /// T09 (KR-18) style: which backend actually ran this hook.
    ///
    /// Hooks launch through `ProcessLaunch` on the host -- the sandbox covers the `AgentBash`
    /// execution class only (see docs/CAPABILITIES.md) -- so this is `"host"`. The field exists so a
    /// receipt can never be read as "this hook was isolated" just because the run had a sandbox
    /// policy configured. Defaults to `"host"` when reading logs written before the field existed.
    #[serde(default = "host_backend")]
    pub execution_backend: String,
}

fn host_backend() -> String {
    "host".to_string()
}

impl HookEvaluation {
    fn new(
        hook_id: &str,
        hook_kind: &str,
        event: HookEvent,
        outcome: &str,
        duration_ms: u64,
        message: Option<String>,
    ) -> Self {
        Self {
            hook_id: hook_id.to_string(),
            hook_kind: hook_kind.to_string(),
            event: event.as_str().to_string(),
            outcome: outcome.to_string(),
            duration_ms,
            message,
            execution_backend: host_backend(),
        }
    }
}

/// Outcome of evaluating every `PolicyHook` registered for an event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyHookOutcome {
    Allow {
        evaluations: Vec<HookEvaluation>,
    },
    Deny {
        reason: String,
        evaluations: Vec<HookEvaluation>,
    },
}

impl PolicyHookOutcome {
    pub fn is_allowed(&self) -> bool {
        matches!(self, PolicyHookOutcome::Allow { .. })
    }

    pub fn evaluations(&self) -> &[HookEvaluation] {
        match self {
            PolicyHookOutcome::Allow { evaluations }
            | PolicyHookOutcome::Deny { evaluations, .. } => evaluations,
        }
    }

    pub fn reason(&self) -> Option<&str> {
        match self {
            PolicyHookOutcome::Deny { reason, .. } => Some(reason.as_str()),
            PolicyHookOutcome::Allow { .. } => None,
        }
    }
}

/// Executes a command-backed hook in an isolated process group with a sanitized environment.
///
/// Exit code `0` means allow/ok; any other exit, a spawn failure or a timeout is a hook failure.
/// Stdout (bounded) becomes the human-readable reason/message.
pub struct CommandHookRunner {
    pub argv: Vec<String>,
    pub timeout: Duration,
}

impl fmt::Debug for CommandHookRunner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CommandHookRunner")
            .field("argv", &self.argv)
            .field("timeout_ms", &self.timeout.as_millis())
            .finish()
    }
}

/// Result of running a command hook.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandHookOutcome {
    pub success: bool,
    pub timed_out: bool,
    pub exit_code: Option<i32>,
    pub stdout: String,
}

impl CommandHookRunner {
    pub fn new(argv: Vec<String>) -> Self {
        Self {
            argv,
            timeout: DEFAULT_HOOK_TIMEOUT,
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Run the hook synchronously with a hard deadline, then sweep the process group / Windows Job Object.
    ///
    /// Synchronous on purpose: it keeps hook semantics identical under the async engine (which
    /// calls this through `spawn_blocking`) and under direct unit tests, with no runtime required.
    pub fn run(
        &self,
        event: HookEvent,
        ctx: &HookContext,
    ) -> Result<CommandHookOutcome, HookError> {
        use std::process::Stdio;

        let hook_name = self
            .argv
            .first()
            .cloned()
            .unwrap_or_else(|| "<empty>".to_string());
        if self.argv.is_empty() {
            return Err(HookError::Failed(
                hook_name,
                "command hook has an empty argv".to_string(),
            ));
        }

        // The shared launch seam owns the credential scrub, the process group / Windows Job Object,
        // and the fail-closed containment attach. `SanitizedWithCustomNoBaseline` keeps the hook's
        // original environment exactly (scrub plus the event variable, no `CI` / `NO_COLOR`), and
        // all three pipes stay piped because `run` writes the context to stdin and drains both
        // output pipes on helper threads.
        let (mut child, proc_group) = crate::process_launch::ProcessLaunch::new(
            self.argv[0].clone(),
            self.argv[1..].to_vec(),
        )
        .cwd(&ctx.workspace_root)
        .stdio(Stdio::piped(), Stdio::piped(), Stdio::piped())
        .env_policy(
            crate::process_launch::EnvironmentPolicy::SanitizedWithCustomNoBaseline(vec![(
                "KROMA_HOOK_EVENT".to_string(),
                event.as_str().to_string(),
            )]),
        )
        .spawn_std()
        .map_err(|e| {
            HookError::Failed(hook_name.clone(), format!("failed to spawn hook: {}", e))
        })?;

        // Hard stop shared by every helper thread (the stdin writer and the stdout/stderr
        // readers). A hook may `setsid()`/double-fork a grandchild that escapes the process-group
        // sweep while still holding one of our pipe ends open, so a helper must stop by this
        // deadline even though the peer never closes its end, or the thread and its pipe fd would
        // leak on every call (F4).
        let pipe_deadline = Instant::now() + self.timeout + HOOK_KILL_GRACE + HOOK_DRAIN_DEADLINE;

        // Feed the JSON context on stdin from a helper thread so a hook that ignores stdin cannot
        // deadlock the parent on a full pipe. The write is bounded by `pipe_deadline` too: a hook
        // (or an escaped grandchild) that never drains stdin would otherwise block `write_all`
        // forever once the payload exceeds the pipe buffer, leaking the thread and its fd.
        if let Some(stdin) = child.stdin.take() {
            let payload = ctx.to_json();
            let write_deadline = pipe_deadline;
            std::thread::Builder::new()
                .name("kroma-hook-stdin".to_string())
                .spawn(move || {
                    write_pipe_bounded(stdin, payload.as_bytes(), write_deadline);
                })
                .expect("failed to spawn the hook stdin writer thread");
        }

        // Start bounded readers *before* waiting on the child: a hook that writes more than an OS
        // pipe buffer holds while the parent is inside `try_wait` would otherwise deadlock.
        let (out_tx, out_rx) = std::sync::mpsc::channel();
        let (err_tx, err_rx) = std::sync::mpsc::channel();
        match child.stdout.take() {
            Some(out) => {
                std::thread::spawn(move || {
                    let buf =
                        read_pipe_bounded(out, (HOOK_OUTPUT_LIMIT_BYTES + 1) as u64, pipe_deadline);
                    let _ = out_tx.send(buf);
                });
            }
            None => {
                let _ = out_tx.send(Vec::new());
            }
        }
        match child.stderr.take() {
            Some(err) => {
                std::thread::spawn(move || {
                    let buf =
                        read_pipe_bounded(err, (HOOK_OUTPUT_LIMIT_BYTES + 1) as u64, pipe_deadline);
                    let _ = err_tx.send(buf);
                });
            }
            None => {
                let _ = err_tx.send(Vec::new());
            }
        }

        let deadline = Instant::now() + self.timeout;
        let mut timed_out = false;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) => {
                    if Instant::now() >= deadline {
                        timed_out = true;
                        break None;
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(e) => {
                    proc_group.sweep_sync(HOOK_KILL_GRACE);
                    return Err(HookError::Failed(
                        hook_name,
                        format!("failed to wait for hook: {}", e),
                    ));
                }
            }
        };

        // Sweep the process group / Job Object on *every* exit path. A background grandchild that inherited
        // stdout/stderr would otherwise keep the pipe FDs open and hang the drain below.
        proc_group.sweep_sync(HOOK_KILL_GRACE);

        if timed_out {
            let _ = child.kill();
            let _ = child.wait();
            return Err(HookError::Timeout(
                hook_name,
                self.timeout.as_millis() as u64,
            ));
        }

        let _ = child.wait();

        // Bounded drain: wait for each reader up to its own hard stop *plus* one drain window, so
        // output the reader has already collected is not discarded. The previous fixed 200 ms window
        // gave up before a reader that is only released at `pipe_deadline` (a hook whose escaped
        // grandchild still holds the pipe) could send. `run` still can never block forever.
        let stdout_bytes = out_rx
            .recv_timeout(
                pipe_deadline.saturating_duration_since(Instant::now()) + HOOK_DRAIN_DEADLINE,
            )
            .unwrap_or_default();
        let stderr_bytes = err_rx
            .recv_timeout(
                pipe_deadline.saturating_duration_since(Instant::now()) + HOOK_DRAIN_DEADLINE,
            )
            .unwrap_or_default();
        let stdout = String::from_utf8_lossy(&stdout_bytes).to_string();
        let stderr = String::from_utf8_lossy(&stderr_bytes).to_string();

        let code = status.and_then(|s| s.code());
        let mut message = stdout.trim().to_string();
        if message.is_empty() {
            message = stderr.trim().to_string();
        }
        let cut = floor_char_boundary(&message, 2048);
        Ok(CommandHookOutcome {
            success: code == Some(0),
            timed_out: false,
            exit_code: code,
            stdout: message[..cut].to_string(),
        })
    }
}

/// Read a hook pipe into memory, capped at `limit` bytes and never blocking past `deadline`.
///
/// A hook can `setsid()`/double-fork a grandchild that escapes the process-group sweep while still
/// holding the pipe write end open, so a blocking `read_to_end` would never see EOF and the reader
/// thread (and the pipe fd) would leak on every call (F4). On Unix the read end is switched to
/// non-blocking and drained with `poll(2)` bounded by `deadline`, so the thread always exits.
#[cfg(unix)]
fn read_pipe_bounded<R>(mut pipe: R, limit: u64, deadline: Instant) -> Vec<u8>
where
    R: std::io::Read + std::os::unix::io::AsRawFd,
{
    let fd = pipe.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags >= 0 {
        unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
    }
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    while (buf.len() as u64) < limit {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let timeout_ms = remaining.as_millis().min(i32::MAX as u128) as i32;
        let rc = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
        if rc < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            break;
        }
        if rc == 0 {
            break; // deadline elapsed with no data
        }
        match pipe.read(&mut chunk) {
            Ok(0) => break, // EOF: every write end closed
            Ok(n) => {
                let want = (limit as usize).saturating_sub(buf.len());
                buf.extend_from_slice(&chunk[..n.min(want)]);
            }
            Err(ref e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    buf
}

/// Non-Unix fallback: the Windows Job Object kills the whole process tree on close, so a blocking
/// read cannot be held open by an escaped grandchild the way `setsid` allows on Unix.
#[cfg(not(unix))]
fn read_pipe_bounded<R>(pipe: R, limit: u64, _deadline: Instant) -> Vec<u8>
where
    R: std::io::Read,
{
    let mut buf = Vec::new();
    let _ = pipe.take(limit).read_to_end(&mut buf);
    buf
}

/// Write a hook's stdin without blocking past `deadline` (Unix).
///
/// A hook that `setsid()`s a grandchild inheriting the read end (or simply never reads) would
/// otherwise block `write_all` forever once the payload exceeds the pipe buffer, leaking the helper
/// thread and its pipe fd on every call (F4). The write end is switched to non-blocking and drained
/// with `poll(2)` bounded by `deadline`; the pipe is dropped (closed) when the deadline elapses or
/// the reader goes away.
#[cfg(unix)]
fn write_pipe_bounded<W>(mut pipe: W, payload: &[u8], deadline: Instant)
where
    W: std::io::Write + std::os::unix::io::AsRawFd,
{
    let fd = pipe.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags >= 0 {
        unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
    }
    let mut written = 0usize;
    while written < payload.len() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLOUT,
            revents: 0,
        };
        let timeout_ms = remaining.as_millis().min(i32::MAX as u128) as i32;
        let rc = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
        if rc < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            break;
        }
        if rc == 0 {
            break; // deadline elapsed with the pipe still full
        }
        match pipe.write(&payload[written..]) {
            Ok(0) => break,
            Ok(n) => written += n,
            Err(ref e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => break, // EPIPE: every read end closed
        }
    }
    let _ = pipe.flush();
}

/// Non-Unix fallback: the Windows Job Object kills the whole process tree on close, so the pipe
/// read end cannot be held open by an escaped grandchild the way `setsid` allows on Unix.
#[cfg(not(unix))]
fn write_pipe_bounded<W>(mut pipe: W, payload: &[u8], _deadline: Instant)
where
    W: std::io::Write,
{
    let _ = pipe.write_all(payload);
    let _ = pipe.flush();
}

/// `PolicyHook` backed by an external command (fail-closed).
pub struct CommandPolicyHook {
    id: String,
    events: &'static [HookEvent],
    runner: CommandHookRunner,
}

impl fmt::Debug for CommandPolicyHook {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CommandPolicyHook")
            .field("id", &self.id)
            .field("runner", &self.runner)
            .finish()
    }
}

impl CommandPolicyHook {
    pub fn new(id: impl Into<String>, argv: Vec<String>) -> Self {
        Self {
            id: id.into(),
            events: &[HookEvent::PreToolUse],
            runner: CommandHookRunner::new(argv),
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.runner.timeout = timeout;
        self
    }

    pub fn argv(&self) -> &[String] {
        &self.runner.argv
    }
}

impl PolicyHook for CommandPolicyHook {
    fn id(&self) -> &str {
        &self.id
    }

    fn events(&self) -> &'static [HookEvent] {
        self.events
    }

    fn evaluate(
        &self,
        event: HookEvent,
        ctx: &HookContext,
    ) -> Result<PolicyHookDecision, HookError> {
        let outcome = self.runner.run(event, ctx)?;
        if outcome.success {
            Ok(PolicyHookDecision::Allow)
        } else {
            let reason = if outcome.stdout.is_empty() {
                format!("command policy hook exited with {:?}", outcome.exit_code)
            } else {
                outcome.stdout
            };
            Ok(PolicyHookDecision::Deny { reason })
        }
    }
}

/// `ObserverHook` backed by an external command (fail-open).
pub struct CommandObserverHook {
    id: String,
    events: &'static [HookEvent],
    runner: CommandHookRunner,
}

impl fmt::Debug for CommandObserverHook {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CommandObserverHook")
            .field("id", &self.id)
            .field("runner", &self.runner)
            .finish()
    }
}

impl CommandObserverHook {
    pub fn new(id: impl Into<String>, argv: Vec<String>) -> Self {
        Self {
            id: id.into(),
            events: &[HookEvent::PostToolUse],
            runner: CommandHookRunner::new(argv),
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.runner.timeout = timeout;
        self
    }

    pub fn argv(&self) -> &[String] {
        &self.runner.argv
    }
}

impl ObserverHook for CommandObserverHook {
    fn id(&self) -> &str {
        &self.id
    }

    fn events(&self) -> &'static [HookEvent] {
        self.events
    }

    fn observe(&self, event: HookEvent, ctx: &HookContext) -> Result<(), HookError> {
        let outcome = self.runner.run(event, ctx)?;
        if outcome.success {
            Ok(())
        } else {
            Err(HookError::Failed(
                self.id.clone(),
                format!(
                    "command observer hook exited with {:?}: {}",
                    outcome.exit_code, outcome.stdout
                ),
            ))
        }
    }
}

/// Dual-class hook engine.
///
/// Policy evaluation is fail-closed and observer evaluation is fail-open; both are enforced here
/// rather than in individual hooks so no hook implementation can weaken the contract.
pub struct HookEngine {
    policy_hooks: Vec<Arc<dyn PolicyHook>>,
    observer_hooks: Vec<Arc<dyn ObserverHook>>,
    timeout: Duration,
}

impl fmt::Debug for HookEngine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HookEngine")
            .field(
                "policy_hooks",
                &self.policy_hooks.iter().map(|h| h.id()).collect::<Vec<_>>(),
            )
            .field(
                "observer_hooks",
                &self
                    .observer_hooks
                    .iter()
                    .map(|h| h.id())
                    .collect::<Vec<_>>(),
            )
            .field("timeout_ms", &self.timeout.as_millis())
            .finish()
    }
}

impl Default for HookEngine {
    fn default() -> Self {
        Self::new(Vec::new(), Vec::new())
    }
}

impl HookEngine {
    pub fn new(
        policy_hooks: Vec<Arc<dyn PolicyHook>>,
        observer_hooks: Vec<Arc<dyn ObserverHook>>,
    ) -> Self {
        Self {
            policy_hooks,
            observer_hooks,
            timeout: DEFAULT_HOOK_TIMEOUT,
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn is_empty(&self) -> bool {
        self.policy_hooks.is_empty() && self.observer_hooks.is_empty()
    }

    pub fn policy_hook_ids(&self) -> Vec<String> {
        self.policy_hooks
            .iter()
            .map(|h| h.id().to_string())
            .collect()
    }

    pub fn observer_hook_ids(&self) -> Vec<String> {
        self.observer_hooks
            .iter()
            .map(|h| h.id().to_string())
            .collect()
    }

    /// Evaluate every `PolicyHook` registered for `event`. Any denial, error, panic or timeout
    /// denies the action; a hook that returns `Allow` can never cancel another hook's `Deny`.
    pub async fn evaluate_policy(&self, event: HookEvent, ctx: &HookContext) -> PolicyHookOutcome {
        let mut evaluations = Vec::new();
        let mut denial: Option<String> = None;

        for hook in &self.policy_hooks {
            if !hook.events().contains(&event) {
                continue;
            }
            let hook_id = hook.id().to_string();
            let started = Instant::now();
            let result = self.call_sync(event, ctx, hook).await;
            let duration_ms = started.elapsed().as_millis() as u64;
            match result {
                Ok(PolicyHookDecision::Allow) => {
                    evaluations.push(HookEvaluation::new(
                        &hook_id,
                        "policy",
                        event,
                        "allow",
                        duration_ms,
                        None,
                    ));
                }
                Ok(PolicyHookDecision::Deny { reason }) => {
                    if denial.is_none() {
                        denial = Some(reason.clone());
                    }
                    evaluations.push(HookEvaluation::new(
                        &hook_id,
                        "policy",
                        event,
                        "deny",
                        duration_ms,
                        Some(reason),
                    ));
                }
                Err(HookError::Timeout(id, ms)) => {
                    let reason = format!("policy hook '{}' timed out after {} ms", id, ms);
                    if denial.is_none() {
                        denial = Some(reason.clone());
                    }
                    evaluations.push(HookEvaluation::new(
                        &hook_id,
                        "policy",
                        event,
                        "timeout",
                        duration_ms,
                        Some(reason),
                    ));
                }
                Err(HookError::Panicked(id)) => {
                    let reason = format!("policy hook '{}' panicked", id);
                    if denial.is_none() {
                        denial = Some(reason.clone());
                    }
                    evaluations.push(HookEvaluation::new(
                        &hook_id,
                        "policy",
                        event,
                        "panicked",
                        duration_ms,
                        Some(reason),
                    ));
                }
                Err(HookError::Failed(id, msg)) => {
                    let reason = format!("policy hook '{}' failed: {}", id, msg);
                    if denial.is_none() {
                        denial = Some(reason.clone());
                    }
                    evaluations.push(HookEvaluation::new(
                        &hook_id,
                        "policy",
                        event,
                        "deny",
                        duration_ms,
                        Some(reason),
                    ));
                }
            }
        }

        match denial {
            Some(reason) => PolicyHookOutcome::Deny {
                reason,
                evaluations,
            },
            None => PolicyHookOutcome::Allow { evaluations },
        }
    }

    /// Evaluate every `ObserverHook` registered for `event`. Failures are recorded as
    /// `failed_open` and never propagate.
    pub async fn run_observers(&self, event: HookEvent, ctx: &HookContext) -> Vec<HookEvaluation> {
        let mut evaluations = Vec::new();
        for hook in &self.observer_hooks {
            if !hook.events().contains(&event) {
                continue;
            }
            let hook_id = hook.id().to_string();
            let started = Instant::now();
            let result = self.call_sync_observer(event, ctx, hook).await;
            let duration_ms = started.elapsed().as_millis() as u64;
            match result {
                Ok(()) => evaluations.push(HookEvaluation::new(
                    &hook_id,
                    "observer",
                    event,
                    "ok",
                    duration_ms,
                    None,
                )),
                Err(HookError::Timeout(id, ms)) => evaluations.push(HookEvaluation::new(
                    &hook_id,
                    "observer",
                    event,
                    "failed_open",
                    duration_ms,
                    Some(format!("observer hook '{}' timed out after {} ms", id, ms)),
                )),
                Err(HookError::Panicked(id)) => evaluations.push(HookEvaluation::new(
                    &hook_id,
                    "observer",
                    event,
                    "failed_open",
                    duration_ms,
                    Some(format!("observer hook '{}' panicked", id)),
                )),
                Err(HookError::Failed(id, msg)) => evaluations.push(HookEvaluation::new(
                    &hook_id,
                    "observer",
                    event,
                    "failed_open",
                    duration_ms,
                    Some(format!("observer hook '{}' failed: {}", id, msg)),
                )),
            }
        }
        evaluations
    }

    /// Run a synchronous hook off the async runtime thread, bounded by the engine timeout.
    ///
    /// A timeout is fail-closed for policy hooks: the blocking thread is abandoned (hooks are
    /// required to be cheap; command hooks enforce their own inner deadline and kill their own
    /// process group) and the caller sees a denial rather than an unbounded wait.
    async fn call_sync(
        &self,
        event: HookEvent,
        ctx: &HookContext,
        hook: &Arc<dyn PolicyHook>,
    ) -> Result<PolicyHookDecision, HookError> {
        let hook = Arc::clone(hook);
        let ctx = ctx.clone();
        let id = hook.id().to_string();
        let timeout = self.timeout;
        let join = tokio::task::spawn_blocking(move || {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| hook.evaluate(event, &ctx)))
        });
        match tokio::time::timeout(timeout, join).await {
            Err(_) => Err(HookError::Timeout(id, timeout.as_millis() as u64)),
            Ok(Err(_join_err)) => Err(HookError::Panicked(id)),
            Ok(Ok(Err(_panic))) => Err(HookError::Panicked(id)),
            Ok(Ok(Ok(result))) => result,
        }
    }

    async fn call_sync_observer(
        &self,
        event: HookEvent,
        ctx: &HookContext,
        hook: &Arc<dyn ObserverHook>,
    ) -> Result<(), HookError> {
        let hook = Arc::clone(hook);
        let ctx = ctx.clone();
        let id = hook.id().to_string();
        let timeout = self.timeout;
        let join = tokio::task::spawn_blocking(move || {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| hook.observe(event, &ctx)))
        });
        match tokio::time::timeout(timeout, join).await {
            Err(_) => Err(HookError::Timeout(id, timeout.as_millis() as u64)),
            Ok(Err(_join_err)) => Err(HookError::Panicked(id)),
            Ok(Ok(Err(_panic))) => Err(HookError::Panicked(id)),
            Ok(Ok(Ok(result))) => result,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Subtree JIT context
// ---------------------------------------------------------------------------------------------

/// One ancestor-directory guidance document that was injected into a tool result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubtreeContextLoad {
    pub directory: String,
    pub file_name: String,
    pub bytes: usize,
    pub rendered: String,
}

/// Tracks which ancestor directories have already contributed JIT guidance on the active session
/// branch, so a directory is loaded at most once and the total budget cannot be exceeded.
#[derive(Debug, Clone)]
pub struct SubtreeContextTracker {
    enabled: bool,
    loaded_dirs: BTreeSet<String>,
    total_bytes: usize,
    total_budget: usize,
}

impl Default for SubtreeContextTracker {
    fn default() -> Self {
        Self::new(true)
    }
}

impl SubtreeContextTracker {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            loaded_dirs: BTreeSet::new(),
            total_bytes: 0,
            total_budget: SUBTREE_TOTAL_MAX_BYTES,
        }
    }

    pub fn disabled() -> Self {
        Self::new(false)
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub fn loaded_dirs(&self) -> Vec<String> {
        self.loaded_dirs.iter().cloned().collect()
    }

    pub fn total_bytes(&self) -> usize {
        self.total_bytes
    }

    /// Restore the already-loaded directory set recorded in a session checkpoint so a `--resume`
    /// does not re-inject the same guidance into a fresh turn.
    pub fn restore_loaded_dirs<I, S>(&mut self, dirs: I)
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        for dir in dirs {
            self.loaded_dirs.insert(dir.into());
        }
    }

    /// Drop all loaded-directory state. Called after a `--fork` rewind: the rewound branch may
    /// visit a subtree the abandoned branch had already loaded, and that branch's guidance is no
    /// longer in context, so it must be reloadable.
    pub fn reset_for_fork(&mut self) {
        self.loaded_dirs.clear();
        self.total_bytes = 0;
    }

    /// Ancestor directories of a workspace-relative path, shallowest first, excluding the workspace
    /// root (which is already described in the initial static prefix).
    ///
    /// `a/b/c.rs` -> `["a", "a/b"]`; `c.rs` -> `[]`.
    pub fn ancestor_dirs(rel_path: &str) -> Vec<String> {
        let normalized = match WorkspaceCap::validate_relative_components(rel_path) {
            Ok(p) => p,
            Err(_) => return Vec::new(),
        };
        let mut dirs = Vec::new();
        let mut current = PathBuf::new();
        let comps: Vec<_> = normalized.components().collect();
        // The last component is the file (or the directory itself when the tool targeted a dir).
        if comps.is_empty() {
            return Vec::new();
        }
        for comp in &comps[..comps.len() - 1] {
            current.push(comp.as_os_str());
            let text = current.to_string_lossy().replace('\\', "/");
            if !text.is_empty() && text != "." {
                dirs.push(text);
            }
        }
        dirs
    }

    /// Load not-yet-seen ancestor `AGENTS.md`/`CLAUDE.md` documents for `rel_path`.
    ///
    /// Reads go through `WorkspaceCap::open_existing_no_symlink`, so symlinked or sensitive
    /// documents are refused atomically. A directory that exists but has no guidance file yet is
    /// remembered so the per-tool-call stat cost stays bounded; a directory that does not exist yet
    /// is *not* remembered, because a later `write` may create its `AGENTS.md`.
    pub fn load_for_path(
        &mut self,
        workspace: &WorkspaceCap,
        rel_path: &str,
    ) -> Vec<SubtreeContextLoad> {
        if !self.enabled {
            return Vec::new();
        }
        let mut loads = Vec::new();
        for dir in Self::candidate_dirs(workspace, rel_path) {
            if self.loaded_dirs.contains(&dir) {
                continue;
            }
            let Some((file_name, text)) = read_subtree_doc(workspace, &dir) else {
                if workspace.read_dir_no_symlink(&dir).is_ok() {
                    self.loaded_dirs.insert(dir);
                }
                continue;
            };
            self.loaded_dirs.insert(dir.clone());
            let remaining = self.total_budget.saturating_sub(self.total_bytes);
            if remaining == 0 {
                break;
            }
            let attrs = format!(
                " directory=\"{}\" file=\"{}\"",
                escape_fence_attribute(&dir),
                escape_fence_attribute(file_name)
            );
            // Size the body cap so the *whole* rendered fence fits the remaining cumulative budget,
            // not just the body: otherwise the fixed fence wrapper could push the total over it.
            let body_cap = remaining
                .saturating_sub(crate::prompt::fence_overhead_bytes(
                    "untrusted_subtree_context",
                    &attrs,
                ))
                .min(SUBTREE_DOC_MAX_BYTES);
            if body_cap == 0 {
                break;
            }
            let rendered = fence_untrusted("untrusted_subtree_context", &attrs, &text, body_cap);
            self.total_bytes += rendered.len();
            loads.push(SubtreeContextLoad {
                directory: dir,
                file_name: file_name.to_string(),
                bytes: rendered.len(),
                rendered,
            });
        }
        loads
    }

    /// Ancestor directories of `rel_path`, shallowest first, plus the target itself when it is an
    /// existing directory (so `ls a/b` also loads `a/b/AGENTS.md`, not just `a/AGENTS.md`).
    fn candidate_dirs(workspace: &WorkspaceCap, rel_path: &str) -> Vec<String> {
        let mut dirs = Self::ancestor_dirs(rel_path);
        if let Ok(normalized) = WorkspaceCap::validate_relative_components(rel_path) {
            let text = normalized.to_string_lossy().replace('\\', "/");
            if !text.is_empty()
                && text != "."
                && !dirs.contains(&text)
                && workspace.read_dir_no_symlink(&text).is_ok()
            {
                dirs.push(text);
            }
        }
        dirs
    }
}

fn read_subtree_doc(workspace: &WorkspaceCap, dir: &str) -> Option<(&'static str, String)> {
    for name in ["AGENTS.md", "CLAUDE.md"] {
        let rel = format!("{}/{}", dir, name);
        let Ok((_, file)) = workspace.open_existing_no_symlink(&rel) else {
            continue;
        };
        let mut bytes = Vec::new();
        if file
            .take((SUBTREE_DOC_MAX_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .is_err()
        {
            continue;
        }
        if bytes.len() > SUBTREE_DOC_MAX_BYTES {
            bytes.truncate(SUBTREE_DOC_MAX_BYTES);
            while !bytes.is_empty() && std::str::from_utf8(&bytes).is_err() {
                bytes.pop();
            }
        }
        let text = String::from_utf8_lossy(&bytes).trim().to_string();
        if text.is_empty() {
            continue;
        }
        return Some((name, text));
    }
    None
}

// ---------------------------------------------------------------------------------------------
// Skills
// ---------------------------------------------------------------------------------------------

/// A discovered on-demand skill definition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillEntry {
    /// Skill name: the YAML `name:` field, or the containing directory name.
    pub name: String,
    pub description: String,
    /// Raw markdown body (bounded by [`SKILL_FILE_MAX_BYTES`]); sanitized at render time.
    pub body: String,
    /// Workspace-relative path of the definition file.
    pub path: String,
    /// Owning extension, when the skill was contributed by a mounted extension.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extension_id: Option<ExtensionId>,
    /// Capability gap #6 (`v1.5.2`): workspace-relative path globs this skill applies to. Empty
    /// means "always offered". A `paths` restricted skill is only listed and activatable for a
    /// matching file, so a Rust-specific playbook does not pollute a Python task's catalog.
    #[serde(default)]
    pub paths: Vec<String>,
}

impl SkillEntry {
    /// Wrap the body in an escaped, bounded `<untrusted_skill_context>` fence.
    pub fn render(&self) -> String {
        let attrs = format!(" name=\"{}\"", escape_fence_attribute(&self.name));
        fence_untrusted(
            "untrusted_skill_context",
            &attrs,
            &self.body,
            SKILL_BODY_MAX_BYTES,
        )
    }

    /// Capability gap #6: `true` when this skill applies to `rel_path` (an unrestricted skill
    /// always applies).
    pub fn applies_to(&self, rel_path: &str) -> bool {
        if self.paths.is_empty() {
            return true;
        }
        let path = normalize_skill_path(rel_path);
        self.paths
            .iter()
            .any(|pattern| skill_path_matches(pattern, &path))
    }

    /// A compact `[paths: a, b]` annotation for the listing, or `None` when unrestricted.
    pub fn paths_annotation(&self) -> Option<String> {
        if self.paths.is_empty() {
            None
        } else {
            Some(format!("[paths: {}]", self.paths.join(", ")))
        }
    }
}

/// Normalize a workspace-relative path for skill `paths` matching.
fn normalize_skill_path(path: &str) -> String {
    path.trim()
        .replace('\\', "/")
        .trim_start_matches("./")
        .trim_matches('/')
        .to_string()
}

/// Match a skill `paths` entry against a normalized relative path.
///
/// A plain entry matches the exact path, a directory prefix (`src/`), or any descendant; an entry
/// containing `*` is treated as a glob where `**` crosses directory separators.
fn skill_path_matches(pattern: &str, path: &str) -> bool {
    let pattern = normalize_skill_path(pattern);
    if pattern.is_empty() {
        return true;
    }
    if !pattern.contains('*') {
        return path == pattern
            || path.starts_with(&format!("{}/", pattern))
            || pattern.starts_with(&format!("{}/", path));
    }
    let mut regex = String::from("^");
    let mut chars = pattern.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '*' => {
                if chars.peek() == Some(&'*') {
                    chars.next();
                    // A `**/` segment may match zero directories (`src/**/*.rs` matches
                    // `src/lib.rs`), so consume the following slash into an optional group.
                    if chars.peek() == Some(&'/') {
                        chars.next();
                        regex.push_str("(?:.*/)?");
                    } else {
                        regex.push_str(".*");
                    }
                } else {
                    regex.push_str("[^/]*");
                }
            }
            '.' | '+' | '(' | ')' | '|' | '^' | '$' | '[' | ']' | '{' | '}' | '?' | '\\' => {
                regex.push('\\');
                regex.push(c);
            }
            other => regex.push(other),
        }
    }
    regex.push('$');
    regex::Regex::new(&regex)
        .map(|re| re.is_match(path))
        .unwrap_or(false)
}

/// Discover skills under every directory in [`SKILL_DIRS`] inside the workspace.
///
/// Symlinks and sensitive paths are refused by `WorkspaceCap`; a malformed skill is skipped rather
/// than aborting discovery, and the result is sorted by skill name for deterministic schemas. A
/// name collision is resolved by scan order (`.agents/skills` before `.claude/skills`).
pub fn discover_skills(workspace: &WorkspaceCap) -> Vec<SkillEntry> {
    let mut by_name: BTreeMap<String, SkillEntry> = BTreeMap::new();
    for dir in SKILL_DIRS {
        for skill in discover_skills_in(workspace, dir) {
            by_name.entry(skill.name.clone()).or_insert(skill);
        }
    }
    by_name.into_values().collect()
}

fn discover_skills_in(workspace: &WorkspaceCap, skill_dir: &str) -> Vec<SkillEntry> {
    let Ok((_, entries)) = workspace.read_dir_no_symlink(skill_dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .into_iter()
        .filter(|entry| entry.is_dir && !entry.is_symlink)
        .map(|entry| entry.name)
        .collect();
    names.sort();

    let mut skills = Vec::new();
    for dir_name in names {
        let rel = format!("{}/{}/SKILL.md", skill_dir, dir_name);
        let Ok((_, file)) = workspace.open_existing_no_symlink(&rel) else {
            continue;
        };
        let mut bytes = Vec::new();
        if file
            .take((SKILL_FILE_MAX_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .is_err()
        {
            continue;
        }
        if bytes.len() > SKILL_FILE_MAX_BYTES {
            bytes.truncate(SKILL_FILE_MAX_BYTES);
            while !bytes.is_empty() && std::str::from_utf8(&bytes).is_err() {
                bytes.pop();
            }
        }
        let raw = String::from_utf8_lossy(&bytes).to_string();
        skills.push(parse_skill(&dir_name, &rel, &raw));
    }
    skills.sort_by(|a, b| a.name.cmp(&b.name));
    skills
}

/// Parse the `paths:` frontmatter value, accepting either an inline list (`paths: [a, b]` /
/// `paths: a, b`) or a YAML block sequence of `- item` lines.
fn parse_frontmatter_paths(frontmatter: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut in_block = false;
    for line in frontmatter.lines() {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("paths:") {
            in_block = true;
            out.extend(split_inline_list(rest));
            continue;
        }
        if in_block {
            if let Some(item) = trimmed.strip_prefix("- ") {
                let item = item.trim().trim_matches('"').trim_matches('\'');
                if !item.is_empty() {
                    out.push(item.to_string());
                }
                continue;
            }
            if !trimmed.is_empty() {
                in_block = false;
            }
        }
    }
    out.into_iter()
        .map(|p| {
            p.trim()
                .replace('\\', "/")
                .trim_start_matches("./")
                .trim_matches('/')
                .to_string()
        })
        .filter(|p| !p.is_empty() && !p.split('/').any(|c| c == ".."))
        .take(MAX_SKILL_PATHS)
        .collect()
}

fn split_inline_list(value: &str) -> Vec<String> {
    let value = value.trim();
    let value = value
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(value);
    value
        .split(',')
        .map(|s| s.trim().trim_matches('"').trim_matches('\'').to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Parse a `SKILL.md`: optional YAML frontmatter (`name`, `description`, `paths`), otherwise the
/// first markdown heading and first paragraph are used.
fn parse_skill(dir_name: &str, path: &str, raw: &str) -> SkillEntry {
    let mut name = dir_name.to_string();
    let mut description = String::new();
    let mut body = raw.to_string();
    let mut paths: Vec<String> = Vec::new();

    if let Some(rest) = raw
        .strip_prefix("---\n")
        .or_else(|| raw.strip_prefix("---\r\n"))
    {
        if let Some(end) = rest.find("\n---") {
            let frontmatter = &rest[..end];
            body = rest[end + 4..].trim_start_matches(['\r', '\n']).to_string();
            paths = parse_frontmatter_paths(frontmatter);
            for line in frontmatter.lines() {
                let Some((key, value)) = line.split_once(':') else {
                    continue;
                };
                let value = value
                    .trim()
                    .trim_matches('"')
                    .trim_matches('\'')
                    .to_string();
                match key.trim() {
                    // A hostile frontmatter `name:` cannot inject a fence attribute: anything
                    // outside the conservative charset falls back to the directory name.
                    "name" if !value.is_empty() && is_valid_skill_name(&value) => name = value,
                    "description" if !value.is_empty() => description = value,
                    _ => {}
                }
            }
        }
    }

    if description.is_empty() {
        for line in body.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed == "---" {
                continue;
            }
            description = trimmed.trim_start_matches('#').trim().to_string();
            if !description.is_empty() {
                break;
            }
        }
    }

    SkillEntry {
        name,
        description,
        body,
        path: path.to_string(),
        extension_id: None,
        paths,
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ActivateSkillArgs {
    name: Option<String>,
    action: Option<String>,
    /// Capability gap #6 (`v1.5.2`): the file the model is currently working on. When supplied, a
    /// `paths`-restricted skill is only listed/activatable if it applies to this path.
    path: Option<String>,
}

/// Execute the built-in read-only `activate_skill` kernel tool.
///
/// `{ "action": "list" }` (or no argument) lists the discovered skills; `{ "name": "..." }`
/// returns the escaped, bounded skill body. An optional `path` scopes `paths`-restricted skills to
/// the file under work.
pub fn execute_activate_skill(catalog: &[SkillEntry], raw_args: &str) -> ToolExecutionOutput {
    let args: ActivateSkillArgs = match serde_json::from_str(raw_args) {
        Ok(a) => a,
        Err(e) => {
            return ToolExecutionOutput::error(
                ErrorCode::InvalidArguments,
                format!("invalid JSON arguments for `activate_skill`: {}", e),
                ToolResultMeta::default(),
            );
        }
    };

    let focus = args
        .path
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty());

    let listing = |catalog: &[SkillEntry]| -> String {
        let visible: Vec<&SkillEntry> = catalog
            .iter()
            .filter(|skill| focus.map(|p| skill.applies_to(p)).unwrap_or(true))
            .collect();
        if visible.is_empty() {
            match focus {
                Some(path) => format!(
                    "No skills apply to '{}'. Add a definition at {}/<name>/SKILL.md.",
                    escape_fence_attribute(path),
                    SKILL_DIR
                ),
                None => format!(
                    "No skills found. Add a definition at {}/<name>/SKILL.md (or the compatible {}/).",
                    SKILL_DIR, CLAUDE_SKILL_DIR
                ),
            }
        } else {
            let mut out = format!("Available skills ({}):\n", visible.len());
            for skill in visible {
                let paths = skill
                    .paths_annotation()
                    .map(|a| format!("  {}", escape_fence_attribute(&a)))
                    .unwrap_or_default();
                out.push_str(&format!(
                    "  {}  —  {}{}\n",
                    escape_fence_attribute(&skill.name),
                    escape_fence_attribute(&skill.description),
                    paths
                ));
            }
            out
        }
    };

    let wants_list = args
        .action
        .as_deref()
        .map(|a| a.eq_ignore_ascii_case("list"))
        .unwrap_or(false);
    let name = args.name.filter(|n| !n.trim().is_empty());

    if wants_list || name.is_none() {
        return ToolExecutionOutput::success(listing(catalog), ToolResultMeta::default());
    }
    let name = name.unwrap();
    match catalog.iter().find(|skill| skill.name == name) {
        Some(skill) => {
            if let Some(path) = focus {
                if !skill.applies_to(path) {
                    return ToolExecutionOutput::error(
                        ErrorCode::InvalidArguments,
                        format!(
                            "skill '{}' is restricted to paths [{}] and does not apply to '{}'",
                            escape_fence_attribute(&skill.name),
                            escape_fence_attribute(&skill.paths.join(", ")),
                            escape_fence_attribute(path),
                        ),
                        ToolResultMeta::default(),
                    );
                }
            }
            // The header sits outside the fence, so it uses the same attribute escaping as the
            // fence itself: a hostile `SKILL.md` cannot inject prompt text or fake fence tags.
            let header = format!(
                "[Activated skill: {} — {}]\n{}\n",
                escape_fence_attribute(&skill.name),
                escape_fence_attribute(&skill.description),
                skill.render()
            );
            ToolExecutionOutput::success(header, ToolResultMeta::default())
        }
        None => ToolExecutionOutput::error(
            ErrorCode::InvalidArguments,
            format!(
                "unknown skill '{}'\n{}",
                escape_fence_attribute(&name),
                listing(catalog)
            ),
            ToolResultMeta::default(),
        ),
    }
}

/// Built-in read-only kernel tool schema for on-demand skill activation.
pub fn activate_skill_registration() -> RegisteredTool {
    RegisteredTool {
        definition: ToolDefinition {
            name: "activate_skill".to_string(),
            description: "List the on-demand skills discovered under .agents/skills (and the compatible .claude/skills), or activate one by name so its instructions are loaded for this turn. Pass `path` to scope skills that declare `paths:`.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "Skill directory name to activate (as reported by {\"action\":\"list\"})."
                    },
                    "action": {
                        "type": "string",
                        "description": "Set to \"list\" to list the discovered skills without activating one."
                    },
                    "path": {
                        "type": "string",
                        "description": "Workspace-relative file you are working on; only skills whose `paths:` match it are listed/activatable."
                    }
                },
                "additionalProperties": false
            }),
        },
        metadata: ToolMetadata {
            effect_class: EffectClass::ReadOnly,
            default_timeout_sec: 30,
            inline_output_cap_bytes: SKILL_BODY_MAX_BYTES,
        },
    }
}

/// Hard cap on the number of tool schemas one `discover_tools` call may return.
pub const MAX_DISCOVERED_TOOLS: usize = 24;
/// Hard cap on the rendered schema payload so a discovery call cannot flood the context.
pub const DISCOVER_TOOLS_MAX_BYTES: usize = 12 * 1024;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DiscoverToolsArgs {
    query: Option<String>,
    #[serde(default)]
    include_schema: bool,
    #[serde(default)]
    activate: Vec<String>,
    #[serde(default)]
    deactivate: Vec<String>,
}

/// The activation half of a `discover_tools` call: the tool names to add to and remove from the
/// request schema set.
///
/// The dispatcher reads these through this function so that the argument set the schema declares,
/// the argument set this parser accepts and the names the runtime activates are one contract. A
/// malformed call is an error rather than an empty activation list, and the caller must not activate
/// anything when it is returned.
pub fn discover_activation_args(
    raw_args: &str,
) -> Result<(Vec<String>, Vec<String>), serde_json::Error> {
    let args: DiscoverToolsArgs = serde_json::from_str(raw_args)?;
    Ok((args.activate, args.deactivate))
}

/// Execute the built-in read-only `discover_tools` kernel tool.
///
/// Capability gap #6 (`v1.5.2`): tool schemas can be fetched on demand instead of being pinned in
/// every request, while the caller still honours the active policy — `catalog` must already be the
/// policy-enabled set, so discovery can never widen what the model may call.
pub fn execute_discover_tools(catalog: &[RegisteredTool], raw_args: &str) -> ToolExecutionOutput {
    let args: DiscoverToolsArgs = match serde_json::from_str(raw_args) {
        Ok(a) => a,
        Err(e) => {
            return ToolExecutionOutput::error(
                ErrorCode::InvalidArguments,
                format!("invalid JSON arguments for `discover_tools`: {}", e),
                ToolResultMeta::default(),
            );
        }
    };
    let query = args
        .query
        .as_deref()
        .map(|q| q.trim().to_ascii_lowercase())
        .filter(|q| !q.is_empty());

    let mut matched: Vec<&RegisteredTool> = catalog
        .iter()
        .filter(|tool| match &query {
            Some(q) => {
                tool.definition.name.to_ascii_lowercase().contains(q)
                    || tool.definition.description.to_ascii_lowercase().contains(q)
            }
            None => true,
        })
        .collect();
    matched.sort_by(|a, b| a.definition.name.cmp(&b.definition.name));
    let total = matched.len();
    matched.truncate(MAX_DISCOVERED_TOOLS);

    let mut body = if total == 0 {
        "No matching tools.".to_string()
    } else {
        format!("Discovered tools ({} shown of {}):\n", matched.len(), total)
    };
    for tool in &matched {
        body.push_str(&format!(
            "- {}: {}\n",
            escape_fence_attribute(&tool.definition.name),
            escape_fence_attribute(&tool.definition.description),
        ));
        if args.include_schema {
            let schema = serde_json::to_string(&tool.definition.parameters)
                .unwrap_or_else(|_| "{}".to_string());
            body.push_str(&format!("  schema: {}\n", schema));
        }
    }
    ToolExecutionOutput::success(
        crate::prompt::fence_untrusted("tool_catalog", "", &body, DISCOVER_TOOLS_MAX_BYTES),
        ToolResultMeta::default(),
    )
}

/// Built-in read-only kernel tool schema for on-demand tool discovery.
pub fn discover_tools_registration() -> RegisteredTool {
    RegisteredTool {
        definition: ToolDefinition {
            name: "discover_tools".to_string(),
            description: "List the tools the active policy currently permits, optionally with their JSON schemas, so a schema need not be pinned in every request. Optional `query` filters by name/description substring. `activate`/`deactivate` change which schemas the following requests declare.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Case-insensitive substring matched against tool name and description."
                    },
                    "include_schema": {
                        "type": "boolean",
                        "description": "When true, include each tool's JSON parameter schema."
                    },
                    "activate": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "Tool names to add to the schemas declared by the following requests. Each name must be permitted by the active policy; a refusal is reported, never silently skipped."
                    },
                    "deactivate": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "Tool names to remove from the schemas declared by the following requests."
                    }
                },
                "additionalProperties": false
            }),
        },
        metadata: ToolMetadata {
            effect_class: EffectClass::ReadOnly,
            default_timeout_sec: 30,
            inline_output_cap_bytes: DISCOVER_TOOLS_MAX_BYTES,
        },
    }
}

/// Workspace-relative directories that trigger JIT context loading for a tool call.
pub fn is_subtree_context_tool(tool_name: &str) -> bool {
    matches!(
        tool_name,
        "read" | "write" | "edit" | "grep" | "find" | "ls"
    )
}

/// Error code used when a path is not valid for subtree traversal.
pub fn subtree_path_error(err: &WorkspaceError) -> ErrorCode {
    err.error_code()
}

#[cfg(all(test, unix))]
mod tests {
    use super::{read_pipe_bounded, write_pipe_bounded};
    use std::io::{Read, Write};
    use std::time::{Duration, Instant};

    /// F4: a hook can `setsid()` a grandchild that inherits the pipe and never closes it. The
    /// reader must therefore return at its deadline instead of blocking (and leaking the thread and
    /// its fd) forever.
    #[test]
    fn read_pipe_bounded_returns_by_the_deadline_without_eof() {
        let (reader, mut writer) = std::os::unix::net::UnixStream::pair().unwrap();
        writer.write_all(b"hello").unwrap();
        let deadline = Instant::now() + Duration::from_millis(50);
        let start = Instant::now();
        let buf = read_pipe_bounded(reader, 4096, deadline);
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "the reader must stop at its deadline, not block on a held-open pipe"
        );
        assert_eq!(buf, b"hello");
        // Keep the writer alive until after the read so the pipe never reports EOF.
        drop(writer);
    }

    /// F4: the stdin writer is the mirror image. A hook (or an escaped grandchild) that never
    /// drains stdin would otherwise block `write_all` forever once the payload exceeds the pipe
    /// buffer, leaking the writer thread and its fd on every call.
    #[test]
    fn write_pipe_bounded_returns_by_the_deadline_when_the_reader_stalls() {
        let (mut reader, writer) = std::os::unix::net::UnixStream::pair().unwrap();
        let deadline = Instant::now() + Duration::from_millis(50);
        let start = Instant::now();
        // 4 MiB is far larger than any socket buffer, so a blocking `write_all` would never return.
        let payload = vec![b'x'; 4 * 1024 * 1024];
        write_pipe_bounded(writer, &payload, deadline);
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "the writer must stop at its deadline, not block on a full pipe"
        );
        // The reader was never drained, so the write could only have been partial.
        let mut probe = [0u8; 4096];
        assert!(reader.read(&mut probe).unwrap() > 0);
    }
}
