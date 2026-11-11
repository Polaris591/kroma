# Kroma 0.4.0 plugin boundaries and cleanup (2026-11-11)

## Accepted runtime boundary

Kroma's **core** owns the agent loop, tool execution, context projection, the
session WAL and snapshot scope, and four generic seams: the `before_turn_end`
hook, a command reply that may carry one `task`, and the `ui.status` / `ui.ask`
host calls.
**Plugins** own domain instructions, model-facing tool schemas, command
interpretation, their own state, and UI/extension policies. No extra approvals,
planners, verification-model calls or independent goal loops exist.

### Goal

The core has **no goal code**. `src/task_control.rs`, `src/plugins/task_control.rs`,
the host controller API, the `task_control` capability, the `TaskControlChanged`
event, the desktop goal panel and the raw-execution receipt capture were all
removed. `examples/plugins/goal` owns its own per-session file and uses only the
four generic seams.

Scaffolding is deliberately thin: `kroma plugins new <id>` writes a working
`plugin.toml` + `main.js` that already answers `ping`, commands and tools, so a
new author starts from a passing `kroma plugins check` instead of a blank file.

What this costs, stated plainly: the host no longer verifies a goal claim against
real command receipts, and an existing conversation keeps its old archived
`kroma_task_control` items as inert history (they are ignored, not migrated).
The plugin is advisory; you can always stop it with Esc or `/goal cancel`.

### Ask

The host owns the interactive surface: `host = "ask_user"` for a model-facing
tool, and the `ui.ask` host call for a plugin that needs an answer mid-call.
Both render the same overlay and return the same ``AskAnswer``, so a dismissed
question can never be reported as approval. When no interface is attached
(`--print`, pipes) `ui.ask` fails fast and the plugin is told to decide for
itself. In the terminal the question takes over the input bar instead of
covering the conversation; the desktop window draws its own question bar and
answers through `answer_question`. In both, a dismissed question is reported
as `ok:false`, so closing it is never consent. Panel content is likewise
host-rendered: a plugin returns structured blocks, the terminal lays them out
as text and the desktop draws real widgets. This is one narrow host
capability, not a second approval subsystem.

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
