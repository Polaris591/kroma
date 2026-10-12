use crate::tool::definition::{builtin_tool_registrations, RegisteredTool, ToolDefinition};
use crate::tool::policy::{PolicyEngine, PolicyMode};
use crate::types::EffectClass;
use std::collections::HashSet;

/// Deterministic tool registry enforcing strict ASCII ascending order and Policy-driven visibility (§2.1, §4.1).
#[derive(Debug, Clone)]
pub struct ToolRegistry {
    tools: Vec<RegisteredTool>,
    tool_filter: Option<HashSet<String>>,
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new(None)
    }
}

impl ToolRegistry {
    pub fn new(tool_filter: Option<Vec<String>>) -> Self {
        let mut tools = builtin_tool_registrations();
        // Strictly sort by tool name in ASCII ascending order
        tools.sort_by(|a, b| a.definition.name.as_bytes().cmp(b.definition.name.as_bytes()));

        let filter_set = tool_filter.map(|list| {
            list.into_iter()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect::<HashSet<String>>()
        });

        Self {
            tools,
            tool_filter: filter_set,
        }
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
        self.tools
            .iter()
            .filter(|t| self.is_tool_enabled_for_policy(t, policy))
            .map(|t| t.definition.clone())
            .collect()
    }

    /// Returns model-visible tool schemas for a `PolicyEngine` (hiding tools that will unconditionally be denied in non-interactive mode).
    pub fn visible_definitions_for_engine(&self, policy_engine: &PolicyEngine) -> Vec<ToolDefinition> {
        self.tools
            .iter()
            .filter(|t| self.is_tool_enabled_for_engine(t, policy_engine))
            .map(|t| t.definition.clone())
            .collect()
    }

    /// Look up a registered tool by name and enforce Policy + filter at the Dispatcher boundary.
    pub fn lookup_for_dispatch(
        &self,
        name: &str,
        policy: PolicyMode,
    ) -> Result<&RegisteredTool, ToolLookupError> {
        let tool = self
            .tools
            .iter()
            .find(|t| t.definition.name == name)
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

    pub fn all_registered(&self) -> &[RegisteredTool] {
        &self.tools
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolLookupError {
    UnknownTool(String),
    PolicyHidden(String),
    FilteredOut(String),
}
