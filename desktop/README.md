# Kroma Desktop

Independent desktop host for the **existing Kroma Rust Core**. The terminal binary stays light. CLI/TUI/Desktop share model profiles, sessions, WAL, and the plugin manager.

## Design

- Rust Core is owned by one in-process Desktop Host. No subprocess CLI bridge, HTTP control server, Electron, or second Agent Loop.
- The desktop event sink coalesces high-frequency text/tool output in a bounded queue. Critical task/tool events are retained, and WAL transcript remains authoritative.
- Tauri commands own one Agent; cancellation and steering use independent small handles so the window stays responsive.
- Svelte initially renders 72 recent messages, with older history on demand. Completed and live responses use sanitized Markdown; streaming updates are coalesced at a bounded visual refresh rate so long answers remain responsive.
- Plugins use the same PluginCatalog. Enable, disable, install, uninstall, auto-generated typed settings, and idle tool/Prompt/event reload are available. Model/context plugins require reconnection. A plugin installed or edited outside the window is picked up by the next message, and the manifest's declared presentation is drawn here too: tool labels, the activity indicator with its status line, Alt+letter shortcuts and plugin panels.
- Shared file import stores bytes privately and passes references to the existing model protocol, not base64 in WAL.

## Settings and conversation capabilities

The sidebar has one **设置** entry with **模型 / 工具 / MCP / Skills / 插件**.
These are navigation categories, not all-or-nothing capability switches. Each
built-in tool, Skill, MCP service and plugin has its own global enable/disable
default, and **会话设置** in the conversation header provides
**开启 / 关闭 / 跟随默认** for every item. **全开 / 全关** are only shortcuts that batch-select
the current list; with a search filter, they affect only matching items. They do
not replace individual controls or set a category-wide policy for future items.
The model editor and plugin manager retain their existing save/connect/install
controls. Global plugin settings use only the plugin manager, with one default
switch for the selected plugin, rather than a second activation list. A session
can enable a default-disabled plugin without changing that default.

These are configuration choices, not permissions or approval. Turning off one item
does not turn off its peers or unrelated capabilities. Disabled items remain in
the inventory; disabling does not uninstall anything or delete configuration.
A disabled plugin contributes neither runtime capabilities nor custom UI.
Capabilities are not actually replaced during an active task; finish or stop it
before applying capability changes.

Tool, MCP and Skill defaults use `~/.kroma/capabilities.json`; plugin defaults
reuse the existing plugin activation store. Conversation overrides live alongside
the existing session catalog, outside the transcript/WAL. Per-item overrides can
be reset to inherit their defaults without changing other items. Missing capability
settings preserve existing defaults, including a plugin's configured enabled state.
MCP service definitions continue to come from `~/.kroma/mcp.json`, project
`.kroma/mcp.json`, or the `KROMA_MCP_CONFIG` override. Skills continue to be read
from `SKILL.md` files in `.agents/skills` and `.kroma/skills` under the home/project
directories. No new MCP/Skills definition editor or installation marketplace is
claimed here.

## Attachments, closing and export

- **Opening images again**: a PNG/JPEG/WebP picked or dropped into the composer
  shows a thumbnail that opens a read-only preview; images already stored in a
  conversation open the same way. Bytes come from one narrow `preview_image`
  command: canonicalized path, regular file, at most 8 MiB, verified PNG/JPEG/WebP
  magic. A history path must resolve inside `~/.kroma/attachments`, so a stale
  transcript entry cannot become a general file reader. Nothing is written to the
  WAL or local storage, Blob URLs are revoked, history images are only read when
  opened, and a draft keeps at most eight attachments. An older Desktop without
  the command says the preview needs an update instead of showing a broken image.
- **Closing during a task**: the title bar close and Alt+F4 ask first while a task
  runs. *继续执行* is the initial focus; *关闭并中断* stops the task, keeps the draft
  and closes at once. Completed tool calls and file edits are not rolled back, and
  an idle window closes without a question. The window needs
  `core:window:allow-destroy` because the API destroys it after an allowed close
  request.
- **Input that never ran**: if attachment import fails, the backend reports
  `input_rejected` and the composer takes the exact text and paths back. A model,
  network or IPC failure never sets it, because that turn may already be saved,
  and nothing is retried automatically. A draft typed meanwhile is never
  overwritten.
- **Markdown export with attachments**: user/assistant turns keep their text and
  now also list attachment names with kind and size. Names are Markdown-escaped,
  the private stored path and file bytes never appear, and the file itself is not
  copied. System, tool and reasoning content stays excluded.

## Desktop UI refactor (unreleased)

