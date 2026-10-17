//! B20: a bounded experience store with an explicit trust boundary.
//!
//! The mechanism under test is whether carried-over experience improves later runs. What this module
//! provides is the part that must hold *before* any such experiment is worth running: experience
//! taken from a repository is untrusted input, so it can never promote itself into an instruction
//! that later runs obey.
//!
//! Layering is explicit:
//!  * [`ExperienceScope::Task`] — what worked for one task; the narrowest reach.
//!  * [`ExperienceScope::Repo`] — knowledge about one repository.
//!  * [`ExperienceScope::Global`] — operator-level guidance, reachable only through a promotion that
//!    requires a trusted actor and real evidence.
//!
//! The rules that make it safe to store repo-derived text at all:
//!  * a proposal is always a `Candidate`; a `Candidate` is never retrieved for use,
//!  * only a [`TrustedActor`] can accept a record, and acceptance requires non-empty evidence,
//!  * promotion to `Global` additionally requires the record's text to have come from a trusted
//!    source — repository content can be repo knowledge at best,
//!  * revocation is sticky for the same dedup key, so revoking is not undone by re-proposing,
//!  * expired records are not retrieved, and version invalidation revokes in bulk.
//!
//! Bounded like every other store in this codebase: caps on records, on field lengths and on
//! retrieval.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Hard caps, so a repository cannot grow the store without bound.
pub const MAX_EXPERIENCE_RECORDS: usize = 512;
pub const MAX_FIELD_LEN: usize = 1024;
/// Default retrieval bound: "minimal retrieval" means a caller asks for a few records, not all.
pub const DEFAULT_RETRIEVAL_LIMIT: usize = 8;

/// How far a record is allowed to reach.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExperienceScope {
    Task,
    Repo,
    Global,
}

/// Where a record's text came from. Repository content is untrusted by construction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum ExperienceSource {
    /// The operator (CLI flag, trusted global config, or a human review step).
    Operator,
    /// Text read out of the workspace under work — possibly attacker-controlled.
    Repository { path: String },
    /// A model's own guess during a run. It is a hypothesis, not evidence: it may inform the run it
    /// came from, but it can never become repository or global knowledge on its own.
    Model { run_id: String },
}

impl ExperienceSource {
    /// `true` when this source may reach `scope` without an operator promoting it.
    fn may_reach(&self, scope: ExperienceScope) -> bool {
        match self {
            ExperienceSource::Operator => true,
            // A model's guess is confined to the run that produced it.
            ExperienceSource::Model { .. } => scope == ExperienceScope::Task,
            ExperienceSource::Repository { .. } => scope <= ExperienceScope::Repo,
        }
    }

