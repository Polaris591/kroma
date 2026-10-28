# P4.1 — Execution trace review (before modifications)

Source evidence: `/tmp/kroma-p3-realdev/results.json`, plus individual
read-only P3 run logs and Kroma's saved session checkpoint.

## What P3 measured

| Metric | Kroma | Pi |
|---|---:|---:|
| Development correctness (all stages) | 4/4 | 4/4 |
| Agent exits normally | 3/4 | 4/4 |
| Elapsed across stages | 723.548 s | 495.789 s |
| Completed coding-model requests | 44 | 34 |
| Shell/Bash calls | 52 | 40 |

The ~120 s extra delay caused by a single Kroma HTTP request timeout is
real, distinct from an artificial Benchmark cap. Removing that delay as a
diagnostic approximation leaves ~108 s of the initial time gap, but it is
**not a new measured run**.

## Concrete excess actions, not speculation

For the initial `config remove` task, Kroma issued 26 shell calls against
Pi's 18. Before first editing the source, Kroma used ten inspection calls,
including separate README searches, toolchain checks and workdir scans.
It then used several rounds of `cargo fmt`, `cargo fmt --check`, file
timestamp inspection, Git checks in a copied directory without a `.git`,
and separate additional tests. Pi used fewer inspections and verification
calls but passed the same external acceptance. Not all extra checks were
pointless: Kroma's version contained more unit tests (16 versus 13 in the
final Rust module). Consequently, **do not optimize by skipping tests**.

## A concrete tool mismatch

- Kroma only offers a `shell` tool, implemented as `sh -lc` on Unix.
- P3 Pi actually invoked its `bash` tool, not dedicated read/edit tools.
- At least one Kroma command used `${PIPESTATUS[0]}` and tried Bash-style
  checking. The Kroma stage-one transcript contained a nonzero shell result.
  POSIX `sh` is not obliged to implement Bash's `PIPESTATUS` or `pipefail`.
  This interface/tool-language mismatch may cause unnecessary retry turns.
- Before adopting a different shell, **test Bash versus sh with the same
  exact model, prompt, tasks, tool schema and external correctness checks**.

## Other known issues

- P3's APEC summary truncation failed repeatedly, and failure suppression
  was wrongly reset on unrelated TRC revisions. P3-C fixed the underlying
  condition; the saved failing session was resumed twice successfully.
- In normal tasks, model response time dominates; context preparation and
  session writes are each small compared with model roundtrips.
- The 16-run four-prompt experiment found the existing Kroma system prompt
  faster and with fewer requests than the three replacement prompts tested.
  Hence no system-prompt replacement in P4.

## Hypotheses to test (do not assume causality)

1. Bash execution compatibility improves correctness/efficiency relative to
   sh when the model generates Bash syntax.
2. A more precise tool description may reduce trial-and-error commands.
3. Redundant length/byte metadata in normal output may be discarded from
   the tool-result JSON without harming execution; compare as a distinct arm.

Any deployment must account for portability: not every Unix system has
Bash, and Windows uses cmd. Preserve safe fallbacks and opt-in behavior if
results do not justify changing defaults.
