# Kroma CLI (`kroma-cli`)

**Kroma** is a next-generation prismatic microkernel coding agent harness written in Rust (~3.2ms cold start on Linux x86_64, median of 25 `kroma --version` runs; `openat2(RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS)` path resolution for the workspace file tools; content-addressed `SnapshotStore` with 3-state `--undo`; evidence-linked context projection; incremental SHA-256 `RepoGraph`; Best-of-N isolated worktree tournaments; and the borderless **Kroma Zen** TUI).

Two boundaries worth knowing before you install:

- The **`bash` tool is not an OS-level sandbox.** It runs commands with your account's privileges and the workspace as its working directory, so a shell can reach anything your account can reach. Approving a command approves it at your privileges, not within the workspace.
- TLS goes through the platform stack via `native-tls` (OpenSSL on Linux/Android, Schannel on Windows, Security.framework on macOS), so a system TLS library is a runtime requirement.

## Quick Install

```bash
# Install globally from npm registry:
npm install -g kroma-cli

# Or install directly from the standalone all-in-one tarball:
npm install -g ./kroma-cli-1.5.0-standalone.tgz

# Launch interactive Kroma Zen REPL:
kroma
```
