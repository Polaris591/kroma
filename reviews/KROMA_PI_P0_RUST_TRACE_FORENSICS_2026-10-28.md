# P0: Pi vs Kroma Rust task — execution-trace forensic review

Date: 2026-10-28. **Read-only log analysis; no new model/API requests.**

## Executive verdict

The evidence does **not** support a claim that Kroma's Rust execution is
fundamentally slower than Pi's Bash implementation. It supports a narrower,
actionable observation: on two relatively clean matched runs of the *same*
Rust WAL regression, Kroma took **five more model turns per run**, which
compounded history/input tokens and introduced additional inspection and
verification actions. The third crossover is **environment-contaminated**:
Pi encountered actual filesystem exhaustion during compilation. Do **not**
use that run to infer a clean reversal or model-sampling advantage.

This is one regression task repeated three times, not three distinct Rust
engineering problems; nothing here establishes a Rust-language-specific
disadvantage versus a general complex-task trajectory problem.

## Reproduction and provenance

Analysis script:

```sh
python3 benchmarks/audit_pi_kroma_traces.py \
  --root /tmp/kroma-pi-crossover-96767baa
```

Input sources:

- `/tmp/kroma-pi-crossover-96767baa/results.json` (paired outcomes/usage)
- `/tmp/kroma-pi-crossover-96767baa/rust_regression/<repeat>/pi/stdout.log`
  (Pi structured model events, tool start/end, timings and results)
- `/tmp/kroma-pi-crossover-96767baa/rust_regression/<repeat>/kroma/stderr.log`
  (Kroma request/profile counters and timings)
- `/tmp/kroma-pi-crossover-96767baa/rust_regression/<repeat>/kroma/session.json.wal`
  (Kroma durable tool order, commands and actual result payloads)

Both used the WorkBuddy `deepseek` endpoint/model/key with High selected.
Benchmark Kroma explicitly selected `reasoning_effort=high` with the distinct
DeepSeek `thinking.type` disabled; Pi used its native High setting. Pi's
model completion cap and both harnesses' instructions/tool affordances were
not made identical. This is a **whole-harness comparison**, not an isolated
language-model API experiment.

## Matched Rust crossover runs

| Run | Pi wall | Kroma wall | Pi requests | Kroma requests | Pi input | Kroma input | Note |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| 1 | 78.476 s | 99.447 s | 9 | 14 | 100,667 | 111,521 | Neither reported disk error |
| 2 | 81.350 s | 117.878 s | 10 | 15 | 116,560 | 201,334 | Neither reported disk error |
| 3 | 122.675 s | 89.574 s | 13 | 11 | 155,110 | 80,414 | **Pi compiled with ENOSPC** |

All six runs ultimately passed the independent Rust regression. In the first
two pairs, Kroma was **36.0% slower in summed wall time** and consumed **44.0%
more summed input**. These are descriptive results with **n=2** and no
generalization claim. Across all three pairs including the contaminated run,
the smaller difference conceals this important asymmetry.

### Where did the clean-run wall difference accumulate?

| Metric, runs 1+2 combined | Pi | Kroma |
| --- | ---: | ---: |
| Whole-run wall | 159.826 s | 217.325 s |
| Actual tool execution | 78.839 s | 101.189 s |
| Non-tool time (wall minus tools) | 80.987 s | 116.136 s |
| Model time, directly logged | Not available | 113.855 s |
| Model requests | 19 | 29 |
| Tool calls | 25 | 31 |

Pi tool duration is from each `tool_execution_end.durationMs` event. Kroma
tool/model durations come from `[profile]` counters. **Pi's non-tool time is
not a measured pure model duration**: it includes SDK startup, orchestration
and other overhead. This distinction matters.

Of the 57.499 s additional wall time for Kroma, 22.350 s (38.9%) was measured
tool time and 35.149 s (61.1%) was non-tool time. That is a decomposition,
**not** proof that a specific implementation caused those differences.

### Context cost: turns matter more than output volume here

In run 1, Kroma's 14 requests versus Pi's 9 meant its **last five requests**
accounted for 62,976 input tokens; in run 2, the last five accounted for
97,270 tokens. These are observed late-turn totals, **not** a causal estimate
of how many tokens Pi's plan would have cost inside Kroma.

| Tool result text volume | Pi | Kroma |
| --- | ---: | ---: |
| Run 1 | 61,050 chars | 39,650 chars |
| Run 2 | 58,776 chars | 58,701 chars |

The old claim that **larger raw Kroma tool outputs are consistently the main
cause** is not supported by these cleaner runs. The decisive observable
difference is more model turns and the subsequent replay of accumulated
context. Kroma's first-request prompt was actually smaller than Pi's.

## Behavioral root-cause candidates visible in tools

### A. Independent inspection batching is inconsistent, not missing

| Run | Pi multi-tool model responses | Kroma multi-tool responses |
| --- | ---: | ---: |
| 1 | 4 | 0 |
| 2 | 4 | 4 |
| 3 | 4 | 7 |

Kroma's Agent Loop does support multiple calls in one response. Pi's first
few responses batched independent source inspections in runs 1 and 2; Kroma
did not do so in run 1. But in run 2 both batched four responses and Kroma
still needed five extra model turns. Therefore batching **alone** cannot
explain the gap and the prior 'force more batching' prompt did not produce
stable improvement in three-run ablation.

