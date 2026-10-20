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
3. **Best-of-N Worktree Tournament (`/race`)**: Spawns isolated Git worktrees in parallel, pits candidates against each other, disqualifies zero-change patches and test-tampering attempts via `ProtectedPaths`, and merges the winner with `MergeIntent` rollback guarantees. A worktree is a *git* isolation boundary, not an OS security boundary: it stops two candidates from editing the same files, not a command from reading `$HOME`. The self-repair experimenter (`docs/CAPABILITIES.md`, `ahe_isolation`) only ever produces a candidate patch and a report — promotion is a human decision taken after an independent regression, never automatic.
4. **Adaptive Escalation**: Automatically escalates to a multi-candidate tournament when a single agent stalls on verification failures (`/config set adaptive_tournament 3`).
5. **A/B Benchmark Harness (`kroma bench`)**: Paired, total-budget-locked evaluation platform. With no `--suite` it runs the built-in smoke suite against a deterministic **synthetic** executor: that validates the measurement plumbing (budget locking, token accounting, aggregation, paired comparison) on a machine with no model endpoint, and every such report is labelled `synthetic`. It is not evidence that a strategy resolves more real tasks — that needs a real suite plus a provider-backed executor. The additional **BF-14B** operator-controlled whole-process bwrap supervisor (`scripts/bench-sandbox-supervisor.js`, see `docs/benchmark/BF14B_KERNEL_SANDBOX_AND_RAW_EVIDENCE.md`) currently supports **offline synthetic execution only**; automatic paid Provider evaluation remains fail-closed until credential brokering and separate held-out scoring are implemented.

---

## 🔐 Security Boundaries

The workspace **file tools** (`read` / `write` / `edit` / `grep` / `find` / `ls`) resolve every path through `openat2(RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS)` on Linux and an equivalent no-reparse-point path on Windows, so they cannot escape the workspace root or be redirected through a symlink. The snapshot store, session journal and config files are created with `O_NOFOLLOW` and their permissions are set with `fchmod` on the open descriptor rather than by path.

**Opt-in isolation exists, and it is opt-in.** Setting `execution_sandbox = "bwrap"` (or
`container:<runtime>:<image>`) in `~/.kroma.toml`, or `KROMA_EXECUTION_SANDBOX`, runs the agent's
`bash` inside that boundary: a `bwrap` namespace with only the workspace writable and no network, or a
named container tracked by a cidfile so a cancelled run stops the workload rather than just the
client. When the boundary is required and the backend is unavailable the command is refused — it is
never silently run on the host. Today only the agent's `bash` goes through this policy; verifier,
probe and hook spawns are not wired into it yet, and the default is host mode. See
`docs/CAPABILITIES.md` for the per-capability evidence and limits.

The **`bash` tool is not an OS-level sandbox by default.** It runs the command as your own user with the workspace as the working directory, and a shell reaches everything your account reaches — files outside the workspace and the network included. Approving a command approves it at your account's privileges; it does not confine it to the workspace. Treat `bash` the way you would your own terminal: fine for repositories you trust, and run Kroma inside a container or VM when the code under test is untrusted.

---

## 📜 License

MIT License © 2026 Polaris & Kroma Contributors.

---

## Experimental NextGen Cache / Tools / Compaction (2026-10-20)

**Source delivery only; Rust tests and real-provider conformance are not yet verified.**
All new cache and proactive compaction behaviors are disabled by default. To opt in
from a trusted operator environment, see `docs/NEXTGEN_CACHE_TOOLS_COMPACTION_20261020.md`.

```sh
export KROMA_OPENAI_CACHE_MODE=explicit       # off (default), implicit or explicit
export KROMA_OPENAI_CACHE_TRACE=1             # no prompt contents logged
export KROMA_CACHE_AWARE_COMPACTION=1         # experimental opt-in; hard budget always wins
sh scripts/nextgen_rust_gate.sh                # MUST pass before release (requires Rust)
```

Trusted global MCP config can use `http_url` and `bearer_token_env` to enable
stateless MCP 2026-07-28; legacy stdio remains available. OpenAI/Claude native
`tool_search`, MRTR and provider-native compaction are **not** part of this patch.

---

## Experimental NextGen Cache / Tools / Compaction (2026-10-20)

**Source delivery only; Rust tests and real-provider conformance are not yet verified.**
All new cache and proactive compaction behaviors are disabled by default. To opt in
from a trusted operator environment, see `docs/NEXTGEN_CACHE_TOOLS_COMPACTION_20261020.md`.

```sh
export KROMA_OPENAI_CACHE_MODE=explicit       # off (default), implicit or explicit
export KROMA_OPENAI_CACHE_TRACE=1             # no prompt contents logged
export KROMA_CACHE_AWARE_COMPACTION=1         # experimental opt-in; hard budget always wins
sh scripts/nextgen_rust_gate.sh                # MUST pass before release (requires Rust)
```

Trusted global MCP config can use `http_url` and `bearer_token_env` to enable
stateless MCP 2026-07-28; legacy stdio remains available. OpenAI/Claude native
`tool_search`, MRTR and provider-native compaction are **not** part of this patch.
