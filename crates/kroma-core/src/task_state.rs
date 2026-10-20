//! T25 (KR-31): typed task state — goals, constraints, verification items and failure hypotheses,
//! each with the evidence that supports it.
//!
//! Stage 1 of the plan: **no generated summaries.** A free-text "thinking summary" produced by a model
//! is exactly the thing that can quietly contradict the user, survive compaction as if it were fact, and
//! cost money to regenerate. What is added instead is structure that is *derived* from the canonical
//! transcript:
//!
//! * `Constraint` — something the user said must hold ("don't touch tests/"), kept with the index of the
//!   user message it came from, so it can be pointed at after compaction.
//! * `VerificationItem` — a stated way to check the work, with the same provenance.
//! * `FailureHypothesis` — a claim about why something fails, tagged with its `HypothesisStatus`
//!   (unverified / supported / contradicted). A hypothesis never becomes a fact just because it was
//!   written down or repeated.
//!
//! Everything here is a *mention* unless the evidence field says otherwise: a path that appears in tool
//! output is recorded as mentioned, not as causally responsible for anything.
//!
//! The compressor is stage 2 and is **off by default** (`SemanticCompression::Disabled`). Enabling it
//! requires holdout evidence (EXP-01/02) that it does not regress quality; until then the only supported
//! configuration is "derive structure from the transcript", which costs nothing.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::evidence_lifecycle::{FactBinding, FileRevision, Freshness, PinnedSet};

/// Where a constraint or verification item came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Provenance {
    /// Canonical transcript index of the user message.
    pub canonical_index: usize,
    /// The entry id in the session WAL, when the transcript came from a session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry_id: Option<String>,
    /// The user's own words, verbatim. Summaries are not allowed here: this is the quote a reviewer
    /// checks against.
    pub quote: String,
}

/// Something the user required to hold.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Constraint {
    /// Normalised statement, e.g. "do not modify tests/".
    pub statement: String,
    pub provenance: Provenance,
}

/// A stated way to verify the work.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerificationItem {
    pub statement: String,
    pub provenance: Provenance,
}

/// How much support a hypothesis actually has.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HypothesisStatus {
    /// Written down, not checked. The default, and the only status a freshly derived hypothesis gets.
    Unverified,
    /// A tool result or test run directly supports it, and the supporting evidence is recorded.
    Supported,
    /// Something observed contradicts it.
    Contradicted,
}

impl HypothesisStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            HypothesisStatus::Unverified => "unverified",
            HypothesisStatus::Supported => "supported",
            HypothesisStatus::Contradicted => "contradicted",
        }
    }

    /// Is this claim allowed to be presented as a fact?
    ///
    /// Only `Supported` with at least one evidence reference. Everything else must be phrased as a
    /// hypothesis, which is what stops a model's guess from being promoted by repetition.
    pub fn is_fact(self, evidence: &[EvidenceRef]) -> bool {
        self == HypothesisStatus::Supported && !evidence.is_empty()
    }
}

/// A pointer to the evidence behind a claim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceRef {
    /// What kind of thing this points at.
    pub kind: EvidenceKind,
    /// Canonical transcript index or `source_entry_id`, depending on `kind`.
    pub reference: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceKind {
    /// A tool result in the canonical transcript.
    ToolResult,
    /// A compacted stub's evidence id (resolvable through `recall_evidence`).
    EvidenceId,
    /// A verification command's outcome.
    Verification,
}

/// A claim about why something fails. Never a fact by default.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FailureHypothesis {
    pub statement: String,
    pub status: HypothesisStatus,
    #[serde(default)]
    pub evidence: Vec<EvidenceRef>,
}

impl FailureHypothesis {
    pub fn new(statement: impl Into<String>) -> Self {
        Self {
            statement: statement.into(),
            // Freshly derived: written down, not checked.
            status: HypothesisStatus::Unverified,
            evidence: Vec::new(),
        }
    }

    /// Record support. Without evidence the status does not change: "supported" with nothing behind it
    /// is the same problem as an unverified claim presented as fact.
    pub fn support(&mut self, evidence: EvidenceRef) {
        self.evidence.push(evidence);
        self.status = HypothesisStatus::Supported;
    }

