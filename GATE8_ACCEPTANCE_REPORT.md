# Kroma v1.3.0 — Gate 8 Acceptance Report

**Version:** `1.3.0` (`Cargo.toml` workspace version, `kroma --version`, Git tag `v1.3.0`)
**Scope:** persistent layered configuration + live hot-switching, and the Best-of-N worktree
tournament engine.

## 1. Layered configuration (`crates/kroma-core/src/config_file.rs`)

- New module exported from `lib.rs`; zero new dependencies (hand-rolled TOML subset parser, matching
  the project's hand-rolled base64 reader).
- Files: user global `~/.kroma/config.toml` (Windows `%USERPROFILE%\.kroma\config.toml`) and
  workspace `<workspace>/.kroma.toml`. Home resolution: `KROMA_HOME` > `HOME` > `USERPROFILE` >
  `getpwuid` account database (so daemons without `HOME` still find their config).
- Precedence (highest first): **CLI flags > `KROMA_*` env vars > workspace `.kroma.toml` >
  selected profile in the user global file > top-level defaults in the user global file.**
- **Trust boundary:** a workspace file may only contribute `model`, `protocol`,
  `reasoning_effort` and `max_subagents`. Every other key — including `api_key`, `base_url`,
  `verify_command`, `policy`, `policy_hook`, `observer_hook`, `allow_write`, `allow_shell` and any
  `[profiles.*]` table — is dropped and reported in `ResolvedConfig::rejected_workspace_keys`.
- `save_user_global()` writes atomically (temp file + rename) with `0600` on Unix and `0700` on the
  containing directory.
- `/config show` redacts the credential as `sk-***`.

## 2. Live hot-switching (`crates/kroma-core/src/surface.rs`, `crates/kroma-cli/src/main.rs`)

- New `ProviderFactory` / `ProviderSwap` seam: the kernel owns swap mechanics, the host supplies
  credentials and endpoint, so `kroma-core` never learns how the operator configured them.
- RPC: `config.get`, `config.set` / `session.configure`, `sessions.list`, `session.resume`,
  `session.tournament`. Each swap broadcasts a `kroma/event` `config_updated` notification.
- REPL slash commands: `/model [<id>] [--save]`, `/thinking [<lvl>] [--save]`,
  `/protocol [<name>]`, `/config [show|set <k> <v>|profile <name>]`, `/sessions`,
  `/resume [<latest|id>]`, `/new`, `/race <N> <task>`, plus the existing command set.
- `/model high` and `/thinking <model-id>` are detected as mix-ups and print a hint pointing at the
  correct command instead of silently corrupting state.
- The startup banner now shows `model`, `thinking`, `protocol`, `workspace` and the key command list.

## 3. Best-of-N worktree tournament (`crates/kroma-core/src/subagents.rs`, `agent_loop.rs`)

- `TournamentSpec`, `TournamentCandidate`, `TournamentScore`, `TournamentOutcome`,
  `run_worktree_tournament` (2–4 `Worker` candidates; a `Scout` candidate is rejected).
- Every candidate forks from the same baseline revision into its own `wt-race-000N` worktree and
  runs in parallel. Verification (`WorkspaceCompletionGate`) and `PredictiveValidator` both run
  **inside the candidate's worktree**, before the parent is touched.
- Deterministic ranking (`TournamentScore::rank_key`), lexicographic:
  1. `gate_passed` (a candidate that failed verification can never win),
  2. `predictive_warnings == 0`,
  3. `conflict_free_with_parent`,
  4. minimal `total_changed_lines`, then fewest `files_modified`, then fewest `turns_used`, then
     candidate index.
- Only the winner is merged; the merge captures one clean parent `WorkspaceRevision` so `/undo`
  reverts the whole tournament in one step. Every loser worktree (and the winner's) is removed.
- **If no candidate passes verification, zero candidates are merged and the parent workspace is
  byte-for-byte untouched.**
- Exposed as `spawn_subagent` `"mode": "tournament"` (or `"tournament": true`) with an optional
  `candidates` count, as the REPL `/race <N> <task>` command, and as the `session.tournament` RPC
  method. Results are rendered as `<subagent_tournament_report>`.

## 4. Verification

- `cargo test --workspace` → **173 passed; 0 failed; 0 warnings** (19 test binaries), including the
  new `crates/kroma-core/tests/gate8_config_and_tournament.rs` (8 tests).
- `cargo check --workspace --target x86_64-pc-windows-gnu --all-targets` → clean.
- `cargo build --workspace --release` and `--target x86_64-pc-windows-gnu --release` → both clean.
- Zero-environment smoke test: with no `KROMA_*` variables and no `HOME`, `kroma repl` starts,
  reports `model: gemini-3.8-flash-high | thinking: high | protocol: chat-completions` from
  `~/.kroma/config.toml`, and `/model`, `/thinking`, `/config show`, `/sessions`, `/resume`, `/new`
  all work.

## 5. Residual risks

- The TOML parser is a deliberate subset (no arrays, no inline tables, no multi-line strings).
  Unsupported syntax is a hard error rather than a silent default, so a typo cannot change behaviour
  quietly; unknown keys inside a supported table are ignored but remembered by the trust guard.
- `approximate_changed_lines` is a line-multiset difference, not an LCS. It is deterministic and
  O(n), which is what the ranking needs, but it can over-count for reordered blocks.
- The tournament spawns up to `MAX_PARALLEL_SUBAGENTS` (4) child agent loops concurrently, each with
  its own token budget; cost scales linearly with the candidate count.
