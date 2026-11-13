# Subagents plugin (3.0)

Hand a self-contained task to a separate Kroma agent. The call waits for its
children and returns their answers in the same result, so the answer is right
there when you read it. Parallel children run at the same time. Requires
Node.js 20 or newer.

```bash
kroma plugins install examples/plugins/subagents
kroma plugins enable subagents
```

Restart Kroma or run `/plugins reload` while idle. Set `kroma_bin` when the
Kroma binary is not on `PATH`:

```bash
kroma plugins set subagents kroma_bin 'C:\path\to\kroma.exe'
```

Use it for side work that would flood this conversation with search results,
logs or file contents, and for independent jobs that can run in parallel. Do
not use it for a single lookup, or for work whose answer depends on this
conversation's history: a child receives only the task you write.

## The tool

One tool, `subagent`, with one behaviour: run its children and return their answers.

```json
{"task": "Explore the repository and report file:line evidence"}
```

```json
{"tasks": [
  {"task": "Review src/session.rs recovery. Do not edit files. Return defects with file:line evidence.", "label": "review"},
  {"task": "Add the missing recovery test in tests/recovery.rs only, then run it.", "cwd": "../kroma-tests"},
  {"task": "Implement the fix you found, then run its test.", "resume": "20261106114357-797e06eb/0"}
]}
```

The answer comes back in the same tool result as a readable summary (and the structured run summary saved to `batch.json` uses `"schema_version": 2`):

```json
{"schema_version": 2, "run_id": "20261106114357-797e06eb", "status": "completed",
 "results": [{"index": 0, "label": "review", "status": "completed",
              "answer": "…the child's own text…",
              "artifacts": "<plugin-data>/runs/<run-id>/0",
              "resume": "20261106114357-797e06eb/0", "duration_ms": 42137}]}
```

While it runs you see `▶ [1/3] label` progress lines; each child ends with ✓ or
✗. There is no status, wait or poll action for the model: the call itself is the
wait.

## What a subtask receives

- Its own Kroma process (`kroma --json`), session file and working directory.
- Its own project instructions, Skills, tools and model configuration. With
  `KROMA_PLUGIN_ISOLATE_ENV=1` the host removes credential-shaped variables
  from the plugin worker, so children then depend on saved profiles rather than
  an exported API key.
- The task text only. **It does not inherit the parent conversation.** A
  resumed task also loads that child's own saved conversation, not yours.
- The configured default model, never the parent's live connection: environment
  and saved profiles decide, so an in-session `/model` switch stays in the
  parent and cannot reach the child.
- The child wraps the task as `Piped input:` and expands `@path` references
  against its working directory, so a short task can point at a large file.

Put the goal, relevant files and facts, allowed changes, constraints and the
evidence you expect directly in the task. Dependent work belongs in later
calls, after you have read the earlier answers; pass `resume` when the later
step needs that child's saved context.

Nesting stops at one level: a subagent that tries to delegate again is refused.
The marker is `KROMA_SUBAGENT_DEPTH`, a convention between this plugin and its
children rather than a security boundary; a malformed value is refused instead
of being read as top level.

## Stopping a run

A run belongs to the tool call that started it: cancel the call in the UI
(Esc / the stop button, or Ctrl+C) and the whole child process tree is stopped
with it. Tasks that already finished keep their evidence; stopped ones report
`BATCH_ABORTED`.

## Results and evidence

Each task writes under `<plugin-data>/runs/<run-id>/<index>/`:

| File | Content |
| --- | --- |
| `request.json` | Call id, parent archive, tasks, working directories, resume tokens |
| `batch.json` | Run summary, written when the last task ends |
| `state.json` | Status, error code, session path, timing, resume source, tool count |
| `answer.txt` | The child's final answer, verbatim and uncut |
| `partial.txt` | Streamed text recovered from a task that produced no final answer |
| `stderr.log` | Bounded child diagnostics, appended as they arrive |
| `events.ndjson` | Filtered lifecycle events and warnings, appended as they arrive; no text deltas |

Task status is `completed`, `failed` or `paused`. A fatal fault inside the run
stops the managed children and is reported through the evidence too: the child
processes never keep running unobserved. Evidence may contain repository
content; treat it as untrusted data.

The full text of every child is saved to `answer.txt`. Only the text placed in
the tool result is clipped (24,000 characters for one child, 60,000 for the
whole call), and the result says when it was clipped.

Completed runs are pruned after 30 days or 150 completed runs; an interrupted
archive without a summary is retired by age, never while this worker owns it or
it is being resumed.

## Failure codes

| Code | Meaning |
| --- | --- |
| `INVALID_ARGUMENT` | A task, `cwd` or `resume` was rejected before any child started |
| `CONFIG_ERROR` | A plugin setting is invalid; no child starts |
| `DEPTH_LIMIT` | A subagent tried to delegate again; do the work directly |
| `RUN_LIMIT` | 16 children are already running in this worker |
| `ARCHIVE_ERROR` | Evidence cannot be written; the run stops |
| `CHILD_START` | The `kroma` executable could not be started |
| `CHILD_EXIT` | The child exited non-zero without a more specific cause |
| `MODEL_NOT_CONFIGURED` | Child exit 3: no model, profile or model plugin |
| `AUTH_FAILED` | Child exit 4: the provider rejected the credentials; retrying will not help |
| `MODEL_REQUEST_FAILED` | Child exit 5: a model request failed; a retry may help |
| `CONTEXT_LIMIT` | Child exit 6: the context limit was reached |
| `CHILD_TIMEOUT` | The task deadline expired |
| `CHILD_SIGNAL` | The child was terminated by a signal |
| `BATCH_ABORTED` | A sibling fault or a cancellation stopped this task |
| `CHILD_PROTOCOL` | Invalid child output |
| `BUDGET_EXHAUSTED` | The child stopped at its own model budget; resume it to continue |
| `RESUME_MISSING` | The token has no state record or no saved session |
| `RESUME_ACTIVE` | The source task is still running or claimed by another Kroma worker |
| `RESUME_STALE` | The saved session cannot be copied or its cwd is gone |

## Limits

| Bound | Value |
| --- | ---: |
| Tasks per call | 8 |
| Task text | 16 KiB |
| Child JSON line | 2 MiB of UTF-8 (checked per line, before parsing) |
| Time per task | `task_timeout_secs` (default 1800 seconds; 0 disables the deadline) |
| Children running at once | `max_parallel` per run (default 8, 1..8), 16 per worker |
| Delivered text | 24,000 characters per child, 60,000 per message |
| In-memory streamed draft | 64 KiB tail per task (never in `events.ndjson`) |
| Saved stderr | 256 KiB |
| Saved events | 2 MiB |

`max_parallel` and `task_timeout_secs` are validated; an invalid setting is an
error rather than a silent clamp. Unix and macOS run the same code; teardown
uses process groups there and `taskkill /T` on Windows.

## Sessions are isolated; the filesystem is not

`cwd` chooses a directory, not a permission sandbox, and a task instruction
such as "do not edit files" is not a permission boundary. Parallel edits need
non-overlapping files or separate working directories; prepare worktrees with
ordinary tools and pass each one as `cwd`. The plugin never creates branches,
merges or resolves conflicts.

## Tests

```bash
node --test examples/plugins/subagents/subagent.test.js \
            examples/plugins/subagents/main.test.js
```