    pub fn contradict(&mut self, evidence: EvidenceRef) {
        self.evidence.push(evidence);
        self.status = HypothesisStatus::Contradicted;
    }

    pub fn is_fact(&self) -> bool {
        self.status.is_fact(&self.evidence)
    }
}

/// A path that appeared somewhere in the run. Recorded as a **mention**, never as a cause.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PathMention {
    pub path: String,
    /// Where it was mentioned (canonical transcript index).
    pub canonical_index: usize,
    /// `true` when the only thing we know is that the string appeared. There is no other value today:
    /// causal claims need a hypothesis with evidence, not a path match.
    pub mention_only: bool,
}

/// Typed state for one run.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskState {
    /// The user's task statement, verbatim, with provenance.
    pub goal: Option<Provenance>,
    pub constraints: Vec<Constraint>,
    pub verification: Vec<VerificationItem>,
    pub failure_hypotheses: Vec<FailureHypothesis>,
    pub path_mentions: Vec<PathMention>,
    /// C00-07: facts this state saw, keyed by what they are about, each with the revisions it was
    /// observed against. Empty for a derivation that observed nothing, which is why the digest of
    /// such a state is unchanged by this field (see [`TaskState::digest`]).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub facts: BTreeMap<String, FactBinding>,
    /// C00-07: fact keys a checkpoint or an active task must keep through GC, even when no
    /// constraint or supported fact names them any more.
    #[serde(default, skip_serializing_if = "PinnedSet::is_empty")]
    pub pins: PinnedSet,
}

/// What [`TaskState::collect_garbage`] did: the fact keys it dropped and the ones it kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GarbageCollection {
    pub dropped: BTreeSet<String>,
    pub retained: BTreeSet<String>,
}

impl TaskState {
    /// Derive the state from the canonical transcript.
    ///
    /// Deliberately rule-based and cheap: user turns are scanned for requirement phrases
    /// ("do not", "don't", "must not", "only", "keep"), and paths are collected as mentions. Nothing
    /// here calls a model, so the derivation cost is zero and cannot silently change behaviour.
    pub fn derive(transcript: &[crate::messages::TranscriptItem]) -> Self {
        Self::derive_with_source_ids(transcript, &[])
    }

    /// Derive the state and point every provenance at the session WAL entry the item came from.
    ///
    /// `entry_ids` is positionally aligned with `transcript` — both come from the same session
    /// branch. When they are not aligned (a caller with no WAL, or a transcript that has drifted
    /// from the leaf) no provenance gets an id at all: a plausible-looking wrong id would be worse
    /// than an explicit absence, because a reader would follow it to the wrong event.
    pub fn derive_with_source_ids(
        transcript: &[crate::messages::TranscriptItem],
        entry_ids: &[String],
    ) -> Self {
        let aligned = entry_ids.len() == transcript.len();
        let mut state = TaskState::default();
        for (index, item) in transcript.iter().enumerate() {
            let source_entry_id = if aligned {
                entry_ids.get(index).cloned()
            } else {
                None
            };
            match item {
                crate::messages::TranscriptItem::User(user) => {
                    let text = user.content.trim();
                    if text.is_empty() {
                        continue;
                    }
                    if state.goal.is_none() {
                        state.goal = Some(Provenance {
                            canonical_index: index,
                            entry_id: source_entry_id.clone(),
                            quote: text.to_string(),
                        });
                    }
                    for sentence in split_requirement_sentences(text) {
                        if let Some(statement) = normalize_constraint(&sentence) {
                            state.constraints.push(Constraint {
                                statement,
                                provenance: Provenance {
                                    canonical_index: index,
                                    entry_id: source_entry_id.clone(),
                                    quote: sentence.clone(),
                                },
                            });
                        }
                        if let Some(statement) = normalize_verification(&sentence) {
                            state.verification.push(VerificationItem {
                                statement,
                                provenance: Provenance {
                                    canonical_index: index,
                                    entry_id: source_entry_id.clone(),
                                    quote: sentence,
                                },
                            });
                        }
                    }
                }
                crate::messages::TranscriptItem::ToolResult(result) => {
                    for path in mentioned_paths(&result.content) {
                        if !state
                            .path_mentions
                            .iter()
                            .any(|m| m.path == path && m.canonical_index == index)
                        {
                            state.path_mentions.push(PathMention {
                                path,
                                canonical_index: index,
                                mention_only: true,
                            });
                        }
                    }
                }
                _ => {}
            }
        }
        state
    }

