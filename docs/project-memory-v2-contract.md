# Project Memory v2 contract (implementation)

Status: implemented for host and plugin (2026-11-06). Additive; V1 plugins unchanged.

## Manifest

```toml
api_version = 2
capabilities = ["provide_context", "task_finished"]
```

- Preferred generic capabilities: `provide_context`, `task_finished`.
- Legacy V2 `request_context` and `task_summary` remain supported without a migration step.
- Both require `command`.
- A V2 plugin may also declare tools/commands; V2 only changes the worker protocol version
  and enables the two host calls.
- V1 manifests must not declare capabilities.

## Worker protocol

Every request envelope keeps `protocol_version`, `request_id`, `config`, `data_dir`,
`workdir`; for V2 `protocol_version` is `2`. Responses echo the same `protocol_version`
and `request_id`, with `content` and `is_error` as today.

## Host -> plugin: provide_context (legacy: request_context)

Sent at most once per user task, before the first model request of that task, only to
enabled plugins declaring the capability.

```json
{
  "type": "provide_context",
  "name": "provide_context",
  "arguments": {
    "intent": "latest user request, bounded",
    "paths": ["optional relative hints"],
    "budget": {"max_tokens": 512, "max_bytes": 8192},
    "project": {"workspace_id": "opaque", "git_root": null, "workdir": "..."},
    "session": {"id": "...", "archive": "..."}
  }
}
```

Response `content` is a JSON string:

```json
{"blocks": [{"id": "opaque", "kind": "preference", "text": "bounded text", "paths": []}]}
```

Host rules:

- max 6 blocks, max 1024 tokens, per-block text <= 1200 chars; whole entries only.
- empty blocks => no injection, byte-identical request.
- timeout/error/malformed/oversized => skip this plugin, never fail the model request.
- inject as a low-trust `<plugin-context>` block appended to the last user
  message (projection only; WAL untouched). Each block carries the originating
  plugin ID, and text is escaped, not interpreted as instructions or markup.
- `conflict` and `last_activity` ephemeral notices can omit a durable ID;
  normal rule/review entries must include one.
- stable pack cache keyed by archive + boundary user-message digest; re-apply identical packs on
  later tool rounds and later turns until compaction/rewind makes the boundary mismatch.
- validate the final request; if the new pack breaks the budget, remove it and retry once.
- do not set `usage_matches_projection=false` merely because a host pack was added; V1 hook
  rewrites keep the existing conservative behavior.

## Host -> plugin: task_finished (legacy: task_summary)

Sent after the Agent finishes a task and the final WAL boundary has been
committed or attempted, **not** after an intermediate model message without
tool calls. Calls are non-blocking, bounded, best effort; a slow or crashed
plugin does not delay the next user operation. The host captures the session
ID and archive path before dispatch so switching sessions cannot retarget the
notification. Model-only consumers should call the explicit lifecycle hook.

```json
{
  "type": "task_finished",
  "name": "task_finished",
  "arguments": {
    "status": "completed",
    "recent_users": ["last up to 8 user messages, each <= 2000 chars, total <= 8192 bytes"],
    "project": {"workspace_id": "opaque", "git_root": null, "workdir": "..."},
    "session": {"id": "...", "archive": "..."}
  }
}
```

Response content: `{"ok": true}` or any JSON; host ignores content. Duplicate summaries are
allowed; the plugin must be idempotent.

## Plugin write semantics

- `memory_remember(kind, text, quote?, paths?, evidence?, supersedes?)`
  - `preference` / `decision`: require `quote` 8..500 chars. Store as `pending`, not searchable.
  - `review`: require >=1 path and >=1 evidence reference. Store active with a 30-day expiry;
    always labeled unverified in output.
  - Secret scan, size limits, path validation; no raw tool output.
- `task_finished`: for each legacy pending entry, normalize whitespace/case and promote only if its
  quote is a contiguous substring of one of `recent_users`. Apply `supersedes` on promotion.
  Drop pending entries older than 7 days or whose quote never matched. Idempotent.
- `memory_search` / `memory_read`: active, unexpired entries only.
- `memory_forget(id)`: immediate tombstone; future search/read/request_context exclude it.
- `memory_remember` and `memory_forget` declare `invalidates_context=true` so
  a same-turn follow-up model request re-fetches context after a mutation.
- `/memory off` stops new writes and returns empty recall; `/memory on` restores.
- Existing advanced commands may remain but are not required for normal use.

## Plugin identity and storage

- Git: resolve the common dir (handle `.git` file/worktree `commondir`), read or create
  `kroma-memory-id` there. All worktrees share it; moving the repo keeps it; clones do not.
- Non-Git: fall back to a user-level registry keyed by canonical workdir.
- Database: `<host data_dir>/projects/<id>/memory.sqlite3`; private permissions; no repo writes.
- Defaults: automation enabled; no approval queue for the tool path.

## Failure policy

Fail-open for recall and summary. Explicit remember/forget may return a structured error.
No network. No raw tool output capture. No automatic execution of memory text.
