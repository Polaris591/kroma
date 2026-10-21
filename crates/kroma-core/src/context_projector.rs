use crate::budget::BudgetConfig;
use crate::cache_aware_compaction::{choose_compaction, CompactionChoice, CompactionSignals};
use crate::messages::{
    CacheBreakpoint, CacheBreakpointMarker, CompactionMetadata, ToolResultMessage, TranscriptItem,
};
use crate::prompt::sha256_hex64;
use crate::types::{TerminalReason, ToolCallId};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock};

pub const MIN_COMPACTABLE_TOOL_BYTES: usize = 1024; // > 1 KiB
/// Capability gap #3 (`v1.5.2`): largest latest read that a causal citation may pin. The size cap
/// keeps the heuristic from ever pushing a projection past its high watermark.
pub const CAUSAL_PIN_MAX_BYTES: usize = 4096;
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
    /// Capability gap #3 (`v1.5.2`): a failed attempt collapsed into a one-line negative lesson.
    DeadEnd,
}

/// Deterministic, replay-stable identifier for the raw evidence behind an Outline Stub.
///
/// Built from the canonical transcript index plus the tool `call_id`, both of which survive
/// `--resume` and `--fork` unchanged for every retained (prefix) item, so the same stub always
/// carries the same id across processes.
pub fn source_entry_id(canonical_index: usize, call_id: &str) -> String {
    format!("ev-{:04}-{}", canonical_index, call_id)
}

/// T05 (KR-04): is `content` exactly the host's own stub shape for `source_entry_id`?
///
/// A bare `source_entry_id=...` substring is **not** enough: source code, logs and tool output can
/// contain one. The host stub always starts with `[Compacted: ` or `[Dead-end: ` and carries the
/// pointer in its exact rendered form. This is the only text-level check trusted anywhere, and it is
/// still only a *cross-check*: the authority is the branch-scoped projection overlay.
pub fn is_host_stub_header(content: &str, source_entry_id: &str) -> bool {
    if source_entry_id.is_empty() {
        return false;
    }
    if !(content.starts_with("[Compacted: ") || content.starts_with("[Dead-end: ")) {
        return false;
    }
    let pointer = format!(
        " | source_entry_id={id} | call recall_evidence(source_entry_id={id})",
        id = source_entry_id
    );
    content.contains(&pointer)
}

