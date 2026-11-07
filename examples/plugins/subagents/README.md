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

Use it when side work would flood this conversation with search results, logs
or file contents, or when independent tasks can run in parallel. Do not use it
for a single lookup, or for work whose answer depends on this conversation's
history: a child receives only the task you write.

## The tool

One tool, `subagent`, supports both foreground legacy batches and a
**Session-as-Job** lifecycle. The host holds jobs only while its plugin worker
is alive; there is no remote queue or hidden scheduler. All jobs share a global
three-child concurrency cap. The original synchronous interface remains:

```json
{"action":"run","task":"Explore the repository and report file:line evidence","profile":"explore"}
{"action":"status","job_id":"returned-run-id"}
{"action":"wait","job_id":"returned-run-id","wait_ms":500}
{"action":"continue","resume":"returned-run-id/0","task":"Continue the same investigation"}
{"action":"cancel","job_id":"returned-run-id"}
```

`run` starts quickly without waiting for the child. `status` and `wait` return
one bounded snapshot; `wait` is at most 750 ms, so the same plugin host can
process `cancel` on the next call. `continue` forks the saved child session,
not the parent session. After worker restart, completed results remain readable
from evidence; an unfinished run is marked interrupted and must be manually
resumed. No claims of persistent background execution across host shutdown.

Legacy foreground batches accept up to eight independent tasks:

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
| `tasks[].resume` | Optional token from an earlier result; continues that child's saved conversation (see below) |
| `tasks[].profile` | `general` (default), `explore`, `planner`, `reviewer`, `verifier` or a custom profile |
| `tasks[].label` | Printable human label up to 60 characters; defaults to first line of task |
| `tasks[].complexity` | Optional `light`, `medium` or `heavy` for auto-model profiles; default medium |

Results always follow input order, even when tasks finish out of order. A started
task also returns a `resume` token.

## What a subtask receives

- Its own Kroma process (`kroma --json`), session file and working directory.
- Its own project instructions, Skills, tools and model configuration. With
  `KROMA_PLUGIN_ISOLATE_ENV=1` the host removes credential-shaped variables
  from the plugin worker, so children then depend on saved profiles rather than
  an exported API key.
- The task text only. **It does not inherit the parent conversation.** A
  resumed task also loads that child's own saved conversation, not yours.
- The child wraps the task as `Piped input:` and expands `@path` references
  against its working directory, so a short task can point at a large file.

Put the goal, relevant files and facts, allowed changes, constraints and the
evidence you expect directly in the task. Dependent work belongs in later
calls, after you have read the earlier results; pass `resume` when the later
step needs that child's saved context.

Nesting stops at one level: a subagent that tries to delegate again is refused.
The marker is `KROMA_SUBAGENT_DEPTH`, a convention between this plugin and its
children rather than a security boundary; a malformed value is refused instead
of being read as top level.

## Results

```json
{
  "schema_version": 1,
  "run_id": "20261106114357-797e06eb",
  "status": "partial",
  "results": [
    {"index": 0, "status": "completed", "exit_code": 0,
     "output": "Found one ordering defect ...", "output_truncated": false,
     "duration_ms": 18420, "tool_calls": 7,
     "usage": {"requests": 3, "input": 8100, "output": 900, "cached": 0, "reasoning": 120},
     "resume": "20261106114357-797e06eb/0",
     "artifacts": ".../runs/<run-id>/0"},
    {"index": 1, "status": "failed", "exit_code": null,
     "output": "The recovery path replays the journal ...", "output_truncated": false,
     "partial": true,
     "error": {"code": "CHILD_TIMEOUT",
               "message": "child timed out after 300s; partial answer kept in output"},
     "duration_ms": 300100, "tool_calls": 6,
     "usage": {"requests": 4, "input": 12000, "output": 1400, "cached": 0, "reasoning": 60},
     "resume": "20261106114357-797e06eb/1",
     "artifacts": ".../runs/<run-id>/1"}
  ]
}
```

- `completed`: the child exited 0 **and** returned a final result. That does not
  prove the work is correct; verify it yourself.
- `failed`: start failure, non-zero exit, signal, missing final result, or a
  protocol fault. `error.code` names the cause.
- `not_started`: the batch deadline stopped the queue before this task began.
  A fatal fault rejects the whole call instead; `batch.json` still records the
  tasks that never started.
- `partial`: only on a failed task. `output` is the most recent text the child
  had already streamed when it was terminated. It is unverified and is also
  written to `partial.txt`; the error message says the partial answer was kept,
  and the expanded tool card labels it `partial answer:`.
- `resume`: a token for continuing that child's saved conversation. It is
  present when the task has a task directory. See **Resuming a child**.
- `tool_calls`: how many tool calls the child started.
- `usage.requests` counts model requests and compaction requests the child
  reports. Missing provider counters stay `null`; they are never reported as zero. They
  are tracked per field, so a provider that omits `cached` or reasoning tokens
  still reports the input and output it did send.

