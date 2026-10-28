# Pi vs Kroma randomized crossover: DeepSeek High

Date: 2026-10-28

## Design

- Both runners used the same WorkBuddy proxy, model alias `deepseek`, and provider key.
- Pi was started with `--thinking high`. Experimental Kroma was started with
  `reasoning_effort=high`, with the explicit `thinking.type` field disabled,
  to align upstream High requests. This is a **benchmark-only** choice,
  not a change to the saved user profile or Kroma production runtime.
- Three independent task types: Python records implementation, Python
  cross-file normalization repair, and Rust WAL session-ordering regression.
- Each task was repeated three times per runner. The nine pairs and their
  execution order were pre-generated using random seed `20261028` before
  execution; no outcome-based reordering.
- Per-trial fresh directory and independent functional validation. The Rust
  benchmark reuses an offline Cargo target cache for both runners. Caching
  and model server load were not reset between adjacent trials.
- Pi enabled its native tools (`read`, `bash`, `edit`, `write`) without skills,
  extensions or MCP. Kroma used the existing single Shell runtime via the
  benchmark-only runner. This compares **harness products**, not tools-only
  or same-instruction variants.

## Aggregated results

| Metric | Pi | Kroma |
| --- | ---: | ---: |
| Independent acceptance | 9/9 | 9/9 |
| Total elapsed seconds | **376.35** | 390.34 |
| Total input tokens incl. cached | 455,032 | **427,252** |
| Uncached input tokens | 64,504 | **59,124** |
| Cached-read tokens | 390,528 | **368,128** |
| Output tokens | **13,893** | 14,212 |
| Foreground model requests | **67** | 71 |
| Tool completions | 75 | 75 |

Kroma was faster in 6 of 9 paired runs. Pi took 3.6% less time in aggregate;
Kroma used 6.1% less total input and 8.3% less uncached input. Because
task classes have unequal difficulty, totals are strongly influenced by the
three Rust runs. No statistically significant dominance is asserted.

## Per-task timing (3 pairs each)

| Task | Pi timing seconds | Kroma timing seconds | Pi median | Kroma median |
| --- | --- | --- | ---: | ---: |
| Records parsing | 12.72, 15.82, 14.38 | 12.91, 13.89, 13.19 | 14.38 | **13.19** |
| Cross-file Python fix | 15.40, 21.30, 14.23 | 13.27, 18.92, 11.26 | 15.40 | **13.27** |
| Rust WAL regression | 78.48, 81.35, 122.67 | 99.45, 117.88, 89.57 | **81.35** | 99.45 |

Within Rust, both runners won at least one pair. In the third pair Pi took
122.67 s and 155,110 input tokens while Kroma took 89.57 s and 80,414 input
tokens. This materially supports concern about run-to-run behavioral variance.
Three repetitions are insufficient to estimate the probability that either
harness wins a randomly chosen future complex Rust task.

## Limitations

The randomized task *order* and runner order control systematic ordering
bias, not model sampling variance, network contention, upstream prompt-cache
TTL, Rust incremental compilation or coding strategy. The same injected
Rust issue was repeated, not three independent large-project bugs. The
experiment measured upstream cached read tokens, not reliable billing cost.
Only three task types were used; long-context APEC, MCP, and multi-day tasks
were not evaluated. Comparing identical Kroma model runtime against Pi's
native toolset may reveal product differences but cannot identify a single
causal mechanism.

## Durable data

Raw report: `/tmp/kroma-pi-crossover-96767baa/results.json`

Runner: `benchmarks/pi_kroma_randomized_crossover.py`

Production `src/` was not intentionally modified by the benchmark. Additional
unrelated worktree changes observed at report time should be reviewed before
any Git commit, deployment or cleanup.
