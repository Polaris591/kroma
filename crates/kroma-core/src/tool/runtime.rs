use crate::activation::{ActivationError, ActivationRegistry, CapabilityClaim};
use crate::attribution::{
    added_symbols_for_call, language_name, mutation_summary, removed_symbols_for_call,
    MutatedFileSpan, MutationTelemetry, PredictiveReport, PredictiveValidator,
};
use crate::budget::{BudgetConfig, RunDeadline};
use crate::env::{ExecutionContext, SharedExecutionEnv};
use crate::extensions::{
    discover_activation_args, execute_activate_skill, execute_discover_tools, AdmissionError,
    CustomToolContext, ExtensionDispatch, HookContext, HookEvaluation,
};
use crate::loop_guard::MAX_EDIT_FAILURE_STREAK;
use crate::messages::{ToolCall, ToolResultMessage, ToolResultMeta};
use crate::prompt::sha256_hex16;
use crate::sensitive_paths::SensitivePathGuard;
use crate::tool::exposure::{Exposure, ExposureSet, RequestSnapshot};
use crate::tool::policy::{
    canonical_arguments_digest, ApprovalDecision, ApprovalRequest, AuthEntry, AuthScope,
    GrantRejection, PolicyCheckResult, PolicyEngine,
};
use crate::tool::registry::{ToolLookupError, ToolRegistry};
use crate::tools::{
    bash::execute_bash_with_deadline, edit::execute_edit, find::execute_find, grep::execute_grep,
    ls::execute_ls, read::execute_read, write::execute_write, ToolExecutionOutput,
};
use crate::types::{EffectClass, ErrorCode};
use crate::workspace::{SensitivePathApproval, WorkspaceCap};
use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;

/// C00-04: the capability whose lease is a run's own dispatch authority.
///
/// One id per run, so withdrawing it is a single epoch bump and every tool the run dispatches is
/// covered. Splitting it per family (read / write / exec) would need an owner decision about which
/// tool belongs to which grant and who may issue it; this wiring does not invent one.
pub const RUN_DISPATCH_CAPABILITY: &str = "tool-dispatch";

/// F01 (`v1.5.1`): whether a policy decision label means the tool implementation was actually
/// dispatched. A dispatched call may have written to the workspace even when it *failed* (non-zero
/// `bash` exit, timeout, cancellation, output flood, extension error), so callers that decide
/// whether a mutation snapshot is required must branch on dispatch, not on `ToolOutcome`.
///
/// Refusals that happen *before* dispatch (`deny_*`, approval denial/cancellation, TOCTOU stale,
/// deadline abort) return `false` and must never fabricate a mutation.
pub fn decision_dispatched(decision: &str) -> bool {
    if decision.starts_with("deny")
        || matches!(
            decision,
            "ask_denied" | "ask_cancelled" | "toctou_stale" | "deadline_exceeded"
        )
    {
        return false;
    }
    true
}

#[derive(Clone)]
pub struct ToolRuntime {
    registry: ToolRegistry,
    policy_engine: PolicyEngine,
    env: SharedExecutionEnv,
    budget: BudgetConfig,
    /// C00-09: the gateway every nested call is admitted through. One per runtime, so the call
    /// allowance, depth and concurrency are properties of the run rather than of a single tool.
    nested: std::sync::Arc<crate::tool::nested::NestedGateway>,
    bash_grace_period: Duration,
    /// Gate 4: run the post-mutation structural check on `edit`/`write` results.
    predictive_check: bool,
    /// F04 (`v1.5.1`): absolute deadline of the enclosing run. `execute_bash` clamps its process
    /// timeout to the remaining time and the approval prompt aborts when it passes, so a tool can
    /// never outlive the run it belongs to.
    run_deadline: Option<RunDeadline>,
    /// F05: Unified execution context of the enclosing run.
    exec_context: Option<ExecutionContext>,
    /// T12 (KR-09): discovered tools the model has activated for this run. Names plus the schema
    /// hash observed at activation time; definitions are resolved live at request time, so an
    /// unmounted extension or a narrowed policy drops the schema instead of leaving a stale one.
    activation: std::sync::Arc<std::sync::Mutex<crate::tool::activation::ToolActivation>>,
    /// T09 (KR-18): the run's execution boundary. Every spawn the runtime performs asks this policy
    /// first, so "sandbox on" cannot mean "on for the scorer only".
    execution: crate::sandbox::ExecutionPolicy,
    /// C00-04: the capability registry whose revocation epoch the dispatch gate reads, and the claim
    /// each gated tool was bound at. Shared with whoever owns activation, so a `revoke` there is
    /// visible here without a second copy of the state.
    capabilities: Arc<ActivationRegistry>,
    capability_claims: Arc<std::sync::Mutex<HashMap<String, CapabilityClaim>>>,
    /// C00-04: the run's own dispatch authority — the claim its lease issued, or the reason no claim
    /// could be issued. Shared through clones so every dispatch of this run tree is judged against
    /// the same authority.
    run_authority: Arc<std::sync::Mutex<RunDispatchAuthority>>,
    /// C00-08: the exposure frozen for the request currently in flight. A refresh (an extension
    /// mount, an MCP `tools/list` walk) that lands mid-request mutates the registry, not this — so
    /// the running request keeps the set it was given.
    request_exposure: Arc<std::sync::Mutex<InFlightExposure>>,
}

/// C00-04: what a run's own dispatch authority currently is.
#[derive(Debug, Default)]
struct RunDispatchAuthority {
    /// The claim the run's lease issued. `None` until the run binds one, so a runtime built directly
    /// by a test (no run around it) stays ungated.
    claim: Option<CapabilityClaim>,
    /// Set when no claim could be issued at all. Every dispatch is then refused with this sentence
    /// instead of running with no authority behind it.
    refused: Option<String>,
}

#[derive(Debug, Default)]
struct InFlightExposure {
    next_request_id: u64,
    in_flight: Option<RequestSnapshot>,
}

/// C00-08: what one request boundary froze, plus the diagnostic that must accompany a degraded
/// declaration. Never returned silently: `deferred_fallback` is `Some` exactly when the caller's
/// declared set was empty and the boundary had to decide what the session gets instead.
#[derive(Debug, Clone)]
pub struct RequestBoundary {
    snapshot: RequestSnapshot,
    deferred_fallback: Option<String>,
}

impl RequestBoundary {
    pub fn snapshot(&self) -> &RequestSnapshot {
        &self.snapshot
    }

    /// `Some` when the deferred exposure produced no declared tools and this boundary degraded to
    /// the authorized base set (or, when nothing is authorized, declares nothing at all).
    pub fn deferred_fallback(&self) -> Option<&str> {
        self.deferred_fallback.as_deref()
    }
}

impl ToolRuntime {
    pub fn new(
        registry: ToolRegistry,
        policy_engine: PolicyEngine,
        env: SharedExecutionEnv,
        budget: BudgetConfig,
    ) -> Self {
        let nested = std::sync::Arc::new(crate::tool::nested::NestedGateway::new(
            crate::tool::nested::NestedLimits::for_run(&budget),
        ));
        Self {
            registry,
            policy_engine,
            env,
            budget,
            nested,
            bash_grace_period: Duration::from_secs(2),
            execution: crate::sandbox::ExecutionPolicy::host(),
            activation: std::sync::Arc::new(std::sync::Mutex::new(
                crate::tool::activation::ToolActivation::new(),
            )),
            predictive_check: true,
            run_deadline: None,
            exec_context: None,
            capabilities: Arc::new(ActivationRegistry::default()),
            capability_claims: Arc::new(std::sync::Mutex::new(HashMap::new())),
            run_authority: Arc::new(std::sync::Mutex::new(RunDispatchAuthority::default())),
            request_exposure: Arc::new(std::sync::Mutex::new(InFlightExposure::default())),
        }
    }

