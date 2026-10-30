# Kroma plugins — Process plugin API (V1 + V2 slices)

Kroma runs normally without plugins. Plugins are local directories with a
`plugin.toml`, and **installed plugins are disabled by default**. Enabling a
plugin takes effect on the next Kroma startup; live reload is not implemented.

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

Changes take effect after restarting Kroma. The Brave key is read from the
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
| `hello` | `/echo hello` | Basic custom tool + slash command + static Prompt |
| `transform` | Ask model to call `shell` | Explicit builtin tool replacement, Prompt replacement, before/after tool hooks |
| `context-demo` | Enable before creating a **new** session | External ContextFactory/Policy state, persisted revision counter, branches; **does not compact messages** |
| `model-http` | Edit example `base_url`, `model` and budget first | HTTP model profile replacement + context budget |
| `model-process` | Enable in a test-only project | Model backend implemented in a separate executable, including any custom wire protocol (V1 final-response, no streaming) |
| `model-hook` | Enable and send a message | Request projection rewrite without mutating session history |
| `subagents` | Ask for `subagent_run` or `subagent_parallel` | Runs separate Kroma processes/sessions, parallel task dispatch |
| `memory` | `/remember text`, `/memories`, `/forgetnote 1` | Optional cross-session local notes, injected into model request projection |
| `status` | `/plugins view status` | Declarative TUI footer label and an information panel |

To try another plugin, run
`kroma plugins install examples/plugins/<directory>`, then
`kroma plugins enable <id>` and restart. Only enable the plugins you intend
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
Changing a setting on a running plugin does not hot-reload that plugin.
The CLI `set` command saves a string value; write typed TOML values directly
in `plugins.toml` if needed.

User-global plugins and project plugins have separate enabled-state keys.
Project activation is bound to its canonical plugin directory, so the same
plugin ID in another project cannot inherit the previous project's enabled
state. Duplicate IDs in one discovery scope are reported. A newly installed
plugin is discoverable immediately, but runtime changes still require restart.

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
  `context_window`, `output_reserve`. Set `protocol = "plugin"` to implement
  any external model wire format inside the plugin process (no `base_url`
  field needed); built-in HTTP protocols continue to work unchanged.
- `[ui]`: `status`, `panel_title`, `panel_text`: static declarative TUI
  UI contributions (not layout/animation APIs).

Only one plugin may replace a given tool, base Prompt, context factory or
model. Conflicts are reported, not silently overwritten. Registration
order determines modifying hook order.

## Request/response protocol

Kroma lazily starts a persistent plugin process in the plugin directory and
sends a single JSON value per line on stdin. One request must produce exactly
one JSON response line on stdout. Log only to stderr.

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
- `before_model_request`: receives `{messages,tools}`; return JSON
  `{ "messages": [ ... ] }`. The message list cannot change length,
  roles, tool-call IDs or opaque provider items. Kroma performs budget
  validation after modification. The durable transcript remains unchanged.

External model operations (`[model] protocol = "plugin"`):

- `model_complete`: receive `{model,messages,tools}`; return JSON inside
  `content`: `{ "message": {"role":"assistant","content":"..."},
  "usage": {"input":12,"output":5,"cached":0} }`.
- `model_generate`: receive `{model,system,prompt,output_tokens}`; return
  `{ "text":"...", "requests":1, "usage":{...} }`. Used for title/APEC.
- V1 process models return one final response, then Kroma emits its content.
  Live delta streaming and provider-specific thinking-level controls are not
  exposed by this adapter yet.

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

Observer events are delivered through a bounded best-effort queue, separate
from the tool worker. High-volume streaming deltas are not forwarded.

## Current limits

There is **no plugin sandbox, marketplace, live reload, or arbitrary TUI
layout/animation API**. Version 2 context plugins can observe and recover,
but production custom compression strategies must supply their own code and
tests; the built-in TRC+APEC remains the only production default.
The process model adapter supports custom upstream protocols but does not
stream interim tokens yet. Subagents do not include a tournament judge or
distributed scheduling. Event observers may drop notifications when the
queue is full. External commands and tools are cancellable; cancellation
terminates the worker tree on Windows and the plugin's process group on Unix. Plugin code executes
with the invoking user's OS privileges.

Plugins execute local code with the user's privileges. Enable only code you
intend to run.