The tool result is marked as an error when any task fails, but every successful
result is still returned. A protocol fault (non-JSON output, a duplicate or
textless final result, an oversized frame) is different: the batch is rejected
with one error naming the evidence directory, because partial results from a
corrupt worker are not trustworthy. The host then discards that worker. Inspect results and evidence before retrying, and
expect a retry to run again and incur its own model calls.

## Resuming a child

Pass a result's `resume` token as `tasks[].resume` to continue that child with
its saved conversation:

```json
{"tasks": [{"task": "Implement the fix you found, then run its test.",
            "resume": "20261106114357-797e06eb/0"}]}
```

- The saved snapshot and journal are copied into the new run. The source
  evidence is untouched, and both runs stay on disk.
- The child runs in the recorded working directory. A `cwd` that resolves
  elsewhere is refused; omit `cwd` and the recorded directory is used.
- A finished task (`completed`, `failed`, `paused`) resumes from its saved
  session. So does an **interrupted** task: a `running` record that no live
  child in this worker owns is what a host shutdown leaves behind, and resuming
  is the only way to recover that work. A task this worker is still executing is
  refused with `RESUME_ACTIVE`; a missing snapshot is `RESUME_MISSING`.
- A resume is a fork, not a retry: two resumes start from the same snapshot,
  and the child is billed again for the context it loads. Use a fresh task for
  independent work, and inspect a failure before resuming it.
- Tokens, working directories and session files are validated before any child
  starts. A malformed or escaping token rejects the batch.

## Parallelism and files

- `max_parallel` (default 3, range 1..8) limits concurrent children per batch.
- Analysis, review and search tasks can share a directory safely.
- Parallel edits need non-overlapping files or separate working directories;
  prepare worktrees with ordinary tools and pass each one as `cwd`.
- The plugin never creates branches, merges or resolves conflicts.

## Profiles and models (1.0)

The bundled profiles are `general` (unrestricted task), `explore` and `planner`
(read-only by instruction), `reviewer` (same) and `verifier` (run focused
tests). `explore`, `planner`, `reviewer` and `verifier` advertise only the
`shell` tool, so no file-editing tool exists for them. Custom Markdown
profiles are read from `~/.kroma/agents/*.md`, then
`<project>/.kroma/agents/*.md`, with the project definition taking precedence.
Frontmatter accepts only `name`, `description`, `model`, `thinking`, `shell`,
`tools`, `disallowedTools`, `maxTurns` and `maxTokens`; the rest is a profile
prompt. Invalid files reject the whole batch before starting children.

The manifest no longer hardcodes DeepSeek or `max` thinking. Profiles can use
`inherit`, `auto`, `light`, `medium`, or `heavy`; user settings
`model_light` / `model_medium` / `model_heavy` select optional model IDs for
the named tiers. Missing mappings reuse the model from the normal child
configuration; no upstream ID is guessed. Advanced setups may set
`model` and `thinking` in plugin settings to override this fallback.

`shell` accepts `allow` (default) or `deny`; `deny` removes the shell tool. The
child Kroma host enforces that tool allowlist/denylist plus model-turn and token
admission limits. It deliberately does **not** parse shell commands: a command
grammar is not an OS sandbox, and it rejects ordinary work such as
`sed -n '1,80p' file` or `rg -n 'a|b' src`. "Read only" in a profile is an
instruction to the model, not an enforced permission. The child never exposes
nested `subagent`, `memory_remember` or `memory_forget` tools, and its automatic
memory task-summary hook is disabled. A child can still execute arbitrary code
from the repository through a shell command it was told not to run.

Completed run evidence is cleaned after 30 days or 150 completed runs, without
deleting in-progress directories or a run explicitly being resumed. Results
and resume tokens remain ordered by the original tasks.
Every result is marked `trust: "untrusted_child_report"`. Obvious instructions
to override the parent's rules set `suspicious_instructions: true` for the
parent to inspect. The heuristic never silently deletes or edits child output.

Sessions are isolated; **the filesystem is not**. A task instruction such as
"do not edit files" is not a permission boundary.

## Live progress

While a batch runs, the card prints a roster row and then one row per running
task:

```text
▷ [1/3] List the failing tests
▷ [2/3] Read Cargo.toml
1▶ 2▶ · 5s · in 0 / out 0
[1/3] List the failing t… 0:05 ▶ model turn 2 · 5.2k tok · "checking the recovery test"
[2/3] Read Cargo.toml 0:05 ▶ shell 0:02 · 2.6k tok
✓ [2/3]   9.9s · in 5.3k / out 89 · "0.4.0"
1✓ 2✓ · 10s · in 10.5k / out 164
✓ 2/2 done · 12.4s · in 12.8k / out 171
1 "three failures" · 2 "0.4.0"
```

Instead of one row per child event, every five seconds the card prints a roster
row and then one row per running task: its task label, elapsed time, what it is
doing right now (model turn, or the tool it is in), how many tokens it has spent
and, after 90 seconds without an event, a `quiet` marker. While the child
streams assistant text, the row ends with a short preview of what it is
writing; the preview is shown only while that text is fresh. Task start and
finish still print once, and the finish row keeps the first characters of the
answer (or of the recovered partial answer).

