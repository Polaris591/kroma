# Kroma v0.4 — Cache-aware TRC + APEC

Minimal AI agent harness written in Rust. This is a **new, independent project**; it does not import code from the previous Kroma.

Kroma supports Chat Completions, Responses and Anthropic Messages; streaming
model/tool execution; a built-in Shell tool with optional MCP tools and
on-demand Agent Skills; saved chat history; and TRC + APEC compression.
It intentionally has no plugin framework or web UI. Interactive terminals use
a lightweight Rust TUI by default; scripts and one-shot commands remain CLI.

## Terminal UI (Ratatui)

### Everyday commands (four-batch core usability pass)

Kroma keeps one Rust Agent runtime. The following shortcuts use existing
Shell, session, model, and event interfaces; none introduces an approval
framework, a second agent loop, or a mandatory plugin system.

- **`@path`** refers to a file in the project. Type `@src/age` and press
  **Tab** for path completion; `@src/agent.rs` keeps the path available for
  the agent's Shell tool rather than eagerly putting the entire file into
  model context. Multiple matches are listed. Paths containing whitespace
  currently require another form of reference.
- **`!command`** executes a command directly with the built-in Shell
  executor, without a model call. Output is shown locally, not inserted
  into the model transcript.
- **`/usage`** reports measured foreground input/output/cached tokens and
  model calls for the current process. Unknown provider measurements are
  marked unknown, and APEC usage is not silently added to foreground usage.
- **`/diff`** previews the Git diff (truncated for a small TUI).
  **`/tools`** also displays MCP servers that failed to connect at startup.
- **`/model`** lists saved profiles; **`/model profile-name`** switches
  models without changing the conversation history. The model's context
  capacity and output reserve are recalculated, and the existing context
  policy is rebuilt against the new budget. It does not change the default
  profile saved on disk.
- **Tab** completes an unambiguous slash command (for example `/exi` to
  `/exit`). Ambiguous matches are listed rather than chosen for you.
- **`/continue`** opens the most recently modified saved conversation.
  **`kroma --continue`** opens that conversation directly in the TUI. Plain
  `kroma` remains unchanged; the recent session is never restored implicitly.
- **Mouse copy:** drag-select visible conversation text, then right-click to
  copy. The selection can extend past the top or bottom of the transcript:
  keep holding the left button at the edge to auto-scroll and continue
  selecting across multiple screens. **Hover a hyperlink and Ctrl+left-click**
  to open its target. Both
  named Markdown links (e.g. `[report](./reviews/report.md)`) and bare
  `https://` URLs, `file://` URLs, and recognizable file paths are supported.
  Web links use your default browser; local paths resolve against the current
  project directory and open with the system default application. The hover
  hint and underline identify links; Ctrl+click does not start text selection.
  A local desktop uses the system clipboard directly; SSH sessions and
  machines without a local clipboard send OSC 52 to the terminal instead.
  OSC 52 requires compatible terminal settings, so the fallback confirms only
  that the request was sent. Mouse-wheel scrolling stays enabled.
- **Live Shell output** is previewed in the current tool activity while a
  command runs, with full stdout/stderr still stored by the existing Shell
  capture mechanism. The live preview is not sent to the model as extra text.
- **`/rename title`** names the current session; **`/sessions`** opens a
  searchable TUI session picker (the CLI lists names and IDs), and
  **`/resume`** accepts a unique name or ID. Session names
  are lightweight metadata; source session files and WAL are unchanged.
- **Ctrl+Z / Ctrl+Y** undo and redo edits to the input box, not project
  files or conversation history. **Ctrl+G** edits the current unsent draft;
  **`/editor`** starts an external-editor draft in
  `$VISUAL` or `$EDITOR`; in-line attachments cannot be edited externally.
- **`/export [filename.md|filename.json]`** exports the current session
  to the project directory; **`--version`** prints the binary version.
  Existing exports are never overwritten.

Noninteractive use: **`kroma --print "task"`** streams ordinary plain text;
**`kroma --json "task"`** prints one JSON object per line, with event types
such as `task_started`, `text_delta`, `tool_started`,
`tool_finished`, `model_finished` and `result`. Both accept piped stdin:
`cargo test 2>&1 | kroma --print "analyze failures"`. Large piped inputs
are stored as file references. JSONL uses stdout for events and stderr for
diagnostics. Both modes share Agent, Shell, model providers, and TRC+APEC.

Global/project `AGENTS.md` are loaded as before. When the task explicitly
references a nested project file, the applicable ancestor `AGENTS.md` files
are also loaded, without scanning and injecting unrelated directories.

### V1.1 visual design

Kroma uses a borderless, terminal-width workspace with only small side
margins on wide screens. Tables, paths, commands, code and diffs use the
available width rather than being capped at 100 columns. A quiet brand/context header,
editor surface and compact footer replace the old full-screen conversation
box. User prompts and assistant prose have distinct heading treatments;
tools appear as compact activity lines, with Ctrl+O expanding output and
Ctrl+R expanding reasoning. Colors are restrained and all built-in dark/light
text and status colors are tested for at least 4.5:1 contrast against the
page background. Mono uses the terminal's default foreground/background,
including light terminal themes; fenced code includes a language label and
Diff additions/deletions use distinct semantic colors. Custom TOML theme keys and keyboard shortcuts remain
compatible with V1. No UI plugin framework, layout config or additional
renderer dependency was introduced.

Run `kroma` (or `cargo run --release`) in a real terminal to open the TUI.
`kroma --no-tui` opens the original line-oriented CLI. `kroma "task"` keeps
the one-shot interface, and redirected/non-TTY input does not activate TUI.
The UI reuses the same Agent Loop, Shell/MCP tools and session WAL, with no
RPC server or UI plugin runtime. Ratatui 0.29 / Crossterm 0.28 preserve the
project's Rust 1.85 toolchain; Cargo.lock pins the compatible dependency set.