    /// Attach the enclosing run's execution context.
    pub fn set_execution_context(&mut self, ctx: ExecutionContext) {
        if self.run_deadline.is_none() && ctx.deadline.is_some() {
            self.run_deadline = ctx.deadline;
        }
        self.exec_context = Some(ctx);
    }

    pub fn execution_context(&self) -> Option<&ExecutionContext> {
        self.exec_context.as_ref()
    }

    /// F04 (`v1.5.1`): attach the enclosing run's absolute deadline. The agent loop sets this once
    /// before the first turn; tools cloned into concurrent sub-batches inherit it.
    pub fn set_run_deadline(&mut self, deadline: Option<RunDeadline>) {
        self.run_deadline = deadline;
    }

    pub fn with_bash_grace_period(mut self, grace: Duration) -> Self {
        self.bash_grace_period = grace;
        self
    }

    /// T09 (KR-18): install the run's execution boundary. Every spawn the runtime performs (today
    /// `bash`; verifier/probe spawns go through the same policy in `bench`) asks this first.
    pub fn with_execution_policy(mut self, policy: crate::sandbox::ExecutionPolicy) -> Self {
        self.execution = policy;
        self
    }

    pub fn execution_policy(&self) -> &crate::sandbox::ExecutionPolicy {
        &self.execution
    }

    /// C00-04: attach the capability registry this runtime's dispatch gate consults. The registry is
    /// shared, not copied: whoever activates and revokes capabilities owns the same object.
    pub fn with_capabilities(mut self, capabilities: Arc<ActivationRegistry>) -> Self {
        self.capabilities = capabilities;
        self
    }

    pub fn capabilities(&self) -> &Arc<ActivationRegistry> {
        &self.capabilities
    }

    /// C00-04: make `tools` dispatchable only while `claim` — the authority a lease issued — is
    /// still current. Reactivation after a revocation is a new claim from a new lease; this old one
    /// stays dead.
    pub fn gate_tools(&self, claim: CapabilityClaim, tools: &[&str]) {
        let mut claims = self
            .capability_claims
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        for tool in tools {
            claims.insert((*tool).to_string(), claim.clone());
        }
    }

    /// C00-04: bind the run's own dispatch authority.
    ///
    /// Declares [`RUN_DISPATCH_CAPABILITY`] if nobody else has, activates it, and makes the lease's
    /// claim the authority every dispatch of this run is judged against — including tools registered
    /// after this call, because the gate is the run's authority and not a list of names. The lease is
    /// released here on purpose: what a run keeps across its turns is the claim, not the lease (see
    /// [`CapabilityClaim`]), and the gate asks the registry whether that claim is still honoured.
    ///
    /// Call this before the run dispatches anything. Returns the generation the authority was issued
    /// at, or the reason it could not be issued — in which case every dispatch is refused with that
    /// reason rather than running with no authority behind it.
    pub fn bind_dispatch_authority(&self) -> Result<u64, String> {
        if self.capabilities.state(RUN_DISPATCH_CAPABILITY).is_none() {
            // Only an absent capability is declared: an owner that declared it with a real
            // initialiser keeps it, and a run must not replace that check with a no-op.
            self.capabilities
                .declare(RUN_DISPATCH_CAPABILITY, &[], Arc::new(|| Ok(())));
        }
        // Activate before taking the lock below, so an initialisation that has to wait on another
        // holder never blocks a dispatch that is trying to read the authority.
        let issued = self
            .capabilities
            .activate(RUN_DISPATCH_CAPABILITY)
            .map(|lease| {
                let claim = lease.claim();
                let generation = lease.generation();
                (claim, generation)
            });
        let mut authority = self.run_authority.lock().unwrap_or_else(|e| e.into_inner());
        match issued {
            Ok((claim, generation)) => {
                authority.claim = Some(claim);
                authority.refused = None;
                Ok(generation)
            }
            Err(error) => {
                let refused = match &error {
                    ActivationError::Revoked { id, generation } => format!(
                        "capability `{id}` was revoked at generation {generation} while this run was \
                         activating it, so no dispatch authority was issued"
                    ),
                    other => {
                        format!("the run's dispatch authority could not be issued: {other:?}")
                    }
                };
                authority.claim = None;
                authority.refused = Some(refused.clone());
                Err(refused)
            }
        }
    }

    /// The dispatch gate. `Ok` while `tool` carries no bound authority, or carries only claims the
    /// registry still honours; `Err` is the sentence the refusal reports, naming the capability and
    /// the epoch that ended it.
    ///
    /// The run's authority and a per-tool claim from [`ToolRuntime::gate_tools`] must both hold: the
    /// per-tool claim narrows what the run was granted, it never widens it.
    pub fn dispatch_authority(&self, tool: &str) -> Result<(), String> {
        // Read each piece of state under its own lock and drop it before taking the next, so the run
        // authority, the claim map and the registry are never held at once.
        let (run_claim, run_refused) = {
            let authority = self.run_authority.lock().unwrap_or_else(|e| e.into_inner());
            (authority.claim.clone(), authority.refused.clone())
        };
        if let Some(refused) = run_refused {
            return Err(refused);
        }
        let per_tool = self
            .capability_claims
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(tool)
            .cloned();
        for claim in per_tool.into_iter().chain(run_claim) {
            self.capabilities
                .authority(&claim)
                .map_err(|revoked| revoked.describe())?;
        }
        Ok(())
    }

    /// The exposure frozen for the request currently in flight, if one has begun.
    pub fn in_flight_snapshot(&self) -> Option<RequestSnapshot> {
        self.request_exposure
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .in_flight
            .clone()
    }

    /// C00-08: freeze the exposure for the request the model is about to receive, and make it the
    /// set dispatch is judged against until the next boundary.
    ///
    /// `declared` is the tool list this request is actually sending. Every other tool the policy
    /// still permits is `Deferred` — reachable once discovered, never declared — and every tool the
    /// registry no longer permits, or whose capability was revoked, is `Hidden`. That is the whole
    /// point of the split: discovery moves a tool between `Deferred` and `Direct`, it never moves it
    /// into the authorized set, which only the gateway fills.
    ///
    /// The exposure set is rebuilt from the live registry here, so an extension mount or an MCP
    /// `tools/list` refresh that landed while the previous request was in flight is picked up at
    /// this boundary, and only at this boundary.
    pub fn begin_request(&self, declared: &[String]) -> RequestBoundary {
        let policy = self.policy_engine.mode();
        let declared_names: BTreeSet<&str> = declared.iter().map(String::as_str).collect();
        let mut exposure = ExposureSet::default();
        let mut authorized = BTreeSet::new();
        for tool in self.registry.all_registered() {
            let name = tool.definition.name.as_str();
            if !self.registry.is_tool_enabled_for_policy(&tool, policy)
                || self.dispatch_authority(name).is_err()
            {
                exposure.declare(name, Exposure::Hidden);
                continue;
            }
            exposure.declare(
                name,
                if declared_names.contains(name) {
                    Exposure::Direct
                } else {
                    Exposure::Deferred
                },
            );
            authorized.insert(name.to_string());
        }

        // C00-08: a deferred set that produced nothing must not leave the session without a tool, and
        // must not look like a normal boundary either.
        let deferred_fallback = if declared.is_empty() {
            if authorized.is_empty() {
                Some(
                    "no authorized tools at this boundary: policy and --tool-filter removed every \
                     tool, so the request declares nothing"
                        .to_string(),
                )
            } else {
                for name in &authorized {
                    exposure.declare(name.clone(), Exposure::Direct);
                }
                Some(format!(
                    "deferred exposure produced no declared tools; declaring the {} authorized base \
                     tool(s) instead",
                    authorized.len()
                ))
            }
        } else {
            None
        };

        let mut state = self
            .request_exposure
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let request_id = state.next_request_id;
        state.next_request_id += 1;
        let snapshot = RequestSnapshot::freeze(request_id, &exposure, &authorized);
        state.in_flight = Some(snapshot.clone());
        drop(state);
        RequestBoundary {
            snapshot,
            deferred_fallback,
        }
    }

