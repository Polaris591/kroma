//! T12 (KR-09): the discovery → activation → call → result → unmount loop.
//!
//! `discover_tools` used to be read-only: it printed a catalog into a tool *result* and nothing else.
//! With `on_demand_tools` on, the next request's `tools` array still held only the core set, so a
//! provider that enforces "you may only call a declared tool" could never call what it had just
//! discovered. The loop was open at exactly the point that mattered.
//!
//! This module is the activation state machine. It stores *names plus the schema hash observed at
//! activation time*; the actual definitions are resolved at request time from the live registry and
//! the live policy, so an unmounted extension or a narrowed policy removes the schema automatically
//! instead of leaving a stale one behind.

use crate::tool::definition::{RegisteredTool, ToolDefinition};

/// How many discovered tools may be activated at once. Small on purpose: activation adds schema bytes
/// to every subsequent request, and the point of on-demand tools is to keep requests small.
pub const MAX_ACTIVE_TOOLS: usize = 8;

/// Byte budget for the activated schemas (canonical JSON, one per tool, summed).
pub const MAX_ACTIVE_SCHEMA_BYTES: usize = 64 * 1024;

/// Why an activation was refused. Every variant is a refusal, never a silent skip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActivationError {
    /// No such tool in the registry.
    UnknownTool(String),
    /// The tool exists but the active policy does not permit it. Discovery must never widen authority.
    DeniedByPolicy(String),
    /// Already active; activating twice is a no-op, reported so the caller knows.
    AlreadyActive(String),
    /// The activation budget is full.
    TooManyActive { limit: usize },
    /// This tool's schema alone does not fit the byte budget.
    SchemaTooLarge { bytes: usize, limit: usize },
    /// Activation would push the total activated schema bytes over the budget.
    SchemaBudgetExceeded { bytes: usize, limit: usize },
    /// The tool is not activatable (e.g. a kernel tool that is always visible anyway).
    NotActivatable(String),
}

impl std::fmt::Display for ActivationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ActivationError::UnknownTool(name) => write!(f, "unknown tool '{}'", name),
            ActivationError::DeniedByPolicy(name) => {
                write!(f, "tool '{}' is not permitted by the active policy", name)
            }
            ActivationError::AlreadyActive(name) => write!(f, "tool '{}' is already active", name),
            ActivationError::TooManyActive { limit } => {
                write!(f, "at most {} tools may be active at once", limit)
            }
            ActivationError::SchemaTooLarge { bytes, limit } => write!(
                f,
                "this tool's schema is {} bytes, over the {} byte per-tool limit",
                bytes, limit
            ),
            ActivationError::SchemaBudgetExceeded { bytes, limit } => write!(
                f,
                "activated schemas would total {} bytes, over the {} byte budget",
                bytes, limit
            ),
            ActivationError::NotActivatable(name) => {
                write!(
                    f,
                    "tool '{}' is always visible and cannot be activated",
                    name
                )
            }
        }
    }
}

/// One activation, with the schema identity observed when it happened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivationRecord {
    pub name: String,
    /// sha256 of the canonical schema JSON at activation time. Recorded so a later request can be
    /// tied to the exact schema the model was given.
    pub schema_hash: String,
    pub schema_bytes: usize,
    /// Monotonic activation order; the request's tool list is sorted by it, so the order is stable
    /// across turns and a diff of two requests is meaningful.
    pub order: u64,
}

/// The activation state machine. Cheap to clone; the runtime keeps one per run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolActivation {
    records: Vec<ActivationRecord>,
    next_order: u64,
}

impl ToolActivation {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_active(&self, name: &str) -> bool {
        self.records.iter().any(|r| r.name == name)
    }

    pub fn records(&self) -> &[ActivationRecord] {
        &self.records
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Total canonical schema bytes currently activated.
    pub fn schema_bytes(&self) -> usize {
        self.records.iter().map(|r| r.schema_bytes).sum()
    }

    /// Activate `tool`, enforcing the policy verdict, the count cap and the byte budget.
    pub fn activate(
        &mut self,
        tool: &RegisteredTool,
        policy_allows: bool,
    ) -> Result<ActivationRecord, ActivationError> {
        let name = tool.definition.name.clone();
        if self.is_active(&name) {
            return Err(ActivationError::AlreadyActive(name));
        }
        if !policy_allows {
            return Err(ActivationError::DeniedByPolicy(name));
        }
        if self.records.len() >= MAX_ACTIVE_TOOLS {
            return Err(ActivationError::TooManyActive {
                limit: MAX_ACTIVE_TOOLS,
            });
        }
        let schema = canonical_schema(&tool.definition)?;
        let schema_bytes = schema.len();
        if schema_bytes > MAX_ACTIVE_SCHEMA_BYTES {
            return Err(ActivationError::SchemaTooLarge {
                bytes: schema_bytes,
                limit: MAX_ACTIVE_SCHEMA_BYTES,
            });
        }
        let total = self.schema_bytes().saturating_add(schema_bytes);
        if total > MAX_ACTIVE_SCHEMA_BYTES {
            return Err(ActivationError::SchemaBudgetExceeded {
                bytes: total,
                limit: MAX_ACTIVE_SCHEMA_BYTES,
            });
        }
        let record = ActivationRecord {
            name,
            schema_hash: crate::prompt::sha256_hex64(schema.as_bytes()),
            schema_bytes,
            order: self.next_order,
        };
        self.next_order += 1;
        self.records.push(record.clone());
        Ok(record)
    }

    /// Remove one activation. Returns whether it was active.
    pub fn deactivate(&mut self, name: &str) -> bool {
        let before = self.records.len();
        self.records.retain(|r| r.name != name);
        self.records.len() != before
    }

    /// Remove every activation whose tool no longer exists (an unmounted extension). Returns the
    /// names that were dropped.
    pub fn prune_to(&mut self, still_registered: &[String]) -> Vec<String> {
        let mut dropped = Vec::new();
        self.records.retain(|r| {
            let keep = still_registered.iter().any(|n| n == &r.name);
            if !keep {
                dropped.push(r.name.clone());
            }
            keep
        });
        dropped
    }

    /// Stable digest over the ordered `(name, schema_hash)` pairs. Two requests with the same digest
    /// were given the same tool schemas.
    pub fn digest(&self) -> String {
        let mut canonical = String::new();
        for record in &self.records {
            canonical.push_str(&record.name);
            canonical.push('=');
            canonical.push_str(&record.schema_hash);
            canonical.push('\n');
        }
        crate::prompt::sha256_hex64(canonical.as_bytes())
    }

    /// The activation names, in activation order.
    pub fn names(&self) -> Vec<String> {
        self.records.iter().map(|r| r.name.clone()).collect()
    }
}

/// Canonical schema JSON: `serde_json`'s object map is a `BTreeMap`, so key order is already stable;
/// this exists so the hash input is spelled out in one place instead of being an accident.
pub fn canonical_schema(definition: &ToolDefinition) -> Result<String, ActivationError> {
    serde_json::to_string(&definition.parameters).map_err(|_| {
        ActivationError::NotActivatable(format!(
            "tool '{}' has a schema that cannot be serialized",
            definition.name
        ))
    })
}
