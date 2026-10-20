//! C00-02: the per-run resource envelope.
//!
//! The blueprint requires fourteen explicit bounds before anything runs unattended
//! (`CAPABILITY_CONTRACT.json` → `resource_envelope_required_fields`), and says why:
//! *"All limits must resolve to explicit finite bounds before unattended execution."*
//! A missing bound is not "unlimited", it is "unknown", and unknown is the direction that
//! lets a run grow without an owner noticing.
//!
//! This module is a contract, not a second authority. It parses and validates the envelope,
//! then hands the enforceable parts to the components that already own them: token limits go
//! to [`TokenBudget`], wall time to [`BudgetConfig::max_run_duration`], tool output to
//! [`BudgetConfig::max_total_tool_output_bytes`]. There is no envelope-local counter, so an
//! envelope can never disagree with the ledger it is supposed to bound.

use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::budget::{BudgetConfig, TokenBudget};

/// Format tag so an envelope is recognisable in evidence and in a config file.
pub const ENVELOPE_FORMAT: &str = "kroma.resource_envelope/1";

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum EnvelopeError {
    #[error("resource envelope field `{field}` must be a positive bound; 0 would silently mean \"nothing may run\"")]
    ZeroBound { field: &'static str },
}

/// The fourteen bounds a run must declare before it may execute unattended.
///
/// Every field is required: deserialisation fails when one is absent, so an incomplete
/// envelope is refused at the edge instead of defaulting to "no limit". `authorized_cost_limit`
/// is in micro-USD to stay exact; a float here would round a spend cap.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceEnvelope {
    pub memory_bytes: u64,
    pub queue_items: u32,
    pub queue_bytes: u64,
    pub active_tasks: u32,
    pub processes: u32,
    pub file_descriptors: u32,
    pub tool_output_bytes: u64,
    pub disk_bytes: u64,
    pub wall_time_ms: u64,
    pub activation_timeout_ms: u64,
    pub cancellation_grace_ms: u64,
    pub switch_count: u32,
    pub authorized_token_limit: u64,
    /// Spend ceiling for the whole run tree, in micro-USD (1e-6 USD).
    pub authorized_cost_limit: u64,
}