The default interface supports streamed assistant text, formatted Markdown,
compact Shell/MCP tool calls with expandable results (failed tools show the
reason without expanding), provider-exposed streaming reasoning collapsed by
default, conversation scrolling, persisted
sessions and the existing slash commands. Shortcuts:

- **Enter** sends; **Shift+Enter**, **Alt+Enter** or **Ctrl+J** insert a newline.
  Bracketed multi-line paste is supported; Up/Down navigate input and history.
- **PgUp/PgDn** or mouse wheel scroll conversation; scrolling up pins the
  reading position even as the model streams, and **Ctrl+End** returns to the
  latest output. **Ctrl+O** expands tool outputs; **Ctrl+R** shows reasoning.
- **Scroll bar:** drag the thin thumb at the right edge of the conversation
  to move directly through history. Its track is proportional to the transcript
  and loads older history when necessary. Ordinary text dragging keeps
  selection active while the viewport moves.
- **Ctrl+P** opens the searchable picker; **Tab** changes its category among
  commands, model profiles, sessions, and files. Type to filter, Up/Down to
  select, Enter to confirm, Esc to dismiss without deleting an unsent draft.
  The **`/model`** and **`/sessions`** commands open the respective pickers;
  typing **`@path`** and pressing Tab opens file choices when several match.
- Input editing respects complete Unicode grapheme clusters (including joined
  Emoji and combining marks), while inline attachment chips remain atomic.
- Older conversation history is rendered on demand as you scroll upward.
  Completed text layout is cached between unchanged frames; only visible rows
  are copied to the terminal. Plain usage updates do not reflow the history.
- **F2** cycles dark/light/mono themes; `/theme dark`, `/theme light`,
  `/theme mono` select a theme immediately, and `/theme reload` rereads the
  on-disk theme. `/help` shows the TUI command summary.
- The footer keeps the current **model**, **Think** and **Ctx** visible at
  ordinary terminal widths (including 80 columns), without duplicating the
  model name in the header. **Ctx** uses the total model context window. During a
  request `~` denotes a conservative estimate including tool schemas; after
  a completed request, upstream input-token usage replaces that estimate when
  supplied. `—` means no recent request, not zero usage. The **Think** label
  shows the requested reasoning level. **Shift+Tab** cycles
  auto/off/low/medium/high/xhigh/max, or use `/think high` to select directly.
  The change applies to the next model request, and gateways may reject a
  level they do not support. Short terminals compact the footer automatically.
- Model output streams as it arrives, with a subtle cursor and working/waiting
  states. Reasoning stays collapsed by default; Ctrl+R reveals provider-exposed
  reasoning deltas as they arrive. Completion reconciles the entry without
  duplication. Providers exposing only final reasoning show it after completion.
  Encrypted or otherwise unavailable private reasoning is never displayed.
- **Esc** dismisses the file-path prompt or transient feedback; it never
  silently deletes the current draft. Short notices disappear on the next
  interaction; task errors also appear in the scrollable conversation.
- **Ctrl+C** cancels an active task. While idle it clears the draft (including
  pending attachments); pressing it again when the input is empty exits TUI.
  While a task runs, submitted text steers it,
  `/steer text` explicitly steers, and `/queue task` adds a follow-up.
  **Ctrl+Q** quits when idle; `/exit` is also supported.
- **Alt+V** pastes a desktop clipboard image or text. When the clipboard is
  unavailable, a separate path prompt accepts a file accessible to Kroma
  without destroying the unfinished message. Alt+V explicitly folds pasted
  text into an inline attachment; ordinary terminal paste is folded only for
  long text (at least 1000 characters or more than 12 lines),
  e.g. `你看看【ui.png】怎么样，分析一下【180字符】`. The chip is one
  editable unit: Backspace/Delete removes it along with its pending file
  reference, while plain text can be typed before, between or after chips.
  The visible labels are not the identity keys; the WAL stores an opaque
  marker and immutable local file reference. Old list-style session
  attachments remain readable. Limits: eight attachments, 32 MiB per file,
  8 MiB per image, 8 MiB per pasted log.
- **Multimodal context projection** preserves every original in the session
  WAL. Images stay active throughout their initial task (including tool
  rounds) and one immediate follow-up, but older images are lightweight text
  references on later turns, avoiding repeated historical base64 uploads.
  Up-arrow input recall preserves the original media references; removing
  a recalled chip or clearing the draft never deletes a previously submitted
  image. While an Agent task is running, new media/long-log pastes are
  deferred with a visible notice instead of being silently ignored.
  When the upstream
  declares the selected model text-only, even active images are replaced by
  explicit image-omission references instead of sending unsupported media.
  If capability metadata is unavailable, support is unknown. When the
  upstream explicitly rejects image input before streaming, Kroma retries
  once with text image references instead of guessing model support from
  its name. A different provider error is returned unchanged.
  Small pasted text (up to 4 KiB) reaches the active model inline; large
  logs and other files remain Shell-readable references. Each item stays
  in its exact position relative to typed text, across Chat Completions,
  Responses and Anthropic requests. Current images have a separate
  approximate input-budget allowance and 16 MiB combined wire-size cap;
  image tokens remain provider-specific rather than exactly predictable.
  Image-heavy requests do not recalibrate the text-only APEC budget heuristic.
  Existing saved media files remain available to other sessions and branches;
  Kroma does not guess that a media file is unreferenced and delete it.
  Sending an image never makes a text-only model visual.
  A terminal/SSH host does not automatically share another device's binary
  clipboard data; this is generic desktop clipboard support, not a custom
  upload or remote clipboard service.

The TUI `Ctx` status shows current request input tokens / total context window
(e.g. `100k / 500k · 20%`), not cumulative session usage. Estimates carry a `~`
marker; unknown usage is shown as `—`. Output reserves still reduce the internal
input/compaction budget, but do not reduce the displayed window or percentage base.

