# Harness v0.9.0 — Gate 1 Acceptance Report

**Gate:** Gate 1 — Workspace Revision 与最小 Undo
**Version:** `0.9.0` (`Cargo.toml` workspace version, `harness --version`, Git tag `v0.9.0`)
**Date:** 2026-10-12
**Spec:** `docs/v1_blueprint/Harness-v1.0-优化蓝图-v2.0.md` §5.1, §9.1, Gate 1; `docs/v1_blueprint/IMPLEMENTATION_GATES_AND_EVAL.md` Gate 1
**CI evidence:** `docs/GATE1_CI_LOG.txt`
**Post-review fixes:** `V090_GATE1_REVIEW_FIX_REPORT.md` (six required fixes from the 3-way Gate 1 review)

---

## 1. What Gate 1 required

From `IMPLEMENTATION_GATES_AND_EVAL.md`:

| Requirement | Status |
| --- | --- |
| Minimal API: `snapshot` / `apply_mutation` / `restore` / `fork` / `compare` | Implemented as `SnapshotStore::capture_revision` / `restore_revision` / `recover_pending_restore` + `SessionJournal::fork_at_turn`. `apply_mutation` and `compare` are folded into `capture_revision` (diff vs parent) — see §6 Deviations. |
| `restore` returns `Applied` / `Conflict` / `RecoveryRequired`, not `bool` | `RestoreResult` enum with all three states. |
| Intent 写入后崩溃 | `RestoreFault::AfterIntentWritten` + `recover_pending_restore`. |
| 文件 mutation 中途崩溃 | `RestoreFault::AfterAppliedFiles(n)` + `recover_pending_restore`. |
| after snapshot 已写但 commit event 未写 | `RevisionCaptured` is written to the WAL immediately after the revision file is durably written; a crash in between leaves an orphan revision (harmless, deduplicated by content hash) and no false WAL record. |
| 磁盘空间不足 | Snapshot writes go through `write_atomic_bytes` (temp + fsync + rename); an `ENOSPC` surfaces as `RevisionError::Io`, which the agent loop treats as fail-closed. |
| 工作区外部修改文件 | `RestoreResult::Conflict` with `ModifiedExternally` / `DeletedExternally` / `CreatedExternally`. |
| 新建目标竞态被其他进程抢先创建 | Restores write through `atomic_write_at` (`openat(O_EXCL)` temp + `renameat`), and `remove_tracked_file` re-stats with `fstatat(AT_SYMLINK_NOFOLLOW)` before unlinking. A user-created symlink/directory at the target path is a conflict (test 10), and `--force-restore` clears it without following the symlink. |
| symlink / 特殊文件 / 大文件 / ignored 文件策略 | Never tracked; reported in `skipped_paths`; never followed or overwritten on restore, and a path skipped in the target revision is never deleted (test 11). |
| undo 再 undo、fork 后修改与恢复 | `--undo` is repeatable (test 6: `T3→T2→T1→T0`); the abandoned branch stays in the WAL (append-only) and cross-branch restore works (test 7). Redo is not a first-class command — see residual risk R6. |

**Invariants**

- *恢复不得静默覆盖未纳入目标 Revision 的外部改动* — enforced by conflict detection before any mutation; `--force-restore` is the only override and it is explicit. Test 2.
- *日志不删除历史事件* — the WAL stays append-only: `--undo` appends a `SetLeaf`, it never truncates. Test 4 + E2E transcript.
- *未知外部副作用不声称可回滚* — documented as residual risk R4; only workspace file bytes and modes are restored.

---

## 2. Delivered surface

### New module `crates/harness-core/src/revision.rs`

- `RevisionId` — content address `r-<32 hex>`, computed from the sorted manifest (path, sha256, mode, size). `RevisionId::parse` rejects anything that is not `<r-><32 hex>`, so `--restore-rev` cannot be used for path traversal.
- `WorkspaceRevision` — `revision_id`, `parent_revision_id`, `wal_entry_id`, `turn_index`, `timestamp_ms`, `tracked_files: BTreeMap<path, TrackedFile{sha256, mode, size}>`, `changed_paths`, `skipped_paths`, `total_bytes`.
- `SnapshotStore` — manifest + content-addressed blob store **outside** the workspace tree:
  - layout `<store>/blobs/<2-hex-shard>/<sha256>` (0600) and `<store>/revisions/<rev>.json` (0600); directories 0700; every open uses `O_NOFOLLOW | O_CLOEXEC`; blobs are hash-verified on read.
  - capture walks the workspace through `WorkspaceCap::read_dir_no_symlink` / `open_existing_no_symlink` only, so the walk cannot be redirected by a symlink race.
  - exclusions: `.git`, `.hg`, `.svn`, `target`, `node_modules`, `.venv`, `venv`, `__pycache__`, `.mypy_cache`, `.pytest_cache`, `.gradle`, `.tox`, `.next`, `dist`, `build`; sensitive paths (`SensitivePathGuard`); symlinks; non-regular files; files > 4 MiB; totals > 256 MiB; depth > 32.
