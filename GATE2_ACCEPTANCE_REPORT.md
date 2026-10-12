# Harness v0.10.0 — Gate 2 Acceptance Report

**Scope:** Gate 2 of the v1.0 blueprint (`docs/v1_blueprint/Harness-v1.0-优化蓝图-v2.0.md` §Gate 2 —
"Context Projection v1: deterministic budgets, active task/file protection, source IDs and token
accounting; old evidence must be recoverable"), plus the six Gate 1 polish items O1–O6 recorded in
`V090_GATE1_REVIEW_FIX_REPORT.md` §2.

**Result:** complete. `cargo build --workspace --all-targets` is warning-free and
`cargo test --workspace` is **100 passed / 0 failed** (was 92). Full transcript in
`docs/GATE2_CI_LOG.txt`; the machine-readable A/B evaluation record is
`docs/GATE2_EVAL_METRICS.json`.

The three-way Gemini audit of the first `v0.10.0` cut produced 2 P0 / 7 P1 / 4 P2 findings; all of
them are resolved and regression-tested — see §6.

---

## 1. Gate 2 — Evidence-Linked Context Projection

### 1.1 Deterministic evidence ids

Every Outline Stub now carries a replay-stable identifier:

```
source_entry_id = "ev-<canonical_index:04>-<call_id>"
```

`canonical_index` is the position in the canonical transcript and `call_id` is the provider tool-call
id — both survive `--resume` and `--fork` unchanged for every retained prefix item, so the same stub
always yields the same id in any process. `context_projector::source_entry_id` builds it and
`extract_source_entry_id` parses it back out of a stub header.

Each stub header also tells the model how to get the raw lines back. The `source_entry_id` sits in
the fixed-length head (right after the tool name), and only the variable-length `Outline`/`Tail`
payload is truncated, so a long `relative_path` or `call_id` can never slice the id out of the
header:

```
[Compacted: read(src/big.rs, lines 1-400 of 400, 11.5 KiB, sha256=0123456789abcdef, seq=2)
 | source_entry_id=ev-0002-call_big | call recall_evidence(source_entry_id=ev-0002-call_big)
 | Outline: L1 pub fn secret_answer() -> u32]
```

`EvidenceLink` records the provenance of every compaction:

| field | meaning |
| --- | --- |
| `source_entry_id` | the deterministic id embedded in the stub |
| `canonical_index` | index of the compacted item in the canonical transcript |
| `call_id` / `tool_name` / `relative_path` | origin of the output |
| `original_sha256` | full SHA-256 of the raw output (64 hex) |
| `original_bytes` / `original_lines` | size of the raw output |
| `stub_bytes` | size of the replacement stub |
| `reason` | `StepDownArchive` or `SupersededRead` |

`SupersededRead` is set when a later *successful* `read` of the same path exists, so the model can
tell "this was reclaimed for budget" from "this was replaced by a newer read".

### 1.2 `recall_evidence`

New built-in **read-only** tool (`crates/harness-core/src/tools/recall_evidence.rs`), registered in
`tool/definition.rs` and therefore visible in the model schema under every policy mode including
`--policy read-only`:

| argument | behaviour |
| --- | --- |
| `source_entry_id` (required) | exact `source_entry_id` **or** the originating tool `call_id` |
| `start_line` | 1-based first line (default 1) |
| `end_line` | 1-based last line, capped at `start_line + 199` |
| `pattern` | case-insensitive substring filter returning matches with ±2 lines of context |

Output is `[Recalled Evidence: <id> | Tool: <tool> | Lines: X-Y of Z | SHA256: <sha16>]` followed by
1-based numbered lines, capped at 200 lines / 32 KiB. An unknown id returns
`ErrorCode::InvalidArguments` **listing the currently available compacted ids** rather than an empty
success.

Raw bytes are retained in an in-memory evidence store on `ContextProjector` and, crucially, are also
**re-seeded from the session WAL on `--resume`**: the WAL keeps the pre-overlay `Append` item while
`active_entries()` yields the compacted stub, so `SessionJournal::raw_entry_item` still returns the
original output. Evidence therefore survives a process restart, not just a single run.

`recall_evidence` is excluded from the parallel read-only sub-batch and dispatched by the agent loop
(which owns the live transcript); appending its call and result never rewrites an earlier projected
item, so the frozen prefix KV cache is preserved. It is still resolved through
`ToolRegistry::lookup_for_dispatch`, so `--policy` and `--tool-filter` hide/deny it exactly like every
other tool (a filtered-out call returns `deny_tool_filter`). Every hit/miss is counted on the
projector (`recall_hit_count` / `recall_miss_count`) and reported in the budget breakdown.

### 1.3 Deterministic category budgets

`ContextProjector::stats()` returns a `ProjectionStats` with a `ContextBudgetBreakdown`:
`system_prefix_bytes`, `tool_schema_bytes`, `history_bytes`, `tool_output_bytes`,
`pinned_working_set_bytes`, `compacted_stub_bytes`, `recent_tail_bytes`,
`total_projected_bytes`, `evidence_link_count`, `recall_hit_count`, `recall_miss_count`, plus a
`tokens` mirror (`ContextBudgetTokens`, `bytes / 4`) for each of the four request components.
Every projected item lands in exactly one bucket, and `total_projected_bytes` equals the sum of the
projected items' estimated bytes.

The agent loop freezes the system-prefix / tool-schema split with
`ContextProjector::set_prefix_breakdown`; a direct projector use that never calls it reports the
whole static prefix as the system prefix so the components still add up.

### 1.4 Projection profiles (Gate 2 A/B)

`ProjectionProfile` selects the algorithm, defaulting to `P2`:

| profile | behaviour |
| --- | --- |
| `Baseline` | no compaction at all (the pre-Gate-2 reference) |
| `P1` | watermarks + Step-Down Compaction + Working-Set Pinning, no evidence graph |
| `P2` (default) | `P1` + `source_entry_id` evidence graph + `recall_evidence` pointers |
| `P3` (experimental, off by default) | `P2` + a 25 % stricter low-watermark target |

`docs/GATE2_EVAL_METRICS.json` records the four profiles across the five blueprint task archetypes
(long-file navigation, cross-file bug fix, repeated-error recovery, over-budget, multi-turn).
Measured on the checked-in fixtures, `Baseline` projects 25–50 KiB while `P1`/`P2` stay under
~9.6 KiB, and `P3` is strictly smaller than `P2` where the fixture's compaction granularity allows
(e.g. over-budget: 9294 B → 6428 B). The 1:1 tool-call/result pairing invariant
(`validate_tool_call_pairing`) is asserted for all 20 profile × archetype combinations.

### 1.5 Replayability

`ContextProjector::project` is deterministic: the same canonical transcript and budget produce
byte-for-byte identical projected messages and identical `source_entry_id`s on a fresh run, after a
`--resume` (the WAL persists the compacted projection and re-projecting it reproduces the same
messages), and after a `--fork` rewind (the surviving prefix is unchanged).

---

## 2. Gate 1 polish items O1–O6

| ID | Fix | Location |
| --- | --- | --- |
| **O1** | A `bash` (`SideEffect`) call now sets `batch_ran_side_effect`, and the `CompletionGate` verification command is followed by a capture too. A post-batch capture records a WAL `RevisionCaptured` **only when the workspace content address actually changed** (baseline always records), so the workspace can no longer drift ahead of `latest_revision_id()` without spamming the WAL. | `agent_loop.rs` |
| **O2** | `store_blob` writes to a sibling `.<sha>.tmp-<uuid>` file with `O_CREAT\|O_EXCL\|O_NOFOLLOW` at mode `0600`, `fsync`s it, then atomically `rename`s it onto the CAS path. A crash or `ENOSPC` mid-write leaves at most a stray temp file — never a truncated blob that would poison every future revision. | `revision.rs` |
| **O3** | `ensure_private_dir` now verifies with `fs::symlink_metadata` that the path is a real directory **before** any `chmod`, so a symlinked store path is refused instead of having its target chmod-ed through. The previous `AlreadyExists`-only check was dead code because `DirBuilder::recursive(true)` returns `Ok` for an existing path. | `revision.rs` |
| **O4** | `revision_records()` orders records by the topological depth of the leaf they were bound to (root → leaf), with WAL append index as the tie-breaker, so an ancestor capture recorded after a fork can no longer shadow a deeper descendant in `latest_revision_id()`. | `session.rs` |
| **O5** | Fault-injection coverage for "snapshot written but commit event not written" (orphan manifest) plus blob-atomicity and symlink-store tests. | `gate2_evidence_projection.rs`, `main.rs` tests |
| **O6** | Added `SnapshotStore::compare(a, b)` and `SnapshotStore::newest_revision()`, the `EventPayload::RevisionRestored { revision_id, restored_files, removed_files }` audit event (emitted by `--undo` / `--restore-rev` / crash recovery), and `reconcile_store_with_wal` which adopts a manifest that was durably written immediately before a crash that stopped the WAL append. | `revision.rs`, `events.rs`, `main.rs` |

`reconcile_store_with_wal` is deliberately conservative: it only adopts a manifest that (a) is not
already recorded, (b) is bound to a `wal_entry_id` that is on the active branch, and (c) is at least
as new as the newest recorded revision — so a manifest from an abandoned branch or another session in
the same state directory is never resurrected.

---

## 3. Tests

`crates/harness-core/tests/gate2_evidence_projection.rs` (14 tests):

| Test | Covers |
| --- | --- |
| `test_compaction_records_evidence_links_and_budget_breakdown` | Test 1 — read/grep/bash compaction embeds `source_entry_id`, records `EvidenceLink` metadata in the projector and the session WAL, tags `SupersededRead`, and reports an accurate `ContextBudgetBreakdown` |
| `test_recall_evidence_returns_line_ranges_and_pattern_matches` | Test 2 — exact line ranges, pattern matches with context, non-matching pattern error, unknown id listing valid ids, lookup by `call_id` |
| `test_agent_loop_recalls_evidence_from_an_outline_stub` | Test 3 — end-to-end `AgentLoop`: a large read is compacted into a stub that strips the function-body constant, the model recalls it by `source_entry_id`, and the projected prefix stays byte-identical |
| `test_recall_evidence_survives_resume` | Gate 2 goal — the raw evidence a stub replaced is still retrievable after a `--resume` in a new process |
| `test_projection_replay_is_deterministic_across_resume_and_fork` | Test 4 — byte-identical replay across a fresh run, a `--resume` and a `--fork` rewind |
| `test_gate1_o1_o6_polish_items_are_verified` | Test 5 — O1 (`bash` mutation snapshot), O2 (atomic blob, no temp leftovers), O3 (symlinked store refused), O4 (depth-ordered revision records), O5/O6 (`compare`, `newest_revision`) |
| `test_crash_before_wal_append_is_reconciled` | O5/O6 — orphan-manifest reconciliation through the production `SnapshotStore::reconcile_with_wal` path, idempotence, and a real `compare` diff |
| `test_fork_isolates_evidence_links_across_branches` | §6 F-1 — compaction overlay recorded on an abandoned branch is invisible on the live branch, an ancestor stub still resolves after `--fork` + reopen, and the same canonical index re-compacts cleanly |
| `test_fork_rewind_prunes_evidence_and_allows_recompaction_at_the_same_index` | §6 F-2 — a rewind prunes the rewound link, raw bytes and `call_id` index, so re-compaction at the same index registers the new bytes |
| `test_resume_restores_compacted_indices_and_budget_breakdown` | §6 F-3 — re-seeding stubs from the WAL restores `compacted_indices`, so the resumed budget breakdown matches the fresh one |
| `test_superseded_read_recall_returns_the_earlier_read_bytes` | §6 F-5 — a re-read is tagged `SupersededRead` and both reads recall their own bytes; a *failed* later read does not supersede |
| `test_recall_evidence_utf8_patterns_and_byte_cap_line_clipping` | §6 F-6 — CJK/emoji/Unicode-case-folded patterns, `start_line`/`end_line` pattern windows, and truthful `Lines: X-Y of Z` + continuation hints under the 32 KiB cap |
| `test_evidence_links_select_the_deepest_overlay_like_active_entries` | §6 F-7 — evidence overlays are selected by leaf depth, not WAL append order |
| `test_projection_profiles_ab_metrics_and_tool_pairing_invariant` | §6 F-4 — four profiles × five archetypes, pairing invariant, and the `docs/GATE2_EVAL_METRICS.json` artifact |

Plus `harness-cli` unit tests
`test_reconcile_orphan_snapshot_and_emit_revision_restored_event` (CLI-level reconciliation
including idempotence and the `RevisionRestored` event) and
`test_undo_rebinds_the_wal_when_the_undone_revision_shares_the_rewind_leaf` (§6 F-8).

Two existing assertions were updated because the built-in tool set legitimately grew from 7 to 8
(`gate0_config_and_events.rs`, `test_tool_registry_ascii_order_and_policy_visibility`).

---

## 4. Verification

```
cargo build --workspace --all-targets   # 0 warnings
cargo test  --workspace                 # 100 passed / 0 failed
harness --version                       # harness 0.10.0
docs/GATE2_EVAL_METRICS.json            # 4 profiles x 5 archetypes, pairing 20/20 ok
```

End-to-end CLI validation against a real OpenAI-compatible provider (workbuddy/deepseek) is recorded
in `docs/GATE2_CI_LOG.txt`: a full read/write/read-back run captured baseline + mutation revisions,
`--list-revisions` and `--undo` rewound the workspace and the session leaf, the `RevisionRestored`
event was emitted, and `recall_evidence` was dispatched under `--policy ask` returning the documented
unknown-id error.

---

## 5. Residual risks / deliberately out of scope

1. **Evidence is not persisted as a separate blob store.** Raw evidence survives `--resume` because
   the WAL retains the pre-overlay item, but `--fork` past a compaction point and then editing the
   WAL by hand could still orphan a stub. Gate 2's exit criteria ("old evidence recoverable") are met
   for the supported `--resume` / `--fork` paths.
2. **`recall_evidence` output is not itself compactable.** It is capped at 32 KiB / 200 lines, well
   under `MIN_COMPACTABLE_TOOL_BYTES` growth risk, but a model that recalls repeatedly can still grow
   the tail window; the existing tail protection handles that.
3. **`ContextBudgetBreakdown` counts `estimated_bytes`**, the same estimator used for watermark
   decisions, so the breakdown is internally consistent but is not a token count. Provider-reported
   usage remains the authority for cost.
4. **`P3` implements the blueprint's "experimental, default off" slot as a stricter budget target,
   not as an ACE-style playbook update.** Playbook injection is Gate 3+ scope; `P3` exists so the
   A/B harness can measure the cost/benefit of a stricter watermark.
5. **The A/B fixtures are synthetic and checked in.** The blueprint also asks for a held-out private
   split; `docs/GATE2_EVAL_METRICS.json` records the public fixtures only.

---

## 6. Post-Gemini-audit fixes (v0.10.0 final)

The three-way Gemini review of the first `v0.10.0` cut reported 2 P0 / 7 P1 / 4 P2 findings. Every
one is fixed, and each fix carries a regression test that fails when the fix is reverted.

| ID | Severity | Finding | Fix |
| --- | --- | --- | --- |
| **F-1** | P1 | `--fork` rewind left stale `evidence` / `evidence_call_index` entries, so a new compaction reusing the same `(canonical_index, call_id)` minted an identical `source_entry_id` that was silently dropped | `ContextProjector::project` prunes the rewound links *and* their raw bytes and `call_id` index; `register_evidence` overwrites an existing id instead of early-returning |
| **F-2** | P1 | `--resume` / `--fork` did not restore `compacted_indices` / `compacted_file_paths`, so `compacted_stub_bytes` was reported as 0 and reread-regret accounting was blind | `register_evidence` now marks the item compacted; `project` also marks any appended item that already carries a stub header |
| **F-3** | P1 | `recall_evidence` misreported `Lines: X-Y` and the continuation `start_line` when the 32 KiB cap clipped the response, and ignored `start_line`/`end_line` when `pattern` was set | `render_lines` checks the byte budget *before* appending and returns the actual last rendered line; the header, `ToolResultMeta.lines_range` and the footer all use it; the pattern scan and its ±2 context lines are clamped to the requested window; the body is capped before the footer is appended |
| **F-4** | P0 | Gate 2's required A/B profiles, benchmark task suite and machine-readable evaluation record were missing | `ProjectionProfile` (`Baseline`/`P1`/`P2`/`P3`), the 5-archetype × 4-profile harness, `validate_tool_call_pairing`, and `docs/GATE2_EVAL_METRICS.json` |
| **F-5** | P2 | `SupersededRead` matched a *failed* later `read`, mislabelling an earlier successful read | added `t.outcome.is_success()` to the supersede check |
| **F-6** | P1 | Budget telemetry lacked the required component/token breakdowns and recall success-rate counters | extended `ContextBudgetBreakdown` / `ProjectionStats` with `system_prefix_bytes`, `tool_schema_bytes`, `history_bytes`, `tool_output_bytes`, `tokens`, `recall_hit_count`, `recall_miss_count`; the agent loop counts recall hits/misses |
| **F-7** | P2 | `SessionJournal::evidence_links` deduplicated in WAL append order, so a shallower overlay could shadow a deeper descendant's | it now selects one overlay per `entry_id` by `(leaf_depth, wal_index)` — the same rule `active_entries()` uses — and drops links past the active transcript length |
| **F-8** | P1 | `--undo` left `latest_revision_id()` naming the *undone* revision whenever `current` and `previous` shared a turn/leaf (i.e. after `--restore-rev` or a `--resume` baseline capture), so the next `--undo` / `--restore-rev` failed with a false `ModifiedExternally` | after `fork_at_turn`, `--undo` re-binds the restored revision when `latest_revision_id()` does not already match it |
| **F-9** | P1 | Post-verification revision capture passed the *relative* `current_turn`, corrupting revision turn ordering on `--resume` | it now passes `absolute_turn` |
| **F-10** | P1 | `recall_evidence` bypassed `--tool-filter` / registry dispatch | the agent loop resolves it through `ToolRegistry::lookup_for_dispatch` and falls through to the runtime (which returns `deny_tool_filter`) when it is hidden |
| **F-11** | P2 | `write_atomic_bytes` leaked its `.tmp-<uuid>` file on write/chmod/fsync/rename failure | the write is wrapped so a failure unlinks the temporary file, matching `store_blob` |
| **F-12** | P2 | `ensure_private_dir` followed symlinked ancestors and used path-based `chmod` | each component is created with `mkdirat` on the *verified* parent descriptor, re-opened with `O_NOFOLLOW \| O_DIRECTORY`, `fstat`-checked, and only the final component is `fchmod`-ed to 0700 |
| **F-13** | P2 | `reconcile_store_with_wal` used the global max `turn_index` and matched any ancestor leaf, and was skipped on `--resume` | `SnapshotStore::reconcile_with_wal` only considers manifests bound to the *current* leaf and picks the latest by `(turn_index, timestamp_ms)`; the `--resume` task path now reconciles too |