The first UI pass keeps the existing runtime and plugin contracts. It unifies
colours and typography, simplifies the sidebar and model/effort picker, moves
turn details below the answer, and anchors jump-to-latest to the transcript.
Plugin themes still override host colours; narrow windows expose sidebar
plugins through a recoverable drawer. Settings groups models, tools, MCP, Skills
and plugins behind one entry, with per-item global defaults and conversation
overrides rather than four category-wide switches (see above). Enabled MCP
services load directly from configuration; there is no approval step. Configuration
commands may start local programs, so review your own MCP configuration before use.

Frontend unit checks and the synthetic-host browser checks are separate:
[Browser checks](tests/README.md) describe per-item capability coverage, including
filtered batch actions, shared plugin defaults and session overrides, without
provider calls or personal conversation data. On 2026-11-12, all 12 main-UI,
10 settings/dialog and 8 per-item capability groups passed, with no browser runtime
errors. Frontend checks, 160 unit tests, the production build, core runtime tests
and selected integration tests also passed. The Desktop native library test
program compiled but could not start on Windows GNU
(`0xc0000139 / STATUS_ENTRYPOINT_NOT_FOUND`); its cases are not counted as passing.
These checks do not certify native window controls, real IME input, platform
performance or installer behavior.
The remaining work and verification limits are recorded in the
[refactor plan](../docs/desktop-refactor-plan.md).

The subsequent design-polish pass uses graphite surfaces, neutral primary
actions and a restrained grey-violet interaction accent, not green as the
default brand/action/running colour. Model settings distinguish viewing,
connecting, startup defaults and unsaved drafts. Plugin settings show details
immediately and retain unsaved secrets only inside the mounted inspector.
The current frontend verification is **211 unit tests and 16 browser suites**,
including frozen running snapshots, live request/usage metrics, long-history
navigation, scoped diagnostics, light themes and system high-contrast mode.
See the [visual system](../docs/desktop-visual-system.md) and
[design review](../docs/desktop-design-review.md). These remain synthetic-host
checks, not a new native-window or installer certification.

## Build

Requirements: Node.js 22, Rust and Tauri platform prerequisites. Windows needs WebView2; **MSVC C++ Build Tools** are recommended for official installer builds, while the existing Windows GNU Rust toolchain also builds a working local preview (verified on this development PC). macOS uses Xcode command-line tools. Linux uses WebKitGTK 4.1 and matching libraries.

In desktop/:

    npm ci
    npm run check
    npm run build
    npm run tauri dev

Root repository cargo build still builds the original CLI/TUI. Desktop can launch without any model profile and expose Settings before model initialization. No CLI installation is required.

### Optional Cargo mirror

