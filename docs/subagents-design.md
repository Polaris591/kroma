# Subagent plugin design

Status: implemented in `examples/plugins/subagents` (plugin `0.2.0`) with the
host-side tool deadline in `src/plugins.rs`.

## Goal

Delegate work the main agent can specify but not afford to explore in its own
context. The plugin is a reliable execution boundary, not a multi-agent
platform:

- the main agent decides goals, splitting and acceptance;
- each subtask runs as a real Kroma agent with its own session;
- the plugin performs execution, bounded concurrency, result classification and
  evidence capture;
- the host owns time and process-tree teardown.

Nothing else. No roles, teams, supervisor model, agent chat, automatic task
graphs, voting, judging, background job center or auto-merge.

## Shape

```text
main Kroma
  -> subagent(tasks)
       host: one whole-call deadline, cancellation, request correlation
       plugin: validation, queue, child processes, evidence
         -> kroma --json  (session A, cwd X)
         -> kroma --json  (session B, cwd Y)
```

Reusing Kroma itself is deliberate: the alternative (a host-side multi-agent
API) would duplicate the Agent Loop before there is evidence it is needed.
`PLUGIN_ROADMAP.md` P4 is updated to record this choice.

## Interface

`subagent({tasks: [{task, cwd?}]})`, 1..8 tasks, validated before any child
starts. `cwd` must be an existing directory; relative paths resolve against the
host working directory. There is no `mode`, `role`, `strategy` or per-task
timeout: dependent work is a second call, and context is written into the task.

## Lifecycle

- The child is spawned as `kroma --json`; the task is written to stdin and
  stdin is closed. No shell, no command-length limit, no task text in argv.
- `kroma --json` inlines piped stdin only up to 16 KiB and otherwise hands the
  model a file pointer, so a task is capped at 16 KiB and rejected before any
  child starts. The child also wraps stdin as `Piped input:` and expands
  `@path` references against its working directory.
- Environment: inherited, plus `KROMA_WORKDIR=<cwd>`,
  `KROMA_SESSION_FILE=<run>/<index>/session.json`, `KROMA_SUBAGENT_DEPTH=1`.
- Exactly one level of delegation; a nested attempt is refused.
- The worker processes lines serially, so two calls cannot interleave.

## Progress

