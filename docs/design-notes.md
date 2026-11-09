> Historical design notes retained for context. Current behavior, commands and
> security guarantees are in [quickstart.md](quickstart.md),
> [reference/cli.md](reference/cli.md) and [security.md](security.md).

# Kroma v0.4 — Lazy single-pass compaction

Minimal AI agent harness written in Rust. This is a **new, independent project**; it does not import code from the previous Kroma.

Kroma supports Chat Completions, Responses and Anthropic Messages; streaming
model/tool execution; a built-in Shell tool with optional MCP tools and
on-demand Agent Skills; saved chat history; and lazy single-pass context compaction.
It has an optional process-plugin host (tools, prompts, context/model strategies, commands, lifecycle hooks). Interactive terminals use
a lightweight Rust TUI by default; scripts and one-shot commands remain CLI. An optional Tauri + Svelte
desktop host lives in `desktop/` and is built by `.github/workflows/desktop.yml`.

## Terminal UI (Ratatui)

### Everyday commands (four-batch core usability pass)

Kroma keeps one Rust Agent runtime. The following shortcuts use existing
Shell, session, model, and event interfaces; none introduces an approval
framework, a second agent loop, or a mandatory plugin dependency.

- **`@path`** refers to a file in the project. Type `@src/age` and press
  **Tab** for path completion; `@src/agent.rs` keeps the path available for
  the agent's Shell tool rather than eagerly putting the entire file into
  model context. Multiple matches are listed. Paths containing whitespace
  currently require another form of reference. File completion reads the
  current path prefix (including a directory you enter); it does not
  recursively index the project, hides dot-prefixed entries, and shows at
  most 16 candidates. The picker's fuzzy filter applies only to those
  already-generated path candidates.
- **`!command`** executes a command directly with the built-in Shell
  executor, without a model call. Output is shown locally, not inserted
  into the model transcript.
- **`/usage`** separates the current session's coding requests, automatic
  session titles, and finalized checkpoint summaries, saved across TUI restarts
  using a small usage sidecar, then shows measured combined
  totals. Unknown upstream usage stays unknown; cache reads are a subset of
  input tokens. Failed checkpoint attempts may not report their cost.
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
  `kroma` opens the branded welcome screen: no new conversation is saved
  until the first message is sent. Existing histories remain accessible.
- **Mouse copy:** drag-select visible conversation text **or text in the
  composer**, then right-click to copy. The input also supports
  **Ctrl+Shift+C / Ctrl+Insert** for selected draft text. Ctrl+C now copies
  an active selection first; otherwise it clears the draft, cancels the task,
  or exits as before. Ctrl+Left/Right navigates by word, while Ctrl+W and
  Alt+Backspace delete a preceding word. Selection preserves CJK, emoji, soft wraps,
  and readable attachment labels. Transcript selection can extend past the
  top or bottom of the history:
  keep holding the left button at the edge to auto-scroll and continue
  selecting across multiple screens. **Hover a hyperlink and Ctrl+left-click**
  to open its target. Both
  explicit Markdown links and bare `https://` URLs are supported. An
  automatically detected local file or explicit local Markdown link is
  clickable only when it exists relative to the active project directory;
  code blocks and tool output are never auto-linkified. Web links use your
  default browser; local paths open with the system default application.
  The hover hint and underline identify valid links; Ctrl+click does not
  start text selection.
  A local desktop uses the system clipboard directly; SSH sessions and
  machines without a local clipboard send OSC 52 to the terminal instead.
  OSC 52 requires compatible terminal settings, so the fallback confirms only
  that the request was sent. Mouse-wheel scrolling stays enabled.
- **Live Shell output** is previewed in the current tool activity while a
  command runs, with full stdout/stderr still stored by the existing Shell
  capture mechanism. The live preview is not sent to the model as extra text.
- **`/rename title`** names the current session; **`/sessions`** opens a
  searchable TUI session picker (the CLI lists names and IDs).
  **`/resume`** without arguments opens the same picker (Up/Down then Enter),
  and **`/resume name-or-id`** resumes a specific session. A new TUI
  conversation is automatically titled with one small tool-free model
  request after its first completed reply. A manual title always wins.
  Session names
  are lightweight metadata; source session files and WAL are unchanged.
- **Ctrl+Z / Ctrl+Y** undo and redo edits to the input box, not project
  files or conversation history. **Ctrl+G** edits the current unsent draft;
  **`/editor`** starts an external-editor draft in
  `$VISUAL` or `$EDITOR`; in-line attachments cannot be edited externally.
- **`/export [filename.md|filename.json]`** exports the current session
  to the project directory; **`--version`** prints the binary version and
  source commit, and **`--build-info`** also prints the executable's actual path.
  Existing exports are never overwritten.

Noninteractive use: **`kroma --print "task"`** streams ordinary plain text;
**`kroma --json "task"`** prints one JSON object per line, with event types
such as `task_started`, `text_delta`, `tool_started`,
`tool_finished`, `model_finished` and `result`. Both accept piped stdin:
`cargo test 2>&1 | kroma --print "analyze failures"`. Large piped inputs
are stored as file references. JSONL uses stdout for events and stderr for
diagnostics. Both modes share Agent, Shell, model providers, and lazy single-pass compaction.