- `RestoreResult` — `Applied { restored_revision, restored_files, removed_files }` | `Conflict { target_revision, conflicting_paths }` | `RecoveryRequired { target_revision, journal_path, reason }`. Conflict detection classifies the live path (`Absent` / `Regular{mode,size,sha256}` / `Directory` / `NonRegular`) with `fstatat(AT_SYMLINK_NOFOLLOW)` and compares kind, content hash, **and** permission bits, so an externally created symlink/directory, a `chmod`, or a type swap is a conflict rather than a silent overwrite.
- Restore ordering: all removals run before all writes, deepest path first, and emptied parent directories are pruned — so a path that changed kind between revisions (file ↔ directory) restores cleanly. `--force-restore` clears an obstructing symlink/directory before writing.
- Paths recorded in `target.skipped_paths` (oversized/special/sensitive) are never removed, and paths skipped in the expected revision are never treated as conflicts.
- Crash-safe restore: `restore-in-progress.json` is written and fsynced **before** the first mutation and removed only after the last one. An unrecovered journal makes any further restore return `RecoveryRequired` instead of layering a second restore on a half-applied tree.
- `recover_pending_restore` re-applies the journal's operations idempotently and then clears it.

### WAL linkage (`session.rs`, `events.rs`, `agent_loop.rs`)

- New append-only `WalPayload::RevisionCaptured { leaf_id, turn, revision_id, parent_revision_id, changed_paths, tracked_file_count, captured_at_ms }`, replayed into `RevisionRecord`s exposed by `SessionJournal::revision_records()` / `latest_revision_id()`.
- `SessionCheckpoint` gained `#[serde(default)] workspace_revision_id: Option<String>`, and `WalPayload::Checkpoint` gained the matching `#[serde(default)]` field. Pre-Gate-1 WALs still load unchanged.
- `AgentRunner::with_snapshot_store(...)` captures a `baseline` revision before the first turn and a `mutation` revision after every tool batch that reported `modified_file`. Capture failure is fail-closed: the run aborts rather than pretending undo protection exists.
- `EventPayload::RevisionCaptured` emits one telemetry line per capture (`EVENT_SCHEMA_VERSION` → `0.7.0`).

### CLI (`harness-cli/src/main.rs`)

| Flag | Behaviour |
| --- | --- |
| `--undo` | Restores the workspace to the last revision the workspace actually differed from, then appends a `SetLeaf` rewinding the transcript to that turn. Refuses with `RestoreResult::Conflict` unless `--force-restore`. |
| `--restore-rev <REV_ID>` | Restores the workspace to a named revision; the expected revision is the session's latest recorded one, and the restored revision is re-recorded in the WAL so a following `--undo` stays in sync. |
| `--force-restore` | Explicit override for external modifications. |
| `--list-revisions` | Prints each captured revision with its turn, tracked/changed counts, changed paths, and which one is current. |

`--task` is optional for the three management actions, which require `--resume` and run **before** the `HARNESS_API_KEY` check, so no provider is contacted. Exit codes: `0` success, `2` invalid invocation, `3` restore conflict, `4` recovery required.

---

## 3. Verification

**Automated:** `cargo test --workspace` → **84 passed / 0 failed / 0 warnings**.

New Gate 1 test binary `crates/harness-core/tests/gate1_workspace_revision.rs` (13 integration tests):