    /// T12 (KR-09): activate discovered tools by name. Every refusal is returned to the caller so the
    /// model sees *why* a tool did not become callable instead of guessing.
    pub fn activate_tools(
        &self,
        names: &[String],
    ) -> Vec<(
        String,
        Result<crate::tool::activation::ActivationRecord, crate::tool::activation::ActivationError>,
    )> {
        let policy = self.policy_engine.mode();
        let catalog = self.registry.all_registered();
        let mut state = self.activation.lock().unwrap_or_else(|e| e.into_inner());
        let mut results = Vec::new();
        for name in names {
            let outcome = match catalog.iter().find(|t| &t.definition.name == name) {
                None => Err(crate::tool::activation::ActivationError::UnknownTool(
                    name.clone(),
                )),
                Some(tool) => {
                    let allowed = self.registry.is_tool_enabled_for_policy(tool, policy);
                    state.activate(tool, allowed)
                }
            };
            results.push((name.clone(), outcome));
        }
        results
    }

    /// Remove activations by name; returns the names that were actually active.
    pub fn deactivate_tools(&self, names: &[String]) -> Vec<String> {
        let mut state = self.activation.lock().unwrap_or_else(|e| e.into_inner());
        names
            .iter()
            .filter(|name| state.deactivate(name))
            .cloned()
            .collect()
    }

    /// The definitions to append to the next request's `tools` array.
    ///
    /// Resolved from the live registry and the live policy, in activation order, with anything that
    /// no longer exists (an unmounted extension) pruned. This is what makes unmount remove both the
    /// schema and the dispatch: there is no second copy of the schema to go stale.
    pub fn activated_tool_definitions(&self) -> Vec<crate::tool::definition::ToolDefinition> {
        let policy = self.policy_engine.mode();
        let catalog = self.registry.all_registered();
        let registered: Vec<String> = catalog.iter().map(|t| t.definition.name.clone()).collect();
        let mut state = self.activation.lock().unwrap_or_else(|e| e.into_inner());
        state.prune_to(&registered);
        state
            .records()
            .iter()
            .filter_map(|record| {
                catalog
                    .iter()
                    .find(|t| t.definition.name == record.name)
                    .filter(|t| self.registry.is_tool_enabled_for_policy(t, policy))
                    .map(|t| t.definition.clone())
            })
            .collect()
    }

    /// Stable digest of the active schema set (names + hashes, activation order).
    pub fn activation_digest(&self) -> String {
        self.activation
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .digest()
    }

    pub fn active_tool_names(&self) -> Vec<String> {
        self.activation
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .names()
    }

    /// Enable/disable the Gate 4 post-mutation structural check (`--no-predictive-check`).
    pub fn with_predictive_check(mut self, enabled: bool) -> Self {
        self.predictive_check = enabled;
        self
    }

    pub fn predictive_check_enabled(&self) -> bool {
        self.predictive_check
    }

    pub fn registry(&self) -> &ToolRegistry {
        &self.registry
    }

    pub fn policy_engine(&self) -> &PolicyEngine {
        &self.policy_engine
    }

    pub fn env(&self) -> &SharedExecutionEnv {
        &self.env
    }

    /// Extract the target relative path from tool arguments if present.
    pub fn extract_relative_path(raw_arguments: &str) -> Option<String> {
        let val: serde_json::Value = serde_json::from_str(raw_arguments).ok()?;
        val.get("path")?.as_str().map(|s| s.to_string())
    }

    /// Determine whether a tool call is a pure read-only operation that can be safely executed
    /// in a concurrent read-only sub-batch (no interactive approval needed).
    pub fn is_parallelizable_read_only(&self, call: &ToolCall) -> bool {
        let Ok(reg) = self
            .registry
            .lookup_for_dispatch(&call.name, self.policy_engine.mode())
        else {
            return false;
        };
        if reg.metadata.effect_class != EffectClass::ReadOnly {
            return false;
        }
        // If sensitive path is targeted directly, do not run in parallel sub-batch
        if let Some(rel) = Self::extract_relative_path(&call.raw_arguments) {
            if let Ok(norm) = WorkspaceCap::validate_relative_components(&rel) {
                if SensitivePathGuard::is_sensitive(&norm) {
                    return false;
                }
            }
        }
        // `recall_evidence` and `spawn_subagent` resolve against the live session / subagent
        // coordinator owned by the agent loop, so they must run on the serial barrier.
        if call.name == "recall_evidence" || call.name == "spawn_subagent" {
            return false;
        }
        true
    }

    /// The currently mounted hook engine (Gate 3). Rebuilt from the registry on every call so a
    /// live `unregister_extension` is reflected immediately.
    pub fn hook_engine(&self) -> crate::extensions::HookEngine {
        self.registry.hook_engine()
    }

    /// Execute a single tool call through the full security, policy, approval, TOCTOU, and ACI pipeline,
    /// explicitly carrying the enclosing run's unified [`ExecutionContext`].
    pub async fn execute_call_with_context(
        &self,
        call: &ToolCall,
        turn: usize,
        exec_ctx: &ExecutionContext,
    ) -> (ToolResultMessage, String, Vec<HookEvaluation>) {
        let (message, decision, mut evaluations) =
            self.execute_call_inner(call, turn, exec_ctx).await;
        evaluations.extend(self.post_tool_hooks(call, turn, &message).await);
        (message, decision, evaluations)
    }

    /// Execute a single tool call through the full security, policy, approval, TOCTOU, and ACI pipeline.
    ///
    /// Returns the tool result, the policy decision label, and every Gate 3 hook evaluation that
    /// contributed to the outcome (for `HookEvaluated` telemetry). `PostToolUse` observers run for
    /// *every* outcome, including early kernel-policy, path-escape, approval and TOCTOU denials, so a
    /// project audit hook never misses a call.
    pub async fn execute_call(
        &self,
        call: &ToolCall,
        turn: usize,
        cancel_rx: watch::Receiver<bool>,
    ) -> (ToolResultMessage, String, Vec<HookEvaluation>) {
        let mut exec_ctx = self
            .exec_context
            .clone()
            .unwrap_or_else(ExecutionContext::empty);
        if exec_ctx.deadline.is_none() {
            exec_ctx.deadline = self.run_deadline;
        }
        if exec_ctx.policy.is_none() {
            exec_ctx.policy = Some(Arc::new(self.policy_engine.clone()));
        }
        exec_ctx.cancel = Some(cancel_rx);
        self.execute_call_with_context(call, turn, &exec_ctx).await
    }

    /// Fail-closed `PreToolUse` policy-hook gate.
    ///
    /// Exposed so a call dispatched outside [`ToolRuntime::execute_call`] (currently
    /// `recall_evidence`, which needs the live transcript) still runs through the exact same
    /// `PreToolUse` `PolicyHook`s as every other tool.
    pub async fn pre_tool_hook_gate(
        &self,
        call: &ToolCall,
        turn: usize,
    ) -> Result<Vec<HookEvaluation>, (ToolResultMessage, Vec<HookEvaluation>)> {
        let hook_engine = self.hook_engine();
        if hook_engine.is_empty() {
            return Ok(Vec::new());
        }
        let hook_ctx = HookContext::for_tool(
            turn,
            self.env.workspace().root().to_path_buf(),
            &call.name,
            call.call_id.as_str(),
            &call.raw_arguments,
        );
        let pre = hook_engine
            .evaluate_policy(crate::extensions::HookEvent::PreToolUse, &hook_ctx)
            .await;
        let evaluations = pre.evaluations().to_vec();
        match pre.reason() {
            Some(reason) => {
                let out = ToolExecutionOutput::error(
                    ErrorCode::PolicyDenied,
                    format!("policy hook denied '{}': {}", call.name, reason),
                    ToolResultMeta {
                        relative_path: Self::extract_relative_path(&call.raw_arguments),
                        ..Default::default()
                    },
                );
                Err((Self::into_message(call, out), evaluations))
            }
            None => Ok(evaluations),
        }
    }