Plugins: see the [plugin guide and concrete examples](../examples/plugins/README.md).
The first practical example is **web search and webpage extraction**:
`kroma plugins install examples/plugins/web`, then `kroma plugins enable web`,
restart Kroma (or `/plugins reload` in an idle TUI) and use `web_search` or `web_fetch` (keyless auto mode by default; no key required, but keyless ranking quality varies).
`/plugins` lists or manages plugins inside CLI/TUI. Further optional examples
include tool/Prompt replacement, model request hooks, context strategy,
subagents, cross-session notes, dynamic TUI panels and plugin-defined shortcuts. Plugins are
disabled by default; Kroma needs Node.js only for JS plugins. User-installed
plugins can be removed with `kroma plugins uninstall <id>` or
`/plugins uninstall <id>`; project-local plugin directories remain source-managed.
For development, `kroma plugins check <dir>` validates the manifest and runs a
bounded process-smoke request. `KROMA_PLUGIN_TRY=<dir>` loads one directory for
this process only without writing activation state; see the plugin guide.
Plugin processes inherit the host environment by default, because the shipped
web example reads provider keys from it. Set `KROMA_PLUGIN_ISOLATE_ENV=1` to
remove credential-shaped variables from plugin children. Non-streaming plugin
tools have no artificial deadline by default; set
`KROMA_PLUGIN_TOOL_TIMEOUT_SECS` to a positive value for automation. A tool may
declare `timeout_secs`, and the smaller of that and the global value wins; the
same total deadline covers streaming tools, and progress frames never extend
it. Hook and ordinary plugin calls retain their 120-second limit, and
cancellation always terminates the plugin process tree.

The **V1 plugin host** exposes tools, Prompt/Hook rewriting, process-backed
model Providers (including streaming tools/reasoning and image-reference
capability declarations), external context strategies, lifecycle events,
private persistent data, commands, and declarative TUI extensions. UI plugins
can add a status/panel, Alt+letter bindings, tool labels, animation frames,
color overrides, and bounded layout adjustments without owning the terminal.
`/plugins reload` atomically replaces safe idle extensions and retains unchanged
tool/command workers. Event subscriptions are recreated on reload so stalled
old handlers can be cancelled independently. Changed model/context backends
and model-request hooks still require restart. Enabled model providers must
initialize successfully and cannot be silently replaced by saved profiles.
External context fork/rewind/undo use cancellable async requests, including
full-copy notifications for out-of-band plugin data. Tools can opt into live
JSONL output progress; plugin settings writes are serialized across Kroma
processes. Session identity is provided to command, model and event plugins.
Failed/cancelled external context work preserves WAL history, while
context-overflow errors can request a single recovery retry and report usage.
Event subscribers have separate bounded queues.

Project plugins under `.kroma/plugins` stay disabled until explicitly enabled
with `kroma plugins enable <id>`. Later source edits take effect at idle reload
without a renewed approval step. Bounded code scans determine worker reuse;
incomplete scans produce diagnostics, not a new approval queue.

The host deliberately does not expose raw terminal widgets, unrestricted
process sandboxing, or an alternate Agent Loop. Official subagents, tournament
judging and memory implementations are separate optional-plugin work, not part
of the V1 host acceptance. Windows verification and cross-platform CI scope
are tracked in [plugin acceptance](../PLUGIN_ROADMAP.md).

Global/project `AGENTS.md` are loaded as before. Nested ancestor files are
loaded when a task references a project path or a tool reveals one. If a model
tool batch newly exposes applicable instructions before execution, that batch
is deferred and returned as not executed so the model can replan. This is
best-effort path discovery, not a shell-command parser; indirect targets still
require the agent to inspect their instructions deliberately.

### Plain visual design

Kroma uses a near-black or silver-white workspace with paper-neutral text and
vivid semantic state colours. Both speakers share one neutral-gray message
surface spanning the transcript width; prose wraps inside that surface instead
of a fixed reading column. The surface stops three cells before the scrollbar,
leaving a visible terminal-background gutter as the right safety margin. User
messages use a violet rail and Kroma replies a cyan rail. One
assistant turn has one `KROMA` label, and its rail continues through all text,
reasoning, tool records and the completion status. The open input dock has one
top divider and preserves the editor/IME geometry.

Dark and light text/status tokens meet 4.5:1 contrast on the base, message and
overlay surfaces; role rails meet 3:1. Selected picker text and tinted diffs
have separate contrast tests. Mono respects the terminal's native
foreground/background. Recent sessions are clickable by their actual ID;
startup still opens home rather than silently resuming.

Every tool invocation keeps an individual record. Successful calls are one
quiet line: semantic marker, effective command segments (shell `cd`/separator
`echo` scaffolding removed) or tool name/arguments. Running, failed and
diagnostic calls add a colored state word and a bounded output preview. Click
the state marker (or use **Ctrl+O** for all calls) to expand that call's exact
full Command/Output,
while failure and unknown records also expose **[view]** for a read-only
snapshot. **Ctrl+O**
expands all details, **Ctrl+T** retains searchable full history. Inspecting a
call never reruns it. Normal answers end quietly; execution records and failed
or cancelled tasks retain their relevant status and real available duration.

Code labels copy original fenced source without gutters, soft wraps or
expanded tabs. The dependency-free highlighter supports Rust, Python,
JS/TS/JSX/TSX, JSON, TOML, YAML, Bash, PowerShell and diffs. Existing theme TOML,
plugin presentation slots and shortcuts remain compatible; no dependency,
model request, prompt, tool schema or conversation format was added.

Run `kroma` (or `cargo run --release`) in a real terminal to open the TUI.
`kroma --no-tui` opens the original line-oriented CLI. `kroma "task"` keeps
the one-shot interface, and redirected/non-TTY input does not activate TUI.
The UI reuses the same Agent Loop, Shell/MCP tools and session WAL, with no
RPC server or UI plugin runtime. Ratatui 0.29 / Crossterm 0.28 preserve the
project's Rust 1.85 toolchain; Cargo.lock pins the compatible dependency set.

**A / Plain is now the default:** run `kroma` directly for the neutral,
single-column interface with an open composer and **S2 semantic colours**. No preview launcher or
`KROMA_TUI_STYLE` environment variable is required. Existing conversations,
model settings and task execution semantics are unchanged. See
[the style notes](../reviews/tui-plain-study/README.md).

The default interface supports streamed assistant text, formatted Markdown,
compact Shell/MCP tool calls with expandable results (failed tools show the
reason without expanding), provider-exposed streaming reasoning collapsed by
default, conversation scrolling, persisted
sessions and the existing slash commands. Shortcuts:

