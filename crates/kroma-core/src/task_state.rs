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

use serde::{Deserialize, Serialize};

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
}

impl TaskState {
    /// Derive the state from the canonical transcript.
    ///
    /// Deliberately rule-based and cheap: user turns are scanned for requirement phrases
    /// ("do not", "don't", "must not", "only", "keep"), and paths are collected as mentions. Nothing
    /// here calls a model, so the derivation cost is zero and cannot silently change behaviour.
    pub fn derive(transcript: &[crate::messages::TranscriptItem]) -> Self {
        let mut state = TaskState::default();
        for (index, item) in transcript.iter().enumerate() {
            match item {
                crate::messages::TranscriptItem::User(user) => {
                    let text = user.content.trim();
                    if text.is_empty() {
                        continue;
                    }
                    if state.goal.is_none() {
                        state.goal = Some(Provenance {
                            canonical_index: index,
                            entry_id: None,
                            quote: text.to_string(),
                        });
                    }
                    for sentence in split_requirement_sentences(text) {
                        if let Some(statement) = normalize_constraint(&sentence) {
                            state.constraints.push(Constraint {
                                statement,
                                provenance: Provenance {
                                    canonical_index: index,
                                    entry_id: None,
                                    quote: sentence.clone(),
                                },
                            });
                        }
                        if let Some(statement) = normalize_verification(&sentence) {
                            state.verification.push(VerificationItem {
                                statement,
                                provenance: Provenance {
                                    canonical_index: index,
                                    entry_id: None,
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
        if has_slash && has_ext && !token.starts_with("http") && !paths.iter().any(|p| p == token)
        {
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
