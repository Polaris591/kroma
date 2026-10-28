# P6 — Rust continuous-development prompt study (2026-10-26)

## Scope and evidence

P6 is complete: **26/26 stage runs passed** the scripted acceptance checks:
18 A/B Rust continuous-development stages (three stages per repeat, three
repeats, two arms), and eight targeted-clause ablation runs (one stage per
repeat, two repeats, four arms). All experimental changes were confined to a
disposable Rust runtime and isolated task checkouts. **No production prompt,
tool interface, or compression-policy changes were made.**

Source: `benchmarks/p6_rust_prompt.py`.
Machine-readable evidence: `/tmp/kroma-p6-rust-prompt/results.json`.
Command trace summary: `/tmp/kroma-p6-rust-prompt/trace-analysis.json`.
Baseline source hashes and original Git HEAD are recorded in the results
metadata, which also records the WorkBuddy DeepSeek High provider setting.

The Rust stages covered provider configuration **remove**, **rename**, and
**JSON show/list**, with process restarts between stages to test session
continuity. Agent-level task deadlines and token quotas were not applied.
Results below include completed model requests; the one timed-out/retried
model request is separately reported.

## Continuous three-stage Rust development: A versus B

* A: original P4 global execution guidance.
* B: replace one guidance sentence with focused searches and a rule to
  proceed to editing after obtaining sufficient evidence rather than
  re-inspecting unchanged files. Tool schema is unchanged.

| Metric (three independent 3-stage repeats per arm) | A | B |
|---|---:|---:|
| Scripted stages passing | 9/9 | 9/9 |
| Total task wall time, seconds | 1097.939 | 892.879 |
| Mean wall time per stage, seconds | 121.99 | 99.21 |
| Completed model requests | 90 | 105 |
| Shell tool calls | 102 | 114 |
| Uncached input tokens | 178,199 | 104,314 |
| Cached input tokens | 2,132,736 | 2,627,968 |
| Output tokens | 110,610 | 79,544 |
| Model timeout retries | 1 | 0 |
| APEC errors | 0 | 0 |

B reduced overall wall time by **18.7%**, uncached input tokens by
**41.5%**, and output tokens by **28.1%**, but increased model requests
by **16.7%**, tool calls by **11.8%**, and cached input tokens by **23.2%**.
Do not call B strictly lower-cost without provider-specific price weighting
and accounting for uncertainty in the token figures.

Command trace classification (heuristic, not ground truth):

| Metric | A | B |
|---|---:|---:|
| Inspection commands | 54 | 48 |
| Inspections before first *possible* edit | 37 | 35 |
| Exact duplicate shell commands | 15 | 20 |
| Verification-class commands | 14 | 23 |

The longer B prompt did not eliminate repeated commands or excessive
verification; in this experiment both increased. The classification
recognizes shell-text patterns and may misclassify multi-action commands.

## Clause-level ablation: two independent remove-task repetitions per arm

* A: original P4 global guidance.
* B: P5 focused searches **plus** proceed-to-edit rule.
* E: focused searches alone.
* F: original focus-on-relevant-files text **plus** proceed-to-edit rule.

| Metric | A | B | E | F |
|---|---:|---:|---:|---:|
| Scripted runs passing | 2/2 | 2/2 | 2/2 | 2/2 |
| Mean wall time (seconds) | 99.32 | 135.00 | 110.65 | **96.14** |
| Completed model requests | 31 | 35 | 34 | **30** |
| Shell tool calls | 43 | 41 | 42 | **35** |
| Uncached input tokens | 35,207 | 38,177 | 36,750 | **31,918** |
| Cached input tokens | 445,824 | 533,888 | 519,168 | **373,760** |
| APEC errors | 0 | 0 | 0 | 0 |

F was the most promising **candidate** in this small test: versus A, its
mean wall time fell 3.2%, model requests 3.2%, tool calls 18.6%,
uncached input tokens 9.3%, and cached input tokens 16.2%.
However, F was slower than A in the first repeat (106.93s versus
90.03s), and faster in the second (85.35s versus 108.60s). The two
repeats cover the same configuration-remove task, so generalization and
statistical significance have **not** been established. B's large
long-task improvement did **not** reproduce in the two remove-only
ablation repeats.

## Decision and next gate

1. **Keep P4 as the production baseline** for now. F is the leading
   experiment-only hypothesis; no production adoption or Git commit was
   made by P6.
2. Conduct an **independent multi-stage Rust A/F comparison**, with a
   preregistered acceptance/cost rule, a balanced execution order, and
   multiple repeats that include session restart and growing context.
3. Analyze repeated inspections and redundant verification using full
   traces rather than assuming prompt guidance is sufficient.
4. Promote a change only if acceptance and reliability remain intact and
   efficiency improvement is replicated across stages, not just one
   short feature or one favorable run. Compare estimated billing cost
   under the actual WorkBuddy price model in addition to wall time.

Limitations: this is a small stochastic sample against one proxy/model
and one evolving Rust project. It cannot prove universal improvement over
Pi or justify new runtime controls. Existing dirty production-checkout
files were not incorporated into experiment artifacts beyond the
recorded source snapshot.