- **Enter** sends; **Shift+Enter**, **Alt+Enter** or **Ctrl+J** insert a newline.
  Prefer **Ctrl+J** when the terminal cannot distinguish Shift+Enter from
  Enter. Multi-line drafts show their line count and newline/send hint.
  Bracketed multi-line paste is supported where the terminal/backend emits
  paste events; Up/Down navigate input and history.
- **Draft selection:** Shift+arrows/Home/End selects text;
  Ctrl+Shift+Home/End selects to the beginning/end of the entire draft, even
  beyond the visible input area. Typing, deleting or pasting replaces the
  selection as one undoable edit. Copy preserves original tabs, blank lines
  and Unicode; tabs display as four spaces without changing the draft.
- **Text paste:** ordinary multi-line text up to **4 KiB of UTF-8** remains
  editable, regardless of its line count. Larger text retains the compact
  attachment path. Ctrl+V / Shift+Insert reads the local text clipboard only
  if the terminal forwards those keys. Over SSH, use the terminal's own
  paste action; it must supply bracketed paste events. For local Windows
  terminals that split pasted text into key events, use **Alt+V** while idle
  to read the clipboard in one operation, or **Ctrl+G** for an external editor.
- **PgUp/PgDn** or mouse wheel scroll conversation; scrolling up pins the
  reading position even as the model streams, and **Ctrl+End** returns to the
  latest output. **Alt+↑ / Alt+↓** jumps between user turns, including after
  resizing; the history hint reports the selected turn. **Ctrl+O** expands
  tool outputs; **Ctrl+R** shows reasoning.
- **Scroll bar:** drag the thin thumb at the right edge of the conversation
  to move directly through history. Its track is proportional to the transcript
  and loads older history when necessary. Ordinary text dragging keeps
  selection active while the viewport moves.
- **Command index:** type `/` or run `/commands` to see commands with descriptions and argument hints. Up/Down selects; **Tab** (or Enter in this completion list) inserts the command without executing it. Type arguments, then Enter to run; Esc closes the list without clearing the draft.
- **Interface language:** `/language zh` switches UI chrome, hints, help and command descriptions to Chinese; `/language en` switches to English; `/language` toggles. Model answers, tool output and upstream diagnostics are not translated. The default is English.
- **Reading layout:** Plain always uses the available workspace width for prose, code, tables and tools. The obsolete `/layout` width toggle has been removed; legacy `full_width` and `display` fields are ignored when reading `~/.kroma/ui.toml`. Interface language remains saved there.
- **Tool records:** every invocation has its own record. Adjacent calls are
  never merged into a group or a hidden-step count. Successful calls are one
  quiet line with the semantic marker and effective command segments; generic
  categories and "Done" are not repeated for every call, and shell `cd`/echo
  scaffolding is removed from that line but kept when expanded. Successful
  records carry no disclosure badge; the command is clipped before the right
  safety margin so the tool block keeps a clean edge. Active, failed and
  diagnostic records add a colored state word, meaningful elapsed time and a
  bounded preview. Click the state marker to disclose that call's full
  Command/Output; **Ctrl+O** discloses all details. Long
  output folds inside that call only; raw results remain unchanged. Terminal
  escape sequences are stripped for display, and remaining control bytes are
  shown as visible placeholders rather than emitted to the terminal. Reasoning remains one folded notice per turn and
  does not split records. Recovered interrupted tools use ⊘ rather than the
  known-failure ✕; symbols and diagnostic text distinguish outcomes without
  relying on colour alone. Cancellation appears once in the completion row;
  saved model-facing notices are localized for display without changing the WAL.
- **Task status:** a quiet S2 state prefix follows the latest transcript entry while Kroma is thinking, using tools or streaming output. The symbol and short state word use a muted semantic colour; commands, logs and elapsed time remain neutral. Execution/failure/cancellation and available elapsed time appear at the end of that turn, not in a floating status bar. Ordinary answers need no completion banner. When browsing older history, a separate navigation hint gives the return-to-latest shortcut and unseen update count without moving the reader.
- **Ctrl+P** opens the searchable picker; **Tab** changes its category among
  commands, model profiles, sessions, and files. Type to filter, Up/Down to
  select, Enter to confirm, Esc to dismiss without deleting an unsent draft.
  The **`/model`**, **`/sessions`**, and **`/resume`** commands open
  their respective pickers;
  typing **`@path`** and pressing Tab opens file choices when several match.
- Input editing respects complete Unicode grapheme clusters (including joined
  Emoji and combining marks), while inline attachment chips remain atomic.
- Older conversation history is rendered on demand as you scroll upward.
  Completed text layout is cached between unchanged frames; only visible rows
  are copied to the terminal. Plain usage updates do not reflow the history.
- **F2** cycles dark/light/mono themes; `/theme dark`, `/theme light`,
  `/theme mono` select a theme immediately, and `/theme reload` rereads the
  on-disk theme. `/help` shows the TUI command summary.
- The single-row footer puts **model** and **thinking level** on the left,
  **Context used/capacity** and the latest foreground **cache hit rate** on the
  right. Long names and labels shorten before values are lost. Context uses the
  total model window; only the reading uses warning colour at 70–89% and error
  colour at 90% or above. Cache, model and thinking metadata stay neutral.
  A leading ~ is an estimate and — means unknown, never a fabricated zero.
  **/usage** retains full token, cache and plugin-status details; background
  titles and summaries cannot overwrite coding metrics. **Shift+Tab** cycles
  thinking, or use /think high.
- Model output streams as it arrives, with a subtle cursor and a quiet
  activity indicator at the end of the transcript while the model is working.
  A connecting/waiting status after a user turn starts its own assistant row,
  separated by one terminal-background line and carrying the cyan Kroma rail;
  activity after assistant content stays attached to that turn.
  Reasoning stays collapsed with one summary per user turn rather than one
  repeated banner after every tool call; Ctrl+R shows every reasoning segment.
  Reasoning interleaved with a tool group is collected immediately before its card. Completion reconciles the entry without duplication.
  Providers exposing only final reasoning show it after completion.
  Encrypted or otherwise unavailable private reasoning is never displayed.