    /// Constraints as they should be shown to the model after compaction: statement plus the quote it
    /// came from, so a reader can check it against the user's own words.
    pub fn constraint_lines(&self) -> Vec<String> {
        self.constraints
            .iter()
            .map(|c| format!("- {} (user: \"{}\")", c.statement, c.provenance.quote))
            .collect()
    }

    /// Hypotheses that may be presented as facts. Everything else must stay phrased as a hypothesis.
    pub fn supported_facts(&self) -> Vec<&FailureHypothesis> {
        self.failure_hypotheses
            .iter()
            .filter(|h| h.is_fact())
            .collect()
    }

    pub fn add_hypothesis(&mut self, hypothesis: FailureHypothesis) {
        self.failure_hypotheses.push(hypothesis);
    }

    /// Record what `binding` was observed against, under `key`.
    pub fn bind_fact(&mut self, key: impl Into<String>, binding: FactBinding) {
        self.facts.insert(key.into(), binding);
    }

    /// Withdraw a fact: the observation it recorded no longer holds, so `key` must not be presented
    /// as a fact until something observes it again.
    pub fn unbind_fact(&mut self, key: &str) {
        self.facts.remove(key);
    }

    /// Keep `key` through GC for `reason` (a checkpoint, an active task). One key can carry several
    /// reasons; it stays pinned until the last one is released.
    pub fn pin_fact(&mut self, key: impl Into<String>, reason: impl Into<String>) {
        self.pins.pin(key, reason);
    }

    /// Release one pin reason. Returns `false` when that reason was not held, which is an answer,
    /// not an error.
    pub fn release_pin(&mut self, key: &str, reason: &str) -> bool {
        self.pins.release(key, reason)
    }

    /// Whether the fact bound under `key` still holds against what `observed` answers for each
    /// subject path and against `environment`.
    ///
    /// `observed` answers `None` for a subject that is no longer there, which is `Missing`, not
    /// freshness. `environment` is what the process can observe now, or `None` when it cannot
    /// observe one at all.
    ///
    /// `None` is **unknown, never fresh**: either nothing is bound under `key`, or the binding records
    /// neither a subject revision nor an environment, so there is nothing to check. The comparison
    /// itself is [`FactBinding::check`], including its rule that a recorded environment which no
    /// longer matches makes the fact not fresh even when every subject is unchanged. A binding that
    /// recorded no environment is not compared against `environment` at all — only what was recorded
    /// can be checked.
    pub fn fact_status(
        &self,
        key: &str,
        observed: impl Fn(&str) -> Option<FileRevision>,
        environment: Option<&str>,
    ) -> Option<Freshness> {
        let binding = self.facts.get(key)?;
        if binding.subject.is_empty() && binding.environment.is_none() {
            return None;
        }
        Some(binding.check(observed, environment))
    }

    /// Drop the bindings of facts nothing references any more, keeping every pinned key.
    ///
    /// `live` is the caller's verdict on which keys are still referenced by a constraint or a
    /// supported fact; a key outside it is stale by that account and is collected unless a pin holds
    /// it. The pin half of the decision is [`PinnedSet::partition`], so a pinned key cannot be dropped
    /// by construction, and the report says which keys were dropped and which survived.
    pub fn collect_garbage(&mut self, live: &BTreeSet<String>) -> GarbageCollection {
        // Partition the keys this state actually holds, so a pinned fact is kept even when `live`
        // does not name it — that is the entire point of a pin. `live` then decides the unpinned ones.
        let bound: BTreeSet<String> = self.facts.keys().cloned().collect();
        let (unpinned, _) = self.pins.partition(&bound);
        let dropped: BTreeSet<String> = unpinned.difference(live).cloned().collect();
        let retained: BTreeSet<String> = bound.difference(&dropped).cloned().collect();
        for key in &dropped {
            self.facts.remove(key);
        }
        GarbageCollection { dropped, retained }
    }

