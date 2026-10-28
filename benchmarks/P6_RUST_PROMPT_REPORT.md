# P6 — Rust longitudinal prompt validation and targeted clause ablation

Date: 2026-10-26. Source: `/root/kroma-new` (P4 source, including existing uncommitted changes). Test implementation: `benchmarks/p6_rust_prompt.py`. Private raw data and transcripts: `/tmp/kroma-p6-rust-prompt/results.json`, `trace-analysis.json`, `ab-*`, `ablate-*`. No production system prompt, Shell schema, TRC/APEC or Agent Loop change was made by P6.

## Reproducibility and scope

- Same configured WorkBuddy DeepSeek High model/proxy; fixed Rust Kroma release binary copied from production P4, with **test-only outgoing-prompt selection**. A is original; B changes exactly one first sentence (matching P5). E retains only the focused-search portion; F retains the original focused-inspection wording plus the stop-reinspection instruction. Shell schema and runtime are identical across arms.
- Three independent Rust source copies per A/B arm. Each copy undergoes three sequential stages (`config remove`, `config rename`, `show/list --json`), with Agent process restart and persistent chat history between stages. Stage order is counterbalanced. There is no whole-agent time/token quota.
- Independent CLI acceptance cases with dummy credentials check state correctness, atomicity, permissions, CLI help and documentation. Compilation and Rust tests are part of the acceptance. A/B each passed 9/9 stage validations and terminated with status 0. Four tool calibration arms passed before the runs.
- Follow-on targeted clause ablation: A/B/E/F, **two fresh repetitions of the Rust `remove` stage per arm**, with the same external acceptance checks. This is a *new narrow task sample*, not additional full-history development evidence.

## Longitudinal A/B outcomes

| Metric | A (production P4) | B (P5 candidate) |
|---|---:|---:|
| Correct, normal-exit stages | **9/9** | **9/9** |
| Total wall time | 1,097.94 s | **892.88 s** |
| Mean stage wall time | 121.99 s | **99.21 s** |
| Completed model requests | **90** | 105 |
| Tool calls | **102** | 114 |
| Uncached input tokens | 178,199 | **104,314** |
| Cached input tokens | **2,132,736** | 2,627,968 |
| Model output tokens | 110,610 | **79,544** |
| Provider timeout retries observed | 1 | 0 |
| APEC summary errors observed | 0 | 0 |

Three-run totals (A / B):

| Repeat | A wall / requests / tools | B wall / requests / tools |
|---|---|---|
| 1 | 407.20 s / 31 / 36 | 321.53 s / 41 / 45 |
| 2 | 403.28 s / 32 / 37 | 282.47 s / 29 / 30 |
| 3 | 287.46 s / 27 / 29 | 288.87 s / 35 / 39 |

B took 18.7% less aggregate wall time, but **16.7% more completed model requests and 11.8% more tool calls**. It was not consistently faster on every stage or every repeat; the A arm had one provider timeout retry, which confounds latency comparisons. The provider-reported uncached input was lower for B, whereas cached input was higher. Actual provider bill/cost and reasoning-token price are unknown; do not label it a proven cost saving.

## Execution trace classification (heuristic, audit raw tool logs)

| Classification | A | B |
|---|---:|---:|
| Inspection-looking commands | 54 | 48 |
| Inspections before first possible edit | 37 | 35 |
| Exact duplicate shell-command strings | **15** | 20 |
| Verification-looking commands | 14 | 23 |

These categories are command-text regex heuristics, **not** ground-truth counts of real inspections/edits. In particular, one shell command can combine reads, edits and tests. The observed increase in duplicates and verification-like commands in B undermines the hypothesis that its speed gain arose primarily from removing redundant inspections. No tests were removed in either arm.

## Clause-level narrow Rust ablation

| Arm | Change | Correct | Mean wall | Requests | Tools | Uncached input | Cached input |
|---|---|---:|---:|---:|---:|---:|---:|
| A | Original P4 | 2/2 | 99.32 s | 31 | 43 | 35,207 | 445,824 |
| B | Focused searches + stop reinspection | 2/2 | 135.00 s | 35 | 41 | 38,177 | 533,888 |
| E | Focused searches only | 2/2 | 110.65 s | 34 | 42 | 36,750 | 519,168 |
| F | Original focused inspections + stop reinspection | 2/2 | **96.14 s** | **30** | **35** | **31,918** | **373,760** |

F is the most promising of these *two-run single-stage* observations. The improvement is very small in mean wall time (3.2%) and cannot establish generalization to the longitudinal task. B fails to reproduce the P5 small-Python-task speed win in this separate narrow Rust sample.

## Environment issue and corrections

The server's `/tmp` is a ~1.9 GiB tmpfs. The first P6 attempt was invalidated by a full `/tmp` during Rust compilation; its records were excluded. Re-runs used the normal disk-backed Cargo target (`/root/kroma-new/target`) for task compilation and the isolated experimental release build was moved out of `/tmp` to keep memory-filesystem headroom. A separate interrupted B3 run from low free space was also preserved and excluded, then repeated from a fresh copy. The 26 included stages are all successful and complete. No real credentials were written into test fixtures.

## Decision and follow-up

**Do not ship B to production.** A/B were equally correct; B was faster in the total longitudinal wall-clock but used more calls and tools, increased exact duplicate commands, and had unstable stage-level gains. This contradicts P5's conclusion that B reliably lowers calls. Retain the P4 global prompt and P4 Bash tool description.

**F is only a candidate**, not a production promotion. The next narrowly scoped test should compare A vs F across fresh longitudinal Rust tasks (plus other-language tasks if possible), check repeat inspections, all required tests and cost-normalized API usage. Do not add a hard cap, skip verification, silently change compression or modify Shell implementation to chase a benchmark.

P6 changes only the benchmark script and this report. Previous P2–P5 uncommitted work remains separate and is not automatically staged or committed here.