- **Esc** dismisses the file-path prompt or transient feedback; it never
  silently deletes the current draft. Short notices disappear on the next
  interaction; task errors also appear in the scrollable conversation.
- **Ctrl+C** cancels an active task. While idle it clears the draft (including
  pending attachments); when the input is already empty it only shows a notice.
  While a task runs, submitted text steers it,
  `/steer text` explicitly steers, and `/queue task` adds a follow-up. Only the
  terminal host runs queued follow-ups; the desktop host reports that it has no
  queue consumer instead of accepting a task it would never start.
  **Ctrl+Q** quits when idle; `/exit` is also supported.
- **Session chrome:** no persistent conversation heading or nested message
  boxes. Both user messages and Kroma replies share the same slightly raised
  neutral-gray surface across the transcript width, ending three cells before
  the scrollbar to leave a visible terminal-background gutter. User messages
  keep one blank padding row above and below and use a full-height violet rail. One Kroma turn has one cyan rail and one `KROMA` label, continuing
  through wrapped text, paragraph gaps, code, reasoning, tool records and its
  completion status. A terminal-background row separates consecutive turns, so
  a new user message cannot look like part of the previous Kroma surface. Role
  labels use the same identity colour but remain separate from execution-state
  colours. Session identity stays in the terminal title. The open composer has one divider and preserves the
  editor/IME geometry. Empty input has no placeholder text. Home keeps a simple
  wordmark, working directory and up to three recent sessions when height permits.
  Command pickers and plugin panels use a distinct neutral overlay surface.
- **Git observations:** branch/dirty status and successful per-turn change
  summaries run asynchronously outside rendering, with 1.5-second command
  deadlines. A missing baseline, timeout or non-repository means no summary,
  not fabricated zeroes. Summaries compare numstat counts and newly untracked
  names, not a semantic attribution of external edits; they are not persisted.
- **明确不做 / Non-goals:** no approval UI, sidebars, default dual-pane IDE,
  full-screen card wall, complex animation framework, extra UI runtime or
  automatic file rollback. Existing /fork and /rewind change conversations,
  not project files.
- **Alt+V** pastes a desktop clipboard image or text. When the clipboard is
  unavailable, a separate path prompt accepts a file accessible to Kroma
  without destroying the unfinished message. Alt+V uses the same text-size
  policy as ordinary terminal paste: up to 4 KiB stays editable; text over
  4 KiB becomes an inline attachment,
  e.g. `你看看【ui.png】怎么样，分析一下【6000字符】`. The chip is one
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
  Activation follows each attachment's occurrence in the conversation, not
  its file path; recalling an image does not reactivate every older copy.
  Within a task, immutable image encodings are kept in temporary memory and
  reused between tool rounds, then dropped. This avoids repeated file reads
  and base64 encoding, but does not change the images sent or billed tokens.
  Up-arrow input recall preserves the original media references; removing
  a recalled chip or clearing the draft never deletes a previously submitted
  image. While an Agent task is running, new media/large-log pastes are
  refused with a visible notice instead of being silently ignored; the
  existing draft and selection are preserved. Small terminal/text-clipboard
  pastes can still edit a follow-up draft, but Alt+V remains idle-only.
  When the upstream
  declares the selected model text-only, even active images are replaced by
  explicit image-omission references instead of sending unsupported media.
  If capability metadata is unavailable, support is unknown. When the
  upstream explicitly rejects image input before streaming, Kroma retries
  once with text image references instead of guessing model support from
  its name. A different provider error is returned unchanged.
  Small pasted text (up to 4 KiB) reaches the active model inline; large
  logs and other files remain Shell-readable references. Inline short-log
  contents, including escaped JSON characters, count toward both the
  compaction planner and final request budget before sending. Each item stays
  in its exact position relative to typed text, across Chat Completions,
  Responses and Anthropic requests. Current images have a separate
  approximate input-budget allowance and 16 MiB combined wire-size cap;
  image tokens remain provider-specific rather than exactly predictable.
  Image-heavy requests are charged before the final request budget check and do not silently inflate text-only estimates.
  Existing saved media files remain available to other sessions and branches;
  Kroma does not guess that a media file is unreferenced and delete it.
  Sending an image never makes a text-only model visual.
  A terminal/SSH host does not automatically share another device's binary
  clipboard data; this is generic desktop clipboard support, not a custom
  upload or remote clipboard service.

The TUI `Context` status shows the latest request's input against the total
context window (e.g. `Context 100k/500k`), not cumulative session usage. `/usage`
retains the detailed measurements and separate request totals. Estimates carry a `~`
marker; unknown usage is shown as `—` or `unreported`, never zero. Output reserves
still reduce the internal input/compaction budget, but do not reduce the
displayed window or percentage base.

