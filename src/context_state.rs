//! Durable context state shared by the session journal and context policies.
//! The built-in variant deliberately serializes exactly like legacy v3
//! `compression` records; custom strategies use an explicit versioned envelope.
use crate::protocol::Message;
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeSet;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SummarySegment {
    pub until: usize,
    pub text: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CompactionState {
    #[serde(default)]
    pub summaries: Vec<SummarySegment>,
    #[serde(default, rename = "summary", skip_serializing)]
    pub(crate) legacy_summary: String,
    pub covered_until: usize,
    #[serde(default = "initial_boundary")]
    pub consumed_until: usize,
    #[serde(default)]
    pub cleared_tools: BTreeSet<usize>,
    #[serde(default)]
    pub revision: u64,
}

impl Default for CompactionState {
    fn default() -> Self {
        Self {
            summaries: Vec::new(),
            legacy_summary: String::new(),
            covered_until: 1,
            consumed_until: 1,
            cleared_tools: BTreeSet::new(),
            revision: 0,
        }
    }
}

const fn initial_boundary() -> usize {
    1
}

impl CompactionState {
    pub fn migrate_legacy(&mut self) -> Result<bool> {
        let mut changed = false;
        if !self.legacy_summary.is_empty() {
            if !self.summaries.is_empty() || self.covered_until <= 1 {
                bail!("inconsistent legacy compaction summary");
            }
            self.summaries.push(SummarySegment {
                until: self.covered_until,
                text: std::mem::take(&mut self.legacy_summary),
            });
            changed = true;
        }
        if self.consumed_until == 1 && !self.cleared_tools.is_empty() {
            self.cleared_tools.clear();
            changed = true;
        }
        Ok(changed)
    }

    pub fn validate(&self, messages: &[Message]) -> Result<()> {
        if messages.first().is_none_or(|msg| msg.role != "system") {
            bail!("conversation must begin with a system message");
        }
        if self.covered_until < 1 || self.covered_until > messages.len() {
            bail!("invalid compaction coverage boundary");
        }
        if self.consumed_until < 1 || self.consumed_until > messages.len() {
            bail!("invalid consumed tool boundary");
        }
        let mut end = 1;
        for segment in &self.summaries {
            if segment.text.trim().is_empty()
                || segment.until <= end
                || segment.until > messages.len()
            {
                bail!("invalid incremental summary segment");
            }
            end = segment.until;
        }
        if end != self.covered_until {
            bail!("compacted history and summary segment boundaries do not match");
        }
        if self.cleared_tools.iter().any(|&index| {
            index < self.covered_until
                || index >= self.consumed_until
                || messages.get(index).is_none_or(|msg| msg.role != "tool")
        }) {
            bail!("invalid TRC tool index in saved session");
        }
        Ok(())
    }
}

/// Not a plugin ABI: this is an internal persisted-state envelope.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalContextState {
    pub strategy: String,
    pub version: u32,
    pub data: Value,
}

impl ExternalContextState {
    pub fn validate(&self) -> Result<()> {
        if self.strategy.trim().is_empty() || self.version == 0 {
            bail!("invalid external context strategy/version");
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PersistedContext {
    Builtin(CompactionState),
    External(ExternalContextState),
}

impl Default for PersistedContext {
    fn default() -> Self {
        Self::Builtin(CompactionState::default())
    }
}

impl From<CompactionState> for PersistedContext {
    fn from(state: CompactionState) -> Self {
        Self::Builtin(state)
    }
}

impl PersistedContext {
    pub fn validate(&self, messages: &[Message]) -> Result<()> {
        match self {
            Self::Builtin(s) => s.validate(messages),
            Self::External(s) => s.validate(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn builtin_state_keeps_legacy_v3_wire_format() {
        let original = CompactionState::default();
        let raw = serde_json::to_vec(&original).unwrap();
        let persisted = serde_json::to_vec(&PersistedContext::Builtin(original)).unwrap();
        assert_eq!(persisted, raw, "default sessions must not change format");
        let restored: PersistedContext = serde_json::from_slice(&raw).unwrap();
        assert!(matches!(restored, PersistedContext::Builtin(_)));

        let legacy = json!({
            "summary": "previous session summary",
            "covered_until": 3,
            "consumed_until": 3,
            "cleared_tools": [],
            "revision": 0
        });
        let mut parsed: CompactionState = serde_json::from_value(legacy).unwrap();
        assert!(parsed.migrate_legacy().unwrap());
        assert_eq!(parsed.summaries[0].text, "previous session summary");
        assert!(serde_json::to_value(&parsed)
            .unwrap()
            .get("summary")
            .is_none());
    }

    #[test]
    fn external_state_is_unambiguously_versioned_and_round_trips() {
        let state = PersistedContext::External(ExternalContextState {
            strategy: "test.counter".into(),
            version: 1,
            data: json!({"counter": 3}),
        });
        let serialized = serde_json::to_vec(&state).unwrap();
        let parsed: PersistedContext = serde_json::from_slice(&serialized).unwrap();
        assert!(matches!(
            parsed,
            PersistedContext::External(ExternalContextState {
                strategy,
                version: 1,
                ..
            }) if strategy == "test.counter"
        ));
    }
}
