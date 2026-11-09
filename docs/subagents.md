# Kroma 子代理（Subagent）设计

> 本文合并了原先两份文档（0.3.0 执行内核的历史设计参考、1.0 归档提案契约），原文件已删除，正文内容逐字保留。
> 已实现的默认值与当前行为以 [subagents README](../examples/plugins/subagents/README.md) 为准。

## 〇、现状（3.0，2026-11）：同步返回，工具调用就是等待

- 工具调用等待全部子任务结束，再把每个子任务的答案放进**同一个**工具结果返回；并行子任务同时跑。
- 运行期间通过流式进度行（`▶ [1/3] 标签`，结束为 ✓/✗）告知用户，不需要轮询。
- 结果是子代理自己的输出（原文），标为不可信数据，附带标签、状态、用时、证据目录和 resume 令牌；主模型不需要再调用工具去取结果。
- 已删除：投递式后台作业（宿主作业表、自动唤醒、"最多两轮"限制、`KROMA_NO_SUBAGENT_DELIVERY`）、面向模型的轮询动作、`/subagents` 命令与 Alt+X 快捷键、五个内置角色与模型分级、`complexity`、输出预算与二分压缩、"可疑指令"启发式。
- 保留：独立会话与工作目录、有序证据目录、失败隔离、任务超时与分级终止、resume、证据保留期。
- 停止：取消这次工具调用（Esc / 停止按钮 / Ctrl+C），插件进程树连同子代理一起结束。
- 下文两份历史文档中凡描述"后台投递""用户命令取消"的部分，均已被上述契约取代。

## 一、设计参考：Subagent plugin design

Status: **historical design reference**. For implemented defaults and current
behavior use [subagents README](../examples/plugins/subagents/README.md).
The early fixed-timeout and foreground-only assumptions below are superseded.

### Goal

Delegate work the main agent can specify but not afford to explore in its own
context. The plugin is a reliable execution boundary, not a multi-agent
platform:

- the main agent decides goals, splitting and acceptance;
- each subtask runs as a real Kroma agent with its own session;
- the plugin performs execution, bounded concurrency, result classification and
  evidence capture;
- the host owns time and process-tree teardown.

A finished child can be resumed from its saved session. That is one explicit
continuation with copied evidence, not an agent chat, shared live state or a
task graph.

Nothing else. No roles, teams, supervisor model, agent chat, automatic task
graphs, voting, judging, background job center or auto-merge.

### Shape

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

### Interface

`subagent({tasks: [{task, cwd?, resume?}]})`, 1..8 tasks, validated before any
child starts. `cwd` must be an existing directory; relative paths resolve
against the host working directory. `resume` is a `<run-id>/<index>` token from
an earlier result. There is no `mode`, `role`, `strategy` or per-task timeout:
dependent work is a second call, with context written into the task or with the
child's own saved conversation loaded by `resume`.

### Lifecycle

- The child is spawned as `kroma --json`; the task is written to stdin and
  stdin is closed. No shell, no command-length limit, no task text in argv.
- `kroma --json` inlines piped stdin only up to 16 KiB and otherwise hands the
  model a file pointer, so a task is capped at 16 KiB and rejected before any
  child starts. The child also wraps stdin as `Piped input:` and expands
  `@path` references against its working directory.
- Environment: inherited, plus `KROMA_WORKDIR=<cwd>`,
  `KROMA_SESSION_FILE=<run>/<index>/session.json`, `KROMA_SUBAGENT_DEPTH=1`.
- A resume token copies the source `session.json` and its journal into the new
  task directory before the child starts. The source run stays immutable, the
  child owns a fresh lock, and only a final `completed` or `failed` state is
  resumable; a `running` record may still own its session.
- Exactly one level of delegation; a nested attempt is refused.
- The worker processes lines serially, so two calls cannot interleave.

### Progress

