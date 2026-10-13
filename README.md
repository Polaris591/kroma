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

## Configuration (`v1.3.0`)

Kroma reads layered configuration so it runs with zero environment variables:

| Layer (highest first) | Location |
| --- | --- |
| CLI flags | `--model`/`--profile`/`--protocol`/`--reasoning-effort`/`--max-subagents` |
| Environment | `KROMA_*` variables |
| Workspace file | `<workspace>/.kroma.toml` (untrusted: `model`, `protocol`, `reasoning_effort`, `max_subagents` only) |
| User file | `~/.kroma/config.toml` (trusted; written `0600`) |

`~/.kroma/config.toml` also holds named profiles:

```toml
default_profile = "gemini"
model = "deepseek"
base_url = "https://example.com/wb/v1"
api_key = "sk-..."

[profiles.gemini]
base_url = "https://example.com/v1"
model = "gemini-3.8-flash-high"
protocol = "chat-completions"
reasoning_effort = "high"
```

A workspace `.kroma.toml` can never set credentials, endpoints, hooks, policy or verification
commands; rejected keys are reported on stderr and by `/config show`.

## Best-of-N worktree tournament (`v1.3.0`)

Race 2–4 `Worker` candidates on the same task, each in its own isolated worktree. Every candidate is
verified *inside* its worktree (completion gate + predictive validation) and ranked deterministically
(gate passed → zero structural warnings → conflict-free → smallest diff). Only the winner is merged,
the merge is a single revision that `--undo` reverts in one step, and every loser worktree is
removed. If no candidate passes verification the parent workspace is untouched.

```console
kroma> /race 3 fix the flaky parser test
```

Also available as `spawn_subagent {"mode":"tournament","task":"...","candidates":3}` and as the
`session.tournament` JSON-RPC method.

## Interactive commands (`kroma>`)

`/model [<id>] [--save]`, `/thinking [<lvl>] [--save]`, `/protocol [<name>]`,
`/config [show|set <k> <v>|profile <name>]`, `/sessions`, `/resume [<latest|id>]`, `/new`,
`/race <N> <task>`, `/status`, `/revisions`, `/undo`, `/diff`, `/skills`, `/graph`, `/fork`,
`/inspect`, `/help`, `/quit`.

## Development

```bash
cargo test --workspace                     # 165 tests
cargo check --workspace --target x86_64-pc-windows-gnu --all-targets
cargo build --workspace --release          # -> target/release/kroma
```

## License

MIT