The first manual theme change writes `~/.kroma/theme.toml`; merely starting
Plain does not create or rewrite it. The `dark` preset is near-black grayscale,
`light` is silver-white grayscale, and `mono` respects native terminal colours.
`/theme reload` rereads the selected preset. Former per-colour overrides and
plugin colours remain stored for compatibility but are not applied by Plain,
so old accent settings cannot recolour the base interface. S2 uses vivid,
high-contrast blue for active state, green for confirmed success, amber for
warnings/unknown results and red for failures. Links turn blue only on hover; diff colour is
limited to addition/removal markers, not entire code lines. Native `mono`
keeps terminal colours and all state symbols/words. See the
[semantic-colour specification](../reviews/tui-semantic-colors/README.md).

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
reverted. Use Git for source changes. A branch or rewind drops summaries and
tool-preview indexes that describe discarded messages, and keeps the confirmed
"already answered" prefix that survived, so a long branch can still checkpoint
instead of failing on its context limit. This keeps session history readable without a
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
Host secret variables (`*_KEY`, `*_TOKEN`, `*_SECRET`, `*_PASSWORD`, cloud and
provider credentials, database passwords, cookies, and `KROMA_*`) are stripped
from local stdio children. Project configs are disabled until approved with
`kroma trust-project`; approval is bound to the project path, the exact config
bytes, expanded `command`/`args`/`env` values, a project-contained working
directory, the bytes of the local entry files and (within a bounded 256-file /
32 MiB scope) the JS/TS modules they reach through explicit relative imports.
Every referenced module is read and scanned in full, so a relative import
written after a large prefix is covered too.
Editing any of those files requires approval again;
unreferenced `node_modules` trees are not walked. This format changed in v0.4,
so re-run `kroma trust-project` after upgrading. Approval and revoke operations
share a cross-process lock, so concurrent writers cannot lose an update or
revive a revoked project. Config reads stop one byte past the 1 MiB limit, so
an oversized untrusted project file cannot allocate its full size before it is
rejected. Project configs may only expand non-secret `MCP_*` and basic path
environment variables; credential-shaped names are rejected. Set
`KROMA_TRUST_PROJECT_MCP=1` to approve for one run.

The default MCP protocol version is `2025-11-25` and uses the standard
initialize/initialized handshake. To use the newer stateless MCP API, set
`"protocolVersion":"2026-07-28"` on that server. Stdio and Streamable HTTP
are supported, including JSON/SSE responses, tool-list pagination and tool
calls. Tools are named `mcp__<server>__<tool>`; a server that fails to start
or list its tools is skipped with a warning rather than disabling Shell.
MCP control/startup requests have a 120-second timeout. Tool calls have no
arbitrary hard deadline and run until the Agent task is cancelled; configs,
stdio/SSE frames, HTTP bodies, tool results and images have explicit size limits.
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
and a single-line `description` (no control characters), and is limited to
1 MiB. Kroma inserts **only** this metadata into its system prompt and
registers a `load_skill` tool. The model should invoke the tool
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
provide generic tool-free text generation rather than checkpoint-specific prompts;
and tool/model completion events occur after the corresponding WAL entry.
Custom context state can be explicitly reset with
`kroma recover-context [session-id]` when the custom implementation is no
longer installed; this **retains full raw history**, but resets derived
summaries. See `benchmarks/P12_ARCHITECTURE_REVIEW_REPAIRS.md`.

Kroma now exposes a Rust library as well as its original CLI. The default
composition registers Shell plus history/skills and lazy MCP tools. Chat,
Responses, Anthropic and optional process-backed models use the same Agent,
request projection and lazy single-pass compaction. CLI/TUI and Desktop are
hosts of this runtime, not separate execution loops.

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

These are **Rust integration interfaces**, separate from the optional JSONL
process-plugin protocol. They do not provide a generic tool permission layer
or a service container. Project MCP/plugin activation has its own content-bound
approval boundary; that boundary is not a process sandbox. See `benchmarks/P11_ARCHITECTURE_FOUNDATION_REPORT.md` for
invariants, isolated replacement tests and regression commands.

## Core-efficiency measurements and opt-in profiling

Set `KROMA_PROFILE=1` when running a real task to print lightweight
`[profile] model_start/model_done/tool_start/tool_done` phase markers, plus a
task-total `[profile]` line at completion, failure or cancellation. Phase markers help
identify the last active phase if a long task is interrupted. The total includes wall time,
context preparation (including awaited compaction requests), file snapshots
(`snapshot_ms`), model-response time, tool execution time, session journal writes, checkpoint finalization,
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
Current offline microbenchmarks and the paired checkpoint-planner ablation are
documented in [performance](performance.md). They measure local CPU/filesystem
work or deterministic fake requests, not real provider latency or billed costs.
The TRC/APEC E1 comparisons in
[the archived E1 report](../reviews/KROMA-E1-CORE-EFFICIENCY-2026-10-26.md)
describe an earlier context implementation; they are not measurements of the
current lazy single-pass policy. In P10 the standalone, zero-hit first-use
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

Kroma also ships an audited built-in catalog for exact public
`(provider, model-id)` pairs. It fills fields the endpoint did not provide,
using `explicit configuration → current endpoint metadata → catalog → conservative default`.
Only the official HTTPS endpoint on its default port is recognized as the
provider; a custom proxy does **not** inherit capabilities from a same-named official
model. To opt in explicitly, set `KROMA_MODEL_CATALOG_MODEL` or the profile's
`catalog_model` to an exact alias such as `anthropic/claude-sonnet-5-5`.
The [catalog snapshot and official sources](model-catalog.md) cover 110 exact API IDs
across 18 providers (verified 2026-11-04), including protocol, availability
and lifecycle caveats. Unknown aliases fail at startup. Every catalog record carries its source URL
and verification date; unknown image/thinking capability stays `unknown`, not
`false`.

Explicit context settings are retained when smaller than a known endpoint or
catalog maximum; a larger value is capped by that hard maximum and the report
shows the cap. Catalog maximum output is a capability, not a request: Kroma
still sends no explicit Chat output cap unless configured, and its per-turn
request stays at most 32k. `kroma config show` displays configured/catalog
effective fields without contacting the network; `/model-status` shows the
runtime report after startup discovery. The same report is exposed in the
desktop snapshot and updated atomically with a model switch.

`KROMA_CONTEXT_WINDOW` overrides the model context limit,
`KROMA_MAX_OUTPUT_TOKENS` controls requested output tokens, and
`KROMA_OUTPUT_RESERVE_TOKENS` overrides output reservation for context planning.
The effective requested maximum is capped by a discovered upstream maximum.
Checkpoint generation also respects the known provider output ceiling.
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
checkpoint summary on both wire formats. They do **not** prove compatibility
with a real provider or any particular proxy. Run live integration tests
before advertising production-level support.

