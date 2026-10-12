# Harness v0.11.0 — Gate 3 Acceptance Report

**Gate:** 3 — Reversible Scoped Plugins / Hooks / Skills
**Version:** `0.11.0` (workspace `Cargo.toml`, binary `harness 0.11.0`)
**Date:** 2026-10-12
**Blueprint:** `docs/v1_blueprint/Harness-v1.0-优化蓝图-v2.0.md` §13 Gate 3,
`docs/v1_blueprint/IMPLEMENTATION_GATES_AND_EVAL.md` §"Gate 3 — Plugins / Hooks / Skills"
**Evidence:** `docs/GATE3_CI_LOG.txt`, `crates/harness-core/tests/gate3_reversible_extensions.rs`

---

## 1. What Gate 3 delivers

Gate 3 is the *reversible extension* gate: it makes tools, hooks and skills mountable and
unmountable at runtime **without** widening kernel authority and **without** disturbing the frozen
context prefix. Three surfaces were added.

### 1.1 Reversible scoped extension registry (`crates/harness-core/src/extensions.rs`)

| Type | Role |
|---|---|
| `ExtensionId`, `ExtensionManifest`, `ExtensionTrustLevel` | Identity, API version, capabilities, trust level, source, optional checksum |
| `ScopedToolDefinition`, `CustomToolHandler`, `CustomToolContext` | Extension tool schema + host-side handler; the handler receives only the capability-guarded `WorkspaceCap` plus a cancellation handle |
| `ScopedExtension`, `ExtensionHandle` | One extension's complete contribution (tools + hooks + skills + optional cleanup callback) |
| `InFlightTracker`, `InFlightCall`, `ExtensionDispatch`, `UnmountReport` | Per-extension in-flight accounting, cancellation signalling and unmount reporting |
| `ExtensionState` | Shared, interior-mutable mount table owned by `ToolRegistry` |
| `ExtensionError` | `ReservedKernelTool`, `ToolNameTaken`, `DuplicateExtension`, `UnknownExtension`, `DuplicateTool`, `DuplicateSkill`, `UndeclaredCapability`, `UntrustedCapability`, `ChecksumMismatch`, `UnsupportedApiVersion`, `InvalidManifest` |

`ToolRegistry` was refactored into `builtin: Vec<RegisteredTool>` + `Arc<Mutex<ExtensionState>>` so
that:

* `register_extension(ext)` validates the **whole** contribution (reserved names, duplicates, API
  version, empty/invalid skill names, skill-name collisions with workspace and other extensions,
  **the capability matrix**, **the trust boundary**, **the pinned source checksum**) *before*
  mutating anything — a rejected mount is a byte-for-byte no-op.
* The **capability matrix** is enforced: a contribution whose kind was not declared in
  `manifest.capabilities` (`tools` / `policy_hook` / `observer_hook` / `skills`) is rejected with
  `UndeclaredCapability`.
* The **trust boundary** is enforced: a `ProjectUntrusted` extension may contribute only `ReadOnly`
  tools, `ObserverHook`s and skills; a `PolicyHook` or a `Mutating`/`SideEffect` tool is rejected
  with `UntrustedCapability`.
* **Source checksum re-trust** is enforced: the first declared `manifest.checksum` is pinned per
  extension id (and the pin survives unmount), so a changed digest is rejected with
  `ChecksumMismatch` until `ToolRegistry::retrust_extension_checksum` is called explicitly.
* `unregister_extension(id)` stops new dispatches immediately, signals cancellation to in-flight
  calls, drains them within a bounded window, runs the extension's cleanup callback(s) in reverse
  order, removes exactly the tools, policy hooks, observer hooks and skills owned by that id, and
  returns an `UnmountReport { extension_id, cancelled_in_flight, drained_in_flight,
  irreversible_side_effects }`.
* Kernel tools (`activate_skill`, `bash`, `edit`, `find`, `grep`, `ls`, `read`, `recall_evidence`,
  `write`) are unshadowable: registering any of those names returns
  `ExtensionError::ReservedKernelTool`; a name already taken by another mounted extension returns
  `ToolNameTaken`.
