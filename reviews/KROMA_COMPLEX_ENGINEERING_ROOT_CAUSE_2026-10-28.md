# Kroma vs Pi: Complex engineering root-cause investigation

Date: 2026-10-28. Research complete at the level justified by available
evidence; **do not interpret this report as proof of universal harness
superiority**. Production Kroma source, provider config, sessions and Git
history were not changed by this investigation.

## Executive answer

**Observed causes of Kroma's slower Rust runs:**

1. **Longer agent trajectories**: unnecessary inspection/verification turns
   sometimes add substantial model round trips, and each extra request
   resubmits accumulated context. This is the clearest common cost amplifier
   in the initial WAL bug runs.
2. **Different validation work, not slower Bash**: on an independent Rust CLI
   feature, Kroma sometimes wrote additional regression tests and rebuilt or
   checked more often; this accounted for the measured wall-time gap. Pi did
   not meet the same self-imposed validation scope. This is a speed/coverage
   tradeoff, not unambiguously waste.
3. **Poorly visible decisive test evidence**: the agent piped target-test output
   through `tail`, removing the named `... ok` line while exit code was zero.
   On two WAL runs it then reran the same test to obtain that evidence.
4. **Environmental and benchmark contamination**: a Pi WAL run encountered
   `No space left on device`; earlier Rust runner orders had Pi first in
   every pair; archived source fixtures lack `.git`; the new Python complex
   task revealed `python` unavailable while `python3` was installed. These
   must not be confused with intrinsic harness quality.

**What the evidence does *not* show:** Kroma's Rust tool execution is inherently
slow; its core context/compaction pipeline is a significant foreground latency
bottleneck; Pi always plans better on complex projects; or adding multiple
basic tools / forcing batching yields a reliable gain.

## Research design and source evidence

- Existing WAL randomized crossover, 3 Pi/Kroma pairs; raw file:
  `/tmp/kroma-pi-crossover-96767baa/results.json`.
- Earlier five-arm and repeated prompt ablations; raw files:
  `/tmp/kroma-harness-ablation-6ce59495/results.json` and
  `/tmp/kroma-harness-ablation-bc94b731/results.json`.
- **New cross-language complexity matrix**, two trials per language/runner,
  counterbalanced start order; raw file:
  `/tmp/kroma-pi-complexity-1f72299c/results.json`.
- Read-only trace analyzer:
  `benchmarks/audit_pi_kroma_traces.py`.
- New independent acceptance runner and fixtures:
  `benchmarks/pi_kroma_complexity_matrix.py`.
- Offline deterministic evidence-loss regression:
  `benchmarks/test_evidence_loss_repro.py` (passed).

All model trials used the same WorkBuddy endpoint, API key and DeepSeek model;
Pi selected native High; experimental Kroma explicitly sent
`reasoning_effort=high` without the separate `thinking.type` field. Pi still
had its own native `bash`/`read`/`edit`/`write` tools, while Kroma used Shell.
This was a **whole-harness comparison**, not identical tool/prompt protocols.

## A. Existing Rust WAL crossover: why the apparent reversal was misleading

| Pair | Pi wall | Kroma wall | Pi input | Kroma input | Quality |
| --- | ---: | ---: | ---: | ---: | --- |
| 1 | 78.476 s | 99.447 s | 100,667 | 111,521 | Both pass |
| 2 | 81.350 s | 117.878 s | 116,560 | 201,334 | Both pass |
| 3 | 122.675 s | 89.574 s | 155,110 | 80,414 | **Pi had ENOSPC** |

In the first two runs, Kroma needed 29 requests vs Pi's 19, and 31 tools vs
25. Pi tools used 78.839 s; Kroma tools 101.189 s. The 57.499 s additional
Kroma wall time splits into +22.350 s tools and +35.149 s non-tool time.
Pi's non-tool duration is **not** a measured pure-model duration.

