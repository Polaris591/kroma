# Harness v0.9.0 — Gate 1 Review-Fix Report

**Scope:** the six required fixes derived from the 3-way review of Gate 1 (`v0.9.0`).
**Reviews consumed:**

- `review-gate1-cas-restore` — `revision.rs` snapshot/restore engine
- `review-gate1-wal-loop-cli` — WAL linkage, `AgentLoop` capture triggers, CLI `--undo` / `--restore-rev`
- `review-gate1-tests-blueprint` — Gate 1 tests vs. the blueprint exit criteria

All three reviews returned **BLOCK**. This report records what was fixed, what is still open, and
the evidence for each.

---

## 1. Fixed

### F1 — [P0] External symlink / directory / mode change bypassed conflict detection

**Was:** `live_sha256` returned `Ok(None)` for a symlink, directory, or missing path and hashed only
file bytes. A user-created symlink or directory at a path present only in the target revision
therefore compared equal to "absent" and was silently overwritten by `renameat`; an external `chmod`
passed because the mode was never compared; and `current_expected_rev == None` skipped conflict
detection entirely.

**Now:** `stat_live_entry` classifies a path as `Absent` / `Regular { mode, size }` / `Directory` /
`NonRegular` using `open_parent_dir_no_symlink` + `fstatat(AT_SYMLINK_NOFOLLOW)`, and
`live_entry_state` adds the content hash for a regular file. `classify_conflict_against_expected` /
`classify_conflict_against_target` then compare kind, content hash, **and** `mode & 0o7777`:

- expected has the path: `Absent` → `DeletedExternally`; `Directory`/`NonRegular` → `ModifiedExternally`;
  `Regular` with a different hash **or** mode → `ModifiedExternally`.
- expected lacks the path (it is only in the target): any live entry that is not already identical to
  the target → `CreatedExternally`. A real directory that is a legitimate ancestor of an
  expected-tracked file (e.g. `foo` when `foo/bar.txt` is tracked) is *not* a conflict.
- `current_expected_rev == None` and `!force` now compares the live state against the target instead
  of skipping the check.

`classify_blocked_parent` distinguishes "an ancestor is a plain file, so the leaf cannot exist"
(`Absent`) from "an ancestor is a symlink" (`NonRegular`), so a symlinked parent directory can never
be written through.

**Tests:** `test_external_symlink_and_mode_change_are_conflicts` (mode change, created symlink at a
target-only path, symlink swapped onto a tracked path);
`test_symlinks_and_sensitive_files_are_never_captured_or_overwritten` (existing symlink case now
asserts `ModifiedExternally` — a symlink on a tracked path is a modification, not a deletion).

### F2 — [P1] File ↔ directory replacement ordering, empty-directory cleanup, force obstruction

**Was:** `restore_inner` queued every `Write` before every `Remove`, and `remove_tracked_file` never
pruned emptied parent directories. If `foo` was a file in one revision and `foo/bar.txt` in the
other, the restore failed with `ENOTDIR` / `EISDIR` after the intent journal was already written, and
`recover_pending_restore` re-failed the same way — a permanent `RecoveryRequired` wedge. With
`force = true`, a symlink or directory parked on a target path also wedged the restore.

**Now:**

- All `Remove` operations are scheduled **before** all `Write` operations, sorted deepest path first,
  and the affected set already contains both `foo` and `foo/bar.txt` so both are vacated/created in
  the right order.
- `remove_tracked_file` prunes now-empty ancestor directories with best-effort
  `unlinkat(..., AT_REMOVEDIR)`, deepest first, stopping at the first non-empty directory, so an
  undone `a/b/c/new.rs` leaves no dangling `a`, `a/b`, `a/b/c`.
- With `force = true`, `write_tracked_file` and `remove_tracked_file` clear an obstructing
  symlink/directory first via `remove_path_force`, which removes entries FD-relative
  (`fstatat` + `unlinkat`, listing through `/proc/self/fd/<fd>`) and never follows a symlink.
- `RestoreIntent` gained a `#[serde(default)] force` field so crash recovery re-applies the same
  overwrite semantics.

**Tests:** `test_file_and_directory_kind_swap_restores_in_both_directions`,
`test_nested_new_directory_tree_is_pruned_on_undo`.

### F3 — [P1] Files skipped in the target revision were deleted on restore

**Was:** if a tracked file grew past `MAX_SNAPSHOT_FILE_BYTES` between revisions, the target recorded
it in `skipped_paths` and omitted it from `tracked_files`; restoring to that revision emitted
`Remove` for it and deleted the file.

**Now:** `target.skipped_paths` is exempt from both conflict detection and removal, and
`expected.skipped_paths` is exempt from conflict detection (its content was never recorded).

**Test:** `test_skipped_large_file_is_preserved_on_restore`.

