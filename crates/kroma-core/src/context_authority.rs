//! C00-06: typed context segments — provenance that cannot be upgraded by rewriting it.
//!
//! The baseline keeps provenance (which tool produced which bytes) but has no type that says
//! *how much authority* a piece of context carries, *how sensitive* it is, or *which scope* it
//! belongs to. Without those, a summary of a tool output can be pasted back in as if it were a
//! host policy, and a recalled evidence entry can be read from a scope that no longer owns it.
//!
//! Two rules, both enforced here and nowhere else:
//!
//! 1. **Derivation never escalates.** A segment derived from parents carries the *least*
//!    privileged authority among them and the *strictest* sensitivity. A caller that declares
//!    more authority than its inputs allow is refused, not corrected.
//! 2. **Recall re-checks scope.** Knowing a segment id is not permission to read it: every read
//!    compares the requester's scope with the segment's current scope.
//!
//! This module deliberately holds no text. It is the label set that the projector and the recall
//! path must agree on; the bytes stay in the existing evidence store.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// How much authority a segment carries. Ordered from most to least privileged, so the
/// authority a derivation is allowed to carry is the *least* privileged of its parents — the
/// maximum in this ordering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum AuthorityClass {
    /// Host policy and user authorization: the only classes that may grant anything.
    HostPolicy,
    /// An explicit constraint the user stated in this conversation.
    UserConstraint,
    /// Bytes a tool actually produced (command output, file content read through a tool).
    ToolOutput,
    /// Repository content read directly (a file the workspace owns).
    RepositoryContent,
    /// A model-written summary or note. Derived, never authoritative.
    ModelSummary,
    /// Content fetched from outside the workspace (web page, remote document).
    RemoteContent,
}

/// How sensitive a segment is. Ordered from least to most restricted, so a derivation takes the
/// strictest of its parents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Sensitivity {
    Public,
    Workspace,
    Secret,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SegmentId(pub String);

impl SegmentId {
    pub fn new(id: impl Into<String>) -> Self {
        SegmentId(id.into())
    }
}

/// Where a segment came from. Kept separate from authority: a tool output is real bytes but has no
/// authority to command anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OriginKind {
    UserTurn,
    HostPolicy,
    ToolOutput,
    RepositoryFile,
    ModelSummary,
    RemotePage,
}

/// The label set every projected segment must carry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextSegment {
    pub id: SegmentId,
    pub origin_kind: OriginKind,
    pub authority: AuthorityClass,
    pub sensitivity: Sensitivity,
    pub scope: String,
    pub derivation_parents: Vec<SegmentId>,
    pub observed_revision: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum SegmentError {
    /// The declared authority is higher than any parent grants.
    AuthorityEscalation {
        declared: AuthorityClass,
        allowed: AuthorityClass,
    },
    /// A parent does not exist, so nothing can be derived from it.
    UnknownParent(SegmentId),
    /// Parents from different scopes cannot be merged into one segment.
    ScopeMismatch { expected: String, found: String },
    /// A segment with this id is already present.
    Duplicate(SegmentId),
}

#[derive(Debug, PartialEq, Eq)]
pub enum RecallError {
    Unknown(SegmentId),
    /// The segment exists but belongs to another scope. Knowing the id is not permission.
    WrongScope {
        segment_scope: String,
        requester_scope: String,
    },
}

#[derive(Default)]
pub struct SegmentLedger {
    segments: HashMap<SegmentId, ContextSegment>,
}

impl SegmentLedger {
    /// Record a segment whose labels are declared by its producer (a user turn, a tool result).
    pub fn admit(&mut self, segment: ContextSegment) -> Result<(), SegmentError> {
        if self.segments.contains_key(&segment.id) {
            return Err(SegmentError::Duplicate(segment.id));
        }
        self.segments.insert(segment.id.clone(), segment);
        Ok(())
    }

    /// Derive a new segment from existing ones, optionally declaring the authority the caller
    /// believes it has. The declaration is checked, never trusted: a summary of tool output that
    /// claims host authority is refused.
    pub fn derive(
        &mut self,
        id: SegmentId,
        origin_kind: OriginKind,
        parents: &[SegmentId],
        declared_authority: Option<AuthorityClass>,
    ) -> Result<SegmentId, SegmentError> {
        let mut allowed = AuthorityClass::HostPolicy;
        let mut sensitivity = Sensitivity::Public;
        let mut scope: Option<String> = None;
        let mut observed_revision = None;

        for parent_id in parents {
            let parent = self
                .segments
                .get(parent_id)
                .ok_or_else(|| SegmentError::UnknownParent(parent_id.clone()))?;
            allowed = allowed.max(parent.authority);
            sensitivity = sensitivity.max(parent.sensitivity);
            match &scope {
                None => {
                    scope = Some(parent.scope.clone());
                    observed_revision = parent.observed_revision.clone();
                }
                Some(expected) if expected == &parent.scope => {}
                Some(expected) => {
                    return Err(SegmentError::ScopeMismatch {
                        expected: expected.clone(),
                        found: parent.scope.clone(),
                    })
                }
            }
        }

        if let Some(declared) = declared_authority {
            if declared < allowed {
                return Err(SegmentError::AuthorityEscalation { declared, allowed });
            }
        }

        let segment = ContextSegment {
            id: id.clone(),
            origin_kind,
            authority: declared_authority.unwrap_or(allowed),
            sensitivity,
            scope: scope.unwrap_or_default(),
            derivation_parents: parents.to_vec(),
            observed_revision,
        };
        self.admit(segment)?;
        Ok(id)
    }