    /// Stable digest of this state, so a checkpoint can name exactly which derivation it saw
    /// without persisting the state itself (the state is always re-derived, never restored).
    ///
    /// `None` only if the state cannot be serialized, which would mean the digest would describe
    /// something other than the state; the caller then records no binding instead of a wrong one.
    pub fn digest(&self) -> Option<String> {
        serde_json::to_vec(self)
            .ok()
            .map(|bytes| crate::prompt::sha256_hex64(&bytes))
    }
}

/// What a derived [`TaskState`] was derived from, recorded on the session checkpoint written before
/// the request that consumed the transcript.
///
/// This is a provenance stamp, not a validity verdict. It names the WAL leaf, the events that
/// supplied the facts, and the workspace revision the process had *observed* at that moment. There
/// is deliberately no `valid`/`verified` flag: a revision only advances when Kroma captures one
/// itself, so an edit made outside the run advances nothing, and a boolean here would be a claim the
/// process cannot check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskStateBinding {
    /// `sha256` of the canonical JSON of the derived state.
    pub state_digest: String,
    /// The workspace revision observed when the state was derived, or `None` when no revision had
    /// been captured (no snapshot store attached).
    pub derived_at_observed_revision: Option<String>,
    /// WAL entry the transcript ended at when the state was derived.
    pub leaf_entry_id: Option<String>,
    /// WAL entries of the transcript items that produced a constraint or verification item. Bounded by
    /// the number of distinct source events, not by the transcript length, so it stays small — but it
    /// is not a fixed size.
    pub source_entry_ids: Vec<String>,
    /// Canonical transcript length the derivation consumed. The digest reproduces from exactly this
    /// prefix of the leaf path — the *canonical* items, i.e. after any projection overlay the request
    /// wrote, not the raw pre-compaction text.
    pub transcript_len: usize,
    pub constraint_count: usize,
    pub verification_count: usize,
}

impl TaskStateBinding {
    /// Stamp what `state` was derived from, or `None` when the derivation cannot name its sources.
    ///
    /// Every constraint/verification provenance must carry a WAL entry id. Without one the binding
    /// would name a leaf it cannot actually point a fact at, so the caller records nothing rather
    /// than a binding that only looks aligned.
    pub fn observe(
        state: &TaskState,
        leaf_entry_id: Option<String>,
        observed_workspace_revision: Option<String>,
        transcript_len: usize,
    ) -> Option<Self> {
        let mut source_entry_ids: Vec<String> = Vec::new();
        for provenance in state
            .constraints
            .iter()
            .map(|constraint| &constraint.provenance)
            .chain(state.verification.iter().map(|item| &item.provenance))
        {
            let entry_id = provenance.entry_id.as_ref()?;
            if !source_entry_ids.iter().any(|seen| seen == entry_id) {
                source_entry_ids.push(entry_id.clone());
            }
        }
        Some(Self {
            state_digest: state.digest()?,
            derived_at_observed_revision: observed_workspace_revision,
            leaf_entry_id,
            source_entry_ids,
            transcript_len,
            constraint_count: state.constraints.len(),
            verification_count: state.verification.len(),
        })
    }
}