impl ResourceEnvelope {
    /// Reject a bound that is present but meaningless.
    ///
    /// Deserialisation already refuses a missing field. This catches the other silent case:
    /// `0`, which reads like "disabled" but behaves like "the first request fails", and would
    /// make a run look configured when it cannot do anything.
    pub fn validate(&self) -> Result<(), EnvelopeError> {
        let bounds: [(&'static str, u64); 14] = [
            ("memory_bytes", self.memory_bytes),
            ("queue_items", self.queue_items.into()),
            ("queue_bytes", self.queue_bytes),
            ("active_tasks", self.active_tasks.into()),
            ("processes", self.processes.into()),
            ("file_descriptors", self.file_descriptors.into()),
            ("tool_output_bytes", self.tool_output_bytes),
            ("disk_bytes", self.disk_bytes),
            ("wall_time_ms", self.wall_time_ms),
            ("activation_timeout_ms", self.activation_timeout_ms),
            ("cancellation_grace_ms", self.cancellation_grace_ms),
            ("switch_count", self.switch_count.into()),
            ("authorized_token_limit", self.authorized_token_limit),
            ("authorized_cost_limit", self.authorized_cost_limit),
        ];
        match bounds.into_iter().find(|(_, value)| *value == 0) {
            Some((field, _)) => Err(EnvelopeError::ZeroBound { field }),
            None => Ok(()),
        }
    }

    pub fn wall_time(&self) -> Duration {
        Duration::from_millis(self.wall_time_ms)
    }

    pub fn activation_timeout(&self) -> Duration {
        Duration::from_millis(self.activation_timeout_ms)
    }

    pub fn cancellation_grace(&self) -> Duration {
        Duration::from_millis(self.cancellation_grace_ms)
    }

    /// The run tree's token ceiling, in the ledger that already owns token accounting.
    pub fn token_budget(&self) -> Arc<TokenBudget> {
        Arc::new(TokenBudget::new(Some(self.authorized_token_limit)))
    }

    /// Project the enforceable bounds onto the run configuration, so the envelope and the
    /// kernel cannot disagree. The remaining fields (memory, processes, descriptors, disk)
    /// stay here until the components that can enforce them exist; they are deliberately not
    /// turned into advisory copies of a limit nobody checks.
    pub fn apply_to(&self, config: &mut BudgetConfig) {
        config.max_total_tokens = Some(self.authorized_token_limit);
        config.max_run_duration = self.wall_time();
        config.max_total_tool_output_bytes = self.tool_output_bytes as usize;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::budget::Settlement;

    const COMPLETE: &str = r#"{
        "memory_bytes": 1073741824,
        "queue_items": 64,
        "queue_bytes": 4194304,
        "active_tasks": 4,
        "processes": 16,
        "file_descriptors": 256,
        "tool_output_bytes": 4194304,
        "disk_bytes": 10737418240,
        "wall_time_ms": 900000,
        "activation_timeout_ms": 30000,
        "cancellation_grace_ms": 5000,
        "switch_count": 8,
        "authorized_token_limit": 600000,
        "authorized_cost_limit": 5000000
    }"#;

    fn envelope() -> ResourceEnvelope {
        serde_json::from_str(COMPLETE).expect("the reference envelope must parse")
    }

    /// C00-02-A1: a missing bound is refused, not defaulted.
    #[test]
    fn every_required_bound_is_required() {
        let complete: serde_json::Value = serde_json::from_str(COMPLETE).unwrap();
        let fields = [
            "memory_bytes",
            "queue_items",
            "queue_bytes",
            "active_tasks",
            "processes",
            "file_descriptors",
            "tool_output_bytes",
            "disk_bytes",
            "wall_time_ms",
            "activation_timeout_ms",
            "cancellation_grace_ms",
            "switch_count",
            "authorized_token_limit",
            "authorized_cost_limit",
        ];
        assert_eq!(fields.len(), 14, "the contract lists fourteen bounds");
        for field in fields {
            let mut partial = complete.clone();
            partial.as_object_mut().unwrap().remove(field);
            let parsed: Result<ResourceEnvelope, _> = serde_json::from_value(partial);
            assert!(
                parsed.is_err(),
                "an envelope without `{field}` must be refused, not defaulted"
            );
        }
    }

    /// A typo'd key is a missing key in disguise, so it is refused too.
    #[test]
    fn unknown_keys_are_refused() {
        let mut value: serde_json::Value = serde_json::from_str(COMPLETE).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .insert("memory_byte".into(), serde_json::json!(1));
        assert!(serde_json::from_value::<ResourceEnvelope>(value).is_err());
    }

    /// A present-but-zero bound is not a bound.
    #[test]
    fn zero_bounds_are_rejected_by_name() {
        let mut value: serde_json::Value = serde_json::from_str(COMPLETE).unwrap();
        value["switch_count"] = serde_json::json!(0);
        let broken: ResourceEnvelope = serde_json::from_value(value).unwrap();
        assert_eq!(
            broken.validate(),
            Err(EnvelopeError::ZeroBound {
                field: "switch_count"
            })
        );
        assert_eq!(envelope().validate(), Ok(()));
    }

    /// C00-02-A2: the envelope's token ceiling is enforced by the existing ledger, so a request
    /// one token over is refused and one at the bound is admitted.
    #[test]
    fn token_ceiling_is_enforced_by_the_ledger_not_a_copy() {
        let env = envelope();
        let budget = env.token_budget();
        assert_eq!(budget.limit(), Some(env.authorized_token_limit));
        assert!(
            budget.try_reserve(env.authorized_token_limit + 1).is_none(),
            "one token over the envelope must be refused"
        );
        let reservation = budget
            .try_reserve(env.authorized_token_limit)
            .expect("the full bound is admissible");
        reservation.settle_unknown();
        assert!(
            budget.try_reserve(1).is_none(),
            "the bound must not be reusable after settlement"
        );
    }

    /// C00-02-A3: an unknown settlement stays unknown; it is never recorded as free.
    #[test]
    fn unknown_usage_is_not_zeroed() {
        let env = envelope();
        let budget = env.token_budget();
        let reservation = budget.try_reserve(1000).unwrap();
        let record = reservation.settle_unknown();
        assert!(matches!(
            record.settlement,
            Settlement::PossiblyDispatchedUnknown
        ));
        let snapshot = budget.accounting_snapshot();
        assert_eq!(
            snapshot.unknown_tokens, 1000,
            "an unknown settlement must stay visible as unknown"
        );
        assert_eq!(snapshot.spent, 1000, "the liability is not released");
        assert_eq!(snapshot.reserved, 0);
    }

    /// The envelope projects onto the existing configuration instead of a parallel one.
    #[test]
    fn apply_to_writes_the_kernel_fields() {
        let env = envelope();
        let mut config = BudgetConfig::default();
        env.apply_to(&mut config);
        assert_eq!(config.max_total_tokens, Some(env.authorized_token_limit));
        assert_eq!(config.max_run_duration, Duration::from_millis(900_000));
        assert_eq!(config.max_total_tool_output_bytes, 4_194_304);
        assert_eq!(env.activation_timeout(), Duration::from_millis(30_000));
        assert_eq!(env.cancellation_grace(), Duration::from_millis(5_000));
    }
}