The extra five *late* Kroma requests in run 1 consumed 62,976 input tokens;
in run 2 they consumed 97,270. These are observed request-position totals,
not a valid causal counterfactual. Overall tool result text was *not*
consistently larger in Kroma. More repeated model context was the stronger
signal.

**The third Pi run is contaminated.** In Pi tool #7, compilation failed with
`error: failed to write /tmp/rustc.../symbols.o: No space left on device`
and prompted disk diagnostics and a changed `TMPDIR`. Its 122.7 s duration
cannot be interpreted solely as model randomness. Excluding or subtracting
individual time chunks cannot reconstruct an uncontaminated run.

### Specific repeated verification

- WAL pair 2 Kroma: after a successful targeted regression test piped through
  `tail -12`, the named success line was absent; the next shell call reran the
  same target test, costing **2.997 s** of tool time and an additional model
  interaction.
- WAL pair 3 Kroma: the same pattern added **3.218 s**. Pi's pair-3 disk
  failure still makes the overall paired timing unsuitable as a clean win.
- WAL pair 1 Kroma: a successful full suite was followed by format checking
  and another full-suite run, approximately **3.75 s** of additional tool time.

Kroma already supports more than one tool call per model response, but the
model did not exploit it consistently. In WAL run 1 Pi had 4 multi-tool
responses, Kroma 0; in run 2 both had 4; run 3 Kroma had 7. The previous
three-repeat 'encourage batching' ablation showed no stable gain.

## B. Different Rust task: implement `kroma config remove`

This was a real different code task across `src/profiles.rs`, CLI, README and
tests. The external verifier exercised CLI behavior, active-profile fallback,
unknown-profile non-mutation and key protection.

| Metric, 2 trials combined | Pi | Kroma |
| --- | ---: | ---: |
| Independent acceptance | 2/2 | 2/2 |
| Wall | **237.398 s** | 271.102 s |
| Input incl. cache | 819,715 | **794,672** |
| Uncached input | 67,843 | **64,560** |
| Model requests | **31** | 36 |
| Tool calls | 46 | **44** |
| Tool execution time | **46.39 s** | 86.09 s |
| Remainder of wall time | 191.01 s | **185.01 s** |

**Key result:** Kroma's combined wall deficit was **33.704 s**, but its
combined tool-execution overhead was **39.70 s**. It was NOT slower in the
remaining wall time. The decisive difference for this feature was how often
and how expansively tests/builds were run, not an intrinsic speed deficiency
in the Rust engine or model interface.

### Run-specific nuance and quality tradeoff

- `rust_config` repeat 2: Pi wall 118.988 s, tools 18.95 s; Kroma wall
  143.060 s, tools **55.32 s**. Kroma added a new integration test file
  (`tests/profile_remove.rs`), executed extra build/test phases and spent
  significantly more time compiling. Pi added no comparable new test file.
- `rust_config` repeat 1: Pi wall 118.410 s, tools 27.44 s; Kroma wall
  128.042 s, tools 30.77 s. Pi additionally ran Clippy, while Kroma added
  tests inside `src/profiles.rs`. The gap here was much smaller and not
  accounted for by extra tool execution alone.

Neither result establishes that extra Kroma testing was unnecessary or Pi's
solution was lower quality: the external acceptance does not score the
long-term value of newly authored tests. That difference must be controlled
in future timed benchmarks by specifying **the same validation obligations**.

## C. Complex Python: disproves a generic Kroma complex-task deficit

Task: fix four interacting modules in a persistent workqueue API: Unicode
normalization, dedup, stable ID assignment, round-robin assignee allocation,
atomic JSON persistence, completion and ordered reporting. Hidden acceptance
was run outside the agents' editable source directories.

| Trial | Pi wall/finish | Kroma wall/finish | Pi input | Kroma input |
| --- | --- | --- | ---: | ---: |
| 1 | 145.144 s, **timeout** | **36.109 s, completed** | 177,658 | 16,683 |
| 2 | 145.018 s, **timeout** | **41.973 s, completed** | 193,476 | 20,919 |

