# Kroma plugins — Process plugin API (V1 + V2 slices)

Kroma runs normally without plugins. Plugins are local directories with a
`plugin.toml`, and **installed plugins are disabled by default**. Enabling a
plugin takes effect after restart or with `/plugins reload` while the TUI is idle. Unchanged model/context strategies and model-request Hooks can stay active during unrelated plugin reloads; actual changes to them still require restarting Kroma.

The plugin process can be written in any language. The bundled examples use
Node.js 20+ without external npm dependencies. JavaScript is not required to
run Kroma without these plugins.

## Get started: internet search and webpage extraction

From the repository directory (on Windows, substitute
`.\target\release\kroma.exe` for `kroma` if it is not installed globally):

```powershell
kroma plugins install .\examples\plugins\web
kroma plugins enable web
$env:EXA_API_KEY = "your-key"
kroma plugins set web provider exa
kroma plugins list
kroma
```

Then ask Kroma to **search for a topic and read the relevant result pages**.
The model receives two new tools:

- `web_search({ "query": "Rust language", "count": 5 })`: titles, URLs and
  snippets. Supports Exa, Brave, Tavily, You.com, Parallel, Firecrawl or SearXNG.
  Default `auto` follows **Parallel → You.com → Exa → Tavily**, skipping missing
  keys. Only network errors, timeouts, HTTP 429 or 5xx advance to the next
  configured provider, within one total deadline. Explicit providers never switch.
  `site:domain` and `-site:domain` restrictions are checked against returned URLs.
  Optional `provider` selects another backend for a single call.
- `web_fetch({ "url": "https://www.rust-lang.org/", "max_chars": 12000 })`:
  page title and structured text (headings, code, tables and links), plus
  `next_start` for continuing a long page with `start_char` and the same URL.

Search backend alternatives (choose one; explicit providers never switch):

```powershell
# Brave instead of Exa
$env:BRAVE_SEARCH_API_KEY = "your-key"
kroma plugins set web provider brave

# Or your own SearXNG instance
kroma plugins set web searxng_url https://your-searxng.example
kroma plugins set web provider searxng

# Auto uses the configured priority; each backend is attempted at most once
kroma plugins set web provider auto
kroma plugins set web search_priority parallel,you,exa,tavily

# Keyless: no API key. free walks SearXNG (if configured) -> Mwmbl ->
# DuckDuckGo HTML/Lite -> Bing RSS; auto does the same when no key is present.
kroma plugins set web provider free
# kroma plugins set web provider mwmbl   # single keyless engine, no switching
```

Changes take effect after restarting Kroma or running `/plugins reload` in an idle TUI.
Launch Kroma with the Exa/Brave key in its environment; environment changes need
a restart. SearXNG must enable JSON search. Mwmbl, Bing RSS and DuckDuckGo HTML/Lite
are keyless best-effort engines with unequal ranking quality; see the web plugin
guide for the measured results on one network. Configure at least one keyed
provider in the priority list, or select a backend explicitly. Auto may incur
multiple API charges; its default per-attempt cap is 8 seconds, total 15 seconds.

The fetcher uses HTTP(S) only; no headless browser, JS rendering or PDF parsing.
It selects static main/article content, removes navigation, preserves code and
links, and decodes declared character sets (including Chinese GBK/GB2312).
Blocked/empty pages fail explicitly rather than masquerading as readable content.
Optional `provider: "firecrawl"` uses a remote, separately billed Markdown
extraction service. Local HTTP remains the default. Credentials can be stored
outside the plugin in a private user-data JSON file; see the web plugin guide.
Webpage text is untrusted data, not instructions.

Long pages return `next_start`; pass it as `start_char` on the same URL.
Continuation reuses a bounded, process-local snapshot for up to two minutes;
a read from `start_char = 0` always refreshes. After eviction/restart/expiry,
continuation returns `CACHE_MISS` without refetching: restart at zero and use
the new offsets. Downloads are limited to 1.5 MB; a warning distinguishes an unavailable source tail from ordinary
pagination. Each request uses a single `timeout_ms` deadline. See the [web plugin guide](web/README.md) for examples and limits.

**Tests**:

```powershell
node --test examples/plugins/web/web.test.js examples/plugins/web/http.test.js examples/plugins/web/main.test.js
cargo test --test plugins_e2e
```