The first theme change writes `~/.kroma/theme.toml`. AI can edit this file
directly, then run `/theme reload` without recompiling Kroma. For example:

```toml
preset = "dark"
accent = "#7ec6ec"
user = "#a2deb4"
border = "#434e63"
```

Supported optional color keys are `accent`, `text`, `muted`, `border`,
`background`, `user`, `tool`, and `code`, using six-digit `#RRGGBB` values.
If the file is absent, Kroma uses its built-in dark theme without creating
files. No widget/plugin/layout SDK is needed for theme changes.

Offline UI checks: `cargo test --locked` and (on Linux/macOS)
`cargo build --release --locked && python3 benchmarks/tui_pty_smoke.py`.

## Architecture Foundation (P0-P4)

### Extension-ready composition

The default startup now exposes `app::default_runtime_with(config, customize)`
and its `RuntimeComponents` (model, tools, context policy, prompt, event sink).
Extensions read the current model with `components.model()` and replace it via
`components.replace_model(backend, ContextConfig)`, which updates the model
and context/output limits together. An adapter may implement
`ModelBackend::project_for_budget` to omit provider metadata that is not
transmitted; this affects estimates only, never durable history.
`CommandRegistry` accepts additional slash commands without editing the CLI.
Tools return structured `ToolResult { content, is_error }`, while only the
original content string is persisted and sent to the model. The Agent no longer
owns the Skills catalog. Session lifecycle events fire after durable changes.

`ContextFactory::branch_state` handles Fork/Rewind: a full-history copy
preserves context state; the built-in policy can rebuild a shortened history;
an external policy must explicitly implement partial rewind or the operation
fails without silently discarding state. These are internal Rust extension
points, not a dynamic plugin installer or runtime.

### Completed-turn history, fork, rewind and undo

In interactive mode, `/history [count]` shows completed assistant-response
checkpoints with stable numeric positions, including `#0` (the empty session).
`/fork [number]` creates and switches to an independent session at that
checkpoint, without modifying the original. `/rewind <number>` shortens the
**current** session to that checkpoint, but first saves the entire previous
history in a new sibling session; the command prints its ID for `/resume`.
`/undo` rewinds the most recent completed turn (or an incomplete tail) in
the same way. A session branch retains its own checkpoint, WAL, context
state and copies/hard-links of any large Shell outputs referenced by its
messages. Branches are regular named sessions, visible via `/sessions`.

Only conversation state is forked/rewound: commands, edited files, Git
commits, detached processes and external MCP side effects are **not**
reverted. Use Git for source changes. Derived APEC/TRC context starts fresh
from the original preserved transcript after a branch or rewind; it is
reconstructed on subsequent requests instead of copying summary indexes
from an abandoned future. This keeps session history readable without a
new database or an additional agent checkpointing service.

Offline regression tests: `cargo test --test session_branches`.

### MCP tools and Agent Skills

Kroma's optional MCP client registers remote tool definitions into the same
`ToolRegistry` as Shell. No MCP configuration means no extra processes,
network requests or tool definitions. MCP servers may be configured in
`~/.kroma/mcp.json` and `<project>/.kroma/mcp.json` (project entries override
same-named global entries), or specify a single configuration path with
`KROMA_MCP_CONFIG`. The JSON schema is:

```json
{
  "mcpServers": {
    "filesystem": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-filesystem", "/your/workspace"]
    },
    "remote": {
      "url": "https://example.com/mcp",
      "headers": {"Authorization": "Bearer ${MCP_API_TOKEN}"}
    }
  }
}
```

These entries are examples; the named server, npm installation and remote
endpoint are not bundled with Kroma. A server uses **either** `command`
(`args`, `env`, optional `cwd`) **or** `url` (optional `headers`).
Use `${ENV_NAME}` placeholders to resolve credentials from the environment.
Local stdio subprocesses inherit the Kroma environment by default.

The default MCP protocol version is `2025-11-25` and uses the standard
initialize/initialized handshake. To use the newer stateless MCP API, set
`"protocolVersion":"2026-07-28"` on that server. Stdio and Streamable HTTP
are supported, including JSON/SSE responses, tool-list pagination and tool
calls. Tools are named `mcp__<server>__<tool>`; a server that fails to start
or list its tools is skipped with a warning rather than disabling Shell.
MCP resources, prompts, sampling, elicitation, and OAuth discovery are not
implemented. Local/remote MCP servers may execute arbitrary operations
under their configured credentials; only connect servers you intend to use.

Agent Skills are discovered at Kroma startup from these directories:

- `~/.agents/skills/<name>/SKILL.md`
- `~/.kroma/skills/<name>/SKILL.md`
- `<project>/.agents/skills/<name>/SKILL.md`
- `<project>/.kroma/skills/<name>/SKILL.md`

Later directories override earlier ones for the same skill name. The
`SKILL.md` must contain YAML frontmatter with `name` (matching the folder)
and `description`. Kroma inserts **only** this metadata into its system
prompt and registers a `load_skill` tool. The model should invoke the tool
for a relevant task before using the full instructions. The tool reads the
current SKILL.md body and returns its directory, so referenced
`references/`, `scripts/` and `assets/` files can be read through Shell
as needed. To discover new skills or changed descriptions, restart Kroma;
editing an already-discovered skill's body takes effect immediately.

Offline integration checks: `cargo test --test mcp_skills_e2e`.

### Global/project instruction files and live task input

Kroma loads optional `~/.kroma/AGENTS.md` (global instructions) and
`<KROMA_WORKDIR>/AGENTS.md` (project instructions). Precedence is built-in
prompt, global instructions, then project instructions. Both are reloaded on
each **new task** and appended to the outgoing system-prompt projection, not
the saved transcript. No files are created automatically. If either file is
absent, the previous behavior is preserved.

