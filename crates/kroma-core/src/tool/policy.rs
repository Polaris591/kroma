use crate::revision::RevisionId;
use crate::types::{EffectClass, RunId, ToolCallId};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// B11a: how long a category approval may be reused before it must be re-asked. Clamped by the
/// run's absolute deadline, so a grant can never outlive the run that authorized it.
pub const CATEGORY_GRANT_MAX_TTL: Duration = Duration::from_secs(600);
/// B11a: how many dispatches a single category approval may authorize. Exhausting it re-asks
/// instead of silently continuing.
pub const CATEGORY_GRANT_MAX_USES: u32 = 32;
/// B11a: hard bound on the live grant table. Exhausted/expired grants are retired eagerly, so this
/// only matters under a pathological approval stream; it never holds unbounded tombstones.
const GRANT_TABLE_CAPACITY: usize = 64;

/// Which production authority path raised an approval. A grant minted by one entry point can never
/// be consumed by another, so an approval through the kernel tool dispatcher does not silently
/// authorize the `session.tournament` gate or a `spawn_subagent` request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthEntry {
    Tool,
    Tournament,
    Subagent,
}

/// The identity a grant is bound to: entry point, tool, run, workspace, and the active workspace
/// revision. `run_id` is the trusted current-owner identity carried by the run's
/// [`crate::env::ExecutionContext`]; it is never taken from task-local state. A grant is only ever
/// visible to an exactly matching scope.
///
/// `revision` is the *real* workspace revision the operator reviewed (from the `SnapshotStore` /
/// active `AgentRunner` revision), not the canonical workspace root: the root is stable across
/// mutations, so using it as a revision identity would let an approval outlive the state it was
/// granted against. A scope with `revision: None` carries no revision identity (an ad-hoc call with
/// no snapshot store); it can never masquerade as a captured revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthScope {
    pub entry: AuthEntry,
    pub tool_name: String,
    pub run_id: Option<RunId>,
    pub workspace_root: PathBuf,
    pub revision: Option<RevisionId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrantKind {
    /// A single call, bound to the exact argument digest of the approved request.
    Once,
    /// A bounded, expiring reuse of one tool for the current run (the operator's "allow this tool
    /// for this run" answer). It authorizes *different* arguments of the same tool by design, but
    /// only within its scope, expiry, and remaining-use budget.
    CategoryForRun,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrantRejection {
    /// No grant exists for the scope (a base-policy allow carries no grant).
    Missing,
    Expired,
    Replay,
    DigestMismatch,
}

struct Grant {
    kind: GrantKind,
    args_digest: String,
    expires_at: Instant,
    remaining_uses: u32,
}

/// Bounded table of live approval grants. `consume` is the only path that turns a grant into a
/// dispatch, so an expired or already-spent grant can never be used twice.
#[derive(Default)]
struct GrantTable {
    entries: Vec<(AuthScope, Grant)>,
}

impl GrantTable {
    fn mint(&mut self, scope: AuthScope, grant: Grant, now: Instant) {
        self.retire_expired(now);
        // A fresh approval for a scope supersedes any stale grant for the same scope.
        self.entries.retain(|(s, _)| s != &scope);
        self.entries.push((scope, grant));
        if self.entries.len() > GRANT_TABLE_CAPACITY {
            self.entries.remove(0);
        }
    }

    fn active_category(&self, scope: &AuthScope, now: Instant) -> bool {
        self.entries.iter().any(|(s, g)| {
            s == scope
                && g.kind == GrantKind::CategoryForRun
                && now < g.expires_at
                && g.remaining_uses > 0
        })
    }

    fn consume(
        &mut self,
        scope: &AuthScope,
        args_digest: &str,
        now: Instant,
    ) -> Result<(), GrantRejection> {
        let Some(pos) = self.entries.iter().position(|(s, _)| s == scope) else {
            return Err(GrantRejection::Missing);
        };
        if now >= self.entries[pos].1.expires_at {
            self.entries.remove(pos);
            return Err(GrantRejection::Expired);
        }
        if self.entries[pos].1.kind == GrantKind::Once
            && self.entries[pos].1.args_digest != args_digest
        {
            return Err(GrantRejection::DigestMismatch);
        }
        if self.entries[pos].1.remaining_uses == 0 {
            self.entries.remove(pos);
            return Err(GrantRejection::Replay);
        }
        self.entries[pos].1.remaining_uses -= 1;
        if self.entries[pos].1.remaining_uses == 0 {
            self.entries.remove(pos);
        }
        Ok(())
    }

    fn retire_expired(&mut self, now: Instant) {
        self.entries.retain(|(_, g)| now < g.expires_at);
    }
}

/// Canonical digest of a tool call's arguments.
///
/// The arguments are parsed once and re-serialized with object keys sorted recursively (array
/// order preserved) before hashing, so two spellings of the same arguments hash identically. The
/// digest is derived from the parsed value, never from the raw bytes, so the policy decision and
/// the dispatch can never disagree about what was approved. Invalid JSON is rejected (`None`)
/// rather than hashed, so a malformed call cannot mint a grant.
pub fn canonical_arguments_digest(raw_arguments: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(raw_arguments).ok()?;
    Some(crate::prompt::sha256_hex64(
        canonical_json(&value).as_bytes(),
    ))
}

pub(crate) fn canonical_json(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let mut out = String::from("{");
            for (i, k) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&serde_json::to_string(k).unwrap_or_default());
                out.push(':');
                out.push_str(&canonical_json(&map[*k]));
            }
            out.push('}');
            out
        }
        serde_json::Value::Array(items) => {
            let mut out = String::from("[");
            for (i, v) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&canonical_json(v));
            }
            out.push(']');
            out
        }
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PolicyMode {
    ReadOnly,
    Ask,
    AllowWrite,
    AllowShell,
    FullAutonomous,
}

