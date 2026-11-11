# Kroma plugins — Process plugin API (V1 + V2 slices)

Kroma runs normally without plugins. Plugins are local directories with a
`plugin.toml`, and **installed plugins are disabled by default**. Enabling a
plugin takes effect after restart or with `/plugins reload` while the TUI is idle. Unchanged model/context strategies and model-request Hooks can stay active during unrelated plugin reloads; actual changes to them still require restarting Kroma.

The plugin process can be written in any language. The bundled examples use
Node.js 22+ without external npm dependencies. JavaScript is not required to
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
  snippets. Supports Exa, Brave, Tavily, You.com, Parallel, SearXNG or the free
  Mwmbl index. `auto` uses the first backend with a credential
  (Parallel → Brave → You.com → Exa → Tavily) and makes one request only, so a
  search never silently bills a second provider; choose a provider explicitly to
  pin one. `site:domain` and `-site:domain` are checked against returned URLs.
  The model cannot switch providers through tool arguments.
- `web_fetch({ "url": "https://www.rust-lang.org/", "max_chars": 12000 })`:
  page title and structured text (headings, code, tables and links), plus
  `next_start` for continuing a long page with `start_char` and the same URL.

Search backend alternatives:

```powershell
# Brave instead of Exa
$env:BRAVE_SEARCH_API_KEY = "your-key"
kroma plugins set web provider brave

# Or your own SearXNG instance, with JSON search enabled
kroma plugins set web searxng_url https://your-searxng.example
kroma plugins set web provider searxng

# A key can also be stored in plugin settings (plaintext file)
kroma plugins set web exa_api_key <key>

# Keyless: the free Mwmbl index only
kroma plugins set web provider free
```

Changes take effect after restarting Kroma or running `/plugins reload` in an idle TUI.
Launch Kroma with the Exa/Brave key in its environment; environment changes need
a restart. Plugin processes inherit the host environment by default so this
example can read its provider key. Set `KROMA_PLUGIN_ISOLATE_ENV=1` to remove
credential-shaped variables from plugin children when a plugin should use saved
settings instead. `KROMA_PLUGIN_TOOL_TIMEOUT_SECS` optionally caps any plugin
tool call, and a manifest `timeout_secs` adds a per-tool deadline; the smaller
of the two wins. Unset preserves the unlimited default for non-streaming tools.
Without a credential, `auto` and `free` use the free Mwmbl index: about a second,
decent for English technical queries, weak for Chinese and niche topics. There
is no chain of scraped search pages, so no blocked engine can stall a search.
Configure a key or a SearXNG instance for dependable coverage; the web plugin
guide records one measured run.

The fetcher uses HTTP(S) only; no headless browser, JS rendering or PDF parsing.
It selects static main/article content, removes navigation, preserves code and
links, decodes declared character sets (including Chinese GBK/GB2312) and
reports an unreadable page (for example a JavaScript application shell) instead
of returning navigation links as content. Compression, HTTP/2 and connection
reuse come from the system fetch; proxy variables in the launch environment are
honoured on Node 22.21+ / 24.5+. Webpage text is untrusted data, not instructions.

Long pages return `next_start`; pass it as `start_char` on the same URL.
Continuation reuses a bounded, process-local snapshot for two idle minutes, and
the window refreshes on every read. After eviction, restart or expiry,
continuation returns `CACHE_MISS` without refetching: restart at zero and use
the new offsets. Downloads are limited to 1.5 MB; a warning distinguishes an
unavailable source tail from ordinary pagination. See the
[web plugin guide](web/README.md) for examples and limits.


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
| `image-gen` | Install, configure an image API, then ask Kroma or run `/image <prompt>` | OpenAI Images / Gemini, recent user images, reusable image IDs, atomic file outputs; see [image-gen guide](image-gen/README.md) |
| `transform` | Ask model to call `shell` | Explicit builtin tool replacement, Prompt replacement, before/after tool hooks. **Replaces the built-in `shell` and rewrites that tool only; other tools pass through unchanged** |
| `context-demo` | Enable only before creating a **new** session | External ContextFactory/Policy state, persisted revision counter, branches; **it never compacts, so a long session hits the context limit and fails** |
| `model-http` | Edit example `base_url`, `model` and budget first | Config-only HTTP model profile replacement + context budget; no plugin process and no JavaScript |
| `model-process` | Enable in a test-only project | Provider in a separate executable, non-streaming contract: `model_complete` and `model_generate` each answer with one final payload |
| `model-stream` | Enable in a test-only project | Provider in a separate executable, streaming contract: correlated `model_stream` frames, text/reasoning/tool-argument deltas, thinking levels, usage and cancellation |
| `model-hook` | Enable and send a message | `before_model_request` Hook: rewrites the outgoing request projection while the configured provider still serves it; no model traffic of its own |
| `subagents` | Let the model call `subagent` | Delegated tasks in separate sessions and working directories, up to 8 parallel children, per-task evidence and a 1800s default child deadline. The call waits for its children and returns their answers in one result, with `▶ [i/n]` progress lines while they run; cancelling the tool call stops the child process tree |
| `goal` | `/goal <task>`, then `/goal` for status | One host-owned goal per session: acceptance points, check plan, pause/resume/cancel, no early finish |
| `ask` | Enable, then ask Kroma for something with a real fork in it | Declarative plugin with no process: `host = "ask_user"` names a tool the host itself runs, so the model can offer 2+ options (single or multiple choice, free text always available) and wait for the pick |
| `memory` | `/remember text`, `/memories`, `/forgetnote 1` | Optional cross-session local notes, injected into model request projection; human-driven, superseded by `project-memory` |
| `project-memory` | Build the Rust binary, install `plugin/`, then use normally | Local SQLite memory: automatic recall, verified original user wording, optional review history and explicit user controls; no automatic eviction |
| `status` | `/plugins view status` | Declarative TUI footer label and an information panel |

