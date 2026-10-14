use crate::budget::BudgetConfig;
use crate::messages::{CacheBreakpoint, CacheBreakpointMarker, ToolResultMessage, TranscriptItem};
use crate::prompt::sha256_hex64;
use crate::types::{TerminalReason, ToolCallId};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock};

pub const MIN_COMPACTABLE_TOOL_BYTES: usize = 1024; // > 1 KiB
pub const WORKING_SET_PIN_TURNS: usize = 8;
pub const MAX_OUTLINE_SYMBOLS: usize = 12;
pub const MAX_OUTLINE_STUB_BYTES: usize = 400;
/// BUG-07: hard cap on the total raw bytes retained for `recall_evidence`.
///
/// Compaction archives every reclaimable tool output, so without a cap a long session would pin
/// all of them for its whole lifetime. When the cap is exceeded the oldest archived entries (by
/// transcript position) are dropped deterministically. Their [`EvidenceLink`]s stay addressable, so
/// a later `recall_evidence` reports that the raw bytes are no longer retained instead of failing
/// opaquely or panicking.
pub const MAX_EVIDENCE_BYTES: usize = 16 * 1024 * 1024;
/// Why a tool result was replaced by an Outline Stub (Gate 2 evidence provenance).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionReason {
    /// Reclaimed by the high→low watermark Step-Down Compaction.
    StepDownArchive,
    /// A later `read` of the same path superseded this output.
    SupersededRead,
}

/// Deterministic, replay-stable identifier for the raw evidence behind an Outline Stub.
///
/// Built from the canonical transcript index plus the tool `call_id`, both of which survive
/// `--resume` and `--fork` unchanged for every retained (prefix) item, so the same stub always
/// carries the same id across processes.
pub fn source_entry_id(canonical_index: usize, call_id: &str) -> String {
    format!("ev-{:04}-{}", canonical_index, call_id)
}

/// Parse the `source_entry_id=<id>` field back out of an Outline Stub header.
pub fn extract_source_entry_id(content: &str) -> Option<String> {
    let start = content.find("source_entry_id=")? + "source_entry_id=".len();
    let rest = &content[start..];
    let end = rest
        .find([' ', '|', ']', ')', ','])
        .unwrap_or(rest.len());
    let id = rest[..end].trim();
    if id.is_empty() {
        None
    } else {
        Some(id.to_string())
    }
}

/// Evidence graph node: binds a compacted Outline Stub to the raw tool output it replaced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceLink {
    pub source_entry_id: String,
    /// Index of the compacted item inside the canonical transcript.
    pub canonical_index: usize,
    pub call_id: String,
    pub tool_name: String,
    pub relative_path: Option<String>,
    pub original_sha256: String,
    pub original_bytes: usize,
    pub original_lines: usize,
    pub stub_bytes: usize,
    pub reason: CompactionReason,
}

/// Estimated-token mirror of [`ContextBudgetBreakdown`] (`bytes / 4`), reported alongside the byte
/// counts so the Gate 2 A/B kroma can compare profiles on a provider-shaped metric.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextBudgetTokens {
    pub system_prefix_tokens: usize,
    pub tool_schema_tokens: usize,
    pub history_tokens: usize,
    pub tool_output_tokens: usize,
    /// Total estimated request tokens (`system prefix + tool schema + history`).
    pub total_tokens: usize,
}

/// Deterministic per-category byte accounting for the projected request (Gate 2).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextBudgetBreakdown {
    /// Bytes of the frozen system prefix (system prompt incl. workspace orientation).
    pub system_prefix_bytes: usize,
    /// Bytes of the model-visible tool schemas.
    pub tool_schema_bytes: usize,
    /// Bytes of every projected history item (= `total_projected_bytes`).
    pub history_bytes: usize,
    /// Bytes of the tool results inside the projected history.
    pub tool_output_bytes: usize,
    /// Bytes of Working-Set-pinned (recently modified file) tool output kept verbatim.
    pub pinned_working_set_bytes: usize,
    /// Bytes occupied by the Outline Stubs that replaced archived output.
    pub compacted_stub_bytes: usize,
    /// Bytes of the protected recent tail window.
    pub recent_tail_bytes: usize,
    /// Total estimated bytes of every projected history item.
    pub total_projected_bytes: usize,
    /// Number of raw-evidence pointers available for `recall_evidence`.
    pub evidence_link_count: usize,
    /// Successful `recall_evidence` calls in this projection's lifetime.
    pub recall_hit_count: u64,
    /// Failed `recall_evidence` calls in this projection's lifetime.
    pub recall_miss_count: u64,
    /// `bytes / 4` estimate of every component above.
    pub tokens: ContextBudgetTokens,
}

impl ContextBudgetBreakdown {
    /// Estimated tokens for each component (`bytes / 4`).
    pub fn estimated_tokens(&self) -> ContextBudgetTokens {
        ContextBudgetTokens {
            system_prefix_tokens: self.system_prefix_bytes / 4,
            tool_schema_tokens: self.tool_schema_bytes / 4,
            history_tokens: self.history_bytes / 4,
            tool_output_tokens: self.tool_output_bytes / 4,
            total_tokens: (self.system_prefix_bytes + self.tool_schema_bytes + self.history_bytes)
                / 4,
        }
    }
}

/// Which Gate 2 context-projection profile a run uses.
///
/// * `Baseline` — no compaction at all (the pre-Gate-2 algorithm, kept for A/B comparison).
/// * `P1` — deterministic watermarks, Step-Down Compaction and Working-Set Pinning, but no
///   evidence graph: stubs carry no `source_entry_id` and raw output is not retained.
/// * `P2` — `P1` plus the `source_entry_id` evidence graph and the `recall_evidence` pointers.
///   This is the default.
/// * `P3` — `P2` plus a stricter budget target (compact an extra 25% below the low watermark).
///   Experimental and off by default.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectionProfile {
    Baseline,
    P1,
    #[default]
    P2,
    P3,
}

impl ProjectionProfile {
    pub fn as_str(&self) -> &'static str {
        match self {
            ProjectionProfile::Baseline => "baseline",
            ProjectionProfile::P1 => "p1",
            ProjectionProfile::P2 => "p2",
            ProjectionProfile::P3 => "p3",
        }
    }

    /// `Baseline` never compacts; every other profile runs Step-Down Compaction.
    pub fn compacts(&self) -> bool {
        !matches!(self, ProjectionProfile::Baseline)
    }

    /// Only `P2`/`P3` mint `source_entry_id`s and retain raw evidence for `recall_evidence`.
    pub fn records_evidence(&self) -> bool {
        matches!(self, ProjectionProfile::P2 | ProjectionProfile::P3)
    }

    /// `P3` compacts an extra 25% below the profile's low-watermark target.
    pub fn is_aggressive(&self) -> bool {
        matches!(self, ProjectionProfile::P3)
    }

    /// B2 (`v1.4.0`): parse a `projection_profile` config value.
    pub fn parse(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "baseline" => Ok(ProjectionProfile::Baseline),
            "p1" => Ok(ProjectionProfile::P1),
            "p2" => Ok(ProjectionProfile::P2),
            "p3" => Ok(ProjectionProfile::P3),
            other => Err(format!(
                "unknown projection profile '{}'; expected baseline|p1|p2|p3",
                other
            )),
        }
    }
}