### F4 — [P1] `--restore-rev` left `latest_revision_id()` stale

**Was:** `--restore-rev` rewrote the workspace but appended nothing to the WAL, so
`latest_revision_id()` still named the pre-restore revision and `--list-revisions` still marked it
current. A following `--undo` passed that stale revision as `expected` and reported the freshly
restored files as external modifications (exit 3).

**Now:** `record_restored_revision` appends a `RevisionCaptured` record (restored manifest's changed
paths and tracked-file count, parent = the expected revision) at the current leaf, and
`--list-revisions` marks only the newest record whose content address matches the live workspace.

**Tests:** `harness-cli` unit test
`test_restore_rev_records_revision_so_a_following_undo_stays_in_sync`; E2E step
`--restore-rev <baseline>` followed by `--undo` (both exit 0).

### F5 — [P1] `--resume` baseline capture ordering and `current_revision_id`

**Was:** on `--resume`, `AgentRunner::run` appended the new `UserMessage` (advancing `leaf_id`) and
*then* captured the baseline, so the baseline record was bound to a leaf with a turn greater than its
own `turn`. A later `--undo` → `fork_at_turn(baseline.turn)` selected the pre-run leaf, evicted the
baseline record from the active branch, and left `latest_revision_id()` describing a revision that
was not on disk. `current_revision_id` was also initialised to `None`, so every resumed baseline was
recorded with `parent_revision_id: None`.

**Now:** the baseline revision is captured **before** the task message is appended (bound to the
pre-run leaf and `session_turn_offset`), and `with_session_journal` seeds `current_revision_id` from
`journal.latest_revision_id()`.

**Test:** `test_resumed_run_baseline_survives_undo_rewind` (verified to fail with the old ordering).

### F6 — [P1] Missing Gate 1 tests

Added to `crates/harness-core/tests/gate1_workspace_revision.rs`:

| Test | Covers |
| --- | --- |
| `test_sequential_undo_across_three_mutating_turns_rewinds_to_the_baseline` | `T3 → T2 → T1 → T0` sequential `--undo`, active-branch revision count at each step |
| `test_fork_then_new_mutation_and_cross_branch_restore` | `--fork` at an earlier turn, new mutation on the new branch, restore of a revision from the abandoned branch |
| `test_nested_new_directory_tree_is_pruned_on_undo` | `a/b/c/new.rs` creation + undo prunes `a/b/c`, `a/b`, `a` |
| `test_file_and_directory_kind_swap_restores_in_both_directions` | `foo` file ↔ `foo/bar.txt` file |
| `test_external_symlink_and_mode_change_are_conflicts` | external `chmod`, created symlink, symlink swap |
| `test_skipped_large_file_is_preserved_on_restore` | >4 MiB file recorded in `skipped_paths` is never removed |
| `test_resumed_run_baseline_survives_undo_rewind` | `--resume` baseline binding + undo rewind |

Plus one `harness-cli` unit test for F4.

---

## 2. Deliberately not fixed in this pass (still open)

These are real findings from the same reviews that were **not** in the required fix list. They are
recorded here and in `docs/GATE1_CI_LOG.txt` (R7–R9) so the next Gate 1 pass picks them up.

| ID | Finding | Location |
| --- | --- | --- |
| O1 | `bash` (`SideEffect`) tool mutations never set `batch_mutated`, and a mutating `CompletionGate` verification command is never snapshotted, so the workspace can drift ahead of `latest_revision_id()`. | `agent_loop.rs` `execute_tool_batch` / `run` |
| O2 | `store_blob` writes the final CAS path in place (`create_new` + `write_all`) instead of temp+rename, so a crash mid-write leaves a truncated blob that later captures reject. | `revision.rs` `store_blob` |
| O3 | `ensure_private_dir` relies on `DirBuilder::recursive(true)`, which returns `Ok` for an existing path (including a symlink), making its `AlreadyExists` symlink check dead code; the following `fs::set_permissions` then chmods the symlink target. | `revision.rs` `ensure_private_dir` |
| O4 | `revision_records()` orders by WAL append index rather than topological depth, so an ancestor capture recorded after a fork can shadow a deeper descendant's revision in `latest_revision_id()`. | `session.rs` `revision_records` |
| O5 | No fault-injection test for "snapshot written but commit event not written" or `ENOSPC`. | `gate1_workspace_revision.rs` |
| O6 | No dedicated `RevisionRestored` WAL/audit event and no `SnapshotStore::compare(rev_a, rev_b)` API. | `revision.rs`, `session.rs`, `events.rs` |

---

## 3. Verification

```
cargo build --workspace --all-targets   # 0 warnings
cargo test  --workspace                 # 83 passed / 0 failed
```

Full transcript, including the end-to-end CLI run, in `docs/GATE1_CI_LOG.txt`.
