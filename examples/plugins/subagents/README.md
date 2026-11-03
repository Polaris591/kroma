# Subagents plugin

Delegate self-contained tasks to separate Kroma agents.

```bash
kroma plugins install examples/plugins/subagents
kroma plugins enable subagents
```

Restart Kroma or run `/plugins reload` while idle. Set `kroma_bin` when the
Kroma binary is not on `PATH`:

```bash
kroma plugins set subagents kroma_bin 'C:\path\to\kroma.exe'
```

## The tool

One tool, `subagent`, takes up to eight independent tasks:

```json
{"tasks": [
  {"task": "Review src/session.rs recovery. Do not edit files. Return defects with file:line evidence."},
  {"task": "Add the missing recovery test in tests/recovery.rs only, then run it.", "cwd": "../kroma-tests"}
]}
```

| Field | Rule |
| --- | --- |
| `tasks` | 1..8 entries, validated before any child starts |
| `tasks[].task` | Nonempty UTF-8 string, at most 16 KiB (the child inlines piped stdin only up to that size) |
| `tasks[].cwd` | Existing directory; defaults to the host working directory |

Results always follow input order, even when tasks finish out of order.

## What a subtask receives

- Its own Kroma process (`kroma --json`), session file and working directory.
- Its own project instructions, Skills, tools and model configuration. With
  `KROMA_PLUGIN_ISOLATE_ENV=1` the host removes credential-shaped variables
  from the plugin worker, so children then depend on saved profiles rather than
  an exported API key.
- The task text only. **It does not inherit the parent conversation.**
- The child wraps the task as `Piped input:` and expands `@path` references
  against its working directory, so a short task can point at a large file.

Put the goal, relevant files and facts, allowed changes, constraints and the
evidence you expect directly in the task. Dependent work belongs in later
calls, after you have read the earlier results.

Nesting stops at one level: a subagent that tries to delegate again is refused.
The marker is `KROMA_SUBAGENT_DEPTH`, a convention between this plugin and its
children rather than a security boundary; a malformed value is refused instead
of being read as top level.

## Results

```json
{
  "schema_version": 1,
  "run_id": "20261103T114357-797e06eb",
  "status": "partial",
  "results": [
    {"index": 0, "status": "completed", "exit_code": 0, "output": "Found one ordering defect ...",
     "output_truncated": false, "duration_ms": 18420,
     "usage": {"requests": 3, "input": 8100, "output": 900, "cached": 0, "reasoning": 120},
     "artifacts": ".../runs/<run-id>/0"},
    {"index": 1, "status": "failed", "exit_code": 1,
     "error": {"code": "CHILD_EXIT", "message": "child exited with code 1: ..."},
     "usage": {"requests": 1, "input": null, "output": null, "cached": null, "reasoning": null},
     "artifacts": ".../runs/<run-id>/1"}
  ]
}
```

- `completed`: the child exited 0 **and** returned a final result. That does not
  prove the work is correct; verify it yourself.
- `failed`: start failure, non-zero exit, signal, missing final result, or a
  protocol fault. `error.code` names the cause.
- `not_started`: the batch was aborted before this task began. A fatal fault
  rejects the tool call, so this status appears only in the `batch.json`
  evidence, never in the response.
- Missing provider counters stay `null`; they are never reported as zero. They
  are tracked per field, so a provider that omits `cached` or reasoning tokens
  still reports the input and output it did send.

The tool result is marked as an error when any task fails, but every successful
result is still returned. A protocol fault (non-JSON output, a duplicate or
textless final result, an oversized frame) is different: the batch is rejected
with one error naming the evidence directory, because partial results from a
corrupt worker are not trustworthy. The host then discards that worker. Inspect results and evidence before retrying, and
expect a retry to run again and incur its own model calls.

## Parallelism and files

- `max_parallel` (default 3, range 1..8) limits concurrent children per batch.
- Analysis, review and search tasks can share a directory safely.
- Parallel edits need non-overlapping files or separate working directories;
  prepare worktrees with ordinary tools and pass each one as `cwd`.
- The plugin never creates branches, merges or resolves conflicts.

Sessions are isolated; **the filesystem is not**. A task instruction such as
"do not edit files" is not a permission boundary.

## Live progress

While a batch runs, the tool card streams one line per event, each on its own
row:

```text
▷ [1/3] started
0/3 done · 3 running · 5s/600s · in 0 / out 0
✓ [2/3]  12.4s · in 5.9k / out 190
✓ 3/3 done · 31.7s · in 25.0k / out 649
```

The heartbeat repeats every five seconds with finished/running counts, elapsed
time against the deadline the host will enforce (the manifest value, capped by
`KROMA_PLUGIN_TOOL_TIMEOUT_SECS`), and tokens spent so far. Progress is best
effort: a failing listener never changes the outcome, and the final result
stays authoritative.

## Deadline and cancellation

The manifest declares `timeout_secs = 600`. The host owns that deadline: it
covers waiting, spawning and running, and progress frames never extend it. The
host timeout message names the tool call and the evidence root, so the run can
be located without guessing.
Ctrl+C or deadline expiry terminates the plugin worker tree, including the
child Kromas. A fatal fault inside the batch stops the managed children itself:
on Unix the child Kroma gets `SIGINT` first so its own cancellation path stops
the Shell it started, and `SIGKILL` follows after a bounded grace period. If the
child ignores `SIGINT`, the hard kill reaches the child Kroma only, and a Shell
that already escaped to its own process group then needs the host deadline or
manual cleanup. Workers are not resumed automatically: after an interruption,
check the evidence directory before deciding what to run again.

## Evidence

A task records `state.json` with its session path and working directory
**before** its child starts, so an interruption still leaves a locatable trace.
Each task writes under `<plugin-data>/runs/<run-id>/<index>/`:

| File | Content |
| --- | --- |
| `request.json` | Call id, parent archive, tasks, working directories |
| `batch.json` | Batch response summary |
| `state.json` | Status, session path, timing (final record written atomically) |
| `answer.txt` | Final answer, when the child produced one |
| `stderr.log` | Bounded child diagnostics, appended as they arrive |
| `events.ndjson` | Filtered lifecycle events, appended as they arrive; no text deltas |

The logs are written while the task runs, not at the end, so a host deadline or
Ctrl+C still leaves the diagnostics that explain what the child was doing.

The session file belongs to the child Kroma, which writes its own snapshot and
WAL. Evidence may contain repository content; treat it as untrusted data.

## Limits

| Bound | Value |
| --- | ---: |
| Tasks per call | 8 |
| Task text | 16 KiB |
| Child JSON line | 2 MiB of UTF-8 (checked per line, before parsing) |
| Tailored output per task | `max_output_chars` (default 12000) |
| Tool response | 512 KiB |
| Saved stderr | 256 KiB |
| Saved events | 2 MiB |

`max_parallel` and `max_output_chars` are validated; an invalid setting is an
error rather than a silent clamp. Unix and macOS run the same code; teardown
uses process groups there and `taskkill /T` on Windows.

## Tests

```bash
node --test examples/plugins/subagents/subagent.test.js \
            examples/plugins/subagents/main.test.js
```
