# Subagents 1.0 design

Status: proposed, builds on plugin `0.3.0`.

This document specifies the `1.0` release only. It is deliberately about
**capability boundaries**, not about turning Kroma into a multi-agent platform.
Background jobs, a task center, worktree isolation, parent-session forks and
remote A2A agents are deferred to later releases.

## 1. Why 1.0 looks like this

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

## 2. Goals and non-goals

### Goals

1. Let the main agent choose a **profile**, not hand-write a new role prompt for
   every delegation.
2. Make read-only and test-running profiles **actually enforced** by the host.
3. Route profiles to different model tiers without exposing raw model IDs to the
   model.
4. Bound each child with turn and token budgets; a limit produces a resumable
   partial result, not an opaque failure.
5. Mark every child report as untrusted and flag instruction-shaped output.
6. Keep existing task shapes, result fields, resume tokens and evidence layout
   working.

### Non-goals for 1.0

- Background tasks, notifications or a task center.
- Worktree or filesystem isolation.
- Forking the parent conversation into a child.
- Live steering, agent-to-agent messaging or agent teams.
- Remote A2A subagents.
- Automatic retry, automatic merge, voting or judging.
- Nested delegation beyond the existing one-level rule.

## 3. User-facing interface

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

### Task fields

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

### Validation

The whole batch is validated before any child starts:

- unknown profile, malformed profile file, duplicate profile name in the same
  scope or unsupported profile field rejects the batch;
- `resume` plus a different explicit profile rejects with `RESUME_PROFILE`;
- `resume` still requires a finished task and a saved session;
- `label` is normalized and bounded;
- `complexity` must be one of the three values;
- task text remains limited to 16 KiB.

## 4. Profiles

### 4.1 Locations and precedence

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
filter, a shell policy, a model tier and numeric limits.

### 4.2 File format

Markdown with a strict YAML subset in frontmatter. The parser supports only
scalar values, inline arrays and comments; no nested maps or multiline values.
This avoids adding a YAML dependency to the plain-Node plugin.

```markdown
---
name: reviewer
description: Read-only review of a diff. Use after code changes.
model: heavy
thinking: high
shell: read-only
tools: [shell]
maxTurns: 20
maxTokens: 60000
---
You are a skeptical reviewer. Report findings with file:line evidence,
severity, and what you could not verify. Do not edit files.
```

### 4.3 Frontmatter fields

| Field | Default | Rules |
| --- | --- | --- |
| `name` | required | `^[a-z0-9][a-z0-9_-]{0,63}$`; no colon. |
| `description` | required | At most 240 characters. Written for the main model; say when to use it. |
| `model` | `inherit` | `auto`, `light`, `medium`, `heavy` or `inherit`. No raw model IDs in profiles. |
| `thinking` | inherit | Uses the host's existing thinking-tier environment; empty means inherit. Provider-specific extensions are out of scope. |
| `shell` | `inherit` | `read-only`, `test`, `inherit` or `deny`. See section 5. |
| `tools` | inherit all | Inline list of host tool names. `[]` means no tools. Omitted means inherit all. |
| `disallowedTools` | none | Inline list removed after `tools`. Deny wins. |
| `maxTurns` | plugin default | Integer 1..100. One turn is one model request. |
| `maxTokens` | 0 | Integer 0..1,000,000. `0` means no per-task token cap. Counts input + cached + output tokens when reported. |
| profile body | required | System-prompt addition, 1..32 KiB. |

Unknown fields are errors. Unknown tool names in `tools` are reported by the
host as a startup warning and simply removed; an allowlist that resolves to no
tools fails the child with a clear `NO_TOOLS` startup error. Unknown names in
`disallowedTools` are ignored with a warning.

### 4.4 Built-in profiles

| Profile | Model | Shell | Tools | Default turns | Purpose |
| --- | --- | --- | --- | --- | --- |
| `explore` | `light` | `read-only` | `[shell]` | 30 | Find files, trace code, answer codebase questions. |
| `planner` | `medium` | `read-only` | `[shell]` | 30 | Produce an implementation plan without edits. |
| `reviewer` | `heavy` | `read-only` | `[shell]` | 20 | Review a diff for correctness, security and missing tests. |
| `verifier` | `heavy` | `test` | `[shell]` | 30 | Run tests and verify behavior against acceptance criteria. |
| `general` | `auto` | `inherit` | inherit all | 40 | Implementation and other full-tool work. |

`general` is the default when `profile` is omitted, so old task shapes keep
working. Its prompt must stay short and must not duplicate the plugin's base
delegation guidance.

Kroma's core only exposes `shell` plus plugin/MCP tools. Therefore `[shell]` is
the normal read-only tool set; enforcement comes from the sh