/// Aggregated projection statistics exposed to the runtime and the acceptance tests.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProjectionStats {
    pub budget: ContextBudgetBreakdown,
    pub tokens: ContextBudgetTokens,
    pub compaction_reread_count: u64,
    pub evidence_link_count: usize,
    pub recall_hit_count: u64,
    pub recall_miss_count: u64,
}

#[derive(Debug, Clone)]
pub struct CompactionEventRecord {
    pub before_bytes: usize,
    pub after_bytes: usize,
    pub compacted_seq_indices: Vec<usize>,
    pub pinned_files: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ProjectedContext {
    pub messages: Vec<TranscriptItem>,
    pub breakpoints: Vec<CacheBreakpointMarker>,
    pub message_history_bytes: usize,
    pub compaction_event: Option<CompactionEventRecord>,
}

/// Cache-Aligned ContextProjector (§7):
/// - Preserves 100% byte-for-byte prefix stability below watermarks (Append-Only).
/// - Executes one-shot Step-Down Compaction from B_high -> B_low (and Token watermarks T_high -> T_low).
/// - Implements Working-Set Pinning (files modified in the last 8 turns exempt their latest `read` and `edit` diff).
/// - Replaces unpinned middle-region read-only outputs > 1 KiB with Outline Stubs (16-char sha256 + top 12 symbols).
/// - Annotates 4 logical `CacheBreakpoint` markers.
/// - Tracks `compaction_reread_count` (archival regret rate).
pub struct ContextProjector {
    projected_items: Vec<TranscriptItem>,
    compacted_indices: HashSet<usize>,
    compacted_file_paths: HashSet<String>,
    compaction_reread_count: u64,
    last_pinned_baseline_end: usize,
    last_touched_turns: HashMap<String, usize>,
    /// Raw tool output retained for `recall_evidence`, keyed by `source_entry_id`.
    evidence: HashMap<String, Arc<str>>,
    /// Total bytes currently retained in `evidence`, used to enforce [`MAX_EVIDENCE_BYTES`].
    evidence_bytes: usize,
    /// `call_id` → `source_entry_id`, so a model may recall by either identifier.
    evidence_call_index: HashMap<String, String>,
    /// Deterministic, ordered evidence graph for the current projection.
    evidence_links: Vec<EvidenceLink>,
    last_budget: ContextBudgetBreakdown,
    /// Frozen system-prefix byte count, reported in the budget breakdown.
    prefix_system_bytes: usize,
    /// Frozen tool-schema byte count, reported in the budget breakdown.
    prefix_tool_schema_bytes: usize,
    /// Successful `recall_evidence` calls (evidence-recovery success rate).
    recall_hit_count: u64,
    /// Failed `recall_evidence` calls.
    recall_miss_count: u64,
    profile: ProjectionProfile,
}

impl Default for ContextProjector {
    fn default() -> Self {
        Self::new()
    }
}

impl ContextProjector {
    pub fn new() -> Self {
        Self {
            projected_items: Vec::new(),
            compacted_indices: HashSet::new(),
            compacted_file_paths: HashSet::new(),
            compaction_reread_count: 0,
            last_pinned_baseline_end: 0,
            last_touched_turns: HashMap::new(),
            evidence: HashMap::new(),
            evidence_bytes: 0,
            evidence_call_index: HashMap::new(),
            evidence_links: Vec::new(),
            last_budget: ContextBudgetBreakdown::default(),
            prefix_system_bytes: 0,
            prefix_tool_schema_bytes: 0,
            recall_hit_count: 0,
            recall_miss_count: 0,
            profile: ProjectionProfile::default(),
        }
    }

    /// Select the Gate 2 projection profile (default `P2`).
    pub fn with_profile(mut self, profile: ProjectionProfile) -> Self {
        self.profile = profile;
        self
    }

    pub fn profile(&self) -> ProjectionProfile {
        self.profile
    }

    /// Freeze the system-prefix / tool-schema byte split for the budget breakdown. The single
    /// `static_prefix_bytes` argument of [`ContextProjector::project`] cannot distinguish them.
    pub fn set_prefix_breakdown(&mut self, system_prefix_bytes: usize, tool_schema_bytes: usize) {
        self.prefix_system_bytes = system_prefix_bytes;
        self.prefix_tool_schema_bytes = tool_schema_bytes;
    }

    /// Record a successful `recall_evidence` call (evidence-recovery success telemetry).
    pub fn record_recall_hit(&mut self) {
        self.recall_hit_count = self.recall_hit_count.saturating_add(1);
    }

    /// Record a failed `recall_evidence` call.
    pub fn record_recall_miss(&mut self) {
        self.recall_miss_count = self.recall_miss_count.saturating_add(1);
    }

    pub fn recall_counts(&self) -> (u64, u64) {
        (self.recall_hit_count, self.recall_miss_count)
    }

    pub fn compaction_reread_count(&self) -> u64 {
        self.compaction_reread_count
    }

    /// Evidence graph for the current projection, ordered by canonical index.
    pub fn evidence_links(&self) -> &[EvidenceLink] {
        &self.evidence_links
    }

    /// Deterministic byte accounting for the last `project` call.
    pub fn stats(&self) -> ProjectionStats {
        ProjectionStats {
            budget: self.last_budget.clone(),
            tokens: self.last_budget.tokens.clone(),
            compaction_reread_count: self.compaction_reread_count,
            evidence_link_count: self.evidence_links.len(),
            recall_hit_count: self.recall_hit_count,
            recall_miss_count: self.recall_miss_count,
        }
    }

    /// Register raw evidence for an Outline Stub (used by compaction and when seeding from a
    /// resumed WAL).
    ///
    /// Registration is also what marks an item as compacted, so a `--resume` that re-seeds stubs
    /// from the WAL restores the same `compacted_indices` / `compacted_file_paths` bookkeeping the
    /// original compaction had — otherwise `ContextBudgetBreakdown.compacted_stub_bytes` would be
    /// under-reported and reread-regret accounting would be blind. Re-registering an existing
    /// `source_entry_id` overwrites the stored link/bytes instead of being dropped, so a rewound
    /// branch's stale state can never block a fresh compaction at the same index.
    pub fn register_evidence(&mut self, link: EvidenceLink, raw: Arc<str>) {
        self.evidence_call_index
            .insert(link.call_id.clone(), link.source_entry_id.clone());
        let raw_len = raw.len();
        if let Some(previous) = self.evidence.insert(link.source_entry_id.clone(), raw) {
            self.evidence_bytes = self.evidence_bytes.saturating_sub(previous.len());
        }
        self.evidence_bytes = self.evidence_bytes.saturating_add(raw_len);
        self.compacted_indices.insert(link.canonical_index);
        if link.tool_name == "read" {
            if let Some(path) = link.relative_path.clone() {
                self.compacted_file_paths.insert(path);
            }
        }
        self.evidence_links
            .retain(|existing| existing.source_entry_id != link.source_entry_id);
        self.evidence_links.push(link);
        self.evidence_links.sort_by(|a, b| {
            a.canonical_index
                .cmp(&b.canonical_index)
                .then_with(|| a.source_entry_id.cmp(&b.source_entry_id))
        });
        self.evict_evidence_over_cap();
    }

