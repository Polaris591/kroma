# Command-line reference

## Top-level

| Command | Meaning |
| --- | --- |
| `kroma` | Interactive TUI, new conversation |
| `kroma [TASK]` | One task in the line CLI |
| `kroma --print TASK` | Stream one answer, fresh conversation |
| `kroma --json TASK` | JSONL events; implies `--print` |
| `kroma setup` | Guided profile setup and optional connection test |
| `kroma config ...` | Manage saved profiles |
| `kroma plugins ...` | Manage process plugins |
| `kroma diagnostics ...` | Write a local diagnostics bundle |
| `kroma update [--check] [--channel CHANNEL]` | Check for, or apply an npm update (`CHANNEL` is `latest` or `next`) |
| `kroma trust-project [--revoke]` | Approve/revoke this project's MCP file |
| `kroma recover-context [id]` | Reset unavailable custom context memory |
| `kroma help [COMMAND]` | Help; unknown topics print the main usage |
| `kroma version` / `--version` / `--build-info` | Version information |

## Run options

| Option | Meaning |
| --- | --- |
| `-p`, `--print` | One-shot plain-text output |
| `--json` | JSONL events; implies `--print` |
| `-c`, `--continue` | Resume the most recent conversation |
| `--session <name>` | Use or create a named conversation |
| `--new` | Force a fresh conversation |
| `--no-mcp` | Do not start or expose MCP tools |
| `--mcp <names>` | Restrict MCP to comma-separated server names |
| `-q`, `--quiet` | Suppress optional runtime diagnostics |
| `-v`, `--verbose` | Print extra runtime diagnostics |
| `--cwd <path>` | Project directory |
| `--config-file <path>` | Alternate profile JSON |
| `--language <code>` | TUI interface language (en or zh) |
| `--no-color` | Disable colour |
| `--no-tui`, `--cli` | Use the line CLI for interactive input |
| `-h`, `--help` | Help |
| `-V`, `--version` | Version |
| `--build-info` | Version and executable path |

Rules:

- The first token `update`, `config`, `plugins`, `setup`, `diagnostics`,
  `trust-project`, `recover-context`, `help`, `version` is a subcommand.
- Recognised options may appear before or after the prompt: `kroma hello --json`.
- Unknown options are a usage error (exit 2); they are never sent to the model.
- `--` makes everything after it prompt text, including leading dashes.
- `--new` conflicts with `--continue`; neither combines with `--session`.
- `--json` implies `--print`.
- Global options (`--cwd`, `--config-file`, `--language`, `--no-color`, `--quiet`,
  `--verbose`, `--no-mcp`, `--mcp`) may appear before a subcommand or after a prompt;
  `--` still makes everything after it literal.

## `kroma config`

```text
kroma config [list|show|set <name>|use <name>|delete <name>|test <name>]
```

`list` shows the active profile, host, protocol, limits and key source. `set` prompts
for endpoint, model, key and optional metadata. `test` sends one small real request.

## `kroma plugins`

```text
kroma plugins [list|enable <id>|disable <id>|install <path>|check <path>|
               uninstall <id>|config <id>|set <id> <key> <value>|reset <id> <key>]
```

Installed plugins are disabled until enabled. Restart or `/plugins reload` after
enabling.

## `kroma diagnostics`

```text
kroma diagnostics [--output <new-directory>]
```

Writes a local, allowlisted bundle. No upload, model request or plugin process.

## Slash commands (TUI and line CLI)

| Command | Meaning |
| --- | --- |
| `/help` | Shortcuts and help |
| `/commands` | Search all commands |
| `/new` | Welcome page / new conversation |
| `/clear` | Clear current conversation |
| `/sessions`, `/resume <id>` | Saved conversations |
| `/continue` | Resume the latest |
| `/compact` | Compact context now |
| `/history [count]` | Conversation checkpoints |
| `/fork <n>`, `/rewind <n>`, `/undo` | Conversation branching/rollback |
| `/checkpoints` | File snapshots (Git worktrees) |
| `/restore-files <n>` | Restore files from a snapshot |
| `/diff` | Working-tree changes |
| `/export <path>` | Export conversation |
| `/skills` | Discovered Agent Skills |
| `/plugins`, `/plugin` | Plugin status/reload |
| `/steer <text>`, `/queue <task>` | Live task input |
| `/pending` | Pending tasks and steering |
| `/exit`, `/quit` | Exit |

`/rewind` changes messages only. `/restore-files` changes files only and is explicit.

## Exit codes

| Code | Meaning |
| --- | --- |
| 0 | Success |
| 1 | Other runtime failure |
| 2 | Command-line usage error |
| 3 | Model not configured |
| 4 | Provider authentication/authorization failure |
| 5 | Model request failure |
| 6 | Context limit |

## Updates

`kroma update --check [--channel latest|next]` explicitly checks the official npm registry.
`kroma update [--channel latest|next]` upgrades an unambiguous npm installation via npm.
Global installs remain global; local installs update their owning project and may change
its package.json/lockfile. Other managers and linked installs require manual upgrades.
Native Cargo/source binaries print instructions instead of replacing themselves.
There are no startup/background checks. Close other Kroma sessions before upgrading.