| Test | Invariant |
| --- | --- |
| `test_capture_then_restore_applies_modified_deleted_and_created_files` | Baseline + mutation capture; `Applied` restores modified, deleted, and newly created files; file mode 0600 round-trips; intent journal cleared. |
| `test_external_modifications_conflict_and_force_restore_overwrites` | `ModifiedExternally` / `DeletedExternally` / `CreatedExternally` all reported with the exact paths; nothing mutated on refusal; a never-tracked file is not a conflict and survives even a forced restore; `--force` applies. |
| `test_interrupted_restore_requires_recovery_then_recovers_to_a_consistent_state` | `AfterIntentWritten` and `AfterAppliedFiles(1)` both yield `RecoveryRequired` with a live journal; a second restore is refused; `recover_pending_restore` finishes to a consistent tree and clears the journal. |
| `test_agent_loop_persists_revision_ids_and_undo_rewinds_wal_and_workspace` | End-to-end `AgentLoop` + `SessionJournal` + `SnapshotStore`: baseline and mutation revisions persisted, checkpoint carries the revision id, `--undo` rewinds both the workspace file and the transcript leaf, abandoned branch retained. |
| `test_symlinks_and_sensitive_files_are_never_captured_or_overwritten` | `.env`, symlinks, `node_modules/`, `target/` excluded and reported in `skipped_paths`; the secret never reaches a blob; a symlink swapped over a tracked path yields a conflict and is not followed or replaced. |
| `test_sequential_undo_across_three_mutating_turns_rewinds_to_the_baseline` | Repeatable `--undo` across `T3 → T2 → T1 → T0`; the active branch loses exactly one revision per undo. |
| `test_fork_then_new_mutation_and_cross_branch_restore` | `--fork` at an earlier turn, a new mutation on the new branch, and a restore of a revision that lives on the abandoned branch. |
| `test_nested_new_directory_tree_is_pruned_on_undo` | Undoing `a/b/c/new.rs` removes the file and prunes the empty `a/b/c`, `a/b`, `a` directories. |
| `test_file_and_directory_kind_swap_restores_in_both_directions` | `foo` file ↔ `foo/bar.txt` file restores cleanly both ways instead of wedging in `RecoveryRequired`. |
| `test_external_symlink_and_mode_change_are_conflicts` | External `chmod`, a symlink created at a target-only path, and a symlink swapped onto a tracked path are all reported as conflicts. |
| `test_skipped_large_file_is_preserved_on_restore` | A >4 MiB file recorded in `skipped_paths` is never removed when restoring to that revision. |
| `test_resumed_run_baseline_survives_undo_rewind` | A resumed run binds its baseline to the pre-run leaf, so `--undo` rewinds without evicting the baseline revision. |
| `test_force_restore_clears_obstructing_symlink_and_directory` | `--force-restore` removes an obstructing non-empty directory or symlink (without following it) instead of wedging in `RecoveryRequired`. |

Plus unit tests: `revision_id_parse_rejects_traversal_and_accepts_content_addresses`, `revision_id_is_content_addressed_and_mode_sensitive`, `diff_paths_reports_added_removed_and_modified_paths_only`, CLI `test_workspace_revision_management_flags_parse`, and CLI `test_restore_rev_records_revision_so_a_following_undo_stays_in_sync`.

**End-to-end CLI** (local OpenAI-compatible mock, no network) — full transcript in `docs/GATE1_CI_LOG.txt`:

```
run --allow-write --no-stream --no-verify
  📸 [revision] turn=0 rev=r-94f630ad… trigger=baseline files=1 changed=1
  🔧 [tool:write] path="src/app.rs" -> success
  📸 [revision] turn=1 rev=r-f7ef24e0… trigger=mutation files=1 changed=1
  🏁 Completed (ModelStop) | turns=2 tools=1
  workspace file: fn main() { println!("hi"); }

--list-revisions            -> turn 0 r-94f630ad…, turn 1 r-f7ef24e0… <- current   (exit 0)
--restore-rev r-94f630ad…   -> restored, re-recorded in the WAL                   (exit 0)
  workspace file: fn main() {}
--list-revisions            -> 3 records; the re-recorded r-94f630ad… is <- current
--undo                      -> restored r-f7ef24e0…, rewound transcript to turn 1 (exit 0)
  workspace file: fn main() { println!("hi"); }
external edit + --restore-rev r-94f630ad…  -> ModifiedExternally src/app.rs       (exit 3, file untouched)
--undo --force-restore      -> restored r-94f630ad…, rewound transcript to turn 0 (exit 0)
  workspace file: fn main() {}
--list-revisions            -> 1 revision on the rewound branch
```

WAL checkpoint binding observed:

```
seq 3  checkpoint.turn=1  workspace_revision_id=r-94f630ad7dd853bf6ac540f5b6b69035
seq 7  checkpoint.turn=2  workspace_revision_id=r-f7ef24e038a5ea5d9dc8d08087de9a2c
seq 10 set_leaf            leaf_id=48984b2d-…   (the --undo rewind, appended, nothing truncated)
```

---

## 4. Files changed