    /// BUG-07: keep the retained raw-evidence total under [`MAX_EVIDENCE_BYTES`]. Entries are
    /// dropped oldest-first by transcript position (ties broken by id, so eviction is deterministic
    /// and replay-stable). Only the raw bytes are dropped: the [`EvidenceLink`] and the
    /// `call_id`→`source_entry_id` index survive, so `recall_evidence` still resolves the stub and
    /// reports that the bytes are no longer retained.
    fn evict_evidence_over_cap(&mut self) {
        if self.evidence_bytes <= MAX_EVIDENCE_BYTES {
            return;
        }
        let mut order: Vec<(usize, String)> = self
            .evidence_links
            .iter()
            .filter(|link| self.evidence.contains_key(&link.source_entry_id))
            .map(|link| (link.canonical_index, link.source_entry_id.clone()))
            .collect();
        order.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        for (_, id) in order {
            if self.evidence_bytes <= MAX_EVIDENCE_BYTES {
                break;
            }
            if let Some(dropped) = self.evidence.remove(&id) {
                self.evidence_bytes = self.evidence_bytes.saturating_sub(dropped.len());
            }
        }
    }

    /// Look up raw evidence by `source_entry_id` or by the originating tool `call_id`.
    pub fn evidence_lookup(&self, id: &str) -> Option<Arc<str>> {
        if let Some(raw) = self.evidence.get(id) {
            return Some(Arc::clone(raw));
        }
        let canonical = self.evidence_call_index.get(id)?;
        self.evidence.get(canonical).map(Arc::clone)
    }

    /// Evidence-graph metadata for `source_entry_id` (or the originating tool `call_id`).
    ///
    /// Resolved from [`Self::evidence_links`] first, so a link stays addressable even after its raw
    /// bytes were evicted by the [`MAX_EVIDENCE_BYTES`] cap — `recall_evidence` can then report that
    /// the bytes are gone instead of claiming the id is unknown.
    pub fn evidence_link(&self, id: &str) -> Option<&EvidenceLink> {
        if let Some(link) = self
            .evidence_links
            .iter()
            .find(|link| link.source_entry_id == id)
        {
            return Some(link);
        }
        let canonical = self.evidence_call_index.get(id)?;
        self.evidence_links
            .iter()
            .find(|link| link.source_entry_id == *canonical)
    }

    /// Stable index of the append-only pinned baseline prefix. Persisted by the session WAL.
    pub fn last_pinned_baseline_end(&self) -> usize {
        self.last_pinned_baseline_end
    }

    /// Restore the pinned baseline cursor from a validated session checkpoint.
    /// Replayed transcript items are still verified and projected normally after restoration.
    pub fn restore_pinned_baseline_end(&mut self, index: usize) {
        self.last_pinned_baseline_end = index;
    }

    /// Restore the durable hot-file ledger from a session checkpoint.
    pub fn restore_last_touched<I>(&mut self, ledger: I)
    where
        I: IntoIterator<Item = (String, usize)>,
    {
        self.last_touched_turns = ledger.into_iter().collect();
    }

    /// Last successful file-modification turn observed on the active transcript path.
    pub fn last_touched_turns(&self) -> &HashMap<String, usize> {
        &self.last_touched_turns
    }

    fn refresh_last_touched_from_transcript(&mut self) {
        let mut assistant_turn = 0usize;
        for item in &self.projected_items {
            match item {
                TranscriptItem::Assistant(_) => assistant_turn = assistant_turn.saturating_add(1),
                TranscriptItem::ToolResult(tr)
                    if tr.outcome.is_success() && tr.meta.modified_file =>
                {
                    if let Some(path) = &tr.meta.relative_path {
                        // Never regress a larger turn restored from the session checkpoint: a file
                        // touched deep in the history must not be re-stamped as "just modified".
                        let slot = self
                            .last_touched_turns
                            .entry(path.clone())
                            .or_insert(assistant_turn);
                        *slot = (*slot).max(assistant_turn);
                    }
                }
                _ => {}
            }
        }
    }

    /// Record when a `read` tool call completes so we can track `compaction_reread_count`
    /// if the file was previously compacted into an Outline Stub.
    pub fn notify_file_read(&mut self, rel_path: &str) {
        if self.compacted_file_paths.remove(rel_path) {
            self.compaction_reread_count += 1;
        }
    }