    fn label(&self) -> String {
        match self {
            ExperienceSource::Operator => "operator".to_string(),
            ExperienceSource::Repository { path } => format!("repository:{}", path),
            ExperienceSource::Model { run_id } => format!("model:{}", run_id),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExperienceStatus {
    Candidate,
    Accepted,
    Revoked,
}

/// One premise → action → evidence triple, with the counterexample and version that make it
/// auditable rather than a model's claim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExperienceRecord {
    pub id: String,
    /// Dedup key: records with the same key describe the same premise/action pair.
    pub key: String,
    pub scope: ExperienceScope,
    pub premise: String,
    pub action: String,
    pub evidence: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub counterexample: Option<String>,
    /// Version of the thing this describes (code revision, tool schema, ...). Bumping the version
    /// invalidates the record instead of silently keeping a stale lesson.
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at_ms: Option<u64>,
    pub status: ExperienceStatus,
    pub source: ExperienceSource,
    /// Trusted actor that accepted it, when it was accepted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accepted_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revoked_reason: Option<String>,
}

/// Only a named trusted actor can accept or promote. The type exists so acceptance cannot happen
/// accidentally from a code path that only has data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustedActor(String);

impl TrustedActor {
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    pub fn name(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExperienceError {
    Capacity,
    InvalidActor,
    UnknownRecord(String),
    /// The record's text came from the repository, so it cannot reach this scope.
    UntrustedSource { id: String, source: String },
    /// Acceptance without evidence is a claim, not a lesson.
    MissingEvidence(String),
    /// A revoked record is not re-accepted; revoking must mean something.
    Revoked(String),
    /// Accepting or promoting to a narrower/equal scope makes no sense.
    ScopeNotWider { id: String },
}

impl std::fmt::Display for ExperienceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExperienceError::Capacity => write!(f, "experience store is full"),
            ExperienceError::InvalidActor => write!(f, "experience acceptance requires a nonblank trusted actor"),
            ExperienceError::UnknownRecord(id) => write!(f, "unknown experience record '{}'", id),
            ExperienceError::UntrustedSource { id, source } => write!(
                f,
                "experience '{}' comes from {} and cannot be promoted beyond repository scope",
                id, source
            ),
            ExperienceError::MissingEvidence(id) => {
                write!(f, "experience '{}' has no evidence to accept", id)
            }
            ExperienceError::Revoked(id) => {
                write!(f, "experience '{}' was revoked and cannot be re-accepted", id)
            }
            ExperienceError::ScopeNotWider { id } => {
                write!(f, "experience '{}' is already at that scope", id)
            }
        }
    }
}

impl std::error::Error for ExperienceError {}

/// A bounded, deterministic store. `BTreeMap` keeps retrieval order stable so two runs with the same
/// records retrieve the same list.
#[derive(Debug, Clone, Default)]
pub struct ExperienceStore {
    records: BTreeMap<String, ExperienceRecord>,
    /// Keys that were ever revoked, so re-proposing the same lesson cannot un-revoke it.
    revoked_keys: BTreeMap<String, String>,
}

impl ExperienceStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    pub fn get(&self, id: &str) -> Option<&ExperienceRecord> {
        self.records.get(id)
    }

    /// Propose a lesson. Always lands as a `Candidate` — never directly usable.
    ///
    /// Proposing the same `key` again replaces the stored candidate's text (a later observation about
    /// the same premise wins) but cannot change its status: a revoked key stays revoked, and an
    /// accepted record is not silently rewritten.
    // One parameter per field of the record being proposed; bundling them into a struct would only
    // move the same list one level down (matching the precedent in `task_host::link_merge_in_session`).
    #[allow(clippy::too_many_arguments)]
    pub fn propose(
        &mut self,
        key: impl Into<String>,
        scope: ExperienceScope,
        premise: impl Into<String>,
        action: impl Into<String>,
        evidence: impl Into<String>,
        version: impl Into<String>,
        source: ExperienceSource,
    ) -> Result<String, ExperienceError> {
        let key = truncate(key.into(), MAX_FIELD_LEN);
        let id = format!("{}:{}", scope_slug(scope), key);
        if self.revoked_keys.contains_key(&key) {
            // Record it as revoked so the history is visible, but do not resurrect it.
            if let Some(existing) = self.records.get_mut(&id) {
                existing.status = ExperienceStatus::Revoked;
                existing.revoked_reason = Some("key revoked previously".to_string());
                return Ok(id);
            }
        }
        if self.records.get(&id).is_some_and(|record| record.status == ExperienceStatus::Accepted) {
            return Ok(id); // A proposal has no authority to replace accepted bytes.
        }
        if !self.records.contains_key(&id) && self.records.len() >= MAX_EXPERIENCE_RECORDS {
            return Err(ExperienceError::Capacity);
        }
        let record = ExperienceRecord {
            id: id.clone(),
            key: key.clone(),
            scope,
            premise: truncate(premise.into(), MAX_FIELD_LEN),
            action: truncate(action.into(), MAX_FIELD_LEN),
            evidence: truncate(evidence.into(), MAX_FIELD_LEN),
            counterexample: None,
            version: truncate(version.into(), MAX_FIELD_LEN),
            expires_at_ms: None,
            status: if self.revoked_keys.contains_key(&key) { ExperienceStatus::Revoked } else { ExperienceStatus::Candidate },
            source,
            accepted_by: None,
            revoked_reason: None,
        };
        self.records.insert(id.clone(), record);
        Ok(id)
    }

