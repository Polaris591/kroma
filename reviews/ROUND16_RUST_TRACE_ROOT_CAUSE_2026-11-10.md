# Round 16: Why DeepSeek Overexplores the Real Rust Unicode Task (2026-11-10)

## Outcome / verdict

**Evidence supports a mixed cause: the test fixture omits the historical generator's required Unicode UCD inputs, while model/tool decisions still contribute avoidable work.** Do not claim that all exploration is a pure Prompt fault, nor that restoring the inputs would certainly eliminate it. **Do not promote the evidence-finishing prompt as a universal winner** based on this Rust case. No production or main branch changes.

## Observed versus inferred

1. **Source inspection**: upstream `scripts/unicode.py` is not a self-contained local generator. `fetch_open()` expects Unicode 17.0.0 UCD/emoji data from disk, otherwise downloads it from `unicode.org`. Round15's reduced historical checkout contained the generator and Rust tables but did not bundle those UCD inputs. The task explicitly demanded both source rule and generated output consistency. This creates a genuine obstacle.
2. **Observed replay**: at least one original baseline run attempted to locate `DerivedCoreProperties.txt` and retrieve it online; other trajectories introduced custom table decoding via Python, awk, Rust examples and temporary files. Several attempts failed because `python`/ `python3` launcher was absent, and generated Python expressions had regex/substring errors. The machine *does* have a Python 3.12 binary for benchmark runners; it was not presented to the model as a guaranteed project-local dependency. These are real tool errors, not automatically an Agent Loop bug.
3. **Upstream positive control**: fetched files from actual merged PR#89 head `fcc0e2d02aa6b53f6d2f14aa2d3ef7a431968ec0` (`scripts/unicode.py`, `src/gen/tables.rs`). `benchmarks/round16_upstream_positive_control.py` verified historical pre-fix source fails the intended hidden width tests and the upstream-fixed files pass **the same reduced offline Rust acceptance**, including generator consistency. The test target/negative control discriminates on the actual issue. This does not repair the missing UCD dataset or turn the harness into a full upstream checkout.
4. **Cost / phase categorization**: read-only WAL audit, not inferred chain of thought. The four trajectories contain 115 issued tool calls, 20 nonzero-exit results, and at least 13 scratch-script/create/decode commands by a conservative heuristic. The first attempted shell edit is not the same as the first actual project-source edit, and the previous round's first-edit counts can be misleading because they included scratch/probe edits.

## Traces, categorized by actually invoked commands

| Round15 replay | Tools | Nonzero exit tools | Scratch commands | First real source edit call | Source edits | Independent acceptance | Normal finish |
|---|---:|---:|---:|---|---:|---|---|
| Repeat1 baseline | 18 | 2 | 0 | none | 0 | fail | timed out |
| Repeat1 finish | 36 | 5 | 5 | **#27** | 2 | **pass** | timed out |
| Repeat2 baseline | 34 | 4 | 1 | **#33** | 2 | fail | timed out |
| Repeat2 finish | 27 | 9 | 7 | none | 0 | fail | timed out |

Categorization is intentionally conservative: many failed greps are ordinary no-match findings rather than tooling bugs, and shell commands combining multiple actions can hide intermediate errors. The `cargo` category includes `cargo test/run/build/check`, not only test invocations. No invented cost savings or average token claims from censored trials.

## Causal hypotheses and discriminating experiments

- **P1 — Environment completeness [source-supported + observed].** Missing UCD/emoji inputs led to a need for manual reconstruction of Rust generated tables. Best validation: restore the *version-pinned* Unicode 17.0.0 data into an independent full historical checkout; compare against the same model/endpoints and exact user task, **without revealing the repair solution**. A prompt tailored to unavailable data is not a general harness improvement. Counterexample: some genuine programming tasks require generating data without convenient access; code agent should still behave reasonably in that scenario, so don't solve only by always handing it data.
- **P2 — Project edit threshold [observed].** Counting first edit by any tool (scratch scripts included) artificially makes repair look earlier. Compare first *project source* edit and task correctness, not `edit_file` alone.
- **P2 — Tool environment [observed].** Explicit Python executable availability is inconsistent (`python` launcher failed although `python3.12` ran in later tasks). A consistent, discoverable environment would reduce false detours without modifying the model's safety policy. Validate via a one-line PATH diagnostic **from inside the harness environment** and compare model behavior only after making the tool available to both arms. Do not silently call it a Prompt success.
- **P2 — Model exploration [observed].** The model still makes broad file reads, failed greps, and brittle custom scripts. Existing short-vs-complete-function prompt ablations already exhibited quality/runtime trade-offs; no basis to add another global "edit sooner" rule. Prefer a new medium-complexity real Rust PR with local preconditions and full working tests, matched cross-over.

## Reproducibility

- Historical bad SHA: `d8654db078bf9f93b5bfe1e6294a477cc4addc28`.
- Genuine PR: https://github.com/unicode-rs/unicode-width/pull/89
- Genuine upstream post-PR head: `fcc0e2d02aa6b53f6d2f14aa2d3ef7a431968ec0`.
- Baseline and candidate WAL: `D:/Kroma/experiments/kroma-round15-rust-real-issues-v6/unicode-width-89/{1,2}/{baseline,finish}/session.json.wal`.
- Audit scripts: `benchmarks/round16_rust_trace_audit.py` and `benchmarks/round16_rust_failure_categorization.py`.
- Positive-control script: `benchmarks/round16_upstream_positive_control.py`.
- Fixed original source files: `benchmarks/round16_upstream_fixed/`.
- Positive-control build/output: `D:/Kroma/experiments/kroma-round16-rust-positive-control`.
- Earlier full A/B: `reviews/ROUND15_REAL_RUST_UNICODE_WIDTH_2026-11-10.md`.
- Code quality: cargo fmt --check and git diff --check passed after auditing. This round was read-only for Kroma product code; benchmark helper scripts and report were added.

## Decision

Leave production prompt unchanged. Continue using evidence-finishing as the top candidate for simpler tasks; first correct the **benchmark setup and evaluation metrics** before retesting complex Rust. Do not fit a new global prompt against missing data and unsupported Windows command aliases. A reliable general efficiency improvement requires repeated authentic medium/complex Rust issues that begin with working prerequisites and have valid hidden acceptance.
