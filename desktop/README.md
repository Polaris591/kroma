# Kroma Desktop (P0–P2)

Independent desktop host for the **existing Kroma Rust Core**. The terminal binary stays light. CLI/TUI/Desktop share model profiles, sessions, WAL, and the plugin manager.

## Design

- Rust Core is owned by one in-process Desktop Host. No subprocess CLI bridge, HTTP control server, Electron, or second Agent Loop.
- The desktop event sink coalesces high-frequency text/tool output in a bounded queue. Critical task/tool events are retained, and WAL transcript remains authoritative.
- Tauri commands own one Agent; cancellation and steering use independent small handles so the window stays responsive.
- Svelte renders a bounded recent-history window (150 messages, with older history on demand), Markdown on completed messages, and plain streaming content while running.
- Plugins use the same PluginCatalog. Enable, disable, install, uninstall, auto-generated typed settings, and idle tool/Prompt/event reload are available. Model/context plugins require reconnection.
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

### Standalone Windows GNU preview (no development server)

From `desktop/` run `npm run build` to generate frontend assets. Then from the repository root run:

```sh
cargo --config 'source.crates-io.replace-with="rsproxy-sparse"' \
      --config 'source.rsproxy-sparse.registry="sparse+https://rsproxy.cn/index/"' \
      build --release --locked --target x86_64-pc-windows-gnu \
      --features tauri/custom-protocol \
      --manifest-path desktop/src-tauri/Cargo.toml
```

**The `tauri/custom-protocol` release feature is mandatory for direct `cargo build`: without it, the packaged frontend is not served and the window attempts the Vite development URL (`localhost:1420`).** Tauri CLI's official `tauri build` flow automatically selects this feature. Open `desktop/src-tauri/target/x86_64-pc-windows-gnu/release/kroma-desktop.exe`, keeping its neighboring `WebView2Loader.dll` beside the executable. No CLI subprocess, Vite server, or preinstalled Kroma binary is needed.

Windows GNU startup, frontend loading, model connection, and a real model chat were smoke-tested locally on 2026-10-30. MSVC packaging, macOS/Linux GUI testing, installer signing, and performance certification remain separate release checks.

## Delivered P0–P2

Project selection, model profiles and keys, real Agent text/reasoning/tool streaming, cancellation, steer, session history/resume/rename, file and image references, Markdown, code, plugin manager, Token/cache metrics and cross-platform build workflow.

**P3/P4 not implemented**: third-party plugin-authored GUI settings, UI component replacement, animated themes and full custom workspace replacement. Their extension contract is reserved; see EXTENSION_PLAN.md.

**P5 not implemented**: signed installers and full real-hardware performance certification. A frontend build or Rust type-check does not prove cold launch, WebView RAM or scrolling FPS.

## Acceptance checks

1. Launch without saved model settings; open Settings successfully.
2. Connect a model; issue a task including a tool call; observe text, thinking and tool output; inspect final WAL transcript.
3. Cancel a long tool request. Completed side effects remain honest and the next task works.
4. Restart the app and restore the same history, usage and plugin enable state.
5. Start CLI and Desktop simultaneously: the same session lock must prevent concurrent writes.
6. Install, enable and configure a tool plugin; idle reload works. Model/context plugin changes require reconnect.
7. Send a file and image; stored history includes file references, not inlined base64.
8. Test 1000 messages, high Token throughput, 10 MB output, RSS of all WebView children, cold start P95, idle CPU and repeat cancellations on all platforms.

## Distribution

The terminal app remains kroma. Desktop is kroma-desktop. Both link the same Rust Core and use the same plugin ecosystem, but are built and downloaded separately.