In interactive mode, type while a task is running:

- `/steer <instruction>` (or plain text): add an instruction to the **current**
  task. Kroma applies it at the next model-turn boundary after committing any
  returned tool calls/results. Accepted steering is journaled as a user message
  before being sent to the model. A late instruction can extend a task that
  was about to finish. Pending steering is discarded if the task is cancelled
  or fails, rather than silently leaking into the next task.
- `/queue <task>`: schedule a **separate** task to run in order after the
  current task finishes successfully. `/queue list` or `/pending` displays
  pending tasks, `/queue clear` discards them, and `/queue run` resumes
  paused follow-ups. A cancelled or failed task pauses the queue.

Follow-up tasks are held in memory while the CLI runs, not persisted across
Kroma process restarts. Once each task begins it is saved through the normal
session WAL. The noninteractive `kroma "task"` command still runs exactly
one task, and embedding hosts can steer via `Agent::task_input()`.

Offline regression tests: `cargo test --test instruction_steering`.

P12 review repairs tighten the extension boundaries: a ContextPolicy can no
longer overwrite the original transcript when persisting strategy state;
extensions' prompt/tool definitions count toward the final request budget;
the Agent future can run on Tokio's multi-thread runtime; model adapters
provide generic tool-free text generation rather than APEC-specific prompts;
and tool/model completion events occur after the corresponding WAL entry.
Custom context state can be explicitly reset with
`kroma recover-context [session-id]` when the custom implementation is no
longer installed; this **retains full raw history**, but resets derived
summaries. See `benchmarks/P12_ARCHITECTURE_REVIEW_REPAIRS.md`.

Kroma now exposes a Rust library as well as its original CLI. The default
implementation still runs **one Shell** and the same OpenAI-compatible
provider, stable prompt and cache-aware TRC + APEC.

- `app::default_agent` combines the built-in implementations.
- `model::ModelBackend` accepts tool definitions supplied by the runtime;
  `tools::ToolRegistry` registers built-in or test tools without editing the
  Agent loop.
- `context::ContextPolicy` and `ContextFactory` allow policy replacement.
  `ports::ContextCommit` keeps authoritative raw history and strategy-state
  commits in SessionStore. The legacy v3 built-in `compression` wire format
  remains unchanged. Non-default strategies use a versioned state envelope.
- `prompt::PromptComposer` adds or replaces prompt content only in the
  request projection; `events::EventSink` exposes optional lightweight
  runtime notifications. Without a subscriber no event queue is created.

These are **internal integration interfaces**, not a plugin ABI or plugin
loader. No approval layer, service container or new background worker was
introduced. See `benchmarks/P11_ARCHITECTURE_FOUNDATION_REPORT.md` for
invariants, isolated replacement tests and regression commands.

## Core-efficiency measurements and opt-in profiling

Set `KROMA_PROFILE=1` when running a real task to print lightweight
`[profile] model_start/model_done/tool_start/tool_done` phase markers, plus a
task-total `[profile]` line at completion, failure or cancellation. Phase markers help
identify the last active phase if a long task is interrupted. The total includes wall time,
context preparation (including any asynchronous compaction waits/checkpoints),
model-response time, tool execution time, session journal writes, APEC finalization,
request/tool counts, and JSON byte counts for original history versus model
request messages. The optional byte measurement adds a serialization pass per
request, so leave profiling off for normal low-latency operation. Existing
`[usage]` lines display **provider-reported** input/output/cache tokens; missing
upstream counts stay `n/a`. JSON bytes and rough token estimates are **not**
billable tokens or precise monetary costs. Values sent to stderr do not include
tool output content or API keys as part of the new profile line.

Example: `KROMA_PROFILE=1 cargo run --release -- "Run the tests and report failures"`

Offline reproducible tests, without a paid model API:

```sh
cargo test --locked --offline -q
cargo build --release --locked
python3 benchmarks/mock_profile_smoke.py
python3 benchmarks/p10_config_regression.py
```

The optional ignored long-history CPU/IO benchmarks can be slow; run only
the desired named benchmark with `cargo test --release --locked <name> --
--ignored --nocapture`. The opt-in P9/P10 experiments skip live model calls
unless their dedicated environment variable is explicitly enabled.
The ignored long-history benchmarks compare the previous full-serialization
TRC/APEC algorithms against the new incremental JSON-size planning on identical
data, then assert identical TRC clearing decisions and summary boundaries.
See `reviews/KROMA-E1-CORE-EFFICIENCY-2026-10-26.md` for measurement details.
That E1 optimization retained the existing cache-aware TRC + raw-history APEC
policy and thresholds. In P10 the standalone, zero-hit first-use
duplicate-line filter was removed after checking retained Rust/Python task
outputs; the original raw outputs and overflow recovery are unchanged.

To compare real model tasks separately from E1's synthetic benchmarks, use
`python3 benchmarks/agent_task_suite.py --list` or `--self-test`. The optional
`--run` uses the explicitly configured Kroma provider to measure task
correctness, latency, and reported usage in isolated workdirs; it may incur
API charges. This test runner does not change Kroma's execution behavior.

## P1: DeepSeek thinking and tool-call continuity

### Multiple model API protocols and model metadata

Kroma supports three wire protocols through the same internal Agent Loop:

- `KROMA_API_PROTOCOL=chat` (default): OpenAI-compatible Chat Completions, including DeepSeek behavior.
- `KROMA_API_PROTOCOL=responses`: OpenAI Responses `/responses`, with streamed function calls and stateless encrypted reasoning continuation.
- `KROMA_API_PROTOCOL=anthropic`: native Anthropic Messages `/messages`, with streamed tool calls, tool results, and signed thinking continuation.