    /// Project the canonical transcript for the next Provider request while preserving prefix stability.
    // The `>` arm appends to `self.projected_items`, which is the collection the `<` arm compares
    // against, so the two arms are deliberately exclusive rather than a `match` on `cmp`.
    #[allow(clippy::comparison_chain)]
    pub fn project(
        &mut self,
        canonical: &mut [TranscriptItem],
        budget: &BudgetConfig,
        static_prefix_bytes: usize,
        last_input_tokens: Option<u64>,
        current_turn: usize,
    ) -> Result<ProjectedContext, TerminalReason> {
        // Append any newly added canonical items without touching already-projected items!
        if canonical.len() > self.projected_items.len() {
            let already_projected = self.projected_items.len();
            for (idx, item) in canonical.iter().enumerate().skip(already_projected) {
                // A resumed transcript already carries Outline Stubs whose raw output lives in the
                // WAL. Mark them compacted so the byte breakdown and reread-regret accounting match
                // a run that compacted them in-process.
                if let TranscriptItem::ToolResult(tr) = item {
                    if extract_source_entry_id(&tr.content).is_some() {
                        self.compacted_indices.insert(idx);
                    }
                }
                self.projected_items.push(item.clone());
            }
        } else if canonical.len() < self.projected_items.len() {
            // `--fork` rewound the canonical transcript: drop projection state past the new tail so
            // stale messages are never re-sent and compaction bookkeeping cannot dangle.
            self.projected_items.truncate(canonical.len());
            self.compacted_indices.retain(|&idx| idx < canonical.len());
            self.last_pinned_baseline_end = self.last_pinned_baseline_end.min(canonical.len());
            // Evidence links for rewound items must go too, so a later append at the same index
            // cannot inherit a stale pointer. The raw bytes and the `call_id` index are pruned with
            // them: otherwise a reused `(canonical_index, call_id)` pair would mint an identical
            // `source_entry_id` and be silently dropped by `register_evidence`.
            let mut stale: Vec<EvidenceLink> = Vec::new();
            self.evidence_links.retain(|link| {
                if link.canonical_index < canonical.len() {
                    true
                } else {
                    stale.push(link.clone());
                    false
                }
            });
            for link in stale {
                if let Some(dropped) = self.evidence.remove(&link.source_entry_id) {
                    self.evidence_bytes = self.evidence_bytes.saturating_sub(dropped.len());
                }
                if self
                    .evidence_call_index
                    .get(&link.call_id)
                    .map(String::as_str)
                    == Some(link.source_entry_id.as_str())
                {
                    self.evidence_call_index.remove(&link.call_id);
                }
            }
            self.compacted_file_paths = self
                .evidence_links
                .iter()
                .filter(|link| link.tool_name == "read")
                .filter_map(|link| link.relative_path.clone())
                .collect();
        }
        self.refresh_last_touched_from_transcript();

        let history_bytes: usize = self
            .projected_items
            .iter()
            .map(|i| i.estimated_bytes())
            .sum();
        let raw_tool_bytes: usize = self
            .projected_items
            .iter()
            .filter_map(|item| match item {
                // Gate 7: a multimodal `read` carries its payload in `image_attachments`, so the
                // tool-output budget must count those bytes or images bypass the cap entirely.
                TranscriptItem::ToolResult(tr) => {
                    Some(tr.content.len() + tr.image_attachment_bytes())
                }
                _ => None,
            })
            .sum();
        let total_request_bytes = static_prefix_bytes + history_bytes;

        let byte_exceeded = total_request_bytes > budget.context_high_watermark_bytes
            || raw_tool_bytes > budget.max_total_tool_output_bytes;
        let token_exceeded = match (budget.token_watermarks(), last_input_tokens) {
            (Some((t_high, _)), Some(last_tok)) => last_tok > t_high,
            _ => false,
        };

        let mut compaction_event = None;

        if self.profile.compacts() && (byte_exceeded || token_exceeded) {
            let before_bytes = total_request_bytes;
            let target_low_bytes = budget
                .context_low_watermark_bytes
                .saturating_sub(static_prefix_bytes);

            // Calculate token-proportional byte target if triggered by token watermark
            let effective_target_bytes = match (budget.token_watermarks(), last_input_tokens) {
                (Some((_, t_low)), Some(last_tok)) if token_exceeded && last_tok > 0 => {
                    let ratio = (t_low as f64) / (last_tok as f64);
                    let tok_target = ((history_bytes as f64) * ratio) as usize;
                    target_low_bytes.min(tok_target)
                }
                _ => target_low_bytes,
            };
            // P3 (experimental, default off) compacts an extra 25% below the profile's low-watermark
            // target so an A/B run can measure the cost/benefit of a stricter budget.
            let effective_target_bytes = if self.profile.is_aggressive() {
                effective_target_bytes - effective_target_bytes / 4
            } else {
                effective_target_bytes
            };

            let (compacted_seqs, pinned_files) = self.run_step_down_compaction(
                canonical,
                budget.context_min_tail_turns,
                budget.context_tail_window_bytes,
                effective_target_bytes,
                current_turn,
            );
            let mut after_history_bytes: usize = self
                .projected_items
                .iter()
                .map(|i| i.estimated_bytes())
                .sum();
            let mut after_total_bytes = static_prefix_bytes + after_history_bytes;

            // §7.2: If still above B_high after normal compaction, shrink tail protection window to 1 turn
            if after_total_bytes > budget.context_high_watermark_bytes
                || (token_exceeded && after_history_bytes > effective_target_bytes)
            {
                let (extra_seqs, _) = self.run_step_down_compaction(
                    canonical,
                    1,
                    0,
                    effective_target_bytes,
                    current_turn,
                );
                let mut all_seqs = compacted_seqs;
                all_seqs.extend(extra_seqs);
                after_history_bytes = self
                    .projected_items
                    .iter()
                    .map(|i| i.estimated_bytes())
                    .sum();
                after_total_bytes = static_prefix_bytes + after_history_bytes;

                if after_total_bytes > budget.context_high_watermark_bytes {
                    // Gate 7: base64 image payloads are the one thing no text compaction can
                    // shrink, and the tail window protects the newest turns by construction. A
                    // valid multimodal `read` must therefore never abort the run: evict every
                    // image except the newest one (which the model still needs to see), and only
                    // if even that is too large drop it too, keeping the one-line summaries.
                    if self.evict_image_attachments(canonical, 1) > 0 {
                        after_history_bytes = self
                            .projected_items
                            .iter()
                            .map(|i| i.estimated_bytes())
                            .sum();
                        after_total_bytes = static_prefix_bytes + after_history_bytes;
                    }
                    if after_total_bytes > budget.context_high_watermark_bytes
                        && self.evict_image_attachments(canonical, 0) > 0
                    {
                        after_history_bytes = self
                            .projected_items
                            .iter()
                            .map(|i| i.estimated_bytes())
                            .sum();
                        after_total_bytes = static_prefix_bytes + after_history_bytes;
                    }
                }

                if after_total_bytes > budget.context_high_watermark_bytes {
                    return Err(TerminalReason::ContextBudgetExceeded);
                }

                compaction_event = Some(CompactionEventRecord {
                    before_bytes,
                    after_bytes: after_total_bytes,
                    compacted_seq_indices: all_seqs,
                    pinned_files,
                });
            } else {
                compaction_event = Some(CompactionEventRecord {
                    before_bytes,
                    after_bytes: after_total_bytes,
                    compacted_seq_indices: compacted_seqs,
                    pinned_files,
                });
            }
        }

        // Verify 1:1 tool_call_id pairing invariant across projected messages
        debug_assert!(verify_tool_call_pairing(&self.projected_items));
        let final_history_bytes: usize = self
            .projected_items
            .iter()
            .map(|i| i.estimated_bytes())
            .sum();

        let tail_start_idx = compute_tail_window_start_index(
            &self.projected_items,
            budget.context_min_tail_turns,
            budget.context_tail_window_bytes,
        );
        // Every breakpoint must index a real projected message: clamp into the closed interval
        // [0, len - 1] (which is just 0 for an empty projection).
        let max_message_index = self.projected_items.len().saturating_sub(1);
        let pinned_end_idx = self
            .last_pinned_baseline_end
            .min(tail_start_idx)
            .max(1.min(self.projected_items.len()))
            .min(max_message_index);
        // Staircase-quantize the tail window start to 8-message steps so the marker only moves once
        // per 8 appended messages, keeping the cached prefix stable between steps.
        let quantized_tail_start_idx = ((tail_start_idx / 8) * 8).min(max_message_index);

        let breakpoints = vec![
            CacheBreakpointMarker {
                breakpoint: CacheBreakpoint::ToolsEnd,
                message_index: 0,
            },
            CacheBreakpointMarker {
                breakpoint: CacheBreakpoint::SystemEnd,
                message_index: 0,
            },
            CacheBreakpointMarker {
                breakpoint: CacheBreakpoint::PinnedBaselineEnd,
                message_index: pinned_end_idx,
            },
            CacheBreakpointMarker {
                breakpoint: CacheBreakpoint::TailWindowStart,
                message_index: quantized_tail_start_idx,
            },
        ];

        // Deterministic per-category byte accounting (Gate 2). Every projected item lands in
        // exactly one bucket, so the breakdown is reproducible across fresh runs, `--resume`, and
        // `--fork` for the same canonical transcript and budget.
        let durable_last_touched = self.last_touched_turns.clone();
        let (pinned_indices, _) =
            compute_working_set_pins(&self.projected_items, current_turn, &durable_last_touched);
        let (system_prefix_bytes, tool_schema_bytes) =
            if self.prefix_system_bytes == 0 && self.prefix_tool_schema_bytes == 0 {
                // No explicit split was supplied (unit tests / direct projector use): report the
                // whole static prefix as the system prefix so the components still add up.
                (static_prefix_bytes, 0)
            } else {
                (self.prefix_system_bytes, self.prefix_tool_schema_bytes)
            };
        let mut breakdown = ContextBudgetBreakdown {
            system_prefix_bytes,
            tool_schema_bytes,
            history_bytes: final_history_bytes,
            tool_output_bytes: self
                .projected_items
                .iter()
                .filter_map(|item| match item {
                    TranscriptItem::ToolResult(tr) => {
                        Some(tr.content.len() + tr.image_attachment_bytes())
                    }
                    _ => None,
                })
                .sum(),
            recall_hit_count: self.recall_hit_count,
            recall_miss_count: self.recall_miss_count,
            ..Default::default()
        };
        for (idx, item) in self.projected_items.iter().enumerate() {
            let bytes = item.estimated_bytes();
            breakdown.total_projected_bytes += bytes;
            if self.compacted_indices.contains(&idx) {
                breakdown.compacted_stub_bytes += bytes;
            } else if idx >= tail_start_idx {
                breakdown.recent_tail_bytes += bytes;
            } else if pinned_indices.contains(&idx) {
                breakdown.pinned_working_set_bytes += bytes;
            }
        }
        breakdown.evidence_link_count = self.evidence_links.len();
        breakdown.tokens = breakdown.estimated_tokens();
        self.last_budget = breakdown;

        Ok(ProjectedContext {
            messages: self.projected_items.clone(),
            breakpoints,
            message_history_bytes: final_history_bytes,
            compaction_event,
        })
    }

