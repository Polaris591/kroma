# P9 APEC chunk-size A/B — live experiment

Date: 2026-10-27. Source: frozen P7 sessions under `/root/kroma-p7-abf`.
Detailed provider usage and summary text: `/root/kroma-p9-apec-ab/`.

## Experiment

- A: unchanged production chunk rule, `(123904 * 7 / 10)` = 86,732 bytes.
- B: candidate 100,000-byte chunk limit.
- Same WorkBuddy-backed configured model, existing Kroma `ModelClient::summarize`
  and `condense` implementation; identical instructions, history,
  1,024 target summary tokens and consolidation threshold.
- Three real Rust coding-history segments, each run twice per arm with
  AB/BA order reversal (6 runs per arm; 12 total). Each model summary
  was saved for inspection. Original histories were not modified.
- Scored user-requirement keyword presence as a **weak factual-recall
  proxy**; this is not an independent downstream coding-accuracy test.

## Completed aggregate (6 runs per arm)

| Metric | A | B |
|---|---:|---:|
| Initial chunks | 16 | 10 |
| Total model requests (including consolidation/retries) | 26 | 18 |
| Uncached input tokens, provider-reported | 15,622 | 64,097 |
| Output tokens, provider-reported | 29,472 | 25,132 |
| Wall time (ms) | 183,478 | 164,784 |
| Keyword requirements present | 42/42 | 42/42 |

Across the three frozen histories, B reduces requests by ~30.8% and
observed wall time by ~10.2%, but increases *reported uncached input*
by ~310%. This cost penalty is dominated by two B runs in which
truncation/retry coincided with low cache hits.

## Why the result is not a safe promotion

- B is not consistently faster in each paired run; in the holdout
  history, A beat B on both round wall times despite B having fewer
  model calls in round 1.
- B had unstable cache hits and truncated summaries on long cases.
  Its lower request count does not establish lower provider cost or
  lower total resource usage.
- Keyword presence is not evidence that exact paths, decisions and
  errors remain accurate. Downstream task success was not measured.
- Cases are correlated slices from one Rust CLI development workload.
  More independent repositories would be needed for general claims.

## Decision

**Keep A (production APEC chunking unchanged).** A 100-kB chunk limit
does not satisfy our quality-plus-cost promotion rule. Do not add a
dynamic selector, alternative compression implementation or broad
parameter search based on these results.

## Reproducibility

`KROMA_P9_APEC_LIVE=1 KROMA_P9_APEC_ROUND=1 cargo test --locked --offline context::p9_apec_ab_live::p9_apec_ab_live -- --ignored --nocapture`

Then reverse order by setting `KROMA_P9_APEC_ROUND=2`.

Holdout: `KROMA_P9_APEC_LIVE=1 cargo test --locked --offline p9_apec_ab_holdout -- --ignored --nocapture`.

Live tests are ignored by default, use existing local configured model
profiles and write only into `/root/kroma-p9-apec-ab/`.