    pub fn with_counterexample(
        &mut self,
        id: &str,
        counterexample: impl Into<String>,
        expires_at_ms: Option<u64>,
    ) -> Result<(), ExperienceError> {
        let record = self
            .records
            .get_mut(id)
            .ok_or_else(|| ExperienceError::UnknownRecord(id.to_string()))?;
        record.counterexample = Some(truncate(counterexample.into(), MAX_FIELD_LEN));
        record.expires_at_ms = expires_at_ms;
        Ok(())
    }

    /// Accept a candidate. Requires a trusted actor and real evidence.
    pub fn accept(&mut self, id: &str, actor: &TrustedActor) -> Result<(), ExperienceError> {
        if actor.name().trim().is_empty() { return Err(ExperienceError::InvalidActor); }
        let record = self
            .records
            .get_mut(id)
            .ok_or_else(|| ExperienceError::UnknownRecord(id.to_string()))?;
        if record.status == ExperienceStatus::Revoked {
            return Err(ExperienceError::Revoked(id.to_string()));
        }
        if record.evidence.trim().is_empty() {
            return Err(ExperienceError::MissingEvidence(id.to_string()));
        }
        if !record.source.may_reach(record.scope) {
            return Err(ExperienceError::UntrustedSource {
                id: id.to_string(),
                source: record.source.label(),
            });
        }
        record.status = ExperienceStatus::Accepted;
        record.accepted_by = Some(actor.name().to_string());
        Ok(())
    }

    /// Widen a record's reach. Repository-sourced text stops at repository scope, and the actor must
    /// be trusted: this is the step that would otherwise let a repository write global instructions.
    pub fn promote(
        &mut self,
        id: &str,
        to: ExperienceScope,
        actor: &TrustedActor,
    ) -> Result<(), ExperienceError> {
        if actor.name().trim().is_empty() { return Err(ExperienceError::InvalidActor); }
        let record = self
            .records
            .get_mut(id)
            .ok_or_else(|| ExperienceError::UnknownRecord(id.to_string()))?;
        if record.status == ExperienceStatus::Revoked {
            return Err(ExperienceError::Revoked(id.to_string()));
        }
        if to <= record.scope {
            return Err(ExperienceError::ScopeNotWider { id: id.to_string() });
        }
        // Widening is what turns a local guess or a repository note into guidance for other work, so
        // the source has to be allowed to reach the wider scope at all.
        if !record.source.may_reach(to) {
            return Err(ExperienceError::UntrustedSource {
                id: id.to_string(),
                source: record.source.label(),
            });
        }
        if record.evidence.trim().is_empty() {
            return Err(ExperienceError::MissingEvidence(id.to_string()));
        }
        record.scope = to;
        record.accepted_by = Some(actor.name().to_string());
        Ok(())
    }

    /// Revoke a lesson. Sticky for the dedup key.
    pub fn revoke(&mut self, id: &str, reason: impl Into<String>) -> Result<(), ExperienceError> {
        let record = self
            .records
            .get_mut(id)
            .ok_or_else(|| ExperienceError::UnknownRecord(id.to_string()))?;
        record.status = ExperienceStatus::Revoked;
        record.revoked_reason = Some(truncate(reason.into(), MAX_FIELD_LEN));
        self.revoked_keys
            .insert(record.key.clone(), record.id.clone());
        Ok(())
    }

    /// Revoke everything recorded against a version that no longer exists.
    pub fn invalidate_version(&mut self, version: &str) -> usize {
        let mut revoked = 0;
        for record in self.records.values_mut() {
            if record.version == version && record.status != ExperienceStatus::Revoked {
                record.status = ExperienceStatus::Revoked;
                record.revoked_reason = Some(format!("version {} is stale", version));
                revoked += 1;
            }
        }
        revoked
    }

    /// Retrieve the few records a caller may actually use: accepted, unexpired, within `scope` (or
    /// narrower), in a stable order. Candidates are never returned — that is what makes storing
    /// repo-derived text safe.
    pub fn retrieve(
        &self,
        scope: ExperienceScope,
        limit: usize,
        now_ms: u64,
    ) -> Vec<&ExperienceRecord> {
        let limit = limit.min(MAX_EXPERIENCE_RECORDS);
        self.records
            .values()
            .filter(|record| record.status == ExperienceStatus::Accepted)
            .filter(|record| record.scope <= scope)
            .filter(|record| record.expires_at_ms.is_none_or(|deadline| deadline > now_ms))
            .take(limit)
            .collect()
    }
}

