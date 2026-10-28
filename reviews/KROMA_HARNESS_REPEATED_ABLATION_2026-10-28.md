# Kroma repeated harness ablation — 2026-10-28

## Summary

All 12/12 independent Rust WAL repair acceptance checks passed (three runs
per arm). The earlier single-run improvement from a more explicit scoped-search
prompt **did not reproduce**. No tested variation showed a repeatable,
unambiguous combined speed-and-token benefit. Do not merge these prompt
variants into production on this evidence.

Raw JSON results and per-trial process logs:
`/tmp/kroma-harness-ablation-bc94b731/results.json`

## Method

- Same WorkBuddy DeepSeek model, `high` reasoning, same source commit and
  injected WAL regression; each trial in a fresh isolated checkout.
- Four variants: `aligned` (Kroma with reasoning effort high and no explicit
  `thinking` field), `scoped` (aligned + search guidance), `batch` (aligned +
  independent parallel-call guidance), and `cache_prefix` (aligned + reordered
  stable system prompt with dynamic working directory at end).
- Three successive repetitions; arm execution order rotated per repetition.
- Correctness verified independently against the Rust regression.
- Cache-read tokens are *input* tokens, not a fraction of requests or a fee.
- Pi is **not** an arm in this follow-on experiment; this is a Kroma-only
  isolation study following the separate Pi vs Kroma comparison.

## Results (three trials per arm)

| Arm | Pass | Wall seconds | Input tokens | Uncached input | Cached input | Output | Model requests | Tool calls |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| Aligned baseline | 3/3 | 272.59 | 278,610 | 44,754 | 233,856 | 9,029 | 34 | 40 |
| Scoped search | 3/3 | 311.90 | 457,546 | 51,530 | 406,016 | 14,150 | 46 | 59 |
| Multi-call guidance | 3/3 | 271.59 | 286,243 | 43,299 | 242,944 | 9,710 | 33 | 39 |
| Stable prefix | 3/3 | 319.45 | 414,895 | 58,415 | 356,480 | 11,049 | 39 | 42 |

### Paired comparisons against aligned

| Arm | Wall difference | Input token difference | Uncached token difference | Requests difference |
| --- | ---: | ---: | ---: | ---: |
| Scoped search | +14.4% | +64.2% | +15.1% | +35.3% |
| Multi-call guidance | -0.4% | +2.7% | -3.3% | -2.9% |
| Stable prefix | +17.2% | +48.9% | +30.5% | +14.7% |

Individual scoped-search inputs were 170,989, 192,323 and 94,234 tokens;
aligned inputs 106,760, 63,419 and 108,431. Variance across identical
repeated tasks is large; these three samples do not establish statistical
significance or reliable generalization to other repositories.

## Interpretation and limitations

1. The multi-tool-call capability already exists in Kroma. Prompt pressure
   does not automatically improve task planning; the baseline itself produced
   9 multi-tool response turns over the three repetitions. Explicit batching
   produced 9 too. More batching is not a meaningful goal on its own.
2. Search instructions can help a particular sample, but can also drive more
   probing and longer trajectories. Measure scope, success and tool output
   bytes before converting a hint into a default policy.
3. Moving the dynamic workdir to the end of the system prompt is a plausible
   cache-prefix idea; this trial does not isolate prefix reuse because the
   number of model turns and trajectory differ. A provider-level matched
   payload experiment is required to prove cache effects.
4. These trials include Rust build/test latency and uncontrolled provider
   cache state. They are within one task class (WAL regression) rather than
   a suite representative of all coding tasks.
5. Do not implement any automatic prompt-tuner, new planner, compulsory
   batching or extra editing tools based only on this experiment.

## Disk cleanup before this experiment

Freed about **1.9 GB**, moving free space from **512 MB (99% used)** to
**2.4 GB (94% used)** by deleting only regenerable incremental/old benchmark
Rust build caches:

- `/root/kroma-new/target/debug/incremental`
- `/root/kroma-p7-build-target`
- `/root/kroma-p7-abf-target`
- `/root/kroma-p6-build-target`

After all twelve trials the filesystem retained ~**2.0 GB free** (95% used).
The Kroma source, user sessions, Git history, all experiment raw reports and
the current benchmark runner were preserved. None of the production `src/`
code was changed or committed.

## Reproduction

```sh
cargo build --locked --offline --example tool_ablation
python3 benchmarks/pi_kroma_harness_ablation.py \
  --repeats 3 --arms aligned,scoped,batch,cache_prefix --timeout 155
```
