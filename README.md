# Kroma

**Kroma** is a next-generation lightweight Rust runtime for coding agents: a single ~17 MB static binary that
runs headless in CI, as an interactive terminal TUI, as an editor ACP stdio server, or as a local
JSON-RPC daemon — all backed by the same kernel (sandboxed workspace, content-addressed snapshots,
evidence-linked context projection, predictive validation, and isolated-worktree subagents).

```console
$ kroma --version
kroma 1.2.0
```

## Install

**Linux / macOS**

```bash
./install.sh            # builds with cargo, installs `kroma` into /usr/local/bin (or ~/.kroma/bin)
```

**Windows (PowerShell)**

```powershell
.\install.ps1           # installs kroma.exe into %USERPROFILE%\.kroma\bin and updates your PATH
```

Then open any project folder and just type `kroma` — it launches the interactive TUI in the current
directory. For a one-off task: `kroma --task "add tests for the parser"`.

## Surfaces

| Command | Description |
| --- | --- |
| `kroma` / `kroma repl` / `kroma tui` | Interactive multi-turn terminal REPL/TUI (`/help`, `/status`, `/graph`, `/model`, `/undo`, `/revisions`, `/skills`, `/fork`) |
| `kroma run --task <TASK>` | Headless single-run mode (default; `--output-format jsonl` for CI) |
| `kroma acp` | JSON-RPC 2.0 Agent Client Protocol server over stdio (stdout stays pure JSON-RPC) |
| `kroma serve --listen 127.0.0.1:7878` | JSON-RPC 2.0 daemon over loopback TCP or a `0600` Unix socket |

## Configuration

Kroma reads configuration from environment variables (and, for scripts, from the CLI flags shown by
`kroma --help`):

| Variable | Purpose |
| --- | --- |
| `KROMA_API_KEY` | Provider API key (stripped from child shell environments) |
| `KROMA_BASE_URL` | OpenAI-compatible base URL (HTTPS required for remote hosts) |
| `KROMA_MODEL` | Model identifier |
| `KROMA_PROTOCOL` | Provider protocol fallback when `--protocol` is not set: `auto` \| `chat-completions` \| `anthropic-messages` \| `openai-responses` |
| `KROMA_REASONING_EFFORT` | Reasoning effort fallback when `--reasoning-effort` is not set |
| `KROMA_VERIFY_COMMAND` | Completion-check command, e.g. `cargo test --workspace` |
| `KROMA_VERIFY_TIMEOUT_SECS` | Completion-check timeout (default 180) |
| `KROMA_GATE_MAX_RETRIES` | Maximum completion-gate rejections (default 3) |
| `KROMA_SHELL` | Shell used for the `bash` tool (Windows: Git Bash / pwsh / cmd auto-detected) |
| `KROMA_CA_BUNDLE` | Extra CA bundle for TLS verification |

## Kernel capabilities

- **Sandboxed workspace** — kernel-enforced path confinement (`openat2 RESOLVE_BENEATH` on Linux,
  reparse-point + final-handle checks on Windows), sensitive-path guard, and a 200 ms pipe-drain /
  process-group kill discipline for supervised shell commands.
- **Snapshot store & undo** — content-addressed revisions with a three-state restore
  (`Applied` / `Conflict` / `RecoveryRequired`) and crash-safe intent journal.
- **Evidence-linked context projection** — deterministic budgets, `source_entry_id` evidence graph,
  and on-demand raw retrieval via the `recall_evidence` tool.
- **Predictive validation & attribution** — in-process structural checks plus candidate failure
  attribution with zero automatic rollback.
- **Controlled subagents** — read-only `Scout` and isolated-worktree `Worker` agents with three-way
  baseline-aware merge conflict detection and post-merge completion verification.
- **Symbol graph navigation** — cross-file `RepoGraph` with personalized PageRank (`code_graph` tool, `/graph` REPL command).
- **Multimodal read** — PNG/JPEG/GIF/WebP images are decoded into vision model content blocks.

## Development

```bash
cargo test --workspace                     # 165 tests
cargo check --workspace --target x86_64-pc-windows-gnu --all-targets
cargo build --workspace --release          # -> target/release/kroma
```

## License

MIT