impl PolicyMode {
    pub fn from_flags(
        base: Option<&str>,
        allow_write: bool,
        allow_shell: bool,
    ) -> Result<Self, String> {
        if !matches!(
            base,
            None | Some("ask" | "read-only" | "allow-write" | "allow-shell")
        ) {
            return Err(format!("unknown policy mode '{}'", base.unwrap_or("")));
        }
        if let Some("read-only") = base {
            if allow_write || allow_shell {
                return Err(
                    "cannot combine --policy read-only with --allow-write or --allow-shell"
                        .to_string(),
                );
            }
            return Ok(PolicyMode::ReadOnly);
        }
        let eff_write = allow_write || base == Some("allow-write");
        let eff_shell = allow_shell || base == Some("allow-shell");
        match (eff_write, eff_shell) {
            (true, true) => Ok(PolicyMode::FullAutonomous),
            (true, false) => Ok(PolicyMode::AllowWrite),
            (false, true) => Ok(PolicyMode::AllowShell),
            (false, false) => Ok(PolicyMode::Ask),
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            PolicyMode::ReadOnly => "read-only",
            PolicyMode::Ask => "ask",
            PolicyMode::AllowWrite => "allow-write",
            PolicyMode::AllowShell => "allow-shell",
            PolicyMode::FullAutonomous => "allow-write+allow-shell",
        }
    }

    /// Whether the operator explicitly authorized unattended shell execution (`--allow-shell`).
    /// Used to gate host-side process spawning that is not a direct user-issued command, such as
    /// auto-detected project verification scripts.
    pub fn allows_shell_auto(self) -> bool {
        matches!(self, PolicyMode::AllowShell | PolicyMode::FullAutonomous)
    }

    /// B11b: the mode's *automatic* (no-approval) authority along the mutating dimension.
    /// `AllowWrite` and `FullAutonomous` apply mutations unattended; `Ask` and `AllowShell` require
    /// an interactive approval for every mutation.
    pub fn auto_mutating(self) -> bool {
        matches!(self, PolicyMode::AllowWrite | PolicyMode::FullAutonomous)
    }

    /// B11b: the mode's automatic authority along the side-effect (shell/process) dimension, i.e.
    /// whether it auto-authorizes unattended side effects without an approval.
    pub fn auto_side_effect(self) -> bool {
        self.allows_shell_auto()
    }

