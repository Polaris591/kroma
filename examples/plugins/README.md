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
kroma plugins list
kroma
```

Then ask Kroma to **search for a topic and read the relevant result pages**.
The model receives two new tools:

- `web_search({ "query": "Rust language", "count": 5 })`: titles, URLs and
  snippets. **Bing RSS is the default** and does not require an API key.
- `web_fetch({ "url": "https://www.rust-lang.org/", "max_chars": 12000 })`:
  readable page title and text, and a truncated indicator.

Search backend alternatives:

```powershell
kroma plugins set web provider duckduckgo
kroma plugins set web provider bing
kroma plugins set web provider brave
$env:BRAVE_SEARCH_API_KEY = "your-key"
kroma plugins set web provider searxng
kroma plugins set web searxng_url https://your-searxng.example
```

Changes take effect after restarting Kroma or running `/plugins reload` in an idle TUI. The Brave key is read from the
process environment, not required in the plugin TOML file. SearXNG must have
its JSON search endpoint enabled. DuckDuckGo HTML parsing is subject to engine
changes/rate limits. Bing's public RSS search endpoint is also not a
guaranteed stable API. For guaranteed API behavior, configure Brave or an
instance of SearXNG you operate.

The fetcher uses HTTP(S) only; no headless browser or JS rendering.
It handles basic HTML text extraction, not complex dynamic sites or PDFs.
Webpage text is untrusted data. Pages can be truncated; use the link to
re-fetch when more text is needed.

**Tests**:

```powershell
node --test examples/plugins/web/web.test.js
cargo test --test plugins_e2e
```

The Rust integration test runs a local HTTP mock server, validating the full
Kroma host -> process-plugin -> HTTP -> tool-result path. The Bing, DDG, Brave
and SearXNG unit tests use mocked responses.

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

## Installation and configuration

Directories:

- User: `~/.kroma/plugins/<id>/plugin.toml`
- Project: `<workdir>/.kroma/plugins/<id>/plugin.toml`
- Activation and settings: `~/.kroma/plugins.toml`
- Override the user directory with `KROMA_PLUGIN_DIR`, which also relocates
  the state file to the parent of the overridden directory.

Commands:

```text
kroma plugins list
kroma plugins install <local-directory>
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
registry first and keeps the old one on failure. Unchanged plugin processes
(and their command/event state) are reused rather than restarted. The active
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

User-global plugins and project plugins have separate enabled-state keys.
Project activation is bound to its canonical plugin directory, so the same
plugin ID in another project cannot inherit the previous project's enabled
state. Duplicate IDs in one discovery scope are reported. A newly installed
plugin is immediately discoverable, and safe runtime changes apply with
`/plugins reload` or a restart.

## Manifest schema

The basic fields:

- `id` and `name`: required.
- `description`, `version`: optional.
- `command` and `args`: executable/arguments required for external tools,
  commands, hooks, event subscriptions or context policies.
- `prompt`: append a static Prompt suffix.
- `replace_prompt`: replace the base system Prompt; all plugins still add
  the built-in execution guidance unless they change it explicitly.
- `[[tools]]`: `name`, `description`, JSON-style `[tools.parameters]`,
  optional `replace = true` to replace an existing named tool.
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
  is intentionally not interchangeable with version 1.
- `[model]`: `model`, `base_url`, `protocol`, `api_key_env`,
  `context_window`, `output_reserve`, optional `streaming = true`,
  `thinking_levels = ["low", "medium", "high"]` and
  `reasoning_level = "medium"`.
  Set `protocol = "plugin"` to implement an external model wire format inside
  a process (no `base_url` field needed). Only process-model plugins may set
  `streaming = true`; other HTTP protocols continue to work unchanged.
- `[ui]`: `status`, `panel_title`, `panel_text` and optional
  `dynamic_panel = true`. A dynamic panel asks the plugin process for its
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
{"protocol_version":1,"request_id":42,"type":"tool","name":"example_echo","arguments":{"text":"hello"},"workdir":"...","config":{"prefix":"Echo: "}}
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
response larger than 2 MiB.

Modifying hooks:

- `before_tool_call`: receives
  `arguments = {name,arguments}`; return JSON-string-encoded
  `{ "arguments": { ... } }` in the response `content`. Cannot change
  tool name, call type or call ID.
- `after_tool_result`: receives `{name,content,is_error}`; return JSON
  `{ "content": "new text", "is_error": false }`.
- `before_model_request`: receives `{messages,tools}`; return any subset of
  `{ "messages": [ ... ], "tools": [ ... ] }`. Request-only projections may
  insert/remove/rewrite messages and adjust tool schemas. Kroma validates the
  final request budget, while the persisted transcript and actual ToolRegistry
  remain unchanged. Custom tool names must still be registered as executable
  tools if the provider calls them.

External model operations (`[model] protocol = "plugin"`):

- `model_complete`: receive `{model,messages,tools,reasoning_level}`; return JSON inside
  `content`: `{ "message": {"role":"assistant","content":"..."},
  "usage": {"input":12,"output":5,"cached":0} }`.
- `model_generate`: receive `{model,system,prompt,output_tokens,reasoning_level}`; return
  `{ "text":"...", "requests":1, "usage":{...} }`. Used for title/APEC.
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

Observer events are delivered through a bounded best-effort queue, separate
from the tool worker. High-volume streaming deltas are not forwarded.

## Current limits

There is **no plugin sandbox, marketplace, model/context hot reload,
or arbitrary TUI layout/animation API**. TUI extensions include static
status, cancellable on-demand panels, idle-reloadable commands and Alt+letter
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