Use an API **base**, such as `https://api.openai.com/v1` or
`https://api.anthropic.com/v1`, not the complete endpoint URL.
Both environment variables and `kroma config set <name>` can select
the protocol and reasoning level.

`KROMA_REASONING_EFFORT=auto` (default) lets the provider choose its
default effort. On Anthropic, if the models endpoint explicitly advertises
adaptive thinking, Kroma enables that mode. Explicit levels include
`low`, `medium`, `high`, `xhigh` and `max`. `off` requests disabled
thinking; some models do not support it. Not every model and provider accepts
every effort level. The legacy `KROMA_DEEPSEEK_THINKING` remains independent.

At startup, Kroma attempts `GET /models/{model}` to discover the context
window, maximum output, and advertised capabilities. Metadata may be absent.
Saved profile limits (see *Saved API profiles*) take precedence over this
metadata. The fallback context window is 128000 tokens. For Responses and Anthropic,
the default generation maximum is 4096 tokens if the upstream limit is unknown;
Chat retains its previous behavior when no limit is available.

`KROMA_CONTEXT_WINDOW` overrides the model context limit,
`KROMA_MAX_OUTPUT_TOKENS` controls requested output tokens, and
`KROMA_OUTPUT_RESERVE_TOKENS` overrides output reservation for context planning.
The effective requested maximum is capped by a discovered upstream maximum.
APEC summary generation also respects the known provider output ceiling.
No separate third-party model database is required.

Example:

```sh
KROMA_BASE_URL=https://api.anthropic.com/v1 \
KROMA_API_PROTOCOL=anthropic \
KROMA_MODEL=claude-your-model-id \
KROMA_REASONING_EFFORT=high \
KROMA_MAX_OUTPUT_TOKENS=16384 \
cargo run --release
```

When switching protocols, prefer a fresh session: signed and encrypted
provider reasoning items are not portable between different APIs.

Offline integration tests require no upstream credentials:

```sh
cargo test --test model_protocol_mock_e2e
```

They use a local HTTP/SSE server and execute a real Shell command through
Agent, then verify the tool-result continuation, WAL restart and standalone
APEC summary on both wire formats. They do **not** prove compatibility
with a real provider or any particular proxy. Run live integration tests
before advertising production-level support.

- Streaming `reasoning_content` deltas are reassembled into a **separate** assistant-message field, persisted in snapshots and WAL, and replayed with the corresponding tool calls and results. The classic line-oriented CLI outputs only `content`; the TUI may show the persisted reasoning when the user presses Ctrl+R. Saved sessions without the new optional field continue to load.
- With the `tools` parameter present, DeepSeek thinking mode requires the reasoning field on previous assistant messages. Kroma leaves it intact in un-compacted history, including across restart and `/resume`. TRC changes only eligible tool **results**; APEC may replace entire old **completed** turns with summaries. The raw transcript, including reasoning content, remains on disk. Summaries do not reproduce reasoning verbatim.
- OpenAI-compatible tool requests now omit the redundant `tool_choice: auto` parameter (DeepSeek V4 thinking can reject it). Older assistant tool-call messages with `content: null` are projected as `content: ""` for provider requests without altering saved raw records.
- **Opt-in DeepSeek V4 thinking:** set `KROMA_DEEPSEEK_THINKING=enabled` to send `thinking: {"type":"enabled"}`, or set `high`/`max` to additionally send `reasoning_effort`. Leave it unset for providers or gateways that do not accept these fields. This does not auto-detect models or force thinking on; some reverse proxies implement different semantics.

## P2: Lean modules and multiple conversations

The runtime is split by responsibility: `provider.rs` (HTTP/SSE and API request mapping), `protocol.rs` (shared messages), `context.rs` (TRC/APEC), `tools.rs` (tool dispatch/schema), `shell.rs` (process execution), `session.rs` (checkpoint/WAL/lock), `sessions.rs` (session directory and IDs), and `agent.rs` (small execution loop). There is no new provider registry, plugin framework or database.

Interactive commands:

- `/new` creates a new conversation with an independent snapshot, journal, and compression state, without erasing the previous one.
- `/sessions` lists saved conversations for the current default session namespace. `*` marks the active one.
- `/resume <id>` switches to an existing conversation; `/resume default` switches to the original auto-loaded project history. Invalid/missing/busy/corrupt sessions are rejected without replacing the current one.
- `/clear` clears **only the active** conversation and its captured outputs. `/exit` quits.

The original `~/.kroma/sessions/<project-hash>.json` still serves as `default`; new conversations are siblings named `<project-hash>--<session-id>.json`, each with its own `.wal` and lock. When `KROMA_SESSION_FILE` is set, its filename becomes the base instead. Do not rename/move only the JSON checkpoint: back up each conversation's **JSON and WAL together**. Session switching never re-executes a command with an unknown result.

## Long-session performance and recovery

Kroma saves full JSON checkpoints and appends individually synced, checksummed
records to a matching `.json.wal` journal between checkpoints. New messages
do **not** rewrite the entire session. Compression-state changes and periodic
thresholds (256 records / 16 MiB) trigger checkpoints. On restart, Kroma
replays complete records, ignores a truncated tail and marks interrupted
tool calls **unknown**, never rerunning commands. Back up or move both the
`.json` checkpoint and its `.json.wal` journal when transporting a session.

APEC memory requests keep the final factual summary concise, but leave
additional completion allowance for reasoning-capable models. A truncated
summary (`finish_reason=length`) is never accepted as memory: Kroma retries
with more completion headroom and reports an explicit failure if all retries
are exhausted. A failed background summary is not repeatedly restarted by
TRC-only state changes in the same user turn; a subsequent user turn may
retry. Full original conversation history is always retained. This recovery
path was validated against a real saved multi-stage development session;
see `benchmarks/P3_REVIEW.md`.

## P4: Shell usability and real task comparison

