# Kroma PR #13 post-merge validation — Windows, 2026-11-10

## Merge and build provenance

- PR [#13](https://github.com/Polaris591/kroma/pull/13) was merged after all required checks were successful: Core, Project Memory, npm distribution, Desktop, and documentation links.
- Merge SHA: `ce0b38641eb1c323a81bf30ad4dc7abcedb3acbb`; main branch verified against this SHA.
- Release built from **a fresh isolated checkout of exactly this main commit**, not from the original user checkout:
  `CARGO_TARGET_DIR=D:/Kroma/target-kroma-check cargo build --release --locked`.
- Windows merged-main full tests: **777 passing Rust unit tests, 11 ignored**, 59 passing plugin E2E tests, other integration suites green. `cargo fmt --check`, `clippy -D warnings`, and GitHub cross-platform CI were also green.
- `--build-info`: `kroma 0.4.0 (ce0b38641eb1)`, no dirty working tree.
- Portable Windows executable: `D:/Kroma/experiments/kroma-release-ce0b386/kroma.exe`.
- Portable archive: `D:/Kroma/experiments/kroma-release-ce0b386/kroma-windows-x64-ce0b386.zip`.
- SHA-256 of executable: `ee42cafb5e3914fcf541f0d15f1b3e77a9107b1ebadc6f5001d67c1d8425a995`.
- This package is a **portable smoke-tested artifact**, not a signed MSI installer or automatic update.
- Original `D:/Kroma/kroma` working tree and current user PATH/installed executable were not modified.

## Windows terminal (native ConPTY)

Script: `benchmarks/round5_tui_conpty.py`, run in an ephemeral uv environment with `pywinpty` + `pyte`.
Result: `D:/Kroma/experiments/kroma-tui-conpty-merged/results.json`.

Passes:
1. Launch and render the real merged-main `kroma.exe` inside a native Windows ConPTY.
2. Unicode and Chinese draft characters displayed correctly.
3. Resize from 100x30 to 80x24 without crashing.
4. `Ctrl+C` with a draft clears the draft but keeps TUI open.
5. Second `Ctrl+C` with an empty draft exits cleanly (exit code 0).

**Not proven:** physical Chinese IME composition, physical mouse/clipboard and long-lived real terminal session behavior. Requires manual validation on the user's desired Windows Terminal/VS Code terminal.

## Complex Rust WAL task: DeepSeek V4.1 Flash / High / Pi 1.5.0

Both Harnesses used the same actual endpoint + model ID + API key. Fixtures were generated from the fixed version and externally tested. Each task run got its own directory; time limit 180s.

| Repetition | Kroma | Pi 1.5.0 |
|---|---|---|
| 1 | PASS, 140.204s, 21 tools | PASS, 148.25s, 14 tools |
| 2 | TIMEOUT, 180.140s, 22 tools | PASS, 145.656s, 15 tools |
| 3 | TIMEOUT, 180.156s, 24 tools | TIMEOUT, 180.187s, 13 tools |
| Within 180s | **1/3** | **2/3** |

**Result:** No stable performance leader; Kroma remains less reliable on this difficult Rust case with extra code-search/tool cycles. Do not extrapolate the one faster success to broad superiority. Trace for repetition 2 shows ~14 model/tool iterations, many sequential source inspections, and repeated cargo tests. This does not prove a specific Prompt rewrite helps.

Logs: `D:/Kroma/experiments/kroma-round5-complex-session-fixed/results.json`. First Pi successful run was recorded separately in `D:/Kroma/experiments/kroma-round5-complex-current/results.json` (the other entries in that directory predate the validated shell isolation).

## Real plugin runtime cost in isolated copies

Script: `benchmarks/round5_plugin_cost.py`. The four installed plugin **source folders** (Goal, Project Memory, Subagents, Web) were copied into independent synthetic data directories with the same plugin IDs enabled. Only manifest defaults were used; **original user plugin settings, data and Memory SQLite were not copied or changed**. Both arms ran the same two tasks twice each (four runs per arm).

| Metric | Plugins disabled | Four plugins enabled |
|---|---:|---:|
| Verified success | 4/4 | 4/4 |
| Elapsed total | **37.41s** | 48.44s |
| Foreground model requests | 14 | **13** |
| Reported total input tokens | **27,043** | 64,954 |
| Uncached input tokens | **8,355** | 20,538 |
| Cached input tokens | 18,688 | 44,416 |
| Output tokens | 1,823 | **1,665** |

The enabled arm costs ~2.4× input tokens on these small tasks, largely due to additional tools and Prompt. Token price depends on the provider's uncached/cached billing, so this is not a direct dollar-cost ratio. Plugins may provide useful capabilities that this limited test does not reward; **do not change the user's enabled settings automatically**.

Logs: `D:/Kroma/experiments/kroma-round5-plugins/results.json`.

## Final decisions and outstanding work

- Keep `edit_file` as default. The prior 8-pair A/B showed it wins over benchmark-only `apply_patch` in total time and Token count with equal pass rates.
- Keep existing compression; the 16K vs 36K pressure test had 13/13 correct recall in both arms. An extreme 8K case exposed a failure boundary but not a production-default failure.
- Do **not** add a universal planning/verification layer or auto-disable user's plugins.
- **High-priority follow-up**: fix Rust search/test loop inefficiency only after a reproducible, narrowly scoped A/B experiment improves success and resource use on multiple tasks. Confirm user-visible IME and mouse in real Windows Terminal manually.
