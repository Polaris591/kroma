# Harness P1 Hardening Report — Session WAL + CompletionGate

Date: 2026-10-12
Workspace: `/opt/harness` (Cargo workspace, no git metadata present)
Binary: `/usr/local/bin/harness` (rebuilt from `/opt/harness/target/debug/harness`, mode 755)

## Summary

Four hardening tasks were completed: the CompletionGate subprocess pipe
deadlock / orphan-process leak was fixed, the `--resume` turn-offset and
hot-file-ledger pinning bugs were fixed, the CLI verification-authorization
bypass was closed and `--verify-cmd` / `--no-verify` were added, and the whole
workspace was re-verified and reinstalled. `cargo test --workspace` passes with
0 failures and 0 warnings.

## Changed files

- `crates/harness-core/src/completion_gate.rs`
- `crates/harness-core/src/agent_loop.rs`
- `crates/harness-core/src/context_projector.rs`
- `crates/harness-core/src/tools/bash.rs` (`cleanup_process_group_after_exit` -> `pub(crate)`)
- `crates/harness-core/src/tool/policy.rs` (new `PolicyMode::allows_shell_auto`)
- `crates/harness-cli/src/main.rs`
- `/usr/local/bin/harness` (installed binary)

---

## Task 1 — CompletionGate pipe deadlock + NormalExit orphan sweep

### Bug

`WorkspaceCompletionGate::check` read stdout/stderr to EOF *before* waiting on
the child (`tokio::join!(stdout_fut, stderr_fut)` then `child.wait()`). A test
suite that backgrounds a grandchild inheriting the pipe handles keeps the pipes
open after the leader exits, so the gate blocked until the full 180 s timeout.
`NormalExit` also never swept the child's process group, leaking background
orphans.

### Fix (aligned with `tools/bash.rs`)

- Both pipes are now drained on background tasks via `spawn_stream_collector`,
  which blocks on `read` while the leader is alive and switches to a per-read
  `PIPE_DRAIN_TIMEOUT` (200 ms, reused from `tools::bash`) once the leader exits.
- The leader is awaited first with `tokio::time::timeout(self.timeout, child.wait())`.
- After the leader exits (normal, timeout, or error) the collectors get their
  200 ms drain window and the shared `HeadTailBuffer` is finalized, so a
  grandchild holding the pipe can no longer hang the gate.
- `cleanup_process_group_after_exit(pgid)` (SIGTERM -> grace -> SIGKILL) is now
  called on **every** exit path, so descendants are swept after the gate returns.

### Tests

`completion_gate::tests::gate_200ms_pipe_drain_prevents_grandchild_hang_and_sweeps_pgid`
runs `sh -c "sleep 30 & printf 'all tests passed\n'; exit 0"` with a 5 s timeout
and asserts `GateOutcome::Approved` is returned in under 1 s. (The pre-fix code
returns a timeout rejection after the full 5 s.)

---

## Task 2 — `--resume` turn offset + permanently-pinned hot files

### Bugs

1. `agent_loop.rs` passed the process-relative `current_turn` (restarting at 1)
   to `projector.project(...)` instead of the absolute session turn, so the
   projector's turn reference was wrong on every `--resume`.
2. `context_projector.rs::compute_working_set_pins` compared
   `turn_counter.saturating_sub(mod_turn)` where `mod_turn` can be an absolute
   checkpoint turn larger than `turn_counter`; the unsigned saturation made the
   difference `0`, so ancient hot files were pinned forever and never compacted.
   `synthesize_remaining_results` also named its parameter `absolute_turn` even
   though `record_transcript_item` adds `session_turn_offset` itself (misleading,
   double-add-prone).

### Fix

- `agent_loop.rs`: `let absolute_turn = self.session_turn_offset.saturating_add(current_turn);`
  and `projector.project(..., absolute_turn)`.
- `agent_loop.rs`: `synthesize_remaining_results` parameter renamed to
  `relative_turn`; every call site passes `budget_tracker.turns_used()` (relative)
  or `0` (recovery) — no double-add, WAL `turn` stays monotonic.
- `context_projector.rs`: `compute_working_set_pins` now uses
  `effective_current_turn = current_turn.max(turn_counter)` and the guarded test
  `effective_current_turn >= mod_turn && effective_current_turn - mod_turn < WORKING_SET_PIN_TURNS`.