On Unix Kroma uses Bash when `/bin/bash` is installed, and falls back to POSIX
`sh` otherwise. On Windows it uses Git Bash (`Git\bin\bash.exe` or
`Git\usr\bin\bash.exe`, also detected under `%LOCALAPPDATA%\Programs\Git` and on
`PATH`) and only falls back to `cmd /C` when no Git Bash exists; `bash.exe` in
`System32` is ignored because it launches WSL, which cannot run Windows paths.
`KROMA_SHELL` overrides the choice (a `bash.exe` path, or `cmd` to force native
semantics). This prevents Bash-specific model-generated syntax from
unexpectedly failing on systems with Bash, and keeps one dialect for both
platforms. The tool description names the dialect actually in use.
The shell is always started in the requested working directory; only Unix gets
a login shell, because a Windows profile can `cd` away from it. Bash now
enables `pipefail` by default: `cargo test | tail` cannot conceal a failing
test behind the success status of `tail`. Note that `head` can close a
pipeline early, producing upstream SIGPIPE (exit code 141); inspect the
actual command and result instead of blindly assuming a test failed.
For ordinary inline results the model sees the full `exit_code`, `stdout` and
`stderr` without redundant byte-count fields; file-backed large output
retains its file reference and byte totals.

The change was chosen after isolated A/B/C/D tool-interface experiments.
See `benchmarks/P4_REPORT.md` for all raw results and qualifications, including
the separate same-model Pi comparison. No task-level token or duration limit
was introduced.

Shell outputs above 64 KiB per stream remain in referenced files with a
head/tail preview. Commands have no output size limit: the entire original
output remains readable by Shell in smaller slices. The context planner also
uses an emergency *request-only* preview of unconsumed tool results when an
old inline output or several results would overflow the hard context limit.
It preserves original saved messages and does not mark unread results as
TRC-consumed. Temporary output captures
from cancelled/failed commands are removed. New captures belong to their
specific session; Kroma cleans up that session's unreferenced captures after
24 hours and `/clear` removes its captured outputs. There is no blanket
expiration for referenced outputs. Context projection uses a forward tool-ID
map instead of repeated backward scans. No Shell permissions or API key
inheritance changed, and the same compression scheme remains in place.

## P8: P7-winning F execution guidance, serial tool execution

P7 tested 72 isolated coding stages across A/B/F. F reduced total foreground
requests, shell calls, and combined foreground+APEC uncached tokens, but
three F integration-review stages and one B stage failed to *finish* because
model streams timed out; all 72 stages passed independent code acceptance.
See `benchmarks/P7_FINAL_REPORT.md` for the full results and caveats.
The production system hint now tells the agent to edit once sufficient
evidence is found rather than repeatedly re-inspecting unchanged files.

Kroma encourages the model to request **multiple independent shell inspections in one assistant response** when useful, rather than unnecessarily making a separate model request for each inspection. Dependent work, edits, and verification still happen in the order they require. This is a short, stable system-prompt hint applied to the outbound request view, **not** an extra model call or a new tool.

If the model returns multiple tool calls together, Kroma executes them **strictly in response order**, persists each raw result with its corresponding `tool_call_id`, and sends all results back together in the next model request. No tool execution is parallelized. Normal Kroma startup uses this policy automatically; no environment variable or mode selection is needed.

The previous experimental `KROMA_TOOL_EXPERIMENT` selector and parallel-read dispatcher were removed from production code. Historical `benchmarks/tool_batch*.py` files document exploratory experiments and **must not be used as live B/C selectors** on this single-policy build.

## v0.4: Single compression policy

### Tool-output preservation (P10 simplification)

There is no longer an extra command-type classifier or duplicate-line filter
before the first model read. Ordinary short Shell output is delivered intact.
Large streams stay fully available in referenced files, and hard-limit context
projections keep the original message in the saved history. This removes a
special-case path which showed no compressible results in the sampled real
Rust and Python task histories.

Kroma uses **one built-in compression strategy: cache-aware TRC + APEC** (the tested experimental C policy). There is no runtime strategy selector. The retired `KROMA_FUSION_EXPERIMENT` setting is ignored; it cannot enable any other compression behavior. Historical scripts under `benchmarks/` are experimental records and no longer compatible with the single-policy production build without modification.

TRC replaces eligible, already-consumed old tool outputs only in the model's projected history; the complete session always stays on disk. **Outside hard-limit emergencies**, Kroma requires observed provider cache tokens and accepts a shortening only when an approximate three-future-read saving outweighs estimated invalidation of the cached suffix. If cache usage is unknown, non-emergency TRC conservatively does not clear. This is a heuristic, not exact pricing. At the hard limit, context safety takes precedence.

APEC asynchronously summarizes **complete, original tool evidence**, never TRC or hard-limit previews. Existing incremental summaries are append-only until an occasional rollup; rollups can invalidate a prompt-cache prefix. No competing evidence-filtering summarizer or alternative compression modes are part of Kroma.

If APEC fails, Kroma suppresses repeated attempts during the current agent turn; a later user turn can retry without resetting the conversation. Session recovery for a partially completed batch retains completed tool results and marks missing results as unknown rather than automatically executing them again. Duplicate model-generated tool call IDs are rejected before execution. None of these changes alters the shell's environment-variable inheritance or the user's authority to run shell commands.

### Review fixes

