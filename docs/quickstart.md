# Quickstart

## 1. Install and start

```bash
cargo build --release --locked
./target/release/kroma
```

On first launch, if no model is configured, Kroma opens the guided setup instead of
exiting. Choose a provider, choose or type a model ID, enter the key (hidden), and
optionally run a tiny connection test. The profile is saved only after you confirm.

You can rerun setup at any time:

```bash
kroma setup
```

## 2. Configure without the wizard

Profiles live in `~/.kroma/config.json` (or `KROMA_CONFIG_FILE`). A profile can store
a key, or reference an environment variable:

```bash
export KROMA_MODEL=deepseek-chat
export KROMA_BASE_URL=https://api.deepseek.com/v1
export KROMA_API_KEY=...
kroma
```

`kroma config set work` is the line-oriented profile editor. `kroma config list`
shows profiles and whether a key is stored, supplied by an environment variable,
or missing. `kroma config test work` makes one small real request.

## 3. One task, one conversation

`--print` and `--json` use a fresh conversation by default, so two scripts can run
in parallel without sharing a session or its lock:

```bash
kroma --print "summarize this repository"
kroma --print "run the tests" --json
```

Continue deliberately:

```bash
kroma --print --continue "and fix the failure"
kroma --print --session release-check "verify the release notes"
```

`--session` names a separate conversation; `--new` forces a fresh one. Without
`--continue` or `--session`, nothing is reused.

## 4. Interactive use

```bash
kroma                 # new TUI conversation
kroma --continue      # resume the latest saved conversation
kroma --session work  # resume or create a named conversation
kroma --no-tui        # line-oriented interactive mode
```

Useful slash commands:

- `/help` — shortcuts and navigation
- `/sessions`, `/resume`, `/new` — conversation management
- `/history`, `/rewind`, `/undo` — conversation checkpoints (messages only)
- `/compact [instructions]` — manually summarize old context; optional focus applies only to this checkpoint (e.g. `/compact preserve pending tests and exact file paths`)
- `/checkpoints`, `/restore-files <n>` — file snapshots (Git worktrees only)
- `/diff` — working-tree changes
- `/exit`, Ctrl+Q or Ctrl+D on an empty draft — quit

Kroma has **one classic TUI layout**: gray transcript, teal assistant rail,
violet user rail, readable Markdown, collapsed reasoning, and tool calls with
concise real output. Ctrl+T shows full reasoning, while Ctrl+O expands or
collapses original tool output in the conversation. Previous `display` entries
in `~/.kroma/ui.toml` are read for migration but no longer affect the layout.
No extra model calls are used to summarize tool output.

Common TUI shortcuts: Enter sends when idle; during a task Enter steers and
Alt+Enter (or Ctrl+Q on Windows) queues a follow-up. Shift+Enter / Ctrl+J
inserts a newline. Esc closes a popup or cancels a running task; Ctrl+C copies
a selection, clears a draft or quits when idle and empty. Ctrl+P / Ctrl+L
opens models; `/` searches commands. Ctrl+R searches sent prompts, Ctrl+T
shows a short preview of recent reasoning (rather than flooding the chat),
and Ctrl+O expands or collapses full command/output logs in the conversation.
The detailed view keeps every tool call visible, with short live previews. Alt+T (or `/transcript`)
opens the full searchable transcript. Shift+Tab cycles thinking levels, Alt+V
imports clipboard media, and `/help` or `?` with an empty draft opens help.
Ctrl+Q does not quit while a draft is unsent. Some Alt shortcuts may need
terminal keybinding remapping.
Some modified-key combinations depend on terminal support.

### Chinese / CJK terminal font

Kroma renders text in terminal cells; **the terminal chooses the font**, not
Kroma. If Chinese falls back to a serif face beside monospace English, install
an actual CJK terminal font such as **Sarasa Term SC (更纱终端黑体)** or
**Maple Mono CN**, then in **Windows Terminal → Settings → Profiles → your
Kroma/PowerShell profile → Appearance → Font face** select that font.
Use a CJK-aware terminal variant so Chinese takes two cells and English takes
one. This is an optional user terminal preference, not a required Kroma
setting, and changing fonts does not alter agent behaviour or project files.

The dark preset keeps the original neutral grayscale backgrounds and text,
with a teal Kroma identity rail, a violet user rail, and separate
success/warning/error colours. Model and context readings use the legible
`muted` gray instead of the decorative `faint` token. Kroma already reduces true-colour output to ANSI
256-colour terminals and supports `NO_COLOR` / `mono` for native terminal inks.

## 5. Safety and recovery

Kroma has no permission-approval prompts by design. Every tool call and its output
is visible, and in a Git worktree Kroma snapshots the files before each task into a
private shadow repository. If a change is unwanted, use `/checkpoints` and then
`/restore-files <n>`. Conversation rollback (`/rewind`) is separate and never
silently changes files.

Read [security.md](security.md) for the exact guarantees and limits.

## 6. MCP tools

MCP servers are **not started at launch** and their tool schemas do not consume
context until needed. The model gets two discovery tools:

- `mcp_search` — start/query configured servers and list matching tools
- `mcp_call` — call a discovered tool

Set `KROMA_MCP_EAGER=1` to load every MCP tool directly (the old behavior), or
`--no-mcp` to skip MCP for one run. Third-party server logs go to `null` unless
`KROMA_MCP_STDERR=inherit`.

## 7. Script output

`--json` emits one JSON object per line: task lifecycle, model deltas, tool events,
usage, and a final `result` or `error` object. A closed pipe (`kroma --json ... |
head`) stops output without a panic.

Exit codes: `0` success, `2` command-line usage, `3` model not configured,
`4` authentication/authorization, `5` model request failure, `6` context limit,
`1` other failure.