* Extension tools flow through the **same** `lookup_for_dispatch` gate as kernel tools, so
  `--policy read-only` (mutating/side-effect extension tools hidden and denied) and
  `--tool-filter` restrict them identically.
* Deterministic ASCII-ascending schema ordering is preserved with extensions active.

### 1.2 Dual-class hook engine

| Trait / type | Class | Failure behaviour |
|---|---|---|
| `PolicyHook` | **fail-closed** | `Deny`, `Err`, panic, non-zero exit or timeout ⇒ action blocked |
| `ObserverHook` | **fail-open** | any failure ⇒ `failed_open` recorded, action proceeds |
| `CommandPolicyHook` / `CommandObserverHook` | command-backed | run in their own process group, sanitized env, JSON context on stdin; stdout/stderr are drained by bounded readers started **before** the child is awaited, the process group is swept with `SIGTERM`+`SIGKILL` on *every* exit path, and the post-exit pipe drain is deadline-bounded (200 ms) so a grandchild holding the pipe can never hang the hook |
| `HookEngine` | orchestrator | `evaluate_policy` / `run_observers`, per-hook timeout, `catch_unwind` panic containment |

* Events: `PreToolUse`, `PostToolUse`, `PreCompletion`, `TurnCompleted`, `RunCompleted`.
* `HookContext`: turn, tool name, call id, raw arguments, relative path, outcome, workspace root —
  no credentials, no paths outside the workspace.
* **A `PolicyHook` can only restrict.** Kernel `PolicyEngine::evaluate` runs *after* the hooks and
  independently of them, so a hook returning `Allow` cannot turn a kernel denial into an execution
  (proved by `test_policy_hook_fails_closed_on_deny_error_and_timeout` case (g)).
* `PreCompletion` denial is fed back to the model as a retry instruction, bounded by the existing
  `HARNESS_GATE_MAX_RETRIES` budget.
* Every evaluation is persisted as `EventPayload::HookEvaluated`.

### 1.3 Subtree JIT context and on-demand skills

* `SubtreeContextTracker` walks the ancestor directories of a tool's relative path (root `.`
  excluded — it is already in the static prefix) and, for any directory not yet loaded **on the
  active branch**, reads `AGENTS.md` (falling back to `CLAUDE.md`) through
  `WorkspaceCap::open_existing_no_symlink` (`openat2(RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS)`).
* Guidance is escaped with the same fence/control/Bidi rules as `AGENTS.md` in `prompt.rs`
  (generalized into `fence_untrusted` / `sanitize_untrusted_doc` / `escape_control_chars`), capped
  at 8 KiB per directory and 32 KiB total (the cumulative cap sizes the body so the whole rendered
  fence fits the remaining budget), and appended to the **current tool result only**.
* A directory that does not exist yet is not marked as visited, so an `AGENTS.md` created later in
  the same run is still loaded; a tool that targets a directory (`ls a/b`) also loads that
  directory's own guidance.
* The static system prefix and every earlier turn stay byte-identical, so the frozen prefix KV cache
  is preserved (asserted by comparing the `system_prompt` of every recorded request).
* Loaded directories are checkpointed into the session WAL (`SessionCheckpoint.subtree_dirs`) at the
  start of every turn **and after every tool batch**, so a `--resume` does not re-inject them and a
  run that ends right after a batch does not lose them.
* A `--fork <TURN>` rewinds the session leaf *before* the checkpoint is read, so the rewound
  branch's `subtree_dirs` are restored exactly like a plain `--resume` (directories whose guidance is
  still present in that branch's transcript are never re-injected, while unvisited subtrees still
  load JIT).
* `.agents/skills/<name>/SKILL.md` is discovered through the same capability guard (symlinked skill
  directories and sensitive paths refused). Optional YAML frontmatter (`name`, `description`) is
  parsed; a `name:` outside `[A-Za-z0-9._-]` is ignored in favour of the directory name.
* Built-in **read-only kernel tool `activate_skill`** (`{ "name": "<skill>" }` or
  `{ "action": "list" }`) returns the body inside
  `<untrusted_skill_context name="...">…</untrusted_skill_context>`, escaped and capped at 16 KiB.
  An unknown name returns `INVALID_ARGUMENTS` **and** lists what is available.
