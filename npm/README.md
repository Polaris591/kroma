# Kroma

An observable Rust AI coding agent with a terminal UI, line CLI, JSONL events,
MCP tools, Agent Skills and file checkpoints. MIT licensed.

## Install

After the first npm release (these packages are not published yet):

```sh
npm install --global kroma
kroma --version
kroma
```

Node.js 22 or later is required. Rust is not required. Keep optional dependencies
enabled: the matching platform package contains the precompiled executable. There
are no install scripts, runtime binary downloads or automatic update requests.
The npm name is provisional until ownership and publication are confirmed.

Packaging targets: Windows x64; glibc Linux x64/arm64; macOS x64/arm64. CI tests
installation on each target; availability is only guaranteed for platforms whose
packages are present in the release. Alpine/musl, Windows ARM64 and 32-bit systems
are not supported by this npm distribution. Source builds remain an alternative.
Linux binaries inherit the minimum glibc version of the release build runner;
see the repository's distribution guide for the build matrix.

On first interactive launch, follow the model setup wizard. You need access to a
model endpoint (a local Ollama endpoint can also be used). File checkpoints need
Git. For non-interactive use, configure `KROMA_MODEL`, `KROMA_BASE_URL` and
`KROMA_API_KEY` first. Never put credentials in an issue or commit them.

```sh
kroma --help
kroma --print "Explain this repository without changing files"
kroma --json "Summarize the README"
```

Task instructions such as "without changing files" are not a permission barrier.

## Safety

Kroma executes commands and writes files with your user permissions, without
per-command approval. It is not a sandbox. Git-worktree checkpoints do not undo
network effects, database changes, ignored files or changes outside the worktree.
Use a disposable worktree, container or VM for untrusted tasks. Read the full
[security model](https://github.com/Polaris591/kroma/blob/main/docs/security.md).

## Upgrade and uninstall

Close running Kroma sessions before upgrading, especially on Windows:

```sh
npm install --global kroma@latest
npm uninstall --global kroma
```

For a project-local installation use `npm install kroma@latest` in that project.
`npx kroma@latest` runs the requested npm version instead of upgrading a global
installation. Built-in `kroma update` is not implemented yet.

If an older Cargo installation shadows the npm command, inspect `where.exe kroma`
(Windows) or `type -a kroma` (Unix) and correct PATH deliberately. The installer
never deletes other installations. Uninstalling does not erase profiles or sessions.

Source, documentation and issues: <https://github.com/Polaris591/kroma>.
