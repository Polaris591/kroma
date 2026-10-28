# Kroma P0-P4: Architecture Foundation delivery

## Scope

Starting revision: `6f360c2`. Behavior-preserving Rust architecture work,
**not** a new plugin runtime and **not** a change to TRC/APEC algorithms.
Keep one Cargo package and the current default Shell.

## P0 — freeze behavior

- `benchmarks/p11_architecture_baseline.py` uses an offline streaming mock
  provider and two successive Kroma invocations against one saved session.
  It checks the default Shell function schema, tool-call order, tool outputs,
  thinking-content continuity, effective system prompt projection, resume
  behavior, and WAL creation.
- Existing P8/P9/P10 tests serve as independent regressions for HTTP faults,
  cancellation, huge outputs, legacy overflow and malformed config.

## P1 — separate runtime and host

- Added `lib.rs` so Kroma is an embeddable Rust library as well as a CLI.
- Moved default startup wiring into `app.rs`; the CLI retains its behavior.
- Consumer-neutral `protocol.rs` supplies shared message types. Runtime
  components no longer import message types via the HTTP provider module.

## P2 — model and tools

- `model.rs`: the object-safe `ModelBackend` interface for streaming
  completion and tool-free memory summarization. The existing HTTP/SSE
  client is the default implementation. It receives tool definitions instead
  of importing the Shell registry.
- `tools.rs`: `ToolRegistry` + `Tool`, with stable registration order and
  exact preservation of the built-in Bash/Shell schema. The Agent executes
  multiple tool calls sequentially, with WAL writes between results.
- `tests/architecture_backends.rs`: inject a fake model and register a new
  tool, without editing the Agent loop.

## P3 — context policy and durable state

- `context_state.rs`: built-in `CompactionState` plus an optional, versioned
  `PersistedContext::External { strategy, version, data }` envelope.
  The default built-in state remains byte-identical in the existing
  `compression` JSON field. Existing v3 snapshot+WAL histories still load.
- `ContextPolicy` / `ContextFactory`: an internal replaceable policy
  boundary. An implementation can maintain its own state and projections,
  while the runtime keeps full raw history.
- `ports.rs`: `ContextCommit` is a small shared interface. The session
  implements it and **still owns the durable commit**. Background APEC
  continues to verify raw-history snapshots before changing state, and writes
  state before changing the in-memory version. No algorithm changes.
- Integration test verifies a custom counter-based strategy is restored after
  a process restart with the raw tool history preserved; the builtin factory
  refuses to silently load an unknown external strategy.

## P4 — prompt and event boundaries

- `prompt.rs`: projection-only base prompt, the original execution guidance,
  optional appended instructions, and optional base prompt replacement.
  Default prompt content and ordering are unchanged and do not become
  duplicated after restarting a session.
- `events.rs`: a lightweight synchronous `EventSink` for task, model,
  text-delta, context, and tool events. Default execution has **no event
  subscriber**, with no event queue, bus, or extra disk writes.
- An integration test confirms the event order and the prompt contribution
  is injected exactly once for each model request, including after resume.

## Verified behavior

```sh
cargo fmt --check
cargo clippy --locked --offline --all-targets -- -D warnings
cargo test --locked --offline -q
cargo build --release --locked --offline -q
python3 benchmarks/p11_architecture_baseline.py
python3 benchmarks/p9_batch1_regression.py
python3 benchmarks/p9_batch1_legacy_regression.py
python3 benchmarks/p9_batch2_regression.py
python3 benchmarks/p10_config_regression.py
python3 benchmarks/mock_profile_smoke.py
```

Verified after P4: 73 default Rust unit tests + 4 architecture integration
tests passed (8 optional ignored), clippy/format/release build passed.
All listed deterministic end-to-end tests passed. The available frozen
multi-turn wire fixture was independently checked unchanged.

## Explicit non-goals and next steps

- **Not implemented**: dynamic plugin loading, a stable third-party ABI,
  plugin install/enable/config UI, MCP, Skills, agent queues, subagents,
  hot reload, cross-platform tool support, remote control or custom UI.
- No new provider calls, changes to context thresholds, TRC/APEC heuristics,
  model-visible default prompt, SessionStore journaling frequency, or Shell
  execution authority.
- The current interfaces are **internal architecture boundaries**; do not
  promise binary compatibility yet. Plugin execution technology (Rust ABI,
  WASM, JS, MCP transport) should be decided after practical extension
  requirements and loading constraints are established.
- Event callbacks are synchronous, best for lightweight UI/test observers;
  remote streaming and event buffering should be designed separately when
  needed. An active Agent future is not yet a public remote-task scheduler.
- The architecture refactor is not claimed to improve model speed or token
  usage. Run an explicit same-model A/B before making performance claims.