* `recall_evidence` is dispatched by the agent loop (it needs the live transcript) but runs through
  the *same* `PreToolUse` `PolicyHook` gate, kernel `PolicyEngine::evaluate` and `PostToolUse`
  `ObserverHook` pipeline as every other tool.

### 1.4 Telemetry

`EVENT_SCHEMA_VERSION` bumped `0.7.0` → `0.8.0`, with three new payloads:
`HookEvaluated { hook_id, hook_kind, event, outcome, duration_ms, message }`,
`ExtensionChanged { extension_id, action }`,
`SubtreeContextLoaded { directory, file_name, bytes }`.

### 1.5 CLI surface

| Flag | Behaviour |
|---|---|
| `--list-skills` | Discover and print the skill catalog, exit 0 (no provider call) |
| `--skill <NAME>` | Pre-activate a skill into the initial untrusted context; unknown name is a hard error listing the catalog |
| `--policy-hook <CMD>` | Attach a command-backed fail-closed `PreToolUse` policy hook |
| `--observer-hook <CMD>` | Attach a command-backed fail-open `PostToolUse` observer hook |
| `--no-subtree-context` | Disable automatic JIT subtree loading |

Both hook flags are mounted through the ordinary extension registry as the operator-trusted
`cli-hooks` extension, so they are removable by the same `unregister_extension` path.

---

## 2. Exit criteria (blueprint) vs. evidence

| Blueprint Gate 3 exit criterion | Status | Evidence |
|---|---|---|
| 初始化失败回滚 (init failure rolls back) | **Met** | `register` validates the entire contribution before committing; reserved-name, duplicate, and bad-API-version mounts are asserted to leave the registry unchanged |
| unmount 清理所有注册 (unmount cleans every registration) | **Met** | `test_extension_register_dispatch_and_byte_identical_unregister` asserts byte-for-byte schema restoration; `test_extension_mount_unmount_is_leak_free_over_many_cycles` runs 1000 mount/unmount cycles with zero accumulation |
| 权限不可由插件提升 (plugins cannot escalate privilege) | **Met** | `RESERVED_KERNEL_TOOLS` unshadowable; extension tools pass the kernel `PolicyEngine` + `--tool-filter`; handlers receive only `WorkspaceCap`; command hooks get `env_clear()` + `build_sanitized_env()` (no `HARNESS_*`/`*_API_KEY`/`*_SECRET`/`*_TOKEN`) |
| 项目扩展必须经过信任决策 (project extensions require a trust decision) | **Met** | `ProjectUntrusted` extensions are refused a `PolicyHook` or any non-`ReadOnly` tool with `UntrustedCapability`; `api_version` mismatch is rejected; `capabilities` are enforced against the actual contribution; a declared `checksum` is pinned and a changed digest is refused until re-trusted |
| 能力矩阵与声明一致 (declared capabilities match the contribution) | **Met** | `test_capability_matrix_and_untrusted_extension_restrictions_and_checksum_retrust` (undeclared `tools`/`policy_hook` rejected) |
| unmount 停止新调用、取消/等待 in-flight、执行 cleanup、报告不可撤销副作用 | **Met** | `test_inflight_extension_tool_unmount_cancels_and_reports_side_effects` (new calls refused, in-flight call signalled + drained, cleanup callback ran, `irreversible_side_effects` reported) |
| Hook 权限绕过 / 插件用 allow 覆盖 Deny | **Met** | fail-closed engine + kernel policy evaluated independently after hooks; asserted in case (g) |
| Policy/Observer 分离 + 各自 timeout | **Met** | two distinct traits and two evaluation paths; per-hook and engine-level timeouts both covered |
| 注册/卸载 1000 次不累积监听器/句柄 | **Met** | `test_extension_mount_unmount_is_leak_free_over_many_cycles` |
| 恶意插件尝试覆盖 Deny、改预算、伪造结果 | **Met (partial, see §5)** | deny-override and result-forgery paths covered; budget mutation is out of the extension API surface entirely (no `BudgetConfig` handle is reachable from a handler or hook) |

