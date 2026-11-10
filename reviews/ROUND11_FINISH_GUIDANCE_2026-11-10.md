# Round 11 — Evidence-based finishing guidance A/B (2026-11-10)

## Decision
**Do not enable by default or merge to main.** The candidate reduced post-success exploration in two matched complex Rust repair trials while maintaining correctness, but a small Rust control regressed, and only one complex defect type was exercised. Keep the opt-in experiment available for later study; stop broad Prompt tuning.

## Experimental contract
- Same built Kroma binary, same existing DeepSeek v4.1 Flash High endpoint/model credentials; same command tool name shell, same raw JSON results, same context and single-shot threshold compaction. No web or unrelated model settings changed.
- Only env KROMA_EXPERIMENT_FINISH_ON_EVIDENCE=1 replaces existing verification-and-finish wording in the request-only system projection. Default off.
- Three tasks: full Rust session-trash metadata recovery bug; sparse-coverage record lookup bug (public targeted parser test passes when lookup remains broken, private post-run acceptance exposes defect); previously used small Rust multi-file regression.
- Two repetitions each, arm orders baseline→finish and finish→baseline; independent workdirs, frozen initial fixtures, post-run checks and requirement to run Cargo testing.
- Correctness first; record normal tool-free completion separately from source passing verification. Tool-count minimization cannot substitute for requirement compliance.

## Results (two repeats each)

| Task | Arm | Independently verified + normally finished | Total wall | Model requests | Tool calls |
|---|---|---:|---:|---:|---:|
| Complex Rust restore | Baseline | 2/2 | 288.406s | 36 | 50 |
| Complex Rust restore | Candidate | 2/2 | 261.453s | 26 | 35 |
| Sparse-coverage Rust lookup | Baseline | 2/2 | 113.672s | 21 | 23 |
| Sparse-coverage Rust lookup | Candidate | 2/2 | 108.797s | 23 | 27 |
| Small Rust multifile | Baseline | 2/2 | 40.031s | 10 | 10 |
| Small Rust multifile | Candidate | 2/2 | 49.687s | 11 | 11 |

Complex task candidate reduction: ~9.3% wall, ~27.8% model requests, 30% tool calls; these are sums over only two pairs, not a performance generalization. Sparse task is mixed on requests vs time, small control regressed in overall wall time.

### Per-pair wall time in seconds
| Task | Repetition | Baseline | Candidate |
|---|---:|---:|---:|
| Complex restore | 1 | 132.656 | 125.796 |
| Complex restore | 2 | 155.750 | 135.657 |
| Sparse lookup | 1 | 46.719 | 53.812 |
| Sparse lookup | 2 | 66.953 | 54.985 |
| Small multifile | 1 | 18.093 | 29.906 |
| Small multifile | 2 | 21.938 | 19.781 |

## Specific post-success mechanism
Read-only WAL tool-result analysis on complex task:
| Pair | Baseline tool calls after first post-edit successful Cargo summary | Candidate |
|---|---:|---:|
| 1 | 8 | 2 |
| 2 | 11 | 1 |

All four complex runs had externally verified correct final code and carried out tests. Fewer post-success calls in candidate is directly consistent with intended behavior. It remains an observational correlation, not an independently proven causal mechanism or proof that each extra baseline regression test was unnecessary.

## Known limitations
- The sparse task deliberately has incomplete public coverage, but prompt explicitly calls attention to the untested lookup requirement. It establishes a safety counterexample against automatic green-test stopping, not hidden gap discovery in an unconstrained task.
- Both arms use a shared on-host Cargo target cache; crossover order controls but cannot eliminate shared build warming or remote request latency.
- Confidence is limited by two runs per task, one primary complex code path, and model stochasticity. The candidate's extra sentence may change attention/response sampling besides stopping behavior.
- The experimental implementation and prior rounds' uncommitted changes are in an isolated Windows research worktree; main is unchanged. The result does not justify a validator, a planner, hard turn limits, or a default auto-stop.

## Engineering verification
- Lab: D:/Kroma/experiments/kroma-round11-finish-on-evidence/results.json
- Driver: benchmarks/round11_finish_guidance.py
- Prompt code: src/prompt.rs; opt-in KROMA_EXPERIMENT_FINISH_ON_EVIDENCE=1.
- Rust unit tests: 777 passed, 0 failed, 11 ignored; Clippy --all-targets -D warnings, cargo fmt --check and git diff --check passed.
- Further work on this same Prompt is not warranted under the agreed success criterion. Return focus to basic harness features; revisit only if a new independent task family reproduces the improvement without meaningful small-task regressions.
