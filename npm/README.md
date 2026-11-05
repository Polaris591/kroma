# Kroma

An observable Rust AI coding agent with a terminal UI, line CLI, JSONL events,
MCP tools, Agent Skills and file checkpoints. MIT licensed. **Public Beta.**

This is the npm launcher package for Kroma. The native executable is installed by
the matching `kroma-<platform>` optional dependency; no Rust toolchain is needed.

## Install

These packages are not published yet, and the npm names are provisional until the
owner confirms registry ownership. Once a release exists:

```sh
npm install --global kroma@next
kroma --version
# Beta updates: kroma update --channel next
kroma
```

Node.js 22 or later is required. Keep optional dependencies enabled: the platform
package contains the precompiled executable. There are no install scripts and no
runtime binary downloads outside an update you request yourself. Do not install
with `--omit=optional`/`--no-optional`; the launcher then stops with an
installation error instead of fetching a binary.

Packaging targets: Windows x64; glibc Linux x64/arm64; macOS x64/arm64. Alpine/musl,
Windows ARM64 and 32-bit systems are not supported by this npm distribution. Source
builds remain an alternative. Linux binaries inherit the minimum glibc version of
the release build runner (Ubuntu 22.04 x64, Ubuntu 24.04 arm64); the repository's
distribution guide has the exact matrix.

On first interactive launch, follow the model setup wizard. You need access to a
model endpoint (a local Ollama endpoint works too). File checkpoints need Git. For
non-interactive use, set `KROMA_MODEL`, `KROMA_BASE_URL` and `KROMA_API_KEY` first.
Never put credentials in an issue or commit them.

```sh
kroma --help
kroma --print "Explain this repository without changing files"
kroma --json "Summarize the README"
```

Task instructions such as "without changing files" are not a permission barrier.

## Updates

Updates are manual:

```sh
kroma update --check             # read-only; prints the available version
kroma update                     # update on the latest channel
kroma update --channel next      # prerelease channel, when one is published
```

Kroma performs no startup check and no background update request. For an npm
installation the launcher handles the update before the native executable starts:
it uses your npm package manager against the official HTTPS npm registry and
nothing else. Global installs remain global; local installs update the owning
project's package.json/lockfile, not an arbitrary current directory. Linked,
transitive, other-manager or unidentified installations require manual action.
It does not scan PATH or remove other installations. Missing platform packages
can be repaired by npm; successful updates verify both package versions.
Profiles, sessions and checkpoints under `~/.kroma` are not modified.

You can always update through npm directly, which is equivalent:

```sh
npm install --global kroma@latest
```

Close running Kroma sessions before upgrading, especially on Windows. For a
project-local install, run `npm install kroma@latest` in that project.
`npx kroma@latest` runs the requested npm version instead of upgrading a global
installation. If an older Cargo installation shadows the npm command, inspect
`where.exe kroma` (Windows) or `type -a kroma` (Unix) and correct PATH deliberately;
the installer never deletes other installations.

## Safety

Kroma executes commands and writes files with your user permissions, without
per-command approval. It is not a sandbox. Git-worktree checkpoints do not undo
network effects, database changes, ignored files or changes outside the worktree.
Use a disposable worktree, container or VM for untrusted tasks. Read the full
[security model](https://github.com/Polaris591/kroma/blob/main/docs/security.md).

## Uninstall

```sh
npm uninstall --global kroma
```

Uninstalling removes the launcher and its platform package; it does not erase
profiles, sessions or checkpoints.

Source, documentation and issues: <https://github.com/Polaris591/kroma>.
Maintainer release checklist and known limits: [releasing](https://github.com/Polaris591/kroma/blob/main/docs/releasing.md).
