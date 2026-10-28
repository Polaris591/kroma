# Kroma P4 — execution efficiency and Pi comparison

Date: 2026-10-26. Source: new Rust Kroma `/root/kroma-new`, `master`.
No Git commit. The main source checkout already had uncommitted P2/P3-C
changes; P4 retained them and did not overwrite previous experiments.

## P4.1: Trace diagnosis

Read `benchmarks/P4_EXECUTION_REVIEW.md` for concrete P3 command sequences.
Kroma's P3 `config remove` run made 26 shell calls versus Pi's 18; the
source inspection/verification was spread across more turns. A single
120-second model transport timeout also contributed heavily to the old
score. P3's repeated APEC `length` failures had already been repaired
and recovery-tested in P3-C.

## P4.2: Isolated tool ablation

The experimental binary was built from a separate copy of Kroma, retaining
the identical model, system prompt, context policy and task fixtures. Four
arms differed only in shell execution dialect, tool descriptor, and returned
JSON metadata. Each completed the Boltons IPv6 regression fix and cross-file
digit-masking feature twice, in fresh workdirs. No task wall-time limit or
token quota was applied. Each run also passed the existing upstream tests.

| Arm | Meaning | Correct | Mean wall | Model requests | Tool calls | Noncached input | Cached input |
|---|---|---:|---:|---:|---:|---:|---:|
| A | `sh` + original descriptor/result | 4/4 | 60.12s | 36 | 43 | 32,951 | 155,520 |
| B | Bash when available | 4/4 | 66.57s | 30 | 38 | 21,301 | 106,240 |
| C | B + precise tool descriptor | 4/4 | 55.55s | 30 | **33** | 27,232 | 125,952 |
| D | C + omit redundant inline byte counters | 4/4 | **49.61s** | **29** | 36 | 26,715 | **105,600** |

Per-task variation was substantial: C won the first bug run but was slower
than A on the first feature run. D won on speed in three of four matched
tasks but lost the second bug run. This is a small sample with model/provider
variance. Full results: `/tmp/kroma-p4-tools/results.json`.

**Production selection:** D is the lowest mean runtime and model-request
count in this sample, with all correctness checks passing. We adopted its
minimal mechanisms with a Bash-if-installed, POSIX-sh fallback on Unix,
and platform-appropriate shell descriptions. Inline tool outputs omit
`stdout_bytes`/`stderr_bytes` *only* when no capture-file reference exists;
exit_code and complete stdout/stderr remain. File-backed output metadata
remains intact. No changes to system prompt, TRC/APEC, or permission policy.

## P4.3: Engineering verification

- Rust tests: **58 passed, 4 ignored, zero failed**.
- `cargo fmt --check`, offline Clippy with `-D warnings`, and offline Release
  build passed.
- Added tests for Bash-only syntax, accurate inline shell results, and
  preservation of tool status while omitting redundant byte counters.
- Kept HTTP transport request timeout behavior unchanged; no evidence yet
  that longer or unrestricted per-request waits would improve stability.
  User-requested absence of **task-level** time/token caps remains intact.

## P4.4: Same-model Pi regression comparison

The updated Release Kroma was compared directly against Pi Coding Agent
1.3.0 with WorkBuddy DeepSeek High. Same isolated Boltons code fixtures,
same task text and external acceptance checks. 2 repeats per task for both
agents, alternate order, no whole-agent deadline/token quota.

| Trial | Kroma elapsed | Pi elapsed | Both correct |
|---|---:|---:|---|
| IPv6 bug, repeat 1 | **28.006s** | 60.519s | yes |
| IPv6 bug, repeat 2 | 39.369s | **32.235s** | yes |
| Cross-file feature, repeat 1 | **52.940s** | 77.172s | yes |
| Cross-file feature, repeat 2 | **38.383s** | 46.680s | yes |
| **Total** | **158.698s** | **216.606s** | **4/4 each** |

| Aggregate | Kroma | Pi |
|---|---:|---:|
| Completed coding-model requests | **24** | 30 |
| Tool calls | **28** | 45 |
| Uncached input tokens | **24,240** | 28,724 |
| Cached input tokens | **83,200** | 152,960 |

Compared with Pi, post-P4 Kroma took 26.7% less time, made 20% fewer
completed model requests, and made 37.8% fewer tool calls *in this
specific four-task run*. Both completed all external acceptance and upstream
tests. There were no Kroma model timeout retries or APEC failures in the
four post-P4 trials. Raw results:
`/tmp/kroma-p4-pi-compare/results.json`.

## What is NOT proved

- This does **not** overturn the historical long-running P3 Rust comparison,
  where Pi was faster and Kroma suffered a real APEC failure. That workload
  has not yet been rerun with the P4 source. Do not splice across runs to
  claim a universal speedup.
- Tool output JSON byte counters might matter to external consumers; the
  production change affects only the **model-visible tool reply**, not the
  full captured output. Monitor compatibility with future extensions.
- Pi/Kroma do not have identical system prompts and tool interfaces, so the
  end-to-end competition cannot isolate which one contributed how much.
- More task diversity and repetitions are needed before concluding that
  this implementation consistently outperforms Pi.

## Pending follow-up (not secretly declared complete)

Retest long multi-stage Rust development and session interruption/recovery
with the P4 Release build before calling the overall product faster.
Further network diagnostics should separate connect, first response, idle
stream and full request latencies before changing transport timeout policy.