**All four final source snapshots pass the corrected hidden functional
checks.** Pi did not end its task before the predeclared timeout; its final
code happened to be functionally valid at termination. Kroma completed
normally in both runs. This is evidence against 'Pi always plans complex
work better', but it is one newly invented Python task, not a broad benchmark.

### Hidden verifier correction (critical)

The initial verifier incorrectly required `state['cursor'] == 3`, whereas the
task required persistent round-robin behavior and permits storing the cursor
modulo the number of assignees. The initial run artifact contains
`passed=false` for both Kroma Python runs **solely because of this
over-constrained hidden check**. After the verifier was corrected to test
actual assignment sequence and persisted state, both Kroma implementations
were independently **re-verified as functional**. The original raw report was
preserved; this correction is explicit rather than silently rewriting it.

### Pi's longer Python path

Both Pi runs used additional checks beyond the required behavior. It created
or revised extended tests, attempted parallel/process stress checks, and
repeated validation. Some tools started with `python ...`, but the benchmark
host had **`python3` and no `python` executable**, causing needless
environment discovery. This is a concrete, fixable benchmark environment
difference, though it cannot alone explain ~145 seconds of work.

The Python task asks for atomic replacement and cross-process persistence,
*not* concurrent mutation correctness; Pi's stress explorations were outside
the explicitly required acceptance. They may still have provided extra
quality assurance, which is not scored here.

## D. Why this happens inside the harnesses

1. **No Agent Loop speed bug identified.** Kroma's logged context preparation
   consumed ~0.09-0.31 s per WAL task, and ~0.60-0.66 s per new Rust config
   feature. Model calls and compiler/test execution dominate elapsed time.
2. **Tools expose evidence, agents choose requests.** Kroma's Bash Shell has
   `pipefail`, returns an exit code, and archives full output with a head/tail
   preview when large (`src/shell.rs`). But a *model-written* `| tail` or
   `| grep` pipeline can discard specific test-status lines before Shell sees
   them. The model may then retest to obtain the missing proof.
3. **Tool count does not equal efficiency.** Pi's independent inspections can
   execute in parallel (Pi agent-loop source supports it); Kroma executes
   same-response tool calls sequentially. However inspection calls in these
   Rust runs generally took milliseconds. Rust builds, not parallel file
   inspection scheduling, dominated the tool-time differences. Automatically
   parallelizing arbitrary Shell commands is risky and not supported as a
   fix by this evidence.
4. **Longer trajectories replay context.** More model requests multiply
   input tokens as accumulated tool history is resent. That explains why
   unnecessary exploration/verification frequently increases both cost and
   latency. It does not imply TRC/APEC is malfunctioning: no APEC pressure
   event was observed in these tasks.
5. **The earlier tool and prompt ablations were mostly negative.** Adding
   Edit/Write and encouraging batching gave mixed or adverse aggregate
   results; broad scoped-search prompts and cache-prefix changes did not
   reliably reduce cost. Do not turn these negative controls into features.

## E. Confirmed remediation vs hypotheses

### E1 — Immediate, low-risk fix: lossless test evidence and truthful status

**Confirmed defect mechanism:** with 16 trailing test-summary lines, `tail
-12` discards the named success even though the command returns 0. The
offline test `python3 benchmarks/test_evidence_loss_repro.py` reproduces this
deterministically; filtering by test name, summary and errors preserves it.

**Safe usage today:** Kroma already archives oversized Shell output. Prefer
running validation commands directly without piping them through `tail` or
`grep` and use the `exit_code` plus the archived complete stdout/stderr.

For a concise *model-authored* alternative, preserve actual exit status:

```bash
log=$(mktemp)
cargo test --locked --offline wal_append_controls_recent_session_order >"$log" 2>&1
status=$?
grep -E 'wal_append_controls_recent_session_order|test result:|error:|FAILED' "$log" | tail -25 || true
printf 'VALIDATION_EXIT=%s\n' "$status"
rm -f "$log"
exit "$status"
```

**Minimal future Kroma change:** amend the built-in Shell tool *description*
to advertise its already-existing full-output archive and explicit exit
code, and warn that filtering test output with a simple tail can hide the
decisive line. This is about one sentence and does **not** require an Agent
Loop redesign, new core tool, special Rust policy or LLM summarization.
Still perform one isolated A/B acceptance test before shipping: previous
prompt changes have had unpredictable effects.

If richer feedback proves necessary, consider a **small opt-in generic test
status tool** that runs a test command once and returns exact exit status,
named test results, error excerpt and full-log path. Keep it optional, not a
hard requirement for all users. Do not build a complex planner just for this.

### E2 — Keep checks proportionate to the task, not artificially minimized

Give the agent a short contract: identify the requested acceptance, run a
relevant focused test after edits, run the explicitly required suite, then
finish if no code changed and requirements are met. If tests fail, repair and
retry; never skip user-required tests purely to win benchmarks. **Requiring
every agent to write more tests** is a separate *quality policy* and must be
scored separately from fast completion.

Kroma already has broadly similar default guidance in `src/prompt.rs`, so
adding a longer generic pep talk is NOT the solution. The useful addition is
test-evidence specificity and a task-specific definition of done. Do not add
runtime stop checks that could prematurely terminate unfinished work.

### E3 — Repair benchmark validity before more model spending

- Reserve adequate disk space, check for ENOSPC before/during runs, avoid
  concurrent Rust builds and control `CARGO_TARGET_DIR`/`TMPDIR` consistently.
- Give both runners the same interpreter command (`python3` on this host),
  source root, no `.git` expectation and test obligations; avoid unrelated
  harness environment variables reaching agent subprocess tests.
- Randomize AND balance runner order *within* each language/task class, and
  use multiple **different** source-level issues, not just repeated WAL fixes.
- Score external acceptance *and* optional test coverage; report both.
- Report output, uncached, cached tokens separately; higher cached-token
  share is NOT a model cache hit rate and does not prove cheaper billing.
- When a run times out but its edited code happens to pass the hidden test,
  report **functional snapshot=true, agent completion=false**, not success.

### E4 — Non-recommendations

- No Rust-specific system prompt or mandatory Cargo wrappers in Kroma core.
- No speculative rewrite of TRC/APEC, caching, provider or TUI.
- No mandatory Read/Edit/Write expansion based on the previous mixed ablation.
- No compulsory parallel Shell execution or unbounded loops to 'verify more'.

## F. Decision and evidence strength

| Finding | Evidence strength | Decision |
| --- | --- | --- |
| Some Rust Kroma runs are longer due to extra requests/checks | Direct traces, reproduced across two WAL + two new Rust trials | **Confirmed description of measured cost** |
| Tail filtering can hide named successful test result | Two Kroma runs + deterministic offline proof | **Confirmed mechanism; low-risk fix available** |
| Difference in Rust CLI trial came from extra tool/validation time | Per-tool measured durations (2 independent runs) | **Confirmed decomposition; quality tradeoff** |
| Kroma is inherently bad at Rust | Contradicted by success + mixed win + adequate capability | **Not established** |
| Pi invariably superior on complex tasks | New Python trials contradict | **Not established** |
| One new sentence in tool description will reliably improve LLM speed | Not A/B validated | **Hypothesis, do not claim as proven** |

**Practical verdict:** maintain Kroma's simple runtime. Tackle feedback
quality and validation stop behavior first, with the smallest possible
changes. Keep correctness and the requested testing obligations invariant.
Only after reproducing an improvement on **different tasks** should this
be promoted into the default core.
