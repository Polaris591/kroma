# Kroma documentation

Start at the [English README](../README.md) or [中文说明](../README.zh-CN.md). The entries below distinguish **current user guides**, **maintainer references** and **historical design notes**.

## Use Kroma

| Guide | Contents |
| --- | --- |
| [Quickstart](quickstart.md) | Build from source, configure a model, start a session |
| [CLI reference](reference/cli.md) | Commands, options and scripting |
| [Configuration](reference/configuration.md) | Profiles, environment variables and provider settings |
| [Plugin guide](../examples/plugins/README.md) | Installation, tools and extension examples |
| [Security model](security.md) | No-approval operation, isolation and snapshot limits |
| [Desktop preview](../desktop/README.md) | Optional Svelte/Tauri host; separate prerequisites |

## Contribute and maintain

| Guide | Contents |
| --- | --- |
| [Contributing](../CONTRIBUTING.md) | Development checks, pull requests and architecture principles |
| [Maintainer operations](maintainer-operations.md) | GitHub settings, issue triage, CI ownership and release gates |
| [Architecture](architecture.md) | Runtime ownership and module boundaries |
| [Lifecycle](lifecycle.md) | Task transitions, event ordering and cancellation |
| [Performance](performance.md) | Opt-in local measurements and interpretation |
| [Release checklist](releasing.md) | Maintainer-only release procedure; not proof of publication |
| [npm distribution](npm-distribution.md) | Native targets, staging and installation tests |
| [Security reporting](../SECURITY.md) | Private vulnerability reports |
| [Changelog](../CHANGELOG.md) | Curated unreleased changes and future release notes |

## Design and research material

These documents are retained for context, and some record proposals, experiments or superseded decisions. They are **not** a replacement for current source, tests or user-facing guides.

- [Design notes](design-notes.md)
- [Model catalog](model-catalog.md)
- [Project Memory design](project-memory-design.md), [V2 contract](project-memory-v2-contract.md), and [zero-maintenance notes](project-memory-zero-maintenance.md)
- [Subagent design](subagents-design.md) and [1.0 design](subagents-1.0-design.md)
- [Historical deployment notes](promotion-linux-do.md)
- [Plugin roadmap](../PLUGIN_ROADMAP.md), [benchmark archive](../benchmarks/README.md) and [review archive](../reviews/README.md)

Use the repository's `main` source and tests as the final authority when historical documents disagree.