### B. Verification evidence was obscured by user-written shell pipelines

- **Kroma run 2**: tool call 14 ran the post-edit targeted test and piped it
  through `tail -12`. Although the process reported success, the returned
  tail did **not** contain the named `wal_append_controls_recent_session_order
  ... ok` line. Tool call 15 reran the same targeted test with a `grep`
  filter to obtain the explicit evidence, adding **2.997 s** of tool time.
- **Kroma run 3**: tool call 14 had the same omission and tool call 15 reran
  the test to print the named success line, adding **3.218 s**.
- **Kroma run 1**: after a successful full-suite run, a format check was
  followed by another full-suite check (about **3.75 s**). This is a
  plausible redundant validation but not by itself an identified defect.
- **Kroma run 2**: after a successful full suite, an additional `cargo check
  --all-targets` (about **3.86 s**) ran. The final shell's `check-exit:1`
  was the exit from the following `grep` match, **not** proof Cargo itself
  failed. This illustrates how composed shell commands can obscure results.

This provides a concrete, testable mechanism: **when the assistant's own
pipeline drops decisive test evidence, it may request more tests**. The
source is the model-generated command/feedback format, not a hardcoded
runtime obligation to repeat Rust tests. Do not add Rust-only rules to the
core without cross-language validation.

### C. Exploration and edits

- Run 1: first attempted edits were tool 9 for Pi (failed text replacement),
  tool 10 for Kroma; Pi then corrected the edit in tool 10. Neither agent
  found a perfect fix on its first attempt every time.
- Run 2: Pi's first Edit was tool 11; Kroma first edited on tool 13, then
  corrected an unnecessary `Result`-style helper with another edit on tool
  14. Pi needed fewer inspection/correction rounds in this pairing.
- Both sometimes probed `git status` on `git archive` fixtures without
  `.git` metadata. This is a small artificial task-environment distraction,
  not a Kroma-only deficit.

All final changes passed the specific hidden regression; the benchmark does
not establish broader maintainability or defect rates beyond that check.

## Critical contamination in run 3

Pi tool call 7 failed to compile because the machine could not write Rust
temporary object files:

`error: failed to write /tmp/rustc.../symbols.o: No space left on device (os error 28)`

Some compound shell commands still emitted tool `isError=false` because their
final pipeline command exited successfully despite compilation failures.
Pi then spent **7.146 s** in one filesystem diagnostic call, inspected disk
and Cargo paths, and used `TMPDIR=/root/kroma-tmp` to recover. The replacement
targeted test took **30.040 s**; finally the full suite passed.

This is a nonrandom environment effect. Moreover, the recorded randomized
schedule placed **Pi first in all three Rust pairs**, so Rust compiler cache
warmth and run order were not balanced despite randomization of the overall
pair list. Removing the failed 15.159 s compile and 7.146 s diagnostic from
122.675 s does **not** produce a valid counterfactual time, because the
remaining recovery command also changed the compilation environment. The
third run must be treated as compromised, not arithmetically 'corrected'.

## What we have and have not learned

**Supported:** Kroma sometimes makes more inspection and verification turns
than Pi on complex Rust work; additional model turns can dominate cumulative
token consumption. Test-result pipelines are a concrete source of duplicated
verification in two of the three Kroma runs. Kroma's runtime can already
batch tools and executes Bash with `pipefail`. The only Kroma-fast Rust pair
here was contaminated by a demonstrated Pi disk failure.

**Not established:** Pi's intrinsic model inference speed is higher; adding
Read/Edit/Write to Kroma solves the issue; the defect is Rust-specific;
prompt-only batching/search tweaks are reliably beneficial; or Pi would
have definitely won in run 3 without ENOSPC. Distinguish behavioral outcomes
from mechanisms and mechanisms from robust generalization.

## Recommended next steps, in order

1. **Fix benchmark controls before buying more API calls:** reserve several
   gigabytes of free disk, prohibit concurrent builds, balance A/B and B/A
   runner order **within each task class**, preserve identical initial repo
   states and warm build targets symmetrically. Distinguish test command exit
   from pipeline exit, and fail/flag a trial immediately when ENOSPC occurs.
2. **Cross-language complexity test:** add one genuinely multi-module Python
   task of similar navigation/state complexity and one *different* Rust task.
   Use hidden acceptance. This is needed to distinguish Rust-specific behavior
   from general complex-project search and verification behavior.
3. **Tiny scoped experiment:** evaluate whether showing complete relevant
   test status and preserving the actual process exit in model-visible tool
   results reduces duplicate verification. Change only output presentation;
   do **not** add compulsory early stopping or rewrite the Agent Loop.
4. Retain an **evidence-based stop rule** for validation: after an unchanged
   codebase passes the required targeted and full checks, check that the
   decisive status was actually visible before deciding any repeat is needed.
   Validate on more than Rust, because Python runners also hide status with
   `tail`, `grep`, or pytest flags.

No production `src/` modifications, deployment, Git commit or new model calls
were performed for this P0 forensic review.
