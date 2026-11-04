# Kroma

A minimal, observable Rust agent harness with a terminal UI, a line CLI, JSONL
events, MCP tools, Agent Skills and recoverable file checkpoints.

Kroma is deliberately autonomous: it does not ask for per-command permission. It
keeps the agent's work visible, snapshots files automatically in Git worktrees, and
makes rollback explicit. Read [docs/security.md](docs/security.md) before running it
on a repository you care about.

## Quick start

Source installation (the npm packages are not published yet):

```bash
cargo build --release
./target/release/kroma
```

With no model configured, the first launch opens a guided setup. Pick a provider,
pick or type a model ID, enter the key without echo, optionally test the connection,
then save. Re-run it later with `kroma setup`.

Non-interactive:

```bash
export KROMA_MODEL=deepseek-chat
export KROMA_BASE_URL=https://api.deepseek.com/v1
export KROMA_API_KEY=...
kroma --print "summarize this repository"
```

See [docs/quickstart.md](docs/quickstart.md) for the full first-run flow.

## Why Kroma

- **One agent loop, two hosts.** The TUI and JSONL/line CLI use the same runtime,
  tools, sessions and compaction. No second implementation to drift.
- **Scriptable one-shot runs.** `--print` and `--json` use a fresh conversation by
  default and can run in parallel. Continue deliberately with `--continue` or
  `--session`.
- **No silent file changes.** In a Git worktree, Kroma snapshots files before each
  task into `~/.kroma/checkpoints/` using a private `GIT_DIR` and index. `/checkpoints`
  lists snapshots; `/restore-files <n>` restores one explicitly, including removing files created after it. Your `.git`, index,
  HEAD and hooks are untouched.
- **Lazy MCP.** Servers are not started at launch, and their schemas do not consume
  context until the model calls `mcp_search`/`mcp_call`. Third-party logs are hidden
  by default.
- **Context discipline.** Lazy single-pass compaction, prompt caching, exact token
  accounting and a stable tool prefix.
- **Honest cancellation.** Ctrl+C stops the active model request or shell process
  tree; completed side effects are not claimed as undone. Ctrl+Z restores a cleared
  draft and its attachments.

## Commands

```text
kroma                         Interactive TUI, new conversation
kroma [TASK]                  One task in the line CLI
kroma --print TASK            Fresh one-shot conversation
kroma --json TASK             JSONL events; implies --print
kroma --continue              Resume the latest conversation
kroma --session NAME TASK     Use or create a named conversation
kroma setup                   Guided model setup
kroma config ...              Manage saved profiles
kroma plugins ...             Manage plugins
kroma diagnostics ...         Local support bundle
kroma trust-project           Approve this project's MCP file
kroma help [COMMAND]          Help
```

Unknown options are usage errors, never prompt text. `--` makes the rest literal.
Full reference: [docs/reference/cli.md](docs/reference/cli.md).

## MCP and skills

MCP tool discovery is lazy:

- `mcp_search` starts configured servers on demand and lists matching tools.
- `mcp_call` invokes a discovered tool.

Set `KROMA_MCP_EAGER=1` to expose every remote tool directly, `--no-mcp` to skip MCP,
and `KROMA_MCP_STDERR=inherit` to debug server logs. Project MCP files require
`kroma trust-project`.

Agent Skills are discovered from `SKILL.md` files and loaded only when relevant.
See [docs/reference/configuration.md](docs/reference/configuration.md).

## Safety

No approval prompts. Visibility, automatic snapshots and explicit rollback instead of
a permission system. Snapshots cover files in a Git worktree only; they cannot undo
pushes, database writes, network effects or changes outside the worktree. Details and
limits: [docs/security.md](docs/security.md).

## npm distribution

A precompiled npm distribution is being prepared; no Rust toolchain will be needed
on end-user machines. The package names are provisional and are not published yet.
See [the distribution guide](docs/npm-distribution.md) for local packaging, isolated
installation checks and the five-platform CI build matrix. Built-in update commands
and automatic npm publishing are not implemented in this stage.

## Development

```bash
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
```

Requirements: Rust 1.85+ (edition 2021), Git for file checkpoints, and an API key for
a supported provider. Platform-specific behavior and the long design history are in
[docs/design-notes.md](docs/design-notes.md); lifecycle details are in
[docs/lifecycle.md](docs/lifecycle.md).

## License

MIT. See [LICENSE](LICENSE).
