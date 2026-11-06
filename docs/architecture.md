# Runtime ownership and module boundaries

Kroma has one foreground Agent loop, shared by CLI/JSONL, TUI and the optional
Desktop host. Hosts own interaction and presentation, not a second copy of tool
execution, context compaction or crash recovery. They need not expose identical
UI features. This note describes current ownership, not a proposed framework.

## Core and composition

| Responsibility | Owner | Boundary |
| --- | --- | --- |
| Default composition | `app.rs` | Resolve config/plugin model and budget; let the Host customize; capture Host tool/prompt bases; apply configured plugin overlays. |
| Foreground orchestration | `agent.rs` | Model/tool sequencing, steering and task outcome. Do not put host rendering here. |
| Model protocol and requests | `model.rs`, `provider.rs`, `provider/*` | ModelBackend and protocol-specific payload/stream validation. Tools are supplied by the runtime. |
| Request context | `context.rs`, `prompt.rs`, `attachments.rs` | Derived projections and budget checks; never replace authoritative raw history with a projection. |
| Durable conversation | `session.rs`, `context_state.rs`, `ports.rs` | SessionStore owns WAL, replay/recovery and context commits. Notifications are not a journal. |
| Session catalog | `sessions.rs`, `catalog_file.rs` | IDs, names, branches and selections; not a second message store. |
| File recovery | `checkpoints.rs`, `file_lock.rs` | Private Git/index, per-store OS transaction lock and cancellation. Do not touch the user's Git index/HEAD. |
| Tool dispatch / process execution | `tools.rs` / `shell.rs`, `winsys.rs` | Tool results are authoritative; interceptors cannot turn failed post-processing into an execution failure. |

A Host replaces a model and its ContextConfig in one operation. A replacement
without a known ModelReport clears the old report; a known report can be supplied
with `replace_model_with_report`. Unknown identity/limits must not be fabricated.
Host tools and prompt text become the idle-reload baseline before plugin overlays,
so repeated reloads neither lose Host contributions nor duplicate plugins.

## TUI: notifications, queue, presentation, state

The TUI modules have distinct responsibilities:

- `tui/mod.rs`: terminal lifetime, input routing and driving the shared Agent.
- `tui/runtime_events.rs`: UiEvent and the synchronous EventSink adapter; convert
  runtime notifications to presentation messages without touching rendering/state.
- `tui/event_queue.rs`: bounded delivery, reliable title lane and aggregated
  accounting/status slots. It depends on event types, not the renderer.
- `tui/tool_display.rs`: pure display formatting of actual tool arguments/results,
  shared by live notifications and reloaded transcript presentation.
- `tui/view.rs`: UiState, applying received messages and drawing; rendering helpers
  live in the existing chrome/transcript/messages/tool-record submodules.
- `tui/title.rs`: separate bounded title requests and catalog-name updates; title
  usage must not overwrite the foreground request's context/cache measurements.

Formatting is presentation only: it cannot change the persisted tool result or
error bit. Queue overload may pause live presentation, but cannot truncate WAL
or cause commands to be replayed. See [lifecycle](lifecycle.md) for limits and
ordering. Moving notifications out of the renderer is an ownership separation,
not a new event bus, scheduler or public plugin ABI.

## Plugins: opt-in runtime versus worker lifetime

- `plugins.rs`: manifest/state/catalog, activation and reload composition, adapters
  for tools/hooks/events and worker process ownership.
- `plugins/approval.rs`: bounded filesystem source scans for idle worker reuse.
  It does not implement an approval workflow or own Host/Agent state.
- `plugins/model.rs`, `plugins/context.rs`, `plugins/stream.rs`: process-backed
  model/context adapters and streamed protocol assembly/validation.

A worker owns a content-scan value, not the scanning algorithm. Explicit plugin
enablement is one persistent switch; project source edits do not require
reapproval. Idle reload compares authored content, not just mtime, to decide
which workers can be reused. Plugin registration stages tool and Prompt changes
atomically. Broken optional plugins are isolated, while an explicitly selected
model/context backend cannot silently fall back to another provider. Event
subscribers have bounded queues and drop diagnostics. Dependency/cache trees
and runtime downloads are not a complete dependency graph or sandbox. See
[security](security.md) and [the plugin guide](../examples/plugins/README.md).

## Changes that do not belong in this cleanup

- Replacing the Agent loop, introducing a dependency-injection framework or
  splitting files without a coherent responsibility boundary.
- Changing compaction thresholds/retries, snapshot timing, tool concurrency or
  Shell timeout policy without a separately justified behavior change.
- Treating a notification as a durable commit, or reporting unknown execution
  outcomes as success/failure and replaying side effects.
- Using synthetic timings to promise end-to-end speedups or lower API bills.

Use [performance](performance.md) for opt-in local measurements. Architecture
cleanup must retain the existing public Rust APIs, JSONL protocol, WAL formats,
reasoning provenance, cache prefix behavior and per-session isolation.