Additional invariant from the task brief — *"calling `unregister_extension` restores the active tool
schemas, hooks, and skills byte-for-byte to the pre-registration state with zero leaked state"* — is
directly asserted on `visible_definitions` JSON, `hook_engine().is_empty()`, `all_registered().len()`
and `skills().len()`.

---

## 3. Test inventory (`crates/harness-core/tests/gate3_reversible_extensions.rs`)

| # | Test | Invariant |
|---|---|---|
| 1 | `test_extension_register_dispatch_and_byte_identical_unregister` | mount → dispatch → unmount is byte-identical; reserved names rejected; `ReadOnly` blocks mutating extension tools; `--tool-filter` applies |
| 2 | `test_extension_mount_unmount_is_leak_free_over_many_cycles` | 1000 mount/unmount cycles accumulate nothing |
| 3 | `test_policy_hook_fails_closed_on_deny_error_and_timeout` | deny / error / panic / in-process timeout / non-zero exit / command timeout all block; an allowing hook cannot override a kernel denial |
| 4 | `test_policy_hook_denial_is_logged_and_returned_to_the_model` | `POLICY_DENIED` reaches the model and `HookEvaluated` is persisted |
| 5 | `test_pre_completion_policy_hook_blocks_completion_and_run_observers_fire` | `PreCompletion` denial feeds back a retry instruction and terminates at the retry budget |
| 6 | `test_observer_hook_fails_open_on_error_and_timeout` | failing / panicking / slow / non-zero-exit observers are `failed_open` and never block |
| 7 | `test_failing_observers_never_abort_the_run` | a run with broken `PostToolUse`+`TurnCompleted`+`RunCompleted` observers still completes |
| 8 | `test_subtree_jit_agents_md_loads_once_escapes_tags_and_preserves_prefix_cache` | load-once per branch, fence escape, symlink refusal, byte caps, byte-identical system prefix |
| 9 | `test_subtree_context_can_be_disabled` | `--no-subtree-context` and `ancestor_dirs` boundary handling |
| 10 | `test_skills_discovery_activate_skill_tool_and_tag_sanitization` | discovery, frontmatter parsing, symlink skip, fencing/escaping, listing, unknown-name error, `ReadOnly` visibility, extension-contributed skills, `--skill` injection |
| 11 | `test_fork_rewind_resets_subtree_jit_state` | checkpoint round-trip, `reset_for_fork`, reloadability |
| 12 | `test_final_turn_subtree_dirs_persisted_in_checkpoint_and_preserved_on_fork` | the post-batch checkpoint persists the JIT-loaded dirs; a `--fork` rewind restores the rewound branch's dirs (no re-injection) while unvisited subtrees still load |
| 13 | `test_gate3_event_schema_and_payloads` | schema `0.8.0` and the three new payloads round-trip |
| 14 | `test_capability_matrix_and_untrusted_extension_restrictions_and_checksum_retrust` | undeclared capability rejected; `ProjectUntrusted` blocked from `PolicyHook`/`Mutating`/`SideEffect`; changed source checksum refused until re-trusted |
| 15 | `test_inflight_extension_tool_unmount_cancels_and_reports_side_effects` | unmount stops new calls, signals + drains the in-flight call, runs cleanup and reports irreversible side effects |
| 16 | `test_command_hook_grandchild_holding_stdout_pipe_does_not_hang` | a hook that backgrounds `sleep 30` (and one that floods the pipe) returns promptly |
| 17 | `test_duplicate_skill_collision_rejected_preserving_reversibility` | workspace/extension/intra-extension skill collisions and invalid names rejected without residue |
| 18 | `test_recall_evidence_runs_pre_and_post_tool_hooks` | `recall_evidence` is gated by `PreToolUse` `PolicyHook`s and observed by `PostToolUse` `ObserverHook`s |
| 19 | `test_subtree_jit_multi_ancestor_order_and_total_cap` | outer-to-inner ancestor ordering and the cumulative 32 KiB budget |