The streaming tool card is the only live surface, and it should show which task
is doing what rather than replay every child event. Comparable harnesses render
a row per agent (name, elapsed, current step, tokens) and keep the transcript for
a drill-down, because an event log scrolls the useful state away; the host's
eight-row window makes that choice sharper here. So the batch prints a
positional roster (`1✓ 2▶ 3·`; `·` queued, `▶` running, `✓` completed, `✗`
failed, `–` never started) with batch elapsed and tokens, then one row per
running task: an 18-character label, task elapsed, the current activity (model
turn, or the tool it is in with that tool's elapsed), its tokens, and a `quiet`
marker once the child has been silent for 90 seconds. Task start and finish
still print once; the finish row carries a 40-character answer preview. Every
row is one of at most four per beat, so the eight-row window holds two complete
beats, and the last two rows a batch emits are the summary and a 160-character
line of per-task answers, because that is what the collapsed card keeps after
the call. The evidence directory is the full record. Tokens come from the
child's own `model_finished` frames, so they are as fresh as the last model
turn; the limit is the manifest deadline capped by
`KROMA_PLUGIN_TOOL_TIMEOUT_SECS`, so the card never promises more time than the
host will grant, and a test keeps the manifest and the module in sync.

## Time and cancellation

The manifest declares `timeout_secs = 600`; the host enforces it for streaming
and non-streaming tools alike, capped by `KROMA_PLUGIN_TOOL_TIMEOUT_SECS` when
smaller (`effective_tool_timeout`). The deadline covers queueing, spawning and
execution, and progress frames never extend it. On expiry the timeout error
names the tool and the evidence root, and the worker tree is terminated.

Inside the batch, a fatal fault terminates managed children politely first: on
Unix the child Kroma receives `SIGINT` so its own cancellation path runs (that is
what stops a Shell, which owns a separate process group), and `SIGKILL` follows
after a bounded grace period. Windows uses `taskkill /T /F`.

Each task is bounded by `task_timeout_secs` (default 300s) so one stuck child
fails with `CHILD_TIMEOUT` and frees its slot without spoiling the batch. The
batch also cuts off remaining stragglers just before the host deadline so
completed results are returned in a `partial` response instead of being lost
to a host worker kill. Nothing is resumed or retried automatically.
Side effects that already happened are not undone.

## Result contract

Top-level `{schema_version, run_id, status, results[]}` with `status` in
`completed | partial | failed`. Per task: `index`, `status`
(`completed | failed | not_started`), `exit_code`, clipped `output` with
`output_truncated` for successes, `error {code, message}` for failures,
`duration_ms`, `usage` and `artifacts`.

`usage` counters are tracked one by one: a provider that omits `cached` or
reasoning tokens leaves those fields `null` without erasing the input and output
it did report. `not_started` never reaches the tool response, because a fatal
fault rejects the call; it is written to `batch.json` so an interrupted batch
still records which tasks never began.

`completed` requires exit code 0 **and** exactly one final `result`. Zero exit
without a result, or more than one result, is a failure. The tool response is
`is_error` when any task failed, but successful results are preserved.

## Failure model

| Situation | Behaviour |
| --- | --- |
| Invalid task or `cwd` | Reject the batch before spawning |
| Child start failure | That task fails; others continue |
| Non-zero exit, signal | That task fails; others continue |
| Exit 0 without a result | Protocol failure for that task |
| Non-JSON stdout, duplicate/textless result, oversized frame | Fatal: abort the batch, reject with the evidence path, discard the worker |
| Evidence write failure | Fatal: the result would be untrustworthy |
| Host deadline or Ctrl+C | Host teardown; final results are not guaranteed |

Fatal errors surface as a stream `error` frame so the host discards the worker;
ordinary task failures use `complete` with `is_error` so the worker is reused.
The host verifies this by comparing worker PIDs: an ordinary `is_error`
completion keeps the same process, a fatal stream error replaces it.

A Shell started by a child Kroma owns its own process group, so a hard kill of
the child would orphan it; that is why `SIGINT` comes first. If the child ignores
it and the grace period expires, `SIGKILL` reaches the child Kroma only, and that
Shell group is left to the host deadline or Ctrl+C: the narrower case is
documented rather than promised.

## Evidence

A task writes `state.json` (session path, working directory, `running`)
before its child starts, so an interruption leaves a locatable trace without a
separate index. `stderr.log` and `events.ndjson` are appended as the child runs,
so a host kill still leaves the diagnostics that matter; `state.json` is only
rewritten at the end, with a temporary file and rename. `<plugin-data>/runs/<run-id>/<index>/`
also holds `answer.txt` (when the child produced an answer); the run directory
holds `request.json` and `batch.json`. A failed evidence write is fatal: the
result would be untrustworthy. Bounds: 16 KiB task, 2 MiB frame/events, 256 KiB
stderr, 512 KiB response.

## Security boundaries

- Session isolation is provided; filesystem isolation is not.
- `cwd` chooses a directory, not a permission sandbox.
- "Do not edit files" is an instruction, not enforcement.
- `KROMA_SUBAGENT_DEPTH` is a process convention between this plugin's parent and
  child, not a security boundary; a malformed value is refused rather than read
  as top level.
- Child Kromas inherit the user's privileges and environment. With
  `KROMA_PLUGIN_ISOLATE_ENV=1` the host strips credential-shaped variables from
  the plugin worker, so children must then rely on saved profiles.
- Model configuration is inherited from the environment and saved profiles;
  a temporary in-session model switch is not forwarded, and the plugin does not
  promise otherwise.
- Evidence may contain repository content and is untrusted data.

## Compatibility

`subagent_run` and `subagent_parallel` were transitional adapters over the same
executor. They are gone: the tool table exposes one `subagent` tool, so the
model has no near-duplicate entries to choose between.

## Acceptance

`examples/plugins/subagents/*.test.js` cover validation-before-spawn, ordered
results under bounded concurrency, independent sessions and `cwd`, failure
isolation, exit-0-without-result, protocol faults (non-JSON, a JSON line that is
not an object, duplicate result, textless result, an oversized UTF-8 line), the
outer `is_error` matching task status, a state record that exists before the
child starts, evidence appended while the task runs, `model_failed` kept and
text deltas excluded, a failed exit naming the last stderr line, the 16 KiB task
boundary, SIGINT-then-SIGKILL escalation, a malformed depth value, the
`not_started` record in `batch.json`, per-field usage, the progress deadline, and
character-boundary clipping. The suite clears its own plugin environment
variables, so it also passes inside a subagent.

Host tests cover the declared deadline for non-streaming tools, a streaming
tool whose heartbeats cannot extend the deadline, worker recovery after a
timeout, a descendant process of the worker being gone after the deadline, the
worker being replaced after a fatal stream error but reused after an ordinary
task failure (PID comparison), and the unchanged fatal-stream-error path:

```bash
node --test examples/plugins/subagents/subagent.test.js \
            examples/plugins/subagents/main.test.js
cargo test --test plugins_e2e
```

A manual smoke run against an unreachable local endpoint confirmed the real
spawn path: the child Kroma executed `--json`, reported failed request usage,
exited non-zero, and the plugin returned `CHILD_EXIT` with artifacts.

Not executed in this environment, and therefore not claimed:

- a live end-to-end subtask against a real model provider (no paid calls were
  made), so model profile inheritance is only reasoned about, not observed;
- the Unix process-group teardown path (verification ran on Windows only);
- cancellation observed from a live TUI while a child Kroma is running a
  Shell, beyond the descendant-teardown test above.
