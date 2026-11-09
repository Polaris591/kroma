# Benchmark and experiment archive

**Reproducibility.** Evidence for these comparisons was written to
`D:/Kroma/experiments/...`, outside this repository (the repository root is
`D:/Kroma/kroma`). Those directories — `trials/`, logs, `metadata.json`,
`plan.json` and `summary.json` — are not committed, so a clone cannot recompute
the numbers quoted below; only a fresh experiment can be generated, with the
commands in each subdirectory README. The harnesses are standard-library Python
3.12 invoked as `uv run --no-project --python 3.12 python ...`, and the Rust side
uses this repository's own toolchain (`cargo build --locked`). No uv version is
recorded. Treat every number here as a historical record, not a result this
repository can re-derive.

This directory contains Kroma experiments, benchmark harnesses, local probes and
reports from different stages of development. Many depend on particular model
endpoints, private configurations, platform tools or now-superseded code. Treat
their numbers as **historical**, not as current speed or quality claims.

## Start here

- [Performance measurement guide](../docs/performance.md) defines the current
  opt-in local fixtures and explains what they can and cannot establish.
- [Agent task suite](agent_task_suite.py) and other Python runners are
  experimental tools; inspect their arguments and dependencies before running.
- [Architecture baseline](ARCHITECTURE_P0_BASELINE.md) and
  [experiment directive](EXPERIMENT_DIRECTIVE.md) document earlier evaluation
  assumptions, not mandatory architecture policy.
- Historical P-series reports record prior ablations and repairs.

## How to publish a new comparison

Record the exact Kroma and competitor commits, **same provider endpoint and
model ID**, inference settings, task corpus, seeds/order, repetitions, time
window, success criteria, usage reporting coverage and raw results. Report
latency distributions, request/tool-call counts, cache input and billed token
measurements separately. Do not label rough JSON/token estimates as API bills.

Experiments using live models may incur charges. Nothing in this archive is run
on startup, in normal project CI or during ordinary installation.

Consult the [current architecture](../docs/architecture.md) before applying
proposals from older reports.