- APEC splits oversized historical evidence into bounded chunks for summarization instead of blocking on a single oversized completed turn. It finalizes an in-flight summary before successful one-shot task exit, and cancels it on Ctrl+C.
- Generated memories are supplied as untrusted assistant-role history, never as additional system instructions.
- Old memory segments are periodically consolidated when their count or token footprint grows; full original history remains on disk.
- Session JSON has an adjacent process-lifetime lock file to prevent concurrent Kroma writers on Unix/Windows.
- Shell stdout/stderr is written directly to private per-workspace capture files. Outputs up to 64 KiB per stream are returned inline; larger streams return a short preview and an absolute path to the complete file. This avoids unlimited output buffering and avoids silently throwing away bytes.
- Nonstandard model `finish_reason` values are now treated as errors. APEC cost is reported separately from primary model traffic.
- **Known tradeoffs:** journal entries are synchronized for durability, and periodic checkpoints still rewrite the full JSON history; a very long single session may still incur occasional large writes. APEC adds summarization requests and may postpone completion while a pending summary is finalized. Summaries remain lossy; source history is authoritative. Context tokens are still estimated rather than counted with a model-specific tokenizer.

- **TRC (Tool Result Clearing):** Only a tool result included in a **successfully completed model request** is eligible for TRC clearing. The result that just came back from Shell is passed to the model without normal TRC shortening, except for hard-limit emergency previews. Older consumed verbose results may be replaced **only in the model request** by compact previews and a pointer into the complete session archive. Normally the two newest eligible consumed tool outputs and recent messages remain intact; at the hard limit, consumed results can also be shortened.
- **APEC (Asynchronous Progressive Context Compression):** The soft/hard triggers are **50% / 90% of available input tokens**, after reserving capacity for the model's output. A background, no-tools summarizer processes a safe **original raw-history** segment (never TRC previews) and appends a new immutable memory block S1, S2, S3, etc. Append-only blocks preserve a cache-stable prefix until an occasional summary rollup rewrites older blocks. Recent messages and pending tool calls remain untouched. At the hard threshold, Kroma waits for a background summary or runs a synchronous one; when compaction is unsafe or unsuccessful, it reports an error rather than silently dropping history. TRC and APEC may operate concurrently because an APEC snapshot is validated against the raw history and summary boundary, not the mutable TRC view.
- **Persistence:** The complete raw history and compression state are recovered from the session checkpoint plus its append journal. v0.3 and v0.4 histories are migrated automatically: an old single summary becomes S1, and unsafe previously cleared tool results are restored in the projected view. The system prompt remains unchanged.
- **Settings:** `KROMA_CONTEXT_WINDOW` (default 128000) should reflect the actual provider context limit. `KROMA_OUTPUT_RESERVE_TOKENS` optionally overrides the output reservation (default: min(4096, 1/8 of the context window)). With a 128000-token window and the default 4096-token output reservation, the effective input capacity is 123904 tokens, with soft/hard triggers at 61952/111513 estimated input tokens. Token size is estimated heuristically and calibrated upwards using reported input usage; estimates are **not** exact tokenization. If the provider refuses a summary call or the context is too large, compression reports an error without deleting the source transcript.
- **Limitations:** APEC uses a separate model request and therefore consumes additional tokens. The background summary is committed on the **next model request boundary**, not necessarily as soon as it completes. Old tool output references are indexes in the saved session checkpoint and/or WAL, not a dedicated retrieval tool. APEC's non-streaming summary endpoint must be supported by the chosen model provider.

## v0.3 additions

- Ctrl+C cancels an active request or shell task; interactive mode returns to the prompt. On Unix, each shell runs in its own process group so ordinary descendants are killed on task cancellation. Detached processes that deliberately leave the group are outside this guarantee. Windows gets the same guarantee through a Job Object: the shell is created suspended, assigned to the job, and only then resumed, so no descendant can start outside the job. Cancellation calls `TerminateJobObject`. If the job cannot be assigned the shell fails instead of running uncontained.
- Cancelled tasks append a cancellation note to saved history. Missing results from interrupted tool calls are explicitly marked unknown so they are not automatically rerun. Interrupted *model streams* also leave a durable failure note, without turning partial assistant text into a complete response or replaying tools.
- Each successful model request reports total elapsed time, first content/tool delta latency, input/output token counts, and reported prompt-cache hits. An aggregate appears at the end of each completed task. All counts come directly from the upstream API; missing counts display `n/a`, not an estimate.
- Streaming requests include `stream_options: {"include_usage": true}`. Providers that reject this option require a compatibility fallback; cached tokens and usage are not guaranteed by every API.

## Requirements

- Rust 1.85 or newer (Cargo)
- An OpenAI-compatible Chat Completions API and model that supports `stream: true` and `tools` / function calling
- A local shell (Bash — `/bin/bash` on Unix, Git Bash on Windows; POSIX `sh` or `cmd` as fallbacks)

## Run

### Saved API profiles (new Rust Kroma)

Run `kroma config set workbuddy` (or `cargo run -- config set workbuddy`) to
enter an API base URL, model ID, DeepSeek thinking level, reasoning default,
context window, maximum output tokens, advertised thinking levels and an
optional API Key. Every prompt keeps the saved value on Enter; enter `-` to
clear an optional limit or level list. The API Key prompt hides terminal echo
and never places the secret on a command line. Press Enter to keep an existing
saved key, or enter `-` to clear it. The Key prompt hides terminal echo (termios
on Unix, the console echo flag on Windows) and works in an interactive
terminal; for headless machines use the `KROMA_API_KEY` environment variable
instead.

Each saved profile may also declare the model's own limits and tiers directly
in `~/.kroma/config.json`:

```json
{
  "active": "astra",
  "profiles": {
    "astra": {
      "base_url": "https://provider.example/v1",
      "model": "gpt-6-astra",
      "api_protocol": "chat",
      "reasoning_effort": "high",
      "context_window": 500000,
      "max_output_tokens": 128000,
      "thinking_levels": ["medium", "high", "xhigh"]
    }
  }
}
```

- `context_window` replaces upstream metadata for context planning; the
  `KROMA_CONTEXT_WINDOW` environment override still wins.
- `max_output_tokens` sets the requested completion maximum; the
  `KROMA_MAX_OUTPUT_TOKENS` environment override still wins, and an explicitly
  discovered upstream maximum still caps the request.
