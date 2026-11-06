# Kroma

A minimal, observable Rust agent harness with a terminal UI, a line CLI, JSONL
events, MCP tools, Agent Skills and recoverable file checkpoints.

**Status: public Beta for the CLI and TUI.** Version 0.4.0. No npm package is
published yet and the npm names are provisional; source builds are the supported
installation path today. The optional Desktop preview is not part of the npm
installer and is not required to use Kroma.

Kroma is deliberately autonomous: it does not ask for per-command permission. It
keeps the agent's work visible, snapshots files automatically in Git worktrees, and
makes rollback explicit. Read [docs/security.md](docs/security.md) before running it
on a repository you care about.

## Project status

| Component | Current state | More information |
| --- | --- | --- |
| Rust CLI, JSONL and TUI | Public Beta; build from source | [Quickstart](docs/quickstart.md) |
| MCP, Agent Skills and process plugins | Implemented; extensions remain opt-in | [Plugins](examples/plugins/README.md) |
| Official Web, Subagents and Project Memory plugins | Source available; disabled by default | [Plugin examples](examples/plugins/README.md) |
| Desktop | Separate preview, not part of the CLI npm installer | [Desktop](desktop/README.md) |
| npm distribution | Five native build targets prepared; not published | [Distribution status](docs/npm-distribution.md) |

See the [project documentation](docs/README.md) for the maintained guides and the
[change history](CHANGELOG.md) for what is still unreleased.

## Quick start

Source installation:

```bash
cargo build --release --locked
./target/release/kroma
```

Requirements: Rust 1.85+ for a source build, a model endpoint and key, and Git for
file snapshots. With no model configured, the first launch opens a guided setup.
Pick a provider, pick or type a model ID, enter the key without echo, optionally
test the connection, then save. Re-run it later with `kroma setup`.

Non-interactive:

```bash
export KROMA_MODEL=deepseek-chat
export KROMA_BASE_URL=https://api.deepseek.com/v1
export KROMA_API_KEY=...
kroma --print "summarize this repository"
```

See [docs/quickstart.md](docs/quickstart.md) for the full first-run flow.

## Why Kroma

- **One runtime, multiple hosts.** The TUI, JSONL/line CLI and optional Desktop
  host share Agent, tools, sessions and compaction; Desktop does not implement a
  second execution loop. Host UI features need not be identical.
- **Scriptable one-shot runs.** `--print` and `--json` use a fresh conversation by
  default and can run in parallel. Continue deliberately with `--continue` or
  `--session`.
- **Local file recovery.** In a Git worktree, Kroma snapshots files before each
  task into `~/.kroma/checkpoints/` using a private `GIT_DIR` and index. `/checkpoints`
  lists snapshots; `/restore-files <n>` restores one explicitly, including removing files created after it. Your `.git`, index,
  HEAD and hooks are untouched.
- **Lazy MCP.** Servers are not started at launch, and their schemas do not consume
  context until the model calls `mcp_search`/`mcp_call`. Third-party logs are hidden
  by default.
- **Context discipline.** Lazy single-pass compaction, prompt caching, provider-reported
  usage, conservative request budgeting and a stable tool prefix.
- **Honest cancellation.** Ctrl+C stops the active model request or shell process
  tree; completed side effects are not claimed as undone. Ctrl+Z restores a cleared
  draft and its attachments.

## Commands

```text
kroma                         Interactive TUI, new conversation
kroma [TASK]                  One task in the line CLI
kroma --print TASK            Fresh one-shot conversation
kroma --json TASK             JSONL events; implies --print
kroma --continue              Resume the latest conversation
kroma --session NAME TASK     Use or create a named conversation
kroma setup                   Guided model setup
kroma config ...              Manage saved profiles
kroma plugins ...             Manage plugins
kroma diagnostics ...         Local support bundle
kroma trust-project           Approve this project's MCP file
kroma update [--check]        Update manually, or only check
kroma help [COMMAND]          Help
```