When crates.io is unreachable, use the RsProxy sparse registry for this command only (no change to the user's global Cargo config). From the repository root:

```sh
cargo --config 'source.crates-io.replace-with="rsproxy-sparse"' \
      --config 'source.rsproxy-sparse.registry="sparse+https://rsproxy.cn/index/"' \
      fetch --manifest-path desktop/src-tauri/Cargo.toml --target x86_64-pc-windows-msvc
```

Use the same two `--config` flags for subsequent `cargo check` / `cargo build` invocations; the desktop's `Cargo.lock` pins the versions.

### Standalone Windows x64 binary (no development server)

From `desktop/` run `npm run build` to generate frontend assets. Then from the repository root run:

```sh
cargo --config 'source.crates-io.replace-with="rsproxy-sparse"' \
      --config 'source.rsproxy-sparse.registry="sparse+https://rsproxy.cn/index/"' \
      build --release --locked --target x86_64-pc-windows-gnu \
      --features tauri/custom-protocol \
      --manifest-path desktop/src-tauri/Cargo.toml
```

**The `tauri/custom-protocol` release feature is mandatory for direct `cargo build`: without it, the packaged frontend is not served and the window attempts the Vite development URL (`localhost:1420`).** Tauri CLI's official `tauri build` flow automatically selects this feature. Open `desktop/src-tauri/target/x86_64-pc-windows-gnu/release/kroma-desktop.exe`, keeping its neighboring `WebView2Loader.dll` beside the executable. No CLI subprocess, Vite server, or preinstalled Kroma binary is needed.

The Windows GNU build packages the UI and uses the adjacent WebView2 loader. The current executable was rebuilt but not launched to replace the user's window. A full native model/tool workflow remains a separate hardware acceptance check; an unattended build is not proof of startup or end-to-end tool execution. MSVC, macOS/Linux GUI testing, code signing, and measured UI performance remain separate release checks.

## First Desktop daily-driver pass

- Search sessions by title or ID in the sidebar. Ctrl/Cmd+K focuses the search,
  Ctrl/Cmd+N creates a new session when idle, and Ctrl/Cmd+, opens Settings.
- Loading older messages preserves the reader's scroll position; the jump-to-latest
  control appears only when the transcript is scrolled away from the bottom.
- Native web search can be enabled per saved model profile. It is opt-in,
  supported only when the configured upstream protocol and endpoint provide it,
  and may incur provider charges. Older saved Desktop profiles preserve the
  setting when edited.
- The drag-and-drop listener is removed if the view unmounts while registration
  is pending.

The first pass has frontend unit coverage. Full hardware/installer validation
remains part of Desktop acceptance, not a prerequisite for continuing UI work.

## Delivered P0–P2

Project selection, model profiles and keys, real Agent text/reasoning/tool streaming, cancellation, steer, session history/resume/rename, file and image references, Markdown, code, plugin manager, Token/cache metrics and cross-platform build workflow.

**P3/P4 not implemented**: third-party plugin-authored GUI settings, UI component replacement, animated themes and full custom workspace replacement. Their extension contract is reserved; see EXTENSION_PLAN.md.

**Release remaining**: installer signing, Windows SmartScreen reputation, real-hardware performance certification and cross-platform GUI acceptance. Unit tests and a process startup check do not prove cold-launch P95, WebView RAM, or scrolling FPS.

## Acceptance checks

1. Launch without saved model settings; open Settings successfully.
2. Connect a model; issue a task including a tool call; observe text, thinking and tool output; inspect final WAL transcript.
3. Cancel a long tool request. Completed side effects remain honest and the next task works.
4. Restart the app and restore the same history, usage and plugin enable state.
5. Start CLI and Desktop simultaneously: the same session lock must prevent concurrent writes.
6. Install, enable and configure a tool plugin; idle reload works. Model/context plugin changes require reconnect.
7. Send a file and image; stored history includes file references, not inlined base64.
8. Test 1000 messages, high Token throughput, 10 MB output, RSS of all WebView children, cold start P95, idle CPU and repeat cancellations on all platforms.

## Windows 0.3.0 release

0.3.0 builds on the stabilized 0.2.x Desktop experience without changing the
theme or core session storage. The new daily-use capabilities are:

- **Follow-up task queue**: while an Agent is running, type another instruction
  and click **排队** (or press Ctrl+Enter). It becomes an independent future user
  turn, not a steering instruction. Queue entries support reordering, deleting,
  pause and resume. Completed tasks advance the queue sequentially through the
  same Agent. Failure and cancellation pause it. Entries are scoped to the
  original project/session and persisted in WebView local storage; reopening
  Desktop **never auto-executes** a saved queue. Active queued tasks removed
  from the queue before submission are never silently retried after an ambiguous
  failure. Attachments are still only supported for immediate tasks.
- **Local conversation content search**: Ctrl+K searches titles instantly and
  recent *user messages* asynchronously, with small inline snippets. Each
  project scans at most 25 recent sessions, skipping archives larger than
  12 MiB; results are capped at 24 across at most 6 recent projects. No
  database, background index, tool-result search or provider call.
- **Markdown export**: use the export icon beside the active session title to
  select a .md file with the native file-save dialog. The export includes
  user/assistant text only, not system messages, reasoning or tool logs.
- **Shortcut reference**: the sidebar entry **快捷键** (or F1) lists the shortcuts
  that actually exist in the window and the composer. F1 never replaces an open
  dialog, and the panel closes with Escape. The list is descriptive only: it
  does not add custom bindings.
- **Code fence language**: a fenced block shows the language it declared
  (`language-*`) as plain text beside the existing copy control. Undeclared
  blocks stay unlabelled, very long names are truncated, and copying still
  yields the exact source text.
- **Pinned conversations**: each session row has a pin control. Pinned sessions
  lead their own workspace, stay visible when the group is folded to its recent
  entries, and can be unpinned from the same row. Pins are a Desktop-local UI
  preference in WebView local storage (per workspace), not session, WAL or CLI
  data; when storage is unavailable the pin applies to this window and the app
  says so.

Earlier performance work remains: streamed events are batched before rendering,
long coding turns render a bounded recent history window, and in-flight
steering preserves text the user continues typing.

The terminal app remains `kroma` and Desktop is `kroma-desktop`. They link the same Rust Core and use the same plugin ecosystem, but are distributed separately.

The Windows installer uses a Tauri NSIS current-user package. It preserves existing user configuration and sessions under the unchanged application identifier `dev.kroma.desktop`. From the repository root:

```sh
npm --prefix desktop ci
npm --prefix desktop run check
npm --prefix desktop run test
npm --prefix desktop run tauri -- build --ci --no-sign \
  --target x86_64-pc-windows-gnu \
  --bundles nsis \
  --config src-tauri/tauri.bundle.conf.json
```

The resulting installer is `desktop/src-tauri/target/x86_64-pc-windows-gnu/release/bundle/nsis/Kroma Desktop_0.3.0_x64-setup.exe`. When built using Windows MSVC, the default target is `desktop/src-tauri/target/release/bundle/nsis/`. The desktop CI workflow runs Windows-native unit tests and produces an installer artifact.

To build the portable ZIP, run the same Tauri build first, then:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File desktop/scripts/package-portable.ps1
```

This packages the compiled executable, `WebView2Loader.dll`, a short README, and SHA-256 hashes; it excludes `~/.kroma`, conversation WALs and API credentials. The archive is generated under ignored `desktop/release/`. **Never ship a package made from an unverified executable.** The installer is currently unsigned; Windows may show an unknown-publisher warning.

### Release acceptance

- Frontend: `npm --prefix desktop run check && npm --prefix desktop run test && npm --prefix desktop run build`.
- Core: `cargo test --offline --lib`; Desktop host: `cargo test --manifest-path desktop/src-tauri/Cargo.toml --lib` on an MSVC-enabled or CI build environment.
- Portable startup: launch the extracted `Kroma Desktop.exe`, check that a real GUI window appears, then close it. The adjacent DLL must stay in place.
- Manual hardware checks remain required for actual provider calls, tool execution, cancellation, session changes and restart/recovery. Do not claim these passed based on a headless build alone.

## Complete drafts, queued attachments, search, backup and save-as

- **Complete drafts (v2)**: a draft now stores the exact text plus up to eight
  file references under `kroma.desktop.draft.v2:<project>:<session>`; the
  previous plain-text `…draft.v1:` entry is still read once and replaced on the
  next save. Nothing stores bytes or Blob URLs. A restored draft checks each
  file once (`inspect_attachments`: regular file, 32 MiB, 8 MiB for images) and
  keeps a failed chip labelled *已失效* with a reason, removable and replaceable;
  editing is never blocked. Checks carry an epoch, so a late answer cannot
  overwrite a newer draft. `beforeunload` saves synchronously, so a close never
  waits for an IPC round trip. A draft longer than 20 000 characters is stored
  truncated **and said out loud**; storage failures show a non-blocking notice
  instead of silently keeping the draft in memory only.
- **Queued attachments**: *排队* (or Ctrl+Enter) while a task runs now copies the
  selected files into the private attachment store immediately
  (`prepare_followup_attachments`), so later edits to the source cannot change a
  queued task. Tasks persist `{id,text,paths}`; older text-only entries still
  load. A pure-attachment task is allowed. Only the exact queued draft is
  cleared, and only if it is still unchanged when the copy finishes. A restored
  queue stays paused; before executing a task the stored paths are checked and a
  missing one pauses the queue with an explicit message instead of sending an
  incomplete turn.
- **Find in conversation (Ctrl+F)**: searches the visible text of user prompts
  and assistant answers in the already loaded messages (never system, tool
  output, reasoning or host references) with previous/next, count, message-level
  highlight and Escape. It never calls the model or a tool, reads no disk per
  keystroke, and says when older history is not loaded. Highlighting marks the
  message element only; Markdown is not rewritten, so code blocks and their copy
  control stay exact. Ctrl+K still searches across conversations: results now
  include role and the absolute `message_index`, and opening a result waits for
  the navigation to settle before paging older history (bounded to 12 pages) to
  the target.
- **Local session backup**: a separate entry from Markdown export writes one
  `*.kroma-backup` JSON file with the complete transcript (snapshot **and** the
  newest WAL entries), the persisted context state, the title and the private
  attachment bytes. Limits: 128 MiB encoded package, 32 MiB per attachment
  (8 MiB images), 96 MiB attachment total, 200 000 messages. Exported attachment
  references are opaque tokens, not local paths; the working directory and
  external tool-log files are not included, and saved credentials or
  model/plugin configuration are not added (sensitive text already in the
  conversation remains; the command reports how many external references were
  omitted). Import
  validates the version, message/context consistency, attachment id mapping,
  duplicates, decoded length and real magic before creating anything; it writes
  attachments under freshly generated private names and creates a brand-new
  session id (never overwriting one), and a failure removes everything that call
  created. It never sends a message or replays a tool. Backup/import are local
  only and refuse to run while a task is active.
- **Save an attachment as**: every stored history attachment (images keep their
  preview) has a save action. The native save dialog confirms overwrite; the
  command canonicalizes the source inside `~/.kroma/attachments`, requires a
  regular file (32 MiB, 8 MiB for images), refuses destinations inside Kroma's
  own data directories and the source itself, and writes the full bytes to a
  temporary file before replacing the target. Cancelling is not an error; a
  missing file says so and can be retried.
- **New native commands**: `inspect_attachments`, `prepare_followup_attachments`,
  `save_attachment_as`, `export_session_backup`, `import_session_backup`. An
  older Desktop binary answers "command not found"; the window then asks for an
  update and restart instead of crashing.