The streaming tool card is the only live surface, and it should show which task
is doing what rather than replay every child event. Comparable harnesses render
a row per agent (name, elapsed, current step, tokens) and keep the transcript for
a drill-down, because an event log scrolls the useful state away; the host's
eight-row window makes that choice sharper here. So the batch prints a
positional roster (`1✓ 2▶ 3·`; `·` queued, `▶` running, `✓` completed, `✗`
failed, `–` never started) with batch elapsed and tokens, then one row per
running task: an 18-character label, task elapsed, the current activity (model
turn, or the tool it is in with that tool's elapsed), its tokens, a short
preview of fresh streamed text, and a `quiet` marker once the child has been
silent for 90 seconds. Task start and finish still print once; the finish row
carries a 40-character answer or partial-answer preview and names the recovered
partial length. Every
row is one of at most four per beat, so the eight-row window holds two complete
beats, and the last two rows a batch emits are the summary and a 160-character
line of per-task answers. After the call the card renders the same summary from
the result JSON, with per-task sections when expanded. The evidence directory
is the full record. Tokens come from the
child's own `model_finished` frames, so they are as fresh as the last model
turn; the limit is the manifest deadline capped by
`KROMA_PLUGIN_TOOL_TIMEOUT_SECS`, so the card never promises more time than the
host will grant, and a test keeps the manifest and the module in sync.

### Time and cancellation

Current implementation: plugin and task execution have **no fixed default
deadline**. An explicit positive `task_timeout_secs` or
`KROMA_PLUGIN_TOOL_TIMEOUT_SECS` bounds execution; no timeout is invented for
unlimited interactive work. An explicit deadline still covers queueing,
spawning and execution and results in teardown when exceeded.

Inside the batch, a fatal fault terminates managed children politely first: on
Unix the child Kroma receives `SIGINT` so its own cancellation path runs (that is
what stops a Shell, which owns a separate process group), and `SIGKILL` follows
after a bounded grace period. Windows uses `taskkill /T /F`.

An explicitly configured task timeout can fail a stuck child with
`CHILD_TIMEOUT` and free its slot without spoiling the batch. When a host
deadline is configured, the batch reserves cleanup time before it. A killed
task keeps a bounded tail of the text it had
streamed as `output` with `partial: true`, so the work is not lost with the
process. Nothing is retried automatically; resuming a finished child is an
explicit later call with its `resume` token. Side effects that already happened
are not undone.

### Result contract

Top-level `{schema_version, run_id, status, results[]}` with `status` in
`completed | partial | failed`. Per task: `index`, `status`
(`completed | failed | not_started`), `exit_code`, clipped `output` with
`output_truncated`, `partial` when output was recovered from streamed text,
`error {code, message}` for failures, `duration_ms`, `tool_calls`, `usage`,
`resume` (when the task has a directory) and `artifacts`.

`usage` counters are tracked one by one: a provider that omits `cached` or
reasoning tokens leaves those fields `null` without erasing the input and output
it did report. `not_started` reaches the response when the batch deadline stops
the queue; a fatal fault rejects the call instead, and `batch.json` still
records which tasks never began.

`completed` requires exit code 0 **and** exactly one final `result`. Zero exit
without a result, or more than one result, is a failure. The tool response is
`is_error` when any task failed, but successful results are preserved.

### Failure model

| Situation | Behaviour |
| --- | --- |
| Invalid task, `cwd` or `resume` | Reject the batch before spawning |
| Child start failure | That task fails; others continue |
| Non-zero exit, signal | That task fails; others continue; exit 3/4/5/6 becomes `MODEL_NOT_CONFIGURED` / `AUTH_FAILED` / `MODEL_REQUEST_FAILED` / `CONTEXT_LIMIT` |
| Exit 0 without a result | Protocol failure for that task |
| Timeout or batch cut-off with streamed text | Task fails with `partial: true` and keeps a bounded `output` |
| Resume of a task this worker is still running, or a missing session | Reject before spawning (`RESUME_ACTIVE` / `RESUME_MISSING`) |
| Resume of an interrupted task (`running` record no live child owns) | Allowed: fork the saved session into a new run |
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

### Evidence

A task writes `state.json` (session path, working directory, `running`)
before its child starts, so an interruption leaves a locatable trace without a
separate index. `stderr.log` and `events.ndjson` are appended as the child runs,
so a host kill still leaves the diagnostics that matter; `state.json` is only
rewritten at the end, with a temporary file and rename, and records
`resumed_from`, `tool_calls`, `text_chars` and `partial_chars`.
`<plugin-data>/runs/<run-id>/<index>/` also holds `answer.txt` (when the child
produced a final answer) and `partial.txt` (when a failed task was killed after
streaming text); the run directory holds `request.json` and `batch.json`. A
failed evidence write is fatal: the result would be untrustworthy. Bounds:
16 KiB task, 2 MiB frame/events, 256 KiB stderr, 512 KiB response, 64 KiB
in-memory draft per task.

### Security boundaries

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
- `resume` tokens are resolved only inside this plugin's `runs` directory; a
token that escapes it, references an unfinished task or has no saved session is
refused before any child starts. A copied session is still the child's private
conversation, not a permission boundary.
- Evidence may contain repository content and is untrusted data.

### Compatibility

`subagent_run` and `subagent_parallel` were transitional adapters over the same
executor. They are gone: the tool table exposes one `subagent` tool, so the
model has no near-duplicate entries to choose between. Version 0.3.0 adds the
optional `resume` input and the `resume`, `partial` and `tool_calls` result
fields; existing task shapes and result fields are unchanged.

### Acceptance

`examples/plugins/subagents/*.test.js` cover validation-before-spawn, ordered
results under bounded concurrency, independent sessions and `cwd`, failure
isolation, exit-0-without-result, protocol faults (non-JSON, a JSON line that is
not an object, duplicate result, textless result, an oversized UTF-8 line), the
outer `is_error` matching task status, a state record that exists before the
child starts, evidence appended while the task runs, `model_failed` kept and
text deltas excluded, a failed exit naming the last stderr line, the 16 KiB task
boundary, SIGINT-then-SIGKILL escalation, a malformed depth value, the
`not_started` record in `batch.json`, per-field usage, compaction request
counts, warning evidence, the progress deadline, live text previews, fair
response budgeting, streamed partial answers, exit-code classification,
tool-call counts, resume copying and validation (malformed, missing, active and
`cwd`-mismatched tokens), and character-boundary clipping. The TUI test renders
the `partial answer:` section of a failed task. The suite clears its own plugin
environment variables, so it also passes inside a subagent.

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


## 二、1.0 契约：Subagents 1.0 design

Status: **archived proposal**, retained as design history, not a specification
of current behavior. See [current subagent plugin](../examples/plugins/subagents/README.md).

This document specifies the `1.0` release only. It is deliberately about
**capability boundaries**, not about turning Kroma into a multi-agent platform.
Background jobs, a task center, worktree isolation, parent-session forks and
remote A2A agents are deferred to later releases.

### 1. Why 1.0 looks like this

The market has converged on a small set of useful ideas:

- isolated context by default, with an explicit way to continue a child;
- reusable profiles with their own prompt, tools, model and limits;
- foreground and background execution, with a task list;
- enforced permissions, not prompt-only instructions;
- resumable partial work instead of silent failure.

Kroma `0.3.0` already has the execution kernel: one subagent tool, isolated
Kroma child processes, bounded concurrency, ordered results, resume, partial
output, evidence, deadlines and error classification. What it lacks is the
product contract around that kernel: profiles, enforced tool access, model
routing, turn budgets and output trust handling.

`1.0` adds those. It keeps the existing architecture:

```text
main Kroma
  -> subagent(tasks)
       plugin: profiles, queue, model routing, evidence, result contract
         -> kroma --json (profile + tool filter + shell policy + budget)
```

No host-side multi-agent coordinator is introduced.

### 2. Goals and non-goals

#### Goals

1. Let the main agent choose a **profile**, not hand-write a new role prompt for
   every delegation.
2. Give profiles a tool allowlist and declare intent in their prompt; do not
   parse shell commands in the host, because a command grammar is not a sandbox.
3. Route profiles to different model tiers without exposing raw model IDs to the
   model.
4. Bound each child with turn and token budgets; a limit produces a resumable
   partial result, not an opaque failure.
5. Mark every child report as untrusted and flag instruction-shaped output.
6. Keep existing task shapes, result fields, resume tokens and evidence layout
   working.

#### Non-goals for 1.0

- Background tasks, notifications or a task center.
- Worktree or filesystem isolation.
- Forking the parent conversation into a child.
- Live steering, agent-to-agent messaging or agent teams.
- Remote A2A subagents.
- Automatic retry, automatic merge, voting or judging.
- Nested delegation beyond the existing one-level rule.

### 3. User-facing interface

The tool remains `subagent({tasks: [...]})`, 1..8 tasks.

```json
{
  "tasks": [
    {
      "profile": "explore",
      "label": "map auth",
      "task": "Goal: ...\nContext: ...\nAllowed actions: read-only\nDeliverable: ...\nEvidence: file:line ..."
    },
    {
      "profile": "general",
      "label": "fix auth",
      "task": "...",
      "cwd": "."
    },
    {
      "profile": "verifier",
      "label": "verify auth",
      "task": "Check the diff against these acceptance criteria ...",
      "resume": "20261106114357-abc12345/0"
    }
  ]
}
```

#### Task fields

| Field | Required | Meaning |
| --- | --- | --- |
| `task` | yes | Self-contained brief, at most 16 KiB. Goal, context, allowed actions, deliverable and evidence should be explicit. |
| `cwd` | no | Existing directory; defaults to the request workdir. Still not a permission sandbox. |
| `resume` | no | `<run-id>/<index>` token from a finished task. Continues the child's saved conversation. |
| `profile` | no | Profile name. Defaults to `general`. Must match the original profile when `resume` is used. |
| `label` | no | Human label for progress and results, 1..60 characters, no control characters. Defaults to the first task line. |
| `complexity` | no | `light`, `medium` or `heavy`. Only affects profiles whose model is `auto`. Defaults to `medium`. |

`1.0` deliberately has no `background`, `isolation`, `model`, `timeout` or
`maxTurns` task field. Profile and plugin settings own those decisions.

#### Validation

The whole batch is validated before any child starts:

- unknown profile, malformed profile file, duplicate profile name in the same
  scope or unsupported profile field rejects the batch;
- `resume` plus a different explicit profile rejects with `RESUME_PROFILE`;
- `resume` still requires a finished task and a saved session;
- `label` is normalized and bounded;
- `complexity` must be one of the three values;
- task text remains limited to 16 KiB.

### 4. Profiles

#### 4.1 Locations and precedence

| Scope | Path | Notes |
| --- | --- | --- |
| Built-in | `<plugin-dir>/profiles/*.md` | Shipped with the plugin; trusted. |
| User | `~/.kroma/agents/*.md` | Per-user; trusted. |
| Project | `<request-workdir>/.kroma/agents/*.md` | Same trust level as project `AGENTS.md`; content is reviewed by the user. |

Precedence is project > user > built-in. If two files in the same scope declare
the same `name`, the profile is invalid and the batch is rejected; do not depend
on filesystem order.

Only the top level of each directory is scanned in 1.0. Profiles are loaded at
the start of a batch, so editing a profile affects the next batch, not a running
child.

Project profiles cannot add hooks, MCP servers, environment variables or
permissions the parent does not already have. They only select a prompt, a tool
filter, a shell switch, a model tier and numeric limits.

#### 4.2 File format

Markdown with a strict YAML subset in frontmatter. The parser supports only
scalar values, inline arrays and comments; no nested maps or multiline values.
This avoids adding a YAML dependency to the plain-Node plugin.

```markdown
---
name: reviewer
description: Read-only review of a diff. Use after code changes.
model: heavy
thinking: high
shell: allow
tools: [shell]
maxTurns: 20
maxTokens: 60000
---
You are a skeptical reviewer. Report findings with file:line evidence,
severity, and what you could not verify. Do not edit files.
```

#### 4.3 Frontmatter fields

| Field | Default | Rules |
| --- | --- | --- |
| `name` | required | `^[a-z0-9][a-z0-9_-]{0,63}$`; no colon. |
| `description` | required | At most 240 characters. Written for the main model; say when to use it. |
| `model` | `inherit` | `auto`, `light`, `medium`, `heavy` or `inherit`. No raw model IDs in profiles. |
| `thinking` | inherit | Uses the host's existing thinking-tier environment; empty means inherit. Provider-specific extensions are out of scope. |
| `shell` | `allow` | `allow` or `deny` (removes the shell tool). "Read only" is prompt guidance, not enforcement. |
| `tools` | inherit all | Inline list of host tool names. `[]` means no tools. Omitted means inherit all. |
| `disallowedTools` | none | Inline list removed after `tools`. Deny wins. |
| `maxTurns` | plugin default | Integer 1..100. One turn is one model request. |
| `maxTokens` | 0 | Integer 0..1,000,000. `0` means no per-task token cap. Counts input + cached + output tokens when reported. |
| profile body | required | System-prompt addition, 1..32 KiB. |

Unknown fields are errors. Unknown tool names in `tools` are reported by the
host as a startup warning and simply removed; an allowlist that resolves to no
tools fails the child with a clear `NO_TOOLS` startup error. Unknown names in
`disallowedTools` are ignored with a warning.

#### 4.4 Built-in profiles

| Profile | Model | Shell | Tools | Default turns | Purpose |
| --- | --- | --- | --- | --- | --- |
| `explore` | `light` | `allow` | `[shell]` | 30 | Find files, trace code, answer codebase questions. |
| `planner` | `medium` | `allow` | `[shell]` | 30 | Produce an implementation plan without edits. |
| `reviewer` | `heavy` | `allow` | `[shell]` | 20 | Review a diff for correctness, security and missing tests. |
| `verifier` | `heavy` | `allow` | `[shell]` | 30 | Run tests and verify behavior against acceptance criteria. |
| `general` | `auto` | `allow` | inherit all | 40 | Implementation and other full-tool work. |

`general` is the default when `profile` is omitted, so old task shapes keep
working. Its prompt must stay short and must not duplicate the plugin's base
delegation guidance.

Kroma's core only exposes `shell` plus plugin/MCP tools, so `[shell]` is the normal
read-only tool set: no file-editing tool is advertised. Preventing a write the
model decides to run through the shell is the profile prompt's job, not the
host's; only `shell: deny` removes the shell tool itself.