    /// Gate 7 (`v1.2.0`): drop the base64 payloads of all but the `keep_newest` newest image tool
    /// results, keeping their one-line summaries in `content`. Returns the bytes reclaimed.
    ///
    /// The canonical transcript is cleared in lockstep so the shared payload is actually freed and
    /// a later `--resume` cannot resurrect megabytes the projection already discarded.
    fn evict_image_attachments(
        &mut self,
        canonical: &mut [TranscriptItem],
        keep_newest: usize,
    ) -> usize {
        let mut with_images: Vec<usize> = self
            .projected_items
            .iter()
            .enumerate()
            .filter_map(|(idx, item)| match item {
                TranscriptItem::ToolResult(tr) if !tr.meta.image_attachments.is_empty() => {
                    Some(idx)
                }
                _ => None,
            })
            .collect();
        if with_images.len() <= keep_newest {
            return 0;
        }
        with_images.truncate(with_images.len() - keep_newest);

        let mut reclaimed = 0usize;
        for idx in with_images {
            if let TranscriptItem::ToolResult(tr) = &mut self.projected_items[idx] {
                reclaimed += tr.image_attachment_bytes();
                tr.meta.image_attachments.clear();
            }
            if let Some(TranscriptItem::ToolResult(canon_tr)) = canonical.get_mut(idx) {
                canon_tr.meta.image_attachments.clear();
            }
        }
        reclaimed
    }

