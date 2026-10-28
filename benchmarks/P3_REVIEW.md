# Kroma P3 — real multi-stage development comparison and targeted repair

Date: 2026-10-26. Base checkout: `ed276fc` plus P2 changes to `src/agent.rs`.
Model: WorkBuddy gateway, `deepseek`, High thinking on both runners.

## Test design

- The original Rust Kroma source, including the uncommitted P2 Agent changes,
  was copied into two byte-identical independent workdirs. The source-copy
  digest was verified before starting either runner.
- Each runner built the same project over four sequential stages, with the
  process restarted while preserving the conversation after every stage:
  `config remove`, `config rename`, `config show/list --json`, then integrated
  Review with Rust checks. Order alternated by stage. The agents were told
  not to touch the main checkout, access credentials or commit Git changes.
- No Benchmark Token quota, deadline or forced process timeout was applied.
  Normal provider transport timeouts and test-process errors remained possible.
- Stage validation used fresh private *dummy* profile files and an external
  CLI checker for atomicity, state selection, JSON shape, credential non-
  disclosure, and file permissions. Rust compile, tests, fmt and Clippy were
  also checked. Exit status and feature correctness are tracked separately.
- Source workdirs, private execution logs, metadata and machine-readable
  results: `/tmp/kroma-p3-realdev/` (especially `results.json`).

## Raw P3 results — before any P3-C repair

| Phase | Pi wall time | Kroma wall time | Pi correctness/exit | Kroma correctness/exit |
|---|---:|---:|---|---|
| Remove profile | 142.099 s | 324.217 s | pass / 0 | pass / 0 |
| Rename profile | 112.635 s | 114.311 s | pass / 0 | pass / 0 |
| JSON output | 102.743 s | 136.388 s | pass / 0 | pass / 0 |
| Final review | 138.312 s | 148.632 s | pass / 0 | pass / 1 |
| **Total** | **495.789 s** | **723.548 s** | **4/4 passed / 4 normal exits** | **4/4 passed / 3 normal exits** |

Pi completed the same requirements in 31.5% less total wall time. Both
codebases passed all external checks. The Kroma final-review process failed
despite leaving passing code, so this is a **runtime stability failure**.

### API requests and tokens (provider-reported)

| Metric | Pi | Kroma |
|---|---:|---:|
| Completed model requests | 34 | 44 |
| Completed tools | 40 | 52 |
| Uncached input tokens | 45,075 | at least 119,821 |
| Cached input tokens | 1,496,832 | at least 1,663,872 |
| Output tokens | 52,860 | at least 61,222 |

Pi reports uncached input and cache reads as separate counters. Kroma's
provider-reported input includes cached tokens, so the comparable Kroma
uncached quantity is `input - cached` per response. Kroma's final review had
no normal terminal task aggregate; its six completed request usage records
were summed individually. **Four failed APEC summary attempts did not
contribute known billable usage** and are not included in Kroma's totals.
These are lower bounds, not a verified price comparison. Cached-token
discounts and actual provider billing are not asserted.

## Confirmed root cause

During the final review, Kroma repeatedly logged:

`[apec] summary failed: APEC summary was truncated by the model`

The default APEC summary output `max_tokens` was the target summary length
(up to 1,024 tokens); a thinking model can spend this entire allowance on
reasoning and return `finish_reason=length`. Furthermore, a failed APEC
attempt was suppressed by a *compaction revision* marker, but successful TRC
changes advanced the revision and removed that suppression. Kroma retried
the same failing historical snapshot four times and ultimately exited with
status 1 when compression was required to stay under the hard context budget.

## P3-C targeted changes (not included in the competition result)

1. For no-tools APEC completions, reserve additional completion headroom for
   reasoning; on `finish_reason=length`, increase the allowance for a bounded
   retry instead of treating incomplete text as valid memory. Preserve the
   concise text target, full raw transcript and explicit error on exhaustion.
   Count completed retry requests and usage when known.
2. Suppress repeated failed APEC starts for the current **user turn**, even
   when TRC revisions change. Permit a later user turn to retry automatically.
   No changes to the TRC/APEC policy, archive format, tools or model setting.
3. Added a self-contained local HTTP integration test simulating a truncated
   APEC completion followed by a successful one. It verifies expanded output
   allowances, correct retry count, token aggregation and no partial-summary
   acceptance; added a regression assertion for the same-turn failure guard.

### P3-C verification

- Rust unit tests and ignored-test status: see final cargo logs.
- `cargo fmt --check`, offline Clippy with `-D warnings`, offline Release
  build, and a real model smoke test.
- Crucially, we copied the **actual P3 failed Kroma session snapshot** to an
  independent checkpoint path and resumed with the updated Kroma binary,
  retaining the same source workdir and configured model. It returned the
  required marker correctly in **67.57 s**, exited 0, committed **one APEC
  summary**, had zero APEC failures and left a normal task profile.
- This proves the previously failed history is resumable and APEC can commit
  with the fix. It does **not** prove that all future large-history workloads
  are bug-free or that Kroma has become faster than Pi.
- A second launch of that same repaired checkpoint then succeeded again in
  **10.77 s**, with exit code 0, the correct response, and no APEC failure.

## Next engineering priorities

- Kroma used **10 more completed model requests and 12 more tools** over this
  test. Inspect the higher tool churn before deciding whether to add native
  read/edit tools or modify batching guidance. No changes should reduce
  functional verification quality.
- Further work needs larger projects and multiple independent repetitions.
  A single longitudinal test has uncontrolled model/provider variance.
- The finished `remove`, `rename` and JSON config implementations remain in
  the two test workdirs. **They have not been merged into the live Kroma**:
  review which implementation to adopt before changing production code.
- Original P3 baselines remain untouched and are not retroactively replaced
  by P3-C results. No Git commit was made.
