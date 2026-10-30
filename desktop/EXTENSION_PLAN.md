# Desktop UI extension contract (P3–P4, not yet implemented)

The existing Rust PluginCatalog is the sole source of plugin installation, activation, settings, and process ownership. P2 already exposes it via GUI.

Future GUI plugins can opt into versioned settings-page and panel entrypoints (loaded only when visible). Additional host-owned slots cover themes, layouts, message and tool renderers, composer, shortcuts, animation, and full workspace replacement.

Use a framework-neutral Web Component mount contract and a small stable Host API for session snapshots, events and actions. Only one plugin may own a full-workspace replacement, while independent local slots may coexist. A broken GUI plugin must be unloadable without losing the default UI.

Disabled plugins do not run background rendering, polling or animations. Plugins never replace Agent Loop, mutate the WAL directly, or require default approval screens. Stabilize and performance-test the ABI before opening third-party UI modules.

P2 intentionally implements automatic host-rendered settings forms only.
