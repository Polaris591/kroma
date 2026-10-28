# Kroma — real coding task reliability review

Reviewed: 2026-10-26; source checkout: `/root/kroma-new`.

This is an **independent engineering review**, not an A/B/F benchmark arm.
The production runtime and prompts were not changed during this review.
P7 continued to use its frozen experimental source and original evaluation
schedule.

## Verified positives

- P6 completed **26/26** independently validated Rust stage trials (18
  continuous development and 8 clause-level ablation runs). P6 telemetry
  recorded 18 APEC model requests and no APEC error.
- P7 completed **12/12** independently validated Python coding trials
  when this review began. A/B/F each achieved 4/4. Every one of the six
  Python feature trials modified all three required locations:
  `boltons/strutils.py`, `tests/test_strutils.py`, and
  `docs/strutils.rst`.
- The first three P7 Rust configuration-removal trials (A/B/F) passed
  their external CLI checks, updated the root help and README, and
  included relevant Rust tests. This is **not** a claim that the rest of
  the 60-stage P7 Rust suite has already passed.
- Provider streaming and compaction contain meaningful invariants:
  complete responses are checked for a valid finish reason; truncated
  APEC summaries are rejected; tool calls/results are durably journaled;
  a process restart does not automatically rerun unfinished shell calls.

## Issue 1 — pipeline can mask a failed test (P1, reproduced in actual traces)

The single Shell tool currently invokes Bash without `pipefail`.
For example:

```bash
cargo test --locked --offline 2>&1 | tail -30
python3 -m pytest -q tests/test_p1_regression.py 2>&1 | tail -40
```

Without `pipefail`, Bash reports the exit code of `tail`, rather than
the failed test process. The returned JSON `exit_code:0` is faithful
to the Bash pipeline but **does not mean the test passed**.

Read-only analysis of 475 historical shell-tool results from the P6
development/ablation sessions and the 12 P7 Python sessions found:

- 79 tool results whose commands appeared to combine a test/check with
  a pipeline (heuristic query).
- 77 of those pipelines reported shell exit code 0.
- **Three concrete false-success cases**, where the tool result reported
  exit 0 while the captured output included an actual failure:
  - P6 narrow F1 `cargo test ... | tail -30`: compiler error,
    `could not compile kroma`.
  - P6 narrow A2 `cargo test ... | tail -40`: compiler error,
    `could not compile kroma`.
  - P7 Python bug F2 `pytest ... | tail -40`: failure,
    `1 failed`.

All three trials **eventually passed** external acceptance. This evidence
does **not** show that Kroma shipped a broken fix in these cases; it
shows the tool status may mislead the model if the output is not read
carefully. A simple global `pipefail` default would catch these
examples, but it also changes semantics of legitimate `... | head`
commands (SIGPIPE). A controlled tool-behavior regression study is
recommended before changing the official Shell executor. Independently,
test commands should check the underlying status explicitly and avoid
`...; echo success` masking failures.

## Issue 2 — partial streaming response is visible but not journaled (P2)

One offline local HTTP mock emitted a valid partial SSE text delta and
closed before sending a successful completion finish reason.
Using the **P7 experimental binary with the original A system guidance**
and an isolated temporary session, the test observed:

- The partial reply marker appeared in stdout.
- Kroma exited with code 1 and reported the missing finish reason.
- The reconstructed persisted session contained only `system` and
  `user` messages, without the partial assistant text or an interruption
  marker.

The tool/source path is clear: `src/main.rs` streams text immediately;
`src/agent.rs` only appends the assistant response after a complete
model result. This is generally correct for avoiding incomplete model
messages, but the next resumed session lacks a **durable indication**
that a visible reply was interrupted. A minimal interruption note (not
blindly retaining and replaying the partial text) would improve clarity.
No real-model mid-stream failure has been observed in P7 so far.

## Issue 3 — recurring inspection and verification overhead (P2)

P6 A/B longitudinal traces classified 15/20 **exact duplicate Shell
commands** and 14/23 verification-looking commands respectively. The
classifications are regex heuristics, not semantic labels. P7 first
Rust stage also exhibited repeated inspections after initial discovery.
Improvements to execution prompts should preserve required verification;
neither blindly skip tests nor add a hard call cap.

## Issue 4 — dirty production checkout and limited task variety

The checked-out Kroma baseline has six modified tracked source/docs
files and numerous untracked benchmark reports/scripts; the committed
HEAD alone does not reproduce the running production candidate. P7
recorded a frozen source digest, but release management should later
commit the reviewed production implementation separately from benchmark
artifacts. Do **not** commit or reset while the experiment is active.

The current workloads cover Rust profile management and two Boltons
Python features. They provide useful evidence, but are not sufficient
to generalize to arbitrary large refactors, migrations, dependency
breakages, interactive debugging, or complex user projects.

## Recommended minimal remediation order

1. After P7 finishes, run deterministic shell-status regression cases
   for failing tests piped to `tail`/`grep`, ordinary successful
   tests, and intentionally cut-off `head` pipelines. Choose the
   smallest reliable status-reporting approach; do not add a new
   policy/approval framework.
2. Add a regression test for partially streamed replies and a durable,
   unambiguous task-interruption record. Do not persist invalid partial
   assistant tool calls.
3. Optimize redundant inspections with a targeted prompt/tool
   experiment, measuring all required test coverage.
4. Keep P7 A/B/F evaluation separate and promote only reproduced,
   correctness-preserving improvements. Cleanly commit validated
   production changes after the experiment completes.

**Review status:** evidence-backed issues identified; no production
implementation changed and no Git commit performed in this review.