    /// Read a segment, re-checking the scope on every call.
    pub fn recall(
        &self,
        id: &SegmentId,
        requester_scope: &str,
    ) -> Result<&ContextSegment, RecallError> {
        let segment = self
            .segments
            .get(id)
            .ok_or_else(|| RecallError::Unknown(id.clone()))?;
        if segment.scope != requester_scope {
            return Err(RecallError::WrongScope {
                segment_scope: segment.scope.clone(),
                requester_scope: requester_scope.to_string(),
            });
        }
        Ok(segment)
    }

    /// Drop a segment (evidence GC, revoked authorization). Later recalls fail instead of
    /// returning a stale label set.
    pub fn forget(&mut self, id: &SegmentId) -> bool {
        self.segments.remove(id).is_some()
    }

    pub fn len(&self) -> usize {
        self.segments.len()
    }

    pub fn is_empty(&self) -> bool {
        self.segments.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_output(id: &str, scope: &str, sensitivity: Sensitivity) -> ContextSegment {
        ContextSegment {
            id: SegmentId::new(id),
            origin_kind: OriginKind::ToolOutput,
            authority: AuthorityClass::ToolOutput,
            sensitivity,
            scope: scope.to_string(),
            derivation_parents: Vec::new(),
            observed_revision: Some("rev-1".to_string()),
        }
    }

    /// C00-06-A1: a summary cannot be more privileged than what it summarises, and it inherits
    /// the strictest sensitivity of its inputs.
    #[test]
    fn a_summary_cannot_escalate_authority_or_launder_sensitivity() {
        let mut ledger = SegmentLedger::default();
        ledger
            .admit(tool_output("t1", "ws-a", Sensitivity::Workspace))
            .unwrap();
        ledger
            .admit(tool_output("t2", "ws-a", Sensitivity::Public))
            .unwrap();

        let escalation = ledger.derive(
            SegmentId::new("sum-1"),
            OriginKind::ModelSummary,
            &[SegmentId::new("t1"), SegmentId::new("t2")],
            Some(AuthorityClass::HostPolicy),
        );
        assert_eq!(
            escalation,
            Err(SegmentError::AuthorityEscalation {
                declared: AuthorityClass::HostPolicy,
                allowed: AuthorityClass::ToolOutput,
            }),
            "a model summary must not claim host authority"
        );

        // Without a declaration it is admitted at the least privileged parent's level, and the
        // workspace-sensitive parent makes the whole summary workspace-sensitive.
        let id = ledger
            .derive(
                SegmentId::new("sum-2"),
                OriginKind::ModelSummary,
                &[SegmentId::new("t1"), SegmentId::new("t2")],
                None,
            )
            .expect("an honest summary is admitted");
        let segment = ledger.recall(&id, "ws-a").unwrap();
        assert_eq!(segment.authority, AuthorityClass::ToolOutput);
        assert_eq!(
            segment.sensitivity,
            Sensitivity::Workspace,
            "mixed sensitivity must take the stricter side, not the milder one"
        );
        assert_eq!(segment.derivation_parents.len(), 2);
        assert_eq!(segment.observed_revision.as_deref(), Some("rev-1"));
    }

    /// C00-06-A2: recall re-checks scope every time; an id is not a capability.
    #[test]
    fn recall_is_refused_across_scopes_and_after_the_segment_is_gone() {
        let mut ledger = SegmentLedger::default();
        ledger
            .admit(tool_output("t1", "ws-a", Sensitivity::Workspace))
            .unwrap();

        assert_eq!(
            ledger.recall(&SegmentId::new("t1"), "ws-b"),
            Err(RecallError::WrongScope {
                segment_scope: "ws-a".to_string(),
                requester_scope: "ws-b".to_string(),
            })
        );
        assert!(ledger.recall(&SegmentId::new("t1"), "ws-a").is_ok());

        assert!(ledger.forget(&SegmentId::new("t1")));
        assert_eq!(
            ledger.recall(&SegmentId::new("t1"), "ws-a"),
            Err(RecallError::Unknown(SegmentId::new("t1"))),
            "a forgotten segment must not be recallable by a stale id"
        );
    }

    /// C00-06-A3: every declared field survives the serialised form the projection receipt uses,
    /// and parents from different scopes cannot be merged.
    #[test]
    fn segments_round_trip_with_every_field_and_refuse_cross_scope_merges() {
        let mut ledger = SegmentLedger::default();
        ledger
            .admit(tool_output("t1", "ws-a", Sensitivity::Secret))
            .unwrap();
        ledger
            .admit(tool_output("t2", "ws-b", Sensitivity::Public))
            .unwrap();

        assert_eq!(
            ledger.derive(
                SegmentId::new("mix"),
                OriginKind::ModelSummary,
                &[SegmentId::new("t1"), SegmentId::new("t2")],
                None,
            ),
            Err(SegmentError::ScopeMismatch {
                expected: "ws-a".to_string(),
                found: "ws-b".to_string(),
            })
        );

        let segment = ledger
            .recall(&SegmentId::new("t1"), "ws-a")
            .unwrap()
            .clone();
        let json = serde_json::to_string(&segment).unwrap();
        let back: ContextSegment = serde_json::from_str(&json).unwrap();
        assert_eq!(back, segment);
        for field in [
            "id",
            "origin_kind",
            "authority",
            "sensitivity",
            "scope",
            "derivation_parents",
            "observed_revision",
        ] {
            assert!(
                json.contains(field),
                "serialised segment must carry `{field}`: {json}"
            );
        }
    }
}