### The model examples: which contract, which one first

Four examples touch the model path, and they are **four different contracts**,
not four ways to do the same thing. Pick by what you need to change:

| Example | Manifest keys | Your code receives | Use it when |
| --- | --- | --- | --- |
| `model-http` | `[model]` with `base_url`, `protocol = "chat"`, `api_key_env`, `context_window`, `output_reserve` | nothing; there is no process | The endpoint is already OpenAI-chat compatible and you only want to repoint model/base URL or fix the context budget |
| `model-process` | `[model]` with `protocol = "plugin"` (no `streaming`) | one `model_complete` request per turn (plus `model_generate` for summary/title work), answered with a single `{message,usage?}` / `{text,usage?,requests?}` | Your backend cannot stream, or you want the smallest process-provider contract; the reply appears all at once |
| `model-stream` | `[model]` with `protocol = "plugin"`, `streaming = true`, `thinking_levels` | `model_stream` followed by correlated frames: `text_delta`, `reasoning_delta`, `tool_call_delta`, then `complete` or `error`; `model_generate` stays single-shot | You need incremental output (thinking levels, partial tool arguments, first-token latency) or cancellation mid-answer |
| `model-hook` | `hooks = ["before_model_request"]` | the projected `messages` array, answered with a JSON patch | You keep the configured provider and only add/rewrite context (injected instructions, redaction); it never talks to a provider itself |

Start order: `model-http` (no code) → `model-process` (your own backend, easiest
process contract) → `model-stream` (only if you need deltas) → `model-hook`
(only if the provider must stay unchanged). `model-stream` is not a simplified
`model-process`: `streaming = true` switches the host to the frame protocol, so
one example cannot stand in for the other. `transform` is not in this group: it
replaces a tool and hooks tool call/result, not the model request.

To try another plugin, run
`kroma plugins install examples/plugins/<directory>`, then
`kroma plugins enable <id>` and either restart or use `/plugins reload` while idle for tool/Prompt/command/UI/event changes. Only enable the plugins you intend
to use. `subagents` starts child agents with `kroma --json`; the Kroma binary
must be available on PATH, or configure `kroma_bin` as an absolute path. Each
task gets its own session, working directory and evidence directory, and the
host stops the whole batch at the declared `timeout_secs`. It reuses your
configured model provider, so running multiple agents incurs the corresponding
model calls and token costs.

The `project-memory` example is a standalone Rust binary (`cargo install --path
examples/plugins/project-memory`) with private project-scoped SQLite storage.
It declares `provide_context` (API 2), saves verified original human wording,
and recalls complete records without silent truncation or automatic eviction.
Optional review history stays accessible when evidence changes. It does not
capture task text at completion. Its four `memory_*` tools and `/memory` commands
support inspection, editing, pin/unpin, forgetting and recovery. See its README
for installation, privacy and migration details.

The `memory` plugin stores notes under the host-provided plugin data directory
(`~/.kroma/plugin-data/memory/memory.json` for a user-global install, so
uninstalling the plugin code keeps your notes), and does not use an AI model to
decide what to remember. Notes are stored only when you run `/remember`. Do not
store secrets in notes you do not intend to include in subsequent model requests.

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