- Streaming `reasoning_content` deltas are reassembled into a **separate** assistant-message field, persisted in snapshots and WAL, and replayed with the corresponding tool calls and results. The classic line-oriented CLI outputs only `content`; the TUI may show the persisted reasoning when the user presses Ctrl+R. Saved sessions without the new optional field continue to load.
- With the `tools` parameter present, DeepSeek thinking mode requires the reasoning field on previous assistant messages. Kroma leaves it intact in un-compacted history, including across restart and `/resume`. Compaction replaces entire old **completed** turns with one checkpoint summary; it never splits a tool call from its results. The raw transcript, including reasoning content, remains on disk. Summaries do not reproduce reasoning verbatim.
- OpenAI-compatible tool requests now omit the redundant `tool_choice: auto` parameter (DeepSeek V4 thinking can reject it). Older assistant tool-call messages with `content: null` are projected as `content: ""` for provider requests without altering saved raw records.
- **Opt-in DeepSeek V4 thinking:** set `KROMA_DEEPSEEK_THINKING=enabled` to send `thinking: {"type":"enabled"}`, or set `high`/`max` to additionally send `reasoning_effort`. Leave it unset for providers or gateways that do not accept these fields. This does not auto-detect models or force thinking on; some reverse proxies implement different semantics.

## Local diagnostics and lifecycle

`kroma diagnostics [--output <new-directory>]` writes a local-only,
allowlisted bundle (`summary.json` and `README.txt`). It includes version,
build revision, executable path, OS/architecture, model limit sources,
plugin enabled/protocol state, and bounded errors. It does **not** include API
keys, Authorization/Cookie headers, environment values, plugin settings,
conversation messages, tool inputs/outputs, or source files. It does not run
credential commands, start plugins, request a model, use the network, or
overwrite an existing target. Paths under HOME/USERPROFILE are redacted; other
paths may still identify a machine. Session excerpts are not supported in this
format. The command is explicit; there is no background upload or telemetry.

The foreground state machine and durability boundaries for runtime events are
documented in [docs/lifecycle.md](lifecycle.md). `TaskFinished` means the
foreground host may accept another operation; no background checkpoint work
exists.

## P2: Lean modules and multiple conversations

The runtime is split by responsibility: `provider.rs` (HTTP/SSE and API request mapping), `protocol.rs` (shared messages), `context.rs` (lazy single-pass compaction), `tools.rs` (tool dispatch/schema), `shell.rs` (process execution), `session.rs` (checkpoint/WAL/lock), `sessions.rs` (session directory and IDs), and `agent.rs` (foreground orchestration). Optional process plugins are hosted at the application boundary; there is no new provider registry or database.

Interactive commands:

- In the TUI, `/new` returns to the Logo welcome page and the next real
  message creates an independent snapshot, journal, and compression state;
  no empty conversations are archived. The CLI `/new` retains its original
  immediate creation behavior.
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

Checkpoint requests keep the final factual summary concise, but leave
additional completion allowance for reasoning-capable models. A truncated
summary (`finish_reason=length`) is never accepted as memory: Kroma retries
with more completion headroom and reports an explicit failure if all retries
are exhausted. A failed checkpoint is not repeated on every tool round in the
same user turn; a subsequent user turn may retry. Full original conversation
history is always retained. This recovery path was validated against a real
saved multi-stage development session; see `benchmarks/P3_REVIEW.md`.

## P4: Shell usability and real task comparison

On Unix Kroma uses Bash when `/bin/bash` is installed, and falls back to POSIX
`sh` otherwise. On Windows it prefers Git Bash (`Git\bin\bash.exe` or
`Git\usr\bin\bash.exe`, also detected under `%LOCALAPPDATA%\Programs\Git` and on
`PATH`), then native PowerShell 7 or Windows PowerShell 5.1, and finally
`cmd /C` only if neither is available; the WSL `bash.exe` stub in `System32`
is not selected. `KROMA_SHELL` can force a `bash.exe` path, `powershell.exe`,
`pwsh.exe`, or `cmd`. Each shell has its own command syntax, which Kroma
explains to the model in the tool description. PowerShell runs with no profile
and UTF-8 output; native process failures retain their exit code.
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
consumed. Temporary output captures
from cancelled/failed commands are removed. New captures belong to their
specific session; Kroma cleans up that session's unreferenced captures after
24 hours and `/clear` removes its captured outputs. There is no blanket
expiration for referenced outputs. Context projection uses a forward tool-ID
map instead of repeated backward scans. No Shell permissions or API key
inheritance changed, and the same compression scheme remains in place.

## P8: P7-winning F execution guidance, serial tool execution

P7 tested 72 isolated coding stages across A/B/F. F reduced total foreground
requests, shell calls, and combined foreground+checkpoint uncached tokens, but
three F integration-review stages and one B stage failed to *finish* because
model streams timed out; all 72 stages passed independent code acceptance.
See `benchmarks/P7_FINAL_REPORT.md` for the full results and caveats.
The production system hint now tells the agent to edit once sufficient
evidence is found rather than repeatedly re-inspecting unchanged files.

Kroma encourages the model to request **multiple independent shell inspections in one assistant response** when useful, rather than unnecessarily making a separate model request for each inspection. Dependent work, edits, and verification still happen in the order they require. This is a short, stable system-prompt hint applied to the outbound request view, **not** an extra model call or a new tool.

If the model returns multiple tool calls together, Kroma executes them **strictly in response order**, persists each raw result with its corresponding `tool_call_id`, and sends all results back together in the next model request. No tool execution is parallelized. Normal Kroma startup uses this policy automatically; no environment variable or mode selection is needed.

The previous experimental `KROMA_TOOL_EXPERIMENT` selector and parallel-read dispatcher were removed from production code. Historical `benchmarks/tool_batch*.py` files document exploratory experiments and **must not be used as live B/C selectors** on this single-policy build.

## v0.4: Lazy single-pass compaction

### Tool-output preservation (P10 simplification)

