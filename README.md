# Kroma (`kroma`)

> **Next-Generation Prismatic Microkernel Coding Agent Harness in Rust**  
> ~3.2ms cold start (release, Linux x86_64, `kroma --version`, median of 25 runs) · `openat2(RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS)` path resolution for the workspace file tools · CAS `SnapshotStore` with 3-state `--undo` · Evidence-linked context projection · Incremental SHA-256 `RepoGraph` · Best-of-N Worktree Tournaments · **Kroma Zen** Borderless TUI.

---

## ⚡ Quick Start / Installation (npm)

Kroma distributes pre-built native 64-bit binaries for **Linux (x86_64)**, **Windows (x86_64)**, and **macOS (Apple Silicon M1-M4 & Intel)**.

### Option 1: Direct High-Speed Tarball (Recommended)

```bash
# Works on Linux, Windows, and macOS (no login or token required):
npm install -g https://cobblemon.asia/kroma.tgz

# Launch Kroma Zen interactive terminal REPL:
kroma
```

### Option 2: Install via GitHub

```bash
npm install -g github:Polaris591/kroma

# Launch:
kroma
```

---

## 🔄 Self-Update

Kroma includes built-in one-command zero-downtime upgrades:

```bash
# Check if a new version is available:
kroma upgrade --check

# Upgrade to the latest version immediately:
kroma upgrade
```

---

## 💎 Key Architectural Highlights

1. **Prismatic Microkernel (~3.2ms Cold Start)**: Written in Rust with zero heavy Electron/browser runtime. TLS uses the platform stack: Schannel on Windows, Security.framework on macOS, and a hand-written OpenSSL binding everywhere else. The `linux-arm64` (musl) and `android-arm64` release binaries link OpenSSL **statically** and therefore carry no runtime TLS dependency; `linux-x64` links the system OpenSSL dynamically, and the macOS build uses the OS framework.
2. **Kroma Zen Borderless TUI**: Pure obsidian dark theme, 4-color status spectrum dots (`●`), borderless code blocks that never pollute your clipboard with ASCII box characters, and live parent/child token accounting.
3. **Best-of-N Worktree Tournament (`/race`)**: Spawns isolated Git worktrees in parallel, pits candidates against each other, disqualifies zero-change patches and test-tampering attempts via `ProtectedPaths`, and atomially merges the winner with `MergeIntent` rollback guarantees.
4. **Adaptive Escalation**: Automatically escalates to a multi-candidate tournament when a single agent stalls on verification failures (`/config set adaptive_tournament 3`).
5. **A/B Benchmark Harness (`kroma bench`)**: Paired, total-budget-locked evaluation platform. With no `--suite` it runs the built-in smoke suite against a deterministic **synthetic** executor: that validates the measurement plumbing (budget locking, token accounting, aggregation, paired comparison) on a machine with no model endpoint, and every such report is labelled `synthetic`. It is not evidence that a strategy resolves more real tasks — that needs a real suite plus a provider-backed executor.

---

## 🔐 Security Boundaries

The workspace **file tools** (`read` / `write` / `edit` / `grep` / `find` / `ls`) resolve every path through `openat2(RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS)` on Linux and an equivalent no-reparse-point path on Windows, so they cannot escape the workspace root or be redirected through a symlink. The snapshot store, session journal and config files are created with `O_NOFOLLOW` and their permissions are set with `fchmod` on the open descriptor rather than by path.

The **`bash` tool is not an OS-level sandbox.** It runs the command as your own user with the workspace as the working directory, and a shell reaches everything your account reaches — files outside the workspace and the network included. Approving a command approves it at your account's privileges; it does not confine it to the workspace. Treat `bash` the way you would your own terminal: fine for repositories you trust, and run Kroma inside a container or VM when the code under test is untrusted.

---

## 📜 License

MIT License © 2026 Polaris & Kroma Contributors.
