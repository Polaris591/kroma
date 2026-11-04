# Security model

Kroma intentionally has **no permission-approval system**. It does not stop before a
shell command, file write, MCP call or network request to ask "allow?". The design
goal is an autonomous agent that stays inspectable and recoverable.

## What Kroma does

- Executes tools in the project working directory and shows calls and output in the
  transcript. In the TUI, the running command and status are visible while it runs.
- Never stores credentials in conversation history. Keys entered through setup are
  hidden while typing; a profile can instead reference an environment variable
  (`api_key_env`) so the secret is not written to `~/.kroma/config.json`.
- Keeps MCP server stderr out of Kroma's output by default, because third-party logs
  can be noisy or contain secrets. Set `KROMA_MCP_STDERR=inherit` when debugging.
- Requires explicit trust for a project's `.kroma/mcp.json`. Approval is bound to
  the file content; editing the file invalidates it. Use
  `kroma trust-project --revoke` to remove trust.
- Keeps diagnostics local. `kroma diagnostics` writes an allowlisted bundle and does
  not upload it, call a model, or start plugins.

## Automatic file snapshots

Before each task, Kroma creates a snapshot in a private bare Git repository under
`~/.kroma/checkpoints/<project-key>.git`. It uses its own `GIT_DIR`, work tree and
index, so it never reads or writes the project's `.git`, HEAD, index, hooks or
remotes. Common build/cache directories (`target`, `node_modules`, `dist`, `build`,
`.next`, `.venv`, `vendor`, `.kroma`) are excluded, as are paths ignored by the
project's `.gitignore`.

Snapshots are available only inside a real Git worktree. Outside Git, the feature is
a silent no-op and `/checkpoints` explains that. The snapshot contains file contents,
not history from your own repository. Set `KROMA_NO_CHECKPOINTS=1` to disable
automatic snapshots. On Unix the store directory is made owner-only,
but it can still contain secrets found in unignored worktree files; treat it as sensitive.
Each session keeps a bounded snapshot history (at most 200 commits; older batches
are pruned automatically).
Deleting the private store reclaims space but also removes all file snapshots.

List snapshots:

```text
/checkpoints
```

Restore a numbered snapshot:

```text
/restore-files 0
```

This is an explicit action. It rewrites files captured by the snapshot and removes
non-ignored files that were created after it, so the worktree returns to the
snapshot state. Built-in excluded directories and files ignored by `.gitignore`
are left alone. Because this can delete new work, run `/checkpoints` first and use
`/restore-files` deliberately. Conversation history is not changed; use `/rewind`
for messages.

## What is not undoable

Snapshots only cover files under the Git worktree. Kroma cannot undo:

- `git push`, remote branch changes, or changes made outside the worktree
- database writes, API calls, messages, deployments, or other network side effects
- files excluded by the project `.gitignore` or the built-in exclude list
- secrets printed into terminal scrollback or sent to a provider
- effects of a command that continues after Kroma is killed

Treat the agent as a capable process with your user permissions, not as a sandbox.
Use a container, VM, separate user or a disposable worktree for untrusted tasks.

## Cancellation

Ctrl+C cancels the active model request or shell process tree; it does not rewind
completed side effects. When idle, Ctrl+C clears an unsent draft and keeps its
attachments recoverable with Ctrl+Z. Ctrl+Q exits. A cancelled task records a durable
boundary in the session.

## Network and providers

Kroma sends the conversation, tool definitions and relevant file contents to the
configured model endpoint. It contacts that endpoint and the providers you configure
for MCP. It does not run a telemetry service. Provider retention and training policies
are outside Kroma's control; read your provider's terms.
