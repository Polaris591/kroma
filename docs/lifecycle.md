# Kroma foreground lifecycle contract

This note describes the existing Rust Agent loop. It does not add another
scheduler, event bus, or set of Pi-style lifecycle names.

## States

Kroma has one foreground Agent task at a time. A task is started by
`Agent::run` / `run_with_attachments`, and the loop remains inside that call
until it returns, fails, or the host drops the future and calls
`Agent::cancel`.

| State | Meaning | Durable work already done |
| --- | --- | --- |
| Idle | No `TaskInput` is active. Session/plugin commands may run. | Previous WAL entries and session snapshots |
| File snapshot | Before task input reaches the model; a cancellable blocking worker owns the private-store transaction. | No new model/tool work yet |
| Model request | A provider request is streaming. Steering may be accepted. | User input and prior tool results |
| Tool batch | Returned tool calls run strictly in order. | Each prior tool result is written before the next call |
| Finish/checkpoint | A text-only response reached `finish_task`; foreground compaction is settled. | Assistant response is already in the WAL |
| Steering drain | Accepted steering is appended as user messages and another model turn starts. | Steering messages are durable before the request |
| Cancelling | The host dropped the running future and called `cancel`. | Synthetic unknown tool results and the cancel note are written |
| Foreground idle | `TaskFinished` was emitted. The host may start the next user task. | Completed/failed/cancelled foreground boundary is recorded where possible |

`TaskFinished` is a foreground boundary. Lazy compaction is synchronous inside
`prepare` only when the projected input reaches the configured checkpoint
threshold; there is no speculative background summary or delayed summary usage.
This is not a process-wide idle guarantee: host-owned title requests and
bounded plugin event subscriptions may still be pending after `TaskFinished`.

## Event ordering

The optional `EventSink` is synchronous, read-only notification. It is not a
journal; `SessionStore` owns durable messages and tool results.

| Event | Trigger | Durability / follow-on work |
| --- | --- | --- |
| `TaskStarted` | `TaskInput` was acquired for a run. | No model request yet. |
| `ContextPrepared` | The exact request projection passed the budget check. | May still start a model request. |
| `ModelStarted` | Immediately before the provider call. | No assistant message is durable yet. |
| `TextDelta` / `ReasoningDelta` | Streaming output only. | Transient; never written as conversation content. |
| `ModelFinished` | Assistant response appended to WAL/snapshot. | May start a tool batch or another turn. Usage is recorded before this event. |
| `ModelFailed` | Failed turn's interruption note was attempted in the WAL. | No retry; the host receives `TaskFinished { status: "failed" }`. |
| `ToolStarted` | Before one tool call executes. | Prior tool results are durable; this call is not. |
| `ToolOutput` | Live stdout/stderr preview. | UI only; never in transcript/WAL. |
| `ToolFinished` | Tool result appended to WAL/snapshot. | May start the next tool or model turn. |
| `CompactionUsage` | Finalized checkpoint usage was taken at a task boundary. | No pending built-in compaction; independent title/event workers are outside this accounting. |
| `Warning` | Nonfatal snapshot or after-tool-hook failure. | Snapshot warnings may precede the user WAL entry; hook warnings follow the durable tool result, whose real execution status is unchanged. |
| `TaskFinished` | Foreground task result, after usage/stats settlement. | Host may accept the next operation. |
| `SessionCreated` / `SessionSwitched` / `SessionForked` / `SessionRewound` / `SessionCleared` | After the new session/state is durable and model/event identity was synchronized. | No old session identity is reused. |

Completion notifications are never sent before their corresponding durable
append. Tool and model failures are not replayed into the next task.

## Steering and follow-ups

- Steering is accepted only while a task is active and is delivered after the
  current assistant turn and all of its tool results.
- The `ActiveTask` guard clears unconsumed steering when the future is dropped
  on failure or cancellation.
- Follow-up tasks are separate user turns. They stay queued; the CLI pauses
  the queue after a failure/cancellation and the host decides when to resume.
- A task that finishes normally seals the input inbox before returning, so a
  late steering message is rejected rather than silently dropped.

## Session, model and plugin ownership

- Session events are emitted only after the catalog/store switch and
  `sync_session_context` have updated the model backend and event sink.
- Changing a model requires a matching `ContextConfig` and clears the previous
  model report; hosts attach the new report after both are swapped.
- Plugin process workers are owned by the plugin catalog. Event subscriptions
  get isolated workers per reload, so dropping an old sink cancels only its
  stalled handler, not a newly registered one.
- Host tool/prompt customizations form the reload base before plugin overlays;
  reloading does not bake an overlay into its own base.
- `/plugins reload` only runs while the TUI is idle. It prepares tools,
  commands and events first and commits them only on success. Model/context
  identity changes still require a restart rather than a hot swap.

## Failure and cancellation checks

- A failed model request attempts to persist one interruption note; any
  partially streamed assistant text is discarded. Early failures after the
  user turn was appended (for example context-budget or attachment errors)
  also attempt one durable failure boundary.
- `cancel` synthesizes missing tool results for previous calls, preserves their
  call IDs, appends the cancel note, then emits `TaskFinished { cancelled }`.
  Calling it after a task future has already returned is a no-op; it never
  creates a second cancellation record.
- Cancelling a run also flags its snapshot worker. That worker terminates Git
  and cleans up under the private-store lock before releasing it; a subsequent
  snapshot transaction waits for that lock. Dropping the foreground future is
  not itself a synchronous join of the blocking worker.
- Cancellation aborts pending context/plugin lifecycle work. It does not undo
  filesystem side effects. A cancelled `/compact` settles its checkpoint usage
  before the command is reported as cancelled.
- An ordinary task failure clears unconsumed external-context lifecycle
  notifications. The built-in context policy runs checkpoints synchronously and
  has no background maintenance to preserve or cancel.
- There is no automatic replay of interrupted commands.
- Shell risk boundary: commands have no default timeout and no output quota.
  A command that blocks indefinitely runs until the user cancels it, unless
  `KROMA_SHELL_TIMEOUT_SECS` supplies an optional positive wall-clock deadline
  for unattended automation. Continuous output can consume disk because
  complete captures are retained. On Unix, descendants that deliberately leave
  the shell process group can survive cancellation. On Windows, cancellation
  terminates the containing job; a successfully completed command may leave
  background descendants running until Kroma exits. These are explicit
  tradeoffs, not missing background cleanup.

## TUI overload policy

The TUI's synchronous event sink never waits for consumer capacity. Pending
presentation is limited to 2,048 events; retained presentation and Git-status
payloads share a 2 MiB budget (8 KiB reserved for the latest Git status).
Foreground/compaction usage and lifecycle/context updates occupy a fixed
number of additional aggregate slots, not one allocation per model request.
If the presentation limit is reached, the UI warns and pauses further live
transcript updates for that turn. The task keeps running, accounting and final
idle state are retained, and the complete authoritative transcript reloads at
turn end. This is not a Shell output quota or a change to WAL durability.

Automatic titles use a separate 32-event reliable lane and at most 16 pending
title tasks. A title task acquires capacity before making a model request;
excess scheduling is skipped without incurring another request. An already
started title request waits asynchronously for delivery, preserving its
originating session ID even while foreground presentation is overloaded.

Module ownership is documented in [architecture](architecture.md).
