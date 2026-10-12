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
| `ScopedToolDefinition`, `CustomToolHandler`, `CustomToolContext` | Extension tool schema + host-side handler; the handler receives only the capability-guarded `WorkspaceCap` |
| `ScopedExtension`, `ExtensionHandle` | One extension's complete contribution (tools + hooks + skills) |
| `ExtensionState` | Shared, interior-mutable mount table owned by `ToolRegistry` |
| `ExtensionError` | `ReservedKernelTool`, `DuplicateExtension`, `UnknownExtension`, `DuplicateTool`, `UnsupportedApiVersion`, `InvalidManifest` |

`ToolRegistry` was refactored into `builtin: Vec<RegisteredTool>` + `Arc<Mutex<ExtensionState>>` so
that:

* `register_extension(ext)` validates the **whole** contribution (reserved names, duplicates, API
  version, empty skill names) *before* mutating anything — a rejected mount is a byte-for-byte no-op.
* `unregister_extension(id)` removes exactly the tools, policy hooks, observer hooks and skills
  owned by that id, and nothing else.
* Kernel tools (`activate_skill`, `bash`, `edit`, `find`, `grep`, `ls`, `read`, `recall_evidence`,
  `write`) are unshadowable: registering any of those names returns
  `ExtensionError::ReservedKernelTool`.
* Extension tools flow through the **same** `lookup_for_dispatch` gate as kernel tools, so
  `--policy read-only` (mutating/side-effect extension tools hidden and denied) and
  `--tool-filter` restrict them identically.
* Deterministic ASCII-ascending schema ordering is preserved with extensions active.

### 1.2 Dual-class hook engine

| Trait / type | Class | Failure behaviour |
|---|---|---|
| `PolicyHook` | **fail-closed** | `Deny`, `Err`, panic, non-zero exit or timeout ⇒ action blocked |
| `ObserverHook` | **fail-open** | any failure ⇒ `failed_open` recorded, action proceeds |
| `CommandPolicyHook` / `CommandObserverHook` | command-backed | run in their own process group, sanitized env, JSON context on stdin, bounded timeout + `SIGTERM`/`SIGKILL` group sweep |
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
  at 8 KiB per directory and 32 KiB total, and appended to the **current tool result only**.
* The static system prefix and every earlier turn stay byte-identical, so the frozen prefix KV cache
  is preserved (asserted by comparing the `system_prompt` of every recorded request).
* Loaded directories are checkpointed into the session WAL (`SessionCheckpoint.subtree_dirs`), so a
  `--resume` does not re-inject; a `--fork` rewind discards them so the rewound branch can reload.
* `.agents/skills/<name>/SKILL.md` is discovered through the same capability guard (symlinked skill
  directories and sensitive paths refused). Optional YAML frontmatter (`name`, `description`) is
  parsed; otherwise the first heading is used.
* Built-in **read-only kernel tool `activate_skill`** (`{ "name": "<skill>" }` or
  `{ "action": "list" }`) returns the body inside
  `<untrusted_skill_context name="...">…</untrusted_skill_context>`, escaped and capped at 16 KiB.
  An unknown name returns `INVALID_ARGUMENTS` **and** lists what is available.

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
| 项目扩展必须经过信任决策 (project extensions require a trust decision) | **Met** | `ExtensionManifest.trust_level` distinguishes `OperatorTrusted` from `ProjectUntrusted`; `api_version` mismatch is rejected; a `checksum` field is carried for re-trust decisions |
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
| 12 | `test_forked_resume_reloads_subtree_guidance_through_the_agent_loop` | end-to-end forked resume reloads guidance and logs `SubtreeContextLoaded` |
| 13 | `test_gate3_event_schema_and_payloads` | schema `0.8.0` and the three new payloads round-trip |

Existing test updated: `crates/harness-core/tests/gate0_config_and_events.rs`
`test_tool_registry_ascii_order_and_policy_visibility` now expects 9 kernel tools
(`activate_skill` added) under every policy mode.

---

## 4. Verification performed

| Check | Result |
|---|---|
| `cargo build --workspace --all-targets` | **0 errors, 0 warnings** |
| `cargo test --workspace` | **113 passed, 0 failed** (was 100 before Gate 3; +13 Gate 3 tests) |
| `install -m 755 target/debug/harness /usr/local/bin/harness` | `harness 0.11.0` |
| `harness --list-skills` on a workspace with one skill | lists `rust-audit` with its path and description |
| `harness --skill nope` | exits 2 with `unknown skill 'nope'; discovered: rust-audit` |
| End-to-end CLI run with `--skill` + `--policy-hook` + `--observer-hook` against a local stub provider | `ExtensionChanged`, `HookEvaluated` (observer `failed_open`), `SubtreeContextLoaded` all persisted; run `Completed`; `system_prompt` byte-identical across both requests; `activate_skill` present in the exposed schema under non-interactive `ask` |
| End-to-end CLI run where the model calls `bash` under `--allow-shell` with a denying `--policy-hook` | hook logged `pre_tool_use -> deny`; `bash` returned an error tool result; the command never ran |

Raw output: `docs/GATE3_CI_LOG.txt`.

---

## 5. Residual risks

1. **In-process hook timeout abandons a blocking thread.** `HookEngine` bounds a synchronous hook
   with `tokio::time::timeout` around `spawn_blocking`. On timeout the call is denied (fail-closed)
   but the blocked thread is not cancellable and is reclaimed only when the hook returns. Command
   hooks are unaffected: they enforce their own inner deadline and `SIGTERM`/`SIGKILL` their process
   group. Mitigation for in-process hooks is the same as `bash`: keep them cheap. Not covered by a
   regression test that asserts thread reclamation.
2. **Extension skills are merged by name, not namespaced.** `ToolRegistry::skills()` sorts and
   de-duplicates by skill name; a workspace skill and an extension skill with the same name resolve
   to the first after sorting (workspace first). There is no conflict diagnostic. Extensions should
   namespace skill names by convention.
3. **Subtree JIT is keyed by directory path, not content.** If `AGENTS.md` is edited mid-run, the
   already-loaded directory is not re-read. This is intentional (it keeps the injected context
   append-only), but it means guidance changes only take effect on a new branch.
4. **`--policy-hook` / `--observer-hook` are operator-supplied commands.** They run with the same OS
   user as the harness (no container), matching the documented `--allow-shell` posture. They do
   receive a sanitized environment and a workspace-scoped cwd, but they are not OS-sandboxed.
5. **`ProjectUntrusted` extensions have no loader yet.** The manifest carries the trust level and the
   registry enforces the invariants, but Gate 3 deliberately ships only the operator-trusted CLI
   path; discovering and trust-prompting project extensions is deferred (blueprint: "先静态 Rust
   crate 与可撤销注册句柄，再引入外部脚本 Hook/MCP").
6. **Gate 2 evaluation harness unaffected but not re-run.** `docs/GATE2_EVAL_METRICS.json` only had
   its `harness_version` field refreshed to `0.11.0`; the Gate 2 A/B numbers were not re-measured
   against the Gate 3 build.

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

---

## 7. Next gate

**Gate 4 — Predictive Validation / Attribution.** Post-`edit` parser/lint/baseline-test runs with
candidate attribution, gated behind explicit opt-in, with measured precision/recall and a
false-positive rollback rate. Gate 3's hook engine is the intended attachment point: a
`PostToolUse` observer can trigger validation without gaining the ability to block.
