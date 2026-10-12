# Harness v0.10.0 — Gate 2 Acceptance Report

**Scope:** Gate 2 of the v1.0 blueprint (`docs/v1_blueprint/Harness-v1.0-优化蓝图-v2.0.md` §Gate 2 —
"Context Projection v1: deterministic budgets, active task/file protection, source IDs and token
accounting; old evidence must be recoverable"), plus the six Gate 1 polish items O1–O6 recorded in
`V090_GATE1_REVIEW_FIX_REPORT.md` §2.

**Result:** complete. `cargo build --workspace --all-targets` is warning-free and
`cargo test --workspace` is **92 passed / 0 failed** (was 84). Full transcript in
`docs/GATE2_CI_LOG.txt`.

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

Each stub header also tells the model how to get the raw lines back:

```
[Compacted: read(src/big.rs, lines 1-400 of 400, 11.5 KiB, sha256=0123456789abcdef, seq=2)
 | source_entry_id=ev-0002-call_big | call recall_evidence(source_entry_id=ev-0002-call_big) for the raw lines
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

`SupersededRead` is set when a later `read` of the same path exists, so the model can tell "this was
reclaimed for budget" from "this was replaced by a newer read".

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
item, so the frozen prefix KV cache is preserved.

### 1.3 Deterministic category budgets

`ContextProjector::stats()` returns a `ProjectionStats` with a `ContextBudgetBreakdown`:
`pinned_working_set_bytes`, `compacted_stub_bytes`, `recent_tail_bytes`, `total_projected_bytes`,
`evidence_link_count`. Every projected item lands in exactly one bucket, and `total_projected_bytes`
equals the sum of the projected items' estimated bytes.

### 1.4 Replayability

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

`crates/harness-core/tests/gate2_evidence_projection.rs` (7 tests):

| Test | Covers |
| --- | --- |
| `test_compaction_records_evidence_links_and_budget_breakdown` | Test 1 — read/grep/bash compaction embeds `source_entry_id`, records `EvidenceLink` metadata in the projector and the session WAL, tags `SupersededRead`, and reports an accurate `ContextBudgetBreakdown` |
| `test_recall_evidence_returns_line_ranges_and_pattern_matches` | Test 2 — exact line ranges, pattern matches with context, non-matching pattern error, unknown id listing valid ids, lookup by `call_id` |
| `test_agent_loop_recalls_evidence_from_an_outline_stub` | Test 3 — end-to-end `AgentLoop`: a large read is compacted into a stub that strips the function-body constant, the model recalls it by `source_entry_id`, and the projected prefix stays byte-identical |
| `test_recall_evidence_survives_resume` | Gate 2 goal — the raw evidence a stub replaced is still retrievable after a `--resume` in a new process |
| `test_projection_replay_is_deterministic_across_resume_and_fork` | Test 4 — byte-identical replay across a fresh run, a `--resume` and a `--fork` rewind |
| `test_gate1_o1_o6_polish_items_are_verified` | Test 5 — O1 (`bash` mutation snapshot), O2 (atomic blob, no temp leftovers), O3 (symlinked store refused), O4 (depth-ordered revision records), O5/O6 (`compare`, `newest_revision`) |
| `test_crash_before_wal_append_is_reconciled` | O5/O6 — orphan manifest reconciliation |

Plus `harness-cli` unit test
`test_reconcile_orphan_snapshot_and_emit_revision_restored_event` for the CLI-level reconciliation
(including idempotence) and the `RevisionRestored` event.

Two existing assertions were updated because the built-in tool set legitimately grew from 7 to 8
(`gate0_config_and_events.rs`, `test_tool_registry_ascii_order_and_policy_visibility`).

---

## 4. Verification

```
cargo build --workspace --all-targets   # 0 warnings
cargo test  --workspace                 # 92 passed / 0 failed
harness --version                       # harness 0.10.0
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
4. **Evidence links are not pruned from `evidence_links` on `--fork` beyond the rewind point** — they
   are filtered out of WAL persistence by `canonical_index` and dropped on the next rewind, but
   `stats().evidence_link_count` is a projection-wide count, not a per-branch one.