- `context_projector.rs`: `refresh_last_touched_from_transcript` keeps the
  larger of the restored checkpoint turn and the transcript-derived turn (never
  regresses a deeper history turn).

### Tests (new `context_projector::tests` module)

- `working_set_pins_release_files_older_than_the_window_under_a_resume_offset`
  — transcript with one assistant turn, ledger `old.rs@1`, `recent.rs@10`,
  current turn 13: `old.rs` released, `recent.rs` pinned. Fails against the old
  `saturating_sub` logic.
- `refresh_last_touched_never_regresses_a_larger_checkpoint_turn`.
- `resumed_compaction_releases_stale_hot_file_and_keeps_recent_file_pinned`
  — turn 1 edits `old.rs`, turn 10 edits `recent.rs`, turn 13 triggers
  high-watermark compaction: `old.rs` becomes an Outline Stub while
  `recent.rs` keeps its Working-Set Pinning exemption.

---

## Task 3 — CLI verification authorization + `--verify-cmd` / `--no-verify`

### Bug

`WorkspaceCompletionGate` could auto-detect and silently execute a repository's
own test script (`cargo test`, `npm test`, ...) under the default `ask` /
`--allow-write` policies — arbitrary host execution without `--allow-shell`.

### Fix

- `completion_gate.rs` gained `allow_auto_detected`; auto-detected manifests are
  only executed when authorized, otherwise the gate returns
  `GateOutcome::Skipped { reason: "auto-detected verification requires --allow-shell or explicit --verify-cmd / HARNESS_VERIFY_COMMAND" }`.
  Explicit commands remain authorized.
- `parse_command_argv(raw)` added: quote-aware argv splitter (single/double
  quotes and `\` escapes), so `sh -c "cargo test --workspace"` splits correctly.
  `from_environment` now uses it for `HARNESS_VERIFY_COMMAND`.
- `configured_timeout()` extracted for `HARNESS_VERIFY_TIMEOUT_SECS`.
- `main.rs`: new `--verify-cmd <CMD>` (priority over `HARNESS_VERIFY_COMMAND`)
  and `--no-verify`. `choose_completion_gate` implements the policy:
  `--no-verify` or `PolicyMode::ReadOnly` -> `AllowAllGate`; explicit command ->
  `WorkspaceCompletionGate` (runs even without `--allow-shell`); otherwise
  auto-detect with `allow_auto = policy_mode.allows_shell_auto()`.
- `PolicyMode::allows_shell_auto()` added (`AllowShell | FullAutonomous`).

### Tests

- `completion_gate::tests::parse_command_argv_preserves_quoted_segments_and_escapes`.
- `completion_gate::tests::auto_detected_verification_without_shell_authorization_returns_skipped`.
- `main.rs` tests `test_verify_cmd_and_no_verify_flags_parse` and
  `test_completion_gate_selection_honors_shell_authorization`.

---

## Task 4 — Verification and installation

- `cargo build --workspace --all-targets`: clean, no warnings/errors.
- `cargo test --workspace`: 49 tests, 0 failed, 0 ignored, exit 0.
  - harness-cli unit: 3
  - harness-core lib: 18 (includes 10 completion_gate + 3 context_projector)
  - integration: context_cache_stability 4, edit_diagnostics 2,
    filesystem_security 4, gate0_config_and_events 3, golden_smoke 1,
    provider_contract 5, shell_supervisor 4, sse_contract 4, tool_contracts 1
- `/usr/local/bin/harness --help` now lists `--resume`, `--fork`,
  `--verify-cmd`, `--no-verify`; `/usr/local/bin/harness --version` -> `harness 0.6.0`.

## Residual risks / notes

- No git repository is present under `/opt/harness`, so no staged files exist
  and no commit was made.
- The CompletionGate drain honours the 200 ms `PIPE_DRAIN_TIMEOUT`; a grandchild
  that keeps writing past the leader exit is drained in successive 200 ms windows
  (same semantics as `tools/bash.rs`).
- `--verify-cmd` is treated as explicit operator authorization and therefore runs
  without `--allow-shell`; this is the intended policy from the task.
