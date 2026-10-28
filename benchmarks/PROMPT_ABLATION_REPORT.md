# Kroma system prompt ablation — 2026-10-26

## Question

Can Kroma become faster by replacing its current system guidance with a
minimal prompt, concise positive instructions or negative constraints?

## Experimental design

- The **same compiled Rust Kroma test harness**, same `shell` tool schema,
  same saved WorkBuddy `deepseek` model with High thinking, same independent
  upstream boltons project and deterministic external acceptance tests.
- The test-only binary was compiled from a **copy** of the current Kroma
  source at `/tmp/kroma-prompt-ablation/runtime`, with one environment flag
  selecting the outgoing system text. The production binary/source was not
  changed or replaced for this experiment.
- A: unchanged initial Kroma system prompt and existing execution guidance.
- B: minimal identity, tool, working directory, accurate reporting.
- C: B plus short positive guidance to inspect relevant code, implement,
  verify and finish.
- D: B plus prohibitions on expanding task scope, pointless repeat checking
  and skipping required tests.
- Two tasks: IPv6 URL parsing bug repair and cross-file digit-masking feature
  (source, tests and docs). Each variant ran each task twice, with fresh
  source copies and alternate orders. 4 variants x 2 tasks x 2 repeats =
  **16 complete trials**. All variants also passed one initial read-tool
  calibration (2 model requests, 1 tool call each).
- No artificial agent time or token limits. External correctness was assessed
  with independent acceptance tests and existing upstream test modules.
  Runtime stderr/stdout and individual cases: `/tmp/kroma-prompt-ablation/`.

## Results

| Arm | Functional passes | Mean time, seconds | Median time, seconds | Total model calls | Total tool calls | Uncached input tokens | Cached input tokens | Output tokens |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| A current | 4/4 | **35.97** | **39.10** | **30** | **35** | **25,903** | **126,336** | **8,597** |
| B minimal | 4/4 | 83.49 | 82.82 | 48 | 60 | 33,150 | 294,656 | 15,600 |
| C positive | 4/4 | 83.28 | 83.85 | 41 | 57 | 29,896 | 179,456 | 12,769 |
| D negative | 4/4 | 70.10 | 66.62 | 40 | 50 | 26,187 | 200,832 | 11,776 |

### Individual wall time, seconds (repetitions 1 / 2)

| Task | A current | B minimal | C positive | D negative |
|---|---|---|---|---|
| IPv6 bug | 31.01 / 17.19 | 62.47 / 93.83 | 86.25 / 81.45 | 39.20 / 46.87 |
| Cross-file feature | 47.18 / 48.48 | 71.81 / 105.85 | 51.59 / 113.81 | 107.97 / 86.37 |

All 16 tasks passed their external and upstream tests and exited normally.
There were **zero model timeout retries and zero APEC failures**. Request and
token statistics come from provider-reported usage, not estimated text size.
Kroma input includes cache-read tokens; uncached input is `input - cached`.

## Interpretation

1. Under these workloads, **A is faster and consumes fewer model/tool calls
   than every alternative**. Simply deleting the existing rules is not an
   efficiency optimization. A was even faster than D, the next fastest arm,
   on all four individual trials.
2. Existing execution guidance seems useful in this test. It explicitly
   suggests batching independent inspections and stopping after necessary
   verification. Removing those hints changes model behavior even though
   tool implementation is identical.
3. This is **not** proof of the optimal general system prompt. Two small
   Python tasks with two repeats per arm do not establish performance on
   long-running Rust projects. Order effects, network variance, model
   stochasticity, and different task complexity are not eliminated.
4. This is a Kroma-only prompt study, **not** a Kroma-vs-Pi benchmark or an
   investigation of Pi's multi-tool API. The previous P3 difference may still
   arise from tool ergonomics, failures, context growth and agent strategies.

## Next step

Retain the existing Kroma prompt as default. If optimizing further, do a
**one-clause-at-a-time** ablation (batch-independent-inspections vs stop-after-
verification vs verification specificity) rather than removing all guidance.
Run those variants on the Rust continuous-development project, using the
same harness and fresh session copy per candidate, and compare completion
quality and cost before adopting a change. Keep the baseline unchanged.