The Rust integration test runs a local HTTP mock server, validating the full
Kroma host -> process-plugin -> HTTP -> tool-result path. The API and experimental-engine unit tests use mocked responses. Paid-provider relevance must be
checked separately using the [live acceptance set](web/evaluation.md).

## Other included examples

| Directory | How to use | What it demonstrates |
| --- | --- | --- |
| `hello` | `/echo hello`, Alt+E, `/plugins view hello` | Basic tool, slash command, Prompt, shortcut and live panel showing processed requests |
| `transform` | Ask model to call `shell` | Explicit builtin tool replacement, Prompt replacement, before/after tool hooks |
| `context-demo` | Enable before creating a **new** session | External ContextFactory/Policy state, persisted revision counter, branches; **does not compact messages** |
| `model-http` | Edit example `base_url`, `model` and budget first | HTTP model profile replacement + context budget |
| `model-process` | Enable in a test-only project | Model backend implemented in a separate executable, with custom wire protocol (single final response) |
| `model-stream` | Enable in a test-only project | Incremental text/reasoning/tool argument fragments, thinking levels, usage and cancellation |
| `model-hook` | Enable and send a message | Request projection rewrite without mutating session history |
| `subagents` | Ask for `subagent_run` or `subagent_parallel` | Runs separate Kroma processes/sessions, parallel task dispatch |
| `memory` | `/remember text`, `/memories`, `/forgetnote 1` | Optional cross-session local notes, injected into model request projection |
| `status` | `/plugins view status` | Declarative TUI footer label and an information panel |

To try another plugin, run
`kroma plugins install examples/plugins/<directory>`, then
`kroma plugins enable <id>` and either restart or use `/plugins reload` while idle for tool/Prompt/command/UI/event changes. Only enable the plugins you intend
to use. `subagents` uses `kroma --print` to start child agents; the Kroma
binary must be available on PATH, or configure `kroma_bin` as an absolute path.
It reuses your configured model provider, so running multiple agents incurs
the corresponding model calls and token costs.

The `memory` plugin stores notes in its installed directory as
`memory.json`, and does not use an AI model to decide what to remember. Notes
are stored only when you run `/remember`. Do not store secrets in notes you
do not intend to include in subsequent model requests.

## Develop and try a plugin without installing it

Use the same manifest validator and process protocol as the runtime, but do
not write activation state:

```powershell
# 1. Validate manifest/API version and run one bounded JSONL smoke request.
kroma plugins check .\examples\plugins\hello

# 2. Load this directory for this process only. plugins.toml is not changed.
$env:KROMA_PLUGIN_TRY = (Resolve-Path .\examples\plugins\hello)
kroma
Remove-Item Env:KROMA_PLUGIN_TRY
```

`KROMA_PLUGIN_TRY` accepts one explicit directory. It uses the normal manifest
validation, command launch, protocol envelope, cancellation and trust boundary;
it does not inherit persisted settings, and `plugins enable/disable/set/uninstall`
refuse to persist it. The process starts only when a tool/command/event is
actually used. A prompt-only plugin does not need a process. This is one
explicit user-supplied path; it does not auto-load project plugins or change
project activation/trust decisions.

The smoke probe sends a harmless `{"type":"ping"}` request. A plugin may reply
with `is_error: true` for an unknown request; that still proves JSONL framing
and response correlation. A crash, timeout, non-JSON frame, mismatched
`request_id`/`protocol_version`, or a missing executable fails the check. The
check never starts a model request and never enables the plugin.

When the plugin is ready, use the normal install/enable flow below. Exit the
trial process or remove `KROMA_PLUGIN_TRY` to stop trying it.

## Installation and configuration

Directories:

- User: `~/.kroma/plugins/<id>/plugin.toml`
- Project: `<workdir>/.kroma/plugins/<id>/plugin.toml`
- Activation and settings: `~/.kroma/plugins.toml`
- Persistent user-plugin data: `~/.kroma/plugin-data/<id>/`
- Persistent project-plugin data: `<workdir>/.kroma/plugin-data/<id>/`
- Override the user directory with `KROMA_PLUGIN_DIR`, which also relocates
  the state file to the parent of the overridden directory.

Commands:

```text
kroma plugins list
kroma plugins install <local-directory>
kroma plugins check <local-directory>
kroma plugins uninstall <id>
kroma plugins enable <id>
kroma plugins disable <id>
kroma plugins config <id>
kroma plugins set <id> <key> <value>
kroma plugins reset <id> <key>
```

In Kroma use `/plugins` with the same subcommands and
`/plugins view <id>` for plugins declaring an information panel.
`/plugins reload` in an **idle TUI** reloads tools, Prompt contributions,
slash commands, event subscribers and static UI metadata. It validates the new
registry first and keeps the old one on failure. Unchanged tool/command
processes are reused; event subscriptions receive fresh isolated workers so
stalled old handlers can be cancelled without killing new ones. The active
conversation, Agent Loop and model stay unchanged. Only an actual change to
an active model, context policy or `before_model_request` Hook requires
restarting Kroma. A broken **disabled** plugin does not block unrelated
reloads; a broken enabled plugin cannot silently disappear. Changes to the
directly executed entrypoint file restart that plugin process; editing nested
dependencies may still require a restart.
Command-line `kroma plugins ...` cannot reload a separate running process.
The CLI `set` command saves a string value; write typed TOML values directly
in `plugins.toml` if needed. Local installation follows ordinary file and
directory symlink targets (such as pnpm-managed dependencies) and copies
contents, without needing symlink creation permissions in the destination.
Recursive symlink cycles are rejected.

`uninstall` removes installed plugin code and its saved activation/settings, even if its manifest is damaged; it intentionally **preserves user-authored plugin data**. Configuration read-modify-write operations use an OS-held `plugins.toml.lock` file across Kroma processes. A damaged but previously activated plugin can still be disabled by ID. Project-local plugins are source-managed: remove their directory manually rather than using `uninstall`. Changes to a running Kroma process still require `/plugins reload` or restart; stateful replacements require restart.

User-global plugins and project plugins have separate enabled-state keys.
Project activation is bound to its canonical plugin directory, so the same
plugin ID in another project cannot inherit the previous project's enabled
state. Duplicate IDs in one discovery scope are reported. A newly installed
plugin is immediately discoverable, and safe runtime changes apply with
`/plugins reload` or a restart.

## Manifest schema

The basic fields:

- `api_version = 1`: optional manifest/API version (defaults to 1 for existing plugins). An unsupported version is rejected at discovery instead of being executed against the wrong contract. This is distinct from a plugin's own `version` and from `[context] version`.
- `id` and `name`: required.
- `description`, `version`: optional.
- `command` and `args`: executable/arguments required for external tools,
  commands, hooks, event subscriptions or context policies.
- `prompt`: append a static Prompt suffix.
- `replace_prompt`: replace the base system Prompt; all plugins still add
  the built-in execution guidance unless they change it explicitly.
- `[[tools]]`: `name`, `description`, JSON-style `[tools.parameters]`,
  optional `replace = true` to replace an existing named tool, and optional
  `streaming = true` to enable live output frames (`tool_stream`).
- `[[commands]]`: slash command `name` and optional `description`.
- `hooks = ["before_tool_call", "after_tool_result", "before_model_request"]`:
  synchronous modifying hooks (each opt-in).
- `events = ["task_started", "tool_finished"]`: observer-only lifecycle
  notifications (best effort, asynchronously delivered).
- `[settings]`: default settings for every request.
- `[context] version = 1`: basic projection/branch adapter (backward compatible).
  `version = 2` additionally enables `context_new_turn`,
  `context_mark_consumed`, `context_observe`, `context_finish`, and
  `context_overflow` lifecycle requests. The plugin's persisted version
  is intentionally not interchangeable with version 1. A context response
  with no `state` property retains the previous state; explicit `"state": null`
  durably resets it. When shortening history for a fork/rewind,
  `context_branch` must supply `state` for shortened history. V2 plugins
  also receive full-history branch notifications for copying out-of-band data;
  the state field is optional in that case. Branching from the CLI/TUI uses
  cancellable async calls. Kroma rejects an incompatible strategy/version
  rather than silently discarding memory.
