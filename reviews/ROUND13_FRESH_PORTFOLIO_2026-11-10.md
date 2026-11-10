# Round 13 — New Cross-Language Prompt Portfolio (2026-11-10)

## Verdict

**Evidence-based finish is the leading experimental candidate on this new portfolio, not an established universal default.** Keep main branch and production prompt unchanged for now. The prior Round 12 holdout also contained candidate failures, so promoting a global default solely from Round 13 would overstate generalization.

A genuinely new set of 4 small-to-medium coding defects, in 2 languages (Node.js and Rust), each repeated 4 times per arm with deterministic reversed pair ordering: **32 billable live agent runs** using the same Kroma release binary and existing DeepSeek v4.1 Flash High provider configuration.

## Contract and negative controls

- **Node CSV:** correctly parse quoted commas, escaped quotes, CRLF, embedded newlines, and trailing empty fields. Public basic checks pass on buggy code; independent post-agent hidden checks fail before repair.
- **Node async retry cache:** deduplicate in-flight requests, persist successes, retry after rejection, and isolate unrelated keys. Public test passes on buggy code; hidden retry check fails.
- **Rust store consistency:** invalidate cached positive and negative values after put/delete/reinsert. Public first read succeeds while hidden mutation cases fail.
- **Rust frame decoder:** preserve split headers/payloads across chunks, emit multiple frames, support zero-length frames. Public whole-frame test passes while hidden chunk tests fail.

All four original fixtures passed basic/public checks but failed independent hidden checks. Hidden test files were injected **only after each agent exited**. All existing/public tests were required to remain unchanged and agent-run tests were tracked separately. No packages were installed.

Model/tool/endpoint/context/reasoning settings and task prompts were identical across arms. The only arm variable is `KROMA_EXPERIMENT_PROMPT_ARM=finish`, with default baseline not setting the variable. The existing experimental research branch and binary were used; main unchanged.

## Results

| Metric | Baseline prompt | Evidence-finish candidate |
|---|---:|---:|
| Total runs | 16 | 16 |
| Final source passes independent acceptance | **16/16** | **16/16** |
| Agent visibly runs a relevant test | **16/16** | **16/16** |
| Independent acceptance + test + normal finish within cap | **14/16** | **16/16** |
| Timed-out but code-correct runs | 2 | 0 |
| Sum of observed/capped wall seconds | 842.72 | **612.84** |
| Median individual wall seconds | 41.80 | **36.04** |
| Tools issued across all runs, including censored | 147 | **126** |
| Pairwise wall-time wins | 6/16 | 10/16 |

Observed/capped wall time reduced by ~27.3% across this small portfolio, **not a reliable universal speed gain**; two baseline samples are censored at 105/125s and cannot be treated as normal completions. The 16 comparisons yield only 10 candidate wins and 6 baseline wins; a sign test would not strongly reject a 50:50 win rate. Additionally, only four independent defect families were evaluated, each repeated four times, so 16 runs per arm are not 16 independent problem types.

### Uncensored comparison only

The 14 matched pairs in which both agents normally finished:

| Metric | Baseline | Candidate |
|---|---:|---:|
| Model requests | 108 | **100** |
| Input tokens | 399,445 | **346,316** |
| Tool calls | 121 | **112** |
| Total elapsed | 612.472 s | **533.562 s** |
| Per-pair speed wins | 6 | 8 |

These totals exclude both censored pairs from **both arms**. They do not prove statistical significance or a guaranteed decrease on any individual task.

### Per-task accuracy and wall observations

| Task | Baseline strict passes | Candidate strict passes | Baseline observed/capped wall total | Candidate wall total |
|---|---:|---:|---:|---:|
| Node CSV | 3/4 | 4/4 | 240.39 s | 134.55 s |
| Node async retry cache | 4/4 | 4/4 | 172.77 s | 151.64 s |
| Rust cache consistency | 4/4 | 4/4 | 190.63 s | 146.56 s |
| Rust chunked frames | 3/4 | 4/4 | 238.94 s | 180.09 s |

On some repetitions baseline finished faster. In particular, normal Rust frame decoding runs often had comparable or better baseline latency; the candidate's total advantage there partly comes from avoiding an outlier timeout.

## Independent verification and build contamination

The initial production-worktree `cargo test` using shared `D:/Kroma/target-kroma-check` unexpectedly executed a stale **Round 12 injected test from another checkout**; `src/sessions.rs` in the research worktree was not modified. Sharing `CARGO_TARGET_DIR` across distinct Kroma copies is a serious experimental harness isolation problem. The failing fixture test itself had an archive missing the required system message, so that failure must not be attributed to a prompt. **Round 12 comparisons with shared Kroma package artifacts deserve reduced confidence.**

Mitigations actually performed:

1. Ran Kroma project's all-lib tests using a **fresh isolated Cargo target** at `D:/Kroma/target-kroma-round13-qa`: **783 passed / 0 failed / 11 ignored**. Clippy `-D warnings`, formatter check, and `git diff --check` passed.
2. Created `benchmarks/round13_independent_recheck.py`; it re-ran **all 32 hidden acceptance suites** from the saved projects, using **distinct Cargo target directories per Rust trial**. **32/32 rechecks matched the original code-verification outcomes.**
3. Fixed a checker-only Windows CRLF mismatch (file text matches immutable fixture text, but raw LF bytes differ from disk CRLF); did not count this diagnostic miscompare as an agent failure. The corrected checker confirmed public tests unchanged.
4. Kept normal agent completion distinct from independent code correctness: 2 baseline runs had correct code but were still strict failures because they timed out before final answer.

Independent data files:
- `D:/Kroma/experiments/kroma-round13-fresh-portfolio/results.json`
- `D:/Kroma/experiments/kroma-round13-fresh-portfolio/independent_recheck.json`

Scripts:
- `benchmarks/round13_fresh_portfolio.py`
- `benchmarks/round13_independent_recheck.py`

## Decision and next step

- **Current best experiment:** evidence-finish. It is supported across previously untried Node/Rust workflows, correctly performs hidden requirements and avoids two slow unfinished trajectories.
- **Not a proven global default:** only four new defects; earlier Round 12 candidate Unicode code was not independently accepted in two capped runs, and that test had fixture/build weaknesses requiring caution. This prevents claiming unconditional improvement.
- **No default merge yet.** For production readiness, use completely isolated Cargo target dirs by project, more natural external repository bug-fix tasks, explicit stop-without-test negative controls, and repeated mixed-difficulty crossover. Promote only after the new candidate maintains correctness and improves task-normal-completion without materially degrading simple tasks.
- No security permissions, forced stopping, planner, validator, approvals, or TRC/APEC changes are justified.

**Repository status:** all prompt experiments, tournament scripts and reports remain in the isolated research worktree `research/rust-agent-turn-efficiency-20261110`; no changes merged to main.
