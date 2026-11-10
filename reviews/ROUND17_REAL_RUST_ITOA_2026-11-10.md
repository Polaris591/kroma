# Round 17 — Real Rust PR #7: Complete offline fixture and prompt A/B (2026-11-10)

## Verdict

**Evidence-finish is slightly faster in aggregate on one genuine, complete/offline historical Rust bug while matching baseline correctness; there is no established generic reduction in model requests, token use, or exploration.** Keep the production default unchanged. This case supplies a necessary counterexample to explanations that all earlier Rust delays were intrinsic to the model or entirely caused by missing generated data.

## Authentic historical bug and validity controls

- Historical upstream PR: https://github.com/dtolnay/itoa/pull/7
- Pinned real broken base: `b2445b6d0ef9cb5c6caf98dc2dcf31f644f705fd`.
- Pinned authentic fixed head: `bd4884d34b0d36525541388d4c7b277186f24a75`.
- Task: change public `itoa::write` return type from `io::Result<()>` to accurate `io::Result<usize>`, including negative values, maximum/minimum integers, and real `io::Write` errors. Must preserve formatting.
- Historical unmodified files retrieved from GitHub: `Cargo.toml`, `src/lib.rs`, `tests/test.rs` and actual fixed copies. No manual fault insertion.
- Validity checks: original base passes normal Rust tests, fails new independent return-type acceptance; true upstream fixed head passes both normal and hidden tests. No third-party crates, downloaded data, container, external API, generated tables, or unrelated environment-specific command requirements; Rust builds offline in isolated Cargo targets.
- Hidden tests are injected **only after the agent exits**. Agent is forbidden to modify any tests, source API except return signature change. Public test unchanged by independent validation. The true fixed head is used **only for validation**, never model context.

### Invalid pilot excluded

First pilot round in `D:/Kroma/experiments/kroma-round17-itoa-ab` was invalid: its prompt allowed changing tests while the oracle prohibited it. Both arms modified public tests; these trials are **excluded** from all below. Wording was corrected for both arms and restarted in a **fresh** lab.

## Four valid crossed pairs — eight paid agent runs

One same Kroma research release binary and DeepSeek v4.1 Flash High endpoint/effort. `finish` arm only sets `KROMA_EXPERIMENT_PROMPT_ARM=finish`. Repeat orders: baseline→finish, finish→baseline, baseline→finish, finish→baseline. Isolated projects, WAL, Cargo target directories, identical fixture + user prompt, same 130-second task cap and independent post-run tests.

| Pair | Baseline wall | Evidence-finish wall | Baseline first real `src/lib.rs` edit | Finish first real `src/lib.rs` edit | Both strict success |
|---|---:|---:|---:|---:|---|
| 1 | 90.640s | **77.000s** | tool #9 | tool #4 | yes |
| 2 | 65.578s | **59.468s** | tool #4 | tool #7 | yes |
| 3 | **53.610s** | 54.093s | tool #7 | tool #5 | yes |
| 4 | 79.375s | **69.750s** | tool #10 | tool #7 | yes |

**Strict success: 4/4 both arms**. Every run kept the public tests unchanged, did a relevant test, passed independent post-run hidden tests, and exited normally. Independent rechecks of all eight final source snapshots agreed **8/8**.

| Aggregate | Baseline | Finish |
|---|---:|---:|
| Observed wall sum | 289.203s | **260.311s** |
| Median wall | 72.477s | **64.609s** |
| Model requests | **58** | 60 |
| Input tokens | **432,213** | 443,392 |
| Tool calls | 68 | **66** |
| Paired wall wins | 1/4 | **3/4** |

Measured time sum is approximately **10.0% lower** for candidate on these eight successful runs. However model requests **rose** 58→60 and input tokens **rose** by ~2.6%. Therefore this observation does **not** prove the prompt reduces exploration or token cost. There may be latency/cache/model sampling differences and the sample has only one distinct fault type and four repeats.

In particular, fewer tool calls and earlier edits are not a monotonic explanation: repeat 2 baseline edited earlier and used fewer tools but took longer; repeat 3 candidate edited earlier and used fewer tools yet ended 0.483 seconds later.

## What the result rules out

- Historical genuine Rust bug can be replicated and independently verified on this Windows host without synthetic data or missing UCD inputs.
- Kroma and DeepSeek can complete the bug without entering the 185-second Unicode generator detour, so that previous case was partly benchmark/environment-specific.
- A single aggressive global instruction that forces earlier edits is not justified; correctness and complete tests must remain first.
- The candidate's success on this case does not establish a strong universal Rust Prompt ranking; the repeated experiment tests one issue, not four independent issues.

## Next discriminating task

For a meaningful Rust generalization test, select a **second** Rust PR with multiple relevant modules and full offline tests. Verify base-red and real-head-green first; then repeat both arms with same environment, track first *project source* edit rather than scratch scripts. Investigate protocol/model-turn compression only if failures persist on an environment-complete task; do not conflate environment missing input with agent reasoning issues.

## Saved evidence

- Prompt A/B script: `benchmarks/round17_itoa_ab.py`.
- Negative/positive preflight script: `benchmarks/round17_itoa_preflight.py`.
- Post-run recheck: `benchmarks/round17_itoa_recheck.py`.
- Historic pinned blobs: `benchmarks/round17_itoa_{base,fixed}/`.
- Valid data: `D:/Kroma/experiments/kroma-round17-itoa-ab-v2/results.json`.
- Rechecks: `D:/Kroma/experiments/kroma-round17-itoa-ab-v2/independent_recheck.json`.
- Source branch `research/rust-agent-turn-efficiency-20261110` remains isolated, no default prompt change or main merge.