| File | Change |
| --- | --- |
| `crates/harness-core/src/revision.rs` | **new** — `RevisionId`, `WorkspaceRevision`, `SnapshotStore`, `RestoreResult`, `RestoreIntent`, `RestoreFault`, fault-injection + recovery, live-entry classification (`LiveEntryStat` / `LiveEntryState`, `stat_live_entry`, `live_entry_state`, `classify_blocked_parent`), conflict classifiers, remove-before-write ordering, empty-ancestor pruning, force obstruction removal, unit tests. |
| `crates/harness-core/src/lib.rs` | export `revision`. |
| `crates/harness-core/src/session.rs` | `RevisionRecord`, `WalPayload::RevisionCaptured`, `#[serde(default)] workspace_revision_id` on `SessionCheckpoint` / `WalPayload::Checkpoint`, `checkpoint_with_revision`, `record_revision_capture`, `revision_records`, `latest_revision_id`. |
| `crates/harness-core/src/events.rs` | `EventPayload::RevisionCaptured`, live-stderr line, schema version `0.7.0`. |
| `crates/harness-core/src/agent_loop.rs` | `with_snapshot_store`, baseline/mutation capture, fail-closed `revision_error`, checkpoint carries the revision id, baseline captured before the task message, `current_revision_id` seeded from the WAL. |
| `crates/harness-cli/src/main.rs` | `--undo` / `--restore-rev` / `--force-restore` / `--list-revisions`, optional `--task`, `run_revision_management`, `record_restored_revision`, usage text, exit codes 3/4, parse tests. |
| `crates/harness-core/tests/gate1_workspace_revision.rs` | **new** — 13 Gate 1 acceptance tests. |
| `Cargo.toml`, `Cargo.lock` | workspace version `0.9.0`. |
| `docs/GATE1_CI_LOG.txt` | **new** — build/test/E2E evidence + residual risk register. |
| `V090_GATE1_REVIEW_FIX_REPORT.md` | **new** — the six post-review fixes and the still-open findings. |
| `GATE1_ACCEPTANCE_REPORT.md` | **new** — this document. |

---

## 5. Gate exit conditions

| Exit condition | Result |
| --- | --- |
| 单文件和受控多文件恢复 | Single-file and controlled multi-file restore, both covered by tests 1–3 and the E2E CLI run. |
| WAL/事件状态机与文件版本绑定 | `WalPayload::RevisionCaptured` + `SessionCheckpoint.workspace_revision_id`, both backward compatible. |
| 故障注入后状态可重建 | Test 3: two fault points, both recovered to a consistent tree. |
| 冲突时不覆盖外部修改 | Test 2 + E2E exit code 3. |
| undo 生成审计事件 | `EventPayload::RevisionCaptured` telemetry and an append-only WAL record; the rewind is an appended `SetLeaf`, never a truncation. |
| 测试崩溃恢复和备份 | Test 3 (crash recovery) and test 1 (mode/byte-exact restore from the CAS). |

---

## 6. Deviations and residual risks

**Deviations from the literal minimal API.** The blueprint's `apply_mutation(parent_revision, expected_hashes, mutation) -> MutationResult` describes a write-ahead path where the harness validates expected hashes before mutating. That is already implemented in the tool layer (`write.rs` `expected_sha256` / `if_not_exists` + the `(parent_dev, parent_ino, file_name)` inode lock), so Gate 1 does not duplicate it; `capture_revision` computes the diff that `compare(a, b)` would return. `fork(target_revision) -> WorkspaceHandle` is realized as `SessionJournal::fork_at_turn` plus a separate `WorkspaceCap`, rather than a new handle type — Gate 3's isolated worktrees are the natural home for a combined session+workspace handle.

**Residual risks** (also in `docs/GATE1_CI_LOG.txt`):

- **R1** Full workspace walk per mutating turn. Bounded, but not yet measured on a large repository; the blueprint requires that measurement before deciding whether this backend stays.
- **R2** Non-UTF-8 file names are skipped by the existing directory reader and are not reported in `skipped_paths`.
- **R3** Only paths tracked by the target or expected revision are touched; a brand-new untracked file is left alone (asserted).
- **R4** Non-file side effects (network, databases, daemons) are not and cannot be rolled back.
- **R5** Restore covers file bytes and modes, not another process's cwd or open handles.
- **R6** No first-class redo command; redoing is a fresh capture after the undo.
- **R7** A `bash` tool call that mutates workspace files still reports no `modified_file`, and a mutating `CompletionGate` verification command is still not snapshotted, so the workspace can drift ahead of `latest_revision_id()` for those two paths (open finding O1).
- **R8** `SnapshotStore::store_blob` writes the final CAS path in place instead of temp+rename, and `ensure_private_dir` chmods an existing directory by path (open findings O2/O3).
- **R9** No dedicated `RevisionRestored` audit event and no `SnapshotStore::compare(rev_a, rev_b)` API yet; `revision_records()` still orders by WAL append index rather than topological depth (open findings O4/O6).

---

## 7. Recommended next step

Proceed to **Gate 2 — Evidence-Linked Context Projection** (deterministic budgets, `source_entry_id` evidence graph, raw-evidence retrieval), and in parallel instrument a snapshot-cost measurement on a large repository to close R1 before Gate 3 builds worktrees on top of this store.