## Start from a template

`examples/plugins/hello` is the smallest complete plugin: one tool, one slash
command, a live panel, an Alt+E shortcut and one setting. Copy that folder,
change `id`, `name`, the tool name and the command name, and it is a working
plugin. Nothing in the runtime names a plugin, so adding one never means
recompiling Kroma.

The proof lives in the test suite:
`tests/plugins_e2e.rs::an_unseen_plugin_needs_no_core_change` installs
`tests/fixtures/plugins/timeline-note` — a plugin whose id, tool, command and
panel appear nowhere in the source — and checks that its tool, command, panel,
tool label, status line, indicator frames and Alt+letter shortcut all reach the
interface, with no core edit.

### Which `[ui]` keys each interface draws

| Key | Terminal (TUI) | Desktop |
| --- | --- | --- |
| `tool_labels` | Tool line label | Tool line label |
| `activity_frames` | Spinner while a task runs | Spinner while a task runs |
| `status` | Status bar text | Status line above the composer |
| `panel_title`, `panel_text`, `dynamic_panel` | Centered panel | Panel dialog in the plugin page |
| `shortcuts` | Alt+letter runs the command | Alt+letter runs the command |
| `colors` | Theme slots: accent, background, border, text, muted, user, tool, code | Same slots paint the plugin's own panel and its status line; unknown or malformed values are ignored |
| `layout.composer_max_rows` | Composer height cap | Same cap on the input box, never above the built-in limit |

A plugin that only wraps another language's tool needs no `[ui]` section at all.

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
The CLI `set` command converts the value to the type of that key's manifest default
(boolean, integer, comma-separated string array or string) and rejects keys the
manifest does not declare, listing the available settings; write typed TOML values
directly in `plugins.toml` for anything else. Local installation follows ordinary file and
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
- `[[tools]]`: `name`, `description`, optional `host = "ask_user"` for a
  tool this process runs itself (then no `command` is needed and the host owns
  `parameters`; declaring them is rejected), JSON-style `[tools.parameters]`,
  optional `replace = true` to replace an existing named tool, optional
  `streaming = true` to enable live output frames (`tool_stream`), and
  optional `timeout_secs` (1..=86400) for a host-enforced whole-call deadline.
  Two further opt-ins gate host-supplied context: `requires_user_context = true`
  adds `trusted_user_context` (`{"message": <first 4096 chars of the current
  user message>, "turn": <turn index>}`), and `requires_user_attachments = true`
  adds `trusted_user_attachments` (`{ref, name, path, mime, bytes}` for images
  of the current turn as `attachment:N` and of the previous turn as
  `previous:N`). Both are `null` unless declared, and call arguments are never
  used as proof of the current user turn.
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
  and `code`; the active user theme remains the base and the same slots apply in
  the terminal and in the desktop shell. `[ui.layout]` optionally
  sets `composer_max_rows = 3..14`. Legacy `header_rows` and
  `full_width_prose` values are ignored because the default Plain layout
  determines header and reading width itself. The host still owns Ratatui, scroll,
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
`message` is also supported; it is fatal, so the host discards that worker and
the next call starts a fresh process. A `complete` result with
`is_error = true` is an ordinary task failure and keeps the worker reusable.
Cancellation terminates the process worker.
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

V2 host capabilities (`api_version = 2`):

- `capabilities = ["provide_context", "task_finished"]` is the generic V2
  opt-in and requires a command. Existing `request_context` / `task_summary`
  V2 plugins remain supported without modification.
  V1 plugins keep the exact V1 frames and behavior.
- `provide_context` is sent once per user task with an intent, project/session fields and a
  token budget. The plugin returns compact blocks; the host appends them to the current
  request projection as untrusted reference data. Empty, slow, malformed or failed replies
  mean no injection and never fail the model request.
- `task_finished` is dispatched after the Agent's final WAL boundary, **not**
  after each assistant reply; it carries the final status and bounded user task
  text, and is best effort, nonblocking and idempotent. A plugin can use it to
  update a project history without ever becoming an Agent Loop or a scheduler.
- `[[tools]] invalidates_context = true` forces a fresh context projection
  after a successful tool mutation (e.g. remembering or forgetting a rule).
- See `docs/project-memory.md` for current project memory behavior and its README for settings.
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
colors and layout dimensions; a `ui.widget` with `placement = "transcript"`
also inserts a content block into the message stream so it scrolls with the
conversation, while the host still owns terminal input, message cards,
the composer and all rendering.
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