    /// B11b: the widest mode whose *automatic* authority is contained in this mode's automatic
    /// authority, dimension by dimension. This is the base-policy bound a child run is allowed to
    /// inherit from its parent.
    ///
    /// The modes form a partial order, not a scalar ladder: `AllowWrite` auto-authorizes
    /// `{read-only, mutating}` while `AllowShell` auto-authorizes `{read-only, side-effect}`, so
    /// neither contains the other. Taking the per-dimension intersection is therefore required;
    /// an enum-order `min`/`max` would either wrongly promote one dimension or wrongly drop both.
    pub fn decayed_auto_authority(self) -> PolicyMode {
        match (self.auto_mutating(), self.auto_side_effect()) {
            (true, true) => PolicyMode::FullAutonomous,
            (true, false) => PolicyMode::AllowWrite,
            (false, true) => PolicyMode::AllowShell,
            (false, false) => PolicyMode::ReadOnly,
        }
    }

    pub fn is_effect_enabled(self, effect_class: EffectClass, is_interactive: bool) -> bool {
        match effect_class {
            EffectClass::ReadOnly => true,
            EffectClass::Mutating => match self {
                PolicyMode::AllowWrite | PolicyMode::FullAutonomous => true,
                PolicyMode::Ask | PolicyMode::AllowShell => is_interactive,
                PolicyMode::ReadOnly => false,
            },
            EffectClass::SideEffect => match self {
                PolicyMode::AllowShell | PolicyMode::FullAutonomous => true,
                PolicyMode::Ask | PolicyMode::AllowWrite => is_interactive,
                PolicyMode::ReadOnly => false,
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyCheckResult {
    Allow,
    Ask,
    Deny(&'static str),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalRequest {
    pub tool_name: String,
    /// The kernel tool-call id this request gates, when one exists. The ACP surface reuses it as the
    /// `toolCallId` of `session/request_permission`, so a client can associate the permission prompt
    /// with the `session/update` `tool_call` it already received.
    pub call_id: Option<ToolCallId>,
    pub effect_class: EffectClass,
    pub is_sensitive_path: bool,
    pub target_path: Option<PathBuf>,
    pub diff_preview: Option<String>,
    pub command_preview: Option<String>,
    pub workspace_root: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalDecision {
    AllowOnce,
    AllowCategoryForRun,
    Deny,
}

pub trait ApprovalHandler: Send + Sync {
    fn request_approval(&self, req: &ApprovalRequest) -> ApprovalDecision;
    fn is_interactive(&self) -> bool;

    /// Gate 6: drain any `ApprovalRequested` / `ApprovalResolved` telemetry events recorded by this
    /// handler during the most recent approval wait so the runtime logger persists them.
    fn drain_events(&self) -> Vec<crate::events::EventPayload> {
        Vec::new()
    }
}

/// Non-interactive fail-safe handler: automatically denies any `Ask` prompt without hanging (§2.1).
#[derive(Debug, Clone, Default)]
pub struct NonInteractiveApprovalHandler;

impl ApprovalHandler for NonInteractiveApprovalHandler {
    fn request_approval(&self, _req: &ApprovalRequest) -> ApprovalDecision {
        ApprovalDecision::Deny
    }

    fn is_interactive(&self) -> bool {
        false
    }
}

#[derive(Clone)]
pub struct PolicyEngine {
    mode: PolicyMode,
    approval_handler: Arc<dyn ApprovalHandler>,
    /// B11a: scoped, expiring, use-counted approval grants. Replaces the old tool-name-only
    /// category cache, which leaked a grant across runs and entry points.
    grants: Arc<Mutex<GrantTable>>,
}

impl PolicyEngine {
    pub fn new(mode: PolicyMode, approval_handler: Arc<dyn ApprovalHandler>) -> Self {
        Self {
            mode,
            approval_handler,
            grants: Arc::new(Mutex::new(GrantTable::default())),
        }
    }

    pub fn mode(&self) -> PolicyMode {
        self.mode
    }

    pub fn is_interactive(&self) -> bool {
        self.approval_handler.is_interactive()
    }

    pub fn is_effect_enabled(&self, effect_class: EffectClass) -> bool {
        self.mode
            .is_effect_enabled(effect_class, self.is_interactive())
    }

    /// Base policy decision for a tool, ignoring any live approval grant. Callers that hold a run
    /// identity must use [`PolicyEngine::evaluate_with_context`] so a scoped category grant is
    /// honoured; this entry point intentionally cannot reuse a cached category approval.
    pub fn evaluate(
        &self,
        _tool_name: &str,
        effect_class: EffectClass,
        is_sensitive_path: bool,
    ) -> PolicyCheckResult {
        if self.mode == PolicyMode::ReadOnly {
            if effect_class != EffectClass::ReadOnly {
                return PolicyCheckResult::Deny("tool is forbidden under --policy read-only");
            }
            if is_sensitive_path {
                return PolicyCheckResult::Deny(
                    "sensitive path access is blocked under --policy read-only",
                );
            }
            return PolicyCheckResult::Allow;
        }

        // Sensitive paths always require explicit interactive confirmation if interactive, or immediate deny if non-interactive
        if is_sensitive_path {
            if !self.approval_handler.is_interactive() {
                return PolicyCheckResult::Deny(
                    "sensitive path access blocked in non-interactive mode",
                );
            }
            return PolicyCheckResult::Ask;
        }

        match effect_class {
            EffectClass::ReadOnly => PolicyCheckResult::Allow,
            EffectClass::Mutating => match self.mode {
                PolicyMode::AllowWrite | PolicyMode::FullAutonomous => PolicyCheckResult::Allow,
                PolicyMode::Ask | PolicyMode::AllowShell => {
                    if !self.is_interactive() {
                        PolicyCheckResult::Deny(
                            "mutating tool requires --allow-write in non-interactive mode",
                        )
                    } else {
                        PolicyCheckResult::Ask
                    }
                }
                PolicyMode::ReadOnly => PolicyCheckResult::Deny("read-only policy"),
            },
            EffectClass::SideEffect => match self.mode {
                PolicyMode::AllowShell | PolicyMode::FullAutonomous => PolicyCheckResult::Allow,
                PolicyMode::Ask | PolicyMode::AllowWrite => {
                    if !self.is_interactive() {
                        PolicyCheckResult::Deny(
                            "shell tool requires --allow-shell in non-interactive mode",
                        )
                    } else {
                        PolicyCheckResult::Ask
                    }
                }
                PolicyMode::ReadOnly => PolicyCheckResult::Deny("read-only policy"),
            },
        }
    }

    /// Ask the operator and return the raw decision. Minting a grant is the caller's job via
    /// [`PolicyEngine::prompt_approval_scoped`]; this method never widens authority on its own.
    pub fn prompt_approval(&self, req: &ApprovalRequest) -> ApprovalDecision {
        if !self.approval_handler.is_interactive() {
            return ApprovalDecision::Deny;
        }
        self.approval_handler.request_approval(req)
    }

    /// B11a: evaluate base policy plus any live category grant bound to exactly `scope`. A sensitive
    /// path never consults a category grant, so a coarse approval cannot open one. A legacy caller
    /// with no run identity gets base policy only — it can never reuse a cached category grant.
    ///
    /// When a category grant is what authorizes the call, one use is consumed **inside the same
    /// `GrantTable` lock** as the hit test. That makes the decision and its consumption atomic: a
    /// grant can never be checked here and then expire, be replayed, or be spent by a concurrent
    /// dispatch before it is consumed. The `Allow` returned from a grant hit is therefore already
    /// charged; callers must not consume it a second time. A base-policy `Allow` (e.g. `AllowWrite`)
    /// carries no grant and needs no ticket.
    pub fn evaluate_with_context(
        &self,
        scope: &AuthScope,
        effect_class: EffectClass,
        is_sensitive_path: bool,
    ) -> PolicyCheckResult {
        if !is_sensitive_path {
            let now = Instant::now();
            let mut table = self.grants.lock().unwrap();
            if table.active_category(scope, now) {
                // `active_category` proved uses > 0 and `now < expires_at`, so a category consume
                // for this scope cannot fail; treat any unexpected failure as a hard denial rather
                // than falling back to a base-policy allow.
                return match table.consume(scope, "", now) {
                    Ok(()) => PolicyCheckResult::Allow,
                    Err(_) => {
                        PolicyCheckResult::Deny("approval grant is no longer valid at dispatch")
                    }
                };
            }
        }
        self.evaluate(&scope.tool_name, effect_class, is_sensitive_path)
    }

    /// B11a: prompt the operator and mint a grant bound to `scope` and, for one-shot / sensitive
    /// approvals, to the exact `args_digest`. A category answer is only widened to a reusable grant
    /// when the run id, a live budget, **and a real workspace revision** are known; otherwise it
    /// degrades to a single call rather than becoming an unbounded cross-request grant.
    ///
    /// B12a: a scope with `revision: None` has no revision identity to bind the reuse to, so a
    /// `AllowCategoryForRun` answer there can only ever become a `Once` grant. Without this, two
    /// unrelated revision-less dispatches (or a dispatch before and after the first revision is
    /// captured) would share a category grant by both being `None`.
    pub fn prompt_approval_scoped(
        &self,
        scope: &AuthScope,
        req: &ApprovalRequest,
        args_digest: Option<&str>,
        budget_remaining: Option<Duration>,
    ) -> ApprovalDecision {
        let decision = self.prompt_approval(req);
        let now = Instant::now();
        let ttl = budget_remaining
            .map(|r| r.min(CATEGORY_GRANT_MAX_TTL))
            .unwrap_or(CATEGORY_GRANT_MAX_TTL);
        let expires_at = now + ttl;
        match decision {
            ApprovalDecision::Deny => ApprovalDecision::Deny,
            ApprovalDecision::AllowOnce => {
                if let Some(digest) = args_digest {
                    self.grants.lock().unwrap().mint(
                        scope.clone(),
                        Grant {
                            kind: GrantKind::Once,
                            args_digest: digest.to_string(),
                            expires_at,
                            remaining_uses: 1,
                        },
                        now,
                    );
                }
                ApprovalDecision::AllowOnce
            }
            ApprovalDecision::AllowCategoryForRun => {
                let can_widen = !req.is_sensitive_path
                    && scope.run_id.is_some()
                    && scope.revision.is_some()
                    && budget_remaining.is_some();
                if can_widen {
                    self.grants.lock().unwrap().mint(
                        scope.clone(),
                        Grant {
                            kind: GrantKind::CategoryForRun,
                            args_digest: args_digest.unwrap_or("").to_string(),
                            expires_at,
                            remaining_uses: CATEGORY_GRANT_MAX_USES,
                        },
                        now,
                    );
                    ApprovalDecision::AllowCategoryForRun
                } else {
                    if let Some(digest) = args_digest {
                        self.grants.lock().unwrap().mint(
                            scope.clone(),
                            Grant {
                                kind: GrantKind::Once,
                                args_digest: digest.to_string(),
                                expires_at,
                                remaining_uses: 1,
                            },
                            now,
                        );
                    }
                    ApprovalDecision::AllowOnce
                }
            }
        }
    }

    /// B11a: atomically consume a grant for exactly `scope` and `args_digest`. Expiry and use-count
    /// are settled under one lock, so two concurrent dispatches can never share a single use.
    pub fn consume_grant(
        &self,
        scope: &AuthScope,
        args_digest: &str,
    ) -> Result<(), GrantRejection> {
        self.grants
            .lock()
            .unwrap()
            .consume(scope, args_digest, Instant::now())
    }

    /// B11a: drop every grant bound to `run_id` when the run ends or is cancelled.
    pub fn invalidate_run(&self, run_id: RunId) {
        self.grants
            .lock()
            .unwrap()
            .entries
            .retain(|(s, _)| s.run_id != Some(run_id));
    }

    pub fn drain_approval_events(&self) -> Vec<crate::events::EventPayload> {
        self.approval_handler.drain_events()
    }
}
