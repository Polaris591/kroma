use crate::types::EffectClass;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

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
    allowed_categories: Arc<Mutex<HashSet<String>>>,
}

impl PolicyEngine {
    pub fn new(mode: PolicyMode, approval_handler: Arc<dyn ApprovalHandler>) -> Self {
        Self {
            mode,
            approval_handler,
            allowed_categories: Arc::new(Mutex::new(HashSet::new())),
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

    pub fn evaluate(
        &self,
        tool_name: &str,
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

        // Check if category was already approved for current run via [a]
        {
            let allowed = self.allowed_categories.lock().unwrap();
            if allowed.contains(tool_name) {
                return PolicyCheckResult::Allow;
            }
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

    pub fn prompt_approval(&self, req: &ApprovalRequest) -> ApprovalDecision {
        if !self.approval_handler.is_interactive() {
            return ApprovalDecision::Deny;
        }
        let decision = self.approval_handler.request_approval(req);
        if decision == ApprovalDecision::AllowCategoryForRun && !req.is_sensitive_path {
            let mut allowed = self.allowed_categories.lock().unwrap();
            allowed.insert(req.tool_name.clone());
        }
        decision
    }

    pub fn drain_approval_events(&self) -> Vec<crate::events::EventPayload> {
        self.approval_handler.drain_events()
    }
}
