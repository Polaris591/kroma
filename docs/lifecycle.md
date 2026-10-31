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
| Model request | A provider request is streaming. Steering may be accepted. | User input and prior tool results |
| Tool batch | Returned tool calls run strictly in order. | Each prior tool result is written before the next call |
| Finish/checkpoint | A text-only response reached `finish_task`; no background work runs. | Assistant response is already in the WAL |
| Steering drain | Accepted steering is appended as user messages and another model turn starts. | Steering messages are durable before the request |
| Cancelling | The host dropped the running future and called `cancel`. | Synthetic unknown tool results and the cancel note are written |
| Foreground idle | `TaskFinished` was emitted. The host may start the next user task. | Completed/failed/cancelled foreground boundary is recorded where possible |

`TaskFinished` is a foreground boundary. Lazy compaction is synchronous inside
`prepare` only when the projected input reaches the configured checkpoint
threshold; there is no speculative background summary or delayed summary usage.

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
| `CompactionUsage` | Finalized checkpoint usage was taken at a task boundary. | No background work remains. |
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
- `/plugins reload` only runs while the TUI is idle. It prepares tools,
  commands and events first and commits them only on success. Model/context
  identity changes still require a restart rather than a hot swap.

## Failure and cancellation checks

- A failed model request attempts to persist one interruption note; any
  partially streamed assistant text is discarded.
- `cancel` synthesizes missing tool results for previous calls, preserves their
  call IDs, appends the cancel note, then emits `TaskFinished { cancelled }`.
- Cancellation aborts pending context/plugin lifecycle work. It does not undo
  filesystem side effects.
- An ordinary task failure clears unconsumed external-context lifecycle
  notifications. The built-in context policy runs checkpoints synchronously and
  has no background maintenance to preserve or cancel.
- There is no automatic replay of interrupted commands.
- Shell risk boundary: commands have no default timeout and no output quota.
  A command that blocks indefinitely runs until the user cancels it; continuous
  output can consume disk because complete captures are retained. On Unix,
  descendants that deliberately leave the shell process group can survive
  cancellation. These are explicit tradeoffs, not missing background cleanup.