- `[model]`: `model`, `base_url`, `protocol`, `api_key_env`,
  `context_window`, `output_reserve`, optional `streaming = true`,
  `thinking_levels = ["low", "medium", "high"]`,
  `reasoning_level = "medium"`, and optional `image_input = true/false`
  for a process-model adapter. An omitted `image_input` means unknown.
  Process providers receive image attachment references in `messages[].provider_items`
  (including `path`, `mime`, and inline/reference `mode`); they read those
  local files themselves. The plugin wire protocol does not automatically
  transmit image bytes/base64. The flag only declares support and controls
  which active attachment references remain inline. HTTP provider plugins
  use their upstream provider's own vision capability discovery.
  Set `protocol = "plugin"` to implement an external model wire format inside
  a process (no `base_url` field needed). Only process-model plugins may set
  `streaming = true`; other HTTP protocols continue to work unchanged.
- `[ui]`: `status`, `panel_title`, `panel_text`, `activity_frames` (2–12
  terminal-safe frames, each at most 8 cells), and optional
  `dynamic_panel = true`. `[ui.tool_labels]` maps actual tool names to display
  labels without changing execution. `[ui.colors]` supplies optional `#RRGGBB`
  overrides for `accent`, `text`, `muted`, `background`, `border`, `user`, `tool`
  and `code`; the active user theme remains the base. `[ui.layout]` optionally
  sets `header_rows = 1..4`, `composer_max_rows = 3..14`, and
  `full_width_prose = true/false`. The host still owns Ratatui, scroll,
  terminal input and history geometry. Conflicting animation/layout slots,
  overlapping color keys and tool labels are rejected during idle reload.
  A dynamic panel asks the plugin process for its
  current body whenever `/plugins view <id>` is opened; it is not polled
  per frame. TUI displays it in a scrollable overlay (Esc to close), not
  in the conversation or model context. The overlay caches its wrapped text
  until the terminal width changes; redrawing and scrolling only copy visible
  rows. CLI `plugins view <id>` also works.
- `[[ui.shortcuts]]`: `key = "Alt+E"` and `command = "/echo example"` bind
  an **Alt+letter** chord to a command declared by the **same** plugin.
  Shortcuts run only while the TUI is idle. Normal typing and all Ctrl
  bindings retain their behavior. Conflicts reject the shortcut map rather
  than silently overriding an existing plugin.

Only one plugin may replace a given tool, base Prompt, context factory or
model. Conflicts are reported, not silently overwritten. Registration
order determines modifying hook order.

## Request/response protocol

Kroma lazily starts a persistent plugin process in the plugin directory and
sends a single JSON value per line on stdin. Most requests produce exactly one
JSON response line. The opt-in `model_stream` request produces multiple correlated
frames, ending in `complete` or `error`. Log only to stderr.

```json
{"protocol_version":1,"request_id":42,"type":"tool","name":"example_echo","arguments":{"text":"hello"},"call_id":"call-1","workdir":"...","archive":".../session.json","data_dir":".../plugin-data/example","config":{"prefix":"Echo: "}}
```

```json
{"protocol_version":1,"request_id":42,"content":"Echo: hello","is_error":false}
```

Successful `is_error` defaults to false. Errors return `is_error=true`.
Legacy plugins may omit `request_id` and `protocol_version` in responses;
when supplied, Kroma validates both values against the current request.
Standard error is drained into the bounded runtime log (up to 64 KiB per
worker), never mixed with stdout.
Tool subprocess requests and modifying hooks are cancellable; Kroma kills a
worker on cancellation, restarts it on the next request, and rejects a JSONL
response larger than 2 MiB. Tools and tool hooks receive the same root-level
`call_id` and `archive` fields, identifying the original model call and the
current session archive. `workdir` remains the user's active project directory.
`data_dir` is a stable, per-plugin writable directory, created lazily on the
first process call. Global and project plugin data have separate roots. Every
process request (tool, Hook, context, model, UI, event) receives this field.
These fields are optional to consume; older plugins continue to work.

A tool declared with `[[tools]] streaming = true` receives a `tool_stream`
request with the same arguments/identity as `tool`. It must respond using
correlated JSONL frames (`protocol_version = 1`, matching `request_id`):

```json
{"protocol_version":1,"request_id":42,"event":"output_delta","stream":"stdout","delta":"Building..."}
{"protocol_version":1,"request_id":42,"event":"output_delta","stream":"stderr","delta":"Warning"}
{"protocol_version":1,"request_id":42,"event":"complete","result":{"content":"Done","is_error":false}}
```