fn split_requirement_sentences(text: &str) -> Vec<String> {
    text.split(['.', '\n', ';'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// Requirement phrasing that has to survive compaction. Intentionally narrow: a false positive here
/// would put words in the user's mouth in the compacted view.
fn normalize_constraint(sentence: &str) -> Option<String> {
    let lower = sentence.to_ascii_lowercase();
    let markers = [
        "do not ",
        "don't ",
        "must not ",
        "mustn't ",
        "never ",
        "only ",
        "keep ",
        "without changing ",
        "不要",
        "别",
        "只能",
        "必须",
    ];
    if !markers.iter().any(|m| lower.contains(m)) {
        return None;
    }
    // Keep the user's sentence as the statement (normalised whitespace), not a paraphrase.
    Some(sentence.split_whitespace().collect::<Vec<_>>().join(" "))
}

/// "verify with X" / "check that X" style statements become verification items.
fn normalize_verification(sentence: &str) -> Option<String> {
    let lower = sentence.to_ascii_lowercase();
    let markers = [
        "verify",
        "check that",
        "run the tests",
        "make sure",
        "验证",
        "确认",
        "跑测试",
    ];
    if !markers.iter().any(|m| lower.contains(m)) {
        return None;
    }
    Some(sentence.split_whitespace().collect::<Vec<_>>().join(" "))
}

/// Paths mentioned in tool output. Purely lexical: anything that looks like a relative path with a
/// file extension. These are *mentions*; nothing here implies the file caused anything.
fn mentioned_paths(content: &str) -> Vec<String> {
    let mut paths = Vec::new();
    for token in
        content.split(|c: char| c.is_whitespace() || c == '"' || c == '\'' || c == ',' || c == ':')
    {
        let token =
            token.trim_matches(|c: char| c == '(' || c == ')' || c == '[' || c == ']' || c == '`');
        if token.len() < 3 || token.len() > 200 {
            continue;
        }
        let has_slash = token.contains('/');
        let has_ext = token
            .rsplit_once('.')
            .map(|(_, ext)| {
                !ext.is_empty() && ext.len() <= 8 && ext.chars().all(|c| c.is_ascii_alphanumeric())
            })
            .unwrap_or(false);
        if has_slash && has_ext && !token.starts_with("http") && !paths.iter().any(|p| p == token) {
            paths.push(token.to_string());
        }
    }
    paths
}

/// How the optional semantic compressor behaves. Stage 2, and **off by default**.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SemanticCompression {
    /// No compressor. Structure is derived from the transcript, which costs nothing.
    #[default]
    Disabled,
    /// A schema-constrained compressor. Only allowed after holdout evidence; see
    /// `docs/adr/0002-task-state-and-semantic-compression.md`.
    SchemaConstrained,
}

/// What a compression step cost, so it lands in the same ledger as provider spend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct CompressionCost {
    /// Tokens the compressor itself consumed.
    pub compressor_input_tokens: u64,
    pub compressor_output_tokens: u64,
    /// Bytes re-read from the evidence store because the summary pointed at them.
    pub reread_bytes: u64,
    /// Cache entries that had to be rebuilt because the prefix changed.
    pub cache_rebuilds: u64,
    /// `true` when no compressor ran (the default configuration).
    pub compressor_disabled: bool,
}

impl CompressionCost {
    /// A compression step that only derived structure: no model call, no reread.
    pub fn structural_only() -> Self {
        Self {
            compressor_disabled: true,
            ..Self::default()
        }
    }

    /// Total tokens attributable to compression, compressor in and out.
    pub fn total_tokens(&self) -> u64 {
        self.compressor_input_tokens
            .saturating_add(self.compressor_output_tokens)
    }

    /// A cost report is only comparable when it accounts for rereads and rebuilds, not just the
    /// compressor call.
    pub fn describe(&self) -> String {
        format!(
            "compressor={}{} tokens, reread={} bytes, cache_rebuilds={}",
            if self.compressor_disabled {
                "disabled "
            } else {
                ""
            },
            self.total_tokens(),
            self.reread_bytes,
            self.cache_rebuilds
        )
    }

    /// Sanity: with the compressor disabled there must be no compressor tokens (a disabled compressor
    /// that still bills is a configuration bug, not a cheaper variant).
    pub fn validate(&self) -> Result<(), String> {
        if self.compressor_disabled && self.total_tokens() > 0 {
            return Err("compression is disabled but compressor tokens were recorded".to_string());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn revision(path: &str, sha256: &str) -> FileRevision {
        FileRevision {
            path: path.to_string(),
            len: 10,
            mtime_ns: 1_000,
            sha256: sha256.to_string(),
        }
    }

    fn keys(paths: &[&str]) -> BTreeSet<String> {
        paths.iter().map(|path| path.to_string()).collect()
    }

    /// What the workspace looks like now, for the paths named in `revisions`; every other path is
    /// gone, which is what a subject the observation does not cover means.
    fn observed(revisions: &[FileRevision]) -> impl Fn(&str) -> Option<FileRevision> {
        let revisions = revisions.to_vec();
        move |path| {
            revisions
                .iter()
                .find(|revision| revision.path == path)
                .cloned()
        }
    }

    fn bound_state() -> TaskState {
        let mut state = TaskState::default();
        state.bind_fact(
            "src/a.rs",
            FactBinding::new("a builds").observing(revision("src/a.rs", "aa")),
        );
        state.bind_fact(
            "src/b.rs",
            FactBinding::new("b builds").observing(revision("src/b.rs", "bb")),
        );
        state
    }

    /// C00-07: GC drops the binding of a fact nothing references any more and keeps a pinned one,
    /// even though the caller's live set names neither.
    #[test]
    fn pinned_fact_survives_gc_while_an_unreferenced_fact_is_dropped() {
        let mut state = bound_state();
        state.pin_fact("src/b.rs", "checkpoint");

        let report = state.collect_garbage(&keys(&[]));

        assert_eq!(report.dropped, keys(&["src/a.rs"]));
        assert_eq!(report.retained, keys(&["src/b.rs"]));
        assert!(!state.facts.contains_key("src/a.rs"));
        assert!(
            state.facts.contains_key("src/b.rs"),
            "a pinned fact is never collected"
        );
        assert!(state.pins.is_pinned("src/b.rs"));
    }

    /// C00-07: a fact whose subject changed is not fresh, an unbound key is unknown rather than
    /// fresh, and a moved environment is not fresh — so the unpinned stale binding is collectable.
    #[test]
    fn stale_unpinned_fact_is_not_fresh_and_is_dropped() {
        let mut state = bound_state();
        state.bind_fact(
            "src/c.rs",
            FactBinding::new("c builds")
                .observing(revision("src/c.rs", "cc"))
                .in_environment("rust 1.85.1"),
        );

        assert_eq!(
            state.fact_status("src/a.rs", observed(&[revision("src/a.rs", "zz")]), None),
            Some(Freshness::Stale {
                changed: vec!["src/a.rs".to_string()]
            })
        );
        assert_eq!(
            state.fact_status(
                "src/c.rs",
                observed(&[revision("src/c.rs", "cc")]),
                Some("rust 1.86.0")
            ),
            Some(Freshness::EnvironmentMoved {
                expected: "rust 1.85.1".to_string(),
                observed: Some("rust 1.86.0".to_string()),
            })
        );
        assert_eq!(
            state.fact_status(
                "src/never-bound.rs",
                observed(&[revision("src/never-bound.rs", "dd")]),
                None
            ),
            None,
            "a fact nobody bound is unknown, never fresh"
        );
        assert_eq!(
            state.fact_status(
                "src/a.rs",
                observed(&[revision("src/other.rs", "ee")]),
                None
            ),
            Some(Freshness::Missing {
                paths: vec!["src/a.rs".to_string()]
            }),
            "a subject the observation does not cover is missing, not fresh"
        );

        let report = state.collect_garbage(&keys(&["src/b.rs"]));
        assert_eq!(report.dropped, keys(&["src/a.rs", "src/c.rs"]));
        assert_eq!(report.retained, keys(&["src/b.rs"]));
        assert_eq!(state.facts.len(), 1);
    }

    /// C00-07: a state with no bindings and no pins serializes without the new fields, so its digest
    /// is byte-identical to the one taken before this wiring existed.
    #[test]
    fn digest_of_a_state_without_bindings_or_pins_is_unchanged() {
        let legacy = TaskState::default();
        let json = serde_json::to_vec(&legacy).expect("state serializes");
        let text = String::from_utf8(json.clone()).expect("json is utf-8");
        let canonical = r#"{"goal":null,"constraints":[],"verification":[],"failure_hypotheses":[],"path_mentions":[]}"#;
        assert_eq!(
            text, canonical,
            "bindings and pins must be absent from the canonical JSON while they are empty"
        );
        assert_eq!(
            legacy.digest(),
            Some(crate::prompt::sha256_hex64(canonical.as_bytes())),
            "the digest is still the hash of that same canonical JSON"
        );

        let mut bound = bound_state();
        bound.pin_fact("src/b.rs", "checkpoint");
        assert_ne!(
            bound.digest(),
            legacy.digest(),
            "bindings and pins are part of the state once they exist"
        );
    }
}