    /// Fail-open `PostToolUse` observer pipeline for one tool result.
    pub async fn post_tool_hooks(
        &self,
        call: &ToolCall,
        turn: usize,
        message: &ToolResultMessage,
    ) -> Vec<HookEvaluation> {
        let hook_engine = self.hook_engine();
        if hook_engine.is_empty() {
            return Vec::new();
        }
        let mut ctx = HookContext::for_tool(
            turn,
            self.env.workspace().root().to_path_buf(),
            &call.name,
            call.call_id.as_str(),
            &call.raw_arguments,
        );
        ctx.relative_path = message.meta.relative_path.clone();
        ctx.outcome = Some(if message.outcome.is_success() {
            "success".to_string()
        } else {
            "error".to_string()
        });
        hook_engine
            .run_observers(crate::extensions::HookEvent::PostToolUse, &ctx)
            .await
    }

    /// Run one extension tool call, gated against a concurrent `unregister_extension` and bound to
    /// the enclosing run's unified [`ExecutionContext`].
    ///
    /// The handler may block (an MCP tool waits on the host worker thread), so it runs on the
    /// blocking pool instead of stalling an async worker. The in-flight guard moves with it, so the
    /// call stays counted in [`crate::extensions::InFlightTracker`] until the handler actually
    /// returns. If cancelled or timed out, the async caller returns promptly without waiting for the
    /// background thread, and marks the lingering thread as a residual execution.
    /// [`crate::extensions::InFlightTracker`] enforces bounded admission across runs and refuses new
    /// overlapping calls while a residual background execution is still active, preventing
    /// overlapping side-effects from uncooperative handlers.
    async fn run_extension_tool(
        &self,
        dispatch: &ExtensionDispatch,
        raw_arguments: &str,
        exec_ctx: &ExecutionContext,
    ) -> ToolExecutionOutput {
        if dispatch.in_flight.is_cancelled() || exec_ctx.is_cancelled() {
            return ToolExecutionOutput::error(
                ErrorCode::NotExecutedCancelled,
                format!(
                    "NOT_EXECUTED_CANCELLED: call '{}' was cancelled before it could start",
                    dispatch.tool_name
                ),
                ToolResultMeta::default(),
            );
        }
        if exec_ctx.is_deadline_exceeded() {
            return ToolExecutionOutput::error(
                ErrorCode::NotExecutedBudgetExhausted,
                format!(
                    "NOT_EXECUTED_BUDGET_EXHAUSTED: run deadline exceeded before '{}' could start",
                    dispatch.tool_name
                ),
                ToolResultMeta::default(),
            );
        }

        let guard = match dispatch.in_flight.begin_admission() {
            Ok(g) => g,
            Err(AdmissionError::Unmounted) => {
                return ToolExecutionOutput::error(
                    ErrorCode::NotExecutedCancelled,
                    format!(
                        "NOT_EXECUTED_CANCELLED: extension '{}' was unmounted before '{}' could start",
                        dispatch.extension_id, dispatch.tool_name
                    ),
                    ToolResultMeta::default(),
                );
            }
            Err(AdmissionError::OverlappingResidual { residual }) => {
                return ToolExecutionOutput::error(
                    ErrorCode::IoError,
                    format!(
                        "call '{}' refused: {} residual background execution(s) from a cancelled/timed-out call are still running",
                        dispatch.tool_name, residual
                    ),
                    ToolResultMeta::default(),
                );
            }
            Err(AdmissionError::CapacityExceeded { active, limit }) => {
                return ToolExecutionOutput::error(
                    ErrorCode::IoError,
                    format!(
                        "call '{}' refused: active in-flight calls ({}) reached admission limit ({})",
                        dispatch.tool_name, active, limit
                    ),
                    ToolResultMeta::default(),
                );
            }
        };

        if dispatch.in_flight.is_cancelled() || exec_ctx.is_cancelled() {
            return ToolExecutionOutput::error(
                ErrorCode::NotExecutedCancelled,
                format!(
                    "NOT_EXECUTED_CANCELLED: call '{}' was cancelled before it could start",
                    dispatch.tool_name
                ),
                ToolResultMeta::default(),
            );
        }
        let workspace = self.env.workspace().clone();
        let in_flight = Arc::clone(&dispatch.in_flight);
        let handler = Arc::clone(&dispatch.handler);
        let raw = raw_arguments.to_string();
        let owned_exec_ctx = exec_ctx.clone();
        let residual_handle = guard.residual_handle();

        let mut join_handle = tokio::task::spawn_blocking(move || {
            // Keep the in-flight accounting alive for exactly as long as the handler runs, even if
            // the awaiting task is dropped before the handler returns.
            let _guard = guard;
            let ctx = CustomToolContext {
                workspace: &workspace,
                in_flight: Some(in_flight),
                exec_context: Some(&owned_exec_ctx),
            };
            handler.execute(&raw, &ctx)
        });

        let mut cancel_watch = exec_ctx.cancel.clone();
        let deadline_instant = exec_ctx.deadline.map(|d| d.tokio_instant());

        let out = tokio::select! {
            biased;
            _ = async {
                match cancel_watch.as_mut() {
                    Some(rx) => {
                        if *rx.borrow() { return; }
                        let _ = rx.changed().await;
                    }
                    None => std::future::pending::<()>().await,
                }
            } => {
                residual_handle.mark_residual();
                join_handle.abort();
                ToolExecutionOutput::error(
                    ErrorCode::IoError,
                    format!(
                        "extension tool '{}' was cancelled while executing; execution status unknown and side-effects may have occurred",
                        dispatch.tool_name
                    ),
                    ToolResultMeta::default(),
                )
            }
            _ = async {
                match deadline_instant {
                    Some(inst) => tokio::time::sleep_until(inst).await,
                    None => std::future::pending::<()>().await,
                }
            } => {
                residual_handle.mark_residual();
                join_handle.abort();
                ToolExecutionOutput::error(
                    ErrorCode::ToolTimeout,
                    format!(
                        "extension tool '{}' exceeded run deadline; execution status unknown and side-effects may have occurred",
                        dispatch.tool_name
                    ),
                    ToolResultMeta::default(),
                )
            }
            res = &mut join_handle => {
                match res {
                    Ok(out) => out,
                    Err(e) if e.is_cancelled() => {
                        ToolExecutionOutput::error(
                            ErrorCode::IoError,
                            format!(
                                "extension tool '{}' was aborted; execution status unknown and side-effects may have occurred",
                                dispatch.tool_name
                            ),
                            ToolResultMeta::default(),
                        )
                    }
                    Err(e) => {
                        ToolExecutionOutput::error(
                            ErrorCode::IoError,
                            format!("extension tool '{}' panicked: {}", dispatch.tool_name, e),
                            ToolResultMeta::default(),
                        )
                    }
                }
            }
        };

        if out.outcome.is_success()
            && matches!(
                dispatch.effect_class,
                EffectClass::Mutating | EffectClass::SideEffect
            )
        {
            dispatch.in_flight.record_side_effect(&dispatch.tool_name);
        }
        out
    }

