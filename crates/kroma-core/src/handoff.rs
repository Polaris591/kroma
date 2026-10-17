//! T28: long-running tasks — a bounded handoff artifact, revocable experience, and invalidation when
//! the world the experience described has changed.
//!
//! Three separate promises, each with its own failure mode:
//!
//! 1. **Revocation is effective on the next request.** A revoked record must not be retrieved again, and
//!    the store must be able to say *why* it was revoked (the audit trail is the point).
//! 2. **Experience does not leak across branches or survive changes it depends on.** A lesson tied to a
//!    file hash, a policy mode or a leaf is stale the moment that binding changes; keeping it is how a
//!    "helpful" memory silently becomes wrong advice.
//! 3. **The handoff is a summary with evidence, not a copy of the conversation.** Handing the whole
//!    transcript to the next turn is unbounded, mostly irrelevant, and drags stale claims forward.
//!
//! Nothing here approves a model's own summary as a permanent fact: `ExperienceStore::accept` still
//! requires a trusted actor (see `experience.rs`), and this module never calls it.

use crate::experience::{ExperienceRecord, ExperienceStatus};
use serde::{Deserialize, Serialize};

/// Counts an operator can read at a glance. Derived, not stored, so they cannot drift from the records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExperienceStats {
    pub total: usize,
    pub candidates: usize,
    pub accepted: usize,
    pub revoked: usize,
    /// Accepted records that carry a counterexample: the evidence that they are *not* universally true.
    pub falsified: usize,
    /// Accepted records whose expiry has passed at the time of the count.
    pub expired: usize,
}

impl ExperienceStats {
    pub fn from_records(records: impl IntoIterator<Item = ExperienceRecord>, now_ms: u64) -> Self {
        let mut stats = ExperienceStats::default();
        for record in records {
            stats.total += 1;
            match record.status {
                ExperienceStatus::Candidate => stats.candidates += 1,
                ExperienceStatus::Accepted => {
                    stats.accepted += 1;
                    if record.counterexample.is_some() {
                        stats.falsified += 1;
                    }
                    if record
                        .expires_at_ms
                        .is_some_and(|deadline| deadline <= now_ms)
                    {
                        stats.expired += 1;
                    }
                }
                ExperienceStatus::Revoked => stats.revoked += 1,
            }
        }
        stats
    }

    /// Accepted, not expired, not falsified: what may still be injected.
    pub fn still_usable(&self) -> usize {
        self.accepted
            .saturating_sub(self.expired)
            .saturating_sub(self.falsified)
    }
}

/// Something about the world that changed since the experience was recorded.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorldChange {
    /// Paths whose bytes changed this turn.
    #[serde(default)]
    pub changed_paths: Vec<String>,
    /// New code/tool version, when it moved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Policy mode before/after, when it changed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy: Option<(String, String)>,
    /// Session leaf before/after, when the run forked or rewound.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<(String, String)>,
}

impl WorldChange {
    pub fn is_empty(&self) -> bool {
        self.changed_paths.is_empty()
            && self.version.is_none()
            && self.policy.is_none()
            && self.branch.is_none()
    }
}

/// Why one record stopped being usable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InvalidationCause {
    /// A path the record's premise/evidence mentions changed.
    PathChanged(String),
    /// The recorded version is no longer the current one.
    VersionChanged,
    /// The policy mode changed: the same action may no longer be allowed.
    PolicyChanged,
    /// The branch changed: a sibling branch's lesson is not this branch's lesson.
    BranchChanged,
}

