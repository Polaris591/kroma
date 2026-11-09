# Goal plugin

One goal, one host-owned controller, one normal Agent loop. The plugin supplies
Goal rules and the `goal_checkpoint` tool; the host keeps execution, persistence
and cancellation. Installing or enabling it never changes an existing setting by
itself.

Status: implemented and covered by the host end-to-end suite
(`tests/task_control_e2e.rs`, 22 goal cases) plus the plugin's own contract
tests. What is still pending is release validation — a full closed loop on a real
provider and a manual deployment rehearsal — so treat the
[design document](../../../docs/goal-plugin-design.md) as the contract and the
source plus tests as the authority.

```bash
kroma plugins install examples/plugins/goal
kroma plugins enable goal
```

Restart Kroma or run `/plugins reload` while idle.

```text
/goal 修复登录失败，补上回归测试，并通过相关检查
/goal                       # current goal, progress and pause reason
/goal pause                 # pause; completed edits are kept
/goal resume                # continue, re-checking the workspace as needed
/goal resume 先只支持邮箱登录  # add a user revision, then continue
/goal cancel                # give up; completed work is not deleted
```

A new `/goal <task>` starts one real turn immediately; it is refused while the
current goal is still running. **No acceptance form is required up front**: work
starts at once, and `goal_checkpoint` only records progress, a completion claim
or a blocker. The goal is not the foreground task: a normal answer, a streamed
draft or an assistant message never means it is complete.

## Recording progress

`goal_checkpoint` needs only `summary` and `claim` (`working` / `done` /
`blocked`). Everything else is optional:

- `criteria` — requirements you want tracked; omit to inherit, `[]` to clear.
  The model may refine or correct its own list at any time.
- `checks` — commands worth running as evidence; omit to inherit, `[]` to clear.
  A check may be registered **after** it ran, and it stays valid evidence for the
  rest of the goal. Running other tools only makes the *completion claim* stale,
  so no rerun is forced.
- `evidence` — tool call ids; the host verifies each id is a real execution of
  this goal and revision, otherwise the checkpoint is rejected as a tool error.
- `unverified` — what a person still has to confirm.

Explicitly stopping a task (`Ctrl+C`, the stop button, `/goal pause`) pauses the
goal. `/goal cancel` ends it. `/goal resume` reopens a paused *or* finished goal
because the user explicitly asked for it; nothing reopens on its own.

## What the host guarantees

- The accepted final answer and the `completed` record are one message append.
- An interrupted `running` record is persisted as `paused` before any new work;
  nothing is replayed.
- A fork or rewind keeps the requirement and its check plan as a *new paused*
  goal: new id, no pass facts, no completion claim. A finished goal is never
  revived, and `/clear` drops the control state.
- Host metadata coming from a model backend is stripped before it is stored, so
  a provider cannot forge a goal record.
- Every model request carries the complete active goal — id, revision, status,
  the user's own words for each revision, the check plan, real execution facts
  and recorded progress. It is a projection, never a durable user instruction.
- User requirements live as exact message references, so a long conversation
  never blocks or truncates a new goal.
- While a turn runs, `/goal pause` and `/goal cancel` stop it through the same
  user-stop path on every host. The plugin is never asked to approve a stop.
- A plugin fault or provider error pauses the goal instead of reporting success.

## Host support

| Host | Start, status, pause, resume, cancel |
| --- | --- |
| CLI, JSONL | Yes (`/goal pause` and `/goal cancel` also stop a running turn) |
| TUI | Yes (`/goal` idle; `pause`/`cancel` while running) |
| Desktop | Yes; the goal shelf is restored from the durable snapshot and survives reloads |

## Settings

There is no request, time or token budget and no refusal counter: the host never
pauses a goal because it "tried too often". A goal ends when the model delivers a
checked result, when it reports a real blocker, or when the user stops it.

## Limits

Command checks compare the exact command line and working directory; the host
does not interpret shell syntax. A passing check is evidence that a command ran,
not proof that the requirement was understood correctly, and no completion
percentage is shown. Nothing here claims exactly-once external side effects.
