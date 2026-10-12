use crate::extensions::{
    ExtensionDispatch, ExtensionError, ExtensionId, ExtensionState, HookEngine, ScopedExtension,
    SkillEntry, UnmountReport,
};
use crate::tool::definition::{builtin_tool_registrations, RegisteredTool, ToolDefinition};
use crate::tool::policy::{PolicyEngine, PolicyMode};
use crate::types::EffectClass;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};

/// Deterministic tool registry enforcing strict ASCII ascending order and Policy-driven visibility
/// (§2.1, §4.1).
///
/// Gate 3 makes the registry the mount point for reversible scoped extensions. Kernel tools are
/// physically separated from extension tools, so an extension can never shadow or remove a
/// built-in: `unregister_extension` restores the visible schemas, hooks and skills byte-for-byte to
/// the pre-mount state.
#[derive(Clone)]
pub struct ToolRegistry {
    builtin: Vec<RegisteredTool>,
    tool_filter: Option<HashSet<String>>,
    workspace_skills: Vec<SkillEntry>,
    extensions: Arc<Mutex<ExtensionState>>,
}

impl std::fmt::Debug for ToolRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolRegistry")
            .field("builtin", &self.builtin.len())
            .field("tool_filter", &self.tool_filter)
            .field("workspace_skills", &self.workspace_skills.len())
            .field("extensions", &self.extensions)
            .finish()
    }
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new(None)
    }
}

impl ToolRegistry {
    pub fn new(tool_filter: Option<Vec<String>>) -> Self {
        let mut builtin = builtin_tool_registrations();
        // Strictly sort by tool name in ASCII ascending order
        builtin.sort_by(|a, b| a.definition.name.as_bytes().cmp(b.definition.name.as_bytes()));

        let filter_set = tool_filter.map(|list| {
            list.into_iter()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect::<HashSet<String>>()
        });

        Self {
            builtin,
            tool_filter: filter_set,
            workspace_skills: Vec::new(),
            extensions: Arc::new(Mutex::new(ExtensionState::default())),
        }
    }

    /// Attach the workspace-discovered `.agents/skills` catalog (Gate 3). The catalog is fixed for
    /// the life of the registry; extension-contributed skills are added/removed dynamically.
    pub fn with_workspace_skills(mut self, skills: Vec<SkillEntry>) -> Self {
        self.workspace_skills = skills;
        self.workspace_skills.sort_by(|a, b| a.name.cmp(&b.name));
        self
    }

    fn ext(&self) -> std::sync::MutexGuard<'_, ExtensionState> {
        self.extensions.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Mount an extension. Rejects reserved kernel tool names, duplicate extensions, undeclared
    /// capabilities, trust-level violations, duplicate skill names and a mismatched API version
    /// before mutating anything, so a failed mount is a no-op.
    pub fn register_extension(&self, ext: ScopedExtension) -> Result<ExtensionId, ExtensionError> {
        // A skill contributed by an extension may not shadow a workspace-discovered skill: that
        // would silently change which body `activate_skill` returns and break reversibility.
        for skill in &ext.skills {
            if self.workspace_skills.iter().any(|s| s.name == skill.name) {
                return Err(ExtensionError::DuplicateSkill(skill.name.clone()));
            }
        }
        self.ext().register(ext)
    }

    /// Unmount an extension, removing every tool, hook and skill it contributed. In-flight calls
    /// are signalled and drained within a bounded window; the report records what was cancelled and
    /// which irreversible side effects already happened.
    pub fn unregister_extension(
        &self,
        id: &ExtensionId,
    ) -> Result<UnmountReport, ExtensionError> {
        self.ext().unregister(id)
    }

    /// Re-trust an extension whose source checksum changed (operator-only).
    pub fn retrust_extension_checksum(&self, id: &ExtensionId, checksum: impl Into<String>) {
        self.ext().retrust_checksum(id, checksum);
    }

    /// Mounted extension ids, in deterministic order.
    pub fn extension_ids(&self) -> Vec<ExtensionId> {
        self.ext().extension_ids()
    }

    /// True when no extension is currently mounted.
    pub fn extensions_are_empty(&self) -> bool {
        self.ext().is_empty()
    }

