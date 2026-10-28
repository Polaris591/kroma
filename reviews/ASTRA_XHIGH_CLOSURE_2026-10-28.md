# Astra Xhigh review: focused closure and acceptance

Date: 2026-10-28
Repository: `/root/kroma-new`
Baseline: `fac576c` plus pre-existing uncommitted changes. **This work does not roll back or commit the existing worktree.**

## Scope

Finish the nine issues flagged in the review, calibrate Anthropic token usage,
and run limited APEC/TRC correctness checks. Avoid rewriting the Agent loop,
session WAL, context algorithms or plugin interfaces. Do not enable speculative
cross-provider caching or change APEC thresholds from unsupported estimates.

## Nine issue acceptance checks

| Issue | Implementation / verification |
| --- | --- |
| JSON Ctrl+C leaves Shell children running | Shared `drive_task` cancellation, process-group guard; `benchmarks/astra_cancel_smoke.py` verifies no delayed side effects, JSONL cancellation event, durable restart without tool replay |
| Saved model resumes with wrong context/output/tiers | Shared `Config::from_profile` and `configured_model`; `app::tests::profile_limits_and_levels_reach_the_runtime`, `benchmarks/astra_tui_profiles_smoke.py` verify B profile switch/restart, output cap and thinking tier |
| DeepSeek `enabled` breaks resumed sessions | Saved thinking is the valid reasoning tier; historical `enabled` is mapped on recovery; PTY restart regression covers both paths |
| TUI `!command` cannot be cancelled | Event-responsive Shell future; PTY smoke sends Ctrl+C during a delayed command and checks command termination and continued TUI availability |
| `/model` drops process output cap | Shared config resolution; PTY checks `KROMA_MAX_OUTPUT_TOKENS=2048` across `/model` A → B and `/resume` |
| Shift+Tab after model switch uses stale tiers | Picker reads active backend tiers; focused Rust unit and PTY `high → xhigh` regression |
| Large pipe input files are group/world readable | Private file helper writes mode 0600 under directory 0700; CLI smoke verifies actual permissions in isolated HOME |
| Responses visible reasoning summary is not requested | Request asks for `reasoning.summary=auto`; only explicit pre-stream field rejection triggers fallback; mock tests verify one-time fallback and preserved effort; partial SSE is not replayed |
| Reasoning `none`/`off` level aliases rejected incorrectly | Normalization before membership validation; tests cover whitespace, case and duplicate aliases |

## Anthropic usage correction

Anthropic's `usage.input_tokens` counts *uncached* input only. The standardized
`Usage.input` now adds it to `cache_creation_input_tokens` and
`cache_read_input_tokens`; `Usage.cached` denotes the latter, as a subset of
the full input. Both streaming foreground responses and non-streaming APEC
responses use this interpretation. Stream counter updates are cumulative, not
incremental. Arithmetic overflow and unknown input remain unknown; absent
cache-read reporting is not treated as an observed zero. Tests cover stream,
APEC and overflow.

This does **not** implement Anthropic `cache_control` or a universal price
calculator. Provider-specific cache writes must be considered independently
before offering caching as an enabled feature.

## APEC/TRC limited evidence and boundaries

The deterministic fixed-history APEC test covers three critical facts in a
single scenario: a non-negotiable user constraint, an unfinished migration,
and an unverified failed command. It verifies that the raw snapshot reaches
the summarizer, the resulting projected memory exposes those facts, a WAL
commit happens, and authoritative raw history stays unchanged. An external
file reference does not pretend that unseen file bytes were summarized.

Observed in the **deterministic mock**, not a live-model A/B:

- Estimated pre-APEC projection: 7,872 tokens.
- Estimated post-APEC projection: 4,809 tokens.
- Two synthetic summarizer calls and synthetic usage input=240/output=30.

Compression diagnostics also log estimated projection size before/after TRC
and APEC commits. They do not infer actual cache savings: the provider's
cached-token count alone cannot locate the exact prefix hit boundary.

These checks establish regression behavior, **not** factual recall quality,
actual provider cost, or task-success improvements for real models. A
controlled live-model A/B remains an empirical research task; it must compare
quality, cache reads/writes, total foreground + APEC requests, recovered
evidence and elapsed completion time before any TRC/APEC tuning.

## Regression commands

All commands use the current source and an isolated test HOME/workdir; mocks
do not use production credentials.

```sh
cargo fmt --all -- --check
git diff --check
cargo test --locked --offline
cargo clippy --locked --offline --all-targets -- -D warnings
cargo build --locked --offline

KROMA_TEST_BINARY=target/debug/kroma python3 benchmarks/cli_automation_smoke.py
KROMA_TEST_BINARY=target/debug/kroma python3 benchmarks/mock_profile_smoke.py
KROMA_TEST_BINARY=target/debug/kroma python3 benchmarks/tui_pty_smoke.py
KROMA_TEST_BINARY=target/debug/kroma python3 benchmarks/astra_cancel_smoke.py
KROMA_TEST_BINARY=target/debug/kroma python3 benchmarks/astra_tui_profiles_smoke.py
```

No default Shell timeouts, forced approvals, tool-result deletion, plugin host,
dynamic prompt optimizer, new caching subsystem or rewrite of WAL/TRC/APEC
were introduced. Historically ignored opt-in real-model benchmark tests remain
ignored by the ordinary test command.