    async fn execute_call_inner(
        &self,
        call: &ToolCall,
        turn: usize,
        exec_ctx: &ExecutionContext,
    ) -> (ToolResultMessage, String, Vec<HookEvaluation>) {
        // C00-04: authority is checked before dispatch, never audited after it. A capability revoked
        // since the claim was issued refuses the call here — before hooks, policy, approval or the
        // tool implementation run — so a revoked tool cannot be executed and then reported.
        if let Err(no_authority) = self.dispatch_authority(&call.name) {
            let out = ToolExecutionOutput::error(
                ErrorCode::PolicyDenied,
                format!(
                    "NOT_DISPATCHED_CAPABILITY_REVOKED: {}; tool '{}' is not dispatchable",
                    no_authority, call.name
                ),
                ToolResultMeta {
                    relative_path: Self::extract_relative_path(&call.raw_arguments),
                    ..Default::default()
                },
            );
            return (
                Self::into_message(call, out),
                "deny_capability_revoked".to_string(),
                Vec::new(),
            );
        }

        let policy_mode = self.policy_engine.mode();
        let registered = match self.registry.lookup_for_dispatch(&call.name, policy_mode) {
            Ok(r) => r,
            Err(ToolLookupError::PolicyHidden(_)) => {
                let out = ToolExecutionOutput::error(
                    ErrorCode::PolicyDenied,
                    format!(
                        "tool '{}' is disabled and hidden under --policy {}",
                        call.name,
                        policy_mode.as_str()
                    ),
                    ToolResultMeta::default(),
                );
                return (
                    Self::into_message(call, out),
                    "deny_policy_hidden".to_string(),
                    Vec::new(),
                );
            }
            Err(ToolLookupError::FilteredOut(_)) => {
                let out = ToolExecutionOutput::error(
                    ErrorCode::PolicyDenied,
                    format!("tool '{}' is disabled by --tool-filter", call.name),
                    ToolResultMeta::default(),
                );
                return (
                    Self::into_message(call, out),
                    "deny_tool_filter".to_string(),
                    Vec::new(),
                );
            }
            Err(ToolLookupError::UnknownTool(_)) => {
                let out = ToolExecutionOutput::error(
                    ErrorCode::UnknownTool,
                    format!("unknown tool '{}'", call.name),
                    ToolResultMeta::default(),
                );
                return (
                    Self::into_message(call, out),
                    "deny_unknown".to_string(),
                    Vec::new(),
                );
            }
        };

        // C00-08: a request in flight keeps the exposure it froze. A tool that a refresh revealed
        // after the boundary — or that discovery activated into the *declaration* without the
        // gateway authorizing it — is refused here, with the reason, instead of being dispatched
        // under a schema the model was never given. It becomes callable at the next boundary, where
        // the model is shown its schema.
        if let Some(snapshot) = self.in_flight_snapshot() {
            if !snapshot.may_call(&call.name) {
                let out = ToolExecutionOutput::error(
                    ErrorCode::PolicyDenied,
                    format!(
                        "NOT_DISPATCHED_REQUEST_BOUNDARY: tool '{}' is not reachable from request \
                         {} (authorized: {}); it becomes callable at the next request boundary",
                        call.name,
                        snapshot.request_id(),
                        if snapshot.declared().is_empty() {
                            "nothing declared".to_string()
                        } else {
                            format!("declared [{}]", snapshot.declared().join(", "))
                        }
                    ),
                    ToolResultMeta {
                        relative_path: Self::extract_relative_path(&call.raw_arguments),
                        ..Default::default()
                    },
                );
                return (
                    Self::into_message(call, out),
                    "deny_request_boundary".to_string(),
                    Vec::new(),
                );
            }
        }

        // Gate 3: PreToolUse policy hooks run BEFORE the kernel policy engine and are fail-closed.
        // A hook may only ever restrict; the kernel `PolicyEngine::evaluate` below is evaluated
        // independently, so a hook can never turn a kernel denial into an allow.
        let mut hook_evaluations: Vec<HookEvaluation> = Vec::new();
        match self.pre_tool_hook_gate(call, turn).await {
            Ok(evaluations) => hook_evaluations.extend(evaluations),
            Err((denied, evaluations)) => {
                return (denied, "deny_policy_hook".to_string(), evaluations);
            }
        }

        let rel_path_opt = Self::extract_relative_path(&call.raw_arguments);
        let mut is_sensitive = false;

        if matches!(call.name.as_str(), "read" | "edit" | "write") {
            if let Some(ref rel) = rel_path_opt {
                match WorkspaceCap::validate_relative_components(rel) {
                    Ok(norm) => {
                        if SensitivePathGuard::is_sensitive(&norm) {
                            is_sensitive = true;
                        } else if let Ok((_, abs)) =
                            self.env.workspace().resolve_existing_path(rel, false)
                        {
                            if let Ok(canon) = fs::canonicalize(&abs) {
                                let rel_canon = canon
                                    .strip_prefix(self.env.workspace().root())
                                    .unwrap_or(&canon);
                                if SensitivePathGuard::is_sensitive(rel_canon) {
                                    is_sensitive = true;
                                }
                            }
                        } else if matches!(
                            self.env.workspace().resolve_existing_path(rel, false),
                            Err(crate::workspace::WorkspaceError::SensitivePath { .. })
                        ) {
                            is_sensitive = true;
                        }
                    }
                    Err(e) => {
                        let out = ToolExecutionOutput::error(
                            e.error_code(),
                            e.to_string(),
                            ToolResultMeta {
                                relative_path: Some(rel.clone()),
                                ..Default::default()
                            },
                        );
                        return (
                            Self::into_message(call, out),
                            "deny_path_escape".to_string(),
                            hook_evaluations,
                        );
                    }
                }
            }
        }

        // B11a: bind the policy decision to the exact authorization scope (entry point, tool, run,
        // workspace) and the canonical argument digest, so a grant can never be reused by another
        // run, another entry point, or a rewritten argument set.
        let auth_scope = AuthScope {
            entry: AuthEntry::Tool,
            tool_name: call.name.clone(),
            run_id: exec_ctx.run_id,
            workspace_root: self.env.workspace().root().to_path_buf(),
            revision: exec_ctx.revision.clone(),
        };
        let args_digest = canonical_arguments_digest(&call.raw_arguments);

        // Evaluate Policy & SensitivePathGuard
        let check = self.policy_engine.evaluate_with_context(
            &auth_scope,
            registered.metadata.effect_class,
            is_sensitive,
        );

        let mut effective_raw_args = call.raw_arguments.clone();
        // BUG-08: a user approval for a sensitive path is scoped to exactly this call. The token
        // is minted only in the `ask_approved` arm below and consumed by a short-lived workspace
        // clone at dispatch; nothing outside this call can reuse it.
        let mut sensitive_approval: Option<SensitivePathApproval> = None;

        let decision_label = match check {
            PolicyCheckResult::Allow => {
                // B11a: a live category grant reaches this arm and `evaluate_with_context` has
                // already consumed exactly one use under its lock. A base-policy allow carries no
                // grant, so there is nothing to consume here.
                "allow".to_string()
            }
            PolicyCheckResult::Deny(reason) => {
                let code = if is_sensitive {
                    ErrorCode::SensitivePathBlocked
                } else {
                    ErrorCode::PolicyDenied
                };
                let out = ToolExecutionOutput::error(
                    code,
                    format!("{} ({})", reason, call.name),
                    ToolResultMeta {
                        relative_path: rel_path_opt,
                        ..Default::default()
                    },
                );
                return (
                    Self::into_message(call, out),
                    format!("deny:{}", reason),
                    hook_evaluations,
                );
            }
            PolicyCheckResult::Ask => {
                let compute_target_sha = |rel: &str| -> Option<String> {
                    let (_, abs) = self
                        .env
                        .workspace()
                        .resolve_existing_path(rel, false)
                        .ok()?;
                    let meta = fs::symlink_metadata(&abs).ok()?;
                    if !meta.file_type().is_file() || meta.len() > 16 * 1024 * 1024 {
                        return None;
                    }
                    let bytes = fs::read(&abs).ok()?;
                    Some(sha256_hex16(&bytes))
                };

                // Capture pre-approval SHA256 for TOCTOU protection on edit/write (§2.1, §5.2)
                let pre_approval_sha = rel_path_opt.as_deref().and_then(compute_target_sha);

                let diff_preview = if matches!(call.name.as_str(), "edit" | "write") {
                    Some(Self::build_approval_diff_preview(
                        &call.name,
                        rel_path_opt.as_deref().unwrap_or("<target>"),
                        &call.raw_arguments,
                    ))
                } else {
                    None
                };

                let command_preview = if call.name == "bash" {
                    Some(Self::build_approval_command_preview(&call.raw_arguments))
                } else {
                    None
                };

                let req = ApprovalRequest {
                    tool_name: call.name.clone(),
                    call_id: Some(call.call_id.clone()),
                    effect_class: registered.metadata.effect_class,
                    is_sensitive_path: is_sensitive,
                    target_path: rel_path_opt.as_ref().map(PathBuf::from),
                    diff_preview,
                    command_preview,
                    workspace_root: self.env.workspace().root().to_path_buf(),
                };

                let approval_cancel_rx = exec_ctx.cancel.clone();
                if approval_cancel_rx.as_ref().is_some_and(|rx| *rx.borrow()) {
                    let out = ToolExecutionOutput::error(
                        ErrorCode::NotExecutedCancelled,
                        "NOT_EXECUTED_CANCELLED: cancelled before approval prompt".to_string(),
                        ToolResultMeta {
                            relative_path: rel_path_opt,
                            ..Default::default()
                        },
                    );
                    return (
                        Self::into_message(call, out),
                        "ask_cancelled".to_string(),
                        hook_evaluations,
                    );
                }

                // F04 (`v1.5.1`): the approval prompt is bounded by the run deadline as well as by
                // cancellation, so a run cannot block forever waiting for a human.
                let approval_deadline = exec_ctx.deadline.or(self.run_deadline);
                let budget_remaining = approval_deadline.map(|d| d.remaining());
                if approval_deadline.is_some_and(|d| d.is_exceeded()) {
                    let out = ToolExecutionOutput::error(
                        ErrorCode::NotExecutedBudgetExhausted,
                        "NOT_EXECUTED_BUDGET_EXHAUSTED: run deadline exceeded before approval prompt"
                            .to_string(),
                        ToolResultMeta {
                            relative_path: rel_path_opt,
                            ..Default::default()
                        },
                    );
                    return (
                        Self::into_message(call, out),
                        "deadline_exceeded".to_string(),
                        hook_evaluations,
                    );
                }

                let pe = self.policy_engine.clone();
                let scope_for_prompt = auth_scope.clone();
                let digest_for_prompt = args_digest.clone();
                let mut prompt_task = tokio::task::spawn_blocking(move || {
                    pe.prompt_approval_scoped(
                        &scope_for_prompt,
                        &req,
                        digest_for_prompt.as_deref(),
                        budget_remaining,
                    )
                });
                let decision = tokio::select! {
                    res = &mut prompt_task => res.unwrap_or(ApprovalDecision::Deny),
                    _ = async {
                        match approval_deadline {
                            Some(d) => tokio::time::sleep_until(d.tokio_instant()).await,
                            None => std::future::pending::<()>().await,
                        }
                    } => {
                        let _ = tokio::time::timeout(
                            std::time::Duration::from_millis(200),
                            &mut prompt_task,
                        )
                        .await;
                        let out = ToolExecutionOutput::error(
                            ErrorCode::NotExecutedBudgetExhausted,
                            "NOT_EXECUTED_BUDGET_EXHAUSTED: run deadline exceeded during approval prompt"
                                .to_string(),
                            ToolResultMeta {
                                relative_path: rel_path_opt,
                                ..Default::default()
                            },
                        );
                        return (Self::into_message(call, out), "deadline_exceeded".to_string(), hook_evaluations);
                    }
                    _ = async {
                        match approval_cancel_rx {
                            Some(mut rx) => {
                                loop {
                                    if *rx.borrow() {
                                        break;
                                    }
                                    if rx.changed().await.is_err() {
                                        std::future::pending::<()>().await;
                                    }
                                    if *rx.borrow() {
                                        break;
                                    }
                                }
                            }
                            None => std::future::pending::<()>().await,
                        }
                    } => {
                        let _ = tokio::time::timeout(
                            std::time::Duration::from_millis(200),
                            &mut prompt_task,
                        )
                        .await;
                        let out = ToolExecutionOutput::error(
                            ErrorCode::NotExecutedCancelled,
                            "NOT_EXECUTED_CANCELLED: cancelled during approval prompt".to_string(),
                            ToolResultMeta {
                                relative_path: rel_path_opt,
                                ..Default::default()
                            },
                        );
                        return (Self::into_message(call, out), "ask_cancelled".to_string(), hook_evaluations);
                    }
                };

                match decision {
                    ApprovalDecision::Deny => {
                        let code = if is_sensitive {
                            ErrorCode::SensitivePathBlocked
                        } else {
                            ErrorCode::PolicyDenied
                        };
                        let out = ToolExecutionOutput::error(
                            code,
                            format!("tool '{}' execution denied by approval policy", call.name),
                            ToolResultMeta {
                                relative_path: rel_path_opt,
                                ..Default::default()
                            },
                        );
                        return (
                            Self::into_message(call, out),
                            "ask_denied".to_string(),
                            hook_evaluations,
                        );
                    }
                    ApprovalDecision::AllowOnce | ApprovalDecision::AllowCategoryForRun => {
                        // B11a: the approval minted a grant bound to this exact scope (and, for
                        // `AllowOnce` / sensitive paths, this exact argument digest). Consume it
                        // before dispatch; any rejection — including `Missing` when no digest could
                        // be bound — fails closed rather than treating an unbound approval as an
                        // allow. `evaluate_with_context` did not consume this path (it returned
                        // `Ask`), so this is the single consumption for an approved call.
                        let rejection = match args_digest.as_deref() {
                            Some(digest) => {
                                match self.policy_engine.consume_grant(&auth_scope, digest) {
                                    Ok(()) => None,
                                    Err(GrantRejection::Missing) => {
                                        Some("no grant was minted for this approval".to_string())
                                    }
                                    Err(rejection) => Some(format!("{:?}", rejection)),
                                }
                            }
                            None => Some("no argument digest to bind the approval".to_string()),
                        };
                        if let Some(detail) = rejection {
                            let out = ToolExecutionOutput::error(
                                ErrorCode::PolicyDenied,
                                format!("approval grant rejected for '{}': {}", call.name, detail),
                                ToolResultMeta {
                                    relative_path: rel_path_opt,
                                    ..Default::default()
                                },
                            );
                            return (
                                Self::into_message(call, out),
                                "deny_grant_rejected".to_string(),
                                hook_evaluations,
                            );
                        }
                        // Secondary SHA-256 TOCTOU check after user approval wait
                        if matches!(call.name.as_str(), "edit" | "write") {
                            if let Some(ref rel) = rel_path_opt {
                                let now_sha = compute_target_sha(rel);
                                if now_sha != pre_approval_sha {
                                    let out = ToolExecutionOutput::error(
                                        ErrorCode::StaleFile,
                                        format!(
                                            "TOCTOU check failed: file '{}' changed during approval wait (was {:?}, now {:?})",
                                            rel, pre_approval_sha, now_sha
                                        ),
                                        ToolResultMeta {
                                            relative_path: Some(rel.clone()),
                                            content_sha256: now_sha,
                                            ..Default::default()
                                        },
                                    );
                                    return (
                                        Self::into_message(call, out),
                                        "toctou_stale".to_string(),
                                        hook_evaluations,
                                    );
                                }
                                // Inject verified SHA-256 / if_not_exists so execute_edit/execute_write re-verifies under InodeMutationLock
                                if let Ok(mut arg_obj) =
                                    serde_json::from_str::<serde_json::Value>(&effective_raw_args)
                                {
                                    if let Some(map) = arg_obj.as_object_mut() {
                                        if let Some(ref sha) = now_sha {
                                            map.entry("expected_sha256".to_string())
                                                .or_insert_with(|| {
                                                    serde_json::Value::String(sha.clone())
                                                });
                                        } else if call.name == "write" {
                                            map.entry("if_not_exists".to_string())
                                                .or_insert_with(|| serde_json::Value::Bool(true));
                                        }
                                        if let Ok(re_ser) = serde_json::to_string(&arg_obj) {
                                            effective_raw_args = re_ser;
                                        }
                                    }
                                }
                            }
                        }
                        // BUG-08: record the approved sensitive path so the dispatched tool
                        // execution may reach exactly that file. `AllowCategoryForRun` is treated
                        // identically here because the policy engine never records a sensitive path
                        // as an allowed *category* (`PolicyEngine::prompt_approval` skips
                        // `is_sensitive_path`), so a sensitive grant is always per-call.
                        if is_sensitive {
                            if let Some(rel) = rel_path_opt.as_deref() {
                                if let Ok(norm) = WorkspaceCap::validate_relative_components(rel) {
                                    sensitive_approval = Some(SensitivePathApproval::new(norm));
                                }
                            }
                        }
                        "ask_approved".to_string()
                    }
                }
            }
        };

        // Dispatch to concrete tool implementation
        // BUG-08: hand the approved clone only to the tools whose sensitive-path detection set
        // `is_sensitive`; every other tool (and every unapproved call) keeps the plain workspace.
        let approved_workspace =
            sensitive_approval.map(|approval| self.env.workspace().with_approval(approval));
        let tool_workspace: &WorkspaceCap = approved_workspace
            .as_ref()
            .unwrap_or_else(|| self.env.workspace());
        let exec_out = match call.name.as_str() {
            "read" => execute_read(tool_workspace, &effective_raw_args),
            "edit" => {
                execute_edit(
                    tool_workspace,
                    self.env.inode_locks(),
                    &effective_raw_args,
                    self.budget.max_edit_file_bytes,
                    self.budget.max_edit_input_bytes,
                )
                .await
            }
            "write" => {
                execute_write(
                    tool_workspace,
                    self.env.inode_locks(),
                    &effective_raw_args,
                    self.budget.max_write_content_bytes,
                )
                .await
            }
            "bash" => {
                let bash_cancel_rx = exec_ctx.cancel.clone().unwrap_or_else(|| {
                    let (_, rx) = watch::channel(false);
                    rx
                });
                let bash_deadline = exec_ctx.deadline.or(self.run_deadline);
                execute_bash_with_deadline(
                    self.env.workspace().root(),
                    self.env.state_dir(),
                    &call.call_id,
                    &effective_raw_args,
                    &self.budget,
                    bash_cancel_rx,
                    self.bash_grace_period,
                    bash_deadline,
                    &self.execution,
                )
                .await
            }
            // M2-03/E04: one program instead of a dozen tool calls. The program is fed to the same
            // supervised bash path on stdin (R12: no host script path, no trusted state directory
            // mapped into the guest), with the guest view of the workspace as its root, so host and
            // isolated mode run the identical program. The declared `timeout_sec` is handed to that
            // executor (R15) instead of being dropped.
            "codemode" => {
                let parsed = crate::tool::codemode::CodeModeArgs::parse(&effective_raw_args);
                // C00-09: the program is a nested call, admitted before it starts. A refusal is
                // reported through the same channel as any other failure - the program does not run.
                let limits = crate::tool::nested::NestedLimits::for_run(&self.budget);
                let admitted = self.nested.begin(0);
                // `None` means the arguments never reached the executor: an invalid call must not
                // start a process, and it must not be reported as a program failure either.
                let run = match (&parsed, &admitted) {
                    (Err(_), _) => None,
                    (_, Err(refusal)) => Some((
                        format!(
                            "printf '%s\\n' {} >&2; exit 1",
                            crate::tool::codemode::shell_quote(&format!(
                                "codemode refused: {refusal:?}"
                            ))
                        ),
                        None,
                    )),
                    (Ok(args), Ok(_)) => Some((
                        crate::tool::codemode::program_command(
                            &self
                                .execution
                                .guest_workspace_root(self.env.workspace().root()),
                            &args.code,
                            limits.max_cpu_seconds,
                            limits.max_memory_bytes,
                        ),
                        Some(args.timeout_sec()),
                    )),
                };
                let outcome = match (run, parsed) {
                    (Some((command, timeout_sec)), _) => {
                        let cancel_rx = exec_ctx.cancel.clone().unwrap_or_else(|| {
                            let (_, rx) = watch::channel(false);
                            rx
                        });
                        let deadline = exec_ctx.deadline.or(self.run_deadline);
                        let raw =
                            serde_json::json!({ "command": command, "timeout_sec": timeout_sec })
                                .to_string();
                        execute_bash_with_deadline(
                            self.env.workspace().root(),
                            self.env.state_dir(),
                            &call.call_id,
                            &raw,
                            &self.budget,
                            cancel_rx,
                            self.bash_grace_period,
                            deadline,
                            &self.execution,
                        )
                        .await
                    }
                    (None, Err(e)) => crate::tools::ToolExecutionOutput::error(
                        crate::types::ErrorCode::InvalidArguments,
                        format!("invalid `codemode` arguments: {e}"),
                        crate::messages::ToolResultMeta::default(),
                    ),
                    (None, Ok(_)) => unreachable!("a parsed call always produces a command"),
                };
                // Settlement releases the in-flight slot, and an over-large result is refused here -
                // that is how the output bound reaches the caller instead of being enforced nowhere.
                match admitted {
                    Ok(ticket) => match self.nested.settle(ticket, outcome.content.len()) {
                        Ok(()) => outcome,
                        Err(refusal) => crate::tools::ToolExecutionOutput::error(
                            crate::types::ErrorCode::InvalidArguments,
                            format!("codemode result refused: {refusal:?}"),
                            crate::messages::ToolResultMeta::default(),
                        ),
                    },
                    Err(_) => outcome,
                }
            }
            "ls" => execute_ls(self.env.workspace(), &effective_raw_args),
            "find" => execute_find(self.env.workspace(), &effective_raw_args),
            "grep" => execute_grep(self.env.workspace(), &effective_raw_args),
            // Gate 7 (v1.2.0): symbol dependency graph + Personalized PageRank navigation.
            "code_graph" => {
                crate::repo_graph::execute_code_graph(self.env.workspace(), &effective_raw_args)
            }
            // Gate 3: on-demand skill activation, read-only and visible under every policy mode.
            "activate_skill" => {
                execute_activate_skill(&self.registry.skills(), &effective_raw_args)
            }
            // Capability gap #6 (`v1.5.2`): on-demand tool discovery. Only tools the active policy
            // permits are listed, so discovery can never widen the model's authority.
            "discover_tools" => {
                let policy = self.policy_engine().mode();
                let catalog: Vec<_> = self
                    .registry
                    .all_registered()
                    .into_iter()
                    .filter(|tool| self.registry.is_tool_enabled_for_policy(tool, policy))
                    .collect();
                // T12 (KR-09): discovery is the entry point of the loop, so it is also where
                // activation happens. Without this the model could learn a tool's schema and still be
                // unable to call it on a provider that only permits declared tools.
                let mut out = execute_discover_tools(&catalog, &effective_raw_args);
                // T12 (KR-09): activation is the second half of the same call, parsed from the same
                // declared argument set. A malformed call has already been reported by the catalog
                // call above, so there is nothing to activate.
                let (activate, deactivate) =
                    discover_activation_args(&effective_raw_args).unwrap_or_default();
                let mut report = String::new();
                if !activate.is_empty() {
                    for (name, outcome) in self.activate_tools(&activate) {
                        match outcome {
                            Ok(record) => report.push_str(&format!(
                                "activated {} (schema {} bytes, hash {})\n",
                                name,
                                record.schema_bytes,
                                &record.schema_hash[..12]
                            )),
                            Err(e) => report.push_str(&format!("not activated: {}\n", e)),
                        }
                    }
                }
                if !deactivate.is_empty() {
                    for name in self.deactivate_tools(&deactivate) {
                        report.push_str(&format!("deactivated {}\n", name));
                    }
                }
                if !report.is_empty() {
                    report.push_str(&format!(
                        "active set: [{}] digest {}\n",
                        self.active_tool_names().join(", "),
                        &self.activation_digest()[..12]
                    ));
                    out.content = format!("{}\n--- activation ---\n{}", out.content, report);
                }
                out
            }
            // `recall_evidence` is dispatched by the agent loop, which owns the live transcript and
            // the evidence store. Reaching the runtime directly means no transcript was available.
            "recall_evidence" => ToolExecutionOutput::error(
                ErrorCode::InvalidArguments,
                "recall_evidence must be dispatched by the agent loop against the live session transcript".to_string(),
                ToolResultMeta::default(),
            ),
            // Gate 5: `spawn_subagent` is dispatched by the agent loop, which owns the subagent
            // backend, spawn budget, snapshot store, and completion gate.
            "spawn_subagent" => ToolExecutionOutput::error(
                ErrorCode::InvalidArguments,
                "spawn_subagent must be dispatched by the agent loop against the subagent coordinator".to_string(),
                ToolResultMeta::default(),
            ),
            // Gate 3: an extension-provided tool. It already passed the same registry lookup,
            // `--tool-filter`, kernel `PolicyEngine` and approval pipeline as every kernel tool.
            other => match self.registry.extension_dispatch(other) {
                Some(dispatch) => {
                    self.run_extension_tool(&dispatch, &effective_raw_args, exec_ctx)
                        .await
                }
                None => ToolExecutionOutput::error(
                    ErrorCode::UnknownTool,
                    format!("unknown tool '{}'", other),
                    ToolResultMeta::default(),
                ),
            },
        };

        let exec_out = self.attach_gate4_telemetry(call, exec_out, &effective_raw_args);

        (
            Self::into_message(call, exec_out),
            decision_label,
            hook_evaluations,
        )
    }