Existing test updated: `crates/harness-core/tests/gate0_config_and_events.rs`
`test_tool_registry_ascii_order_and_policy_visibility` now expects 9 kernel tools
(`activate_skill` added) under every policy mode.

---

## 4. Verification performed

| Check | Result |
|---|---|
| `cargo build --workspace --all-targets` | **0 errors, 0 warnings** |
| `cargo test --workspace` | **119 passed, 0 failed** (was 113 before the post-review hardening; +6 review-driven tests) |
| `install -m 755 target/debug/harness /usr/local/bin/harness` | `harness 0.11.0` |
| `harness --list-skills` on a workspace with one skill | lists the skill with its path and description |
| `harness --skill nope` | exits 2 with `unknown skill 'nope'; discovered: …` |
| End-to-end CLI run with `--skill` + `--policy-hook` + `--observer-hook` against a local stub provider | `ExtensionChanged`, `HookEvaluated` (observer `failed_open`), `SubtreeContextLoaded` all persisted; run `Completed`; `system_prompt` byte-identical across both requests; `activate_skill` present in the exposed schema under non-interactive `ask` |
| End-to-end CLI run where the model calls `bash` under `--allow-shell` with a denying `--policy-hook` | hook logged `pre_tool_use -> deny`; `bash` returned an error tool result; the command never ran |
| **Post-review** end-to-end CLI A/B against a local OpenAI-compatible stub provider | with a denying `--policy-hook` the `read` call returns `error` and the log holds `pre_tool_use` + `deny_policy_hook`; with an allowing hook the same `read` returns `success` and the hook logs `allow`. Proves the capability-matrix/trust enforcement did not break the operator-trusted `cli-hooks` mount |

Raw output: `docs/GATE3_CI_LOG.txt`.

---

## 5. Residual risks

1. **In-process hook timeout abandons a blocking thread.** `HookEngine` bounds a synchronous hook
   with `tokio::time::timeout` around `spawn_blocking`. On timeout the call is denied (fail-closed)
   but the blocked thread is not cancellable and is reclaimed only when the hook returns. Command
   hooks are unaffected: they enforce their own inner deadline and `SIGTERM`/`SIGKILL` their process
   group. Mitigation for in-process hooks is the same as `bash`: keep them cheap. Not covered by a
   regression test that asserts thread reclamation.
2. **Extension skills are merged by name, not namespaced.** A skill name that collides with a
   workspace skill or another extension's skill is now **rejected** at mount time
   (`ExtensionError::DuplicateSkill`), so there is no silent overwrite and unmount stays reversible;
   extensions should still namespace skill names by convention to avoid needing the rejection.
3. **Subtree JIT is keyed by directory path, not content.** If `AGENTS.md` is edited mid-run, the
   already-loaded directory is not re-read. This is intentional (it keeps the injected context
   append-only), but it means guidance changes only take effect on a new branch.
4. **`--policy-hook` / `--observer-hook` are operator-supplied commands.** They run with the same OS
   user as the harness (no container), matching the documented `--allow-shell` posture. They do
   receive a sanitized environment and a workspace-scoped cwd, but they are not OS-sandboxed.
5. **`ProjectUntrusted` extensions have no loader yet.** The manifest carries the trust level, the
   registry now *enforces* it (a project extension cannot register a `PolicyHook` or a non-read-only
   tool, and a changed source checksum must be re-trusted), but Gate 3 deliberately ships only the
   operator-trusted CLI path; discovering and trust-prompting project extensions is deferred
   (blueprint: "先静态 Rust crate 与可撤销注册句柄，再引入外部脚本 Hook/MCP"). The trust pin is
   in-memory only — it is not persisted to disk, so a restart re-pins from the first mount of the
   process.
6. **Gate 2 evaluation harness unaffected but not re-run.** `docs/GATE2_EVAL_METRICS.json` only had
   its `harness_version` field refreshed to `0.11.0`; the Gate 2 A/B numbers were not re-measured
   against the Gate 3 build.