Unknown options are usage errors, never prompt text. `--` makes the rest literal.
Full reference: [docs/reference/cli.md](docs/reference/cli.md).

## Updates

Updates are manual. Kroma checks nothing at startup and contacts no update service
unless you run `kroma update`. There is no background updater or automatic restart.

```text
kroma update --check                  Report your version and the channel's version
kroma update                          Update on the latest channel
kroma update --channel latest         Same as above; the default channel
kroma update --channel next           Update on the prerelease channel
```

`--check` is read-only: it reads the official npm registry over HTTPS, prints the
available version and changes nothing. For an npm installation (once the packages
are published) the npm launcher handles the update itself, through your npm package
manager and the official HTTPS npm registry, before the native executable starts.
It updates only the invoked installation: global npm installs remain global;
local installs update their owning project, including package.json/lockfile.
Linked, transitive, other-manager or unidentified installs require manual action.
It does not scan PATH or remove other installations. The default channel is
`latest`; use `--channel next` for a Beta published only on that dist-tag.

A Cargo/source build is never overwritten by Kroma: `kroma update` prints the
manual rebuild or `cargo install` command and exits. Project-local updates may modify package.json/lockfile; profiles, sessions and the checkpoint store under `~/.kroma`
are not rewritten.

## MCP and skills

MCP tool discovery is lazy:

- `mcp_search` starts configured servers on demand and lists matching tools.
- `mcp_call` invokes a discovered tool.

Set `KROMA_MCP_EAGER=1` to expose every remote tool directly, `--no-mcp` to skip MCP,
and `KROMA_MCP_STDERR=inherit` to debug server logs. Project MCP files require
`kroma trust-project`.

Agent Skills are discovered from `SKILL.md` files and loaded only when relevant.
See [docs/reference/configuration.md](docs/reference/configuration.md).

## Safety

No approval prompts. Kroma executes tools and commands with your user permissions
and is **not a sandbox**. Visibility, automatic snapshots and explicit rollback
stand in for a permission system. Snapshots cover files in a Git worktree only;
they cannot undo `git push`, database writes, network effects, secrets already sent
or printed, or changes outside the worktree, including files excluded by
`.gitignore`. Use a container, VM or disposable worktree for untrusted tasks.
Details and limits: [docs/security.md](docs/security.md). Vulnerability reporting:
[SECURITY.md](SECURITY.md).

## npm distribution

Precompiled npm packages are prepared for Windows x64, glibc Linux x64/arm64 and
macOS Intel/Apple Silicon, so end-user machines need no Rust toolchain. Nothing is
published yet: `kroma` and the five `kroma-<platform>` package names are provisional
until the owner confirms registry ownership. Publication is meant to go through a
separate maintainer release workflow with provenance; it has not been run for these
packages. Alpine/musl, Windows ARM64, 32-bit systems and other architectures are not
supported by the npm packages; source builds remain available. See
[the distribution guide](docs/npm-distribution.md) for packaging, CI baselines and
local checks, and [releasing](docs/releasing.md) for the maintainer checklist.

## Project documentation and contributing

- [Documentation index](docs/README.md) — user guides, architecture, extension references and historical research.
- [Contributing](CONTRIBUTING.md) — development checks and change scope.
- [Changelog](CHANGELOG.md) — curated changes and release status; not a published-release claim.
- [Issues](https://github.com/Polaris591/kroma/issues) — reproducible bugs and focused feature proposals.
- [Maintainer operations](docs/maintainer-operations.md) — release gates and GitHub settings.

## Development

```bash
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
```

Platform-specific behavior and the long design history are in
[docs/design-notes.md](docs/design-notes.md); lifecycle details are in
[docs/lifecycle.md](docs/lifecycle.md). For runtime ownership and extension
boundaries, see [architecture](docs/architecture.md). For opt-in local
measurements and their limits, see [performance](docs/performance.md).

## License

MIT. See [LICENSE](LICENSE).

中文说明与快速开始：[README.zh-CN.md](README.zh-CN.md)