    /// Gate 4: run the post-mutation structural check on a successful `edit`/`write`, attach the
    /// advisory warning block to the tool output, and record the mutation spans + check summary in
    /// the result metadata so the agent loop can build a `MutationRecord`.
    ///
    /// The change is never rolled back here: the block is advisory evidence only.
    fn attach_gate4_telemetry(
        &self,
        call: &ToolCall,
        mut out: ToolExecutionOutput,
        raw_args: &str,
    ) -> ToolExecutionOutput {
        if !out.outcome.is_success() || !matches!(call.name.as_str(), "edit" | "write") {
            return out;
        }
        let Some(rel) = out.meta.relative_path.clone() else {
            return out;
        };
        let content = crate::attribution::read_workspace_text(self.env.workspace(), &rel).ok();
        let report = match &content {
            Some(content) => {
                PredictiveValidator::new(self.predictive_check).validate_content(&rel, content)
            }
            None => PredictiveReport::skipped(
                &rel,
                language_name(&rel),
                "file could not be re-read after the mutation",
                0,
            ),
        };
        if let Some(block) = report.render_block() {
            let mut appended = String::from(out.content.as_str());
            appended.push('\n');
            appended.push_str(&block);
            out.content = appended;
        }
        let (start_line, end_line) = out.meta.lines_range.unwrap_or((0, 0));
        let span = MutatedFileSpan {
            relative_path: rel.clone(),
            start_line,
            end_line,
            added_symbols: added_symbols_for_call(&call.name, raw_args, content.as_deref()),
            removed_symbols: removed_symbols_for_call(&call.name, raw_args),
            summary: mutation_summary(&call.name, &rel, start_line, end_line),
        };
        out.meta.mutation = Some(MutationTelemetry {
            spans: vec![span],
            // A `skipped` report (check disabled via `--no-predictive-check`, unsupported file type,
            // unreadable or oversized file) produces no `PredictiveCheck` telemetry: an event means
            // a structural check actually ran.
            predictive: if report.status == "skipped" {
                None
            } else {
                Some(report.summary())
            },
        });
        out
    }