7. **Unmount drain is a bounded cooperative wait.** `unregister_extension` signals cancellation and
   waits at most 250 ms for in-flight handlers to observe it. A handler that never polls
   `CustomToolContext::is_cancelled` is reported as `cancelled_in_flight` and keeps running to
   completion; the harness cannot preempt a synchronous in-process handler.

---

## 6. Files changed

**Added**
* `crates/harness-core/src/extensions.rs` (registry, hook engine, subtree tracker, skills)
* `crates/harness-core/tests/gate3_reversible_extensions.rs`
* `GATE3_ACCEPTANCE_REPORT.md`, `docs/GATE3_CI_LOG.txt`

**Modified**
* `Cargo.toml`, `Cargo.lock` — workspace version `0.10.0` → `0.11.0`
* `crates/harness-core/src/lib.rs` — export `extensions`
* `crates/harness-core/src/prompt.rs` — public `fence_untrusted` / `sanitize_untrusted_doc` /
  `escape_control_chars` / `escape_fence_attribute`; `with_activated_skill`
* `crates/harness-core/src/env.rs` — `build_sanitized_env`
* `crates/harness-core/src/events.rs` — schema `0.8.0`, three new payloads, live-stderr rendering
* `crates/harness-core/src/session.rs` — `SessionCheckpoint.subtree_dirs` + WAL round-trip
* `crates/harness-core/src/tool/definition.rs` — register `activate_skill`
* `crates/harness-core/src/tool/registry.rs` — extension mount points, combined lookup/visibility
* `crates/harness-core/src/tool/runtime.rs` — pre-tool policy hooks, post-tool observers,
  `activate_skill` and extension dispatch, hook telemetry return value
* `crates/harness-core/src/agent_loop.rs` — subtree JIT application, hook telemetry logging,
  `PreCompletion` gating, `TurnCompleted`/`RunCompleted` observers, `build_runtime_with_registry`
* `crates/harness-cli/src/main.rs` — five new flags, skill pre-validation, extension mount,
  subtree tracker wiring
* `crates/harness-core/tests/gate0_config_and_events.rs` — 9-tool expectations
* `crates/harness-core/tests/filesystem_security.rs`, `tests/shell_supervisor.rs` — `execute_call`
  arity
* `docs/GATE2_EVAL_METRICS.json` — version field

**Modified in the post-review hardening round**
* `crates/harness-core/src/extensions.rs` — capability/trust/checksum enforcement, skill-collision
  and name validation, `InFlightTracker`/`UnmountReport`/`ExtensionDispatch`, cleanup callbacks,
  hardened `CommandHookRunner`, subtree candidate dirs + negative-cache fix, fence-aware body cap
* `crates/harness-core/src/tool/registry.rs` — workspace skill collision check, `extension_dispatch`,
  `retrust_extension_checksum`, `unregister_extension` returns `UnmountReport`
* `crates/harness-core/src/tool/runtime.rs` — `pre_tool_hook_gate` / `post_tool_hooks` extraction,
  post-hook epilogue for every outcome, in-flight-gated extension dispatch
* `crates/harness-core/src/agent_loop.rs` — `subtree_dirs` restoration, post-batch checkpoint,
  `recall_evidence` hook pipeline, loop-guard rollback
* `crates/harness-core/src/loop_guard.rs` — `rollback_recorded`
* `crates/harness-core/src/prompt.rs` — `escape_fence_attribute` completeness, `</` neutralization,
  caller-tag check, entity-safe truncation, `fence_overhead_bytes`
* `crates/harness-cli/src/main.rs` — fork restores `subtree_dirs` from the rewound checkpoint
* `crates/harness-core/tests/gate3_reversible_extensions.rs` — capability-declaring `manifest()`
  helper, 6 new tests, fork/checkpoint test rewritten for the corrected semantics

---

## 7. Post-review hardening (3-way Gemini audit)

Three independent read-only reviewers audited the first `v0.11.0` cut. Every finding was fixed and
has a regression test.