There is no longer an extra command-type classifier or duplicate-line filter
before the first model read. Ordinary short Shell output is delivered intact.
Large streams stay fully available in referenced files, and hard-limit context
projections keep the original message in the saved history. This removes a
special-case path which showed no compressible results in the sampled real
Rust and Python task histories.

Kroma uses **one built-in context strategy: lazy single-pass compaction**.
There is no runtime strategy selector. The retired `KROMA_FUSION_EXPERIMENT`
setting is ignored; it cannot enable any other behavior. Historical scripts
under `benchmarks/` are experimental records and may not match the current
single-policy build.

Normal requests keep the complete projected conversation. There is no
background pre-summary, no routine rewriting of already-consumed tool output,
and no cache-benefit calculation that silently changes the prompt prefix. Large
Shell streams remain file-backed from the first tool result; the complete raw
transcript is always retained and remains available through `history_search`
and `history_read`.

When the provider-reported projected input reaches **90% of the full model
window**, Kroma sends one checkpoint request using the same model, endpoint,
tool definitions and history prefix, with only a final checkpoint instruction
appended. The trigger is capped below the real input capacity for small
windows: a small estimator-error margin is reserved, so 90% is never forced
when the output reserve leaves less room. The checkpoint replaces older
projected history with a structured continuation summary and keeps roughly the
newest 32k tokens raw (scaled down for small windows). A commit requires a
non-empty, plausibly complete, shorter and actually reducing summary;
otherwise the original history is preserved. At most one checkpoint is
committed per `prepare()`, and a failed attempt is retried only on a later
user turn. If one tool result is too large at the hard limit, request-only
previews point to the complete original on disk.

Use **`/compact`** while idle to request a checkpoint immediately, without
waiting for the automatic threshold. It takes no arguments, does not add a
user turn or execute tools, and reports estimated context tokens before and
after compaction. Completed-turn boundaries and recent-history retention
still apply; an empty or too-small history returns “Nothing to compact”
without a model request. The original transcript remains intact. The TUI
shows compaction progress; **Esc/Ctrl+C** cancels it. The complete checkpoint
operation has a **1,440-second outer limit**: the initial checkpoint attempt
plus up to three timeout retries, each bounded by a **360-second per-attempt
deadline**. Session-title requests remain short and keep their 120-second
deadline.
Failures, timeouts and cancellations before commit leave the existing context
unchanged. External context policies must explicitly implement manual
compaction; unsupported policies return an error instead of silently clearing
context.

For the default policy the limits are:

```
margin      = max(window / 100, 1024), capped at available_input / 8
safe_input  = available_input - margin
soft_limit  = min(window * 90 / 100, safe_input - margin)
hard_limit  = safe_input
available_input = window - output_reserve
```

For a 500k window with a 32768-token output reserve this is
`available_input=467232`, `soft_limit=450000`, `hard_limit=462232`.
Every request ceiling plus its output reserve remains inside the model window.

### Review fixes

- Generated checkpoints are supplied as untrusted assistant-role history, never as additional system instructions or a new user task.
- Session JSON has an adjacent process-lifetime lock file to prevent concurrent Kroma writers on Unix/Windows.
- Shell stdout/stderr is written directly to private per-workspace capture files. Outputs up to 64 KiB per stream are returned inline; larger streams return a short preview and an absolute path to the complete file. This avoids unlimited output buffering and avoids silently throwing away bytes.
- Nonstandard model `finish_reason` values are treated as errors. Checkpoint cost is reported separately from primary model traffic.
- **Known tradeoffs:** journal entries are synchronized for durability, and periodic checkpoints still rewrite the full JSON history; a very long single session may still incur occasional large writes. A checkpoint adds one summarization request near the input limit. Summaries remain lossy; source history is authoritative. Token water marks use provider-reported input when the measured prefix is unchanged, with a conservative estimate for appended messages. That measurement is stored in the compaction state as counters and hashes only, so reopening a session (or rebuilding the backend for the same endpoint) keeps calibrating instead of dropping to a byte estimate; it is revalidated against the live transcript, dropped by any checkpoint, and the status line is planned from the same basis.
- **Persistence:** The complete raw history and compaction state are recovered from the session checkpoint plus its append journal. Older v0.3/v0.4 histories and legacy cleared-tool projections remain readable and are migrated in the projected view without rewriting the raw transcript.
- **Settings:** `KROMA_CONTEXT_WINDOW` (default 128000) should reflect the actual provider context limit. `KROMA_OUTPUT_RESERVE_TOKENS` optionally overrides the output reservation (default: min(4096, 1/8 of the context window)). A single turn requests at most 32k output tokens even when the profile allows more, so a 128k capability is not reserved on every request. Token size is estimated heuristically when no matching provider measurement is available; estimates are **not** exact tokenization. Content that is still available is counted by character class (ASCII at 3.5 bytes per token, about one token per CJK character), a length-only floor of three bytes per token covers callers that only kept a byte count, and a residual 10% margin extends a measured prefix. The rates come from 43 measured request deltas in real sessions: the class-aware base lands at 1.02-1.58x the provider count (median 1.21x) and the measured-suffix path at 1.13-1.74x (median 1.33x), with no underestimation on any measured payload. Lowering them further would start underestimating dense tool JSON, which is the direction that gets requests rejected.
- **Limitations:** A checkpoint is a separate non-streaming model request and therefore consumes additional tokens. It must be supported by the chosen provider/model. `history_search` and `history_read` let the model inspect the complete saved transcript after the projection has been compacted.

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

- `context_window` supplies context planning when the endpoint does not; the
  `KROMA_CONTEXT_WINDOW` environment override still wins. A value larger than a
  known endpoint/catalog hard maximum is capped, and the cap is shown in the
  source report.
- `max_output_tokens` sets the requested completion maximum, but Kroma caps a
  single turn at 32k so a 128k capability is not reserved on every request; the
  `KROMA_MAX_OUTPUT_TOKENS` environment override still wins, and an explicitly
  discovered upstream maximum still caps the request.