    fn build_approval_diff_preview(tool_name: &str, rel_path: &str, raw_args: &str) -> String {
        let Ok(val) = serde_json::from_str::<serde_json::Value>(raw_args) else {
            return raw_args.to_string();
        };
        let mut diff = format!("--- a/{}\n+++ b/{}\n", rel_path, rel_path);
        if tool_name == "write" {
            if let Some(content) = val.get("content").and_then(|v| v.as_str()) {
                for line in content.lines().take(50) {
                    diff.push('+');
                    diff.push_str(line);
                    diff.push('\n');
                }
                return diff;
            }
        } else if tool_name == "edit" {
            if let Some(edits) = val.get("edits").and_then(|v| v.as_array()) {
                for (i, item) in edits.iter().enumerate() {
                    diff.push_str(&format!("@@ edit #{} @@\n", i + 1));
                    if let Some(old_t) = item.get("old_text").and_then(|v| v.as_str()) {
                        for l in old_t.lines().take(25) {
                            diff.push('-');
                            diff.push_str(l);
                            diff.push('\n');
                        }
                    }
                    if let Some(new_t) = item.get("new_text").and_then(|v| v.as_str()) {
                        for l in new_t.lines().take(25) {
                            diff.push('+');
                            diff.push_str(l);
                            diff.push('\n');
                        }
                    }
                }
                return diff;
            }
        }
        raw_args.to_string()
    }

