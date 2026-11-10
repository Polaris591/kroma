# Kroma 0.4.0 plugin boundaries and cleanup (2026-11-11)

## Accepted runtime boundary

Kroma's **core** owns the agent loop, tool execution, raw execution receipts, context
projection, session WAL and snapshot scope, and the generic owner-bound task
control state machine. **Plugins** own domain instructions, model-facing tool
schemas, command interpretation, and UI/extension policies. No extra approvals,
planners, verification-model calls or independent goal loops were introduced.

### Goal

The model-visible `goal_checkpoint` schema now requires only `summary` and
`claim`; progress details, criteria, check commands and evidence references
are optional. `evidence[].kind/call_id` remains exposed so a model can cite
real executed calls. The host continues to validate references and check
execution facts before completing a goal.

The core's `task_control.rs` still includes `CheckSpec`, `SourceRef`, and
`goal_sources`. They are entangled with serialized WAL/snapshot
compatibility and the existing Goal plugin protocol. **We deliberately did
not move these persisted types into JavaScript or remove them**: that would
change the recovery format for existing users, with no measured runtime
benefit. A future migration should first establish a versioned data adapter
with round-trip tests on old sessions. Line-count reduction is not sufficient
justification for breaking recoverability.

The old extra schema fields remain accepted by the plugin's host protocol and
stored data. They are not advertised to ordinary requests, and schema is not
silently used to erase existing goal state. Advanced structured source
references are intentionally not emitted by the compact model-facing schema;
the original user message remains authoritative.

### Ask

The host still owns `host = "ask_user"`: interactive rendering, response
collection, dismissal and free-text behavior are impossible to implement from
a pure declaration without a host callback. This is a legitimate narrow
host implementation, not a second approval subsystem.

### Model capability identity

One `uses_deepseek_tool_reasoning()` method is shared by the Chat request
serializer, budget projection, and summary requests. It uses an exact official
catalog identity when present; explicit `deepseek_thinking` configuration and
legacy custom-proxy model-name fallback remain in place. This is **not**
equivalent to treating any arbitrary proxy as an official DeepSeek endpoint.
The remaining name heuristic is a documented compatibility fallback, not an
independent primary capability database.

Compatibility tests cover official DeepSeek models, unlisted reverse proxy
names, explicitly enabled reasoning, and ordinary OpenAI-compatible models.
Future work can remove the fallback only after profiles declare a separate
reasoning-replay capability explicitly and existing proxy users have migration
tests.

## Static request cost

`benchmarks/round18_plugin_manifest_cost.py` compares pinned old manifests
with the new manifests using Kroma's conservative character-class estimator.
No model/API calls are made, and these are not invoice/uncached-token counts.

| Always-visible contribution | Before estimated tokens | After | Saved |
|---|---:|---:|---:|
| Goal tool schema | 1269 | 429 | 840 |
| Goal prompt | 163 | 98 | 65 |
| Ask prompt | 260 | 73 | 187 |
| **Total changed fields** | **1692** | **600** | **1092** |

Ask's host-defined tool JSON schema is unchanged; the script intentionally
does not misrepresent it as newly reduced. Cache reuse and provider tokenizer
differences mean actual billed savings may be lower.

## Production experiment removal

The single default coding instruction is byte-identical to the previous
evidence-finishing instruction followed by the V7 quality instruction:
`benchmarks/round18_prompt_parity.py` asserts the complete **2209 UTF-8 bytes**.

All `KROMA_EXPERIMENT_*` branches were removed from production agent/prompt
and tool paths, including the cargo-specific finishing detector, model-only
shell-result rewrite and shell tool rename. Historical prompt alternatives are
stored in `benchmarks/legacy_prompt_arms.rs` for research reference. Old
rounds relying on experiment env switches must run against the pinned earlier
commit, **not the current production binary**.

## Migration and verification

- `model-stream` now responds to the optional check ping using the matching
  request ID and retains error ID correlation.
- Nested project-memory manifests give a copyable install/check path instead
  of an opaque OS missing-file error; the recommended path remains
  `examples/plugins/project-memory/plugin`.
- No stored Goal, Memory, plugin enablement, keys, or user session files are
  altered by this source change. Installed plugin manifests are not silently
  overwritten; a new release or explicit update is required.
- Regression: entire Rust `cargo test --all-targets`, Clippy warnings as
  errors, format check, Goal Node tests and plugin smoke checks.