- `catalog_model` is an optional explicit `provider/model-id` alias for a proxy
  or renamed model, for example `anthropic/claude-sonnet-5-5`. It is never
  inferred from a same-named model ID; an unknown alias fails validation. The
  equivalent environment override is `KROMA_MODEL_CATALOG_MODEL`.
- `thinking_levels` restricts the tiers the model accepts, in display order.
  Shift+Tab cycles only those levels, and an unsupported level is refused
  instead of being sent. `auto` stays available as "provider default", and the
  profile's `reasoning_effort` default must be one of the listed levels unless
  it is `auto`. Omit the field to allow every Kroma level.
- All three fields are optional; profiles saved before they existed keep the
  previous behavior. `context_window` must be 1024–2000000 and
  `max_output_tokens` must be smaller than `context_window`.

Profiles are stored in `~/.kroma/config.json` (not the **old Kroma**'s
`~/.kroma/config.toml`). New files are created privately. On Unix the loader
warns when an existing profile file is readable by other users, but does not
silently move or delete it. No credentials are printed by `config list` or
`config show`.

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

`KROMA_WORKDIR` selects the starting working directory for shell commands, defaulting to the directory from which Kroma starts. `KROMA_SHELL` selects the shell on Windows (`bash.exe`, `powershell.exe`, `pwsh.exe` or `cmd`). `KROMA_API_KEY` can be omitted for gateways that do not require a key.

## Streaming and saved chat

- Responses are displayed as text arrives rather than waiting for the entire model response. Tool calls are assembled from streaming fragments before execution.
- The complete conversation (including tool calls and results) is stored in
  `~/.kroma/sessions/` JSON and WAL files, separate from the source repository.
  Plain TUI `kroma` now starts a new welcome page instead of loading old
  messages automatically; use `/resume` or `kroma --continue` to restore them.
- To pick another default history file, set `KROMA_SESSION_FILE=/path/to/conversation.json`. New sessions are stored alongside that file; `/clear` replaces only the active saved conversation with a fresh one.
- On Unix, newly created history files are private (`0600`). If the process dies during a tool call, Kroma records the missing result as unknown on reopening, without automatically repeating a potentially destructive command.
- History is stored as JSON checkpoint plus a checksummed, append-only journal without pruning. The checkpoint may lag the latest turn until the next compaction checkpoint. Only one Kroma process may open a given session at a time (an adjacent lock file enforces this). Session metadata (model choice, names, most recent session) is updated under an adjacent cross-process lock, so concurrent writers cannot lose an update. Profile edits are applied through a cross-process locked read-modify-write (`Profiles::update`), so two Kroma processes changing different profiles cannot overwrite each other. Session checkpoints, profile files and session metadata are replaced through `src/atomic_file.rs`: `rename` on Unix and `MoveFileExW(MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH)` on Windows, since a plain `rename` fails on Windows when the target exists.

## Execution contract

1. A user message is added to the conversation.
2. Kroma calls `POST {KROMA_BASE_URL}/chat/completions` with the message history, `stream: true`, and the `shell` tool schema.
3. If the model returns `tool_calls`, Kroma executes each `shell(command)` call in order and appends the result with its `tool_call_id`.
4. Kroma sends the updated history to the model, repeating until it receives a normal assistant response. There is no artificial Agent Loop step limit.

Each shell result reports `exit_code`, `stdout`, and `stderr`. Shell commands have no artificial timeout or output limit by default; `KROMA_SHELL_TIMEOUT_SECS` sets an optional per-command wall-clock deadline for unattended automation (`0` or unset means unlimited). Failed commands are reported back to the model, which decides whether to retry or correct the task. Streaming model HTTP requests have a 20-second connection-establishment timeout and a 300-second idle timeout (for response headers or a gap between streamed network chunks), **not** a 120-second total-request deadline. Before the response body carries any protocol event, transient transport failures (timeouts, refused or reset connections, and a body that ends before its declared length) and transient HTTP 429/502/503/504 may be retried up to 3 additional times. A body that delivered only SSE keepalive comments has not started a response and may still be retried. Numeric `Retry-After` values are honored up to 10 seconds; longer or unparseable server wait instructions are not silently overridden. Permanent HTTP failures (such as 401), TLS verification errors, malformed payloads and internal size limits return immediately. Once any `data:` event arrives, including a start event that may carry billed usage, the request is never replayed, so duplicate output, tool execution and double billing stay impossible. Provider-reported usage survives an SSE error event or a connection failure after `message_start`. Separate non-streaming checkpoint requests keep a 360-second per-attempt deadline with up to three timeout retries, bounded by a 1,440-second overall compaction deadline, and honor the same catalog output cap as coding requests. Task summaries now include a `task_status` marker on completion, failure and Ctrl+C cancellation; incomplete provider usage remains unknown rather than zero.

**Security:** `shell` executes arbitrary commands with the permissions of the Kroma process. The working directory is **not** a sandbox. Use a container or a restricted account for untrusted tasks, and never run this prototype with elevated privileges against important data. Very large command output is truncated inline and written to private capture files, so it can still consume disk; a hanging command may run indefinitely until the user cancels it or an optional `KROMA_SHELL_TIMEOUT_SECS` deadline expires. On Unix, cancellation terminates the shell process group; independently detached descendants can survive. On Windows, cancellation terminates the job object, which contains every descendant created through the shell. A successfully completed command may leave background descendants running; Kroma retains their job handle until Kroma exits, then the OS closes it and terminates them. Force-killing Kroma also terminates contained descendants.

## Validation

```sh
cargo fmt -- --check
cargo test
cargo clippy -- -D warnings
```

## License

MIT. See [LICENSE](../LICENSE).

Current module ownership and safe extension points are summarized in
[architecture](architecture.md); historical phase reports above are not a promise
that every measured optimization is still present.