`output_delta` is UI-only live progress; the final `complete.result.content`
is the tool result persisted in the transcript. A terminal `error` frame with
`message` is also supported. Cancellation terminates the process worker.
Legacy single-response tool plugins keep the original `tool` request.

Modifying hooks:

- `before_tool_call`: receives root-level `call_id`/`archive` and
  `arguments = {name,arguments}`; return JSON-string-encoded
  `{ "arguments": { ... } }` in the response `content`. Cannot change
  tool name, call type or call ID.
- `after_tool_result`: receives root-level `call_id`/`archive` and
  `{name,content,is_error}`; return JSON
  `{ "content": "new text", "is_error": false }`.
- `before_model_request`: receives `{messages,tools}` and root-level
  `session_id`/`archive`; return any subset of
  `{ "messages": [ ... ], "tools": [ ... ] }`. Request-only projections may
  insert/remove/rewrite messages and adjust tool schemas. Kroma validates the
  final request budget, while the persisted transcript and actual ToolRegistry
  remain unchanged. Custom tool names must still be registered as executable
  tools if the provider calls them.

External context operations (`[context]`):

- `context_prepare`, `context_finish`, and `context_overflow` receive the active
  session path as root-level `archive` as well as the existing messages/state
  in `arguments`. Plugins can refer to original transcripts or keep their own
  per-session files without modifying the WAL directly.
- `context_branch` receives root-level `archive` set to the destination
  session, plus `arguments.source_archive` and `arguments.destination_archive`.
  Shortened branches must return `{ "state": ... }`. For full copies (V2),
  the plugin is notified and may omit `state` to keep the existing value.
  The host persists the new branch only after success. Synchronous Agent
  branching refuses process-backed context adapters rather than blocking the TUI;
  use the async branch API (as the built-in slash commands do).
- Failed tasks clear unconsumed lifecycle notifications, while already
  committed context state and original messages remain durable. Failed
  overflow recovery may return `usage` even with `recovered: false`; the
  usage is counted without committing the rejected recovery state.

External model operations (`[model] protocol = "plugin"`):

- Every process-model and model-Hook request includes root-level
  `session_id` and `archive`. An enabled model plugin must initialize
  successfully at startup; Kroma never silently falls back to another
  Provider. The TUI does not replace it with a saved session profile.
- `model_complete`: receive `{model,messages,tools,reasoning_level}`; return JSON inside
  `content`: `{ "message": {"role":"assistant","content":"..."},
  "usage": {"input":12,"output":5,"cached":0} }`.
- `model_generate`: receive `{model,system,prompt,output_tokens,reasoning_level}`; return
  `{ "text":"...", "requests":1, "usage":{...} }`. Used for title/APEC.
- Both single-response and streaming completions require a real assistant
  message with text or tool calls. Function calls need distinct nonempty IDs,
  nonempty names, and JSON object arguments. Invalid final calls are rejected
  before any tool execution, consistent with the built-in provider.
- To request Kroma's existing single context-overflow recovery retry,
  single-response models return `{ "is_error":true, "code":"context_overflow", "content":"..." }`.
  Streaming models emit `{ "event":"error", "code":"context_overflow", "message":"..." }`
  alongside their normal `protocol_version` and `request_id`. The host forwards
  a typed error to the active context strategy, never guessing from arbitrary
  error text. Other error codes remain ordinary failures.
