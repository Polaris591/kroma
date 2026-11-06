# Changelog

Notable changes to Kroma are tracked here. This is a curated summary, **not** proof of a published binary or npm release. For exact history, consult [commits](https://github.com/Polaris591/kroma/commits/main) and [tags](https://github.com/Polaris591/kroma/tags).

The source currently declares version `0.4.0` in `Cargo.toml`. No public npm package is documented as published. This file deliberately does not claim a release date or a production certification.

## Unreleased

### Added
- Opt-in server-side `web_search` for Responses profiles (`native_web_search`): the endpoint runs the search, Kroma shows it through the existing tool activity view, keeps real source URLs with the turn and reports provider-supplied search counts.
- Public Beta CLI/TUI documentation, configuration and first-run setup.
- Shared Rust runtime for CLI, TUI and optional Desktop preview.
- Opt-in process plugin host with tool hooks, model and context backends, streaming, event delivery and lifecycle contracts.
- Official plugin sources for Web Search & Fetch, Project Memory and Subagents, with separate test suites and distribution preparation.
- Session journals, file checkpoints, recovery and context management diagnostics.
- Cross-platform CI, native npm-package staging tests and maintainer-gated release workflows.

### Changed
- Context compression and cache/usage diagnostics received further correctness and regression coverage.
- Plugin architecture, child-process handling and session/task notifications were refined.
- The terminal UI gained additional rendering, interaction and transcript tests.

### Known limitations
- The Desktop host is a separate preview, not part of the CLI npm distribution.
- npm package names remain provisional until ownership and publication are verified.
- Real provider/gateway compatibility, performance, hardware-specific UI behavior and package publication require explicit release verification.
- Kroma intentionally executes commands without per-tool approval and is not a sandbox. See [security](docs/security.md).

Before publishing a release, create a dated version section with changes verified against the release tag and update the installation/status text in both READMEs.