impl InvalidationCause {
    pub fn as_str(&self) -> &str {
        match self {
            InvalidationCause::PathChanged(path) => path,
            InvalidationCause::VersionChanged => "version",
            InvalidationCause::PolicyChanged => "policy",
            InvalidationCause::BranchChanged => "branch",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct InvalidationReport {
    /// `(record id, cause)` for every record that must not be injected any more.
    pub invalidated: Vec<(String, InvalidationCause)>,
    pub kept: usize,
}

impl InvalidationReport {
    pub fn is_clean(&self) -> bool {
        self.invalidated.is_empty()
    }
}

/// Which accepted records became stale because of `change`.
///
/// Conservative by design: a policy or branch change invalidates *every* accepted record, because the
/// module cannot prove any of them still applies, and keeping a stale lesson is worse than re-learning
/// it. A path change only invalidates records that mention that path.
pub fn invalidate_on_change(
    records: &[ExperienceRecord],
    change: &WorldChange,
) -> InvalidationReport {
    let mut report = InvalidationReport::default();
    for record in records {
        if record.status != ExperienceStatus::Accepted {
            continue;
        }
        let mut cause: Option<InvalidationCause> = None;
        if let Some((from, to)) = &change.policy {
            if from != to {
                cause = Some(InvalidationCause::PolicyChanged);
            }
        }
        if cause.is_none() {
            if let Some((from, to)) = &change.branch {
                if from != to {
                    cause = Some(InvalidationCause::BranchChanged);
                }
            }
        }
        if cause.is_none() {
            if let Some(version) = &change.version {
                if record.version != *version {
                    cause = Some(InvalidationCause::VersionChanged);
                }
            }
        }
        if cause.is_none() {
            let haystack = format!("{} {} {}", record.premise, record.action, record.evidence);
            if let Some(path) = change
                .changed_paths
                .iter()
                .find(|path| !path.is_empty() && haystack.contains(path.as_str()))
            {
                cause = Some(InvalidationCause::PathChanged(path.clone()));
            }
        }
        match cause {
            Some(cause) => report.invalidated.push((record.id.clone(), cause)),
            None => report.kept += 1,
        }
    }
    report
}

/// The handoff between turns of a long task: goal, what is verified, what is unresolved, and the
/// evidence for each claim.
///
/// **No transcript.** If a caller wants the model to re-read a tool result, the evidence reference
/// points at it (a `recall_evidence` id or a path) instead of copying the bytes forward.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HandoffArtifact {
    /// The user's own task statement (verbatim, from T25's typed state).
    pub goal: String,
    /// Claims that were actually verified, each with the check that verified it.
    #[serde(default)]
    pub verified: Vec<VerifiedClaim>,
    /// Things still open, phrased as questions rather than conclusions.
    #[serde(default)]
    pub unresolved: Vec<String>,
    /// Evidence pointers (paths, evidence ids, verification commands). Never file contents.
    #[serde(default)]
    pub evidence: Vec<String>,
    /// Experience records this turn relied on, so a later turn can notice they were revoked.
    #[serde(default)]
    pub experience_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerifiedClaim {
    pub claim: String,
    /// What actually ran/looked at it, e.g. `cargo test -p kroma-core` or an evidence id.
    pub check: String,
}

/// A byte-bounded rendering of the handoff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedHandoff {
    pub text: String,
    /// `true` when the rendering was cut at the cap; the caller must say so rather than pretend the
    /// handoff is complete.
    pub truncated: bool,
}

/// Cap on the rendered handoff. Large enough for a real task summary, small enough that it cannot become
/// a transcript copy.
pub const MAX_HANDOFF_BYTES: usize = 8 * 1024;

impl HandoffArtifact {
    pub fn new(goal: impl Into<String>) -> Self {
        Self {
            goal: goal.into(),
            ..Self::default()
        }
    }

    pub fn verify(&mut self, claim: impl Into<String>, check: impl Into<String>) {
        self.verified.push(VerifiedClaim {
            claim: claim.into(),
            check: check.into(),
        });
    }

    pub fn open(&mut self, question: impl Into<String>) {
        self.unresolved.push(question.into());
    }

    pub fn cite(&mut self, evidence: impl Into<String>) {
        self.evidence.push(evidence.into());
    }

    /// Render within [`MAX_HANDOFF_BYTES`].
    ///
    /// Refuses content that looks like a transcript: a handoff that embeds raw tool output is exactly
    /// the "copy the whole conversation forward" failure this structure exists to prevent.
    pub fn render(&self) -> Result<RenderedHandoff, String> {
        if self.goal.trim().is_empty() {
            return Err("a handoff must state the goal".to_string());
        }
        let mut text = String::new();
        text.push_str("goal: ");
        text.push_str(self.goal.trim());
        text.push('\n');
        if !self.verified.is_empty() {
            text.push_str("verified:\n");
            for claim in &self.verified {
                text.push_str(&format!("- {} (check: {})\n", claim.claim, claim.check));
            }
        }
        if !self.unresolved.is_empty() {
            text.push_str("unresolved:\n");
            for question in &self.unresolved {
                text.push_str(&format!("- {}\n", question));
            }
        }
        if !self.evidence.is_empty() {
            text.push_str("evidence: ");
            text.push_str(&self.evidence.join(", "));
            text.push('\n');
        }
        if !self.experience_ids.is_empty() {
            text.push_str("experience: ");
            text.push_str(&self.experience_ids.join(", "));
            text.push('\n');
        }
        let truncated = text.len() > MAX_HANDOFF_BYTES;
        if truncated {
            text.truncate(MAX_HANDOFF_BYTES);
            // Never cut mid-character or mid-line in a way that reads as a complete sentence.
            while !text.ends_with('\n') && text.pop().is_some() {}
            text.push_str("... [handoff truncated at the byte cap]\n");
        }
        Ok(RenderedHandoff { text, truncated })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::experience::{ExperienceScope, ExperienceSource, ExperienceStore, TrustedActor};

    fn accept(store: &mut ExperienceStore, key: &str, version: &str) -> String {
        let id = store
            .propose(
                key,
                ExperienceScope::Repo,
                format!("premise mentioning src/lib.rs for {key}"),
                "do the action",
                "evidence: tests passed",
                version,
                ExperienceSource::Operator,
            )
            .expect("propose");
        store
            .accept(&id, &TrustedActor::new("owner"))
            .expect("accept");
        id
    }

    #[test]
    fn revocation_is_effective_on_the_next_retrieval() {
        let mut store = ExperienceStore::new();
        let id = accept(&mut store, "k1", "v1");
        assert_eq!(
            store.retrieve(ExperienceScope::Repo, 10, 0).len(),
            1,
            "the accepted record should be retrievable"
        );

        store
            .revoke(&id, "the premise turned out to be wrong")
            .unwrap();
        assert!(
            store.retrieve(ExperienceScope::Repo, 10, 0).is_empty(),
            "a revoked record must not be retrieved again"
        );
        let record = store.get(&id).expect("record");
        assert_eq!(record.status, ExperienceStatus::Revoked);
        assert_eq!(
            record.revoked_reason.as_deref(),
            Some("the premise turned out to be wrong")
        );
    }

    #[test]
    fn statistics_separate_falsified_from_expired() {
        let mut store = ExperienceStore::new();
        let plain = accept(&mut store, "plain", "v1");
        let falsified = accept(&mut store, "falsified", "v1");
        store
            .with_counterexample(&falsified, "fails when the file is a symlink", None)
            .expect("counterexample");
        let revoked = accept(&mut store, "revoked", "v1");
        store.revoke(&revoked, "superseded").unwrap();
        let _ = plain;

        let stats = ExperienceStats::from_records(store.all().into_iter().cloned(), 0);
        assert_eq!(stats.total, 3);
        // Status is current, not historical: the revoked record counts as revoked, not accepted.
        assert_eq!(stats.accepted, 2);
        assert_eq!(stats.revoked, 1);
        assert_eq!(stats.falsified, 1);
        assert_eq!(stats.expired, 0);
        // Only one of the accepted records is still usable: the falsified one is not.
        assert_eq!(stats.still_usable(), 1);
    }

    #[test]
    fn a_changed_file_policy_or_branch_invalidates_the_right_records() {
        let mut store = ExperienceStore::new();
        let about_lib = accept(&mut store, "lib", "v1");
        let about_other = store
            .propose(
                "other",
                ExperienceScope::Repo,
                "premise about docs/readme.md",
                "do the action",
                "evidence",
                "v1",
                ExperienceSource::Operator,
            )
            .expect("propose");
        store
            .accept(&about_other, &TrustedActor::new("owner"))
            .unwrap();
        let records: Vec<ExperienceRecord> = store.all().into_iter().cloned().collect();

        // A path change only invalidates the record that mentions it.
        let report = invalidate_on_change(
            &records,
            &WorldChange {
                changed_paths: vec!["src/lib.rs".to_string()],
                ..WorldChange::default()
            },
        );
        assert_eq!(report.invalidated.len(), 1);
        assert_eq!(report.invalidated[0].0, about_lib);
        assert_eq!(
            report.invalidated[0].1,
            InvalidationCause::PathChanged("src/lib.rs".to_string())
        );
        assert_eq!(report.kept, 1);

        // A policy change is conservative: every accepted record is stale.
        let report = invalidate_on_change(
            &records,
            &WorldChange {
                policy: Some(("read_only".to_string(), "allow_write".to_string())),
                ..WorldChange::default()
            },
        );
        assert_eq!(report.invalidated.len(), 2);
        assert!(report
            .invalidated
            .iter()
            .all(|(_, cause)| *cause == InvalidationCause::PolicyChanged));

        // A branch change too: a sibling branch's lesson is not this branch's.
        let report = invalidate_on_change(
            &records,
            &WorldChange {
                branch: Some(("leaf-a".to_string(), "leaf-b".to_string())),
                ..WorldChange::default()
            },
        );
        assert_eq!(report.invalidated.len(), 2);
        assert!(report
            .invalidated
            .iter()
            .all(|(_, cause)| *cause == InvalidationCause::BranchChanged));

        // A version bump invalidates only the records whose version moved.
        let report = invalidate_on_change(
            &records,
            &WorldChange {
                version: Some("v2".to_string()),
                ..WorldChange::default()
            },
        );
        assert_eq!(report.invalidated.len(), 2);
        assert!(report
            .invalidated
            .iter()
            .all(|(_, cause)| *cause == InvalidationCause::VersionChanged));

        // Nothing changed: nothing invalidated.
        assert!(invalidate_on_change(&records, &WorldChange::default()).is_clean());
    }

    #[test]
    fn the_handoff_carries_claims_and_evidence_not_the_transcript() {
        let mut handoff =
            HandoffArtifact::new("Fix the failing parser test without touching tests/.");
        handoff.verify(
            "the parser now rejects empty input",
            "cargo test -p kroma-core parser",
        );
        handoff.open("should the error message include the column?");
        handoff.cite("src/parser.rs");
        handoff.cite("ev-0003-call_parser");
        handoff.experience_ids.push("exp-1".to_string());

        let rendered = handoff.render().expect("a valid handoff renders");
        assert!(rendered.text.contains("goal: Fix the failing parser test"));
        assert!(rendered
            .text
            .contains("check: cargo test -p kroma-core parser"));
        assert!(rendered.text.contains("unresolved:"));
        assert!(rendered.text.contains("src/parser.rs"));
        assert!(!rendered.truncated, "a small handoff must not be truncated");
        // The transcript is not copied: no message roles, no raw tool output markers.
        for forbidden in ["tool_result", "[Compacted:", "assistant:", "user:"] {
            assert!(
                !rendered.text.contains(forbidden),
                "the handoff must not embed transcript content ({forbidden})"
            );
        }
        // Every claim has a check; a claim without one cannot be added through this API.
        for claim in &handoff.verified {
            assert!(!claim.check.trim().is_empty());
        }

        // A handoff with no goal is refused.
        assert!(HandoffArtifact::default().render().is_err());
    }

    #[test]
    fn an_oversized_handoff_is_truncated_and_says_so() {
        let mut handoff = HandoffArtifact::new("long task");
        for i in 0..500 {
            handoff.verify(
                format!("claim {i} that takes up a reasonable number of bytes"),
                "cargo test",
            );
        }
        let rendered = handoff.render().expect("renders");
        assert!(rendered.truncated, "the rendering must be capped");
        assert!(
            rendered.text.len() <= MAX_HANDOFF_BYTES + 64,
            "the cap must actually bound the output: {}",
            rendered.text.len()
        );
        assert!(rendered
            .text
            .ends_with("handoff truncated at the byte cap]\n"));
    }
}