    fn run_step_down_compaction(
        &mut self,
        canonical: &mut [TranscriptItem],
        min_tail_turns: usize,
        tail_window_bytes: usize,
        target_history_bytes: usize,
        current_turn: usize,
    ) -> (Vec<usize>, Vec<String>) {
        let tail_start = compute_tail_window_start_index(
            &self.projected_items,
            min_tail_turns,
            tail_window_bytes,
        );
        // The persisted pinned baseline must never extend into (or past) the protected tail window.
        self.last_pinned_baseline_end = self.last_pinned_baseline_end.min(tail_start);
        let durable_last_touched = self.last_touched_turns.clone();
        let (pinned_item_indices, pinned_files) =
            compute_working_set_pins(&self.projected_items, current_turn, &durable_last_touched);

        let mut current_bytes: usize = self
            .projected_items
            .iter()
            .map(|i| i.estimated_bytes())
            .sum();
        let mut compacted_seqs = Vec::new();

        // Middle region: only scan `self.last_pinned_baseline_end.max(1)..tail_start`
        // so already-sent pinned baseline prefix items are NEVER mutated on subsequent compactions.
        let scan_start = self.last_pinned_baseline_end.max(1);
        if scan_start < tail_start {
            // Once an uncompacted, still-pinned hot file is met, stop advancing the baseline so the
            // file stays inside the rescan window and can still be compacted after its 8-turn
            // Working-Set Pinning window expires.
            let mut can_advance_baseline = true;
            for idx in scan_start..tail_start {
                if current_bytes <= target_history_bytes {
                    break;
                }
                if self.compacted_indices.contains(&idx) || pinned_item_indices.contains(&idx) {
                    if can_advance_baseline
                        && pinned_item_indices.contains(&idx)
                        && matches!(
                            &self.projected_items[idx],
                            TranscriptItem::ToolResult(tr)
                                if tr.content.len() + tr.image_attachment_bytes()
                                    > MIN_COMPACTABLE_TOOL_BYTES
                        )
                    {
                        can_advance_baseline = false;
                    }
                    if can_advance_baseline {
                        self.last_pinned_baseline_end = self.last_pinned_baseline_end.max(idx + 1);
                    }
                    continue;
                }
                if can_advance_baseline {
                    self.last_pinned_baseline_end = self.last_pinned_baseline_end.max(idx + 1);
                }

                // A later `read` of the same path supersedes this output only when it actually
                // covers the same lines, or when a successful `edit`/`write` on the file landed in
                // between. Paging through a file (1-200, then 201-400) is *not* supersession, so
                // it must not be flagged nor inflate `compaction_reread_count`. Computed from an
                // immutable borrow so the mutable borrow below stays short-lived.
                let superseded = match &self.projected_items[idx] {
                    TranscriptItem::ToolResult(tr) if tr.tool_name == "read" => {
                        let this_range = tr.meta.lines_range;
                        tr.meta.relative_path.as_deref().is_some_and(|path| {
                            self.projected_items[idx + 1..].iter().any(|later| {
                                let TranscriptItem::ToolResult(t) = later else {
                                    return false;
                                };
                                if !t.outcome.is_success()
                                    || t.meta.relative_path.as_deref() != Some(path)
                                {
                                    return false;
                                }
                                match t.tool_name.as_str() {
                                    // A successful mutation after this read invalidates it.
                                    "edit" | "write" => true,
                                    "read" => match (this_range, t.meta.lines_range) {
                                        (Some((start, end)), Some((later_start, later_end))) => {
                                            later_start <= start && later_end >= end
                                        }
                                        // Unknown range: fall back to the conservative reading.
                                        _ => true,
                                    },
                                    _ => false,
                                }
                            })
                        })
                    }
                    _ => false,
                };

                let mut raw_evidence: Option<Arc<str>> = None;
                let mut new_link: Option<EvidenceLink> = None;
                let mut read_path: Option<String> = None;
                let mut compacted_now = false;
                {
                    let tr = match &mut self.projected_items[idx] {
                        TranscriptItem::ToolResult(tr) => tr,
                        _ => continue,
                    };
                    // Gate 7: a multimodal `read` keeps only a ~70-byte summary in `content`, so
                    // testing `content.len()` alone would permanently pin multi-megabyte base64
                    // payloads inside the frozen prefix. The attachment bytes are part of the
                    // reclaimable cost.
                    let old_bytes = tr.content.len() + tr.image_attachment_bytes();
                    if old_bytes <= MIN_COMPACTABLE_TOOL_BYTES {
                        continue;
                    }
                    // Compact read-only outputs and large bash outputs (preserving exit code & tail)
                    let raw: Arc<str> = Arc::clone(&tr.content);
                    let sid = source_entry_id(idx, &tr.call_id.0);
                    let reason = if superseded {
                        CompactionReason::SupersededRead
                    } else {
                        CompactionReason::StepDownArchive
                    };
                    let records_evidence = self.profile.records_evidence();
                    let stub = build_outline_stub_for_profile(tr, idx, &sid, records_evidence);
                    let new_bytes = stub.len();
                    if new_bytes < old_bytes {
                        let stub_arc: Arc<str> = Arc::from(stub);
                        tr.content = Arc::clone(&stub_arc);
                        // Gate 7: the raw image payload is dropped with the raw text. Older turns keep
                        // only the one-line summary (already inside the stub) so the context budget
                        // does not keep paying for base64 megabytes, while recent turns still carry
                        // the multimodal block.
                        tr.meta.image_attachments.clear();
                        // Synchronize stub to canonical transcript so shared memory is freed (§7.1)
                        if let Some(TranscriptItem::ToolResult(canon_tr)) = canonical.get_mut(idx) {
                            canon_tr.content = stub_arc;
                            canon_tr.meta.image_attachments.clear();
                        }
                        read_path = if tr.tool_name == "read" {
                            tr.meta.relative_path.clone()
                        } else {
                            None
                        };
                        // `P1` runs the same compaction without minting an evidence pointer, so the
                        // raw bytes are dropped rather than retained for `recall_evidence`.
                        if records_evidence {
                            new_link = Some(EvidenceLink {
                                source_entry_id: sid,
                                canonical_index: idx,
                                call_id: tr.call_id.0.clone(),
                                tool_name: tr.tool_name.clone(),
                                relative_path: tr.meta.relative_path.clone(),
                                original_sha256: sha256_hex64(raw.as_bytes()),
                                original_bytes: old_bytes,
                                original_lines: raw.lines().count(),
                                stub_bytes: new_bytes,
                                reason,
                            });
                            raw_evidence = Some(raw);
                        }
                        compacted_now = true;
                        current_bytes = current_bytes.saturating_sub(old_bytes - new_bytes);
                    }
                }
                // Retain the raw bytes as retrievable evidence (Gate 2) before they are dropped
                // from the projected transcript. The item is marked compacted for every compacting
                // profile, so the byte breakdown and reread-regret accounting stay accurate even
                // when no evidence graph is recorded (`P1`).
                if compacted_now {
                    if let (Some(link), Some(raw)) = (new_link, raw_evidence) {
                        self.register_evidence(link, raw);
                    }
                    if let Some(path) = read_path {
                        self.compacted_file_paths.insert(path);
                    }
                    self.compacted_indices.insert(idx);
                    compacted_seqs.push(idx);
                }
            }
        }

        (compacted_seqs, pinned_files)
    }
}

/// Compute Working-Set Pinning (§7.1 V6-03):
/// Files modified by a successful `edit` or `write` within the last 8 turns have their
/// latest successful `read` result and corresponding `edit` diff exempted from compaction.
fn compute_working_set_pins(
    items: &[TranscriptItem],
    current_turn: usize,
    durable_last_touched: &HashMap<String, usize>,
) -> (HashSet<usize>, Vec<String>) {
    let mut turn_counter = 0usize;
    // Seed from the checkpoint, then reconcile against active transcript entries below.
    let mut file_last_modified_turn: HashMap<String, usize> = durable_last_touched.clone();
    let mut file_latest_read_idx: HashMap<String, usize> = HashMap::new();
    let mut file_latest_edit_idx: HashMap<String, usize> = HashMap::new();

    for (idx, item) in items.iter().enumerate() {
        match item {
            TranscriptItem::Assistant(_) => {
                turn_counter += 1;
            }
            TranscriptItem::ToolResult(tr) => {
                if !tr.outcome.is_success() {
                    continue;
                }
                let Some(ref path) = tr.meta.relative_path else {
                    continue;
                };
                match tr.tool_name.as_str() {
                    "edit" | "write" if tr.meta.modified_file => {
                        let slot = file_last_modified_turn
                            .entry(path.clone())
                            .or_insert(turn_counter);
                        *slot = (*slot).max(turn_counter);
                        file_latest_edit_idx.insert(path.clone(), idx);
                    }
                    "read" => {
                        file_latest_read_idx.insert(path.clone(), idx);
                    }
                    _ => {}
                }
            }
            TranscriptItem::User(_) => {}
        }
    }

    // The reference point is the absolute session turn (`current_turn`, supplied by the agent loop
    // as `session_turn_offset + relative_turn`). `turn_counter` is only a fallback for callers that
    // pass a turn at or before the transcript's own assistant count, so a short transcript can never
    // make an ancient modification look recent via unsigned underflow.
    let effective_current_turn = current_turn.max(turn_counter);
    let mut pinned_indices = HashSet::new();
    let mut pinned_files: Vec<String> = Vec::new();

    for (path, mod_turn) in file_last_modified_turn {
        if effective_current_turn >= mod_turn
            && effective_current_turn - mod_turn < WORKING_SET_PIN_TURNS
        {
            pinned_files.push(path.clone());
            if let Some(&read_idx) = file_latest_read_idx.get(&path) {
                pinned_indices.insert(read_idx);
            }
            if let Some(&edit_idx) = file_latest_edit_idx.get(&path) {
                pinned_indices.insert(edit_idx);
            }
        }
    }
    pinned_files.sort();
    (pinned_indices, pinned_files)
}

/// Compute the start message index of the protected tail window:
/// Keeps at least `min_tail_turns` Assistant turns and up to `tail_window_bytes`, never splitting an Assistant+ToolResult group.
fn compute_tail_window_start_index(
    items: &[TranscriptItem],
    min_tail_turns: usize,
    tail_window_bytes: usize,
) -> usize {
    if items.len() <= 1 {
        return items.len();
    }
    let mut assistant_turns_seen = 0usize;
    let mut accumulated_bytes = 0usize;
    let mut tail_start = items.len();

    for idx in (1..items.len()).rev() {
        let item_bytes = items[idx].estimated_bytes();
        if matches!(items[idx], TranscriptItem::Assistant(_)) {
            assistant_turns_seen += 1;
        }
        if assistant_turns_seen > min_tail_turns
            && accumulated_bytes + item_bytes > tail_window_bytes
        {
            break;
        }
        accumulated_bytes += item_bytes;
        tail_start = idx;
    }

    // Ensure we don't start mid-tool-result batch (must start at User or Assistant boundary)
    while tail_start > 1 && matches!(items[tail_start], TranscriptItem::ToolResult(_)) {
        tail_start -= 1;
    }

    tail_start
}

/// Build an Outline Stub (≤ 400B) containing 16-char SHA256 and top-level symbols with line numbers (§7.1 V6-03).
///
/// The header always carries `source_entry_id=<id>` plus a pointer to `recall_evidence`, so the
/// model can recover the raw lines that this stub replaced on demand (Gate 2).
pub fn build_outline_stub(tr: &ToolResultMessage, seq_idx: usize, source_entry_id: &str) -> String {
    build_outline_stub_for_profile(tr, seq_idx, source_entry_id, true)
}

/// Profile-aware stub builder: `P1` compacts without minting an evidence pointer, `P2`/`P3` embed
/// the `source_entry_id` and the `recall_evidence` hint.
pub fn build_outline_stub_for_profile(
    tr: &ToolResultMessage,
    seq_idx: usize,
    source_entry_id: &str,
    include_evidence: bool,
) -> String {
    let size_kib = (tr.content.len() as f64) / 1024.0;
    let path = tr.meta.relative_path.as_deref().unwrap_or(".");
    let sha = tr.meta.content_sha256.as_deref().unwrap_or("none");
    let evidence = if include_evidence {
        format!(
            " | source_entry_id={} | call recall_evidence(source_entry_id={})",
            source_entry_id, source_entry_id
        )
    } else {
        String::new()
    };

    if tr.tool_name == "bash" {
        let exit_code = tr.meta.exit_code.unwrap_or(0);
        let tail_lines: Vec<&str> = tr.content.lines().rev().take(15).collect();
        let tail_joined = tail_lines.into_iter().rev().collect::<Vec<_>>().join(" | ");
        let head = format!(
            "[Compacted: bash(exit_code={}, duration={}ms, {:.1} KiB, seq={}){} | Tail: ",
            exit_code, tr.meta.duration_ms, size_kib, seq_idx, evidence
        );
        return append_bounded_tail(head, &tail_joined);
    }

    let (start_l, end_l) = tr
        .meta
        .lines_range
        .unwrap_or((1, tr.content.lines().count()));
    let outline_items = extract_top_level_symbols_from(&tr.content, MAX_OUTLINE_SYMBOLS, start_l);
    let outline_str = if outline_items.is_empty() {
        "none".to_string()
    } else {
        outline_items.join(", ")
    };

    let head = format!(
        "[Compacted: {}({}, lines {}-{}, {:.1} KiB, sha256={}, seq={}){} | Outline: ",
        tr.tool_name, path, start_l, end_l, size_kib, sha, seq_idx, evidence
    );
    append_bounded_tail(head, &outline_str)
}

/// Append the variable-length payload (symbol outline / bash tail) after a fixed header, truncating
/// **only the payload**. Truncating the assembled stub instead could slice through `source_entry_id=`
/// and break WAL evidence recovery on `--resume` (a long `relative_path` or `call_id` is enough).
fn append_bounded_tail(head: String, tail: &str) -> String {
    let budget = MAX_OUTLINE_STUB_BYTES.saturating_sub(head.len() + 4);
    let cut = crate::tool::output_buffer::floor_char_boundary(tail, budget);
    let mut stub = head;
    stub.push_str(&tail[..cut]);
    if cut < tail.len() {
        stub.push_str("...");
    }
    stub.push(']');
    stub
}

fn symbol_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // Rust / TypeScript / Go / Python top-level declarations. An optional `export`/`default`
        // /`declare`/`pub(..)`/`const`/`static`/`async`/`unsafe`/`extern ".."` prefix is tolerated,
        // as are Go method receivers and generic parameter lists before the symbol name.
        Regex::new(
            r#"^(?:\s*(\d+)\t)?\s*(?:export\s+)?(?:default\s+)?(?:declare\s+)?(?:pub(?:\([^)]*\))?\s+)?(?:const\s+)?(?:static\s+)?(?:async\s+)?(?:unsafe\s+)?(?:extern\s+"[^"]*"\s+)?(fn|func|function|struct|enum|impl|trait|class|def|interface|type|mod|namespace|union|const|static|var)\s+(?:<[^>]*>\s*)?(?:\([^)]*\)\s+)?([A-Za-z_][A-Za-z0-9_]*)"#,
        )
        .unwrap()
    })
}