- `thinking_levels` restricts the tiers the model accepts, in display order.
  Shift+Tab cycles only those levels, and an unsupported level is refused
  instead of being sent. `auto` stays available as "provider default", and the
  profile's `reasoning_effort` default must be one of the listed levels unless
  it is `auto`. Omit the field to allow every Kroma level.
- All three fields are optional; profiles saved before they existed keep the
  previous behavior. `context_window` must be 1024–2000000 and
  `max_output_tokens` must be smaller than `context_window`.

Profiles are stored in `~/.kroma/config.json` (not the **old Kroma**'s
`~/.kroma/config.toml`). New files are created privately, and on Unix the
loader rejects profile files readable by other users. No credentials are
printed by `config list` or `config show`.

When both `KROMA_MODEL` and `KROMA_BASE_URL` are explicitly set, and
`KROMA_PROVIDER_PROFILE` is not set, Kroma uses an environment-only
configuration without reading any saved profile. This also avoids unintended
inheritance of a saved API key or thinking setting. An explicitly selected
profile, or incomplete environment settings, still uses normal profile
validation. The saved config remains unchanged.

```sh
kroma config list
kroma config use workbuddy
kroma config show
kroma 'Read the README and describe this project'
```

The first profile saved becomes the active default. Multiple profiles can be
saved using `config set <name>`, then selected using `config use <name>`.
`KROMA_PROVIDER_PROFILE=<name>` chooses a profile for a single process without changing
the saved default. `KROMA_MODEL`, `KROMA_BASE_URL`, `KROMA_API_KEY` and
`KROMA_DEEPSEEK_THINKING` individually override the selected profile when set.
`KROMA_CONFIG_FILE` selects an alternate JSON profile path, useful for testing.
Saved settings take effect on next launch; interactive session switching does
not change the model connection mid-task.

Environment-only startup remains compatible:

```sh
export KROMA_BASE_URL="https://your-provider.example/v1"
export KROMA_API_KEY="your-api-key"
export KROMA_MODEL="your-model-id"

cargo run -- "List the files in this project using the shell tool."
```

To launch an interactive session:

```sh
cargo run
```

Commands: `/new`, `/sessions`, `/resume <id>`, `/clear` (active session only), `/exit`. `cargo run -- --help` shows all environment options.

`KROMA_WORKDIR` selects the starting working directory for shell commands, defaulting to the directory from which Kroma starts. `KROMA_SHELL` selects the shell on Windows (`bash.exe` or `cmd`). `KROMA_API_KEY` can be omitted for gateways that do not require a key.

## Streaming and saved chat

- Responses are displayed as text arrives rather than waiting for the entire model response. Tool calls are assembled from streaming fragments before execution.
- The complete conversation (including tool calls and results) is stored across `~/.kroma/sessions/<project-hash>.json` and its `.wal` journal and automatically reloaded when Kroma is opened again in the same working directory. Both are separate from the source repository.
- To pick another default history file, set `KROMA_SESSION_FILE=/path/to/conversation.json`. New sessions are stored alongside that file; `/clear` replaces only the active saved conversation with a fresh one.
- On Unix, newly created history files are private (`0600`). If the process dies during a tool call, Kroma records the missing result as unknown on reopening, without automatically repeating a potentially destructive command.
- History is stored as JSON checkpoint plus a checksummed, append-only journal without pruning. The checkpoint may lag the latest turn until the next compaction checkpoint. Only one Kroma process may open a given session at a time (an adjacent lock file enforces this). Session metadata (model choice, names, most recent session) is updated under an adjacent cross-process lock, so concurrent writers cannot lose an update. Session checkpoints, profile files and session metadata are replaced through `src/atomic_file.rs`: `rename` on Unix and `MoveFileExW(MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH)` on Windows, since a plain `rename` fails on Windows when the target exists.

## Execution contract

1. A user message is added to the conversation.
2. Kroma calls `POST {KROMA_BASE_URL}/chat/completions` with the message history, `stream: true`, and the `shell` tool schema.
3. If the model returns `tool_calls`, Kroma executes each `shell(command)` call in order and appends the result with its `tool_call_id`.
4. Kroma sends the updated history to the model, repeating until it receives a normal assistant response. There is no artificial Agent Loop step limit.

Each shell result reports `exit_code`, `stdout`, and `stderr`. Shell commands have no artificial timeout or output limit. Failed commands are reported back to the model, which decides whether to retry or correct the task. Streaming model HTTP requests have a 20-second connection-establishment timeout and a 300-second idle timeout (for response headers or a gap between streamed network chunks), **not** a 120-second total-request deadline. Before receiving any stream data, transport timeouts, connect errors, and transient HTTP 429/502/503/504 may be retried up to 3 additional times. Numeric `Retry-After` values are honored up to 10 seconds; longer or unparseable server wait instructions are not silently overridden. Permanent HTTP failures (such as 401) return immediately. Partially delivered streams are never retried to avoid duplicate output or tool execution. Failed attempts have unknown provider usage unless separately reported. Separate non-streaming APEC requests retain a 120-second request deadline. Task summaries now include a `task_status` marker on completion, failure and Ctrl+C cancellation; incomplete provider usage remains unknown rather than zero.

**Security:** `shell` executes arbitrary commands with the permissions of the Kroma process. The working directory is **not** a sandbox. Use a container or a restricted account for untrusted tasks, and never run this prototype with elevated privileges against important data. Unlimited command output may exhaust memory; a hanging command may run indefinitely until the user cancels it. On Unix, cancellation terminates the shell process group; independently detached descendants can survive. On Windows, cancellation terminates the job object, which contains every descendant created through the shell.

## Validation

```sh
cargo fmt -- --check
cargo test
cargo clippy -- -D warnings
```