/// Retrieved experience rendered for injection into a prompt.
///
/// B20 requires that carrying experience over must not rewrite the frozen cache prefix: the block is
/// therefore *appended* to whatever the caller already has, and the caller's prefix stays a byte
/// prefix of the result (see [`append_after_prefix`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExperienceInjection {
    pub block: String,
    pub record_ids: Vec<String>,
    /// `true` when the block was cut at the byte cap.
    pub truncated: bool,
}

/// Cap on the injected block, so a store full of lessons cannot displace the task itself.
pub const MAX_INJECTION_BYTES: usize = 2 * 1024;

impl ExperienceInjection {
    /// Render at most `limit` accepted, unexpired records into a bounded block.
    pub fn render(
        store: &ExperienceStore,
        scope: ExperienceScope,
        limit: usize,
        now_ms: u64,
    ) -> Self {
        let mut block = String::new();
        let mut record_ids = Vec::new();
        let mut truncated = false;
        for record in store.retrieve(scope, limit, now_ms) {
            let line = format!(
                "- when {}: {} (evidence: {})\n",
                record.premise, record.action, record.evidence
            );
            if block.len() + line.len() > MAX_INJECTION_BYTES {
                truncated = true;
                break;
            }
            block.push_str(&line);
            record_ids.push(record.id.clone());
        }
        Self {
            block,
            record_ids,
            truncated,
        }
    }
}

/// Append an injection block after the caller's static prefix.
///
/// The guard is explicit: the prefix must remain a byte-for-byte prefix of the result, so whatever
/// the provider caches (prompt hash, tool-schema hash) still describes the same bytes.
pub fn append_after_prefix(prefix: &str, injection: &ExperienceInjection) -> String {
    let mut out = String::with_capacity(prefix.len() + injection.block.len());
    out.push_str(prefix);
    out.push_str(&injection.block);
    debug_assert!(out.starts_with(prefix));
    out
}

fn scope_slug(scope: ExperienceScope) -> &'static str {
    match scope {
        ExperienceScope::Task => "task",
        ExperienceScope::Repo => "repo",
        ExperienceScope::Global => "global",
    }
}