| # | Finding | Fix |
|---|---|---|
| P0 | `CommandHookRunner::run` read stdout/stderr with an unbounded blocking `read_to_string` *after* the child exited, and only sent `SIGTERM`; a grandchild inheriting the pipes (or >64 KiB of output) could hang or OOM the hook | bounded reader threads started immediately after `spawn` (`Read::take(4097)`), `SIGTERM`+`SIGKILL` process-group sweep on **every** exit path, and a 200 ms deadline-bounded drain (`mpsc::recv_timeout`) |
| P0 | `ExtensionManifest.capabilities`, `ExtensionTrustLevel::ProjectUntrusted` and `checksum` were inert fields never inspected at mount time | capability matrix, trust boundary and checksum re-trust are now enforced in `ExtensionState::register` (new errors `UndeclaredCapability`, `UntrustedCapability`, `ChecksumMismatch`; new `ToolRegistry::retrust_extension_checksum`) |
| P1 | Unchecked skill-name collisions overwrote another extension's skill and broke reversibility; `skills()` silently de-duplicated against workspace skills | `DuplicateSkill` is returned before any commit for intra-extension, cross-extension and workspace collisions; skill names are restricted to `[A-Za-z0-9._-]` |
| P1 | No in-flight tracking, cancellation/drain or cleanup on unmount, and no `UnmountReport` | `InFlightTracker` + `InFlightCall` guard per extension, `CustomToolContext::is_cancelled`, bounded drain window, reverse-order cleanup callbacks and `UnmountReport` |
| P1 | Unvalidated skill frontmatter and an incomplete `escape_fence_attribute` allowed `<untrusted_skill_context>` fence/header injection | `escape_fence_attribute` escapes `&`, `<`, `>`, `"`, `'`; frontmatter `name:` is charset-validated; the `activate_skill` header and listing use the same escaping |
| P1 | `recall_evidence` bypassed the `PreToolUse`/`PostToolUse` hook pipeline | `ToolRuntime::pre_tool_hook_gate` / `post_tool_hooks` are shared with the agent loop's recall branch, which now also runs `PolicyEngine::evaluate` |
| P1 | `--fork <TURN>` skipped `subtree_dirs` restoration and `AgentRunner` never restored them | the CLI restores from `latest_active_checkpoint()` for forks too, and `AgentRunner::run_inner` restores them for library callers |
| P1 | Session checkpoints were only written at turn start, losing the final batch's `subtree_dirs` | a post-batch `persist_session_checkpoint` runs before any abort path |
| P1 | `LoopGuard::check_and_record` recorded calls that a `PreToolUse` hook then denied | `LoopGuard::rollback_recorded` removes the signature on a `deny_policy_hook` decision (serial and parallel sub-batches) |
| P2 | `PostToolUse` observers were skipped on kernel-policy, path-escape, approval and TOCTOU denials | `execute_call` is split into `execute_call_inner` + a single post-hook epilogue that runs for every outcome |
| P2 | `SubtreeContextTracker` permanently cached negative lookups and ignored directory targets | a not-yet-existing directory is not marked visited, and `candidate_dirs` adds the target itself when it is an existing directory |
| P2 | A cross-extension tool collision reported `ReservedKernelTool` | new `ExtensionError::ToolNameTaken` |
| P2 | `sanitize_untrusted_doc` missed a truncated `</…` prefix; `fence_untrusted` ignored a caller-supplied tag and could slice mid-entity | `</` now forces angle-bracket escaping, the caller's `tag` is checked too, and truncation stops before a dangling `&` |
| P2 | The subtree JIT test missed ancestor ordering and the cumulative cap | `test_subtree_jit_multi_ancestor_order_and_total_cap`, and the cumulative cap now sizes the body so the whole fence fits |

Reviewer claim that the report's trust-decision row contradicted residual risk #5: the row now
reflects the *enforced* trust boundary, and risk #5 states precisely what is still deferred (no
project-extension loader, in-memory trust pin).

---

## 8. Next gate

**Gate 4 — Predictive Validation / Attribution.** Post-`edit` parser/lint/baseline-test runs with
candidate attribution, gated behind explicit opt-in, with measured precision/recall and a
false-positive rollback rate. Gate 3's hook engine is the intended attachment point: a
`PostToolUse` observer can trigger validation without gaining the ability to block.
