# Round 12 Prompt Tournament — Interim Decision Record (2026-11-10)

## Scope and verdict
**28 independently recorded live attempts, no defensible global prompt winner. Keep production baseline.** This is an interim tournament rather than a completed balanced 6×4×2 factorial experiment: remote execution was interrupted mid-batch, several expensive arms hit task time caps, and some arm/task cells are missing.

- One compiled candidate Kroma binary; same configured DeepSeek v4.1 Flash High endpoint, identical tool definitions and raw JSON tool-result projection unless the selected candidate changes only the prompt.
- Six arms: baseline, lean, focus, finish, focus_finish, dynamic. Dynamic only adds a transient checkpoint after the agent has observed a nonzero passing Cargo summary and at least two subsequent tool calls; it adds no extra LLM request by design. Dynamic logic remains experimental.
- Fixture types: small Rust multifile, sparse-test lookup with external post-run hidden acceptance, history Unicode regression, Rust trash-session metadata restoration, and independent holdout in Unicode session-search snippet. The sparse negative control was confirmed to have public green tests with hidden failing behavior; Unicode session-search holdout was confirmed initially failing.
- Both self-tests, initial negative controls, and research build Clippy/formatter passed earlier; keep data independent of premature prompt promotion.
- Per-task overall wall-time caps 95/120/175/185 seconds depending on fixture, and normal completion is required. A timed-out run that passes oracle is **not** strict success. Private post-run acceptance and agent-requested testing are separately tracked.

## Snapshot of evaluated cells
| Task | Arm | Runs | Strict successes | External code passes | Notes |
|---|---|---:|---:|---:|---|
| Small Rust multifile | baseline | 2 | 2 | 2 | Total ~44.3s |
| Small Rust multifile | finish | 2 | 2 | 2 | Total ~40.8s |
| Small Rust multifile | dynamic | 2 | **1** | 2 | One otherwise correct run did not execute required tests |
| Small Rust multifile | lean | 1 | 1 | 1 | 72.0s, 16 model requests: conspicuous regression |
| Small Rust multifile | focus | 1 | 1 | 1 | 29.5s, 6 requests |
| Small Rust multifile | focus_finish | 1 | 1 | 1 | 20.6s, 5 requests |
| Sparse lookup, green-public/hidden-red trap | baseline | 2 | 2 | 2 | Total ~114.5s |
| Sparse lookup | finish | 2 | 2 | 2 | Total ~83.8s |
| Sparse lookup | dynamic | 1 | 1 | 1 | Second repeat only; interrupted first is not scored |
| Sparse lookup | lean | 1 | 1 | 1 | 56.2s |
| Sparse lookup | focus | 1 | 1 | 1 | 51.8s |
| Sparse lookup | focus_finish | 1 | **0** | **0** | Agent finished but hidden lookup acceptance failed |
| Complex Rust metadata restoration | baseline | 1 | 0 | 1 | 185s cap: correct code without finish |
| Complex Rust metadata restoration | finish | 1 | 0 | 1 | 185s cap: correct code without finish |
| Complex Rust metadata restoration | lean | 1 | 0 | 1 | 185s cap: correct code without finish |
| Complex Rust metadata restoration | dynamic | 1 | 1 | 1 | 179.9s, 14 completed model requests |
| History Unicode repair | baseline | 1 | 0 | 1 | 175s cap: correct code without finish |
| History Unicode repair | finish | 1 | 0 | 0 | 175s cap: unverified |
| History Unicode repair | dynamic | 1 | 0 | 0 | 175s cap: unverified |
| New holdout: session Unicode snippet | baseline | 2 | 0 | **2** | Both 185s caps, code externally correct |
| New holdout: session Unicode snippet | finish | 2 | 0 | **0** | Both 185s caps, code did not pass acceptance |

Total recorded rows: 28. Any absent cell is **not tested** rather than a failure. Under strict rules a code-correct timed-out task is still unfinished; do not report inflated success rates by silently treating these as normal successes.

## Key inferences
1. **Baseline is the most defensible shipping choice**, not necessarily the theoretically fastest prompt. It consistently handles both repeats of small and sparse tasks; it also produces correct code in tested complex timeout scenarios and both holdout attempts.
2. **Finish** shows encouraging speed on smaller tasks and in previously studied Rust metadata repairs, but has two externally incorrect holdout outcomes, so it cannot be promoted as a universal winner.
3. **Focus_finish** gives an isolated fast small-task result but fails the sparse green-public/hidden-red counterexample; this is a decisive correctness penalty.
4. **Lean** causes a 72s/16-request small-task outlier versus baseline 20.9s/5 requests. Shorter prompts are not reliably cheaper.
5. **Dynamic** looks promising in one complex trial but failed mandatory test-running on a simple trial and was not evaluated successfully on new complex holdouts; automatic checkpoint logic is not justified.
6. Long tasks may be dominated by network/model delay, model stochasticity and Rust rebuilds. Multiple 175–185s censored observations cannot be averaged as regular completed runtimes. This incomplete grid cannot establish global superiority, statistical significance or a ranked champion.

## Reproducibility and design caveats
- Main readout: `D:/Kroma/experiments/kroma-round12-prompt-tournament/results.json`.
- Driver: `benchmarks/round12_prompt_tournament.py`; related regression/safety fixture helpers in `round9_complete_function_read.py`, `round11_finish_guidance.py`, `round7_readable_shell_holdout.py`.
- The holdout is new to this tournament, but visible as an injected test inside the project; it is an independent acceptance, **not an invisible surprise test**.
- An interrupted remote call created a sparse/dynamic worktree without a completed result/log. Do not treat that interrupted attempt as successful or failed; the second repeat has an independent completed dynamic result.
- Adaptive implementation is a single-use, request-only experimental prompt, not an approval/validator, and remains disabled. It is not considered production-ready.
- The multi-arm screening was not a full permutation-balanced design; the later per-task pair reversals help only specific follow-up comparisons.
- Code may be correct even when task completion is absent; score oracle pass, proper test execution, normal finish, model requests, wall and tools separately.

## Action
**Do not merge any new prompt.** Maintain baseline and preserve this experimental dataset as a counterexample against assuming a universal optimized prompt. If work resumes, focus on a genuinely new independent benchmark portfolio and sufficient replications rather than reusing old code-path tasks to search for a lucky prompt. Avoid additional broad experimental changes to core Agent Loop based on these observations alone.
