# Kroma P2: evidence-based Agent loop improvement

Baseline: `ed276fc` (Rust Kroma v0.4). Date: 2026-10-26.

## Root-cause review

- P1 demonstrated that **model requests dominate** short engineering tasks. For
  example, a 61.27 s feature implementation spent 57.56 s in the model,
  3.70 s in the shell, and under 3 ms on context planning and journaling.
  For a 39.81 s continuous-development stage, model time was 39.28 s.
- A P1 continuous-development stage hit its 70 s deadline after **11 completed
  model requests and 15 shell tools**. The requested stage-one acceptance test
  already passed, but the agent continued to investigate unrelated doctest
  details, repeatedly inspecting the same sources. This is unnecessary tool
  exploration, not evidence of a slow TRC/APEC implementation.
- Context planning and journaling were consistently negligible in these
  samples. APEC finalization was 0 ms. These short tests do **not** stress
  long-history compression, so there is no justification for redesigning it.
- Tool calls are currently serial by design. Independent reads may be batched
  in one model response, but running arbitrary shell commands in parallel
  without dependency analysis could break correctness. No parallel execution
  was added.

## Minimal changes

1. Replaced the old projection-only batching hint with stable execution
   guidance: focus on relevant files, request independent inspections together,
   run required and relevant regression tests, investigate real failures, and
   stop after the work is verified. Do not run unrelated doctests or repeat
   successful checks. No tool cap, confidence heuristic, new API request,
   history deletion or compression change.
2. When `KROMA_PROFILE=1`, record `model_start`, `model_done`, `tool_start`
   and `tool_done` events. A cancelled or timed-out task now leaves a clear
   last-known phase. No added logging on the default path.
3. Updated benchmark parsers to select task-total `tool_calls` from the final
   profile line, not the new per-round telemetry. Fixed the Pi subprocess
   fixture to close stdin, preventing spurious zero-event startup stalls.

## Before / after (same WorkBuddy DeepSeek High, independent boltons copies)

| Task | P1 baseline | P2 replay | Interpretation |
|---|---|---|---|
| Continuous development, repeat 2 stage 1 | 70 s timeout, 11 complete model requests, 15 tools | 50.82 s, 6 model requests, 6 tools, passed | No unrelated doctest investigation loop |
| Continuous development, repeat 1, both stages | 69.39 s, passed | 69.98 s, passed | Essentially unchanged, not a speed win |
| Continuous development, repeat 2, both stages | Never reached stage 2 | 105.24 s total, both passed | Better completion, but slower run-to-run variation remains |
| IPv6 bug fix, repeat 1 | 33.71 s, 9 requests, 14 tools, passed | 30.11 s, 9 requests, 11 tools, passed | Fewer tools, modest speed change |
| Cross-file feature, repeat 1 | 61.27 s, 10 requests, 14 tools, passed | 41.03 s, 9 requests, 11 tools, passed | Fewer calls; single-run time not causal proof |

The single model/proxy remains a source of run-to-run variance. The tested
project is a Python utility library, not a large multi-language production
repository. Results must not be extrapolated into guaranteed cost or speed
improvements. Kroma's `input` token count includes cached tokens, whereas Pi
reports cache reads separately; any monetary comparison must normalize both.

## Verification

- Rust unit tests, `cargo fmt --check`, `cargo clippy --all-targets -D warnings`,
  and offline release build.
- Real provider smoke: the model used the shell tool, returned the correct
  marker, and all start/done events paired as expected.
- External Python acceptance tests for two consecutive P2 continuous runs,
  one bug-fix run, and one cross-file feature run all passed. The unchanged
  upstream boltons tests also passed during these replays.
- Main checkout remains on `master`, with no Git commit. The original
  untracked `run_agent_bench.py` was not modified.

## Remaining risks / next phase

- The actual bottleneck is model round-trips, not Rust CPU. Further claims
  require more repetitions, provider-side first-token latency, and harder
  independent projects. Do not optimize toward a single benchmark's prompts.
- There is still no evidence to justify arbitrary parallel tool execution,
  hard turn caps, aggressive TRC truncation or a wholesale Agent redesign.
- Kroma may still take longer on some valid tasks; the new guidance is an
  optimization hint, not a deterministic early-exit mechanism.
