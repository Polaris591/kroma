# Gate 5 Acceptance Report — Controlled Subagents & Isolated Worktrees (`v0.13.0`)

## 1. Scope & Blueprint v2.0 Alignment

Gate 5 (`v0.13.0`) implements controlled child subagents (`Scout` & `Worker`) and isolated worktree execution with three-way baseline-aware merge, conflict detection, and post-merge `WorkspaceCompletionGate` verification:

1. **Role Separation (`crates/harness-core/src/subagents.rs`)**:
   - `SubagentRole::Scout`: strictly read-only (`PolicyMode::ReadOnly`), sees only the 6 read-only tools (`activate_skill`, `find`, `grep`, `ls`, `read`, `recall_evidence`), runs against the parent `WorkspaceCap` with zero mutation capability, and returns a bounded `<subagent_report>` without polluting the parent's transcript with intermediate tool outputs.
   - `SubagentRole::Worker`: runs inside an [`IsolatedWorktree`] materialized from the parent workspace's [`SnapshotStore`] revision with its own [`WorkspaceCap`] rooted at the isolated worktree directory (`state_dir/worktrees/wt-<subagent_id>`). It cannot touch or corrupt the parent workspace during execution and does not require `git` or `.git/` to exist.
   - `allowed_paths` / `allowed_write_globs`: enforced both in-flight via a fail-closed `PreToolUse` `PolicyHook` (`AllowedWriteGlobsPolicyHook`) inside the worker runtime and at merge time on every modified/removed path.

2. **Strict Anti-Recursion & Independent Budgets**:
   - `ToolRegistry::without_subagents()` physically removes `spawn_subagent` from all child subagent registries (`max_depth == 1`), and child `AgentRunner`s run with `with_max_subagents(0)`.
   - Independent per-subagent `SubagentBudget` (`max_turns` default `8` / ceiling `16`, `max_tool_calls` default `16` / ceiling `32`, `max_tokens` default `64_000`, `timeout_ms` default `60_000`) and parent session `--max-subagents <N>` (default `8`) / `--no-subagents`.
   - Timeout or parent cancellation immediately signals the child cancellation channel, aborts in-flight execution, sweeps child processes, and removes the isolated worktree directory (`Drop` + explicit `cleanup()`).

3. **Three-Way Baseline-Aware Merge & Post-Merge Verification (`WorktreeMergeResult`)**:
   - `IsolatedWorktree::merge_into_parent` computes the worker's delta (`written_files` and `removed_files`) relative to `base_revision` and verifies every touched path against `allowed_write_globs`, sensitive-path rules, and the parent workspace's live state (`live_entry_state` vs `base_revision`).
   - Any concurrent parent modification or scope violation returns `WorktreeMergeResult::Conflict { base_revision, conflicting_paths, reasons, worker_worktree_preserved: true, .. }` leaving the parent workspace 100% untouched.
   - Clean merges atomically apply removals (deepest-first) and writes via `WorkspaceCap`, record a parent `WorkspaceRevision` + Gate 4 `MutationRecord` (so `--undo` reverts an entire merged worker in one step), and run the post-merge `CompletionGate`.
   - When `post_merge_gate` fails and `revert_on_verify_failure: true` is set, the parent workspace is automatically restored to `revision_before`.

4. **Telemetry & CLI Surface (`events.rs`, `main.rs`)**:
   - Bumped `EVENT_SCHEMA_VERSION` to `"0.10.0"`.
   - Added `EventPayload::SubagentSpawned`, `EventPayload::SubagentCompleted`, and `EventPayload::WorktreeMerged`.
   - Added `--max-subagents <N>` and `--no-subagents` CLI flags.

## 2. Verification Summary

- Workspace version: `0.13.0`
- Event schema version: `0.10.0`
- Built-in kernel tools: `10` (`activate_skill`, `bash`, `edit`, `find`, `grep`, `ls`, `read`, `recall_evidence`, `spawn_subagent`, `write`)
- Test suite: **144 passed, 0 failed, 0 warnings** across 14 test binaries (`gate5_controlled_subagents.rs`: 6 end-to-end integration tests + `docs/GATE5_EVAL_METRICS.json`).