- By default process models return one final response and Kroma emits its
  content. With `streaming = true`, Kroma sends `model_stream` and receives
  **multiple JSONL frames** before completion. Each frame must include
  `protocol_version:1`, the matching `request_id` and `event`.
  Its request arguments include `{model,messages,tools,reasoning_level}`:

  ```json
  {"protocol_version":1,"request_id":42,"event":"reasoning_delta","delta":"Thinking"}
  {"protocol_version":1,"request_id":42,"event":"text_delta","delta":"Hello"}
  {"protocol_version":1,"request_id":42,"event":"complete","result":{"message":{"role":"assistant","content":"Hello"},"usage":{"input":40,"output":2,"cached":12}}}
  ```

  Tools can additionally be streamed as normalized argument fragments:

  ```json
  {"protocol_version":1,"request_id":42,"event":"tool_call_delta","index":0,"id":"call-1","name":"shell","arguments_delta":"{\"command\":\""}
  {"protocol_version":1,"request_id":42,"event":"tool_call_delta","index":0,"arguments_delta":"echo hi\"}"}
  {"protocol_version":1,"request_id":42,"event":"complete","result":{"message":{"role":"assistant","content":null}}}
  ```

  The first fragment identifies the call; later fragments may append more
  `arguments_delta`. Indexes must be contiguous starting at 0, IDs unique,
  and each final argument string valid JSON object text. At most 64 calls
  and 1 MiB arguments per call are supported. Kroma assembles the tools in
  index order and verifies any duplicate final `message.tool_calls` match
  semantically. Until completion, fragments are buffered internally rather
  than displayed as executable calls.

  Alternatively emit `{ "event": "error", "message": "..." }` with the
  correlation fields. Streamed text/reasoning must match the final message
  when those fields are present; Kroma can fill missing final text from deltas.
  Frames are capped at 2 MiB and buffered through a 32-frame bounded queue.
  Cancelling the model call terminates the plugin worker, and the next call
  starts a clean process. `model_generate` remains a single-response request.
  The manifest's declared `thinking_levels` populate the TUI selector; Kroma
  snapshots `reasoning_level` into each provider request, with no extra IPC.
  The provider is responsible for translating it to its actual wire format.

External context operations:

- `context_prepare`: receives complete messages, current state and available
  token budget. Return `{messages: [...], state: {...}}` serialized inside
  response `content`; must start with a system message. Kroma persists
  state using its versioned ContextState envelope.
- `context_branch`: receives source and retained messages plus old state;
  returns `{state: {...}}`. Rewind/fork that shortens history requires this
  implementation.
- Version 2 batches `context_new_turn`, `context_mark_consumed`, and
  `context_observe` into the **`lifecycle` array on the next** `context_prepare`
  or `context_finish` request. The synchronous Rust callbacks only queue
  compact metadata (no IPC and no second copy of the full transcript).
  `context_finish` commits final derived state. `context_overflow` can
  return `recovered:true` with a new `state` to retry one rejected request.
  Return JSON objects encoded as response `content`. Context state changes
  are applied only after their durable commit succeeds.
- Existing saved builtin-context sessions remain on their original builtin
  policy. New sessions use the external strategy if enabled. If a session has
  external state but its plugin is disabled, Kroma cannot restore that session
  automatically; run `kroma recover-context <session-id>` to explicitly
  recover using its intact original history.

Dynamic UI panel request (requires `[ui] dynamic_panel = true` and `command`):

- Request: `{"type":"ui","name":"panel","arguments":{"id":"hello"}}`
  plus the standard protocol envelope.
- Response: `{"content":"Text to show in the overlay"}`. The process may
  use live state shared with its tools and commands. A TUI cancellation
  aborts the panel worker request; no background polling is performed.

Observer events are delivered through **independent 32-frame bounded queues
per enabled subscriber**, separate from the tool worker. Event envelopes include
`session_id` and `archive`. Reloading or disabling a subscriber cancels its
stalled worker without blocking the TUI or killing the new subscriber. A slow
subscriber cannot block other plugins or the Agent; its own overflowing
notifications are dropped. High-volume streaming deltas are not forwarded.

## Current limits

There is **no plugin sandbox, marketplace, hot replacement of live
model/context policies, or raw Ratatui widget/terminal API**. TUI plugins
may declare status, on-demand panels, bounded animation frames, tool labels,
colors and layout dimensions; the host still owns terminal input and rendering.
TUI extensions also include idle-reloadable commands and Alt+letter
bindings. Version 2 context plugins can observe and recover,
but production custom compression strategies must supply their own code and
tests; the built-in TRC+APEC remains the only production default.
Process providers can stream text, reasoning and JSON tool-call fragments,
plus advertise/switch thinking tiers. They do not yet expose a general vision
attachment transport, provider-specific metadata discovery or a live TUI
preview of partial tool arguments. Subagents do not include a tournament judge or
distributed scheduling. Event observers may drop notifications when the
queue is full. External commands and tools are cancellable; cancellation
terminates the worker tree on Windows and the plugin's process group on Unix. Plugin code executes
with the invoking user's OS privileges.

Plugins execute local code with the user's privileges. Enable only code you
intend to run.