/// Backwards-compatible wrapper: extract symbols assuming the content starts at line 1.
pub fn extract_top_level_symbols(content: &str, max_symbols: usize) -> Vec<String> {
    extract_top_level_symbols_from(content, max_symbols, 1)
}

/// Extract up to `max_symbols` top-level declarations, rendering real line numbers.
///
/// A `read` result's line-number prefix (`    12\t`) is authoritative when present; otherwise the
/// absolute line number is derived from `start_line` (the first line reported by `lines_range`)
/// plus the physical line index. Function-like declarations also carry the first 80 bytes of
/// their signature so callers can still reason about parameters after the body was compacted.
fn extract_top_level_symbols_from(
    content: &str,
    max_symbols: usize,
    start_line: usize,
) -> Vec<String> {
    let re = symbol_regex();
    let mut symbols = Vec::new();

    for (idx, line) in content.lines().enumerate() {
        if symbols.len() >= max_symbols {
            break;
        }
        if let Some(caps) = re.captures(line) {
            let line_num = caps
                .get(1)
                .and_then(|m| m.as_str().trim().parse::<usize>().ok())
                .unwrap_or_else(|| start_line + idx);
            let kind = caps.get(2).map(|m| m.as_str()).unwrap_or("");
            let name = caps.get(3).map(|m| m.as_str()).unwrap_or("");
            let symbol = if matches!(kind, "fn" | "func" | "function" | "def") {
                let kind_start = caps.get(2).map(|m| m.start()).unwrap_or(0);
                let signature = line[kind_start..]
                    .split('{')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .trim_end_matches(':')
                    .trim();
                let signature = if signature.len() > 80 {
                    let cut = crate::tool::output_buffer::floor_char_boundary(signature, 80);
                    &signature[..cut]
                } else {
                    signature
                };
                format!("L{} {}", line_num, signature)
            } else {
                format!("L{} {} {}", line_num, kind, name)
            };
            symbols.push(symbol);
        }
    }
    symbols
}

pub fn verify_tool_call_pairing(items: &[TranscriptItem]) -> bool {
    validate_tool_call_pairing(items).is_ok()
}