The roster row is positional: `1✓ 2▶ 3·` means task 1 finished, task 2 is
running and task 3 is queued (`·` queued, `▶` running, `✓` completed, `✗` failed,
`–` never started), followed by elapsed time and the tokens spent so far.
When `KROMA_PLUGIN_TOOL_TIMEOUT_SECS` sets an explicit host deadline, the
progress line also shows it.

The host keeps the last eight rows of a running tool (`src/tui/tool_records.rs`).
After the call it renders the result JSON: a summary line and one line of
failures or answers, with per-task `──` sections when expanded. The evidence
directory keeps the full story. Progress is best effort: a failing listener
never changes the outcome, and the final result stays authoritative.

## Deadline and cancellation

There is **no automatic task deadline, batch deadline, model-turn limit or
token limit** by default. A child finishes when its task finishes or the user
cancels it. Profile `maxTurns` and `maxTokens` default to `0` (unlimited);
`task_timeout_secs = 0` also means unlimited. A user can explicitly set a
positive `task_timeout_secs`, per-profile budgets or
`KROMA_PLUGIN_TOOL_TIMEOUT_SECS`. A task that exceeds an explicitly configured
deadline is terminated with `CHILD_TIMEOUT` without aborting its siblings,
and the batch cuts off stragglers just before an explicit host deadline so
completed results are still returned. A task killed
by its own timeout or by a batch cut-off keeps the text it had streamed as
`output` with `partial: true`, and `partial.txt` is written for it. The
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

## Failure codes

| Code | Meaning |
| --- | --- |
| `INVALID_ARGUMENT` | A task, `cwd` or `resume` was rejected before any child started |
| `CONFIG_ERROR` | A plugin setting is invalid; no child starts |
| `DEPTH_LIMIT` | A subagent tried to delegate again; do the work directly |
| `ARCHIVE_ERROR` | Evidence cannot be written; fatal, the worker is discarded |
| `CHILD_START` | The `kroma` executable could not be started |
| `CHILD_EXIT` | The child exited non-zero without a more specific cause |
| `MODEL_NOT_CONFIGURED` | Child exit 3: no model, profile or model plugin |
| `AUTH_FAILED` | Child exit 4: the provider rejected the credentials; retrying will not help |
| `MODEL_REQUEST_FAILED` | Child exit 5: a model request failed; a retry may help |
| `CONTEXT_LIMIT` | Child exit 6: the context limit was reached |
| `CHILD_TIMEOUT` | The task or batch deadline expired |
| `CHILD_SIGNAL` | The child was terminated by a signal |
| `BATCH_ABORTED` | A sibling fault stopped this task |
| `CHILD_PROTOCOL` | Invalid child output; fatal, the worker is discarded |
| `RESUME_MISSING` | The token has no state record or no saved session |
| `RESUME_ACTIVE` | The source task is still running or claimed by another Kroma worker |
| `RESUME_STALE` | The saved session cannot be copied or its cwd is gone |

Fatal codes reject the whole call with one error instead of returning per-task
results. Ordinary task failures still return every successful sibling.

## Evidence

A task records `state.json` with its session path and working directory
**before** its child starts, so an interruption still leaves a locatable trace.
Each task writes under `<plugin-data>/runs/<run-id>/<index>/`:

| File | Content |
| --- | --- |
| `request.json` | Call id, parent archive, tasks, working directories, resume tokens |
| `batch.json` | Batch response summary |
| `state.json` | Status, session path, timing, resume source, tool count (final record written atomically) |
| `answer.txt` | Final answer, when the child produced one |
| `partial.txt` | Streamed text recovered from a failed task that produced no final result |
| `stderr.log` | Bounded child diagnostics, appended as they arrive |
| `events.ndjson` | Filtered lifecycle events and warnings, appended as they arrive; no text deltas |

The logs are written while the task runs, not at the end, so a host deadline or
Ctrl+C still leaves the diagnostics that explain what the child was doing.

The session file belongs to the child Kroma, which writes its own snapshot and
WAL. Evidence may contain repository content; treat it as untrusted data.
Completed runs are pruned after 30 days or after more than 150 completed run
archives; in-flight and explicitly resumed source runs are preserved.

## Limits

| Bound | Value |
| --- | ---: |
| Tasks per call | 8 |
| Task text | 16 KiB |
| Child JSON line | 2 MiB of UTF-8 (checked per line, before parsing) |
| Time per task | `task_timeout_secs` (default 0 = unlimited) |
| Tailored output per task | `max_output_chars` (default 12000) |
| In-memory streamed draft | 64 KiB tail per task (never in `events.ndjson`) |
| Tool response | 512 KiB |
| Saved stderr | 256 KiB |
| Saved events | 2 MiB |

`max_parallel`, `max_output_chars`, `task_timeout_secs`, `model` and `thinking` are validated; an invalid setting is an
error rather than a silent clamp. Unix and macOS run the same code; teardown
uses process groups there and `taskkill /T` on Windows.

## Tests

```bash
node --test examples/plugins/subagents/subagent.test.js \
            examples/plugins/subagents/main.test.js
```