fn truncate(value: String, max: usize) -> String {
    if value.chars().count() <= max {
        return value;
    }
    value.chars().take(max).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reproposing_an_accepted_key_cannot_rewrite_its_guidance() {
        let mut store=ExperienceStore::new();
        let id=store.propose("same",ExperienceScope::Repo,"p","accepted-action","e","v1",ExperienceSource::Operator).unwrap();
        store.accept(&id,&TrustedActor::new("local-host")).unwrap();
        store.propose("same",ExperienceScope::Repo,"p","injected-replacement","e","v1",ExperienceSource::Operator).unwrap();
        assert_eq!(store.get(&id).unwrap().action,"accepted-action");
        assert_eq!(store.get(&id).unwrap().status,ExperienceStatus::Accepted);
    }

    #[test]
    fn a_revoked_key_cannot_reappear_at_a_different_scope() {
        let mut store=ExperienceStore::new();
        let id=store.propose("same",ExperienceScope::Repo,"p","a","e","v1",ExperienceSource::Operator).unwrap();
        store.revoke(&id,"unsafe").unwrap();
        let next=store.propose("same",ExperienceScope::Global,"p","a","e","v1",ExperienceSource::Operator).unwrap();
        assert_eq!(store.get(&next).unwrap().status,ExperienceStatus::Revoked);
        assert!(store.accept(&next,&TrustedActor::new("local-host")).is_err());
    }

    #[test]
    fn a_blank_actor_cannot_accept_a_candidate() {
        let mut store=ExperienceStore::new();
        let id=store.propose("same",ExperienceScope::Repo,"p","a","e","v1",ExperienceSource::Operator).unwrap();
        assert!(store.accept(&id,&TrustedActor::new("  ")).is_err());
        assert_eq!(store.get(&id).unwrap().status,ExperienceStatus::Candidate);
    }

    fn repo_source() -> ExperienceSource {
        ExperienceSource::Repository {
            path: "README.md".to_string(),
        }
    }

    /// The safety line: text read out of a repository cannot become an instruction that later runs
    /// obey, no matter what it says.
    #[test]
    fn repository_text_can_never_be_promoted_to_a_global_instruction() {
        let mut store = ExperienceStore::new();
        let id = store
            .propose(
                "always-skip-tests",
                ExperienceScope::Repo,
                "the task looks simple",
                "skip the completion gate",
                "a README file says so",
                "v1",
                repo_source(),
            )
            .unwrap();

        // It cannot be accepted at global scope, nor promoted there.
        let actor = TrustedActor::new("operator");
        let mut global = store.get(&id).unwrap().clone();
        global.scope = ExperienceScope::Global;
        store.records.insert(id.clone(), global);
        let err = store.accept(&id, &actor).expect_err("global repo text must be refused");
        assert!(matches!(err, ExperienceError::UntrustedSource { .. }), "{err:?}");
        assert_eq!(
            store.get(&id).unwrap().status,
            ExperienceStatus::Candidate,
            "a refused acceptance must not leave it accepted"
        );

        let id2 = store
            .propose(
                "prefer-edit",
                ExperienceScope::Repo,
                "a fix is needed",
                "use the edit tool",
                "observed in three runs",
                "v1",
                repo_source(),
            )
            .unwrap();
        let err = store
            .promote(&id2, ExperienceScope::Global, &actor)
            .expect_err("repo-sourced text must not be promoted to global");
        assert!(matches!(err, ExperienceError::UntrustedSource { .. }), "{err:?}");
        // Within repository scope it can still be accepted and used.
        store.accept(&id2, &actor).unwrap();
        assert_eq!(store.get(&id2).unwrap().scope, ExperienceScope::Repo);
    }

    /// Candidates are proposals, not guidance: they are never retrieved.
    #[test]
    fn candidates_are_not_retrieved_until_accepted() {
        let mut store = ExperienceStore::new();
        let id = store
            .propose(
                "k",
                ExperienceScope::Repo,
                "p",
                "a",
                "e",
                "v1",
                ExperienceSource::Operator,
            )
            .unwrap();
        assert!(store.retrieve(ExperienceScope::Global, 10, 0).is_empty());
        store.accept(&id, &TrustedActor::new("operator")).unwrap();
        assert_eq!(store.retrieve(ExperienceScope::Global, 10, 0).len(), 1);
    }

    /// Evidence is required: acceptance without it is a model's claim, not a lesson.
    #[test]
    fn acceptance_requires_evidence() {
        let mut store = ExperienceStore::new();
        let id = store
            .propose(
                "k",
                ExperienceScope::Repo,
                "p",
                "a",
                "   ",
                "v1",
                ExperienceSource::Operator,
            )
            .unwrap();
        let err = store
            .accept(&id, &TrustedActor::new("operator"))
            .expect_err("blank evidence must be refused");
        assert!(matches!(err, ExperienceError::MissingEvidence(_)), "{err:?}");
    }

    /// Revocation sticks: re-proposing the same lesson does not bring it back.
    #[test]
    fn revocation_is_sticky_for_the_same_key() {
        let mut store = ExperienceStore::new();
        let id = store
            .propose(
                "k",
                ExperienceScope::Repo,
                "p",
                "a",
                "e",
                "v1",
                ExperienceSource::Operator,
            )
            .unwrap();
        store.accept(&id, &TrustedActor::new("operator")).unwrap();
        store.revoke(&id, "wrong after all").unwrap();
        assert!(store.retrieve(ExperienceScope::Global, 10, 0).is_empty());

        // Re-proposing the same key must not resurrect it.
        let again = store
            .propose(
                "k",
                ExperienceScope::Repo,
                "p",
                "a",
                "e",
                "v1",
                ExperienceSource::Operator,
            )
            .unwrap();
        assert_eq!(again, id);
        assert_eq!(
            store.get(&id).unwrap().status,
            ExperienceStatus::Revoked,
            "re-proposing must not clear a revocation"
        );
        assert!(matches!(
            store.accept(&id, &TrustedActor::new("operator")),
            Err(ExperienceError::Revoked(_))
        ));
    }

    /// Expiry and version invalidation both remove a lesson from use.
    #[test]
    fn expired_and_stale_records_are_not_retrieved() {
        let mut store = ExperienceStore::new();
        let fresh = store
            .propose(
                "fresh",
                ExperienceScope::Repo,
                "p",
                "a",
                "e",
                "v1",
                ExperienceSource::Operator,
            )
            .unwrap();
        let short = store
            .propose(
                "short",
                ExperienceScope::Repo,
                "p",
                "a",
                "e",
                "v1",
                ExperienceSource::Operator,
            )
            .unwrap();
        let actor = TrustedActor::new("operator");
        store.accept(&fresh, &actor).unwrap();
        store.accept(&short, &actor).unwrap();
        store.with_counterexample(&short, "only true before v2", Some(1_000)).unwrap();

        assert_eq!(store.retrieve(ExperienceScope::Global, 10, 500).len(), 2);
        assert_eq!(
            store.retrieve(ExperienceScope::Global, 10, 2_000).len(),
            1,
            "an expired record must not be retrieved"
        );

        assert_eq!(store.invalidate_version("v1"), 2);
        assert!(store.retrieve(ExperienceScope::Global, 10, 500).is_empty());
    }

    /// Same key, one record; and retrieval is bounded and in a stable order.
    #[test]
    fn duplicate_keys_deduplicate_and_retrieval_is_bounded() {
        let mut store = ExperienceStore::new();
        let first = store
            .propose(
                "same",
                ExperienceScope::Repo,
                "p1",
                "a1",
                "e1",
                "v1",
                ExperienceSource::Operator,
            )
            .unwrap();
        let second = store
            .propose(
                "same",
                ExperienceScope::Repo,
                "p2",
                "a2",
                "e2",
                "v1",
                ExperienceSource::Operator,
            )
            .unwrap();
        assert_eq!(first, second, "the same key must reuse the same record");
        assert_eq!(store.len(), 1);
        assert_eq!(store.get(&first).unwrap().premise, "p2", "the later text wins");

        for index in 0..20 {
            let id = store
                .propose(
                    format!("k{index}"),
                    ExperienceScope::Repo,
                    "p",
                    "a",
                    "e",
                    "v1",
                    ExperienceSource::Operator,
                )
                .unwrap();
            store.accept(&id, &TrustedActor::new("operator")).unwrap();
        }
        let retrieved = store.retrieve(ExperienceScope::Global, 5, 0);
        assert_eq!(retrieved.len(), 5, "retrieval is bounded by the caller's limit");
        let again = store.retrieve(ExperienceScope::Global, 5, 0);
        assert_eq!(
            retrieved.iter().map(|r| r.id.clone()).collect::<Vec<_>>(),
            again.iter().map(|r| r.id.clone()).collect::<Vec<_>>(),
            "retrieval order must be stable across calls"
        );
    }

    /// A model's own guess is a hypothesis, not a fact: it may inform the run that produced it and
    /// nothing wider, even when the operator is willing to accept it.
    #[test]
    fn a_model_guess_cannot_become_repository_or_global_knowledge() {
        let mut store = ExperienceStore::new();
        let actor = TrustedActor::new("operator");
        let guess = || ExperienceSource::Model {
            run_id: "run-1".to_string(),
        };

        // At repository scope the guess is refused outright.
        let repo = store
            .propose(
                "guess-repo",
                ExperienceScope::Repo,
                "the failure looks like a cache issue",
                "clear the cache",
                "the model said so",
                "v1",
                guess(),
            )
            .unwrap();
        let err = store
            .accept(&repo, &actor)
            .expect_err("a model guess must not become repository knowledge");
        assert!(matches!(err, ExperienceError::UntrustedSource { .. }), "{err:?}");

        // Within the run it came from, it may be accepted and used.
        let task = store
            .propose(
                "guess-task",
                ExperienceScope::Task,
                "the failure looks like a cache issue",
                "clear the cache",
                "observed once in this run",
                "v1",
                guess(),
            )
            .unwrap();
        store.accept(&task, &actor).unwrap();
        assert_eq!(store.retrieve(ExperienceScope::Task, 10, 0).len(), 1);
        assert_eq!(
            store.get(&task).unwrap().scope,
            ExperienceScope::Task,
            "the guess must stay confined to the run that produced it"
        );

        // And it cannot be promoted out of that run.
        let err = store
            .promote(&task, ExperienceScope::Repo, &actor)
            .expect_err("a model guess must not be promoted");
        assert!(matches!(err, ExperienceError::UntrustedSource { .. }), "{err:?}");
    }

    /// Carrying experience over must not rewrite the frozen prefix: the block is appended, and the
    /// caller's prefix stays a byte-for-byte prefix of the result. The block is bounded, and a
    /// candidate (not accepted) is never injected.
    #[test]
    fn injection_is_bounded_and_leaves_the_cache_prefix_untouched() {
        let mut store = ExperienceStore::new();
        let actor = TrustedActor::new("operator");
        for index in 0..40 {
            let id = store
                .propose(
                    format!("k{index}"),
                    ExperienceScope::Repo,
                    format!("premise {index} {}", "x".repeat(80)),
                    format!("action {index}"),
                    format!("evidence {index}"),
                    "v1",
                    ExperienceSource::Operator,
                )
                .unwrap();
            store.accept(&id, &actor).unwrap();
        }
        // A candidate must not appear in the injected text.
        let pending = store
            .propose(
                "pending",
                ExperienceScope::Repo,
                "UNACCEPTED-PREMISE",
                "action",
                "evidence",
                "v1",
                ExperienceSource::Operator,
            )
            .unwrap();

        let injection = ExperienceInjection::render(&store, ExperienceScope::Repo, 50, 0);
        assert!(
            injection.block.len() <= MAX_INJECTION_BYTES,
            "the injected block must stay bounded, got {}",
            injection.block.len()
        );
        assert!(injection.truncated, "the cap must be reported when it bites");
        assert!(!injection.record_ids.contains(&pending));
        assert!(!injection.block.contains("UNACCEPTED-PREMISE"));

        // The cache prefix is untouched: same bytes, same hash.
        let prefix = "SYSTEM PROMPT\n\nfrozen prefix bytes";
        let before = {
            use sha2::{Digest, Sha256};
            let mut hasher = Sha256::new();
            hasher.update(prefix.as_bytes());
            format!("{:x}", hasher.finalize())
        };
        let assembled = append_after_prefix(prefix, &injection);
        assert!(assembled.starts_with(prefix), "the prefix must remain a byte prefix");
        let after = {
            use sha2::{Digest, Sha256};
            let mut hasher = Sha256::new();
            hasher.update(&assembled.as_bytes()[..prefix.len()]);
            format!("{:x}", hasher.finalize())
        };
        assert_eq!(before, after, "injection changed the cached prefix");
    }

    /// Scope narrows what is reachable: task-scope lessons do not leak into repo-wide retrieval.
    #[test]
    fn retrieval_respects_scope() {
        let mut store = ExperienceStore::new();
        let actor = TrustedActor::new("operator");
        let task = store
            .propose(
                "t",
                ExperienceScope::Task,
                "p",
                "a",
                "e",
                "v1",
                ExperienceSource::Operator,
            )
            .unwrap();
        let repo = store
            .propose(
                "r",
                ExperienceScope::Repo,
                "p",
                "a",
                "e",
                "v1",
                ExperienceSource::Operator,
            )
            .unwrap();
        store.accept(&task, &actor).unwrap();
        store.accept(&repo, &actor).unwrap();

        assert_eq!(store.retrieve(ExperienceScope::Task, 10, 0).len(), 1);
        assert_eq!(store.retrieve(ExperienceScope::Repo, 10, 0).len(), 2);
    }
}
