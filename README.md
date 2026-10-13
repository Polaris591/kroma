# Kroma (`kroma`)

> **Next-Generation Prismatic Microkernel Coding Agent Harness in Rust**  
> ~4.2ms cold start · `openat2` & Windows Job Object kernel sandboxes · CAS `SnapshotStore` with 3-state `--undo` · Evidence-linked context projection · Incremental SHA-256 `RepoGraph` · Best-of-N Worktree Tournaments · **Kroma Zen** Borderless TUI.

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

1. **Prismatic Microkernel (~4.2ms Cold Start)**: Written in 100% pure Rust with zero heavy Electron/browser runtime bloat.
2. **Kroma Zen Borderless TUI**: Pure obsidian dark theme, 4-color status spectrum dots (`●`), borderless code blocks that never pollute your clipboard with ASCII box characters, and live父子 Token accounting.
3. **Best-of-N Worktree Tournament (`/race`)**: Spawns isolated Git worktrees in parallel, pits candidates against each other, disqualifies zero-change patches and test-tampering attempts via `ProtectedPaths`, and atomially merges the winner with `MergeIntent` rollback guarantees.
4. **Adaptive Escalation**: Automatically escalates to a multi-candidate tournament when a single agent stalls on verification failures (`/config set adaptive_tournament 3`).
5. **A/B Benchmark Harness (`kroma bench`)**: Paired, total-budget-locked evaluation platform to test and prove architectural strategies.

---

## 📜 License

MIT License © 2026 Polaris & Kroma Contributors.