    /// Resolved dispatch for an extension-provided tool, if one is mounted under that name.
    pub fn extension_dispatch(&self, name: &str) -> Option<ExtensionDispatch> {
        self.ext().dispatch_for(name)
    }

    /// Every tool currently registered (kernel + extension), in strict ASCII ascending order.
    pub fn all_registered(&self) -> Vec<RegisteredTool> {
        let mut tools = self.builtin.clone();
        tools.extend(self.ext().extension_tools());
        tools.sort_by(|a, b| a.definition.name.as_bytes().cmp(b.definition.name.as_bytes()));
        tools
    }

    /// Every skill visible to `activate_skill`: workspace-discovered plus extension-contributed.
    pub fn skills(&self) -> Vec<SkillEntry> {
        let mut skills = self.workspace_skills.clone();
        skills.extend(self.ext().extension_skills());
        skills.sort_by(|a, b| a.name.cmp(&b.name));
        skills.dedup_by(|a, b| a.name == b.name);
        skills
    }

    /// Snapshot the currently mounted hooks as an engine. Rebuilt per call so an unmount is
    /// immediately reflected in tool dispatch.
    pub fn hook_engine(&self) -> HookEngine {
        let state = self.ext();
        HookEngine::new(state.policy_hooks(), state.observer_hooks())
    }

    fn find(&self, name: &str) -> Option<RegisteredTool> {
        if let Some(tool) = self.builtin.iter().find(|t| t.definition.name == name) {
            return Some(tool.clone());
        }
        self.ext()
            .extension_tools()
            .into_iter()
            .find(|t| t.definition.name == name)
    }

    pub fn is_tool_enabled_for_policy(&self, tool: &RegisteredTool, policy: PolicyMode) -> bool {
        if let Some(ref filter) = self.tool_filter {
            if !filter.contains(&tool.definition.name) {
                return false;
            }
        }
        if policy == PolicyMode::ReadOnly && tool.metadata.effect_class != EffectClass::ReadOnly {
            return false;
        }
        true
    }

    pub fn is_tool_enabled_for_engine(
        &self,
        tool: &RegisteredTool,
        policy_engine: &PolicyEngine,
    ) -> bool {
        if let Some(ref filter) = self.tool_filter {
            if !filter.contains(&tool.definition.name) {
                return false;
            }
        }
        policy_engine.is_effect_enabled(tool.metadata.effect_class)
    }

    /// Returns model-visible tool schemas strictly sorted in ASCII ascending order.
    pub fn visible_definitions(&self, policy: PolicyMode) -> Vec<ToolDefinition> {
        self.all_registered()
            .iter()
            .filter(|t| self.is_tool_enabled_for_policy(t, policy))
            .map(|t| t.definition.clone())
            .collect()
    }

    /// Returns model-visible tool schemas for a `PolicyEngine` (hiding tools that will unconditionally be denied in non-interactive mode).
    pub fn visible_definitions_for_engine(&self, policy_engine: &PolicyEngine) -> Vec<ToolDefinition> {
        self.all_registered()
            .iter()
            .filter(|t| self.is_tool_enabled_for_engine(t, policy_engine))
            .map(|t| t.definition.clone())
            .collect()
    }

    /// Look up a registered tool by name and enforce Policy + filter at the Dispatcher boundary.
    ///
    /// Extension tools pass through exactly the same gate as kernel tools, so `--tool-filter` and
    /// `--policy read-only` restrict them identically.
    pub fn lookup_for_dispatch(
        &self,
        name: &str,
        policy: PolicyMode,
    ) -> Result<RegisteredTool, ToolLookupError> {
        let tool = self
            .find(name)
            .ok_or_else(|| ToolLookupError::UnknownTool(name.to_string()))?;

        if let Some(ref filter) = self.tool_filter {
            if !filter.contains(name) {
                return Err(ToolLookupError::FilteredOut(name.to_string()));
            }
        }

        if policy == PolicyMode::ReadOnly && tool.metadata.effect_class != EffectClass::ReadOnly {
            return Err(ToolLookupError::PolicyHidden(name.to_string()));
        }

        Ok(tool)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolLookupError {
    UnknownTool(String),
    PolicyHidden(String),
    FilteredOut(String),
}