    fn build_approval_command_preview(raw_args: &str) -> String {
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(raw_args) {
            if let Some(cmd) = val.get("command").and_then(|v| v.as_str()) {
                return cmd.to_string();
            }
        }
        raw_args.to_string()
    }

    pub fn augment_edit_streak_warning(msg: &mut ToolResultMessage, streak: usize) {
        if streak >= MAX_EDIT_FAILURE_STREAK {
            let path = msg.meta.relative_path.as_deref().unwrap_or("<target_file>");
            let appended = format!(
                "{}\n[CIRCUIT BREAKER: {} consecutive failed `edit` attempts on '{}'. \
                You MUST call `read` on '{}' around the target lines before attempting another `edit`.]",
                msg.content, streak, path, path
            );
            msg.content = Arc::from(appended);
        }
    }

    fn into_message(call: &ToolCall, out: ToolExecutionOutput) -> ToolResultMessage {
        let mut meta = out.meta;
        // Gate 7: multimodal attachments travel with the result message so they are persisted in the
        // session WAL and can be projected into the next provider request.
        if !out.image_attachments.is_empty() {
            meta.image_attachments = out.image_attachments;
        }
        ToolResultMessage {
            call_id: call.call_id.clone(),
            tool_name: call.name.clone(),
            outcome: out.outcome,
            content: Arc::from(out.content),
            meta,
        }
    }
}