/// Parse the `source_entry_id=<id>` field back out of a stub header.
///
/// T05: display/telemetry only. Nothing may grant trust from this alone — see
/// [`is_host_stub_header`].
pub fn extract_source_entry_id(content: &str) -> Option<String> {
    let start = content.find("source_entry_id=")? + "source_entry_id=".len();
    let rest = &content[start..];
    let end = rest.find([' ', '|', ']', ')', ',']).unwrap_or(rest.len());
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

    /// C00-03: the four legacy ids are the baseline's public names and keep their meaning.
    /// A new algorithm is addressed by its own `ctx.*` id instead of being squeezed into a `p`
    /// number, so an experiment can never be mistaken for a configuration a user already has.
    pub const LEGACY_IDS: [&'static str; 4] = ["baseline", "p1", "p2", "p3"];

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
    /// Capability gap #3 (`v1.5.2`): failed tool results collapsed into a negative-lesson stub.
    pub dead_end_compactions: u64,
    /// Capability gap #3: files pinned because a later assistant turn cited them (causal value).
    pub causal_pins: u64,
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
    /// KR-J03: shadow economic decisions recorded for this run (advice only; nothing reads them back
    /// as control flow, and `applied` is always false).
    economy_decisions: Vec<crate::shadow_economy::EconomyDecision>,
    /// KR-J03 planner configuration; defaults reproduce the shipped heuristic.
    economy_config: crate::shadow_economy::EconomyConfig,
    last_pinned_baseline_end: usize,
    last_touched_turns: HashMap<String, usize>,
    /// Raw tool output retained for `recall_evidence`, keyed by `source_entry_id`.
    evidence: HashMap<String, Arc<str>>,
    /// T15 (KR-11): durable artifact layer for evicted evidence. Memory is a cache; this is the
    /// record. `None` keeps the historical behaviour (evicted bytes are simply gone).
    evidence_store: Option<std::sync::Arc<crate::evidence_store::EvidenceStore>>,
    evidence_session: String,
    evidence_branch: String,
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
    /// Capability gap #3 (`v1.5.2`): opt-in causal citation pinning + dead-end negative-sample
    /// compression. Off by default so the experimental mechanism is measured by its own A/B run.
    causal_pruning: bool,
    /// Capability gap #3: count of dead-end tool results collapsed into a negative-lesson stub.
    dead_end_compactions: u64,
    /// Capability gap #3: files pinned because a later assistant turn explicitly cited them.
    causal_pins: u64,
    /// Measured provider cache reads from the previous request. Never infer from a byte estimate.
    measured_cache_hits: Option<u64>,
    /// Trusted runtime option; defaults off to preserve the e95845b projection contract.
    cache_aware_compaction: bool,
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
            economy_decisions: Vec::new(),
            economy_config: crate::shadow_economy::EconomyConfig::default(),
            last_pinned_baseline_end: 0,
            last_touched_turns: HashMap::new(),
            evidence: HashMap::new(),
            evidence_store: None,
            evidence_session: String::new(),
            evidence_branch: String::new(),
            evidence_bytes: 0,
            evidence_call_index: HashMap::new(),
            evidence_links: Vec::new(),
            last_budget: ContextBudgetBreakdown::default(),
            prefix_system_bytes: 0,
            prefix_tool_schema_bytes: 0,
            recall_hit_count: 0,
            recall_miss_count: 0,
            causal_pruning: false,
            dead_end_compactions: 0,
            causal_pins: 0,
            measured_cache_hits: None,
            cache_aware_compaction: false,
            profile: ProjectionProfile::default(),
        }
    }

    /// Capability gap #3 (`v1.5.2`): enable causal citation pinning and dead-end negative-sample
    /// compression. Off by default; the ablation switch for the paired benchmark.
    pub fn with_causal_pruning(mut self, enabled: bool) -> Self {
        self.causal_pruning = enabled;
        self
    }

    /// Capability gap #3: whether the experimental causal-pruning path is active.
    pub fn causal_pruning(&self) -> bool {
        self.causal_pruning
    }

    /// Opt-in proactive compaction. Kept separate from mandatory high-watermark compaction.
    pub fn with_cache_aware_compaction(mut self, enabled: bool) -> Self {
        self.cache_aware_compaction = enabled;
        self
    }

    /// Store *measured* prompt-cache reuse from the provider. `None` is unknown, not a cold cache.
    pub fn observe_cache_usage(&mut self, cached_input_tokens: Option<u64>) {
        self.measured_cache_hits = cached_input_tokens;
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

    /// KR-J03: the shadow planner's decisions for this run.
    pub fn economy_decisions(&self) -> &[crate::shadow_economy::EconomyDecision] {
        &self.economy_decisions
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
            dead_end_compactions: self.dead_end_compactions,
            causal_pins: self.causal_pins,
        }
    }

    /// T15 (KR-11): attach the durable artifact store. `session_id` and `branch_id` become part of
    /// every artifact key, so a fork cannot read its sibling's evidence and a resumed session reads
    /// exactly the objects it wrote.
    pub fn with_evidence_store(
        mut self,
        store: std::sync::Arc<crate::evidence_store::EvidenceStore>,
        session_id: impl Into<String>,
        branch_id: impl Into<String>,
    ) -> Self {
        self.evidence_store = Some(store);
        self.evidence_session = session_id.into();
        self.evidence_branch = branch_id.into();
        self
    }

    fn artifact_key(&self, event_id: &str, sha256: &str) -> crate::evidence_store::ArtifactKey {
        crate::evidence_store::ArtifactKey::new(
            &self.evidence_session,
            &self.evidence_branch,
            event_id,
            sha256,
        )
    }

    /// T15: availability of an evidence id, including the durable layer.
    ///
    /// `None` means "no durable layer is attached and the bytes are not in memory", which is the
    /// pre-T15 behaviour and stays distinguishable from an explicit `Gone`.
    pub fn evidence_status(
        &self,
        source_entry_id: &str,
    ) -> Option<crate::evidence_store::ArtifactStatus> {
        if self.evidence.contains_key(source_entry_id) {
            return None;
        }
        let canonical = self
            .evidence_call_index
            .get(source_entry_id)
            .cloned()
            .unwrap_or_else(|| source_entry_id.to_string());
        let link = self.evidence_link(&canonical)?;
        let store = self.evidence_store.as_ref()?;
        Some(store.get(&self.artifact_key(&link.source_entry_id, &link.original_sha256)))
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
                // T15 (KR-11): eviction from memory is not deletion. Hand the bytes to the durable
                // layer first, keyed by event + content hash, so `recall_evidence` can still produce
                // them (and can report Gone/Corrupt/Forbidden instead of guessing).
                if let (Some(store), Some(link)) = (
                    self.evidence_store.clone(),
                    self.evidence_links
                        .iter()
                        .find(|l| l.source_entry_id == id)
                        .cloned(),
                ) {
                    let key = self.artifact_key(&link.source_entry_id, &link.original_sha256);
                    let _ = store.put(&key, &dropped);
                }
            }
        }
    }

    /// Look up raw evidence by `source_entry_id` or by the originating tool `call_id`.
    pub fn evidence_lookup(&self, id: &str) -> Option<Arc<str>> {
        if let Some(raw) = self.evidence.get(id) {
            return Some(Arc::clone(raw));
        }
        let canonical = self
            .evidence_call_index
            .get(id)
            .cloned()
            .unwrap_or_else(|| id.to_string());
        if let Some(raw) = self.evidence.get(&canonical) {
            return Some(Arc::clone(raw));
        }
        // T15 (KR-11): a memory miss falls through to the durable artifact, verified against the
        // content hash recorded when it was captured. It never re-reads the workspace file: an edited
        // file is not the evidence that was captured.
        if let Some(crate::evidence_store::ArtifactStatus::Available { text, .. }) =
            self.evidence_status(&canonical)
        {
            return Some(Arc::from(text.as_str()));
        }
        None
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

    /// T03 (KR-03): bytes of the context that would be sent right now, *before* a projection step.
    /// Used to estimate token pressure when the provider has not reported usage yet.
    pub fn projected_bytes(&self) -> usize {
        self.projected_items
            .iter()
            .map(TranscriptItem::estimated_bytes)
            .sum()
    }

    /// Invariant: a projection may summarize tool results, but it must not rewrite user asks,
    /// assistant decisions, tool IDs or result identities. Check every request, not only debug.
    pub fn authoritative_history_preserved(&self, canonical: &[TranscriptItem]) -> bool {
        if self.projected_items.len() != canonical.len() {
            return false;
        }
        self.projected_items
            .iter()
            .zip(canonical)
            .all(|(projected, original)| match (projected, original) {
                (TranscriptItem::ToolResult(a), TranscriptItem::ToolResult(b)) => {
                    a.call_id == b.call_id && a.tool_name == b.tool_name && a.outcome == b.outcome
                }
                (a, b) => a == b,
            })
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
                // Restore only host-issued, typed projection metadata. Tool text is untrusted:
                // merely reading Kroma's source containing `source_entry_id=` must NOT mark
                // an ordinary source file as archived or suppress its future compaction.
                if let TranscriptItem::ToolResult(tr) = item {
                    if tr.meta.compaction.as_ref().is_some_and(|meta| {
                        meta.schema_version == 1
                            && meta.source_entry_id == source_entry_id(idx, tr.call_id.as_str())
                    }) {
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
        // Proactive planning must not treat multimodal attachments, existing archive stubs,
        // failed tools, or tiny output as reclaimable. The hard byte budget ABOVE still counts
        // every byte, including images; only the optional cache-economics heuristic changes.
        let reclaimable_tool_bytes: usize = self
            .projected_items
            .iter()
            .filter_map(|item| match item {
                TranscriptItem::ToolResult(tr)
                    if tr.outcome.is_success()
                        && tr.meta.compaction.is_none()
                        && tr.meta.image_attachments.is_empty()
                        && tr.content.len() >= MIN_COMPACTABLE_TOOL_BYTES =>
                {
                    Some(tr.content.len())
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
        let mandatory_compaction = byte_exceeded || token_exceeded;
        let proactive_compaction = self.cache_aware_compaction
            && self.profile.compacts()
            && choose_compaction(CompactionSignals {
                total_request_bytes,
                high_watermark_bytes: budget.context_high_watermark_bytes,
                raw_tool_bytes: reclaimable_tool_bytes,
                measured_cached_input_tokens: self.measured_cache_hits,
                hard_pressure: mandatory_compaction,
            }) == CompactionChoice::Proactive;

        if self.profile.compacts() && (mandatory_compaction || proactive_compaction) {
            let before_bytes = total_request_bytes;
            // A speculative compaction stops halfway between low/high watermarks instead
            // of paying the full cache rewrite cost of an aggressive hard-pressure trim.
            let target_watermark = if proactive_compaction && !mandatory_compaction {
                budget.context_low_watermark_bytes / 2 + budget.context_high_watermark_bytes / 2
            } else {
                budget.context_low_watermark_bytes
            };
            let target_low_bytes = target_watermark.saturating_sub(static_prefix_bytes);

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

        // Compaction is allowed to archive *tool outputs only*. User instructions and every
        // assistant/tool-call plan are authoritative history, even if they look verbose. Fail
        // closed instead of dispatching a model request with silently modified constraints.
        if !self.authoritative_history_preserved(canonical) {
            return Err(TerminalReason::ProviderProtocolError(
                "context compaction changed authoritative user/assistant history".into(),
            ));
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
        let (pinned_indices, _) = {
            let (pins, files, _causal) = compute_working_set_pins(
                &self.projected_items,
                current_turn,
                &durable_last_touched,
                None,
            );
            (pins, files)
        };
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
        // Capability gap #3 (`v1.5.2`): only compute the causal citation graph when the optional
        // pruning path is enabled, so the default projection is byte-for-byte unchanged.
        let causal_citations = if self.causal_pruning {
            Some(compute_causal_citations(&self.projected_items))
        } else {
            None
        };
        let (pinned_item_indices, pinned_files, causal_pins) = compute_working_set_pins(
            &self.projected_items,
            current_turn,
            &durable_last_touched,
            causal_citations.as_ref(),
        );
        self.causal_pins = self.causal_pins.saturating_add(causal_pins as u64);

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
                    } else if self.causal_pruning && !tr.outcome.is_success() {
                        // Capability gap #3: a failed attempt is a dead end; collapse it into a
                        // negative lesson instead of an outline of a file that was never read.
                        CompactionReason::DeadEnd
                    } else {
                        CompactionReason::StepDownArchive
                    };
                    let records_evidence = self.profile.records_evidence();
                    let stub = if reason == CompactionReason::DeadEnd {
                        build_dead_end_stub(tr, idx, &sid, records_evidence)
                    } else {
                        build_outline_stub_for_profile(tr, idx, &sid, records_evidence)
                    };
                    let new_bytes = stub.len();
                    if new_bytes < old_bytes {
                        let stub_arc: Arc<str> = Arc::from(stub);
                        tr.content = Arc::clone(&stub_arc);
                        tr.meta.compaction = Some(CompactionMetadata {
                            schema_version: 1,
                            source_entry_id: sid.clone(),
                            original_sha256: sha256_hex64(raw.as_bytes()),
                            reason: match reason {
                                CompactionReason::StepDownArchive => "step_down_archive",
                                CompactionReason::SupersededRead => "superseded_read",
                                CompactionReason::DeadEnd => "dead_end",
                            }
                            .into(),
                            // T05 (KR-04): the raw bytes were just registered in the evidence
                            // store, so the pointer is honest at the moment it is written.
                            availability: if records_evidence {
                                crate::messages::CompactionAvailability::Available
                            } else {
                                crate::messages::CompactionAvailability::Unknown
                            },
                        });
                        // Gate 7: the raw image payload is dropped with the raw text. Older turns keep
                        // only the one-line summary (already inside the stub) so the context budget
                        // does not keep paying for base64 megabytes, while recent turns still carry
                        // the multimodal block.
                        tr.meta.image_attachments.clear();
                        // Synchronize stub to canonical transcript so shared memory is freed (§7.1)
                        if let Some(TranscriptItem::ToolResult(canon_tr)) = canonical.get_mut(idx) {
                            canon_tr.content = stub_arc;
                            canon_tr.meta.compaction = tr.meta.compaction.clone();
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
                        // KR-J03: record what the shadow planner would have said about this item.
                        // The window check that forced this compaction already ran upstream, which is
                        // why HardWindow can appear here as the reason and still never be overridden.
                        let window_remaining = target_history_bytes.saturating_sub(current_bytes);
                        self.economy_decisions.push(crate::shadow_economy::plan(
                            &self.economy_config,
                            old_bytes,
                            current_turn.saturating_sub(idx),
                            window_remaining,
                        ));
                        if reason == CompactionReason::DeadEnd {
                            self.dead_end_compactions = self.dead_end_compactions.saturating_add(1);
                        }
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

/// Collect the assistant `content` + tool-call arguments as one searchable string, so a path that
/// appears only in a tool call (for example `edit {"path":"src/a.rs"}`) still counts as a citation.
fn assistant_reference_text(item: &TranscriptItem) -> Option<String> {
    let TranscriptItem::Assistant(turn) = item else {
        return None;
    };
    let mut text = String::new();
    if let Some(content) = &turn.content {
        text.push_str(content);
    }
    for call in &turn.tool_calls {
        text.push_str(&call.raw_arguments);
        text.push(' ');
    }
    Some(text)
}

/// Capability gap #3 (`v1.5.2`): map each workspace-relative path to the last assistant turn that
/// actually *cited* it (mentioned it in prose or referenced it in a tool call). This is the causal
/// value signal that separates "a file the model keeps reasoning about" from "a file that happens
/// to have been touched recently".
fn compute_causal_citations(items: &[TranscriptItem]) -> HashMap<String, usize> {
    let known_paths: HashSet<String> = items
        .iter()
        .filter_map(|item| match item {
            TranscriptItem::ToolResult(tr) => tr.meta.relative_path.clone(),
            _ => None,
        })
        .collect();
    let mut citations: HashMap<String, usize> = HashMap::new();
    let mut turn = 0usize;
    for item in items {
        let Some(text) = assistant_reference_text(item) else {
            continue;
        };
        turn += 1;
        for path in &known_paths {
            if text.contains(path.as_str()) {
                citations.insert(path.clone(), turn);
            }
        }
    }
    citations
}

/// Compute Working-Set Pinning (§7.1 V6-03):
/// Files modified by a successful `edit` or `write` within the last 8 turns have their
/// latest successful `read` result and corresponding `edit` diff exempted from compaction.
///
/// Capability gap #3 (`v1.5.2`): when `causal_citations` is `Some`, a file that a later assistant
/// turn explicitly cited is pinned as well, even if it was not modified recently. The return value
/// includes how many files were pinned *only* by causal citation, for telemetry.
fn compute_working_set_pins(
    items: &[TranscriptItem],
    current_turn: usize,
    durable_last_touched: &HashMap<String, usize>,
    causal_citations: Option<&HashMap<String, usize>>,
) -> (HashSet<usize>, Vec<String>, usize) {
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

    for (path, mod_turn) in &file_last_modified_turn {
        if effective_current_turn >= *mod_turn
            && effective_current_turn - mod_turn < WORKING_SET_PIN_TURNS
        {
            pinned_files.push(path.clone());
            if let Some(&read_idx) = file_latest_read_idx.get(path) {
                pinned_indices.insert(read_idx);
            }
            if let Some(&edit_idx) = file_latest_edit_idx.get(path) {
                pinned_indices.insert(edit_idx);
            }
        }
    }

    // Capability gap #3: a file the assistant cited after touching it stays pinned even once its
    // modification is older than the working-set window, because the model is still reasoning about
    // it. Count only the files the working-set rule did not already pin. A citation pin is capped by
    // size so a huge cited file can never push the projection over its high watermark — the budget
    // invariant outranks the heuristic.
    let mut causal_pins = 0usize;
    if let Some(citations) = causal_citations {
        for (path, cite_turn) in citations {
            if effective_current_turn.saturating_sub(*cite_turn) >= WORKING_SET_PIN_TURNS {
                continue;
            }
            let latest_read = file_latest_read_idx.get(path).copied();
            let read_small_enough = match latest_read {
                Some(idx) => match &items[idx] {
                    TranscriptItem::ToolResult(tr) => tr.content.len() <= CAUSAL_PIN_MAX_BYTES,
                    _ => false,
                },
                None => false,
            };
            if !read_small_enough {
                continue;
            }
            if !file_last_modified_turn.contains_key(path) {
                causal_pins += 1;
            }
            if let Some(idx) = latest_read {
                pinned_indices.insert(idx);
            }
            if let Some(&edit_idx) = file_latest_edit_idx.get(path) {
                pinned_indices.insert(edit_idx);
            }
            if !pinned_files.iter().any(|existing| existing == path) {
                pinned_files.push(path.clone());
            }
        }
    }
    pinned_files.sort();
    (pinned_indices, pinned_files, causal_pins)
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

/// Capability gap #3 (`v1.5.2`): build the bounded one-line "negative lesson" stub that replaces a
/// failed tool result under causal pruning. It keeps the `source_entry_id` pointer so the raw
/// failure output stays recoverable through `recall_evidence`, and it never claims the attempt
/// succeeded — the code and path are carried explicitly.
pub fn build_dead_end_stub(
    tr: &ToolResultMessage,
    seq_idx: usize,
    source_entry_id: &str,
    include_evidence: bool,
) -> String {
    let path = tr.meta.relative_path.as_deref().unwrap_or(".");
    let status = tr
        .meta
        .exit_code
        .map(|code| format!("exit={}", code))
        .unwrap_or_else(|| "failed".to_string());
    let first_error = tr
        .content
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("(no output)");
    let lesson = crate::prompt::escape_control_chars(first_error);
    let evidence = if include_evidence {
        format!(
            " | source_entry_id={} | call recall_evidence(source_entry_id={})",
            source_entry_id, source_entry_id
        )
    } else {
        String::new()
    };
    let head = format!(
        "[Dead-end: {}({}, {}, seq={}){} | negative lesson (this attempt failed; do not repeat): ",
        tr.tool_name, path, status, seq_idx, evidence
    );
    append_bounded_tail(head, &lesson)
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

        let (_, pinned, _) = compute_working_set_pins(&items, 13, &ledger, None);
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

/// C00-03: a candidate projection algorithm, addressed by its own id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectionCandidate {
    /// Typed task-state projection over the existing `task_state` derivation.
    TaskStateV1,
    /// Extractive semantic compaction.
    SemanticExtractiveV1,
    /// Guideline-optimised compaction.
    GuidelineOptimizedV1,
}

impl ProjectionCandidate {
    pub const ALL: [ProjectionCandidate; 3] = [
        ProjectionCandidate::TaskStateV1,
        ProjectionCandidate::SemanticExtractiveV1,
        ProjectionCandidate::GuidelineOptimizedV1,
    ];

    pub fn id(&self) -> &'static str {
        match self {
            ProjectionCandidate::TaskStateV1 => "ctx.taskstate.v1",
            ProjectionCandidate::SemanticExtractiveV1 => "ctx.semantic-extractive.v1",
            ProjectionCandidate::GuidelineOptimizedV1 => "ctx.guideline-optimized.v1",
        }
    }

    /// The legacy profile a candidate composes onto. A candidate adds a stage; it never redefines
    /// the base it builds on.
    pub fn base_profile(&self) -> ProjectionProfile {
        ProjectionProfile::P2
    }

    /// No candidate is on by default: an unmeasured algorithm must not become the default by
    /// shipping.
    pub fn enabled_by_default(&self) -> bool {
        false
    }

    pub fn parse(value: &str) -> Result<Self, String> {
        let lowered = value.trim().to_ascii_lowercase();
        match lowered.as_str() {
            "ctx.taskstate.v1" => Ok(ProjectionCandidate::TaskStateV1),
            "ctx.semantic-extractive.v1" => Ok(ProjectionCandidate::SemanticExtractiveV1),
            "ctx.guideline-optimized.v1" => Ok(ProjectionCandidate::GuidelineOptimizedV1),
            other => Err(format!(
                "unknown projection candidate '{}'; expected one of {}",
                other,
                ProjectionCandidate::ALL
                    .iter()
                    .map(|candidate| candidate.id())
                    .collect::<Vec<_>>()
                    .join("|")
            )),
        }
    }
}

/// A configured projection: either a legacy profile or a `ctx.*` candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectionSelector {
    Legacy(ProjectionProfile),
    Candidate(ProjectionCandidate),
}

impl ProjectionSelector {
    pub fn parse(value: &str) -> Result<Self, String> {
        if value.trim().to_ascii_lowercase().starts_with("ctx.") {
            return ProjectionCandidate::parse(value).map(ProjectionSelector::Candidate);
        }
        ProjectionProfile::parse(value).map(ProjectionSelector::Legacy)
    }
}

#[cfg(test)]
mod c0003_profile_tests {
    use super::*;

    /// C00-03-A1: the four legacy ids keep their meaning, including the default.
    #[test]
    fn legacy_profile_semantics_are_unchanged() {
        assert_eq!(ProjectionProfile::default(), ProjectionProfile::P2);
        assert!(!ProjectionProfile::Baseline.compacts());
        assert!(!ProjectionProfile::Baseline.records_evidence());
        assert!(ProjectionProfile::P1.compacts());
        assert!(!ProjectionProfile::P1.records_evidence());
        assert!(ProjectionProfile::P2.compacts());
        assert!(ProjectionProfile::P2.records_evidence());
        assert!(!ProjectionProfile::P2.is_aggressive());
        assert!(ProjectionProfile::P3.records_evidence());
        assert!(ProjectionProfile::P3.is_aggressive());
        for id in ProjectionProfile::LEGACY_IDS {
            assert!(
                ProjectionProfile::parse(id).is_ok(),
                "legacy id `{id}` must keep parsing"
            );
        }
    }

    /// C00-03-A2: an id nobody declared is refused, in both namespaces.
    #[test]
    fn unknown_ids_are_refused() {
        for unknown in [
            "p4",
            "p0",
            "taskstate",
            "ctx.unknown.v1",
            "ctx.taskstate.v2",
        ] {
            assert!(
                ProjectionSelector::parse(unknown).is_err(),
                "`{unknown}` must not resolve to a profile"
            );
        }
    }

    /// C00-03-A3: candidates live in their own namespace, compose onto P2, and are off by default.
    #[test]
    fn candidates_use_their_own_ids_and_stay_off_by_default() {
        for candidate in ProjectionCandidate::ALL {
            let id = candidate.id();
            assert!(id.starts_with("ctx."), "candidate ids are namespaced: {id}");
            assert!(
                ProjectionProfile::parse(id).is_err(),
                "a candidate id must not masquerade as a legacy profile: {id}"
            );
            assert!(!candidate.enabled_by_default(), "{id} must be opt-in");
            assert_eq!(candidate.base_profile(), ProjectionProfile::P2);
            assert_eq!(
                ProjectionSelector::parse(id),
                Ok(ProjectionSelector::Candidate(candidate))
            );
        }
        assert_eq!(
            ProjectionSelector::parse("P2"),
            Ok(ProjectionSelector::Legacy(ProjectionProfile::P2)),
            "legacy ids still resolve through the selector"
        );
    }
}

#[cfg(test)]
mod nextgen_history_guard_tests {
    use super::*;
    use crate::messages::UserMessage;

    #[test]
    fn user_constraints_must_remain_byte_identical_in_projection() {
        let mut transcript = vec![TranscriptItem::User(UserMessage {
            content: "Do not delete tests. Only edit src/main.rs.".into(),
        })];
        let mut projector = ContextProjector::new();
        projector
            .project(&mut transcript, &BudgetConfig::default(), 0, None, 1)
            .unwrap();
        assert!(projector.authoritative_history_preserved(&transcript));
        projector.projected_items[0] = TranscriptItem::User(UserMessage {
            content: "Do delete tests.".into(),
        });
        assert!(!projector.authoritative_history_preserved(&transcript));
    }

    #[test]
    fn missing_cache_telemetry_never_equals_zero_cache_hits() {
        let mut projector = ContextProjector::new().with_cache_aware_compaction(true);
        assert_eq!(projector.measured_cache_hits, None);
        projector.observe_cache_usage(Some(0));
        assert_eq!(projector.measured_cache_hits, Some(0));
        projector.observe_cache_usage(None);
        assert_eq!(projector.measured_cache_hits, None);
    }
}
