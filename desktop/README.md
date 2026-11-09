# Kroma Desktop

Independent desktop host for the **existing Kroma Rust Core**. The terminal binary stays light. CLI/TUI/Desktop share model profiles, sessions, WAL, and the plugin manager.

## Design

- Rust Core is owned by one in-process Desktop Host. No subprocess CLI bridge, HTTP control server, Electron, or second Agent Loop.
- The desktop event sink coalesces high-frequency text/tool output in a bounded queue. Critical task/tool events are retained, and WAL transcript remains authoritative.
- Tauri commands own one Agent; cancellation and steering use independent small handles so the window stays responsive.
- Svelte initially renders 72 recent messages, with older history on demand. Completed and live responses use sanitized Markdown; streaming updates are coalesced at a bounded visual refresh rate so long answers remain responsive.
- Plugins use the same PluginCatalog. Enable, disable, install, uninstall, auto-generated typed settings, and idle tool/Prompt/event reload are available. Model/context plugins require reconnection. A plugin installed or edited outside the window is picked up by the next message, and the manifest's declared presentation is drawn here too: tool labels, the activity indicator with its status line, Alt+letter shortcuts and plugin panels.
- Shared file import stores bytes privately and passes references to the existing model protocol, not base64 in WAL.

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

The Windows GNU build boots with the bundled UI and WebView2 loader. A full model/tool workflow is still a separate hardware acceptance check; an unattended build is not proof of end-to-end tool execution. MSVC, macOS/Linux GUI testing, code signing, and measured UI performance remain separate release checks.

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