/// Gate 2 hard-constraint validator: every `Assistant` tool call must be answered by exactly one
/// `ToolResult` with the same `call_id`, in order, before the next `Assistant`/`User` message.
///
/// Returns a human-readable description of the first violation, so the A/B kroma can assert the
/// invariant across compaction boundaries with a diagnostic instead of a bare `false`.
pub fn validate_tool_call_pairing(items: &[TranscriptItem]) -> Result<(), String> {
    fn ids(calls: &[ToolCallId]) -> Vec<&str> {
        calls.iter().map(|c| c.0.as_str()).collect()
    }
    fn names(calls: &[ToolCallId]) -> Vec<String> {
        calls.iter().map(|c| c.0.clone()).collect()
    }
    let mut expected_ids: Vec<ToolCallId> = Vec::new();
    let mut seen_ids: Vec<ToolCallId> = Vec::new();

    for (index, item) in items.iter().enumerate() {
        match item {
            TranscriptItem::Assistant(a) => {
                if expected_ids != seen_ids {
                    return Err(format!(
                        "message {}: expected tool results {:?} before the next Assistant turn, observed {:?}",
                        index,
                        ids(&expected_ids),
                        ids(&seen_ids)
                    ));
                }
                expected_ids = a.tool_calls.iter().map(|tc| tc.call_id.clone()).collect();
                seen_ids.clear();
            }
            TranscriptItem::ToolResult(tr) => seen_ids.push(tr.call_id.clone()),
            TranscriptItem::User(_) => {
                if expected_ids != seen_ids {
                    return Err(format!(
                        "message {}: expected tool results {:?} before the next User message, observed {:?}",
                        index,
                        ids(&expected_ids),
                        ids(&seen_ids)
                    ));
                }
                expected_ids.clear();
                seen_ids.clear();
            }
        }
    }
    if expected_ids != seen_ids {
        return Err(format!(
            "transcript tail: expected tool results {:?}, observed {:?}",
            names(&expected_ids),
            names(&seen_ids)
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::budget::BudgetConfig;
    use crate::messages::{
        AssistantTurn, FinishReason, ToolCall, ToolOutcome, ToolResultMessage, ToolResultMeta,
        UserMessage,
    };

    fn assistant_with(call_ids: &[String]) -> TranscriptItem {
        TranscriptItem::Assistant(AssistantTurn {
            content: None,
            reasoning_blocks: Vec::new(),
            tool_calls: call_ids
                .iter()
                .map(|id| ToolCall {
                    call_id: ToolCallId::new(id.as_str()),
                    name: "read".into(),
                    raw_arguments: "{}".into(),
                })
                .collect(),
            finish_reason: FinishReason::ToolCalls,
            usage: None,
        })
    }

    fn read_result(call_id: &str, path: &str, body: String) -> TranscriptItem {
        TranscriptItem::ToolResult(ToolResultMessage {
            call_id: ToolCallId::new(call_id),
            tool_name: "read".into(),
            outcome: ToolOutcome::Success,
            content: Arc::from(body),
            meta: ToolResultMeta {
                relative_path: Some(path.into()),
                ..Default::default()
            },
        })
    }

    fn edit_result(call_id: &str, path: &str) -> TranscriptItem {
        TranscriptItem::ToolResult(ToolResultMessage {
            call_id: ToolCallId::new(call_id),
            tool_name: "edit".into(),
            outcome: ToolOutcome::Success,
            content: Arc::from(format!("[Edited: {}]", path)),
            meta: ToolResultMeta {
                relative_path: Some(path.into()),
                modified_file: true,
                ..Default::default()
            },
        })
    }

    fn find_read<'a>(items: &'a [TranscriptItem], path: &str) -> &'a ToolResultMessage {
        items
            .iter()
            .find_map(|item| match item {
                TranscriptItem::ToolResult(tr)
                    if tr.tool_name == "read" && tr.meta.relative_path.as_deref() == Some(path) =>
                {
                    Some(tr)
                }
                _ => None,
            })
            .expect("expected a read tool result for the path")
    }

    fn large_source(header: &str, pad: &str) -> String {
        let mut body = String::from(header);
        body.push_str(&pad.repeat(220));
        body
    }

    /// A resumed session reaches absolute turn 13 while the projected transcript only carries a
    /// single assistant turn. The durable checkpoint ledger records `old.rs` at turn 1 (12 turns
    /// ago) and `recent.rs` at turn 10 (3 turns ago); only the latter may remain Working-Set pinned.
    #[test]
    fn working_set_pins_release_files_older_than_the_window_under_a_resume_offset() {
        let items = vec![
            TranscriptItem::User(UserMessage {
                content: "task".into(),
            }),
            assistant_with(&["call_old_read".to_string(), "call_old_edit".to_string()]),
            read_result(
                "call_old_read",
                "src/old.rs",
                large_source("old\n", "pad\n"),
            ),
            edit_result("call_old_edit", "src/old.rs"),
        ];
        let mut ledger = HashMap::new();
        ledger.insert("src/old.rs".to_string(), 1usize);
        ledger.insert("src/recent.rs".to_string(), 10usize);

        let (_, pinned) = compute_working_set_pins(&items, 13, &ledger);
        assert!(
            pinned.contains(&"src/recent.rs".to_string()),
            "recent.rs (3 turns old) must stay pinned"
        );
        assert!(
            !pinned.contains(&"src/old.rs".to_string()),
            "old.rs (12 turns old) must be released from pinning"
        );
    }

    /// `refresh_last_touched_from_transcript` must not overwrite a larger turn restored from the
    /// session checkpoint with the transcript-local assistant count.
    #[test]
    fn refresh_last_touched_never_regresses_a_larger_checkpoint_turn() {
        let mut projector = ContextProjector::new();
        projector.restore_last_touched(vec![("src/old.rs".to_string(), 9)]);
        let mut canonical = vec![
            TranscriptItem::User(UserMessage {
                content: "task".into(),
            }),
            assistant_with(&["call_old_edit".to_string()]),
            edit_result("call_old_edit", "src/old.rs"),
        ];
        let budget = BudgetConfig::default();
        let _ = projector
            .project(&mut canonical, &budget, 0, None, 13)
            .unwrap();
        assert_eq!(projector.last_touched_turns().get("src/old.rs"), Some(&9));
    }

    /// End-to-end resume scenario: turn 1 edits `old.rs`, turns 2-12 happen, and the 13th turn
    /// triggers high-watermark compaction. `old.rs` (over 8 turns stale) must be compacted to an
    /// Outline Stub while `recent.rs` (edited in turn 10) keeps its Working-Set Pinning exemption.
    #[test]
    fn resumed_compaction_releases_stale_hot_file_and_keeps_recent_file_pinned() {
        let budget = BudgetConfig {
            context_high_watermark_bytes: 64 * 1024,
            context_low_watermark_bytes: 32 * 1024,
            context_tail_window_bytes: 2 * 1024,
            context_min_tail_turns: 1,
            ..Default::default()
        };

        let old_body = large_source(
            "[File: src/old.rs | Lines: 1-200 of 200 | SHA256: 00000000000000aa]\n",
            "   20\t// old padding line\n",
        );
        let recent_body = large_source(
            "[File: src/recent.rs | Lines: 1-200 of 200 | SHA256: 00000000000000bb]\n",
            "   30\t// recent padding line\n",
        );

        let mut canonical = vec![TranscriptItem::User(UserMessage {
            content: "task".into(),
        })];
        // Turn 1: read + edit the soon-to-be-stale file.
        canonical.push(assistant_with(&[
            "call_old_read".to_string(),
            "call_old_edit".to_string(),
        ]));
        canonical.push(read_result("call_old_read", "src/old.rs", old_body.clone()));
        canonical.push(edit_result("call_old_edit", "src/old.rs"));
        // Turns 2-9: filler reads that push the request over the high watermark.
        for turn in 2..=9 {
            let read_id = format!("call_filler_read_{}", turn);
            canonical.push(assistant_with(&[read_id.clone()]));
            canonical.push(read_result(
                &read_id,
                &format!("src/filler{}.rs", turn),
                large_source(
                    &format!("[File: src/filler{}.rs | Lines: 1-200 of 200]\n", turn),
                    &format!("   40\t// filler {} padding line\n", turn),
                ),
            ));
        }
        // Turn 10: read + edit the recent file.
        canonical.push(assistant_with(&[
            "call_recent_read".to_string(),
            "call_recent_edit".to_string(),
        ]));
        canonical.push(read_result(
            "call_recent_read",
            "src/recent.rs",
            recent_body.clone(),
        ));
        canonical.push(edit_result("call_recent_edit", "src/recent.rs"));
        // Turns 11-12: more filler so the resumed run lands on absolute turn 13.
        for turn in 11..=12 {
            let read_id = format!("call_filler_read_{}", turn);
            canonical.push(assistant_with(&[read_id.clone()]));
            canonical.push(read_result(
                &read_id,
                &format!("src/filler{}.rs", turn),
                large_source(
                    &format!("[File: src/filler{}.rs | Lines: 1-200 of 200]\n", turn),
                    &format!("   40\t// filler {} padding line\n", turn),
                ),
            ));
        }

        let mut projector = ContextProjector::new();
        let projected = projector
            .project(&mut canonical, &budget, 1000, None, 13)
            .expect("resumed compaction should succeed");
        assert!(
            projected.compaction_event.is_some(),
            "high watermark should trigger compaction"
        );

        let old_read = find_read(&projected.messages, "src/old.rs");
        assert!(
            old_read.content.starts_with("[Compacted: read(src/old.rs"),
            "stale old.rs read should be replaced by an Outline Stub, got: {}",
            &old_read.content[..old_read.content.len().min(80)]
        );
        let recent_read = find_read(&projected.messages, "src/recent.rs");
        assert_eq!(
            recent_read.content.as_ref(),
            recent_body.as_str(),
            "recent.rs (edited in turn 10) must keep its Working-Set Pinning exemption"
        );
    }
}
