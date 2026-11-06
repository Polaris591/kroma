# Contributing to Kroma

Thanks for helping improve Kroma. Kroma aims to be a small, observable Rust coding-agent harness: one Agent loop, clear ownership boundaries, reliable sessions, and opt-in extensions. Contributions should preserve that simplicity.

## Before opening a change

- For bugs, search existing issues and include a minimal reproduction.
- For behavior changes or broad architecture proposals, open a discussion issue first. Describe the user-facing problem and the smallest change that would solve it.
- Keep unrelated fixes in separate pull requests. Do not bundle refactors with behavior changes unless necessary.
- Do not commit API keys, private prompts, conversation archives, diagnostic bundles, generated credentials, or proprietary project data. Follow [SECURITY.md](SECURITY.md) for vulnerabilities.

## Development setup

Use Rust **1.85+** and Node.js **22+** for the JavaScript plugin and npm tests. Git is required to exercise file checkpoints. Install platform prerequisites for Desktop only if working on that host.

```sh
git clone https://github.com/Polaris591/kroma.git
cd kroma
cargo build --locked
cargo test --locked
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
node --test examples/plugins/web/web.test.js examples/plugins/web/http.test.js examples/plugins/web/main.test.js
node --test examples/plugins/subagents/subagent.test.js examples/plugins/subagents/main.test.js
npm --prefix npm test
python3 scripts/check_doc_links.py
```

The full CI matrix runs Rust tests and Clippy on Linux, macOS and Windows. Project Memory and Desktop have **separate** checks; consult their documentation and workflows instead of assuming the root Cargo command covers them. In environments that set `NO_COLOR`, the TUI color-assertion tests can fail; unset it for tests explicitly expecting colored rendering.

To work on the optional memory worker:

```sh
cargo test --locked --manifest-path examples/plugins/project-memory/Cargo.toml
cargo clippy --locked --manifest-path examples/plugins/project-memory/Cargo.toml --all-targets -- -D warnings
```

For Desktop, see [desktop/README.md](desktop/README.md). For benchmarks, see [docs/performance.md](docs/performance.md); do not treat mock benchmarks as real model performance measurements.

## Design boundaries

- The CLI, TUI and Desktop reuse the same Rust Agent runtime. Avoid a second Agent loop, generic dependency-injection framework, or mandatory plugin daemon.
- Core retains authoritative transcript/WAL, task ordering, provider-response validation, cancellation and context invariants. Plugins may extend or explicitly replace defined behaviors.
- Plugins remain disabled by default; avoid adding approval prompts or mandatory extensions.
- Avoid unnecessary model calls, extra IPC on the no-plugin path, and changes that invalidate stable prompt/cache prefixes without evidence.
- Do not discard historical evidence to reduce context: distinguish authoritative history from request projections.
- State the supported operating systems, compatibility assumptions and failure semantics. A passing mock fixture is not a live-provider certification.

See [architecture](docs/architecture.md), [lifecycle](docs/lifecycle.md) and [security model](docs/security.md).

## Pull requests

Base feature and maintenance PRs on `main`. Include the problem, implementation, observable behavior, tests actually run (with results), compatibility impact and any remaining risks. If performance is the motivation, supply reproducible before/after results on identical tasks, models, endpoints and settings.

Prefer focused commits with clear messages such as `fix:`, `feat:`, `docs:`, `test:` or `refactor:`. Keep the PR open until its checks finish; avoid claiming CI passed before it does. Releases and npm publication are maintainer-operated and are **not** triggered by ordinary contributions.

## Project references

- [Getting started](docs/quickstart.md)
- [CLI reference](docs/reference/cli.md) and [configuration](docs/reference/configuration.md)
- [Plugin guide](examples/plugins/README.md)
- [Documentation index](docs/README.md)
- [Release procedure](docs/releasing.md)

MIT-licensed contributions are made under the [project license](LICENSE).
